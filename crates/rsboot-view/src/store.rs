//! 模板集合的存储、磁盘缓存与热重载，外加"模板里引用模板"的解析规则。
//!
//! 三件事放在一起是因为它们共享同一份"目录 → 解析结果"的映射：
//!
//! 1. [`Store`]：编译好的只读快照。渲染时整份 `Arc` 克隆，锁只持一瞬。
//! 2. 磁盘缓存（`.tplc`）：把**解析结果**（AST）存成 JSON，下次启动跳过词法/语法。
//!    注意它不省读文件：变更检测本身要 `stat`，mtime+size 变了才读内容。
//! 3. 热重载：按固定间隔 `stat` 一遍目录，有变化就重建一份新 `Store` 整体换掉。
//!
//! 引用规则（`{include}` / `{extends}`）：
//! - `"row.tpl"`、`"admin/row.tpl"`：按模板名原样找（根相对）。
//! - `"./row.tpl"`、`"../shared/row.tpl"`：相对**当前模板所在目录**解析，
//!   解析结果不许跑出模板根（`..` 冒头就报错），免得把服务器任意文件当模板读。
//! - 静态名字在编译期就解析掉、并校验"存在 + 不成环"；动态名字
//!   （`{include file=$x}`）仍走运行期，保留 32 层深度兜底。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::parse::{parse, Expr, Stmt};
use crate::{split_src, Error, Src};

/// 编译好的模板集合。构造完即只读，可跨线程共享。
#[derive(Default)]
pub(crate) struct Store {
    /// 展开后的 AST：继承已合成，渲染只看这份。
    pub(crate) templates: HashMap<String, Arc<Vec<Stmt>>>,
}

impl Store {
    pub(crate) fn nodes(&self, name: &str) -> Option<&[Stmt]> {
        self.templates.get(name).map(|n| n.as_slice())
    }

    pub(crate) fn len(&self) -> usize {
        self.templates.len()
    }
}

/// 一份文件解析出来的 AST + 判重信息。
#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct AstEntry {
    pub(crate) name: String,
    pub(crate) mtime: u64,
    pub(crate) size: u64,
    /// 源码内容的 FNV-1a 64。mtime 变了但内容没变时靠它复用旧 AST。
    pub(crate) hash: u64,
    pub(crate) nodes: Vec<Stmt>,
}

/// `.tplc` 文件的顶层结构。`format` 一变就整体丢弃重建。
#[derive(Serialize, Deserialize)]
struct AstDoc {
    format: u32,
    /// 写这个文件的 crate 版本；跨版本升级后 AST 布局可能变，直接作废更安全。
    version: String,
    entries: Vec<AstEntry>,
}

/// 缓存格式版本。改 `Stmt` 的序列化形状就要 +1。
const CACHE_FORMAT: u32 = 1;

pub(crate) fn crate_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// FNV-1a 64：够用的内容指纹，不需要引入哈希库。
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn stamp(md: &std::fs::Metadata) -> (u64, u64) {
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| {
            // 纳秒塞进后 9 位十进制，够精确也不溢出
            d.as_secs()
                .saturating_mul(1_000_000_000)
                .saturating_add(u64::from(d.subsec_nanos()))
        })
        .unwrap_or(0);
    (mtime, md.len())
}

/// 读缓存文件。任何异常（不存在、坏 JSON、格式/版本不符）都当没缓存——
/// 缓存失效只是慢一点，绝不能因为它启动失败。
pub(crate) fn load_cache(path: &Path) -> Option<HashMap<String, AstEntry>> {
    let text = std::fs::read_to_string(path).ok()?;
    let doc: AstDoc = serde_json::from_str(&text).ok()?;
    if doc.format != CACHE_FORMAT || doc.version != crate_version() {
        return None;
    }
    Some(
        doc.entries
            .into_iter()
            .map(|e| (e.name.clone(), e))
            .collect(),
    )
}

