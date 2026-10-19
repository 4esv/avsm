//! Guards, audits and plausibility: what the kernel reads beside the
//! metric. A guard past its bound pauses or asks and holds every lever but
//! stop; an audit asks when the target rises while it worsens, and no model
//! ever sees it; an implausible reading is kept and never a premise; a
//! deadband and a hold keep a threshold from thrashing.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use avsmd::gate::{self, Adopted, Candidate, Reason, State};
use avsmd::kernel::Kernel;
use avsmd::mandate::Mandate;
use avsmd::record::{Claim, Entry, Kind, Record, Seq, Signal, Verdict, GENESIS};
use avsmd::role::{self, Context, InferRequest, Role};
use avsmd::warden::LocalWarden;

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
    // The metric reads `value`; a value of FAIL makes the report fail.
    script(
        &dir,
        "report",
        "v=$(cat value); [ \"$v\" = FAIL ] && exit 2; echo \"$v\"",
    );
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

fn set(dir: &Path, file: &str, value: &str) {
    std::fs::write(dir.join(file), value).unwrap();
}

/// Report with the metric at `value`.
fn report(k: &mut Kernel, dir: &Path, name: &str, value: &str) {
    set(dir, "value", value);
    k.report(name, false);
}

fn signals(k: &Kernel, pick: impl Fn(&Signal) -> bool) -> Vec<Entry> {
    k.entries()
        .unwrap()
        .into_iter()
        .filter(|e| matches!(&e.record, Record::Ask { signal: Some(s), .. } if pick(s)))
        .collect()
}

fn count(k: &Kernel, kind: Kind, action: &str) -> usize {
    k.entries()
        .unwrap()
        .iter()
        .filter(|e| e.record.kind() == kind && e.record.action() == Some(action))
        .count()
}

fn state(k: &Kernel, name: &str) -> State {
    gate::state_in(&k.index().unwrap(), &k.adopted(), name)
}

const SHOP: &str = "purpose: Sell.\nmetric: orders\ncontroller: {kind: model}\ncadence: 1h\n";

#[test]
fn a_guard_that_pauses_pauses_and_no_approval_gets_a_lever_past_it() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = venture(
        tmp.path(),
        "shop",
        &format!(
            "{SHOP}permitted:\n- {{name: wipe, run: touch wiped, cap: 9/day, reversible: false}}\n\
             guards:\n- {{name: spend, run: cat spend, above: 100}}\n"
        ),
    );
    set(&dir, "spend", "20");
    let mut k = kernel(&home, Box::new(Silent));
    k.add(&dir).unwrap();
    report(&mut k, &dir, "shop", "5");
    // Asked while the guard is inside.
    k.propose("shop", "wipe", None).unwrap();
    let ask = k.open_asks().unwrap()[0];
    assert_eq!(state(&k, "shop"), State::Asking);

    // The guard goes past its bound: a measured stop pauses the venture.
    set(&dir, "spend", "150");
    report(&mut k, &dir, "shop", "5");
    assert_eq!(state(&k, "shop"), State::Paused);
    let entries = k.entries().unwrap();
    let stop = entries
        .iter()
        .find(|e| e.record.kind() == Kind::Action && e.record.action() == Some("stop"))
        .expect("a stop");
    assert_eq!(stop.record.claim(), Claim::Measured);
    assert!(entries.iter().any(|e| matches!(&e.record,
        Record::Report { metric, value: Some(v), claim: Claim::Measured, .. } if metric == "guard.spend" && *v == 150.0)));

    // Another reading past it does not stop again.
    report(&mut k, &dir, "shop", "5");
    assert_eq!(count(&k, Kind::Action, "stop"), 1);

    // The human approves and resumes: the guard still holds the lever.
    k.approve(ask, None).unwrap();
    k.resume("shop").unwrap();
    k.tick();
    assert!(!dir.join("wiped").exists(), "no approval passes a guard");
    assert_eq!(count(&k, Kind::Intent, "wipe"), 0);
    let entries = k.entries().unwrap();
    let approval = entries
        .iter()
        .rev()
        .find(|e| {
            matches!(
                &e.record,
                Record::Decision {
                    verdict: Verdict::Approve { .. },
                    ..
                }
            )
        })
        .unwrap()
        .seq;
    let refusal = gate::gate_in(
        &k.index().unwrap(),
        &Candidate {
            venture: "shop".into(),
            action: "wipe".into(),
            cites: vec![approval],
        },
        &k.adopted(),
        &k.config.quotas,
        avsmd::kernel::now(),
    )
    .unwrap_err();
    assert!(
        refusal
            .reasons
            .iter()
            .any(|r| matches!(r, Reason::Guarded { guard, .. } if guard == "spend")),
        "{refusal}"
    );
    assert!(!refusal
        .reasons
        .iter()
        .any(|r| matches!(r, Reason::Paused { .. })));
    // Stopping still passes.
    assert!(gate::gate_in(
        &k.index().unwrap(),
        &Candidate {
            venture: "shop".into(),
            action: "stop".into(),
            cites: vec![],
        },
        &k.adopted(),
        &k.config.quotas,
        avsmd::kernel::now(),
    )
    .is_ok());
}

