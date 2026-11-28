//! `rsboot-claw`：零依赖爬虫框架。
//!
//! 分层照 Scrapy（`spider-lib` 那套也是这个骨架）：
//!
//! | 层 | 在这里 | 干什么 |
//! | --- | --- | --- |
//! | Scheduler | [`frontier`] | 待抓队列、去重、深度、per-host 限速 |
//! | Downloader | [`http`] | HTTP/1.1 GET、keep-alive、重定向、chunked |
//! | Parser | [`html`] | 链接 / 标题 / 描述 / canonical / 可见文本 |
//! | Policy | [`robots`] | robots.txt：Allow / Disallow / Crawl-delay |
//! | Item | [`Page`] | 交给 handler 的抓取结果 |
//!
//! 与 rsboot 的关系：**没有关系**。这是独立 crate，不依赖 rsboot（
//! rsboot 只在 dev-dependencies 里，用来在 e2e 里起一个被爬的站点）。
//! 想把它挂到 rsboot 上，就在 handler 里自己拼 `Response`。
//!
//! # 最小用法
//!
//! ```no_run
//! use rsboot_claw::Crawler;
//!
//! let stats = Crawler::new()
//!     .seed("http://127.0.0.1:8080/")
//!     .max_depth(2)
//!     .workers(4)          // thread-per-core，和 rsboot 一个模型
//!     .obey_robots(true)
//!     .run(|page| {
//!         println!("{} {} {}", page.status, page.url, page.title);
//!         for link in page.links() {       // 只读：看它链到哪
//!             println!("  -> {link}");
//!         }
//!         rsboot_claw::follow_internal(page)   // 返回要跟进的地址（Vec<String>）
//!     });
//! println!("{:?}", stats);
//! ```

pub mod frontier;
pub mod html;
pub mod http;
pub mod middleware;
pub mod pipeline;
pub mod robots;
pub mod sitemap;
pub mod url;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub use frontier::{Frontier, Item, Pop, Push};
pub use html::{scan, Document, Link};
pub use pipeline::{CollectSink, JsonlSink, Sink};
pub use http::Client;
pub use middleware::{Action, Middleware, RetryPolicy, RotateUserAgent, StaticHeaders};
pub use robots::Robots;
pub use sitemap::Sitemap;
pub use url::Url;

/// 一次抓取的全部结果。handler 拿到的就是这个。
#[derive(Debug, Clone)]
pub struct Page {
    /// 跟随重定向之后的最终地址
    pub url: Url,
    pub status: u16,
    pub depth: u32,
    pub title: String,
    pub description: String,
    pub canonical: Option<String>,
    /// HTML 扫描结果。非 HTML 响应时是空的 `Document`
    pub doc: Document,
    pub body_len: usize,
    pub elapsed: Duration,
}

impl Page {
    /// 把页面里的链接解析成绝对地址，丢掉解析失败的。
    ///
    /// `nofollow` 默认**不**过滤：爬虫的第一职责是把站点抓全，
    /// 是否遵守 nofollow 是策略问题，交给调用方自己用 `doc.links` 判。
    pub fn links(&self) -> Vec<Url> {
        let base = match &self.doc.base {
            Some(b) => Url::resolve(&self.url, b).unwrap_or_else(|| self.url.clone()),
            None => self.url.clone(),
        };
        self.doc
            .links
            .iter()
            .filter_map(|l| Url::resolve(&base, &l.href))
            .collect()
    }

    /// 只要同站链接（站内爬最常用）。
    pub fn internal_links(&self) -> Vec<Url> {
        self.links()
            .into_iter()
            .filter(|u| u.host == self.url.host && u.real_port() == self.url.real_port())
            .collect()
    }

    pub fn text(&self) -> &str {
        &self.doc.text
    }

