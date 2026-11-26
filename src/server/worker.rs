//! worker 线程：连接多路复用、协程推进、连接借调。
//!
//! ## 为什么是"一个 worker 持有一批连接"，而不是"一条连接一个线程"
//!
//! 线程-per-dedicated-connection 的模型下，空闲的 keep-alive 连接会长期占住
//! worker，后面的连接全部饿死（基准里 max = 5004 ms 就是这个）。这里把每个
//! worker 队列里的所有连接都摆进同一个 `poll` 集合，谁就绪就服务谁，
//! 不需要为每条连接开线程。
//!
//! ## 关闭的两级
//!
//! 外部 `shutdown` 与内部 `closing`（drain 期满）合起来算"该停了"。worker
//! 每轮开头看一眼，命中就把手上与队列里的连接各回一个 503 + close ——
//! 静默断连时客户端只看到 "empty reply"，会当成故障。
//!
//! ## 连接借调
//!
//! 静态按连接数均分 ≠ 负载均分。50 条连接各自速率不同，实测两个 worker 的
//! CPU 能差 24%。这里按**累计处理请求数**周期性地把一半连接交给最闲的同行。

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::runtime::context::Context;
use crate::server::conn::{Conn, ConnOutcome, Limits};
use crate::server::{clock, mn, poll, profile};
use crate::web::route::Router;

use super::{ContextFactory, WorkerExitHook};

/// worker 等连接就绪的兜底超时。**正常情况下 worker 会被 `Waker` 立刻叫醒**，
/// 这个值只在"唤醒链路漏了一处"时兜底，防止 worker 永久卡在 `poll` 里关不掉。
const WORKER_IDLE_MS: i32 = 500;
/// 连接借调的检查周期。太短会让 worker 频繁搬连接（迁移有成本），
/// 太长则失衡要持续一段时间才被纠正。
const REBALANCE_EVERY: Duration = Duration::from_millis(200);
/// 手上至少这么多连接才考虑借调。2 条时搬走一半只会两边乒乓，3 条起才有意义。
const REBALANCE_MIN_CONNS: usize = 3;
/// 自己累计处理量超过同行的这个百分比才触发借调。130 = "多干 30%"。
const REBALANCE_TRIGGER_PCT: usize = 130;
/// 每条 worker 队列的容量。满了就当场 503，而不是无限排队。
const QUEUE_CAP: usize = 256;

/// 一个 worker 需要、且只能从主线程传进来的全部东西。
///
/// 打包成结构体而不是二十个参数：worker 起得对不对，取决于这些字段是否
/// **全部**齐备（尤其 sender/waker 必须先在 spawn 之前全建好，因为借调
/// 时要能往**别人**的队列里塞连接）。
pub(crate) struct Shared {
    pub workers: usize,
    pub router: Arc<Router>,
    pub factory: ContextFactory,
    pub limits: Limits,
    pub shutdown: Arc<AtomicBool>,
    pub closing: Arc<AtomicBool>,
    pub on_exit: Option<WorkerExitHook>,
    pub senders: Arc<Vec<SyncSender<Conn>>>,
    pub wakers: Arc<Vec<poll::Waker>>,
    pub served: Arc<Vec<AtomicU64>>,
    pub rt: Arc<mn::sched::Runtime>,
    pub rebalance_stats: Option<Arc<AtomicUsize>>,
}

/// 建队列与唤醒器。**先全建好再 spawn**：借调要求每个 worker 手里都握着
/// 全部 sender/waker。
pub(crate) type Channels = (
    Vec<SyncSender<Conn>>,
    Vec<Option<Receiver<Conn>>>,
    Vec<poll::Waker>,
);

pub(crate) fn channels(workers: usize) -> Result<Channels> {
    let mut senders = Vec::with_capacity(workers);
    let mut receivers = Vec::with_capacity(workers);
    let mut wakers = Vec::with_capacity(workers);
    for _ in 0..workers {
        let (tx, rx) = sync_channel::<Conn>(QUEUE_CAP);
        senders.push(tx);
        receivers.push(Some(rx));
        wakers.push(
            poll::Waker::new().map_err(|e| Error::Lifecycle(format!("创建唤醒器失败：{e}")))?,
        );
    }
    Ok((senders, receivers, wakers))
}

