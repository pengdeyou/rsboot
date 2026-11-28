# rsboot 路线图（下一步全部方向）

这份文档是"接下来做什么"的唯一清单。性能数字的口径见 [`perf-log.md`](perf-log.md)，代码口径是"核心零第三方依赖、扩展能力一律走 `rsboot-*` 独立 crate"。

状态标记：`[x]` 已完成并验证 / `[ ]` 待做 / `[~]` 部分完成。

---

## 0. 当前站位（作为所有目标的基线）

| 项 | 现状 |
|---|---|
| 核心 | HTTP/1.1，thread-per-core + 协程调度（`server::mn`），零第三方依赖，`src/` 约 16k 行 |
| 核心已内建 | 路由（前缀树）、`Path<T>`/`Query<T>`/`Bearer`/`ApiKey` 提取器、统一 `Res`/`ApiError`/`exception`、CORS/限流/安全头/访问日志中间件、`service` 池+重试+熔断、MySQL 协程化驱动、`config` YAML+分层+**热加载 watcher**、`runtime` module/lifecycle+优雅停机、`web/openapi`(`RouteDoc` + `/openapi.json` + `/docs`)、`web/stats` 钩子 |
| 扩展 crate | `rsboot-ops`、`rsboot-cookie`、`rsboot-form`、`rsboot-static`、`rsboot-view`、`rsboot-sse`、`rsboot-claw` |
| 性能 | 4 worker / 客户端 4 线程 p16：rsboot 999.4k，actix-web 4 1033.2k，**落后 3.27%**，胜场 1:19 |
| 流式 | `Response::stream` + `ChunkSink` + chunked 写回；HTTP/1.0 降级为"写完即断" |
| 模板 | `{extends}` / `{block}` / `{assign}` / `{if}` / `{foreach}`，编译期展开继承 |
| 测试 | `cargo test --workspace` 415 passed |

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

### 2.8 并发内存与阻塞点（README 自己列的 limitation，逐条消）

- [ ] **协程栈**：`mn/co.rs` 的 `STACK_SIZE = 64 KiB` 是固定值，10k 并发 ≈ 640 MB 栈。已有 `take_stack`/`give_stack` 栈池，但池只解决复用、不解决峰值。可选方向：按路由分级栈（轻请求 16 KiB / 重请求 64 KiB）、栈按需增长（guard page + grow），先量一次 `10k` 并发下的 RSS 再决定
- [ ] **客户端 connect 仍阻塞**：`service` 的建连走阻塞 `connect`，慢 DNS/慢握手会钉住 worker；改成协程化 `connect`（`coio` 已有写侧经验）
- [ ] **主路径读头/读体可能阻塞**：确认 `serve_once` 里 `read` 何时让出（`EAGAIN` 后），慢客户端粘包时是否会把整个 worker 拖在同一个 fd 上
- [ ] **10k 并发实测**：按协议跑 10k 连接 p1，观察 RSS、P999、`EAGAIN` 次数，作为上面三条的判定基线

---

## 3. P2 —— 能力补齐（对标 actix / axum / Spring Boot）

原则：每个能力一个独立 crate，核心不新增依赖；crate 之间可以互相依赖，但不许反向污染核心。

### 3.1 协议与传输（核心的真空白在这里）

- [ ] `rsboot-compress`：**当前完全没有响应压缩**（`src` 里没有 `content-encoding`），大 JSON 走网络全是白扔的字节。按 `Accept-Encoding` 与 `content-type` 白名单、按体积阈值（< 1 KiB 不压）；流式响应支持边压边发
- [ ] `rsboot-ws`：RFC 6455 握手 + 帧编解码（掩码、分片、ping/pong、close），复用 `ChunkSink` 通道。README 的 Known limitations 把它列为 S12
- [ ] `rsboot-multipart`：流式解析 `multipart/form-data`，小文件进内存、超阈值落临时文件，支持进度回调（现在 `rsboot-form` 只吃 urlencoded）
- [ ] `rsboot-tls`：`rustls` 封装，feature-gated；`App` 增加 `serve_tls_with`。**核心零依赖不变**，TLS 只是可选 crate
- `rsboot-h2`：**不做**。README 已声明 gRPC out of scope，理由是 HTTP/2 要拉 `h2` 进来、等于放弃零运行时依赖；h2c 同理，收益（连接复用）在 keep-alive 已经够用的场景里换不回这笔账

### 3.2 校验与人体工学（提取器与体积上限都已有）

