//! 配置分层流水线（唯一一份实现）。
//!
//! `Builder` 与热加载走同一个函数，否则两边迟早出现"启动时这样、热更时那样"的偏差。
//!
//! 顺序：文件 → profile 段 → 代码覆盖 → 环境变量 → 环境默认值。
//! 代码覆盖放在 profile 之后：代码里显式 `set` 的键永远赢。

use std::path::Path;

use crate::config::environment::{apply_profile, Environment};
use crate::config::yaml;
use crate::config::{parse, Config};
use crate::error::{Error, Result};

/// 按扩展名解析文件，返回原始键值（未应用 profile）。
pub fn raw_entries(path: &Path) -> Result<Vec<(String, String)>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::Config(format!("读不到配置文件 {}：{e}", path.display())))?;
    let origin = path.display().to_string();
    match path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("yaml") | Some("yml") => yaml::parse(&text, &origin).map_err(Error::Config),
        _ => parse::parse(&text, &origin).map_err(Error::Config),
    }
}

/// 完整分层。返回 `(最终配置, 生效环境, 环境是否显式声明)`。
///
/// 环境是否显式声明要带出去：没声明时要打一条启动提示，
/// 因为"release 编译跑在开发机"这类事故只有提示才拦得住。
pub fn layered(
    code: &Config,
    file: Option<&Path>,
    env_prefix: Option<&str>,
) -> Result<(Config, Environment, bool)> {
    let entries = match file {
        Some(p) => raw_entries(p)?,
        None => Vec::new(),
    };

    // 判定环境：代码里的 app.env 优先于文件里的
    let mut probe = Config::new();
    for (k, v) in &entries {
        probe.insert(k.clone(), v.clone(), "probe");
    }
    for key in code.keys() {
        if key == "app.env" || key == "env" {
            if let Some(v) = code.get(key) {
                probe.insert(key, v, "probe");
            }
        }
    }
    let (env, explicit) = Environment::detect(&probe).map_err(Error::Config)?;

    let entries = apply_profile(entries, env);

    let mut config = Config::new();
    if let Some(p) = file {
        let origin = p.display().to_string();
        for (k, v) in entries {
            config.insert(k, v, &origin);
        }
    }
    for key in code.keys() {
        if let Some(v) = code.get(key) {
            config.insert(key, v, "代码");
        }
    }
    if let Some(prefix) = env_prefix {
        config = config.env_overlay(prefix);
    }
    Ok((env.apply_defaults(config), env, explicit))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(name: &str, body: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rsboot-layered-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn yaml_and_ini_produce_the_same_flattened_shape() {
        let y = tmp("a.yaml", "database:\n  default: sqlite:///y.db\n");
        let i = tmp("a.ini", "[database]\ndefault = sqlite:///i.db\n");
        let (cy, _, _) = layered(&Config::new(), Some(&y), None).unwrap();
        let (ci, _, _) = layered(&Config::new(), Some(&i), None).unwrap();
        assert_eq!(cy.get("database.default"), Some("sqlite:///y.db"));
        assert_eq!(ci.get("database.default"), Some("sqlite:///i.db"));
        assert_eq!(cy.keys(), ci.keys(), "两种格式的键集合必须一致");
    }

    #[test]
    fn code_wins_over_file() {
        let f = tmp("b.yaml", "app:\n  name: from-file\n");
        let code = Config::new().set("app.name", "from-code");
        let (c, _, _) = layered(&code, Some(&f), None).unwrap();
        assert_eq!(c.get("app.name"), Some("from-code"));
    }

    #[test]
    fn profile_segment_is_applied_per_environment() {
        let f = tmp(
            "c.yaml",
            "http:\n  bind: 127.0.0.1:8080\nprod:\n  http:\n    bind: 0.0.0.0:8080\n",
        );
        let prod = Config::new().set("app.env", "prod");
        let (c, env, explicit) = layered(&prod, Some(&f), None).unwrap();
        assert_eq!(env, Environment::Prod);
        assert!(explicit);
        assert_eq!(c.get("http.bind"), Some("0.0.0.0:8080"));

        let dev = Config::new().set("app.env", "dev");
        let (c, _, _) = layered(&dev, Some(&f), None).unwrap();
        assert_eq!(c.get("http.bind"), Some("127.0.0.1:8080"));
    }

    #[test]
    fn env_defaults_are_applied_last() {
        let dev = Config::new().set("app.env", "dev");
        let (c, _, _) = layered(&dev, None, None).unwrap();
        assert!(c.get("http.bind").is_some());
        let prod = Config::new().set("app.env", "prod");
        let (c, _, _) = layered(&prod, None, None).unwrap();
        assert!(c.get("http.bind").is_none());
    }
}
