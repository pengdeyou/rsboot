//! 提取器：把请求的一部分变成有类型的值。
//!
//! 一个 trait，六种实现。没有 axum 那种 `FromRequestParts`/`FromRequest` 的两段
//! 体系——那套存在的理由是"能拿借用"，而我们所有提取器都拿所有权（body 已经
//! 在内存里、路径参数本来就小）。少一层抽象，错误信息也短。
//!
//! ## 失败必须是 `ApiError`，不是 `String`
//!
//! 因为失败的语义差别很大：缺字段是 422，缺令牌是 **401**（不是 422）。
//! 混成一个错误类型，客户端就得猜。`ApiError` 里带 `status`，这一步不容妥协。

use serde::de::DeserializeOwned;

use crate::web::error::ApiError;
use crate::web::request::Request;

pub trait FromRequest: Sized {
    // `ApiError` 有 144 字节，clippy 建议装箱。这里不装：错误路径本就冷，
    // 而装箱会让每个提取器的签名都变成 `Box<ApiError>`，调用处多一层解引用，
    // 收益远小于成本。这是有意识的取舍，不是疏漏。
    #[allow(clippy::result_large_err)]
    fn from_request(req: &mut Request) -> Result<Self, ApiError>;
}

/// 路径参数，例如 `/users/{id}` 对应 `Path<u64>`。
///
/// 先把字符串尽力转成数字/布尔再反序列化，这样 `Path<u64>` 能直接工作；
/// 失败时退回全字符串再试一次，让自定义 `Deserialize` 也有机会。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Path<T>(pub T);

impl<T: DeserializeOwned> FromRequest for Path<T> {
    #[allow(clippy::result_large_err)]
    fn from_request(req: &mut Request) -> Result<Self, ApiError> {
        // 一个参数 → 直接反序列化成 T（`Path<u64>`）；
        // 多个参数 → 序列（tuple），顺序即模板里的顺序。
        // 用 map 会失败：`u64` 期望数字，`(A, B)` 期望序列，都不是 map。
        let shape = |as_string: bool| -> serde_json::Value {
            let conv = |v: &str| {
                if as_string {
                    serde_json::Value::String(v.to_string())
                } else {
                    coerce(v)
                }
            };
            match req.params.len() {
                1 => conv(&req.params[0].1),
                _ => serde_json::Value::Array(req.params.iter().map(|(_, v)| conv(v)).collect()),
            }
        };

        match serde_json::from_value(shape(false)) {
            Ok(v) => Ok(Path(v)),
            Err(first) => serde_json::from_value(shape(true)).map(Path).map_err(|_| {
                let names = req
                    .params
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>()
                    .join("、");
                ApiError::validation("path_type", "path", &names, first.to_string())
            }),
        }
    }
}

fn coerce(v: &str) -> serde_json::Value {
    if let Ok(n) = v.parse::<i64>() {
        return serde_json::Value::Number(n.into());
    }
    if let Ok(f) = v.parse::<f64>() {
        if let Some(n) = serde_json::Number::from_f64(f) {
            return serde_json::Value::Number(n);
        }
    }
    match v {
        "true" => serde_json::Value::Bool(true),
        "false" => serde_json::Value::Bool(false),
        _ => serde_json::Value::String(v.to_string()),
    }
}

/// 查询串，例如 `?limit=10` 对应 `Query<Limit>`。
///
/// 空查询串交给 serde：字段全是 `Option` 的结构体可以成功，必填字段则会 422。
/// 这正是我们要的行为（FastAPI 里必填 query 参数缺失也是 422）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Query<T>(pub T);

impl<T: DeserializeOwned> FromRequest for Query<T> {
    #[allow(clippy::result_large_err)]
    fn from_request(req: &mut Request) -> Result<Self, ApiError> {
        serde_urlencoded::from_str::<T>(&req.query)
            .map(Query)
            .map_err(|e| ApiError::validation("query_invalid", "query", "", e.to_string()))
    }
}

/// `Authorization: Bearer <token>`。
///
/// 缺失或格式不对 → **401**，不是 422。客户端据此决定"去取令牌"而不是
/// "改参数"，这个区分是可观测的行为差异。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bearer(pub String);

