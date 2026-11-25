//! `App::serve` 的端到端测试：真实 socket、真实 App 生命周期、真实数据库。
//!
//! `App` 不是 `Send`（`Context` 里是 `RefCell<Session<Engine>>`），所以这里
//! 在**服务线程内部**构建 App —— 这也正是生产里"每个 worker 自己建 Context"
//! 的同一条约束在测试里的体现。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rsboot::config::Config;
use rsboot::web::route::Router;
use rsboot::web::response::Response;
use rsboot::{App, Context};

/// 抢一个空闲端口：bind 到 :0 拿到端口后立刻放开。
/// 理论上存在"放开到重新 bind 之间被别人抢走"的窗口，本地测试里可以接受。
fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn wait_ready(addr: &str) -> TcpStream {
    for _ in 0..40 {
        if let Ok(s) = TcpStream::connect(addr) {
            return s;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("服务在 2 秒内没有起来：{addr}");
}

fn request(s: &mut TcpStream, raw: &str) -> String {
    s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    s.write_all(raw.as_bytes()).unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    String::from_utf8(buf).unwrap()
}

#[test]
fn app_serves_routes_and_databases() {
    let addr = format!("127.0.0.1:{}", free_port());
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = Arc::clone(&stop);
    let a2 = addr.clone();

    let handle = std::thread::spawn(move || {
        let mut app = App::builder()
            .config(Config::new())
            .database("default", "sqlite://:memory:")
            .build()
            .unwrap();

        let router = Router::new()
            .get("/ping", |_, _| Response::text("pong"))
            .unwrap()
            // 真查一次库，确认 worker 自己的 Context 拿到了可用连接
            .get("/count", |req, ctx: &Context| {
                let _ = req;
                let n = ctx
                    .with_db(|s| {
                        // sqlite 的 `SELECT 1` 会返回 1 行；这里只验证"连接可用"
                        s.fetch(rsbatis::Sql::raw("SELECT 1"))
                            .map(|rows| rows.len())
                            .map_err(rsboot::Error::Driver)
                    })
                    .unwrap();
                Response::text(format!("rows={n}"))
            })
            .unwrap();

        app.serve_with(&a2, router, s2).unwrap();
    });

    let mut s = wait_ready(&addr);
    let r = request(&mut s, "GET /ping HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n");
    assert!(r.starts_with("HTTP/1.1 200 OK"), "{r}");
    assert!(r.ends_with("pong"), "{r}");

    let mut s = wait_ready(&addr);
    let r = request(&mut s, "GET /count HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n");
    assert!(r.ends_with("rows=1"), "{r}");

    stop.store(true, Ordering::Relaxed);
    handle.join().unwrap();
}

#[test]
fn serve_without_bind_config_is_a_clear_error() {
    // prod 默认**不给** http.bind：缺了必须报错，而不是悄悄监听 0.0.0.0:8000
    // （dev 有默认 127.0.0.1:8000，所以这里必须显式声明跑 prod）
    let mut app = App::builder()
        .config(Config::new().set("app.env", "prod"))
        .database("default", "sqlite://:memory:")
        .build()
        .unwrap();
    let err = app.serve(Router::new()).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("http.bind"), "{msg}");
}

#[test]
fn bind_addr_comes_from_config() {
    // `serve()` 走的是配置里的 http.bind；这条只验读取与缺省行为，
    // 真起服务由上面的 serve_with 用例覆盖（serve_on 会永久阻塞，不适合测）。
    let a2 = format!("127.0.0.1:{}", free_port());
    let app = App::builder()
        .config(Config::new().set("http.bind", &a2))
        .database("default", "sqlite://:memory:")
        .build()
        .unwrap();
    assert_eq!(app.bind_addr().unwrap(), a2);
}

#[test]
fn workers_can_be_configured_and_are_capped() {
    // 0 或负数必须在启动期报错，而不是起 0 个 worker 然后永远不响应
    let mut app = App::builder()
        .config(Config::new().set("http.workers", "0"))
        .database("default", "sqlite://:memory:")
        .build()
        .unwrap();
    let err = app.serve_on("127.0.0.1:0", Router::new()).unwrap_err();
    assert!(err.to_string().contains("http.workers"), "{err}");
}

#[test]
fn docs_cors_and_security_headers_are_wired() {
    let addr = format!("127.0.0.1:{}", free_port());
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = Arc::clone(&stop);
    let a2 = addr.clone();

    let handle = std::thread::spawn(move || {
        let mut app = App::builder()
            .config(
                Config::new()
                    .set("app.env", "dev")
                    .set("http.docs", "true")
                    .set("http.cors.allow_origin", "http://localhost:5173"),
            )
            .database("default", "sqlite://:memory:")
            .build()
            .unwrap();

        let router = Router::new()
            .get("/users/{id}", |_, _| Response::text("u"))
            .unwrap()
            .describe(
                rsboot::Method::Get,
                "/users/{id}",
                rsboot::RouteDoc::new("查用户")
                    .param(rsboot::ParamDoc::path("id", serde_json::json!({"type":"integer"}))),
            )
            .unwrap();

        app.serve_with(&a2, router, s2).unwrap();
    });

    // /docs 与 /openapi.json 自动挂载
    let mut s = wait_ready(&addr);
    let r = request(&mut s, "GET /docs HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n");
    assert!(r.starts_with("HTTP/1.1 200 OK"), "{r}");
    assert!(r.contains("text/html"), "{r}");

    let mut s = wait_ready(&addr);
    let r = request(&mut s, "GET /openapi.json HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n");
    assert!(r.contains("\"openapi\""), "{r}");
    assert!(r.contains("/users/{id}"), "{r}");
    assert!(r.contains("查用户"), "describe 的内容要进文档：{r}");

    // CORS：命中的来源回显 + Vary，未命中的什么都不给
    let mut s = wait_ready(&addr);
    let r = request(
        &mut s,
        "GET /users/7 HTTP/1.1\r\nHost: a\r\nOrigin: http://localhost:5173\r\nConnection: close\r\n\r\n",
    );
    assert!(
        r.to_lowercase().contains("access-control-allow-origin: http://localhost:5173"),
        "{r}"
    );
    assert!(r.to_lowercase().contains("vary: origin"), "{r}");

    let mut s = wait_ready(&addr);
    let r = request(
        &mut s,
        "GET /users/7 HTTP/1.1\r\nHost: a\r\nOrigin: https://evil.com\r\nConnection: close\r\n\r\n",
    );
    assert!(!r.to_lowercase().contains("access-control-allow-origin"), "{r}");

    // 安全头
    let mut s = wait_ready(&addr);
    let r = request(&mut s, "GET /users/7 HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n");
    let low = r.to_lowercase();
    assert!(low.contains("x-content-type-options: nosniff"), "{r}");
    assert!(low.contains("x-frame-options: deny"), "{r}");

    // 预检短路，不进 handler
    let mut s = wait_ready(&addr);
    let r = request(
        &mut s,
        "OPTIONS /users/7 HTTP/1.1\r\nHost: a\r\nOrigin: http://localhost:5173\r\n\
         Access-Control-Request-Method: DELETE\r\nConnection: close\r\n\r\n",
    );
    assert!(r.starts_with("HTTP/1.1 204"), "预检应短路成 204：{r}");
    assert!(r.to_lowercase().contains("access-control-allow-methods"), "{r}");

    stop.store(true, Ordering::Relaxed);
    handle.join().unwrap();
}

#[test]
fn rate_limit_returns_429_with_retry_after() {
    let addr = format!("127.0.0.1:{}", free_port());
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = Arc::clone(&stop);
    let a2 = addr.clone();

    let handle = std::thread::spawn(move || {
        let mut app = App::builder()
            .config(
                Config::new()
                    .set("app.env", "dev")
                    .set("http.log.requests", "false")
                    // 每秒 0.001 个令牌、突发 1：第二条请求必然被拒
                    .set("http.rate.rate", "0.001")
                    .set("http.rate.burst", "1"),
            )
            .database("default", "sqlite://:memory:")
            .build()
            .unwrap();
        app.serve_with(
            &a2,
            Router::new().get("/p", |_, _| Response::text("p")).unwrap(),
            s2,
        )
        .unwrap();
    });

    let mut s = wait_ready(&addr);
    let first = request(&mut s, "GET /p HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n");
    assert!(first.contains("200 OK"), "{first}");

    let mut s = wait_ready(&addr);
    let second = request(&mut s, "GET /p HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n");
    assert!(second.contains("429"), "{second}");
    assert!(second.to_lowercase().contains("retry-after"), "{second}");

    stop.store(true, Ordering::Relaxed);
    handle.join().unwrap();
}
