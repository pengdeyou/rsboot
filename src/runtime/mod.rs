//! runtime：应用运行时，与 HTTP 无关。
//!
//! 这一层知道"配置、数据库、模块、生命周期"，不知道"请求、响应、路由"。
//! 反向依赖是被禁止的（`web` 不许 use `runtime::App`），所以路由与 handler
//! 可以在没有 App 的情况下单独测试。

pub mod app;
pub mod context;
pub mod module;

pub use app::{App, Builder};
pub use context::{Context, DbHealth};
pub use module::Module;
