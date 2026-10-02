"""Render the reward ledger HTML from the validated 660,000 snapshot data."""
import csv, json

SP = "/tmp/claude-1000/-home-blockheads-crosslink-monolith/5a0e07e2-9ed3-42da-b1f4-6dedf63160e4/scratchpad"
R = "/home/blockheads/crosslink-indexer/reports/pow-660000"
rev = lambda h: bytes.fromhex(h)[::-1].hex()

fin = list(csv.DictReader(open(f"{R}/finalizers.csv")))
mean_power = {rev(r[0]): int(r[3]) for r in csv.reader(open(f"{SP}/part_raw.csv"))}
F = []
for r in fin:
    F.append([
        r["finalizer"], r["terminated_at_pow"] or "", r["in_roster_at_cutoff"] == "True", r["mirror_of"],
        int(r["bft_eligible"]), int(r["bft_signed"]), float(r["bft_sign_rate_pct"]),
        mean_power.get(r["finalizer"], 0) / 1e8,
        float(r["active_value_ctaz"]), float(r["active_stake_share_pct"]), float(r["accrued_ctaz"]),
        float(r["burned_value_ctaz"]), int(r["bonds"]), float(r["principal_ctaz"]),
    ])
# live side of a mirror pair = the one that has signed
live = {r[0] for r in F if r[5] > 0}

M = [[r["miner_address"], int(r["blocks"]), float(r["pct_of_blocks"]), int(r["first_height"]),
      int(r["last_height"]), int(r["total_zats"]) / 1e8, int(r["fees_zats"]) / 1e8]
     for r in csv.DictReader(open(f"{R}/miners.csv"))]

SC = {"Active": 0, "Unbonding": 1, "Withdrawn": 2, "Burned": 3}
B = [[r["bond_key"], r["target_finalizer"], int(r["created_height"]), int(r["principal_zats"]),
      int(r["amount_zats"]), int(r["accrued_zats"]), SC[r["status"]], int(r["status_height"]), int(r["retargets"])]
     for r in csv.DictReader(open(f"{R}/bonds.csv"))]

data = json.dumps({"F": F, "M": M, "B": B, "live": sorted(live)}, separators=(",", ":"))

html = open(f"{SP}/report_template.html").read().replace("/*__DATA__*/null", data)
open(f"{SP}/season-one-reward-ledger.html", "w").write(html)
print(f"wrote {len(html)/1e6:.2f} MB  ({len(F)} finalizers, {len(M)} miners, {len(B)} bonds)")
