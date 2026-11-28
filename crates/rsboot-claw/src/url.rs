//! URL 解析、相对链接解析、规范化。
//!
//! 只做爬虫真正需要的那部分：`scheme://host[:port]/path[?query]`。
//! 不做 IDN、不做 percent-encoding 归一（这两件各自都能写一本书，
//! 而爬虫去重要的是"同一个资源得到同一个 key"，不是"人类可读"）。
//!
//! 明确不支持：`mailto:` / `javascript:` / `data:` 等非 http(s) 协议，
//! 解析直接返回 `None`——候选链接里这类占很大比例，早点丢掉省得后面判。

use std::fmt;

/// 规范化后的 URL。`fragment` 已被丢弃：同一个资源的不同锚点不是不同页面。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Url {
    /// `http` 或 `https`（小写）
    pub scheme: String,
    /// 小写主机名。不做 DNS 解析，`example.com` 与 `EXAMPLE.COM` 视为同一台。
    pub host: String,
    /// 端口。默认端口（http 80 / https 443）归一成 `None`。
    pub port: Option<u16>,
    /// 以 `/` 开头的路径，已消掉 `.` / `..`。空路径归一成 `/`。
    pub path: String,
    /// 不含 `?`，保持原始顺序（不排序：排序会让 `?a=1&b=2` 与 `?b=2&a=1`
    /// 合并，而那可能是两个不同的资源）。
    pub query: Option<String>,
}

impl Url {
    /// 解析一个绝对 URL。不是 `http(s)` 就返回 `None`。
    pub fn parse(input: &str) -> Option<Self> {
        let s = input.trim();
        let (scheme, rest) = s.split_once("://")?;
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return None;
        }
        // authority 到第一个 `/` / `?` / `#` 为止
        let end = rest
            .find(['/', '?', '#'])
            .unwrap_or(rest.len());
        let authority = &rest[..end];
        let tail = &rest[end..];
        if authority.is_empty() {
            return None;
        }
        let (host, port) = split_authority(authority)?;
        let (path, query) = split_tail(tail);
        let port = normalize_port(&scheme, port);
        Some(Self {
            scheme,
            host,
            port,
            path: normalize_path(path),
            query,
        })
    }

    /// 把 `href` 解析成绝对 URL。相对路径按 `base` 的目录解析。
    ///
    /// 优先级按 RFC 3986 §5.2：完整 URL → `//host/x` → `/x` → `x` → `?q` → `#f`。
    pub fn resolve(base: &Url, href: &str) -> Option<Self> {
        let href = href.trim();
        if href.is_empty() || href.starts_with('#') {
            return None; // 纯锚点不是新页面
        }
        let lower = href.to_ascii_lowercase();
        // 这些不是"页面"，跟着它们爬只会浪费时间
        for bad in [
            "javascript:", "mailto:", "tel:", "data:", "ftp:", "file:", "about:", "blob:",
        ] {
            if lower.starts_with(bad) {
                return None;
            }
        }
        if let Some(abs) = Self::parse(href) {
            return Some(abs);
        }
        if let Some(rest) = href.strip_prefix("//") {
            return Self::parse(&format!("{}://{}", base.scheme, rest));
        }
        // 剩下的都继承 base 的 scheme/host/port
        let (raw_path, query) = if let Some(q) = href.strip_prefix('?') {
            (base.path.clone(), Some(q.to_string()))
        } else {
            let no_frag = href.split('#').next().unwrap_or("");
            match no_frag.split_once('?') {
                Some((p, q)) => (p.to_string(), Some(q.to_string())),
                None => (no_frag.to_string(), None),
            }
        };
        let path = if raw_path.starts_with('/') {
            normalize_path(&raw_path)
        } else {
            normalize_path(&format!("{}/{}", parent_dir(&base.path), raw_path))
        };
        Some(Self {
            scheme: base.scheme.clone(),
            host: base.host.clone(),
            port: base.port,
            path,
            query,
        })
    }

    /// 请求行里要写的东西：`/path?query`。
    pub fn request_target(&self) -> String {
        match &self.query {
            Some(q) => format!("{}?{q}", self.path),
            None => self.path.clone(),
        }
    }

    /// `Host` 头的值（非默认端口要带端口）。
    pub fn host_header(&self) -> String {
        match self.port {
            Some(p) => format!("{}:{p}", self.host),
            None => self.host.clone(),
        }
    }

    /// 去重用的 key：scheme + host + port + path + query。
    ///
    /// 比 URL 字符串更稳：`http://a.com` 与 `http://a.com/` 得到同一个 key。
    pub fn key(&self) -> String {
        let mut k = String::with_capacity(self.path.len() + self.host.len() + 16);
        k.push_str(&self.scheme);
        k.push_str("://");
        k.push_str(&self.host);
        if let Some(p) = self.port {
            k.push(':');
            k.push_str(&p.to_string());
        }
        k.push_str(&self.path);
        if let Some(q) = &self.query {
            k.push('?');
            k.push_str(q);
        }
        k
    }

    /// 端口：非默认才算有端口。`http://a.com:80/x` 与 `http://a.com/x` 等价。
    pub fn real_port(&self) -> u16 {
        self.port
            .unwrap_or(if self.scheme == "https" { 443 } else { 80 })
    }
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://{}", self.scheme, self.host)?;
        if let Some(p) = self.port {
            write!(f, ":{p}")?;
        }
        write!(f, "{}", self.path)?;
        if let Some(q) = &self.query {
            write!(f, "?{q}")?;
        }
        Ok(())
    }
}

