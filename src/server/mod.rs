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
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::runtime::context::Context;
use crate::server::conn::{Conn, ConnOutcome, Limits};
use crate::web::route::Router;

pub mod conn;
pub mod mn;
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
/// worker 等连接就绪的兜底超时。**正常情况下 worker 会被 `Waker` 立刻叫醒**，
/// 这个值只在"唤醒链路漏了一处"时兜底，防止 worker 永久卡在 `poll` 里关不掉。
/// 它不再决定延迟，也不再决定空闲 CPU（那是 `Waker` 的活）。
const WORKER_IDLE_MS: i32 = 500;
/// listen backlog。std 硬编码 128，短连接突发下会丢 SYN（见 `raise_backlog`）。
/// 实际生效值由 `net.core.somaxconn` 截断。
const LISTEN_BACKLOG: i32 = 1024;
/// 连接借调的检查周期。太短会让 worker 频繁搬连接（连接迁移有成本），
/// 太长则失衡要持续一段时间才被纠正。
const REBALANCE_EVERY: Duration = Duration::from_millis(200);
/// 手上至少要有这么多连接才考虑借调。2 条时搬走一半只会让两边来回乒乓，
/// 3 条起才有意义（搬 1 条、剩 2 条）。
const REBALANCE_MIN_CONNS: usize = 3;
/// 自己累计处理量超过同行的这个百分比才触发借调。取 130 表示"多干 30%"。
const REBALANCE_TRIGGER_PCT: usize = 130;

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
        let mut senders: Vec<SyncSender<Conn>> = Vec::with_capacity(workers);
        // 每 worker 一个唤醒器：派发连接给谁，就叫醒谁。这样 worker 的 poll
        // 可以长时间阻塞（近乎零空转），又不用等一个 tick 才发现新连接。
        let mut wakers: Vec<poll::Waker> = Vec::with_capacity(workers);
        let mut handles = Vec::with_capacity(workers);
        // worker 启动握手：全部就绪才开始 accept。
        // 不让"半残服务"上线——两个 worker 里只起来一个，比直接启动失败更难查。
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<std::result::Result<(), String>>();
        // 内部关闭标志：drain 期满后置位。它让**已建立的连接**知道
        // "别再给我派新请求了"，而不是只靠主线程停 accept。
        let closing = Arc::new(AtomicBool::new(false));

        // 先建好全部队列与唤醒器，再 spawn。原因：连接借调（见 worker 里的
        // `rebalance`）需要 worker 能往**别人**的队列里塞连接，所以每个 worker
        // 都得持有全部 sender / waker，这些必须先齐。
        let mut receivers: Vec<Option<std::sync::mpsc::Receiver<Conn>>> =
            Vec::with_capacity(workers);
        for _ in 0..workers {
            let (tx, rx) = sync_channel::<Conn>(256);
            senders.push(tx);
            receivers.push(Some(rx));
            wakers.push(
                poll::Waker::new()
                    .map_err(|e| Error::Lifecycle(format!("创建唤醒器失败：{e}")))?,
            );
        }
        let senders = Arc::new(senders);
        // 协程运行时随这次服务走，不是进程级全局：同进程起第二个 Server
        // 时两者的 worker id 会撞车（踩过，表现为偶发请求挂死）。
        let rt = mn::sched::Runtime::new();
        rt.start_timer();
        let wakers = Arc::new(wakers);
        // 每 worker 的累计处理请求数，用于连接借调
        let rebalance_stats = self.rebalance_stats.clone();
        let served: Arc<Vec<std::sync::atomic::AtomicU64>> = Arc::new(
            (0..workers)
                .map(|_| std::sync::atomic::AtomicU64::new(0))
                .collect(),
        );

        for i in 0..workers {
            let rx = receivers[i]
                .take()
                .expect("每个 worker 的接收端只取一次");
            let router = Arc::clone(&self.router);
            let factory = Arc::clone(&self.factory);
            let limits = self.limits;
            let stop = Arc::clone(&shutdown);
            let close = Arc::clone(&closing);
            let on_exit = self.on_worker_exit.clone();
            let ready = ready_tx.clone();
            let my_waker = wakers[i].clone();
            let rebalance_stats = rebalance_stats.clone();
            let rt = Arc::clone(&rt);
            let all_senders = Arc::clone(&senders);
            let all_wakers = Arc::clone(&wakers);
            let served = Arc::clone(&served);
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
                    // 接上协程调度器：这个 worker 线程从此可以在 I/O 等待处让出。
                    // 定时器线程只有一个（进程级），`start_timer` 内部做了幂等。
                    mn::sched::init(rt, i, mn::sched::Shared::new(my_waker.clone()));
                    // 处理完、仍可 keep-alive 的连接由协程投回这里，主循环统一收回
                    let rets: Arc<std::sync::Mutex<Vec<Conn>>> =
                        Arc::new(std::sync::Mutex::new(Vec::new()));
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
                    // 常驻的 poll 集合：fd 数组与就绪缓冲只分配一次
                    let mut ps = poll::PollSet::with_waker(my_waker.read_fd());
                    let mut last_rebalance = std::time::Instant::now();
                    // 连接借调的冷却：刚被塞过连接的 worker 先别又把它们送回去
                    // （否则两个 worker 会互相推诿，连接在队列里空转）
                    let mut stole_at = std::time::Instant::now()
                        .checked_sub(REBALANCE_EVERY)
                        .unwrap_or_else(std::time::Instant::now);
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

                        // 推进协程：让出的请求在这里接着跑。worker 只有这一处会切进
                        // 协程栈，别的地方都是纯事件循环。
                        mn::sched::pump(4096);
                        {
                            let mut g = rets.lock().unwrap();
                            if !g.is_empty() {
                                conns.extend(g.drain(..));
                            }
                        }

                        // 先把新派来的连接收进本地表
                        let mut got_from_peer = false;
                        while let Ok(c) = rx.try_recv() {
                            conns.push(c);
                            got_from_peer = true;
                        }
                        if got_from_peer {
                            stole_at = std::time::Instant::now();
                        }

                        // 连接借调：静态按连接数均分 ≠ 负载均分。
                        // 50 条连接里每条请求速率不同，实测两个 worker 的 CPU 能差
                        // 24%（2.72s vs 2.20s），吞吐被慢的那个拖住。这里按**累计
                        // 处理请求数**做周期性再平衡：干得比同行多 30% 就把一半连接
                        // 交出去，交给累计最少的那个。
                        if last_rebalance.elapsed() >= REBALANCE_EVERY
                            && stole_at.elapsed() >= REBALANCE_EVERY
                            && conns.len() >= REBALANCE_MIN_CONNS
                            && workers > 1
                        {
                            last_rebalance = std::time::Instant::now();
                            let mine = served[i].load(Ordering::Relaxed);
                            let total: u64 = served
                                .iter()
                                .map(|c| c.load(Ordering::Relaxed))
                                .sum();
                            let avg = total / workers as u64;
                            // 注意放大的倍数：`mine * 100 > avg * PCT` 才是"多 30%"
                            // （写 10 会变成"多 12 倍"，是测试逼出来的真 bug）
                            if mine * 100 > avg * (REBALANCE_TRIGGER_PCT as u64) {
                                // 交给累计最少的同行（不含自己）
                                let target = (0..workers)
                                    .filter(|&j| j != i)
                                    .min_by_key(|&j| served[j].load(Ordering::Relaxed))
                                    .expect("workers > 1 时必然有别人");
                                let give = conns.len() / 2;
                                let mut moved = 0usize;
                                for _ in 0..give {
                                    let Some(c) = conns.pop() else { break };
                                    match all_senders[target].try_send(c) {
                                        Ok(()) => moved += 1,
                                        Err(std::sync::mpsc::TrySendError::Full(c))
                                        | Err(std::sync::mpsc::TrySendError::Disconnected(c)) => {
                                            // 对方队列满/已退：放回自己手里，别再折腾
                                            conns.push(c);
                                            break;
                                        }
                                    }
                                }
                                if moved > 0 {
                                    if let Some(h) = &rebalance_stats {
                                        h.fetch_add(moved, Ordering::Relaxed);
                                    }
                                    // 先入队后唤醒，和 accept 路径同一个顺序约束
                                    all_wakers[target].notify();
                                    // 记一笔"我欠对方"，避免下一轮又立刻判定自己超载
                                    served[i].store(
                                        served[i].load(Ordering::Relaxed) / 2,
                                        Ordering::Relaxed,
                                    );
                                }
                            }
                        }

                        // 拼出本轮要等的 fd（唤醒器恒在 items[0]），然后真阻塞。
                        // 兜底超时只防"唤醒漏了"，正常路径由 `Waker` 立刻叫醒：
                        // 派发方 push 进队列之后才 notify，所以醒来必能 try_recv 到。
                        // 协程在等哪个 socket，就把它一并交给 `poll`——这是让
                        // "DB/下游调用"也能让出的全部机制。`n_conns` 记下分界点：
                        // 就绪下标小于它是连接，大于等于它是等待表里的 fd。
                        let n_conns = conns.len();
                        let wait_fds = mn::sched::wait_fds();
                        ps.reset_conns(
                            conns
                                .iter()
                                .filter_map(|c| c.raw_fd())
                                .chain(wait_fds),
                        );
                        // 队列里还有待跑的协程就立刻回来继续推进，别在 poll 里等
                        let idle = if mn::sched::pending() > 0 { 0 } else { WORKER_IDLE_MS };
                        if let Err(e) = ps.wait(idle) {
                            eprintln!("[rsboot] worker poll 失败：{e}");
                            std::thread::sleep(std::time::Duration::from_millis(5));
                            continue;
                        }
                        if ps.woken() {
                            // 不清零的话下一轮 poll 会立刻又返回，变成忙等
                            my_waker.drain();
                        }

                        // 只处理就绪的连接。`swap_remove` + 处理 + 放回：O(1)
                        //
                        // 必须**按下标降序**处理。`swap_remove(i)` 会把尾部元素搬到 i，
                        // `push` 又会改变长度；若按升序处理，后续的 `ready[j]`（j > i）
                        // 就指向了别的连接——表现为"同一条连接被处理两次、另一条就绪的
                        // 连接延后一轮"。更糟的是被重复处理的那条此刻可能已无数据，
                        // `serve_once` 会阻塞到读超时，把 worker 整条卡住。
                        // `ready` 天然升序，所以从后往前遍历即可，无需排序。
                        for k in (0..ps.ready_len()).rev() {
                            let idx = ps.ready_at(k);
                            if idx >= n_conns {
                                // 等待表里的 fd 就绪：把那个协程放回队列。
                                // 必须降序（同下面的理由）：`wake_wait` 内部也
                                // 用 `swap_remove`。
                                mn::sched::wake_wait(idx - n_conns);
                                continue;
                            }
                            if idx >= conns.len() {
                                continue;
                            }
                            let mut c = conns.swap_remove(idx);
                            served[i].fetch_add(1, Ordering::Relaxed);
                            // 请求处理放进协程：handler 里凡是要等 I/O 的地方
                            // （框架的 sleep / DB / 下游调用）都能让出，worker 立刻
                            // 回去处理别的连接。同步 handler 的签名和借用规则不变。
                            if mn::sched::ready() {
                                let rp: *const Router = &*router;
                                let cp: *const crate::runtime::context::Context = &ctx;
                                let s2 = Arc::clone(&stop);
                                let c2 = Arc::clone(&close);
                                let lim = limits;
                                let rets2 = Arc::clone(&rets);
                                mn::sched::spawn(move || {
                                    let stop_fn =
                                        || s2.load(Ordering::Relaxed) || c2.load(Ordering::Relaxed);
                                    // SAFETY: 同线程；router/ctx 活到 worker 退出
                                    let outcome =
                                        unsafe { c.serve_once(&*rp, &*cp, &lim, &stop_fn) };
                                    if let Ok(ConnOutcome::KeepAlive) = outcome {
                                        rets2.lock().unwrap().push(c);
                                    }
                                });
                            } else if let Ok(ConnOutcome::KeepAlive) =
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
                        let wi = rr % workers;
                        rr = rr.wrapping_add(1);
                        let tx = &senders[wi];
                        let wk = &wakers[wi];
                        // 每条连接只在这里 dup 一次 fd（读缓冲跟着连接走）
                        let c = match Conn::new(stream, &self.limits) {
                            Ok(c) => c,
                            Err(_) => continue,
                        };
                        match tx.try_send(c) {
                            // 先入队、后唤醒：worker 醒来必能取到，不会空跑
                            Ok(()) => wk.notify(),
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
        // worker 此时可能正阻塞在 poll 里（兜底超时 500ms），必须叫醒它们
        // 去看 should_stop，否则关闭会白等一个兜底周期。
        for w in wakers.iter() {
            w.notify();
        }
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
