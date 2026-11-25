//! 单连接处理：keep-alive 循环。
//!
//! 一个连接上可以连续处理多个请求。循环的退出条件有五个，每个都必须显式：
//!
//! - 客户端关闭 / 读超时（空闲连接不能空占 worker）
//! - 请求要求 `Connection: close`
//! - 解析失败（**必须断开**：歧义之后这条连接上的字节归属不可信）
//! - body 超限（同理，残留字节会被当成下一个请求）
//! - 达到 `max_requests`（防止一个连接长期占用 worker）
//! - 服务端要关闭（`shutdown` 置位）
//!
//! 最后一条很关键：只停 accept 而不通知现有连接，优雅关闭会被空闲的
//! keep-alive 连接拖到 `io_timeout` 才结束——用户点了"停止"却要等几十秒。
//!
//! ## trace id
//!
//! 客户端给了 `X-Request-Id` 就用它的（便于跨服务串联），没给就生成一个。
//! 无论哪种，**都回写进响应头**——出问题时用户能把 id 直接给到运维。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// 读 socket 的轮询间隔。设成 250ms 是为了让 shutdown 能及时打断空闲连接，
/// 代价是每个空闲连接每 250ms 醒一次——这个开销远小于让关闭等 5 秒。
const POLL_INTERVAL: Duration = Duration::from_millis(250);

use crate::runtime::context::Context;
use crate::server::parse::{self, BodyLen, ParseError, MAX_HEADER_BYTES, MAX_REQUEST_LINE};
use crate::server::write;
use crate::web::error::ApiError;
use crate::web::http::{Method, Status};
use crate::web::request::Request;
use crate::web::route::Router;

/// 连接级限制。放在一个结构里传，避免 serve_conn 长出七个参数。
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_body: usize,
    /// 读写超时。空闲连接超过它就断开。
    ///
    /// 5 秒而不是 30 秒：keep-alive 的价值在于一个页面连发十几个请求，
    /// 那种场景是毫秒级的。留 30 秒只会让关闭变得更慢，并让大量僵尸连接
    /// 占着 worker。客户端真要复用长连接，5 秒足够发下一批。
    pub io_timeout: Duration,
    /// 一条 keep-alive 连接最多处理多少个请求。
    pub max_requests: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_body: 1024 * 1024,
            io_timeout: Duration::from_secs(5),
            max_requests: 100,
        }
    }
}

