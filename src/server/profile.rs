//! 分阶段计时：只在 `RSBOOT_PROFILE=1` 时开启，用来回答"时间到底花在哪"。
//!
//! 这台 VM 上一次 `Instant::now()` 要 **1.8 µs**（vDSO 没生效），
//! 拿它打点，五个阶段光计时钟就 18 µs——测出来的全是测量开销。
//! 所以这里直接用 `rdtsc`（十几个周期），启动时校准一次频率。
//!
//! 用法：
//! ```text
//! RSBOOT_PROFILE=1 ./rsboot-serve &
//! ./load 127.0.0.1:9001 /json 20 3
//! ```

// `thread_local!` 的块在 feature 关闭时依然要能编译，所以这几个 import 不按 feature 裁
#[allow(unused_imports)]
use std::cell::Cell;
#[allow(unused_imports)]
use std::sync::atomic::{AtomicU64, Ordering};
#[allow(unused_imports)]
use std::sync::OnceLock;

pub const READ: usize = 0;
pub const PARSE: usize = 1;
pub const BUILD: usize = 2;
pub const HANDLE: usize = 3;
pub const WRITE: usize = 4;
/// `WRITE` 里再拆两层：组头 / 真正写出去。短连接下这两者的比例很不一样。
pub const HEAD: usize = 5;
pub const FLUSH: usize = 6;

// `HANDLE` 是七段里最模糊的一段：它把中间件、路由匹配、用户 handler、出口收尾
// 全包在一起，只给一个 ~555 ns 的总数。3% 的差距 ≈ 150 ns，落在这四者中的哪一个
// 完全看不出来，所以历史上所有"应该会快"的改动都只能盲改。
// 这四层只用于定位，**不参与七段合计**（`HL_MATCH` 包含 `HL_USER`，直接相加会重复）。
pub const HL_MW: usize = 7;
pub const HL_MATCH: usize = 8;
pub const HL_USER: usize = 9;
pub const HL_END: usize = 10;
const STAGES: usize = 11;

#[inline(always)]
fn tsc() -> u64 {
    // profile 要的是精确差值，不能读请求级的缓存（那个 64 次才刷新一次）
    crate::server::clock::raw_ns()
}

#[inline]
#[cfg(feature = "prof")]
pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("RSBOOT_PROFILE").is_some())
}

thread_local! {
    static ACC: Cell<[u64; STAGES]> = const { Cell::new([0; STAGES]) };
    static COUNT: Cell<u64> = const { Cell::new(0) };
    /// 只有被抽到的那 1/64 个请求才计时。
    /// 每请求六段计时的开销足以把 QPS 砍掉一半，测出来的分布就是假的。
    static SAMPLE: Cell<bool> = const { Cell::new(false) };
    static SEQ: Cell<u64> = const { Cell::new(0) };
}

/// 每个请求开头调用：按 1/64 采样决定这一条要不要计时。
#[cfg(feature = "prof")]
pub fn pick() {
    if !on() {
        return;
    }
    let n = SEQ.with(|c| {
        let n = c.get() + 1;
        c.set(n);
        n
    });
    SAMPLE.with(|c| c.set(n % 64 == 0));
}

/// 一个被计时的区间；离开作用域时把耗时（纳秒）累加进当前 worker 的计数器。
pub struct Span(u64, usize);

#[inline]
#[cfg(feature = "prof")]
pub fn start(stage: usize) -> Option<Span> {
    if on() && SAMPLE.with(|c| c.get()) {
        Some(Span(tsc(), stage))
    } else {
        None
    }
}

impl Drop for Span {
    #[inline]
    fn drop(&mut self) {
        let d = tsc().saturating_sub(self.0);
        ACC.with(|acc| {
            let mut v = acc.get();
            v[self.1] += d;
            acc.set(v);
        });
    }
}

// 写系统调用的次数与其中 EAGAIN 的次数（只统计被采样的请求）。
//
// `flush` 一段 14–17 µs 到底是内核慢，还是"EAGAIN → 让出 → 等下一轮"？
// 只有把次数拆出来才能判断：每请求 1 次且 0 次 EAGAIN，就是内核成本；
// 出现多次 EAGAIN，就是让出策略的账。
//
// 必须是线程局部：每个 worker 有自己的采样窗口，进程级共享会让"写次数/请求数"
// 变成两个 worker 互相污染的数字（实测出现 1.87 与 0.20 并存的假象）。
thread_local! {
    static WRITE_CALLS: Cell<u64> = const { Cell::new(0) };
    static WRITE_EAGAIN: Cell<u64> = const { Cell::new(0) };
}

