//! `App::serve`：把路由表接到真实 socket 上。
//!
//! ## 为什么用工厂而不是把 `self.ctx` 交给 worker
//!
//! `Context` 里有 `RefCell<Session<Engine>>`——`Session` 是 `!Send`（SQLite
//! 句柄不能跨线程）。所以每个 worker 必须在自己线程里重新 `create_engine`。
//! 工厂闭包只捕获 URL 列表、默认库名、环境，以及**配置槽的 `Arc`**。
//!
//! 配置槽是共享的（`Arc<RwLock<Arc<Config>>>`）：热加载换一次快照，
//! 所有 worker 立刻看到新配置。如果每个 worker 各持一份配置副本，
//! 热加载只改到主线程那一份，改动看起来"成功"了却毫无效果。

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::runtime::app::databases;
use crate::runtime::app::App;
use crate::runtime::context::Context;
use crate::server::conn::Limits;
use crate::server::{ContextFactory, Server};
use crate::web::route::Router;

impl App {
    /// 起服务，地址取配置 `http.bind`。会响应 SIGINT/SIGTERM。
    ///
    /// 故意不提供默认地址：prod 的默认配置里没有 `http.bind`，缺了就报错。
    /// "忘了配就悄悄监听 0.0.0.0:8000" 在云上是安全事故。
    pub fn serve(&mut self, router: Router) -> Result<()> {
        let addr = self.bind_addr()?;
        self.serve_on(&addr, router)
    }

    /// 从配置读出监听地址。
    ///
    /// prod 的默认配置里**故意没有** `http.bind`，所以这里会失败——把"忘了配"
    /// 变成启动期错误，而不是悄悄监听 `0.0.0.0:8000` 上线。
    pub fn bind_addr(&self) -> Result<String> {
        self.ctx
            .config()
            .get("http.bind")
            .map(str::to_string)
            .ok_or_else(|| {
                Error::Config(format!(
                    "没有监听地址：配置里缺 `http.bind`（当前环境 `{}`）。\
                     显式写 `http.bind: 127.0.0.1:8000`，或用 `serve_on(addr, router)`",
                    self.ctx.env().name()
                ))
            })
    }

    /// 起服务，地址由调用方给。
    ///
    /// 阻塞运行，直到收到 `SIGINT`（Ctrl+C）/ `SIGTERM`（容器停止）并完成优雅关闭。
    /// 需要程序内部控制退出（测试、灰度下线）请用 [`App::serve_with`]。
    pub fn serve_on(&mut self, addr: &str, router: Router) -> Result<()> {
        crate::server::signal::install();
        self.serve_with(addr, router, Arc::new(AtomicBool::new(false)))
    }

