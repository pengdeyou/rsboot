//! 统一异常处理。
//!
//! Rust 里没有异常，但有**两层**会让请求以失败告终的东西，必须分别处理，
//! 只做一层是不够的：
//!
//! 1. `Result` 错误：可预期。数据库超时、参数不合法、未授权。
//! 2. `panic`：不可预期。索引越界、`unwrap` 了 `None`、断言失败。
//!
//! ## 为什么必须 catch panic
//!
//! 同步 worker 模型下，一个 handler panic 会让**整个 worker 线程退出**。
//! 线程池容量只减不增——服务不会立刻挂，但会慢慢变成 503。这是"上线三天后
//! 才开始出事"的典型故障，必须在框架层兜住。
//!
//! 前提：`panic = "unwind"`（Rust 默认）。如果使用者设成 `abort`，
//! `catch_unwind` 不起作用，整个进程会退出——这一点在文档里必须写明，
//! 否则会出现"框架说兜住了、实际没兜住"的错觉。
//!
//! ## panic 之后请求对象不能复用
//!
//! 用 `AssertUnwindSafe` 是因为 `&mut Request` 天然不是 `UnwindSafe`。
//! 这不代表可以继续用它：panic 后请求内部的借用标志、body 都被读过，
//! 状态未知。所以捕获后**直接丢弃请求**，回 500，不再碰它。
//!
//! `RefCell` 的借用标志在栈展开时会正常释放（`RefMut` 的析构会跑），
//! 所以数据库连接不会因为 handler panic 而永久"被占用"。这条是实测结论，
//! 不是推测——`panic_does_not_poison_db_session` 测试盯着它。

use std::error::Error as StdError;
use std::panic::AssertUnwindSafe;

use crate::web::error::ApiError;
use crate::web::http::Status;
use crate::web::response::{IntoResponse, Response};

/// handler 里 `?` 的默认错误类型。
pub type ApiResult<T> = Result<T, ApiError>;

/// 业务错误 → `ApiError`。
///
/// 实现它之后，handler 就能直接 `-> Result<Json<T>, MyError>` 并在函数体里用 `?`，
/// 不需要每处 `.map_err(...)`。这是"统一异常处理"最关键的一条：**一处定义，
/// 全站一致**。
///
/// 默认实现必须保守：未知错误映射成 500，`detail` 不回给客户端。
/// 想让客户端看到具体话术，就得显式构造 `ApiError::new(Status::BAD_REQUEST, ...)`。
pub trait IntoApiError {
    fn into_api_error(self) -> ApiError;
}

impl IntoApiError for ApiError {
    fn into_api_error(self) -> ApiError {
        self
    }
}

impl IntoApiError for crate::error::Error {
    fn into_api_error(self) -> ApiError {
        // 复用已有的 From：驱动错误带 kind，进日志；客户端只看到 500
        ApiError::from(self)
    }
}

impl IntoApiError for rsbatis::DriverError {
    fn into_api_error(self) -> ApiError {
        ApiError::from(self)
    }
}

impl IntoApiError for std::convert::Infallible {
    fn into_api_error(self) -> ApiError {
        match self {}
    }
}

/// 字符串当作内部错误：**不**把原文回给客户端。
///
/// 这是刻意的：`Err("table users has no column nmae".to_string())` 这种写法
/// 在业务代码里很常见，而它正是泄露来源。要 400 请显式构造 `ApiError`。
impl IntoApiError for String {
    fn into_api_error(self) -> ApiError {
        ApiError::internal(self)
    }
}

impl IntoApiError for &str {
    fn into_api_error(self) -> ApiError {
        ApiError::internal(self)
    }
}

impl IntoApiError for Box<dyn StdError + Send + Sync> {
    fn into_api_error(self) -> ApiError {
        ApiError::internal(self)
    }
}

/// `Result<T, E>` 直接作为 handler 返回值。
impl<T, E> IntoResponse for Result<T, E>
where
    T: IntoResponse,
    E: IntoApiError,
{
    fn into_response(self) -> Response {
        match self {
            Ok(v) => v.into_response(),
            Err(e) => e.into_api_error().into_response(),
        }
    }
}

// ───────────────────────── trace 上下文 ─────────────────────────

thread_local! {
    /// 当前线程正在处理的请求 id。存在的理由只有一个：
    /// panic hook 是全局的、拿不到请求上下文，而"panic 时这段日志属于哪个请求"
    /// 恰恰是最该知道的事。
    static CURRENT_TRACE: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// 在 trace 上下文里执行。退出时恢复原值（支持嵌套调用）。
pub fn with_trace<R>(trace: Option<&str>, f: impl FnOnce() -> R) -> R {
    let saved = CURRENT_TRACE.with(|c| {
        std::mem::replace(&mut *c.borrow_mut(), trace.map(|s| s.to_string()))
    });
    // 用 guard 保证 panic 时也能恢复，否则 trace 会串到下一个请求
    struct Restore(Option<String>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let v = self.0.take();
            CURRENT_TRACE.with(|c| *c.borrow_mut() = v);
        }
    }
    let _restore = Restore(saved);
    f()
}

