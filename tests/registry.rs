//! The failure-mode registry: one test per way an agent like this goes
//! wrong, each naming the borrowed countermeasure and where it lives in
//! `src/`. Each runs the real kernel, gate and warden end to end. If a
//! countermeasure is removed, its test here fails, whatever else passes.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use avsmd::gate;
use avsmd::kernel::Kernel;
use avsmd::record::{self, Claim, Entry, Kind, Record, Signal};
use avsmd::role::{self, Context, InferRequest, ModelRole, Role};
use avsmd::warden::LocalWarden;
use common::{imap_stub, ran};

/// The virtual clock is shared by the whole process: one test at a time.
static CLOCK: Mutex<()> = Mutex::new(());

const T0: i64 = 1_800_000_000;
const HOUR: i64 = 3_600;
const DAY: i64 = 86_400;

fn clock() -> MutexGuard<'static, ()> {
    let guard = CLOCK.lock().unwrap_or_else(|e| e.into_inner());
    avsmd::clock::set(T0);
    guard
}

// ----- shared scaffolding -----

/// A role that proposes nothing and keeps everything it was shown: the
/// context as JSON, both prompts, and whatever its drill-downs return.
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
        for (what, arg) in [("know", ctx.venture.as_str()), ("know", "audit.refunds")] {
            seen.push(drill(what, arg).unwrap_or_else(|e| e));
        }
        let top = 1 + ctx.entries.iter().map(|e| e.seq).max().unwrap_or(0);
        for seq in 1..=top + 20 {
            if let Ok(t) = drill("why", &seq.to_string()) {
                seen.push(t);
            }
        }
        Ok(vec![])
    }
}

/// A role that never proposes anything.
struct Quiet;

impl Role for Quiet {
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

/// A venture directory whose `report` prints `value`, failing on FAIL.
fn venture(parent: &Path, name: &str, mandate: &str) -> PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("mandate.yaml"), mandate).unwrap();
    script(
        &dir,
        "report",
        "v=$(cat value 2>/dev/null || echo 12); [ \"$v\" = FAIL ] && exit 2; echo \"$v\"",
    );
    dir
}

fn open(home: &Path, config: &str, role: Box<dyn Role + Send>) -> Kernel {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(
        home.join("config.yaml"),
        format!(
            "nightly: never\ndefault: k\nbackends:\n  k: {{kind: command, run: echo nothing}}\n{config}"
        ),
    )
    .unwrap();
    let warden = LocalWarden::open(home).unwrap();
    Kernel::with(home, role, Box::new(warden)).unwrap()
}

fn of_kind(k: &Kernel, kind: Kind) -> Vec<Entry> {
    k.entries()
        .unwrap()
        .into_iter()
        .filter(|e| e.record.kind() == kind)
        .collect()
}

fn signals(k: &Kernel, pick: impl Fn(&Signal) -> bool) -> Vec<Entry> {
    k.entries()
        .unwrap()
        .into_iter()
        .filter(|e| matches!(&e.record, Record::Ask { signal: Some(s), .. } if pick(s)))
        .collect()
}

fn intents(k: &Kernel, venture: &str, action: &str) -> usize {
    of_kind(k, Kind::Intent)
        .iter()
        .filter(|e| e.record.venture() == venture && e.record.action() == Some(action))
        .count()
}

