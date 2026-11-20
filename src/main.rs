//! Shopping Sessions pipeline. Implements rules.md exactly; ref/reference.py is the oracle.
//!
//! Flow: mmap every CSV -> parallel line parsing in ~8 MB chunks -> exact event dedup
//! (sort by hash, compare bytes) -> validate -> intern user IDs -> union-find over links
//! (root = byte-wise smallest ID) -> sort events and clicks by (person, ts) -> per person:
//! sessions, credit, assists (sliding 7-day click window) -> aggregate -> Parquet.

use std::collections::HashMap;
use std::fs::File;
use std::hash::BuildHasher;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::{ArrayRef, Date32Array, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use memmap2::Mmap;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use rayon::prelude::*;

const DAY: i64 = 86_400_000;
const GAP: i64 = 1_800_000;
const WEEK: i64 = 7 * DAY;
const CHUNK: usize = 8 << 20;
const BAD: i64 = i64::MIN;

type Hasher = foldhash::fast::FixedState;
fn hasher() -> Hasher {
    Hasher::with_seed(0x5eed_1234_abcd_9876)
}

// ------------------------------------------------------------------ files and chunks
struct Src {
    maps: Vec<Mmap>,
}

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

fn open_all(paths: &[PathBuf]) -> Src {
    let maps = paths
        .par_iter()
        .filter_map(|p| {
            let f = File::open(p).ok()?;
            if f.metadata().ok()?.len() == 0 {
                return None;
            }
            unsafe { Mmap::map(&f).ok() }
        })
        .collect();
    Src { maps }
}

/// Byte ranges (map index, start, end) of data lines, skipping each file's header line.
fn chunks(src: &Src) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    for (i, m) in src.maps.iter().enumerate() {
        let d: &[u8] = m;
        let start = match memchr::memchr(b'\n', d) {
            Some(p) => p + 1,
            None => continue, // header only
        };
        let mut s = start;
        while s < d.len() {
            let mut e = (s + CHUNK).min(d.len());
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

/// Calls f(line) for every line in data (lines end at \n; a final unterminated piece is a line if non-empty).
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
    let mut n = 0;
    let mut s = 0;
    let mut push = |f: &'a [u8], n: &mut usize| -> bool {
        if *n >= N {
            return false;
        }
        let v = if memchr::memchr(b'"', f).is_some() {
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
        if !push(&line[s..p], &mut n) {
            return None;
        }
        s = p + 1;
    }
    if !push(&line[s..], &mut n) {
        return None;
    }
    if n == N {
        Some(out)
    } else {
        None
    }
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
            return BAD; // far outside any window (and possibly past 64 bits)
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
        None => return -1, // zero
        Some(p) => &v[p..],
    };
    if t.len() > 8 {
        return -1;
    }
    let x = t.iter().fold(0i64, |a, &c| a * 10 + (c - b'0') as i64);
    if (1..=10_000_000).contains(&x) { x } else { -1 }
}

// ------------------------------------------------------------------ small tables
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

// ------------------------------------------------------------------ interning
/// Map every key to a dense id; equal byte strings get equal ids. Returns (ids, uniques).
fn intern<'a>(keys: &[&'a [u8]]) -> (Vec<u32>, Vec<&'a [u8]>) {
    const SH: usize = 128;
    let hs = hasher();
    let hashes: Vec<u64> = keys.par_iter().map(|k| hs.hash_one(k)).collect();
    // partition key indices by shard, in parallel blocks
    let block = (keys.len() / (rayon::current_num_threads() * 4)).max(1 << 16);
    let parts: Vec<Vec<Vec<u32>>> = hashes
        .par_chunks(block)
        .enumerate()
        .map(|(bi, hc)| {
            let mut b: Vec<Vec<u32>> = (0..SH).map(|_| Vec::new()).collect();
            for (j, h) in hc.iter().enumerate() {
                b[(h >> 57) as usize % SH].push((bi * block + j) as u32);
            }
            b
        })
        .collect();
    let shard_res: Vec<(Vec<(u32, u32)>, Vec<&'a [u8]>)> = (0..SH)
        .into_par_iter()
        .map(|s| {
            let mut m: HashMap<&'a [u8], u32, Hasher> = HashMap::with_hasher(hasher());
            let mut uniq = Vec::new();
            let mut asg = Vec::new();
            for p in &parts {
                for &i in &p[s] {
                    let k = keys[i as usize];
                    let id = *m.entry(k).or_insert_with(|| {
                        uniq.push(k);
                        (uniq.len() - 1) as u32
                    });
                    asg.push((i, id));
                }
            }
            (asg, uniq)
        })
        .collect();
    let mut offs = Vec::with_capacity(SH);
    let mut tot = 0u32;
    for (_, u) in &shard_res {
        offs.push(tot);
        tot += u.len() as u32;
    }
    let mut ids = vec![0u32; keys.len()];
    let mut uniq_all = Vec::with_capacity(tot as usize);
    for (s, (asg, u)) in shard_res.iter().enumerate() {
        for &(i, id) in asg {
            ids[i as usize] = offs[s] + id;
        }
        uniq_all.extend_from_slice(u);
    }
    (ids, uniq_all)
}

// ------------------------------------------------------------------ records
#[derive(Clone, Copy)]
struct RawEv<'a> {
    eid: &'a [u8],
    uid: Option<&'a [u8]>,
    pid: Option<&'a [u8]>,
    ts: i64,
    ing: i64,
    typ: u8, // 0 view, 1 cart, 2 purchase, 3 invalid
    price: i64,
}

