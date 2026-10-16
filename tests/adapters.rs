//! Every adapter against a stub: metrics measured through the warden with
//! the one key each is scoped to, levers pulled with their payloads, and a
//! venture that is nothing but a mandate.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use avsm::adapter::Metric;
use avsm::config::Adapters;
use avsm::kernel::Kernel;
use avsm::record::{Kind, Record};
use avsm::role::{Context, InferRequest, Role};
use avsm::warden::LocalWarden;
use common::{imap_stub, route, Stub};

/// A role that never says anything.
struct Quiet;

impl Role for Quiet {
    fn decide(
        &mut self,
        _ctx: &Context,
        _infer: &mut dyn FnMut(&InferRequest) -> Result<String, String>,
    ) -> Result<Vec<Record>, String> {
        Ok(vec![])
    }
}

fn home(parent: &Path, stub: &str, mail: &str) -> PathBuf {
    let home = parent.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.yaml"),
        format!(
            "nightly: never\nadapters:\n  stripe: {{url: {stub}/v1}}\n  github: {{url: {stub}}}\n  mail: {{command: \"{mail}\", from: avsm@example.com}}\n"
        ),
    )
    .unwrap();
    std::fs::write(
        home.join("keys"),
        "stripe=sk_test_1\ngithub=ghp_1\nimap=alice:s3cret\n",
    )
    .unwrap();
    std::fs::set_permissions(home.join("keys"), std::fs::Permissions::from_mode(0o600)).unwrap();
    home
}

fn kernel(home: &Path) -> Kernel {
    let warden = LocalWarden::open(home).unwrap();
    Kernel::with(home, Box::new(Quiet), Box::new(warden)).unwrap()
}

/// A venture directory holding only a mandate.
fn venture(parent: &Path, name: &str, mandate: &str) -> PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).unwrap();
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

