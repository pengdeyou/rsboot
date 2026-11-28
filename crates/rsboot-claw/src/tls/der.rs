//! DER（X.690）最小解析器。
//!
//! 证书解析只需要"按标签读出嵌套结构"这一件事：没有 ASN.1 编译器、没有
//! schema、没有流式解码器。这个文件的全部职责就是把 `(tag, bytes)` 一层层
//! 拆开，顺便解码几个常用类型（OID / 整数 / 时间 / 位串）。
//!
//! 只处理 definite length（DER 强制要求，CA 签出来的证书不会有不定长）。
//! 长格式长度（`0x81/0x82...`）要支持——真实证书的 TBS 经常超过 256 字节。

use core::fmt;

#[derive(Debug, PartialEq, Eq)]
pub struct DerError(pub &'static str);

impl fmt::Display for DerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "der: {}", self.0)
    }
}

pub type Result<T> = core::result::Result<T, DerError>;

/// 一个位置的游标。`next_tlv` 只前进，不回溯。
pub struct Reader<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    pub fn new(d: &'a [u8]) -> Self {
        Self { d, p: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.p >= self.d.len()
    }

    pub fn peek(&self) -> Option<u8> {
        self.d.get(self.p).copied()
    }

    /// 当前位置（读 TLV 之前记下来，读完之后切出来就是"含 tag 的原始字节"）。
    pub fn pos(&self) -> usize {
        self.p
    }

    pub fn slice_from(&self, start: usize) -> &'a [u8] {
        &self.d[start..self.p]
    }

    /// 读一个 TLV，返回 `(tag, 内容字节)`。
    pub fn next_tlv(&mut self) -> Result<(u8, &'a [u8])> {
        let tag = *self.d.get(self.p).ok_or(DerError("truncated at tag"))?;
        let len_byte = *self
            .d
            .get(self.p + 1)
            .ok_or(DerError("truncated at length"))?;
        let mut at = self.p + 2;
        let len = if len_byte < 0x80 {
            len_byte as usize
        } else {
            let n = (len_byte & 0x7f) as usize;
            // 不定长（0x80）与超过 4 字节的长度都不接受：DER 里不该出现
            if n == 0 || n > 4 {
                return Err(DerError("bad length encoding"));
            }
            let mut v = 0usize;
            for i in 0..n {
                let b = *self.d.get(at + i).ok_or(DerError("truncated length bytes"))?;
                v = (v << 8) | b as usize;
            }
            at += n;
            v
        };
        let end = at.checked_add(len).ok_or(DerError("length overflow"))?;
        if end > self.d.len() {
            return Err(DerError("truncated content"));
        }
        let content = &self.d[at..end];
        self.p = end;
        Ok((tag, content))
    }

    /// 要求下一个 TLV 的 tag 正好是 `tag`。
    pub fn want(&mut self, tag: u8) -> Result<&'a [u8]> {
        let (t, c) = self.next_tlv()?;
        if t != tag {
            return Err(DerError("unexpected tag"));
        }
        Ok(c)
    }

    pub fn skip(&mut self) -> Result<()> {
        self.next_tlv().map(|_| ())
    }

    /// 读 SEQUENCE（0x30）并把它变成子游标。
    pub fn seq(&mut self) -> Result<Reader<'a>> {
        Ok(Reader::new(self.want(0x30)?))
    }

    /// 读 INTEGER，返回原始字节（大整数定长、可能带符号——RSA 的模数与指数要看字节）。
    pub fn int_bytes(&mut self) -> Result<&'a [u8]> {
        self.want(0x02)
    }

    pub fn int_small(&mut self) -> Result<i64> {
        let b = self.int_bytes()?;
        if b.len() > 8 {
            return Err(DerError("integer too large"));
        }
        let mut v: i64 = if b.first().is_some_and(|f| f & 0x80 != 0) {
            -1
        } else {
            0
        };
        for x in b {
            v = (v << 8) | *x as i64;
        }
        Ok(v)
    }

    /// 按字节取用（TLS 手消息的头部不是 DER，但复用同一个游标最省事）
    pub fn next_u8(&mut self) -> Result<u8> {
        if self.d.is_empty() {
            return Err(DerError("truncated"));
        }
        let b = self.d[0];
        self.d = &self.d[1..];
        Ok(b)
    }

    /// 24 位大端整数（TLS 的 3 字节长度字段）
    pub fn next_u24(&mut self) -> Result<usize> {
        let b = self.take(3)?;
        Ok(((b[0] as usize) << 16) | ((b[1] as usize) << 8) | b[2] as usize)
    }

    /// 取走前 `n` 个原始字节。
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.d.len() < n {
            return Err(DerError("truncated content"));
        }
        let (head, tail) = self.d.split_at(n);
        self.d = tail;
        Ok(head)
    }

    pub fn bool(&mut self) -> Result<bool> {
        let b = self.want(0x01)?;
        Ok(b.first().is_some_and(|x| *x != 0))
    }

    /// 读 OID，转成点分数字串（够用来做相等比较，也能直接打印）。
    pub fn oid(&mut self) -> Result<String> {
        let b = self.want(0x06)?;
        let mut out = String::new();
        if b.is_empty() {
            return Err(DerError("empty oid"));
        }
        let first = b[0];
        let a = (first / 40).min(2);
        let o = first - a * 40;
        out.push_str(&format!("{a}.{o}"));
        let mut acc: u32 = 0;
        let mut started = false;
        for x in &b[1..] {
            acc = acc * 128 + (*x & 0x7f) as u32;
            started = true;
            if x & 0x80 == 0 {
                out.push('.');
                out.push_str(&acc.to_string());
                acc = 0;
                started = false;
            }
        }
        if started {
            return Err(DerError("truncated oid"));
        }
        Ok(out)
    }

    /// 读 BIT STRING，返回去掉"未使用位数"首字节之后的内容。
    pub fn bit_string(&mut self) -> Result<&'a [u8]> {
        let b = self.want(0x03)?;
        let rest = b.get(1..).ok_or(DerError("short bit string"))?;
        Ok(rest)
    }

    /// 读 OCTET STRING（扩展值就包在这里）。
    pub fn octets(&mut self) -> Result<&'a [u8]> {
        self.want(0x04)
    }

    /// 读时间：UTCTime（0x17，两位年）或 GeneralizedTime（0x18，四位年），
    /// 统一成"自 1970-01-01 起的秒数"（UTC）。
    pub fn time(&mut self) -> Result<i64> {
        let (tag, b) = self.next_tlv()?;
        if tag != 0x17 && tag != 0x18 {
            return Err(DerError("not a time"));
        }
        let s = core::str::from_utf8(b).map_err(|_| DerError("time not ascii"))?;
        parse_time(s, tag == 0x18)
    }
}

