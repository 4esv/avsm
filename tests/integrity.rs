//! The ledger's integrity against its owner, a stray INSERT, a lost
//! journal and a second daemon, through the real binaries (#44).

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use avsmd::kernel::Kernel;
use avsmd::ledger::Ledger;
use avsmd::record::{Entry, Record};

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

fn avsm(tmp: &Path, args: &[&str]) -> Output {
    run(env!("CARGO_BIN_EXE_avsm"), tmp, args)
}

fn avsmd(tmp: &Path, args: &[&str]) -> Output {
    run(env!("CARGO_BIN_EXE_avsmd"), tmp, args)
}

fn text(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(tmp: &Path, home: &Path, log: &str) -> Daemon {
    let sock = avsmd::kernel::socket_path(home);
    let child = Command::new(env!("CARGO_BIN_EXE_avsmd"))
        .args(["--home", home.to_str().unwrap()])
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

fn sigterm(d: &mut Daemon) {
    // SAFETY: kill has no memory preconditions.
    unsafe {
        libc::kill(d.0.id() as libc::pid_t, libc::SIGTERM);
    }
    assert!(d.0.wait().unwrap().success());
}

const NEVER: &str = "nightly: never\nforecast: {days: 2, seeds: 2}\n";

/// A home with one venture adopted and nothing running.
fn adopted_home(tmp: &Path) -> PathBuf {
    let home = tmp.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.yaml"), NEVER).unwrap();
    let dir = tmp.join("counter");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mandate.yaml"),
        "purpose: Keep the counter below ten.\n\
         metric: count\n\
         controller:\n  kind: threshold\n  above: 10\n  action: reset\n\
         cadence: 1h\n\
         permitted:\n  - name: reset\n    run: ./reset\n    cap: 3/day\n    reversible: true\n",
    )
    .unwrap();
    for (name, body) in [("report", "#!/bin/sh\necho 1\n"), ("reset", "#!/bin/sh\n")] {
        std::fs::write(dir.join(name), body).unwrap();
        std::fs::set_permissions(dir.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut k = Kernel::open(&home).unwrap();
    k.add(&dir).unwrap();
    for _ in 0..5 {
        k.pause("counter").unwrap();
        k.resume("counter").unwrap();
    }
    home
}

/// The `seq:hash` anchor the digest prints.
fn anchor(digest: &str) -> String {
    let line = digest
        .lines()
        .find(|l| l.starts_with("head "))
        .unwrap_or_else(|| panic!("no head line in the digest:\n{digest}"));
    line["head ".len()..]
        .split(|c: char| c == ';' || c.is_whitespace())
        .next()
        .unwrap()
        .to_string()
}

/// What an owner can do with the file: lift the triggers, change a
/// record, rehash every row after it, and put the triggers back.
fn owner_rewrite(db: &Path, seq: u64) {
    let conn = rusqlite::Connection::open(db).unwrap();
    let triggers: Vec<(String, String)> = conn
        .prepare("SELECT name, sql FROM sqlite_master WHERE type = 'trigger'")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for (name, _) in &triggers {
        conn.execute_batch(&format!("DROP TRIGGER {name}")).unwrap();
    }
    let rows: Vec<(i64, i64, String)> = conn
        .prepare("SELECT seq, at, record FROM records ORDER BY seq")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let mut prev = avsmd::record::GENESIS.to_string();
    for (s, at, text) in rows {
        let at = if s as u64 == seq { at + 3600 } else { at };
        let record: Record = serde_json::from_str(&text).unwrap();
        let e = Entry::new(s as u64, at, &prev, record);
        conn.execute(
            "UPDATE records SET at = ?1, prev = ?2, hash = ?3 WHERE seq = ?4",
            rusqlite::params![e.at, e.prev, e.hash, s],
        )
        .unwrap();
        prev = e.hash;
    }
    for (_, sql) in &triggers {
        conn.execute_batch(sql).unwrap();
    }
}

#[test]
fn the_digest_carries_the_head_and_doctor_since_catches_an_owner_rewrite() {
    let tmp = tempfile::tempdir().unwrap();
    let home = adopted_home(tmp.path());
    let h = home.to_str().unwrap();
    let mut d = start(tmp.path(), &home, "daemon.log");
    let out = avsm(tmp.path(), &["--home", h, "digest"]);
    let (digest, _) = text(&out);
    assert!(out.status.success(), "{digest}");
    sigterm(&mut d);
    let at = anchor(&digest);
    let (seq, _) = at.split_once(':').unwrap();
    let head = Ledger::open(&home.join("ledger.db"))
        .unwrap()
        .get(seq.parse().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(at, format!("{}:{}", head.seq, &head.hash[..16]), "{digest}");
    assert!(
        digest.contains(&format!("avsm doctor --since {at}")),
        "{digest}"
    );

    // Untouched: the anchor holds, by seq:hash and by the bare hash.
    for since in [at.clone(), head.hash.clone(), head.hash[..16].to_string()] {
        let out = avsm(tmp.path(), &["--home", h, "doctor", "--since", &since]);
        let (stdout, _) = text(&out);
        assert!(stdout.contains("ok    anchor "), "{since}: {stdout}");
        assert!(!stdout.contains("anchor") || !stdout.contains("FAIL  anchor"));
        // No daemon is the only failure.
        assert_eq!(out.status.code(), Some(69), "{stdout}");
    }

    // The owner rewrites record 3 and rehashes the chain: it verifies,
    // and only the anchor kept elsewhere tells.
    owner_rewrite(&home.join("ledger.db"), 3);
    let out = avsm(tmp.path(), &["--home", h, "doctor"]);
    assert!(text(&out).0.contains("chain verified"), "{:?}", text(&out));
    let out = avsm(tmp.path(), &["--home", h, "doctor", "--since", &at]);
    let (stdout, _) = text(&out);
    assert_eq!(out.status.code(), Some(65), "{stdout}");
    assert!(stdout.contains("FAIL  anchor "), "{stdout}");
    assert!(stdout.contains("rewritten or rewound"), "{stdout}");

    // A head past the end (a rewind) fails as loudly.
    let out = avsm(
        tmp.path(),
        &["--home", h, "doctor", "--since", "9999:0123456789abcdef"],
    );
    assert_eq!(out.status.code(), Some(65), "{:?}", text(&out));
    // Nonsense is a usage error, not a pass.
    for bad in ["12", "x:y", "3:abc"] {
        let out = avsm(tmp.path(), &["--home", h, "doctor", "--since", bad]);
        assert_eq!(out.status.code(), Some(64), "{bad}: {:?}", text(&out));
    }
}

#[test]
fn doctor_names_a_dropped_trigger() {
    let tmp = tempfile::tempdir().unwrap();
    let home = adopted_home(tmp.path());
    let h = home.to_str().unwrap();

    // Healthy: all three triggers are there, so doctor names none (the
    // daemon not running is the only failure).
    let out = avsm(tmp.path(), &["--home", h, "doctor"]);
    let (stdout, _) = text(&out);
    assert_eq!(out.status.code(), Some(69), "{stdout}");
    assert!(!stdout.contains("missing its trigger"), "{stdout}");

    // The owner drops one trigger: doctor names it, read-only, before
    // anything puts it back.
    let conn = rusqlite::Connection::open(home.join("ledger.db")).unwrap();
    conn.execute_batch("DROP TRIGGER records_no_delete")
        .unwrap();
    drop(conn);
    let out = avsm(tmp.path(), &["--home", h, "doctor"]);
    let (stdout, _) = text(&out);
    assert_eq!(out.status.code(), Some(65), "{stdout}");
    assert!(stdout.contains("records_no_delete"), "{stdout}");
    assert!(stdout.contains("missing its trigger"), "{stdout}");
}

/// The last row's seq and hash, read straight from the file.
fn last(conn: &rusqlite::Connection) -> (i64, String) {
    conn.query_row(
        "SELECT seq, hash FROM records ORDER BY seq DESC LIMIT 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

#[test]
fn an_insert_that_does_not_chain_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let home = adopted_home(tmp.path());
    let conn = rusqlite::Connection::open(home.join("ledger.db")).unwrap();
    let (seq, hash) = last(&conn);
    let insert = |s: i64, prev: &str, h: &str| {
        conn.execute(
            "INSERT INTO records (seq, at, prev, hash, weight, record) \
             VALUES (?1, 0, ?2, ?3, 1, '{\"kind\":\"action\"}')",
            rusqlite::params![s, prev, h],
        )
    };
    // The wrong seq, the wrong prev, a gap: all refused.
    for (s, prev) in [
        (seq, hash.as_str()),
        (seq + 2, hash.as_str()),
        (seq + 1, avsmd::record::GENESIS),
        (seq + 1, "beef"),
    ] {
        let e = insert(s, prev, "x1").unwrap_err().to_string();
        assert!(e.contains("chained"), "{s} {prev}: {e}");
    }
    // The next seq on the last hash is taken: the trigger cannot check
    // the record itself, so this is the row the daemon must name.
    insert(seq + 1, &hash, "x2").unwrap();
    drop(conn);

    let h = home.to_str().unwrap();
    let bad = seq + 1;
    let out = avsmd(tmp.path(), &["--home", h]);
    let (_, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(65), "{stderr}");
    assert!(stderr.contains(&format!("record #{bad}")), "{stderr}");
    assert!(
        stderr.contains(&format!("avsm --home {h} doctor --cut {bad}")),
        "{stderr}"
    );
    let out = avsm(tmp.path(), &["--home", h, "doctor"]);
    let (stdout, _) = text(&out);
    assert_eq!(out.status.code(), Some(65), "{stdout}");
    assert!(stdout.contains(&format!("record #{bad}")), "{stdout}");
    assert!(
        stdout.contains(&format!("fix: avsm --home {h} doctor --cut {bad}")),
        "{stdout}"
    );

    // The cut: a backup holding every row, the rows before kept, the
    // triggers back, and a declared record of the cut on top.
    let out = avsm(
        tmp.path(),
        &["--home", h, "doctor", "--cut", &bad.to_string()],
    );
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    let backup: PathBuf = std::fs::read_dir(&home)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.to_string_lossy().contains("ledger.db.before-cut-"))
        .expect("a backup");
    assert!(stdout.contains(&backup.display().to_string()), "{stdout}");
    let kept = rusqlite::Connection::open(&backup).unwrap();
    assert_eq!(last(&kept).0, bad);
    // The triggers are back before anything else opens the ledger.
    let conn = rusqlite::Connection::open(home.join("ledger.db")).unwrap();
    let triggers: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(triggers, 3);
    assert!(conn
        .execute("DELETE FROM records WHERE seq = 1", [])
        .is_err());
    drop(conn);
    let ledger = Ledger::open(&home.join("ledger.db")).unwrap();
    assert_eq!(ledger.verify().unwrap() as i64, bad);
    let cut = ledger.all().unwrap().pop().unwrap();
    assert_eq!(cut.record.claim(), avsmd::record::Claim::Declared);
    assert!(
        cut.record.summary().contains(&format!("#{bad}")),
        "{}",
        cut.line()
    );
    drop(ledger);
    // And the daemon starts on it.
    let mut d = start(tmp.path(), &home, "daemon.log");
    sigterm(&mut d);
}

#[test]
fn a_cut_refuses_while_avsmd_runs_and_refuses_nonsense() {
    let tmp = tempfile::tempdir().unwrap();
    let home = adopted_home(tmp.path());
    let h = home.to_str().unwrap();
    for bad in ["0", "9999", "x"] {
        let out = avsm(tmp.path(), &["--home", h, "doctor", "--cut", bad]);
        assert_ne!(out.status.code(), Some(0), "{bad}: {:?}", text(&out));
    }
    let n = Ledger::open(&home.join("ledger.db"))
        .unwrap()
        .len()
        .unwrap();
    let mut d = start(tmp.path(), &home, "daemon.log");
    let out = avsm(tmp.path(), &["--home", h, "doctor", "--cut", "3"]);
    let (_, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(75), "{stderr}");
    assert!(stderr.contains("stop avsmd first"), "{stderr}");
    sigterm(&mut d);
    assert!(
        Ledger::open(&home.join("ledger.db"))
            .unwrap()
            .len()
            .unwrap()
            > n
    );
}

fn cut_out(tmp: &Path, home: &Path, args: &[&str]) -> (Option<i32>, String) {
    let mut a = vec!["--home", home.to_str().unwrap(), "doctor"];
    a.extend_from_slice(args);
    let out = Command::new(env!("CARGO_BIN_EXE_avsm"))
        .args(&a)
        .env("HOME", tmp)
        .env("USER", "mallory")
        .env_remove("AVSMD_HOME")
        .output()
        .unwrap();
    let (o, e) = text(&out);
    (out.status.code(), format!("{o}{e}"))
}

#[test]
fn a_cut_needs_a_bad_record_or_force_and_never_erases_a_cut() {
    let tmp = tempfile::tempdir().unwrap();
    let home = adopted_home(tmp.path());
    let n = Ledger::open(&home.join("ledger.db"))
        .unwrap()
        .len()
        .unwrap();
    // A sound ledger is not cut without --force, and nothing is written.
    let (code, out) = cut_out(tmp.path(), &home, &["--cut", "3"]);
    assert_eq!(code, Some(64), "{out}");
    assert!(
        out.contains("no bad record") && out.contains("--force"),
        "{out}"
    );
    assert_eq!(
        Ledger::open(&home.join("ledger.db"))
            .unwrap()
            .len()
            .unwrap(),
        n
    );
    // Record 1 is not cut without --force either.
    let (code, out) = cut_out(tmp.path(), &home, &["--cut", "1"]);
    assert_eq!(code, Some(64), "{out}");

    // With --force it is cut; the account is the uid's, not $USER.
    let (code, out) = cut_out(tmp.path(), &home, &["--cut", "3", "--force"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("kept #1..#2"), "{out}");
    let ledger = Ledger::open(&home.join("ledger.db")).unwrap();
    let note = ledger.all().unwrap().pop().unwrap().record.summary();
    assert!(!note.contains("mallory"), "{note}");
    assert!(note.contains(" by "), "{note}");
    drop(ledger);

    // The cut's own record is #3: a cut at or before it is refused.
    for at in ["3", "2"] {
        let (code, out) = cut_out(tmp.path(), &home, &["--cut", at, "--force"]);
        assert_ne!(code, Some(0), "{at}: {out}");
        assert!(out.contains("earlier cut"), "{at}: {out}");
    }
    let ledger = Ledger::open(&home.join("ledger.db")).unwrap();
    assert_eq!(ledger.len().unwrap(), 3);
    assert!(ledger.all().unwrap()[2]
        .record
        .summary()
        .contains("was cut"));
}

#[test]
fn a_cut_before_record_one_says_so_and_needs_force_even_on_a_bad_ledger() {
    let tmp = tempfile::tempdir().unwrap();
    let home = adopted_home(tmp.path());
    let conn = rusqlite::Connection::open(home.join("ledger.db")).unwrap();
    let (seq, hash) = last(&conn);
    conn.execute(
        "INSERT INTO records (seq, at, prev, hash, weight, record) \
         VALUES (?1, 0, ?2, 'x2', 1, '{\"kind\":\"action\"}')",
        rusqlite::params![seq + 1, hash],
    )
    .unwrap();
    drop(conn);
    let (code, out) = cut_out(tmp.path(), &home, &["--cut", "1"]);
    assert_eq!(code, Some(64), "{out}");
    assert!(out.contains("--force"), "{out}");
    let (code, out) = cut_out(tmp.path(), &home, &["--cut", "1", "--force"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(
        out.contains("kept nothing") && !out.contains("#1..#0"),
        "{out}"
    );
}

/// Run a second avsmd on the home and wait at most ten seconds for it to
/// exit: one that starts is killed and fails the test, not left hanging.
fn bounded(tmp: &Path, h: &str) -> (Option<i32>, String) {
    let log = tmp.join("second.log");
    let mut child = Command::new(env!("CARGO_BIN_EXE_avsmd"))
        .args(["--home", h])
        .env("HOME", tmp)
        .env_remove("AVSMD_HOME")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let began = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if began.elapsed() > Duration::from_secs(10) {
            let _ = Command::new("pkill")
                .args(["-9", "-f", &format!("avsmd --home {h}")])
                .status();
            let _ = child.wait();
            panic!(
                "a second avsmd started on the home: {}",
                std::fs::read_to_string(&log).unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    (
        status.code(),
        std::fs::read_to_string(&log).unwrap_or_default(),
    )
}

#[test]
fn a_second_daemon_exits_75_naming_the_first_which_takes_its_socket_back() {
    let tmp = tempfile::tempdir().unwrap();
    let home = adopted_home(tmp.path());
    let h = home.to_str().unwrap();
    let sock = avsmd::kernel::socket_path(&home);
    let mut first = start(tmp.path(), &home, "first.log");
    let pid = first.0.id();
    std::fs::remove_file(&sock).unwrap();
    let before = Ledger::open(&home.join("ledger.db"))
        .unwrap()
        .all()
        .unwrap()
        .len();

    let (code, stderr) = bounded(tmp.path(), h);
    assert_eq!(code, Some(75), "{stderr}");
    assert!(stderr.contains(&format!("pid {pid}")), "{stderr}");
    assert!(
        !stderr.contains("listening"),
        "the second daemon made a socket"
    );
    // It wrote nothing: no false "did act happen?" ask.
    let entries = Ledger::open(&home.join("ledger.db"))
        .unwrap()
        .all()
        .unwrap();
    assert!(
        !entries[before..]
            .iter()
            .any(|e| e.record.kind() == avsmd::record::Kind::Ask),
        "{:?}",
        entries[before..]
            .iter()
            .map(Entry::line)
            .collect::<Vec<_>>()
    );

    // The first, which holds the lock, listens there again (#52).
    let began = Instant::now();
    while !avsmd::protocol::alive(&sock) {
        assert!(began.elapsed() < Duration::from_secs(5), "no socket again");
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = avsm(tmp.path(), &["--home", h, "doctor"]);
    let (stdout, _) = text(&out);
    assert!(stdout.contains("ok    avsmd answering at"), "{stdout}");
    assert!(stdout.contains("ok    warden pid"), "{stdout}");
    assert!(!stdout.contains("socket is missing"), "{stdout}");

    // A socket put in its place while it holds the lock is no daemon's of
    // this home: it is replaced, and the daemon's own goes with it.
    std::fs::remove_file(&sock).unwrap();
    let other = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let theirs = avsmd::lock::identity(&sock);
    let began = Instant::now();
    while avsmd::lock::identity(&sock) == theirs {
        assert!(began.elapsed() < Duration::from_secs(5), "never taken back");
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(other);
    sigterm(&mut first);
    assert!(!sock.exists());
}

#[test]
fn deleting_the_journal_files_under_a_running_daemon_loses_no_record() {
    let tmp = tempfile::tempdir().unwrap();
    let home = adopted_home(tmp.path());
    let h = home.to_str().unwrap();
    let d = start(tmp.path(), &home, "daemon.log");
    let journals = || -> Vec<PathBuf> {
        std::fs::read_dir(&home)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                let name = p.file_name().unwrap().to_string_lossy().into_owned();
                name.starts_with("ledger.db-")
            })
            .collect()
    };
    for p in journals() {
        std::fs::remove_file(p).unwrap();
    }
    for _ in 0..3 {
        assert!(avsm(tmp.path(), &["--home", h, "pause", "counter"])
            .status
            .success());
        assert!(avsm(tmp.path(), &["--home", h, "resume", "counter"])
            .status
            .success());
    }
    for p in journals() {
        std::fs::remove_file(p).unwrap();
    }
    let out = avsm(tmp.path(), &["--home", h, "tail", "1"]);
    let (stdout, _) = text(&out);
    let seen: u64 = stdout
        .trim_start_matches('#')
        .split_whitespace()
        .next()
        .unwrap()
        .trim_start_matches('#')
        .parse()
        .unwrap_or_else(|_| panic!("{stdout}"));
    // kill -9, the warden and roles at the same moment: a warden left to
    // close its connection would checkpoint the log and hide the loss.
    let killed = Command::new("pkill")
        .args(["-9", "-f", &format!("avsmd --home {h}")])
        .status()
        .unwrap();
    assert!(killed.success());
    drop(d);
    std::thread::sleep(Duration::from_millis(500));
    let ledger = Ledger::open(&home.join("ledger.db")).unwrap();
    let n = ledger.verify().unwrap();
    assert!(n >= seen, "records #{}..#{seen} vanished", n + 1);
}
