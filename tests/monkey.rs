//! Small bugs found by simulated users before 2.0 (#42, #47): what the
//! clients say and how they exit, not what the daemon decides.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use avsmd::kernel::Kernel;
use avsmd::record::{Claim, Kind, Record};
use avsmd::role::{Context, InferRequest, Role};
use avsmd::warden::LocalWarden;

static CLOCK: Mutex<()> = Mutex::new(());
const T0: i64 = 1_800_000_000;

fn clock() -> MutexGuard<'static, ()> {
    let guard = CLOCK.lock().unwrap_or_else(|e| e.into_inner());
    avsmd::clock::set(T0);
    guard
}

struct NoRole;

impl Role for NoRole {
    fn decide(
        &mut self,
        _ctx: &Context,
        _infer: &mut dyn FnMut(&InferRequest) -> Result<String, String>,
        _drill: &mut dyn FnMut(&str, &str) -> Result<String, String>,
    ) -> Result<Vec<Record>, String> {
        Ok(vec![])
    }
}

fn script(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn kernel(tmp: &Path, config: &str) -> Kernel {
    let home = tmp.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.yaml"),
        format!("nightly: never\ndefault: k\nbackends:\n  k: {{kind: command, run: echo nothing}}\n{config}"),
    )
    .unwrap();
    let warden = LocalWarden::open(&home).unwrap();
    let mut k = Kernel::with(&home, Box::new(NoRole), Box::new(warden)).unwrap();
    k.adopt_self().unwrap();
    k
}

fn venture(parent: &Path, name: &str, mandate: &str) -> PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("mandate.yaml"), mandate).unwrap();
    script(&dir, "report", "echo 12");
    dir
}

fn handle(k: &mut Kernel, args: &[&str]) -> Result<String, String> {
    k.handle(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
}

const COUNTER: &str = "purpose: Keep the counter low.\nmetric: count\n\
     controller: {kind: threshold, above: 10, action: reset}\ncadence: 1h\n\
     permitted:\n- {name: reset, run: ./reset, cap: 3/day, reversible: true}\n";

fn ask(venture: &str, action: &str) -> Record {
    Record::Ask {
        packet: None,
        cites: vec![],
        claim: Claim::Inferred,
        venture: venture.to_string(),
        action: Some(action.to_string()),
        question: format!("may I {action}?"),
        refusal: None,
        signal: None,
        payload: None,
    }
}

// ----- 2: status shows the metric -----

#[test]
fn status_shows_the_metrics_latest_reading_not_the_latest_record() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kernel(tmp.path(), "");
    let dir = venture(tmp.path(), "counter", COUNTER);
    script(&dir, "reset", "true");
    k.add(&dir).unwrap();
    k.report("counter", false);
    avsmd::clock::advance(60);
    k.append(Record::report(
        Claim::Measured,
        "counter",
        "guard.pressure",
        Some(10.0),
        "",
    ))
    .unwrap();
    k.append(Record::report(
        Claim::Measured,
        "counter",
        "expectation",
        Some(17.0),
        "",
    ))
    .unwrap();
    let status = k.status().unwrap();
    let line = status.lines().find(|l| l.starts_with("counter")).unwrap();
    assert!(line.contains("count=12"), "{line}");
    assert!(!line.contains("guard.pressure"), "{line}");
    assert!(!line.contains("expectation"), "{line}");
}

// ----- 5: why on a simulation decision -----

#[test]
fn why_does_not_put_a_simulation_over_the_tailnet() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kernel(tmp.path(), "");
    let a = k.append(ask("counter", "deploy")).unwrap().seq;
    let answered = k
        .handle_as(&["deny".to_string(), a.to_string()], Some("simulation"))
        .unwrap();
    let d: u64 = answered
        .trim_start_matches('#')
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let why = handle(&mut k, &["why", &d.to_string()]).unwrap();
    assert!(why.contains("declared by simulation"), "{why}");
    assert!(!why.contains("tailnet"), "{why}");
    // A login still is over the tailnet.
    let b = k.append(ask("counter", "rollback")).unwrap().seq;
    let answered = k
        .handle_as(&["deny".to_string(), b.to_string()], Some("axel@github"))
        .unwrap();
    let d: u64 = answered
        .trim_start_matches('#')
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let why = handle(&mut k, &["why", &d.to_string()]).unwrap();
    assert!(why.contains("axel@github, over the tailnet"), "{why}");
}

// ----- 9: tail 0 -----

#[test]
fn tail_zero_is_refused_not_called_an_empty_ledger() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kernel(tmp.path(), "");
    let err = handle(&mut k, &["tail", "0"]).unwrap_err();
    assert!(err.contains("1 or more"), "{err}");
    assert!(!err.contains("empty"), "{err}");
    assert!(handle(&mut k, &["tail", "1"]).is_ok());
}

