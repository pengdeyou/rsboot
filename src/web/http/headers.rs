//! 头部容器。
//!
//! 名字统一小写存储，`get("Content-Type")` 与 `get("content-type")` 等价。
//! 值里的 `\r` / `\n` 在**入口处**就被拒掉（解析时 400，构造响应时拒绝写入），
//! 否则一个精心构造的头部值就能在响应里插入额外的头（HTTP 响应拆分）。

/// 有序头部列表。头部数量很少（一般 < 20），线性查找比哈希更快也更省内存。
#[derive(Debug, Clone, Default)]
pub struct Headers {
    items: Vec<(String, String)>,
}

impl Headers {
    pub fn new() -> Self {
        Self::default()
    }

    /// 头部名统一小写。含非法字符（非 token）或值含 CRLF 时返回 `false`。
    pub fn insert(&mut self, name: &str, value: &str) -> bool {
        if !is_token(name) || has_crlf(value) {
            return false;
        }
        let lower = name.to_ascii_lowercase();
        if let Some(slot) = self.items.iter_mut().find(|(n, _)| *n == lower) {
            slot.1 = value.to_string();
        } else {
            self.items.push((lower, value.to_string()));
        }
        true
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        let lower = name.to_ascii_lowercase();
        self.items
            .iter()
            .find(|(n, _)| *n == lower)
            .map(|(_, v)| v.as_str())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    pub fn remove(&mut self, name: &str) {
        let lower = name.to_ascii_lowercase();
        self.items.retain(|(n, _)| *n != lower);
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.items.iter().map(|(n, v)| (n.as_str(), v.as_str()))
    }

    /// 解析 `Content-Length`。非法值返回 `None`——由调用方决定当 400 还是 413。
    pub fn content_length(&self) -> Option<usize> {
        self.get("content-length")?.trim().parse().ok()
    }

    /// 头部的总字节量（名 + 值 + `: ` + CRLF）。用于上限校验。
    pub fn total_bytes(&self) -> usize {
        self.items
            .iter()
            .map(|(n, v)| n.len() + v.len() + 4)
            .sum()
    }
}

/// `token` 字符集（RFC 7230）。头部名用了别的字符一律不接受。
fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    '!' | '#' | '$' | '%' | '&' | '\'' | '*' | '+' | '-' | '.' | '^' | '_' | '`'
                        | '|' | '~'
                )
        })
}

/// 值里出现 CR/LF 就是响应拆分的入口，必须拒。
pub fn has_crlf(s: &str) -> bool {
    s.contains('\r') || s.contains('\n')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_names_are_case_insensitive_and_overwrite() {
        let mut h = Headers::new();
        assert!(h.insert("Content-Type", "application/json"));
        assert_eq!(h.get("content-type"), Some("application/json"));
        assert!(h.insert("content-type", "text/plain"));
        assert_eq!(h.len(), 1);
        assert_eq!(h.get("CONTENT-TYPE"), Some("text/plain"));
    }

    #[test]
    fn illegal_name_is_refused() {
        let mut h = Headers::new();
        assert!(!h.insert("bad name", "x"), "空格不是合法 token 字符");
        assert!(!h.insert("", "x"));
        assert!(!h.insert("bad:name", "x"));
        assert_eq!(h.len(), 0);
    }

    #[test]
    fn crlf_in_value_is_refused() {
        let mut h = Headers::new();
        assert!(!h.insert("location", "/x\r\nSet-Cookie: admin=1"));
        assert!(!h.insert("x", "/x\nY: 1"));
        assert_eq!(h.len(), 0);
    }

    #[test]
    fn content_length_parsing() {
        let mut h = Headers::new();
        assert!(h.insert("content-length", "42"));
        assert_eq!(h.content_length(), Some(42));
        assert!(h.insert("content-length", "abc"));
        assert_eq!(h.content_length(), None);
        assert!(h.insert("content-length", "-1"));
        assert_eq!(h.content_length(), None);
    }

    #[test]
    fn total_bytes_counts_every_header() {
        let mut h = Headers::new();
        h.insert("a", "1");
        h.insert("b", "22");
        // ("a","1") = 1+1+4 ; ("b","22") = 1+2+4
        assert_eq!(h.total_bytes(), 6 + 7);
    }
}
