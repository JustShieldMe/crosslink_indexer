# crosslink-indexer

Analytics indexer for a Crosslink node. Builds a SQLite database covering three
domains that the node keeps in three very different places:

| Domain | Source | Why |
|---|---|---|
| Finalizer behaviour | the node's `pos.chain` file | RPC exposes only the current tip, not history |
| Mining / PoW blocks | JSON-RPC `getblock <h> 2` | straightforward |
| Staking actions | raw transaction bytes from that same RPC | **no RPC exposes staking actions in any form** |

## Why this repo is not dependency-free

It is a separate repo, but it is **not** independent of the node, and it cannot
be. Staking actions live in the `VCrosslink` transaction variant (tx version 7,
version group `0xFFFFFFFE`), serialised after the Orchard bundle; the BFT chain
is stored in a bespoke binary format. Both need consensus-exact parsing.

That parsing lives in the monolith's **fork** of `zcash_primitives`. The
crates.io crate of the same name and version has no `bft` module, no
`VCrosslink`, and no `StakingAction` — the crosslink version group id is even
still marked *"In-development"* in the source. So this repo carries exactly one
link to the node:

```toml
zcash_primitives = { path = "../crosslink_monolith/librustzcash/zcash_primitives" }
zcash_protocol   = { path = "../crosslink_monolith/librustzcash/components/zcash_protocol" }
```

Consequences worth knowing before you rely on this:

* **A sibling checkout is required.** `crosslink-indexer/` and
  `crosslink_monolith/` must sit in the same parent directory. To point
  elsewhere, edit the two paths in `Cargo.toml`.
* **`Cargo.lock` is committed and load-bearing.** It is seeded from
  `librustzcash/Cargo.lock` because the dependency tree pins `core2 0.3.x`,
  which is **yanked** from crates.io. Resolving from scratch fails outright.
  Do not delete it.
* **The wire formats are unstable.** `BftBlock` is already at version 2 and its
  own source warns against changing the layout without bumping the version.
  When the node's formats move, rebuild against the updated monolith and
  re-index. The indexer reads the version field and stores it, so a format
  change shows up in the data rather than being silently mis-parsed.

Vendoring a hand-written parser instead would remove the dependency, but would
silently rot the first time a format changed. Linking the real code is the
safer trade.

## Build

```sh
cargo build --release
```

## Use

```sh
POS=~/.cache/zebra/<your-cache-dir>/pos.chain

# Finalizer history: full 1.4 GB chain in ~10s. Resumable; safe to re-run.
./target/release/crosslink-indexer bft --pos-chain "$POS"

# PoW blocks + staking actions. ~2000 blocks/s; a full 515k backfill is minutes.
# Resumes from where it stopped when --from is omitted.
./target/release/crosslink-indexer pow

./target/release/crosslink-indexer stats
./target/release/crosslink-indexer participation --from 95000
./target/release/crosslink-indexer finalizer <pubkey-as-the-RPC-shows-it>
```

Both commands are incremental, so a cron entry or a `watch` loop keeps the
database current.

## Two traps

### 1. Pubkey byte order

The node does not display finalizer pubkeys consistently:

* `PubKeyID` — certificate signatures, `get_tfl_recency_status`, `getbondinfo`
  bond keys — has `Display`/`Serialize` impls that **reverse** the bytes.
* `RosterMember.pub_key` is a plain `[u8; 32]` serialised **forward**.

So the same finalizer appears under two different hex strings depending on
which RPC you asked. This indexer stores the **raw** bytes everywhere (that is
what makes `bft_signature` and `roster_entry` join correctly) and **displays**
the reversed form, because that is the one you can paste back into an RPC call.

`finalizer <key>` takes the RPC display form by default; pass `--raw-order` for
literal storage bytes. If a lookup reports "never appeared in any roster", the
byte order is the first thing to check.

### 2. `pos.chain` is a live file

