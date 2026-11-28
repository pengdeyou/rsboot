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
//! | `{include file="x.tpl" item=$it}` | 引入子模板；参数进子作用域，父作用域只读 |
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
//! ## 错误定位
//!
//! 语法错误在 `add`/`add_dir` 时就报（带行号）；运行期错误（除零、缺模板、
//! 修饰符参数不对）也会带 `模板:行`——每个标签在 AST 里留了一个行号标记，
//! 执行到就更新，出错直接取当前值。
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

pub use parse::SyntaxError;
pub use render::escape_html;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parse::{Expr, Stmt};
use serde_json::Value;

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
/// 构造完就是只读的：所有模板在 `add` / `add_dir` 时编译一遍，
/// 渲染期只查表，不需要锁。放进 `Arc` 就能跨 worker 共享。
pub struct Engine {
    /// 原始 AST：`add` 进来的样子，含 `{extends}` 与顶层 `{block}` 定义。
    raw: HashMap<String, Arc<Src>>,
    /// 展开后的 AST：`{extends}` 已换成父模板节点、`{block}` 已套上子模板的覆盖。
    /// 渲染只看这一份，所以运行期既没有锁，也没有"父模板还没到"的中间态。
    templates: HashMap<String, Arc<Vec<Stmt>>>,
    funcs: HashMap<String, Func>,
    /// 每次渲染都会先铺进根作用域的常量（站点名、版本号之类）。
    globals: HashMap<String, Value>,
    auto_escape: bool,
}

/// 一份模板拆出来的三件事。
struct Src {
    /// 没有 `{extends}` 时就是完整节点列表；有的话这份内容会被丢弃，
    /// 只剩 `blocks` 起作用（Smarty 的语义：子模板除 block 外不输出）。
    nodes: Vec<Stmt>,
    extends: Option<String>,
    blocks: HashMap<String, Vec<Stmt>>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("templates", &self.templates.len())
            .field("funcs", &self.funcs.len())
            .field("globals", &self.globals.len())
            .field("auto_escape", &self.auto_escape)
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
            raw: HashMap::new(),
            templates: HashMap::new(),
            funcs: HashMap::new(),
            globals: HashMap::new(),
            auto_escape: true,
        }
    }

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
    /// 立刻展开一次，所以 `{extends}` 指向的父模板必须**已经**注册。
    /// 顺序不好控制时（比如自定义加载顺序）用 [`Engine::add_dir`]，
    /// 它先把目录里的模板全登记完，最后统一展开一次。
    pub fn add(mut self, name: impl Into<String>, src: &str) -> Result<Self, Error> {
        let name = name.into();
        let nodes = parse::parse(src).map_err(|e| Error {
            template: name.clone(),
            line: e.line,
            msg: e.msg,
        })?;
        self.raw.insert(name.clone(), Arc::new(split_src(&name, nodes)?));
        self.rebuild()?;
        Ok(self)
    }

    /// 递归加载目录下的 `*.tpl` / `*.html`，模板名用相对路径（`/` 分隔）。
    ///
    /// 启动期一次编译完，运行期零 IO——模板热更新不在这个版本的目标里，
    /// 重启换模板反而避免了"一半进程用旧模板"的中间态。
    pub fn add_dir(mut self, root: impl AsRef<Path>) -> Result<Self, Error> {
        let root = root.as_ref();
        let mut files = Vec::new();
        collect(root, root, &mut files).map_err(|e| Error {
            template: root.display().to_string(),
            line: 0,
            msg: format!("读取目录失败：{e}"),
        })?;
        // 先全部登记，最后只展开一次：文件系统的遍历顺序不可控，
        // 逐个 `add` 会因为"父模板还没读到"而误报。
        for (name, path) in files {
            let src = std::fs::read_to_string(&path).map_err(|e| Error {
                template: name.clone(),
                line: 0,
                msg: format!("读取失败：{e}"),
            })?;
            let nodes = parse::parse(&src).map_err(|e| Error {
                template: name.clone(),
                line: e.line,
                msg: e.msg,
            })?;
            self.raw.insert(name.clone(), Arc::new(split_src(&name, nodes)?));
        }
        self.rebuild()?;
        Ok(self)
    }

    /// 注册一个模板函数，模板里写 `{名字 参数="值"}`。
    pub fn func<F>(mut self, name: impl Into<String>, f: F) -> Self
    where
        F: Fn(&[(&str, Value)]) -> Result<Value, String> + Send + Sync + 'static,
    {
        self.funcs.insert(name.into(), Box::new(f));
        self
    }

    pub fn template(&self, name: &str) -> Option<Template<'_>> {
        // `get_key_value` 拿到的键生命周期挂在 `self` 上，模板名因此
        // 不必克隆一份——`Template` 只借，不持。
        let (key, nodes) = self.templates.get_key_value(name)?;
        Some(Template {
            engine: self,
            name: key.as_str(),
            nodes,
        })
    }

    /// 一步渲染。
    pub fn render(&self, name: &str, data: &Value) -> Result<String, Error> {
        match self.template(name) {
            Some(t) => t.render(data),
            None => Err(Error {
                template: name.to_string(),
                line: 0,
                msg: "找不到模板".to_string(),
            }),
        }
    }

    /// 渲染到已有缓冲区：高频路径（列表页、SSE 片段）能省一次 `String` 分配。
    pub fn render_into(&self, name: &str, data: &Value, out: &mut String) -> Result<(), Error> {
        match self.template(name) {
            Some(t) => t.render_into(data, out),
            None => Err(Error {
                template: name.to_string(),
                line: 0,
                msg: "找不到模板".to_string(),
            }),
        }
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

    /// 渲染层要拿这层常量，不能用同一个名字的 builder 方法，故另起名。
    pub(crate) fn global_values(&self) -> &HashMap<String, Value> {
        &self.globals
    }

    pub(crate) fn nodes(&self, name: &str) -> Option<&[Stmt]> {
        self.templates.get(name).map(|n| n.as_slice())
    }

    pub(crate) fn lookup_func(&self, name: &str) -> Option<&Func> {
        self.funcs.get(name)
    }

    pub(crate) fn is_auto_escape(&self) -> bool {
        self.auto_escape
    }

    /// 把所有模板重新展开一遍。
    ///
    /// 每 `add` 一次全量重建，看起来浪费，实际是几十个模板、毫秒级的事，
    /// 换来的是"`add` 顺序无关"和"渲染期零 IO 零锁"。真要几百个模板，
    /// 改成增量展开也只影响这一处。
    fn rebuild(&mut self) -> Result<(), Error> {
        let names: Vec<String> = self.raw.keys().cloned().collect();
        let mut out = HashMap::with_capacity(names.len());
        for name in names {
            let mut stack = Vec::new();
            let nodes = compose(&self.raw, &name, &mut stack)?;
            out.insert(name, Arc::new(nodes));
        }
        self.templates = out;
        Ok(())
    }
}

