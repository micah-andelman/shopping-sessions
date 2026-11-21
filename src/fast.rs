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

pub fn run(data: &Path, out: &Path) -> bool {
    let t0 = std::time::Instant::now();
    let verbose = std::env::var("SS_VERBOSE").is_ok();
    let lap = |what: &str| {
        if verbose {
            eprintln!("{:>8.3}s  {}", t0.elapsed().as_secs_f64(), what)
        }
    };

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
    let ev_groups: Vec<&'static [Ch]> = split_groups(ev_chunks, std::env::var("SS_GROUPS").ok().and_then(|v| v.parse().ok()).unwrap_or(nthreads));
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
        const LANES: usize = 4;
        for lane in 0..LANES {
            std::thread::spawn(move || {
                for (i, &(m, s, e)) in wins.iter().enumerate() {
                    if i % LANES == lane {
                        let s0 = s & !4095;
                        let _ = m.advise_range(memmap2::Advice::WillNeed, s0, e - s0);
                    }
                }
            });
        }
    }

    // ---------------- links -> table: id key -> person key (root = byte-wise smallest id)
    let link_job = || -> Option<(hashbrown::HashTable<(u64, u64)>, Fb)> {
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
        for (v, _) in &parts {
            for &(a, b) in v {
                let ra = find(&mut parent, get(a));
                let rb = find(&mut parent, get(b));
                if ra != rb {
                    if ra < rb { parent[rb as usize] = ra } else { parent[ra as usize] = rb }
                }
            }
        }
        let mut table: hashbrown::HashTable<(u64, u64)> = hashbrown::HashTable::with_capacity(lids.len());
        for i in 0..lids.len() as u32 {
            let r = find(&mut parent, i);
            let k = lids[i as usize];
            table.insert_unique(th(k), (k, lids[r as usize]), |e| th(e.0));
        }
        let fb: Fb = parts.into_iter().flat_map(|p| p.1).collect();
        Some((table, fb))
    };

    // ---------------- events: parse every copy, scatter by event_id bucket
    let ev_job = || -> Vec<(Vec<Vec<Ev>>, Fb)> {
        ev_groups
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
                        let mut price = 0u64;
                        if ok && typ == 2 {
                            let p = parse_price(f[6]);
                            if p < 0 { ok = false } else { price = p as u64 }
                        }
                        let (ukey, lo) = if ok {
                            let cat = prod.get(f[5].unwrap()).map(|c| c as u64).unwrap_or(unknown);
                            (key_of(f[1].unwrap(), &mut fb), ((ts - ws) as u64) << 33 | price << 9 | cat)
                        } else {
                            (0, NONE)
                        };
                        bk[bucket(ekey)].push(Ev { ekey, ukey, ing, lo });
                    });
                }
                (bk, fb)
            })
            .collect()
    };

    let (links, evp) = rayon::join(link_job, ev_job);
    let (ltab, lfb) = match links {
        Some(x) => x,
        None => return false,
    };
    lap(&format!("links ({} ids) and events parsed", ltab.len()));
    let person = |u: u64| -> u64 {
        match ltab.find(th(u), |e| e.0 == u) {
            Some(e) => e.1,
            None => u,
        }
    };

    // ---------------- dedup per bucket (smallest ingest wins; kept-but-invalid drops the event)
    let dedup_job = || -> Vec<Vec<(u64, u64)>> {
        (0..NB)
            .into_par_iter()
            .map(|b| {
                let n: usize = evp.iter().map(|g| g.0[b].len()).sum();
                let mut t: hashbrown::HashTable<Ev> = hashbrown::HashTable::with_capacity(n);
                for g in &evp {
                    for e in &g.0[b] {
                        let h = th(e.ekey);
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
                let mut out = Vec::with_capacity(t.len());
                for e in t.iter() {
                    if e.lo != NONE {
                        out.push((person(e.ukey), e.lo));
                    }
                }
                out
            })
            .collect()
    };

    // ---------------- clicks (no dedup needed: copies are identical)
    let ck_job = || -> (Vec<Ck>, Fb) {
        let ch = chunks(ck_maps);
        let parts: Vec<(Vec<Ck>, Fb)> = ch
            .par_iter()
            .map(|&(mi, s, e)| {
                let d: &'static [u8] = &ck_maps[mi][s..e];
                let mut v = Vec::with_capacity(d.len() / 45 + 16);
                let mut fb = Fb::new();
                scan::<4>(d, |f| {
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
                    let pkey = person(key_of(uid, &mut fb));
                    v.push(Ck { pkey, lo: ((ts - cws) as u64) << 32 | camp as u64, cid: key_of(cid, &mut fb) });
                });
                (v, fb)
            })
            .collect();
        let n: usize = parts.iter().map(|p| p.0.len()).sum();
        let mut ck = Vec::with_capacity(n);
        let mut fb = Fb::new();
        for (v, f) in parts {
            ck.extend_from_slice(&v);
            fb.extend(f);
        }
        (ck, fb)
    };

    let (eparts, (mut ck, cfb)) = if std::env::var("SS_SEQ").is_ok() {
        let a = dedup_job();
        lap("  dedup alone");
        let b = ck_job();
        lap("  clicks alone");
        (a, b)
    } else {
        rayon::join(dedup_job, ck_job)
    };
    lap("dedup + clicks");
    let fbm = match merge_fb(evp.iter().map(|g| &g.1).chain(std::iter::once(&lfb)).chain(std::iter::once(&cfb))) {
        Some(m) => m,
        None => return false,
    };
    drop(evp);

    // concatenate survivors
    let n: usize = eparts.iter().map(|v| v.len()).sum();
    let mut ek: Vec<(u64, u64)> = Vec::with_capacity(n);
    unsafe { ek.set_len(n) };
    {
        let mut offs = Vec::with_capacity(eparts.len());
        let mut o = 0usize;
        for v in &eparts {
            offs.push(o);
            o += v.len();
        }
        let p = Raw(ek.as_mut_ptr());
        eparts.par_iter().zip(offs.par_iter()).for_each(|(v, &o)| {
            let p = &p;
            unsafe { std::ptr::copy_nonoverlapping(v.as_ptr(), p.0.add(o), v.len()) };
        });
    }
    drop(eparts);
    lap(&format!("{} valid events, {} valid clicks", n, ck.len()));

    rayon::join(|| ek.par_sort_unstable(), || ck.par_sort_unstable_by_key(|c| (c.pkey, c.lo)));
    lap("sorted");

    // ---------------- sessions, credit, assists (parallel over person ranges)
    let ncamp = organic as usize + 1;
    let ncats = cats.len();
    let parts = nthreads * 16;
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
            let mut ci = ck.partition_point(|c| c.pkey < ek[w[0]].0);
            let mut i = w[0];
            while i < w[1] {
                let key = ek[i].0;
                let mut j = i + 1;
                while j < w[1] && ek[j].0 == key {
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
    let nf = nthreads;
    let per = results.len().div_ceil(nf).max(1);
    let groups: Vec<&[(Vec<UserRow>, Vec<[i64; 4]>)]> = results.chunks(per).collect();
    let write_users = || {
        groups.par_iter().enumerate().for_each(|(pi, g)| {
            let all: Vec<&UserRow> = g.iter().flat_map(|(r, _)| r.iter()).collect();
            let rows = || all.iter().copied();
            let cols: Vec<ArrayRef> = vec![
                Arc::new(StringArray::from_iter_values(rows().map(|r| s(&bytes_of(r.key, &fbm))))),
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
    true
}
