//! Curve25519 的 X25519 密钥交换（RFC 7748）。
//!
//! TLS 1.3 里 `x25519` 是事实上的默认组：字节最少、实现最短、没有 P-256
//! 那种"点是否在曲线上"的历史包袱（Montgomery ladder 只碰 x 坐标）。
//!
//! 实现用 16 个 16 bit limb 的表示（`i64` 存，容忍中间值为负），乘法照
//! RFC 7748 §5 的双点 ladder。这是 Curve25519 里最不容易写错的一种写法：
//! 没有任何查表、没有条件分支依赖秘密数据（cswap 用掩码），
//! 天然躲开缓存侧信道。
//!
//! 单位注意：`car` 里的 `1 << 16` 与 `38 * (c - 1)` 是 2^(9×16) ≡ 38 的折返，
//! 这是 2^255 − 19 在 16-limb 表示下的必然结果，不是随手写的常数。

type Fe = [i64; 16];

/// 基点（u = 9）。
pub const BASEPOINT: [u8; 32] = {
    let mut b = [0u8; 32];
    b[0] = 9;
    b
};

/// 规范化：把每个 limb 压回 [0, 2^16)，溢出折回 2^255 − 19。
fn car(o: &mut Fe) {
    for i in 0..16 {
        o[i] += 1 << 16;
        let c = o[i] >> 16;
        if i < 15 {
            o[i + 1] += c - 1;
        } else {
            // 2^256 ≡ 38 (mod 2^255 − 19)
            o[0] += 38 * (c - 1);
        }
        o[i] -= c << 16;
    }
}

fn add(o: &mut Fe, a: &Fe, b: &Fe) {
    for i in 0..16 {
        o[i] = a[i] + b[i];
    }
    car(o);
}

fn sub(o: &mut Fe, a: &Fe, b: &Fe) {
    for i in 0..16 {
        o[i] = a[i] - b[i];
    }
    car(o);
}

fn mul(o: &mut Fe, a: &Fe, b: &Fe) {
    let mut t = [0i64; 31];
    for i in 0..16 {
        for j in 0..16 {
            t[i + j] += a[i] * b[j];
        }
    }
    for i in 0..15 {
        t[i] += 38 * t[i + 16];
    }
    o.copy_from_slice(&t[..16]);
    car(o);
    car(o);
}

fn sq(o: &mut Fe, a: &Fe) {
    let mut t = [0i64; 31];
    for i in 0..16 {
        for j in 0..16 {
            t[i + j] += a[i] * a[j];
        }
    }
    for i in 0..15 {
        t[i] += 38 * t[i + 16];
    }
    o.copy_from_slice(&t[..16]);
    car(o);
    car(o);
}

fn mul_small(o: &mut Fe, a: &Fe, k: i64) {
    for i in 0..16 {
        o[i] = a[i] * k;
    }
    car(o);
    car(o);
}

/// 常数时间条件交换：`b` 为 1 时交换，0 时不动。掩码而不是分支。
fn cswap(p: &mut Fe, q: &mut Fe, b: i64) {
    let c = !(b - 1);
    for i in 0..16 {
        let t = c & (p[i] ^ q[i]);
        p[i] ^= t;
        q[i] ^= t;
    }
}

fn unpack(n: &[u8; 32]) -> Fe {
    let mut o = [0i64; 16];
    for i in 0..16 {
        o[i] = n[2 * i] as i64 + ((n[2 * i + 1] as i64) << 8);
    }
    // u 的最高位按 RFC 7748 忽略
    o[15] &= 0x7fff;
    o
}

fn pack(n: &Fe) -> [u8; 32] {
    let mut t = *n;
    car(&mut t);
    car(&mut t);
    car(&mut t);
    let mut m = [0i64; 16];
    // 两轮条件减：把 t 从"可能 ≥ p"压到 [0, p)
    for _ in 0..2 {
        m[0] = t[0] - 0xffed;
        for i in 1..15 {
            m[i] = t[i] - 0xffff - ((m[i - 1] >> 16) & 1);
            m[i - 1] &= 0xffff;
        }
        m[15] = t[15] - 0x7fff - ((m[14] >> 16) & 1);
        let b = (m[15] >> 16) & 1;
        m[14] &= 0xffff;
        cswap(&mut t, &mut m, 1 - b);
    }
    let mut o = [0u8; 32];
    for i in 0..16 {
        o[2 * i] = (t[i] & 0xff) as u8;
        o[2 * i + 1] = ((t[i] >> 8) & 0xff) as u8;
    }
    o
}

