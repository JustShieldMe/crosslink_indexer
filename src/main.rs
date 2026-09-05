//! crosslink-indexer -- analytics indexer for a Crosslink node.
//!
//! Three data domains, two very different sources:
//!   * finalizer behaviour -- read from the node's `pos.chain` file
//!   * mining + staking actions -- read over JSON-RPC, staking actions decoded
//!     from raw VCrosslink transaction bytes
//! See README.md for why the split exists.

mod bft;
mod db;
mod pow;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "crosslink-indexer", version, about)]
struct Cli {
    /// SQLite database to write to.
    #[arg(long, default_value = "crosslink.sqlite", global = true)]
    db: String,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Index the BFT/finalizer history from the node's pos.chain file.
    Bft {
        /// Path to pos.chain. Opened read-only.
        #[arg(long)]
        pos_chain: PathBuf,
        /// Discard existing BFT rows and re-index from the start.
        #[arg(long)]
        reset: bool,
    },
    /// Index PoW blocks and staking actions over JSON-RPC.
    Pow {
        #[arg(long, default_value = "http://127.0.0.1:8232/")]
        rpc: String,
        /// First height to index. Defaults to resuming where the last run stopped.
        #[arg(long)]
        from: Option<u32>,
        /// Last height to index. Defaults to the current chain tip.
        #[arg(long)]
        to: Option<u32>,
        /// Blocks per SQLite transaction.
        #[arg(long, default_value_t = 500)]
        batch: u32,
        #[arg(long)]
        quiet: bool,
    },
    /// Print a summary of what is indexed.
    Stats,
    /// Detail for one finalizer, keyed the way the RPCs display it.
    Finalizer {
        /// Finalizer pubkey as shown by the RPCs (e.g. get_tfl_recency_status).
        key: String,
        /// Interpret the key as raw storage byte order instead of RPC display order.
        #[arg(long)]
        raw_order: bool,
    },
    /// Per-finalizer participation over a BFT height range.
    Participation {
        #[arg(long, default_value_t = 0)]
        from: i64,
        #[arg(long)]
        to: Option<i64>,
        #[arg(long, default_value_t = 40)]
        limit: i64,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut conn = db::open(&cli.db).with_context(|| format!("opening {}", cli.db))?;

    match cli.cmd {
        Cmd::Bft { pos_chain, reset } => {
            let total = bft::index(&mut conn, &pos_chain, reset)?;
            println!("bft_block rows: {total}");
        }
        Cmd::Pow { rpc, from, to, batch, quiet } => {
            let client = pow::Rpc::new(&rpc);
            let tip = client.block_count()?;
            let from = match from {
                Some(f) => f,
                None => db::meta_get(&conn, "pow_indexed_through")?
                    .and_then(|s| s.parse::<u32>().ok())
                    .map(|h| h + 1)
                    .unwrap_or(0),
            };
            let to = to.unwrap_or(tip).min(tip);
            if from > to {
                println!("nothing to do: already indexed through {}", to);
                return Ok(());
            }
            println!("indexing PoW blocks {from}..={to} (tip {tip})");
            let (blocks, actions) = pow::index(&mut conn, &client, from, to, batch, quiet)?;
            println!("indexed {blocks} blocks, found {actions} staking actions");
        }
        Cmd::Stats => stats(&conn)?,
        Cmd::Finalizer { key, raw_order } => finalizer(&conn, &key, raw_order)?,
        Cmd::Participation { from, to, limit } => participation(&conn, from, to, limit)?,
    }
    Ok(())
}

/// Pubkey byte order, which is a genuine trap in this codebase.
///
/// Every pubkey is stored here as the RAW 32 bytes, because that is what makes
/// `bft_signature` and `roster_entry` join correctly. But the node does NOT
/// display raw bytes consistently:
///   * `PubKeyID` (certificate signatures, `get_tfl_recency_status`,
///     `getbondinfo` bond keys) has Display/Serialize impls that REVERSE it.
///   * `RosterMember.pub_key` is a plain `[u8; 32]` serialised forward.
/// So the same finalizer appears under two different hex strings depending on
/// which RPC you asked. We render the reversed form everywhere, since that is
/// the one a user can paste back into an RPC call.
fn pk_display(raw: &[u8]) -> String {
    let mut b = raw.to_vec();
    b.reverse();
    hex::encode(b)
}

/// Accepts a finalizer key in either convention and returns the raw bytes used
/// as the storage key. Defaults to treating input as the node's display
/// (reversed) form; `raw_order` takes it literally.
fn pk_parse(s: &str, raw_order: bool) -> Result<Vec<u8>> {
    let mut b = hex::decode(s.trim()).context("finalizer key must be hex")?;
    anyhow::ensure!(b.len() == 32, "finalizer key must be 32 bytes, got {}", b.len());
    if !raw_order {
        b.reverse();
    }
    Ok(b)
}

fn scalar(conn: &rusqlite::Connection, sql: &str) -> Result<i64> {
    Ok(conn.query_row(sql, [], |r| r.get::<_, Option<i64>>(0))?.unwrap_or(0))
}

fn stats(conn: &rusqlite::Connection) -> Result<()> {
    println!("== BFT / finalizer ==");
    let n = scalar(conn, "SELECT COUNT(*) FROM bft_block")?;
    println!("  bft blocks       : {n}");
    if n > 0 {
        println!(
            "  height range     : {} .. {}",
            scalar(conn, "SELECT MIN(bft_height) FROM bft_block")?,
            scalar(conn, "SELECT MAX(bft_height) FROM bft_block")?
        );
        println!("  signatures       : {}", scalar(conn, "SELECT COUNT(*) FROM bft_signature")?);
        println!("  roster rows      : {}", scalar(conn, "SELECT COUNT(*) FROM roster_entry")?);
        println!(
            "  distinct signers : {}",
            scalar(conn, "SELECT COUNT(DISTINCT pub_key) FROM bft_signature")?
        );
        let row: (i64, i64, f64) = conn.query_row(
            "SELECT MIN(roster_size), MAX(roster_size), AVG(CAST(signer_power AS REAL)/NULLIF(roster_power,0))
             FROM bft_block",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, Option<f64>>(2)?.unwrap_or(0.0))))?;
        println!("  roster size      : {} .. {}", row.0, row.1);
        println!("  mean signed power: {:.1}%  (BFT needs > 66.7%)", row.2 * 100.0);
    }

    println!("== PoW / mining ==");
    let b = scalar(conn, "SELECT COUNT(*) FROM pow_block")?;
    println!("  blocks           : {b}");
    if b > 0 {
        println!(
            "  height range     : {} .. {}",
            scalar(conn, "SELECT MIN(height) FROM pow_block")?,
            scalar(conn, "SELECT MAX(height) FROM pow_block")?
        );
        println!(
            "  distinct miners  : {}",
            scalar(conn, "SELECT COUNT(DISTINCT miner_address) FROM pow_block")?
        );
    }

    println!("== Staking ==");
    let s = scalar(conn, "SELECT COUNT(*) FROM staking_action")?;
    println!("  actions          : {s}");
    if s > 0 {
        let mut st = conn.prepare(
            "SELECT kind_name, COUNT(*), COALESCE(SUM(amount_zats),0)
             FROM staking_action GROUP BY kind_name ORDER BY COUNT(*) DESC",
        )?;
        let rows = st.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        })?;
        for row in rows {
            let (k, c, amt) = row?;
            println!("    {k:<40} {c:>7}  {:.4} ctaz", amt as f64 / 1e8);
        }
        let bad = scalar(
            conn,
            "SELECT COUNT(*) FROM staking_action WHERE period_offset >= 70",
        )?;
        if bad > 0 {
            println!("  WARNING: {bad} actions outside the staking window (offset >= 70)");
        }
    }
    Ok(())
}

