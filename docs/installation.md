# Installation

## Requirements

* A Rust toolchain (built and tested on 1.96.0 stable).
* A checkout of `crosslink_monolith` **as a sibling directory**.
* Read access to the node's `pos.chain` file, and its JSON-RPC endpoint.

## Layout

```
parent/
├── crosslink_monolith/     <- the node
└── crosslink-indexer/      <- this repo
```

This layout is required by default because `Cargo.toml` points at the monolith
with relative paths:

```toml
zcash_primitives = { path = "../crosslink_monolith/librustzcash/zcash_primitives" }
zcash_protocol   = { path = "../crosslink_monolith/librustzcash/components/zcash_protocol" }
```

To keep the monolith elsewhere, edit those two paths.

## Why there is a path dependency at all

This repo is separate from the node, but it is **not** independent of it, and
it cannot be.

Both of the interesting formats need consensus-exact parsing:

* **Staking actions** live in the `VCrosslink` transaction variant — tx version
  7, version group `0xFFFFFFFE` — serialised after the Orchard bundle.
* **BFT blocks and certificates** use a bespoke binary layout.

That parsing exists only in the monolith's **fork** of `zcash_primitives`. The
crates.io crate of the same name and version has no `bft` module, no
`VCrosslink` transaction variant, and no `StakingAction` type. The crosslink
version group id is still marked *"In-development crosslink ID"* in the source.

The alternative — hand-writing a standalone parser — was considered and
rejected. Both formats are simple little-endian layouts, so it is very doable,
but it would silently rot the first time the node changed a field. Linking the
real code means a format change is a compile error or a version bump visible in
the data, not corrupt rows.

## Why `Cargo.lock` is committed

**Do not delete it.** The dependency tree pins `core2 0.3.x`, and *every*
published `0.3.x` of `core2` is **yanked** from crates.io. Resolving from
scratch fails outright:

```
error: failed to select a version for the requirement `core2 = "^0.3"`
  version 0.3.3 is yanked
```

The committed lockfile is seeded from `librustzcash/Cargo.lock`, which already
pins a working set. It is the reason `cargo build` works here at all.

If you ever need to regenerate it, copy the monolith's lockfile again rather
than running `cargo update`:

```sh
cp ../crosslink_monolith/librustzcash/Cargo.lock ./Cargo.lock
```

## Build

```sh
cargo build --release
```

First build compiles the Zcash cryptography stack and takes a few minutes.
Incremental rebuilds of this crate alone are a few seconds.

## Putting it on your PATH

The build leaves the binary at `target/release/crosslink-indexer`. To run it as
`crosslink-indexer` from anywhere, symlink it onto your PATH:

```sh
ln -s "$PWD/target/release/crosslink-indexer" ~/.local/bin/crosslink-indexer
```

Prefer the symlink over `cargo install --path .`. It points at the build output,
so every `cargo build --release` takes effect immediately with no reinstall
step — which matters here, because the parsers track a moving monolith and get
rebuilt often (see [below](#keeping-up-with-the-monolith)).

Check it resolves:

```sh
crosslink-indexer --help
```

## Where you run it from matters

`--db` defaults to `crosslink.sqlite` — a **relative** path, resolved against
the current working directory. The database is also created on first use. Those
two facts combine badly: running the tool outside the directory holding your
database does not fail, it silently creates a new empty one and reports zeros.

```
$ cd /tmp && crosslink-indexer stats
== BFT / finalizer ==
  bft blocks       : 0
== PoW / mining ==
  blocks           : 0
```

**If `stats` reports zeros on a database you know is populated, this is why.**
Check your working directory before concluding anything is wrong with the index,
and before re-running a backfill over it.

Either run from the repo:

```sh
cd ~/crosslink-indexer && crosslink-indexer stats
```

Or give an absolute path, which works from anywhere:

```sh
crosslink-indexer --db ~/crosslink-indexer/crosslink.sqlite stats
```

If you use it from other directories routinely, an alias pins the database once:

```sh
alias cidx='crosslink-indexer --db ~/crosslink-indexer/crosslink.sqlite'
```

## Keeping up with the monolith

When the monolith's wire formats change, rebuild:

```sh
git -C ../crosslink_monolith pull
cargo build --release
./target/release/crosslink-indexer bft --pos-chain "$POS" --reset
```

The indexer stores `bft_block.version` for every record, so a format bump is
visible in the data. See [operations.md](operations.md#when-formats-change).
