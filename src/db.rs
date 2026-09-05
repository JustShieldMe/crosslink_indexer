//! SQLite schema and helpers.
//!
//! Two ingest sources write here, and they are deliberately kept in separate
//! tables with no foreign keys between them: `pos.chain` (BFT/finalizer) can be
//! indexed from genesis in seconds, while the PoW/staking side is a slow RPC
//! backfill. Either can be rebuilt without touching the other.

use anyhow::Result;
use rusqlite::Connection;

pub const SCHEMA_VERSION: i64 = 2;

pub fn open(path: &str) -> Result<Connection> {
    let conn = Connection::open(path)?;
    // WAL keeps read queries working while a long backfill is writing.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    init(&conn)?;
    Ok(conn)
}

fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- ---------------------------------------------------------------- BFT / finalizer
-- One row per decided BFT block, read from pos.chain.
CREATE TABLE IF NOT EXISTS bft_block (
    bft_height        INTEGER PRIMARY KEY,
    version           INTEGER NOT NULL,
    -- The certificate over this block: which BFT block hash, at which height/round.
    cert_block_hash   BLOB    NOT NULL,
    cert_height       INTEGER NOT NULL,
    cert_round        INTEGER NOT NULL,
    -- headers[0] is the finalization candidate: the PoW block this BFT block finalizes.
    -- Join to pow_block.hash to cross the two chains.
    candidate_hash    BLOB,
    header_count      INTEGER NOT NULL,
    hardfork_count    INTEGER NOT NULL,
    do_not_include_until_bc_height INTEGER NOT NULL,
    -- Denormalised aggregates so the common queries avoid a join.
    roster_size       INTEGER NOT NULL,
    roster_power      INTEGER NOT NULL,
    signer_count      INTEGER NOT NULL,
    signer_power      INTEGER NOT NULL,
    proposal_sig_count INTEGER NOT NULL,
    -- Byte range of this record in pos.chain, for resuming and for forensics.
    byte_offset       INTEGER NOT NULL,
    byte_len          INTEGER NOT NULL
);

-- Who actually signed the certificate at each height. THE finalizer-behaviour table.
CREATE TABLE IF NOT EXISTS bft_signature (
    bft_height INTEGER NOT NULL,
    pub_key    BLOB    NOT NULL,
    PRIMARY KEY (bft_height, pub_key)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_bft_signature_pk ON bft_signature(pub_key, bft_height);

-- Who was eligible to sign, and with how much power. Pair with bft_signature
-- to get participation: eligible-but-did-not-sign is the interesting set.
CREATE TABLE IF NOT EXISTS roster_entry (
    bft_height   INTEGER NOT NULL,
    pub_key      BLOB    NOT NULL,
    voting_power INTEGER NOT NULL,
    txid_count   INTEGER NOT NULL,
    PRIMARY KEY (bft_height, pub_key)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_roster_entry_pk ON roster_entry(pub_key, bft_height);

-- ---------------------------------------------------------------- PoW / mining
-- `hash` is the RAW block hash, matching bft_block.candidate_hash so the two
-- chains join directly. RPCs display block hashes byte-reversed; reverse before
-- comparing against anything you copied out of a node response.
CREATE TABLE IF NOT EXISTS pow_block (
    height        INTEGER PRIMARY KEY,
    hash          BLOB    NOT NULL UNIQUE,
    time          INTEGER NOT NULL,
    bits          INTEGER,
    difficulty    REAL,
    size          INTEGER,
    tx_count      INTEGER NOT NULL,
    miner_address TEXT,
    subsidy_zats  INTEGER
);
CREATE INDEX IF NOT EXISTS idx_pow_block_miner ON pow_block(miner_address, height);
CREATE INDEX IF NOT EXISTS idx_pow_block_time  ON pow_block(time);

-- ---------------------------------------------------------------- Staking
-- Decoded from VCrosslink transaction bodies. Not available from any RPC.
CREATE TABLE IF NOT EXISTS staking_action (
    txid             BLOB    NOT NULL,
    height           INTEGER NOT NULL,
    tx_index         INTEGER NOT NULL,
    kind             INTEGER NOT NULL,
    kind_name        TEXT    NOT NULL,
    amount_zats      INTEGER NOT NULL,
    bond_key         BLOB    NOT NULL,  -- arg32_0, the unique pubkey identifying the bond
    challenge        BLOB,              -- arg32_1
    target_finalizer BLOB,              -- arg32_2, where meaningful for the kind
    -- Staking actions are only consensus-valid when height % 150 < 70; recorded
    -- so a violation shows up as data rather than being silently normalised away.
    staking_period   INTEGER NOT NULL,
    period_offset    INTEGER NOT NULL,
    PRIMARY KEY (txid, tx_index)
);
CREATE INDEX IF NOT EXISTS idx_staking_height ON staking_action(height);
CREATE INDEX IF NOT EXISTS idx_staking_bond   ON staking_action(bond_key, height);
CREATE INDEX IF NOT EXISTS idx_staking_target ON staking_action(target_finalizer, height);
CREATE INDEX IF NOT EXISTS idx_staking_kind   ON staking_action(kind, height);
"#,
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO meta(key, value) VALUES ('schema_version', ?1)",
        [SCHEMA_VERSION.to_string()],
    )?;
    Ok(())
}

pub fn meta_get(conn: &Connection, key: &str) -> Result<Option<String>> {
    let mut st = conn.prepare("SELECT value FROM meta WHERE key = ?1")?;
    let mut rows = st.query([key])?;
    Ok(match rows.next()? {
        Some(r) => Some(r.get(0)?),
        None => None,
    })
}

pub fn meta_set(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [key, value],
    )?;
    Ok(())
}
