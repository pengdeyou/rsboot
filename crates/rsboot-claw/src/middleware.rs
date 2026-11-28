//! Downloader Middleware：请求发出去之前、响应回来之后、出错时的三个钩子。
//!
//! Scrapy 的分层里，这一层管"怎么发"——UA 轮换、加头、按状态码/错误决定重试。
//! 它和 Sink 的区别：
//!
//! - `Sink`（[`crate::pipeline`]）管**结果往哪写**，拿不到请求，也改不了请求
//! - `Middleware`（这里）管**请求怎么发**，改的是要发出去的东西，不碰落盘
//!
//! 每个钩子返回 [`Action`]：
//!
//! | 返回 | 含义 |
//! | --- | --- |
//! | `Continue` | 走下一层，最终照常发/照常收 |
//! | `Skip` | 这一页不要了（不发，或收到了也丢） |
//! | `Retry` | 重发（由调用方退避后重试；有硬上限护住，不会被中间件拖成死循环） |
//!
//! 多个中间件按注册顺序串起来：任一 `Skip` 就短路，任一 `Retry` 就重试。
//!
//! 明确不做**代理**：HTTP 代理要 `CONNECT` 隧道或把请求行改成绝对 URI，
//! 还要处理 `407` 认证，是另一套东西；零依赖下先不做，需要时自己实现一个
//! `Middleware` 也做不到（它只能改头，改不了连接目标），得在 `Client` 外面套。

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::http::Error;
use crate::url::Url;

/// 钩子返回值。见模块文档里的表。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Continue,
    Retry,
    Skip,
}

/// 一个中间件。三个方法都有默认实现（`Continue`），实现时只覆盖需要的那个。
pub trait Middleware: Send + Sync {
    /// 请求发出前。`headers` 是本次请求的额外头，可以直接 push。
    /// `attempt` 从 0 开始，重试时递增。
    fn before(&self, url: &Url, headers: &mut Vec<(String, String)>, attempt: usize) -> Action {
        let _ = (url, headers, attempt);
        Action::Continue
    }

    /// 响应回来了（还没交给 handler）。
    fn after(&self, url: &Url, status: u16, attempt: usize) -> Action {
        let _ = (url, status, attempt);
        Action::Continue
    }

    /// 请求失败（连接错误、协议错误、body 超限……）。
    fn on_error(&self, url: &Url, error: &Error, attempt: usize) -> Action {
        let _ = (url, error, attempt);
        Action::Continue
    }
}

/// 按状态码 + 网络错误重试。
///
/// 默认重试 2 次。**只决定要不要重试**，退避节奏由
/// [`Crawler::retry_backoff`](crate::Crawler) 统一控制——两处都配会打架。
///
/// 重试的状态码是 `408` / `429` / `5xx`——这三个是"对面让我等一下再试"的信号；
/// `404` / `403` 重试多少次都是同一个答案。
pub struct RetryPolicy {
    retries: usize,
}

impl RetryPolicy {
    pub fn new() -> Self {
        Self { retries: 2 }
    }

    pub fn retries(mut self, n: usize) -> Self {
        self.retries = n;
        self
    }

    pub fn limit(&self) -> usize {
        self.retries
    }

    fn worth_retrying(status: u16) -> bool {
        status == 408 || status == 429 || status >= 500
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl Middleware for RetryPolicy {
    fn after(&self, _url: &Url, status: u16, attempt: usize) -> Action {
        if Self::worth_retrying(status) && attempt < self.retries {
            Action::Retry
        } else {
            Action::Continue
        }
    }

    fn on_error(&self, _url: &Url, _error: &Error, attempt: usize) -> Action {
        if attempt < self.retries {
            Action::Retry
        } else {
            Action::Continue
        }
    }
}

/// 每个请求换一个 UA。
///
/// 用 `AtomicUsize` 轮转而不是随机：并发下也要确定——"第 N 个请求用第
/// N % k 个 UA"能在测试里断言出来，随机不行。
pub struct RotateUserAgent {
    agents: Vec<String>,
    cursor: AtomicUsize,
}

impl RotateUserAgent {
    pub fn new(agents: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let agents: Vec<String> = agents.into_iter().map(Into::into).collect();
        assert!(!agents.is_empty(), "RotateUserAgent 至少要一个 UA");
        Self {
            agents,
            cursor: AtomicUsize::new(0),
        }
    }

    /// 已经发出过多少个请求（用于断言轮转真的转起来了）。
    pub fn issued(&self) -> usize {
        self.cursor.load(Ordering::Relaxed)
    }
}

impl Middleware for RotateUserAgent {
    fn before(&self, _url: &Url, headers: &mut Vec<(String, String)>, _attempt: usize) -> Action {
        let i = self.cursor.fetch_add(1, Ordering::Relaxed);
        headers.push((
            "user-agent".to_string(),
            self.agents[i % self.agents.len()].clone(),
        ));
        Action::Continue
    }
}

/// 给每个请求加固定的一组头（`Accept-Language`、`Cookie`、站点要求的 `Referer`）。
pub struct StaticHeaders {
    headers: Vec<(String, String)>,
}

impl StaticHeaders {
    pub fn new(pairs: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>) -> Self {
        Self {
            headers: pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }
}

impl Middleware for StaticHeaders {
    fn before(&self, _url: &Url, headers: &mut Vec<(String, String)>, _attempt: usize) -> Action {
        headers.extend(self.headers.iter().cloned());
        Action::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u() -> Url {
        Url::parse("http://a.com/x").unwrap()
    }

    #[test]
    fn retry_policy_counts_attempts() {
        let p = RetryPolicy::new().retries(2);
        assert_eq!(p.after(&u(), 503, 0), Action::Retry);
        assert_eq!(p.after(&u(), 503, 1), Action::Retry);
        assert_eq!(p.after(&u(), 503, 2), Action::Continue, "第 3 次不再重试");
        assert_eq!(p.after(&u(), 404, 0), Action::Continue, "404 不重试");
        assert_eq!(p.after(&u(), 429, 0), Action::Retry);
        assert_eq!(p.after(&u(), 200, 0), Action::Continue);
    }

    #[test]
    fn retry_policy_retries_errors_then_gives_up() {
        let p = RetryPolicy::new().retries(1);
        let e = Error::Connect("x".into());
        assert_eq!(p.on_error(&u(), &e, 0), Action::Retry);
        assert_eq!(p.on_error(&u(), &e, 1), Action::Continue);
    }

    #[test]
    fn ua_rotates_deterministically() {
        let r = RotateUserAgent::new(["a", "b", "c"]);
        let mut seen = Vec::new();
        for _ in 0..4 {
            let mut hs = Vec::new();
            r.before(&u(), &mut hs, 0);
            assert_eq!(hs.len(), 1);
            seen.push(hs[0].1.clone());
        }
        assert_eq!(seen, vec!["a", "b", "c", "a"], "轮转必须是确定的");
        assert_eq!(r.issued(), 4);
    }

    #[test]
    fn static_headers_are_appended() {
        let s = StaticHeaders::new([("Accept-Language", "zh-CN"), ("X-A", "1")]);
        let mut hs = Vec::new();
        s.before(&u(), &mut hs, 0);
        assert_eq!(hs.len(), 2);
        assert_eq!(hs[0].0, "Accept-Language");
    }
}
