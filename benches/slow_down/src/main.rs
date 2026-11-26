//! 假下游：每个请求睡 `DELAY_MS` 再回答。给 `/down` 端点当被调方用。
//!
//! 刻意是"每连接一线程"的朴素实现——它只是延迟源，不是被测对象。

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

fn main() {
    let bind = std::env::var("BIND").unwrap_or_else(|_| "127.0.0.1:9010".into());
    let delay: u64 = std::env::var("DELAY_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let l = TcpListener::bind(&bind).expect("绑不上端口");
    println!("slow-down 监听 {bind}，每请求 {delay}ms");
    for stream in l.incoming() {
        let Ok(s) = stream else { continue };
        std::thread::spawn(move || {
            let mut r = BufReader::new(match s.try_clone() {
                Ok(c) => c,
                Err(_) => return,
            });
            let mut w = s;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    return;
                }
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 {
                        return;
                    }
                    if h.trim().is_empty() {
                        break;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(delay));
                let body = b"{\"up\":1}";
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n",
                    body.len()
                );
                if w.write_all(head.as_bytes()).is_err() || w.write_all(body).is_err() {
                    return;
                }
            }
        });
    }
}
