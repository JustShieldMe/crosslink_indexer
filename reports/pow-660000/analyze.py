"""Join staking replay, BFT participation and mining into per-finalizer / per-miner tables."""
import csv, json, re, sqlite3
from collections import defaultdict

SP = "/tmp/claude-1000/-home-blockheads-crosslink-monolith/5a0e07e2-9ed3-42da-b1f4-6dedf63160e4/scratchpad"
IDX = "/home/blockheads/crosslink-indexer"
Z = 1e8
rev = lambda h: bytes.fromhex(h)[::-1].hex()

# ---- hardfork schedule (display-order keys), shipped rule + node config ----
rules = [{"pow": 225000, "bft": 2158, "keys": ["b8c5272fe6d34980ba6e8aa88d65a6cbf3daabaabea80bdd51a5a5d3bce30c9e"]}]
cur = None
for line in open("/home/blockheads/.config/zebrad.toml"):
    l = line.strip()
    if l.startswith("["):
        if cur: rules.append(cur)
        cur = {"pow": 0, "bft": 0, "keys": []} if l == "[[crosslink.hardforks]]" else None
        continue
    if cur is None: continue
    if l.startswith("pow_activation_height"): cur["pow"] = int(l.split("=")[1])
    elif l.startswith("bft_certificate_height"): cur["bft"] = int(l.split("=")[1])
    elif re.match(r'^"[0-9a-f]{64}"', l): cur["keys"].append(l.strip('",'))
if cur: rules.append(cur)
terminated = {k: r["pow"] for r in rules for k in r["keys"]}

# ---- staking (display-order targets from the replay) ----
bonds = list(csv.DictReader(open(f"{SP}/bonds_660000.csv")))
st = defaultdict(lambda: defaultdict(int))
for b in bonds:
    s = st[b["target_finalizer"]]
    s["bonds"] += 1
    s[f"n_{b['status']}"] += 1
    s["principal"] += int(b["principal_zats"])
    s["value"] += int(b["amount_zats"])
    s["accrued"] += int(b["accrued_zats"])
    s[f"value_{b['status']}"] += int(b["amount_zats"])
    s[f"accrued_{b['status']}"] += int(b["accrued_zats"])
    if b["target_finalizer"] != b["original_target"]: s["retargeted_in"] += 1

# ---- BFT participation over BFT 0..126209 ----
part = {}
for row in csv.reader(open(f"{SP}/part_raw.csv")):
    raw, elig, signed, mp, lo, hi = row
    part[rev(raw)] = dict(eligible=int(elig), signed=int(signed), mean_power=int(mp), first=int(lo), last=int(hi))
db = sqlite3.connect(f"{IDX}/bft_now.sqlite")
in_final = {rev(r[0].hex()) for r in db.execute("SELECT pub_key FROM roster_entry WHERE bft_height=126209")}

keys = set(st) | set(part)
active_total = sum(st[k]["value_Active"] for k in keys)

rows = []
for k in keys:
    s, p = st[k], part.get(k, {})
    mirror = rev(k) if rev(k) in keys and rev(k) != k else ""
    rows.append({
        "finalizer": k,
        "terminated_at_pow": terminated.get(k, ""),
        "in_roster_at_cutoff": k in in_final,
        "mirror_of": mirror,
        "bft_eligible": p.get("eligible", 0),
        "bft_signed": p.get("signed", 0),
        "bft_sign_rate_pct": round(100 * p["signed"] / p["eligible"], 2) if p.get("eligible") else 0.0,
        "roster_first_bft": p.get("first", ""),
        "roster_last_bft": p.get("last", ""),
        "bonds": s["bonds"],
        "bonds_active": s["n_Active"], "bonds_burned": s["n_Burned"],
        "bonds_unbonding": s["n_Unbonding"], "bonds_withdrawn": s["n_Withdrawn"],
        "principal_ctaz": round(s["principal"] / Z, 8),
        "value_ctaz": round(s["value"] / Z, 8),
        "accrued_ctaz": round(s["accrued"] / Z, 8),
        "active_value_ctaz": round(s["value_Active"] / Z, 8),
        "active_stake_share_pct": round(100 * s["value_Active"] / active_total, 4) if active_total else 0,
        "burned_value_ctaz": round(s["value_Burned"] / Z, 8),
        "burned_accrued_ctaz": round(s["accrued_Burned"] / Z, 8),
    })
rows.sort(key=lambda r: -r["value_ctaz"])
with open(f"{SP}/finalizers_660000.csv", "w", newline="") as f:
    w = csv.DictWriter(f, fieldnames=list(rows[0].keys())); w.writeheader(); w.writerows(rows)

# ---- mirror pairs ----
seen, pairs = set(), []
for r in rows:
    m = r["mirror_of"]
    if m and r["finalizer"] not in seen:
        seen |= {r["finalizer"], m}
        a, b = r, next(x for x in rows if x["finalizer"] == m)
        live, phantom = (a, b) if a["bft_signed"] >= b["bft_signed"] else (b, a)
        pairs.append((live, phantom))

summary = {
    "rules": [{"pow": r["pow"], "bft": r["bft"], "n": len(r["keys"])} for r in rules],
    "finalizers_total": len(rows),
    "ever_signed": sum(1 for r in rows if r["bft_signed"] > 0),
    "in_roster_at_cutoff": sum(1 for r in rows if r["in_roster_at_cutoff"]),
    "active_total_ctaz": active_total / Z,
    "never_signed_but_staked": [(r["finalizer"], r["value_ctaz"]) for r in rows if r["bft_signed"] == 0 and r["value_ctaz"] > 0],
    "mirror_pairs": [(l["finalizer"], l["bft_signed"], l["value_ctaz"], p["finalizer"], p["bft_signed"], p["value_ctaz"], p["terminated_at_pow"], p["burned_value_ctaz"]) for l, p in pairs],
}
json.dump(summary, open(f"{SP}/summary.json", "w"), indent=1)

print(f"finalizers: {len(rows)}  ever signed: {summary['ever_signed']}  in roster at cutoff: {summary['in_roster_at_cutoff']}")
print(f"active stake at 660,000: {active_total/Z:,.2f} ctaz\n")
print("MIRROR PAIRS (key and its byte-reversal both hold stake):")
for l, p in pairs:
    print(f"  live    {l['finalizer'][:16]}  signed {l['bft_signed']:>7}  value {l['value_ctaz']:>13,.2f}")
    print(f"  phantom {p['finalizer'][:16]}  signed {p['bft_signed']:>7}  value {p['value_ctaz']:>13,.2f}  terminated@{p['terminated_at_pow'] or '-'}  burned {p['burned_value_ctaz']:,.2f}")
nsig = summary["never_signed_but_staked"]
print(f"\nfinalizers holding stake that NEVER signed a BFT block: {len(nsig)}, "
      f"stake {sum(v for _, v in nsig):,.2f} ctaz")
