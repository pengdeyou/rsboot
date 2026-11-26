# rsboot

对标 FastAPI 的 Rust 应用框架：声明式路由、类型化提取、自动 OpenAPI，HTTP 与调度全部自研。

[rsbatis](https://github.com/pengdeyou/rsbatis) 是 ORM，rsboot 是应用框架，两者独立版本、独立仓库。

**状态：alpha（`0.1.0-alpha.1`）。** 核心链路（HTTP、路由、提取、响应体、异常、OpenAPI、模块、下线、协程调度、数据库 socket）已跑通并有测试覆盖；SSE / WebSocket / JSON-RPC 尚未实现，见文末「已知限制」。

## 它要解决的问题

FastAPI 好用的地方是三件事：声明式路由、类型即校验、文档自动生成。Rust 主流框架各缺一块：

| | 写法 | 问题 |
|---|---|---|
| axum / actix-web | `async fn` handler | `Send + 'static` 约束、不能跨 await 借 `&Request`；异步 DB/HTTP 要连带引入 tokio / hyper / sqlx |
| 纯同步框架 | `fn` handler | worker 数 = 核心数，慢 I/O 直接把吞吐限制为「worker 数 ÷ 阻塞时长」 |

第二条是实测过的：20ms 的下游延迟、200 并发，纯同步实现只有 **99 QPS**，因为 2 个 worker 全被阻塞。

rsboot 的做法是**连接层事件驱动 + 请求处理层有栈协程**：

- HTTP/1.1 自己实现（不引 axum / hyper / tokio），零第三方运行时依赖
- handler 保持同步签名，`&mut Request` 可跨让出点持有——有栈协程没有「状态机由编译器生成」这个前提
- 慢 I/O 时自动让出 worker，吞吐不再被阻塞时长限制

## 实测

4 核 Linux，2 worker，三服务同窗口逐一轮流压测：

| 场景 | rsboot | axum | actix-web |
|---|---|---|---|
| `/slow` 20ms × 5 并发 | 248 | 236 | 236 |
| `/slow` 20ms × 20 并发 | 990 | 944 | 944 |
| `/slow` 20ms × 200 并发 | 9 687 | 9 237 | 9 292 |
| `/json` 无阻塞 | 157 730 | 138 926 | 162 098 |

接入协程前，`/slow` 200 并发只有 **100 QPS**——200 并发这一档的差距是 96 倍，这就是这套设计存在的原因。

数字会随机器漂移，只能同窗口对比，不要跨机器比绝对值。

## 特性

- **路由与提取**：`Router::get/post/…`，`Path<T>` / `Query<T>` / `Json<T>` / `ApiKey` / `Bearer` 提取器，失败走统一异常层（400/422），不会退化成 500
- **统一响应体**：`Api(...)` 包一层，成功/失败同一形状；`http.envelope` 可关
- **统一异常**：`ApiError` 一个类型贯穿，SQL 原文只进日志不回客户端
- **OpenAPI**：`/openapi.json` 与 `/docs` 自动生成，路由可挂 `RouteDoc`
- **安全**：CORS 启动期硬校验（配置错就起不来，不静默放行）、令牌桶限流、安全响应头、连接串密码脱敏
- **生命周期**：`Module` 四个钩子（init/start/stop/drain），逆序关闭；SIGINT/SIGTERM 自处理
- **优雅下线**：收到信号先进跛鸭期（`http.drain_secs`），`/ready` 返 503 让 LB 摘流量，关闭期新请求返 503
- **配置**：YAML / ini / env 分层，`app.env` 区分 dev 与 prod，支持热加载
- **数据库 socket 协程化**：`mysql+co://` 走协程读，慢查询不占死 worker（依赖 `rsbatis-mysql` vendor fork）
- **下游调用协程化**：`ctx.service("name")` 带连接池、重试、熔断

## 快速开始

```bash
cargo run --example hello
curl -i http://127.0.0.1:8000/api/hello/rsboot
```

最小形态：

```rust
use rsboot::{ApiError, App, Config, FromRequest, Path, Response, Router};

fn main() -> rsboot::Result<()> {
    let config = Config::new()
        .set("app.env", "dev")
        .set("http.bind", "127.0.0.1:8000")
        .set("http.docs", "true");

    let mut app = App::builder()
        .config(config)
        .database("default", "sqlite:///hello.db")
        .build()?;

    let router = Router::new()
        .get("/", |_, _| Response::text("rsboot is running\n"))?
        .get("/api/hello/{name}", |req, _| -> Result<String, ApiError> {
            let Path(name) = Path::<String>::from_request(req)?;
            Ok(format!("hello, {name}"))
        })?;

    app.serve(router)
}
```

`serve` 会装好信号处理：停止 accept、等 worker 收尾、逆序关模块，不需要外部再叠一套。

## 目录

```text
src/
  server/       HTTP/1.1 解析、连接、writev、poll、worker；mn/ 是协程调度与协程化 socket
  web/          路由、请求、提取器、JSON、统一响应体、统一异常、中间件（CORS/限流/日志）
  runtime/      App、Context、模块生命周期、serving
  service/      下游 HTTP 客户端：连接池、重试、熔断
  config/       YAML/ini/env 分层、环境、热加载
  support/      脱敏等旁路工具
```

## 已知限制

诚实列出来，不假装完整：

- SSE、WebSocket、JSON-RPC 2.0 还没做（设计里的 S11–S13）
- gRPC 明确不做：要 HTTP/2，等于把 h2 拉进来，与「零第三方运行时依赖」冲突
- 认证只有 `Bearer` / `ApiKey` 提取器，没有完整的角色权限体系
- 协程栈固定 64 KB，1 万并发约 640 MB 栈内存
- 客户端建连仍是阻塞的（已连接之后的读写才走协程）
- 读请求头与请求体在主路径上仍可能阻塞 worker

## 许可

MIT OR Apache-2.0，任选其一。
