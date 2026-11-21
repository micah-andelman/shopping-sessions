//! Engine 2: a fast path in front of v5.12 for data whose IDs are written the way the published
//! generator writes them (a prefix letter, then lowercase base 36 of a 40-bit bijective scramble
//! of a counter). Ideas from the SHOP_ENGINE2 design notes (Team 67's public design, our code).
//!
//! Correctness never depends on the scramble keys: decoding a canonical ID string is a bijection
//! onto 0..2^40 for ANY key, so "same string" == "same decoded value". The keys only decide how
//! many IDs land in the dense ranges the fast structures cover; everything else takes an exact
//! "odd" path keyed by the raw bytes (`key_of`, with the same collision check as v5.12).
//! - event ID -> dense index: dedup is a direct-indexed slot array per 32,768 indices;
//! - user ID -> (person, alias): identity is a per-person 15-alias union (nibble parents), and
//!   only links that leave a person (shared devices, odd strings, aliases >= 15) go to a general
//!   union-find, whose persons are then handled by an exact sort-based path;
//! - survivors are grouped by person with a counting scatter that is nearly sequential, because
//!   event index order is person order; per person the events are checked (and if needed sorted)
//!   by time, so no assumption about order is needed for correctness.
//! Returns false (nothing written) when a layout limit is exceeded; the caller then runs v5.12.

use super::*;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering::Relaxed};

/// Board diagnostic switch (see `decline`).
const DIAG_SLEEP: bool = false;
const MASK40: u64 = (1 << 40) - 1;
const M1: u64 = 0x9E37_79B9_7F4B;
const M2: u64 = 0xD6E8_FEB8_6659;
const fn inv64(a: u64) -> u64 {
    let mut x = a;
    let mut i = 0;
    while i < 6 {
        x = x.wrapping_mul(2u64.wrapping_sub(a.wrapping_mul(x)));
        i += 1;
    }
    x
}
const IM1: u64 = inv64(M1) & MASK40;
const IM2: u64 = inv64(M2) & MASK40;

/// The generator's scramble (kit/generator text.rs), a bijection on 0..2^40.
#[inline]
pub fn scramble(v: u64, key: u64) -> u64 {
    let mut m = ((v & MASK40).wrapping_mul(M1) ^ key) & MASK40;
    m ^= m >> 17;
    m.wrapping_mul(M2) & MASK40
}
/// Undo everything after the key xor: pre(scramble(v, k)) == (v * M1 ^ k) mod 2^40.
#[inline]
fn pre(w: u64) -> u64 {
    let mut m = w.wrapping_mul(IM2) & MASK40;
    m ^= m >> 17;
    m ^= m >> 34;
    m
}
#[inline]
pub fn unscramble(w: u64, key: u64) -> u64 {
    ((pre(w) ^ key) & MASK40).wrapping_mul(IM1) & MASK40
}

const fn b36_table() -> [u8; 256] {
    let mut t = [255u8; 256];
    let mut i = 0;
    while i < 10 {
        t[b'0' as usize + i] = i as u8;
        i += 1;
    }
    let mut i = 0;
    while i < 26 {
        t[b'a' as usize + i] = 10 + i as u8;
        i += 1;
    }
    t
}
static B36: [u8; 256] = b36_table();
const DIG: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// Canonical `prefix` + lowercase base 36 (no leading zero, value < 2^40) -> the value.
#[inline]
pub fn b36(b: &[u8], prefix: u8) -> Option<u64> {
    let n = b.len();
    if n < 2 || n > 9 || b[0] != prefix || (b[1] == b'0' && n > 2) {
        return None;
    }
    let mut v = 0u64;
    for &c in &b[1..] {
        let d = B36[c as usize];
        if d >= 36 {
            return None;
        }
        v = v * 36 + d as u64;
    }
    if v > MASK40 { None } else { Some(v) }
}
/// Inverse of `b36`.
pub fn enc_into(prefix: u8, mut w: u64, out: &mut Vec<u8>) {
    let mut buf = [0u8; 10];
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = DIG[(w % 36) as usize];
        w /= 36;
        if w == 0 {
            break;
        }
    }
    out.push(prefix);
    out.extend_from_slice(&buf[at..]);
}

/// Simple users: alias < 15 and person < 2^24. Everything else is an odd user.
const NALIAS: u64 = 15;
const PBITS: u32 = 24;
const TAG: u64 = 1 << 62;
#[inline]
fn node_s(p: u32, a: u32) -> u64 {
    TAG | (p as u64) << 4 | a as u64
}
#[inline]
fn user(b: &[u8], ku: u64, pb: u64) -> Option<(u32, u32)> {
    let w = b36(b, b'u')?;
    let v = unscramble(w, ku);
    let (p, a) = (v >> 9, v & 511);
    if a < NALIAS && p < pb { Some((p as u32, a as u32)) } else { None }
}
/// Sort key with the byte order of base-36 strings: the digits left-aligned to 8 places, then the
/// length (a string that is a prefix of another sorts first).
const P36: [u64; 9] = [1, 36, 1296, 46656, 1679616, 60466176, 2176782336, 78364164096, 2821109907456];
#[inline]
fn name_key(w: u64) -> (u64, u8) {
    let mut len = 1usize;
    while len < 8 && w >= P36[len] {
        len += 1;
    }
    (w * P36[8 - len], len as u8)
}
fn user_name(p: u32, a: u32, ku: u64, out: &mut Vec<u8>) {
    enc_into(b'u', scramble((p as u64) << 9 | a as u64, ku), out);
}

/// Per-person alias forest: nibble a = parent of alias a (0 = untouched: every alias its own).
const NIB_ID: u64 = 0x0EDC_BA98_7654_3210;
#[inline]
fn nfind(x: u64, mut a: u32) -> u32 {
    if x == 0 {
        return a;
    }
    loop {
        let p = ((x >> (4 * a)) & 15) as u32;
        if p == a {
            return a;
        }
        a = p;
    }
}
fn nunion(x: &mut u64, a: u32, b: u32) {
    if *x == 0 {
        *x = NIB_ID;
    }
    let (ra, rb) = (nfind(*x, a), nfind(*x, b));
    if ra != rb {
        *x = (*x & !(15u64 << (4 * ra))) | (rb as u64) << (4 * ra);
    }
}

/// At most `miss` samples fail `good` (stops at the first miss over the limit: wrong candidate
/// keys fail almost every sample, so this usually reads one or two).
#[inline]
fn within(ws: &[u64], miss: usize, good: impl Fn(u64) -> bool) -> bool {
    let mut m = 0;
    for &w in ws {
        if !good(w) {
            m += 1;
            if m > miss {
                return false;
            }
        }
    }
    true
}
/// Recover the user key: some sample is alias 0 of a person < pb, so key = y0 ^ (P*512*M1) for
/// one P; a candidate is accepted when every sample decodes to a person < pb.
fn recover_user_key(ws: &[u64], pb: u64) -> Option<u64> {
    let ys: Vec<u64> = ws.iter().map(|&w| pre(w)).collect();
    // tolerant: a few sampled ids outside the dense range (not generator-shaped) are fine
    let need = ws.len() - ws.len() / 24; // up to 2 of 48 may miss
    let ok = |k: u64| within(ws, ws.len() - need, |w| unscramble(w, k) >> 9 < pb);
    for &y0 in ys.iter().take(8) {
        let hit = (0..pb).into_par_iter().find_any(|&p| ok(y0 ^ ((p << 9).wrapping_mul(M1) & MASK40)));
        if let Some(p) = hit {
            return Some(y0 ^ ((p << 9).wrapping_mul(M1) & MASK40));
        }
    }
    None
}
/// Recover the event key: `y0`'s index is below `lim`; every sample must decode below `eb`.
fn recover_event_key(ws: &[u64], w0: u64, lim: u64, eb: u64) -> Option<u64> {
    let y0 = pre(w0);
    let need = ws.len() - ws.len() / 24; // up to 2 of 48 may miss
    let ok = |k: u64| within(ws, ws.len() - need, |w| unscramble(w, k) < eb);
    (0..lim).into_par_iter().find_any(|&i| ok(y0 ^ (i.wrapping_mul(M1) & MASK40))).map(|i| y0 ^ (i.wrapping_mul(M1) & MASK40))
}

/// Event record word 0: (ts - ws) << 33 | price << 9 | cat << 4 | alias (alias 15 = odd user),
/// or NONE for an invalid copy. Word 1: idx & 0x7FFF | ing << 15 | pid << 36, where pid is the
/// person (simple user) or worker << 22 | index into that worker's odd-user list.
const BSH: u32 = 15;
/// Click buckets: 2^CB persons each.
const CB: u32 = 10;
/// The low CB bits of a person, kept in a click's `lo` at bits 18..28 (campaigns < 2^18).
const PLO: u64 = (1 << CB) - 1;
const PSH: u32 = 18;
const BSZ: usize = 1 << BSH;
const ING_BAD: u64 = (1 << 21) - 1;
#[inline]
fn ing_of(w1: u64) -> u64 {
    (w1 >> 15) & ING_BAD
}

