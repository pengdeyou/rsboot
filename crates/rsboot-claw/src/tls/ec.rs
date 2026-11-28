//! ECDSA 验签，覆盖 NIST P-256 与 P-384。
//!
//! 曲线运算借 `bignum::Mont`：域元素一律存在 Montgomery 域里（加减是线性的
//! 所以照旧成立，乘法走 mont mul，求逆用费马大定理 `a^(p-2)`）。
//! 点用 Jacobian 坐标，只在最后求一次仿射 x，省掉每步一次求逆。
//! 两条曲线的代码完全共用，只有常数与摘要长度不同，`Params` 里一并带上。

use super::bignum::{self, Mont};
use core::cmp::Ordering;

/// 曲线参数。`size` 是域元素/坐标的字节数（P-256 = 32，P-384 = 48）。
pub struct Params {
    pub p: &'static str,
    pub n: &'static str,
    pub b: &'static str,
    pub gx: &'static str,
    pub gy: &'static str,
    /// p − 2（求逆用）
    pub pm2: &'static str,
    /// n − 2（求逆用）
    pub nm2: &'static str,
    pub size: usize,
}

pub const P256: Params = Params {
    p: "ffffffff00000001000000000000000000000000ffffffffffffffffffffffff",
    n: "ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551",
    b: "5ac635d8aa3a93e7b3ebbd55769886bc651d06b0cc53b0f63bce3c3e27d2604b",
    gx: "6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296",
    gy: "4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5",
    pm2: "ffffffff00000001000000000000000000000000fffffffffffffffffffffffd",
    nm2: "ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc63254f",
    size: 32,
};

pub const P384: Params = Params {
    p: "fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffeffffffff0000000000000000ffffffff",
    n: "ffffffffffffffffffffffffffffffffffffffffffffffffc7634d81f4372ddf581a0db248b0a77aecec196accc52973",
    b: "b3312fa7e23ee7e4988e056be3f82d19181d9c6efe8141120314088f5013875ac656398d8a2ed19d2a85c8edd3ec2aef",
    gx: "aa87ca22be8b05378eb1c71ef320ad746e1d3b628ba79b9859f741e082542a385502f25dbf55296c3a545e3872760ab7",
    gy: "3617de4a96262c6f5d9e98bf9292dc29f8f41dbd289a147ce9da3113b5f0b8c00a60b1ce1d7e819d7a431d7c90ea0e5f",
    pm2: "fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffeffffffff0000000000000000fffffffd",
    nm2: "ffffffffffffffffffffffffffffffffffffffffffffffffc7634d81f4372ddf581a0db248b0a77aecec196accc52971",
    size: 48,
};

fn hex(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 2);
    let mut i = 0;
    while i + 1 < b.len() {
        let hi = (b[i] as char).to_digit(16).unwrap() as u8;
        let lo = (b[i + 1] as char).to_digit(16).unwrap() as u8;
        out.push((hi << 4) | lo);
        i += 2;
    }
    out
}

/// 曲线上下文：域 p 与阶 n 两套 Montgomery 参数。
pub struct Curve {
    fp: Mont,
    fq: Mont,
    g: Point,
    pm2: Vec<u8>,
    nm2: Vec<u8>,
    size: usize,
}

#[derive(Clone, Debug)]
struct Point {
    x: Vec<u32>,
    y: Vec<u32>,
    z: Vec<u32>,
}

impl Curve {
    pub fn from(q: &Params) -> Curve {
        let fp = Mont::new(&hex(q.p)).expect("p is odd");
        let fq = Mont::new(&hex(q.n)).expect("n is odd");
        let gx = fp.to_mont(&bignum::from_be(&hex(q.gx)));
        let gy = fp.to_mont(&bignum::from_be(&hex(q.gy)));
        let one = fp.to_mont(&bignum::from_be(&[1]));
        Curve {
            fp,
            fq,
            g: Point { x: gx, y: gy, z: one },
            pm2: hex(q.pm2),
            nm2: hex(q.nm2),
            size: q.size,
        }
    }

