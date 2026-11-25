//! 示例里 handler 统一返回 `Result<_, ApiError>`：`ApiError` 目前约 144 字节，
//! clippy 会提示 `result_large_err`。这是**刻意先接受**的取舍——把错误结构
//! Box 化会改掉公开字段（`loc`/`kind`），留到 OpenAPI 阶段和 schema 一起动，
//! 免得同一个结构改两遍。成功路径不受影响（`Ok` 只搬 `Response`）。
#![allow(clippy::result_large_err)]

//! 最小可运行示例：起一个真 HTTP 服务，含路由、路径参数、JSON、统一响应体、
//! 统一异常、查库。
//!
//! ```bash
//! cargo run -p rsboot --example hello
//! curl -i http://127.0.0.1:8000/
//! curl -i http://127.0.0.1:8000/api/hello/rsboot
//! curl -i http://127.0.0.1:8000/api/boom      # 统一异常形状
//! curl -i "http://127.0.0.1:8000/api/search?q=a"
//! ```

use rsboot::{
    Api, ApiError, App, Config, Context, FromRequest, ParamDoc, Path, Query, Response, RouteDoc,
    Router, Status,
};

#[derive(serde::Deserialize)]
struct Search {
    q: String,
}

fn main() -> rsboot::Result<()> {
    // 显式声明环境：不写会在启动时打印"按编译模式推断"的提示
    let config = Config::new()
        .set("app.env", "dev")
        .set("http.bind", "127.0.0.1:8000")
        .set("http.envelope", "true")        // 打开统一响应体
        .set("http.docs", "true")            // 打开 /docs 与 /openapi.json
        .set("http.cors.allow_origin", "http://localhost:5173") // 前端 dev server
        // 收到 SIGTERM 后再当 1 秒"跛鸭"（prod 默认 5 秒），
        // 这段时间 /ready 已经返回 503，可以从容摘流量
        .set("http.drain_secs", "1");

    let mut app = App::builder()
        .config(config)
        .database("default", "sqlite:///hello.db")
        .build()?;

    let router = Router::new()
        .get("/", |_, _| Response::text("rsboot is running\n"))
        .unwrap()
        // readiness 探针：收到 SIGTERM 后在跛鸭期返回 503，
        // 让 LB / K8s 先把流量摘走，而不是等连接被硬断
        .get("/ready", |_, ctx: &Context| {
            let st = match ctx.state::<rsboot::ServeStatus>() {
                Ok(g) => g.state(),
                Err(_) => return Response::new(Status::SERVICE_UNAVAILABLE),
            };
            match st {
                rsboot::ServeState::Serving => Response::text("serving\n"),
                _ => Response::new(Status::SERVICE_UNAVAILABLE),
            }
        })
        .unwrap()
        // 路径参数：`Path<String>` 直接拿到 {name}
        // 提取失败用 `?` 交给统一异常层（400/422），绝不要 unwrap：
        // 那会变成 500 + 一段"internal server error"，把用户的错当成了服务端的错。
        .get("/api/hello/{name}", |req, _| -> Result<String, ApiError> {
            let Path(name) = Path::<String>::from_request(req)?;
            Ok(format!("hello, {name}"))
        })
        .unwrap()
        // 给这条路由挂文档：/docs 里会出现参数说明，/openapi.json 里会有 schema
        .describe(
            rsboot::Method::Get,
            "/api/hello/{name}",
            RouteDoc::new("打招呼")
                .tag("示例")
                .describe("返回一句问候，用来验证路径参数提取")
                .param(ParamDoc::path("name", serde_json::json!({ "type": "string" })))
                .response(serde_json::json!({ "type": "string" }))
                .error(400, "路径参数不合法"),
        )
        .unwrap()
        // 查询参数：`Query<T>` 反序列化到结构体，缺字段会得到 422
        .get("/api/search", |req, _| -> Result<Response, ApiError> {
            let Query(s): Query<Search> = Query::from_request(req)?;
            Ok(Api(format!("你搜的是 {}", s.q)).respond(req))
        })
        .unwrap()
        // 错误：直接返回 Err，形状由统一异常层决定
        .get("/api/boom", |_, _| -> Result<Response, ApiError> {
            Err(ApiError::new(Status::CONFLICT, "这个名字已被占用"))
        })
        .unwrap()
        // 查库：Context 里每个 worker 都有自己的一份连接
        .get("/api/users", |req, ctx: &Context| -> Result<Response, ApiError> {
            let n = ctx
                .with_db(|s| {
                    s.fetch(rsbatis::Sql::raw("SELECT 1"))
                        .map(|rows| rows.len())
                        .map_err(rsboot::Error::Driver)
                })
                // 数据库错误一律 500 + 笼统文案：SQL 原文只进日志，不回给客户端
                .map_err(ApiError::internal)?;
            Ok(Api(serde_json::json!({ "rows": n })).respond(req))
        })
        .unwrap();

    println!("监听 http://127.0.0.1:8000，Ctrl+C / SIGTERM 优雅退出");

    // `serve` 会装好 SIGINT/SIGTERM：收到信号后停止 accept、等 worker 收尾、
    // 逆序关闭模块，然后再退出。不需要外部再加一套信号处理。
    app.serve(router)
}
