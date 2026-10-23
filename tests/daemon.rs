//! The daemon and its conventions: avsmd's flags, its drain on SIGTERM,
//! sysexits codes, the home, and `avsm doctor`, through the real binaries.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use avsmd::kernel::Kernel;
use avsmd::ledger::Ledger;
use avsmd::record::Record;

/// Run a binary with HOME in the temp dir, so no real home is read.
fn run(bin: &str, tmp: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    Command::new(bin)
        .args(args)
        .env("HOME", tmp)
        .env_remove("AVSMD_HOME")
        .env_remove("XDG_STATE_HOME")
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .output()
        .expect("run")
}

fn avsm(tmp: &Path, args: &[&str]) -> Output {
    run(env!("CARGO_BIN_EXE_avsm"), tmp, args, &[])
}

fn avsmd(tmp: &Path, args: &[&str]) -> Output {
    run(env!("CARGO_BIN_EXE_avsmd"), tmp, args, &[])
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

/// Start avsmd and wait for its socket.
fn start(tmp: &Path, args: &[&str], env: &[(&str, &str)], sock: &Path) -> Daemon {
    let child = Command::new(env!("CARGO_BIN_EXE_avsmd"))
        .args(args)
        .env("HOME", tmp)
        .env_remove("AVSMD_HOME")
        .envs(env.iter().copied())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(tmp.join("daemon.log")).unwrap())
        .spawn()
        .expect("spawn avsmd");
    let began = Instant::now();
    while !avsmd::protocol::alive(sock) {
        assert!(
            began.elapsed() < Duration::from_secs(30),
            "avsmd did not start: {}",
            std::fs::read_to_string(tmp.join("daemon.log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    Daemon(child)
}

/// A config that never refuses a run for the night. Forecasts stay on but
/// small: the default, 30 days x 16 seeds, runs in the tick before the
/// first lever and takes tens of seconds in a debug build on a loaded CI
/// runner; these tests time the daemon, not forecast throughput.
const NEVER: &str = "nightly: never\nforecast: {days: 2, seeds: 2}\n";

/// A venture whose lever is `lever` and whose report always reads 11,
/// above its threshold of 10, so the lever runs at once.
fn venture(parent: &Path, lever: &str) -> PathBuf {
    let dir = parent.join("counter");
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
    for (name, body) in [
        ("report", "#!/bin/sh\necho 11\n".to_string()),
        ("reset", format!("#!/bin/sh\n{lever}\n")),
    ] {
        std::fs::write(dir.join(name), body).unwrap();
        std::fs::set_permissions(dir.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

/// A home with the venture adopted and nothing running, and a config.
fn adopted_home(tmp: &Path) -> (PathBuf, PathBuf) {
    let home = tmp.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.yaml"), NEVER).unwrap();
    let dir = venture(tmp, "exit 0");
    Kernel::open(&home).unwrap().add(&dir).unwrap();
    (home, dir)
}

/// Every file under a directory, by path, with its bytes and mode.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, (Vec<u8>, u32)> {
    let mut files = BTreeMap::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let meta = std::fs::symlink_metadata(&p).unwrap();
            if meta.is_dir() {
                todo.push(p.clone());
                files.insert(p, (Vec::new(), meta.permissions().mode()));
            } else {
                let bytes = std::fs::read(&p).unwrap_or_default();
                files.insert(p, (bytes, meta.permissions().mode()));
            }
        }
    }
    files
}

/// The two snapshots hold the same files with the same bytes and modes;
/// else fail naming each path that differs.
fn same(before: &BTreeMap<PathBuf, (Vec<u8>, u32)>, after: &BTreeMap<PathBuf, (Vec<u8>, u32)>) {
    let changed: Vec<String> = before
        .keys()
        .chain(after.keys())
        .filter(|p| before.get(*p) != after.get(*p))
        .map(|p| p.display().to_string())
        .collect();
    assert!(changed.is_empty(), "doctor changed files: {changed:?}");
}

#[test]
fn test_flag_prints_the_teachback_and_fails_a_broken_mandate_with_78() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, dir) = adopted_home(tmp.path());
    let h = home.to_str().unwrap();

    let out = avsmd(tmp.path(), &["--home", h, "-t"]);
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    assert!(stdout.contains("config.yaml: ok"), "{stdout}");
    assert!(stdout.contains("counter from "), "{stdout}");
    assert!(
        stdout.contains("purpose \"Keep the counter below ten.\""),
        "{stdout}"
    );
    assert!(stdout.contains("watching count every 1h"), "{stdout}");
    assert!(stdout.contains("may reset  3/day  reversible"), "{stdout}");

    // The mandate on disk no longer parses: what `avsm add` would refuse.
    std::fs::write(dir.join("mandate.yaml"), "purpose: [unclosed\n").unwrap();
    let out = avsmd(tmp.path(), &["--home", h, "-t"]);
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(78), "{stdout}{stderr}");
    assert!(stderr.contains("counter: "), "{stderr}");
    assert!(stderr.contains("mandate.yaml"), "{stderr}");

    // A config that does not parse is EX_CONFIG too.
    std::fs::write(dir.join("mandate.yaml"), "x").unwrap();
    std::fs::write(home.join("config.yaml"), "nightly: banana\n").unwrap();
    let out = avsmd(tmp.path(), &["--home", h, "-t"]);
    assert_eq!(out.status.code(), Some(78), "{:?}", text(&out));
}

#[test]
fn capital_t_prints_the_config_in_force_and_f_reads_another() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.yaml"), "nightly: \"03:15\"\n").unwrap();
    let h = home.to_str().unwrap();

    let out = avsmd(tmp.path(), &["--home", h, "-T"]);
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    assert!(stdout.contains("nightly: 03:15") || stdout.contains("nightly: '03:15'"));
    // Defaults the file leaves out are in force, and printed.
    assert!(stdout.contains("ratchet: "), "{stdout}");
    assert!(stdout.contains("port: "), "{stdout}");
    // What -T prints reads back as the same config.
    let body: String =
        stdout
            .lines()
            .filter(|l| !l.starts_with('#'))
            .fold(String::new(), |mut s, l| {
                s.push_str(l);
                s.push('\n');
                s
            });
    assert_eq!(
        avsmd::config::Config::parse(&body).unwrap(),
        avsmd::config::Config::load(&home).unwrap()
    );

    let other = tmp.path().join("other.yaml");
    std::fs::write(&other, "nightly: \"04:45\"\n").unwrap();
    let out = avsmd(
        tmp.path(),
        &["--home", h, "-f", other.to_str().unwrap(), "-T"],
    );
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    assert!(stdout.contains("04:45"), "{stdout}");
    assert!(!stdout.contains("03:15"), "{stdout}");
    assert!(
        stdout.contains(&format!("# config {}", other.display())),
        "{stdout}"
    );

    // -f is what -t tests.
    std::fs::write(&other, "nightly: banana\n").unwrap();
    let out = avsmd(
        tmp.path(),
        &["--home", h, "-f", other.to_str().unwrap(), "-t"],
    );
    assert_eq!(out.status.code(), Some(78), "{:?}", text(&out));
    assert!(text(&out).1.contains("other.yaml"), "{:?}", text(&out));

    // A file -f names that is not there is EX_NOINPUT, not defaults.
    let gone = tmp.path().join("gone.yaml");
    let out = avsmd(
        tmp.path(),
        &["--home", h, "-f", gone.to_str().unwrap(), "-T"],
    );
    assert_eq!(out.status.code(), Some(66), "{:?}", text(&out));
}

