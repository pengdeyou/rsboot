//! ChaCha20-Poly1305（RFC 8439）。
//!
//! 为什么先做它而不是 AES-GCM：ChaCha 不用查表（AES 的 S-box 与 T 表是缓存
//! 侧信道的老话题），实现短、常数表少，RFC 给的测试向量覆盖也最全。
//! TLS 1.3 里 `TLS_CHACHA20_POLY1305_SHA256` 是三大必备套件之一，
//! 绝大多数服务端（含 CDN）都开。
//!
//! 记录层用法（RFC 8446 §5.3）：nonce = 静态 iv 异或 64 位序号，
//! 计数器固定从 1 起（0 号块留给 Poly1305 的 key）。

/// 一轮 ChaCha 的四分之一轮函数。
#[inline(always)]
fn qr(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

fn init(key: &[u8; 32], counter: u32, nonce: &[u8; 12]) -> [u32; 16] {
    let mut s = [0u32; 16];
    s[0] = 0x6170_7865; // "expa"
    s[1] = 0x3320_646e; // "nd 3"
    s[2] = 0x7962_2d32; // "2-by"
    s[3] = 0x6b20_6574; // "te k"
    for i in 0..8 {
        s[4 + i] = u32::from_le_bytes([
            key[i * 4],
            key[i * 4 + 1],
            key[i * 4 + 2],
            key[i * 4 + 3],
        ]);
    }
    s[12] = counter;
    for i in 0..3 {
        s[13 + i] = u32::from_le_bytes([
            nonce[i * 4],
            nonce[i * 4 + 1],
            nonce[i * 4 + 2],
            nonce[i * 4 + 3],
        ]);
    }
    s
}

fn block(key: &[u8; 32], counter: u32, nonce: &[u8; 12]) -> [u8; 64] {
    let base = init(key, counter, nonce);
    let mut s = base;
    // 20 轮 = 10 次「列 + 对角」两趟
    for _ in 0..10 {
        qr(&mut s, 0, 4, 8, 12);
        qr(&mut s, 1, 5, 9, 13);
        qr(&mut s, 2, 6, 10, 14);
        qr(&mut s, 3, 7, 11, 15);
        qr(&mut s, 0, 5, 10, 15);
        qr(&mut s, 1, 6, 11, 12);
        qr(&mut s, 2, 7, 8, 13);
        qr(&mut s, 3, 4, 9, 14);
    }
    for i in 0..16 {
        s[i] = s[i].wrapping_add(base[i]);
    }
    let mut out = [0u8; 64];
    for i in 0..16 {
        out[i * 4..i * 4 + 4].copy_from_slice(&s[i].to_le_bytes());
    }
    out
}

/// 原地异或密钥流。计数器会随数据增长，因此可以跨块连续调用。
pub fn xor_stream(key: &[u8; 32], counter: u32, nonce: &[u8; 12], data: &mut [u8]) {
    let mut ctr = counter;
    for chunk in data.chunks_mut(64) {
        let ks = block(key, ctr, nonce);
        for (b, k) in chunk.iter_mut().zip(ks.iter()) {
            *b ^= *k;
        }
        ctr = ctr.wrapping_add(1);
    }
}

const M26: u64 = 0x03ff_ffff;

#[inline(always)]
fn le32(b: &[u8], i: usize) -> u64 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as u64
}

