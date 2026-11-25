//! 分层配置：平坦的 `键 = 值` 映射。
//!
//! 一份配置有五个来源，合并顺序（后者覆盖前者）：
//! 文件 → profile 段 → 代码 `set` → 环境变量 → 环境默认值。
//! 环境变量排在文件之后，是因为容器部署时改不了打进镜像的配置。
//!
//! 格式按扩展名分派：`.yaml`/`.yml` 走 YAML 子集（见 `yaml`），其余走 ini（见 `parse`）。
//! 两种格式产出完全同形的平坦映射，所以下游逻辑只有一份。

use std::collections::HashMap;
use std::path::Path;

use crate::error::{Error, Result};

pub mod environment;
pub mod layered;
pub mod parse;
pub mod watcher;
pub mod yaml;

pub use environment::Environment;

#[derive(Debug, Clone, Default)]
pub struct Config {
    map: HashMap<String, String>,
    /// 值的来源。诊断时回答"这个值是谁给的"。
    from: HashMap<String, String>,
}

impl Config {
    pub fn new() -> Self {
        Self::default()
    }

    /// 读一个配置文件（格式按扩展名），并跑完整分层（含 profile 段与环境默认值）。
    ///
    /// 文件不存在就报错，不静默跳过——"我明明写了配置却没生效"多半是路径写错。
    pub fn file(self, path: impl AsRef<Path>) -> Result<Self> {
        let p = path.as_ref();
        layered::layered(&self, Some(p), None).map(|(c, _, _)| c)
    }

    /// 从字符串加载（测试、嵌入式配置用）。按 ini 解析。
    pub fn load_str(&mut self, text: &str, origin: &str) -> Result<()> {
        self.load_entries(parse::parse(text, origin).map_err(Error::Config)?, origin)
    }

    /// 从字符串加载 YAML 子集。
    pub fn load_yaml(&mut self, text: &str, origin: &str) -> Result<()> {
        self.load_entries(yaml::parse(text, origin).map_err(Error::Config)?, origin)
    }

    fn load_entries(&mut self, entries: Vec<(String, String)>, origin: &str) -> Result<()> {
        for (k, v) in entries {
            self.insert(k, v, origin);
        }
        Ok(())
    }

    /// 用环境变量覆盖已有键。`prefix = "MYAPP_"` 时 `database.default`
    /// 对应 `MYAPP_DATABASE_DEFAULT`。
    ///
    /// 只覆盖**已存在**的键：环境里偶然的同名变量不该凭空造出配置项。
    pub fn env_overlay(mut self, prefix: &str) -> Self {
        for key in self.map.keys().cloned().collect::<Vec<_>>() {
            let var = format!("{prefix}{}", key.replace('.', "_").to_uppercase());
            if let Ok(v) = std::env::var(&var) {
                self.map.insert(key.clone(), v);
                self.from.insert(key, format!("环境变量 {var}"));
            }
        }
        self
    }

    /// 代码里显式设置（优先级仅次于环境变量）。
    pub fn set(self, key: impl Into<String>, value: impl Into<String>) -> Self {
        let mut me = self;
        me.insert(key.into(), value.into(), "代码");
        me
    }

    /// 供分层流水线内部用：写入并标注来源。
    pub(crate) fn insert(
        &mut self,
        key: impl Into<String>,
        value: impl Into<String>,
        source: &str,
    ) {
        let k = key.into();
        self.map.insert(k.clone(), value.into());
        self.from.insert(k, source.to_string());
    }

    /// 删掉一个键（热加载时配置文件里删了某项要用）。
    pub(crate) fn remove(&mut self, key: &str) {
        self.map.remove(key);
        self.from.remove(key);
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.map.get(key).map(|s| s.as_str())
    }

    /// 必填项。缺失时列出所有已知键——配置错误十有八九是键名拼错。
    pub fn require(&self, key: &str) -> Result<&str> {
        self.get(key).ok_or_else(|| {
            let known = self.keys();
            Error::Config(format!(
                "缺少配置项 `{key}`。当前已知的键：{}",
                if known.is_empty() {
                    "（无）".to_string()
                } else {
                    known.join("、")
                }
            ))
        })
    }

    pub fn get_int(&self, key: &str) -> Result<Option<i64>> {
        match self.get(key) {
            None => Ok(None),
            Some(v) => v.parse().map(Some).map_err(|_| {
                Error::Config(format!(
                    "配置项 `{key}` 应为整数，实际是 `{v}`（来自{}）",
                    self.source(key)
                ))
            }),
        }
    }

