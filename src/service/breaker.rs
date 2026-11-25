//! 熔断器：下游连续失败到阈值后，直接拒绝，不再把调用方拖死。
//!
//! ## 为什么必须有
//!
//! 下游已经挂了的时候，"每个请求都去试一次"会带来三个后果：
//! 1. 每个请求都白等一个超时（把本服务的线程也占住，故障扩散）；
//! 2. 下游恢复瞬间被一堆排队请求砸死（雪崩）；
//! 3. 日志被同一个错误刷屏，掩盖真正的根因。
//!
//! 熔断把"连续失败"这件事本身当成信号：够了就别再试。
//!
//! ## half-open 的简化
//!
//! 标准实现是 open → half-open（放一个探针）→ 成功则 closed。这里简化为
//! "冷却期满后清零并放行"：放行后如果又失败，计数会立刻回到阈值以上再开。
//! 少了"只放一个探针"的精细度，但省掉一整套状态机与锁，行为对调用方等价。

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub struct Breaker {
    threshold: u32,
    cooldown: Duration,
    failures: AtomicU32,
    /// 打开到什么时候（用进程启动起的毫秒数表示，避免存 `Instant` 到原子量里）
    opened_until_ms: AtomicU64,
    started: Instant,
}

impl Breaker {
    pub fn new(threshold: u32, cooldown: Duration) -> Self {
        Self {
            // 0 或 1 的阈值会让正常抖动就熔断，抬到 2
            threshold: threshold.max(2),
            cooldown,
            failures: AtomicU32::new(0),
            opened_until_ms: AtomicU64::new(0),
            started: Instant::now(),
        }
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// 现在能不能发？`false` 表示熔断中。
    pub fn allow(&self) -> bool {
        let until = self.opened_until_ms.load(Ordering::Acquire);
        if until == 0 {
            return true;
        }
        if self.now_ms() < until {
            return false;
        }
        // 冷却期满：清零放行，让它重新证明自己
        self.failures.store(0, Ordering::Release);
        self.opened_until_ms.store(0, Ordering::Release);
        true
    }

    pub fn on_success(&self) {
        self.failures.store(0, Ordering::Release);
        self.opened_until_ms.store(0, Ordering::Release);
    }

    pub fn on_failure(&self) {
        let n = self.failures.fetch_add(1, Ordering::AcqRel) + 1;
        if n >= self.threshold {
            let until = self.now_ms() + self.cooldown.as_millis() as u64;
            self.opened_until_ms.store(until, Ordering::Release);
        }
    }

    pub fn is_open(&self) -> bool {
        !self.allow()
    }

    pub fn failures(&self) -> u32 {
        self.failures.load(Ordering::Acquire)
    }
}

impl std::fmt::Debug for Breaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Breaker")
            .field("threshold", &self.threshold)
            .field("cooldown", &self.cooldown)
            .field("failures", &self.failures())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_after_threshold_and_recovers_after_cooldown() {
        let b = Breaker::new(3, Duration::from_millis(30));
        assert!(b.allow());
        b.on_failure();
        b.on_failure();
        assert!(b.allow(), "两次失败还不够");

        b.on_failure();
        assert!(!b.allow(), "第三次应熔断");

        std::thread::sleep(Duration::from_millis(40));
        assert!(b.allow(), "冷却期满应放行");
    }

    #[test]
    fn success_resets_counter() {
        let b = Breaker::new(3, Duration::from_secs(1));
        b.on_failure();
        b.on_failure();
        b.on_success();
        b.on_failure();
        assert!(b.allow(), "成功之后计数应归零，两次失败不该熔断");
    }

    #[test]
    fn threshold_below_two_is_raised() {
        let b = Breaker::new(0, Duration::from_secs(1));
        assert_eq!(b.threshold, 2);
    }
}
