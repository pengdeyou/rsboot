# rsboot-claw

零依赖爬虫框架，rsboot 的扩展 crate。和 `rsboot-core` 一样守"不引第三方依赖"这条线：URL 解析、HTML 扫描、robots.txt、HTTP 客户端、cookie jar、通配匹配全在仓库里。

唯一的例外是 **可选** 的 `tls` feature（引 `rustls`）：TLS 不可能手写，而默认 `default = []`，不用 TLS 的人依赖图里一个字节也不会多。

```
cargo test -p rsboot-claw          # 85 项（69 单测 + 15 e2e + 1 doctest）
cargo test -p rsboot-claw --features tls            # 再加 TLS 相关项
cargo test -p rsboot-claw --features tls --test tls_e2e -- --ignored   # 真站点（要外网）
```

## 快速开始

```rust
use std::sync::Arc;
use rsboot_claw::{follow_internal, Crawler, JsonlSink, Proxy, RetryPolicy, RotateUserAgent};

let sink = Arc::new(JsonlSink::create("pages.jsonl")?);   // 一行一页 JSON

let stats = Crawler::new()
    .seed("http://127.0.0.1:9001/")
    .max_depth(3)
    .workers(4)              // thread-per-core，和 rsboot 一个模型
    .obey_robots(true)       // 读 robots.txt，遵守 Crawl-delay
    .delay(Duration::from_millis(200))
    .max_per_host(4)         // 同一个站同时最多 4 发（默认 0 = 不限）
    .priority_fn(|u| if u.path.starts_with("/list") { 10 } else { 0 })  // 列表页先抓
    .deny("*.pdf").deny("*.zip").deny("*?page=*")   // 拒掉的链接不发请求
    .max_pages(1000)
    .sitemap(true)           // 先抓 robots 的 Sitemap: + /sitemap.xml 当种子
    .sink(sink.clone())      // 旁路落盘，和"下一步抓什么"解耦
    .retry_backoff(Duration::from_millis(200))
    .middleware(Arc::new(RetryPolicy::new().retries(2)))     // 5xx/429/网络错误重试
    .middleware(Arc::new(RotateUserAgent::new(["claw/1", "claw/2")))
    .checkpoint("crawl.wal")          // 断了接着爬：中断时队列里没抓的在里面
    .avoid_duplicate_content(true)    // 正文一样的页只交付一次
    // .proxy(Proxy::parse("http://user:pass@10.0.0.1:3128"))
    .run(|page| {
        println!("{} {} {}", page.status, page.url, page.title);
        follow_internal(page)     // 返回要跟进的地址
    });

println!("{} 页 / {:.0} 页每秒，跳过 robots {} 条，去重 {} 条",
    stats.pages, stats.pages_per_sec, stats.skipped_robots, stats.duplicates);
```

handler 是 `Fn(&Page) -> Vec<String>`，同步调用，**返回什么就抓什么**——返回相对地址会按当前页解析。想只要站内链接就用 `page.internal_links()`，想自己筛 URL 就直接遍历 `page.links()`。

## 分层

架构照 Scrapy，落到同步模型：

| 层 | 文件 | 干什么 |
| --- | --- | --- |
| Scheduler | `frontier.rs` | 按 host 分桶的待抓队列、去重、深度、per-host 限速 |
| Downloader | `http.rs` | HTTP/1.1 GET、keep-alive 复用、重定向、chunked |
| Middleware | `middleware.rs` | 发出前改写（UA 轮换/加头）、响应后判定、出错时决定重试 |
| Parser | `html.rs` | 链接 / 标题 / 描述 / canonical / `<base>` / 可见文本 |
| Policy | `robots.rs` | Allow / Disallow / Crawl-delay / Sitemap |
| Pipeline | `pipeline.rs` | 结果旁路：`Sink` trait + `JsonlSink`（一行一页）+ `CollectSink` |
| Item | `lib.rs` `Page` | 交给 handler 的结果 |

## 从哪些实现里 fork 了什么

调研了 Rust 侧四个主流实现，**fork 的是架构而不是代码**：

