//! JSON 提取与响应。
//!
//! 错误形状对齐 FastAPI 的 422：`detail` 是数组，`loc` 指到出错的字段。
//! 这里只给到 `["body"]` 一层——`serde_json` 的错误信息里已经带了字段路径
//! （`invalid type: string, expected u32 at line 1 column 12`），拆成结构化
//! 的 `loc` 需要自己写访问者，收益不抵成本。

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::web::error::ApiError;
use crate::web::extract::FromRequest;
use crate::web::request::Request;
use crate::web::response::{IntoResponse, Response};

/// 请求体按 JSON 解析 / 返回值按 JSON 序列化。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Json<T>(pub T);

impl<T> Json<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> FromRequest for Json<T>
where
    T: DeserializeOwned,
{
    #[allow(clippy::result_large_err)]
    fn from_request(req: &mut Request) -> Result<Self, ApiError> {
        let body = req.take_body();
        if body.is_empty() {
            // 明确的两种情况分开报：空体 vs 已被读过。
            // 混成一句"解析失败"会让排查卡很久。
            return Err(ApiError::validation(
                "missing",
                "body",
                "",
                "请求体是空的；如果是 `POST` 且没带 body，请检查客户端",
            ));
        }
        serde_json::from_slice(&body)
            .map(Json)
            .map_err(|e| ApiError::validation("json_invalid", "body", "", e.to_string()))
    }
}

impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        match serde_json::to_vec(&self.0) {
            Ok(bytes) => Response::json_with(&bytes, crate::web::http::Status::OK),
            // 序列化失败是服务端 bug（比如自定义 Serialize 里 panic），不是客户端问题
            Err(e) => ApiError::internal(format!("响应序列化失败：{e}")).into_response(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Body {
        name: String,
        age: u8,
    }

    fn post(body: &[u8]) -> Request {
        Request::new(crate::web::http::Method::Post, "/x").with_body(body.to_vec())
    }

    #[test]
    fn parses_valid_json() {
        let mut r = post(br#"{"name":"a","age":3}"#);
        let Json(b) = Json::<Body>::from_request(&mut r).unwrap();
        assert_eq!(b, Body { name: "a".into(), age: 3 });
    }

    #[test]
    fn invalid_json_is_422_with_loc_body() {
        let mut r = post(br#"{"name":"a","age":"x"}"#);
        let e = Json::<Body>::from_request(&mut r).unwrap_err();
        assert_eq!(e.status, crate::web::http::Status::UNPROCESSABLE_ENTITY);
        assert_eq!(e.loc, Some(("body".to_string(), String::new())));
    }

    #[test]
    fn empty_body_is_reported_separately() {
        let mut r = post(b"");
        let e = Json::<Body>::from_request(&mut r).unwrap_err();
        assert!(e.internal.unwrap_or_default().contains("空的"));
    }

    #[test]
    fn response_sets_json_content_type() {
        let resp = Json(serde_json::json!({"ok": true})).into_response();
        assert_eq!(resp.status, crate::web::http::Status::OK);
        assert_eq!(
            resp.headers.get("content-type"),
            Some("application/json; charset=utf-8")
        );
    }
}
