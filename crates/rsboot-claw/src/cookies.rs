//! 极简 cookie jar：per-host 会话保持，够"登录态 / 分页会话"用。
//!
//! 只做 RFC 6265 里真正会被爬虫撞上的那部分：
//! `name=value`、`Domain`、`Path`、`Secure`、`Max-Age`、`Expires`。
//! `HttpOnly` 只做记录——claw 不跑 JS，它没有实际作用；
//! `SameSite` 同理忽略（这不是浏览器，没有跨站上下文）。
//!
//! 为什么默认开：真实站点大量依赖"先拿一个 session cookie，后续请求带上"，
//! 关掉会让很多站看起来"403/302 死循环"。不想要的人一句
//! `Client::cookie_jar(false)` 就回到无状态。
//!
//! 每个 worker 一个 `Client`，也就一个 jar——会话天然按线程隔离，
//! 不会出现两个 worker 共用一半登录态的情况。

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

/// 一个 cookie。字段名对着 RFC 6265 的属性名，不做本地化缩写。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    /// 小写 host（`host_only` 为真时是精确匹配的那个 host）
    pub domain: String,
    /// `Set-Cookie` 没带 `Domain`：只能回给同一个 host
    pub host_only: bool,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    /// `None` = 会话 cookie（进程结束就没了）
    pub expires: Option<SystemTime>,
}

impl Cookie {
    fn matches(&self, host: &str, path: &str, secure: bool, now: SystemTime) -> bool {
        if let Some(e) = self.expires {
            if e <= now {
                return false;
            }
        }
        if self.secure && !secure {
            return false;
        }
        let host_ok = if self.host_only {
            host == self.domain
        } else {
            host == self.domain || host.ends_with(&format!(".{}", self.domain))
        };
        if !host_ok {
            return false;
        }
        // RFC 6265 §5.1.4 的简化：cookie 的 path 必须是请求 path 的前缀，
        // 且（path 以 `/` 结尾 或）前一个字符之后紧跟 `/` 或正好到末尾。
        if !path.starts_with(&self.path) {
            return false;
        }
        if path.len() > self.path.len() && !self.path.ends_with('/') {
            // 前缀之后必须紧跟 `/`：`/docs` 匹配 `/docs/x`，不匹配 `/docsomething`
            return path.as_bytes()[self.path.len()] == b'/';
        }
        true
    }

    /// 同 name + domain + path 视为同一个 cookie（后来的覆盖前面的）。
    fn same_slot(&self, other: &Cookie) -> bool {
        self.name == other.name && self.domain == other.domain && self.path == other.path
    }
}

/// 一组 cookie。按 host 分桶只为遍历快；匹配仍然按 domain 规则做。
#[derive(Debug, Clone, Default)]
pub struct Jar {
    by_host: HashMap<String, Vec<Cookie>>,
}

