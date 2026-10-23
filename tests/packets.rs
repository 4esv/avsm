//! Asks are decision packets: premises, then options, then the model's
//! reason labeled inferred, then "Unanswered means no." And canary asks:
//! asks that can never run, answered by a human who is measured by them.

use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use avsmd::config::{Config, Quotas};
use avsmd::gate::{self, Candidate, Reason};
use avsmd::kernel::Kernel;
use avsmd::record::{Claim, Entry, Kind, Packet, Payload, Record, Seq, Signal, Verdict};
use avsmd::role::{Context, InferRequest, Role};
use avsmd::warden::LocalWarden;

/// The virtual clock is shared by the whole process: one test at a time.
static CLOCK: Mutex<()> = Mutex::new(());

const T0: i64 = 1_800_057_600;

fn clock() -> MutexGuard<'static, ()> {
    let guard = CLOCK.lock().unwrap_or_else(|e| e.into_inner());
    avsmd::clock::set(T0);
    guard
}

/// A role that proposes announcing, with a payload and a reason, each time.
struct Announcer {
    n: usize,
}

impl Role for Announcer {
    fn decide(
        &mut self,
        ctx: &Context,
        _infer: &mut dyn FnMut(&InferRequest) -> Result<String, String>,
        _drill: &mut dyn FnMut(&str, &str) -> Result<String, String>,
    ) -> Result<Vec<Record>, String> {
        self.n += 1;
        Ok(vec![Record::Proposal {
            cites: vec![],
            claim: Claim::Inferred,
            venture: ctx.venture.clone(),
            action: "announce".into(),
            reason: format!("the model says news {}", self.n),
            payload: Some(Payload::Text {
                text: format!("Hello {}", self.n),
            }),
        }])
    }
}

/// A role that says nothing.
struct Silent;

impl Role for Silent {
    fn decide(
        &mut self,
        _ctx: &Context,
        _infer: &mut dyn FnMut(&InferRequest) -> Result<String, String>,
        _drill: &mut dyn FnMut(&str, &str) -> Result<String, String>,
    ) -> Result<Vec<Record>, String> {
        Ok(vec![])
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
    script(&dir, "report", "echo 5");
    dir
}

fn kernel(tmp: &Path, config: &str, role: Box<dyn Role + Send>) -> Kernel {
    let home = tmp.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.yaml"),
        format!("nightly: never\ndefault: k\nbackends:\n  k: {{kind: command, run: echo nothing}}\n{config}"),
    )
    .unwrap();
    let warden = LocalWarden::open(&home).unwrap();
    let mut k = Kernel::with(&home, role, Box::new(warden)).unwrap();
    k.adopt_self().unwrap();
    k
}

const SHOP: &str = "purpose: Sell more.\nmetric: orders\ncontroller: {kind: model}\ncadence: 1h\n\
permitted:\n- {name: announce, run: \"cat >> announced\", cap: 9/day, reversible: true, takes: text}\n";

fn asks(k: &Kernel) -> Vec<Entry> {
    k.entries()
        .unwrap()
        .into_iter()
        .filter(|e| e.record.kind() == Kind::Ask)
        .collect()
}

fn canary_asks(k: &Kernel) -> Vec<Entry> {
    asks(k)
        .into_iter()
        .filter(|e| e.record.action() == Some("canary"))
        .collect()
}

fn packet(e: &Entry) -> Packet {
    match &e.record {
        Record::Ask {
            packet: Some(p), ..
        } => p.clone(),
        other => panic!("no packet on {other:?}"),
    }
}

/// A shop with a reading, and the model's announce ask open.
fn shop_with_ask(tmp: &Path, config: &str) -> (Kernel, PathBuf, Entry) {
    let mut k = kernel(tmp, config, Box::new(Announcer { n: 0 }));
    let dir = venture(tmp, "shop", SHOP);
    k.add(&dir).unwrap();
    k.report("shop", false);
    k.cycle("shop");
    let ask = asks(&k)
        .into_iter()
        .find(|e| e.record.action() == Some("announce"))
        .expect("announce asks");
    (k, dir, ask)
}

// ----- rendering -----

