//! 协程让出的端到端验证：handler 在 I/O 等待处让出 worker 之后，
//! 并发请求的总耗时应该由"单次延迟"决定，而不是"请求数 × 延迟 / worker 数"。

use rsboot::server::conn::Limits;
use rsboot::web::response::Response;
use rsboot::web::route::Router;
use rsboot::{
    Config, ConnLimits, Context, Environment, Error, Server,
};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

#[allow(dead_code)]
fn ctx() -> rsboot::Result<Context> {
    Ok(Context::new(
        Config::new(),
        vec![(
            "default".to_string(),
            rsbatis::create_engine("sqlite://:memory:").map_err(|e| Error::Missing(e.message))?,
        )],
        Some("default".to_string()),
        Environment::Dev,
    ))
}

fn limits() -> Limits {
    Limits {
        max_requests: 1_000_000,
        ..Limits::default()
    }
}

fn start(
    router: Router,
    workers: usize,
) -> (SocketAddr, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let server = Server::bind(
        "127.0.0.1:0",
        Arc::new(router),
        Arc::new(ctx),
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
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    String::from_utf8(buf).unwrap()
}

/// 并发打 `n` 个请求，返回总耗时。
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
        assert!(resp.contains("200 OK"), "{resp}");
    }
    t0.elapsed()
}

#[test]
fn rsboot_sleep_yields_the_worker() {
    // 2 个 worker，20 个请求，每个 handler 睡 20ms。
    // 同步模型下这是 20 × 20ms / 2 = 200ms；让出之后只该花 ~20~40ms。
    let router = Router::new()
        .get("/slow", |_, _| {
            rsboot::sleep(Duration::from_millis(20));
            Response::text("ok")
        })
        .unwrap();
    let (addr, stop, h) = start(router, 2);

    let elapsed = burst(addr, "/slow", 20);
    stop.store(true, Ordering::Relaxed);
    h.join().unwrap();

    assert!(
        elapsed < Duration::from_millis(120),
        "协程没有让出 worker：20 个 20ms 请求花了 {elapsed:?}（同步模型约为 200ms）"
    );
}

#[test]
fn thread_sleep_still_starves_the_worker() {
    // 对照组：同样的负载，handler 改用 std 的 sleep —— 必须慢一个数量级。
    // 这条测试把"为什么必须用 rsboot::sleep"钉死，不然文档写了也没人信。
    let router = Router::new()
        .get("/slow", |_, _| {
            std::thread::sleep(Duration::from_millis(20));
            Response::text("ok")
        })
        .unwrap();
    let (addr, stop, h) = start(router, 2);

    let elapsed = burst(addr, "/slow", 20);
    stop.store(true, Ordering::Relaxed);
    h.join().unwrap();

    assert!(
        elapsed > Duration::from_millis(150),
        "对照组的耗时不该这么低：{elapsed:?}——如果它快了，说明 worker 数或并发没按预期生效"
    );
}

#[test]
fn handlers_keep_working_after_yield() {
    // 让出之后连接状态必须完好：同一 worker 上交替跑普通请求和让出请求，
    // 普通请求不能被协程饿死。
    let router = Router::new()
        .get("/fast", |_, _| Response::text("fast"))
        .unwrap()
        .get("/slow", |_, _| {
            rsboot::sleep(Duration::from_millis(10));
            Response::text("slow")
        })
        .unwrap();
    let (addr, stop, h) = start(router, 1);

    for i in 0..5 {
        let r1 = roundtrip(addr, "/slow");
        assert!(r1.ends_with("slow"), "第 {i} 轮 /slow 响应异常：{r1:?}");
        let r2 = roundtrip(addr, "/fast");
        assert!(r2.ends_with("fast"), "第 {i} 轮 /fast 响应异常：{r2:?}");
    }

    stop.store(true, Ordering::Relaxed);
    h.join().unwrap();
}

#[test]
fn limits_are_still_honoured() {
    // 协程化不该改动连接限制语义：max_requests 到点仍然主动关。
    let router = Router::new()
        .get("/hi", |_, _| Response::text("hi"))
        .unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        Arc::new(router),
        Arc::new(ctx),
        Limits {
            max_requests: 3,
            ..Limits::default()
        },
        1,
    )
    .unwrap();
    let addr = server.local_addr();
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = Arc::clone(&stop);
    let h = std::thread::spawn(move || {
        server.run(s2).unwrap();
    });
    let _ = ConnLimits::default();

    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let mut seen = 0usize;
    for _ in 0..5 {
        if s.write_all(b"GET /hi HTTP/1.1\r\nHost: a\r\n\r\n").is_err() {
            break;
        }
        let mut buf = Vec::new();
        let mut tmp = [0u8; 256];
        loop {
            match s.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.ends_with(b"hi") {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        if buf.is_empty() {
            break;
        }
        seen += 1;
    }
    assert_eq!(seen, 3, "max_requests=3 之后连接必须被关掉");

    stop.store(true, Ordering::Relaxed);
    h.join().unwrap();
}
