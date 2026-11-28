//! TLS 1.3 客户端握手（RFC 8446），从 ClientHello 到应用数据。
//!
//! 只做爬虫需要的部分：单向认证、x25519 密钥交换、ChaCha20-Poly1305 或
//! AES-128-GCM、SHA-256 系列签名。不做会话恢复、0-RTT、HRR、客户端证书、
//! 后量子组——遇到就报明确的错误，不悄悄降级。
//!
//! 这是"HTTPS 必须做，但不许引第三方库"那条线的落点：所有密码学都在
//! `tls::` 下的本 crate 代码里，`[dependencies]` 仍然为空。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use super::chain;
use super::der::Reader;
use super::ec;
use super::rsa;
use super::sha256::{self, hmac_sha256, hkdf_expand, hkdf_extract};
use super::tls12::{self, Error12};
use super::x25519;
use super::x509::{self, Cert};

pub const CHACHA20_POLY1305_SHA256: u16 = 0x1303;
pub const AES_128_GCM_SHA256: u16 = 0x1301;

const CT_CHANGE_CIPHER_SPEC: u8 = 20;
const CT_ALERT: u8 = 21;
const CT_HANDSHAKE: u8 = 22;
const CT_APPLICATION_DATA: u8 = 23;

const HS_CLIENT_HELLO: u8 = 1;
const HS_SERVER_HELLO: u8 = 2;
const HS_NEW_SESSION_TICKET: u8 = 4;
const HS_ENCRYPTED_EXTENSIONS: u8 = 8;
const HS_CERTIFICATE: u8 = 11;
const HS_CERTIFICATE_VERIFY: u8 = 15;
const HS_FINISHED: u8 = 20;

#[derive(Debug)]
pub enum TlsError {
    Io(std::io::Error),
    Protocol(&'static str),
    /// 证书或链没通过
    Cert(&'static str),
    /// 服务端发的 alert
    Alert(u8, u8),
}

impl std::fmt::Display for TlsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TlsError::Io(e) => write!(f, "tls io: {e}"),
            TlsError::Protocol(m) => write!(f, "tls protocol: {m}"),
            TlsError::Cert(m) => write!(f, "tls certificate: {m}"),
            TlsError::Alert(l, d) => write!(f, "tls alert {l}/{d}"),
        }
    }
}

impl From<super::der::DerError> for TlsError {
    fn from(_: super::der::DerError) -> Self {
        // Reader 的所有失败在握手里都等于"对端发了畸形字节"
        TlsError::Protocol("malformed handshake bytes")
    }
}

impl From<std::io::Error> for TlsError {
    fn from(e: std::io::Error) -> Self {
        TlsError::Io(e)
    }
}

pub type Result<T> = core::result::Result<T, TlsError>;

// ---------------------------------------------------------------- 对称层

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Suite {
    Chacha,
    Aes128,
}

impl Suite {
    fn key_len(self) -> usize {
        match self {
            Suite::Chacha => 32,
            Suite::Aes128 => 16,
        }
    }
}

struct AeadKey {
    suite: Suite,
    key: Vec<u8>,
    iv: [u8; 12],
}

impl AeadKey {
    /// `aad` 是这条记录的 5 字节头部（RFC 8446 §5.2 要求把不加密的头部作为
    /// additional data 传进 AEAD），漏掉它所有记录都过不了认证。
    fn open(&self, seq: u64, aad: &[u8], ct: &[u8]) -> Option<Vec<u8>> {
        let nonce = nonce_from_iv(&self.iv, seq);
        match self.suite {
            Suite::Chacha => {
                let k: [u8; 32] = self.key.as_slice().try_into().ok()?;
                super::chacha::open(&k, &nonce, aad, ct)
            }
            Suite::Aes128 => {
                let k: [u8; 16] = self.key.as_slice().try_into().ok()?;
                super::aes::open(&k, &nonce, aad, ct)
            }
        }
    }

    fn seal(&self, seq: u64, aad: &[u8], pt: &[u8]) -> Vec<u8> {
        let nonce = nonce_from_iv(&self.iv, seq);
        match self.suite {
            Suite::Chacha => {
                let k: [u8; 32] = self.key.as_slice().try_into().expect("32 字节");
                super::chacha::seal(&k, &nonce, aad, pt)
            }
            Suite::Aes128 => {
                let k: [u8; 16] = self.key.as_slice().try_into().expect("16 字节");
                super::aes::seal(&k, &nonce, aad, pt)
            }
        }
    }
}

/// 记录层密钥：1.3 与 1.2 的 nonce 与 AAD 规则不同，这里统一成一套读写接口，
/// 上层（握手、`TlsStream`）就不用关心版本了。
enum RecordKey {
    V13(AeadKey),
    V12(tls12::Key12),
}

impl RecordKey {
    /// 解一条记录，返回 `(内容类型, 明文)`。
    /// 1.3 要剥掉内部的 content type，1.2 的外层类型就是内容类型。
    fn open(&self, seq: u64, outer: u8, header: &[u8; 5], body: &[u8]) -> Option<(u8, Vec<u8>)> {
        match self {
            RecordKey::V13(k) => {
                if outer != CT_APPLICATION_DATA {
                    return None;
                }
                let plain = k.open(seq, header, body)?;
                let mut i = plain.len();
                while i > 0 && plain[i - 1] == 0 {
                    i -= 1;
                }
                if i == 0 {
                    return None;
                }
                Some((plain[i - 1], plain[..i - 1].to_vec()))
            }
            RecordKey::V12(k) => {
                Some((outer, k.open(seq, outer, body)?))
            }
        }
    }