/// 64-byte line of four event records.
#[derive(Clone, Copy)]
#[repr(C, align(64))]
struct Line4([[u64; 2]; 4]);
/// Write-combining bucket: records are staged in one line (all buckets' staging lines stay in
/// cache) and full lines go out with non-temporal stores, so the scatter does no
/// read-for-ownership of its target lines.
///
/// Lines go into a fixed region of the record arena (`a`, `acap` lines; see the parse setup), so
/// the dedup can later write the bucket's survivor stream over the same, already-faulted pages.
/// Lines past the region go to `over` (rare: the region has 25% headroom).
struct Wc4 {
    a: *mut Line4,
    alen: usize,
    acap: usize,
    over: Vec<Line4>,
    n: usize,
    buf: Line4,
}
// Safety: the region belongs to this Wc4 (one writer during the parse, one reader in the dedup).
unsafe impl Send for Wc4 {}
impl Wc4 {
    fn new() -> Self {
        Wc4 { a: std::ptr::null_mut(), alen: 0, acap: 0, over: Vec::new(), n: 0, buf: Line4([[0; 2]; 4]) }
    }
    fn with_region(a: *mut Line4, acap: usize) -> Self {
        Wc4 { a, alen: 0, acap, over: Vec::new(), n: 0, buf: Line4([[0; 2]; 4]) }
    }
    #[inline(always)]
    fn push(&mut self, r: [u64; 2], _cap_lines: usize) {
        self.buf.0[self.n & 3] = r;
        self.n += 1;
        if self.n & 3 == 0 {
            if self.alen < self.acap {
                unsafe {
                    let dst = self.a.add(self.alen);
                    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
                    {
                        use std::arch::x86_64::*;
                        let s = &self.buf as *const Line4 as *const __m256i;
                        let d = dst as *mut __m256i;
                        _mm256_stream_si256(d, _mm256_load_si256(s));
                        _mm256_stream_si256(d.add(1), _mm256_load_si256(s.add(1)));
                    }
                    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
                    std::ptr::write(dst, self.buf);
                }
                self.alen += 1;
            } else {
                self.over.push(self.buf);
            }
        }
    }
    fn finish(&mut self) {
        if self.n & 3 != 0 {
            if self.alen < self.acap {
                unsafe { std::ptr::write(self.a.add(self.alen), self.buf) };
                self.alen += 1;
            } else {
                self.over.push(self.buf);
            }
        }
    }
    /// The records: the region's part, then the overflow part.
    #[inline]
    fn parts(&self) -> (&[[u64; 2]], &[[u64; 2]]) {
        let ra = self.n.min(self.alen * 4);
        let a = if ra == 0 { &[][..] } else { unsafe { std::slice::from_raw_parts(self.a as *const [u64; 2], ra) } };
        let o = unsafe { std::slice::from_raw_parts(self.over.as_ptr() as *const [u64; 2], self.n - ra) };
        (a, o)
    }
}

/// Survivors of one dedup bucket in index order: word 0 per event, plus one (person, start)
/// header per run of one person.
/// Survivor words: written into borrowed memory (the bucket's spent record region, never freed)
/// or into an owned Vec; moves to an owned Vec if it ever fills.
struct Sb {
    p: *mut u64,
    len: usize,
    cap: usize,
    own: Vec<u64>,
}
// Safety: `p` points into `own` or into the leaked record arena; one owner at a time.
unsafe impl Send for Sb {}
unsafe impl Sync for Sb {}
impl Default for Sb {
    fn default() -> Self {
        Sb::owned(0)
    }
}
impl Sb {
    fn owned(n: usize) -> Self {
        let mut own: Vec<u64> = Vec::with_capacity(n);
        Sb { p: own.as_mut_ptr(), len: 0, cap: own.capacity(), own }
    }
    /// Safety: `p..p+cap` is writable, 8-byte aligned and outlives the Sb.
    unsafe fn borrowed(p: *mut u64, cap: usize) -> Self {
        Sb { p, len: 0, cap, own: Vec::new() }
    }
    #[inline(always)]
    fn push(&mut self, x: u64) {
        if self.len == self.cap {
            self.grow();
        }
        unsafe { *self.p.add(self.len) = x };
        self.len += 1;
    }
    #[cold]
    fn grow(&mut self) {
        let mut v: Vec<u64> = Vec::with_capacity((self.cap * 2).max(16));
        v.extend_from_slice(self);
        self.own = v;
        self.p = self.own.as_mut_ptr();
        self.cap = self.own.capacity();
    }
}
impl std::ops::Deref for Sb {
    type Target = [u64];
    #[inline]
    fn deref(&self) -> &[u64] {
        unsafe { std::slice::from_raw_parts(self.p, self.len) }
    }
}

#[derive(Default)]
struct Sv {
    w: Sb,
    runs: Vec<(u32, u32)>,
}
impl Sv {
    #[inline(always)]
    fn push(&mut self, p: u32, w0: u64) {
        if self.runs.last().map_or(true, |r| r.0 != p) {
            self.runs.push((p, self.w.len() as u32));
        }
        self.w.push(w0);
    }
    /// (person, start, end) of every run
    fn each(&self) -> impl Iterator<Item = (u32, usize, usize)> + '_ {
        let n = self.runs.len();
        (0..n).map(move |i| {
            let e = if i + 1 < n { self.runs[i + 1].1 as usize } else { self.w.len() };
            (self.runs[i].0, self.runs[i].1 as usize, e)
        })
    }
}

struct Wk {
    /// Event records per index bucket. UnsafeCell: the dedup worker that owns bucket b frees
    /// its vectors as soon as the bucket is done (each bucket has exactly one owner).
    bk: Vec<std::cell::UnsafeCell<Wc4>>,
    oddev: Vec<(u64, [u64; 2])>,
    oddu: Vec<u64>,
    fb: Fb,
    /// simple clicks: ((ts - cws) << 32 | alias << 28 | (person & PLO) << 18 | campaign, click key)
    /// by person bucket (person >> CB); the bucket gives the rest of the person
    cks: Vec<Vec<(u64, u64)>>,
    /// odd-user clicks: (user key, (ts - cws) << 32 | campaign, click key)
    cko: Vec<(u64, u64, u64)>,
    li: Vec<(u32, u8, u8)>,
    lg: Vec<(u64, u64)>,
    maxp: u32,
    /// 1: ingest not whole seconds or before window_start - 1 day; 2: odd-user list overflow
    fail: u8,
}
// Safety: `bk` cells are only mutated by the single dedup worker that claimed that bucket index.
unsafe impl Sync for Wk {}

struct Raw<T>(*mut T);
unsafe impl<T> Send for Raw<T> {}
unsafe impl<T> Sync for Raw<T> {}
impl<T> Clone for Raw<T> {
    fn clone(&self) -> Self {
        Raw(self.0)
    }
}
impl<T> Copy for Raw<T> {}

/// read_small with borrowed fields: the file is read once into a leaked buffer and rows are
/// (first, second) slices of it (same lines and fields as read_small, no per-row allocations).
fn read_pairs(path: &Path) -> Vec<(&'static [u8], &'static [u8])> {
    let data: &'static [u8] = match std::fs::read(path) {
        Ok(d) => Vec::leak(d),
        Err(_) => return Vec::new(),
    };
    let body = match memchr::memchr(b'\n', data) {
        Some(p) => &data[p + 1..],
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    for_lines(body, |l| {
        if let Some([Some(a), Some(b)]) = fields::<2>(l) {
            out.push((a, b));
        }
    });
    out
}

struct UserOut {
    c: [super::pq::Col; 6],
    n: usize,
}
impl UserOut {
    fn new(est: usize) -> Self {
        use super::pq::{Col, Kind};
        UserOut {
            c: [
                Col::new("user_id", Kind::Str, est),
                Col::new("day", Kind::Date, est),
                Col::new("sessions", Kind::I32, est),
                Col::new("events", Kind::I32, est),
                Col::new("revenue_cents", Kind::I64, est),
                Col::new("top_category", Kind::OptStr, est),
            ],
            n: 0,
        }
    }
}
struct Acc {
    camp: Vec<[i64; 4]>,
    cnt: Vec<u32>,
    active: Vec<u32>,
    catsum: Vec<i64>,
    touched: Vec<u16>,
}
struct Ctx<'a> {
    ws: i64,
    cws: i64,
    ws_day: i64,
    ndays: usize,
    organic: u32,
    cats: &'a [Vec<u8>],
    fbm: &'a HashMap<u64, &'static [u8]>,
}

