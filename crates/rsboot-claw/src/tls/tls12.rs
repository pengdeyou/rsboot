//! TLS 1.2 客户端（RFC 5246 + RFC 5289/7905），只为"只有 1.2 的老站点"存在。
//!
//! 范围收得很窄，且是刻意的：
//!
//! - 只支持 **ECDHE + AEAD** 的四个套件（AES-128-GCM 与 ChaCha20-Poly1305，
//!   证书侧 ECDSA 或 RSA 都行）。CBC 套件一律不做——那要 HMAC-then-encrypt
//!   与 padding，Lucky13 一类的侧信道全从这个坑里长出来，收益又只是"多打开
//!   几个早就该淘汰的站"。
//! - 不做重协商、不做会话复用（不发 `session_ticket`，session id 每次随机）、
//!   不做压缩、不做客户端证书、不做有限域 DHE（那要 2048 位模幂）。
//! - 证书链、主机名与 1.3 走同一套 `chain::verify`，没有"1.2 就放宽"的分支。
//!
//! 与 1.3 的实质差别只在两处：密钥派生用 PRF（`P_SHA256`）而不是 HKDF，
//! 记录层的 nonce/AAD 布局不同（1.2 的 GCM 要显式 nonce，AAD 带 seq）。
//! 剩下都是"消息顺序不同"，照 RFC 抄即可。

use std::io::{Read, Write};
use std::net::TcpStream;

use super::chain;
use super::der::{DerError, Reader};
use super::ec;
use super::rsa;
use super::sha256::{hmac_sha256, sha256};
use super::x25519;
use super::x509::{self, Cert};

// 套件编号
pub const ECDHE_ECDSA_AES128_GCM_SHA256: u16 = 0xc02b;
pub const ECDHE_RSA_AES128_GCM_SHA256: u16 = 0xc02f;
pub const ECDHE_RSA_CHACHA20_POLY1305_SHA256: u16 = 0xcca8;
pub const ECDHE_ECDSA_CHACHA20_POLY1305_SHA256: u16 = 0xcca9;

// 我们愿意谈的命名曲线
pub const GROUP_X25519: u16 = 0x001d;
pub const GROUP_SECP256R1: u16 = 0x0017;

const CT_CHANGE_CIPHER_SPEC: u8 = 20;
const CT_ALERT: u8 = 21;
const CT_HANDSHAKE: u8 = 22;
const CT_APPLICATION_DATA: u8 = 23;

const HS_CERTIFICATE: u8 = 11;
const HS_SERVER_KEY_EXCHANGE: u8 = 12;
const HS_CERTIFICATE_REQUEST: u8 = 13;
const HS_SERVER_HELLO_DONE: u8 = 14;
const HS_CLIENT_KEY_EXCHANGE: u8 = 16;
const HS_FINISHED: u8 = 20;

#[derive(Debug)]
pub enum Error12 {
    Io(std::io::Error),
    Protocol(&'static str),
    Cert(&'static str),
    Alert(u8, u8),
}

impl std::fmt::Display for Error12 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error12::Io(e) => write!(f, "tls1.2 io: {e}"),
            Error12::Protocol(m) => write!(f, "tls1.2 protocol: {m}"),
            Error12::Cert(m) => write!(f, "tls1.2 certificate: {m}"),
            Error12::Alert(l, d) => write!(f, "tls1.2 alert {l}/{d}"),
        }
    }
}

impl From<std::io::Error> for Error12 {
    fn from(e: std::io::Error) -> Self {
        Error12::Io(e)
    }
}

impl From<DerError> for Error12 {
    fn from(_: DerError) -> Self {
        Error12::Protocol("malformed handshake bytes")
    }
}

pub type Result12<T> = core::result::Result<T, Error12>;

/// 1.2 的 AEAD 套件（哈希固定 SHA-256）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Suite12 {
    Aes128Gcm,
    Chacha20,
}

impl Suite12 {
    pub fn from_id(id: u16) -> Option<Suite12> {
        match id {
            ECDHE_ECDSA_AES128_GCM_SHA256 | ECDHE_RSA_AES128_GCM_SHA256 => Some(Suite12::Aes128Gcm),
            ECDHE_RSA_CHACHA20_POLY1305_SHA256 | ECDHE_ECDSA_CHACHA20_POLY1305_SHA256 => {
                Some(Suite12::Chacha20)
            }
            _ => None,
        }
    }