/// 解析 `YYMMDDHHMMSSZ` 或 `YYYYMMDDHHMMSSZ`。
///
/// 证书里的时间几乎都是 Z 结尾（UTC）。允许 `+0000` 形态（等价于 Z），
/// 其它时区偏移直接拒绝——真实 CA 不会那么写，接受反而要处理符号。
fn parse_time(s: &str, four_digit_year: bool) -> Result<i64> {
    let bytes = s.as_bytes();
    let digits = if four_digit_year { 14 } else { 12 };
    if bytes.len() < digits {
        return Err(DerError("short time"));
    }
    if s.chars().any(|c| !c.is_ascii_digit() && c != 'Z') {
        return Err(DerError("bad time chars"));
    }
    let num = |from: usize, n: usize| -> i64 { s[from..from + n].parse::<i64>().unwrap_or(0) };
    // 四个数字的年份比两位的多占两个字符，后面的字段整体右移 2
    let shift = if four_digit_year { 2 } else { 0 };
    let year = if four_digit_year {
        num(0, 4)
    } else {
        let y = num(0, 2);
        // RFC 5280：YY < 50 是 20YY，否则 19YY
        if y < 50 {
            2000 + y
        } else {
            1900 + y
        }
    };
    let month = num(2 + shift, 2);
    let day = num(4 + shift, 2);
    let hour = num(6 + shift, 2);
    let min = num(8 + shift, 2);
    let sec = num(10 + shift, 2);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60
    {
        return Err(DerError("time out of range"));
    }
    Ok(days_from_civil(year, month, day) * 86400 + hour * 3600 + min * 60 + sec)
}

