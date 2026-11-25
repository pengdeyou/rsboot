//! SIGINT / SIGTERM → shutdown 标志。
//!
//! ## 为什么自己写 FFI，不引 `signal-hook`
//!
//! 需要的功能只有一个：收到信号时把一个 `AtomicBool` 置位。`signal-hook` 提供
//! 的是完整的事件循环、注册表、跨平台抽象，而它同样最终要靠一个全局可变状态
//! 让"信号处理函数"和"业务代码"通信。核心框架为这个能力多背一个依赖不划算。
//!
//! ## 安全边界
//!
//! 信号处理函数里**只能**做 async-signal-safe 的事。这里只做
//! `AtomicBool::store`——它不分配内存、不加锁、不调用任何可能重入的函数。
//! 绝不在这里打日志、加锁、清理资源：那些操作在信号上下文里可能死锁。
//!
//! ## 只处理两个信号
//!
//! `SIGINT`（Ctrl+C）与 `SIGTERM`（容器/系统正常停止）。不接管 `SIGHUP`
//! （重新加载配置另有热加载机制），不接管 `SIGQUIT`（留给调试器取栈）。
//!
//! ## 非 Unix 平台
//!
//! 退化为"永不收到信号"。Windows 要做得用 `SetConsoleCtrlHandler`，
//! 目前不做——在 Windows 上按 Ctrl+C 会直接结束进程，这一点写在文档里，
//! 好过假装支持却行为不一致。

use std::sync::atomic::{AtomicBool, Ordering};

/// 进程级标志。信号处理函数只能写全局静态量。
static RECEIVED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
mod imp {
    use super::RECEIVED;
    use std::ffi::c_void;
    use std::sync::atomic::Ordering;
    use std::sync::Once;

    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;

    unsafe extern "C" {
        /// `sighandler_t signal(int signum, sighandler_t handler)`。
        /// 返回值可能是 `SIG_ERR`（非函数指针），所以拿 `*mut c_void` 接住并不使用它。
        fn signal(signum: i32, handler: *mut c_void) -> *mut c_void;
    }

    extern "C" fn on_signal(_sig: i32) {
        // 只有这一行：async-signal-safe
        RECEIVED.store(true, Ordering::SeqCst);
    }

    pub fn install() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            let h = on_signal as *const () as *mut c_void;
            unsafe {
                signal(SIGINT, h);
                signal(SIGTERM, h);
            }
        });
    }

    pub fn received() -> bool {
        RECEIVED.load(Ordering::SeqCst)
    }
}

#[cfg(not(unix))]
mod imp {
    pub fn install() {}
    pub fn received() -> bool {
        false
    }
}

/// 安装处理函数。幂等：重复调用只有第一次生效。
pub fn install() {
    imp::install();
}

/// 是否已经收到过 SIGINT / SIGTERM。
pub fn received() -> bool {
    imp::received()
}

/// 清空标志。写测试与"用完复位"时需要。
pub fn reset() {
    RECEIVED.store(false, Ordering::SeqCst);
}

/// 模拟一次信号（测试用）。与真信号走同一个标志，所以能验证整条关闭路径。
pub fn trigger_for_test() {
    RECEIVED.store(true, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_roundtrip() {
        reset();
        assert!(!received());
        trigger_for_test();
        assert!(received());
        reset();
        assert!(!received());
    }

    #[test]
    fn install_is_idempotent() {
        // 重复安装不该 panic（Once 保护）
        install();
        install();
    }
}
