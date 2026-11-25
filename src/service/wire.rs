//! 下游调用的线格式：请求序列化 + 响应解析。
//!
//! 服务端那套（`server/parse.rs` / `server/write.rs`）不能反过来用 ——
//! 请求和响应虽然长得像，但**边界判定规则完全不同**：
//! - 服务端读请求：body 长度必须由 `content-length` 明示，没有就当成 0（绝不猜）；
//! - 客户端读响应：body 可能是 `chunked`（别的框架默认就这么回），也可能是
//!   "读到 EOF 为止"（`connection: close` 无长度头）。
//!
//! 把客户端解析塞进服务端解析函数里，两边的安全假设会互相污染。所以单开一个文件。

use std::io::{BufRead, Read};

/// 响应头总量上限。8 KiB 是常见代理（nginx 默认 4~8k）的量级。
const MAX_HEAD_BYTES: usize = 8 * 1024;
/// 单行上限，防止一条超长头把 `read_line` 变成无界读。
const MAX_LINE_BYTES: usize = 4 * 1024;

#[derive(Debug)]
pub enum WireError {
    Io(std::io::Error),
    /// 状态行不是 `HTTP/1.x NNN`
    BadStatusLine(String),
    /// 头部格式错误或超限
    BadHeader(String),
    /// 响应体超过上限
    BodyTooLarge(usize),
    /// 对端在响应没写完时关闭
    Truncated,
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::Io(e) => write!(f, "读写出错：{e}"),
            WireError::BadStatusLine(s) => write!(f, "状态行不合法：{s:?}"),
            WireError::BadHeader(s) => write!(f, "响应头不合法：{s}"),
            WireError::BodyTooLarge(n) => write!(f, "响应体超过上限 {n} 字节"),
            WireError::Truncated => write!(f, "对端在响应中途关闭"),
        }
    }
}

impl From<std::io::Error> for WireError {
    fn from(e: std::io::Error) -> Self {
        WireError::Io(e)
    }
}

pub struct WireRequest {
    pub method: &'static str,
    /// 源站路径（含查询串）。代理模式下这里会是绝对 URI，暂不支持。
    pub target: String,
    pub host: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl WireRequest {
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + self.body.len());
        out.extend_from_slice(self.method.as_bytes());
        out.push(b' ');
        out.extend_from_slice(self.target.as_bytes());
        out.extend_from_slice(b" HTTP/1.1\r\n");

        // `host` 必须有：HTTP/1.1 强制要求，缺了很多网关直接 400
        out.extend_from_slice(b"host: ");
        out.extend_from_slice(self.host.as_bytes());
        out.extend_from_slice(b"\r\n");

        for (k, v) in &self.headers {
            // 头里出现 CR/LF 就是请求走私的原料，直接跳过而不是写进去
            if k.contains(['\r', '\n']) || v.contains(['\r', '\n']) {
                continue;
            }
            out.extend_from_slice(k.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(v.as_bytes());
            out.extend_from_slice(b"\r\n");
        }

        if !self.body.is_empty() {
            out.extend_from_slice(b"content-length: ");
            out.extend_from_slice(self.body.len().to_string().as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"connection: keep-alive\r\n\r\n");
        out.extend_from_slice(&self.body);
        out
    }
}

#[derive(Debug)]
pub struct WireResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl WireResponse {
    /// 头名大小写不敏感（RFC 7230 §3.2）。
    pub fn header(&self, name: &str) -> Option<&str> {
        let want = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k.to_ascii_lowercase() == want)
            .map(|(_, v)| v.as_str())
    }
}

fn read_line<R: BufRead>(r: &mut R) -> Result<String, WireError> {
    let mut line = Vec::new();
    // 用 take 限制单行长度：read_until 本身不设上限
    let n = (&mut *r)
        .take(MAX_LINE_BYTES as u64 + 1)
        .read_until(b'\n', &mut line)?;
    if n == 0 {
        return Err(WireError::Truncated);
    }
    if line.len() > MAX_LINE_BYTES {
        return Err(WireError::BadHeader(format!(
            "单行超过 {MAX_LINE_BYTES} 字节"
        )));
    }
    if !line.ends_with(b"\n") {
        return Err(WireError::Truncated);
    }
    line.pop();
    if line.ends_with(b"\r") {
        line.pop();
    }
    String::from_utf8(line)
        .map_err(|_| WireError::BadHeader("头部出现非法字节（非 UTF-8）".into()))
}

/// 读一条完整响应（状态行 + 头 + 按规则定界的 body）。
///
/// `max_body` 是这道闸门的意义所在：下游返回一个 500MB 的 HTML 错误页时，
/// 不能把它整个读进内存再发现"不是 JSON"。
pub fn read_response<R: BufRead>(r: &mut R, max_body: usize) -> Result<WireResponse, WireError> {
    let status_line = read_line(r)?;
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    let status = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/1.") || status.len() != 3 {
        return Err(WireError::BadStatusLine(status_line));
    }
    let status: u16 = status
        .parse()
        .map_err(|_| WireError::BadStatusLine(status_line.clone()))?;
    let reason = parts.next().unwrap_or("").to_string();

    let mut headers = Vec::new();
    let mut total = status_line.len();
    loop {
        let line = read_line(r)?;
        if line.is_empty() {
            break;
        }
        total += line.len();
        if total > MAX_HEAD_BYTES {
            return Err(WireError::BadHeader(format!(
                "响应头超过 {MAX_HEAD_BYTES} 字节"
            )));
        }
        let (k, v) = line
            .split_once(':')
            .ok_or_else(|| WireError::BadHeader(format!("缺少冒号：{line:?}")))?;
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }

    let resp = WireResponse {
        status,
        headers,
        body: Vec::new(),
    };
    let mut resp = resp;
    resp.body = read_body(r, &resp, max_body)?;
    let _ = reason;
    Ok(resp)
}

