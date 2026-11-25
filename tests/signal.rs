//! SIGTERM / SIGINT 的优雅下线，以及模块的 worker 钩子。
//!
//! 这个文件单独存在是为了隔离全局状态：`signal::received()` 是**进程级**标志，
//! 一旦在本文件里置位，同进程内的其他测试服务都会跟着停。Rust 的每个
//! `tests/*.rs` 是独立进程，所以放在这里最安全。

use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rsboot::web::response::Response;
use rsboot::web::route::Router;
use rsboot::{App, Config, Context, Error, Module, Result};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_ready(addr: &str) -> TcpStream {
    for _ in 0..40 {
        if let Ok(s) = TcpStream::connect(addr) {
            return s;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("服务没起来：{addr}");
}

/// 记录四个钩子的调用，并在 `start_worker` 里真查一次库——
/// 用来证明 worker 拿到的 `Context` 是"该 worker 自己的、连接可用"的。
struct Rec {
    log: Arc<Mutex<Vec<String>>>,
}

impl Module for Rec {
    fn name(&self) -> &'static str {
        "rec"
    }

    fn start(&self, _ctx: &Context) -> Result<()> {
        self.log.lock().unwrap().push("start".into());
        Ok(())
    }

    fn start_worker(&self, ctx: &Context) -> Result<()> {
        ctx.with_db(|db| {
            db.fetch(rsbatis::Sql::raw("SELECT 1"))
                .map(|_| ())
                .map_err(Error::Driver)
        })?;
        self.log.lock().unwrap().push("start_worker".into());
        Ok(())
    }

    fn stop_worker(&self, _ctx: &Context) -> Result<()> {
        self.log.lock().unwrap().push("stop_worker".into());
        Ok(())
    }

    fn stop(&self, _ctx: &Context) -> Result<()> {
        self.log.lock().unwrap().push("stop".into());
        Ok(())
    }
}

#[test]
fn sigterm_triggers_graceful_shutdown_with_worker_hooks() {
    rsboot::server::signal::reset();

    let addr = format!("127.0.0.1:{}", free_port());
    let log = Arc::new(Mutex::new(Vec::new()));
    let l2 = Arc::clone(&log);
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let a2 = addr.clone();

    let handle = std::thread::spawn(move || {
        let mut app = App::builder()
            .config(
                Config::new()
                    .set("app.env", "dev")
                    .set("http.workers", "2")
                    .set("http.log.requests", "false"),
            )
            .database("default", "sqlite://:memory:")
            .module(Rec { log: l2 })
            .build()
            .unwrap();
        app.serve_with(
            &a2,
            Router::new().get("/p", |_, _| Response::text("p")).unwrap(),
            shutdown,
        )
        .unwrap();
    });

    // 服务可用
    let mut s = wait_ready(&addr);
    use std::io::{Read, Write};
    s.write_all(b"GET /p HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut buf = Vec::new();
    s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let _ = s.read_to_end(&mut buf);
    assert!(String::from_utf8_lossy(&buf).contains("200 OK"));

    // 模拟收到 SIGTERM（与真信号写同一个标志）
    rsboot::server::signal::trigger_for_test();

    // 服务线程必须自己退出（不靠 shutdown），且 10 秒内完成
    let t0 = std::time::Instant::now();
    handle.join().unwrap();
    assert!(
        t0.elapsed() < Duration::from_secs(10),
        "关闭耗时 {:?}，太慢",
        t0.elapsed()
    );

    let calls = log.lock().unwrap().clone();
    assert_eq!(calls.first().map(String::as_str), Some("start"));
    assert_eq!(
        calls.iter().filter(|c| *c == "start_worker").count(),
        2,
        "两个 worker 各调一次 start_worker：{calls:?}"
    );
    assert_eq!(
        calls.iter().filter(|c| *c == "stop_worker").count(),
        2,
        "两个 worker 各调一次 stop_worker：{calls:?}"
    );
    assert_eq!(calls.last().map(String::as_str), Some("stop"));

    rsboot::server::signal::reset();
}

#[test]
fn module_start_failure_aborts_startup_and_unwinds() {
    struct Boom;
    impl Module for Boom {
        fn name(&self) -> &'static str {
            "boom"
        }
        fn start(&self, _ctx: &Context) -> Result<()> {
            Err(Error::Lifecycle("boom".into()))
        }
        fn stop(&self, _ctx: &Context) -> Result<()> {
            Ok(())
        }
    }

    let mut app = App::builder()
        .config(Config::new().set("app.env", "dev"))
        .database("default", "sqlite://:memory:")
        .module(Boom)
        .build()
        .unwrap();

    let err = app.serve_on("127.0.0.1:0", Router::new()).unwrap_err();
    assert!(err.to_string().contains("boom"), "{err}");
}