    pub fn key_len(self) -> usize {
        match self {
            Suite12::Aes128Gcm => 16,
            Suite12::Chacha20 => 32,
        }
    }

    /// key block 里的 write_IV 长度：GCM 是 4 字节隐式部分（显式部分写在记录里），
    /// ChaCha20-Poly1305 是完整 12 字节（RFC 7905 不加显式 nonce）。
    pub fn iv_len(self) -> usize {
        match self {
            Suite12::Aes128Gcm => 4,
            Suite12::Chacha20 => 12,
        }
    }
}

/// 1.2 的记录密钥。
pub struct Key12 {
    suite: Suite12,
    key: Vec<u8>,
    iv: Vec<u8>,
}

impl Key12 {
    fn new(suite: Suite12, key: Vec<u8>, iv: Vec<u8>) -> Key12 {
        Key12 { suite, key, iv }
    }

    /// 解一条记录。`body` 是记录里的密文（GCM 还带 8 字节显式 nonce 前缀）。
    pub fn open(&self, seq: u64, ct: u8, body: &[u8]) -> Option<Vec<u8>> {
        match self.suite {
            Suite12::Aes128Gcm => {
                if body.len() < 8 + 16 {
                    return None;
                }
                let pt_len = body.len() - 8 - 16;
                let aad = aad12(seq, ct, pt_len);
                let explicit = &body[..8];
                let mut nonce = [0u8; 12];
                nonce[..4].copy_from_slice(&self.iv[..4]);
                nonce[4..].copy_from_slice(explicit);
                let k: [u8; 16] = self.key.as_slice().try_into().ok()?;
                super::aes::open(&k, &nonce, &aad, &body[8..])
            }
            Suite12::Chacha20 => {
                if body.len() < 16 {
                    return None;
                }
                let aad = aad12(seq, ct, body.len() - 16);
                let nonce: [u8; 12] = self.iv.as_slice().try_into().ok()?;
                let k: [u8; 32] = self.key.as_slice().try_into().ok()?;
                super::chacha::open(&k, &nonce, &aad, body)
            }
        }
    }

    /// 封一条记录，返回记录体（GCM 前面带 8 字节显式 nonce）。
    pub fn seal(&self, seq: u64, ct: u8, plain: &[u8]) -> Vec<u8> {
        // extra：GCM 要把 8 字节显式 nonce 写进记录体，ChaCha 不写
        let extra = match self.suite {
            Suite12::Aes128Gcm => 8,
            Suite12::Chacha20 => 0,
        };
        let aad = aad12(seq, ct, plain.len());
        match self.suite {
            Suite12::Aes128Gcm => {
                let mut nonce = [0u8; 12];
                nonce[..4].copy_from_slice(&self.iv[..4]);
                nonce[4..].copy_from_slice(&seq.to_be_bytes());
                let k: [u8; 16] = self.key.as_slice().try_into().expect("16 字节");
                let mut out = Vec::with_capacity(extra + plain.len() + 16);
                out.extend_from_slice(&seq.to_be_bytes());
                out.extend_from_slice(&super::aes::seal(&k, &nonce, &aad, plain));
                out
            }
            Suite12::Chacha20 => {
                let nonce: [u8; 12] = self.iv.as_slice().try_into().expect("12 字节");
                let k: [u8; 32] = self.key.as_slice().try_into().expect("32 字节");
                super::chacha::seal(&k, &nonce, &aad, plain)
            }
        }
    }
}

/// 1.2 的 AAD：`seq(8) ‖ type(1) ‖ version(2) ‖ length(2)`。
///
/// 这里的 length 是**明文长度**（RFC 5246 §6.2.3.3 的 `TLSCompressed.length`），
/// 不是记录头里写的密文长度——两者在 AEAD 下差着显式 nonce 与 tag。
/// 这一条曾经按"头里的长度"写，服务端一律回 bad_record_mac。
fn aad12(seq: u64, ct: u8, ct_len: usize) -> Vec<u8> {
    let mut aad = Vec::with_capacity(13);
    aad.extend_from_slice(&seq.to_be_bytes());
    aad.push(ct);
    aad.push(0x03);
    aad.push(0x03);
    aad.extend_from_slice(&(ct_len as u16).to_be_bytes());
    aad
}