    /// 一行 JSON，给它做落盘/喂消息队列用。
    ///
    /// 手写而不是 derive：核心零依赖，`serde_json` 进不来；
    /// 而这里只需要"把几个字段转义成合法 JSON"这一件事。
    /// `text` 截断到 [`Self::JSON_TEXT_LIMIT`] 字符，正文整段塞进日志没有意义。
    pub fn to_json(&self) -> String {
        let mut s = String::with_capacity(256);
        s.push_str("{\"url\":");
        s.push_str(&json_string(&self.url.to_string()));
        s.push_str(",\"status\":");
        s.push_str(&self.status.to_string());
        s.push_str(",\"depth\":");
        s.push_str(&self.depth.to_string());
        s.push_str(",\"title\":");
        s.push_str(&json_string(&self.title));
        s.push_str(",\"description\":");
        s.push_str(&json_string(&self.description));
        s.push_str(",\"canonical\":");
        match &self.canonical {
            Some(c) => s.push_str(&json_string(c)),
            None => s.push_str("null"),
        }
        s.push_str(",\"links\":");
        s.push_str(&self.doc.links.len().to_string());
        s.push_str(",\"headings\":");
        s.push_str(&self.doc.headings.len().to_string());
        s.push_str(",\"text_len\":");
        s.push_str(&self.doc.text.chars().count().to_string());
        s.push_str(",\"bytes\":");
        s.push_str(&self.body_len.to_string());
        s.push_str(",\"ms\":");
        s.push_str(&format!("{:.1}", self.elapsed.as_secs_f64() * 1000.0));
        s.push_str(",\"text\":");
        let text: String = self
            .doc
            .text
            .chars()
            .take(Self::JSON_TEXT_LIMIT)
            .collect();
        s.push_str(&json_string(&text));
        s.push('}');
        s
    }

    /// `to_json` 里 `text` 的截断长度。
    pub const JSON_TEXT_LIMIT: usize = 2000;
}

/// 把一段文本转成 JSON 字符串字面量（含两端引号）。
///
/// 转义的是 JSON 规范要求的最小集合：`"` `\\` 和 0x00–0x1F。
/// `/` 不转义（合法）、非 ASCII 不转（合法且省字节）。
pub fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// 抓取任务借走一个 per-host 并发名额，Drop 时归还。
///
/// 用 Drop 而不是在流程末尾手写一句 `release`：中间任何一步提前 `continue`、
/// 或者用户的 handler panic 展开，都会走到 Drop。
struct Release<'a> {
    frontier: &'a Mutex<Frontier>,
    host: Box<str>,
}

impl Drop for Release<'_> {
    fn drop(&mut self) {
        self.frontier.lock().unwrap().release(&self.host);
    }
}

/// 正文指纹：64 位 FNV-1a。
///
/// 为什么不引哈希库：这是零依赖 crate；而这里只需要"两次同样的文本给同样的数"，
/// FNV 够用且够快（一个字节一轮 `xor` + 乘）。正文全空时用标题兜底，
/// 否则所有图片页/空壳页会一起被判成同一页。
fn content_fingerprint(page: &Page) -> u64 {
    let text = if page.doc.text.is_empty() {
        page.doc.title.as_str()
    } else {
        page.doc.text.as_str()
    };
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in text.as_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// 一次 `run` 的汇总。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stats {
    /// 成功抓到的页面数（含 4xx/5xx——`status` 里能看出来）
    pub pages: usize,
    pub bytes: usize,
    /// 网络/协议错误
    pub errors: usize,
    /// 被 robots.txt 挡掉的
    pub skipped_robots: usize,
    /// 入队时被判重的（同一个 URL）
    pub duplicates: usize,
    /// 抓到了但正文和之前某页一样的（开了 `avoid_duplicate_content` 才会计数）。
    /// 这些页不算进 `pages`——它们没有交付出去
    pub duplicates_content: usize,
    /// 入队时因超深被丢的
    pub too_deep: usize,
    /// 中间件要求重试的次数（含最后那次失败的尝试）
    pub retries: usize,
    /// 中间件要求跳过的页数
    pub skipped: usize,
    /// 从断点日志恢复出的待抓条数（没开 checkpoint 就是 0）
    pub resumed: usize,
    /// 网络错误详情（每类一条，最多 32 条）
    pub error_kinds: Vec<String>,
    pub elapsed: Duration,
    /// 每秒抓到几个页面
    pub pages_per_sec: f64,
}

#[derive(Default)]
struct Counters {
    pages: usize,
    bytes: usize,
    duplicates_content: usize,
    errors: usize,
    skipped_robots: usize,
    retries: usize,
    skipped: usize,
    error_kinds: Vec<String>,
}

