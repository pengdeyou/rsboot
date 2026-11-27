//! HTTP/1.1 请求解析。
//!
//! 这一层的唯一目标是**要么完整理解请求，要么明确拒绝**。解析器绝不在
//! 半懂的状态下把请求交给 handler——那正是请求走私（request smuggling）的入口：
//! 前后端对"body 到哪结束"理解不一致，攻击者就能把一个请求劈成两个。
//!
//! ## 拒绝清单（每一条都对应一个真实攻击面）
//!
//! | 情况 | 处置 | 原因 |
//! |---|---|---|
//! | `Transfer-Encoding` 与 `Content-Length` 同时出现 | 400 + 断开 | 两者冲突，谁优先取决于实现，是走私的标准手法 |
//! | `Transfer-Encoding` 单独出现 | 501 + 断开 | 请求体只支持 `Content-Length`，明确不支持而不是猜 |
//! | `Content-Length` 重复且不一致 | 400 + 断开 | 同上，取值歧义 |
//! | 头部续行（以空格/Tab 开头） | 400 + 断开 | obs-fold 已被 RFC 7230 废弃，且是绕过头部校验的老手法 |
//! | 头部总字节 > 8 KiB | 431 + 断开 | 防头部炸弹 |
//! | 请求行（方法+路径）> 8 KiB | 414 + 断开 | 防超长 URL 打日志/打缓存 |
//! | body > 上限 | 413 + **断开** | 不能"回 413 然后继续读"——残留字节会被当成下一个请求 |
//! | HTTP/1.1 缺 `Host` | 400 | 规范要求，且虚拟主机路由需要它 |
//!
//! 断开（而不是保持连接）是刻意的：一旦解析出现歧义，这条连接上后续字节的
//! 归属就不可信了，继续复用等于把风险传给下一个请求。

use crate::web::http::{Headers, Method, Status};
use std::sync::Arc;

/// 请求行里的目标：`/path?query`。
///
/// 两个字段都是**原始请求头缓冲里的区间**，解析不做任何分配；`Request`
/// 也直接借用它们，所以一次请求从头到尾只有 body 会分配。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target<'a> {
    pub path: &'a str,
    pub query: &'a str,
}

/// 请求体的长度声明。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyLen {
    /// 没有 body
    None,
    Length(usize),
}

#[derive(Debug)]
pub struct ParsedRequest<'a> {
    pub method: Method,
    pub target: Target<'a>,
    pub version: u8,
    pub headers: Headers<'a>,
    pub body_len: BodyLen,
    /// `Connection: close` 或 HTTP/1.0 未要求 keep-alive
    pub close: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub status: Status,
    /// 面向客户端的说明（不含内部细节，也不回显原始字节）
    pub message: String,
}

impl ParseError {
    fn new(status: Status, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

/// 头部的总字节上限。8 KiB 足以放下常见客户端的全部头，又挡住头部炸弹。
pub const MAX_HEADER_BYTES: usize = 8 * 1024;
/// 请求行上限（方法 + 目标 + 版本）。
pub const MAX_REQUEST_LINE: usize = 8 * 1024;
/// 单条头行的上限，防止一行就吃满配额。
const MAX_HEADER_LINE: usize = 4 * 1024;

/// 从连接读到的原始首部（不含 body）解析成请求。
///
/// `head` 是"请求行 + 头 + 空行"这一段原始字节，`max_body` 是配置的 body 上限。
pub fn parse_head<'a>(head: &'a str, max_body: usize) -> Result<ParsedRequest<'a>, ParseError> {
    let bytes = head.as_bytes();
    // 请求行到第一个 `\r\n` 为止（整块 head 本身就是"到空行"那一段）。
    // 手扫而不是 `find("\r\n")`：后者要跑两字节搜索的通用算法，而这里
    // 一行的平均长度只有三十来字节，逐字节比调用搜索器便宜。
    let mut rl_end = 0usize;
    while rl_end + 1 < bytes.len() && !(bytes[rl_end] == b'\r' && bytes[rl_end + 1] == b'\n') {
        rl_end += 1;
    }
    if rl_end + 1 >= bytes.len() {
        rl_end = bytes.len();
    }
    let request_line = &head[..rl_end];
    if request_line.is_empty() {
        return Err(ParseError::new(Status::BAD_REQUEST, "请求行是空的"));
    }
    if request_line.len() > MAX_REQUEST_LINE {
        return Err(ParseError::new(Status(414), "请求行过长"));
    }

