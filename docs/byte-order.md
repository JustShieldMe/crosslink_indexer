# Byte order

**Read this before querying tables directly.** During development this caused a
wrong conclusion about a finalizer, an unusable txid, and a join that silently
matched zero rows. It is the single most likely source of a confidently wrong
answer from this database.

## The rule

**Every 32-byte value in this database is stored as RAW bytes.** The CLI
displays whichever convention the node uses for that value, so CLI output can
be pasted straight into an RPC call. If you query the tables yourself, you must
convert.

## Why it is confusing

The node uses **three** different display conventions, and they do not agree
even for the same logical thing:

| Type | Where it shows up | Node displays it |
|---|---|---|
| `PubKeyID` | certificate signatures, `get_tfl_recency_status`, `getbondinfo` bond keys | **reversed** |
| `RosterMember.pub_key` | `get_tfl_roster_zats` / `get_tfl_roster_zec` | **forward** |
| Zcash block hashes, txids | `getblock`, `getrawtransaction` | **reversed** |
| `Blake3Hash` (BFT block hashes) | BFT internals | **forward** |

The first two rows are the nasty one: the *same finalizer* appears under two
different hex strings depending on which RPC you asked. A key from
`get_tfl_recency_status` and a key from `get_tfl_roster_zats` that look
completely unrelated can be the same participant.

## Per-column reference

| Column | Stored | To match RPC output |
|---|---|---|
| `bft_signature.pub_key` | raw | **reverse** |
| `roster_entry.pub_key` | raw | **reverse** for `PubKeyID`-style RPCs; forward matches `get_tfl_roster_*` |
| `bft_block.cert_block_hash` | raw | forward — `Blake3Hash` displays forward |
| `bft_block.candidate_hash` | raw | **reverse** (it is a Zcash block hash) |
| `pow_block.hash` | raw | **reverse** |
| `staking_action.txid` | raw | **reverse** |
| `staking_action.bond_key` | raw | **reverse** (`getbondinfo` takes the reversed form) |
| `staking_action.target_finalizer` | raw | **reverse** |
| `staking_action.challenge` | raw | opaque; no RPC displays it |

Because everything is stored raw, **joins between tables always work directly**
and need no conversion:

```sql
-- correct as written
FROM roster_entry r LEFT JOIN bft_signature s USING (bft_height, pub_key)
FROM bft_block b JOIN pow_block p ON p.hash = b.candidate_hash
```

Conversion is only needed at the boundary with the node.

## Converting

The CLI does it for you:

```sh
crosslink-indexer finalizer <key-as-the-RPC-shows-it>   # default
crosslink-indexer finalizer <raw-hex> --raw-order       # literal storage bytes
```

In SQLite there is no built-in byte-reverse, so reverse in your client:

```python
import sqlite3
db = sqlite3.connect("crosslink.sqlite")

rpc_key = "034ad1af698987ba532b7a9acbaecebae8af0d5057cccda0e48d2e0acd3487fe"
raw = bytes.fromhex(rpc_key)[::-1]          # -> storage order

rows = db.execute(
    "SELECT COUNT(*) FROM bft_signature WHERE pub_key = ?", (raw,)
).fetchone()

# going the other way, for display:
display = raw[::-1].hex()
```

```sh
# shell one-liner
python3 -c "print(bytes.fromhex('<hex>')[::-1].hex())"
```

## Sanity checks

Three cheap ways to catch a byte-order mistake before it becomes a conclusion:

1. **`finalizer <key>` reporting "never appeared in any roster"** almost always
   means the wrong order, not an unknown finalizer.
2. **A PoW block hash should have leading zeros** in display order (that is what
   the difficulty target requires). If your hex *ends* in zeros, it is reversed.
3. **A join returning exactly zero rows** is the classic signature. A genuine
   mismatch usually returns *some* rows.

## Worked example

From a live node, `get_tfl_fat_pointer_to_bft_chain_tip` lists a signer as
`fe8734cd0a2e8de4...`. That is `PubKeyID` display order, so the stored key is
the reverse:

```
RPC display : fe8734cd0a2e8de4a0cdcc57500dafe8baceaecb9a7a2b53ba878969afd14a03
raw storage : 034ad1af698987ba532b7a9acbaecebae8af0d5057cccda0e48d2e0acd3487fe
```

Both strings are valid 32-byte hex and both appear in node output, which is
exactly why this is easy to get wrong: querying the display form as if it were
raw returns a plausible-looking row for a *different* finalizer, rather than an
error.