fn first_seq(out: &str) -> u64 {
    out.trim_start_matches('#')
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

/// A threshold venture whose lever `reset` may run once a day.
const COUNTER: &str = "purpose: Keep the counter low.\nmetric: count\n\
     controller: {kind: threshold, above: 10, action: reset}\ncadence: 1h\n\
     permitted:\n- {name: reset, run: echo reset >> resets, cap: 1/day, reversible: true, cost: 2}\n";

/// A planner venture over `levers`, in a world whose reply comes from
/// `world`, a shell body that reads the request on stdin as `$line` and
/// sets `$m`.
fn planner(levers: &str, world: &str) -> (tempfile::TempDir, Kernel, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let mut k = open(&tmp.path().join("home"), "", Box::new(ModelRole));
    let dir = tmp.path().join("shop");
    std::fs::create_dir_all(&dir).unwrap();
    script(
        &dir,
        "world",
        &format!("read -r line\n{world}\necho \"{{\\\"state\\\": null, \\\"metric\\\": $m}}\""),
    );
    script(&dir, "report", "cat value");
    std::fs::write(
        dir.join("mandate.yaml"),
        format!(
            "purpose: Sell.\nmetric: count\n\
             controller: {{kind: plan, horizon: 1, depth: 1, members: 8}}\ncadence: 1h\n\
             permitted:\n{levers}world: ./world\ncalibration: -1\n"
        ),
    )
    .unwrap();
    k.add(&dir).unwrap();
    // Five measured scores: enough to trust the world model at bar -1.
    for _ in 0..5 {
        k.append(Record::report(
            Claim::Measured,
            "shop",
            "calibration.world",
            Some(0.5),
            "",
        ))
        .unwrap();
    }
    assert!(k.trusted("shop"));
    (tmp, k, dir)
}

fn lever(name: &str) -> String {
    format!(
        "- {{name: {name}, run: touch {name}, cap: 99/day, reversible: true, delegated: true}}\n"
    )
}

/// A report of the planner's metric, an hour on.
fn plan_report(k: &mut Kernel, dir: &Path, now: &mut i64, value: f64) {
    *now += HOUR;
    avsmd::clock::set(*now);
    std::fs::write(dir.join("value"), format!("{value}\n")).unwrap();
    k.report("shop", false);
}

// ----- the registry -----

/// FAILURE: survivorship. Only what ran is on record, so what was refused
/// or passed over is invisible and every look at the past is flattering.
///
/// COUNTERMEASURE: refusal and branch records. A refused lever writes a
/// `refusal` record (`src/kernel/refusals.rs`); a plan writes a `branch`
/// record that keeps the chosen sequence and the rejected alternatives
/// with their propensities (`src/kernel/plan.rs`, `src/record.rs`).
///
/// SHOWN: a lever past its cap leaves a refusal naming why; a plan of
/// three levers leaves a branch whose alternatives include the two it did not
/// choose, with propensity 0.
#[test]
fn survivorship_a_refused_lever_and_a_rejected_alternative_leave_records() {
    let _c = clock();
    // A refused lever.
    let tmp = tempfile::tempdir().unwrap();
    let mut k = open(&tmp.path().join("home"), "", Box::new(Quiet));
    k.add(&venture(tmp.path(), "counter", COUNTER)).unwrap();
    k.report("counter", false);
    k.report("counter", false);
    assert_eq!(intents(&k, "counter", "reset"), 1, "the first run");
    let refusals = of_kind(&k, Kind::Refusal);
    assert_eq!(refusals.len(), 1, "the second was refused and is on record");
    assert!(
        matches!(&refusals[0].record,
            Record::Refusal { action, reason, claim: Claim::Measured, .. }
            if action == "reset" && reason == "OverCap(1/day)"),
        "{refusals:?}"
    );

    // A rejected alternative.
    let levers = format!("{}{}{}", lever("a"), lever("b"), lever("c"));
    let world = "case \"$line\" in\n*'\"name\":\"a\"'*) m=30 ;;\n*'\"name\":\"b\"'*) m=20 ;;\n\
                 *'\"name\":\"c\"'*) m=15 ;;\n*) m=10 ;;\nesac";
    let (_tmp, mut k, dir) = planner(&levers, world);
    let mut now = T0;
    plan_report(&mut k, &dir, &mut now, 10.0);
    let branches = of_kind(&k, Kind::Branch);
    assert_eq!(branches.len(), 1);
    let Record::Branch {
        chosen,
        alternatives,
        ..
    } = &branches[0].record
    else {
        panic!("{:?}", branches[0])
    };
    assert_eq!(chosen.action, "a");
    let rejected: Vec<&str> = alternatives.iter().map(|a| a.action.as_str()).collect();
    assert!(
        rejected.contains(&"b") && rejected.contains(&"c"),
        "the plans it did not take are kept: {rejected:?}"
    );
    assert!(alternatives.iter().all(|a| a.propensity == 0.0));
}

/// FAILURE: Goodhart. The metric is the target, so it is the thing being
/// pushed, and it stops measuring what it was chosen to measure.
///
/// COUNTERMEASURE: audits, readings the target does not see. An audit
/// asks when the target improves while the audit worsens
/// (`check_audits` in `src/kernel/guards.rs`), and no audit reading reaches
/// a model prompt or drill-down (`src/role.rs`), so a model
/// cannot learn to move it.
///
/// SHOWN: the target rising while the audit worsens writes one audit ask,
/// the reading is in the ledger, and the model, shown everything it can
/// see, never sees the reading.
#[test]
fn goodhart_target_up_while_the_audit_worsens_asks_and_no_model_sees_the_audit() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut k = open(
        &tmp.path().join("home"),
        "",
        Box::new(Watching(Arc::clone(&seen))),
    );
    let dir = venture(
        tmp.path(),
        "shop",
        "purpose: Sell.\nmetric: orders\ncontroller: {kind: model}\ncadence: 1h\n\
         audits:\n- {name: refunds, run: cat refunds, worse: up, tolerance: 1}\n",
    );
    k.add(&dir).unwrap();
    for (i, orders) in ["1", "2", "3", "4", "5", "6"].iter().enumerate() {
        std::fs::write(dir.join("refunds"), format!("77712{i}")).unwrap();
        std::fs::write(dir.join("value"), orders).unwrap();
        k.report("shop", false);
    }
    let asks = signals(&k, |s| matches!(s, Signal::Audit { .. }));
    assert_eq!(asks.len(), 1, "one open ask per audit: {asks:?}");
    assert!(matches!(
        &asks[0].record,
        Record::Ask {
            claim: Claim::Measured,
            action: None,
            ..
        }
    ));
    assert!(
        k.entries().unwrap().iter().any(|e| matches!(&e.record,
            Record::Report { metric, .. } if metric == "audit.refunds")),
        "the readings are in the ledger"
    );
    k.cycle("shop");
    let all = seen.lock().unwrap().join("\n");
    assert!(all.contains("orders=6"), "the role ran and saw the metric");
    for hidden in ["audit.refunds=", "\"audit.refunds\"", "77712", "\"audit\""] {
        assert!(!all.contains(hidden), "a model saw {hidden:?}:\n{all}");
    }
}