    /// 起服务，外部可以通过 `shutdown` 编排优雅关闭（接 SIGTERM / 测试）。
    pub fn serve_with(
        &mut self,
        addr: &str,
        router: Router,
        shutdown: Arc<AtomicBool>,
    ) -> Result<()> {
        // 模块、迁移、热加载都走正常启动流程
        self.start()?;

        let limits = self.limits_from_config()?;
        let workers = self.workers_from_config()?;
        let grace = self.grace_from_config()?;
        let drain = self.drain_from_config()?;
        let factory = self.context_factory();
        // worker 退出钩子：模块的 stop_worker。逆序释放，与启动相反。
        let exit = self.worker_exit_hook();

        // 中间件按配置装（cors / rate / security / log），使用者自己加的排在外层
        let cfg = self.ctx.config();
        let router = match crate::web::middleware::install_defaults(router, &cfg) {
            Ok(r) => r,
            Err(e) => return Err(self.unwind(e)),
        };

        // 文档页与 `/openapi.json`（`http.docs`，dev 默认开、prod 默认关）
        let docs_on = match cfg.get_bool("http.docs") {
            Ok(v) => v.unwrap_or(false),
            Err(e) => return Err(self.unwind(e)),
        };
        let router = if docs_on {
            crate::web::openapi::attach(router, crate::web::openapi::OpenApi::from_config(&cfg))
        } else {
            router
        };
        let router = Arc::new(router);

        // 端口被占之类的失败也要把已启动的模块收干净，不能带着半启动状态退出
        let server = match Server::bind(addr, Arc::clone(&router), factory, limits, workers) {
            Ok(s) => s
                .on_worker_exit(exit)
                .grace(grace)
                .drain(drain)
                .status(self.status.clone()),
            Err(e) => return Err(self.unwind(e)),
        };

        let result = server.run(shutdown);
        let stopped = self.stop();
        match (result, stopped) {
            (Err(e), _) => Err(e),
            (Ok(()), Err(e)) => Err(e),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    /// 每个 worker 一份 Context 的工厂。
    fn context_factory(&self) -> ContextFactory {
        let urls = self.database_urls.clone();
        let default_db = self.ctx.default_db().map(str::to_string);
        let slot = self.ctx.config_slot();
        let env = self.ctx.env();

        let modules = Arc::new(self.modules.clone());
        let status = self.status.clone();
        let services = self.services.clone();

        Arc::new(move || {
            let sessions = databases::open(urls.clone())?;
            let ctx = Context::from_shared(
                Arc::clone(&slot),
                sessions,
                default_db.clone(),
                env,
            );
            // 每个 worker 的 Context 里放同一个状态句柄：
            // handler 读到的 Draining/Closing 就是主线程此刻的真实状态。
            ctx.insert_state(status.clone());
            if !services.is_empty() {
                ctx.insert_state(services.clone());
            }
            // 模块的 worker 钩子：正序启动。失败就让这个 worker 退出，
            // 其余 worker 继续服务（一个 worker 起不来 ≠ 整个服务起不来）。
            for m in modules.iter() {
                m.start_worker(&ctx).map_err(|e| {
                    Error::Lifecycle(format!("模块 `{}` 在 worker 启动失败：{e}", m.name()))
                })?;
            }
            Ok(ctx)
        })
    }

    /// worker 退出前跑模块的 `stop_worker`，逆序。
    fn worker_exit_hook(&self) -> crate::server::WorkerExitHook {
        let modules = Arc::new(self.modules.clone());
        Arc::new(move |ctx: &Context| {
            for m in modules.iter().rev() {
                if let Err(e) = m.stop_worker(ctx) {
                    // 关闭阶段的错误只记日志：此时再返回错误也没有人能处理
                    eprintln!("[rsboot] 模块 `{}` 在 worker 关闭时出错：{e}", m.name());
                }
            }
        })
    }

    fn grace_from_config(&self) -> Result<std::time::Duration> {
        let secs = self
            .ctx
            .config()
            .get_int("http.shutdown_grace_secs")?
            .unwrap_or(10);
        if secs < 0 {
            return Err(Error::Config(format!(
                "`http.shutdown_grace_secs` 不能是负数，当前 {secs}"
            )));
        }
        Ok(std::time::Duration::from_secs(secs as u64))
    }

    /// 跛鸭期。prod 默认 5 秒：K8s 删 Pod 时从 endpoints 摘掉到真正断流
    /// 有个滞后，立刻停 accept 会把正在被派过来的请求打空。
    fn drain_from_config(&self) -> Result<std::time::Duration> {
        let secs = self
            .ctx
            .config()
            .get_int("http.drain_secs")?
            .unwrap_or(0);
        if secs < 0 {
            return Err(Error::Config(format!(
                "`http.drain_secs` 不能是负数，当前 {secs}"
            )));
        }
        Ok(std::time::Duration::from_secs(secs as u64))
    }

    fn limits_from_config(&self) -> Result<Limits> {
        let cfg = self.ctx.config();
        let mut limits = Limits::default();
        if let Some(n) = cfg.get_int("http.max_body_bytes")? {
            if n <= 0 {
                return Err(Error::Config(format!(
                    "`http.max_body_bytes` 必须是正数，当前 {n}"
                )));
            }
            limits.max_body = n as usize;
        }
        if let Some(secs) = cfg.get_int("http.idle_timeout_secs")? {
            if secs <= 0 {
                return Err(Error::Config(format!(
                    "`http.idle_timeout_secs` 必须是正数，当前 {secs}"
                )));
            }
            limits.io_timeout = std::time::Duration::from_secs(secs as u64);
        }
        if let Some(n) = cfg.get_int("http.max_requests")? {
            limits.max_requests = (n.max(1)) as usize;
        }
        Ok(limits)
    }

    /// worker 数。默认 = CPU 核数，上限 64（再多也压不出更多 IO 并行）。
    fn workers_from_config(&self) -> Result<usize> {
        let mut n = match self.ctx.config().get_int("http.workers")? {
            Some(v) if v <= 0 => {
                return Err(Error::Config(format!(
                    "`http.workers` 必须是正数，当前 {v}；想用默认值就删掉这一行"
                )))
            }
            Some(v) => v as usize,
            None => std::thread::available_parallelism()
                .map(|p| p.get())
                .unwrap_or(1),
        };
        // 上限 64：每个 worker 一份连接池，超过这个数只会把数据库连接打爆
        if n > 64 {
            n = 64;
        }
        Ok(n)
    }
}
