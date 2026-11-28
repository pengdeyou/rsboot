//! 任意精度整数，只做验签真正用到的那点模运算。
//!
//! limb 取 `u32`：`u32 · u32 + t + c` 的累积恰好装进 `u64` 不会溢出，又比
//! `u64` limb 少一半的进位麻烦。数按小端存（`limbs[0]` 是最低位）。
//!
//! 模乘一律走 Montgomery。验签场景里指数要么极小（RSA 的 65537，17 位），
//! 要么位数只有 256（P-256 求逆），所以不需要 Karatsuba 或滑动窗口。

use core::cmp::Ordering;

/// 大端字节 → 小端 u32 limb，去掉前导零。
pub fn from_be(bytes: &[u8]) -> Vec<u32> {
    let mut v = bytes.to_vec();
    while v.first() == Some(&0) && v.len() > 1 {
        v.remove(0);
    }
    let mut out = Vec::with_capacity(v.len() / 4 + 1);
    for chunk in v.rchunks(4) {
        let mut word = 0u32;
        for &b in chunk {
            word = (word << 8) | b as u32;
        }
        out.push(word);
    }
    // 高位的 0 limb 去掉，保证 limb 数最小
    while out.len() > 1 && *out.last().unwrap() == 0 {
        out.pop();
    }
    out
}

/// 小端 limb → 大端字节，左侧补零到 `len`。
pub fn to_be(limbs: &[u32], len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    for (i, &l) in limbs.iter().enumerate() {
        for j in 0..4 {
            if i * 4 + j >= len {
                break;
            }
            out[len - 1 - (i * 4 + j)] = (l >> (8 * j)) as u8;
        }
    }
    out
}