/// z^(p−2)，即模逆元。指数链：`2^255 − 21` 的二进制里除第 2、4 位外全是 1。
fn inv(a: &Fe) -> Fe {
    let mut c = *a;
    for i in (0..=253).rev() {
        let mut s = [0i64; 16];
        sq(&mut s, &c);
        c = s;
        if i != 2 && i != 4 {
            let mut m = [0i64; 16];
            mul(&mut m, &c, a);
            c = m;
        }
    }
    c
}

/// X25519：`scalar × point`。
pub fn scalarmult(scalar: &[u8; 32], point: &[u8; 32]) -> [u8; 32] {
    let mut k = *scalar;
    // RFC 7748 的钳位：低 3 位清零、最高位清零并置次高位
    k[0] &= 248;
    k[31] = (k[31] & 127) | 64;

    let x1 = unpack(point);
    let mut x2 = [0i64; 16];
    x2[0] = 1;
    let mut z2 = [0i64; 16];
    let mut x3 = x1;
    let mut z3 = [0i64; 16];
    z3[0] = 1;
    let mut swap = 0i64;

    for t in (0..=254).rev() {
        let kt = ((k[t / 8] >> (t % 8)) & 1) as i64;
        swap ^= kt;
        cswap(&mut x2, &mut x3, swap);
        cswap(&mut z2, &mut z3, swap);
        swap = kt;

        let mut a = [0i64; 16];
        add(&mut a, &x2, &z2);
        let mut aa = [0i64; 16];
        sq(&mut aa, &a);
        let mut b = [0i64; 16];
        sub(&mut b, &x2, &z2);
        let mut bb = [0i64; 16];
        sq(&mut bb, &b);
        let mut e = [0i64; 16];
        sub(&mut e, &aa, &bb);
        let mut c = [0i64; 16];
        add(&mut c, &x3, &z3);
        let mut d = [0i64; 16];
        sub(&mut d, &x3, &z3);
        let mut da = [0i64; 16];
        mul(&mut da, &d, &a);
        let mut cb = [0i64; 16];
        mul(&mut cb, &c, &b);

        let mut sum = [0i64; 16];
        add(&mut sum, &da, &cb);
        sq(&mut x3, &sum);
        let mut diff = [0i64; 16];
        sub(&mut diff, &da, &cb);
        let mut diff2 = [0i64; 16];
        sq(&mut diff2, &diff);
        mul(&mut z3, &x1, &diff2);

        mul(&mut x2, &aa, &bb);
        // a24 = 121665（RFC 7748；不是 121666，那个是曲线方程里的 a+2 的一半）
        let mut e121 = [0i64; 16];
        mul_small(&mut e121, &e, 121665);
        let mut t2 = [0i64; 16];
        add(&mut t2, &aa, &e121);
        mul(&mut z2, &e, &t2);
    }
    cswap(&mut x2, &mut x3, swap);
    cswap(&mut z2, &mut z3, swap);

    let zinv = inv(&z2);
    let mut out = [0i64; 16];
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
        // 0 与 1 理论上不该被用来算密钥，但 ladder 不能因为输入退化就崩
        let zero = [0u8; 32];
        let one = {
            let mut b = [0u8; 32];
            b[0] = 1;
            b
        };
        let _ = scalarmult(&zero, &BASEPOINT);
        let _ = scalarmult(&one, &BASEPOINT);
        // 每个字节都改一下也能跑完（不 panic、不出错）
        let mut k = [0x42u8; 32];
        for i in 0..32 {
            k[i] = i as u8;
            let _ = public_key(&k);
        }
    }

    #[test]
    fn pack_roundtrip_of_small_values() {
        let mut f = [0i64; 16];
        f[0] = 9;
        assert_eq!(pack(&f), BASEPOINT);
        // 2^255 − 19 应规约成 0
        let mut p = [0i64; 16];
        p[0] = 0xffed;
        p[1..15].fill(0xffff);
        p[15] = 0x7fff;
        assert_eq!(pack(&p), [0u8; 32], "p ≡ 0 mod p");
    }
}
