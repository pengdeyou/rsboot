//! Cookie 读取与写入。
//!
//! 读取只解析 `Cookie` 头（`a=1; b=2`），写入走 `Set-Cookie`。
//! 一个响应可以带多条 `Set-Cookie`，所以写入走 `Headers::append` 而不是 `insert`。

use rsboot::Request;
use rsboot::Response;

/// 取一个 cookie 的值。
pub fn get<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    let raw = req.header("cookie")?;
    for part in raw.split(';') {
        let part = part.trim();
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        if k.trim() == name {
            return Some(v.trim());
        }
    }
    None
}

/// 取全部 cookie。
pub fn all<'a>(req: &'a Request) -> Vec<(&'a str, &'a str)> {
    let Some(raw) = req.header("cookie") else {
        return Vec::new();
    };
    raw.split(';')
        .filter_map(|part| {
            let part = part.trim();
            let (k, v) = part.split_once('=')?;
            Some((k.trim(), v.trim()))
        })
        .collect()
}

/// `SameSite` 取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameSite {
    Strict,
    Lax,
    None,
}

impl SameSite {
    fn as_str(self) -> &'static str {
        match self {
            SameSite::Strict => "Strict",
            SameSite::Lax => "Lax",
            SameSite::None => "None",
        }
    }
}

/// 一条 `Set-Cookie`。
#[derive(Debug, Clone)]
pub struct SetCookie {
    name: String,
    value: String,
    path: Option<String>,
    domain: Option<String>,
    max_age: Option<i64>,
    http_only: bool,
    secure: bool,
    same_site: Option<SameSite>,
}

impl SetCookie {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            path: None,
            domain: None,
            max_age: None,
            http_only: false,
            secure: false,
            same_site: None,
        }
    }

    pub fn path(mut self, p: impl Into<String>) -> Self {
        self.path = Some(p.into());
        self
    }

    pub fn domain(mut self, d: impl Into<String>) -> Self {
        self.domain = Some(d.into());
        self
    }

    /// 秒。负数表示立刻过期（用来删 cookie）。
    pub fn max_age(mut self, secs: i64) -> Self {
        self.max_age = Some(secs);
        self
    }

    pub fn http_only(mut self) -> Self {
        self.http_only = true;
        self
    }

    pub fn secure(mut self) -> Self {
        self.secure = true;
        self
    }

    pub fn same_site(mut self, s: SameSite) -> Self {
        self.same_site = Some(s);
        self
    }

    fn render(&self) -> String {
        let mut s = String::with_capacity(48 + self.name.len() + self.value.len());
        s.push_str(&self.name);
        s.push('=');
        s.push_str(&self.value);
        if let Some(p) = &self.path {
            s.push_str("; Path=");
            s.push_str(p);
        }
        if let Some(d) = &self.domain {
            s.push_str("; Domain=");
            s.push_str(d);
        }
        if let Some(m) = self.max_age {
            s.push_str("; Max-Age=");
            s.push_str(&m.to_string());
        }
        if self.http_only {
            s.push_str("; HttpOnly");
        }
        if self.secure {
            s.push_str("; Secure");
        }
        if let Some(sm) = self.same_site {
            s.push_str("; SameSite=");
            s.push_str(sm.as_str());
        }
        s
    }

    /// 写到响应上（追加，不覆盖已有的 `Set-Cookie`）。
    pub fn apply(self, resp: &mut Response) {
        resp.headers.append("set-cookie", &self.render());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsboot::{Method, Status};

    fn req(cookie: &str) -> Request {
        Request::new(Method::Get, "/").with_header("cookie", cookie)
    }

    #[test]
    fn reads_one_and_all() {
        let r = req("sid=abc; theme=dark");
        assert_eq!(get(&r, "sid"), Some("abc"));
        assert_eq!(get(&r, "theme"), Some("dark"));
        assert_eq!(get(&r, "missing"), None);
        assert_eq!(all(&r), vec![("sid", "abc"), ("theme", "dark")]);
    }

    #[test]
    fn writes_with_attributes_and_allows_repeats() {
        let mut resp = Response::new(Status::OK);
        SetCookie::new("sid", "abc")
            .path("/")
            .http_only()
            .secure()
            .same_site(SameSite::Lax)
            .max_age(3600)
            .apply(&mut resp);
        SetCookie::new("theme", "dark").apply(&mut resp);
        let all: Vec<&str> = resp
            .headers
            .iter()
            .filter(|(k, _)| *k == "set-cookie")
            .map(|(_, v)| v)
            .collect();
        assert_eq!(all.len(), 2, "两条 Set-Cookie 必须都在");
        assert!(all[0].starts_with("sid=abc"), "{}", all[0]);
        assert!(all[0].contains("HttpOnly") && all[0].contains("SameSite=Lax"));
    }
}
