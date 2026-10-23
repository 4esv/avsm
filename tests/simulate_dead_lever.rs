//! Issue #49: a lever measured ineffective stays refused across a series
//! of sensor faults, with a human who approves two asks in three. One test
//! in this file: the virtual clock belongs to the whole process.

use std::os::unix::fs::PermissionsExt;

use avsmd::record::{Record, Signal};
use avsmd::simulate::{run, Human, Plan};

/// The world of the hostile year in small: a tomato that dries 4 a step
/// and takes 15 a watering, dead at 0 from day 2, a watering of it then
/// counted in `dead`. Its sensor lies on days 4 (stuck at 15), 6 (stuck
/// at 12) and 8 (a flap of 5 and 95).
const WORLD: &str = r#"
cd "$W"
m=$(cat m)
if [ -n "${AVSM_LEVER:-}" ]; then
  if [ "$(cat alive)" = 1 ]; then echo $((m + 15)) > m; else echo x >> dead; fi
  echo watered; exit 0
fi
if [ "$AVSM_DAY" -ge 2 ]; then echo 0 > alive; echo 0 > m; m=0
else m=$((m - 4)); [ $m -lt 0 ] && m=0; echo $m > m; fi
s=$m
case "$AVSM_DAY" in
  4) s=15 ;;
  6) s=12 ;;
  8) if [ $(( (AVSM_NOW / 7200) % 2 )) -eq 1 ]; then s=5; else s=95; fi ;;
esac
echo $s > sensor
"#;

#[test]
fn a_dead_lever_stays_refused_through_sensor_faults_with_a_mixed_human() {
    std::env::set_var("TZ", "UTC");
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    // Packet forecasts off: what is under test is the gate and the asks.
    std::fs::write(
        home.join("config.yaml"),
        "nightly: never\ndefault: k\nbackends:\n  k: {kind: command, run: echo nothing}\n\
         forecast: {days: 7, seeds: 0}\n",
    )
    .unwrap();
    let world = tmp.path().join("world");
    std::fs::create_dir_all(&world).unwrap();
    std::fs::write(world.join("m"), "60\n").unwrap();
    std::fs::write(world.join("alive"), "1\n").unwrap();
    std::fs::write(world.join("sensor"), "60\n").unwrap();
    let tomato = tmp.path().join("tomato");
    std::fs::create_dir_all(&tomato).unwrap();
    let report = tomato.join("report");
    std::fs::write(
        &report,
        format!("#!/bin/sh\ncat {}\n", world.join("sensor").display()),
    )
    .unwrap();
    std::fs::set_permissions(&report, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        tomato.join("mandate.yaml"),
        "purpose: Keep the tomato alive; soil moisture between 35 and 80.\nmetric: moisture\n\
         controller: {kind: threshold, below: 35, action: water}\ncadence: 2h\n\
         range: [0, 100]\nmax_step: 30\n\
         permitted:\n  - {name: water, run: ./water, cap: 4/day, reversible: true, cost: 1, expects: up 5 within 1}\n",
    )
    .unwrap();
    let plan = Plan {
        days: 10,
        step: 7200,
        human: Human::Mixed,
        start: Some(1_767_225_600), // 2026-01-01T00:00:00Z
        add: vec![tomato.clone()],
        world: Some(format!("W={} ; {WORLD}", world.display())),
    };
    let mut out = Vec::new();
    run(&home, &plan, &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();

    let k = avsmd::kernel::Kernel::open(&home).unwrap();
    let entries = k.entries().unwrap();
    let start = 1_767_225_600;
    let day = |at: i64| (at - start) / 86_400;
    // The plant was watered alive, so the lever worked before it died.
    let waters: Vec<i64> = entries
        .iter()
        .filter(|e| matches!(&e.record, Record::Intent { action, .. } if action == "water"))
        .map(|e| day(e.at))
        .collect();
    assert!(waters.iter().any(|d| *d < 2), "{waters:?}\n{text}");
    // Found ineffective once it died, and never asked about again on a
    // reading of a faulty sensor.
    let asks: Vec<i64> = entries
        .iter()
        .filter(|e| {
            matches!(
                &e.record,
                Record::Ask {
                    signal: Some(Signal::Ineffective { .. }),
                    ..
                }
            )
        })
        .map(|e| day(e.at))
        .collect();
    assert!(!asks.is_empty(), "{text}");
    assert!(
        asks.iter().all(|d| *d < 4),
        "an ineffective ask on a fault: days {asks:?}\n{text}"
    );
    // No watering from the first fault on, and at most the two before
    // the evidence and on the first answer.
    assert!(
        waters.iter().all(|d| *d < 4),
        "watered on a fault: days {waters:?}\n{text}"
    );
    let dead = std::fs::read_to_string(world.join("dead")).unwrap_or_default();
    assert!(dead.lines().count() <= 2, "{dead:?}\n{text}");
}
