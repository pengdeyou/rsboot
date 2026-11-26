//! 路径模板与百分号解码。
//!
//! 模板语法只有 `{名字}`：`/users/{id}`、`/users/{id}/posts/{post_id}`。
//! 不做通配符（`{rest:path}`）——一旦支持，路由优先级就变成需要人记的规则，
//! 而 FastAPI 也一样只有 `{param}` 与 `{path:path}` 两种，我们把后者留到有真实需求时。
//!
//! ## 匹配成本
//!
//! 每条路由只在**首段字面量**相同的情况下才会被完整比较（见 `router.rs` 的分桶），
//! 所以 `/users/xxx` 不会去试 `/orders/{id}` 的模板。

/// 模板中的一段。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seg {
    Lit(String),
    Param(String),
}

/// 一条编译好的路径模板。
#[derive(Debug, Clone)]
pub struct PathTemplate {
    /// 原始文本，用于 OpenAPI 与错误信息
    pub raw: String,
    segs: Vec<Seg>,
    /// 是否含参数（不含参数的路由可以走快速比较）
    pub has_params: bool,
    /// 参数个数。命中时才 `Vec::with_capacity`，别为拿长度再遍历一遍。
    param_count: usize,
}

impl PathTemplate {
    pub fn parse(raw: &str) -> Result<Self, String> {
        if !raw.starts_with('/') {
            return Err(format!("路径必须以 `/` 开头：`{raw}`"));
        }
        if raw.len() > 1 && raw.ends_with('/') {
            return Err(format!(
                "路径不要以 `/` 结尾（`/a/` 与 `/a` 会变成两条路由）：`{raw}`"
            ));
        }
        // 根路径：`trim_start_matches('/')` 会得到空串，被下面的空段检查误判
        if raw == "/" {
            return Ok(Self {
                raw: "/".to_string(),
                segs: Vec::new(),
                has_params: false,
                param_count: 0,
            });
        }
        let mut segs = Vec::new();
        let mut names: Vec<&str> = Vec::new();
        for part in raw.trim_start_matches('/').split('/') {
            if part.is_empty() {
                return Err(format!("路径里有连续 `//`：`{raw}`"));
            }
            if let Some(name) = part.strip_prefix('{').and_then(|p| p.strip_suffix('}')) {
                if name.is_empty() {
                    return Err(format!("路径参数名是空的：`{raw}`"));
                }
                if !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
                {
                    return Err(format!(
                        "路径参数名只能用字母、数字、`_`：`{{{name}}}` 在 `{raw}` 里"
                    ));
                }
                if names.contains(&name) {
                    return Err(format!("路径参数 `{name}` 在 `{raw}` 里重复"));
                }
                names.push(name);
                segs.push(Seg::Param(name.to_string()));
            } else {
                if part.contains('{') || part.contains('}') {
                    return Err(format!(
                        "路径段 `{part}` 里的花括号不成对（参数必须整段：`{{id}}`，不支持 `a{{id}}b`）"
                    ));
                }
                segs.push(Seg::Lit(part.to_string()));
            }
        }
        let has_params = !names.is_empty();
        Ok(Self {
            raw: raw.to_string(),
            param_count: names.len(),
            segs,
            has_params,
        })
    }

    /// 逐段视图：`Some(字面量)` 或 `None`（参数段）。给路由树建索引用。
    pub fn segments(&self) -> impl Iterator<Item = Option<&str>> {
        self.segs.iter().map(|s| match s {
            Seg::Lit(l) => Some(l.as_str()),
            Seg::Param(_) => None,
        })
    }

    /// 首段字面量（用于分桶）。首段是参数时返回 `None`。
    pub fn first_literal(&self) -> Option<&str> {
        match self.segs.first() {
            Some(Seg::Lit(s)) => Some(s.as_str()),
            _ => None,
        }
    }

    /// 段数（用于快速排除）。
    pub fn len(&self) -> usize {
        self.segs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.segs.is_empty()
    }

    /// 模板里的参数名列表（OpenAPI 与"取唯一参数"都要用）。
    pub fn param_names(&self) -> Vec<&str> {
        self.segs
            .iter()
            .filter_map(|s| match s {
                Seg::Param(n) => Some(n.as_str()),
                Seg::Lit(_) => None,
            })
            .collect()
    }

    /// 匹配已解码的请求路径。命中时返回参数。
    ///
    /// 参数值已经做过百分号解码（`%20` → 空格），所以这里拿到的是业务可用的值。
    pub fn matches(&self, path: &str) -> Option<Vec<(String, String)>> {
        self.matches_inner(path, false)
    }

    /// 调用方已经保证"段数与自己相同"时用这个：省掉一次全路径的 `/` 计数。
    pub(crate) fn matches_with_len(&self, path: &str) -> Option<Vec<(String, String)>> {
        self.matches_inner(path, true)
    }