#[test]
fn premises_come_before_options_and_options_before_the_reason() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let (k, _dir, ask) = shop_with_ask(tmp.path(), "");
    let shown = k.asks().unwrap();
    let at = |needle: &str| {
        shown
            .find(needle)
            .unwrap_or_else(|| panic!("no {needle:?} in:\n{shown}"))
    };
    let (premises, options, reason) = (
        at("premises:"),
        at("options:"),
        at("reason (inferred): the model says news 1"),
    );
    assert!(premises < options && options < reason, "{shown}");
    // The reading is a premise; the model's proposal is not.
    let p = packet(&ask);
    assert!(
        p.premises
            .iter()
            .all(|p| p.claim != Claim::Inferred && p.summary.starts_with("orders=5")),
        "{p:?}"
    );
    assert!(!shown[premises..options].contains("the model says"));
    // A lever that takes a payload offers an edit.
    let names: Vec<&str> = p.options.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(names, ["approve", "approve with edit", "deny"]);
    // The last line, always.
    assert_eq!(
        shown.trim_end().lines().last(),
        Some("Unanswered means no.")
    );
    // The same packet when drilling into the ask.
    let why = k.why(ask.seq).unwrap();
    assert!(why.trim_end().ends_with("Unanswered means no."), "{why}");
}

#[test]
fn a_signal_ask_gets_its_premises_from_the_readings_that_triggered_it() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kernel(tmp.path(), "", Box::new(Silent));
    let dir = venture(
        tmp.path(),
        "shop",
        &format!("{SHOP}guards:\n- {{name: errors, run: cat errors, above: 3, then: ask}}\n"),
    );
    std::fs::write(dir.join("errors"), "5").unwrap();
    k.add(&dir).unwrap();
    for _ in 0..3 {
        k.report("shop", false);
    }
    let ask = asks(&k)
        .into_iter()
        .find(|e| {
            matches!(
                &e.record,
                Record::Ask {
                    signal: Some(Signal::Guard { .. }),
                    ..
                }
            )
        })
        .expect("a guard ask");
    let p = packet(&ask);
    assert!(
        p.premises
            .iter()
            .any(|p| p.claim == Claim::Measured && p.summary.starts_with("guard.errors=5")),
        "{p:?}"
    );
    assert!(p.reason.is_none(), "code's ask has no model reason");
    let shown = k.asks().unwrap();
    assert!(shown.find("premises:").unwrap() < shown.find("options:").unwrap());
    assert!(!shown.contains("reason (inferred)"), "{shown}");
    assert!(shown.trim_end().ends_with("Unanswered means no."));
}

// ----- options -----

const BLOG: &str = "purpose: Grow.\nmetric: posts\ncontroller: {kind: model}\ncadence: 1h\n\
     permitted:\n- {name: announce, run: echo ran >> invoked, cap: 9/day, reversible: true, takes: text}\n";

fn blog_ask(forecasts: bool) -> Packet {
    blog_ask_in(forecasts, "forecast: {days: 1, seeds: 2}\n")
}

fn blog_ask_in(forecasts: bool, config: &str) -> Packet {
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kernel(tmp.path(), config, Box::new(Announcer { n: 0 }));
    k.forecasts = forecasts;
    let dir = tmp.path().join("blog");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("mandate.yaml"), BLOG).unwrap();
    script(&dir, "report", "cat value");
    std::fs::write(dir.join("value"), "100\n").unwrap();
    k.add(&dir).unwrap();
    for v in [1, 2, 3] {
        avsmd::clock::advance(3_600);
        std::fs::write(dir.join("value"), format!("{v}\n")).unwrap();
        k.report("blog", false);
    }
    k.cycle("blog");
    let ask = asks(&k)
        .into_iter()
        .find(|e| e.record.action() == Some("announce"))
        .expect("announce asks");
    packet(&ask)
}

#[test]
fn with_forecasts_on_each_option_carries_a_forecast_and_with_them_off_it_says_none() {
    let _c = clock();
    let on = blog_ask(true);
    assert_eq!(on.options.len(), 3);
    for o in &on.options {
        let f = o
            .forecast
            .as_ref()
            .unwrap_or_else(|| panic!("{} has no forecast: {on:?}", o.name));
        assert_eq!(
            (f.days, f.seeds),
            (1, 2),
            "forecast: in config.yaml sets it"
        );
        assert_eq!(f.metric, "posts");
        assert!(f.p5 <= f.p50 && f.p50 <= f.p95);
        assert!((0.0..=1.0).contains(&f.breach));
        assert!(
            o.cost.as_deref().is_some_and(|c| c.contains("cost")),
            "{o:?}"
        );
    }
    assert!(!on.render().contains("no forecast"));

    avsmd::clock::set(T0);
    let off = blog_ask(false);
    assert!(off.options.iter().all(|o| o.forecast.is_none()));
    let shown = off.render();
    assert_eq!(
        shown.matches("no forecast").count(),
        off.options.len(),
        "{shown}"
    );
}

