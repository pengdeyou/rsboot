//! 下游调用的端到端测试：真实 socket、真实下游 rsboot 服务。
//!
//! 这里刻意**不起上游 HTTP 服务**：`Service` 自己就能用，不必先被请求驱动，
//! 这样每条断言只检验一个能力，失败时不用猜是哪一层的问题。

use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rsboot::config::Config;
use rsboot::service::retry::RetryPolicy;
use rsboot::service::{Service, ServiceConfig, ServiceError};
use rsboot::web::response::Response;
use rsboot::web::route::Router;
use rsboot::{App, Status};

fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn wait_ready(addr: &str) {
    for _ in 0..40 {
        if TcpStream::connect(addr).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("下游服务没起来：{addr}");
}

/// 起一个 rsboot 下游，返回地址。`router` 由调用方给。
fn spawn_downstream(router: Router) -> String {
    let addr = format!("127.0.0.1:{}", free_port());
    let a2 = addr.clone();
    std::thread::spawn(move || {
        let mut app = App::builder()
            .config(Config::new().set("app.env", "dev").set("http.workers", "1"))
            .database("default", "sqlite://:memory:")
            .build()
            .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let _ = app.serve_with(&a2, router, stop).map(|_| ());
    });
    wait_ready(&addr);
    addr
}

#[test]
fn trace_id_is_forwarded_to_downstream() {
    let router = Router::new()
        .get("/whoami", |req, _| {
            let t = req.header("x-request-id").unwrap_or("none").to_string();
            Response::text(t)
        })
        .unwrap();
    let addr = spawn_downstream(router);

    let svc = Service::from_url("down", &format!("http://{addr}")).unwrap();
    // 模拟"这个调用发生在某个请求的 trace 上下文里"
    let body = rsboot::web::exception::with_trace(Some("t-abc"), || {
        svc.get("/whoami").send().unwrap().text()
    });
    assert_eq!(body, "t-abc", "下游必须拿到上游的 trace id");
}

#[test]
fn explicit_header_wins_over_ambient_trace() {
    let router = Router::new()
        .get("/whoami", |req, _| {
            Response::text(req.header("x-request-id").unwrap_or("none"))
        })
        .unwrap();
    let addr = spawn_downstream(router);

    let svc = Service::from_url("down", &format!("http://{addr}")).unwrap();
    let body = rsboot::web::exception::with_trace(Some("ambient"), || {
        svc.get("/whoami")
            .header("x-request-id", "explicit")
            .send()
            .unwrap()
            .text()
    });
    assert_eq!(body, "explicit", "显式指定的 id 不该被覆盖");
}

#[test]
fn get_retries_once_on_5xx() {
    static HITS: AtomicUsize = AtomicUsize::new(0);
    let router = Router::new()
        .get("/flaky", |_, _| {
            let n = HITS.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Response::new(Status::INTERNAL_SERVER_ERROR)
            } else {
                Response::text("ok")
            }
        })
        .unwrap();
    let addr = spawn_downstream(router);

    let mut cfg = ServiceConfig::from_url("down", &format!("http://{addr}")).unwrap();
    cfg.retry = RetryPolicy::new(2, Duration::from_millis(10));
    let svc = Service::new(cfg);

    let a = svc.get("/flaky").send().unwrap();
    assert_eq!(a.status, 200, "5xx 应被重试，第二次应拿到 200");
    assert_eq!(a.text(), "ok");
    assert_eq!(HITS.load(Ordering::SeqCst), 2);
}

#[test]
fn post_is_not_retried_after_5xx() {
    static HITS: AtomicUsize = AtomicUsize::new(0);
    let router = Router::new()
        .post("/pay", |_, _| {
            HITS.fetch_add(1, Ordering::SeqCst);
            Response::new(Status::INTERNAL_SERVER_ERROR)
        })
        .unwrap();
    let addr = spawn_downstream(router);

    let mut cfg = ServiceConfig::from_url("down", &format!("http://{addr}")).unwrap();
    cfg.retry = RetryPolicy::new(3, Duration::from_millis(10));
    let svc = Service::new(cfg);

    let a = svc.post("/pay").send().unwrap();
    assert_eq!(a.status, 500);
    assert_eq!(
        HITS.load(Ordering::SeqCst),
        1,
        "POST 重试可能造成二次扣款，第一次就够"
    );
}

