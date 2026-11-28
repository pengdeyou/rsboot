//! sitemap.xml 解析。
//!
//! 站点自己声明的"全站有哪些页"，比爬链接快得多——一个 sitemap 就是一份
//! 现成的 URL 清单。`robots.txt` 里的 `Sitemap:` 行和 `/sitemap.xml`
//! 默认位置是两个入口（[`Robots::sitemaps`](crate::robots::Robots::sitemaps)
//! 已经解析出前者）。
//!
//! 两种文档形态：
//!
//! - `<urlset>`——一页一条 `<url><loc>...`
//! - `<sitemapindex>`——本身不含页面，只指向下一层 sitemap（大站会分片，
//!   分片还能再分片，所以这里要能递归；调用方负责限层数）
//!
//! 只做 XML 的一个子集：按标签名扫 `<loc>`，不建 DOM、不管命名空间前缀
//! （`<ns:loc>` 也能认）。
//! 明确不支持 `.xml.gz`——解 gzip 要引库，和零依赖冲突。

use crate::html::unescape;

/// sitemap 里的一条。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Entry {
    pub loc: String,
    pub lastmod: Option<String>,
    /// 站点给的优先级提示（0.0–1.0）。v0.2 只解析，不入队、不排序。
    pub priority: Option<String>,
}

/// 解析结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sitemap {
    /// 这是 `sitemapindex` 而不是 `urlset`
    pub is_index: bool,
    pub entries: Vec<Entry>,
}

impl Sitemap {
    /// 只有 URL 清单，方便直接喂给 frontier。
    pub fn locs(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.loc.clone()).collect()
    }
}

/// 解析一份 sitemap.xml。
///
/// 判别 `urlset` / `sitemapindex` 只看根元素名字——两者不能互相嵌套，
/// 这个判断是可靠的。
pub fn parse(xml: &str) -> Sitemap {
    let mut out = Sitemap {
        is_index: match root_name(xml) {
            Some(n) => n.eq_ignore_ascii_case("sitemapindex"),
            None => false,
        },
        ..Default::default()
    };

    let b = xml.as_bytes();
    let mut i = 0usize;
    let mut cur: Option<Entry> = None;

    while i < b.len() {
        let Some(open) = find_byte(b, i, b'<') else {
            break;
        };
        if b[open..].starts_with(b"<!--") {
            match find_sub(b, open + 4, b"-->") {
                Some(end) => {
                    i = end + 3;
                    continue;
                }
                None => break,
            }
        }
        let Some(gt) = find_byte(b, open, b'>') else {
            break;
        };
        let raw = &xml[open + 1..gt];
        let closing = raw.starts_with('/');
        let trimmed = if closing {
            raw[1..].trim_start()
        } else {
            raw.trim_start()
        };
        let name_end = trimmed
            .find(|c: char| c.is_whitespace() || c == '/')
            .unwrap_or(trimmed.len());
        let name = &trimmed[..name_end];
        // 命名空间前缀：`sm:loc` → `loc`
        let short = name.rsplit(':').next().unwrap_or(name);
        i = gt + 1;

        // 一个条目容器的开/闭：urlset 用 `<url>`，sitemapindex 用 `<sitemap>`
        let is_item = short.eq_ignore_ascii_case("url") || short.eq_ignore_ascii_case("sitemap");
        if is_item {
            if closing {
                if let Some(e) = cur.take() {
                    if !e.loc.trim().is_empty() {
                        out.entries.push(e);
                    }
                }
            } else {
                cur = Some(Entry::default());
            }
            continue;
        }
        if closing {
            continue;
        }

        // 文本元素：`<loc>https://a.com/x</loc>`
        let is_text = ["loc", "lastmod", "priority"]
            .iter()
            .any(|k| short.eq_ignore_ascii_case(k));
        if !is_text {
            continue;
        }
        let Some(next) = find_byte(b, i, b'<') else {
            break;
        };
        if !b[next..].starts_with(b"</") {
            // `<loc>` 后面跟着子标签：不是我们要的形态，跳过
            i = next;
            continue;
        }
        let text = unescape(xml[i..next].trim());
        let e = cur.get_or_insert_with(Entry::default);
        if short.eq_ignore_ascii_case("loc") {
            e.loc = text;
        } else if short.eq_ignore_ascii_case("lastmod") {
            e.lastmod = Some(text);
        } else {
            e.priority = Some(text);
        }
        i = next;
    }
    out
}

