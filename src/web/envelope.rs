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
//! 成功：{"code":200,"message":"success","data":{...},"trace_id":"..."}
//! 失败：{"code":400,"message":"...","trace_id":"..."}
//! ```
//!
//! `code` 就是 HTTP 状态码数字。语义本来就由状态码承载，body 里再发明一套
//! 字符串码只会让两套码打架（`401` 到底是"未登录"还是"令牌过期"？）。
//!
//! **没有数据就不写 `data` 字段**，而不是写 `null`。只返回一句提示时，
//! `{"code":200,"message":"ok"}` 就够了——客户端判断 `data` 是否存在，
//! 比判断 `data is null` 少一层分支。
//!
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
        match serialize(200, "success", Some(&self.0), req.request_id()) {
            Ok(bytes) => Response::json_owned(bytes, Status::OK),
            Err(e) => ApiError::internal(format!("响应序列化失败：{e}")).into_response(),
        }
    }
}

impl<T: Serialize> IntoResponse for Api<T> {
    fn into_response(self) -> Response {
        match serialize(200, "success", Some(&self.0), None) {
            Ok(bytes) => Response::json_owned(bytes, Status::OK),
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
    let code = err.code;

    // 只有校验失败才需要 `fields` 这棵树（本身是罕见路径，建树无妨）。
    // 其余错误 `data` 就是 null，直接写字面量，不经过任何中间结构。
    if let (Some((where_, name)), Some(kind)) = (&err.loc, &err.kind) {
        let loc = if name.is_empty() {
            serde_json::json!([where_])
        } else {
            serde_json::json!([where_, name])
        };
        let data = serde_json::json!({
            "fields": [{
                "loc": loc,
                "msg": err.detail,
                "type": kind,
            }]
        });
        return match serialize(code, "参数校验失败", Some(&data), trace) {
            Ok(bytes) => Response::json_owned(bytes, err.status),
            Err(_) => fallback_500(),
        };
    }

    // `ApiError::internal` 已经把真因挪进日志、`detail` 是通用文案，
    // 所以这里可以直接用 `detail`，不会再泄漏 SQL 或内部路径。
    match serialize(code, &err.detail, None::<&serde_json::Value>, trace) {
        Ok(bytes) => Response::json_owned(bytes, err.status),
        Err(_) => fallback_500(),
    }
}

/// 连错误体都序列化不了，说明是极端环境（内存耗尽等），给最小可用的 500。
fn fallback_500() -> Response {
    Response::json_owned(
        br#"{"code":500,"message":"internal error"}"#.to_vec(),
        Status::INTERNAL_SERVER_ERROR,
    )
}

/// 逐段把 envelope 写进一个 buffer，**不建 `serde_json::Value` 中间树**。
///
/// 原实现是 `to_value(&data)` → `json!({...})` 建第二棵树 → `to_vec` →
/// `Response::json_with` 再 `to_vec()` 一次：一个响应最多四次分配、三次遍历。
/// 现在只分配一次，字段顺序与旧实现逐字节一致（`code` → `message` →
/// `data` → `trace_id`）。
pub(crate) fn serialize<D: Serialize>(
    code: u32,
    message: &str,
    data: Option<&D>,
    trace: Option<&str>,
) -> serde_json::Result<Vec<u8>> {
    // 128 覆盖绝大多数 envelope，避免小响应触发扩容
    let mut buf = Vec::with_capacity(128);
    buf.push(b'{');
    buf.extend_from_slice(br#""code":"#);
    push_u32(&mut buf, code);
    buf.extend_from_slice(br#","message":"#);
    serde_json::to_writer(&mut buf, message)?;
    if let Some(d) = data {
        buf.extend_from_slice(br#","data":"#);
        serde_json::to_writer(&mut buf, d)?;
    }
    // 没有 trace_id 时不写这个字段，而不是写 null：
    // 客户端"_ 缺失"比"_ 是 null"更容易判断
    if let Some(t) = trace {
        buf.extend_from_slice(br#","trace_id":"#);
        serde_json::to_writer(&mut buf, t)?;
    }
    buf.push(b'}');
    Ok(buf)
}

/// 手写整数写入：状态码只有 3 位，比走 `serde_json` 的整数路径少一层。
fn push_u32(buf: &mut Vec<u8>, mut n: u32) {
    if n == 0 {
        buf.push(b'0');
        return;
    }
    let mut digits = [0u8; 10];
    let mut i = digits.len();
    while n > 0 {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    buf.extend_from_slice(&digits[i..]);
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
        assert_eq!(v["code"], 200);
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
        assert_eq!(v["code"], 4040);
        assert_eq!(v["message"], "没有这个用户");
        assert_eq!(v["trace_id"], "t1");
    }

    #[test]
    fn validation_error_keeps_fields_in_data() {
        let e = ApiError::validation("value", "query", "limit", "应大于 0");
        let r = error(&e, None, true);
        assert_eq!(r.status, Status::UNPROCESSABLE_ENTITY);
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["code"], 4220);
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