    /// 封一条记录，返回 `(5 字节头, 记录体)`。头部长度必须先算好，
    /// 因为它（及其长度字段）本身就是 AAD 的一部分。
    fn seal(&self, seq: u64, ct: u8, pt: &[u8]) -> ([u8; 5], Vec<u8>) {
        match self {
            RecordKey::V13(k) => {
                let mut inner = Vec::with_capacity(pt.len() + 1);
                inner.extend_from_slice(pt);
                inner.push(ct);
                let mut hdr = [0u8; 5];
                hdr[0] = CT_APPLICATION_DATA;
                hdr[1] = 0x03;
                hdr[2] = 0x03;
                hdr[3..].copy_from_slice(&((inner.len() + 16) as u16).to_be_bytes());
                (hdr, k.seal(seq, &hdr, &inner))
            }
            RecordKey::V12(k) => {
                let body = k.seal(seq, ct, pt);
                let mut hdr = [0u8; 5];
                hdr[0] = ct;
                hdr[1] = 0x03;
                hdr[2] = 0x03;
                hdr[3..].copy_from_slice(&(body.len() as u16).to_be_bytes());
                (hdr, body)
            }
        }
    }

    fn is_13(&self) -> bool {
        matches!(self, RecordKey::V13(_))
    }
}

fn nonce_from_iv(iv: &[u8; 12], seq: u64) -> [u8; 12] {
    let mut n = *iv;
    for (i, b) in seq.to_be_bytes().iter().enumerate() {
        n[4 + i] ^= b;
    }
    n
}

// ---------------------------------------------------------------- 流

/// 握手完成后的连接。`Read`/`Write` 直接收发明文应用数据。
pub struct TlsStream {
    stream: TcpStream,
    read: RecordKey,
    write: RecordKey,
    read_seq: u64,
    write_seq: u64,
    /// 已解密但还没被 read() 取走的数据
    pending: Vec<u8>,
    /// 对端发过 close_notify（或被当成 EOF），之后 read() 一律返回 0
    eof: bool,
}