    // 两个空格就是三个字段的分界；第三个空格直接判错（目标里不许有空格）。
    // 手扫一趟同时拿到两个位置，比 `split(' ')` 取四次 next 少一层迭代器。
    let (mut sp1, mut sp2) = (usize::MAX, usize::MAX);
    let mut k = 0usize;
    while k < rl_end {
        if bytes[k] == b' ' {
            if sp1 == usize::MAX {
                sp1 = k;
            } else if sp2 == usize::MAX {
                sp2 = k;
            } else {
                return Err(ParseError::new(
                    Status::BAD_REQUEST,
                    "请求行里有多余的空格。目标里不允许出现空格（应百分号编码）",
                ));
            }
        }
        k += 1;
    }
    if sp1 == usize::MAX {
        return Err(ParseError::new(Status::BAD_REQUEST, "缺少请求目标"));
    }
    if sp2 == usize::MAX {
        return Err(ParseError::new(Status::BAD_REQUEST, "缺少协议版本"));
    }
    let method_raw = &request_line[..sp1];
    let target_raw = &request_line[sp1 + 1..sp2];
    let version_raw = &request_line[sp2 + 1..];

    let method = Method::parse(method_raw).ok_or_else(|| {
        ParseError::new(
            Status(405),
            format!("不支持的方法 `{method_raw}`"),
        )
    })?;

    let version = match version_raw {
        "HTTP/1.1" => 1,
        "HTTP/1.0" => 0,
        "HTTP/2.0" | "HTTP/2" => {
            return Err(ParseError::new(
                Status(505),
                "这是 HTTP/1.1 服务端；HTTP/2 需要 TLS ALPN 或 h2c 升级，暂不支持",
            ))
        }
        _ => {
            return Err(ParseError::new(
                Status::BAD_REQUEST,
                "协议版本必须是 HTTP/1.1 或 HTTP/1.0",
            ))
        }
    };

    if !target_raw.starts_with('/') {
        // 只接受 origin-form。absolute-form（`http://host/path`）常用于代理，
        // 直接支持会让"路径校验"绕过去（host 与 path 由客户端控制）。
        return Err(ParseError::new(
            Status::BAD_REQUEST,
            "请求目标必须是 `/path` 形式（origin-form）",
        ));
    }
    // `?` 与 `#` 一趟扫完：`split_once('?')` 找问号、`contains('#')` 再找井号，
    // 两趟扫描合成一趟。
    let tb = target_raw.as_bytes();
    let mut qmark = usize::MAX;
    let mut hash = usize::MAX;
    let mut m = 0usize;
    while m < tb.len() {
        let c = tb[m];
        if c == b'?' && qmark == usize::MAX {
            qmark = m;
        } else if c == b'#' {
            hash = m;
            break;
        }
        m += 1;
    }
    if hash != usize::MAX {
        return Err(ParseError::new(Status::BAD_REQUEST, "请求目标里不能有 `#`"));
    }
    let (path, query) = if qmark == usize::MAX {
        (target_raw, "")
    } else {
        (&target_raw[..qmark], &target_raw[qmark + 1..])
    };

    // 整块 head **不再复制**：`Headers` 直接借这份原文，每个头只是它上面的
    // 一个区间。原文的生命周期由连接体的 head 缓冲保证。
    let mut headers = Headers::sliced(head, 12);
    let mut total: usize = 0;
    let mut host_seen = false;
    let mut content_length: Option<usize> = None;
    let mut transfer_encoding = false;
    let mut conn_close = false;
    let mut conn_keepalive = false;

