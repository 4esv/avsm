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
fn a_guard_that_is_not_executable_gets_the_chmod_fix_with_a_clean_path() {
    use avsmd::kernel::Kernel;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    struct Quiet;
    impl avsmd::role::Role for Quiet {
        fn decide(
            &mut self,
            _: &avsmd::role::Context,
            _: &mut dyn FnMut(&avsmd::role::InferRequest) -> Result<String, String>,
            _: &mut dyn FnMut(&str, &str) -> Result<String, String>,
        ) -> Result<Vec<avsmd::record::Record>, String> {
            Ok(vec![])
        }
    }
    let warden = avsmd::warden::LocalWarden::open(&home).unwrap();
    let mut k = Kernel::with(&home, Box::new(Quiet), Box::new(warden)).unwrap();
    let dir = venture_with(tmp.path(), "gx", b"#!/bin/sh\necho 3\n", 0o755);
    let mut mandate = std::fs::read_to_string(dir.join("mandate.yaml")).unwrap();
    mandate.push_str("guards:\n  - {name: g, run: ./g, above: 5}\n");
    std::fs::write(dir.join("mandate.yaml"), mandate).unwrap();
    std::fs::write(dir.join("g"), "#!/bin/sh\necho 0\n").unwrap();
    std::fs::set_permissions(dir.join("g"), std::fs::Permissions::from_mode(0o644)).unwrap();
    let added = k.add(&dir).unwrap();
    assert!(added.contains("chmod +x "), "{added}");
    assert!(!added.contains("/./"), "{added}");
    k.report("gx", false);
    let lines: Vec<String> = k.entries().unwrap().iter().map(|e| e.line()).collect();
    let line = lines
        .iter()
        .find(|l| l.contains("guard.g failed"))
        .unwrap_or_else(|| panic!("{lines:#?}"));
    let want = format!(
        "fix: chmod +x {}",
        dir.canonicalize().unwrap().join("g").display()
    );
    assert!(line.contains(&want), "{line}");
    assert!(!line.contains("/./"), "{line}");
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

#[test]
fn a_stray_hash_that_arrives_as_an_argument_hints_at_zsh() {
    // zsh without interactive_comments passes `# stop it` as arguments.
    let tmp = tempfile::tempdir().unwrap();
    let (code, _, err) = avsm_out(tmp.path(), &["pause", "water", "#", "stop", "it"]);
    assert_eq!(code, Some(64), "{err}");
    assert!(err.contains("hint: '#' arrived as an argument"), "{err}");
}

#[test]
fn remove_rm_and_delete_say_there_is_no_remove_and_how_to_stop_one() {
    // Monkey round 3: clap offered "did you mean resume" for these.
    let tmp = tempfile::tempdir().unwrap();
    for word in ["remove", "rm", "delete"] {
        let (code, _, err) = avsm_out(tmp.path(), &[word, "w2"]);
        assert_eq!(code, Some(64), "{word}: {err}");
        assert!(
            err.contains(&format!(
                "there is no {word}: a venture stays in the ledger; stop it with avsm pause <venture>"
            )),
            "{word}: {err}"
        );
        assert!(!err.contains("resume"), "{word}: {err}");
    }
}

#[test]
fn list_stop_and_logs_name_the_command_that_does_it() {
    let tmp = tempfile::tempdir().unwrap();
    for (word, tip) in [
        ("list", "see every venture with avsm status"),
        (
            "stop",
            "stop a venture with avsm pause <venture> (avsmd itself stops on SIGTERM)",
        ),
        ("logs", "read the ledger with avsm tail"),
    ] {
        let (code, _, err) = avsm_out(tmp.path(), &[word]);
        assert_eq!(code, Some(64), "{word}: {err}");
        assert!(
            err.contains(&format!("there is no {word}: {tip}")),
            "{word}: {err}"
        );
    }
}

#[test]
fn add_help_says_what_a_venture_is_and_names_report_and_cadence() {
    let out = Command::new(env!("CARGO_BIN_EXE_avsm"))
        .arg("add")
        .arg("--help")
        .output()
        .unwrap();
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout).into_owned();
    for word in ["report", "cadence", "mandate.yaml", "purpose", "permitted"] {
        assert!(help.contains(word), "{word}: {help}");
    }
}

