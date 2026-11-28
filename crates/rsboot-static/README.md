# rsboot-static

静态文件服务，`rsboot` 的扩展 crate，零额外依赖。

```rust
use rsboot_static::Static;

let assets = Static::dir("public").max_age(3600).immutable();
let r = Router::new().get("/assets/*path", {
    let s = assets.clone();          // clone 只加一次引用计数
    move |req: &mut Request, _| s.serve_or_404(req)
})?;
```

- ETag（`W/"<len>-<mtime>"`）与 `Last-Modified`，`If-None-Match` 优先 → 304
- 单区间 `Range` → 206 + `Content-Range`；越界 → 416；多区间忽略回 200
- 穿越防护三道：逐段 `percent_decode` 后拒 `.`/`..`/分隔符/NUL →
  `canonicalize` 后必须 `starts_with(root)` → 只服务普通文件
- `fallback()` 返回 `'static` 闭包，直接给 `Router::fallback`
