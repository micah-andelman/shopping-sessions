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
use super::scan::scan;

const NB: usize = 256;
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
#[inline]
fn pack(b: &[u8]) -> Option<u64> {
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

type Fb = Vec<(u64, &'static [u8])>;

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
}
impl LinkTab {
    fn new(n: usize) -> Self {
        let cap = (n * 2).max(16).next_power_of_two();
        LinkTab { mask: cap - 1, slots: vec![[0u64; 2]; cap], n: 0 }
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
    /// Resolve keys in place, prefetching DIST lookups ahead.
    fn resolve_all(&self, ks: &mut [u64]) {
        const DIST: usize = 16;
        for i in 0..ks.len().min(DIST) {
            self.prefetch(ks[i]);
        }
        for i in 0..ks.len() {
            if i + DIST < ks.len() {
                self.prefetch(ks[i + DIST]);
            }
            ks[i] = self.get(ks[i]);
        }
    }
}

pub fn run(data: &Path, out: &Path) -> bool {
    let t0 = std::time::Instant::now();
    let verbose = std::env::var("SS_VERBOSE").is_ok();
    let lap = |what: &str| {
        if verbose {
            eprintln!("{:>8.3}s  {}", t0.elapsed().as_secs_f64(), what)
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
    if std::env::var("SS_PARSEBENCH").is_ok() {
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        let sink = AtomicU64::new(0);
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
            scan::<2>(d, |f| {
                if let [Some(a), Some(b)] = *f {
                    v.push((key_of(a, &mut fb), key_of(b, &mut fb)));
                }
            });
            (v, fb)
        })
            .collect();
    if verbose { eprintln!("  link {} {:.3}s", "parsed", t0.elapsed().as_secs_f64()); }
    let link_job = move || -> Option<(LinkTab, Fb)> {
        let fbm = merge_fb(parts.iter().map(|p| &p.1))?;
        let mut lids: Vec<u64> = Vec::with_capacity(parts.iter().map(|p| p.0.len() * 2).sum());
        for (v, _) in &parts {
            for &(a, b) in v {
                lids.push(a);
                lids.push(b);
            }
        }
        lids.par_sort_unstable();
        lids.dedup();
        if !fbm.is_empty() {
            lids.sort_by(|&a, &b| cmp_id(a, b, &fbm));
        }
        if verbose { eprintln!("  link {} {:.3}s", "sorted", t0.elapsed().as_secs_f64()); }
        let mut idx: hashbrown::HashTable<(u64, u32)> = hashbrown::HashTable::with_capacity(lids.len());
        for (i, &k) in lids.iter().enumerate() {
            idx.insert_unique(th(k), (k, i as u32), |e| th(e.0));
        }
        if verbose { eprintln!("  link {} {:.3}s", "idx", t0.elapsed().as_secs_f64()); }
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
        for (v, _) in &parts {
            for &(a, b) in v {
                let ra = find(&mut parent, get(a));
                let rb = find(&mut parent, get(b));
                if ra != rb {
                    if ra < rb { parent[rb as usize] = ra } else { parent[ra as usize] = rb }
                }
            }
        }
        if verbose { eprintln!("  link {} {:.3}s", "uf", t0.elapsed().as_secs_f64()); }
        let mut table = LinkTab::new(lids.len());
        for i in 0..lids.len() as u32 {
            let r = find(&mut parent, i);
            table.insert(lids[i as usize], lids[r as usize]);
        }
        if verbose { eprintln!("  link {} {:.3}s", "table", t0.elapsed().as_secs_f64()); }
        let fb: Fb = parts.into_iter().flat_map(|p| p.1).collect();
        Some((table, fb))
    };

    // ---------------- events: parse every copy, scatter by event_id bucket
    let ev_job = || -> Vec<(Vec<Vec<Ev>>, Fb)> {
        let r: Vec<(Vec<Vec<Ev>>, Fb)> = ev_groups
            .par_iter()
            .map(|grp| {
                let bytes: usize = grp.iter().map(|c| c.2 - c.1).sum();
                let cap = bytes / 64 / NB + 64;
                let mut bk: Vec<Vec<Ev>> = (0..NB).map(|_| Vec::with_capacity(cap)).collect();
                let mut fb = Fb::new();
                for &(mi, s, e) in grp.iter() {
                    let d: &'static [u8] = &ev_maps[mi][s..e];
                    scan::<7>(d, |f| {
                        let eid = match f[0] {
                            Some(x) => x,
                            None => return,
                        };
                        let ekey = key_of(eid, &mut fb);
                        let ing = parse_ts_fast(f[3]);
                        let ing = if ing == BAD { i64::MAX } else { ing };
                        let ts = parse_ts_fast(f[2]);
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
                        bk[bucket(ekey)].push(Ev { ekey, ukey, ing, lo });
                    });
                }
                (bk, fb)
            })
            .collect();
        if verbose { eprintln!("  events done {:.3}s", t0.elapsed().as_secs_f64()); }
        r
    };

    let (links, evp) = rayon::join(link_job, ev_job);
    let (ltab, lfb) = match links {
        Some(x) => x,
        None => return false,
    };
    lap(&format!("links ({} ids) and events parsed", ltab.n));

    // Person partitions: survivors and clicks are scattered by hash(person key) so each
    // partition is sorted on its own, in cache, instead of one global sort.
    const NP: usize = 256;
    #[inline]
    fn ppart(k: u64) -> usize {
        (mix(k ^ 0x5bd1_e995) >> 56) as usize
    }
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
                        for e in &g.0[b] {
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
                        for e in &g.0[b] {
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
    let ck_job = || -> (Vec<Vec<Vec<Ck>>>, Fb) {
        let ch = chunks(ck_maps);
        let groups = split_groups(&ch, nthreads * 2);
        let parts: Vec<(Vec<Vec<Ck>>, Fb)> = groups
            .par_iter()
            .map(|grp| {
                let mut outp: Vec<Vec<Ck>> = (0..NP).map(|_| Vec::new()).collect();
                let mut raw: Vec<Ck> = Vec::new();
                let mut fb = Fb::new();
                for &(mi, s, e) in grp.iter() {
                    let d: &'static [u8] = &ck_maps[mi][s..e];
                    scan::<4>(d, |f| {
                        let (cid, uid) = match (f[0], f[1]) {
                            (Some(c), Some(u)) => (c, u),
                            _ => return,
                        };
                        let ts = parse_ts_fast(f[3]);
                        if ts == BAD || ts < cws || ts >= we {
                            return;
                        }
                        let camp = match f[2].and_then(|c| camp_map.get(c)) {
                            Some(c) => c,
                            None => return,
                        };
                        // pkey holds the user key until the batched person lookup below
                        raw.push(Ck { pkey: key_of(uid, &mut fb), lo: ((ts - cws) as u64) << 32 | camp as u64, cid: key_of(cid, &mut fb) });
                    });
                }
                let mut ks: Vec<u64> = raw.iter().map(|c| c.pkey).collect();
                ltab.resolve_all(&mut ks);
                for (c, &pk) in raw.iter().zip(ks.iter()) {
                    outp[ppart(pk)].push(Ck { pkey: pk, ..*c });
                }
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

    let (eparts, (cparts, cfb)) = rayon::join(dedup_job, ck_job);
    lap("dedup + clicks");
    let fbm = match merge_fb(evp.iter().map(|g| &g.1).chain(std::iter::once(&lfb)).chain(std::iter::once(&cfb))) {
        Some(m) => m,
        None => return false,
    };
    drop(evp);

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
    struct Acc {
        camp: Vec<[i64; 4]>,
        cnt: Vec<u32>,
        active: Vec<u32>,
        catsum: Vec<i64>,
        touched: Vec<u16>,
    }
    let do_person = |key: u64, evr: &[(u64, u64)], ckr: &[Ck], acc: &mut Acc, rows: &mut Vec<UserRow>| {
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
    let results: Vec<(Vec<UserRow>, Vec<[i64; 4]>)> = (0..nthreads)
        .into_par_iter()
        .map(|_| {
            let mut rows: Vec<UserRow> = Vec::new();
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
                        ek.extend_from_slice(&g[p]);
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
    let nf = nthreads;
    let per = results.len().div_ceil(nf).max(1);
    let groups: Vec<&[(Vec<UserRow>, Vec<[i64; 4]>)]> = results.chunks(per).collect();
    let write_users = || {
        groups.par_iter().enumerate().for_each(|(pi, g)| {
            let n: usize = g.iter().map(|(r, _)| r.len()).sum();
            let mut c = [
                Col::new("user_id", Kind::Str, n),
                Col::new("day", Kind::Date, n),
                Col::new("sessions", Kind::I32, n),
                Col::new("events", Kind::I32, n),
                Col::new("revenue_cents", Kind::I64, n),
                Col::new("top_category", Kind::OptStr, n),
            ];
            for r in g.iter().flat_map(|(r, _)| r.iter()) {
                c[0].bytes_with(|o| id_into(r.key, &fbm, o));
                c[1].i32(r.day as i32);
                c[2].i32(r.sessions as i32);
                c[3].i32(r.events as i32);
                c[4].i64(r.revenue);
                match r.top {
                    Some(t) => c[5].bytes(&cats[t as usize]),
                    None => c[5].null(),
                }
            }
            pq::write(&ud_dir.join(format!("part-{:04}.parquet", pi)), &c, n).unwrap();
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
    true
}
