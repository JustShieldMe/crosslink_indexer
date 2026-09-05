//! Ingest of PoW blocks and staking actions over the node's JSON-RPC.
//!
//! Mining data comes straight out of `getblock <h> 2`. Staking actions do not:
//! no RPC on this node exposes them in any form. They live inside the
//! `VCrosslink` transaction variant (tx version 7, version group `0xFFFFFFFE`),
//! serialised after the Orchard bundle, so the only way to see one is to take
//! the `hex` field of each transaction and deserialise it with the node's own
//! consensus code -- which is what `Transaction::read` does here.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::time::Duration;

use zcash_protocol::consensus::BranchId;
use zcash_primitives::transaction::{StakingActionKind, Transaction};

pub const STAKING_PERIOD: u32 = 150;
pub const STAKING_DAY_WINDOW: u32 = 70;

pub struct Rpc {
    url: String,
    agent: ureq::Agent,
    id: std::cell::Cell<u64>,
}

impl Rpc {
    pub fn new(url: &str) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout(Duration::from_secs(60))
            // One pooled connection is what makes a 500k-block backfill
            // finish in minutes instead of hours.
            .max_idle_connections_per_host(4)
            .build();
        Self { url: url.to_string(), agent, id: std::cell::Cell::new(0) }
    }

    pub fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        self.id.set(self.id.get() + 1);
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": self.id.get(), "method": method, "params": params
        });
        let resp: serde_json::Value = self
            .agent
            .post(&self.url)
            .set("Content-Type", "application/json")
            .send_json(&body)
            .with_context(|| format!("RPC {method} failed"))?
            .into_json()?;
        if let Some(err) = resp.get("error") {
            if !err.is_null() {
                anyhow::bail!("RPC {method} returned error: {err}");
            }
        }
        Ok(resp.get("result").cloned().unwrap_or(serde_json::Value::Null))
    }

    pub fn block_count(&self) -> Result<u32> {
        Ok(self.call("getblockcount", serde_json::json!([]))?
            .as_u64()
            .context("getblockcount did not return a number")? as u32)
    }

    pub fn block_verbose(&self, height: u32) -> Result<serde_json::Value> {
        self.call("getblock", serde_json::json!([height.to_string(), 2]))
    }
}

pub fn kind_name(k: StakingActionKind) -> &'static str {
    match k {
        StakingActionKind::Null => "Null",
        StakingActionKind::CreateNewDelegationBond => "CreateNewDelegationBond",
        StakingActionKind::BeginDelegationUnbonding => "BeginDelegationUnbonding",
        StakingActionKind::WithdrawDelegationBond => "WithdrawDelegationBond",
        StakingActionKind::RetargetDelegationBond => "RetargetDelegationBond",
        StakingActionKind::RegisterFinalizer => "RegisterFinalizer",
        StakingActionKind::ConvertFinalizerRewardToDelegationBond => {
            "ConvertFinalizerRewardToDelegationBond"
        }
        StakingActionKind::UpdateFinalizerKey => "UpdateFinalizerKey",
    }
}

/// `arg32_2` only carries a target finalizer for the kinds that name one.
fn target_finalizer(a: &zcash_primitives::transaction::StakingAction) -> Option<Vec<u8>> {
    match a.kind {
        StakingActionKind::CreateNewDelegationBond
        | StakingActionKind::RetargetDelegationBond
        | StakingActionKind::ConvertFinalizerRewardToDelegationBond
        | StakingActionKind::UpdateFinalizerKey => Some(a.arg32_2.to_vec()),
        _ => None,
    }
}

/// Pull the miner payout out of the coinbase. The first output of the coinbase
/// is the miner's; later ones are funding streams.
fn coinbase_payout(txs: &[serde_json::Value]) -> (Option<String>, Option<i64>) {
    let Some(cb) = txs.first() else { return (None, None) };
    let Some(vouts) = cb.get("vout").and_then(|v| v.as_array()) else { return (None, None) };
    let Some(first) = vouts.first() else { return (None, None) };
    let addr = first
        .get("scriptPubKey")
        .and_then(|s| s.get("addresses"))
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
        .and_then(|a| a.as_str())
        .map(|s| s.to_string());
    let zats = first.get("valueZat").and_then(|v| v.as_i64());
    (addr, zats)
}

