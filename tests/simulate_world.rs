//! A simulation never runs a real lever: every run goes to the simulation
//! world, or to the venture's world model, and the ledger shows the same
//! intent and outcome a real run would leave.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use avsmd::kernel::Kernel;
use avsmd::record::{Claim, Kind, Record};
use avsmd::simulate::{run, Human, Plan};

/// The virtual clock belongs to the whole process: one test at a time.
static CLOCK: Mutex<()> = Mutex::new(());

const START: i64 = 1_767_225_600; // 2026-01-01T00:00:00Z

fn clock() -> MutexGuard<'static, ()> {
    std::env::set_var("TZ", "UTC");
    CLOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn script(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A home, and a venture `low` whose metric reads 3 from a file, under a
/// threshold that calls its lever `touch` below 5. The lever, were it to
/// run, would leave a file named LEVER_RAN.
fn setup(extra: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.yaml"), "nightly: never\n").unwrap();
    let dir = tmp.path().join("low");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mandate.yaml"),
        format!(
            "purpose: Keep it up.\nmetric: count\n\
             controller: {{kind: threshold, below: 5, action: touch}}\ncadence: 1h\n\
             permitted:\n- {{name: touch, run: touch LEVER_RAN, cap: 1/day, reversible: true}}\n{extra}"
        ),
    )
    .unwrap();
    std::fs::write(dir.join("value"), "3\n").unwrap();
    script(&dir, "report", "cat value");
    (tmp, home, dir)
}

fn plan(dir: &Path, world: Option<&str>) -> Plan {
    Plan {
        days: 1,
        step: 3600,
        human: Human::Away,
        start: Some(START),
        add: vec![dir.to_path_buf()],
        world: world.map(str::to_string),
    }
}