/// FAILURE: thrashing. A controller sitting on its threshold flips its
/// lever, or a parent shifts budget back and forth, burning caps and
/// trust on noise.
///
/// COUNTERMEASURE: the deadband. A threshold with `deadband` re-arms only
/// once the metric is clear of the line by that much (`fires` in
/// `src/kernel/guards.rs`, the field in `src/mandate.rs`); a parent's
/// `allocation` refuses a budget change inside the deadband, and one that
/// reverses the last change inside the hold (`Reason::Reversal`,
/// `src/gate.rs`).
///
/// SHOWN: 11,9,11,9,11 around a threshold of 10 fires three times bare
/// and once with a deadband; a budget going 3 to 6 and back to 3 is
/// refused on the way back.
#[test]
fn thrashing_a_deadband_fires_an_oscillation_once_and_a_reversal_is_refused() {
    let _c = clock();
    let resets = |controller: &str| {
        let tmp = tempfile::tempdir().unwrap();
        let mut k = open(&tmp.path().join("home"), "", Box::new(Quiet));
        let dir = venture(
            tmp.path(),
            "counter",
            &format!(
                "purpose: Keep it low.\nmetric: count\ncontroller: {controller}\ncadence: 1h\n\
                 permitted:\n- {{name: reset, run: echo reset >> resets, cap: 99/day, reversible: true}}\n"
            ),
        );
        k.add(&dir).unwrap();
        for v in ["11", "9", "11", "9", "11"] {
            std::fs::write(dir.join("value"), v).unwrap();
            k.report("counter", false);
        }
        intents(&k, "counter", "reset")
    };
    assert_eq!(
        resets("{kind: threshold, above: 10, action: reset}"),
        3,
        "without a deadband it follows the noise"
    );
    assert_eq!(
        resets("{kind: threshold, above: 10, action: reset, deadband: 2}"),
        1,
        "with one it fires once"
    );

    // The allocation: 3, then 6, then 3 again inside the hold.
    let tmp = tempfile::tempdir().unwrap();
    let mut k = open(&tmp.path().join("home"), "", Box::new(Quiet));
    for name in ["shop", "farm"] {
        k.add(&venture(
            tmp.path(),
            name,
            &format!(
                "purpose: Run {name}.\nmetric: shell echo 1\ncontroller: {{kind: model}}\ncadence: 1h\nparent: portfolio\n"
            ),
        ))
        .unwrap();
    }
    k.add(&venture(
        tmp.path(),
        "portfolio",
        "purpose: Allocate.\nmetric: shell echo 1\ncontroller: {kind: model}\ncadence: 1h\n\
         children: [shop, farm]\nallocation: {deadband: 2, hold: 1d}\n\
         permitted:\n- {name: budget, cap: 9/day, reversible: true, delegated: true}\n",
    ))
    .unwrap();
    let budget = |k: &mut Kernel, amount: &str| {
        k.propose("portfolio", "budget", Some(&format!("shop {amount}")))
            .unwrap()
    };
    assert!(ran(
        &budget(&mut k, "3"),
        "budget",
        "shop may spend 3 a day"
    ));
    assert!(ran(
        &budget(&mut k, "6"),
        "budget",
        "shop may spend 6 a day"
    ));
    let out = budget(&mut k, "3");
    assert!(
        out.contains("shop's budget moved the other way in #"),
        "{out}"
    );
    assert!(!ran(&out, "budget", "shop may spend 3 a day"), "{out}");
    let why = k.why(first_seq(&out)).unwrap();
    assert!(why.contains("because    shop's budget moved"), "{why}");
}

