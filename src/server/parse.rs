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

/// 请求行里的目标：`/path?query`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub path: String,
    pub query: String,
}

/// 请求体的长度声明。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyLen {
    /// 没有 body
    None,
    Length(usize),
}

#[derive(Debug)]
pub struct ParsedRequest {
    pub method: Method,
    pub target: Target,
    pub version: u8,
    pub headers: Headers,
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
pub fn parse_head(head: &str, max_body: usize) -> Result<ParsedRequest, ParseError> {
    let mut lines = head.split("\r\n");

    let request_line = lines.next().unwrap_or("");
    if request_line.is_empty() {
        return Err(ParseError::new(Status::BAD_REQUEST, "请求行是空的"));
    }
    if request_line.len() > MAX_REQUEST_LINE {
        return Err(ParseError::new(Status(414), "请求行过长"));
    }

    let mut parts = request_line.split(' ');
    let method_raw = parts
        .next()
        .ok_or_else(|| ParseError::new(Status::BAD_REQUEST, "缺少方法"))?;
    let target_raw = parts
        .next()
        .ok_or_else(|| ParseError::new(Status::BAD_REQUEST, "缺少请求目标"))?;
    let version_raw = parts
        .next()
        .ok_or_else(|| ParseError::new(Status::BAD_REQUEST, "缺少协议版本"))?;
    if parts.next().is_some() {
        return Err(ParseError::new(
            Status::BAD_REQUEST,
            "请求行里有多余的空格。目标里不允许出现空格（应百分号编码）",
        ));
    }

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
    let (path, query) = match target_raw.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target_raw.to_string(), String::new()),
    };
    if path.contains('#') {
        return Err(ParseError::new(Status::BAD_REQUEST, "请求目标里不能有 `#`"));
    }

    let mut headers = Headers::new();
    let mut total: usize = 0;
    let mut host_seen = false;
    let mut content_length: Option<usize> = None;
    let mut transfer_encoding = false;

    for line in lines {
        if line.is_empty() {
            continue;
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
        let Some((name, value)) = line.split_once(':') else {
            return Err(ParseError::new(
                Status::BAD_REQUEST,
                format!("头部缺少冒号：`{}`", truncate(line)),
            ));
        };
        let name = name.trim();
        let value = value.trim();
        if !headers.insert(name, value) {
            // Headers::insert 拒非法 token 与含 CRLF 的值
            return Err(ParseError::new(
                Status::BAD_REQUEST,
                format!("头部名或值非法：`{}`", truncate(name)),
            ));
        }

        match name.to_ascii_lowercase().as_str() {
            "host" => host_seen = true,
            "content-length" => {
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
            "transfer-encoding" => transfer_encoding = true,
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

    let conn_header = headers
        .get("connection")
        .map(|v| v.to_ascii_lowercase())
        .unwrap_or_default();
    let close = if version == 0 {
        // HTTP/1.0 默认关闭，除非显式 keep-alive
        !conn_header.contains("keep-alive")
    } else {
        conn_header.contains("close")
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

    fn ok(head: &str) -> ParsedRequest {
        parse_head(head, 1024 * 1024).expect("应当解析成功")
    }

    fn err(head: &str) -> ParseError {
        parse_head(head, 1024 * 1024).expect_err("应当被拒绝")
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
