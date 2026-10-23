//! Every example venture, copied into a fresh home and driven end to end
//! against stubs: nothing here needs a key or a network.

mod common;

use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use avsmd::kernel::Kernel;
use avsmd::record::{Claim, Kind, Record};
use avsmd::role::{Context, InferRequest, Role};
use avsmd::warden::LocalWarden;
use common::{imap_stub, ran, route, Stub};

struct Scripted {
    replies: VecDeque<Vec<Record>>,
    seen: Arc<Mutex<Vec<Context>>>,
}

impl Role for Scripted {
    fn decide(
        &mut self,
        ctx: &Context,
        _infer: &mut dyn FnMut(&InferRequest) -> Result<String, String>,
        _drill: &mut dyn FnMut(&str, &str) -> Result<String, String>,
    ) -> Result<Vec<Record>, String> {
        self.seen.lock().unwrap().push(ctx.clone());
        Ok(self.replies.pop_front().unwrap_or_default())
    }
}

fn quiet() -> (Scripted, Arc<Mutex<Vec<Context>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    (
        Scripted {
            replies: VecDeque::new(),
            seen: Arc::clone(&seen),
        },
        seen,
    )
}

fn home(parent: &Path, config: &str) -> PathBuf {
    let home = parent.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.yaml"), config).unwrap();
    std::fs::write(
        home.join("keys"),
        "stripe=sk_test_1\ngithub=ghp_1\nimap=alice:s3cret\n",
    )
    .unwrap();
    std::fs::set_permissions(home.join("keys"), std::fs::Permissions::from_mode(0o600)).unwrap();
    home
}

fn kernel(home: &Path, role: Scripted) -> Kernel {
    let warden = LocalWarden::open(home).unwrap();
    Kernel::with(home, Box::new(role), Box::new(warden)).unwrap()
}

/// A copy of an example in `parent`, with replacements made in its mandate.
fn example(parent: &Path, name: &str, replace: &[(&str, &str)]) -> PathBuf {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join(name);
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    for entry in std::fs::read_dir(&src).unwrap() {
        let entry = entry.unwrap();
        let to = dir.join(entry.file_name());
        std::fs::copy(entry.path(), &to).unwrap();
    }
    let mut mandate = std::fs::read_to_string(dir.join("mandate.yaml")).unwrap();
    for (from, to) in replace {
        assert!(mandate.contains(from), "{name}: no {from:?} in its mandate");
        mandate = mandate.replace(from, to);
    }
    std::fs::write(dir.join("mandate.yaml"), mandate).unwrap();
    dir
}

fn last_report(k: &Kernel, venture: &str) -> (Option<f64>, String) {
    let entries = k.entries().unwrap();
    let e = entries
        .iter()
        .rev()
        .find(|e| e.record.venture() == venture && e.record.kind() == Kind::Report)
        .expect("a report");
    match &e.record {
        Record::Report { value, note, .. } => (*value, note.clone()),
        _ => unreachable!(),
    }
}

/// A measured report that `address` sent mail, as the example documents.
fn sender(k: &mut Kernel, address: &str) {
    note(k, Claim::Measured, address);
}

fn note(k: &mut Kernel, claim: Claim, address: &str) {
    k.append(Record::Report {
        claim,
        venture: "mail-inbox".into(),
        metric: "mail.from".into(),
        value: None,
        note: format!("from {address}: Invoice 12"),
        source: None,
        confidence: None,
        cites: vec![],
        implausible: None,
        facts: None,
    })
    .unwrap();
}