/// The warden and roles run with the config -f named, not the home's.
#[test]
fn f_reaches_the_warden() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    // The home's config would refuse every run; the chosen one allows it.
    std::fs::write(home.join("config.yaml"), "nightly: banana\n").unwrap();
    let other = tmp.path().join("other.yaml");
    std::fs::write(&other, NEVER).unwrap();
    let sock = avsmd::kernel::socket_path(&home);
    let h = home.to_str().unwrap();
    let _d = start(
        tmp.path(),
        &["--home", h, "-f", other.to_str().unwrap()],
        &[],
        &sock,
    );
    let dir = venture(tmp.path(), "touch ran");
    let out = avsm(tmp.path(), &["--home", h, "add", dir.to_str().unwrap()]);
    assert!(out.status.success(), "{:?}", text(&out));
    let began = Instant::now();
    while !dir.join("ran").exists() {
        assert!(
            began.elapsed() < Duration::from_secs(30),
            "the lever never ran: {}",
            std::fs::read_to_string(tmp.path().join("daemon.log")).unwrap()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn sigterm_during_a_lever_drains_within_the_bound_and_writes_its_outcome() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.yaml"), NEVER).unwrap();
    let sock = avsmd::kernel::socket_path(&home);
    let h = home.to_str().unwrap();
    let mut daemon = start(tmp.path(), &["--home", h], &[], &sock);
    // A lever that ignores SIGTERM and would run a minute: only the
    // drain's SIGKILL ends it.
    let dir = venture(tmp.path(), "trap '' TERM; touch started; sleep 60");
    let out = avsm(tmp.path(), &["--home", h, "add", dir.to_str().unwrap()]);
    assert!(out.status.success(), "{:?}", text(&out));
    let began = Instant::now();
    while !dir.join("started").exists() {
        assert!(
            began.elapsed() < Duration::from_secs(30),
            "the lever never started"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let signalled = Instant::now();
    // SAFETY: kill has no memory preconditions.
    unsafe {
        libc::kill(daemon.0.id() as libc::pid_t, libc::SIGTERM);
    }
    let status = loop {
        if let Some(status) = daemon.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            signalled.elapsed() < Duration::from_secs(20),
            "avsmd still running 20 s after SIGTERM: {}",
            std::fs::read_to_string(tmp.path().join("daemon.log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    let took = signalled.elapsed();
    let log = std::fs::read_to_string(tmp.path().join("daemon.log")).unwrap();
    assert_eq!(status.code(), Some(0), "{log}");
    assert!(
        took <= avsmd::kernel::DRAIN,
        "took {took:?}, over the drain bound\n{log}"
    );
    assert!(!sock.exists(), "the socket outlived the daemon");

    // The intent has its outcome.
    let ledger = Ledger::open_read_only(&home.join("ledger.db")).unwrap();
    let entries = ledger.all().unwrap();
    let intents: Vec<u64> = entries
        .iter()
        .filter(|e| matches!(&e.record, Record::Intent { action, .. } if action == "reset"))
        .map(|e| e.seq)
        .collect();
    assert_eq!(intents.len(), 1, "{log}");
    let outcome = entries.iter().find_map(|e| match &e.record {
        Record::Outcome { intent, ok, .. } if *intent == intents[0] => Some(*ok),
        _ => None,
    });
    assert_eq!(outcome, Some(false), "the intent has no outcome\n{log}");
}

#[test]
fn exit_codes_follow_sysexits() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let h = home.to_str().unwrap();
    // A home that is not there: EX_NOINPUT, and it says so.
    let out = avsm(tmp.path(), &["--home", h, "status"]);
    assert_eq!(out.status.code(), Some(66), "{:?}", text(&out));
    assert!(text(&out).1.contains("no home at"), "{:?}", text(&out));
    std::fs::create_dir_all(&home).unwrap();
    // No daemon: EX_UNAVAILABLE.
    let out = avsm(tmp.path(), &["--home", h, "status"]);
    assert_eq!(out.status.code(), Some(69), "{:?}", text(&out));
    assert!(text(&out)
        .1
        .contains("avsmd is not running; start it with: avsmd"));
    // Bad usage: EX_USAGE, for both binaries.
    for out in [
        avsm(tmp.path(), &["dance"]),
        avsm(tmp.path(), &["tail", "--since"]),
        avsm(tmp.path(), &["tail", "5", "--since", "3"]),
        avsmd(tmp.path(), &["--bogus"]),
        avsmd(tmp.path(), &["-t", "extra"]),
    ] {
        assert_eq!(out.status.code(), Some(64), "{:?}", text(&out));
        assert!(out.stdout.is_empty(), "usage goes to standard error");
    }
    // Help and version are not errors.
    for out in [
        avsm(tmp.path(), &["--help"]),
        avsmd(tmp.path(), &["--help"]),
        avsmd(tmp.path(), &["-V"]),
    ] {
        assert_eq!(out.status.code(), Some(0), "{:?}", text(&out));
    }
    let help = text(&avsmd(tmp.path(), &["--help"])).0;
    for flag in ["-t", "-T", "-f", "-d", "--home"] {
        assert!(help.contains(flag), "avsmd help lacks {flag}:\n{help}");
    }
    assert!(!help.contains("warden"), "{help}");
}

#[test]
fn doctor_passes_a_healthy_home_fails_a_broken_config_and_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, _dir) = adopted_home(tmp.path());
    let h = home.to_str().unwrap();
    let sock = avsmd::kernel::socket_path(&home);

    // Before anything else reads this home: no daemon, so the ledger is
    // closed, and every check must leave every file as it was.
    let before = snapshot(tmp.path());
    let out = avsm(tmp.path(), &["--home", h, "doctor"]);
    let (stdout, _) = text(&out);
    assert_eq!(out.status.code(), Some(69), "{stdout}");
    assert!(
        stdout.contains(&format!("fix: avsmd --home {h}")),
        "{stdout}"
    );
    same(&before, &snapshot(tmp.path()));

    // A broken config: the fix names the file, the code is EX_CONFIG
    // (worse than the daemon being down), and nothing changes.
    std::fs::write(home.join("config.yaml"), "nightly: banana\n").unwrap();
    let before = snapshot(tmp.path());
    let out = avsm(tmp.path(), &["--home", h, "doctor"]);
    let (stdout, _) = text(&out);
    assert_eq!(out.status.code(), Some(78), "{stdout}");
    assert!(
        stdout.contains(&format!(
            "fix: ${{EDITOR:-vi}} {}/config.yaml",
            home.display()
        )),
        "{stdout}"
    );
    same(&before, &snapshot(tmp.path()));
    std::fs::write(home.join("config.yaml"), NEVER).unwrap();

    // Healthy and running.
    {
        let mut d = start(tmp.path(), &["--home", h], &[], &sock);
        let out = avsm(tmp.path(), &["--home", h, "doctor"]);
        let (stdout, stderr) = text(&out);
        assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
        assert!(!stdout.contains("FAIL"), "{stdout}");
        for what in [
            "ok    home ",
            "ok    config ",
            "chain verified",
            "ok    mandate counter",
            "ok    avsmd answering",
            "ok    warden and roles executable",
        ] {
            assert!(stdout.contains(what), "doctor lacks {what:?}:\n{stdout}");
        }
        // SAFETY: kill has no memory preconditions.
        unsafe {
            libc::kill(d.0.id() as libc::pid_t, libc::SIGTERM);
        }
        assert!(d.0.wait().unwrap().success());
    }
    // Stopped, and its warden gone too: the ledger's journal files go
    // with the last connection.
    let began = Instant::now();
    while ["ledger.db-wal", "ledger.db-shm"]
        .iter()
        .any(|f| home.join(f).exists())
    {
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "the warden lingers"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // A 1.0 home left behind is named, with the command that moves it.
    std::fs::write(home.join("config.yaml"), NEVER).unwrap();
    std::fs::create_dir_all(tmp.path().join(".avsm")).unwrap();
    let out = avsm(tmp.path(), &["--home", h, "doctor"]);
    let (stdout, _) = text(&out);
    assert_eq!(out.status.code(), Some(78), "{stdout}");
    assert!(stdout.contains("an avsm 1.0 home is left at"), "{stdout}");
    assert!(stdout.contains("-1.0  #"), "{stdout}");
}

#[test]
fn doctor_names_a_changed_mandate_and_a_broken_ledger() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, dir) = adopted_home(tmp.path());
    let h = home.to_str().unwrap();
    std::fs::write(
        dir.join("mandate.yaml"),
        std::fs::read_to_string(dir.join("mandate.yaml")).unwrap() + "# edited\n",
    )
    .unwrap();
    let out = avsm(tmp.path(), &["--home", h, "doctor"]);
    let (stdout, _) = text(&out);
    assert_eq!(out.status.code(), Some(78), "{stdout}");
    assert!(
        stdout.contains(&format!(
            "fix: avsm add {}",
            dir.canonicalize().unwrap().display()
        )),
        "{stdout}"
    );

    // A ledger edited behind the chain's back: EX_DATAERR, the worst.
    let ledger = home.join("ledger.db");
    tamper(&ledger);
    let out = avsm(tmp.path(), &["--home", h, "doctor"]);
    let (stdout, _) = text(&out);
    assert_eq!(out.status.code(), Some(65), "{stdout}");
    assert!(stdout.contains("ledger.db.broken"), "{stdout}");
}

/// Change a record's time in place, as someone editing the file would.
fn tamper(db: &Path) {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute_batch(
        "DROP TRIGGER records_no_update; UPDATE records SET at = at + 1 WHERE seq = 1;",
    )
    .unwrap();
}

#[test]
fn tail_since_prints_every_record_after_a_number() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, _dir) = adopted_home(tmp.path());
    let h = home.to_str().unwrap();
    let sock = avsmd::kernel::socket_path(&home);
    let _d = start(tmp.path(), &["--home", h], &[], &sock);
    let out = avsm(tmp.path(), &["--home", h, "tail", "--since", "1"]);
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stderr}");
    let seqs: Vec<u64> = stdout
        .lines()
        .map(|l| {
            l.trim_start_matches('#')
                .split_whitespace()
                .next()
                .unwrap()
                .parse()
                .unwrap()
        })
        .collect();
    assert!(!seqs.is_empty(), "{stdout}");
    assert_eq!(seqs[0], 2, "{stdout}");
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "{stdout}");
    let last = *seqs.last().unwrap();
    let out = avsm(
        tmp.path(),
        &["--home", h, "tail", "--since", &format!("#{last}")],
    );
    assert!(out.status.success());
    let (stdout, _) = text(&out);
    assert!(
        stdout.lines().all(|l| !l.starts_with(&format!("#{last} "))),
        "{stdout}"
    );
    // A record number that is not one is the input's fault.
    let out = avsm(tmp.path(), &["--home", h, "tail", "--since", "x"]);
    assert_eq!(out.status.code(), Some(65), "{:?}", text(&out));
}

