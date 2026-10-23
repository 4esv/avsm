//! A ledger written by the 1.0 binary still opens, verifies and answers.
//! Its adoptions carry no mandate text, and the ventures' directories no
//! longer exist; that is reported, never a crash.

use std::path::{Path, PathBuf};

use avsmd::kernel::Kernel;
use avsmd::ledger::Ledger;
use avsmd::record::{Entry, Record, Verdict};

fn open_copy(tmp: &Path) -> PathBuf {
    let home = tmp.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ledger-1.0.db"),
        home.join("ledger.db"),
    )
    .unwrap();
    home
}

#[test]
fn the_1_0_chain_still_verifies() {
    let tmp = tempfile::tempdir().unwrap();
    let home = open_copy(tmp.path());
    let ledger = Ledger::open(&home.join("ledger.db")).unwrap();
    let n = ledger.verify().expect("the 1.0 chain verifies under 2.0");
    assert!(n >= 645, "{n}");
    let adopts: Vec<_> = ledger
        .all()
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.record {
            Record::Decision {
                verdict: Verdict::Adopt { text, .. },
                ..
            } => Some(text),
            _ => None,
        })
        .collect();
    assert!(adopts.len() >= 5);
    assert!(adopts.iter().all(Option::is_none), "1.0 stored no text");
}

/// Every row of a ledger file as stored, read with plain SQLite.
fn raw_rows(db: &Path) -> Vec<(i64, i64, String, String, f64, String)> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare("SELECT seq, at, prev, hash, weight, record FROM records ORDER BY seq")
        .unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })
        .unwrap();
    rows.map(Result::unwrap).collect()
}

#[test]
fn the_1_0_ledger_gains_the_chain_trigger_on_open_and_keeps_every_row() {
    let tmp = tempfile::tempdir().unwrap();
    let home = open_copy(tmp.path());
    let db = home.join("ledger.db");
    let before = raw_rows(&db);
    let ledger = Ledger::open(&db).unwrap();
    assert!(ledger.verify().unwrap() >= 645);
    drop(ledger);
    assert_eq!(raw_rows(&db), before, "no row rewritten");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let e = conn
        .execute(
            "INSERT INTO records (seq, at, prev, hash, weight, record) \
             VALUES (99999, 0, 'x', 'y', 1, '{}')",
            [],
        )
        .unwrap_err()
        .to_string();
    assert!(e.contains("chained to the last"), "{e}");
}

#[test]
fn the_commands_answer_on_a_1_0_ledger() {
    let tmp = tempfile::tempdir().unwrap();
    let home = open_copy(tmp.path());
    let k = Kernel::open(&home).unwrap();

    let status = k.status().unwrap();
    assert!(status.contains("garden"), "{status}");
    // Where a venture's directory is gone the status says so; where it is
    // still there (this may be the machine that wrote the ledger) the 1.0
    // adoption reads from it as before.
    for line in status.lines().skip(1).filter(|l| !l.starts_with("self")) {
        assert!(
            line.contains("cannot read") || line.contains("reported"),
            "{line}"
        );
    }
    let tail = k.tail(5).unwrap();
    assert_eq!(tail.lines().count(), 5, "{tail}");
    k.asks().unwrap();

    // An action of 1.0: the version is named from the ledger alone.
    let why = k.why(16).unwrap();
    assert!(why.contains("garden mandate v1 (#2, "), "{why}");
    // An adoption has no mandate step of its own, and why still answers.
    k.why(2).unwrap();
}

#[test]
fn a_1_0_adoption_whose_directory_is_gone_is_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut ledger = Ledger::open(&home.join("ledger.db")).unwrap();
    let old = r#"{"kind":"decision","claim":"declared","venture":"vanished","verdict":{"verdict":"adopt","mandate":"abc","path":"/nowhere/vanished"}}"#;
    ledger.append(serde_json::from_str(old).unwrap()).unwrap();
    drop(ledger);
    let k = Kernel::open(&home).unwrap();
    let status = k.status().unwrap();
    assert!(status.contains("cannot read /nowhere/vanished"), "{status}");
    assert!(k.adopted().is_empty(), "nothing is permitted to it");
}

/// The measured reports that say an approved lever did not run, citing
/// `decision`.
fn did_not_run(k: &Kernel, decision: u64) -> Vec<u64> {
    k.entries()
        .unwrap()
        .into_iter()
        .filter(|e| {
            matches!(&e.record, Record::Report { value: None, note, cites, .. }
                if note.starts_with("did not run") && cites.contains(&decision))
        })
        .map(|e| e.seq)
        .collect()
}

#[test]
fn a_1_0_approval_that_already_did_not_run_is_not_run_again_on_start() {
    // #356 approves trader's delegate; #359 is its outcome, written by 1.0:
    // it did not run. Two starts of 2.0 write nothing more for it.
    let tmp = tempfile::tempdir().unwrap();
    let home = open_copy(tmp.path());
    {
        let k = Kernel::open(&home).unwrap();
        assert_eq!(did_not_run(&k, 356), vec![359]);
    }
    for start in 1..=2 {
        let mut k = Kernel::open(&home).unwrap();
        let from = k.entries().unwrap().len();
        k.tick();
        assert_eq!(did_not_run(&k, 356), vec![359], "start {start}");
        if start == 2 {
            // Whatever the first start tried and failed, the second does
            // not try again.
            let again: Vec<_> = k.entries().unwrap()[from..]
                .iter()
                .filter(|e| {
                    matches!(&e.record, Record::Report { note, .. } if note.starts_with("did not run"))
                })
                .map(Entry::line)
                .collect();
            assert!(again.is_empty(), "{again:?}");
        }
    }
}