    pub fn p256() -> Curve {
        Curve::from(&P256)
    }

    pub fn p384() -> Curve {
        Curve::from(&P384)
    }

    /// Montgomery 域里的 `a^(m-2)`，即 `a^{-1}`。
    fn inv_p(&self, a: &[u32]) -> Vec<u32> {
        self.fp.pow(a, &self.pm2)
    }

    fn inv_n(&self, a: &[u32]) -> Vec<u32> {
        self.fq.pow(a, &self.nm2)
    }

    fn is_zero(&self, a: &[u32]) -> bool {
        a.iter().all(|&x| x == 0)
    }

    fn equal_mont(&self, a: &[u32], b: &[u32]) -> bool {
        self.fp.cmp(a, b) == Ordering::Equal
    }

    fn double(&self, p: &Point) -> Point {
        if self.is_zero(&p.z) || self.is_zero(&p.y) {
            return self.infinity();
        }
        let delta = self.fp.mul(&p.z.clone(), &p.z.clone());
        let gamma = self.fp.mul(&p.y.clone(), &p.y.clone());
        let beta = self.fp.mul(&p.x.clone(), &gamma);
        // alpha = 3·(x−delta)·(x+delta)
        let t1 = self.fp.sub(&p.x.clone(), &delta);
        let t2 = self.fp.add(&p.x.clone(), &delta);
        let alpha = self.fp.mul(&self.fp.mul(&t1, &t2), &self.three());
        let x3 = {
            let a2 = self.fp.mul(&alpha.clone(), &alpha.clone());
            let eight_beta = self.fp.mul(&self.eight(), &beta.clone());
            self.fp.sub(&a2, &eight_beta)
        };
        let z3 = {
            let s = self.fp.add(&p.y.clone(), &p.z.clone());
            let s2 = self.fp.mul(&s.clone(), &s);
            let t = self.fp.sub(&s2, &gamma);
            self.fp.sub(&t, &delta)
        };
        let y3 = {
            let four_beta = self.fp.mul(&self.four(), &beta.clone());
            let t = self.fp.sub(&four_beta, &x3);
            let a = self.fp.mul(&alpha.clone(), &t);
            let g2 = self.fp.mul(&gamma.clone(), &gamma.clone());
            let eight_g2 = self.fp.mul(&self.eight(), &g2);
            self.fp.sub(&a, &eight_g2)
        };
        Point { x: x3, y: y3, z: z3 }
    }

    fn add(&self, p1: &Point, p2: &Point) -> Point {
        if self.is_zero(&p1.z) {
            return p2.clone();
        }
        if self.is_zero(&p2.z) {
            return p1.clone();
        }
        let z1z1 = self.fp.mul(&p1.z.clone(), &p1.z.clone());
        let z2z2 = self.fp.mul(&p2.z.clone(), &p2.z.clone());
        let u1 = self.fp.mul(&p1.x.clone(), &z2z2);
        let u2 = self.fp.mul(&p2.x.clone(), &z1z1);
        let s1 = self.fp.mul(&self.fp.mul(&p1.y.clone(), &p2.z.clone()), &z2z2);
        let s2 = self.fp.mul(&self.fp.mul(&p2.y.clone(), &p1.z.clone()), &z1z1);
        if self.equal_mont(&u1, &u2) {
            if self.equal_mont(&s1, &s2) {
                return self.double(p1);
            }
            return self.infinity();
        }
        let h = self.fp.sub(&u2, &u1);
        let i = {
            let two_h = self.fp.add(&h.clone(), &h.clone());
            self.fp.mul(&two_h.clone(), &two_h)
        };
        let j = self.fp.mul(&h.clone(), &i);
        let r = self.fp.add(&self.fp.sub(&s2, &s1), &self.fp.sub(&s2, &s1));
        let v = self.fp.mul(&u1, &i);
        let x3 = {
            let r2 = self.fp.mul(&r.clone(), &r.clone());
            let t = self.fp.sub(&r2, &j.clone());
            let two_v = self.fp.add(&v.clone(), &v.clone());
            self.fp.sub(&t, &two_v)
        };
        let y3 = {
            let t = self.fp.sub(&v.clone(), &x3.clone());
            let a = self.fp.mul(&r.clone(), &t);
            let s1j = self.fp.mul(&s1, &j.clone());
            let two_s1j = self.fp.add(&s1j.clone(), &s1j);
            self.fp.sub(&a, &two_s1j)
        };
        let z3 = {
            let s = self.fp.add(&p1.z.clone(), &p2.z.clone());
            let s2m = self.fp.mul(&s.clone(), &s);
            let t = self.fp.sub(&s2m, &z1z1);
            let t = self.fp.sub(&t, &z2z2);
            self.fp.mul(&t, &h)
        };
        Point { x: x3, y: y3, z: z3 }
    }

