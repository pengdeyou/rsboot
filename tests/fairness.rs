//! 回归：keep-alive 连接之间必须公平轮转，不能互相饿死。
//!
//! 真实踩过的坑：worker 拿到一条连接就 `for` 到底（读请求 → 处理 → 读下一个），
//! keep-alive 客户端会让它**长期占住 worker**。1 个 worker 遇上 2 条长连接，
//! 第二条要等第一条空闲超时（5s）才拿到服务。
//!
//! 基准里的特征值很说明问题：P99 = 47µs，但 max = 5004ms —— 平均值漂亮，
//! 尾部全是饥饿。现在每处理完一个请求就把连接交回队列，由 worker 取下一条。

use std::net::{TcpListener, TcpStream};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rsboot::config::Config;
use rsboot::service::{Service, ServiceConfig};
use rsboot::web::response::Response;
use rsboot::web::route::Router;
use rsboot::App;

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
    panic!("服务没起来：{addr}");
}

fn client(addr: &str) -> Arc<Service> {
    // 客户端超时给足 4s：这个测试要测的是"服务端有没有轮转"，
    // 而不是"客户端多久放弃"
    let mut cfg = ServiceConfig::from_url("c", &format!("http://{addr}")).unwrap();
    cfg.timeout = Duration::from_secs(4);
    cfg.retry = rsboot::service::retry::RetryPolicy::none();
    Service::new(cfg)
}

#[test]
fn two_keep_alive_connections_do_not_starve() {
    let addr = format!("127.0.0.1:{}", free_port());
    let a2 = addr.clone();

    std::thread::spawn(move || {
        // 只给 1 个 worker：饥饿在 worker 数最小时最明显
        let cfg = Config::new()
            .set("app.env", "dev")
            .set("http.workers", "1")
            .set("http.log.requests", "false");
        let mut app = App::builder()
            .config(cfg)
            .database("default", "sqlite://:memory:")
            .build()
            .unwrap();
        let router = Router::new()
            .get("/", |_, _| Response::json(b"{\"ok\":true}"))
            .unwrap();
        let _ = app.serve_with(&a2, router, Arc::new(AtomicBool::new(false)));
    });
    wait_ready(&addr);

    // 两个独立客户端 = 两条并存的长连接（各自的池里各持一条）
    let ca = client(&addr);
    let cb = client(&addr);

    let t0 = Instant::now();
    assert_eq!(ca.get("/").send().unwrap().status, 200, "A 首请求");
    let first = t0.elapsed();
    assert!(
        first < Duration::from_millis(500),
        "第一条连接自己就慢：{first:?}"
    );

    // 此刻 A 的连接空闲但没关（keep-alive）。B 必须立刻能拿到服务。
    let t1 = Instant::now();
    assert_eq!(cb.get("/").send().unwrap().status, 200, "B 首请求");
    let second = t1.elapsed();
    assert!(
        second < Duration::from_millis(300),
        "第二条 keep-alive 连接被饿住了（{second:?}）。\
         旧实现里要等 A 的空闲超时（5s）才轮到它"
    );

    // 交替再打两轮，确认是轮转而不是"先到者独占"
    for i in 0..2 {
        let t = Instant::now();
        ca.get("/").send().unwrap();
        let d = t.elapsed();
        assert!(d < Duration::from_millis(300), "第 {i} 轮 A 被饿住：{d:?}");

        let t = Instant::now();
        cb.get("/").send().unwrap();
        let d = t.elapsed();
        assert!(d < Duration::from_millis(300), "第 {i} 轮 B 被饿住：{d:?}");
    }
}

// 第二个坑（S9 复查时发现）：worker 用 `swap_remove(ready_idx)` 逐个摘连接处理，
// 但 `poll` 返回的是**多个下标**。按升序处理时，`swap_remove(i)` 会把尾部元素搬到
// i，`push` 又会改变长度，于是后续 `ready[j]`（j > i）指向的已经不是原来那条连接：
// 一条被处理两次、另一条就绪的延后一轮。被重复处理的那条此刻多半已无数据，
// `serve_once` 会阻塞到读超时，把整个 worker 卡住。
//
// 所以必须按下标降序处理。这条测试用 3 条同时就绪的连接把它钉住。

#[test]
fn three_connections_ready_at_once_each_get_their_own_response() {
    let addr = format!("127.0.0.1:{}", free_port());
    let a2 = addr.clone();

    std::thread::spawn(move || {
        let cfg = Config::new()
            .set("app.env", "dev")
            .set("http.workers", "1")
            .set("http.log.requests", "false");
        let mut app = App::builder()
            .config(cfg)
            .database("default", "sqlite://:memory:")
            .build()
            .unwrap();
        let router = Router::new()
            .get("/a", |_, _| Response::json(br#"{"v":"a"}"#))
            .unwrap()
            .get("/b", |_, _| Response::json(br#"{"v":"b"}"#))
            .unwrap()
            .get("/c", |_, _| Response::json(br#"{"v":"c"}"#))
            .unwrap();
        let _ = app.serve_with(&a2, router, Arc::new(AtomicBool::new(false)));
    });
    wait_ready(&addr);

    let cl = [client(&addr), client(&addr), client(&addr)];

    // 先把三条连接都"建立"起来（各自发一个请求，让连接进入 keep-alive 空闲态）
    for c in &cl {
        assert_eq!(c.get("/a").send().unwrap().status, 200);
    }

    // 然后几乎同时让三条连接各发一个**不同路径**的请求。
    // 若下标错位，就会出现响应错位（拿到别人的路径结果）或某条被卡到读超时。
    let paths = ["/a", "/b", "/c"];
    let expect = [r#"{"v":"a"}"#, r#"{"v":"b"}"#, r#"{"v":"c"}"#];
    for round in 0..3 {
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut hs = Vec::new();
        for (i, c) in cl.iter().enumerate() {
            let c = Arc::clone(c);
            let b = Arc::clone(&barrier);
            let p = paths[i];
            let want = expect[i];
            hs.push(std::thread::spawn(move || {
                b.wait();
                let t = Instant::now();
                let resp = c.get(p).send().unwrap();
                let d = t.elapsed();
                assert_eq!(resp.status, 200, "round {round} {p}");
                let body = String::from_utf8_lossy(&resp.body).to_string();
                assert!(
                    body.contains(want),
                    "round {round} 连接 {i} 拿到了别人的响应：{body}"
                );
                assert!(d < Duration::from_millis(500), "round {round} {p} 慢了：{d:?}");
            }));
        }
        for h in hs {
            h.join().unwrap();
        }
    }
}
