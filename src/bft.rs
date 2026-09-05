//! Ingest of the node's `pos.chain` file: the full BFT/finalizer history.
//!
//! Record layout, mirroring the node's append path in
//! `zebra-crosslink/src/lib.rs` (`handle_new_decided_bft_block`):
//!
//! ```text
//! BftBlock | FatPointerToBftBlock | u64 roster_len | RosterMember[] | u64 sig_len | [u8;64][]
//! ```
//!
//! Safety rules this module follows, because the node holds the same file open
//! in append mode and `unwrap()`s on write failure:
//!   1. Open strictly read-only; never create, never write, never truncate.
//!   2. Commit only whole records. The tail is being appended to live, so the
//!      final record is routinely torn; we stop at the last complete one.
//!   3. Detect replacement of the file (the chain has diverged and been rebuilt
//!      before) rather than blindly resuming at a stale byte offset.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use zcash_primitives::bft::{BftBlock, FatPointerToBftBlock};
use zcash_primitives::block::BlockHash;
use zcash_primitives::transaction::RosterMember;

/// Guards against a corrupt length field turning into a huge allocation.
const MAX_ROSTER: u64 = 100_000;
const MAX_SIGS: u64 = 100_000;

pub struct Record {
    pub block: BftBlock,
    pub cert: FatPointerToBftBlock,
    pub roster: Vec<RosterMember>,
    pub proposal_sig_count: u64,
    pub byte_offset: u64,
    pub byte_len: u64,
}

/// The certificate's 44-byte vote blob is `[0..32] block hash | [32..40] height
/// LE | [40..44] round LE`, with the round's high bit set as a tag by
/// `FatPointerToBftBlock::from_parts`. Mask it back off.
pub fn cert_parts(cert: &FatPointerToBftBlock) -> ([u8; 32], u64, u32) {
    let v = &cert.vote_for_block_without_finalizer_public_key;
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&v[0..32]);
    let height = u64::from_le_bytes(v[32..40].try_into().unwrap());
    let round = u32::from_le_bytes(v[40..44].try_into().unwrap()) & 0x7fff_ffff;
    (hash, height, round)
}

/// Read records sequentially, invoking `f` for each complete one. Stops cleanly
/// at the first incomplete/torn record and returns how many whole records were
/// read and the byte offset just past the last of them.
pub fn scan<F>(path: &Path, start_offset: u64, mut f: F) -> Result<(u64, u64)>
where
    F: FnMut(Record) -> Result<()>,
{
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut r = BufReader::with_capacity(4 << 20, file);
    if start_offset > 0 {
        r.seek(SeekFrom::Start(start_offset))?;
    }

    let mut count = 0u64;
    let mut offset = start_offset;
    loop {
        let record_start = offset;

        let block = match BftBlock::zcash_deserialize(&mut r) {
            Ok(b) => b,
            Err(_) => break,
        };
        let cert = match FatPointerToBftBlock::zcash_deserialize(&mut r) {
            Ok(c) => c,
            Err(_) => break,
        };

        let mut b8 = [0u8; 8];
        if r.read_exact(&mut b8).is_err() {
            break;
        }
        let roster_len = u64::from_le_bytes(b8);
        if roster_len > MAX_ROSTER {
            anyhow::bail!("roster_len {roster_len} at offset {record_start} exceeds sanity limit; pos.chain is corrupt or the format changed");
        }
        let mut roster = Vec::with_capacity(roster_len as usize);
        let mut complete = true;
        for _ in 0..roster_len {
            match RosterMember::read_from(&mut r) {
                Ok(m) => roster.push(m),
                Err(_) => {
                    complete = false;
                    break;
                }
            }
        }
        if !complete {
            break;
        }

        if r.read_exact(&mut b8).is_err() {
            break;
        }
        let sig_len = u64::from_le_bytes(b8);
        if sig_len > MAX_SIGS {
            anyhow::bail!("proposal sig_len {sig_len} at offset {record_start} exceeds sanity limit; pos.chain is corrupt or the format changed");
        }
        for _ in 0..sig_len {
            let mut s = [0u8; 64];
            if r.read_exact(&mut s).is_err() {
                complete = false;
                break;
            }
        }
        if !complete {
            break;
        }

        offset = r.stream_position()?;
        f(Record {
            block,
            cert,
            roster,
            proposal_sig_count: sig_len,
            byte_offset: record_start,
            byte_len: offset - record_start,
        })?;
        count += 1;
    }

    Ok((count, offset))
}

/// Identity of the chain stored in this file: the hash of its first decided BFT
/// block. A `pos.chain` rebuilt for a different chain has a different one, so a
/// stale resume offset can never be applied to unrelated bytes.
pub fn chain_identity(path: &Path) -> Result<String> {
    let mut ident = None;
    // Read exactly one record.
    scan(path, 0, |rec| {
        if ident.is_none() {
            ident = Some(hex::encode(rec.block.blake3_hash().0));
        }
        // Cheap way to stop after the first record without reading 1.4 GB.
        Err(anyhow::anyhow!("__stop__"))
    })
    .ok();
    ident.context("pos.chain contains no complete BFT record")
}

