//! YAML 子集解析器（无依赖）。
//!
//! 支持：嵌套映射、标量（字符串/整数/布尔/null）、单双引号转义、`#` 注释、
//! 列表（编码为 `key.0`、`key.1` 索引键）。
//!
//! 不支持（明确报错，绝不静默忽略）：Tab 缩进、多行字符串 `|` / `>`、
//! 锚点 `&` / 别名 `*` / 合并 `<<`、多文档 `---`、流式 `[a, b]` / `{a: b}`。
//!
//! 为什么自研：`serde_yaml` 已归档，继任者分裂；而配置只需要上面这一小撮能力。
//! 200 行换掉一个依赖，划算。代价是超集写法会报错——但报错比静默误读安全。

/// 输出与 ini 解析器完全同形：平坦的 `(完整键, 值)`。
/// 这样配置分层逻辑只写一份。
pub fn parse(text: &str, origin: &str) -> Result<Vec<(String, String)>, String> {
    let mut out: Vec<(String, String)> = Vec::new();
    // 当前键路径与每级的缩进宽度
    let mut path: Vec<String> = Vec::new();
    let mut indents: Vec<usize> = Vec::new();
    // 列表键的已分配索引
    let mut list_next: std::collections::HashMap<String, usize> = Default::default();
    let mut seen_doc_marker = false;

    for (idx, raw) in text.lines().enumerate() {
        let line_no = idx + 1;

        let head: &str = &raw[..raw.len() - raw.trim_start().len()];
        if head.contains('\t') {
            return Err(format!(
                "{origin}:{line_no} 缩进里有 Tab。YAML 规范禁止用 Tab 缩进，请换成空格"
            ));
        }

        let content = strip_comment(raw.trim());
        if content.is_empty() {
            continue;
        }

        if content == "---" {
            if seen_doc_marker || !out.is_empty() {
                return Err(format!(
                    "{origin}:{line_no} 只接受单文档配置，不支持多个 `---` 分隔的文档"
                ));
            }
            seen_doc_marker = true;
            continue;
        }
        if content == "..." {
            continue;
        }
        if content.starts_with('&') || content.starts_with('*') || content.starts_with("<<" ) {
            return Err(format!(
                "{origin}:{line_no} 不支持锚点/别名/合并（`&`、`*`、`<<`）。重复的配置请直接写两遍"
            ));
        }
        if content.contains("|") && content.trim_end().ends_with('|') {
            return Err(format!(
                "{origin}:{line_no} 不支持多行字符串 `|`。把内容写成单行，或用多个键"
            ));
        }
        if content.trim_end().ends_with('>') {
            return Err(format!(
                "{origin}:{line_no} 不支持折叠字符串 `>`。把内容写成单行"
            ));
        }
        if content.starts_with('[') || content.starts_with('{') {
            return Err(format!(
                "{origin}:{line_no} 不支持流式写法 `[...]`/`{{...}}`，请用 `- ` 逐项列"
            ));
        }

        let indent = head.len();

        // 列表项：归属"当前路径"，索引递增
        if let Some(item) = content.strip_prefix("- ") {
            if path.is_empty() {
                return Err(format!(
                    "{origin}:{line_no} 列表项前面没有键。列表必须挂在某个键下面"
                ));
            }
            if looks_like_mapping(item) {
                return Err(format!(
                    "{origin}:{line_no} 不支持列表里的映射项。改成 `键名:` 再逐条 `- `"
                ));
            }
            let owner = path.join(".");
            let i = list_next.entry(owner.clone()).or_insert(0);
            out.push((format!("{owner}.{i}"), unquote(item.trim())));
            *i += 1;
            continue;
        }
        if content == "-" {
            return Err(format!("{origin}:{line_no} 空列表项"));
        }

        // 普通键
        let (k, v) = content.split_once(':').ok_or_else(|| {
            format!("{origin}:{line_no} 既不是 `键: 值` 也不是 `- 项`：`{content}`")
        })?;
        let key = k.trim();
        if key.is_empty() {
            return Err(format!("{origin}:{line_no} 键名是空的"));
        }
        if !key
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
        {
            return Err(format!(
                "{origin}:{line_no} 键名只能含字母、数字、`_`、`-`、`.`：`{key}`"
            ));
        }
        if key.contains(' ') {
            return Err(format!("{origin}:{line_no} 键名不能含空格：`{key}`"));
        }

        // 同级或更浅：弹出到上一层
        while indents.last().is_some_and(|&i| i >= indent) {
            indents.pop();
            path.pop();
        }

        let vraw = v.trim();
        // 锚点/别名/合并在值里的形态也拒掉（顶格那种上面已拦过）
        if vraw.starts_with('&') || vraw.starts_with('*') || vraw.starts_with("<< ") {
            return Err(format!(
                "{origin}:{line_no} 不支持锚点/别名/合并（`&`、`*`、`<<`）。重复的配置请直接写两遍"
            ));
        }
        if vraw.starts_with('[') || vraw.starts_with('{') {
            return Err(format!(
                "{origin}:{line_no} 不支持流式写法 `[...]`/`{{...}}`，请用 `- ` 逐项列"
            ));
        }
        if vraw.is_empty() {
            indents.push(indent);
            path.push(key.to_string());
        } else {
            let full = if path.is_empty() {
                key.to_string()
            } else {
                format!("{}.{}", path.join("."), key)
            };
            out.push((full, unquote(vraw)));
        }
    }

    Ok(out)
}

