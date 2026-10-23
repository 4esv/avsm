//! Text, CLI and doctor papercuts (#54), through the real binaries.

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub fn avsm_out(home: &Path, args: &[&str]) -> (Option<i32>, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_avsm"))
        .arg("--home")
        .arg(home)
        .args(args)
        .output()
        .expect("run avsm");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

pub fn avsm(home: &Path, args: &[&str]) -> (bool, String) {
    let (code, out, err) = avsm_out(home, args);
    (code == Some(0), format!("{out}{err}"))
}

const SMALL_FORECASTS: &str = "forecast: {days: 2, seeds: 2}\n";

pub struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn start(home: &Path) -> Daemon {
    if home.is_dir() && !home.join("config.yaml").exists() {
        std::fs::write(home.join("config.yaml"), SMALL_FORECASTS).unwrap();
    }
    let child = Command::new(env!("CARGO_BIN_EXE_avsmd"))
        .arg("--home")
        .arg(home)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(home.with_extension("log")).unwrap())
        .spawn()
        .expect("spawn avsmd");
    let sock = avsmd::kernel::socket_path(home);
    let began = Instant::now();
    while !avsmd::protocol::alive(&sock) {
        assert!(
            began.elapsed() < Duration::from_secs(20),
            "avsmd did not start: {}",
            std::fs::read_to_string(home.with_extension("log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    Daemon(child)
}

pub fn counter_venture(parent: &Path) -> PathBuf {
    let dir = parent.join("counter");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mandate.yaml"),
        "purpose: Keep the counter below ten.\n\
         metric: count\n\
         controller:\n  kind: threshold\n  above: 10\n  action: reset\n\
         cadence: 30s\n\
         permitted:\n  - name: reset\n    run: ./reset\n    cap: 3/day\n    reversible: true\n",
    )
    .unwrap();
    std::fs::write(dir.join("report"), "#!/bin/sh\necho 3\n").unwrap();
    std::fs::write(dir.join("reset"), "#!/bin/sh\ntrue\n").unwrap();
    for f in ["report", "reset"] {
        std::fs::set_permissions(dir.join(f), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

#[test]
fn a_closed_pipe_ends_the_client_quietly() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let _daemon = start(&home);
    let dir = counter_venture(tmp.path());
    assert!(avsm(&home, &["add", dir.to_str().unwrap()]).0);
    // A reader that is already gone: every write to stdout is a broken pipe.
    for args in [["tail", "1000"], ["status", ""]] {
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader);
        let mut child = Command::new(env!("CARGO_BIN_EXE_avsm"))
            .arg("--home")
            .arg(&home)
            .args(args.iter().filter(|a| !a.is_empty()))
            .stdout(writer)
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut err = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut err)
            .unwrap();
        let status = child.wait().unwrap();
        assert!(
            !err.contains("panicked") && !err.contains("Broken pipe"),
            "{args:?}: {err}"
        );
        assert_eq!(status.code(), Some(0), "{args:?}: {err}");
    }
}

#[test]
fn text_a_model_wrote_is_escaped_where_it_is_shown() {
    let shown = avsmd::protocol::display("a\u{1b}[2Jb\u{202e}c\u{2066}d\u{200b}e\u{feff}\tf\n");
    assert_eq!(
        shown,
        "a\\x1b[2Jb\\u{202e}c\\u{2066}d\\u{200b}e\\u{feff}\tf\n"
    );
    for n in (0x202a..=0x202e).chain(0x2066..=0x2069) {
        let c = char::from_u32(n).unwrap().to_string();
        assert!(!avsmd::protocol::display(&c).contains(&c), "{n:x}");
    }
    assert_eq!(avsmd::protocol::display("naïve café ✓"), "naïve café ✓");
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn a_new_home_and_ledger_are_for_their_owner_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("fresh");
    let _daemon = start(&home);
    let dir = counter_venture(tmp.path());
    assert!(avsm(&home, &["add", dir.to_str().unwrap()]).0);
    assert_eq!(mode(&home), 0o700);
    assert_eq!(mode(&home.join("ledger.db")), 0o600);
    let journal = home.join("ledger.db-journal");
    assert!(journal.exists(), "the journal is the file to check");
    assert_eq!(mode(&journal), 0o600);
}

#[test]
fn an_existing_home_and_ledger_are_left_as_they_are_and_doctor_warns() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("old");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).unwrap();
    {
        let _ = avsmd::kernel::Kernel::open(&home).unwrap();
    }
    let ledger = home.join("ledger.db");
    std::fs::set_permissions(&ledger, std::fs::Permissions::from_mode(0o644)).unwrap();
    let (_, out, _) = avsm_out(&home, &["doctor"]);
    assert!(
        out.contains("warn  ") && out.contains("ledger.db is mode 644"),
        "{out}"
    );
    assert!(out.contains("fix: chmod 600"), "{out}");
    assert_eq!(mode(&ledger), 0o644, "doctor changes nothing");
    assert_eq!(mode(&home), 0o755);
    for m in [0o640, 0o604] {
        std::fs::set_permissions(&ledger, std::fs::Permissions::from_mode(m)).unwrap();
        let (_, out, _) = avsm_out(&home, &["doctor"]);
        assert!(
            out.contains(&format!("ledger.db is mode {m:o}")),
            "{m:o}: {out}"
        );
    }
    std::fs::set_permissions(&ledger, std::fs::Permissions::from_mode(0o600)).unwrap();
    let (_, out, _) = avsm_out(&home, &["doctor"]);
    assert!(!out.contains("warn  "), "{out}");
}

#[test]
fn why_an_answered_ask_names_the_decision_that_answered_it() {
    use avsmd::kernel::Kernel;
    use avsmd::record::{Claim, Packet, Record};
    let tmp = tempfile::tempdir().unwrap();
    let mut k = Kernel::open(&tmp.path().join("home")).unwrap();
    let ask = |action: &str| Record::Ask {
        packet: Some(Packet {
            premises: vec![],
            options: vec![],
            reason: None,
            policies: vec![],
        }),
        cites: vec![],
        claim: Claim::Inferred,
        venture: "counter".to_string(),
        action: Some(action.to_string()),
        question: format!("may I {action}?"),
        refusal: None,
        signal: None,
        payload: None,
    };
    let first = k.append(ask("deploy")).unwrap().seq;
    let second = k.append(ask("rollback")).unwrap().seq;
    let third = k.append(ask("restart")).unwrap().seq;
    let say = |k: &mut Kernel, args: &[&str]| {
        k.handle(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            .unwrap()
    };
    let open = say(&mut k, &["why", &first.to_string()]);
    assert!(open.contains("Unanswered means no."), "{open}");
    let approved = say(&mut k, &["approve", &first.to_string()]);
    let denied = say(&mut k, &["deny", &second.to_string(), "too", "risky"]);
    let seq_of = |line: &str| -> String {
        line.trim_start_matches('#')
            .split_whitespace()
            .next()
            .unwrap()
            .to_string()
    };
    let why = say(&mut k, &["why", &first.to_string()]);
    assert!(!why.contains("Unanswered means no."), "{why}");
    assert!(
        why.contains(&format!(
            "Answered by #{}: approved #{first}",
            seq_of(&approved)
        )),
        "{why}"
    );
    let why = say(&mut k, &["why", &second.to_string()]);
    assert!(
        why.contains(&format!(
            "Answered by #{}: denied #{second}: too risky",
            seq_of(&denied)
        )),
        "{why}"
    );
    let why = say(&mut k, &["why", &third.to_string()]);
    assert!(why.contains("Unanswered means no."), "{why}");
}

fn venture_with(parent: &Path, name: &str, report: &[u8], mode: u32) -> PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mandate.yaml"),
        "purpose: x\nmetric: count\ncontroller: {kind: threshold, above: 10, action: reset}\n\
         cadence: 1h\npermitted:\n  - {name: reset, run: ./reset, cap: 3/day, reversible: true}\n",
    )
    .unwrap();
    std::fs::write(dir.join("reset"), "#!/bin/sh\ntrue\n").unwrap();
    std::fs::set_permissions(dir.join("reset"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(dir.join("report"), report).unwrap();
    std::fs::set_permissions(dir.join("report"), std::fs::Permissions::from_mode(mode)).unwrap();
    dir
}

#[test]
fn a_report_that_cannot_run_says_it_did_not_run_why_and_the_fix() {
    use avsmd::kernel::Kernel;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let mut k = Kernel::open(&home).unwrap();
    let noexec = venture_with(tmp.path(), "noexec", b"#!/bin/sh\necho 3\n", 0o644);
    let crlf = venture_with(tmp.path(), "crlf", b"#!/bin/sh\r\necho 3\r\n", 0o755);
    let added = k.add(&noexec).unwrap();
    assert!(
        added.contains("warning: report is not executable") && added.contains("chmod +x"),
        "{added}"
    );
    let added = k.add(&crlf).unwrap();
    assert!(
        added.contains("carriage return") && added.contains("perl -pi"),
        "{added}"
    );
    k.report("noexec", false);
    k.report("crlf", false);
    let lines: Vec<String> = k.entries().unwrap().iter().map(|e| e.line()).collect();
    let noexec_line = lines
        .iter()
        .find(|l| l.contains("noexec") && l.contains("report did not run"))
        .unwrap_or_else(|| panic!("{lines:#?}"));
    assert!(noexec_line.contains("permission denied") && noexec_line.contains("chmod +x"));
    let crlf_line = lines
        .iter()
        .find(|l| l.contains("crlf") && l.contains("report did not run"))
        .unwrap_or_else(|| panic!("{lines:#?}"));
    assert!(crlf_line.contains("Windows line ending") && crlf_line.contains("perl -pi"));
    for l in [noexec_line, crlf_line] {
        assert!(!l.contains("exit -1") && !l.contains("os error"), "{l}");
    }
    drop(k);
    // Doctor names the same fixes.
    let (_, out, _) = avsm_out(&home, &["doctor"]);
    assert!(out.contains("fix: chmod +x "), "{out}");
    assert!(out.contains("fix: perl -pi -e 's/\\r$//' "), "{out}");
    assert!(
        !out.contains("create it in") || !out.contains("noexec: report"),
        "{out}"
    );
}

#[test]
fn a_lever_that_exists_without_the_execute_bit_gets_chmod_not_create() {
    use avsmd::kernel::Kernel;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let mut k = Kernel::open(&home).unwrap();
    let dir = venture_with(tmp.path(), "lev", b"#!/bin/sh\necho 3\n", 0o755);
    k.add(&dir).unwrap();
    std::fs::set_permissions(dir.join("reset"), std::fs::Permissions::from_mode(0o644)).unwrap();
    drop(k);
    let (_, out, _) = avsm_out(&home, &["doctor"]);
    assert!(
        out.contains("lever reset runs ./reset, which is there but not executable"),
        "{out}"
    );
    assert!(out.contains("fix: chmod +x "), "{out}");
    assert!(!out.contains("fix: create it"), "{out}");
}

#[test]
fn doctor_stops_failing_a_moved_directory_once_its_venture_is_paused() {
    use avsmd::kernel::Kernel;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let mut k = Kernel::open(&home).unwrap();
    let dir = venture_with(tmp.path(), "moved", b"#!/bin/sh\necho 3\n", 0o755);
    k.add(&dir).unwrap();
    std::fs::rename(&dir, tmp.path().join("elsewhere")).unwrap();
    let (_, out, _) = avsm_out(&home, &["doctor"]);
    assert!(
        out.contains("FAIL  moved: ") && out.contains("fix: avsm pause moved"),
        "{out}"
    );
    k.pause("moved").unwrap();
    let (_, out, _) = avsm_out(&home, &["doctor"]);
    assert!(!out.contains("FAIL  moved"), "{out}");
    assert!(out.contains("note  moved is paused"), "{out}");
    k.resume("moved").unwrap();
    let (_, out, _) = avsm_out(&home, &["doctor"]);
    assert!(out.contains("FAIL  moved: "), "{out}");
}

#[test]
fn a_missing_record_number_hints_at_zsh_comments() {
    let tmp = tempfile::tempdir().unwrap();
    for cmd in ["approve", "deny", "why"] {
        let (code, _, err) = avsm_out(tmp.path(), &[cmd]);
        assert_eq!(code, Some(64), "{cmd}: {err}");
        assert!(
            err.contains("hint: zsh treats # as a comment; write 12 or '#12'"),
            "{cmd}: {err}"
        );
    }
    let (_, _, err) = avsm_out(tmp.path(), &["pause"]);
    assert!(!err.contains("zsh"), "{err}");
}
