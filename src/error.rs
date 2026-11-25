//! 框架的统一错误类型。
//!
//! 用一个枚举而不是 `Box<dyn Error>`：调用方最常做的判断是"是配置写错了、
//! 还是数据库不在"，两者该给完全不同的提示。

use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// 配置缺失或格式不对（含文件名与行号）
    Config(String),
    /// 要的东西没注册：数据库名、模块名
    Missing(String),
    /// 驱动层错误（连接、SQL）
    Driver(rsbatis::DriverError),
    /// 生命周期：启动顺序、嵌套借用、开关钩子
    Lifecycle(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Config(m) => write!(f, "配置错误：{m}"),
            Error::Missing(m) => write!(f, "{m}"),
            Error::Driver(e) => write!(f, "数据库错误（{:?}）：{}", e.kind, e.message),
            Error::Lifecycle(m) => write!(f, "生命周期错误：{m}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Driver(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rsbatis::DriverError> for Error {
    fn from(e: rsbatis::DriverError) -> Self {
        Error::Driver(e)
    }
}

impl From<rsbatis::MigrateError> for Error {
    fn from(e: rsbatis::MigrateError) -> Self {
        Error::Lifecycle(format!("迁移失败：{e}"))
    }
}

pub type Result<T> = std::result::Result<T, Error>;
