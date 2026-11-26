//! 极简 HTTP/1.1 压测器（raw socket）。
//!
//! 刻意不用 rsboot 自己的 `Service` 来压：那会让被测栈同时承担客户端成本，
//! 测出来的是"客户端+服务端"的混合值。这里只依赖 `std`，对三个被测框架
//! 完全中立。
//!
//! 两条关键设计，都是被实测推出来的：
//!
//! 1. **每请求零分配**。旧版每请求都要 `try_clone()`（一次 dup 系统调用）、
//!    新建 `BufReader`（8 KB 分配）、`format!` 拼请求、每个响应头一次
//!    `to_ascii_lowercase`。客户端自己就能吃掉好几微秒，压出来的是客户端的
//!    上限，不是被测框架的上限。
//! 2. **用 TSC 取时**。这台 VM 上一次 `Instant::now()` 要 1.8 µs（vDSO 未生效），
//!    每请求两次就是 3.6 µs 的纯计时开销——足以把 QPS 砍掉三分之一。
//!    `rdtsc` 是十几个周期，且是单调计数。
//!
//! 用法：`load <host:port> <path> <线程数> <秒数> [--close]`
//! `--close` 关掉 keep-alive（每次请求新建连接），用来对比长连接的收益。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 每个 TSC tick 折合多少微秒的倒数（tick/µs）。启动时校准一次。
static TICKS_PER_US: AtomicU64 = AtomicU64::new(1);

#[inline(always)]
fn tsc() -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: rdtsc 无副作用，只是读时间戳计数器
        unsafe { core::arch::x86_64::_rdtsc() }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        0
    }
}

#[inline(always)]
fn since_us(mark: u64) -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        let now = tsc();
        (now.saturating_sub(mark)) / TICKS_PER_US.load(Ordering::Relaxed).max(1)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = mark;
        0
    }
}

