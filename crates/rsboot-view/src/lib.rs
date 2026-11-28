//! `rsboot-view`：Smarty 风格的模板引擎。
//!
//! ```ignore
//! use serde_json::json;
//! use rsboot_view::Engine;
//! use std::sync::Arc;
//!
//! let view = Arc::new(Engine::new()
//!     .add("page.tpl", "<h1>{$title}</h1>{foreach $items as $it}<li>{$it.name}</li>{/foreach}")?
//!     .auto_escape(true));
//!
//! let html = view.render("page.tpl", &json!({
//!     "title": "订单",
//!     "items": [{"name": "<script>"}],
//! }))?;
//! ```
//!
//! ## 语法（Smarty）
//!
//! | 写法 | 含义 |
//! | --- | --- |
//! | `{$name}` / `{$user.name}` / `{$list[0]}` / `{$obj[$k]}` | 取值，点与下标可混用 |
//! | `{if $a > 1}…{elseif $b}…{else}…{/if}` | 条件；`and`/`or`/`not` 与 `&&`/`\|\|`/`!` 等价 |
//! | `{foreach $list as $it}` / `as $k => $v` / `{foreachelse}` | 迭代数组或对象 |
//! | `{$it@index}` `@iteration` `@first` `@last` `@total` `@key` | 循环属性，只在 `foreach` 体内有效 |
//! | `{for $i=1 to 5 step 2}…{forelse}…{/for}` | 数字区间；默认步长 1，倒着数写 `step -1` |
//! | `{capture name="x"}…{/capture}` / `append=true` | 把渲染片段存进变量（回显要 `\|raw`） |
//! | `{assign var="x" value=$y}` | 往当前作用域写变量 |
//! | `{include file="x.tpl" item=$it}` | 引用子模板；参数进子作用域，父作用域只读 |
//! | `{include "./row.tpl"}` / `{include "../shared/h.tpl"}` | 相对当前模板目录引用，不许跑出模板根 |
//! | `{extends "layout"}` + `{block name="x"}` | 模板继承，编译期展开 |
//! | `{cycle values="odd,even"}` | 按出现次序轮转取值（斑马纹、左右分栏） |
//! | `{html_select name="c" options=$list selected=$cur empty="请选择"}` | 直接产出 `<select>`，内部强制转义 |
//! | `{literal}…{/literal}` / `{* … *}` | 原样输出 / 注释 |
//! | `{myfunc a=$x b="y"}` | 调注册函数（见 [`Engine::func`]） |
//!
//! 修饰符（可链式）：`escape` `raw` `upper` `lower` `trim` `capitalize`
//! `nl2br` `default:x` `length`/`count` `truncate:n[:suffix]` `substr:start[:len]`
//! `replace:"a":"b"` `split:","` `join:sep` `pad:n:"0"` `repeat:n`
//! `number_format:2` `date:"%Y-%m-%d"` `urlencode` `strip_tags` `json`。
//!
//! 内建变量：`$smarty.now`（渲染时刻的 unix 秒）、`$smarty.now_date`（UTC 串）、
//! `$smarty.tpl`（当前模板名）、`$smarty.version`；带请求上下文渲染时还有
//! `$smarty.get` / `$smarty.params` / `$smarty.path` / `$smarty.method`
//! （见 [`Engine::render_request`] 与 [`request_context`]）。
//!
//! ## 错误定位
//!
//! 语法错误在 `add`/`add_dir` 时就报（带行号）；运行期错误（除零、缺模板、
//! 修饰符参数不对）也会带 `模板:行`——每个标签在 AST 里留了一个行号标记，
//! 执行到就更新，出错直接取当前值。
//!
//! ## 引用与继承的时机
//!
//! `{extends}`/`{block}` 在编译期展开；`{include}` 的**字面量名字**也在编译期
//! 解析成正式模板名，并校验"存在 + 不成环"，错误在启动时就暴露，而不是等
//! 某个页面被访问。运行期仍是一次 `HashMap` 查找（几十纳秒），不复制子模板
//! AST——`header.tpl` 被 50 个页面引用也只占一份内存。
//! 动态名字（`{include file=$x}`）只能留到运行期，靠 32 层深度兜底。
//!
//! ## 为什么不是"引一个模板引擎"
//!
//! 核心的承诺是零第三方依赖，引 `minijinja` 或 `tera` 都会打破它；而两者
//! 的语法是 Jinja（`{{ }}` / `{% %}`），和 Smarty 的 `{$x}` 不是一个东西。
//! 所以**架构照 minijinja**（不建 token 流、AST 一次编译多次渲染、作用域
//! 栈、错误带位置），**语法照 Smarty**，实现自己写。
//!
//! ## 安全默认
//!
//! `auto_escape` 默认**开**：`{$user.name}` 里的 `<script>` 会被转义。
//! 要插 HTML 片段的字段显式写 `|raw`——把"危险"写成显式的，比把"安全"
//! 写成显式的更不容易漏。
//!
//! Smarty 默认不转义（PHP 时代的选择），这里故意不跟随：忘记写 `|escape`
//! 的代价是 XSS，忘记写 `|raw` 的代价只是看到一个 `&lt;div&gt;`。

mod parse;
mod render;
mod store;

pub use parse::SyntaxError;
pub use render::escape_html;
pub use store::now_secs;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use parse::{Expr, Stmt};
use serde_json::{Map, Value};
use store::{AstEntry, Hot, Store};

/// 模板函数：参数按名字树传进来，返回要输出的值。
pub type Func = Box<dyn Fn(&[(&str, Value)]) -> Result<Value, String> + Send + Sync>;

/// 模板错误：语法错误带行号，渲染错误带模板名。
#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    pub template: String,
    /// 1 起；0 表示运行期错误（此时行号不可用）
    pub line: usize,
    pub msg: String,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.line > 0 {
            write!(f, "{}:{}: {}", self.template, self.line, self.msg)
        } else {
            write!(f, "{}: {}", self.template, self.msg)
        }
    }
}

impl std::error::Error for Error {}

impl Error {
    /// 交给 HTTP 层：模板坏了是服务端问题（`500` + `5000`），
    /// 不该把语法细节回给浏览器。
    pub fn to_api_error(&self) -> rsboot::web::error::ApiError {
        rsboot::web::error::ApiError::with_code(
            rsboot::web::code::INTERNAL,
            format!("模板渲染失败（{}）", self),
        )
    }
}

/// 让 handler 能直接 `?` 掉模板错误：`Result<Response, Error>` 里的 `Err`
/// 会被转成 500 + 5000 的统一错误体，不会把模板源码或数据泄露给浏览器。
impl rsboot::web::IntoResponse for Error {
    fn into_response(self) -> rsboot::web::Response {
        self.to_api_error().into_response()
    }
}

