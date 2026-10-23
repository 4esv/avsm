//! Recorded, then done: every lever run stores its intent before it runs
//! and its outcome after; an intent with no outcome is never run again
//! blindly; a model turn has not happened until its record is stored.

use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use avsmd::gate::{Candidate, Proof};
use avsmd::kernel::Kernel;
use avsmd::record::{self, Claim, Entry, Kind, Payload, Record, Seq, Signal};
use avsmd::role::{Context, InferRequest, ModelRole, Role};
use avsmd::warden::{LocalWarden, Warden};

struct FakeRole(VecDeque<Vec<Record>>);

impl Role for FakeRole {
    fn decide(
        &mut self,
        _ctx: &Context,
        _infer: &mut dyn FnMut(&InferRequest) -> Result<String, String>,
        _drill: &mut dyn FnMut(&str, &str) -> Result<String, String>,
    ) -> Result<Vec<Record>, String> {
        Ok(self.0.pop_front().unwrap_or_default())
    }
}

/// The real warden, with an adapter that can tell, by key, that a run
/// happened.
struct Confirming(LocalWarden);

impl Warden for Confirming {
    fn execute(&mut self, c: &Candidate) -> Result<Vec<Entry>, avsmd::warden::Failure> {
        self.0.execute(c)
    }
    fn infer(&mut self, r: &InferRequest) -> Result<String, String> {
        self.0.infer(r)
    }
    fn measure(&mut self, v: &str) -> Result<(Option<f64>, String), String> {
        self.0.measure(v)
    }
    fn confirm(&mut self, _intent: Seq) -> Result<Option<(bool, String)>, String> {
        Ok(Some((true, "found the key on the other side".into())))
    }
}

const MANDATE: &str = "purpose: Sell.\nmetric: orders\ncontroller: {kind: model}\ncadence: 1h\n\
     permitted:\n\
     - {name: tidy, run: \"echo x >> tidies; echo $AVSM_KEY > key\", cap: 9/day, reversible: true}\n\
     - {name: ship, run: \"echo x >> ships\", cap: 9/day, reversible: false, delegated: true}\n\
     - {name: post, run: \"cat > posted; echo $AVSM_KEY > key\", cap: 9/day, reversible: true, takes: text}\n\
     - {name: wipe, run: \"echo x >> wipes\", cap: 9/day, reversible: false}\n";

fn setup(tmp: &Path, backend: &str) -> (PathBuf, PathBuf) {
    setup_with(tmp, backend, "")
}

fn setup_with(tmp: &Path, backend: &str, extra: &str) -> (PathBuf, PathBuf) {
    let home = tmp.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.yaml"),
        format!(
            "nightly: never\ndefault: k\nbackends:\n  k: {{kind: command, run: '{backend}'}}\n{extra}"
        ),
    )
    .unwrap();
    let dir = tmp.join("shop");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("mandate.yaml"), MANDATE).unwrap();
    let report = dir.join("report");
    std::fs::write(&report, "#!/bin/sh\necho 5\n").unwrap();
    std::fs::set_permissions(&report, std::fs::Permissions::from_mode(0o755)).unwrap();
    (home, dir)
}

fn open(home: &Path, role: Box<dyn Role + Send>) -> Kernel {
    Kernel::with(home, role, Box::new(LocalWarden::open(home).unwrap())).unwrap()
}

fn quiet() -> Box<dyn Role + Send> {
    Box::new(FakeRole(VecDeque::new()))
}

fn runs(dir: &Path, file: &str) -> usize {
    std::fs::read_to_string(dir.join(file))
        .map(|t| t.lines().count())
        .unwrap_or(0)
}

/// The records written since `from`, as (kind, record).
fn since(k: &Kernel, from: usize) -> Vec<Entry> {
    k.entries().unwrap()[from..].to_vec()
}

/// A lever run's records: the intent, then the outcome that cites it.
fn assert_intent_then_outcome(written: &[Entry], action: &str) -> (Entry, Entry) {
    let i = written
        .iter()
        .position(|e| e.record.kind() == Kind::Intent)
        .unwrap_or_else(|| panic!("no intent: {written:?}"));
    let o = written
        .iter()
        .position(|e| e.record.kind() == Kind::Outcome)
        .unwrap_or_else(|| panic!("no outcome: {written:?}"));
    assert!(i < o, "the intent comes first: {written:?}");
    let (intent, outcome) = (written[i].clone(), written[o].clone());
    assert!(intent.seq < outcome.seq);
    assert_eq!(intent.record.action(), Some(action));
    assert!(
        matches!(&intent.record, Record::Intent { key: Some(k), .. } if k.len() == 64),
        "{intent:?}"
    );
    assert!(
        matches!(&outcome.record, Record::Outcome { intent: n, ok: true, claim: Claim::Measured, .. } if *n == intent.seq),
        "{outcome:?}"
    );
    assert!(
        !written.iter().any(|e| e.record.kind() == Kind::Action),
        "no bare action for a lever run: {written:?}"
    );
    (intent, outcome)
}

