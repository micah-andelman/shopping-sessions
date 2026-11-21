//! v5 fast path. Same rules as `legacy`, different data layout:
//! - every ID becomes an exact 64-bit key: 1..=10 bytes of [0-9A-Z_a-z] packed 6 bits per byte
//!   (integer order == byte order), anything else = hash | 1<<63 with a global collision check
//!   (a collision makes us return false and the caller runs the byte-exact legacy path);
//! - events are scattered by event_id hash into 256 buckets while parsing, deduped per bucket in a
//!   cache-sized hash table, and survivors become 16-byte (person key, ts|price|cat) records;
//! - one global sort per table, then the same per-person sessionization as legacy.
//! Returns false (before writing anything) whenever an assumption does not hold.

use super::*;
use std::collections::HashMap;
use super::scan::{scan, scan2, scan3, scan3a, Line};

const NB: usize = 256;
/// Typical click line length in bytes, a little low on purpose: sizes the per-group click vector
/// (bytes / this). Only a capacity hint: a denser file just makes the vector grow.
const CK_LINE_EST: usize = 40;
const FB: u64 = 1 << 63;
const NONE: u64 = u64::MAX;

#[inline]
fn mix(k: u64) -> u64 {
    let x = (k ^ (k >> 31)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^ (x >> 29)
}
#[inline]
fn bucket(k: u64) -> usize {
    (mix(k) >> 56) as usize
}
#[inline]
fn th(k: u64) -> u64 {
    // bucket() uses the top 8 bits; keep them out of hashbrown's index bits and tag bits
    mix(k).rotate_right(8)
}

const fn code_table() -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut c = 1u8;
    let mut b = 0usize;
    while b < 256 {
        let ch = b as u8;
        if ch.is_ascii_digit() || ch.is_ascii_uppercase() || ch == b'_' || ch.is_ascii_lowercase() {
            t[b] = c;
            c += 1;
        }
        b += 1;
    }
    t
}
const fn dec_table() -> [u8; 64] {
    let mut d = [0u8; 64];
    let mut b = 0usize;
    while b < 256 {
        let c = CODE[b];
        if c != 0 {
            d[c as usize] = b as u8;
        }
        b += 1;
    }
    d
}
static CODE: [u8; 256] = code_table();
static DEC: [u8; 64] = dec_table();

/// Exact order-preserving key for short alphanumeric IDs. `b` is non-empty.
/// Reference implementation: one table lookup per byte.
#[inline]
fn pack_ref(b: &[u8]) -> Option<u64> {
    if b.len() > 10 {
        return None;
    }
    let mut k = 0u64;
    let mut sh = 54u32;
    for &c in b {
        let x = CODE[c as usize];
        if x == 0 {
            return None;
        }
        k |= (x as u64) << sh;
        sh = sh.wrapping_sub(6);
    }
    Some(k)
}

/// Same key as `pack_ref` for every input, computed from one 16-byte load: two nibble lookups
/// classify every byte, a third gives its 6-bit code, PEXT squeezes the codes together.
/// The load reads up to 16 - len bytes past the field, so it is only used when all 16 bytes lie
/// in the page that holds the field's first byte (a page that holds a readable byte is readable
/// to its end, also for the last page of a file mapping); fields that start in the last 15 bytes
/// of a page take `pack_ref`. Bytes past `len` are masked out before they can affect the result.
#[inline]
fn pack(b: &[u8]) -> Option<u64> {
    #[cfg(all(target_arch = "x86_64", target_feature = "ssse3", target_feature = "bmi2"))]
    {
        let n = b.len();
        if n > 10 {
            return None;
        }
        if n != 0 && (b.as_ptr() as usize) & 4095 <= 4096 - 16 {
            // SAFETY: the 16 bytes at b.as_ptr() are inside one mapped page (see above).
            return unsafe { pack16(b.as_ptr(), n) };
        }
    }
    pack_ref(b)
}

/// `p` must be readable for 16 bytes; `n <= 10` is the field length.
#[cfg(all(target_arch = "x86_64", target_feature = "ssse3", target_feature = "bmi2"))]
#[inline(always)]
unsafe fn pack16(p: *const u8, n: usize) -> Option<u64> {
    use std::arch::x86_64::*;
    debug_assert!(n <= 10);
    let v = _mm_loadu_si128(p as *const __m128i);
    // Class bit per high nibble: 3x -> 1, 4x/6x -> 2, 5x -> 4, 7x -> 8 (everything else 0), and
    // per low nibble the set of classes in which that low nibble is an alphabet byte:
    // 30-39 digits, 41-4F / 61-6F letters, 50-5A + 5F ('_'), 70-7A. A byte is valid iff the two
    // sets intersect. pshufb yields 0 for index bytes >= 0x80, so non-ASCII is never valid.
    let lo_lut = _mm_setr_epi8(13, 15, 15, 15, 15, 15, 15, 15, 15, 15, 14, 2, 2, 2, 2, 6);
    let hi_lut = _mm_setr_epi8(0, 0, 0, 1, 2, 4, 2, 8, 0, 0, 0, 0, 0, 0, 0, 0);
    // code = byte + offset: '0' -> 1, 'A' -> 11, 'a' -> 38; '_' (0x5F) -> 37 needs 4 less than 'P'..'Z'.
    let off_lut = _mm_setr_epi8(0, 0, 0, -0x2F, -0x36, -0x36, -0x3B, -0x3B, 0, 0, 0, 0, 0, 0, 0, 0);
    let hi_n = _mm_and_si128(_mm_srli_epi16(v, 4), _mm_set1_epi8(0x0F));
    let class = _mm_and_si128(_mm_shuffle_epi8(lo_lut, v), _mm_shuffle_epi8(hi_lut, hi_n));
    let bad = _mm_movemask_epi8(_mm_cmpeq_epi8(class, _mm_setzero_si128())) as u32;
    if bad & ((1u32 << n) - 1) != 0 {
        return None;
    }
    let us = _mm_and_si128(_mm_cmpeq_epi8(v, _mm_set1_epi8(b'_' as i8)), _mm_set1_epi8(-4));
    let codes = _mm_add_epi8(_mm_add_epi8(v, _mm_shuffle_epi8(off_lut, hi_n)), us);
    // Keep only the first n code bytes (BZHI with an index >= 64 keeps everything).
    let lo = _bzhi_u64(_mm_cvtsi128_si64(codes) as u64, 8 * n as u32);
    let hi = _bzhi_u64(_mm_extract_epi16(codes, 4) as u64, (8 * n as u32).saturating_sub(64));
    // Byte 0 must end up in the top 6 bits of the 60-bit key: reverse the bytes, squeeze 8 codes
    // into 48 bits, then append codes 8 and 9.
    let a = _pext_u64(lo.swap_bytes(), 0x3F3F_3F3F_3F3F_3F3F);
    Some(a << 12 | (hi & 0xFF) << 6 | hi >> 8)
}

type Fb = Vec<(u64, &'static [u8])>;

/// Small id -> value table keyed by the exact `pack` key (campaign ids): one multiply and usually
/// one 16-byte probe instead of hashing the bytes and comparing slices. Ids that do not pack
/// (longer than 10 bytes or outside the alphabet) are not stored here; `get` sends them to the Dict.
struct PackTab {
    mask: usize,
    /// (pack key, value); key 0 = empty slot (a packed non-empty id is never 0).
    slots: Vec<(u64, u32)>,
}
impl PackTab {
    /// `entries`: the Dict's (id, value) pairs, each id once.
    fn new<'x>(entries: impl Iterator<Item = (&'x [u8], u32)> + Clone) -> Self {
        let cap = (entries.clone().count() * 4).max(16).next_power_of_two();
        let mut t = PackTab { mask: cap - 1, slots: vec![(0, 0); cap] };
        for (id, v) in entries {
            if id.is_empty() {
                continue;
            }
            if let Some(k) = pack(id) {
                let mut i = mix(k) as usize & t.mask;
                while t.slots[i].0 != 0 && t.slots[i].0 != k {
                    i = (i + 1) & t.mask;
                }
                if t.slots[i].0 == 0 {
                    t.slots[i] = (k, v);
                }
            }
        }
        t
    }
    /// Same answer as `dict.get(id)` when `self` was built from all of `dict`'s entries. `id` is non-empty.
    #[inline]
    fn get(&self, id: &[u8], dict: &Dict) -> Option<u32> {
        match pack(id) {
            Some(k) => {
                let mut i = mix(k) as usize & self.mask;
                loop {
                    let s = self.slots[i];
                    if s.0 == k {
                        return Some(s.1);
                    }
                    if s.0 == 0 {
                        return None;
                    }
                    i = (i + 1) & self.mask;
                }
            }
            None => dict.get(id),
        }
    }
}

#[inline]
fn key_of(b: &'static [u8], fb: &mut Fb) -> u64 {
    match pack(b) {
        Some(k) => k,
        None => {
            let k = h64(b) | FB;
            fb.push((k, b));
            k
        }
    }
}

fn bytes_of(k: u64, fbm: &HashMap<u64, &'static [u8]>) -> Vec<u8> {
    if k & FB != 0 {
        return fbm[&k].to_vec();
    }
    let mut s = Vec::with_capacity(10);
    let mut sh = 54i32;
    while sh >= 0 {
        let x = ((k >> sh) & 63) as usize;
        if x == 0 {
            break;
        }
        s.push(DEC[x]);
        sh -= 6;
    }
    s
}

/// Append the ID bytes of key `k` to `out` (same decoding as bytes_of, no allocation).
#[inline]
fn id_into(k: u64, fbm: &HashMap<u64, &'static [u8]>, out: &mut Vec<u8>) {
    if k & FB != 0 {
        out.extend_from_slice(fbm[&k]);
        return;
    }
    let mut sh = 54i32;
    while sh >= 0 {
        let x = ((k >> sh) & 63) as usize;
        if x == 0 {
            break;
        }
        out.push(DEC[x]);
        sh -= 6;
    }
}

/// Byte-wise order of two keyed IDs.
#[inline]
fn cmp_id(a: u64, b: u64, fbm: &HashMap<u64, &'static [u8]>) -> std::cmp::Ordering {
    if (a | b) & FB == 0 {
        a.cmp(&b)
    } else {
        bytes_of(a, fbm).cmp(&bytes_of(b, fbm))
    }
}

/// Merge fallback lists; None if two different byte strings share a key.
fn merge_fb<'a>(lists: impl Iterator<Item = &'a Fb>) -> Option<HashMap<u64, &'static [u8]>> {
    let mut m: HashMap<u64, &'static [u8]> = HashMap::new();
    for l in lists {
        for &(k, b) in l {
            match m.get(&k) {
                Some(&x) if x != b => return None,
                Some(_) => {}
                None => {
                    m.insert(k, b);
                }
            }
        }
    }
    Some(m)
}

type Ch = (usize, usize, usize);

/// Contiguous chunk groups of roughly equal byte size.
fn split_groups(ch: &[Ch], g: usize) -> Vec<&[Ch]> {
    let total: usize = ch.iter().map(|c| c.2 - c.1).sum();
    let per = total / g.max(1) + 1;
    let mut out = Vec::new();
    let (mut s, mut acc) = (0usize, 0usize);
    for (i, c) in ch.iter().enumerate() {
        acc += c.2 - c.1;
        if acc >= per {
            out.push(&ch[s..=i]);
            s = i + 1;
            acc = 0;
        }
    }
    if s < ch.len() {
        out.push(&ch[s..]);
    }
    out
}

#[derive(Clone, Copy)]
struct Ev {
    ekey: u64,
    ukey: u64,
    ing: i64,
    /// (ts - ws) << 33 | price << 9 | cat, or NONE when this copy is invalid
    lo: u64,
}

/// One-pass record path (SS_ONEPASS=1): each event copy leaves the parse as a 16-byte dedup
/// record in its event-id bucket, and (valid copies only) a 16-byte payload (user key, lo)
/// written ONCE into the person partition of its user key. Dedup then only marks losing
/// payloads dead in place (lo = NONE); a later pass moves just the rows whose user id maps to
/// a different person. Dedup rule (identical to the old path): per event id the copy with the
/// smallest ingest_ts wins; if the winner is invalid the event is dropped, so every valid loser
/// is killed either way. Among copies with EQUAL ingest the choice is arbitrary: at-parse batching
/// appends the dedup records of maybe-linked copies at batch flush, so append order is not parse
/// order and depends on when the link table was published. This is safe only because rules.md
/// guarantees "Copies with equal ingest_ts are identical" (same validity, user and payload).
/// Do not build anything on dd append order.
#[derive(Clone, Copy)]
#[repr(C)]
struct DRec {
    ekey: u64,
    /// ingest in whole seconds relative to ING_BASE; u32::MAX = unparseable ingest (never wins
    /// over a parseable one, as i64::MAX did)
    ing: u32,
    /// partition << 24 | index into this group's payload partition, or u32::MAX if invalid
    r: u32,
}

