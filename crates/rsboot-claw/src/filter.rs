//! URL 过滤：决定"哪些链接值得入队"。
//!
//! 为什么放在入队前而不是抓完再丢：一条被拒的链接省下的是**整个请求**——
//! 带宽、对方的负载、你自己的重试预算。爬虫最常见的浪费就是一路跟着
//! `*.pdf` / `/cart/` / `?sort=` 抓下去。
//!
//! 匹配对象是 `path?query`（不是整个 URL）：规则写在路径上更好读，
//! 也天然不受 `http://a.com` 与 `http://a.com/` 这种归一差异影响。
//!
//! 是**整串**匹配，不是子串包含：要匹配"任意前缀的分页"就写 `"*?page=*"`，
//! 而不是 `"?page=*"`。
//!
//! 语义：**deny 优先**；`allow` 非空时等于白名单（不在里面的一律拒）。
//! 通配只有一个字符：`*` 任意串（可以跨 `/`）。
//! 不做正则——正则引擎是几百行代码加一个回溯炸弹，爬虫的过滤需求用不上。
//!
//! 一开始还支持过 `?` 当"任意单字符"，很快就撞上 `?page=1` 这种写法：
//! 用户写 `"?page=*"` 想要的是"所有分页"，引擎读成"任意字符 + page=…"，
//! 静默不匹配。URL 里 `?` 是查询分隔符，不能兼作通配符——删掉。

use crate::url::Url;

/// 一条过滤规则。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    raw: String,
}

impl Pattern {
    pub fn new(raw: impl Into<String>) -> Self {
        Self { raw: raw.into() }
    }

    /// 匹配 `path?query`。
    pub fn matches(&self, target: &str) -> bool {
        glob_match(self.raw.as_bytes(), target.as_bytes())
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

/// 一组 allow / deny 规则。默认（空）放过一切。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UrlFilter {
    allow: Vec<Pattern>,
    deny: Vec<Pattern>,
}

impl UrlFilter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty()
    }

    /// 加一条允许规则。调用过它就变成白名单语义。
    pub fn allow(mut self, pattern: impl Into<String>) -> Self {
        self.allow.push(Pattern::new(pattern));
        self
    }

    /// 加一条拒绝规则。deny 比 allow 优先：同时命中时拒。
    pub fn deny(mut self, pattern: impl Into<String>) -> Self {
        self.deny.push(Pattern::new(pattern));
        self
    }

    pub fn allowed(&self, url: &Url) -> bool {
        if self.is_empty() {
            return true;
        }
        let target = url.request_target();
        if self.deny.iter().any(|p| p.matches(&target)) {
            return false;
        }
        if self.allow.is_empty() {
            return true;
        }
        self.allow.iter().any(|p| p.matches(&target))
    }

    /// `(allow 条数, deny 条数)`，给文档和调试看。
    pub fn len(&self) -> (usize, usize) {
        (self.allow.len(), self.deny.len())
    }
}

/// 经典的单星号回溯匹配：`*` 记位置，失配就回退。
///
/// 复杂度最坏 O(n·m)，没有指数回溯——因为只有一个 `*` 记位点，
/// 不会像正则那样出现多个量词互相嵌套的爆炸。
fn glob_match(pat: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;
    while t < text.len() {
        if p < pat.len() && pat[p] == text[t] {
            p += 1;
            t += 1;
        } else if p < pat.len() && pat[p] == b'*' {
            star = Some(p);
            p += 1;
            mark = t;
        } else if let Some(s) = star {
            // 回退：让上一个 `*` 多吃一个字符
            p = s + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn glob_basics() {
        assert!(glob_match(b"*.pdf", b"/a/b.pdf"));
        assert!(glob_match(b"/docs/*", b"/docs/x/y"));
        assert!(glob_match(b"/docs/*", b"/docs/"));
        assert!(!glob_match(b"/docs/*", b"/docs"));
        // `?` 是普通字符：`?page=*` 这种规则要能按字面匹配
        assert!(glob_match(b"/p?page=*", b"/p?page=1"));
        assert!(!glob_match(b"/p?page=*", b"/pa?page=1"));
        assert!(glob_match(b"*", b""));
        assert!(!glob_match(b"a*c", b"ab"));
    }

    #[test]
    fn empty_filter_allows_everything() {
        let f = UrlFilter::new();
        assert!(f.allowed(&u("http://a.com/anything?x=1")));
    }

    #[test]
    fn deny_wins_over_allow() {
        let f = UrlFilter::new().allow("/docs/*").deny("*.pdf");
        assert!(f.allowed(&u("http://a.com/docs/a.html")));
        assert!(!f.allowed(&u("http://a.com/docs/a.pdf")));
    }

    #[test]
    fn allow_turns_it_into_a_whitelist() {
        let f = UrlFilter::new().allow("/docs/*");
        assert!(!f.allowed(&u("http://a.com/blog/x")));
        assert!(f.allowed(&u("http://a.com/docs/x")));
    }

    #[test]
    fn query_takes_part_in_matching() {
        // 只留第一页：`?page=2` 之类直接拒掉
        let f = UrlFilter::new().deny("*?page=*");
        assert!(!f.allowed(&u("http://a.com/list?page=1")));
        assert!(f.allowed(&u("http://a.com/list?sort=1")));
        let g = UrlFilter::new().allow("/list?page=1");
        assert!(g.allowed(&u("http://a.com/list?page=1")));
        assert!(!g.allowed(&u("http://a.com/list?page=2")));
    }

    #[test]
    fn matching_is_case_sensitive() {
        let f = UrlFilter::new().deny("*.PDF");
        assert!(f.allowed(&u("http://a.com/a.pdf")));
        assert!(!f.allowed(&u("http://a.com/a.PDF")));
    }
}
