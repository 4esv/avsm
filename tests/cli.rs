//! Every command, through the real binary and its socket, against a temp
//! ledger.

mod common;

use common::ran;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use avsmd::kernel::Kernel;
use avsmd::record::{Claim, Record};

fn avsm(home: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_avsm"))
        .arg("--home")
        .arg(home)
        .args(args)
        .output()
        .expect("run avsm");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

/// A config that never refuses a run for the night. Forecasts stay on but
/// small: the default, 30 days x 16 seeds, runs in the tick before the
/// first lever and takes tens of seconds in a debug build on a loaded CI
/// runner; these tests time the daemon, not forecast throughput.
const NEVER: &str = "nightly: never\nforecast: {days: 2, seeds: 2}\n";

/// Forecasts small, as in `NEVER`, the night left as it is.
const SMALL_FORECASTS: &str = "forecast: {days: 2, seeds: 2}\n";

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

fn counter_venture(parent: &Path) -> std::path::PathBuf {
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
    std::fs::set_permissions(dir.join("report"), std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

#[test]
fn says_so_when_not_running() {
    let home = tempfile::tempdir().unwrap();
    let (ok, text) = avsm(home.path(), &["status"]);
    assert!(!ok);
    assert!(
        text.contains("avsmd is not running; start it with: avsmd"),
        "{text}"
    );
}

#[test]
fn help_reads_plainly() {
    let out = Command::new(env!("CARGO_BIN_EXE_avsm"))
        .arg("--help")
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&out.stdout).to_lowercase();
    for cmd in [
        "add", "status", "tail", "asks", "approve", "deny", "pause", "resume", "why", "propose",
        "know", "digest", "doctor",
    ] {
        assert!(
            help.contains(&format!("  {cmd} ")),
            "help lacks {cmd}:\n{help}"
        );
    }
    for word in [
        "socket", "hash", "datalog", "sqlite", "json", "rpc", "protocol",
    ] {
        assert!(!help.contains(word), "help uses jargon {word:?}:\n{help}");
    }
    for cmd in [
        "add", "tail", "approve", "deny", "pause", "resume", "why", "propose", "know", "doctor",
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_avsm"))
            .args([cmd, "--help"])
            .output()
            .unwrap();
        let help = String::from_utf8_lossy(&out.stdout).to_lowercase();
        for word in [
            "socket", "hash", "datalog", "sqlite", "json", "rpc", "protocol",
        ] {
            assert!(
                !help.contains(word),
                "{cmd} help uses jargon {word:?}:\n{help}"
            );
        }
    }
}

#[test]
fn every_command_against_a_temp_ledger() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.yaml"), SMALL_FORECASTS).unwrap();
    let _daemon = start(&home);
    let run = |args: &[&str]| avsm(&home, args);
    let seq_of = |line: &str| -> String {
        line.trim_start_matches('#')
            .split_whitespace()
            .next()
            .unwrap()
            .to_string()
    };

    // The self venture exists from the first start.
    let (ok, text) = run(&["status"]);
    assert!(ok, "{text}");
    assert_eq!(text.lines().next(), Some("all running"), "{text}");
    assert!(
        text.lines()
            .nth(1)
            .unwrap()
            .starts_with("self         running  "),
        "{text}"
    );

    let (ok, text) = run(&["tail"]);
    assert!(ok);
    assert!(
        text.lines()
            .next()
            .unwrap()
            .contains("self         declared  decision  adopted mandate"),
        "{text}"
    );

    let (ok, text) = run(&["add", tmp.path().join("missing").to_str().unwrap()]);
    assert!(!ok);
    assert!(text.contains("cannot find"), "{text}");

    let dir = counter_venture(tmp.path());
    let (ok, text) = run(&["add", dir.to_str().unwrap()]);
    assert!(ok, "{text}");
    assert!(text.starts_with("added counter from "), "{text}");
    assert!(text.contains("watching count every 30s"), "{text}");
    assert!(text.contains("may reset  3/day  reversible"), "{text}");
    let adopt_line = text.lines().last().unwrap().trim().to_string();
    assert!(
        adopt_line.contains("counter      declared  decision  adopted mandate"),
        "{text}"
    );
    let adopt = seq_of(&adopt_line);

    let (ok, text) = run(&["status"]);
    assert!(ok, "{text}");
    assert_eq!(text.lines().next(), Some("all running"), "{text}");
    assert!(
        text.lines()
            .nth(1)
            .unwrap()
            .starts_with("counter      running  "),
        "{text}"
    );
    assert!(
        text.lines()
            .nth(2)
            .unwrap()
            .starts_with("self         running  "),
        "{text}"
    );

    let (ok, text) = run(&["tail", "1"]);
    assert!(ok);
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.starts_with('#'), "{text}");

    let (ok, text) = run(&["asks"]);
    assert!(ok);
    assert_eq!(text, "no open asks\n");

    let (ok, text) = run(&["approve", "1"]);
    assert!(!ok);
    assert!(text.contains("#1 is not an open ask"), "{text}");
    let (ok, text) = run(&["deny", "1", "not", "now"]);
    assert!(!ok);
    assert!(text.contains("#1 is not an open ask"), "{text}");
    let (ok, text) = run(&["approve", "x"]);
    assert!(!ok);
    assert!(
        text.contains("a record number looks like 12 or #12"),
        "{text}"
    );

    let (ok, text) = run(&["pause", "counter"]);
    assert!(ok, "{text}");
    assert!(
        text.contains("counter      declared  decision  paused"),
        "{text}"
    );
    let (ok, text) = run(&["status"]);
    assert!(ok);
    assert_eq!(text.lines().next(), Some("paused: counter"), "{text}");
    assert!(
        text.lines()
            .nth(1)
            .unwrap()
            .starts_with("counter      paused      "),
        "{text}"
    );
    let (ok, text) = run(&["pause", "nothing"]);
    assert!(!ok);
    assert!(text.contains("no venture named nothing"), "{text}");

    let (ok, text) = run(&["resume", "counter"]);
    assert!(ok, "{text}");
    assert!(
        text.contains("counter      declared  decision  resumed"),
        "{text}"
    );
    let resumed = seq_of(text.trim());
    let (ok, text) = run(&["status"]);
    assert!(ok);
    assert_eq!(text.lines().next(), Some("all running"), "{text}");
    assert!(
        text.lines()
            .nth(1)
            .unwrap()
            .starts_with("counter      running  "),
        "{text}"
    );

    let (ok, text) = run(&["why", &adopt]);
    assert!(ok, "{text}");
    assert!(text.starts_with(&format!("#{adopt} ")), "{text}");
    assert!(text.contains("declared by a human"), "{text}");
    let (ok, text) = run(&["why", &format!("#{resumed}")]);
    assert!(ok, "{text}");
    assert!(text.contains("resumed"), "{text}");
    let (ok, text) = run(&["why", "999"]);
    assert!(!ok);
    assert!(text.contains("no record #999"), "{text}");

    let (ok, text) = run(&["tail"]);
    assert!(ok);
    assert!(text.lines().count() >= 4, "{text}");
    for line in text.lines() {
        assert!(line.starts_with('#'), "{line}");
    }

    let (ok, text) = run(&["dance"]);
    assert!(!ok);
    assert!(
        text.contains("unrecognized subcommand") || text.contains("does not understand"),
        "{text}"
    );
}

