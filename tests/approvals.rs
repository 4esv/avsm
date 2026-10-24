//! An approval is a human's word, used once (#51): one refused for any
//! reason is spent, only the first answer to an ask counts, a record the
//! running daemon did not write is an alarm and never a premise, and a
//! process the daemon started cannot approve over the socket.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use avsmd::kernel::Kernel;
use avsmd::ledger::Ledger;
use avsmd::record::{Claim, Entry, Record, Seq, Verdict};
use avsmd::warden::LocalWarden;

// ----- in process -----

/// A venture whose report always reads 5, above its threshold of 3, and
/// whose one lever, irreversible, appends a line to `ran` when it runs.
fn venture(parent: &Path, name: &str, cap: &str) -> PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mandate.yaml"),
        format!(
            "purpose: Keep n down.\nmetric: n\n\
             controller: {{kind: threshold, above: 3, action: bump}}\ncadence: 1s\n\
             permitted:\n  - {{name: bump, run: ./bump, cap: {cap}, reversible: false}}\n"
        ),
    )
    .unwrap();
    script(&dir, "report", "#!/bin/sh\necho 5\n");
    script(
        &dir,
        "bump",
        &format!("#!/bin/sh\necho ran >> {}\n", dir.join("ran").display()),
    );
    dir
}

fn script(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).unwrap();
    std::fs::set_permissions(dir.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn runs(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("ran"))
        .map(|t| t.lines().count())
        .unwrap_or(0)
}

fn home(tmp: &Path) -> PathBuf {
    let home = tmp.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.yaml"),
        "nightly: never\nforecast: {days: 2, seeds: 2}\n",
    )
    .unwrap();
    home
}

fn kernel(home: &Path) -> Kernel {
    Kernel::with(
        home,
        Box::new(avsmd::role::ModelRole),
        Box::new(LocalWarden::open(home).unwrap()),
    )
    .unwrap()
}

fn lines(entries: &[Entry]) -> String {
    entries.iter().map(|e| e.line() + "\n").collect()
}

/// The open asks to bump, oldest first.
fn bump_asks(k: &Kernel) -> Vec<Seq> {
    let open = k.open_asks().unwrap();
    k.entries()
        .unwrap()
        .into_iter()
        .filter(|e| open.contains(&e.seq) && e.record.action() == Some("bump"))
        .map(|e| e.seq)
        .collect()
}

#[test]
fn an_approval_refused_at_its_cap_is_spent_and_never_runs_after_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let home = home(tmp.path());
    let dir = venture(tmp.path(), "v1", "1/hour");
    let t0 = avsmd::clock::now();
    let mut k = kernel(&home);
    k.add(&dir).unwrap();
    k.tick();
    let first = *bump_asks(&k).first().expect("an ask to bump");
    // A second ask for the same run, as a human may find two open: both
    // approved, the cap lets one run, and the other is refused at it.
    let ask = k
        .entries()
        .unwrap()
        .into_iter()
        .find(|e| e.seq == first)
        .unwrap();
    let second = k.append(ask.record.clone()).unwrap().seq;
    k.approve(first, None).unwrap();
    k.approve(second, None).unwrap();
    let approvals: Vec<Seq> = k
        .entries()
        .unwrap()
        .iter()
        .filter(|e| {
            matches!(
                &e.record,
                Record::Decision {
                    verdict: Verdict::Approve { .. },
                    ..
                }
            )
        })
        .map(|e| e.seq)
        .collect();
    k.tick();
    assert_eq!(runs(&dir), 1, "{}", lines(&k.entries().unwrap()));
    let refused = k.entries().unwrap().into_iter().find(|e| {
        matches!(&e.record, Record::Refusal { action, cites, .. }
            if action == "bump" && cites.iter().any(|c| approvals.contains(c)))
    });
    assert!(
        refused.is_some(),
        "the refusal at the cap cites the approval it spends:\n{}",
        lines(&k.entries().unwrap())
    );
    assert_eq!(
        k.index().unwrap().unacted().count(),
        0,
        "nothing left to act on"
    );
    drop(k);

    // Two hours on, the cap is free again: a restart runs nothing.
    let _later = avsmd::clock::scoped(t0 + 7200);
    let mut k = kernel(&home);
    k.tick();
    k.tick();
    assert_eq!(runs(&dir), 1, "{}", lines(&k.entries().unwrap()));
}

