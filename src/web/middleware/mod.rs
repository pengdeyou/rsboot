//! 中间件：横切关注点，不进 handler。
//!
//! ## 为什么是"前后两段钩子"而不是嵌套洋葱
//!
//! 洋葱模型（`next: &dyn Fn(Request) -> Response`）看起来更通用，但在 Rust 里
//! 会立刻遇到三个麻烦：`Request` 的所有权在链上只能移动一次；每层要么泛型化
//! （编译期展开成巨型类型），要么装箱（每次请求多一次间接调用）；异步化时
//! 还得讨论 `Pin<Box<dyn Future>>`。
//!
//! 前后钩子把能做的事收窄成两件：**拦在前面短路**、**改后面的响应**。
//! cors / log / security / limit 四类需求全部落在这一收窄范围内，而实现只是
//! 一个 for 循环。代价是不能"包住 handler 执行期间"（比如在 handler 跑的过程中
//! 做超时中断），这类需求交给 S6 的连接层，而不是塞进中间件。
//!
//! ## 顺序
//!
//! `before` 按注册顺序（先注册的是外层），`after` 逆序。
//! 也就是说：外层的 `after` 最后跑，看到的是内层已经处理过的响应。
//!
//! ## 错误响应同样过 `after`
//!
//! CORS 头必须加在 4xx/5xx 上，否则浏览器里只看到"请求失败"，看不到真正原因。
//! 这就是错误路径不能绕过 `after` 的原因。

use crate::runtime::context::Context;
use crate::web::request::Request;
use crate::web::response::Response;

pub trait Middleware: Send + Sync {
    fn name(&self) -> &'static str;

    /// 在分派前。返回 `Some` 表示**短路**：后续中间件与 handler 都不执行。
    fn before(&self, _req: &mut Request, _ctx: &Context) -> Option<Response> {
        None
    }

    /// 在响应成形后（含 404 / 405 / panic / 短路响应）。
    fn after(&self, _req: &Request, _ctx: &Context, resp: Response) -> Response {
        resp
    }
}

pub mod cors;
pub mod limit;
pub mod log;
pub mod security;

pub use cors::Cors;
pub use limit::RateLimit;
pub use log::RequestLog;
pub use security::SecurityHeaders;

/// 按配置装一批默认中间件。`App` 在 `serve_with` 里调用它，
/// 使用者要自定义就自己再 `.middleware(...)`，顺序由使用者决定。
///
/// 装的条件：`http.cors` 有配置才装 CORS（没配就不该有 CORS 头）；
/// 安全头默认装（几乎没有代价，且"忘了加"是常见事故）；
/// 限流只在下限（rate）有配置时装。
pub fn install_defaults(
    router: crate::web::route::Router,
    config: &crate::config::Config,
) -> crate::error::Result<crate::web::route::Router> {
    use crate::error::Error;

    let mut router = router;

    if let Some(cors) = cors::Cors::from_config(config)? {
        router = router.middleware(cors);
    }
    if let Some(limit) = limit::RateLimit::from_config(config)? {
        router = router.middleware(limit);
    }
    router = router.middleware(security::SecurityHeaders::from_config(config)?);
    if config
        .get_bool("http.log.requests")
        .map_err(|e| Error::Config(e.to_string()))?
        .unwrap_or(true)
    {
        router = router.middleware(log::RequestLog::new());
    }
    Ok(router)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::web::http::Status;

    struct Noop;
    impl Middleware for Noop {
        fn name(&self) -> &'static str {
            "noop"
        }
    }

    #[test]
    fn defaults_are_non_intrusive() {
        // 空实现不该改变任何东西：before 不短路，after 原样返回
        let ctx = Context::new(Config::new(), vec![], None, crate::config::Environment::Dev);
        let mut req = Request::new(crate::web::http::Method::Get, "/x");
        assert!(Noop.before(&mut req, &ctx).is_none());
        let r = Noop.after(&req, &ctx, Response::text("x"));
        assert_eq!(r.status, Status::OK);
        assert_eq!(r.body, b"x");
    }
}
