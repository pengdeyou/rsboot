//! robots.txt 解析（RFC 9309）。
//!
//! 只做爬虫真正用得上的：哪些路径不许抓、多久抓一次、sitemap 在哪。
//! 按 RFC 取**最长匹配**：`Disallow: /` 和 `Allow: /public` 同时存在时，
//! `/public/a` 归 Allow 管，`/private/a` 归 Disallow 管。
//! 等长时 Allow 胜（Google 的口径，也是事实标准）。

use std::time::Duration;

/// 一条规则。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    allow: bool,
    /// 原始 pattern，可能含 `*` 和结尾的 `$`
    pattern: String,
}

/// 解析后的 robots.txt。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Robots {
    rules: Vec<Rule>,
    /// `Crawl-delay`，秒
    pub crawl_delay: Option<Duration>,
    pub sitemaps: Vec<String>,
    /// 该 UA 命中的是哪个组（`*` 或具体名字），仅供诊断
    pub agent: Option<String>,
}

impl Robots {
    /// 没有 robots.txt（404 / 连不上）时的策略：全放行。
    ///
    /// 这是爬虫界的默认：抓不到规则就当作没规则，不是"全禁"。
    pub fn allow_all() -> Self {
        Self::default()
    }

    /// 全部禁止。给"用户明确要求遵守"的场景用。
    pub fn disallow_all() -> Self {
        Self {
            rules: vec![Rule {
                allow: false,
                pattern: "/".to_string(),
            }],
            ..Default::default()
        }
    }

    /// 解析一份 robots.txt，按 `user_agent` 选组。
    pub fn parse(txt: &str, user_agent: &str) -> Self {
        #[derive(Default)]
        struct Group {
            agents: Vec<String>,
            rules: Vec<Rule>,
            crawl_delay: Option<Duration>,
        }

        let mut robots = Self::default();
        let mut groups: Vec<Group> = Vec::new();
        let mut cur: Option<Group> = None;
        // 组结束的标志：出现过规则之后又出现 User-agent
        let mut seen_rule = false;

        for line in txt.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();
            match key.as_str() {
                "user-agent" | "useragent" => {
                    if seen_rule {
                        if let Some(g) = cur.take() {
                            groups.push(g);
                        }
                        seen_rule = false;
                    }
                    let g = cur.get_or_insert_with(Group::default);
                    g.agents.push(value.to_ascii_lowercase());
                }
                "disallow" => {
                    seen_rule = true;
                    if value.is_empty() {
                        continue; // `Disallow:` 空值是"什么都不禁"
                    }
                    cur.get_or_insert_with(Group::default).rules.push(Rule {
                        allow: false,
                        pattern: value.to_string(),
                    });
                }
                "allow" => {
                    seen_rule = true;
                    if value.is_empty() {
                        continue;
                    }
                    cur.get_or_insert_with(Group::default).rules.push(Rule {
                        allow: true,
                        pattern: value.to_string(),
                    });
                }
                "crawl-delay" => {
                    if let Ok(secs) = value.parse::<f64>() {
                        if let Some(g) = cur.as_mut() {
                            g.crawl_delay = Some(Duration::from_secs_f64(secs.max(0.0)));
                        }
                    }
                }
                "sitemap" => robots.sitemaps.push(value.to_string()),
                _ => {}
            }
        }
        if let Some(g) = cur {
            groups.push(g);
        }

