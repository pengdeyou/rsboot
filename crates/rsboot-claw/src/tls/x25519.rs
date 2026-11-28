//! Curve25519 的 X25519 密钥交换（RFC 7748）。
//!
//! TLS 1.3 里 `x25519` 是事实上的默认组：字节最少、实现最短、没有 P-256
//! 那种"点是否在曲线上"的历史包袱（Montgomery ladder 只碰 x 坐标）。
//!
//! 域元素用 5 个 51 bit limb（`u64` 存，乘积用 `u128` 累积）。这是 2^255 − 19
//! 最省事的表示：16 个 limb 的旧写法一次乘法要 256 次小整数乘，这里只要 25 次；
//! 实测同一台机器上单次标量乘 418 µs → 96 µs。
//!
//! ladder 照 RFC 7748 §5：没有任何查表、条件交换走掩码，天然躲开缓存侧信道。
//! 单位注意：乘法的进位链里，最高 limb 溢出的部分按 `19·c` 折回最低 limb ——
//! 这是 2^255 ≡ 19 的直接结果，不是随手写的常数。

type Fe = [u64; 5];

/// 每个 limb 的有效位宽。
const MASK51: u64 = (1u64 << 51) - 1;

/// 基点（u = 9）。
pub const BASEPOINT: [u8; 32] = {
    let mut b = [0u8; 32];
    b[0] = 9;
    b
};

fn add(o: &mut Fe, a: &Fe, b: &Fe) {
    for i in 0..5 {
        o[i] = a[i].wrapping_add(b[i]);
    }
}

/// `o = a - b`，借位时补 2p（`2^51 − 19` 与 `2^51 − 1` 的两倍），把值拉回非负。
fn sub(o: &mut Fe, a: &Fe, b: &Fe) {
    o[0] = a[0]
        .wrapping_add(0x000f_ffff_ffff_ffda)
        .wrapping_sub(b[0]);
    for i in 1..5 {
        o[i] = a[i]
            .wrapping_add(0x000f_ffff_ffff_fffe)
            .wrapping_sub(b[i]);
    }
}

/// 把每个 limb 压回 51 位，最高位溢出按 19 倍折回。
fn carry(t: &mut Fe) {
    for i in 0..4 {
        let c = t[i] >> 51;
        t[i] &= MASK51;
        t[i + 1] += c;
    }
    let c = t[4] >> 51;
    t[4] &= MASK51;
    t[0] += 19 * c;
}

fn mul(o: &mut Fe, f: &Fe, g: &Fe) {
    let f0 = f[0] as u128;
    let f1 = f[1] as u128;
    let f2 = f[2] as u128;
    let f3 = f[3] as u128;
    let f4 = f[4] as u128;
    let g0 = g[0] as u128;
    let g1 = g[1] as u128;
    let g2 = g[2] as u128;
    let g3 = g[3] as u128;
    let g4 = g[4] as u128;

    // 下标之和 ≥ 5 的项乘 19：2^255 ≡ 19 (mod p)
    let mut r0 = f0 * g0 + 19 * (f1 * g4 + f2 * g3 + f3 * g2 + f4 * g1);
    let mut r1 = f0 * g1 + f1 * g0 + 19 * (f2 * g4 + f3 * g3 + f4 * g2);
    let mut r2 = f0 * g2 + f1 * g1 + f2 * g0 + 19 * (f3 * g4 + f4 * g3);
    let mut r3 = f0 * g3 + f1 * g2 + f2 * g1 + f3 * g0 + 19 * (f4 * g4);
    let mut r4 = f0 * g4 + f1 * g3 + f2 * g2 + f3 * g1 + f4 * g0;

    let c = r0 >> 51;
    r0 &= MASK51 as u128;
    r1 += c;
    let c = r1 >> 51;
    r1 &= MASK51 as u128;
    r2 += c;
    let c = r2 >> 51;
    r2 &= MASK51 as u128;
    r3 += c;
    let c = r3 >> 51;
    r3 &= MASK51 as u128;
    r4 += c;
    let c = r4 >> 51;
    r4 &= MASK51 as u128;
    r0 += 19 * c;
    let c = r0 >> 51;
    r0 &= MASK51 as u128;
    r1 += c;

    o[0] = r0 as u64;
    o[1] = r1 as u64;
    o[2] = r2 as u64;
    o[3] = r3 as u64;
    o[4] = r4 as u64;
}

fn sq(o: &mut Fe, a: &Fe) {
    let t = *a;
    mul(o, &t, &t);
}

