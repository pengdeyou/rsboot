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

use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::runtime::context::Context;
use crate::server::conn::{Conn, ConnOutcome, Limits};
use crate::web::route::Router;

pub mod conn;
pub mod parse;
pub mod poll;
pub mod signal;
pub mod write;

pub use conn::Limits as ConnLimits;

/// `poll` 的单个超时上限：决定关闭/跛鸭判定的精度，也决定空闲时醒来的频率。
/// 5ms 对关闭响应足够快，对 CPU 几乎等于零成本（poll 不占 CPU，只在超时后醒一次）。
const ACCEPT_POLL_MS: i32 = 5;
/// 一次可读最多排空多少条连接：突发流量下别让 poll 和 accept 来回打转。
const MAX_ACCEPT_PER_WAIT: usize = 64;
/// worker 等连接就绪的超时：决定"新连接/关闭信号"的感知粒度，不决定吞吐。
const WORKER_POLL_MS: i32 = 1;

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
        let workers = self.workers;
        let mut senders: Vec<SyncSender<Conn>> = Vec::with_capacity(workers);
        let mut handles = Vec::with_capacity(workers);
        // worker 启动握手：全部就绪才开始 accept。
        // 不让"半残服务"上线——两个 worker 里只起来一个，比直接启动失败更难查。
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<std::result::Result<(), String>>();
        // 内部关闭标志：drain 期满后置位。它让**已建立的连接**知道
        // "别再给我派新请求了"，而不是只靠主线程停 accept。
        let closing = Arc::new(AtomicBool::new(false));

        for i in 0..workers {
            let (tx, rx) = sync_channel::<Conn>(256);
            senders.push(tx);

            let router = Arc::clone(&self.router);
            let factory = Arc::clone(&self.factory);
            let limits = self.limits;
            let stop = Arc::clone(&shutdown);
            let close = Arc::clone(&closing);
            let on_exit = self.on_worker_exit.clone();
            let ready = ready_tx.clone();
            let handle = std::thread::Builder::new()
                .name(format!("rsboot-w{i}"))
                .spawn(move || {
                    let ctx = match factory() {
                        Ok(c) => c,
                        Err(e) => {
                            let msg = format!("worker {i} 初始化失败：{e}");
                            eprintln!("[rsboot] {msg}");
                            let _ = ready.send(Err(msg));
                            return;
                        }
                    };
                    if ready.send(Ok(())).is_err() {
                        // 主线程已经放弃等待（另一个 worker 失败了），这个也别干等了
                        if let Some(hook) = &on_exit {
                            hook(&ctx);
                        }
                        return;
                    }
                    // 任一来源要求关闭都算：外部编排，或 drain 期满的内部关闭
                    let should_stop = || stop.load(Ordering::Relaxed) || close.load(Ordering::Relaxed);
                    // 本 worker 负责的连接表。worker 不是"拿到一条连接读到死"，
                    // 而是**同时持有它队列里的所有连接**，用 `poll` 等任意一条就绪。
                    //
                    // 这一层是必须的：线程-per-worker 的模型下，"一条连接读到死"
                    // 会让空闲的 keep-alive 连接长期占住 worker，后面的连接全部饿死
                    // （基准里 max = 5004ms 就是这个）。多路复用把这个问题从根上去掉，
                    // 而且不需要为每条连接开线程。
                    let mut conns: Vec<Conn> = Vec::new();
                    let mut idle_spins = 0u32;
                    loop {
                        // 关闭必须能立刻生效：worker 自己握着 sender，
                        // 光靠 "主线程 drop senders" 永远收不到 Disconnected。
                        if should_stop() {
                            // 手上和队列里的连接都得给明确答复（503 + close），
                            // 不能静默断——否则客户端只看到 "empty reply"。
                            for mut c in conns.drain(..) {
                                let _ = c.serve_once(&router, &ctx, &limits, &should_stop);
                            }
                            while let Ok(mut c) = rx.try_recv() {
                                let _ = c.serve_once(&router, &ctx, &limits, &should_stop);
                            }
                            break;
                        }

                        // 先把新派来的连接收进本地表
                        loop {
                            match rx.try_recv() {
                                Ok(c) => conns.push(c),
                                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                            }
                        }

                        if conns.is_empty() {
                            // 手上没连接：等新连接（50ms 只是为了能及时看到关闭标志）
                            match rx.recv_timeout(std::time::Duration::from_millis(50)) {
                                Ok(c) => conns.push(c),
                                Err(RecvTimeoutError::Timeout) => continue,
                                Err(RecvTimeoutError::Disconnected) => break,
                            }
                            continue;
                        }

                        // 等任意一条连接可读。1ms 超时让"刚派进来的新连接"也能
                        // 很快被感知，同时空闲时几乎不耗 CPU（poll 阻塞，不空转）。
                        let fds: Vec<i32> = conns.iter().filter_map(|c| c.raw_fd()).collect();
                        let ready = match poll::wait_multi(&fds, WORKER_POLL_MS) {
                            Ok(r) => r,
                            Err(e) => {
                                eprintln!("[rsboot] worker poll 失败：{e}");
                                std::thread::sleep(std::time::Duration::from_millis(1));
                                continue;
                            }
                        };

                        if ready.is_empty() {
                            idle_spins += 1;
                            if idle_spins >= 8 {
                                idle_spins = 0;
                                // 长时间没有流量：让出一个调度片，别在 poll 里磨 CPU
                                std::thread::sleep(std::time::Duration::from_millis(1));
                            }
                            continue;
                        }
                        idle_spins = 0;

                        // 只处理就绪的连接。`swap_remove` + 处理 + 放回：O(1)
                        //
                        // 必须**按下标降序**处理。`swap_remove(i)` 会把尾部元素搬到 i，
                        // `push` 又会改变长度；若按升序处理，后续的 `ready[j]`（j > i）
                        // 就指向了别的连接——表现为"同一条连接被处理两次、另一条就绪的
                        // 连接延后一轮"。更糟的是被重复处理的那条此刻可能已无数据，
                        // `serve_once` 会阻塞到读超时，把 worker 整条卡住。
                        let mut ready = ready;
                        ready.sort_unstable_by(|a, b| b.cmp(a));
                        for idx in ready {
                            if idx >= conns.len() {
                                continue;
                            }
                            let mut c = conns.swap_remove(idx);
                            if let Ok(ConnOutcome::KeepAlive) =
                                c.serve_once(&router, &ctx, &limits, &should_stop)
                            {
                                conns.push(c);
                            }
                            // 关闭/出错/对端要 close 的一律直接丢
                        }
                    }

                    // worker 收尾：模块在这里释放线程局部资源（连接、缓冲）
                    if let Some(hook) = &on_exit {
                        hook(&ctx);
                    }
                })
                .map_err(|e| Error::Lifecycle(format!("启动 worker {i} 失败：{e}")))?;
            handles.push(handle);
        }

        drop(ready_tx);
        let mut ready_ok = 0usize;
        let mut ready_failed: Option<String> = None;
        for _ in 0..workers {
            match ready_rx.recv() {
                Ok(Ok(())) => ready_ok += 1,
                Ok(Err(e)) => ready_failed = Some(e),
                // 所有 sender 都 drop 了还没等齐：说明有 worker 在握手前就退了
                Err(_) => break,
            }
        }
        if let Some(e) = ready_failed {
            drop(senders);
            for h in handles {
                let _ = h.join();
            }
            return Err(Error::Lifecycle(format!("启动失败：{e}")));
        }
        if ready_ok == 0 {
            return Err(Error::Lifecycle(
                "没有一个 worker 成功启动（检查数据库配置与模块的 start_worker）".into(),
            ));
        }

        self.listener
            .set_nonblocking(true)
            .map_err(|e| Error::Lifecycle(format!("设置非阻塞模式失败：{e}")))?;

        let watch_signal = self.watch_signal;
        let grace = self.grace;
        let drain = self.drain;
        let status = self.status.clone();
        let mut rr = 0usize;
        // 跛鸭期的截止时刻。`None` = 还没开始关。
        let mut drain_deadline: Option<Instant> = None;
        loop {
            if shutdown.load(Ordering::Relaxed) {
                status.set(ServeState::Closing);
                closing.store(true, Ordering::Relaxed);
                break;
            }
            // 第一次看到信号：进入跛鸭期。此间照常接客，过了预算才真停。
            if drain_deadline.is_none() && watch_signal && signal::received() {
                status.set(ServeState::Draining);
                drain_deadline = Some(Instant::now() + drain);
            }
            if let Some(d) = drain_deadline {
                if Instant::now() >= d {
                    status.set(ServeState::Closing);
                    // 让 worker 里**已建立的连接**开始礼貌拒绝，
                    // 不然它们会一路 keep-alive 到 idle 超时才停
                    closing.store(true, Ordering::Relaxed);
                    break;
                }
            }
            // 睡到"有连接"为止，而不是 sleep 轮询。轮询会让每条短连接
            // 白等最多一个轮询周期（原来是 20ms，基准里直接把 QPS 打到 116）。
            #[cfg(unix)]
            {
                use std::os::unix::io::AsRawFd;
                match poll::wait_readable(self.listener.as_raw_fd(), ACCEPT_POLL_MS) {
                    Ok(true) => {}
                    Ok(false) => continue, // 超时：回到循环头检查关闭/drain
                    Err(e) => {
                        eprintln!("[rsboot] poll 失败：{e}");
                        std::thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                }
            }
            #[cfg(not(unix))]
            {
                std::thread::sleep(Duration::from_millis(ACCEPT_POLL_MS as u64));
            }

            // 一次可读往往意味着 backlog 里不止一条：排空它，别让突发连接
            // 排队等下一轮 poll。上限 64 免得一条线程被 accept 独占。
            for _ in 0..MAX_ACCEPT_PER_WAIT {
                match self.listener.accept() {
                    Ok((stream, _peer)) => {
                        let tx = &senders[rr % workers];
                        rr = rr.wrapping_add(1);
                        // 每条连接只在这里 dup 一次 fd（读缓冲跟着连接走）
                        let c = match Conn::new(stream, &self.limits) {
                            Ok(c) => c,
                            Err(_) => continue,
                        };
                        match tx.try_send(c) {
                            Ok(()) => {}
                            // 队列满或 worker 已死 → 当场 503，不进内核 backlog 继续堆
                            Err(std::sync::mpsc::TrySendError::Full(c)) => {
                                reject_overloaded(c.into_stream())
                            }
                            Err(std::sync::mpsc::TrySendError::Disconnected(c)) => {
                                reject_overloaded(c.into_stream())
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        // 文件描述符耗尽之类的临时错误：退避后重试，不让整个服务退出
                        eprintln!("[rsboot] accept 失败：{e}");
                        std::thread::sleep(Duration::from_millis(100));
                        break;
                    }
                }
            }
        }

        // 关闭 sender → worker 的 recv 结束 → 处理完手上连接后退出
        status.set(ServeState::Closing);
        drop(senders);

        // 等 worker 收尾，但有预算。某个 worker卡在一条慢请求上不该让整个进程
        // 永远退不出去——超时就放弃等待（线程泄漏，但进程马上结束，无碍）。
        let deadline = Instant::now() + grace;
        let mut stragglers = 0usize;
        for h in handles {
            while !h.is_finished() {
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            if h.is_finished() {
                let _ = h.join();
            } else {
                stragglers += 1;
            }
        }
        if stragglers > 0 {
            eprintln!(
                "[rsboot] 关闭超过 {:?}，{stragglers} 个 worker 仍未退出（进行中的请求可能被硬切断）",
                grace
            );
        }
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

fn reject_overloaded(mut s: TcpStream) {
    let body = "{\"detail\":\"服务繁忙，worker 队列已满\"}";
    let head = format!(
        "HTTP/1.1 503 Service Unavailable\r\ncontent-type: application/json; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(body.as_bytes());
    let _ = s.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
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
