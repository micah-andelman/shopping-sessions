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

// ---------------------------------------------------------------------------------------------
// Two-stage scanner. Same dialect and same closure interface as `scan` (which stays as the oracle).
//
// In this dialect there is no "inside quotes" state: '\n' always ends a line and every ',' always
// splits, so the delimiter mask needs no quote parity. A quote is legal only as the first byte of a
// field (the byte before it is a delimiter or the line start) paired with a quote as the last byte
// (the byte after it is a delimiter), and the two must be different bytes.
//
// Stage 1, per 64-byte block, no data-dependent branches: masks for delimiters (',' | '\n'),
// newlines and quotes; delimiter and newline positions are flattened into two u32 buffers; a quote
// that is NOT exactly one of {right after a delimiter, right before a delimiter} is recorded in a
// (normally empty) "bad" list.
// Stage 2, per line: the line's N delimiters are N consecutive entries of the delimiter buffer; if
// the N-th one is the line's newline, no bad quote lies in the line, and every field has a quote at
// both ends or at neither, the fields are cut branch-free. Any other line is handed to the
// reference `fields` (exact, slow), as is a last line without '\n' and a line longer than a batch.

/// Input bytes per batch (multiple of 64). Position buffers for one batch stay in L1/L2.
pub const BATCH: usize = 16 * 1024;

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    /// Lines seen (including empty and dropped ones).
    pub lines: u64,
    /// Lines that went through the exact reference path.
    pub slow: u64,
}

