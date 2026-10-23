//! The ratchet and the digest: five approvals of the same lever and
//! payload shape yield one proposal from self; adopting it rewrites the
//! mandate and ends the asks. The digest says what the night did.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use avsmd::kernel::Kernel;
use avsmd::record::{Claim, Kind, Payload, Record};
use avsmd::role::{Context, InferRequest, Role};
use avsmd::warden::LocalWarden;

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

fn venture(parent: &Path, name: &str, mandate: &str) -> PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("mandate.yaml"), mandate).unwrap();
    dir
}

fn announce(n: usize) -> Vec<Record> {
    vec![Record::Proposal {
        cites: vec![],
        claim: Claim::Inferred,
        venture: "shop".into(),
        action: "announce".into(),
        reason: format!("news {n}"),
        payload: Some(Payload::Text {
            text: format!("Hello {n}"),
        }),
    }]
}

fn open_ask(k: &Kernel, lever: &str) -> Option<u64> {
    k.asks()
        .unwrap()
        .lines()
        .find(|l| l.contains(&format!(" {lever}? ")))
        .map(|l| {
            l.trim_start_matches('#')
                .split_whitespace()
                .next()
                .unwrap()
                .parse()
                .unwrap()
        })
}

const SHOP: &str = "# The shop.\npurpose: Sell more.\nmetric: shell echo 5\ncontroller: {kind: model}\ncadence: 1h\n\
permitted:\n  - name: announce\n    run: \"cat >> announced\"\n    cap: 9/day\n    reversible: true\n    takes: text\n";