/// 逐个起 worker。每个线程自己调一次工厂建 `Context`（`Session` 是 `!Send`）。
///
/// `ready_tx` 按值进来，函数结束时自动 drop —— 这正是握手循环的终止条件
/// （所有 sender 都没了还没等到齐，说明有 worker 在握手前就退了）。
pub(crate) fn spawn_all(
    shared: Arc<Shared>,
    receivers: &mut [Option<Receiver<Conn>>],
    ready_tx: std::sync::mpsc::Sender<std::result::Result<(), String>>,
) -> Result<Vec<std::thread::JoinHandle<()>>> {
    let mut handles = Vec::with_capacity(shared.workers);
    for i in 0..shared.workers {
        let rx = receivers[i]
            .take()
            .expect("每个 worker 的接收端只取一次");
        let sh = Arc::clone(&shared);
        let my_waker = shared.wakers[i].clone();
        let ready = ready_tx.clone();
        let handle = std::thread::Builder::new()
            .name(format!("rsboot-w{i}"))
            .spawn(move || main_loop(i, rx, my_waker, sh, ready))
            .map_err(|e| Error::Lifecycle(format!("启动 worker {i} 失败：{e}")))?;
        handles.push(handle);
    }
    Ok(handles)
}

fn main_loop(
    id: usize,
    rx: Receiver<Conn>,
    my_waker: poll::Waker,
    sh: Arc<Shared>,
    ready: std::sync::mpsc::Sender<std::result::Result<(), String>>,
) {
    let ctx = match (sh.factory)() {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("worker {id} 初始化失败：{e}");
            eprintln!("[rsboot] {msg}");
            let _ = ready.send(Err(msg));
            return;
        }
    };
    if ready.send(Ok(())).is_err() {
        // 主线程已经放弃等待（另一个 worker 失败了），这个也别干等了
        if let Some(hook) = &sh.on_exit {
            hook(&ctx);
        }
        return;
    }

    // 接上协程调度器：这个 worker 线程从此可以在 I/O 等待处让出。
    // 定时器线程只有一个（进程级），`start_timer` 内部做了幂等。
    mn::sched::init(Arc::clone(&sh.rt), id, mn::sched::Shared::new(my_waker.clone()));
    // 处理完、仍可 keep-alive 的连接由协程投回这里，主循环统一收回
    let rets: Arc<std::sync::Mutex<Vec<Conn>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    // 任一来源要求关闭都算：外部编排，或 drain 期满的内部关闭
    let should_stop = || {
        sh.shutdown.load(Ordering::Relaxed) || sh.closing.load(Ordering::Relaxed)
    };

    let mut conns: Vec<Conn> = Vec::new();
    // 常驻的 poll 集合：fd 数组与就绪缓冲只分配一次
    let mut ps = poll::PollSet::with_waker(my_waker.read_fd());
    let mut last_rebalance = Instant::now();
    // 借调冷却：刚被塞过连接的 worker 先别又把它们送回去，
    // 否则两个 worker 会互相推诿，连接在队列里空转
    let mut stole_at = Instant::now()
        .checked_sub(REBALANCE_EVERY)
        .unwrap_or_else(Instant::now);

    loop {
        let round_t0 = clock::now_ns();

        // 关闭必须能立刻生效：worker 自己握着 sender，
        // 光靠 "主线程 drop senders" 永远收不到 Disconnected。
        if should_stop() {
            flush_and_stop(&mut conns, &rx, &sh, &ctx, &should_stop);
            break;
        }

        // 推进协程：让出的请求在这里接着跑。worker 只有这一处会切进
        // 协程栈，别的地方都是纯事件循环。
        let mut mark = round_t0;
        mn::sched::pump(4096);
        mark = {
            let n = clock::now_ns();
            profile::note_prep(0, mark, n);
            n
        };
        {
            let mut g = rets.lock().unwrap();
            if !g.is_empty() {
                conns.extend(g.drain(..));
            }
        }
        mark = {
            let n = clock::now_ns();
            profile::note_prep(1, mark, n);
            n
        };

        // 先把新派来的连接收进本地表
        let mut got_from_peer = false;
        while let Ok(c) = rx.try_recv() {
            conns.push(c);
            got_from_peer = true;
        }
        if got_from_peer {
            stole_at = Instant::now();
        }
        mark = {
            let n = clock::now_ns();
            profile::note_prep(2, mark, n);
            n
        };

        maybe_rebalance(id, &mut conns, &sh, &mut last_rebalance, stole_at);
        mark = {
            let n = clock::now_ns();
            profile::note_prep(3, mark, n);
            n
        };

        // 拼出本轮要等的 fd（唤醒器恒在 items[0]），然后真阻塞。
        // 兜底超时只防"唤醒漏了"，正常路径由 `Waker` 立刻叫醒：
        // 派发方 push 进队列之后才 notify，所以醒来必能 try_recv 到。
        //
        // 协程在等哪个 socket，就把它一并交给 `poll`——这是让
        // "DB/下游调用"也能让出的全部机制。`n_conns` 记下分界点：
        // 就绪下标小于它是连接，大于等于它是等待表里的 fd。
        let n_conns = conns.len();
        let now = clock::now_ns();
        mn::sched::expire_waits(now);
        let wait_fds = mn::sched::wait_fds();
        ps.reset_conns_with(
            conns.iter().filter_map(|c| c.raw_fd()),
            wait_fds.into_iter(),
        );
        // 队列里还有待跑的协程就立刻回来继续推进，别在 poll 里等
        let idle = if mn::sched::pending() > 0 {
            0
        } else {
            WORKER_IDLE_MS
        };
        // 有人等着超时的话，poll 不能睡到 500ms——那会让它的截止时刻迟到
        let cap = mn::sched::wait_deadline().map(|d| (d.saturating_sub(now) / 1_000_000) as i32);
        let timeout = cap.map_or(idle, |c| c.clamp(0, idle));
        // 量一下阻塞真正的占比：若 `poll` 只占个位数，换 `epoll` 就纯属
        // 白费——那点内核扫描不是差距来源
        let poll_t0 = clock::now_ns();
        profile::note_prep(profile::PREP_LAST, mark, poll_t0);
        if let Err(e) = ps.wait(timeout) {
            eprintln!("[rsboot] worker poll 失败：{e}");
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        profile::note_poll(round_t0, poll_t0);
        if ps.woken() {
            // 不清零的话下一轮 poll 会立刻又返回，变成忙等
            my_waker.drain();
        }

        service_ready(id, &mut conns, &ps, n_conns, &sh, &ctx, &rets);
    }

    // worker 收尾：模块在这里释放线程局部资源（连接、缓冲）
    if let Some(hook) = &sh.on_exit {
        hook(&ctx);
    }
}