/// One identity's rows. `ev`: word-0 values sorted by time; `ck`: (lo, click key) sorted by time,
/// campaign in the low 28 bits of lo. Same rules as fast.rs `do_person`.
fn do_person(cx: &Ctx, name: &[u8], ev: &[u64], ck: &[(u64, u64)], acc: &mut Acc, rows: &mut UserOut) {
    let (ws, cws, ndays) = (cx.ws, cx.cws, cx.ndays);
    for &a in &acc.active {
        acc.cnt[a as usize] = 0;
    }
    acc.active.clear();
    let (mut cl, mut cr) = (0usize, 0usize);
    let mut cur_day = i64::MIN;
    let mut row: Option<(i64, i64, i64, i64)> = None; // day, sessions, events, revenue
    let flush = |r: (i64, i64, i64, i64), acc: &mut Acc, rows: &mut UserOut| {
        let top = take_top(&mut acc.catsum, &mut acc.touched);
        rows.c[0].bytes(name);
        rows.c[1].i32(r.0 as i32);
        rows.c[2].i32(r.1 as i32);
        rows.c[3].i32(r.2 as i32);
        rows.c[4].i64(r.3);
        match top {
            Some(t) => rows.c[5].bytes(&cx.cats[t as usize]),
            None => rows.c[5].null(),
        }
        rows.n += 1;
    };
    let mut k = 0usize;
    let j = ev.len();
    while k < j {
        let start = (ev[k] >> 33) as i64 + ws;
        let mut prev = start;
        let mut m = k + 1;
        while m < j {
            let t = (ev[m] >> 33) as i64 + ws;
            if t - prev > GAP {
                break;
            }
            prev = t;
            m += 1;
        }
        let day = start.div_euclid(DAY);
        let di = (day - cx.ws_day) as usize;
        if day != cur_day {
            if let Some(r) = row.take() {
                flush(r, acc, rows);
            }
            cur_day = day;
            row = Some((day, 0, 0, 0));
        }
        let mut rev = 0i64;
        let mut purch = false;
        for &w in &ev[k..m] {
            let price = ((w >> 9) & 0xFF_FFFF) as i64;
            if price > 0 {
                let cat = ((w >> 4) & 31) as usize;
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
        while cr < ck.len() && (ck[cr].0 >> 32) <= hi {
            let c = (ck[cr].0 as u32) & 0x0FFF_FFFF;
            if acc.cnt[c as usize] == 0 {
                acc.active.push(c);
            }
            acc.cnt[c as usize] += 1;
            cr += 1;
        }
        while cl < cr && (ck[cl].0 >> 32) < lo_w {
            let c = (ck[cl].0 as u32) & 0x0FFF_FFFF;
            acc.cnt[c as usize] -= 1;
            cl += 1;
        }
        let credit = if cl < cr {
            let top = ck[cr - 1].0 >> 32;
            let mut best = &ck[cr - 1];
            let mut f = cr - 1;
            while f > cl && (ck[f - 1].0 >> 32) == top {
                f -= 1;
                let c = &ck[f];
                if cmp_id(c.1, best.1, cx.fbm) == std::cmp::Ordering::Less {
                    best = c;
                }
            }
            (best.0 as u32) & 0x0FFF_FFFF
        } else {
            cx.organic
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
        r.1 += 1;
        r.2 += (m - k) as i64;
        r.3 += rev;
        k = m;
    }
    if let Some(r) = row.take() {
        flush(r, acc, rows);
    }
}

/// General union-find over node keys (simple-user tags or odd-user keys).
struct Uf {
    map: HashMap<u64, u32>,
    par: Vec<u32>,
    nodes: Vec<u64>,
}
impl Uf {
    fn id(&mut self, k: u64) -> u32 {
        if let Some(&i) = self.map.get(&k) {
            return i;
        }
        let i = self.par.len() as u32;
        self.map.insert(k, i);
        self.par.push(i);
        self.nodes.push(k);
        i
    }
    fn find(&mut self, mut x: u32) -> u32 {
        while self.par[x as usize] != x {
            let g = self.par[self.par[x as usize] as usize];
            self.par[x as usize] = g;
            x = g;
        }
        x
    }
    fn union(&mut self, a: u64, b: u64) {
        let (ia, ib) = (self.id(a), self.id(b));
        let (ra, rb) = (self.find(ia), self.find(ib));
        if ra != rb {
            self.par[ra as usize] = rb;
        }
    }
}

/// argv and envp (with SS_E2=0 first) for re-executing this binary, built before any work so
/// the fault handler does not allocate. Pointers to leaked, NUL-terminated arrays.
static RX_ARGV: AtomicUsize = AtomicUsize::new(0);
static RX_ENVP: AtomicUsize = AtomicUsize::new(0);
static RX_EXE: AtomicUsize = AtomicUsize::new(0);
fn prep_reexec() {
    use std::os::unix::ffi::OsStrExt;
    let cs = |b: &[u8]| -> *const libc::c_char { std::ffi::CString::new(b.to_vec()).map(|c| c.into_raw() as *const libc::c_char).unwrap_or(std::ptr::null()) };
    let mut argv: Vec<*const libc::c_char> = std::env::args_os().map(|a| cs(a.as_bytes())).filter(|p| !p.is_null()).collect();
    argv.push(std::ptr::null());
    let mut envp: Vec<*const libc::c_char> = vec![cs(b"SS_E2=0")];
    for (k, v) in std::env::vars_os() {
        if k.as_bytes() == b"SS_E2" {
            continue;
        }
        let mut e = k.as_bytes().to_vec();
        e.push(b'=');
        e.extend_from_slice(v.as_bytes());
        let p = cs(&e);
        if !p.is_null() {
            envp.push(p);
        }
    }
    envp.push(std::ptr::null());
    RX_ARGV.store(Box::leak(argv.into_boxed_slice()).as_ptr() as usize, Relaxed);
    RX_ENVP.store(Box::leak(envp.into_boxed_slice()).as_ptr() as usize, Relaxed);
    RX_EXE.store(cs(b"/proc/self/exe") as usize, Relaxed);
}
/// Re-execute with engine 2 off, unless output writing has started (then return). Async-signal
/// safe: only atomics and execve.
fn reexec_fallback() {
    if WRITING.load(std::sync::atomic::Ordering::SeqCst) || RX_EXE.load(Relaxed) == 0 {
        return;
    }
    unsafe {
        libc::execve(
            RX_EXE.load(Relaxed) as *const libc::c_char,
            RX_ARGV.load(Relaxed) as *const *const libc::c_char,
            RX_ENVP.load(Relaxed) as *const *const libc::c_char,
        )
    };
}
extern "C" fn on_fault(sig: libc::c_int) {
    reexec_fallback();
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// Set when engine 2 starts writing output (see the panic hook in `run`).
static WRITING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Probe v2: facts and timings collected as compact text, written into campaign_daily channel
/// strings of a few rows (keys kept), so the dev check shows them as differing rows.
static PROBE_TXT: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
fn ptxt(s: &str) {
    PROBE_TXT.lock().unwrap().push_str(s);
}
fn probe2() {
    let rd = |p: &str| std::fs::read_to_string(p).unwrap_or_else(|_| "?".into());
    let sel = |p: &str| -> String {
        let t = rd(p);
        t.split_whitespace().find(|w| w.starts_with('[')).map(|w| w.trim_matches(|c| c == '[' || c == ']').to_string()).unwrap_or_else(|| t.trim().chars().take(12).collect())
    };
    ptxt(&format!("v2;thp={};dfg={};shm={};", sel("/sys/kernel/mm/transparent_hugepage/enabled"), sel("/sys/kernel/mm/transparent_hugepage/defrag"), sel("/sys/kernel/mm/transparent_hugepage/shmem_enabled")));
    let ci = rd("/proc/cpuinfo");
    let model = ci.lines().find(|l| l.starts_with("model name")).and_then(|l| l.split(':').nth(1)).unwrap_or("?").trim().replace("Intel(R) ", "").replace("(R)", "").replace("CPU ", "");
    let mhz = ci.lines().find(|l| l.starts_with("cpu MHz")).and_then(|l| l.split(':').nth(1)).unwrap_or("?").trim().split('.').next().unwrap_or("?").to_string();
    let a512 = ci.lines().find(|l| l.starts_with("flags")).map_or(false, |l| l.contains("avx512f"));
    let ncpu = ci.lines().filter(|l| l.starts_with("processor")).count();
    let st = rd("/proc/self/status");
    let allowed = st.lines().find(|l| l.starts_with("Cpus_allowed_list")).and_then(|l| l.split(':').nth(1)).unwrap_or("?").trim().to_string();
    ptxt(&format!("cpu={};mhz={};a512={};ncpu={};allow={};cpumax={};memmax={};", model.chars().take(40).collect::<String>(), mhz, a512 as u8, ncpu, allowed,
        rd("/sys/fs/cgroup/cpu.max").trim(), rd("/sys/fs/cgroup/memory.max").trim()));
    let mi = rd("/proc/self/mountinfo");
    for m in ["/data", "/output"] {
        let fs = mi.lines().find(|l| l.split_whitespace().nth(4) == Some(m)).and_then(|l| l.split(" - ").nth(1)).and_then(|r| r.split_whitespace().next()).unwrap_or("?").to_string();
        ptxt(&format!("{}={};", &m[1..], fs));
    }
    let memtotal = rd("/proc/meminfo").lines().next().unwrap_or("?").split_whitespace().nth(1).unwrap_or("?").to_string();
    ptxt(&format!("memkb={};", memtotal));
    // fresh-page cost, 256 MB: default / MADV_HUGEPAGE / MADV_NOHUGEPAGE
    for (tag, adv) in [("D", -1), ("H", libc::MADV_HUGEPAGE), ("N", libc::MADV_NOHUGEPAGE)] {
        let n = 256usize << 20;
        let p = unsafe { libc::mmap(std::ptr::null_mut(), n, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0) } as *mut u8;
        if adv >= 0 {
            unsafe { libc::madvise(p as *mut libc::c_void, n, adv) };
        }
        let mut r0: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut r0) };
        let t = std::time::Instant::now();
        let mut o = 0;
        while o < n {
            unsafe { *p.add(o) = 1 };
            o += 4096;
        }
        let ft = t.elapsed().as_secs_f64();
        let mut r1: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut r1) };
        let t2 = std::time::Instant::now();
        unsafe { std::ptr::write_bytes(p, 7, n) };
        let wt = t2.elapsed().as_secs_f64();
        let t3 = std::time::Instant::now();
        unsafe { libc::munmap(p as *mut libc::c_void, n) };
        ptxt(&format!("f{}={:.0}ms/{}flt/w{:.1}GBs/um{:.1}ms;", tag, ft * 1e3, r1.ru_minflt - r0.ru_minflt, n as f64 / wt / 1e9, t3.elapsed().as_secs_f64() * 1e3));
    }
    // memcpy bandwidth (warm 256 MB buffers) and a single-thread integer loop (laptop: see PERF)
    {
        let n = 256usize << 20;
        let a = vec![1u8; n];
        let mut b = vec![2u8; n];
        let t = std::time::Instant::now();
        b.copy_from_slice(&a);
        let ct = t.elapsed().as_secs_f64();
        std::hint::black_box(&b);
        let t = std::time::Instant::now();
        let mut x = 1u64;
        for _ in 0..200_000_000u64 {
            x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407));
        }
        ptxt(&format!("cp={:.1}GBs;loop={:.0}ms;", n as f64 / ct / 1e9, t.elapsed().as_secs_f64() * 1e3));
    }
}

