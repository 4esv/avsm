//! The ledger: an append-only, hash-chained store of records.
//!
//! One SQLite file holds one table, `records`. Each row is an [`Entry`]:
//! numbered from 1, timed, chained to the hash of the row before it, and
//! hashed itself. Two triggers refuse every `UPDATE` and `DELETE`, and a
//! third refuses an `INSERT` of anything but the next row chained to the
//! last, so the only way to change the table is to append to it.
//!
//! Two processes, the kernel and the warden, append to the same file.
//! Every append is one `BEGIN IMMEDIATE` transaction so two appends can
//! never chain to the same previous record, and a connection waits up to
//! five seconds for the other process to finish before giving up. The
//! file keeps a rollback journal, not a write-ahead log, with
//! `synchronous=FULL`: a commit is in the main file before the append
//! returns, so deleting the journal beside it can lose no record that was
//! written (#44). Each append also checks that the file at the path is
//! still the one it opened and still holds the last record it saw, and
//! fails closed when either is not so.
//!
//! [`Ledger::verify`] reads every row back and recomputes the chain, so a
//! change to any stored byte, or a row missing from the middle, is found.
//! Rows cut off the end are not, and neither is a rewrite by the file's
//! owner, who can lift the triggers and rehash: the ledger alone cannot
//! know how long it should be or which chain is the true one. The head an
//! anchor names (`seq:hash`, printed by the digest), kept off the machine,
//! catches both; [`Ledger::holds`] checks one.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension, Params, TransactionBehavior};

use crate::record::{verify_chain, verify_chain_from, Entry, Record, Seq, Tamper, GENESIS};

/// An open ledger.
pub struct Ledger {
    conn: Connection,
    /// The file opened, by path and identity: None in memory.
    file: Option<Opened>,
    /// The last record this connection appended or found at the head,
    /// which must still be there at the next append.
    seen: Option<(i64, String)>,
    /// Where each append is noted, number and hash, when someone keeps
    /// count of what this process wrote.
    track: Option<Written>,
    /// Opened read-only from a file still in WAL mode: one this version
    /// has not opened (every open since #44 leaves the rollback journal).
    unopened: bool,
}

/// The records a process appended, by number and hash, noted as each
/// commits: the daemon tells its own writes from another writer's by
/// them (#51). Held across the append, so whoever reads it after seeing a
/// row has that row's note too.
pub type Written = std::sync::Arc<std::sync::Mutex<Vec<(Seq, String)>>>;

/// A ledger file as opened: its path, device and inode.
struct Opened {
    path: PathBuf,
    dev: u64,
    ino: u64,
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
    /// The file under the open ledger was replaced, removed or rewound:
    /// nothing more is written to it.
    #[error("ledger: {0}")]
    Gone(String),
}

/// Does the text a record is stored as read back as a record? Checked
/// before anything is written: one unreadable row fails every later read.
pub(crate) fn readable(record: &Record) -> Result<(), LedgerError> {
    serde_json::from_str::<Record>(&record.canonical())?;
    Ok(())
}

/// The table and the two triggers that make it append-only. Everything is
/// `IF NOT EXISTS`, so opening an existing ledger runs it again harmlessly.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS records (
    seq    INTEGER PRIMARY KEY,
    at     INTEGER NOT NULL,
    prev   TEXT    NOT NULL,
    hash   TEXT    NOT NULL UNIQUE,
    weight REAL    NOT NULL,
    record TEXT    NOT NULL
);
CREATE TRIGGER IF NOT EXISTS records_no_update BEFORE UPDATE ON records
BEGIN
    SELECT RAISE(ABORT, 'the ledger is append-only');
END;
CREATE TRIGGER IF NOT EXISTS records_no_delete BEFORE DELETE ON records
BEGIN
    SELECT RAISE(ABORT, 'the ledger is append-only');
END;
CREATE TRIGGER IF NOT EXISTS records_chain BEFORE INSERT ON records
WHEN NEW.seq IS NOT (SELECT IFNULL(MAX(seq), 0) + 1 FROM records)
  OR NEW.prev IS NOT IFNULL((SELECT hash FROM records ORDER BY seq DESC LIMIT 1),
     '0000000000000000000000000000000000000000000000000000000000000000')
BEGIN
    SELECT RAISE(ABORT, 'the ledger takes only the next record, chained to the last');
END;
";

/// The three triggers that make a ledger append-only, by name. Opening an
/// existing ledger re-creates any that are missing, so a dropped trigger
/// is found only by asking before the open: [`Ledger::missing_triggers`].
const TRIGGERS: [&str; 3] = ["records_no_update", "records_no_delete", "records_chain"];

/// The journal every ledger file keeps: the rollback journal, emptied at
/// each commit, so a commit is in the main file when it returns.
const JOURNAL: &str = "truncate";

/// The columns of a row, in the order [`read_row`] expects them.
const COLUMNS: &str = "seq, at, prev, hash, weight, record";

/// How long to wait for the other process to finish its append.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

impl Ledger {
    /// Open or create a ledger file. The directory must already exist.
    pub fn open(path: &Path) -> Result<Ledger, LedgerError> {
        // A new ledger is for its owner alone, and SQLite gives its journal
        // the mode of the file. One that exists is left as it is.
        {
            use std::os::unix::fs::OpenOptionsExt;
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
            {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                // Let SQLite say what is wrong with the path.
                Err(_) => {}
            }
        }
        let mut ledger = Ledger::prepare(Connection::open(path)?)?;
        let meta = std::fs::metadata(path)
            .map_err(|e| LedgerError::Gone(format!("cannot stat {}: {e}", path.display())))?;
        ledger.file = Some(Opened {
            path: path.to_path_buf(),
            dev: meta.dev(),
            ino: meta.ino(),
        });
        Ok(ledger)
    }

    /// Open an existing ledger file to read, writing nothing: no schema,
    /// no journal mode. With no write-ahead log beside it, every record is
    /// in the file and nobody is writing, so it opens immutable and no
    /// journal file is made; with one, a writer may be at work and the
    /// log is read through the files it already made.
    pub fn open_read_only(path: &Path) -> Result<Ledger, LedgerError> {
        use rusqlite::OpenFlags;
        let mut wal = path.as_os_str().to_owned();
        wal.push("-wal");
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        // A file that keeps a rollback journal is read under SQLite's
        // shared lock, as any reader would; only a file still in WAL mode
        // (one no daemon has opened since #44) needs the log's files.
        if !in_wal_mode(path) {
            let conn = Connection::open_with_flags(path, flags)?;
            conn.busy_timeout(BUSY_TIMEOUT)?;
            return Ok(Ledger::bare(conn));
        }
        let conn = if Path::new(&wal).exists() {
            Connection::open_with_flags(path, flags)?
        } else {
            // A URI, so the path is percent-encoded where it must be.
            let mut uri = String::from("file:");
            for b in path.to_string_lossy().bytes() {
                match b {
                    b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'/' | b'.' | b'-' | b'_' | b'~' => {
                        uri.push(b as char)
                    }
                    _ => uri.push_str(&format!("%{b:02X}")),
                }
            }
            uri.push_str("?immutable=1");
            Connection::open_with_flags(uri, flags | OpenFlags::SQLITE_OPEN_URI)?
        };
        conn.busy_timeout(BUSY_TIMEOUT)?;
        let mut ledger = Ledger::bare(conn);
        ledger.unopened = true;
        Ok(ledger)
    }