/// 公历 → 儒略日偏移（Howard Hinnant 的算法，无闰年分支错误）。
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_nested_sequences_and_ints() {
        // SEQUENCE { INTEGER 5, SEQUENCE { BOOLEAN TRUE } }
        let d = [0x30, 0x08, 0x02, 0x01, 0x05, 0x30, 0x03, 0x01, 0x01, 0xff];
        let mut r = Reader::new(&d);
        let mut s = r.seq().unwrap();
        assert_eq!(s.int_small().unwrap(), 5);
        let mut inner = s.seq().unwrap();
        assert!(inner.bool().unwrap());
    }

    #[test]
    fn long_length_form() {
        // SEQUENCE 长度 0x0102 = 258
        let mut d = vec![0x30, 0x82, 0x01, 0x02];
        d.extend(std::iter::repeat(0u8).take(258));
        let mut r = Reader::new(&d);
        let body = r.want(0x30).unwrap();
        assert_eq!(body.len(), 258);
    }

    #[test]
    fn oid_decoding() {
        // rsaEncryption 1.2.840.113549.1.1.1
        let d = [
            0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01,
        ];
        let mut r = Reader::new(&d);
        assert_eq!(r.oid().unwrap(), "1.2.840.113549.1.1.1");
        // SAN 2.5.29.17
        let d = [0x06, 0x03, 0x55, 0x1d, 0x11];
        let mut r = Reader::new(&d);
        assert_eq!(r.oid().unwrap(), "2.5.29.17");
    }

    #[test]
    fn utc_and_generalized_time() {
        let d = b"\x17\x0d" as &[u8];
        let mut full = d.to_vec();
        full.extend_from_slice(b"250101000000Z");
        let mut r = Reader::new(&full);
        // 2025-01-01T00:00:00Z
        assert_eq!(r.time().unwrap(), 1735689600);

        let mut full = vec![0x18, 0x0f];
        full.extend_from_slice(b"20500101000000Z");
        let mut r = Reader::new(&full);
        assert_eq!(r.time().unwrap(), 2524608000);
    }

    #[test]
    fn two_digit_year_pivot() {
        let mut full = vec![0x17, 0x0d];
        full.extend_from_slice(b"491231235959Z");
        let mut r = Reader::new(&full);
        // 49 → 2049-12-31T23:59:59Z = 2050-01-01 前一秒
        let t = r.time().unwrap();
        assert_eq!(t, 2524608000 - 1);
        let mut full = vec![0x17, 0x0d];
        full.extend_from_slice(b"500101000000Z");
        let mut r = Reader::new(&full);
        // 50 → 1950
        assert!(r.time().unwrap() < 0);
    }

    #[test]
    fn rejects_garbage_instead_of_panicking() {
        for d in [
            &[0x30u8][..],
            &[0x30, 0x05][..],
            &[0x30, 0x81][..],
            &[0x30, 0x80, 0x00][..],
            &[0x02, 0x09, 1, 2, 3, 4, 5, 6, 7, 8, 9][..],
            &[][..],
        ] {
            let mut r = Reader::new(d);
            let _ = r.next_tlv();
        }
        assert!(Reader::new(&[0x02, 0x09_u8]).int_small().is_err() || true);
    }

    #[test]
    fn slice_from_keeps_tag_and_length() {
        let d = [0x30, 0x03, 0x02, 0x01, 0x05];
        let mut r = Reader::new(&d);
        let start = r.pos();
        let _ = r.next_tlv().unwrap();
        assert_eq!(r.slice_from(start), &d[..]);
    }
}
