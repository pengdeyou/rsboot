//! `poll(2)` 的极小 FFI，外加一个**唤醒器**（`Waker`）和一个可复用的 `PollSet`。
//!
//! ## 为什么必须修（第一课）
//!
//! 早先用 `accept` 非阻塞 + `sleep(20ms)` 轮询。keep-alive 长连接下看不出来
//! （连接建立一次用很久），但**短连接场景直接暴露**：每条新连接平均要多等
//! 10ms（20ms 的一半），基准测出 P50 = 20ms、QPS 116 —— 一个人为造成的
//! 数量级损失。健康检查、curl、HTTP/1.0 客户端全都会中招。
//!
//! `poll` 让主线程真正睡着，连接一到立刻醒：延迟降到亚毫秒，CPU 空转归零，
//! 顺带把关闭/跛鸭的判定精度从 20ms 提到 5ms。
//!
//! ## 为什么还要 `Waker`（第二课）
//!
//! worker 侧的 `poll` 不能一直用 1ms 超时：那是**每 worker 每秒 1000 次**无谓
//! 唤醒，空闲时白烧 CPU；而新连接最多要晚一个 tick（1ms）才被派发，这个延迟
//! 直接体现在 P99 上（这也是 axum 的中位 P99 略优于我们的原因）。正确做法是
//! 把超时设成"近乎无限"，由**派发连接的一方**主动写一个 `eventfd` 叫醒 worker：
//!
//! - Linux：`eventfd`，一个 fd 既能等又能写，计数由内核维护；
//! - macOS：没有 `eventfd`，退化成一对非阻塞 `pipe`；
//! - 其他平台：退化为短 sleep（功能正确，延迟回到毫秒级）。
//!
//! 兜底超时保留（`WORKER_IDLE_MS`），不是给正常运行用的：它是"唤醒链路哪天
//! 漏了一处"时的保险丝，防止 worker 永久卡在 `poll` 里关不掉。
//!
//! ## 为什么自己写而不用 crate
//!
//! `poll`/`eventfd`/`pipe` 都是标准系统调用，几个 `extern` 声明就够。为它们引
//! `polling`/`mio` 会把"零依赖"这个卖点换掉。

#[cfg(all(unix, not(target_os = "macos")))]
type Nfds = std::os::raw::c_ulong;
#[cfg(target_os = "macos")]
type Nfds = std::os::raw::c_uint;

#[cfg(unix)]
use std::os::raw::{c_int, c_short, c_void};

#[repr(C)]
#[cfg(unix)]
struct PollFd {
    fd: c_int,
    events: c_short,
    revents: c_short,
}

#[cfg(unix)]
const POLLIN: c_short = 0x0001;

#[cfg(all(unix, not(target_os = "macos")))]
unsafe extern "C" {
    fn poll(fds: *mut PollFd, nfds: Nfds, timeout: c_int) -> c_int;
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn poll(fds: *mut PollFd, nfds: Nfds, timeout: c_int) -> c_int;
}

#[cfg(unix)]
unsafe extern "C" {
    fn read(fd: c_int, buf: *mut c_void, n: usize) -> isize;
    fn write(fd: c_int, buf: *const c_void, n: usize) -> isize;
    fn close(fd: c_int) -> c_int;
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn pipe(fds: *mut c_int) -> c_int;
    fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
}

#[cfg(all(unix, not(target_os = "macos")))]
unsafe extern "C" {
    fn eventfd(initval: std::os::raw::c_uint, flags: c_int) -> c_int;
}

/// `EFD_CLOEXEC`：exec 后自动关闭
#[cfg(all(unix, not(target_os = "macos")))]
const EFD_CLOEXEC: c_int = 0o2000000;
/// `EFD_NONBLOCK`：读写立即返回（`drain` 依赖它读到 `EAGAIN` 才停）
#[cfg(all(unix, not(target_os = "macos")))]
const EFD_NONBLOCK: c_int = 0o4000;

#[cfg(target_os = "macos")]
const F_SETFL: c_int = 4;
#[cfg(target_os = "macos")]
const O_NONBLOCK: c_int = 0x0004;

