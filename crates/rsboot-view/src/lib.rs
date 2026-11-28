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
//! | `{$name}` / `{$user.name}` / `{$list[0]}` | 取值，点与下标可混用 |
//! | `{if $a > 1}…{elseif $b}…{else}…{/if}` | 条件；`and`/`or`/`not` 与 `&&`/`\|\|`/`!` 等价 |
//! | `{foreach $list as $it}` / `as $k => $v` / `{foreachelse}` | 迭代数组或对象 |
//! | `{$x\|escape}` `\|upper` `\|lower` `\|trim` `\|default:"-"` `\|length` `\|truncate:20` `\|json` `\|join:", "` `\|nl2br` `\|raw` | 修饰符，可链式 |
//! | `{include "x.tpl"}` / `{include file="x.tpl"}` | 引入子模板（共享父作用域） |
//! | `{literal}…{/literal}` | 原样输出（里面的 `{` 不当标签） |
//! | `{* … *}` | 注释 |
//! | `{myfunc a=$x b="y"}` | 调注册函数（见 [`Engine::func`]） |
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
            auto_escape: true,
        }
    }

    /// 是否自动做 HTML 转义（默认开，见模块文档）。
    pub fn auto_escape(mut self, on: bool) -> Self {
        self.auto_escape = on;
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
        let mut rt = render::Rt::new(self.engine, self.name, data);
        rt.run(self.nodes)
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
}
