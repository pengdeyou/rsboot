//! rsboot：FastAPI 风格的 Rust 应用框架。
//!
//! 这个文件**只做 re-export，零实现**。任何一行逻辑写在这里都会变成
//! "所有人都依赖的隐藏耦合点"，模块边界也就失效了。
//!
//! 依赖方向（单向，不可逆）：
//!
//! ```text
//! support  ← 谁都可以用，它不 use 任何人
//! error    ← 只 use support
//! config   ← use error、support
//! runtime  ← use config、error、support（以及 rsbatis）
//! web      ← use config、error、support；不许 use runtime::App
//! server   ← use web、runtime；只认 &Router / &Context
//! ```
//!
//! 各 crate（协议扩展）都挂在 `server` 之上：`rsboot-ws`（WebSocket）、
//! `rsboot-rpc`（JSON-RPC）、`rsboot-sse`（服务端推送）都是独立 crate，
//! 按需引入，核心不背它们的依赖。

pub mod config;
pub mod error;
pub mod runtime;
pub mod server;
pub mod service;
pub mod support;
pub mod web;

pub use config::{Config, Environment};
pub use error::{Error, Result};
pub use runtime::{App, Builder, Context, DbHealth, Module};
pub use server::{ConnLimits, ServeState, ServeStatus, Server};

/// 睡一段时间。**在请求处理里请用它，不要用 `std::thread::sleep`**。
///
/// 在 worker 的协程里调用时，这 20ms 不占用线程：worker 会立刻回去处理
/// 别的连接，到点再把你接回来。`std::thread::sleep` 会把整条 worker 焊死，
/// 一次下游慢调用就能让吞吐从 9900 QPS 掉到 `worker 数 / 延迟`（实测 100）。
pub fn sleep(d: std::time::Duration) {
    crate::server::mn::sleep(d);
}

/// 让出 CPU，让同一 worker 上的其他请求先跑。协程外调用是空操作。
pub fn yield_now() {
    crate::server::mn::yield_now();
}
pub use service::{
    Answer, Call, Service, ServiceConfig, ServiceError, ServiceRegistry,
};
pub use web::{
    Api, ApiError, ApiKey, ApiResult, Bearer, Envelope, FromRequest, IntoApiError, IntoResponse, Json,
    Method, Path, Query, Request,
    ParamDoc, Response, RouteDoc, Router, Status,
};

/// 版本号来自 Cargo.toml，供 `/health` 与启动日志展示。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