// ----- 10: an answered ask -----

#[test]
fn an_answered_ask_says_what_answered_it() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kernel(tmp.path(), "");
    let a = k.append(ask("counter", "deploy")).unwrap().seq;
    let b = k.append(ask("counter", "rollback")).unwrap().seq;
    let denied = handle(&mut k, &["deny", &a.to_string(), "no"]).unwrap();
    let by_a: u64 = denied
        .trim_start_matches('#')
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let approved = handle(&mut k, &["approve", &b.to_string()]).unwrap();
    let by_b: u64 = approved
        .trim_start_matches('#')
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    for (ask, by, word) in [(a, by_a, "denied"), (b, by_b, "approved")] {
        for args in [["approve", &ask.to_string()], ["deny", &ask.to_string()]] {
            let err = handle(&mut k, &args).unwrap_err();
            assert!(
                err.starts_with(&format!("#{ask} was already {word} by #{by}")),
                "{err}"
            );
        }
    }
    // A record that is no ask at all keeps the plain words.
    let err = handle(&mut k, &["approve", "1"]).unwrap_err();
    assert!(err.starts_with("#1 is not an open ask"), "{err}");
}

// ----- 11, 12, 14: what add says -----

#[test]
fn add_warns_of_a_lever_program_that_is_not_there() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kernel(tmp.path(), "");
    let dir = venture(
        tmp.path(),
        "counter",
        &COUNTER.replace("./reset", "./resett"),
    );
    script(&dir, "reset", "true");
    let out = k.add(&dir).unwrap();
    assert!(
        out.contains("warning: lever reset runs ./resett, which is not an executable file"),
        "{out}"
    );
    // A program that is there, and a shell line, are not warned about.
    let tmp2 = tempfile::tempdir().unwrap();
    let mut k2 = kernel(tmp2.path(), "");
    let dir = venture(tmp2.path(), "counter", COUNTER);
    script(&dir, "reset", "true");
    let out = k2.add(&dir).unwrap();
    assert!(!out.contains("warning"), "{out}");
    let tmp3 = tempfile::tempdir().unwrap();
    let mut k3 = kernel(tmp3.path(), "");
    let dir = venture(
        tmp3.path(),
        "counter",
        &COUNTER.replace("./reset", "echo hi >> out"),
    );
    let out = k3.add(&dir).unwrap();
    assert!(!out.contains("warning"), "{out}");
}

#[test]
fn a_cap_spent_by_failed_runs_says_the_runs_failed() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kernel(tmp.path(), "");
    let dir = venture(
        tmp.path(),
        "counter",
        &COUNTER
            .replace("./reset", "./resett")
            .replace("3/day", "1/day"),
    );
    k.add(&dir).unwrap();
    k.report("counter", false);
    avsmd::clock::advance(3_600);
    k.report("counter", false);
    let entries = k.entries().unwrap();
    let question = entries
        .iter()
        .find_map(|e| match &e.record {
            Record::Ask {
                signal: Some(avsmd::record::Signal::Saturated { .. }),
                question,
                ..
            } => Some(question.clone()),
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!(
                "no saturation ask: {:#?}",
                entries.iter().map(|e| e.line()).collect::<Vec<_>>()
            )
        });
    assert!(
        question.contains("run in the window failed") && question.contains("fix reset"),
        "{question}"
    );
    assert!(
        !question.contains("the limits cannot keep up"),
        "{question}"
    );
}

#[test]
fn add_warns_of_a_parent_that_is_not_a_venture_yet_but_adds() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kernel(tmp.path(), "");
    let dir = venture(
        tmp.path(),
        "counter",
        &format!("{COUNTER}parent: nosuchparent\n"),
    );
    script(&dir, "reset", "true");
    let out = k.add(&dir).unwrap();
    assert!(
        out.contains("warning: nosuchparent is not a venture yet"),
        "{out}"
    );
    assert!(out.contains("added counter"), "{out}");
}

#[test]
fn add_says_when_config_changed_on_disk_and_names_the_backend_by_the_article() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kernel(tmp.path(), "");
    let mandate = "purpose: Watch.\nmetric: count\n\
         controller: {kind: model, backend: envcheck}\ncadence: 1h\n\
         permitted:\n- {name: reset, run: echo hi, cap: 3/day, reversible: true}\n";
    let dir = venture(tmp.path(), "watcher", mandate);
    // Refused: the config in force has no such backend.
    let err = k.add(&dir).unwrap_err();
    assert!(err.contains("envcheck backend"), "{err}");
    assert!(err.contains("add it under backends"), "{err}");
    // The file gets it; the daemon does not know until restarted.
    std::fs::write(
        tmp.path().join("home/config.yaml"),
        "nightly: never\ndefault: k\nbackends:\n  k: {kind: command, run: echo nothing}\n  envcheck: {kind: command, run: echo nothing}\n",
    )
    .unwrap();
    let err = k.add(&dir).unwrap_err();
    assert!(
        err.contains("config.yaml changed on disk; restart avsmd to apply it"),
        "{err}"
    );
    // A mandate that needs nothing new still adds, with the warning.
    let dir = venture(tmp.path(), "counter", COUNTER);
    script(&dir, "reset", "true");
    let out = k.add(&dir).unwrap();
    assert!(
        out.contains("warning: config.yaml changed on disk; restart avsmd to apply it"),
        "{out}"
    );
}