/// 从解析结果里拆出 `{extends}` 与顶层 `{block}`。
fn split_src(name: &str, nodes: Vec<Stmt>) -> Result<Src, Error> {
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

/// 递归展开一个模板。
fn compose(
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
    if !raw.contains_key(parent) {
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
    let r = compose(raw, parent, stack);
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

/// 一份模板的渲染入口。
pub struct Template<'a> {
    engine: &'a Engine,
    name: &'a str,
    nodes: &'a Arc<Vec<Stmt>>,
}

impl Template<'_> {
    pub fn name(&self) -> &str {
        self.name
    }

    pub fn render(&self, data: &Value) -> Result<String, Error> {
        let mut out = String::with_capacity(1024);
        self.render_into(data, &mut out)?;
        Ok(out)
    }

    /// 渲染追加到已有缓冲区（模板片段拼接、SSE 分片输出都用它）。
    pub fn render_into(&self, data: &Value, out: &mut String) -> Result<(), Error> {
        let mut rt = render::Rt::new(self.engine, self.name, data);
        rt.run_into(self.nodes, out)
    }
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, out)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn engine() -> Engine {
        Engine::new()
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
    fn include_cycle_is_a_clear_error() {
        let e = engine().add("a", "{include \"a\"}").unwrap();
        let err = e.render("a", &json!({})).unwrap_err();
        assert!(err.msg.contains("嵌套太深"), "{err}");
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
        let dir = std::env::temp_dir().join(format!("rsboot-view-cycle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.tpl"), "{extends \"b.tpl\"}").unwrap();
        std::fs::write(dir.join("b.tpl"), "{extends \"a.tpl\"}").unwrap();
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
}