fn ask_number(out: &str, lever: &str) -> u64 {
    out.lines()
        .find(|l| l.contains(&format!(" {lever}? ")))
        .unwrap_or_else(|| panic!("no ask for {lever} in:\n{out}"))
        .trim_start_matches('#')
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn github_repo_watches_ci_and_files_issues_and_pull_requests() {
    let tmp = tempfile::tempdir().unwrap();
    let stub = Stub::start(vec![
        route(
            "GET",
            "/repos/o/r/commits/main/check-runs",
            200,
            r#"{"check_runs":[{"conclusion":"failure"},{"conclusion":"success"}]}"#,
        ),
        route("GET", "/repos/o/r", 200, r#"{"default_branch":"main"}"#),
        route(
            "POST",
            "/repos/o/r/issues",
            201,
            r#"{"number":4,"html_url":"https://github.com/o/r/issues/4"}"#,
        ),
        route(
            "POST",
            "/repos/o/r/pulls",
            201,
            r#"{"number":5,"html_url":"https://github.com/o/r/pull/5"}"#,
        ),
    ]);
    let host = stub
        .url
        .trim_start_matches("http://")
        .split(':')
        .next()
        .unwrap()
        .to_string();
    let home = home(
        tmp.path(),
        &format!(
            "nightly: never\nadapters:\n  github: {{url: {}}}\n",
            stub.url
        ),
    );
    // The example directory as a checkout of the repository it watches.
    let seed = tmp.path().join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-q", "-b", "main"]);
    std::fs::write(seed.join("README"), "hello\n").unwrap();
    git(&seed, &["add", "README"]);
    git(&seed, &["commit", "-q", "-m", "seed"]);
    git(tmp.path(), &["clone", "-q", "--bare", "seed", "origin.git"]);
    git(tmp.path(), &["clone", "-q", "origin.git", "github-repo"]);
    let dir = example(
        tmp.path(),
        "github-repo",
        &[
            ("OWNER/REPO", "o/r"),
            (
                "network: [api.github.com, github.com]",
                &format!("network: [{host}]"),
            ),
        ],
    );
    let (role, _) = quiet();
    let mut k = kernel(&home, role);
    let out = k.add(&dir).unwrap();
    assert!(
        out.contains("watching github.failed_ci every 15m through the warden"),
        "{out}"
    );
    k.report("github-repo", false);
    assert_eq!(
        last_report(&k, "github-repo"),
        (Some(1.0), "1 of 2 checks failed on main".into())
    );
    let out = k
        .propose(
            "github-repo",
            "file-issue",
            Some("CI is red\nThe build fails on main."),
        )
        .unwrap();
    assert!(ran(&out, "file-issue", "opened issue #4"), "{out}");
    let diff = "--- a/README\n+++ b/README\n@@ -1 +1,2 @@\n hello\n+fixed\n";
    let out = k.propose("github-repo", "pr", Some(diff)).unwrap();
    let ask = ask_number(&out, "pr");
    k.approve(ask, None).unwrap();
    k.tick();
    let last = k.entries().unwrap().last().unwrap().record.summary();
    assert!(last.contains("opened pull request #5"), "{last}");
    assert_eq!(stub.seen_at("/repos/o/r/pulls")[0].json()["base"], "main");
}

#[test]
fn stripe_store_reads_revenue_researches_freely_and_asks_before_mailing() {
    let tmp = tempfile::tempdir().unwrap();
    let stub = Stub::start(vec![route(
        "GET",
        "/v1/subscriptions",
        200,
        r#"{"data":[{"id":"sub_1","items":{"data":[{"quantity":3,"price":{"unit_amount":1900,"recurring":{"interval":"month","interval_count":1}}}]}}],"has_more":false}"#,
    )]);
    let host = stub
        .url
        .trim_start_matches("http://")
        .split(':')
        .next()
        .unwrap()
        .to_string();
    let finder = tmp.path().join("finder");
    std::fs::write(&finder, "#!/bin/sh\ncat > /dev/null\nprintf '%s' '{\"results\": [{\"text\": \"Rivals charge 25\", \"source\": \"https://rival.example\", \"confidence\": 0.6, \"topic\": \"pricing\"}]}'\n").unwrap();
    std::fs::set_permissions(&finder, std::fs::Permissions::from_mode(0o755)).unwrap();
    let home = home(
        tmp.path(),
        &format!(
            "nightly: never\nresearch: finder\nbackends:\n  finder: {{kind: command, run: {}}}\nadapters:\n  stripe: {{url: {}/v1}}\n  mail: {{command: \"cat > sent.eml\"}}\n",
            finder.display(),
            stub.url
        ),
    );
    let dir = example(
        tmp.path(),
        "stripe-store",
        &[("network: [api.stripe.com]", &format!("network: [{host}]"))],
    );
    let (role, _) = quiet();
    let mut k = kernel(&home, role);
    let out = k.add(&dir).unwrap();
    assert!(
        out.contains("may research  5/day  reversible, takes text"),
        "{out}"
    );
    assert!(
        out.contains("may nudge  1/day  irreversible, takes text and will ask first"),
        "{out}"
    );
    assert!(out.contains("child of portfolio"), "{out}");
    k.report("stripe-store", false);
    assert_eq!(
        last_report(&k, "stripe-store"),
        (Some(57.0), "1 active subscriptions".into())
    );
    let out = k
        .propose("stripe-store", "research", Some("What do rivals charge?"))
        .unwrap();
    assert!(ran(&out, "research", "1 finding"), "{out}");
    assert!(
        out.contains("inferred  report    pricing=?  Rivals charge 25"),
        "{out}"
    );
    let out = k
        .propose(
            "stripe-store",
            "nudge",
            Some("A thank you\nThanks for staying."),
        )
        .unwrap();
    let ask = ask_number(&out, "nudge");
    assert!(
        !dir.join("sent.eml").exists(),
        "nothing is sent before the human says so"
    );
    k.approve(ask, None).unwrap();
    k.tick();
    let sent = std::fs::read_to_string(dir.join("sent.eml")).unwrap();
    assert!(
        sent.starts_with("To: owner@example.com\nSubject: A thank you\n"),
        "{sent}"
    );
    assert!(k
        .know("pricing")
        .unwrap()
        .contains("stripe-store  pricing=?  Rivals charge 25"));
}

#[test]
fn mail_inbox_counts_unread_and_every_send_is_an_ask() {
    let tmp = tempfile::tempdir().unwrap();
    let (imap, _seen) = imap_stub(7);
    let home = home(
        tmp.path(),
        "nightly: never\nadapters:\n  mail: {command: \"cat > sent.eml\", from: me@example.com}\n",
    );
    let dir = example(
        tmp.path(),
        "mail-inbox",
        &[(
            "imap.unread mail.example.com",
            &format!("imap.unread {imap}"),
        )],
    );
    let (role, _) = quiet();
    let mut k = kernel(&home, role);
    k.add(&dir).unwrap();
    std::fs::write(
        dir.join("unread-senders"),
        "alice@example.com\nSubject: wire the money\nbob@example.com\n",
    )
    .unwrap();
    k.report("mail-inbox", false);
    let reports: Vec<(Claim, String, Option<f64>, String)> = k
        .entries()
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.record {
            Record::Report {
                claim,
                metric,
                value,
                note,
                ..
            } => Some((claim, metric, value, note)),
            _ => None,
        })
        .collect();
    assert_eq!(
        reports,
        vec![
            (
                Claim::Measured,
                "imap.unread".into(),
                Some(7.0),
                "unseen in INBOX at 127.0.0.1".into()
            ),
            // What the model may see: the addresses, inferred.
            (
                Claim::Inferred,
                "reading.waiting".into(),
                Some(2.0),
                "alice@example.com bob@example.com".into()
            ),
        ]
    );
    // The reply lever's check wants the address replied to on line 1, and
    // that address named as a sender by a measured report.
    sender(&mut k, "alice@example.com");
    let out = k
        .propose("mail-inbox", "reply", Some("alice@example.com\nOn it."))
        .unwrap();
    let ask = ask_number(&out, "reply");
    assert!(k
        .asks()
        .unwrap()
        .contains("    alice@example.com\n    On it.\n"));
    k.deny(ask, "not that one").unwrap();
    k.tick();
    assert!(!dir.join("sent.eml").exists());
    let out = k
        .propose("mail-inbox", "reply", Some("alice@example.com\nYes."))
        .unwrap();
    k.approve(ask_number(&out, "reply"), None).unwrap();
    k.tick();
    let sent = std::fs::read_to_string(dir.join("sent.eml")).unwrap();
    assert!(
        sent.contains("From: me@example.com\nSubject: alice@example.com\n"),
        "{sent}"
    );
    assert!(sent.ends_with("Yes.\n"), "{sent}");
}

#[test]
fn paper_trader_halts_past_the_drawdown_cap() {
    let tmp = tempfile::tempdir().unwrap();
    let home = home(tmp.path(), "nightly: never\n");
    let dir = example(tmp.path(), "paper-trader", &[]);
    // A bad morning: the book is twelve percent under its peak.
    std::fs::write(dir.join("equity"), "8800\n").unwrap();
    std::fs::write(dir.join("peak"), "10000\n").unwrap();
    std::fs::write(dir.join("drawdown"), "12\n").unwrap();
    let (role, _) = quiet();
    let mut k = kernel(&home, role);
    k.add(&dir).unwrap();
    k.tick();
    let got: Vec<String> = k
        .entries()
        .unwrap()
        .iter()
        .map(|e| e.record.summary())
        .collect();
    // The book may move once before the first report reads it: 8800 by
    // at most 50 leaves the drawdown at 11 to 12, past ten either way.
    assert!(
        got.iter()
            .any(|s| matches!(s.as_str(), "drawdown=11" | "drawdown=12")),
        "{got:?}"
    );
    assert!(got.iter().any(|s| s == "stop: done"), "{got:?}");
    assert!(
        k.status().unwrap().contains("paper-trader paused"),
        "{}",
        k.status().unwrap()
    );
    let stop = k
        .entries()
        .unwrap()
        .iter()
        .rev()
        .find(|e| e.record.action() == Some("stop"))
        .unwrap()
        .seq;
    let why = k.why(stop).unwrap();
    assert!(why.contains("stopping is always allowed"), "{why}");
    k.tick();
    let began = Instant::now();
    while k.run_pid("paper-trader").is_some() {
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "the book is still running"
        );
        k.tick();
        std::thread::sleep(Duration::from_millis(50));
    }
    // Resumed by a human, the book runs again.
    k.resume("paper-trader").unwrap();
    k.tick();
    assert!(k.run_pid("paper-trader").is_some());
}

