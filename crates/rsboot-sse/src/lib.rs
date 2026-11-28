//! `rsboot-sse`：Server-Sent Events。
//!
//! 传输由核心的 [`Response::stream`] 负责（chunked），这个 crate 只做两件事：
//! 把事件拼成符合 SSE 规范的那几行文本，以及给"事件源 → 事件流"配一个
//! 带心跳的适配器。
//!
//! ```ignore
//! use rsboot_sse::{Event, Sse};
//!
//! let (tx, rx) = std::sync::mpsc::channel();
//! std::thread::spawn(move || {
//!     for i in 0..3 {
//!         tx.send(Event::new(format!("第 {i} 条"))).ok();
//!         std::thread::sleep(std::time::Duration::from_millis(200));
//!     }
//! });
//!
//! Router::new().get("/events", move |_, _| Sse::new().stream(rx_clone))?;
//! ```
//!
//! ## 为什么心跳是必需的
//!
//! SSE 是长连接。中间只要有一层代理（nginx、云厂商的 L7、企业网关）在
//! 60 秒内看不到字节，就会把连接掐掉——前端看到的现象是"EventSource 每隔
//! 一分钟自动重连一次"，而后端日志里什么都没有。注释帧（`: keep-alive`）
//! 不触发前端事件，但足以让代理认为连接还活着。

#![deny(missing_docs)]

use std::io;
use std::sync::mpsc::Receiver;
use std::time::Duration;

/// 事件轮询粒度。50 ms 对"看起来即时"足够，又不会把 worker 唤醒得太频繁
/// （一条空闲连接每秒醒 20 次，比每 15 秒阻塞一次要便宜得多）。
const TICK: Duration = Duration::from_millis(50);

use rsboot::web::http::Status;
use rsboot::web::response::{ChunkSink, Response};

/// 一条事件。
#[derive(Debug, Clone, Default)]
pub struct Event {
    /// 事件 ID。断线重连时前端会带上 `Last-Event-ID`，后端可以据此补发。
    pub id: Option<String>,
    /// 事件名。不填就是默认的 `message` 事件（`onmessage` 收得到）。
    pub event: Option<String>,
    /// 重连间隔（毫秒），前端会用它重置自己的重连计时。
    pub retry: Option<u64>,
    /// 数据。多行会被拆成多条 `data:` 行——SSE 规范就是这么拼多行文本的。
    pub data: String,
}

impl Event {
    /// 一条只有数据的默认事件（前端 `onmessage` 能收到）。
    pub fn new(data: impl Into<String>) -> Self {
        Self {
            data: data.into(),
            ..Default::default()
        }
    }

    /// 命名事件：前端要用 `addEventListener("progress", …)` 才收得到。
    pub fn event(mut self, name: impl Into<String>) -> Self {
        self.event = Some(name.into());
        self
    }

    /// 事件 ID，配合前端的 `Last-Event-ID` 做断线续传。
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// 建议的重连间隔（毫秒）。
    pub fn retry(mut self, ms: u64) -> Self {
        self.retry = Some(ms);
        self
    }

    /// 拼成帧并写出去。**整条事件一次 `send`**：一条事件拆成四五次写，
    /// 就是把四次系统调用换成四次机会让客户端看到"半条事件"。
    pub fn write_to(&self, sink: &mut dyn ChunkSink) -> io::Result<()> {
        let mut frame = String::with_capacity(self.data.len() + 32);
        if let Some(id) = &self.id {
            // id 里换个行就把帧拆坏了
            frame.push_str("id: ");
            frame.push_str(&id.replace(['\r', '\n'], " "));
            frame.push('\n');
        }
        if let Some(ev) = &self.event {
            frame.push_str("event: ");
            frame.push_str(&ev.replace(['\r', '\n'], " "));
            frame.push('\n');
        }
        if let Some(ms) = self.retry {
            frame.push_str("retry: ");
            frame.push_str(&ms.to_string());
            frame.push('\n');
        }
        for line in self.data.split('\n') {
            frame.push_str("data: ");
            frame.push_str(line.trim_end_matches('\r'));
            frame.push('\n');
        }
        frame.push('\n');
        sink.send(frame.as_bytes())
    }
}

/// SSE 响应工厂。
#[derive(Debug, Clone)]
pub struct Sse {
    heartbeat: Duration,
}

impl Default for Sse {
    fn default() -> Self {
        Self::new()
    }
}

impl Sse {
    /// 心跳 15 秒：比大多数代理的 60 秒空闲阈值短得多，留了三次余量。
    ///
    /// 心跳只是"发一条注释帧"的阈值，实际轮询按 [`TICK`] 走。
    pub fn new() -> Self {
        Self {
            heartbeat: Duration::from_secs(15),
        }
    }

