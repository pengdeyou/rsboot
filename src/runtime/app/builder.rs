//! `Builder`：收集意图，在 `build()` 时一次性校验并组装。
//!
//! 校验集中在 `build()`，而不是散落在各个 setter 里。理由：setter 阶段还
//! 不知道别的设置会怎么配（"热更白名单指向不存在的键"这种问题只有最后才看得出来）。

use std::collections::HashSet;
use std::sync::Arc;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::layered;
use crate::config::{Config, Environment};
use crate::error::{Error, Result};
use crate::runtime::app::databases;
use crate::runtime::app::{App, ReloadPlan};
use crate::runtime::context::Context;
use crate::runtime::module::Module;

pub struct Builder {
    config: Config,
    file: Option<String>,
    env_prefix: Option<String>,
    /// 代码里显式注册的库（覆盖配置文件同名项）
    databases: Vec<(String, String)>,
    modules: Vec<Arc<dyn Module>>,
    migrations: Vec<String>,
    migrate_on_start: bool,
    /// 代码里注册的下游服务（名字，base_url）
    services: Vec<(String, String)>,
    /// 允许热生效的键模式
    hot: Vec<String>,
    reload_interval: Duration,
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

impl Builder {
    pub fn new() -> Self {
        Self {
            config: Config::new(),
            file: None,
            env_prefix: None,
            databases: Vec::new(),
            modules: Vec::new(),
            migrations: Vec::new(),
            migrate_on_start: false,
            services: Vec::new(),
            hot: Vec::new(),
            reload_interval: Duration::from_secs(1),
        }
    }

    /// 配置文件路径（`.yaml`/`.yml` 走 YAML 子集，其余走 ini）。读不到就报错。
    pub fn file(mut self, path: impl Into<String>) -> Self {
        self.file = Some(path.into());
        self
    }

    /// 环境变量前缀，例如 `MYAPP_`。只覆盖已存在的键。
    pub fn env_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.env_prefix = Some(prefix.into());
        self
    }

    /// 显式声明环境。等价于 `set("app.env", ...)`，但写起来短、也不会拼错。
    pub fn env(mut self, env: Environment) -> Self {
        self.config = self.config.set("app.env", env.name());
        self
    }

    pub fn config(mut self, config: Config) -> Self {
        self.config = config;
        self
    }

    pub fn set(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.config = self.config.set(key, value);
        self
    }

    /// 注册数据库。名为 `default` 的库是默认库。
    pub fn database(mut self, name: impl Into<String>, url: impl Into<String>) -> Self {
        self.databases.push((name.into(), url.into()));
        self
    }

    /// 注册模块。启动顺序 = 注册顺序，关闭顺序相反。
    /// 注册一个下游服务（微服务调用）。
    ///
    /// 名字用来在 handler 里取：`ctx.service("user")`。只支持 `http://`——
    /// https 需要 TLS 栈，rsboot 不引。base_url 里的 path 会当作前缀。
    pub fn service(mut self, name: impl Into<String>, base_url: impl Into<String>) -> Self {
        self.services.push((name.into(), base_url.into()));
        self
    }

    pub fn module<M: Module + 'static>(mut self, m: M) -> Self {
        self.modules.push(Arc::new(m));
        self
    }

    /// 已装箱的模块（条件注册、或同一模块要在多个 App 间共享时用）。
    pub fn module_boxed(mut self, m: Arc<dyn Module>) -> Self {
        self.modules.push(m);
        self
    }

    /// 启动时把迁移跑到最新。可多次调用以聚合多个模块的迁移目录。
    pub fn migrate_on_start(mut self, dir: impl Into<String>) -> Self {
        self.migrations.push(dir.into());
        self.migrate_on_start = true;
        self
    }

    /// 只登记迁移目录，不自动跑。
    pub fn migrations(mut self, dir: impl Into<String>) -> Self {
        self.migrations.push(dir.into());
        self
    }

    /// 允许这些键热生效（结尾 `*` 为前缀匹配）。
    ///
    /// **不在这里列出的键，改了必须重启**——框架只会打一条 warning 提醒。
    /// 刻意不提供"全部热更"：连接串、worker 数这类键热更等于静默不生效，
    /// 比不支持更糟。
    pub fn hot_reload(mut self, patterns: &[&str]) -> Self {
        self.hot.extend(patterns.iter().map(|s| s.to_string()));
        self
    }

    /// 轮询间隔。默认 1 秒；测试可调小。
    pub fn reload_interval(mut self, d: Duration) -> Self {
        self.reload_interval = d;
        self
    }

    /// 组装。顺序：配置分层 → 库合并与默认推断 → 连库 → 模块名校验 → 热加载计划。
    pub fn build(self) -> Result<App> {
        let file_path = self.file.as_deref().map(Path::new);
        let (config, env, explicit) =
            layered::layered(&self.config, file_path, self.env_prefix.as_deref())?;

        if !explicit {
            // 只有提示，不改行为：release 跑在开发机、debug 跑在生产都是真实事故
            eprintln!(
                "[rsboot] 未显式声明环境，按编译模式推断为 `{}`。\
                 生产部署请设置 `app.env` 或环境变量 `RSBOOT_ENV`",
                env.name()
            );
        }

        let urls = databases::collect_urls(&config, &self.databases);
        let default_db = databases::resolve_default(&config, &urls)?;
        let sessions = databases::open(urls.clone())?;

        let mut seen = HashSet::new();
        for m in &self.modules {
            if !seen.insert(m.name()) {
                return Err(Error::Config(format!(
                    "模块名 `{}` 重复。模块名用于日志与诊断，必须唯一",
                    m.name()
                )));
            }
        }

        let reload_enabled = config.get_bool("reload.enabled")?.unwrap_or(false);
        let reload = match (&self.file, reload_enabled) {
            (Some(f), true) => Some(ReloadPlan {
                base: self.config.clone(),
                path: PathBuf::from(f),
                prefix: self.env_prefix.clone(),
                hot: self.hot.clone(),
                interval: self.reload_interval,
            }),
            _ => None,
        };

        // 下游服务在这里一次性建好：base_url 写错属于启动期错误，
        // 不该等到第一个请求进来才发现
        let mut registry = crate::service::ServiceRegistry::new();
        for (name, url) in self.services {
            let svc = crate::service::Service::from_url(&name, &url)?;
            registry.insert(svc);
        }

        let ctx = Context::new(config, sessions, default_db.clone(), env);
        if !registry.is_empty() {
            ctx.insert_state(registry.clone());
        }

        Ok(App {
            ctx,
            database_urls: urls,
            modules: self.modules,
            started: Vec::new(),
            migrations: self.migrations,
            migrate_on_start: self.migrate_on_start,
            reload,
            watcher: None,
            status: crate::server::ServeStatus::new(),
            services: registry,
        })
    }
}

impl std::fmt::Debug for Builder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Builder")
            .field("file", &self.file)
            .field(
                "databases",
                &self.databases.iter().map(|(n, _)| n).collect::<Vec<_>>(),
            )
            .field("modules", &self.modules.len())
            .field("migrations", &self.migrations)
            .field("hot", &self.hot)
            .finish()
    }
}
