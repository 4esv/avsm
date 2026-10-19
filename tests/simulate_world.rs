//! A simulation never runs a real lever: every run goes to the simulation
//! world, or to the venture's world model, and the ledger shows the same
//! intent and outcome a real run would leave.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use avsm::kernel::Kernel;
use avsm::record::{Claim, Kind, Record};
use avsm::simulate::{run, Human, Plan};

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
        "[ -n \"$AVSM_LEVER\" ] || exit 0; echo \"$AVSM_LEVER $AVSM_KEY\" >> world.log; echo moved";
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
    // The first reading is real and seeds the baseline; the rest come
    // from the model's state.
    let notes: Vec<&str> = entries
        .iter()
        .filter_map(|e| match &e.record {
            Record::Report { metric, note, .. } if metric == "count" => Some(note.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(notes[0], "", "{text}");
    assert!(notes.len() > 20, "{text}");
    assert!(
        notes[1..].iter().all(|n| *n == "simulated: baseline"),
        "{text}"
    );
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
