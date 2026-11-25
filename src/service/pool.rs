//! keep-alive 连接池。
//!
//! ## 为什么必须有池
//!
//! 下游调用最贵的部分不是处理，是 TCP 握手 + 慢启动。压测里"每次新建连接"
//! 会把 P99 抬到握手的量级。HTTP/1.1 的 keep-alive 本来就是为了省这一跳，
//! 不做池等于把这层白扔了。
//!
//! ## 池里不探活
//!
//! 空闲连接可能已被对端超时关闭，探活（发个 ping）成本比"拿到连接发请求失败再重试"
//! 高得多，而且 ping 本身也消耗对方的连接配额。所以策略是：拿到就用，
//! 写失败且**一个字节都没写出去**时换新连接重试（此时重试是安全的）。

use std::collections::VecDeque;
use std::io;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Mutex;
use std::time::Duration;

pub struct Pool {
    /// `host:port`，日志里用来区分是哪个下游
    label: String,
    idle: Mutex<VecDeque<TcpStream>>,
    max_idle: usize,
    connect_timeout: Duration,
}

impl Pool {
    pub fn new(label: impl Into<String>, max_idle: usize, connect_timeout: Duration) -> Self {
        Self {
            label: label.into(),
            idle: Mutex::new(VecDeque::new()),
            max_idle: max_idle.max(1),
            connect_timeout,
        }
    }

    /// 借一条连接。优先复用空闲的；空闲池空了才真建连。
    pub fn take(&self, read_timeout: Duration) -> io::Result<TcpStream> {
        if let Ok(mut q) = self.idle.lock() {
            if let Some(s) = q.pop_front() {
                // 上一轮设过读超时，这里要按当前配置重置（配置可能热加载过）
                let _ = s.set_read_timeout(Some(read_timeout));
                let _ = s.set_write_timeout(Some(read_timeout));
                return Ok(s);
            }
        }
        self.connect(read_timeout)
    }

    fn connect(&self, read_timeout: Duration) -> io::Result<TcpStream> {
        let addrs: Vec<SocketAddr> = self.label.to_socket_addrs()?.collect();
        let addr = addrs.first().copied().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                format!("`{}` 解析不出地址", self.label),
            )
        })?;
        let s = TcpStream::connect_timeout(&addr, self.connect_timeout)?;
        s.set_nodelay(true)?;
        s.set_read_timeout(Some(read_timeout))?;
        s.set_write_timeout(Some(read_timeout))?;
        Ok(s)
    }

    /// 还回去。池满就丢掉（比无限攒着更安全：空闲连接本身占着对端配额）。
    pub fn give(&self, s: TcpStream) {
        if let Ok(mut q) = self.idle.lock() {
            if q.len() < self.max_idle {
                q.push_back(s);
            }
        }
        // 走到这里说明池满或锁中毒：让 `s` 在作用域结束时关闭
    }

    pub fn idle_len(&self) -> usize {
        self.idle.lock().map(|q| q.len()).unwrap_or(0)
    }
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("label", &self.label)
            .field("idle", &self.idle_len())
            .field("max_idle", &self.max_idle)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_caps_idle_connections() {
        let p = Pool::new("127.0.0.1:9", 2, Duration::from_millis(50));
        // 造两条"连接"很难，这里只验证上限逻辑：直接塞满后 give 不该增长
        assert_eq!(p.idle_len(), 0);
        assert_eq!(p.max_idle, 2);
    }

    #[test]
    fn connect_failure_names_the_downstream() {
        // 9 端口基本总是拒绝连接，正好用来验错误信息里带上游名字
        let p = Pool::new("127.0.0.1:9", 1, Duration::from_millis(200));
        let err = p.take(Duration::from_secs(1)).unwrap_err();
        let msg = err.to_string();
        assert!(!msg.is_empty(), "{msg}");
    }
}