/// Parse output of one event group: the old 32-byte buckets, or the one-pass records.
/// One-pass rows parsed BEFORE the link table is published go to their user key's partition
/// (relocated later, only within `early[p]`); rows parsed after it go straight to their
/// person's partition: ids the bit filter clears are their own person, the rest are resolved
/// through a small prefetched batch.
struct GOut<'a> {
    onepass: bool,
    bk: Vec<Wc>,
    dd: Vec<Vec<DRec>>,
    pay: Vec<Vec<(u64, u64)>>,
    ing_base: i64,
    fail: bool,
    lcell: &'a std::sync::OnceLock<LinkTab>,
    lt: Option<&'a LinkTab>,
    /// per partition: rows [0, early[p]) were written before the link table was ready
    early: Vec<u32>,
    batch: Vec<(u64, u32, u64, u64)>,
    atparse: bool,
}
impl<'a> GOut<'a> {
    fn new(onepass: bool, cap: usize, ing_base: i64, lcell: &'a std::sync::OnceLock<LinkTab>, plain: bool) -> Self {
        let atparse = std::env::var("SS_NOATPARSE").is_err();
        if onepass {
            GOut {
                onepass,
                bk: Vec::new(),
                dd: (0..NB).map(|_| Vec::with_capacity(cap)).collect(),
                pay: (0..256).map(|_| Vec::with_capacity(cap)).collect(),
                ing_base,
                fail: false,
                lcell,
                lt: None,
                early: vec![u32::MAX; 256],
                batch: Vec::with_capacity(16),
                atparse,
            }
        } else {
            GOut {
                onepass,
                bk: (0..NB).map(|_| Wc::with_capacity(cap, plain)).collect(),
                dd: Vec::new(),
                pay: Vec::new(),
                ing_base,
                fail: false,
                lcell,
                lt: None,
                early: Vec::new(),
                batch: Vec::new(),
                atparse: false,
            }
        }
    }
    /// write one valid copy's payload (key = person key, or user key before the table is ready)
    /// and its dedup record
    #[inline(always)]
    fn put(&mut self, ekey: u64, ing: u32, key: u64, lo: u64) {
        let p = ppart_k(key);
        let v = &mut self.pay[p];
        if v.len() >= (1 << 24) - 1 {
            self.fail = true; // index must stay below the all-ones "invalid" reference
        }
        v.push((key, lo));
        let r = ((p as u32) << 24) | (v.len() as u32 - 1);
        self.dd[bucket(ekey)].push(DRec { ekey, ing, r });
    }
    fn flush_batch(&mut self, t: &LinkTab) {
        let b = std::mem::take(&mut self.batch);
        for &(ekey, ing, ukey, lo) in &b {
            let pk = t.get(ukey);
            self.put(ekey, ing, pk, lo);
        }
        self.batch = b;
        self.batch.clear();
    }
    /// end of the group: drain the batch and close the early segments
    fn finish(&mut self) {
        if !self.onepass {
            for w in self.bk.iter_mut() {
                w.finish();
            }
            return;
        }
        if let Some(t) = self.lt {
            self.flush_batch(t);
        }
        for p in 0..256 {
            if self.early[p] == u32::MAX {
                self.early[p] = self.pay[p].len() as u32;
            }
        }
    }
    #[inline(always)]
    fn push(&mut self, e: Ev) {
        if !self.onepass {
            self.bk[bucket(e.ekey)].push(e);
            return;
        }
        let ing = if e.ing == i64::MAX {
            u32::MAX
        } else {
            let d = e.ing - self.ing_base;
            if e.ing % 1000 != 0 || d < 0 || d / 1000 >= u32::MAX as i64 {
                self.fail = true;
                0
            } else {
                (d / 1000) as u32
            }
        };
        if e.lo == NONE {
            self.dd[bucket(e.ekey)].push(DRec { ekey: e.ekey, ing, r: u32::MAX });
            return;
        }
        if self.lt.is_none() && self.atparse {
            if let Some(t) = self.lcell.get() {
                self.lt = Some(t);
                for p in 0..256 {
                    self.early[p] = self.pay[p].len() as u32;
                }
            }
        }
        match self.lt {
            Some(t) if t.maybe_linked(e.ukey) => {
                t.prefetch(e.ukey);
                self.batch.push((e.ekey, ing, e.ukey, e.lo));
                if self.batch.len() == 16 {
                    self.flush_batch(t);
                }
            }
            _ => self.put(e.ekey, ing, e.ukey, e.lo),
        }
    }
}

/// Person partition of a key (256 partitions).
#[inline(always)]
fn ppart_k(k: u64) -> usize {
    (mix(k ^ 0x5bd1_e995) >> 56) as usize
}

/// One 64-byte cache line of two event records (64-byte aligned).
#[derive(Clone, Copy)]
#[repr(C, align(64))]
struct Line2([Ev; 2]);

/// Write-combining bucket for the parse scatter: records are staged in a one-line buffer that
/// stays in L1/L2 (256 buckets x 64 B per thread) and full lines are written with non-temporal
/// stores into 64-byte-aligned storage, so the 256-way scatter costs no read-for-ownership of
/// the target lines and each bucket's output is sequential. SS_WCPLAIN=1 uses ordinary stores
/// for the flush (same staging) for A/B. Readers use `as_slice` after `finish`.
struct Wc {
    lines: Vec<Line2>,
    n: usize,
    buf: Line2,
    plain: bool,
}
impl Wc {
    fn with_capacity(rows: usize, plain: bool) -> Self {
        let z = Ev { ekey: 0, ukey: 0, ing: 0, lo: 0 };
        Wc { lines: Vec::with_capacity(rows / 2 + 1), n: 0, buf: Line2([z, z]), plain }
    }
    #[inline(always)]
    fn push(&mut self, e: Ev) {
        self.buf.0[self.n & 1] = e;
        self.n += 1;
        if self.n & 1 == 0 {
            self.flush_line();
        }
    }
    #[inline(always)]
    fn flush_line(&mut self) {
        if self.lines.len() == self.lines.capacity() {
            self.lines.reserve(self.lines.capacity().max(16));
        }
        unsafe {
            let dst = self.lines.as_mut_ptr().add(self.lines.len());
            #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
            {
                if !self.plain {
                    use std::arch::x86_64::*;
                    let s = &self.buf as *const Line2 as *const __m256i;
                    let d = dst as *mut __m256i;
                    _mm256_stream_si256(d, _mm256_load_si256(s));
                    _mm256_stream_si256(d.add(1), _mm256_load_si256(s.add(1)));
                } else {
                    std::ptr::write(dst, self.buf);
                }
            }
            #[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
            std::ptr::write(dst, self.buf);
            self.lines.set_len(self.lines.len() + 1);
        }
    }
    /// write the partial last line with an ordinary store and order the streamed stores
    fn finish(&mut self) {
        if self.n & 1 == 1 {
            self.lines.push(self.buf);
        }
        #[cfg(target_arch = "x86_64")]
        unsafe {
            std::arch::x86_64::_mm_sfence()
        };
    }
    #[inline]
    fn as_slice(&self) -> &[Ev] {
        unsafe { std::slice::from_raw_parts(self.lines.as_ptr() as *const Ev, self.n) }
    }
}

#[derive(Clone, Copy)]
struct Ck {
    pkey: u64,
    /// (ts - cws) << 32 | campaign
    lo: u64,
    cid: u64,
}

/// LSD radix sort of (key, payload) pairs on the low `bits` bits of the key, 11 bits per pass.
/// Passes whose digit is the same for every record are skipped.
fn radix_sort(a: &mut Vec<(u64, u64)>, tmp: &mut Vec<(u64, u64)>, bits: u32) {
    const D: u32 = 11;
    let n = a.len();
    tmp.clear();
    tmp.resize(n, (0, 0));
    let mut shift = 0u32;
    while shift < bits {
        let mut cnt = [0usize; 1 << D];
        for x in a.iter() {
            cnt[((x.0 >> shift) & ((1 << D) - 1)) as usize] += 1;
        }
        if cnt.iter().any(|&c| c == n) {
            shift += D;
            continue;
        }
        let mut sum = 0usize;
        for c in cnt.iter_mut() {
            let t = *c;
            *c = sum;
            sum += t;
        }
        for x in a.iter() {
            let d = ((x.0 >> shift) & ((1 << D) - 1)) as usize;
            tmp[cnt[d]] = *x;
            cnt[d] += 1;
        }
        std::mem::swap(a, tmp);
        shift += D;
    }
}

