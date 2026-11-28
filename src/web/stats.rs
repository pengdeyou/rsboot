//! 统计钩子。
//!
//! 核心不认识"指标"这个概念：`/metrics`、APM 上报、慢请求日志都是**可选的**，
//! 想采数据的一方（比如 `rsboot-ops` 的 Prometheus 端点）在启动时装一个
//! 函数指针进来。
//!
//! 没装的时候热路径只多一次原子读 + 一个分支（`begin` → `None`，`end` 直接
//! 返回，**连时钟都不读**）；装了则每请求两次 `rdtsc`。
//!
//! ```ignore
//! rsboot::web::stats::install(|status, ns| { /* 记账 */ });
//! ```

use std::sync::atomic::{AtomicUsize, Ordering};

/// 观察者：`(HTTP 状态码, 请求耗时纳秒)`。
pub type Observer = fn(u16, u64);

static OBSERVER: AtomicUsize = AtomicUsize::new(0);

/// 装一个观察者。重复调用会替换（后者生效）。
pub fn install(observer: Observer) {
    OBSERVER.store(observer as usize, Ordering::Release);
}

/// 是否装了观察者。
#[inline]
pub fn enabled() -> bool {
    OBSERVER.load(Ordering::Acquire) != 0
}

/// 记起点。没装观察者返回 `None`——调用方拿 `None` 走快速路径。
#[inline]
pub fn begin() -> Option<u64> {
    if !enabled() {
        return None;
    }
    Some(crate::server::clock::now_ns())
}

/// 记终点并分发。
#[inline]
pub fn end(t0: Option<u64>, status: u16) {
    let Some(t0) = t0 else { return };
    let observer = OBSERVER.load(Ordering::Acquire);
    if observer == 0 {
        return;
    }
    let t1 = crate::server::clock::now_ns();
    // SAFETY: 这个地址只可能是 `install` 存进去的 `Observer`（`fn` 指针，
    // 无捕获、`'static`），读回后立刻调用一次；`install` 是唯一写入点。
    let f: Observer = unsafe { std::mem::transmute::<usize, Observer>(observer) };
    f(status, t1.saturating_sub(t0));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    static SEEN: AtomicUsize = AtomicUsize::new(0);
    fn record(_status: u16, _ns: u64) {
        SEEN.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    fn fires_only_after_install() {
        let before = begin();
        assert!(before.is_none(), "默认没装观察者，连时钟都不该读");
        install(record);
        assert!(enabled());
        let t0 = begin().expect("装了之后要有时钟");
        end(Some(t0), 200);
        end(None, 200);
        assert_eq!(SEEN.load(Ordering::Relaxed), 1, "None 不该分发");
    }
}
