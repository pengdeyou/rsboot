//! 头部容器。
//!
//! 名字大小写不敏感，`get("Content-Type")` 与 `get("content-type")` 等价。
//! 值里的 `\r` / `\n` 在**入口处**就被拒掉（解析时 400，构造响应时拒绝写入），
//! 否则一个精心构造的头部值就能在响应里插入额外的头（HTTP 响应拆分）。
//!
//! ## 解析路径不分配
//!
//! 典型请求带 8~12 个头。旧实现每头两次 `String`（小写名 + 值），
//! 一个请求就是 16~24 次分配——实测占 parse 阶段的大头。
//! 现在解析出来的头只存**原文里的两个区间**，整块头原文由 `Arc<str>` 持有，
//! 读取时才切片。`get` / `iter` 的签名没变，照样返回 `&str`。
//!
//! 手工 `insert`（构造响应头）仍走 `String`：那条路径本来就要拥有数据。

use std::borrow::Cow;
use std::ops::Range;
use std::sync::Arc;

/// 典型请求有多少个头（含服务端补的那条 `x-request-id`）。
const TYPICAL_HDRS: usize = 13;

/// 可复用的头部槽位。
///
/// `Headers` 每次解析都要一个 `Vec::with_capacity(13)`，这是每请求一次的固定税。
/// 而 keep-alive 连接上，相邻请求的头部形状几乎一样——把这批槽位在连接里
/// 存一份，下一个请求直接拿来用，这次 malloc 就没了。`clear` 掉旧项是必须的：
/// 它们的区间指向上一轮的头原文，而那份原文下一轮就被覆写了。
pub(crate) struct HeaderSpare {
    items: Vec<Hdr>,
}

impl HeaderSpare {
    #[inline]
    pub(crate) fn new() -> Self {
        Self {
            items: Vec::with_capacity(TYPICAL_HDRS),
        }
    }
}

impl Default for HeaderSpare {
    fn default() -> Self {
        Self::new()
    }
}

/// 有序头部列表。头部数量很少（一般 < 20），线性查找比哈希更快也更省内存。
///
/// 生命周期参数只跟"请求头原文"有关：解析出来的头部借连接体内的缓冲，
/// 不复制也不分配。响应头没有原文，用默认的 `'static`——所以老的
/// `Headers` / `Response { headers }` 写法一行都不用改。
#[derive(Debug, Clone)]
pub struct Headers<'a> {
    /// 解析来源：整块"请求头原文"。`Sliced` 项都指向它。
    raw: Option<Cow<'a, str>>,
    items: Vec<Hdr>,
}

#[derive(Debug, Clone)]
struct Hdr {
    name: Name,
    value: Val,
}

#[derive(Debug, Clone)]
enum Name {
    /// 手工插入，名字已小写
    Owned(String),
    /// 编译期常量名（`content-type`、`x-request-id` 之类），零分配
    Static(&'static str),
    /// 解析得来，指向 `raw`
    Sliced(Range<u32>),
}

#[derive(Debug, Clone)]
enum Val {
    Owned(String),
    /// 编译期常量值
    Static(&'static str),
    /// 与别处共享的同一份文本（如 trace id），clone 只加引用计数
    Shared(Arc<str>),
    Sliced(Range<u32>),
}

impl Name {
    fn as_str<'a>(&'a self, raw: Option<&'a str>) -> &'a str {
        match self {
            Name::Owned(s) => s,
            Name::Static(s) => s,
            Name::Sliced(r) => slice(raw, r),
        }
    }
}

impl Val {
    fn as_str<'a>(&'a self, raw: Option<&'a str>) -> &'a str {
        match self {
            Val::Owned(s) => s,
            Val::Static(s) => s,
            Val::Shared(a) => a,
            Val::Sliced(r) => slice(raw, r),
        }
    }
}

#[inline]
fn slice<'a>(raw: Option<&'a str>, r: &Range<u32>) -> &'a str {
    // 构造方保证区间落在 utf-8 边界内，且 raw 一定在场
    raw.map(|s| &s[r.start as usize..r.end as usize])
        .unwrap_or("")
}

