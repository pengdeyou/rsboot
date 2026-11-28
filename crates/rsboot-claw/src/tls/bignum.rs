//! 任意精度整数，只做验签真正用到的那点模运算。
//!
//! limb 取 `u64`：产品用 `u128` 累积，一次 CIOS 乘法的内层就是 u128 的乘加。
//! 早先试过 `u32` limb（2048 位模数要 64 个 limb），实测慢 4 倍 —— RSA 验签
//! 340 µs vs 现在 ~90 µs。数按小端存（`limbs[0]` 是最低位）。
//!
//! 模乘一律走 Montgomery。验签场景里指数要么极小（RSA 的 65537，17 位），
//! 要么位数只有 384（P-384 求逆），所以不需要 Karatsuba 或滑动窗口。

use core::cmp::Ordering;

/// `mul_into`/`add_into`/`sub_into` 走栈上中间量的最大 limb 数。
/// P-384 是 6，P-256 是 4，留一点余量。
const STACK_K: usize = 8;


/// 大端字节 → 小端 u64 limb，去掉前导零。
pub fn from_be(bytes: &[u8]) -> Vec<u64> {
    let mut v = bytes.to_vec();
    while v.first() == Some(&0) && v.len() > 1 {
        v.remove(0);
    }
    let mut out = Vec::with_capacity(v.len() / 8 + 1);
    for chunk in v.rchunks(8) {
        let mut word = 0u64;
        for &b in chunk {
            word = (word << 8) | b as u64;
        }
        out.push(word);
    }
    while out.len() > 1 && *out.last().unwrap() == 0 {
        out.pop();
    }
    out
}

/// 小端 limb → 大端字节，左侧补零到 `len`。
pub fn to_be(limbs: &[u64], len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    for (i, &l) in limbs.iter().enumerate() {
        for j in 0..8 {
            if i * 8 + j >= len {
                break;
            }
            out[len - 1 - (i * 8 + j)] = (l >> (8 * j)) as u8;
        }
    }
    out
}

fn cmp_slice(a: &[u64], b: &[u64]) -> Ordering {
    if a.len() != b.len() {
        return a.len().cmp(&b.len());
    }
    for i in (0..a.len()).rev() {
        if a[i] != b[i] {
            return a[i].cmp(&b[i]);
        }
    }
    Ordering::Equal
}

/// 定长比较（长度相同，用于 Montgomery 内部）。
fn cmp_fixed(a: &[u64], b: &[u64]) -> Ordering {
    for i in (0..a.len().min(b.len())).rev() {
        if a[i] != b[i] {
            return a[i].cmp(&b[i]);
        }
    }
    Ordering::Equal
}

/// 固定的模数上下文：`n` 及其 Montgomery 参数。
#[derive(Clone)]
pub struct Mont {
    k: usize,
    n: Vec<u64>,
    /// `-n[0]^{-1} mod 2^64`
    n0inv: u64,
    /// `R² mod n`，R = 2^(64k)
    r2: Vec<u64>,
}

impl Mont {
    pub fn new(n_be: &[u8]) -> Option<Mont> {
        let n = from_be(n_be);
        if n[0] & 1 == 0 {
            return None;
        }
        let k = n.len();
        let mut inv = 1u64;
        // Newton 迭代求 n0^{-1} mod 2^64：每轮有效位数翻倍，6 轮够 64 位
        for _ in 0..6 {
            inv = inv.wrapping_mul(2u64.wrapping_sub(n[0].wrapping_mul(inv)));
        }
        let n0inv = inv.wrapping_neg();
        let r2 = r2_mod(&n, k);
        Some(Mont { k, n, n0inv, r2 })
    }

    pub fn limb_count(&self) -> usize {
        self.k
    }

    pub fn modulus(&self) -> &[u64] {
        &self.n
    }

    /// 1 的 Montgomery 表示，即 `R mod n`。
    pub fn one(&self) -> Vec<u64> {
        let mut r: Vec<u64> = vec![0; self.k + 1];
        r[0] = 1;
        for _ in 0..64 * self.k {
            shl1(&mut r);
            if r[self.k] != 0 || cmp_fixed(&r[..self.k], &self.n) != Ordering::Less {
                sub_in_place(&mut r, &self.n);
            }
        }
        r[..self.k].to_vec()
    }

    /// 普通域 → Montgomery 域。
    pub fn to_mont(&self, x: &[u64]) -> Vec<u64> {
        let mut a = vec![0u64; self.k];
        let n = a.len().min(x.len());
        a[..n].copy_from_slice(&x[..n]);
        let r2 = self.r2.clone();
        self.mul(&a, &r2)
    }

