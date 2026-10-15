//! The ledger: an append-only, hash-chained store of records.

use std::path::Path;

use rusqlite::Connection;

use crate::record::{Entry, Record, Seq, Tamper};

/// An open ledger.
pub struct Ledger {
    #[allow(dead_code)]
    conn: Connection,
}

/// Anything that can go wrong reading or appending.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("ledger: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("ledger: {0}")]
    Encoding(#[from] serde_json::Error),
    #[error("ledger: {0}")]
    Tamper(#[from] Tamper),
}

impl Ledger {
    /// Open or create a ledger file.
    pub fn open(_path: &Path) -> Result<Ledger, LedgerError> {
        unimplemented!("issue 1")
    }

    /// A ledger that lives only in memory, for tests.
    pub fn open_in_memory() -> Result<Ledger, LedgerError> {
        unimplemented!("issue 1")
    }

    /// Append a record now.
    pub fn append(&mut self, _record: Record) -> Result<Entry, LedgerError> {
        unimplemented!("issue 1")
    }

    /// Append a record with a given time (unix seconds).
    pub fn append_at(&mut self, _record: Record, _at: i64) -> Result<Entry, LedgerError> {
        unimplemented!("issue 1")
    }

    /// One entry by sequence number.
    pub fn get(&self, _seq: Seq) -> Result<Option<Entry>, LedgerError> {
        unimplemented!("issue 1")
    }

    /// Every entry, oldest first.
    pub fn all(&self) -> Result<Vec<Entry>, LedgerError> {
        unimplemented!("issue 1")
    }

    /// The last `n` entries, oldest first.
    pub fn tail(&self, _n: usize) -> Result<Vec<Entry>, LedgerError> {
        unimplemented!("issue 1")
    }

    /// How many entries there are.
    pub fn len(&self) -> Result<u64, LedgerError> {
        unimplemented!("issue 1")
    }

    /// Check the whole chain; Ok carries the number of entries verified.
    pub fn verify(&self) -> Result<u64, LedgerError> {
        unimplemented!("issue 1")
    }
}