/// 根元素名（跳过 `<?xml?>` / `<!doctype>` / 注释）。
fn root_name(xml: &str) -> Option<String> {
    let b = xml.as_bytes();
    let mut i = 0usize;
    while let Some(p) = find_byte(b, i, b'<') {
        if b[p..].starts_with(b"<?") {
            i = find_sub(b, p + 2, b"?>")? + 2;
            continue;
        }
        if b[p..].starts_with(b"<!--") {
            i = find_sub(b, p + 4, b"-->")? + 3;
            continue;
        }
        if b[p..].starts_with(b"<!") {
            i = find_byte(b, p + 2, b'>')? + 1;
            continue;
        }
        let end = find_byte(b, p, b'>')?;
        let raw = xml[p + 1..end].trim_start();
        if raw.starts_with('/') {
            return None;
        }
        let n = raw
            .find(|c: char| c.is_whitespace() || c == '/')
            .unwrap_or(raw.len());
        let name = &raw[..n];
        return Some(name.rsplit(':').next().unwrap_or(name).to_string());
    }
    None
}

fn find_byte(b: &[u8], from: usize, needle: u8) -> Option<usize> {
    if from >= b.len() {
        return None;
    }
    b[from..].iter().position(|&c| c == needle).map(|p| p + from)
}

fn find_sub(b: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from >= b.len() || needle.len() > b.len() {
        return None;
    }
    b[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urlset() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <url><loc>https://a.com/</loc><lastmod>2026-01-01</lastmod><priority>1.0</priority></url>
  <!-- 注释里的不该被抓到：<url><loc>https://a.com/comment</loc></url> -->
  <url><loc>https://a.com/about?a=1&amp;b=2</loc></url>
  <url><loc></loc></url>
</urlset>"#;
        let s = parse(xml);
        assert!(!s.is_index);
        assert_eq!(s.entries.len(), 2, "空 loc 要丢掉");
        assert_eq!(s.entries[0].loc, "https://a.com/");
        assert_eq!(s.entries[0].lastmod.as_deref(), Some("2026-01-01"));
        assert_eq!(s.entries[0].priority.as_deref(), Some("1.0"));
        assert_eq!(s.entries[1].loc, "https://a.com/about?a=1&b=2", "实体要解");
    }

    #[test]
    fn parses_index() {
        let xml = r#"<sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
            <sitemap><loc>https://a.com/sitemap-1.xml</loc></sitemap>
            <sitemap><loc>https://a.com/sitemap-2.xml</loc></sitemap>
        </sitemapindex>"#;
        let s = parse(xml);
        assert!(s.is_index);
        assert_eq!(
            s.locs(),
            vec!["https://a.com/sitemap-1.xml", "https://a.com/sitemap-2.xml"]
        );
    }

    #[test]
    fn tolerates_namespaces_and_attrs() {
        let xml =
            r#"<sm:urlset xmlns:sm="x"><sm:url><sm:loc lang="zh">https://a.com/中文</sm:loc></sm:url></sm:urlset>"#;
        let s = parse(xml);
        assert_eq!(s.entries.len(), 1);
        assert_eq!(s.entries[0].loc, "https://a.com/中文");
    }

    #[test]
    fn garbage_does_not_panic() {
        assert_eq!(parse("").entries.len(), 0);
        assert_eq!(parse("not xml at all").entries.len(), 0);
        assert_eq!(parse("<urlset><url><loc>https://a.com").entries.len(), 0);
        let s = parse("<urlset><url><loc>http://a.com/x</loc></url></urlset>");
        assert_eq!(s.entries[0].loc, "http://a.com/x");
    }
}
