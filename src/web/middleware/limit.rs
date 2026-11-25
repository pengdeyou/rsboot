//! 按来源限流（令牌桶）。
//!
//! ## 为什么是令牌桶，不是"每秒计数器"
//!
//! 固定窗口计数器在窗口边界会放过双倍流量（59.9s 打满 + 60.1s 再打满）。
//! 令牌桶能同时表达"平均速率"和"允许突发"两件事：`rate` 决定长期平均，
//! `burst` 决定瞬间能攒多少。
//!
//! ## 计数器的内存必须有上限
//!
//! `HashMap<IpAddr, Bucket>` 在公网服务里等于一个可以被灌爆的内存洞
//! （换 IP 就能新增一个桶）。所以：
//!
//! - `max_keys` 硬上限；
//! - 超过上限时**先清理已满桶**（令牌已攒满的桶等价于"没在限流"，删掉不丢信息），
//!   仍然超就对新来源直接放行——限流的目的是保护服务，不是把自己变成 DoS 目标。
//!
//! ## 只做单机
//!
//! 多实例部署时这里的桶是各自的，全局限流要放网关或 Redis。
//! 这一点写清楚，避免"配了限流所以安全了"的错觉。
//!
//! ## key 的选择
//!
//! 取 `peer` 的 IP 部分，**不信任** `x-forwarded-for`（可伪造，
//! 除非部署在可信代理后面并显式配置）。想按 XFF 限流就自己写一个中间件。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Instant;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::runtime::context::Context;
use crate::web::http::Status;
use crate::web::middleware::Middleware;
use crate::web::request::Request;
use crate::web::response::Response;

struct Bucket {
    tokens: f64,
    last: Instant,
}

impl std::fmt::Debug for RateLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimit")
            .field("rate", &self.rate)
            .field("burst", &self.burst)
            .field("max_keys", &self.max_keys)
            .field("keys", &self.buckets.lock().map(|m| m.len()).unwrap_or(0))
            .finish()
    }
}