/// FAILURE: alarm fatigue. Past a quota on asks, a human stops reading
/// them, or an ask quota silences the one alarm that matters.
///
/// COUNTERMEASURE: saturation past the quota. When a limit keeps a lever
/// from acting on a crossed threshold, a measured saturation ask is
/// written whatever is left of the asks quota, once while it is open, and
/// it is exempt from that quota (`src/kernel/refusals.rs`,
/// `Signal::Saturated` in `src/record.rs`).
///
/// SHOWN: with `asks: 1/day` already spent, five more refused crossings
/// still write exactly one saturation ask, and it can run no lever.
#[test]
fn alarm_fatigue_a_saturation_ask_is_written_past_the_asks_quota_and_only_once() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let mut k = open(
        &tmp.path().join("home"),
        "quotas: {asks: 1/day}\n",
        Box::new(Quiet),
    );
    k.add(&venture(tmp.path(), "counter", COUNTER)).unwrap();
    // The one ask the quota allows is spent.
    k.append(Record::Ask {
        packet: None,
        cites: vec![],
        claim: Claim::Inferred,
        venture: "counter".into(),
        action: None,
        question: "should we hire?".into(),
        refusal: None,
        signal: None,
        payload: None,
    })
    .unwrap();
    for _ in 0..6 {
        k.report("counter", false);
    }
    let asks = signals(&k, |s| matches!(s, Signal::Saturated { .. }));
    assert_eq!(asks.len(), 1, "{asks:?}");
    let Record::Ask {
        claim,
        action,
        signal: Some(Signal::Saturated { action: lever, .. }),
        ..
    } = &asks[0].record
    else {
        panic!("{:?}", asks[0])
    };
    assert_eq!(*claim, Claim::Measured);
    assert_eq!(*action, None, "an approval must not run a lever");
    assert_eq!(lever, "reset");
    assert_eq!(k.index().unwrap().open_signals().len(), 1);
}

