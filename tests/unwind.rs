//! A guard's declared unwind: when a guard's measured reading is past its
//! bound, the kernel runs the one lever, with the one payload, the mandate
//! names for it, recorded then done. Every other lever, and the same lever
//! with any other payload, stays guarded. Caps, budgets and the payload
//! check still hold it; it runs once per episode; the improving loop can
//! neither add nor edit one.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use avsmd::gate::{self, Candidate, Reason, Step};
use avsmd::kernel::Kernel;
use avsmd::mandate::{self, Mandate};
use avsmd::record::{Entry, Kind, Payload, Record, Seq};
use avsmd::role::{Context, InferRequest, Role};
use avsmd::warden::LocalWarden;

static CLOCK: Mutex<()> = Mutex::new(());
const T0: i64 = 1_800_000_000;
const DAY: i64 = 86_400;

fn clock() -> MutexGuard<'static, ()> {
    let guard = CLOCK.lock().unwrap_or_else(|e| e.into_inner());
    avsmd::clock::set(T0);
    guard
}

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

/// The fund's mandate: equity, a trade lever that appends its payload to
/// `trades`, a drawdown guard with an unwind. `cap`, `then`, `check` and
/// `extra` (more guards) vary.
fn fund(cap: &str, then: &str, check: &str, extra: &str) -> String {
    format!(
        "purpose: Grow.\nmetric: equity\ncontroller: {{kind: model}}\ncadence: 1h\n\
         permitted:\n\
         \x20 - name: trade\n    run: \"cat >> trades; echo >> trades\"\n    cap: {cap}\n    reversible: false\n    takes: json\n    fields: {{side: string, qty: integer}}\n{check}\
         \x20 - {{name: tidy, run: touch tidied, cap: 9/day, reversible: true}}\n\
         guards:\n\
         \x20 - {{name: dd, drawdown: 15%, of: equity, then: {then}, unwind: {{action: trade, payload: {{side: sell, qty: 100000}}}}}}\n{extra}"
    )
}

fn venture(parent: &Path, mandate: &str) -> PathBuf {
    let dir = parent.join("fund");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("mandate.yaml"), mandate).unwrap();
    script(&dir, "report", "cat value");
    dir
}

fn kernel(home: &Path) -> Kernel {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(
        home.join("config.yaml"),
        "nightly: never\ndefault: k\nbackends:\n  k: {kind: command, run: echo nothing}\n",
    )
    .unwrap();
    let warden = LocalWarden::open(home).unwrap();
    Kernel::with(home, Box::new(Silent), Box::new(warden)).unwrap()
}

fn report(k: &mut Kernel, dir: &Path, value: &str) {
    std::fs::write(dir.join("value"), value).unwrap();
    k.report("fund", false);
    avsmd::clock::advance(3_600);
}

fn trades(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("trades"))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}

fn intents(k: &Kernel, action: &str) -> Vec<Entry> {
    k.entries()
        .unwrap()
        .into_iter()
        .filter(|e| e.record.kind() == Kind::Intent && e.record.action() == Some(action))
        .collect()
}

fn refusals(k: &Kernel, action: &str) -> Vec<(String, String)> {
    k.entries()
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.record {
            Record::Refusal {
                action: a,
                reason,
                detail,
                ..
            } if a == action => Some((reason, detail)),
            _ => None,
        })
        .collect()
}

