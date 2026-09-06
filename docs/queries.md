# Query cookbook

Analytics queries for the three domains. All run against `crosslink.sqlite`:

```sh
sqlite3 crosslink.sqlite
```

> Pubkeys, txids and block hashes come back as **raw** blobs. `hex()` in SQLite
> does *not* reverse them, so most values need reversing before they match node
> output. See [byte-order.md](byte-order.md). Joins between tables need no
> conversion.

---

## Finalizer behaviour

### Participation leaderboard

```sql
SELECT hex(r.pub_key)                              AS pub_key_raw,
       COUNT(*)                                    AS eligible,
       SUM(s.pub_key IS NOT NULL)                  AS signed,
       ROUND(100.0 * SUM(s.pub_key IS NOT NULL) / COUNT(*), 1) AS pct,
       ROUND(AVG(r.voting_power) / 1e8, 2)         AS mean_ctaz
FROM roster_entry r
LEFT JOIN bft_signature s USING (bft_height, pub_key)
GROUP BY r.pub_key
ORDER BY mean_ctaz DESC;
```

### Finalizers that have gone quiet

Still in the roster, no longer signing — the highest-value alert in the dataset.

```sql
SELECT hex(r.pub_key) AS pub_key_raw,
       COUNT(*) AS eligible,
       SUM(s.pub_key IS NOT NULL) AS signed,
       ROUND(AVG(r.voting_power) / 1e8, 2) AS mean_ctaz
FROM roster_entry r
LEFT JOIN bft_signature s USING (bft_height, pub_key)
WHERE r.bft_height > (SELECT MAX(bft_height) - 1000 FROM bft_block)
GROUP BY r.pub_key
HAVING signed * 1.0 / eligible < 0.5
ORDER BY mean_ctaz DESC;
```

### Participation trend for one finalizer

Buckets of 1,000 heights, to see when behaviour changed.

```sql
SELECT (r.bft_height / 1000) * 1000 AS bucket,
       COUNT(*) AS eligible,
       SUM(s.pub_key IS NOT NULL) AS signed,
       ROUND(100.0 * SUM(s.pub_key IS NOT NULL) / COUNT(*), 1) AS pct
FROM roster_entry r
LEFT JOIN bft_signature s USING (bft_height, pub_key)
WHERE r.pub_key = :raw_key
GROUP BY bucket ORDER BY bucket;
```

### How close finality runs to the two-thirds threshold

```sql
SELECT bft_height,
       signer_count, roster_size,
       ROUND(100.0 * signer_power / roster_power, 2) AS power_pct
FROM bft_block
WHERE roster_power > 0
ORDER BY 1.0 * signer_power / roster_power
LIMIT 50;
```

Margin over time, to spot drift toward the line:

```sql
SELECT (bft_height / 5000) * 5000 AS bucket,
       ROUND(AVG(100.0 * signer_power / roster_power), 2) AS mean_power_pct,
       ROUND(MIN(100.0 * signer_power / roster_power), 2) AS worst_pct
FROM bft_block WHERE roster_power > 0
GROUP BY bucket ORDER BY bucket;
```

### Consensus rounds

Round 0 means decided first try. A rising distribution means consensus is
working harder.

```sql
SELECT cert_round, COUNT(*) AS blocks
FROM bft_block GROUP BY cert_round ORDER BY cert_round;

SELECT (bft_height / 5000) * 5000 AS bucket,
       ROUND(AVG(cert_round), 2) AS mean_round,
       MAX(cert_round) AS worst
FROM bft_block GROUP BY bucket ORDER BY bucket;
```

### Finality stalls

Heights that needed an unusual number of rounds, joined to the PoW block they
finalized for a calendar date. `cert_round >= 10` is a deliberate cutoff, not a
round number: 99.91% of this chain's history decides in under 10 rounds, so
crossing it is already rare (86 heights out of 99,467 at last count).

```sql
SELECT b.bft_height, b.cert_round, b.roster_size,
       ROUND(b.roster_power/1e8,2) AS roster_ctaz, b.signer_count,
       ROUND(100.0*b.signer_power/b.roster_power,2) AS pct,
       datetime(p.time,'unixepoch') AS approx_time
FROM bft_block b
LEFT JOIN pow_block p ON p.hash = b.candidate_hash
WHERE b.cert_round >= 10
ORDER BY b.bft_height DESC;
```

Whether the roster itself moved right around a flagged height — a real
membership change would explain a struggle to re-establish quorum; an
unchanged roster points elsewhere:

```sql
SELECT 'joined' AS chg, hex(pub_key) FROM roster_entry
WHERE bft_height = :h
  AND pub_key NOT IN (SELECT pub_key FROM roster_entry WHERE bft_height = :h - 1)
UNION ALL
SELECT 'left', hex(pub_key) FROM roster_entry
WHERE bft_height = :h - 1
  AND pub_key NOT IN (SELECT pub_key FROM roster_entry WHERE bft_height = :h);
```