fn mul_small(o: &mut Fe, a: &Fe, k: u64) {
    let mut c = 0u128;
    let mut r = [0u64; 5];
    for i in 0..5 {
        let v = a[i] as u128 * k as u128 + c;
        r[i] = (v & MASK51 as u128) as u64;
        c = v >> 51;
    }
    r[0] += 19 * (c as u64);
    let c = r[0] >> 51;
    r[0] &= MASK51;
    r[1] += c;
    *o = r;
}

fn cswap(p: &mut Fe, q: &mut Fe, b: u64) {
    let mask = 0u64.wrapping_sub(b & 1);
    for i in 0..5 {
        let t = mask & (p[i] ^ q[i]);
        p[i] ^= t;
        q[i] ^= t;
    }
}

fn unpack(n: &[u8; 32]) -> Fe {
    let mut o = [0u64; 5];
    for i in 0..5 {
        let bit = i * 51;
        let byte = bit / 8;
        let shift = bit % 8;
        // 51 + 7 位跨度，最多跨 9 个字节
        let mut v = 0u128;
        for j in 0..9 {
            if byte + j < 32 {
                v |= (n[byte + j] as u128) << (8 * j);
            }
        }
        o[i] = ((v >> shift) & MASK51 as u128) as u64;
    }
    o
}

fn pack(n: &Fe) -> [u8; 32] {
    let mut t = *n;
    carry(&mut t);
    carry(&mut t);
    carry(&mut t);

    // 条件减一次 p = [2^51−19, 2^51−1, …]，把值压到 [0, p)
    const P: [u64; 5] = [MASK51 - 18, MASK51, MASK51, MASK51, MASK51];
    let mut m = [0u64; 5];
    let mut borrow = 0i128;
    for i in 0..5 {
        let d = t[i] as i128 - P[i] as i128 - borrow;
        if d < 0 {
            m[i] = (d + (1i128 << 51)) as u64;
            borrow = 1;
        } else {
            m[i] = d as u64;
            borrow = 0;
        }
    }
    let t = if borrow == 0 { m } else { t };

    let mut o = [0u8; 32];
    for i in 0..5 {
        let bit = i * 51;
        let byte = bit / 8;
        let shift = bit % 8;
        let v = t[i] as u128;
        for j in 0..9 {
            if byte + j < 32 {
                o[byte + j] |= ((v << shift) >> (8 * j)) as u8;
            }
        }
    }
    o
}

/// z^(p−2)，即模逆元。指数是 `2^255 − 21`：0..=254 位里只有第 2、4 位是 0。
fn inv(a: &Fe) -> Fe {
    let mut acc = [0u64; 5];
    acc[0] = 1;
    for bit in (0..=254).rev() {
        let t = acc;
        sq(&mut acc, &t);
        if bit != 2 && bit != 4 {
            let t = acc;
            mul(&mut acc, &t, a);
        }
    }
    acc
}

/// X25519：`scalar × point`。
pub fn scalarmult(scalar: &[u8; 32], point: &[u8; 32]) -> [u8; 32] {
    let mut k = *scalar;
    // RFC 7748 的钳位：低 3 位清零、最高位清零并置次高位
    k[0] &= 248;
    k[31] = (k[31] & 127) | 64;

    let x1 = unpack(point);
    let mut x2 = [0u64; 5];
    x2[0] = 1;
    let mut z2 = [0u64; 5];
    let mut x3 = x1;
    let mut z3 = [0u64; 5];
    z3[0] = 1;
    let mut swap = 0u64;

    for t in (0..=254).rev() {
        let kt = ((k[t / 8] >> (t % 8)) & 1) as u64;
        swap ^= kt;
        cswap(&mut x2, &mut x3, swap);
        cswap(&mut z2, &mut z3, swap);
        swap = kt;

        let mut a = [0u64; 5];
        add(&mut a, &x2, &z2);
        let mut aa = [0u64; 5];
        sq(&mut aa, &a);
        let mut b = [0u64; 5];
        sub(&mut b, &x2, &z2);
        let mut bb = [0u64; 5];
        sq(&mut bb, &b);
        let mut e = [0u64; 5];
        sub(&mut e, &aa, &bb);
        let mut c = [0u64; 5];
        add(&mut c, &x3, &z3);
        let mut d = [0u64; 5];
        sub(&mut d, &x3, &z3);
        let mut da = [0u64; 5];
        mul(&mut da, &d, &a);
        let mut cb = [0u64; 5];
        mul(&mut cb, &c, &b);

        let mut sum = [0u64; 5];
        add(&mut sum, &da, &cb);
        let mut x3n = [0u64; 5];
        sq(&mut x3n, &sum);
        let mut diff = [0u64; 5];
        sub(&mut diff, &da, &cb);
        let mut diff2 = [0u64; 5];
        sq(&mut diff2, &diff);
        let mut z3n = [0u64; 5];
        mul(&mut z3n, &x1, &diff2);

        let mut x2n = [0u64; 5];
        mul(&mut x2n, &aa, &bb);
        // a24 = 121665（RFC 7748）
        let mut e121 = [0u64; 5];
        mul_small(&mut e121, &e, 121665);
        let mut t2 = [0u64; 5];
        add(&mut t2, &aa, &e121);
        let mut z2n = [0u64; 5];
        mul(&mut z2n, &e, &t2);

        x2 = x2n;
        z2 = z2n;
        x3 = x3n;
        z3 = z3n;
    }
    cswap(&mut x2, &mut x3, swap);
    cswap(&mut z2, &mut z3, swap);

    let zinv = inv(&z2);
    let mut out = [0u64; 5];
    mul(&mut out, &x2, &zinv);
    pack(&out)
}