    /// A ledger on a connection, with no file identity and nothing seen.
    fn bare(conn: Connection) -> Ledger {
        Ledger {
            conn,
            file: None,
            seen: None,
            track: None,
            unopened: false,
        }
    }

    /// Note every append from now on in `written`.
    pub fn track(&mut self, written: Written) {
        self.track = Some(written);
    }

    /// A ledger that lives only in memory, for tests.
    pub fn open_in_memory() -> Result<Ledger, LedgerError> {
        Ledger::prepare(Connection::open_in_memory()?)
    }

    /// Set the connection up and make sure the table and triggers exist.
    fn prepare(conn: Connection) -> Result<Ledger, LedgerError> {
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // NOTE: turning a new file to WAL takes an exclusive lock, and
        // SQLite answers busy at once there instead of calling the busy
        // handler. avsmd's kernel and warden open a new ledger at the same
        // moment, so the loser waits here, as long as an append would.
        // Leaving WAL mode (a ledger written before #44) also needs the
        // file to itself: SQLite then answers with the mode it kept, and
        // that is waited out the same way.
        let began = std::time::Instant::now();
        loop {
            // An in-memory database has no file to journal; it stays in
            // "memory" mode, and SQLite reports that instead of failing.
            let set = conn
                .pragma_update_and_check(None, "journal_mode", JOURNAL, |row| {
                    row.get::<_, String>(0)
                })
                .and_then(|mode| {
                    if mode != JOURNAL && mode != "memory" {
                        return Err(rusqlite::Error::SqliteFailure(
                            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                            Some(format!("the ledger stays in {mode} mode")),
                        ));
                    }
                    conn.pragma_update(None, "synchronous", "FULL")?;
                    conn.execute_batch(SCHEMA)
                });
            match set {
                Err(rusqlite::Error::SqliteFailure(e, _))
                    if e.code == rusqlite::ErrorCode::DatabaseBusy
                        && began.elapsed() < BUSY_TIMEOUT =>
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                set => break set?,
            }
        }
        let mut ledger = Ledger::bare(conn);
        ledger.seen = ledger.head()?;
        Ok(ledger)
    }

