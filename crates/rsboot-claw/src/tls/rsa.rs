//! RSA 验签：PKCS#1 v1.5（SHA-256/384/512）与 PSS（SHA-256）。
//!
//! 只要"验"不要"签"：客户端不持有私钥，签名生成那套（CRT、盲化、
//! 常时间比较）全都不需要。

use super::bignum::{self, Mont};
use super::sha256;
use super::sha512;

/// DigestInfo ::= SEQUENCE { AlgorithmIdentifier, OCTET STRING } 的 DER 前缀
/// （RFC 8017 §9.2 注 1），不含摘要值本身。
const SHA256_DIGEST_INFO: [u8; 19] =
    hex_const(b"3031300d060960864801650304020105000420");
const SHA384_DIGEST_INFO: [u8; 19] =
    hex_const(b"3041300d060960864801650304020205000430");
const SHA512_DIGEST_INFO: [u8; 19] =
    hex_const(b"3051300d060960864801650304020305000440");

const fn hex_const(s: &[u8]) -> [u8; 19] {
    let mut out = [0u8; 19];
    let mut i = 0;
    while i < 19 {
        out[i] = nib(s[2 * i]) << 4 | nib(s[2 * i + 1]);
        i += 1;
    }
    out
}

const fn nib(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        _ => 0,
    }
}

/// 模数字节数（去掉大端前导零）。
fn key_bytes(n_be: &[u8]) -> usize {
    let mut v = n_be;
    while v.len() > 1 && v[0] == 0 {
        v = &v[1..];
    }
    v.len()
}

fn bit_len(n_be: &[u8]) -> usize {
    let mut v = n_be;
    while v.len() > 1 && v[0] == 0 {
        v = &v[1..];
    }
    let mut bits = (v.len() - 1) * 8;
    let mut top = v[0];
    while top != 0 {
        bits += 1;
        top >>= 1;
    }
    bits
}

/// `s^e mod n`，结果补零到模数字节长度。
fn rsa_public(n_be: &[u8], e_be: &[u8], s: &[u8]) -> Option<Vec<u8>> {
    let k = key_bytes(n_be);
    if s.len() > k {
        return None;
    }
    let mont = mont_for(n_be)?;
    // 大端补零到 k 字节（签名比模数短是允许的）
    let mut sb = vec![0u8; k];
    sb[k - s.len()..].copy_from_slice(s);
    let sm = mont.to_mont(&bignum::from_be(&sb));
    let em = mont.pow(&sm, e_be);
    Some(bignum::to_be(&mont.from_mont(&em), k))
}

thread_local! {
    /// 模数 → Montgomery 上下文。`Mont::new` 里的 `R² mod n` 是一次 2^(128k) 的
    /// 逐位长除法，2048 位模数要 ~100 µs —— 比一次验签本身还贵。同一个站点的证书
    /// 一次握手要验两次（链 + CertificateVerify），按线程缓存后这笔开销归零。
    static MONT_CACHE: std::cell::RefCell<Vec<(Vec<u8>, std::rc::Rc<Mont>)>> =
        std::cell::RefCell::new(Vec::new());
}

fn mont_for(n_be: &[u8]) -> Option<std::rc::Rc<Mont>> {
    MONT_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if let Some((_, m)) = c.iter().find(|(k, _)| k.as_slice() == n_be) {
            return Some(m.clone());
        }
        let m = std::rc::Rc::new(Mont::new(n_be)?);
        if c.len() >= 4 {
            c.remove(0);
        }
        c.push((n_be.to_vec(), m.clone()));
        Some(m)
    })
}

/// PKCS#1 v1.5 + 任意 SHA-2 摘要（按 DigestInfo 前缀分派）。
fn verify_pkcs1(
    n_be: &[u8],
    e_be: &[u8],
    msg: &[u8],
    sig: &[u8],
    digest: fn(&[u8]) -> Vec<u8>,
    digest_info: &[u8],
) -> bool {
    let k = key_bytes(n_be);
    let Some(em) = rsa_public(n_be, e_be, sig) else {
        return false;
    };
    let hash = digest(msg);
    // 00 01 FF…FF 00 || DigestInfo || hash，FF 段至少 8 字节
    let tail = digest_info.len() + hash.len();
    if k < 3 + 8 + tail {
        return false;
    }
    if em[0] != 0x00 || em[1] != 0x01 {
        return false;
    }
    let sep = k - tail - 1;
    if em[sep] != 0x00 {
        return false;
    }
    if !em[2..sep].iter().all(|&b| b == 0xff) {
        return false;
    }
    if em[sep + 1..sep + 1 + digest_info.len()] != *digest_info {
        return false;
    }
    em[k - hash.len()..] == hash
}

