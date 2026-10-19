//! The ledger: an append-only, hash-chained store of records.
//!
//! One SQLite file holds one table, `records`. Each row is an [`Entry`]:
//! numbered from 1, timed, chained to the hash of the row before it, and
//! hashed itself. Two triggers refuse every `UPDATE` and `DELETE`, so the
//! only way to change the table is to append to it.
//!
//! Two processes, the kernel and the warden, append to the same file. The
//! file is in WAL mode so one can read while the other writes; every append
//! is one `BEGIN IMMEDIATE` transaction so two appends can never chain to
//! the same previous record; and a connection waits up to five seconds for
//! the other process to finish before giving up.
//!
//! [`Ledger::verify`] reads every row back and recomputes the chain, so a
//! change to any stored byte, or a row missing from the middle, is found.
//! Rows cut off the end are not: the ledger alone cannot know how long it
//! should be.

use std::path::Path;
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension, Params, TransactionBehavior};

use crate::record::{verify_chain, Entry, Record, Seq, Tamper, GENESIS};

/// An open ledger.
pub struct Ledger {
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
";

/// The columns of a row, in the order [`read_row`] expects them.
const COLUMNS: &str = "seq, at, prev, hash, weight, record";

/// How long to wait for the other process to finish its append.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

impl Ledger {
    /// Open or create a ledger file. The directory must already exist.
    pub fn open(path: &Path) -> Result<Ledger, LedgerError> {
        Ledger::prepare(Connection::open(path)?)
    }

    /// A ledger that lives only in memory, for tests.
    pub fn open_in_memory() -> Result<Ledger, LedgerError> {
        Ledger::prepare(Connection::open_in_memory()?)
    }

    /// Set the connection up and make sure the table and triggers exist.
    fn prepare(conn: Connection) -> Result<Ledger, LedgerError> {
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // An in-memory database has no file to journal; it stays in "memory"
        // mode, and SQLite reports that instead of failing.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Ledger { conn })
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
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let last: Option<(i64, String)> = tx
            .query_row(
                "SELECT seq, hash FROM records ORDER BY seq DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
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
        tx.commit()?;
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

    /// Run a query that selects [`COLUMNS`] and decode every row it returns.
    fn select<P: Params>(&self, sql: &str, params: P) -> Result<Vec<Entry>, LedgerError> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params, read_row)?;
        rows.map(|row| to_entry(row?)).collect()
    }
}

/// Unix seconds now. Before 1970 the clock reads negative, as it should.
fn now() -> i64 {
    crate::clock::now()
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
    let record: Record = serde_json::from_str(&text)?;
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

    /// Any record: all eight kinds, all three claims, every optional field
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
                prop::option::of(any::<Seq>())
            )
                .prop_map(|(claim, venture, action, reason, detail, count, closes)| {
                    Record::Refusal {
                        claim,
                        venture,
                        action,
                        reason,
                        detail,
                        count,
                        closes,
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
        assert!(matches!(ledger.verify(), Err(LedgerError::Encoding(_))));
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
}