The node holds it open in append mode and `unwrap()`s on write failure, so
corrupting it takes the node down with it. This indexer therefore:

1. opens it **strictly read-only** — never creates, writes, or truncates;
2. commits only **whole records**, because the tail is routinely torn mid-append;
3. **verifies the resume point** — the last indexed record must end exactly
   where the stored byte offset says, and the chain identity (hash of the first
   decided BFT block) must match — and otherwise re-indexes from scratch.

That third check matters: this chain has diverged and been rebuilt before, as
the `pos.chain.diverged_at_*` snapshots next to the live file show. A stale
byte offset applied to a rebuilt file would otherwise write silent garbage.

## Schema

`bft_block` — one row per decided BFT block: certificate hash/height/round,
`candidate_hash` (the PoW block it finalizes — **join key to `pow_block.hash`**),
roster size and total power, signer count and power, byte range in `pos.chain`.

`bft_signature (bft_height, pub_key)` — who signed each certificate.

`roster_entry (bft_height, pub_key, voting_power, txid_count)` — who was
*eligible* to sign, and with how much power.

> Participation is `roster_entry LEFT JOIN bft_signature`. Eligible-but-absent
> is the interesting set. Note that BFT needs ⅔ **by power, not by count**, so
> a certificate carrying 15 of 45 signatures is normal — weight by power.

`pow_block` — height, hash, time, bits, difficulty, size, tx count,
`miner_address` and `subsidy_zats` (from the coinbase's first output).

`staking_action` — decoded from VCrosslink bodies: `kind`/`kind_name`,
`amount_zats`, `bond_key` (arg32_0), `challenge` (arg32_1), `target_finalizer`
(arg32_2, only for kinds that name one), plus `period_offset` so window
violations show up as data. Staking actions are only consensus-valid where
`height % 150 < 70`.

## Example queries

```sql
-- Finalizers that have gone quiet: still in the roster, no longer signing.
SELECT r.pub_key, COUNT(*) eligible,
       SUM(s.pub_key IS NOT NULL) signed,
       AVG(r.voting_power)/1e8 mean_ctaz
FROM roster_entry r
LEFT JOIN bft_signature s USING (bft_height, pub_key)
WHERE r.bft_height > (SELECT MAX(bft_height) - 1000 FROM bft_block)
GROUP BY r.pub_key HAVING signed * 1.0 / eligible < 0.5
ORDER BY mean_ctaz DESC;

-- How close finality runs to the 2/3 power threshold.
SELECT bft_height, 1.0 * signer_power / roster_power AS frac
FROM bft_block WHERE roster_power > 0 AND frac < 0.75 ORDER BY frac LIMIT 50;

-- Mining concentration.
SELECT miner_address, COUNT(*) blocks, SUM(subsidy_zats)/1e8 ctaz
FROM pow_block GROUP BY miner_address ORDER BY blocks DESC;

-- Bond lifecycle for one bond.
SELECT height, kind_name, amount_zats/1e8 ctaz
FROM staking_action WHERE bond_key = ?1 ORDER BY height;

-- Cross-chain: BFT finalization against the PoW block it finalized.
SELECT b.bft_height, p.height pow_height, p.time
FROM bft_block b JOIN pow_block p ON p.hash = b.candidate_hash
ORDER BY b.bft_height DESC LIMIT 20;
```

## Limits

* **Staking rewards are not derivable.** `getbondinfo` returns only
  `{amount, status, last_action_height}` with no reward field, and nothing else
  separates yield from principal. This indexer records principal movements
  only, and does not invent a yield number.
* **`pow_block` is best-chain only**, as seen by the node at index time; reorged
  blocks are overwritten rather than retained.
* **Proposal signatures** in `pos.chain` are stored as a count. They are bare
  64-byte signatures with no pubkey attached, so attributing them would mean
  re-deriving the signing order — not attempted.
* `roster_entry.txid_count` records how many stake txids back each roster entry;
  the txids themselves are not yet expanded into their own table.
