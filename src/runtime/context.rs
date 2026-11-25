//! 运行时上下文：配置快照 + 已打开的数据库 + 模块共享状态。
//!
//! ## 配置为什么是 `RwLock<Arc<Config>>`
//!
//! 热加载需要在后台线程换掉整份配置，而 `&Config` 这种长期借用会挡住它。
//! 用 `Arc` 快照：一次请求里多次读配置拿到的是同一份（不会读到一半被换），
//! 替换是一次引用计数交换，不阻塞读者。代价是 `config()` 返回 `Arc<Config>`。
//!
//! ## 借用粒度是刻意设计的
//!
//! 每个数据库各自一个 `RefCell<Session<_>>`，而不是"整个 map 一个 RefCell"。
//! 后者会让"在 A 库的回调里访问 B 库"直接 panic，而多数据源场景下这恰恰是常态
//! （主库写完去读报表库）。现在只有**同一个库自己嵌套**才会报错，那本来就不该做。

use std::any::{Any, TypeId};
use std::cell::{Ref, RefCell};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use rsbatis::{Dialect, Engine, Session, Sql};

use crate::config::{Config, Environment};
use crate::error::{Error, Result};

pub struct Context {
    config: Arc<RwLock<Arc<Config>>>,
    env: Environment,
    databases: HashMap<String, RefCell<Session<Engine>>>,
    default_db: Option<String>,
    state: RefCell<HashMap<TypeId, Box<dyn Any>>>,
}

impl Context {
    pub fn new(
        config: Config,
        databases: Vec<(String, Session<Engine>)>,
        default_db: Option<String>,
        env: Environment,
    ) -> Self {
        let databases = databases
            .into_iter()
            .map(|(n, s)| (n, RefCell::new(s)))
            .collect();
        Self {
            config: Arc::new(RwLock::new(Arc::new(config))),
            env,
            databases,
            default_db,
            state: RefCell::new(HashMap::new()),
        }
    }

    /// 当前配置快照。热加载后自动指向新的一份。
    pub fn config(&self) -> Arc<Config> {
        match self.config.read() {
            Ok(g) => Arc::clone(&g),
            // 读锁被 poison 说明某个写线程 panic 了。此时旧值依然是完好的，
            // 继续服务比 panic 更好——热加载本来就是"不能把服务搞挂"的功能。
            Err(e) => Arc::clone(&e.into_inner()),
        }
    }

    /// 配置槽的共享句柄（热加载线程持有它来换快照）。
    pub(crate) fn config_slot(&self) -> Arc<RwLock<Arc<Config>>> {
        Arc::clone(&self.config)
    }

    /// 换一份配置（热加载用）。
    pub fn install_config(&self, next: Arc<Config>) {
        match self.config.write() {
            Ok(mut g) => *g = next,
            Err(e) => *e.into_inner() = next,
        }
    }

    pub fn env(&self) -> Environment {
        self.env
    }

    /// 已注册的库名（按字母序，输出稳定）。
    pub fn db_names(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.databases.keys().map(|s| s.as_str()).collect();
        v.sort();
        v
    }

    pub fn default_db(&self) -> Option<&str> {
        self.default_db.as_deref()
    }

    /// 在默认库上执行闭包。
    pub fn with_db<T>(&self, f: impl FnOnce(&mut Session<Engine>) -> Result<T>) -> Result<T> {
        let name = self.default_db.clone().ok_or_else(|| {
            Error::Missing(
                "没有默认数据库：用 `.database(\"default\", url)` 注册，或在配置文件里写 `[database] default = ...`"
                    .to_string(),
            )
        })?;
        self.with_db_named(&name, f)
    }

    /// 在指定的库上执行闭包。
    pub fn with_db_named<T>(
        &self,
        name: &str,
        f: impl FnOnce(&mut Session<Engine>) -> Result<T>,
    ) -> Result<T> {
        let cell = self.databases.get(name).ok_or_else(|| {
            Error::Missing(format!(
                "没有名为 `{name}` 的数据库。已注册：{}",
                if self.db_names().is_empty() {
                    "（无）".to_string()
                } else {
                    self.db_names().join("、")
                }
            ))
        })?;
        let mut db = cell.try_borrow_mut().map_err(|_| {
            Error::Lifecycle(format!(
                "数据库 `{name}` 正在被外层使用：不能在 with_db 回调里再访问同一个库。\
                 外层已持有会话，事务会互相看见。需要另一个库就用 `with_db_named` 指向别的名字"
            ))
        })?;
        f(&mut db)
    }

    pub fn dialect(&self, name: &str) -> Option<Dialect> {
        self.databases.get(name).map(|c| c.borrow().dialect())
    }

