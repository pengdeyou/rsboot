# rsboot-view

Smarty 风格的模板引擎，`rsboot` 的扩展 crate。架构照 minijinja（不建 token 流、
边读边解析、AST 编译一次渲染多次），语法照 Smarty，实现自研。

```rust
use std::sync::Arc;
use rsboot_view::Engine;

let view = Arc::new(Engine::new()
    .globals(&serde_json::json!({"site": {"name": "rsboot"}}))
    .add("page.tpl", "<h1>{$title}</h1>
        {foreach $items as $it}<li>{$it@index}. {$it.name|escape}</li>{/foreach}")?);

let html = view.render("page.tpl", &serde_json::json!({
    "title": "订单",
    "items": [{"name": "<script>alert(1)</script>"}],
}))?;
```

handler 里直接返回 HTML：

```rust
fn list(vm: Arc<Engine>) -> Result<Response, ApiError> {
    let data = serde_json::json!({ "rows": rows });
    Ok(vm.render_response("list", &data)?)   // 失败 → 500 + 5000，细节不落到浏览器
}
```

页面型 handler 想顺手拿到 query 参数，用 `render_request`：

```rust
fn detail(vm: Arc<Engine>, req: &Request) -> Result<Response, ApiError> {
    // 模板里可以直接写 {$smarty.get.page} / {$smarty.params.id}
    Ok(vm.render_request_response("detail", &data, req)?)
}
```

## API

- `Engine::add(name, src)` / `Engine::add_dir("templates")`：启动期编译，运行期零 IO
- `Engine::cache_dir(path)`：换 `.tplc` 的位置，传 `None` 表示不写（`add_dir` 默认 `<root>/.tplc`）
- `Engine::hot_reload(true)` / `hot_reload_interval(Duration)`：改模板不重启
- `Engine::reload()`：手动重建一次；`reload_error()`：最近一次热重载失败的原因
- `Engine::globals(&json)`：每个模板都能用的常量（业务数据同名覆盖）
- `Engine::func(name, |args| ...)`：注册自定义模板函数
- `Engine::auto_escape(true)`：默认**开**，`|raw` 才原样输出
- `Engine::render(name, &data)` / `render_with(name, &data, &ctx)` /
  `render_into(name, &data, &mut buf)` / `render_response(name, &data)` /
  `render_request(name, &data, request)`
- `Template::render` / `Template::render_with` / `Template::render_into`：拿到句柄后重复渲染
- `Error` 实现 `IntoResponse`，handler 直接 `?` 即可；`Error::to_api_error()` 可自行包装

## 语法

| 写法 | 含义 |
| --- | --- |
| `{$name}` `{$user.name}` `{$list[0]}` `{$obj[$k]}` | 取值，点与下标可混用；缺字段输出空串不报错 |
| `{if …}…{elseif …}…{else}…{/if}` | 条件；`and`/`or`/`not` 与 `&&`/`\|\|`/`!` 等价 |
| `{foreach $list as $it}` / `as $k => $v` / `{foreachelse}` | 迭代数组（键是下标）或对象（键是字段名） |
| `{$it@index}` `@iteration` `@first` `@last` `@total` `@key` | 循环属性，只在 `foreach` 体内有效 |
| `{for $i=1 to 5 step 2}…{forelse}…{/for}` | 数字区间；默认步长 1，倒着数写 `step -1` |
| `{capture name="x"}…{/capture}` / `append=true` | 把渲染片段存进变量（回显要 `\|raw`） |
| `{assign var="x" value=$y}` | 写当前作用域 |
| `{include file="row.tpl" item=$it}` / `{include "x.tpl"}` | 引入子模板；参数进子作用域，父作用域只读；32 层上限 |
| `{include file="./row.tpl"}` / `{include file="../shared/h.tpl"}` | 相对当前模板目录引用，解析后不许跑出模板根 |
| `{extends "layout"}` + `{block name="x"}` | 模板继承，编译期展开，子模板除 block 外不输出；`extends` 也支持 `./`、`../` |
| `{cycle values="odd,even"}` | 按出现次序轮转（斑马纹、左右分栏），`name=` 分组、`reset=true` 归零 |
| `{html_select name="c" options=$list selected=$cur empty="请选择"}` | 直接产出 `<select>`，内部强制转义 |
| `{literal}…{/literal}` / `{* … *}` | 原样输出 / 注释 |
| `{myfunc a=$x b="y"}` | 调注册函数 |

### 内建变量 `$smarty`

| 字段 | 内容 |
| --- | --- |
| `$smarty.now` | 渲染时刻的 unix 秒（UTC），给 `\|date` 用 |
| `$smarty.now_date` | 同上的 `%Y-%m-%d %H:%M:%S` 串 |
| `$smarty.tpl` | 当前模板名（`include` 时会变成子模板名） |
| `$smarty.version` | 本 crate 版本 |
| `$smarty.get` | query 参数（已百分号解码，`+` 当空格；同名取第一个） |
| `$smarty.params` | 路径参数（路由抓出来的） |
| `$smarty.path` / `$smarty.method` / `$smarty.request_id` | 请求行信息 |

