//! Replay delegation-bond state and staking rewards through a given PoW height.
//!
//! Staking rewards are state changes, not transactions: every block mints a fixed reward
//! that is split across all active bonds pro rata by size and compounded into them. They
//! cannot be read off the chain, so they are recomputed here by driving zebra-state's own
//! `StakingReplay` -- the consensus code the node itself uses -- with the staking actions
//! recorded by crosslink-indexer. Using the consensus implementation rather than a
//! re-derivation means the figures are exact, including rounding and slash burns.
//!
//! Only four fields of a staking action affect bond state (kind, amount, arg32_0 = bond
//! key, arg32_2 = target finalizer); the indexer stores all four, so no block re-fetching
//! is needed.
//!
//! Usage:
//!   replay_staking_rewards <actions.tsv> <zebrad.toml> <through-height> <out.csv>
//!
//! actions.tsv: height, tx_index, kind_name, amount_zats, bond_key_hex, target_hex
//! (raw byte order, as stored by the indexer), sorted by height then tx_index.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;

use zcash_primitives::bft::{HardForkConfig, PubKeyID};
use zcash_primitives::transaction::{StakingAction, StakingActionKind};
use zebra_chain::block::Height;
use zebra_chain::parameters::hardfork::HardForkSchedule;
use zebra_state::{StakingReplay, TransactionLocation};

const POS_BLOCK_REWARD_ZATS: u64 = 500_000_000;

fn hex32(s: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    if !s.is_empty() {
        hex::decode_to_slice(s, &mut out).expect("32-byte hex");
    }
    out
}

/// Minimal reader for the `[[crosslink.hardforks]]` tables of a node config. Reads the
/// real config rather than a hand-typed copy so the schedule cannot drift from the node's.
fn config_hardforks(path: &str) -> Vec<HardForkConfig> {
    let text = std::fs::read_to_string(path).expect("read zebrad.toml");
    let mut rules = Vec::new();
    let mut cur: Option<(u64, u64, Vec<PubKeyID>)> = None;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            if let Some((p, b, f)) = cur.take() {
                rules.push(HardForkConfig { pow_activation_height: p, bft_certificate_height: b, terminated_finalizers: f });
            }
            if l == "[[crosslink.hardforks]]" {
                cur = Some((0, 0, Vec::new()));
            }
            continue;
        }
        let Some((p, b, f)) = cur.as_mut() else { continue };
        if let Some(v) = l.strip_prefix("pow_activation_height =") {
            *p = v.trim().parse().expect("pow height");
        } else if let Some(v) = l.strip_prefix("bft_certificate_height =") {
            *b = v.trim().parse().expect("bft height");
        } else if l.starts_with('"') {
            // Config hex is display order; PubKeyID stores it reversed.
            let mut bytes = hex32(l.trim_matches(|c| c == '"' || c == ','));
            bytes.reverse();
            f.push(PubKeyID(bytes));
        }
    }
    if let Some((p, b, f)) = cur.take() {
        rules.push(HardForkConfig { pow_activation_height: p, bft_certificate_height: b, terminated_finalizers: f });
    }
    rules
}

