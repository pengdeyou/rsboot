# rsboot 路线图（下一步全部方向）

这份文档是"接下来做什么"的唯一清单。性能数字的口径见 [`perf-log.md`](perf-log.md)，代码口径是"核心零第三方依赖、扩展能力一律走 `rsboot-*` 独立 crate"。

状态标记：`[x]` 已完成并验证 / `[ ]` 待做 / `[~]` 部分完成。

---

## 0. 当前站位（作为所有目标的基线）

| 项 | 现状 |
|---|---|
| 核心 | HTTP/1.1，thread-per-core + 协程调度（`server::mn`），零第三方依赖，`src/` 约 11.8k 行 |
| 扩展 crate | `rsboot-ops`、`rsboot-cookie`、`rsboot-form`、`rsboot-static`、`rsboot-view`、`rsboot-sse` |
| 性能 | 4 worker / 客户端 4 线程 p16：rsboot 999.4k，actix-web 4 1033.2k，**落后 3.27%**，胜场 1:19 |
| 流式 | `Response::stream` + `ChunkSink` + chunked 写回；HTTP/1.0 降级为"写完即断" |
| 模板 | `{extends}` / `{block}` / `{assign}` / `{if}` / `{foreach}`，编译期展开继承 |
| 测试 | `cargo test --workspace` 364 passed |

任何性能项必须走 `ab5.sh` 两批反序（AB / BA 各 10 轮 × 8s），噪声底 **±0.35%**；单批同向才算收益，两批反向即噪声。

---

## 1. P0 —— 把 3.3% 追回来（唯一有明确对手的目标）

剖面上每请求 2 829–6 287 ns，其中两次 syscall 占 70%，用户态只剩约 1 µs（parse ~275 / build ~145 / handle ~555 ns）。3% ≈ 150 ns，只能从 parse / build / handle 三段的固定动作里抠。

### 1.1 先把 handle 的 555 ns 拆开（测量项，不改代码）

- [ ] 在 `route.rs` 的 `dispatch` / `finalize` 与 handler 返回处加 1/64 采样（复用 `profile` 已有开关，关闭时零开销）
- [ ] 把 555 ns 分成四段：路由匹配 / 上下文构造 / 用户 handler / 响应构造
- [ ] 产出一张"每段 ns + 占比"表追加进 `perf-log.md`
- 验收：四段之和与 555 ns 的误差 < 10%，且能指出最大的一段

**在这张表出来之前，下面 1.2–1.6 都不许动手**——历史三次"应该会快"的改动全部落在噪声底内或微负。

### 1.2 日期头缓存

- [ ] `Date` 头改成每秒一算：后台协程（或写路径首次调用时）生成 `&'static str`，热路径只做一次指针拷贝
- 依据：每请求都在做 `strftime` 等价工作 + 堆分配，属于"固定动作"里最肥的一块
- 验收：`ab5.sh` 两批同向，均值 ≥ +0.5%
- 风险：秒边界漂移 1 秒（HTTP 允许），注意别让缓存的字符串生命周越过 `write`

### 1.3 状态行与常见响应头静态化

- [ ] `200 OK` / `404 Not Found` / `500 Internal Server Error` 等 reason phrase 换成 `&'static str` 查表，不走 `write!`
- [ ] `server:` / `connection:` 头值预置为静态切片
- 验收：`build` 段 ns 下降，整体两批同向 ≥ +0.3%

### 1.4 请求头容器容量复用

- [ ] 核实 `Headers` 的 `Vec` 是否每请求 `clear()` 后重新增长到 12–16 槽（模板 `perf-log` 已证"内联 20 槽"是 −8.3%，但**跨请求复用容量**是另一件事，没试过）
- [ ] 做法：连接对象上挂 `Vec<Hdr>` 池，`serve_once` 里 `split_off` 交给请求、请求 Drop 时退回
- 验收：两批同向 ≥ +0.3%；若为负，记入"已否决"

### 1.5 路由匹配内联

