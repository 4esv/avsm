//! The greenhouse across a simulated day: the temperature follows the
//! clock, and the vent lowers it. One test in this file: the virtual clock
//! belongs to the whole process.

use std::path::Path;

use avsm::record::Record;
use avsm::simulate::{run, Human, Plan};

/// The world the simulation runs the vent in: it opens the vent as the
/// real lever would, writing the time it opened for the report to read.
const WORLD: &str = "[ \"$AVSM_LEVER\" = greenhouse/vent ] || exit 0; \
     echo \"$AVSM_NOW\" > vented; echo \"vent opened at $AVSM_NOW\"";

fn copy_example(to: &Path) {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/greenhouse");
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
    }
}

#[test]
fn the_temperature_follows_the_clock_and_the_vent_lowers_it() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.yaml"), "nightly: never\n").unwrap();
    let dir = tmp.path().join("greenhouse");
    copy_example(&dir);
    let start = 1_767_225_600; // 2026-01-01T00:00:00Z
    let plan = Plan {
        days: 1,
        step: 3600,
        human: Human::ApproveAll,
        start: Some(start),
        add: vec![dir.clone()],
        world: Some(WORLD.to_string()),
    };
    let mut out = Vec::new();
    run(&home, &plan, &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();

    let k = avsm::kernel::Kernel::open(&home).unwrap();
    let entries = k.entries().unwrap();
    let temps: Vec<(i64, f64)> = entries
        .iter()
        .filter_map(|e| match &e.record {
            Record::Report {
                venture,
                metric,
                value: Some(v),
                ..
            } if venture == "greenhouse" && metric == "temperature" => Some((e.at - start, *v)),
            _ => None,
        })
        .collect();
    let at = |hour: i64| {
        temps
            .iter()
            .find(|(t, _)| *t >= hour * 3600)
            .map(|(_, v)| *v)
            .unwrap_or_else(|| panic!("no report by hour {hour}: {text}"))
    };

    // The night is cool and the afternoon is not: the report reads the clock.
    assert!(at(2) < 20.0, "02:00 is {}: {text}", at(2));
    assert!(at(12) > 30.0 || at(13) > 30.0, "{temps:?}");

    // The vent ran in the afternoon, after a reading over thirty.
    let vents: Vec<i64> = entries
        .iter()
        .filter(|e| {
            e.record.action() == Some("vent") && e.record.kind() == avsm::record::Kind::Intent
        })
        .map(|e| e.at - start)
        .collect();
    assert!(!vents.is_empty(), "the vent never opened: {text}");
    let first = vents[0];
    let intent = entries
        .iter()
        .find(|e| {
            e.record.action() == Some("vent") && e.record.kind() == avsm::record::Kind::Intent
        })
        .unwrap()
        .seq;
    let why = k.why(intent).unwrap();
    assert!(why.contains("measured  temperature=31.3"), "{why}");
    assert!(why.contains("threshold  temperature above 30"), "{why}");
    assert!(why.contains("cap        0 of 12/hour used"), "{why}");
    assert!(
        (11 * 3600..17 * 3600).contains(&first),
        "vented at {first}s: {text}"
    );
    assert!(dir.join("vented").is_file(), "the vent left no state");
    // The vent ran in the world, never as the lever.
    let outcome = format!("#{intent} done  simulated: world  vent opened at");
    assert!(text.contains(&outcome), "{text}");

    // The reading after the vent is lower than the one before it, by more
    // than the clock alone would have moved it.
    let before = temps.iter().rev().find(|(t, _)| *t <= first).unwrap().1;
    let after = temps.iter().find(|(t, _)| *t > first).unwrap().1;
    assert!(before > 30.0, "{temps:?}");
    assert!(after < before - 2.0, "{before} then {after}: {temps:?}");
    // The evening is under thirty with no more vents.
    assert!(at(22) < 30.0, "{temps:?}");
}
