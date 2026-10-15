//! Every command, through the real binary and its socket, against a temp
//! ledger.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use avsm::kernel::Kernel;
use avsm::record::{Claim, Record};

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

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(home: &Path) -> Daemon {
    let child = Command::new(env!("CARGO_BIN_EXE_avsm"))
        .arg("--home")
        .arg(home)
        .arg("start")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn avsm start");
    let sock = avsm::kernel::socket_path(home);
    let began = Instant::now();
    while !avsm::protocol::alive(&sock) {
        assert!(
            began.elapsed() < Duration::from_secs(20),
            "avsm did not start"
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
        text.contains("avsm is not running; start it with: avsm start"),
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
        "start", "add", "status", "tail", "asks", "approve", "deny", "pause", "resume", "why",
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
    for cmd in ["add", "tail", "approve", "deny", "pause", "resume", "why"] {
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
    assert!(text.starts_with("self         running  "), "{text}");

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
    assert!(text.starts_with("counter      running  "), "{text}");
    assert!(
        text.lines()
            .nth(1)
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
    assert!(text.starts_with("counter      paused   "), "{text}");
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
    assert!(text.starts_with("counter      running  "), "{text}");

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
        claim: Claim::Inferred,
        venture: "counter".to_string(),
        action: Some(action.to_string()),
        question: format!("may I {action}?"),
        refusal: None,
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
    assert!(err.contains("is not an open ask"), "{err}");

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
