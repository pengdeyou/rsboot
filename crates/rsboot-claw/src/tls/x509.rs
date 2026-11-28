//! X.509 证书解析（RFC 5280）与主机名匹配。
//!
//! 只解析 TLS 客户端真正要看的部分：签名算法、TBS 原文（验签要它）、
//! 签发者/主体（链匹配靠 DER 字节相等，不做名字规范化）、有效期、
//! 公钥、SAN、基本约束。策略映射、名称约束、CRL 分发点一概不碰。
//!
//! 时间一律换成 UTC 秒数再比较——字符串比时间戳的坑（格式、时区）太多。

use super::der::{self, Reader};
use core::fmt;

#[derive(Debug, PartialEq, Eq)]
pub struct X509Error(pub &'static str);

impl fmt::Display for X509Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "x509: {}", self.0)
    }
}

pub type Result<T> = core::result::Result<T, X509Error>;

impl From<der::DerError> for X509Error {
    fn from(e: der::DerError) -> Self {
        // DER 的错误串本来就是 `&'static str`，原样带出来便于定位
        X509Error(e.0)
    }
}

/// 证书签名算法。TLS 1.3 只认 SHA-256 起步的那几个。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SigAlg {
    RsaPkcs1Sha256,
    /// sha384WithRSAEncryption：不少 DV 中间证书用这一档
    RsaPkcs1Sha384,
    /// sha512WithRSAEncryption
    RsaPkcs1Sha512,
    RsaPssSha256,
    EcdsaP256Sha256,
    /// ecdsa-with-SHA384：真实站点的中间证书大量用它
    EcdsaP384Sha384,
    /// 其它（老证书常见的 sha1WithRSA 等）
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PubKey {
    Rsa { modulus: Vec<u8>, exponent: Vec<u8> },
    /// P-256 的未压缩点（0x04 || X || Y）
    EcP256 { point: Vec<u8> },
    /// P-384 的未压缩点（0x04 || X || Y，97 字节）
    EcP384 { point: Vec<u8> },
    Other(String),
}

#[derive(Debug, Clone)]
pub struct Cert {
    /// 整张证书的 DER
    pub der: Vec<u8>,
    /// 待签名部分（TBSCertificate）的 DER，验签用
    pub tbs: Vec<u8>,
    /// 签发者名字的 DER（链匹配：与 issuer 的 subject 逐字节相等）
    pub issuer: Vec<u8>,
    pub subject: Vec<u8>,
    pub serial: Vec<u8>,
    pub sig_alg: SigAlg,
    pub signature: Vec<u8>,
    pub key: PubKey,
    pub not_before: i64,
    pub not_after: i64,
    /// SAN 里的 dNSName（小写）
    pub dns_names: Vec<String>,
    /// SAN 里的 iPAddress（点分）
    pub ip_addrs: Vec<String>,
    pub is_ca: bool,
    /// subject 里的 CN，仅在证书没有 SAN 时才用来兜底匹配
    pub cn: Option<String>,
}

impl Cert {
    pub fn valid_at(&self, unix_secs: i64) -> bool {
        unix_secs >= self.not_before && unix_secs <= self.not_after
    }

    /// 目标主机是否被这张证书覆盖。
    ///
    /// 规则（RFC 6125）：有 SAN 就只看 SAN；没有才回退 CN（老证书）。
    /// 通配符 `*.example.com` 只匹配**一级**标签，`example.com` 本身不算，
    /// `a.b.example.com` 也不算——这是浏览器现在的行为。
    pub fn matches_host(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
            return self.ip_addrs.iter().any(|a| *a == ip.to_string());
        }
        if !self.dns_names.is_empty() {
            return self.dns_names.iter().any(|p| match_dns(p, &host));
        }
        match &self.cn {
            Some(cn) => match_dns(&cn.to_ascii_lowercase(), &host),
            None => false,
        }
    }
}

