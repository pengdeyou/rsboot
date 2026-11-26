//! 让出式的连接 I/O：inbound 连接从"阻塞 socket + 轮询"改成"非阻塞 + 挂起等待"。
//!
//! 这是高并发掉队的根因所在——旧模型下 `read_until` 会阻塞 worker 250 ms，
//! 写缓冲满时 `write` 会阻塞到 `write_timeout`，一条慢连接就能把整条 worker 按住。
//! 新模型下每个会阻塞的点都变成"登记到 worker 的 `poll` 并让出协程"，
//! worker 永远只在 `poll` 上等，一次事件可以推进多条连接。
//!
//! ## 超时怎么办
//!
//! 让出之后原来靠 `SO_RCVTIMEO` 的 250 ms 心跳就没了。这里把"请求读截止时间"
//! 放在线程局部，等待时一并交给调度器的定时器：到期协程被放回队列，
//! 醒来发现已过点就返回 `WouldBlock`，`read_head` 里原有的 `elapsed` 判定
//! 于是照常生效，slowloris 防御不打折。
//!
//! ## 为什么不是 busy-loop
//!
//! `mine::net::write_all` 那种"写就绪几乎立刻发生，遇 `EAGAIN` 就 `yield`"的做法
//! 在出站小包上够用，但对 inbound 不合适：客户端读得慢时会让协程空转，
//! 白烧 CPU。这里读写都真正挂到 `POLLIN`/`POLLOUT` 上。

use crate::server::{mn, poll};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::unix::io::AsRawFd;
use std::time::Duration;

/// 包一层 `TcpStream`，把每次读写都变成可能让出的点。
///
/// 放在 `BufReader` 里面用：`read_until` / `read_exact` 一个都不用改，
/// 它们内部调用的 `read` 就已经是让出式的了。
pub struct CoIo {
    inner: TcpStream,
}

impl CoIo {
    /// 设成非阻塞。注意 `O_NONBLOCK` 挂在打开的文件描述上，
    /// `try_clone` 出来的句柄共享它，所以这里设一次，写路径那边也是非阻塞。
    pub fn new(inner: TcpStream) -> io::Result<Self> {
        inner.set_nonblocking(true)?;
        Ok(Self { inner })
    }

    pub fn get_ref(&self) -> &TcpStream {
        &self.inner
    }

    pub fn into_inner(self) -> TcpStream {
        self.inner
    }
}

impl Read for CoIo {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let fd = self.inner.as_raw_fd();
        loop {
            match self.inner.read(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // 0 超时先探一次：数据已经在缓冲里就别惊动调度器
                    if poll::wait_readable(fd, 0).unwrap_or(false) {
                        continue;
                    }
                    if !mn::sched::ready() {
                        // 没有协程环境（单测、迁移脚本）：退回阻塞等待，
                        // 别在这里空转把 CPU 烧掉
                        let _ = poll::wait_readable(fd, 100);
                        continue;
                    }
                    // 每次挂起都重标一个心跳截止点：到期由定时器叫醒，
                    // 醒来过点了就交回 `read_head` 的 elapsed 判定
                    let dl = crate::server::clock::now_ns()
                        + IDLE_POLL.as_nanos() as u64;
                    mn::sched::wait_fd_until(fd, Some(dl));
                    if crate::server::clock::now_ns() >= dl {
                        mn::sched::cancel_wait(fd);
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "读请求超时",
                        ));
                    }
                }
                other => return other,
            }
        }
    }
}

impl Write for CoIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let fd = self.inner.as_raw_fd();
        let mut spins = 0u8;
        loop {
            match self.inner.write(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if !mn::sched::ready() {
                        let _ = poll::wait_writable(fd, 100);
                        continue;
                    }
                    if spins < 8 {
                        spins += 1;
                        mn::sched::yield_now();
                        continue;
                    }
                    spins = 0;
                    mn::sched::wait_fd_write(fd);
                }
                other => return other,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// 直接对 fd 写，`EAGAIN` 时挂到 `POLLOUT` 上让出。
///
/// 写响应走的是裸 `write`（省掉 `Write` 适配层的返回值检查），
/// 所以让出逻辑要在这里也有一份。
pub fn write_fd_all(fd: std::os::raw::c_int, mut buf: &[u8]) -> io::Result<()> {
    use std::os::raw::{c_int, c_void};
    unsafe extern "C" {
        fn write(fd: c_int, buf: *const c_void, n: usize) -> isize;
    }
    let mut spins = 0u8;
    while !buf.is_empty() {
        // SAFETY: fd 由调用方保证有效，buf 是有效切片
        let n = unsafe { write(fd, buf.as_ptr().cast::<c_void>(), buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                // EINTR：被信号打断，重试
                Some(4) => continue,
                // EAGAIN：发送缓冲满。写就绪通常立刻发生（对端在读），
                // 所以先让出协程再试，比"poll 探测 + 挂等待表"少一次系统调用；
                // 连着让 8 次还是满，说明对端真的慢，这时才挂到 POLLOUT 上睡。
                Some(11) | Some(35) => {
                    if !mn::sched::ready() {
                        let _ = poll::wait_writable(fd, 100);
                        continue;
                    }
                    if spins < 8 {
                        spins += 1;
                        mn::sched::yield_now();
                        continue;
                    }
                    spins = 0;
                    mn::sched::wait_fd_write(fd);
                    continue;
                }
                _ => return Err(e),
            }
        }
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "连接已关闭，写不出去",
            ));
        }
        buf = &buf[n as usize..];
    }
    Ok(())
}

/// 空闲等待用的兜底时长，与旧的 `POLL_INTERVAL` 对齐。
pub const IDLE_POLL: Duration = Duration::from_millis(250);