fn save_cache(path: &Path, entries: &HashMap<String, AstEntry>) {
    let mut list: Vec<AstEntry> = entries.values().cloned().collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    let doc = AstDoc {
        format: CACHE_FORMAT,
        version: crate_version(),
        entries: list,
    };
    let Ok(text) = serde_json::to_string(&doc) else {
        return;
    };
    // 先写临时文件再 rename：进程在写一半时被杀，不会留下半个 JSON 让下次启动全废。
    let tmp = path.with_extension("tplc.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// 目录里该收哪些文件。
pub(crate) fn collect_files(root: &Path) -> std::io::Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    walk(root, root, &mut out)?;
    Ok(out)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, out)?;
        } else {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext.eq_ignore_ascii_case("tpl") || ext.eq_ignore_ascii_case("html") {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((rel, path));
            }
        }
    }
    Ok(())
}

/// 从目录构建快照。
///
/// `prior` 是上一次的 AST（热重载时传进来做增量），`cache` 是磁盘缓存文件。
/// 两级复用：mtime+size 没变 → 连文件都不读；读到了但内容指纹一致 →
/// 跳过解析，只重算 `split_src` / 继承展开（这两步都是毫秒级）。
pub(crate) fn build(
    root: &Path,
    cache: Option<&Path>,
    prior: Option<&HashMap<String, AstEntry>>,
) -> Result<(Store, HashMap<String, AstEntry>), Error> {
    let files = collect_files(root).map_err(|e| Error {
        template: root.display().to_string(),
        line: 0,
        msg: format!("读取目录失败：{e}"),
    })?;
    let disk = cache.and_then(load_cache);
    let mut ast: HashMap<String, AstEntry> = HashMap::with_capacity(files.len());

    for (name, path) in files {
        let md = std::fs::metadata(&path).map_err(|e| Error {
            template: name.clone(),
            line: 0,
            msg: format!("读取失败：{e}"),
        })?;
        let (mtime, size) = stamp(&md);
        let reuse = |e: &AstEntry| e.mtime == mtime && e.size == size;

        if let Some(e) = prior.and_then(|p| p.get(&name)).filter(|e| reuse(e)) {
            ast.insert(name.clone(), e.clone());
            continue;
        }
        if let Some(e) = disk.as_ref().and_then(|d| d.get(&name)).filter(|e| reuse(e)) {
            ast.insert(name.clone(), e.clone());
            continue;
        }
        let src = std::fs::read_to_string(&path).map_err(|e| Error {
            template: name.clone(),
            line: 0,
            msg: format!("读取失败：{e}"),
        })?;
        let hash = fnv1a(src.as_bytes());
        // mtime 变了（git checkout、touch）但内容没变：指纹一致就复用
        if let Some(e) = prior
            .and_then(|p| p.get(&name))
            .or_else(|| disk.as_ref().and_then(|d| d.get(&name)))
            .filter(|e| e.hash == hash)
        {
            let mut e = e.clone();
            e.mtime = mtime;
            e.size = size;
            ast.insert(name.clone(), e);
            continue;
        }
        let nodes = parse(&src).map_err(|e| Error {
            template: name.clone(),
            line: e.line,
            msg: e.msg,
        })?;
        ast.insert(
            name.clone(),
            AstEntry {
                name: name.clone(),
                mtime,
                size,
                hash,
                nodes,
            },
        );
    }

    let store = compose_all(&ast)?;
    if let Some(p) = cache {
        save_cache(p, &ast);
    }
    Ok((store, ast))
}

/// 打出 `raw` + `templates` 两份表。
fn compose_all(ast: &HashMap<String, AstEntry>) -> Result<Store, Error> {
    let mut raw = HashMap::with_capacity(ast.len());
    for (name, e) in ast {
        raw.insert(name.clone(), Arc::new(split_src(name, e.nodes.clone())?));
    }
    check_refs(&raw)?;
    let mut templates = HashMap::with_capacity(raw.len());
    for name in raw.keys() {
        let mut stack = Vec::new();
        templates.insert(name.clone(), Arc::new(crate::compose(&raw, name, &mut stack)?));
    }
    Ok(Store { templates })
}

