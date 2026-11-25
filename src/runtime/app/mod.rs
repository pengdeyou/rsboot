//! `App` 的组装与访问器。
//!
//! 这里只放"结构与薄访问器"；配置合并、连库、生命周期、迁移各自一个文件。
//! 一个 386 行的 `app.rs` 混着五件事，是混乱的开始。

mod builder;
mod databases;
mod serve;
mod lifecycle;
mod migrate;

pub use builder::Builder;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rsbatis::{Engine, Session};

use crate::config::watcher::{ReloadSpec, Watcher};
use crate::config::{Config, Environment};
use crate::runtime::context::Context;
use crate::runtime::module::Module;

/// 组装完成的应用。
///
/// 生命周期是显式的：`start()` → 用 `with_db` 跑业务 → `stop()`。
/// 没有隐式 Drop 关闭——关闭钩子没执行这件事必须在调用方看得见，
/// 而不是藏在析构里。
pub struct App {
    pub(crate) ctx: Context,
    /// 打开库用的 URL 列表。留存不是为了复用连接，而是为了让每个 server
    /// worker 能在自己的线程里**重建**一份 Context（`Session` 不跨线程）。
    pub(crate) database_urls: Vec<(String, String)>,
    /// 模块用 `Arc` 而不是 `Box`：除了 `App` 自己，每个 worker 也要引用它们
    /// 来跑 `start_worker` / `stop_worker`。
    pub(crate) modules: Vec<Arc<dyn Module>>,
    /// 已成功启动的模块下标（关闭时按它逆序）
    pub(crate) started: Vec<usize>,
    /// 迁移目录，可多个（模块各自带迁移，启动时聚合）
    pub(crate) migrations: Vec<String>,
    pub(crate) migrate_on_start: bool,
    /// 热加载计划（`start()` 时才真正起线程）
    pub(crate) reload: Option<ReloadPlan>,
    /// 活了就代表热加载线程在跑；`stop()` 先 drop 它
    pub(crate) watcher: Option<Watcher>,
    /// 下游服务表。和每个 worker 的 `Context` 里那份是同一批 `Arc`，
    /// 所以连接池与熔断状态在多 worker 间共享（各熔断各的等于没熔断）。
    pub(crate) services: crate::service::ServiceRegistry,
    /// 对外服务状态。和每个 worker 的 `Context` 里那一个是同一个 `Arc`，
    /// 所以 handler 读到的是主线程 accept 循环写的值。
    pub(crate) status: crate::server::ServeStatus,
    /// 连接借调的累计迁移计数（可选）。给了就由 `Server` 往里累加。
    pub(crate) rebalance_stats: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

/// 热加载所需的一切输入。放在这里而不是 Builder：它是运行期行为。
pub(crate) struct ReloadPlan {
    pub base: Config,
    pub path: PathBuf,
    pub prefix: Option<String>,
    pub hot: Vec<String>,
    pub interval: Duration,
}

impl ReloadPlan {
    pub(crate) fn to_spec(&self) -> ReloadSpec {
        ReloadSpec {
            path: self.path.clone(),
            hot: self.hot.clone(),
            interval: self.interval,
            base: self.base.clone(),
            env_prefix: self.prefix.clone(),
        }
    }
}

impl App {
    /// 把连接借调的累计迁移数写进外部计数器。
    ///
    /// 用途有两个：测试断言借调**真的发生了**（否则"响应都正常"这种断言
    /// 在借调完全没触发时也会绿，等于安慰剂）；运维上也能看到 worker 之间
    /// 是否长期失衡。
    pub fn rebalance_stats(&mut self, h: Arc<std::sync::atomic::AtomicUsize>) {
        self.rebalance_stats = Some(h);
    }

    pub fn builder() -> Builder {
        Builder::new()
    }

    pub fn context(&self) -> &Context {
        &self.ctx
    }

    /// 当前配置快照（热加载后会指向新的一份）。
    pub fn config(&self) -> std::sync::Arc<Config> {
        self.ctx.config()
    }

    /// 生效环境（dev / prod）。
    pub fn env(&self) -> Environment {
        self.ctx.env()
    }

    /// 热加载是否真的开着（有配置文件且 `reload.enabled = true`）。
    pub fn hot_reload_active(&self) -> bool {
        self.watcher.is_some()
    }

    /// 在默认库上执行闭包。
    pub fn with_db<T>(
        &self,
        f: impl FnOnce(&mut Session<Engine>) -> crate::error::Result<T>,
    ) -> crate::error::Result<T> {
        self.ctx.with_db(f)
    }

    /// 在指定库上执行闭包。
    pub fn with_db_named<T>(
        &self,
        name: &str,
        f: impl FnOnce(&mut Session<Engine>) -> crate::error::Result<T>,
    ) -> crate::error::Result<T> {
        self.ctx.with_db_named(name, f)
    }

    /// 探活所有已注册的库（不查下游服务，那是 `/ready` 的事）。
    pub fn health(&self) -> Vec<crate::runtime::context::DbHealth> {
        self.ctx.health()
    }

    pub fn is_running(&self) -> bool {
        !self.started.is_empty()
    }

    /// 已启动的模块名（按启动顺序）。
    pub fn started_modules(&self) -> Vec<&'static str> {
        self.started
            .iter()
            .map(|&i| self.modules[i].name())
            .collect()
    }
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("databases", &self.ctx.db_names())
            .field("modules", &self.modules.len())
            .field("started", &self.started_modules())
            .finish()
    }
}