    /// The last row's seq and hash, if there is one.
    fn head(&self) -> Result<Option<(i64, String)>, LedgerError> {
        Ok(self
            .conn
            .query_row(
                "SELECT seq, hash FROM records ORDER BY seq DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    /// Is the file at the path still the one this ledger opened? A file
    /// moved over it or put back from a copy is another inode, and every
    /// append to the old one would be lost.
    fn same_file(&self) -> Result<(), LedgerError> {
        let Some(opened) = &self.file else {
            return Ok(());
        };
        match std::fs::metadata(&opened.path) {
            Ok(m) if m.dev() == opened.dev && m.ino() == opened.ino => Ok(()),
            Ok(_) => Err(LedgerError::Gone(format!(
                "{} was replaced under this process; nothing more is written to it",
                opened.path.display()
            ))),
            Err(e) => Err(LedgerError::Gone(format!(
                "{} is gone under this process ({e}); nothing more is written to it",
                opened.path.display()
            ))),
        }
    }

    /// Append a record now.
    pub fn append(&mut self, record: Record) -> Result<Entry, LedgerError> {
        self.append_at(record, now())
    }

    /// Append a record with a given time (unix seconds).
    ///
    /// Reading the last row and inserting the new one happen in one
    /// `BEGIN IMMEDIATE` transaction, which takes the write lock up front.
    /// The other process waits its turn, so two appends can never both
    /// chain to the same previous record.
    pub fn append_at(&mut self, record: Record, at: i64) -> Result<Entry, LedgerError> {
        // A ledger must never store a record it cannot read.
        readable(&record)?;
        self.same_file()?;
        // Held from before the write until it is noted: a reader of the
        // notes waits for an append it may already see in the file.
        let track = self.track.clone();
        let mut noted = track
            .as_ref()
            .map(|t| t.lock().unwrap_or_else(|p| p.into_inner()));
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Another avsmd took the home: nothing more from this one (#56).
        crate::lock::may_write().map_err(LedgerError::Gone)?;
        let last: Option<(i64, String)> = tx
            .query_row(
                "SELECT seq, hash FROM records ORDER BY seq DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        // The last record this connection saw must still be there: a
        // ledger cut short or put back from an older copy under a running
        // process is refused, not appended to with reused numbers.
        if let Some((seen, hash)) = &self.seen {
            let there: Option<String> = tx
                .query_row("SELECT hash FROM records WHERE seq = ?1", [seen], |row| {
                    row.get(0)
                })
                .optional()?;
            if there.as_ref() != Some(hash) {
                return Err(LedgerError::Gone(format!(
                    "record #{seen} ({}) is no longer in the ledger: it was cut short or \
                     rewound under this process; nothing more is written to it",
                    &hash[..hash.len().min(16)]
                )));
            }
        }
        let (seq, prev) = match last {
            Some((seq, hash)) => (seq + 1, hash),
            None => (1, GENESIS.to_string()),
        };
        let entry = Entry::new(unsigned(seq)?, at, &prev, record);
        tx.execute(
            "INSERT INTO records (seq, at, prev, hash, weight, record) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                seq,
                entry.at,
                entry.prev,
                entry.hash,
                entry.weight,
                entry.record.canonical()
            ],
        )?;
        // Told before it commits: a warden killed once it has committed
        // has already told the daemon, which never takes the record for
        // another writer's (#52). A note of a record that never commits
        // matches no record.
        if noted.is_some() {
            crate::groups::note_record(entry.seq, &entry.hash);
        }
        tx.commit()?;
        #[cfg(debug_assertions)]
        if crate::groups::noting() && entry.record.kind() == crate::record::Kind::Outcome {
            hold_for_test();
        }
        self.seen = Some((seq, entry.hash.clone()));
        if let Some(noted) = noted.as_mut() {
            noted.push((entry.seq, entry.hash.clone()));
        }
        Ok(entry)
    }

    /// One entry by sequence number.
    pub fn get(&self, seq: Seq) -> Result<Option<Entry>, LedgerError> {
        let Ok(seq) = i64::try_from(seq) else {
            return Ok(None);
        };
        self.conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM records WHERE seq = ?1"),
                [seq],
                read_row,
            )
            .optional()?
            .map(to_entry)
            .transpose()
    }

    /// Every entry, oldest first.
    pub fn all(&self) -> Result<Vec<Entry>, LedgerError> {
        self.select(&format!("SELECT {COLUMNS} FROM records ORDER BY seq"), [])
    }

    /// The last `n` entries, oldest first.
    pub fn tail(&self, n: usize) -> Result<Vec<Entry>, LedgerError> {
        let n = i64::try_from(n).unwrap_or(i64::MAX);
        let mut entries = self.select(
            &format!("SELECT {COLUMNS} FROM records ORDER BY seq DESC LIMIT ?1"),
            [n],
        )?;
        entries.reverse();
        Ok(entries)
    }

    /// Every entry after `seq`, oldest first: what is new since a reader
    /// last looked.
    pub fn after(&self, seq: Seq) -> Result<Vec<Entry>, LedgerError> {
        let Ok(seq) = i64::try_from(seq) else {
            return Ok(Vec::new());
        };
        self.select(
            &format!("SELECT {COLUMNS} FROM records WHERE seq > ?1 ORDER BY seq"),
            [seq],
        )
    }

    /// Write a copy of the ledger as it stands, in one read transaction,
    /// to a new file at `path`: a forecast's fork at the head.
    pub fn copy_to(&self, path: &Path) -> Result<(), LedgerError> {
        self.conn
            .execute("VACUUM INTO ?1", params![path.to_string_lossy()])?;
        Ok(())
    }

    /// Stop waiting for the disk on each append: for a forecast branch's
    /// copy, which is deleted after, never for a ledger that is kept.
    pub(crate) fn throwaway(&self) -> Result<(), LedgerError> {
        self.conn.pragma_update(None, "synchronous", "OFF")?;
        Ok(())
    }

    /// How many entries there are.
    pub fn len(&self) -> Result<u64, LedgerError> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM records", [], |row| {
                unsigned(row.get(0)?)
            })?)
    }

    /// Is there nothing in the ledger yet?
    pub fn is_empty(&self) -> Result<bool, LedgerError> {
        Ok(self.len()? == 0)
    }

    /// Check the whole chain; Ok carries the number of entries verified.
    ///
    /// Every row is read back and decoded, then the chain is recomputed
    /// from the decoded fields: sequence numbers must run 1, 2, 3..., each
    /// entry must name the hash of the one before it, and each hash must
    /// match the entry's contents.
    pub fn verify(&self) -> Result<u64, LedgerError> {
        let entries = self.all()?;
        verify_chain(&entries)?;
        Ok(entries.len() as u64)
    }

    /// The schema version SQLite keeps in the file's header. Appends
    /// leave it alone; dropping or creating a trigger changes it.
    pub fn schema_version(&self) -> Result<i64, LedgerError> {
        Ok(self
            .conn
            .query_row("PRAGMA schema_version", [], |row| row.get(0))?)
    }

    /// The append-only triggers that must be in the schema, and are not:
    /// each name here is one someone dropped. Dropping one is how the
    /// file's owner edits an append-only ledger, and a later open would
    /// re-create it silently, so a read-only open checks this first. The
    /// chain trigger is expected only once this version has opened the file.
    pub fn missing_triggers(&self) -> Result<Vec<&'static str>, LedgerError> {
        let mut missing = Vec::new();
        // A ledger no open of this version has touched still has 1.0's
        // two triggers; the chain trigger is added by the first open.
        let unopened = self.unopened;
        for name in TRIGGERS {
            if unopened && name == "records_chain" {
                continue;
            }
            let there = self
                .conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type = 'trigger' AND name = ?1",
                    [name],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !there {
                missing.push(name);
            }
        }
        Ok(missing)
    }

    /// Check only the entries after entry `seq`, whose hash is `prev`, as
    /// `verify` checks them all; `(0, GENESIS)` checks the whole chain.
    /// Ok carries how many entries were checked and the number and hash
    /// of the last one (`seq` and `prev` when there were none).
    pub fn verify_after(&self, seq: Seq, prev: &str) -> Result<(u64, Seq, String), LedgerError> {
        let entries = self.after(seq)?;
        verify_chain_from(seq, prev, &entries)?;
        Ok(match entries.last() {
            Some(e) => (entries.len() as u64, e.seq, e.hash.clone()),
            None => (0, seq, prev.to_string()),
        })
    }

    /// The first row that does not read or does not chain, oldest first:
    /// where the ledger stops being sound, and so where a cut keeps the
    /// most history. None when every row reads and the chain verifies.
    pub fn first_bad(&self) -> Result<Option<Tamper>, LedgerError> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {COLUMNS} FROM records ORDER BY seq"))?;
        let mut rows = stmt.query([])?;
        let (mut seq, mut prev): (Seq, String) = (0, GENESIS.to_string());
        while let Some(row) = rows.next()? {
            let at = row
                .get::<_, i64>(0)
                .ok()
                .and_then(|s| u64::try_from(s).ok())
                .unwrap_or(seq + 1);
            let entry = match read_row(row) {
                Err(e) => {
                    return Ok(Some(Tamper {
                        seq: at,
                        what: format!("does not read: {e}"),
                    }))
                }
                Ok(stored) => match to_entry(stored) {
                    Ok(entry) => entry,
                    Err(LedgerError::Tamper(t)) => return Ok(Some(t)),
                    Err(e) => return Err(e),
                },
            };
            if let Err(t) = verify_chain_from(seq, &prev, std::slice::from_ref(&entry)) {
                return Ok(Some(t));
            }
            seq = entry.seq;
            prev = entry.hash;
        }
        Ok(None)
    }

    /// Does the ledger still hold `hash` (or a hash it begins) at `seq`,
    /// or anywhere when `seq` is None? An anchor kept off the machine is
    /// checked this way: a rewrite or a rewind past it is a no. Ok carries
    /// the seq it was found at.
    pub fn holds(&self, seq: Option<Seq>, hash: &str) -> Result<Option<Seq>, LedgerError> {
        let prefix = hash.to_ascii_lowercase();
        if prefix.is_empty() || !prefix.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(None);
        }
        let found: Vec<(i64, String)> = match seq {
            Some(seq) => {
                let Ok(seq) = i64::try_from(seq) else {
                    return Ok(None);
                };
                let mut stmt = self
                    .conn
                    .prepare("SELECT seq, hash FROM records WHERE seq = ?1")?;
                let rows = stmt.query_map([seq], |r| Ok((r.get(0)?, r.get(1)?)))?;
                rows.collect::<Result<_, _>>()?
            }
            None => {
                let mut stmt = self
                    .conn
                    .prepare("SELECT seq, hash FROM records WHERE substr(hash, 1, ?1) = ?2")?;
                let rows = stmt.query_map(params![prefix.len() as i64, prefix], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?;
                rows.collect::<Result<_, _>>()?
            }
        };
        Ok(found
            .into_iter()
            .find(|(_, h)| h.starts_with(&prefix))
            .and_then(|(s, _)| u64::try_from(s).ok()))
    }

    /// Cut the ledger just before `seq`, keeping every record before it,
    /// after a copy of the whole file is written to `backup` (which must
    /// not exist). The triggers that forbid a delete are lifted and put
    /// back in the one transaction that deletes, so no other writer ever
    /// sees the table without them. Ok carries how many rows were cut.
    /// The caller records the cut, declared, as the next record.
    pub fn cut(&mut self, seq: Seq, backup: &Path) -> Result<u64, LedgerError> {
        let last = self.head()?.map_or(0, |(s, _)| s);
        let at = i64::try_from(seq).unwrap_or(i64::MAX);
        if seq == 0 || at > last {
            return Err(LedgerError::Gone(format!(
                "there is no record #{seq} to cut before; the last is #{last}"
            )));
        }
        if let Some(bad) = self.first_bad()?.filter(|t| t.seq < seq) {
            return Err(LedgerError::Gone(format!(
                "record #{} before it is bad too ({}); cut there instead",
                bad.seq, bad.what
            )));
        }
        self.conn
            .execute("VACUUM INTO ?1", params![backup.to_string_lossy()])?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch("DROP TRIGGER IF EXISTS records_no_delete")?;
        let cut = tx.execute("DELETE FROM records WHERE seq >= ?1", [at])?;
        tx.execute_batch(SCHEMA)?;
        tx.commit()?;
        self.seen = self.head()?;
        Ok(cut as u64)
    }

    /// Run a query that selects [`COLUMNS`] and decode every row it returns.
    fn select<P: Params>(&self, sql: &str, params: P) -> Result<Vec<Entry>, LedgerError> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params, read_row)?;
        rows.map(|row| to_entry(row?)).collect()
    }
}

