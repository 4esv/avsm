//! The kernel with a fake role and a shell venture: one full cycle, reports
//! on cadence with threshold actions, run supervision, approvals, and the
//! warden's backends against local stubs.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use avsm::gate::Candidate;
use avsm::kernel::{Kernel, RolesProcess, WardenProcess};
use avsm::record::{Claim, Kind, Payload, Record};
use avsm::role::{Context, InferRequest, Role};
use avsm::warden::{LocalWarden, Warden};

/// A role that answers from a script of replies.
struct FakeRole(VecDeque<Vec<Record>>);

impl Role for FakeRole {
    fn decide(
        &mut self,
        _ctx: &Context,
        _infer: &mut dyn FnMut(&InferRequest) -> Result<String, String>,
    ) -> Result<Vec<Record>, String> {
        Ok(self.0.pop_front().unwrap_or_default())
    }
}

fn script(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn venture(parent: &Path, name: &str, mandate: &str) -> PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("mandate.yaml"), mandate).unwrap();
    dir
}

fn kernel(home: &Path, role: Box<dyn Role + Send>) -> Kernel {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(
        home.join("config.yaml"),
        "nightly: never\ndefault: k\nbackends:\n  k: {kind: command, run: echo nothing}\n",
    )
    .unwrap();
    let warden = LocalWarden::open(home).unwrap();
    Kernel::with(home, role, Box::new(warden)).unwrap()
}

fn proposal(venture: &str, action: &str) -> Record {
    Record::Proposal {
        claim: Claim::Inferred,
        venture: venture.into(),
        action: action.into(),
        reason: "the night is long".into(),
        payload: None,
    }
}

fn proposal_with(venture: &str, action: &str, text: &str) -> Record {
    Record::Proposal {
        claim: Claim::Inferred,
        venture: venture.into(),
        action: action.into(),
        reason: "the night is long".into(),
        payload: Some(Payload::Text { text: text.into() }),
    }
}

fn kinds(entries: &[avsm::record::Entry]) -> Vec<(Kind, String)> {
    entries
        .iter()
        .map(|e| (e.record.kind(), e.record.summary()))
        .collect()
}

#[test]
fn one_full_cycle_with_a_fake_role_and_a_shell_venture() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = venture(
        tmp.path(),
        "shop",
        "purpose: Sell more.\nmetric: orders\ncontroller: {kind: model}\ncadence: 1h\n\
         permitted:\n\
         - {name: tidy, run: touch tidied, cap: 3/day, reversible: true}\n\
         - {name: deploy, run: touch deployed, cap: 1/day, reversible: false, delegated: true}\n\
         - {name: wipe, run: touch wiped, cap: 1/day, reversible: false}\n",
    );
    script(&dir, "report", "echo 5");
    script(&dir, "run", "while true; do sleep 1; done");
    let replies = VecDeque::from(vec![vec![
        proposal("shop", "tidy"),
        proposal("shop", "deploy"),
        proposal("shop", "wipe"),
        Record::Ask {
            claim: Claim::Inferred,
            venture: "shop".into(),
            action: None,
            question: "should we hire?".into(),
            refusal: None,
            payload: None,
        },
    ]]);
    let mut k = kernel(&home, Box::new(FakeRole(replies)));
    k.add(&dir).unwrap();

    // First tick: the run starts and the first report is read at once.
    k.tick();
    let pid = k.run_pid("shop").expect("run started");
    assert!(Path::new(&format!("/proc/{pid}")).exists());
    let entries = k.entries().unwrap();
    assert_eq!(
        kinds(&entries)[1],
        (Kind::Report, "orders=5".to_string()),
        "{:?}",
        kinds(&entries)
    );

    // The nightly cycle, on demand.
    k.cycle("shop");
    let entries = k.entries().unwrap();
    let got = kinds(&entries);
    let expect = [
        (Kind::Decision, "adopted"),
        (Kind::Report, "orders=5"),
        (Kind::Proposal, "tidy: the night is long"),
        (Kind::Action, "tidy: done"),
        (Kind::Proposal, "deploy: the night is long"),
        (Kind::Intent, "deploy: starting"),
        (Kind::Outcome, "#6 done"),
        (Kind::Proposal, "wipe: the night is long"),
        (Kind::Ask, "wipe? may I wipe? it is irreversible"),
        (Kind::Ask, "should we hire?"),
    ];
    assert_eq!(got.len(), expect.len(), "{got:?}");
    for (g, e) in got.iter().zip(expect.iter()) {
        assert_eq!(g.0, e.0, "{got:?}");
        assert!(g.1.starts_with(e.1), "{g:?} vs {e:?}");
    }
    assert!(dir.join("tidied").exists());
    assert!(dir.join("deployed").exists());
    assert!(!dir.join("wiped").exists());
    assert!(entries
        .iter()
        .all(|e| e.record.kind() != Kind::Proposal || e.record.claim() == Claim::Inferred));
    assert_eq!(
        k.status()
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1),
        Some("asking")
    );
    assert_eq!(k.asks().unwrap().lines().count(), 2);

    // The proof behind the action cites the proposal as occasion only.
    let why = k.why(4).unwrap();
    assert!(
        why.contains("occasion   #3  inferred  proposal: tidy"),
        "{why}"
    );
    assert!(why.contains("(authorizes nothing)"), "{why}");
    let why = k.why(9).unwrap();
    assert!(
        why.contains("because    wipe is irreversible and no declared decision approves it"),
        "{why}"
    );

    // Approving the ask makes the next tick act: intent, then outcome.
    k.approve(9, None).unwrap();
    k.tick();
    let entries = k.entries().unwrap();
    let last = kinds(&entries);
    assert_eq!(last[last.len() - 3].0, Kind::Decision);
    assert_eq!(
        last[last.len() - 2],
        (Kind::Intent, "wipe: starting".to_string()),
        "{last:?}"
    );
    assert!(last[last.len() - 1].1.starts_with("#12 done"), "{last:?}");
    assert!(dir.join("wiped").exists());
    let why = k.why(entries.len() as u64 - 1).unwrap();
    assert!(
        why.contains("decision   #11  declared  approved ask #9"),
        "{why}"
    );

    // A second tick does not act twice on the same approval.
    k.tick();
    assert_eq!(k.entries().unwrap().len(), entries.len());
    assert_eq!(k.ledger.verify().unwrap(), entries.len() as u64);
    assert_eq!(k.run_pid("shop"), Some(pid));
}

