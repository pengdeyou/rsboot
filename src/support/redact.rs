//! 日志脱敏。
//!
//! 连接串里带密码是常态（`mysql://user:pass@host/db`），而这个字符串一定会
//! 出现在启动日志、错误信息、`/health` 诊断里。脱敏必须发生在**源头**，
//! 靠"记得别打日志"是防不住的。

/// 把 URL 里的密码与敏感 query 参数打码。
///
/// `mysql://u:p@h/db` → `mysql://u:***@h/db`
/// `...?password=x` → `...?password=***`
///
/// 认不出结构时原样返回：宁可多显示，也不能把整串吞掉导致无法定位。
pub fn url(u: &str) -> String {
    let mut out = u.to_string();

    if let (Some(scheme_end), Some(at)) = (out.find("://"), out.find('@')) {
        if at > scheme_end {
            let creds_start = scheme_end + 3;
            let creds = &out[creds_start..at];
            if let Some(colon) = creds.find(':') {
                let user = &creds[..colon];
                out = format!("{}://{}:***@{}", &out[..scheme_end], user, &out[at + 1..]);
            }
        }
    }

    for key in ["password", "passwd", "pwd", "token", "secret", "api_key", "apikey"] {
        out = mask_query_param(&out, key);
    }
    out
}

fn mask_query_param(s: &str, key: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find(key) {
        let after = &rest[pos + key.len()..];
        let candidate = after.strip_prefix('=');
        let boundary_ok = pos == 0
            || matches!(
                rest.as_bytes().get(pos - 1),
                Some(b'?') | Some(b'&') | Some(b';')
            );
        match (boundary_ok, candidate) {
            (true, Some(v)) => {
                let end = v.find(['&', '#', ';']).unwrap_or(v.len());
                out.push_str(&rest[..pos + key.len()]);
                out.push_str("=***");
                rest = &v[end..];
            }
            _ => {
                let step = pos + key.len();
                out.push_str(&rest[..step]);
                rest = &rest[step..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_in_authority_is_masked() {
        assert_eq!(
            url("mysql://rsbatis:s3cret@127.0.0.1:3306/db"),
            "mysql://rsbatis:***@127.0.0.1:3306/db"
        );
    }

    #[test]
    fn no_password_means_no_change() {
        let u = "sqlite:///app.db";
        assert_eq!(url(u), u);
        let u = "postgres://user@host/db";
        assert_eq!(url(u), u);
    }

    #[test]
    fn sensitive_query_params_are_masked() {
        assert_eq!(url("http://h/x?password=abc&a=1"), "http://h/x?password=***&a=1");
        assert_eq!(url("http://h/x?a=1&token=zz"), "http://h/x?a=1&token=***");
    }

    #[test]
    fn lookalike_keys_are_not_masked() {
        // `mypassword` 不是 `password` 参数
        assert_eq!(url("http://h/x?mypassword=1"), "http://h/x?mypassword=1");
    }

    #[test]
    fn value_with_hash_keeps_rest() {
        assert_eq!(url("http://h/x?password=a#frag"), "http://h/x?password=***#frag");
    }
}