impl Counters {
    fn note_error(&mut self, e: &http::Error) {
        self.errors += 1;
        if self.error_kinds.len() < 32 {
            let kind = format!("{e}");
            // 只留前半段（URL 之类的东西会把同类错误拆成很多行）
            let head: String = kind.chars().take(40).collect();
            if !self.error_kinds.contains(&head) {
                self.error_kinds.push(head);
            }
        }
    }
}

/// 爬虫配置。所有方法都 consume self 返回 self，可以链式写。
#[derive(Clone)]
pub struct Crawler {
    seeds: Vec<(Url, i32)>,
    max_depth: u32,
    workers: usize,
    delay: Option<Duration>,
    obey_robots: bool,
    user_agent: String,
    max_pages: Option<usize>,
    same_host_only: bool,
    timeout: Duration,
    max_body_bytes: usize,
    sitemap: bool,
    sink: Option<Arc<dyn Sink>>,
    middlewares: Vec<Arc<dyn Middleware>>,
    retry_backoff: Duration,
    checkpoint: Option<PathBuf>,
    max_per_host: usize,
    avoid_duplicate_content: bool,
    priority_fn: Option<PriorityFn>,
}

/// 给一条 URL 打分。见 [`Crawler::priority_fn`]。
type PriorityFn = Arc<dyn Fn(&Url) -> i32 + Send + Sync>;

impl Crawler {
    pub fn new() -> Self {
        Self {
            seeds: Vec::new(),
            max_depth: 2,
            workers: 4,
            delay: None,
            obey_robots: false,
            user_agent: format!("rsboot-claw/{}", env!("CARGO_PKG_VERSION")),
            max_pages: None,
            same_host_only: true,
            timeout: Duration::from_secs(10),
            max_body_bytes: 4 * 1024 * 1024,
            sitemap: false,
            sink: None,
            middlewares: Vec::new(),
            retry_backoff: Duration::from_millis(200),
            checkpoint: None,
            max_per_host: 0,
            avoid_duplicate_content: false,
            priority_fn: None,
        }
    }

    /// 加一个种子。相对地址在这里没有意义，会直接 panic——
    /// 种子是用户手写的，静默丢掉比报错更难查。
    pub fn seed(self, url: &str) -> Self {
        self.seed_with(url, 0)
    }

    /// 带优先级的种子。同一条队列里大的先抓（只在同一个站内比较）。
    ///
    /// 典型用法：把首页/列表页给个正数、把"已知很深"的入口给负数，
    /// 这样第一次 `run` 的抓取顺序更接近"先铺开再捞细"。
    pub fn seed_with(mut self, url: &str, priority: i32) -> Self {
        match Url::parse(url) {
            Some(u) => self.seeds.push((u, priority)),
            None => panic!("seed must be an absolute http(s) URL: {url}"),
        }
        self
    }

    /// 按 URL 算入队优先级。抓到的每条链接、以及 sitemap 种子都过一遍它。
    ///
    /// 为什么是 `Fn(&Url)` 而不是让 handler 返回 `(url, priority)`：handler 的签名
    /// 是 v0.1 就定下的 `Fn(&Page) -> Vec<String>`，改它会动到所有已写好的调用方。
    /// 而"哪类 URL 更要紧"通常只看地址本身（`/list` vs `/detail`、有没有 `?page=`），
    /// 不需要看页面内容。
    pub fn priority_fn<F>(mut self, f: F) -> Self
    where
        F: Fn(&Url) -> i32 + Send + Sync + 'static,
    {
        self.priority_fn = Some(Arc::new(f));
        self
    }

    fn priority_for(&self, u: &Url) -> i32 {
        self.priority_fn.as_ref().map_or(0, |f| f(u))
    }

    pub fn max_depth(mut self, d: u32) -> Self {
        self.max_depth = d;
        self
    }

    pub fn workers(mut self, n: usize) -> Self {
        self.workers = n.max(1);
        self
    }

    /// 同一个站两次请求之间至少隔这么久。会被 robots.txt 的 `Crawl-delay`
    /// 覆盖（取更长的那个由 `set_host_delay` 负责）。
    pub fn delay(mut self, d: Duration) -> Self {
        self.delay = Some(d);
        self
    }

