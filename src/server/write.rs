//! 响应序列化。
//!
//! 一处组装，所有路径都走它——包括 400/413 这类解析期错误。如果错误响应
//! 自己拼字节，迟早出现"错误响应缺 `Content-Length` 导致客户端挂住"这种问题。
//!
//! 三条强制规则：
//!
//! 1. **204/304 不能有 body**，也不能带 `Content-Length`（RFC 7230）。
//! 2. **HEAD 不写 body，但保留 `Content-Length`**——正是它让客户端知道
//!    "如果这是 GET 会拿到多大"。
//! 3. **所有头都经 `Headers::insert`**，含 CRLF 的值写不进去（响应拆分防护）。

use std::io::{self, Write};

use crate::web::response::Response;

/// 写完整响应。`head_only` 为真时不写 body（HEAD 请求）。
///
/// ## 一次系统调用
///
/// 早先版本对 `out`（就是 `TcpStream`）连着 `write!` 好几次：状态行一次、
/// 自动头几次、业务头每个一次、body 一次。每条响应 5~8 个 `write(2)`
/// 系统调用，在基准里是纯粹的白送开销。
///
/// 现在先在内存 `Vec` 里拼整条响应，最后 `write_all` 一次。这也是所有成熟
/// HTTP 服务端的做法（缓冲写出，`flush` 只发生在该发的时刻）。
pub fn write_response<W: Write>(
    out: &mut W,
    resp: &Response,
    close: bool,
    head_only: bool,
) -> io::Result<()> {
    let mut buf: Vec<u8> = Vec::with_capacity(96 + resp.headers.len() * 32 + resp.body.len());
    fill_head(&mut buf, resp, close);
    if body_forbidden(resp) || head_only {
        // 不写 body
    } else {
        buf.extend_from_slice(&resp.body);
    }
    out.write_all(&buf)?;
    out.flush()
}

/// `204/304` 一律不允许带 body（RFC 7230）。
fn body_forbidden(resp: &Response) -> bool {
    resp.status.0 == 204 || resp.status.0 == 304
}