#[test]
fn threshold_acts_on_measured_reports_until_the_cap() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = venture(
        tmp.path(),
        "counter",
        "purpose: Keep the counter low.\nmetric: count\n\
         controller: {kind: threshold, above: 10, action: reset}\ncadence: 1h\n\
         permitted:\n- {name: reset, run: echo reset >> resets, cap: 2/day, reversible: true}\n",
    );
    script(&dir, "report", "echo 12");
    let mut k = kernel(&home, Box::new(FakeRole(VecDeque::new())));
    k.add(&dir).unwrap();
    for _ in 0..3 {
        k.report("counter", false);
    }
    let got = kinds(&k.entries().unwrap());
    let actions = got.iter().filter(|(k, _)| *k == Kind::Action).count();
    assert_eq!(actions, 2, "{got:?}");
    assert_eq!(
        std::fs::read_to_string(dir.join("resets")).unwrap(),
        "reset\nreset\n"
    );
    let why = k.why(3).unwrap();
    assert!(why.contains("report     #2  measured  count=12"), "{why}");
    assert!(why.contains("threshold  count above 10"), "{why}");
    assert!(why.contains("counter: reset is allowed"), "{why}");

    // A report that fails is still a record, with no value.
    script(&dir, "report", "echo broken >&2; exit 2");
    k.report("counter", false);
    let last = k.entries().unwrap().pop().unwrap();
    assert!(
        matches!(&last.record, Record::Report { value: None, note, .. } if note.contains("report failed: exit 2  broken")),
        "{last:?}"
    );
    script(&dir, "report", "echo not a number");
    k.report("counter", false);
    let last = k.entries().unwrap().pop().unwrap();
    assert!(
        matches!(&last.record, Record::Report { value: None, note, .. } if note.contains("did not print a number")),
        "{last:?}"
    );

    // Below the threshold nothing happens.
    script(&dir, "report", "echo 3");
    let before = k.entries().unwrap().len();
    k.report("counter", false);
    assert_eq!(k.entries().unwrap().len(), before + 1);
}