#[test]
fn a_lever_run_leaves_its_intent_then_its_outcome_reversible_or_not() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, dir) = setup(tmp.path(), "echo nothing");
    let mut k = open(&home, quiet());
    k.add(&dir).unwrap();

    let from = k.entries().unwrap().len();
    k.propose("shop", "tidy", None).unwrap();
    let written = since(&k, from);
    assert_intent_then_outcome(&written, "tidy");
    assert_eq!(runs(&dir, "tidies"), 1);

    let from = k.entries().unwrap().len();
    k.propose("shop", "ship", None).unwrap();
    let written = since(&k, from);
    let (intent, outcome) = assert_intent_then_outcome(&written, "ship");
    assert_eq!(runs(&dir, "ships"), 1);
    // `why` on the outcome shows the proof the intent rests on.
    let why = k.why(outcome.seq).unwrap();
    assert!(
        why.contains(&format!("outcome of intent #{}", intent.seq)),
        "{why}"
    );
    assert!(why.contains("shop: ship is allowed"), "{why}");
}

#[test]
fn the_key_is_the_same_for_the_same_run_and_reaches_the_lever() {
    let text = |t: &str| Payload::Text { text: t.into() };
    let a = record::key("shop", "post", &[3, 1], Some(&text("hi")));
    assert_eq!(a, record::key("shop", "post", &[1, 3], Some(&text("hi"))));
    assert_eq!(a.len(), 64);
    for other in [
        record::key("farm", "post", &[1, 3], Some(&text("hi"))),
        record::key("shop", "tidy", &[1, 3], Some(&text("hi"))),
        record::key("shop", "post", &[1], Some(&text("hi"))),
        record::key("shop", "post", &[1, 3], Some(&text("ho"))),
        record::key("shop", "post", &[1, 3], None),
    ] {
        assert_ne!(a, other);
    }

    let tmp = tempfile::tempdir().unwrap();
    let (home, dir) = setup(tmp.path(), "echo nothing");
    let mut k = open(&home, quiet());
    k.add(&dir).unwrap();
    let from = k.entries().unwrap().len();
    k.propose("shop", "post", Some("Hello")).unwrap();
    let written = since(&k, from);
    let proposal = written[0].seq;
    let (intent, _) = assert_intent_then_outcome(&written, "post");
    let want = record::key("shop", "post", &[proposal], Some(&text("Hello")));
    assert!(
        matches!(&intent.record, Record::Intent { key: Some(k), .. } if *k == want),
        "{intent:?}"
    );
    let seen = std::fs::read_to_string(dir.join("key")).unwrap();
    assert_eq!(seen.trim(), want, "the lever sees AVSM_KEY");
}

/// Write what a warden that died mid-run leaves: an intent and no outcome.
fn crash_mid_run(k: &mut Kernel, action: &str) -> Seq {
    k.append(Record::Intent {
        claim: Claim::Measured,
        venture: "shop".into(),
        action: action.into(),
        proof: Proof {
            venture: "shop".into(),
            action: action.into(),
            steps: vec![],
        },
        payload: None,
        key: Some(record::key("shop", action, &[], None)),
    })
    .unwrap()
    .seq
}

fn pending_ask(k: &Kernel, intent: Seq) -> Option<Entry> {
    let open = k.open_asks().unwrap();
    k.entries().unwrap().into_iter().find(|e| {
        open.contains(&e.seq)
            && matches!(&e.record, Record::Ask {
                    claim: Claim::Measured,
                    action: None,
                    question,
                    signal: Some(Signal::Pending { intent: n, .. }),
                    ..
                }
                if *n == intent && question.contains(&format!("intent #{intent} has no outcome")))
    })
}

