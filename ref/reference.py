"""Reference implementation of rules.md: slow, literal, easy to check. The Rust pipeline must match it exactly.

Usage: python ref/reference.py DATA_DIR OUTPUT_DIR      (also reads DATA_DIR / OUTPUT_DIR from env)
Writes OUTPUT_DIR/user_daily/part-0.parquet and OUTPUT_DIR/campaign_daily/part-0.parquet.
"""
import datetime
import glob
import json
import os
import sys
from collections import defaultdict

DIG = frozenset(b"0123456789")
DAY_MS = 86_400_000
GAP_MS = 1_800_000
WEEK_MS = 7 * DAY_MS


# ---------------------------------------------------------------- CSV dialect
def parse_line(line: bytes, n: int):
    """Fields as bytes (None for empty), or None if the line is malformed."""
    parts = line.split(b",")
    if len(parts) != n:
        return None
    out = []
    for f in parts:
        if b'"' in f:
            if len(f) >= 2 and f[0] == 34 and f[-1] == 34 and b'"' not in f[1:-1]:
                f = f[1:-1]
            else:
                return None
        out.append(f if f else None)
    return out


def read_rows(paths, n):
    for p in paths:
        data = open(p, "rb").read()
        lines = data.split(b"\n")
        if lines and lines[-1] == b"":
            lines.pop()  # the final \n ends the last line, it does not start an empty one
        for line in lines[1:]:  # first line of every file is its header
            r = parse_line(line, n)
            if r is not None:
                yield r


# ---------------------------------------------------------------- timestamps
def is_leap(y):
    return y % 4 == 0 and (y % 100 != 0 or y % 400 == 0)


MDAYS = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]


def days_from_civil(y, m, d):
    y -= m <= 2
    era = (y if y >= 0 else y - 399) // 400
    yoe = y - era * 400
    doy = (153 * (m + (-3 if m > 2 else 9)) + 2) // 5 + d - 1
    doe = yoe * 365 + yoe // 4 - yoe // 100 + doy
    return era * 146097 + doe - 719468


def num(b: bytes):
    if not b or any(c not in DIG for c in b):
        return None
    return int(b)


def parse_ts(v):
    """Epoch ms, or None for a bad timestamp (window not checked here)."""
    if v is None:
        return None
    if all(c in DIG for c in v):
        return int(v)
    if len(v) == 20:
        if v[19:20] != b"Z":
            return None
        off = 0
    elif len(v) == 25:
        sign = v[19:20]
        if sign not in (b"+", b"-") or v[22:23] != b":":
            return None
        oh, om = num(v[20:22]), num(v[23:25])
        if oh is None or om is None or oh > 23 or om > 59:
            return None
        off = (oh * 60 + om) * 60_000 * (1 if sign == b"+" else -1)
    else:
        return None
    if v[4:5] != b"-" or v[7:8] != b"-" or v[10:11] != b"T" or v[13:14] != b":" or v[16:17] != b":":
        return None
    y, mo, d = num(v[0:4]), num(v[5:7]), num(v[8:10])
    h, mi, s = num(v[11:13]), num(v[14:16]), num(v[17:19])
    if None in (y, mo, d, h, mi, s):
        return None
    if not (1 <= mo <= 12) or h > 23 or mi > 59 or s > 59:
        return None
    md = 29 if mo == 2 and is_leap(y) else MDAYS[mo - 1]
    if not (1 <= d <= md):
        return None
    return (days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + s) * 1000 - off