pub struct RateLimit {
    /// 每秒补充多少令牌（长期平均速率）
    rate: f64,
    /// 桶容量（允许的突发）
    burst: f64,
    max_keys: usize,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimit {
    pub fn new(rate: f64, burst: f64) -> Self {
        Self {
            rate,
            burst,
            max_keys: 8192,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// 配置项：`http.rate.rate`（每秒）、`http.rate.burst`（突发）。
    /// 两个都不给就不装。
    pub fn from_config(config: &Config) -> Result<Option<Self>> {
        let Some(raw_rate) = config.get("http.rate.rate") else {
            return Ok(None);
        };
        let rate: f64 = raw_rate.parse().map_err(|_| {
            Error::Config(format!("`http.rate.rate` 必须是数字，当前 `{raw_rate}`"))
        })?;
        if rate <= 0.0 {
            return Err(Error::Config(
                "`http.rate.rate` 必须大于 0（想关掉限流就删掉这个键）".into(),
            ));
        }
        let burst = config
            .get("http.rate.burst")
            .map(|s| {
                s.parse::<f64>().map_err(|_| {
                    Error::Config(format!("`http.rate.burst` 必须是数字，当前 `{s}`"))
                })
            })
            .transpose()?
            .unwrap_or(rate);
        if burst < 1.0 {
            return Err(Error::Config(format!(
                "`http.rate.burst` 至少为 1，当前 {burst}"
            )));
        }
        let max_keys = config
            .get_int("http.rate.max_keys")
            .map_err(|e| Error::Config(e.to_string()))?
            .unwrap_or(8192);
        if max_keys <= 0 {
            return Err(Error::Config(format!(
                "`http.rate.max_keys` 必须是正数，当前 {max_keys}"
            )));
        }
        Ok(Some(Self {
            rate,
            burst,
            max_keys: max_keys as usize,
            buckets: Mutex::new(HashMap::new()),
        }))
    }

    fn key_of(req: &Request) -> String {
        match &req.peer {
            Some(p) => match p.parse::<std::net::SocketAddr>() {
                Ok(sa) => sa.ip().to_string(),
                // IPv6 的 `[::1]:1234` 也在这里被处理；再不行就退回原文
                Err(_) => match p.rsplit_once(':') {
                    Some((ip, _)) if ip.parse::<IpAddr>().is_ok() => ip.to_string(),
                    _ => p.clone(),
                },
            },
            None => "unknown".to_string(),
        }
    }
}

impl Middleware for RateLimit {
    fn name(&self) -> &'static str {
        "rate"
    }

    fn before(&self, req: &mut Request, _ctx: &Context) -> Option<Response> {
        let key = Self::key_of(req);
        let now = Instant::now();

        let mut map = match self.buckets.lock() {
            Ok(m) => m,
            // 锁被毒化（上一个持锁线程 panic）不该让服务挂掉：放行，并让日志看得见
            Err(_) => {
                eprintln!("[rsboot] 限流器锁被毒化，本次请求放行");
                return None;
            }
        };

        if !map.contains_key(&key) && map.len() >= self.max_keys {
            // 先扔掉已满桶：它们不再影响任何判定
            map.retain(|_, b| b.tokens < self.burst);
            if map.len() >= self.max_keys {
                eprintln!(
                    "[rsboot] 限流键已达上限 {}，新来源 {} 放行（限流是为了保护服务，不是变成 DoS）",
                    self.max_keys, key
                );
                return None;
            }
        }

        let bucket = map.entry(key.clone()).or_insert(Bucket {
            tokens: self.burst,
            last: now,
        });
        let elapsed = now.duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.rate).min(self.burst);
        bucket.last = now;

        if bucket.tokens < 1.0 {
            let wait = (1.0 - bucket.tokens) / self.rate;
            let retry = wait.ceil().max(1.0) as u64;
            drop(map);
            let mut resp = Response::new(Status(429));
            resp.headers.insert("retry-after", &retry.to_string());
            resp.headers.insert("content-type", "application/json; charset=utf-8");
            resp.body = format!(
                "{{\"detail\":\"请求过于频繁，请 {} 秒后重试\"}}",
                retry
            )
            .into_bytes();
            return Some(resp);
        }
        bucket.tokens -= 1.0;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Environment;

    fn ctx() -> Context {
        Context::new(Config::new(), vec![], None, Environment::Dev)
    }

    fn req_from(ip: &str) -> Request {
        let mut r = Request::new(crate::web::http::Method::Get, "/a");
        r.peer = Some(format!("{ip}:5555"));
        r
    }

    #[test]
    fn burst_then_429() {
        // burst=2：前两个放行，第三个立即 429
        let m = RateLimit::new(0.0 + 0.0001, 2.0);
        assert!(m.before(&mut req_from("1.2.3.4"), &ctx()).is_none());
        assert!(m.before(&mut req_from("1.2.3.4"), &ctx()).is_none());
        let r = m.before(&mut req_from("1.2.3.4"), &ctx()).unwrap();
        assert_eq!(r.status, Status(429));
        assert!(r.headers.get("retry-after").is_some());
    }

    #[test]
    fn different_ips_have_separate_buckets() {
        let m = RateLimit::new(0.0001, 1.0);
        assert!(m.before(&mut req_from("1.1.1.1"), &ctx()).is_none());
        assert!(m.before(&mut req_from("2.2.2.2"), &ctx()).is_none());
        assert!(m.before(&mut req_from("1.1.1.1"), &ctx()).is_some());
    }

    #[test]
    fn ipv6_peer_is_parsed() {
        // peer 是 `[::1]:5555` 这种带括号的 SocketAddr 原文
        let mut r = Request::new(crate::web::http::Method::Get, "/a");
        r.peer = Some("[::1]:5555".into());
        assert_eq!(RateLimit::key_of(&r), "::1");

        // 只剩 IP 没有端口（某些代理头会这么给），走 rsplit 兜底
        let mut r = Request::new(crate::web::http::Method::Get, "/a");
        r.peer = Some("::1".into());
        assert_eq!(RateLimit::key_of(&r), "::1");
    }

    #[test]
    fn config_absent_means_off() {
        assert!(RateLimit::from_config(&Config::new()).unwrap().is_none());
    }

    #[test]
    fn bad_config_is_refused() {
        let mut c = Config::new();
        c = c.set("http.rate.rate", "0");
        assert!(RateLimit::from_config(&c).unwrap_err().to_string().contains("大于 0"));
    }

    #[test]
    fn key_cap_does_not_explode_memory() {
        let mut m = RateLimit::new(0.5, 1.0);
        m.max_keys = 4;
        // 4 个桶都用满（tokens=0），第 5 个来源进不来时不该无限增长
        for ip in ["1.0.0.1", "1.0.0.2", "1.0.0.3", "1.0.0.4"] {
            assert!(m.before(&mut req_from(ip), &ctx()).is_none());
        }
        // 已满桶（消耗过）不会被 retain 清掉，所以这里应当直接放行而不新增
        assert!(m.before(&mut req_from("1.0.0.5"), &ctx()).is_none());
        assert_eq!(m.buckets.lock().unwrap().len(), 4);
    }
}
