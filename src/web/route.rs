//! 路由表。
//!
//! ## 匹配策略：首段字面量分桶 + 段数预筛
//!
//! 1000 条路由时全表扫描是 1000 次字符串比较；分桶后 `/users/7` 只跟
//! 首段为 `users` 的那几条比。参数开头的路由（`/{tenant}/x`）单独放一个桶，
//! 因为它们的首段匹配任何值。
//!
//! 段数是第二道筛子：`/a/b` 不可能匹配 3 段的模板，连字符串比较都不用做。
//!
//! ## 405 不是 404
//!
//! 路径存在但方法不对，必须回 `405` + `Allow` 头。把这种情况回成 404 会让
//! 客户端以为是路径写错，而不去检查方法——这是很常见的框架缺陷。
//!
//! `GET` 自动带 `HEAD`（body 丢弃），`OPTIONS` 自动回 `204 + Allow`。
//! 这两件是 HTTP 规范要求的，让使用者手写只会漏。

use std::collections::HashMap;

use crate::error::{Error, Result};
use crate::runtime::context::Context;
use crate::web::error::ApiError;
use crate::web::exception;
use crate::web::http::{Method, Status};
use crate::web::path::PathTemplate;
use crate::web::request::Request;
use crate::web::response::{IntoResponse, Response};

pub type HandlerFn = Box<dyn Fn(&mut Request, &Context) -> Response>;

pub struct Route {
    pub method: Method,
    pub template: PathTemplate,
    handler: HandlerFn,
}

impl Route {
    pub fn path(&self) -> &str {
        &self.template.raw
    }
}

/// 路由注册与分派。
pub struct Router {
    routes: Vec<Route>,
    /// 首段字面量 → 路由下标。空串这个桶放"首段是参数"的路由。
    bucket: HashMap<String, Vec<usize>>,
    fallback: Option<HandlerFn>,
}

impl std::fmt::Debug for Router {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Router")
            .field("routes", &self.routes.len())
            .field("fallback", &self.fallback.is_some())
            .finish()
    }
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

impl Router {
    pub fn new() -> Self {
        Self {
            routes: Vec::new(),
            bucket: HashMap::new(),
            fallback: None,
        }
    }

    /// 注册一条路由。重复注册（同方法 + 同路径）当场报错。
    ///
    /// 在注册期报错而不是运行期：两条路由抢同一个路径时，先注册的那条会
    /// 静默获胜，这种"看起来生效了"的配置错误最难查。
    pub fn route<H, R>(mut self, method: Method, path: &str, handler: H) -> Result<Self>
    where
        H: Fn(&mut Request, &Context) -> R + 'static,
        R: IntoResponse + 'static,
    {
        let template = PathTemplate::parse(path).map_err(Error::Config)?;
        if self
            .routes
            .iter()
            .any(|r| r.method == method && r.template.raw == template.raw)
        {
            return Err(Error::Config(format!(
                "路由重复注册：{} {}。同一方法同一路径只能有一条",
                method.as_str(),
                template.raw
            )));
        }
        let key = template.first_literal().unwrap_or("").to_string();
        let idx = self.routes.len();
        self.routes.push(Route {
            method,
            template,
            // 在这里就把 `IntoResponse` 收成 `Response`：
            // 存储层只有一种类型，分派时不用再泛型化
            handler: Box::new(move |req, ctx| handler(req, ctx).into_response()),
        });
        self.bucket.entry(key).or_default().push(idx);
        Ok(self)
    }

    pub fn get<H, R>(self, path: &str, h: H) -> Result<Self>
    where
        H: Fn(&mut Request, &Context) -> R + 'static,
        R: IntoResponse + 'static,
    {
        self.route(Method::Get, path, h)
    }

    pub fn post<H, R>(self, path: &str, h: H) -> Result<Self>
    where
        H: Fn(&mut Request, &Context) -> R + 'static,
        R: IntoResponse + 'static,
    {
        self.route(Method::Post, path, h)
    }

    pub fn put<H, R>(self, path: &str, h: H) -> Result<Self>
    where
        H: Fn(&mut Request, &Context) -> R + 'static,
        R: IntoResponse + 'static,
    {
        self.route(Method::Put, path, h)
    }

    pub fn patch<H, R>(self, path: &str, h: H) -> Result<Self>
    where
        H: Fn(&mut Request, &Context) -> R + 'static,
        R: IntoResponse + 'static,
    {
        self.route(Method::Patch, path, h)
    }

    pub fn delete<H, R>(self, path: &str, h: H) -> Result<Self>
    where
        H: Fn(&mut Request, &Context) -> R + 'static,
        R: IntoResponse + 'static,
    {
        self.route(Method::Delete, path, h)
    }