#[test]
fn a_second_approval_of_one_ask_runs_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let home = home(tmp.path());
    let dir = venture(tmp.path(), "v1", "9/hour");
    let mut k = kernel(&home);
    k.add(&dir).unwrap();
    k.tick();
    let ask = *bump_asks(&k).first().expect("an ask to bump");
    k.approve(ask, None).unwrap();
    // The CLI refuses a second answer; the ledger can still hold one.
    let again = k
        .append(Record::Decision {
            claim: Claim::Declared,
            venture: "v1".into(),
            verdict: Verdict::Approve { ask, payload: None },
            by: None,
        })
        .unwrap()
        .seq;
    k.tick();
    k.tick();
    assert_eq!(runs(&dir), 1, "{}", lines(&k.entries().unwrap()));
    assert!(
        !k.index().unwrap().unacted().any(|d| d == again),
        "a later answer authorizes nothing"
    );
    drop(k);
    let mut k = kernel(&home);
    k.tick();
    assert_eq!(runs(&dir), 1, "{}", lines(&k.entries().unwrap()));
}

// ----- through the real binaries -----

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Daemon {
    /// SIGTERM, and wait for it to drain.
    fn stop(mut self) {
        // SAFETY: kill has no memory preconditions.
        unsafe {
            libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM);
        }
        let _ = self.0.wait();
    }
}

