//! 模块：应用里可插拔的一块功能。
//!
//! 对应 Spring 的 `@Configuration` + `InitializingBean`。区别在于这里**顺序是显式的**：
//! `App` 按注册顺序启动、按逆序关闭。Spring 靠 `@DependsOn`/构造注入推导顺序，
//! 代价是循环依赖时报错难懂；在 Rust 里依赖本来就得自己在 `main` 里拼，
//! 顺序写清楚反而更省事。
//!
//! ```ignore
//! struct Schema;
//!
//! impl Module for Schema {
//!     fn name(&self) -> &'static str { "schema" }
//!     fn start(&mut self, ctx: &Context) -> rsboot::Result<()> {
//!         ctx.with_db(|db| {
//!             db.execute_batch("CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY)")?;
//!             Ok(())
//!         })
//!     }
//! }
//! ```

use crate::runtime::context::Context;
use crate::error::Result;

pub trait Module {
    /// 名字用于日志与错误信息，必须唯一（`App::build` 会检查重名）。
    fn name(&self) -> &'static str;

    /// 启动钩子。失败即整个应用启动失败，已启动的模块会被逆序关闭。
    fn start(&mut self, ctx: &Context) -> Result<()>;

    /// 关闭钩子。默认什么都不做。
    ///
    /// 这里**不要**假设数据库一定可用：关闭阶段经常是数据库先挂了。
    /// 所以失败要返回错误（`App::stop` 会汇总），但不要 panic。
    fn stop(&mut self, ctx: &Context) -> Result<()> {
        let _ = ctx;
        Ok(())
    }
}
