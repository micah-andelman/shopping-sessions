//! Shopping Sessions pipeline v2. Implements rules.md exactly; ref/reference.py is the oracle.
//!
//! Flow: mmap CSVs -> parallel parse straight to compact records (hashes, ms timestamps, validity)
//! -> exact event dedup (sort (eid hash, ingest, idx), compare bytes on equal hash)
//! -> links: sorted distinct IDs, union-find whose root is the byte-wise smallest ID
//! -> person key = hash(person name), verified per group (exact even on 64-bit collisions)
//! -> sort events and clicks by (person key, ts) -> per person: sessions, credit, assists
//! -> aggregate -> Parquet -> exit without freeing.

use std::fs::File;
use std::hash::BuildHasher;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::{ArrayRef, Date32Array, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use hashbrown::HashTable;
use memmap2::Mmap;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use rayon::prelude::*;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const DAY: i64 = 86_400_000;
const GAP: i64 = 1_800_000;
const WEEK: i64 = 7 * DAY;
const CHUNK: usize = 4 << 20;
const BAD: i64 = i64::MIN;

type Hasher = foldhash::fast::FixedState;
fn hasher() -> Hasher {
    Hasher::with_seed(0x5eed_1234_abcd_9876)
}
#[inline]
fn h64(b: &[u8]) -> u64 {
    hasher().hash_one(b)
}

// ------------------------------------------------------------------ files and chunks
fn list_csv(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                let n = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                n.ends_with(".csv") && !n.starts_with('.') && p.is_file()
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    v.sort();
    v
}

fn open_all(paths: &[PathBuf]) -> Vec<Mmap> {
    paths
        .par_iter()
        .filter_map(|p| {
            let f = File::open(p).ok()?;
            if f.metadata().ok()?.len() == 0 {
                return None;
            }
            unsafe { Mmap::map(&f).ok() }
        })
        .collect()
}

/// Chunk size; SS_CHUNK overrides it (tests use tiny chunks to stress split/merge logic).
fn chunk_size() -> usize {
    static C: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *C.get_or_init(|| std::env::var("SS_CHUNK").ok().and_then(|v| v.parse().ok()).filter(|&c: &usize| c > 0).unwrap_or(CHUNK))
}

/// Byte ranges (map index, start, end) of data lines (each file's header line skipped), biggest first.
fn chunks(maps: &[Mmap]) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    for (i, m) in maps.iter().enumerate() {
        let d: &[u8] = m;
        let start = match memchr::memchr(b'\n', d) {
            Some(p) => p + 1,
            None => continue,
        };
        let mut s = start;
        while s < d.len() {
            let mut e = (s + chunk_size()).min(d.len());
            if e < d.len() {
                e = match memchr::memchr(b'\n', &d[e..]) {
                    Some(p) => e + p + 1,
                    None => d.len(),
                };
            }
            out.push((i, s, e));
            s = e;
        }
    }
    out
}

#[inline]
fn for_lines<'a>(data: &'a [u8], mut f: impl FnMut(&'a [u8])) {
    let mut s = 0;
    for p in memchr::memchr_iter(b'\n', data) {
        f(&data[s..p]);
        s = p + 1;
    }
    if s < data.len() {
        f(&data[s..]);
    }
}

/// Split a line into exactly N fields under the rules' dialect. None = malformed.
#[inline]
fn fields<'a, const N: usize>(line: &'a [u8]) -> Option<[Option<&'a [u8]>; N]> {
    let mut out: [Option<&'a [u8]>; N] = [None; N];
    let mut n = 0usize;
    let mut s = 0usize;
    let quoted = memchr::memchr(b'"', line).is_some();
    let mut put = |f: &'a [u8], n: &mut usize| -> bool {
        if *n >= N {
            return false;
        }
        let v = if quoted && memchr::memchr(b'"', f).is_some() {
            if f.len() >= 2 && f[0] == b'"' && f[f.len() - 1] == b'"' && memchr::memchr(b'"', &f[1..f.len() - 1]).is_none() {
                &f[1..f.len() - 1]
            } else {
                return false;
            }
        } else {
            f
        };
        out[*n] = if v.is_empty() { None } else { Some(v) };
        *n += 1;
        true
    };
    for p in memchr::memchr_iter(b',', line) {
        if !put(&line[s..p], &mut n) {
            return None;
        }
        s = p + 1;
    }
    if !put(&line[s..], &mut n) {
        return None;
    }
    if n == N { Some(out) } else { None }
}

// ------------------------------------------------------------------ timestamps and numbers
#[inline]
fn digits(b: &[u8]) -> bool {
    !b.is_empty() && b.iter().all(|c| c.is_ascii_digit())
}

