//! CORS。
//!
//! ## 三条容易配错、配错就出安全事件的规则
//!
//! **1. `*` 与 credentials 不能共存。** 浏览器明确禁止
//! `Access-Control-Allow-Origin: *` + `Allow-Credentials: true` 的组合——
//! 那等于"任何站点都可以带着用户的 Cookie 读这个接口"。所以这里在**启动期**
//! 就把它变成配置错误，而不是等浏览器拒绝、开发者苦查"为什么 Cookie 没带上"。
//!
//! **2. 回显具体 Origin 时必须 `Vary: Origin`。** 否则中间缓存/CDN 会把
//! A 站拿到的响应（含 `Allow-Origin: a.com`）原样发给 B 站，等于绕过白名单。
//!
//! **3. 预检只在 `Access-Control-Request-Method` 存在时处理。**
//! 普通的 `OPTIONS`（能力探测）要留给 Router 回 `204 + Allow`，
//! 不能被 CORS 顺手接走——那会让"这个路径支持哪些方法"的信息消失。
//!
//! ## 白名单写法
//!
//! `*` / 逗号分隔的精确值 / 末尾 `*` 前缀（`https://*.example.com` 不支持——
//! 通配子域在 CORS 里没有意义且容易被配成 `https://` 前缀匹配全部）：
//! 前缀匹配按**字符串前缀**，`http://localhost:*` 可以，因为端口变化是本地开发的常态。

use crate::config::Config;
use crate::error::{Error, Result};
use crate::runtime::context::Context;
use crate::web::http::{Method, Status};
use crate::web::request::Request;
use crate::web::response::Response;
use crate::web::middleware::Middleware;

#[derive(Debug, Clone, PartialEq, Eq)]
enum OriginRule {
    /// `*`：允许任意来源（**不允许**配 credentials）
    Any,
    /// 精确或前缀匹配
    List(Vec<String>),
}

impl OriginRule {
    fn allows(&self, origin: &str) -> bool {
        match self {
            OriginRule::Any => true,
            OriginRule::List(items) => items.iter().any(|p| {
                if let Some(prefix) = p.strip_suffix('*') {
                    origin.starts_with(prefix)
                } else {
                    origin.eq_ignore_ascii_case(p)
                }
            }),
        }
    }

    fn is_any(&self) -> bool {
        matches!(self, OriginRule::Any)
    }
}

#[derive(Debug, Clone)]
pub struct Cors {
    origins: OriginRule,
    allow_methods: Vec<String>,
    allow_headers: Vec<String>,
    allow_credentials: bool,
    max_age: u64,
}

