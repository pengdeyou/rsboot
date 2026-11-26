//! 基准用的 rsboot 服务：三个端点，和 axum/actix 对照实现完全一致。
//!
//! 刻意关掉日志中间件与统一响应体：这两个是 rsboot 的"额外功能"，
//! 开着比不公平（对照框架没有同等工作量），也掩盖栈本身的性能。

use rsboot::web::extract::{FromRequest, Path};
use rsboot::web::response::Response;
use rsboot::web::route::Router;
use rsboot::{App, Config, Status};

fn main() -> rsboot::Result<()> {
    let workers = std::env::var("WORKERS").unwrap_or_else(|_| "4".into());
    let bind = std::env::var("BIND").unwrap_or_else(|_| "127.0.0.1:9001".into());
    // 下游地址：给了才有 /down 端点，用来量"下游慢时 worker 会不会被焊死"
    let down = std::env::var("DOWN").ok();

    // 预生成响应字节：不让"每次序列化"混进被测的栈性能里
    let json_body = br#"{"ok":true,"n":1,"msg":"benchmark payload 0123456789abcdef"}"#;
    // 大响应：64KB 预生成字节，考察 writev 路径（不做序列化）
    let big_body: &'static [u8] = {
        static BIG: [u8; 64 * 1024] = [b'a'; 64 * 1024];
        &BIG
    };

    let router = Router::new()
        .get("/json", move |_, _| Response::json_with(json_body, Status::OK))
        .unwrap()
        .get("/big", move |_, _| Response::json_with(big_body, Status::OK))
        .unwrap()
        .get("/slow", |_, _| {
            // 关键：用框架的 sleep（协程让出），不是 std 的（焊死 worker）
            rsboot::sleep(std::time::Duration::from_millis(20));
            Response::json_with(json_body, Status::OK)
        })
        .unwrap()
        .get("/items/{id}", |req, _| -> Result<Response, rsboot::ApiError> {
            let Path(id) = Path::<String>::from_request(req)?;
            let body = format!(r#"{{"id":"{id}"}}"#);
            Ok(Response::json(body.as_bytes()))
        })
        .unwrap();

    let down_route = down.clone();
    let router = router
        .get("/down", move |_, ctx: &rsboot::Context| -> Result<Response, rsboot::ApiError> {
            let url = down_route.clone().ok_or_else(|| rsboot::ApiError::internal("没配 DOWN"))?;
            let svc = ctx
                .service("down")
                .ok_or_else(|| rsboot::ApiError::internal(format!("没注册 down：{url}")))?;
            let ans = svc.get("/x").send().map_err(|e| {
                rsboot::ApiError::new(rsboot::Status::BAD_GATEWAY, format!("下游失败：{e}"))
            })?;
            Ok(Response::json_with(ans.text().as_bytes(), Status::OK))
        })
        .unwrap();

    let cfg = Config::new()
        .set("app.env", "prod")
        .set("http.bind", bind)
        .set("http.workers", workers)
        .set("http.docs", "false")
        .set("http.envelope", "false")
        .set("http.log.requests", "false")
        .set("http.security.headers", "false")
        .set("http.drain_secs", "1")
        // 默认 100：单个连接处理 100 个请求后主动关（防长期占用 worker）。
        // 基准里调大，否则"每 100 个请求重连一次"的成本会混进结果，
        // 而 axum/actix 不做这个限制——这样比才公平。生产请保留默认。
        .set("http.max_requests", "1000000");

    let mut b = App::builder().config(cfg).database("default", "sqlite://:memory:");
    if let Some(u) = &down {
        b = b.service("down", u.as_str());
    }
    let mut app = b.build()?;

    println!("rsboot 监听 {}", app.bind_addr()?);
    app.serve(router)
}