    let mut i = rl_end + 2;
    while i < bytes.len() {
        // 一行一行走，同时校验 CR/LF 规范：只允许成对的 `\r\n`，孤立的
        // `\r` / `\n` 直接拒——那是值里做 CRLF 注入的入口。
        // 顺手的好处是不用 `split("\r\n")`：它每取一行都要再打一次双字节
        // 搜索，12 行就是 200 ns，而这里整块只扫一遍。
        let line_start = i;
        let mut j = i;
        // 冒号在这里一起找：一行只扫一遍，不再 `line.find(':')` 再来一趟。
        let mut colon = usize::MAX;
        while j < bytes.len() {
            let b = bytes[j];
            if b == b'\r' || b == b'\n' {
                break;
            }
            if b == b':' && colon == usize::MAX {
                colon = j;
            }
            j += 1;
        }
        let line = &head[line_start..j];
        let next = match bytes.get(j) {
            None => bytes.len(),
            Some(&b'\r') if bytes.get(j + 1) == Some(&b'\n') => j + 2,
            Some(&b'\r') => {
                return Err(ParseError::new(
                    Status::BAD_REQUEST,
                    "头部里出现了孤立的 `\\r`（只允许成对的 CRLF）",
                ))
            }
            Some(_) => {
                return Err(ParseError::new(
                    Status::BAD_REQUEST,
                    "头部里出现了孤立的 `\\n`（只允许成对的 CRLF）",
                ))
            }
        };
        i = next;
        if line.is_empty() {
            break;
        }
        total += line.len() + 2;
        if total > MAX_HEADER_BYTES {
            return Err(ParseError::new(Status(431), "头部总字节超过 8 KiB"));
        }
        if line.len() > MAX_HEADER_LINE {
            return Err(ParseError::new(Status(431), "单条头部过长"));
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            return Err(ParseError::new(
                Status::BAD_REQUEST,
                "不支持头部续行（obs-fold）；请把同一个头写成一行",
            ));
        }
        if colon == usize::MAX {
            return Err(ParseError::new(
                Status::BAD_REQUEST,
                format!("头部缺少冒号：`{}`", truncate(line)),
            ));
        }
        // RFC 7230 的 OWS 只有 SP / HTAB。`str::trim` 走 Unicode 空白表：
        // 既慢，又把 `\u{A0}` 之类当可 trim 的空白——口径比规范宽。
        // 一次扫描拿两侧边界，下面同时用来切字符串和算区间偏移。
        let name_raw = &head[line_start..colon];
        let value_raw = &head[colon + 1..j];
        let (ns, ne) = trim_ows(name_raw);
        let (vs, ve) = trim_ows(value_raw);
        let name = &name_raw[ns..ne];
        let value = &value_raw[vs..ve];
        if !is_token_bytes(name.as_bytes()) {
            return Err(ParseError::new(
                Status::BAD_REQUEST,
                format!("头部名非法：`{}`", truncate(name)),
            ));
        }

        // 记下区间而不是复制字符串：一个请求省掉 16~24 次分配
        let name_off = line_start + ns;
        let value_off = colon + 1 + vs;
        headers.push_sliced(
            name_off as u32..(name_off + name.len()) as u32,
            value_off as u32..(value_off + value.len()) as u32,
        );

        // 每头只看一次长度再比字节：原来三条 `eq_ignore_ascii_case` 串行，
        // 一个头要扫三遍。长度不等就直接落到 `_`。
        match name.len() {
            4 if eq_ci(name, b"host") => host_seen = true,
            10 if eq_ci(name, b"connection") => {
                for t in value.split(',') {
                    let t = t.trim_ascii();
                    if t.eq_ignore_ascii_case("close") {
                        conn_close = true;
                    } else if t.eq_ignore_ascii_case("keep-alive") {
                        conn_keepalive = true;
                    }
                }
            }
            14 if eq_ci(name, b"content-length") => {
                let n: usize = value.parse().map_err(|_| {
                    ParseError::new(Status::BAD_REQUEST, "Content-Length 必须是十进制非负整数")
                })?;
                match content_length {
                    Some(prev) if prev != n => {
                        return Err(ParseError::new(
                            Status::BAD_REQUEST,
                            "Content-Length 出现了多个不同的值",
                        ))
                    }
                    _ => content_length = Some(n),
                }
            }
            17 if eq_ci(name, b"transfer-encoding") => transfer_encoding = true,
            _ => {}
        }
    }

    if transfer_encoding && content_length.is_some() {
        return Err(ParseError::new(
            Status::BAD_REQUEST,
            "`Transfer-Encoding` 与 `Content-Length` 不能同时出现（会造成请求边界歧义）",
        ));
    }
    if transfer_encoding {
        return Err(ParseError::new(
            Status(501),
            "请求体的 `Transfer-Encoding` 暂不支持；请用 `Content-Length` 发送",
        ));
    }
    if version == 1 && !host_seen {
        return Err(ParseError::new(Status::BAD_REQUEST, "HTTP/1.1 必须带 `Host` 头"));
    }

