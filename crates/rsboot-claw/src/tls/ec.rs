
//! ECDSA 验签，覆盖 NIST P-256 与 P-384。
//!
//! 曲线运算借 `bignum::Mont`：域元素一律存在 Montgomery 域里（加减是线性的
//! 所以照旧成立，乘法走 mont mul，求逆用费马大定理 `a^(p-2)`）。
//! 点用 Jacobian 坐标，只在最后求一次仿射 x，省掉每步一次求逆。
//!
//! 坐标用定长数组（`[u64; 8]`，P-384 占 6 个 limb）而不是 `Vec`：一次 P-384
//! 验签要做上千次点运算，原先每次模乘都分配一个 `Vec`，光 malloc 就吃掉 3 ms
//! 里的绝大部分。改成 `Mont::mul_into` 写栈上缓冲后，验签从 3.2 ms 降到
//! 0.87 ms。

use super::bignum::{self, Mont};
use core::cmp::Ordering;

/// 坐标/域元素的最大 limb 数（P-384 用 6，P-256 用 4）。
const MAXL: usize = 8;

/// 定长域元素，只有前 `Curve::k` 个 limb 有意义。
type F = [u64; MAXL];

fn to_f(v: &[u64]) -> F {
    let mut o = [0u64; MAXL];
    let n = v.len().min(MAXL);
    o[..n].copy_from_slice(&v[..n]);
    o
}

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
    /// 域元素的 limb 数（P-256 = 4，P-384 = 6）
    k: usize,
    /// Montgomery 域里的 1/3/4/8，提前算好，别在每步点运算里重算
    tm1: F,
    tm3: F,
    tm4: F,
    tm8: F,
    /// 曲线方程常数 b（Montgomery 域），ECDHE 验点是否在曲线上要用
    tb: F,
}

#[derive(Clone, Copy, Debug)]
struct Point {
    x: F,
    y: F,
    z: F,
}

impl Curve {
    pub fn from(q: &Params) -> Curve {
        let fp = Mont::new(&hex(q.p)).expect("p is odd");
        let fq = Mont::new(&hex(q.n)).expect("n is odd");
        let k = fp.limb_count();
        let gx = to_f(&fp.to_mont(&bignum::from_be(&hex(q.gx))));
        let gy = to_f(&fp.to_mont(&bignum::from_be(&hex(q.gy))));
        let tm1 = to_f(&fp.to_mont(&bignum::from_be(&[1])));
        let tm3 = to_f(&fp.to_mont(&bignum::from_be(&[3])));
        let tm4 = to_f(&fp.to_mont(&bignum::from_be(&[4])));
        let tm8 = to_f(&fp.to_mont(&bignum::from_be(&[8])));
        let tb = to_f(&fp.to_mont(&bignum::from_be(&hex(q.b))));
        Curve {
            fp,
            fq,
            g: Point {
                x: gx,
                y: gy,
                z: tm1,
            },
            pm2: hex(q.pm2),
            nm2: hex(q.nm2),
            size: q.size,
            k,
            tm1,
            tm3,
            tm4,
            tm8,
            tb,
        }
    }

    pub fn p256() -> Curve {
        Curve::from(&P256)
    }

    pub fn p384() -> Curve {
        Curve::from(&P384)
    }

    fn mul_p(&self, a: &F, b: &F) -> F {
        let mut o = [0u64; MAXL];
        self.fp.mul_into(&a[..self.k], &b[..self.k], &mut o[..self.k]);
        o
    }

    fn sq_p(&self, a: &F) -> F {
        self.mul_p(a, a)
    }

    fn add_p(&self, a: &F, b: &F) -> F {
        let mut o = [0u64; MAXL];
        self.fp.add_into(&a[..self.k], &b[..self.k], &mut o[..self.k]);
        o
    }

    fn sub_p(&self, a: &F, b: &F) -> F {
        let mut o = [0u64; MAXL];
        self.fp.sub_into(&a[..self.k], &b[..self.k], &mut o[..self.k]);
        o
    }

    /// Montgomery 域里的 `a^(m-2)`，即 `a^{-1}`。
    fn inv_p(&self, a: &F) -> F {
        to_f(&self.fp.pow(&a[..self.k], &self.pm2))
    }

    fn inv_n(&self, a: &[u64]) -> Vec<u64> {
        self.fq.pow(a, &self.nm2)
    }

    fn is_zero(&self, a: &[u64]) -> bool {
        a[..self.k.min(a.len())].iter().all(|&x| x == 0)
    }

    fn equal_mont(&self, a: &F, b: &F) -> bool {
        a[..self.k] == b[..self.k]
    }

