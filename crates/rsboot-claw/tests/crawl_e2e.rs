//! 端到端：用 rsboot 自己起一个被爬的站点，claw 真去爬它。
//!
//! 为什么不 mock HTTP：这一层要验证的是"整条链路咬不咬得上"——
//! robots 缓存、redirect 之后的 URL 归属、相对链接按 `<base>`/当前页解析、
//! 去重在多线程抢队列时还成不成立。这些只有真 socket + 真服务端才验得出来。
//!
//! 顺便也是"扩展 crate 与 rsboot 能一起用"的证明：被爬的站点就是 rsboot 起的。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rsboot::config::Config;
use rsboot::web::http::Status;
use rsboot::web::response::Response;
use rsboot::web::route::Router;
use rsboot::App;

use rsboot_claw::{follow_internal, CollectSink, Crawler, RetryPolicy, RotateUserAgent, StaticHeaders};

#[test]
fn retry_policy_recovers_from_5xx() {
    let addr = format!("127.0.0.1:{}", free_port());
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let h = hits.clone();
    let router = Router::new()
        .get("/flaky", move |_, _| {
            let n = h.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Response::new(Status::BAD_GATEWAY)
            } else {
                Response::text("ok")
            }
        })
        .unwrap();
    let (addr, stop) = serve_at(addr, router);
    let stats = Crawler::new()
        .seed(&format!("http://{addr}/flaky"))
        .max_depth(0)
        .workers(1)
        .retry_backoff(Duration::from_millis(5))
        .middleware(Arc::new(RetryPolicy::new().retries(2)))
        .run(|_| Vec::new());
    stop.store(true, Ordering::Relaxed);

    assert_eq!(stats.pages, 1, "重试应当把这一页救回来");
    assert_eq!(stats.retries, 1);
    assert_eq!(stats.errors, 0, "{:?}", stats.error_kinds);
    assert_eq!(hits.load(Ordering::SeqCst), 2, "服务端应当被打了两次");
}