#[test]
fn run_is_restarted_per_policy_and_stopped_when_paused() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = venture(
        tmp.path(),
        "svc",
        "purpose: Serve.\nmetric: up\ncontroller: {kind: model}\ncadence: 1h\nrestart: on-failure\n",
    );
    script(&dir, "run", "sleep 60");
    let mut k = kernel(&home, Box::new(FakeRole(VecDeque::new())));
    k.add(&dir).unwrap();
    k.tick();
    let first = k.run_pid("svc").expect("started");

    // Killed from outside: restarted after the backoff.
    unsafe { libc::kill(first as i32, libc::SIGKILL) };
    std::thread::sleep(Duration::from_millis(100));
    k.tick();
    assert_eq!(k.run_pid("svc"), None, "noticed the death");
    std::thread::sleep(Duration::from_millis(1100));
    k.tick();
    let second = k.run_pid("svc").expect("restarted");
    assert_ne!(first, second);

    // Paused: stopped. Resumed: started again.
    k.pause("svc").unwrap();
    k.tick();
    assert_eq!(k.run_pid("svc"), None);
    assert!(!Path::new(&format!("/proc/{second}")).exists() || zombie(second));
    k.tick();
    assert_eq!(k.run_pid("svc"), None, "stays stopped while paused");
    k.resume("svc").unwrap();
    k.tick();
    assert!(k.run_pid("svc").is_some());

    // A clean exit under on-failure is left alone.
    let dir2 = venture(
        tmp.path(),
        "once",
        "purpose: Run once.\nmetric: up\ncontroller: {kind: model}\ncadence: 1h\nrestart: on-failure\n",
    );
    script(&dir2, "run", "exit 0");
    k.add(&dir2).unwrap();
    k.tick();
    std::thread::sleep(Duration::from_millis(100));
    k.tick();
    assert_eq!(k.run_pid("once"), None);
    std::thread::sleep(Duration::from_millis(1100));
    k.tick();
    assert_eq!(k.run_pid("once"), None, "exited 0: not restarted");
}

fn zombie(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .map(|s| s.contains("State:\tZ"))
        .unwrap_or(true)
}

/// A one-shot HTTP server that records the request and answers with a body.
fn http_stub(body: &'static str) -> (String, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let mut header_end = None;
        let mut content_length = 0usize;
        loop {
            let n = stream.read(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if header_end.is_none() {
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    header_end = Some(pos + 4);
                    let head = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                    for line in head.lines() {
                        if let Some(v) = line.strip_prefix("content-length:") {
                            content_length = v.trim().parse().unwrap();
                        }
                    }
                }
            }
            if let Some(h) = header_end {
                if buf.len() >= h + content_length {
                    break;
                }
            }
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).unwrap();
        String::from_utf8_lossy(&buf).into_owned()
    });
    (format!("http://127.0.0.1:{port}"), handle)
}

#[test]
fn backends_of_every_kind_answer_through_the_warden() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let (anthropic, a_handle) = http_stub(
        "{\"content\":[{\"type\":\"text\",\"text\":\"{\\\"choice\\\": \\\"nothing\\\"}\"}],\"stop_reason\":\"end_turn\"}",
    );
    let (openai, o_handle) =
        http_stub("{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"nothing\"}}]}");
    std::fs::write(
        home.join("config.yaml"),
        format!(
            "default: a\nbackends:\n\
             \x20 a: {{kind: anthropic, url: {anthropic}/v1/messages, model: claude-opus-5-5, effort: low}}\n\
             \x20 o: {{kind: openai, url: {openai}/v1/chat/completions, model: deepseek-chat, key: deepseek}}\n\
             \x20 k: {{kind: command, run: \"cat > prompt.json; echo '{{\\\"choice\\\": \\\"ask\\\", \\\"question\\\": \\\"all well?\\\"}}'\"}}\n"
        ),
    )
    .unwrap();
    std::fs::write(home.join("keys"), "anthropic=sk-test-1\ndeepseek=dk-2\n").unwrap();
    std::fs::set_permissions(home.join("keys"), std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut warden = LocalWarden::open(&home).unwrap();
    let req = |backend: &str| InferRequest {
        backend: backend.into(),
        system: "be brief".into(),
        user: "well?".into(),
        max_tokens: 64,
    };

    assert_eq!(
        warden.infer(&req("a")).unwrap(),
        "{\"choice\": \"nothing\"}"
    );
    let seen = a_handle.join().unwrap().replace([' ', '\n', '\r'], "");
    assert!(seen.starts_with("POST/v1/messagesHTTP"), "{seen}");
    assert!(seen.contains("x-api-key:sk-test-1"), "{seen}");
    assert!(seen.contains("anthropic-version:2023-06-01"), "{seen}");
    assert!(seen.contains("\"model\":\"claude-opus-5-5\""), "{seen}");
    assert!(seen.contains("\"effort\":\"low\""), "{seen}");
    assert!(seen.contains("\"fallbacks\":\"default\""), "{seen}");
    assert!(seen.contains("\"system\":\"bebrief\""), "{seen}");

    assert_eq!(warden.infer(&req("o")).unwrap(), "nothing");
    let seen = o_handle.join().unwrap().replace([' ', '\n', '\r'], "");
    assert!(seen.contains("authorization:Bearerdk-2"), "{seen}");
    assert!(seen.contains("\"model\":\"deepseek-chat\""), "{seen}");
    assert!(seen.contains("\"role\":\"system\""), "{seen}");

    let reply = warden.infer(&req("k")).unwrap();
    assert_eq!(reply, "{\"choice\": \"ask\", \"question\": \"all well?\"}");
    let prompt = std::fs::read_to_string(home.join("prompt.json")).unwrap();
    assert!(
        prompt.replace(' ', "").contains("\"user\":\"well?\""),
        "{prompt}"
    );

    assert!(warden
        .infer(&req("zz"))
        .unwrap_err()
        .contains("no backend named zz"));
}