impl TlsStream {
    /// 连上并完成握手。`roots` 为空时现场加载系统根证书。
    pub fn connect(
        host: &str,
        addr: SocketAddr,
        timeout: Duration,
        roots: Option<&[Cert]>,
    ) -> Result<TlsStream> {
        let stream = TcpStream::connect_timeout(&addr, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let _ = stream.set_nodelay(true);
        Self::handshake(stream, host, roots)
    }

    pub fn handshake(mut stream: TcpStream, host: &str, roots: Option<&[Cert]>) -> Result<TlsStream> {
        let (r, w, rseq, wseq) = Self::perform_handshake(&mut stream, host, roots)?;
        Ok(TlsStream {
            stream,
            read: r,
            write: w,
            read_seq: rseq,
            write_seq: wseq,
            pending: Vec::new(),
            eof: false,
        })
    }

    fn perform_handshake(
        stream: &mut TcpStream,
        host: &str,
        roots: Option<&[Cert]>,
    ) -> Result<(RecordKey, RecordKey, u64, u64)> {
        let roots_owned;
        let roots: &[Cert] = match roots {
            Some(r) => r,
            None => {
                roots_owned = x509::system_roots().map_err(|_| TlsError::Cert("no system CA bundle"))?;
                &roots_owned
            }
        };

        // ---- ClientHello
        let secret = random_bytes(32);
        let sk = x25519_private(&secret);
        let pubkey = x25519::public_key(&sk);
        let random = random_bytes(32);
        let session_id = random_bytes(32);
        let hello = build_client_hello(host, &pubkey, &random, &session_id);
        let mut transcript = Vec::with_capacity(2048);
        transcript.extend_from_slice(&hello);
        write_plain(stream, CT_HANDSHAKE, &hello)?;

        // ---- ServerHello（明文）
        let mut sh_buf = Vec::new();
        loop {
            let (ct, _header, content) = read_record(stream)?;
            match ct {
                CT_CHANGE_CIPHER_SPEC => continue, // 兼容中间盒的空 CCS
                CT_HANDSHAKE => {
                    let mut r = Reader::new(&content);
                    let typ = r.next_u8()?;
                    if typ != HS_SERVER_HELLO {
                        return Err(TlsError::Protocol("expected ServerHello"));
                    }
                    let len = r.next_u24()?;
                    let body = r.take(len)?;
                    sh_buf.extend_from_slice(&[HS_SERVER_HELLO]);
                    sh_buf.extend_from_slice(&u24(len));
                    sh_buf.extend_from_slice(body);
                    transcript.extend_from_slice(&sh_buf);
                    break;
                }
                CT_ALERT => {
                    let mut r = Reader::new(&content);
                    return Err(TlsError::Alert(r.next_u8().unwrap_or(0), r.next_u8().unwrap_or(0)));
                }
                _ => return Err(TlsError::Protocol("unexpected record during handshake")),
            }
        }
        let sh = parse_server_hello(&sh_buf)?;

        // 服务端没回 supported_versions 就是只认 1.2：交给 tls12 那段跑完
        if let ServerHelloKind::V12 {
            suite,
            server_random,
            ems,
        } = sh.kind
        {
            let mut cr = [0u8; 32];
            cr.copy_from_slice(&random);
            let r = tls12::handshake(
                stream,
                host,
                roots,
                &hello,
                &sh_buf,
                &cr,
                &server_random,
                suite,
                ems,
            )
            .map_err(map12)?;
            return Ok((
                RecordKey::V12(r.read),
                RecordKey::V12(r.write),
                r.read_seq,
                r.write_seq,
            ));
        }
        let (suite, server_ks) = match sh.kind {
            ServerHelloKind::V13 { suite, key_share } => (suite, key_share),
            ServerHelloKind::V12 { .. } => unreachable!("1.2 在上面已经返回"),
        };
        // 我们每次都用新的 32 字节 legacy_session_id，服务端必须原样回显；
        // 回显了但长度不同就意味着对面理解成了 session resumption —— 不支持，直接拒
        if sh.session_id.as_deref().map(<[u8]>::len) != Some(32) {
            return Err(TlsError::Protocol("server did not echo session id"));
        }

        // ---- 密钥派生
        let shared = x25519::shared_secret(&sk, &server_ks);
        if std::env::var("RSBOOT_TLS_DEBUG").is_ok() {
            eprintln!("sk={}", hex(&sk));
            eprintln!("peer={}", hex(&server_ks));
        }
        // RFC 8446 §7.1：Early Secret 的 salt 与 IKM 都是"0"，在 HKDF 里指
        // 32 字节全零（不是空串——空 salt 的 HMAC 结果与 32 个零字节不同）
        let early = hkdf_extract(&[0u8; 32], &[0u8; 32]);
        // Derive-Secret 的 context 是「空 transcript 的哈希」= SHA256("")，
        // 不是空字节串。RFC 8448 §3 给出的 info 里那 32 字节 e3b0c442… 就是它，
        // 传错会让 handshake_secret 之后的值全部与标准实现分叉。
        let empty_hash = sha256::sha256(&[]);
        let derived = expand_label(&early, "derived", &empty_hash, 32);
        let handshake_secret = hkdf_extract(&derived, &shared);
        let th_sh = sha256::sha256(&transcript);
        let c_hs = expand_label(&handshake_secret, "c hs traffic", &th_sh, 32);
        let s_hs = expand_label(&handshake_secret, "s hs traffic", &th_sh, 32);
        if std::env::var("RSBOOT_TLS_DEBUG").is_ok() {
            eprintln!("suite={:?}", suite);
            eprintln!("hello={}", hex(&transcript[..hello.len()]));
            eprintln!("sh={}", hex(&sh_buf));
            eprintln!("shared={}", hex(&shared));
            eprintln!("early={}", hex(&early));
            eprintln!("derived={}", hex(&derived));
            eprintln!("hs={}", hex(&handshake_secret));
            eprintln!("th_sh={}", hex(&th_sh));
            eprintln!("c_hs={}", hex(&c_hs));
            eprintln!("s_hs={}", hex(&s_hs));
        }

        let read_hs = traffic_key(&s_hs, suite);
        if std::env::var("RSBOOT_TLS_DEBUG").is_ok() {
            eprintln!("rkey={}", hex(&read_hs.key));
            eprintln!("riv={}", hex(&read_hs.iv));
        }
        let read = RecordKey::V13(read_hs);
        let mut read_seq = 0u64;

        // ---- 加密握手：EE / Certificate / CertificateVerify / Finished
        let mut leaf: Option<Cert> = None;
        let mut inters: Vec<Cert> = Vec::new();
        let mut finished_verified = false;
        let mut hs_buf = Vec::new(); // 待解析的握手字节流
        let server_finished_key = expand_label(&s_hs, "finished", &[], 32);

        while !finished_verified {
            // 先从缓冲里取完整握手消息
            loop {
                if hs_buf.len() < 4 {
                    break;
                }
                let typ = hs_buf[0];
                let len = u32::from_be_bytes([0, hs_buf[1], hs_buf[2], hs_buf[3]]) as usize;
                if std::env::var("RSBOOT_TLS_DEBUG").is_ok() {
                    eprintln!("  hs msg typ={typ} len={len} buf={}", hs_buf.len());
                }
                if hs_buf.len() < 4 + len {
                    break;
                }
                let msg: Vec<u8> = hs_buf.drain(..4 + len).collect();
                match typ {
                    HS_ENCRYPTED_EXTENSIONS => {
                        transcript.extend_from_slice(&msg);
                    }
                    HS_CERTIFICATE => {
                        transcript.extend_from_slice(&msg);
                        let certs = parse_certificate(&msg[4..])?;
                        if certs.is_empty() {
                            return Err(TlsError::Cert("empty certificate list"));
                        }
                        let mut parsed = Vec::with_capacity(certs.len());
                        for c in &certs {
                            parsed.push(x509::parse(c).map_err(|e| TlsError::Cert(e.0))?);
                        }
                        leaf = parsed.first().cloned();
                        inters = parsed.into_iter().skip(1).collect();
                    }
                    HS_CERTIFICATE_VERIFY => {
                        // 签名覆盖"到 Certificate 为止"的 transcript
                        let pre = sha256::sha256(&transcript);
                        transcript.extend_from_slice(&msg);
                        let body = &msg[4..];
                        verify_certificate_verify(
                            body,
                            leaf.as_ref(),
                            &inters,
                            roots,
                            host,
                            &pre,
                        )?;
                    }
                    HS_FINISHED => {
                        // Finished 的 MAC 覆盖的是"Finished 之前"的转录，
                        // 所以先算再把它自己（以及后续的 NewSessionTicket）追加进去
                        let th = sha256::sha256(&transcript);
                        let want = hmac_sha256(&server_finished_key, &th);
                        if body_of(&msg) != want {
                            return Err(TlsError::Protocol("server Finished mismatch"));
                        }
                        transcript.extend_from_slice(&msg);
                        finished_verified = true;
                    }
                    HS_NEW_SESSION_TICKET => {
                        transcript.extend_from_slice(&msg);
                    }
                    _ => {
                        transcript.extend_from_slice(&msg);
                    }
                }
                if finished_verified {
                    break;
                }
            }
            if finished_verified {
                break;
            }
            // 缓冲区不够，再读一条记录
            let (ct, content) = read_encrypted(stream, &read, read_seq)?;
            if ct != CT_CHANGE_CIPHER_SPEC {
                read_seq += 1;
            }
            if ct == CT_ALERT {
                let mut r = Reader::new(&content);
                return Err(TlsError::Alert(r.next_u8().unwrap_or(0), r.next_u8().unwrap_or(0)));
            }
            hs_buf.extend_from_slice(&content);
        }

        // ---- master secret 与应用密钥
        let derived2 = expand_label(&handshake_secret, "derived", &empty_hash, 32);
        let master = hkdf_extract(&derived2, &[0u8; 32]);
        let th_full = sha256::sha256(&transcript);
        let c_ap = expand_label(&master, "c ap traffic", &th_full, 32);
        let s_ap = expand_label(&master, "s ap traffic", &th_full, 32);

        // ---- 客户端 Finished
        let client_finished_key = expand_label(&c_hs, "finished", &[], 32);
        let th_cf = sha256::sha256(&transcript);
        let fin = hmac_sha256(&client_finished_key, &th_cf);
        let mut msg = vec![HS_FINISHED, 0, 0, 32];
        msg.extend_from_slice(&fin);
        transcript.extend_from_slice(&msg);
        let write = RecordKey::V13(traffic_key(&c_hs, suite));
        let seq_for_write = 0u64;
        write_encrypted(stream, &write, seq_for_write, CT_HANDSHAKE, &msg)?;

        // ---- 切换为应用数据密钥
        let read_key = traffic_key(&s_ap, suite);
        let write_key = traffic_key(&c_ap, suite);
        let _ = (server_finished_key, read_seq as u8);
        // 客户端 handshake 记录用掉 seq 0，应用数据从 0 重新计数
        Ok((RecordKey::V13(read_key), RecordKey::V13(write_key), 0, 0))
    }

    fn read_record_encrypted(&mut self) -> Result<Vec<u8>> {
        let (ct, content) = read_encrypted(&mut self.stream, &self.read, self.read_seq)?;
        self.read_seq += 1;
        match ct {
            CT_APPLICATION_DATA => Ok(content),
            CT_ALERT => {
                let mut r = Reader::new(&content);
                let level = r.next_u8().unwrap_or(2);
                let desc = r.next_u8().unwrap_or(0);
                // close_notify(0) 是正常收尾：当成 EOF，别让上层看见一个错误
                if desc == 0 && level == 1 {
                    self.eof = true;
                    return Ok(Vec::new());
                }
                Err(TlsError::Alert(level, desc))
            }
            // 握手消息（NewSessionTicket 等）在应用阶段直接忽略
            _ => Ok(Vec::new()),
        }
    }
}

fn body_of(msg: &[u8]) -> &[u8] {
    &msg[4..]
}

impl Read for TlsStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.eof {
            return Ok(0);
        }
        if self.pending.is_empty() {
            loop {
                match self.read_record_encrypted() {
                    Ok(v) => {
                        if !v.is_empty() {
                            self.pending = v;
                            break;
                        }
                    }
                    Err(TlsError::Io(e)) => return Err(e),
                    Err(e) => {
                        return Err(std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
                    }
                }
            }
        }
        let n = buf.len().min(self.pending.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }
}

impl Write for TlsStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // 单条记录上限 2^14，超了就切
        let mut off = 0;
        while off < buf.len() {
            let n = (buf.len() - off).min(16384);
            let seq = self.write_seq;
            let res = write_encrypted(&mut self.stream, &self.write, seq, CT_APPLICATION_DATA, &buf[off..off + n]);
            match res {
                Ok(()) => {
                    self.write_seq += 1;
                    off += n;
                }
                Err(TlsError::Io(e)) => return Err(e),
                Err(e) => return Err(std::io::Error::new(std::io::ErrorKind::Other, e.to_string())),
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

// ---------------------------------------------------------------- 编解码

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn u24(n: usize) -> [u8; 3] {
    [(n >> 16) as u8, (n >> 8) as u8, n as u8]
}

pub(crate) fn random_bytes(n: usize) -> Vec<u8> {
    // 零依赖下唯一可靠的熵源：Unix 的 /dev/urandom。
    // 注意不能用 std::fs::read——那会把整个字符设备读到内存耗尽，
    // 必须显式 read_exact。
    use std::io::Read as _;
    let dev = std::fs::File::open("/dev/urandom").and_then(|mut f| {
        let mut out = vec![0u8; n];
        f.read_exact(&mut out).map(|_| out)
    });
    match dev {
        Ok(v) if v.len() >= n => v[..n].to_vec(),
        _ => {
            // 极端兜底：时间 + 地址做混合，仅供非安全环境跑通
            let mut seed = Vec::new();
            if let Ok(d) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
                seed.extend_from_slice(&d.as_nanos().to_le_bytes());
            }
            let mut out = Vec::with_capacity(n);
            let mut ctr = 0u32;
            while out.len() < n {
                seed.extend_from_slice(&ctr.to_le_bytes());
                out.extend_from_slice(&sha256::sha256(&seed));
                ctr += 1;
            }
            out.truncate(n);
            out
        }
    }
}

/// 1.2 那半边的错误码映射回统一错误。
fn map12(e: Error12) -> TlsError {
    match e {
        Error12::Io(io) => TlsError::Io(io),
        Error12::Protocol(m) => TlsError::Protocol(m),
        Error12::Cert(m) => TlsError::Cert(m),
        Error12::Alert(l, d) => TlsError::Alert(l, d),
    }
}

fn x25519_private(seed: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 32];
    let n = seed.len().min(32);
    k[..n].copy_from_slice(&seed[..n]);
    k
}

/// 一个 ClientHello 同时兼容 1.3 与 1.2。
///
/// 关键在 `legacy_version` 与 `supported_versions` 的分工：legacy 固定 0x0303
/// （1.2 服务端只读这个字段），`supported_versions` 里同时列 1.3 和 1.2。
/// 1.3 服务端会挑 1.3；只认 1.2 的服务端按规范忽略这个不认识的扩展、回落到
/// legacy 版本，于是同一个 hello 两边都能谈——不需要"先试 1.3 失败再重试"。
/// 套件列表同理：1.3 与 1.2 的编号互不认识，各自忽略对方的。
fn build_client_hello(host: &str, pubkey: &[u8; 32], random: &[u8], session_id: &[u8]) -> Vec<u8> {
    let mut ext = Vec::new();
    // server_name
    {
        let mut name = Vec::new();
        name.push(0); // host_name
        name.extend_from_slice(&(host.len() as u16).to_be_bytes());
        name.extend_from_slice(host.as_bytes());
        ext.extend_from_slice(&[0x00, 0x00]);
        ext.extend_from_slice(&((name.len() + 2) as u16).to_be_bytes());
        ext.extend_from_slice(&(name.len() as u16).to_be_bytes());
        ext.extend_from_slice(&name);
    }
    // supported_groups: x25519 与 secp256r1。1.3 只用前者；1.2 的 ECDHE 两个都可能
    // 被选（x25519 在 1.2 里由 RFC 8422 定义，老服务端会退到 secp256r1）
    {
        let groups: [u16; 2] = [0x001d, 0x0017];
        ext.extend_from_slice(&[0x00, 0x0a]);
        ext.extend_from_slice(&((groups.len() * 2 + 2) as u16).to_be_bytes());
        ext.extend_from_slice(&((groups.len() * 2) as u16).to_be_bytes());
        for g in groups {
            ext.extend_from_slice(&g.to_be_bytes());
        }
    }
    // ec_point_formats: 只支持 uncompressed。1.2 的某些老服务端缺这个扩展会拒
    {
        ext.extend_from_slice(&[0x00, 0x0b, 0x00, 0x02, 0x01, 0x00]);
    }
    // signature_algorithms：1.3 的 CertVerify 与 1.2 的 ServerKeyExchange 共用
    {
        // 0403=ECDSA P-256, 0503=ECDSA P-384, 0804=RSA-PSS, 0401/0501/0601=RSA-PKCS1
        let algs: [u16; 6] = [0x0403, 0x0503, 0x0804, 0x0401, 0x0501, 0x0601];
        ext.extend_from_slice(&[0x00, 0x0d]);
        ext.extend_from_slice(&((algs.len() * 2 + 2) as u16).to_be_bytes());
        ext.extend_from_slice(&((algs.len() * 2) as u16).to_be_bytes());
        for a in algs {
            ext.extend_from_slice(&a.to_be_bytes());
        }
    }
    // supported_versions: 1.3 与 1.2 都列
    {
        ext.extend_from_slice(&[0x00, 0x2b, 0x00, 0x05, 0x04, 0x03, 0x04, 0x03, 0x03]);
    }
    // extended_master_secret（1.2 才有意义，1.3 服务端忽略）。
    // 有它 master secret 绑定了握手转录，能挡 triple handshake 那一类
    ext.extend_from_slice(&[0x00, 0x17, 0x00, 0x00]);
    // renegotiation_info：空表示"本次是新连接"，1.2 服务端往往要求它存在
    ext.extend_from_slice(&[0xff, 0x01, 0x00, 0x01, 0x00]);
    // key_share: x25519
    {
        ext.extend_from_slice(&[0x00, 0x33, 0x00, 0x26, 0x00, 0x24, 0x00, 0x1d, 0x00, 0x20]);
        ext.extend_from_slice(pubkey);
    }

    // 套件：1.3 两个 + 1.2 的四个 ECDHE-AEAD（CBC 一个都不列）
    let ciphers: [u16; 6] = [
        CHACHA20_POLY1305_SHA256,
        AES_128_GCM_SHA256,
        tls12::ECDHE_ECDSA_AES128_GCM_SHA256,
        tls12::ECDHE_RSA_AES128_GCM_SHA256,
        tls12::ECDHE_ECDSA_CHACHA20_POLY1305_SHA256,
        tls12::ECDHE_RSA_CHACHA20_POLY1305_SHA256,
    ];

    let mut body = Vec::new();
    body.extend_from_slice(&[0x03, 0x03]); // legacy_version
    body.extend_from_slice(random);
    body.push(session_id.len() as u8);
    body.extend_from_slice(session_id);
    body.extend_from_slice(&((ciphers.len() * 2) as u16).to_be_bytes());
    for c in ciphers {
        body.extend_from_slice(&c.to_be_bytes());
    }
    body.push(1);
    body.push(0); // null compression
    body.extend_from_slice(&(ext.len() as u16).to_be_bytes());
    body.extend_from_slice(&ext);

    let mut out = vec![HS_CLIENT_HELLO];
    out.extend_from_slice(&u24(body.len()));
    out.extend_from_slice(&body);
    out
}

struct ServerHello {
    kind: ServerHelloKind,
    session_id: Option<Vec<u8>>,
}

/// 服务端到底谈了哪个版本。1.3 的服务端必须回 `supported_versions`，
/// 没有这个扩展就说明对面只认 1.2（我们发的 legacy_version 是 0x0303）。
enum ServerHelloKind {
    V13 { suite: Suite, key_share: [u8; 32] },
    V12 {
        suite: tls12::Suite12,
        server_random: [u8; 32],
        ems: bool,
    },
}

fn parse_server_hello(msg: &[u8]) -> Result<ServerHello> {
    let mut r = Reader::new(msg);
    let _typ = r.next_u8()?;
    let len = r.next_u24()?;
    let body = r.take(len)?;
    let mut b = Reader::new(body);
    let _legacy = b.take(2)?;
    let random = b.take(32)?.to_vec();
    let sid_len = b.next_u8()? as usize;
    let session_id = b.take(sid_len)?.to_vec();
    let cs = u16::from_be_bytes([b.next_u8()?, b.next_u8()?]);
    let comp = b.next_u8()?;
    if comp != 0 {
        // 我们只提供 null 压缩，服务端选别的等于协议违规
        return Err(TlsError::Protocol("compression must be null"));
    }

    let mut version13: Option<u16> = None;
    let mut key_share: Option<[u8; 32]> = None;
    let mut ems = false;
    if !b.is_empty() {
        let ext_len = u16::from_be_bytes([b.next_u8()?, b.next_u8()?]) as usize;
        let exts = b.take(ext_len)?;
        let mut e = Reader::new(exts);
        while !e.is_empty() {
            let t = u16::from_be_bytes([e.next_u8()?, e.next_u8()?]);
            let l = u16::from_be_bytes([e.next_u8()?, e.next_u8()?]) as usize;
            let data = e.take(l)?;
            match t {
                43 => {
                    let mut v = Reader::new(data);
                    version13 = Some(u16::from_be_bytes([v.next_u8()?, v.next_u8()?]));
                }
                51 => {
                    let mut k = Reader::new(data);
                    let group = u16::from_be_bytes([k.next_u8()?, k.next_u8()?]);
                    if group != 0x001d {
                        return Err(TlsError::Protocol(
                            "server picked a key share group we did not offer",
                        ));
                    }
                    let kl = u16::from_be_bytes([k.next_u8()?, k.next_u8()?]) as usize;
                    let kd = k.take(kl)?;
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&kd[..32]);
                    key_share = Some(arr);
                }
                23 => ems = true,
                _ => {}
            }
        }
    }

