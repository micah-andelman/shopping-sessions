//! One-pass CSV record scanner: 64-byte blocks -> bitmasks of {',' '\n'} and '"', then walk set bits.
//! Same dialect as `fields` + `for_lines`: only '\n' ends a line; a field holding a quote must be
//! exactly "..." with no inner quote; wrong field count => line dropped; empty field => None.

#[inline(always)]
fn masks(p: &[u8]) -> (u64, u64) {
    debug_assert!(p.len() >= 64);
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    unsafe {
        use std::arch::x86_64::*;
        let a = _mm256_loadu_si256(p.as_ptr() as *const __m256i);
        let b = _mm256_loadu_si256(p.as_ptr().add(32) as *const __m256i);
        let c = _mm256_set1_epi8(b',' as i8);
        let n = _mm256_set1_epi8(b'\n' as i8);
        let q = _mm256_set1_epi8(b'"' as i8);
        let da = _mm256_movemask_epi8(_mm256_or_si256(_mm256_cmpeq_epi8(a, c), _mm256_cmpeq_epi8(a, n))) as u32 as u64;
        let db = _mm256_movemask_epi8(_mm256_or_si256(_mm256_cmpeq_epi8(b, c), _mm256_cmpeq_epi8(b, n))) as u32 as u64;
        let qa = _mm256_movemask_epi8(_mm256_cmpeq_epi8(a, q)) as u32 as u64;
        let qb = _mm256_movemask_epi8(_mm256_cmpeq_epi8(b, q)) as u32 as u64;
        (da | db << 32, qa | qb << 32)
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
    {
        let (mut d, mut q) = (0u64, 0u64);
        for i in 0..64 {
            let c = p[i];
            d |= ((c == b',' || c == b'\n') as u64) << i;
            q |= ((c == b'"') as u64) << i;
        }
        (d, q)
    }
}

/// Benchmark helper: only the 64-byte structural mask pass (no bit walk), returns a checksum.
pub fn masks_only(data: &[u8]) -> u64 {
    let mut acc = 0u64;
    let mut base = 0;
    while base + 64 <= data.len() {
        let (d, q) = masks(&data[base..base + 64]);
        acc = acc.wrapping_add((d | q).count_ones() as u64);
        base += 64;
    }
    acc
}

pub fn scan<'a, const N: usize>(data: &'a [u8], mut emit: impl FnMut(&[Option<&'a [u8]>; N])) {
    let len = data.len();
    let mut out: [Option<&'a [u8]>; N] = [None; N];
    let mut nf = 0usize;
    let mut bad = false;
    let mut fs = 0usize;
    let mut qc = 0u32;
    let mut qstart = false;
    let mut lq = 0usize;
    let mut tail = [0u8; 64];
    let mut base = 0usize;

    macro_rules! end_field {
        ($p:expr) => {{
            let p = $p;
            if !bad {
                let v: &'a [u8] = if qc == 0 {
                    &data[fs..p]
                } else if qc == 2 && qstart && lq + 1 == p && p - fs >= 2 {
                    &data[fs + 1..p - 1]
                } else {
                    bad = true;
                    &data[0..0]
                };
                if nf < N {
                    out[nf] = if v.is_empty() { None } else { Some(v) };
                } else {
                    bad = true;
                }
            }
            nf += 1;
            fs = p + 1;
            qc = 0;
            qstart = false;
        }};
    }
    macro_rules! end_line {
        () => {{
            if !bad && nf == N {
                emit(&out);
            }
            nf = 0;
            bad = false;
        }};
    }

    while base < len {
        let (d, q) = if base + 64 <= len {
            masks(&data[base..base + 64])
        } else {
            let n = len - base;
            tail = [0u8; 64];
            tail[..n].copy_from_slice(&data[base..]);
            masks(&tail)
        };
        let mut m = d | q;
        while m != 0 {
            let i = m.trailing_zeros() as usize;
            m &= m - 1;
            let p = base + i;
            if (q >> i) & 1 == 1 {
                qc += 1;
                lq = p;
                if p == fs {
                    qstart = true;
                }
                continue;
            }
            end_field!(p);
            if data[p] == b'\n' {
                end_line!();
            }
        }
        base += 64;
    }
    if fs < len || nf > 0 {
        end_field!(len);
        end_line!();
    }
}

#[cfg(test)]
mod tests {
    use super::scan;
    use crate::{fields, for_lines};

    fn check<const N: usize>(data: &[u8]) {
        let mut a: Vec<Vec<Option<Vec<u8>>>> = Vec::new();
        for_lines(data, |l| {
            if let Some(f) = fields::<N>(l) {
                a.push(f.iter().map(|x| x.map(|v| v.to_vec())).collect());
            }
        });
        let mut b: Vec<Vec<Option<Vec<u8>>>> = Vec::new();
        scan::<N>(data, |f| b.push(f.iter().map(|x| x.map(|v| v.to_vec())).collect()));
        assert_eq!(a, b, "input {:?}", String::from_utf8_lossy(data));
    }

    #[test]
    fn matches_reference_parser() {
        let alpha = b"ab,,,\"\"\n\n\r 1";
        let mut x: u64 = 0x1234_5678_9abc_def1;
        for _ in 0..300_000 {
            x ^= x << 13; x ^= x >> 7; x ^= x << 17;
            let len = (x % 200) as usize;
            let mut d = Vec::with_capacity(len);
            for _ in 0..len {
                x ^= x << 13; x ^= x >> 7; x ^= x << 17;
                d.push(alpha[(x % alpha.len() as u64) as usize]);
            }
            check::<2>(&d);
            check::<4>(&d);
            check::<7>(&d);
        }
    }
}
