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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::runtime::context::Context;
use crate::server::parse::ParsedRequest;
use crate::server::parse::{self, BodyLen, ParseError, MAX_HEADER_BYTES, MAX_REQUEST_LINE};
use crate::server::write;
use super::{coio, profile};
use crate::web::error::ApiError;
use crate::web::http::headers::HeaderSpare;
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
    write::write_response(out, &resp, true, false, None)
}

/// 一条连接的全部状态：socket、读缓冲、已处理请求数。
///
/// 为什么把 `BufReader` 一起放进来：无缓冲的话每次处理都要重新
/// `try_clone`（一次 `dup(2)`）。把连接做成"可以被搬来搬去的对象"后，
/// worker 能在多条连接之间轮转而不用每次付这个成本。
/// 读缓冲起始容量。
///
/// `BufReader::new` 默认要 8 KiB——它不是"按需"的，一建连接就占着。
/// keep-alive 连接空闲时什么都不读，这 8 KiB 仍然是实打实的 RSS：
/// 五千条挂在 `poll` 上的空闲连接就是 40 MB，纯粹为"万一头很大"付的。
/// 请求头 99 % 在 1 KiB 内；更大的头只是多几次 `read`（每次 `poll` 已
/// 返回可读，成本是 syscall 本身），比"每条空闲连接白占 7 KiB"划算。
/// 请求体的读取不吃这个容量：`read_exact` 走的是 `BufReader::read` 的
/// bypass 分支（剩余长度 ≥ 容量时直接读 socket），块大小由内核决定。
/// 读缓冲的默认大小。
///
/// 1 KiB 是"省内存"的选择：1 000 条空闲连接实测 6.7 MB RSS。但它也是
/// pipelining 的上限——一个请求头约 80 字节，1 KiB 只装得下 12 个，
/// 连发 16 个就要读两次。`RSBOOT_READ_BUF` 可以把它调大做 A/B。
pub const READ_BUF_BYTES: usize = 1024;

fn read_buf_bytes() -> usize {
    use std::sync::OnceLock;
    static F: OnceLock<usize> = OnceLock::new();
    *F.get_or_init(|| {
        std::env::var("RSBOOT_READ_BUF")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|v| *v >= 256 && *v <= 64 * 1024)
            .unwrap_or(READ_BUF_BYTES)
    })
}
/// 头原文缓冲的起点（一次请求头约 200–600 字节）。
const HEAD_BUF_BYTES: usize = 512;
/// `read_until` 的行缓冲起点。
const LINE_BUF_BYTES: usize = 256;
/// 响应头组装缓冲起点。
const OUT_BUF_BYTES: usize = 256;

/// 攒到这么多字节就先写一次。太小会退化成"每个响应一次 write"，
/// 太大则单连接占住内存、且大响应反而占不到合并的好处。
const FLUSH_BYTES: usize = 16 * 1024;

/// 超过这个大小的响应不参与合并写，直接 `writev`（见 `serve_one` 里的注释）。
const BIG_BODY_BYTES: usize = 8 * 1024;

pub struct Conn {
    /// 让出式读缓冲：`read_until` / `read_exact` 内部就会挂起等 `POLLIN`。
    ///
    /// 连接的所有权只在这里一份。早先 `Conn` 另存一个 `TcpStream`（从
    /// `try_clone` dup 出来的），读写各握一个 fd：dup 是每连接一次系统调用，
    /// 还多占一个 fd 配额。短连接压测里 accept 路径 5.7 µs/连接，这一项
    /// 加上 `set_nodelay` 的 setsockopt 就是其中两刀。现在写响应直接取
    /// 这里面的 fd。
    reader: BufReader<coio::CoIo>,
    /// 对端地址，建连时算一次（`getpeername` 不该每请求一次）。
    peer_addr: Option<String>,
    /// 当前请求的头原文。按连接复用：keep-alive 下每条连接处理成千上万个请求，
    /// 每请求新建一个 `String` 意味着一次 malloc + 数次 realloc（头部有 8 行时
    /// 容量要长两三轮）。空闲时由 `trim_idle` 把被撑大的容量还回去。
    head_buf: String,
    /// `read_until` 的行缓冲，同样按连接复用。
    line_buf: Vec<u8>,
    /// 响应头的组装缓冲。`write_response_fd` 每次新建一个 `Vec` 来拼状态行与头，
    /// 同样属于每请求一次的固定税。
    out_buf: Vec<u8>,
    served: usize,
    /// 连接自增 id。生成的 trace id 用它做第二段，于是不必再每请求抢
    /// 一个全局计数器（3 个 worker 抢同一条 cache line，那是跨核流量）。
    id: u64,
    /// 请求头槽位的复用池。每请求一次 `Vec::with_capacity(13)` 是固定税，
    /// 而相邻请求的头部形状几乎一样，还回来就能省掉这次 malloc。
    hdr_spare: HeaderSpare,
}