/// 编译好的模板集合。
///
/// 快照放在 `RwLock<Arc<Store>>` 后面：渲染只借一瞬就克隆 `Arc`（一次原子加），
/// 之后整段渲染都不持锁；热重载换的是整个 `Arc`，运行中的渲染继续用旧快照，
/// 不存在"读到一半模板被换掉"的中间态。
pub struct Engine {
    store: RwLock<Arc<Store>>,
    /// `add` 进来的源码。重建时要用它重算（目录模式下为空）。
    srcs: HashMap<String, Vec<Stmt>>,
    funcs: HashMap<String, Func>,
    /// 每次渲染都会先铺进根作用域的常量（站点名、版本号之类）。
    globals: HashMap<String, Value>,
    auto_escape: bool,
    /// `add_dir` 的根目录，热重载与缓存都靠它。
    dir_root: Option<PathBuf>,
    /// `.tplc` 缓存文件路径。
    cache: Option<PathBuf>,
    hot: Option<Hot>,
    /// 上一次构建的 AST，热重载时用来做增量。
    ast: HashMap<String, AstEntry>,
    /// 热重载失败的原因（只记最近一次），运维接口可以取出来。
    reload_error: Mutex<Option<String>>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = self.store.read().map(|s| s.len()).unwrap_or(0);
        f.debug_struct("Engine")
            .field("templates", &n)
            .field("srcs", &self.srcs.len())
            .field("funcs", &self.funcs.len())
            .field("globals", &self.globals.len())
            .field("auto_escape", &self.auto_escape)
            .field("hot_reload", &self.hot.is_some())
            .field("cache", &self.cache)
            .finish()
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        Self {
            store: RwLock::new(Arc::new(Store::default())),
            srcs: HashMap::new(),
            funcs: HashMap::new(),
            globals: HashMap::new(),
            auto_escape: true,
            dir_root: None,
            cache: None,
            hot: None,
            ast: HashMap::new(),
            reload_error: Mutex::new(None),
        }
    }

    // ---- 构建 ----

    /// 是否自动做 HTML 转义（默认开，见模块文档）。
    pub fn auto_escape(mut self, on: bool) -> Self {
        self.auto_escape = on;
        self
    }

    /// 注册一批全局常量。业务数据里的同名字段会覆盖它。
    ///
    /// 用途很窄但很省事：`{$site.name}`、`{$app.version}` 这种每个页面都要的
    /// 东西，不必每个 handler 都往 `data` 里塞一遍。
    pub fn globals(mut self, g: &Value) -> Self {
        if let Value::Object(m) = g {
            for (k, v) in m {
                self.globals.insert(k.clone(), v.clone());
            }
        }
        self
    }

    /// 注册一份模板源码。
    ///
    /// 每 `add` 一次全量重建：几十个模板、毫秒级，换来"`add` 顺序无关"。
    /// `{extends}` / `{include}` 指向的模板可以后注册——重建时会重新校验。
    pub fn add(mut self, name: impl Into<String>, src: &str) -> Result<Self, Error> {
        let name = name.into();
        let nodes = parse::parse(src).map_err(|e| Error {
            template: name.clone(),
            line: e.line,
            msg: e.msg,
        })?;
        self.srcs.insert(name, nodes);
        self.rebuild()?;
        Ok(self)
    }

    /// 递归加载目录下的 `*.tpl` / `*.html`，模板名用相对路径（`/` 分隔）。
    ///
    /// 启动期一次编译完，运行期零 IO。要"改了模板不重启"用
    /// [`Engine::hot_reload`]；缓存文件默认写到 `<root>/.tplc`，可用
    /// [`Engine::cache_dir`] 改位置或关掉。
    pub fn add_dir(mut self, root: impl AsRef<Path>) -> Result<Self, Error> {
        let root = root.as_ref().to_path_buf();
        let cache = root.join(".tplc");
        let (st, ast) = store::build(&root, Some(&cache), None)?;
        self.dir_root = Some(root);
        self.cache = Some(cache);
        *self.store.get_mut().unwrap_or_else(|e| e.into_inner()) = Arc::new(st);
        self.ast = ast;
        Ok(self)
    }

    /// 不用缓存 / 换缓存位置（`None` 表示不写）。
    ///
    /// 缓存的是 **AST**，不是可执行代码：省掉词法 + 语法解析，继承展开与
    /// 引用校验照做。文件由引擎自己写，别提交进版本库。
    pub fn cache_dir(mut self, path: Option<impl AsRef<Path>>) -> Result<Self, Error> {
        let path = path.map(|p| p.as_ref().to_path_buf());
        let Some(root) = self.dir_root.clone() else {
            return Err(Error {
                template: "<engine>".to_string(),
                line: 0,
                msg: "`cache_dir` 只能配合 `add_dir` 使用".to_string(),
            });
        };
        let (st, ast) = store::build(&root, path.as_deref(), None)?;
        self.cache = path;
        *self.store.get_mut().unwrap_or_else(|e| e.into_inner()) = Arc::new(st);
        self.ast = ast;
        Ok(self)
    }

    /// 打开模板热重载：默认 1 秒检查一次目录，有变化就重建。
    ///
    /// 检查是 `stat` 一遍目录，几百个模板也就几十微秒，所以放在渲染前做，
    /// 不需要后台线程和 watcher。代价是"最多晚一个检查间隔生效"。
    pub fn hot_reload(mut self, on: bool) -> Self {
        self.hot = if on {
            self.dir_root
                .clone()
                .map(|r| Hot::new(r, Duration::from_secs(1), self.ast.clone()))
        } else {
            None
        };
        self
    }

    /// 热重载的检查间隔。间隔越短越"实时"，`stat` 越频繁。
    pub fn hot_reload_interval(mut self, d: Duration) -> Self {
        self.hot = self
            .dir_root
            .clone()
            .map(|r| Hot::new(r, d, self.ast.clone()));
        self
    }

    /// 注册一个模板函数，模板里写 `{名字 参数="值"}`。
    pub fn func<F>(mut self, name: impl Into<String>, f: F) -> Self
    where
        F: Fn(&[(&str, Value)]) -> Result<Value, String> + Send + Sync + 'static,
    {
        self.funcs.insert(name.into(), Box::new(f));
        self
    }

    /// 从内存源码重建（`add` 模式）。
    fn rebuild(&mut self) -> Result<(), Error> {
        let st = store::build_srcs(&self.srcs)?;
        *self.store.get_mut().unwrap_or_else(|e| e.into_inner()) = Arc::new(st);
        Ok(())
    }

    /// 立刻重读目录并替换快照（部署后手动触发，或测试用）。
    pub fn reload(&self) -> Result<(), Error> {
        let Some(root) = self.dir_root.clone() else {
            return Ok(());
        };
        let (s, ast) = store::build(&root, self.cache.as_deref(), None)?;
        *self.store.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(s);
        if let Some(h) = &self.hot {
            let mut st = h.state.lock().unwrap_or_else(|e| e.into_inner());
            st.ast = ast;
            st.stamps = store::stamps_of(&root).unwrap_or_default();
            st.primed = true;
            st.last = std::time::Instant::now();
        }
        Ok(())
    }

    /// 最近一次热重载失败的原因。模板语法写错时旧快照继续服务，这里留个痕。
    pub fn reload_error(&self) -> Option<String> {
        self.reload_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 取当前快照；开了热重载就先检查一遍。
    ///
    /// 到了检查时间点时只有一个线程去 `stat`，其余线程直接拿旧快照——
    /// 这把锁只保护"上次检查时间"，不参与渲染。
    fn snapshot(&self) -> Arc<Store> {
        if let Some(h) = &self.hot {
            let due = {
                let st = h.state.lock().unwrap_or_else(|e| e.into_inner());
                st.last.elapsed() >= h.interval
            };
            if due {
                self.maybe_reload(h);
            }
        }
        self.store
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn maybe_reload(&self, h: &Hot) {
        let mut st = h.state.lock().unwrap_or_else(|e| e.into_inner());
        // 双检：多个线程同时发现"到点了"时，只有一个真的去扫目录
        if st.last.elapsed() < h.interval {
            return;
        }
        st.last = std::time::Instant::now();
        let Some(stamps) = store::stamps_of(&h.root) else {
            return;
        };
        if !st.primed {
            st.stamps = stamps;
            st.primed = true;
            return;
        }
        if stamps == st.stamps {
            return;
        }
        match store::build(&h.root, self.cache.as_deref(), Some(&st.ast)) {
            Ok((s, ast)) => {
                *self.store.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(s);
                st.ast = ast;
                st.stamps = stamps;
                *self.reload_error.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
            Err(e) => {
                // 旧快照继续服务：一个模板写错不该让线上 500 一片
                *self.reload_error.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(e.to_string());
            }
        }
    }

    // ---- 渲染 ----

    pub fn template(&self, name: &str) -> Option<Template<'_>> {
        let store = self.snapshot();
        if !store.templates.contains_key(name) {
            return None;
        }
        Some(Template {
            engine: self,
            store,
            name: name.to_string(),
        })
    }

    /// 一步渲染。
    pub fn render(&self, name: &str, data: &Value) -> Result<String, Error> {
        self.render_with(name, data, &Value::Null)
    }

    /// 带请求上下文渲染。`ctx` 会并进 `$smarty`（`get` / `params` / `path` …）。
    ///
    /// 上下文只属于这一次渲染，不落进 `Engine`，所以同一个 `Engine`
    /// 可以被所有请求共享。
    pub fn render_with(&self, name: &str, data: &Value, ctx: &Value) -> Result<String, Error> {
        match self.template(name) {
            Some(t) => t.render_with(data, ctx),
            None => Err(not_found(name)),
        }
    }

    /// 渲染到已有缓冲区：高频路径（列表页、SSE 片段）能省一次 `String` 分配。
    pub fn render_into(&self, name: &str, data: &Value, out: &mut String) -> Result<(), Error> {
        self.render_into_with(name, data, &Value::Null, out)
    }

    pub fn render_into_with(
        &self,
        name: &str,
        data: &Value,
        ctx: &Value,
        out: &mut String,
    ) -> Result<(), Error> {
        match self.template(name) {
            Some(t) => t.render_into_with(data, ctx, out),
            None => Err(not_found(name)),
        }
    }

    /// 用请求渲染：把 query / 路径参数 / path / method 装进 `$smarty`。
    ///
    /// 只读 `Request`，不碰 body：模板里要回显表单字段就自己传 `data`——
    /// 表单原始 body 直接进模板是 XSS 与隐私双重风险，不在这里开口子。
    pub fn render_request(
        &self,
        name: &str,
        data: &Value,
        req: &rsboot::web::Request<'_>,
    ) -> Result<String, Error> {
        let ctx = request_context(req);
        self.render_with(name, data, &ctx)
    }

    /// 渲染成可以直接返回的 HTML 响应。
    ///
    /// handler 里写 `Ok(view.render_response("list", &data)?)` 就够了——错误
    /// 走 [`Error::to_api_error`]，模板坏了是 500 + 5000，细节不落到浏览器。
    pub fn render_response(
        &self,
        name: &str,
        data: &Value,
    ) -> Result<rsboot::web::Response, Error> {
        let html = self.render(name, data)?;
        Ok(rsboot::web::Response::html(html))
    }

    /// 带请求上下文 + 直接给 HTML 响应。页面型 handler 最常用的一条路。
    pub fn render_request_response(
        &self,
        name: &str,
        data: &Value,
        req: &rsboot::web::Request<'_>,
    ) -> Result<rsboot::web::Response, Error> {
        let html = self.render_request(name, data, req)?;
        Ok(rsboot::web::Response::html(html))
    }

    pub(crate) fn global_values(&self) -> &HashMap<String, Value> {
        &self.globals
    }

    pub(crate) fn lookup_func(&self, name: &str) -> Option<&Func> {
        self.funcs.get(name)
    }

    pub(crate) fn is_auto_escape(&self) -> bool {
        self.auto_escape
    }
}

fn not_found(name: &str) -> Error {
    Error {
        template: name.to_string(),
        line: 0,
        msg: "找不到模板".to_string(),
    }
}

/// 把请求里能被模板安全读到的部分转成 `$smarty` 上下文的 JSON。
///
/// `get` 是 query 参数（百分号解码、`+` 当空格），`params` 是路径参数。
/// 同名多值只保留第一个——模板不是处理表单数组的地方。
pub fn request_context(req: &rsboot::web::Request<'_>) -> Value {
    let mut get = Map::new();
    for pair in req.query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let key = percent_decode_plus(k);
        if key.is_empty() || get.contains_key(&key) {
            continue;
        }
        get.insert(key, Value::String(percent_decode_plus(v)));
    }
    let mut params = Map::new();
    for (k, v) in &req.params {
        params.insert(k.clone(), Value::String(v.clone()));
    }

    let mut sm = Map::new();
    sm.insert("get".into(), Value::Object(get));
    sm.insert("params".into(), Value::Object(params));
    sm.insert("path".into(), Value::String(req.path.to_string()));
    sm.insert("method".into(), Value::String(req.method.to_string()));
    if let Some(id) = req.request_id() {
        sm.insert("request_id".into(), Value::String(id.to_string()));
    }
    Value::Object(sm)
}