#[inline]
#[cfg(feature = "prof")]
pub fn note_write(eagain: bool) {
    if !on() || !SAMPLE.with(|c| c.get()) {
        return;
    }
    WRITE_CALLS.with(|c| c.set(c.get() + 1));
    if eagain {
        WRITE_EAGAIN.with(|c| c.set(c.get() + 1));
    }
}

/// 每 5000 个请求打一行，然后清零重新累计。
#[cfg(feature = "prof")]
pub fn finish_request() {
    if !on() || !SAMPLE.with(|c| c.get()) {
        return;
    }
    COUNT.with(|c| {
        let n = c.get() + 1;
        if n < 5000 {
            c.set(n);
            return;
        }
        ACC.with(|acc| {
            let v = acc.get();
            let per = |i: usize| v[i] as f64 / n as f64;
            let wc = WRITE_CALLS.with(|c| c.get());
            let we = WRITE_EAGAIN.with(|c| c.get());
            // `匹配` 要减掉 `用户`：`HL_MATCH` 的区间包住 handler 调用，
            // 不减就是同一段时间数了两遍。
            let user = per(HL_USER);
            let matched = (per(HL_MATCH) - user).max(0.0);
            eprintln!(
                "[profile] {} req | read {:.0}  parse {:.0}  build {:.0}  handle {:.0}  write {:.0} (head {:.0} + flush {:.0})  = {:.0} ns  wr {:.2}/req eagain {:.2}",
                n,
                per(READ),
                per(PARSE),
                per(BUILD),
                per(HANDLE),
                per(WRITE),
                per(HEAD),
                per(FLUSH),
                per(READ) + per(PARSE) + per(BUILD) + per(HANDLE) + per(WRITE),
                wc as f64 / n as f64,
                we as f64 / n as f64
            );
            eprintln!(
                "[handle] 中间件 {:.0}  匹配 {:.0}  用户 handler {:.0}  收尾 {:.0}  = {:.0} ns",
                per(HL_MW),
                matched,
                user,
                per(HL_END),
                per(HL_MW) + matched + user + per(HL_END)
            );
            acc.set([0; STAGES]);
            WRITE_CALLS.with(|c| c.set(0));
            WRITE_EAGAIN.with(|c| c.set(0));
        });
        c.set(0);
    });
}

// ---- poll 阻塞占比：worker 级统计，不参与 per-request 累计 ----
static POLL_NS: AtomicU64 = AtomicU64::new(0);
static ROUND_NS: AtomicU64 = AtomicU64::new(0);
static POLL_CNT: AtomicU64 = AtomicU64::new(0);

/// `round_t0` 是本轮进 worker 循环的时刻，`poll_t0` 是进 `poll` 的时刻。
/// `poll_t0..now` 是真正阻塞的时间，`round_t0..now` 是本轮总耗时。
#[cfg(feature = "prof")]
pub fn note_poll(round_t0: u64, poll_t0: u64) {
    if !on() {
        return;
    }
    let now = super::clock::now_ns();
    let wait = now.saturating_sub(poll_t0);
    let round = now.saturating_sub(round_t0).max(wait);
    let w = POLL_NS.fetch_add(wait, Ordering::Relaxed) + wait;
    let r = ROUND_NS.fetch_add(round, Ordering::Relaxed) + round;
    let n = POLL_CNT.fetch_add(1, Ordering::Relaxed) + 1;
    if n % 2000 == 0 {
        eprintln!(
            "[poll] {n} 次 | 平均等待 {:.0} ns | 平均本轮 {:.0} ns | 阻塞占 {:.1}%",
            w as f64 / n as f64,
            r as f64 / n as f64,
            100.0 * w as f64 / r as f64
        );
    }
}

// ---- 轮内分段：poll 之前那段"准备工作"到底花在哪 ----
//
// `note_poll` 只能给出"等待 vs 本轮总长"，本轮总长减去等待就是
// poll **之前**做的事（收队列、调度器查询、rebalance 检查、拼 fd 数组）。
// 短连接下一轮只服务一条连接，那段准备成本会被整条连接摊走——必须细分。
pub const PREP_PUMP: usize = 0;
pub const PREP_DRAIN: usize = 1;
pub const PREP_RECV: usize = 2;
pub const PREP_REBAL: usize = 3;
pub const PREP_RESET: usize = 4;
/// `poll` 之后取出连接、spawn 协程的那一段。过去它没被计时，
/// 于是"七段合计"与"本轮总长"之间有一块黑洞。
pub const PREP_SERVE: usize = 5;
pub const PREP_STAGES: usize = 6;
/// 最后一段的下标：`note_prep` 收到它就顺手打一行。
pub const PREP_LAST: usize = PREP_STAGES - 1;
pub const PREP_NAMES: [&str; PREP_STAGES] = [
    "pump",
    "drain",
    "recv",
    "rebalance",
    "reset",
    "serve",
];