impl Cors {
    /// 从配置构造。没有 `http.cors.allow_origin` 就**不装**（返回 `None`），
    /// 而不是偷偷给一个 `*` —— 默认全开是最糟的默认值。
    pub fn from_config(config: &Config) -> Result<Option<Self>> {
        let Some(raw) = config.get("http.cors.allow_origin") else {
            return Ok(None);
        };
        let mut origins = Vec::new();
        let mut any = false;
        for item in raw.split(',') {
            let t = item.trim();
            if t == "*" {
                any = true;
            } else if !t.is_empty() {
                origins.push(t.to_string());
            }
        }
        if !any && origins.is_empty() {
            return Err(Error::Config(
                "`http.cors.allow_origin` 是空的。想放开就写 `*`，想收紧就写具体来源".into(),
            ));
        }
        let allow_credentials = config
            .get_bool("http.cors.allow_credentials")?
            .unwrap_or(false);
        if any && allow_credentials {
            return Err(Error::Config(
                "`http.cors.allow_origin = *` 不能和 `allow_credentials = true` 同时用：\
                 浏览器会拒绝这种组合，而且那等于允许任意站点带 Cookie 读你的接口。\
                 请列出具体来源"
                    .into(),
            ));
        }

        let methods = config
            .get("http.cors.allow_methods")
            .map(|s| {
                s.split(',')
                    .map(|m| m.trim().to_ascii_uppercase())
                    .filter(|m| !m.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| {
                vec![
                    "GET".into(),
                    "POST".into(),
                    "PUT".into(),
                    "PATCH".into(),
                    "DELETE".into(),
                    "HEAD".into(),
                    "OPTIONS".into(),
                ]
            });
        // 白名单外的字符串会变成"看起来配了、其实永远不生效"的头，注册期拒绝
        for m in &methods {
            if Method::parse(m).is_none() {
                return Err(Error::Config(format!(
                    "`http.cors.allow_methods` 里有未知方法 `{m}`"
                )));
            }
        }

        let headers = config
            .get("http.cors.allow_headers")
            .map(|s| {
                s.split(',')
                    .map(|h| h.trim().to_ascii_lowercase())
                    .filter(|h| !h.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| {
                vec![
                    "content-type".into(),
                    "authorization".into(),
                    "x-request-id".into(),
                    "accept".into(),
                ]
            });

        let max_age = config
            .get_int("http.cors.max_age")
            .map_err(|e| Error::Config(e.to_string()))?
            .unwrap_or(600);
        if max_age < 0 {
            return Err(Error::Config(format!(
                "`http.cors.max_age` 不能是负数，当前 {max_age}"
            )));
        }

        Ok(Some(Self {
            origins: if any {
                OriginRule::Any
            } else {
                OriginRule::List(origins)
            },
            allow_methods: methods,
            allow_headers: headers,
            allow_credentials,
            max_age: max_age as u64,
        }))
    }

    pub fn any() -> Self {
        Self {
            origins: OriginRule::Any,
            allow_methods: vec![
                "GET".into(),
                "POST".into(),
                "PUT".into(),
                "PATCH".into(),
                "DELETE".into(),
                "HEAD".into(),
                "OPTIONS".into(),
            ],
            allow_headers: vec!["content-type".into(), "authorization".into()],
            allow_credentials: false,
            max_age: 600,
        }
    }

    pub fn allow_credentials(mut self, yes: bool) -> Self {
        self.allow_credentials = yes;
        self
    }

    fn allow_origin_value(&self, req: &Request) -> Option<String> {
        let origin = req.header("origin")?;
        if !self.origins.allows(origin) {
            return None;
        }
        Some(match self.origins {
            // `*` 才输出 `*`；白名单命中要回显具体来源
            OriginRule::Any => "*".to_string(),
            _ => origin.to_string(),
        })
    }

    fn add_common(&self, req: &Request, mut resp: Response) -> Response {
        if let Some(v) = self.allow_origin_value(req) {
            resp.headers.insert("access-control-allow-origin", &v);
            if !self.origins.is_any() {
                // 回显了具体来源 → 必须 Vary，否则缓存会把 A 站的授权发给 B 站
                resp.headers.insert("vary", "origin");
            }
            if self.allow_credentials {
                resp.headers.insert("access-control-allow-credentials", "true");
            }
        }
        resp
    }
}

impl Middleware for Cors {
    fn name(&self) -> &'static str {
        "cors"
    }

    fn before(&self, req: &mut Request, _ctx: &Context) -> Option<Response> {
        // 只有"真预检"才归 CORS；普通 OPTIONS 交给 Router 去回 Allow
        if req.method != Method::Options {
            return None;
        }
        req.header("access-control-request-method")?;

        let mut resp = Response::new(Status::NO_CONTENT);

        // 请求的方法必须在白名单里，否则不带 allow-methods（等价于拒绝）
        let asked = req
            .header("access-control-request-method")
            .map(|s| s.to_ascii_uppercase());
        if let Some(m) = asked {
            if self.allow_methods.contains(&m) {
                resp.headers
                    .insert("access-control-allow-methods", &self.allow_methods.join(", "));
            }
        }

        // 请求头逐个校验：全部命中才回 allow-headers
        let mut headers_ok = true;
        if let Some(raw) = req.header("access-control-request-headers") {
            let asked: Vec<String> = raw
                .split(',')
                .map(|h| h.trim().to_ascii_lowercase())
                .filter(|h| !h.is_empty())
                .collect();
            headers_ok = asked.iter().all(|h| {
                self.allow_headers.iter().any(|a| a == h)
                    // 简单请求头由浏览器自己管，不在列表里也算通过
                    || matches!(
                        h.as_str(),
                        "accept" | "accept-language" | "content-language"
                    )
            });
            if headers_ok && !asked.is_empty() {
                resp.headers
                    .insert("access-control-allow-headers", &asked.join(", "));
            }
        }
        if !headers_ok {
            // 预检不通过：回 204 但不给 allow 头，浏览器据此拒绝发出真实请求
            return Some(resp);
        }

        resp.headers
            .insert("access-control-max-age", &self.max_age.to_string());
        Some(self.add_common(req, resp))
    }

    fn after(&self, req: &Request, _ctx: &Context, resp: Response) -> Response {
        self.add_common(req, resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::http::Method;

    fn cfg(pairs: &[(&str, &str)]) -> Config {
        let mut c = Config::new();
        for (k, v) in pairs {
            c = c.set(*k, *v);
        }
        c
    }

    fn req(method: Method, headers: &[(&str, &str)]) -> Request {
        let mut r = Request::new(method, "/x");
        for (k, v) in headers {
            r.headers.insert(k, v);
        }
        r
    }

    #[test]
    fn no_config_means_no_cors() {
        assert!(Cors::from_config(&Config::new()).unwrap().is_none());
    }

    #[test]
    fn star_with_credentials_is_refused_at_startup() {
        let c = cfg(&[
            ("http.cors.allow_origin", "*"),
            ("http.cors.allow_credentials", "true"),
        ]);
        let e = Cors::from_config(&c).unwrap_err();
        assert!(e.to_string().contains("不能和"), "{e}");
    }

    #[test]
    fn dynamic_origin_echoes_and_varies() {
        let c = cfg(&[("http.cors.allow_origin", "https://a.com,https://b.com")]);
        let cors = Cors::from_config(&c).unwrap().unwrap();
        let r = req(Method::Get, &[("origin", "https://b.com")]);
        let out = cors.after(&r, &ctx(), Response::text("x"));
        assert_eq!(out.headers.get("access-control-allow-origin"), Some("https://b.com"));
        assert_eq!(out.headers.get("vary"), Some("origin"));
    }

    #[test]
    fn unknown_origin_gets_no_cors_headers() {
        let c = cfg(&[("http.cors.allow_origin", "https://a.com")]);
        let cors = Cors::from_config(&c).unwrap().unwrap();
        let r = req(Method::Get, &[("origin", "https://evil.com")]);
        let out = cors.after(&r, &ctx(), Response::text("x"));
        assert!(out.headers.get("access-control-allow-origin").is_none());
    }

    #[test]
    fn star_does_not_echo_origin() {
        let cors = Cors::any();
        let r = req(Method::Get, &[("origin", "https://a.com")]);
        let out = cors.after(&r, &ctx(), Response::text("x"));
        assert_eq!(out.headers.get("access-control-allow-origin"), Some("*"));
        // `*` 不需要 Vary：响应对所有来源都一样
        assert!(out.headers.get("vary").is_none());
    }

    #[test]
    fn preflight_is_answered_and_short_circuits() {
        let cors = Cors::any();
        let mut r = req(
            Method::Options,
            &[
                ("origin", "https://a.com"),
                ("access-control-request-method", "POST"),
                ("access-control-request-headers", "content-type"),
            ],
        );
        let out = cors.before(&mut r, &ctx()).expect("预检必须短路");
        assert_eq!(out.status, Status::NO_CONTENT);
        assert!(out.headers.get("access-control-allow-methods").unwrap().contains("POST"));
        assert_eq!(out.headers.get("access-control-max-age"), Some("600"));
    }

    #[test]
    fn plain_options_is_not_hijacked() {
        // 没有 Access-Control-Request-Method 的 OPTIONS 要留给 Router
        let cors = Cors::any();
        let mut r = req(Method::Options, &[("origin", "https://a.com")]);
        assert!(cors.before(&mut r, &ctx()).is_none());
    }

    #[test]
    fn disallowed_request_header_fails_preflight_without_allow_header() {
        let cors = Cors::any();
        let mut r = req(
            Method::Options,
            &[
                ("access-control-request-method", "POST"),
                ("access-control-request-headers", "x-custom-secret"),
            ],
        );
        let out = cors.before(&mut r, &ctx()).unwrap();
        assert!(out.headers.get("access-control-allow-headers").is_none());
    }

    #[test]
    fn unknown_method_in_config_is_refused() {
        let c = cfg(&[
            ("http.cors.allow_origin", "*"),
            ("http.cors.allow_methods", "GET,TRACE"),
        ]);
        let e = Cors::from_config(&c).unwrap_err();
        assert!(e.to_string().contains("TRACE"), "{e}");
    }

    #[test]
    fn prefix_rule_matches_localhost_ports() {
        let c = cfg(&[("http.cors.allow_origin", "http://localhost:*")]);
        let cors = Cors::from_config(&c).unwrap().unwrap();
        let r = req(Method::Get, &[("origin", "http://localhost:5173")]);
        let out = cors.after(&r, &ctx(), Response::text("x"));
        assert_eq!(out.headers.get("access-control-allow-origin"), Some("http://localhost:5173"));
    }

    fn ctx() -> Context {
        Context::new(Config::new(), vec![], None, crate::config::Environment::Dev)
    }
}