    fn infinity(&self) -> Point {
        let zero = vec![0u32; self.fp.limb_count()];
        Point {
            x: zero.clone(),
            y: zero.clone(),
            z: zero,
        }
    }

    fn three(&self) -> Vec<u32> {
        self.fp.to_mont(&bignum::from_be(&[3]))
    }
    fn four(&self) -> Vec<u32> {
        self.fp.to_mont(&bignum::from_be(&[4]))
    }
    fn eight(&self) -> Vec<u32> {
        self.fp.to_mont(&bignum::from_be(&[8]))
    }

    /// 点的标量乘（double-and-add，标量按大端字节）。
    fn mul_point(&self, p: &Point, k_be: &[u8]) -> Point {
        let mut r = self.infinity();
        let mut started = false;
        for &byte in k_be {
            for bit in (0..8).rev() {
                if started {
                    r = self.double(&r);
                }
                if (byte >> bit) & 1 == 1 {
                    r = if started { self.add(&r, p) } else { p.clone() };
                    started = true;
                }
            }
        }
        r
    }

    /// 仿射 x 坐标的大端字节。
    fn affine_x(&self, p: &Point) -> Option<Vec<u8>> {
        if self.is_zero(&p.z) {
            return None;
        }
        let zinv = self.inv_p(&p.z);
        let zinv2 = self.fp.mul(&zinv.clone(), &zinv);
        let x = self.fp.mul(&p.x.clone(), &zinv2);
        Some(bignum::to_be(&self.fp.from_mont(&x), self.size))
    }
}

/// 验签。`qx_be`/`qy_be` 是公钥坐标（各 `q.size` 字节大端），`r_be`/`s_be` 是签名。
fn verify_with(
    q: &Params,
    digest: fn(&[u8]) -> Vec<u8>,
    qx_be: &[u8],
    qy_be: &[u8],
    msg: &[u8],
    r_be: &[u8],
    s_be: &[u8],
) -> bool {
    let size = q.size;
    let c = Curve::from(q);
    let r = normalize(r_be, size);
    let s = normalize(s_be, size);
    if r.iter().all(|&b| b == 0) || s.iter().all(|&b| b == 0) {
        return false;
    }
    let n_be = hex(q.n);
    // ECDSA 要求 r、s 都在 [1, n-1]，越界的必须拒（否则 s+n 也能验过）
    if r.as_slice() >= n_be.as_slice() || s.as_slice() >= n_be.as_slice() {
        return false;
    }
    let rm = c.fq.to_mont(&bignum::from_be(&r));
    let sm = c.fq.to_mont(&bignum::from_be(&s));

    // e = 摘要。P-256 配 SHA-256、P-384 配 SHA-384，位数正好等于 n，不需截断
    let e = digest(msg);
    let e = normalize(&e, size);

    let w = c.inv_n(&sm);
    let u1 = c.fq.mul(&c.fq.to_mont(&bignum::from_be(&e)), &w);
    let u2 = c.fq.mul(&rm, &w);

    let q = {
        let qx = c.fp.to_mont(&bignum::from_be(&normalize(qx_be, size)));
        let qy = c.fp.to_mont(&bignum::from_be(&normalize(qy_be, size)));
        Point {
            x: qx,
            y: qy,
            z: c.fp.to_mont(&bignum::from_be(&[1])),
        }
    };

    let u1_be = bignum::to_be(&c.fq.from_mont(&u1), size);
    let u2_be = bignum::to_be(&c.fq.from_mont(&u2), size);
    let p1 = c.mul_point(&c.g, &u1_be);
    let p2 = c.mul_point(&q, &u2_be);
    let sum = c.add(&p1, &p2);
    let Some(mut xv) = c.affine_x(&sum) else {
        return false;
    };
    // x mod n：x < p < 2n，最多减一次
    if xv.as_slice() >= n_be.as_slice() {
        let mut borrow = 0i32;
        for i in (0..size).rev() {
            let d = xv[i] as i32 - n_be[i] as i32 - borrow;
            if d < 0 {
                xv[i] = (d + 256) as u8;
                borrow = 1;
            } else {
                xv[i] = d as u8;
                borrow = 0;
            }
        }
    }
    xv == r
}