#[test]
fn warden_and_roles_run_as_separate_processes() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.yaml"),
        "nightly: never\ndefault: k\nbackends:\n  k: {kind: command, run: \"cat > /dev/null; echo tidy\"}\n",
    )
    .unwrap();
    std::fs::write(home.join("keys"), "anthropic=sk-secret\n").unwrap();
    std::fs::set_permissions(home.join("keys"), std::fs::Permissions::from_mode(0o600)).unwrap();
    let dir = venture(
        tmp.path(),
        "shop",
        "purpose: Sell more.\nmetric: orders\ncontroller: {kind: classifier}\ncadence: 1h\n\
         permitted:\n- {name: tidy, run: touch tidied, cap: 3/day, reversible: true}\n",
    );

    // The test binary is not avsm, so point the children at the real one.
    let exe = Path::new(env!("CARGO_BIN_EXE_avsm"));
    let warden = WardenProcess::spawn_exe(exe, &home).unwrap();
    let roles = RolesProcess::spawn_exe(exe, &home).unwrap();
    let mut k = Kernel::with(&home, Box::new(roles), Box::new(warden)).unwrap();
    k.add(&dir).unwrap();

    // The roles process asks the warden (through the kernel) and proposes.
    k.cycle("shop");
    let got = kinds(&k.entries().unwrap());
    assert_eq!(got[1].0, Kind::Proposal, "{got:?}");
    assert!(got[1].1.starts_with("tidy: chosen by k"), "{got:?}");
    assert_eq!(got[2].0, Kind::Action, "{got:?}");
    assert!(dir.join("tidied").exists());

    // The warden re-checks the gate: an unjustified action is refused.
    let refused = k.why(2).unwrap();
    assert!(refused.contains("shop: tidy is allowed"), "{refused}");
    let bogus = Candidate {
        venture: "shop".into(),
        action: "tidy".into(),
        cites: vec![],
    };
    let mut w = WardenProcess::spawn_exe(exe, &home).unwrap();
    let err = w.execute(&bogus).unwrap_err();
    assert!(
        err.contains("nothing measured or declared calls for tidy"),
        "{err}"
    );

    // Stopping always passes, and pauses the venture.
    let stop = Candidate {
        venture: "shop".into(),
        action: "stop".into(),
        cites: vec![],
    };
    let written = w.execute(&stop).unwrap();
    assert_eq!(written.len(), 1);
    assert_eq!(written[0].record.kind(), Kind::Action);
    assert_eq!(
        k.status().unwrap().split_whitespace().nth(1),
        Some("paused")
    );
}

