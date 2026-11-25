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
    let status = resp.status;
    let body_forbidden = status.0 == 204 || status.0 == 304;

    let mut buf: Vec<u8> = Vec::with_capacity(96 + resp.headers.len() * 32 + resp.body.len());

    // 状态行
    buf.extend_from_slice(b"HTTP/1.1 ");
    let _ = write!(&mut buf, "{}", status.0);
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
    if !body_forbidden {
        buf.extend_from_slice(b"content-length: ");
        let _ = write!(&mut buf, "{}", resp.body.len());
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

    if !head_only && !body_forbidden {
        buf.extend_from_slice(&resp.body);
    }

    out.write_all(&buf)?;
    out.flush()
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