impl Jar {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.by_host.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_host.values().map(|v| v.len()).sum()
    }

    pub fn clear(&mut self) {
        self.by_host.clear();
    }

    /// 吃一条 `Set-Cookie` 头。`host` 是发这条响应的主机（小写）。
    ///
    /// 返回 true 表示 jar 发生了变化（新增/覆盖/删除）。
    pub fn store(&mut self, host: &str, raw: &str, now: SystemTime) -> bool {
        let raw = raw.trim();
        if raw.is_empty() {
            return false;
        }
        let mut parts = raw.split(';');
        let first = parts.next().unwrap_or("").trim();
        let Some((name, value)) = first.split_once('=') else {
            return false;
        };
        let (name, value) = (name.trim(), value.trim());
        if name.is_empty() {
            return false;
        }

        let mut domain: Option<String> = None;
        let mut path: Option<String> = None;
        let mut secure = false;
        let mut http_only = false;
        let mut max_age: Option<i64> = None;
        let mut expires: Option<SystemTime> = None;

        for attr in parts {
            let attr = attr.trim();
            if attr.is_empty() {
                continue;
            }
            let (k, v) = match attr.split_once('=') {
                Some((k, v)) => (k.trim(), Some(v.trim())),
                None => (attr, None),
            };
            match k.to_ascii_lowercase().as_str() {
                "domain" => {
                    if let Some(d) = v {
                        let d = d.trim_start_matches('.').to_ascii_lowercase();
                        if !d.is_empty() {
                            domain = Some(d);
                        }
                    }
                }
                "path" => {
                    if let Some(p) = v {
                        if p.starts_with('/') {
                            path = Some(p.to_string());
                        }
                    }
                }
                "secure" => secure = true,
                "httponly" => http_only = true,
                "max-age" => {
                    if let Some(secs) = v.and_then(|s| s.parse::<i64>().ok()) {
                        max_age = Some(secs);
                    }
                }
                "expires" => {
                    if let Some(t) = v.and_then(parse_http_date) {
                        expires = Some(t);
                    }
                }
                _ => {}
            }
        }

        // 域名校验：显式 Domain 必须是本机域名或它的子域，否则是"给别人的 cookie"
        if let Some(d) = &domain {
            if !(host == d || host.ends_with(&format!(".{d}"))) {
                return false;
            }
        }

        let host_only = domain.is_none();
        let domain = domain.unwrap_or_else(|| host.to_string());
        let path = path.unwrap_or_else(|| "/".to_string());

        // 过期判定：Max-Age 优先于 Expires（RFC 6265 §5.3）
        let dead = match max_age {
            Some(a) => a <= 0,
            None => expires.is_some_and(|e| e <= now),
        };

        let bucket = self.by_host.entry(host.to_string()).or_default();
        if dead {
            let before = bucket.len();
            bucket.retain(|c| !(c.name == name && c.domain == domain && c.path == path));
            return bucket.len() != before;
        }

        let cookie = Cookie {
            name: name.to_string(),
            value: value.to_string(),
            domain,
            host_only,
            path,
            secure,
            http_only,
            expires: match max_age {
                Some(a) if a > 0 => Some(now + Duration::from_secs(a as u64)),
                _ => expires,
            },
        };
        match bucket.iter_mut().find(|c| c.same_slot(&cookie)) {
            Some(slot) => {
                *slot = cookie;
            }
            None => bucket.push(cookie),
        }
        true
    }

    /// 吃一个响应的全部 `Set-Cookie` 头。
    pub fn store_headers(&mut self, host: &str, headers: &[(String, String)], now: SystemTime) -> usize {
        let mut n = 0;
        for (k, v) in headers {
            if k.eq_ignore_ascii_case("set-cookie") && self.store(host, v, now) {
                n += 1;
            }
        }
        n
    }

    /// 给这个请求拼 `Cookie:` 头的值。没匹配到就是 `None`。
    ///
    /// 按 path 长度降序：更具体的 cookie 排在前面，服务端按"第一个同名"取时
    /// 拿到的是最具体那个（RFC 6265 §5.4 的建议顺序）。
    pub fn header_for(&self, host: &str, path: &str, secure: bool, now: SystemTime) -> Option<String> {
        let mut picked: Vec<&Cookie> = self
            .by_host
            .values()
            .flatten()
            .filter(|c| c.matches(host, path, secure, now))
            .collect();
        if picked.is_empty() {
            return None;
        }
        picked.sort_by_key(|c| std::cmp::Reverse(c.path.len()));
        let mut out = String::new();
        for c in picked {
            if !out.is_empty() {
                out.push_str("; ");
            }
            out.push_str(&c.name);
            out.push('=');
            out.push_str(&c.value);
        }
        Some(out)
    }

    /// 只读快照，给测试和调试看。
    pub fn cookies(&self) -> Vec<&Cookie> {
        self.by_host.values().flatten().collect()
    }
}

/// 解析 RFC 1123 日期（`Sun, 06 Nov 1994 08:49:37 GMT`）。
///
/// 只认这一种：真实服务器（nginx / Apache / CDN）基本都用它。
/// 解析失败就当没给 `Expires`，退化成会话 cookie——比把 cookie 判成
/// 已过期要安全（少发一个 cookie 就是 403，多发一个最多是冗余）。
fn parse_http_date(s: &str) -> Option<SystemTime> {
    let s = s.trim();
    let rest = s.split_once(',').map_or(s, |(_, r)| r).trim();
    let mut it = rest.split_whitespace();
    let day: u32 = it.next()?.parse().ok()?;
    let month = match it.next()?.to_ascii_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    };
    let year: i64 = it.next()?.parse().ok()?;
    let hms = it.next()?;
    let mut hms = hms.split(':');
    let h: i64 = hms.next()?.parse().ok()?;
    let m: i64 = hms.next()?.parse().ok()?;
    let sec: i64 = hms.next()?.trim().parse().ok()?;
    if !(1..=31).contains(&day) || h > 23 || m > 59 || sec > 60 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let secs = days * 86400 + h * 3600 + m * 60 + sec;
    if secs < 0 {
        return None;
    }
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64))
}