#[test]
fn approve_and_deny_answer_open_asks() {
    let tmp = tempfile::tempdir().unwrap();
    let mut kernel = Kernel::open(&tmp.path().join("home")).unwrap();
    let ask = |action: &str| Record::Ask {
        packet: None,
        cites: vec![],
        claim: Claim::Inferred,
        venture: "counter".to_string(),
        action: Some(action.to_string()),
        question: format!("may I {action}?"),
        refusal: None,
        signal: None,
        payload: None,
    };
    let first = kernel.append(ask("deploy")).unwrap().seq;
    let second = kernel.append(ask("rollback")).unwrap().seq;
    let handle = |k: &mut Kernel, args: &[&str]| {
        k.handle(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    };

    let text = handle(&mut kernel, &["asks"]).unwrap();
    assert_eq!(text.lines().count(), 2, "{text}");
    assert!(text.contains("deploy? may I deploy?"), "{text}");

    let text = handle(&mut kernel, &["approve", &first.to_string()]).unwrap();
    assert!(
        text.contains(&format!("declared  decision  approved #{first}")),
        "{text}"
    );
    let text = handle(&mut kernel, &["asks"]).unwrap();
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.contains("rollback?"), "{text}");

    let err = handle(&mut kernel, &["approve", &first.to_string()]).unwrap_err();
    assert!(
        err.contains(&format!("#{first} was already approved by #")),
        "{err}"
    );

    let text = handle(&mut kernel, &["deny", &second.to_string(), "too", "risky"]).unwrap();
    assert!(
        text.contains(&format!("denied #{second}: too risky")),
        "{text}"
    );
    assert_eq!(handle(&mut kernel, &["asks"]).unwrap(), "no open asks\n");

    let text = handle(&mut kernel, &["why", &first.to_string()]).unwrap();
    assert!(text.contains("asked: may I deploy?"), "{text}");
    assert_eq!(kernel.ledger.verify().unwrap(), 4);
}