#[test]
fn keys_are_unreadable_to_the_dropped_user_when_root() {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("not root; skipping the privilege drop check");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let keys = tmp.path().join("keys");
    std::fs::write(&keys, "anthropic=sk-secret\n").unwrap();
    std::fs::set_permissions(&keys, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o711)).unwrap();
    use std::os::unix::process::CommandExt;
    let out = std::process::Command::new("cat")
        .arg(&keys)
        .uid(65534)
        .gid(65534)
        .output()
        .unwrap();
    assert!(!out.status.success(), "nobody could read the keys file");
    let out = std::process::Command::new("cat")
        .arg(&keys)
        .output()
        .unwrap();
    assert!(out.status.success(), "the owner can");
}

#[test]
fn self_reports_integrity_and_halts_everything_when_it_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = venture(
        tmp.path(),
        "counter",
        "purpose: Keep the counter low.\nmetric: count\n\
         controller: {kind: threshold, above: 10, action: reset}\ncadence: 1h\n\
         permitted:\n- {name: reset, run: echo reset >> resets, cap: 9/day, reversible: true}\n",
    );
    script(&dir, "report", "echo 12");
    let mut k = kernel(&home, Box::new(FakeRole(VecDeque::new())));
    k.adopt_self().unwrap();
    k.add(&dir).unwrap();
    k.tick();
    let got = kinds(&k.entries().unwrap());
    assert_eq!(got[0].0, Kind::Decision, "{got:?}");
    assert!(got[0].1.starts_with("adopted mandate"), "{got:?}");
    let self_report = got
        .iter()
        .find(|(kind, s)| *kind == Kind::Report && s.starts_with("integrity=1"))
        .expect("self reported");
    assert!(
        self_report.1.contains("chain ok") && self_report.1.contains("canary ok"),
        "{got:?}"
    );
    assert!(
        got.iter()
            .any(|(kind, s)| *kind == Kind::Action && s.starts_with("reset: done")),
        "{got:?}"
    );
    assert!(k
        .status()
        .unwrap()
        .contains("self         running  integrity=1"));

    // Tamper with the ledger behind avsm's back. The next self report sees it.
    let conn = rusqlite::Connection::open(home.join("ledger.db")).unwrap();
    conn.execute_batch(
        "DROP TRIGGER records_no_update; UPDATE records SET at = at + 1 WHERE seq = 1;",
    )
    .unwrap();
    k.report("self", false);
    let got = kinds(&k.entries().unwrap());
    let n = got.len();
    assert!(
        got[n - 2]
            .1
            .starts_with("integrity=0  chain broken: record #1"),
        "{got:?}"
    );
    assert_eq!(
        got[n - 1],
        (Kind::Action, "stop: done".to_string()),
        "{got:?}"
    );
    assert!(k.status().unwrap().contains("self         paused"));
    let why = k.why(n as u64).unwrap();
    assert!(
        why.contains("axiom      stopping is always allowed"),
        "{why}"
    );

    // Halted: the counter still reports but nothing acts, and its run is stopped.
    let resets_before = std::fs::read_to_string(dir.join("resets"))
        .unwrap()
        .lines()
        .count();
    k.report("counter", true);
    k.tick();
    let got = kinds(&k.entries().unwrap());
    assert_eq!(
        std::fs::read_to_string(dir.join("resets"))
            .unwrap()
            .lines()
            .count(),
        resets_before
    );
    assert!(
        got.iter().filter(|(kind, _)| *kind == Kind::Action).count() == 2,
        "{got:?}"
    );

    // A human resumes self; the next report acts again.
    k.resume("self").unwrap();
    k.tick();
    assert!(k.status().unwrap().contains("self         running"));
    k.report("counter", false);
    assert_eq!(
        std::fs::read_to_string(dir.join("resets"))
            .unwrap()
            .lines()
            .count(),
        resets_before + 1
    );
}

