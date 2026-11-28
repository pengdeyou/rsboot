//! 静态文件服务：ETag / `Last-Modified` / `304` / 单区间 `Range`。
//!
//! 挂法是 fallback，这样"路径没被 API 接住"时才轮到它：
//!
//! ```ignore
//! let assets = Static::dir("public").max_age(3600);
//! let r = Router::new().get("/api/ping", |_, _| Res::success("ok"))?;
//! // 生产里通常再包一层 App::router(...).fallback(assets.fallback())
//! ```
//!
//! ## 为什么不是"内置到核心"
//!
//! 读磁盘、猜 MIME、算 ETag 都是**可选的**能力：只写 JSON API 的服务
//! 一行都用不上。放在独立 crate 里，核心继续零依赖。
//!
//! ## 安全边界
//!
//! 三道关卡，缺一不可：
//! 1. 逐段解码后拒绝 `.` / `..` / 含分隔符的段（`%2e%2e` 解码后是 `..`）；
//! 2. 目标路径 `canonicalize` 后必须以根目录为前缀——防软链接逃逸；
//! 3. 只服务**普通文件**，目录一律走 `index`。
//!
//! ## 大文件
//!
//! 非 `Range` 请求会整块读进内存（前端资源都在这个量级）。要发几百 MB 的
//! 视频就让客户端带 `Range`（`<video>` 会自动带），那条路径只读区间。

mod date;
mod mime;

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use rsboot::web::http::Status;
use rsboot::web::path::percent_decode;
use rsboot::web::response::Response;
use rsboot::{Context, Method, Request};

/// 静态目录。
#[derive(Clone)]
pub struct Static {
    root: PathBuf,
    /// 根目录的规范化结果，第一次用到时算一次；目录还没建时是 `None`。
    canon: Arc<OnceLock<Option<PathBuf>>>,
    max_age: u64,
    immutable: bool,
    index: String,
}

impl Static {
    pub fn dir(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
            canon: Arc::new(OnceLock::new()),
            max_age: 0,
            immutable: false,
            index: "index.html".to_string(),
        }
    }

    /// `cache-control: max-age=N`。`0` 表示 `no-cache`（每次都协商，靠 ETag 换 304）。
    pub fn max_age(mut self, secs: u64) -> Self {
        self.max_age = secs;
        self
    }

    /// 给 `max-age` 追加 `immutable`：适合内容带哈希的文件名（`app.a1b2c3.js`）。
    pub fn immutable(mut self) -> Self {
        self.immutable = true;
        self
    }

    pub fn index(mut self, name: impl Into<String>) -> Self {
        self.index = name.into();
        self
    }

    /// 作为 `Router::fallback` 的 handler。闭包可 `'static` 是因为 `Static` 只持 `Arc`。
    pub fn fallback(self) -> impl Fn(&mut Request, &Context) -> Response + Send + Sync + 'static {
        move |req, _| self.serve_or_404(req)
    }

    /// 命中就回文件，没命中回 404。`Router::fallback` 里直接用这个。
    pub fn serve_or_404(&self, req: &Request) -> Response {
        self.serve(req)
            .unwrap_or_else(|| Response::text("not found").with_status(Status(404)))
    }

    /// `None` = 这个请求不该由静态服务处理（方法不是 GET/HEAD）。
    pub fn serve(&self, req: &Request) -> Option<Response> {
        if req.method != Method::Get && req.method != Method::Head {
            return None;
        }
        // 不安全路径明确回 404，而不是"不归我管"——后者会让 fallback 落到
        // 上一层语义里去，等于把拒绝权交出去。
        let segs = match sanitize(req.path) {
            Some(s) => s,
            None => return Some(not_found()),
        };
        let root = match self.canon.get_or_init(|| self.root.canonicalize().ok()) {
            Some(r) => r,
            None => return Some(not_found()),
        };

        let mut full = root.clone();
        for s in &segs {
            full.push(s);
        }
        if full.is_dir() {
            full.push(&self.index);
        }
        // 软链接、`..`、符号大小写都可能让路径跑到根目录外，只有规范化后的
        // 路径才可信。
        let Ok(target) = full.canonicalize() else {
            return Some(not_found());
        };
        if !target.starts_with(root.as_path()) {
            return Some(not_found());
        }
        let Ok(meta) = std::fs::metadata(&target) else {
            return Some(not_found());
        };
        if !meta.is_file() {
            return Some(not_found());
        }
        Some(self.build(&target, &meta, req))
    }

    fn build(&self, path: &Path, meta: &std::fs::Metadata, req: &Request) -> Response {
        let len = meta.len();
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let etag = format!("W/\"{len}-{mtime}\"");

        let mut resp = Response::new(Status::OK);
        resp.headers.insert("etag", &etag);
        resp.headers.insert("last-modified", &date::format(mtime));
        resp.headers.insert("accept-ranges", "bytes");
        if self.max_age == 0 {
            resp.headers.insert("cache-control", "no-cache");
        } else if self.immutable {
            resp.headers
                .insert("cache-control", &format!("public, max-age={}, immutable", self.max_age));
        } else {
            resp.headers
                .insert("cache-control", &format!("public, max-age={}", self.max_age));
        }

        if is_not_modified(req, &etag, mtime) {
            resp.status = Status(304);
            return resp;
        }

        match req.header("range").and_then(|h| parse_range(h, len)) {
            Some(Spec::Bad) => {
                resp.status = Status(416);
                resp.headers
                    .insert("content-range", &format!("bytes */{len}"));
                resp.body = b"range not satisfiable".to_vec();
                resp.headers.insert("content-type", "text/plain; charset=utf-8");
                return resp;
            }
            Some(Spec::Single(start, end)) => {
                let want = end - start + 1;
                match read_range(path, start, want) {
                    Some(bytes) => {
                        resp.status = Status(206);
                        resp.headers
                            .insert("content-range", &format!("bytes {start}-{end}/{len}"));
                        resp.headers.insert("content-type", mime::of(path.to_str().unwrap_or("")));
                        resp.body = bytes;
                        resp
                    }
                    None => not_found(),
                }
            }
            None => match std::fs::read(path) {
                Ok(bytes) => {
                    resp.headers.insert("content-type", mime::of(path.to_str().unwrap_or("")));
                    resp.body = bytes;
                    resp
                }
                Err(_) => not_found(),
            },
        }
    }
}