/// Open-addressing id key -> person key table (linear probing, 16-byte slots, key 0 = empty:
/// no real key is 0). Separate from hashbrown so lookups can be prefetched a batch ahead.
struct LinkTab {
    mask: usize,
    slots: Vec<[u64; 2]>,
    n: usize,
    /// Bit filter over ids whose person is a DIFFERENT id (no false negatives). Ids that are their
    /// own person (unlinked, or the smallest id of their group: most rows) skip the big table.
    nf: Vec<u64>,
    fmask: usize,
}
impl LinkTab {
    fn new(n: usize) -> Self {
        let cap = (n * 2).max(16).next_power_of_two();
        LinkTab { mask: cap - 1, slots: vec![[0u64; 2]; cap], n: 0, nf: Vec::new(), fmask: 0 }
    }
    #[inline]
    fn fbit(k: u64) -> usize {
        mix(k ^ 0x6a09_e667_f3bc_c909) as usize
    }
    /// Build the non-root filter (~16 bits per non-root id) by scanning the table (test reference).
    #[cfg(test)]
    fn build_filter(&mut self) {
        if std::env::var("SS_NOFILTER").is_ok() {
            return;
        }
        let cnt = self.slots.iter().filter(|s| s[0] != 0 && s[0] != s[1]).count();
        let bits = (cnt * 16).max(1 << 12).next_power_of_two();
        self.fmask = bits - 1;
        self.nf = vec![0u64; bits / 64];
        for s in &self.slots {
            if s[0] != 0 && s[0] != s[1] {
                let b = Self::fbit(s[0]) & self.fmask;
                self.nf[b >> 6] |= 1u64 << (b & 63);
            }
        }
    }
    /// Same filter as `build_filter`, from the list of non-root ids (`cnt` of them).
    fn build_filter_from(&mut self, cnt: usize, nonroot: impl Iterator<Item = u64>) {
        if std::env::var("SS_NOFILTER").is_ok() {
            return;
        }
        let bits = (cnt * 16).max(1 << 12).next_power_of_two();
        self.fmask = bits - 1;
        self.nf = vec![0u64; bits / 64];
        for k in nonroot {
            let b = Self::fbit(k) & self.fmask;
            self.nf[b >> 6] |= 1u64 << (b & 63);
        }
    }
    #[inline]
    fn maybe_linked(&self, k: u64) -> bool {
        if self.nf.is_empty() {
            return true;
        }
        let b = Self::fbit(k) & self.fmask;
        self.nf[b >> 6] & (1u64 << (b & 63)) != 0
    }
    #[inline]
    fn home(&self, k: u64) -> usize {
        mix(k) as usize & self.mask
    }
    fn insert(&mut self, k: u64, v: u64) {
        let mut i = self.home(k);
        while self.slots[i][0] != 0 {
            if self.slots[i][0] == k {
                self.slots[i][1] = v;
                return;
            }
            i = (i + 1) & self.mask;
        }
        self.slots[i] = [k, v];
        self.n += 1;
    }
    /// Build step: dense number of id `k` (its slot value), numbering new ids in order of first
    /// appearance and recording them in `ids`.
    #[inline]
    fn intern(&mut self, k: u64, ids: &mut Vec<u64>) -> u32 {
        let mut i = self.home(k);
        loop {
            let s = self.slots[i];
            if s[0] == k {
                return s[1] as u32;
            }
            if s[0] == 0 {
                let id = ids.len() as u32;
                self.slots[i] = [k, id as u64];
                self.n += 1;
                ids.push(k);
                return id;
            }
            i = (i + 1) & self.mask;
        }
    }
    /// Person key of an id key (itself when unlinked).
    #[inline]
    fn get(&self, k: u64) -> u64 {
        let mut i = self.home(k);
        loop {
            let s = self.slots[i];
            if s[0] == k {
                return s[1];
            }
            if s[0] == 0 {
                return k;
            }
            i = (i + 1) & self.mask;
        }
    }
    #[inline]
    fn prefetch(&self, k: u64) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
            _mm_prefetch(self.slots.as_ptr().add(self.home(k)) as *const i8, _MM_HINT_T0);
        }
    }
    #[inline]
    fn prefetch_filter(&self, k: u64) {
        #[cfg(target_arch = "x86_64")]
        if !self.nf.is_empty() {
            unsafe {
                use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
                _mm_prefetch(self.nf.as_ptr().add((Self::fbit(k) & self.fmask) >> 6) as *const i8, _MM_HINT_T0);
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        let _ = k;
    }
    /// Resolve keys in place, prefetching DIST lookups ahead.
    fn resolve_all(&self, ks: &mut [u64]) {
        const DIST: usize = 16;
        // only ids the filter flags can map to another id; the rest are their own person
        let todo: Vec<u32> = (0..ks.len() as u32).filter(|&i| self.maybe_linked(ks[i as usize])).collect();
        for &i in todo.iter().take(DIST) {
            self.prefetch(ks[i as usize]);
        }
        for j in 0..todo.len() {
            if j + DIST < todo.len() {
                self.prefetch(ks[todo[j + DIST] as usize]);
            }
            let i = todo[j] as usize;
            ks[i] = self.get(ks[i]);
        }
    }
}

/// Person partitions (see `run`).
const NP: usize = 256;
#[inline]
fn ppart(k: u64) -> usize {
    (mix(k ^ 0x5bd1_e995) >> 56) as usize
}

/// Clicks of one parse group: user key -> person key in place (same result as
/// `LinkTab::resolve_all`), then scattered by person partition into exactly sized vectors,
/// in input order within each partition.
fn resolve_scatter(raw: &mut [Ck], ltab: &LinkTab) -> Vec<Vec<Ck>> {
    let cnt = resolve_count(raw, ltab);
    let mut outp: Vec<Vec<Ck>> = cnt.iter().map(|&n| Vec::with_capacity(n as usize)).collect();
    for c in raw.iter() {
        outp[ppart(c.pkey)].push(*c);
    }
    outp
}

/// First half of `resolve_scatter`: person keys in place, and how many clicks each partition gets.
/// Works in blocks so the filter words and the table slots of a block are prefetched before
/// they are read, with no per-group side arrays.
fn resolve_count(raw: &mut [Ck], ltab: &LinkTab) -> [u32; NP] {
    const B: usize = 256;
    let mut cnt = [0u32; NP];
    let mut todo = [0u16; B];
    for blk in raw.chunks_mut(B) {
        for c in blk.iter() {
            ltab.prefetch_filter(c.pkey);
        }
        let mut m = 0usize;
        for (i, c) in blk.iter().enumerate() {
            // only ids the filter flags can map to another id; the rest are their own person
            if ltab.maybe_linked(c.pkey) {
                todo[m] = i as u16;
                m += 1;
                ltab.prefetch(c.pkey);
            }
        }
        for &i in &todo[..m] {
            let c = &mut blk[i as usize];
            c.pkey = ltab.get(c.pkey);
        }
        for c in blk.iter() {
            cnt[ppart(c.pkey)] += 1;
        }
    }
    cnt
}

/// The previous resolve + scatter (SS_CKOLD=1), kept as the oracle for `resolve_scatter`.
fn resolve_scatter_old(raw: &[Ck], ltab: &LinkTab) -> Vec<Vec<Ck>> {
    let mut outp: Vec<Vec<Ck>> = (0..NP).map(|_| Vec::new()).collect();
    let mut ks: Vec<u64> = raw.iter().map(|c| c.pkey).collect();
    ltab.resolve_all(&mut ks);
    for (c, &pk) in raw.iter().zip(ks.iter()) {
        outp[ppart(pk)].push(Ck { pkey: pk, ..*c });
    }
    outp
}

/// Link table from the parsed id pairs: id key -> key of the byte-wise smallest id of its
/// connected component. One open-addressing table does both jobs: while the pairs are read it
/// maps an id to a dense number (for the union-find), then every slot value is replaced by the
/// component's root key. Roots are chosen by id order during the union, so no sort is needed.
fn build_link_table(parts: &[(Vec<(u64, u64)>, Fb)], fbm: &HashMap<u64, &'static [u8]>) -> LinkTab {
    let npairs: usize = parts.iter().map(|p| p.0.len()).sum();
    let mut table = LinkTab::new(npairs * 2);
    let mut ids: Vec<u64> = Vec::with_capacity(npairs * 2);
    let mut parent: Vec<u32> = Vec::with_capacity(npairs * 2);
    fn find(p: &mut [u32], mut x: u32) -> u32 {
        while p[x as usize] != x {
            let g = p[p[x as usize] as usize];
            p[x as usize] = g;
            x = g;
        }
        x
    }
    const DIST: usize = 8;
    for (v, _) in parts {
        for (j, &(a, b)) in v.iter().enumerate() {
            if let Some(&(pa, pb)) = v.get(j + DIST) {
                table.prefetch(pa);
                table.prefetch(pb);
            }
            let ia = table.intern(a, &mut ids);
            let ib = table.intern(b, &mut ids);
            while parent.len() < ids.len() {
                parent.push(parent.len() as u32);
            }
            let ra = find(&mut parent, ia);
            let rb = find(&mut parent, ib);
            if ra != rb {
                // the root of a merged component is the smaller id, so every root is its
                // component's smallest id
                let (ka, kb) = (ids[ra as usize], ids[rb as usize]);
                if cmp_id(ka, kb, fbm) == std::cmp::Ordering::Less {
                    parent[rb as usize] = ra
                } else {
                    parent[ra as usize] = rb
                }
            }
        }
    }
    // root key per dense id (sequential over the ids), then one sequential pass over the slots
    let roots: Vec<u64> = (0..ids.len() as u32).map(|i| ids[find(&mut parent, i) as usize]).collect();
    for s in table.slots.iter_mut() {
        if s[0] != 0 {
            s[1] = roots[s[1] as usize];
        }
    }
    // the non-root filter from the id list instead of two more scans of the whole table
    let nonroot = ids.iter().zip(roots.iter()).filter(|(a, b)| a != b).map(|(a, _)| *a);
    table.build_filter_from(nonroot.clone().count(), nonroot);
    table
}

pub fn run(data: &Path, out: &Path) -> bool {
    let t0 = std::time::Instant::now();
    let verbose = std::env::var("SS_VERBOSE").is_ok();
    if let Some(v) = std::env::var("SS_PF").ok().and_then(|v| v.parse().ok()) {
        super::scan::PREFETCH.store(v, std::sync::atomic::Ordering::Relaxed);
    }
    let lap = |what: &str| {
        if verbose {
            // cumulative process user/sys CPU seconds and minor faults at each lap
            let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
            unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
            let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
            eprintln!("{:>8.3}s  {}  [usr {:.3} sys {:.3} minflt {}]", t0.elapsed().as_secs_f64(), what, tv(ru.ru_utime), tv(ru.ru_stime), ru.ru_minflt)
        }
    };
    // SS_TIMELINE=1 (diagnostic): every 5 ms print how many threads were busy, from getrusage CPU time.
    if std::env::var("SS_TIMELINE").is_ok() {
        std::thread::spawn(move || {
            let cpu = || unsafe {
                let mut r: libc::rusage = std::mem::zeroed();
                libc::getrusage(libc::RUSAGE_SELF, &mut r);
                (r.ru_utime.tv_sec + r.ru_stime.tv_sec) as f64 + (r.ru_utime.tv_usec + r.ru_stime.tv_usec) as f64 * 1e-6
            };
            let (mut pw, mut pc) = (t0.elapsed().as_secs_f64(), cpu());
            loop {
                std::thread::sleep(std::time::Duration::from_millis(5));
                let (w, c) = (t0.elapsed().as_secs_f64(), cpu());
                eprintln!("{:>8.3}s  busy {:.1}", w, (c - pc) / (w - pw));
                pw = w;
                pc = c;
            }
        });
    }

    let man = match std::fs::read_to_string(data.join("manifest.json")) {
        Ok(m) => m,
        Err(_) => return false,
    };
    let grab = |k: &str| -> Option<i64> {
        let p = man.find(k)? + k.len();
        let rest = &man[p..];
        let q1 = rest.find('"')?;
        let rest = &rest[q1 + 1..];
        let q2 = rest.find('"')?;
        let v = parse_ts(Some(rest[..q2].as_bytes()));
        if v == BAD { None } else { Some(v) }
    };
    let (ws, we) = match (grab("\"window_start\""), grab("\"window_end\"")) {
        (Some(a), Some(b)) => (a, b),
        _ => return false,
    };
    let cws = ws - WEEK;
    if we <= ws || we - ws >= (1i64 << 31) || we - cws >= (1i64 << 32) {
        return false;
    }
    let ws_day = ws.div_euclid(DAY);
    let ndays = ((we - 1).div_euclid(DAY) - ws_day + 1).max(1) as usize;

    let products = read_small(&data.join("products.csv"));
    let campaigns = read_small(&data.join("campaigns.csv"));
    let mut cats: Vec<Vec<u8>> = products.iter().map(|r| r[1].clone()).collect();
    cats.push(b"unknown".to_vec());
    cats.sort();
    cats.dedup();
    if cats.len() > 512 {
        return false;
    }
    let cat_of = |c: &[u8]| cats.binary_search_by(|x| x.as_slice().cmp(c)).unwrap() as u32;
    let unknown = cat_of(b"unknown") as u64;
    let mut prod = Dict::new(products.len() * 2);
    for r in &products {
        prod.insert(&r[0], cat_of(&r[1]));
    }
    let mut camp_map = Dict::new(campaigns.len() * 2);
    let mut camp_names: Vec<(&[u8], &[u8])> = Vec::new();
    for r in &campaigns {
        if camp_map.get(&r[0]).is_none() {
            camp_map.insert(&r[0], camp_names.len() as u32);
            camp_names.push((&r[0], &r[1]));
        }
    }
    let organic = camp_names.len() as u32;
    let camp_tab = PackTab::new(camp_names.iter().enumerate().map(|(i, c)| (c.0, i as u32)));

    let (ev_maps, (ck_maps, lk_maps)): (Vec<Mmap>, (Vec<Mmap>, Vec<Mmap>)) = rayon::join(
        || open_all(&list_csv(&data.join("events"))),
        || rayon::join(|| open_all(&list_csv(&data.join("clicks"))), || open_all(&list_csv(&data.join("links")))),
    );
    let ev_maps: &'static [Mmap] = Vec::leak(ev_maps);
    let ck_maps: &'static [Mmap] = Vec::leak(ck_maps);
    let lk_maps: &'static [Mmap] = Vec::leak(lk_maps);
    let nthreads = rayon::current_num_threads().max(1);
    lap("opened");

    // Event chunk groups, computed up front so the prefetcher can read ahead in parse order.
    let ev_chunks: &'static [Ch] = Vec::leak(chunks(ev_maps));
    let ev_groups: Vec<&'static [Ch]> = split_groups(ev_chunks, std::env::var("SS_GROUPS").ok().and_then(|v| v.parse().ok()).unwrap_or(nthreads * 8));
    if std::env::var("SS_PB2").is_ok() {
        // Stage ablation of the real event parse path (scan::<7> + the ev_job row body), warm data.
        // Each stage adds one piece; min of 3 runs per stage.
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        let use_memo = std::env::var("SS_PB2MEMO").is_ok();
        let names = ["touch 1B/4KB", "SIMD masks only", "scan walk+fields", "+2x parse_ts", "+2x key_of", "+type/valid/price/cat", "+bucket scatter (full)"];
        for stage in 0..7 {
            let mut best = f64::MAX;
            for _ in 0..3 {
                let sink = AtomicU64::new(0);
                let t = std::time::Instant::now();
                ev_groups.par_iter().for_each(|grp| {
                    let mut acc = 0u64;
                    let bytes: usize = grp.iter().map(|c| c.2 - c.1).sum();
                    let mut bk: Vec<Vec<Ev>> = if stage == 6 { (0..NB).map(|_| Vec::with_capacity(bytes / 64 / NB + 64)).collect() } else { Vec::new() };
                    let mut fb = Fb::new();
                    let mut memo = TsMemo::default();
                    for &(mi, s0, e0) in grp.iter() {
                        let d: &'static [u8] = &ev_maps[mi][s0..e0];
                        if stage == 0 {
                            let mut i = 0;
                            while i < d.len() { acc += d[i] as u64; i += 4096; }
                            continue;
                        }
                        if stage == 1 {
                            acc = acc.wrapping_add(super::scan::masks_only(d));
                            continue;
                        }
                        scan::<7>(d, |f| {
                            if stage == 2 {
                                acc = acc.wrapping_add(f[0].map_or(0, |x| x.len()) as u64);
                                return;
                            }
                            let (ing, ts) = if use_memo { (parse_ts_memo(f[3], &mut memo), parse_ts_memo(f[2], &mut memo)) } else { (parse_ts_fast(f[3]), parse_ts_fast(f[2])) };
                            if stage == 3 {
                                acc = acc.wrapping_add(ing as u64 ^ ts as u64);
                                return;
                            }
                            let ekey = match f[0] { Some(x) => key_of(x, &mut fb), None => 0 };
                            let uk = match f[1] { Some(x) => key_of(x, &mut fb), None => 0 };
                            if stage == 4 {
                                acc = acc.wrapping_add(ing as u64 ^ ts as u64 ^ ekey ^ uk);
                                return;
                            }
                            let ing = if ing == BAD { i64::MAX } else { ing };
                            let typ = match f[4] {
                                Some(b"view") => 0u8,
                                Some(b"cart") => 1,
                                Some(b"purchase") => 2,
                                _ => 3,
                            };
                            let mut ok = typ != 3 && f[1].is_some() && f[5].is_some() && ts != BAD && ts >= ws && ts < we && ing - ts <= DAY;
                            let mut price = 0u64;
                            if ok && typ == 2 {
                                let pz = parse_price_fast(f[6]);
                                if pz < 0 { ok = false } else { price = pz as u64 }
                            }
                            let lo = if ok {
                                let cat = if typ == 2 { prod.get(f[5].unwrap()).map(|c| c as u64).unwrap_or(unknown) } else { 0 };
                                ((ts - ws) as u64) << 33 | price << 9 | cat
                            } else {
                                NONE
                            };
                            if stage == 5 {
                                acc = acc.wrapping_add(ekey ^ uk ^ lo ^ ing as u64);
                                return;
                            }
                            bk[bucket(ekey)].push(Ev { ekey, ukey: if ok { uk } else { 0 }, ing, lo });
                        });
                    }
                    acc = acc.wrapping_add(bk.iter().map(|b| b.len() as u64).sum::<u64>() + fb.len() as u64);
                    sink.fetch_add(acc, Relaxed);
                });
                best = best.min(t.elapsed().as_secs_f64());
                std::hint::black_box(sink.load(Relaxed));
            }
            eprintln!("pb2 stage {} {:<24} {:.3}s", stage, names[stage], best);
        }
        std::process::exit(0);
    }
    if std::env::var("SS_PARSEBENCH").is_ok() {
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        let sink = AtomicU64::new(0);
        if std::env::var("SS_PARSEBENCH").as_deref() == Ok("scan") {
            // Old vs new scanner alone over every event chunk, alternating, trivial closure.
            let rounds: usize = std::env::var("SS_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(7);
            let (lines, slow) = (AtomicU64::new(0), AtomicU64::new(0));
            let bytes: usize = ev_chunks.iter().map(|c| c.2 - c.1).sum();
            let names = ["scan", "scan2", "scan3", "scan3a"];
            for r in 0..rounds * 4 {
                let which = r % 4;
                sink.store(0, Relaxed);
                let t = std::time::Instant::now();
                ev_chunks.par_iter().for_each(|&(mi, s, e)| {
                    let d: &'static [u8] = &ev_maps[mi][s..e];
                    let mut acc = 0u64;
                    macro_rules! body {
                        ($g:expr) => {{
                            acc = acc.wrapping_mul(31).wrapping_add(1);
                            for i in 0..7 {
                                let x: Option<&[u8]> = $g(i);
                                acc = acc.wrapping_add(x.map_or(7, |x| x.len() as u64 ^ (x[0] as u64) << 8 ^ (x[x.len() - 1] as u64) << 16));
                            }
                        }};
                    }
                    match which {
                        0 => scan::<7>(d, |f| body!(|i: usize| f[i])),
                        1 => {
                            scan2::<7>(d, |f| body!(|i: usize| f[i]));
                        }
                        3 => {
                            scan3a::<7>(d, |f| body!(|i: usize| f[i]));
                        }
                        _ => {
                            let st = scan3::<7>(d, |f: &Line<7>| body!(|i: usize| f.get(i)));
                            if r == 2 {
                                lines.fetch_add(st.lines, Relaxed);
                                slow.fetch_add(st.slow, Relaxed);
                            }
                        }
                    }
                    sink.fetch_add(acc, Relaxed);
                });
                eprintln!("scanbench {} {:.4}s sum {}", names[which], t.elapsed().as_secs_f64(), sink.load(Relaxed));
            }
            eprintln!("scanbench bytes {} lines {} slow {}", bytes, lines.load(Relaxed), slow.load(Relaxed));
            for mode in [0u32, 1, 0, 1, 0, 1, 0, 1, 0, 1, 2] {
                let acc: [AtomicU64; 4] = Default::default();
                let t = std::time::Instant::now();
                ev_chunks.par_iter().for_each(|&(mi, s, e)| {
                    let r = super::scan::scan2_diag(&ev_maps[mi][s..e], mode);
                    for i in 0..4 { acc[i].fetch_add(r[i], Relaxed); }
                });
                eprintln!("diag mode {} {:.4}s {:?}", mode, t.elapsed().as_secs_f64(), acc.iter().map(|a| a.load(Relaxed)).collect::<Vec<_>>());
            }
            // The full event row body (as in the event job below, minus the bucket push) with each scanner.
            for r in 0..rounds * 4 {
                let which = r % 4;
                sink.store(0, Relaxed);
                let t = std::time::Instant::now();
                ev_chunks.par_iter().for_each(|&(mi, s, e)| {
                    let d: &'static [u8] = &ev_maps[mi][s..e];
                    let mut acc = 0u64;
                    let mut fb = Fb::new();
                    let mut memo = TsMemo::default();
                    macro_rules! body {
                        ($g:expr) => {{
                        let eid = match $g(0) {
                            Some(x) => x,
                            None => return,
                        };
                        let ekey = key_of(eid, &mut fb);
                        let ing = parse_ts_memo($g(3), &mut memo);
                        let ing = if ing == BAD { i64::MAX } else { ing };
                        let ts = parse_ts_memo($g(2), &mut memo);
                        let typ = match $g(4) {
                            Some(b"view") => 0u8,
                            Some(b"cart") => 1,
                            Some(b"purchase") => 2,
                            _ => 3,
                        };
                        let mut ok = typ != 3 && $g(1).is_some() && $g(5).is_some() && ts != BAD && ts >= ws && ts < we && ing - ts <= DAY;
                        let mut price = 0u64;
                        if ok && typ == 2 {
                            let p = parse_price_fast($g(6));
                            if p < 0 { ok = false } else { price = p as u64 }
                        }
                        let (ukey, lo) = if ok {
                            let cat = if typ == 2 { prod.get($g(5).unwrap()).map(|c| c as u64).unwrap_or(unknown) } else { 0 };
                            (key_of($g(1).unwrap(), &mut fb), ((ts - ws) as u64) << 33 | price << 9 | cat)
                        } else {
                            (0, NONE)
                        };
                        acc = acc.wrapping_mul(31).wrapping_add(ekey ^ ukey ^ lo ^ ing as u64);
                    }};
                    }
                    match which {
                        0 => scan::<7>(d, |f| body!(|i: usize| f[i])),
                        1 => {
                            scan2::<7>(d, |f| body!(|i: usize| f[i]));
                        }
                        3 => {
                            scan3a::<7>(d, |f| body!(|i: usize| f[i]));
                        }
                        _ => {
                            scan3::<7>(d, |f: &Line<7>| body!(|i: usize| f.get(i)));
                        }
                    }
                    sink.fetch_add(acc, Relaxed);
                });
                eprintln!("rowbench {} {:.4}s sum {}", names[which], t.elapsed().as_secs_f64(), sink.load(Relaxed));
            }
            std::process::exit(0);
        }
        for stage in 0..6 {
            let t = std::time::Instant::now();
            ev_chunks.par_iter().for_each(|&(mi, s, e)| {
                let d: &'static [u8] = &ev_maps[mi][s..e];
                let mut acc = 0u64;
                if stage == 0 {
                    let mut i = 0;
                    while i < d.len() { acc += d[i] as u64; i += 4096; }
                } else {
                    let mut fb = Fb::new();
                    for_lines(d, |l| {
                        if stage == 1 { acc += l.len() as u64; return; }
                        let f = match fields::<7>(l) { Some(f) => f, None => return };
                        if stage == 2 { acc += f[0].map_or(0, |x| x.len()) as u64; return; }
                        let a = parse_ts(f[2]); let b = parse_ts(f[3]);
                        acc = acc.wrapping_add(a as u64 ^ b as u64);
                        if stage == 3 { return; }
                        acc = acc.wrapping_add(key_of(f[0].unwrap_or(b"x"), &mut fb) ^ key_of(f[1].unwrap_or(b"x"), &mut fb));
                        if stage == 4 { return; }
                        acc += prod.get(f[5].unwrap_or(b"x")).unwrap_or(0) as u64;
                    });
                }
                sink.fetch_add(acc, Relaxed);
            });
            eprintln!("stage {} {:.3}s", stage, t.elapsed().as_secs_f64());
        }
        eprintln!("{}", sink.load(Relaxed));
        std::process::exit(0);
    }
    // SS_ROWBENCH=1 (diagnostic): cumulative cost of the event row body over the real groups
    // with the real scanner, plus CPU time and page faults per stage.
    if std::env::var("SS_ROWBENCH").is_ok() {
        if std::env::var("SS_NOTHP").is_ok() { unsafe { libc::prctl(libc::PR_SET_THP_DISABLE, 1, 0, 0, 0); } }
        let only: Option<Vec<usize>> = std::env::var("SS_RB_STAGES").ok().map(|v| v.split(',').filter_map(|x| x.parse().ok()).collect());
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        let ru = || unsafe {
            let mut r: libc::rusage = std::mem::zeroed();
            libc::getrusage(libc::RUSAGE_SELF, &mut r);
            (
                r.ru_utime.tv_sec as f64 + r.ru_utime.tv_usec as f64 * 1e-6,
                r.ru_stime.tv_sec as f64 + r.ru_stime.tv_usec as f64 * 1e-6,
                r.ru_minflt as i64,
            )
        };
        let names = ["touch", "scan", "+2 ts", "+ekey", "+type/valid/price/cat/ukey", "+push (full)", "+push (full)"];
        for rep in 0..2 {
            for stage in 0..7usize {
                if let Some(o) = &only { if !o.contains(&stage) { continue; } }
                let sink = AtomicU64::new(0);
                let rows = AtomicU64::new(0);
                let (u0, s0, f0) = ru();
                let t = std::time::Instant::now();
                ev_groups.par_iter().for_each(|grp| {
                    let bytes: usize = grp.iter().map(|c| c.2 - c.1).sum();
                    let cap = bytes / 64 / NB + 64;
                    let mut bk: Vec<Vec<Ev>> = if stage >= 5 { (0..NB).map(|_| Vec::with_capacity(cap)).collect() } else { Vec::new() };
                    let mut fb = Fb::new();
                    let (mut acc, mut n) = (0u64, 0u64);
                    for &(mi, s, e) in grp.iter() {
                        let d: &'static [u8] = &ev_maps[mi][s..e];
                        if stage == 0 {
                            let mut i = 0;
                            while i < d.len() { acc += d[i] as u64; i += 4096; }
                            continue;
                        }
                        scan::<7>(d, |f| {
                            n += 1;
                            let eid = match f[0] { Some(x) => x, None => return };
                            if stage == 1 { acc += eid.len() as u64; return; }
                            let ing = parse_ts_fast(f[3]);
                            let ing = if ing == BAD { i64::MAX } else { ing };
                            let ts = parse_ts_fast(f[2]);
                            if stage == 2 { acc = acc.wrapping_add(ing as u64 ^ ts as u64); return; }
                            let ekey = key_of(eid, &mut fb);
                            if stage == 3 { acc = acc.wrapping_add(ing as u64 ^ ts as u64 ^ ekey); return; }
                            let typ = match f[4] { Some(b"view") => 0u8, Some(b"cart") => 1, Some(b"purchase") => 2, _ => 3 };
                            let mut ok = typ != 3 && f[1].is_some() && f[5].is_some() && ts != BAD && ts >= ws && ts < we && ing - ts <= DAY;
                            let mut price = 0u64;
                            if ok && typ == 2 {
                                let p = parse_price_fast(f[6]);
                                if p < 0 { ok = false } else { price = p as u64 }
                            }
                            let (ukey, lo) = if ok {
                                let cat = if typ == 2 { prod.get(f[5].unwrap()).map(|c| c as u64).unwrap_or(unknown) } else { 0 };
                                (key_of(f[1].unwrap(), &mut fb), ((ts - ws) as u64) << 33 | price << 9 | cat)
                            } else {
                                (0, NONE)
                            };
                            if stage == 4 { acc = acc.wrapping_add(ing as u64 ^ ekey ^ ukey ^ lo); return; }
                            bk[bucket(ekey)].push(Ev { ekey, ukey, ing, lo });
                        });
                    }
                    if stage == 6 { std::mem::forget(bk); }
                    sink.fetch_add(acc, Relaxed);
                    rows.fetch_add(n, Relaxed);
                });
                let w = t.elapsed().as_secs_f64();
                let (u1, s1, f1) = ru();
                let n = rows.load(Relaxed).max(1) as f64;
                eprintln!(
                    "rep {} stage {} {:<28} wall {:.3}s  user {:.3}s sys {:.3}s  minflt {:>7}  cpu-ns/row {:>6.1}  wall*thr-ns/row {:>6.1}  [{}]",
                    rep, stage, names[stage], w, u1 - u0, s1 - s0, f1 - f0, (u1 - u0 + s1 - s0) * 1e9 / n, w * nthreads as f64 * 1e9 / n, sink.load(Relaxed) & 0xffff
                );
            }
        }
        std::process::exit(0);
    }
    // Cold-cache I/O overlap: background threads ask the kernel for every input window
    // (links, then events interleaved across parse groups, then clicks) so reads run ahead of parsing.
    if std::env::var("SS_NOPREFETCH").is_err() {
        let mut wins: Vec<(&'static Mmap, usize, usize)> = Vec::new();
        const W: usize = 4 << 20;
        let whole = |maps: &'static [Mmap], wins: &mut Vec<(&'static Mmap, usize, usize)>| {
            for m in maps {
                let mut s = 0;
                while s < m.len() {
                    let e = (s + W).min(m.len());
                    wins.push((m, s, e));
                    s = e;
                }
            }
        };
        whole(lk_maps, &mut wins);
        let rounds = ev_groups.iter().map(|g| g.len()).max().unwrap_or(0);
        for r in 0..rounds {
            for g in &ev_groups {
                if let Some(&(mi, s, e)) = g.get(r) {
                    wins.push((&ev_maps[mi], s, e));
                }
            }
        }
        whole(ck_maps, &mut wins);
        let wins: &'static [(&'static Mmap, usize, usize)] = Vec::leak(wins);
        let mut mode = std::env::var("SS_IO").unwrap_or_else(|_| "auto".into());
        if mode == "auto" {
            // Sample the first page of every window with mincore(): if the input is already in the
            // page cache, prefetch threads only cost CPU; if it is cold, readers that populate
            // windows ahead of the parsers win.
            let (mut hit, mut n) = (0usize, 0usize);
            for &(m, s, _) in wins.iter() {
                let mut v = [0u8; 1];
                let p = unsafe { m.as_ptr().add(s & !4095) } as *mut libc::c_void;
                if unsafe { libc::mincore(p, 1, v.as_mut_ptr()) } == 0 {
                    n += 1;
                    hit += (v[0] & 1) as usize;
                }
            }
            let frac = if n == 0 { 1.0 } else { hit as f64 / n as f64 };
            mode = if frac >= 0.8 { "none".into() } else { "populate".into() };
            if verbose {
                eprintln!("input resident {:.0}% -> io mode {}", frac * 100.0, mode);
            }
        }
        let lanes: usize = std::env::var("SS_IOT").ok().and_then(|v| v.parse().ok()).unwrap_or(if mode == "advise" { 4 } else if mode == "touch" { 16 } else { 8 });
        if mode == "populate" {
            // MADV_POPULATE_READ (Linux 5.14+): read AND map each window, so parsers never fault.
            // Falls back to WILLNEED where unsupported.
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            for _ in 0..lanes {
                std::thread::spawn(move || loop {
                    let i = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if i >= wins.len() {
                        break;
                    }
                    let (m, s, e) = wins[i];
                    let s0 = s & !4095;
                    if m.advise_range(memmap2::Advice::PopulateRead, s0, e - s0).is_err() {
                        let _ = m.advise_range(memmap2::Advice::WillNeed, s0, e - s0);
                    }
                });
            }
        } else if mode == "touch" {
            // Real reads in parse order: each lane faults in whole windows (one load per page),
            // so `lanes` windows are always in flight; parsers then hit the page cache.
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            for &(m, _, _) in wins.iter() {
                let _ = m.advise(memmap2::Advice::Sequential);
            }
            for _ in 0..lanes {
                std::thread::spawn(move || loop {
                    let i = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if i >= wins.len() {
                        break;
                    }
                    let (m, s, e) = wins[i];
                    let p = m.as_ptr();
                    let mut o = s & !4095;
                    let mut acc = 0u8;
                    while o < e {
                        acc ^= unsafe { std::ptr::read_volatile(p.add(o)) };
                        o += 4096;
                    }
                    std::hint::black_box(acc);
                });
            }
        } else if mode == "advise" {
            for lane in 0..lanes {
                std::thread::spawn(move || {
                    for (i, &(m, s, e)) in wins.iter().enumerate() {
                        if i % lanes == lane {
                            let s0 = s & !4095;
                            let _ = m.advise_range(memmap2::Advice::WillNeed, s0, e - s0);
                        }
                    }
                });
            }
        }
    }

    // ---------------- links -> table: id key -> person key (root = byte-wise smallest id)
    // Links are small: parse them first on all threads, so the serial tail below (sort, index,
    // union-find, table) runs on one thread WHILE the events parse on the others, instead of
// the link chunks queueing behind the big event groups and the tail running alone after.
    let lk_chunks = chunks(lk_maps);
    let parts: Vec<(Vec<(u64, u64)>, Fb)> = lk_chunks
        .par_iter()
        .map(|&(mi, s, e)| {
            let d: &'static [u8] = &lk_maps[mi][s..e];
            let mut v = Vec::new();
            let mut fb = Fb::new();
            scan3a::<2>(d, |f| {
                if let [Some(a), Some(b)] = *f {
                    v.push((key_of(a, &mut fb), key_of(b, &mut fb)));
                }
            });
            (v, fb)
        })
            .collect();
    if verbose { eprintln!("  link {} {:.3}s", "parsed", t0.elapsed().as_secs_f64()); }
    let lcell: std::sync::OnceLock<LinkTab> = std::sync::OnceLock::new();
    let lref = &lcell;
    let link_job = move || -> Option<Fb> {
        let fbm = merge_fb(parts.iter().map(|p| &p.1))?;
        let table = build_link_table(&parts, &fbm);
        if verbose { eprintln!("  link {} {:.3}s", "table", t0.elapsed().as_secs_f64()); }
        let fb: Fb = parts.into_iter().flat_map(|p| p.1).collect();
        // publish: one-pass parse workers resolve persons directly from here on
        let _ = lref.set(table);
        Some(fb)
    };
    let onepass = std::env::var("SS_ONEPASS").is_ok();
    // ingest seconds are stored relative to ~68 years before the window (u32 range)
    let ing_base = ws - (1i64 << 31) * 1000;

    // ---------------- events: parse every copy, scatter by event_id bucket
    let wc_plain = std::env::var("SS_WCPLAIN").is_ok();
    let ev_job = || -> Vec<(GOut, Fb)> {
        let r: Vec<(GOut, Fb)> = ev_groups
            .par_iter()
            .map(|grp| {
                let bytes: usize = grp.iter().map(|c| c.2 - c.1).sum();
                // rows per bucket from the chunk bytes (event lines are >= ~68 bytes; a short
                // estimate only means one regrowth of that bucket)
                let cap = bytes / 68 / NB + 32;
                let mut bk = GOut::new(onepass, cap, ing_base, &lcell, wc_plain);
                let mut fb = Fb::new();
                let mut memo = TsMemo::default();
                for &(mi, s, e) in grp.iter() {
                    let d: &'static [u8] = &ev_maps[mi][s..e];
                    scan3a::<7>(d, |f| {
                        let eid = match f[0] {
                            Some(x) => x,
                            None => return,
                        };
                        let ekey = key_of(eid, &mut fb);
                        let ing = parse_ts_memo(f[3], &mut memo);
                        let ing = if ing == BAD { i64::MAX } else { ing };
                        let ts = parse_ts_memo(f[2], &mut memo);
                        let typ = match f[4] {
                            Some(b"view") => 0u8,
                            Some(b"cart") => 1,
                            Some(b"purchase") => 2,
                            _ => 3,
                        };
                        let mut ok = typ != 3 && f[1].is_some() && f[5].is_some() && ts != BAD && ts >= ws && ts < we && ing - ts <= DAY;
                        let mut price = 0u64;
                        if ok && typ == 2 {
                            let p = parse_price_fast(f[6]);
                            if p < 0 { ok = false } else { price = p as u64 }
                        }
                        let (ukey, lo) = if ok {
                            // Category only matters for purchases (price > 0); skip the product
                            // hash lookup for views and carts (~97% of valid rows).
                            let cat = if typ == 2 { prod.get(f[5].unwrap()).map(|c| c as u64).unwrap_or(unknown) } else { 0 };
                            (key_of(f[1].unwrap(), &mut fb), ((ts - ws) as u64) << 33 | price << 9 | cat)
                        } else {
                            (0, NONE)
                        };
                        bk.push(Ev { ekey, ukey, ing, lo });
                    });
                }
                bk.finish();
                (bk, fb)
            })
            .collect();
        if verbose { eprintln!("  events done {:.3}s", t0.elapsed().as_secs_f64()); }
        r
    };

    // Clicks don't need the link table until the person lookup, so their parse runs here,
    // beside the events (SS_CKLATE=1 restores the old order: parse after links+events).
    let ck_parse = || -> Vec<(Vec<Ck>, Fb)> {
        let ch = chunks(ck_maps);
        let groups = split_groups(&ch, nthreads * 2);
        groups
            .par_iter()
            .map(|grp| {
                // Reserved once (untouched pages cost nothing) instead of growing by doubling.
                let mut raw: Vec<Ck> = Vec::with_capacity(grp.iter().map(|c| c.2 - c.1).sum::<usize>() / CK_LINE_EST);
                let mut fb = Fb::new();
                let mut memo = TsMemo::default();
                for &(mi, s, e) in grp.iter() {
                    let d: &'static [u8] = &ck_maps[mi][s..e];
                    scan3a::<4>(d, |f| {
                        let (cid, uid) = match (f[0], f[1]) {
                            (Some(c), Some(u)) => (c, u),
                            _ => return,
                        };
                        let ts = parse_ts_memo(f[3], &mut memo);
                        if ts == BAD || ts < cws || ts >= we {
                            return;
                        }
                        let camp = match f[2].and_then(|c| camp_tab.get(c, &camp_map)) {
                            Some(c) => c,
                            None => return,
                        };
                        // pkey holds the user key until the batched person lookup
                        raw.push(Ck { pkey: key_of(uid, &mut fb), lo: ((ts - cws) as u64) << 32 | camp as u64, cid: key_of(cid, &mut fb) });
                    });
                }
                (raw, fb)
            })
            .collect()
    };
    let ck_early = std::env::var("SS_CKLATE").is_err();
    let (links, (evp, ck_raw)) = rayon::join(link_job, || rayon::join(ev_job, || if ck_early { Some(ck_parse()) } else { None }));
    let lfb = match links {
        Some(x) => x,
        None => return false,
    };
    let ltab: &LinkTab = match lcell.get() {
        Some(t) => t,
        None => return false,
    };
    lap(&format!("links ({} ids) and events parsed", ltab.n));
    if std::env::var("SS_CKBENCH").is_ok() {
        // Diagnostic: cumulative cost of the click path over the real groups (fresh state per run).
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        let cpu = || {
            let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
            unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
            let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
            (tv(ru.ru_utime), tv(ru.ru_stime))
        };
        let ch = chunks(ck_maps);
        let groups = split_groups(&ch, nthreads * 2);
        let flagged = AtomicU64::new(0);
        let names = ["scan", "+ts/window", "+campaign", "+2 keys", "+push (parse as shipped)", "+resolve+scatter (old)", "+resolve_scatter (new, instead)", "+resolve_count only (new)", "whole click path, new"];
        let rounds: usize = std::env::var("SS_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(5);
        for stage in 0..names.len() {
            let mut best = (f64::MAX, f64::MAX, f64::MAX);
            let (mut rows, mut last_sum) = (0u64, 0u64);
            for _ in 0..rounds {
                let (sink, cnt) = (AtomicU64::new(0), AtomicU64::new(0));
                let (c0, t) = (cpu(), std::time::Instant::now());
                groups.par_iter().for_each(|grp| {
                    let mut raw: Vec<Ck> = Vec::new();
                    if stage == 8 {
                        raw.reserve(grp.iter().map(|c| c.2 - c.1).sum::<usize>() / CK_LINE_EST);
                    }
                    let mut fb = Fb::new();
                    let mut memo = TsMemo::default();
                    let (mut acc, mut n) = (0u64, 0u64);
                    for &(mi, s, e) in grp.iter() {
                        let d: &'static [u8] = &ck_maps[mi][s..e];
                        scan3a::<4>(d, |f| {
                            let (cid, uid) = match (f[0], f[1]) {
                                (Some(c), Some(u)) => (c, u),
                                _ => return,
                            };
                            if stage == 0 {
                                acc += (cid.len() + uid.len()) as u64;
                                return;
                            }
                            let ts = parse_ts_memo(f[3], &mut memo);
                            if ts == BAD || ts < cws || ts >= we {
                                return;
                            }
                            if stage == 1 {
                                acc = acc.wrapping_add(ts as u64);
                                return;
                            }
                            let camp = match f[2].and_then(|c| if stage == 8 { camp_tab.get(c, &camp_map) } else { camp_map.get(c) }) {
                                Some(c) => c,
                                None => return,
                            };
                            if stage == 2 {
                                acc = acc.wrapping_add(ts as u64 ^ camp as u64);
                                return;
                            }
                            let c = Ck { pkey: key_of(uid, &mut fb), lo: ((ts - cws) as u64) << 32 | camp as u64, cid: key_of(cid, &mut fb) };
                            n += 1;
                            if stage == 3 {
                                acc = acc.wrapping_add(c.pkey ^ c.lo ^ c.cid);
                                return;
                            }
                            raw.push(c);
                        });
                    }
                    if stage == 7 {
                        flagged.fetch_add(raw.iter().filter(|c| ltab.maybe_linked(c.pkey)).count() as u64, Relaxed);
                        let cnt = resolve_count(&mut raw, &ltab);
                        acc = acc.wrapping_add(cnt.iter().map(|&x| x as u64).sum::<u64>() );
                    } else if stage == 6 || stage == 8 {
                        let outp = resolve_scatter(&mut raw, &ltab);
                        acc = acc.wrapping_add(outp.iter().map(|v| v.len() as u64 + v.last().map_or(0, |c| c.pkey)).sum::<u64>());
                    } else if stage == 5 {
                        let mut outp: Vec<Vec<Ck>> = (0..NP).map(|_| Vec::new()).collect();
                        let mut ks: Vec<u64> = raw.iter().map(|c| c.pkey).collect();
                        ltab.resolve_all(&mut ks);
                        for (c, &pk) in raw.iter().zip(ks.iter()) {
                            outp[ppart(pk)].push(Ck { pkey: pk, ..*c });
                        }
                        acc = acc.wrapping_add(outp.iter().map(|v| v.len() as u64 + v.last().map_or(0, |c| c.pkey)).sum::<u64>());
                    }
                    acc = acc.wrapping_add(raw.len() as u64);
                    sink.fetch_add(acc, Relaxed);
                    cnt.fetch_add(n, Relaxed);
                });
                let (w, c1) = (t.elapsed().as_secs_f64(), cpu());
                rows = rows.max(cnt.load(Relaxed));
                last_sum = sink.load(Relaxed);
                if c1.0 - c0.0 + c1.1 - c0.1 < best.1 + best.2 {
                    best = (w, c1.0 - c0.0, c1.1 - c0.1);
                }
            }
            eprintln!("ckbench stage {} {:<32} wall {:.4}s user {:.4}s sys {:.4}s rows {} sum {}", stage, names[stage], best.0, best.1, best.2, rows, last_sum);
        }
        std::process::exit(0);
    }
    // The event and link files (and the clicks, when parsed early) are fully parsed. Drop their
    // page-table entries now, on a spare thread, instead of leaving ~1.9 GB of 4 KB file mappings
    // to be torn down serially at exit (~18 ms at 20M, inside the measured wall time).
    // MADV_DONTNEED on a read-only file mapping only unmaps: fallback IDs that still point into
    // a file fault back in from the page cache. SS_EARLYUNMAP=0 disables, =sync waits for it.
    let eu = std::env::var("SS_EARLYUNMAP").unwrap_or_default();
    if eu != "0" {
        let with_clicks = ck_raw.is_some();
        let h = std::thread::spawn(move || {
            let rss_file = || {
                let st = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
                st.lines().find(|l| l.starts_with("RssFile")).unwrap_or("").to_string()
            };
            let before = if verbose { rss_file() } else { String::new() };
            let t = std::time::Instant::now();
            let cks: &[Mmap] = if with_clicks { ck_maps } else { &[] };
            for m in ev_maps.iter().chain(lk_maps.iter()).chain(cks.iter()) {
                unsafe { libc::madvise(m.as_ptr() as *mut libc::c_void, m.len(), libc::MADV_DONTNEED) };
            }
            if verbose {
                eprintln!("  inputs unmapped in {:.4}s ({} -> {})", t.elapsed().as_secs_f64(), before, rss_file());
            }
        });
        if eu == "sync" {
            let _ = h.join();
        }
    }

    // Person partitions: survivors and clicks are scattered by hash(person key) so each
    // partition is sorted on its own, in cache, instead of one global sort.
    // ---------------- dedup per bucket (smallest ingest wins; kept-but-invalid drops the event)
    let dedup_job = || -> Vec<Vec<Vec<(u64, u64)>>> {
        let ng = (nthreads * 4).min(NB);
        (0..ng)
            .into_par_iter()
            .map(|gi| {
                let mut outp: Vec<Vec<(u64, u64)>> = (0..NP).map(|_| Vec::new()).collect();
                let mut t: hashbrown::HashTable<Ev> = hashbrown::HashTable::new();
                let (mut ks, mut los): (Vec<u64>, Vec<u64>) = (Vec::new(), Vec::new());
                const FBITS: usize = 1 << 20;
                const FMASK: usize = FBITS - 1;
                let mut seen = vec![0u64; FBITS / 64];
                let mut dup = vec![0u64; FBITS / 64];
                for b in (gi * NB / ng)..((gi + 1) * NB / ng) {
                    // Prepass: a bit filter over event-id hashes finds ids that may occur more
                    // than once (2-4% real repeats plus filter collisions). Only those copies go
                    // through the exact dedup table; every other copy is its event's only copy.
                    seen.fill(0);
                    dup.fill(0);
                    for g in &evp {
                        for e in g.0.bk[b].as_slice() {
                            let h = th(e.ekey) as usize & FMASK;
                            let (w, m) = (h >> 6, 1u64 << (h & 63));
                            dup[w] |= seen[w] & m;
                            seen[w] |= m;
                        }
                    }
                    ks.clear();
                    los.clear();
                    t.clear();
                    for g in &evp {
                        for e in g.0.bk[b].as_slice() {
                            let h = th(e.ekey);
                            let f = h as usize & FMASK;
                            if dup[f >> 6] & (1u64 << (f & 63)) == 0 {
                                if e.lo != NONE {
                                    ks.push(e.ukey);
                                    los.push(e.lo);
                                }
                                continue;
                            }
                            match t.find_mut(h, |x| x.ekey == e.ekey) {
                                Some(x) => {
                                    if e.ing < x.ing {
                                        *x = *e;
                                    }
                                }
                                None => {
                                    t.insert_unique(h, *e, |x| th(x.ekey));
                                }
                            }
                        }
                    }
                    // survivors: resolve persons in a batch with prefetch, then scatter
                    for e in t.iter() {
                        if e.lo != NONE {
                            ks.push(e.ukey);
                            los.push(e.lo);
                        }
                    }
                    ltab.resolve_all(&mut ks);
                    for (&pk, &lo) in ks.iter().zip(los.iter()) {
                        outp[ppart(pk)].push((pk, lo));
                    }
                }
                outp
            })
            .collect()
    };

    // ---------------- clicks (no dedup needed: copies are identical), scattered by person
    // SS_CKOLD=1 keeps the previous resolve + scatter (A/B and differential switch).
    let old_ck = std::env::var("SS_CKOLD").is_ok();
    let ck_job = || -> (Vec<Vec<Vec<Ck>>>, Fb) {
        let raws: Vec<(Vec<Ck>, Fb)> = match ck_raw {
            Some(r) => r,
            None => ck_parse(),
        };
        let parts: Vec<(Vec<Vec<Ck>>, Fb)> = raws
            .into_par_iter()
            .map(|(mut raw, fb)| {
                let outp = if old_ck { resolve_scatter_old(&raw, &ltab) } else { resolve_scatter(&mut raw, &ltab) };
                (outp, fb)
            })
            .collect();
        let mut fb = Fb::new();
        let mut cks = Vec::with_capacity(parts.len());
        for (v, f) in parts {
            cks.push(v);
            fb.extend(f);
        }
        (cks, fb)
    };

    let (eparts, cparts, fbm) = if !onepass {
        let (eparts, (cparts, cfb)) = rayon::join(dedup_job, ck_job);
        lap("dedup + clicks");
        let fbm = match merge_fb(evp.iter().map(|g| &g.1).chain(std::iter::once(&lfb)).chain(std::iter::once(&cfb))) {
            Some(m) => m,
            None => return false,
        };
        drop(evp);
        (eparts, cparts, fbm)
    } else {
        if evp.iter().any(|g| g.0.fail) {
            eprintln!("note: one-pass record layout declined (ingest not whole seconds/out of range, or a partition over 2^24 rows)");
            return false; // a record did not fit the packed layout: byte-exact legacy path
        }
        let mut evp = evp;
        // (a) mark dedup losers dead in place. Workers own disjoint event-id buckets, so the
        // payload slots they write (one per losing copy) never overlap.
        let pptr: Vec<Vec<usize>> = evp.iter_mut().map(|g| g.0.pay.iter_mut().map(|v| v.as_mut_ptr() as usize).collect()).collect();
        let mark_job = || {
            let ng = (nthreads * 4).min(NB);
            (0..ng).into_par_iter().for_each(|gi| {
                const FBITS: usize = 1 << 20;
                const FMASK: usize = FBITS - 1;
                let mut seen = vec![0u64; FBITS / 64];
                let mut dup = vec![0u64; FBITS / 64];
                // candidates: (ekey, group, position in that group's bucket)
                let mut cand: Vec<(u64, u32, u32)> = Vec::new();
                // winner per event id: (ekey, ing, group, position)
                let mut t: hashbrown::HashTable<(u64, u32, u32, u32)> = hashbrown::HashTable::new();
                for b in (gi * NB / ng)..((gi + 1) * NB / ng) {
                    seen.fill(0);
                    dup.fill(0);
                    for g in &evp {
                        for d in &g.0.dd[b] {
                            let h = th(d.ekey) as usize & FMASK;
                            let (w, m) = (h >> 6, 1u64 << (h & 63));
                            dup[w] |= seen[w] & m;
                            seen[w] |= m;
                        }
                    }
                    cand.clear();
                    t.clear();
                    for (gix, g) in evp.iter().enumerate() {
                        for (pos, d) in g.0.dd[b].iter().enumerate() {
                            let h = th(d.ekey);
                            let f = h as usize & FMASK;
                            if dup[f >> 6] & (1u64 << (f & 63)) == 0 {
                                continue; // the only copy of its event: already in place
                            }
                            cand.push((d.ekey, gix as u32, pos as u32));
                            match t.find_mut(h, |x| x.0 == d.ekey) {
                                Some(x) => {
                                    if d.ing < x.1 {
                                        *x = (d.ekey, d.ing, gix as u32, pos as u32);
                                    }
                                }
                                None => {
                                    t.insert_unique(h, (d.ekey, d.ing, gix as u32, pos as u32), |x| th(x.0));
                                }
                            }
                        }
                    }
                    for &(ek, gix, pos) in &cand {
                        let w = t.find(th(ek), |x| x.0 == ek).unwrap();
                        if w.2 == gix && w.3 == pos {
                            continue;
                        }
                        let r = evp[gix as usize].0.dd[b][pos as usize].r;
                        if r != u32::MAX {
                            let (p, i) = ((r >> 24) as usize, (r & 0xFF_FFFF) as usize);
                            unsafe { (*((pptr[gix as usize][p] as *mut (u64, u64)).add(i))).1 = NONE };
                        }
                    }
                }
            });
        };
        let ((), (cparts, cfb)) = rayon::join(mark_job, ck_job);
        // (b) rows whose user id maps to a different person: rewrite the key in place, or move
        // the row to its person's partition (the old slot is marked dead)
        let compact = std::env::var("SS_NOCOMPACT").is_err();
        let relocs: Vec<Vec<Vec<(u64, u64)>>> = evp
            .par_iter_mut()
            .map(|g| {
                let mut out: Vec<Vec<(u64, u64)>> = (0..NP).map(|_| Vec::new()).collect();
                let mut idx: Vec<u32> = Vec::new();
                let mut ks: Vec<u64> = Vec::new();
                let (pays, early) = (&mut g.0.pay, &g.0.early);
                for (p, v) in pays.iter_mut().enumerate() {
                    idx.clear();
                    ks.clear();
                    let lim = (early[p] as usize).min(v.len());
                    for (i, row) in v[..lim].iter().enumerate() {
                        if row.1 != NONE && ltab.maybe_linked(row.0) {
                            idx.push(i as u32);
                            ks.push(row.0);
                        }
                    }
                    ltab.resolve_all(&mut ks);
                    for (&i, &pk) in idx.iter().zip(ks.iter()) {
                        let row = &mut v[i as usize];
                        if pk == row.0 {
                            continue;
                        }
                        let q = ppart(pk);
                        if q == p {
                            row.0 = pk;
                        } else {
                            out[q].push((pk, row.1));
                            row.1 = NONE;
                        }
                    }
                    // compact while the partition is still in cache: the person stage then never
                    // walks dedup losers or moved-out slots (SS_NOCOMPACT=1 keeps them)
                    if compact {
                        v.retain(|r| r.1 != NONE);
                    }
                }
                out
            })
            .collect();
        let mut eparts: Vec<Vec<Vec<(u64, u64)>>> = Vec::with_capacity(evp.len() + relocs.len());
        let mut fbs: Vec<Fb> = Vec::with_capacity(evp.len());
        for (g, fb) in evp {
            eparts.push(g.pay);
            fbs.push(fb);
        }
        eparts.extend(relocs);
        let evp: Vec<((), Fb)> = fbs.into_iter().map(|f| ((), f)).collect();
        let fbm = match merge_fb(evp.iter().map(|g| &g.1).chain(std::iter::once(&lfb)).chain(std::iter::once(&cfb))) {
            Some(m) => m,
            None => return false,
        };
        lap("dedup marked + relocated (one-pass)");
        (eparts, cparts, fbm)
    };

    let ncamp = organic as usize + 1;
    let ncats = cats.len();

    struct UserRow {
        key: u64,
        day: i64,
        sessions: i64,
        events: i64,
        revenue: i64,
        top: Option<u16>,
    }
    /// Output rows of one worker, encoded straight into the six Parquet column buffers of its
    /// user_daily file (the 48-byte rows are never stored). `push` is the only way in.
    struct UserOut<'a> {
        c: [super::pq::Col; 6],
        n: usize,
        fbm: &'a HashMap<u64, &'static [u8]>,
        cats: &'a [Vec<u8>],
    }
    impl<'a> UserOut<'a> {
        fn new(fbm: &'a HashMap<u64, &'static [u8]>, cats: &'a [Vec<u8>], est_rows: usize) -> Self {
            use super::pq::{Col, Kind};
            UserOut {
                c: [
                    Col::new("user_id", Kind::Str, est_rows),
                    Col::new("day", Kind::Date, est_rows),
                    Col::new("sessions", Kind::I32, est_rows),
                    Col::new("events", Kind::I32, est_rows),
                    Col::new("revenue_cents", Kind::I64, est_rows),
                    Col::new("top_category", Kind::OptStr, est_rows),
                ],
                n: 0,
                fbm,
                cats,
            }
        }
        #[inline]
        fn push(&mut self, r: UserRow) {
            let fbm = self.fbm;
            self.c[0].bytes_with(|o| id_into(r.key, fbm, o));
            self.c[1].i32(r.day as i32);
            self.c[2].i32(r.sessions as i32);
            self.c[3].i32(r.events as i32);
            self.c[4].i64(r.revenue);
            match r.top {
                Some(t) => self.c[5].bytes(&self.cats[t as usize]),
                None => self.c[5].null(),
            }
            self.n += 1;
        }
    }
    struct Acc {
        camp: Vec<[i64; 4]>,
        cnt: Vec<u32>,
        active: Vec<u32>,
        catsum: Vec<i64>,
        touched: Vec<u16>,
    }
    let do_person = |key: u64, evr: &[(u64, u64)], ckr: &[Ck], acc: &mut Acc, rows: &mut UserOut| {
        for &a in &acc.active {
            acc.cnt[a as usize] = 0;
        }
        acc.active.clear();
        let (mut cl, mut cr) = (0usize, 0usize);
        let mut cur_day = i64::MIN;
        let mut row: Option<UserRow> = None;
        let mut k = 0usize;
        let j = evr.len();
        while k < j {
            let start = (evr[k].1 >> 33) as i64 + ws;
            let mut prev = start;
            let mut m = k + 1;
            while m < j {
                let t = (evr[m].1 >> 33) as i64 + ws;
                if t - prev > GAP {
                    break;
                }
                prev = t;
                m += 1;
            }
            let day = start.div_euclid(DAY);
            let di = (day - ws_day) as usize;
            if day != cur_day {
                if let Some(mut r) = row.take() {
                    r.top = take_top(&mut acc.catsum, &mut acc.touched);
                    rows.push(r);
                }
                cur_day = day;
                row = Some(UserRow { key, day, sessions: 0, events: 0, revenue: 0, top: None });
            }
            let mut rev = 0i64;
            let mut purch = false;
            for q in k..m {
                let lo = evr[q].1;
                let price = ((lo >> 9) & 0xFF_FFFF) as i64;
                if price > 0 {
                    let cat = (lo & 511) as usize;
                    rev += price;
                    purch = true;
                    if acc.catsum[cat] == 0 {
                        acc.touched.push(cat as u16);
                    }
                    acc.catsum[cat] += price;
                }
            }
            let hi = (start - cws) as u64;
            let lo_w = (start - WEEK - cws) as u64;
            while cr < ckr.len() && (ckr[cr].lo >> 32) <= hi {
                let c = ckr[cr].lo as u32;
                if acc.cnt[c as usize] == 0 {
                    acc.active.push(c);
                }
                acc.cnt[c as usize] += 1;
                cr += 1;
            }
            while cl < cr && (ckr[cl].lo >> 32) < lo_w {
                let c = ckr[cl].lo as u32;
                acc.cnt[c as usize] -= 1;
                cl += 1;
            }
            let credit = if cl < cr {
                let top = ckr[cr - 1].lo >> 32;
                let mut best = &ckr[cr - 1];
                let mut f = cr - 1;
                while f > cl && (ckr[f - 1].lo >> 32) == top {
                    f -= 1;
                    let c = &ckr[f];
                    if cmp_id(c.cid, best.cid, &fbm) == std::cmp::Ordering::Less {
                        best = c;
                    }
                }
                best.lo as u32
            } else {
                organic
            };
            let slot = &mut acc.camp[credit as usize * ndays + di];
            slot[0] += 1;
            slot[1] += purch as i64;
            slot[2] += rev;
            let (cnt, active, camp) = (&acc.cnt, &mut acc.active, &mut acc.camp);
            active.retain(|&c| cnt[c as usize] > 0);
            for &c in active.iter() {
                camp[c as usize * ndays + di][3] += 1;
            }
            let r = row.as_mut().unwrap();
            r.sessions += 1;
            r.events += (m - k) as i64;
            r.revenue += rev;
            k = m;
        }
        if let Some(mut r) = row.take() {
            r.top = take_top(&mut acc.catsum, &mut acc.touched);
            rows.push(r);
        }
    };

    // per person partition: gather, sort in cache, walk persons
    // Partitions are claimed one at a time, largest first, by one worker per thread, so the
    // partitions holding whale persons start early instead of finishing last.
    let mut order: Vec<usize> = (0..NP).collect();
    let psize: Vec<usize> = (0..NP).map(|p| eparts.iter().map(|g| g[p].len()).sum()).collect();
    order.sort_by_key(|&p| std::cmp::Reverse(psize[p]));
    let next = std::sync::atomic::AtomicUsize::new(0);
    // Column buffers are sized for an even share of the survivors at one row per 6 events
    // (20M: one per 8.7); they grow if a worker produces more.
    let est_rows = psize.iter().sum::<usize>() / nthreads / 6 + 1024;
    let results: Vec<(UserOut, Vec<[i64; 4]>)> = (0..nthreads)
        .into_par_iter()
        .map(|_| {
            let mut rows = UserOut::new(&fbm, &cats, est_rows);
            let mut acc = Acc {
                camp: vec![[0i64; 4]; ncamp * ndays],
                cnt: vec![0u32; ncamp],
                active: Vec::new(),
                catsum: vec![0i64; ncats],
                touched: Vec::new(),
            };
            let mut ek: Vec<(u64, u64)> = Vec::new();
            let mut tmp: Vec<(u64, u64)> = Vec::new();
            let mut ck: Vec<Ck> = Vec::new();
            let mut ids: hashbrown::HashTable<(u64, u32)> = hashbrown::HashTable::new();
            let mut keys: Vec<u64> = Vec::new();
            let use_radix = std::env::var("SS_NORADIX").is_err();
            loop {
                let oi = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if oi >= NP {
                    break;
                }
                let p = order[oi];
                ek.clear();
                ck.clear();
                if !use_radix {
                    for g in &eparts {
                        ek.extend(g[p].iter().filter(|r| r.1 != NONE));
                    }
                    for g in &cparts {
                        ck.extend_from_slice(&g[p]);
                    }
                    ek.sort_unstable();
                    ck.sort_unstable_by_key(|c| (c.pkey, c.lo));
                    let n = ek.len();
                    let (mut ci, mut i) = (0usize, 0usize);
                    while i < n {
                        let key = ek[i].0;
                        let mut j = i + 1;
                        while j < n && ek[j].0 == key {
                            j += 1;
                        }
                        while ci < ck.len() && ck[ci].pkey < key {
                            ci += 1;
                        }
                        let c0 = ci;
                        while ci < ck.len() && ck[ci].pkey == key {
                            ci += 1;
                        }
                        do_person(key, &ek[i..j], &ck[c0..ci], &mut acc, &mut rows);
                        i = j;
                    }
                    continue;
                }
                // Dense person numbers for this partition, then an LSD radix sort on
                // (number << 31 | ts) instead of a comparison sort of (person key, payload).
                ids.clear();
                keys.clear();
                for g in &eparts {
                    for &(pk, lo) in &g[p] {
                        if lo == NONE {
                            continue; // dedup loser or moved row (one-pass path)
                        }
                        let h = mix(pk);
                        let id = match ids.find(h, |e| e.0 == pk) {
                            Some(e) => e.1,
                            None => {
                                let id = keys.len() as u32;
                                ids.insert_unique(h, (pk, id), |e| mix(e.0));
                                keys.push(pk);
                                id
                            }
                        };
                        ek.push(((id as u64) << 31 | lo >> 33, lo));
                    }
                }
                let n = ek.len();
                if n == 0 {
                    continue;
                }
                let idbits = 64 - (keys.len() as u64).leading_zeros();
                radix_sort(&mut ek, &mut tmp, 31 + idbits);
                for g in &cparts {
                    for c in &g[p] {
                        if let Some(e) = ids.find(mix(c.pkey), |e| e.0 == c.pkey) {
                            ck.push(Ck { pkey: e.1 as u64, ..*c });
                        }
                    }
                }
                ck.sort_unstable_by_key(|c| (c.pkey, c.lo));
                let (mut ci, mut i) = (0usize, 0usize);
                while i < n {
                    let id = ek[i].0 >> 31;
                    let mut j = i + 1;
                    while j < n && ek[j].0 >> 31 == id {
                        j += 1;
                    }
                    while ci < ck.len() && ck[ci].pkey < id {
                        ci += 1;
                    }
                    let c0 = ci;
                    while ci < ck.len() && ck[ci].pkey == id {
                        ci += 1;
                    }
                    do_person(keys[id as usize], &ek[i..j], &ck[c0..ci], &mut acc, &mut rows);
                    i = j;
                }
            }
            (rows, acc.camp)
        })
        .collect();
    lap("aggregated");

    let mut camp = vec![[0i64; 4]; ncamp * ndays];
    for (_, c) in &results {
        for (a, b) in camp.iter_mut().zip(c.iter()) {
            for t in 0..4 {
                a[t] += b[t];
            }
        }
    }

    // ---------------- write Parquet
    // Hand-written PLAIN Parquet (src/pq.rs): raw ID bytes straight into page buffers. Counts
    // are INT32 (each is bounded by the total number of events, < 2^31); revenue stays INT64.
    use super::pq::{self, Col, Kind};
    let ud_dir = out.join("user_daily");
    let cd_dir = out.join("campaign_daily");
    std::fs::create_dir_all(&ud_dir).unwrap();
    std::fs::create_dir_all(&cd_dir).unwrap();
    // One file per worker: its column buffers are already encoded (UserOut), so this only
    // adds the page headers and footer and hands the buffers to the kernel.
    let write_users = || {
        results.par_iter().enumerate().for_each(|(pi, (rows, _))| {
            pq::write(&ud_dir.join(format!("part-{:04}.parquet", pi)), &rows.c, rows.n).unwrap();
        });
    };
    let write_camps = || {
        let n = (0..ncamp * ndays).filter(|&i| camp[i][0] != 0 || camp[i][3] != 0).count();
        let mut c = [
            Col::new("campaign_id", Kind::Str, n),
            Col::new("channel", Kind::Str, n),
            Col::new("day", Kind::Date, n),
            Col::new("sessions", Kind::I32, n),
            Col::new("purchasing_sessions", Kind::I32, n),
            Col::new("revenue_cents", Kind::I64, n),
            Col::new("assisted_sessions", Kind::I32, n),
        ];
        for ci in 0..ncamp {
            for d in 0..ndays {
                let v = camp[ci * ndays + d];
                if v[0] == 0 && v[3] == 0 {
                    continue;
                }
                if ci as u32 == organic {
                    c[0].bytes(b"organic");
                    c[1].bytes(b"organic");
                } else {
                    c[0].bytes(camp_names[ci].0);
                    c[1].bytes(camp_names[ci].1);
                }
                c[2].i32((ws_day + d as i64) as i32);
                c[3].i32(v[0] as i32);
                c[4].i32(v[1] as i32);
                c[5].i64(v[2]);
                c[6].i32(v[3] as i32);
            }
        }
        pq::write(&cd_dir.join("part-0000.parquet"), &c, n).unwrap();
    };
    rayon::join(write_users, write_camps);
    lap("written");
    if verbose {
        // peak RSS and total CPU seconds (user+sys), the board tracks CPU-seconds closely
        let st = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        let hwm = st.lines().find(|l| l.starts_with("VmHWM")).unwrap_or("").to_string();
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
        let f: Vec<&str> = stat.rsplit(')').next().unwrap_or("").split_whitespace().collect();
        let tk = |i: usize| f.get(i).and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0) / 100.0;
        eprintln!("{}  cpu user {:.2}s sys {:.2}s", hwm, tk(11), tk(12));
    }
    // Every output file is written and closed. Exit here, inside run(), so the multi-GB locals
    // above are never dropped (returning first would free them all, ~20 ms at 20M).
    std::process::exit(0)
}