/// PKCS#1 v1.5 + SHA-256（OID 1.2.840.113549.1.1.11）。
pub fn verify_pkcs1_sha256(n_be: &[u8], e_be: &[u8], msg: &[u8], sig: &[u8]) -> bool {
    verify_pkcs1(
        n_be,
        e_be,
        msg,
        sig,
        |m| sha256::sha256(m).to_vec(),
        &SHA256_DIGEST_INFO,
    )
}

/// PKCS#1 v1.5 + SHA-384（OID 1.2.840.113549.1.1.12）。Let's Encrypt 之后
/// 一大批 DV 证书（GlobalSign Atlas 等）的中间证书用的就是这一档。
pub fn verify_pkcs1_sha384(n_be: &[u8], e_be: &[u8], msg: &[u8], sig: &[u8]) -> bool {
    verify_pkcs1(n_be, e_be, msg, sig, sha512::sha384, &SHA384_DIGEST_INFO)
}

/// PKCS#1 v1.5 + SHA-512（OID 1.2.840.113549.1.1.13）。
pub fn verify_pkcs1_sha512(n_be: &[u8], e_be: &[u8], msg: &[u8], sig: &[u8]) -> bool {
    verify_pkcs1(n_be, e_be, msg, sig, sha512::sha512, &SHA512_DIGEST_INFO)
}

/// MGF1（RFC 8017 B.2.3），摘要固定 SHA-256。
fn mgf1(seed: &[u8], out_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(out_len + 32);
    let mut counter = 0u32;
    while out.len() < out_len {
        let mut buf = Vec::with_capacity(seed.len() + 4);
        buf.extend_from_slice(seed);
        buf.extend_from_slice(&counter.to_be_bytes());
        out.extend_from_slice(&sha256::sha256(&buf));
        counter += 1;
    }
    out.truncate(out_len);
    out
}

/// RSASSA-PSS + SHA-256 + MGF1-SHA256，盐长固定 32（TLS 1.3 的要求）。
pub fn verify_pss_sha256(n_be: &[u8], e_be: &[u8], msg: &[u8], sig: &[u8]) -> bool {
    const H_LEN: usize = 32;
    const S_LEN: usize = 32;
    let k = key_bytes(n_be);
    let em_bits = bit_len(n_be) - 1;
    let em_len = (em_bits + 7) / 8;
    let Some(full) = rsa_public(n_be, e_be, sig) else {
        return false;
    };
    if em_len + 1 < k {
        // 模数最高字节的最高位是 1 时 emLen = k - 1，其余情况 emLen = k
        return false;
    }
    // 超出的那部分（最高位）必须是 0，否则 EM 超界
    if k > em_len && !full[..k - em_len].iter().all(|&b| b == 0) {
        return false;
    }
    let em = &full[k - em_len..];
    if em_len < H_LEN + S_LEN + 2 {
        return false;
    }
    if em[em_len - 1] != 0xbc {
        return false;
    }
    let h = &em[em_len - H_LEN - 1..em_len - 1];
    let masked = &em[..em_len - H_LEN - 1];
    // maskedDB 最左边 8·emLen − emBits 位必须为 0（注意：这一步在「异或解开前」的
    // maskedDB 上做。MGF1 的输出是满位的，解开之后那几个位本来就是废位，只需清掉）
    let zero_bits = 8 * em_len - em_bits;
    let forbid = if zero_bits > 0 {
        0xffu8 << (8 - zero_bits)
    } else {
        0
    };
    if masked[0] & forbid != 0 {
        return false;
    }
    let mut db = masked.to_vec();
    let mask = mgf1(h, db.len());
    for (d, m) in db.iter_mut().zip(mask.iter()) {
        *d ^= *m;
    }
    db[0] &= !forbid;
    // DB = PS(0x00…) || 0x01 || salt
    let ps_len = em_len - H_LEN - S_LEN - 2;
    if !db[..ps_len].iter().all(|&b| b == 0) {
        return false;
    }
    if db[ps_len] != 0x01 {
        return false;
    }
    let salt = &db[ps_len + 1..];
    if salt.len() != S_LEN {
        return false;
    }
    // H' = SHA256(00…00 || msgHash || salt)，8 字节的零前缀
    let msg_hash = sha256::sha256(msg);
    let mut m_prime = Vec::with_capacity(8 + 32 + 32);
    m_prime.extend_from_slice(&[0u8; 8]);
    m_prime.extend_from_slice(&msg_hash);
    m_prime.extend_from_slice(salt);
    sha256::sha256(&m_prime) == h
}