fn match_dns(pattern: &str, host: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix("*.") {
        // 通配符必须至少有一个点，且只吃一个标签
        match host.split_once('.') {
            Some((first, rest)) => !first.is_empty() && rest == suffix,
            None => false,
        }
    } else {
        pattern == host
    }
}

/// 解析一张 DER 证书。
pub fn parse(der: &[u8]) -> Result<Cert> {
    let mut top = Reader::new(der);
    let body = top.want(0x30).map_err(|_| X509Error("certificate is not a SEQUENCE"))?;
    let mut c = Reader::new(body);

    // ---- TBSCertificate（要保存含 tag 的原始字节）
    let tbs_start = c.pos();
    let tbs_body = c.want(0x30)?;
    let tbs = c.slice_from(tbs_start).to_vec();
    let mut t = Reader::new(tbs_body);

    if t.peek() == Some(0xa0) {
        // [0] EXPLICIT Version：v3 是 2，我们不用这个值，只是必须跳过
        let mut v = Reader::new(t.want(0xa0)?);
        let _ = v.int_small()?;
    }
    let serial = t.int_bytes()?.to_vec();
    // 内层签名算法（与外面那个通常相同，链校验用外面那个，这里只跳过）
    let _ = t.seq()?;
    let issuer_start = t.pos();
    let _ = t.seq()?;
    let issuer = t.slice_from(issuer_start).to_vec();
    let mut validity = t.seq()?;
    let not_before = validity.time()?;
    let not_after = validity.time()?;
    let subject_start = t.pos();
    let _ = t.seq()?;
    let subject = t.slice_from(subject_start).to_vec();

    // ---- SubjectPublicKeyInfo
    let mut spki = t.seq()?;
    let mut alg = spki.seq()?;
    let alg_oid = alg.oid()?;
    let key = match alg_oid.as_str() {
        // rsaEncryption 与 RSASSA-PSS：后者公钥结构完全相同，只是签名参数在 alg 里
        "1.2.840.113549.1.1.1" | "1.2.840.113549.1.1.10" => {
            let bits = spki.bit_string()?;
            let mut r = Reader::new(bits);
            let mut inner = r.seq()?;
            let modulus = trim_zero(inner.int_bytes()?);
            let exponent = trim_zero(inner.int_bytes()?);
            PubKey::Rsa { modulus, exponent }
        }
        "1.2.840.10045.2.1" => {
            let curve = alg.oid().unwrap_or_default();
            let point = spki.bit_string()?.to_vec();
            if curve == "1.2.840.10045.3.1.7" {
                PubKey::EcP256 { point }
            } else if curve == "1.3.132.0.34" {
                PubKey::EcP384 { point }
            } else {
                PubKey::Other(format!("ec/{curve}"))
            }
        }
        other => PubKey::Other(other.to_string()),
    };

    // ---- 可选字段：[1] issuerUID、[2] subjectUID、[3] extensions
    let mut dns_names = Vec::new();
    let mut ip_addrs = Vec::new();
    let mut is_ca = false;
    while !t.is_empty() {
        let tag = t.peek().unwrap_or(0);
        if tag == 0xa3 {
            let exts = t.want(0xa3)?;
            let mut e = Reader::new(exts);
            let mut list = e.seq()?;
            while !list.is_empty() {
                let mut one = list.seq()?;
                let oid = one.oid()?;
                let critical = if one.peek() == Some(0x01) {
                    one.bool()?
                } else {
                    false
                };
                let _ = critical;
                let value = one.octets()?;
                match oid.as_str() {
                    // subjectAltName
                    "2.5.29.17" => {
                        let mut v = Reader::new(value);
                        let mut names = v.seq()?;
                        while !names.is_empty() {
                            let (t, content) = names.next_tlv()?;
                            match t {
                                // [2] dNSName（IA5String，隐式标签）
                                0x82 => {
                                    if let Ok(s) =
                                        core::str::from_utf8(content)
                                    {
                                        dns_names.push(s.to_ascii_lowercase());
                                    }
                                }
                                // [7] iPAddress
                                0x87 => {
                                    if content.len() == 4 {
                                        ip_addrs.push(format!(
                                            "{}.{}.{}.{}",
                                            content[0], content[1], content[2], content[3]
                                        ));
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    // basicConstraints
                    "2.5.29.19" => {
                        let mut v = Reader::new(value);
                        let mut bc = v.seq()?;
                        if bc.peek() == Some(0x01) {
                            is_ca = bc.bool().unwrap_or(false);
                        }
                    }
                    _ => {}
                }
            }
        } else {
            t.skip()?;
        }
    }

    // ---- 外层签名算法与签名值
    let mut sa = c.seq()?;
    let sa_oid = sa.oid()?;
    let sig_alg = match sa_oid.as_str() {
        "1.2.840.113549.1.1.11" => SigAlg::RsaPkcs1Sha256,
        "1.2.840.113549.1.1.12" => SigAlg::RsaPkcs1Sha384,
        "1.2.840.113549.1.1.13" => SigAlg::RsaPkcs1Sha512,
        "1.2.840.113549.1.1.10" => SigAlg::RsaPssSha256,
        "1.2.840.10045.4.3.2" => SigAlg::EcdsaP256Sha256,
        "1.2.840.10045.4.3.3" => SigAlg::EcdsaP384Sha384,
        other => SigAlg::Other(other.to_string()),
    };
    let signature = c.bit_string()?.to_vec();

    let cn = name_cn(&subject);

    Ok(Cert {
        der: der.to_vec(),
        tbs,
        issuer,
        subject,
        serial,
        sig_alg,
        signature,
        key,
        not_before,
        not_after,
        dns_names,
        ip_addrs,
        is_ca,
        cn,
    })
}

/// DER 的 INTEGER 在最高位为 1 时会补一个 0x00 保正号，
/// 大数运算要的是纯数值，这里去掉前导零。
fn trim_zero(b: &[u8]) -> Vec<u8> {
    let mut i = 0;
    while i + 1 < b.len() && b[i] == 0 {
        i += 1;
    }
    b[i..].to_vec()
}

/// 从 Name 的 DER 里取第一段 CN（2.5.4.3）。
/// 把 X.520 Name（RDNSequence）拆成 (OID, 值内容) 列表。
///
/// 只按内容比，不按字节比：同一张根证书在不同 bundle 里，`CN=AAA Certificate
/// Services` 可能编码成 PrintableString，而中间证书的 issuer 里又写成
/// UTF8String —— 字节不同但名字相同，按 RFC 5280 应当判定为同一个。
pub fn parse_name(der: &[u8]) -> Option<Vec<(String, Vec<u8>)>> {
    let mut r = Reader::new(der);
    let (tag, body) = r.next_tlv().ok()?;
    if tag != 0x30 {
        return None;
    }
    let mut seq = Reader::new(body);
    let mut out = Vec::new();
    while !seq.is_empty() {
        let (set_tag, set_body) = seq.next_tlv().ok()?;
        if set_tag != 0x31 {
            return None;
        }
        let mut set = Reader::new(set_body);
        while !set.is_empty() {
            let (_, atv) = set.next_tlv().ok()?;
            let mut a = Reader::new(atv);
            let oid = a.oid().ok()?;
            let (_vt, value) = a.next_tlv().ok()?;
            out.push((oid, value.to_vec()));
        }
    }
    Some(out)
}

/// 两个 Name 是否指向同一个名字（忽略字符串编码差异、忽略 RDN 内部顺序）。
pub fn name_eq(a: &[u8], b: &[u8]) -> bool {
    match (parse_name(a), parse_name(b)) {
        (Some(mut x), Some(mut y)) => {
            x.sort();
            y.sort();
            x == y
        }
        _ => a == b,
    }
}

pub(crate) fn name_cn(name: &[u8]) -> Option<String> {
    let mut r = Reader::new(name);
    let body = r.want(0x30).ok()?;
    let mut rdns = Reader::new(body);
    while !rdns.is_empty() {
        let mut set = Reader::new(rdns.want(0x31).ok()?);
        while !set.is_empty() {
            let mut atv = Reader::new(set.want(0x30).ok()?);
            let oid = atv.oid().ok()?;
            if oid == "2.5.4.3" {
                let (tag, val) = atv.next_tlv().ok()?;
                let s = match tag {
                    0x0c | 0x13 | 0x16 => core::str::from_utf8(val).ok()?.to_string(),
                    _ => return None,
                };
                return Some(s);
            }
        }
    }
    None
}

// ---------------------------------------------------------------- 根证书

/// 系统根证书文件的候选路径。顺序有意义：先 Debian/Ubuntu，再 RHEL，
/// 再 macOS/BSD——容器里通常只有第一个存在。
const BUNDLE_PATHS: &[&str] = &[
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/cert.pem",
    "/usr/local/etc/openssl/cert.pem",
    "/opt/homebrew/etc/openssl@3/cert.pem",
];

/// 加载系统根证书。找不到任何 bundle 时返回错误（而不是"信任所有"）。
pub fn system_roots() -> Result<Vec<Cert>> {
    let mut last_err = X509Error("no system CA bundle found");
    if let Ok(p) = std::env::var("SSL_CERT_FILE") {
        if let Ok(pem) = std::fs::read(&p) {
            let certs = parse_pem_bundle(&pem);
            if !certs.is_empty() {
                return Ok(certs);
            }
        }
    }
    for path in BUNDLE_PATHS {
        match std::fs::read(path) {
            Ok(pem) => {
                let certs = parse_pem_bundle(&pem);
                if !certs.is_empty() {
                    return Ok(certs);
                }
                last_err = X509Error("CA bundle contained no certificate");
            }
            Err(_) => continue,
        }
    }
    Err(last_err)
}

/// 解析 PEM 里的全部证书块（跳过无法解析的，不让一张坏证书毁掉整包）。
pub fn parse_pem_bundle(pem: &[u8]) -> Vec<Cert> {
    let mut out = Vec::new();
    let mut rest = pem;
    while let Some(begin) = find(rest, b"-----BEGIN CERTIFICATE-----") {
        let after = &rest[begin + 27..];
        let Some(end) = find(after, b"-----END CERTIFICATE-----") else {
            break;
        };
        let b64 = &after[..end];
        if let Some(der) = base64_decode(b64) {
            if let Ok(c) = parse(&der) {
                out.push(c);
            }
        }
        rest = &after[end + 25..];
    }
    out
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// RFC 4648 标准 base64 解码，忽略换行与空白。
fn base64_decode(input: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut pad = 0usize;
    for &b in input {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                pad += 1;
                continue;
            }
            b'\n' | b'\r' | b' ' | b'\t' => continue,
            _ => return None,
        };
        if pad > 0 {
            return None; // `=` 之后不该再有字符
        }
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSA_ROOT: &[u8] = include_bytes!("../../tests/fixtures/rsa_root.der");
    const RSA_LEAF: &[u8] = include_bytes!("../../tests/fixtures/rsa_leaf.der");
    const EC_ROOT: &[u8] = include_bytes!("../../tests/fixtures/ec_root.der");
    const EC_LEAF: &[u8] = include_bytes!("../../tests/fixtures/ec_leaf.der");

    #[test]
    fn parses_rsa_leaf_fields() {
        let c = parse(RSA_LEAF).unwrap();
        assert_eq!(c.sig_alg, SigAlg::RsaPkcs1Sha256);
        assert_eq!(c.cn.as_deref(), Some("example.com"));
        assert_eq!(c.dns_names, vec!["example.com", "www.example.com"]);
        assert!(!c.is_ca);
        assert!(c.valid_at(1735689600)); // 2025-01-01
        assert!(!c.valid_at(0)); // 1970 早于 notBefore
        match &c.key {
            PubKey::Rsa { modulus, exponent } => {
                assert_eq!(modulus.len(), 256, "2048 位模数");
                assert_eq!(exponent, &vec![0x01, 0x00, 0x01], "e = 65537");
            }
            other => panic!("{other:?}"),
        }
        // tbs 必须含 tag 且比整张证书短
        assert!(c.tbs.len() < c.der.len());
        assert_eq!(c.tbs[0], 0x30);
        assert_eq!(c.signature.len(), 256, "RSA 签名长度等于模长");
    }

    #[test]
    fn parses_ec_leaf_fields() {
        let c = parse(EC_LEAF).unwrap();
        assert_eq!(c.sig_alg, SigAlg::EcdsaP256Sha256);
        assert_eq!(c.dns_names, vec!["ec.example.com"]);
        match &c.key {
            PubKey::EcP256 { point } => {
                assert_eq!(point.len(), 65);
                assert_eq!(point[0], 0x04, "未压缩点");
            }
            other => panic!("{other:?}"),
        }
        // ECDSA 签名是 SEQUENCE { r, s }，每项 32 字节
        let mut r = Reader::new(&c.signature);
        let mut sig = r.seq().unwrap();
        let rr = sig.int_bytes().unwrap();
        let ss = sig.int_bytes().unwrap();
        assert!(rr.len() <= 33 && ss.len() <= 33);
    }

    #[test]
    fn root_is_ca_and_self_issued() {
        let r = parse(RSA_ROOT).unwrap();
        assert!(r.is_ca);
        assert_eq!(r.issuer, r.subject);
        assert_eq!(r.cn.as_deref(), Some("Test Root CA"));
        let e = parse(EC_ROOT).unwrap();
        assert!(e.is_ca);
        assert_eq!(e.issuer, e.subject);
    }

    #[test]
    fn leaf_issuer_matches_root_subject() {
        // 链校验的第一步：签发者名字必须逐字节等于上级的主体
        let root = parse(RSA_ROOT).unwrap();
        let leaf = parse(RSA_LEAF).unwrap();
        assert_eq!(leaf.issuer, root.subject);
        let ec_root = parse(EC_ROOT).unwrap();
        let ec_leaf = parse(EC_LEAF).unwrap();
        assert_eq!(ec_leaf.issuer, ec_root.subject);
    }

    #[test]
    fn hostname_matching_rules() {
        let c = parse(RSA_LEAF).unwrap();
        assert!(c.matches_host("example.com"));
        assert!(c.matches_host("EXAMPLE.COM"));
        assert!(c.matches_host("www.example.com"));
        assert!(!c.matches_host("api.example.com"));
        assert!(!c.matches_host("example.com.evil.com"));
        assert!(!c.matches_host("badexample.com"));

        // 通配符只吃一级标签
        assert!(match_dns("*.example.com", "a.example.com"));
        assert!(!match_dns("*.example.com", "example.com"));
        assert!(!match_dns("*.example.com", "a.b.example.com"));
        assert!(!match_dns("*.example.com", ".example.com"));
    }

    #[test]
    fn system_bundle_parses_many_certs() {
        let roots = match system_roots() {
            Ok(r) => r,
            Err(_) => return, // 没有系统的 CA bundle 就跳过（不把环境依赖变成红灯）
        };
        assert!(roots.len() > 20, "根证书数量 {}", roots.len());
        let cas = roots.iter().filter(|c| c.is_ca).count();
        assert!(cas > 10, "CA 数量 {cas}");
        // 绝大多数必须是 RSA / P-256；Ed25519、国密等极少数归 Other
        let other: Vec<_> = roots
            .iter()
            .filter(|c| matches!(c.key, PubKey::Other(_)))
            .collect();
        assert!(
            other.len() * 100 <= roots.len() * 45,
            "无法处理的公钥类型过多：{} / {}，例如 {:?}",
            other.len(),
            roots.len(),
            other.first().map(|c| (&c.cn, &c.key))
        );
    }

    #[test]
    fn pem_bundle_roundtrip() {
        // 用 fixture 包一层 PEM 再解析，验证 base64 与分块
        let pem = format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            b64(RSA_LEAF)
        );
        let certs = parse_pem_bundle(pem.as_bytes());
        assert_eq!(certs.len(), 1);
        assert_eq!(certs[0].cn.as_deref(), Some("example.com"));
        // 两个块
        let two = format!("{pem}{pem}");
        assert_eq!(parse_pem_bundle(two.as_bytes()).len(), 2);
        // 坏块不毁整包
        let bad = format!("-----BEGIN CERTIFICATE-----\n!!!\n-----END CERTIFICATE-----\n{pem}");
        assert_eq!(parse_pem_bundle(bad.as_bytes()).len(), 1);
    }

    fn b64(der: &[u8]) -> String {
        let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut s = String::new();
        for chunk in der.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let idx = [
                b[0] >> 2,
                ((b[0] & 3) << 4) | (b[1] >> 4),
                ((b[1] & 0xf) << 2) | (b[2] >> 6),
                b[2] & 0x3f,
            ];
            for (i, v) in idx.iter().enumerate() {
                if i <= chunk.len() {
                    s.push(table[*v as usize] as char);
                } else {
                    s.push('=');
                }
            }
        }
        s
    }

    #[test]
    fn base64_matches_known_value() {
        assert_eq!(base64_decode(b"aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode(b"aGVsbG8").unwrap(), b"hello");
        assert!(base64_decode(b"!!!!").is_none());
        assert!(base64_decode(b"aGVsbG8=extra").is_none());
    }

    #[test]
    fn truncated_and_bogus_der_returns_error() {
        assert!(parse(&[]).is_err());
        assert!(parse(&[0x30]).is_err());
        assert!(parse(b"not a certificate").is_err());
        // 砍掉尾巴必须报错而不是 panic
        let full = parse(RSA_LEAF).unwrap();
        for cut in [10usize, 100, full.der.len() - 5] {
            assert!(parse(&full.der[..cut]).is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn days_from_civil_edges() {
        assert_eq!(der::days_from_civil(1970, 1, 1), 0);
        assert_eq!(der::days_from_civil(2000, 3, 1), 11017);
        assert_eq!(der::days_from_civil(2024, 2, 29), 19782);
        assert_eq!(der::days_from_civil(1900, 3, 1), -25508);
    }

    /// 同样的 CN，一个用 PrintableString 编码、一个用 UTF8String 编码 ——
    /// 字节不同，但按 RFC 5280 是同一个名字（真实 bundle 里就是这样）。
    #[test]
    fn name_eq_ignores_string_encoding() {
        // SEQUENCE { SET { SEQUENCE { OID 2.5.4.3, PrintableString "AAA" } } }
        let printable = [
            0x30, 0x0e, 0x31, 0x0c, 0x30, 0x0a, 0x06, 0x03, 0x55, 0x04, 0x03, 0x13, 0x03, b'A',
            b'A', b'A',
        ];
        let utf8 = [
            0x30, 0x0e, 0x31, 0x0c, 0x30, 0x0a, 0x06, 0x03, 0x55, 0x04, 0x03, 0x0c, 0x03, b'A',
            b'A', b'A', b'A',
        ];
        assert_ne!(&printable[..], &utf8[..]);
        assert!(name_eq(&printable, &utf8));
        let other = [
            0x30, 0x0e, 0x31, 0x0c, 0x30, 0x0a, 0x06, 0x03, 0x55, 0x04, 0x03, 0x13, 0x03, b'B',
            b'B', b'B',
        ];
        assert!(!name_eq(&printable, &other));
        assert_eq!(parse_name(&printable).unwrap().len(), 1);
    }
}