#[cfg(test)]
mod click_tests {
    use super::*;

    fn rng(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    fn same(a: &[Vec<Ck>], b: &[Vec<Ck>]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.len() == y.len() && x.iter().zip(y).all(|(p, q)| (p.pkey, p.lo, p.cid) == (q.pkey, q.lo, q.cid)))
    }

    #[test]
    fn resolve_scatter_matches_the_old_path() {
        let mut s = 0x2468_ace1_3579_bdf0u64;
        for round in 0..60 {
            // A link table of n ids in groups (some their own root), with and without the filter.
            let n = [0usize, 1, 7, 300, 5000, 40000][round % 6];
            let ids: Vec<u64> = (0..n).map(|_| (rng(&mut s) >> 4) | 1).collect();
            let mut t = LinkTab::new(n);
            for (i, &id) in ids.iter().enumerate() {
                let root = if rng(&mut s) % 3 == 0 { id } else { ids[(rng(&mut s) % (i as u64 + 1)) as usize] };
                t.insert(id, root);
            }
            if round % 2 == 0 {
                t.build_filter();
            }
            // Every length around the block size, keys mixing linked ids, roots and strangers.
            for len in [0usize, 1, 2, 255, 256, 257, 511, 512, 513, 1000, 70000] {
                if len == 70000 && round % 10 != 0 {
                    continue;
                }
                let raw: Vec<Ck> = (0..len)
                    .map(|i| {
                        let k = if n > 0 && rng(&mut s) % 3 != 0 { ids[(rng(&mut s) % n as u64) as usize] } else { (rng(&mut s) >> 4) | 1 };
                        Ck { pkey: k, lo: i as u64, cid: rng(&mut s) }
                    })
                    .collect();
                let want = resolve_scatter_old(&raw, &t);
                let mut r2 = raw.clone();
                let got = resolve_scatter(&mut r2, &t);
                assert!(same(&want, &got), "round {} len {}", round, len);
                assert!(got.iter().enumerate().all(|(p, v)| v.capacity() == v.len() && v.iter().all(|c| ppart(c.pkey) == p)));
                // person keys are what the table says, one by one
                assert!(raw.iter().zip(r2.iter()).all(|(a, b)| b.pkey == t.get(a.pkey) && (a.lo, a.cid) == (b.lo, b.cid)));
            }
        }
    }