fn start(tmp: &Path, home: &Path, log: &str) -> Daemon {
    let sock = avsmd::kernel::socket_path(home);
    let child = Command::new(env!("CARGO_BIN_EXE_avsmd"))
        .arg("--home")
        .arg(home)
        .env("HOME", tmp)
        .env_remove("AVSMD_HOME")
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(tmp.join(log)).unwrap())
        .spawn()
        .expect("spawn avsmd");
    let began = Instant::now();
    while !avsmd::protocol::alive(&sock) {
        assert!(
            began.elapsed() < Duration::from_secs(30),
            "avsmd did not start: {}",
            std::fs::read_to_string(tmp.join(log)).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    Daemon(child)
}

fn avsm(tmp: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_avsm"))
        .arg("--home")
        .arg(home)
        .args(args)
        .env("HOME", tmp)
        .env_remove("AVSMD_HOME")
        .stdin(Stdio::null())
        .output()
        .expect("run avsm")
}

fn ok(out: &Output) -> String {
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn ledger(home: &Path) -> Vec<Entry> {
    Ledger::open_read_only(&home.join("ledger.db"))
        .unwrap()
        .all()
        .unwrap()
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
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// No alarm: every record was the daemon's own, the warden's outcomes
/// written on its threads among them.
fn no_alarm(entries: &[Entry]) {
    assert!(
        !entries
            .iter()
            .any(|e| matches!(&e.record, Record::Report { metric, .. } if metric == "foreign")),
        "a false alarm:\n{}",
        lines(entries)
    );
}

/// The open asks to bump of v1, oldest first, as the ledger says.
fn open_bump(entries: &[Entry]) -> Vec<Seq> {
    let answered: Vec<Seq> = entries
        .iter()
        .filter_map(|e| match &e.record {
            Record::Decision {
                verdict: Verdict::Approve { ask, .. } | Verdict::Deny { ask, .. },
                ..
            } => Some(*ask),
            _ => None,
        })
        .collect();
    entries
        .iter()
        .filter(|e| {
            matches!(&e.record, Record::Ask { venture, action: Some(a), .. }
                if venture == "v1" && a == "bump")
                && !answered.contains(&e.seq)
        })
        .map(|e| e.seq)
        .collect()
}

/// A home with v1 (cap 20/hour, cadence 1s) and a config that keeps
/// forecasts small.
fn daemon_home(tmp: &Path) -> (PathBuf, PathBuf) {
    let home = home(tmp);
    let dir = venture(tmp, "v1", "20/hour");
    (home, dir)
}

#[test]
fn an_approval_refused_while_paused_never_runs_and_its_fresh_ask_runs_once() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, dir) = daemon_home(tmp.path());
    let d = start(tmp.path(), &home, "d1.log");
    ok(&avsm(tmp.path(), &home, &["add", dir.to_str().unwrap()]));
    let entries = until(&home, 30, "an ask to bump", |e| !open_bump(e).is_empty());
    let ask = open_bump(&entries)[0];
    ok(&avsm(tmp.path(), &home, &["pause", "v1"]));
    // A human's approve is refused while paused, and the ask stays open.
    let out = avsm(tmp.path(), &home, &["approve", &ask.to_string()]);
    assert_eq!(out.status.code(), Some(65));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("v1 is paused; resume it first"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    d.stop();
    // An approval that reaches the gate while paused all the same (written
    // while avsmd was down, so no alarm): the gate refuses it and the
    // refusal spends it.
    let decision = Ledger::open(&home.join("ledger.db"))
        .unwrap()
        .append(Record::Decision {
            claim: Claim::Declared,
            venture: "v1".into(),
            verdict: Verdict::Approve { ask, payload: None },
            by: None,
        })
        .unwrap()
        .seq;
    let d = start(tmp.path(), &home, "d1b.log");
    until(&home, 15, "a refusal of bump after the approval", |e| {
        e.iter().any(|e| {
            e.seq > decision
                && matches!(&e.record, Record::Refusal { action, .. } if action == "bump")
        })
    });
    ok(&avsm(tmp.path(), &home, &["resume", "v1"]));
    let entries = until(&home, 30, "a fresh ask to bump", |e| {
        open_bump(e).iter().any(|s| *s > decision)
    });
    let fresh = *open_bump(&entries).last().unwrap();
    d.stop();

    let d = start(tmp.path(), &home, "d2.log");
    std::thread::sleep(Duration::from_secs(5));
    assert_eq!(
        runs(&dir),
        0,
        "the refused approval ran at the next start:\n{}",
        lines(&ledger(&home))
    );
    ok(&avsm(tmp.path(), &home, &["approve", &fresh.to_string()]));
    let began = Instant::now();
    while runs(&dir) < 1 && began.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_secs(4));
    d.stop();
    assert_eq!(runs(&dir), 1, "{}", lines(&ledger(&home)));
    no_alarm(&ledger(&home));
}

#[test]
fn a_record_the_daemon_did_not_write_is_an_alarm_and_never_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, dir) = daemon_home(tmp.path());
    let d = start(tmp.path(), &home, "d.log");
    ok(&avsm(tmp.path(), &home, &["add", dir.to_str().unwrap()]));
    let entries = until(&home, 30, "an ask to bump", |e| !open_bump(e).is_empty());
    let ask = open_bump(&entries)[0];
    // Another writer of the same uid: a correctly chained, "declared"
    // approval, appended while avsmd runs.
    let forged = Ledger::open(&home.join("ledger.db"))
        .unwrap()
        .append(Record::Decision {
            claim: Claim::Declared,
            venture: "v1".into(),
            verdict: Verdict::Approve { ask, payload: None },
            by: None,
        })
        .unwrap()
        .seq;
    let entries = until(&home, 15, "an alarm citing the forged record", |e| {
        e.iter().any(|e| {
            matches!(&e.record, Record::Report { claim: Claim::Measured, venture, metric, cites, .. }
                if venture == "self" && metric == "foreign" && cites.contains(&forged))
        })
    });
    assert!(
        entries.iter().any(|e| e.seq > forged
            && matches!(&e.record, Record::Decision { claim: Claim::Measured, venture, verdict: Verdict::Pause, .. } if venture == "self")),
        "everything is paused:\n{}",
        lines(&entries)
    );
    std::thread::sleep(Duration::from_secs(4));
    assert_eq!(runs(&dir), 0, "{}", lines(&ledger(&home)));
    let status = ok(&avsm(tmp.path(), &home, &["status"]));
    assert!(status.contains(&format!("#{forged}")), "{status}");
    // The rows say what holds them, and the caution says what goes on.
    let row = status.lines().find(|l| l.starts_with("v1 ")).unwrap();
    assert!(row.contains("paused (alarm)"), "{status}");
    assert!(status.contains("no lever runs"), "{status}");
    let why = ok(&avsm(tmp.path(), &home, &["why", &forged.to_string()]));
    assert!(why.contains("not written by avsmd"), "{why}");
    assert!(!why.contains("by a human"), "{why}");
    let doctor = avsm(tmp.path(), &home, &["doctor"]);
    assert_eq!(
        doctor.status.code(),
        Some(65),
        "doctor passes a forged ledger"
    );
    // A human resumes; the forged approval stays spent.
    ok(&avsm(tmp.path(), &home, &["resume", "self"]));
    std::thread::sleep(Duration::from_secs(4));
    d.stop();
    assert_eq!(runs(&dir), 0, "{}", lines(&ledger(&home)));
    let log = std::fs::read_to_string(tmp.path().join("d.log")).unwrap();
    assert!(log.contains("not written by this avsmd"), "{log}");
}

