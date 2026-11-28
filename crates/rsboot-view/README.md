# rsboot-view

Smarty 风格的模板引擎，`rsboot` 的扩展 crate。架构照 minijinja（不建 token 流、
边读边解析、AST 编译一次渲染多次），语法照 Smarty，实现自研。

```rust
use std::sync::Arc;
use rsboot_view::Engine;

let view = Arc::new(Engine::new()
    .add("page.tpl", "<h1>{$title}</h1>
        {foreach $items as $it}<li>{$it.name|escape}</li>{/foreach}")?);

let html = view.render("page.tpl", &serde_json::json!({
    "title": "订单",
    "items": [{"name": "<script>alert(1)</script>"}],
}))?;
```

- `Engine::add(name, src)` / `Engine::add_dir("templates")`：启动期编译，运行期零 IO
- `Engine::func(name, |args| ...)`：注册自定义模板函数
- `Engine::auto_escape(true)`：默认**开**，`|raw` 才原样输出
- 修饰符：`escape` `raw` `upper` `lower` `trim` `nl2br` `default:x` `length`
  `truncate:n[:suffix]` `json` `join:sep`
- `include` 共享父作用域，32 层上限，环形 include 报错而不是爆栈

依赖：`rsboot` + `serde_json`（后者本就随 `rsboot` 一起引入）。
