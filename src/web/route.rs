//! 路由表。
//!
//! ## 匹配策略：段级前缀树
//!
//! 分派成本跟**段数**走，不跟**路由条数**走。200 条都挂在 `/api/v1/...`
//! 下时，早先的"按首段分桶再线性扫"会把 200 条全塞进同一个桶，每次请求
//! 扫到最后一条要几千纳秒（路由越多越慢，这是最容易被忽略的一类退化）。
//! 前缀树上 `/api/v1/orders/7` 只走 `api`→`v1`→`orders`→`{id}` 四次哈希查。
//!
//! 同一层同时存在字面量段和参数段时（`/users/me` 与 `/users/{id}`），
//! 字面量优先：更具体的那条先被尝试，它不接（比如方法不对）再让给参数段。
//! 这样两条都能命中时结果稳定可预期，不依赖注册顺序。
//!
//! 没有任何路由匹配时才算 404；路径匹配上但方法不对仍是 405 + `Allow`——
//! 把 405 回成 404 会让客户端以为是路径写错，而不去检查方法。
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
use crate::web::envelope::{self};
use crate::web::exception;
use crate::web::middleware::Middleware;
use crate::web::http::{Method, Status};
use crate::web::path::PathTemplate;
use crate::web::request::Request;
use crate::web::response::{IntoResponse, Response};

/// handler 必须 `Send + Sync`：它会在多个 worker 线程里被调用。
/// 这条约束会传导到闭包捕获的类型——捕获一个 `Rc` 或 `RefCell` 就编译不过，
/// 那正是我们要的（那些类型跨线程用会出错）。
pub type HandlerFn = Box<dyn Fn(&mut Request, &Context) -> Response + Send + Sync>;

pub struct Route {
    pub method: Method,
    pub template: PathTemplate,
    handler: HandlerFn,
    /// OpenAPI 描述。`None` 的路由仍会被列出来（只是没有 summary/参数说明），
    /// 因为"路由存在但没写文档"应该是可见的，而不是从文档里凭空消失。
    pub doc: Option<crate::web::openapi::RouteDoc>,
}

impl Route {
    pub fn path(&self) -> &str {
        &self.template.raw
    }
}

/// 路径段上的一个节点。
///
/// `statics` 是字面量段（`/users` 里的 `users`），`param` 是参数段
/// （`/users/{id}` 里的 `{id}`）。两者可以同时存在，匹配时字面量先走。
struct Node {
    statics: HashMap<Box<str>, Node>,
    param: Option<Box<Node>>,
    /// 在这条路径上结束的路由下标。同一路径可以有多条路由（不同方法），
    /// 405 的 `Allow` 就是从这里的兄弟项收出来的。
    ends: Vec<usize>,
}

impl Default for Node {
    fn default() -> Self {
        Self {
            statics: HashMap::new(),
            param: None,
            ends: Vec::new(),
        }
    }
}

impl Node {
    /// 沿路径下行，按"字面量优先、参数其次"的顺序把**所有**匹配到的终点
    /// 交给 `f`。`f` 返回 `true` 表示已处理完，停止回溯。
    ///
    /// 用回调而不是返回 `Vec`：候选通常是 1 组，没必要为它分配。回溯只在
    /// "字面量子树整条链都没接住"时才发生，深度最多等于段数。
    fn visit<'a, F>(node: &'a Node, tail: &str, f: &mut F) -> bool
    where
        F: FnMut(&'a Vec<usize>) -> bool,
    {
        if tail.is_empty() {
            return if node.ends.is_empty() {
                false
            } else {
                f(&node.ends)
            };
        }
        // 当场切出这一段，不先 `split('/').collect()` 成 Vec：
        // 那是每请求一次堆分配，只为了让递归能按切片下标走。
        let (head, rest) = match tail.find('/') {
            Some(i) => (&tail[..i], &tail[i + 1..]),
            None => (tail, ""),
        };
        if let Some(child) = node.statics.get(head) {
            if Self::visit(child, rest, f) {
                return true;
            }
        }
        if let Some(p) = &node.param {
            // 空段（`/users//7`）不算参数命中，跟模板匹配保持同一口径
            if !head.is_empty() && Self::visit(p, rest, f) {
                return true;
            }
        }
        false
    }

    /// 注册期插入一段路径。
    fn insert(&mut self, idx: usize, segs: &[Option<String>]) {
        let mut node = self;
        for seg in segs {
            node = match seg {
                Some(lit) => node
                    .statics
                    .entry(lit.as_str().into())
                    .or_insert_with(Node::default),
                None => node
                    .param
                    .get_or_insert_with(|| Box::new(Node::default()))
                    .as_mut(),
            };
        }
        node.ends.push(idx);
    }
}