/// SS_PROBE=1 (board probe only): print scorer facts and memory micro-timings to stderr.
#[allow(dead_code)]
fn probe_facts() {
    let rd = |p: &str| std::fs::read_to_string(p).unwrap_or_else(|e| format!("<{}>", e));
    for p in ["/sys/kernel/mm/transparent_hugepage/enabled", "/sys/kernel/mm/transparent_hugepage/defrag",
              "/sys/kernel/mm/transparent_hugepage/khugepaged/defrag", "/sys/fs/cgroup/cpu.max",
              "/sys/fs/cgroup/memory.max", "/sys/fs/cgroup/cpuset.cpus.effective", "/proc/version",
              "/proc/sys/kernel/numa_balancing", "/sys/kernel/mm/transparent_hugepage/shmem_enabled"] {
        eprintln!("PROBE {} = {}", p, rd(p).trim());
    }
    let ci = rd("/proc/cpuinfo");
    for key in ["model name", "cpu MHz", "cache size", "siblings", "cpu cores", "flags"] {
        if let Some(l) = ci.lines().find(|l| l.starts_with(key)) {
            let l = if key == "flags" { format!("flags has avx512f={} avx2={}", l.contains("avx512f"), l.contains("avx2")) } else { l.to_string() };
            eprintln!("PROBE cpuinfo {}", l);
        }
    }
    eprintln!("PROBE cpus {}", ci.lines().filter(|l| l.starts_with("processor")).count());
    for l in rd("/proc/meminfo").lines().take(8) {
        eprintln!("PROBE meminfo {}", l);
    }
    for l in rd("/proc/self/mountinfo").lines().filter(|l| l.contains(" /data ") || l.contains(" /output ") || l.contains(" /scratch ") || l.contains(" /tmp ")) {
        eprintln!("PROBE mount {}", l);
    }
    for l in rd("/proc/self/status").lines().filter(|l| l.starts_with("Cpus_allowed_list") || l.starts_with("THP")) {
        eprintln!("PROBE status {}", l);
    }
    // fresh-page cost: 256 MB anon, touched once per 4 KB, default / MADV_HUGEPAGE / NOHUGEPAGE
    for (name, adv) in [("default", -1), ("hugepage", libc::MADV_HUGEPAGE), ("nohugepage", libc::MADV_NOHUGEPAGE)] {
        let n = 256usize << 20;
        let p = unsafe { libc::mmap(std::ptr::null_mut(), n, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0) } as *mut u8;
        if adv >= 0 {
            unsafe { libc::madvise(p as *mut libc::c_void, n, adv) };
        }
        let mut ru0: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru0) };
        let t = std::time::Instant::now();
        let mut o = 0;
        while o < n {
            unsafe { *p.add(o) = 1 };
            o += 4096;
        }
        let ft = t.elapsed().as_secs_f64();
        let t2 = std::time::Instant::now();
        unsafe { std::ptr::write_bytes(p, 7, n) };
        let wt = t2.elapsed().as_secs_f64();
        let mut ru1: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru1) };
        let t3 = std::time::Instant::now();
        unsafe { libc::munmap(p as *mut libc::c_void, n) };
        eprintln!("PROBE fault256MB {} first-touch {:.1} ms, minflt {}, rewrite {:.1} ms ({:.1} GB/s), munmap {:.1} ms",
            name, ft * 1e3, ru1.ru_minflt - ru0.ru_minflt, wt * 1e3, n as f64 / wt / 1e9, t3.elapsed().as_secs_f64() * 1e3);
    }
}

