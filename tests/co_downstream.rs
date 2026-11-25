//! 下游调用让出 worker 的端到端验证。
//!
//! 之前 `Service::send` 用同步 `TcpStream` 读响应：一次 20ms 的下游就把
//! worker 焊死 20ms，于是 `QPS` 恒等于 `worker 数 / 下游延迟`，并发数再高
//! 也没用。这条测试直接量这件事：40 个并发请求打一个 20ms 的下游，
//! 2 个 worker。同步模型要 400ms（40×20/2），让出之后只该是一次延迟量级。

// `ApiError` 约 144 字节（已知技术债，等 OpenAPI schema Box 化时一起改），
// 不为了压警告改 handler 签名。
#![allow(clippy::result_large_err)]

use rsboot::server::conn::Limits;
use rsboot::web::response::Response;
use rsboot::web::route::Router;
use rsboot::{
    ApiError, Config, Context, Environment, Server, Service, ServiceRegistry,
};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

/// 起一个慢下游：每个请求睡 `delay` 再回 200。
///
/// 用裸 socket 手写 HTTP/1.1，不引任何异步运行库——被测的是 rsboot 客户端，
/// 下游是不是 tokio 无关。
fn spawn_slow_downstream(delay: Duration) -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in l.incoming() {
            let Ok(s) = stream else { continue };
            std::thread::spawn(move || {
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut w = s;
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    if line.trim().is_empty() {
                        return;
                    }
                    // 读到空行为止（请求体这段测试用不上）
                    loop {
                        let mut h = String::new();
                        if r.read_line(&mut h).unwrap_or(0) == 0 {
                            return;
                        }
                        if h.trim().is_empty() {
                            break;
                        }
                    }
                    std::thread::sleep(delay);
                    let body = b"{\"ok\":true}";
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n",
                        body.len()
                    );
                    if w.write_all(head.as_bytes()).is_err() || w.write_all(body).is_err() {
                        return;
                    }
                }
            });
        }
    });
    addr
}

fn limits() -> Limits {
    Limits {
        max_requests: 1_000_000,
        ..Limits::default()
    }
}

fn start(
    router: Router,
    host: String,
    workers: usize,
) -> (SocketAddr, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let factory = move || -> rsboot::Result<Context> {
        let ctx = Context::new(Config::new(), vec![], None, Environment::Dev);
        let mut reg = ServiceRegistry::new();
        reg.insert(Service::from_url("down", &format!("http://{host}"))?);
        ctx.insert_state(reg);
        Ok(ctx)
    };
    let server = Server::bind(
        "127.0.0.1:0",
        Arc::new(router),
        Arc::new(factory),
        limits(),
        workers,
    )
    .unwrap();
    let addr = server.local_addr();
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = Arc::clone(&stop);
    let h = std::thread::spawn(move || {
        server.run(s2).unwrap();
    });
    (addr, stop, h)
}

fn roundtrip(addr: SocketAddr, path: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n").as_bytes())
        .unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    String::from_utf8(buf).unwrap()
}

fn burst(addr: SocketAddr, path: &'static str, n: usize) -> Duration {
    let barrier = Arc::new(Barrier::new(n));
    let t0 = Instant::now();
    let hs: Vec<_> = (0..n)
        .map(|_| {
            let b = Arc::clone(&barrier);
            std::thread::spawn(move || {
                b.wait();
                roundtrip(addr, path)
            })
        })
        .collect();
    for h in hs {
        let resp = h.join().unwrap();
        assert!(resp.contains("200 OK"), "响应异常：{resp}");
        assert!(resp.contains("{\"ok\":true}"), "下游响应体丢了：{resp}");
    }
    t0.elapsed()
}

/// 对照组：**串行**发 20 次，在没有调度器的普通线程里走——`mn::net` 必须
/// 退化成原来的阻塞语义。20 × 20ms 就是 400ms 量级；如果它意外变快，
/// 说明读被跳过了（那比慢危险得多）。
///
/// 注意别写成"40 个客户端线程并发"：那种写法的并行度在下游侧，总耗时
/// 本来就只有一次延迟，测不出任何东西。
#[test]
fn same_load_without_scheduler_stays_serial() {
    let down = spawn_slow_downstream(Duration::from_millis(20));
    let svc = Service::from_url("down", &format!("http://{down}")).unwrap();

    let t0 = Instant::now();
    for _ in 0..20 {
        assert!(svc.get("/x").send().unwrap().text().contains("ok"));
    }
    let elapsed = t0.elapsed();
    assert!(
        elapsed > Duration::from_millis(300),
        "无调度器时本该串行复现 400ms 量级，实际 {elapsed:?}"
    );
}

#[test]
fn downstream_call_yields_the_worker() {
    let down = spawn_slow_downstream(Duration::from_millis(20));
    let router = Router::new()
        .get("/down", |_, ctx: &Context| -> Result<Response, ApiError> {
            let svc = ctx
                .service("down")
                .ok_or_else(|| ApiError::internal("没注册 down"))?;
            // 这里把错误就地转成 502，别让大的 Err 变体从闭包里冒出去
            let ans = svc.get("/x").send().map_err(|e| {
                ApiError::new(rsboot::Status::BAD_GATEWAY, format!("下游失败：{e}"))
            })?;
            Ok(Response::text(ans.text()))
        })
        .unwrap();
    let (addr, stop, h) = start(router, down.to_string(), 2);

    let elapsed = burst(addr, "/down", 40);
    stop.store(true, Ordering::Relaxed);
    h.join().unwrap();

    assert!(
        elapsed < Duration::from_millis(200),
        "下游调用没有让出 worker：40 个 20ms 下游请求花了 {elapsed:?}（同步模型约 400ms）"
    );
}
