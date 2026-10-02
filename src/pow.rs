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
use zcash_primitives::transaction::{StakingAction, StakingActionKind, Transaction};

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
        // `StakingAction::kind()` never produces this; it only exists so
        // `StakingActionKind` has a value for "no action" in the on-disk formats
        // that need one.
        StakingActionKind::Null => "Null",
        StakingActionKind::CreateNewDelegationBond => "CreateNewDelegationBond",
        StakingActionKind::BeginDelegationUnbonding => "BeginDelegationUnbonding",
        StakingActionKind::WithdrawDelegationBond => "WithdrawDelegationBond",
        StakingActionKind::RetargetDelegationBond => "RetargetDelegationBond",
        StakingActionKind::ConvertFinalizerRewardToDelegationBond => {
            "ConvertFinalizerRewardToDelegationBond"
        }
    }
}

/// Create/Convert name a single target finalizer; Begin/Withdraw name none.
/// Retarget names two (see [from_finalizer]/[to_finalizer]) rather than one.
fn target_finalizer(a: &StakingAction) -> Option<Vec<u8>> {
    match a {
        StakingAction::CreateNewDelegationBond { .. }
        | StakingAction::ConvertFinalizerRewardToDelegationBond { .. } => {
            Some(a.target_finalizer_pk().to_vec())
        }
        _ => None,
    }
}

fn from_finalizer(a: &StakingAction) -> Option<Vec<u8>> {
    match a {
        StakingAction::RetargetDelegationBond { from_finalizer, .. } => {
            Some(from_finalizer.pub_key.0.to_vec())
        }
        _ => None,
    }
}

fn to_finalizer(a: &StakingAction) -> Option<Vec<u8>> {
    match a {
        StakingAction::RetargetDelegationBond { to_finalizer, .. } => {
            Some(to_finalizer.pub_key.0.to_vec())
        }
        _ => None,
    }
}

// No VCrosslink transaction has appeared on this featurenet yet (checked every
// block from genesis to the tip), so the column-extraction logic above can't be
// checked against a real decoded transaction. These exercise it directly
// against each `StakingAction` variant instead.
#[cfg(test)]
mod tests {
    use super::*;
    use zcash_primitives::bft::{FinalizerAddress, PubKeyID, TMSig};

    fn finalizer(byte: u8) -> FinalizerAddress {
        FinalizerAddress { pub_key: PubKeyID([byte; 32]), sig: TMSig([0; 64]) }
    }

    #[test]
    fn create_names_its_target_and_nothing_else() {
        let a = StakingAction::CreateNewDelegationBond {
            amount_zats: 500_000_000,
            unique_pubkey: [1; 32],
            bond_salt: [0; 32],
            target_finalizer: finalizer(2),
            signature: [0; 64],
        };
        assert_eq!(a.kind(), StakingActionKind::CreateNewDelegationBond);
        assert_eq!(a.bond_key(), [1; 32]);
        assert_eq!(a.amount_zats(), 500_000_000);
        assert_eq!(target_finalizer(&a), Some(vec![2; 32]));
        assert_eq!(from_finalizer(&a), None);
        assert_eq!(to_finalizer(&a), None);
    }

    #[test]
    fn begin_unbonding_names_no_finalizer_and_no_amount() {
        let a = StakingAction::BeginDelegationUnbonding { unique_pubkey: [3; 32], signature: [0; 64] };
        assert_eq!(a.bond_key(), [3; 32]);
        assert_eq!(a.amount_zats(), 0);
        assert_eq!(target_finalizer(&a), None);
        assert_eq!(from_finalizer(&a), None);
        assert_eq!(to_finalizer(&a), None);
    }

    #[test]
    fn withdraw_carries_an_amount_but_no_finalizer() {
        let a = StakingAction::WithdrawDelegationBond {
            amount_zats: 42,
            unique_pubkey: [4; 32],
            signature: [0; 64],
        };
        assert_eq!(a.amount_zats(), 42);
        assert_eq!(target_finalizer(&a), None);
    }

    #[test]
    fn retarget_splits_from_and_to_not_a_single_target() {
        let a = StakingAction::RetargetDelegationBond {
            unique_pubkey: [5; 32],
            signature: [0; 64],
            from_finalizer: finalizer(6),
            to_finalizer: finalizer(7),
        };
        assert_eq!(target_finalizer(&a), None);
        assert_eq!(from_finalizer(&a), Some(vec![6; 32]));
        assert_eq!(to_finalizer(&a), Some(vec![7; 32]));
    }

    #[test]
    fn convert_targets_this_finalizer_raw_not_wrapped() {
        let a = StakingAction::ConvertFinalizerRewardToDelegationBond {
            unique_pubkey: [8; 32],
            signature: [0; 64],
            bond_salt: [0; 32],
            this_finalizer: [9; 32],
            amount_zats: 7,
            finalizer_signature: [0; 64],
        };
        assert_eq!(target_finalizer(&a), Some(vec![9; 32]));
        assert_eq!(from_finalizer(&a), None);
        assert_eq!(to_finalizer(&a), None);
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
                  target_finalizer, from_finalizer, to_finalizer, staking_period, period_offset)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            )?;

            for h in height..=batch_end {
                let blk = rpc.block_verbose(h)?;
                // Store the RAW hash, not the RPC's display hex. Zcash block
                // hashes are shown byte-reversed, while `candidate_hash` in
                // bft_block comes from BlockHash::from_header_data and is raw.
                // Storing display order here would make that join silently
                // match nothing. Raw everywhere; reverse only to display.
                let mut hash = hex::decode(
                    blk.get("hash").and_then(|v| v.as_str()).context("block has no hash")?,
                )?;
                hash.reverse();
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
                    // v14 only accepts VCrosslink transactions under Nu6.3; the
                    // predecessor's Nu6 branch id was dropped from the sighash rules.
                    let parsed = match Transaction::read(&bytes[..], BranchId::Nu6_3) {
                        Ok(p) => p,
                        Err(e) => {
                            eprintln!("warning: height {h} tx {i}: VCrosslink parse failed: {e}");
                            continue;
                        }
                    };
                    let Some(a) = parsed.staking_action() else { continue };
                    ins_action.execute(rusqlite::params![
                        parsed.txid().as_ref().to_vec(),
                        h as i64,
                        i as i64,
                        u8::from(a.kind()) as i64,
                        kind_name(a.kind()),
                        a.amount_zats() as i64,
                        a.bond_key().to_vec(),
                        target_finalizer(&a),
                        from_finalizer(&a),
                        to_finalizer(&a),
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