#[test]
fn a_lever_that_runs_avsm_approve_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, dir) = daemon_home(tmp.path());
    // w's reversible lever, delegated, approves v1's ask over the socket.
    let w = tmp.path().join("w");
    std::fs::create_dir_all(&w).unwrap();
    std::fs::write(
        w.join("mandate.yaml"),
        "purpose: Tidy.\nmetric: m\ncontroller: {kind: threshold, above: 8, action: tidy}\n\
         cadence: 1s\npermitted:\n  - {name: tidy, run: ./tidy, cap: 1/hour, reversible: true}\n",
    )
    .unwrap();
    script(&w, "report", "#!/bin/sh\necho 9\n");
    let avsm_bin = env!("CARGO_BIN_EXE_avsm");
    let out = w.join("out");
    script(
        &w,
        "tidy",
        &format!(
            "#!/bin/sh\nA='{avsm_bin}'; H='{h}'\n\
             i=0; while [ $i -lt 60 ]; do\n\
               ASK=$(\"$A\" --home \"$H\" asks | awk '/bump\\?/{{print $1; exit}}' | tr -d '#')\n\
               [ -n \"$ASK\" ] && break; i=$((i+1)); sleep 0.5\n\
             done\n\
             \"$A\" --home \"$H\" approve \"$ASK\" > '{o}' 2>&1; echo \"exit $?\" >> '{o}'\n",
            h = home.display(),
            o = out.display()
        ),
    );
    let d = start(tmp.path(), &home, "d.log");
    ok(&avsm(tmp.path(), &home, &["add", dir.to_str().unwrap()]));
    ok(&avsm(tmp.path(), &home, &["add", w.to_str().unwrap()]));
    let began = Instant::now();
    while !std::fs::read_to_string(&out)
        .unwrap_or_default()
        .contains("exit")
    {
        assert!(
            began.elapsed() < Duration::from_secs(60),
            "the lever never tried:\n{}",
            lines(&ledger(&home))
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let said = std::fs::read_to_string(&out).unwrap();
    assert!(
        !said.contains("exit 0"),
        "the lever's approve went through: {said}"
    );
    assert!(said.contains("started by avsmd"), "{said}");
    std::thread::sleep(Duration::from_secs(4));
    assert_eq!(runs(&dir), 0, "{}", lines(&ledger(&home)));
    // The ask is still open, and a human's approval runs it once.
    let ask = open_bump(&ledger(&home))[0];
    ok(&avsm(tmp.path(), &home, &["approve", &ask.to_string()]));
    let began = Instant::now();
    while runs(&dir) < 1 && began.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_secs(3));
    d.stop();
    assert_eq!(runs(&dir), 1, "{}", lines(&ledger(&home)));
    no_alarm(&ledger(&home));
}

