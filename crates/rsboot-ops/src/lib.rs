//! rsboot 的运维端点。
//!
//! 和 Spring Boot Actuator 的最小集合对齐——`/healthz`（进程活着）、
//! `/readyz`（依赖可用）、`/metrics`（Prometheus 文本）。三者都是普通
//! handler，挂到路由上即可：
//!
//! ```ignore
//! use rsboot_ops as ops;
//!
//! ops::Metrics::install();          // 想采指标才装，不装则核心零开销
//! let r = Router::new()
//!     .get("/healthz", |_, _| ops::healthz())?
//!     .get("/readyz", |_, ctx: &Context| ops::readyz(ctx))?
//!     .get("/metrics", |_, _| ops::Metrics::scrape())?;
//! ```

use rsboot::web::code;
use rsboot::web::envelope;
use rsboot::web::response::Response;
use rsboot::{Context, Status};

/// 进程存活：不碰任何依赖，永远只回 200。
pub fn healthz() -> Response {
    let data = serde_json::json!({ "status": "up", "version": rsboot::VERSION });
    match envelope::serialize(200, "ok", Some(&data), None) {
        Ok(bytes) => Response::json_owned(bytes, Status::OK),
        Err(_) => Response::new(Status::INTERNAL_SERVER_ERROR),
    }
}

/// 就绪：逐个探数据库，有一个不通就 `503` + `5030`，并列出每个库的状态。
pub fn readyz(ctx: &Context) -> Response {
    let reports = ctx.health();
    let ready = reports.iter().all(|r| r.ok);
    let data = serde_json::json!({
        "status": if ready { "ready" } else { "not_ready" },
        "databases": reports.iter().map(|r| serde_json::json!({
            "name": r.name,
            "ok": r.ok,
            "elapsed_ms": r.elapsed.as_millis() as u64,
            "message": r.message,
        })).collect::<Vec<_>>(),
    });
    if ready {
        match envelope::serialize(200, "ready", Some(&data), None) {
            Ok(bytes) => Response::json_owned(bytes, Status::OK),
            Err(_) => Response::new(Status::INTERNAL_SERVER_ERROR),
        }
    } else {
        match envelope::serialize(code::NOT_READY, "not ready", Some(&data), None) {
            Ok(bytes) => Response::json_owned(bytes, Status(503)),
            Err(_) => Response::new(Status(503)),
        }
    }
}

/// 请求计数器 + `/metrics` 输出。
pub struct Metrics;

impl Metrics {
    /// 装到核心的统计钩子上。装上之后每请求多两次 `rdtsc`（几十个周期）。
    pub fn install() {
        rsboot::web::stats::install(observe);
    }

    /// Prometheus 文本。
    pub fn render() -> String {
        use std::sync::atomic::Ordering;
        let total = TOTAL.load(Ordering::Relaxed).max(1);
        let mut s = String::with_capacity(320);
        s.push_str("# TYPE rsboot_requests_total counter\n");
        s.push_str(&format!("rsboot_requests_total {total}\n"));
        s.push_str("# TYPE rsboot_responses_total counter\n");
        for (label, v) in [
            ("2xx", S2.load(Ordering::Relaxed)),
            ("3xx", S3.load(Ordering::Relaxed)),
            ("4xx", S4.load(Ordering::Relaxed)),
            ("5xx", S5.load(Ordering::Relaxed)),
        ] {
            s.push_str(&format!("rsboot_responses_total{{class=\"{label}\"}} {v}\n"));
        }
        let sum = SUM_NS.load(Ordering::Relaxed);
        s.push_str("# TYPE rsboot_request_duration_seconds summary\n");
        s.push_str(&format!(
            "rsboot_request_duration_seconds_sum {}\n",
            sum as f64 / 1e9
        ));
        s.push_str(&format!("rsboot_request_duration_seconds_count {total}\n"));
        s.push_str(&format!(
            "rsboot_request_duration_seconds_max {}\n",
            MAX_NS.load(Ordering::Relaxed) as f64 / 1e9
        ));
        s
    }

    /// 采集端点（`text/plain; version=0.0.4`）。
    pub fn scrape() -> Response {
        Response::text(Self::render())
            .with_header("content-type", "text/plain; version=0.0.4; charset=utf-8")
    }
}

mod counters {
    use std::sync::atomic::AtomicU64;

    pub static TOTAL: AtomicU64 = AtomicU64::new(0);
    pub static S2: AtomicU64 = AtomicU64::new(0);
    pub static S3: AtomicU64 = AtomicU64::new(0);
    pub static S4: AtomicU64 = AtomicU64::new(0);
    pub static S5: AtomicU64 = AtomicU64::new(0);
    pub static SUM_NS: AtomicU64 = AtomicU64::new(0);
    pub static MAX_NS: AtomicU64 = AtomicU64::new(0);

    pub fn observe(status: u16, ns: u64) {
        use std::sync::atomic::Ordering as O;
        TOTAL.fetch_add(1, O::Relaxed);
        let bucket = match status {
            200..=299 => &S2,
            300..=399 => &S3,
            400..=499 => &S4,
            _ => &S5,
        };
        bucket.fetch_add(1, O::Relaxed);
        SUM_NS.fetch_add(ns, O::Relaxed);
        MAX_NS.fetch_max(ns, O::Relaxed);
    }
}

use counters::*;

#[cfg(test)]
mod tests {
    use super::*;
    use rsboot::web::response::IntoResponse;

    #[test]
    fn healthz_is_200_and_carries_version() {
        let r = healthz();
        assert_eq!(r.status, Status::OK);
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["code"], 200);
        assert_eq!(v["data"]["status"], "up");
        assert_eq!(v["data"]["version"], rsboot::VERSION);
    }

    #[test]
    fn readyz_shapes_without_panicking() {
        let ctx = Context::new(
            rsboot::Config::new(),
            Vec::new(),
            None,
            rsboot::config::Environment::Dev,
        );
        let r = readyz(&ctx);
        assert!(r.status == Status::OK || r.status.0 == 503);
        let _ = r.into_response();
    }

    #[test]
    fn metrics_render_has_the_text_format() {
        Metrics::install();
        observe(200, 1000);
        let text = Metrics::render();
        assert!(text.contains("rsboot_requests_total"));
        assert!(text.contains("class=\"2xx\""));
        assert!(text.contains("rsboot_request_duration_seconds_sum"));
        let r = Metrics::scrape();
        assert_eq!(
            r.headers.get("content-type"),
            Some("text/plain; version=0.0.4; charset=utf-8")
        );
    }
}
