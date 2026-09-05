# Operations

## Keeping the database current

Both commands are incremental. A first full build is ~10 seconds for BFT and
~6 minutes for PoW; after that, catching up costs milliseconds.

```sh
#!/bin/sh
# refresh.sh
set -e
cd /home/you/crosslink-indexer
POS=~/.cache/zebra/<your-cache-dir>/pos.chain
./target/release/crosslink-indexer bft --pos-chain "$POS"
./target/release/crosslink-indexer pow --quiet
```

Run it from cron as often as you like — it is cheap and safe to run against a
live node:

```cron
*/5 * * * * /home/you/crosslink-indexer/refresh.sh >> /var/log/crosslink-indexer.log 2>&1
```

Both commands are safe to interrupt. Each PoW batch commits atomically with its
resume marker, so a killed run loses at most one batch (500 blocks by default).

## `pos.chain` safety

The running node holds `pos.chain` open in **append mode and `unwrap()`s on
write failure** — damaging the file takes the node down with it, and a resync is
expensive.

This tool therefore:

1. **Opens it strictly read-only.** It never creates, writes, truncates, or
   locks the file.
2. **Commits only whole records.** The tail of a live file is routinely torn
   mid-append; a partial record is left for the next run.
3. **Verifies the resume point** before using it — the stored offset must be
   within the file *and* land exactly at the end of the last indexed record,
   and the chain identity must match.

Never point this at a `pos.chain` you are also copying or editing, and do not
"fix" a stale offset by editing `meta` — use `--reset`.

### Why the identity check exists

This chain has diverged and been rebuilt before. Beside the live file you will
typically find snapshots like:

```
pos.chain.diverged_at_362622_20260820_153558
pos.chain.diverged_at_301318_20260806_222626
pos.chain.pre_orchard_resync_20260812_190508
```

A stale byte offset applied to a rebuilt file would read from the middle of an
unrelated record and write silent garbage. The identity check (hash of the first
decided BFT block) catches a replaced chain and forces a clean re-index:

```
pos.chain identity changed (chain rebuilt or replaced); re-indexing from scratch
```

Those `.diverged_*` snapshots are themselves indexable — point `--pos-chain` at
one with a separate `--db` to compare a fork against the live chain.

## Rebuilding

```sh
# BFT only, from scratch (~10s)
crosslink-indexer bft --pos-chain "$POS" --reset

# PoW range, re-indexed idempotently
crosslink-indexer pow --from 500000 --to 515000

# everything, from scratch
rm -f crosslink.sqlite*
crosslink-indexer bft --pos-chain "$POS"
crosslink-indexer pow --from 0
```

`rm` the `-wal` and `-shm` files alongside the database, which is what the
`crosslink.sqlite*` glob is for.

## When formats change

The monolith's wire formats are still under development. `BftBlock` is at
version 2 and its own source warns against changing the layout without bumping
the version.

After pulling monolith changes:

```sh
git -C ../crosslink_monolith pull
cargo build --release
crosslink-indexer bft --pos-chain "$POS" --reset
```

Signals that a format moved:

* a **new value in `bft_block.version`**:

  ```sql
  SELECT version, COUNT(*), MIN(bft_height), MAX(bft_height)
  FROM bft_block GROUP BY version;
  ```
* the `bft` command **erroring on a sanity limit** (`roster_len ... exceeds
  sanity limit`), which means the record layout no longer matches;
* `pow` logging **`VCrosslink parse failed`** warnings, meaning the transaction
  format moved.

The sanity limits exist so a format change fails loudly instead of allocating
wildly on a misread length field.

## Performance

Measured on the devnet node this was built against:

| Operation | Rate | Full run |
|---|---|---|
| `bft` full index (1.4 GB) | ~10M records/min | ~10 s |
| `bft` incremental | — | ~30 ms |
| `pow` backfill | ~1,500 blocks/s | ~6 min for 515k |
| `pow` incremental | — | under a second |

`pow` is bounded by the node's RPC, not by SQLite. `--batch` trades commit
frequency against restart granularity; the 500 default is a reasonable balance.
Raising it does not measurably help throughput.

The PoW pass only fully parses `VCrosslink` transactions, prefiltering on the
8-byte version group id. That prefilter is what keeps the rate at ~1,500
blocks/s rather than an order of magnitude lower.

## Sizing

About **614 MB** at PoW ~515k / BFT ~99k, dominated by `roster_entry` (~3.9M
rows) and `bft_signature` (~1.4M rows). Both scale with BFT height **times
roster size**, so a growing roster inflates the database faster than a growing
chain does.

To reclaim space after a large re-index:

```sh
sqlite3 crosslink.sqlite "VACUUM;"
```

## Querying during a backfill

WAL journaling is enabled, so reads do not block on the writer. `stats` and any
SQL client can run against the database mid-backfill; you will see a consistent
snapshot as of the last committed batch.

## Backups

The database is fully reproducible from the node — a rebuild is ~6 minutes — so
it usually does not need backing up. If you want a consistent copy while the
indexer may be running, use SQLite's own backup rather than `cp`:

```sh
sqlite3 crosslink.sqlite ".backup crosslink-backup.sqlite"
```

## Troubleshooting

**`failed to select a version for the requirement core2 = "^0.3"` / "version is
yanked"** — the `Cargo.lock` was deleted or updated. Restore it:
`cp ../crosslink_monolith/librustzcash/Cargo.lock ./Cargo.lock`. See
[installation.md](installation.md#why-cargolock-is-committed).

**`finalizer` says "never appeared in any roster"** — almost always byte order.
Try `--raw-order`, and read [byte-order.md](byte-order.md).

**A join returns zero rows** — the classic byte-order signature. Joins *between
tables* need no conversion; joins against values pasted from an RPC do.

**`pow` fails to connect** — check the endpoint and that RPC is enabled:

```sh
curl -s -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getblockcount","params":[]}' \
  http://127.0.0.1:8232/
```

If the node has `enable_cookie_auth = true`, this tool does not currently send
cookie credentials; disable cookie auth for the local endpoint or proxy it.

**`bft` re-indexes from scratch every run** — the offset check is failing.
Expected after a chain rebuild. If it repeats on an unchanged file, the database
and the file have diverged; `--reset` once to resynchronise.