pub fn current_trace() -> Option<String> {
    CURRENT_TRACE.with(|c| c.borrow().clone())
}

/// 安装 panic hook：日志里带线程名与 trace_id。
///
/// 只装一次（重复调用会覆盖，行为幂等）。在 `App::start()` 里调用。
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let trace = current_trace().unwrap_or_else(|| "-".to_string());
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed");
        eprintln!("[rsboot] panic thread={name} trace={trace} {info}");
        // 保留标准 hook 的行为（RUST_BACKTRACE 仍然生效）
        default(info);
    }));
}

/// 捕获 panic。返回 `Err` 时调用方必须丢弃请求对象，不要再读它。
pub fn guard<R>(f: impl FnOnce() -> R) -> std::thread::Result<R> {
    std::panic::catch_unwind(AssertUnwindSafe(f))
}

/// panic 负载 → 500 响应。
///
/// `detail` 永远笼统（不把 panic 消息回给客户端，那可能包含路径、SQL、数据）；
/// panic 原文进服务端日志（由 hook 输出）。
pub fn panic_response(payload: Box<dyn std::any::Any + Send>) -> Response {
    let msg = payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "非字符串 panic".to_string());
    ApiError::internal(format!("handler panic：{msg}")).into_response()
}

/// 断言用：构造一个"一定能 panic"的响应，测试里用。
#[doc(hidden)]
pub fn boom_for_test() -> Response {
    panic!("故意 panic 的测试 handler");
    #[allow(unreachable_code)]
    Response::new(Status::INTERNAL_SERVER_ERROR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::http::Method;
    use crate::web::request::Request;

    #[derive(Debug)]
    struct BizError;
    impl IntoApiError for BizError {
        fn into_api_error(self) -> ApiError {
            ApiError::new(Status::BAD_REQUEST, "余额不足")
        }
    }

    fn done() -> Response {
        Response::text("ok")
    }

    #[test]
    fn result_ok_and_err_become_responses() {
        let ok: Result<Response, ApiError> = Ok(done());
        assert_eq!(ok.into_response().status, Status::OK);

        let bad: Result<Response, ApiError> = Err(ApiError::not_found("x"));
        assert_eq!(bad.into_response().status, Status::NOT_FOUND);
    }

    #[test]
    fn business_error_carries_its_own_status() {
        let r: Result<Response, BizError> = Err(BizError);
        let resp = r.into_response();
        assert_eq!(resp.status, Status::BAD_REQUEST);
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["detail"], "余额不足");
    }

    #[test]
    fn string_errors_are_internal_and_not_leaked() {
        let r: Result<Response, String> = Err("SELECT 失败：no column nmae".into());
        let resp = r.into_response();
        assert_eq!(resp.status, Status::INTERNAL_SERVER_ERROR);
        let body = String::from_utf8(resp.body).unwrap();
        assert!(!body.contains("nmae"), "原文不能进响应：{body}");
    }

    #[test]
    fn unwrap_panic_is_caught_and_becomes_500() {
        let r = guard(|| -> Response { panic!("boom") });
        let resp = r.err().map(panic_response).expect("必须捕获到 panic");
        assert_eq!(resp.status, Status::INTERNAL_SERVER_ERROR);
        let body = String::from_utf8(resp.body).unwrap();
        assert!(!body.contains("boom"), "panic 原文不能进响应：{body}");
    }

    #[test]
    fn non_string_panic_payload_still_handled() {
        let r = guard(|| -> Response { std::panic::panic_any(42u8) });
        let resp = r.err().map(panic_response).unwrap();
        assert_eq!(resp.status, Status::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn trace_context_is_restored_even_on_panic() {
        let r = guard(|| with_trace(Some("abc"), || panic!("x")));
        assert!(r.is_err());
        assert!(
            current_trace().is_none(),
            "panic 后 trace 必须清掉，否则会串到下一个请求"
        );
    }

    #[test]
    fn nested_trace_calls_restore_outer() {
        with_trace(Some("outer"), || {
            with_trace(Some("inner"), || {
                assert_eq!(current_trace().as_deref(), Some("inner"));
            });
            assert_eq!(current_trace().as_deref(), Some("outer"));
        });
    }

    #[test]
    fn panic_does_not_lose_request_trace_value() {
        let req = Request::new(Method::Get, "/x").with_header("x-request-id", "t-1");
        let id = req.request_id().map(|s| s.to_string());
        let r = with_trace(id.as_deref(), || guard(|| panic!("y")));
        assert!(r.is_err());
        assert_eq!(req.request_id(), Some("t-1"), "请求本身不受影响");
    }
}
