//! 极简 HTTP/1.1 客户端。
//!
//! 只做爬虫需要的那半边：`GET`、keep-alive 复用、`Content-Length` / `chunked`
//! / 读到 EOF 三种 body 形态、3xx 跟随、cookie、可选 TLS（`tls` feature）。
//! 不做的：HTTP/2、gzip 解压、缓存。
//!
//! 为什么不用现成的 client：rsboot 核心零第三方依赖，扩展 crate 也守这条线；
//! 而爬虫对客户端的要求比一般 API 调用**更松**——不需要连接池并发安全，
//! 因为每个工作线程自己持有一个 `Client`，池天然是线程私有的。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, SystemTime};

use crate::cookies::Jar;
use crate::url::Url;

pub type Conn = TcpStream;

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

/// HTTP 代理。
///
/// 只支持**明文 http 代理**（请求行用绝对 URI 的那种），这是最简单也最通用
/// 的形态：连到代理、把 `GET http://target/path HTTP/1.1` 发过去。不做
/// `CONNECT` 隧道——那是给 https 用的，而 claw 本来就不做 TLS。
///
/// 带凭据时直接在每个请求里带 `Proxy-Authorization: Basic …`，而不是
/// 先吃一个 `407` 再重试：多一次往返省不下什么，代码还多一条分支。
#[derive(Debug, Clone)]
pub struct Proxy {
    pub host: String,
    pub port: u16,
    /// `Basic base64(user:pass)`，没凭据是 `None`
    auth: Option<String>,
}

impl Proxy {
    /// 解析 `http://host:port` 或 `http://user:pass@host:port`。
    pub fn parse(spec: &str) -> Option<Self> {
        let spec = spec.trim();
        let rest = spec
            .strip_prefix("http://")
            .or_else(|| spec.strip_prefix("https://"))
            .unwrap_or(spec);
        let (userinfo, hostport) = match rest.rsplit_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, rest),
        };
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse::<u16>().ok()?),
            None => (hostport.to_string(), 8080),
        };
        if host.is_empty() {
            return None;
        }
        let auth = userinfo.map(|u| format!("Basic {}", base64(u.as_bytes())));
        Some(Self {
            host,
            port,
            auth,
        })
    }

    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// 标准 base64（RFC 4648），只为 `Proxy-Authorization` 服务。
fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len() * 4 / 3 + 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// 一个线程一个 `Client`，内含 per-host 的 keep-alive 连接。
pub struct Client {
    user_agent: String,
    timeout: Duration,
    max_redirects: usize,
    max_body: usize,
    pool: HashMap<String, BufReader<Conn>>,
    /// 累计建立的连接数，用来确认 keep-alive 真的生效了
    connects: usize,
    proxy: Option<Proxy>,
    /// 会话 cookie。`None` = 无状态（`cookie_jar(false)`）
    jar: Option<Jar>,
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
            proxy: None,
            jar: Some(Jar::new()),
        }
    }

    /// 走代理。传 `None` 等于直连。
    pub fn proxy(mut self, p: Option<Proxy>) -> Self {
        self.proxy = p;
        self.pool.clear(); // 旧池里的连接是直连的，换目标就作废
        self
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

    /// 开/关会话 cookie（默认开）。关掉会清空已有 cookie。
    ///
    /// 关掉的场景：目标站对 cookie 敏感、或者你就是要一个每个请求都等价于
    /// "第一次访问"的爬虫。想要固定登录态就别用这个，直接
    /// `StaticHeaders` 带一个 `Cookie: …` 更直接。
    pub fn cookie_jar(mut self, on: bool) -> Self {
        self.jar = if on { Some(Jar::new()) } else { None };
        self
    }

    pub fn jar(&self) -> Option<&Jar> {
        self.jar.as_ref()
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
        let key = self.pool_key(url);
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
                self.capture_cookies(url, &headers);
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
                self.capture_cookies(url, &headers);
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

    fn capture_cookies(&mut self, url: &Url, headers: &[(String, String)]) {
        if let Some(jar) = self.jar.as_mut() {
            jar.store_headers(&url.host, headers, SystemTime::now());
        }
    }

    fn drop_pool(&mut self, url: &Url) {
        let key = self.pool_key(url);
        self.pool.remove(&key);
    }

    /// 池的键：有代理时还要带上代理地址——不然换了代理会复用旧连接。
    fn pool_key(&self, url: &Url) -> String {
        // scheme 入键：`http://a.com` 与 `https://a.com:443` 将来是两条不同
        // 的连接，共用一个槽位会把明文连接复用给 https 请求
        match &self.proxy {
            Some(p) => format!("{}|{}|{}", p.addr(), url.scheme, url.host_header()),
            None => format!("{}|{}", url.scheme, url.host_header()),
        }
    }

    fn connect(&mut self, url: &Url) -> Result<BufReader<Conn>> {
        // 明文代理承载不了 https：那需要 CONNECT 隧道，claw 不做
        if url.scheme == "https" && self.proxy.is_some() {
            return Err(Error::Protocol(
                "https over a plain http proxy needs CONNECT, which claw does not do".into(),
            ));
        }
        let addr = match &self.proxy {
            Some(p) => p.addr(),
            None => format!("{}:{}", url.host, url.real_port()),
        };
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
                    return self.upgrade(s, url);
                }
                Err(e) => last = Some(e),
            }
        }
        Err(Error::Connect(match last {
            Some(e) => format!("{addr}: {e}"),
            None => format!("{addr}: no address"),
        }))
    }

    fn upgrade(&self, s: TcpStream, _url: &Url) -> Result<BufReader<Conn>> {
        Ok(BufReader::new(s))
    }

    #[cfg(feature = "tls")]
    fn upgrade(&self, s: TcpStream, url: &Url) -> Result<BufReader<Conn>> {
        if url.scheme == "https" {
            let tls = crate::tls::wrap(s, &url.host)?;
            return Ok(BufReader::new(Box::new(tls) as Box<dyn ReadWrite + Send>));
        }
        Ok(BufReader::new(Box::new(s) as Box<dyn ReadWrite + Send>))
    }

    /// 发请求、读响应。返回 `(状态, 头, body, 连接是否可复用)`。
    fn exchange(
        &mut self,
        reader: &mut BufReader<Conn>,
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
        // 直连用 origin-form（`/path`），走代理用 absolute-form（`http://host/path`）
        let target = match &self.proxy {
            Some(_) => url.to_string(),
            None => url.request_target(),
        };
        let proxy_auth = self
            .proxy
            .as_ref()
            .and_then(|p| p.auth.as_ref())
            .map(|a| format!("Proxy-Authorization: {a}\r\n"))
            .unwrap_or_default();
        // 显式给了 `Cookie` 头就以调用方为准（那是"固定身份"的用法），
        // 否则用 jar 里匹配到的。两条 cookie 头同时发出去没有意义。
        let cookie_line = if extra.iter().any(|(k, _)| k.eq_ignore_ascii_case("cookie")) {
            String::new()
        } else {
            match &self.jar {
                Some(j) => j
                    .header_for(&url.host, &url.path, url.scheme == "https", SystemTime::now())
                    .map(|c| format!("Cookie: {c}\r\n"))
                    .unwrap_or_default(),
                None => String::new(),
            }
        };
        let req = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {}\r\nAccept: text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8\r\nAccept-Encoding: identity\r\n{}{}{}Connection: keep-alive\r\n\r\n",
            target,
            url.host_header(),
            ua,
            proxy_auth,
            cookie_line,
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
    fn base64_matches_rfc4648() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"abc"), "YWJj");
        assert_eq!(base64(b"user:pass"), "dXNlcjpwYXNz");
    }

    #[test]
    fn proxy_spec_parsing() {
        let p = Proxy::parse("http://127.0.0.1:8888").unwrap();
        assert_eq!(p.addr(), "127.0.0.1:8888");
        assert!(p.auth.is_none());
        let q = Proxy::parse("http://u:p@proxy.local:3128").unwrap();
        assert_eq!(q.addr(), "proxy.local:3128");
        assert_eq!(q.auth.as_deref(), Some("Basic dTpw"));
        assert_eq!(Proxy::parse("proxy.local").unwrap().port, 8080, "缺端口按 8080");
        assert!(Proxy::parse("").is_none());
    }

    #[test]
    fn proxied_request_uses_absolute_uri_and_auth() {
        use std::sync::{Arc, Mutex};
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let seen = Arc::new(Mutex::new(String::new()));
        let s2 = seen.clone();
        thread::spawn(move || {
            if let Ok((mut s, _)) = l.accept() {
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut req = String::new();
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap() == 0 || line.trim_end().is_empty() {
                        break;
                    }
                    req.push_str(&line);
                }
                *s2.lock().unwrap() = req;
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
            }
        });
        let mut c = Client::new().proxy(Some(Proxy::parse(&format!("http://u:p@{addr}")).unwrap()));
        let u = Url::parse("http://example.invalid/path?q=1").unwrap();
        assert_eq!(c.get(&u).unwrap().text(), "ok");
        let req = seen.lock().unwrap().clone();
        assert!(
            req.starts_with("GET http://example.invalid/path?q=1 HTTP/1.1"),
            "走代理必须用 absolute-form：{req}"
        );
        assert!(req.contains("Host: example.invalid"), "{req}");
        assert!(req.contains("Proxy-Authorization: Basic dTpw"), "{req}");
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
