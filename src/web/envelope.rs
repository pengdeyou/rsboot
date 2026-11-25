//! 统一响应体（envelope）。
//!
//! ## 四条硬规则
//!
//! **默认关**。裸 JSON 是 REST 的通用语言，第三方客户端、网关、SDK 都按它写；
//! 包装是内部约定，应该显式开启，而不是默认强加。
//!
//! **开了就全站一致，包括错误**。绝不能出现"成功带 `data`、失败带 `detail`"，
//! 那会逼客户端写两套解析，半包装比不包装更难用。
//!
//! **HTTP 状态码照常表达语义**。绝不"全部 200 + body 里 code 非 0"，
//! 后者会毁掉网关重试、监控告警、缓存与浏览器行为。
//!
//! **`trace_id` 与日志同源**（就是 `X-Request-Id`），报障时能直接拿它搜日志。
//!
//! ## 形状
//!
//! ```text
//! 成功：{"code":"OK","message":"success","data":{...},"trace_id":"..."}
//! 失败：{"code":"VALIDATION_ERROR","message":"...","data":null,"trace_id":"..."}
//! ```
//!
//! `code` 用字符串而不是数字：数字码是历史包袱，可读性和可扩展性都差。
//! 校验失败时 `data` 里带 `fields`，保留 FastAPI 那样的 `loc/msg/type`，
//! 信息一点不丢——包装不该以丢失诊断能力为代价。

use serde::Serialize;

use crate::web::error::ApiError;
use crate::web::http::Status;
use crate::web::request::Request;
use crate::web::response::{IntoResponse, Response};

/// 开关。`http.envelope = true` 时开启。默认关。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Envelope {
    pub on: bool,
}

impl Envelope {
    pub fn on() -> Self {
        Self { on: true }
    }

    pub fn off() -> Self {
        Self { on: false }
    }

    pub fn from_config(config: &crate::config::Config) -> Self {
        Self {
            on: config.get_bool("http.envelope").ok().flatten().unwrap_or(false),
        }
    }
}

/// 成功响应包装。`Api(data)` 在 handler 里返回即可。
pub struct Api<T>(pub T);

impl<T> Api<T> {
    /// 带 trace_id 的完整包装。handler 里能拿到 `&Request` 时优先用它。
    pub fn respond(self, req: &Request) -> Response
    where
        T: Serialize,
    {
        match serde_json::to_vec(&body("OK", "success", serde_json::to_value(&self.0).ok(), req.request_id())) {
            Ok(bytes) => Response::json_with(&bytes, Status::OK),
            Err(e) => ApiError::internal(format!("响应序列化失败：{e}")).into_response(),
        }
    }
}

impl<T: Serialize> IntoResponse for Api<T> {
    fn into_response(self) -> Response {
        let data = serde_json::to_value(&self.0).ok();
        match serde_json::to_vec(&body("OK", "success", data, None)) {
            Ok(bytes) => Response::json_with(&bytes, Status::OK),
            Err(e) => ApiError::internal(format!("响应序列化失败：{e}")).into_response(),
        }
    }
}