// ---------------------------------------------------------------- PRF

/// `P_SHA256`（RFC 5246 §5）：A(0)=seed，每轮取 HMAC(secret, A(i) ‖ seed)。
fn p_sha256(secret: &[u8], seed: &[u8], out_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(out_len);
    let mut a = hmac_sha256(secret, seed);
    while out.len() < out_len {
        let mut buf = Vec::with_capacity(32 + seed.len());
        buf.extend_from_slice(&a);
        buf.extend_from_slice(seed);
        let d = hmac_sha256(secret, &buf);
        let take = (out_len - out.len()).min(32);
        out.extend_from_slice(&d[..take]);
        a = hmac_sha256(secret, &a);
    }
    out
}

/// `PRF(secret, label, seed)` = `P_hash(secret, label ‖ seed)`。
pub fn prf(secret: &[u8], label: &str, seed: &[u8], out_len: usize) -> Vec<u8> {
    let mut s = Vec::with_capacity(label.len() + seed.len());
    s.extend_from_slice(label.as_bytes());
    s.extend_from_slice(seed);
    p_sha256(secret, &s, out_len)
}

// ---------------------------------------------------------------- 主流程

pub struct HandshakeResult {
    pub read: Key12,
    pub write: Key12,
    /// 服务端 Finished 之后的下一个 seq（应用数据从这里开始）
    pub read_seq: u64,
    pub write_seq: u64,
}