/// A venture `w` whose report, the first time it runs, leaves behind a
/// process that approves v1's ask over the socket: started by `escape`,
/// a shell prefix that takes it out of avsmd's tree. What it was told is
/// in `out`.
fn escaping_reporter(tmp: &Path, home: &Path, escape: &str) -> (PathBuf, PathBuf) {
    let w = tmp.join("w");
    std::fs::create_dir_all(&w).unwrap();
    std::fs::write(
        w.join("mandate.yaml"),
        "purpose: Tidy.\nmetric: m\ncontroller: {kind: threshold, above: 100, action: tidy}\n\
         cadence: 1s\npermitted:\n  - {name: tidy, run: 'true', cap: 1/hour, reversible: true}\n",
    )
    .unwrap();
    let avsm_bin = env!("CARGO_BIN_EXE_avsm");
    let out = w.join("out");
    script(
        &w,
        "approver",
        &format!(
            "#!/bin/sh\nexec </dev/null >/dev/null 2>&1\nA='{avsm_bin}'; H='{h}'\n\
             i=0; while [ $i -lt 60 ]; do\n\
               ASK=$(\"$A\" --home \"$H\" asks | awk '/bump\\?/{{print $1; exit}}' | tr -d '#')\n\
               [ -n \"$ASK\" ] && break; i=$((i+1)); sleep 0.5\n\
             done\n\
             \"$A\" --home \"$H\" approve \"$ASK\" > '{o}.tmp' 2>&1; echo \"exit $?\" >> '{o}.tmp'\n\
             mv '{o}.tmp' '{o}'\n",
            h = home.display(),
            o = out.display()
        ),
    );
    script(
        &w,
        "report",
        &format!(
            "#!/bin/sh\nif [ ! -e forked ]; then touch forked; {escape} ./approver; fi\necho 9\n"
        ),
    );
    (w, out)
}

/// Wait for what the escaped approver was told.
fn told(out: &Path, home: &Path) -> String {
    let began = Instant::now();
    while !out.exists() {
        assert!(
            began.elapsed() < Duration::from_secs(60),
            "the approver never tried:\n{}",
            lines(&ledger(home))
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    std::fs::read_to_string(out).unwrap()
}

#[test]
fn a_report_that_double_forks_avsm_approve_is_refused_and_the_lever_never_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, dir) = daemon_home(tmp.path());
    // `( ( approver ) & ) &`: the approver's parent exits at once, so it
    // is no child of avsmd's by the time it connects.
    let (w, out) = escaping_reporter(tmp.path(), &home, "( ( ./approver ) & ) & : ");
    let d = start(tmp.path(), &home, "d.log");
    ok(&avsm(tmp.path(), &home, &["add", dir.to_str().unwrap()]));
    ok(&avsm(tmp.path(), &home, &["add", w.to_str().unwrap()]));
    let said = told(&out, &home);
    assert!(
        !said.contains("exit 0"),
        "the double-forked approve went through: {said}"
    );
    assert!(said.contains("refused"), "{said}");
    std::thread::sleep(Duration::from_secs(3));
    d.stop();
    assert_eq!(runs(&dir), 0, "{}", lines(&ledger(&home)));
    no_alarm(&ledger(&home));
}

