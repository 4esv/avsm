//! Issue #53: a sensor that lags its lever, through the real binaries.
//! Power 2 of #50 read a disk cache refreshed every 25 s at a cadence of
//! 5 s, with `expects: down within 3`, and found a working cleanup
//! ineffective twice. Here the times are a fifth: a cadence of 1 s and a
//! cache refreshed every 4 to 6 reports, as a 25 s refresh read every 5 s
//! with jitter is. Usage grows 2 a report; cleanup frees 25.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use avsmd::ledger::Ledger;
use avsmd::record::{Entry, Record, Signal};

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        // SAFETY: kill has no memory preconditions.
        unsafe {
            libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM);
        }
        let began = Instant::now();
        while began.elapsed() < Duration::from_secs(10) {
            if let Ok(Some(_)) = self.0.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn script(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(dir.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A home with the disk venture adopted: its report reads a cache of the
/// usage, refreshed on reports whose numbers are in `refresh`, and its
/// cleanup runs `cleanup`.
fn disk(tmp: &Path, cleanup: &str) -> (PathBuf, PathBuf) {
    let home = tmp.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.yaml"),
        "nightly: never\nforecast: {days: 2, seeds: 0}\n",
    )
    .unwrap();
    let dir = tmp.join("disk");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mandate.yaml"),
        "purpose: Keep disk usage under 80 percent.\nmetric: disk_pct\n\
         controller: {kind: threshold, above: 80, action: cleanup}\ncadence: 1s\n\
         range: [0, 100]\npermitted:\n  - {name: cleanup, run: ./cleanup, cap: 20/hour, \
         reversible: true, cost: 1, expects: down within 3}\n",
    )
    .unwrap();
    std::fs::write(dir.join("actual"), "60\n").unwrap();
    std::fs::write(dir.join("cached"), "60\n").unwrap();
    std::fs::write(dir.join("n"), "0\n").unwrap();
    // A refresh every 5 reports on average: 5, 4, 6 between them.
    let mut at = 0;
    let refresh: String = [5, 4, 6]
        .iter()
        .cycle()
        .take(120)
        .map(|gap| {
            at += gap;
            format!("{at}\n")
        })
        .collect();
    std::fs::write(dir.join("refresh"), refresh).unwrap();
    script(
        &dir,
        "report",
        "n=$(($(cat n) + 1)); echo $n > n\n\
         a=$(($(cat actual) + 2)); [ $a -gt 100 ] && a=100; echo $a > actual\n\
         grep -qx \"$n\" refresh && cp actual cached\n\
         cat cached",
    );
    script(&dir, "cleanup", cleanup);
    avsmd::kernel::Kernel::open(&home)
        .unwrap()
        .add(&dir)
        .unwrap();
    (home, dir)
}

fn start(tmp: &Path, home: &Path) -> Daemon {
    let sock = avsmd::kernel::socket_path(home);
    let child = Command::new(env!("CARGO_BIN_EXE_avsmd"))
        .arg("--home")
        .arg(home)
        .env("HOME", tmp)
        .env_remove("AVSMD_HOME")
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(tmp.join("daemon.log")).unwrap())
        .spawn()
        .expect("spawn avsmd");
    let began = Instant::now();
    while !avsmd::protocol::alive(&sock) {
        assert!(
            began.elapsed() < Duration::from_secs(30),
            "avsmd did not start: {}",
            std::fs::read_to_string(tmp.join("daemon.log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    Daemon(child)
}

fn ledger(home: &Path) -> Vec<Entry> {
    Ledger::open_read_only(&home.join("ledger.db"))
        .unwrap()
        .all()
        .unwrap()
}

fn lines(entries: &[Entry]) -> String {
    entries
        .iter()
        .filter(|e| !matches!(&e.record, Record::Report { venture, .. } if venture == "self"))
        .map(|e| e.line() + "\n")
        .collect()
}

fn cleanups(entries: &[Entry]) -> Vec<u64> {
    entries
        .iter()
        .filter(|e| matches!(&e.record, Record::Intent { action, .. } if action == "cleanup"))
        .map(|e| e.seq)
        .collect()
}

fn ineffective(entries: &[Entry]) -> bool {
    entries.iter().any(|e| {
        matches!(
            &e.record,
            Record::Ask {
                signal: Some(Signal::Ineffective { .. }),
                ..
            }
        )
    })
}

fn readings_after(entries: &[Entry], seq: u64) -> usize {
    entries
        .iter()
        .filter(|e| e.seq > seq)
        .filter(|e| matches!(&e.record, Record::Report { metric, .. } if metric == "disk_pct"))
        .count()
}

/// Wait up to `secs` for `f` to hold; the last value of the ledger.
fn until(home: &Path, secs: u64, what: &str, f: impl Fn(&[Entry]) -> bool) -> Vec<Entry> {
    let began = Instant::now();
    loop {
        let entries = ledger(home);
        if f(&entries) {
            return entries;
        }
        assert!(
            began.elapsed() < Duration::from_secs(secs),
            "{what}, within {secs}s:\n{}",
            lines(&entries)
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn a_lagging_sensor_never_suspends_a_cleanup_that_works() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, _dir) = disk(
        tmp.path(),
        "a=$(($(cat actual) - 25)); [ $a -lt 0 ] && a=0; echo $a > actual",
    );
    let _d = start(tmp.path(), &home);
    // Three runs, each judged on the refresh after it: the gate holds the
    // next one until the last is judged, and the third has a refresh
    // after it too.
    let entries = until(&home, 150, "three cleanups judged", |e| {
        ineffective(e)
            || cleanups(e)
                .get(2)
                .is_some_and(|third| readings_after(e, *third) >= 8)
    });
    assert!(
        !ineffective(&entries),
        "a working cleanup behind a lagging sensor was found ineffective:\n{}",
        lines(&entries)
    );
    assert!(
        !entries
            .iter()
            .any(|e| matches!(&e.record, Record::Report { metric, .. } if metric == "suspension")),
        "{}",
        lines(&entries)
    );
}

#[test]
fn a_dead_cleanup_behind_the_same_sensor_is_suspended_within_two_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, _dir) = disk(tmp.path(), "echo freed nothing");
    let _d = start(tmp.path(), &home);
    let entries = until(
        &home,
        120,
        "the dead cleanup found ineffective",
        ineffective,
    );
    let runs = cleanups(&entries);
    assert!(
        !runs.is_empty() && runs.len() <= 2,
        "{} runs before the ask:\n{}",
        runs.len(),
        lines(&entries)
    );
}
