//! HTTP 层：协议词汇（`http`）、路由模板（`path`）、错误与响应。
//!
//! 这一层**不许** use `runtime::App`，路由与 handler 只认 `&Context`。
//! 这样路由表可以在没有 App 的情况下单独测试（`TestClient` 就是这么来的）。

pub mod envelope;
pub mod exception;
pub mod error;
pub mod extract;
pub mod http;
pub mod json;
pub mod path;
pub mod request;
pub mod response;
pub mod route;

pub use envelope::{Api, Envelope};
pub use exception::{ApiResult, IntoApiError};
pub use error::ApiError;
pub use extract::{ApiKey, Bearer, FromRequest, Path, Query};
pub use http::{Headers, Method, Status};
pub use json::Json;
pub use path::PathTemplate;
pub use request::Request;
pub use response::{IntoResponse, Response};
pub use route::Router;
