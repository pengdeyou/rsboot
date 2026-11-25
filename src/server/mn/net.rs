//! 协程式 socket I/O：等数据时让出 worker，而不是把线程焊在 `read` 上。
//!
//! 这是"慢下游不再拖垮整条 worker"的关键一步。之前 `Service::call` 用同步
//! `TcpStream` 读响应，一次 20ms 的下游就把 worker 卡死 20ms——`QPS` 直接被
//! 锁在 `worker 数 / 延迟`。
//!
//! 没有调度器时（单元测试、非 worker 线程）自动退化为阻塞调用，语义不变。

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use super::sched;
use crate::server::poll;

/// 诊断开关：设了 `RSBOOT_BLOCKING_IO` 就退回阻塞读。
///
/// 只用于基准里量"协程化到底值多少"，不该出现在生产配置里——
/// 它让一条慢下游重新焊死 worker。
fn blocking_io() -> bool {
    use std::sync::OnceLock;
    static F: OnceLock<bool> = OnceLock::new();
    *F.get_or_init(|| std::env::var_os("RSBOOT_BLOCKING_IO").is_some())
}

/// 读。协程里遇到 `WouldBlock` 就登记到 worker 的 `poll` 并让出。
pub fn read(s: &TcpStream, buf: &mut [u8]) -> io::Result<usize> {
    if !sched::ready() || blocking_io() {
        return (&*s).read(buf);
    }
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        s.set_nonblocking(true)?;
        let fd = s.as_raw_fd();
        loop {
            match (&*s).read(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // 先 0 超时探一次：已经就绪就别惊动调度器
                    if poll::wait_readable(fd, 0).unwrap_or(false) {
                        continue;
                    }
                    sched::wait_fd(fd);
                }
                other => return other,
            }
        }
    }
    #[cfg(not(unix))]
    {
        (&*s).read(buf)
    }
}

/// 写全部。写缓冲满时协作让出（写就绪几乎总是立刻发生，不值得为它再挂一个等待表）。
pub fn write_all(s: &TcpStream, buf: &[u8]) -> io::Result<()> {
    if !sched::ready() || blocking_io() {
        return (&*s).write_all(buf);
    }
    let mut rest = buf;
    s.set_nonblocking(true)?;
    while !rest.is_empty() {
        match (&*s).write(rest) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "连接已关闭，写不出去",
                ))
            }
            Ok(n) => rest = &rest[n..],
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => sched::yield_now(),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// 建连。**仍然阻塞**，但有 `timeout` 兜底。
///
/// 没有协程化：连接池命中后建连不频繁，而真正的非阻塞 connect 要自己
/// `socket()` 再处理 `EINPROGRESS`，还得把"连接失败"和"还在连"分开——
/// 留给需要的时候再做，不在这一步引入新的出错路径。
pub fn connect(addr: &std::net::SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
    TcpStream::connect_timeout(addr, timeout)
}

/// 给 `rsbatis` 用的 hook 版本（单次写，符合 `Write::write` 语义）。
pub fn write_some(s: &TcpStream, buf: &[u8]) -> io::Result<usize> {
    if !sched::ready() || blocking_io() {
        return (&*s).write(buf);
    }
    s.set_nonblocking(true)?;
    loop {
        match (&*s).write(buf) {
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => sched::yield_now(),
            Err(e) => return Err(e),
        }
    }
}

/// 把协程式读写装进 rsbatis 的 MySQL 驱动。
///
/// 在 `Server::run` 里装一次即可：钩子是进程级的，装完所有 `mysql+co://`
/// 连接都走协程。没装的时候驱动还是原来的阻塞行为——功能不受影响。
pub fn install_db_hooks() {
    rsbatis::set_mysql_socket_hooks(read, write_some);
}

/// `&TcpStream` 的 `Read` 适配器，让 `BufReader` 也能走上协程链路。
pub struct CoReader<'a>(pub &'a TcpStream);

impl Read for CoReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        read(self.0, buf)
    }
}
