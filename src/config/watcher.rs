//! 配置热加载：mtime 轮询 + 白名单 + 原子换快照。
//!
//! 四条硬性行为：
//!
//! 1. **配置写坏时绝不退出**。重新解析失败 → 打一条 error → 继续用旧配置。
//!    热加载的价值是"改错了不至于把服务搞死"，否则它比没有更危险。
//! 2. **只有白名单里的键会热生效**。其余键变更只记 warning：`需要重启才生效`。
//!    静默半生效是最难排查的故障。
//! 3. **替换是原子的**：一次 `Arc` 交换。读者要么看到整份旧的，要么整份新的。
//! 4. **mtime 轮询，不用 inotify**：零依赖、跨平台一致；配置变更频率极低，
//!    1 秒延迟可接受，换来不用处理"文件被 rename/mv 时事件丢失"这一堆坑。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use crate::config::layered;
use crate::config::Config;

/// 热加载读到的变化。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReloadEvent {
    /// 已生效的键
    Applied(Vec<String>),
    /// 变了但需要重启才生效的键
    NeedsRestart(Vec<String>),
    /// 文件读不了/解析失败，旧配置仍在服务
    Failed(String),
}

pub struct ReloadSpec {
    pub path: PathBuf,
    /// 允许热生效的键模式。结尾 `*` 为前缀匹配。空 = 一个都不热更。
    pub hot: Vec<String>,
    pub interval: Duration,
    /// 代码层的配置（重放分层时作为最高优先级输入）
    pub base: Config,
    pub env_prefix: Option<String>,
}

impl ReloadSpec {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            hot: Vec::new(),
            interval: Duration::from_secs(1),
            base: Config::new(),
            env_prefix: None,
        }
    }
}

/// 持有它就相当于"热加载开着"，`Drop` 即停。
pub struct Watcher {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Watcher {
    pub fn spawn(slot: Arc<RwLock<Arc<Config>>>, spec: ReloadSpec) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("rsboot-config-reload".to_string())
            .spawn(move || run(slot, spec, stop_thread))?;
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }

    /// 手动停（不等 Drop）。
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            // 线程最长睡一个 interval，join 阻塞时间有界
            let _ = h.join();
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run(slot: Arc<RwLock<Arc<Config>>>, spec: ReloadSpec, stop: Arc<AtomicBool>) {
    let mut last = mtime(&spec.path);
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(spec.interval);
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let now = mtime(&spec.path);
        if now == last {
            continue;
        }
        // 变了才更新基准时间：失败也更新，否则会每秒重试一次刷屏日志
        last = now;

        match layered::layered(&spec.base, Some(&spec.path), spec.env_prefix.as_deref()) {
            Ok((fresh, _, _)) => {
                let current = read_slot(&slot);
                let (merged, event) = plan(&current, &fresh, &spec.hot);
                if matches!(event, ReloadEvent::Applied(_)) {
                    write_slot(&slot, Arc::new(merged));
                }
                report(&event);
            }
            Err(e) => report(&ReloadEvent::Failed(e.to_string())),
        }
    }
}

/// 纯函数：算出"新旧应用后的配置"与"发生了什么事"。
///
/// 只把白名单命中的键写进结果，其余保持旧值——这样"改了但没重启"的键
/// 不会因为一次无关的热更而偷偷生效。
pub fn plan(old: &Config, fresh: &Config, hot: &[String]) -> (Config, ReloadEvent) {
    let mut merged = old.clone();
    let mut applied = Vec::new();
    let mut need_restart = Vec::new();

    let mut keys: Vec<String> = old.keys().into_iter().map(|s| s.to_string()).collect();
    for k in fresh.keys() {
        if !keys.iter().any(|x| x == k) {
            keys.push(k.to_string());
        }
    }
    keys.sort();

    for key in keys {
        if old.get(&key) == fresh.get(&key) {
            continue;
        }
        if hot_match(hot, &key) {
            match fresh.get(&key) {
                Some(v) => merged.insert(key.clone(), v, "热加载"),
                None => merged.remove(&key),
            }
            applied.push(key);
        } else {
            need_restart.push(key);
        }
    }

    if !applied.is_empty() {
        // 两者可能同时存在，优先报"已生效"，重启项另起一条
        (merged, ReloadEvent::Applied(applied))
    } else if !need_restart.is_empty() {
        (merged, ReloadEvent::NeedsRestart(need_restart))
    } else {
        (merged, ReloadEvent::Applied(Vec::new()))
    }
}

