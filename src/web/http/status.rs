//! HTTP 状态码。
//!
//! 只列框架真正会产出的码，不做完整枚举。`reason()` 对未知码返回
//! `"Unknown"`——不 panic，也不给空串（空串会让状态行变成 `HTTP/1.1 200 `）。

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Status(pub u16);

impl Status {
    pub const OK: Status = Status(200);
    pub const CREATED: Status = Status(201);
    pub const NO_CONTENT: Status = Status(204);
    pub const MOVED_PERMANENTLY: Status = Status(301);
    pub const FOUND: Status = Status(302);
    pub const NOT_MODIFIED: Status = Status(304);
    pub const BAD_REQUEST: Status = Status(400);
    pub const UNAUTHORIZED: Status = Status(401);
    pub const FORBIDDEN: Status = Status(403);
    pub const NOT_FOUND: Status = Status(404);
    pub const METHOD_NOT_ALLOWED: Status = Status(405);
    pub const CONFLICT: Status = Status(409);
    pub const LENGTH_REQUIRED: Status = Status(411);
    pub const PAYLOAD_TOO_LARGE: Status = Status(413);
    pub const UNPROCESSABLE_ENTITY: Status = Status(422);
    pub const TOO_MANY_REQUESTS: Status = Status(429);
    pub const INTERNAL_SERVER_ERROR: Status = Status(500);
    pub const NOT_IMPLEMENTED: Status = Status(501);
    pub const SERVICE_UNAVAILABLE: Status = Status(503);

    pub fn is_success(self) -> bool {
        (200..300).contains(&self.0)
    }
    pub fn is_client_error(self) -> bool {
        (400..500).contains(&self.0)
    }
    pub fn is_server_error(self) -> bool {
        (500..600).contains(&self.0)
    }

    pub fn reason(self) -> &'static str {
        match self.0 {
            100 => "Continue",
            101 => "Switching Protocols",
            200 => "OK",
            201 => "Created",
            204 => "No Content",
            206 => "Partial Content",
            301 => "Moved Permanently",
            302 => "Found",
            304 => "Not Modified",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            406 => "Not Acceptable",
            409 => "Conflict",
            411 => "Length Required",
            413 => "Payload Too Large",
            422 => "Unprocessable Entity",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            501 => "Not Implemented",
            502 => "Bad Gateway",
            503 => "Service Unavailable",
            504 => "Gateway Timeout",
            _ => "Unknown",
        }
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.0, self.reason())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes() {
        assert!(Status::OK.is_success());
        assert!(Status::NOT_FOUND.is_client_error());
        assert!(Status::INTERNAL_SERVER_ERROR.is_server_error());
    }

    #[test]
    fn reason_of_unknown_is_not_empty() {
        assert_eq!(Status(422).reason(), "Unprocessable Entity");
        assert_eq!(Status(999).reason(), "Unknown");
    }

    #[test]
    fn displays_as_status_line_without_version() {
        assert_eq!(Status::OK.to_string(), "200 OK");
    }
}