#[test]
fn a_packet_forecast_is_a_week_of_eight_seeds_unless_config_says_otherwise() {
    let _c = clock();
    let p = blog_ask_in(true, "");
    for o in &p.options {
        let f = o.forecast.as_ref().expect("a forecast");
        assert_eq!((f.days, f.seeds), (7, 8));
    }
}

// ----- canary asks -----

#[test]
fn canaries_are_off_by_default() {
    assert_eq!(Config::default().canary_asks, None);
    assert_eq!(Config::parse("nightly: never\n").unwrap().canary_asks, None);
    assert!(Config::parse("canary_asks: {every: 0}\n").is_err());
    assert_eq!(
        Config::parse("canary_asks: {every: 3}\n")
            .unwrap()
            .canary_asks
            .map(|c| c.every),
        Some(3)
    );

    // Many asks, many ticks, and none of them is a canary.
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let (mut k, _dir, _ask) = shop_with_ask(tmp.path(), "");
    for _ in 0..6 {
        let open = open_ask(&k, "announce").expect("a real ask is open");
        k.deny(open, "no").unwrap();
        k.tick();
        k.cycle("shop");
    }
    assert!(asks(&k).len() >= 5);
    assert!(canary_asks(&k).is_empty());
}

#[test]
fn a_canary_ask_can_never_run_even_when_approved() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let (mut k, dir, ask) = shop_with_ask(tmp.path(), "canary_asks: {every: 1}\n");
    k.deny(ask.seq, "later").unwrap();
    k.tick();
    let canary = canary_asks(&k);
    assert_eq!(canary.len(), 1, "one canary after one ask");
    let c = &canary[0];
    // It reads like any other ask: its lever is not shown on the line.
    assert!(!c.line().contains("canary"), "{}", c.line());
    assert!(matches!(
        &c.record,
        Record::Ask {
            claim: Claim::Measured,
            payload: None,
            ..
        }
    ));
    k.approve(c.seq, None).unwrap();
    k.tick();
    k.tick();

    assert!(!dir.join("canary-ran").exists(), "a canary ran");
    let entries = k.entries().unwrap();
    assert!(
        !entries
            .iter()
            .any(|e| matches!(e.record.kind(), Kind::Action | Kind::Intent)
                && e.record.action() == Some("canary")),
        "an action or intent for the canary"
    );
    // The gate says no on its own rule, though a declared decision
    // approved it. (A mandate can no longer name the lever at all, so
    // this holds for a ledger that still has one from before.)
    let decision = entries
        .iter()
        .find(|e| matches!(&e.record, Record::Decision { verdict: Verdict::Approve { ask, .. }, .. } if *ask == c.seq))
        .unwrap()
        .seq;
    let refusal = gate::gate_in(
        &k.index().unwrap(),
        &Candidate {
            venture: "shop".into(),
            action: "canary".into(),
            cites: vec![decision],
        },
        &k.adopted(),
        &Quotas::default(),
        avsmd::clock::now(),
    )
    .unwrap_err();
    assert!(
        refusal
            .reasons
            .iter()
            .any(|r| matches!(r, Reason::Hardwired { action, .. } if action == "canary")),
        "{refusal}"
    );
}

#[test]
fn answering_a_canary_writes_the_measured_canary_record() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let (mut k, _dir, ask) = shop_with_ask(tmp.path(), "canary_asks: {every: 1}\n");
    k.deny(ask.seq, "later").unwrap();
    k.tick();
    let first = canary_asks(&k).remove(0);
    assert!(
        !k.entries()
            .unwrap()
            .iter()
            .any(|e| matches!(&e.record, Record::Report { metric, .. } if metric == "canary")),
        "nothing is measured before it is answered"
    );
    k.approve(first.seq, None).unwrap();
    let entries = k.entries().unwrap();
    let decision = entries
        .iter()
        .find(|e| matches!(&e.record, Record::Decision { verdict: Verdict::Approve { ask, .. }, .. } if *ask == first.seq))
        .unwrap();
    let marks: Vec<&Entry> = entries
        .iter()
        .filter(|e| matches!(&e.record, Record::Report { metric, .. } if metric == "canary"))
        .collect();
    assert_eq!(marks.len(), 1);
    assert!(matches!(&marks[0].record,
        Record::Report { claim: Claim::Measured, venture, value: Some(v), cites, .. }
        if venture == "shop" && *v == 1.0 && cites == &vec![decision.seq]));
    assert_eq!(k.index().unwrap().canary_rate("local"), 1.0);

    // A real ask does not leave one; a denied canary does, as 0.
    k.cycle("shop");
    k.tick();
    let second = canary_asks(&k).into_iter().last().unwrap();
    assert_ne!(second.seq, first.seq);
    k.deny(second.seq, "that is wrong").unwrap();
    let values: Vec<f64> = k
        .entries()
        .unwrap()
        .iter()
        .filter_map(|e| match &e.record {
            Record::Report {
                metric,
                value: Some(v),
                ..
            } if metric == "canary" => Some(*v),
            _ => None,
        })
        .collect();
    assert_eq!(values, [1.0, 0.0]);
    assert_eq!(k.index().unwrap().canary_rate("local"), 0.5);
    assert_eq!(k.index().unwrap().canary_rate("someone else"), 0.0);
}

