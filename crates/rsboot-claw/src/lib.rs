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
pub mod robots;
pub mod url;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub use frontier::{Frontier, Item, Pop, Push};
pub use html::{scan, Document, Link};
pub use http::Client;
pub use robots::Robots;
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
    /// 入队时被判重的
    pub duplicates: usize,
    /// 入队时因超深被丢的
    pub too_deep: usize,
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
    errors: usize,
    skipped_robots: usize,
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
    seeds: Vec<Url>,
    max_depth: u32,
    workers: usize,
    delay: Option<Duration>,
    obey_robots: bool,
    user_agent: String,
    max_pages: Option<usize>,
    same_host_only: bool,
    timeout: Duration,
    max_body_bytes: usize,
}

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
        }
    }

    /// 加一个种子。相对地址在这里没有意义，会直接 panic——
    /// 种子是用户手写的，静默丢掉比报错更难查。
    pub fn seed(mut self, url: &str) -> Self {
        match Url::parse(url) {
            Some(u) => self.seeds.push(u),
            None => panic!("seed must be an absolute http(s) URL: {url}"),
        }
        self
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

    /// 开跑。handler 在每个工作线程里被同步调用，返回要跟进的地址
    /// （相对地址按当前页面解析）。
    pub fn run<H>(self, handler: H) -> Stats
    where
        H: Fn(&Page) -> Vec<String> + Sync,
    {
        let start = Instant::now();
        let frontier = Arc::new(Mutex::new(Frontier::new(self.max_depth, self.delay)));
        {
            let now = Instant::now();
            let mut f = frontier.lock().unwrap();
            for s in &self.seeds {
                f.push(
                    Item {
                        url: s.clone(),
                        depth: 0,
                        referer: None,
                    },
                    now,
                );
            }
        }

        let counters = Mutex::new(Counters::default());
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

                        let t0 = Instant::now();
                        let resp = match client.get(&item.url) {
                            Ok(r) => r,
                            Err(e) => {
                                counters.lock().unwrap().note_error(&e);
                                continue;
                            }
                        };

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

                        {
                            let mut c = counters.lock().unwrap();
                            c.pages += 1;
                            c.bytes += resp.body.len();
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
                            f.push(
                                Item {
                                    url: u,
                                    depth: item.depth + 1,
                                    referer: Some(page.url.to_string()),
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
            too_deep: deep,
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