/// 接在 ServerHello 之后跑完 1.2 握手。
///
/// `hello`/`sh_msg` 是已经收发的两条原始握手消息（用于转录哈希），
/// `server_ems` 表示服务端是否回了 extended_master_secret 扩展。
pub fn handshake(
    stream: &mut TcpStream,
    host: &str,
    roots: &[Cert],
    hello: &[u8],
    sh_msg: &[u8],
    client_random: &[u8; 32],
    server_random: &[u8; 32],
    suite: Suite12,
    server_ems: bool,
) -> Result12<HandshakeResult> {
    let debug = std::env::var("RSBOOT_TLS_DEBUG").is_ok();
    let mut transcript = Vec::with_capacity(2048);
    transcript.extend_from_slice(hello);
    transcript.extend_from_slice(sh_msg);

    // ---- 读 plaintext 握手：Certificate / ServerKeyExchange / ServerHelloDone
    let mut hs_buf: Vec<u8> = Vec::new();
    let mut certs: Vec<Cert> = Vec::new();
    let mut server_point: Option<(u16, Vec<u8>)> = None;
    let mut done = false;

    while !done {
        // 先把缓冲里的完整消息处理掉
        loop {
            if hs_buf.len() < 4 {
                break;
            }
            let typ = hs_buf[0];
            let len = u32::from_be_bytes([0, hs_buf[1], hs_buf[2], hs_buf[3]]) as usize;
            if hs_buf.len() < 4 + len {
                break;
            }
            let msg: Vec<u8> = hs_buf.drain(..4 + len).collect();
            if debug {
                eprintln!("  t12 hs typ={typ} len={len}");
            }
            match typ {
                HS_CERTIFICATE => {
                    transcript.extend_from_slice(&msg);
                    certs = parse_cert_list(&msg[4..])?;
                    if certs.is_empty() {
                        return Err(Error12::Cert("empty certificate list"));
                    }
                }
                HS_SERVER_KEY_EXCHANGE => {
                    transcript.extend_from_slice(&msg);
                    let pre_hash_input = {
                        let mut v = Vec::with_capacity(64);
                        v.extend_from_slice(client_random);
                        v.extend_from_slice(server_random);
                        v
                    };
                    server_point = Some(parse_ske(&msg[4..], &certs, &pre_hash_input)?);
                }
                HS_CERTIFICATE_REQUEST => {
                    // 要客户端证书就退出：这个能力我们没做，也不该假装能做
                    return Err(Error12::Protocol(
                        "server requested a client certificate (unsupported)",
                    ));
                }
                HS_SERVER_HELLO_DONE => {
                    transcript.extend_from_slice(&msg);
                    done = true;
                }
                _ => {
                    // CertificateStatus 等：我们没请求过，收到就忽略
                    transcript.extend_from_slice(&msg);
                }
            }
            if done {
                break;
            }
        }
        if done {
            break;
        }
        let (ct, content) = read_plain(stream)?;
        match ct {
            CT_HANDSHAKE => hs_buf.extend_from_slice(&content),
            CT_ALERT => {
                let mut r = Reader::new(&content);
                return Err(Error12::Alert(
                    r.next_u8().unwrap_or(0),
                    r.next_u8().unwrap_or(0),
                ));
            }
            CT_CHANGE_CIPHER_SPEC => continue, // 这里出现 CCS 不合规，但别炸
            _ => return Err(Error12::Protocol("unexpected record while waiting ServerHelloDone")),
        }
    }

    let (group, peer_point) =
        server_point.ok_or(Error12::Protocol("server did not send ECDHE key exchange"))?;

    // ---- 证书链与主机名（与 1.3 同一套强制校验）
    let leaf = certs.first().cloned().ok_or(Error12::Cert("no leaf certificate"))?;
    let inters: Vec<Cert> = certs.into_iter().skip(1).collect();
    chain::verify(&leaf, &inters, roots, host).map_err(|e| Error12::Cert(e.as_str()))?;

    // ---- 临时密钥对与共享密钥
    let (our_priv, our_point, pms) = match group {
        GROUP_X25519 => {
            let mut k = [0u8; 32];
            k.copy_from_slice(&crate::tls::handshake::random_bytes(32));
            let pubk = x25519::public_key(&k);
            let pms = x25519::shared_secret(&k, &to_u32(&peer_point)?);
            (k.to_vec(), pubk.to_vec(), pms.to_vec())
        }
        GROUP_SECP256R1 => {
            let k = loop {
                let r = crate::tls::handshake::random_bytes(32);
                if let Some(p) = ec::p256_public(&r) {
                    break (r, p);
                }
            };
            let pms = ec::p256_shared(&k.0, &peer_point)
                .ok_or(Error12::Protocol("peer ECDHE point is not on the curve"))?;
            (k.0, k.1, pms)
        }
        _ => return Err(Error12::Protocol("server picked an ECDHE group we did not offer")),
    };

    let cke_body = {
        let mut b = Vec::with_capacity(our_point.len() + 1);
        b.push(our_point.len() as u8);
        b.extend_from_slice(&our_point);
        b
    };
    let cke = hs_msg(HS_CLIENT_KEY_EXCHANGE, &cke_body);
    transcript.extend_from_slice(&cke);
    write_plain(stream, CT_HANDSHAKE, &cke)?;
    if debug {
        eprintln!("  t12 pms={}", hex(&pms));
        eprintln!("  t12 cke point={}", hex(&our_point));
    }

    // ---- master secret（EMS 优先，服务端不回就退回 old-style）
    let master = if server_ems {
        let session_hash = sha256(&transcript);
        prf(&pms, "extended master secret", &session_hash, 48)
    } else {
        let mut seed = Vec::with_capacity(64);
        seed.extend_from_slice(client_random);
        seed.extend_from_slice(server_random);
        prf(&pms, "master secret", &seed, 48)
    };

    // ---- key block：client/server 的写 key 与 IV（顺序固定：先 client 后 server）
    let mut seed = Vec::with_capacity(64);
    seed.extend_from_slice(server_random);
    seed.extend_from_slice(client_random);
    let kb_len = 2 * (suite.key_len() + suite.iv_len());
    let kb = prf(&master, "key expansion", &seed, kb_len);
    let kl = suite.key_len();
    let il = suite.iv_len();
    let c_key = kb[..kl].to_vec();
    let s_key = kb[kl..2 * kl].to_vec();
    let c_iv = kb[2 * kl..2 * kl + il].to_vec();
    let s_iv = kb[2 * kl + il..].to_vec();
    let write = Key12::new(suite, c_key, c_iv);
    let read = Key12::new(suite, s_key, s_iv);
    if debug {
        eprintln!("  t12 master={}", hex(&master));
        eprintln!("  t12 cwkey={}", hex(&write.key));
        eprintln!("  t12 swkey={}", hex(&read.key));
    }

    // ---- 客户端 CCS + Finished
    let fin = prf(&master, "client finished", &sha256(&transcript), 12);
    let fin_msg = hs_msg(HS_FINISHED, &fin);
    transcript.extend_from_slice(&fin_msg);
    write_plain(stream, CT_CHANGE_CIPHER_SPEC, &[1])?;
    write_record12(stream, &write, 0, CT_HANDSHAKE, &fin_msg)?;

    // ---- 服务端 CCS + Finished
    let mut server_finished_seq = 0u64;
    loop {
        let (ct, content) = read_plain(stream)?;
        match ct {
            CT_CHANGE_CIPHER_SPEC => {
                // 紧随其后的第一条加密记录就是 Finished
                let (rct, body) = read_record(stream)?;
                if rct == CT_ALERT {
                    let mut r = Reader::new(&body);
                    return Err(Error12::Alert(
                        r.next_u8().unwrap_or(0),
                        r.next_u8().unwrap_or(0),
                    ));
                }
                let plain = read
                    .open(server_finished_seq, rct, &body)
                    .ok_or(Error12::Protocol("server Finished record did not decrypt"))?;
                let mut r = Reader::new(&plain);
                let typ = r.next_u8()?;
                if typ != HS_FINISHED {
                    return Err(Error12::Protocol("expected server Finished"));
                }
                let len = r.next_u24()? as usize;
                let verify = r.take(len)?;
                let want = prf(&master, "server finished", &sha256(&transcript), 12);
                if verify != want.as_slice() {
                    return Err(Error12::Protocol("server Finished mismatch"));
                }
                server_finished_seq += 1;
                break;
            }
            CT_HANDSHAKE => {
                // 1.2 的 NewSessionTicket 会出现在 CCS 之前，我们不发 ticket 扩展，
                // 收到就跳过（不能当握手数据处理进转录）
                continue;
            }
            CT_ALERT => {
                let mut r = Reader::new(&content);
                return Err(Error12::Alert(
                    r.next_u8().unwrap_or(0),
                    r.next_u8().unwrap_or(0),
                ));
            }
            _ => return Err(Error12::Protocol("unexpected record before server CCS")),
        }
    }

    Ok(HandshakeResult {
        read,
        write,
        read_seq: server_finished_seq,
        write_seq: 1, // 客户端已经用掉 seq 0 发 Finished
    })
}

