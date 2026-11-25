//! 重试策略。
//!
//! ## 重试是最容易闯祸的"优化"
//!
//! 一个 `POST /pay` 超时后重试，可能让用户被扣两次钱。所以这里的原则是：
//! **只有当"重复执行不会改变结果"时才重试**。判断分三层：
//!
//! 1. 请求有没有真的发出去？连接失败 / 建连超时 → 一定没发出去，任何方法都能重试。
//! 2. 方法是幂等的吗？GET/HEAD/PUT/DELETE/OPTIONS/TRACE → 可以重试。
//! 3. 失败值不值得重试？5xx 与传输层错误值得，4xx 不值得（重试 400 只会再来一个 400）。

use std::time::Duration;

#[derive(Debug)]
pub struct RetryPolicy {
    /// 额外的重试次数（不含首次）。0 = 不重试。
    pub max: u32,
    /// 退避基数，第 n 次重试等 `base * 2^n`（封顶 2s）
    pub base: Duration,
}

impl RetryPolicy {
    pub fn none() -> Self {
        Self {
            max: 0,
            base: Duration::from_millis(0),
        }
    }

    pub fn new(max: u32, base: Duration) -> Self {
        Self { max, base }
    }

    /// 第 `attempt` 次（从 0 起算）重试前睡多久。
    pub fn backoff(&self, attempt: u32) -> Duration {
        let ms = self.base.as_millis() as u64 * (1u64 << attempt.min(6));
        Duration::from_millis(ms.min(2000))
    }

    pub fn is_idempotent(method: &str) -> bool {
        matches!(
            method.to_ascii_uppercase().as_str(),
            "GET" | "HEAD" | "OPTIONS" | "TRACE" | "PUT" | "DELETE"
        )
    }
}

/// 为什么要重试。决定"能不能重试"和"要不要算进熔断"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryReason {
    /// 连不上：请求肯定没送达
    NotSent,
    /// 送达了但没回来（超时 / 断连）：不确定下游有没有执行
    Ambiguous,
    /// 下游明确返回 5xx
    ServerError,
    /// 不该重试（4xx、熔断中、非幂等）
    No,
}

impl RetryPolicy {
    /// 结合方法与失败原因，给一个结论。
    ///
    /// `NotSent` 不受幂等性限制——请求一个字节都没写出去，重发不会产生第二次副作用。
    pub fn reason(&self, method: &str, not_sent: bool, status: Option<u16>) -> RetryReason {
        if not_sent {
            return RetryReason::NotSent;
        }
        if !Self::is_idempotent(method) {
            return RetryReason::No;
        }
        match status {
            Some(s) if s >= 500 => RetryReason::ServerError,
            Some(_) => RetryReason::No,
            // 没有状态码 = 传输层失败（超时、连接被重置）
            None => RetryReason::Ambiguous,
        }
    }

    pub fn should_retry(&self, attempt: u32, reason: RetryReason) -> bool {
        attempt < self.max && reason != RetryReason::No
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_is_not_retried_after_sent() {
        let p = RetryPolicy::new(2, Duration::from_millis(10));
        assert_eq!(p.reason("POST", false, None), RetryReason::No);
        assert!(!p.should_retry(0, p.reason("POST", false, None)));
    }

    #[test]
    fn post_can_be_retried_when_not_sent() {
        let p = RetryPolicy::new(2, Duration::from_millis(10));
        let r = p.reason("POST", true, None);
        assert_eq!(r, RetryReason::NotSent);
        assert!(p.should_retry(0, r), "没送达的重发不会造成二次副作用");
    }

    #[test]
    fn get_retries_on_5xx() {
        let p = RetryPolicy::new(1, Duration::from_millis(10));
        assert!(p.should_retry(0, p.reason("GET", false, Some(503))));
    }

    #[test]
    fn get_does_not_retry_on_4xx() {
        let p = RetryPolicy::new(3, Duration::from_millis(10));
        assert!(!p.should_retry(0, p.reason("GET", false, Some(404))));
    }

    #[test]
    fn exhausts_after_max() {
        let p = RetryPolicy::new(2, Duration::from_millis(10));
        assert!(p.should_retry(0, RetryReason::ServerError));
        assert!(p.should_retry(1, RetryReason::ServerError));
        assert!(!p.should_retry(2, RetryReason::ServerError));
    }

    #[test]
    fn backoff_is_bounded() {
        let p = RetryPolicy::new(9, Duration::from_millis(100));
        assert_eq!(p.backoff(0), Duration::from_millis(100));
        assert_eq!(p.backoff(1), Duration::from_millis(200));
        assert!(p.backoff(9) <= Duration::from_millis(2000));
    }

    #[test]
    fn put_and_delete_are_idempotent() {
        assert!(RetryPolicy::is_idempotent("put"));
        assert!(RetryPolicy::is_idempotent("DELETE"));
        assert!(!RetryPolicy::is_idempotent("PATCH"));
    }
}
