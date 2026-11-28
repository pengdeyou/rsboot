//! AES-128-GCM（NIST SP 800-38D）。
//!
//! 为什么还留着 AES 而不是只做 ChaCha：有些服务端（尤其硬件加速的老网关）
//! 只开 `TLS_AES_128_GCM_SHA256`。客户端 offer 里把 ChaCha 放前面就够了，
//! 但 AES 这条兼容路径必须存在，否则连不上。
//!
//! GHASH 的有限域乘法用最朴素的版本（128 次条件异或）。它慢——大约比
//! 4 bit 查表版本慢一个数量级——但：一、握手阶段只有几百字节；二、
//! 只做客户端时 AES 是兜底路径；三、常数表越少，越没有缓存侧信道的话题。
//! 真要跑大流量，换 ChaCha 套件就是了。

use super::consts::SBOX;

fn xtime(a: u8) -> u8 {
    (a << 1) ^ if a & 0x80 != 0 { 0x1b } else { 0 }
}

fn expand_key(key: &[u8; 16]) -> [u8; 176] {
    let mut w = [0u8; 176];
    w[..16].copy_from_slice(key);
    let mut rcon = 1u8;
    for i in 4..44 {
        let mut t = [
            w[(i - 1) * 4],
            w[(i - 1) * 4 + 1],
            w[(i - 1) * 4 + 2],
            w[(i - 1) * 4 + 3],
        ];
        if i % 4 == 0 {
            t = [
                SBOX[t[1] as usize] ^ rcon,
                SBOX[t[2] as usize],
                SBOX[t[3] as usize],
                SBOX[t[0] as usize],
            ];
            rcon = xtime(rcon);
        }
        for j in 0..4 {
            w[i * 4 + j] = w[(i - 4) * 4 + j] ^ t[j];
        }
    }
    w
}

fn shift_rows(s: &mut [u8; 16]) {
    let t = *s;
    for r in 0..4 {
        for c in 0..4 {
            s[r + 4 * c] = t[r + 4 * ((c + r) % 4)];
        }
    }
}

fn mix_columns(s: &mut [u8; 16]) {
    for c in 0..4 {
        let i = 4 * c;
        let a0 = s[i];
        let a1 = s[i + 1];
        let a2 = s[i + 2];
        let a3 = s[i + 3];
        s[i] = xtime(a0) ^ xtime(a1) ^ a1 ^ a2 ^ a3;
        s[i + 1] = a0 ^ xtime(a1) ^ xtime(a2) ^ a2 ^ a3;
        s[i + 2] = a0 ^ a1 ^ xtime(a2) ^ xtime(a3) ^ a3;
        s[i + 3] = xtime(a0) ^ a0 ^ a1 ^ a2 ^ xtime(a3);
    }
}

fn encrypt_block(rk: &[u8; 176], block: &[u8; 16]) -> [u8; 16] {
    let mut s = *block;
    for j in 0..16 {
        s[j] ^= rk[j];
    }
    for round in 1..10 {
        for b in s.iter_mut() {
            *b = SBOX[*b as usize];
        }
        shift_rows(&mut s);
        mix_columns(&mut s);
        for j in 0..16 {
            s[j] ^= rk[round * 16 + j];
        }
    }
    for b in s.iter_mut() {
        *b = SBOX[*b as usize];
    }
    shift_rows(&mut s);
    for j in 0..16 {
        s[j] ^= rk[160 + j];
    }
    s
}

/// GHASH 里的 GF(2^128) 乘法：GCM 的位序是"最左为 x^0"。
fn gf_mul(x: u128, y: u128) -> u128 {
    let mut z = 0u128;
    let mut v = y;
    for i in 0..128 {
        if (x >> (127 - i)) & 1 == 1 {
            z ^= v;
        }
        let lsb = v & 1;
        v >>= 1;
        if lsb == 1 {
            v ^= 0xe100_0000_0000_0000_0000_0000_0000_0000;
        }
    }
    z
}

fn read_block(b: &[u8]) -> u128 {
    let mut a = [0u8; 16];
    let n = b.len().min(16);
    a[..n].copy_from_slice(&b[..n]);
    u128::from_be_bytes(a)
}

/// GHASH：AAD 与密文各补零到 16 字节，最后带一块两个长度。
fn ghash(h: u128, aad: &[u8], ct: &[u8]) -> u128 {
    let mut y = 0u128;
    let feed = |data: &[u8], y: &mut u128| {
        for chunk in data.chunks(16) {
            *y = gf_mul(*y ^ read_block(chunk), h);
        }
    };
    feed(aad, &mut y);
    feed(ct, &mut y);
    // 长度块是**比特**数（SP 800-38D §2.5），不是字节数——RFC 8439 的
    // ChaCha20-Poly1305 用的是字节数，两边别弄混
    let lens = (((aad.len() as u128) * 8) << 64) | ((ct.len() as u128) * 8);
    gf_mul(y ^ lens, h)
}