/// 关闭时给手上与队列里的连接各自一个明确答复，而不是静默断连。
fn flush_and_stop<F: Fn() -> bool>(
    conns: &mut Vec<Conn>,
    rx: &Receiver<Conn>,
    sh: &Shared,
    ctx: &Context,
    should_stop: &F,
) {
    for mut c in conns.drain(..) {
        let _ = c.serve_once(&sh.router, ctx, &sh.limits, should_stop);
    }
    while let Ok(mut c) = rx.try_recv() {
        let _ = c.serve_once(&sh.router, ctx, &sh.limits, should_stop);
    }
}

/// 按**累计处理请求数**做周期性再平衡：干得比同行多 30% 就把一半连接交出去，
/// 交给累计最少的那个。
fn maybe_rebalance(
    id: usize,
    conns: &mut Vec<Conn>,
    sh: &Shared,
    last_rebalance: &mut Instant,
    stole_at: Instant,
) {
    if last_rebalance.elapsed() < REBALANCE_EVERY
        || stole_at.elapsed() < REBALANCE_EVERY
        || conns.len() < REBALANCE_MIN_CONNS
        || sh.workers <= 1
    {
        return;
    }
    *last_rebalance = Instant::now();
    let mine = sh.served[id].load(Ordering::Relaxed);
    let total: u64 = sh.served.iter().map(|c| c.load(Ordering::Relaxed)).sum();
    let avg = total / sh.workers as u64;
    // 注意放大倍数：`mine * 100 > avg * PCT` 才是"多 30%"。
    // （写成 10 会变成"多 12 倍"，是测试逼出来的真 bug）
    if mine * 100 <= avg * (REBALANCE_TRIGGER_PCT as u64) {
        return;
    }
    // 交给累计最少的同行（不含自己）
    let target = (0..sh.workers)
        .filter(|&j| j != id)
        .min_by_key(|&j| sh.served[j].load(Ordering::Relaxed))
        .expect("workers > 1 时必然有别人");
    let give = conns.len() / 2;
    let mut moved = 0usize;
    for _ in 0..give {
        let Some(c) = conns.pop() else { break };
        match sh.senders[target].try_send(c) {
            Ok(()) => moved += 1,
            Err(TrySendError::Full(c)) | Err(TrySendError::Disconnected(c)) => {
                // 对方队列满/已退：放回自己手里，别再折腾
                conns.push(c);
                break;
            }
        }
    }
    if moved > 0 {
        if let Some(h) = &sh.rebalance_stats {
            h.fetch_add(moved, Ordering::Relaxed);
        }
        // 先入队后唤醒，和 accept 路径同一个顺序约束
        sh.wakers[target].notify();
        // 记一笔"我欠对方"，避免下一轮又立刻判定自己超载
        sh.served[id].store(
            sh.served[id].load(Ordering::Relaxed) / 2,
            Ordering::Relaxed,
        );
    }
}