#[test]
fn five_approvals_yield_one_proposal_and_adopting_it_ends_the_asks() {
    let tmp = tempfile::tempdir().unwrap();
    let home = home(tmp.path(), "nightly: never\ndefault: k\nbackends:\n  k: {kind: command, run: echo nothing}\nratchet: 5\n");
    let role = Scripted {
        replies: (1..=7).map(announce).collect(),
        seen: Arc::new(Mutex::new(vec![])),
    };
    let mut k = Kernel::with(
        &home,
        Box::new(role),
        Box::new(LocalWarden::open(&home).unwrap()),
    )
    .unwrap();
    k.adopt_self().unwrap();
    let dir = venture(tmp.path(), "shop", SHOP);
    k.add(&dir).unwrap();

    // Five nights, five asks, five approvals.
    for n in 1..=5 {
        k.cycle("shop");
        let ask = open_ask(&k, "announce").expect("announce asks each time");
        assert!(
            open_ask(&k, "delegate").is_none(),
            "no proposal before five: night {n}"
        );
        k.approve(ask, None).unwrap();
        k.tick();
    }
    assert_eq!(
        std::fs::read_to_string(dir.join("announced")).unwrap(),
        "Hello 1Hello 2Hello 3Hello 4Hello 5"
    );

    // Self proposed delegating announce, with the five approvals as evidence.
    let entries = k.entries().unwrap();
    let proposal = entries
        .iter()
        .find(|e| e.record.kind() == Kind::Proposal && e.record.action() == Some("delegate"))
        .expect("self proposed");
    assert_eq!(proposal.record.claim(), Claim::Measured);
    assert!(
        proposal
            .record
            .summary()
            .starts_with("delegate: announce was approved 5 times with a text payload (#"),
        "{}",
        proposal.record.summary()
    );
    assert_eq!(
        entries
            .iter()
            .filter(|e| e.record.action() == Some("delegate"))
            .count(),
        2,
        "one proposal, one ask"
    );
    let ask = open_ask(&k, "delegate").expect("the proposal became an ask");
    let asks = k.asks().unwrap();
    assert!(
        asks.contains("    # The shop.\n"),
        "the ask shows the whole new mandate: {asks}"
    );
    assert!(asks.contains("    delegated: true\n"), "{asks}");
    let why = k.why(ask).unwrap();
    assert!(
        why.contains("a changed mandate is adopted only by a declared decision"),
        "{why}"
    );
    assert!(
        why.contains("because    delegate is irreversible and no declared decision approves it")
            || why.contains("no declared decision approves it"),
        "{why}"
    );

    // A tick in between proposes nothing twice.
    k.tick();
    assert_eq!(
        k.entries()
            .unwrap()
            .iter()
            .filter(|e| e.record.action() == Some("delegate"))
            .count(),
        2
    );

    // Approving rewrites the mandate file and re-adds the venture.
    k.approve(ask, None).unwrap();
    k.tick();
    let entries = k.entries().unwrap();
    let done = entries
        .iter()
        .find(|e| e.record.kind() == Kind::Intent && e.record.action() == Some("delegate"))
        .expect("delegate ran");
    let outcome = entries
        .iter()
        .find(|e| matches!(&e.record, Record::Outcome { intent, .. } if *intent == done.seq))
        .expect("delegate has an outcome");
    assert_eq!(
        outcome.record.summary(),
        format!("#{} done  mandate rewritten; delegated: announce", done.seq)
    );
    let adopted = entries.last().unwrap();
    assert!(
        adopted.record.summary().starts_with("adopted mandate"),
        "{}",
        adopted.line()
    );
    assert_eq!(adopted.record.claim(), Claim::Declared);
    let text = std::fs::read_to_string(dir.join("mandate.yaml")).unwrap();
    assert!(
        matches!(&adopted.record, Record::Decision { verdict: avsmd::record::Verdict::Adopt { text: Some(t), .. }, .. } if *t == text),
        "a delegate adoption stores the mandate it adopted: {}",
        adopted.line()
    );
    assert!(text.starts_with("# The shop.\n"), "comments kept: {text}");
    assert!(
        text.contains("    takes: text\n    delegated: true\n"),
        "{text}"
    );
    assert!(
        k.status().unwrap().contains("shop         running"),
        "{}",
        k.status().unwrap()
    );
    let why = k.why(done.seq).unwrap();
    assert!(
        why.contains("decision   #") && why.contains("approved ask #"),
        "{why}"
    );
    assert!(
        why.contains("payload    #") && why.contains("measured  text"),
        "{why}"
    );

    // From now on announce runs without asking: the asks have ended.
    k.cycle("shop");
    assert_eq!(k.asks().unwrap(), "no open asks\n");
    assert_eq!(
        std::fs::read_to_string(dir.join("announced")).unwrap(),
        "Hello 1Hello 2Hello 3Hello 4Hello 5Hello 6"
    );
    // The first announce rested on the first mandate; the delegated one on
    // the second, the one the delegate action adopted.
    let announces: Vec<_> = k
        .entries()
        .unwrap()
        .into_iter()
        .filter(|e| e.record.kind() == Kind::Intent && e.record.action() == Some("announce"))
        .collect();
    let first = k.why(announces.first().unwrap().seq).unwrap();
    assert!(first.contains("shop mandate v1 (#"), "{first}");
    let last = k.why(announces.last().unwrap().seq).unwrap();
    assert!(
        last.contains(&format!("shop mandate v2 (#{}, ", adopted.seq)),
        "{last}"
    );
    k.tick();
    assert_eq!(
        k.entries()
            .unwrap()
            .iter()
            .filter(|e| e.record.action() == Some("delegate"))
            .count(),
        3,
        "no new proposal"
    );
    assert_eq!(
        k.ledger.verify().unwrap(),
        k.entries().unwrap().len() as u64
    );
}