/// FAILURE: hindsight. A lever that never did anything is kept, because
/// afterwards any lever looks as though it helped.
///
/// COUNTERMEASURE: expects. A lever says in advance what it should do to
/// the metric and by when (`expects: up 5 within 2`, `src/mandate.rs`);
/// at the horizon the kernel judges the measured reports against that and
/// suspends the delegation of a lever that missed (`src/kernel/expect.rs`,
/// `src/expect.rs`). Only a declared approval gives it back.
///
/// SHOWN: a lever runs, the metric does not move, and at the horizon an
/// ineffective ask is written and the next crossing is refused, with no
/// ask of its own.
/// One report short of the horizon, nothing is judged.
#[test]
fn hindsight_a_lever_that_does_nothing_is_judged_ineffective_and_loses_its_delegation() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let mut k = open(&tmp.path().join("home"), "", Box::new(Quiet));
    let dir = venture(
        tmp.path(),
        "raiser",
        "purpose: Keep the count up.\nmetric: count\n\
         controller: {kind: threshold, below: 5, action: boost}\ncadence: 1h\n\
         permitted:\n- {name: boost, run: echo boost >> boosts, cap: 1/day, reversible: false, \
         delegated: true, expects: up 5 within 2}\n",
    );
    k.add(&dir).unwrap();
    let mut now = T0;
    let mut report = |k: &mut Kernel, value: f64, later: i64| {
        now += later;
        avsmd::clock::set(now);
        std::fs::write(dir.join("value"), format!("{value}\n")).unwrap();
        k.report("raiser", false);
    };
    let suspended = |k: &Kernel| k.index().unwrap().suspended("raiser", "boost");
    report(&mut k, 3.0, HOUR); // crosses: the delegated lever runs
    assert_eq!(intents(&k, "raiser", "boost"), 1);
    report(&mut k, 3.0, HOUR);
    assert!(!suspended(&k), "one report is not the horizon");
    report(&mut k, 3.0, HOUR); // the horizon: nothing moved
    assert!(suspended(&k));
    let ineffective = signals(&k, |s| matches!(s, Signal::Ineffective { .. }));
    assert_eq!(ineffective.len(), 1, "{ineffective:?}");
    assert!(matches!(&ineffective[0].record,
        Record::Ask { claim: Claim::Measured, action: None,
            signal: Some(Signal::Ineffective { action, .. }), .. } if action == "boost"));
    // Tomorrow's crossing is refused, with a refusal record; it does not
    // run and asks nothing more.
    report(&mut k, 3.0, DAY);
    assert_eq!(intents(&k, "raiser", "boost"), 1);
    let entries = k.entries().unwrap();
    assert!(!entries
        .iter()
        .any(|e| matches!(&e.record, Record::Ask { action: Some(a), .. } if a == "boost")));
    assert!(entries.iter().any(|e| matches!(&e.record,
        Record::Refusal { action, reason, .. } if action == "boost" && reason.contains("Suspended"))));
}

/// FAILURE: tampering. Whoever can write the store can rewrite what
/// happened, and every later proof rests on a lie.
///
/// COUNTERMEASURE: the chain. Each row of the ledger hashes the one before
/// it (`verify_chain` in `src/record.rs`, `Ledger::verify` in
/// `src/ledger.rs`), and the built-in self venture
/// verifies the chain every cycle, reports `integrity` and stops
/// everything when it breaks (`measure` in `src/selfventure.rs`,
/// `src/kernel.rs`).
///
/// SHOWN: one stored row edited behind avsm's back fails verification,
/// and the next self report says `chain broken` and pauses self.
#[test]
fn tampering_editing_one_stored_row_breaks_the_chain_and_self_reports_it() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let mut k = open(&home, "", Box::new(Quiet));
    k.adopt_self().unwrap();
    k.add(&venture(tmp.path(), "counter", COUNTER)).unwrap();
    k.tick();
    let n = k.ledger.verify().expect("an untouched chain verifies");
    assert!(n > 3);
    assert!(k
        .status()
        .unwrap()
        .contains("self         running     integrity=1"));

    let conn = rusqlite::Connection::open(home.join("ledger.db")).unwrap();
    conn.execute_batch(
        "DROP TRIGGER records_no_update; UPDATE records SET at = at + 1 WHERE seq = 2;",
    )
    .unwrap();
    let err = k.ledger.verify().expect_err("one edited row is caught");
    assert!(err.to_string().contains("record #2"), "{err}");
    k.report("self", false);
    let reports: Vec<String> = of_kind(&k, Kind::Report)
        .iter()
        .filter(|e| e.record.venture() == "self")
        .map(|e| e.record.summary())
        .collect();
    assert!(
        reports
            .last()
            .unwrap()
            .starts_with("integrity=0  chain broken: record #2"),
        "{reports:?}"
    );
    assert!(k.status().unwrap().contains("self         paused"));
}