    /// Montgomery 域 → 普通域。
    pub fn from_mont(&self, x: &[u64]) -> Vec<u64> {
        let mut one = vec![0u64; self.k];
        one[0] = 1;
        self.mul(x, &one)
    }

    /// Montgomery 乘法：输入输出都在域内，结果为 `a·b·R^{-1} mod n`。
    ///
    /// CIOS（Coarsely Integrated Operand Scanning）：乘与归约交织在同一轮 limb
    /// 扫描里，中间结果用 u128 装。比「先算完整 schoolbook 积、再逐 limb 归约」
    /// 快近 2 倍 —— 后者要把 k² 扫两遍。
    pub fn mul(&self, a: &[u64], b: &[u64]) -> Vec<u64> {
        let mut out = vec![0u64; self.k];
        self.mul_into(a, b, &mut out);
        out
    }

    /// 同 `mul`，但结果写进调用方的缓冲（长度 ≥ `limb_count()`），不分配堆。
    ///
    /// 椭圆曲线一次验签要做上千次模乘，每次返回 `Vec` 就是一次 malloc ——
    /// P-384 上光这一项就占掉 3 ms 里的大半。`k ≤ 8`（P-256/P-384 分别是
    /// 4 和 6）时中间量全在栈上；RSA 那种 32 limb 的模数仍走原来的路径。
    pub fn mul_into(&self, a: &[u64], b: &[u64], out: &mut [u64]) {
        let k = self.k;
        if k <= STACK_K {
            let mut t = [0u64; STACK_K + 2];
            let mut res = [0u64; STACK_K + 1];
            self.cios(a, b, &mut t[..k + 2]);
            res[..=k].copy_from_slice(&t[..=k]);
            if res[k] != 0 || cmp_fixed(&res[..k], &self.n) != Ordering::Less {
                sub_in_place(&mut res[..=k], &self.n);
            }
            out[..k].copy_from_slice(&res[..k]);
            return;
        }
        let mut t = vec![0u64; k + 2];
        self.cios(a, b, &mut t);
        let mut res = t[..=k].to_vec();
        if res[k] != 0 || cmp_fixed(&res[..k], &self.n) != Ordering::Less {
            sub_in_place(&mut res, &self.n);
        }
        out[..k].copy_from_slice(&res[..k]);
    }

    /// CIOS 主体：`t` 长度必须是 `k + 2` 且已清零。
    fn cios(&self, a: &[u64], b: &[u64], t: &mut [u64]) {
        let k = self.k;
        let t = &mut t[..k + 2];
        for i in 0..k {
            let ai = a.get(i).copied().unwrap_or(0);
            if ai != 0 {
                let mut c = 0u128;
                for j in 0..k {
                    let s =
                        t[j] as u128 + (ai as u128) * (b.get(j).copied().unwrap_or(0) as u128) + c;
                    t[j] = s as u64;
                    c = s >> 64;
                }
                let s = t[k] as u128 + c;
                t[k] = s as u64;
                t[k + 1] = (s >> 64) as u64;
            }

            let m = t[0].wrapping_mul(self.n0inv);
            let mut c = 0u128;
            for j in 0..k {
                let s = t[j] as u128 + (m as u128) * (self.n[j] as u128) + c;
                if j > 0 {
                    t[j - 1] = s as u64;
                }
                c = s >> 64;
            }
            let s = t[k] as u128 + c;
            t[k - 1] = s as u64;
            t[k] = t[k + 1] + (s >> 64) as u64;
        }
    }

    /// `a + b mod n`，结果写入 `out`。
    pub fn add_into(&self, a: &[u64], b: &[u64], out: &mut [u64]) {
        let k = self.k;
        let mut r = [0u64; STACK_K + 1];
        let mut c = 0u128;
        for i in 0..k {
            let s = a.get(i).copied().unwrap_or(0) as u128
                + b.get(i).copied().unwrap_or(0) as u128
                + c;
            r[i] = s as u64;
            c = s >> 64;
        }
        r[k] = c as u64;
        if r[k] != 0 || cmp_fixed(&r[..k], &self.n) != Ordering::Less {
            sub_in_place(&mut r[..=k], &self.n);
        }
        out[..k].copy_from_slice(&r[..k]);
    }

