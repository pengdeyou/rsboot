//! HTTP 日期（RFC 7231 的 IMF-fixdate）：格式化与解析。
//!
//! `Last-Modified` 与 `If-Modified-Since` 都要它。不引 `httpdate`
//! 是因为这里只需要一种格式——现代客户端全用它，RFC 850 / asctime
//! 那两种"必须能解析"的格式已经没人在发。

const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// 1970-01-01 起的天数 → (年, 月, 日)。Howard Hinnant 的 `civil_from_days`。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 把 UNIX 秒格式化成 `Sun, 06 Nov 1994 08:49:37 GMT`。
pub fn format(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    // 1970-01-01 是周四；`DAYS` 以周四打头，正好省一次取模调整。
    let wd = DAYS[(days.rem_euclid(7)) as usize];
    format!(
        "{wd}, {d:02} {} {y:04} {:02}:{:02}:{:02} GMT",
        MONTHS[(m - 1) as usize],
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// 解析 IMF-fixdate。宽松：星期与 `GMT` 后缀缺失也接受，数字允许 1 位。
///
/// 返回 UNIX 秒；格式不认识就 `None`（调用方忽略这个条件头即可，
/// 不能因为一个坏头把请求判死）。
pub fn parse(s: &str) -> Option<u64> {
    let s = s.trim();
    // 跳过 `Sun, ` 这段星期（也可能没有）
    let s = match s.find(',') {
        Some(i) => s[i + 1..].trim(),
        None => s,
    };
    let mut it = s.split_whitespace();
    let d: u32 = it.next()?.parse().ok()?;
    let mon = it.next()?;
    let m = MONTHS.iter().position(|x| x.eq_ignore_ascii_case(mon))? as u32 + 1;
    let y: i64 = it.next()?.parse().ok()?;
    let time = it.next()?;
    let mut tp = time.split(':');
    let h: u64 = tp.next()?.parse().ok()?;
    let mi: u64 = tp.next()?.parse().ok()?;
    let se: u64 = tp.next()?.trim_end_matches("GMT").parse().ok()?;
    if !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let days = days_from_civil(y, m, d);
    if days < 0 {
        return None;
    }
    Some(days as u64 * 86_400 + h * 3600 + mi * 60 + se)
}

/// (年, 月, 日) → 1970-01-01 起的天数。
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) as u64 + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_the_rfc_example() {
        assert_eq!(format(784_111_777), "Sun, 06 Nov 1994 08:49:37 GMT");
    }

    #[test]
    fn parses_what_it_formats() {
        for secs in [784_111_777u64, 1_600_000_000, 0, 1_700_000_000] {
            assert_eq!(parse(&format(secs)), Some(secs), "secs={secs}");
        }
    }

    #[test]
    fn tolerates_a_missing_weekday() {
        assert_eq!(parse("06 Nov 1994 08:49:37 GMT"), Some(784_111_777));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse("not a date"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("99 Nov 1994 08:49:37 GMT"), None);
    }
}