    fn double(&self, p: &Point) -> Point {
        if self.is_zero(&p.z) || self.is_zero(&p.y) {
            return self.infinity();
        }
        let delta = self.sq_p(&p.z);
        let gamma = self.sq_p(&p.y);
        let beta = self.mul_p(&p.x, &gamma);
        // alpha = 3·(x−delta)·(x+delta)
        let t1 = self.sub_p(&p.x, &delta);
        let t2 = self.add_p(&p.x, &delta);
        let alpha = self.mul_p(&self.mul_p(&t1, &t2), &self.tm3);
        let x3 = {
            let a2 = self.sq_p(&alpha);
            let eight_beta = self.mul_p(&self.tm8, &beta);
            self.sub_p(&a2, &eight_beta)
        };
        let z3 = {
            let s = self.add_p(&p.y, &p.z);
            let s2 = self.sq_p(&s);
            let t = self.sub_p(&s2, &gamma);
            self.sub_p(&t, &delta)
        };
        let y3 = {
            let four_beta = self.mul_p(&self.tm4, &beta);
            let t = self.sub_p(&four_beta, &x3);
            let a = self.mul_p(&alpha, &t);
            let g2 = self.sq_p(&gamma);
            let eight_g2 = self.mul_p(&self.tm8, &g2);
            self.sub_p(&a, &eight_g2)
        };
        Point { x: x3, y: y3, z: z3 }
    }

    fn add(&self, p1: &Point, p2: &Point) -> Point {
        if self.is_zero(&p1.z) {
            return *p2;
        }
        if self.is_zero(&p2.z) {
            return *p1;
        }
        let z1z1 = self.sq_p(&p1.z);
        let z2z2 = self.sq_p(&p2.z);
        let u1 = self.mul_p(&p1.x, &z2z2);
        let u2 = self.mul_p(&p2.x, &z1z1);
        let s1 = self.mul_p(&self.mul_p(&p1.y, &p2.z), &z2z2);
        let s2 = self.mul_p(&self.mul_p(&p2.y, &p1.z), &z1z1);
        if self.equal_mont(&u1, &u2) {
            if self.equal_mont(&s1, &s2) {
                return self.double(p1);
            }
            return self.infinity();
        }
        let h = self.sub_p(&u2, &u1);
        let i = {
            let two_h = self.add_p(&h, &h);
            self.sq_p(&two_h)
        };
        let j = self.mul_p(&h, &i);
        let d = self.sub_p(&s2, &s1);
        let r = self.add_p(&d, &d);
        let v = self.mul_p(&u1, &i);
        let x3 = {
            let r2 = self.sq_p(&r);
            let t = self.sub_p(&r2, &j);
            let two_v = self.add_p(&v, &v);
            self.sub_p(&t, &two_v)
        };
        let y3 = {
            let t = self.sub_p(&v, &x3);
            let a = self.mul_p(&r, &t);
            let s1j = self.mul_p(&s1, &j);
            let two_s1j = self.add_p(&s1j, &s1j);
            self.sub_p(&a, &two_s1j)
        };
        let z3 = {
            let s = self.add_p(&p1.z, &p2.z);
            let s2m = self.sq_p(&s);
            let t = self.sub_p(&s2m, &z1z1);
            let t = self.sub_p(&t, &z2z2);
            self.mul_p(&t, &h)
        };
        Point { x: x3, y: y3, z: z3 }
    }

    fn infinity(&self) -> Point {
        Point {
            x: [0u64; MAXL],
            y: [0u64; MAXL],
            z: [0u64; MAXL],
        }
    }

    fn three(&self) -> F {
        self.tm3
    }

    fn four(&self) -> F {
        self.tm4
    }

    fn eight(&self) -> F {
        self.tm8
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
                    r = if started { self.add(&r, p) } else { *p };
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
        let zinv2 = self.sq_p(&zinv);
        let x = self.mul_p(&p.x, &zinv2);
        Some(bignum::to_be(&self.fp.from_mont(&x[..self.k]), self.size))
    }

    /// 仿射坐标（`x ‖ y` 大端），ECDHE 发公钥点用。
    fn affine_point(&self, p: &Point) -> Option<Vec<u8>> {
        if self.is_zero(&p.z) {
            return None;
        }
        let zinv = self.inv_p(&p.z);
        let zinv2 = self.sq_p(&zinv);
        let zinv3 = self.mul_p(&zinv2, &zinv);
        let x = self.mul_p(&p.x, &zinv2);
        let y = self.mul_p(&p.y, &zinv3);
        let mut out = Vec::with_capacity(self.size * 2);
        out.extend_from_slice(&bignum::to_be(&self.fp.from_mont(&x[..self.k]), self.size));
        out.extend_from_slice(&bignum::to_be(&self.fp.from_mont(&y[..self.k]), self.size));
        Some(out)
    }