#[test]
fn a_pending_intent_is_asked_about_past_the_asks_quota() {
    let tmp = tempfile::tempdir().unwrap();
    // A cap of 0 is refused by the config, so the quota is one, spent.
    let (home, dir) = setup_with(tmp.path(), "echo nothing", "quotas: {asks: 1/day}\n");
    let mut k = open(&home, quiet());
    k.add(&dir).unwrap();
    k.propose("shop", "wipe", None).unwrap();
    let spent = k.open_asks().unwrap();
    assert_eq!(spent.len(), 1);
    k.deny(spent[0], "not now").unwrap();
    // The quota is spent: an ordinary ask is not written.
    let out = k.propose("shop", "wipe", None).unwrap_or_else(|e| e);
    assert!(out.contains("asks quota"), "{out}");
    assert!(k.open_asks().unwrap().is_empty(), "{out}");
    let intent = crash_mid_run(&mut k, "ship");
    k.tick();
    let ask = pending_ask(&k, intent).expect("asked past the quota");
    assert!(
        matches!(&ask.record, Record::Ask { signal: Some(Signal::Pending { intent: n, action }), .. }
            if *n == intent && action == "ship"),
        "{}",
        ask.line()
    );
    // One open ask per intent, however many ticks.
    k.tick();
    k.tick();
    assert_eq!(k.open_asks().unwrap(), vec![ask.seq]);
}

#[test]
fn an_approval_blocked_by_a_pending_intent_runs_once_it_is_settled() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, dir) = setup(tmp.path(), "echo nothing");
    let mut k = open(&home, quiet());
    k.add(&dir).unwrap();
    k.propose("shop", "wipe", None).unwrap();
    let wipe_ask = k.open_asks().unwrap()[0];
    // A run of wipe that died mid-way, then the human approves the ask.
    let intent = crash_mid_run(&mut k, "wipe");
    k.approve(wipe_ask, None).unwrap();
    k.tick();
    k.tick();
    assert_eq!(runs(&dir, "wipes"), 0, "blocked while pending");
    // Settled in the same process: the approval runs on the next tick.
    let pending = pending_ask(&k, intent).expect("asked about the intent");
    k.deny(pending.seq, "it never ran").unwrap();
    k.tick();
    assert_eq!(runs(&dir, "wipes"), 1, "the approval was not spent");
    k.tick();
    assert_eq!(runs(&dir, "wipes"), 1, "and runs once");
}

#[test]
fn after_a_crash_a_pending_intent_is_asked_about_never_rerun() {
    for approve in [true, false] {
        let tmp = tempfile::tempdir().unwrap();
        let (home, dir) = setup(tmp.path(), "echo nothing");
        let mut k = open(&home, quiet());
        k.add(&dir).unwrap();
        let intent = crash_mid_run(&mut k, "ship");
        drop(k);

        // Reopen: the intent is not run again; a code ask is written once.
        let mut k = open(&home, quiet());
        assert_eq!(runs(&dir, "ships"), 0, "never rerun blindly");
        let ask = pending_ask(&k, intent).expect("an ask about the intent");
        assert!(
            matches!(&ask.record, Record::Ask { question, .. } if question.starts_with("did ship happen?")),
            "{}",
            ask.line()
        );
        k.tick();
        k.tick();
        drop(k);
        let mut k = open(&home, quiet());
        let asks = k
            .entries()
            .unwrap()
            .iter()
            .filter(|e| e.record.kind() == Kind::Ask)
            .count();
        assert_eq!(asks, 1, "asked once");
        assert_eq!(runs(&dir, "ships"), 0);

        // While pending, the gate refuses the lever; others still run.
        let out = k.propose("shop", "ship", None).unwrap_or_else(|e| e);
        assert!(
            out.contains(&format!("intent #{intent} has no outcome")),
            "{out}"
        );
        assert_eq!(runs(&dir, "ships"), 0);
        k.propose("shop", "tidy", None).unwrap();
        assert_eq!(runs(&dir, "tidies"), 1);

        // The human's answer is the declared outcome, and unblocks.
        let out = if approve {
            k.approve(ask.seq, None).unwrap()
        } else {
            k.deny(ask.seq, "it never left").unwrap()
        };
        let last = k.entries().unwrap().last().unwrap().clone();
        assert!(
            matches!(&last.record, Record::Outcome { claim: Claim::Declared, intent: n, ok, .. }
                if *n == intent && *ok == approve),
            "{out}"
        );
        assert!(out.contains(&last.line()), "{out}");
        assert!(pending_ask(&k, intent).is_none());
        k.tick();
        assert_eq!(runs(&dir, "ships"), 0, "the answer runs nothing");
        let out = k.propose("shop", "ship", None).unwrap();
        assert!(!out.contains("has no outcome"), "{out}");
        assert_eq!(runs(&dir, "ships"), 1, "unblocked");
    }
}