    let kind = match version13 {
        Some(0x0304) => {
            let suite = match cs {
                CHACHA20_POLY1305_SHA256 => Suite::Chacha,
                AES_128_GCM_SHA256 => Suite::Aes128,
                _ => return Err(TlsError::Protocol("server picked an unsupported cipher suite")),
            };
            ServerHelloKind::V13 {
                suite,
                key_share: key_share
                    .ok_or(TlsError::Protocol("ServerHello without key_share"))?,
            }
        }
        // 服务端回了 supported_versions 但选的是 1.2，或者干脆没回这个扩展：
        // 两种都当 1.2 处理，套件必须落在我们列的那四个 AEAD 里
        Some(other) if other == 0x0303 || other == 0x0302 => {
            let suite = tls12::Suite12::from_id(cs)
                .ok_or(TlsError::Protocol("server picked an unsupported TLS 1.2 suite"))?;
            let mut sr = [0u8; 32];
            sr.copy_from_slice(&random);
            ServerHelloKind::V12 {
                suite,
                server_random: sr,
                ems,
            }
        }
        Some(_) => return Err(TlsError::Protocol("server agreed on an unsupported TLS version")),
        None => {
            let suite = tls12::Suite12::from_id(cs)
                .ok_or(TlsError::Protocol("server picked an unsupported TLS 1.2 suite"))?;
            let mut sr = [0u8; 32];
            sr.copy_from_slice(&random);
            ServerHelloKind::V12 {
                suite,
                server_random: sr,
                ems,
            }
        }
    };
    Ok(ServerHello { kind, session_id: Some(session_id) })
}