fn open_ask(k: &Kernel, lever: &str) -> Option<Seq> {
    let open = k.open_asks().unwrap();
    let index = k.index().unwrap();
    open.into_iter().find(|q| {
        index
            .get(*q)
            .is_some_and(|e| e.record.action() == Some(lever))
    })
}

/// Approve `n` announce asks, one a night, and say whether self then
/// proposed delegating it.
fn approvals_until_delegation(canaried: bool, n: usize) -> (usize, Kernel, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kernel(tmp.path(), "ratchet: 2\n", Box::new(Announcer { n: 0 }));
    let dir = venture(tmp.path(), "shop", SHOP);
    k.add(&dir).unwrap();
    k.report("shop", false);
    if canaried {
        // The approver approves one canary and denies one: a rate of a half.
        let mut canaries = VecDeque::new();
        for _ in 0..2 {
            let e = k
                .append(Record::Ask {
                    cites: vec![],
                    claim: Claim::Measured,
                    venture: "shop".into(),
                    action: Some("canary".into()),
                    question: "may I reply to an address seen only in an inferred record?".into(),
                    refusal: None,
                    payload: None,
                    signal: None,
                    packet: None,
                })
                .unwrap();
            canaries.push_back(e.seq);
        }
        k.approve(canaries[0], None).unwrap();
        k.deny(canaries[1], "no").unwrap();
        assert_eq!(k.index().unwrap().canary_rate("local"), 0.5);
    }
    for night in 1..=n {
        k.cycle("shop");
        let ask = open_ask(&k, "announce").expect("announce asks each time");
        k.approve(ask, None).unwrap();
        k.tick();
        if open_ask(&k, "delegate").is_some() {
            return (night, k, tmp);
        }
    }
    (0, k, tmp)
}

#[test]
fn the_ratchet_discounts_approvals_from_an_approver_who_approved_a_canary() {
    let _c = clock();
    // Without canaries, two approvals are enough.
    let (nights, _, _tmp) = approvals_until_delegation(false, 8);
    assert_eq!(nights, 2);
    // An approver who approved half the canaries counts each approval for
    // a half: delegation needs twice the approvals.
    let (nights, k, _tmp) = approvals_until_delegation(true, 8);
    assert_eq!(nights, 4, "needs 4 half-weight approvals");
    let proposal = k
        .entries()
        .unwrap()
        .into_iter()
        .find(|e| e.record.kind() == Kind::Proposal && e.record.action() == Some("delegate"))
        .unwrap();
    assert!(
        proposal.record.summary().contains("4 times"),
        "{}",
        proposal.line()
    );
}

#[test]
fn what_a_cap_has_left_does_not_count_runs_dated_in_the_future() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let (mut k, _dir, ask) = shop_with_ask(tmp.path(), "");
    let left = |e: &Entry| {
        let text = format!("{:?}", packet(e));
        text.split("cap ")
            .nth(1)
            .and_then(|s| s.split(" left after").next())
            .unwrap_or_else(|| panic!("no cap left in {text}"))
            .to_string()
    };
    assert_eq!(left(&ask), "8");
    // The run happens two hours ahead of the clock the next ask is written
    // by: it is not in that ask's window yet.
    avsmd::clock::set(T0 + 7_200);
    k.approve(ask.seq, None).unwrap();
    k.tick();
    avsmd::clock::set(T0);
    let ahead = k
        .entries()
        .unwrap()
        .iter()
        .filter(|e| e.at > T0 && matches!(e.record.kind(), Kind::Action | Kind::Intent))
        .count();
    assert!(ahead > 0, "the approved run is dated ahead");
    k.cycle("shop");
    let next = asks(&k)
        .into_iter()
        .rev()
        .find(|e| e.record.action() == Some("announce") && e.seq > ask.seq)
        .expect("a second announce ask");
    assert_eq!(left(&next), "8", "{:?}", packet(&next));
}
