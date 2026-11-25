//! 响应：状态 + 头部 + 字节体。
//!
//! 只有一个结构体，没有 builder 链的重载。原因是响应对象会被中间件反复
//! 改造（加头、改状态），`&mut` 比 chain 更直接，也省一次 move。
//!
//! ## 204 / 304 不带 body
//!
//! 由 `into_response` 之外的写出口负责（`server` 层）。这里**不**偷偷清空 body，
//! 否则调用方写了 body 却收不到，会很困惑。改由写出口按 RFC 丢弃并记 warn。

use crate::web::error::ApiError;
use crate::web::http::Headers;
use crate::web::http::Status;

#[derive(Debug, Clone)]
pub struct Response {
    pub status: Status,
    pub headers: Headers,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: Status) -> Self {
        Self {
            status,
            headers: Headers::new(),
            body: Vec::new(),
        }
    }

    pub fn ok() -> Self {
        Self::new(Status::OK)
    }

    /// 已是序列化好的字节，避免再套一层 `Value`。
    pub fn json_with(bytes: &[u8], status: Status) -> Self {
        let mut r = Self {
            status,
            headers: Headers::new(),
            body: bytes.to_vec(),
        };
        r.headers
            .insert("content-type", "application/json; charset=utf-8");
        r
    }

    pub fn json(bytes: &[u8]) -> Self {
        Self::json_with(bytes, Status::OK)
    }

    pub fn text(body: impl Into<String>) -> Self {
        let s = body.into();
        let mut r = Self {
            status: Status::OK,
            headers: Headers::new(),
            body: s.into_bytes(),
        };
        r.headers
            .insert("content-type", "text/plain; charset=utf-8");
        r
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        // 非法名/值静默丢弃是有意的：构造期的头来自代码而非用户输入，
        // 但值里可能拼进了用户数据，此时"不写入"比"panic"安全。
        self.headers.insert(name, value);
        self
    }

    pub fn with_status(mut self, status: Status) -> Self {
        self.status = status;
        self
    }
}

/// 让 handler 可以直接返回各种类型。
///
/// 用 trait 而不是"只接受 Response"：handler 里写 `Ok("done")`、`Ok(())`、
/// `Ok(Status::NO_CONTENT)` 都应该能编译过，这是易用性最直接的体现。
pub trait IntoResponse {
    fn into_response(self) -> Response;
}

impl IntoResponse for Response {
    fn into_response(self) -> Response {
        self
    }
}

impl IntoResponse for Status {
    fn into_response(self) -> Response {
        Response::new(self)
    }
}

impl IntoResponse for () {
    fn into_response(self) -> Response {
        Response::new(Status::NO_CONTENT)
    }
}

impl IntoResponse for &str {
    fn into_response(self) -> Response {
        Response::text(self)
    }
}

impl IntoResponse for String {
    fn into_response(self) -> Response {
        Response::text(self)
    }
}

impl IntoResponse for Vec<u8> {
    fn into_response(self) -> Response {
        let mut r = Response::new(Status::OK);
        r.headers
            .insert("content-type", "application/octet-stream");
        r.body = self;
        r
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        self.to_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_sets_content_type() {
        let r = Response::json(b"{\"a\":1}");
        assert_eq!(r.status, Status::OK);
        assert_eq!(
            r.headers.get("content-type"),
            Some("application/json; charset=utf-8")
        );
        assert_eq!(r.body, b"{\"a\":1}");
    }

    #[test]
    fn empty_body_gets_no_content() {
        assert_eq!(().into_response().status, Status::NO_CONTENT);
    }

    #[test]
    fn str_and_string_become_text() {
        let r = "hi".into_response();
        assert_eq!(r.headers.get("content-type"), Some("text/plain; charset=utf-8"));
        assert_eq!(String::from("hi").into_response().body, b"hi");
    }

    #[test]
    fn header_with_crlf_is_dropped_not_panicked() {
        let r = Response::ok().with_header("x", "a\r\nb");
        assert!(r.headers.is_empty());
    }

    #[test]
    fn status_alone_is_a_valid_handler_return() {
        assert_eq!(Status::NOT_FOUND.into_response().status, Status::NOT_FOUND);
    }
}
