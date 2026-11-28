# rsboot-claw

零依赖爬虫框架，rsboot 的扩展 crate。和 `rsboot-core` 一样守"不引第三方依赖"这条线：URL 解析、HTML 扫描、robots.txt、HTTP 客户端全在仓库里。

```
cargo test -p rsboot-claw          # 48 项（40 单测 + 7 e2e + 1 doctest）
```

## 快速开始

```rust
use std::sync::Arc;
use rsboot_claw::{follow_internal, Crawler, JsonlSink};

let sink = Arc::new(JsonlSink::create("pages.jsonl")?);   // 一行一页 JSON

let stats = Crawler::new()
    .seed("http://127.0.0.1:9001/")
    .max_depth(3)
    .workers(4)              // thread-per-core，和 rsboot 一个模型
    .obey_robots(true)       // 读 robots.txt，遵守 Crawl-delay
    .delay(Duration::from_millis(200))
    .max_pages(1000)
    .sitemap(true)           // 先抓 robots 的 Sitemap: + /sitemap.xml 当种子
    .sink(sink.clone())      // 旁路落盘，和"下一步抓什么"解耦
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
| Parser | `html.rs` | 链接 / 标题 / 描述 / canonical / `<base>` / 可见文本 |
| Policy | `robots.rs` | Allow / Disallow / Crawl-delay / Sitemap |
| Pipeline | `pipeline.rs` | 结果旁路：`Sink` trait + `JsonlSink`（一行一页）+ `CollectSink` |
| Item | `lib.rs` `Page` | 交给 handler 的结果 |

## 从哪些实现里 fork 了什么

调研了 Rust 侧四个主流实现，**fork 的是架构而不是代码**：

- [`spider`](https://github.com/spider-rs/spider)（MIT，Rust 生态最主流）：fork 它的 **frontier + robots + sitemap** 思路——按 host 组织队列、礼貌限速、robots 最长匹配。它是 tokio async，rsboot 是 thread-per-core + 栈协程，async 的调度器搬不过来。
- [`spider-lib`](https://crates.io/crates/spider-lib)：fork 它的 **Scrapy 分层**（Scheduler / Middleware / Pipeline / Stats）和"checkpoint 可恢复"的设计取向。v0.1 落了 Scheduler 与 Stats，v0.2 补上 Pipeline（`Sink`），Middleware 留到后面。
- `kumo`：Scrapy-for-Rust 的另一条路线，参考它"用 trait 描述 Spider"的做法，但这里用闭包 `Fn(&Page)` 更贴近 rsboot 的 handler 风格。
- `fastCRW`：只借了它的"结果即 `Page`"命名。

**为什么不直接依赖 `spider`**：它把 tokio、reqwest、chromium 一起带进来；一个零依赖框架的扩展 crate 装不下这些。同样、同样不引 `scraper`/`select`——它们的 CSS 选择器值钱，但爬虫 80% 的需求就是"取链接 + 取标题"，`html.rs` 两百多行就能覆盖。

## 做了什么

- **去重**：以 `scheme://host[:port]/path?query` 为键，`http://a.com` 与 `http://a.com/`、`:80` 与无端口都归一成同一个 key。
- **礼貌**：`delay` 是 per-host 的；某站冷却时其它站照抓（这是按 host 分桶的唯一理由）。robots 的 `Crawl-delay` 会自动抬高该站的 delay。
- **深度**：只按"距种子几跳"算，不猜路径层级。
- **重定向**：最多 5 跳，`Page::url` 是最终地址，相对链接按最终地址解析。
- **keep-alive**：每个工作线程一个 `Client`，per-host 一条连接复用；复用到已被对端关掉的连接会自动重连一次（这是真实世界最常见的坑）。
- **body 形态**：`Content-Length` / `chunked` / 读到 EOF 三种都支持，超过 `max_body_bytes` 直接报错而不是把内存吃光。
- **HTML**：一趟扫描不建树，跳过 `<script>`/`<style>`/注释，解常见实体 + 数字实体。
- **sitemap**：`.sitemap(true)` 先抓 `robots.txt` 的 `Sitemap:` 和 `/sitemap.xml`，把清单里的地址当种子；`sitemapindex` 往下走两层（真实站点很少更深，而"不限层数"等于把递归交给对面）。
- **落盘**：`Sink` 是旁路——handler 决定"下一步抓什么"，sink 决定"抓到的往哪写"，两者生命周期不同。`JsonlSink` 写一行一页 JSON（`text` 截断 2000 字），手写转义而不引 `serde_json`。

## 明确不做

- **HTTPS**：要 TLS 就得引 `rustls`/`native-tls`，和零依赖冲突。`https://` 会返回 `Error::UnsupportedScheme`，调用方自己决定跳过还是报错。真需要 HTTPS，在这个 crate 外面套一层支持 TLS 的 Downloader 即可——`Client` 的接口只有一个 `get(&Url) -> Result<Response>`。
- **gzip / brotli**：请求里发 `Accept-Encoding: identity`，服务端就不会压。
- **JS 渲染**：不做 headless browser（那就是引 chromium 了）。
- **分布式 / 断点续传**：`Frontier` 是内存里的，重启即从种子重来。需要持久化就把 `seen` 换成一个可写后端——`Frontier` 已经把"去重判定"和"调度"分开，替换点是明确的。
- **`sitemap.xml.gz`**：解 gzip 要引库，遇到这种只能跳过（`.sitemap(true)` 静默忽略非 200 的分片）。
- **cookie / 登录态**：需要的话在 handler 外面自己维护，v0.1 的 `Client` 没有 cookie jar。

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

## 测试

- `url` / `html` / `robots` / `frontier`：纯逻辑单测，含等价写法去重、最长匹配、通配符、`$` 锚定、慢站不阻塞快站。
- `http`：用真 `TcpListener` 起假服务器，覆盖 chunked、短读、keep-alive 复用、复用死连接重试、重定向成环、body 上限。
- `tests/crawl_e2e.rs`：**用 rsboot 自己起一个站点**再爬它，验证 robots 拦截、重定向后的 URL 归属、回环页去重、`max_pages` 计数、sitemap 当种子、jsonl 行数、连不上时不挂死。