    let body_len = match content_length {
        None | Some(0) => BodyLen::None,
        Some(n) => {
            if n > max_body {
                return Err(ParseError::new(
                    Status::PAYLOAD_TOO_LARGE,
                    format!("请求体超过上限 {max_body} 字节"),
                ));
            }
            BodyLen::Length(n)
        }
    };

    // `Connection` 是逗号分隔的 token 列表。上面解析时顺手按 token 比完了，
    // 不在这里再 `get("connection")` 线性扫一遍头、也不整条 `to_lowercase()`。
    let close = if version == 0 {
        // HTTP/1.0 默认关闭，除非显式 keep-alive
        !conn_keepalive
    } else {
        conn_close
    };

    Ok(ParsedRequest {
        method,
        target: Target { path, query },
        version,
        headers,
        body_len,
        close,
    })
}

/// token 允许的字节（RFC 7230 的 tchar）。查表一次，不为每个字符走分支匹配。
/// 表开到 256 项：非 ASCII 字节自然落到 `false`，省掉逐字节的 `< 128` 判断。
static TOKEN_TABLE: [bool; 256] = {
    const fn tchar(c: u8) -> bool {
        matches!(c, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9'
            | b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' | b'^' | b'_'
            | b'`' | b'|' | b'~')
    }
    let mut t = [false; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = i < 128 && tchar(i as u8);
        i += 1;
    }
    t
};

#[inline]
fn is_token_bytes(b: &[u8]) -> bool {
    !b.is_empty() && b.iter().all(|&c| TOKEN_TABLE[c as usize])
}

/// 与给定的小写字面量做大小写不敏感比较。长度不等立刻返回。
#[inline]
fn eq_ci(s: &str, lower: &[u8]) -> bool {
    s.as_bytes().eq_ignore_ascii_case(lower)
}

/// 头部值两侧的 OWS（SP / HTAB）边界。返回字节下标区间 `[start, end)`。
#[inline(always)]
fn trim_ows(s: &str) -> (usize, usize) {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
        i += 1;
    }
    let mut j = b.len();
    while j > i && (b[j - 1] == b' ' || b[j - 1] == b'\t') {
        j -= 1;
    }
    (i, j)
}