#[test]
fn an_adapter_that_can_check_settles_a_pending_intent_by_itself() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, dir) = setup(tmp.path(), "echo nothing");
    let mut k = open(&home, quiet());
    k.add(&dir).unwrap();
    let intent = crash_mid_run(&mut k, "ship");
    drop(k);
    let k = Kernel::with(
        &home,
        quiet(),
        Box::new(Confirming(LocalWarden::open(&home).unwrap())),
    )
    .unwrap();
    let last = k.entries().unwrap().last().unwrap().clone();
    assert!(
        matches!(&last.record, Record::Outcome { claim: Claim::Measured, intent: n, ok: true, detail, .. }
            if *n == intent && detail.contains("found the key")),
        "{}",
        last.line()
    );
    assert!(k.open_asks().unwrap().is_empty());
    assert_eq!(runs(&dir, "ships"), 0);
}

#[test]
fn a_model_turn_that_chooses_nothing_leaves_an_inferred_record() {
    // The model's own reply, through the warden and the model role.
    let tmp = tempfile::tempdir().unwrap();
    let (home, dir) = setup(
        tmp.path(),
        r#"echo "{\"choice\": \"nothing\", \"reason\": \"orders are steady\"}""#,
    );
    let mut k = open(&home, Box::new(ModelRole));
    k.add(&dir).unwrap();
    k.report("shop", false);
    k.cycle("shop");
    let last = k.entries().unwrap().last().unwrap().clone();
    assert!(
        matches!(&last.record, Record::Report { claim: Claim::Inferred, venture, metric, note, value: None, .. }
            if venture == "shop" && metric == "nothing" && note == "orders are steady"),
        "{}",
        last.line()
    );

    // A role that returns no record at all still leaves one.
    let tmp = tempfile::tempdir().unwrap();
    let (home, dir) = setup(tmp.path(), "echo nothing");
    let mut k = open(&home, quiet());
    k.add(&dir).unwrap();
    let before = k.entries().unwrap().len();
    k.cycle("shop");
    let written = since(&k, before);
    assert_eq!(written.len(), 1, "{written:?}");
    assert!(
        matches!(&written[0].record, Record::Report { claim: Claim::Inferred, metric, .. } if metric == "nothing"),
        "{}",
        written[0].line()
    );
}

/// The real warden, except that a run of `fails` cannot start: the
/// error a warden gives before it writes an intent. It counts the tries.
struct CannotStart {
    inner: LocalWarden,
    fails: &'static str,
    tries: Arc<AtomicUsize>,
}

impl Warden for CannotStart {
    fn execute(&mut self, c: &Candidate) -> Result<Vec<Entry>, avsmd::warden::Failure> {
        if c.action == self.fails {
            self.tries.fetch_add(1, Ordering::SeqCst);
            return Err("cannot start".to_string().into());
        }
        self.inner.execute(c)
    }
    fn infer(&mut self, r: &InferRequest) -> Result<String, String> {
        self.inner.infer(r)
    }
    fn measure(&mut self, v: &str) -> Result<(Option<f64>, String), String> {
        self.inner.measure(v)
    }
    fn confirm(&mut self, i: Seq) -> Result<Option<(bool, String)>, String> {
        self.inner.confirm(i)
    }
}

#[test]
fn an_approval_that_did_not_run_is_not_tried_again_after_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, dir) = setup(tmp.path(), "echo nothing");
    let tries = Arc::new(AtomicUsize::new(0));
    let start = |home: &Path| {
        let warden = CannotStart {
            inner: LocalWarden::open(home).unwrap(),
            fails: "wipe",
            tries: tries.clone(),
        };
        Kernel::with(home, quiet(), Box::new(warden)).unwrap()
    };
    let mut k = start(&home);
    k.add(&dir).unwrap();
    k.propose("shop", "wipe", None).unwrap();
    let ask = k.open_asks().unwrap()[0];
    k.approve(ask, None).unwrap();
    let decision = k.entries().unwrap().last().unwrap().seq;
    k.tick();
    let failed = |k: &Kernel| {
        k.entries()
            .unwrap()
            .into_iter()
            .filter(|e| {
                matches!(&e.record, Record::Report { metric, value: None, cites, .. }
                    if metric == "wipe" && cites.contains(&decision))
            })
            .count()
    };
    assert_eq!(
        failed(&k),
        1,
        "the gate said yes and nothing ran: one record"
    );
    assert_eq!(tries.load(Ordering::SeqCst), 1);
    drop(k);
    // Two restarts: the approval has its outcome and is not tried again.
    for _ in 0..2 {
        let mut k = start(&home);
        k.tick();
        k.tick();
        assert_eq!(failed(&k), 1);
    }
    assert_eq!(tries.load(Ordering::SeqCst), 1);
    assert_eq!(runs(&dir, "wipes"), 0);
}
