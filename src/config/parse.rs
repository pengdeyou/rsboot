//! ini 文本 → 键值对。
//!
//! 纯函数：不读文件、不看环境变量、不依赖 `Error`。这样它可以被
//! "从字符串加载"的测试直接覆盖，也让配置层的高层 API 保持简单。

/// 解析结果：`(完整键, 值)`。段名以 `.` 拼在键前；无段的键就是裸名。
///
/// 错误信息里带 `origin:line`（origin 通常是文件名），因为配置错误
/// 最常见的形态就是"某一行写错了"，光说"格式不对"没用。
pub fn parse(text: &str, origin: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    let mut section = String::new();

    for (idx, raw) in text.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        if line.starts_with('[') {
            let Some(sec) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
                return Err(format!(
                    "{origin}:{line_no} 段名没闭合，应为 `[名字]`：`{line}`"
                ));
            };
            let sec = sec.trim();
            if sec.is_empty()
                || !sec
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
            {
                return Err(format!(
                    "{origin}:{line_no} 段名只能含字母、数字、`_`、`-`：`{sec}`"
                ));
            }
            section = sec.to_string();
            continue;
        }

        let Some((k, v)) = line.split_once('=') else {
            return Err(format!(
                "{origin}:{line_no} 缺少 `=`：`{line}`。\
                 值里有 `#` 不需要转义（这里不按 TOML 处理注释）"
            ));
        };
        let key = k.trim();
        if key.is_empty() {
            return Err(format!("{origin}:{line_no} 键名是空的"));
        }
        let full = if section.is_empty() {
            key.to_string()
        } else {
            format!("{section}.{key}")
        };
        out.push((full, unquote(v.trim())));
    }
    Ok(out)
}

/// 去掉成对引号；不成对的引号原样保留（URL 里可能有）。
fn unquote(s: &str) -> String {
    for q in ['"', '\''] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_are_prefixed() {
        let kv = parse("[app]\nname = demo\n", "t.ini").unwrap();
        assert_eq!(kv, vec![("app.name".to_string(), "demo".to_string())]);
    }

    #[test]
    fn key_outside_section_is_bare() {
        let kv = parse("root = 1\n[app]\nname = demo\n", "t.ini").unwrap();
        assert_eq!(kv[0].0, "root");
        assert_eq!(kv[1].0, "app.name");
    }

    #[test]
    fn hash_inside_value_is_kept() {
        let kv = parse("[database]\ndefault = mysql://u:p#x@h/db\n", "t.ini").unwrap();
        assert_eq!(kv[0].1, "mysql://u:p#x@h/db");
    }

    #[test]
    fn quotes_are_stripped() {
        let kv = parse("[app]\ntitle = \"my demo\"\n", "t.ini").unwrap();
        assert_eq!(kv[0].1, "my demo");
        // 不成对的不动
        let kv = parse("[app]\nt = \"x\n", "t.ini").unwrap();
        assert_eq!(kv[0].1, "\"x");
    }

    #[test]
    fn errors_carry_file_and_line() {
        assert!(parse("[app]\nname demo\n", "a.ini")
            .unwrap_err()
            .contains("a.ini:2"));
        assert!(parse("[app\nx=1\n", "a.ini")
            .unwrap_err()
            .contains("没闭合"));
        assert!(parse("[a b]\nx=1\n", "a.ini")
            .unwrap_err()
            .contains("段名只能含"));
        assert!(parse("= 1\n", "a.ini").unwrap_err().contains("键名是空的"));
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let kv = parse("\n# c\n; c2\n[app]\n\nname = x\n", "t.ini").unwrap();
        assert_eq!(kv.len(), 1);
    }
}