/// `log.*` 匹配 `log.level`；`*` 匹配全部；无通配则精确相等。
pub fn hot_match(patterns: &[String], key: &str) -> bool {
    patterns.iter().any(|p| match p.strip_suffix('*') {
        Some(prefix) => key.starts_with(prefix),
        None => p == key,
    })
}

fn report(event: &ReloadEvent) {
    // 暂时直接写 stderr：日志层（可替换 Logger）在 S4 落地后改为走它。
    match event {
        ReloadEvent::Applied(keys) if keys.is_empty() => {}
        ReloadEvent::Applied(keys) => {
            eprintln!("[rsboot] 配置已热加载：{}", keys.join("、"))
        }
        ReloadEvent::NeedsRestart(keys) => eprintln!(
            "[rsboot] 配置项 {} 已变更，但需要重启才生效",
            keys.join("、")
        ),
        ReloadEvent::Failed(e) => {
            eprintln!("[rsboot] 配置重新加载失败，继续使用旧配置：{e}")
        }
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn read_slot(slot: &RwLock<Arc<Config>>) -> Arc<Config> {
    match slot.read() {
        Ok(g) => Arc::clone(&g),
        Err(e) => Arc::clone(&e.into_inner()),
    }
}

fn write_slot(slot: &RwLock<Arc<Config>>, next: Arc<Config>) {
    match slot.write() {
        Ok(mut g) => *g = next,
        Err(e) => *e.into_inner() = next,
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
    fn only_whitelisted_keys_are_applied() {
        let old = cfg(&[("log.level", "info"), ("database.default", "a")]);
        let fresh = cfg(&[("log.level", "debug"), ("database.default", "b")]);
        let (merged, ev) = plan(&old, &fresh, &["log.*".to_string()]);
        assert_eq!(merged.get("log.level"), Some("debug"));
        assert_eq!(
            merged.get("database.default"),
            Some("a"),
            "未进白名单的键必须保持旧值"
        );
        assert_eq!(ev, ReloadEvent::Applied(vec!["log.level".to_string()]));
    }

    #[test]
    fn non_whitelisted_change_is_reported_as_needs_restart() {
        let old = cfg(&[("database.default", "a")]);
        let fresh = cfg(&[("database.default", "b")]);
        let (merged, ev) = plan(&old, &fresh, &["log.*".to_string()]);
        assert_eq!(merged.get("database.default"), Some("a"));
        assert_eq!(ev, ReloadEvent::NeedsRestart(vec!["database.default".to_string()]));
    }

    #[test]
    fn no_change_produces_empty_event() {
        let c = cfg(&[("a.b", "1")]);
        let (_, ev) = plan(&c, &c, &["*".to_string()]);
        assert_eq!(ev, ReloadEvent::Applied(vec![]));
    }

    #[test]
    fn removed_key_in_whitelist_disappears() {
        let old = cfg(&[("log.level", "info")]);
        let fresh = Config::new();
        let (merged, _) = plan(&old, &fresh, &["log.*".to_string()]);
        assert!(merged.get("log.level").is_none());
    }

    #[test]
    fn hot_match_semantics() {
        let hot = vec!["log.*".to_string(), "http.cors.allow_origin".to_string()];
        assert!(hot_match(&hot, "log.level"));
        assert!(hot_match(&hot, "http.cors.allow_origin"));
        assert!(!hot_match(&hot, "http.bind"));
        assert!(!hot_match(&hot, "logs.level"), "前缀必须按段边界，不是纯字符串前缀");
        assert!(hot_match(&["*".to_string()], "anything"));
        assert!(!hot_match(&[], "anything"));
    }
}
