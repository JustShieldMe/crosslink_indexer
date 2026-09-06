# Findings

Things the index surfaced that were not visible from RPC alone. Each is stated
with how it was verified and how confident that verification is, because the
indexer can establish *what is in the chain* but not *what the rules were when
each block was accepted*.

Findings are from a devnet node at PoW ~515,600 / BFT ~99,400.

---

## A finalizer that has never signed

`034ad1af698987ba532b7a9acbaecebae8af0d5057cccda0e48d2e0acd3487fe` (RPC display
order) has been in the roster since **BFT height 13,737**, holds roughly
**98,166 ctaz** of voting power, and has signed **0 of 85,622** certificates.

```
$ crosslink-indexer finalizer 034ad1af698987ba532b7a9acbaecebae8af0d5057cccda0e48d2e0acd3487fe
  in roster heights : 13737 .. 99358
  eligible / signed : 85622 / 0  (0.0%)
  mean voting power : 98165.9236 ctaz
  last   100 heights : 0/100  (0.0%)
```

**Verification.** Independently corroborated by the live node:
`get_tfl_recency_status` reports zero votes for this key. This is not a decoding
artefact.

**Why it matters.** Stake delegated here is inert: it counts toward
`roster_power` (the denominator for the two-thirds threshold) while never
contributing to `signer_power`. Enough such stake pushes the network toward the
liveness threshold without any finalizer visibly misbehaving.

Others sit in between — one finalizer holds ~229,549 ctaz at a 16.4%
signing rate over recent heights. `participation --from <recent>` lists them.

---

## 28 staking actions outside the window that consensus does not exempt

`stats` reports 28 staking actions where `height % 150 >= 70`, after applying
the consensus predicate exactly — `RetargetDelegationBond` exempt, and heights
1120, 2320, 2620, 2621, 3224 hardcoded exceptions.

They run from height **352,270 to 513,220**, all at offsets 70–77, just past the
window boundary.

**Verification.** Confirmed directly against the node, not just internally:

```
$ getrawtransaction f2cff876f7fe54223213c2db06b0a4416e065cc21ada67099e294a14b7857ebd 1
  height 352270, version 7 (VCrosslink), height % 150 = 70
```

These are main-chain transactions. `check_staking_day_window` is live in the
verification path (`zebra-consensus/src/transaction.rs:411`), and the rule as
written would reject them.

**What is not established.** Why they were accepted. The most likely reading is
that they predate the rule reaching its current form and the exception list was
never extended past the early heights — the same pattern its own
`TODO: @Prod @Season2 remove this temporary cruft` comment describes. But the
indexer cannot see which rules were in force at each height, so this is a
hypothesis, not a conclusion.

**Why it may matter.** If the current rule is what a fresh verifying sync would
apply, a resync from genesis would fail at height 352,270. Worth confirming
before anyone attempts one.

Reported as a **NOTE** rather than a WARNING for exactly this reason.

---

## Corroboration that the decoder is correct

The consensus exception list — heights **1120, 2320, 2620, 2621, 3224** — was
rediscovered from raw transaction bytes by an earlier version of the check that
knew nothing about it. Before the carve-outs were applied, the naive rule
reported 184 violations:

| Category | Count |
|---|---|
| `RetargetDelegationBond` (exempt by rule) | 151 |
| The five hardcoded exception heights | 5 |
| Genuine, unexplained | 28 |

That the decoder independently lands on precisely the five heights the node
hardcodes is strong evidence it is reading the `VCrosslink` format correctly.

A second, independent check: decoded staking amounts match the operator's own
staking script logs to four decimal places — `4.5003 ctaz` at 21:56 and
`5.4998 ctaz` at 22:22 appear at heights 515,277 and 515,415.

---

## Consensus rarely decides on the first round

`cert_round` distribution over ~99,000 BFT blocks:

| Round | Blocks |
|---|---|
| 0 | 4,586 |
| 1 | 23,391 |
| 2 | 28,220 |
| 3 | 20,282 |
| 4 | 12,251 |
| 5 | 6,179 |
| 6+ | ~4,000 |

Round 0 — decided first try — is the **minority**, under 5%. The mode is round
2. This is stated as an observation, not a defect: it is the network's normal
operating profile, and the useful signal is *drift* in this distribution rather
than any single value.

---

## Finality stalls are rare, isolated, and correlate with small quorums

99.91% of BFT heights decide in under 10 rounds. 86 (0.09%) needed 10 or more;
13 of those (0.013%) needed 30 or more, up to **281** at height 20,626. The 5
most recent, all within the last three weeks of indexed history:

| BFT height | Date (UTC) | Rounds | Final quorum | Margin |
|---|---|---|---|---|
| 90,732 | 2026-09-02 07:44 | 46 | 12/39 | 69.4% |
| 90,652 | 2026-09-01 23:31 | 70 | 11/39 | 67.1% |
| 88,248 | 2026-09-01 02:16 | 19 | 16/39 | 78.8% |
| 53,835 | 2026-08-19 13:48 | 68 | 9/37 | 66.8% |
| 50,886 | 2026-08-17 03:20 | 155 | 9/36 | 67.8% |

Height 88,248 was the tail of a tighter cluster — seven heights between
88,239 and 88,248, all within one hour on 2026-09-01, each needing 10–19
rounds at a comfortable 73.7–78.8% margin. The two severe stalls 21 hours
later (90,652, 90,732) look different in character: fewer, larger-power
signers, and margins sitting close to the 66.7% threshold (67.1%, 69.4%).

**Verification.** For all 5 heights, checked directly against `roster_entry`
rather than assumed:

* **Roster composition is byte-for-byte identical** the height before, during,
  and after every one — no finalizer joined or left, and `roster_power` moved
  only by routine stake accrual. Membership churn is ruled out.
* **Every stall resolves by the very next height** — round 0–2 in 4 of 5 cases.
  Nothing about these is a sustained network split; each is a self-contained,
  single-height event.
* Across the full 86-height tail, heights needing 30+ rounds were decided by
  fewer final signers on average than heights needing 10–29 (mean 8.5 vs
  10.8 signers; correlation −0.30 between round count and signer count). Not a
  strong correlation, but directionally consistent with the idea that a
  smaller, more concentrated winning coalition takes longer to assemble.
* The finalizer with a lifetime 0% signing rate (see above) was in the roster
  at all 5, holding 2.9–3.5% of total power each time — a permanent, small
  drag on the achievable margin, but nowhere near enough on its own to explain
  a climb to 155 or 281 rounds.

**What is not established.** `bft_signature` records only the *winning*
round's signers — `pos.chain` does not retain who was asked and did not answer
in the rounds that failed before it (see [limits](schema.md#limits)). So this
can rule causes out and surface correlates, but cannot name which specific
finalizer(s) were slow to respond in a given stall, or distinguish network
latency, load, and a leader-rotation quirk from each other. That would need
instrumentation on the node itself.

Reproduce this with the queries in [queries.md](queries.md#finality-stalls).

---

## Signature count is a poor proxy for finality health

Certificates typically carry 14–18 signatures against rosters of 37–48 members,
which looks alarming until weighted by power. Mean `signer_power / roster_power`
is **79.8%**, comfortably above the 66.7% threshold.

Any dashboard built on `signer_count / roster_size` will show a false crisis.
Use `signer_power / roster_power`; `bft_block` stores both precomputed.