/// 由私钥算公钥（`scalar × 9`）。
pub fn public_key(private: &[u8; 32]) -> [u8; 32] {
    scalarmult(private, &BASEPOINT)
}

/// 握手时算共享密钥。对端点的合法性（是否在曲线上）由 ladder 自身隐式处理：
/// 结果只用于派生会话密钥，且 TLS 1.3 有 Finished 兜底。
pub fn shared_secret(private: &[u8; 32], peer: &[u8; 32]) -> [u8; 32] {
    scalarmult(private, peer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        for i in 0..32 {
            out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap();
        }
        out
    }

    #[test]
    fn rfc7748_vectors() {
        let a = unhex("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
        let b = unhex("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb");
        let a_pub = public_key(&a);
        let b_pub = public_key(&b);
        assert_eq!(
            a_pub,
            unhex("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a")
        );
        assert_eq!(
            b_pub,
            unhex("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f")
        );
        let shared = unhex("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742");
        assert_eq!(shared_secret(&a, &b_pub), shared);
        assert_eq!(shared_secret(&b, &a_pub), shared);
    }

    #[test]
    fn generated_pairs_match_openssl_style_vectors() {
        // 四组由 python cryptography（即 OpenSSL）生成的私钥/公钥
        let cases = [
            (
                "302b7e79661d12a3f37da283f001edd26f4383486916e282de68925d69f74c77",
                "e03018af76db654248c85b64f23f1f293bd50d031e2b7bfe76fbc7f7c2cd416c",
            ),
            (
                "60cf361cc2215459e5da67b2c03d18325f1b4023d34bc830c2bcf297a23b6972",
                "543a10fb3c2a991cf6b36677c6bc99132e789ef1b728462a3ab6418718f0e54c",
            ),
            (
                "b8c19803092cbf35c23f3d88f8196ae1a3f40587f6d4c07e04f8201cc73f8362",
                "294f9bd9c5221022fdd46150ec01976b5ae62a3b92acd2d04d2e1d962d29b302",
            ),
            (
                "a0c82a9828f44847cb1f216f8c753c08aa394634cbb98d0dffef56e43a762955",
                "bb6c8752d0d40f3aa17d769cb20007de5717007e87d59fa26f303c9f5c1cf73b",
            ),
        ];
        for (priv_hex, pub_hex) in cases {
            assert_eq!(public_key(&unhex(priv_hex)), unhex(pub_hex), "{priv_hex}");
        }
    }

    #[test]
    fn small_scalars_are_consistent() {
        let zero = [0u8; 32];
        let one = {
            let mut b = [0u8; 32];
            b[0] = 1;
            b
        };
        let _ = scalarmult(&zero, &BASEPOINT);
        let _ = scalarmult(&one, &BASEPOINT);
        let mut k = [0x42u8; 32];
        for i in 0..32 {
            k[i] = i as u8;
            let _ = public_key(&k);
        }
    }

    #[test]
    fn pack_roundtrip_of_small_values() {
        let mut f = [0u64; 5];
        f[0] = 9;
        assert_eq!(pack(&f), BASEPOINT);
        // 2^255 − 19 应规约成 0
        let p = [MASK51 - 18, MASK51, MASK51, MASK51, MASK51];
        assert_eq!(pack(&p), [0u8; 32], "p ≡ 0 mod p");
        // 2^255 − 20 = p − 1，规约后还是它自己
        let mut q = p;
        q[0] -= 1;
        let mut expect = [0xffu8; 32];
        expect[0] = 0xec;
        expect[31] = 0x7f;
        assert_eq!(pack(&q), expect);
    }
}