        let ua = user_agent.to_ascii_lowercase();
        // 选择顺序：精确/前缀命中的具体组优先，否则 `*`
        let specific = groups.iter().find(|g| {
            g.agents
                .iter()
                .any(|a| *a != "*" && !a.is_empty() && ua.starts_with(a.as_str()))
        });
        let picked = specific.or_else(|| {
            groups
                .iter()
                .find(|g| g.agents.iter().any(|a| a == "*"))
        });
        if let Some(g) = picked {
            robots.rules = g.rules.clone();
            robots.crawl_delay = g.crawl_delay;
            robots.agent = g.agents.first().cloned();
        }
        robots
    }

    /// 这个路径能不能抓。`path` 要带 query（robots 规则里的 query 是有意义的）。
    pub fn allowed(&self, path: &str) -> bool {
        let mut best: Option<(usize, bool)> = None; // (匹配长度, 是否 allow)
        for r in &self.rules {
            if let Some(len) = match_pattern(&r.pattern, path) {
                match best {
                    // 更长者胜；等长时 Allow 胜
                    Some((bl, ba)) if bl > len || (bl == len && ba && !r.allow) => {}
                    _ => best = Some((len, r.allow)),
                }
            }
        }
        best.map(|(_, allow)| allow).unwrap_or(true)
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// pattern 与路径的匹配：支持 `*`（任意中间串）和结尾 `$`（必须整串结束）。
/// 返回匹配长度——RFC 用它做优先级排序。
fn match_pattern(pattern: &str, path: &str) -> Option<usize> {
    if pattern.is_empty() {
        return None;
    }
    let (pat, anchored) = match pattern.strip_suffix('$') {
        Some(p) => (p, true),
        None => (pattern, false),
    };
    if !pat.contains('*') {
        let ok = if anchored {
            path == pat
        } else {
            path.starts_with(pat)
        };
        return ok.then_some(pattern.len());
    }
    // 带 `*`：按 `*` 切段，逐段贪心前进
    let parts: Vec<&str> = pat.split('*').collect();
    let first = parts[0];
    let last = *parts.last().unwrap();
    if !path.starts_with(first) {
        return None;
    }
    let mut pos = first.len();
    for mid in &parts[1..parts.len().saturating_sub(1)] {
        if mid.is_empty() {
            continue;
        }
        let idx = path[pos..].find(mid)?;
        pos += idx + mid.len();
    }
    if anchored {
        if path.len() < pos + last.len() || !path.ends_with(last) {
            return None;
        }
        if path.len() - last.len() < pos {
            return None;
        }
    } else if !path[pos..].contains(last) && !last.is_empty() {
        return None;
    }
    Some(pattern.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TXT: &str = "\
# 注释行
User-agent: *
Disallow: /admin
Disallow: /tmp/
Allow: /admin/public
Crawl-delay: 0.5

User-agent: rsboot-claw
Disallow: /only-for-claw
";

    #[test]
    fn picks_wildcard_group_for_other_agents() {
        let r = Robots::parse(TXT, "my-crawler/1.0");
        assert_eq!(r.agent.as_deref(), Some("*"));
        assert_eq!(r.crawl_delay, Some(Duration::from_millis(500)));
        assert!(!r.allowed("/admin"));
        assert!(!r.allowed("/admin/users"));
        assert!(r.allowed("/admin/public"), "更长的 Allow 应该胜出");
        assert!(r.allowed("/public"));
        assert!(!r.allowed("/tmp/a"));
        assert!(r.allowed("/only-for-claw"), "不该套用别的组的规则");
    }

    #[test]
    fn picks_specific_group_by_prefix() {
        let r = Robots::parse(TXT, "Rsboot-Claw/0.1");
        assert_eq!(r.agent.as_deref(), Some("rsboot-claw"));
        assert!(!r.allowed("/only-for-claw"));
        assert!(r.allowed("/admin"), "具体组没写 /admin，就应该放行");
    }

    #[test]
    fn wildcards_and_anchors() {
        let r = Robots::parse("User-agent: *\nDisallow: /*.pdf$\nDisallow: /search*q=\n", "x");
        assert!(!r.allowed("/a/b.pdf"));
        assert!(r.allowed("/a/b.pdf.html"), "$ 要求整串结尾");
        assert!(!r.allowed("/search?q=rust"));
        assert!(r.allowed("/search"));
    }

    #[test]
    fn equal_length_allow_wins() {
        let r = Robots::parse("User-agent: *\nDisallow: /x\nAllow: /x\n", "x");
        assert!(r.allowed("/x"));
    }

    #[test]
    fn empty_disallow_means_allow_all() {
        let r = Robots::parse("User-agent: *\nDisallow:\n", "x");
        assert!(r.is_empty());
        assert!(r.allowed("/anything"));
    }

    #[test]
    fn allow_all_and_disallow_all() {
        assert!(Robots::allow_all().allowed("/x"));
        assert!(!Robots::disallow_all().allowed("/x"));
    }

    #[test]
    fn crlf_and_bom_tolerated() {
        let r = Robots::parse("User-agent: *\r\nDisallow: /a\r\n", "x");
        assert!(!r.allowed("/a"));
    }
}
