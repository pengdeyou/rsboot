//! 一行式统一响应。
//!
//! 形状固定为 `code` → `message` → `data` 三个字段，字段顺序与
//! [`envelope`](super::envelope) 逐字节一致——所以开着 `http.envelope` 时，
//! 这里的响应不会被二次包装。
//!
//! ```
//! use rsboot::web::res::Res;
//!
//! # fn demo(items: Vec<u32>) {
//! let ok = Res::success("ok");          // {"code":"OK","message":"ok","data":null}
//! let list = Res::data(items);          // {"code":"OK","message":"success","data":[...]}
//! let bad = Res::fail("库存不足");       // 400 + {"code":"BAD_REQUEST","message":"库存不足","data":null}
//! # std::hint::black_box((ok, list, bad));
//! # }
//! ```
//!
//! handler 里就是：
//!
//! ```ignore
//! fn list(_: &mut Request, _: &Context) -> Result<Response, ApiError> {
//!     Ok(Res::data(items))
//! }
//!
//! fn take(_: &mut Request, _: &Context) -> Result<Response, ApiError> {
//!     if out_of_stock { return Err(Res::fail("库存不足")); }
//!     Ok(Res::success("ok"))
//! }
//! ```
//!
//! ## 为什么 `fail` 返回 `ApiError` 而不是 `Response`
//!
//! 因为它要能直接放进 `Err(...)`：`return Err(Res::fail("库存不足"))`。
//! 它带 `always_wrap` 标记，出口处无条件按统一形状输出——用户显式要了
//! 这个形状，就不该再被 `http.envelope` 的全局开关改写一次。

use serde::Serialize;

use crate::web::envelope;
use crate::web::error::ApiError;
use crate::web::http::Status;
use crate::web::response::Response;

/// 统一响应的命名空间。用关联函数：`Res::success(...)`、`Res::data(...)`、`Res::fail(...)`。
pub struct Res;

impl Res {
    /// 成功，`data` 为 `null`：`{"code":"OK","message":"<你给的文案>","data":null}`
    pub fn success(message: impl Into<String>) -> Response {
        let msg = message.into();
        match envelope::serialize("OK", &msg, None::<&()>, None) {
            Ok(bytes) => Response::json_owned(bytes, Status::OK),
            Err(_) => encode_failed(),
        }
    }

    /// 成功 + 数据：`{"code":"OK","message":"success","data":<序列化结果>}`
    ///
    /// `data` 直接流式写进同一个缓冲，不建 `serde_json::Value` 中间树。
    pub fn data<T: Serialize>(value: T) -> Response {
        match envelope::serialize("OK", "success", Some(&value), None) {
            Ok(bytes) => Response::json_owned(bytes, Status::OK),
            Err(_) => encode_failed(),
        }
    }

    /// 失败，默认 `400`：`{"code":"BAD_REQUEST","message":"<你给的文案>","data":null}`
    pub fn fail(message: impl Into<String>) -> ApiError {
        Self::fail_with(Status::BAD_REQUEST, message)
    }

    /// 指定状态码的失败。`code` 由状态码推导，与 `envelope::error` 同一套映射。
    pub fn fail_with(status: Status, message: impl Into<String>) -> ApiError {
        let mut err = ApiError::new(status, message);
        err.always_wrapped();
        err
    }
}

/// 序列化本身失败（只有内存耗尽这类极端情况），给最小可用的 500。
fn encode_failed() -> Response {
    Response::json_owned(
        br#"{"code":"INTERNAL_ERROR","message":"internal error","data":null}"#.to_vec(),
        Status::INTERNAL_SERVER_ERROR,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_has_null_data() {
        let r = Res::success("ok");
        assert_eq!(r.status, Status::OK);
        assert_eq!(&r.body[..], br#"{"code":"OK","message":"ok","data":null}"#);
        assert_eq!(
            r.headers.get("content-type"),
            Some("application/json; charset=utf-8")
        );
    }

    #[test]
    fn data_serialises_inline() {
        let r = Res::data(serde_json::json!({"n": 1, "ok": true}));
        assert_eq!(
            &r.body[..],
            br#"{"code":"OK","message":"success","data":{"n":1,"ok":true}}"#
        );
    }

    #[test]
    fn fail_is_400_and_wrapped() {
        let e = Res::fail("库存不足");
        assert_eq!(e.status, Status::BAD_REQUEST);
        assert!(e.always_wrap);
    }

    #[test]
    fn fail_with_keeps_status() {
        let e = Res::fail_with(Status::CONFLICT, "已经存在");
        assert_eq!(e.status, Status::CONFLICT);
    }
}