#[test]
fn add_says_what_is_wrong_with_the_path_without_os_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let _daemon = start(&home);
    let h = home.as_path();

    let (code, _, err) = avsm_out(h, &["add", tmp.path().join("nope").to_str().unwrap()]);
    assert_eq!(code, Some(66), "{err}");
    assert!(
        err.contains("cannot find") && !err.contains("os error"),
        "{err}"
    );

    let yml = tmp.path().join("yml");
    std::fs::create_dir_all(&yml).unwrap();
    std::fs::write(yml.join("mandate.yml"), "purpose: x\n").unwrap();
    let (code, _, err) = avsm_out(h, &["add", yml.to_str().unwrap()]);
    assert_eq!(code, Some(65), "{err}");
    assert!(err.contains("rename mandate.yml to mandate.yaml"), "{err}");
    assert!(!err.contains("os error"), "{err}");

    let dir = counter_venture(tmp.path());
    let (code, _, err) = avsm_out(h, &["add", dir.join("mandate.yaml").to_str().unwrap()]);
    assert_eq!(code, Some(65), "{err}");
    assert!(err.contains("pass the directory"), "{err}");
    assert!(!err.contains("os error"), "{err}");

    let empty = tmp.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let (code, _, err) = avsm_out(h, &["add", empty.to_str().unwrap()]);
    assert_eq!(code, Some(65), "{err}");
    assert!(
        err.contains("holds no mandate.yaml") && !err.contains("os error"),
        "{err}"
    );

    // The daemon says the same to a caller that did not check first.
    let mut k = avsmd::kernel::Kernel::open(&tmp.path().join("other")).unwrap();
    let err = k.add(&yml).unwrap_err();
    assert!(err.contains("rename mandate.yml to mandate.yaml"), "{err}");
}

#[test]
fn doctor_since_takes_the_digests_head_line_as_printed() {
    let line = "head 42:0123456789abcdef; keep it off this machine and check it with: avsm doctor --since 42:0123456789abcdef";
    let want = (Some(42), "0123456789abcdef".to_string());
    for text in [
        line,
        "  head 42:0123456789abcdef  ",
        "42:0123456789abcdef",
        "head 42:0123456789abcdef;",
    ] {
        assert_eq!(avsmd::doctor::parse_anchor(text).unwrap(), want, "{text}");
    }
    assert_eq!(
        avsmd::doctor::parse_anchor("head 0123456789ABCDEF").unwrap(),
        (None, "0123456789abcdef".to_string())
    );
    for bad in ["head", "head 12", "x:y", "head 3:abc"] {
        assert!(avsmd::doctor::parse_anchor(bad).is_err(), "{bad}");
    }
}

#[test]
fn wording_counts_and_durations_read_as_a_person_says_them() {
    use std::time::Duration;
    let d = |s| avsmd::mandate::spoken(Duration::from_secs(s));
    assert_eq!(d(86_400), "1 day");
    assert_eq!(d(2 * 86_400), "2 days");
    assert_eq!(d(90 * 60), "1h 30m");
    assert_eq!(d(14 * 86_400), "14 days");
    assert_eq!(d(86_400 + 3_600), "1 day 1h");
    assert_eq!(avsmd::record::times(1), "once");
    assert_eq!(avsmd::record::times(2), "2 times");
    assert_eq!(avsmd::record::times(21), "21 times");
}