/// 从内存源码构建（`Engine::add` 用，没有目录、没有缓存）。
pub(crate) fn build_srcs(srcs: &HashMap<String, Vec<Stmt>>) -> Result<Store, Error> {
    let mut raw = HashMap::with_capacity(srcs.len());
    for (name, nodes) in srcs {
        raw.insert(name.clone(), Arc::new(split_src(name, nodes.clone())?));
    }
    check_refs(&raw)?;
    let mut templates = HashMap::with_capacity(raw.len());
    for name in raw.keys() {
        let mut stack = Vec::new();
        templates.insert(name.clone(), Arc::new(crate::compose(&raw, name, &mut stack)?));
    }
    Ok(Store { templates })
}

/// 模板名归一化：`a//b.tpl` → `a/b.tpl`，去掉 `./`，`..` 往上退一级。
fn normalize(parts: &[&str]) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    for p in parts {
        match *p {
            "" | "." => {}
            ".." => {
                out.pop()?;
            }
            other => out.push(other),
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out.join("/"))
    }
}

/// `./x.tpl` / `../shared/x.tpl` 相对当前模板解析；其它写法原样当模板名。
///
/// 返回 `Err` 表示这条引用跑到了模板根之外——那多半是写错了，也可能是
/// 有人在拿用户输入拼模板名，一律拒绝。
pub(crate) fn resolve_ref(current: &str, target: &str) -> Result<String, String> {
    if !target.starts_with("./") && !target.starts_with("../") {
        return Ok(target.trim_start_matches('/').to_string());
    }
    let mut parts: Vec<&str> = current.split('/').collect();
    parts.pop(); // 去掉当前模板自己的文件名
    let mut all: Vec<&str> = parts;
    all.extend(target.split('/'));
    normalize(&all).ok_or_else(|| format!("引用 `{target}` 跑到了模板根之外"))
}

/// 递归收集一条模板里的静态引用（`{include}` 的字面量名字），带上行号。
fn refs_of(nodes: &[Stmt], line: &mut usize, out: &mut Vec<(String, usize)>) {
    for n in nodes {
        match n {
            Stmt::At(l) => *line = *l,
            Stmt::Include {
                tpl: Expr::Lit(serde_json::Value::String(s)),
                ..
            } => out.push((s.clone(), *line)),
            Stmt::If { arms, else_ } => {
                for (_, b) in arms {
                    refs_of(b, line, out);
                }
                refs_of(else_, line, out);
            }
            Stmt::For { body, else_, .. } | Stmt::ForEach { body, else_, .. } => {
                refs_of(body, line, out);
                refs_of(else_, line, out);
            }
            Stmt::Capture { body, .. } | Stmt::Block { body, .. } => refs_of(body, line, out),
            _ => {}
        }
    }
}