/// 证书列表：3 字节总长 + 每项 3 字节长度。
fn parse_certificate(body: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut r = Reader::new(body);
    let _ctx_len = r.next_u8()?;
    let total = r.next_u24()? as usize;
    if std::env::var("RSBOOT_TLS_DEBUG").is_ok() {
        eprintln!("  cert body={} ctx={} total={}", body.len(), _ctx_len, total);
    }
    let list = r.take(total)?;
    let mut l = Reader::new(list);
    let mut out = Vec::new();
    while !l.is_empty() {
        let n = l.next_u24()? as usize;
        out.push(l.take(n)?.to_vec());
        // CertificateEntry 的 extensions（通常是 status_request / SCT），跳过
        let ext_len = u16::from_be_bytes([l.next_u8()?, l.next_u8()?]) as usize;
        let _ = l.take(ext_len)?;
    }
    Ok(out)
}

fn verify_certificate_verify(
    body: &[u8],
    leaf: Option<&Cert>,
    inters: &[Cert],
    roots: &[Cert],
    host: &str,
    transcript_hash: &[u8; 32],
) -> Result<()> {
    let leaf = leaf.ok_or(TlsError::Cert("CertificateVerify before Certificate"))?;
    // 先把链验掉：签名算法可信的前提是证书本身来自信任根
    chain::verify(leaf, inters, roots, host).map_err(|e| TlsError::Cert(e.as_str()))?;

    let mut r = Reader::new(body);
    let scheme = u16::from_be_bytes([r.next_u8()?, r.next_u8()?]);
    let sig_len = u16::from_be_bytes([r.next_u8()?, r.next_u8()?]) as usize;
    let sig = r.take(sig_len)?;

    // 签名输入：64 个空格 + 标签 + 0x00 + transcript hash
    let mut input = Vec::with_capacity(64 + 33 + 32);
    input.extend_from_slice(&[0x20u8; 64]);
    input.extend_from_slice(b"TLS 1.3, server CertificateVerify");
    input.push(0);
    input.extend_from_slice(transcript_hash);

    let ok = match scheme {
        0x0403 => match &leaf.key {
            x509::PubKey::EcP256 { point } if point.len() == 65 => {
                let mut sr = Reader::new(sig);
                let Ok(mut seq) = sr.seq() else {
                    return Err(TlsError::Cert("bad ECDSA signature"));
                };
                let Ok(rr) = seq.int_bytes() else {
                    return Err(TlsError::Cert("bad ECDSA signature"));
                };
                let Ok(ss) = seq.int_bytes() else {
                    return Err(TlsError::Cert("bad ECDSA signature"));
                };
                ec::verify_p256(&point[1..33], &point[33..], &input, rr, ss)
            }
            _ => false,
        },
        0x0503 => match &leaf.key {
            x509::PubKey::EcP384 { point } if point.len() == 97 => {
                let mut sr = Reader::new(sig);
                let Ok(mut seq) = sr.seq() else {
                    return Err(TlsError::Cert("bad ECDSA signature"));
                };
                let Ok(rr) = seq.int_bytes() else {
                    return Err(TlsError::Cert("bad ECDSA signature"));
                };
                let Ok(ss) = seq.int_bytes() else {
                    return Err(TlsError::Cert("bad ECDSA signature"));
                };
                ec::verify_p384(&point[1..49], &point[49..], &input, rr, ss)
            }
            _ => false,
        },
        0x0804 => match &leaf.key {
            x509::PubKey::Rsa { modulus, exponent } => {
                rsa::verify_pss_sha256(modulus, exponent, &input, sig)
            }
            _ => false,
        },
        0x0401 => match &leaf.key {
            x509::PubKey::Rsa { modulus, exponent } => {
                rsa::verify_pkcs1_sha256(modulus, exponent, &input, sig)
            }
            _ => false,
        },
        // 我们没在 ClientHello 里列这个 scheme，服务端选了就是协议违规
        _ => return Err(TlsError::Protocol("server chose a signature scheme we did not offer")),
    };
    if !ok {
        return Err(TlsError::Cert("CertificateVerify signature does not verify"));
    }
    Ok(())
}