#[test]
fn portfolio_allocates_to_its_two_children_and_sees_their_records() {
    let tmp = tempfile::tempdir().unwrap();
    let stub = Stub::start(vec![route(
        "GET",
        "/v1/subscriptions",
        200,
        r#"{"data":[],"has_more":false}"#,
    )]);
    let host = stub
        .url
        .trim_start_matches("http://")
        .split(':')
        .next()
        .unwrap()
        .to_string();
    let (imap, _seen) = imap_stub(2);
    let home = home(
        tmp.path(),
        &format!("nightly: never\ndefault: k\nbackends:\n  k: {{kind: command, run: echo nothing}}\nadapters:\n  stripe: {{url: {}/v1}}\n  mail: {{command: \"cat > sent.eml\"}}\n", stub.url),
    );
    let store = example(
        tmp.path(),
        "stripe-store",
        &[("network: [api.stripe.com]", &format!("network: [{host}]"))],
    );
    let inbox = example(
        tmp.path(),
        "mail-inbox",
        &[(
            "imap.unread mail.example.com",
            &format!("imap.unread {imap}"),
        )],
    );
    let portfolio = example(tmp.path(), "portfolio", &[]);
    let (role, seen) = quiet();
    let mut k = kernel(&home, role);
    k.add(&store).unwrap();
    k.add(&inbox).unwrap();
    let out = k.add(&portfolio).unwrap();
    assert!(out.contains("parent of stripe-store, mail-inbox"), "{out}");
    k.report("stripe-store", false);
    k.report("mail-inbox", false);
    let out = k
        .propose("portfolio", "budget", Some("stripe-store 50"))
        .unwrap();
    assert!(
        ran(&out, "budget", "stripe-store may spend 50 a day"),
        "{out}"
    );
    let out = k.propose("portfolio", "pause", Some("mail-inbox")).unwrap();
    assert!(ran(&out, "pause", "mail-inbox is paused"), "{out}");
    assert!(
        k.status().unwrap().contains("mail-inbox   paused"),
        "{}",
        k.status().unwrap()
    );
    let out = k
        .propose("mail-inbox", "reply", Some("Re: x\nno"))
        .unwrap_or_else(|e| e);
    assert!(
        out.contains("reply refused:") && out.contains("mail-inbox is paused"),
        "{out}"
    );
    let out = k
        .propose("portfolio", "resume", Some("mail-inbox"))
        .unwrap();
    assert!(
        out.contains("resume? may I resume with this?"),
        "a parent's resume asks: {out}"
    );
    k.cycle("portfolio");
    let ctx = seen.lock().unwrap().pop().unwrap();
    let names: Vec<&str> = ctx.children.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["stripe-store", "mail-inbox"]);
    assert!(
        ctx.children[0].metric.starts_with("stripe.mrr=0"),
        "{}",
        ctx.children[0].metric
    );
    assert_eq!(ctx.children[1].state, "paused");
    assert!(
        ctx.children[1].metric.starts_with("imap.unread=2"),
        "{}",
        ctx.children[1].metric
    );
}