    /// 放一个跨模块共享的值（按类型取，同类型只能有一个）。
    ///
    /// 注意：这里是**进程级**状态。放进去的必须是"所有用户共享"的东西
    /// （连接池、注册表、缓存）。**认证结果不能放这里**——那是请求级的，
    /// 放进来就会把 A 用户的身份串到 B 用户身上，应放 `Request::extensions`。
    pub fn insert_state<T: Any>(&self, value: T) {
        self.state
            .borrow_mut()
            .insert(TypeId::of::<T>(), Box::new(value));
    }

    /// 取共享值。返回 `Ref`：借用期间持读锁，可以同时有多个读者。
    pub fn state<T: Any>(&self) -> Result<Ref<'_, T>> {
        if !self.has_state::<T>() {
            return Err(Error::Missing(format!(
                "没有注册类型 `{}` 的共享状态。先在某处 `ctx.insert_state(value)`",
                std::any::type_name::<T>()
            )));
        }
        let map = self.state.borrow();
        let boxed: Ref<'_, Box<dyn Any>> =
            Ref::map(map, |m| m.get(&TypeId::of::<T>()).expect("已检查存在"));
        Ok(Ref::map(boxed, |b| {
            b.as_ref()
                .downcast_ref::<T>()
                .expect("TypeId 匹配却 downcast 失败")
        }))
    }

    pub fn has_state<T: Any>(&self) -> bool {
        self.state.borrow().contains_key(&TypeId::of::<T>())
    }

    /// 探活：对每个库跑一次 `SELECT 1`，返回耗时。
    ///
    /// 用 `SELECT 1` 而不是 `SELECT 1 FROM <某张表>`：前者不依赖任何表存在，
    /// 迁移还没跑的空库也能探通——健康检查要回答的是"连接还能用吗"。
    pub fn health(&self) -> Vec<DbHealth> {
        let mut out = Vec::new();
        for name in self.db_names() {
            let started = std::time::Instant::now();
            let cell = self.databases.get(name).expect("名字来自同一个 map");
            let res = match cell.try_borrow_mut() {
                Ok(mut db) => db.fetch(Sql::raw("SELECT 1")).map(|_| ()),
                Err(_) => Err(rsbatis::DriverError::new(
                    rsbatis::ErrorKind::Connection,
                    "连接正被占用，无法探活",
                )),
            };
            out.push(DbHealth {
                name: name.to_string(),
                ok: res.is_ok(),
                message: res.err().map(|e| e.message),
                elapsed: started.elapsed(),
            });
        }
        out
    }
}

#[derive(Debug, Clone)]
pub struct DbHealth {
    pub name: String,
    pub ok: bool,
    pub message: Option<String>,
    pub elapsed: std::time::Duration,
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("env", &self.env.name())
            .field("databases", &self.db_names())
            .field("default_db", &self.default_db)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsbatis::create_engine;

    fn ctx() -> Context {
        Context::new(
            Config::new(),
            vec![(
                "default".to_string(),
                create_engine("sqlite://:memory:").unwrap(),
            )],
            Some("default".to_string()),
            Environment::Dev,
        )
    }

    #[test]
    fn with_db_runs_on_default() {
        let c = ctx();
        c.with_db(|db| {
            db.execute_batch("CREATE TABLE t (a INTEGER)")?;
            Ok(())
        })
        .unwrap();
        let rows = c
            .with_db(|db| Ok(db.fetch(Sql::raw("SELECT 1 AS a"))?))
            .unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn nested_same_db_is_refused_with_explanation() {
        let c = ctx();
        let e = c
            .with_db(|_db| c.with_db(|_| Ok(())))
            .unwrap_err();
        assert!(e.to_string().contains("正在被外层使用"), "{e}");
    }

    #[test]
    fn different_db_nesting_is_allowed() {
        let c = Context::new(
            Config::new(),
            vec![
                ("a".to_string(), create_engine("sqlite://:memory:").unwrap()),
                ("b".to_string(), create_engine("sqlite://:memory:").unwrap()),
            ],
            Some("a".to_string()),
            Environment::Dev,
        );
        c.with_db_named("a", |_| {
            c.with_db_named("b", |_| Ok(()))?;
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn missing_db_lists_registered_names() {
        let c = ctx();
        let e = c.with_db_named("nope", |_| Ok(())).unwrap_err();
        assert!(e.to_string().contains("default"), "{e}");
    }

    #[test]
    fn install_config_swaps_the_snapshot() {
        let c = ctx();
        let before = c.config();
        assert!(before.get("app.name").is_none());
        c.install_config(Arc::new(Config::new().set("app.name", "next")));
        assert_eq!(c.config().get("app.name"), Some("next"));
        assert!(before.get("app.name").is_none(), "旧快照不受影响");
    }

    #[test]
    fn health_reports_each_db() {
        let c = ctx();
        let h = c.health();
        assert_eq!(h.len(), 1);
        assert!(h[0].ok, "内存库应能连通：{:?}", h[0].message);
        assert_eq!(h[0].name, "default");
    }
}