#[test]
fn a_levers_teachback_says_it_will_ask_first_once() {
    let m = avsmd::mandate::Mandate::parse(
        "purpose: p\nmetric: m\ncontroller: {kind: model}\ncadence: 1h\npermitted:\n\
         - {name: mail, run: ./mail, cap: 3/day, reversible: false, takes: text}\n\
         - {name: wipe, run: ./wipe, cap: 3/day, reversible: false}\n\
         - {name: post, run: ./post, cap: 3/day, reversible: true, takes: text}\n\
         - {name: sent, run: ./sent, cap: 3/day, reversible: false, delegated: true, takes: text}\n",
    )
    .unwrap();
    let text = avsmd::teach::render(&m);
    for line in text.lines().filter(|l| l.contains("may ")) {
        assert!(line.matches("will ask first").count() <= 1, "{line}");
    }
    assert!(
        text.contains("may mail  3/day  irreversible, takes text and will ask first"),
        "{text}"
    );
    assert!(
        text.contains("may wipe  3/day  irreversible, will ask first"),
        "{text}"
    );
    assert!(
        text.contains("may post  3/day  reversible, takes text and will ask first"),
        "{text}"
    );
    assert!(
        text.contains("may sent  3/day  irreversible, delegated, takes text"),
        "{text}"
    );
}

#[test]
fn avsmd_dash_capital_t_prints_the_forecast_and_canary_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let show = |config: &str| {
        let home = tmp.path().join("h");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("config.yaml"), config).unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_avsmd"))
            .arg("--home")
            .arg(&home)
            .arg("-T")
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let out = show("nightly: never\n");
    assert!(
        out.contains("# forecast: {days: 30, seeds: 16} is the default"),
        "{out}"
    );
    assert!(
        !out.contains("\nforecast:"),
        "a default is a comment: {out}"
    );
    assert!(out.contains("canary_asks: null"), "{out}");
    let out = show("forecast: {days: 3, seeds: 4}\ncanary_asks: {every: 7}\n");
    assert!(out.contains("forecast:\n  days: 3\n  seeds: 4\n"), "{out}");
    assert!(out.contains("canary_asks:\n  every: 7\n"), "{out}");
    assert!(!out.contains("canary_asks: null"), "{out}");
}

#[test]
fn pausing_twice_and_resuming_a_running_venture_record_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let _daemon = start(&home);
    let dir = counter_venture(tmp.path());
    assert!(avsm(&home, &["add", dir.to_str().unwrap()]).0);

    // The first pause writes the declared pause.
    let (ok, out) = avsm(&home, &["pause", "counter"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("counter      declared  decision  paused"),
        "{out}"
    );

    // The second pause writes nothing and says so.
    let (ok, out) = avsm(&home, &["pause", "counter"]);
    assert!(ok, "{out}");
    assert!(out.contains("counter is already paused"), "{out}");

    let (_, tail, _) = avsm_out(&home, &["tail", "1000"]);
    assert_eq!(
        tail.lines()
            .filter(|l| l.contains("decision  paused"))
            .count(),
        1,
        "{tail}"
    );

    // Resuming writes the declared resume.
    let (ok, out) = avsm(&home, &["resume", "counter"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("counter      declared  decision  resumed"),
        "{out}"
    );

    // Resuming a running venture writes nothing and says so.
    let (ok, out) = avsm(&home, &["resume", "counter"]);
    assert!(ok, "{out}");
    assert!(out.contains("counter is not paused"), "{out}");

    let (_, tail, _) = avsm_out(&home, &["tail", "1000"]);
    assert_eq!(
        tail.lines()
            .filter(|l| l.contains("decision  paused"))
            .count(),
        1,
        "{tail}"
    );
    assert_eq!(
        tail.lines()
            .filter(|l| l.contains("decision  resumed"))
            .count(),
        1,
        "{tail}"
    );
}

#[test]
fn a_mandate_yaml_left_in_the_home_is_named_not_read() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("mandate.yaml"),
        "purpose: x\nmetric: count\ncontroller: {kind: model}\ncadence: 1h\npermitted: []\n",
    )
    .unwrap();
    let note =
        "mandate.yaml in the home is not read: a venture lives in its own directory; add it with avsm add <dir>";

    // avsmd -t names it.
    let out = Command::new(env!("CARGO_BIN_EXE_avsmd"))
        .arg("--home")
        .arg(&home)
        .arg("-t")
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(text.contains(note), "{text}");

    // avsm doctor names it too.
    let (_, out, _) = avsm_out(&home, &["doctor"]);
    assert!(out.contains(note), "{out}");
}