/// Poly1305 一次性 MAC。
///
/// 用的是 5 × 26 bit limb 的标准实现：乘法的 130 位折返靠 `r4 * 5`
/// （因为 2^130 ≡ 5 mod (2^130 − 5)），比全宽 big-int 快得多也短得多。
pub fn poly1305(key: &[u8; 32], msg: &[u8]) -> [u8; 16] {
    let r0 = le32(key, 0) & 0x3ff_ffff;
    let r1 = (le32(key, 3) >> 2) & 0x3ff_ff03;
    let r2 = (le32(key, 6) >> 4) & 0x3ff_c0ff;
    let r3 = (le32(key, 9) >> 6) & 0x3f0_3fff;
    let r4 = (le32(key, 12) >> 8) & 0x00f_ffff;

    let mut h = [0u64; 5];
    for chunk in msg.chunks(16) {
        // 16 字节（不足按 0 补齐）读成 5 个 26 bit limb
        let mut b = [0u8; 16];
        b[..chunk.len()].copy_from_slice(chunk);
        // 16 字节按小端读成 128 位，再切成 5 个 26 位 limb。
        // 直接按字节偏移拼 limb 很容易把跨字节的位拼错，先聚成一个 u128 最稳。
        let mut val = u128::from_le_bytes(b);
        if chunk.len() < 16 {
            // 短块的隐含 1 bit 在 2^(8*len)（≤ 2^127，能进 u128）
            val |= 1u128 << (chunk.len() * 8);
        }
        let mut m = [0u64; 5];
        for (i, mi) in m.iter_mut().enumerate() {
            *mi = ((val >> (26 * i)) & 0x3ff_ffff) as u64;
        }
        if chunk.len() == 16 {
            // 完整块的隐含 1 bit 在 2^128，即 limb4 的 bit 24
            m[4] |= 1 << 24;
        }

        let h0 = h[0] + m[0];
        let h1 = h[1] + m[1];
        let h2 = h[2] + m[2];
        let h3 = h[3] + m[3];
        let h4 = h[4] + m[4];

        let s1 = r1 * 5;
        let s2 = r2 * 5;
        let s3 = r3 * 5;
        let s4 = r4 * 5;

        let d0 = h0 * r0 + h1 * s4 + h2 * s3 + h3 * s2 + h4 * s1;
        let d1 = h0 * r1 + h1 * r0 + h2 * s4 + h3 * s3 + h4 * s2;
        let d2 = h0 * r2 + h1 * r1 + h2 * r0 + h3 * s4 + h4 * s3;
        let d3 = h0 * r3 + h1 * r2 + h2 * r1 + h3 * r0 + h4 * s4;
        let d4 = h0 * r4 + h1 * r3 + h2 * r2 + h3 * r1 + h4 * r0;

        let mut c = d0 >> 26;
        h[0] = d0 & M26;
        let d1 = d1 + c;
        c = d1 >> 26;
        h[1] = d1 & M26;
        let d2 = d2 + c;
        c = d2 >> 26;
        h[2] = d2 & M26;
        let d3 = d3 + c;
        c = d3 >> 26;
        h[3] = d3 & M26;
        let d4 = d4 + c;
        c = d4 >> 26;
        h[4] = d4 & M26;
        h[0] += c * 5;
        c = h[0] >> 26;
        h[0] &= M26;
        h[1] += c;
    }

    // 收尾：先完全进位，再看 h 是否 ≥ 2^130 − 5
    let mut c = h[1] >> 26;
    h[1] &= M26;
    h[2] += c;
    c = h[2] >> 26;
    h[2] &= M26;
    h[3] += c;
    c = h[3] >> 26;
    h[3] &= M26;
    h[4] += c;
    c = h[4] >> 26;
    h[4] &= M26;
    h[0] += c * 5;
    c = h[0] >> 26;
    h[0] &= M26;
    h[1] += c;

    // g = h + 5，若 g 仍 < 2^130（即 h < p）则保留 h
    let mut g = [0u64; 5];
    let mut carry = 5u64;
    for i in 0..4 {
        g[i] = h[i] + carry;
        carry = g[i] >> 26;
        g[i] &= M26;
    }
    let g4 = h[4] + carry;
    let overflow = g4 >> 26; // h ≥ p 时为 1
    g[4] = g4 & M26;
    let mask = 0u64.wrapping_sub(overflow);

    let mut hh = [0u64; 5];
    for i in 0..5 {
        hh[i] = (h[i] & !mask) | (g[i] & mask);
    }

    let acc: u128 = (hh[0] as u128)
        + ((hh[1] as u128) << 26)
        + ((hh[2] as u128) << 52)
        + ((hh[3] as u128) << 78)
        + ((hh[4] as u128) << 104);
    let s = u128::from_le_bytes([
        key[16], key[17], key[18], key[19], key[20], key[21], key[22], key[23], key[24],
        key[25], key[26], key[27], key[28], key[29], key[30], key[31],
    ]);
    let out = acc.wrapping_add(s);
    out.to_le_bytes()
}

/// AEAD 的 MAC 输入（RFC 8439 §2.8）：AAD 与密文各补零到 16 字节边界，
/// 再跟上两者的长度。少了这几个补零与长度块，标签永远对不上——AAD 不是
/// 直接拼在密文前面的。
fn aead_input(aad: &[u8], ct: &[u8]) -> Vec<u8> {
    let pad = |n: usize| (16 - (n % 16)) % 16;
    let mut v = Vec::with_capacity(aad.len() + ct.len() + 32);
    v.extend_from_slice(aad);
    v.extend(std::iter::repeat(0u8).take(pad(aad.len())));
    v.extend_from_slice(ct);
    v.extend(std::iter::repeat(0u8).take(pad(ct.len())));
    v.extend_from_slice(&(aad.len() as u64).to_le_bytes());
    v.extend_from_slice(&(ct.len() as u64).to_le_bytes());
    v
}

/// 加密：返回 `密文 || 16 字节 tag`。
pub fn seal(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], plain: &[u8]) -> Vec<u8> {
    let mut ct = plain.to_vec();
    xor_stream(key, 1, nonce, &mut ct);
    let poly_key = block(key, 0, nonce);
    let mut pk = [0u8; 32];
    pk.copy_from_slice(&poly_key[..32]);
    let tag = poly1305(&pk, &aead_input(aad, &ct));
    ct.extend_from_slice(&tag);
    ct
}