    pub fn obey_robots(mut self, yes: bool) -> Self {
        self.obey_robots = yes;
        self
    }

    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = ua.into();
        self
    }

    /// 抓满这么多页就停。压测和"只抓前 100 页"的场景用。
    pub fn max_pages(mut self, n: usize) -> Self {
        self.max_pages = Some(n);
        self
    }

    /// 只跟同 host 的链接。默认 true：绝大多数爬虫不想不小心爬出站。
    pub fn same_host_only(mut self, yes: bool) -> Self {
        self.same_host_only = yes;
        self
    }

    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }

    pub fn max_body_bytes(mut self, n: usize) -> Self {
        self.max_body_bytes = n;
        self
    }

    /// 开跑前先抓 `robots.txt` 里的 `Sitemap:` 与 `/sitemap.xml`，把清单里的
    /// 地址当种子塞进队列（`sitemapindex` 会往下走两层）。
    ///
    /// 比爬链接快得多：一个 sitemap 就是站点自己给的全量清单。
    pub fn sitemap(mut self, yes: bool) -> Self {
        self.sitemap = yes;
        self
    }

    /// 每抓完一页调一次。和 handler 的区别：sink 是"旁路"，拿不到返回的链接、
    /// 也不影响调度；它用来落盘/打点，多个 worker 共享一个。
    pub fn sink(mut self, s: Arc<dyn Sink>) -> Self {
        self.sink = Some(s);
        self
    }

    /// 加一个中间件。按注册顺序串联：任一 `Skip` 就短路，任一 `Retry` 就重试。
    ///
    /// 重试有硬上限（[`Crawler::MAX_ATTEMPTS`]），中间件写错也不会拖成死循环。
    pub fn middleware(mut self, m: Arc<dyn Middleware>) -> Self {
        self.middlewares.push(m);
        self
    }

    /// 开断点续爬：把队列写进一个 WAL 文件（`+ 深度 key` 入队、`- key` 抓完）。
    ///
    /// 下次用同一个文件启动，未抓完的 URL 会自动回到队列，抓过的不会重抓。
    /// 日志是追加写的纯文本，一行一条，可以直接 `grep` 看进度。
    pub fn checkpoint(mut self, path: impl Into<PathBuf>) -> Self {
        self.checkpoint = Some(path.into());
        self
    }

    /// 重试的退避基数：第 n 次重试前睡 `n * base`（线性，不做指数——
    /// 爬虫的重试窗口本来就短，指数退避只会让一次抓取拖到超时）。
    pub fn retry_backoff(mut self, d: Duration) -> Self {
        self.retry_backoff = d;
        self
    }

    /// 同一个站同时最多几个请求在飞。默认 `0` = 不限（只受 `workers` 约束）。
    ///
    /// 和 `.delay()` 管的是两件事：`delay` 管"这个站两次请求隔多久"，这个管
    /// "同时几发"。只限速不限并发时，`workers(8)` 会在 `delay=0` 下同时向
    /// 同一个站开 8 发——对人家不够客气，而自己这边 8 个 worker 也全被
    /// 这一个站占住。多个种子站时建议设成 2–4。
    pub fn max_per_host(mut self, n: usize) -> Self {
        self.max_per_host = n;
        self
    }

    /// 正文指纹去重：不同 URL 但正文一样的页，只交付一次。
    ///
    /// 对付的是镜像站、分页参数只换 `?page=1` 的空壳页、以及导航菜单
    /// 撑起来的"看起来不一样其实一样"的页。命中的页**仍然会抽取站内链接**
    /// 继续往下走（不然等于把下游砍了），但不会进 sink、也不会调 handler。
    ///
    /// 指纹是 64 位 FNV-1a，理论上有碰撞——碰撞的结果是**少交一页**，
    /// 不会把不同的页混成同一页，所以这个概率可以接受。
    pub fn avoid_duplicate_content(mut self, yes: bool) -> Self {
        self.avoid_duplicate_content = yes;
        self
    }

    /// 开跑。handler 在每个工作线程里被同步调用，返回要跟进的地址
    /// （相对地址按当前页面解析）。
    pub fn run<H>(self, handler: H) -> Stats
    where
        H: Fn(&Page) -> Vec<String> + Sync,
    {
        let start = Instant::now();
        let (frontier, resumed) = match &self.checkpoint {
            Some(path) => match Frontier::resume(path, self.max_depth, self.delay) {
                Ok((f, n)) => (Arc::new(Mutex::new(f)), n),
                // 日志读不出来（权限/损坏）不该让整次抓取失败：退回"从头开始"
                Err(_) => (
                    Arc::new(Mutex::new(Frontier::new(self.max_depth, self.delay))),
                    0,
                ),
            },
            None => (
                Arc::new(Mutex::new(Frontier::new(self.max_depth, self.delay))),
                0,
            ),
        };

        frontier
            .lock()
            .unwrap()
            .set_max_per_host(self.max_per_host);

        // sitemap 要在开跑**之前**抓完：它是种子级的输入，跑起来之后再抓
        // 会让"种子为什么这么少"变得不可解释。
        let extra_seeds = if self.sitemap {
            self.seed_from_sitemaps()
        } else {
            Vec::new()
        };

        {
            let now = Instant::now();
            let mut f = frontier.lock().unwrap();
            for u in &extra_seeds {
                f.push(
                    Item {
                        url: u.clone(),
                        depth: 0,
                        referer: Some("sitemap".to_string()),
                        priority: self.priority_for(u),
                    },
                    now,
                );
            }
            for (s, p) in &self.seeds {
                f.push(
                    Item {
                        url: s.clone(),
                        depth: 0,
                        referer: None,
                        priority: *p,
                    },
                    now,
                );
            }
        }

        let counters = Mutex::new(Counters::default());
        let seen_content: Mutex<HashSet<u64>> = Mutex::new(HashSet::new());
        let robots_cache: Mutex<HashMap<String, Arc<Robots>>> = Mutex::new(HashMap::new());
        let fetched = AtomicUsize::new(0);
        let stop = AtomicBool::new(false);
        let cfg = &self;

        std::thread::scope(|scope| {
            for _ in 0..self.workers {
                scope.spawn(|| {
                    let mut client = Client::new()
                        .user_agent(cfg.user_agent.clone())
                        .timeout(cfg.timeout)
                        .max_body_bytes(cfg.max_body_bytes);
                    loop {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        let item = {
                            let mut f = frontier.lock().unwrap();
                            match f.pop(Instant::now()) {
                                Pop::Ready(i) => i,
                                Pop::Wait(d) => {
                                    drop(f);
                                    std::thread::sleep(d.min(Duration::from_millis(50)));
                                    continue;
                                }
                                Pop::Empty => break,
                            }
                        };

                        // 拿到了这个任务就把该站的并发名额占住，抓完（或 panic 展开）
                        // 由 Drop 归还。不归还的话这个站的并发上限会一路掉到 0
                        let _slot = Release {
                            frontier: &frontier,
                            host: item.url.host_header().into_boxed_str(),
                        };

                        // 名额在开抓**之前**占：抓到才算数的话，4 个 worker 会各多抓一页
                        if let Some(max) = cfg.max_pages {
                            if fetched.fetch_add(1, Ordering::Relaxed) >= max {
                                stop.store(true, Ordering::Relaxed);
                                break;
                            }
                        }

                        if cfg.obey_robots {
                            let rules = robots_for(&mut client, &robots_cache, &item.url, cfg);
                            if !rules.allowed(&item.url.request_target()) {
                                counters.lock().unwrap().skipped_robots += 1;
                                continue;
                            }
                            if let Some(d) = rules.crawl_delay {
                                frontier.lock().unwrap().set_host_delay(&item.url.host_header(), d);
                            }
                        }

                        // 一次抓取可能被中间件要求重试；这里是一个带硬上限的循环
                        let mut headers: Vec<(String, String)> = Vec::new();
                        let mut attempt = 0usize;
                        let mut t0 = Instant::now();
                        let resp = loop {
                            headers.clear();
                            let mut before = Action::Continue;
                            for mw in &cfg.middlewares {
                                match mw.before(&item.url, &mut headers, attempt) {
                                    Action::Skip => {
                                        before = Action::Skip;
                                        break;
                                    }
                                    Action::Retry => before = Action::Retry,
                                    Action::Continue => {}
                                }
                            }
                            if before == Action::Skip {
                                counters.lock().unwrap().skipped += 1;
                                break None;
                            }

                            t0 = Instant::now();
                            match client.get_with(&item.url, &headers) {
                                Ok(r) => {
                                    let mut after = Action::Continue;
                                    for mw in &cfg.middlewares {
                                        match mw.after(&item.url, r.status, attempt) {
                                            Action::Skip => {
                                                after = Action::Skip;
                                                break;
                                            }
                                            Action::Retry => after = Action::Retry,
                                            Action::Continue => {}
                                        }
                                    }
                                    match after {
                                        Action::Skip => {
                                            counters.lock().unwrap().skipped += 1;
                                            break None;
                                        }
                                        Action::Retry if attempt + 1 < Crawler::MAX_ATTEMPTS => {
                                            counters.lock().unwrap().retries += 1;
                                            attempt += 1;
                                            std::thread::sleep(cfg.backoff_at(attempt));
                                            continue;
                                        }
                                        _ => break Some(r),
                                    }
                                }
                                Err(e) => {
                                    let mut retry = false;
                                    for mw in &cfg.middlewares {
                                        if mw.on_error(&item.url, &e, attempt) == Action::Retry {
                                            retry = true;
                                        }
                                    }
                                    if retry && attempt + 1 < Crawler::MAX_ATTEMPTS {
                                        counters.lock().unwrap().retries += 1;
                                        attempt += 1;
                                        std::thread::sleep(cfg.backoff_at(attempt));
                                        continue;
                                    }
                                    counters.lock().unwrap().note_error(&e);
                                    if let Some(sink) = &cfg.sink {
                                        sink.on_error(&item.url, &e.to_string());
                                    }
                                    break None;
                                }
                            }
                        };
                        let Some(resp) = resp else { continue };

                        let doc = if resp.is_html() {
                            scan(&resp.text())
                        } else if resp.content_type().starts_with("text/") {
                            // 纯文本也把内容留在 `Page::text()` 里：抓下来的东西
                            // 不该因为不是 HTML 就整段丢掉
                            Document {
                                text: resp.text().trim().to_string(),
                                ..Default::default()
                            }
                        } else {
                            Document::default()
                        };
                        let page = Page {
                            url: resp.url.clone(),
                            status: resp.status,
                            depth: item.depth,
                            title: doc.title.clone(),
                            description: doc.description.clone(),
                            canonical: doc.canonical.clone(),
                            doc,
                            body_len: resp.body.len(),
                            elapsed: t0.elapsed(),
                        };

                        if cfg.avoid_duplicate_content {
                            let fp = content_fingerprint(&page);
                            if !seen_content.lock().unwrap().insert(fp) {
                                {
                                    let mut c = counters.lock().unwrap();
                                    c.duplicates_content += 1;
                                    c.bytes += resp.body.len();
                                }
                                // 内容一样不代表链接一样（镜像站往往是另一套导航），
                                // 所以下游照走：只抽站内链接，不惊动 sink 和 handler
                                let now = Instant::now();
                                let mut f = frontier.lock().unwrap();
                                for u in page.internal_links() {
                                    let priority = cfg.priority_for(&u);
                                    f.push(
                                        Item {
                                            url: u,
                                            depth: item.depth + 1,
                                            referer: Some(page.url.to_string()),
                                            priority,
                                        },
                                        now,
                                    );
                                }
                                continue;
                            }
                        }

                        {
                            let mut c = counters.lock().unwrap();
                            c.pages += 1;
                            c.bytes += resp.body.len();
                        }

                        if let Some(sink) = &cfg.sink {
                            sink.on_page(&page);
                        }
                        let next = handler(&page);
                        let now = Instant::now();
                        let mut f = frontier.lock().unwrap();
                        for href in next {
                            let Some(u) = Url::resolve(&page.url, &href) else {
                                continue;
                            };
                            if cfg.same_host_only
                                && (u.host != page.url.host || u.real_port() != page.url.real_port())
                            {
                                continue;
                            }
                            let priority = cfg.priority_for(&u);
                            f.push(
                                Item {
                                    url: u,
                                    depth: item.depth + 1,
                                    referer: Some(page.url.to_string()),
                                    priority,
                                },
                                now,
                            );
                        }
                    }
                });
            }
        });

        let c = counters.into_inner().unwrap();
        let (dups, deep) = {
            let f = frontier.lock().unwrap();
            f.rejected()
        };
        let elapsed = start.elapsed();
        Stats {
            pages: c.pages,
            bytes: c.bytes,
            errors: c.errors,
            skipped_robots: c.skipped_robots,
            duplicates: dups,
            duplicates_content: c.duplicates_content,
            too_deep: deep,
            retries: c.retries,
            skipped: c.skipped,
            resumed,
            error_kinds: c.error_kinds,
            pages_per_sec: if elapsed.as_secs_f64() > 0.0 {
                c.pages as f64 / elapsed.as_secs_f64()
            } else {
                0.0
            },
            elapsed,
        }
    }
}