    #[test]
    fn pack_tab_matches_the_dict() {
        let mut s = 0x0f1e_2d3c_4b5a_6978u64;
        let alpha = b"abcXYZ019_-. \xff\x00";
        for round in 0..300 {
            let n = [0usize, 1, 3, 50, 2000][round % 5];
            let mk = |s: &mut u64| -> Vec<u8> {
                let len = 1 + (rng(s) % 13) as usize;
                // mostly packable ids, some with bytes outside the alphabet or longer than 10
                let wild = rng(s) % 4 == 0;
                (0..len).map(|_| alpha[(rng(s) % if wild { alpha.len() as u64 } else { 10 }) as usize]).collect()
            };
            let names: Vec<Vec<u8>> = (0..n).map(|i| if i % 7 == 3 { format!("c_{}", i).into_bytes() } else { mk(&mut s) }).collect();
            let mut dict = Dict::new(n * 2);
            let mut uniq: Vec<(&[u8], u32)> = Vec::new();
            for nm in &names {
                if dict.get(nm).is_none() {
                    dict.insert(nm, uniq.len() as u32);
                    uniq.push((nm, uniq.len() as u32));
                }
            }
            let tab = PackTab::new(uniq.iter().copied());
            for nm in &names {
                assert_eq!(tab.get(nm, &dict), dict.get(nm));
            }
            for i in 0..4000 {
                let q = if i % 5 == 0 { format!("c_{}", rng(&mut s) % 3000).into_bytes() } else { mk(&mut s) };
                assert_eq!(tab.get(&q, &dict), dict.get(&q), "{:?}", q);
                // a present id with one byte changed or one byte appended
                if n > 0 {
                    let mut v = names[(rng(&mut s) % n as u64) as usize].clone();
                    if i % 2 == 0 {
                        let k = (rng(&mut s) % v.len() as u64) as usize;
                        v[k] = alpha[(rng(&mut s) % alpha.len() as u64) as usize];
                    } else {
                        v.push(alpha[(rng(&mut s) % alpha.len() as u64) as usize]);
                    }
                    assert_eq!(tab.get(&v, &dict), dict.get(&v), "{:?}", v);
                }
            }
        }
    }
}

