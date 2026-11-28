//! 证书链校验：把 leaf、若干中间证书、信任根摆到一起去验。
//!
//! 只做浏览器握手必需的那几步——签名、CA 位、有效期、主机名。
//! 撤销（CRL/OCSP）、名称约束、策略映射一概不做，这是客户端爬虫，
//! 和浏览器信任模型的差距写在 README 里。

use super::der::Reader;
use super::ec;
use super::rsa;
use super::x509::{name_eq, Cert, PubKey, SigAlg, X509Error};

pub type Result<T> = core::result::Result<T, X509Error>;

/// 用 `issuer` 的公钥验证 `subject.der` 上那一段 TBS 的签名。
fn verify_signature(issuer: &Cert, alg: &SigAlg, tbs: &[u8], sig: &[u8]) -> bool {
    match (alg, &issuer.key) {
        (SigAlg::RsaPkcs1Sha256, PubKey::Rsa { modulus, exponent }) => {
            rsa::verify_pkcs1_sha256(modulus, exponent, tbs, sig)
        }
        (SigAlg::RsaPkcs1Sha384, PubKey::Rsa { modulus, exponent }) => {
            rsa::verify_pkcs1_sha384(modulus, exponent, tbs, sig)
        }
        (SigAlg::RsaPkcs1Sha512, PubKey::Rsa { modulus, exponent }) => {
            rsa::verify_pkcs1_sha512(modulus, exponent, tbs, sig)
        }
        (SigAlg::RsaPssSha256, PubKey::Rsa { modulus, exponent }) => {
            rsa::verify_pss_sha256(modulus, exponent, tbs, sig)
        }
        (SigAlg::EcdsaP384Sha384, PubKey::EcP384 { point }) => {
            // SPKI 里的点是 04 || X || Y（各 48 字节）
            if point.len() != 97 || point[0] != 0x04 {
                return false;
            }
            let mut r = Reader::new(sig);
            let Ok(mut seq) = r.seq() else { return false };
            let Ok(rr) = seq.int_bytes() else {
                return false;
            };
            let Ok(ss) = seq.int_bytes() else {
                return false;
            };
            ec::verify_p384(&point[1..49], &point[49..], tbs, rr, ss)
        }
        (SigAlg::EcdsaP256Sha256, PubKey::EcP256 { point }) => {
            // SPKI 里的点是 04 || X || Y
            if point.len() != 65 || point[0] != 0x04 {
                return false;
            }
            let mut r = Reader::new(sig);
            let Ok(mut seq) = r.seq() else { return false };
            let Ok(rr) = seq.int_bytes() else {
                return false;
            };
            let Ok(ss) = seq.int_bytes() else {
                return false;
            };
            ec::verify_p256(&point[1..33], &point[33..], tbs, rr, ss)
        }
        // 算法和密钥对不上（比如 P-384 根签的中间证书）——明确拒绝，不做降级
        _ => false,
    }
}

/// 链校验的结果摘要，方便调用方在错误信息里说清"卡在哪一步"。
#[derive(Debug, PartialEq, Eq)]
pub enum ChainError {
    /// 找不到某个证书的签发者
    NoIssuer,
    /// 签名验不过（含算法不支持）
    BadSignature,
    /// 中间证书没有 CA 位
    NotCa,
    /// 过期或尚未生效
    Expired,
    /// 链没走到任何信任根
    Untrusted,
    /// 主机名不匹配
    HostMismatch,
    /// 链太长
    TooLong,
}

impl ChainError {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChainError::NoIssuer => "no issuer for certificate",
            ChainError::BadSignature => "certificate signature does not verify",
            ChainError::NotCa => "intermediate certificate lacks CA bit",
            ChainError::Expired => "certificate expired or not yet valid",
            ChainError::Untrusted => "chain does not end at a trusted root",
            ChainError::HostMismatch => "certificate does not match host",
            ChainError::TooLong => "certificate chain too long",
        }
    }
}

/// 当前 UTC 秒数（证书有效期比较用）。
pub fn now_secs() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(_) => 0,
    }
}

/// 校验整条链。
///
/// `inters` 是服务端发来的中间证书（顺序随意，靠 issuer/subject 自己配对），
/// `roots` 是信任锚。链必须真正走到 `roots` 里的某一张，且最后一张必须是
/// 自签的（防止同名不同体的伪造）。
pub fn verify(leaf: &Cert, inters: &[Cert], roots: &[Cert], host: &str) -> core::result::Result<(), ChainError> {
    verify_at(leaf, inters, roots, host, now_secs())
}

pub fn verify_at(
    leaf: &Cert,
    inters: &[Cert],
    roots: &[Cert],
    host: &str,
    now: i64,
) -> core::result::Result<(), ChainError> {
    if !leaf.valid_at(now) {
        return Err(ChainError::Expired);
    }
    if !leaf.matches_host(host) {
        return Err(ChainError::HostMismatch);
    }

    // 从 leaf 往上走，最多 6 跳（正常站点 2–3 跳）。
    //
    // 同一张根证书常以两种形态出现：`SSL.com TLS ECC Root CA 2022` 既可能被服务端
    // 以「被 AAA Certificate Services 交叉签过」的形态发来，也可能以自签形态躺在
    // 系统根里。只按名字取第一个匹配的签发者就会走进走不通的那条分支，所以这里
    // 沿所有候选做一遍 DFS，只要有一条能落到信任锚就算通过。
    let mut last = ChainError::NoIssuer;
    let mut used: Vec<Vec<u8>> = Vec::new();
    if walk(leaf, inters, roots, now, 0, &mut used, &mut last) {
        return Ok(());
    }
    Err(last)
}