/// 解密：`input` 是 `密文 || tag`，校验不过返回 `None`（不泄露"哪一位错了"）。
pub fn open(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], input: &[u8]) -> Option<Vec<u8>> {
    if input.len() < 16 {
        return None;
    }
    let (ct, tag) = input.split_at(input.len() - 16);
    let poly_key = block(key, 0, nonce);
    let mut pk = [0u8; 32];
    pk.copy_from_slice(&poly_key[..32]);
    let want = poly1305(&pk, &aead_input(aad, ct));
    // 常数时间比较
    let mut diff = 0u8;
    for i in 0..16 {
        diff |= want[i] ^ tag[i];
    }
    if diff != 0 {
        return None;
    }
    let mut pt = ct.to_vec();
    xor_stream(key, 1, nonce, &mut pt);
    Some(pt)
}

/// TLS 记录层的 nonce：静态 iv 与 64 位序号异或（放在低 8 字节）。
pub fn nonce_from_iv(iv: &[u8; 12], seq: u64) -> [u8; 12] {
    let mut n = *iv;
    for i in 0..8 {
        n[4 + i] ^= (seq >> (56 - i * 8)) as u8;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn rfc8439_block_vector() {
        // RFC 8439 §2.3.2：key = 00..1f，nonce = 000000090000004a00000000，counter = 1
        let key: [u8; 32] = core::array::from_fn(|i| i as u8);
        let nonce = [0, 0, 0, 0x09, 0, 0, 0, 0x4a, 0, 0, 0, 0];
        let ks = block(&key, 1, &nonce);
        assert_eq!(hex(&ks[..16]), "10f1e7e4d13b5915500fdd1fa32071c4");
        assert_eq!(
            hex(&ks),
            "10f1e7e4d13b5915500fdd1fa32071c4c7d1f4c733c068030422aa9ac3d46c4e\
             d2826446079faa0914c2d705d98b02a2b5129cd1de164eb9cbd083e8a2503c4e"
        );
    }

    #[test]
    fn rfc8439_aead_vector() {
        // RFC 8439 §2.8.2
        let key: [u8; 32] = [
            0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x8b, 0x8c, 0x8d,
            0x8e, 0x8f, 0x90, 0x91, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0x9b,
            0x9c, 0x9d, 0x9e, 0x9f,
        ];
        let nonce = [
            0x07, 0x00, 0x00, 0x00, 0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47,
        ];
        let aad = unhex("50515253c0c1c2c3c4c5c6c7");
        let plain = b"Ladies and Gentlemen of the class of '99: If I could offer you \
only one tip for the future, sunscreen would be it.";
        let got = seal(&key, &nonce, &aad, plain);
        let want = concat!(
            "d31a8d34648e60db7b86afbc53ef7ec2a4aded51296e08fea9e2b5a736ee62d6",
            "3dbea45e8ca9671282fafb69da92728b1a71de0a9e060b2905d6a5b67ecd3b36",
            "92ddbd7f2d778b8c9803aee328091b58fab324e4fad675945585808b4831d7bc",
            "3ff4def08e4b7a9de576d26586cec64b6116",
            "1ae10b594f09e26a7e902ecbd0600691"
        );
        assert_eq!(hex(&got), want);
        // 回解
        let back = open(&key, &nonce, &aad, &got).unwrap();
        assert_eq!(back, plain.to_vec());
        // tag 改一个 bit 必须被拒
        let mut bad = got.clone();
        let n = bad.len();
        bad[n - 1] ^= 1;
        assert!(open(&key, &nonce, &aad, &bad).is_none());
        // aad 改了也必须被拒
        let mut aad2 = aad.clone();
        aad2[0] ^= 1;
        assert!(open(&key, &nonce, &aad2, &got).is_none());
    }

    #[test]
    fn poly1305_rfc8439_vector() {
        // RFC 8439 §2.5.2
        let key: [u8; 32] = [
            0x85, 0xd6, 0xbe, 0x78, 0x57, 0x55, 0x6d, 0x33, 0x7f, 0x44, 0x52, 0xfe, 0x42, 0xd5,
            0x06, 0xa8, 0x01, 0x03, 0x80, 0x8a, 0xfb, 0x0d, 0xb2, 0xfd, 0x4a, 0xbf, 0xf6, 0xaf,
            0x41, 0x49, 0xf5, 0x1b,
        ];
        let msg = b"Cryptographic Forum Research Group";
        assert_eq!(hex(&poly1305(&key, msg)), "a8061dc1305136c6c22b8baf0c0127a9");
    }

    #[test]
    fn nonce_xors_the_sequence_number() {
        let iv = [0u8; 12];
        assert_eq!(nonce_from_iv(&iv, 1)[11], 1);
        assert_eq!(nonce_from_iv(&iv, 0x0102_0304_0506_0708)[4..], [1, 2, 3, 4, 5, 6, 7, 8]);
        let iv = [0xffu8; 12];
        assert_eq!(nonce_from_iv(&iv, 0)[..], [0xff; 12]);
    }

    #[test]
    fn empty_plaintext_still_has_a_tag() {
        let key = [7u8; 32];
        let nonce = [9u8; 12];
        let ct = seal(&key, &nonce, b"aad", b"");
        assert_eq!(ct.len(), 16);
        assert_eq!(open(&key, &nonce, b"aad", &ct).unwrap(), Vec::<u8>::new());
    }

}