/// 用一次 30 ms 睡眠校准 TSC 频率。`Instant` 很贵，但这里只调两次。
fn calibrate() {
    let t = Instant::now();
    let c0 = tsc();
    std::thread::sleep(Duration::from_millis(30));
    let c1 = tsc();
    let us = t.elapsed().as_micros() as u64;
    if c1 > c0 && us > 0 {
        TICKS_PER_US.store((c1 - c0) / us, Ordering::Relaxed);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 5 {
        eprintln!("用法：load <host:port> <path> <线程数> <秒数> [--close]");
        std::process::exit(2);
    }
    let addr = args[1].clone();
    let path = args[2].clone();
    let threads: usize = args[3].parse().expect("线程数");
    let secs: u64 = args[4].parse().expect("秒数");
    let keep_alive = !args.iter().any(|a| a == "--close");

    calibrate();

    let running = Arc::new(AtomicBool::new(true));
    let errors = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();
    for _ in 0..threads {
        let addr = addr.clone();
        let path = path.clone();
        let running = Arc::clone(&running);
        let errors = Arc::clone(&errors);
        handles.push(std::thread::spawn(move || {
            let mut lat: Vec<u64> = Vec::with_capacity(200_000);
            let mut conn = Conn::new(&addr, &path, keep_alive);
            while running.load(Ordering::Relaxed) {
                match conn.request() {
                    Ok(us) => lat.push(us),
                    Err(_) => {
                        errors.fetch_add(1, Ordering::Relaxed);
                        // 连接坏了就重连，别让一个错误卡死整条线程
                        conn = Conn::new(&addr, &path, keep_alive);
                    }
                }
            }
            lat
        }));
    }

    let start = Instant::now();
    std::thread::sleep(Duration::from_secs(secs));
    running.store(false, Ordering::Relaxed);

    let mut all: Vec<u64> = Vec::new();
    for h in handles {
        all.extend(h.join().unwrap());
    }
    let elapsed = start.elapsed().as_secs_f64();
    let errs = errors.load(Ordering::Relaxed);

    // 越界那次请求会多算进耗时里，按实际完成数算 QPS 更诚实
    let n = all.len() as f64;
    if all.is_empty() {
        println!("没有成功的请求（errors={errs}）");
        std::process::exit(1);
    }
    all.sort_unstable();
    let q = |p: f64| -> f64 {
        let i = ((all.len() as f64 - 1.0) * p).round() as usize;
        all[i] as f64 / 1000.0
    };
    let avg = all.iter().map(|v| *v as f64).sum::<f64>() / n / 1000.0;

    println!("线程 {threads}  时长 {elapsed:.2}s  完成 {n:.0}");
    println!("QPS      {:.0}", n / elapsed);
    println!(
        "延迟(ms)  平均 {avg:.3}   P50 {:.3}   P90 {:.3}   P99 {:.3}   P999 {:.3}   max {:.3}",
        q(0.50),
        q(0.90),
        q(0.99),
        q(0.999),
        all[all.len() - 1] as f64 / 1000.0
    );
    if errs > 0 {
        println!("错误     {errs}");
    }
}

struct Conn {
    reader: BufReader<TcpStream>,
    addr: String,
    keep_alive: bool,
    /// 请求字节只构造一次
    req: Vec<u8>,
    /// 逐行读取的复用缓冲
    line: Vec<u8>,
    /// 响应体的复用缓冲
    body: Vec<u8>,
}

impl Conn {
    fn new(addr: &str, path: &str, keep_alive: bool) -> Self {
        let mut req = Vec::with_capacity(96 + path.len());
        req.extend_from_slice(b"GET ");
        req.extend_from_slice(path.as_bytes());
        req.extend_from_slice(b" HTTP/1.1\r\nhost: bench\r\n");
        req.extend_from_slice(if keep_alive {
            b"connection: keep-alive\r\n"
        } else {
            b"connection: close\r\n"
        });
        req.extend_from_slice(b"\r\n");

        let stream = TcpStream::connect(addr).expect("无法连接被测服务");
        stream.set_nodelay(true).ok();
        Self {
            reader: BufReader::with_capacity(16 * 1024, stream),
            addr: addr.to_string(),
            keep_alive,
            req,
            line: Vec::with_capacity(256),
            body: Vec::with_capacity(4096),
        }
    }

    /// 返回本次请求的耗时（微秒）。全程零分配。
    fn request(&mut self) -> Result<u64, ()> {
        let m = tsc();
        self.reader
            .get_mut()
            .write_all(&self.req)
            .map_err(|_| ())?;

        // 状态行
        self.line.clear();
        self.reader
            .read_until(b'\n', &mut self.line)
            .map_err(|_| ())?;
        if !self.line.starts_with(b"HTTP/1.1 200") && !self.line.starts_with(b"HTTP/1.0 200") {
            return Err(());
        }

        // 头：逐行比对前缀，不做小写转换、不分配
        let mut len = 0usize;
        loop {
            self.line.clear();
            let n = self
                .reader
                .read_until(b'\n', &mut self.line)
                .map_err(|_| ())?;
            if n == 0 {
                return Err(());
            }
            if self.line.as_slice() == b"\r\n" {
                break;
            }
            if let Some(v) = strip_prefix_ci(&self.line, b"content-length:") {
                len = parse_dec(v);
            }
        }

        if len > 0 {
            if self.body.len() < len {
                self.body.resize(len, 0);
            }
            self.reader
                .read_exact(&mut self.body[..len])
                .map_err(|_| ())?;
        }

        if !self.keep_alive {
            self.reconnect()?;
        }

        Ok(since_us(m))
    }

    fn reconnect(&mut self) -> Result<(), ()> {
        let s = TcpStream::connect(&self.addr).map_err(|_| ())?;
        s.set_nodelay(true).ok();
        self.reader = BufReader::with_capacity(16 * 1024, s);
        Ok(())
    }
}

/// 忽略大小写的前缀匹配，返回剩余部分（末尾的 `\r\n` 留给 `parse_dec` 去 trim）。
fn strip_prefix_ci<'a>(hay: &'a [u8], pre: &[u8]) -> Option<&'a [u8]> {
    if hay.len() < pre.len() {
        return None;
    }
    let head = &hay[..pre.len()];
    if head.eq_ignore_ascii_case(pre) {
        Some(&hay[pre.len()..])
    } else {
        None
    }
}

/// 解析十进制数字，跳过空白与末尾 CRLF。
fn parse_dec(mut b: &[u8]) -> usize {
    while let [first, rest @ ..] = b {
        if first.is_ascii_digit() {
            break;
        }
        b = rest;
    }
    let mut n = 0usize;
    for &c in b {
        if !c.is_ascii_digit() {
            break;
        }
        n = n.saturating_mul(10).saturating_add((c - b'0') as usize);
    }
    n
}
