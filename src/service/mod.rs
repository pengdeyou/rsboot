//! 下游服务调用（service client）。
//!
//! rsboot 是同步模型（一个请求占一个 worker 线程到处理完），所以客户端也是
//! **同步**的：调用 `send()` 时线程阻塞。这与框架自身的并发模型一致，
//! 不引入 async 桥接（那会让简单的事情变复杂两倍）。
//!
//! ## 一次调用做了什么
//!
//! ```text
//! 熔断检查 → 借连接（池里有就复用）→ 序列化 → 发 → 读 → 还连接
//!          ↑____________________ 失败且可重试 ____________________|
//! ```
//!
//! 每一层都有明确的"不做什么"：
//! - 池：不探活（探活比失败重试贵）；
//! - 重试：只对幂等方法 / 没发出去的请求；
//! - 熔断：连续失败到阈值就停手，别把本服务线程拖死。
//!
//! ## 明确的限制
//!
//! 只支持 `http://`。https 需要 TLS 栈（rustls 或 openssl），rsboot 的
//! 自研 HTTP/1.1 栈里没有，也不会为了下游调用引进来——那等于把"零依赖"
//! 这个最大的优点换掉。跨公网调用请走内网服务或网关。

use std::io::BufReader;
use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::error::{Error, Result};
use crate::web::error::ApiError;
use crate::web::exception::IntoApiError;
use crate::web::http::Status;

pub mod breaker;
pub mod pool;
pub mod retry;
pub mod wire;

use breaker::Breaker;
use pool::Pool;
use retry::{RetryPolicy, RetryReason};

/// 下游调用的失败分类。**分类的意义在"能不能重试"与"给客户端什么码"**。
#[derive(Debug)]
pub enum ServiceError {
    /// 配置不合法（base_url 写错），启动期就该炸
    Config(String),
    /// 连不上：请求肯定没送达
    Connect(String),
    /// 发出去了，等待超时
    Timeout { after: Duration },
    /// 写/读中途失败（连接被重置等）
    Transport(String),
    /// 响应格式不合法
    Wire(wire::WireError),
    /// 响应体超过上限
    BodyTooLarge(usize),
    /// 熔断中，本次根本没发
    BreakerOpen { failures: u32 },
    /// 下游返回了非 2xx（只在 `json()` 这类"要数据"的调用上出现）
    Status { status: u16, body: Vec<u8> },
    /// JSON 反序列化失败
    Decode(String),
}

impl ServiceError {
    /// 请求是否"肯定没送达"。决定非幂等方法能不能安全重试。
    pub fn not_sent(&self) -> bool {
        matches!(self, ServiceError::Connect(_) | ServiceError::BreakerOpen { .. } | ServiceError::Config(_))
    }

    /// 映射成 API 错误：**下游挂了不该是本服务的 500**。
    ///
    /// 500 会让上游以为"是我们自己的 bug"；502/503 才表达"问题在依赖"。
    pub fn to_api_error(&self) -> ApiError {
        match self {
            ServiceError::Config(m) => ApiError::internal(format!("下游服务配置错误：{m}")),
            ServiceError::Connect(m) => ApiError::new(
                Status::SERVICE_UNAVAILABLE,
                format!("下游服务不可用：{m}"),
            ),
            ServiceError::Timeout { after } => ApiError::new(
                Status::GATEWAY_TIMEOUT,
                format!("下游服务响应超时（{}ms）", after.as_millis()),
            ),
            ServiceError::Transport(m) => {
                ApiError::new(Status::BAD_GATEWAY, format!("与下游的连接中断：{m}"))
            }
            ServiceError::Wire(e) => {
                ApiError::new(Status::BAD_GATEWAY, format!("下游响应不合法：{e}"))
            }
            ServiceError::BodyTooLarge(n) => ApiError::new(
                Status::BAD_GATEWAY,
                format!("下游响应体过大（{n} 字节），拒绝读入"),
            ),
            ServiceError::BreakerOpen { failures } => ApiError::new(
                Status::SERVICE_UNAVAILABLE,
                format!("下游已被熔断（连续失败 {failures} 次），稍后重试"),
            ),
            ServiceError::Decode(m) => {
                ApiError::new(Status::BAD_GATEWAY, format!("下游响应无法解析：{m}"))
            }
            // 4xx 原样透传（把"参数错了"如实告诉上游）；5xx 换成 502
            ServiceError::Status { status, .. } if *status < 500 => {
                ApiError::new(Status(*status), format!("下游返回 {status}"))
            }
            ServiceError::Status { status, .. } => ApiError::new(
                Status::BAD_GATEWAY,
                format!("下游错误（{status}）"),
            ),
        }
    }
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServiceError::Config(m) => write!(f, "配置错误：{m}"),
            ServiceError::Connect(m) => write!(f, "连接失败：{m}"),
            ServiceError::Timeout { after } => {
                write!(f, "超时（{}ms）", after.as_millis())
            }
            ServiceError::Transport(m) => write!(f, "传输失败：{m}"),
            ServiceError::Wire(e) => write!(f, "{e}"),
            ServiceError::BodyTooLarge(n) => write!(f, "响应体过大：{n} 字节"),
            ServiceError::BreakerOpen { failures } => write!(f, "熔断中（失败 {failures} 次）"),
            ServiceError::Status { status, .. } => write!(f, "下游返回 {status}"),
            ServiceError::Decode(m) => write!(f, "解析失败：{m}"),
        }
    }
}