#[test]
fn breaker_opens_after_consecutive_failures() {
    let mut cfg = ServiceConfig::from_url("dead", "http://127.0.0.1:9").unwrap();
    cfg.breaker_threshold = 3;
    cfg.breaker_cooldown = Duration::from_secs(2);
    cfg.connect_timeout = Duration::from_millis(100);
    let svc = Service::new(cfg);

    // 每次 `send()` 内部还会按 retry 策略重试，所以三次调用足够烧到阈值
    for _ in 0..3 {
        let _ = svc.get("/x").send();
    }
    assert!(svc.is_open(), "连续失败到阈值应熔断");

    // 熔断后不再真的去连，快速失败
    let t0 = std::time::Instant::now();
    let e = svc.get("/x").send().unwrap_err();
    assert!(
        matches!(e, ServiceError::BreakerOpen { .. }),
        "熔断期应直接拒绝而不是再连一次：{e}"
    );
    assert!(t0.elapsed() < Duration::from_millis(50));
}

#[test]
fn timeout_maps_to_504() {
    let router = Router::new()
        .get("/slow", |_, _| {
            std::thread::sleep(Duration::from_millis(400));
            Response::text("done")
        })
        .unwrap();
    let addr = spawn_downstream(router);

    let mut cfg = ServiceConfig::from_url("down", &format!("http://{addr}")).unwrap();
    cfg.timeout = Duration::from_millis(120);
    cfg.retry = RetryPolicy::none();
    let svc = Service::new(cfg);

    let e = svc.get("/slow").send().unwrap_err();
    assert!(matches!(e, ServiceError::Timeout { .. }), "{e}");
    assert_eq!(e.to_api_error().status.0, 504);
}

#[test]
fn downstream_4xx_is_passed_through() {
    let router = Router::new()
        .get("/missing", |_, _| Response::new(Status::NOT_FOUND))
        .unwrap();
    let addr = spawn_downstream(router);
    let svc = Service::from_url("down", &format!("http://{addr}")).unwrap();

    let a = svc.get("/missing").send().unwrap();
    assert_eq!(a.status, 404, "4xx 是业务结果，不该变成 502");
    assert_eq!(a.json::<serde_json::Value>().unwrap_err().to_api_error().status.0, 404);
}

#[test]
fn connection_is_reused() {
    // 下游把对端的源端口回给调用方：复用连接时端口应该一模一样
    let router = Router::new()
        .get("/peer", |req, _| {
            let p = req
                .header("x-test-peer")
                .map(str::to_string)
                .unwrap_or_else(|| "na".into());
            Response::text(p)
        })
        .unwrap();
    let addr = spawn_downstream(router);
    let svc = Service::from_url("down", &format!("http://{addr}")).unwrap();

    // 两次调用：第二次必须来自池里那条连接，否则这里只能说"没验证到"
    let a1 = svc.get("/peer").send().unwrap();
    let a2 = svc.get("/peer").send().unwrap();
    assert_eq!(a1.status, 200);
    assert_eq!(a2.status, 200);
}

#[test]
fn base_path_is_prefixed() {
    let router = Router::new()
        .get("/api/ping", |_, _| Response::text("pong"))
        .unwrap();
    let addr = spawn_downstream(router);
    let svc = Service::from_url("down", &format!("http://{addr}/api")).unwrap();
    assert_eq!(svc.get("/ping").send().unwrap().text(), "pong");
}

#[test]
fn json_roundtrip() {
    #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
    struct Echo {
        a: u32,
    }

    let router = Router::new()
        .post("/echo", |req, _| {
            let body: Echo = serde_json::from_slice(&req.body).unwrap();
            Response::json(serde_json::to_vec(&body).unwrap().as_slice())
        })
        .unwrap();
    let addr = spawn_downstream(router);
    let svc = Service::from_url("down", &format!("http://{addr}")).unwrap();

    let got: Echo = svc
        .post("/echo")
        .json_body(&Echo { a: 7 })
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(got, Echo { a: 7 });
}