#[test]
fn a_script_lever_goes_to_the_world_and_never_runs() {
    let _c = clock();
    let (_tmp, home, dir) = setup("");
    let world =
        "[ -n \"$AVSM_LEVER\" ] || { echo count=3; exit 0; }; echo \"$AVSM_LEVER $AVSM_KEY\" >> world.log; echo moved";
    let mut out = Vec::new();
    run(&home, &plan(&dir, Some(world)), &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(!dir.join("LEVER_RAN").exists(), "{text}");
    let log = std::fs::read_to_string(dir.join("world.log")).unwrap();
    let k = Kernel::open(&home).unwrap();
    let entries = k.entries().unwrap();
    let intents: Vec<_> = entries
        .iter()
        .filter(|e| e.record.kind() == Kind::Intent)
        .collect();
    assert_eq!(intents.len(), 1, "{text}");
    let Record::Intent { key: Some(key), .. } = &intents[0].record else {
        panic!("{intents:?}");
    };
    assert_eq!(log, format!("low/touch {key}\n"));
    assert!(
        entries.iter().any(|e| matches!(&e.record,
            Record::Outcome { intent, ok: true, detail, claim: Claim::Measured, .. }
                if *intent == intents[0].seq && detail == "simulated: world  moved")),
        "{text}"
    );
}

#[test]
fn without_a_world_the_lever_steps_the_world_model_and_reports_follow_it() {
    let _c = clock();
    let (_tmp, home, dir) = setup("");
    history(&home, &dir, &[3.0, 3.0, 3.0]);
    let mut out = Vec::new();
    run(&home, &plan(&dir, None), &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(!dir.join("LEVER_RAN").exists(), "{text}");
    let k = Kernel::open(&home).unwrap();
    let entries = k.entries().unwrap();
    let intent = entries
        .iter()
        .find(|e| e.record.kind() == Kind::Intent)
        .unwrap_or_else(|| panic!("{text}"));
    assert!(
        entries.iter().any(|e| matches!(&e.record,
            Record::Outcome { intent: i, ok: true, detail, .. }
                if *i == intent.seq && detail.starts_with("simulated: baseline"))),
        "{text}"
    );
    // The baseline starts from the ledger's last reading; every reading
    // comes from the model's state, none from the report program.
    let notes: Vec<&str> = entries
        .iter()
        .filter(|e| e.at >= START)
        .filter_map(|e| match &e.record {
            Record::Report { metric, note, .. } if metric == "count" => Some(note.as_str()),
            _ => None,
        })
        .collect();
    assert!(notes.len() > 20, "{text}");
    assert!(notes.iter().all(|n| *n == "simulated: baseline"), "{text}");
}

/// Take the venture on before the simulation starts, with these readings
/// of its metric in the ledger, an hour apart, ending an hour before
/// START.
fn history(home: &Path, dir: &Path, values: &[f64]) {
    avsmd::clock::set(START - (values.len() as i64 + 1) * 3600);
    let mut k = Kernel::open(home).unwrap();
    k.add(dir).unwrap();
    for v in values {
        avsmd::clock::advance(3600);
        k.append(Record::report(
            Claim::Measured,
            "low",
            "count",
            Some(*v),
            "",
        ))
        .unwrap();
    }
}

/// A venture whose every program, were it to run, leaves a trace: the
/// report counts its runs in `count`, as examples/counter's does, and the
/// guard, the audit and the reading each touch a file.
fn tracing() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let (tmp, home, dir) = setup(
        "guards:\n- {name: spend, run: ./spend, above: 50, then: ask}\n\
         audits:\n- {name: refunds, run: ./refunds, worse: up}\n\
         readings:\n- {name: waiting, run: ./waiting, lines: address}\n",
    );
    std::fs::write(dir.join("count"), "500\n").unwrap();
    script(
        &dir,
        "report",
        "n=$(cat count); echo $((n + 1)) > count; echo 3",
    );
    for name in ["spend", "refunds", "waiting"] {
        let mark = name.to_uppercase();
        script(&dir, name, &format!("touch {mark}_RAN; echo 1"));
    }
    (tmp, home, dir)
}

/// Every program the venture holds is as it was: none of it ran.
fn untouched(dir: &Path, text: &str) {
    assert_eq!(
        std::fs::read_to_string(dir.join("count")).unwrap(),
        "500\n",
        "the report ran: {text}"
    );
    for name in ["LEVER_RAN", "SPEND_RAN", "REFUNDS_RAN", "WAITING_RAN"] {
        assert!(!dir.join(name).exists(), "{name}: {text}");
    }
}

#[test]
fn a_simulation_runs_none_of_the_ventures_programs() {
    let _c = clock();
    let (_tmp, home, dir) = tracing();
    let mut out = Vec::new();
    run(&home, &plan(&dir, None), &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    untouched(&dir, &text);
    // With nothing in the ledger to start from, the baseline says so on
    // every reading, tagged as the simulation's.
    let entries = Kernel::open(&home).unwrap().entries().unwrap();
    let reports: Vec<&Record> = entries
        .iter()
        .map(|e| &e.record)
        .filter(|r| r.venture() == "low" && r.kind() == Kind::Report)
        .collect();
    assert!(reports.len() > 20, "{text}");
    for r in reports {
        let Record::Report {
            note,
            value,
            metric,
            ..
        } = r
        else {
            unreachable!()
        };
        assert!(note.starts_with("simulated:"), "{metric} {note}: {text}");
        assert_eq!(*value, None, "{metric}: {text}");
    }
}

#[test]
fn a_simulation_starts_from_the_ledgers_last_reading() {
    let _c = clock();
    let (_tmp, home, dir) = tracing();
    history(&home, &dir, &[7.0, 6.0, 7.0, 6.0]);
    let mut out = Vec::new();
    run(&home, &plan(&dir, None), &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    untouched(&dir, &text);
    let entries = Kernel::open(&home).unwrap().entries().unwrap();
    let first = entries
        .iter()
        .filter(|e| e.at >= START)
        .find_map(|e| match &e.record {
            Record::Report {
                metric,
                value,
                note,
                ..
            } if metric == "count" => Some((*value, note.clone())),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{text}"));
    assert_eq!(first.1, "simulated: baseline", "{text}");
    let x = first.0.unwrap_or_else(|| panic!("{text}"));
    assert!((5.0..=8.0).contains(&x), "{x}: {text}");
}

#[test]
fn simulate_refuses_a_world_that_is_a_ventures_world_model() {
    let _c = clock();
    let (_tmp, home, dir) = setup("world: ./model\n");
    let err = run(&home, &plan(&dir, Some("model")), &mut Vec::new()).unwrap_err();
    assert!(err.contains("world model"), "{err}");
    assert!(Kernel::open(&home).unwrap().entries().unwrap().is_empty());
    // Once taken on, it is refused just the same.
    Kernel::open(&home).unwrap().add(&dir).unwrap();
    let err = run(&home, &plan(&dir, Some("./model")), &mut Vec::new()).unwrap_err();
    assert!(err.contains("low's world model"), "{err}");
}
