//! 启动握手与关闭序列。
//!
//! 两件事都只做"等"和"叫醒"，不碰连接的业务逻辑，所以单独一层更清楚：
//! `run` 只负责把顺序串起来，具体怎么等、等多久、超时算什么，都在这里。

use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::server::conn::Conn;
use crate::server::poll;
use crate::server::ServeState;
use crate::server::ServeStatus;

/// 等全部 worker 报告就绪。**全部**就绪才开始 accept：
/// 两个 worker 只起来一个，比直接启动失败更难查。
pub(crate) fn await_ready(
    workers: usize,
    rx: Receiver<std::result::Result<(), String>>,
    senders: Arc<Vec<SyncSender<Conn>>>,
    handles: Vec<std::thread::JoinHandle<()>>,
) -> Result<Vec<std::thread::JoinHandle<()>>> {
    let mut ready_ok = 0usize;
    let mut failed: Option<String> = None;
    for _ in 0..workers {
        match rx.recv() {
            Ok(Ok(())) => ready_ok += 1,
            Ok(Err(e)) => failed = Some(e),
            // 所有 sender 都 drop 了还没等齐：说明有 worker 在握手前就退了
            Err(_) => break,
        }
    }
    if let Some(e) = failed {
        drop(senders);
        for h in handles {
            let _ = h.join();
        }
        return Err(Error::Lifecycle(format!("启动失败：{e}")));
    }
    if ready_ok == 0 {
        return Err(Error::Lifecycle(
            "没有一个 worker 成功启动（检查数据库配置与模块的 start_worker）".into(),
        ));
    }
    Ok(handles)
}

/// 关闭序列：置状态 → 叫醒 worker（它们可能正阻塞在 poll 里）→ 丢 sender
/// （worker 的 `recv` 返回错误）→ 有预算地等它们把手上连接处理完。
///
/// 某个 worker 卡在一条慢请求上不该让整个进程永远退不出去——超时就放弃等待
/// （线程泄漏，但进程马上结束，无碍），并记一笔日志说明有请求被硬切断。
pub(crate) fn shutdown(
    wakers: &[poll::Waker],
    senders: Arc<Vec<SyncSender<Conn>>>,
    handles: Vec<std::thread::JoinHandle<()>>,
    grace: Duration,
    status: &ServeStatus,
) {
    status.set(ServeState::Closing);
    // worker 此时可能正阻塞在 poll 里（兜底超时 500ms），必须叫醒它们去看
    // should_stop，否则关闭会白等一个兜底周期。
    for w in wakers {
        w.notify();
    }
    drop(senders);

    let deadline = Instant::now() + grace;
    let mut stragglers = 0usize;
    for h in handles {
        while !h.is_finished() {
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if h.is_finished() {
            let _ = h.join();
        } else {
            stragglers += 1;
        }
    }
    if stragglers > 0 {
        eprintln!(
            "[rsboot] 关闭超过 {grace:?}，{stragglers} 个 worker 仍未退出（进行中的请求可能被硬切断）"
        );
    }
}
