//! 可选 TLS（`tls` feature，默认关）。
//!
//! 为什么是 feature 而不是默认：TLS 不可能手写，一定要引 `rustls`；
//! 而这个 crate 的立身之本是"装进来不会给你的依赖图带进任何东西"。
//! 具体体现在 `Cargo.toml` 里：`default = []`，`rustls` 全是 `optional`。
//!
//! 根证书用 `webpki-roots`（Mozilla 根证书编进二进制）而不是读系统 store：
//! 系统 store 在不同平台/容器里差别很大，爬同一个站可能在一台机器上成功、
//! 另一台上失败。代价是私有 CA 的站连不上——那属于"自己要加信任锚"的场景，
//! 该由调用方显式配置，不该由运营系统的默认行为决定。

use std::net::TcpStream;
use std::sync::{Arc, OnceLock};

use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use rustls_pki_types::ServerName;

use crate::http::Error;

/// 一份 TLS 连接。`StreamOwned` = 连接 + 底下那条 socket。
pub type TlsStream = StreamOwned<ClientConnection, TcpStream>;

/// 进程内只建一次：根证书解析是几百个对象，每个连接都建一次纯属浪费。
///
/// `OnceLock` 而不是 `lazy_static`/`once_cell`：标准库自 1.70 就有，
/// 正好在这一版 rust-version 之内。
fn config() -> Arc<ClientConfig> {
    static CFG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CFG.get_or_init(|| {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Arc::new(
            ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    })
    .clone()
}

/// 把一条已连上的 TCP 包成 TLS。`host` 同时用于 SNI 和证书校验。
///
/// 证书校验是必须的：爬虫跳过校验就等于把"对面是谁"交给 DNS 决定。
pub fn wrap(tcp: TcpStream, host: &str) -> Result<TlsStream, Error> {
    let name = ServerName::try_from(host.to_string())
        .map_err(|e| Error::Connect(format!("bad tls server name {host}: {e}")))?;
    let conn = ClientConnection::new(config(), name)
        .map_err(|e| Error::Connect(format!("tls handshake init: {e}")))?;
    Ok(StreamOwned::new(conn, tcp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_store_is_not_empty() {
        // 根证书列表为空会导致所有握手都失败，而且现象是"连不上"，
        // 很难从错误里看出来是这里的问题
        let cfg = config();
        assert!(cfg.alpn_protocols.is_empty());
        assert!(!webpki_roots::TLS_SERVER_ROOTS.is_empty());
        // 同一个进程第二次拿到的应当是同一份
        assert!(Arc::ptr_eq(&cfg, &config()));
    }
}
