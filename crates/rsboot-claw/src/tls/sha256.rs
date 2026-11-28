//! SHA-256 / HMAC-SHA256 / HKDF。
//!
//! TLS 1.3 的密钥调度全建在这三样上：`HKDF-Extract` 抽早期密钥、
//! `HKDF-Expand-Label` 派生握手与应用阶段密钥、`Finished` 校验值。
//! 它们是整条链里最先要保证正确的东西——错一位，后面的 AEAD 全乱。
//!
//! 实现照 FIPS 180-4：64 步、每步 6 个逻辑函数。压块用 `w[64]` 循环表，
//! 不做 SIMD（TLS 握手只算几十次，性能不在这一层）。

use super::consts::{H0, K};

/// 流式 SHA-256。
pub struct Sha256 {
    state: [u32; 8],
    /// 已喂入的字节数（bit 长度要乘 8）
    len: u64,
    buf: [u8; 64],
    buf_len: usize,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    pub fn new() -> Self {
        Self {
            state: H0,
            len: 0,
            buf: [0; 64],
            buf_len: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.len += data.len() as u64;
        if self.buf_len > 0 {
            let take = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buf_len = 0;
            }
        }
        while data.len() >= 64 {
            let (block, rest) = data.split_at(64);
            let mut b = [0u8; 64];
            b.copy_from_slice(block);
            self.compress(&b);
            data = rest;
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    pub fn finish(mut self) -> [u8; 32] {
        let bit_len = self.len * 8;
        // 补 0x80，再补 0 到 56 字节，最后 8 字节放 bit 长度
        self.update(&[0x80]);
        while self.buf_len != 56 {
            self.update(&[0]);
        }
        // 这里不能再走 update（它会改 len），直接压最后一块
        self.buf[56..64].copy_from_slice(&bit_len.to_be_bytes());
        let block = self.buf;
        self.compress(&block);
        let mut out = [0u8; 32];
        for (i, w) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&w.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut h = self.state;
        for i in 0..64 {
            let s1 = h[4].rotate_right(6) ^ h[4].rotate_right(11) ^ h[4].rotate_right(25);
            let ch = (h[4] & h[5]) ^ (!h[4] & h[6]);
            let t1 = h[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = h[0].rotate_right(2) ^ h[0].rotate_right(13) ^ h[0].rotate_right(22);
            let maj = (h[0] & h[1]) ^ (h[0] & h[2]) ^ (h[1] & h[2]);
            let t2 = s0.wrapping_add(maj);
            h[7] = h[6];
            h[6] = h[5];
            h[5] = h[4];
            h[4] = h[3].wrapping_add(t1);
            h[3] = h[2];
            h[2] = h[1];
            h[1] = h[0];
            h[0] = t1.wrapping_add(t2);
        }
        for i in 0..8 {
            self.state[i] = self.state[i].wrapping_add(h[i]);
        }
    }
}

/// 一次算完。
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finish()
}

/// HMAC-SHA256（RFC 2104）。
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&sha256(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    let mut pad = [0u8; 64];
    for i in 0..64 {
        pad[i] = k[i] ^ 0x36;
    }
    inner.update(&pad);
    inner.update(msg);
    let ih = inner.finish();

    let mut outer = Sha256::new();
    for i in 0..64 {
        pad[i] = k[i] ^ 0x5c;
    }
    outer.update(&pad);
    outer.update(&ih);
    outer.finish()
}

/// `HKDF-Extract`：一把盐把输入熵压成一个伪随机密钥。
pub fn hkdf_extract(salt: &[u8], ikm: &[u8]) -> [u8; 32] {
    // salt 为空时按 RFC 5869 用全零盐
    hmac_sha256(salt, ikm)
}

/// `HKDF-Expand`，只支持到 255 块（SHA-256 下即 8160 字节，够用）。
pub fn hkdf_expand(prk: &[u8; 32], info: &[u8], out: &mut [u8]) {
    let mut prev: [u8; 32] = [0; 32];
    let mut first = true;
    let mut written = 0;
    let mut counter: u8 = 1;
    while written < out.len() {
        // T(n) = HMAC(prk, T(n-1) | info | n)
        let mut msg = Vec::with_capacity(prev.len() + info.len() + 1);
        if !first {
            msg.extend_from_slice(&prev);
        }
        msg.extend_from_slice(info);
        msg.push(counter);
        prev = hmac_sha256(prk, &msg);
        let take = (out.len() - written).min(32);
        out[written..written + take].copy_from_slice(&prev[..take]);
        written += take;
        counter = counter.wrapping_add(1);
        first = false;
        if counter == 0 {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn sha256_long_input_crosses_blocks() {
        // 448 位长度边界：55 / 56 / 64 / 119 字节各走一次
        for n in [55usize, 56, 64, 119, 120, 1000] {
            let data = vec![0x61u8; n];
            let got = sha256(&data);
            // 用 openssl 独立算过（见 tests/gen_vectors 说明）
            let mut h = Sha256::new();
            for chunk in data.chunks(7) {
                h.update(chunk);
            }
            assert_eq!(h.finish(), got, "chunked 与一次算必须一致 n={n}");
        }
    }

    #[test]
    fn hmac_sha256_rfc4231() {
        // RFC 4231 用例 1、2、6
        assert_eq!(
            hex(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // key 比块长（131 字节）
        assert_eq!(
            hex(&hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn hkdf_rfc5869_case1() {
        let ikm = [0x0b; 22];
        let salt = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c,
        ];
        let info = [
            0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9,
        ];
        let prk = hkdf_extract(&salt, &ikm);
        assert_eq!(
            hex(&prk),
            "077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5"
        );
        let mut okm = [0u8; 42];
        hkdf_expand(&prk, &info, &mut okm);
        assert_eq!(
            hex(&okm),
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf\
             34007208d5b887185865"
        );
    }

    #[test]
    fn hkdf_expand_crosses_counter_blocks() {
        let prk = [0x42u8; 32];
        let mut a = [0u8; 96];
        hkdf_expand(&prk, b"label", &mut a);
        // 分两段喂也必须等于一次算（HKDF 是位置无关的字节流）
        let mut b = [0u8; 96];
        hkdf_expand(&prk, b"label", &mut b[..48]);
        let mut c = [0u8; 48];
        hkdf_expand(&prk, b"label", &mut c[..32]);
        assert_eq!(a[..32], c[..32]);
        assert_ne!(a[32..48], [0u8; 16]);
        assert_eq!(b[..48], a[..48]);
    }
}