/// query 里的 `+` 是空格（`application/x-www-form-urlencoded` 的约定）。
fn percent_decode_plus(s: &str) -> String {
    if s.contains('+') {
        rsboot::web::path::percent_decode(&s.replace('+', " "))
    } else {
        rsboot::web::path::percent_decode(s)
    }
}

/// 一份模板的渲染入口。
///
/// 持有的是**快照**而不是 `Engine` 的锁：`Template` 活得再久，也不会挡住热重载。
pub struct Template<'a> {
    engine: &'a Engine,
    store: Arc<Store>,
    name: String,
}

impl Template<'_> {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn render(&self, data: &Value) -> Result<String, Error> {
        self.render_with(data, &Value::Null)
    }

    pub fn render_with(&self, data: &Value, ctx: &Value) -> Result<String, Error> {
        let mut out = String::with_capacity(1024);
        self.render_into_with(data, ctx, &mut out)?;
        Ok(out)
    }

    /// 渲染追加到已有缓冲区（模板片段拼接、SSE 分片输出都用它）。
    pub fn render_into(&self, data: &Value, out: &mut String) -> Result<(), Error> {
        self.render_into_with(data, &Value::Null, out)
    }

    pub fn render_into_with(
        &self,
        data: &Value,
        ctx: &Value,
        out: &mut String,
    ) -> Result<(), Error> {
        let nodes = self.store.nodes(self.name.as_str()).unwrap_or(&[]);
        let mut rt = render::Rt::new(self.engine, &self.store, &self.name, data, ctx);
        rt.run_into(nodes, out)
    }
}

