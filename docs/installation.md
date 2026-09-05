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

## Keeping up with the monolith

When the monolith's wire formats change, rebuild:

```sh
git -C ../crosslink_monolith pull
cargo build --release
./target/release/crosslink-indexer bft --pos-chain "$POS" --reset
```

The indexer stores `bft_block.version` for every record, so a format bump is
visible in the data. See [operations.md](operations.md#when-formats-change).