#[cfg(test)]
mod pack_tests {
    use super::*;

    fn rng(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }
    const ALPHA: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ_abcdefghijklmnopqrstuvwxyz";

    /// Check `pack` against `pack_ref` with the field at a random offset of a page-aligned
    /// buffer that straddles a page boundary, so both the wide path and the page-edge path are
    /// compared, with random bytes around the field.
    #[repr(align(4096))]
    struct Pages([u8; 8192]);
    fn check(id: &[u8], s: &mut u64, pages: &mut Pages) {
        // Field start anywhere in the last 48 bytes of the first page or first 16 of the second.
        let o = 4096 - 48 + (rng(s) % 64) as usize;
        for x in pages.0[4096 - 64..4096 + 64].iter_mut() {
            *x = rng(s) as u8;
        }
        pages.0[o..o + id.len()].copy_from_slice(id);
        let f = &pages.0[o..o + id.len()];
        assert_eq!(pack(f), pack_ref(f), "id {:?} at page offset {}", id, o & 4095);
    }

    #[test]
    fn alphabet_is_the_reference_alphabet() {
        // 63 codes 1..=63 in byte order, nothing else; 0 is "no byte" so length is part of the key.
        let mut next = 1u8;
        for b in 0..=255u8 {
            if ALPHA.contains(&b) {
                assert_eq!(CODE[b as usize], next);
                next += 1;
            } else {
                assert_eq!(CODE[b as usize], 0);
            }
        }
        assert_eq!(next, 64);
    }

