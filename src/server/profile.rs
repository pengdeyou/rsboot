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

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

pub const READ: usize = 0;
pub const PARSE: usize = 1;
pub const BUILD: usize = 2;
pub const HANDLE: usize = 3;
pub const WRITE: usize = 4;
const STAGES: usize = 5;

#[inline(always)]
fn tsc() -> u64 {
    crate::server::clock::now_ns()
}

#[inline]
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

/// 每 5000 个请求打一行，然后清零重新累计。
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
            eprintln!(
                "[profile] {} req | read {:.0}  parse {:.0}  build {:.0}  handle {:.0}  write {:.0}  = {:.0} ns",
                n,
                per(READ),
                per(PARSE),
                per(BUILD),
                per(HANDLE),
                per(WRITE),
                per(READ) + per(PARSE) + per(BUILD) + per(HANDLE) + per(WRITE)
            );
            acc.set([0; STAGES]);
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
