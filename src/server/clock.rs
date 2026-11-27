//! 便宜的单调时钟。
//!
//! 这台 VM 上 `Instant::now()` 要 **1.8 µs**（vDSO 没生效，走的是系统调用），
//! 而 worker 主循环每轮、协程每次挂起都要看时间。用 `Instant` 等于每轮白扔
//! 几微秒——实测就是被这笔开销拖慢了 12%。
//!
//! 这里用 `rdtsc`：十几个周期，换算成纳秒。只在启动时校准一次频率。
//!
//! 注意：TSC 在虚拟机里可能被 host 迁移/降频影响，所以它只用于**超时判定**
//! 这类"差几毫秒无所谓"的场合，不能拿来当日志时间戳或业务时间。

use std::cell::Cell;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Once;
use std::time::{Duration, Instant};

/// 每微秒的 TSC 刻度数。用微秒而不是纳秒做分母：2.5 GHz 的机器上
/// `ticks/ns` 会截断成 2（20% 误差），`ticks/us` 是 2500，精度够用。
static TICKS_PER_US: AtomicU64 = AtomicU64::new(1);
/// 这台机器上 `rdtsc` 到底走不走。KVM 里被 host 迁移或关了 TSC scaling 时，
/// `rdtsc` 会返回**恒定值**——校准能当场看出来（20 ms 过去刻度没动）。
/// 那时所有"按时间差判定"的逻辑都会静默失效：超时不触发、idle 连接不清、
/// 协程的截止点永远不到。所以必须显式降级，不能假设 TSC 一定在走。
static TSC_OK: AtomicU8 = AtomicU8::new(0);
/// TSC 跑着跑着停住（KVM 里见过：20 ms 校准窗口内它在走，之后整段冻结，
/// 2000 万次忙等也一动不动）。所以校准通过不代表一直可用，运行期还要盯着。
static TSC_DEAD: AtomicU8 = AtomicU8::new(0);
static TSC_LAST_NS: AtomicU64 = AtomicU64::new(0);
static TSC_STRIKES: AtomicU32 = AtomicU32::new(0);
static INIT: Once = Once::new();
static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

#[inline(always)]
fn tsc() -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: rdtsc 只读时间戳计数器，无副作用
        unsafe { core::arch::x86_64::_rdtsc() }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        0
    }
}

fn calibrate() {
    let t = Instant::now();
    let c0 = tsc();
    std::thread::sleep(Duration::from_millis(20));
    let c1 = tsc();
    let us = (t.elapsed().as_nanos() as u64) / 1000;
    if c1 > c0 && us > 0 {
        TICKS_PER_US.store(((c1 - c0) / us).max(1), Ordering::Relaxed);
        TSC_OK.store(1, Ordering::Relaxed);
    }
    let _ = *START.get_or_init(Instant::now);
}

/// 精确但贵的"现在"：TSC 在走就用 TSC，不动了就用 `Instant`（单次约 1.8 µs）。
/// 只给 `profile` 这种"量一次就不想再量"的地方用。
pub fn raw_ns() -> u64 {
    INIT.call_once(calibrate);
    #[cfg(target_arch = "x86_64")]
    {
        if TSC_OK.load(Ordering::Relaxed) == 1 && TSC_DEAD.load(Ordering::Relaxed) == 0 {
            let per_us = TICKS_PER_US.load(Ordering::Relaxed).max(1);
            // 1000 是 ns→us 的倒数换算；u64 装得下，几百年才溢出
            let ns = tsc().saturating_mul(1000) / per_us;
            // 比的是**换算后的纳秒**，不是原始刻度：冻结时 rdtsc 仍会微跳几个 tick，
            // 换算成 ns 后完全不变，只看刻度就会以为它还在走。
            let prev = TSC_LAST_NS.swap(ns, Ordering::Relaxed);
            if ns == prev {
                // 连续 64 次读到的"现在是同一刻"才算冻结（正常 TSC 不会这样）
                if TSC_STRIKES.fetch_add(1, Ordering::Relaxed) > 64 {
                    TSC_DEAD.store(1, Ordering::Relaxed);
                }
            } else {
                TSC_STRIKES.store(0, Ordering::Relaxed);
            }
            return ns;
        }
    }
    // 必须相对同一个起点，不能每次 `Instant::now().elapsed()`——那永远是 0
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

thread_local! {
    /// 请求级看到的"现在"。每 64 次调用才真正取一次时钟，
    /// 其余全读这个缓存——`Instant` 在这台 VM 上要 1.8 µs，一次都省不得。
    /// 精度 64 × 单请求时间（几百 µs），对 20 ms 级的读超时判定够用。
    static NOW_CACHE: Cell<u64> = const { Cell::new(0) };
    static NOW_TICK: Cell<u16> = const { Cell::new(0) };
}

/// 单调递增的纳秒计数（进程内基准，不是墙钟）。请求级用，几乎零成本。
#[inline]
pub fn now_ns() -> u64 {
    NOW_TICK.with(|t| {
        let n = t.get().wrapping_add(1);
        t.set(n);
        let cached = NOW_CACHE.with(|c| c.get());
        if n % 64 == 0 || cached == 0 {
            let v = raw_ns();
            NOW_CACHE.with(|c| c.set(v));
            v
        } else {
            cached
        }
    })
}

/// 诊断：`(TSC_OK, 每微秒刻度, 两次相隔 5ms 的读数)`
#[doc(hidden)]
pub fn clock_probe() -> (u8, u64, u64, u64) {
    let a = raw_ns();
    std::thread::sleep(Duration::from_millis(5));
    let b = raw_ns();
    (
        (TSC_OK.load(Ordering::Relaxed) << 1) | TSC_DEAD.load(Ordering::Relaxed),
        TICKS_PER_US.load(Ordering::Relaxed),
        a,
        b,
    )
}

/// 毫秒换算，给 `poll` 超时用。
#[inline]
pub fn ms_since(then_ns: u64, now: u64) -> u64 {
    now.saturating_sub(then_ns) / 1_000_000
}