impl Crawler {
    /// 一次抓取最多尝试几次（含首次）。这是**硬上限**：
    /// 中间件说 `Retry` 也不能突破它，否则一个写错的中间件就能让 worker 卡死。
    pub const MAX_ATTEMPTS: usize = 5;

    fn backoff_at(&self, attempt: usize) -> Duration {
        self.retry_backoff * attempt.max(1) as u32
    }

    /// 抓 `robots.txt` 的 `Sitemap:` + `/sitemap.xml`，展开成种子。
    ///
    /// `sitemapindex` 往下走两层：真实站点的分片基本不会更深，
    /// 而"不限层数"等于把递归交给对面控制。
    fn seed_from_sitemaps(&self) -> Vec<Url> {
        let mut out = Vec::new();
        let mut client = Client::new()
            .user_agent(self.user_agent.clone())
            .timeout(self.timeout)
            .max_body_bytes(self.max_body_bytes);

        for seed in &self.seeds {
            let host = seed.0.host_header();
            let mut queue: Vec<String> = Vec::new();

            if let Some(u) = Url::parse(&format!("http://{host}/robots.txt")) {
                if let Ok(r) = client.get(&u) {
                    if r.status == 200 {
                        queue.extend(Robots::parse(&r.text(), &self.user_agent).sitemaps);
                    }
                }
            }
            queue.push(format!("http://{host}/sitemap.xml"));

            for _depth in 0..2 {
                if queue.is_empty() {
                    break;
                }
                let mut next = Vec::new();
                for loc in std::mem::take(&mut queue) {
                    let Some(u) = Url::parse(&loc) else { continue };
                    let Ok(r) = client.get(&u) else { continue };
                    if r.status != 200 {
                        continue;
                    }
                    let sm = sitemap::parse(&r.text());
                    if sm.is_index {
                        next.extend(sm.locs());
                    } else {
                        for l in sm.locs() {
                            if let Some(u) = Url::parse(&l) {
                                out.push(u);
                            }
                        }
                    }
                }
                queue = next;
            }
        }

        if self.same_host_only {
            let hosts: Vec<(String, u16)> = self
                .seeds
                .iter()
                .map(|(s, _)| (s.host.clone(), s.real_port()))
                .collect();
            out.retain(|u| hosts.iter().any(|(h, p)| *h == u.host && *p == u.real_port()));
        }
        out
    }
}

impl Default for Crawler {
    fn default() -> Self {
        Self::new()
    }
}

/// 抓 robots.txt（带缓存）。抓不到就当没规则。
fn robots_for(
    client: &mut Client,
    cache: &Mutex<HashMap<String, Arc<Robots>>>,
    url: &Url,
    cfg: &Crawler,
) -> Arc<Robots> {
    let host = url.host_header();
    if let Some(r) = cache.lock().unwrap().get(&host) {
        return r.clone();
    }
    let robots_url = Url::parse(&format!("http://{host}/robots.txt"));
    let rules = match robots_url {
        Some(u) => match client.get(&u) {
            Ok(resp) if resp.status == 200 => {
                Arc::new(Robots::parse(&resp.text(), &cfg.user_agent))
            }
            _ => Arc::new(Robots::allow_all()),
        },
        None => Arc::new(Robots::allow_all()),
    };
    cache.lock().unwrap().insert(host, rules.clone());
    rules
}

/// 最常用的 handler：跟进所有站内链接。
pub fn follow_internal(page: &Page) -> Vec<String> {
    page.internal_links()
        .into_iter()
        .map(|u| u.to_string())
        .collect()
}