    pub fn get_bool(&self, key: &str) -> Result<Option<bool>> {
        match self.get(key) {
            None => Ok(None),
            Some(v) => match v.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => Ok(Some(true)),
                "0" | "false" | "no" | "off" => Ok(Some(false)),
                _ => Err(Error::Config(format!(
                    "配置项 `{key}` 应为布尔值（true/false/1/0），实际是 `{v}`"
                ))),
            },
        }
    }

    /// 读列表。YAML 的 `- a` 会编码成 `key.0`、`key.1`；
    /// 也接受单值写法（返回长度 1 的 Vec）。
    ///
    /// 索引读法而不是逗号切分，是因为值里可能真含逗号。
    pub fn list(&self, key: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut i = 0usize;
        while let Some(v) = self.get(&format!("{key}.{i}")) {
            out.push(v.to_string());
            i += 1;
        }
        if out.is_empty() {
            if let Some(v) = self.get(key) {
                if !v.is_empty() {
                    out.push(v.to_string());
                }
            }
        }
        out
    }

    pub fn source(&self, key: &str) -> String {
        self.from
            .get(key)
            .cloned()
            .unwrap_or_else(|| "默认".to_string())
    }

    /// 以 `section.` 为前缀的全部键值（已去前缀、已排序）。
    /// 多数据源靠它枚举：`[database]` 段写几个就注册几个。
    pub fn section(&self, section: &str) -> Vec<(String, String)> {
        let p = format!("{section}.");
        let mut out: Vec<(String, String)> = self
            .map
            .iter()
            .filter_map(|(k, v)| k.strip_prefix(&p).map(|rest| (rest.to_string(), v.clone())))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    pub fn keys(&self) -> Vec<&str> {
        let mut k: Vec<&str> = self.map.keys().map(|s| s.as_str()).collect();
        k.sort();
        k
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn later_set_wins_and_source_is_code() {
        let mut c = Config::new();
        c.load_str("[app]\nname = from-file\n", "t.ini").unwrap();
        let c = c.set("app.name", "from-code");
        assert_eq!(c.get("app.name"), Some("from-code"));
        assert_eq!(c.source("app.name"), "代码");
    }

    #[test]
    fn require_hints_at_typo() {
        let c = Config::new().set("app.nmae", "x");
        let e = c.require("app.name").unwrap_err();
        assert!(e.to_string().contains("app.nmae"), "{e}");
    }

    #[test]
    fn section_enumeration_is_sorted() {
        let mut c = Config::new();
        c.load_str("[database]\nreport = r\ndefault = d\n", "t.ini")
            .unwrap();
        let dbs = c.section("database");
        assert_eq!(dbs[0].0, "default");
        assert_eq!(dbs[1].0, "report");
    }

    #[test]
    fn int_and_bool() {
        let c = Config::new().set("a.n", "42").set("a.b", "yes");
        assert_eq!(c.get_int("a.n").unwrap(), Some(42));
        assert_eq!(c.get_bool("a.b").unwrap(), Some(true));
        assert!(Config::new().set("a.n", "x").get_int("a.n").is_err());
        assert!(Config::new().set("a.b", "maybe").get_bool("a.b").is_err());
    }

    #[test]
    fn missing_file_is_an_error() {
        let e = Config::new().file("/nonexistent/app.ini").unwrap_err();
        assert!(e.to_string().contains("读不到配置文件"), "{e}");
    }

    #[test]
    fn env_overlay_only_touches_existing_keys() {
        std::env::set_var("RSBOOT_T2_APP_NAME", "from-env");
        let mut c = Config::new();
        c.load_str("[app]\nname = from-file\n", "t.ini").unwrap();
        let c = c.env_overlay("RSBOOT_T2_");
        assert_eq!(c.get("app.name"), Some("from-env"));
        assert!(c.get("app.other").is_none());
        std::env::remove_var("RSBOOT_T2_APP_NAME");
    }

    #[test]
    fn list_reads_indexed_keys_and_falls_back_to_single_value() {
        let mut c = Config::new();
        c.load_yaml("cors:\n  origins:\n    - a\n    - b\n", "t.yaml")
            .unwrap();
        assert_eq!(c.list("cors.origins"), ["a", "b"]);
        assert_eq!(Config::new().set("x.y", "one").list("x.y"), ["one"]);
        assert!(Config::new().list("x.y").is_empty());
    }
}