/// Eligible-but-silent is the signal worth watching: a finalizer in the roster
/// that is not in the certificate contributed no power to finality at that height.
fn participation(conn: &rusqlite::Connection, from: i64, to: Option<i64>, limit: i64) -> Result<()> {
    let to = match to {
        Some(t) => t,
        None => scalar(conn, "SELECT MAX(bft_height) FROM bft_block")?,
    };
    println!("finalizer participation over BFT heights {from}..={to}\n");
    println!(
        "{:<20} {:>8} {:>8} {:>7}  {:>18}",
        "finalizer (RPC hex)", "eligible", "signed", "rate", "mean power"
    );
    let mut st = conn.prepare(
        "SELECT r.pub_key,
                COUNT(*) AS eligible,
                COALESCE(SUM(CASE WHEN s.pub_key IS NOT NULL THEN 1 ELSE 0 END), 0) AS signed,
                AVG(CAST(r.voting_power AS REAL)) AS mean_power
         FROM roster_entry r
         LEFT JOIN bft_signature s
           ON s.bft_height = r.bft_height AND s.pub_key = r.pub_key
         WHERE r.bft_height BETWEEN ?1 AND ?2
         GROUP BY r.pub_key
         ORDER BY mean_power DESC
         LIMIT ?3",
    )?;
    let rows = st.query_map([from, to, limit], |r| {
        Ok((
            r.get::<_, Vec<u8>>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
        ))
    })?;
    for row in rows {
        let (pk, eligible, signed, power) = row?;
        let rate = if eligible > 0 { signed as f64 / eligible as f64 * 100.0 } else { 0.0 };
        println!(
            "{:<20} {eligible:>8} {signed:>8} {rate:>6.1}%  {:>18.4} ctaz",
            &pk_display(&pk)[..16],
            power / 1e8
        );
    }
    Ok(())
}