fn read_body<R: BufRead>(
    r: &mut R,
    resp: &WireResponse,
    max_body: usize,
) -> Result<Vec<u8>, WireError> {
    // 204/304 一律无 body，即使带了 content-length 也不读（否则会卡住等下一条响应）
    if resp.status == 204 || resp.status == 304 || (100..200).contains(&resp.status) {
        return Ok(Vec::new());
    }

    if let Some(te) = resp.header("transfer-encoding") {
        if te.to_ascii_lowercase().contains("chunked") {
            return read_chunked(r, max_body);
        }
    }

    if let Some(cl) = resp.header("content-length") {
        let n: usize = cl
            .trim()
            .parse()
            .map_err(|_| WireError::BadHeader(format!("content-length 不是数字：{cl:?}")))?;
        if n > max_body {
            return Err(WireError::BodyTooLarge(n));
        }
        let mut buf = vec![0u8; n];
        r.read_exact(&mut buf)?;
        return Ok(buf);
    }

    // 既没长度也没 chunked：只能读到 EOF。这条连接之后不可复用。
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = r.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > max_body {
            return Err(WireError::BodyTooLarge(buf.len()));
        }
    }
    Ok(buf)
}

fn read_chunked<R: BufRead>(r: &mut R, max_body: usize) -> Result<Vec<u8>, WireError> {
    let mut out = Vec::new();
    loop {
        let line = read_line(r)?;
        // chunk 扩展（`1a;foo=bar`）要丢掉
        let size_part = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_part, 16)
            .map_err(|_| WireError::BadHeader(format!("chunk 长度不合法：{line:?}")))?;
        if size == 0 {
            // 结尾：可能是 0\r\n\r\n，也可能带 trailer 头
            loop {
                let l = read_line(r)?;
                if l.is_empty() {
                    break;
                }
            }
            return Ok(out);
        }
        if out.len() + size > max_body {
            return Err(WireError::BodyTooLarge(out.len() + size));
        }
        let mut buf = vec![0u8; size];
        r.read_exact(&mut buf)?;
        out.append(&mut buf);
        // 每个 chunk 后面那个 CRLF
        let crlf = read_line(r)?;
        if !crlf.is_empty() {
            return Err(WireError::BadHeader("chunk 后缺少空行".into()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    fn parse(bytes: &[u8]) -> Result<WireResponse, WireError> {
        read_response(&mut BufReader::new(std::io::Cursor::new(bytes.to_vec())), 1 << 20)
    }

    #[test]
    fn reads_content_length_body() {
        let r = parse(b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello").unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"hello");
        assert_eq!(r.header("Content-Length"), Some("5"));
    }

    #[test]
    fn decodes_chunked() {
        let raw = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n\
                    5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let r = parse(raw).unwrap();
        assert_eq!(r.body, b"hello world");
    }

    #[test]
    fn chunked_with_trailer_is_ok() {
        let raw = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n\
                    2\r\nok\r\n0\r\ngrpc-status: 0\r\n\r\n";
        assert_eq!(parse(raw).unwrap().body, b"ok");
    }

    #[test]
    fn no_length_means_read_to_eof() {
        let r = parse(b"HTTP/1.1 200 OK\r\n\r\nuntil-close").unwrap();
        assert_eq!(r.body, b"until-close");
    }

    #[test]
    fn rejects_oversized_body() {
        let err = read_response(
            &mut BufReader::new(std::io::Cursor::new(
                b"HTTP/1.1 200 OK\r\ncontent-length: 1048576\r\n\r\nxx".to_vec(),
            )),
            1024,
        )
        .unwrap_err();
        assert!(matches!(err, WireError::BodyTooLarge(_)));
    }

    #[test]
    fn rejects_bad_status_line() {
        assert!(matches!(
            parse(b"NOT-HTTP\r\n\r\n"),
            Err(WireError::BadStatusLine(_))
        ));
    }

    #[test]
    fn no_body_for_204_even_with_length() {
        let r = parse(b"HTTP/1.1 204 No Content\r\ncontent-length: 10\r\n\r\n").unwrap();
        assert!(r.body.is_empty(), "204 不能等 body，否则会吃掉下一条响应");
    }

    #[test]
    fn truncation_is_detected() {
        let err = parse(b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhel").unwrap_err();
        assert!(matches!(err, WireError::Io(_) | WireError::Truncated));
    }

    #[test]
    fn header_injection_is_not_written() {
        let req = WireRequest {
            method: "GET",
            target: "/x".into(),
            host: "h".into(),
            headers: vec![("bad".into(), "v\r\nx-evil: 1".into())],
            body: Vec::new(),
        };
        let raw = String::from_utf8(req.serialize()).unwrap();
        assert!(!raw.contains("x-evil"));
    }
}
