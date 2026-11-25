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
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// 对端关了
    Eof,
    /// 服务端自己要关（`should_stop` 为真）
    ///
    /// 必须与 `Eof` 分开：同样都是"读不到请求了"，但一个该静默退出，
    /// 另一个该回 503——否则关闭期客户端只看到"连接被断"，不知道能重试。
    Stopped,
    Timeout,
    TooLarge,
    Malformed(&'static str),
}

/// 关闭期给老连接的答复：503 + `retry-after` + 关连接。
///
/// 客户端拿到这个才知道"换实例重试"，而不是把空连接当成故障。
fn write_closing<W: Write>(out: &mut W) -> std::io::Result<()> {
    let mut resp = ApiError::new(
        Status::SERVICE_UNAVAILABLE,
        "服务正在重启，请稍后重试或换一个实例",
    )
    .to_response();
    resp.headers.insert("retry-after", "1");
    write::write_response(out, &resp, true, false)
}

/// 一条连接的全部状态：socket、读缓冲、已处理请求数。
///
/// 为什么把 `BufReader` 一起放进来：无缓冲的话每次处理都要重新
/// `try_clone`（一次 `dup(2)`）。把连接做成"可以被搬来搬去的对象"后，
/// worker 能在多条连接之间轮转而不用每次付这个成本。
pub struct Conn {
    stream: TcpStream,
    reader: BufReader<TcpStream>,
    served: usize,
}

/// 处理完一个请求之后的去向。
#[derive(Debug, PartialEq, Eq)]
pub enum ConnOutcome {
    /// 还能继续接请求，把它放回队列让别人也有机会
    KeepAlive,
    /// 该关了（对端要求 close、出错、或达到 `max_requests`）
    Closed,
}

const CLOSED: std::result::Result<ConnOutcome, std::io::Error> = Ok(ConnOutcome::Closed);
const KEEP: std::result::Result<ConnOutcome, std::io::Error> = Ok(ConnOutcome::KeepAlive);