    /// `a - b mod n`，结果写入 `out`。
    pub fn sub_into(&self, a: &[u64], b: &[u64], out: &mut [u64]) {
        let k = self.k;
        let mut r = [0u64; STACK_K];
        let mut borrow = 0i128;
        for i in 0..k {
            let d = a.get(i).copied().unwrap_or(0) as i128
                - b.get(i).copied().unwrap_or(0) as i128
                - borrow;
            if d < 0 {
                r[i] = (d + (1i128 << 64)) as u64;
                borrow = 1;
            } else {
                r[i] = d as u64;
                borrow = 0;
            }
        }
        if borrow != 0 {
            let mut c = 0u128;
            for i in 0..k {
                let s = r[i] as u128 + self.n[i] as u128 + c;
                r[i] = s as u64;
                c = s >> 64;
            }
        }
        out[..k].copy_from_slice(&r[..k]);
    }

    /// 模幂，`base` 在 Montgomery 域，`exp` 是大端字节。
    pub fn pow(&self, base: &[u64], exp_be: &[u8]) -> Vec<u64> {
        let mut e = exp_be.to_vec();
        while e.first() == Some(&0) {
            e.remove(0);
        }
        let mut r = self.one();
        for byte in &e {
            for bit in (0..8).rev() {
                r = self.mul(&r, &r);
                if (byte >> bit) & 1 == 1 {
                    r = self.mul(&r, base);
                }
            }
        }
        r
    }

    /// `a + b mod n`。
    pub fn add(&self, a: &[u64], b: &[u64]) -> Vec<u64> {
        let k = self.k;
        let mut r = vec![0u64; k + 1];
        let mut c = 0u128;
        for i in 0..k {
            let s = a.get(i).copied().unwrap_or(0) as u128
                + b.get(i).copied().unwrap_or(0) as u128
                + c;
            r[i] = s as u64;
            c = s >> 64;
        }
        r[k] = c as u64;
        if r[k] != 0 || cmp_fixed(&r[..k], &self.n) != Ordering::Less {
            sub_in_place(&mut r, &self.n);
        }
        r.truncate(k);
        r
    }

    /// `a - b mod n`。
    pub fn sub(&self, a: &[u64], b: &[u64]) -> Vec<u64> {
        let k = self.k;
        let mut r = vec![0u64; k];
        let mut borrow = 0i128;
        for i in 0..k {
            let d = a.get(i).copied().unwrap_or(0) as i128
                - b.get(i).copied().unwrap_or(0) as i128
                - borrow;
            if d < 0 {
                r[i] = (d + (1i128 << 64)) as u64;
                borrow = 1;
            } else {
                r[i] = d as u64;
                borrow = 0;
            }
        }
        if borrow != 0 {
            // 借位说明结果落在 [n, 2^64k)，补一个 n 就回到域内
            let mut c = 0u128;
            for i in 0..k {
                let s = r[i] as u128 + self.n[i] as u128 + c;
                r[i] = s as u64;
                c = s >> 64;
            }
        }
        r
    }

    /// 是否等于 0（域内表示）。
    pub fn is_zero(&self, a: &[u64]) -> bool {
        a.iter().all(|&x| x == 0)
    }

    /// 域内比较。
    pub fn cmp(&self, a: &[u64], b: &[u64]) -> Ordering {
        cmp_slice(a, b)
    }
}

fn shl1(v: &mut [u64]) {
    let mut c = 0u64;
    for slot in v.iter_mut() {
        let nc = *slot >> 63;
        *slot = (*slot << 1) | c;
        c = nc;
    }
}

fn sub_in_place(a: &mut [u64], b: &[u64]) {
    let mut borrow = 0i128;
    for i in 0..a.len() {
        let d = a[i] as i128 - b.get(i).copied().unwrap_or(0) as i128 - borrow;
        if d < 0 {
            a[i] = (d + (1i128 << 64)) as u64;
            borrow = 1;
        } else {
            a[i] = d as u64;
            borrow = 0;
        }
    }
    let _ = borrow;
}

