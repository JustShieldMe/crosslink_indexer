# CLI reference

```
crosslink-indexer [--db <PATH>] <COMMAND>
```

`--db` is global and defaults to `crosslink.sqlite` in the working directory.
It is created on first use, with WAL journaling so queries keep working while a
backfill writes.

Commands: [`bft`](#bft) · [`pow`](#pow) · [`stats`](#stats) ·
[`participation`](#participation) · [`finalizer`](#finalizer)

---

## `bft`

Index the BFT/finalizer history from the node's `pos.chain` file. Populates
`bft_block`, `bft_signature`, and `roster_entry`.

```
crosslink-indexer bft --pos-chain <PATH> [--reset]
```

| Flag | Description |
|---|---|
| `--pos-chain <PATH>` | **Required.** Path to `pos.chain`. Opened strictly read-only. |
| `--reset` | Discard existing BFT rows and re-index from the start. |

`pos.chain` normally lives beside the node's state directory:

```sh
ls ~/.cache/zebra/*/pos.chain
```

Full index of a 1.4 GB file takes about **10 seconds**. Re-running is
incremental and takes milliseconds:

```
$ crosslink-indexer bft --pos-chain "$POS"
indexed 99363 BFT records, now at byte offset 1366818058
bft_block rows: 99363

$ crosslink-indexer bft --pos-chain "$POS"     # a minute later
indexed 33 BFT records, now at byte offset 1367320214
bft_block rows: 99396
```

### Resume semantics

The command stores a byte offset and a chain identity in `meta`. Before
resuming it verifies **all** of:

* the stored offset is not past the end of the file;
* the last indexed record ends exactly at the stored offset;
* the chain identity (hash of the first decided BFT block) still matches.

If any check fails it re-indexes from scratch and says so:

```
stored offset 999999 does not match the last indexed record; re-indexing from scratch
```

This matters because this chain has diverged and been rebuilt before — there are
`pos.chain.diverged_at_*` snapshots beside the live file. A stale offset applied
to a rebuilt file would otherwise write silent garbage.

The tail of a live `pos.chain` is routinely torn mid-append; only whole records
are committed, so a partial record is simply picked up on the next run.

---

## `pow`

Index PoW blocks and staking actions over JSON-RPC. Populates `pow_block` and
`staking_action`.

```
crosslink-indexer pow [--rpc <URL>] [--from <H>] [--to <H>] [--batch <N>] [--quiet]
```

| Flag | Default | Description |
|---|---|---|
| `--rpc <URL>` | `http://127.0.0.1:8232/` | Node JSON-RPC endpoint. |
| `--from <H>` | resume point | First height. Defaults to one past the last indexed height. |
| `--to <H>` | chain tip | Last height. Clamped to the tip. |
| `--batch <N>` | `500` | Blocks per SQLite transaction. |
| `--quiet` | off | Suppress per-batch progress lines. |

Runs at roughly **1,500 blocks/sec**, so a full 515k backfill is about
**6 minutes**. Progress reports a live rate and estimate:

```
$ crosslink-indexer pow --from 0
indexing PoW blocks 0..=515537 (tip 515537)
  515499/515537  (1505.6 blk/s, 8646 staking actions, ~0m left)
indexed 515538 blocks, found 8646 staking actions
```

Omit `--from` to resume:

```
$ crosslink-indexer pow
indexing PoW blocks 515538..=515626 (tip 515626)
indexed 89 blocks, found 3 staking actions
```

If already current it exits without work:

```
nothing to do: already indexed through 515626
```

### Notes

* Each batch commits atomically and updates the resume marker, so an
  interrupted run loses at most one batch.
* Rows are written with `INSERT OR REPLACE`, so re-indexing a range is safe and
  idempotent.
* Only `VCrosslink` transactions are fully parsed. Others are skipped after an
  8-byte version-group check, which is what keeps the throughput high.
* A transaction that fails to parse produces a warning on stderr and is skipped;
  it does not abort the run.

---

## `stats`

Summary of everything indexed. Cheap; safe to run during a backfill.

```
crosslink-indexer stats
```

```
== BFT / finalizer ==
  bft blocks       : 99397
  height range     : 0 .. 99396
  signatures       : 1360464
  roster rows      : 3888438
  distinct signers : 33
  roster size      : 1 .. 48
  mean signed power: 79.8%  (BFT needs > 66.7%)
== PoW / mining ==
  blocks           : 515627
  height range     : 0 .. 515626
  distinct miners  : 73
== Staking ==
  actions          : 8649
    CreateNewDelegationBond      8352   2159335.1871 ctaz
    RetargetDelegationBond        281         0.0000 ctaz
    BeginDelegationUnbonding       10         0.0000 ctaz
    WithdrawDelegationBond          6       824.1882 ctaz
  NOTE: 28 actions outside the staking window that consensus does not exempt
    height 352270 (offset 70)  f2cff876f7fe54223213c2db06b0a4416e065cc21ada67099e294a14b7857ebd
```

**`mean signed power`** is the average of `signer_power / roster_power`. BFT
needs more than 66.7%; a value drifting toward that line is the thing to watch.

**The staking-window NOTE** applies the consensus predicate exactly, including
both carve-outs (`RetargetDelegationBond` exempt, five hardcoded heights). Txids
are printed in RPC display order, so they paste straight into
`getrawtransaction`. See [findings.md](findings.md).

---

## `participation`

Per-finalizer participation over a BFT height range, ordered by mean voting
power.

```
crosslink-indexer participation [--from <H>] [--to <H>] [--limit <N>]
```

| Flag | Default | Description |
|---|---|---|
| `--from <H>` | `0` | First BFT height (inclusive). |
| `--to <H>` | max indexed | Last BFT height (inclusive). |
| `--limit <N>` | `40` | Rows to print. |

```
$ crosslink-indexer participation --from 95000 --limit 8
finalizer participation over BFT heights 95000..=99358

finalizer (RPC hex)  eligible   signed    rate          mean power
fe8734cd0a2e8de4         4359     4359  100.0%        1323120.2755 ctaz
aa0d7c0be893c830         4359     4085   93.7%         637108.7181 ctaz
647ef7d85182e424         4359     4028   92.4%         318840.1090 ctaz
1e62d8b43cf12ed0         4359      715   16.4%         229549.4632 ctaz
```

Keys are shown in **RPC display order** (truncated to 16 hex chars) so they can
be matched against node output. `eligible` counts heights where the finalizer
was in the roster; `signed` counts heights where it also signed the certificate.

Use a recent `--from` to spot current behaviour — a lifetime average hides a
finalizer that stopped signing last week.

---

## `finalizer`

Detail for a single finalizer, including recency windows.

```
crosslink-indexer finalizer <KEY> [--raw-order]
```

| Argument | Description |
|---|---|
| `<KEY>` | 32-byte hex pubkey, in **RPC display order** by default. |
| `--raw-order` | Interpret `<KEY>` as literal storage bytes instead. |

```
$ crosslink-indexer finalizer 034ad1af698987ba532b7a9acbaecebae8af0d5057cccda0e48d2e0acd3487fe
finalizer 034ad1af698987ba532b7a9acbaecebae8af0d5057cccda0e48d2e0acd3487fe
  raw storage order : fe8734cd0a2e8de4a0cdcc57500dafe8baceaecb9a7a2b53ba878969afd14a03
  in roster heights : 13737 .. 99358
  eligible / signed : 85622 / 0  (0.0%)
  mean voting power : 98165.9236 ctaz
  last   100 heights : 0/100  (0.0%)
  last  1000 heights : 0/1000  (0.0%)
  last 10000 heights : 0/10000  (0.0%)
```

The trailing windows are the useful part: lifetime numbers lag badly, and a
finalizer that went quiet recently shows up here immediately.

> If this reports **"never appeared in any roster"**, suspect byte order before
> concluding the finalizer is unknown. See [byte-order.md](byte-order.md).

---

## Exit codes

`0` on success; nonzero with a message on stderr for a missing `pos.chain`, an
unreachable RPC endpoint, an unreadable database, or a `pos.chain` whose
structure fails the sanity limits (which indicates corruption or a format
change).