`ctx` 里给了同名键会覆盖内建值，所以测试可以固定 `now`：

```rust
view.render_with("p", &data, &json!({"now": 1704067200}))?  // 2024-01-01 00:00:00
```

### 修饰符（可链式）

`|escape`（别名 `e`/`html`）`|raw` `|upper` `|lower` `|trim` `|capitalize`
`|nl2br` `|default:x` `|length`/`|count` `|truncate:n[:suffix]` `|substr:start[:len]`
`|replace:"a":"b"` `|split:","` `|join:sep` `|pad:n:"0"` `|repeat:n`
`|number_format:2[:"."][:","]` `|date:"%Y-%m-%d %H:%M:%S"` `|urlencode`
`|strip_tags` `|json`

`|date` 按 **UTC** 格式化（模板引擎不读进程时区）；要本地时间就在数据里传算好的字符串。
`|json` 会把 `< > & '` 转成 `\uXXXX`，可以直接塞进 `<script>`。

## 模板引用与继承

静态名字（字面量）在**编译期**解析、校验"存在 + 不成环"，错的模板启动就炸，不会等到
某个页面被访问：

```rust
Engine::new().add("a", "{include file=\"nope.tpl\"}")?;   // Err: 引用的模板 `nope.tpl` 未注册
Engine::new().add("a", "{include file=\"a\"}")?;          // Err: `{include}` 成环：a → a
```

运行期只有一次 `HashMap` 查找，**不内联**子模板 AST：`header.tpl` 被 50 个页面 include
也只占一份内存。动态名字（`{include file=$x}`）必须在运行期解析，靠 32 层深度兜住环。

`./`、`../` 按**当前模板所在目录**解析并归一化；`..` 冒到模板根之外直接报错（防止有人
拿用户输入拼模板名去读服务器任意文件）。

## 热重载与 `.tplc`

```rust
let view = Engine::new()
    .add_dir("templates")?            // 同时写 templates/.tplc
    .hot_reload_interval(Duration::from_millis(500));
```

- `.tplc` 存的是**解析结果（AST）**，不是可执行代码：省掉词法 + 语法，继承展开与引用
  校验照做。三级复用：`mtime+size` 相同 → 连文件都不读；读到了但内容指纹（FNV-1a）
  一致 → 跳过解析；否则重解析。写文件先写 `.tplc.tmp` 再 rename，进程被杀不会留下半个
  文件；格式/版本不符整体作废，读失败一律当无缓存。
- 热重载按间隔 `stat` 一遍目录，比对 `(mtime, size)` 表，有变化就增量重建一份 `Store`
  整体换掉。检查在渲染前做，不起后台线程；只有到点的那一个线程去扫目录。
- 重建失败（新模板语法写错）**保留旧快照继续服务**，错误记在 `reload_error()`——一个
  模板写错不该让线上整片 500。

## 设计取舍

- **默认转义**：Smarty 默认不转义（PHP 时代的选择），这里反过来。忘记 `|escape` 的
  代价是 XSS，忘记 `|raw` 的代价只是看到一个 `&lt;div&gt;`。
- **编译期展开继承**：`add` 时就把 `{extends}`/`{block}` 合成一棵树，渲染期既无锁也
  无"父模板还没到"的中间态；代价是父模板必须先注册（`add_dir` 会自动处理）。
- **快照 + `Arc`**：渲染开头克隆一次 `Arc<Store>`，之后全程不持锁；热重载换的是整个
  `Arc`，正在跑的那次渲染看到的是同一份模板集合，没有"读到一半被换掉"的中间态。
- **行号标记**：每个标签在 AST 里留一个 `At(line)`，运行期错误也能报 `模板:3:`。
- **作用域是栈**：`foreach`/`include` 各压一层，`assign` 只影响最内层，不漏回父层。
- **循环上限**：`for` 超过 100 万次直接报错，`include` 超过 32 层报环。
- **不读请求 body**：表单原始内容进模板是 XSS 与隐私双重风险，要回显就自己传 `data`。

## 依赖

`rsboot` + `serde`（derive，给 AST 序列化）+ `serde_json`。

`serde` 只为 `.tplc` 缓存存在：要把 AST 存盘就得能序列化。不想付这个依赖可以只用
`add` + `hot_reload`，不碰 `add_dir` 的缓存文件。

## 测试

```sh
cargo test -p rsboot-view     # 68 个单测：语法、继承、引用与成环、转义、修饰符、行号、
                             # $smarty、query 解码、.tplc 缓存、热重载、HTTP 接线
```

## 明确不做

JS 渲染、自定义定界符、`{strip}` 空白压缩、`{switch}`、模板里跑任意表达式（只认
`$var` / 字面量 / 四则与比较），以及"`{include}` 内联展开"（会让共享模板复制成 N 份）。
