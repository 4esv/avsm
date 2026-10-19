//! A ledger written by the 1.0 binary still opens, verifies and answers.
//! Its adoptions carry no mandate text, and the ventures' directories no
//! longer exist; that is reported, never a crash.

use std::path::{Path, PathBuf};

use avsm::kernel::Kernel;
use avsm::ledger::Ledger;
use avsm::record::{Record, Verdict};

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
