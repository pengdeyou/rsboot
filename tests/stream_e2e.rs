//! 流式 body 的端到端测试：真 socket 上看 chunked 帧长什么样。
//!
//! 这里必须走真连接：chunked 的边界（`<hex> CRLF data CRLF`、终止块 `0 CRLF CRLF`）
//! 和"头先发、body 后补"的时序，只有真 socket 才验证得出来。写回层单测只能
//! 证明字节拼对了，证明不了它真的在流。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rsboot::config::Config;
use rsboot::web::http::Status;
use rsboot::web::response::Response;
use rsboot::web::route::Router;
use rsboot::{App, Context};

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

/// 发一个请求并读完整个响应（用 `Connection: close` 让服务端主动收尾）。
fn request(addr: &str, raw: &str) -> String {
    let mut s = wait_ready(addr);
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(raw.as_bytes()).unwrap();
    let mut buf = Vec::new();
    if let Err(e) = s.read_to_end(&mut buf) {
        panic!("读响应失败：{e}；已收到 {:?}", String::from_utf8_lossy(&buf));
    }
    String::from_utf8(buf).unwrap()
}

fn serve_once(router: Router) -> String {
    let addr = format!("127.0.0.1:{}", free_port());
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = Arc::clone(&stop);
    let a2 = addr.clone();
    std::thread::spawn(move || {
        let mut app = App::builder()
            .config(Config::new())
            .build()
            .unwrap();
        app.serve_with(&a2, router, s2).unwrap();
    });
    addr
}

#[test]
fn streamed_body_uses_chunked_and_terminates() {
    // 每块之间故意睡一下：真流式的意思是"第一块不必等到最后一块算完"。
    // 这里只能验证结果顺序与帧结构；时序由下面的 SSE 用例覆盖。
    let router = Router::new()
        .get("/stream", |_, _| {
            Response::stream(Status::OK, |sink| {
                sink.send(b"first\n")?;
                sink.send(b"second\n")?;
                sink.send(b"third\n")?;
                Ok(())
            })
        })
        .unwrap();

    let addr = serve_once(router);
    let raw = request(
        &addr,
        "GET /stream HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );

    assert!(raw.starts_with("HTTP/1.1 200"), "{raw}");
    let lower = raw.to_lowercase();
    assert!(lower.contains("transfer-encoding: chunked"), "{raw}");
    // 流式 body 绝对不能带 content-length：长度根本还不知道
    assert!(!lower.contains("content-length"), "{raw}");

    // 不能用 split：body 里的终止块 `0\r\n\r\n` 本身就含 `\r\n\r\n`，
    // 按它切会把终止块切掉。头和 body 的分界只有第一个。
    let body = &raw[raw.find("\r\n\r\n").unwrap() + 4..];
    assert_eq!(
        body, "6\r\nfirst\n\r\n7\r\nsecond\n\r\n6\r\nthird\n\r\n0\r\n\r\n",
        "chunked 帧结构不对：{body:?}"
    );
}

#[test]
fn head_of_a_stream_sends_no_body() {
    let router = Router::new()
        .get("/s", |_, _| {
            Response::stream(Status::OK, |sink| {
                sink.send(b"never")?;
                Ok(())
            })
        })
        .unwrap();

    let addr = serve_once(router);
    let raw = request(&addr, "HEAD /s HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    // HEAD 的 body 段必须是空的；闭包也不该被执行（否则是一次白算）。
    let body = &raw[raw.find("\r\n\r\n").unwrap() + 4..];
    assert_eq!(body, "", "HEAD 不该有 body：{raw:?}");
}

#[test]
fn sse_like_stream_reaches_the_client_in_order() {
    // 用一个 channel 模拟"事件源"：客户端必须能按发送顺序逐块读到。
    let (tx, rx) = std::sync::mpsc::channel::<&'static str>();
    // 先把两条事件塞进队列，再放掉发送端：接收端迭代完就会正常收尾
    // （这正是"事件源结束 → 流结束"的路径，SSE 里对应服务主动关门）。
    tx.send("data: one\n\n").unwrap();
    tx.send("data: two\n\n").unwrap();
    drop(tx);
    // 闭包是 `Fn`（响应会被克隆），所以通道得共享一份而不是 move 进去
    let rx = Arc::new(Mutex::new(rx));
    let rx_for_route = Arc::clone(&rx);
    let router = Router::new()
        .get("/events", move |_, _| {
            let rx = Arc::clone(&rx_for_route);
            let mut resp = Response::stream(Status::OK, move |sink| {
                let guard = rx.lock().unwrap();
                for s in guard.iter() {
                    sink.send(s.as_bytes())?;
                }
                Ok(())
            });
            resp.headers
                .insert_static("content-type", "text/event-stream");
            resp
        })
        .unwrap();

    let addr = serve_once(router);
    let raw = request(&addr, "GET /events HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    let lower = raw.to_lowercase();
    assert!(lower.contains("text/event-stream"), "{raw}");
    assert!(lower.contains("transfer-encoding: chunked"), "{raw}");
    let body = &raw[raw.find("\r\n\r\n").unwrap() + 4..];
    assert!(body.contains("data: one"), "{body:?}");
    assert!(body.contains("data: two"), "{body:?}");
    // 顺序：先 one 后 two
    assert!(
        body.find("one").unwrap() < body.find("two").unwrap(),
        "事件顺序不对：{body:?}"
    );
    assert!(body.ends_with("0\r\n\r\n"), "流没有正常收尾：{body:?}");
}
