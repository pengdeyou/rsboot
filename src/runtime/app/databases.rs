//! 数据库的收集、默认库推断与打开。
//!
//! 单独一个文件是因为这里有三件容易写错的事：配置文件与代码注册的**合并顺序**、
//! 默认库的**推断规则**、以及失败时的**可读信息**（不能把带密码的 URL 打出去）。

use rsbatis::{Engine, Session};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::support::redact;

/// 配置文件 `[database]` 段 + 代码里显式注册（同名以后者为准）。
///
/// 顺序保留配置文件的字母序，再追加代码里新的名字——这样"注册了几个库"
/// 在日志里是稳定的，不随 HashMap 顺序变。
pub(super) fn collect_urls(
    config: &Config,
    explicit: &[(String, String)],
) -> Vec<(String, String)> {
    let mut urls = config.section("database");
    for (name, url) in explicit {
        if let Some(slot) = urls.iter_mut().find(|(n, _)| n == name) {
            slot.1 = url.clone();
        } else {
            urls.push((name.clone(), url.clone()));
        }
    }
    urls
}

/// 推断默认库：显式配置 > 名为 `default` 的 > 唯一那个。
///
/// 有多个库又没有 `default` 时不猜——猜错了会在第一条 SQL 上炸开，
/// 而错误现场离真正的原因很远。
pub(super) fn resolve_default(
    config: &Config,
    urls: &[(String, String)],
) -> Result<Option<String>> {
    if let Some(n) = config.get("app.default_database") {
        if !urls.iter().any(|(name, _)| name == n) {
            return Err(Error::Config(format!(
                "`app.default_database = {n}`，但没有注册名为 `{n}` 的数据库。已注册：{}",
                names_of(urls)
            )));
        }
        return Ok(Some(n.to_string()));
    }
    Ok(match urls.len() {
        0 => None,
        1 => Some(urls[0].0.clone()),
        _ if urls.iter().any(|(n, _)| n == "default") => Some("default".to_string()),
        _ => {
            return Err(Error::Config(format!(
                "注册了多个数据库但没有 `default`：{}。\
                 用 `.database(\"default\", url)` 指定默认库，或写 `app.default_database`",
                names_of(urls)
            )));
        }
    })
}

/// 逐个打开。失败信息带上库名与**脱敏后的 URL**。
pub(super) fn open(urls: Vec<(String, String)>) -> Result<Vec<(String, Session<Engine>)>> {
    let mut out = Vec::with_capacity(urls.len());
    for (name, url) in urls {
        let session = rsbatis::create_engine(&url).map_err(|e| {
            Error::Config(format!(
                "打开数据库 `{name}`（{}）失败：{}",
                redact::url(&url),
                e.message
            ))
        })?;
        out.push((name, session));
    }
    Ok(out)
}

pub(super) fn names_of(urls: &[(String, String)]) -> String {
    if urls.is_empty() {
        "（无）".to_string()
    } else {
        urls.iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
            .join("、")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(pairs: &[(&str, &str)]) -> Config {
        let mut c = Config::new();
        for (k, v) in pairs {
            c = c.set(*k, *v);
        }
        c
    }

    #[test]
    fn explicit_registration_overrides_file() {
        let c = cfg(&[("database.default", "sqlite:///file.db")]);
        let urls = collect_urls(&c, &[("default".into(), "sqlite:///code.db".into())]);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].1, "sqlite:///code.db");
    }

    #[test]
    fn explicit_new_names_are_appended_after_file_ones() {
        let c = cfg(&[("database.a", "x"), ("database.b", "y")]);
        let urls = collect_urls(&c, &[("c".into(), "z".into())]);
        assert_eq!(urls.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
    }

    #[test]
    fn single_db_is_the_default() {
        let urls = vec![("main".to_string(), "u".to_string())];
        assert_eq!(resolve_default(&Config::new(), &urls).unwrap(), Some("main".into()));
    }

    #[test]
    fn multiple_without_default_is_refused() {
        let urls = vec![
            ("a".to_string(), "u".to_string()),
            ("b".to_string(), "v".to_string()),
        ];
        let e = resolve_default(&Config::new(), &urls).unwrap_err();
        assert!(e.to_string().contains("没有 `default`"), "{e}");
    }

    #[test]
    fn named_default_wins_over_position() {
        let urls = vec![
            ("a".to_string(), "u".to_string()),
            ("default".to_string(), "v".to_string()),
        ];
        assert_eq!(
            resolve_default(&Config::new(), &urls).unwrap(),
            Some("default".into())
        );
    }

    #[test]
    fn configured_default_must_exist() {
        let c = cfg(&[("app.default_database", "nope")]);
        let urls = vec![("a".to_string(), "u".to_string())];
        let e = resolve_default(&c, &urls).unwrap_err();
        assert!(e.to_string().contains("没有注册"), "{e}");
    }
}