impl std::error::Error for ServiceError {}

/// handler 里可以直接 `?`：`ServiceError` 自动变成 502/503
impl IntoApiError for ServiceError {
    fn into_api_error(self) -> ApiError {
        self.to_api_error()
    }
}

/// 一个下游的配置。构造后不可变（池与熔断器内部可变）。
#[derive(Debug)]
pub struct ServiceConfig {
    pub name: String,
    /// `host:port`（从 base_url 解析出来）
    pub host: String,
    /// 路径前缀，来自 base_url 的 path 部分
    pub base_path: String,
    pub timeout: Duration,
    pub connect_timeout: Duration,
    pub max_body: usize,
    pub max_idle: usize,
    pub retry: RetryPolicy,
    pub breaker_threshold: u32,
    pub breaker_cooldown: Duration,
}

impl ServiceConfig {
    pub fn from_url(name: impl Into<String>, url: &str) -> Result<Self> {
        let name = name.into();
        let (host, base_path) = parse_http_url(url)?;
        Ok(Self {
            name,
            host,
            base_path,
            timeout: Duration::from_secs(3),
            connect_timeout: Duration::from_millis(800),
            max_body: 8 * 1024 * 1024,
            max_idle: 8,
            retry: RetryPolicy::new(1, Duration::from_millis(20)),
            breaker_threshold: 5,
            breaker_cooldown: Duration::from_secs(10),
        })
    }
}

/// 只解析 `http://host[:port][/path]`。https 是显式错误而不是静默降级——
/// 静默降级成明文会把"以为加密了"变成安全事故。
fn parse_http_url(url: &str) -> Result<(String, String)> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| {
            if url.starts_with("https://") {
                Error::Config(format!(
                    "`{url}` 是 https：rsboot 的客户端没有 TLS 栈（不引 rustls）。\
                     请走内网明文服务或前置网关"
                ))
            } else {
                Error::Config(format!("`{url}` 必须以 http:// 开头"))
            }
        })?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return Err(Error::Config(format!("`{url}` 缺少主机名")));
    }
    let host = if authority.contains(':') {
        authority.to_string()
    } else {
        format!("{authority}:80")
    };
    Ok((host, path.to_string()))
}

/// 一个下游服务。`Arc` 共享：池与熔断器在多个 worker 间共用同一份，
/// 否则每个 worker 各熔断各的，等于没熔断。
pub struct Service {
    cfg: ServiceConfig,
    pool: Pool,
    breaker: Breaker,
}

impl Service {
    pub fn new(cfg: ServiceConfig) -> Arc<Self> {
        let pool = Pool::new(cfg.host.clone(), cfg.max_idle, cfg.connect_timeout);
        let breaker = Breaker::new(cfg.breaker_threshold, cfg.breaker_cooldown);
        Arc::new(Self {
            cfg,
            pool,
            breaker,
        })
    }

    pub fn from_url(name: &str, url: &str) -> Result<Arc<Self>> {
        Ok(Self::new(ServiceConfig::from_url(name, url)?))
    }

    pub fn name(&self) -> &str {
        &self.cfg.name
    }

    pub fn host(&self) -> &str {
        &self.cfg.host
    }

    /// 熔断是否打开（给健康检查用）
    pub fn is_open(&self) -> bool {
        self.breaker.is_open()
    }