    /// 改心跳间隔。测试里会调到几十毫秒。
    pub fn heartbeat(mut self, d: Duration) -> Self {
        self.heartbeat = d.max(Duration::from_millis(10));
        self
    }

    /// 把一条事件通道变成 SSE 响应。
    ///
    /// 通道断开（发送端全部 drop）时流正常收尾：发终止块，连接可以复用或
    /// 优雅关闭。这是"业务算完了"的正常路径，不是错误。
    pub fn stream(self, rx: Receiver<Event>) -> Response {
        let heartbeat = self.heartbeat;
        // `Response` 会因中间件被 clone，闭包必须是 `Fn`；接收端只能消费一次，
        // 所以用 `Option` + `take`：第一次调用拿走，之后调用立即返回（流早就完了）。
        let slot = std::sync::Mutex::new(Some(rx));
        let mut resp = Response::stream(Status::OK, move |sink| {
            let Some(rx) = slot.lock().unwrap().take() else {
                return Ok(());
            };
            let mut idle = Duration::ZERO;
            loop {
                match rx.try_recv() {
                    Ok(ev) => {
                        idle = Duration::ZERO;
                        ev.write_to(sink)?;
                        continue;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                }
                // 关键：**不能**用 `recv()`/`recv_timeout()`。std 的通道没有
                // fd，阻塞等 15 秒就是把整个 worker 冻 15 秒——同一 worker 上
                // 其它连接全部停摆。这里用框架的让出式 sleep，CPU 交出去，
                // 到点再回来看看有没有事件。
                rsboot::server::mn::sleep(TICK);
                idle += TICK;
                if idle >= heartbeat {
                    idle = Duration::ZERO;
                    // 注释帧：前端看不见，代理能看见，防空闲断流
                    sink.send(b": keep-alive\n\n")?;
                }
            }
            Ok(())
        });
        resp.headers
            .insert_static("content-type", "text/event-stream; charset=utf-8");
        // 三个头都在防同一件事：中间层把这条流缓存或攒起来
        resp.headers.insert_static("cache-control", "no-cache");
        resp.headers.insert_static("x-accel-buffering", "no");
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MemSink(Vec<u8>);

    impl ChunkSink for MemSink {
        fn send(&mut self, chunk: &[u8]) -> io::Result<()> {
            self.0.extend_from_slice(chunk);
            Ok(())
        }
    }

    fn frames(events: Vec<Event>) -> String {
        let mut sink = MemSink::default();
        for e in events {
            e.write_to(&mut sink).unwrap();
        }
        String::from_utf8(sink.0).unwrap()
    }

    #[test]
    fn a_plain_event_is_one_data_line() {
        assert_eq!(frames(vec![Event::new("hello")]), "data: hello\n\n");
    }

    #[test]
    fn multiline_data_becomes_multiple_data_lines() {
        assert_eq!(
            frames(vec![Event::new("a\nb")]),
            "data: a\ndata: b\n\n",
            "多行必须拆成多条 data:，否则前端收到的是一个被截断的字符串"
        );
    }

    #[test]
    fn id_event_retry_and_order() {
        let e = Event::new("x").id("42").event("progress").retry(3000);
        assert_eq!(
            frames(vec![e]),
            "id: 42\nevent: progress\nretry: 3000\ndata: x\n\n"
        );
    }

    #[test]
    fn newlines_in_id_do_not_break_the_frame() {
        let e = Event::new("x").id("a\nb");
        assert_eq!(frames(vec![e]), "id: a b\ndata: x\n\n");
    }

    #[test]
    fn stream_sends_events_in_order_then_terminates() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Event::new("one")).unwrap();
        tx.send(Event::new("two")).unwrap();
        drop(tx);

        let resp = Sse::new().stream(rx);
        let mut sink = MemSink::default();
        resp.stream.unwrap().send(&mut sink).unwrap();
        assert_eq!(
            String::from_utf8(sink.0).unwrap(),
            "data: one\n\ndata: two\n\n"
        );
    }

    #[test]
    fn idle_stream_emits_keep_alive() {
        let (tx, rx) = std::sync::mpsc::channel();
        let resp = Sse::new().heartbeat(Duration::from_millis(20)).stream(rx);
        let mut sink = MemSink::default();
        // 没有事件，只有心跳；200 ms 后放掉发送端结束
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(tx);
        });
        resp.stream.unwrap().send(&mut sink).unwrap();
        h.join().unwrap();
        let out = String::from_utf8(sink.0).unwrap();
        assert!(out.starts_with(": keep-alive\n\n"), "{out:?}");
        assert!(out.matches(": keep-alive").count() >= 3, "{out:?}");
    }
}