/// FAILURE: cascade. One venture's trouble spreads: its failure stops its
/// siblings, or its parent reaches in and acts as it.
///
/// COUNTERMEASURE: supervision. Each venture is supervised alone: a
/// child's failed report or pause changes only its own state, and the
/// gate refuses any lever a mandate does not itself permit, so a parent
/// never runs a child's lever (`src/gate.rs`, `src/kernel.rs`; parents allocate and children act).
///
/// SHOWN: a child's report fails, then it is paused: no intent is written
/// for the parent or the sibling, the sibling still acts for itself, and
/// the parent proposing the child's lever is refused with no intent.
#[test]
fn cascade_a_childs_failure_or_pause_runs_no_lever_of_a_parent_or_sibling() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let mut k = open(&tmp.path().join("home"), "", Box::new(Quiet));
    let child = |name: &str| {
        format!(
            "purpose: Run {name}.\nmetric: count\ncontroller: {{kind: model}}\ncadence: 1h\nparent: portfolio\n\
             permitted:\n- {{name: spend, run: \"echo spent >> spent\", cap: 9/day, reversible: true, delegated: true}}\n"
        )
    };
    let shop = venture(tmp.path(), "shop", &child("shop"));
    let farm = venture(tmp.path(), "farm", &child("farm"));
    k.add(&shop).unwrap();
    k.add(&farm).unwrap();
    k.add(&venture(
        tmp.path(),
        "portfolio",
        "purpose: Allocate.\nmetric: shell echo 1\ncontroller: {kind: model}\ncadence: 1h\n\
         children: [shop, farm]\n\
         permitted:\n- {name: budget, cap: 9/day, reversible: true, delegated: true}\n\
         - {name: pause, cap: 9/day, reversible: true, delegated: true}\n",
    ))
    .unwrap();

    // The child fails.
    std::fs::write(shop.join("value"), "FAIL").unwrap();
    k.report("shop", false);
    k.tick();
    // Then it is paused.
    k.pause("shop").unwrap();
    k.tick();
    assert!(k.status().unwrap().contains("shop         paused"));
    let strangers = of_kind(&k, Kind::Intent)
        .into_iter()
        .filter(|e| e.record.venture() != "shop")
        .count();
    assert_eq!(strangers, 0, "no lever of anyone else ran");

    // The sibling is untouched and acts for itself.
    let out = k.propose("farm", "spend", None).unwrap();
    assert!(ran(&out, "spend", ""), "{out}");
    assert_eq!(intents(&k, "farm", "spend"), 1);
    assert_eq!(intents(&k, "portfolio", "spend"), 0);

    // A parent cannot run a child's lever.
    let before = of_kind(&k, Kind::Intent).len();
    let err = k.propose("portfolio", "spend", None).unwrap_err();
    assert!(err.contains("portfolio does not permit spend"), "{err}");
    assert_eq!(of_kind(&k, Kind::Intent).len(), before, "no intent");
    assert!(!tmp.path().join("portfolio").join("spent").exists());
    // And the paused child's own lever is held.
    let out = k.propose("shop", "spend", None).unwrap();
    assert!(out.contains("spend refused: shop is paused"), "{out}");
}

/// FAILURE: double effect. A crash between doing a thing and recording it
/// leaves a restart that does the thing again: two emails, two charges.
///
/// COUNTERMEASURE: idempotent intent. Every lever run writes its intent,
/// carrying a key, before it runs and its outcome after
/// (`src/kernel.rs`, `record::key` in `src/record.rs`); an intent with no
/// outcome blocks that lever (`Reason::Pending` in `src/gate.rs`) and is
/// asked about once, never rerun (`Signal::Pending`, `src/kernel.rs`).
///
/// SHOWN: an intent with no outcome survives a restart; neither the
/// reopen nor ticks run the lever, one pending ask is written, the gate
/// refuses it by name, and only a human's answer unblocks it.
#[test]
fn double_effect_a_crash_after_intent_never_reruns_the_lever() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = venture(
        tmp.path(),
        "shop",
        "purpose: Sell.\nmetric: orders\ncontroller: {kind: model}\ncadence: 1h\n\
         permitted:\n- {name: ship, run: \"echo x >> ships; echo $AVSM_KEY > key\", cap: 9/day, reversible: false, delegated: true}\n",
    );
    let ships = || std::fs::read_to_string(dir.join("ships")).map_or(0, |t| t.lines().count());
    let mut k = open(&home, "", Box::new(Quiet));
    k.add(&dir).unwrap();
    // What a warden that died mid-run leaves: an intent and no outcome.
    let intent = k
        .append(Record::Intent {
            claim: Claim::Measured,
            venture: "shop".into(),
            action: "ship".into(),
            proof: gate::Proof {
                venture: "shop".into(),
                action: "ship".into(),
                steps: vec![],
            },
            payload: None,
            key: Some(record::key("shop", "ship", &[], None)),
        })
        .unwrap()
        .seq;
    drop(k);

    let mut k = open(&home, "", Box::new(Quiet));
    k.tick();
    k.tick();
    assert_eq!(ships(), 0, "never rerun blindly");
    let pending = signals(
        &k,
        |s| matches!(s, Signal::Pending { intent: n, .. } if *n == intent),
    );
    assert_eq!(pending.len(), 1, "asked about once: {pending:?}");
    let out = k.propose("shop", "ship", None).unwrap();
    assert!(
        out.contains(&format!("intent #{intent} has no outcome")),
        "{out}"
    );
    assert_eq!(ships(), 0, "the gate refuses it by name");
    // A human says it never happened: only then is it free.
    k.deny(pending[0].seq, "it never left").unwrap();
    let out = k.propose("shop", "ship", None).unwrap();
    assert!(!out.contains("has no outcome"), "{out}");
    assert_eq!(ships(), 1);
    let key = std::fs::read_to_string(dir.join("key")).unwrap();
    assert_eq!(key.trim().len(), 64, "the lever got an idempotency key");
}