/// No command colors its output: nothing to turn off with NO_COLOR, and
/// nothing to leak into a pipe.
#[test]
fn no_color_and_pipes_get_no_escape_codes() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, _dir) = adopted_home(tmp.path());
    let h = home.to_str().unwrap();
    let sock = avsmd::kernel::socket_path(&home);
    let _d = start(tmp.path(), &["--home", h], &[], &sock);
    for env in [&[("NO_COLOR", "1")][..], &[]] {
        for args in [
            &["status"][..],
            &["tail"],
            &["asks"],
            &["doctor"],
            &["why", "1"],
            &["nonsense"],
        ] {
            let mut full = vec!["--home", h];
            full.extend_from_slice(args);
            let out = run(env!("CARGO_BIN_EXE_avsm"), tmp.path(), &full, env);
            assert!(
                !out.stdout.contains(&0x1b) && !out.stderr.contains(&0x1b),
                "{args:?} {env:?} wrote an escape code"
            );
        }
        let out = run(
            env!("CARGO_BIN_EXE_avsmd"),
            tmp.path(),
            &["--home", h, "-t"],
            env,
        );
        assert!(!out.stdout.contains(&0x1b) && !out.stderr.contains(&0x1b));
    }
}

#[test]
fn avsmd_home_picks_the_home_and_home_flag_beats_it() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("by-env");
    let env = [("AVSMD_HOME", home.to_str().unwrap())];
    let sock = avsmd::kernel::socket_path(&home);
    let _d = start(tmp.path(), &["-d"], &env, &sock);
    let out = run(env!("CARGO_BIN_EXE_avsm"), tmp.path(), &["status"], &env);
    assert!(out.status.success(), "{:?}", text(&out));
    assert!(text(&out).0.contains("self "), "{:?}", text(&out));
    // AVSM_HOME is 1.0's and means nothing now.
    let out = run(
        env!("CARGO_BIN_EXE_avsm"),
        tmp.path(),
        &["status"],
        &[("AVSM_HOME", home.to_str().unwrap())],
    );
    // The default home is not there: no home, not "not running".
    assert_eq!(out.status.code(), Some(66), "{:?}", text(&out));
    // --home wins over the environment.
    let elsewhere = tmp.path().join("elsewhere");
    let out = run(
        env!("CARGO_BIN_EXE_avsm"),
        tmp.path(),
        &["--home", elsewhere.to_str().unwrap(), "status"],
        &env,
    );
    assert_eq!(out.status.code(), Some(66), "{:?}", text(&out));
    // -d: verbose diagnostics on standard error.
    let began = Instant::now();
    loop {
        let log = std::fs::read_to_string(tmp.path().join("daemon.log")).unwrap();
        if log.contains("tick took") && log.contains("command: status") {
            break;
        }
        assert!(began.elapsed() < Duration::from_secs(10), "{log}");
        std::thread::sleep(Duration::from_millis(100));
    }
}