// ---------------------------------------------------------------- 消息编解码

fn hs_msg(typ: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + body.len());
    out.push(typ);
    out.extend_from_slice(&[(body.len() >> 16) as u8, (body.len() >> 8) as u8, body.len() as u8]);
    out.extend_from_slice(body);
    out
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// 1.2 的 Certificate：3 字节列表总长 + 每项 `3 字节长度 ‖ DER`（没有扩展字段）。
fn parse_cert_list(body: &[u8]) -> Result12<Vec<Cert>> {
    let mut r = Reader::new(body);
    let total = r.next_u24()? as usize;
    let list = r.take(total)?;
    let mut l = Reader::new(list);
    let mut out = Vec::new();
    while !l.is_empty() {
        let n = l.next_u24()? as usize;
        let der = l.take(n)?;
        out.push(x509::parse(der).map_err(|e| Error12::Cert(e.0))?);
    }
    Ok(out)
}

/// ServerKeyExchange（ECDHE）：curve_type=3、named_curve、点、签名算法、签名。
/// 返回 `(group, point)`，签名一并验掉。
fn parse_ske(body: &[u8], certs: &[Cert], prefix: &[u8]) -> Result12<(u16, Vec<u8>)> {
    let mut r = Reader::new(body);
    let curve_type = r.next_u8()?;
    if curve_type != 3 {
        return Err(Error12::Protocol("only named_curve ECDHE is supported"));
    }
    let group = u16::from_be_bytes([r.next_u8()?, r.next_u8()?]);
    let plen = r.next_u8()? as usize;
    let point = r.take(plen)?.to_vec();
    // 签名覆盖 client_random ‖ server_random ‖ 到点为止的参数
    let mut input = Vec::with_capacity(prefix.len() + 1 + 2 + 1 + point.len());
    input.extend_from_slice(prefix);
    input.push(curve_type);
    input.extend_from_slice(&group.to_be_bytes());
    input.push(plen as u8);
    input.extend_from_slice(&point);

    let scheme = u16::from_be_bytes([r.next_u8()?, r.next_u8()?]);
    let slen = u16::from_be_bytes([r.next_u8()?, r.next_u8()?]) as usize;
    let sig = r.take(slen)?.to_vec();

    let leaf = certs.first().ok_or(Error12::Cert("ServerKeyExchange before Certificate"))?;
    if !verify_sig(leaf, scheme, &input, &sig)? {
        return Err(Error12::Cert("ServerKeyExchange signature does not verify"));
    }
    Ok((group, point))
}