/// 只处理就绪的连接。`swap_remove` + 处理 + 放回：O(1)。
///
/// 必须**按下标降序**处理。`swap_remove(i)` 会把尾部元素搬到 i，`push` 又会
/// 改变长度；若按升序处理，后续的 `ready[j]`（j > i）就指向了别的连接——
/// 表现为"同一条连接被处理两次、另一条就绪的延后一轮"。更糟的是被重复
/// 处理的那条此刻可能已无数据，`serve_once` 会阻塞到读超时，把 worker 卡住。
/// `ready` 天然升序，所以从后往前遍历即可，无需排序。
fn service_ready(
    id: usize,
    conns: &mut Vec<Conn>,
    ps: &poll::PollSet,
    n_conns: usize,
    sh: &Arc<Shared>,
    ctx: &Context,
    rets: &Arc<std::sync::Mutex<Vec<Conn>>>,
) {
    for k in (0..ps.ready_len()).rev() {
        let idx = ps.ready_at(k);
        if idx >= n_conns {
            // 等待表里的 fd 就绪：把那个协程放回队列。
            // 必须降序（同上）：`wake_wait` 内部也用 `swap_remove`。
            mn::sched::wake_wait(idx - n_conns);
            continue;
        }
        if idx >= conns.len() {
            continue;
        }
        let mut c = conns.swap_remove(idx);
        sh.served[id].fetch_add(1, Ordering::Relaxed);
        // 请求处理放进协程：handler 里凡是要等 I/O 的地方（框架的 sleep /
        // DB / 下游调用）都能让出，worker 立刻回去处理别的连接。
        // 同步 handler 的签名和借用规则不变。
        if mn::sched::ready() && !no_coro() {
            // SAFETY: `sh` 与 `ctx` 活到 worker 线程结束，协程只在这个
            // worker 的 `pump` 里跑，不会比它们活得久。
            let rp: *const Router = &*sh.router;
            let cp: *const Context = ctx;
            let sh2 = Arc::clone(sh);
            let lim = sh.limits;
            let rets2 = Arc::clone(rets);
            mn::sched::spawn(move || {
                let stop_fn = || {
                    sh2.shutdown.load(Ordering::Relaxed) || sh2.closing.load(Ordering::Relaxed)
                };
                // 每处理一个请求就交回队列：这是**刻意**的。试过在协程里连跑
                // 8 个请求，吞吐掉 12%——单连接占住 worker 期间，别的就绪连接
                // 只能等。
                match unsafe { c.serve_once(&*rp, &*cp, &lim, &stop_fn) } {
                    Ok(ConnOutcome::KeepAlive) | Ok(ConnOutcome::Idle) => {
                        rets2.lock().unwrap().push(c);
                    }
                    _ => {}
                }
            });
        } else if let Ok(ConnOutcome::KeepAlive) =
            c.serve_once(&sh.router, ctx, &sh.limits, &|| {
                sh.shutdown.load(Ordering::Relaxed) || sh.closing.load(Ordering::Relaxed)
            })
        {
            conns.push(c);
        }
        // 关闭/出错/对端要 close 的一律直接丢
    }
}

/// 仅用于测量：关掉协程，让请求在 worker 栈上同步跑。
/// 目的不是上线，是量出"每请求一次 spawn"到底值多少时间。
fn no_coro() -> bool {
    use std::sync::OnceLock;
    static F: OnceLock<bool> = OnceLock::new();
    *F.get_or_init(|| std::env::var_os("RSBOOT_NO_CORO").is_some())
}