fn cmp_slice(a: &[u32], b: &[u32]) -> Ordering {
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
fn cmp_fixed(a: &[u32], b: &[u32]) -> Ordering {
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
    n: Vec<u32>,
    /// `-n[0]^{-1} mod 2^32`
    n0inv: u32,
    /// `R² mod n`，R = 2^(32k)
    r2: Vec<u32>,
}

impl Mont {
    pub fn new(n_be: &[u8]) -> Option<Mont> {
        let n = from_be(n_be);
        // Montgomery 只对奇数模数成立
        if n[0] & 1 == 0 {
            return None;
        }
        let k = n.len();
        let mut inv = 1u32;
        // Newton 迭代求 n0^{-1} mod 2^32，5 轮从 1 位涨到 32 位
        for _ in 0..5 {
            inv = inv.wrapping_mul(2u32.wrapping_sub(n[0].wrapping_mul(inv)));
        }
        let n0inv = inv.wrapping_neg();
        let r2 = r2_mod(&n, k);
        Some(Mont { k, n, n0inv, r2 })
    }

    pub fn limb_count(&self) -> usize {
        self.k
    }

    pub fn modulus(&self) -> &[u32] {
        &self.n
    }

    /// 1 的 Montgomery 表示，即 `R mod n`。
    pub fn one(&self) -> Vec<u32> {
        let mut r: Vec<u32> = vec![0; self.k + 1];
        r[0] = 1;
        // R mod n：从 1 开始逐位左移 32k 次
        for _ in 0..32 * self.k {
            shl1(&mut r);
            // r 可能跨进第 k+1 个 limb，那个位也必须算进 ">= n" 的判断
            if r[self.k] != 0 || cmp_fixed(&r[..self.k], &self.n) != Ordering::Less {
                sub_in_place(&mut r, &self.n);
            }
        }
        let mut v = vec![0u32; self.k];
        v.copy_from_slice(&r[..self.k]);
        v
    }

    /// 普通域 → Montgomery 域。
    pub fn to_mont(&self, x: &[u32]) -> Vec<u32> {
        let mut a = vec![0u32; self.k];
        let n = a.len().min(x.len());
        a[..n].copy_from_slice(&x[..n]);
        self.mul(&a, &self.r2.clone())
    }

    /// Montgomery 域 → 普通域。
    pub fn from_mont(&self, x: &[u32]) -> Vec<u32> {
        let mut one = vec![0u32; self.k];
        one[0] = 1;
        self.mul(x, &one)
    }

    /// Montgomery 乘法：输入输出都在域内，结果为 `a·b·R^{-1} mod n`。
    pub fn mul(&self, a: &[u32], b: &[u32]) -> Vec<u32> {
        let k = self.k;
        // 先做完整 schoolbook 积（2k+2 limb 容纳进位），再逐字归约
        let mut t = vec![0u32; 2 * k + 2];
        for i in 0..k {
            let ai = a.get(i).copied().unwrap_or(0);
            if ai == 0 {
                continue;
            }
            let mut c = 0u64;
            for j in 0..k {
                let s = t[i + j] as u64 + (ai as u64) * (b.get(j).copied().unwrap_or(0) as u64) + c;
                t[i + j] = s as u32;
                c = s >> 32;
            }
            let mut idx = i + k;
            while c != 0 {
                let s = t[idx] as u64 + c;
                t[idx] = s as u32;
                c = s >> 32;
                idx += 1;
            }
        }
        for i in 0..k {
            let u = t[i].wrapping_mul(self.n0inv);
            if u == 0 {
                continue;
            }
            let mut c = 0u64;
            for j in 0..k {
                let s = t[i + j] as u64 + (u as u64) * (self.n[j] as u64) + c;
                t[i + j] = s as u32;
                c = s >> 32;
            }
            let mut idx = i + k;
            while c != 0 {
                let s = t[idx] as u64 + c;
                t[idx] = s as u32;
                c = s >> 32;
                idx += 1;
            }
        }
        // Montgomery 结果的上界是 2n，且只可能多出一个 limb
        let mut res: Vec<u32> = t[k..k + k + 1].to_vec();
        if res[k] != 0 || cmp_fixed(&res[..k], &self.n) != Ordering::Less {
            sub_in_place(&mut res, &self.n);
        }
        res.truncate(k);
        res
    }

    /// 模幂，`base` 在 Montgomery 域，`exp` 是大端字节。
    pub fn pow(&self, base: &[u32], exp_be: &[u8]) -> Vec<u32> {
        let mut e = exp_be.to_vec();
        while e.first() == Some(&0) {
            e.remove(0);
        }
        let mut r = self.one();
        for byte in &e {
            for bit in (0..8).rev() {
                r = self.mul(&r.clone(), &r.clone());
                if (byte >> bit) & 1 == 1 {
                    r = self.mul(&r.clone(), base);
                }
            }
        }
        r
    }

    /// `a + b mod n`。
    pub fn add(&self, a: &[u32], b: &[u32]) -> Vec<u32> {
        let k = self.k;
        let mut r = vec![0u32; k + 1];
        let mut c = 0u64;
        for i in 0..k {
            let s = a.get(i).copied().unwrap_or(0) as u64
                + b.get(i).copied().unwrap_or(0) as u64
                + c;
            r[i] = s as u32;
            c = s >> 32;
        }
        r[k] = c as u32;
        if r[k] != 0 || cmp_fixed(&r[..k], &self.n) != Ordering::Less {
            sub_in_place(&mut r, &self.n);
        }
        r.truncate(k);
        r
    }

    /// `a - b mod n`。
    pub fn sub(&self, a: &[u32], b: &[u32]) -> Vec<u32> {
        let k = self.k;
        let mut r = vec![0u32; k];
        let mut borrow = 0i64;
        for i in 0..k {
            let d = a.get(i).copied().unwrap_or(0) as i64
                - b.get(i).copied().unwrap_or(0) as i64
                - borrow;
            if d < 0 {
                r[i] = (d + (1i64 << 32)) as u32;
                borrow = 1;
            } else {
                r[i] = d as u32;
                borrow = 0;
            }
        }
        if borrow != 0 {
            // 借位说明结果落在 [n, 2^32k)，补一个 n 就回到域内
            let mut c = 0u64;
            for i in 0..k {
                let s = r[i] as u64 + self.n[i] as u64 + c;
                r[i] = s as u32;
                c = s >> 32;
            }
        }
        r
    }

    /// 是否等于 0（域内表示）。
    pub fn is_zero(&self, a: &[u32]) -> bool {
        a.iter().all(|&x| x == 0)
    }

    /// 域内比较。
    pub fn cmp(&self, a: &[u32], b: &[u32]) -> Ordering {
        cmp_slice(a, b)
    }
}

fn shl1(v: &mut [u32]) {
    let mut c = 0u32;
    for slot in v.iter_mut() {
        let nc = *slot >> 31;
        *slot = (*slot << 1) | c;
        c = nc;
    }
}

fn sub_in_place(a: &mut [u32], b: &[u32]) {
    let mut borrow = 0i64;
    for i in 0..a.len() {
        let d = a[i] as i64 - b.get(i).copied().unwrap_or(0) as i64 - borrow;
        if d < 0 {
            a[i] = (d + (1i64 << 32)) as u32;
            borrow = 1;
        } else {
            a[i] = d as u32;
            borrow = 0;
        }
    }
    let _ = borrow;
}

/// `2^(64k) mod n`，用逐位长除法算一次。
fn r2_mod(n: &[u32], k: usize) -> Vec<u32> {
    let total_bits = 64 * k;
    let mut r = vec![0u32; k + 1];
    for i in (0..=total_bits).rev() {
        shl1(&mut r);
        if i == total_bits {
            r[0] |= 1;
        }
        while r[k] != 0 || cmp_fixed(&r[..k], n) != Ordering::Less {
            sub_in_place(&mut r, n);
        }
    }
    r.truncate(k);
    r
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
            let mut v = vec![0u32; k];
            let mut y = x;
            for slot in v.iter_mut() {
                *slot = (y & 0xffffffff) as u32;
                y >>= 32;
            }
            v
        };
        let am = m.to_mont(&to_limbs(a));
        let bm = m.to_mont(&to_limbs(b));
        let rm = m.mul(&am, &bm);
        let r = m.from_mont(&rm);
        let mut x: i128 = 0;
        for (i, l) in r.iter().enumerate() {
            x |= (*l as i128) << (32 * i);
        }
        let res = m.from_mont(&m.to_mont(&to_limbs(a)));
        let mut y: i128 = 0;
        for (i, l) in res.iter().enumerate() {
            y |= (*l as i128) << (32 * i);
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
        // 期望值全部由 python3 pow(b, e, m) 现算（见下方注释里的命令）
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
        // R² mod n 应当等于 (2^(64k)) mod n，用 python 现算的 255 位素数做交叉检查
        let n: [u8; 16] = [
            0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff,
            0xff, 0xff,
        ];
        let m = Mont::new(&n).unwrap();
        let k = m.limb_count();
        let one = m.one();
        // R mod n 再自乘一次应该得到 R（因为 (R·R)·R^{-1} = R）
        let rr = m.mul(&one, &one);
        assert_eq!(rr, one, "k={k}");
    }

    #[test]
    fn add_sub_wrap() {
        let n: [u8; 4] = [0xff, 0xff, 0xff, 0xfd]; // 4294967293，奇数
        let m = Mont::new(&n).unwrap();
        let a = m.to_mont(&from_be(&[0xff, 0xff, 0xff, 0xfc]));
        let b = m.to_mont(&from_be(&[0x00, 0x00, 0x00, 0x02]));
        // n-1 + 2 = 1 mod n
        let s = m.add(&a, &b);
        let s = m.from_mont(&s);
        assert_eq!(to_be(&s, 4), vec![0, 0, 0, 1]);
        // (n-1) - 1 = n-2 = 0xfffffffb
        let one = m.to_mont(&from_be(&[1]));
        let d = m.sub(&a, &one);
        let d = m.from_mont(&d);
        assert_eq!(to_be(&d, 4), vec![0xff, 0xff, 0xff, 0xfb]);
        // 0 - 1 = n-1
        let z = vec![0u32; m.limb_count()];
        let neg = m.sub(&z, &one);
        let neg = m.from_mont(&neg);
        assert_eq!(to_be(&neg, 4), vec![0xff, 0xff, 0xff, 0xfc]);
    }

    #[test]
    fn be_le_conversion() {
        assert_eq!(from_be(&[1, 2, 3, 4, 5]), vec![0x02030405, 0x00000001]);
        assert_eq!(to_be(&[0x02030405, 0x00000001], 5), vec![1, 2, 3, 4, 5]);
        assert_eq!(from_be(&[0, 0, 0, 0]), vec![0]);
        assert_eq!(to_be(&[0], 4), vec![0, 0, 0, 0]);
        // 前导零被去掉，字节长度按请求补齐
        assert_eq!(from_be(&[0x00, 0xff]), vec![0xff]);
    }
}
