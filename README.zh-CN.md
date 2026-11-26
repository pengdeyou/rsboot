# rsboot

> ### 最快的 Rust Web 服务开发方式。
>
> 慢 I/O 负载下快于 axum 与 actix-web——200 并发 / 20 ms 延迟下 **9 687 QPS**，是自己旧同步实现的 **96 倍**。同时是上手最短的一条路：`Cargo.toml` 里一行依赖声明，信号处理、OpenAPI 文档、连接池全部内置，几行代码就能跑通端点。
>
> 但我们**不是裸吞吐冠军**，这份 README 也不装作是：无 I/O 的 JSON 路径上，高并发下 actix-web 领先约 18 %。下表把每一列都列出来，包括我们输掉的那几列。

扎根 Rust 生态的应用框架——FastAPI 式的声明式路由、类型化提取、自动 OpenAPI。**同步 handler 跑在栈式协程上，底层 socket 全程非阻塞**：慢 I/O 让出 worker，而不是把它占住。HTTP/1.1 协议栈、协程调度与事件循环全部自研——不引 tokio，不引 hyper，不引 axum。

[English](README.md) · [中文](README.zh-CN.md) · [Deutsch](README.de.md) · [Français](README.fr.md) · [العربية](README.ar.md) · [Русский](README.ru.md)

**状态：alpha（`0.1.0-alpha.1`）。** 核心链路——HTTP、路由、提取器、统一响应体、统一异常、OpenAPI、模块生命周期、优雅下线、协程调度、数据库 socket——已实现并有测试覆盖。SSE / WebSocket / JSON-RPC 尚未建设，见文末「已知限制」。