/// (delimiters = ',' | '\n', newlines, quotes) for 64 bytes.
#[inline(always)]
fn masks3(p: &[u8]) -> (u64, u64, u64) {
    debug_assert!(p.len() >= 64);
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    unsafe {
        use std::arch::x86_64::*;
        let a = _mm256_loadu_si256(p.as_ptr() as *const __m256i);
        let b = _mm256_loadu_si256(p.as_ptr().add(32) as *const __m256i);
        let c = _mm256_set1_epi8(b',' as i8);
        let n = _mm256_set1_epi8(b'\n' as i8);
        let q = _mm256_set1_epi8(b'"' as i8);
        let (na, nb) = (_mm256_cmpeq_epi8(a, n), _mm256_cmpeq_epi8(b, n));
        let da = _mm256_movemask_epi8(_mm256_or_si256(_mm256_cmpeq_epi8(a, c), na)) as u32 as u64;
        let db = _mm256_movemask_epi8(_mm256_or_si256(_mm256_cmpeq_epi8(b, c), nb)) as u32 as u64;
        let la = _mm256_movemask_epi8(na) as u32 as u64;
        let lb = _mm256_movemask_epi8(nb) as u32 as u64;
        let qa = _mm256_movemask_epi8(_mm256_cmpeq_epi8(a, q)) as u32 as u64;
        let qb = _mm256_movemask_epi8(_mm256_cmpeq_epi8(b, q)) as u32 as u64;
        (da | db << 32, la | lb << 32, qa | qb << 32)
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
    {
        let (mut d, mut l, mut q) = (0u64, 0u64, 0u64);
        for i in 0..64 {
            let c = p[i];
            d |= ((c == b',' || c == b'\n') as u64) << i;
            l |= ((c == b'\n') as u64) << i;
            q |= ((c == b'"') as u64) << i;
        }
        (d, l, q)
    }
}

/// Append the positions of the set bits of `m` (plus `off`) at `*n`. Writes whole groups
/// unconditionally (up to 7 slots past the last real entry), then advances by the popcount.
///
/// Safety: `buf` must have room for `*n + 64 + 8` entries.
#[inline(always)]
unsafe fn flatten<const G: usize>(buf: *mut u32, n: &mut usize, mut m: u64, off: u32) {
    let cnt = m.count_ones() as usize;
    let mut p = buf.add(*n);
    for _ in 0..G {
        *p = off + m.trailing_zeros();
        m &= m.wrapping_sub(1);
        p = p.add(1);
    }
    if cnt > G {
        let mut done = G;
        while done < cnt {
            for _ in 0..8 {
                *p = off + m.trailing_zeros();
                m &= m.wrapping_sub(1);
                p = p.add(1);
            }
            done += 8;
        }
    }
    *n += cnt;
}

#[cold]
#[inline(never)]
fn slow_line<'a, const N: usize>(line: &'a [u8], emit: &mut impl FnMut(&[Option<&'a [u8]>; N])) {
    if let Some(f) = crate::fields::<N>(line) {
        emit(&f);
    }
}

/// Drop-in replacement for `scan`.
#[inline]
pub fn scan2<'a, const N: usize>(data: &'a [u8], emit: impl FnMut(&[Option<&'a [u8]>; N])) -> Stats {
    scan2_batched::<N>(data, BATCH, emit)
}

/// `batch` must be a positive multiple of 64 (tests use tiny batches to exercise the seams).
#[inline(always)]
pub fn scan2_batched<'a, const N: usize>(data: &'a [u8], batch: usize, mut emit: impl FnMut(&[Option<&'a [u8]>; N])) -> Stats {
    assert!(N >= 1 && batch >= 64 && batch % 64 == 0 && batch <= 1 << 30);
    let len = data.len();
    let dp = data.as_ptr();
    let mut st = Stats::default();
    // Worst case every byte is a delimiter / newline / follows a quote; + slack for `flatten` and sentinels.
    let cap = batch + 64 + 16 + N;
    let mut dl: Vec<u32> = vec![0; cap];
    let mut nl: Vec<u32> = vec![0; cap];
    let mut bad: Vec<u32> = vec![0; cap];
    let mut base = 0usize; // always the first byte of a line
    while base < len {
        let end = (base + batch).min(len);
        let last = end == len;
        // ---- stage 1: positions relative to `base`
        let (mut nd, mut nn, mut nb) = (0usize, 0usize, 0usize);
        let mut cq = 0u64; // previous byte is a quote
        let mut cd = 0b10u64; // bit 1: previous byte is a delimiter (line start), bit 0: the one before
        let mut o = base;
        while o < end {
            let (d, l, q) = if o + 64 <= len {
                masks3(&data[o..o + 64])
            } else {
                let mut tail = [0u8; 64];
                tail[..len - o].copy_from_slice(&data[o..]);
                masks3(&tail)
            };
            let off = (o - base) as u32;
            // Bit i of `b`: byte i-1 is a quote that is neither "right after a delimiter, not right
            // before one" nor "right before a delimiter, not right after one".
            let pq = q << 1 | cq;
            let p2d = d << 2 | cd;
            let mut b = pq & !(p2d ^ d);
            cq = q >> 63;
            cd = d >> 62;
            // Safety: nd, nn <= o - base <= batch - 64 here, each call adds at most 64 and may
            // write 7 slots beyond; cap = batch + 64 + 16 + N.
            unsafe {
                flatten::<8>(dl.as_mut_ptr(), &mut nd, d, off);
                flatten::<2>(nl.as_mut_ptr(), &mut nn, l, off);
            }
            while b != 0 {
                bad[nb] = off + b.trailing_zeros();
                nb += 1;
                b &= b - 1;
            }
            o += 64;
        }
        if nn == 0 {
            // No complete line in this batch: one line longer than the batch, or the last line.
            let stop = if last { len } else { memchr::memchr(b'\n', &data[end..]).map_or(len, |p| end + p) };
            st.lines += 1;
            st.slow += 1;
            slow_line::<N>(&data[base..stop], &mut emit);
            base = stop + 1;
            continue;
        }
        for i in 0..N {
            dl[nd + i] = u32::MAX;
        }
        bad[nb] = u32::MAX;
        // ---- stage 2
        let (mut k, mut bi) = (0usize, 0usize);
        let mut ls = 0u32;
        let lp = unsafe { dp.add(base) };
        for j in 0..nn {
            // Safety (all unchecked reads below): j < nn; k + N - 1 < nd + N (k never passes the
            // entry of the current newline, which is < nd); bi <= nb (the sentinel stops it).
            let le = unsafe { *nl.get_unchecked(j) };
            let nth = unsafe { *dl.get_unchecked(k + N - 1) };
            let b0 = unsafe { *bad.get_unchecked(bi) };
            if nth == le && b0 > le {
                // Exactly N-1 commas, then this newline. Field i is [s, e) with s <= e <= le < len - base.
                let mut out: [Option<&'a [u8]>; N] = [None; N];
                let mut s = ls as usize;
                let mut odd = 0usize;
                for i in 0..N {
                    let e = unsafe { *dl.get_unchecked(k + i) } as usize;
                    debug_assert!(s <= e && base + e < len);
                    // An empty field reads the delimiter at s (== e) twice: never a quote.
                    let qs = unsafe { *lp.add(s) == b'"' } as usize;
                    let qe = unsafe { *lp.add(e - (e > s) as usize) == b'"' } as usize;
                    odd |= qs ^ qe;
                    let (a, z) = (s + qs, e - qe);
                    out[i] = if a < z { Some(unsafe { std::slice::from_raw_parts(lp.add(a), z - a) }) } else { None };
                    s = e + 1;
                }
                k += N;
                st.lines += 1;
                if odd == 0 {
                    emit(&out);
                } else {
                    st.slow += 1;
                    slow_line::<N>(&data[base + ls as usize..base + le as usize], &mut emit);
                }
            } else {
                st.lines += 1;
                st.slow += 1;
                slow_line::<N>(&data[base + ls as usize..base + le as usize], &mut emit);
                while bad[bi] <= le {
                    bi += 1;
                }
                while dl[k] != le {
                    k += 1;
                }
                k += 1;
            }
            ls = le + 1;
        }
        base += ls as usize;
        if last {
            if base < len {
                st.lines += 1;
                st.slow += 1;
                slow_line::<N>(&data[base..], &mut emit);
            }
            break;
        }
    }
    st
}

#[cfg(test)]
mod tests {
    use super::{scan, scan2, scan2_batched};
    use crate::{fields, for_lines};

    type Rows = Vec<Vec<Option<Vec<u8>>>>;

    fn reference<const N: usize>(data: &[u8]) -> Rows {
        let mut a: Rows = Vec::new();
        for_lines(data, |l| {
            if let Some(f) = fields::<N>(l) {
                a.push(f.iter().map(|x| x.map(|v| v.to_vec())).collect());
            }
        });
        a
    }

    fn check<const N: usize>(data: &[u8]) {
        let a = reference::<N>(data);
        let mut b: Rows = Vec::new();
        scan::<N>(data, |f| b.push(f.iter().map(|x| x.map(|v| v.to_vec())).collect()));
        assert_eq!(a, b, "old scan, N={} input {:?}", N, String::from_utf8_lossy(data));
        for batch in [0usize, 64, 128, 192] {
            let mut c: Rows = Vec::new();
            let mut push = |f: &[Option<&[u8]>; N]| {
                for x in f.iter().flatten() {
                    // Every emitted slice must lie inside the input.
                    let (p, q) = (x.as_ptr() as usize, data.as_ptr() as usize);
                    assert!(p >= q && p + x.len() <= q + data.len());
                }
                c.push(f.iter().map(|x| x.map(|v| v.to_vec())).collect())
            };
            if batch == 0 {
                scan2::<N>(data, &mut push);
            } else {
                scan2_batched::<N>(data, batch, &mut push);
            }
            assert_eq!(a, c, "scan2 batch {} N={} input {:?}", batch, N, String::from_utf8_lossy(data));
        }
    }

    fn check_all(data: &[u8]) {
        check::<1>(data);
        check::<2>(data);
        check::<3>(data);
        check::<4>(data);
        check::<7>(data);
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let x = &mut self.0;
            *x ^= *x << 13;
            *x ^= *x >> 7;
            *x ^= *x << 17;
            *x
        }
        fn fill(&mut self, alpha: &[u8], len: usize) -> Vec<u8> {
            (0..len).map(|_| alpha[(self.next() % alpha.len() as u64) as usize]).collect()
        }
    }

    #[test]
    fn matches_reference_parser() {
        let alpha = b"ab,,,\"\"\n\n\r 1";
        let mut r = Rng(0x1234_5678_9abc_def1);
        for _ in 0..300_000 {
            let len = (r.next() % 200) as usize;
            let d = r.fill(alpha, len);
            check::<2>(&d);
            check::<4>(&d);
            check::<7>(&d);
        }
    }

    /// Mostly well-formed lines (so the fast path really runs), with quoted fields and rare damage.
    fn gen_lines(r: &mut Rng, nf: usize, lines: usize, damage: u64) -> Vec<u8> {
        let mut d = Vec::new();
        for _ in 0..lines {
            let n = if r.next() % 16 == 0 { (r.next() % 10) as usize } else { nf };
            for i in 0..n {
                if i > 0 {
                    d.push(b',');
                }
                let l = (r.next() % 12) as usize;
                let body = r.fill(b"abcxyz0123456789-:TZ\r ", l);
                match r.next() % 8 {
                    0 => {}
                    1 | 2 | 3 => {
                        d.push(b'"');
                        d.extend(&body);
                        d.push(b'"');
                    }
                    _ => d.extend(&body),
                }
            }
            d.push(b'\n');
        }
        if damage > 0 {
            for _ in 0..(d.len() as u64 / damage) {
                let i = (r.next() % d.len() as u64) as usize;
                d[i] = b"\",\n\ra"[(r.next() % 5) as usize];
            }
        }
        if r.next() % 2 == 0 {
            d.pop();
        }
        d
    }

    #[test]
    fn mostly_valid_lines() {
        let mut r = Rng(0x9e37_79b9_7f4a_7c15);
        for round in 0..6000 {
            for nf in [2usize, 4, 7] {
                let lines = 1 + (r.next() % 12) as usize;
                let d = gen_lines(&mut r, nf, lines, [0, 40, 200][round % 3]);
                check::<2>(&d);
                check::<4>(&d);
                check::<7>(&d);
            }
        }
        // Larger inputs through the production batch size.
        for nf in [2usize, 4, 7] {
            for damage in [0u64, 500, 5000] {
                let d = gen_lines(&mut r, nf, 4000, damage);
                check_all(&d);
            }
        }
    }

    #[test]
    fn every_short_string_over_structural_bytes() {
        // Exhaustive up to length 8 over {',', '"', '\n', 'a'}.
        let alpha = b",\"\na";
        for len in 0..=8usize {
            for code in 0..(1u32 << (2 * len)) {
                let d: Vec<u8> = (0..len).map(|i| alpha[((code >> (2 * i)) & 3) as usize]).collect();
                check_all(&d);
            }
        }
        // Random, every length 0..=200 (and around the block and batch seams).
        let mut r = Rng(0xdead_beef_cafe_f00d);
        for len in (0..=200usize).chain(250..=262).chain(16376..=16392) {
            let reps = if len > 300 { 6 } else { 400 };
            for _ in 0..reps {
                let d = r.fill(alpha, len);
                check_all(&d);
                let d = r.fill(b"aaaaaaaa,,\"\n", len);
                check_all(&d);
                let d = r.fill(b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa,,,,\"\"\"\n\r", len);
                check_all(&d);
            }
        }
    }

    #[test]
    fn quotes_and_delimiters_at_block_seams() {
        // A record whose structural bytes are slid across positions 0..=146, with every quote
        // arrangement: covers quote at 63 + comma at 64, opening quote at 64, and so on.
        let shapes: [&[u8]; 14] = [
            b"\"x\",y\n", b"x,\"y\"\n", b"\"x\",\"y\"\n", b"\"\",\"\"\n", b"\",\"\n", b"\"\n\"\n", b"x\",y\n",
            b"\"x,y\n", b"x,y\"\n", b"x,\"y\n", b"\"x\"y,z\n", b"x\"\"y,z\n", b"\"\"\"\",z\n", b"\"x\"\r,z\n",
        ];
        for pad in 0..=140usize {
            for sh in shapes.iter() {
                for pre in [&b"a"[..], b"a,", b"a\n", b"\"", b","] {
                    let mut d = Vec::new();
                    while d.len() < pad {
                        d.extend_from_slice(pre);
                    }
                    d.truncate(pad);
                    d.extend_from_slice(sh);
                    check_all(&d);
                    d.extend_from_slice(b"p,q\n\"p\",\"q\"");
                    check_all(&d);
                    d.push(b'\n');
                    check_all(&d);
                }
            }
        }
    }

    #[test]
    fn long_lines() {
        use super::BATCH as B;
        // Lines longer than a block, than the test batches, and than the production batch, with the
        // last separator landing on and around every seam; followed and preceded by normal lines.
        for total in [62usize, 63, 64, 65, 66, 126, 127, 128, 129, 130, 190, 191, 192, 193, 255, 256, 257, B - 2, B - 1, B, B + 1, B + 70, 3 * B + 5] {
            for quoted in [false, true] {
                for tailnl in [false, true] {
                    // "<long>,k\n" where the newline is at index total-1.
                    let mut line = Vec::new();
                    if quoted {
                        line.push(b'"');
                    }
                    while line.len() + 3 + (quoted as usize) < total {
                        line.push(b'z');
                    }
                    if quoted {
                        line.push(b'"');
                    }
                    line.extend_from_slice(b",k\n");
                    let mut d = line.clone();
                    check_all(&d);
                    d.extend_from_slice(b"a,b\n");
                    d.extend_from_slice(&line);
                    if !tailnl {
                        d.pop();
                    }
                    check_all(&d);
                    let mut e = b"a,\"b\"\n\n".to_vec();
                    e.extend_from_slice(&d);
                    check_all(&e);
                    // A broken quote deep inside the long line.
                    let mid = e.len() / 2;
                    e[mid] = b'"';
                    check_all(&e);
                }
            }
        }
        // Seven-field lines of every length 13..=300 (last separator sweeps bytes 63/64/127/128).
        for flen in 1..=42usize {
            for extra in 0..7usize {
                let mut d = Vec::new();
                for rep in 0..3 {
                    for i in 0..7 {
                        if i > 0 {
                            d.push(b',');
                        }
                        let l = flen + (i < extra) as usize;
                        if (i + rep) % 3 == 0 {
                            d.push(b'"');
                            d.extend(std::iter::repeat(b'q').take(l));
                            d.push(b'"');
                        } else {
                            d.extend(std::iter::repeat(b'f').take(l));
                        }
                    }
                    d.push(b'\n');
                }
                check_all(&d);
            }
        }
    }

    #[test]
    fn dialect_cases() {
        let cases: &[&[u8]] = &[
            b"", b"\n", b"\n\n\n", b",", b",\n", b",,,,,,", b",,,,,,\n", b",,,,,,,\n", b",,,,,\n",
            b"a", b"a\n", b"a,b", b"a,b\n", b"a,b\n\n", b"\na,b\n", b"a,b\r\n", b"a,b\r", b"a\r,b\n", b"\r\n", b"\r",
            b"\"", b"\"\n", b"\"\"", b"\"\"\n", b"\"\"\"", b"\"\"\"\"", b"\"a\"", b"\"a\"\n", b"a\"", b"\"a", b"a\"b",
            b"\"a\",\"\"\n", b"\"\",\"\"\n", b"\",\"\n", b"\"a,b\"\n", b"\"a\nb\"\n", b"\"a\",b\"\n", b"a,\"b\n\"c\",d\n",
            b"\"a\"\r,b\n", b"\"a\r\",b\n", b"\r\"a\",b\n", b"a,\"b\"\r\n", b"a,\"b\r\"\n",
            b"1,2,3,4,5,6,7\n", b"1,2,3,4,5,6,7", b"1,2,3,4,5,6,7,8\n", b"1,2,3,4,5,6\n", b"1,2,3,4,5,6,7,\n", b",1,2,3,4,5,6,7\n",
            b"1,2,3,4,5,6,\"\"\n", b"1,2,3,4,5,6,\"\"", b"1,2,3,4,5,6,\"", b"1,2,3,4,5,6,", b"\"1\",\"2\",\"3\",\"4\",\"5\",\"6\",\"7\"",
            b"e3\r,u,t,t,view,p,\ne3,u,t,t,view,p,1000\r\n",
            b"h1,h2\n", b"h1,h2", b"h1,h2\n\n", b"x,y\nx,y,z\nx\n\nx,y\n,\n\"\",\n,\"\"\nx,y",
            "\u{ff11},\u{0661}\n\u{e9},\"\u{e9}\"\n".as_bytes(), b"\x00,\x00\n\xff,\"\xff\"\n",
        ];
        for c in cases {
            check_all(c);
            // The same case behind and in front of filler, so it also meets every seam.
            for pad in [57usize, 60, 61, 62, 63, 64, 65, 120, 125, 126, 127, 128, 129] {
                let mut d = vec![b'a'; pad - 1];
                d.push(b'\n');
                d.extend_from_slice(c);
                check_all(&d);
                let mut d = c.to_vec();
                d.push(b'\n');
                d.extend(std::iter::repeat(b'b').take(pad));
                check_all(&d);
            }
        }
        // Spot checks of the meaning itself, not only agreement with the reference.
        let rows = |d: &[u8]| {
            let mut v: Rows = Vec::new();
            scan2::<2>(d, |f| v.push(f.iter().map(|x| x.map(|v| v.to_vec())).collect()));
            v
        };
        assert_eq!(rows(b"\"a\",\"\"\n,b\r\n\",x\nq\"q,x\n\"\"\",x\nlast,\"z\""), vec![
            vec![Some(b"a".to_vec()), None],
            vec![None, Some(b"b\r".to_vec())],
            vec![Some(b"last".to_vec()), Some(b"z".to_vec())],
        ]);
    }

    #[test]
    fn more_than_n_fields_is_dropped_not_truncated() {
        for n in 0..40usize {
            let mut d = Vec::new();
            for rep in 0..3 {
                for i in 0..n {
                    if i > 0 {
                        d.push(b',');
                    }
                    d.extend_from_slice(if (i + rep) % 2 == 0 { b"\"v\"" } else { b"w" });
                }
                d.push(b'\n');
            }
            check_all(&d);
        }
        // All commas, all newlines, all quotes: the densest position streams.
        for len in [1usize, 63, 64, 65, 1000, super::BATCH, super::BATCH + 1, 2 * super::BATCH + 77] {
            for ch in [b',', b'\n', b'"'] {
                check_all(&vec![ch; len]);
            }
        }
    }

    #[test]
    fn splitting_at_any_line_start_gives_the_same_rows() {
        // The callers hand `scan2` chunks that begin at a line start; rows must not depend on the cut.
        let mut r = Rng(0x0123_4567_89ab_cdef);
        for _ in 0..300 {
            let d = gen_lines(&mut r, 4, 30, 60);
            let whole = reference::<4>(&d);
            let cuts: Vec<usize> = std::iter::once(0).chain(d.iter().enumerate().filter(|x| *x.1 == b'\n').map(|x| x.0 + 1)).collect();
            for &c in &cuts {
                let mut got: Rows = Vec::new();
                scan2::<4>(&d[..c], |f| got.push(f.iter().map(|x| x.map(|v| v.to_vec())).collect()));
                scan2::<4>(&d[c..], |f| got.push(f.iter().map(|x| x.map(|v| v.to_vec())).collect()));
                assert_eq!(whole, got);
            }
        }
    }

    /// The input ends exactly at the end of a mapping and the next page is inaccessible: any read
    /// past the last byte (or before the first) faults.
    #[cfg(target_os = "linux")]
    #[test]
    fn no_read_outside_the_input() {
        unsafe {
            let pg = 4096usize;
            let m = libc::mmap(std::ptr::null_mut(), 8 * pg, libc::PROT_NONE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0) as *mut u8;
            assert!(m as isize != -1);
            let mid = m.add(pg);
            assert_eq!(libc::mprotect(mid as *mut libc::c_void, 6 * pg, libc::PROT_READ | libc::PROT_WRITE), 0);
            let mut r = Rng(0x5555_aaaa_1234_8765);
            let mut tails: Vec<Vec<u8>> = vec![
                b"a,b".to_vec(), b"a,b\n".to_vec(), b"a,\"b\"".to_vec(), b"a,\"".to_vec(), b"a,".to_vec(), b",".to_vec(), b"\"".to_vec(),
                b"\n".to_vec(), b"1,2,3,4,5,6,".to_vec(), b"1,2,3,4,5,6,\"\"".to_vec(), vec![b','; 200], vec![b'"'; 200], vec![b'\n'; 200],
            ];
            for _ in 0..40 {
                tails.push(gen_lines(&mut r, 7, 3, 30));
            }
            for len in [0usize, 1, 2, 3, 63, 64, 65, 127, 128, 129, 4095, 4096, 4097, 6 * pg - 1, 6 * pg] {
                for t in &tails {
                    for at_end in [true, false] {
                        // at_end: the slice's last byte is the mapping's last byte; else its first is the first.
                        let start = if at_end { mid.add(6 * pg - len) } else { mid };
                        let buf = std::slice::from_raw_parts_mut(start, len);
                        let mut body = gen_lines(&mut r, 7, len / 20 + 1, 300);
                        body.push(b'a');
                        for i in 0..len {
                            buf[i] = body[i % body.len()];
                        }
                        let k = t.len().min(len);
                        if at_end {
                            buf[len - k..].copy_from_slice(&t[t.len() - k..]);
                        } else {
                            buf[..k].copy_from_slice(&t[..k]);
                        }
                        let d: &[u8] = buf;
                        check::<2>(d);
                        check::<7>(d);
                    }
                }
            }
            libc::munmap(m as *mut libc::c_void, 8 * pg);
        }
    }
}