    #[test]
    fn every_byte_value_at_every_position_and_length() {
        let mut s = 0x1234_5678_9ABC_DEF1u64;
        let mut pages = Box::new(Pages([0; 8192]));
        for len in 1..=16usize {
            for pos in 0..len {
                for b in 0..=255u8 {
                    for fill in [b'0', b'z', b'_', b'A'] {
                        for _ in 0..4 {
                            let mut id = vec![fill; len];
                            id[pos] = b;
                            check(&id, &mut s, &mut pages);
                        }
                    }
                }
            }
        }
        check(b"", &mut s, &mut pages);
    }

    #[test]
    fn random_and_adversarial_ids_match_reference() {
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        let mut pages = Box::new(Pages([0; 8192]));
        // Bytes next to the alphabet ranges, separators, CR, NUL, DEL and non-ASCII.
        let edge: &[u8] = b"/:@[^`{\x7f\x00\r\n\t ,\"-.\x80\xb0\xb9\xc1\xdf\xe1\xff\x30\x39\x41\x5a\x5f\x61\x7a";
        for i in 0..6_000_000u32 {
            let len = 1 + (rng(&mut s) % 16) as usize;
            let mut id = [0u8; 16];
            let mode = i % 4;
            for x in id[..len].iter_mut() {
                let r = rng(&mut s);
                *x = match mode {
                    0 => ALPHA[(r % 63) as usize],
                    1 => r as u8,
                    2 => {
                        if r % 9 == 0 {
                            edge[(r >> 8) as usize % edge.len()]
                        } else {
                            ALPHA[(r >> 8) as usize % 63]
                        }
                    }
                    _ => {
                        if r % 16 == 0 {
                            (r >> 8) as u8
                        } else {
                            ALPHA[(r >> 8) as usize % 63]
                        }
                    }
                };
            }
            check(&id[..len], &mut s, &mut pages);
        }
    }