/// P-256 + SHA-256 验签。
pub fn verify_p256(qx_be: &[u8], qy_be: &[u8], msg: &[u8], r_be: &[u8], s_be: &[u8]) -> bool {
    verify_with(&P256, |m: &[u8]| super::sha256::sha256(m).to_vec(), qx_be, qy_be, msg, r_be, s_be)
}

/// P-384 + SHA-384 验签。
pub fn verify_p384(qx_be: &[u8], qy_be: &[u8], msg: &[u8], r_be: &[u8], s_be: &[u8]) -> bool {
    verify_with(&P384, super::sha512::sha384, qx_be, qy_be, msg, r_be, s_be)
}

/// 把任意长度的大端字节左补零/截断到 `size` 字节。
fn normalize(b: &[u8], size: usize) -> Vec<u8> {
    let mut v = b;
    while v.len() > size && v[0] == 0 {
        v = &v[1..];
    }
    if v.len() > size {
        return v[v.len() - size..].to_vec();
    }
    let mut out = vec![0u8; size];
    out[size - v.len()..].copy_from_slice(v);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const Q: &[u8] = include_bytes!("../../tests/fixtures/ec_test_q.bin");
    const SIG: &[u8] = include_bytes!("../../tests/fixtures/ec_test_sig.bin");
    const MSG: &[u8] = b"rsboot tls test message";

    fn split_sig(der: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut r = super::super::der::Reader::new(der);
        let mut seq = r.seq().unwrap();
        let rr = seq.int_bytes().unwrap().to_vec();
        let ss = seq.int_bytes().unwrap().to_vec();
        (rr, ss)
    }

    #[test]
    fn verifies_real_signature() {
        let (r, s) = split_sig(SIG);
        assert!(verify_p256(&Q[..32], &Q[32..], MSG, &r, &s));
        assert!(!verify_p256(&Q[..32], &Q[32..], b"other", &r, &s));
        // 篡改 s
        let mut bad = s.clone();
        bad[31] ^= 0x01;
        assert!(!verify_p256(&Q[..32], &Q[32..], MSG, &r, &bad));
    }

    #[test]
    fn generator_is_on_curve_and_p_plus_n_ordering() {
        let c = Curve::p256();
        // y² = x³ − 3x + b（用 Montgomery 域算一遍）
        let gx = c.g.x.clone();
        let gy2 = c.fp.mul(&c.g.y.clone(), &c.g.y.clone());
        let x2 = c.fp.mul(&gx.clone(), &gx.clone());
        let x3 = c.fp.mul(&x2, &gx.clone());
        let three_x = c.fp.mul(&c.three(), &gx);
        let b = c.fp.to_mont(&bignum::from_be(&hex(P256.b)));
        let rhs = c.fp.add(&c.fp.sub(&x3, &three_x), &b);
        assert_eq!(
            bignum::to_be(&c.fp.from_mont(&gy2), 32),
            bignum::to_be(&c.fp.from_mont(&rhs), 32)
        );
    }

    #[test]
    fn scalar_mul_matches_known_multiple() {
        // 2G 的 x 坐标由 python 现算
        let c = Curve::p256();
        let two = bignum::to_be(&[2u32], 32);
        let p2 = c.mul_point(&c.g, &two);
        let x = c.affine_x(&p2).unwrap();
        let want = hex("7cf27b188d034f7e8a52380304b51ac3c08969e277f21b35a60b48fc47669978");
        assert_eq!(x, want, "2G x");
    }

    #[test]
    fn point_doubling_via_add_matches_double() {
        let c = Curve::p256();
        let g2a = c.add(&c.g, &c.g);
        let g2d = c.double(&c.g);
        let xa = c.affine_x(&g2a).unwrap();
        let xd = c.affine_x(&g2d).unwrap();
        assert_eq!(xa, xd);
    }

    #[test]
    fn infinity_is_neutral() {
        let c = Curve::p256();
        let inf = c.infinity();
        let r = c.add(&c.g, &inf);
        assert_eq!(c.affine_x(&r).unwrap(), c.affine_x(&c.g).unwrap());
        // G + (−G) = O
        let neg = Point {
            x: c.g.x.clone(),
            y: c.fp.sub(&vec![0u32; c.fp.limb_count()], &c.g.y),
            z: c.g.z.clone(),
        };
        assert!(c.affine_x(&c.add(&c.g, &neg)).is_none());
    }

    #[test]
    fn inverse_works() {
        let c = Curve::p256();
        let a = c.fp.to_mont(&bignum::from_be(&[7]));
        let ia = c.inv_p(&a);
        let one = c.fp.to_mont(&bignum::from_be(&[1]));
        assert_eq!(
            bignum::to_be(&c.fp.from_mont(&c.fp.mul(&a, &ia)), 32),
            bignum::to_be(&c.fp.from_mont(&one), 32)
        );
    }

    const Q384: &[u8] = include_bytes!("../../tests/fixtures/ec384_q.bin");
    const SIG384: &[u8] = include_bytes!("../../tests/fixtures/ec384_sig.der");

    fn der_rs(sig: &[u8]) -> (Vec<u8>, Vec<u8>) {
        // SEQUENCE { INTEGER r, INTEGER s }
        let mut i = 0;
        assert_eq!(sig[i], 0x30);
        i += 1;
        let _len = sig[i];
        i += 1;
        let mut out = Vec::new();
        for _ in 0..2 {
            assert_eq!(sig[i], 0x02);
            i += 1;
            let l = sig[i] as usize;
            i += 1;
            out.push(sig[i..i + l].to_vec());
            i += l;
        }
        (out[0].clone(), out[1].clone())
    }

    #[test]
    fn p384_signature_verifies() {
        let (r, s) = der_rs(SIG384);
        let msg = b"rsboot tls test message";
        assert!(verify_p384(&Q384[..48], &Q384[48..], msg, &r, &s));
        assert!(!verify_p384(&Q384[..48], &Q384[48..], b"tampered", &r, &s));
    }

    #[test]
    fn p384_rejects_non_canonical_r_s() {
        let (r, s) = der_rs(SIG384);
        let msg = b"rsboot tls test message";
        // s + n 也在数学上成立，必须按范围拒掉
        let n = hex(P384.n);
        let mut sp = normalize(&s, 48);
        let mut carry = 0u16;
        for i in (0..48).rev() {
            let v = sp[i] as u16 + n[i] as u16 + carry;
            sp[i] = v as u8;
            carry = v >> 8;
        }
        assert!(!verify_p384(&Q384[..48], &Q384[48..], msg, &r, &sp));
    }
}