/// Is the SQLite file at `path` in WAL mode? Bytes 18 and 19 of its
/// header are 2 then; 1 for a rollback journal.
fn in_wal_mode(path: &Path) -> bool {
    use std::io::Read;
    let mut header = [0u8; 20];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut header))
        .is_ok_and(|()| header[18] == 2)
}

/// Unix seconds now. Before 1970 the clock reads negative, as it should.
fn now() -> i64 {
    crate::clock::now()
}

/// A test hook, in debug builds only: with `AVSMD_TEST_HOLD_OUTCOME`
/// naming a file that does not exist yet, the first outcome a tracking
/// process commits creates it and the append holds there for good, its
/// note to the process's own count not yet made, so a test can kill the
/// warden at that moment.
#[cfg(debug_assertions)]
fn hold_for_test() {
    let Some(path) = std::env::var_os("AVSMD_TEST_HOLD_OUTCOME") else {
        return;
    };
    let fresh = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .is_ok();
    if fresh {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    }
}

/// SQLite integers are signed; sequence numbers and counts are not.
fn unsigned(n: i64) -> rusqlite::Result<u64> {
    u64::try_from(n).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, n))
}

/// One row as stored: seq, at, prev, hash, weight, and the record's JSON.
type Stored = (Seq, i64, String, String, f64, String);

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Stored> {
    Ok((
        unsigned(row.get(0)?)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
    ))
}