fn not_found() -> Response {
    Response::text("not found").with_status(Status(404))
}

/// 逐段解码 + 拒绝危险段。返回 `None` 表示这条路径不该被服务。
fn sanitize(path: &str) -> Option<Vec<String>> {
    let mut segs = Vec::new();
    for raw in path.trim_start_matches('/').split('/') {
        if raw.is_empty() {
            continue;
        }
        let seg = percent_decode(raw);
        if seg.is_empty() || seg == "." || seg == ".." {
            return None;
        }
        // 解码后冒出分隔符（`%2f` / `%5c`）或 NUL，一律拒绝：
        // 否则 `/a%2f..%2fb` 会在 join 时变成两级路径。
        if seg.contains('/') || seg.contains('\\') || seg.contains('\0') {
            return None;
        }
        segs.push(seg);
    }
    Some(segs)
}

/// 去掉弱校验前缀与引号，得到可比较的不透明串。
fn opaque(tag: &str) -> &str {
    tag.trim().trim_start_matches("W/").trim_matches('"')
}

fn is_not_modified(req: &Request, etag: &str, mtime: u64) -> bool {
    // `If-None-Match` 优先级高于 `If-Modified-Since`（RFC 7232 §3.3）：
    // 两个都在时，弱校验值不一致就必须回 200，哪怕时间看着没变。
    if let Some(inm) = req.header("if-none-match") {
        let inm = inm.trim();
        if inm == "*" {
            return true;
        }
        return inm.split(',').any(|t| opaque(t) == opaque(etag));
    }
    if let Some(ims) = req.header("if-modified-since") {
        if let Some(t) = date::parse(ims) {
            return mtime <= t;
        }
    }
    false
}

enum Spec {
    Single(u64, u64),
    Bad,
}

/// 只认单区间。多区间（`bytes=0-1,5-6`）回全量 200——比拼 multipart/byteranges
/// 简单得多，而且浏览器与播放器都只发单区间。
fn parse_range(h: &str, len: u64) -> Option<Spec> {
    let v = h.trim().strip_prefix("bytes=")?;
    if v.contains(',') {
        return None;
    }
    let (a, b) = v.split_once('-')?;
    if a.trim().is_empty() {
        let n: u64 = b.trim().parse().ok()?;
        if n == 0 || len == 0 {
            return Some(Spec::Bad);
        }
        let n = n.min(len);
        return Some(Spec::Single(len - n, len - 1));
    }
    let start: u64 = a.trim().parse().ok()?;
    if len == 0 || start >= len {
        return Some(Spec::Bad);
    }
    let end = if b.trim().is_empty() {
        len - 1
    } else {
        b.trim().parse::<u64>().ok()?.min(len - 1)
    };
    if end < start {
        return Some(Spec::Bad);
    }
    Some(Spec::Single(start, end))
}