已有：`Path<T>` / `Query<T>` / `Json<T>`（错误形状对齐 FastAPI 422）/ `Form<T>`（`rsboot-form`）/ `Bearer` / `ApiKey`；
`http.max_body_bytes` 已在 `config` 里（dev 1 MiB、prod 10 MiB）并接到 `limits.max_body`。

- [ ] `rsboot-validate`：声明式字段校验（`#[derive(Validate)]` + `len/min/max/regex/oneof`），错误聚合成 `{"code":4003,"message":"...","fields":{...}}`。**这是唯一真缺口**，现在每个 handler 都要手写 if
- [ ] `content-type` 强校验：`Json<T>` 现在只凭 body 解析，`text/plain` 里塞 JSON 也能过；按 `Accept` 协商返回形状（`application/json` vs `text/html` 错误页）
- [ ] `rsboot-macros`：`#[get("/x")]` 路由糖、`#[derive(Extract)]`、`routes![...]` 批量注册
- 不做：完整 RBAC（README 已明说认证停在 Bearer/ApiKey），要做也是独立 crate，不进核心

### 3.3 数据与状态（已有 `service` 池 / 重试 / 熔断 / MySQL 协程化驱动）

- [ ] `rsboot-redis`：手写 RESP2/RESP3 客户端 + 连接池 + pipeline，socket 走 `coio`（和 MySQL 驱动同一套让出机制）
- [ ] `rsboot-cache`：进程内 LRU + TTL（分片锁，避免全局 mutex），可切到 redis 后端；`Res` 层加 `Cache-Control` 糖
- [ ] `rsboot-job`：`@Scheduled` 风格定时任务 + 一次性延迟任务，挂在 `mn` 调度上（现在完全没有，`cron`/`scheduled` 在 `src` 里零命中）
- [ ] `rsboot-queue`：进程内任务队列 + 并发 worker + 重试/退避
- [ ] 只读副本 / 分库路由：`service` 已有连接池，缺"按读写选池"的声明式配置

### 3.4 可观测性与运维（健康检查 / `/metrics` / 优雅停机 / 热加载 均已内建）

- [ ] 结构化访问日志：`web/middleware/log.rs` 现在是文本行，补 JSON 行 + `traceparent` 透传（跨服务追踪）
- [ ] 慢请求日志：阈值可配，记录路由、耗时、状态码、trace id，不落敏感头
- [ ] `/metrics` 补运行时指标：协程数、栈池命中率、连接数/idle、写侧 `EAGAIN` 次数、路由级 QPS/延迟直方图（现在只有通用计数）
- [ ] 采样剖面做成运维开关：`--features prof` 编译 + `RSBOOT_PROFILE=1`，线上排障用，默认零开销
- [ ] 优雅停机补指标与超时：drain 期间的剩余连接数、超时强制关闭计数
- 不做：另起 `rsboot-metrics` crate（`rsboot-ops` 已有 `/metrics`，重复建设）

### 3.5 静态资源与模板增强（已有 ETag / 304 / 单段 206 / 穿越防护 / index）

- [ ] `rsboot-static`：Linux `sendfile`（大文件零拷贝）+ 预压缩旁路（`.gz` / `.br` 存在就直接发，配 `content-encoding`）
- [ ] `rsboot-static`：`Vary: Accept-Encoding`、`If-Modified-Since` 与 `If-None-Match` 并存时的优先级、目录列表默认关闭的显式参数
- [ ] `rsboot-view`：`{for}`、`{capture}`、内建变量（`$smarty.now` 等）、过滤器 `|date` `|default` `|truncate`（`|escape` `|upper` `|json` 已有）
- [ ] `rsboot-view`：模板热重载（dev 模式 mtime 检测，复用 `config/watcher` 的机制）
- [ ] `rsboot-view`：编译产物缓存落盘（`.tplc`），避免每次启动重解析

### 3.6 接口契约与国际化（`web/openapi` 已有 `RouteDoc` + `/openapi.json` + `/docs`）

- [ ] OpenAPI 补严格度：请求/响应体 JSON Schema、`oneOf` 错误体、按 module 分组 tag、鉴权 scheme 声明
- [ ] `rsboot-i18n`：`Accept-Language` 协商 + 消息目录（TOML/JSON）+ `Res::success` 自动本地化（`src` 里零命中，全空白）

### 3.7 测试与开发体验