/// 把静态引用改写成归一化后的模板名，并校验存在性与成环。
///
/// 运行期只差一次 `HashMap` 查找，但错误全部提前到启动：少了模板、引用
/// 写错文件名、A include B include A，都不会等到某个页面被访问才炸。
fn check_refs(raw: &HashMap<String, Arc<Src>>) -> Result<(), Error> {
    let mut resolved: HashMap<String, Vec<(String, usize)>> = HashMap::new();
    for (name, src) in raw {
        let mut line = 0usize;
        let mut list = Vec::new();
        refs_of(&src.nodes, &mut line, &mut list);
        let mut fixed = Vec::with_capacity(list.len());
        for (target, l) in list {
            let full = resolve_ref(name, &target).map_err(|m| Error {
                template: name.clone(),
                line: l,
                msg: m,
            })?;
            if !raw.contains_key(&full) {
                return Err(Error {
                    template: name.clone(),
                    line: l,
                    msg: format!("引用的模板 `{full}` 未注册"),
                });
            }
            fixed.push((full, l));
        }
        resolved.insert(name.clone(), fixed);
    }

    // 成环检测：include 图上的 DFS。`{extends}` 的环由 compose 自己报。
    fn dfs(
        name: &str,
        graph: &HashMap<String, Vec<(String, usize)>>,
        stack: &mut Vec<String>,
        done: &mut HashMap<String, ()>,
    ) -> Result<(), Error> {
        if stack.iter().any(|n| n == name) {
            let mut chain = stack.clone();
            chain.push(name.to_string());
            return Err(Error {
                template: name.to_string(),
                line: 0,
                msg: format!("`{{include}}` 成环：{}", chain.join(" → ")),
            });
        }
        if done.contains_key(name) {
            return Ok(());
        }
        stack.push(name.to_string());
        if let Some(edges) = graph.get(name) {
            for (to, line) in edges {
                dfs(to, graph, stack, done).map_err(|mut e| {
                    if e.line == 0 {
                        e.line = *line;
                    }
                    e
                })?;
            }
        }
        stack.pop();
        done.insert(name.to_string(), ());
        Ok(())
    }

    let mut done = HashMap::new();
    for name in resolved.keys() {
        dfs(name, &resolved, &mut Vec::new(), &mut done)?;
    }
    Ok(())
}

// ---- 热重载 ----

/// 热重载状态。`Engine` 里是 `Option<Hot>`，没开就是零成本。
pub(crate) struct Hot {
    pub(crate) root: PathBuf,
    pub(crate) interval: Duration,
    pub(crate) state: Mutex<HotState>,
}

pub(crate) struct HotState {
    pub(crate) last: Instant,
    pub(crate) primed: bool,
    pub(crate) stamps: HashMap<String, (u64, u64)>,
    pub(crate) ast: HashMap<String, AstEntry>,
}

impl Hot {
    /// `ast` 是当前快照的 AST：交给它做增量，首次变更时不必全量重解析。
    /// `stamps` 一开始就取好，`primed = true`，避免"第一次检查误判为变更"。
    pub(crate) fn new(root: PathBuf, interval: Duration, ast: HashMap<String, AstEntry>) -> Self {
        let stamps = stamps_of(&root).unwrap_or_default();
        Self {
            root,
            interval,
            state: Mutex::new(HotState {
                last: Instant::now(),
                primed: true,
                stamps,
                ast,
            }),
        }
    }
}

/// 目录当前的 stamp 表；`stat` 失败的文件按"永远变化"处理。
pub(crate) fn stamps_of(root: &Path) -> Option<HashMap<String, (u64, u64)>> {
    let files = collect_files(root).ok()?;
    let mut out = HashMap::with_capacity(files.len());
    for (name, path) in files {
        let md = std::fs::metadata(&path).ok()?;
        out.insert(name, stamp(&md));
    }
    Some(out)
}

/// 现在的时间戳（秒）。模板里的 `$smarty.now` 用它。
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_relative_paths_against_the_template_dir() {
        assert_eq!(resolve_ref("admin/list.tpl", "row.tpl").unwrap(), "row.tpl");
        assert_eq!(
            resolve_ref("admin/list.tpl", "./row.tpl").unwrap(),
            "admin/row.tpl"
        );
        assert_eq!(
            resolve_ref("admin/part/list.tpl", "../row.tpl").unwrap(),
            "admin/row.tpl"
        );
        assert_eq!(
            resolve_ref("admin/list.tpl", "../shared/h.tpl").unwrap(),
            "shared/h.tpl"
        );
        // 根相对写法：`/x.tpl` 按 x.tpl 找
        assert_eq!(resolve_ref("a.tpl", "/x.tpl").unwrap(), "x.tpl");
    }

    #[test]
    fn resolve_refuses_to_escape_the_root() {
        let err = resolve_ref("a.tpl", "../etc/passwd").unwrap_err();
        assert!(err.contains("模板根"), "{err}");
    }

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_ne!(fnv1a(b"a"), fnv1a(b"b"));
    }
}