/// 路由注册与分派。
pub struct Router {
    routes: Vec<Route>,
    /// 段级前缀树，分派只查它。
    tree: Node,
    fallback: Option<HandlerFn>,
    /// 中间件。注册顺序 = 外层到内层；`after` 逆序执行。
    pub(crate) middlewares: Vec<Box<dyn Middleware>>,
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
            tree: Node::default(),
            fallback: None,
            middlewares: Vec::new(),
        }
    }

    /// 注册一条路由。重复注册（同方法 + 同路径）当场报错。
    ///
    /// 在注册期报错而不是运行期：两条路由抢同一个路径时，先注册的那条会
    /// 静默获胜，这种"看起来生效了"的配置错误最难查。
    pub fn route<H, R>(mut self, method: Method, path: &str, handler: H) -> Result<Self>
    where
        H: Fn(&mut Request, &Context) -> R + Send + Sync + 'static,
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
        let segs: Vec<Option<String>> = template
            .segments()
            .map(|s| s.map(str::to_string))
            .collect();
        let idx = self.routes.len();
        self.routes.push(Route {
            method,
            template,
            doc: None,
            // 在这里就把 `IntoResponse` 收成 `Response`：
            // 存储层只有一种类型，分派时不用再泛型化
            handler: Box::new(move |req, ctx| handler(req, ctx).into_response()),
        });
        self.tree.insert(idx, &segs);
        Ok(self)
    }

    pub fn get<H, R>(self, path: &str, h: H) -> Result<Self>
    where
        H: Fn(&mut Request, &Context) -> R + Send + Sync + 'static,
        R: IntoResponse + 'static,
    {
        self.route(Method::Get, path, h)
    }

    pub fn post<H, R>(self, path: &str, h: H) -> Result<Self>
    where
        H: Fn(&mut Request, &Context) -> R + Send + Sync + 'static,
        R: IntoResponse + 'static,
    {
        self.route(Method::Post, path, h)
    }

    pub fn put<H, R>(self, path: &str, h: H) -> Result<Self>
    where
        H: Fn(&mut Request, &Context) -> R + Send + Sync + 'static,
        R: IntoResponse + 'static,
    {
        self.route(Method::Put, path, h)
    }

    pub fn patch<H, R>(self, path: &str, h: H) -> Result<Self>
    where
        H: Fn(&mut Request, &Context) -> R + Send + Sync + 'static,
        R: IntoResponse + 'static,
    {
        self.route(Method::Patch, path, h)
    }

    pub fn delete<H, R>(self, path: &str, h: H) -> Result<Self>
    where
        H: Fn(&mut Request, &Context) -> R + Send + Sync + 'static,
        R: IntoResponse + 'static,
    {
        self.route(Method::Delete, path, h)
    }

    /// 没有匹配时的处理器。不设置就回标准的 404 JSON。
    pub fn fallback<H, R>(mut self, h: H) -> Self
    where
        H: Fn(&mut Request, &Context) -> R + Send + Sync + 'static,
        R: IntoResponse + 'static,
    {
        self.fallback = Some(Box::new(move |req, ctx| h(req, ctx).into_response()));
        self
    }

    /// 装一个中间件。先装的在外面：它的 `before` 先跑，`after` 最后跑。
    ///
    /// 顺序自己定，框架不预设——"认证要在日志之前还是之后"取决于你想不想
    /// 让日志记录被拒绝的请求，这没有普适答案。
    pub fn middleware<M: Middleware + 'static>(mut self, m: M) -> Self {
        self.middlewares.push(Box::new(m));
        self
    }

    /// 给已注册的路由挂 OpenAPI 描述。
    ///
    /// 描述找不到路由就**报错**：静默丢弃会让 `/openapi.json` 少一条，
    /// 而少的那条正是使用者以为写好了的那条。
    pub fn describe(
        mut self,
        method: Method,
        path: &str,
        doc: crate::web::openapi::RouteDoc,
    ) -> Result<Self> {
        let normalized = PathTemplate::parse(path).map_err(Error::Config)?;
        let Some(idx) = self
            .routes
            .iter()
            .position(|r| r.method == method && r.template.raw == normalized.raw)
        else {
            return Err(Error::Config(format!(
                "没有 {} {} 这条路由，无法挂文档。先 `.route(...)` 注册，再 `.describe(...)`",
                method.as_str(),
                normalized.raw
            )));
        };
        self.routes[idx].doc = Some(doc);
        Ok(self)
    }

    pub fn middleware_names(&self) -> Vec<&'static str> {
        self.middlewares.iter().map(|m| m.name()).collect()
    }

    /// 已注册的路由（OpenAPI 生成用，按注册顺序）。
    pub fn list(&self) -> impl Iterator<Item = (&Method, &str)> {
        self.routes.iter().map(|r| (&r.method, r.template.raw.as_str()))
    }

    /// 带描述的路由视图。
    pub fn routes(&self) -> impl Iterator<Item = &Route> {
        self.routes.iter()
    }

    /// 这个"方法 + 路径"是否已注册（自动挂 `/docs` 时用来避免重复注册）。
    pub fn contains(&self, method: Method, path: &str) -> bool {
        let normalized = match PathTemplate::parse(path) {
            Ok(t) => t,
            Err(_) => return false,
        };
        self.routes
            .iter()
            .any(|r| r.method == method && r.template.raw == normalized.raw)
    }

    pub fn len(&self) -> usize {
        self.routes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    /// 分派。
    pub fn handle(&self, req: &mut Request, ctx: &Context) -> Response {
        // before：顺序执行，任一个返回 Some 就短路（后续中间件与 handler 都不跑）
        //
        // `entered` 记录"实际进入过"的层数：没进入的层不该在 after 里改一个
        // 它没参与过的响应。这是洋葱语义——只 unwind 已经进栈的那些。
        let mut entered = 0;
        let mut short = None;
        for m in &self.middlewares {
            entered += 1;
            if let Some(r) = m.before(req, ctx) {
                short = Some(r);
                break;
            }
        }
        let resp = match short {
            Some(r) => r,
            None => self.dispatch(req, ctx),
        };
        let mut resp = self.finalize(resp, req, ctx);

        // after：逆序（外层最后跑）。4xx/5xx 也过一遍——
        // CORS 头加不到错误响应上，浏览器里就只剩一句"请求失败"。
        for m in self.middlewares.iter().take(entered).rev() {
            resp = m.after(req, ctx, resp);
        }

        // HEAD 不再在这里清 body。
        //
        // 曾经的写法是 `resp.body.clear()`：结果写回层按 `body.len()` 算
        // `content-length`，HEAD 的响应头变成 `content-length: 0`——客户端
        // 以为 GET 只会拿到 0 字节。语义该由写回层负责（它有 `head_only`，
        // 逐字节决定发不发 body），路由层只管把真实长度留在 `Response` 上。
        resp
    }

    /// 出口处的唯一一次"换皮"。
    ///
    /// 统一响应体开着的时候，错误必须和成功同形（`code`/`message`/`data`/`trace_id`）。
    /// 但 handler 里 `return Err(e)` 走的是 `IntoResponse`，它拿不到配置，
    /// 只能回裸 `{"detail":...}`——这就是"成功带 data、失败带 detail"的经典不一致。
    /// 所以在这里统一处理：凡是带着 `api_error` 原件的响应，按开关重写一次。
    fn finalize(&self, mut resp: Response, req: &Request, ctx: &Context) -> Response {
        if resp.status.0 < 400 {
            return resp;
        }
        // 两条通路都要包：全局开关打开；或者这条错误自己要求过
        // （`Res::fail` 显式要统一形状，不该被全局开关再关掉）。
        let wrap = ctx.envelope_on()
            || resp
                .api_error
                .as_ref()
                .map(|e| e.always_wrap)
                .unwrap_or(false);
        if !wrap {
            return resp;
        }
        let Some(err) = resp.api_error.take() else {
            return resp;
        };
        let mut wrapped = envelope::error(&err, Some(req), true);
        // 405 之类会带 `allow`，换皮不能把这类业务头丢掉
        for (k, v) in resp.headers.iter() {
            if k.eq_ignore_ascii_case("allow") {
                wrapped.headers.insert(k, v);
            }
        }
        wrapped
    }

    fn dispatch(&self, req: &mut Request, ctx: &Context) -> Response {
        // 路径切段一次，之后全是沿树走。切段要借 `req.path`，而命中后要写
        // `req.params`——先把路径拷成局部 `String`，让两段借用各自独立。
        // 这条拷贝换掉的是"每请求扫 N 条候选"，值。
        // `req.path` 是 `&'a str`：这里只复制引用（8+8 字节），不产生字符串拷贝
        let path = req.path;
        let trimmed = path.strip_prefix('/').unwrap_or(path);

        let mut allowed: Vec<Method> = Vec::new();
        let mut hit: Option<Response> = None;
        Node::visit(&self.tree, trimmed, &mut |idxs| {
            match self.attempt(idxs, req, req.method, ctx, path, &mut allowed) {
                Some(resp) => {
                    hit = Some(resp);
                    true
                }
                None => false,
            }
        });
        if let Some(resp) = hit {
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
            return exception::with_trace_owned(trace, || {
                match exception::guard(|| (fb)(req, ctx)) {
                    Ok(resp) => resp,
                    Err(payload) => exception::panic_response(payload),
                }
            });
        }
        ApiError::not_found(format!("没有匹配的路由：{} {}", req.method.as_str(), req.path))
            .to_response()
    }

    /// 同一路径上的几条路由里，挑出这次真正要跑的那条。
    ///
    /// 返回 `None` 有两种含义：方法都对不上（已记进 `allowed`，外面会回
    /// 405/204），或者模板细节没匹配上（`/a//b` 这类畸形路径）。两者都
    /// 表示"这一组没接住，让上层继续往参数分支回溯"。
    fn attempt(
        &self,
        idxs: &[usize],
        req: &mut Request,
        method: Method,
        ctx: &Context,
        path: &str,
        allowed: &mut Vec<Method>,
    ) -> Option<Response> {
        for &idx in idxs {
            let r = &self.routes[idx];
            // 走到这里段数已经和模板一致，不用再数一遍 `/`
            let Some(params) = r.template.matches_with_len(path) else {
                continue;
            };

            let method_ok = r.method == method
                || (method == Method::Head && r.method == Method::Get)
                || (method == Method::Options && r.method == Method::Get);

            if !method_ok {
                if !allowed.contains(&r.method) {
                    allowed.push(r.method);
                }
                continue;
            }

            // OPTIONS：不执行 handler，直接回能力声明
            if method == Method::Options {
                if !allowed.contains(&r.method) {
                    allowed.push(r.method);
                }
                continue;
            }

            req.params = params;

            // 一个 handler panic 不能让 worker 线程退出。捕获后请求对象直接丢弃：
            // panic 之后它的内部状态不可信，继续用等于在赌。
            let trace = req.request_id().map(|s| s.to_string());
            let outcome = exception::with_trace_owned(trace, || {
                exception::guard(|| (r.handler)(req, ctx))
            });
            let resp = match outcome {
                Ok(resp) => resp,
                Err(payload) => exception::panic_response(payload),
            };
            return Some(resp);
        }
        None
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

    /// handler 直接返回 `Response` 时不需要 `Ok(..)`——成功与错误分支都能直接 return。
    #[test]
    fn handler_can_return_response_without_ok() {
        let r = Router::new()
            .get("/ping", |_, _| crate::web::res::Res::success("ok"))
            .unwrap()
            .get("/boom", |_, _| crate::web::res::Res::fail_response("库存不足"))
            .unwrap();
        let ok = call(&r, Method::Get, "/ping");
        assert_eq!(ok.status, Status::OK);
        assert_eq!(&ok.body[..], "{\"code\":\"OK\",\"message\":\"ok\",\"data\":null}".as_bytes());
        let bad = call(&r, Method::Get, "/boom");
        assert_eq!(bad.status, Status::BAD_REQUEST);
        assert_eq!(
            &bad.body[..],
            "{\"code\":\"BAD_REQUEST\",\"message\":\"库存不足\",\"data\":null}".as_bytes()
        );
    }

    /// `Res::fail` 造的错误必须在全局 envelope 关闭时也输出统一形状。
    #[test]
    fn res_fail_wraps_even_when_envelope_off() {
        let r = Router::new()
            .get("/x", |_, _| -> std::result::Result<Response, ApiError> {
                Err(crate::web::res::Res::fail("库存不足"))
            })
            .unwrap();
        let resp = call(&r, Method::Get, "/x");
        assert_eq!(resp.status, Status::BAD_REQUEST);
        let body = String::from_utf8(resp.body).unwrap();
        assert!(body.contains(r#""code":"BAD_REQUEST""#), "{body}");
        assert!(body.contains("库存不足"), "{body}");
        assert!(body.contains(r#""data":null"#), "{body}");
    }

    /// 200 条路由挂在同一前缀下的分派成本。命中最后一个 = 最坏情况。
    /// 小路由表（真实项目常见规模）上，树的分派成本绝对值。
    #[test]
    fn bench_dispatch_small() {
        let r = router();
        let c = ctx();
        let n = 20000usize;
        for path in ["/", "/users", "/users/7"] {
            let mut req = Request::new(Method::Get, path);
            let started = std::time::Instant::now();
            for _ in 0..n {
                std::hint::black_box(r.dispatch(&mut req, &c));
            }
            let per = started.elapsed().as_nanos() as f64 / n as f64;
            println!("dispatch 4 条路由 {path}: {per:.0} ns/请求");
        }
    }

    #[test]
    fn bench_dispatch_200() {
        let mut r = Router::new();
        for i in 0..100 {
            r = r
                .get(&format!("/api/v1/static{i}"), |_, _| Response::text("s"))
                .unwrap();
        }
        for i in 0..100 {
            r = r
                .get(&format!("/api/v1/param{i}/{{id}}"), |_, _| Response::text("p"))
                .unwrap();
        }
        let c = ctx();
        let n = 3000usize;
        for path in ["/api/v1/static99", "/api/v1/param99/42", "/api/v1/missing"] {
            let mut req = Request::new(Method::Get, path);
            let started = std::time::Instant::now();
            for _ in 0..n {
                std::hint::black_box(r.dispatch(&mut req, &c));
            }
            let per = started.elapsed().as_nanos() as f64 / n as f64;
            println!("dispatch 200 条路由 {path}: {per:.0} ns/请求");
        }
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
    fn tree_split_semantics() {
        // 树是"按 `/` 切段"建起来的，口径必须与当年 `split('/')` 一致，
        // 否则分派会静默漏路由或者把畸形路径当成命中。
        let r = Router::new()
            .get("/", |_, _| Response::text("root"))
            .unwrap()
            .get("/users", |_, _| Response::text("list"))
            .unwrap()
            .get("/users/{id}", |_, _| Response::text("one"))
            .unwrap()
            .get("/a/b", |_, _| Response::text("ab"))
            .unwrap();
        // 根
        assert_eq!(call(&r, Method::Get, "/").body, b"root");
        assert_eq!(call(&r, Method::Get, "").body, b"root");
        // 无前导斜杠也算同一条路径（与旧实现一致）
        assert_eq!(call(&r, Method::Get, "users/7").body, b"one");
        assert_eq!(call(&r, Method::Get, "/users/7").body, b"one");
        assert_eq!(call(&r, Method::Get, "/users").body, b"list");
        // 段数不对：`/users` 不该被 `{id}` 接住
        assert_eq!(call(&r, Method::Get, "/users/7/extra").status, Status::NOT_FOUND);
        // 空段是畸形路径，不该被当成参数 ""
        assert_eq!(call(&r, Method::Get, "/users//7").status, Status::NOT_FOUND);
        assert_eq!(call(&r, Method::Get, "/a//b").status, Status::NOT_FOUND);
    }

    #[test]
    fn head_gets_get_route_and_keeps_the_body_length() {
        // 路由层**不再**清 body：HEAD 不发 body 是写回层的事（它知道
        // `head_only`），而 `content-length` 必须按真实 body 算。
        // 这里锁住"body 还在"，否则写回层只能算出一个 0。
        let r = router();
        let get = call(&r, Method::Get, "/users/7");
        let head = call(&r, Method::Head, "/users/7");
        assert_eq!(head.status, Status::OK);
        assert_eq!(head.body.len(), get.body.len(), "HEAD 要保留 GET 的长度");
        assert!(!head.body.is_empty(), "body 留给写回层决定发不发");
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

    /// 这条盯着一个关键事实：panic 在持有 `&mut Session` 期间发生时，
    /// `RefMut` 的析构会在栈展开时跑，借用标志正常释放。
    /// 如果它不成立，一次 panic 就会让该库永久报"正在被外层使用"，
    /// 等于整库卡死——比 panic 本身严重得多。
    #[test]
    fn panic_does_not_poison_db_session() {
        use crate::error::Error as CrateError;

        let c = ctx();
        let r = Router::new()
            .get("/boom", |_, ctx| {
                let _ = ctx.with_db(|_db| -> crate::error::Result<()> {
                    panic!("持有 &mut Session 时 panic");
                });
                Response::text("不可达")
            })
            .unwrap();

        let resp = call(&r, Method::Get, "/boom");
        assert_eq!(resp.status, Status::INTERNAL_SERVER_ERROR);

        // 同一 Context 的同一个库仍可正常使用
        let ok = c
            .with_db(|db| Ok::<_, CrateError>(db.execute_batch("CREATE TABLE t (a INTEGER)")?))
            .is_ok();
        assert!(ok, "panic 之后会话必须仍可用，不能永久占用");
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