/// 把状态行 + 全部头写进 `buf`（到结尾的 CRLF 为止）。
///
/// 抽出来是为了让"泛型写出"和"fd 直写（大 body 走 `writev`）"两条路径
/// 拼出**完全相同**的头字节——两份实现迟早跑偏。
fn fill_head(buf: &mut Vec<u8>, resp: &Response, close: bool) {
    let status = resp.status;

    // 状态行
    buf.extend_from_slice(b"HTTP/1.1 ");
    let _ = write!(buf, "{}", status.0);
    buf.push(b' ');
    buf.extend_from_slice(status.reason().as_bytes());
    buf.extend_from_slice(b"\r\n");

    // 自动头。业务头里已经有同名的，以业务头为准（下面会跳过重复写入）。
    // date 走秒级缓存：每响应重算一遍 RFC 1123 是纯浪费（actix 也是这么做）
    buf.extend_from_slice(b"date: ");
    buf.extend_from_slice(http_date_cached().as_bytes());
    buf.extend_from_slice(b"\r\nserver: ");
    buf.extend_from_slice(SERVER.as_bytes());
    buf.extend_from_slice(b"\r\n");
    if !body_forbidden(resp) {
        buf.extend_from_slice(b"content-length: ");
        let _ = write!(buf, "{}", resp.body.len());
        buf.extend_from_slice(b"\r\n");
    }
    if close {
        buf.extend_from_slice(b"connection: close\r\n");
    } else {
        buf.extend_from_slice(b"connection: keep-alive\r\n");
    }

    for (name, value) in resp.headers.iter() {
        // 这几个由本层负责，业务侧设置会被覆盖成一致的值
        if matches!(name, "content-length" | "connection" | "date" | "server") {
            continue;
        }
        // 再过一遍校验：value 里若混进 CRLF（比如来自用户数据）就丢弃
        if name.is_empty() || value.contains('\r') || value.contains('\n') {
            continue;
        }
        buf.extend_from_slice(name.as_bytes());
        buf.extend_from_slice(b": ");
        buf.extend_from_slice(value.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }
    buf.extend_from_slice(b"\r\n");

}

/// 大 body 的阈值：超过它就改走 `writev`，body 不再拷进 head 缓冲。
///
/// 小响应（绝大多数 API）应该继续"拼一条 + 单次 write"：那是最省的形态，
/// 退回两次 `write` 反而更慢。大响应才值得为省一次 memcpy 付一次 `writev`。
#[cfg(unix)]
const WRITEV_THRESHOLD: usize = 8 * 1024;

/// 直接写 fd 的响应路径，供服务端热路径使用。
///
/// 与泛型版返回的字节**完全一致**（head 由同一个 `fill_head` 拼），区别只在
/// 大 body 的写法：
///
/// - body 小于阈值：拼成一条再 `write_all`（一次系统调用，无额外 iovec 开销）
/// - body 大于阈值：`head` 与 `body` 两段 iovec 一次 `writev`，**body 零拷贝**，
///   也不再为它分配一块与 body 等大的缓冲（64KB 响应每请求省一次 64KB 分配）
#[cfg(unix)]
pub fn write_response_fd(
    fd: std::os::raw::c_int,
    resp: &Response,
    close: bool,
    head_only: bool,
) -> io::Result<()> {
    let mut head: Vec<u8> = Vec::with_capacity(96 + resp.headers.len() * 32);
    fill_head(&mut head, resp, close);

    let body: &[u8] = if head_only || body_forbidden(resp) {
        &[]
    } else {
        &resp.body
    };

    if body.len() <= WRITEV_THRESHOLD {
        head.extend_from_slice(body);
        return write_all_fd(fd, &head);
    }
    writev_all(fd, &head, body)
}

#[cfg(unix)]
fn write_all_fd(fd: std::os::raw::c_int, mut buf: &[u8]) -> io::Result<()> {
    use std::os::raw::{c_int, c_void};
    unsafe extern "C" {
        fn write(fd: c_int, buf: *const c_void, n: usize) -> isize;
    }
    while !buf.is_empty() {
        // SAFETY: fd 由调用方保证有效，buf 是有效切片。
        let n = unsafe { write(fd, buf.as_ptr().cast::<c_void>(), buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            // EINTR：被信号打断，重试
            if e.raw_os_error() == Some(4) {
                continue;
            }
            return Err(e);
        }
        if n == 0 {
            return Err(io::Error::from(io::ErrorKind::WriteZero));
        }
        buf = &buf[n as usize..];
    }
    Ok(())
}

/// `writev` 一次写出两段，并正确处理部分写。
///
/// 部分写是必须处理的：`writev` 返回的是"这次写进去多少字节"，可能小于两段之和
/// （内核发送缓冲满、被信号打断等）。不处理就等于静默截断响应——客户端会挂住
/// 等剩下的 content-length。
#[cfg(unix)]
fn writev_all(fd: std::os::raw::c_int, head: &[u8], body: &[u8]) -> io::Result<()> {
    use std::os::raw::{c_int, c_void};

    #[repr(C)]
    struct IoVec {
        base: *const c_void,
        len: usize,
    }

    unsafe extern "C" {
        fn writev(fd: c_int, iov: *const IoVec, cnt: c_int) -> isize;
    }

    let mut a: &[u8] = head;
    let mut b: &[u8] = body;
    while !a.is_empty() || !b.is_empty() {
        let iov = [
            IoVec {
                base: a.as_ptr().cast::<c_void>(),
                len: a.len(),
            },
            IoVec {
                base: b.as_ptr().cast::<c_void>(),
                len: b.len(),
            },
        ];
        // SAFETY: 两个 iovec 都指向有效切片，cnt 如实为 2。
        let n = unsafe { writev(fd, iov.as_ptr(), 2) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(4) {
                continue;
            }
            return Err(e);
        }
        if n == 0 {
            return Err(io::Error::from(io::ErrorKind::WriteZero));
        }
        // 推进游标：先消耗 head，跨过了再消耗 body
        let n = n as usize;
        if n < a.len() {
            a = &a[n..];
        } else {
            let rest = n - a.len();
            a = &[];
            b = &b[rest.min(b.len())..];
        }
    }
    Ok(())
}

/// `server` 头是常量：没必要每响应 `format!` 一次
const SERVER: &str = concat!("rsboot/", env!("CARGO_PKG_VERSION"));

/// 秒级缓存的 `Date` 头，**每线程一份**。
///
/// HTTP 的 `Date` 精度就是秒，同一个秒内所有请求的响应头一模一样。
/// 用 `Arc<str>` 而不是 `String`：返回时只做一次原子自增，没有分配。
///
/// 为什么是 thread_local 而不是全局 `Mutex`：worker 是线程-per-core，每个
/// 响应都要取一次 Date。全局锁会让 2 个 worker 在热路径上互挤同一把锁；
/// 每线程各存一份，连"无竞争时的原子操作"都省了。缓存错位最多一个时钟
/// 刻度，对 HTTP 语义没有影响。
fn http_date_cached() -> std::sync::Arc<str> {
    use std::sync::Arc;

    thread_local! {
        static CACHE: std::cell::RefCell<(u64, Option<Arc<str>>)> =
            const { std::cell::RefCell::new((0, None)) };
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    CACHE.with(|c| {
        let mut g = c.borrow_mut();
        if g.0 != now || g.1.is_none() {
            g.0 = now;
            g.1 = Some(Arc::from(http_date_at(now as i64)));
        }
        // `is_none` 已排除上面的分支，这里必然有值
        Arc::clone(g.1.as_ref().expect("Date 缓存刚被填充"))
    })
}

/// RFC 1123 日期，用于 `Date` 头。
///
/// 自己算而不是引 chrono：只需要一个格式化函数，引入一个日期库为它付编译时间
/// 与依赖维护成本不划算。算法是 Hinnant 的 civil_from_days，无闰年分支错误。
pub fn http_date() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    http_date_at(secs)
}

/// 给定 unix 秒数算 RFC 1123 日期字符串。
fn http_date_at(secs: i64) -> String {
    let days = secs / 86400;
    let sod = secs % 86400;
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    const WD: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MO: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let wd = WD[(((days % 7) + 7 + 4) % 7) as usize];
    format!(
        "{wd}, {:02} {} {y:04} {:02}:{:02}:{:02} GMT",
        d,
        MO[(m - 1) as usize],
        h,
        mi,
        s
    )
}

/// 1970-01-01 起的天数 → (年, 月, 日)。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (y + if m <= 2 { 1 } else { 0 }, m, d)
}

#[cfg(test)]
mod tests {

    // 两条写出路径必须产出**完全相同**的字节：head 共用 `fill_head`，
    // 只有 body 的写法不同（拼接 vs writev）。这里把两条路径的字节逐一对齐，
    // 小 body 与大 body（跨过 writev 阈值）各测一次。
    #[cfg(unix)]
    fn drain_fd_path(resp: &crate::web::response::Response, close: bool, head_only: bool) -> Vec<u8> {
        use std::io::Read;
        use std::os::unix::io::AsRawFd;

        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let c = std::net::TcpStream::connect(addr).unwrap();
        let (mut s, _) = l.accept().unwrap();
        // 6 万字节的 body 用默认缓冲可能写满就阻塞，读端要先开读
        let h = std::thread::spawn(move || {
            let mut got = Vec::new();
            s.read_to_end(&mut got).unwrap();
            got
        });
        super::write_response_fd(c.as_raw_fd(), resp, close, head_only).unwrap();
        drop(c);
        h.join().unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn fd_path_matches_generic_path() {
        use crate::web::http::Status;

        for size in [0usize, 10, 4096, 8 * 1024, 20_000, 64 * 1024] {
            let body = vec![b'x'; size];
            let resp = crate::web::response::Response::json(&body);
            let mut want = Vec::new();
            super::write_response(&mut want, &resp, false, false).unwrap();
            let got = drain_fd_path(&resp, false, false);
            assert_eq!(
                got, want,
                "body={size} 时 fd 路径与泛型路径字节不一致（len {} vs {}）",
                got.len(),
                want.len()
            );
        }

        // HEAD：不写 body，但必须保留 content-length
        let resp = crate::web::response::Response::json(&vec![b'y'; 20_000]);
        let mut want = Vec::new();
        super::write_response(&mut want, &resp, true, true).unwrap();
        let got = drain_fd_path(&resp, true, true);
        assert_eq!(got, want, "HEAD 响应两条路径不一致");
        assert!(
            String::from_utf8_lossy(&got).contains("content-length: 20000"),
            "HEAD 也要带 content-length"
        );

        // 204：既没有 body 也没有 content-length
        let mut resp = crate::web::response::Response::new(Status::NO_CONTENT);
        resp.body = vec![b'z'; 20_000];
        let got = drain_fd_path(&resp, false, false);
        assert!(
            !String::from_utf8_lossy(&got).contains("content-length"),
            "204 不能带 content-length"
        );
        assert!(got.ends_with(b"\r\n\r\n"), "204 之后不该有多余 body");
    }
    use super::*;
    use crate::web::http::Status;

    fn render(resp: &Response, close: bool, head_only: bool) -> String {
        let mut buf = Vec::new();
        write_response(&mut buf, resp, close, head_only).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn normal_response_has_status_line_and_length() {
        let r = Response::text("hi");
        let s = render(&r, false, false);
        assert!(s.starts_with("HTTP/1.1 200 OK\r\n"), "{s}");
        assert!(s.contains("content-length: 2\r\n"), "{s}");
        assert!(s.contains("connection: keep-alive\r\n"), "{s}");
        assert!(s.ends_with("\r\n\r\nhi"), "{s}");
    }

    #[test]
    fn close_is_reflected_in_header() {
        let s = render(&Response::text("x"), true, false);
        assert!(s.contains("connection: close\r\n"), "{s}");
    }

    #[test]
    fn head_keeps_content_length_but_writes_no_body() {
        let s = render(&Response::text("hello"), false, true);
        assert!(s.contains("content-length: 5\r\n"), "{s}");
        assert!(s.ends_with("\r\n\r\n"), "HEAD 不能有 body：{s}");
    }

    #[test]
    fn no_content_has_no_body_and_no_length() {
        let s = render(&Response::new(Status::NO_CONTENT), false, false);
        assert!(s.contains("204 No Content"));
        assert!(!s.contains("content-length"), "204 不该带 Content-Length：{s}");
    }

    #[test]
    fn business_headers_are_kept_but_managed_ones_are_overridden() {
        let mut r = Response::text("x");
        r.headers.insert("x-custom", "1");
        r.headers.insert("content-length", "999"); // 应被忽略
        let s = render(&r, false, false);
        assert!(s.contains("x-custom: 1\r\n"), "{s}");
        assert!(s.contains("content-length: 1\r\n"), "长度以实际 body 为准：{s}");
        assert!(!s.contains("999"), "{s}");
    }

    #[test]
    fn header_injection_is_dropped_at_write_time() {
        let mut r = Response::text("x");
        // Headers::insert 已经拒了，这里直接模拟"绕过 insert 的脏值"
        r.headers.insert("x-evil", "a\r\nSet-Cookie: admin=1");
        let s = render(&r, false, false);
        assert!(!s.contains("admin=1"), "响应拆分必须被拦住：{s}");
    }

    #[test]
    fn date_is_rfc1123() {
        let d = http_date();
        // 形如 "Thu, 01 Jan 1970 00:00:00 GMT"
        assert!(d.ends_with(" GMT"), "{d}");
        assert_eq!(d.len(), 29, "RFC 1123 长度固定：{d}");
        assert!(d.contains(", "), "{d}");
    }

    #[test]
    fn civil_from_days_known_points() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(365), (1971, 1, 1));
        // 2000-03-01 = 11017 天
        assert_eq!(civil_from_days(11017), (2000, 3, 1));
    }
}
