//! Parents allocate, children act: budgets moved between two children, a
//! child paused by its parent, thresholds set from above, shared quotas
//! refused before the gate and shown in why, loops refused at add, and
//! the parent's nightly view of its children.

mod common;

use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use avsm::kernel::Kernel;
use avsm::record::{Claim, Kind, Record};
use avsm::role::{Context, InferRequest, Role};
use avsm::warden::LocalWarden;
use common::{route, Stub};

/// A role that answers from a script and keeps what it was shown.
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

fn home(parent: &Path, config: &str) -> PathBuf {
    let home = parent.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.yaml"), config).unwrap();
    home
}

fn kernel(home: &Path, role: Scripted) -> Kernel {
    let warden = LocalWarden::open(home).unwrap();
    Kernel::with(home, Box::new(role), Box::new(warden)).unwrap()
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

fn venture(parent: &Path, name: &str, mandate: &str) -> PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("mandate.yaml"), mandate).unwrap();
    dir
}

fn script(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

const PORTFOLIO: &str = "purpose: Put money where it works.\nmetric: shell echo 1\ncontroller: {kind: model}\ncadence: 1h\n\
    children: [shop, farm]\n\
    permitted:\n\
    - {name: budget, cap: 9/day, reversible: true, delegated: true}\n\
    - {name: pause, cap: 9/day, reversible: true, delegated: true}\n\
    - {name: resume, cap: 9/day, reversible: true}\n\
    - {name: threshold, cap: 9/day, reversible: true, delegated: true}\n";

fn child(name: &str, extra: &str) -> String {
    format!(
        "purpose: Run {name}.\nmetric: shell echo 1\ncontroller: {{kind: model}}\ncadence: 1h\nparent: portfolio\n\
         permitted:\n- {{name: spend, run: \"echo spent >> spent\", cap: 9/day, reversible: true, delegated: true, cost: 10}}\n{extra}"
    )
}

fn lines_of(k: &mut Kernel, venture: &str, lever: &str, payload: &str) -> String {
    k.propose(venture, lever, Some(payload)).unwrap()
}

#[test]
fn a_budget_moves_between_two_children() {
    let tmp = tempfile::tempdir().unwrap();
    let home = home(tmp.path(), "nightly: never\n");
    let (role, _) = quiet();
    let mut k = kernel(&home, role);
    let shop = venture(tmp.path(), "shop", &child("shop", ""));
    let farm = venture(tmp.path(), "farm", &child("farm", ""));
    let portfolio = venture(tmp.path(), "portfolio", PORTFOLIO);
    k.add(&shop).unwrap();
    k.add(&farm).unwrap();
    let out = k.add(&portfolio).unwrap();
    assert!(out.contains("parent of shop, farm"), "{out}");

    // Ten for the shop, nothing for the farm.
    let out = lines_of(&mut k, "portfolio", "budget", "shop 10");
    assert!(
        out.contains("action    budget: done  shop may spend 10 a day"),
        "{out}"
    );
    let out = lines_of(&mut k, "portfolio", "budget", "farm 0");
    assert!(out.contains("farm may spend 0 a day"), "{out}");
    let out = k.propose("shop", "spend", None).unwrap();
    assert!(out.contains("action    spend: done"), "{out}");
    let out = k.propose("shop", "spend", None).unwrap();
    assert!(
        out.contains(
            "spend refused: the parent's budget is 10 a day, 10 is spent and this costs 10"
        ),
        "{out}"
    );
    let out = k.propose("farm", "spend", None).unwrap();
    assert!(
        out.contains("the parent's budget is 0 a day, 0 is spent and this costs 10"),
        "{out}"
    );
    assert!(!farm.join("spent").exists());

    // The budget moves: now the farm may, the shop may not.
    lines_of(&mut k, "portfolio", "budget", "farm 10");
    lines_of(&mut k, "portfolio", "budget", "shop 0");
    let out = k.propose("farm", "spend", None).unwrap();
    assert!(out.contains("action    spend: done"), "{out}");
    assert_eq!(
        std::fs::read_to_string(farm.join("spent")).unwrap(),
        "spent\n"
    );
    let out = k.propose("shop", "spend", None).unwrap();
    assert!(out.contains("the parent's budget is 0 a day"), "{out}");
    let seq = out
        .trim_start_matches('#')
        .split_whitespace()
        .next()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    let why = k.why(seq).unwrap();
    assert!(
        why.contains("budget     10 of 0 a day spent  set by the parent in #"),
        "{why}"
    );
    assert!(
        why.contains("permitted  spend  9/day  reversible, delegated, costs 10"),
        "{why}"
    );

    // A parent cannot allocate to a venture that is not its child.
    k.add(&venture(
        tmp.path(),
        "loner",
        "purpose: Be alone.\nmetric: shell echo 1\ncontroller: {kind: model}\ncadence: 1h\n",
    ))
    .unwrap();
    let out = lines_of(&mut k, "portfolio", "budget", "loner 5");
    assert!(
        out.contains(
            "budget did not run: loner is not a child of portfolio: both mandates must say so"
        ),
        "{out}"
    );
    let out = lines_of(&mut k, "portfolio", "budget", "stranger 5");
    assert!(
        out.contains("budget did not run: stranger is not a venture"),
        "{out}"
    );
    let out = lines_of(&mut k, "portfolio", "budget", "shop lots");
    assert!(out.contains("budget takes a whole number"), "{out}");
    assert_eq!(
        k.ledger.verify().unwrap(),
        k.entries().unwrap().len() as u64
    );
}

#[test]
fn a_child_paused_by_its_parent_cannot_act_and_its_run_stops() {
    let tmp = tempfile::tempdir().unwrap();
    let home = home(tmp.path(), "nightly: never\n");
    let (role, _) = quiet();
    let mut k = kernel(&home, role);
    let shop = venture(tmp.path(), "shop", &child("shop", "restart: never\n"));
    script(&shop, "run", "sleep 60");
    k.add(&venture(tmp.path(), "farm", &child("farm", "")))
        .unwrap();
    k.add(&shop).unwrap();
    k.add(&venture(tmp.path(), "portfolio", PORTFOLIO)).unwrap();
    k.tick();
    assert!(k.run_pid("shop").is_some());

    let out = lines_of(&mut k, "portfolio", "pause", "shop");
    assert!(
        out.contains("action    pause: done  shop is paused"),
        "{out}"
    );
    assert!(k.status().unwrap().contains("shop         paused"));
    k.tick();
    assert_eq!(k.run_pid("shop"), None, "the run is stopped");
    let out = k.propose("shop", "spend", None).unwrap();
    assert!(out.contains("spend refused: shop is paused"), "{out}");

    // The parent's resume is declared-only: it asks, a human approves.
    let out = lines_of(&mut k, "portfolio", "resume", "shop");
    assert!(
        out.contains(
            "ask       resume? may I resume with this?  proposed by hand  [text \"shop\"]"
        ),
        "{out}"
    );
    let ask = out
        .lines()
        .find(|l| l.contains("resume? may I"))
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
    let resumed = entries
        .iter()
        .rev()
        .find(|e| e.record.kind() == Kind::Action && e.record.action() == Some("resume"))
        .expect("the resume ran");
    assert_eq!(resumed.record.summary(), "resume: done  shop may run again");
    assert!(k.status().unwrap().contains("shop         running"));
    k.tick();
    assert!(k.run_pid("shop").is_some(), "the run is back");
    let out = k.propose("shop", "spend", None).unwrap();
    assert!(out.contains("action    spend: done"), "{out}");
}

#[test]
fn the_parent_sets_a_childs_threshold() {
    let tmp = tempfile::tempdir().unwrap();
    let home = home(tmp.path(), "nightly: never\n");
    let (role, _) = quiet();
    let mut k = kernel(&home, role);
    let farm = venture(
        tmp.path(),
        "farm",
        "purpose: Keep the greenhouse cool.\nmetric: temperature\ncontroller: {kind: threshold, above: 30, action: vent}\ncadence: 1h\nparent: portfolio\n\
         permitted:\n- {name: vent, run: \"echo vented >> vents\", cap: 9/day, reversible: true}\n",
    );
    script(&farm, "report", "echo 27");
    k.add(&farm).unwrap();
    k.add(&venture(tmp.path(), "shop", &child("shop", "")))
        .unwrap();
    k.add(&venture(tmp.path(), "portfolio", PORTFOLIO)).unwrap();
    k.report("farm", false);
    assert!(!farm.join("vents").exists(), "27 is under 30");
    let out = lines_of(&mut k, "portfolio", "threshold", "farm above 25");
    assert!(out.contains("threshold: done  farm acts above 25"), "{out}");
    k.report("farm", false);
    assert!(farm.join("vents").exists(), "27 is over 25");
    let entries = k.entries().unwrap();
    let vent = entries
        .iter()
        .rev()
        .find(|e| e.record.kind() == Kind::Action && e.record.action() == Some("vent"))
        .unwrap();
    let why = k.why(vent.seq).unwrap();
    assert!(
        why.contains("threshold  temperature above 25  set by the parent in #"),
        "{why}"
    );
}

#[test]
fn shared_quotas_are_refused_before_the_gate_and_shown_in_why() {
    let tmp = tempfile::tempdir().unwrap();
    let stub = Stub::start(vec![route("POST", "/hook", 200, "{}")]);
    let home = home(
        tmp.path(),
        "nightly: never\nquotas: {spend: 15/day, sends: 1/day, asks: 2/day}\n",
    );
    let (role, _) = quiet();
    let mut k = kernel(&home, role);
    let dir = venture(
        tmp.path(),
        "shop",
        &format!(
            "purpose: Sell.\nmetric: shell echo 1\ncontroller: {{kind: model}}\ncadence: 1h\n\
             permitted:\n\
             - {{name: buy, run: \"echo bought >> buys\", cap: 9/day, reversible: true, delegated: true, cost: 10}}\n\
             - {{name: post, adapter: http.post {}/hook, cap: 9/day, reversible: false, delegated: true}}\n\
             - {{name: w1, run: \"true\", cap: 9/day, reversible: false}}\n\
             - {{name: w2, run: \"true\", cap: 9/day, reversible: false}}\n\
             - {{name: w3, run: \"true\", cap: 9/day, reversible: false}}\n",
            stub.url
        ),
    );
    k.add(&dir).unwrap();
    // Spend: 10 of 15, then no room for another 10.
    let out = k.propose("shop", "buy", None).unwrap();
    assert!(out.contains("action    buy: done"), "{out}");
    let out = k.propose("shop", "buy", None).unwrap();
    assert!(
        out.contains(
            "buy refused before the gate: the spend quota is 15/day and 10 are already used"
        ),
        "{out}"
    );
    let seq = out
        .trim_start_matches('#')
        .split_whitespace()
        .next()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    let why = k.why(seq).unwrap();
    assert!(why.contains("quota      spend 15/day, 10 used"), "{why}");
    assert!(
        why.contains("because    the spend quota is 15/day and 10 are already used"),
        "{why}"
    );
    // Sends: one post goes, the second is refused.
    let out = k.propose("shop", "post", Some("{\"n\":1}")).unwrap();
    assert!(out.contains("outcome   #"), "{out}");
    let out = k.propose("shop", "post", Some("{\"n\":2}")).unwrap();
    assert!(
        out.contains(
            "post refused before the gate: the sends quota is 1/day and 1 are already used"
        ),
        "{out}"
    );
    assert_eq!(stub.seen().len(), 1);
    // Asks: two may be open, the third is not written.
    for lever in ["w1", "w2", "w3"] {
        k.propose("shop", lever, None).unwrap();
    }
    let asks = k.asks().unwrap();
    assert_eq!(asks.lines().count(), 2, "{asks}");
    assert!(
        asks.contains("w1?") && asks.contains("w2?") && !asks.contains("w3?"),
        "{asks}"
    );
    let out = k.propose("shop", "w3", None).unwrap();
    assert!(
        out.contains("w3 needs a decision, but the asks quota is used up; not asking"),
        "{out}"
    );
}

#[test]
fn loops_and_disagreements_are_refused_at_add() {
    let tmp = tempfile::tempdir().unwrap();
    let home = home(tmp.path(), "nightly: never\n");
    let (role, _) = quiet();
    let mut k = kernel(&home, role);
    let plain = |name: &str, children: &str, parent: &str| {
        format!(
            "purpose: Be {name}.\nmetric: shell echo 1\ncontroller: {{kind: model}}\ncadence: 1h\n{children}{parent}"
        )
    };
    // The mandate itself refuses the shortest loop.
    k.add(&venture(
        tmp.path(),
        "a",
        &plain("a", "children: [b]\n", ""),
    ))
    .unwrap();
    let err = k
        .add(&venture(
            tmp.path(),
            "b",
            &plain("b", "children: [a]\n", "parent: a\n"),
        ))
        .unwrap_err();
    assert!(
        err.contains("a cannot be both this venture's parent and its child"),
        "{err}"
    );
    // A child that names another parent, or none, does not agree with a.
    let err = k
        .add(&venture(tmp.path(), "b", &plain("b", "", "parent: z\n")))
        .unwrap_err();
    assert!(
        err.contains("a lists b under children, but b names z as its parent"),
        "{err}"
    );
    let err = k
        .add(&venture(tmp.path(), "b", &plain("b", "", "")))
        .unwrap_err();
    assert!(
        err.contains("a lists b under children, but b names no parent"),
        "{err}"
    );
    let err = k
        .add(&venture(tmp.path(), "c", &plain("c", "", "parent: a\n")))
        .unwrap_err();
    assert!(err.contains("a does not list c under children"), "{err}");
    // Every link agreeing, a loop of three is refused as soon as it closes,
    // through what a mandate declares even before that venture is added.
    k.add(&venture(
        tmp.path(),
        "a",
        &plain("a", "children: [b]\n", "parent: d\n"),
    ))
    .unwrap();
    let err = k
        .add(&venture(
            tmp.path(),
            "b",
            &plain("b", "children: [d]\n", "parent: a\n"),
        ))
        .unwrap_err();
    assert!(
        err.contains("that would make a loop: b -> d -> a -> b"),
        "{err}"
    );
    // A parent that drops a child cannot be re-added while the child still names it.
    k.add(&venture(tmp.path(), "b", &plain("b", "", "parent: a\n")))
        .unwrap();
    let err = k
        .add(&venture(tmp.path(), "a", &plain("a", "", "")))
        .unwrap_err();
    assert!(
        err.contains("b names a as its parent, but a does not list b"),
        "{err}"
    );
    // Self is nobody's.
    let err = k
        .add(&venture(
            tmp.path(),
            "e",
            &plain("e", "children: [self]\n", ""),
        ))
        .unwrap_err();
    assert!(err.contains("self cannot be a child"), "{err}");
    let err = k
        .add(&venture(
            tmp.path(),
            "f",
            &plain("f", "children: [f]\n", ""),
        ))
        .unwrap_err();
    assert!(err.contains("f cannot be its own child"), "{err}");
}

#[test]
fn the_parents_nightly_call_sees_childrens_records_not_their_raw_input() {
    let tmp = tempfile::tempdir().unwrap();
    let home = home(
        tmp.path(),
        "nightly: never\ndefault: k\nbackends:\n  k: {kind: command, run: echo nothing}\n",
    );
    let (role, seen) = quiet();
    let mut k = kernel(&home, role);
    k.add(&venture(tmp.path(), "shop", &child("shop", "")))
        .unwrap();
    k.add(&venture(tmp.path(), "farm", &child("farm", "")))
        .unwrap();
    k.add(&venture(tmp.path(), "portfolio", PORTFOLIO)).unwrap();
    k.report("shop", false);
    k.propose("shop", "spend", None).unwrap();
    // A finding the shop's research brought in: the world's raw input.
    k.append(Record::Report {
        claim: Claim::Inferred,
        venture: "shop".into(),
        metric: "research".into(),
        value: None,
        note: "RAW WORLD TEXT the parent must not see".into(),
        source: Some("https://x".into()),
        confidence: Some(0.5),
        cites: vec![],
    })
    .unwrap();
    k.cycle("portfolio");
    let ctx = seen.lock().unwrap().pop().expect("the parent was asked");
    assert_eq!(ctx.children.len(), 2);
    let shop = &ctx.children[0];
    assert_eq!(shop.name, "shop");
    assert_eq!(shop.state, "running");
    assert!(
        shop.metric.starts_with("shell=1  reported"),
        "{}",
        shop.metric
    );
    assert!(
        shop.lines
            .iter()
            .any(|l| l.contains("measured  action    spend: done")),
        "{:?}",
        shop.lines
    );
    assert!(
        shop.lines
            .iter()
            .any(|l| l.contains("declared  decision  adopted mandate")),
        "{:?}",
        shop.lines
    );
    assert!(
        !shop
            .lines
            .iter()
            .any(|l| l.contains("RAW WORLD TEXT") || l.contains("inferred")),
        "{:?}",
        shop.lines
    );
    assert_eq!(ctx.children[1].metric, "no report yet");
}
