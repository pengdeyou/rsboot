//! 错误响应。形状刻意对齐 FastAPI，客户端代码可以直接复用。
//!
//! - 校验失败：`422` + `{"detail":[{"loc":["query","limit"],"msg":"...","type":"..."}]}`
//! - 其他错误：`{"detail":"..."}`
//!
//! ## 内部错误不泄露细节
//!
//! `5xx` 的 `detail` 永远是笼统的一句话，真实原因只出现在服务端日志。
//! 把 SQL 错误文本回给客户端是很常见的泄露来源（表名、列名、甚至数据值）。
//! 客户端拿到的是 `request_id`，用它在日志里定位。

use std::fmt;

use crate::web::http::Status;
use crate::web::response::Response;

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: Status,
    /// 面向客户端的说明
    pub detail: String,
    /// `422` 时的位置：`("path", "id")` / `("query", "limit")` / `("body", "name")`
    pub loc: Option<(String, String)>,
    /// 校验失败的类型标签（对齐 FastAPI 的 `type` 字段）
    pub kind: Option<String>,
    /// 服务端日志用的真实原因；不回给客户端
    pub internal: Option<String>,
    /// 请求 id，便于在日志里定位
    pub request_id: Option<u64>,
}

impl ApiError {
    pub fn new(status: Status, detail: impl Into<String>) -> Self {
        Self {
            status,
            detail: detail.into(),
            loc: None,
            kind: None,
            internal: None,
            request_id: None,
        }
    }

    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self::new(Status::BAD_REQUEST, detail)
    }

    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(Status::NOT_FOUND, detail)
    }

    pub fn unauthorized(detail: impl Into<String>) -> Self {
        Self::new(Status::UNAUTHORIZED, detail)
    }

    /// 校验失败：缺参数、类型不对、body 解析不了。
    pub fn validation(kind: &str, where_: &str, name: &str, msg: impl Into<String>) -> Self {
        Self {
            status: Status::UNPROCESSABLE_ENTITY,
            // 客户端看到的 msg 与 loc 组合起来就能直接定位字段
            detail: format!("{where_}.{name}"),
            loc: Some((where_.to_string(), name.to_string())),
            kind: Some(format!("{kind}_error")),
            internal: Some(msg.into()),
            request_id: None,
        }
    }

    /// `5xx`：客户端只看到笼统的话，真实原因进 `internal`（服务端日志）。
    pub fn internal(err: impl fmt::Display) -> Self {
        Self {
            status: Status::INTERNAL_SERVER_ERROR,
            detail: "internal server error".to_string(),
            loc: None,
            kind: None,
            internal: Some(err.to_string()),
            request_id: None,
        }
    }

    pub fn with_request_id(mut self, id: u64) -> Self {
        self.request_id = Some(id);
        self
    }

    pub fn status(mut self, s: Status) -> Self {
        self.status = s;
        self
    }

    /// 序列化成响应体。字段名与 FastAPI 一致，多余的字段不出现。
    pub fn to_response(&self) -> Response {
        let body = match (&self.loc, &self.kind) {
            (Some((where_, name)), Some(kind)) => serde_json::json!({
                "detail": [{
                    "loc": [where_, name],
                    "msg": self.detail,
                    "type": kind,
                }]
            }),
            _ => {
                let mut o = serde_json::json!({ "detail": self.detail });
                if let Some(id) = self.request_id {
                    o["request_id"] = serde_json::json!(id);
                }
                o
            }
        };
        Response::json_with(serde_json::to_vec(&body).unwrap_or_default().as_slice(), self.status)
    }

    /// 该回给客户端的最小信息（含 5xx 的真实原因——只在服务端用）。
    pub fn server_log_line(&self) -> String {
        match &self.internal {
            Some(i) => format!("{} {} | {i}", self.status, self.detail),
            None => format!("{} {}", self.status, self.detail),
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}：{}", self.status, self.detail)?;
        if let Some((where_, name)) = &self.loc {
            write!(f, "（{where_}.{name}）")?;
        }
        Ok(())
    }
}

impl std::error::Error for ApiError {}

impl From<crate::error::Error> for ApiError {
    fn from(e: crate::error::Error) -> Self {
        match e {
            crate::error::Error::Driver(d) => ApiError::internal(format!("{:?}: {}", d.kind, d.message)),
            other => ApiError::internal(other),
        }
    }
}

impl From<rsbatis::DriverError> for ApiError {
    fn from(e: rsbatis::DriverError) -> Self {
        ApiError::internal(format!("{:?}: {}", e.kind, e.message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_shape_matches_fastapi() {
        let e = ApiError::validation("value", "query", "limit", "不是整数");
        let body: serde_json::Value =
            serde_json::from_slice(&e.to_response().body).unwrap();
        assert_eq!(body["detail"][0]["loc"][0], "query");
        assert_eq!(body["detail"][0]["loc"][1], "limit");
        assert_eq!(body["detail"][0]["type"], "value_error");
    }

    #[test]
    fn plain_error_has_string_detail() {
        let e = ApiError::not_found("没有这个用户");
        let body: serde_json::Value =
            serde_json::from_slice(&e.to_response().body).unwrap();
        assert_eq!(body["detail"], "没有这个用户");
        assert_eq!(e.to_response().status, Status::NOT_FOUND);
    }

    #[test]
    fn internal_error_does_not_leak_details_to_client() {
        let e = ApiError::internal("table users has no column 'nmae'");
        let body: serde_json::Value =
            serde_json::from_slice(&e.to_response().body).unwrap();
        assert_eq!(body["detail"], "internal server error");
        assert!(
            !serde_json::to_string(&body).unwrap().contains("nmae"),
            "真因不能出现在响应里"
        );
        // 但服务端日志要有
        assert!(e.server_log_line().contains("nmae"));
    }

    #[test]
    fn request_id_is_exposed_for_internal_errors() {
        let e = ApiError::internal("boom").with_request_id(17);
        let body: serde_json::Value =
            serde_json::from_slice(&e.to_response().body).unwrap();
        assert_eq!(body["request_id"], 17);
    }

    #[test]
    fn driver_error_becomes_500() {
        let d = rsbatis::DriverError::new(rsbatis::ErrorKind::Sql, "near \"SELEC\": syntax error");
        let e: ApiError = d.into();
        assert_eq!(e.status, Status::INTERNAL_SERVER_ERROR);
        assert!(!e.to_response().body.windows(5).any(|w| w == b"SELEC"));
    }
}
