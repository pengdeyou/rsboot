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
const POLLOUT: c_short = 0x0004;

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

/// 等 `fd` 可写最多 `timeout_ms` 毫秒。写缓冲满（`EAGAIN`）时用它。
#[cfg(unix)]
pub fn wait_writable(fd: c_int, timeout_ms: i32) -> std::io::Result<bool> {
    let mut p = PollFd {
        fd,
        events: POLLOUT,
        revents: 0,
    };
    // SAFETY: 同 `wait_readable`，只读写这一块栈内存
    let n = unsafe { poll(&mut p, 1, timeout_ms) };
    if n < 0 {
        let e = std::io::Error::last_os_error();
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

/// Linux 的 `epoll` 后端（macOS 没有 epoll，继续走 `poll` 路径）。
///
/// ## 为什么值得写
///
/// `poll(2)` 每轮都要把 `nfds` 个 fd 整个交给内核重扫一次（每个 fd 至少一次
/// 虚调用）。1 000 条连接就是每轮上千次，摊到每请求上是固定成本。新压测器
/// （单线程、非阻塞、1 000 条 keep-alive 连接）量出来的账：rsboot 每请求
/// `sys` 9.8 µs，axum（epoll）2.8 µs，而两边 `user` 几乎相同（2.4 对 2.6 µs）
/// ——差的 7 µs 有相当一部分就是这趟重扫。旁证：QPS 对连接数不敏感（200 与
/// 1 000 连接同为 139 k），所以它是**每请求固定成本**，不是 O(n) 的抖动。
///
/// ## 代价控制
///
/// 不为注册付 O(n) 的 `epoll_ctl`。`reg` 位图记着"上一轮已经注册过哪些
/// fd"，只有集合真的变了才发系统调用；稳态（1 000 条长连接）每轮零 `ctl`。
/// 每轮固定开销是清零 + 置位两张位图（约 300 ns），比一次 `epoll_wait` 便宜。
///
/// ## fd 复用这个坑
///
/// 连接关掉后 fd 号会被新连接拿走。内核在 `close` 时已把它踢出 epoll 集合，
/// 我们的位图却还以为它在——新连接于是永远收不到通知（只能靠兜底超时醒来，
/// 表现为偶发 0.5 s 卡顿）。所以每 `REBUILD_EVERY` 轮做一次全量重建（DEL
/// 全部 + ADD 全部）自愈，250 个 fd 的重建约 100 µs，摊到 64 轮可忽略。
#[cfg(all(unix, not(target_os = "macos")))]
mod ep {
    use std::os::raw::c_int;

    pub const BITS: usize = 8192;
    pub const WORDS: usize = BITS / 64;
    /// 少于这么多项时 `poll` 更划算（没有可省的扫描，少一次 `epoll` 状态维护）
    pub const MIN_ITEMS: usize = 8;
    pub const MAX_EVENTS: usize = 256;
    pub const REBUILD_EVERY: u32 = 1024;

    const OP_ADD: c_int = 1;
    const OP_DEL: c_int = 2;
    const OP_MOD: c_int = 3;
    const EPOLL_CLOEXEC: c_int = 0o2000000;

    pub const IN: u8 = 0x01;
    pub const OUT: u8 = 0x04;

    /// 与 glibc 一致：`events: u32` + `data: u64`，**紧凑 12 字节**
    #[repr(C, packed)]
    #[derive(Clone, Copy)]
    pub struct Event {
        pub events: u32,
        pub data: u64,
    }

    impl Default for Event {
        fn default() -> Self {
            Self { events: 0, data: 0 }
        }
    }

    unsafe extern "C" {
        fn epoll_create1(flags: c_int) -> c_int;
        fn epoll_ctl(epfd: c_int, op: c_int, fd: c_int, ev: *mut Event) -> c_int;
        pub fn epoll_wait(
            epfd: c_int,
            evs: *mut Event,
            maxevents: c_int,
            timeout: c_int,
        ) -> c_int;
    }

    pub struct Session {
        pub epfd: c_int,
        /// 已注册位图（与 `evs` 同步维护）
        reg: [u64; WORDS],
        /// 本轮出现的 fd 位图（复用，不每轮分配）
        cur: [u64; WORDS],
        /// 每个 fd 已注册的事件掩码，0 = 未注册
        evs: [u8; BITS],
        epoch: u32,
        pub evbuf: [Event; MAX_EVENTS],
    }

    impl Drop for Session {
        fn drop(&mut self) {
            if self.epfd >= 0 {
                // SAFETY: 这个 fd 是本结构独占创建的
                unsafe {
                    super::close(self.epfd);
                }
            }
        }
    }

    impl Session {
        pub fn new() -> std::io::Result<Self> {
            // SAFETY: 系统调用无参数副作用
            let fd = unsafe { epoll_create1(EPOLL_CLOEXEC) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self {
                epfd: fd,
                reg: [0; WORDS],
                cur: [0; WORDS],
                evs: [0; BITS],
                epoch: 0,
                evbuf: [Event::default(); MAX_EVENTS],
            })
        }

        fn ctl(&self, op: c_int, fd: c_int, mask: u8) -> bool {
            let mut ev = Event {
                events: mask as u32,
                data: fd as u64,
            };
            // `Event` 是 packed：不能对它取普通引用，只能取裸指针
            let p = std::ptr::addr_of_mut!(ev);
            // SAFETY: 指针指向刚构造的栈上事件，大小与布局与内核约定一致
            unsafe { epoll_ctl(self.epfd, op, fd, p) == 0 }
        }

        /// 把当前 `items` 与已注册集合对账，只对差异发系统调用。
        pub fn sync(&mut self, items: &[super::PollFd]) {
            self.epoch = self.epoch.wrapping_add(1);
            self.cur = [0; WORDS];

            if self.epoch % REBUILD_EVERY == 0 {
                // 全量重建：自愈 fd 复用留下的"假注册"
                for w in 0..WORDS {
                    let mut bits = self.reg[w];
                    while bits != 0 {
                        let b = bits.trailing_zeros() as usize;
                        bits &= bits - 1;
                        let fd = (w * 64 + b) as c_int;
                        self.ctl(OP_DEL, fd, 0);
                    }
                    self.reg[w] = 0;
                }
                self.evs = [0; BITS];
            }

            for p in items {
                let fd = p.fd as usize;
                if fd >= BITS {
                    continue;
                }
                let bit = 1u64 << (fd % 64);
                self.cur[fd / 64] |= bit;
                let mask = events_mask(p.events);
                let have = self.evs[fd];
                if have == 0 {
                    if self.ctl(OP_ADD, fd as c_int, mask) {
                        self.reg[fd / 64] |= bit;
                        self.evs[fd] = mask;
                    }
                } else if have != mask && self.ctl(OP_MOD, fd as c_int, mask) {
                    self.evs[fd] = mask;
                }
            }

            for w in 0..WORDS {
                let mut gone = self.reg[w] & !self.cur[w];
                while gone != 0 {
                    let b = gone.trailing_zeros() as usize;
                    gone &= gone - 1;
                    let idx = w * 64 + b;
                    self.ctl(OP_DEL, idx as c_int, 0);
                    self.evs[idx] = 0;
                    self.reg[w] &= !(1u64 << b);
                }
            }
        }
    }

    pub fn events_mask(ev: std::os::raw::c_short) -> u8 {
        let mut m = 0u8;
        if ev & super::POLLIN != 0 {
            m |= IN;
        }
        if ev & super::POLLOUT != 0 {
            m |= OUT;
        }
        m
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn no_epoll() -> bool {
    use std::sync::OnceLock;
    static F: OnceLock<bool> = OnceLock::new();
    *F.get_or_init(|| std::env::var_os("RSBOOT_NO_EPOLL").is_some())
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
    /// Linux 上的 epoll 后端；项数够多且有变化才存在，否则走 `poll`
    #[cfg(all(unix, not(target_os = "macos")))]
    ep: Option<ep::Session>,
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
            #[cfg(all(unix, not(target_os = "macos")))]
            ep: None,
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
        self.reset_conns_with(fds, std::iter::empty());
    }

    /// 连接统一等可读，等待表里的 fd 可以指定等可读还是等可写。
    ///
    /// 分界点仍是 `n_conns`：前一段是连接，后一段是协程在等的 fd。
    pub fn reset_conns_with<I, J>(&mut self, conns: I, waits: J)
    where
        I: Iterator<Item = c_int>,
        J: Iterator<Item = (c_int, bool)>,
    {
        self.items.truncate(self.waker_slots);
        for fd in conns {
            self.items.push(PollFd {
                fd,
                events: POLLIN,
                revents: 0,
            });
        }
        for (fd, want_write) in waits {
            self.items.push(PollFd {
                fd,
                events: if want_write { POLLOUT } else { POLLIN },
                revents: 0,
            });
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        self.sync_backend();
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
        #[cfg(all(unix, not(target_os = "macos")))]
        if self.ep.is_some() {
            return self.wait_ep(timeout_ms);
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

    /// 决定这一轮走 `epoll` 还是 `poll`，并把 `epoll` 的注册集合与 `items` 对账。
    ///
    /// 退出条件（退回 `poll`）：项数太少，或某个 fd 超出位图范围（极端高 fd）。
    #[cfg(all(unix, not(target_os = "macos")))]
    fn sync_backend(&mut self) {
        // 逃生阀 + A/B 开关：`RSBOOT_NO_EPOLL=1` 强制走 `poll`（macOS 路径）
        if no_epoll() {
            self.ep = None;
            return;
        }
        let use_ep = self.items.len() >= ep::MIN_ITEMS
            && self
                .items
                .iter()
                .all(|p| p.fd >= 0 && (p.fd as usize) < ep::BITS);
        if !use_ep {
            self.ep = None; // Drop 里 close
            return;
        }
        if self.ep.is_none() {
            match ep::Session::new() {
                Ok(s) => self.ep = Some(s),
                // 建不出来（EMFILE/ENOMEM）不是错误：这轮及以后走 `poll` 就是了
                Err(_) => return,
            }
        }
        if let Some(s) = self.ep.as_mut() {
            s.sync(&self.items);
        }
    }

    /// `epoll_wait` 分支：只收内核报回来的就绪 fd，不再全量重扫。
    ///
    /// 内核报的是 fd 号（`data`），这里换回 `items` 下标。就绪数通常只有几个
    /// （本轮 2–3 个），线性查找的代价是一次就绪 × 几百字节比较，可忽略；
    /// 好处是下标与 `items` 永远同步，不怕连接在轮内被 `swap_remove` 打乱。
    #[cfg(all(unix, not(target_os = "macos")))]
    fn wait_ep(&mut self, timeout_ms: i32) -> std::io::Result<()> {
        let items_addr = self.items.as_ptr();
        let items_len = self.items.len();
        let sess = self.ep.as_mut().expect("wait_ep 只在有 ep 时调用");
        // SAFETY: 缓冲区属于本结构，长度按常量传入
        let n = unsafe {
            ep::epoll_wait(
                sess.epfd,
                sess.evbuf.as_mut_ptr(),
                ep::MAX_EVENTS as c_int,
                timeout_ms,
            )
        };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(4) {
                return Ok(());
            }
            return Err(e);
        }
        for k in 0..n as usize {
            // SAFETY: `Event` 是 packed，只能按裸指针读字段
            let d = unsafe { std::ptr::addr_of!(sess.evbuf[k].data).read_unaligned() };
            let fd = d as c_int;
            let mut hit = usize::MAX;
            for i in 0..items_len {
                // SAFETY: i < items_len，指针有效且未被写
                if unsafe { (*items_addr.add(i)).fd } == fd {
                    hit = i;
                    break;
                }
            }
            if hit == usize::MAX {
                continue;
            }
            if hit < self.waker_slots {
                self.woken = true;
            } else {
                self.ready.push(hit - self.waker_slots);
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
    pub fn reset_conns_with<I, J>(&mut self, _c: I, _w: J)
    where
        I: Iterator<Item = i32>,
        J: Iterator<Item = (i32, bool)>,
    {
    }
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
