//! 扩展名 → `content-type`。
//!
//! 只收录前端资源里真正常见的类型。查表是 `match`，编译成跳转表，
//! 比 `HashMap` 少一次哈希。未知扩展名一律 `application/octet-stream`
//! ——宁可让浏览器下载，也不要猜错类型让浏览器执行。

pub fn of(path: &str) -> &'static str {
    let ext = match path.rfind('.') {
        Some(i) => &path[i + 1..],
        None => return OCTET,
    };
    match_ignore_case(ext)
}

const OCTET: &str = "application/octet-stream";

fn match_ignore_case(ext: &str) -> &'static str {
    // 扩展名几乎总是小写；大写先降一次临时串更省事，但每条请求都分配
    // 不划算，所以这里逐字节比。
    macro_rules! eq {
        ($a:expr) => {
            ext.len() == $a.len() && ext.as_bytes().eq_ignore_ascii_case($a.as_bytes())
        };
    }
    if eq!("html") || eq!("htm") {
        "text/html; charset=utf-8"
    } else if eq!("css") {
        "text/css; charset=utf-8"
    } else if eq!("js") || eq!("mjs") {
        "text/javascript; charset=utf-8"
    } else if eq!("json") || eq!("map") {
        "application/json; charset=utf-8"
    } else if eq!("txt") || eq!("log") {
        "text/plain; charset=utf-8"
    } else if eq!("md") {
        "text/markdown; charset=utf-8"
    } else if eq!("csv") {
        "text/csv; charset=utf-8"
    } else if eq!("xml") {
        "application/xml; charset=utf-8"
    } else if eq!("svg") {
        "image/svg+xml"
    } else if eq!("png") {
        "image/png"
    } else if eq!("jpg") || eq!("jpeg") {
        "image/jpeg"
    } else if eq!("gif") {
        "image/gif"
    } else if eq!("webp") {
        "image/webp"
    } else if eq!("avif") {
        "image/avif"
    } else if eq!("ico") {
        "image/x-icon"
    } else if eq!("woff2") {
        "font/woff2"
    } else if eq!("woff") {
        "font/woff"
    } else if eq!("ttf") {
        "font/ttf"
    } else if eq!("otf") {
        "font/otf"
    } else if eq!("wasm") {
        "application/wasm"
    } else if eq!("pdf") {
        "application/pdf"
    } else if eq!("zip") {
        "application/zip"
    } else if eq!("gz") {
        "application/gzip"
    } else if eq!("mp4") {
        "video/mp4"
    } else if eq!("webm") {
        "video/webm"
    } else if eq!("mp3") {
        "audio/mpeg"
    } else if eq!("wav") {
        "audio/wav"
    } else {
        OCTET
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knows_the_common_web_types() {
        assert_eq!(of("/a/b/index.html"), "text/html; charset=utf-8");
        assert_eq!(of("app.js"), "text/javascript; charset=utf-8");
        assert_eq!(of("logo.WOFF2"), "font/woff2");
        assert_eq!(of("data.unknown"), OCTET);
        assert_eq!(of("noext"), OCTET);
        assert_eq!(of("weird."), OCTET);
    }
}