fn lines(k: &Kernel) -> String {
    k.entries()
        .unwrap()
        .iter()
        .map(Entry::line)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The latest reading of the drawdown guard.
fn dd_reading(k: &Kernel) -> Seq {
    k.entries()
        .unwrap()
        .iter()
        .rev()
        .find(|e| matches!(&e.record, Record::Report { metric, .. } if metric == "guard.dd"))
        .unwrap()
        .seq
}

fn sell_all() -> serde_json::Value {
    serde_json::json!({"side": "sell", "qty": 100000})
}

// ----- the mandate -----

#[test]
fn an_unwind_parses_round_trips_and_names_a_permitted_lever_with_its_payload() {
    let m = Mandate::parse(&fund("3/day", "ask", "", "")).unwrap();
    let u = m.guards[0].unwind.as_ref().expect("an unwind");
    assert_eq!(u.action, "trade");
    assert_eq!(u.payload, Some(sell_all()));
    let again = Mandate::parse(&serde_yaml::to_string(&m).unwrap()).unwrap();
    assert_eq!(again, m);
    let perm = m.permitted("trade").unwrap();
    assert_eq!(
        u.payload_for(perm).unwrap(),
        Some(Payload::Json { json: sell_all() })
    );
}

#[test]
fn an_unwind_is_refused_at_parse_unless_its_lever_takes_exactly_that_payload() {
    let with = |unwind: &str| {
        let text = fund("3/day", "ask", "", "").replace(
            "unwind: {action: trade, payload: {side: sell, qty: 100000}}",
            unwind,
        );
        Mandate::parse(&text).unwrap_err().to_string()
    };
    for (unwind, wanted) in [
        (
            "unwind: {action: dump, payload: {side: sell, qty: 1}}",
            "not a permitted action",
        ),
        ("unwind: {action: trade}", "takes json"),
        (
            "unwind: {action: trade, payload: {side: sell}}",
            "qty is missing",
        ),
        (
            "unwind: {action: trade, payload: {side: sell, qty: 1.5}}",
            "qty must be integer",
        ),
        (
            "unwind: {action: trade, payload: {side: sell, qty: 1, all: true}}",
            "all is not a field",
        ),
        (
            "unwind: {action: trade, payload: sell all}",
            "a json object",
        ),
        ("unwind: {action: tidy, payload: now}", "takes no payload"),
        ("unwind: {action: stop}", "stop always passes"),
        (
            "unwind: {action: trade, payload: {side: sell, qty: 1}, when: now}",
            "unknown field",
        ),
        ("unwind: trade", "unwind is {action"),
    ] {
        let err = with(unwind);
        assert!(err.contains(wanted), "{unwind}: {err}");
    }
    // A forbidden lever is no unwind either.
    let forbidden = fund("3/day", "ask", "", "") + "forbidden: [trade]\n";
    let err = Mandate::parse(&forbidden).unwrap_err().to_string();
    assert!(err.contains("forbidden"), "{err}");
}

#[test]
fn the_improving_loop_can_neither_add_nor_edit_an_unwind() {
    let with = Mandate::parse(&fund("3/day", "ask", "", "")).unwrap();
    let mut without = with.clone();
    without.guards[0].unwind = None;
    assert_eq!(
        mandate::edits_limits(&without, &with).as_deref(),
        Some("guards.dd.unwind")
    );
    assert_eq!(
        mandate::edits_limits(&with, &without).as_deref(),
        Some("guards.dd.unwind")
    );
    let mut smaller = with.clone();
    smaller.guards[0].unwind.as_mut().unwrap().payload =
        Some(serde_json::json!({"side": "sell", "qty": 1}));
    assert_eq!(
        mandate::edits_limits(&with, &smaller).as_deref(),
        Some("guards.dd.unwind.payload.qty")
    );
    // Only a lever's own `delegated` turning on is the ratchet's move; the
    // same word inside a payload is a limit like any other.
    let mut a = with.clone();
    a.guards[0].unwind.as_mut().unwrap().payload =
        Some(serde_json::json!({"side": "sell", "delegated": false}));
    let mut b = a.clone();
    b.guards[0].unwind.as_mut().unwrap().payload =
        Some(serde_json::json!({"side": "sell", "delegated": true}));
    assert_eq!(
        mandate::edits_limits(&a, &b).as_deref(),
        Some("guards.dd.unwind.payload.delegated")
    );
}

// ----- the kernel -----

#[test]
fn a_tripped_guard_runs_its_unwind_once_with_a_proof_from_the_reading_and_the_mandate() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let dir = venture(tmp.path(), &fund("9/day", "ask", "", ""));
    let mut k = kernel(&tmp.path().join("home"));
    k.add(&dir).unwrap();
    report(&mut k, &dir, "100");
    report(&mut k, &dir, "120");
    assert!(trades(&dir).is_empty(), "inside the bound nothing unwinds");
    report(&mut k, &dir, "101");
    let reading = dd_reading(&k);
    assert_eq!(
        trades(&dir),
        vec![r#"{"qty":100000,"side":"sell"}"#.to_string()],
        "{}",
        lines(&k)
    );
    let ran = intents(&k, "trade");
    assert_eq!(ran.len(), 1);
    let Record::Intent {
        proof,
        payload,
        key,
        ..
    } = &ran[0].record
    else {
        unreachable!()
    };
    assert_eq!(payload, &Some(Payload::Json { json: sell_all() }));
    assert!(key.is_some(), "an idempotency key");
    assert!(proof
        .steps
        .iter()
        .any(|s| matches!(s, Step::Mandate { .. })));
    assert!(proof.steps.iter().any(|s| matches!(s,
        Step::Report { seq, metric, .. } if *seq == reading && metric == "guard.dd")));
    assert!(proof.steps.iter().any(|s| matches!(s,
        Step::Unwind { guard, report, .. } if guard == "dd" && *report == reading)));
    let why = k.why(ran[0].seq).unwrap();
    assert!(
        why.contains("a delegation of exactly this payload, so no ask"),
        "{why}"
    );
    assert!(
        k.entries()
            .unwrap()
            .iter()
            .any(|e| matches!(&e.record, Record::Outcome { intent, ok: true, .. } if *intent == ran[0].seq)),
        "recorded, then done"
    );
    // Still past the bound: one successful unwind is the episode's.
    report(&mut k, &dir, "95");
    report(&mut k, &dir, "90");
    assert_eq!(trades(&dir).len(), 1, "{}", lines(&k));
    assert!(
        refusals(&k, "trade").is_empty(),
        "{:?}",
        refusals(&k, "trade")
    );
    // Back inside, then past again: a new episode unwinds again.
    report(&mut k, &dir, "118");
    report(&mut k, &dir, "100");
    assert_eq!(trades(&dir).len(), 2, "{}", lines(&k));
}

#[test]
fn an_unwind_is_never_held_waiting_on_the_last_run() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    // The trade lever expects equity up within 5 reports: a run's
    // expectation outlasts the next trip of the guard.
    let mandate = fund("9/day", "ask", "    expects: up within 5\n", "");
    let dir = venture(tmp.path(), &mandate);
    let mut k = kernel(&tmp.path().join("home"));
    k.add(&dir).unwrap();
    report(&mut k, &dir, "100");
    report(&mut k, &dir, "120");
    report(&mut k, &dir, "101"); // trips: the unwind runs
    assert_eq!(trades(&dir).len(), 1, "{}", lines(&k));
    report(&mut k, &dir, "118"); // back inside
    report(&mut k, &dir, "100"); // past again, 2 of 5 readings since the run
    assert_eq!(
        trades(&dir).len(),
        2,
        "a tripped guard's unwind is not held by a waiting expectation: {}",
        lines(&k)
    );
    // Any other run of the lever then is held: the last run's
    // expectation waits.
    let candidate = Candidate {
        venture: "fund".into(),
        action: "trade".into(),
        cites: vec![],
    };
    let refusal = gate::gate(
        &candidate,
        &k.entries().unwrap(),
        &k.adopted(),
        avsmd::clock::now(),
    )
    .unwrap_err();
    assert!(
        refusal
            .reasons
            .iter()
            .any(|r| matches!(r, Reason::Waiting { action, .. } if action == "trade")),
        "{refusal}"
    );
}