#[inline]
fn d2(b: &[u8]) -> Option<i64> {
    if b.len() == 2 && b[0].is_ascii_digit() && b[1].is_ascii_digit() {
        Some(((b[0] - b'0') * 10 + (b[1] - b'0')) as i64)
    } else {
        None
    }
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

// ---- fast paths: same results as parse_ts / parse_price (differentially tested in tests below);
// anything unusual falls back to the reference functions.

/// SWAR: true iff every byte selected by `m` is an ASCII digit and every other byte equals `sep`.
#[inline(always)]
fn swar_ok(w: u64, m: u64, sep: u64) -> bool {
    let x = w ^ 0x3030_3030_3030_3030;
    if (w & !m) != sep || (x & m & 0xF0F0_F0F0_F0F0_F0F0) != 0 {
        return false;
    }
    ((x & m).wrapping_add(0x0606_0606_0606_0606 & m) & 0x1010_1010_1010_1010 & m) == 0
}

/// Single pass over up to 18 digits (no separate digits()/leading-zero scans).
#[inline(always)]
fn digits_u64(v: &[u8]) -> Option<i64> {
    let mut x: i64 = 0;
    let mut bad = 0u8;
    for &c in v {
        let d = c.wrapping_sub(b'0');
        bad |= (d > 9) as u8;
        x = x.wrapping_mul(10).wrapping_add(d as i64);
    }
    if bad == 0 { Some(x) } else { None }
}

/// Epoch ms, or BAD (same contract as parse_ts).
#[inline]
fn parse_ts_fast(v: Option<&[u8]>) -> i64 {
    let v = match v {
        Some(v) => v,
        None => return BAD,
    };
    let n = v.len();
    if n == 20 || n == 25 {
        // "YYYY-MM-" "DDTHH:MM" ":SS" then 'Z' or "+hh:mm"/"-hh:mm"
        let a = u64::from_le_bytes(v[0..8].try_into().unwrap());
        let b = u64::from_le_bytes(v[8..16].try_into().unwrap());
        const MA: u64 = 0x00FF_FF00_FFFF_FFFF; // digits at 0-3, 5-6
        const SA: u64 = 0x2D00_002D_0000_0000; // '-' at 4 and 7
        const MB: u64 = 0xFFFF_00FF_FF00_FFFF; // digits at 0-1, 3-4, 6-7
        const SB: u64 = 0x0000_3A00_0054_0000; // 'T' at 2, ':' at 5
        if !swar_ok(a, MA, SA) || !swar_ok(b, MB, SB) || v[16] != b':' {
            return parse_ts(Some(v));
        }
        let (s1, s2) = (v[17].wrapping_sub(b'0'), v[18].wrapping_sub(b'0'));
        if s1 > 9 || s2 > 9 {
            return BAD;
        }
        let ad = a ^ 0x3030_3030_3030_3030;
        let bd = b ^ 0x3030_3030_3030_3030;
        let by = |w: u64, i: u32| ((w >> (8 * i)) & 0xFF) as i64;
        let y = by(ad, 0) * 1000 + by(ad, 1) * 100 + by(ad, 2) * 10 + by(ad, 3);
        let mo = by(ad, 5) * 10 + by(ad, 6);
        let d = by(bd, 0) * 10 + by(bd, 1);
        let h = by(bd, 3) * 10 + by(bd, 4);
        let mi = by(bd, 6) * 10 + by(bd, 7);
        let s = s1 as i64 * 10 + s2 as i64;
        let off = if n == 20 {
            if v[19] != b'Z' {
                return BAD;
            }
            0
        } else {
            let sign = v[19];
            if (sign != b'+' && sign != b'-') || v[22] != b':' {
                return BAD;
            }
            let (o1, o2, o3, o4) =
                (v[20].wrapping_sub(b'0'), v[21].wrapping_sub(b'0'), v[23].wrapping_sub(b'0'), v[24].wrapping_sub(b'0'));
            if o1 > 9 || o2 > 9 || o3 > 9 || o4 > 9 {
                return BAD;
            }
            let (oh, om) = (o1 as i64 * 10 + o2 as i64, o3 as i64 * 10 + o4 as i64);
            if oh > 23 || om > 59 {
                return BAD;
            }
            let o = (oh * 60 + om) * 60_000;
            if sign == b'+' { o } else { -o }
        };
        if !(1..=12).contains(&mo) || h > 23 || mi > 59 || s > 59 || d < 1 {
            return BAD;
        }
        let md = if mo == 2 {
            if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) { 29 } else { 28 }
        } else {
            [31, 0, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][(mo - 1) as usize]
        };
        if d > md {
            return BAD;
        }
        return (days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + s) * 1000 - off;
    }
    if n >= 1 && n <= 18 {
        return match digits_u64(v) {
            Some(x) => x,
            None => parse_ts(Some(v)),
        };
    }
    parse_ts(Some(v))
}

/// Same contract as parse_price.
#[inline]
fn parse_price_fast(v: Option<&[u8]>) -> i64 {
    match v {
        Some(v) if !v.is_empty() && v.len() <= 8 => match digits_u64(v) {
            Some(x) if (1..=10_000_000).contains(&x) => x,
            Some(_) => -1,
            None => -1,
        },
        _ => parse_price(v),
    }
}

