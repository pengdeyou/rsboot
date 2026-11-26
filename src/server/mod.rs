//! HTTP/1.1 服务端：监听、worker、优雅关闭。
//!
//! ## worker-per-core，每个 worker 一个 `Context`
//!
//! `Context` 里的 `Session` 是 `!Send`（SQLite 连接不能跨线程），所以**不能**
//! 在主线程建好再分发。每个 worker 启动时自己调一次工厂函数建 Context，
//! 各自持有自己的数据库连接。这也顺带解决了"多线程抢一把 DB 锁"的问题。
//!
//! 代价是连接数 = workers × 每 worker 的池大小，需要按这个数配数据库的
//! `max_connections`。
//!
//! ## 分发与背压
//!
//! 主线程 `accept` 后 round-robin 投给 worker 的**有界队列**（容量 256）。
//! 队列满就当场回 503 并关闭，而不是无限排队——排队只会把超时往后推，
//! 客户端等到的仍是失败，却多占了一份内存。
//!
//! ## accept 用非阻塞轮询
//!
//! 没有用"自连一次唤醒 accept"或 signal：轮询 20ms 一次，空转成本可以接受，
//! 换来的是关闭语义简单可靠（置位 flag，下一轮就退出），也不会在容器里
//! 因为信号投递不确定而卡住。

use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::runtime::context::Context;
use crate::server::conn::Limits;
use crate::web::route::Router;

pub mod clock;
pub mod coio;
pub mod conn;
pub mod mn;
pub mod parse;
pub mod poll;
pub(crate) mod profile;
pub mod signal;
pub mod write;

pub(crate) mod accept;
pub(crate) mod lifecycle;
pub(crate) mod worker;

pub use conn::Limits as ConnLimits;

/// listen backlog。std 硬编码 128，短连接突发下会丢 SYN（见 `raise_backlog`）。
/// 实际生效值由 `net.core.somaxconn` 截断。
const LISTEN_BACKLOG: i32 = 1024;

const ST_SERVING: u8 = 0;
const ST_DRAINING: u8 = 1;
const ST_CLOSING: u8 = 2;

/// 服务对外状态。业务侧拿它做 readiness 探针：
/// 收到 SIGTERM 后先 `Draining`（还能正常服务，LB 趁这段时间摘流量），
/// 过了 drain 预算才 `Closing`（新请求开始被 503）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeState {
    Serving,
    Draining,
    Closing,
}

/// 可共享的状态句柄。放在 `Context` 里给 handler 读。
#[derive(Debug, Clone)]
pub struct ServeStatus(Arc<AtomicU8>);

impl ServeStatus {
    pub fn new() -> Self {
        Self(Arc::new(AtomicU8::new(ST_SERVING)))
    }

    pub fn state(&self) -> ServeState {
        match self.0.load(Ordering::Acquire) {
            ST_DRAINING => ServeState::Draining,
            ST_CLOSING => ServeState::Closing,
            _ => ServeState::Serving,
        }
    }

    pub fn is_serving(&self) -> bool {
        self.state() == ServeState::Serving
    }

    /// 正在摘流量：能服务，但不该再被派新流量。
    pub fn is_draining(&self) -> bool {
        self.state() == ServeState::Draining
    }

    /// 已在关闭：新请求会拿到 503。
    pub fn is_closing(&self) -> bool {
        self.state() == ServeState::Closing
    }

    pub fn set(&self, st: ServeState) {
        let v = match st {
            ServeState::Serving => ST_SERVING,
            ServeState::Draining => ST_DRAINING,
            ServeState::Closing => ST_CLOSING,
        };
        self.0.store(v, Ordering::Release);
    }
}

impl Default for ServeStatus {
    fn default() -> Self {
        Self::new()
    }
}

/// 每个 worker 建自己 Context 的工厂。
pub type ContextFactory = Arc<dyn Fn() -> Result<Context> + Send + Sync>;

/// worker 退出前的钩子（跑模块的 `stop_worker`）。抽成类型别名是为了让
/// `Server` 的字段一眼看得懂，而不是让 clippy 满意。
pub type WorkerExitHook = Arc<dyn Fn(&Context) + Send + Sync>;