fn kind_from_name(name: &str) -> StakingActionKind {
    match name {
        "CreateNewDelegationBond" => StakingActionKind::CreateNewDelegationBond,
        "BeginDelegationUnbonding" => StakingActionKind::BeginDelegationUnbonding,
        "WithdrawDelegationBond" => StakingActionKind::WithdrawDelegationBond,
        "RetargetDelegationBond" => StakingActionKind::RetargetDelegationBond,
        other => panic!("unexpected staking action kind {other}"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 {
        eprintln!("usage: {} <actions.tsv> <zebrad.toml> <through-height> <out.csv>", args[0]);
        std::process::exit(2);
    }
    let through: u32 = args[3].parse().expect("through height");

    let rules = config_hardforks(&args[2]);
    for r in &rules {
        println!("config hardfork: pow {} bft {} terminating {} finalizers",
                 r.pow_activation_height, r.bft_certificate_height, r.terminated_finalizers.len());
    }
    let schedule = HardForkSchedule::new(rules);
    for r in schedule.rules() {
        println!("schedule rule  : pow {} bft {} terminating {}",
                 r.pow_activation_height, r.bft_certificate_height, r.terminated_finalizers.len());
    }

    // Load actions grouped by height, preserving transaction order.
    let raw = std::fs::read_to_string(&args[1]).expect("read actions");
    let mut by_height: BTreeMap<u32, Vec<(usize, StakingAction)>> = BTreeMap::new();
    for line in raw.lines() {
        let c: Vec<&str> = line.split('\t').collect();
        if c.len() < 6 { continue }
        let height: u32 = c[0].parse().unwrap();
        let tx_index: usize = c[1].parse().unwrap();
        let action = StakingAction {
            kind: kind_from_name(c[2]),
            amount_zats: c[3].parse().unwrap(),
            arg32_0: hex32(c[4]),
            arg32_1: [0; 32],
            arg32_2: hex32(c[5]),
            arg32_3: [0; 32],
            arg64_0: [0; 64],
            arg64_1: [0; 64],
        };
        by_height.entry(height).or_default().push((tx_index, action));
    }
    let n_actions: usize = by_height.values().map(|v| v.len()).sum();
    println!("actions loaded : {n_actions} across {} heights", by_height.len());

    let mut replay = StakingReplay::new(&schedule);
    let dummy_hash = zebra_chain::transaction::Hash([0; 32]);

    // Per-bond bookkeeping the replay itself does not keep.
    let mut principal: HashMap<[u8; 32], u64> = HashMap::new();
    let mut created: HashMap<[u8; 32], u32> = HashMap::new();
    let mut original_target: HashMap<[u8; 32], [u8; 32]> = HashMap::new();
    let mut last_event: HashMap<[u8; 32], (u32, &'static str)> = HashMap::new();
    let mut retargets: HashMap<[u8; 32], u32> = HashMap::new();
    let mut total_issued: u128 = 0;
    let mut reward_blocks: u64 = 0;
    let mut burn_events: Vec<(u32, usize)> = Vec::new();

    let t0 = std::time::Instant::now();
    for h in 1..=through {
        if let Some(actions) = by_height.get(&h) {
            for (tx_index, action) in actions {
                let key = action.arg32_0;
                match action.kind {
                    StakingActionKind::CreateNewDelegationBond => {
                        principal.insert(key, action.amount_zats);
                        created.insert(key, h);
                        original_target.insert(key, action.arg32_2);
                    }
                    StakingActionKind::BeginDelegationUnbonding => { last_event.insert(key, (h, "unbond")); }
                    StakingActionKind::WithdrawDelegationBond => { last_event.insert(key, (h, "withdraw")); }
                    StakingActionKind::RetargetDelegationBond => { *retargets.entry(key).or_default() += 1; }
                    _ => {}
                }
                replay
                    .apply_staking_action(action, &dummy_hash, TransactionLocation::from_usize(Height(h), *tx_index))
                    .unwrap_or_else(|e| panic!("staking action at {h}.{tx_index} rejected: {e:?}"));
            }
        }

        let any_active = replay.delegation_bonds.values()
            .any(|(_, s)| format!("{s:?}") == "Active");
        replay.apply_block_reward();
        if any_active {
            total_issued += POS_BLOCK_REWARD_ZATS as u128;
            reward_blocks += 1;
        }

        if let Some(burns) = replay.apply_slash_burns(Height(h)) {
            println!("slash at {h}: {} finalizers terminated, {} bonds burned",
                     burns.finalizers.len(), burns.burned.len());
            for k in &burns.burned {
                last_event.insert(*k, (h, "burned"));
            }
            burn_events.push((h, burns.burned.len()));
        }

        if h % 100_000 == 0 {
            println!("  ...height {h}  bonds {}  ({:.1}s)", replay.delegation_bonds.len(), t0.elapsed().as_secs_f64());
        }
    }
    println!("replayed 1..={through} in {:.1}s", t0.elapsed().as_secs_f64());

    // ---- output ---------------------------------------------------------------------
    let mut out = std::fs::File::create(&args[4]).expect("create csv");
    writeln!(out, "bond_key,target_finalizer,original_target,created_height,principal_zats,amount_zats,accrued_zats,status,status_height,retargets").unwrap();

    let mut sum_accrued: u128 = 0;
    let mut by_status: BTreeMap<String, (u64, u128, u128)> = BTreeMap::new();
    let mut keys: Vec<_> = replay.delegation_bonds.keys().copied().collect();
    keys.sort();
    for key in keys {
        let (bond, status) = &replay.delegation_bonds[&key];
        let amount = bond.amount.zatoshis() as u64;
        let p = *principal.get(&key).expect("every bond has a create action");
        let accrued = amount.checked_sub(p).expect("bond never shrinks below principal");
        sum_accrued += accrued as u128;

        // Display order, matching logs and get_tfl_* RPC output.
        let mut t = bond.target_finalizer; t.reverse();
        let mut ot = original_target[&key]; ot.reverse();
        let status = format!("{status:?}");
        let (sh, _) = last_event.get(&key).copied().unwrap_or((0, ""));
        writeln!(out, "{},{},{},{},{},{},{},{},{},{}",
                 hex::encode(key), hex::encode(t), hex::encode(ot), created[&key],
                 p, amount, accrued, status, sh, retargets.get(&key).copied().unwrap_or(0)).unwrap();

        let e = by_status.entry(status).or_default();
        e.0 += 1; e.1 += p as u128; e.2 += amount as u128;
    }

    println!("\n=== bonds at height {through} ===");
    for (s, (n, p, a)) in &by_status {
        println!("  {s:<10} {n:>6} bonds  principal {:>16.4} ctaz  value {:>16.4} ctaz",
                 *p as f64 / 1e8, *a as f64 / 1e8);
    }
    println!("\n=== invariant: every issued zat is accounted for ===");
    println!("  blocks paying a reward : {reward_blocks}");
    println!("  issued (5 ctaz/block)  : {:.8} ctaz", total_issued as f64 / 1e8);
    println!("  sum of accrued         : {:.8} ctaz", sum_accrued as f64 / 1e8);
    println!("  EXACT MATCH            : {}", total_issued == sum_accrued);
}