#[derive(Debug)]
enum HeadError {
    Eof,
    Timeout,
    TooLarge,
    Malformed(&'static str),
}

/// 处理一条连接，直到它结束。返回 `Ok` 不代表"成功"，只代表"正常收尾"。
pub fn serve_conn(
    stream: TcpStream,
    router: &Router,
    ctx: &Context,
    limits: &Limits,
    shutdown: &AtomicBool,
) -> std::io::Result<()> {
    // 读超时 = 轮询间隔；真正的空闲上限在 read_head 里按 elapsed 判定
    let _ = stream.set_read_timeout(Some(POLL_INTERVAL));
    let _ = stream.set_write_timeout(Some(limits.io_timeout));
    let _ = stream.set_nodelay(true);

    let peer = stream.peer_addr().ok().map(|a| a.to_string());
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    for _ in 0..limits.max_requests {
        // 每个请求开头看一眼：正在关闭就不接新请求了（已处理完的那条不受影响）
        if shutdown.load(Ordering::Relaxed) {
            return Ok(());
        }
        let head = match read_head(&mut reader, shutdown, limits.io_timeout) {
            Ok(h) => h,
            Err(HeadError::Eof) | Err(HeadError::Timeout) => return Ok(()),
            Err(HeadError::TooLarge) => {
                let _ = write_error(&mut writer, ApiError::new(Status(431), "请求头过大"), true);
                return Ok(());
            }
            Err(HeadError::Malformed(m)) => {
                let _ = write_error(&mut writer, ApiError::new(Status::BAD_REQUEST, m), true);
                return Ok(());
            }
        };

        let parsed = match parse::parse_head(&head, limits.max_body) {
            Ok(p) => p,
            Err(ParseError { status, message }) => {
                // 解析层拒绝一律断开：边界已不可信
                let _ = write_error(&mut writer, ApiError::new(status, message), true);
                return Ok(());
            }
        };

        let body = match parsed.body_len {
            BodyLen::None => Vec::new(),
            BodyLen::Length(n) => {
                let mut buf = vec![0u8; n];
                if reader.read_exact(&mut buf).is_err() {
                    // body 没读全：断开，绝不把半截数据交给 handler
                    let _ = write_error(
                        &mut writer,
                        ApiError::new(Status::BAD_REQUEST, "请求体不完整"),
                        true,
                    );
                    return Ok(());
                }
                buf
            }
        };

        let mut req = Request::new(parsed.method, parsed.target.path.clone())
            .with_query(parsed.target.query.clone())
            .with_body(body);
        req.headers = parsed.headers;
        req.peer = peer.clone();

        // trace id：优先客户端给的
        let trace = req
            .request_id()
            .filter(|s| !s.is_empty() && s.len() <= 64)
            .map(|s| s.to_string())
            .unwrap_or_else(next_trace_id);
        req.headers.insert("x-request-id", &trace);

        let mut resp = router.handle(&mut req, ctx);
        resp.headers.insert("x-request-id", &trace);

        let head_only = matches!(parsed.method, Method::Head);
        write::write_response(&mut writer, &resp, parsed.close, head_only)?;
        if parsed.close {
            return Ok(());
        }
    }
    Ok(())
}

fn write_error<W: Write>(out: &mut W, err: ApiError, close: bool) -> std::io::Result<()> {
    write::write_response(out, &err.to_response(), close, false)
}

/// 读"请求行 + 头 + 空行"。
///
/// 逐行读而不是"读到 `\r\n\r\n`"：逐行能在超限时立刻停手，
/// 不会为了找结束标记而把一个 8 KiB 上限的读取变成无界读取。
fn read_head<R: BufRead>(
    reader: &mut R,
    shutdown: &AtomicBool,
    idle: Duration,
) -> Result<String, HeadError> {
    let mut head = String::new();
    let mut line = Vec::new();
    let budget = MAX_REQUEST_LINE + MAX_HEADER_BYTES;
    // 从"开始等这个请求"算起。慢速发送超过 idle 就当超时（slowloris 防御）：
    // 一个字节省着发的客户端不该占住 worker。
    let started = Instant::now();

    loop {
        if shutdown.load(Ordering::Relaxed) {
            return Err(HeadError::Eof);
        }
        line.clear();
        let n = match reader.read_until(b'\n', &mut line) {
            Ok(n) => n,
            Err(e) => match e.kind() {
                // socket 设的读超时是轮询间隔（250ms），不是真正的超时；
                // 真正超时由 elapsed 判定，这样才能在等待期间响应 shutdown。
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                    if started.elapsed() >= idle {
                        return Err(HeadError::Timeout);
                    }
                    continue;
                }
                // 连接被重置也当作正常结束
                _ => return Err(HeadError::Eof),
            },
        };
        if n == 0 {
            return Err(HeadError::Eof);
        }

        if !line.ends_with(b"\r\n") {
            return Err(HeadError::Malformed(
                "行必须以 CRLF 结尾（不接受裸 LF，否则会出现解析器差异）",
            ));
        }
        let content = &line[..line.len() - 2];
        if content.len() > MAX_REQUEST_LINE {
            return Err(HeadError::TooLarge);
        }
        let s = std::str::from_utf8(content)
            .map_err(|_| HeadError::Malformed("请求行/头部必须是 ASCII/UTF-8"))?;

        head.push_str(s);
        head.push_str("\r\n");
        if head.len() > budget {
            return Err(HeadError::TooLarge);
        }
        if s.is_empty() {
            return Ok(head);
        }
    }
}

fn next_trace_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    static EPOCH: OnceLock<u64> = OnceLock::new();
    let epoch = *EPOCH.get_or_init(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    });
    format!("{epoch:x}-{:x}", SEQ.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_head_from(bytes: &[u8]) -> Result<String, HeadError> {
        let mut r = BufReader::new(std::io::Cursor::new(bytes.to_vec()));
        let stop = AtomicBool::new(false);
        read_head(&mut r, &stop, Duration::from_secs(5))
    }

    #[test]
    fn reads_until_blank_line() {
        let head = read_head_from(b"GET / HTTP/1.1\r\nHost: a\r\n\r\nBODY").unwrap();
        assert_eq!(head, "GET / HTTP/1.1\r\nHost: a\r\n\r\n");
    }

    #[test]
    fn eof_before_blank_line_is_eof() {
        assert!(matches!(
            read_head_from(b"GET / HTTP/1.1\r\nHost: a\r\n"),
            Err(HeadError::Eof)
        ));
    }

    #[test]
    fn bare_lf_is_malformed() {
        assert!(matches!(
            read_head_from(b"GET / HTTP/1.1\nHost: a\n\n"),
            Err(HeadError::Malformed(_))
        ));
    }

    #[test]
    fn oversized_line_is_too_large() {
        let mut buf = b"GET /".to_vec();
        buf.extend(std::iter::repeat(b'a').take(MAX_REQUEST_LINE + 10));
        buf.extend_from_slice(b" HTTP/1.1\r\n");
        assert!(matches!(read_head_from(&buf), Err(HeadError::TooLarge)));
    }

    #[test]
    fn trace_ids_are_unique() {
        let a = next_trace_id();
        let b = next_trace_id();
        assert_ne!(a, b);
    }
}
