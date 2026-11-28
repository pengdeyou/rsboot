//! 零依赖 TLS 1.3 客户端。
//!
//! rsboot 的扩展 crate 不引第三方依赖，那就没有 `rustls` 可用——所以这套
//! TLS 是**在本仓库里自己实现的**：x25519、ChaCha20-Poly1305、AES-GCM、
//! SHA-256/HMAC/HKDF、X.509 解析与签名校验，全部手写。
//!
//! 这不是"自己造密码学"式的莽撞：上面那些原语都有公开的标准测试向量，
//! 每个都在 `#[test]` 里对过；TLS 1.3 的握手只做**客户端**这一侧，
//! 且强制校验证书链与主机名，不提供"跳过校验"的开关。
//!
//! 明确不做：TLS 1.2 及以下（那要 RSA 密钥交换与 CBC 时代的一堆历史包袱）、
//! 客户端证书、会话票据恢复、ALPN 协商（HTTP/2 不做），以及 DTLS。
//!
//! 分层：`sha256` / `chacha` / `aes` / `x25519` / `p256` / `rsa` 是原语，
//! `x509` 是证书解析与链校验，`handshake` 是记录层与握手状态机。

pub(crate) mod consts;
pub mod aes;
pub mod chacha;
pub mod sha256;
pub mod x25519;