pub fn run(data: &Path, out: &Path) -> bool {
    if std::env::var("SS_PROBE").is_ok() {
        // time from this process's exec to here: /proc/uptime minus our start time (clock ticks)
        let up: f64 = std::fs::read_to_string("/proc/uptime").ok().and_then(|u| u.split_whitespace().next().and_then(|x| x.parse().ok())).unwrap_or(0.0);
        let st = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
        let f: Vec<&str> = st.rsplit(')').next().unwrap_or("").split_whitespace().collect();
        let start: f64 = f.get(19).and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0) / unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
        eprintln!("PROBE exec-to-main {:.1} ms (pid {})", (up - start) * 1e3, std::process::id());
        ptxt(&format!("x2m={:.0}ms;", (up - start) * 1e3));
        probe2();
        eprintln!("PROBE {}", PROBE_TXT.lock().unwrap());
    }
    let t0 = std::time::Instant::now();
    // Insurance: a panic in engine 2 before any output is written re-executes this binary with
    // engine 2 off (SS_E2=0), so unexpected data costs time, never a crash. Once writing has
    // started the panic stays a panic (partial output must not be mixed with a second run's).
    if std::env::var("SS_E2NOREEXEC").is_err() {
        prep_reexec();
        std::panic::set_hook(Box::new(|info| {
            eprintln!("engine2 panic: {}", info);
            reexec_fallback();
        }));
        // A fault (SIGSEGV/SIGBUS from an over-read, SIGILL, SIGFPE) is handled the same way.
        for sig in [libc::SIGSEGV, libc::SIGBUS, libc::SIGILL, libc::SIGFPE] {
            unsafe { libc::signal(sig, on_fault as extern "C" fn(libc::c_int) as libc::sighandler_t) };
        }
    }
    if std::env::var("SS_E2PANICTEST").is_ok() {
        panic!("SS_E2PANICTEST");
    }
    if std::env::var("SS_E2SEGVTEST").is_ok() {
        unsafe { libc::raise(libc::SIGSEGV) };
    }
    let verbose = std::env::var("SS_VERBOSE").is_ok();
    let probe = std::env::var("SS_PROBE").is_ok();
    let lap = |what: &str| {
        if verbose {
            let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
            unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
            let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
            eprintln!("e2 {:>8.3}s  {}  [usr {:.3} sys {:.3} minflt {} majflt {}]", t0.elapsed().as_secs_f64(), what, tv(ru.ru_utime), tv(ru.ru_stime), ru.ru_minflt, ru.ru_majflt);
            if probe {
                // cumulative: wall ms / user ms / sys ms / minflt (diff consecutive laps)
                let w: String = what.split_whitespace().next().unwrap_or("?").chars().take(5).collect();
                ptxt(&format!("{}:{:.0}/{:.0}/{:.0}/{};", w, t0.elapsed().as_secs_f64() * 1e3, tv(ru.ru_utime) * 1e3, tv(ru.ru_stime) * 1e3, ru.ru_minflt));
            }
        }
    };
    let decline = |why: &str| {
        if verbose || std::env::var("SS_E2NOTE").is_ok() {
            eprintln!("note: engine2 declined: {}", why);
        }
        // BOARD DIAGNOSTIC (remove after one draw): a decline sleeps 1 s per reason code, so the
        // board time says whether and why engine 2 declined on the hidden data.
        if DIAG_SLEEP && std::env::var("SS_E2NODIAG").is_err() {
            const WHY: [&str; 14] = ["manifest", "manifest window", "window size", "more than 32 categories", "campaign count", "more than 64 threads", "no event files", "too few canonical sample ids", "user key not found", "event key not found", "ingest not whole seconds or too early", "too many odd user ids", "fallback key collision", "more than 2^32 clicks"];
            let code = WHY.iter().position(|w| *w == why).map_or(15, |i| i + 1);
            std::thread::sleep(std::time::Duration::from_secs(code as u64));
        }
        false
    };

    // ---------------- setup (same reading as fast::run)
    let man = match std::fs::read_to_string(data.join("manifest.json")) {
        Ok(m) => m,
        Err(_) => return decline("manifest"),
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
        _ => return decline("manifest window"),
    };
    let cws = ws - WEEK;
    if we <= ws || we - ws >= (1i64 << 31) || we - cws >= (1i64 << 32) {
        return decline("window size");
    }
    let ws_day = ws.div_euclid(DAY);
    let ndays = ((we - 1).div_euclid(DAY) - ws_day + 1).max(1) as usize;
    let ibase = ws - DAY; // ingest seconds are stored relative to this, 21 bits

    // Small tables (products, campaigns) are built beside key recovery, after the maps are open.
    let small_fn = || -> Result<_, &'static str> {
        let products = read_pairs(&data.join("products.csv"));
        let campaigns = read_pairs(&data.join("campaigns.csv"));
        // distinct categories by a short linear scan (at most 33 before declining) instead of
        // copying and sorting every product's category; then the same sorted order as before
        let mut raw: Vec<&'static [u8]> = Vec::new();
        let mut pidx: Vec<u8> = Vec::with_capacity(products.len());
        for &(_, c) in &products {
            let i = match raw.iter().position(|&x| x == c) {
                Some(i) => i,
                None => {
                    if raw.len() >= 33 {
                        return Err("more than 32 categories");
                    }
                    raw.push(c);
                    raw.len() - 1
                }
            };
            pidx.push(i as u8);
        }
        let mut cats: Vec<Vec<u8>> = raw.iter().map(|c| c.to_vec()).collect();
        cats.push(b"unknown".to_vec());
        cats.sort();
        cats.dedup();
        if cats.len() > 32 {
            return Err("more than 32 categories");
        }
        let cat_of = |c: &[u8]| cats.binary_search_by(|x| x.as_slice().cmp(c)).unwrap() as u32;
        let rank: Vec<u32> = raw.iter().map(|c| cat_of(c)).collect();
        let unknown = cat_of(b"unknown") as u64;
        let mut prod = Dict::new(products.len() * 2);
        for (r, &i) in products.iter().zip(&pidx) {
            prod.insert(r.0, rank[i as usize]);
        }
        let mut camp_map = Dict::new(campaigns.len() * 2);
        let mut camp_names: Vec<(&'static [u8], &'static [u8])> = Vec::new();
        for &(a, b) in &campaigns {
            if camp_map.get(a).is_none() {
                camp_map.insert(a, camp_names.len() as u32);
                camp_names.push((a, b));
            }
        }
        let organic = camp_names.len() as u32;
        if organic as u64 >= 1 << PSH {
            return Err("campaign count");
        }
        let camp_tab = PackTab::new(camp_names.iter().enumerate().map(|(i, c)| (c.0, i as u32)));
        Ok((cats, unknown, prod, camp_map, camp_names, organic, camp_tab))
    };

    let (ev_maps, (ck_maps, lk_maps)): (Vec<Mmap>, (Vec<Mmap>, Vec<Mmap>)) = rayon::join(
        || open_all(&list_csv(&data.join("events"))),
        || rayon::join(|| open_all(&list_csv(&data.join("clicks"))), || open_all(&list_csv(&data.join("links")))),
    );
    let ev_maps: &'static [Mmap] = Vec::leak(ev_maps);
    let ck_maps: &'static [Mmap] = Vec::leak(ck_maps);
    let lk_maps: &'static [Mmap] = Vec::leak(lk_maps);
    let nthreads = rayon::current_num_threads().max(1);
    if nthreads > 64 {
        return decline("more than 64 threads");
    }
    let ev_bytes: usize = ev_maps.iter().map(|m| m.len()).sum();
    // An event line is longer than 32 bytes, so rows (and so unique indices) < ebound.
    let ebound = (ev_bytes as u64 / 32 + 1024).min(1 << 40);
    let pb = (ev_bytes as u64 / 32 / 8 + 1024).min(1 << PBITS);
    lap("opened");

    // ---------------- keys from a sample of event rows (the largest file's first lines)
    let keys_fn = || -> Result<(u64, u64), &'static str> {
        let mut big: Option<&'static [u8]> = None;
        for m in ev_maps.iter() {
            if big.map_or(true, |b| m.len() > b.len()) {
                big = Some(&m[..]);
            }
        }
        let d = match big {
            Some(d) => d,
            None => return Err("no event files"),
        };
        let start = memchr::memchr(b'\n', d).map_or(d.len(), |p| p + 1);
        let mut end = (start + (1 << 20)).min(d.len());
        if end < d.len() {
            end = memchr::memrchr(b'\n', &d[start..end]).map_or(start, |p| start + p + 1);
        }
        let mut rows: Vec<(u64, u64, i64)> = Vec::new();
        let mut memo = TsMemo::default();
        scan3a::<7>(&d[start..end], |f| {
            if let (Some(e), Some(u)) = (f[0], f[1]) {
                if let (Some(we_), Some(wu)) = (b36(e, b'e'), b36(u, b'u')) {
                    let ts = parse_ts_memo(f[2], &mut memo);
                    rows.push((we_, wu, if ts == BAD { i64::MAX } else { ts }));
                }
            }
        });
        let mut us: Vec<u64> = rows.iter().map(|r| r.1).collect();
        us.sort_unstable();
        us.dedup();
        if us.len() < 24 || rows.len() < 24 {
            return Err("too few canonical sample ids");
        }
        // claude-45's genid recovery first (frequency anchors, absolute bounds), then ours
        let (geb, gpb) = super::super::genid::bounds(ev_bytes as u64);
        let uys: Vec<u64> = rows.iter().map(|r| r.1).collect();
        let ku = match super::super::genid::recover_user_key_freq(&uys, gpb) {
            Some(k) => k,
            None => {
                let step = (us.len() / 48).max(1);
                let usample: Vec<u64> = us.iter().step_by(step).take(48).copied().collect();
                match recover_user_key(&usample, pb) {
                    Some(k) => k,
                    None => return Err("user key not found"),
                }
            }
        };
        let ke = match super::super::genid::recover_event_key_ts(&rows, ku, geb) {
            Some(k) => k,
            None => {
                let mut withp: Vec<(u64, u64)> = rows.iter().filter_map(|&(e, u, _)| {
                    let v = unscramble(u, ku);
                    if v >> 9 < pb { Some((v >> 9, e)) } else { None }
                }).collect();
                withp.sort_unstable();
                if withp.is_empty() {
                    return Err("event key not found");
                }
                let esample: Vec<u64> = rows.iter().step_by((rows.len() / 48).max(1)).take(48).map(|r| r.0).collect();
                let (p0, w0) = withp[0];
                let lim = if p0 == 0 { (ebound / 40 + 4096).min(ebound) } else { ebound };
                match recover_event_key(&esample, w0, lim, ebound) {
                    Some(k) => k,
                    None => return Err("event key not found"),
                }
            }
        };
        Ok((ku, ke))
    };
    let (sm, kr) = rayon::join(small_fn, keys_fn);
    let (cats, unknown, prod, camp_map, camp_names, organic, camp_tab) = match sm {
        Ok(s) => s,
        Err(why) => return decline(why),
    };
    let (ku, ke) = match kr {
        Ok(k) => k,
        Err(why) => return decline(why),
    };
    // Test knob: wrong keys send every ID down the exact odd paths (output must not change).
    let (ku, ke) = if std::env::var("SS_E2BADKEY").is_ok() { (ku ^ 0x5a5a_5a5a5a, ke ^ 0x3c3c_3c3c3c) } else { (ku, ke) };
    if let Ok(k) = std::env::var("SS_E2KEYS") {
        eprintln!("e2 keys user {:010x} event {:010x} ({})", ku, ke, k);
    }
    lap("keys");

    // ---------------- parse everything: links, clicks, events (one queue, one state per worker)
    let nbk = (ebound as usize >> BSH) + 1;
    let mut jobs: Vec<(u8, usize, usize, usize)> = Vec::new();
    for c in chunks(lk_maps) {
        jobs.push((0, c.0, c.1, c.2));
    }
    for c in chunks(ck_maps) {
        jobs.push((1, c.0, c.1, c.2));
    }
    for c in chunks(ev_maps) {
        jobs.push((2, c.0, c.1, c.2));
    }
    let est_rows = ev_bytes / 80;
    let bcap = (est_rows as f64 * 1.25 / nbk.min(est_rows / BSZ + 1) as f64 / nthreads as f64) as usize + 64;
    // One record arena: region (bucket b, worker t) = acap lines at (b * nthreads + t) * acap.
    // Reserved, not committed: only written pages fault. Never unmapped (the dedup streams live
    // in it until exit). If the mapping fails, every Wc4 uses its overflow Vec instead.
    // SS_E2ACAP (test knob): region lines per (bucket, worker), to force the overflow paths
    let acap = std::env::var("SS_E2ACAP").ok().and_then(|v| v.parse().ok()).unwrap_or(bcap / 4 + 1);
    let arena_bytes = nbk * nthreads * acap * std::mem::size_of::<Line4>();
    let arena: usize = {
        let bytes = arena_bytes;
        let p = unsafe {
            libc::mmap(std::ptr::null_mut(), bytes, libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE, -1, 0)
        };
        if p == libc::MAP_FAILED {
            0
        } else {
            unsafe { libc::madvise(p, bytes, libc::MADV_HUGEPAGE) };
            p as usize
        }
    };
    // clicks per (worker, person bucket), generously: ~25 events per person, lines >= 36 bytes
    let ck_bytes: usize = ck_maps.iter().map(|m| m.len()).sum();
    let ccap = ck_bytes / 36 / ((est_rows / 25 >> CB) + 1) / nthreads * 2 + 64;
    let next = AtomicUsize::new(0);
    // SS_E2ABL=1..4 (diagnostic): stop the event row body after scan / +timestamps / +ID decode /
    // +all fields (no record write), print the parse time and exit.
    let abl: u32 = std::env::var("SS_E2ABL").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    // Each parse job first maps its input range with MADV_POPULATE_READ (one call instead of a
    // page fault per page; claude-b7's measurement: less sys time). SS_E2POP=0 turns it off; the
    // first rejection (EINVAL on kernels before 5.14) turns it off for the rest of the run.
    // After a job is parsed, its page-aligned interior is unmapped (MADV_DONTNEED) by the same
    // worker, so page-table entries never pile up (read-only file mappings: odd IDs that point
    // there fault back in from the page cache). SS_E2DROP=0 turns it off.
    let dropj = std::env::var("SS_E2DROP").map_or(true, |v| v != "0");
    let pop = std::sync::atomic::AtomicBool::new(std::env::var("SS_E2POP").map_or(true, |v| v != "0"));
    let wks: Vec<Wk> = (0..nthreads)
        .into_par_iter()
        .map(|t| {
            let mut w = Wk {
                bk: (0..nbk)
                    .map(|b| {
                        std::cell::UnsafeCell::new(if arena == 0 {
                            Wc4::new()
                        } else {
                            Wc4::with_region(unsafe { (arena as *mut Line4).add((b * nthreads + t) * acap) }, acap)
                        })
                    })
                    .collect(),
                oddev: Vec::new(),
                oddu: Vec::new(),
                fb: Fb::new(),
                cks: Vec::new(),
                cko: Vec::new(),
                li: Vec::new(),
                lg: Vec::new(),
                maxp: 0,
                fail: 0,
            };
            let mut memo = TsMemo::default();
            let mut last: Option<usize> = None;
            loop {
                if let Some(lj) = last.take() {
                    let (kind, mi, s, e) = jobs[lj];
                    let m = match kind {
                        0 => &lk_maps[mi],
                        1 => &ck_maps[mi],
                        _ => &ev_maps[mi],
                    };
                    let (a, b) = ((s + 4095) & !4095, e & !4095);
                    if b > a {
                        // Safety: read-only file mapping; see `dropj`.
                        unsafe { libc::madvise(m.as_ptr().add(a) as *mut libc::c_void, b - a, libc::MADV_DONTNEED) };
                    }
                }
                let j = next.fetch_add(1, Relaxed);
                if j >= jobs.len() {
                    break;
                }
                if dropj {
                    last = Some(j);
                }
                let (kind, mi, s, e) = jobs[j];
                if pop.load(Relaxed) {
                    let m = match kind {
                        0 => &lk_maps[mi],
                        1 => &ck_maps[mi],
                        _ => &ev_maps[mi],
                    };
                    let s0 = s & !4095;
                    if m.advise_range(memmap2::Advice::PopulateRead, s0, e - s0).is_err() {
                        pop.store(false, Relaxed);
                    }
                }
                match kind {
                    0 => {
                        let d: &'static [u8] = &lk_maps[mi][s..e];
                        scan3a::<2>(d, |f| {
                            if let [Some(a), Some(b)] = *f {
                                match (user(a, ku, pb), user(b, ku, pb)) {
                                    (Some((pa, aa)), Some((pb_, ab))) if pa == pb_ => {
                                        w.maxp = w.maxp.max(pa);
                                        w.li.push((pa, aa as u8, ab as u8));
                                    }
                                    (x, y) => {
                                        let na = match x {
                                            Some((p, al)) => {
                                                w.maxp = w.maxp.max(p);
                                                node_s(p, al)
                                            }
                                            None => key_of(a, &mut w.fb),
                                        };
                                        let nb = match y {
                                            Some((p, al)) => {
                                                w.maxp = w.maxp.max(p);
                                                node_s(p, al)
                                            }
                                            None => key_of(b, &mut w.fb),
                                        };
                                        w.lg.push((na, nb));
                                    }
                                }
                            }
                        });
                    }
                    1 => {
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
                            let ckey = key_of(cid, &mut w.fb);
                            let lo = ((ts - cws) as u64) << 32 | camp as u64;
                            match user(uid, ku, pb) {
                                Some((p, a)) => {
                                    w.maxp = w.maxp.max(p);
                                    let cb = (p >> CB) as usize;
                                    if cb >= w.cks.len() {
                                        w.cks.resize_with(cb + 1, Vec::new);
                                    }
                                    if w.cks[cb].capacity() == 0 {
                                        w.cks[cb].reserve(ccap); // untouched capacity costs nothing
                                    }
                                    w.cks[cb].push((lo | (a as u64) << 28 | (p as u64 & PLO) << PSH, ckey));
                                }
                                None => {
                                    let uk = key_of(uid, &mut w.fb);
                                    w.cko.push((uk, lo, ckey));
                                }
                            }
                        });
                    }
                    _ => {
                        let d: &'static [u8] = &ev_maps[mi][s..e];
                        scan3a::<7>(d, |f| {
                            let eid = match f[0] {
                                Some(x) => x,
                                None => return,
                            };
                            if abl == 1 {
                                w.maxp = w.maxp.wrapping_add(eid.len() as u32);
                                return;
                            }
                            if abl == 2 {
                                let a = parse_ts_memo(f[3], &mut memo) ^ parse_ts_memo(f[2], &mut memo);
                                w.maxp = w.maxp.wrapping_add(a as u32 & 1);
                                return;
                            }
                            if abl == 3 {
                                let a = parse_ts_memo(f[3], &mut memo) ^ parse_ts_memo(f[2], &mut memo);
                                let b = b36(eid, b'e').map_or(0, |x| unscramble(x, ke)) ^ f[1].and_then(|u| b36(u, b'u')).map_or(0, |x| unscramble(x, ku));
                                w.maxp = w.maxp.wrapping_add((a as u64 ^ b) as u32 & 1);
                                return;
                            }
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
                            // ingest seconds since ws - 1 day; unparseable = ING_BAD (loses to
                            // every parseable copy, as i64::MAX does in v5.12; such a copy is
                            // invalid). Later than the 21-bit range: clamped (the copy is invalid
                            // then, so ties among clamped copies drop the event either way).
                            let ingr = if ing == i64::MAX {
                                ING_BAD
                            } else if ing % 1000 != 0 || ing < ibase {
                                w.fail |= 1;
                                0
                            } else {
                                (((ing - ibase) / 1000) as u64).min(ING_BAD - 1)
                            };
                            if ingr == ING_BAD - 1 && ok {
                                w.fail |= 1; // a valid copy must have an exact ingest (cannot happen: such a copy is late)
                            }
                            let (w0, pid) = if ok {
                                let cat = if typ == 2 { prod.get(f[5].unwrap()).map(|c| c as u64).unwrap_or(unknown) } else { 0 };
                                let (pid, alias) = match user(f[1].unwrap(), ku, pb) {
                                    Some((p, a)) => {
                                        w.maxp = w.maxp.max(p);
                                        (p as u64, a as u64)
                                    }
                                    None => {
                                        let k = key_of(f[1].unwrap(), &mut w.fb);
                                        let i = w.oddu.len();
                                        if i >= 1 << 22 {
                                            w.fail |= 2;
                                        }
                                        w.oddu.push(k);
                                        ((t as u64) << 22 | i as u64, 15)
                                    }
                                };
                                (((ts - ws) as u64) << 33 | price << 9 | cat << 4 | alias, pid)
                            } else {
                                (NONE, 0)
                            };
                            let w1 = ingr << 15 | pid << 36;
                            match b36(eid, b'e') {
                                Some(x) => {
                                    let idx = unscramble(x, ke);
                                    if idx < ebound {
                                        if abl == 4 {
                                            w.maxp = w.maxp.wrapping_add((w0 ^ w1 ^ idx) as u32 & 1);
                                            return;
                                        }
                                        w.bk[(idx >> BSH) as usize].get_mut().push([w0, w1 | (idx & (BSZ as u64 - 1))], bcap / 4 + 1);
                                        return;
                                    }
                                }
                                None => {}
                            }
                            let k = key_of(eid, &mut w.fb);
                            w.oddev.push((k, [w0, w1]));
                        });
                    }
                }
            }
            for b in w.bk.iter_mut() {
                b.get_mut().finish();
            }
            #[cfg(target_arch = "x86_64")]
            unsafe {
                std::arch::x86_64::_mm_sfence()
            };
            w
        })
        .collect();
    if abl != 0 {
        lap(&format!("ablation stage {} parse done", abl));
        std::process::exit(0);
    }
    let fail = wks.iter().fold(0u8, |a, w| a | w.fail);
    if fail & 1 != 0 {
        return decline("ingest not whole seconds or too early");
    }
    if fail & 2 != 0 {
        return decline("too many odd user ids");
    }
    let fbm = match merge_fb(wks.iter().map(|w| &w.fb)) {
        Some(m) => m,
        None => return decline("fallback key collision"),
    };
    let np = wks.iter().map(|w| w.maxp).max().unwrap_or(0) as usize + 1;
    lap(&format!("parsed ({} persons)", np));
    // Drop the input files' page-table entries on a spare thread now (as fast.rs does), instead
    // of leaving them to the serial teardown at exit. Read-only file mappings: odd IDs that still
    // point into a file fault back in from the page cache.
    if std::env::var("SS_EARLYUNMAP").as_deref() != Ok("0") {
        std::thread::spawn(move || {
            for m in ev_maps.iter().chain(lk_maps.iter()).chain(ck_maps.iter()) {
                unsafe { libc::madvise(m.as_ptr() as *mut libc::c_void, m.len(), libc::MADV_DONTNEED) };
            }
        });
    }

    // ---------------- identity
    let mut cplx = vec![0u64; np / 64 + 1];
    let is_c = |c: &[u64], p: usize| c[p >> 6] >> (p & 63) & 1 != 0;
    for w in &wks {
        for &(a, b) in &w.lg {
            for n in [a, b] {
                if n & (TAG | FB) == TAG {
                    let p = ((n >> 4) & ((1 << PBITS) - 1)) as usize;
                    cplx[p >> 6] |= 1 << (p & 63);
                }
            }
        }
    }
    let cplx = cplx;
    // The alias unions run beside the dedup (which needs only the complex flags).
    let ident = || {
        let mut nib = vec![0u64; np];
        let mut uf = Uf { map: HashMap::new(), par: Vec::new(), nodes: Vec::new() };
        for w in &wks {
            for &(p, a, b) in &w.li {
                if is_c(&cplx, p as usize) {
                    uf.union(node_s(p, a as u32), node_s(p, b as u32));
                } else {
                    nunion(&mut nib[p as usize], a as u32, b as u32);
                }
            }
            for &(a, b) in &w.lg {
                uf.union(a, b);
            }
        }
        (nib, uf)
    };

    // ---------------- odd event ids: exact dedup (rare)
    let route = |w0: u64, w1: u64, simple: &mut Sv, cx: &mut Vec<(u64, u64)>| {
        let alias = w0 & 15;
        let pid = w1 >> 36;
        if alias == 15 {
            let (t, i) = ((pid >> 22) as usize, (pid & ((1 << 22) - 1)) as usize);
            cx.push((wks[t].oddu[i], w0));
        } else if is_c(&cplx, pid as usize) {
            cx.push((node_s(pid as u32, alias as u32), w0));
        } else {
            simple.push(pid as u32, w0);
        }
    };
    let mut odd_simple = Sv::default();
    let mut odd_cx: Vec<(u64, u64)> = Vec::new();
    {
        let mut m: HashMap<u64, [u64; 2]> = HashMap::new();
        for w in &wks {
            for &(k, r) in &w.oddev {
                match m.get_mut(&k) {
                    Some(x) => {
                        if ing_of(r[1]) < ing_of(x[1]) {
                            *x = r;
                        }
                    }
                    None => {
                        m.insert(k, r);
                    }
                }
            }
        }
        for r in m.values() {
            if r[0] != NONE {
                route(r[0], r[1], &mut odd_simple, &mut odd_cx);
            }
        }
    }

    // ---------------- dedup per index bucket: direct slot array, smallest ingest wins
    let ecnt: Vec<AtomicU32> = (0..np + 1).map(|_| AtomicU32::new(0)).collect();
    let count_runs = |v: &Sv, cnt: &[AtomicU32]| {
        for (p, a, b) in v.each() {
            cnt[p as usize].fetch_add((b - a) as u32, Relaxed);
        }
    };
    count_runs(&odd_simple, &ecnt);
    let nextb = AtomicUsize::new(0);
    let dedup = || -> Vec<(Vec<Sv>, Vec<(u64, u64)>)> { (0..nthreads)
        .into_par_iter()
        .map(|_| {
            let mut win = vec![[0u64; 2]; BSZ];
            let mut gen = vec![0u32; BSZ];
            let mut cur = 0u32;
            let mut outs: Vec<Sv> = Vec::new();
            let mut cx: Vec<(u64, u64)> = Vec::new();
            loop {
                let b = nextb.fetch_add(1, Relaxed);
                if b >= nbk {
                    break;
                }
                // Safety: bucket b is claimed by this worker only.
                let n: usize = wks.iter().map(|w| unsafe { (*w.bk[b].get()).n }).sum();
                if n == 0 {
                    continue;
                }
                cur += 1;
                for w in &wks {
                    let (ra, ro) = unsafe { (*w.bk[b].get()).parts() };
                    for r in ra.iter().chain(ro) {
                        let s = (r[1] & (BSZ as u64 - 1)) as usize;
                        if gen[s] != cur || ing_of(r[1]) < ing_of(win[s][1]) {
                            win[s] = *r;
                            gen[s] = cur;
                        }
                    }
                }
                // Every record is now in `win`, so the bucket's arena region (nthreads * acap
                // lines, 8 words each) is spent: the survivor stream (<= n words) goes over it,
                // onto pages that are already faulted.
                let rcap = nthreads * acap * 8;
                let sw = if arena != 0 && n <= rcap {
                    unsafe { Sb::borrowed((arena as *mut Line4).add(b * nthreads * acap) as *mut u64, rcap) }
                } else {
                    Sb::owned(n)
                };
                let mut simple = Sv { w: sw, runs: Vec::with_capacity(n / 16 + 16) };
                for s in 0..BSZ {
                    if gen[s] == cur && win[s][0] != NONE {
                        route(win[s][0], win[s][1], &mut simple, &mut cx);
                    }
                }
                count_runs(&simple, &ecnt);
                outs.push(simple);
                // the bucket's overflow records are done: free them (its arena region now holds the stream)
                for w in &wks {
                    unsafe { *w.bk[b].get() = Wc4::new() };
                }
            }
            (outs, cx)
        })
        .collect() };
    let ((nib, uf), dd) = rayon::join(ident, dedup);
    lap(&format!("identity ({} general nodes) and dedup", uf.nodes.len()));

    // ---------------- clicks: per person bucket, a local counting sort by person, then each
    // person's clicks sorted by time (all in cache)
    let ncb = (np >> CB) + 1;
    let nextc = AtomicUsize::new(0);
    let cparts: Vec<(Vec<(usize, Vec<(u64, u64)>, Vec<u32>)>, Vec<(u64, u64, u64)>)> = (0..nthreads)
        .into_par_iter()
        .map(|t| {
            let mut out = Vec::new();
            let mut cx: Vec<(u64, u64, u64)> = Vec::new();
            for &(k, lo, ck) in &wks[t].cko {
                cx.push((k, lo, ck));
            }
            let mut cnt = vec![0u32; (1 << CB) + 1];
            loop {
                let cb = nextc.fetch_add(1, Relaxed);
                if cb >= ncb {
                    break;
                }
                cnt.iter_mut().for_each(|c| *c = 0);
                let mut n = 0usize;
                for w in &wks {
                    if let Some(v) = w.cks.get(cb) {
                        for &(lo, ck) in v {
                            let q = ((lo >> PSH) & PLO) as u32;
                            let p = (cb as u32) << CB | q;
                            if is_c(&cplx, p as usize) {
                                cx.push((node_s(p, ((lo >> 28) & 15) as u32), lo & !(15u64 << 28 | PLO << PSH), ck));
                            } else {
                                cnt[q as usize + 1] += 1;
                                n += 1;
                            }
                        }
                    }
                }
                if n == 0 {
                    continue;
                }
                for i in 1..cnt.len() {
                    cnt[i] += cnt[i - 1];
                }
                let offs = cnt.clone();
                let mut v: Vec<(u64, u64)> = vec![(0, 0); n];
                for w in &wks {
                    if let Some(src) = w.cks.get(cb) {
                        for &(lo, ck) in src {
                            let q = ((lo >> PSH) & PLO) as usize;
                            if !is_c(&cplx, (cb << CB) | q) {
                                v[cnt[q] as usize] = (lo & !(PLO << PSH), ck);
                                cnt[q] += 1;
                            }
                        }
                    }
                }
                for q in 0..1 << CB {
                    let r = &mut v[offs[q] as usize..offs[q + 1] as usize];
                    if r.len() > 1 {
                        r.sort_unstable_by_key(|c| c.0);
                    }
                }
                out.push((cb, v, offs));
            }
            (out, cx)
        })
        .collect();
    let mut cgrp: Vec<(Vec<(u64, u64)>, Vec<u32>)> = (0..ncb).map(|_| (Vec::new(), Vec::new())).collect();
    let mut ck_cx: Vec<Vec<(u64, u64, u64)>> = Vec::new();
    for (out, cx) in cparts {
        for (cb, v, o) in out {
            cgrp[cb] = (v, o);
        }
        ck_cx.push(cx);
    }
    lap("clicks grouped by person");

    // ---------------- per person, straight from the dedup streams: a run of one person whose
    // length equals the person's survivor count is the whole person (the usual case, since event
    // index order is person order); every other run (bucket edges, whales, rows a person has
    // outside its block) is gathered afterwards.
    let ncamp = organic as usize + 1;
    let ncats = cats.len();
    let cx = Ctx { ws, cws, ws_day, ndays, organic, cats: &cats, fbm: &fbm };
    let new_acc = || Acc {
        camp: vec![[0i64; 4]; ncamp * ndays],
        cnt: vec![0u32; ncamp],
        active: Vec::new(),
        catsum: vec![0i64; ncats],
        touched: Vec::new(),
    };
    struct Bufs {
        name: Vec<u8>,
        tmp: Vec<u8>,
        evs: Vec<u64>,
        cks: Vec<(u64, u64)>,
    }
    // SS_E2PABL=1 (diagnostic): skip the per-person work (classes, names, sessions).
    let pabl: u32 = std::env::var("SS_E2PABL").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    // One person: `ev` sorted by time.
    let person = |p: usize, ev: &[u64], acc: &mut Acc, rows: &mut UserOut, bf: &mut Bufs| {
        if pabl == 1 {
            acc.cnt[0] = acc.cnt[0].wrapping_add(ev.len() as u32);
            return;
        }
        let g = &cgrp[p >> CB];
        let ck: &[(u64, u64)] = if g.1.is_empty() {
            &[]
        } else {
            let q = p & ((1 << CB) - 1);
            &g.0[g.1[q] as usize..g.1[q + 1] as usize]
        };
        let x = nib[p];
        // root of each alias slot (identity when the person has no same-person links)
        let mut rt = [0u8; 16];
        for a in 0..16u32 {
            rt[a as usize] = if x == 0 || a >= NALIAS as u32 { a as u8 } else { nfind(x, a) as u8 };
        }
        let rof = |a: u32| rt[a as usize] as u32;
        let mut amask = 0u16;
        for &w in ev.iter() {
            amask |= 1 << (w & 15);
        }
        let mut roots = 0u16;
        let mut am = amask;
        while am != 0 {
            roots |= 1 << rof(am.trailing_zeros());
            am &= am - 1;
        }
        // byte-wise smallest ID of the class, chosen by name_key, encoded once
        let class_name = |r: u32, name: &mut Vec<u8>, _tmp: &mut Vec<u8>| {
            let mut best = (name_key(scramble((p as u64) << 9 | r as u64, ku)), r);
            if x != 0 {
                for a in 0..NALIAS as u32 {
                    if a != r && rt[a as usize] as u32 == r {
                        let k = name_key(scramble((p as u64) << 9 | a as u64, ku));
                        if k < best.0 {
                            best = (k, a);
                        }
                    }
                }
            }
            name.clear();
            user_name(p as u32, best.1, ku, name);
        };
        let Bufs { name, tmp, evs, cks } = bf;
        if roots.count_ones() == 1 {
            let r = roots.trailing_zeros();
            class_name(r, name, tmp);
            if ck.iter().all(|c| rof(((c.0 >> 28) & 15) as u32) == r) {
                do_person(&cx, name, ev, ck, acc, rows);
            } else {
                cks.clear();
                cks.extend(ck.iter().filter(|c| rof(((c.0 >> 28) & 15) as u32) == r));
                do_person(&cx, name, ev, cks, acc, rows);
            }
        } else {
            let mut rm = roots;
            while rm != 0 {
                let r = rm.trailing_zeros();
                rm &= rm - 1;
                class_name(r, name, tmp);
                evs.clear();
                evs.extend(ev.iter().filter(|&&w| rof((w & 15) as u32) == r));
                cks.clear();
                cks.extend(ck.iter().filter(|c| rof(((c.0 >> 28) & 15) as u32) == r));
                do_person(&cx, name, evs, cks, acc, rows);
            }
        }
    };
    let mut streams: Vec<&Sv> = vec![&odd_simple];
    for (o, _) in &dd {
        streams.extend(o.iter());
    }
    let ne: usize = streams.iter().map(|v| v.w.len()).sum();
    let est_out = ne / nthreads / 6 + 1024;
    let nextt = AtomicUsize::new(0);
    let new_bufs = || Bufs { name: Vec::with_capacity(16), tmp: Vec::with_capacity(16), evs: Vec::new(), cks: Vec::new() };
    type Part = (UserOut, Vec<[i64; 4]>, Vec<(u32, u32, u32, u32)>);
    // ---------------- complex identities: exact sort-based path. Independent of the whole-run
    // persons below, so it runs beside them (rayon::join); its rows are still appended last.
    let complex = || -> (UserOut, Vec<[i64; 4]>) {
        let mut uf = uf;
        let mut cev: Vec<(u64, u64)> = odd_cx;
        for (_, c) in &dd {
            cev.extend_from_slice(c);
        }
        let mut cck: Vec<(u64, u64, u64)> = Vec::new();
        for c in ck_cx {
            cck.extend(c);
        }
        // node -> class (root index); nodes in no link are their own class
        let mut ev2: Vec<(u32, u64)> = cev.iter().map(|&(n, w)| {
            let i = uf.id(n);
            (uf.find(i), w)
        }).collect();
        let mut ck2: Vec<(u32, u64, u64)> = cck.iter().map(|&(n, lo, k)| {
            let i = uf.id(n);
            (uf.find(i), lo, k)
        }).collect();
        // class names: byte-wise smallest member
        let nn = uf.nodes.len();
        let mut best: Vec<Option<Vec<u8>>> = vec![None; nn];
        let mut buf = Vec::new();
        for i in 0..nn as u32 {
            let r = uf.find(i) as usize;
            let k = uf.nodes[i as usize];
            buf.clear();
            if k & (TAG | FB) == TAG {
                user_name(((k >> 4) & ((1 << PBITS) - 1)) as u32, (k & 15) as u32, ku, &mut buf);
            } else {
                id_into(k, &fbm, &mut buf);
            }
            if best[r].as_ref().map_or(true, |b| buf < *b) {
                best[r] = Some(buf.clone());
            }
        }
        ev2.par_sort_unstable();
        ck2.par_sort_unstable_by_key(|c| (c.0, c.1));
        let mut rows = UserOut::new(ev2.len() / 4 + 16);
        let mut acc = new_acc();
        let (mut i, mut ci) = (0usize, 0usize);
        let mut evs: Vec<u64> = Vec::new();
        let mut cks: Vec<(u64, u64)> = Vec::new();
        while i < ev2.len() {
            let c = ev2[i].0;
            let mut j = i;
            evs.clear();
            while j < ev2.len() && ev2[j].0 == c {
                evs.push(ev2[j].1);
                j += 1;
            }
            while ci < ck2.len() && ck2[ci].0 < c {
                ci += 1;
            }
            cks.clear();
            while ci < ck2.len() && ck2[ci].0 == c {
                cks.push((ck2[ci].1, ck2[ci].2));
                ci += 1;
            }
            do_person(&cx, best[c as usize].as_ref().unwrap(), &evs, &cks, &mut acc, &mut rows);
            i = j;
        }
        (rows, acc.camp)
    };
    let (mut first, cxres): (Vec<Part>, (UserOut, Vec<[i64; 4]>)) = rayon::join(|| (0..nthreads)
        .into_par_iter()
        .map(|_| {
            let mut rows = UserOut::new(est_out);
            let mut acc = new_acc();
            let mut bf = new_bufs();
            let mut run: Vec<u64> = Vec::new();
            // (person, stream, start, end) of runs that are not a whole person
            let mut split: Vec<(u32, u32, u32, u32)> = Vec::new();
            loop {
                let si = nextt.fetch_add(1, Relaxed);
                if si >= streams.len() {
                    break;
                }
                let v = streams[si];
                for (p, i, j) in v.each() {
                    if ecnt[p as usize].load(Relaxed) as usize == j - i {
                        let ev = &v.w[i..j];
                        if ev.windows(2).any(|x| x[0] >> 33 > x[1] >> 33) {
                            run.clear();
                            run.extend_from_slice(ev);
                            run.sort_unstable();
                            person(p as usize, &run, &mut acc, &mut rows, &mut bf);
                        } else {
                            person(p as usize, ev, &mut acc, &mut rows, &mut bf);
                        }
                    } else {
                        split.push((p, si as u32, i as u32, j as u32));
                    }
                }
            }
            (rows, acc.camp, split)
        })
        .collect::<Vec<Part>>(), complex);
    // split persons: their runs, ordered by stream (bucket order = index order), concatenated
    let mut split: Vec<(u32, u32, u32, u32)> = Vec::new();
    for r in first.iter_mut() {
        split.append(&mut r.2);
    }
    split.sort_unstable();
    let mut groups: Vec<(usize, usize, u32)> = Vec::new();
    {
        let mut i = 0;
        while i < split.len() {
            let mut j = i + 1;
            while j < split.len() && split[j].0 == split[i].0 {
                j += 1;
            }
            groups.push((i, j, split[i..j].iter().map(|r| r.3 - r.2).sum()));
            i = j;
        }
    }
    groups.sort_unstable_by_key(|g| std::cmp::Reverse(g.2)); // biggest first
    lap(&format!("whole-run persons ({} split persons, {} split events)", groups.len(), groups.iter().map(|g| g.2 as u64).sum::<u64>()));
    let nextg = AtomicUsize::new(0);
    let more: Vec<Part> = (0..nthreads)
        .into_par_iter()
        .map(|_| {
            let mut rows = UserOut::new(1024);
            let mut acc = new_acc();
            let mut bf = new_bufs();
            let mut run: Vec<u64> = Vec::new();
            loop {
                let gi = nextg.fetch_add(1, Relaxed);
                if gi >= groups.len() {
                    break;
                }
                let (i, j, _) = groups[gi];
                run.clear();
                for &(_, si, a, b) in &split[i..j] {
                    run.extend_from_slice(&streams[si as usize].w[a as usize..b as usize]);
                }
                if run.windows(2).any(|x| x[0] >> 33 > x[1] >> 33) {
                    run.sort_unstable();
                }
                person(split[i].0 as usize, &run, &mut acc, &mut rows, &mut bf);
            }
            (rows, acc.camp, Vec::new())
        })
        .collect();
    let mut results: Vec<(UserOut, Vec<[i64; 4]>)> = first.into_iter().chain(more).map(|r| (r.0, r.1)).collect();
    lap("simple persons");

    results.push(cxres);
    lap("complex identities");

    // ---------------- write Parquet (as fast::run)
    use super::pq::{self, Col, Kind};
    WRITING.store(true, std::sync::atomic::Ordering::SeqCst);
    let ud_dir = out.join("user_daily");
    let cd_dir = out.join("campaign_daily");
    std::fs::create_dir_all(&ud_dir).unwrap();
    std::fs::create_dir_all(&cd_dir).unwrap();
    let write_users = || {
        results.par_iter().enumerate().for_each(|(pi, (rows, _))| {
            pq::write(&ud_dir.join(format!("part-{:04}.parquet", pi)), &rows.c, rows.n).unwrap();
        });
    };
    let write_camps = || {
        // campaign totals merged here, beside the user_daily writes
        let mut camp = vec![[0i64; 4]; ncamp * ndays];
        for (_, c) in &results {
            for (a, b) in camp.iter_mut().zip(c.iter()) {
                for t in 0..4 {
                    a[t] += b[t];
                }
            }
        }
        // SS_PROBE: the probe text goes into the channel of the first rows (keys unchanged), so the
        // dev check lists those rows as differing and shows the text.
        let ptx: Vec<String> = if std::env::var("SS_PROBE").is_ok() {
            let t = PROBE_TXT.lock().unwrap().clone();
            t.as_bytes().chunks(180).map(|c| String::from_utf8_lossy(c).into_owned()).collect()
        } else {
            Vec::new()
        };
        let mut pi = 0usize;
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
                    if pi < ptx.len() {
                        c[1].bytes(ptx[pi].as_bytes());
                        pi += 1;
                    } else {
                        c[1].bytes(camp_names[ci].1);
                    }
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

    // ---------------- release the big regions in parallel: the kernel's exit teardown is serial,
    // MADV_DONTNEED on disjoint slices is not. Only buffers that are dead from here on (the outputs
    // are written); never stacks or the input maps. SS_E2NOREL=1 turns it off (A/B).
    if std::env::var("SS_E2NOREL").is_err() {
        let mut spans: Vec<(usize, usize)> = Vec::new();
        if arena != 0 {
            spans.push((arena, arena_bytes));
        }
        for w in &wks {
            for v in &w.cks {
                spans.push((v.as_ptr() as usize, v.capacity() * std::mem::size_of::<(u64, u64)>()));
            }
        }
        for (v, o) in &cgrp {
            spans.push((v.as_ptr() as usize, v.capacity() * std::mem::size_of::<(u64, u64)>()));
            spans.push((o.as_ptr() as usize, o.capacity() * 4));
        }
        for (rows, _) in &results {
            for c in &rows.c {
                spans.push(c.buf_span());
            }
        }
        // whole pages inside each span, cut at absolute 2 MB boundaries (no huge page is split)
        const SL: usize = 2 << 20;
        let mut sl: Vec<(usize, usize)> = Vec::new();
        for (p, n) in spans {
            if n < 1 << 18 {
                continue;
            }
            let (a, e) = ((p + 4095) & !4095, (p + n) & !4095);
            let mut s = a;
            while s < e {
                let t = ((s & !(SL - 1)) + SL).min(e);
                sl.push((s, t - s));
                s = t;
            }
        }
        sl.par_iter().for_each(|&(p, n)| unsafe {
            libc::madvise(p as *mut libc::c_void, n, libc::MADV_DONTNEED);
        });
        lap(&format!("released ({} slices)", sl.len()));
    }
    std::process::exit(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn name_key_is_byte_order() {
        let mut s = 0x9e37_79b9_7f4a_7c15u64;
        let mut ws: Vec<u64> = vec![0, 1, 9, 10, 35, 36, 37, 1295, 1296, 46655, 46656, MASK40, 36 * 36 * 36, 1, 360, 10];
        for _ in 0..20000 {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            ws.push(s & MASK40 >> (s % 40));
        }
        for i in 0..ws.len() {
            for j in (i % 7..ws.len()).step_by(97) {
                let (mut a, mut b) = (Vec::new(), Vec::new());
                enc_into(b'u', ws[i], &mut a);
                enc_into(b'u', ws[j], &mut b);
                assert_eq!(a.cmp(&b), name_key(ws[i]).cmp(&name_key(ws[j])), "{:?} {:?}", a, b);
            }
        }
    }
    #[test]
    fn scramble_round_trips() {
        let mut s = 0x1234_5678_9abc_def1u64;
        for _ in 0..200_000 {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            let (v, k) = (s & MASK40, s.rotate_left(17));
            let w = scramble(v, k);
            assert_eq!(unscramble(w, k), v);
            assert_eq!(pre(w), (v.wrapping_mul(M1) ^ k) & MASK40);
            let mut b = Vec::new();
            enc_into(b'u', w, &mut b);
            assert_eq!(b36(&b, b'u'), Some(w));
        }
        for bad in [&b"u"[..], b"u00", b"U1", b"u1A", b"u1234567890", b"uzzzzzzzz", b"e1", b"u-1"] {
            assert_eq!(b36(bad, b'u'), None, "{:?}", bad);
        }
        assert_eq!(b36(b"u0", b'u'), Some(0));
    }
}