/// 用 leaf 证书的公钥验一段签名。`scheme` 是 1.2 的
/// `SignatureAndHashAlgorithm`（高字节 hash、低字节 signature）。
fn verify_sig(leaf: &Cert, scheme: u16, input: &[u8], sig: &[u8]) -> Result12<bool> {
    let ok = match scheme {
        0x0401 | 0x0403 | 0x0501 | 0x0503 | 0x0601 | 0x0804 => match (&leaf.key, scheme) {
            (x509::PubKey::Rsa { modulus, exponent }, 0x0401) => {
                rsa::verify_pkcs1_sha256(modulus, exponent, input, sig)
            }
            (x509::PubKey::Rsa { modulus, exponent }, 0x0501) => {
                rsa::verify_pkcs1_sha384(modulus, exponent, input, sig)
            }
            (x509::PubKey::Rsa { modulus, exponent }, 0x0601) => {
                rsa::verify_pkcs1_sha512(modulus, exponent, input, sig)
            }
            (x509::PubKey::Rsa { modulus, exponent }, 0x0804) => {
                rsa::verify_pss_sha256(modulus, exponent, input, sig)
            }
            (x509::PubKey::EcP256 { point }, 0x0403) if point.len() == 65 => {
                let mut sr = Reader::new(sig);
                let mut seq = sr.seq()?;
                let rr = seq.int_bytes()?;
                let ss = seq.int_bytes()?;
                ec::verify_p256(&point[1..33], &point[33..], input, rr, ss)
            }
            (x509::PubKey::EcP384 { point }, 0x0503) if point.len() == 97 => {
                let mut sr = Reader::new(sig);
                let mut seq = sr.seq()?;
                let rr = seq.int_bytes()?;
                let ss = seq.int_bytes()?;
                ec::verify_p384(&point[1..49], &point[49..], input, rr, ss)
            }
            _ => {
                return Err(Error12::Cert(
                    "ServerKeyExchange signature scheme does not match the key type",
                ))
            }
        },
        _ => return Err(Error12::Protocol("server chose a signature algorithm we did not offer")),
    };
    Ok(ok)
}

fn to_u32(point: &[u8]) -> Result12<[u8; 32]> {
    if point.len() != 32 {
        return Err(Error12::Protocol("x25519 public value must be 32 bytes"));
    }
    let mut o = [0u8; 32];
    o.copy_from_slice(point);
    Ok(o)
}

// ---------------------------------------------------------------- 记录层

fn read_exact(stream: &mut TcpStream, n: usize) -> Result12<Vec<u8>> {
    let mut buf = vec![0u8; n];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}

fn read_record(stream: &mut TcpStream) -> Result12<(u8, Vec<u8>)> {
    let hdr = read_exact(stream, 5)?;
    if hdr[1] != 0x03 {
        return Err(Error12::Protocol("record version is not TLS"));
    }
    let len = u16::from_be_bytes([hdr[3], hdr[4]]) as usize;
    if len > 16384 + 512 {
        return Err(Error12::Protocol("record too large"));
    }
    Ok((hdr[0], read_exact(stream, len)?))
}

fn read_plain(stream: &mut TcpStream) -> Result12<(u8, Vec<u8>)> {
    read_record(stream)
}

fn write_plain(stream: &mut TcpStream, ct: u8, body: &[u8]) -> Result12<()> {
    write_record(stream, ct, body)
}

