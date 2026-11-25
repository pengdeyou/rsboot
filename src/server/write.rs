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

use crate::web::http::Headers;
use crate::web::response::Response;

/// 写完整响应。`head_only` 为真时不写 body（HEAD 请求）。
pub fn write_response<W: Write>(
    out: &mut W,
    resp: &Response,
    close: bool,
    head_only: bool,
) -> io::Result<()> {
    let status = resp.status;
    let body_forbidden = status.0 == 204 || status.0 == 304;

    // 状态行
    write!(out, "HTTP/1.1 {} {}\r\n", status.0, status.reason())?;

    // 自动头。业务头里已经有同名的，以业务头为准（下面会跳过重复写入）。
    let mut headers = Headers::new();
    headers.insert("date", &http_date());
    headers.insert("server", &format!("rsboot/{}", crate::VERSION));
    if !body_forbidden {
        headers.insert("content-length", &resp.body.len().to_string());
    }
    if close {
        headers.insert("connection", "close");
    } else {
        headers.insert("connection", "keep-alive");
    }

    for (name, value) in resp.headers.iter() {
        // 这几个由本层负责，业务侧设置会被覆盖成一致的值
        if matches!(name, "content-length" | "connection" | "date" | "server") {
            continue;
        }
        headers.insert(name, value);
    }

    for (name, value) in headers.iter() {
        // 再过一遍 insert 的校验：value 里若混进 CRLF（比如来自用户数据）就丢弃
        if name.is_empty() || value.contains('\r') || value.contains('\n') {
            continue;
        }
        write!(out, "{name}: {value}\r\n")?;
    }
    write!(out, "\r\n")?;

    if !head_only && !body_forbidden {
        out.write_all(&resp.body)?;
    }
    out.flush()
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