// ---- 编译期展开（继承 + 引用解析）----

/// 一份模板拆出来的三件事。
pub(crate) struct Src {
    /// 没有 `{extends}` 时就是完整节点列表；有的话这份内容会被丢弃，
    /// 只剩 `blocks` 起作用（Smarty 的语义：子模板除 block 外不输出）。
    pub(crate) nodes: Vec<Stmt>,
    pub(crate) extends: Option<String>,
    pub(crate) blocks: HashMap<String, Vec<Stmt>>,
}

/// 从解析结果里拆出 `{extends}` 与顶层 `{block}`，并把静态引用的相对路径归一化。
pub(crate) fn split_src(name: &str, nodes: Vec<Stmt>) -> Result<Src, Error> {
    let mut extends = None;
    for n in &nodes {
        if let Stmt::Extends(e) = n {
            if extends.is_some() {
                return Err(Error {
                    template: name.to_string(),
                    line: 0,
                    msg: "一个模板只能有一个 `{extends}`".to_string(),
                });
            }
            match e {
                Expr::Lit(Value::String(s)) => extends = Some(s.clone()),
                _ => {
                    return Err(Error {
                        template: name.to_string(),
                        line: 0,
                        msg: "`{extends}` 的参数必须是字符串字面量".to_string(),
                    })
                }
            }
        }
    }
    let nodes = resolve_paths(name, nodes)?;
    if extends.is_none() {
        // 没有继承：`{block}` 只是个透明容器，留在原位置照常渲染。
        return Ok(Src {
            nodes,
            extends: None,
            blocks: HashMap::new(),
        });
    }
    let mut blocks = HashMap::new();
    for n in &nodes {
        if let Stmt::Block { name: bn, body } = n {
            blocks.insert(bn.clone(), body.clone());
        }
    }
    let body = nodes
        .into_iter()
        .filter(|n| !matches!(n, Stmt::Extends(_) | Stmt::Block { .. }))
        .collect();
    Ok(Src {
        nodes: body,
        extends,
        blocks,
    })
}

/// 把静态 `{include "./x.tpl"}` 的模板名换成归一化后的名字。
///
/// 只动字面量：动态名字在编译期根本不知道是什么，留到运行期。
fn resolve_paths(tpl: &str, nodes: Vec<Stmt>) -> Result<Vec<Stmt>, Error> {
    nodes.into_iter().map(|n| resolve_one(tpl, n)).collect()
}

fn resolve_one(tpl: &str, n: Stmt) -> Result<Stmt, Error> {
    Ok(match n {
        Stmt::Include { tpl: e, params } => Stmt::Include {
            tpl: fix_include_name(tpl, e)?,
            params,
        },
        Stmt::If { arms, else_ } => Stmt::If {
            arms: arms
                .into_iter()
                .map(|(c, b)| Ok((c, resolve_paths(tpl, b)?)))
                .collect::<Result<Vec<_>, Error>>()?,
            else_: resolve_paths(tpl, else_)?,
        },
        Stmt::For {
            var,
            start,
            end,
            step,
            body,
            else_,
        } => Stmt::For {
            var,
            start,
            end,
            step,
            body: resolve_paths(tpl, body)?,
            else_: resolve_paths(tpl, else_)?,
        },
        Stmt::ForEach {
            iter,
            key,
            val,
            body,
            else_,
        } => Stmt::ForEach {
            iter,
            key,
            val,
            body: resolve_paths(tpl, body)?,
            else_: resolve_paths(tpl, else_)?,
        },
        Stmt::Capture { name, append, body } => Stmt::Capture {
            name,
            append,
            body: resolve_paths(tpl, body)?,
        },
        Stmt::Block { name, body } => Stmt::Block {
            name,
            body: resolve_paths(tpl, body)?,
        },
        other => other,
    })
}

fn fix_include_name(tpl: &str, e: Expr) -> Result<Expr, Error> {
    match e {
        Expr::Lit(Value::String(s)) => {
            let full = store::resolve_ref(tpl, &s).map_err(|m| Error {
                template: tpl.to_string(),
                line: 0,
                msg: m,
            })?;
            Ok(Expr::Lit(Value::String(full)))
        }
        other => Ok(other),
    }
}

