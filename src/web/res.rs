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
//! let ok = Res::success("ok");          // {"code":200,"message":"ok"}
//! let list = Res::data(items);          // {"code":200,"message":"success","data":[...]}
//! let bad = Res::fail("库存不足");       // 400 + {"code":400,"message":"库存不足"}
//! # std::hint::black_box((ok, list, bad));
//! # }
//! ```
//!
//! handler 里可以直接 return，不用 `Ok(...)`：
//!
//! ```ignore
//! fn take(_: &mut Request, _: &Context) -> Response {
//!     if out_of_stock { return Res::fail("库存不足"); }
//!     Res::success("ok")
//! }
//! ```
//!
//! 需要 `?` 传播或 `-> Result<..>` 签名时，用 `err` 系列，它返回
//! [`ApiError`]，能放进 `Err(...)`：
//!
//! ```ignore
//! fn detail(_: &mut Request, _: &Context) -> Result<Response, ApiError> {
//!     if miss { return Err(Res::err("没有这条记录")); }
//!     Ok(Res::data(row))
//! }
//! ```
//!
//! 两组都带 `always_wrap` 标记，出口处无条件按统一形状输出——用户显式要了
//! 这个形状，就不该再被 `http.envelope` 的全局开关改写一次。

use serde::Serialize;

use crate::web::envelope;
use crate::web::error::ApiError;
use crate::web::http::Status;
use crate::web::response::Response;

/// 统一响应的命名空间。用关联函数：`Res::success(...)`、`Res::data(...)`、`Res::fail(...)`；
/// `-> Result<..>` 的 handler 用 `Res::err(...)`。
pub struct Res;

impl Res {
    /// 成功，只有提示语：`{"code":200,"message":"<你给的文案>"}`（不写 `data` 字段）
    pub fn success(message: impl Into<String>) -> Response {
        let msg = message.into();
        match envelope::serialize(200, &msg, None::<&()>, None) {
            Ok(bytes) => Response::json_owned(bytes, Status::OK),
            Err(_) => encode_failed(),
        }
    }

    /// 成功 + 数据：`{"code":200,"message":"success","data":<序列化结果>}`
    ///
    /// `data` 直接流式写进同一个缓冲，不建 `serde_json::Value` 中间树。
    pub fn data<T: Serialize>(value: T) -> Response {
        match envelope::serialize(200, "success", Some(&value), None) {
            Ok(bytes) => Response::json_owned(bytes, Status::OK),
            Err(_) => encode_failed(),
        }
    }

    /// 失败，默认 `400`：`{"code":400,"message":"<你给的文案>"}`
    ///
    /// 直接返回 `Response`，所以 `-> Response` 签名的 handler 一行就能返回。
    pub fn fail(message: impl Into<String>) -> Response {
        Self::fail_with(Status::BAD_REQUEST, message)
    }

    /// 带状态码的失败响应。`code` 由状态码推导，与 `envelope::error` 同一套映射。
    pub fn fail_with(status: Status, message: impl Into<String>) -> Response {
        let err = Self::err_with(status, message);
        envelope::error(&err, None, true)
    }

    /// 失败，返回 [`ApiError`]：给 `-> Result<..>` 签名、要用 `Err(...)` 或 `?` 的 handler。
    pub fn err(message: impl Into<String>) -> ApiError {
        Self::err_with(Status::BAD_REQUEST, message)
    }

    /// 带状态码的版本，同上。
    pub fn err_with(status: Status, message: impl Into<String>) -> ApiError {
        let mut err = ApiError::new(status, message);
        err.always_wrapped();
        err
    }
}

/// 序列化本身失败（只有内存耗尽这类极端情况），给最小可用的 500。
fn encode_failed() -> Response {
    Response::json_owned(
        br#"{"code":500,"message":"internal error"}"#.to_vec(),
        Status::INTERNAL_SERVER_ERROR,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_has_no_data_field() {
        let r = Res::success("ok");
        assert_eq!(r.status, Status::OK);
        assert_eq!(&r.body[..], br#"{"code":200,"message":"ok"}"#);
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
            br#"{"code":200,"message":"success","data":{"n":1,"ok":true}}"#
        );
    }

    #[test]
    fn err_is_400_and_wrapped() {
        let e = Res::err("库存不足");
        assert_eq!(e.status, Status::BAD_REQUEST);
        assert!(e.always_wrap);
    }

    #[test]
    fn fail_is_400_and_wrapped_body() {
        let r = Res::fail("库存不足");
        assert_eq!(r.status, Status::BAD_REQUEST);
        assert_eq!(
            &r.body[..],
            "{\"code\":400,\"message\":\"库存不足\"}".as_bytes()
        );
    }

    #[test]
    fn err_with_keeps_status() {
        let e = Res::err_with(Status::CONFLICT, "已经存在");
        assert_eq!(e.status, Status::CONFLICT);
    }

    #[test]
    fn fail_with_keeps_status() {
        let r = Res::fail_with(Status::CONFLICT, "已经存在");
        assert_eq!(r.status, Status::CONFLICT);
    }
}