struct Click<'a> {
    cid: &'a [u8],
    uid: &'a [u8],
    camp: u32,
    ts: i64,
}

fn main() {
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    if let Some(n) = std::env::var("NCPU").ok().and_then(|v| v.parse::<usize>().ok()) {
        let _ = rayon::ThreadPoolBuilder::new().num_threads(n.max(1)).build_global();
    }
    let args: Vec<String> = std::env::args().collect();
    let data = PathBuf::from(if args.len() > 2 { args[1].clone() } else { env("DATA_DIR", "/data") });
    let out = PathBuf::from(if args.len() > 2 { args[2].clone() } else { env("OUTPUT_DIR", "/output") });
    let t0 = std::time::Instant::now();
    let lap = |what: &str| eprintln!("{:>8.3}s  {}", t0.elapsed().as_secs_f64(), what);

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

    // small tables
    let products = read_small(&data.join("products.csv"));
    let campaigns = read_small(&data.join("campaigns.csv"));
    let mut cats: Vec<Vec<u8>> = products.iter().map(|r| r[1].clone()).collect();
    cats.push(b"unknown".to_vec());
    cats.sort();
    cats.dedup();
    let cat_idx: HashMap<Vec<u8>, u16, Hasher> = cats.iter().enumerate().map(|(i, c)| (c.clone(), i as u16)).collect();
    let mut prod: HashMap<&[u8], u16, Hasher> = HashMap::with_hasher(hasher());
    for r in &products {
        prod.insert(&r[0], cat_idx[&r[1]]);
    }
    let unknown = cat_idx[&b"unknown".to_vec()];
    let mut camp_map: HashMap<&[u8], u32, Hasher> = HashMap::with_hasher(hasher());
    for r in &campaigns {
        let n = camp_map.len() as u32;
        camp_map.entry(&r[0]).or_insert(n);
    }
    let mut camp_names: Vec<(&[u8], &[u8])> = vec![(b"", b""); camp_map.len()];
    for r in &campaigns {
        let i = camp_map[r[0].as_slice()] as usize;
        camp_names[i] = (&r[0], &r[1]);
    }
    let organic = camp_names.len() as u32;

    let ev_src = open_all(&list_csv(&data.join("events")));
    let ck_src = open_all(&list_csv(&data.join("clicks")));
    let lk_src = open_all(&list_csv(&data.join("links")));
    lap("opened");

    // ---------------- parse events
    let ev_chunks = chunks(&ev_src);
    let evs: Vec<Vec<RawEv>> = ev_chunks
        .par_iter()
        .map(|&(mi, s, e)| {
            let d: &[u8] = &ev_src.maps[mi][s..e];
            let mut v = Vec::with_capacity(d.len() / 70);
            for_lines(d, |l| {
                if let Some(f) = fields::<7>(l) {
                    let eid = match f[0] {
                        Some(x) => x,
                        None => return,
                    };
                    let typ = match f[4] {
                        Some(b"view") => 0,
                        Some(b"cart") => 1,
                        Some(b"purchase") => 2,
                        _ => 3,
                    };
                    let ing = parse_ts(f[3]);
                    v.push(RawEv {
                        eid,
                        uid: f[1],
                        pid: f[5],
                        ts: parse_ts(f[2]),
                        ing: if ing == BAD { i64::MAX } else { ing },
                        typ,
                        price: if typ == 2 { parse_price(f[6]) } else { 0 },
                    });
                }
            });
            v
        })
        .collect();
    let mut offs = Vec::with_capacity(evs.len());
    let mut n_ev = 0usize;
    for v in &evs {
        offs.push(n_ev);
        n_ev += v.len();
    }
    lap(&format!("parsed {} event lines", n_ev));

    // ---------------- dedup on event_id: smallest ingest_ts wins (exact: sort by hash then compare bytes)
    let hs = hasher();
    let mut keys: Vec<(u64, i64, u32)> = vec![(0, 0, 0); n_ev];
    {
        let mut rest: &mut [(u64, i64, u32)] = &mut keys;
        let mut parts = Vec::with_capacity(evs.len());
        for v in &evs {
            let (a, b) = rest.split_at_mut(v.len());
            parts.push(a);
            rest = b;
        }
        parts.into_par_iter().zip(evs.par_iter()).zip(offs.par_iter()).for_each(|((dst, v), &o)| {
            for (j, e) in v.iter().enumerate() {
                dst[j] = (hs.hash_one(e.eid), e.ing, (o + j) as u32);
            }
        });
    }
    keys.par_sort_unstable();
    let get = |g: u32| -> &RawEv {
        let g = g as usize;
        let ci = offs.partition_point(|&o| o <= g) - 1;
        &evs[ci][g - offs[ci]]
    };
    // split into ranges at hash boundaries and keep the first copy of each distinct eid
    let nth = rayon::current_num_threads() * 8;
    let mut bounds = vec![0usize];
    for k in 1..nth {
        let mut p = n_ev * k / nth;
        while p > 0 && p < n_ev && keys[p].0 == keys[p - 1].0 {
            p += 1;
        }
        if p > *bounds.last().unwrap() && p < n_ev {
            bounds.push(p);
        }
    }
    bounds.push(n_ev);
    let kept: Vec<Vec<u32>> = bounds
        .par_windows(2)
        .map(|w| {
            let mut out = Vec::with_capacity(w[1] - w[0]);
            let mut i = w[0];
            while i < w[1] {
                let mut j = i + 1;
                while j < w[1] && keys[j].0 == keys[i].0 {
                    j += 1;
                }
                if j == i + 1 {
                    out.push(keys[i].2);
                } else {
                    for a in i..j {
                        let ea = get(keys[a].2).eid;
                        if !(i..a).any(|b| get(keys[b].2).eid == ea) {
                            out.push(keys[a].2);
                        }
                    }
                }
                i = j;
            }
            out
        })
        .collect();
    drop(keys);
    lap("dedup");

    // ---------------- validate kept events
    struct Ev<'a> {
        uid: &'a [u8],
        ts: i64,
        price: i64, // >0 for purchases
        cat: u16,
    }
    let good: Vec<Ev> = kept
        .par_iter()
        .flat_map_iter(|ks| {
            ks.iter().filter_map(|&g| {
                let e = get(g);
                let uid = e.uid?;
                let pid = e.pid?;
                if e.ts == BAD || e.ts < ws || e.ts >= we || e.typ == 3 {
                    return None;
                }
                if e.typ == 2 && e.price < 0 {
                    return None;
                }
                if e.ing - e.ts > DAY {
                    return None;
                }
                Some(Ev {
                    uid,
                    ts: e.ts,
                    price: if e.typ == 2 { e.price } else { 0 },
                    cat: *prod.get(pid).unwrap_or(&unknown),
                })
            })
        })
        .collect();
    drop(kept);
    lap(&format!("{} valid events", good.len()));

    // ---------------- clicks
    let ck_chunks = chunks(&ck_src);
    let cks: Vec<Click> = ck_chunks
        .par_iter()
        .flat_map_iter(|&(mi, s, e)| {
            let d: &[u8] = &ck_src.maps[mi][s..e];
            let mut v = Vec::with_capacity(d.len() / 45);
            for_lines(d, |l| {
                if let Some(f) = fields::<4>(l) {
                    if let Some(cid) = f[0] {
                        // validity does not depend on which copy is kept (copies are identical)
                        let uid = match f[1] {
                            Some(u) => u,
                            None => return,
                        };
                        let ts = parse_ts(f[3]);
                        if ts == BAD || ts < ws - WEEK || ts >= we {
                            return;
                        }
                        let camp = match f[2].and_then(|c| camp_map.get(c)) {
                            Some(&c) => c,
                            None => return,
                        };
                        v.push(Click { cid, uid, camp, ts });
                    }
                }
            });
            v.into_iter()
        })
        .collect();
    lap(&format!("{} valid click lines", cks.len()));

    // ---------------- links
    let lk_chunks = chunks(&lk_src);
    let links: Vec<(&[u8], &[u8])> = lk_chunks
        .par_iter()
        .flat_map_iter(|&(mi, s, e)| {
            let d: &[u8] = &lk_src.maps[mi][s..e];
            let mut v = Vec::new();
            for_lines(d, |l| {
                if let Some([Some(a), Some(b)]) = fields::<2>(l) {
                    v.push((a, b));
                }
            });
            v.into_iter()
        })
        .collect();

    // ---------------- intern all user IDs (events, clicks, links)
    let mut ukeys: Vec<&[u8]> = Vec::with_capacity(good.len() + cks.len() + links.len() * 2);
    ukeys.extend(good.iter().map(|e| e.uid));
    ukeys.extend(cks.iter().map(|c| c.uid));
    for &(a, b) in &links {
        ukeys.push(a);
        ukeys.push(b);
    }
    let (uids, names) = intern(&ukeys);
    drop(ukeys);
    lap(&format!("{} distinct user ids", names.len()));

    // union-find; the root of every component is its byte-wise smallest ID
    let mut parent: Vec<u32> = (0..names.len() as u32).collect();
    fn find(p: &mut [u32], mut x: u32) -> u32 {
        let mut r = x;
        while p[r as usize] != r {
            r = p[r as usize];
        }
        while p[x as usize] != r {
            let n = p[x as usize];
            p[x as usize] = r;
            x = n;
        }
        r
    }
    let lbase = good.len() + cks.len();
    for k in 0..links.len() {
        let a = find(&mut parent, uids[lbase + 2 * k]);
        let b = find(&mut parent, uids[lbase + 2 * k + 1]);
        if a != b {
            if names[a as usize] < names[b as usize] {
                parent[b as usize] = a;
            } else {
                parent[a as usize] = b;
            }
        }
    }
    for i in 0..parent.len() as u32 {
        find(&mut parent, i);
    }
    let parent = parent; // fully compressed: parent[x] is x's root
    lap("links");

    // ---------------- sort events and clicks by (person, ts)
    let mut ek: Vec<(u64, u32)> = good
        .par_iter()
        .enumerate()
        .map(|(i, e)| (((parent[uids[i] as usize] as u64) << 32) | (e.ts - ws) as u64, i as u32))
        .collect();
    ek.par_sort_unstable();
    let cbase = good.len();
    let cws = ws - WEEK;
    // clicks: dedup on click_id first (copies identical; keep one)
    let ckeys: Vec<&[u8]> = cks.iter().map(|c| c.cid).collect();
    let (cids, _) = intern(&ckeys);
    drop(ckeys);
    let mut seen = vec![false; cids.iter().map(|&x| x as usize + 1).max().unwrap_or(0)];
    let mut ck: Vec<(u64, u32)> = Vec::with_capacity(cks.len());
    for (i, c) in cks.iter().enumerate() {
        let id = cids[i] as usize;
        if seen[id] {
            continue;
        }
        seen[id] = true;
        ck.push((((parent[uids[cbase + i] as usize] as u64) << 32) | (c.ts - cws) as u64, i as u32));
    }
    drop(seen);
    // within equal (person, ts), order by click_id bytes so the first is the tie winner
    ck.par_sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| cks[a.1 as usize].cid.cmp(cks[b.1 as usize].cid)));
    lap("sorted");

    // ---------------- sessions, credit, assists, aggregation (parallel over person ranges)
    let ncamp = organic as usize + 1;
    struct UserRow {
        person: u32,
        day: i64,
        sessions: i64,
        events: i64,
        revenue: i64,
        top: Option<u16>,
    }
    let n = ek.len();
    let parts = rayon::current_num_threads() * 16;
    let mut eb = vec![0usize];
    for k in 1..parts {
        let mut p = n * k / parts;
        while p > 0 && p < n && (ek[p].0 >> 32) == (ek[p - 1].0 >> 32) {
            p += 1;
        }
        if p > *eb.last().unwrap() && p < n {
            eb.push(p);
        }
    }
    eb.push(n);
    let ncats = cats.len();
    let results: Vec<(Vec<UserRow>, Vec<[i64; 4]>)> = eb
        .par_windows(2)
        .map(|w| {
            let mut rows: Vec<UserRow> = Vec::new();
            let mut camp = vec![[0i64; 4]; ncamp * ndays];
            let mut cnt = vec![0u32; ncamp];
            let mut active: Vec<u32> = Vec::new();
            let mut catsum = vec![0i64; ncats];
            let mut touched: Vec<u16> = Vec::new();
            let mut i = w[0];
            while i < w[1] {
                let person = (ek[i].0 >> 32) as u32;
                let mut j = i;
                while j < w[1] && (ek[j].0 >> 32) as u32 == person {
                    j += 1;
                }
                // this person's clicks
                let lo = ck.partition_point(|c| (c.0 >> 32) < person as u64);
                let hi = ck.partition_point(|c| (c.0 >> 32) <= person as u64);
                let pc = &ck[lo..hi];
                let mut cl = 0usize; // first click with ts >= start - WEEK
                let mut cr = 0usize; // first click with ts > start
                for &a in &active {
                    cnt[a as usize] = 0;
                }
                active.clear();
                // walk sessions
                let mut cur_day = i64::MIN;
                let mut row: Option<UserRow> = None;
                let mut k = i;
                while k < j {
                    let start = (ek[k].0 & 0xffff_ffff) as i64 + ws;
                    let mut prev = start;
                    let mut m = k + 1;
                    while m < j {
                        let t = (ek[m].0 & 0xffff_ffff) as i64 + ws;
                        if t - prev > GAP {
                            break;
                        }
                        prev = t;
                        m += 1;
                    }
                    // session = events k..m
                    let day = start.div_euclid(DAY);
                    let di = (day - ws_day) as usize;
                    if day != cur_day {
                        if let Some(mut r) = row.take() {
                            r.top = take_top(&mut catsum, &mut touched);
                            rows.push(r);
                        }
                        cur_day = day;
                        row = Some(UserRow { person, day, sessions: 0, events: 0, revenue: 0, top: None });
                    }
                    let mut rev = 0i64;
                    let mut purch = false;
                    for q in k..m {
                        let e = &good[ek[q].1 as usize];
                        if e.price > 0 {
                            rev += e.price;
                            purch = true;
                            if catsum[e.cat as usize] == 0 {
                                touched.push(e.cat);
                            }
                            catsum[e.cat as usize] += e.price;
                        }
                    }
                    // slide the click window to [start - WEEK, start]
                    let rel_hi = (start - cws) as u64; // click key low bits <= this
                    let rel_lo = (start - WEEK - cws) as u64;
                    while cr < pc.len() && (pc[cr].0 & 0xffff_ffff) <= rel_hi {
                        let c = cks[pc[cr].1 as usize].camp;
                        if cnt[c as usize] == 0 {
                            active.push(c);
                        }
                        cnt[c as usize] += 1;
                        cr += 1;
                    }
                    while cl < cr && (pc[cl].0 & 0xffff_ffff) < rel_lo {
                        let c = cks[pc[cl].1 as usize].camp;
                        cnt[c as usize] -= 1;
                        cl += 1;
                    }
                    let credit = if cl < cr {
                        // latest ts in window; among equals the smallest click_id (sorted first)
                        let top = pc[cr - 1].0;
                        let mut f = cr - 1;
                        while f > cl && pc[f - 1].0 == top {
                            f -= 1;
                        }
                        cks[pc[f].1 as usize].camp
                    } else {
                        organic
                    };
                    let slot = &mut camp[credit as usize * ndays + di];
                    slot[0] += 1;
                    slot[1] += purch as i64;
                    slot[2] += rev;
                    active.retain(|&c| cnt[c as usize] > 0);
                    for &c in &active {
                        camp[c as usize * ndays + di][3] += 1;
                    }
                    let r = row.as_mut().unwrap();
                    r.sessions += 1;
                    r.events += (m - k) as i64;
                    r.revenue += rev;
                    k = m;
                }
                if let Some(mut r) = row.take() {
                    r.top = take_top(&mut catsum, &mut touched);
                    rows.push(r);
                }
                i = j;
            }
            (rows, camp)
        })
        .collect();
    lap("aggregated");

    // ---------------- merge campaign counters
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
    results.par_iter().enumerate().for_each(|(pi, (rows, _))| {
        if rows.is_empty() && pi != 0 {
            return;
        }
        let cols: Vec<ArrayRef> = vec![
            Arc::new(StringArray::from_iter_values(rows.iter().map(|r| s(names[r.person as usize])))),
            Arc::new(Date32Array::from_iter_values(rows.iter().map(|r| r.day as i32))),
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.sessions))),
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.events))),
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.revenue))),
            Arc::new(StringArray::from_iter(rows.iter().map(|r| r.top.map(|c| s(&cats[c as usize]))))),
        ];
        let batch = RecordBatch::try_new(us.clone(), cols).unwrap();
        let f = File::create(ud_dir.join(format!("part-{:04}.parquet", pi))).unwrap();
        let mut w = ArrowWriter::try_new(f, us.clone(), Some(props.clone())).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
    });
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
    let mut w = ArrowWriter::try_new(f, cs, Some(props)).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
    lap("written");
    let _ = (n_ev, cbase, unknown);
}

/// Category with the highest summed purchase cents (ties: byte-wise smallest, i.e. lowest index); resets the sums.
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
