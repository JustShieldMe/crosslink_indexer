# Concepts

What the data actually means. Read this before [queries.md](queries.md) —
several columns are easy to misinterpret without it.

## Two chains

Crosslink runs a **proof-of-work chain** and a **BFT (proof-of-stake)
finalization chain** side by side.

* The **PoW chain** produces blocks the usual Zcash way. Heights here run to
  ~515,000 on the devnet this was built against.
* The **BFT chain** is a separate chain of *decided blocks*, each finalizing a
  PoW block. Its heights are independent — ~99,000 when PoW was at ~515,000.

The ratio between them is **not fixed**, and averaging over all history hides
that. Measured per 20,000 BFT heights, PoW blocks finalized per BFT block:

| BFT heights | PoW blocks per BFT block |
|---|---|
| 0–20k | 12.65 |
| 20k–40k | 3.80 |
| 40k–60k | 2.95 |
| 60k–80k | 3.55 |
| 80k+ | 2.92 |

So the network settled from ~12 early on to ~3 once finalizers were
participating steadily. Finality currently runs about **2 PoW blocks behind the
tip**. Derive both from the data rather than assuming a constant:

```sql
WITH j AS (SELECT b.bft_height, p.height AS pow_height
           FROM bft_block b JOIN pow_block p ON p.hash = b.candidate_hash)
SELECT bft_height, pow_height - LAG(pow_height) OVER (ORDER BY bft_height) AS gap
FROM j;
```

A BFT block carries a small run of PoW block headers. The **first** header is
the *finalization candidate* — the PoW block that block finalizes. On this
network each BFT block carries exactly 3 headers (the network's
`bc_confirmation_depth_sigma`).

`bft_block.candidate_hash` is the hash of that first header, which is how the
two chains join:

```sql
SELECT b.bft_height, p.height AS pow_height
FROM bft_block b JOIN pow_block p ON p.hash = b.candidate_hash;
```

## Finalizers, rosters, and certificates

A **finalizer** is a participant in BFT consensus, identified by a 32-byte
ed25519 public key.

The **roster** is the set of finalizers eligible to vote at a given BFT height,
each with a **voting power** in zatoshis — the total stake delegated to them.
The roster changes over time as bonds are created and retargeted; on this
network it grew from 1 to 48 members.

A **certificate** (`FatPointerToBftBlock` in the source) is the proof that a
block was decided: a vote — block hash, height, round — plus the signatures of
the finalizers who voted for it.

### Participation: the central idea

`roster_entry` says who *could* have signed at a height. `bft_signature` says
who *did*. The gap between them is the finalizer-behaviour signal:

```sql
SELECT r.pub_key, COUNT(*) AS eligible, SUM(s.pub_key IS NOT NULL) AS signed
FROM roster_entry r
LEFT JOIN bft_signature s USING (bft_height, pub_key)
GROUP BY r.pub_key;
```

> **BFT needs two thirds by power, not by count.** A certificate with 15
> signatures against a 45-member roster is entirely normal, because those 15
> may hold most of the stake. Always weight by `voting_power`. `bft_block`
> stores `signer_power` and `roster_power` precomputed for exactly this reason;
> `signer_power / roster_power` is the number that matters, and it should stay
> above 0.667.

### Rounds

`bft_block.cert_round` is the consensus round that produced the decision. Round
0 means decided first try; higher rounds mean earlier rounds failed to reach a
decision. On this network most blocks take 2–3 rounds, and round 0 is the
*minority* — so a nonzero round is normal, not an incident. Watch the
distribution and its drift rather than individual values.

## Staking

### The staking period and window

Staking actions are only valid in a recurring window:

```
STAKING_PERIOD     = 150 blocks     "a staking day"
STAKING_DAY_WINDOW = 70 blocks      actions allowed when height % 150 < 70
```

`staking_action.period_offset` is `height % 150`, stored so a violation shows up
as data rather than being normalised away.

Two carve-outs in the consensus rule (`check_staking_day_window` in
`zebra-consensus`):

* `RetargetDelegationBond` is **exempt** and may be submitted at any time.
* Heights **1120, 2320, 2620, 2621, 3224** are hardcoded exceptions.

A related rule, `STAKING_ACTION_DELAY` (= `STAKING_DAY_WINDOW + 5` = 75 blocks),
requires that many blocks to pass between actions on the same bond.

### Bonds and their lifecycle

A **bond** is a delegation of stake to a finalizer, identified by
`bond_key` — a unique public key generated per bond (`arg32_0` in the raw
action). The normal lifecycle:

```
CreateNewDelegationBond   -> bond exists, amount_zats staked to target_finalizer
  RetargetDelegationBond  -> optional, any number of times, moves it to a new finalizer
BeginDelegationUnbonding  -> starts unbonding
WithdrawDelegationBond    -> funds withdrawn, amount_zats returned
```

Trace one bond with:

```sql
SELECT height, kind_name, amount_zats FROM staking_action
WHERE bond_key = ? ORDER BY height;
```

### Action kinds

| Code | Name | `amount_zats` | `target_finalizer` |
|---|---|---|---|
| 0 | `Null` | — | — | *(never indexed)* |
| 1 | `CreateNewDelegationBond` | amount staked | yes |
| 2 | `BeginDelegationUnbonding` | 0 | no |
| 3 | `WithdrawDelegationBond` | amount withdrawn | no |
| 4 | `RetargetDelegationBond` | 0 | yes (the new target) |
| 5 | `RegisterFinalizer` | 0 | no |
| 6 | `ConvertFinalizerRewardToDelegationBond` | amount | yes |
| 7 | `UpdateFinalizerKey` | 0 | yes |

Kinds 5–7 exist in the protocol but had not appeared on this devnet at the time
of writing. `amount_zats` is stored exactly as it appears in the action; the
table above says how to read it, and a `0` for a retarget is meaningful (nothing
moved), not missing data.

Amounts are in **zatoshis**; divide by `1e8` for ctaz.

## Mining

`pow_block` is ordinary chain data. `miner_address` and `subsidy_zats` come from
the **first output of the coinbase transaction**, which is the miner's payout;
later coinbase outputs are funding streams and are not attributed to the miner.

`difficulty` and `bits` are as the node reports them. `time` is the block header
timestamp (seconds, UTC) and is miner-supplied, so it is not perfectly monotonic
— use height for ordering and treat `time` as approximate.

## What is *not* in the data

* **Staking rewards / yield.** `getbondinfo` returns only
  `{amount, status, last_action_height}`; no RPC and no on-chain field separates
  yield from principal. This indexer records principal movements only. Any
  "rewards earned" number would be invented.
* **Slashing.** The node has a slashed-bond index in its own state, but nothing
  is exposed to reconstruct slashing history here.
* **Per-signature attribution of proposal signatures.** See
  [schema.md](schema.md#limits).