- [ ] `rsboot-test`：`TestClient`（不起真实端口，直接喂字节进 `serve_once`）+ 断言宏 + 临时服务 helper
- [ ] `examples/` 补齐：CRUD、SSE 推送、上传、模板站、WebSocket 聊天
- [ ] `cargo rsboot new` 脚手架（独立 bin crate，模板内置）

### 3.8 `rsboot-claw` 爬虫（v0.1 已落地）

v0.1 的定位是"能独立跑起来、礼貌、可测"，架构 fork Scrapy 的分层（Scheduler / Downloader / Parser / Policy），但落到同步模型，不引 tokio。

- [x] `frontier`：per-host 分桶的待抓队列、URL 归一化去重（`http://a.com` == `http://a.com/` == `:80`）、深度限制、per-host 限速（robots 的 `Crawl-delay` 会抬高该站）
- [x] `http`：HTTP/1.1 GET、keep-alive 复用、复用死连接自动重连一次、`Content-Length`/`chunked`/读到 EOF 三种 body、3xx 跟随 ≤5 跳、body 上限
- [x] `html`：一趟扫描不建树，链接 / `rel=nofollow` / `<base>` / `<title>` / `meta description` / `link rel=canonical` / `h1–h6` / 可见文本，跳过 `script`/`style`/注释
- [x] `robots`（RFC 9309）：最长匹配、等长 Allow 胜、`*` 通配与 `$` 锚定、按 UA 前缀选组
- [x] `Crawler`：`std::thread::scope` 多 worker 抢一把 `Mutex<Frontier>`，handler 同步调用，`Stats` 汇总（页数/字节/错误分类/robots 拦截/去重/超深）
- [x] e2e：用 rsboot 自己起站点再爬它（robots 拦截、重定向归属、回环去重、`max_pages`、连不上不挂死）
- [x] v0.2 `sitemap.xml`：`sitemap::parse` 解 `<urlset>` / `<sitemapindex>`；`.sitemap(true)` 把 robots 的 `Sitemap:` + `/sitemap.xml` 当种子，index 往下两层；`.xml.gz` 不支持（解 gzip 要引库）
- [x] v0.2 Pipeline：`Sink` trait（`on_page` / `on_error`）+ `JsonlSink`（一行一页，手写 JSON 转义）+ `CollectSink`；`Crawler::sink(Arc<dyn Sink>)` 走旁路，不影响调度
- [ ] v0.2 Middleware：请求前改写（UA 轮换 / 代理 / 重试策略）——Scrapy 分层里唯一还没落的一层
- [ ] v0.2 checkpoint：`seen` 集合落盘（换掉内存 `HashSet`），支持中断续爬
- [ ] v0.2 HTTPS：作为**可选 feature**（`tls`，引 rustls），默认关闭以守住零依赖；不做就在 `Error::UnsupportedScheme` 上让调用方决定
- 不做：JS 渲染（要 headless browser）、gzip 解压（请求侧直接 `identity`）、分布式调度（单机爬虫不做分布式协调）

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

另外三条是产品层面的决定，不是性能实验：

- **HTTP/2 / gRPC**：README 已声明 out of scope，要拉 `h2` 进来、等于放弃零运行时依赖
- **另起 `rsboot-metrics` / `rsboot-openapi` crate**：`rsboot-ops` 与 `web/openapi` 已有，重复建设
- **核心内建完整 RBAC**：认证停在 Bearer/ApiKey，权限体系属于业务层

---

## 6. 里程碑

| 里程碑 | 内容 | 出口条件 |
|---|---|---|
| **M1**（近期） | 1.1 剖面 → 1.2 日期头 → 1.3 静态头 → `rsboot-validate` | 剖面表进 `perf-log.md`；至少一项 P0 改动两批同向 ≥ +0.5%；`Validate` 能聚合多字段错误 |
| **M2** | `rsboot-compress`（真空白，优先）+ `rsboot-ws` + static 的 sendfile/预压缩 + 2.8 的 10k 并发基线 | 压缩有 e2e 与体积对比；WS 跑通 ping/pong 与广播；10k 连接 RSS 与 P999 有数 |
| **M3** | `rsboot-tls` + `rsboot-redis`/`rsboot-cache` + `rsboot-job`/`rsboot-queue` + OpenAPI 严格度 | TLS 起来后 HTTPS 与 h1 并存；redis pipeline 可用；定时任务有 cron 用例 |

排期不写死日期：每个方向按"测量 → 改动 → 两批反序验证 → 记账"走，没通过 `ab5.sh` 的一律回退并写进已否决表。