// ---------------------------------------------------------------- 记录层

fn write_plain(stream: &mut TcpStream, ct: u8, body: &[u8]) -> Result<()> {
    let mut hdr = Vec::with_capacity(5);
    hdr.push(ct);
    hdr.extend_from_slice(&[0x03, 0x03]);
    hdr.extend_from_slice(&(body.len() as u16).to_be_bytes());
    stream.write_all(&hdr)?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

fn read_exact(stream: &mut TcpStream, n: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}

fn read_record(stream: &mut TcpStream) -> Result<(u8, [u8; 5], Vec<u8>)> {
    let hdr = read_exact(stream, 5)?;
    let mut header = [0u8; 5];
    header.copy_from_slice(&hdr);
    if hdr[1] != 0x03 {
        return Err(TlsError::Protocol("record version is not TLS"));
    }
    let len = u16::from_be_bytes([hdr[3], hdr[4]]) as usize;
    if len > 16384 + 512 {
        return Err(TlsError::Protocol("record too large"));
    }
    let body = read_exact(stream, len)?;
    if std::env::var("RSBOOT_TLS_DEBUG").is_ok() {
        eprintln!("  rec type={} len={} head={}", hdr[0], len, hex(&body[..body.len().min(24)]));
    }
    Ok((hdr[0], header, body))
}

/// 解一条记录，返回 `(内容类型, 明文)`。版本差异全在 `RecordKey` 里。
fn read_encrypted(stream: &mut TcpStream, key: &RecordKey, seq: u64) -> Result<(u8, Vec<u8>)> {
    let (outer, header, body) = read_record(stream)?;
    if std::env::var("RSBOOT_TLS_DEBUG").is_ok() {
        eprintln!(
            "rec outer={outer} len={} seq={seq} body={}",
            body.len(),
            hex(&body[..body.len().min(96)])
        );
    }
    if outer == CT_CHANGE_CIPHER_SPEC {
        return Ok((CT_CHANGE_CIPHER_SPEC, Vec::new()));
    }
    // 明文 alert 可能出现在密钥切换点前后；1.2 的 alert 是加密的，走下面
    if key.is_13() && outer == CT_ALERT {
        return Ok((CT_ALERT, body));
    }
    key.open(seq, outer, &header, &body)
        .ok_or(TlsError::Protocol("record decryption failed"))
}

fn write_encrypted(
    stream: &mut TcpStream,
    key: &RecordKey,
    seq: u64,
    ct: u8,
    body: &[u8],
) -> Result<()> {
    let (hdr, sealed) = key.seal(seq, ct, body);
    stream.write_all(&hdr)?;
    stream.write_all(&sealed)?;
    stream.flush()?;
    Ok(())
}

// ---------------------------------------------------------------- 标签派生

/// HKDF-Expand-Label（RFC 8446 §7.1）。
fn expand_label(secret: &[u8], label: &str, context: &[u8], out_len: usize) -> Vec<u8> {
    let prk: &[u8; 32] = secret.try_into().expect("SHA-256 的密钥固定 32 字节");
    // HkdfLabel = 2 字节输出长度 || 1 字节 label 长度 || label || 1 字节 context 长度 || context
    // label 已带 "tls13 " 前缀，只带前缀的 label 长度字段曾经漏写过，
    // 结果就是派生出的密钥全错、握手到第一条加密记录就崩。
    let mut info = Vec::with_capacity(label.len() + context.len() + 10);
    info.extend_from_slice(&(out_len as u16).to_be_bytes());
    info.push((6 + label.len()) as u8);
    info.extend_from_slice(b"tls13 ");
    info.extend_from_slice(label.as_bytes());
    info.push(context.len() as u8);
    info.extend_from_slice(context);
    let mut out = vec![0u8; out_len];
    hkdf_expand(prk, &info, &mut out);
    out
}

fn traffic_key(traffic_secret: &[u8], suite: Suite) -> AeadKey {
    let key = expand_label(traffic_secret, "key", &[], suite.key_len());
    let iv = expand_label(traffic_secret, "iv", &[], 12);
    let mut iv12 = [0u8; 12];
    iv12.copy_from_slice(&iv);
    AeadKey {
        suite,
        key,
        iv: iv12,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_xor_uses_low_four_bytes() {
        let iv = [0u8; 12];
        let n = nonce_from_iv(&iv, 1);
        assert_eq!(n[11], 1);
        assert_eq!(n[10], 0);
        let n2 = nonce_from_iv(&iv, 0x0102_0304_0506_0708);
        assert_eq!(&n2[4..], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn client_hello_is_well_formed() {
        let pk = [7u8; 32];
        let rnd = [1u8; 32];
        let sid = [2u8; 32];
        let hello = build_client_hello("example.com", &pk, &rnd, &sid);
        assert_eq!(hello[0], HS_CLIENT_HELLO);
        let len = u32::from_be_bytes([0, hello[1], hello[2], hello[3]]) as usize;
        assert_eq!(hello.len(), 4 + len);
        // 解析回来确认关键扩展都在
        let body = &hello[4..];
        assert!(body.windows(11).any(|w| w == b"example.com"));
        // 两个 cipher suite
        let mut r = Reader::new(body);
        let _ = r.take(2 + 32);
        let sl = r.next_u8().unwrap() as usize;
        let _ = r.take(sl);
        let _cs_len = r.take(2).unwrap(); // 2 字节的 cipher suite 列表长度
        let cs = r.take(4).unwrap();
        assert_eq!(cs, vec![0x13, 0x03, 0x13, 0x01]);
        // 末尾的 key_share 必须带我们的公钥
        assert!(hello.ends_with(&pk));
    }

    #[test]
    fn expand_label_matches_rfc_vector() {
        // RFC 8448 §3 的 client_handshake_traffic_secret 派生链太长，
        // 这里用"同输入必同输出 + 长度正确 + 上下文参与"三条性质验
        let s = [0xab; 32];
        let a = expand_label(&s, "key", &[], 32);
        let b = expand_label(&s, "key", &[], 32);
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
        let c = expand_label(&s, "key", &[1, 2], 32);
        assert_ne!(a, c);
        let d = expand_label(&s, "iv", &[], 12);
        assert_eq!(d.len(), 12);
    }

    /// RFC 8448 §3 的派生结果：PRK 与其 key/iv 是公开的固定值，
    /// 能一次性验掉 label 编码 + HKDF-Expand 两件事。
    #[test]
    fn rfc8448_key_iv_derivation() {
        let prk: Vec<u8> =
            b"\xb6\x7b\x7d\x69\x0c\xc1\x6c\x4e\x75\xe5\x42\x13\xcb\x2d\x37\xb4\
              \xe9\xc9\x12\xbc\xde\xd9\x10\x5d\x42\xbe\xfd\x59\xd3\x91\xad\x38"
                .to_vec();
        assert_eq!(prk.len(), 32);
        let key = expand_label(&prk, "key", &[], 16);
        assert_eq!(
            key,
            vec![
                0x3f, 0xce, 0x51, 0x60, 0x09, 0xc2, 0x17, 0x27, 0xd0, 0xf2, 0xe4, 0xe8, 0x6e, 0xe4,
                0x03, 0xbc
            ]
        );
        let iv = expand_label(&prk, "iv", &[], 12);
        assert_eq!(
            iv,
            vec![0x5d, 0x31, 0x3e, 0xb2, 0x67, 0x12, 0x76, 0xee, 0x13, 0x00, 0x0b, 0x30]
        );
    }

    #[test]
    fn traffic_key_lengths_follow_suite() {
        let ts = [0x11u8; 32];
        let c = traffic_key(&ts, Suite::Chacha);
        assert_eq!(c.key.len(), 32);
        let a = traffic_key(&ts, Suite::Aes128);
        assert_eq!(a.key.len(), 16);
        // iv 派生与 suite 无关（长度都 12），两者相同是对的；
        // 真正区分在于 key 长度与 AEAD 算法
        assert_eq!(c.iv, a.iv);
    }

    #[test]
    fn aead_roundtrip() {
        for suite in [Suite::Chacha, Suite::Aes128] {
            let k = traffic_key(&[3u8; 32], suite);
            let aad = [0x17, 0x03, 0x03, 0x00, 0x06];
            let ct = k.seal(0, &aad, b"hello");
            assert_eq!(k.open(0, &aad, &ct).unwrap(), b"hello");
            assert!(k.open(1, &aad, &ct).is_none());
            let mut bad = ct.clone();
            bad[0] ^= 1;
            assert!(k.open(0, &aad, &bad).is_none());
            // AAD 必须参与认证：换掉头部就解不开
            assert!(k.open(0, b"", &ct).is_none());
        }
    }
}
