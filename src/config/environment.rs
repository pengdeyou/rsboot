//! 环境模式与 profile 段。
//!
//! 模式只有一个作用：**给未显式设置的键填默认值**。不在业务代码里散落
//! `if dev { ... }`——那样最终没人说得清线上生效的到底是什么。
//!
//! profile 段（`dev:` / `prod:`）在选定环境后整段覆盖同名键，先把不选中的
//! 那段丢掉，免得它后的解析歧义。

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Environment {
    Dev,
    Prod,
}

impl Environment {
    pub fn name(self) -> &'static str {
        match self {
            Environment::Dev => "dev",
            Environment::Prod => "prod",
        }
    }

    pub fn is_dev(self) -> bool {
        self == Environment::Dev
    }

    /// 判定环境：`app.env` > 环境变量 `RSBOOT_ENV` > 编译模式推断。
    ///
    /// 返回 `(环境, 是否显式声明)`。第二个值用于启动时打提示：
    /// release 编译跑在开发机、debug 编译跑在生产，都是真实事故，值得提醒。
    pub fn detect(config: &Config) -> Result<(Self, bool), String> {
        let from_config = config.get("app.env").or_else(|| config.get("env"));
        let raw = match from_config {
            Some(v) => (v.to_string(), "app.env"),
            None => match std::env::var("RSBOOT_ENV") {
                Ok(v) => (v, "环境变量 RSBOOT_ENV"),
                Err(_) => return Ok((Self::from_build(), false)),
            },
        };
        Self::parse(&raw.0)
            .map(|e| (e, true))
            .map_err(|_| {
                format!(
                    "{}(来自 {}) 不是合法环境名。可选：dev、prod（也接受 development、production）",
                    raw.0, raw.1
                )
            })
    }

    fn parse(s: &str) -> Result<Self, ()> {
        match s.trim().to_ascii_lowercase().as_str() {
            "dev" | "development" | "debug" | "local" => Ok(Environment::Dev),
            "prod" | "production" | "release" => Ok(Environment::Prod),
            _ => Err(()),
        }
    }

    /// 没显式声明时的兜底：按编译模式。
    fn from_build() -> Self {
        if cfg!(debug_assertions) {
            Environment::Dev
        } else {
            Environment::Prod
        }
    }

    /// 填默认值。只碰**当前缺失**的键，绝不覆盖使用者写过的值。
    pub fn apply_defaults(self, mut config: Config) -> Config {
        let src = format!("默认值({})", self.name());
        for (k, v) in self.defaults() {
            if config.get(k).is_none() {
                config.insert(k, v, &src);
            }
        }
        config
    }

    fn defaults(self) -> Vec<(&'static str, &'static str)> {
        match self {
            Environment::Dev => vec![
                ("log.format", "text"),
                ("log.level", "debug"),
                ("http.docs", "true"),
                // 开发时放宽，免得调大文件就 413
                ("http.max_body_bytes", "10485760"),
                // 只绑本机：开发服务不该在局域网里裸奔
                ("http.bind", "127.0.0.1:8000"),
                ("http.cors.allow_origin", "http://localhost:*"),
                ("reload.enabled", "true"),
                ("http.envelope", "false"),
            ],
            Environment::Prod => vec![
                ("log.format", "json"),
                ("log.level", "info"),
                ("http.docs", "false"),
                ("http.max_body_bytes", "1048576"),
                ("reload.enabled", "false"),
                ("http.envelope", "false"),
                // 跛鸭期：K8s 删 Pod 到 endpoints 真正摘掉有滞后，
                // 立刻停 accept 会把仍在被派过来的请求打空
                ("http.drain_secs", "5"),
                // 刻意不填 http.bind：默认绑 0.0.0.0 是事故之源，必须由使用者写出来
            ],
        }
    }
}

/// 应用 profile 段：把选中环境那段里的键覆盖到全局，再丢掉两段残留。
pub fn apply_profile(entries: Vec<(String, String)>, env: Environment) -> Vec<(String, String)> {
    let want = format!("{}.", env.name());
    let others = ["dev.", "prod."];
    let mut base: Vec<(String, String)> = entries
        .iter()
        .filter(|(k, _)| {
            !k.starts_with(&want)
                && !others.iter().any(|p| k.starts_with(p) || k == p.trim_end_matches('.'))
        })
        .cloned()
        .collect();

    for (k, v) in entries.iter().filter(|(k, _)| k.starts_with(&want)) {
        let target = &k[want.len()..];
        match base.iter_mut().find(|(n, _)| n == target) {
            Some(slot) => slot.1 = v.clone(),
            None => base.push((target.to_string(), v.clone())),
        }
    }
    base
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
    fn dev_defaults_fill_only_missing_keys() {
        let c = cfg(&[("log.level", "warn")]);
        let c = Environment::Dev.apply_defaults(c);
        assert_eq!(c.get("log.level"), Some("warn"), "不能覆盖使用者写的值");
        assert_eq!(c.get("http.docs"), Some("true"));
        assert_eq!(c.get("http.bind"), Some("127.0.0.1:8000"));
        assert_eq!(c.source("http.bind"), "默认值(dev)");
    }

    #[test]
    fn prod_never_guesses_bind_address() {
        let c = Environment::Prod.apply_defaults(Config::new());
        assert!(c.get("http.bind").is_none(), "prod 不该给 bind 默认值");
        assert_eq!(c.get("http.docs"), Some("false"));
        assert_eq!(c.get("log.format"), Some("json"));
    }

    #[test]
    fn profile_section_overrides_global_and_drops_the_other_one() {
        let entries = vec![
            ("http.bind".to_string(), "127.0.0.1:8080".to_string()),
            ("database.default".to_string(), "sqlite:///a.db".to_string()),
            ("prod.http.bind".to_string(), "0.0.0.0:8080".to_string()),
            ("dev.http.bind".to_string(), "127.0.0.1:9000".to_string()),
        ];
        let prod = apply_profile(entries.clone(), Environment::Prod);
        assert_eq!(
            prod.iter().find(|(k, _)| k == "http.bind").unwrap().1,
            "0.0.0.0:8080"
        );
        assert!(!prod.iter().any(|(k, _)| k.starts_with("dev.")));

        let dev = apply_profile(entries, Environment::Dev);
        assert_eq!(
            dev.iter().find(|(k, _)| k == "http.bind").unwrap().1,
            "127.0.0.1:9000"
        );
        assert!(!dev.iter().any(|(k, _)| k.starts_with("prod.")));
    }

    #[test]
    fn profile_adds_keys_that_were_absent_globally() {
        let entries = vec![("prod.db.pool_max".to_string(), "50".to_string())];
        let v = apply_profile(entries, Environment::Prod);
        assert_eq!(v, vec![("db.pool_max".to_string(), "50".to_string())]);
    }

    #[test]
    fn explicit_env_wins_and_typo_is_rejected() {
        let c = cfg(&[("app.env", "PRODUCTION")]);
        let (env, explicit) = Environment::detect(&c).unwrap();
        assert_eq!(env, Environment::Prod);
        assert!(explicit);

        let bad = cfg(&[("app.env", "prodd")]);
        let e = Environment::detect(&bad).unwrap_err();
        assert!(e.contains("prodd"), "{e}");
    }
}