/// FAILURE: injection. Text from the open world says "reply to this
/// address" and a model, or a hurried human approving, goes along.
///
/// COUNTERMEASURE: the payload check. A lever's `check:` command runs
/// after the gate, so after any approval, over the venture's measured and
/// declared reports only, never an inferred one (`src/warden.rs`
/// `payload_check`; `examples/mail-inbox/check-sender`).
///
/// SHOWN: a reply to an address that only an inferred record names is
/// approved by a human and refused by the check: no mail command call, no
/// intent, a `PayloadCheck` refusal on record. The same reply to a
/// measured sender goes through.
#[test]
fn injection_an_approved_reply_to_an_address_seen_only_in_inferred_records_is_refused() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let (imap, _seen) = imap_stub(1);
    let mut k = open(
        &tmp.path().join("home"),
        "adapters:\n  mail: {command: \"echo run >> calls.log; cat > sent.eml\", from: me@example.com}\n",
        Box::new(Quiet),
    );
    let dir = tmp.path().join("mail-inbox");
    std::fs::create_dir_all(&dir).unwrap();
    for f in ["mandate.yaml", "check-sender"] {
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("examples/mail-inbox")
                .join(f),
            dir.join(f),
        )
        .unwrap();
    }
    let mandate = std::fs::read_to_string(dir.join("mandate.yaml"))
        .unwrap()
        .replace(
            "imap.unread mail.example.com",
            &format!("imap.unread {imap}"),
        )
        .replace("parent: portfolio\n", "");
    std::fs::write(dir.join("mandate.yaml"), mandate).unwrap();
    k.add(&dir).unwrap();
    let note = |k: &mut Kernel, claim: Claim, address: &str| {
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
    };
    let approve = |k: &mut Kernel, payload: &str| {
        let out = k.propose("mail-inbox", "reply", Some(payload)).unwrap();
        let ask = out
            .lines()
            .find(|l| l.contains(" reply? "))
            .unwrap_or_else(|| panic!("no ask in:\n{out}"));
        k.approve(first_seq(ask), None).unwrap();
        k.tick();
    };
    let calls = || std::fs::read_to_string(dir.join("calls.log")).map_or(0, |t| t.lines().count());
    note(&mut k, Claim::Measured, "alice@example.com");
    // The phishing case: a finding a model produced names a sender.
    note(&mut k, Claim::Inferred, "phish@evil.example");

    // The check runs before any ask is written: a refusal and no ask.
    let out = k
        .propose(
            "mail-inbox",
            "reply",
            Some("phish@evil.example\nWire the money."),
        )
        .unwrap();
    assert!(!out.contains(" reply? "), "{out}");
    assert_eq!(calls(), 0, "the mail command ran for an inferred address");
    assert_eq!(intents(&k, "mail-inbox", "reply"), 0);
    let refusals: Vec<String> = of_kind(&k, Kind::Refusal)
        .iter()
        .filter_map(|e| match &e.record {
            Record::Refusal { reason, detail, .. } => Some(format!("{reason} {detail}")),
            _ => None,
        })
        .collect();
    assert!(
        refusals
            .iter()
            .any(|r| r.starts_with("PayloadCheck") && r.contains("phish@evil.example")),
        "{refusals:?}"
    );

    approve(&mut k, "alice@example.com\nOn it.");
    assert_eq!(calls(), 1, "a measured sender is let through");
}