/// 递归展开一个模板（`{extends}`）。
pub(crate) fn compose(
    raw: &HashMap<String, Arc<Src>>,
    name: &str,
    stack: &mut Vec<String>,
) -> Result<Vec<Stmt>, Error> {
    let Some(src) = raw.get(name) else {
        return Err(Error {
            template: name.to_string(),
            line: 0,
            msg: "找不到模板".to_string(),
        });
    };
    let Some(parent) = &src.extends else {
        return Ok(src.nodes.clone());
    };
    // `{extends "./layout.tpl"}` 也要按当前模板目录解析
    let parent = store::resolve_ref(name, parent).map_err(|m| Error {
        template: name.to_string(),
        line: 0,
        msg: m,
    })?;
    if !raw.contains_key(&parent) {
        return Err(Error {
            template: name.to_string(),
            line: 0,
            msg: format!("`{{extends}}` 的模板 `{parent}` 未注册"),
        });
    }
    if stack.iter().any(|n| n == name) {
        let mut chain = stack.join(" → ");
        chain.push_str(" → ");
        chain.push_str(name);
        return Err(Error {
            template: name.to_string(),
            line: 0,
            msg: format!("`{{extends}}` 成环：{chain}"),
        });
    }
    stack.push(name.to_string());
    let r = compose(raw, &parent, stack);
    stack.pop();
    let parent_nodes = r?;
    Ok(replace_blocks(&parent_nodes, &src.blocks))
}