fn split_authority(authority: &str) -> Option<(String, Option<u16>)> {
    // 用户信息（`user:pass@`）对爬虫没用，丢掉但要能解析出来
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    if host_port.is_empty() {
        return None;
    }
    // IPv6 字面量：`[::1]:8080`
    if let Some(rest) = host_port.strip_prefix('[') {
        let (h, tail) = rest.split_once(']')?;
        let port = tail.strip_prefix(':').and_then(|p| p.parse().ok());
        return Some((format!("[{h}]"), port));
    }
    match host_port.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() => Some((h.to_ascii_lowercase(), p.parse().ok())),
        Some(_) => None,
        None => Some((host_port.to_ascii_lowercase(), None)),
    }
}

fn split_tail(tail: &str) -> (&str, Option<String>) {
    let no_frag = tail.split('#').next().unwrap_or("");
    match no_frag.split_once('?') {
        Some((p, q)) => (p, Some(q.to_string())),
        None => (no_frag, None),
    }
}

fn normalize_port(scheme: &str, port: Option<u16>) -> Option<u16> {
    match (scheme, port) {
        ("http", Some(80)) | ("https", Some(443)) => None,
        (_, p) => p,
    }
}

/// 消掉 `.` / `..` / 重复的 `/`，保证以 `/` 开头。
fn normalize_path(raw: &str) -> String {
    if raw.is_empty() {
        return "/".to_string();
    }
    let mut out: Vec<&str> = Vec::new();
    for seg in raw.split('/') {
        match seg {
            "" | "." => continue,
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    let mut p = String::with_capacity(raw.len());
    p.push('/');
    p.push_str(&out.join("/"));
    // 保留尾斜杠：`/a/` 和 `/a` 在部分站点上是两个资源
    if raw.len() > 1 && raw.ends_with('/') && !p.ends_with('/') {
        p.push('/');
    }
    p
}

/// base 的目录部分（最后一个 `/` 之前），用于拼接相对链接。
fn parent_dir(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "",
        Some(i) => &path[..i],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).expect(s)
    }

    #[test]
    fn parses_and_normalizes_defaults() {
        let a = u("HTTP://Example.COM:80/a/./b/../c?x=1#frag");
        assert_eq!(a.scheme, "http");
        assert_eq!(a.host, "example.com");
        assert_eq!(a.port, None, "80 是默认端口，归一成 None");
        assert_eq!(a.path, "/a/c");
        assert_eq!(a.query.as_deref(), Some("x=1"));
        assert_eq!(a.to_string(), "http://example.com/a/c?x=1");
    }

    #[test]
    fn non_http_schemes_are_rejected() {
        assert!(Url::parse("mailto:a@b.com").is_none());
        assert!(Url::parse("javascript:void(0)").is_none());
        assert!(Url::parse("ftp://a.com/x").is_none());
        assert!(Url::resolve(&u("http://a.com/"), "mailto:a@b.com").is_none());
    }

    #[test]
    fn resolves_relative_forms() {
        let base = u("http://a.com/docs/page.html");
        assert_eq!(
            Url::resolve(&base, "../x/y.html").unwrap().path,
            "/x/y.html"
        );
        assert_eq!(Url::resolve(&base, "/root").unwrap().path, "/root");
        assert_eq!(
            Url::resolve(&base, "sub/z.html").unwrap().path,
            "/docs/sub/z.html"
        );
        // 协议相对
        assert_eq!(
            Url::resolve(&base, "//other.com/p").unwrap().host,
            "other.com"
        );
        // 只有 query / 只有锚点
        assert_eq!(
            Url::resolve(&base, "?page=2").unwrap().query.as_deref(),
            Some("page=2")
        );
        assert!(Url::resolve(&base, "#top").is_none());
    }

    #[test]
    fn key_merges_equivalent_spellings() {
        assert_eq!(u("http://a.com").key(), u("http://a.com/").key());
        assert_eq!(u("http://a.com:80/x").key(), u("http://a.com/x").key());
        assert_ne!(u("http://a.com/x").key(), u("http://a.com/x?q=1").key());
    }

    #[test]
    fn request_target_and_host_header() {
        let a = u("https://a.com:8443/p?q=2");
        assert_eq!(a.request_target(), "/p?q=2");
        assert_eq!(a.host_header(), "a.com:8443");
        let b = u("https://a.com/p");
        assert_eq!(b.host_header(), "a.com");
        assert_eq!(b.real_port(), 443);
    }

    #[test]
    fn keeps_trailing_slash() {
        assert_eq!(u("http://a.com/dir/").path, "/dir/");
        assert_eq!(u("http://a.com/dir").path, "/dir");
    }
}
