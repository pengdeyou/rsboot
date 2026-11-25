//! 协议原语：方法、状态码、头部。
//!
//! 这三个是"HTTP 的词汇表"，不含任何路由或业务逻辑，可以被任何协议层复用
//! （WebSocket 握手要用状态码 101 与头部，RPC 要复用方法语义）。

pub mod headers;
pub mod method;
pub mod status;

pub use headers::Headers;
pub use method::Method;
pub use status::Status;