#[test]
fn middleware_headers_reach_the_server_and_ua_rotates() {
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let rec = seen.clone();
    let addr = format!("127.0.0.1:{}", free_port());
    let router = Router::new()
        .get("/", |_, _| {
            Response::html(r#"<a href="/p1">1</a><a href="/p2">2</a><a href="/p3">3</a>"#)
        })
        .unwrap()
        .get("/p1", {
            let rec = rec.clone();
            move |req, _| {
                rec.lock().unwrap().push(format!(
                    "{}|{}",
                    req.header("user-agent").unwrap_or("-"),
                    req.header("accept-language").unwrap_or("-")
                ));
                Response::text("p1")
            }
        })
        .unwrap()
        .get("/p2", {
            let rec = rec.clone();
            move |req, _| {
                rec.lock().unwrap().push(format!(
                    "{}|{}",
                    req.header("user-agent").unwrap_or("-"),
                    req.header("accept-language").unwrap_or("-")
                ));
                Response::text("p2")
            }
        })
        .unwrap()
        .get("/p3", {
            let rec = rec.clone();
            move |req, _| {
                rec.lock().unwrap().push(format!(
                    "{}|{}",
                    req.header("user-agent").unwrap_or("-"),
                    req.header("accept-language").unwrap_or("-")
                ));
                Response::text("p3")
            }
        })
        .unwrap();
    let (addr, stop) = serve_at(addr, router);

    Crawler::new()
        .seed(&format!("http://{addr}/"))
        .max_depth(1)
        .workers(1) // 单 worker：轮转顺序才是确定的
        .middleware(Arc::new(RotateUserAgent::new(["claw-a", "claw-b"])))
        .middleware(Arc::new(StaticHeaders::new([(
            "Accept-Language",
            "zh-CN",
        )])))
        .run(follow_internal);
    stop.store(true, Ordering::Relaxed);

    let got: Vec<String> = seen.lock().unwrap().clone();
    assert_eq!(got.len(), 3, "{got:?}");
    assert!(
        got.iter().all(|l| l.ends_with("|zh-CN")),
        "StaticHeaders 应当出现在每个请求里：{got:?}"
    );
    let uas: Vec<&str> = got.iter().map(|l| l.split('|').next().unwrap()).collect();
    assert!(
        uas.contains(&"claw-a") && uas.contains(&"claw-b"),
        "UA 应当在轮转：{uas:?}"
    );
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn serve_at(addr: String, router: Router) -> (String, Arc<AtomicBool>) {
    let stop = Arc::new(AtomicBool::new(false));
    let (s2, a2) = (Arc::clone(&stop), addr.clone());
    std::thread::spawn(move || {
        let mut app = App::builder()
            .config(
                Config::new()
                    .set("http.envelope", "false")
                    .set("http.log.requests", "false")
                    .set("http.docs", "false")
                    .set("http.security.headers", "false")
                    .set("http.max_requests", "10000"),
            )
            .build()
            .unwrap();
        app.serve_with(&a2, router, s2).unwrap();
    });
    // 等它真的能连上
    for _ in 0..60 {
        if std::net::TcpStream::connect(&addr).is_ok() {
            return (addr, stop);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("rsboot 站点没起来：{addr}");
}

/// 一棵小站：首页三个链接（其中一个是重复的）、`/a` 链到 `/c`、
/// `/b` 绕回首页（测回环去重）、`/private/x` 被 robots 禁止、
/// 还有一个站外链接和一个 `mailto:`（都该被丢掉）。
fn site() -> (String, Arc<AtomicBool>) {
    site_with_sitemap(false)
}

/// `with_sitemap` 时额外提供 `/sitemap.xml`，里面有 `/s1` `/s2` 两个
/// **没有任何页面链到**的地址——正好验证"sitemap 能不能当种子用"。
fn site_with_sitemap(with_sitemap: bool) -> (String, Arc<AtomicBool>) {
    let addr = format!("127.0.0.1:{}", free_port());
    let a = addr.clone();
    let mut router = Router::new()
        .get("/", |_, _| {
            Response::html(
                r##"<html><head><title>首页</title>
                <meta name="description" content="测试站">
                <base href="/v2/"></head><body>
                <a href="/a">A</a><a href="/a">A again</a><a href="/b">B</a>
                <a href="/private/x">secret</a>
                <a href="http://example.invalid/out">out</a>
                <a href="mailto:x@y.z">mail</a>
                <a href="#top">anchor</a>
                </body></html>"##,
            )
        })
        .unwrap()
        // rsboot 的路由不允许尾斜杠，所以是 `/v2` 而不是 `/v2/`
        .get("/v2", |_, _| {
            Response::html(r#"<html><head><title>V2</title></head><body>无链接</body></html>"#)
        })
        .unwrap()
        .get("/a", |_, _| {
            Response::html(r#"<html><head><title>A 页</title></head><body><a href="/c">到 C</a></body></html>"#)
        })
        .unwrap()
        .get("/b", |_, _| {
            Response::html(r#"<html><head><title>B 页</title></head><body><a href="/">回首页</a></body></html>"#)
        })
        .unwrap()
        .get("/c", |_, _| Response::text("纯文本，没有链接"))
        .unwrap()
        .get("/private/x", |_, _| Response::text("不该被爬到"))
        .unwrap()
        .get("/robots.txt", |_, _| {
            Response::text("User-agent: *\nDisallow: /private\n")
        })
        .unwrap()
        .get("/redirect", |_, _| {
            Response::new(Status::FOUND).with_header("location", "/c")
        })
        .unwrap();
    if with_sitemap {
        router = router
            .get("/sitemap.xml", move |_, _| {
                Response::text(format!(
                    r#"<urlset><url><loc>http://{a}/s1</loc></url><url><loc>http://{a}/s2</loc></url></urlset>"#
                ))
            })
            .unwrap()
            .get("/s1", |_, _| Response::text("s1"))
            .unwrap()
            .get("/s2", |_, _| Response::text("s2"))
            .unwrap();
    }
    serve_at(addr, router)
}

#[test]
fn crawls_a_real_rsboot_site() {
    let (addr, stop) = site();
    let stats = Crawler::new()
        .seed(&format!("http://{addr}/"))
        .max_depth(3)
        .workers(3)
        .obey_robots(true)
        .run(follow_internal);
    stop.store(true, Ordering::Relaxed);

    assert_eq!(stats.errors, 0, "不该有网络错误：{:?}", stats.error_kinds);
    assert_eq!(stats.skipped_robots, 1, "robots 应该挡住 /private/x");
    // /  /a  /b  /c；/v2 没有任何页链到它，所以不该被爬
    assert_eq!(stats.pages, 4, "{stats:?}");
    assert!(stats.duplicates >= 2, "重复的 /a 和绕回首页的 / 都该被判重：{stats:?}");
    assert!(stats.bytes > 0);
    assert!(stats.pages_per_sec > 0.0);
}

#[test]
fn collects_metadata_and_text() {
    let (addr, stop) = site();
    let titles = std::sync::Mutex::new(Vec::new());
    let text = std::sync::Mutex::new(Vec::new());
    Crawler::new()
        .seed(&format!("http://{addr}/"))
        .max_depth(2) // /c 是第二跳，必须给到 2 层
        .workers(2)
        .run(|page| {
            titles.lock().unwrap().push((page.url.path.clone(), page.title.clone()));
            text.lock().unwrap().push((page.url.path.clone(), page.text().to_string()));
            follow_internal(page)
        });
    stop.store(true, Ordering::Relaxed);

    let t = titles.into_inner().unwrap();
    let text = text.into_inner().unwrap();
    assert!(t.iter().any(|(p, ti)| p == "/" && ti == "首页"), "{t:?}");
    let home = t.iter().find(|(p, _)| p == "/").expect("/ 首页应当被爬到");
    assert_eq!(home.1, "首页");
    let c = text.iter().find(|(p, _)| p == "/c").expect("/c 应当被爬到");
    assert_eq!(c.1, "纯文本，没有链接", "非 HTML 也要能拿到文本");
}

#[test]
fn follows_redirect_to_final_url() {
    let (addr, stop) = site();
    // 直接爬 /redirect：它 302 到 /c，最终 URL 应当是 /c
    let seen = std::sync::Mutex::new(Vec::new());
    Crawler::new()
        .seed(&format!("http://{addr}/redirect"))
        .max_depth(0)
        .workers(1)
        .run(|page| {
            seen.lock().unwrap().push((page.status, page.url.path.clone()));
            Vec::new()
        });
    stop.store(true, Ordering::Relaxed);
    let s = seen.into_inner().unwrap();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].0, 200, "重定向应被跟随到 200");
    assert_eq!(s[0].1, "/c", "最终 URL 应当是重定向目标");
}

#[test]
fn max_pages_stops_early() {
    let (addr, stop) = site();
    let stats = Crawler::new()
        .seed(&format!("http://{addr}/"))
        .max_depth(10)
        .workers(4)
        .max_pages(2)
        .run(follow_internal);
    stop.store(true, Ordering::Relaxed);
    assert_eq!(stats.pages, 2, "{stats:?}");
}

#[test]
fn sitemap_is_used_as_seeds() {
    let (addr, stop) = site_with_sitemap(true);
    // 首页一个链接都没有指向 /s1 /s2，所以这俩必须来自 sitemap
    let stats = Crawler::new()
        .seed(&format!("http://{addr}/"))
        .max_depth(0) // 只抓种子：/ + sitemap 里的两条
        .workers(2)
        .sitemap(true)
        .run(|_| Vec::new());
    stop.store(true, Ordering::Relaxed);
    assert_eq!(stats.pages, 3, "{stats:?}");
    assert_eq!(stats.errors, 0, "{:?}", stats.error_kinds);
}

#[test]
fn sink_gets_pages_and_errors() {
    let (addr, stop) = site();
    let dir = std::env::temp_dir().join(format!("claw-e2e-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("pages.jsonl");
    let sink = std::sync::Arc::new(rsboot_claw::JsonlSink::create(&path).unwrap());
    let stats = Crawler::new()
        .seed(&format!("http://{addr}/"))
        .max_depth(2)
        .workers(2)
        .sink(sink.clone())
        .run(follow_internal);
    stop.store(true, Ordering::Relaxed);
    assert_eq!(sink.rows(), stats.pages, "每页一行，不该少");
    let content = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), stats.pages);
    assert!(lines[0].starts_with("{\"url\":"), "{:?}", lines[0]);
    assert!(
        lines.iter().any(|l| l.contains("\"title\":\"首页\"")),
        "标题要落进 jsonl"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn checkpoint_resumes_after_interruption() {
    let (addr, stop) = site();
    let dir = std::env::temp_dir().join(format!("claw-wal-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let wal = dir.join("crawl.wal");
    let _ = std::fs::remove_file(&wal);
    let seed = format!("http://{addr}/");

    // 第一次只抓 2 页就"停机"
    let first = Crawler::new()
        .seed(&seed)
        .max_depth(3)
        .workers(2)
        .max_pages(2)
        .checkpoint(&wal)
        .run(follow_internal);
    assert_eq!(first.pages, 2, "{first:?}");

    // 第二次用同一个 WAL：上一轮已入队没抓完的必须回来
    let second = Crawler::new()
        .seed(&seed)
        .max_depth(3)
        .workers(2)
        .checkpoint(&wal)
        .run(follow_internal);
    assert!(second.resumed > 0, "应当恢复出待抓的条目：{second:?}");
    assert!(second.pages >= 1, "{second:?}");

    // 第三次：全抓过了，一页都不该再抓
    let third = Crawler::new()
        .seed(&seed)
        .max_depth(3)
        .workers(2)
        .checkpoint(&wal)
        .run(follow_internal);
    assert_eq!(third.pages, 0, "断点日志应当把见过的都挡住：{third:?}");
    assert_eq!(third.errors, 0);

    let wal_text = std::fs::read_to_string(&wal).unwrap();
    assert!(wal_text.lines().any(|l| l.starts_with("+ ")), "WAL 里要能直接看到进度");
    let _ = std::fs::remove_dir_all(&dir);
    stop.store(true, Ordering::Relaxed);
}

/// 服务端自己数并发峰值：`max_per_host(2)` 之下，同一个站不该同时被开 2 发以上。
///
/// 这里**不用 rsboot 起站点**：rsboot 的服务端是一个 event loop 上跑 handler，
/// 在 handler 里 sleep 会把整个服务端串起来，并发永远是 1 —— 那样的测试
/// 无论 crawler 有没有限并发都会通过，等于没测。所以用一个每连接一条线程的
/// 极简服务端，才能真正看到"同时打到几发"。
#[test]
fn per_host_concurrency_is_capped() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let cur = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    {
        let (cur, peak, st) = (cur.clone(), peak.clone(), stop.clone());
        std::thread::spawn(move || {
            for s in l.incoming() {
                if st.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(s) = s else { break };
                let (c, p) = (cur.clone(), peak.clone());
                std::thread::spawn(move || threaded_serve(s, c, p));
            }
        });
    }

    let stats = Crawler::new()
        .seed(&format!("http://{addr}/"))
        .max_depth(1)
        .workers(8)
        .max_per_host(2)
        .run(follow_internal);
    stop.store(true, Ordering::Relaxed);

    assert_eq!(stats.errors, 0, "{:?}", stats.error_kinds);
    assert_eq!(stats.pages, 9, "并发上限不能把队列堵死：{stats:?}");
    assert!(
        peak.load(Ordering::SeqCst) <= 2,
        "同一个站同时最多 2 发，实测 {}",
        peak.load(Ordering::SeqCst)
    );
}

/// 每连接一条线程的最小 HTTP/1.1 服务端：根页给 8 个链接，别的路径回 `ok`。
fn threaded_serve(mut s: std::net::TcpStream, cur: Arc<AtomicUsize>, peak: Arc<AtomicUsize>) {
    use std::io::{BufRead, BufReader, Write};
    let mut rd = BufReader::new(s.try_clone().unwrap());
    loop {
        let mut line = String::new();
        if rd.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        loop {
            let mut h = String::new();
            if rd.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                break;
            }
        }
        let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
        let n = cur.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(n, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(20));
        cur.fetch_sub(1, Ordering::SeqCst);

        let body = if path == "/" {
            let links: String = (0..8)
                .map(|i| format!("<a href=\"/p/{i}\">p{i}</a>"))
                .collect();
            format!("<html><body>{links}</body></html>")
        } else {
            "ok".to_string()
        };
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
            body.len(),
            body
        );
        if s.write_all(resp.as_bytes()).is_err() {
            return;
        }
    }
}

/// 正文一样的两页只交付一次，但两边各自的链接都要继续走。
#[test]
fn duplicate_content_is_skipped_but_links_still_followed() {
    let addr = format!("127.0.0.1:{}", free_port());
    let router = Router::new()
        .get("/", |_, _| {
            Response::html(
                r#"<html><body>
                <a href="/same/1">a</a><a href="/same/2">b</a><a href="/other">c</a>
                </body></html>"#,
            )
        })
        .unwrap()
        .get("/same/{n}", |req, _| {
            // 两个地址、同一段正文；链接不同（这才是镜像站的样子）
            let n = req.param("n").unwrap_or("0").to_string();
            Response::html(format!(
                r#"<html><body>同一段正文<a href="/deep/{n}">next</a></body></html>"#
            ))
        })
        .unwrap()
        .get("/other", |_, _| Response::html("<html><body>别的正文</body></html>"))
        .unwrap()
        .get("/deep/{n}", |req, _| {
            let n = req.param("n").unwrap_or("0").to_string();
            Response::html(format!("<html><body>叶子{n}</body></html>"))
        })
        .unwrap();
    let (addr, stop) = serve_at(addr, router);

    let seen = Arc::new(AtomicUsize::new(0));
    let cnt = seen.clone();
    let stats = Crawler::new()
        .seed(&format!("http://{addr}/"))
        .max_depth(2)
        .workers(4)
        .avoid_duplicate_content(true)
        .run(move |page| {
            cnt.fetch_add(1, Ordering::Relaxed);
            follow_internal(page)
        });
    stop.store(true, Ordering::Relaxed);

    assert_eq!(stats.errors, 0, "{:?}", stats.error_kinds);
    assert_eq!(stats.duplicates_content, 1, "{stats:?}");
    // 首页 + 两个 same 里的一个 + /other + 两个 deep
    assert_eq!(stats.pages, 5, "{stats:?}");
    assert_eq!(seen.load(Ordering::Relaxed), 5, "handler 只见 5 次");
    // 两个 deep 都该被抓到：内容重复那页的链接不能丢
    assert!(stats.bytes > 0);
}

/// 优先级决定**站内**的抓取顺序：列表页先于详情页，和链接在页面里出现的先后无关。
#[test]
fn priority_fn_reorders_the_queue() {
    let addr = format!("127.0.0.1:{}", free_port());
    let router = Router::new()
        .get("/", |_, _| {
            // 故意先写详情、再写列表：优先级要能把它翻过来
            Response::html(
                r#"<html><body>
                <a href="/detail/1">d1</a><a href="/list/1">l1</a>
                <a href="/detail/2">d2</a><a href="/list/2">l2</a>
                </body></html>"#,
            )
        })
        .unwrap()
        .get("/list/{n}", |req, _| {
            let n = req.param("n").unwrap_or("0").to_string();
            Response::html(format!("<html><body>列表{n}</body></html>"))
        })
        .unwrap()
        .get("/detail/{n}", |req, _| {
            let n = req.param("n").unwrap_or("0").to_string();
            Response::html(format!("<html><body>详情{n}</body></html>"))
        })
        .unwrap();
    let (addr, stop) = serve_at(addr, router);

    let sink = Arc::new(CollectSink::new());
    let stats = Crawler::new()
        .seed(&format!("http://{addr}/"))
        .max_depth(1)
        .workers(1) // 单 worker 才能断言顺序；多 worker 下顺序本来就不确定
        .priority_fn(|u| if u.path.starts_with("/list") { 10 } else { 0 })
        .sink(sink.clone())
        .run(follow_internal);
    stop.store(true, Ordering::Relaxed);

    assert_eq!(stats.errors, 0, "{:?}", stats.error_kinds);
    let order: Vec<String> = sink.pages().iter().map(|p| p.url.path.clone()).collect();
    assert_eq!(
        order,
        ["/", "/list/1", "/list/2", "/detail/1", "/detail/2"],
        "{stats:?}"
    );
}

/// 被规则拒掉的链接**不发请求**：服务端那几路一次都不该被打到。
#[test]
fn deny_rules_stop_links_before_they_are_fetched() {
    let addr = format!("127.0.0.1:{}", free_port());
    let hits: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let router = Router::new()
        .get("/", |_, _| {
            Response::html(
                r#"<html><body>
                <a href="/keep/1">k</a><a href="/files/a.pdf">p</a>
                <a href="/files/b.zip">z</a><a href="/drop">d</a>
                </body></html>"#,
            )
        })
        .unwrap()
        .get("/keep/{n}", |req, _| {
            let n = req.param("n").unwrap_or("0").to_string();
            Response::html(format!("<html><body>keep{n}</body></html>"))
        })
        .unwrap()
        .get("/drop", |_, _| Response::html("<html><body>drop</body></html>"))
        .unwrap()
        .get("/files/{name}", move |_, _| {
            h.fetch_add(1, Ordering::SeqCst);
            Response::text("binary-ish")
        })
        .unwrap();
    let (addr, stop) = serve_at(addr, router);

    let stats = Crawler::new()
        .seed(&format!("http://{addr}/"))
        .max_depth(1)
        .workers(2)
        .deny("*.pdf")
        .deny("*.zip")
        .run(follow_internal);
    stop.store(true, Ordering::Relaxed);

    assert_eq!(stats.errors, 0, "{:?}", stats.error_kinds);
    assert_eq!(stats.pages, 3, "只该抓首页 + keep + drop：{stats:?}");
    assert_eq!(stats.skipped_filter, 2, "{stats:?}");
    assert_eq!(hits.load(Ordering::SeqCst), 0, "被拒的链接不该发请求");
}

/// 会话 cookie：先 `/login` 拿到 `Set-Cookie`，后面的 `/private` 才放行。
/// 关掉 jar 时必须 403——否则这个测试只是在证明"服务端没在看 cookie"。
#[test]
fn session_cookie_is_carried_between_requests() {
    let addr = format!("127.0.0.1:{}", free_port());
    let authed: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    let a = authed.clone();
    let router = Router::new()
        .get("/", |_, _| Response::html(r#"<html><body><a href="/login">go</a></body></html>"#))
        .unwrap()
        .get("/login", |_, _| {
            Response::html(r#"<html><body><a href="/private">p</a></body></html>"#)
                .with_header("Set-Cookie", "sid=abc123; Path=/; HttpOnly")
        })
        .unwrap()
        .get("/private", move |req, _| {
            let ok = req
                .header("cookie")
                .map(|c| c.contains("sid=abc123"))
                .unwrap_or(false);
            if ok {
                a.fetch_add(1, Ordering::SeqCst);
                Response::html("<html><body>secret</body></html>")
            } else {
                Response::new(Status::FORBIDDEN)
            }
        })
        .unwrap();
    let (addr, stop) = serve_at(addr, router);

    let sink = Arc::new(CollectSink::new());
    let stats = Crawler::new()
        .seed(&format!("http://{addr}/"))
        .max_depth(2)
        .workers(1) // 单 worker 才有确定的"先 login 后 private"
        .sink(sink.clone())
        .run(follow_internal);

    assert_eq!(stats.errors, 0, "{:?}", stats.error_kinds);
    assert_eq!(stats.pages, 3, "{stats:?}");
    assert_eq!(
        authed.load(Ordering::SeqCst),
        1,
        "private 必须带着 sid 才放行（403 也会被当成抓到的页，所以还要数命中）"
    );
    let pages = sink.pages();
    let private = pages
        .iter()
        .find(|p| p.url.path == "/private")
        .expect("private 应该被抓到");
    assert_eq!(private.status, 200, "带上 cookie 之后应当是 200");
    stop.store(true, Ordering::Relaxed);
}

#[test]
fn unreachable_host_is_an_error_not_a_hang() {
    let port = free_port();
    let stats = Crawler::new()
        .seed(&format!("http://127.0.0.1:{port}/"))
        .max_depth(1)
        .workers(1)
        .timeout(Duration::from_millis(300))
        .run(follow_internal);
    assert_eq!(stats.pages, 0);
    assert_eq!(stats.errors, 1, "{:?}", stats.error_kinds);
}