/// Decode a stored row into an entry.
///
/// The record text must be exactly the record's canonical JSON. The hash
/// covers that canonical form, not the stored bytes, so a change that
/// leaves the text readable as the same record (a renamed key the record
/// does not need, `1E3` for `1e3`) would otherwise go unnoticed.
fn to_entry((seq, at, prev, hash, weight, text): Stored) -> Result<Entry, LedgerError> {
    // A row that does not read is named, so the cut before it can be.
    let record: Record = serde_json::from_str(&text).map_err(|e| Tamper {
        seq,
        what: format!("does not read: {e}"),
    })?;
    if record.canonical() != text {
        return Err(Tamper {
            seq,
            what: "stored text is not the record's canonical JSON".to_string(),
        }
        .into());
    }
    Ok(Entry {
        seq,
        at,
        prev,
        hash,
        weight,
        record,
    })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use proptest::sample::Index;

    use super::*;
    use crate::gate::{Proof, Refusal};
    use crate::record::{Claim, Kind, Payload, Verdict};

    fn report(value: f64) -> Record {
        Record::report(Claim::Measured, "counter", "count", Some(value), "")
    }

    /// Short strings of any characters: quotes, backslashes, control
    /// characters and non-ASCII all have to survive JSON and SQLite.
    fn text() -> impl Strategy<Value = String> {
        prop::collection::vec(any::<char>(), 0..12).prop_map(String::from_iter)
    }

    /// Any finite f64: both signs, zero, subnormals, the lot.
    fn finite() -> impl Strategy<Value = f64> {
        use prop::num::f64::{NEGATIVE, NORMAL, POSITIVE, SUBNORMAL, ZERO};
        POSITIVE | NEGATIVE | NORMAL | SUBNORMAL | ZERO
    }

    fn claim() -> impl Strategy<Value = Claim> {
        prop_oneof![
            Just(Claim::Measured),
            Just(Claim::Declared),
            Just(Claim::Inferred),
        ]
    }

    /// Any payload: every shape, with json objects of short keys.
    fn payload() -> impl Strategy<Value = Payload> {
        prop_oneof![
            text().prop_map(|text| Payload::Text { text }),
            prop::collection::btree_map(text(), text(), 0..3).prop_map(|map| Payload::Json {
                json: serde_json::Value::Object(
                    map.into_iter()
                        .map(|(k, v)| (k, serde_json::Value::String(v)))
                        .collect()
                ),
            }),
            text().prop_map(|diff| Payload::Diff { diff }),
            text().prop_map(|path| Payload::File { path }),
        ]
    }

    fn verdict() -> impl Strategy<Value = Verdict> {
        prop_oneof![
            (any::<Seq>(), prop::option::of(payload()))
                .prop_map(|(ask, payload)| Verdict::Approve { ask, payload }),
            (any::<Seq>(), text()).prop_map(|(ask, reason)| Verdict::Deny { ask, reason }),
            Just(Verdict::Pause),
            Just(Verdict::Resume),
            (text(), text(), proptest::option::of(text())).prop_map(|(mandate, path, text)| {
                Verdict::Adopt {
                    mandate,
                    path,
                    text,
                }
            }),
        ]
    }

    fn proof() -> impl Strategy<Value = Proof> {
        (text(), text()).prop_map(|(venture, action)| Proof {
            venture,
            action,
            steps: vec![],
        })
    }

    fn refusal() -> impl Strategy<Value = Refusal> {
        (text(), text()).prop_map(|(venture, action)| Refusal {
            venture,
            action,
            steps: vec![],
            reasons: vec![],
        })
    }

    /// Any record: all nine kinds, all three claims, every optional field
    /// present or absent.
    fn record() -> impl Strategy<Value = Record> {
        prop_oneof![
            (
                claim(),
                text(),
                text(),
                prop::option::of(finite()),
                text(),
                prop::option::of(text()),
                prop::option::of(finite()),
                prop::collection::vec(any::<Seq>(), 0..3),
            )
                .prop_map(
                    |(claim, venture, metric, value, note, source, confidence, cites)| {
                        Record::Report {
                            claim,
                            venture,
                            metric,
                            value,
                            note,
                            source,
                            confidence,
                            cites,
                            implausible: None,
                            facts: None,
                        }
                    }
                ),
            (claim(), text(), text(), text(), prop::option::of(payload())).prop_map(
                |(claim, venture, action, reason, payload)| Record::Proposal {
                    cites: vec![],
                    claim,
                    venture,
                    action,
                    reason,
                    payload,
                }
            ),
            (
                claim(),
                text(),
                text(),
                proof(),
                any::<bool>(),
                text(),
                prop::option::of(payload())
            )
                .prop_map(|(claim, venture, action, proof, ok, detail, payload)| {
                    Record::Action {
                        claim,
                        venture,
                        action,
                        proof,
                        ok,
                        detail,
                        payload,
                    }
                }),
            (
                claim(),
                text(),
                prop::option::of(text()),
                text(),
                prop::option::of(refusal()),
                prop::option::of(payload()),
                prop::option::of((text(), text()))
            )
                .prop_map(
                    |(claim, venture, action, question, refusal, payload, signal)| Record::Ask {
                        packet: None,
                        cites: vec![],
                        claim,
                        venture,
                        action,
                        question,
                        refusal,
                        payload,
                        signal: signal.map(|(action, reason)| crate::record::Signal::Saturated {
                            action,
                            reason,
                        }),
                    }
                ),
            (claim(), text(), verdict(), prop::option::of(text())).prop_map(
                |(claim, venture, verdict, by)| Record::Decision {
                    claim,
                    venture,
                    verdict,
                    by,
                }
            ),
            (
                claim(),
                text(),
                text(),
                proof(),
                prop::option::of(payload()),
                prop::option::of(text())
            )
                .prop_map(|(claim, venture, action, proof, payload, key)| {
                    Record::Intent {
                        claim,
                        venture,
                        action,
                        proof,
                        payload,
                        key,
                    }
                }),
            (claim(), text(), any::<Seq>(), any::<bool>(), text()).prop_map(
                |(claim, venture, intent, ok, detail)| Record::Outcome {
                    claim,
                    venture,
                    intent,
                    ok,
                    detail,
                }
            ),
            (
                claim(),
                text(),
                text(),
                text(),
                text(),
                any::<u32>(),
                prop::option::of(any::<Seq>()),
                prop::collection::vec(any::<Seq>(), 0..3)
            )
                .prop_map(
                    |(claim, venture, action, reason, detail, count, closes, cites)| {
                        Record::Refusal {
                            claim,
                            venture,
                            action,
                            reason,
                            detail,
                            count,
                            closes,
                            cites,
                        }
                    }
                ),
            (
                claim(),
                text(),
                text(),
                text(),
                text(),
                prop::option::of(text())
            )
                .prop_map(|(claim, venture, from, to, why, refused)| {
                    Record::Escalation {
                        claim,
                        venture,
                        from,
                        to,
                        why,
                        refused,
                    }
                }),
            (
                claim(),
                text(),
                any::<Seq>(),
                any::<bool>(),
                prop::collection::vec(any::<Seq>(), 0..3),
                text()
            )
                .prop_map(|(claim, venture, escalation, ok, cites, detail)| {
                    Record::Answer {
                        claim,
                        venture,
                        escalation,
                        ok,
                        cites,
                        detail,
                    }
                }),
            (
                claim(),
                text(),
                text(),
                prop::option::of(text()),
                any::<i64>(),
                prop::collection::vec(finite(), 7),
                prop::collection::vec((text(), finite()), 0..3),
            )
                .prop_map(|(claim, venture, metric, action, made_at, q, events)| {
                    Record::Prediction {
                        claim,
                        venture,
                        model: "baseline".to_string(),
                        metric,
                        action,
                        made_at,
                        resolves_at: made_at / 2,
                        quantiles: crate::record::Quantiles::from_values([
                            q[0], q[1], q[2], q[3], q[4], q[5], q[6],
                        ]),
                        events: events
                            .into_iter()
                            .map(|(name, p)| crate::record::Event { name, p })
                            .collect(),
                    }
                }),
        ]
    }

    /// Between 1 and 40 records with their times, ready to append.
    fn records() -> impl Strategy<Value = Vec<(Record, i64)>> {
        prop::collection::vec((record(), any::<i64>()), 1..=40)
    }

    /// An in-memory ledger holding `records`, and what `append_at` returned.
    fn filled(records: &[(Record, i64)]) -> (Ledger, Vec<Entry>) {
        let mut ledger = Ledger::open_in_memory().unwrap();
        let entries = records
            .iter()
            .map(|(record, at)| ledger.append_at(record.clone(), *at).unwrap())
            .collect();
        (ledger, entries)
    }

    /// A stored column a tamperer might touch.
    #[derive(Clone, Copy, Debug)]
    enum Column {
        At,
        Prev,
        Hash,
        Weight,
        Record,
    }

    const TAMPERABLE: &[Column] = &[
        Column::At,
        Column::Prev,
        Column::Hash,
        Column::Weight,
        Column::Record,
    ];

    proptest! {
        /// Claim and kind are typed: the JSON carries them under those
        /// names as the enums' own values, and the JSON reads back as the
        /// very same record, byte for byte.
        #[test]
        fn record_json_is_typed_and_round_trips(record in record()) {
            let json = record.canonical();
            let value: serde_json::Value = serde_json::from_str(&json).unwrap();
            let kind: Kind = serde_json::from_value(value["kind"].clone()).unwrap();
            let claim: Claim = serde_json::from_value(value["claim"].clone()).unwrap();
            prop_assert_eq!(kind, record.kind());
            prop_assert_eq!(claim, record.claim());
            let back: Record = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(&back, &record);
            prop_assert_eq!(back.canonical(), json);
        }

        /// After N appends the chain verifies, and every way of reading
        /// the ledger agrees with every other.
        #[test]
        fn chain_verifies_after_appends(records in records(), k in 0usize..=41) {
            let (ledger, appended) = filled(&records);
            let n = records.len();
            prop_assert_eq!(ledger.verify().unwrap(), n as u64);
            prop_assert_eq!(ledger.len().unwrap(), n as u64);
            prop_assert!(!ledger.is_empty().unwrap());

            let all = ledger.all().unwrap();
            prop_assert_eq!(&all, &appended);
            for (i, entry) in all.iter().enumerate() {
                prop_assert_eq!(entry.seq, i as Seq + 1);
                prop_assert_eq!(entry.at, records[i].1);
                prop_assert_eq!(&entry.record, &records[i].0);
                let got = ledger.get(entry.seq).unwrap();
                prop_assert_eq!(got.as_ref(), Some(entry));
            }
            prop_assert_eq!(ledger.get(0).unwrap(), None);
            prop_assert_eq!(ledger.get(n as Seq + 1).unwrap(), None);

            prop_assert_eq!(&ledger.tail(k).unwrap()[..], &all[n.saturating_sub(k)..]);
            prop_assert_eq!(&ledger.after(k as Seq).unwrap()[..], &all[k.min(n)..]);
        }

        /// Change one byte of one row, any row, any column: verify fails.
        /// The update trigger has to go first; a tamperer with the file
        /// can drop it too, and the chain must not depend on it.
        #[test]
        fn verify_fails_after_any_byte_flip(
            records in records(),
            row in any::<Index>(),
            column in prop::sample::select(TAMPERABLE),
            position in any::<Index>(),
            replacement in any::<char>(),
            delta in prop_oneof![Just(-1i64), Just(1i64)],
            weight in finite(),
        ) {
            let (ledger, _) = filled(&records);
            let seq = row.index(records.len()) as i64 + 1;
            let conn = &ledger.conn;
            conn.execute_batch("DROP TRIGGER records_no_update").unwrap();
            match column {
                Column::At => {
                    let at: i64 = conn
                        .query_row("SELECT at FROM records WHERE seq = ?1", [seq], |r| r.get(0))
                        .unwrap();
                    conn.execute(
                        "UPDATE records SET at = ?1 WHERE seq = ?2",
                        params![at.wrapping_add(delta), seq],
                    )
                    .unwrap();
                }
                Column::Weight => {
                    let weight = if weight == 1.0 { 2.0 } else { weight };
                    conn.execute(
                        "UPDATE records SET weight = ?1 WHERE seq = ?2",
                        params![weight, seq],
                    )
                    .unwrap();
                }
                Column::Prev | Column::Hash | Column::Record => {
                    let name = match column {
                        Column::Prev => "prev",
                        Column::Hash => "hash",
                        _ => "record",
                    };
                    let text: String = conn
                        .query_row(
                            &format!("SELECT {name} FROM records WHERE seq = ?1"),
                            [seq],
                            |r| r.get(0),
                        )
                        .unwrap();
                    let mut chars: Vec<char> = text.chars().collect();
                    let i = position.index(chars.len());
                    chars[i] = if replacement != chars[i] {
                        replacement
                    } else if chars[i] != 'x' {
                        'x'
                    } else {
                        'y'
                    };
                    let text: String = chars.into_iter().collect();
                    conn.execute(
                        &format!("UPDATE records SET {name} = ?1 WHERE seq = ?2"),
                        params![text, seq],
                    )
                    .unwrap();
                }
            }
            prop_assert!(ledger.verify().is_err());
        }
    }

    #[test]
    fn an_empty_ledger_verifies() {
        let ledger = Ledger::open_in_memory().unwrap();
        assert!(ledger.is_empty().unwrap());
        assert_eq!(ledger.len().unwrap(), 0);
        assert_eq!(ledger.verify().unwrap(), 0);
        assert!(ledger.all().unwrap().is_empty());
        assert!(ledger.tail(3).unwrap().is_empty());
        assert!(ledger.after(0).unwrap().is_empty());
        assert_eq!(ledger.get(1).unwrap(), None);
    }

    #[test]
    fn deleting_a_middle_record_is_detected() {
        let mut ledger = Ledger::open_in_memory().unwrap();
        for i in 0..5 {
            ledger.append_at(report(i as f64), 100 + i).unwrap();
        }
        ledger
            .conn
            .execute_batch("DROP TRIGGER records_no_delete")
            .unwrap();
        ledger
            .conn
            .execute("DELETE FROM records WHERE seq = 3", [])
            .unwrap();
        let err = ledger.verify().unwrap_err();
        assert_eq!(err.to_string(), "ledger: record #4: expected record #3");
    }

    #[test]
    fn a_rewrite_that_still_reads_as_the_same_record_is_detected() {
        let mut ledger = Ledger::open_in_memory().unwrap();
        ledger
            .append_at(
                Record::report(Claim::Measured, "counter", "count", None, ""),
                100,
            )
            .unwrap();
        ledger
            .conn
            .execute_batch("DROP TRIGGER records_no_update")
            .unwrap();
        // With the key misspelt, serde reads `value` as missing, which for
        // an Option is None: the same record, so the hash still matches.
        let changed = ledger
            .conn
            .execute(
                "UPDATE records SET record = replace(record, '\"value\":null', '\"valve\":null')",
                [],
            )
            .unwrap();
        assert_eq!(changed, 1);
        let err = ledger.verify().unwrap_err();
        assert_eq!(
            err.to_string(),
            "ledger: record #1: stored text is not the record's canonical JSON"
        );
        assert!(matches!(ledger.get(1), Err(LedgerError::Tamper(_))));

        ledger
            .conn
            .execute("UPDATE records SET record = 'not json'", [])
            .unwrap();
        assert!(
            matches!(ledger.verify(), Err(LedgerError::Tamper(t)) if t.seq == 1 && t.what.starts_with("does not read"))
        );
    }

    #[test]
    fn triggers_refuse_update_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let mut ledger = Ledger::open(&path).unwrap();
        ledger.append_at(report(1.0), 100).unwrap();
        ledger.append_at(report(2.0), 101).unwrap();

        // The triggers live in the file, so they bind every connection.
        let other = Connection::open(&path).unwrap();
        for sql in [
            "UPDATE records SET at = 0 WHERE seq = 1",
            "UPDATE records SET record = '{}'",
            "DELETE FROM records WHERE seq = 1",
            "DELETE FROM records",
        ] {
            let err = other.execute(sql, []).unwrap_err();
            assert!(
                err.to_string().contains("the ledger is append-only"),
                "{sql}: {err}"
            );
        }
        let err = ledger
            .conn
            .execute("UPDATE records SET weight = 2 WHERE seq = 2", [])
            .unwrap_err();
        assert!(
            err.to_string().contains("the ledger is append-only"),
            "{err}"
        );

        assert_eq!(ledger.verify().unwrap(), 2);
        assert_eq!(ledger.get(1).unwrap().unwrap().at, 100);
        assert_eq!(ledger.get(2).unwrap().unwrap().weight, 1.0);
    }

    #[test]
    fn two_connections_interleave_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let mut kernel = Ledger::open(&path).unwrap();
        let mut warden = Ledger::open(&path).unwrap();
        for i in 0..10 {
            let a = kernel.append_at(report(i as f64), 100 + i).unwrap();
            let b = warden.append_at(report(-(i as f64)), 100 + i).unwrap();
            assert_eq!(b.seq, a.seq + 1);
            assert_eq!(b.prev, a.hash);
        }
        assert_eq!(kernel.verify().unwrap(), 20);
        assert_eq!(warden.verify().unwrap(), 20);
        assert_eq!(kernel.all().unwrap(), warden.all().unwrap());
        assert_eq!(warden.after(18).unwrap().len(), 2);
    }

    #[test]
    fn two_connections_append_at_the_same_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let writers: Vec<_> = (0..2)
            .map(|w| {
                let mut ledger = Ledger::open(&path).unwrap();
                std::thread::spawn(move || {
                    for i in 0..50 {
                        ledger
                            .append_at(report((w * 100 + i) as f64), i as i64)
                            .unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        let ledger = Ledger::open(&path).unwrap();
        assert_eq!(ledger.verify().unwrap(), 100);
    }

    /// avsmd's start: the kernel and the warden open a ledger that is not
    /// there yet at the same moment, and the kernel appends at once. The
    /// open that loses the race waits; it does not fail.
    #[test]
    fn two_connections_open_a_new_ledger_at_the_same_time() {
        let dir = tempfile::tempdir().unwrap();
        for round in 0..200 {
            let path = dir.path().join(format!("ledger{round}.db"));
            let gate = std::sync::Arc::new(std::sync::Barrier::new(2));
            let openers: Vec<_> = (0..2)
                .map(|w| {
                    let path = path.clone();
                    let gate = std::sync::Arc::clone(&gate);
                    std::thread::spawn(move || {
                        gate.wait();
                        let mut ledger = Ledger::open(&path)
                            .unwrap_or_else(|e| panic!("round {round}: open: {e}"));
                        if w == 0 {
                            ledger
                                .append_at(report(1.0), 1)
                                .unwrap_or_else(|e| panic!("round {round}: append: {e}"));
                        }
                    })
                })
                .collect();
            for opener in openers {
                opener.join().unwrap();
            }
        }
    }

    #[test]
    fn a_reopened_ledger_keeps_its_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let before = {
            let mut ledger = Ledger::open(&path).unwrap();
            ledger.append(report(1.0)).unwrap();
            ledger.append(report(2.0)).unwrap();
            ledger.all().unwrap()
        };
        let ledger = Ledger::open(&path).unwrap();
        assert_eq!(ledger.verify().unwrap(), 2);
        assert_eq!(ledger.all().unwrap(), before);
        // `append` stamps the current time, which is well after 2023.
        assert!(before[0].at > 1_700_000_000);
        assert!(before[0].at <= before[1].at);
    }

    /// A file-backed ledger with `n` reports in it.
    fn on_disk(dir: &Path, n: usize) -> (PathBuf, Ledger) {
        let path = dir.join("ledger.db");
        let mut ledger = Ledger::open(&path).unwrap();
        for i in 0..n {
            ledger.append(report(i as f64)).unwrap();
        }
        (path, ledger)
    }

    /// A journal that dies with the process: its files deleted while the
    /// ledger is open, then the process gone without closing (kill -9).
    /// Every append that returned is still in the file (#44).
    #[test]
    fn deleting_the_journal_files_and_dying_loses_no_append() {
        let dir = tempfile::tempdir().unwrap();
        let (path, mut ledger) = on_disk(dir.path(), 5);
        let journals = || {
            for suffix in ["-wal", "-shm", "-journal"] {
                let mut p = path.as_os_str().to_owned();
                p.push(suffix);
                let _ = std::fs::remove_file(PathBuf::from(p));
            }
        };
        journals();
        for i in 0..5 {
            ledger.append(report(f64::from(i))).unwrap();
        }
        journals();
        std::mem::forget(ledger);
        let ledger = Ledger::open(&path).unwrap();
        assert_eq!(ledger.verify().unwrap(), 10);
    }

    #[test]
    fn a_ledger_file_keeps_a_rollback_journal_synced_in_full() {
        let dir = tempfile::tempdir().unwrap();
        let (_, ledger) = on_disk(dir.path(), 1);
        let mode: String = ledger
            .conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, JOURNAL);
        let sync: i64 = ledger
            .conn
            .query_row("PRAGMA synchronous", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sync, 2, "FULL");
    }

    #[test]
    fn an_append_fails_closed_when_the_file_is_replaced_or_rewound() {
        let dir = tempfile::tempdir().unwrap();
        let (path, mut ledger) = on_disk(dir.path(), 3);
        let copy = dir.path().join("copy.db");
        ledger.copy_to(&copy).unwrap();
        ledger.append(report(9.0)).unwrap();

        // Rewound in place: the older copy's rows written over the file.
        {
            let other = Connection::open(&path).unwrap();
            other
                .execute_batch("DROP TRIGGER records_no_delete")
                .unwrap();
            other
                .execute("DELETE FROM records WHERE seq = 4", [])
                .unwrap();
            other.execute_batch(SCHEMA).unwrap();
            let e = Entry::new(4, 1, &ledger.get(3).unwrap().unwrap().hash, report(7.0));
            other
                .execute(
                    "INSERT INTO records VALUES (4, ?1, ?2, ?3, 1, ?4)",
                    params![e.at, e.prev, e.hash, e.record.canonical()],
                )
                .unwrap();
        }
        let e = ledger.append(report(1.0)).unwrap_err().to_string();
        assert!(e.contains("record #4") && e.contains("rewound"), "{e}");
        assert_eq!(Ledger::open(&path).unwrap().len().unwrap(), 4);

        // Replaced: another file moved over the path.
        let (path2, mut ledger2) = on_disk(&dir.path().join("b").tap_mkdir(), 2);
        std::fs::copy(&copy, dir.path().join("swap.db")).unwrap();
        std::fs::rename(dir.path().join("swap.db"), &path2).unwrap();
        let e = ledger2.append(report(1.0)).unwrap_err().to_string();
        assert!(e.contains("replaced"), "{e}");
        // Removed.
        std::fs::remove_file(&path2).unwrap();
        let e = ledger2.append(report(1.0)).unwrap_err().to_string();
        assert!(e.contains("gone"), "{e}");
    }

    trait TapMkdir {
        fn tap_mkdir(self) -> PathBuf;
    }

    impl TapMkdir for PathBuf {
        fn tap_mkdir(self) -> PathBuf {
            std::fs::create_dir_all(&self).unwrap();
            self
        }
    }

    #[test]
    fn the_insert_trigger_takes_only_the_next_chained_row() {
        let mut ledger = Ledger::open_in_memory().unwrap();
        let insert = |l: &Ledger, seq: i64, prev: &str| {
            l.conn.execute(
                "INSERT INTO records (seq, at, prev, hash, weight, record) \
                 VALUES (?1, 0, ?2, ?3, 1, '{}')",
                params![seq, prev, format!("h{seq}{prev}")],
            )
        };
        // Empty: only #1 on the genesis hash.
        assert!(insert(&ledger, 2, GENESIS).is_err());
        assert!(insert(&ledger, 1, "beef").is_err());
        let first = ledger.append(report(1.0)).unwrap();
        for (seq, prev) in [
            (1, first.hash.as_str()),
            (3, first.hash.as_str()),
            (2, GENESIS),
        ] {
            let e = insert(&ledger, seq, prev).unwrap_err().to_string();
            assert!(e.contains("chained to the last"), "{seq}: {e}");
        }
        insert(&ledger, 2, &first.hash).unwrap();
        assert_eq!(ledger.first_bad().unwrap().unwrap().seq, 2);
    }

    /// The schema before #44: the two triggers, no INSERT trigger, WAL.
    const OLD_SCHEMA: &str = "
        PRAGMA journal_mode = WAL;
        CREATE TABLE records (
            seq INTEGER PRIMARY KEY, at INTEGER NOT NULL, prev TEXT NOT NULL,
            hash TEXT NOT NULL UNIQUE, weight REAL NOT NULL, record TEXT NOT NULL);
        CREATE TRIGGER records_no_update BEFORE UPDATE ON records
        BEGIN SELECT RAISE(ABORT, 'the ledger is append-only'); END;
        CREATE TRIGGER records_no_delete BEFORE DELETE ON records
        BEGIN SELECT RAISE(ABORT, 'the ledger is append-only'); END;";

    #[test]
    fn an_older_ledger_gains_the_insert_trigger_and_keeps_every_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let rows = |conn: &Connection| -> Vec<Stored> {
            let mut stmt = conn
                .prepare(&format!("SELECT {COLUMNS} FROM records ORDER BY seq"))
                .unwrap();
            let rows = stmt.query_map([], read_row).unwrap();
            rows.map(Result::unwrap).collect()
        };
        let before = {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(OLD_SCHEMA).unwrap();
            let mut prev = GENESIS.to_string();
            for i in 1..=4u64 {
                let e = Entry::new(i, 100, &prev, report(i as f64));
                conn.execute(
                    "INSERT INTO records VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        i as i64,
                        e.at,
                        e.prev,
                        e.hash,
                        e.weight,
                        e.record.canonical()
                    ],
                )
                .unwrap();
                prev = e.hash;
            }
            rows(&conn)
        };
        let ledger = Ledger::open(&path).unwrap();
        assert_eq!(rows(&ledger.conn), before, "no row rewritten");
        assert_eq!(ledger.verify().unwrap(), 4);
        let e = ledger
            .conn
            .execute("INSERT INTO records VALUES (9, 0, 'x', 'y', 1, '{}')", [])
            .unwrap_err()
            .to_string();
        assert!(e.contains("chained"), "{e}");
    }

    #[test]
    fn first_bad_names_the_first_row_that_does_not_read_or_chain() {
        let dir = tempfile::tempdir().unwrap();
        let (_, ledger) = on_disk(dir.path(), 6);
        assert_eq!(ledger.first_bad().unwrap(), None);
        ledger
            .conn
            .execute_batch(
                "DROP TRIGGER records_no_update;
                 UPDATE records SET record = '{\"kind\":\"action\"}' WHERE seq = 5;
                 UPDATE records SET at = at + 1 WHERE seq = 3;",
            )
            .unwrap();
        let bad = ledger.first_bad().unwrap().unwrap();
        assert_eq!(bad.seq, 3, "{bad}");
        ledger
            .conn
            .execute("UPDATE records SET at = at - 1 WHERE seq = 3", [])
            .unwrap();
        let bad = ledger.first_bad().unwrap().unwrap();
        assert_eq!(bad.seq, 5, "{bad}");
        assert!(bad.what.starts_with("does not read"), "{bad}");
        // A plain read names it too.
        let e = ledger.all().unwrap_err().to_string();
        assert!(e.contains("record #5"), "{e}");
    }

    #[test]
    fn holds_finds_an_anchor_by_seq_and_hash_or_hash_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (_, ledger) = on_disk(dir.path(), 4);
        let e = ledger.get(3).unwrap().unwrap();
        assert_eq!(ledger.holds(Some(3), &e.hash[..16]).unwrap(), Some(3));
        assert_eq!(ledger.holds(None, &e.hash).unwrap(), Some(3));
        assert_eq!(
            ledger.holds(None, &e.hash[..16].to_uppercase()).unwrap(),
            Some(3)
        );
        assert_eq!(ledger.holds(Some(2), &e.hash[..16]).unwrap(), None);
        assert_eq!(ledger.holds(Some(9), &e.hash[..16]).unwrap(), None);
        assert_eq!(ledger.holds(None, "0123456789abcdef").unwrap(), None);
        assert_eq!(ledger.holds(None, "'; --").unwrap(), None);
    }

    #[test]
    fn a_cut_keeps_what_is_before_backs_up_the_rest_and_restores_the_triggers() {
        let dir = tempfile::tempdir().unwrap();
        let (path, mut ledger) = on_disk(dir.path(), 6);
        let kept = ledger.tail(6).unwrap()[..3].to_vec();
        let backup = dir.path().join("backup.db");
        assert!(ledger.cut(0, &backup).is_err());
        assert!(ledger.cut(7, &backup).is_err());
        assert!(!backup.exists());
        assert_eq!(ledger.cut(4, &backup).unwrap(), 3);
        assert_eq!(ledger.all().unwrap(), kept);
        assert_eq!(Ledger::open(&backup).unwrap().verify().unwrap(), 6);
        // The triggers are back, and the ledger appends on as before.
        assert!(ledger.conn.execute("DELETE FROM records", []).is_err());
        assert!(ledger
            .conn
            .execute("UPDATE records SET at = 0", [])
            .is_err());
        let e = ledger.append(report(1.0)).unwrap();
        assert_eq!(e.seq, 4);
        assert_eq!(Ledger::open(&path).unwrap().verify().unwrap(), 4);
        // A bad row before the cut point refuses the cut.
        ledger
            .conn
            .execute_batch(
                "DROP TRIGGER records_no_update; UPDATE records SET at = 1 WHERE seq = 2;",
            )
            .unwrap();
        let e = ledger
            .cut(3, &dir.path().join("b2.db"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("record #2"), "{e}");
    }
}