pub fn index(conn: &mut Connection, path: &Path, reset: bool) -> Result<u64> {
    let fp = chain_identity(path)?;
    let stored_fp = crate::db::meta_get(conn, "pos_chain_identity")?;
    let stored_off: u64 = crate::db::meta_get(conn, "pos_chain_offset")?
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let mut start = stored_off;
    let replaced = stored_fp.as_deref().map(|s| s != fp).unwrap_or(false);
    if reset || replaced {
        if replaced {
            eprintln!(
                "pos.chain identity changed (chain rebuilt or replaced); re-indexing from scratch"
            );
        }
        conn.execute_batch(
            "DELETE FROM bft_signature; DELETE FROM roster_entry; DELETE FROM bft_block;",
        )?;
        start = 0;
    }

    let file_len = std::fs::metadata(path)?.len();
    if start > file_len {
        eprintln!("stored offset {start} is past end of file ({file_len}); re-indexing from scratch");
        conn.execute_batch(
            "DELETE FROM bft_signature; DELETE FROM roster_entry; DELETE FROM bft_block;",
        )?;
        start = 0;
    }

    // Trusting a stored byte offset is the one way this indexer could silently
    // write garbage, so verify it against what we actually recorded: the last
    // indexed record must end exactly where we intend to resume.
    if start > 0 {
        let tail: Option<(i64, i64)> = conn
            .query_row(
                "SELECT byte_offset, byte_len FROM bft_block ORDER BY bft_height DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        let consistent = matches!(tail, Some((off, len)) if (off + len) as u64 == start);
        if !consistent {
            eprintln!("stored offset {start} does not match the last indexed record; re-indexing from scratch");
            conn.execute_batch(
                "DELETE FROM bft_signature; DELETE FROM roster_entry; DELETE FROM bft_block;",
            )?;
            start = 0;
        }
    }

    let tx = conn.transaction()?;
    {
        let mut ins_block = tx.prepare(
            "INSERT OR REPLACE INTO bft_block (
                bft_height, version, cert_block_hash, cert_height, cert_round,
                candidate_hash, header_count, hardfork_count,
                do_not_include_until_bc_height, roster_size, roster_power,
                signer_count, signer_power, proposal_sig_count, byte_offset, byte_len
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
        )?;
        let mut ins_sig = tx.prepare(
            "INSERT OR REPLACE INTO bft_signature (bft_height, pub_key) VALUES (?1, ?2)",
        )?;
        let mut ins_roster = tx.prepare(
            "INSERT OR REPLACE INTO roster_entry (bft_height, pub_key, voting_power, txid_count)
             VALUES (?1, ?2, ?3, ?4)",
        )?;

        scan(path, start, |rec| {
            let h = rec.block.height as i64;
            let (cert_hash, cert_height, cert_round) = cert_parts(&rec.cert);

            // headers[0] is the finalization candidate -- the PoW block this
            // BFT block finalizes. Hashing it gives the join key to pow_block.
            let candidate_hash: Option<Vec<u8>> = rec
                .block
                .headers
                .first()
                .map(|hdr| BlockHash::from_header_data(hdr).0.to_vec());

            // Power is u64 per member and can sum past i64 on a large roster;
            // saturate rather than wrap so a bad number is obvious, not silent.
            let roster_power: u64 = rec
                .roster
                .iter()
                .fold(0u64, |a, m| a.saturating_add(m.voting_power));

            // Signer power requires matching each certificate signature back to
            // its roster entry; a signature from outside the roster contributes
            // no power (and is itself worth looking at).
            let mut signer_power: u64 = 0;
            for s in &rec.cert.signatures {
                if let Some(m) = rec.roster.iter().find(|m| m.pub_key == s.pub_key.0) {
                    signer_power = signer_power.saturating_add(m.voting_power);
                }
            }

            ins_block.execute(rusqlite::params![
                h,
                rec.block.version as i64,
                cert_hash.to_vec(),
                cert_height as i64,
                cert_round as i64,
                candidate_hash,
                rec.block.headers.len() as i64,
                rec.block.hardforks.len() as i64,
                rec.block.do_not_include_until_bc_height as i64,
                rec.roster.len() as i64,
                roster_power as i64,
                rec.cert.signatures.len() as i64,
                signer_power as i64,
                rec.proposal_sig_count as i64,
                rec.byte_offset as i64,
                rec.byte_len as i64,
            ])?;

            for s in &rec.cert.signatures {
                ins_sig.execute(rusqlite::params![h, s.pub_key.0.to_vec()])?;
            }
            for m in &rec.roster {
                ins_roster.execute(rusqlite::params![
                    h,
                    m.pub_key.to_vec(),
                    m.voting_power as i64,
                    m.txids.len() as i64,
                ])?;
            }
            Ok(())
        })
        .map(|(count, end)| {
            println!("indexed {count} BFT records, now at byte offset {end}");
            (count, end)
        })
        .and_then(|(count, end)| {
            crate::db::meta_set(&tx, "pos_chain_offset", &end.to_string())?;
            crate::db::meta_set(&tx, "pos_chain_identity", &fp)?;
            Ok((count, end))
        })?;
    }
    tx.commit()?;

    let n: i64 = conn.query_row("SELECT COUNT(*) FROM bft_block", [], |r| r.get(0))?;
    Ok(n as u64)
}