- [ ] 前缀树首层按首字节做 256 项分派表（`path[0]` 直接定位到子树），省掉首层线性比较
- [ ] 静态段（无 `:` 参数）走"整段 memcmp"而非逐字节；参数段才回退到扫描
- 依据：路由表 20 条内影响可忽略，200 条以上才显现；本例只有 `/json`、`/plaintext` 等少数路由，**收益预期低**
- 验收：两批同向 ≥ +0.3%，否则不做（防止为了"架构好看"付性能账）

### 1.6 写路径 syscall 摊薄（唯一能碰到 70% 的地方）

- [ ] 试 `MSG_MORE` / `TCP_CORK`：把 head 与 body 两次 `write` 合成一次内核可见的段（Linux 专属，需 `cfg`）
- [ ] 试 per-worker 的 `sendmsg` + `iovec[2]`（head/body 分离的不连续写），与现有 writev 分支对比
- 说明：`SO_REUSEPORT`、`io_uring`、攒批、关协程、`SNDBUF` 调整都已测过无效或更差，**不要重试**
- 验收：`wr/req` 从 1.00 降到 0.5 才有意义；只降 syscall 次数不降时间的不算

### 1.7 单请求栈与 `mn` 调度开销

- [ ] 量一次 `serve_once` 协程切换成本（`yield`/`resume` 的 ns）
- [ ] 若 > 100 ns，试"同一协程连跑 2 个请求再让出"（历史测过"连跑多请求"是 −12%，但那是"每连接常驻 task"的重构，不是这个最小改动；若复测仍负，记入已否决）

---

## 2. P1 —— 稳定性、抖动、长跑

Mac 本机交替 5 轮 A/B（加流式前后）落在 57k–73k QPS、同代码自身波动 ±13%，**该机不能做判定**，只能做冒烟。真实判定必须在 4 核云主机上按协议跑。

- [ ] **抖动归因**：在云主机上跑 10 轮 × 8s，记录每轮的 QPS、P99、`/proc/interrupts` 增量、CPU 频率、`steal` 时间，判断抖动来自中断合并、频率调节还是同机邻居
- [ ] **Soak test**：连续 1h + 24h，采样 RSS / fd 数 / P999，画曲线；RSS 单调上升即为泄漏
- [ ] **空闲连接读缓冲瘦身**：keep-alive 空闲期把读缓冲降到 1 KiB 以下（现在是常驻大 buffer），N 个空闲连接 × 8 KiB 是实打实的内存账
- [ ] **慢客户端背压**：写侧 `EAGAIN` 时协程让出并记录水位；单连接积压超过阈值直接断开（防 slow-read 占满 worker）
- [ ] **流式健壮性**：客户端提前断开（`EPIPE`）必须终止生成器；SSE 单 worker 连接数配额，防止一个 worker 被 100 个空转流吃满
- [ ] **HTTP/1.0 流式策略**：当前"写完即断"，评估"小于阈值时物化后带 `content-length`"是否更优
- [ ] **请求走私防护**：`Content-Length` 与 `Transfer-Encoding` 同时出现、重复 `Content-Length`、`TE: chunked, chunked` 一律 400
- [ ] **慢速攻击**：读头阶段总超时（slowloris），按 worker 连接数上限
- [ ] **压测回归脚本**：`benches/ab5.sh` 固化进仓库，参数从环境变量读，避免每轮手敲

---

## 3. P2 —— 能力补齐（对标 actix / axum / Spring Boot）

原则：每个能力一个独立 crate，核心不新增依赖；crate 之间可以互相依赖，但不许反向污染核心。

### 3.1 协议与传输