pub fn index(
    conn: &mut Connection,
    rpc: &Rpc,
    from: u32,
    to: u32,
    batch: u32,
    quiet: bool,
) -> Result<(u64, u64)> {
    let mut blocks_done = 0u64;
    let mut actions_found = 0u64;
    let started = std::time::Instant::now();

    let mut height = from;
    while height <= to {
        let batch_end = (height + batch - 1).min(to);
        let tx = conn.transaction()?;
        {
            let mut ins_block = tx.prepare(
                "INSERT OR REPLACE INTO pow_block
                 (height, hash, time, bits, difficulty, size, tx_count, miner_address, subsidy_zats)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            )?;
            let mut ins_action = tx.prepare(
                "INSERT OR REPLACE INTO staking_action
                 (txid, height, tx_index, kind, kind_name, amount_zats, bond_key,
                  challenge, target_finalizer, staking_period, period_offset)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            )?;

            for h in height..=batch_end {
                let blk = rpc.block_verbose(h)?;
                let hash = hex::decode(
                    blk.get("hash").and_then(|v| v.as_str()).context("block has no hash")?,
                )?;
                let txs = blk.get("tx").and_then(|v| v.as_array()).cloned().unwrap_or_default();
                let (miner, subsidy) = coinbase_payout(&txs);

                ins_block.execute(rusqlite::params![
                    h as i64,
                    hash,
                    blk.get("time").and_then(|v| v.as_i64()),
                    blk.get("bits").and_then(|v| v.as_i64()),
                    blk.get("difficulty").and_then(|v| v.as_f64()),
                    blk.get("size").and_then(|v| v.as_i64()),
                    txs.len() as i64,
                    miner,
                    subsidy,
                ])?;

                for (i, t) in txs.iter().enumerate() {
                    let Some(raw) = t.get("hex").and_then(|v| v.as_str()) else { continue };
                    let bytes = hex::decode(raw)?;
                    // Cheap prefilter: only VCrosslink transactions can carry a
                    // staking action, and they are a small minority. The version
                    // group id sits at bytes 4..8 little-endian.
                    if bytes.len() < 8 {
                        continue;
                    }
                    let vgid = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
                    if vgid != 0xFFFF_FFFE {
                        continue;
                    }
                    let parsed = match Transaction::read(&bytes[..], BranchId::Nu6) {
                        Ok(p) => p,
                        Err(e) => {
                            eprintln!("warning: height {h} tx {i}: VCrosslink parse failed: {e}");
                            continue;
                        }
                    };
                    let Some(a) = parsed.staking_action() else { continue };
                    if a.kind == StakingActionKind::Null {
                        continue;
                    }
                    ins_action.execute(rusqlite::params![
                        parsed.txid().as_ref().to_vec(),
                        h as i64,
                        i as i64,
                        u8::from(a.kind) as i64,
                        kind_name(a.kind),
                        a.amount_zats as i64,
                        a.arg32_0.to_vec(),
                        a.arg32_1.to_vec(),
                        target_finalizer(&a),
                        STAKING_PERIOD as i64,
                        (h % STAKING_PERIOD) as i64,
                    ])?;
                    actions_found += 1;
                }
                blocks_done += 1;
            }
        }
        crate::db::meta_set(&tx, "pow_indexed_through", &batch_end.to_string())?;
        tx.commit()?;

        if !quiet {
            let rate = blocks_done as f64 / started.elapsed().as_secs_f64().max(0.001);
            let remaining = (to - batch_end) as f64 / rate.max(0.001);
            println!(
                "  {batch_end}/{to}  ({:.1} blk/s, {} staking actions, ~{:.0}m left)",
                rate,
                actions_found,
                remaining / 60.0
            );
        }
        height = batch_end + 1;
    }

    Ok((blocks_done, actions_found))
}