impl Headers<'static> {
    /// 空头部表，**预分配 4 格**：响应头（content-type / content-length /
    /// x-request-id …）几乎总有 2–4 条，`Vec` 从 0 长到 4 意味着 2–3 次
    /// `realloc`，而这是每请求都会走的路。
    ///
    /// 定义在 `'static` 上：响应头永远没有"原文"，所以 `Headers::new()`
    /// 的类型不需要调用方标注。
    pub fn new() -> Self {
        Self {
            raw: None,
            items: Vec::with_capacity(4),
        }
    }
}

impl Default for Headers<'static> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> Headers<'a> {
    /// 解析结果构造：整块原文 + 每头的 (名区间, 值区间)。
    pub(crate) fn from_parsed(raw: &'a str, items: Vec<(Range<u32>, Range<u32>)>) -> Self {
        let mut h = Self::sliced(raw, items.len());
        for (n, v) in items {
            h.push_sliced(n, v);
        }
        h
    }

    /// 解析路径用：先挂上整块原文，再逐条 `push_sliced`。
    ///
    /// 这样一次 `Vec` 分配就装完，不再"先攒 `Vec<(Range, Range)>`、再
    /// `collect` 成一个 `Vec<Hdr>`"——那是每请求两笔堆分配换一次结构转换。
    pub(crate) fn sliced(raw: &'a str, capacity: usize) -> Self {
        Self::sliced_with(
            raw,
            HeaderSpare {
                // 多留一格：服务端会在请求头里补一条 `x-request-id`，
                // 按原样精确分配的话，那一次插入就要整体搬迁一次
                items: Vec::with_capacity(capacity + 1),
            },
        )
    }

    /// 用连接里存下的槽位构造：容量够就**一次分配都不做**。
    pub(crate) fn sliced_with(raw: &'a str, mut spare: HeaderSpare) -> Self {
        // 上一轮的项指向的是旧的头原文，不能留
        spare.items.clear();
        if spare.items.capacity() < TYPICAL_HDRS {
            spare.items.reserve(TYPICAL_HDRS);
        }
        Self {
            raw: Some(Cow::Borrowed(raw)),
            items: spare.items,
        }
    }

    /// 把这批槽位还回连接，供下一个请求复用。
    #[inline]
    pub(crate) fn take_spare(&mut self) -> HeaderSpare {
        HeaderSpare {
            items: std::mem::take(&mut self.items),
        }
    }

    /// 追加一条指向 `raw` 的区间。区间由解析方保证落在字符边界内。
    #[inline]
    pub(crate) fn push_sliced(&mut self, name: Range<u32>, value: Range<u32>) {
        self.items.push(Hdr {
            name: Name::Sliced(name),
            value: Val::Sliced(value),
        });
    }

    /// 名字大小写不敏感。含非法字符（非 token）或值含 CRLF 时返回 `false`。
    pub fn insert(&mut self, name: &str, value: &str) -> bool {
        if !is_token(name) || has_crlf(value) {
            return false;
        }
        let raw = self.raw.as_deref();
        if let Some(slot) = self
            .items
            .iter_mut()
            .find(|h| h.name.as_str(raw).eq_ignore_ascii_case(name))
        {
            slot.value = Val::Owned(value.to_string());
        } else {
            self.items.push(Hdr {
                name: Name::Owned(name.to_ascii_lowercase()),
                value: Val::Owned(value.to_string()),
            });
        }
        true
    }

    /// 追加一条同名头，**不替换**已有的（`set-cookie` 允许出现多次）。
    ///
    /// `insert` 是"一个名字一个值"的语义，`set-cookie` 是唯一的例外：
    /// 一个响应带两条 `Set-Cookie` 必须两条都在，用 `insert` 会把前一条冲掉。
    pub fn append(&mut self, name: &str, value: &str) -> bool {
        if !is_token(name) || has_crlf(value) {
            return false;
        }
        self.items.push(Hdr {
            name: Name::Owned(name.to_ascii_lowercase()),
            value: Val::Owned(value.to_string()),
        });
        true
    }

