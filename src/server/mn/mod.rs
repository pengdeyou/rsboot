//! 有栈协程运行时：让同步 handler 在 I/O 等待处让出 worker。
//!
//! ## 它解决什么
//!
//! worker-per-core 下 `QPS 上限 = worker 数 / 每请求阻塞时长`，一次 20ms 的
//! 下游调用就把 2 个 worker 焊死成 100 QPS，并发数完全不进这个公式（实测：
//! 5/20/200 并发全部 99~100）。这个上限不是调优能救的，是模型决定的。
//!
//! ## 它怎么解决
//!
//! handler 里凡是要等 I/O 的地方，改成"登记 + 让出"，worker 立刻回去处理
//! 别的连接；I/O 就绪或定时器到期再把它接回来。用户代码仍是普通同步函数。
//!
//! ## 边界
//! 只有**框架自己的 I/O** 走在这条链路上才有效。用户代码里直接
//! `std::thread::sleep` / `std::fs::read` 仍会焊死 worker——文档必须写明
//! "等待请用 `rsboot::sleep` 或框架提供的 I/O 原语"。

pub mod co;
pub mod sched;

use std::time::Duration;

/// 当前线程能不能让出（即处在 worker 的调度器里）。
pub fn ready() -> bool {
    sched::ready()
}

/// 让出 CPU。不在协程里调用时退化为空操作。
pub fn yield_now() {
    if sched::ready() {
        sched::yield_now();
    }
}

/// 睡一段时间。在协程里调用时**不占用 worker**；不在协程里则退化为
/// `std::thread::sleep`（测试、单元环境都安全）。
pub fn sleep(d: Duration) {
    if sched::ready() {
        sched::sleep(d);
    } else {
        std::thread::sleep(d);
    }
}
