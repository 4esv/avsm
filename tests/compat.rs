//! A ledger written by the 1.0 binary still opens, verifies and answers.
//! Its adoptions carry no mandate text, and the ventures' directories no
//! longer exist; that is reported, never a crash.

use std::path::{Path, PathBuf};

use avsmd::kernel::Kernel;
use avsmd::ledger::Ledger;
use avsmd::record::{Entry, Kind, Record, Verdict};

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
    // It is not running: the first line cautions, and its row says so.
    assert!(
        status.starts_with("caution: 1 venture cannot run: vanished\n"),
        "{status}"
    );
    let row = status.lines().find(|l| l.starts_with("vanished")).unwrap();
    assert!(row.contains("cannot run"), "{row}");
    assert!(!row.contains("running"), "{row}");
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

// ----- issue #53: records dated in the future -----

/// When Power 2 upgraded the 1.0 ledger: 2026-10-23T06:12Z. Its records
/// run to 2026-10-29: 22 of tomato's waterings are dated after this.
const UPGRADED: i64 = 1_792_735_920;
const DAY: i64 = 86_400;

/// The waterings of tomato the ledger dates in (from, to].
fn waterings(home: &Path, from: i64, to: i64) -> usize {
    Ledger::open_read_only(&home.join("ledger.db"))
        .unwrap()
        .all()
        .unwrap()
        .iter()
        .filter(|e| e.at > from && e.at <= to)
        .filter(|e| matches!(&e.record, Record::Action { venture, action, .. } if venture == "tomato" && action == "water"))
        .count()
}

#[test]
fn records_dated_in_the_future_never_fill_a_cap_window() {
    let _clock = avsmd::clock::scoped(UPGRADED);
    let tmp = tempfile::tempdir().unwrap();
    let home = open_copy(tmp.path());
    assert_eq!(waterings(&home, UPGRADED - DAY, UPGRADED), 3);
    assert_eq!(waterings(&home, UPGRADED, i64::MAX), 22);
    let mut k = Kernel::with(
        &home,
        Box::new(avsmd::role::ModelRole),
        Box::new(avsmd::warden::LocalWarden::open(&home).unwrap()),
    )
    .unwrap();
    let index = k.index().unwrap();
    assert_eq!(index.used("tomato", "water", UPGRADED - DAY, UPGRADED), 3);
    drop(index);
    // The tomato adopted again, dry: it waters, on a cap of 4 a day with
    // 3 used today.
    let dir = tmp.path().join("tomato");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mandate.yaml"),
        "purpose: Keep the tomato alive.\nmetric: moisture\n\
         controller: {kind: threshold, below: 35, action: water}\ncadence: 2h\n\
         permitted:\n  - {name: water, run: echo watered, cap: 4/day, reversible: true}\n",
    )
    .unwrap();
    std::fs::write(dir.join("report"), "#!/bin/sh\necho 20\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir.join("report"), std::fs::Permissions::from_mode(0o755)).unwrap();
    k.add(&dir).unwrap();
    k.report("tomato", false);
    let entries = k.entries().unwrap();
    let ran = entries.iter().rev().find_map(|e| match &e.record {
        Record::Intent {
            venture,
            action,
            proof,
            ..
        } if venture == "tomato" && action == "water" && e.at == UPGRADED => Some(proof.clone()),
        _ => None,
    });
    let refused: Vec<String> = entries
        .iter()
        .filter(|e| e.at == UPGRADED)
        .filter_map(|e| match &e.record {
            Record::Refusal { detail, .. } => Some(detail.clone()),
            _ => None,
        })
        .collect();
    let proof = ran.unwrap_or_else(|| panic!("tomato was refused: {refused:?}"));
    assert!(
        proof
            .steps
            .iter()
            .any(|s| matches!(s, avsmd::gate::Step::Cap { used: 3, .. })),
        "{proof:?}"
    );
}