/// Monkey round 3: an approval given while its venture is paused is spent
/// by the gate without running, so a lever that pauses its venture could
/// burn a human's approvals. Approve is refused until a resume; deny is
/// not.
#[test]
fn approve_is_refused_while_the_ventures_paused_and_deny_is_not() {
    use avsmd::record::Verdict;
    let tmp = tempfile::tempdir().unwrap();
    let mut kernel = Kernel::open(&tmp.path().join("home")).unwrap();
    let ask = |action: &str| Record::Ask {
        packet: None,
        cites: vec![],
        claim: Claim::Inferred,
        venture: "counter".to_string(),
        action: Some(action.to_string()),
        question: format!("may I {action}?"),
        refusal: None,
        signal: None,
        payload: None,
    };
    let first = kernel.append(ask("deploy")).unwrap().seq;
    let second = kernel.append(ask("rollback")).unwrap().seq;
    let decide = |k: &mut Kernel, verdict: Verdict| {
        k.append(Record::Decision {
            claim: Claim::Declared,
            venture: "counter".to_string(),
            verdict,
            by: None,
        })
        .unwrap();
    };
    decide(&mut kernel, Verdict::Pause);
    let handle = |k: &mut Kernel, args: &[&str]| {
        k.handle(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    };
    let err = handle(&mut kernel, &["approve", &first.to_string()]).unwrap_err();
    assert!(
        err.contains(
            "counter is paused; resume it first (avsm resume counter), then approve: \
             an approval given while paused is spent without running"
        ),
        "{err}"
    );
    assert!(
        handle(&mut kernel, &["asks"])
            .unwrap()
            .contains("may I deploy?"),
        "the ask stays open"
    );
    let text = handle(&mut kernel, &["deny", &second.to_string(), "no"]).unwrap();
    assert!(text.contains(&format!("denied #{second}")), "{text}");
    decide(&mut kernel, Verdict::Resume);
    let text = handle(&mut kernel, &["approve", &first.to_string()]).unwrap();
    assert!(text.contains(&format!("approved #{first}")), "{text}");
}

/// Monkey round 3: `why` on a record the foreign-record alarm named said
/// "declared by a human". It says avsmd did not write it, citing the alarm.
#[test]
fn why_on_a_record_the_alarm_named_says_avsmd_did_not_write_it() {
    use avsmd::record::Verdict;
    let tmp = tempfile::tempdir().unwrap();
    let mut kernel = Kernel::open(&tmp.path().join("home")).unwrap();
    let forged = kernel
        .append(Record::Decision {
            claim: Claim::Declared,
            venture: "v1".to_string(),
            verdict: Verdict::Pause,
            by: None,
        })
        .unwrap()
        .seq;
    let mut alarm = Record::report(Claim::Measured, "self", "foreign", Some(1.0), "not ours");
    if let Record::Report { cites, .. } = &mut alarm {
        cites.push(forged);
    }
    let alarm = kernel.append(alarm).unwrap().seq;
    let text = kernel.why(forged).unwrap();
    assert!(
        text.contains(&format!(
            "not written by avsmd: the foreign-record alarm #{alarm} named it"
        )),
        "{text}"
    );
    assert!(!text.contains("by a human"), "{text}");
    assert!(text.contains(&format!("avsm why {alarm}")), "{text}");
    // The sentence runs on, not a run of spaces where a continuation was
    // dropped.
    assert!(
        text.contains("named it. another process wrote it"),
        "{text}"
    );
    assert!(!text.contains("named it.  "), "{text}");
    assert!(!text.contains("is that  "), "{text}");
}

/// Monkey round 3: the digest of a venture whose mandate cannot be read
/// showed a blank metric and "no report". It names the metric of the
/// last adopted mandate in the ledger, and its reading.
#[test]
fn the_digest_of_a_venture_whose_mandate_cannot_be_read_names_its_last_adopted_metric() {
    use avsmd::record::Verdict;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let mandate = "purpose: p\nmetric: count\ncontroller: {kind: model}\ncadence: 1h\n\
                   permitted:\n  - {name: water, run: ./water, cap: 3/day, reversible: true}\n";
    {
        let mut kernel = Kernel::open(&home).unwrap();
        // Adopted text that does not match its hash: the venture cannot run.
        kernel
            .append(Record::Decision {
                claim: Claim::Declared,
                venture: "broken".to_string(),
                verdict: Verdict::Adopt {
                    mandate: "0000".to_string(),
                    path: tmp.path().join("broken").to_string_lossy().into_owned(),
                    text: Some(mandate.to_string()),
                },
                by: None,
            })
            .unwrap();
        kernel
            .append(Record::report(
                Claim::Measured,
                "broken",
                "count",
                Some(4.0),
                "",
            ))
            .unwrap();
    }
    let kernel = Kernel::open(&home).unwrap();
    let digest = kernel.digest().unwrap();
    let line = digest
        .lines()
        .find(|l| l.starts_with("broken"))
        .unwrap_or_else(|| panic!("{digest}"));
    assert!(line.contains("count=4"), "{digest}");
    assert!(!line.contains("no report"), "{digest}");
}

/// The digest of a venture whose adopted mandate cannot be read, and
/// whose ledger names no later mandate with a metric, says it cannot run
/// rather than a stray "name : no report".
#[test]
fn the_digest_of_a_venture_whose_mandate_cannot_be_read_says_so() {
    use avsmd::record::Verdict;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    {
        let mut kernel = Kernel::open(&home).unwrap();
        // No adopted text and no file on disk: the venture cannot run,
        // and nothing names a metric to show in its place.
        kernel
            .append(Record::Decision {
                claim: Claim::Declared,
                venture: "broken".to_string(),
                verdict: Verdict::Adopt {
                    mandate: "0000".to_string(),
                    path: tmp.path().join("gone").to_string_lossy().into_owned(),
                    text: None,
                },
                by: None,
            })
            .unwrap();
    }
    let kernel = Kernel::open(&home).unwrap();
    let digest = kernel.digest().unwrap();
    let line = digest
        .lines()
        .find(|l| l.starts_with("broken"))
        .unwrap_or_else(|| panic!("{digest}"));
    assert_eq!(
        line, "broken: cannot run (its mandate cannot be read)",
        "{digest}"
    );
}

/// The README walkthrough: add examples/counter, watch the count cross the
/// threshold, read the proof. The count is seeded at 10 so it crosses on
/// the first report.
#[test]
fn examples_counter_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = tmp.path().join("counter");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/counter");
    std::fs::create_dir_all(&dir).unwrap();
    for name in ["mandate.yaml", "report", "reset"] {
        std::fs::copy(src.join(name), dir.join(name)).unwrap();
    }
    std::fs::write(dir.join("count"), "10\n").unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.yaml"), SMALL_FORECASTS).unwrap();
    let _daemon = start(&home);
    let (ok, text) = avsm(&home, &["add", dir.to_str().unwrap()]);
    assert!(ok, "{text}");

    let began = Instant::now();
    let action = loop {
        let (ok, text) = avsm(&home, &["tail", "50"]);
        assert!(ok, "{text}");
        if ran(&text, "reset", "count is 0 again") {
            break text
                .lines()
                .find(|l| l.contains("intent    reset: starting"))
                .unwrap()
                .to_string();
        }
        assert!(
            began.elapsed() < Duration::from_secs(30),
            "no action yet:\n{text}"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    let seq = action
        .trim_start_matches('#')
        .split_whitespace()
        .next()
        .unwrap();
    let (ok, why) = avsm(&home, &["why", seq]);
    assert!(ok, "{why}");
    assert!(why.contains("counter: reset is allowed"), "{why}");
    assert!(why.contains("measured  count=11"), "{why}");
    assert!(why.contains("threshold  count above 10"), "{why}");
    assert!(why.contains("cap        0 of 60/hour used"), "{why}");
    assert_eq!(
        std::fs::read_to_string(dir.join("count")).unwrap().trim(),
        "0"
    );
}

/// A human proposes by hand, reads the payload under the ask, and approves
/// with an edit: what they saved is what runs.
#[test]
fn propose_asks_and_approve_edit_over_the_socket() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = tmp.path().join("shop");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mandate.yaml"),
        "purpose: Sell more.\nmetric: orders\ncontroller: {kind: model}\ncadence: 1h\n\
         permitted:\n\
         - {name: announce, run: \"cat > announced\", cap: 3/day, reversible: true, takes: text}\n\
         - {name: mail, run: \"cat > mailed\", cap: 3/day, reversible: false, takes: text}\n",
    )
    .unwrap();
    std::fs::write(dir.join("report"), "#!/bin/sh\necho 3\n").unwrap();
    std::fs::set_permissions(dir.join("report"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.yaml"), NEVER).unwrap();
    let _daemon = start(&home);
    let run = |args: &[&str]| avsm(&home, args);
    let (ok, text) = run(&["add", dir.to_str().unwrap()]);
    assert!(ok, "{text}");
    assert!(
        text.contains("may announce  3/day  reversible, takes text and will ask first"),
        "{text}"
    );
    assert!(
        text.contains("may mail  3/day  irreversible, takes text and will ask first"),
        "{text}"
    );

    // A reversible payload lever, proposed by hand, runs at once.
    let (ok, text) = run(&["propose", "shop", "announce", "Hello", "all"]);
    assert!(ok, "{text}");
    assert!(
        text.contains("declared  proposal  announce: proposed by hand  [text \"Hello all\"]"),
        "{text}"
    );
    assert!(ran(&text, "announce", ""), "{text}");
    assert_eq!(
        std::fs::read_to_string(dir.join("announced")).unwrap(),
        "Hello all"
    );

    // An irreversible one asks first, even from a human; the ask shows the payload.
    let (ok, text) = run(&["propose", "shop", "mail", "Dear all,"]);
    assert!(ok, "{text}");
    assert!(
        text.contains("declared  ask       mail? may I mail with this?  proposed by hand  [text \"Dear all,\"]"),
        "{text}"
    );
    let ask = text
        .lines()
        .find(|l| l.contains("mail? may I mail"))
        .unwrap()
        .trim_start_matches('#')
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    let (ok, text) = run(&["asks"]);
    assert!(ok, "{text}");
    assert!(text.contains("mail? may I mail with this?"), "{text}");
    assert!(text.contains("\n    Dear all,\n"), "{text}");

    // approve --edit runs the editor on the payload and approves what it saved.
    let editor = tmp.path().join("editor");
    std::fs::write(&editor, "#!/bin/sh\nprintf 'Dear all, edited' > \"$1\"\n").unwrap();
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_avsm"))
        .arg("--home")
        .arg(&home)
        .args(["approve", &ask, "--edit"])
        .env("EDITOR", &editor)
        .env("VISUAL", &editor)
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{text}");
    assert!(
        text.contains(&format!(
            "approved #{ask} with amended text \"Dear all, edited\""
        )),
        "{text}"
    );
    let began = Instant::now();
    loop {
        // The file appears before its content does: wait for the content.
        if std::fs::read_to_string(dir.join("mailed")).is_ok_and(|t| t == "Dear all, edited") {
            break;
        }
        assert!(began.elapsed() < Duration::from_secs(20), "mail never ran");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        std::fs::read_to_string(dir.join("mailed")).unwrap(),
        "Dear all, edited"
    );
    let (ok, text) = run(&["tail", "3"]);
    assert!(ok, "{text}");
    assert!(text.contains("intent    mail: starting"), "{text}");
    let (ok, text) = run(&["asks"]);
    assert!(ok);
    assert_eq!(text, "no open asks\n");
    let (ok, text) = run(&["propose", "shop", "announce"]);
    assert!(!ok);
    assert!(text.contains("announce takes text"), "{text}");
}
