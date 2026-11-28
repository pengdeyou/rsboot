//! `application/x-www-form-urlencoded` 表单提取器。
//!
//! 和 `Query` 共用同一套解析（`serde_urlencoded`），区别只是数据来自 body。
//! 解析失败按参数校验错误返回：`422` + `4220`，`loc` 指向 `body`。
//!
//! ```ignore
//! fn login(Form(f): Form<Login>) -> Response { ... }
//! ```

use rsboot::web::code;
use rsboot::web::error::ApiError;
use rsboot::web::extract::FromRequest;
use rsboot::Request;
use serde::de::DeserializeOwned;

/// 表单 body。
#[derive(Debug, Clone)]
pub struct Form<T>(pub T);

impl<T> Form<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T: DeserializeOwned> FromRequest for Form<T> {
    #[allow(clippy::result_large_err)]
    fn from_request(req: &mut Request) -> Result<Self, ApiError> {
        let is_form = req
            .header("content-type")
            .map(|v| v.starts_with("application/x-www-form-urlencoded"))
            .unwrap_or(false);
        if !is_form {
            return Err(ApiError::with_code(
                code::VALIDATION,
                "表单请求的 content-type 必须是 application/x-www-form-urlencoded",
            )
            .at("body", ""));
        }
        let body = req.take_body();
        serde_urlencoded::from_bytes::<T>(&body)
            .map(Form)
            .map_err(|e| {
                ApiError::with_code(code::VALIDATION, format!("表单解析失败：{e}")).at("body", "")
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsboot::{Method, Status};

    #[derive(serde::Deserialize, Debug)]
    struct Login {
        user: String,
        age: u8,
    }

    #[test]
    fn parses_typed_fields() {
        let mut r = Request::new(Method::Post, "/login")
            .with_header("content-type", "application/x-www-form-urlencoded")
            .with_body("user=%E4%B8%89&age=7");
        let Form(login) = Form::<Login>::from_request(&mut r).unwrap();
        assert_eq!(login.user, "三");
        assert_eq!(login.age, 7);
    }

    #[test]
    fn wrong_content_type_is_4220() {
        let mut r = Request::new(Method::Post, "/login")
            .with_header("content-type", "application/json")
            .with_body("{}");
        let e = Form::<Login>::from_request(&mut r).unwrap_err();
        assert_eq!(e.status, Status::UNPROCESSABLE_ENTITY);
        assert_eq!(e.code, code::VALIDATION);
    }
}
