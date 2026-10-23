//! The daemon stays live: a hung lever or metric leaves the socket
//! answering and the other ventures reporting, a pause stops a lever in
//! flight, a dead warden or roles process is respawned and recorded, and a
//! ledger that cannot be written shows in status. Through the real
//! binaries.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use avsmd::ledger::Ledger;
use avsmd::record::{Claim, Entry, Record};

/// A config that never refuses a run for the night, with forecasts small.
const NEVER: &str = "nightly: never\nforecast: {days: 2, seeds: 2}\n";

fn run(bin: &str, tmp: &Path, args: &[&str]) -> Output {
    Command::new(bin)
        .args(args)
        .env("HOME", tmp)
        .env_remove("AVSMD_HOME")
        .env_remove("XDG_STATE_HOME")
        .stdin(Stdio::null())
        .output()
        .expect("run")
}

fn avsm(tmp: &Path, home: &Path, args: &[&str]) -> Output {
    let mut all = vec!["--home", home.to_str().unwrap()];
    all.extend_from_slice(args);
    run(env!("CARGO_BIN_EXE_avsm"), tmp, &all)
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

struct Daemon {
    child: Child,
    log: PathBuf,
}

impl Daemon {
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// The pid the daemon logged for its warden or roles process.
    fn pid_of(&self, what: &str) -> i32 {
        let log = self.log();
        let line = log
            .lines()
            .rfind(|l| l.contains("warden pid"))
            .unwrap_or_else(|| panic!("no pids in the log:\n{log}"));
        let word = format!("{what} pid ");
        let at = line.find(&word).unwrap() + word.len();
        line[at..]
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start an avsmd binary on `home` and wait for its socket.
fn start_with(bin: &Path, tmp: &Path, home: &Path) -> Daemon {
    std::fs::create_dir_all(home).unwrap();
    if !home.join("config.yaml").exists() {
        std::fs::write(home.join("config.yaml"), NEVER).unwrap();
    }
    let log = tmp.join("daemon.log");
    let child = Command::new(bin)
        .args(["--home", home.to_str().unwrap()])
        .env("HOME", tmp)
        .env_remove("AVSMD_HOME")
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .expect("spawn avsmd");
    let d = Daemon { child, log };
    let sock = avsmd::kernel::socket_path(home);
    wait_for("avsmd to start", Duration::from_secs(30), || {
        avsmd::protocol::alive(&sock)
    });
    d
}

fn start(tmp: &Path, home: &Path) -> Daemon {
    start_with(Path::new(env!("CARGO_BIN_EXE_avsmd")), tmp, home)
}

/// Poll `ok` until it holds, or fail after `within`.
fn wait_for(what: &str, within: Duration, mut ok: impl FnMut() -> bool) {
    let began = Instant::now();
    while !ok() {
        assert!(began.elapsed() < within, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A venture named after its directory: its report prints `reads`, its
/// one lever `reset` runs `lever`, and it fires when the reading is above
/// 10. `extra` goes under the lever.
fn venture(
    parent: &Path,
    name: &str,
    reads: &str,
    lever: &str,
    extra: &str,
    cadence: &str,
) -> PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mandate.yaml"),
        format!(
            "purpose: Keep the counter below ten.\n\
             metric: count\n\
             controller:\n  kind: threshold\n  above: 10\n  action: reset\n\
             cadence: {cadence}\n\
             permitted:\n  - name: reset\n    run: ./reset\n    cap: 3/day\n    reversible: true\n{extra}"
        ),
    )
    .unwrap();
    for (file, body) in [
        ("report", format!("#!/bin/sh\n{reads}\n")),
        ("reset", format!("#!/bin/sh\n{lever}\n")),
    ] {
        std::fs::write(dir.join(file), body).unwrap();
        std::fs::set_permissions(dir.join(file), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

/// A lever that says it started and its pid, then hangs.
const HANGS: &str = "echo $$ > pid; touch started; sleep 3000";

fn entries(home: &Path) -> Vec<Entry> {
    Ledger::open_read_only(&home.join("ledger.db"))
        .and_then(|l| l.all())
        .unwrap_or_default()
}

fn intents(home: &Path, venture: &str) -> Vec<u64> {
    entries(home)
        .iter()
        .filter(|e| matches!(&e.record, Record::Intent { venture: v, .. } if v == venture))
        .map(|e| e.seq)
        .collect()
}

fn outcome_of(home: &Path, intent: u64) -> Option<(bool, String)> {
    entries(home).into_iter().find_map(|e| match e.record {
        Record::Outcome {
            intent: i,
            ok,
            detail,
            ..
        } if i == intent => Some((ok, detail)),
        _ => None,
    })
}

fn reports(home: &Path, venture: &str) -> usize {
    entries(home)
        .iter()
        .filter(|e| matches!(&e.record, Record::Report { venture: v, metric, .. } if v == venture && metric == "count"))
        .count()
}

/// Run a client command, and fail when it takes longer than `limit`.
/// The client is killed at the limit, so a daemon that never answers
/// fails the test at once instead of hanging it.
fn timed(tmp: &Path, home: &Path, args: &[&str], limit: Duration) -> Output {
    let mut all = vec!["--home", home.to_str().unwrap()];
    all.extend_from_slice(args);
    let mut child = Command::new(env!("CARGO_BIN_EXE_avsm"))
        .args(&all)
        .env("HOME", tmp)
        .env_remove("AVSMD_HOME")
        .env_remove("XDG_STATE_HOME")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run avsm");
    let began = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if began.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            panic!("avsm {} took over {limit:?}", args.join(" "));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

fn alive(pid: i32) -> bool {
    // SAFETY: kill with signal 0 only checks.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn pid_in(dir: &Path) -> i32 {
    std::fs::read_to_string(dir.join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

#[test]
fn a_hung_lever_leaves_status_answering_and_another_venture_reporting() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let d = start(tmp.path(), &home);
    let hung = venture(tmp.path(), "hung", "echo 11", HANGS, "", "1h");
    let steady = venture(tmp.path(), "steady", "echo 5", "true", "", "1s");
    for dir in [&steady, &hung] {
        let out = timed(
            tmp.path(),
            &home,
            &["add", dir.to_str().unwrap()],
            Duration::from_secs(10),
        );
        assert!(out.status.success(), "{}", text(&out));
    }
    wait_for("the hung lever to start", Duration::from_secs(30), || {
        hung.join("started").exists()
    });
    // The socket answers while the lever hangs.
    for args in [&["status"][..], &["asks"], &["tail", "5"]] {
        let out = timed(tmp.path(), &home, args, Duration::from_secs(2));
        assert!(out.status.success(), "{}\n{}", text(&out), d.log());
    }
    let status = text(&timed(
        tmp.path(),
        &home,
        &["status"],
        Duration::from_secs(2),
    ));
    assert!(status.contains("reset running"), "{status}");
    // The other venture keeps reporting.
    let before = reports(&home, "steady");
    std::thread::sleep(Duration::from_secs(4));
    let after = reports(&home, "steady");
    assert!(
        after >= before + 2,
        "steady went from {before} to {after} reports\n{}",
        d.log()
    );
    // The lever is still in flight: one intent, no outcome, and no
    // "did it happen?" ask about it.
    let i = intents(&home, "hung");
    assert_eq!(i.len(), 1, "{}", d.log());
    assert_eq!(outcome_of(&home, i[0]), None);
    let asks = text(&avsm(tmp.path(), &home, &["asks"]));
    assert!(!asks.contains("did reset happen"), "{asks}");
}

#[test]
fn a_hung_metric_leaves_status_answering_and_another_venture_reporting() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let d = start(tmp.path(), &home);
    let hung = venture(
        tmp.path(),
        "slow",
        "touch started; sleep 100000",
        "true",
        "",
        "5s",
    );
    let steady = venture(tmp.path(), "steady", "echo 5", "true", "", "1s");
    for dir in [&hung, &steady] {
        let out = timed(
            tmp.path(),
            &home,
            &["add", dir.to_str().unwrap()],
            Duration::from_secs(10),
        );
        assert!(out.status.success(), "{}", text(&out));
    }
    wait_for("the hung report to start", Duration::from_secs(30), || {
        hung.join("started").exists()
    });
    let out = timed(tmp.path(), &home, &["status"], Duration::from_secs(2));
    assert!(out.status.success(), "{}", text(&out));
    let before = reports(&home, "steady");
    std::thread::sleep(Duration::from_secs(4));
    let after = reports(&home, "steady");
    assert!(
        after >= before + 2,
        "steady went from {before} to {after} reports\n{}",
        d.log()
    );
}

#[test]
fn pausing_a_venture_stops_its_hung_lever_and_records_the_outcome() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let d = start(tmp.path(), &home);
    let hung = venture(tmp.path(), "hung", "echo 11", HANGS, "", "1h");
    let out = avsm(tmp.path(), &home, &["add", hung.to_str().unwrap()]);
    assert!(out.status.success(), "{}", text(&out));
    wait_for("the hung lever to start", Duration::from_secs(30), || {
        hung.join("started").exists()
    });
    let pid = pid_in(&hung);
    let out = timed(
        tmp.path(),
        &home,
        &["pause", "hung"],
        Duration::from_secs(2),
    );
    assert!(out.status.success(), "{}", text(&out));
    let i = intents(&home, "hung");
    assert_eq!(i.len(), 1, "{}", d.log());
    wait_for(
        "the outcome of the paused lever",
        Duration::from_secs(20),
        || outcome_of(&home, i[0]).is_some(),
    );
    let (ok, detail) = outcome_of(&home, i[0]).unwrap();
    assert!(!ok, "{detail}");
    assert!(detail.contains("paused"), "{detail}");
    wait_for("the lever's process to end", Duration::from_secs(5), || {
        !alive(pid)
    });
    // Paused, nothing runs again.
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(intents(&home, "hung").len(), 1, "{}", d.log());
}

#[test]
fn a_lever_is_killed_at_its_timeout() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let d = start(tmp.path(), &home);
    let hung = venture(
        tmp.path(),
        "hung",
        "echo 11",
        HANGS,
        "    timeout: 2s\n",
        "1h",
    );
    let out = avsm(tmp.path(), &home, &["add", hung.to_str().unwrap()]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("2s"),
        "the teachback names the timeout: {}",
        text(&out)
    );
    wait_for("the hung lever to start", Duration::from_secs(30), || {
        hung.join("started").exists()
    });
    let i = intents(&home, "hung");
    assert_eq!(i.len(), 1, "{}", d.log());
    wait_for("the lever's timeout", Duration::from_secs(15), || {
        outcome_of(&home, i[0]).is_some()
    });
    let (ok, detail) = outcome_of(&home, i[0]).unwrap();
    assert!(!ok && detail.contains("killed after 2s"), "{detail}");
}

#[test]
fn a_lever_timeout_is_a_positive_duration() {
    let tmp = tempfile::tempdir().unwrap();
    for bad in ["soon", "0s", "5"] {
        let dir = venture(
            tmp.path(),
            "bad",
            "echo 1",
            "true",
            &format!("    timeout: {bad}\n"),
            "1h",
        );
        let text = std::fs::read_to_string(dir.join("mandate.yaml")).unwrap();
        let e = avsmd::mandate::Mandate::parse(&text).expect_err(bad);
        assert!(e.0.contains("timeout"), "{bad}: {}", e.0);
    }
    let ok = venture(
        tmp.path(),
        "fine",
        "echo 1",
        "true",
        "    timeout: 90s\n",
        "1h",
    );
    let m =
        avsmd::mandate::Mandate::parse(&std::fs::read_to_string(ok.join("mandate.yaml")).unwrap())
            .unwrap();
    assert_eq!(m.permitted[0].timeout, Some(Duration::from_secs(90)));
}

#[test]
fn a_warden_killed_alone_is_respawned_and_the_lever_in_flight_is_asked_about() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let d = start(tmp.path(), &home);
    let hung = venture(tmp.path(), "hung", "echo 11", HANGS, "", "1h");
    let out = avsm(tmp.path(), &home, &["add", hung.to_str().unwrap()]);
    assert!(out.status.success(), "{}", text(&out));
    wait_for("the hung lever to start", Duration::from_secs(30), || {
        hung.join("started").exists()
    });
    let warden = d.pid_of("warden");
    // SAFETY: kill has no memory preconditions.
    unsafe {
        libc::kill(warden, libc::SIGKILL);
        libc::kill(pid_in(&hung), libc::SIGKILL);
    }
    // A measured report that it happened.
    wait_for(
        "a report of the dead warden",
        Duration::from_secs(15),
        || {
            entries(&home).iter().any(|e| {
                e.record.claim() == Claim::Measured
                    && matches!(&e.record, Record::Report { metric, note, .. }
                    if metric == "warden" && note.contains("respawned"))
            })
        },
    );
    assert!(d.log().contains("warden"), "{}", d.log());
    assert_ne!(d.pid_of("warden"), warden, "{}", d.log());
    // The intent in flight is asked about, never run again blindly.
    wait_for("the did-it-happen ask", Duration::from_secs(15), || {
        text(&avsm(tmp.path(), &home, &["asks"])).contains("did reset happen")
    });
    assert_eq!(intents(&home, "hung").len(), 1, "{}", d.log());
    // The new warden runs levers.
    let after = venture(tmp.path(), "after", "echo 11", "touch ran", "", "1h");
    let out = avsm(tmp.path(), &home, &["add", after.to_str().unwrap()]);
    assert!(out.status.success(), "{}", text(&out));
    wait_for(
        "a lever through the new warden",
        Duration::from_secs(20),
        || after.join("ran").exists(),
    );
    let doctor = text(&avsm(tmp.path(), &home, &["doctor"]));
    assert!(doctor.contains("ok    warden pid"), "{doctor}");
}

#[test]
fn a_roles_process_killed_alone_is_respawned_and_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let d = start(tmp.path(), &home);
    let roles = d.pid_of("roles");
    // SAFETY: kill has no memory preconditions.
    unsafe {
        libc::kill(roles, libc::SIGKILL);
    }
    wait_for(
        "a report of the dead roles process",
        Duration::from_secs(15),
        || {
            entries(&home).iter().any(|e| {
                matches!(&e.record, Record::Report { metric, note, .. }
                if metric == "roles" && note.contains("respawned"))
            })
        },
    );
    assert_ne!(d.pid_of("roles"), roles, "{}", d.log());
}

#[test]
fn a_warden_that_cannot_be_respawned_shows_in_status_and_doctor() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    // A copy of avsmd that goes away once started: the warden cannot be
    // started again from it.
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let copy = bin.join("avsmd");
    std::fs::copy(env!("CARGO_BIN_EXE_avsmd"), &copy).unwrap();
    let d = start_with(&copy, tmp.path(), &home);
    std::fs::rename(&copy, bin.join("gone")).unwrap();
    let warden = d.pid_of("warden");
    // SAFETY: kill has no memory preconditions.
    unsafe {
        libc::kill(warden, libc::SIGKILL);
    }
    wait_for(
        "status to name the dead warden",
        Duration::from_secs(15),
        || text(&avsm(tmp.path(), &home, &["status"])).contains("warden"),
    );
    let status = text(&avsm(tmp.path(), &home, &["status"]));
    assert!(!status.starts_with("all running"), "{status}");
    let out = avsm(tmp.path(), &home, &["doctor"]);
    let doctor = text(&out);
    assert!(!out.status.success(), "{doctor}");
    assert!(doctor.contains("FAIL  the warden"), "{doctor}");
    assert!(d.log().contains("cannot start the warden"), "{}", d.log());
}

#[test]
fn a_ledger_that_cannot_be_written_shows_in_status_and_is_logged_once() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let d = start(tmp.path(), &home);
    let steady = venture(tmp.path(), "steady", "echo 5", "true", "", "1s");
    let out = avsm(tmp.path(), &home, &["add", steady.to_str().unwrap()]);
    assert!(out.status.success(), "{}", text(&out));
    wait_for("a few reports", Duration::from_secs(20), || {
        reports(&home, "steady") >= 3
    });
    // Truncate the ledger under the daemon, as the chaos test did: its
    // pages moved out of the write-ahead log first, so what is cut is
    // the records.
    let db = home.join("ledger.db");
    rusqlite::Connection::open(&db)
        .unwrap()
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&db)
        .unwrap()
        .set_len(8192)
        .unwrap();
    wait_for(
        "status to name the ledger failure",
        Duration::from_secs(20),
        || text(&avsm(tmp.path(), &home, &["status"])).contains("the ledger cannot be written"),
    );
    let status = text(&avsm(tmp.path(), &home, &["status"]));
    assert!(!status.contains("all running"), "{status}");
    // Loud once, not every tick.
    std::thread::sleep(Duration::from_secs(4));
    let log = d.log();
    let loud = log
        .lines()
        .filter(|l| l.contains("the ledger cannot be written"))
        .count();
    assert_eq!(loud, 1, "{log}");
    assert!(
        !log.lines().any(|l| l.contains("cannot write report")),
        "{log}"
    );
}
