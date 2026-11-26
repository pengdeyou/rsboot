//! 主线程的 accept 循环。
//!
//! ## 为什么是 poll 而不是"自连一次唤醒"
//!
//! 没有用 "自连一次唤醒 accept" 或 signal：`poll` 5ms 一超时，空转成本近乎零，
//! 换来的是关闭语义简单可靠（置位 flag，下一轮就退出），也不会在容器里因为
//! 信号投递不确定而卡住。
//!
//! ## 分发顺序约束
//!
//! **先入队，后唤醒**。反过来 worker 醒来时队列可能还是空的，白跑一轮；
//! worker 的 `try_recv` 依赖这个顺序才能保证"醒来必有活"。

use std::io::Write;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::server::conn::{Conn, Limits};
use crate::server::{poll, signal};

use super::{ServeState, ServeStatus};

/// `poll` 的单个超时上限：决定关闭/跛鸭判定的精度，也决定空闲时醒来的频率。
/// 5ms 对关闭响应足够快，对 CPU 几乎等于零成本（poll 不占 CPU，只在超时后醒一次）。
const ACCEPT_POLL_MS: i32 = 5;
/// 一次可读最多排空多少条连接：突发流量下别让 poll 和 accept 来回打转。
const MAX_ACCEPT_PER_WAIT: usize = 64;

/// 循环需要的全部东西。字段都从 `Server` 借来，循环本身不持有所有权。
pub(crate) struct Loop<'a> {
    pub listener: &'a TcpListener,
    pub senders: &'a [SyncSender<Conn>],
    pub wakers: &'a [poll::Waker],
    pub limits: Limits,
    pub workers: usize,
    pub shutdown: &'a AtomicBool,
    pub closing: &'a AtomicBool,
    pub status: &'a ServeStatus,
    pub drain: Duration,
    pub watch_signal: bool,
}

pub(crate) fn run(l: Loop<'_>) -> Result<()> {
    l.listener
        .set_nonblocking(true)
        .map_err(|e| Error::Lifecycle(format!("设置非阻塞模式失败：{e}")))?;

    let mut rr = 0usize;
    // 跛鸭期的截止时刻。`None` = 还没开始关。
    let mut drain_deadline: Option<Instant> = None;

    loop {
        if l.shutdown.load(Ordering::Relaxed) {
            l.status.set(ServeState::Closing);
            l.closing.store(true, Ordering::Relaxed);
            break;
        }
        // 第一次看到信号：进入跛鸭期。此间照常接客，过了预算才真停。
        if drain_deadline.is_none() && l.watch_signal && signal::received() {
            l.status.set(ServeState::Draining);
            drain_deadline = Some(Instant::now() + l.drain);
        }
        if let Some(d) = drain_deadline {
            if Instant::now() >= d {
                l.status.set(ServeState::Closing);
                // 让 worker 里**已建立的连接**开始礼貌拒绝，
                // 不然它们会一路 keep-alive 到 idle 超时才停
                l.closing.store(true, Ordering::Relaxed);
                break;
            }
        }

        // 睡到"有连接"为止，而不是 sleep 轮询。轮询会让每条短连接
        // 白等最多一个轮询周期（原来是 20ms，基准里直接把 QPS 打到 116）。
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            match poll::wait_readable(l.listener.as_raw_fd(), ACCEPT_POLL_MS) {
                Ok(true) => {}
                Ok(false) => continue, // 超时：回到循环头检查关闭/drain
                Err(e) => {
                    eprintln!("[rsboot] poll 失败：{e}");
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
            }
        }
        #[cfg(not(unix))]
        {
            std::thread::sleep(Duration::from_millis(ACCEPT_POLL_MS as u64));
        }

        // 一次可读往往意味着 backlog 里不止一条：排空它，别让突发连接
        // 排队等下一轮 poll。上限 64 免得一条线程被 accept 独占。
        for _ in 0..MAX_ACCEPT_PER_WAIT {
            match l.listener.accept() {
                Ok((stream, _peer)) => {
                    let wi = rr % l.workers;
                    rr = rr.wrapping_add(1);
                    let tx = &l.senders[wi];
                    let wk = &l.wakers[wi];
                    // 每条连接只在这里 dup 一次 fd（读缓冲跟着连接走）
                    let c = match Conn::new(stream, &l.limits) {
                        Ok(c) => c,
                        Err(_) => continue,
                    };
                    match tx.try_send(c) {
                        // 先入队、后唤醒：worker 醒来必能取到，不会空跑
                        Ok(()) => wk.notify(),
                        // 队列满或 worker 已死 → 当场 503，不进内核 backlog 继续堆
                        Err(TrySendError::Full(c)) | Err(TrySendError::Disconnected(c)) => {
                            reject_overloaded(c.into_stream())
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    // 文件描述符耗尽之类的临时错误：退避后重试，不让整个服务退出
                    eprintln!("[rsboot] accept 失败：{e}");
                    std::thread::sleep(Duration::from_millis(100));
                    break;
                }
            }
        }
    }
    Ok(())
}

/// 队列满时当场回 503 并关连接。排队只会把超时往后推——客户端等到的仍是失败，
/// 却多占了一份内存。
pub(crate) fn reject_overloaded(mut s: std::net::TcpStream) {
    let _ = s.set_write_timeout(Some(Duration::from_millis(200)));
    let _ = s.write_all(
        b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
    );
    let _ = s.flush();
    // 直接 drop：由内核发 FIN。不做四次挥手等待，这里没有值得保护的状态。
}