- [ ] `rsboot-tls`：`rustls` 封装，feature-gated；`App` 增加 `serve_tls_with`。**核心零依赖不变**，TLS 只是可选 crate
- [ ] `rsboot-h2`（远期）：h2c 优先（明文，无需 TLS），帧解析手写；这是对上 actix/axum 的最大结构性差距
- [ ] `rsboot-compress`：`gzip` / `deflate` / `br`，按 `Accept-Encoding` 与 `content-type` 白名单、按体积阈值（< 1 KiB 不压）；流式响应支持边压边发
- [ ] `rsboot-ws`：RFC 6455 握手 + 帧编解码（掩码、分片、ping/pong、close），复用 `ChunkSink` 通道
- [ ] `rsboot-multipart`：流式解析 `multipart/form-data`，小文件进内存、超阈值落临时文件，支持进度回调

### 3.2 提取器与校验（当前最大的人体工学缺口）

- [ ] `rsboot-extract`：`Path<T>` / `Query<T>` / `Json<T>` / `Form<T>` / `Body` 泛型提取器，解析失败统一映射 4003/4002 业务错误码
- [ ] 请求体保护：`max_body`（默认 2 MiB）、`content-type` 强校验、`charset` 处理
- [ ] `rsboot-validate`：声明式字段校验（`#[derive(Validate)]` + `len/min/max/regex/oneof`），错误聚合成 `{"code":4003,"message":"...","fields":{...}}`
- [ ] `rsboot-macros`：`#[get("/x")]` 路由糖、`#[derive(Extract)]`、`routes![...]` 批量注册

### 3.3 数据与状态

- [ ] `rsboot-db`：连接池（零依赖可先做"线程本地连接 + 空闲回收"），SQL 走 rsbatis
- [ ] `rsboot-redis`：手写 RESP2/RESP3 客户端 + 连接池 + pipeline
- [ ] `rsboot-cache`：进程内 LRU + TTL（分片锁，避免全局 mutex），可切到 redis 后端
- [ ] `rsboot-job`：`@Scheduled` 风格的定时任务 + 一次性延迟任务，挂在 `mn` 调度上
- [ ] `rsboot-queue`：进程内任务队列 + 并发 worker + 重试/退避

### 3.4 可观测性与运维

- [ ] `rsboot-metrics`：Prometheus 文本暴露（计数器/直方图/仪表），零依赖手写；已有 `web::stats` 钩子直接喂
- [ ] 慢请求日志：阈值可配，记录路由、耗时、状态码、trace id，不落敏感头
- [ ] 结构化访问日志（JSON 行），支持 `traceparent` 透传
- [ ] 健康检查/就绪检查标准实现（`code::NOT_READY = 5030` 已有，补 `/healthz` `/readyz` 默认路由）
- [ ] 优雅停机：停止 accept → 排空在途 → 关闭空闲 keep-alive（当前 `shutdown` 已有雏形，补超时与指标）

### 3.5 静态资源与模板增强

- [ ] `rsboot-static`：`ETag` / `Last-Modified` / `If-None-Match` / `Range`（206）/ 预压缩 `.gz` `.br` 旁路 / Linux `sendfile`
- [ ] `rsboot-view`：`{for}`、`{capture}`、内建变量（`$smarty.now` 等）、过滤器（`|date` `|escape` `|default` `|truncate`）、模板热重载（dev 模式 mtime 检测）、`Response::html(tpl)` 糖
- [ ] `rsboot-view`：编译产物缓存落盘（`.tplc`），避免每次启动重解析

### 3.6 接口契约与国际化

- [ ] `rsboot-openapi`：从路由表 + handler 元信息导出 OpenAPI 3.1 JSON/YAML，提供 `/openapi.json` 内置路由
- [ ] `rsboot-i18n`：`Accept-Language` 协商 + 消息目录（TOML/JSON）+ `Res::success` 自动本地化

### 3.7 测试与开发体验

- [ ] `rsboot-test`：`TestClient`（不起真实端口，直接喂字节进 `serve_once`）+ 断言宏 + 临时服务 helper
- [ ] `examples/` 补齐：CRUD、SSE 推送、上传、模板站、WebSocket 聊天
- [ ] `cargo rsboot new` 脚手架（独立 bin crate，模板内置）

---