#[test]
fn a_denied_delegation_waits_for_fresh_approvals() {
    let tmp = tempfile::tempdir().unwrap();
    let home = home(tmp.path(), "nightly: never\ndefault: k\nbackends:\n  k: {kind: command, run: echo nothing}\nratchet: 2\n");
    let role = Scripted {
        replies: (1..=5).map(announce).collect(),
        seen: Arc::new(Mutex::new(vec![])),
    };
    let mut k = Kernel::with(
        &home,
        Box::new(role),
        Box::new(LocalWarden::open(&home).unwrap()),
    )
    .unwrap();
    k.adopt_self().unwrap();
    k.add(&venture(tmp.path(), "shop", SHOP)).unwrap();
    for _ in 0..2 {
        k.cycle("shop");
        k.approve(open_ask(&k, "announce").unwrap(), None).unwrap();
        k.tick();
    }
    let ask = open_ask(&k, "delegate").expect("two is enough here");
    k.deny(ask, "not yet").unwrap();
    k.tick();
    k.cycle("shop");
    k.approve(open_ask(&k, "announce").unwrap(), None).unwrap();
    k.tick();
    assert!(
        open_ask(&k, "delegate").is_none(),
        "one approval since the denial is not two"
    );
    k.cycle("shop");
    k.approve(open_ask(&k, "announce").unwrap(), None).unwrap();
    k.tick();
    assert!(
        open_ask(&k, "delegate").is_some(),
        "two more approvals: asked again"
    );
}

#[test]
fn the_digest_says_what_the_night_did_and_is_written_as_a_record() {
    let tmp = tempfile::tempdir().unwrap();
    let home = home(tmp.path(), "nightly: never\n");
    let role = Scripted {
        replies: VecDeque::new(),
        seen: Arc::new(Mutex::new(vec![])),
    };
    let mut k = Kernel::with(
        &home,
        Box::new(role),
        Box::new(LocalWarden::open(&home).unwrap()),
    )
    .unwrap();
    k.adopt_self().unwrap();
    let shop = venture(
        tmp.path(),
        "shop",
        "purpose: Sell.\nmetric: shell echo 12\ncontroller: {kind: model}\ncadence: 1h\n\
         permitted:\n- {name: buy, run: \"true\", cap: 9/day, reversible: true, delegated: true, cost: 10}\n\
         - {name: wipe, run: \"true\", cap: 9/day, reversible: false}\n",
    );
    k.add(&shop).unwrap();
    k.add(&venture(
        tmp.path(),
        "farm",
        "purpose: Grow.\nmetric: shell echo 3\ncontroller: {kind: model}\ncadence: 1h\n",
    ))
    .unwrap();
    // A reading from two days ago, for the delta.
    let day = 86_400;
    let now = avsmd::kernel::now();
    k.ledger
        .append_at(
            Record::report(Claim::Measured, "shop", "shell", Some(8.0), ""),
            now - 2 * day,
        )
        .unwrap();
    k.report("shop", false);
    k.report("self", false);
    k.propose("shop", "buy", None).unwrap();
    k.propose("shop", "buy", None).unwrap();
    k.propose("shop", "wipe", None).unwrap();
    let text = k.digest().unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("digest since "), "{text}");
    assert!(
        lines[0].ends_with(": 3 ventures, 2 actions, 1 asks, cost 20"),
        "{text}"
    );
    assert_eq!(
        lines[1], "farm         shell: no report     0 actions  0 asks  cost 0",
        "{text}"
    );
    assert_eq!(
        lines[2], "shop         shell=12 (+4)        2 actions  1 asks  cost 20",
        "{text}"
    );
    assert!(
        lines[3].starts_with("self         integrity=1          asks 1/week  incidents 0/week"),
        "{text}"
    );
    assert_eq!(lines[4], "open asks: 1  (avsm asks)", "{text}");
    assert!(lines[5].starts_with("head "), "the anchor: {text}");
    assert_eq!(lines.len(), 6, "under a screen: {text}");

    let entry = k.record_digest().unwrap();
    assert_eq!(entry.record.venture(), "self");
    assert!(
        entry
            .record
            .summary()
            .starts_with("digest=?  digest since "),
        "{}",
        entry.line()
    );
    let known = k.know("self").unwrap();
    assert!(known.contains("measured  integrity=1"), "{known}");
    assert!(known.contains("measured  asks_per_week=1"), "{known}");
    assert!(
        known.contains("measured  incidents_per_week=0  failed actions, broken chains and stops"),
        "{known}"
    );
    let known = k.know("digest").unwrap();
    assert!(known.starts_with("digest: 1 current fact\n"), "{known}");
    let why = k.why(entry.seq).unwrap();
    assert!(why.contains("measured by code"), "{why}");
    // The next digest covers what came after this one.
    k.propose("shop", "buy", None).unwrap();
    let text = k.digest().unwrap();
    assert!(
        text.lines()
            .next()
            .unwrap()
            .ends_with(": 3 ventures, 1 actions, 0 asks, cost 10"),
        "{text}"
    );
    assert_eq!(k.handle(&["digest".to_string()]).unwrap(), text);
}

