//! 请求。
//!
//! ## 为什么 `body` 是 `Vec<u8>` 而不是流
//!
//! 自研 HTTP/1.1 栈是同步的，body 在进入 handler 之前已经读完（`server` 层有
//! 1 MiB 上限）。这样提取器可以直接拿走所有权，不需要在 handler 里处理
//! "读一半失败"的中间状态。大文件上传走独立的流式接口，不走这里。
//!
//! ## `extensions` 是请求级，不是进程级
//!
//! 认证结果、租户 ID、当前用户这些东西**必须**挂在请求上。放进 `Context`
//! 会让 A 用户的身份串到 B 用户——这是框架最容易犯的严重 bug，这里从类型上
//! 就不给那个入口（`Context::insert_state` 的文档里也写了同一句话）。

use std::any::{Any, TypeId};
use std::collections::HashMap;

use crate::web::http::{Headers, Method};

/// 请求级扩展存储。按类型取，同类型只能有一个值。
#[derive(Default)]
pub struct Extensions {
    map: HashMap<TypeId, Box<dyn Any>>,
}

impl Extensions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert<T: Any>(&mut self, value: T) -> Option<T> {
        let old = self.map.insert(TypeId::of::<T>(), Box::new(value))?;
        old.downcast::<T>().ok().map(|b| *b)
    }

    pub fn get<T: Any>(&self) -> Option<&T> {
        self.map
            .get(&TypeId::of::<T>())
            .and_then(|b| b.downcast_ref::<T>())
    }

    pub fn contains<T: Any>(&self) -> bool {
        self.map.contains_key(&TypeId::of::<T>())
    }

    pub fn remove<T: Any>(&mut self) -> Option<T> {
        self.map
            .remove(&TypeId::of::<T>())
            .and_then(|b| b.downcast::<T>().ok())
            .map(|b| *b)
    }
}

impl std::fmt::Debug for Extensions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Extensions")
            .field("len", &self.map.len())
            .finish()
    }
}

#[derive(Debug)]
pub struct Request {
    pub method: Method,
    /// 原始路径（未百分号解码）。解码在模板匹配时逐段做，这样 `%2F` 不会
    /// 被当成段分隔符——否则 `/a%2Fb` 能被绕过成两段路径。
    pub path: String,
    /// 原始查询串（不含 `?`）
    pub query: String,
    pub headers: Headers,
    pub body: Vec<u8>,
    /// 模板匹配后填入（已解码）
    pub params: Vec<(String, String)>,
    /// 请求级状态：认证结果等
    pub extensions: Extensions,
    /// 对端地址（形如 `1.2.3.4:5678`），日志与限流用
    pub peer: Option<String>,
}

impl Request {
    pub fn new(method: Method, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            query: String::new(),
            headers: Headers::new(),
            body: Vec::new(),
            params: Vec::new(),
            extensions: Extensions::new(),
            peer: None,
        }
    }

    pub fn with_query(mut self, q: impl Into<String>) -> Self {
        self.query = q.into();
        self
    }

    pub fn with_body(mut self, b: impl Into<Vec<u8>>) -> Self {
        self.body = b.into();
        self
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.insert(name, value);
        self
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)
    }

    /// 取路径参数。
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// 取走 body。第二次调用得到空 slice——提取器会明确报错而不是静默。
    pub fn take_body(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.body)
    }

    /// 客户端提供的 trace id（`X-Request-Id`），没有就 None。
    pub fn request_id(&self) -> Option<&str> {
        self.header("x-request-id")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_are_typed_per_request() {
        let mut r = Request::new(Method::Get, "/x");
        r.extensions.insert(42u64);
        assert_eq!(r.extensions.get::<u64>(), Some(&42));
        assert!(r.extensions.get::<i32>().is_none());
        assert!(r.extensions.contains::<u64>());
        assert_eq!(r.extensions.remove::<u64>(), Some(42));
        assert!(!r.extensions.contains::<u64>());
    }

    #[test]
    fn params_lookup() {
        let mut r = Request::new(Method::Get, "/u/7");
        r.params.push(("id".into(), "7".into()));
        assert_eq!(r.param("id"), Some("7"));
        assert_eq!(r.param("nope"), None);
    }

    #[test]
    fn take_body_empties_it() {
        let mut r = Request::new(Method::Post, "/x").with_body(b"{}".to_vec());
        assert_eq!(r.take_body(), b"{}");
        assert!(r.take_body().is_empty(), "第二次取必须是空，提取器据此报错");
    }

    #[test]
    fn headers_are_case_insensitive() {
        let r = Request::new(Method::Get, "/x").with_header("X-Request-Id", "abc");
        assert_eq!(r.request_id(), Some("abc"));
    }
}