impl Conn {
    pub fn new(stream: TcpStream, limits: &Limits) -> std::io::Result<Self> {
        // 读超时 = 轮询间隔；真正的空闲上限在 read_head 里按 elapsed 判定
        let _ = stream.set_read_timeout(Some(POLL_INTERVAL));
        let _ = stream.set_write_timeout(Some(limits.io_timeout));
        let _ = stream.set_nodelay(true);
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Self {
            stream,
            reader,
            served: 0,
        })
    }

    pub fn peer(&self) -> Option<String> {
        self.stream.peer_addr().ok().map(|a| a.to_string())
    }

    /// 原始 fd，给 worker 的 `poll` 用。
    pub fn raw_fd(&self) -> Option<std::os::raw::c_int> {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            Some(self.stream.as_raw_fd())
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    /// 这条连接此刻有没有待处理输入（0ms `poll`）。
    ///
    /// worker 用它来决定"该不该现在服务这条连接"：不可读的连接直接放回队列，
    /// 否则 worker 会卡在 read 上等它的下一个请求，把后面排队的连接全饿住。
    #[cfg(unix)]
    pub fn has_pending_input(&self) -> std::io::Result<bool> {
        use std::os::unix::io::AsRawFd;
        crate::server::poll::wait_readable(self.stream.as_raw_fd(), 0)
    }

    #[cfg(not(unix))]
    pub fn has_pending_input(&self) -> std::io::Result<bool> {
        Ok(true)
    }

    pub fn into_stream(self) -> TcpStream {
        self.stream
    }

    /// 处理**一个**请求就返回。
    ///
    /// ## 为什么不是"一条连接循环到底"
    ///
    /// 早先版本 worker 拿到一条连接就 `for 0..max_requests` 一直读。看着省事，
    /// 但 keep-alive 客户端的连接会**长期占住 worker**：2 个 worker 遇上 20 条
    /// 长连接，18 条在队列里干等到前面的空闲超时（5s）。基准里表现为
    /// "P99 = 47µs 但 max = 5004ms"——平均值很好看，尾部全是饥饿。
    ///
    /// 现在每处理完一个请求就交回队列，由 worker 取下一条连接。这等于在
    /// 线程模型里做了协作式轮转：代价是一次 `mpsc` 传递，换来连接公平。
    pub fn serve_once<F: Fn() -> bool>(
        &mut self,
        router: &Router,
        ctx: &Context,
        limits: &Limits,
        should_stop: &F,
    ) -> std::io::Result<ConnOutcome> {
        // 单连接处理上限：防止一个连接永久占着一个 worker 的队列位
        if self.served >= limits.max_requests {
            return CLOSED;
        }

        // 每个请求开头看一眼：正在关闭就不再接新请求，但**要给个明确答复**。
        // 直接断连时客户端只看到 "empty reply"，重试逻辑差一点的会当故障；
        // 回 503 + `retry-after` + `connection: close` 它才知道该换实例重试。
        if should_stop() {
            let _ = write_closing(&mut self.stream);
            return CLOSED;
        }

        let peer = self.peer();
        let head = match read_head(&mut self.reader, should_stop, limits.io_timeout) {
            Ok(h) => h,
            Err(HeadError::Eof) | Err(HeadError::Timeout) => return CLOSED,
            Err(HeadError::Stopped) => {
                // 正在关闭：给明确答复再走
                let _ = write_closing(&mut self.stream);
                return CLOSED;
            }
            Err(HeadError::TooLarge) => {
                let _ = write_error(
                    &mut self.stream,
                    ApiError::new(Status(431), "请求头过大"),
                    true,
                );
                return CLOSED;
            }
            Err(HeadError::Malformed(m)) => {
                let _ = write_error(&mut self.stream, ApiError::new(Status::BAD_REQUEST, m), true);
                return CLOSED;
            }
        };

        let parsed = match parse::parse_head(&head, limits.max_body) {
            Ok(p) => p,
            Err(ParseError { status, message }) => {
                // 解析层拒绝一律断开：边界已不可信
                let _ = write_error(&mut self.stream, ApiError::new(status, message), true);
                return CLOSED;
            }
        };

        let body = match parsed.body_len {
            BodyLen::None => Vec::new(),
            BodyLen::Length(n) => {
                let mut buf = vec![0u8; n];
                if self.reader.read_exact(&mut buf).is_err() {
                    // body 没读全：断开，绝不把半截数据交给 handler
                    let _ = write_error(
                        &mut self.stream,
                        ApiError::new(Status::BAD_REQUEST, "请求体不完整"),
                        true,
                    );
                    return CLOSED;
                }
                buf
            }
        };

        let mut req = Request::new(parsed.method, parsed.target.path.clone())
            .with_query(parsed.target.query.clone())
            .with_body(body);
        req.headers = parsed.headers;
        req.peer = peer;

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
        write::write_response(&mut self.stream, &resp, parsed.close, head_only)?;
        self.served += 1;
        if parsed.close {
            return CLOSED;
        }
        KEEP
    }
}

fn write_error<W: Write>(out: &mut W, err: ApiError, close: bool) -> std::io::Result<()> {
    write::write_response(out, &err.to_response(), close, false)
}

/// 读"请求行 + 头 + 空行"。
///
/// 逐行读而不是"读到 `\r\n\r\n`"：逐行能在超限时立刻停手，
/// 不会为了找结束标记而把一个 8 KiB 上限的读取变成无界读取。
fn read_head<R: BufRead, F: Fn() -> bool>(
    reader: &mut R,
    should_stop: &F,
    idle: Duration,
) -> Result<String, HeadError> {
    let mut head = String::new();
    let mut line = Vec::new();
    let budget = MAX_REQUEST_LINE + MAX_HEADER_BYTES;
    // 从"开始等这个请求"算起。慢速发送超过 idle 就当超时（slowloris 防御）：
    // 一个字节省着发的客户端不该占住 worker。
    let started = Instant::now();

    loop {
        if should_stop() {
            return Err(HeadError::Stopped);
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
        read_head(&mut r, &|| false, Duration::from_secs(5))
    }

    #[test]
    fn stop_during_read_is_distinguishable_from_eof() {
        // 同样是"读不到请求"，服务端主动关必须能被上层识别出来，
        // 否则关闭期只能静默断连、给不出 503
        let mut r = BufReader::new(std::io::Cursor::new(b"GET / HTT".to_vec()));
        let stop = || true;
        assert!(matches!(
            read_head(&mut r, &stop, Duration::from_secs(5)),
            Err(HeadError::Stopped)
        ));
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