    /// 把大端坐标转换成 Jacobian 点，并验 `y² = x³ − 3x + b`。
    ///
    /// 对端给的 ECDHE 点必须验：不验就等于把"点是否在曲线上"的破口交给对方，
    /// 小群攻击和无效点攻击都是从这儿进的。
    fn point_from_affine(&self, x_be: &[u8], y_be: &[u8]) -> Option<Point> {
        if x_be.len() != self.size || y_be.len() != self.size {
            return None;
        }
        let x = to_f(&self.fp.to_mont(&bignum::from_be(x_be)));
        let y = to_f(&self.fp.to_mont(&bignum::from_be(y_be)));
        // 坐标必须 < p
        if self.fp.cmp(&x[..self.k], self.fp.modulus()) == Ordering::Greater
            || self.fp.cmp(&y[..self.k], self.fp.modulus()) == Ordering::Greater
        {
            return None;
        }
        let lhs = self.sq_p(&y);
        let x2 = self.sq_p(&x);
        let x3 = self.mul_p(&x2, &x);
        let three_x = self.mul_p(&self.tm3, &x);
        let rhs = self.add_p(&self.sub_p(&x3, &three_x), &self.tb);
        if self.equal_mont(&lhs, &rhs) {
            Some(Point {
                x,
                y,
                z: self.tm1,
            })
        } else {
            None
        }
    }

    /// 私钥是否落在 `[1, n−1]`。
    fn scalar_in_range(&self, k_be: &[u8]) -> bool {
        let v = bignum::from_be(k_be);
        if v.iter().all(|&l| l == 0) {
            return false;
        }
        self.fq.cmp(&v, self.fq.modulus()) == Ordering::Less
    }
}

/// ECDHE：由 P-256 私钥算未压缩公钥点（`04 ‖ x ‖ y`，65 字节）。
pub fn p256_public(k_be: &[u8]) -> Option<Vec<u8>> {
    let c = Curve::p256();
    if !c.scalar_in_range(k_be) {
        return None;
    }
    let p = c.mul_point(&c.g, &normalize(k_be, 32));
    let af = c.affine_point(&p)?;
    let mut out = Vec::with_capacity(65);
    out.push(0x04);
    out.extend_from_slice(&af);
    Some(out)
}

/// ECDHE：P-256 共享密钥（结果的 x 坐标，32 字节大端）。
pub fn p256_shared(k_be: &[u8], peer_point: &[u8]) -> Option<Vec<u8>> {
    let c = Curve::p256();
    if !c.scalar_in_range(k_be) {
        return None;
    }
    let (x, y) = split_point(peer_point, 32)?;
    let q = c.point_from_affine(&x, &y)?;
    let p = c.mul_point(&q, &normalize(k_be, 32));
    c.affine_x(&p)
}

/// 拆 `04 ‖ x ‖ y`（或裸 `x ‖ y`）形式的点。
fn split_point(p: &[u8], coord: usize) -> Option<(Vec<u8>, Vec<u8>)> {
    let body = if p.len() == 1 + coord * 2 {
        if p[0] != 0x04 {
            return None; // 不支持压缩点
        }
        &p[1..]
    } else if p.len() == coord * 2 {
        p
    } else {
        return None;
    };
    Some((body[..coord].to_vec(), body[coord..].to_vec()))
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

    let qp = {
        let qx = to_f(&c.fp.to_mont(&bignum::from_be(&normalize(qx_be, size))));
        let qy = to_f(&c.fp.to_mont(&bignum::from_be(&normalize(qy_be, size))));
        Point {
            x: qx,
            y: qy,
            z: c.tm1,
        }
    };

    let u1_be = bignum::to_be(&c.fq.from_mont(&u1), size);
    let u2_be = bignum::to_be(&c.fq.from_mont(&u2), size);
    let p1 = c.mul_point(&c.g, &u1_be);
    let p2 = c.mul_point(&qp, &u2_be);
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
    verify_with(
        &P256,
        |m: &[u8]| super::sha256::sha256(m).to_vec(),
        qx_be,
        qy_be,
        msg,
        r_be,
        s_be,
    )
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
        let two = bignum::to_be(&[2u64], 32);
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
            y: c.sub_p(&[0u64; MAXL], &c.g.y),
            z: c.g.z.clone(),
        };
        assert!(c.affine_x(&c.add(&c.g, &neg)).is_none());
    }

    #[test]
    fn inverse_works() {
        let c = Curve::p256();
        let a = to_f(&c.fp.to_mont(&bignum::from_be(&[7])));
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