/// Epoch ms, or BAD. Window is checked by the caller.
fn parse_ts(v: Option<&[u8]>) -> i64 {
    let v = match v {
        Some(v) => v,
        None => return BAD,
    };
    if digits(v) {
        let t = match v.iter().position(|&c| c != b'0') {
            None => return 0,
            Some(p) => &v[p..],
        };
        if t.len() > 18 {
            return BAD;
        }
        let mut x: i64 = 0;
        for &c in t {
            x = x * 10 + (c - b'0') as i64;
        }
        return x;
    }
    let off = match v.len() {
        20 => {
            if v[19] != b'Z' {
                return BAD;
            }
            0
        }
        25 => {
            let sign = v[19];
            if (sign != b'+' && sign != b'-') || v[22] != b':' {
                return BAD;
            }
            let (oh, om) = match (d2(&v[20..22]), d2(&v[23..25])) {
                (Some(a), Some(b)) => (a, b),
                _ => return BAD,
            };
            if oh > 23 || om > 59 {
                return BAD;
            }
            let o = (oh * 60 + om) * 60_000;
            if sign == b'+' { o } else { -o }
        }
        _ => return BAD,
    };
    if v[4] != b'-' || v[7] != b'-' || v[10] != b'T' || v[13] != b':' || v[16] != b':' {
        return BAD;
    }
    if !v[0..4].iter().all(|c| c.is_ascii_digit()) {
        return BAD;
    }
    let y = v[0..4].iter().fold(0i64, |a, &c| a * 10 + (c - b'0') as i64);
    let (mo, d, h, mi, s) = match (d2(&v[5..7]), d2(&v[8..10]), d2(&v[11..13]), d2(&v[14..16]), d2(&v[17..19])) {
        (Some(a), Some(b), Some(c), Some(e), Some(f)) => (a, b, c, e, f),
        _ => return BAD,
    };
    if !(1..=12).contains(&mo) || h > 23 || mi > 59 || s > 59 {
        return BAD;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let md = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][(mo - 1) as usize];
    if d < 1 || d > md {
        return BAD;
    }
    (days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + s) * 1000 - off
}

/// Purchase price: digits only, 1..=10_000_000, else -1.
fn parse_price(v: Option<&[u8]>) -> i64 {
    let v = match v {
        Some(v) if digits(v) => v,
        _ => return -1,
    };
    let t = match v.iter().position(|&c| c != b'0') {
        None => return -1,
        Some(p) => &v[p..],
    };
    if t.len() > 8 {
        return -1;
    }
    let x = t.iter().fold(0i64, |a, &c| a * 10 + (c - b'0') as i64);
    if (1..=10_000_000).contains(&x) { x } else { -1 }
}

fn read_small(path: &Path) -> Vec<[Vec<u8>; 2]> {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    let body = match memchr::memchr(b'\n', &data) {
        Some(p) => &data[p + 1..],
        None => return out,
    };
    for_lines(body, |l| {
        if let Some([Some(a), Some(b)]) = fields::<2>(l) {
            out.push([a.to_vec(), b.to_vec()]);
        }
    });
    out
}

/// Byte-string -> u32 lookup table with precomputed hashes.
struct Dict<'a> {
    t: HashTable<(u64, &'a [u8], u32)>,
}
impl<'a> Dict<'a> {
    fn new(cap: usize) -> Self {
        Dict { t: HashTable::with_capacity(cap) }
    }
    fn insert(&mut self, k: &'a [u8], v: u32) {
        let h = h64(k);
        if self.t.find(h, |e| e.1 == k).is_none() {
            self.t.insert_unique(h, (h, k, v), |e| e.0);
        }
    }
    #[inline]
    fn get_h(&self, h: u64, k: &[u8]) -> Option<u32> {
        self.t.find(h, |e| e.0 == h && e.1 == k).map(|e| e.2)
    }
    #[inline]
    fn get(&self, k: &[u8]) -> Option<u32> {
        self.get_h(h64(k), k)
    }
}

// ------------------------------------------------------------------ records
/// One parsed event line (any copy). `ok` = this copy passes every validity and lateness rule.
struct RawEv<'a> {
    eid: &'a [u8],
    eh: u64,
    uid: &'a [u8],
    ing: i64,
    ts: i64,
    price: u32,
    cat: u16,
    ok: bool,
}

/// A person-key record for sorting: key = hash(person name); low = (ts - base) << 32 | idx.
type Rec = (u64, u64);

struct Payload<'a> {
    name: &'a [u8],
    price: u32,
    cat: u16,
}
struct CPay<'a> {
    name: &'a [u8],
    cid: &'a [u8],
    camp: u32,
}

struct Raw<T>(*mut T);
unsafe impl<T> Send for Raw<T> {}
unsafe impl<T> Sync for Raw<T> {}

mod fast;
mod pq;
mod scan;

fn main() {
    if let Some(n) = std::env::var("NCPU").ok().and_then(|v| v.parse::<usize>().ok()) {
        let _ = rayon::ThreadPoolBuilder::new().num_threads(n.max(1)).build_global();
    }
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let args: Vec<String> = std::env::args().collect();
    let data = PathBuf::from(if args.len() > 2 { args[1].clone() } else { env("DATA_DIR", "/data") });
    let out = PathBuf::from(if args.len() > 2 { args[2].clone() } else { env("OUTPUT_DIR", "/output") });
    // SS_LEGACY forces the v4 path (for differential testing).
    if std::env::var("SS_LEGACY").is_err() && fast::run(&data, &out) {
        std::process::exit(0);
    }
    eprintln!("note: fast path declined, running the byte-exact legacy path");
    legacy(data, out);
}