## 4. P1/P2 —— 工程与发布

- [ ] **README 补齐**（英文主版 + `zh-CN` / `ar` / `de` / `fr` / `ru` 五语言同步）：
  - `Extension crates` 表补 `rsboot-sse` 行
  - 新增「Streaming / chunked / SSE」章节（含 `Response::stream` 用法与 HTTP/1.0 降级说明）
  - `rsboot-view` 语法表补 `{extends}` / `{block}` / `{assign}`
  - 文末链接本 `ROADMAP.md`
- [ ] **CI**（GitHub Actions）：`cargo test --workspace` + `cargo clippy -D warnings` + 交叉编译 `x86_64-unknown-linux-gnu` / `musl` / `aarch64` + 保留 `benches/*` 独立 workspace 的构建检查
- [ ] **benches 独立化**：已给 `benches/{load,rsboot_serve,micro,slow_down}/Cargo.toml` 尾部加空 `[workspace]`，并在根 `Cargo.toml` 加 `exclude`；两处需在 CI 里各跑一遍防止回归
- [ ] **发布节奏**：crates.io 先发核心，再按依赖序发 `rsboot-*`；遵守"每天最多一版"
- [ ] **版本号策略**：`0.x` 期间 minor 升位即允许破坏性变更，写进 README 的 `Known limitations`
- [ ] **兼容性冒烟矩阵**：`curl`（1.0/1.1、HEAD、Expect: 100-continue）、`wrk`、`ab`、Chrome DevTools，脚本化

---

## 5. 明确不做（已测过，别重复投入）

来自 `perf-log.md` 的"已否决"表，此处只列结论：

- `Hdr` 压缩到 32 字节（−0.59%，拆表多一次间接寻址）
- parse 热循环 `get_unchecked`（−1.69%）
- 响应头跳过名单首字节预筛 / 手写 CRLF 扫描（−1.36%，`memchr` 更快）
- `from_utf8_unchecked`（−0.99%）
- 响应头 `Vec` 线程本地池化（−0.35%，借用到函数末编译不过）
- `target-cpu=v3/native`（两批反向 = 噪声）
- `Headers` 内联 12 槽（−8.3%，memset 比 malloc 贵）
- 自研 tcache 分配器默认开启（+0.36%，天花板 0.5%，保持 feature 关闭）
- 全攒进 `out_buf` 合并写（−4.3%）
- 读侧统计探针（−2.3%）
- 每连接常驻 task / 同协程连跑多请求（−12%）
- `SO_REUSEPORT` 每 worker 自 accept（稳态无收益）
- 关协程、io_uring 批量化、调小 `SNDBUF`/`RCVBUF`（更差或无收益）
- 读缓冲默认 4 KiB（+0.8%，不显著）
- **解析器结构性重构**：三次结构性改动全部落在噪声底内，275 ns 不在"扫几趟"

---

## 6. 里程碑

| 里程碑 | 内容 | 出口条件 |
|---|---|---|
| **M1**（近期） | 1.1 剖面 → 1.2 日期头 → 1.3 静态头 → `rsboot-extract` 骨架 | 剖面表进 `perf-log.md`；至少一项 P0 改动两批同向 ≥ +0.5%；`Path/Query/Json` 可用且有错误码 |
| **M2** | `rsboot-compress` + `rsboot-static` 增强 + `rsboot-ws` + `rsboot-metrics` + soak/抖动归因 | 压缩与静态缓存有 e2e；WS 能跑通 ping/pong 与广播；24h soak RSS 平稳 |
| **M3** | `rsboot-h2`（h2c）+ `rsboot-tls` + `rsboot-openapi` + `rsboot-db`/`redis` | h2c 通过 h2spec 主要用例；TLS 起来后 HTTPS 与 h1 并存；OpenAPI 能覆盖全部示例路由 |

排期不写死日期：每个方向按"测量 → 改动 → 两批反序验证 → 记账"走，没通过 `ab5.sh` 的一律回退并写进已否决表。