fn gcm_ctr(rk: &[u8; 176], nonce: &[u8; 12], data: &mut [u8]) {
    let mut ctr: u32 = 2; // 1 是 J0 自身（给 tag 用）
    for chunk in data.chunks_mut(16) {
        let mut blk = [0u8; 16];
        blk[..12].copy_from_slice(nonce);
        blk[12..].copy_from_slice(&ctr.to_be_bytes());
        let ks = encrypt_block(rk, &blk);
        for (b, k) in chunk.iter_mut().zip(ks.iter()) {
            *b ^= *k;
        }
        ctr = ctr.wrapping_add(1);
    }
}

fn setup(key: &[u8; 16], nonce: &[u8; 12]) -> ([u8; 176], u128, u128) {
    let rk = expand_key(key);
    let h = u128::from_be_bytes(encrypt_block(&rk, &[0u8; 16]));
    let mut j0 = [0u8; 16];
    j0[..12].copy_from_slice(nonce);
    j0[15] = 1;
    let ej0 = u128::from_be_bytes(encrypt_block(&rk, &j0));
    (rk, h, ej0)
}

/// 加密，返回 `密文 || 16 字节 tag`（接口与 `chacha::seal` 一致）。
pub fn seal(key: &[u8; 16], nonce: &[u8; 12], aad: &[u8], plain: &[u8]) -> Vec<u8> {
    let (rk, h, ej0) = setup(key, nonce);
    let mut ct = plain.to_vec();
    gcm_ctr(&rk, nonce, &mut ct);
    let tag = ghash(h, aad, &ct) ^ ej0;
    ct.extend_from_slice(&tag.to_be_bytes());
    ct
}

/// 解密，tag 不对返回 `None`。
pub fn open(key: &[u8; 16], nonce: &[u8; 12], aad: &[u8], input: &[u8]) -> Option<Vec<u8>> {
    if input.len() < 16 {
        return None;
    }
    let (ct, tag) = input.split_at(input.len() - 16);
    let (rk, h, ej0) = setup(key, nonce);
    let want = ghash(h, aad, ct) ^ ej0;
    // 常数时间比较整个 16 字节
    let mut diff = 0u8;
    let want_b = want.to_be_bytes();
    for i in 0..16 {
        diff |= want_b[i] ^ tag[i];
    }
    if diff != 0 {
        return None;
    }
    let mut pt = ct.to_vec();
    gcm_ctr(&rk, nonce, &mut pt);
    Some(pt)
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
    fn aes_block_fips197() {
        // FIPS-197 附录 B / C.1
        let key: [u8; 16] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let pt: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let rk = expand_key(&key);
        assert_eq!(
            hex(&encrypt_block(&rk, &pt)),
            "69c4e0d86a7b0430d8cdb78070b4c55a"
        );
    }

    #[test]
    fn gcm_nist_vectors() {
        // NIST GCM 测试用例：全零 key/iv，空 AAD
        let key = [0u8; 16];
        let nonce = [0u8; 12];
        let ct = seal(&key, &nonce, b"", b"");
        assert_eq!(hex(&ct), "58e2fccefa7e3061367f1d57a4e7455a");

        // 16 字节全零明文
        let pt = [0u8; 16];
        let ct = seal(&key, &nonce, b"", &pt);
        assert_eq!(
            hex(&ct),
            "0388dace60b6a392f328c2b971b2fe78ab6e47d42cec13bdf53a67b21257bddf"
        );
        assert_eq!(open(&key, &nonce, b"", &ct).unwrap(), pt.to_vec());
    }

    #[test]
    fn gcm_with_aad_and_odd_lengths() {
        let key = [0x42u8; 16];
        let nonce = [0x17u8; 12];
        for n in [0usize, 1, 15, 16, 17, 63] {
            for aad_len in [0usize, 5, 16, 20] {
                let pt: Vec<u8> = (0..n).map(|i| (i * 7) as u8).collect();
                let aad: Vec<u8> = (0..aad_len).map(|i| (i * 3) as u8).collect();
                let ct = seal(&key, &nonce, &aad, &pt);
                assert_eq!(ct.len(), pt.len() + 16);
                assert_eq!(open(&key, &nonce, &aad, &ct).unwrap(), pt);
                // 改一个字节必须失败
                let mut bad = ct.clone();
                bad[0] ^= 0x80;
                assert!(open(&key, &nonce, &aad, &bad).is_none(), "n={n} aad={aad_len}");
            }
        }
    }

    #[test]
    fn gcm_known_128bit_vector() {
        // NIST SP 800-38D 用例 3（key/iv 有值、非空 AAD/明文）
        let key = unhex("feffe9928665731c6d6a8f9467308308");
        let mut k = [0u8; 16];
        k.copy_from_slice(&key);
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&unhex("cafebabefacedbaddecaf888"));
        let pt = unhex(
            "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a72\
             1c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b39",
        );
        let aad = unhex("feedfacedeadbeeffeedfacedeadbeefabaddad2");
        let ct = seal(&k, &nonce, &aad, &pt);
        assert_eq!(
            hex(&ct),
            "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e\
             21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e091\
             5bc94fbc3221a5db94fae95ae7121a47"
        );
        assert_eq!(open(&k, &nonce, &aad, &ct).unwrap(), pt);
    }
}
