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

## API

- `Engine::add(name, src)` / `Engine::add_dir("templates")`：启动期编译，运行期零 IO
- `Engine::globals(&json)`：每个模板都能用的常量（业务数据同名覆盖）
- `Engine::func(name, |args| ...)`：注册自定义模板函数
- `Engine::auto_escape(true)`：默认**开**，`|raw` 才原样输出
- `Engine::render(name, &data)` / `render_into(name, &data, &mut buf)` / `render_response(name, &data)`
- `Template::render` / `Template::render_into`：拿到模板句柄后重复渲染
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
| `{extends "layout"}` + `{block name="x"}` | 模板继承，编译期展开，子模板除 block 外不输出 |
| `{cycle values="odd,even"}` | 按出现次序轮转（斑马纹、左右分栏），`name=` 分组、`reset=true` 归零 |
| `{html_select name="c" options=$list selected=$cur empty="请选择"}` | 直接产出 `<select>`，内部强制转义 |
| `{literal}…{/literal}` / `{* … *}` | 原样输出 / 注释 |
| `{myfunc a=$x b="y"}` | 调注册函数 |

### 修饰符（可链式）

`|escape`（别名 `e`/`html`）`|raw` `|upper` `|lower` `|trim` `|capitalize`
`|nl2br` `|default:x` `|length`/`|count` `|truncate:n[:suffix]` `|substr:start[:len]`
`|replace:"a":"b"` `|split:","` `|join:sep` `|pad:n:"0"` `|repeat:n`
`|number_format:2[:"."][:","]` `|date:"%Y-%m-%d %H:%M:%S"` `|urlencode`
`|strip_tags` `|json`

`|date` 按 **UTC** 格式化（模板引擎不读进程时区）；要本地时间就在数据里传算好的字符串。
`|json` 会把 `< > & '` 转成 `\uXXXX`，可以直接塞进 `<script>`。

## 设计取舍

- **默认转义**：Smarty 默认不转义（PHP 时代的选择），这里反过来。忘记 `|escape` 的
  代价是 XSS，忘记 `|raw` 的代价只是看到一个 `&lt;div&gt;`。
- **编译期展开继承**：`add` 时就把 `{extends}`/`{block}` 合成一棵树，渲染期既无锁也
  无"父模板还没到"的中间态；代价是父模板必须先注册（`add_dir` 会自动处理）。
- **行号标记**：每个标签在 AST 里留一个 `At(line)`，运行期错误也能报 `模板:3:`。
- **作用域是栈**：`foreach`/`include` 各压一层，`assign` 只影响最内层，不漏回父层。
- **循环上限**：`for` 超过 100 万次直接报错，`include` 超过 32 层报环。

## 依赖

`rsboot` + `serde_json`（后者本就随 `rsboot` 一起引入）。

## 测试

```sh
cargo test -p rsboot-view     # 50 个单测：语法、继承、转义、修饰符、行号、HTTP 接线
```

## 明确不做

模板热重载（重启换模板）、`.tplc` 编译缓存落盘、`$smarty.get` 之类的请求上下文
（`Engine::globals` 够用）、自定义定界符、`{strip}` 空白压缩、`{switch}`。