/// `2^(128k) mod n`（即 R² mod n），用逐位长除法算一次。
fn r2_mod(n: &[u64], k: usize) -> Vec<u64> {
    let total_bits = 128 * k;
    let mut r = vec![0u64; k + 1];
    for i in (0..=total_bits).rev() {
        shl1(&mut r);
        if i == total_bits {
            r[0] |= 1;
        }
        while r[k] != 0 || cmp_fixed(&r[..k], n) != Ordering::Less {
            sub_in_place(&mut r, n);
        }
    }
    r[..k].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用 i128 做小规模对拍，能把进位/借位的 bug 全暴露出来。
    fn small_mont(n: i128, a: i128, b: i128) -> (i128, i128) {
        let n_bytes = {
            let mut v = Vec::new();
            let mut x = n;
            while x > 0 {
                v.push((x & 0xff) as u8);
                x >>= 8;
            }
            v.reverse();
            v
        };
        let m = Mont::new(&n_bytes).unwrap();
        let k = m.limb_count();
        let to_limbs = |x: i128| {
            let mut v = vec![0u64; k];
            let mut y = x;
            for slot in v.iter_mut() {
                *slot = (y & 0xffff_ffff_ffff_ffff) as u64;
                y >>= 64;
            }
            v
        };
        let am = m.to_mont(&to_limbs(a));
        let bm = m.to_mont(&to_limbs(b));
        let rm = m.mul(&am, &bm);
        let r = m.from_mont(&rm);
        let mut x: i128 = 0;
        for (i, l) in r.iter().enumerate() {
            x |= (*l as i128) << (64 * i);
        }
        let res = m.from_mont(&m.to_mont(&to_limbs(a)));
        let mut y: i128 = 0;
        for (i, l) in res.iter().enumerate() {
            y |= (*l as i128) << (64 * i);
        }
        (x, y)
    }

    #[test]
    fn roundtrip_and_multiply_small() {
        for n in [3i128, 17, 65537, 1_000_003, 4_294_967_311] {
            for a in [1i128, 2, n - 1, n / 2] {
                for b in [1i128, 3, n - 2] {
                    let (prod, rt) = small_mont(n, a, b);
                    assert_eq!(rt, a % n, "往返 n={n} a={a}");
                    assert_eq!(prod, (a * b) % n, "乘 n={n} a={a} b={b}");
                }
            }
        }
    }

    #[test]
    fn modular_pow_matches_python() {
        // 期望值全部由 python3 pow(b, e, m) 现算
        let cases: &[(&[u8], &[u8], &[u8], &[u8])] = &[
            (&[0x01, 0x00, 0x01], &[0x02], &[0x11], &[0x02]),
            (&[0x01, 0x00, 0x01], &[0x02], &[0x61], &[0x19]),
            (&[0x03], &[0x05], &[0x0b], &[0x04]),
            (&[0xff], &[0x07], &[0x0d], &[0x05]),
        ];
        for &(e, b, m, want) in cases {
            let mont = Mont::new(m).unwrap();
            let base = mont.to_mont(&from_be(b));
            let r = mont.pow(&base, e);
            assert_eq!(to_be(&mont.from_mont(&r), want.len()), want, "m={m:?}");
        }
    }

    #[test]
    fn r2_is_correct() {
        let n: [u8; 16] = [
            0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff,
            0xff, 0xff,
        ];
        let m = Mont::new(&n).unwrap();
        let k = m.limb_count();
        let one = m.one();
        // (R·R)·R^{-1} = R
        let rr = m.mul(&one, &one);
        assert_eq!(rr, one, "k={k}");
    }

    #[test]
    fn add_sub_wrap() {
        let n: [u8; 4] = [0xff, 0xff, 0xff, 0xfd]; // 4294967293，奇数
        let m = Mont::new(&n).unwrap();
        let a = m.to_mont(&from_be(&[0xff, 0xff, 0xff, 0xfc]));
        let b = m.to_mont(&from_be(&[0x00, 0x00, 0x00, 0x02]));
        let s = m.add(&a, &b);
        let s = m.from_mont(&s);
        assert_eq!(to_be(&s, 4), vec![0, 0, 0, 1]);
        let one = m.to_mont(&from_be(&[1]));
        let d = m.sub(&a, &one);
        let d = m.from_mont(&d);
        assert_eq!(to_be(&d, 4), vec![0xff, 0xff, 0xff, 0xfb]);
        let z = vec![0u64; m.limb_count()];
        let neg = m.sub(&z, &one);
        let neg = m.from_mont(&neg);
        assert_eq!(to_be(&neg, 4), vec![0xff, 0xff, 0xff, 0xfc]);
    }

    #[test]
    fn be_le_conversion() {
        assert_eq!(from_be(&[1, 2, 3, 4, 5]), vec![0x0000_0001_0203_0405]);
        assert_eq!(to_be(&[0x0000_0001_0203_0405], 5), vec![1, 2, 3, 4, 5]);
        assert_eq!(from_be(&[0, 0, 0, 0]), vec![0]);
        assert_eq!(to_be(&[0], 4), vec![0, 0, 0, 0]);
        assert_eq!(from_be(&[0x00, 0xff]), vec![0xff]);
        // 9 字节：跨两个 limb，最高字节单独成一个 limb
        assert_eq!(
            from_be(&[0x01, 2, 3, 4, 5, 6, 7, 8, 9]),
            vec![0x0203_0405_0607_0809, 0x01]
        );
    }
}
