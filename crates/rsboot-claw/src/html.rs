//! 零依赖 HTML 扫描器。
//!
//! 不是通用 DOM：爬虫要的是「标题、描述、canonical、链接、可见文本」这几样，
//! 为它们建一棵完整树（再配 CSS 选择器）是另一件事，也会把依赖引进来。
//!
//! 设计照 `scraper` / `lol_html` 的流式思路：**一趟扫描、不建树、不回溯**。
//! 相比"先 parse 成 DOM 再一次遍历"，省掉的是每个节点的堆分配；
//! 代价是不能做 `:nth-child` 这类结构查询——爬虫不需要。
//!
//! 扫描是字节级的，所有切片位置都落在 ASCII 的 `<` / `>` 上，
//! 因此 UTF-8 边界天然安全（多字节序列里不会出现 ASCII 字节）。
//!
//! 明确不做：实体除了最常见的几个，`&nbsp;` 之外一律原样保留；
//! 不处理 `<script>` 里字符串中的 `</script>`（浏览器也有同样歧义）。

/// 一条候选链接。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub href: String,
    /// 锚文本（去掉实体、压掉连续空白）
    pub text: String,
    /// `rel` 原值，小写。`nofollow` / `sponsored` / `ugc` 由调用方决定是否跳过。
    pub rel: Option<String>,
}

impl Link {
    /// 这个链接作者明确说过"别跟"。
    pub fn is_nofollow(&self) -> bool {
        matches!(&self.rel, Some(rel)
            if rel.split_whitespace().any(|t| t == "nofollow" || t == "sponsored" || t == "ugc"))
    }
}

/// 一趟扫描的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Document {
    pub title: String,
    pub description: String,
    /// `<link rel="canonical">`，站点用它声明"这一页的正式地址"
    pub canonical: Option<String>,
    /// `<base href>`——所有相对链接的真实基准
    pub base: Option<String>,
    pub links: Vec<Link>,
    /// 可见文本（跳过 script / style / 注释），连续空白压成一个空格
    pub text: String,
    /// `(层级, 文本)`，`h1`–`h6`
    pub headings: Vec<(u8, String)>,
}

