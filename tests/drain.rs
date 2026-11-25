//! 跛鸭期（drain）与关闭期应答的端到端测试。
//!
//! 这里的两个断言对应滚动更新里最容易出事的两个瞬间：
//! 1. 信号刚到、LB 还没摘干净流量 —— 此时必须**还能正常服务**，
//!    立刻停 accept 会把正被派过来的请求打空；
//! 2. drain 期满、开始关闭 —— 老连接上新来的请求必须拿到明确答复
//!    （503 + retry-after + connection: close），而不是被静默断连。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use rsboot::config::Config;
use rsboot::server::ServeState;
use rsboot::web::response::Response;
use rsboot::web::route::Router;
use rsboot::{App, Context, ServeStatus};

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

/// 发一次请求，读到"响应头 + content-length 个 body 字节"。
///
/// 不能 `read_to_end`：keep-alive 连接不会关，会一直等；
/// 也不能只读到 `\\r\\n\\r\\n`——那样 body 还没进来，断言就看不到状态词。
fn request_keepalive(s: &mut TcpStream, raw: &str) -> String {
    s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    if s.write_all(raw.as_bytes()).is_err() {
        // 对端已经关了：对"关闭期应答"的断言来说这就是失败信号，
        // 用可读的标记返回，别让它变成 BrokenPipe panic
        return "<write failed：对端已关闭>".to_string();
    }
    let mut buf = Vec::new();
    let mut tmp = [0u8; 256];

    // 1) 读满响应头
    loop {
        match s.read(&mut tmp) {
            Ok(0) => return String::from_utf8_lossy(&buf).to_string(),
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => return String::from_utf8_lossy(&buf).to_string(),
        }
    }

    // 2) 按 content-length 补读 body
    let head_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap();
    let head = String::from_utf8_lossy(&buf[..head_end]).to_lowercase();
    let want: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    while buf.len() < head_end + want {
        match s.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    }
    String::from_utf8_lossy(&buf).to_string()
}

#[test]
fn drain_keeps_serving_then_closes_with_503() {
    let addr = format!("127.0.0.1:{}", free_port());
    let a2 = addr.clone();
    let stop = Arc::new(AtomicBool::new(false));

    let handle = std::thread::spawn(move || {
        // 只留 1 个 worker 让时序可预测；drain 压到 1 秒，测试别等太久
        let cfg = Config::new()
            .set("http.workers", "1")
            .set("http.drain_secs", "1");

        let mut app = App::builder()
            .config(cfg)
            .database("default", "sqlite://:memory:")
            .build()
            .unwrap();

        let router = Router::new()
            .get("/state", |_, ctx: &Context| {
                // handler 读的是主线程写的那个状态，不是自己猜的
                let guard = ctx.state::<ServeStatus>().expect("Context 应注入 ServeStatus");
                let name = match guard.state() {
                    ServeState::Serving => "serving",
                    ServeState::Draining => "draining",
                    ServeState::Closing => "closing",
                };
                Response::text(name)
            })
            .unwrap();

        app.serve_with(&a2, router, stop)
    });

    // 第一条连接在关闭前后都要复用：关闭期的 503 只在**已建立的连接**上出现
    let mut conn = wait_ready(&addr);
    let first = request_keepalive(
        &mut conn,
        "GET /state HTTP/1.1\r\nHost: x\r\nConnection: keep-alive\r\n\r\n",
    );
    assert!(first.starts_with("HTTP/1.1 200"), "正常期应 200：{first}");
    assert!(first.contains("serving"), "正常期状态：{first}");

    // 模拟 SIGTERM：进入跛鸭期
    rsboot::server::signal::reset();
    rsboot::server::signal::trigger_for_test();
    std::thread::sleep(Duration::from_millis(150));

    // drain 期内（1 秒预算，此时才 150ms）：仍要能正常服务
    let during = request_keepalive(
        &mut conn,
        "GET /state HTTP/1.1\r\nHost: x\r\nConnection: keep-alive\r\n\r\n",
    );
    assert!(
        during.starts_with("HTTP/1.1 200"),
        "跛鸭期必须还能服务（LB 还在摘流量）：{during}"
    );
    assert!(during.contains("draining"), "跛鸭期状态应为 draining：{during}");

    // 等过 drain 预算（1s）+ 余量。关闭期只对**已建立的连接**给 503，
    // 新连接连不上是正常的（accept 已停），所以这里必须复用第一条连接。
    std::thread::sleep(Duration::from_millis(1500));
    let closing = request_keepalive(
        &mut conn,
        "GET /state HTTP/1.1\r\nHost: x\r\nConnection: keep-alive\r\n\r\n",
    );
    assert!(
        closing.starts_with("HTTP/1.1 503"),
        "关闭期老连接上的新请求应回 503，实际：{closing:?}"
    );
    assert!(
        closing.to_lowercase().contains("retry-after"),
        "503 要带 retry-after 让客户端知道能重试：{closing}"
    );
    assert!(
        closing.to_lowercase().contains("connection: close"),
        "503 要明确关连接，否则客户端会在死连接上重试：{closing}"
    );

    // 关闭流程自己会走完（drain 1s + grace），这里只等它返回
    let r = handle.join().unwrap();
    assert!(r.is_ok(), "关闭应正常返回：{:?}", r.err());
}