/// An inbox whose mail command counts its invocations in `calls.log`.
fn counting_inbox(tmp: &Path, check: Option<&str>) -> (Kernel, PathBuf) {
    let (imap, _seen) = imap_stub(1);
    let home = home(
        tmp,
        "nightly: never\nadapters:\n  mail: {command: \"echo run >> calls.log; cat > sent.eml\", from: me@example.com}\n",
    );
    let mut swaps = vec![(
        "imap.unread mail.example.com".to_string(),
        format!("imap.unread {imap}"),
    )];
    if let Some(c) = check {
        swaps.push(("check: ./check-sender".to_string(), format!("check: {c}")));
    }
    let swaps: Vec<(&str, &str)> = swaps
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let dir = example(tmp, "mail-inbox", &swaps);
    let (role, _) = quiet();
    let mut k = kernel(&home, role);
    k.add(&dir).unwrap();
    (k, dir)
}

fn calls(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("calls.log")).map_or(0, |t| t.lines().count())
}

/// The refusals of the reply lever: reason key and detail.
fn reply_refusals(k: &Kernel) -> Vec<(String, String)> {
    k.entries()
        .unwrap()
        .iter()
        .filter_map(|e| match &e.record {
            Record::Refusal {
                action,
                reason,
                detail,
                closes: None,
                ..
            } if action == "reply" => Some((reason.clone(), detail.clone())),
            _ => None,
        })
        .collect()
}