    /// 没有匹配时的处理器。不设置就回标准的 404 JSON。
    pub fn fallback<H, R>(mut self, h: H) -> Self
    where
        H: Fn(&mut Request, &Context) -> R + 'static,
        R: IntoResponse + 'static,
    {
        self.fallback = Some(Box::new(move |req, ctx| h(req, ctx).into_response()));
        self
    }

    /// 已注册的路由（OpenAPI 生成用，按注册顺序）。
    pub fn list(&self) -> impl Iterator<Item = (&Method, &str)> {
        self.routes.iter().map(|r| (&r.method, r.template.raw.as_str()))
    }

    pub fn len(&self) -> usize {
        self.routes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    /// 分派。
    pub fn handle(&self, req: &mut Request, ctx: &Context) -> Response {
        let seg_count = segment_count(&req.path);
        let first = first_segment(&req.path);

        let mut candidates: Vec<usize> = Vec::new();
        if let Some(v) = self.bucket.get(&first) {
            candidates.extend_from_slice(v);
        }
        if !first.is_empty() {
            if let Some(v) = self.bucket.get("") {
                candidates.extend_from_slice(v);
            }
        }

        let mut allowed: Vec<Method> = Vec::new();
        for idx in candidates {
            let r = &self.routes[idx];
            if r.template.len() != seg_count {
                continue;
            }
            let Some(params) = r.template.matches(&req.path) else {
                continue;
            };

            let method_ok = r.method == req.method
                || (req.method == Method::Head && r.method == Method::Get)
                || (req.method == Method::Options && r.method == Method::Get);

            if !method_ok {
                if !allowed.contains(&r.method) {
                    allowed.push(r.method);
                }
                continue;
            }

            // OPTIONS：不执行 handler，直接回能力声明
            if req.method == Method::Options {
                if !allowed.contains(&r.method) {
                    allowed.push(r.method);
                }
                continue;
            }

            req.params = params;

            // 一个 handler panic 不能让 worker 线程退出。捕获后请求对象直接丢弃：
            // panic 之后它的内部状态不可信，继续用等于在赌。
            let trace = req.request_id().map(|s| s.to_string());
            let outcome = exception::with_trace(trace.as_deref(), || {
                exception::guard(|| (r.handler)(req, ctx))
            });
            let mut resp = match outcome {
                Ok(resp) => resp,
                Err(payload) => exception::panic_response(payload),
            };
            if req.method == Method::Head {
                // HEAD 的语义是"和 GET 一样的头，但没有 body"
                resp.body.clear();
                resp.headers.remove("content-length");
            }
            return resp;
        }

        if !allowed.is_empty() {
            if req.method == Method::Options {
                let mut resp = Response::new(Status::NO_CONTENT);
                resp.headers.insert("allow", &allow_list(&allowed));
                return resp;
            }
            let mut resp = ApiError::new(
                Status::METHOD_NOT_ALLOWED,
                format!("`{}` 不支持 {} 方法", req.path, req.method.as_str()),
            )
            .to_response();
            resp.headers.insert("allow", &allow_list(&allowed));
            return resp;
        }

        if let Some(fb) = &self.fallback {
            let trace = req.request_id().map(|s| s.to_string());
            return exception::with_trace(trace.as_deref(), || {
                match exception::guard(|| (fb)(req, ctx)) {
                    Ok(resp) => resp,
                    Err(payload) => exception::panic_response(payload),
                }
            });
        }
        ApiError::not_found(format!("没有匹配的路由：{} {}", req.method.as_str(), req.path))
            .to_response()
    }
}

/// `Allow` 头的值：显式方法 + 由 GET 派生的 HEAD + 恒有的 OPTIONS。
fn allow_list(allowed: &[Method]) -> String {
    let mut out: Vec<&str> = Vec::new();
    for m in allowed {
        let s = m.as_str();
        if !out.contains(&s) {
            out.push(s);
        }
    }
    if out.contains(&"GET") && !out.contains(&"HEAD") {
        out.push("HEAD");
    }
    if !out.contains(&"OPTIONS") {
        out.push("OPTIONS");
    }
    out.join(", ")
}

fn segment_count(path: &str) -> usize {
    let t = path.strip_prefix('/').unwrap_or(path);
    if t.is_empty() {
        0
    } else {
        t.split('/').count()
    }
}

fn first_segment(path: &str) -> String {
    let t = path.strip_prefix('/').unwrap_or(path);
    t.split('/').next().unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Environment;
    use crate::runtime::context::Context;
    use rsbatis::create_engine;

    fn ctx() -> Context {
        Context::new(
            crate::config::Config::new(),
            vec![(
                "default".to_string(),
                create_engine("sqlite://:memory:").unwrap(),
            )],
            Some("default".to_string()),
            Environment::Dev,
        )
    }

    fn router() -> Router {
        Router::new()
            .get("/", |_, _| Response::text("root"))
            .unwrap()
            .get("/users/{id}", |req, _| {
                Response::text(format!("user {}", req.param("id").unwrap_or("")))
            })
            .unwrap()
            .post("/users", |_, _| Response::text("created"))
            .unwrap()
    }

    fn call(r: &Router, method: Method, path: &str) -> Response {
        let mut req = Request::new(method, path);
        let c = ctx();
        r.handle(&mut req, &c)
    }

    #[test]
    fn root_and_literal_routes() {
        let r = router();
        assert_eq!(call(&r, Method::Get, "/").body, b"root");
        assert_eq!(call(&r, Method::Get, "/users/7").body, b"user 7");
        assert_eq!(call(&r, Method::Post, "/users").status, Status::OK);
    }

    #[test]
    fn missing_path_is_404() {
        let r = router();
        assert_eq!(call(&r, Method::Get, "/nope").status, Status::NOT_FOUND);
    }

    #[test]
    fn wrong_method_is_405_with_allow_header() {
        let r = router();
        let resp = call(&r, Method::Get, "/users");
        assert_eq!(resp.status, Status::METHOD_NOT_ALLOWED);
        let allow = resp.headers.get("allow").unwrap();
        assert!(allow.contains("POST"), "{allow}");
        assert!(allow.contains("OPTIONS"), "{allow}");
    }

    #[test]
    fn allow_header_includes_head_for_get_routes() {
        let r = router();
        let resp = call(&r, Method::Delete, "/users/7");
        let allow = resp.headers.get("allow").unwrap();
        assert!(allow.contains("GET"), "{allow}");
        assert!(allow.contains("HEAD"), "GET 必须带 HEAD：{allow}");
    }

    #[test]
    fn head_gets_get_route_without_body() {
        let r = router();
        let resp = call(&r, Method::Head, "/users/7");
        assert_eq!(resp.status, Status::OK);
        assert!(resp.body.is_empty(), "HEAD 不能有 body");
        assert!(resp.headers.get("content-length").is_none());
    }

    #[test]
    fn options_returns_204_with_allow() {
        let r = router();
        let resp = call(&r, Method::Options, "/users/7");
        assert_eq!(resp.status, Status::NO_CONTENT);
        assert!(resp.headers.get("allow").unwrap().contains("GET"));
    }

    #[test]
    fn duplicate_registration_is_refused() {
        let e = router()
            .get("/users/{id}", |_, _| Response::text("dup"))
            .unwrap_err();
        assert!(e.to_string().contains("重复注册"), "{e}");
    }

    #[test]
    fn same_path_different_methods_is_fine() {
        let r = Router::new()
            .get("/x", |_, _| Response::text("g"))
            .unwrap()
            .post("/x", |_, _| Response::text("p"))
            .unwrap();
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn param_first_route_is_matchable() {
        let r = Router::new()
            .get("/{tenant}/info", |req, _| {
                Response::text(req.param("tenant").unwrap_or("").to_string())
            })
            .unwrap();
        assert_eq!(call(&r, Method::Get, "/acme/info").body, b"acme");
    }

    #[test]
    fn fallback_is_used_when_nothing_matches() {
        let r = Router::new().fallback(|_, _| Response::text("custom-404"));
        assert_eq!(call(&r, Method::Get, "/x").body, b"custom-404");
    }

    #[test]
    fn handler_panic_becomes_500_and_router_keeps_serving() {
        let r = Router::new()
            .get("/boom", |_, _| -> Response { panic!("故意 panic") })
            .unwrap()
            .get("/ok", |_, _| Response::text("ok"))
            .unwrap();
        let resp = call(&r, Method::Get, "/boom");
        assert_eq!(resp.status, Status::INTERNAL_SERVER_ERROR);
        let body = String::from_utf8(resp.body).unwrap();
        assert!(!body.contains("故意"), "panic 原文不能进响应：{body}");
        // 关键：同一条路由表后续请求不受影响
        assert_eq!(call(&r, Method::Get, "/ok").body, b"ok");
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn handler_may_return_result() {
        let r = Router::new()
            .get("/missing", |_, _| {
                Err::<Response, ApiError>(ApiError::not_found("没有"))
            })
            .unwrap();
        assert_eq!(call(&r, Method::Get, "/missing").status, Status::NOT_FOUND);
    }

    #[test]
    fn bad_template_is_rejected_at_registration() {
        assert!(Router::new().get("no-slash", |_, _| Response::text("x")).is_err());
    }
}