fn truncate(s: &str) -> String {
    // 回显给客户端的错误信息里只放前 32 字节：既是诊断，又不把恶意长串铺开
    if s.len() <= 32 {
        s.to_string()
    } else {
        format!("{}…", &s[..32])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(head: &str) -> ParsedRequest<'_> {
        parse_head(head, 1024 * 1024).expect("应当解析成功")
    }

    fn err(head: &str) -> ParseError {
        parse_head(head, 1024 * 1024).expect_err("应当被拒绝")
    }





    /// 请求头解析的成本。两组样本：压测器实发的极简头（3 行），
    /// 和真实浏览器/客户端常见规模（12 头 ≈ 700 B）。
    #[test]
    fn bench_parse_head() {
        let lean = "GET /json HTTP/1.1\r\nhost: bench\r\nconnection: keep-alive\r\n\r\n";
        let full = "GET /api/v1/orders/42?page=2 HTTP/1.1\r\n\
host: api.example.com\r\n\
user-agent: Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36\r\n\
accept: application/json, text/plain;q=0.9\r\n\
accept-encoding: gzip, deflate, br\r\n\
accept-language: zh-CN,zh;q=0.9,en;q=0.8\r\n\
connection: keep-alive\r\n\
cookie: sid=8f3a9c; theme=dark; lang=zh\r\n\
referer: https://api.example.com/docs\r\n\
x-request-id: 9d2f1a7c-4b3e-4a51-9c8f-2b7d6e1a0c33\r\n\
x-forwarded-for: 203.0.113.7, 10.0.0.2\r\n\
if-none-match: \"33a64df5\"\r\n\
\r\n";
        let n = 20000usize;
        for (label, head) in [("极简 3 行", lean), ("常见 12 头", full)] {
            let started = std::time::Instant::now();
            for _ in 0..n {
                std::hint::black_box(parse_head(std::hint::black_box(head), 1 << 20).ok());
            }
            let per = started.elapsed().as_nanos() as f64 / n as f64;
            println!("parse_head {label} ({} B): {per:.0} ns/请求", head.len());
        }
    }

    #[test]
    fn parses_simple_get() {
        let r = ok("GET /users/7?limit=2 HTTP/1.1\r\nHost: a\r\n");
        assert_eq!(r.method, Method::Get);
        assert_eq!(r.target.path, "/users/7");
        assert_eq!(r.target.query, "limit=2");
        assert_eq!(r.version, 1);
        assert!(!r.close);
    }

    #[test]
    fn parses_body_with_content_length() {
        let r = ok("POST /x HTTP/1.1\r\nHost: a\r\nContent-Length: 12\r\n");
        assert_eq!(r.body_len, BodyLen::Length(12));
    }

    #[test]
    fn te_and_cl_together_is_rejected() {
        let e = err("POST /x HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n");
        assert_eq!(e.status, Status::BAD_REQUEST);
        assert!(e.message.contains("歧义"), "{}", e.message);
    }

    #[test]
    fn chunked_alone_is_501() {
        let e = err("POST /x HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n");
        assert_eq!(e.status, Status(501));
    }

    #[test]
    fn conflicting_content_lengths_are_rejected() {
        let e = err("POST /x HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\nContent-Length: 9\r\n");
        assert_eq!(e.status, Status::BAD_REQUEST);
    }

    #[test]
    fn duplicate_identical_content_length_is_fine() {
        let r = ok("POST /x HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\nContent-Length: 5\r\n");
        assert_eq!(r.body_len, BodyLen::Length(5));
    }

    #[test]
    fn obs_fold_is_rejected() {
        let e = err("GET /x HTTP/1.1\r\nHost: a\r\nX: 1\r\n  2\r\n");
        assert_eq!(e.status, Status::BAD_REQUEST);
        assert!(e.message.contains("续行"), "{}", e.message);
    }

    #[test]
    fn oversized_headers_are_431() {
        let mut head = String::from("GET /x HTTP/1.1\r\nHost: a\r\n");
        for i in 0..1000 {
            head.push_str(&format!("X-{i}: {}\r\n", "v".repeat(8)));
        }
        let e = err(&head);
        assert_eq!(e.status, Status(431));
    }

    #[test]
    fn oversized_request_line_is_414() {
        let path = "a".repeat(MAX_REQUEST_LINE + 1);
        let e = err(&format!("GET /{path} HTTP/1.1\r\nHost: a\r\n"));
        assert_eq!(e.status, Status(414));
    }

    #[test]
    fn oversized_body_is_413() {
        let e = parse_head("POST /x HTTP/1.1\r\nHost: a\r\nContent-Length: 999999999\r\n", 1024)
            .unwrap_err();
        assert_eq!(e.status, Status::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn missing_host_on_http11_is_400() {
        let e = err("GET /x HTTP/1.1\r\n");
        assert_eq!(e.status, Status::BAD_REQUEST);
        assert!(e.message.contains("Host"), "{}", e.message);
    }

    #[test]
    fn http10_defaults_to_close() {
        let r = ok("GET /x HTTP/1.0\r\n\r\n");
        assert!(r.close, "HTTP/1.0 默认关闭连接");
        let r = ok("GET /x HTTP/1.0\r\nConnection: keep-alive\r\n\r\n");
        assert!(!r.close);
    }

    #[test]
    fn absolute_form_target_is_rejected() {
        let e = err("GET http://evil/x HTTP/1.1\r\nHost: a\r\n");
        assert_eq!(e.status, Status::BAD_REQUEST);
    }

    #[test]
    fn unknown_method_is_405() {
        let e = err("BREW /x HTTP/1.1\r\nHost: a\r\n");
        assert_eq!(e.status, Status(405));
    }

    #[test]
    fn http2_request_line_is_505() {
        let e = err("GET /x HTTP/2.0\r\nHost: a\r\n");
        assert_eq!(e.status, Status(505));
    }

    #[test]
    fn negative_content_length_is_rejected() {
        let e = err("POST /x HTTP/1.1\r\nHost: a\r\nContent-Length: -1\r\n");
        assert_eq!(e.status, Status::BAD_REQUEST);
    }

    #[test]
    fn extra_spaces_in_request_line_are_rejected() {
        let e = err("GET  /x HTTP/1.1\r\nHost: a\r\n");
        // 双空格会切成空的目标
        assert_eq!(e.status, Status::BAD_REQUEST);
    }

    #[test]
    fn connection_close_is_detected() {
        let r = ok("GET /x HTTP/1.1\r\nHost: a\r\nConnection: close\r\n");
        assert!(r.close);
    }
}
