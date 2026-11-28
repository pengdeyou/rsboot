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
    /// 细化错误码：前 3 位是 HTTP 状态，后面是细分序号（见 [`code`](crate::web::code)）。
    pub code: u32,
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
    /// 无论 `http.envelope` 开没开，这条错误都必须按统一响应体输出。
    /// `res::fail` 造出来的错误会置为 `true`：用户显式要了统一形状，
    /// 就不该被全局开关再改一次。
    pub(crate) always_wrap: bool,
}

impl ApiError {
    pub fn new(status: Status, detail: impl Into<String>) -> Self {
        Self {
            code: status.0 as u32 * 10,
            status,
            detail: detail.into(),
            loc: None,
            kind: None,
            internal: None,
            request_id: None,
            always_wrap: false,
        }
    }

    /// 用细化码构造：HTTP 状态由码的前三位推出（`4002` → 400，`5001` → 500）。
    ///
    /// 业务码自行分配即可：`4001` 库存不足、`4002` 余额不足、`4003` 状态非法……
    /// 只要落在外层对应的 4xx/5xx 段里，网关和客户端两边都读得懂。
    pub fn with_code(code: u32, detail: impl Into<String>) -> Self {
        let mut e = Self::new(crate::web::code::status_of(code), detail);
        e.code = code;
        e
    }

    /// 服务异常（`500` + `5000`）：逻辑 bug、依赖挂掉这类"服务端自己的问题"。
    pub fn service(detail: impl Into<String>) -> Self {
        Self::with_code(crate::web::code::INTERNAL, detail)
    }

    /// 下游 / 第三方服务异常（`500` + `5001`）。
    pub fn upstream(detail: impl Into<String>) -> Self {
        Self::with_code(crate::web::code::UPSTREAM, detail)
    }

    /// 数据库异常（`500` + `5002`）。
    pub fn database(detail: impl Into<String>) -> Self {
        Self::with_code(crate::web::code::DATABASE, detail)
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
            code: crate::web::code::VALIDATION,
            status: Status::UNPROCESSABLE_ENTITY,
            // msg 就是给客户端看的那句话（对齐 FastAPI 的 `msg`）。
            // 早先这里只写 `where.name`，把真正的解析原因（`missing field `q``）
            // 塞进 internal 只进日志——客户端只知道"哪里错"，不知道"错在哪"，
            // 排查全靠猜。定位由 `loc` 承担，msg 承担原因，两者不重复。
            detail: msg.into(),
            loc: Some((where_.to_string(), name.to_string())),
            kind: Some(format!("{kind}_error")),
            internal: None,
            request_id: None,
            always_wrap: false,
        }
    }

    /// `5xx`：客户端只看到笼统的话，真实原因进 `internal`（服务端日志）。
    pub fn internal(err: impl fmt::Display) -> Self {
        Self {
            code: crate::web::code::INTERNAL,
            status: Status::INTERNAL_SERVER_ERROR,
            detail: "internal server error".to_string(),
            loc: None,
            kind: None,
            internal: Some(err.to_string()),
            request_id: None,
            always_wrap: false,
        }
    }

    /// 标记：无论全局开关如何，这条错误都按统一响应体输出（`Res::err` / `Res::fail` 用）。
    pub(crate) fn always_wrapped(&mut self) {
        self.always_wrap = true;
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
            (Some((where_, name)), Some(kind)) => {
                // 整个 body/query 解析失败时没有具体字段名，`loc` 只给一层。
                // 输出 `["body", ""]` 会让客户端以为有个叫空字符串的参数。
                let loc = if name.is_empty() {
                    serde_json::json!([where_])
                } else {
                    serde_json::json!([where_, name])
                };
                serde_json::json!({
                    "detail": [{
                        "loc": loc,
                        "msg": self.detail,
                        "type": kind,
                    }]
                })
            }
            _ => {
                let mut o = serde_json::json!({ "detail": self.detail });
                if let Some(id) = self.request_id {
                    o["request_id"] = serde_json::json!(id);
                }
                o
            }
        };
        let mut r = Response::json_with(
            serde_json::to_vec(&body).unwrap_or_default().as_slice(),
            self.status,
        );
        // 留一份原件，出口处（Router）据此决定是否换成统一响应体形状
        r.api_error = Some(self.clone());
        r
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
    fn code_carries_the_status_in_its_prefix() {
        let e = ApiError::with_code(4002, "余额不足");
        assert_eq!(e.status, Status::BAD_REQUEST);
        assert_eq!(e.code, 4002);

        let e = ApiError::service("库存服务挂了");
        assert_eq!(e.status, Status::INTERNAL_SERVER_ERROR);
        assert_eq!(e.code, crate::web::code::INTERNAL);

        let e = ApiError::database("db down");
        assert_eq!(e.code, crate::web::code::DATABASE);
        assert_eq!(e.status, Status::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn driver_error_becomes_500() {
        let d = rsbatis::DriverError::new(rsbatis::ErrorKind::Sql, "near \"SELEC\": syntax error");
        let e: ApiError = d.into();
        assert_eq!(e.status, Status::INTERNAL_SERVER_ERROR);
        assert!(!e.to_response().body.windows(5).any(|w| w == b"SELEC"));
    }
}