/// 处理完一个请求之后的去向。
#[derive(Debug, PartialEq, Eq)]
pub enum ConnOutcome {
    /// 还能继续接请求，把它放回队列让别人也有机会
    KeepAlive,
    /// 这条连接暂时没数据（空闲），协程可以退出，连接交回队列
    ///
    /// 与 `Closed` 的区别：连接还活着，`poll` 一有事件就该重新服务它。
    Idle,
    /// 该关了（对端要求 close、出错、或达到 `max_requests`）
    Closed,
}

/// 调小收发缓冲（Linux 专用，opt-in）。
///
/// 默认 `SO_SNDBUF` 走内核自动调优，能涨到几 MB。缓冲越大，单次 `write` 要维护
/// 的 `sk_buff` 队列越长；我们一个响应只有 260 B，用不上。这是 A/B 用的开关，
/// 不设 env 就不动任何东西，行为与之前完全一致。
#[cfg(target_os = "linux")]
fn tune_bufs(stream: &std::net::TcpStream) {
    use std::os::raw::{c_int, c_uint, c_void};
    unsafe extern "C" {
        fn setsockopt(fd: c_int, level: c_int, name: c_int, val: *const c_void, len: c_uint) -> c_int;
    }
    use std::sync::OnceLock;
    static VALS: OnceLock<(c_int, c_int)> = OnceLock::new();
    let (snd, rcv) = *VALS.get_or_init(|| {
        let rd = |k: &str| -> c_int {
            std::env::var(k)
                .ok()
                .and_then(|v| v.trim().parse::<c_int>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(0)
        };
        (rd("RSBOOT_SNDBUF"), rd("RSBOOT_RCVBUF"))
    });
    if snd == 0 && rcv == 0 {
        return;
    }
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    const SOL_SOCKET: c_int = 1;
    for (name, val) in [(7, snd), (8, rcv)] {
        if val > 0 {
            let p = &val as *const c_int as *const c_void;
            let _ = unsafe { setsockopt(fd, SOL_SOCKET, name, p, 4) };
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn tune_bufs(_stream: &std::net::TcpStream) {}

const CLOSED: std::result::Result<ConnOutcome, std::io::Error> = Ok(ConnOutcome::Closed);
const KEEP: std::result::Result<ConnOutcome, std::io::Error> = Ok(ConnOutcome::KeepAlive);

impl Conn {
    pub fn new(stream: TcpStream, _limits: &Limits) -> std::io::Result<Self> {
        // 连接改成非阻塞：会阻塞的点统统交给调度器，worker 不再被一条慢连接按住。
        // 读超时不再依赖 SO_RCVTIMEO——`CoIo` 挂起时自带 250ms 心跳，
        // 真正的空闲上限仍由 `read_head` 里的 elapsed 判定。
        let _ = stream.set_nodelay(nodelay_on());
        tune_bufs(&stream);
        // 对端地址是**连接级**信息，建连时查一次就够。原来每个请求都
        // `peer_addr()` + `to_string()`：一次 `getpeername` 系统调用加一次
        // 分配/格式化，而它发生在 READ 段之前，剖面里完全看不见。
        let peer_addr = stream.peer_addr().ok().map(|a| a.to_string());
        let reader = BufReader::with_capacity(read_buf_bytes(), coio::CoIo::new(stream)?);
        Ok(Self {
            reader,
            peer_addr,
            head_buf: String::with_capacity(HEAD_BUF_BYTES),
            line_buf: Vec::with_capacity(LINE_BUF_BYTES),
            out_buf: Vec::with_capacity(OUT_BUF_BYTES),
            served: 0,
            id: NEXT_CONN.fetch_add(1, Ordering::Relaxed),
            hdr_spare: HeaderSpare::new(),
        })
    }

    /// 连接进入空闲时把复用缓冲归还。
    ///
    /// 触发点是"已经空闲满 `io_timeout`"（`read_head` 返回 `Timeout`），
    /// 所以不会打断一个正在发请求的连接，也不是每个请求都做。正常请求的
    /// 容量本来就在起点附近，这里等价于三次整数比较；只有被大 Cookie /
    /// 长 URL 撑到几 KiB 的连接才真的 realloc 一次。
    ///
    /// 为什么不在请求之间无条件收缩：每次处理完一个请求都可能短暂回到队列，
    /// 那时收缩会让下个请求再长一次——拿吞吐换内存，不划算。空闲 5 秒才是
    /// "这容量确实用不上了"的证据。
    fn trim_idle(&mut self) {
        self.head_buf.shrink_to(HEAD_BUF_BYTES);
        self.line_buf.shrink_to(LINE_BUF_BYTES);
        self.out_buf.shrink_to(OUT_BUF_BYTES);
    }

    /// 对端地址，建连时算好、此后只做一次 `String` clone。
    pub fn peer(&self) -> Option<String> {
        self.peer_addr.clone()
    }

    /// 原始 fd，给 worker 的 `poll` 用。
    pub fn raw_fd(&self) -> Option<std::os::raw::c_int> {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            Some(self.reader.get_ref().get_ref().as_raw_fd())
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
    /// 缓冲里是否还有没处理完的字节（pipelining 的后续请求就在这儿）。
    ///
    /// 注意不能用 `poll(fd, 0)` 去查：那些字节已经在用户态缓冲里，
    /// socket 本身是空的，poll 会说"没数据"——旧实现就是这么错的。
    #[inline]
    pub fn has_buffered_input(&self) -> bool {
        !self.reader.buffer().is_empty()
    }

    pub fn into_stream(self) -> TcpStream {
        // `Conn` 现在有 `Drop`（负责从 epoll 集合里摘 fd），所以不能直接移动字段。
        // 这里把 `reader` 读出来，再 `forget` 掉 `self`：fd 的所有权已经转给返回的
        // `TcpStream`，不能再走一次关闭/注销。
        let reader = unsafe { std::ptr::read(&self.reader) };
        std::mem::forget(self);
        reader.into_inner().into_inner()
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
    fn serve_one<F: Fn() -> bool>(
        &mut self,
        router: &Router,
        ctx: &Context,
        limits: &Limits,
        should_stop: &F,
    ) -> std::io::Result<ConnOutcome> {
        profile::pick();
        // 单连接处理上限：防止一个连接永久占着一个 worker 的队列位
        if self.served >= limits.max_requests {
            return CLOSED;
        }

        // 每个请求开头看一眼：正在关闭就不再接新请求，但**要给个明确答复**。
        // 直接断连时客户端只看到 "empty reply"，重试逻辑差一点的会当故障；
        // 回 503 + `retry-after` + `connection: close` 它才知道该换实例重试。
        if should_stop() {
            let _ = write_closing(&mut self.out_buf);
            return CLOSED;
        }

        let peer = self.peer();
        let _t = profile::start(profile::READ);
        match read_head(
            &mut self.reader,
            &mut self.head_buf,
            &mut self.line_buf,
            should_stop,
            limits.io_timeout,
        ) {
            Ok(()) => (),
            // 空闲连接不是坏连接：交回队列，让 worker 去服务别人。
            // 顺手把被撑大的缓冲还掉——此刻它已经闲满 `io_timeout`，
            // 那些容量短期内不会被用到。
            Err(HeadError::Timeout) => {
                self.trim_idle();
                return Ok(ConnOutcome::Idle);
            }
            Err(HeadError::Eof) => return CLOSED,
            Err(HeadError::Stopped) => {
                // 正在关闭：给明确答复再走
                let _ = write_closing(self.reader.get_mut());
                return CLOSED;
            }
            Err(HeadError::TooLarge) => {
                let _ = write_error(
                    &mut self.out_buf,
                    ApiError::new(Status(431), "请求头过大"),
                    true,
                );
                return CLOSED;
            }
            Err(HeadError::Malformed(m)) => {
                let _ = write_error(&mut self.out_buf, ApiError::new(Status::BAD_REQUEST, m), true);
                return CLOSED;
            }
        };

        drop(_t);
        let _t = profile::start(profile::PARSE);
        let spare = std::mem::take(&mut self.hdr_spare);
        let parsed = match parse::parse_head_with(&self.head_buf, limits.max_body, spare) {
            Ok(p) => p,
            Err(ParseError { status, message }) => {
                // 解析层拒绝一律断开：边界已不可信
                let _ = write_error(&mut self.out_buf, ApiError::new(status, message), true);
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
                        &mut self.out_buf,
                        ApiError::new(Status::BAD_REQUEST, "请求体不完整"),
                        true,
                    );
                    return CLOSED;
                }
                buf
            }
        };

        drop(_t);
        let _t = profile::start(profile::BUILD);
        // 路径与查询串直接搬进去，不再 `clone`：
        // `parsed` 之后只剩 `method`/`close` 要用，没有第二个人需要这两个 String
        let ParsedRequest {
            method,
            target,
            headers,
            close: keep_close,
            ..
        } = parsed;
        let mut req = Request::new(method, target.path)
            .with_query(target.query)
            .with_body(body);
        req.headers = headers;
        req.peer = peer;

        // trace id：优先客户端给的，否则自己生成。
        //
        // 全部落在栈缓冲里（`TRACE_MAX` 字节），**零分配**。原来是
        // `String` + `Arc<str>` 两次 malloc，再加一次 `Headers` 插入——profile
        // 里光这一段就是 198 ns/请求，流水线下占总成本的 8%。
        //
        // 代价：服务端生成的 id 不再出现在**请求头**里（`req.request_id()`
        // 只在客户端给过时才有值），它只出现在响应头。客户端给的 id 仍然原样
        // 回显在响应头里。
        // `RSBOOT_NO_TRACE`：1 = 固定 id，2 = 完全不发；只为拆分测量，生产别设。
        let mut trace_buf = [0u8; TRACE_MAX];
        let trace_len = match trace_mode() {
            2 => 0,
            1 => {
                trace_buf[..STATIC_TRACE.len()].copy_from_slice(STATIC_TRACE.as_bytes());
                STATIC_TRACE.len()
            }
            _ => match req.request_id().filter(|s| !s.is_empty() && s.len() <= 64) {
                Some(s) => {
                    trace_buf[..s.len()].copy_from_slice(s.as_bytes());
                    s.len()
                }
                None => write_trace_id(&mut trace_buf, self.id, self.served),
            },
        };
        let trace: Option<&[u8]> = if trace_len == 0 {
            None
        } else {
            Some(&trace_buf[..trace_len])
        };

        drop(_t);
        let _t = profile::start(profile::HANDLE);
        let mut resp = router.handle(&mut req, ctx);
        // handler 已经返回，请求头不再有人读：把这批槽位还回连接。
        self.hdr_spare = req.headers.take_spare();
        // 响应头不插了：`fill_head` 直接把它拼到 head 末尾（见 `write.rs`）。
        // 请求头那一份仍然插着，因为 handler 可能用 `req.request_id()` 记日志。

        drop(_t);
        let _t = profile::start(profile::WRITE);
        let head_only = matches!(method, Method::Head);
        // 热路径走 fd 直写：大 body 用 `writev` 省一次 memcpy 与大块分配。
        // 泛型版留给测试（它要能用 `Vec<u8>` 当输出）。
        // 先攒进 `out_buf`，由 `serve_once` 统一 flush：一条连接上排队的 N 个请求
        // 可以合并成**一次** `write`。之前是每个响应一次 `write`，16 个请求就是
        // 16 次系统调用；pipelining 下这是实打实的差距。
        //
        // 大 body 例外：`write_response_fd` 走 `writev`，body 不复制。若把它先搬进
        // `out_buf`，一个 1 MB 的下载就要多一次 1 MB memcpy。所以大响应用先把攒下的
        // 部分写出去，再直接写 fd，保持零拷贝。
        // 缓冲里没有下一个请求（绝大多数长连接只发一个就等）→ 沿用原来的
        // `writev` 直写，一次系统调用、body 不复制，与引入合并写之前完全一致。
        // 只有确实排着队的连接才值得攒起来合并。
        if resp.body.len() > BIG_BODY_BYTES
            || (self.reader.buffer().is_empty() && self.out_buf.is_empty())
        {
            self.flush_out()?;
            #[cfg(unix)]
            {
                use std::os::unix::io::AsRawFd;
                write::write_response_fd(
                    self.reader.get_ref().get_ref().as_raw_fd(),
                    &resp,
                    keep_close,
                    head_only,
                    &mut self.out_buf,
                    trace,
                )?;
            }
            // `write_response_fd` 会把 head 留在 `out_buf` 里（它复用这块缓冲拼 head），
            // 写完之后必须清掉：否则下一次 flush 会把上一个响应**再写一遍**，
            // 客户端按 content-length 继续往下读，整条连接的数据就错位了。
            self.out_buf.clear();
            #[cfg(not(unix))]
            write::write_response(self.reader.get_mut(), &resp, keep_close, head_only, trace)?;
        } else {
            write::write_response(&mut self.out_buf, &resp, keep_close, head_only, trace)?;
        }
        drop(_t);
        profile::finish_request();
        self.served += 1;
        if keep_close {
            return CLOSED;
        }
        KEEP
    }

    /// 处理请求，直到"缓冲里没有排队的数据"或连接结束。
    ///
    /// 为什么需要这层循环：`BufReader` 读到第一个请求头时，往往已经把后面的
    /// 请求一起读进了缓冲区。那一刻 socket 上没有新数据，`poll` 不会报事件，
    /// 连接就挂在那儿直到超时——HTTP/1.1 pipelining 的请求正是这样被静默吞掉的
    /// （实测一条连接连发 4 个请求，只回 1 个）。所以只要缓冲区里还有字节，
    /// 就在同一个协程里接着处理，不交回队列。
    ///
    /// 半截请求也走这条路径：`read_head` 在 `WouldBlock` 时会挂起等数据，与
    /// 普通等待没有区别，只是省掉一次 `mpsc` 往返。
    pub fn serve_once<F: Fn() -> bool>(
        &mut self,
        router: &Router,
        ctx: &Context,
        limits: &Limits,
        should_stop: &F,
    ) -> std::io::Result<ConnOutcome> {
        let out = loop {
            let outcome = self.serve_one(router, ctx, limits, should_stop)?;
            let more = outcome == ConnOutcome::KeepAlive
                && !self.reader.buffer().is_empty()
                && !should_stop();
            if !more || self.out_buf.len() >= FLUSH_BYTES {
                self.flush_out()?;
                if !more {
                    break outcome;
                }
            }
        };
        Ok(out)
    }

    /// 把攒下的响应一次写出去。
    fn flush_out(&mut self) -> std::io::Result<()> {
        if self.out_buf.is_empty() {
            return Ok(());
        }
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            super::coio::write_fd_all(self.reader.get_ref().get_ref().as_raw_fd(), &self.out_buf)?;
        }
        #[cfg(not(unix))]
        self.reader.get_mut().write_all(&self.out_buf)?;
        self.out_buf.clear();
        Ok(())
    }
}

fn write_error<W: Write>(out: &mut W, err: ApiError, close: bool) -> std::io::Result<()> {
    write::write_response(out, &err.to_response(), close, false, None)
}

/// 读"请求行 + 头 + 空行"。
///
/// 逐行读而不是"读到 `\r\n\r\n`"：逐行能在超限时立刻停手，
/// 不会为了找结束标记而把一个 8 KiB 上限的读取变成无界读取。
fn read_head<R: BufRead, F: Fn() -> bool>(
    reader: &mut R,
    head: &mut String,
    _line: &mut Vec<u8>,
    should_stop: &F,
    idle: Duration,
) -> Result<(), HeadError> {
    head.clear();
    let budget = MAX_REQUEST_LINE + MAX_HEADER_BYTES;
    // 从"开始等这个请求"算起。慢速发送超过 idle 就当超时（slowloris 防御）：
    // 一个字节省着发的客户端不该占住 worker。
    // 用 TSC 廉价时钟：这台 VM 上一次 `Instant::now()` 要 1.87 µs，
    // 而它是**每请求**一次，等于给每个请求加了一笔固定税。
    let started_ns = super::clock::now_ns();
    let idle_ns = idle.as_nanos() as u64;

    // 一次看住一整个缓冲，而不是逐行 `read_until` + `push_str`。
    //
    // 原来那套是**同一个字节走两遍**：`read_until` 从 `BufReader` 抄进
    // `line: Vec<u8>`，再 `push_str` 抄进 `head`，中间还有每行一次
    // `clear` + `ends_with` + `from_utf8`。这里改成在缓冲上直接扫
    // `\r\n\r\n`，命中就整段 `extend`，一个字节只拷一次。
    //
    // 扫描窗口必须兼顾**跨缓冲段**的结束标记（上一段以 `\r\n\r` 收尾、
    // 本段以 `\n` 开头），所以段首先只检查 head 尾 1..3 字节与本段首
    // 1..3 字节的三种切分，再在段内扫。
    loop {
        if should_stop() {
            return Err(HeadError::Stopped);
        }
        let buf = match reader.fill_buf() {
            Ok(b) => b,
            Err(e) => match e.kind() {
                // socket 设的读超时是轮询间隔（250ms），不是真正的超时；
                // 真正超时由 elapsed 判定，这样才能在等待期间响应 shutdown。
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                    if super::clock::now_ns().saturating_sub(started_ns) >= idle_ns {
                        return Err(HeadError::Timeout);
                    }
                    continue;
                }
                // 连接被重置也当作正常结束
                _ => return Err(HeadError::Eof),
            },
        };
        if buf.is_empty() {
            return Err(HeadError::Eof);
        }

        let hlen = head.len();
        // 上一行已经积了多少字节：从 head 尾往前找最近的 CRLF。反向搜，
        // 几乎一两个字节就命中。
        let mut row = match head.rfind("\r\n") {
            Some(i) => hlen - (i + 2),
            None => hlen,
        };
        let mut cut: Option<usize> = None;

        // 1) 结束标记跨段
        if hlen > 0 {
            let hb = head.as_bytes();
            let mut k = 1usize;
            while k <= 3 && k <= hlen {
                let need = 4 - k;
                if buf.len() >= need
                    && hb[hlen - k..] == b"\r\n\r\n"[..k]
                    && buf[..need] == b"\r\n\r\n"[k..]
                {
                    cut = Some(need);
                    break;
                }
                k += 1;
            }
        }

        // 2) 段内：找结束，同时校验裸 LF 与单行长度
        if cut.is_none() {
            let mut prev_cr = hlen > 0 && head.as_bytes()[hlen - 1] == b'\r';
            let mut i = 0usize;
            while i < buf.len() {
                let c = buf[i];
                if c == b'\r' {
                    if i + 3 < buf.len()
                        && buf[i + 1] == b'\n'
                        && buf[i + 2] == b'\r'
                        && buf[i + 3] == b'\n'
                    {
                        cut = Some(i + 4);
                        break;
                    }
                    prev_cr = true;
                    row += 1;
                } else if c == b'\n' {
                    if !prev_cr {
                        return Err(HeadError::Malformed(
                            "行必须以 CRLF 结尾（不接受裸 LF，否则会出现解析器差异）",
                        ));
                    }
                    prev_cr = false;
                    row = 0;
                } else {
                    prev_cr = false;
                    row += 1;
                    if row > MAX_REQUEST_LINE {
                        return Err(HeadError::TooLarge);
                    }
                }
                i += 1;
            }
        }

        let take = cut.unwrap_or(buf.len());
        let seg = std::str::from_utf8(&buf[..take])
            .map_err(|_| HeadError::Malformed("请求行/头部必须是 ASCII/UTF-8"))?;
        if hlen + seg.len() > budget {
            return Err(HeadError::TooLarge);
        }
        head.push_str(seg);
        // `seg` 用完，buf 的借用到此结束，可以消费了。
        reader.consume(take);
        if cut.is_some() {
            return Ok(());
        }
    }
}

/// trace id 的最大长度（客户端给的 64 上限 + 自生成的 ~20）。
const TRACE_MAX: usize = 64;

const STATIC_TRACE: &str = "00000000-00000001";

/// 把十六进制写进 `out`，返回写入的字节数。
fn push_hex(out: &mut [u8], mut v: u64) -> usize {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut tmp = [0u8; 16];
    let mut i = tmp.len();
    loop {
        i -= 1;
        tmp[i] = DIGITS[(v & 0xf) as usize];
        v >>= 4;
        if v == 0 {
            break;
        }
    }
    let n = tmp.len() - i;
    out[..n].copy_from_slice(&tmp[i..]);
    n
}

/// 生成 `<epoch>-<seq>` 到 `out`，返回长度。**不分配**。
/// 全局连接序号。每**连接**取一次，摊到每请求上是零。
static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

/// 生成一个 trace id：`epoch-连接id-请求序号`。
///
/// 原来第二段来自一个全局 `fetch_add`，**每请求**一次。三个 worker 抢同一条
/// cache line 就是跨核流量，而这种 id 只需要"能区分"就够了——连接 id 建连时
/// 取一次、请求序号用 `served`（本来就在递增），于是热路径上没有任何原子操作。
fn write_trace_id(out: &mut [u8], conn: u64, seq: usize) -> usize {
    static EPOCH: OnceLock<u64> = OnceLock::new();
    let epoch = *EPOCH.get_or_init(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    });
    let mut n = push_hex(out, epoch);
    out[n] = b'-';
    n += 1;
    n += push_hex(&mut out[n..], conn);
    out[n] = b'-';
    n += 1;
    n + push_hex(&mut out[n..], seq as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一对已连上的本机 TCP socket（`Conn` 要真的 `TcpStream`，还要 `try_clone`）。
    fn tcp_pair() -> (std::net::TcpStream, std::net::TcpStream) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let c = std::net::TcpStream::connect(addr).unwrap();
        let (s, _) = l.accept().unwrap();
        (s, c)
    }

    #[test]
    fn new_conn_does_not_reserve_8k_of_read_buffer() {
        // `BufReader::new` 的默认容量是 8 KiB。空闲连接读不到东西，
        // 这 8 KiB 全是白占——这条测试锁住"起点就是 1 KiB"。
        let (a, _b) = tcp_pair();
        let conn = Conn::new(a, &Limits::default()).unwrap();
        assert_eq!(conn.reader.capacity(), READ_BUF_BYTES);
    }

    #[test]
    fn idle_trim_releases_oversized_buffers() {
        let (a, _b) = tcp_pair();
        let mut conn = Conn::new(a, &Limits::default()).unwrap();
        // 模拟被大 Cookie / 长 URL 撑到 4 KiB 的连接
        // 真实路径上 `read_head` 开头就 `head.clear()`，空闲态是空串。
        // (`shrink_to` 不会缩到比 `len` 更小，所以这里必须先清空。)
        conn.head_buf = String::with_capacity(4096);
        conn.head_buf.push_str(&"x".repeat(4096));
        conn.head_buf.clear();
        conn.line_buf = Vec::with_capacity(4096);
        conn.out_buf = Vec::with_capacity(4096);
        conn.trim_idle();
        // allocator 可能给略多于目标的容量，所以只断言上界
        assert!(conn.head_buf.capacity() <= 1024, "{}", conn.head_buf.capacity());
        assert!(conn.line_buf.capacity() <= 1024, "{}", conn.line_buf.capacity());
        assert!(conn.out_buf.capacity() <= 1024, "{}", conn.out_buf.capacity());
    }

    #[test]
    fn idle_trim_is_a_noop_when_capacity_is_already_small() {
        let (a, _b) = tcp_pair();
        let mut conn = Conn::new(a, &Limits::default()).unwrap();
        let before = conn.head_buf.capacity();
        conn.trim_idle();
        // 起点本来就 ≤ 目标：不该有 shrink（也不该有分配）
        assert_eq!(conn.head_buf.capacity(), before);
    }

    fn read_head_from(bytes: &[u8]) -> Result<String, HeadError> {
        let mut r = BufReader::new(std::io::Cursor::new(bytes.to_vec()));
        let mut head = String::new();
        let mut line = Vec::new();
        read_head(&mut r, &mut head, &mut line, &|| false, Duration::from_secs(5))?;
        Ok(head)
    }

    #[test]
    fn stop_during_read_is_distinguishable_from_eof() {
        // 同样是"读不到请求"，服务端主动关必须能被上层识别出来，
        // 否则关闭期只能静默断连、给不出 503
        let mut r = BufReader::new(std::io::Cursor::new(b"GET / HTT".to_vec()));
        let stop = || true;
        let mut head = String::new();
        let mut line = Vec::new();
        assert!(matches!(
            read_head(&mut r, &mut head, &mut line, &stop, Duration::from_secs(5)),
            Err(HeadError::Stopped)
        ));
    }

    /// 每次 `fill_buf` 只吐 `step` 字节，逼出"结束标记跨缓冲段"那条路径。
    struct Chunked {
        data: Vec<u8>,
        pos: usize,
        step: usize,
    }

    impl std::io::Read for Chunked {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            let end = (self.pos + self.step).min(self.data.len());
            let n = end - self.pos;
            b[..n].copy_from_slice(&self.data[self.pos..end]);
            self.pos = end;
            Ok(n)
        }
    }

    impl std::io::BufRead for Chunked {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            let end = (self.pos + self.step).min(self.data.len());
            Ok(&self.data[self.pos..end])
        }
        fn consume(&mut self, n: usize) {
            self.pos = (self.pos + n).min(self.data.len());
        }
    }

    fn read_head_chunked(bytes: &[u8], step: usize) -> Result<String, HeadError> {
        let mut r = Chunked {
            data: bytes.to_vec(),
            pos: 0,
            step,
        };
        let mut head = String::new();
        let mut line = Vec::new();
        read_head(&mut r, &mut head, &mut line, &|| false, Duration::from_secs(5))?;
        Ok(head)
    }

    #[test]
    fn blank_line_spanning_buffer_fills() {
        // step 取 1..3：`\r\n\r\n` 的每一种跨段切分都要走一遍
        for step in 1..=3 {
            let head =
                read_head_chunked(b"GET / HTTP/1.1\r\nHost: a\r\n\r\nBODY", step).unwrap();
            assert_eq!(head, "GET / HTTP/1.1\r\nHost: a\r\n\r\n", "step={step}");
        }
    }

    #[test]
    fn bare_lf_spanning_buffer_fills_is_malformed() {
        for step in 1..=3 {
            assert!(
                matches!(
                    read_head_chunked(b"GET / HTTP/1.1\nX: 1\n\n", step),
                    Err(HeadError::Malformed(_))
                ),
                "step={step}"
            );
        }
    }

    #[test]
    fn oversized_line_spanning_buffer_fills_is_too_large() {
        let mut bytes = b"GET /".to_vec();
        bytes.extend_from_slice(&vec![b'a'; MAX_REQUEST_LINE + 100]);
        bytes.extend_from_slice(b"\r\n\r\n");
        for step in 1..=3 {
            assert!(
                matches!(read_head_chunked(&bytes, step), Err(HeadError::TooLarge)),
                "step={step}"
            );
        }
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
        let mut a = [0u8; TRACE_MAX];
        let mut b = [0u8; TRACE_MAX];
        let an = write_trace_id(&mut a, 1, 0);
        let bn = write_trace_id(&mut b, 2, 0);
        assert_ne!(&a[..an], &b[..bn]);
        assert!(an > 3 && an <= TRACE_MAX);
    }
}

/// `TCP_NODELAY` 开关：默认开（小响应一写就走，不让 Nagle 攒）。
/// `RSBOOT_NODELAY=0` 关掉，只为 A/B 量化它的系统调用成本。
fn nodelay_on() -> bool {
    use std::sync::OnceLock;
    static F: OnceLock<bool> = OnceLock::new();
    *F.get_or_init(|| std::env::var("RSBOOT_NODELAY").map(|v| v != "0").unwrap_or(true))
}

/// `x-request-id` 模式：0 = 正常生成（默认），1 = 固定 id，2 = 不发。
fn trace_mode() -> u8 {
    use std::sync::OnceLock;
    static F: OnceLock<u8> = OnceLock::new();
    *F.get_or_init(|| {
        std::env::var("RSBOOT_NO_TRACE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    })
}

impl Drop for Conn {
    fn drop(&mut self) {
        // 连接没了，就从 epoll 集合里摘掉：fd 号会被下一条连接复用，
        // 留着"已注册"的假账会让新连接永远收不到读事件。
        if let Some(fd) = self.raw_fd() {
            super::poll::defer_unregister(fd);
        }
    }
}