pub struct Server {
    listener: TcpListener,
    addr: SocketAddr,
    router: Arc<Router>,
    factory: ContextFactory,
    limits: Limits,
    workers: usize,
    /// worker 退出前调用（模块的 `stop_worker` 挂在这里）
    on_worker_exit: Option<WorkerExitHook>,
    /// 关闭时等 worker 收尾的预算。超时就放弃等待并记日志，绝不无限挂住。
    grace: Duration,
    /// 收到关闭信号后先当"跛鸭"服务多久。这段时间里照常接客，
    /// 给 LB / k8s 摘流量的时间。0 表示立刻停 accept。
    drain: Duration,
    /// 连接借调的累计计数（迁移了多少条连接）。给测试与运维观测用。
    rebalance_stats: Option<Arc<std::sync::atomic::AtomicUsize>>,
    /// 对外可见的服务状态
    status: ServeStatus,
    /// 是否把 SIGINT/SIGTERM 也当作关闭信号
    watch_signal: bool,
}

impl Server {
    /// 绑定端口。`":0"` 由系统分配（测试与本地开发常用）。
    ///
    /// **这里不调用工厂**。早先版本在 bind 里预检一次工厂，结果是
    /// "绑定成功前就跑了模块的 `start_worker`，而那个 Context 之后被丢弃"——
    /// 副作用（连接、线程、注册）全部泄漏，且没有对称的 `stop_worker`。
    /// 真正的验证放在 `run` 里：等全部 worker 报告就绪再开始 accept。
    pub fn bind(
        addr: &str,
        router: Arc<Router>,
        factory: ContextFactory,
        limits: Limits,
        workers: usize,
    ) -> Result<Self> {
        let listener = TcpListener::bind(addr).map_err(|e| {
            Error::Config(format!("监听 {addr} 失败：{e}。端口被占用，或地址格式不对"))
        })?;
        raise_backlog(&listener);
        let addr = listener
            .local_addr()
            .map_err(|e| Error::Lifecycle(format!("读取监听地址失败：{e}")))?;

        Ok(Self {
            listener,
            addr,
            router,
            factory,
            limits,
            workers: workers.max(1),
            on_worker_exit: None,
            grace: Duration::from_secs(10),
            drain: Duration::ZERO,
            rebalance_stats: None,
            status: ServeStatus::new(),
            watch_signal: true,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// worker 退出前的钩子（用来跑模块的 `stop_worker`）。
    pub fn on_worker_exit(mut self, hook: WorkerExitHook) -> Self {
        self.on_worker_exit = Some(hook);
        self
    }

    /// 关闭时等 worker 收尾的预算。默认 10 秒。
    pub fn grace(mut self, d: Duration) -> Self {
        self.grace = d;
        self
    }

    /// 观测钩子：把"连接借调累计迁移了多少条连接"写进外部计数器。
    ///
    /// 这个数对运维是有意义的：持续增长说明 worker 之间负载长期不均
    /// （例如长连接大量集中在某一匹 worker 上），值得关注而不是无视。
    pub fn rebalance_stats(mut self, h: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        self.rebalance_stats = Some(h);
        self
    }

    /// 跛鸭期：收到信号后仍正常服务多久再停 accept。默认 0。
    pub fn drain(mut self, d: Duration) -> Self {
        self.drain = d;
        self
    }

    /// 共享状态句柄。handler 从 `Context` 里取到的是同一个对象。
    pub fn status(mut self, s: ServeStatus) -> Self {
        self.status = s;
        self
    }

    /// 是否响应 SIGINT/SIGTERM。测试里关掉，避免全局标志互相干扰。
    pub fn watch_signal(mut self, yes: bool) -> Self {
        self.watch_signal = yes;
        self
    }

    /// 阻塞运行，直到 `shutdown` 置位。
    ///
    /// 关闭过程：停止 accept → 丢弃 sender（worker 的 `recv` 返回错误）→
    /// 等 worker 把手上这条连接处理完 → join。进行中的请求不会被粗暴切断。
    pub fn run(self, shutdown: Arc<AtomicBool>) -> Result<()> {
        // 先把数据库 socket 接到协程链路上：一次装好，之后所有
        // `mysql+co://` 连接读写都会让出 worker。
        mn::net::install_db_hooks();

        let workers = self.workers;
        // 内部关闭标志：drain 期满后置位。它让**已建立的连接**知道
        // "别再给我派新请求了"，而不是只靠主线程停 accept。
        let closing = Arc::new(AtomicBool::new(false));

        // 队列与唤醒器必须**先全建好**再 spawn：借调要求每个 worker 手里
        // 都握着全部 sender/waker。
        let (senders, mut receivers, wakers) = worker::channels(workers)?;
        let senders = Arc::new(senders);
        let wakers = Arc::new(wakers);

        // 协程运行时随这次服务走，不是进程级全局：同进程起第二个 Server
        // 时两者的 worker id 会撞车（踩过，表现为偶发请求挂死）。
        let rt = mn::sched::Runtime::new();
        rt.start_timer();

        let shared = Arc::new(worker::Shared {
            workers,
            router: Arc::clone(&self.router),
            factory: Arc::clone(&self.factory),
            limits: self.limits,
            shutdown: Arc::clone(&shutdown),
            closing: Arc::clone(&closing),
            on_exit: self.on_worker_exit.clone(),
            senders: Arc::clone(&senders),
            wakers: Arc::clone(&wakers),
            // 每 worker 的累计处理请求数，用于连接借调
            served: Arc::new((0..workers).map(|_| AtomicU64::new(0)).collect()),
            rt,
            rebalance_stats: self.rebalance_stats.clone(),
        });

        // worker 启动握手：全部就绪才开始 accept。不让"半残服务"上线——
        // 两个 worker 里只起来一个，比直接启动失败更难查。
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<std::result::Result<(), String>>();
        let handles = worker::spawn_all(Arc::clone(&shared), &mut receivers, ready_tx)?;
        let handles = lifecycle::await_ready(workers, ready_rx, Arc::clone(&senders), handles)?;

        accept::run(accept::Loop {
            listener: &self.listener,
            senders: senders.as_slice(),
            wakers: wakers.as_slice(),
            limits: self.limits,
            workers,
            shutdown: &shutdown,
            closing: &closing,
            status: &self.status,
            drain: self.drain,
            watch_signal: self.watch_signal,
        })?;

        lifecycle::shutdown(&wakers, senders, handles, self.grace, &self.status);
        Ok(())
    }
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("addr", &self.addr)
            .field("workers", &self.workers)
            .field("routes", &self.router.len())
            .finish()
    }
}

/// 把 listen backlog 抬到 `LISTEN_BACKLOG`。
///
/// `std::net::TcpListener::bind` 内部把 backlog 硬编码成 **128**。短连接或突发
/// 流量下这个值很容易被打满：内核开始丢 SYN，客户端按 RTO 重传，现象是
/// "平均很快、偶尔几百毫秒"的尾部尖峰——只看均值完全查不出来。
///
/// 对**已经 listen** 的 socket 再调一次 `listen(fd, n)` 是 POSIX 允许的，
/// 内核会更新 backlog 上限，不影响已排队的连接。所以不需要自己重写
/// `socket/bind/listen` 那一套，也就不会引入地址解析与 IPv6 的坑。
///
/// 实际生效值会被 `net.core.somaxconn` 截断，设大了无害。
#[cfg(unix)]
fn raise_backlog(l: &TcpListener) {
    use std::os::raw::c_int;
    use std::os::unix::io::AsRawFd;

    unsafe extern "C" {
        fn listen(fd: c_int, backlog: c_int) -> c_int;
    }

    // SAFETY: fd 由 `l` 持有且此刻正在 listen，只调整 backlog 数值。
    let rc = unsafe { listen(l.as_raw_fd(), LISTEN_BACKLOG) };
    if rc != 0 {
        // 抬不上去不该让服务起不来：留在系统默认值，记一行日志
        eprintln!(
            "[rsboot] 调整 listen backlog 失败（保持系统默认）：{}",
            std::io::Error::last_os_error()
        );
    }
}

#[cfg(not(unix))]
fn raise_backlog(_l: &TcpListener) {}


#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;
    use crate::config::{Config, Environment};
    use crate::web::http::{Method, Status};
    use crate::web::request::Request;
    use crate::web::response::Response;
    use std::io::Read;

    fn ctx() -> Result<Context> {
        Ok(Context::new(
            Config::new(),
            vec![(
                "default".to_string(),
                rsbatis::create_engine("sqlite://:memory:").map_err(|e| Error::Missing(e.message))?,
            )],
            Some("default".to_string()),
            Environment::Dev,
        ))
    }

    fn start(router: Router) -> (SocketAddr, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
        let server = Server::bind(
            "127.0.0.1:0",
            Arc::new(router),
            Arc::new(ctx),
            Limits::default(),
            2,
        )
        .unwrap();
        let addr = server.local_addr();
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = Arc::clone(&stop);
        let h = std::thread::spawn(move || server.run(s2).unwrap());
        (addr, stop, h)
    }

    fn roundtrip(addr: SocketAddr, raw: &str) -> String {
        use std::io::Write as _;
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        s.write_all(raw.as_bytes()).unwrap();
        let mut buf = Vec::new();
        let _ = s.read_to_end(&mut buf);
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn serves_get_end_to_end() {
        let router = Router::new()
            .get("/hi", |_, _| Response::text("hello"))
            .unwrap();
        let (addr, stop, h) = start(router);

        let resp = roundtrip(addr, "GET /hi HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n");
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "{resp}");
        assert!(resp.contains("content-length: 5"), "{resp}");
        assert!(resp.contains("x-request-id:"), "响应必须回写 trace id：{resp}");
        assert!(resp.ends_with("hello"), "{resp}");

        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();
    }

    #[test]
    fn unknown_path_is_404_json() {
        let router = Router::new().get("/hi", |_, _| Response::text("x")).unwrap();
        let (addr, stop, h) = start(router);
        let resp = roundtrip(addr, "GET /nope HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n");
        assert!(resp.starts_with("HTTP/1.1 404 Not Found"), "{resp}");
        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();
    }

    #[test]
    fn keep_alive_serves_two_requests_on_one_connection() {
        use std::io::Write as _;
        let router = Router::new().get("/hi", |_, _| Response::text("ok")).unwrap();
        let (addr, stop, h) = start(router);
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

        for _ in 0..2 {
            s.write_all(b"GET /hi HTTP/1.1\r\nHost: a\r\n\r\n").unwrap();
            // 读到 "\r\n\r\n" 再读到 body 结束
            let mut buf = Vec::new();
            let mut tmp = [0u8; 64];
            loop {
                let n = s.read(&mut tmp).unwrap();
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.ends_with(b"ok") {
                    break;
                }
            }
            assert!(String::from_utf8(buf).unwrap().contains("200 OK"));
        }

        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();
    }

    #[test]
    fn smuggling_attempt_is_rejected_with_close() {
        let router = Router::new().post("/x", |_, _| Response::text("x")).unwrap();
        let (addr, stop, h) = start(router);
        let resp = roundtrip(
            addr,
            "POST /x HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\nhello",
        );
        assert!(resp.contains("400 Bad Request"), "{resp}");
        assert!(resp.contains("connection: close"), "歧义请求必须断开：{resp}");
        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();
    }

    #[test]
    fn oversized_body_is_413() {
        let router = Router::new().post("/x", |_, _| Response::text("x")).unwrap();
        let server = Server::bind(
            "127.0.0.1:0",
            Arc::new(router),
            Arc::new(ctx),
            Limits {
                max_body: 64,
                ..Limits::default()
            },
            1,
        )
        .unwrap();
        let addr = server.local_addr();
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = Arc::clone(&stop);
        let h = std::thread::spawn(move || server.run(s2).unwrap());

        let resp = roundtrip(
            addr,
            "POST /x HTTP/1.1\r\nHost: a\r\nContent-Length: 4096\r\n\r\n",
        );
        assert!(resp.contains("413"), "{resp}");
        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();
    }

    #[test]
    fn handler_panic_does_not_kill_worker() {
        let router = Router::new()
            .get("/boom", |_, _| -> Response { panic!("boom") })
            .unwrap()
            .get("/ok", |_, _| Response::text("fine"))
            .unwrap();
        let (addr, stop, h) = start(router);

        let r1 = roundtrip(addr, "GET /boom HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n");
        assert!(r1.contains("500"), "{r1}");
        // 同一个 worker 仍然能服务下一条连接
        let r2 = roundtrip(addr, "GET /ok HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n");
        assert!(r2.contains("200 OK") && r2.ends_with("fine"), "{r2}");

        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();
    }

    #[test]
    fn router_works_without_server() {
        // 路由表不依赖 socket：handler 单测不需要起服务
        let r = Router::new().get("/x", |_, _| Response::text("y")).unwrap();
        let c = ctx().unwrap();
        let mut req = Request::new(Method::Get, "/x");
        assert_eq!(r.handle(&mut req, &c).status, Status::OK);
    }
}
