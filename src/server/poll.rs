//! `poll(2)` 的极小 FFI：等监听 fd 可读，别用 sleep 轮询。
//!
//! ## 为什么必须修
//!
//! 早先用 `accept` 非阻塞 + `sleep(20ms)` 轮询。keep-alive 长连接下看不出来
//! （连接建立一次用很久），但**短连接场景直接暴露**：每条新连接平均要多等
//! 10ms（20ms 的一半），基准测出 P50 = 20ms、QPS 116 —— 一个人为造成的
//! 数量级损失。健康检查、curl、HTTP/1.0 客户端全都会中招。
//!
//! `poll` 让主线程真正睡着，连接一到立刻醒：延迟降到亚毫秒，CPU 空转归零，
//! 顺带把关闭/跛鸭的判定精度从 20ms 提到 5ms。
//!
//! ## 为什么自己写而不用 crate
//!
//! `poll` 是 POSIX 标准函数，一个结构体一个 extern 声明就够了。为它引
//! `polling`/`mio` 会把"零依赖"这个卖点换掉。非 unix 平台退化为短 sleep。

#[cfg(all(unix, not(target_os = "macos")))]
type Nfds = std::os::raw::c_ulong;
#[cfg(target_os = "macos")]
type Nfds = std::os::raw::c_uint;

#[repr(C)]
struct PollFd {
    fd: std::os::raw::c_int,
    events: std::os::raw::c_short,
    revents: std::os::raw::c_short,
}

const POLLIN: std::os::raw::c_short = 0x0001;

#[cfg(all(unix, not(target_os = "macos")))]
unsafe extern "C" {
    fn poll(fds: *mut PollFd, nfds: Nfds, timeout: std::os::raw::c_int) -> std::os::raw::c_int;
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn poll(fds: *mut PollFd, nfds: Nfds, timeout: std::os::raw::c_int) -> std::os::raw::c_int;
}

/// 等 `fd` 可读最多 `timeout_ms` 毫秒。返回 `true` = 可读，`false` = 超时。
///
/// 被信号打断（`EINTR`）也算超时：主循环下一轮会重新进来，不需要特判。
#[cfg(unix)]
pub fn wait_readable(fd: std::os::raw::c_int, timeout_ms: i32) -> std::io::Result<bool> {
    let mut p = PollFd {
        fd,
        events: POLLIN,
        revents: 0,
    };
    // SAFETY: 结构体布局与 `struct pollfd` 一致（int/short/short），
    // 只传一个元素，`poll` 只读写这一块栈内存。
    let n = unsafe { poll(&mut p, 1, timeout_ms) };
    if n < 0 {
        let e = std::io::Error::last_os_error();
        // EINTR = 4：被信号打断，当成超时处理（调用方会重试）
        if e.raw_os_error() == Some(4) {
            return Ok(false);
        }
        return Err(e);
    }
    Ok(n > 0)
}

/// 一次等多个 fd，返回"有事件"的下标列表。这是 worker 的主循环入口：
/// 一条连接一个线程会堆出成千上万线程，`poll` 多路复用让它天然能服务多条。
#[cfg(unix)]
pub fn wait_multi(fds: &[std::os::raw::c_int], timeout_ms: i32) -> std::io::Result<Vec<usize>> {
    if fds.is_empty() {
        return Ok(Vec::new());
    }
    let mut v: Vec<PollFd> = fds
        .iter()
        .map(|&fd| PollFd {
            fd,
            events: POLLIN,
            revents: 0,
        })
        .collect();
    // SAFETY: `v` 与 `struct pollfd` 数组内存布局一致，长度如实传入。
    let n = unsafe { poll(v.as_mut_ptr(), v.len() as Nfds, timeout_ms) };
    if n < 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(4) {
            return Ok(Vec::new());
        }
        return Err(e);
    }
    Ok(v.iter()
        .enumerate()
        .filter(|(_, p)| p.revents != 0)
        .map(|(i, _)| i)
        .collect())
}

/// 非 unix：没有 `poll`，退化成短睡眠。功能正确，只是延迟回到毫秒级。
#[cfg(not(unix))]
pub fn wait_readable(_fd: std::os::raw::c_int, timeout_ms: i32) -> std::io::Result<bool> {
    std::thread::sleep(std::time::Duration::from_millis(timeout_ms.max(1) as u64));
    Ok(true)
}

#[cfg(not(unix))]
pub fn wait_multi(fds: &[std::os::raw::c_int], timeout_ms: i32) -> std::io::Result<Vec<usize>> {
    std::thread::sleep(std::time::Duration::from_millis(timeout_ms.max(1) as u64));
    Ok((0..fds.len()).collect())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn poll_timeout_returns_false() {
        use std::os::unix::io::AsRawFd;
        // 一个没人写的 socket 对端：必然超时
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let t0 = std::time::Instant::now();
        assert!(!super::wait_readable(l.as_raw_fd(), 30).unwrap());
        assert!(t0.elapsed() >= std::time::Duration::from_millis(20));
    }

    #[cfg(unix)]
    #[test]
    fn poll_wakes_on_connect() {
        use std::os::unix::io::AsRawFd;
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            let _ = std::net::TcpStream::connect(addr);
        });
        assert!(super::wait_readable(l.as_raw_fd(), 500).unwrap());
        assert!(l.accept().is_ok());
    }
}