/// 把父模板里的 `{block}` 换成子模板的版本；没被覆盖的保留默认内容。
fn replace_blocks(nodes: &[Stmt], overrides: &HashMap<String, Vec<Stmt>>) -> Vec<Stmt> {
    nodes
        .iter()
        .map(|n| match n {
            Stmt::Block { name, body } => match overrides.get(name) {
                // 覆盖体不再递归替换：它里面若又写同名 block 会无限套娃。
                Some(b) => Stmt::Block {
                    name: name.clone(),
                    body: b.clone(),
                },
                None => Stmt::Block {
                    name: name.clone(),
                    body: replace_blocks(body, overrides),
                },
            },
            // 父模板的 block 可能藏在 if / foreach 里，也得进去换。
            Stmt::If { arms, else_ } => Stmt::If {
                arms: arms
                    .iter()
                    .map(|(c, b)| (c.clone(), replace_blocks(b, overrides)))
                    .collect(),
                else_: replace_blocks(else_, overrides),
            },
            Stmt::ForEach {
                iter,
                key,
                val,
                body,
                else_,
            } => Stmt::ForEach {
                iter: iter.clone(),
                key: key.clone(),
                val: val.clone(),
                body: replace_blocks(body, overrides),
                else_: replace_blocks(else_, overrides),
            },
            Stmt::For {
                var,
                start,
                end,
                step,
                body,
                else_,
            } => Stmt::For {
                var: var.clone(),
                start: start.clone(),
                end: end.clone(),
                step: step.clone(),
                body: replace_blocks(body, overrides),
                else_: replace_blocks(else_, overrides),
            },
            Stmt::Capture { name, append, body } => Stmt::Capture {
                name: name.clone(),
                append: *append,
                body: replace_blocks(body, overrides),
            },
            other => other.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn engine() -> Engine {
        Engine::new()
    }

    /// 每个测试一个临时目录，名字带 pid + 用途，并行跑不会撞。
    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "rsboot-view-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(dir: &Path, name: &str, src: &str) {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, src).unwrap();
    }

    #[test]
    fn prints_variables_and_paths() {
        let e = engine().add("a", "{$user.name}/{$list[0]}").unwrap();
        let out = e
            .render("a", &json!({"user": {"name": "张三"}, "list": [7, 8]}))
            .unwrap();
        assert_eq!(out, "张三/7");
    }

    #[test]
    fn missing_field_is_empty_not_an_error() {
        let e = engine().add("a", "[{$nope}]").unwrap();
        assert_eq!(e.render("a", &json!({})).unwrap(), "[]");
    }

    #[test]
    fn auto_escape_is_on_by_default() {
        let e = engine().add("a", "{$x}").unwrap();
        assert_eq!(
            e.render("a", &json!({"x": "<script>alert(1)</script>"}))
                .unwrap(),
            "&lt;script&gt;alert(1)&lt;/script&gt;"
        );
    }

    #[test]
    fn raw_and_escape_are_not_doubly_escaped() {
        let e = engine()
            .add("a", "{$x|raw}|{$y|escape}|{$z}")
            .unwrap();
        let out = e
            .render("a", &json!({"x": "<b>", "y": "<b>", "z": "<b>"}))
            .unwrap();
        assert_eq!(out, "<b>|&lt;b&gt;|&lt;b&gt;");
    }

    #[test]
    fn if_elseif_else_and_operators() {
        let e = engine()
            .add(
                "a",
                "{if $n > 10}big{elseif $n == 10}ten{else}small{/if}|{if $ok and not $off}ok{/if}",
            )
            .unwrap();
        assert_eq!(e.render("a", &json!({"n": 11, "ok": true})).unwrap(), "big|ok");
        assert_eq!(e.render("a", &json!({"n": 10})).unwrap(), "ten|");
        assert_eq!(e.render("a", &json!({"n": 3})).unwrap(), "small|");
    }

    #[test]
    fn foreach_over_array_and_object_with_else() {
        let e = engine()
            .add(
                "a",
                "{foreach $rows as $i => $r}[{$i}:{$r.name}]{foreachelse}none{/foreach}",
            )
            .unwrap();
        let out = e
            .render("a", &json!({"rows": [{"name": "a"}, {"name": "b"}]}))
            .unwrap();
        assert_eq!(out, "[0:a][1:b]");
        assert_eq!(e.render("a", &json!({"rows": []})).unwrap(), "none");

        // 对象按字段名迭代
        let out = e
            .render("a", &json!({"rows": {"x": {"name": "X"}}}))
            .unwrap();
        assert_eq!(out, "[x:X]");
    }

    #[test]
    fn modifiers() {
        let e = engine()
            .add(
                "a",
                r#"{$s|upper}|{$s|truncate:3}|{$n|default:0}|{$list|length}|{$list|join:"-"}|{$v|json}"#,
            )
            .unwrap();
        let out = e
            .render(
                "a",
                &json!({"s": "abcdef", "n": null, "list": [1,2,3], "v": {"k": 1}}),
            )
            .unwrap();
        assert_eq!(out, "ABCDEF|abc...|0|3|1-2-3|{\"k\":1}");
    }

    #[test]
    fn include_shares_the_parent_scope() {
        let e = engine()
            .add("row", "<td>{$name}</td>")
            .unwrap()
            .add("page", "<tr>{include \"row\"}</tr>")
            .unwrap();
        assert_eq!(
            e.render("page", &json!({"name": "李四"})).unwrap(),
            "<tr><td>李四</td></tr>"
        );
    }

    #[test]
    fn include_cycle_is_caught_at_registration() {
        // 自引用在 `add` 时就报，不必等某个页面被访问
        let err = engine().add("a", "{include \"a\"}").unwrap_err();
        assert!(err.msg.contains("成环"), "{err}");
    }

    /// 两个模板互相 include：目录模式下一次构建就能看出环。
    #[test]
    fn include_cycle_across_two_templates() {
        let dir = tmpdir("cycle");
        write(&dir, "a.tpl", "{include \"b.tpl\"}");
        write(&dir, "b.tpl", "{include \"a.tpl\"}");
        let err = Engine::new().add_dir(&dir).unwrap_err();
        assert!(err.msg.contains("成环"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn custom_function() {
        let e = engine()
            .func("url", |args| {
                let p = args.iter().find(|(k, _)| *k == "path").map(|(_, v)| v.clone());
                Ok(match p {
                    Some(Value::String(s)) => json!(format!("/app{s}")),
                    _ => json!("/app"),
                })
            })
            .add("a", "<a href=\"{url path=$p}\">x</a>")
            .unwrap();
        assert_eq!(
            e.render("a", &json!({"p": "/orders"})).unwrap(),
            "<a href=\"/app/orders\">x</a>"
        );
    }

    #[test]
    fn unknown_function_and_modifier_are_errors() {
        let e = engine().add("a", "{nope}").unwrap();
        assert!(e.render("a", &json!({})).unwrap_err().msg.contains("没有注册函数"));
        let e = engine().add("b", "{$x|nope}").unwrap();
        assert!(e.render("b", &json!({"x": 1})).unwrap_err().msg.contains("修饰符"));
    }

    #[test]
    fn syntax_error_carries_the_line() {
        let err = engine().add("a", "ok\n{if $x}\n").unwrap_err();
        assert_eq!(err.line, 2, "{err}");
        assert_eq!(err.template, "a");
    }

    #[test]
    fn empty_template_and_plain_html() {
        let e = engine().add("a", "").unwrap();
        assert_eq!(e.render("a", &json!({})).unwrap(), "");
        let e = engine()
            .add("b", "<style>body { color: red }</style>{$x}")
            .unwrap();
        assert_eq!(
            e.render("b", &json!({"x": 1})).unwrap(),
            "<style>body { color: red }</style>1"
        );
    }

    // ---- 继承与 assign ----

    fn layout() -> Engine {
        Engine::new()
            .add(
                "layout",
                "<html><head><title>{block name=\"title\"}默认标题{/block}</title></head>\
                 <body>{block name=\"content\"}空{/block}</body></html>",
            )
            .unwrap()
    }

    #[test]
    fn extends_overrides_blocks_and_keeps_defaults() {
        let e = layout()
            .add(
                "page",
                "{extends \"layout\"}page 里这段文字不输出\
                 {block name=\"title\"}订单{/block}\
                 {block name=\"content\"}<ul>{$n}</ul>{/block}",
            )
            .unwrap();
        let out = e.render("page", &json!({"n": 3})).unwrap();
        assert_eq!(
            out,
            "<html><head><title>订单</title></head><body><ul>3</ul></body></html>"
        );
    }

    #[test]
    fn parent_renders_its_own_defaults() {
        let e = layout();
        assert_eq!(
            e.render("layout", &json!({})).unwrap(),
            "<html><head><title>默认标题</title></head><body>空</body></html>"
        );
    }

    #[test]
    fn missing_parent_is_an_error() {
        let e = engine().add("a", "{extends \"b\"}").unwrap_err();
        assert!(e.msg.contains("未注册"), "{e}");
    }

    #[test]
    fn extends_cycle_is_an_error() {
        // 逐个 `add` 时环一定先撞上"未注册"，环检测只在 add_dir
        // 全量登记后才有机会上场——这里正是那个场景。
        let dir = tmpdir("extcycle");
        write(&dir, "a.tpl", "{extends \"b.tpl\"}");
        write(&dir, "b.tpl", "{extends \"a.tpl\"}");
        let err = Engine::new().add_dir(&dir).unwrap_err();
        assert!(err.msg.contains("成环"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn extends_must_be_literal() {
        let err = Engine::new().add("a", "{extends $tpl}").unwrap_err();
        assert!(err.msg.contains("字面量"), "{err}");
    }

    #[test]
    fn assign_writes_into_the_current_scope() {
        let e = engine()
            .add("a", "{assign var=\"total\" value=$a + $b}{$total}")
            .unwrap();
        assert_eq!(e.render("a", &json!({"a": 2, "b": 40})).unwrap(), "42");
    }

    #[test]
    fn json_values_render_as_json() {
        let e = engine().add("a", "{$o}").unwrap().auto_escape(false);
        assert_eq!(e.render("a", &json!({"o": {"a": 1}})).unwrap(), "{\"a\":1}");
    }

    // ---- v1.0：循环属性、for、capture、带参 include、cycle、select、修饰符、globals ----

    #[test]
    fn for_loop_steps_and_handles_else() {
        let e = engine().add("a", "{for $i=1 to 5 step 2}{$i},{/for}").unwrap();
        assert_eq!(e.render("a", &json!({})).unwrap(), "1,3,5,");
        // 倒着数要显式 `step -1`
        let e = engine().add("b", "{for $i=3 to 1 step -1}{$i}{forelse}none{/for}").unwrap();
        assert_eq!(e.render("b", &json!({})).unwrap(), "321");
        // 不写 step 时方向不自动反转：起点大于终点就是一次都不跑，走 forelse
        let e = engine().add("c", "{for $i=3 to 1}x{forelse}none{/for}").unwrap();
        assert_eq!(e.render("c", &json!({})).unwrap(), "none");
        // step 为 0 是模板写错了，必须报错而不是死循环
        let e = engine().add("d", "{for $i=1 to 3 step 0}x{/for}").unwrap();
        assert!(e.render("d", &json!({})).unwrap_err().msg.contains("step"));
    }

    #[test]
    fn foreach_loop_properties() {
        let e = engine()
            .add(
                "a",
                "{foreach $list as $it}{$it@index}/{$it@iteration}/{$it@first}/{$it@last}/{$it@total};{/foreach}",
            )
            .unwrap();
        assert_eq!(
            e.render("a", &json!({"list": ["x", "y", "z"]})).unwrap(),
            "0/1/true/false/3;1/2/false/false/3;2/3/false/true/3;"
        );
        // 对象迭代时 @key 是字段名
        let e = engine().add("b", "{foreach $o as $v}{$v@key}={$v};{/foreach}").unwrap();
        assert_eq!(e.render("b", &json!({"o": {"a": 1}})).unwrap(), "a=1;");
        // 嵌套同名变量：内层作用域优先
        let e = engine()
            .add("c", "{foreach $a as $i}{foreach $b as $i}{$i@index}{/foreach}{/foreach}")
            .unwrap();
        assert_eq!(e.render("c", &json!({"a": [1,2], "b": [9]})).unwrap(), "00");
    }

    #[test]
    fn capture_stores_rendered_fragment() {
        let e = engine()
            .add(
                "a",
                "{capture name=\"head\"}<b>{$t}</b>{/capture}[{$head}]",
            )
            .unwrap();
        // capture 存的是**已渲染的 HTML 片段**，回显要 `|raw`：
        // 自动转义对"已经由引擎渲染过的片段"同样生效，这是有意的——
        // 否则 capture 就成了绕过转义的暗门。
        assert_eq!(e.render("a", &json!({"t": "x"})).unwrap(), "[&lt;b&gt;x&lt;/b&gt;]");
        let e = engine()
            .add("a2", "{capture name=\"head\"}<b>{$t}</b>{/capture}[{$head|raw}]")
            .unwrap();
        assert_eq!(e.render("a2", &json!({"t": "x"})).unwrap(), "[<b>x</b>]");
        let e = engine()
            .add("b", "{capture name=\"h\"}a{/capture}{capture name=\"h\" append=true}b{/capture}{$h}")
            .unwrap();
        assert_eq!(e.render("b", &json!({})).unwrap(), "ab");
    }

    #[test]
    fn include_takes_params_and_does_not_leak_back() {
        let e = engine()
            .add("row", "<td>{$label}:{$item.name}</td>")
            .unwrap()
            .add("page", "{include file=\"row\" item=$it label=\"名字\"}{$label}")
            .unwrap();
        assert_eq!(
            e.render("page", &json!({"it": {"name": "张三"}})).unwrap(),
            "<td>名字:张三</td>"
        );
        // 子模板里 assign 的东西不回到父层
        let e = engine()
            .add("row2", "{assign var=\"tmp\" value=1}")
            .unwrap()
            .add("page2", "{include file=\"row2\"}[{$tmp}]")
            .unwrap();
        assert_eq!(e.render("page2", &json!({})).unwrap(), "[]");
    }

    #[test]
    fn cycle_alternates_and_can_reset() {
        let e = engine()
            .add("a", "{for $i=1 to 3}{cycle values=\"odd,even\"},{/for}")
            .unwrap();
        assert_eq!(e.render("a", &json!({})).unwrap(), "odd,even,odd,");
        let e = engine()
            .add("b", "{cycle values=\"a,b\"}{cycle values=\"a,b\"}")
            .unwrap();
        assert_eq!(e.render("b", &json!({})).unwrap(), "ab");
    }

    #[test]
    fn html_select_escapes_and_marks_selected() {
        let e = engine()
            .add(
                "a",
                r#"{html_select name="city" options=$cities selected=$cur empty="请选择"}"#,
            )
            .unwrap();
        let out = e
            .render(
                "a",
                &json!({
                    "cities": [{"value": 1, "label": "北京"}, {"value": 2, "label": "<上海>"}],
                    "cur": 2
                }),
            )
            .unwrap();
        assert_eq!(
            out,
            "<select name=\"city\"><option value=\"\">请选择</option>\
             <option value=\"1\">北京</option>\
             <option value=\"2\" selected>&lt;上海&gt;</option></select>"
        );
        // 没传 selected 时，空选项默认选中
        let out = e.render("a", &json!({"cities": [1]})).unwrap();
        assert!(out.contains("<option value=\"\" selected>"));
    }

    #[test]
    fn new_modifiers() {
        let e = engine()
            .add(
                "a",
                r#"{$n|number_format:2}|{$t|date:"%Y-%m-%d"}|{$s|replace:"a":"b"}|{$s|substr:1:2}|{$s|capitalize}|{$i|pad:3}|{$s|repeat:2}|{$u|urlencode}|{$h|strip_tags}"#,
            )
            .unwrap();
        let out = e
            .render(
                "a",
                &json!({
                    "n": 1234567.891, "t": 1704067200, "s": "abc",
                    "i": 7, "u": "a b", "h": "<b>x</b>"
                }),
            )
            .unwrap();
        assert_eq!(
            out,
            "1,234,567.89|2024-01-01|bbc|bc|Abc|007|abcabc|a%20b|x"
        );
        // split 产出数组，可以直接喂给 foreach
        let e = engine()
            .add("b", "{foreach $s|split:\"-\" as $p}[{$p}]{/foreach}")
            .unwrap();
        assert_eq!(e.render("b", &json!({"s": "a-b-c"})).unwrap(), "[a][b][c]");
    }

    #[test]
    fn globals_are_overridable_by_data() {
        let e = Engine::new()
            .globals(&json!({"site": {"name": "rsboot"}}))
            .add("a", "{$site.name}")
            .unwrap();
        assert_eq!(e.render("a", &json!({})).unwrap(), "rsboot");
        assert_eq!(
            e.render("a", &json!({"site": {"name": "业务"}})).unwrap(),
            "业务"
        );
    }

    #[test]
    fn runtime_error_reports_template_and_line() {
        let e = engine().add("a", "line1\nline2\n{$x|nope}").unwrap();
        let err = e.render("a", &json!({"x": 1})).unwrap_err();
        assert_eq!(err.line, 3, "{err}");
        assert!(err.to_string().starts_with("a:3: "), "{err}");
    }

    #[test]
    fn render_into_appends_and_render_response_sets_html() {
        let e = engine().add("a", "<p>{$x}</p>").unwrap();
        let mut buf = String::from("<ul>");
        e.render_into("a", &json!({"x": 1}), &mut buf).unwrap();
        assert_eq!(buf, "<ul><p>1</p>");

        let resp = e.render_response("a", &json!({"x": 2})).unwrap();
        assert_eq!(resp.body, b"<p>2</p>".to_vec());
        assert_eq!(
            resp.headers.get("content-type").map(|v| v.to_string()),
            Some("text/html; charset=utf-8".to_string())
        );
    }

    #[test]
    fn error_into_response_is_a_500() {
        let e = engine().add("a", "{$x|nope}").unwrap();
        let err = e.render("a", &json!({"x": 1})).unwrap_err();
        let resp = rsboot::web::IntoResponse::into_response(err);
        assert_eq!(resp.status.0, 500);
    }

    // ---- v1.1：引用 / smarty / 缓存 / 热重载 ----

    #[test]
    fn relative_include_resolves_against_current_template() {
        let e = engine()
            .add("admin/row.tpl", "<{$v}>")
            .unwrap()
            .add("admin/list.tpl", "{include file=\"./row.tpl\" v=1}")
            .unwrap();
        assert_eq!(e.render("admin/list.tpl", &json!({})).unwrap(), "<1>");
    }

    #[test]
    fn include_can_walk_up_one_directory() {
        let e = engine()
            .add("shared/h.tpl", "[{$t}]")
            .unwrap()
            .add("admin/part/x.tpl", "{include file=\"../../shared/h.tpl\" t=\"上级\"}")
            .unwrap();
        assert_eq!(e.render("admin/part/x.tpl", &json!({})).unwrap(), "[上级]");
    }

    #[test]
    fn static_include_of_missing_template_fails_at_registration() {
        let err = engine()
            .add("a.tpl", "{include file=\"./nope.tpl\"}")
            .unwrap_err();
        assert!(err.msg.contains("未注册"), "{err}");
    }

    #[test]
    fn add_dir_supports_relative_include_and_same_template_twice() {
        let dir = tmpdir("reldir");
        write(&dir, "list.tpl", "{include file=\"./row.tpl\" n=1}{include file=\"./row.tpl\" n=2}");
        write(&dir, "row.tpl", "({$n})");
        let e = Engine::new().add_dir(&dir).unwrap();
        assert_eq!(e.render("list.tpl", &json!({})).unwrap(), "(1)(2)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn smarty_exposes_time_template_and_version() {
        let e = engine()
            .add("p", "{$smarty.tpl}|{$smarty.version}|{$smarty.now}")
            .unwrap();
        let out = e.render("p", &json!({})).unwrap();
        let expect = format!("p|{}|", env!("CARGO_PKG_VERSION"));
        assert!(out.starts_with(&expect), "{out}");
        // 后面那截是 unix 秒，至少得是 2024 年以后
        let ts: i64 = out.rsplit('|').next().unwrap().trim().parse().unwrap();
        assert!(ts > 1_700_000_000, "{ts}");
    }

    #[test]
    fn smarty_now_date_is_utc_and_formattable() {
        // ctx 覆盖 `now`：测试要固定时间，不能靠读时钟
        let e = engine()
            .add("p", "{$smarty.now_date}|{$smarty.now|date:\"%Y-%m-%d\"}")
            .unwrap();
        let out = e
            .render_with("p", &json!({}), &json!({"now": 1_704_067_200}))
            .unwrap();
        assert_eq!(out, "2024-01-01 00:00:00|2024-01-01");
    }

    #[test]
    fn request_context_decodes_query_and_params() {
        let mut req = rsboot::web::Request::new(rsboot::web::Method::Get, "/u/7")
            .with_query("q=%E4%B8%AD+文&page=2&q=dup&empty");
        req.params.push(("id".into(), "7".into()));

        assert_eq!(request_context(&req)["get"]["q"], json!("中 文"));
        assert_eq!(request_context(&req)["get"]["page"], json!("2"));
        // 同名只留第一个；没值的键是空串
        assert_eq!(request_context(&req)["get"]["empty"], json!(""));
        assert_eq!(request_context(&req)["params"]["id"], json!("7"));
        assert_eq!(request_context(&req)["path"], json!("/u/7"));
        assert_eq!(request_context(&req)["method"], json!("GET"));
    }

    #[test]
    fn render_request_puts_query_into_smarty_get() {
        let e = engine()
            .add("p", "{$smarty.path}?page={$smarty.get.page}")
            .unwrap();
        let req = rsboot::web::Request::new(rsboot::web::Method::Get, "/list")
            .with_query("page=3");
        assert_eq!(e.render_request("p", &json!({}), &req).unwrap(), "/list?page=3");
    }

    #[test]
    fn add_dir_writes_and_reuses_the_ast_cache() {
        let dir = tmpdir("cache");
        write(&dir, "a.tpl", "hi {$name}");
        let cache = dir.join(".tplc");
        let e = Engine::new().add_dir(&dir).unwrap();
        assert!(cache.exists(), "add_dir 默认写 {:?}", cache);
        assert_eq!(
            e.render("a.tpl", &json!({"name": "小明"})).unwrap(),
            "hi 小明"
        );
        // 缓存文件本身是合法 JSON，且记了 crate 版本
        let text = std::fs::read_to_string(&cache).unwrap();
        assert!(text.contains("\"format\":1"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_cache_is_ignored_not_fatal() {
        let dir = tmpdir("badcache");
        write(&dir, "a.tpl", "ok");
        std::fs::write(dir.join(".tplc"), "{ not json").unwrap();
        let e = Engine::new().add_dir(&dir).unwrap();
        assert_eq!(e.render("a.tpl", &json!({})).unwrap(), "ok");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_dir_none_disables_the_file() {
        let dir = tmpdir("nocache");
        write(&dir, "a.tpl", "x");
        let e = Engine::new()
            .add_dir(&dir)
            .unwrap()
            .cache_dir(None::<PathBuf>)
            .unwrap();
        assert_eq!(e.render("a.tpl", &json!({})).unwrap(), "x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hot_reload_picks_up_an_edit_without_restart() {
        let dir = tmpdir("hot");
        write(&dir, "a.tpl", "one");
        let e = Engine::new()
            .add_dir(&dir)
            .unwrap()
            .hot_reload_interval(Duration::from_millis(40));
        assert_eq!(e.render("a.tpl", &json!({})).unwrap(), "one");

        // 长度变化保证检测得到（不依赖 mtime 精度）
        write(&dir, "a.tpl", "two!!");
        std::thread::sleep(Duration::from_millis(90));
        assert_eq!(e.render("a.tpl", &json!({})).unwrap(), "two!!");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_syntax_error_while_hot_keeps_the_old_snapshot() {
        let dir = tmpdir("hotbad");
        write(&dir, "a.tpl", "good");
        let e = Engine::new()
            .add_dir(&dir)
            .unwrap()
            .hot_reload_interval(Duration::from_millis(40));
        assert_eq!(e.render("a.tpl", &json!({})).unwrap(), "good");

        write(&dir, "a.tpl", "{if $x}");
        std::thread::sleep(Duration::from_millis(90));
        // 旧快照继续服务，错误只留痕
        assert_eq!(e.render("a.tpl", &json!({})).unwrap(), "good");
        assert!(e.reload_error().unwrap_or_default().contains("a.tpl"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extends_keeps_working_from_a_subdirectory() {
        let dir = tmpdir("extsub");
        write(&dir, "layout.tpl", "<body>{block name=\"main\"}默认{/block}</body>");
        write(&dir, "sub/page.tpl", "{extends \"../layout.tpl\"}{block name=\"main\"}子{/block}");
        let e = Engine::new().add_dir(&dir).unwrap();
        assert_eq!(e.render("sub/page.tpl", &json!({})).unwrap(), "<body>子</body>");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