/// With job control on, the shell puts the approver in a process group of
/// its own: only the session avsmd made still says whose it is.
#[test]
fn a_report_that_moves_its_approver_to_a_group_of_its_own_is_still_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, dir) = daemon_home(tmp.path());
    let (w, out) = escaping_reporter(tmp.path(), &home, "sh -c 'set -m; \"$0\" & exit'");
    let d = start(tmp.path(), &home, "d.log");
    ok(&avsm(tmp.path(), &home, &["add", dir.to_str().unwrap()]));
    ok(&avsm(tmp.path(), &home, &["add", w.to_str().unwrap()]));
    let said = told(&out, &home);
    assert!(!said.contains("exit 0"), "the approve went through: {said}");
    assert!(said.contains("refused"), "{said}");
    std::thread::sleep(Duration::from_secs(3));
    d.stop();
    assert_eq!(runs(&dir), 0, "{}", lines(&ledger(&home)));
}

/// A program that calls setsid itself leaves avsmd's session. On Linux
/// avsmd is its subreaper, so it is still found in avsmd's tree and
/// refused; on macOS nothing ties it to avsmd, and its approve goes
/// through, as the README says.
#[test]
fn a_report_that_calls_setsid_to_approve_is_refused_on_linux_and_documented_on_macos() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, dir) = daemon_home(tmp.path());
    let escape = if cfg!(target_os = "linux") {
        "setsid -f"
    } else {
        "perl -MPOSIX -e 'exit if fork; POSIX::setsid(); exit if fork; exec @ARGV'"
    };
    let (w, out) = escaping_reporter(tmp.path(), &home, escape);
    let d = start(tmp.path(), &home, "d.log");
    ok(&avsm(tmp.path(), &home, &["add", dir.to_str().unwrap()]));
    ok(&avsm(tmp.path(), &home, &["add", w.to_str().unwrap()]));
    let said = told(&out, &home);
    if cfg!(target_os = "linux") {
        assert!(!said.contains("exit 0"), "{said}");
        assert!(said.contains("refused"), "{said}");
    } else {
        assert!(said.contains("exit 0"), "{said}");
        let began = Instant::now();
        while runs(&dir) < 1 && began.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(runs(&dir), 1, "{}", lines(&ledger(&home)));
    }
    d.stop();
}

#[test]
fn reports_go_on_during_a_foreign_alarm_and_a_forged_record_is_never_a_reading() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, dir) = daemon_home(tmp.path());
    let d = start(tmp.path(), &home, "d.log");
    ok(&avsm(tmp.path(), &home, &["add", dir.to_str().unwrap()]));
    until(&home, 30, "a report of n", |e| {
        e.iter()
            .any(|e| matches!(&e.record, Record::Report { venture, metric, .. } if venture == "v1" && metric == "n"))
    });
    // A forged "measured" reading of the metric, appended by another writer.
    let forged = Ledger::open(&home.join("ledger.db"))
        .unwrap()
        .append(Record::report(
            Claim::Measured,
            "v1",
            "n",
            Some(1.0),
            "forged",
        ))
        .unwrap()
        .seq;
    let entries = until(&home, 15, "an alarm citing the forged record", |e| {
        e.iter().any(|e| {
            matches!(&e.record, Record::Report { venture, metric, cites, .. }
                if venture == "self" && metric == "foreign" && cites.contains(&forged))
        })
    });
    let alarm = entries
        .iter()
        .find(|e| matches!(&e.record, Record::Report { metric, .. } if metric == "foreign"))
        .unwrap()
        .seq;
    // Reports are measurements and go on while everything is paused.
    let after = |e: &[Entry]| {
        e.iter()
            .filter(|e| {
                e.seq > alarm
                    && matches!(&e.record, Record::Report { venture, metric, value: Some(x), .. }
                if venture == "v1" && metric == "n" && *x == 5.0)
            })
            .count()
    };
    let entries = until(&home, 20, "reports after the alarm", |e| after(e) >= 3);
    assert!(after(&entries) >= 3, "{}", lines(&entries));
    // The forged record is not the venture's reading: status, digest and
    // know all read the daemon's 5.
    let status = ok(&avsm(tmp.path(), &home, &["status"]));
    let row = status.lines().find(|l| l.starts_with("v1 ")).unwrap();
    assert!(row.contains("n=5"), "{status}");
    d.stop();
}
