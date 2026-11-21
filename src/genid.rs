//! Generator ID decoding (engine2, stage S0). Written from the published generator source in
//! kit/generator (text.rs `scramble`/`base36`, user.rs, plan.rs); no external code.
//!
//! The generator writes every ID as a one-letter prefix plus lowercase base 36 of
//! `scramble(counter, key)`, where `scramble` is a bijection on 0..2^40:
//!     m = (v * M1 mod 2^40) ^ key;  m ^= m >> 17;  y = m * M2 mod 2^40.
//! Counters: event ID `e` = global event index; click ID `k` = global click index; user ID `u` =
//! person * 512 + alias slot, or 2^39 + n for the n-th shared kiosk. Keys are per seed (three
//! SplitMix64 draws in plan.rs); only their low 40 bits matter. We never hard-code a key: they are
//! recovered per run from sample IDs (`recover_key`), and any doubt means "no key" -> the caller
//! falls back to the generic engine for the whole run.

pub const MASK: u64 = (1 << 40) - 1;
const M1: u64 = 0x9E37_79B9_7F4B;
const M2: u64 = 0xD6E8_FEB8_6659;
const ALIAS_SLOTS: u64 = 512;
const KIOSK_BASE: u64 = 1 << 39;
/// kiosks per dataset are 3..=6 (plan.rs KIOSKS); leave room
const KIOSK_MAX: u64 = 64;

/// Multiplicative inverse of an odd number modulo 2^64 (Newton iteration), then reduced mod 2^40.
const fn inv(x: u64) -> u64 {
    let mut y = x; // correct to 3 bits for odd x
    let mut i = 0;
    while i < 6 {
        y = y.wrapping_mul(2u64.wrapping_sub(x.wrapping_mul(y)));
        i += 1;
    }
    y & MASK
}
const M1_INV: u64 = inv(M1);
const M2_INV: u64 = inv(M2);

#[inline(always)]
fn unxs17(y: u64) -> u64 {
    // inverse of x ^= x >> 17 on 40 bits
    (y ^ (y >> 17) ^ (y >> 34)) & MASK
}

/// The generator's scramble (for round-trip checks and encoding).
#[inline(always)]
pub fn scramble(v: u64, key: u64) -> u64 {
    let mut m = ((v & MASK).wrapping_mul(M1) ^ key) & MASK;
    m ^= m >> 17;
    m.wrapping_mul(M2) & MASK
}

/// Exact inverse of `scramble` for the same key.
#[inline(always)]
pub fn unscramble(y: u64, key: u64) -> u64 {
    let z = unxs17(y.wrapping_mul(M2_INV) & MASK);
    ((z ^ key) & MASK).wrapping_mul(M1_INV) & MASK
}

/// Key-independent part of the inverse: z = (v * M1 mod 2^40) ^ key.
#[inline(always)]
fn zpart(y: u64) -> u64 {
    unxs17(y.wrapping_mul(M2_INV) & MASK)
}

/// Canonical decode of `prefix` + lowercase base 36 (no leading zero unless the digits are exactly
/// "0", at most 8 digits, value < 2^40). Anything else (quotes already stripped by the caller,
/// upper case, other characters, wrong prefix, too long, out of range) -> None (odd path).
#[inline]
pub fn decode_b36(s: &[u8], prefix: u8) -> Option<u64> {
    if s.len() < 2 || s.len() > 9 || s[0] != prefix {
        return None;
    }
    let d = &s[1..];
    if d.len() > 1 && d[0] == b'0' {
        return None;
    }
    let mut v: u64 = 0;
    for &c in d {
        let x = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'z' => c - b'a' + 10,
            _ => return None,
        };
        v = v * 36 + x as u64;
    }
    if v > MASK {
        return None;
    }
    Some(v)
}

/// The generator's ID text for a scrambled value.
pub fn encode_b36(prefix: u8, y: u64) -> Vec<u8> {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = [0u8; 13];
    let mut at = buf.len();
    let mut r = y;
    loop {
        at -= 1;
        buf[at] = DIGITS[(r % 36) as usize];
        r /= 36;
        if r == 0 {
            break;
        }
    }
    let mut out = vec![prefix];
    out.extend_from_slice(&buf[at..]);
    out
}

/// Sentinel returned by the hot-path decoders for anything non-canonical.
pub const ODD: u64 = u64::MAX;