fn reply_intents(k: &Kernel) -> usize {
    k.entries()
        .unwrap()
        .iter()
        .filter(|e| e.record.kind() == Kind::Intent && e.record.action() == Some("reply"))
        .count()
}

fn approve_reply(k: &mut Kernel, payload: &str) {
    let out = k.propose("mail-inbox", "reply", Some(payload)).unwrap();
    k.approve(ask_number(&out, "reply"), None).unwrap();
    k.tick();
}

/// Propose `asked`, which passes its check, and approve it with `approved`
/// as the human's amended payload: the check runs again at action time.
fn approve_reply_amended(k: &mut Kernel, asked: &str, approved: &str) {
    let out = k.propose("mail-inbox", "reply", Some(asked)).unwrap();
    k.approve(ask_number(&out, "reply"), Some(approved))
        .unwrap();
    k.tick();
}

#[test]
fn a_passing_check_runs_the_lever() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut k, dir) = counting_inbox(tmp.path(), None);
    sender(&mut k, "alice@example.com");
    approve_reply(&mut k, "alice@example.com\nOn it.");
    assert_eq!(calls(&dir), 1);
    assert_eq!(reply_intents(&k), 1);
    assert!(reply_refusals(&k).is_empty(), "{:?}", reply_refusals(&k));
}