/// 从 `current` 往上找一条能落到信任锚的路。
fn walk(
    current: &Cert,
    inters: &[Cert],
    roots: &[Cert],
    now: i64,
    depth: usize,
    used: &mut Vec<Vec<u8>>,
    last: &mut ChainError,
) -> bool {
    if depth >= 6 {
        *last = ChainError::TooLong;
        return false;
    }
    let candidates: Vec<&Cert> = inters
        .iter()
        .chain(roots.iter())
        .filter(|c| name_eq(&c.subject, &current.issuer))
        .collect();
    if candidates.is_empty() {
        if std::env::var("RSBOOT_TLS_DEBUG").is_ok() {
            eprintln!(
                "  no issuer at depth={depth} want_cn={:?} roots={} inters={}",
                super::x509::name_cn(&current.issuer),
                roots.len(),
                inters.len()
            );
        }
        *last = ChainError::NoIssuer;
        return false;
    }
    for issuer in candidates {
        if used.iter().any(|d| d == &issuer.der) {
            continue;
        }
        if !issuer.valid_at(now) {
            *last = ChainError::Expired;
            continue;
        }
        // 除 leaf 的直接签发者外，每一张都必须是 CA
        if depth > 0 && !issuer.is_ca {
            *last = ChainError::NotCa;
            continue;
        }
        let sig_ok = verify_signature(issuer, &current.sig_alg, &current.tbs, &current.signature);
        if std::env::var("RSBOOT_TLS_DEBUG").is_ok() {
            let kind = match &issuer.key {
                PubKey::Rsa { .. } => "rsa",
                PubKey::EcP256 { .. } => "p256",
                PubKey::EcP384 { .. } => "p384",
                _ => "other",
            };
            eprintln!(
                "  chain depth={depth} alg={:?} key={kind} sig_ok={sig_ok} subj={:?}",
                current.sig_alg,
                super::x509::name_cn(&current.subject)
            );
        }
        if !sig_ok {
            *last = ChainError::BadSignature;
            continue;
        }
        // 自签（issuer==subject 且签的是自己）→ 必须本身就在信任锚里
        if name_eq(&issuer.issuer, &issuer.subject) {
            if roots.iter().any(|r| r.der == issuer.der) {
                return true;
            }
            *last = ChainError::Untrusted;
            continue;
        }
        used.push(issuer.der.clone());
        if walk(issuer, inters, roots, now, depth + 1, used, last) {
            return true;
        }
        used.pop();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::x509;

    const RSA_ROOT: &[u8] = include_bytes!("../../tests/fixtures/rsa_root.der");
    const RSA_LEAF: &[u8] = include_bytes!("../../tests/fixtures/rsa_leaf.der");
    const EC_ROOT: &[u8] = include_bytes!("../../tests/fixtures/ec_root.der");
    const EC_LEAF: &[u8] = include_bytes!("../../tests/fixtures/ec_leaf.der");

    fn cert(b: &[u8]) -> x509::Cert {
        x509::parse(b).unwrap()
    }

    #[test]
    fn rsa_chain_verifies() {
        let leaf = cert(RSA_LEAF);
        let roots = [cert(RSA_ROOT)];
        // fixture 的有效期是 2024→2034，取 2026 年时刻
        let ok = verify_at(&leaf, &[], &roots, "example.com", 1767225600);
        assert_eq!(ok, Ok(()));
    }

    #[test]
    fn ec_chain_verifies() {
        let leaf = cert(EC_LEAF);
        let roots = [cert(EC_ROOT)];
        assert_eq!(
            verify_at(&leaf, &[], &roots, "ec.example.com", 1767225600),
            Ok(())
        );
    }

    #[test]
    fn untrusted_root_is_rejected() {
        let leaf = cert(RSA_LEAF);
        let wrong = [cert(EC_ROOT)];
        assert_eq!(
            verify_at(&leaf, &[], &wrong, "example.com", 1767225600),
            Err(ChainError::NoIssuer)
        );
        // 用自己的根冒充：拿 EC 根当 RSA leaf 的信任锚，名字都对不上
        let self_signed = cert(RSA_ROOT);
        let empty: [x509::Cert; 0] = [];
        assert_eq!(
            verify_at(&self_signed, &[], &empty, "Test Root CA", 1767225600),
            Err(ChainError::NoIssuer)
        );
    }

    #[test]
    fn hostname_and_time_checks_run_first() {
        let leaf = cert(RSA_LEAF);
        let roots = [cert(RSA_ROOT)];
        assert_eq!(
            verify_at(&leaf, &[], &roots, "wrong.example.com", 1767225600),
            Err(ChainError::HostMismatch)
        );
        assert_eq!(
            verify_at(&leaf, &[], &roots, "example.com", 1000000000),
            Err(ChainError::Expired)
        );
    }

    #[test]
    fn tampered_leaf_fails_signature() {
        let leaf = cert(RSA_LEAF);
        let roots = [cert(RSA_ROOT)];
        // 改掉 TBS 里一个字节，签名必须失效
        let mut bad = leaf.clone();
        bad.tbs[20] ^= 0x01;
        assert_eq!(
            verify_at(&bad, &[], &roots, "example.com", 1767225600),
            Err(ChainError::BadSignature)
        );
    }
}
