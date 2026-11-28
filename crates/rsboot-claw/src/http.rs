//! 极简 HTTP/1.1 客户端。
//!
//! 只做爬虫需要的那半边：`GET`、keep-alive 复用、`Content-Length` / `chunked`
//! / 读到 EOF 三种 body 形态、3xx 跟随。不做的：TLS、HTTP/2、gzip 解压、
//! cookie、缓存。
//!
//! 为什么不用现成的 client：rsboot 核心零第三方依赖，扩展 crate 也守这条线；
//! 而爬虫对客户端的要求比一般 API 调用**更松**——不需要连接池并发安全，
//! 因为每个工作线程自己持有一个 `Client`，池天然是线程私有的。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::url::Url;

/// 出错种类。爬虫对错误的态度是"记一笔、跳过、继续"，所以这里只需要能分类。
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// 响应头/状态行不合语法
    Protocol(String),
    /// `https` 需要 TLS，本 crate 明确不支持
    UnsupportedScheme(String),
    /// 响应体超过上限
    BodyTooLarge { limit: usize },
    /// 重定向超过上限
    TooManyRedirects(usize),
    /// 连不上
    Connect(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io: {e}"),
            Error::Protocol(m) => write!(f, "protocol: {m}"),
            Error::UnsupportedScheme(s) => write!(f, "unsupported scheme: {s} (claw 只做 http)"),
            Error::BodyTooLarge { limit } => write!(f, "body too large (> {limit} bytes)"),
            Error::TooManyRedirects(n) => write!(f, "too many redirects (> {n})"),
            Error::Connect(m) => write!(f, "connect: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl Error {
    /// 复用的连接被对端悄悄关掉时会出现这几种错误。区分出来才能只对它们重试。
    fn is_stale_connection(&self) -> bool {
        match self {
            Error::Io(e) => matches!(
                e.kind(),
                std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
            ),
            Error::Protocol(m) => m == "empty response" || m == "truncated headers",
            _ => false,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// `exchange` 的返回：`(状态, 头, body, 连接是否还能复用)`。
/// 拆成别名是因为 4 元组直接写在签名里会触发 `type_complexity`。
type Exchange = (u16, Vec<(String, String)>, Vec<u8>, bool);

/// 一次成功的抓取。
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// 跟随重定向之后的最终地址
    pub url: Url,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn content_type(&self) -> &str {
        self.header("content-type").unwrap_or("")
    }

    /// 是不是可以按 HTML 解析。
    ///
    /// 没有 `Content-Type` 时按 HTML 处理：老站点常省这个头，
    /// 直接当二进制丢掉会漏掉一大片。
    pub fn is_html(&self) -> bool {
        let ct = self.content_type().to_ascii_lowercase();
        ct.is_empty() || ct.contains("html") || ct.contains("xml")
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// 一个线程一个 `Client`，内含 per-host 的 keep-alive 连接。
pub struct Client {
    user_agent: String,
    timeout: Duration,
    max_redirects: usize,
    max_body: usize,
    pool: HashMap<String, BufReader<TcpStream>>,
    /// 累计建立的连接数，用来确认 keep-alive 真的生效了
    connects: usize,
}

impl Client {
    pub fn new() -> Self {
        Self {
            user_agent: format!("rsboot-claw/{}", env!("CARGO_PKG_VERSION")),
            timeout: Duration::from_secs(10),
            max_redirects: 5,
            max_body: 8 * 1024 * 1024,
            pool: HashMap::new(),
            connects: 0,
        }
    }

    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = ua.into();
        self
    }

    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }

    pub fn max_body_bytes(mut self, n: usize) -> Self {
        self.max_body = n;
        self
    }

    pub fn connects(&self) -> usize {
        self.connects
    }

    /// GET 一个地址，自动跟随 3xx。
    pub fn get(&mut self, url: &Url) -> Result<Response> {
        self.get_with(url, &[])
    }

    /// 同 [`Client::get`]，但额外带一组请求头。
    ///
    /// `user-agent` 会**覆盖** Client 自己的 UA（不是追加，否则一次请求里
    /// 出现两个 UA，服务端怎么取都算我们错）；`host` / `connection` /
    /// `accept-encoding` 会被忽略——这三个由连接和协议层决定，让中间件改
    /// 只会改出协议错误。其余头原样拼进请求。
    pub fn get_with(&mut self, url: &Url, extra: &[(String, String)]) -> Result<Response> {
        let mut cur = url.clone();
        for _ in 0..=self.max_redirects {
            let resp = self.get_once(&cur, extra)?;
            if (300..400).contains(&resp.status) {
                if let Some(loc) = resp.header("location") {
                    // 3xx 的 body 没用，也不再复用这条连接（不少服务端在 3xx 后关连接）
                    self.drop_pool(&cur);
                    let next = Url::resolve(&cur, loc).ok_or_else(|| {
                        Error::Protocol(format!("bad location: {loc}"))
                    })?;
                    cur = next;
                    continue;
                }
            }
            return Ok(Response { url: cur, ..resp });
        }
        Err(Error::TooManyRedirects(self.max_redirects))
    }

    fn get_once(&mut self, url: &Url, extra: &[(String, String)]) -> Result<Response> {
        if url.scheme != "http" {
            return Err(Error::UnsupportedScheme(url.scheme.clone()));
        }
        let key = url.host_header();
        let pooled = self.pool.remove(&key);
        let reused = pooled.is_some();
        let mut reader = match pooled {
            Some(r) => r,
            None => self.connect(url)?,
        };
        match self.exchange(&mut reader, url, extra) {
            Ok((status, headers, body, keep)) => {
                if keep {
                    self.pool.insert(key, reader);
                }
                Ok(Response {
                    status,
                    headers,
                    body,
                    url: url.clone(),
                })
            }
            Err(e) if reused && e.is_stale_connection() => {
                // 复用到的连接可能已被对端悄悄关掉：重来一次，用新连接
                let mut fresh = self.connect(url)?;
                let (status, headers, body, keep) = self.exchange(&mut fresh, url, extra)?;
                if keep {
                    self.pool.insert(key, fresh);
                }
                Ok(Response {
                    status,
                    headers,
                    body,
                    url: url.clone(),
                })
            }
            Err(e) => Err(e),
        }
    }

    fn drop_pool(&mut self, url: &Url) {
        self.pool.remove(&url.host_header());
    }

    fn connect(&mut self, url: &Url) -> Result<BufReader<TcpStream>> {
        let addr = format!("{}:{}", url.host, url.real_port());
        let addrs: Vec<_> = addr
            .to_socket_addrs()
            .map_err(|e| Error::Connect(format!("{addr}: {e}")))?
            .collect();
        let mut last = None;
        for a in addrs {
            match TcpStream::connect_timeout(&a, self.timeout) {
                Ok(s) => {
                    s.set_read_timeout(Some(self.timeout))?;
                    s.set_write_timeout(Some(self.timeout))?;
                    let _ = s.set_nodelay(true);
                    self.connects += 1;
                    return Ok(BufReader::new(s));
                }
                Err(e) => last = Some(e),
            }
        }
        Err(Error::Connect(match last {
            Some(e) => format!("{addr}: {e}"),
            None => format!("{addr}: no address"),
        }))
    }

    /// 发请求、读响应。返回 `(状态, 头, body, 连接是否可复用)`。
    fn exchange(
        &mut self,
        reader: &mut BufReader<TcpStream>,
        url: &Url,
        extra: &[(String, String)],
    ) -> Result<Exchange> {
        let ua: String = extra
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| self.user_agent.clone());
        let mut extra_lines = String::new();
        for (k, v) in extra {
            // 名字里带 CR/LF 的头会把请求切开（请求头注入），直接丢
            if k.contains(['\r', '\n']) || v.contains(['\r', '\n']) {
                continue;
            }
            if ["user-agent", "host", "connection", "accept-encoding"]
                .iter()
                .any(|h| k.eq_ignore_ascii_case(h))
            {
                continue;
            }
            extra_lines.push_str(k);
            extra_lines.push_str(": ");
            extra_lines.push_str(v);
            extra_lines.push_str("\r\n");
        }
        let req = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {}\r\nAccept: text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8\r\nAccept-Encoding: identity\r\n{}Connection: keep-alive\r\n\r\n",
            url.request_target(),
            url.host_header(),
            ua,
            extra_lines
        );
        {
            let s = reader.get_mut();
            s.write_all(req.as_bytes())?;
            s.flush()?;
        }

        let (status, headers) = read_head(reader)?;
        let keep = {
            let conn = header_of(&headers, "connection").unwrap_or("");
            let conn = conn.to_ascii_lowercase();
            if conn.contains("close") {
                false
            } else if conn.contains("keep-alive") {
                true
            } else {
                // HTTP/1.1 默认长连接
                status >= 200
            }
        };

        let chunked = header_of(&headers, "transfer-encoding")
            .map(|v| v.to_ascii_lowercase().contains("chunked"))
            .unwrap_or(false);
        let len = header_of(&headers, "content-length").and_then(|v| v.trim().parse::<usize>().ok());

        let body = if chunked {
            read_chunked(reader, self.max_body)?
        } else if let Some(n) = len {
            if n > self.max_body {
                return Err(Error::BodyTooLarge {
                    limit: self.max_body,
                });
            }
            let mut buf = Vec::with_capacity(n.min(64 * 1024));
            let got = (&mut *reader).take(n as u64).read_to_end(&mut buf)?;
            if got < n {
                // 短读：连接已经不可用
                return Ok((status, headers, buf, false));
            }
            buf
        } else {
            // 既没长度也不是 chunked：读到 EOF，之后必须关连接
            let mut buf = Vec::new();
            let got = (&mut *reader)
                .take(self.max_body as u64 + 1)
                .read_to_end(&mut buf)?;
            if got > self.max_body {
                return Err(Error::BodyTooLarge {
                    limit: self.max_body,
                });
            }
            return Ok((status, headers, buf, false));
        };

        // 204 / 304 没有 body
        Ok((status, headers, body, keep))
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

fn header_of<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn read_head(r: &mut impl BufRead) -> Result<(u16, Vec<(String, String)>)> {
    let mut line = String::new();
    let n = r.read_line(&mut line)?;
    if n == 0 {
        return Err(Error::Protocol("empty response".into()));
    }
    let status_line = line.trim_end();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| Error::Protocol(format!("bad status line: {status_line}")))?;

    let mut headers = Vec::new();
    let mut total = 0usize;
    loop {
        line.clear();
        let n = r.read_line(&mut line)?;
        if n == 0 {
            return Err(Error::Protocol("truncated headers".into()));
        }
        total += n;
        if total > 64 * 1024 {
            return Err(Error::Protocol("headers too large".into()));
        }
        let l = line.trim_end_matches(['\r', '\n']);
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    Ok((status, headers))
}

fn read_chunked(r: &mut impl BufRead, max: usize) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        r.read_line(&mut line)?;
        let size_part = line.split([';', '\r', '\n']).next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_part, 16)
            .map_err(|_| Error::Protocol(format!("bad chunk size: {size_part:?}")))?;
        if size == 0 {
            // trailer
            loop {
                line.clear();
                let n = r.read_line(&mut line)?;
                if n == 0 || line.trim_end_matches(['\r', '\n']).is_empty() {
                    break;
                }
            }
            return Ok(body);
        }
        if body.len() + size > max {
            return Err(Error::BodyTooLarge { limit: max });
        }
        let mut chunk = vec![0u8; size];
        r.read_exact(&mut chunk)?;
        body.extend_from_slice(&chunk);
        // 块尾的 CRLF
        line.clear();
        r.read_line(&mut line)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread;

    /// 起一个只服务 `n` 个连接的假服务器，请求内容交给 `f` 处理。
    fn serve(n: usize, f: impl Fn(&str, usize) -> String + Send + 'static) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        thread::spawn(move || {
            let mut served = 0usize;
            while served < n {
                let (mut s, _) = l.accept().unwrap();
                let mut r = BufReader::new(s.try_clone().unwrap());
                // 一个连接上可能连着好几个 keep-alive 请求
                while served < n {
                    let mut req = String::new();
                    loop {
                        let mut line = String::new();
                        if r.read_line(&mut line).unwrap() == 0 {
                            return;
                        }
                        if line.trim_end().is_empty() {
                            break;
                        }
                        req.push_str(&line);
                    }
                    let body = f(&req, served);
                    served += 1;
                    let keep = !body.contains("Connection: close");
                    let _ = s.write_all(body.as_bytes());
                    if !keep {
                        break;
                    }
                }
            }
        });
        format!("{addr}")
    }

    fn url(addr: &str, path: &str) -> Url {
        Url::parse(&format!("http://{addr}{path}")).unwrap()
    }

    #[test]
    fn reads_content_length() {
        let addr = serve(1, |_req, _| {
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello"
                .into()
        });
        let mut c = Client::new();
        let r = c.get(&url(&addr, "/x")).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.text(), "hello");
        assert!(r.is_html());
        assert_eq!(r.url.path, "/x");
    }

    #[test]
    fn reads_chunked() {
        let addr = serve(1, |_req, _| {
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n7;x=y\r\n, world\r\n0\r\n\r\n".into()
        });
        let mut c = Client::new();
        let r = c.get(&url(&addr, "/")).unwrap();
        assert_eq!(r.text(), "hello, world");
    }

    #[test]
    fn reads_until_eof_when_no_length() {
        let addr = serve(1, |_req, _| {
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\nbody".into()
        });
        let mut c = Client::new();
        assert_eq!(c.get(&url(&addr, "/")).unwrap().text(), "body");
    }

    #[test]
    fn follows_redirects_relatively() {
        let addr = serve(2, |_req, i| {
            if i == 0 {
                "HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
            } else {
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".into()
            }
        });
        let mut c = Client::new();
        let r = c.get(&url(&addr, "/start")).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.text(), "ok");
        assert_eq!(r.url.path, "/final");
    }

    #[test]
    fn redirect_loop_is_bounded() {
        let addr = serve(10, |_req, _| {
            "HTTP/1.1 302 Found\r\nLocation: /loop\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
        });
        let mut c = Client::new().max_body_bytes(1024);
        match c.get(&url(&addr, "/loop")) {
            Err(Error::TooManyRedirects(_)) => {}
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn keep_alive_reuses_connection() {
        let addr = serve(2, |_req, _| {
            "HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc".into()
        });
        let mut c = Client::new();
        for _ in 0..2 {
            assert_eq!(c.get(&url(&addr, "/")).unwrap().text(), "abc");
        }
        assert_eq!(c.connects(), 1, "第二次应当复用连接");
    }

    #[test]
    fn body_limit_is_enforced() {
        let addr = serve(1, |_req, _| {
            "HTTP/1.1 200 OK\r\nContent-Length: 9999\r\nConnection: close\r\n\r\n".into()
        });
        let mut c = Client::new().max_body_bytes(1024);
        match c.get(&url(&addr, "/")) {
            Err(Error::BodyTooLarge { limit }) => assert_eq!(limit, 1024),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn extra_headers_override_ua_and_drop_reserved() {
        let addr = serve(1, |req, _| {
            let body = format!(
                "ua={}|xtoken={}|host={}",
                req.matches("User-Agent:").count(),
                req.contains("X-Token: abc"),
                req.contains("Host: ")
            );
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
        });
        let mut c = Client::new().user_agent("default-agent");
        let extra = vec![
            ("User-Agent".to_string(), "rotated-agent".to_string()),
            ("X-Token".to_string(), "abc".to_string()),
            ("Host".to_string(), "evil.example".to_string()),
        ];
        let r = c.get_with(&url(&addr, "/"), &extra).unwrap();
        assert_eq!(r.text(), "ua=1|xtoken=true|host=true", "UA 必须唯一且被覆盖");
    }

    #[test]
    fn header_injection_is_blocked() {
        let addr = serve(1, |req, _| {
            let injected = req.contains("X-Evil");
            let body = if injected { "LEAK" } else { "clean" };
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
        });
        let mut c = Client::new();
        let extra = vec![("X-Bad".to_string(), "a\r\nX-Evil: 1".to_string())];
        assert_eq!(c.get_with(&url(&addr, "/"), &extra).unwrap().text(), "clean");
    }

    #[test]
    fn https_is_rejected_clearly() {
        let mut c = Client::new();
        let u = Url::parse("https://example.com/").unwrap();
        match c.get(&u) {
            Err(Error::UnsupportedScheme(s)) => assert_eq!(s, "https"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn dead_pooled_connection_retries() {
        // 第一个连接正常返回但随即关闭；第二次请求应当自动重连
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        thread::spawn(move || {
            let mut count = 0;
            for s in l.incoming() {
                let mut s = s.unwrap();
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                while r.read_line(&mut line).unwrap() > 0 && !line.trim_end().is_empty() {
                    line.clear();
                }
                count += 1;
                let _ = s.write_all(
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}", count, count).as_bytes(),
                );
                if count >= 2 {
                    break;
                }
            }
        });
        let mut c = Client::new();
        assert_eq!(c.get(&url(&addr, "/")).unwrap().text(), "1");
        assert_eq!(c.get(&url(&addr, "/")).unwrap().text(), "2");
        assert_eq!(c.connects(), 2);
    }
}