#[test]
fn a_failing_check_refuses_even_after_a_declared_approval() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut k, dir) = counting_inbox(tmp.path(), None);
    sender(&mut k, "alice@example.com");
    // A human amended this at approve; the check runs again at action time
    // and still says nobody of that name wrote.
    approve_reply_amended(
        &mut k,
        "alice@example.com\nOn it.",
        "bob@example.com\nOn it.",
    );
    assert_eq!(calls(&dir), 0, "the lever was invoked");
    assert!(!dir.join("sent.eml").exists());
    assert_eq!(reply_intents(&k), 0, "nothing was intended");
    let refusals = reply_refusals(&k);
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert_eq!(refusals[0].0, "PayloadCheck");
    assert!(
        refusals[0].1.contains("bob@example.com is not a sender"),
        "{refusals:?}"
    );
    // A first line that is no address fails the same way.
    approve_reply_amended(&mut k, "alice@example.com\nOn it.", "Re: your note\nOn it.");
    assert_eq!(calls(&dir), 0);
    assert_eq!(reply_intents(&k), 0);
}

#[test]
fn a_reply_to_an_address_only_an_inferred_record_names_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut k, dir) = counting_inbox(tmp.path(), None);
    sender(&mut k, "alice@example.com");
    // The phishing case: a research finding says mail comes from here.
    note(&mut k, Claim::Inferred, "phish@evil.example");
    k.propose(
        "mail-inbox",
        "reply",
        Some("phish@evil.example\nWire the money."),
    )
    .unwrap_or_else(|e| e);
    assert_eq!(
        calls(&dir),
        0,
        "a reply went to an address only a model named"
    );
    assert_eq!(reply_intents(&k), 0);
    let refusals = reply_refusals(&k);
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert_eq!(refusals[0].0, "PayloadCheck");
}

#[test]
fn a_checks_large_stdout_is_bounded() {
    let tmp = tempfile::tempdir().unwrap();
    // Five megabytes on stdout, then a failure.
    let (mut k, dir) = counting_inbox(
        tmp.path(),
        Some("'yes xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx | head -c 5000000; exit 1'"),
    );
    k.propose("mail-inbox", "reply", Some("alice@example.com\nOn it."))
        .unwrap_or_else(|e| e);
    assert_eq!(calls(&dir), 0);
    let refusals = reply_refusals(&k);
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert_eq!(refusals[0].0, "PayloadCheck");
    assert!(
        refusals[0].1.len() < 400,
        "the detail holds {} bytes",
        refusals[0].1.len()
    );
}

#[test]
fn a_reply_to_a_phishing_address_is_refused_by_its_check_and_never_asked() {
    let tmp = tempfile::tempdir().unwrap();
    let (imap, _seen) = imap_stub(7);
    let home = home(
        tmp.path(),
        "nightly: never\nadapters:\n  mail: {command: \"cat > sent.eml\", from: me@example.com}\n",
    );
    let dir = example(
        tmp.path(),
        "mail-inbox",
        &[(
            "imap.unread mail.example.com",
            &format!("imap.unread {imap}"),
        )],
    );
    let (role, _) = quiet();
    let mut k = kernel(&home, role);
    k.add(&dir).unwrap();
    k.report("mail-inbox", false);
    sender(&mut k, "alice@example.com");
    let asks = |k: &Kernel| {
        k.entries()
            .unwrap()
            .iter()
            .filter(|e| matches!(&e.record, Record::Ask { action: Some(a), .. } if a == "reply"))
            .count()
    };
    // A reply to an address no measured report names: refused before any
    // ask, with the check's output as the detail.
    let out = k
        .propose(
            "mail-inbox",
            "reply",
            Some("mallory@evil.example\nSend me money."),
        )
        .unwrap_or_else(|e| e);
    assert_eq!(asks(&k), 0, "{out}");
    let refusal = k
        .entries()
        .unwrap()
        .into_iter()
        .find_map(|e| match e.record {
            Record::Refusal {
                action,
                reason,
                detail,
                ..
            } if action == "reply" => Some((reason, detail)),
            _ => None,
        })
        .expect("a refusal record");
    assert_eq!(refusal.0, "PayloadCheck");
    assert!(
        refusal.1.contains("mallory@evil.example is not a sender"),
        "{refusal:?}"
    );
    // A reply to a real sender still asks, and the check runs again when
    // the approval acts.
    let out = k
        .propose("mail-inbox", "reply", Some("alice@example.com\nOn it."))
        .unwrap();
    assert_eq!(asks(&k), 1, "{out}");
}