impl FromRequest for Bearer {
    fn from_request(req: &mut Request) -> Result<Self, ApiError> {
        let raw = req
            .header("authorization")
            .ok_or_else(|| ApiError::unauthorized("缺少 Authorization 头"))?;
        let (scheme, token) = raw
            .split_once(' ')
            .ok_or_else(|| ApiError::unauthorized("Authorization 头格式应为 `Bearer <token>`"))?;
        if !scheme.eq_ignore_ascii_case("bearer") {
            return Err(ApiError::unauthorized(format!(
                "只支持 Bearer 方案，收到 `{scheme}`"
            )));
        }
        let token = token.trim();
        if token.is_empty() {
            return Err(ApiError::unauthorized("Bearer 令牌是空的"));
        }
        Ok(Bearer(token.to_string()))
    }
}

/// API Key，从 `X-API-Key` 头取。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKey(pub String);

impl FromRequest for ApiKey {
    fn from_request(req: &mut Request) -> Result<Self, ApiError> {
        req.header("x-api-key")
            .map(|v| ApiKey(v.trim().to_string()))
            .filter(|k| !k.0.is_empty())
            .ok_or_else(|| ApiError::unauthorized("缺少 `X-API-Key` 头"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::http::{Method, Status};

    fn req_with_params(pairs: &[(&str, &str)]) -> Request {
        let mut r = Request::new(Method::Get, "/x");
        r.params = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        r
    }

    #[test]
    fn path_parses_numbers() {
        let mut r = req_with_params(&[("id", "42")]);
        let Path(id) = Path::<u64>::from_request(&mut r).unwrap();
        assert_eq!(id, 42);
    }

    #[test]
    fn path_parses_string_and_bool() {
        let mut r = req_with_params(&[("name", "abc"), ("flag", "true")]);
        let Path((n, f)) = Path::<(String, bool)>::from_request(&mut r).unwrap();
        assert_eq!(n, "abc");
        assert!(f);
    }

    #[test]
    fn path_type_error_is_422_with_param_names() {
        let mut r = req_with_params(&[("id", "abc")]);
        let e = Path::<u64>::from_request(&mut r).unwrap_err();
        assert_eq!(e.status, Status::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn query_parses_options() {
        #[derive(serde::Deserialize, Debug)]
        struct Q {
            limit: Option<u32>,
            q: Option<String>,
        }
        let mut r = Request::new(Method::Get, "/x").with_query("limit=10&q=a+b");
        let Query(q) = Query::<Q>::from_request(&mut r).unwrap();
        assert_eq!(q.limit, Some(10));
        assert_eq!(q.q.as_deref(), Some("a b"), "`+` 应解码成空格");
    }

    #[test]
    fn missing_required_query_is_422() {
        #[derive(serde::Deserialize, Debug)]
        struct Q {
            #[allow(dead_code)]
            limit: u32,
        }
        let mut r = Request::new(Method::Get, "/x");
        assert_eq!(
            Query::<Q>::from_request(&mut r).unwrap_err().status,
            Status::UNPROCESSABLE_ENTITY
        );
    }

    #[test]
    fn bearer_is_401_when_absent_and_case_insensitive_when_present() {
        let mut r = Request::new(Method::Get, "/x");
        assert_eq!(
            Bearer::from_request(&mut r).unwrap_err().status,
            Status::UNAUTHORIZED
        );

        let mut r = Request::new(Method::Get, "/x").with_header("authorization", "bearer tok");
        assert_eq!(Bearer::from_request(&mut r).unwrap().0, "tok");
    }

    #[test]
    fn bearer_rejects_other_schemes() {
        let mut r = Request::new(Method::Get, "/x").with_header("authorization", "Basic dXNlcg==");
        let e = Bearer::from_request(&mut r).unwrap_err();
        assert_eq!(e.status, Status::UNAUTHORIZED);
        assert!(e.detail.contains("Bearer"), "{e}");
    }

    #[test]
    fn api_key_extracts() {
        let mut r = Request::new(Method::Get, "/x").with_header("x-api-key", "k1");
        assert_eq!(ApiKey::from_request(&mut r).unwrap().0, "k1");
    }
}
