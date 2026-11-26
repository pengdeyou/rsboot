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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Once;
use std::time::{Duration, Instant};

/// 每微秒的 TSC 刻度数。用微秒而不是纳秒做分母：2.5 GHz 的机器上
/// `ticks/ns` 会截断成 2（20% 误差），`ticks/us` 是 2500，精度够用。
static TICKS_PER_US: AtomicU64 = AtomicU64::new(1);
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
    }
    let _ = *START.get_or_init(Instant::now);
}

/// 单调递增的纳秒计数（进程内基准，不是墙钟）。
#[inline]
pub fn now_ns() -> u64 {
    INIT.call_once(calibrate);
    #[cfg(target_arch = "x86_64")]
    {
        let per_us = TICKS_PER_US.load(Ordering::Relaxed).max(1);
        // 1000 是 ns→us 的倒数换算；u64 装得下，几百年才溢出
        tsc().saturating_mul(1000) / per_us
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        // 没有 TSC 就退回 `Instant`，但必须相对同一个起点，
        // 不能每次 `Instant::now().elapsed()`——那永远是 0
        START.get_or_init(|| Instant::now()).elapsed().as_nanos() as u64
    }
}

/// 毫秒换算，给 `poll` 超时用。
#[inline]
pub fn ms_since(then_ns: u64, now: u64) -> u64 {
    now.saturating_sub(then_ns) / 1_000_000
}