impl Document {
    /// 只取 `h1`–`h6` 拼成一行，做摘要/分类够用了。
    pub fn headings_text(&self) -> String {
        self.headings
            .iter()
            .map(|(_, t)| t.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// 扫描一段 HTML。
pub fn scan(html: &str) -> Document {
    let b = html.as_bytes();
    let mut doc = Document::default();
    let mut i = 0usize;
    // 当前正在捕获的文本去哪：0=可见文本 1=title 2=锚文本 3=h1..h6
    let mut capture = Cap::Body;
    let mut buf = String::new();
    let mut anchor_start = 0usize; // 当前 `<a>` 的 link 下标
    let mut heading_level = 0u8;

    while i < b.len() {
        if b[i] != b'<' {
            let start = i;
            while i < b.len() && b[i] != b'<' {
                i += 1;
            }
            // 文本段：交给当前捕获目标
            let raw = &html[start..i];
            match capture {
                Cap::Body => push_text(&mut doc.text, raw),
                Cap::Title | Cap::Anchor | Cap::Heading => push_text(&mut buf, raw),
            }
            continue;
        }

        // `<!--` 注释
        if b[i..].starts_with(b"<!--") {
            match find(b, i + 4, b"-->") {
                Some(end) => i = end + 3,
                None => break,
            }
            continue;
        }
        // `<!doctype>` / `<?xml?>` 之类
        if b[i..].starts_with(b"<!") || b[i..].starts_with(b"<?") {
            match find(b, i + 2, b">") {
                Some(end) => i = end + 1,
                None => break,
            }
            continue;
        }

        let closing = b.get(i + 1) == Some(&b'/');
        let name_at = if closing { i + 2 } else { i + 1 };
        let (name, after) = read_name(b, name_at);
        let name_l = name.to_ascii_lowercase();
        let (attrs, next) = read_attrs(b, after);

        if closing {
            match name_l.as_str() {
                "title" => {
                    doc.title = take(&mut buf);
                    capture = Cap::Body;
                }
                "a" => {
                    let text = take(&mut buf);
                    if !text.is_empty() {
                        push_text(&mut doc.text, &text);
                    }
                    if let Some(link) = doc.links.get_mut(anchor_start) {
                        link.text = text;
                    }
                    capture = Cap::Body;
                }
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                    let text = take(&mut buf);
                    if text.is_empty() {
                        doc.headings.truncate(doc.headings.len().saturating_sub(1));
                    } else {
                        push_text(&mut doc.text, &text);
                        doc.headings.push((heading_level, text));
                    }
                    capture = Cap::Body;
                    heading_level = 0;
                }
                _ => {}
            }
            i = next;
            continue;
        }

        match name_l.as_str() {
            "script" | "style" => {
                // 跳过整段内容：里面的文本不是可见文本
                let close = format!("</{name_l}");
                match find_ignore_case(b, next, close.as_bytes()) {
                    Some(end) => match find(b, end, b">") {
                        Some(gt) => i = gt + 1,
                        None => break,
                    },
                    None => break,
                }
                continue;
            }
            "title" => {
                capture = Cap::Title;
                buf.clear();
            }
            "a" => {
                if let Some(href) = attr(&attrs, "href") {
                    let rel = attr(&attrs, "rel").map(|r| r.to_ascii_lowercase());
                    doc.links.push(Link {
                        href: unescape(href),
                        text: String::new(),
                        rel,
                    });
                    anchor_start = doc.links.len() - 1;
                    buf.clear();
                    capture = Cap::Anchor;
                }
            }
            "base" => {
                if let Some(href) = attr(&attrs, "href") {
                    doc.base = Some(unescape(href));
                }
            }
            "link" => {
                let rel = attr(&attrs, "rel").unwrap_or_default().to_ascii_lowercase();
                if rel.split_whitespace().any(|t| t == "canonical") {
                    if let Some(href) = attr(&attrs, "href") {
                        doc.canonical = Some(unescape(href));
                    }
                }
            }
            "meta" => {
                let name_l = attr(&attrs, "name").unwrap_or_default().to_ascii_lowercase();
                let prop = attr(&attrs, "property")
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                let is_desc = name_l == "description" || prop == "og:description";
                if is_desc && doc.description.is_empty() {
                    if let Some(content) = attr(&attrs, "content") {
                        doc.description = unescape(content).trim().to_string();
                    }
                }
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                heading_level = name_l.as_bytes()[1] - b'0';
                buf.clear();
                capture = Cap::Heading;
            }
            _ => {}
        }
        i = next;
    }

    doc.title = doc.title.trim().to_string();
    doc.text = doc.text.trim().to_string();
    doc
}

/// 取一个标签上某属性的值（属性名大小写不敏感）。
fn attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// 把一段原始文本追加到目标，压掉连续空白。
///
/// 实体解码放在这里做：`&amp;` 之类的解码结果仍是文本，
/// 早解一步能让"压缩空白"看见解码后的空格。
fn push_text(dst: &mut String, raw: &str) {
    let decoded = unescape(raw);
    for ch in decoded.chars() {
        if ch.is_whitespace() {
            if !dst.ends_with(' ') && !dst.is_empty() {
                dst.push(' ');
            }
        } else {
            dst.push(ch);
        }
    }
}

fn take(buf: &mut String) -> String {
    let out = buf.trim().to_string();
    buf.clear();
    out
}

enum Cap {
    Body,
    Title,
    Anchor,
    Heading,
}

/// 读标签名，返回 (名字, 名字之后的偏移)。
fn read_name(b: &[u8], mut i: usize) -> (&str, usize) {
    let start = i;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_alphanumeric() || c == b'-' || c == b':' || c == b'_' {
            i += 1;
        } else {
            break;
        }
    }
    // 调用方保证这里一定是 UTF-8 边界（ASCII 字符）
    (std::str::from_utf8(&b[start..i]).unwrap_or(""), i)
}

/// 解析属性直到 `>`，返回 (属性表, `>` 之后的位置)。
fn read_attrs(b: &[u8], mut i: usize) -> (Vec<(String, String)>, usize) {
    let mut out: Vec<(String, String)> = Vec::new();
    loop {
        while i < b.len() && (b[i] == b' ' || b[i] == b'\t' || b[i] == b'\n' || b[i] == b'\r') {
            i += 1;
        }
        if i >= b.len() {
            return (out, i);
        }
        match b[i] {
            b'>' => return (out, i + 1),
            b'/' if b.get(i + 1) == Some(&b'>') => return (out, i + 2),
            _ => {}
        }
        let (name, next) = read_name(b, i);
        if next == i {
            // 既不是名字也不是 `>`：跳过这个字节，别原地打转
            i += 1;
            continue;
        }
        i = next;
        while i < b.len() && (b[i] == b' ' || b[i] == b'\t' || b[i] == b'\n' || b[i] == b'\r') {
            i += 1;
        }
        if i >= b.len() || b[i] != b'=' {
            out.push((name.to_string(), String::new()));
            continue;
        }
        i += 1;
        while i < b.len() && (b[i] == b' ' || b[i] == b'\t' || b[i] == b'\n' || b[i] == b'\r') {
            i += 1;
        }
        if i >= b.len() {
            return (out, i);
        }
        let value = match b[i] {
            q @ (b'"' | b'\'') => {
                let start = i + 1;
                match b[start..].iter().position(|&c| c == q) {
                    Some(len) => {
                        i = start + len + 1;
                        std::str::from_utf8(&b[start..start + len]).unwrap_or("")
                    }
                    None => {
                        let s = std::str::from_utf8(&b[start..]).unwrap_or("");
                        out.push((name.to_string(), s.to_string()));
                        return (out, b.len());
                    }
                }
            }
            _ => {
                let start = i;
                while i < b.len()
                    && b[i] != b' '
                    && b[i] != b'>'
                    && b[i] != b'\t'
                    && b[i] != b'\n'
                    && b[i] != b'\r'
                {
                    i += 1;
                }
                std::str::from_utf8(&b[start..i]).unwrap_or("")
            }
        };
        out.push((name.to_string(), value.to_string()));
    }
}

fn find(b: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from >= b.len() {
        return None;
    }
    b[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

fn find_ignore_case(b: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from >= b.len() || needle.len() > b.len() {
        return None;
    }
    let lower = needle.to_ascii_lowercase();
    b[from..]
        .windows(lower.len())
        .position(|w| w.eq_ignore_ascii_case(&lower))
        .map(|p| p + from)
}

/// 解最常见的几个实体 + 数字实体。
///
/// 不解全表（HTML5 有 2231 个）：剩下那些原样保留，爬虫拿去喂文本分析也够。
pub fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'&' {
            let start = i;
            while i < b.len() && b[i] != b'&' {
                i += 1;
            }
            out.push_str(&s[start..i]);
            continue;
        }
        // 找最近的 `;`，窗口最多 12 字符（最长实体 `&#x10FFFF;`）
        let limit = (i + 12).min(b.len());
        match b[i + 1..limit].iter().position(|&c| c == b';') {
            Some(rel) => {
                let ent = &s[i + 1..i + 1 + rel];
                if let Some(ch) = decode_entity(ent) {
                    out.push(ch);
                } else {
                    out.push('&');
                    out.push_str(ent);
                    out.push(';');
                }
                i += rel + 2;
            }
            None => {
                out.push('&');
                i += 1;
            }
        }
    }
    out
}

fn decode_entity(ent: &str) -> Option<char> {
    match ent {
        "amp" => return Some('&'),
        "lt" => return Some('<'),
        "gt" => return Some('>'),
        "quot" => return Some('"'),
        "apos" => return Some('\''),
        "nbsp" => return Some(' '),
        _ => {}
    }
    if let Some(hex) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
        return u32::from_str_radix(hex, 16).ok().and_then(char::from_u32);
    }
    if let Some(dec) = ent.strip_prefix('#') {
        return dec.parse::<u32>().ok().and_then(char::from_u32);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_title_meta_and_links() {
        let html = r#"<!doctype html>
<html><head>
<title>  订单 &amp; 报表  </title>
<meta name="description" content="一分钟看懂&#x4E2D;文">
<link rel="canonical" href="https://a.com/orders">
<base href="https://a.com/v2/">
<script>var x = "<a href='/fake'>";</script>
<style>.a{color:red}</style>
</head><body>
<a href="/a?x=1" rel="nofollow">去 A</a>
<a href='b.html' class="x">去 <b>B</b></a>
<a href=/c?q=2>去 C</a>
</body></html>"#;
        let d = scan(html);
        assert_eq!(d.title, "订单 & 报表");
        assert_eq!(d.description, "一分钟看懂中文");
        assert_eq!(d.canonical.as_deref(), Some("https://a.com/orders"));
        assert_eq!(d.base.as_deref(), Some("https://a.com/v2/"));
        assert_eq!(d.links.len(), 3, "script 里的假链接不该被扫出来");
        assert_eq!(d.links[0].href, "/a?x=1");
        assert_eq!(d.links[0].text, "去 A");
        assert!(d.links[0].is_nofollow());
        assert_eq!(d.links[1].href, "b.html");
        assert_eq!(d.links[1].text, "去 B");
        assert!(!d.links[1].is_nofollow());
        assert_eq!(d.links[2].href, "/c?q=2");
        assert!(!d.text.contains("color:red"), "style 内容不算可见文本");
        assert!(!d.text.contains("var x"), "script 内容不算可见文本");
        assert!(d.text.starts_with("去 A"), "{}", d.text);
    }

    #[test]
    fn headings_and_text() {
        let d = scan("<h1>标题一</h1><p>正文<br>换行</p><h3>三级</h3>");
        assert_eq!(d.headings, vec![(1, "标题一".to_string()), (3, "三级".to_string())]);
        assert_eq!(d.headings_text(), "标题一 三级");
        assert_eq!(d.text, "标题一正文换行三级");
    }

    #[test]
    fn unclosed_tags_do_not_loop() {
        // 属性没有结束引号、标签没有 `>`
        let d = scan("<a href=\"/x\" <b>粗</b>");
        assert_eq!(d.links.len(), 1);
        assert_eq!(d.links[0].href, "/x");
        let d2 = scan("<a href=\"unterminated");
        assert_eq!(d2.links[0].href, "unterminated");
    }

    #[test]
    fn unescape_handles_dec_and_hex() {
        assert_eq!(unescape("a&amp;b"), "a&b");
        assert_eq!(unescape("&#65;&#x42;"), "AB");
        assert_eq!(unescape("&unknown;"), "&unknown;");
        assert_eq!(unescape("a&b"), "a&b", "裸 & 原样保留");
    }
}