/// 统一的错误响应。
///
/// `on = false` 时退回裸形状（与 `ApiError::to_response()` 一致），
/// 这样开关只影响一处，不会两边各有各的实现。
pub fn error(err: &ApiError, req: Option<&Request>, on: bool) -> Response {
    if !on {
        return err.to_response();
    }
    let trace = req.and_then(|r| r.request_id());
    let code = code_of(err.status);
    let data = match (&err.loc, &err.kind) {
        (Some((where_, name)), Some(kind)) => {
            let loc = if name.is_empty() {
                serde_json::json!([where_])
            } else {
                serde_json::json!([where_, name])
            };
            Some(serde_json::json!({
                "fields": [{
                    "loc": loc,
                    "msg": err.detail,
                    "type": kind,
                }]
            }))
        }
        _ => None,
    };
    let message = if data.is_some() {
        "参数校验失败".to_string()
    } else {
        err.detail.clone()
    };
    match serde_json::to_vec(&body(code, &message, data, trace)) {
        Ok(bytes) => Response::json_with(&bytes, err.status),
        // 连错误体都序列化不了，说明是极端环境（内存耗尽等），给最小可用的 500
        Err(_) => Response::json_with(br#"{"code":"INTERNAL_ERROR","message":"internal error","data":null}"#, Status::INTERNAL_SERVER_ERROR),
    }
}

/// 状态码 → 业务码。刻意**不用** HTTP 状态码当业务码：
/// 两者含义不同（HTTP 说"请求怎么了"，业务码说"领域里发生了什么"）。
fn code_of(status: Status) -> &'static str {
    match status.0 {
        400 => "BAD_REQUEST",
        401 => "UNAUTHORIZED",
        403 => "FORBIDDEN",
        404 => "NOT_FOUND",
        405 => "METHOD_NOT_ALLOWED",
        409 => "CONFLICT",
        413 => "PAYLOAD_TOO_LARGE",
        422 => "VALIDATION_ERROR",
        429 => "TOO_MANY_REQUESTS",
        s if s >= 500 => "INTERNAL_ERROR",
        _ => "ERROR",
    }
}

fn body(
    code: &str,
    message: &str,
    data: Option<serde_json::Value>,
    trace_id: Option<&str>,
) -> serde_json::Value {
    let mut o = serde_json::json!({
        "code": code,
        "message": message,
        "data": data.unwrap_or(serde_json::Value::Null),
    });
    // 没有 trace_id 时不写这个字段，而不是写 null：
    // 客户端"_ 缺失"比"_ 是 null"更容易判断
    if let Some(t) = trace_id {
        o["trace_id"] = serde_json::json!(t);
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::http::Method;

    fn req(id: Option<&str>) -> Request {
        let mut r = Request::new(Method::Get, "/x");
        if let Some(i) = id {
            r = r.with_header("x-request-id", i);
        }
        r
    }

    #[test]
    fn success_shape_has_code_message_data() {
        let r = Api(serde_json::json!({"id": 7})).respond(&req(Some("abc")));
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["code"], "OK");
        assert_eq!(v["message"], "success");
        assert_eq!(v["data"]["id"], 7);
        assert_eq!(v["trace_id"], "abc");
        assert_eq!(r.status, Status::OK);
    }

    #[test]
    fn no_trace_id_means_no_field() {
        let r = Api(1).respond(&req(None));
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert!(v.get("trace_id").is_none(), "缺失而不是 null");
    }

    #[test]
    fn error_keeps_http_status_and_adds_code() {
        let e = ApiError::not_found("没有这个用户");
        let r = error(&e, Some(&req(Some("t1"))), true);
        assert_eq!(r.status, Status::NOT_FOUND, "状态码必须保留");
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["code"], "NOT_FOUND");
        assert_eq!(v["message"], "没有这个用户");
        assert_eq!(v["trace_id"], "t1");
    }

    #[test]
    fn validation_error_keeps_fields_in_data() {
        let e = ApiError::validation("value", "query", "limit", "应大于 0");
        let r = error(&e, None, true);
        assert_eq!(r.status, Status::UNPROCESSABLE_ENTITY);
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["code"], "VALIDATION_ERROR");
        assert_eq!(v["data"]["fields"][0]["loc"][0], "query");
        assert_eq!(v["data"]["fields"][0]["loc"][1], "limit");
    }

    #[test]
    fn internal_error_does_not_leak_detail_in_envelope() {
        let e = ApiError::internal("table users has no column nmae");
        let r = error(&e, None, true);
        let body = String::from_utf8(r.body).unwrap();
        assert!(!body.contains("nmae"), "真因不能进响应：{body}");
        assert_eq!(r.status, Status::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn switching_off_returns_bare_shape() {
        let e = ApiError::not_found("x");
        let r = error(&e, None, false);
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["detail"], "x", "关掉时应与 to_response 一致");
        assert!(v.get("code").is_none());
    }

    #[test]
    fn config_switch_defaults_off() {
        assert!(!Envelope::from_config(&crate::config::Config::new()).on);
        let c = crate::config::Config::new().set("http.envelope", "true");
        assert!(Envelope::from_config(&c).on);
    }
}