> `bft_block` carries no timestamp of its own — only `cert_height`/`cert_round`
> and the roster/signer tallies. The `approx_time` above is the *finalized PoW
> block's* time, which is a fine proxy but not the moment the BFT decision
> itself landed. See [findings.md](findings.md#finality-stalls-are-rare-isolated-and-correlate-with-small-quorums).

### Roster churn

```sql
SELECT bft_height, roster_size, ROUND(roster_power / 1e8, 2) AS total_ctaz
FROM bft_block
WHERE bft_height % 1000 = 0
ORDER BY bft_height;
```

Joiners and leavers:

```sql
SELECT hex(pub_key) AS pub_key_raw,
       MIN(bft_height) AS joined, MAX(bft_height) AS last_seen
FROM roster_entry GROUP BY pub_key ORDER BY joined;
```

### Stake concentration

How much power the top finalizer holds at a height — a centralisation measure.

```sql
SELECT bft_height,
       ROUND(100.0 * MAX(voting_power) /
             (SELECT roster_power FROM bft_block b
              WHERE b.bft_height = r.bft_height), 2) AS top_holder_pct
FROM roster_entry r
WHERE bft_height % 5000 = 0
GROUP BY bft_height ORDER BY bft_height;
```

---

## Mining

### Miner distribution

```sql
SELECT miner_address,
       COUNT(*) AS blocks,
       ROUND(100.0 * COUNT(*) / (SELECT COUNT(*) FROM pow_block), 2) AS pct,
       ROUND(SUM(subsidy_zats) / 1e8, 2) AS ctaz
FROM pow_block WHERE miner_address IS NOT NULL
GROUP BY miner_address ORDER BY blocks DESC;
```

### Miner share over time

```sql
SELECT (height / 50000) * 50000 AS bucket, miner_address, COUNT(*) AS blocks
FROM pow_block WHERE miner_address IS NOT NULL
GROUP BY bucket, miner_address
HAVING blocks > 100
ORDER BY bucket, blocks DESC;
```

### Top miners' cumulative rewards over time

Ranks miners by lifetime reward, then walks daily cumulative earnings for just
that top 10 — the shape a "who's winning" chart wants.

```sql
WITH top_miners AS (
  SELECT miner_address, SUM(subsidy_zats) AS total_zats
  FROM pow_block
  WHERE miner_address IS NOT NULL AND height > 0
  GROUP BY miner_address
  ORDER BY total_zats DESC
  LIMIT 10
),
daily AS (
  SELECT date(time, 'unixepoch') AS day, miner_address, SUM(subsidy_zats) AS day_zats
  FROM pow_block
  WHERE height > 0 AND miner_address IN (SELECT miner_address FROM top_miners)
  GROUP BY day, miner_address
)
SELECT day, miner_address,
       ROUND(SUM(day_zats) OVER (PARTITION BY miner_address ORDER BY day) / 1e8, 4)
         AS cumulative_ctaz
FROM daily
ORDER BY miner_address, day;
```

> **`height > 0` is not a style choice — it excludes a bad timestamp.** Block 0
> carries a placeholder genesis time inherited from upstream Zcash params
> (`2016-10-28`), a decade before this devnet existed. Block 1's timestamp is
> the real inception (`2026-04-16` on this index). Include height 0 in a
> time-bucketed query and it opens the chart with a decade-wide empty gap.
> `time` is otherwise miner-supplied and not strictly monotonic — see
> [schema.md](schema.md#pow_block).
>
> Days with no block from a given miner are simply absent from `daily`, so a
> plotted line will skip from one dated point to the next rather than holding
> flat. To force a continuous line, left-join against a generated calendar
> (`WITH RECURSIVE`) and `COALESCE` missing days to 0 before the running sum.

### Block interval

`time` is miner-supplied, so clamp negatives before trusting the mean.

```sql
WITH d AS (
  SELECT height, time - LAG(time) OVER (ORDER BY height) AS gap
  FROM pow_block
)
SELECT (height / 50000) * 50000 AS bucket,
       COUNT(*) AS blocks,
       ROUND(AVG(gap), 1) AS mean_gap_s,
       MAX(gap) AS max_gap_s,
       SUM(gap < 0) AS negative_gaps
FROM d WHERE gap IS NOT NULL
GROUP BY bucket ORDER BY bucket;
```

### Difficulty and block size

```sql
SELECT (height / 10000) * 10000 AS bucket,
       ROUND(AVG(difficulty), 2) AS mean_difficulty,
       ROUND(AVG(size), 0)       AS mean_size_bytes,
       ROUND(AVG(tx_count), 2)   AS mean_txs
FROM pow_block GROUP BY bucket ORDER BY bucket;
```

---

## Staking

### Bonding activity over time

```sql
SELECT (height / 10000) * 10000 AS bucket,
       kind_name,
       COUNT(*) AS n,
       ROUND(SUM(amount_zats) / 1e8, 2) AS ctaz
FROM staking_action
GROUP BY bucket, kind_name ORDER BY bucket, n DESC;
```

### Cumulative stake bonded

```sql
SELECT height,
       ROUND(SUM(CASE WHEN kind_name = 'CreateNewDelegationBond'
                      THEN amount_zats
                      WHEN kind_name = 'WithdrawDelegationBond'
                      THEN -amount_zats ELSE 0 END)
             OVER (ORDER BY height) / 1e8, 2) AS cumulative_ctaz
FROM staking_action ORDER BY height;
```

> **The `WithdrawDelegationBond` amount does not reconcile with what that bond
> staked.** One bond created for 0.01 ctaz withdrew 599.5655 ctaz — a 59,957×
> return — while another created for 10 ctaz withdrew 10.0052, almost exactly
> its principal. There is no consistent rate across the 6 withdrawals this
> chain has ever seen (ratios from 1.0× to 59,957×). At *daily* resolution the
> effect is invisible — 824 ctaz of withdrawals against 2.16M ever bonded — but
> at block resolution this query's running total goes **negative** for heights
> 337–626, which no real "stake bonded" figure can be. Don't read a "yield"
> out of `amount_zats` on either side of a bond; see
> [findings.md](findings.md#withdrawal-amounts-dont-reconcile-with-principal).

### Lifecycle of one bond

```sql
SELECT height, kind_name,
       ROUND(amount_zats / 1e8, 4) AS ctaz,
       hex(target_finalizer) AS target_raw
FROM staking_action WHERE bond_key = :raw_bond_key ORDER BY height;
```

### Bonds that never completed the cycle

Created and began unbonding, but never withdrew.

```sql
SELECT hex(bond_key) AS bond_raw,
       MIN(height) AS created,
       MAX(height) AS last_action
FROM staking_action GROUP BY bond_key
HAVING SUM(kind_name = 'BeginDelegationUnbonding') > 0
   AND SUM(kind_name = 'WithdrawDelegationBond')   = 0;
```

### Where stake is being delegated

```sql
SELECT hex(target_finalizer) AS finalizer_raw,
       COUNT(*) AS bonds,
       ROUND(SUM(amount_zats) / 1e8, 2) AS ctaz
FROM staking_action
WHERE kind_name = 'CreateNewDelegationBond'
GROUP BY target_finalizer ORDER BY ctaz DESC;
```

### Retarget churn

Finalizers stake is moving away from, and toward.

```sql
SELECT hex(target_finalizer) AS moved_to_raw, COUNT(*) AS retargets
FROM staking_action WHERE kind_name = 'RetargetDelegationBond'
GROUP BY target_finalizer ORDER BY retargets DESC;
```

### Staking window usage

Where in the 150-block period actions actually land.

```sql
SELECT period_offset, COUNT(*) AS n
FROM staking_action GROUP BY period_offset ORDER BY period_offset;
```

Actions consensus would reject (mirrors `check_staking_day_window` exactly):

```sql
SELECT height, period_offset, kind_name, ROUND(amount_zats / 1e8, 4) AS ctaz
FROM staking_action
WHERE period_offset >= 70
  AND kind_name != 'RetargetDelegationBond'
  AND height NOT IN (1120, 2320, 2620, 2621, 3224)
ORDER BY height;
```

---

## Cross-chain

### BFT finalization against the PoW block it finalized

```sql
SELECT b.bft_height, p.height AS pow_height, p.time
FROM bft_block b JOIN pow_block p ON p.hash = b.candidate_hash
ORDER BY b.bft_height DESC LIMIT 20;
```

### Finalization lag

How far behind the PoW tip finality runs, in blocks.

```sql
WITH j AS (
  SELECT b.bft_height, p.height AS pow_height
  FROM bft_block b JOIN pow_block p ON p.hash = b.candidate_hash
)
SELECT bft_height, pow_height,
       (SELECT MAX(height) FROM pow_block) - pow_height AS blocks_behind_tip
FROM j ORDER BY bft_height DESC LIMIT 20;
```

### Time from stake to voting power

Correlate a bond's creation height with when its target finalizer's power rose.

```sql
SELECT s.height AS staked_at_pow_height,
       ROUND(s.amount_zats / 1e8, 2) AS ctaz,
       hex(s.target_finalizer) AS finalizer_raw,
       MIN(r.bft_height) AS first_bft_height_seen
FROM staking_action s
JOIN roster_entry r ON r.pub_key = s.target_finalizer
WHERE s.kind_name = 'CreateNewDelegationBond'
GROUP BY s.txid ORDER BY s.height DESC LIMIT 20;
```

---

## Exporting

```sh
sqlite3 -header -csv crosslink.sqlite "SELECT ...;" > out.csv
```

For heavier analysis, the two large tables scan much faster in DuckDB over the
same file:

```sql
INSTALL sqlite; LOAD sqlite;
SELECT * FROM sqlite_scan('crosslink.sqlite', 'roster_entry') LIMIT 10;
```