#[cfg(test)]
mod tests {
    use super::*;

    const N: &[u8] = include_bytes!("../../tests/fixtures/rsa_test_n.bin");
    const E: &[u8] = include_bytes!("../../tests/fixtures/rsa_test_e.bin");
    const V15: &[u8] = include_bytes!("../../tests/fixtures/rsa_v15_sig.bin");
    const PSS: &[u8] = include_bytes!("../../tests/fixtures/rsa_pss_sig.bin");

    const MSG: &[u8] = b"rsboot tls test message";
    const N384: &[u8] = include_bytes!("../../tests/fixtures/rsa_sha384_n.bin");
    const S384: &[u8] = include_bytes!("../../tests/fixtures/rsa_sha384_sig.bin");
    const REAL_N: &[u8] = include_bytes!("../../tests/fixtures/pss_real_n.bin");
    const REAL_MSG: &[u8] = include_bytes!("../../tests/fixtures/pss_real_msg.bin");
    const REAL_SIG: &[u8] = include_bytes!("../../tests/fixtures/pss_real_sig.bin");

    #[test]
    fn pkcs1_v15_from_openssl_signature() {
        // 向量由 python cryptography 用同一把私钥签出，非手写期望值
        assert!(verify_pkcs1_sha256(N, E, MSG, V15));
        assert!(!verify_pkcs1_sha256(N, E, b"another message", V15));
        // 改一个字节的签名必须失败
        let mut bad = V15.to_vec();
        bad[0] ^= 0x01;
        assert!(!verify_pkcs1_sha256(N, E, MSG, &bad));
    }

    #[test]
    fn pss_with_32_byte_salt() {
        assert!(verify_pss_sha256(N, E, MSG, PSS));
        assert!(!verify_pss_sha256(N, E, b"another message", PSS));
        let mut bad = PSS.to_vec();
        bad[100] ^= 0x80;
        assert!(!verify_pss_sha256(N, E, MSG, &bad));
    }

    #[test]
    fn wrong_exponent_is_rejected() {
        // e = 3 与 e = 65537 解出的 EM 完全不同
        assert!(!verify_pkcs1_sha256(N, &[3], MSG, V15));
        // 截断签名也必须拒绝，不能因为"长度不够"就当作有效
        assert!(!verify_pkcs1_sha256(N, E, MSG, &V15[..255]));
    }

    #[test]
    fn key_size_bits() {
        assert_eq!(key_bytes(N), 256);
        assert_eq!(bit_len(N), 2048);
        assert_eq!(bit_len(&[0x00, 0x80]), 8);
        assert_eq!(bit_len(&[0x01]), 1);
    }

    #[test]
    fn mgf1_is_deterministic_and_sized() {
        let a = mgf1(b"abc", 40);
        let b = mgf1(b"abc", 40);
        assert_eq!(a, b);
        assert_eq!(a.len(), 40);
        assert_ne!(mgf1(b"abc", 40), mgf1(b"abd", 40));
        // 前 32 字节应该就是 SHA256("abc" || 00000000)
        let mut buf = b"abc".to_vec();
        buf.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(&a[..32], &sha256::sha256(&buf)[..]);
    }

    /// 真实站点的 CertificateVerify 向量（2048 位模数、salt 32）。这条在
    /// maskedDB 左位检查写错位置时会失败：那几个高位必须在「异或解开前」的
    /// maskedDB 上检查。
    #[test]
    fn pss_real_world_certificate_verify_vector() {
        const E: &[u8] = &[0x01, 0x00, 0x01];
        assert!(verify_pss_sha256(REAL_N, E, REAL_MSG, REAL_SIG));
        // 换一位就必须失败
        let mut bad = REAL_SIG.to_vec();
        bad[0] ^= 0x01;
        assert!(!verify_pss_sha256(REAL_N, E, REAL_MSG, &bad));
    }

    /// sha384WithRSAEncryption（1.2.840.113549.1.1.12）：DV 中间证书常见。
    #[test]
    fn pkcs1_v15_sha384_from_openssl_signature() {
        const E: &[u8] = &[0x01, 0x00, 0x01];
        assert!(verify_pkcs1_sha384(N384, E, MSG, S384));
        // 摘要算法换了就验不过：同一份签名不能当 SHA-256 用
        assert!(!verify_pkcs1_sha256(N384, E, MSG, S384));
        assert!(!verify_pkcs1_sha384(N384, E, b"other", S384));
    }
}