/// Howard Hinnant 的 `days_from_civil`：公历日期 → 距 1970-01-01 的天数。
///
/// 用它而不是自己算闰年：闰年规则（4/100/400）在这里一步到位，
/// 也顺手处理了"2 月算成上一年的 13/14 月"这种边界。
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    #[test]
    fn stores_and_sends_a_session_cookie() {
        let mut j = Jar::new();
        assert!(j.store("a.com", "sid=abc123; Path=/", now()));
        assert_eq!(
            j.header_for("a.com", "/x/y", false, now()).as_deref(),
            Some("sid=abc123")
        );
        // host-only：换 host 不发
        assert_eq!(j.header_for("b.com", "/", false, now()), None);
    }

    #[test]
    fn domain_attr_allows_subdomains_but_not_others() {
        let mut j = Jar::new();
        assert!(j.store("a.com", "g=1; Domain=.a.com; Path=/", now()));
        assert!(j.header_for("api.a.com", "/", false, now()).is_some());
        assert_eq!(j.header_for("evil.com", "/", false, now()), None);
        // 反过来：别人想给 a.com 塞 cookie，不收
        assert!(!j.store("evil.com", "g=1; Domain=.a.com", now()));
    }

    #[test]
    fn path_prefix_rules() {
        let mut j = Jar::new();
        j.store("a.com", "p=1; Path=/docs", now());
        assert!(j.header_for("a.com", "/docs/x", false, now()).is_some());
        assert_eq!(
            j.header_for("a.com", "/docsomething", false, now()),
            None,
            "/docs 只能匹配 /docs 或 /docs/… ，不能匹配 /docsomething"
        );
    }

    #[test]
    fn secure_cookie_is_not_sent_over_plain_http() {
        let mut j = Jar::new();
        j.store("a.com", "s=1; Secure; HttpOnly", now());
        assert_eq!(j.header_for("a.com", "/", false, now()), None);
        assert!(j.header_for("a.com", "/", true, now()).is_some());
    }

    #[test]
    fn max_age_zero_deletes_and_positive_expires() {
        let mut j = Jar::new();
        assert!(j.store("a.com", "t=1", now()));
        assert_eq!(j.len(), 1);
        // 删除
        assert!(j.store("a.com", "t=; Max-Age=0", now()));
        assert_eq!(j.len(), 0);
        // 正向 Max-Age
        assert!(j.store("a.com", "u=1; Max-Age=60", now()));
        assert!(j.header_for("a.com", "/", false, now()).is_some());
        assert_eq!(j.header_for("a.com", "/", false, now() + Duration::from_secs(61)), None);
    }

    #[test]
    fn expires_header_is_parsed() {
        let mut j = Jar::new();
        j.store(
            "a.com",
            "e=1; Expires=Wed, 21 Oct 2015 07:28:00 GMT",
            now(),
        );
        // 2015 已经过去了：这条应当被当成"立即删除"，不存
        assert_eq!(j.len(), 0);

        j.store(
            "a.com",
            "f=1; Expires=Mon, 01 Jan 2035 00:00:00 GMT",
            now(),
        );
        assert!(j.header_for("a.com", "/", false, now()).is_some());
    }

    #[test]
    fn later_store_overwrites_the_same_slot() {
        let mut j = Jar::new();
        j.store("a.com", "k=v1; Path=/", now());
        j.store("a.com", "k=v2; Path=/", now());
        assert_eq!(j.len(), 1);
        assert_eq!(
            j.header_for("a.com", "/", false, now()).as_deref(),
            Some("k=v2")
        );
    }

    #[test]
    fn header_orders_by_path_length() {
        let mut j = Jar::new();
        j.store("a.com", "a=root; Path=/", now());
        j.store("a.com", "b=deep; Path=/docs/x", now());
        let h = j.header_for("a.com", "/docs/x/y", false, now()).unwrap();
        assert!(h.starts_with("b=deep; a=root"), "{h}");
    }

    #[test]
    fn date_parsing_matches_known_epoch() {
        // 1994-11-06 08:49:37 UTC = 784111777
        let t = parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT").unwrap();
        assert_eq!(
            t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs(),
            784_111_777
        );
    }
}