fn finalizer(conn: &rusqlite::Connection, key: &str, raw_order: bool) -> Result<()> {
    let raw = pk_parse(key, raw_order)?;
    println!("finalizer {}", pk_display(&raw));
    println!("  raw storage order : {}", hex::encode(&raw));

    let eligible: i64 = conn.query_row(
        "SELECT COUNT(*) FROM roster_entry WHERE pub_key = ?1", [&raw], |r| r.get(0))?;
    if eligible == 0 {
        println!("  never appeared in any roster (check the byte order of the key)");
        return Ok(());
    }
    let signed: i64 = conn.query_row(
        "SELECT COUNT(*) FROM bft_signature WHERE pub_key = ?1", [&raw], |r| r.get(0))?;
    let (first, last): (i64, i64) = conn.query_row(
        "SELECT MIN(bft_height), MAX(bft_height) FROM roster_entry WHERE pub_key = ?1",
        [&raw], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let power: f64 = conn.query_row(
        "SELECT AVG(CAST(voting_power AS REAL)) FROM roster_entry WHERE pub_key = ?1",
        [&raw], |r| Ok(r.get::<_, Option<f64>>(0)?.unwrap_or(0.0)))?;

    println!("  in roster heights : {first} .. {last}");
    println!("  eligible / signed : {eligible} / {signed}  ({:.1}%)",
             signed as f64 / eligible as f64 * 100.0);
    println!("  mean voting power : {:.4} ctaz", power / 1e8);

    // Recent behaviour matters more than the lifetime average for spotting a
    // finalizer that has just gone quiet.
    let tip: i64 = scalar(conn, "SELECT MAX(bft_height) FROM bft_block")?;
    for window in [100i64, 1000, 10000] {
        let lo = (tip - window).max(0);
        let e: i64 = conn.query_row(
            "SELECT COUNT(*) FROM roster_entry WHERE pub_key = ?1 AND bft_height > ?2",
            rusqlite::params![&raw, lo], |r| r.get(0))?;
        let sg: i64 = conn.query_row(
            "SELECT COUNT(*) FROM bft_signature WHERE pub_key = ?1 AND bft_height > ?2",
            rusqlite::params![&raw, lo], |r| r.get(0))?;
        if e > 0 {
            println!("  last {window:>5} heights : {sg}/{e}  ({:.1}%)", sg as f64 / e as f64 * 100.0);
        }
    }
    Ok(())
}