#[test]
fn a_guard_that_asks_asks_once_and_refuses_levers_until_back_inside() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = venture(
        tmp.path(),
        "shop",
        &format!(
            "{SHOP}permitted:\n- {{name: tidy, run: touch tidied, cap: 9/day, reversible: true}}\n\
             guards:\n- {{name: errors, run: cat errors, above: 3, then: ask}}\n"
        ),
    );
    set(&dir, "errors", "5");
    let mut k = kernel(&home, Box::new(Silent));
    k.add(&dir).unwrap();
    for _ in 0..3 {
        report(&mut k, &dir, "shop", "5");
    }
    let asks = signals(&k, |s| matches!(s, Signal::Guard { .. }));
    assert_eq!(asks.len(), 1, "one guard ask: {asks:?}");
    assert!(matches!(&asks[0].record,
        Record::Ask { claim: Claim::Measured, action: None, signal: Some(Signal::Guard { guard, value, bound }), .. }
        if guard == "errors" && *value == 5.0 && *bound == 3.0));
    assert_ne!(
        state(&k, "shop"),
        State::Paused,
        "an asking guard does not pause"
    );
    // Answered while still past the bound: the same breach, no new ask.
    k.deny(asks[0].seq, "seen").unwrap();
    report(&mut k, &dir, "shop", "5");
    assert_eq!(
        signals(&k, |s| matches!(s, Signal::Guard { .. })).len(),
        1,
        "one ask per breach"
    );

    k.propose("shop", "tidy", None).unwrap();
    assert!(!dir.join("tidied").exists(), "refused while past the bound");
    assert!(k.entries().unwrap().iter().any(|e| matches!(&e.record,
        Record::Refusal { action, reason, .. } if action == "tidy" && reason.contains("Guarded(errors)"))));

    set(&dir, "errors", "1");
    report(&mut k, &dir, "shop", "5");
    k.propose("shop", "tidy", None).unwrap();
    assert!(dir.join("tidied").exists(), "back inside, the lever runs");
}

/// The drawdown guard's readings for a run of equity reports.
fn drawdown(values: &[&str]) -> (Vec<f64>, usize) {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = venture(
        tmp.path(),
        "fund",
        "purpose: Grow.\nmetric: equity\ncontroller: {kind: model}\ncadence: 1h\n\
         permitted:\n- {name: trade, run: touch traded, cap: 9/day, reversible: true}\n\
         guards:\n- {name: dd, drawdown: 15%, of: equity, then: ask}\n",
    );
    let mut k = kernel(&home, Box::new(Silent));
    k.add(&dir).unwrap();
    for v in values {
        report(&mut k, &dir, "fund", v);
    }
    let readings = k
        .entries()
        .unwrap()
        .iter()
        .filter_map(|e| match &e.record {
            Record::Report {
                metric,
                value: Some(v),
                ..
            } if metric == "guard.dd" => Some(*v),
            _ => None,
        })
        .collect();
    let asks = signals(
        &k,
        |s| matches!(s, Signal::Guard { guard, .. } if guard == "dd"),
    )
    .len();
    (readings, asks)
}

#[test]
fn the_drawdown_guard_trips_at_fifteen_percent_from_the_peak() {
    let (readings, asks) = drawdown(&["100", "120", "101"]);
    assert_eq!(readings.len(), 3);
    assert!((readings[2] - 15.833).abs() < 0.01, "{readings:?}");
    assert_eq!(asks, 1, "100, 120, 101 trips");
    let (readings, asks) = drawdown(&["100", "120", "103"]);
    assert!((readings[2] - 14.166).abs() < 0.01, "{readings:?}");
    assert_eq!(asks, 0, "100, 120, 103 does not");
}

#[test]
fn a_guard_that_reads_a_lever_or_the_metric_is_refused_at_add() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let mut k = kernel(&home, Box::new(Silent));
    let lever = venture(
        tmp.path(),
        "a",
        &format!(
            "{SHOP}permitted:\n- {{name: tidy, run: ./tidy.sh, cap: 9/day, reversible: true}}\n\
             guards:\n- {{name: g, run: tidy.sh, above: 1}}\n"
        ),
    );
    let err = k.add(&lever).unwrap_err();
    assert!(err.contains("the command of lever \"tidy\""), "{err}");
    let metric = venture(
        tmp.path(),
        "b",
        &format!("{SHOP}guards:\n- {{name: g, run: ./report, above: 1}}\n"),
    );
    let err = k.add(&metric).unwrap_err();
    assert!(err.contains("how the metric is read"), "{err}");
    let adapter = venture(
        tmp.path(),
        "c",
        "purpose: p\nmetric: shell echo 3\ncontroller: {kind: model}\ncadence: 1h\n\
         guards:\n- {name: g, adapter: shell echo 3, above: 1}\n",
    );
    let err = k.add(&adapter).unwrap_err();
    assert!(err.contains("how the metric is read"), "{err}");
    assert!(k.ventures.is_empty(), "nothing was adopted");
}

/// A role that keeps what it was shown, and drills for audits.
struct Watching(Arc<Mutex<Vec<String>>>);

impl Role for Watching {
    fn decide(
        &mut self,
        ctx: &Context,
        _infer: &mut dyn FnMut(&InferRequest) -> Result<String, String>,
        drill: &mut dyn FnMut(&str, &str) -> Result<String, String>,
    ) -> Result<Vec<Record>, String> {
        let mut seen = self.0.lock().unwrap();
        seen.push(serde_json::to_string(ctx).unwrap());
        seen.push(role::prompt(ctx).user);
        seen.push(role::summary_prompt(ctx).user);
        for (what, arg) in [
            ("know", ctx.venture.as_str()),
            ("know", "audit.refunds"),
            ("know", "kid"),
        ] {
            seen.push(drill(what, arg).unwrap_or_else(|e| e));
        }
        for e in ctx.entries.iter().chain(&ctx.to_summarize) {
            seen.push(drill("why", &e.seq.to_string()).unwrap_or_else(|e| e));
        }
        let audit = 1 + ctx.entries.iter().map(|e| e.seq).max().unwrap_or(0);
        for seq in 1..=audit + 20 {
            if let Ok(t) = drill("why", &seq.to_string()) {
                seen.push(t);
            }
        }
        Ok(vec![])
    }
}