const fn b36_table() -> [u8; 256] {
    let mut t = [0x80u8; 256];
    let mut c = 0;
    while c < 10 {
        t[b'0' as usize + c] = c as u8;
        c += 1;
    }
    let mut c = 0;
    while c < 26 {
        t[b'a' as usize + c] = 10 + c as u8;
        c += 1;
    }
    t
}
static B36: [u8; 256] = b36_table();

/// Hot path: counter of a canonical `prefix` + lowercase base-36 ID under `key`, or ODD.
/// Branch-light: one table lookup per digit, the invalid flags OR-ed and tested once.
/// Same acceptance as `decode_b36` (tested equal on random and real strings).
#[inline(always)]
pub fn counter(s: &[u8], prefix: u8, key: u64) -> u64 {
    let n = s.len();
    if n < 2 || n > 9 || s[0] != prefix {
        return ODD;
    }
    let mut v: u64 = 0;
    let mut bad: u8 = 0;
    for &c in &s[1..] {
        let d = B36[c as usize];
        bad |= d;
        v = v * 36 + (d & 0x3f) as u64;
    }
    if bad & 0x80 != 0 || (n > 2 && s[1] == b'0') || v > MASK {
        return ODD;
    }
    unscramble(v, key)
}

/// Hot path: user counter (person * 512 + alias; kiosks at 2^39 + n) or ODD.
#[inline(always)]
pub fn user_counter(s: &[u8], key: u64) -> u64 {
    counter(s, b'u', key)
}

/// Hot path: global event index or ODD.
#[inline(always)]
pub fn event_index(s: &[u8], key: u64) -> u64 {
    counter(s, b'e', key)
}

/// Event key recovery using the user key: person 0 (a whale) owns the first event indices, so a
/// sampled row whose user decodes to person 0 can anchor a search over the bottom `low_frac` of
/// the index range only. `pairs` = (scrambled event id, scrambled user id) from the same rows.
/// Falls back to the full search if no person-0 row was sampled.
pub fn recover_event_key(pairs: &[(u64, u64)], user_key: u64, bound: u64, low_frac: f64) -> Option<u64> {
    let ys: Vec<u64> = pairs.iter().map(|p| p.0).collect();
    let anchor = pairs.iter().position(|&(_, uy)| unscramble(uy, user_key) / ALIAS_SLOTS == 0);
    if let Some(a) = anchor {
        let mut order: Vec<u64> = Vec::with_capacity(ys.len());
        order.push(ys[a]);
        order.extend(ys.iter().enumerate().filter(|&(i, _)| i != a).map(|(_, &y)| y));
        let low = ((bound as f64 * low_frac) as u64).max(1 << 12);
        // anchor 0 only: its counter is known to be < low
        let key = recover_key_bounded(&order, low, bound, 6, 0.99);
        if key.is_some() {
            return key;
        }
    }
    recover_key(&ys, bound, 1, 4, 6, 0.99, &|v| v < bound)
}