#[test]
fn an_invalid_edit_of_a_delegation_is_refused_before_it_is_approved() {
    let tmp = tempfile::tempdir().unwrap();
    let home = home(tmp.path(), "nightly: never\ndefault: k\nbackends:\n  k: {kind: command, run: echo nothing}\nratchet: 5\n");
    let role = Scripted {
        replies: (1..=5).map(announce).collect(),
        seen: Arc::new(Mutex::new(vec![])),
    };
    let mut k = Kernel::with(
        &home,
        Box::new(role),
        Box::new(LocalWarden::open(&home).unwrap()),
    )
    .unwrap();
    k.adopt_self().unwrap();
    let dir = venture(tmp.path(), "shop", SHOP);
    k.add(&dir).unwrap();
    for _ in 1..=5 {
        k.cycle("shop");
        let ask = open_ask(&k, "announce").unwrap();
        k.approve(ask, None).unwrap();
        k.tick();
    }
    // The human edits the new mandate into something that is not YAML:
    // approve refuses it, nothing is decided, the mandate is untouched.
    let ask = open_ask(&k, "delegate").expect("self proposed");
    let before = k.entries().unwrap().len();
    let err = k
        .approve(ask, Some("purpose: Sell more.\nthis line is not yaml\n"))
        .expect_err("refused at approve");
    assert!(!err.is_empty());
    k.tick();
    assert_eq!(k.entries().unwrap().len(), before, "nothing written");
    assert!(open_ask(&k, "delegate").is_some());
    assert_eq!(
        std::fs::read_to_string(dir.join("mandate.yaml")).unwrap(),
        SHOP
    );
}

fn kernel_with(tmp: &Path, ratchet: u32, replies: Vec<Vec<Record>>) -> Kernel {
    let home = home(
        tmp,
        &format!("nightly: never\ndefault: k\nbackends:\n  k: {{kind: command, run: echo nothing}}\nratchet: {ratchet}\n"),
    );
    let role = Scripted {
        replies: replies.into(),
        seen: Arc::new(Mutex::new(vec![])),
    };
    let mut k = Kernel::with(
        &home,
        Box::new(role),
        Box::new(LocalWarden::open(&home).unwrap()),
    )
    .unwrap();
    k.adopt_self().unwrap();
    k
}

fn wipe(n: usize) -> Vec<Record> {
    vec![Record::Proposal {
        cites: vec![],
        claim: Claim::Inferred,
        venture: "shop".into(),
        action: "wipe".into(),
        reason: format!("tidy {n}"),
        payload: None,
    }]
}

const IRREVERSIBLE: &str =
    "purpose: Sell more.\nmetric: shell echo 5\ncontroller: {kind: model}\ncadence: 1h\n\
permitted:\n  - name: wipe\n    run: \"echo x >> wiped\"\n    cap: 99/day\n    reversible: false\n";

fn delegate_asks(k: &Kernel) -> usize {
    k.entries()
        .unwrap()
        .iter()
        .filter(|e| e.record.action() == Some("delegate"))
        .count()
}