/// v4 pipeline: byte-exact everywhere, no layout assumptions. Used when the fast path declines.
fn legacy(data: PathBuf, out: PathBuf) {
    let t0 = std::time::Instant::now();
    let verbose = std::env::var("SS_VERBOSE").is_ok();
    let lap = |what: &str| {
        if verbose {
            eprintln!("{:>8.3}s  {}", t0.elapsed().as_secs_f64(), what)
        }
    };

    // manifest
    let man = std::fs::read_to_string(data.join("manifest.json")).expect("manifest");
    let grab = |k: &str| -> i64 {
        let p = man.find(k).expect("manifest key") + k.len();
        let rest = &man[p..];
        let q1 = rest.find('"').unwrap();
        let rest = &rest[q1 + 1..];
        let q2 = rest.find('"').unwrap();
        parse_ts(Some(rest[..q2].as_bytes()))
    };
    let ws = grab("\"window_start\"");
    let we = grab("\"window_end\"");
    let ws_day = ws.div_euclid(DAY);
    let ndays = ((we - 1).div_euclid(DAY) - ws_day + 1).max(1) as usize;
    let cws = ws - WEEK;

    // small tables
    let products = read_small(&data.join("products.csv"));
    let campaigns = read_small(&data.join("campaigns.csv"));
    let mut cats: Vec<Vec<u8>> = products.iter().map(|r| r[1].clone()).collect();
    cats.push(b"unknown".to_vec());
    cats.sort();
    cats.dedup();
    let cat_of = |c: &[u8]| cats.binary_search_by(|x| x.as_slice().cmp(c)).unwrap() as u32;
    let unknown = cat_of(b"unknown") as u16;
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
    // inputs live until process exit (we never unmap them)
    let ev_maps: &'static [Mmap] = Vec::leak(ev_maps);
    let ck_maps: &'static [Mmap] = Vec::leak(ck_maps);
    let lk_maps: &'static [Mmap] = Vec::leak(lk_maps);
    lap("opened");

    // ---------------- links (small): distinct IDs sorted byte-wise, so index order = byte order
    let link_job = || {
    let lk_chunks = chunks(lk_maps);
    let links: Vec<(&[u8], &[u8])> = lk_chunks
        .par_iter()
        .flat_map_iter(|&(mi, s, e)| {
            let d: &[u8] = &lk_maps[mi][s..e];
            let mut v = Vec::new();
            for_lines(d, |l| {
                if let Some([Some(a), Some(b)]) = fields::<2>(l) {
                    v.push((a, b));
                }
            });
            v.into_iter()
        })
        .collect();
    let mut lids: Vec<&'static [u8]> = Vec::with_capacity(links.len() * 2);
    for &(a, b) in &links {
        lids.push(a);
        lids.push(b);
    }
    lids.par_sort_unstable();
    lids.dedup();
    let lids: &'static [&'static [u8]] = Vec::leak(lids);
    let mut ldict: Dict<'static> = Dict::new(lids.len() * 2);
    for (i, k) in lids.iter().enumerate() {
        ldict.insert(k, i as u32);
    }
    let mut parent: Vec<u32> = (0..lids.len() as u32).collect();
    fn find(p: &mut [u32], mut x: u32) -> u32 {
        while p[x as usize] != x {
            let g = p[p[x as usize] as usize];
            p[x as usize] = g;
            x = g;
        }
        x
    }
    for &(a, b) in &links {
        let ra = find(&mut parent, ldict.get(a).unwrap());
        let rb = find(&mut parent, ldict.get(b).unwrap());
        if ra != rb {
            // smaller index = byte-wise smaller ID becomes the root
            if ra < rb { parent[rb as usize] = ra } else { parent[ra as usize] = rb }
        }
    }
    for i in 0..parent.len() as u32 {
        let r = find(&mut parent, i);
        parent[i as usize] = r;
    }
    let root_hash: Vec<u64> = (0..lids.len()).into_par_iter().map(|i| if parent[i] as usize == i { h64(lids[i]) } else { 0 }).collect();
    (lids, ldict, parent, root_hash)
    };
    let ev_job = || {
    let ev_chunks = chunks(ev_maps);
    let evs: Vec<Vec<RawEv>> = ev_chunks
        .par_iter()
        .map(|&(mi, s, e)| {
            let d: &[u8] = &ev_maps[mi][s..e];
            let mut v = Vec::with_capacity(d.len() / 70 + 16);
            for_lines(d, |l| {
                let f = match fields::<7>(l) {
                    Some(f) => f,
                    None => return,
                };
                let eid = match f[0] {
                    Some(x) => x,
                    None => return,
                };
                let ing = parse_ts(f[3]);
                let ing = if ing == BAD { i64::MAX } else { ing };
                let ts = parse_ts(f[2]);
                let typ = match f[4] {
                    Some(b"view") => 0u8,
                    Some(b"cart") => 1,
                    Some(b"purchase") => 2,
                    _ => 3,
                };
                let mut ok = typ != 3 && f[1].is_some() && f[5].is_some() && ts != BAD && ts >= ws && ts < we && ing - ts <= DAY;
                let mut price = 0u32;
                if ok && typ == 2 {
                    let p = parse_price(f[6]);
                    if p < 0 { ok = false } else { price = p as u32 }
                }
                let cat = if ok { prod.get(f[5].unwrap()).map(|c| c as u16).unwrap_or(unknown) } else { 0 };
                v.push(RawEv { eid, eh: h64(eid), uid: f[1].unwrap_or(b""), ing, ts, price, cat, ok });
            });
            v
        })
        .collect();
    evs
    };
    let ((lids, ldict, parent, root_hash), evs) = rayon::join(link_job, ev_job);
    lap(&format!("links ({} ids) and events parsed", lids.len()));
    // person (key, name) of a user id
    let person = |uid: &'_ [u8]| -> (u64, usize) {
        let h = h64(uid);
        match ldict.get_h(h, uid) {
            Some(i) => {
                let r = parent[i as usize] as usize;
                (root_hash[r], r)
            }
            None => (h, usize::MAX),
        }
    };
    let mut offs = Vec::with_capacity(evs.len() + 1);
    let mut n_ev = 0usize;
    for v in &evs {
        offs.push(n_ev);
        n_ev += v.len();
    }
    lap(&format!("parsed {} event lines", n_ev));

    let ck_job = || {
    // ---------------- clicks (no dedup needed: copies are identical, so they cannot change credit or assists)
    let ck_chunks = chunks(ck_maps);
    let cparts: Vec<(Vec<Rec>, Vec<CPay>)> = ck_chunks
        .par_iter()
        .map(|&(mi, s, e)| {
            let d: &[u8] = &ck_maps[mi][s..e];
            let mut k = Vec::with_capacity(d.len() / 45 + 16);
            let mut p = Vec::with_capacity(d.len() / 45 + 16);
            for_lines(d, |l| {
                let f = match fields::<4>(l) {
                    Some(f) => f,
                    None => return,
                };
                let (cid, uid) = match (f[0], f[1]) {
                    (Some(c), Some(u)) => (c, u),
                    _ => return,
                };
                let ts = parse_ts(f[3]);
                if ts == BAD || ts < cws || ts >= we {
                    return;
                }
                let camp = match f[2].and_then(|c| camp_map.get(c)) {
                    Some(c) => c,
                    None => return,
                };
                let (key, r) = person(uid);
                let name = if r == usize::MAX { uid } else { lids[r] };
                k.push((key, ((ts - cws) as u64) << 32));
                p.push(CPay { name, cid, camp });
            });
            (k, p)
        })
        .collect();
    let n_ck: usize = cparts.iter().map(|p| p.0.len()).sum();
    let mut ck: Vec<Rec> = Vec::with_capacity(n_ck);
    let mut cpay: Vec<CPay> = Vec::with_capacity(n_ck);
    for (k, p) in cparts {
        let base = ck.len() as u64;
        ck.extend(k.into_iter().enumerate().map(|(i, (a, b))| (a, b | (base + i as u64))));
        cpay.extend(p);
    }
    (ck, cpay, n_ck)
    };
    let ev_job2 = || {
    // ---------------- dedup on event_id: smallest ingest_ts wins. kept[g] = 1 for the surviving copy.
    const B: usize = 1024;
    let cnts: Vec<Vec<u32>> = evs
        .par_iter()
        .map(|v| {
            let mut c = vec![0u32; B];
            for e in v {
                c[(e.eh >> 54) as usize] += 1;
            }
            c
        })
        .collect();
    let mut bstart = vec![0usize; B + 1];
    for c in &cnts {
        for b in 0..B {
            bstart[b + 1] += c[b] as usize;
        }
    }
    for b in 0..B {
        bstart[b + 1] += bstart[b];
    }
    let mut run = bstart[..B].to_vec();
    let coffs: Vec<Vec<usize>> = cnts
        .iter()
        .map(|c| {
            let o = run.clone();
            for b in 0..B {
                run[b] += c[b] as usize;
            }
            o
        })
        .collect();
    let mut keys: Vec<(u64, i64, u32)> = Vec::with_capacity(n_ev);
    unsafe { keys.set_len(n_ev) };
    {
        let kp = Raw(keys.as_mut_ptr());
        evs.par_iter().zip(offs.par_iter()).zip(coffs.into_par_iter()).for_each(|((v, &o), mut co)| {
            let kp = &kp;
            for (j, e) in v.iter().enumerate() {
                let b = (e.eh >> 54) as usize;
                unsafe { *kp.0.add(co[b]) = (e.eh, e.ing, (o + j) as u32) };
                co[b] += 1;
            }
        });
    }
    let mut kept = vec![0u8; n_ev];
    {
        let locate = |g: u32| -> &RawEv {
            let g = g as usize;
            let ci = offs.partition_point(|&o| o <= g) - 1;
            &evs[ci][g - offs[ci]]
        };
        let kp = Raw(kept.as_mut_ptr());
        let keyp = Raw(keys.as_mut_ptr());
        (0..B).into_par_iter().for_each(|b| {
            let (kp, keyp) = (&kp, &keyp);
            let sl = unsafe { std::slice::from_raw_parts_mut(keyp.0.add(bstart[b]), bstart[b + 1] - bstart[b]) };
            sl.sort_unstable();
            let n = sl.len();
            let mut i = 0;
            while i < n {
                let mut j = i + 1;
                while j < n && sl[j].0 == sl[i].0 {
                    j += 1;
                }
                if j == i + 1 {
                    unsafe { *kp.0.add(sl[i].2 as usize) = 1 };
                } else {
                    for a in i..j {
                        let ea = locate(sl[a].2).eid;
                        if !(i..a).any(|c| locate(sl[c].2).eid == ea) {
                            unsafe { *kp.0.add(sl[a].2 as usize) = 1 };
                        }
                    }
                }
                i = j;
            }
        });
    }
    drop(keys);
    lap("dedup");

    // ---------------- surviving valid events -> (person key, ts|idx) records + payload
    let counts: Vec<usize> = evs.par_iter().zip(offs.par_iter()).map(|(v, &o)| v.iter().enumerate().filter(|(j, e)| e.ok && kept[o + j] == 1).count()).collect();
    let mut poffs = Vec::with_capacity(counts.len());
    let mut n_good = 0usize;
    for c in &counts {
        poffs.push(n_good);
        n_good += c;
    }
    let mut ek: Vec<Rec> = Vec::with_capacity(n_good);
    let mut epay: Vec<Payload> = Vec::with_capacity(n_good);
    unsafe {
        ek.set_len(n_good);
        epay.set_len(n_good);
    }
    {
        let kp = Raw(ek.as_mut_ptr());
        let pp = Raw(epay.as_mut_ptr());
        evs.par_iter().zip(offs.par_iter()).zip(poffs.par_iter()).for_each(|((v, &o), &po)| {
            let (kp, pp) = (&kp, &pp);
            let mut w = po;
            for (j, e) in v.iter().enumerate() {
                if !(e.ok && kept[o + j] == 1) {
                    continue;
                }
                let (key, r) = person(e.uid);
                let name = if r == usize::MAX { e.uid } else { lids[r] };
                unsafe {
                    *kp.0.add(w) = (key, (((e.ts - ws) as u64) << 32) | w as u64);
                    std::ptr::write(pp.0.add(w), Payload { name, price: e.price, cat: e.cat });
                }
                w += 1;
            }
        });
    }
    (ek, epay, n_good)
    };
    let ((mut ek, epay, n_good), (mut ck, cpay, n_ck)) = rayon::join(ev_job2, ck_job);
    lap(&format!("{} valid events", n_good));

    lap(&format!("{} valid click lines", n_ck));

    // ---------------- sort by (person key, ts, idx)
    rayon::join(|| ek.par_sort_unstable(), || ck.par_sort_unstable());
    lap("sorted");

    // ---------------- sessions, credit, assists, aggregation (parallel over person ranges)
    let ncamp = organic as usize + 1;
    let ncats = cats.len();
    let n = ek.len();
    let parts = rayon::current_num_threads() * 16;
    let mut eb = vec![0usize];
    for k in 1..parts {
        let mut p = n * k / parts;
        while p > 0 && p < n && ek[p].0 == ek[p - 1].0 {
            p += 1;
        }
        if p > *eb.last().unwrap() && p < n {
            eb.push(p);
        }
    }
    eb.push(n);

    struct UserRow<'a> {
        name: &'a [u8],
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

    // process one person: events (sorted by ts) and their clicks (sorted by ts)
    let do_person = |evr: &[Rec], ckr: &[Rec], acc: &mut Acc, rows: &mut Vec<UserRow<'_>>| {
        let name = epay[(evr[0].1 & 0xffff_ffff) as usize].name;
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
            let start = (evr[k].1 >> 32) as i64 + ws;
            let mut prev = start;
            let mut m = k + 1;
            while m < j {
                let t = (evr[m].1 >> 32) as i64 + ws;
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
                row = Some(UserRow { name, day, sessions: 0, events: 0, revenue: 0, top: None });
            }
            let mut rev = 0i64;
            let mut purch = false;
            for q in k..m {
                let e = &epay[(evr[q].1 & 0xffff_ffff) as usize];
                if e.price > 0 {
                    rev += e.price as i64;
                    purch = true;
                    if acc.catsum[e.cat as usize] == 0 {
                        acc.touched.push(e.cat);
                    }
                    acc.catsum[e.cat as usize] += e.price as i64;
                }
            }
            // click window [start - WEEK, start]
            let hi = (start - cws) as u64;
            let lo = (start - WEEK - cws) as u64;
            while cr < ckr.len() && (ckr[cr].1 >> 32) <= hi {
                let c = cpay[(ckr[cr].1 & 0xffff_ffff) as usize].camp;
                if acc.cnt[c as usize] == 0 {
                    acc.active.push(c);
                }
                acc.cnt[c as usize] += 1;
                cr += 1;
            }
            while cl < cr && (ckr[cl].1 >> 32) < lo {
                let c = cpay[(ckr[cl].1 & 0xffff_ffff) as usize].camp;
                acc.cnt[c as usize] -= 1;
                cl += 1;
            }
            let credit = if cl < cr {
                let top = ckr[cr - 1].1 >> 32;
                let mut best = &cpay[(ckr[cr - 1].1 & 0xffff_ffff) as usize];
                let mut f = cr - 1;
                while f > cl && (ckr[f - 1].1 >> 32) == top {
                    f -= 1;
                    let c = &cpay[(ckr[f].1 & 0xffff_ffff) as usize];
                    if c.cid < best.cid {
                        best = c;
                    }
                }
                best.camp
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

    let results: Vec<(Vec<UserRow>, Vec<[i64; 4]>)> = eb
        .par_windows(2)
        .map(|w| {
            let mut rows: Vec<UserRow> = Vec::with_capacity((w[1] - w[0]) / 8);
            let mut acc = Acc {
                camp: vec![[0i64; 4]; ncamp * ndays],
                cnt: vec![0u32; ncamp],
                active: Vec::new(),
                catsum: vec![0i64; ncats],
                touched: Vec::new(),
            };
            let mut ci = ck.partition_point(|c| c.0 < ek[w[0]].0);
            let mut i = w[0];
            while i < w[1] {
                let key = ek[i].0;
                let mut j = i + 1;
                while j < w[1] && ek[j].0 == key {
                    j += 1;
                }
                while ci < ck.len() && ck[ci].0 < key {
                    ci += 1;
                }
                let c0 = ci;
                while ci < ck.len() && ck[ci].0 == key {
                    ci += 1;
                }
                let evr = &ek[i..j];
                let ckr = &ck[c0..ci];
                let name0 = epay[(evr[0].1 & 0xffff_ffff) as usize].name;
                let clean = evr.iter().all(|e| epay[(e.1 & 0xffff_ffff) as usize].name == name0)
                    && ckr.iter().all(|c| cpay[(c.1 & 0xffff_ffff) as usize].name == name0);
                if clean {
                    do_person(evr, ckr, &mut acc, &mut rows);
                } else {
                    // 64-bit hash collision between different people: split exactly by name
                    let mut names: Vec<&[u8]> = evr.iter().map(|e| epay[(e.1 & 0xffff_ffff) as usize].name).collect();
                    names.sort_unstable();
                    names.dedup();
                    for nm in names {
                        let e2: Vec<Rec> = evr.iter().copied().filter(|e| epay[(e.1 & 0xffff_ffff) as usize].name == nm).collect();
                        let c2: Vec<Rec> = ckr.iter().copied().filter(|c| cpay[(c.1 & 0xffff_ffff) as usize].name == nm).collect();
                        do_person(&e2, &c2, &mut acc, &mut rows);
                    }
                }
                i = j;
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
    let props = WriterProperties::builder()
        .set_compression(Compression::UNCOMPRESSED)
        .set_dictionary_enabled(false)
        .set_statistics_enabled(EnabledStatistics::None)
        .set_data_page_size_limit(8 << 20)
        .set_max_row_group_size(1 << 22)
        .set_write_batch_size(1 << 16)
        .build();
    let ud_dir = out.join("user_daily");
    let cd_dir = out.join("campaign_daily");
    std::fs::create_dir_all(&ud_dir).unwrap();
    std::fs::create_dir_all(&cd_dir).unwrap();
    let us = Arc::new(Schema::new(vec![
        Field::new("user_id", DataType::Utf8, false),
        Field::new("day", DataType::Date32, false),
        Field::new("sessions", DataType::Int64, false),
        Field::new("events", DataType::Int64, false),
        Field::new("revenue_cents", DataType::Int64, false),
        Field::new("top_category", DataType::Utf8, true),
    ]));
    let s = |b: &[u8]| -> String { String::from_utf8_lossy(b).into_owned() };
    // group the per-range results into ~NCPU files
    let nf = rayon::current_num_threads().max(1);
    let per = results.len().div_ceil(nf).max(1);
    let groups: Vec<&[(Vec<UserRow>, Vec<[i64; 4]>)]> = results.chunks(per).collect();
    let write_users = || {
        groups.par_iter().enumerate().for_each(|(pi, g)| {
            let all: Vec<&UserRow> = g.iter().flat_map(|(r, _)| r.iter()).collect();
            let rows = || all.iter().copied();
            let cols: Vec<ArrayRef> = vec![
                Arc::new(StringArray::from_iter_values(rows().map(|r| s(r.name)))),
                Arc::new(Date32Array::from_iter_values(rows().map(|r| r.day as i32))),
                Arc::new(Int64Array::from_iter_values(rows().map(|r| r.sessions))),
                Arc::new(Int64Array::from_iter_values(rows().map(|r| r.events))),
                Arc::new(Int64Array::from_iter_values(rows().map(|r| r.revenue))),
                Arc::new(StringArray::from_iter(rows().map(|r| r.top.map(|c| s(&cats[c as usize]))))),
            ];
            let batch = RecordBatch::try_new(us.clone(), cols).unwrap();
            let f = File::create(ud_dir.join(format!("part-{:04}.parquet", pi))).unwrap();
            let mut w = ArrowWriter::try_new(f, us.clone(), Some(props.clone())).unwrap();
            w.write(&batch).unwrap();
            w.close().unwrap();
        });
    };
    let write_camps = || {
        let cs = Arc::new(Schema::new(vec![
            Field::new("campaign_id", DataType::Utf8, false),
            Field::new("channel", DataType::Utf8, false),
            Field::new("day", DataType::Date32, false),
            Field::new("sessions", DataType::Int64, false),
            Field::new("purchasing_sessions", DataType::Int64, false),
            Field::new("revenue_cents", DataType::Int64, false),
            Field::new("assisted_sessions", DataType::Int64, false),
        ]));
        let mut cid = Vec::new();
        let mut chan = Vec::new();
        let mut cday = Vec::new();
        let mut cv: [Vec<i64>; 4] = Default::default();
        for c in 0..ncamp {
            for d in 0..ndays {
                let v = camp[c * ndays + d];
                if v[0] == 0 && v[3] == 0 {
                    continue;
                }
                if c as u32 == organic {
                    cid.push("organic".to_string());
                    chan.push("organic".to_string());
                } else {
                    cid.push(s(camp_names[c].0));
                    chan.push(s(camp_names[c].1));
                }
                cday.push((ws_day + d as i64) as i32);
                for t in 0..4 {
                    cv[t].push(v[t]);
                }
            }
        }
        let cols: Vec<ArrayRef> = vec![
            Arc::new(StringArray::from(cid)),
            Arc::new(StringArray::from(chan)),
            Arc::new(Date32Array::from(cday)),
            Arc::new(Int64Array::from(std::mem::take(&mut cv[0]))),
            Arc::new(Int64Array::from(std::mem::take(&mut cv[1]))),
            Arc::new(Int64Array::from(std::mem::take(&mut cv[2]))),
            Arc::new(Int64Array::from(std::mem::take(&mut cv[3]))),
        ];
        let batch = RecordBatch::try_new(cs.clone(), cols).unwrap();
        let f = File::create(cd_dir.join("part-0000.parquet")).unwrap();
        let mut w = ArrowWriter::try_new(f, cs, Some(props.clone())).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
    };
    rayon::join(write_users, write_camps);
    lap("written");
    // skip freeing ~GBs of memory and unmapping inputs: the kernel reclaims it at exit
    std::process::exit(0);
}

/// Category with the highest summed purchase cents (ties: byte-wise smallest = lowest index); resets the sums.
fn take_top(catsum: &mut [i64], touched: &mut Vec<u16>) -> Option<u16> {
    let mut best: Option<u16> = None;
    for &c in touched.iter() {
        let v = catsum[c as usize];
        best = match best {
            None => Some(c),
            Some(b) => {
                let bv = catsum[b as usize];
                if v > bv || (v == bv && c < b) { Some(c) } else { Some(b) }
            }
        };
    }
    for &c in touched.iter() {
        catsum[c as usize] = 0;
    }
    touched.clear();
    best
}

#[cfg(test)]
mod parse_fast_tests {
    use super::*;

    fn rng(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    #[test]
    fn ts_and_price_match_reference() {
        let mut fixed: Vec<Vec<u8>> = [
            "2028-02-29T12:00:00Z", "2027-02-29T12:00:00Z", "2100-02-29T00:00:00Z", "2000-02-29T00:00:00Z",
            "2026-11-20T23:59:59Z", "2026-11-20T24:00:00Z", "2026-11-20T23:60:00Z", "2026-11-20T23:59:60Z",
            "2026-13-01T00:00:00Z", "2026-00-01T00:00:00Z", "2026-04-31T00:00:00Z", "2026-04-00T00:00:00Z",
            "2026-11-20T10:00:00+12:00", "2026-11-20T10:00:00-10:00", "2026-11-20T10:00:00+24:00",
            "2026-11-20T10:00:00+05:60", "2026-11-20T10:00:00*05:00", "2026-11-20T10:00:00+0500x",
            "2026-11-20 10:00:00Z", "2026/11/20T10:00:00Z", "2026-11-20T10:00:00z", "2026-11-20T10:00:0aZ",
            "0000-01-01T00:00:00Z", "9999-12-31T23:59:59Z", "1789000000000", "0001789000000000", "0",
            "000000000000000000", "999999999999999999", "1234567890123456789", "00000000000001700000",
            "12a", "", " 1", "1 ", "-1", "+1", "1.5", "17890000000000000000000",
        ]
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .collect();
        let alphabet = b"0123456789-:TZ+ .x\x00\xff/";
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..400_000 {
            let base = fixed[(rng(&mut s) % 39) as usize].clone();
            let mut v = base;
            for _ in 0..(rng(&mut s) % 3) {
                if v.is_empty() {
                    break;
                }
                let i = (rng(&mut s) as usize) % v.len();
                v[i] = alphabet[(rng(&mut s) as usize) % alphabet.len()];
            }
            fixed.push(v);
        }
        for _ in 0..200_000 {
            let n = (rng(&mut s) % 27) as usize;
            fixed.push((0..n).map(|_| alphabet[(rng(&mut s) as usize) % alphabet.len()]).collect());
        }
        for v in &fixed {
            assert_eq!(parse_ts_fast(Some(v)), parse_ts(Some(v)), "ts {:?}", String::from_utf8_lossy(v));
            assert_eq!(parse_price_fast(Some(v)), parse_price(Some(v)), "price {:?}", String::from_utf8_lossy(v));
        }
        assert_eq!(parse_ts_fast(None), parse_ts(None));
        assert_eq!(parse_price_fast(None), parse_price(None));
    }
}