    #[test]
    fn keys_are_exact_and_order_preserving() {
        // Different strings never share a key, and key order is byte order (all sampled pairs).
        let mut s = 0xDEAD_BEEF_1234_5677u64;
        let mut ids: Vec<Vec<u8>> = [&b"a"[..], b"a0", b"a00", b"ab", b"aB", b"a_", b"0", b"zzzzzzzzzz", b"0000000000", b"000000000", b"_", b"Z", b"z"]
            .iter()
            .map(|x| x.to_vec())
            .collect();
        for _ in 0..2500 {
            let len = 1 + (rng(&mut s) % 10) as usize;
            ids.push((0..len).map(|_| ALPHA[(rng(&mut s) % 6 * 12) as usize]).collect());
        }
        let keys: Vec<u64> = ids.iter().map(|i| pack(i).unwrap()).collect();
        for k in &keys {
            assert!(*k != 0 && *k & FB == 0 && *k != NONE);
        }
        for i in 0..ids.len() {
            for j in 0..ids.len() {
                assert_eq!(ids[i].cmp(&ids[j]), keys[i].cmp(&keys[j]), "{:?} {:?}", ids[i], ids[j]);
            }
        }
        // Round trip through the decoder.
        let m = HashMap::new();
        for (i, k) in ids.iter().zip(keys.iter()) {
            assert_eq!(&bytes_of(*k, &m), i);
        }
    }

    #[test]
    fn bytes_outside_the_alphabet_never_alias() {
        // A trailing CR, NUL, space, '-' or non-ASCII byte makes a different ID: it must take the
        // hashed fallback (flag bit set) and be recorded for the byte-exact collision check.
        let mut fb = Fb::new();
        let plain = key_of(b"e3", &mut fb);
        assert!(fb.is_empty() && plain & FB == 0);
        let odd: [&'static [u8]; 13] =
            [b"e3\r", b"e3\0", b"e3 ", b"e3-", b"e3.", b"e3\t", b"\xef\xbc\x93", b"e\xd9\xa3", b"e3\xff", b" e3", b"e3@", b"abcdefghijk", b"e3\0\0\0\0\0\0\0\0"];
        let mut seen = vec![plain];
        for (i, id) in odd.iter().enumerate() {
            let k = key_of(id, &mut fb);
            assert!(k & FB != 0, "{:?}", id);
            assert_eq!(fb.len(), i + 1);
            assert_eq!(fb[i], (k, *id));
            assert!(!seen.contains(&k));
            seen.push(k);
        }
    }

    /// Fields at the very end of a mapping whose next page is unmapped (a file whose size is a
    /// multiple of 4096): `pack` must not read past the page. The test faults if it does.
    #[cfg(target_os = "linux")]
    #[test]
    fn no_read_past_a_mapping_that_ends_on_a_page_boundary() {
        unsafe {
            let pg = 4096usize;
            let m = libc::mmap(std::ptr::null_mut(), 3 * pg, libc::PROT_NONE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0);
            assert!(m != libc::MAP_FAILED);
            let base = (m as *mut u8).add(pg);
            assert_eq!(libc::mprotect(base as *mut libc::c_void, pg, libc::PROT_READ | libc::PROT_WRITE), 0);
            // [PROT_NONE][readable page][PROT_NONE]
            let page = std::slice::from_raw_parts_mut(base, pg);
            let mut s = 77u64;
            for x in page.iter_mut() {
                *x = ALPHA[(rng(&mut s) % 63) as usize];
            }
            let page: &[u8] = page;
            for len in 0..=16usize {
                for end_gap in 0..=40usize {
                    let f = &page[pg - end_gap - len..pg - end_gap];
                    assert_eq!(pack(f), pack_ref(f));
                }
                let f = &page[..len];
                assert_eq!(pack(f), pack_ref(f));
            }
            libc::munmap(m, 3 * pg);
        }
    }
}

#[cfg(test)]
mod link_tests {
    use super::*;

    fn rng(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    /// The previous construction (sort ids, index table, union-find by index, second table).
    fn build_link_table_ref(parts: &[(Vec<(u64, u64)>, Fb)], fbm: &HashMap<u64, &'static [u8]>) -> LinkTab {
        let mut lids: Vec<u64> = Vec::with_capacity(parts.iter().map(|p| p.0.len() * 2).sum());
        for (v, _) in parts.iter() {
            for &(a, b) in v {
                lids.push(a);
                lids.push(b);
            }
        }
        lids.sort_unstable();
        lids.dedup();
        if !fbm.is_empty() {
            lids.sort_by(|&a, &b| cmp_id(a, b, fbm));
        }
        let mut idx: hashbrown::HashTable<(u64, u32)> = hashbrown::HashTable::with_capacity(lids.len());
        for (i, &k) in lids.iter().enumerate() {
            idx.insert_unique(th(k), (k, i as u32), |e| th(e.0));
        }
        let get = |k: u64| idx.find(th(k), |e| e.0 == k).unwrap().1;
        let mut parent: Vec<u32> = (0..lids.len() as u32).collect();
        fn find(p: &mut [u32], mut x: u32) -> u32 {
            while p[x as usize] != x {
                let g = p[p[x as usize] as usize];
                p[x as usize] = g;
                x = g;
            }
            x
        }
        for (v, _) in parts.iter() {
            for &(a, b) in v {
                let ra = find(&mut parent, get(a));
                let rb = find(&mut parent, get(b));
                if ra != rb {
                    if ra < rb { parent[rb as usize] = ra } else { parent[ra as usize] = rb }
                }
            }
        }
        let mut table = LinkTab::new(lids.len());
        for i in 0..lids.len() as u32 {
            let r = find(&mut parent, i);
            table.insert(lids[i as usize], lids[r as usize]);
        }
        table.build_filter();
        table
    }

    fn contents(t: &LinkTab) -> Vec<[u64; 2]> {
        let mut v: Vec<[u64; 2]> = t.slots.iter().filter(|s| s[0] != 0).cloned().collect();
        v.sort();
        v
    }

    #[test]
    fn one_table_build_matches_two_table_build() {
        let mut s = 0x0123_4567_89AB_CDEFu64;
        // id pool: short packed ids plus ids that need the hashed fallback (non-alphabet bytes,
        // longer than 10), so both key kinds and their byte-wise ordering are covered
        let mut pool: Vec<&'static [u8]> = Vec::new();
        for i in 0..3000u32 {
            let txt = match i % 7 {
                0 => format!("u{}", i),
                1 => format!("U{:05}", i),
                2 => format!("user_{}", i % 500),
                3 => format!("u-{}", i),
                4 => format!("averyveryverylongid{}", i),
                5 => format!("u{}\r", i),
                _ => format!("{}", i),
            };
            pool.push(Box::leak(txt.into_bytes().into_boxed_slice()));
        }
        for round in 0..300 {
            let nparts = 1 + (rng(&mut s) % 4) as usize;
            let universe = match round % 4 {
                0 => 8,
                1 => 60,
                2 => 600,
                _ => pool.len(),
            };
            let mut parts: Vec<(Vec<(u64, u64)>, Fb)> = Vec::new();
            for _ in 0..nparts {
                let mut fb = Fb::new();
                let n = (rng(&mut s) % if round % 5 == 0 { 3 } else { 900 }) as usize;
                let mut v = Vec::new();
                for _ in 0..n {
                    let a = pool[(rng(&mut s) as usize) % universe];
                    // chains, self links and repeated pairs
                    let b = match rng(&mut s) % 10 {
                        0 => a,
                        1..=3 => pool[((rng(&mut s) as usize) % universe + 1) % universe],
                        _ => pool[(rng(&mut s) as usize) % universe],
                    };
                    v.push((key_of(a, &mut fb), key_of(b, &mut fb)));
                }
                parts.push((v, fb));
            }
            let fbm = merge_fb(parts.iter().map(|p| &p.1)).unwrap();
            let new = build_link_table(&parts, &fbm);
            let old = build_link_table_ref(&parts, &fbm);
            assert_eq!(new.n, old.n);
            assert_eq!(contents(&new), contents(&old), "round {}", round);
            assert!(new.nf == old.nf && new.fmask == old.fmask && !new.nf.is_empty());
            // lookups agree for members and for ids that are not in any link
            for id in pool.iter().take(universe.min(700)) {
                let mut fb = Fb::new();
                let k = key_of(id, &mut fb);
                assert_eq!(new.get(k), old.get(k));
                if new.get(k) != k {
                    assert!(new.maybe_linked(k));
                }
            }
        }
    }
}