fn read_range(path: &Path, start: u64, len: u64) -> Option<Vec<u8>> {
    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = vec![0u8; len as usize];
    f.read_exact(&mut buf).ok()?;
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            // pid + 进程内单调计数器：同一进程的多条测试线程各拿一个独立目录。
            // 原先用 `pid + 当前纳秒`，macOS 上时钟粒度粗到会撞——两个测试算出
            // 同一个目录，其中一个 Drop 时把另一个的目录删了，于是偶发 404。
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!("rsboot-static-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p); // 上次崩溃可能留了残骸
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn write(&self, name: &str, body: &[u8]) {
            std::fs::write(self.0.join(name), body).unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn get(path: &'static str) -> Request<'static> {
        Request::new(Method::Get, path)
    }

    #[test]
    fn serves_a_file_with_etag_and_mime() {
        let d = TempDir::new();
        d.write("index.html", b"<h1>hi</h1>");
        let st = Static::dir(&d.0);
        let r = st.serve(&get("/index.html")).unwrap();
        assert_eq!(r.status, Status::OK);
        assert_eq!(r.body, b"<h1>hi</h1>");
        assert_eq!(r.headers.get("content-type"), Some("text/html; charset=utf-8"));
        let etag = r.headers.get("etag").unwrap().to_string();
        assert!(etag.starts_with("W/\""), "{etag}");
        assert!(r.headers.get("last-modified").is_some());
    }

    #[test]
    fn if_none_match_is_304_without_body() {
        let d = TempDir::new();
        d.write("a.txt", b"hello");
        let st = Static::dir(&d.0);
        let first = st.serve(&get("/a.txt")).unwrap();
        let etag = first.headers.get("etag").unwrap().to_string();
        let mut req = get("/a.txt");
        req.headers.insert("if-none-match", &etag);
        let r = st.serve(&req).unwrap();
        assert_eq!(r.status, Status(304));
        assert!(r.body.is_empty(), "304 不能带 body");
        assert_eq!(r.headers.get("etag").unwrap(), etag);

        // 弱校验：带不带 `W/` 都算同一个
        let mut req = get("/a.txt");
        req.headers.insert("if-none-match", opaque(&etag));
        assert_eq!(st.serve(&req).unwrap().status, Status(304));
    }

    #[test]
    fn range_is_206_with_content_range() {
        let d = TempDir::new();
        d.write("big.txt", b"0123456789");
        let st = Static::dir(&d.0);
        let mut req = get("/big.txt");
        req.headers.insert("range", "bytes=2-5");
        let r = st.serve(&req).unwrap();
        assert_eq!(r.status, Status(206));
        assert_eq!(r.body, b"2345");
        assert_eq!(r.headers.get("content-range"), Some("bytes 2-5/10"));

        // 后缀区间：最后 3 个字节
        let mut req = get("/big.txt");
        req.headers.insert("range", "bytes=-3");
        let r = st.serve(&req).unwrap();
        assert_eq!(r.body, b"789");

        // 越界 → 416
        let mut req = get("/big.txt");
        req.headers.insert("range", "bytes=100-200");
        let r = st.serve(&req).unwrap();
        assert_eq!(r.status, Status(416));
        assert_eq!(r.headers.get("content-range"), Some("bytes */10"));
    }

    #[test]
    fn traversal_and_encoded_dots_are_404() {
        let d = TempDir::new();
        d.write("ok.txt", b"ok");
        let st = Static::dir(&d.0);
        for p in ["/../etc/passwd", "/%2e%2e/etc/passwd", "/a%2f..%2fb", "/..%2f..%2fetc"] {
            let r = st.serve(&get(p)).unwrap();
            assert_eq!(r.status, Status(404), "{p}");
        }
    }

    #[test]
    fn directory_falls_back_to_index_and_missing_is_404() {
        let d = TempDir::new();
        d.write("index.html", b"root");
        let st = Static::dir(&d.0);
        assert_eq!(st.serve(&get("/")).unwrap().body, b"root");
        assert_eq!(st.serve(&get("/nope")).unwrap().status, Status(404));
    }

    #[test]
    fn non_get_is_not_handled_here() {
        let d = TempDir::new();
        d.write("a.txt", b"x");
        let st = Static::dir(&d.0);
        let req = Request::new(Method::Post, "/a.txt");
        assert!(st.serve(&req).is_none(), "非 GET/HEAD 应该交给路由层");
    }

    #[test]
    fn fallback_wraps_misses_as_404() {
        let d = TempDir::new();
        d.write("a.txt", b"x");
        let st = Static::dir(&d.0);
        let mut req = get("/missing");
        let r = st.fallback()(&mut req, &test_ctx());
        assert_eq!(r.status, Status(404));
    }

    fn test_ctx() -> Context {
        Context::new(
            rsboot::Config::new(),
            Vec::new(),
            None,
            rsboot::config::Environment::Dev,
        )
    }
}
