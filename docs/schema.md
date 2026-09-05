# Database schema

SQLite, WAL journaling. `schema_version` 2, recorded in `meta`.

Five data tables in two independent groups. There are deliberately **no foreign
keys between the BFT group and the PoW group**: the BFT side indexes from
genesis in seconds, the PoW side is a slower RPC backfill, and either must be
rebuildable without touching the other.

```
 pos.chain ──> bft_block ──┬──> bft_signature      (who signed)
                           └──> roster_entry       (who was eligible)
                                     │
        candidate_hash ──────────────┼──> pow_block   (cross-chain join)
                                     │
 JSON-RPC ──> pow_block ─────────────┴──> staking_action
```

> **All 32-byte values are stored RAW.** Joins between tables work directly;
> converting is only needed at the boundary with the node. See
> [byte-order.md](byte-order.md).

---

## `bft_block`

One row per decided BFT block. ~99,000 rows.

| Column | Type | Description |
|---|---|---|
| `bft_height` | INTEGER PK | BFT chain height (0-based). |
| `version` | INTEGER | `BftBlock` format version. 1 for early heights, 2 onward. A change here means the wire format moved. |
| `cert_block_hash` | BLOB(32) | BFT block hash the certificate votes for. `Blake3Hash` — displays **forward**. |
| `cert_height` | INTEGER | Height recorded inside the certificate's vote. |
| `cert_round` | INTEGER | Consensus round that decided this block. 0 = first try. Nonzero is normal; see [concepts.md](concepts.md#rounds). |
| `candidate_hash` | BLOB(32) | Hash of `headers[0]`, the PoW block this finalizes. **Join key to `pow_block.hash`.** Nullable if the block carried no headers. |
| `header_count` | INTEGER | PoW headers carried. Always 3 on this network. |
| `hardfork_count` | INTEGER | Hardfork configs embedded (v2+ only). Almost always 0. |
| `do_not_include_until_bc_height` | INTEGER | Scheduling constraint carried by the block (v2+). |
| `roster_size` | INTEGER | Finalizers eligible at this height. |
| `roster_power` | INTEGER | Total voting power of the roster, zatoshis. |
| `signer_count` | INTEGER | Signatures on the certificate. |
| `signer_power` | INTEGER | Voting power of those signers, zatoshis. |
| `proposal_sig_count` | INTEGER | Proposal signatures in the record. Count only — see [limits](#limits). |
| `byte_offset` | INTEGER | Offset of this record in `pos.chain`. |
| `byte_len` | INTEGER | Length of the record. |

`roster_size`/`roster_power`/`signer_count`/`signer_power` are denormalised from
the two child tables so the common health query needs no join:

```sql
SELECT bft_height, 1.0 * signer_power / roster_power AS power_frac
FROM bft_block WHERE roster_power > 0;
```

> `signer_power` counts only signatures whose key is in that height's roster. A
> signature from outside the roster contributes no power — and is itself worth
> looking at, since `signer_count` will exceed the number of matched keys.

---

## `bft_signature`

Who signed each certificate. `WITHOUT ROWID`. ~1.36M rows.

| Column | Type | Description |
|---|---|---|
| `bft_height` | INTEGER | Part of PK. |
| `pub_key` | BLOB(32) | Finalizer key, **raw**. Part of PK. |

`PRIMARY KEY (bft_height, pub_key)`, plus `idx_bft_signature_pk (pub_key,
bft_height)` for per-finalizer time series.

---

## `roster_entry`

Who was *eligible* to sign, and with how much power. `WITHOUT ROWID`. ~3.89M rows.

| Column | Type | Description |
|---|---|---|
| `bft_height` | INTEGER | Part of PK. |
| `pub_key` | BLOB(32) | Finalizer key, **raw**. Part of PK. |
| `voting_power` | INTEGER | Delegated stake at this height, zatoshis. |
| `txid_count` | INTEGER | Stake txids backing this entry. The txids themselves are not expanded; see [limits](#limits). |

`PRIMARY KEY (bft_height, pub_key)`, plus `idx_roster_entry_pk (pub_key,
bft_height)`.

This is the largest table — roster size times BFT height. It is what makes
participation analysis possible, since it records the denominator.

---

## `pow_block`

One row per PoW block. ~515,000 rows.

| Column | Type | Description |
|---|---|---|
| `height` | INTEGER PK | Block height. |
| `hash` | BLOB(32) UNIQUE | **Raw** block hash. Reverse to compare with RPC output. |
| `time` | INTEGER | Header timestamp, seconds UTC. Miner-supplied, not strictly monotonic. |
| `bits` | INTEGER | Compact difficulty target. |
| `difficulty` | REAL | Difficulty as the node reports it. |
| `size` | INTEGER | Serialised block size, bytes. |
| `tx_count` | INTEGER | Transactions including coinbase. |
| `miner_address` | TEXT | First coinbase output's address. Nullable. |
| `subsidy_zats` | INTEGER | First coinbase output's value. Nullable. |

Indexes: `idx_pow_block_miner (miner_address, height)`,
`idx_pow_block_time (time)`.

> `miner_address` and `subsidy_zats` are the **first** coinbase output only.
> Later outputs are funding streams and are not attributed to the miner.

Best-chain only, as seen at index time; reorged blocks are overwritten rather
than retained.

---

## `staking_action`

Decoded from `VCrosslink` transaction bodies. ~8,600 rows.

| Column | Type | Description |
|---|---|---|
| `txid` | BLOB(32) | **Raw** txid. Part of PK. Reverse for RPC. |
| `height` | INTEGER | Block height the action was mined in. |
| `tx_index` | INTEGER | Index within the block. Part of PK. |
| `kind` | INTEGER | Numeric kind, 1–7. |
| `kind_name` | TEXT | Human-readable kind. |
| `amount_zats` | INTEGER | Amount in zatoshis. Meaning depends on kind. |
| `bond_key` | BLOB(32) | `arg32_0`, the bond's unique pubkey. **Raw.** |
| `challenge` | BLOB(32) | `arg32_1`. Opaque. |
| `target_finalizer` | BLOB(32) | `arg32_2`. NULL for kinds that name no target. **Raw.** |
| `staking_period` | INTEGER | Period in force (150). |
| `period_offset` | INTEGER | `height % staking_period`. |

`PRIMARY KEY (txid, tx_index)`. Indexes on `height`, `(bond_key, height)`,
`(target_finalizer, height)`, `(kind, height)`.

See [concepts.md](concepts.md#action-kinds) for what `amount_zats` and
`target_finalizer` mean per kind — a `0` amount on a retarget is meaningful,
not missing.

`Null` (kind 0) actions are never stored.

---

## `meta`

Key/value bookkeeping.

| Key | Meaning |
|---|---|
| `schema_version` | Currently `2`. |
| `pos_chain_offset` | Byte offset reached in `pos.chain`. |
| `pos_chain_identity` | Hash of the first decided BFT block, to detect a rebuilt chain. |
| `pow_indexed_through` | Last PoW height indexed. |

Editing these by hand is how you force a re-index, but `--reset` is safer: the
`bft` command validates the offset against the last indexed record and will
re-index from scratch rather than trust an inconsistent marker.

---

## Sizing

At PoW ~515k / BFT ~99k the database is about **614 MB**, dominated by
`roster_entry` and `bft_signature`. Both grow linearly with BFT height times
roster size, so a larger roster grows the database faster than a longer chain
does.

---

## Limits

* **Staking rewards are not derivable.** No RPC or on-chain field separates
  yield from principal. Principal movements only; no yield number is invented.
* **`pow_block` is best-chain only.** Reorged blocks are overwritten, not kept.
* **Proposal signatures are a count, not rows.** In `pos.chain` they are bare
  64-byte signatures with no attached pubkey, so attributing them would require
  re-deriving the signing order. Only `proposal_sig_count` is stored.
* **Roster txids are a count, not rows.** `roster_entry.txid_count` records how
  many stake txids back each entry; expanding them into their own table would
  allow tracing bond → finalizer → participation end to end, and is the obvious
  next addition.
* **Kinds 5–7** (`RegisterFinalizer`, `ConvertFinalizerRewardToDelegationBond`,
  `UpdateFinalizerKey`) are decoded but had not appeared on this devnet, so
  their handling is untested against real data.