#[test]
fn ten_approvals_of_an_irreversible_lever_give_no_delegate_proposal() {
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kernel_with(tmp.path(), 5, (1..=10).map(wipe).collect());
    let dir = venture(tmp.path(), "shop", IRREVERSIBLE);
    k.add(&dir).unwrap();
    for _ in 1..=10 {
        k.cycle("shop");
        let ask = open_ask(&k, "wipe").expect("wipe asks each time");
        k.approve(ask, None).unwrap();
        k.tick();
    }
    assert_eq!(
        std::fs::read_to_string(dir.join("wiped")).unwrap().len(),
        20,
        "ten approvals, ten runs"
    );
    assert_eq!(
        delegate_asks(&k),
        0,
        "an irreversible lever is never proposed"
    );
    assert!(open_ask(&k, "delegate").is_none());
}

#[test]
fn five_approvals_of_a_reversible_payload_lever_still_give_a_proposal() {
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kernel_with(tmp.path(), 5, (1..=5).map(announce).collect());
    k.add(&venture(tmp.path(), "shop", SHOP)).unwrap();
    for _ in 1..=5 {
        k.cycle("shop");
        let ask = open_ask(&k, "announce").unwrap();
        k.approve(ask, None).unwrap();
        k.tick();
    }
    assert_eq!(delegate_asks(&k), 2, "one proposal, one ask");
}

/// A delegate ask for the shop, open, with its mandate text as payload.
fn delegate_ask(tmp: &Path) -> (Kernel, u64, PathBuf) {
    let mut k = kernel_with(tmp, 2, (1..=2).map(announce).collect());
    let dir = venture(tmp, "shop", SHOP);
    k.add(&dir).unwrap();
    for _ in 1..=2 {
        k.cycle("shop");
        let ask = open_ask(&k, "announce").unwrap();
        k.approve(ask, None).unwrap();
        k.tick();
    }
    let ask = open_ask(&k, "delegate").expect("delegate ask");
    (k, ask, dir)
}

fn decisions(k: &Kernel) -> usize {
    k.entries()
        .unwrap()
        .iter()
        .filter(|e| e.record.kind() == Kind::Decision)
        .count()
}

#[test]
fn an_invalid_edited_mandate_is_refused_at_approve_and_nothing_is_written() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut k, ask, dir) = delegate_ask(tmp.path());
    let before = decisions(&k);
    let len = k.entries().unwrap().len();
    let original = std::fs::read_to_string(dir.join("mandate.yaml")).unwrap();

    let err = k
        .approve(ask, Some("purpose: [unclosed\n  metric: : :"))
        .expect_err("bad YAML is refused");
    assert!(!err.is_empty());

    let unknown = format!("{original}bogus_field: 1\n");
    let err = k
        .approve(ask, Some(&unknown))
        .expect_err("an unknown field is refused");
    assert!(err.contains("bogus_field"), "{err}");

    assert_eq!(decisions(&k), before, "no decision record written");
    assert_eq!(k.entries().unwrap().len(), len, "nothing written at all");
    assert!(open_ask(&k, "delegate").is_some(), "the ask stays open");
    k.tick();
    assert_eq!(
        std::fs::read_to_string(dir.join("mandate.yaml")).unwrap(),
        original
    );
}

#[test]
fn a_valid_edited_mandate_is_approved_and_adopted() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut k, ask, dir) = delegate_ask(tmp.path());
    let edited = SHOP.replace("cap: 9/day", "cap: 3/day").replace(
        "reversible: true\n",
        "reversible: true\n    delegated: true\n",
    );
    k.approve(ask, Some(&edited)).unwrap();
    k.tick();
    let text = std::fs::read_to_string(dir.join("mandate.yaml")).unwrap();
    assert!(text.contains("cap: 3/day"), "{text}");
    let entries = k.entries().unwrap();
    assert!(
        entries
            .last()
            .unwrap()
            .record
            .summary()
            .starts_with("adopted mandate"),
        "{}",
        entries.last().unwrap().line()
    );
}
