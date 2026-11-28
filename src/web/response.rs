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
    pub headers: Headers<'static>,
    pub body: Vec<u8>,
    /// 这条响应"本来是哪个错误"。
    ///
    /// `IntoResponse for Result<T, E>` 把 `E` 转成响应后，**统一响应体开关**
    /// 还需要原始 `ApiError` 才能换成包装形状（`code`/`message`/`data`）。
    /// 所以这里留一份原件：包装只是在出口处换皮，诊断信息一点不丢。
    /// 关掉包装时这个字段被忽略，形状与裸 `detail` 完全一致。
    pub api_error: Option<ApiError>,
    /// 流式 body。为 `Some` 时 `body` 被忽略，写出口改用
    /// `transfer-encoding: chunked` 边算边发（HTTP/1.0 客户端则靠关闭连接界定）。
    ///
    /// 平时是 `None`：判空只是一次位测试，热路径几乎零成本。
    pub stream: Option<StreamBody>,
}

/// 分块输出的落点。要写一块就 `send` 一次，不要自己攒「反正最后一起发」——
/// 攒起来就等于回到一次性 body，`content-length` 又没法给了。
pub trait ChunkSink {
    fn send(&mut self, chunk: &[u8]) -> std::io::Result<()>;
}

/// `Response::stream` 的 body 工厂。
///
/// 用 `Fn` 而不是 `FnOnce`：`Response` 是 `Clone`（中间件到处在克隆），
/// `FnOnce` 会让它编不过。代价是内部状态得自己拿 `Mutex` 包起来——反正
/// 一条流只写一次，那把锁永远不竞争。
#[derive(Clone)]
pub struct StreamBody(std::sync::Arc<dyn Fn(&mut dyn ChunkSink) -> std::io::Result<()> + Send + Sync>);

impl std::fmt::Debug for StreamBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StreamBody")
    }
}

impl StreamBody {
    pub fn new<F>(f: F) -> Self
    where
        F: Fn(&mut dyn ChunkSink) -> std::io::Result<()> + Send + Sync + 'static,
    {
        Self(std::sync::Arc::new(f))
    }

    pub fn send(&self, sink: &mut dyn ChunkSink) -> std::io::Result<()> {
        (self.0)(sink)
    }
}

impl Response {
    pub fn new(status: Status) -> Self {
        Self {
            status,
            headers: Headers::new(),
            body: Vec::new(),
            api_error: None,
            stream: None,
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
            api_error: None,
            stream: None,
        };
        r.headers
            .insert_static("content-type", "application/json; charset=utf-8");
        r
    }

    /// 直接接管调用方已拼好的字节，**零拷贝**。
    ///
    /// `json_with` 末尾有一次 `to_vec()`：调用方刚分配好 buffer 又被拷一遍，
    /// 在统一响应体这种"本来就要逐段拼字节"的路径上纯属白拷。
    pub fn json_owned(body: Vec<u8>, status: Status) -> Self {
        let mut r = Self {
            status,
            headers: Headers::new(),
            body,
            api_error: None,
            stream: None,
        };
        r.headers
            .insert_static("content-type", "application/json; charset=utf-8");
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
            api_error: None,
            stream: None,
        };
        r.headers
            .insert_static("content-type", "text/plain; charset=utf-8");
        r
    }

    /// HTML 响应。`/docs` 这类内置页面用；业务要回 HTML 也走它。
    ///
    /// 不设 `content-security-policy`：文档页要在浏览器里 fetch 自己的
    /// `/openapi.json` 并执行内联脚本，任何严格 CSP 都会把它锁死。
    pub fn html(body: impl Into<String>) -> Self {
        let s = body.into();
        let mut r = Self {
            status: Status::OK,
            headers: Headers::new(),
            body: s.into_bytes(),
            api_error: None,
            stream: None,
        };
        r.headers.insert_static("content-type", "text/html; charset=utf-8");
        r
    }

    /// 分块输出：头先发出去，body 由闭包一块一块算出来。
    ///
    /// 为什么值得占一个字段：SSE、日志尾随、大 CSV 导出、AI 流式回复，
    /// 这些都没法在 handler 里先拼成 `Vec<u8>` 再返回——要么内存爆，要么
    /// 首字节等到算完才出。chunked 是 HTTP/1.1 给出的标准答案。
    ///
    /// 闭包里必须**自己处理错误**：头已经发出去了，状态码改不了了，
    /// 这里返回 `Err` 只会让连接被断开（客户端看到不完整的流）。
    pub fn stream<F>(status: Status, f: F) -> Self
    where
        F: Fn(&mut dyn ChunkSink) -> std::io::Result<()> + Send + Sync + 'static,
    {
        let mut r = Self::new(status);
        r.stream = Some(StreamBody::new(f));
        r
    }

    /// 有没有流式 body。
    pub fn is_stream(&self) -> bool {
        self.stream.is_some()
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
