# crosslink-indexer

Builds a queryable SQLite database of a Crosslink node's history, for analytics
on **mining**, **staking actions**, and **finalizer behaviour**.

## What it does

A Crosslink node keeps those three domains in three different places, and only
one of them is fully reachable over JSON-RPC. This tool reads all three and
normalises them into one database you can query with plain SQL.

| Domain | Where the node keeps it | Reachable by RPC? |
|---|---|---|
| Finalizer behaviour | the `pos.chain` file on disk | tip only — no history |
| Mining / PoW blocks | block store, via `getblock` | yes |
| Staking actions | inside raw transaction bodies | **no — not exposed at all** |

The staking gap is the reason this exists as a Rust binary rather than a script.
Staking actions live in the `VCrosslink` transaction variant (tx version 7,
version group `0xFFFFFFFE`), serialised after the Orchard bundle. No RPC decodes
them, so the only way to see one is to pull each transaction's raw bytes and
deserialise them with the node's own consensus code — which is what this does.

Likewise the BFT chain: `get_tfl_fat_pointer_to_bft_chain_tip` gives you the
current tip and nothing behind it, while `pos.chain` holds every decided block,
every certificate signature, and the full roster with voting power at every
height.

## What you get

Roughly, on a node at PoW height ~515k / BFT height ~99k:

```
bft_block        99,397 rows    one per decided BFT block
bft_signature     1.36M rows    who signed each certificate
roster_entry      3.89M rows    who was eligible, and with how much power
pow_block          515k rows    blocks, miners, subsidies, difficulty
staking_action     8,649 rows   decoded bonds, unbondings, withdrawals, retargets
                    ~614 MB
```

Indexing the full `pos.chain` (1.4 GB) takes about **10 seconds**. A full
515k-block RPC backfill takes about **6 minutes** at ~1,500 blocks/sec. Both
commands are incremental, so keeping it current costs milliseconds.

## Quickstart

Requires a sibling checkout of the monolith (see
[docs/installation.md](docs/installation.md) — this is not optional, and the
committed `Cargo.lock` is load-bearing).

```sh
cargo build --release

# optional, but assumed by every example below: put it on your PATH
ln -s "$PWD/target/release/crosslink-indexer" ~/.local/bin/crosslink-indexer

POS=~/.cache/zebra/<your-cache-dir>/pos.chain
crosslink-indexer bft --pos-chain "$POS"   # ~10s
crosslink-indexer pow                      # ~6 min first run

crosslink-indexer stats
crosslink-indexer participation --from 95000
```

Run these **from the repo directory**, or pass `--db <absolute path>`. `--db`
is a relative path by default, so the working directory decides which database
you get — see [below](#three-things-to-know-up-front).

## Documentation

| Document | What's in it |
|---|---|
| [docs/installation.md](docs/installation.md) | Build requirements, the path dependency, why `Cargo.lock` is committed |
| [docs/concepts.md](docs/concepts.md) | The Crosslink data model — BFT vs PoW, certificates, rosters, staking periods, bond lifecycle |
| [docs/cli.md](docs/cli.md) | Every command and flag, with examples and resume semantics |
| [docs/schema.md](docs/schema.md) | Every table and column, indexes, and how the tables join |
| [docs/queries.md](docs/queries.md) | A cookbook of analytics queries for all three domains |
| [docs/byte-order.md](docs/byte-order.md) | **Read before writing queries.** Three different hex conventions are in play |
| [docs/operations.md](docs/operations.md) | Keeping it current, performance, sizing, safe rebuilds |
| [docs/findings.md](docs/findings.md) | Notable things the index has surfaced so far |

## Three things to know up front

**Byte order will bite you.** The node uses three different hex display
conventions for 32-byte values, and the same finalizer can appear under two
different hex strings depending on which RPC you asked. This tool stores raw
bytes everywhere and displays the node's convention. Read
[docs/byte-order.md](docs/byte-order.md) before querying tables directly.

**`pos.chain` is a live file.** The running node holds it open in append mode
and `unwrap()`s on write failure, so damaging it takes the node down. This tool
opens it strictly read-only and never writes to it. See
[docs/operations.md](docs/operations.md#poschain-safety).

**The working directory picks your database.** `--db` defaults to the relative
path `crosslink.sqlite`, and the file is created on first use. Run the tool from
somewhere else and it will not complain — it quietly creates a second, empty
database and reports zeros:

```
$ cd /tmp && crosslink-indexer stats
== BFT / finalizer ==
  bft blocks       : 0
```

So if `stats` shows zeros on an index you know is populated, check where you are
before re-indexing anything. Pass `--db ~/crosslink-indexer/crosslink.sqlite` to
work from anywhere. See
[docs/installation.md](docs/installation.md#where-you-run-it-from-matters).

## Status

Working and in use, but young. The formats it parses are still under
development in the monolith — `BftBlock` is at version 2 and its own source
warns against changing the layout without bumping the version. When the node's
formats move, rebuild against the updated monolith and re-index. The indexer
records format versions, so a change surfaces as data rather than silent
mis-parsing.

Known limits are listed in [docs/schema.md](docs/schema.md#limits).

## License

MIT — see [LICENSE](LICENSE).