    pub fn get(&self, path: &str) -> Call<'_> {
        self.call("GET", path)
    }
    pub fn post(&self, path: &str) -> Call<'_> {
        self.call("POST", path)
    }
    pub fn put(&self, path: &str) -> Call<'_> {
        self.call("PUT", path)
    }
    pub fn patch(&self, path: &str) -> Call<'_> {
        self.call("PATCH", path)
    }
    pub fn delete(&self, path: &str) -> Call<'_> {
        self.call("DELETE", path)
    }

    fn call(&self, method: &'static str, path: &str) -> Call<'_> {
        Call {
            svc: self,
            method,
            path: path.to_string(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// 把 base_path 与调用路径拼成请求目标。
    fn target(&self, path: &str) -> String {
        let base = self.cfg.base_path.trim_end_matches('/');
        if path.is_empty() {
            return if base.is_empty() {
                "/".to_string()
            } else {
                base.to_string()
            };
        }
        if path.starts_with('/') || path.starts_with('?') {
            format!("{base}{path}")
        } else {
            format!("{base}/{path}")
        }
    }

    /// 发一次（不含重试）。
    fn once(&self, req: &wire::WireRequest) -> Attempt {
        // 熔断器在 send 里判，这里不再判：一次请求只查一次
        let stream = match self.pool.take(self.cfg.timeout) {
            Ok(s) => s,
            Err(e) => {
                return Attempt::Failed(ServiceError::Connect(format!("{}：{e}", self.cfg.host)), true)
            }
        };
        let bytes = req.serialize();
        if let Err(e) = crate::server::mn::net::write_all(&stream, &bytes) {
            // 一个字节都没写出去才是"没送达"；write_all 失败无法区分，
            // 保守按"不确定"处理（非幂等方法因此不会重试）
            let _ = e;
            return Attempt::Failed(
                ServiceError::Transport(format!("写请求失败：{e}")),
                false,
            );
        }
        // 借用而非 `try_clone`：省一次 fd dup，顺便让读走上协程链路。
        // `BufReader` 在块结束时就 drop，之后 `stream` 才能移交给连接池。
        let resp = {
            let mut reader = BufReader::new(crate::server::mn::net::CoReader(&stream));
            wire::read_response(&mut reader, self.cfg.max_body)
        };
        match resp {
            Ok(r) => {
                // 一问一答读完了，连接可以复用（残留数据会让下轮解析失败并触发重试）
                self.pool.give(stream);
                Attempt::Done(Answer {
                    status: r.status,
                    headers: r.headers,
                    body: r.body,
                })
            }
            Err(e) => {
                // 连接状态已不可知：不还回去
                let err = match &e {
                    wire::WireError::Io(io) if is_timeout(io) => ServiceError::Timeout {
                        after: self.cfg.timeout,
                    },
                    wire::WireError::BodyTooLarge(n) => ServiceError::BodyTooLarge(*n),
                    _ => ServiceError::Wire(e),
                };
                Attempt::Failed(err, false)
            }
        }
    }
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

enum Attempt {
    Done(Answer),
    /// (`错误`, 是否"肯定没送达")
    Failed(ServiceError, bool),
}

/// 一次调用的构建器。`send()` 之后拿到 `Answer`。
pub struct Call<'a> {
    svc: &'a Service,
    method: &'static str,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl<'a> Call<'a> {
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn json_body<T: Serialize>(mut self, value: &T) -> Result<Self> {
        let bytes = serde_json::to_vec(value)
            .map_err(|e| Error::Config(format!("请求体序列化失败：{e}")))?;
        self.headers
            .push(("content-type".into(), "application/json".into()));
        self.body = bytes;
        Ok(self)
    }

    pub fn text_body(mut self, body: impl Into<String>) -> Self {
        self.body = body.into().into_bytes();
        self
    }

    pub fn send(self) -> std::result::Result<Answer, ServiceError> {
        let svc = self.svc;
        // 透传 trace：同一链路的日志能用同一个 id 串起来。
        // 调用方显式给了就不覆盖（方便打一个独立的 id 做隔离排查）。
        let mut headers = self.headers;
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("x-request-id")) {
            if let Some(t) = crate::web::exception::current_trace() {
                headers.push(("x-request-id".into(), t));
            }
        }

        let target = svc.target(&self.path);
        let req = wire::WireRequest {
            method: self.method,
            target,
            host: svc.cfg.host.clone(),
            headers,
            body: self.body,
        };

        let mut attempt = 0u32;
        loop {
            if !svc.breaker.allow() {
                let failures = svc.breaker.failures();
                return Err(ServiceError::BreakerOpen { failures });
            }
            match svc.once(&req) {
                Attempt::Done(a) if a.status >= 500 => {
                    svc.breaker.on_failure();
                    let reason = svc.cfg.retry.reason(self.method, false, Some(a.status));
                    if svc.cfg.retry.should_retry(attempt, reason) {
                        std::thread::sleep(svc.cfg.retry.backoff(attempt));
                        attempt += 1;
                        continue;
                    }
                    // 5xx 是"下游明确报错"，重试完了也要如实回给调用方
                    return Ok(a);
                }
                Attempt::Done(a) => {
                    svc.breaker.on_success();
                    return Ok(a);
                }
                Attempt::Failed(e, not_sent) => {
                    svc.breaker.on_failure();
                    let reason = svc.cfg.retry.reason(self.method, not_sent, None);
                    if matches!(reason, RetryReason::NotSent | RetryReason::Ambiguous)
                        && svc.cfg.retry.should_retry(attempt, reason)
                    {
                        std::thread::sleep(svc.cfg.retry.backoff(attempt));
                        attempt += 1;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    /// 要数据：非 2xx 直接变错误，省掉调用方每次自己判状态码。
    pub fn json<T: DeserializeOwned>(self) -> std::result::Result<T, ServiceError> {
        let a = self.send()?;
        a.json()
    }
}

/// 下游的原始响应。
#[derive(Debug)]
pub struct Answer {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Answer {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        let want = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k.to_ascii_lowercase() == want)
            .map(|(_, v)| v.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }

    pub fn json<T: DeserializeOwned>(&self) -> std::result::Result<T, ServiceError> {
        if !self.is_success() {
            return Err(ServiceError::Status {
                status: self.status,
                body: self.body.clone(),
            });
        }
        serde_json::from_slice(&self.body).map_err(|e| ServiceError::Decode(e.to_string()))
    }
}

/// 名字 → 服务。放在 `Context` 里，handler 里 `ctx.service("user")` 直接取。
#[derive(Default, Clone)]
pub struct ServiceRegistry {
    map: std::collections::HashMap<String, Arc<Service>>,
}

impl ServiceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, svc: Arc<Service>) {
        self.map.insert(svc.name().to_string(), svc);
    }

    pub fn get(&self, name: &str) -> Option<Arc<Service>> {
        self.map.get(name).cloned()
    }

    pub fn names(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.map.keys().map(|s| s.as_str()).collect();
        v.sort_unstable();
        v
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl std::fmt::Debug for ServiceRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceRegistry")
            .field("services", &self.names())
            .finish()
    }
}

pub use retry::RetryReason as Reason;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_parses_host_and_base_path() {
        let c = ServiceConfig::from_url("user", "http://127.0.0.1:8081/api").unwrap();
        assert_eq!(c.host, "127.0.0.1:8081");
        assert_eq!(c.base_path, "/api");
    }

    #[test]
    fn url_defaults_port_80() {
        let c = ServiceConfig::from_url("user", "http://example.com").unwrap();
        assert_eq!(c.host, "example.com:80");
        assert_eq!(c.base_path, "");
    }

    #[test]
    fn https_is_a_config_error_not_silent_downgrade() {
        let e = ServiceConfig::from_url("user", "https://example.com").unwrap_err();
        assert!(e.to_string().contains("TLS"));
    }

    #[test]
    fn target_joins_paths_without_double_slash() {
        let svc = Service::from_url("x", "http://127.0.0.1:1/api/").unwrap();
        assert_eq!(svc.target("/users"), "/api/users");
        assert_eq!(svc.target("users"), "/api/users");
        assert_eq!(svc.target(""), "/api");
    }

    #[test]
    fn error_mapping_keeps_downstream_4xx() {
        let e = ServiceError::Status {
            status: 404,
            body: Vec::new(),
        };
        assert_eq!(e.to_api_error().status.0, 404);
    }

    #[test]
    fn downstream_5xx_becomes_502_not_500() {
        let e = ServiceError::Status {
            status: 500,
            body: Vec::new(),
        };
        assert_eq!(
            e.to_api_error().status.0,
            502,
            "下游 500 不该冒充本服务的 500"
        );
    }

    #[test]
    fn registry_lookup_by_name() {
        let svc = Service::from_url("user", "http://127.0.0.1:1").unwrap();
        let mut reg = ServiceRegistry::new();
        reg.insert(svc);
        assert!(reg.get("user").is_some());
        assert!(reg.get("nope").is_none());
        assert_eq!(reg.names(), vec!["user"]);
    }
}