/// 列表项是不是映射（`- key: value`）？
///
/// 不能只看"含冒号"：`- http://a` 也含冒号。YAML 里映射的冒号后面必须
/// 是空格或行尾，`http://` 后面是 `/`，所以不是映射。
fn looks_like_mapping(item: &str) -> bool {
    match item.find(':') {
        Some(pos) => {
            let after = &item[pos + 1..];
            after.is_empty() || after.starts_with(' ')
        }
        None => false,
    }
}

/// 剥掉行尾注释。`#` 只有在行首或前面是空白时才算注释——
/// 否则 `mysql://u:p#x@h/db` 这种值会被截断。
fn strip_comment(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'"' | b'\'' => {
                if quote == Some(c) {
                    quote = None;
                } else if quote.is_none() {
                    quote = Some(c);
                }
            }
            b'\\' if quote == Some(b'"') => {
                i += 2;
                continue;
            }
            // `#` 只有在行首或前面是空白时才算注释，否则是值的一部分
            b'#' if quote.is_none() && (i == 0 || bytes[i - 1] == b' ' || bytes[i - 1] == b'\t') => {
                return &s[..i];
            }
            _ => {}
        }
        i += 1;
    }
    s
}

fn unquote(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        let inner = &s[1..s.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        }
        return out;
    }
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        return s[1..s.len() - 1].replace("''", "'");
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kvs(text: &str) -> Vec<(String, String)> {
        parse(text, "t.yaml").unwrap()
    }

    fn get<'a>(v: &'a [(String, String)], k: &str) -> &'a str {
        &v.iter().find(|(n, _)| n == k).unwrap_or_else(|| panic!("缺 {k}")).1
    }

    #[test]
    fn nested_mapping_flattens_with_dots() {
        let v = kvs("database:\n  default: sqlite:///app.db\n  report: mysql://h/r\n");
        assert_eq!(get(&v, "database.default"), "sqlite:///app.db");
        assert_eq!(get(&v, "database.report"), "mysql://h/r");
    }

    #[test]
    fn sibling_keys_at_same_indent_pop_correctly() {
        let v = kvs("app:\n  name: demo\n  port: 8080\nhttp:\n  bind: 0.0.0.0:80\n");
        assert_eq!(v.len(), 3);
        assert_eq!(get(&v, "app.name"), "demo");
        assert_eq!(get(&v, "app.port"), "8080");
        assert_eq!(get(&v, "http.bind"), "0.0.0.0:80");
    }

    #[test]
    fn lists_become_indexed_keys() {
        let v = kvs("http:\n  cors:\n    allow_origin:\n      - http://a\n      - http://b\n");
        assert_eq!(get(&v, "http.cors.allow_origin.0"), "http://a");
        assert_eq!(get(&v, "http.cors.allow_origin.1"), "http://b");
    }

    #[test]
    fn list_at_same_indent_as_key_is_allowed() {
        let v = kvs("hosts:\n- a\n- b\n");
        assert_eq!(get(&v, "hosts.0"), "a");
        assert_eq!(get(&v, "hosts.1"), "b");
    }

    #[test]
    fn comment_needs_leading_space() {
        let v = kvs("app:\n  url: mysql://u:p#x@h/db   # 真注释\n  n: 1 # c\n");
        assert_eq!(get(&v, "app.url"), "mysql://u:p#x@h/db");
        assert_eq!(get(&v, "app.n"), "1");
    }

    #[test]
    fn hash_inside_quotes_is_kept() {
        let v = kvs("app:\n  s: \"a # b\"\n");
        assert_eq!(get(&v, "app.s"), "a # b");
    }

    #[test]
    fn quotes_and_escapes() {
        let v = kvs("a:\n  d: \"x\\ny\"\n  s: 'it''s'\n  e: \"\"\n");
        assert_eq!(get(&v, "a.d"), "x\ny");
        assert_eq!(get(&v, "a.s"), "it's");
        assert_eq!(get(&v, "a.e"), "");
    }

    #[test]
    fn single_doc_marker_is_fine_multiple_is_not() {
        assert!(parse("---\na:\n  b: 1\n", "t.yaml").is_ok());
        assert!(parse("---\na: 1\n---\nb: 2\n", "t.yaml")
            .unwrap_err()
            .contains("单文档"));
    }

    #[test]
    fn tab_indent_is_rejected_with_line_number() {
        let e = parse("a:\n\tb: 1\n", "t.yaml").unwrap_err();
        assert!(e.contains("t.yaml:2"), "{e}");
        assert!(e.contains("Tab"), "{e}");
    }

    #[test]
    fn unsupported_constructs_are_rejected_not_ignored() {
        for (src, hint) in [
            ("a:\n  b: |\n    x\n", "多行字符串"),
            ("a:\n  b: >\n    x\n", "折叠字符串"),
            ("a: &anchor 1\n", "锚点"),
            ("a: *ref\n", "锚点"),
            ("a:\n  b: [1, 2]\n", "流式"),
            ("a:\n  b: {x: 1}\n", "流式"),
            ("a:\n  - x: 1\n", "列表里的映射"),
        ] {
            let e = parse(src, "t.yaml").unwrap_err();
            assert!(e.contains(hint), "期望提到「{hint}」，实际：{e}");
        }
    }

    #[test]
    fn list_item_without_key_is_rejected() {
        assert!(parse("- a\n", "t.yaml").unwrap_err().contains("前面没有键"));
    }

    #[test]
    fn bad_key_characters_are_rejected() {
        assert!(parse("a b: 1\n", "t.yaml").is_err());
        assert!(parse(": 1\n", "t.yaml").unwrap_err().contains("键名是空的"));
        assert!(parse("a: 1\nnope\n", "t.yaml").is_err());
    }

    #[test]
    fn deep_nesting_after_sibling() {
        let v = kvs(
            "svc:\n  db:\n    pool:\n      min: 1\n      max: 9\n  http:\n    port: 80\n",
        );
        assert_eq!(get(&v, "svc.db.pool.min"), "1");
        assert_eq!(get(&v, "svc.db.pool.max"), "9");
        assert_eq!(get(&v, "svc.http.port"), "80");
    }
}