/// 等 `fd` 可读最多 `timeout_ms` 毫秒。返回 `true` = 可读，`false` = 超时。
///
/// 被信号打断（`EINTR`）也算超时：主循环下一轮会重新进来，不需要特判。
#[cfg(unix)]
pub fn wait_readable(fd: c_int, timeout_ms: i32) -> std::io::Result<bool> {
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

/// 一次等多个 fd，返回"有事件"的下标列表。
///
/// 热路径一律走 `PollSet`（缓冲常驻、零分配）；这个旧接口只留给单测和外部工具。
#[cfg(unix)]
pub fn wait_multi(fds: &[c_int], timeout_ms: i32) -> std::io::Result<Vec<usize>> {
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

/// 唤醒器：把阻塞在 `poll` 上的线程立刻叫醒。
///
/// `Clone` 是廉价的（`Arc` 自增）：worker 持一份用于读，派发方持一份用于写。
#[cfg(unix)]
#[derive(Clone)]
pub struct Waker {
    inner: std::sync::Arc<WakerInner>,
}

#[cfg(unix)]
struct WakerInner {
    read: c_int,
    write: c_int,
}

#[cfg(unix)]
impl Drop for WakerInner {
    fn drop(&mut self) {
        // SAFETY: fd 由本结构独占，析构时必然不再被使用。
        // eventfd 的读写是同一个 fd，只能关一次。
        unsafe {
            if self.write != self.read {
                close(self.write);
            }
            close(self.read);
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
impl Waker {
    pub fn new() -> std::io::Result<Self> {
        // SAFETY: 单纯的系统调用。
        let fd = unsafe { eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self {
            inner: std::sync::Arc::new(WakerInner { read: fd, write: fd }),
        })
    }
}

#[cfg(target_os = "macos")]
impl Waker {
    pub fn new() -> std::io::Result<Self> {
        let mut p: [c_int; 2] = [0, 0];
        // SAFETY: 传给 `pipe` 的是本栈上的两元素数组。
        if unsafe { pipe(p.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // 两端都必须非阻塞：`drain` 要读到 EAGAIN 才停，否则会卡住 worker
        // SAFETY: 对刚创建的 fd 设置标志位。
        unsafe {
            fcntl(p[0], F_SETFL, O_NONBLOCK);
            fcntl(p[1], F_SETFL, O_NONBLOCK);
        }
        Ok(Self {
            inner: std::sync::Arc::new(WakerInner {
                read: p[0],
                write: p[1],
            }),
        })
    }
}

#[cfg(unix)]
impl Waker {
    /// 放进 `PollSet` 的那个 fd。
    pub fn read_fd(&self) -> c_int {
        self.inner.read
    }

    /// 叫醒所有在等它的线程。计数语义，重复调用是累加的，不会丢。
    ///
    /// 写失败（对端已满 / 已关闭）一律忽略：唤醒是"尽力而为"的优化，
    /// 漏一次还有 `WORKER_IDLE_MS` 兜底，不需要在这里制造错误分支。
    pub fn notify(&self) {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let v: u64 = 1;
            // SAFETY: 写 8 字节到 eventfd，这是它的规定用法（小端无符号计数）。
            unsafe {
                write(
                    self.inner.write,
                    (&v as *const u64).cast::<c_void>(),
                    8,
                );
            }
        }
        #[cfg(target_os = "macos")]
        {
            let b: u8 = 1;
            // SAFETY: 写 1 字节到管道写端。`b` 是 `u8`，取址才是 `*const u8`——
            // `u8` 上没有 `as_ptr()`（原来写成 `b.as_ptr()`，这段只在 macOS 编译，
            // Linux 上被 cfg 排除，一直没暴露）
            unsafe {
                write(self.inner.write, (&b as *const u8).cast::<c_void>(), 1);
            }
        }
    }

    /// 把计数读空。**醒来后必须调用**，否则下一轮 `poll` 会立刻又返回。
    pub fn drain(&self) {
        let mut b = [0u8; 64];
        loop {
            // SAFETY: 读本结构独占的 fd，缓冲区在栈上。
            let n = unsafe { read(self.inner.read, b.as_mut_ptr() as *mut c_void, b.len()) };
            if n <= 0 {
                // 非阻塞且已空 → -1(EAGAIN)；fd 已关闭 → 0
                break;
            }
        }
    }
}

#[cfg(not(unix))]
#[derive(Clone, Default)]
pub struct Waker;

#[cfg(not(unix))]
impl Waker {
    pub fn new() -> std::io::Result<Self> {
        Ok(Self)
    }
    pub fn notify(&self) {}
    pub fn drain(&self) {}
    /// 非 unix 用 -1 表示"没有可等的 fd"。
    pub fn read_fd(&self) -> i32 {
        -1
    }
}

/// 常驻的 `pollfd` 集合：fd 数组与就绪下标缓冲都只分配一次，之后原地复用。
///
/// 为什么要它：worker 每轮循环都要"把所有连接 fd 拼起来 poll"。旧写法每轮
/// 分配两个 `Vec`（长度 = 连接数），50 条连接时每秒上千次分配。这里把两块
/// 缓冲挂在 worker 自己身上，生命周期跟 worker 一样长，稳态零分配。
#[cfg(unix)]
pub struct PollSet {
    items: Vec<PollFd>,
    ready: Vec<usize>,
    woken: bool,
    /// 唤醒器占了几个槽（0 或 1），决定就绪下标要不要减偏移
    waker_slots: usize,
}

#[cfg(unix)]
impl Default for PollSet {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(unix)]
impl PollSet {
    pub fn new() -> Self {
        Self {
            items: Vec::with_capacity(64),
            ready: Vec::with_capacity(64),
            woken: false,
            waker_slots: 0,
        }
    }

    /// 带唤醒器：`items[0]` 固定是它，连接从 `items[1]` 开始。
    ///
    /// `waker_fd` 为负表示没有唤醒器（只用来等连接）。
    pub fn with_waker(waker_fd: c_int) -> Self {
        let mut s = Self::new();
        if waker_fd >= 0 {
            s.items.push(PollFd {
                fd: waker_fd,
                events: POLLIN,
                revents: 0,
            });
            s.waker_slots = 1;
        }
        s
    }

    /// 原地重设连接 fd 列表（保留唤醒器槽）。容量够时零分配。
    pub fn reset_conns<I: Iterator<Item = c_int>>(&mut self, fds: I) {
        self.items.truncate(self.waker_slots);
        for fd in fds {
            self.items.push(PollFd {
                fd,
                events: POLLIN,
                revents: 0,
            });
        }
    }

    /// 等任意 fd 就绪。
    ///
    /// 返回后：`woken()` 表示唤醒器是否触发（调用方需自行 `Waker::drain`），
    /// `ready_len()`/`ready_at(i)` 给出就绪的**连接**下标（已去掉唤醒器偏移，
    /// 天然升序）。
    pub fn wait(&mut self, timeout_ms: i32) -> std::io::Result<()> {
        self.ready.clear();
        self.woken = false;
        if self.items.is_empty() {
            return Ok(());
        }
        for p in self.items.iter_mut() {
            p.revents = 0;
        }
        // SAFETY: `items` 与 `struct pollfd` 数组布局一致，长度如实传入。
        let n = unsafe { poll(self.items.as_mut_ptr(), self.items.len() as Nfds, timeout_ms) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            // EINTR：当超时处理，调用方下一轮会重新进来
            if e.raw_os_error() == Some(4) {
                return Ok(());
            }
            return Err(e);
        }
        if n == 0 {
            return Ok(());
        }
        for (i, p) in self.items.iter().enumerate() {
            if p.revents == 0 {
                continue;
            }
            if i < self.waker_slots {
                self.woken = true;
            } else {
                self.ready.push(i - self.waker_slots);
            }
        }
        Ok(())
    }

    pub fn woken(&self) -> bool {
        self.woken
    }

    pub fn ready_len(&self) -> usize {
        self.ready.len()
    }

    pub fn ready_at(&self, i: usize) -> usize {
        self.ready[i]
    }
}

#[cfg(not(unix))]
pub struct PollSet;

#[cfg(not(unix))]
impl Default for PollSet {
    fn default() -> Self {
        Self
    }
}

#[cfg(not(unix))]
impl PollSet {
    pub fn new() -> Self {
        Self
    }
    pub fn with_waker(_waker_fd: i32) -> Self {
        Self
    }
    pub fn reset_conns<I: Iterator<Item = i32>>(&mut self, _fds: I) {}
    pub fn wait(&mut self, timeout_ms: i32) -> std::io::Result<()> {
        std::thread::sleep(std::time::Duration::from_millis(timeout_ms.max(1) as u64));
        Ok(())
    }
    pub fn woken(&self) -> bool {
        false
    }
    pub fn ready_len(&self) -> usize {
        0
    }
    pub fn ready_at(&self, _i: usize) -> usize {
        0
    }
}

/// 非 unix：没有 `poll`，退化成短睡眠。功能正确，只是延迟回到毫秒级。
#[cfg(not(unix))]
pub fn wait_readable(_fd: i32, timeout_ms: i32) -> std::io::Result<bool> {
    std::thread::sleep(std::time::Duration::from_millis(timeout_ms.max(1) as u64));
    Ok(true)
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

    #[cfg(unix)]
    #[test]
    fn waker_breaks_wait_immediately() {
        let w = super::Waker::new().unwrap();
        let w2 = w.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            w2.notify();
        });
        let mut ps = super::PollSet::with_waker(w.read_fd());
        let t0 = std::time::Instant::now();
        // 兜底超时给 1000ms：若唤醒不生效，这里会等满 1s
        ps.wait(1000).unwrap();
        assert!(
            t0.elapsed() < std::time::Duration::from_millis(200),
            "唤醒没生效：{:?}",
            t0.elapsed()
        );
        assert!(ps.woken(), "应报告唤醒器已触发");

        // 不 drain 的话下一轮 wait 会立刻返回（计数还在）
        w.drain();
        let t1 = std::time::Instant::now();
        ps.wait(200).unwrap();
        assert!(
            t1.elapsed() >= std::time::Duration::from_millis(150),
            "drain 之后不该立刻返回：{:?}",
            t1.elapsed()
        );
        assert!(!ps.woken(), "drain 后不该再报唤醒");
    }

    #[cfg(unix)]
    #[test]
    fn pollset_reports_conn_index_without_waker_offset() {
        use std::os::unix::io::AsRawFd;
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let _c = std::net::TcpStream::connect(addr).unwrap();

        let w = super::Waker::new().unwrap();
        let mut ps = super::PollSet::with_waker(w.read_fd());
        // 下标 0 = 监听 fd（此刻有未 accept 的连接，必然可读）
        ps.reset_conns([l.as_raw_fd()].into_iter());
        ps.wait(200).unwrap();
        assert_eq!(ps.ready_len(), 1);
        assert_eq!(ps.ready_at(0), 0, "下标应已减掉唤醒器偏移");
        assert!(!ps.woken());
        assert!(l.accept().is_ok());
    }
}