#[test]
fn doctor_tail_know_and_the_digest_say_a_record_is_dated_in_the_future() {
    let _clock = avsmd::clock::scoped(UPGRADED);
    let tmp = tempfile::tempdir().unwrap();
    let home = open_copy(tmp.path());
    let found = avsmd::doctor::doctor(&home, None, None);
    let note = found
        .out
        .lines()
        .find(|l| l.contains("dated in the future"))
        .unwrap_or_else(|| panic!("{}", found.out));
    let future: Vec<Entry> = Ledger::open_read_only(&home.join("ledger.db"))
        .unwrap()
        .all()
        .unwrap()
        .into_iter()
        .filter(|e| e.at > UPGRADED)
        .collect();
    assert!(
        note.contains(&format!("{} records", future.len())),
        "{note}"
    );
    assert!(note.contains(&format!("#{}", future[0].seq)), "{note}");
    assert!(note.contains("2026-10-29"), "{note}");
    assert!(
        !found.problems.iter().any(|p| p.contains("future")),
        "a note, not a failure: {:?}",
        found.problems
    );

    let k = Kernel::open(&home).unwrap();
    // The last record is dated ahead: tail marks it.
    let tail = k.tail(3).unwrap();
    assert!(
        tail.lines()
            .all(|l| l.ends_with("d ahead]") && l.contains("[dated ")),
        "{tail}"
    );
    let know = k.know("tomato").unwrap();
    assert!(!know.contains("0s ago"), "{know}");
    assert!(know.contains("ahead"), "{know}");
    // The digest's window ends now, and starts in the past.
    let digest = k.digest().unwrap();
    let since = digest
        .split("digest since ")
        .nth(1)
        .and_then(|s| s.split(':').next())
        .unwrap_or_default()
        .to_string();
    assert!(since.as_str() <= "2026-10-23T06", "{digest}");
    assert!(digest.contains("dated in the future"), "{digest}");
}

#[test]
fn the_digest_counts_only_what_is_dated_up_to_now() {
    let tmp = tempfile::tempdir().unwrap();
    let home = open_copy(tmp.path());
    let all = Ledger::open_read_only(&home.join("ledger.db"))
        .unwrap()
        .all()
        .unwrap();
    let mut ahead_asks = 0;
    let mut ahead_runs = 0;
    for now in [UPGRADED, UPGRADED - 2 * DAY, UPGRADED - 4 * DAY] {
        let _clock = avsmd::clock::scoped(now);
        let kind = |e: &&Entry, k: &[Kind]| k.contains(&e.record.kind());
        // Since the last digest of the week, else the last day.
        let digest_seq = all
            .iter()
            .rev()
            .filter(|e| e.at <= now)
            .find(|e| matches!(&e.record, Record::Report { venture, metric, .. } if venture == "self" && metric == "digest"))
            .filter(|e| e.at > now - 7 * DAY)
            .map(|e| e.seq);
        let day: Vec<&Entry> = all
            .iter()
            .filter(|e| e.at <= now)
            .filter(|e| match digest_seq {
                Some(d) => e.seq > d,
                None => e.at > now - DAY,
            })
            .collect();
        let runs = day
            .iter()
            .filter(|e| kind(e, &[Kind::Action, Kind::Intent]))
            .count();
        let asks = day.iter().filter(|e| kind(e, &[Kind::Ask])).count();
        let week = || all.iter().filter(|e| e.at > now - 7 * DAY && e.at <= now);
        let weekly_asks = week().filter(|e| kind(e, &[Kind::Ask])).count();
        let weekly_incidents = week()
            .filter(|e| {
                matches!(
                    &e.record,
                    Record::Action { ok: false, .. }
                        | Record::Outcome { ok: false, .. }
                        | Record::Escalation { .. }
                )
            })
            .count();
        ahead_asks += all
            .iter()
            .filter(|e| e.at > now && e.at <= now + 7 * DAY && kind(e, &[Kind::Ask]))
            .count();
        ahead_runs += all
            .iter()
            .filter(|e| e.at > now && e.at <= now + DAY && kind(e, &[Kind::Action, Kind::Intent]))
            .count();
        let k = Kernel::open(&home).unwrap();
        let digest = k.digest().unwrap();
        assert!(
            digest.contains(&format!(" {runs} actions, {asks} asks,")),
            "{runs} actions, {asks} asks at {now}: {digest}"
        );
        // Incidents of stops and a broken chain are counted too; those the
        // plain counts above leave out are no more than what the week holds.
        let row = digest
            .lines()
            .find(|l| l.contains("/week"))
            .unwrap_or_else(|| panic!("{digest}"));
        assert!(
            row.contains(&format!("asks {weekly_asks}/week")),
            "{weekly_asks} at {now}: {row}"
        );
        let incidents: usize = row
            .split("incidents ")
            .nth(1)
            .and_then(|s| s.split('/').next())
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("{row}"));
        assert!(incidents >= weekly_incidents, "{row}");
    }
    assert!(ahead_asks > 0 && ahead_runs > 0, "the ledger reaches ahead");
}