// ----- 4: propose exits 65 when refused -----

#[test]
fn a_refused_proposal_is_an_error_but_still_recorded() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kernel(tmp.path(), "");
    let dir = venture(tmp.path(), "counter", COUNTER);
    script(&dir, "reset", "true");
    k.add(&dir).unwrap();
    handle(&mut k, &["pause", "counter"]).unwrap();
    let before = k.entries().unwrap().len();
    let err = handle(&mut k, &["propose", "counter", "reset"]).unwrap_err();
    assert!(err.contains("counter: reset refused"), "{err}");
    let kinds: Vec<Kind> = k.entries().unwrap()[before..]
        .iter()
        .map(|e| e.record.kind())
        .collect();
    assert!(kinds.contains(&Kind::Proposal), "{kinds:?}");
}

// ----- the clients -----

fn avsm(home: &Path, args: &[&str]) -> (Option<i32>, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_avsm"))
        .arg("--home")
        .arg(home)
        .args(args)
        .output()
        .expect("run avsm");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code(), text)
}

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(home: &Path) -> Daemon {
    let child = Command::new(env!("CARGO_BIN_EXE_avsmd"))
        .arg("--home")
        .arg(home)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn avsmd");
    let sock = avsmd::kernel::socket_path(home);
    let began = Instant::now();
    while !avsmd::protocol::alive(&sock) {
        assert!(
            began.elapsed() < Duration::from_secs(20),
            "avsmd did not start"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    Daemon(child)
}

const SMALL: &str = "nightly: never\nforecast: {days: 2, seeds: 2}\n";

#[test]
fn a_mistyped_home_says_there_is_no_home() {
    let tmp = tempfile::tempdir().unwrap();
    let nowhere = tmp.path().join("nothere");
    let (code, text) = avsm(&nowhere, &["status"]);
    assert_eq!(code, Some(66), "{text}");
    assert!(
        text.contains(&format!("no home at {}", nowhere.display())),
        "{text}"
    );
    assert!(
        !text.contains("start it with: avsmd\n") || text.contains("--home"),
        "{text}"
    );
    // A home that exists with no daemon keeps the old words.
    let (code, text) = avsm(tmp.path(), &["status"]);
    assert_eq!(code, Some(69), "{text}");
    assert!(
        text.contains("avsmd is not running; start it with: avsmd"),
        "{text}"
    );
}

fn long_home(tmp: &Path) -> PathBuf {
    let home = tmp
        .join("a-home-with-a-long-name-for-the-socket")
        .join("and-another-long-directory-name-too")
        .join("avsmd-home-dir");
    std::fs::create_dir_all(&home).unwrap();
    home
}

#[test]
fn a_socket_path_too_long_is_flagged_by_the_client_by_t_and_by_doctor() {
    let tmp = tempfile::tempdir().unwrap();
    let home = long_home(tmp.path());
    let sock = avsmd::kernel::socket_path(&home);
    let n = sock.as_os_str().len();
    let limit = avsmd::protocol::socket_limit();
    assert!(n > limit, "fixture too short: {n} <= {limit}");
    let want = format!(
        "use a shorter --home or AVSMD_HOME (the socket path is {n} bytes, the limit is {limit})"
    );

    let (code, text) = avsm(&home, &["status"]);
    assert_eq!(code, Some(64), "{text}");
    assert!(text.contains(&want), "{text}");

    let out = Command::new(env!("CARGO_BIN_EXE_avsmd"))
        .arg("--home")
        .arg(&home)
        .arg("-t")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "-t passed a socket path that is too long"
    );
    assert!(text.contains(&want), "{text}");

    let (code, text) = avsm(&home, &["doctor"]);
    assert!(code != Some(0), "{text}");
    assert!(text.contains(&want), "{text}");
    // The fix is not the command that fails.
    let fix = text
        .lines()
        .skip_while(|l| !l.contains("use a shorter --home"))
        .find(|l| l.contains("fix:"))
        .unwrap();
    assert!(
        !fix.contains(&format!("avsmd --home {}", home.display())),
        "{fix}"
    );

    let out = Command::new(env!("CARGO_BIN_EXE_avsmd"))
        .arg("--home")
        .arg(&home)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains(&want));
}