#[test]
fn audit_readings_never_reach_a_model() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let shop = venture(
        tmp.path(),
        "shop",
        "purpose: Sell.\nmetric: orders\ncontroller: {kind: model}\ncadence: 1h\nsummarize_after: 4\n\
         children: [kid]\n\
         audits:\n- {name: refunds, run: cat refunds, worse: up, tolerance: 1}\n",
    );
    let kid = venture(
        tmp.path(),
        "kid",
        "purpose: Help.\nmetric: helped\ncontroller: {kind: model}\ncadence: 1h\nparent: shop\n\
         audits:\n- {name: refunds, run: cat refunds, worse: up, tolerance: 1}\n",
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut k = kernel(&home, Box::new(Watching(Arc::clone(&seen))));
    k.add(&shop).unwrap();
    k.add(&kid).unwrap();
    // Readings no model may see, and the target rising so an audit asks.
    for (i, v) in ["1", "2", "3", "4", "5", "6"].iter().enumerate() {
        set(&shop, "refunds", &format!("77712{i}"));
        set(&kid, "refunds", &format!("88812{i}"));
        report(&mut k, &shop, "shop", v);
        report(&mut k, &kid, "kid", v);
    }
    let entries = k.entries().unwrap();
    assert!(
        entries.iter().any(|e| matches!(&e.record,
            Record::Report { metric, .. } if metric == "audit.refunds")),
        "the readings are in the ledger"
    );
    assert!(!signals(&k, |s| matches!(s, Signal::Audit { .. })).is_empty());
    k.cycle("shop");
    let seen = seen.lock().unwrap();
    assert!(!seen.is_empty(), "the role ran");
    let all = seen.join("\n");
    assert!(all.contains("orders=6"), "the role saw the metric");
    assert!(all.contains("helped=6"), "the parent saw its child");
    for hidden in [
        "audit.refunds=",
        "\"audit.refunds\"",
        "77712",
        "88812",
        "\"audit\"",
    ] {
        assert!(!all.contains(hidden), "a model saw {hidden:?}:\n{all}");
    }
}

