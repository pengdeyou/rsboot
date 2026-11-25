//! 访问日志。
//!
//! 只打一行，字段固定：`方法 路径 状态 耗时 来源 trace`。
//!
//! ## 为什么不用 `log` / `tracing` 生态
//!
//! 那两个库要使用者自己配 subscriber、配格式、配输出目标。rsboot 的定位是
//! "开箱能用"：默认就有一行可读的访问日志，要接 ELK / OTLP 的人自己换掉这个
//! 中间件即可（`after` 里做，不用改框架）。
//!
//! ## 耗时放在 extensions 里
//!
//! 不能用中间件自己的字段：那是所有请求共享的，并发下会互相覆盖。
//! `extensions` 是**请求级**的，这是它能放 `Instant` 的原因。

use std::time::Instant;

use crate::runtime::context::Context;
use crate::web::middleware::Middleware;
use crate::web::request::Request;
use crate::web::response::Response;

/// 起点。用私有类型包一层，避免和业务往里放的 `Instant` 撞 TypeId。
struct StartedAt(Instant);

pub struct RequestLog {
    /// 静默开关：压测时关掉，否则日志本身成为瓶颈
    enabled: bool,
}

impl RequestLog {
    pub fn new() -> Self {
        Self { enabled: true }
    }

    pub fn silent() -> Self {
        Self { enabled: false }
    }
}

impl Default for RequestLog {
    fn default() -> Self {
        Self::new()
    }
}

impl Middleware for RequestLog {
    fn name(&self) -> &'static str {
        "log"
    }

    fn before(&self, req: &mut Request, _ctx: &Context) -> Option<Response> {
        if self.enabled {
            req.extensions.insert(StartedAt(Instant::now()));
        }
        None
    }

    fn after(&self, req: &Request, _ctx: &Context, resp: Response) -> Response {
        if !self.enabled {
            return resp;
        }
        let ms = req
            .extensions
            .get::<StartedAt>()
            .map(|s| s.0.elapsed().as_secs_f64() * 1000.0)
            .unwrap_or(0.0);
        let trace = req.request_id().unwrap_or("-");
        // 路径不含查询串：查询串里常有 token、手机号，原样落日志是泄露
        eprintln!(
            "[rsboot] {} {} {} {:.1}ms trace={}",
            req.method.as_str(),
            req.path,
            resp.status.0,
            ms,
            trace
        );
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Environment};
    use crate::web::http::Method;

    fn ctx() -> Context {
        Context::new(Config::new(), vec![], None, Environment::Dev)
    }

    #[test]
    fn records_start_and_keeps_response() {
        let m = RequestLog::new();
        let mut req = Request::new(Method::Get, "/a");
        assert!(m.before(&mut req, &ctx()).is_none());
        assert!(req.extensions.get::<StartedAt>().is_some());
        let out = m.after(&req, &ctx(), Response::text("x"));
        assert_eq!(out.body, b"x");
    }

    #[test]
    fn silent_mode_does_nothing() {
        let m = RequestLog::silent();
        let mut req = Request::new(Method::Get, "/a");
        assert!(m.before(&mut req, &ctx()).is_none());
        assert!(req.extensions.get::<StartedAt>().is_none());
    }
}
