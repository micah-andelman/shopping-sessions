"""canon_hash.py OUT: rules.md self-checks plus SHA-256 of each table as a fully sorted CSV.
Lets us compare outputs at scales where no second implementation fits in memory."""
import hashlib, io, sys
from pathlib import Path
import polars as pl
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "scripts"))
from run_fixtures import SCHEMAS, load_actual

out = Path(sys.argv[1])
t = {n: load_actual(out / n, s) for n, s in SCHEMAS.items()}
u, c = t["user_daily"], t["campaign_daily"]
ok = all(u[k].sum() == c[k].sum() for k in ("sessions", "revenue_cents"))
ok &= c["assisted_sessions"].sum() >= c.filter(pl.col("campaign_id") != "organic")["sessions"].sum()
for name, df in t.items():
    keys = [k for k in ("user_id", "campaign_id", "day") if k in df.columns]
    ok &= df.height == df.n_unique(subset=keys)
    buf = io.BytesIO(); df.sort(list(df.columns)).write_csv(buf)
    print(f"{name} rows={df.height} sha256={hashlib.sha256(buf.getvalue()).hexdigest()}")
print("self-checks", "OK" if ok else "FAIL")