- [`spider`](https://github.com/spider-rs/spider)（MIT，Rust 生态最主流）：fork 它的 **frontier + robots + sitemap** 思路——按 host 组织队列、礼貌限速、robots 最长匹配。它是 tokio async，rsboot 是 thread-per-core + 栈协程，async 的调度器搬不过来。
- [`spider-lib`](https://crates.io/crates/spider-lib)：fork 它的 **Scrapy 分层**（Scheduler / Middleware / Pipeline / Stats）和"checkpoint 可恢复"的设计取向。v0.2 已把 Scheduler / Downloader / Parser / Policy / Pipeline / Middleware 六层落地；中间件按 Scrapy 的 Downloader Middleware 形态写（`RetryPolicy` / `RotateUserAgent` / `StaticHeaders`）。
- `kumo`：Scrapy-for-Rust 的另一条路线，参考它"用 trait 描述 Spider"的做法，但这里用闭包 `Fn(&Page)` 更贴近 rsboot 的 handler 风格。
- `fastCRW`：只借了它的"结果即 `Page`"命名。

**为什么不直接依赖 `spider`**：它把 tokio、reqwest、chromium 一起带进来；一个零依赖框架的扩展 crate 装不下这些。同样、同样不引 `scraper`/`select`——它们的 CSS 选择器值钱，但爬虫 80% 的需求就是"取链接 + 取标题"，`html.rs` 两百多行就能覆盖。

## 做了什么

- **去重**：以 `scheme://host[:port]/path?query` 为键，`http://a.com` 与 `http://a.com/`、`:80` 与无端口都归一成同一个 key。
- **礼貌**：`delay` 是 per-host 的；某站冷却时其它站照抓（这是按 host 分桶的唯一理由）。robots 的 `Crawl-delay` 会自动抬高该站的 delay。
- **per-host 并发**：`.max_per_host(n)` 限"同时几发"，和 `delay` 限的"隔多久"是两件事。不设时 `workers(8)` 会对同一个站同时开 8 发——对人家不够客气，自己也被一个站占满。名额在拿到任务时占、由 `Drop` 归还，handler panic 也还。
- **内容去重**：`.avoid_duplicate_content(true)` 按正文算 64 位 FNV-1a 指纹，不同 URL 同正文只交付一次（镜像站、`?page=1` 空壳页、导航菜单撑大的页）。命中页仍然抽站内链接继续走——不抽等于把下游砍了。理论上有碰撞，后果是"少交一页"，不会把两页混成一页。
- **深度**：只按"距种子几跳"算，不猜路径层级。
- **TLS（可选 feature）**：`features = ["tls"]` 之后 `https://` 直接可用，引 `rustls` + `webpki-roots`（Mozilla 根证书编进二进制，不读系统 store——否则同一个站在不同机器上能不能爬取决于运营系统的配置）。**默认关**，`default = []`，不给不用 TLS 的人引任何依赖。证书校验必须开：跳过校验等于把"对面是谁"交给 DNS 决定。连接池的键带 scheme，`http://a.com` 与 `https://a.com` 不会共用一个槽位。https 走明文代理需要 `CONNECT` 隧道，那个不做，会明确报错。
- **会话 cookie**：默认开，per-host 的 jar，`Set-Cookie` 收、下次请求带。认 `Domain` / `Path` / `Secure` / `Max-Age` / `Expires`（RFC 1123 日期手写解析），`HttpOnly` 只记录（不跑 JS，没实际作用）。域名校验是双向的——别人想给 `a.com` 塞 cookie 不收，`a.com` 的 `Domain=.a.com` 才能往子域走。`Client::cookie_jar(false)` 回到无状态。每个 worker 一个 jar，不会出现两个 worker 共享半份登录态。
- **URL 过滤**：`.deny("*.pdf")` / `.allow("/docs/*")`，匹配对象是 `path?query`。

  只看两个字符：`*`（任意串，可跨 `/`）。是**整串**匹配不是子串包含——"任意前缀的分页"要写 `"*?page=*"`，写成 `"?page=*"` 是不匹配的（`?` 是查询分隔符，不能兼作通配符，一开始支持过，撞上真实写法就删了）。`deny` 优先；`allow` 非空即白名单。被拒的链接**不入队、不发请求**，所以这也是省流量和对方负载的地方。
- **优先级**：`.priority_fn(|u| …)` 给 URL 打分，同站内大的先抓、同分保持入队顺序（FIFO）。跨站**不比较**——按 host 轮转是礼貌的一部分，让高分站插队会把慢站饿死。种子也能带分（`.seed_with(url, p)`）。为什么用 `Fn(&Url)` 而不是让 handler 返回 `(url, priority)`：handler 签名是 v0.1 定下的，而"哪类 URL 更要紧"通常只看地址本身。
- **重定向**：最多 5 跳，`Page::url` 是最终地址，相对链接按最终地址解析。
- **keep-alive**：每个工作线程一个 `Client`，per-host 一条连接复用；复用到已被对端关掉的连接会自动重连一次（这是真实世界最常见的坑）。
- **body 形态**：`Content-Length` / `chunked` / 读到 EOF 三种都支持，超过 `max_body_bytes` 直接报错而不是把内存吃光。
- **HTML**：一趟扫描不建树，跳过 `<script>`/`<style>`/注释，解常见实体 + 数字实体。
- **sitemap**：`.sitemap(true)` 先抓 `robots.txt` 的 `Sitemap:` 和 `/sitemap.xml`，把清单里的地址当种子；`sitemapindex` 往下走两层（真实站点很少更深，而"不限层数"等于把递归交给对面）。
- **代理**：明文 http 代理（absolute-form 请求行），带凭据就直接带 `Proxy-Authorization: Basic …`，不吃"先 407 再重试"那一趟。池的键带代理地址，换代理不会复用旧连接。
- **断点续爬**：`.checkpoint("crawl.wal")` 把队列写成 WAL（`+ 深度 key` 入队、`- key` 抓完）。恢复时 `seen` 取所有 `+`、待抓取"有 `+` 无 `-`"的按原序重放。只记 `seen` 是不够的——中断时队列里"已入队没抓"的 URL 会丢，恢复后从种子重来还会因为种子在 `seen` 里直接空队列。
- **中间件**：三个钩子 `before` / `after` / `on_error`，返回 `Continue` / `Retry` / `Skip`。多个按注册顺序串联——任一 `Skip` 短路、任一 `Retry` 重试，重试次数有硬上限 5（中间件写错也拖不死 worker）。退避由 `Crawler::retry_backoff` 统一控制，中间件只决定"要不要再试一次"，两处都配会打架。
- **落盘**：`Sink` 是旁路——handler 决定"下一步抓什么"，sink 决定"抓到的往哪写"，两者生命周期不同。`JsonlSink` 写一行一页 JSON（`text` 截断 2000 字），手写转义而不引 `serde_json`。

## 明确不做

- **gzip / brotli**：请求里发 `Accept-Encoding: identity`，服务端就不会压。
- **JS 渲染**：不做 headless browser（那就是引 chromium 了）。
- **分布式调度**：单机爬虫，不做跨机器的 URL 分配和去重共识。断点续爬是单机维度的（见上），两件事别混。
- **`sitemap.xml.gz`**：解 gzip 要引库，遇到这种只能跳过（`.sitemap(true)` 静默忽略非 200 的分片）。
- **`CONNECT` 隧道（https 走明文代理）**：那需要在代理里开一条隧道。做了 `tls` feature 也不做它——多一条状态机不划算，先明确报错。
- **`SameSite` / CSP / 存储分区**：那是浏览器语义，爬虫没有跨站上下文，忽略。

## 和 rsboot 一起用

crate 本身不依赖 rsboot。典型组合：rsboot 里加一个管理端点，把 `Crawler` 丢进后台线程跑，进度用 `Stats` 回吐。

```rust
// 在 rsboot 的 handler 里起一个爬虫任务
router.get("/crawl/start", |_, _| {
    std::thread::spawn(|| {
        Crawler::new().seed("http://example.com/").max_pages(200).run(follow_internal);
    });
    Response::text("started")
});
```

如果想边爬边写库，就在 handler 里调 rsbatis——这也是 handler 设计成同步的原因：不用在 async 上下文里"block_on"一个数据库调用。

## 踩过的坑

- **"队列空了"不等于"没活了"**：worker 看到 `Empty` 就退出，而队列可能正好空在"某几发还在路上、抓完会推新链接"的那一刻。单站场景下这会直接把并发退化成 1（只有抢到种子的那个 worker 活着），而且尾部整段丢掉。现在 `Empty` 只在"队列没活 **且** 没有任何在飞的请求"时才报，否则给一个 1 ms 的 `Wait`。
- **每连接一条线程的假服务端才测得准并发**：rsboot 的服务端是一个 event loop 跑 handler，在 handler 里 `sleep` 会把整个服务端串起来，并发恒为 1 —— 拿它测"并发上限"会假通过。`tests/crawl_e2e.rs` 里那个 `threaded_serve` 就是为此写的。
- **链接里的参数要真的插值**：一开始把链接写成 `/p/{n}` 字面量，8 个"不同"链接其实指向同一个 URL，表现成"入队判重 1 次 + 只抓到一页"，看着像 crawler 的 bug。rsboot 的参数路由是 `.get("/p/{n}")`，页面里的链接得由 handler 用 `req.param("n")` 拼出来。

## 测试

- `url` / `html` / `robots` / `frontier`：纯逻辑单测，含等价写法去重、最长匹配、通配符、`$` 锚定、慢站不阻塞快站。
- `http`：用真 `TcpListener` 起假服务器，覆盖 chunked、短读、keep-alive 复用、复用死连接重试、重定向成环、body 上限。
- `tests/crawl_e2e.rs`：**用 rsboot 自己起一个站点**再爬它，验证 robots 拦截、重定向后的 URL 归属、回环页去重、`max_pages` 计数、sitemap 当种子、jsonl 行数、5xx 重试把页面救回来、UA 轮转与 `StaticHeaders` 真的写进了请求、断点续爬（中断 → 恢复 → 再跑一遍应零抓取）、`max_per_host` 之下的并发峰值（服务端自己数，见"踩过的坑"）、内容重复页被跳过后链接仍然继续走、`priority_fn` 能把"先详情后列表"的链接顺序翻过来、`deny` 掉的 `.pdf`/`.zip` 一次请求都没发出去、会话 cookie 真的被后续请求带上（关掉 jar 就 403）、连不上时不挂死。