static PREP_NS: [AtomicU64; PREP_STAGES] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];
/// 本轮处理了多少条连接（= spawn 了几次）。用来把"每轮成本"换算成"每请求成本"。
static SERVED: AtomicU64 = AtomicU64::new(0);
static PREP_CNT: AtomicU64 = AtomicU64::new(0);

/// 每轮 `pump` 跑了多少步，用来把 pump 段摊到每步上。
static PUMP_STEPS: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "prof")]
pub fn note_pump_steps(n: usize) {
    if on() {
        PUMP_STEPS.fetch_add(n as u64, Ordering::Relaxed);
    }
}

/// 记一段：`from..to` 属于第 `stage` 段。最后一段调用时打一行。
#[inline]
#[cfg(feature = "prof")]
pub fn note_prep(stage: usize, from: u64, to: u64) {
    if !on() {
        return;
    }
    PREP_NS[stage].fetch_add(to.saturating_sub(from), Ordering::Relaxed);
    if stage + 1 == PREP_STAGES {
        let n = PREP_CNT.fetch_add(1, Ordering::Relaxed) + 1;
        if n % 2000 == 0 {
            let mut s = String::with_capacity(120);
            let mut sum = 0.0;
            for (i, name) in PREP_NAMES.iter().enumerate() {
                let v = PREP_NS[i].load(Ordering::Relaxed) as f64 / n as f64;
                sum += v;
                s.push_str(&format!("{name} {v:.0}  "));
            }
            let wait = POLL_NS.load(Ordering::Relaxed) as f64 / POLL_CNT.load(Ordering::Relaxed).max(1) as f64;
            let round = ROUND_NS.load(Ordering::Relaxed) as f64 / POLL_CNT.load(Ordering::Relaxed).max(1) as f64;
            let served = SERVED.load(Ordering::Relaxed) as f64 / n as f64;
            let steps = PUMP_STEPS.load(Ordering::Relaxed) as f64 / SERVED.load(Ordering::Relaxed).max(1) as f64;
            // pump 段每步的净开销：pump 段里还含着被它 resume 的协程服务时间，
            // 把那部分（七段合计）减掉才是调度本身的成本。
            let pstep = if steps > 0.0 {
                let pump_ns = PREP_NS[0].load(Ordering::Relaxed) as f64 / SERVED.load(Ordering::Relaxed).max(1) as f64 / steps - 2600.0;
                pump_ns.max(0.0)
            } else {
                0.0
            };
            eprintln!(
                "[prep] {n} 轮 | {s}| wait {wait:.0}  = 已计 {:.0}  / 本轮 {round:.0}  | 差 {:.0}  | 每轮 {served:.1} 条 | 步/请求 {steps:.2}  pump步 {:.0}ns",
                sum, round - sum - wait, pstep
            );
        }
    }
}

/// 本轮 `service_ready` 服务了几条连接。
#[inline]
#[cfg(feature = "prof")]
pub fn note_served(n: usize) {
    if on() {
        SERVED.fetch_add(n as u64, Ordering::Relaxed);
    }
}

/// 未启用 `prof` feature 时的一组空实现。
///
/// 探针按请求要过七八次 `on()`（每次是一个 `OnceLock` 原子读 + 分支），
/// 加起来约 10–15 ns；而它只在排查时有意义。让它在编译期整个消失，
/// 默认构建的热路径上一个字节都不留——要计时就
/// `cargo build --features rsboot/prof`。
#[cfg(not(feature = "prof"))]
mod off {
    use super::Span;

    #[inline(always)]
    pub fn on() -> bool {
        false
    }
    #[inline(always)]
    pub fn pick() {}
    #[inline(always)]
    pub fn start(_stage: usize) -> Option<Span> {
        None
    }
    #[inline(always)]
    pub fn note_write(_eagain: bool) {}
    #[inline(always)]
    pub fn finish_request() {}
    #[inline(always)]
    pub fn note_poll(_round_t0: u64, _poll_t0: u64) {}
    #[inline(always)]
    pub fn note_pump_steps(_n: usize) {}
    #[inline(always)]
    pub fn note_prep(_stage: usize, _from: u64, _to: u64) {}
    #[inline(always)]
    pub fn note_served(_n: usize) {}
}

#[cfg(not(feature = "prof"))]
pub use off::*;
