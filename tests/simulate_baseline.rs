//! Simulation without `--world`: the baseline stands in for the levers,
//! and what it reads is the ledger's own past, no evidence of what a
//! lever does. And stderr says a refusal once an episode, as the ledger
//! does, not once a tick.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard};

use avsmd::record::{Claim, Record, Signal};
use avsmd::simulate::{run, Human, Plan};

/// The virtual clock belongs to the whole process: one test at a time.
static CLOCK: Mutex<()> = Mutex::new(());

const START: i64 = 1_767_225_600; // 2026-01-01T00:00:00Z

fn clock() -> MutexGuard<'static, ()> {
    std::env::set_var("TZ", "UTC");
    CLOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Take the janitor on before START with a few readings of 89: a
/// simulation starts from the ledger's last reading, and never runs the
/// report program to get one.
fn history(home: &Path, dir: &Path) {
    avsmd::clock::set(START - 5 * 3600);
    let mut k = avsmd::kernel::Kernel::open(home).unwrap();
    k.add(dir).unwrap();
    for _ in 0..4 {
        avsmd::clock::advance(3600);
        k.append(Record::report(
            Claim::Measured,
            "janitor",
            "usage",
            Some(89.0),
            "",
        ))
        .unwrap();
    }
}

/// A cleanup venture whose usage reads 89 and stays there unless
/// something moves it, with one delegated lever under `lever`.
fn janitor(dir: &Path, lever: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("mandate.yaml"),
        format!(
            "purpose: Keep usage down.\nmetric: usage\n\
             controller: {{kind: threshold, above: 50, action: clean}}\ncadence: 1h\n\
             permitted:\n- {lever}\n"
        ),
    )
    .unwrap();
    let report = dir.join("report");
    std::fs::write(&report, "#!/bin/sh\necho 89\n").unwrap();
    std::fs::set_permissions(&report, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn home_in(tmp: &Path) -> std::path::PathBuf {
    let home = tmp.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.yaml"),
        "nightly: never\nforecast: {days: 2, seeds: 2}\n",
    )
    .unwrap();
    home
}

#[test]
fn a_frozen_baseline_never_finds_a_lever_ineffective() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let home = home_in(tmp.path());
    let dir = tmp.path().join("janitor");
    janitor(
        &dir,
        "{name: clean, run: 'true', cap: 6/hour, reversible: true, delegated: true, \
         expects: down within 2}",
    );
    history(&home, &dir);
    let plan = Plan {
        days: 1,
        step: 3600,
        human: Human::Away,
        start: Some(START),
        add: vec![dir],
        world: None,
    };
    let mut out = Vec::new();
    run(&home, &plan, &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    let k = avsmd::kernel::Kernel::open(&home).unwrap();
    let entries = k.entries().unwrap();
    let ineffective = entries
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
        .count();
    assert_eq!(ineffective, 0, "{text}");
    assert!(!k.index().unwrap().suspended("janitor", "clean"), "{text}");
    let runs = entries
        .iter()
        .filter(|e| matches!(&e.record, Record::Intent { action, .. } if action == "clean"))
        .count();
    assert!(runs > 1, "the lever keeps running: {runs}\n{text}");
}

#[test]
fn stderr_says_a_refusal_once_an_episode() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let home = home_in(tmp.path());
    let dir = tmp.path().join("janitor");
    // Runs once, then its cap refuses it every tick of the day.
    janitor(
        &dir,
        "{name: clean, run: 'true', cap: 1/day, reversible: true, delegated: true}",
    );
    history(&home, &dir);
    let out = Command::new(env!("CARGO_BIN_EXE_avsm"))
        .args([
            "--home",
            home.to_str().unwrap(),
            "simulate",
            "--days",
            "1",
            "--step",
            "10m",
            "--approve",
            "none",
            "--start",
            "1767225600",
            dir.to_str().unwrap(),
        ])
        .env("HOME", tmp.path())
        .env("TZ", "UTC")
        .env_remove("AVSMD_HOME")
        .env_remove("XDG_STATE_HOME")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}{stderr}");
    let refused = stderr
        .lines()
        .filter(|l| l.contains("clean refused"))
        .count();
    let opened = stdout.matches("refusal").count();
    assert!(opened >= 1, "the cap refused it: {stdout}");
    assert_eq!(refused, 1, "one line an episode, not a tick:\n{stderr}");
}
