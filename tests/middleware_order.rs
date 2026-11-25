//! 中间件链的行为：顺序、短路、错误路径。
//!
//! `ApiError` 约 144 字节，handler 返回 `Result<_, ApiError>` 会触发
//! `result_large_err`。这是框架已登记的技术债（见设计文档 §18.7）。
#![allow(clippy::result_large_err)]
//!
//! 这些不是"某一行代码对不对"的问题，而是"顺序错了会静默变形"的问题
//! （比如 401 响应上没加 CORS 头，浏览器只会说"请求失败"，开发者看不到 401）。

use std::sync::{Arc, Mutex};

use rsboot::config::{Config, Environment};
use rsboot::web::error::ApiError;
use rsboot::web::http::{Method, Status};
use rsboot::web::middleware::Middleware;
use rsboot::web::request::Request;
use rsboot::web::response::{IntoResponse, Response};
use rsboot::web::route::Router;
use rsboot::Context;

fn ctx() -> Context {
    Context::new(Config::new(), vec![], None, Environment::Dev)
}

struct Rec {
    name: &'static str,
    log: Arc<Mutex<Vec<String>>>,
    short: Option<Status>,
}

impl Middleware for Rec {
    fn name(&self) -> &'static str {
        self.name
    }

    fn before(&self, req: &mut Request, _ctx: &Context) -> Option<Response> {
        self.log
            .lock()
            .unwrap()
            .push(format!("before:{}:{}", self.name, req.path));
        self.short
            .map(|s| ApiError::new(s, "short").into_response())
    }

    fn after(&self, _req: &Request, _ctx: &Context, resp: Response) -> Response {
        self.log
            .lock()
            .unwrap()
            .push(format!("after:{}:{}", self.name, resp.status.0));
        resp
    }
}

fn rec(
    name: &'static str,
    log: &Arc<Mutex<Vec<String>>>,
    short: Option<Status>,
) -> Rec {
    Rec {
        name,
        log: Arc::clone(log),
        short,
    }
}

#[test]
fn before_is_registration_order_after_is_reverse() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let router = Router::new()
        .middleware(rec("outer", &log, None))
        .middleware(rec("inner", &log, None))
        .get("/x", |_, _| Response::text("ok"))
        .unwrap();

    let mut req = Request::new(Method::Get, "/x");
    let resp = router.handle(&mut req, &ctx());

    assert_eq!(resp.body, b"ok");
    assert_eq!(
        log.lock().unwrap().as_slice(),
        [
            "before:outer:/x",
            "before:inner:/x",
            "after:inner:200",
            "after:outer:200",
        ]
    );
}

#[test]
fn short_circuit_skips_inner_and_handler() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let handler_ran = Arc::new(Mutex::new(false));

    let flag = Arc::clone(&handler_ran);
    let router = Router::new()
        .middleware(rec("gate", &log, Some(Status::UNAUTHORIZED)))
        .middleware(rec("inner", &log, None))
        .get("/x", move |_, _| {
            *flag.lock().unwrap() = true;
            Response::text("不应该执行")
        })
        .unwrap();

    let mut req = Request::new(Method::Get, "/x");
    let resp = router.handle(&mut req, &ctx());

    assert_eq!(resp.status, Status::UNAUTHORIZED);
    assert!(!*handler_ran.lock().unwrap(), "短路后 handler 不该跑");
    // 短路响应仍然过**已进入**那几层的 after（否则 401 上没有 CORS 头）。
    // `inner` 的 before 没跑，它的 after 也就不该跑——只 unwind 已经进栈的层。
    assert_eq!(
        log.lock().unwrap().as_slice(),
        ["before:gate:/x", "after:gate:401"]
    );
}

#[test]
fn after_also_runs_for_404_and_405() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let router = Router::new()
        .middleware(rec("m", &log, None))
        .get("/x", |_, _| Response::text("ok"))
        .unwrap();

    let mut req = Request::new(Method::Get, "/nope");
    assert_eq!(router.handle(&mut req, &ctx()).status, Status::NOT_FOUND);

    let mut req = Request::new(Method::Post, "/x");
    assert_eq!(
        router.handle(&mut req, &ctx()).status,
        Status::METHOD_NOT_ALLOWED
    );

    assert_eq!(
        log.lock().unwrap().as_slice(),
        ["before:m:/nope", "after:m:404", "before:m:/x", "after:m:405"]
    );
}

#[test]
fn after_runs_when_handler_returns_err() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let router = Router::new()
        .middleware(rec("m", &log, None))
        .get("/boom", |_, _| -> Result<Response, ApiError> {
            Err(ApiError::new(Status(500), "boom"))
        })
        .unwrap();

    let mut req = Request::new(Method::Get, "/boom");
    let resp = router.handle(&mut req, &ctx());
    assert_eq!(resp.status, Status(500));
    assert_eq!(log.lock().unwrap().last().unwrap(), "after:m:500");
}

#[test]
fn after_can_still_mutate_response() {
    // after 不是只读钩子：加头、改状态、替换 body 都要允许
    struct Stamp;
    impl Middleware for Stamp {
        fn name(&self) -> &'static str {
            "stamp"
        }
        fn after(&self, _req: &Request, _ctx: &Context, mut resp: Response) -> Response {
            resp.headers.insert("x-stamped", "1");
            resp
        }
    }

    let router = Router::new()
        .middleware(Stamp)
        .get("/x", |_, _| Response::text("ok"))
        .unwrap();
    let mut req = Request::new(Method::Get, "/x");
    let resp = router.handle(&mut req, &ctx());
    assert_eq!(resp.headers.get("x-stamped"), Some("1"));
}