    fn matches_inner(&self, path: &str, len_known: bool) -> Option<Vec<(String, String)>> {
        let trimmed = path.strip_prefix('/').unwrap_or(path);
        // 根路径 `/`
        if self.segs.is_empty() {
            return if trimmed.is_empty() {
                Some(Vec::new())
            } else {
                None
            };
        }
        if !self.has_params {
            // 无参数：`raw` 就是这条路径的字面量，直接比，不再逐段拼一个 String
            // 出来再比。早先的写法每匹配一次静态路由就要 `collect` 一个 Vec、
            // 再 `join` 出一个 String——只为做一次字符串相等判断。
            let expect = self.raw.strip_prefix('/').unwrap_or(&self.raw);
            return if trimmed == expect {
                Some(Vec::new())
            } else {
                None
            };
        }
        // 先按段数排除。这一步不分配：早先是 `trimmed.split('/').collect()`
        // 成一个 Vec，再不匹配也要为它买一次堆。
        if !len_known && count_segments(trimmed) != self.segs.len() {
            return None;
        }
        let mut params = Vec::with_capacity(self.param_count);
        let mut parts = trimmed.split('/');
        for seg in self.segs.iter() {
            let Some(part) = parts.next() else { return None };
            match seg {
                Seg::Lit(l) => {
                    if l.as_str() != part {
                        return None;
                    }
                }
                Seg::Param(n) => {
                    // 空段（`/users//1`）不算命中：那是畸形路径，不该被当成参数 ""
                    if part.is_empty() {
                        return None;
                    }
                    params.push((n.clone(), percent_decode(part)));
                }
            }
        }
        // 段数已经对齐过，理论上不会多出片段；多出来说明计数与切分口径不一致，
        // 宁可判不命中，也不要把半截路径当命中。
        if parts.next().is_some() {
            return None;
        }
        Some(params)
    }
}

/// 数 `a/b/c` 有几段，不分配。
fn count_segments(trimmed: &str) -> usize {
    if trimmed.is_empty() {
        return 0;
    }
    1 + trimmed.bytes().filter(|b| *b == b'/').count()
}

/// `%41` → `A`，`+` **不**当空格（路径里 `+` 是字面加号，只有 query 里才是空格）。
pub fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hi = hex(b[i + 1]);
            let lo = hex(b[i + 2]);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    // 非法编码按原样保留（不丢数据，也不谎报错误）
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> PathTemplate {
        PathTemplate::parse(s).unwrap()
    }

    /// 量"路由一多，分派退化成什么"。不是断言，是给"要不要换前缀树"
    /// 这个决定提供数字：200 条同首段、同段数的路由，命中最后一个
    /// （最坏情况扫全表）。
    #[test]
    fn bench_miss_scan() {
        let statics: Vec<PathTemplate> =
            (0..200).map(|i| t(&format!("/api/v1/route{i}"))).collect();
        let params: Vec<PathTemplate> =
            (0..200).map(|i| t(&format!("/api/v1/seg{i}/{{id}}"))).collect();
        let n = 2000usize;

        let path = "/api/v1/route199";
        let started = std::time::Instant::now();
        let mut hits = 0usize;
        for _ in 0..n {
            for tp in &statics {
                if tp.matches(std::hint::black_box(path)).is_some() {
                    hits += 1;
                    break;
                }
            }
        }
        let static_ns = started.elapsed().as_nanos() as f64 / n as f64;

        let ppath = "/api/v1/seg199/42";
        let started = std::time::Instant::now();
        let mut phits = 0usize;
        for _ in 0..n {
            for tp in &params {
                if tp.matches(std::hint::black_box(ppath)).is_some() {
                    phits += 1;
                    break;
                }
            }
        }
        let param_ns = started.elapsed().as_nanos() as f64 / n as f64;

        println!(
            "200 条路由的最坏扫描：静态 {static_ns:.0} ns/请求（命中 {hits}），参数 {param_ns:.0} ns/请求（命中 {phits}）"
        );
    }

    #[test]
    fn root_matches_only_root() {
        let r = t("/");
        assert_eq!(r.len(), 0);
        assert!(r.is_empty());
        assert_eq!(r.matches("/"), Some(vec![]));
        assert_eq!(r.matches("/x"), None);
    }

    #[test]
    fn static_path_matches_exactly() {
        let p = t("/health");
        assert_eq!(p.matches("/health"), Some(vec![]));
        assert_eq!(p.matches("/health/"), None);
        assert_eq!(p.matches("/healthz"), None);
        assert_eq!(p.first_literal(), Some("health"));
    }

    #[test]
    fn param_capture() {
        let p = t("/users/{id}");
        let got = p.matches("/users/42").unwrap();
        assert_eq!(got, vec![("id".to_string(), "42".to_string())]);
        assert_eq!(p.matches("/users/"), None, "空参数不算命中");
        assert_eq!(p.matches("/users/42/posts"), None);
    }

    #[test]
    fn two_params() {
        let p = t("/users/{id}/posts/{post_id}");
        let got = p.matches("/users/7/posts/9").unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[1], ("post_id".to_string(), "9".to_string()));
        assert_eq!(p.first_literal(), Some("users"));
    }

    #[test]
    fn path_param_is_percent_decoded() {
        let p = t("/tags/{name}");
        let got = p.matches("/tags/a%20b").unwrap();
        assert_eq!(got[0].1, "a b");
    }

    #[test]
    fn plus_is_literal_in_path() {
        let p = t("/tags/{name}");
        assert_eq!(p.matches("/tags/a+b").unwrap()[0].1, "a+b");
    }

    #[test]
    fn malformed_escape_is_left_alone() {
        assert_eq!(percent_decode("a%zzb"), "a%zzb");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%E4%B8%AD"), "中");
    }

    #[test]
    fn template_validation_errors() {
        assert!(PathTemplate::parse("users").unwrap_err().contains("以 `/` 开头"));
        assert!(PathTemplate::parse("/a/").unwrap_err().contains("不要以"));
        assert!(PathTemplate::parse("/a//b").unwrap_err().contains("连续"));
        assert!(PathTemplate::parse("/a/{}").unwrap_err().contains("空的"));
        assert!(PathTemplate::parse("/a/{x y}").unwrap_err().contains("只能用"));
        assert!(PathTemplate::parse("/{id}/{id}")
            .unwrap_err()
            .contains("重复"));
        assert!(PathTemplate::parse("/a/b{c}")
            .unwrap_err()
            .contains("不成对"));
    }
}