    /// 名字与值都是编译期常量：**零分配**写入。
    ///
    /// 响应头里的 `content-type`、`allow`、`retry-after` 这类全是静态的，
    /// 走这条路径不再为每个响应分配两个 `String`。
    pub fn insert_static(&mut self, name: &'static str, value: &'static str) -> bool {
        if !is_token(name) || has_crlf(value) {
            return false;
        }
        let raw = self.raw.as_deref();
        if let Some(slot) = self
            .items
            .iter_mut()
            .find(|h| h.name.as_str(raw).eq_ignore_ascii_case(name))
        {
            slot.value = Val::Static(value);
        } else {
            self.items.push(Hdr {
                name: Name::Static(name),
                value: Val::Static(value),
            });
        }
        true
    }

    /// 名字静态、值来自别处共享的文本（trace id 这种）。
    pub fn insert_shared(&mut self, name: &'static str, value: Arc<str>) -> bool {
        if !is_token(name) || has_crlf(&value) {
            return false;
        }
        let raw = self.raw.as_deref();
        if let Some(slot) = self
            .items
            .iter_mut()
            .find(|h| h.name.as_str(raw).eq_ignore_ascii_case(name))
        {
            slot.value = Val::Shared(value);
        } else {
            self.items.push(Hdr {
                name: Name::Static(name),
                value: Val::Shared(value),
            });
        }
        true
    }

    /// 大小写不敏感查找，**零分配**：直接逐字节比较，不再每次小写化一个 `String`。
    pub fn get(&self, name: &str) -> Option<&str> {
        let raw = self.raw.as_deref();
        self.items
            .iter()
            .find(|h| h.name.as_str(raw).eq_ignore_ascii_case(name))
            .map(|h| h.value.as_str(raw))
    }

    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    pub fn remove(&mut self, name: &str) {
        let raw = self.raw.as_deref();
        self.items
            .retain(|h| !h.name.as_str(raw).eq_ignore_ascii_case(name));
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        let raw = self.raw.as_deref();
        self.items
            .iter()
            .map(move |h| (h.name.as_str(raw), h.value.as_str(raw)))
    }

    /// 解析 `Content-Length`。非法值返回 `None`——由调用方决定当 400 还是 413。
    pub fn content_length(&self) -> Option<usize> {
        self.get("content-length")?.trim().parse().ok()
    }

    /// 头部的总字节量（名 + 值 + `: ` + CRLF）。用于上限校验。
    pub fn total_bytes(&self) -> usize {
        self.iter().map(|(n, v)| n.len() + v.len() + 4).sum()
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

/// 解析时的成对校验：名字必须是合法 token，值里不能有 CR/LF。
/// 与 `insert` 同一套规则，只是不落存储——解析路径直接记区间。
pub(crate) fn validate_pair(name: &str, value: &str) -> bool {
    is_token(name) && !has_crlf(value)
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

    #[test]
    fn sliced_view_reads_without_allocating() {
        let raw = "Host: a\r\nContent-Length: 7\r\n\r\n";
        let items = vec![(0..4, 6..7), (9..23, 25..26)];
        let h = Headers::from_parsed(raw, items);
        assert_eq!(h.len(), 2);
        assert_eq!(h.get("host"), Some("a"));
        assert_eq!(h.get("CONTENT-LENGTH"), Some("7"));
        assert_eq!(h.content_length(), Some(7));
    }

    #[test]
    fn insert_overwrites_a_sliced_header() {
        let raw = "X-Trace: old\r\n\r\n";
        let mut h = Headers::from_parsed(raw, vec![(0..7, 9..12)]);
        assert!(h.insert("x-trace", "new"));
        assert_eq!(h.len(), 1, "同名覆盖，不该多出一条");
        assert_eq!(h.get("x-trace"), Some("new"));
    }
}