/// As recover_key with stride 1 and a single anchor (ys[0]) whose counter is < anchor_bound,
/// all counters < bound.
fn recover_key_bounded(ys: &[u64], anchor_bound: u64, bound: u64, verify: usize, min_share: f64) -> Option<u64> {
    if ys.len() < verify + 2 {
        return None;
    }
    let zs: Vec<u64> = ys.iter().map(|&y| zpart(y)).collect();
    let keys = search_range(zs[0], &zs[1..verify + 1], M1 & MASK, anchor_bound, &|v| v < bound);
    if keys.len() != 1 {
        return None;
    }
    let key = keys[0];
    let good = zs.iter().filter(|&&z| (((z ^ key) & MASK).wrapping_mul(M1_INV) & MASK) < bound).count();
    if good as f64 >= min_share * zs.len() as f64 { Some(key) } else { None }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserId {
    Person { person: u64, alias: u16 },
    Kiosk(u16),
}

/// Event ID -> global event index (None = odd: non-canonical or out of range).
#[inline]
pub fn decode_event(s: &[u8], key: u64, max_index: u64) -> Option<u64> {
    let v = unscramble(decode_b36(s, b'e')?, key);
    if v < max_index { Some(v) } else { None }
}

/// Click ID -> global click index.
#[inline]
pub fn decode_click(s: &[u8], key: u64, max_index: u64) -> Option<u64> {
    let v = unscramble(decode_b36(s, b'k')?, key);
    if v < max_index { Some(v) } else { None }
}

/// User ID -> (person, alias slot) or a kiosk number.
#[inline]
pub fn decode_user(s: &[u8], key: u64, max_person: u64) -> Option<UserId> {
    let v = unscramble(decode_b36(s, b'u')?, key);
    if v >= KIOSK_BASE {
        let n = v - KIOSK_BASE;
        return if n < KIOSK_MAX { Some(UserId::Kiosk(n as u16)) } else { None };
    }
    let (person, alias) = (v / ALIAS_SLOTS, v % ALIAS_SLOTS);
    if person < max_person { Some(UserId::Person { person, alias: alias as u16 }) } else { None }
}

/// Recover a 40-bit key from scrambled sample values `ys` (already canonical-decoded), knowing
/// every counter lies in `ok(v)` and that at least one of the first `anchors` samples has a
/// counter that is a multiple of `stride` below `bound` (stride 1 for events/clicks; 512 for users,
/// whose rows mostly use alias 0). For each anchor, every candidate counter is tried; a key
/// survives only if the next `verify` samples all decode in range. The result is accepted only
/// if it is unique and at least `min_share` of ALL samples decode in range (consistency check).
pub fn recover_key(ys: &[u64], bound: u64, stride: u64, anchors: usize, verify: usize, min_share: f64, ok: &(dyn Fn(u64) -> bool + Sync)) -> Option<u64> {
    if ys.len() < verify + 2 {
        return None;
    }
    let zs: Vec<u64> = ys.iter().map(|&y| zpart(y)).collect();
    let step = stride.wrapping_mul(M1) & MASK;
    for a in 0..anchors.min(zs.len() - verify - 1) {
        let za = zs[a];
        let others: Vec<u64> = zs.iter().enumerate().filter(|&(i, _)| i != a).map(|(_, &z)| z).take(verify).collect();
        let steps = bound.div_ceil(stride);
        let keys = search_range(za, &others, step, steps, ok);
        if keys.len() > 1 {
            return None; // ambiguous: refuse
        }
        if let Some(&key) = keys.first() {
            let good = zs.iter().filter(|&&z| ok(((z ^ key) & MASK).wrapping_mul(M1_INV) & MASK)).count();
            if good as f64 >= min_share * zs.len() as f64 {
                return Some(key);
            }
        }
    }
    None
}

/// Candidate keys: the anchor's counter runs over 0, s, 2s, ... (`steps` values; `step` = s*M1
/// mod 2^40); a key survives if every `others` counter passes `ok`. Split across the rayon pool.
fn search_range(za: u64, others: &[u64], step: u64, steps: u64, ok: &(dyn Fn(u64) -> bool + Sync)) -> Vec<u64> {
    use rayon::prelude::*;
    let d0 = za ^ others[0];
    let chunks = (rayon::current_num_threads() as u64 * 4).max(1).min(steps.max(1));
    let per = steps.div_ceil(chunks);
    let mut keys: Vec<u64> = (0..chunks)
        .into_par_iter()
        .flat_map_iter(|c| {
            let (lo, hi) = (c * per, ((c + 1) * per).min(steps));
            let mut w = lo.wrapping_mul(step) & MASK;
            let mut out = Vec::new();
            for _ in lo..hi {
                // first filter: the second sample's counter, without forming the key
                let v1 = ((d0 ^ w) & MASK).wrapping_mul(M1_INV) & MASK;
                if ok(v1) {
                    let key = (za ^ w) & MASK;
                    if others[1..].iter().all(|&z| ok(((z ^ key) & MASK).wrapping_mul(M1_INV) & MASK)) {
                        out.push(key);
                    }
                }
                w = (w + step) & MASK;
            }
            out
        })
        .collect();
    keys.sort_unstable();
    keys.dedup();
    keys
}

/// Test/diagnostic only: the generator's keys for a seed (plan.rs: Random::new(seed, Dataset, 0),
/// draws: below, 4 x rate, then user/event/click keys), masked to 40 bits.
pub fn true_keys_for_seed(seed: u64) -> (u64, u64, u64) {
    fn mix(mut v: u64) -> u64 {
        v = (v ^ (v >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        v = (v ^ (v >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        v ^ (v >> 31)
    }
    let mut state = mix(mix(seed ^ 0x5D58_8B65_6C07_8965) ^ mix(0 + 1) ^ 0u64.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let mut next = || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        mix(state)
    };
    for _ in 0..5 {
        next();
    }
    let u = next() & MASK;
    let e = next() & MASK;
    let c = next() & MASK;
    (u, e, c)
}

// ------------------------------------------------------------------ diagnostic run (SS_GENID_CHECK=1)

fn strip(f: &[u8]) -> &[u8] {
    if f.len() >= 2 && f[0] == b'"' && f[f.len() - 1] == b'"' { &f[1..f.len() - 1] } else { f }
}

fn read_rows(dir: &std::path::Path, n_fields: usize, mut f: impl FnMut(&[&[u8]])) {
    let mut files: Vec<_> = std::fs::read_dir(dir).map(|r| r.filter_map(|e| e.ok()).map(|e| e.path()).collect()).unwrap_or_default();
    files.retain(|p| p.extension().map_or(false, |x| x == "csv"));
    files.sort();
    for p in files {
        let data = match std::fs::read(&p) {
            Ok(d) => d,
            Err(_) => continue,
        };
        for line in data.split(|&c| c == b'\n').skip(1) {
            let parts: Vec<&[u8]> = line.split(|&c| c == b',').map(strip).collect();
            if parts.len() == n_fields {
                f(&parts);
            }
        }
    }
}

/// Recovers keys from the first rows of a dataset, then checks every ID and reports structure.
/// `seed`: if given, also compares with the generator's true keys.
pub fn check(data: &std::path::Path, seed: Option<u64>) {
    let t0 = std::time::Instant::now();
    let ev_dir = data.join("events");
    let ck_dir = data.join("clicks");
    let lk_dir = data.join("links");
    let ev_bytes: u64 = std::fs::read_dir(&ev_dir).map(|r| r.filter_map(|e| e.ok()).filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum()).unwrap_or(0);
    let ck_bytes: u64 = std::fs::read_dir(&ck_dir).map(|r| r.filter_map(|e| e.ok()).filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum()).unwrap_or(0);
    // generous counter bounds from input size (rows are >= ~50 bytes)
    let ev_bound = (ev_bytes / 40).max(1 << 16);
    let ck_bound = (ck_bytes / 25).max(1 << 16);
    let person_bound = (ev_bound / 15).max(1 << 12);
    // samples: the first canonical IDs in file order
    let (mut es, mut us, mut cs) = (Vec::new(), Vec::new(), Vec::new());
    let mut pairs: Vec<(u64, u64)> = Vec::new();
    read_rows(&ev_dir, 7, |f| {
        if es.len() < 5000 {
            let (e, u) = (decode_b36(f[0], b'e'), decode_b36(f[1], b'u'));
            if let Some(y) = e { es.push(y); }
            if let Some(y) = u { us.push(y); }
            if let (Some(a), Some(b)) = (e, u) { pairs.push((a, b)); }
        }
    });
    read_rows(&ck_dir, 4, |f| {
        if cs.len() < 4000 {
            if let Some(y) = decode_b36(f[0], b'k') { cs.push(y); }
        }
    });
    let tr = std::time::Instant::now();
    let ke = recover_key(&es, ev_bound, 1, 4, 6, 0.99, &|v| v < ev_bound);
    let te = tr.elapsed();
    let tr = std::time::Instant::now();
    let kc = recover_key(&cs, ck_bound, 1, 4, 6, 0.99, &|v| v < ck_bound);
    let tc = tr.elapsed();
    let tr = std::time::Instant::now();
    let user_ok = |v: u64| v < person_bound * ALIAS_SLOTS || (v >= KIOSK_BASE && v < KIOSK_BASE + KIOSK_MAX);
    let ku = recover_key(&us, person_bound * ALIAS_SLOTS, ALIAS_SLOTS, 8, 8, 0.95, &user_ok);
    let tu = tr.elapsed();
    if let Some(uk) = ku {
        let tw = std::time::Instant::now();
        let kw = recover_event_key(&pairs, uk, ev_bound, 0.025);
        eprintln!("genid: event key via person-0 anchor {:?} ({:.2} ms; equals full search: {})",
            kw.map(|k| format!("{:010x}", k)), tw.elapsed().as_secs_f64() * 1e3, kw == ke);
    }
    eprintln!("genid: samples ev {} user {} click {}; bounds ev {} click {} person {}", es.len(), us.len(), cs.len(), ev_bound, ck_bound, person_bound);
    eprintln!("genid: recovered event key {:?} ({:.1} ms), click key {:?} ({:.1} ms), user key {:?} ({:.1} ms)",
        ke.map(|k| format!("{:010x}", k)), te.as_secs_f64() * 1e3, kc.map(|k| format!("{:010x}", k)), tc.as_secs_f64() * 1e3,
        ku.map(|k| format!("{:010x}", k)), tu.as_secs_f64() * 1e3);
    if let Some(s) = seed {
        let (u, e, c) = true_keys_for_seed(s);
        eprintln!("genid: true keys for seed {}: user {:010x} event {:010x} click {:010x} -> match user {} event {} click {}",
            s, u, e, c, ku == Some(u), ke == Some(e), kc == Some(c));
    }
    let (ke, ku, kc) = match (ke, ku, kc) {
        (Some(a), Some(b), Some(c)) => (a, b, c),
        _ => {
            eprintln!("genid: key recovery FAILED -> generic path");
            return;
        }
    };
    // full pass: round trips, odd counts, structure
    let mut n_ev = 0u64;
    let mut odd_ev = 0u64;
    let mut odd_user = 0u64;
    let mut rt_fail = 0u64;
    let mut kiosk_rows = 0u64;
    let mut alias_hist = vec![0u64; 512];
    let mut rows: Vec<(u64, u64, i64)> = Vec::new(); // (event index, person, ts) of canonical rows with valid ts
    let mut check_rt = |s: &[u8], prefix: u8, key: u64, v: u64| {
        let y = scramble(v, key);
        if encode_b36(prefix, y) != s {
            rt_fail += 1;
        }
    };
    let mut hot_mismatch = 0u64;
    read_rows(&ev_dir, 7, |f| {
        n_ev += 1;
        let he = event_index(f[0], ke);
        let hr = decode_b36(f[0], b'e').map(|y| unscramble(y, ke)).unwrap_or(ODD);
        let hu = user_counter(f[1], ku);
        let hru = decode_b36(f[1], b'u').map(|y| unscramble(y, ku)).unwrap_or(ODD);
        if he != hr || hu != hru {
            hot_mismatch += 1;
        }
        let e = decode_event(f[0], ke, ev_bound);
        let u = decode_user(f[1], ku, person_bound);
        match e {
            Some(v) => check_rt(f[0], b'e', ke, v),
            None => odd_ev += 1,
        }
        match u {
            Some(UserId::Person { person, alias }) => {
                alias_hist[alias as usize] += 1;
                let v = person * ALIAS_SLOTS + alias as u64;
                check_rt(f[1], b'u', ku, v);
                if let Some(ei) = e {
                    let ts = super::parse_ts(Some(f[2]));
                    if ts != super::BAD {
                        rows.push((ei, person, ts));
                    }
                }
            }
            Some(UserId::Kiosk(n)) => {
                kiosk_rows += 1;
                check_rt(f[1], b'u', ku, KIOSK_BASE + n as u64);
            }
            None => {
                if !f[1].is_empty() {
                    odd_user += 1;
                }
            }
        }
    });
    let mut n_ck = 0u64;
    let mut odd_ck = 0u64;
    read_rows(&ck_dir, 4, |f| {
        n_ck += 1;
        match decode_click(f[0], kc, ck_bound) {
            Some(v) => check_rt(f[0], b'k', kc, v),
            None => odd_ck += 1,
        }
    });
    let mut n_lk = 0u64;
    let mut odd_lk = 0u64;
    let mut cross_person = 0u64;
    read_rows(&lk_dir, 2, |f| {
        n_lk += 1;
        let a = decode_user(f[0], ku, person_bound);
        let b = decode_user(f[1], ku, person_bound);
        match (a, b) {
            (Some(UserId::Person { person: pa, .. }), Some(UserId::Person { person: pb, .. })) => {
                if pa != pb {
                    cross_person += 1;
                }
            }
            (Some(_), Some(_)) => cross_person += 1, // kiosk link
            _ => odd_lk += 1,
        }
    });
    // structure: sort by event index; is each person's run contiguous, and time-ordered?
    rows.sort_unstable_by_key(|r| r.0);
    rows.dedup_by_key(|r| r.0); // duplicate copies share the index
    let mut person_runs = 0u64;
    let mut time_inversions = 0u64;
    let mut persons = std::collections::HashSet::new();
    let mut gaps_in_runs = 0u64;
    for w in rows.windows(2) {
        if w[1].1 != w[0].1 {
            person_runs += 1;
        } else {
            if w[1].2 < w[0].2 {
                time_inversions += 1;
            }
            if w[1].0 != w[0].0 + 1 {
                gaps_in_runs += 1;
            }
        }
    }
    for r in &rows {
        persons.insert(r.1);
    }
    let max_alias = alias_hist.iter().rposition(|&c| c > 0).unwrap_or(0);
    let alias0 = alias_hist[0] as f64 / alias_hist.iter().sum::<u64>().max(1) as f64;
    let alias_ge14: u64 = alias_hist[14..].iter().sum();
    eprintln!("genid: event rows {} | odd event ids {} ({:.4}%) | odd user ids {} ({:.4}%) | kiosk rows {} | round-trip failures {}",
        n_ev, odd_ev, 100.0 * odd_ev as f64 / n_ev.max(1) as f64, odd_user, 100.0 * odd_user as f64 / n_ev.max(1) as f64, kiosk_rows, rt_fail);
    eprintln!("genid: click rows {} odd {} | link rows {} odd {} cross-person/kiosk {}", n_ck, odd_ck, n_lk, odd_lk, cross_person);
    eprintln!("genid: hot-path decoder vs reference decoder mismatches: {}", hot_mismatch);
    eprintln!("genid: structure over {} distinct canonical events: {} persons, {} person runs (runs/persons = {:.3}; 1.000 = each person's events contiguous), {} index gaps inside runs, {} time inversions inside runs",
        rows.len(), persons.len(), person_runs + 1, (person_runs + 1) as f64 / persons.len().max(1) as f64, gaps_in_runs, time_inversions);
    eprintln!("genid: alias slots: max {} | alias 0 share {:.1}% | rows with alias >= 14: {}", max_alias, 100.0 * alias0, alias_ge14);
    eprintln!("genid: check done in {:.2} s", t0.elapsed().as_secs_f64());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inverses_and_roundtrip() {
        assert_eq!(M1.wrapping_mul(M1_INV) & MASK, 1);
        assert_eq!(M2.wrapping_mul(M2_INV) & MASK, 1);
        let mut x: u64 = 0x1234_5678_9abc;
        for _ in 0..200_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let (v, k) = (x & MASK, (x >> 13) & MASK);
            assert_eq!(unscramble(scramble(v, k), k), v);
            let y = scramble(v, k);
            assert_eq!(decode_b36(&encode_b36(b'e', y), b'e'), Some(y));
        }
    }

    #[test]
    fn rejects_non_canonical() {
        for s in [&b"e"[..], b"E1", b"eA", b"e01", b"u1", b"e-1", b"e 1", b"e0000000001", b"ezzzzzzzzz", b"\"e1\"", b"e1\r"] {
            assert_eq!(decode_b36(s, b'e'), None, "{:?}", String::from_utf8_lossy(s));
        }
        assert_eq!(decode_b36(b"e0", b'e'), Some(0));
        // 2^40 - 1 fits, 2^40 does not
        let max = encode_b36(b'e', MASK);
        assert_eq!(decode_b36(&max, b'e'), Some(MASK));
        let over = encode_b36(b'e', MASK + 1);
        assert_eq!(decode_b36(&over, b'e'), None);
    }

    #[test]
    fn hot_path_matches_reference() {
        let alpha = b"0123456789abcdefghijklmnopqrstuvwxyzAZ_-\"eku ";
        let mut x: u64 = 99;
        for _ in 0..300_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let n = (x % 12) as usize;
            let mut s: Vec<u8> = Vec::with_capacity(n);
            for i in 0..n {
                let r = (x >> (i * 5)) as usize ^ i;
                s.push(if i == 0 && x & 1 == 0 { b'u' } else { alpha[r % alpha.len()] });
            }
            let k = (x >> 3) & MASK;
            let want = decode_b36(&s, b'u').map(|y| unscramble(y, k)).unwrap_or(ODD);
            assert_eq!(user_counter(&s, k), want, "{:?}", String::from_utf8_lossy(&s));
        }
        for v in [0u64, 1, 35, 36, 511, 512, 1 << 30, MASK] {
            let k = 0x12_3456_789a;
            let s = encode_b36(b'u', scramble(v, k));
            assert_eq!(user_counter(&s, k), v);
        }
    }

    #[test]
    fn recovers_a_key_from_samples() {
        let key = 0x9_d3f1_2a77 & MASK;
        let mut x: u64 = 77;
        let n = 2_000_000u64;
        let ys: Vec<u64> = (0..3000)
            .map(|_| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                scramble((x >> 20) % n, key)
            })
            .collect();
        let got = recover_key(&ys, n, 1, 4, 6, 0.99, &|v| v < n);
        assert_eq!(got, Some(key));
    }
}