#[test]
fn only_the_declared_payload_passes_the_guard() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let dir = venture(tmp.path(), &fund("9/day", "ask", "", ""));
    let mut k = kernel(&tmp.path().join("home"));
    k.add(&dir).unwrap();
    for v in ["100", "120", "101"] {
        report(&mut k, &dir, v);
    }
    assert_eq!(trades(&dir).len(), 1);
    let reading = dd_reading(&k);
    // A human's own sell of one share is still guarded.
    let out = k
        .propose("fund", "trade", Some(r#"{"side": "sell", "qty": 1}"#))
        .unwrap();
    assert_eq!(trades(&dir).len(), 1, "{out}\n{}", lines(&k));
    assert!(refusals(&k, "trade")
        .iter()
        .any(|(r, _)| r.contains("Guarded(dd)")));
    let gate = |action: &str, cites: Vec<Seq>| {
        gate::gate_in(
            &k.index().unwrap(),
            &Candidate {
                venture: "fund".into(),
                action: action.into(),
                cites,
            },
            &k.adopted(),
            &k.config.quotas,
            avsmd::kernel::now(),
        )
    };
    let guarded = |r: Result<gate::Proof, gate::Refusal>| {
        r.unwrap_err()
            .reasons
            .iter()
            .any(|r| matches!(r, Reason::Guarded { guard, .. } if guard == "dd"))
    };
    // Another lever citing the reading is guarded.
    assert!(guarded(gate("tidy", vec![reading])));
    // The unwind's lever citing the reading and anything else is guarded:
    // the payload would not be the mandate's alone.
    let proposal = k
        .entries()
        .unwrap()
        .iter()
        .find(|e| e.record.kind() == Kind::Proposal)
        .unwrap()
        .seq;
    assert!(guarded(gate("trade", vec![reading, proposal])));
    assert!(guarded(gate("trade", vec![proposal])));
    // A stale reading past the bound is no occasion once a newer one is in.
    report(&mut k, &dir, "100");
    let stale = gate::gate_in(
        &k.index().unwrap(),
        &Candidate {
            venture: "fund".into(),
            action: "trade".into(),
            cites: vec![reading],
        },
        &k.adopted(),
        &k.config.quotas,
        avsmd::kernel::now(),
    );
    assert!(stale.is_err());
}

#[test]
fn an_unwind_counts_against_the_cap_and_retries_until_it_runs() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let dir = venture(tmp.path(), &fund("1/day", "ask", "", ""));
    let mut k = kernel(&tmp.path().join("home"));
    k.add(&dir).unwrap();
    report(&mut k, &dir, "100");
    // The day's one trade, approved by a human.
    k.propose("fund", "trade", Some(r#"{"side": "buy", "qty": 1}"#))
        .unwrap();
    let ask = *k.open_asks().unwrap().last().unwrap();
    k.approve(ask, None).unwrap();
    k.tick();
    assert_eq!(trades(&dir).len(), 1, "{}", lines(&k));
    report(&mut k, &dir, "120");
    report(&mut k, &dir, "101");
    assert_eq!(trades(&dir).len(), 1, "over the cap: {}", lines(&k));
    assert!(
        refusals(&k, "trade")
            .iter()
            .any(|(r, _)| r.contains("OverCap")),
        "{:?}",
        refusals(&k, "trade")
    );
    assert_eq!(intents(&k, "trade").len(), 1);
    // Still past the bound and not yet run: the next day it runs.
    avsmd::clock::advance(DAY);
    report(&mut k, &dir, "100");
    assert_eq!(trades(&dir).len(), 2, "{}", lines(&k));
    assert!(trades(&dir)[1].contains("100000"));
}

#[test]
fn the_payload_check_still_refuses_an_unwind() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let check = "    check: \"grep -q buy\"\n";
    let dir = venture(tmp.path(), &fund("9/day", "ask", check, ""));
    let mut k = kernel(&tmp.path().join("home"));
    k.add(&dir).unwrap();
    for v in ["100", "120", "101"] {
        report(&mut k, &dir, v);
    }
    assert!(trades(&dir).is_empty(), "{}", lines(&k));
    assert!(intents(&k, "trade").is_empty());
    assert!(
        refusals(&k, "trade")
            .iter()
            .any(|(r, _)| r == "PayloadCheck"),
        "{:?}",
        refusals(&k, "trade")
    );
}

#[test]
fn a_pausing_guard_unwinds_then_stops() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let dir = venture(tmp.path(), &fund("9/day", "pause", "", ""));
    let mut k = kernel(&tmp.path().join("home"));
    k.add(&dir).unwrap();
    for v in ["100", "120", "101"] {
        report(&mut k, &dir, v);
    }
    assert_eq!(trades(&dir).len(), 1, "{}", lines(&k));
    let entries = k.entries().unwrap();
    let intent = entries
        .iter()
        .find(|e| e.record.kind() == Kind::Intent)
        .unwrap()
        .seq;
    let stop = entries
        .iter()
        .find(|e| e.record.action() == Some("stop"))
        .expect("a stop")
        .seq;
    assert!(intent < stop, "unwind, then stop");
    assert_eq!(
        gate::state_in(&k.index().unwrap(), &k.adopted(), "fund"),
        gate::State::Paused
    );
    report(&mut k, &dir, "90");
    assert_eq!(trades(&dir).len(), 1);
}

#[test]
fn another_tripped_guard_without_an_unwind_does_not_hold_this_one() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let extra = "  - {name: exposure, run: cat exposure, above: 60, then: ask}\n";
    let dir = venture(tmp.path(), &fund("9/day", "ask", "", extra));
    std::fs::write(dir.join("exposure"), "80").unwrap();
    let mut k = kernel(&tmp.path().join("home"));
    k.add(&dir).unwrap();
    for v in ["100", "120"] {
        report(&mut k, &dir, v);
    }
    // Exposure alone is past its bound and has no unwind: nothing sells.
    assert!(trades(&dir).is_empty());
    report(&mut k, &dir, "101");
    assert_eq!(trades(&dir).len(), 1, "{}", lines(&k));
}