#[test]
fn payload_levers_ask_first_run_with_what_the_human_approved() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = venture(
        tmp.path(),
        "shop",
        "purpose: Sell more.\nmetric: orders\ncontroller: {kind: model}\ncadence: 1h\n\
         permitted:\n\
         - {name: announce, run: \"cat > announced; echo sent $AVSM_PAYLOAD\", cap: 3/day, reversible: true, takes: text}\n\
         - {name: label, run: \"cat > labelled\", cap: 3/day, reversible: true, takes: json, delegated: true}\n\
         - {name: tidy, run: touch tidied, cap: 3/day, reversible: true}\n",
    );
    script(&dir, "report", "echo 5");
    let replies = VecDeque::from(vec![vec![
        proposal_with("shop", "announce", "Hello all"),
        Record::Proposal {
            claim: Claim::Inferred,
            venture: "shop".into(),
            action: "label".into(),
            reason: "tag it".into(),
            payload: Some(Payload::Json {
                json: serde_json::json!({"tag": "new"}),
            }),
        },
        proposal_with("shop", "tidy", "ignored: tidy takes nothing"),
    ]]);
    let mut k = kernel(&home, Box::new(FakeRole(replies)));
    k.add(&dir).unwrap();
    k.cycle("shop");
    let got = kinds(&k.entries().unwrap());
    // announce: a model's content waits for a human. label: delegated, so
    // it ran with the JSON. tidy: the payload was dropped, the lever ran.
    let expect = [
        (Kind::Decision, "adopted"),
        (
            Kind::Proposal,
            "announce: the night is long  [text \"Hello all\"]",
        ),
        (
            Kind::Ask,
            "announce? may I announce with this?  [text \"Hello all\"]",
        ),
        (
            Kind::Proposal,
            "label: tag it  [json \"{\\\"tag\\\":\\\"new\\\"}\"]",
        ),
        (Kind::Action, "label: done"),
        (Kind::Proposal, "tidy: the night is long"),
        (Kind::Action, "tidy: done"),
    ];
    assert_eq!(got.len(), expect.len(), "{got:?}");
    for (g, e) in got.iter().zip(expect.iter()) {
        assert_eq!(g.0, e.0, "{got:?}");
        assert!(g.1.starts_with(e.1), "{g:?} vs {e:?}");
    }
    assert!(!dir.join("announced").exists());
    assert_eq!(
        std::fs::read_to_string(dir.join("labelled")).unwrap(),
        "{\"tag\":\"new\"}"
    );
    let why = k.why(3).unwrap();
    assert!(
        why.contains("announce is irreversible and no declared decision approves it")
            || why.contains("because"),
        "{why}"
    );
    assert!(
        why.contains("payload    #2  inferred  text \"Hello all\""),
        "{why}"
    );

    // The asks list shows the payload under the ask.
    let asks = k.asks().unwrap();
    assert!(
        asks.contains("announce? may I announce with this?"),
        "{asks}"
    );
    assert!(asks.contains("\n    Hello all\n"), "{asks}");
    assert_eq!(k.payload(3).unwrap(), "text\nHello all");

    // Approve with an amended payload: the amended text is what runs, and
    // the proof cites the decision for it.
    let text = k.approve(3, Some("Hello all, amended")).unwrap();
    assert!(
        text.contains("approved #3 with amended text \"Hello all, amended\""),
        "{text}"
    );
    k.tick();
    let entries = k.entries().unwrap();
    let action = entries
        .iter()
        .rev()
        .find(|e| e.record.kind() == Kind::Action)
        .unwrap();
    assert!(
        action
            .record
            .summary()
            .starts_with("announce: done  sent text"),
        "{}",
        action.record.summary()
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("announced")).unwrap(),
        "Hello all, amended"
    );
    assert_eq!(
        action.record.payload(),
        Some(&Payload::Text {
            text: "Hello all, amended".into()
        })
    );
    let why = k.why(action.seq).unwrap();
    assert!(
        why.contains("decision   #8  declared  approved ask #3"),
        "{why}"
    );
    assert!(
        why.contains("payload    #8  declared  text \"Hello all, amended\""),
        "{why}"
    );

    // A human proposal by hand: declared, gated, and run at once.
    let text = k.propose("shop", "announce", Some("From me")).unwrap();
    assert!(
        text.contains("declared  proposal  announce: proposed by hand  [text \"From me\"]"),
        "{text}"
    );
    assert!(
        text.contains("measured  action    announce: done"),
        "{text}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("announced")).unwrap(),
        "From me"
    );
    let err = k.propose("shop", "announce", None).unwrap_err();
    assert!(err.contains("announce takes text"), "{err}");
    let err = k.propose("shop", "tidy", Some("x")).unwrap_err();
    assert!(err.contains("tidy takes no payload"), "{err}");
    let err = k.propose("shop", "label", Some("not json")).unwrap_err();
    assert!(err.contains("not JSON"), "{err}");
    let err = k.propose("shop", "wipe", None).unwrap_err();
    assert!(err.contains("does not permit wipe"), "{err}");
    assert_eq!(
        k.ledger.verify().unwrap(),
        k.entries().unwrap().len() as u64
    );
}