#[test]
fn a_run_that_is_not_there_fails_doctor_and_t() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.yaml"), SMALL).unwrap();
    let dir = venture(
        tmp.path(),
        "counter",
        &COUNTER.replace("./reset", "./resett"),
    );
    {
        let _d = start(&home);
        let (code, text) = avsm(&home, &["add", dir.to_str().unwrap()]);
        assert_eq!(code, Some(0), "{text}");
        assert!(
            text.contains("warning: lever reset runs ./resett"),
            "{text}"
        );
        let (code, text) = avsm(&home, &["doctor"]);
        assert_eq!(code, Some(78), "{text}");
        assert!(
            text.contains("FAIL  counter: lever reset runs ./resett"),
            "{text}"
        );
    }
    let out = Command::new(env!("CARGO_BIN_EXE_avsmd"))
        .arg("--home")
        .arg(&home)
        .arg("-t")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(78));
    assert!(String::from_utf8_lossy(&out.stderr).contains("lever reset runs ./resett"));
    // Put the program there, and both pass the check.
    script(&dir, "resett", "true");
    let out = Command::new(env!("CARGO_BIN_EXE_avsmd"))
        .arg("--home")
        .arg(&home)
        .arg("-t")
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn control_characters_in_a_reading_reach_no_terminal() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.yaml"), SMALL).unwrap();
    let dir = venture(tmp.path(), "evil", COUNTER);
    script(&dir, "reset", "true");
    script(
        &dir,
        "report",
        "printf '\\033[2J\\033]0;PWNED\\007'; exit 1",
    );
    let _d = start(&home);
    let (code, text) = avsm(&home, &["add", dir.to_str().unwrap()]);
    assert_eq!(code, Some(0), "{text}");
    let began = Instant::now();
    loop {
        let (_, tail) = avsm(&home, &["tail", "50"]);
        if tail.contains("PWNED") {
            break;
        }
        assert!(
            began.elapsed() < Duration::from_secs(20),
            "no report: {tail}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    for args in [
        vec!["status"],
        vec!["tail", "50"],
        vec!["tail", "--since", "0"],
    ] {
        let (_, text) = avsm(&home, &args);
        assert!(
            !text
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t'),
            "{args:?} printed a control character: {text:?}"
        );
    }
    let (_, tail) = avsm(&home, &["tail", "50"]);
    assert!(tail.contains("\\x1b[2J\\x1b]0;PWNED\\x07"), "{tail}");
    // The ledger keeps the bytes as they came.
    let raw = avsmd::ledger::Ledger::open_read_only(&home.join("ledger.db"))
        .unwrap()
        .all()
        .unwrap();
    assert!(
        raw.iter().any(|e| e.line().contains('\u{1b}')),
        "the ledger was changed"
    );
}

#[test]
fn display_escapes_what_a_terminal_would_act_on_and_nothing_else() {
    use avsmd::protocol::display;
    assert_eq!(display("a\tb\nc é ✓"), "a\tb\nc é ✓");
    assert_eq!(display("\u{1b}[2J"), "\\x1b[2J");
    assert_eq!(display("\u{7}\u{0}\r"), "\\x07\\x00\\x0d");
    assert_eq!(display("\u{9b}31m"), "\\x9b31m");
    assert_eq!(display("\u{2028}"), "\u{2028}");
}

#[test]
fn propose_exits_65_when_refused_and_0_when_it_goes_through() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.yaml"), SMALL).unwrap();
    let dir = venture(tmp.path(), "counter", COUNTER);
    script(&dir, "reset", "true");
    let _d = start(&home);
    assert_eq!(avsm(&home, &["add", dir.to_str().unwrap()]).0, Some(0));
    let (code, text) = avsm(&home, &["propose", "counter", "reset"]);
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(avsm(&home, &["pause", "counter"]).0, Some(0));
    let (code, text) = avsm(&home, &["propose", "counter", "reset"]);
    assert_eq!(code, Some(65), "{text}");
    assert!(text.contains("refused"), "{text}");
    // And a tail of nothing is a usage error, not an empty ledger.
    let (code, text) = avsm(&home, &["tail", "0"]);
    assert_eq!(code, Some(65), "{text}");
    assert!(!text.contains("empty"), "{text}");
}

#[test]
fn network_none_names_the_system_it_is_not_enforced_on() {
    if cfg!(target_os = "linux") {
        return;
    }
    let note = avsmd::kernel::network_none_note();
    assert!(note.contains("macOS") || !note.contains("Linux"), "{note}");
    assert!(!note.contains("this Linux"), "{note}");
}