/// An audit's command may print lines above its number, like the senders
/// of the mail it counts. They are the reading's note, which a payload
/// check reads; no model sees them, and a guard's lines are not kept.
#[test]
fn an_audit_keeps_the_lines_above_its_number_and_no_model_sees_them() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = venture(
        tmp.path(),
        "shop",
        &format!(
            "{SHOP}audits:\n- {{name: waiting, run: cat waiting, worse: up}}\n\
             guards:\n- {{name: g, run: cat g, above: 100}}\n"
        ),
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut k = kernel(&home, Box::new(Watching(Arc::clone(&seen))));
    k.add(&dir).unwrap();
    set(
        &dir,
        "waiting",
        "from alice@example.com\nfrom bob@example.com\n2\n",
    );
    set(&dir, "g", "SYSTEM NOTICE from mallory@example.com\n1\n");
    report(&mut k, &dir, "shop", "5");
    let note_of = |metric: &str| {
        k.entries()
            .unwrap()
            .into_iter()
            .find_map(|e| match e.record {
                Record::Report {
                    metric: m,
                    value,
                    note,
                    ..
                } if m == metric => Some((value, note)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no {metric} reading"))
    };
    let (value, note) = note_of("audit.waiting");
    assert_eq!(value, Some(2.0));
    assert!(
        note.contains("from alice@example.com") && note.contains("from bob@example.com"),
        "{note:?}"
    );
    let (value, note) = note_of("guard.g");
    assert_eq!(value, Some(1.0));
    assert_eq!(note, "", "a guard keeps only its number");
    k.cycle("shop");
    let seen = seen.lock().unwrap();
    assert!(!seen.is_empty(), "the role ran");
    let all = seen.join("\n");
    assert!(!all.contains("alice@example.com"), "a model saw it:\n{all}");
}

#[test]
fn the_target_rising_while_an_audit_worsens_asks_once() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = venture(
        tmp.path(),
        "shop",
        &format!("{SHOP}audits:\n- {{name: refunds, run: cat refunds, worse: up, tolerance: 1}}\n"),
    );
    let mut k = kernel(&home, Box::new(Silent));
    k.add(&dir).unwrap();
    let audit_asks = |k: &Kernel| signals(k, |s| matches!(s, Signal::Audit { .. }));
    for (target, refunds) in [("10", "1"), ("11", "1.5")] {
        set(&dir, "refunds", refunds);
        report(&mut k, &dir, "shop", target);
    }
    assert!(audit_asks(&k).is_empty(), "within tolerance");
    set(&dir, "refunds", "3");
    report(&mut k, &dir, "shop", "12");
    let asks = audit_asks(&k);
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert!(matches!(&asks[0].record,
        Record::Ask { claim: Claim::Measured, signal: Some(Signal::Audit { audit, target_delta, audit_delta }), .. }
        if audit == "refunds" && *target_delta == 2.0 && *audit_delta == 2.0));
    set(&dir, "refunds", "4");
    report(&mut k, &dir, "shop", "13");
    assert_eq!(audit_asks(&k).len(), 1, "one open per audit");

    // The target falling while the audit worsens is no audit ask.
    let tmp2 = tempfile::tempdir().unwrap();
    let dir = venture(
        tmp2.path(),
        "shop",
        &format!("{SHOP}audits:\n- {{name: refunds, run: cat refunds, worse: up, tolerance: 1}}\n"),
    );
    let mut k = kernel(&tmp2.path().join("home"), Box::new(Silent));
    k.add(&dir).unwrap();
    for (target, refunds) in [("12", "1"), ("11", "3"), ("10", "5")] {
        set(&dir, "refunds", refunds);
        report(&mut k, &dir, "shop", target);
    }
    assert!(audit_asks(&k).is_empty());
}

/// How many times the threshold's lever ran over these reports.
fn resets(controller: &str, values: &[&str]) -> usize {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = venture(
        tmp.path(),
        "counter",
        &format!(
            "purpose: Keep it low.\nmetric: count\ncontroller: {controller}\ncadence: 1h\n\
             permitted:\n- {{name: reset, run: echo reset >> resets, cap: 99/day, reversible: true}}\n"
        ),
    );
    let mut k = kernel(&home, Box::new(Silent));
    k.add(&dir).unwrap();
    for v in values {
        report(&mut k, &dir, "counter", v);
    }
    count(&k, Kind::Intent, "reset")
}

#[test]
fn a_deadband_fires_once_on_an_oscillation() {
    let wobble = ["11", "9", "11", "9", "11"];
    assert_eq!(
        resets("{kind: threshold, above: 10, action: reset}", &wobble),
        3
    );
    assert_eq!(
        resets(
            "{kind: threshold, above: 10, action: reset, deadband: 2}",
            &wobble
        ),
        1
    );
    assert_eq!(
        resets(
            "{kind: threshold, above: 10, action: reset, deadband: 2}",
            &["11", "7", "11"]
        ),
        2,
        "back past the deadband, it re-arms"
    );
}

#[test]
fn hold_needs_three_crossed_reports_in_a_row() {
    let c = "{kind: threshold, above: 10, action: reset, hold: 3}";
    assert_eq!(resets(c, &["11", "11"]), 0);
    assert_eq!(resets(c, &["11", "11", "9", "11", "11"]), 0);
    assert_eq!(resets(c, &["11", "11", "9", "11", "11", "11"]), 1);
}

/// Feed reports; return the kernel and its directory.
fn plausibility(controller: &str, values: &[&str]) -> (Kernel, PathBuf, tempfile::TempDir) {
    plausibility_in("[0, 1000]", controller, values)
}

/// The same, with this range.
fn plausibility_in(
    range: &str,
    controller: &str,
    values: &[&str],
) -> (Kernel, PathBuf, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = venture(
        tmp.path(),
        "counter",
        &format!(
            "purpose: Keep it low.\nmetric: count\ncontroller: {controller}\ncadence: 1h\n\
             range: {range}\nmax_step: 50\n\
             permitted:\n- {{name: reset, run: echo reset >> resets, cap: 99/day, reversible: true}}\n"
        ),
    );
    let mut k = kernel(&home, Box::new(Silent));
    k.add(&dir).unwrap();
    for v in values {
        report(&mut k, &dir, "counter", v);
    }
    (k, dir, tmp)
}

#[test]
fn implausible_reports_are_kept_never_a_premise_and_ask_once_an_episode() {
    let above10 = "{kind: threshold, above: 10, action: reset}";
    let above45 = "{kind: threshold, above: 45, action: reset}";
    let below0 = "{kind: threshold, below: 0, action: reset}";
    let cases: [(&str, &str, &[&str], &str); 6] = [
        ("NaN", above10, &["5", "nan"], "NaN"),
        ("missing", above10, &["5", "FAIL"], "missing"),
        ("absurd", above10, &["5", "1000000"], "out of range"),
        ("negative", below0, &["5", "-3"], "out of range"),
        (
            "too big a step",
            above10,
            &["5", "60"],
            "more than max_step",
        ),
        ("flapping", above45, &["10", "45", "5", "46"], "flapping"),
    ];
    for (what, controller, values, why) in cases {
        let (mut k, dir, _tmp) = plausibility(controller, values);
        let entries = k.entries().unwrap();
        let last = entries
            .iter()
            .rev()
            .find(|e| e.record.kind() == Kind::Report && e.record.venture() == "counter")
            .unwrap();
        assert_eq!(last.record.claim(), Claim::Measured, "{what}");
        assert!(
            last.record.implausible().is_some_and(|w| w.contains(why)),
            "{what}: {last:?}"
        );
        assert_eq!(
            count(&k, Kind::Intent, "reset"),
            0,
            "{what}: never a premise"
        );
        let implausible =
            |k: &Kernel| signals(k, |s| matches!(s, Signal::Implausible { .. })).len();
        assert_eq!(implausible(&k), 1, "{what}: one ask");
        // Still implausible: the same episode, no second ask.
        report(&mut k, &dir, "counter", "FAIL");
        assert_eq!(implausible(&k), 1, "{what}: once an episode");
        // A plausible report ends the episode; the next implausible one
        // starts another, which asks only once the first is answered.
        report(&mut k, &dir, "counter", "5");
        let first = signals(&k, |s| matches!(s, Signal::Implausible { .. }))[0].seq;
        k.deny(first, "seen").unwrap();
        report(&mut k, &dir, "counter", "FAIL");
        assert_eq!(implausible(&k), 2, "{what}: a new episode asks");
    }
}

#[test]
fn the_gate_never_takes_an_implausible_report_as_a_premise() {
    let text = "purpose: p\nmetric: count\ncontroller: {kind: threshold, above: 10, action: reset}\n\
                cadence: 1h\npermitted:\n- {name: reset, run: ./reset, cap: 9/day, reversible: true}\n";
    let mandate = Mandate::parse(text).unwrap();
    let hash = Mandate::hash(text);
    let mut entries: Vec<Entry> = Vec::new();
    let mut push = |record: Record| {
        let prev = entries
            .last()
            .map_or(GENESIS.to_string(), |e| e.hash.clone());
        let e = Entry::new(entries.len() as u64 + 1, 1_700_000_000, &prev, record);
        entries.push(e);
        entries.len() as u64
    };
    let adopt = push(Record::Decision {
        claim: Claim::Declared,
        venture: "c".into(),
        verdict: Verdict::Adopt {
            mandate: hash.clone(),
            path: "/c".into(),
            text: None,
        },
        by: None,
    });
    let believed = push(Record::report(
        Claim::Measured,
        "c",
        "count",
        Some(12.0),
        "",
    ));
    let adopted = vec![Adopted {
        seq: adopt,
        venture: "c".into(),
        hash,
        mandate,
    }];
    let cand = |s| Candidate {
        venture: "c".into(),
        action: "reset".into(),
        cites: vec![s],
    };
    assert!(gate::gate(&cand(believed), &entries, &adopted, 1_700_000_001).is_ok());
    let doubted = {
        let mut r = Record::report(Claim::Measured, "c", "count", Some(12.0), "");
        if let Record::Report { implausible, .. } = &mut r {
            *implausible = Some("a step of 12 is more than max_step 1".into());
        }
        let prev = entries.last().unwrap().hash.clone();
        let e = Entry::new(entries.len() as u64 + 1, 1_700_000_000, &prev, r);
        entries.push(e);
        entries.len() as u64
    };
    for refused in [
        gate::gate(&cand(doubted), &entries, &adopted, 1_700_000_001),
        gate::reference::gate(&cand(doubted), &entries, &adopted, 1_700_000_001),
    ] {
        let r = refused.unwrap_err();
        assert!(
            r.reasons
                .iter()
                .any(|x| matches!(x, Reason::NotJustified { .. })),
            "{r}"
        );
    }
}

#[test]
fn existing_mandates_parse_unchanged() {
    for text in [SHOP, "purpose: p\nmetric: n\ncontroller: {kind: threshold, above: 1, action: stop}\ncadence: 1h\n"] {
        let m = Mandate::parse(text).unwrap();
        assert!(m.guards.is_empty() && m.audits.is_empty());
        assert_eq!((m.range, m.max_step), (None, None));
        assert_eq!(m.deadband_hold(), (0.0, 1));
        assert!(!serde_yaml::to_string(&m).unwrap().contains("guards"));
    }
}

#[test]
fn the_warden_reads_a_guard_through_a_built_in_metric() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = venture(
        tmp.path(),
        "shop",
        &format!(
            "{SHOP}guards:\n- {{name: disk, adapter: file.number disk, below: 10, then: ask}}\n"
        ),
    );
    set(&dir, "disk", "4");
    let mut k = kernel(&home, Box::new(Silent));
    k.add(&dir).unwrap();
    report(&mut k, &dir, "shop", "5");
    let asks = signals(
        &k,
        |s| matches!(s, Signal::Guard { guard, value, .. } if guard == "disk" && *value == 4.0),
    );
    assert_eq!(asks.len(), 1, "{:?}", k.entries().unwrap());
}

/// The audit asks for an inbox whose target improves downward, over these
/// (unread, complaints) reports.
fn inbox_audit_asks(reports: &[(&str, &str)]) -> Vec<Entry> {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = venture(
        tmp.path(),
        "inbox",
        "purpose: Answer mail.\nmetric: unread\ncontroller: {kind: model}\ncadence: 1h\n\
         audits:\n- {name: complaints, run: cat complaints, worse: up, better: down, tolerance: 1}\n",
    );
    let mut k = kernel(&home, Box::new(Silent));
    k.add(&dir).unwrap();
    for (unread, complaints) in reports {
        set(&dir, "complaints", complaints);
        report(&mut k, &dir, "inbox", unread);
    }
    signals(&k, |s| matches!(s, Signal::Audit { .. }))
}

#[test]
fn an_audit_knows_which_way_the_target_improves() {
    // Unread mail falling is the target improving: with complaints up past
    // tolerance, that asks.
    let asks = inbox_audit_asks(&[("40", "1"), ("30", "2"), ("20", "4")]);
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert!(matches!(&asks[0].record,
        Record::Ask { signal: Some(Signal::Audit { audit, target_delta, audit_delta }), .. }
        if audit == "complaints" && *target_delta == -20.0 && *audit_delta == 3.0));
    // Unread mail rising is the target getting worse: no audit ask.
    assert!(inbox_audit_asks(&[("20", "1"), ("30", "2"), ("40", "4")]).is_empty());
}

/// The plausibility flag of each of the venture's reports of `metric`.
fn flags(k: &Kernel, venture: &str, metric: &str) -> Vec<Option<String>> {
    k.entries()
        .unwrap()
        .iter()
        .filter_map(|e| match &e.record {
            Record::Report {
                venture: v,
                metric: m,
                implausible,
                ..
            } if v == venture && m == metric => Some(implausible.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_stuck_sensor_then_the_truth_re_anchors_on_the_third_agreeing_reading() {
    // The hostile sim: moisture stuck at 60 while the truth fell to 0.
    let water = "{kind: threshold, below: 20, action: reset}";
    let (mut k, dir, _tmp) = plausibility(water, &["60", "60", "60", "2", "1"]);
    let f = flags(&k, "counter", "count");
    assert!(f[..3].iter().all(Option::is_none), "{f:?}");
    assert!(f[3..].iter().all(Option::is_some), "a jump from 60: {f:?}");
    assert_eq!(count(&k, Kind::Intent, "reset"), 0, "not yet");
    // The third reading that agrees with the two before it is plausible.
    report(&mut k, &dir, "counter", "0");
    let f = flags(&k, "counter", "count");
    assert_eq!(f[5], None, "re-anchored on the third: {f:?}");
    assert_eq!(count(&k, Kind::Intent, "reset"), 1, "and it can act");
    // The ask stays as the record of the jump; the episode is over.
    let asks = signals(&k, |s| matches!(s, Signal::Implausible { .. }));
    assert_eq!(asks.len(), 1, "{asks:?}");
    // Re-anchored, the next readings are plausible, a small wiggle too.
    report(&mut k, &dir, "counter", "1");
    report(&mut k, &dir, "counter", "0");
    let f = flags(&k, "counter", "count");
    assert!(f[6..].iter().all(Option::is_none), "{f:?}");
}

#[test]
fn a_stuck_value_never_anchors_and_the_truth_re_anchors_on_three_that_agree() {
    let none = "{kind: threshold, above: 900, action: reset}";
    let values = [
        "60", "62", "61", "63", "60", "200", "200", "200", "200", "300", "302", "301",
    ];
    let (_k, _dir, _tmp) = plausibility(none, &values);
    let f = flags(&_k, "counter", "count");
    assert!(f[..5].iter().all(Option::is_none), "{f:?}");
    assert!(f[5..11].iter().all(Option::is_some), "never 200: {f:?}");
    assert!(
        f[8].as_deref().is_some_and(|w| w.starts_with("stuck")),
        "the fourth 200 is stuck: {f:?}"
    );
    assert_eq!(f[11], None, "re-anchored on 301: {f:?}");
}

#[test]
fn a_sensor_stuck_at_its_last_value_is_stuck_from_the_fourth() {
    let none = "{kind: threshold, above: 900, action: reset}";
    let values = ["60", "62", "61", "63", "60", "60", "60", "60", "60"];
    let (k, _dir, _tmp) = plausibility(none, &values);
    let f = flags(&k, "counter", "count");
    assert!(f[..7].iter().all(Option::is_none), "{f:?}");
    assert!(
        f[7..]
            .iter()
            .all(|w| w.as_deref().is_some_and(|w| w.starts_with("stuck"))),
        "{f:?}"
    );
    assert_eq!(implausible_asks(&k, "count"), 1, "one ask an episode");
}

#[test]
fn a_reading_at_its_range_floor_is_never_stuck_and_can_act() {
    let water = "{kind: threshold, below: 20, action: reset}";
    // Within max_step all the way down: 0 repeated is dry soil, not stuck.
    let values = ["30", "32", "31", "33", "30", "10", "0", "0", "0", "0", "0"];
    let (k, _dir, _tmp) = plausibility_in("[0, 100]", water, &values);
    let f = flags(&k, "counter", "count");
    assert!(f.iter().all(Option::is_none), "{f:?}");
    assert!(count(&k, Kind::Intent, "reset") > 0);
    // After a jump, 0, 0, 0 at the floor agree and re-anchor on the third.
    let values = ["60", "62", "61", "63", "60", "0", "0", "0", "0", "0"];
    let (k, _dir, _tmp) = plausibility_in("[0, 100]", water, &values);
    let f = flags(&k, "counter", "count");
    assert!(f[5..7].iter().all(Option::is_some), "{f:?}");
    assert!(f[7..].iter().all(Option::is_none), "{f:?}");
    assert_eq!(count(&k, Kind::Intent, "reset"), 3, "the floor acts");
}

/// A metric stuck mid-range at 60: flagged from the fourth repeat.
fn stuck_at_60() -> (Kernel, PathBuf, tempfile::TempDir, Seq) {
    let none = "{kind: threshold, above: 900, action: reset}";
    let values = ["60", "62", "61", "63", "60", "60", "60", "60", "60"];
    let (k, dir, tmp) = plausibility_in("[0, 100]", none, &values);
    let f = flags(&k, "counter", "count");
    assert!(f[7..].iter().all(Option::is_some), "{f:?}");
    let ask = signals(&k, |s| matches!(s, Signal::Implausible { .. }))[0].seq;
    (k, dir, tmp, ask)
}

#[test]
fn a_declared_approval_vouches_for_one_reading_and_a_denial_for_none() {
    // Approving a stuck reading accepts that reading, not its repeats.
    let (mut k, dir, _tmp, ask) = stuck_at_60();
    report(&mut k, &dir, "counter", "60");
    assert!(
        flags(&k, "counter", "count").last().unwrap().is_some(),
        "code never re-anchors"
    );
    k.approve(ask, None).unwrap();
    for _ in 0..5 {
        report(&mut k, &dir, "counter", "60");
    }
    let f = flags(&k, "counter", "count");
    assert!(
        f[f.len() - 5..]
            .iter()
            .all(|w| w.as_deref().is_some_and(|w| w.starts_with("stuck"))),
        "{f:?}"
    );

    // Approving a jump anchors the next step on it.
    let none = "{kind: threshold, above: 900, action: reset}";
    let jump = ["60", "62", "61", "63", "60", "200"];
    for approve in [true, false] {
        let (mut k, dir, _tmp) = plausibility_in("[0, 1000]", none, &jump);
        let ask = signals(&k, |s| matches!(s, Signal::Implausible { .. }))[0].seq;
        if approve {
            k.approve(ask, None).unwrap();
        } else {
            k.deny(ask, "not real").unwrap();
        }
        report(&mut k, &dir, "counter", "201");
        let f = flags(&k, "counter", "count");
        assert_eq!(f[6].is_none(), approve, "approved {approve}: {f:?}");
    }

    let (mut k, dir, _tmp, ask) = stuck_at_60();
    k.deny(ask, "not real").unwrap();
    for _ in 0..3 {
        report(&mut k, &dir, "counter", "60");
    }
    let f = flags(&k, "counter", "count");
    assert!(
        f[f.len() - 3..].iter().all(Option::is_some),
        "denied: {f:?}"
    );
}

#[test]
fn a_measured_or_inferred_approval_never_re_anchors() {
    for claim in [Claim::Measured, Claim::Inferred] {
        let (mut k, dir, tmp, ask) = stuck_at_60();
        let mut ledger =
            avsmd::ledger::Ledger::open(&tmp.path().join("home").join("ledger.db")).unwrap();
        ledger
            .append(Record::Decision {
                claim,
                venture: "counter".into(),
                verdict: Verdict::Approve { ask, payload: None },
                by: None,
            })
            .unwrap();
        drop(ledger);
        for _ in 0..3 {
            report(&mut k, &dir, "counter", "60");
        }
        let f = flags(&k, "counter", "count");
        assert!(
            f[f.len() - 3..].iter().all(Option::is_some),
            "{claim:?}: {f:?}"
        );
    }
}

#[test]
fn a_constant_metric_is_never_stuck() {
    let none = "{kind: threshold, above: 900, action: reset}";
    let (k, _dir, _tmp) = plausibility(none, &["50"; 12]);
    let f = flags(&k, "counter", "count");
    assert!(f.iter().all(Option::is_none), "{f:?}");
}

#[test]
fn expects_is_never_judged_on_stuck_readings() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = venture(
        tmp.path(),
        "pot",
        "purpose: Keep it wet.\nmetric: moisture\ncontroller: {kind: model}\ncadence: 1h\n\
         range: [0, 100]\nmax_step: 50\n\
         permitted:\n- {name: water, run: touch watered, cap: 9/day, reversible: true, delegated: true, expects: up 5 within 1}\n",
    );
    let mut k = kernel(&home, Box::new(Silent));
    k.add(&dir).unwrap();
    for v in ["30", "32", "31", "33", "30", "30", "30"] {
        report(&mut k, &dir, "pot", v);
    }
    k.propose("pot", "water", None).unwrap();
    assert!(dir.join("watered").exists(), "water ran");
    // The sensor is stuck: every reading after the run is flagged.
    for _ in 0..4 {
        report(&mut k, &dir, "pot", "30");
    }
    let f = flags(&k, "pot", "moisture");
    assert!(
        f[7..]
            .iter()
            .all(|w| w.as_deref().is_some_and(|w| w.starts_with("stuck"))),
        "{f:?}"
    );
    assert!(
        signals(&k, |s| matches!(s, Signal::Ineffective { .. })).is_empty(),
        "no judgement on a frozen number"
    );
    assert!(!k
        .entries()
        .unwrap()
        .iter()
        .any(|e| matches!(&e.record, Record::Report { metric, .. } if metric == "expectation")));
}

#[test]
fn a_flap_never_re_anchors() {
    let water = "{kind: threshold, below: 20, action: reset}";
    let (k, _dir, _tmp) = plausibility(
        water,
        &["100", "100", "0", "200", "0", "200", "0", "200", "0", "200"],
    );
    let f = flags(&k, "counter", "count");
    assert!(f[2..].iter().all(Option::is_some), "{f:?}");
    assert_eq!(count(&k, Kind::Intent, "reset"), 0);
}

#[test]
fn every_flap_reading_is_flagged_though_a_human_approves_the_asks() {
    // The hostile sim's flap: 5 and 95 alternating, the human approving
    // each implausible ask as it comes. An approval vouches for one
    // reading; it never makes the next one that equals it plausible.
    let none = "{kind: threshold, above: 900, action: reset}";
    let (mut k, dir, _tmp) = plausibility_in("[0, 100]", none, &["40", "42", "41", "43", "40"]);
    for i in 0..12 {
        report(&mut k, &dir, "counter", if i % 2 == 0 { "95" } else { "5" });
        for ask in signals(&k, |s| matches!(s, Signal::Implausible { .. })) {
            if k.index().unwrap().is_open(ask.seq) {
                k.approve(ask.seq, None).unwrap();
            }
        }
    }
    let f = flags(&k, "counter", "count");
    assert!(f[..5].iter().all(Option::is_none), "{f:?}");
    assert!(f[5..].iter().all(Option::is_some), "all 12 flagged: {f:?}");
}

#[test]
fn a_single_absurd_spike_stays_implausible() {
    let above = "{kind: threshold, above: 200, action: reset}";
    let (k, _dir, _tmp) = plausibility(above, &["100", "100", "100", "900", "100"]);
    let f = flags(&k, "counter", "count");
    assert!(
        f[3].as_deref().is_some_and(|w| w.contains("max_step")),
        "{f:?}"
    );
    assert_eq!(f[4], None, "{f:?}");
    assert_eq!(count(&k, Kind::Intent, "reset"), 0);
}

/// A shop with an asking soil guard below 20, range [0, 100], max_step 30.
fn soil(first: &str) -> (Kernel, PathBuf, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = venture(
        tmp.path(),
        "shop",
        &format!(
            "{SHOP}permitted:\n- {{name: tidy, run: touch tidied, cap: 99/day, reversible: true}}\n\
             guards:\n- {{name: soil, run: cat soil, below: 20, then: ask, range: [0, 100], max_step: 30}}\n"
        ),
    );
    set(&dir, "soil", first);
    let mut k = kernel(&home, Box::new(Silent));
    k.add(&dir).unwrap();
    report(&mut k, &dir, "shop", "5");
    (k, dir, tmp)
}

fn guard_asks(k: &Kernel) -> usize {
    signals(k, |s| matches!(s, Signal::Guard { .. })).len()
}

fn implausible_asks(k: &Kernel, metric: &str) -> usize {
    signals(
        k,
        |s| matches!(s, Signal::Implausible { metric: m, .. } if m == metric),
    )
    .len()
}

#[test]
fn an_implausible_guard_reading_never_trips_a_breach() {
    let (mut k, dir, _tmp) = soil("50");
    for bad in ["nan", "-5", "150"] {
        set(&dir, "soil", bad);
        report(&mut k, &dir, "shop", "5");
        assert_ne!(state(&k, "shop"), State::Guarded, "{bad}");
        assert_eq!(guard_asks(&k), 0, "{bad}");
    }
    let f = flags(&k, "shop", "guard.soil");
    assert_eq!(f[0], None);
    // The warden reads "nan" as no number at all.
    assert!(
        f[1].as_deref()
            .is_some_and(|w| w == "NaN" || w == "missing"),
        "{f:?}"
    );
    assert!(
        f[2].as_deref()
            .is_some_and(|w| w.contains("negative") || w.contains("out of range")),
        "{f:?}"
    );
    assert!(f[3].is_some(), "{f:?}");
    assert!(k.entries().unwrap().iter().any(|e| matches!(&e.record,
        Record::Report { metric, claim: Claim::Measured, implausible: Some(_), .. } if metric == "guard.soil")));
    assert_eq!(implausible_asks(&k, "guard.soil"), 1, "one ask an episode");
    k.propose("shop", "tidy", None).unwrap();
    assert!(dir.join("tidied").exists(), "never guarded");
}

#[test]
fn an_implausible_guard_reading_never_clears_a_breach() {
    let (mut k, dir, _tmp) = soil("10");
    assert_eq!(guard_asks(&k), 1);
    // Answered, so only the reading holds the venture guarded.
    let ask = signals(&k, |s| matches!(s, Signal::Guard { .. }))[0].seq;
    k.deny(ask, "seen").unwrap();
    assert_eq!(state(&k, "shop"), State::Guarded);
    for bad in ["nan", "500", "90"] {
        set(&dir, "soil", bad);
        report(&mut k, &dir, "shop", "5");
        assert_eq!(state(&k, "shop"), State::Guarded, "{bad}");
        k.propose("shop", "tidy", None).unwrap();
        assert!(!dir.join("tidied").exists(), "{bad}: still guarded");
    }
    assert_eq!(implausible_asks(&k, "guard.soil"), 1);
    // Back inside, plausibly: the breach clears, and no second guard ask.
    set(&dir, "soil", "30");
    report(&mut k, &dir, "shop", "5");
    k.propose("shop", "tidy", None).unwrap();
    assert!(dir.join("tidied").exists(), "back inside, the lever runs");
    assert_ne!(state(&k, "shop"), State::Guarded);
    assert_eq!(guard_asks(&k), 1);
}

#[test]
fn an_implausible_guard_reading_never_pauses() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = venture(
        tmp.path(),
        "shop",
        &format!(
            "{SHOP}guards:\n- {{name: spend, run: cat spend, above: 100, range: [0, 1000]}}\n"
        ),
    );
    set(&dir, "spend", "5000");
    let mut k = kernel(&home, Box::new(Silent));
    k.add(&dir).unwrap();
    report(&mut k, &dir, "shop", "5");
    assert_ne!(state(&k, "shop"), State::Paused);
    assert_eq!(count(&k, Kind::Action, "stop"), 0);
}

#[test]
fn an_implausible_audit_reading_never_feeds_its_delta() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = venture(
        tmp.path(),
        "shop",
        &format!(
            "{SHOP}audits:\n- {{name: refunds, run: cat refunds, worse: up, tolerance: 1, range: [0, 100]}}\n"
        ),
    );
    let mut k = kernel(&home, Box::new(Silent));
    k.add(&dir).unwrap();
    for (target, refunds) in [("10", "1"), ("11", "1"), ("12", "5000")] {
        set(&dir, "refunds", refunds);
        report(&mut k, &dir, "shop", target);
    }
    assert!(signals(&k, |s| matches!(s, Signal::Audit { .. })).is_empty());
    assert_eq!(implausible_asks(&k, "audit.refunds"), 1);
}