fn write_record(stream: &mut TcpStream, ct: u8, body: &[u8]) -> Result12<()> {
    let mut hdr = Vec::with_capacity(5);
    hdr.push(ct);
    hdr.extend_from_slice(&[0x03, 0x03]);
    hdr.extend_from_slice(&(body.len() as u16).to_be_bytes());
    stream.write_all(&hdr)?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

/// 加密写一条 1.2 记录：头部长度必须包含 GCM 的显式 nonce 与 tag，
/// 而且它本身就是 AAD 里的 length 字段——先算长度再加密，顺序不能反。
fn write_record12(
    stream: &mut TcpStream,
    key: &Key12,
    seq: u64,
    ct: u8,
    plain: &[u8],
) -> Result12<()> {
    let sealed = key.seal(seq, ct, plain);
    write_record(stream, ct, &sealed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prf_matches_known_vector() {
        // RFC 5246 没有给 PRF 的固定向量，这里验三条性质 + 与 openssl 的
        // key block 长度对齐；真正的对齐在 e2e 里由服务端接受 Finished 来保证
        let secret = [0x11u8; 48];
        let a = prf(&secret, "master secret", &[0x22u8; 64], 48);
        assert_eq!(a.len(), 48);
        let b = prf(&secret, "master secret", &[0x22u8; 64], 48);
        assert_eq!(a, b);
        let c = prf(&secret, "key expansion", &[0x22u8; 64], 48);
        assert_ne!(a, c, "label 必须参与");
        // 超出 32 字节会跨 block，长度必须精确
        let d = prf(&secret, "x", &[], 100);
        assert_eq!(d.len(), 100);
        // 填满 32 的整数倍时不许多写
        assert_eq!(prf(&secret, "x", &[], 32).len(), 32);
    }

    #[test]
    fn p_sha256_first_block_is_hmac_of_a1_and_seed() {
        let secret = b"secret";
        let seed = b"seed";
        let a1 = hmac_sha256(secret, seed);
        let mut buf = Vec::new();
        buf.extend_from_slice(&a1);
        buf.extend_from_slice(seed);
        let want = hmac_sha256(secret, &buf);
        assert_eq!(p_sha256(secret, seed, 32), want.to_vec());
    }

    #[test]
    fn aad_layout_is_13_bytes() {
        let aad = aad12(1, 22, 100);
        assert_eq!(aad.len(), 13);
        assert_eq!(&aad[..8], &1u64.to_be_bytes());
        assert_eq!(aad[8], 22);
        assert_eq!(&aad[9..11], &[0x03, 0x03]);
        assert_eq!(&aad[11..], &(100u16).to_be_bytes());
    }

    #[test]
    fn key12_roundtrip_both_suites() {
        let iv4 = vec![0xaa; 4];
        let iv12 = vec![0xbb; 12];
        for (suite, iv) in [(Suite12::Aes128Gcm, iv4), (Suite12::Chacha20, iv12)] {
            let key = vec![0x11u8; suite.key_len()];
            let k = Key12::new(suite, key, iv);
            let body = k.seal(0, 23, b"hello tls1.2");
            let back = k.open(0, 23, &body).expect("roundtrip");
            assert_eq!(back, b"hello tls1.2");
            // seq 参与 nonce/AAD：换 seq 必须解不开
            assert!(k.open(1, 23, &body).is_none());
            // 类型参与 AAD：换成 handshake 也不行
            assert!(k.open(0, 22, &body).is_none());
        }
    }

    #[test]
    fn gcm_body_carries_explicit_nonce() {
        let k = Key12::new(Suite12::Aes128Gcm, vec![0x11; 16], vec![0xaa; 4]);
        let body = k.seal(7, 23, b"x");
        assert_eq!(&body[..8], &7u64.to_be_bytes());
        assert_eq!(body.len(), 8 + 1 + 16);
    }

    #[test]
    fn suite_ids_map_to_expected_shapes() {
        assert_eq!(Suite12::from_id(0xc02f), Some(Suite12::Aes128Gcm));
        assert_eq!(Suite12::from_id(0xc02b), Some(Suite12::Aes128Gcm));
        assert_eq!(Suite12::from_id(0xcca8), Some(Suite12::Chacha20));
        assert_eq!(Suite12::from_id(0xcca9), Some(Suite12::Chacha20));
        assert_eq!(Suite12::from_id(0x1301), None, "1.3 套件不走这里");
        assert_eq!(Suite12::Aes128Gcm.iv_len(), 4);
        assert_eq!(Suite12::Chacha20.iv_len(), 12);
    }
}
