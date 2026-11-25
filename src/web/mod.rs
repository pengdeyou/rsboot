//! HTTP 层：协议词汇（`http`）、路由模板（`path`）、错误与响应。
//!
//! 这一层**不许** use `runtime::App`，路由与 handler 只认 `&Context`。
//! 这样路由表可以在没有 App 的情况下单独测试（`TestClient` 就是这么来的）。

pub mod error;
pub mod http;
pub mod path;
pub mod response;

pub use error::ApiError;
pub use http::{Headers, Method, Status};
pub use path::PathTemplate;
pub use response::{IntoResponse, Response};
