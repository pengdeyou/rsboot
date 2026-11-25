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
//!     fn start(&self, ctx: &Context) -> rsboot::Result<()> {
//!         ctx.with_db(|db| {
//!             db.execute_batch("CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY)")?;
//!             Ok(())
//!         })
//!     }
//! }
//! ```

use crate::runtime::context::Context;
use crate::error::Result;

/// ## 为什么全部是 `&self`，且要求 `Send + Sync`
///
/// 模块要被**每个 worker 引用一次**（`start_worker`），所以必然是共享的。
/// 早期版本用 `&mut self`，看起来省了锁，实际把"模块只能有一个执行者"写进了
/// 类型里——那正好和"每个 worker 都要初始化自己的东西"矛盾。
///
/// 需要可变状态的模块自己装 `Mutex`/`OnceLock`，或者把 worker 私有状态放进
/// `ctx.insert_state(...)`（`Context` 是每 worker 一份的，天然隔离）。
///
/// ## 两个启动钩子的分工
///
/// - `start`：**主线程一次**。建表、建全局缓存、注册定时任务、拉远程配置。
/// - `start_worker`：**每个 worker 一次**。拿到的是该 worker 自己的 `Context`，
///   里面有它自己的数据库连接。适合"把本线程要用的东西准备好"——预热连接、
///   填 thread-local、按核数分配容量。
///
/// 顺序是：`start`（全部模块正序）→ 起 worker（各自 `start_worker`）。
/// 也就是说 `start_worker` 一定发生在所有模块 `start` 之后。
pub trait Module: Send + Sync {
    /// 名字用于日志与错误信息，必须唯一（`App::build` 会检查重名）。
    fn name(&self) -> &'static str;

    /// 启动钩子（主线程一次）。失败即整个应用启动失败，已启动的模块会被逆序关闭。
    fn start(&self, ctx: &Context) -> Result<()>;

    /// 每个 worker 启动时调一次。拿到的 `ctx` 是该 worker 私有的。
    ///
    /// 失败会让**那个 worker** 退出（其余 worker 继续服务）。
    /// 这里不该做"主线程才该做"的事（建表、全局注册），那是 `start` 的职责。
    fn start_worker(&self, _ctx: &Context) -> Result<()> {
        Ok(())
    }

    /// 每个 worker 退出前调一次（`shutdown` 之后、线程结束之前）。
    fn stop_worker(&self, _ctx: &Context) -> Result<()> {
        Ok(())
    }

    /// 关闭钩子。默认什么都不做。
    ///
    /// 这里**不要**假设数据库一定可用：关闭阶段经常是数据库先挂了。
    /// 所以失败要返回错误（`App::stop` 会汇总），但不要 panic。
    fn stop(&self, ctx: &Context) -> Result<()> {
        let _ = ctx;
        Ok(())
    }
}