`rsboot` 是应用框架，[rsbatis](https://github.com/pengdeyou/rsbatis) 是 ORM。两者独立 crate、独立仓库、独立版本。

## 它要解决的问题

FastAPI 好用，靠的是三件事：声明式路由、类型即校验、文档自动生成。Rust 主流框架各缺一块：

| | handler 写法 | 代价 |
|---|---|---|
| axum / actix-web | `async fn` | `Send + 'static` 约束；不能跨 await 借 `&Request`；异步 DB/HTTP 会把 tokio + hyper + sqlx 一起拖进依赖树 |
| 纯同步框架 | `fn` | worker 数等于核数，吞吐上限是「worker 数 ÷ 阻塞时长」——一个慢依赖就能把整台服务卡死 |

第二行是实测出来的，不是推测：20 ms 下游延迟、200 并发，纯同步实现只有 **99 QPS**，两个 worker 全在等。

rsboot 把问题拆成两层：**连接层事件驱动，请求层跑有栈协程。** handler 保持普通同步签名，慢 I/O 时让出 worker 而不是占住它。

## 性能对比

与 axum、actix-web 同机对比。条件：4 核 Linux 虚拟机，三套服务**都是 2 个 worker 线程**，路由完全一致，三者同一时间窗口内逐一轮流压测，数字为 QPS。

| 场景 | rsboot | axum | actix-web |
|---|---:|---:|---:|
| `/slow` — 20 ms 延迟，5 并发 | **248** | 236 | 236 |
| `/slow` — 20 ms 延迟，20 并发 | **990** | 944 | 944 |
| `/slow` — 20 ms 延迟，200 并发 | **9 687** | 9 237 | 9 292 |
| `/json` — 4 concurrent, no blocking I/O | **112 267** | 92 280 | 110 919 |
| `/json` — 20 concurrent, no blocking I/O | 131 350 | 133 843 | **155 315** |

这张表说明了什么，包括不替自己遮掩的部分：

- 存在阻塞的负载上，rsboot 三者中最快，但只比 axum/actix 高出几个百分点——因为三者都在「把 worker 占满」，收敛到同一量级。真正有价值的数字是**与自身旧实现的对比**：200 并发下 99 → 9 687 QPS，**96 倍**。协程层存在的全部理由就是这个。
- 纯 JSON 吞吐按并发达数分裂，我们输掉其中一半。4 并发时 rsboot 领先（**112 267**，比 actix 高 1.2 %，比 axum 高 21.7 %）；20 并发时 **actix-web 反超 18 %**（155 315 对 131 350），axum 也以 1.9 % 微弱超过我们。实测原因是我们的请求头状态机（单次请求头 757 ns，`httparse` 是 122 ns）加上每请求的固定记账开销——这些成本随并发上升而放大。也验证了它**不是**什么：`poll` 阻塞只占 worker 时间 14 %，小响应本就一次 `write` 发出。
- 这份 README 原先印的 157 730 已**作废**。压测器自身每请求要花掉数微秒（这台测试机上一次 `Instant::now()` 约 1.8 µs，每请求调用两次），把三个框架拉平成同一个数。上表数字来自重写过的客户端：`rdtsc` 计时、每请求零分配、零系统调用。
- 下游调用：40 并发 × 20 ms 依赖，协程档 **655 QPS / P99 62 ms**，阻塞档 **33 QPS / P99 2 323 ms**——只差一个开关（`RSBOOT_BLOCKING_IO`），20 倍差距。

这些数字会随机器漂移。只在同一次运行内横向比较，不要跨机器比，也不要跟别人的博客比绝对值。

## 设计亮点

1. **零第三方运行时。** HTTP/1.1 解析、keep-alive 连接管理、`writev` 批量写、poll 循环、M:N 调度器全部在仓库内。依赖层级是 serde 级，不是 tokio 级。少一层活动部件，也不用把版本绑在某个异步运行时的内部实现上。

2. **handler 保持同步，却不阻塞服务。** handler 就是 `fn(&mut Request, &Context) -> Result<Response, ApiError>`。有栈协程让状态活在真实栈上，`&mut Request` 可以跨让出点持有——这是编译器生成状态机做不到的事，不然就得在生命周期上打一堆补丁。

3. **两层刻意分开。** 连接由事件循环多路复用，请求跑在协程里。协程数量对应的是**正在处理的请求**，不是打开的连接——所以 keep-alive 连接每条只花几十字节，而不是 64 KB 栈。

4. **数据库 socket 会让出。** `mysql+co://…` 让 MySQL 驱动走协程 socket，慢查询把请求挂起，而不是焊死一个 worker。钩子是进程级、模式是线程局部，所以同一进程里的两个引擎不会互相污染。

5. **下游调用也让出。** `ctx.service("name")` 自带连接池、重试、熔断；socket 读是协程感知的，一个慢依赖只拖慢一个端点，不会拖垮整台服务。

6. **一个响应体，一个错误类型。** `Api(...)` 把成功和失败包成同一形状；`ApiError` 是 handler 唯一会返回的错误类型，状态码映射由它负责。SQL 原文和内部路径进日志，绝不回给客户端。

7. **安全在启动期失败，不在运行期失败。** CORS 配置写错就起不来，而不是静默放行一切。令牌桶限流、安全响应头、连接串密码脱敏都是内置的。

8. **生命周期与优雅下线是一等公民。** 模块实现 `init / start / stop / drain` 四钩子，逆序关闭。收到 SIGTERM 先进跛鸭期（`http.drain_secs`），此时 `/ready` 返 503，让负载均衡在连接被硬断之前把流量摘走；关闭期新请求返 503，而不是连接被重置。

9. **OpenAPI 是长出来的，不是贴上去的。** `RouteDoc` 挂在路由定义上，`/openapi.json` 与 `/docs` 和 handler 共用同一个事实来源。

10. **配置分层、dev/prod 分离、支持热加载。** YAML / ini / env 三层，`app.env` 切换行为，文件监听器让你改配置不用重启。

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

`serve` 自带信号处理：停止 accept、等 worker 收尾、逆序关模块，不需要在外面再叠一套。

## 已知限制

如实列出，因为一个藏起短板的框架比一个写明短板的框架更糟：

- SSE、WebSocket、JSON-RPC 2.0 尚未实现（规划中的 S11–S13）
- gRPC 明确不做：它要 HTTP/2，等于把 `h2` 拉进来，与「零第三方运行时依赖」冲突
- 认证只到 `Bearer` / `ApiKey` 两个提取器，没有完整的角色与权限体系
- 协程栈固定 64 KB，1 万并发约需 640 MB 栈内存
- 客户端**建连**仍是阻塞的，连接建立之后的读写才走协程
- 主路径上读请求头与请求体仍可能阻塞 worker

## 许可

MIT OR Apache-2.0，任选其一。
