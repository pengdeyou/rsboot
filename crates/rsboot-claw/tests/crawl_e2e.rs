//! 端到端：用 rsboot 自己起一个被爬的站点，claw 真去爬它。
//!
//! 为什么不 mock HTTP：这一层要验证的是"整条链路咬不咬得上"——
//! robots 缓存、redirect 之后的 URL 归属、相对链接按 `<base>`/当前页解析、
//! 去重在多线程抢队列时还成不成立。这些只有真 socket + 真服务端才验得出来。
//!
//! 顺便也是"扩展 crate 与 rsboot 能一起用"的证明：被爬的站点就是 rsboot 起的。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rsboot::config::Config;
use rsboot::web::http::Status;
use rsboot::web::response::Response;
use rsboot::web::route::Router;
use rsboot::App;

use rsboot_claw::{follow_internal, Crawler};

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn serve(router: Router) -> (String, Arc<AtomicBool>) {
    let addr = format!("127.0.0.1:{}", free_port());
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
    let router = Router::new()
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
    serve(router)
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
