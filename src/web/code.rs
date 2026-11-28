//! 细化错误码。
//!
//! `code` 是 4 位（也可更长）数字：**前 3 位就是 HTTP 状态码，后面是细分序号**。
//! 这样一张码表同时满足两层消费者——网关看状态码决定重试/缓存，
//! 业务看细分码决定给用户看哪句提示、走哪个分支，不需要查两张表对齐。
//!
//! ```text
//! 4000  参数错误（通用）
//! 4002  余额不足（业务自己分配）
//! 4010  未登录
//! 4040  资源不存在
//! 4220  参数校验失败
//! 5000  服务异常（通用）
//! 5001  下游/第三方服务异常
//! 5002  数据库异常
//! 5003  超时
//! 5030  未就绪
//! ```
//!
//! 4xx 段留给业务码，5xx 段留给基础设施。同一段内各占一位数的序号，
//! 够分 9 类；不够就往后加位数，`status_of` 照样只看前三位。

use crate::web::http::Status;

/// 成功。
pub const OK: u32 = 200;

// ——— 4xx：请求 / 业务错误 ———

/// 参数错误（通用）。
pub const BAD_REQUEST: u32 = 4000;
/// 未认证。
pub const UNAUTHORIZED: u32 = 4010;
/// 无权限。
pub const FORBIDDEN: u32 = 4030;
/// 资源不存在。
pub const NOT_FOUND: u32 = 4040;
/// 方法不允许。
pub const METHOD_NOT_ALLOWED: u32 = 4050;
/// 冲突（重复创建、版本过期）。
pub const CONFLICT: u32 = 4090;
/// 请求体过大。
pub const PAYLOAD_TOO_LARGE: u32 = 4130;
/// 参数校验失败。
pub const VALIDATION: u32 = 4220;
/// 限流。
pub const TOO_MANY_REQUESTS: u32 = 4290;

// ——— 5xx：服务异常 ———

/// 服务异常（通用）。
pub const INTERNAL: u32 = 5000;
/// 下游 / 第三方服务异常。
pub const UPSTREAM: u32 = 5001;
/// 数据库异常。
pub const DATABASE: u32 = 5002;
/// 超时。
pub const TIMEOUT: u32 = 5003;
/// 配置 / 环境问题。
pub const CONFIG: u32 = 5004;

/// 服务未就绪（启动中、依赖未通）。
pub const NOT_READY: u32 = 5030;

/// 由细化码推出 HTTP 状态：取前三位。`4002` → 400，`5001` → 500，`200` → 200。
pub fn status_of(code: u32) -> Status {
    if code < 1000 {
        return Status(code as u16);
    }
    let mut lead = code;
    while lead >= 1000 {
        lead /= 10;
    }
    Status(lead as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_is_the_leading_three_digits() {
        assert_eq!(status_of(4002).0, 400);
        assert_eq!(status_of(5001).0, 500);
        assert_eq!(status_of(4220).0, 422);
        assert_eq!(status_of(4040).0, 404);
        assert_eq!(status_of(200).0, 200, "三位码就是状态码本身");
        assert_eq!(status_of(50003).0, 500, "更长的码也只看前三位");
    }
}
