//! 安全响应头。
//!
//! 这些头不改变业务逻辑，但"漏掉一个"就是可被扫描器直接命中的问题：
//!
//! | 头 | 挡什么 |
//! |---|---|
//! | `x-content-type-options: nosniff` | 浏览器把 JSON 当 HTML 执行（内容嗅探 → XSS） |
//! | `x-frame-options: DENY` | 点击劫持（把本站塞进 iframe 诱导点击） |
//! | `referrer-policy: no-referrer` | 跳转时把带 token 的 URL 泄露给外站 |
//! | `permissions-policy` | 相机/麦克风/定位被第三方脚本滥用 |
//! | `strict-transport-security` | 只在 HTTPS 下发（HTTP 下发无效，还可能误导） |
//!
//! ## 为什么默认装
//!
//! "每个项目自己记得加"这件事从来没有成立过。默认装上、要改再改，
//! 比默认不装、出事后补，代价低得多。
//!
//! ## HSTS 的判定
//!
//! 只看 `x-forwarded-proto`（前置代理/负载均衡通常会带）。**不**猜端口、
//! 不猜"是不是本地"，宁可少加一个头，也不要在 HTTP 站上发 HSTS 把用户锁在
//! 打不开的域名上（HSTS 有 max-age 缓存，配错很难回滚）。
//!
//! ## 不做 CSP
//!
//! CSP 必须按业务具体配置（哪些脚本、哪些图片域名），给一个通用值只会
//! 逼使用者关掉它。要 CSP 就自己 `.middleware(...)` 加一层。

use crate::config::Config;
use crate::error::{Error, Result};
use crate::runtime::context::Context;
use crate::web::middleware::Middleware;
use crate::web::request::Request;
use crate::web::response::Response;

pub struct SecurityHeaders {
    on: bool,
    hsts_max_age: u64,
    // 只在 HTTPS 下发的头，和始终下发的头分开，逻辑一目了然
    always: Vec<(&'static str, &'static str)>,
}

impl SecurityHeaders {
    pub fn from_config(config: &Config) -> Result<Self> {
        let on = config
            .get_bool("http.security.headers")
            .map_err(|e| Error::Config(e.to_string()))?
            .unwrap_or(true);
        let hsts = config
            .get_int("http.security.hsts_max_age")
            .map_err(|e| Error::Config(e.to_string()))?
            .unwrap_or(0);
        if hsts < 0 {
            return Err(Error::Config(format!(
                "`http.security.hsts_max_age` 不能是负数，当前 {hsts}"
            )));
        }
        Ok(Self {
            on,
            hsts_max_age: hsts as u64,
            always: vec![
                ("x-content-type-options", "nosniff"),
                ("x-frame-options", "DENY"),
                ("referrer-policy", "no-referrer"),
                (
                    "permissions-policy",
                    "camera=(), microphone=(), geolocation=()",
                ),
            ],
        })
    }

    pub fn new() -> Self {
        Self {
            on: true,
            hsts_max_age: 0,
            always: vec![
                ("x-content-type-options", "nosniff"),
                ("x-frame-options", "DENY"),
                ("referrer-policy", "no-referrer"),
                (
                    "permissions-policy",
                    "camera=(), microphone=(), geolocation=()",
                ),
            ],
        }
    }
}

impl Default for SecurityHeaders {
    fn default() -> Self {
        Self::new()
    }
}

impl Middleware for SecurityHeaders {
    fn name(&self) -> &'static str {
        "security"
    }

    fn after(&self, req: &Request, _ctx: &Context, mut resp: Response) -> Response {
        if !self.on {
            return resp;
        }
        for (k, v) in &self.always {
            // 业务已经显式设过就不覆盖：使用者的选择优先
            if !resp.headers.contains(k) {
                resp.headers.insert(k, v);
            }
        }
        if self.hsts_max_age > 0 {
            let https = req
                .header("x-forwarded-proto")
                .map(|p| p.eq_ignore_ascii_case("https"))
                .unwrap_or(false);
            if https && !resp.headers.contains("strict-transport-security") {
                resp.headers.insert(
                    "strict-transport-security",
                    &format!("max-age={}", self.hsts_max_age),
                );
            }
        }
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Environment;
    use crate::web::http::Method;

    fn ctx() -> Context {
        Context::new(Config::new(), vec![], None, Environment::Dev)
    }

    #[test]
    fn adds_baseline_headers() {
        let m = SecurityHeaders::new();
        let req = Request::new(Method::Get, "/a");
        let out = m.after(&req, &ctx(), Response::text("x"));
        assert_eq!(out.headers.get("x-content-type-options"), Some("nosniff"));
        assert_eq!(out.headers.get("x-frame-options"), Some("DENY"));
        assert_eq!(out.headers.get("referrer-policy"), Some("no-referrer"));
    }

    #[test]
    fn hsts_only_on_https() {
        let m = SecurityHeaders {
            on: true,
            hsts_max_age: 31536000,
            always: vec![],
        };
        let plain = Request::new(Method::Get, "/a");
        assert!(m
            .after(&plain, &ctx(), Response::text("x"))
            .headers
            .get("strict-transport-security")
            .is_none());

        let proxied = Request::new(Method::Get, "/a").with_header("x-forwarded-proto", "https");
        let out = m.after(&proxied, &ctx(), Response::text("x"));
        assert_eq!(
            out.headers.get("strict-transport-security"),
            Some("max-age=31536000")
        );
    }

    #[test]
    fn business_choice_wins() {
        let m = SecurityHeaders::new();
        let req = Request::new(Method::Get, "/a");
        let mut resp = Response::text("x");
        resp.headers.insert("x-frame-options", "SAMEORIGIN");
        let out = m.after(&req, &ctx(), resp);
        assert_eq!(out.headers.get("x-frame-options"), Some("SAMEORIGIN"));
    }

    #[test]
    fn can_be_turned_off() {
        let m = SecurityHeaders {
            on: false,
            hsts_max_age: 0,
            always: vec![],
        };
        let out = m.after(&Request::new(Method::Get, "/a"), &ctx(), Response::text("x"));
        assert!(out.headers.get("x-content-type-options").is_none());
    }
}