# ---------------------------------------------------------------- main
def main():
    data = os.environ.get("DATA_DIR") or sys.argv[1]
    out = os.environ.get("OUTPUT_DIR") or sys.argv[2]
    man = json.load(open(os.path.join(data, "manifest.json")))
    ws = parse_ts(man["window_start"].encode())
    we = parse_ts(man["window_end"].encode())

    def files(sub):
        return sorted(glob.glob(os.path.join(data, sub, "*.csv")))

    # links -> person name (byte-wise smallest ID of the component)
    parent = {}

    def find(x):
        r = x
        while parent.get(r, r) != r:
            r = parent[r]
        while parent.get(x, x) != r:
            parent[x], x = r, parent[x]
        return r

    for a, b in read_rows(files("links"), 2):
        if a is None or b is None:
            continue
        ra, rb = find(a), find(b)
        if ra != rb:
            if rb < ra:
                ra, rb = rb, ra
            parent[rb] = ra  # the root is always the smallest ID of its component

    def person(u):
        return find(u)

    products = {}
    for pid, cat in read_rows([os.path.join(data, "products.csv")], 2):
        products[pid] = cat
    campaigns = {}
    for cid, ch in read_rows([os.path.join(data, "campaigns.csv")], 2):
        campaigns[cid] = ch

    # events: parse, dedup (smallest ingest_ts), validate, late
    kept = {}
    for r in read_rows(files("events"), 7):
        eid = r[0]
        if eid is None:
            continue
        ing = parse_ts(r[3])
        cur = kept.get(eid)
        if cur is None or ing < cur[0]:
            kept[eid] = (ing, r)
    per = defaultdict(list)  # person -> [(ts, is_purchase, price, category)]
    for ing, (eid, uid, ts_s, _, et, pid, price) in kept.values():
        if uid is None or pid is None:
            continue
        ts = parse_ts(ts_s)
        if ts is None or not (ws <= ts < we):
            continue
        if et not in (b"view", b"cart", b"purchase"):
            continue
        pc = 0
        if et == b"purchase":
            pc = num(price) if price is not None else None
            if pc is None or not (1 <= pc <= 10_000_000):
                continue
        if ing - ts > DAY_MS:
            continue
        per[person(uid)].append((ts, et == b"purchase", pc, products.get(pid, b"unknown")))

    # clicks: parse, dedup, validate
    clicks = {}
    for cid, uid, cmp_, ts_s in read_rows(files("clicks"), 4):
        if cid is None or cid in clicks:
            continue
        clicks[cid] = (uid, cmp_, ts_s)
    pclicks = defaultdict(list)  # person -> [(ts, click_id, campaign)]
    for cid, (uid, cmp_, ts_s) in clicks.items():
        if uid is None:
            continue
        ts = parse_ts(ts_s)
        if ts is None or not (ws - WEEK_MS <= ts < we) or cmp_ not in campaigns:
            continue
        pclicks[person(uid)].append((ts, cid, cmp_))
    for v in pclicks.values():
        v.sort()

    # sessions, attribution, aggregation
    user = {}       # (person, day) -> [sessions, events, revenue, {cat: cents}]
    camp = {}       # (campaign, day) -> [sessions, purchasing, revenue, assisted]
    for p, evs in per.items():
        evs.sort(key=lambda e: e[0])
        sessions = []
        prev = None
        for e in evs:
            if prev is None or e[0] - prev > GAP_MS:
                sessions.append([])
            sessions[-1].append(e)
            prev = e[0]
        cl = pclicks.get(p, [])
        for s in sessions:
            start = s[0][0]
            day = start // DAY_MS
            rev = sum(e[2] for e in s if e[1])
            purchasing = any(e[1] for e in s)
            u = user.setdefault((p, day), [0, 0, 0, defaultdict(int)])
            u[0] += 1
            u[1] += len(s)
            u[2] += rev
            for e in s:
                if e[1]:
                    u[3][e[3]] += e[2]
            best = None
            assists = set()
            for cts, cid, cmp_ in cl:
                if start - WEEK_MS <= cts <= start:
                    assists.add(cmp_)
                    if best is None or cts > best[0] or (cts == best[0] and cid < best[1]):
                        best = (cts, cid, cmp_)
            credit = best[2] if best else b"organic"
            c = camp.setdefault((credit, day), [0, 0, 0, 0])
            c[0] += 1
            c[1] += purchasing
            c[2] += rev
            for a in assists:
                camp.setdefault((a, day), [0, 0, 0, 0])[3] += 1

    import pyarrow as pa
    import pyarrow.parquet as pq

    epoch = datetime.date(1970, 1, 1)
    ud = {"user_id": [], "day": [], "sessions": [], "events": [], "revenue_cents": [], "top_category": []}
    for (p, day), (s, n, rev, cats) in user.items():
        top = None
        if cats:
            top = min(cats.items(), key=lambda kv: (-kv[1], kv[0]))[0].decode()
        ud["user_id"].append(p.decode()); ud["day"].append(epoch + datetime.timedelta(days=day))
        ud["sessions"].append(s); ud["events"].append(n); ud["revenue_cents"].append(rev); ud["top_category"].append(top)
    cd = {"campaign_id": [], "channel": [], "day": [], "sessions": [], "purchasing_sessions": [],
          "revenue_cents": [], "assisted_sessions": []}
    for (cmp_, day), (s, pur, rev, ast) in camp.items():
        cd["campaign_id"].append(cmp_.decode())
        cd["channel"].append("organic" if cmp_ == b"organic" else campaigns[cmp_].decode())
        cd["day"].append(epoch + datetime.timedelta(days=day))
        cd["sessions"].append(s); cd["purchasing_sessions"].append(pur); cd["revenue_cents"].append(rev); cd["assisted_sessions"].append(ast)
    for name, cols, types in (
        ("user_daily", ud, [pa.string(), pa.date32(), pa.int64(), pa.int64(), pa.int64(), pa.string()]),
        ("campaign_daily", cd, [pa.string(), pa.string(), pa.date32(), pa.int64(), pa.int64(), pa.int64(), pa.int64()]),
    ):
        os.makedirs(os.path.join(out, name), exist_ok=True)
        t = pa.table({k: pa.array(v, type=ty) for (k, v), ty in zip(cols.items(), types)})
        pq.write_table(t, os.path.join(out, name, "part-0.parquet"))


if __name__ == "__main__":
    main()