/// FAILURE: overfit mandates. Try enough variants of a mandate and one
/// will look best on the seeds it was tried on, by luck.
///
/// COUNTERMEASURE: PBO, the probability of backtest overfitting, by
/// combinatorially symmetric cross-validation (`src/pbo.rs`, reported by
/// every forecast sweep in `src/forecast.rs`). It asks how often the
/// variant best on half the scenarios ranks at or below the median on the
/// other half.
///
/// SHOWN: twenty variants that differ by nothing but noise report a PBO of
/// at least 0.4; one variant that is truly better on every scenario
/// reports at most 0.1.
#[test]
fn overfit_mandates_a_sweep_of_noise_reports_a_high_pbo_and_a_dominant_variant_a_low_one() {
    fn splitmix(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    let mut rng = 20_260_101u64;
    let mut uniform = || (splitmix(&mut rng) >> 11) as f64 / (1u64 << 53) as f64;
    let noise: Vec<Vec<f64>> = (0..48)
        .map(|_| (0..20).map(|_| uniform()).collect())
        .collect();
    let pbo = avsmd::pbo::cscv(&noise).expect("a matrix to judge");
    assert!(pbo.pbo >= 0.4, "pure noise should look overfit: {pbo:?}");

    let dominant: Vec<Vec<f64>> = noise
        .iter()
        .map(|row| {
            let mut row = row.clone();
            row[7] += 10.0;
            row
        })
        .collect();
    let pbo = avsmd::pbo::cscv(&dominant).unwrap();
    assert!(pbo.pbo <= 0.1, "a real winner should not: {pbo:?}");
}

/// FAILURE: model error. The world model is wrong, and a planner that
/// believes its average picks the lever that wins on paper and loses in
/// the one world where it is wrong.
///
/// COUNTERMEASURE: the pessimistic ensemble. The planner steps several
/// members of the world model and ranks each lever by its worst member,
/// then its mean (`src/plan.rs` `worst`, `search`; recorded by
/// `src/kernel/plan.rs`).
///
/// SHOWN: a lever whose members mostly see 2 but sometimes 100 has the
/// better best case and a worst of 2; it loses to a lever that is 14 in
/// every member: the branch chooses the steady one and keeps the gamble as
/// a rejected alternative with its low worst.
#[test]
fn model_error_the_plan_picks_the_better_worst_member_not_the_better_mean() {
    let _c = clock();
    // Members whose seed ends in 0, 1 or 2 see the gamble pay 100; the
    // rest see 2.
    let world = "seed=$(echo \"$line\" | sed 's/.*\"seed\":\\([0-9]*\\).*/\\1/')\n\
                 case \"$line\" in\n\
                 *'\"name\":\"steady\"'*) m=14 ;;\n\
                 *'\"name\":\"gamble\"'*) case \"$seed\" in *0|*1|*2) m=100 ;; *) m=2 ;; esac ;;\n\
                 *) m=10 ;;\nesac";
    let levers = format!("{}{}", lever("steady"), lever("gamble"));
    let (_tmp, mut k, dir) = planner(&levers, world);
    let mut now = T0;
    plan_report(&mut k, &dir, &mut now, 10.0);
    let branches = of_kind(&k, Kind::Branch);
    assert_eq!(branches.len(), 1);
    let Record::Branch {
        chosen,
        alternatives,
        ..
    } = &branches[0].record
    else {
        panic!("{:?}", branches[0])
    };
    let gamble = alternatives
        .iter()
        .find(|a| a.action == "gamble")
        .expect("the gamble was weighed");
    // On paper the gamble wins: its best members beat the steady lever's.
    assert!(gamble.quantiles.p90 > 14.0, "{gamble:?}");
    assert_eq!(gamble.worst, 2.0);
    assert_eq!(chosen.action, "steady", "{chosen:?}");
    assert_eq!(chosen.worst, 14.0);
}