#[test]
fn metric_adapters_measure_through_the_warden_with_scoped_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let stub = Stub::start(vec![
        route(
            "GET",
            "/v1/subscriptions",
            200,
            r#"{"data":[{"id":"sub_1","items":{"data":[{"quantity":2,"price":{"unit_amount":2500,"recurring":{"interval":"month","interval_count":1}}}]}}],"has_more":false}"#,
        ),
        route(
            "GET",
            "/v1/charges",
            200,
            r#"{"data":[{"id":"ch_1","amount":1000,"status":"succeeded"},{"id":"ch_2","amount":500,"status":"failed"}],"has_more":false}"#,
        ),
        route(
            "GET",
            "/repos/o/r/issues",
            200,
            r#"[{"number":1},{"number":2,"pull_request":{"url":"x"}},{"number":3}]"#,
        ),
        route(
            "GET",
            "/repos/o/r/commits/main/check-runs",
            200,
            r#"{"check_runs":[{"conclusion":"failure"},{"conclusion":"success"},{"conclusion":"failure"}]}"#,
        ),
        route("GET", "/repos/o/r", 200, r#"{"default_branch":"main"}"#),
        route("GET", "/health", 204, ""),
    ]);
    let (imap, imap_seen) = imap_stub(4);
    let home = home(tmp.path(), &stub.url, "cat > sent.eml");
    let mut k = kernel(&home);
    let mandate = |metric: &str| {
        format!("purpose: Watch.\nmetric: {metric}\ncontroller: {{kind: model}}\ncadence: 1h\n")
    };
    let cases: Vec<(&str, String, f64, &str)> = vec![
        ("mrr", mandate("stripe.mrr"), 50.0, "1 active subscriptions"),
        (
            "charges",
            mandate("stripe.charges_today"),
            10.0,
            "1 charges since midnight UTC",
        ),
        (
            "issues",
            mandate("github.open_issues o/r"),
            2.0,
            "open issues in o/r",
        ),
        (
            "ci",
            mandate("github.failed_ci o/r"),
            2.0,
            "2 of 3 checks failed on main",
        ),
        (
            "health",
            mandate(&format!("http.status {}/health", stub.url)),
            204.0,
            "GET ",
        ),
        ("file", mandate("file.number data/n"), 42.5, ""),
        ("shell", mandate("shell echo 7; echo 8"), 8.0, ""),
        (
            "inbox",
            mandate(&format!("imap.unread {imap}")),
            4.0,
            "unseen in INBOX",
        ),
    ];
    for (name, text, _, _) in &cases {
        let dir = venture(tmp.path(), name, text);
        if *name == "file" {
            std::fs::create_dir_all(dir.join("data")).unwrap();
            std::fs::write(dir.join("data/n"), "old\n42.5\n").unwrap();
        }
        let out = k.add(&dir).unwrap();
        assert!(out.contains("through the warden"), "{out}");
    }
    for (name, _, value, note) in &cases {
        k.report(name, false);
        let (got, got_note) = last_report(&k, name);
        assert_eq!(got, Some(*value), "{name}: {got_note}");
        assert!(got_note.starts_with(note), "{name}: {got_note}");
    }

    // Each adapter carried its own key and nobody else's.
    for r in stub.seen_at("/v1/") {
        assert_eq!(
            r.header("authorization").as_deref(),
            Some("bearer sk_test_1"),
            "{r:?}"
        );
    }
    for r in stub.seen_at("/repos/") {
        assert_eq!(
            r.header("authorization").as_deref(),
            Some("bearer ghp_1"),
            "{r:?}"
        );
        assert_eq!(r.header("user-agent").as_deref(), Some("avsm"));
    }
    assert!(stub
        .seen_at("/health")
        .iter()
        .all(|r| r.header("authorization").is_none()));
    let lines = imap_seen.join().unwrap();
    assert_eq!(lines[0], "a1 LOGIN \"alice\" \"s3cret\"", "{lines:?}");
    assert_eq!(lines[1], "a2 STATUS INBOX (UNSEEN)");
    assert_eq!(lines[2], "a3 LOGOUT");

    // Without its key an adapter measures nothing and says which key.
    let (value, note) = Metric::StripeMrr.measure(tmp.path(), None, &Adapters::default());
    assert_eq!(value, None);
    assert_eq!(note, "stripe.mrr needs a key named stripe in the keys file");
    let (value, note) = Metric::FileNumber {
        path: "missing".into(),
    }
    .measure(tmp.path(), None, &Adapters::default());
    assert_eq!(value, None);
    assert!(note.starts_with("cannot read"), "{note}");

    // The status line and the ledger line read as any other metric.
    let status = k.status().unwrap();
    assert!(
        status.contains("mrr          running  stripe.mrr=50"),
        "{status}"
    );
    assert_eq!(
        k.ledger.verify().unwrap(),
        k.entries().unwrap().len() as u64
    );
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
fn lever_adapters_pull_through_the_warden() {
    let tmp = tempfile::tempdir().unwrap();
    let stub = Stub::start(vec![
        route(
            "POST",
            "/repos/o/r/issues",
            201,
            r#"{"number":7,"html_url":"https://github.com/o/r/issues/7"}"#,
        ),
        route(
            "POST",
            "/repos/o/r/issues/3/comments",
            201,
            r#"{"html_url":"https://github.com/o/r/issues/3#c1"}"#,
        ),
        route(
            "POST",
            "/repos/o/r/issues/9/comments",
            201,
            r#"{"html_url":"https://github.com/o/r/issues/9#c2"}"#,
        ),
        route("POST", "/hook", 200, r#"{"ok":true}"#),
        route(
            "POST",
            "/repos/o/r/pulls",
            201,
            r#"{"number":12,"html_url":"https://github.com/o/r/pull/12"}"#,
        ),
    ]);
    let home = home(tmp.path(), &stub.url, "cat > sent.eml");

    // A repository to open a pull request against: a bare origin with one
    // commit on main, and the venture directory as its checkout.
    let seed = tmp.path().join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-q", "-b", "main"]);
    std::fs::write(seed.join("README"), "hello\n").unwrap();
    git(&seed, &["add", "README"]);
    git(&seed, &["commit", "-q", "-m", "seed"]);
    let origin = tmp.path().join("origin.git");
    git(tmp.path(), &["clone", "-q", "--bare", "seed", "origin.git"]);
    git(tmp.path(), &["clone", "-q", "origin.git", "desk"]);
    let dir = tmp.path().join("desk");
    std::fs::write(
        dir.join("mandate.yaml"),
        format!(
            "purpose: Keep the repo tidy.\nmetric: shell echo 1\ncontroller: {{kind: model}}\ncadence: 1h\n\
             permitted:\n\
             - {{name: file-issue, adapter: github.issue o/r, cap: 3/day, reversible: true}}\n\
             - {{name: comment, adapter: \"github.comment o/r#3\", cap: 3/day, reversible: true}}\n\
             - {{name: reply, adapter: github.comment o/r, cap: 3/day, reversible: true}}\n\
             - {{name: hook, adapter: http.post {}/hook, cap: 3/day, reversible: false}}\n\
             - {{name: mail, adapter: mail.send you@example.com, cap: 3/day, reversible: false}}\n\
             - {{name: pr, adapter: github.pr o/r, cap: 3/day, reversible: false}}\n",
            stub.url
        ),
    )
    .unwrap();
    let mut k = kernel(&home);
    let out = k.add(&dir).unwrap();
    assert!(
        out.contains("may file-issue  3/day  reversible, takes text and will ask first"),
        "{out}"
    );
    assert!(
        out.contains("may pr  3/day  irreversible, will ask first, takes diff and will ask first"),
        "{out}"
    );

    // Reversible levers run on a human's proposal at once.
    let out = k
        .propose("desk", "file-issue", Some("A title\nThe body\nmore"))
        .unwrap();
    assert!(
        out.contains("action    file-issue: done  opened issue #7 https://github.com/o/r/issues/7"),
        "{out}"
    );
    let seen = stub.seen_at("/repos/o/r/issues");
    assert_eq!(seen[0].json()["title"], "A title");
    assert_eq!(seen[0].json()["body"], "The body\nmore");
    assert_eq!(
        seen[0].header("authorization").as_deref(),
        Some("bearer ghp_1")
    );
    let out = k.propose("desk", "comment", Some("Thanks!")).unwrap();
    assert!(out.contains("comment: done  commented on #3"), "{out}");
    let out = k.propose("desk", "reply", Some("#9\nSee above.")).unwrap();
    assert!(out.contains("reply: done  commented on #9"), "{out}");
    assert_eq!(
        stub.seen_at("/repos/o/r/issues/9/comments")[0].json()["body"],
        "See above."
    );
    let out = k.propose("desk", "reply", Some("no number here")).unwrap();
    assert!(
        out.contains("reply: failed  github.comment: the first line of the text names the issue"),
        "{out}"
    );

    // Irreversible levers ask first; approved, they run as intent then outcome.
    let diff = "--- a/README\n+++ b/README\n@@ -1 +1,2 @@\n hello\n+world\n";
    for (lever, payload) in [
        ("hook", "{\"n\": 1}"),
        ("mail", "Subject line\nHello there"),
        ("pr", diff),
    ] {
        let out = k.propose("desk", lever, Some(payload)).unwrap();
        assert!(
            out.contains(&format!("ask       {lever}? may I {lever} with this?")),
            "{out}"
        );
        let ask = out
            .lines()
            .find(|l| l.contains(&format!("{lever}? may I")))
            .unwrap()
            .trim_start_matches('#')
            .split_whitespace()
            .next()
            .unwrap()
            .parse::<u64>()
            .unwrap();
        k.approve(ask, None).unwrap();
        k.tick();
        let entries = k.entries().unwrap();
        let outcome = entries.last().unwrap();
        assert_eq!(outcome.record.kind(), Kind::Outcome, "{}", outcome.line());
        assert!(
            outcome.record.summary().contains(" done"),
            "{}",
            outcome.line()
        );
    }
    let summaries: Vec<String> = k
        .entries()
        .unwrap()
        .iter()
        .filter(|e| e.record.kind() == Kind::Outcome)
        .map(|e| e.record.summary())
        .collect();
    assert!(
        summaries[0].contains("/hook answered 200: {\"ok\":true}"),
        "{summaries:?}"
    );
    assert_eq!(stub.seen_at("/hook")[0].json()["n"], 1);
    assert!(
        summaries[1].contains("sent to you@example.com: Subject line"),
        "{summaries:?}"
    );
    let sent = std::fs::read_to_string(dir.join("sent.eml")).unwrap();
    assert_eq!(
        sent,
        "To: you@example.com\nFrom: avsm@example.com\nSubject: Subject line\nMIME-Version: 1.0\nContent-Type: text/plain; charset=utf-8\n\nHello there\n"
    );
    assert!(
        summaries[2].contains("opened pull request #12 https://github.com/o/r/pull/12"),
        "{summaries:?}"
    );
    let pr = &stub.seen_at("/repos/o/r/pulls")[0];
    assert!(
        pr.json()["head"]
            .as_str()
            .unwrap()
            .starts_with("avsm/desk-"),
        "{pr:?}"
    );
    assert_eq!(pr.json()["base"], "main");
    assert_eq!(pr.json()["title"], "desk: pr");
    assert!(
        pr.json()["body"]
            .as_str()
            .unwrap()
            .contains("desk: pr is allowed"),
        "{pr:?}"
    );
    let branch = git(&origin, &["branch", "--list", "avsm/*"])
        .trim_start_matches("* ")
        .trim()
        .to_string();
    assert!(branch.starts_with("avsm/desk-"), "{branch}");
    assert_eq!(
        git(&origin, &["show", &format!("{branch}:README")]),
        "hello\nworld"
    );
    assert_eq!(
        git(&dir, &["status", "--porcelain", "--untracked-files=no"]),
        "",
        "the checkout is untouched"
    );
    assert_eq!(
        git(&dir, &["worktree", "list"]).lines().count(),
        1,
        "no worktree left behind"
    );
    assert_eq!(
        k.ledger.verify().unwrap(),
        k.entries().unwrap().len() as u64
    );
}

#[test]
fn a_mandate_alone_is_a_venture_when_everything_is_an_adapter() {
    let tmp = tempfile::tempdir().unwrap();
    let stub = Stub::start(vec![]);
    let home = home(tmp.path(), &stub.url, "cat > sent.eml");
    let mut k = kernel(&home);
    let dir = venture(
        tmp.path(),
        "lone",
        "purpose: Watch disk.\nmetric: shell echo 5\ncontroller: {kind: model}\ncadence: 1h\n\
         permitted:\n- {name: nudge, adapter: mail.send me@example.com, cap: 1/day, reversible: false}\n",
    );
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    k.add(&dir).unwrap();
    k.report("lone", false);
    assert_eq!(last_report(&k, "lone").0, Some(5.0));

    let dir = venture(
        tmp.path(),
        "mute",
        "purpose: Watch.\nmetric: count\ncontroller: {kind: model}\ncadence: 1h\n",
    );
    let err = k.add(&dir).unwrap_err();
    assert!(err.contains("holds no report program"), "{err}");
    assert!(err.contains("stripe.mrr"), "{err}");
}

/// The research lever: a human's question goes to the stub command as
/// JSON, and its findings come back as inferred records that cite the
/// action, with source and confidence; a model's question asks first.
#[test]
fn research_runs_the_configured_command_and_writes_inferred_findings() {
    let tmp = tempfile::tempdir().unwrap();
    let stub = Stub::start(vec![]);
    let home = home(tmp.path(), &stub.url, "cat > sent.eml");
    let finder = tmp.path().join("finder");
    std::fs::write(
        &finder,
        format!(
            "#!/bin/sh\ncat > {}/asked.json\nprintf '%s' '{{\"results\": [\
             {{\"text\": \"Rivals charge 9\", \"source\": \"https://rival.example/pricing\", \"confidence\": 0.8, \"value\": 9, \"topic\": \"pricing\", \"api_key\": \"leak\"}},\
             {{\"text\": \"Nobody knows\"}}]}}'\n",
            tmp.path().display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&finder, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        home.join("config.yaml"),
        format!(
            "nightly: never\nresearch: finder\nbackends:\n  finder: {{kind: command, run: {}}}\n",
            finder.display()
        ),
    )
    .unwrap();
    let dir = venture(
        tmp.path(),
        "shop",
        "purpose: Sell more.\nmetric: shell echo 3\ncontroller: {kind: model}\ncadence: 1h\n\
         permitted:\n- {name: research, cap: 5/day, reversible: true, delegated: true}\n",
    );
    let mut k = kernel(&home);
    let out = k.add(&dir).unwrap();
    assert!(
        out.contains("may research  5/day  reversible, takes text"),
        "{out}"
    );
    let out = k
        .propose("shop", "research", Some("What do rivals charge?"))
        .unwrap();
    assert!(
        out.contains("measured  action    research: done  2 findings"),
        "{out}"
    );
    assert!(
        out.contains("inferred  report    pricing=9  Rivals charge 9"),
        "{out}"
    );
    assert!(
        out.contains("inferred  report    research=?  Nobody knows"),
        "{out}"
    );
    let asked: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(tmp.path().join("asked.json")).unwrap())
            .unwrap();
    assert_eq!(asked["question"], "What do rivals charge?");
    assert_eq!(asked["venture"], "shop");
    let entries = k.entries().unwrap();
    let action = entries
        .iter()
        .find(|e| e.record.kind() == Kind::Action)
        .unwrap();
    let finding = entries
        .iter()
        .find(|e| e.record.summary().starts_with("pricing=9"))
        .unwrap();
    assert!(
        matches!(&finding.record, Record::Report { source: Some(s), confidence: Some(c), cites, .. }
        if s == "https://rival.example/pricing" && *c == 0.8 && *cites == vec![action.seq])
    );
    assert!(!finding.record.canonical().contains("leak"), "stripped");
    let why = k.why(finding.seq).unwrap();
    assert!(why.contains("inferred by a model"), "{why}");
    assert!(
        why.contains("source     https://rival.example/pricing"),
        "{why}"
    );
    assert!(why.contains("confidence 0.8"), "{why}");
    assert!(
        why.contains(&format!("cites      #{}", action.seq)),
        "{why}"
    );
    assert!(why.contains("authorizes nothing on its own"), "{why}");

    // A question nobody can answer: no backend, the action fails and says so.
    std::fs::write(home.join("config.yaml"), "nightly: never\n").unwrap();
    let mut k = kernel(&home);
    let out = k.propose("shop", "research", Some("Anything?")).unwrap();
    assert!(
        out.contains("research: failed  no research backend"),
        "{out}"
    );
    assert_eq!(
        k.ledger.verify().unwrap(),
        k.entries().unwrap().len() as u64
    );
}
