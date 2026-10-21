//! Expectations: a delegated lever whose runs do not move the metric as
//! the mandate expects loses its delegation. The loss is measured and
//! the gift back is declared: only a declared approval of the ask that
//! says so restores it.

use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use avsmd::kernel::Kernel;
use avsmd::record::{Claim, Entry, Kind, Record, Seq, Signal, Verdict};
use avsmd::role::{Context, InferRequest, Role};
use avsmd::warden::LocalWarden;

/// The virtual clock is shared by the whole process: one test at a time.
static CLOCK: Mutex<()> = Mutex::new(());

const T0: i64 = 1_800_000_000;
const DAY: i64 = 86_400;
const HOUR: i64 = 3_600;

fn clock() -> MutexGuard<'static, ()> {
    let guard = CLOCK.lock().unwrap_or_else(|e| e.into_inner());
    avsmd::clock::set(T0);
    guard
}

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

fn script(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A venture that raises `count` when it falls below 5, with one lever,
/// `boost`, that may run once a day.
struct Fixture {
    _tmp: tempfile::TempDir,
    k: Kernel,
    dir: PathBuf,
    now: i64,
}

fn fixture(lever: &str, run: &str) -> Fixture {
    fixture_of(false, lever, run)
}

fn fixture_of(reversible: bool, lever: &str, run: &str) -> Fixture {
    fixture_in(
        "controller: {kind: threshold, below: 5, action: boost}\ncadence: 1h\n",
        &format!("{{name: boost, run: {run}, cap: 1/day, reversible: {reversible}, {lever}}}"),
    )
}

/// The hostile sim's tomato: range [0, 100], max_step 30, a threshold
/// below 35, and a delegated boost that expects the count up 5 within one
/// report and may run every hour.
fn tomato() -> Fixture {
    fixture_in(
        "controller: {kind: threshold, below: 35, action: boost}\ncadence: 1h\n\
         range: [0, 100]\nmax_step: 30\n",
        "{name: boost, run: echo boost >> boosts, cap: 99/day, reversible: true, \
         delegated: true, expects: up 5 within 1}",
    )
}

/// A venture with this mandate head (controller, cadence, plausibility)
/// and this one permitted lever.
fn fixture_in(head: &str, lever: &str) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.yaml"),
        "nightly: never\ndefault: k\nbackends:\n  k: {kind: command, run: echo nothing}\n",
    )
    .unwrap();
    let warden = LocalWarden::open(&home).unwrap();
    let mut k = Kernel::with(&home, Box::new(FakeRole(VecDeque::new())), Box::new(warden)).unwrap();
    let dir = tmp.path().join("raiser");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mandate.yaml"),
        format!("purpose: Keep the count up.\nmetric: count\n{head}permitted:\n- {lever}\n"),
    )
    .unwrap();
    script(&dir, "report", "cat value");
    k.add(&dir).unwrap();
    Fixture {
        _tmp: tmp,
        k,
        dir,
        now: T0,
    }
}

impl Fixture {
    /// One report of the metric, an hour after the last.
    fn report(&mut self, value: f64) {
        self.now += HOUR;
        avsmd::clock::set(self.now);
        std::fs::write(self.dir.join("value"), format!("{value}\n")).unwrap();
        self.k.report("raiser", false);
    }

    /// The same, on the next day, when the cap allows another run.
    fn report_tomorrow(&mut self, value: f64) {
        self.now += DAY;
        self.report(value);
    }

    fn entries(&self) -> Vec<Entry> {
        self.k.entries().unwrap()
    }

    fn intents(&self) -> usize {
        self.entries()
            .iter()
            .filter(|e| e.record.kind() == Kind::Intent)
            .count()
    }

    fn expectations(&self) -> Vec<Entry> {
        self.entries()
            .into_iter()
            .filter(
                |e| matches!(&e.record, Record::Report { metric, .. } if metric == "expectation"),
            )
            .collect()
    }

    /// The asks that say a lever is ineffective.
    fn ineffective(&self) -> Vec<Entry> {
        self.entries()
            .into_iter()
            .filter(|e| {
                matches!(
                    &e.record,
                    Record::Ask {
                        signal: Some(Signal::Ineffective { .. }),
                        ..
                    }
                )
            })
            .collect()
    }

    /// Asks about running the lever itself.
    fn lever_asks(&self) -> Vec<Entry> {
        self.entries()
            .into_iter()
            .filter(|e| matches!(&e.record, Record::Ask { action: Some(a), .. } if a == "boost"))
            .collect()
    }

    fn suspended(&self) -> bool {
        let index = self.k.index().unwrap().suspended("raiser", "boost");
        let reference = avsmd::gate::reference::suspended(&self.entries(), "raiser", "boost");
        assert_eq!(index, reference, "the index and the reference agree");
        index
    }

    /// The plausibility flag of each report of the metric, oldest first.
    fn flags(&self) -> Vec<Option<String>> {
        self.entries()
            .iter()
            .filter_map(|e| match &e.record {
                Record::Report {
                    metric,
                    implausible,
                    ..
                } if metric == "count" => Some(implausible.clone()),
                _ => None,
            })
            .collect()
    }

    /// Approve every open ask this picks, as a human who says yes would.
    fn approve_open(&mut self, pick: impl Fn(&Record) -> bool) {
        let open: Vec<Seq> = self
            .entries()
            .iter()
            .filter(|e| pick(&e.record) && self.k.index().unwrap().is_open(e.seq))
            .map(|e| e.seq)
            .collect();
        for seq in open {
            self.k.approve(seq, None).unwrap();
        }
    }

    /// Refusal records of the lever for being suspended.
    fn suspended_refusals(&self) -> usize {
        self.entries()
            .iter()
            .filter(|e| {
                matches!(&e.record, Record::Refusal { action, reason, .. }
                if action == "boost" && reason.contains("Suspended"))
            })
            .count()
    }

    /// Approve every open ask to run the lever itself.
    fn approve_lever_asks(&mut self) {
        self.approve_open(|r| matches!(r, Record::Ask { action: Some(a), .. } if a == "boost"));
    }
}

fn implausible_ask(r: &Record) -> bool {
    matches!(
        r,
        Record::Ask {
            signal: Some(Signal::Implausible { .. }),
            ..
        }
    )
}

const TEST_DELEGATED: &str = "delegated: true, expects: up 5 within 2";

#[test]
fn within_tolerance_writes_nothing() {
    let _c = clock();
    let mut f = fixture(TEST_DELEGATED, "echo boost >> boosts");
    f.report(3.0); // crosses: boost runs
    f.report(6.0);
    f.report(8.0); // up 5 from 3, as expected
    f.report(9.0);
    assert_eq!(f.intents(), 1);
    assert!(f.expectations().is_empty());
    assert!(f.ineffective().is_empty());
    assert!(!f.suspended());
}

#[test]
fn the_right_way_by_the_wrong_size_is_one_measured_report() {
    let _c = clock();
    let mut f = fixture(TEST_DELEGATED, "echo boost >> boosts");
    f.report(3.0);
    f.report(12.0);
    f.report(13.0); // up 10 where 5 was expected
    f.report(14.0);
    f.report(15.0);
    let got = f.expectations();
    assert_eq!(got.len(), 1, "{got:?}");
    match &got[0].record {
        Record::Report {
            claim,
            venture,
            value,
            cites,
            ..
        } => {
            assert_eq!(*claim, Claim::Measured);
            assert_eq!(venture, "raiser");
            assert_eq!(*value, Some(10.0));
            let intent = f
                .entries()
                .iter()
                .find(|e| e.record.kind() == Kind::Intent)
                .unwrap()
                .seq;
            assert_eq!(cites, &vec![intent]);
        }
        other => panic!("{other:?}"),
    }
    assert!(f.ineffective().is_empty());
    assert!(!f.suspended());
}

/// A run that did not move the metric as expected, in the three ways.
fn missed(f: &mut Fixture) {
    assert_eq!(f.ineffective().len(), 1, "{:#?}", f.entries());
    assert!(f.suspended());
    match &f.ineffective()[0].record {
        Record::Ask {
            claim,
            action,
            signal: Some(Signal::Ineffective { action: lever, why }),
            ..
        } => {
            assert_eq!(*claim, Claim::Measured);
            assert_eq!(*action, None);
            assert_eq!(lever, "boost");
            assert!(!why.is_empty());
        }
        other => panic!("{other:?}"),
    }
    // The delegation is suspended: tomorrow's crossing is refused, and
    // asks nothing.
    let runs = f.intents();
    f.report_tomorrow(3.0);
    assert_eq!(f.intents(), runs, "refused, not run");
    assert!(f.lever_asks().is_empty(), "{:?}", f.lever_asks());
    assert!(f.suspended_refusals() > 0, "the refusal is a record");
    // Only one ineffective ask ever, and it is open.
    assert_eq!(f.ineffective().len(), 1);
}

#[test]
fn the_wrong_direction_suspends_the_delegation() {
    let _c = clock();
    let mut f = fixture(TEST_DELEGATED, "echo boost >> boosts");
    f.report(3.0);
    f.report(2.0);
    assert!(!f.suspended(), "one report is not enough to judge");
    f.report(1.0); // down 2 where up 5 was expected
                   // The wrong way may be a reading that has not caught up with the run:
                   // it has until boost may run again to show its effect late, and is
                   // judged on the first report past that, before it may run.
    assert!(!f.suspended(), "waits out the window");
    f.report_tomorrow(1.0);
    missed(&mut f);
}

#[test]
fn no_effect_suspends_the_delegation() {
    let _c = clock();
    let mut f = fixture(TEST_DELEGATED, "echo boost >> boosts");
    f.report(3.0);
    f.report(3.0);
    f.report(3.0);
    missed(&mut f);
}

#[test]
fn revoke_after_counts_failed_outcomes() {
    let _c = clock();
    let mut f = fixture("delegated: true, revoke_after: 2", "exit 1");
    f.report(3.0); // runs, fails
    assert_eq!(f.intents(), 1);
    assert!(!f.suspended(), "one failure is under revoke_after");
    f.report_tomorrow(3.0); // runs, fails again
    assert_eq!(f.intents(), 2);
    assert!(f.suspended(), "{:#?}", f.entries());
    missed(&mut f);
}

#[test]
fn a_success_between_failures_resets_the_count() {
    let _c = clock();
    let mut f = fixture("delegated: true, revoke_after: 2", "test -f ok");
    f.report(3.0); // fails
    std::fs::write(f.dir.join("ok"), "").unwrap();
    f.report_tomorrow(3.0); // succeeds
    std::fs::remove_file(f.dir.join("ok")).unwrap();
    f.report_tomorrow(3.0); // fails
    assert_eq!(f.intents(), 3);
    assert!(!f.suspended());
}

#[test]
fn failures_do_not_count_without_revoke_after() {
    let _c = clock();
    let mut f = fixture("delegated: true, expects: up 5 within 2", "exit 1");
    f.report(3.0);
    f.report_tomorrow(3.0);
    f.report_tomorrow(3.0);
    assert_eq!(f.intents(), 3);
    assert!(!f.suspended());
}

#[test]
fn a_reversible_lever_that_does_nothing_is_suspended_after_one_miss() {
    let _c = clock();
    // The tomato died and the water ran on: a reversible lever, fired by
    // a threshold, whose runs do not move the metric.
    let mut f = fixture_of(true, "expects: up 5 within 1", "echo boost >> boosts");
    f.report(3.0); // crosses: runs, reversibly
    assert_eq!(f.intents(), 1);
    f.report(3.0); // no effect: suspended
    assert!(f.suspended());
    for _ in 0..50 {
        f.report_tomorrow(3.0);
    }
    assert_eq!(f.intents(), 1, "50 more crossings, no more runs");
    assert!(f.lever_asks().is_empty(), "no ask a run, not fifty");
    assert_eq!(f.ineffective().len(), 1, "the one ask of the episode");
    let refused = f
        .entries()
        .into_iter()
        .find_map(|e| match e.record {
            Record::Refusal { detail, .. } => Some(detail),
            _ => None,
        })
        .unwrap();
    assert!(
        refused.contains("found ineffective"),
        "the refusal says why: {refused}"
    );
    assert!(!refused.contains("is irreversible"), "{refused}");
}

#[test]
fn a_lever_that_is_not_delegated_can_be_suspended() {
    let _c = clock();
    let mut f = fixture_of(true, "expects: up 5 within 1", "echo boost >> boosts");
    f.report(3.0);
    f.report(3.0);
    assert!(f.suspended());
    assert_eq!(f.ineffective().len(), 1);
}

#[test]
fn a_declared_adoption_after_the_ask_restores_the_lever() {
    let _c = clock();
    let (mut f, ask) = suspended();
    f.k.deny(ask, "no").unwrap();
    assert!(f.suspended());
    let runs = f.intents();
    let dir = f.dir.clone();
    f.k.add(&dir).unwrap();
    assert!(!f.suspended());
    f.report_tomorrow(3.0);
    assert_eq!(f.intents(), runs + 1, "runs again");
}

/// A suspended lever, and the ask that suspended it.
fn suspended() -> (Fixture, Seq) {
    let mut f = fixture(TEST_DELEGATED, "echo boost >> boosts");
    f.report(3.0);
    f.report(3.0);
    f.report(3.0);
    assert!(f.suspended());
    let ask = f.ineffective()[0].seq;
    (f, ask)
}

#[test]
fn only_a_declared_approval_restores_the_delegation() {
    let _c = clock();
    let (mut f, ask) = suspended();
    let runs = f.intents();
    f.k.approve(ask, None).unwrap();
    assert!(!f.suspended());
    f.report_tomorrow(3.0);
    assert_eq!(f.intents(), runs + 1, "delegated again: it runs");
}

#[test]
fn a_measured_record_cannot_restore_the_delegation() {
    let _c = clock();
    let (mut f, ask) = suspended();
    let runs = f.intents();
    f.k.append(Record::Decision {
        claim: Claim::Measured,
        venture: "raiser".into(),
        verdict: Verdict::Approve { ask, payload: None },
        by: None,
    })
    .unwrap();
    // Nor a report, a model's say-so, nor another venture's decision.
    f.k.append(Record::Decision {
        claim: Claim::Inferred,
        venture: "raiser".into(),
        verdict: Verdict::Approve { ask, payload: None },
        by: None,
    })
    .unwrap();
    f.k.append(Record::Decision {
        claim: Claim::Declared,
        venture: "someone-else".into(),
        verdict: Verdict::Approve { ask, payload: None },
        by: None,
    })
    .unwrap();
    assert!(f.suspended());
    f.report_tomorrow(3.0);
    assert_eq!(f.intents(), runs, "still an ask instead of a run");
}

#[test]
fn a_denial_leaves_the_delegation_suspended() {
    let _c = clock();
    let (mut f, ask) = suspended();
    f.k.deny(ask, "no").unwrap();
    assert!(f.suspended());
}

/// A suspended lever whose ask was denied, asked again on a reading
/// somewhere new: the fixture and the new ask, still open.
fn denied_then_asked_again() -> (Fixture, Seq) {
    let (mut f, ask) = suspended();
    f.k.deny(ask, "no").unwrap();
    assert!(f.suspended());
    f.report_tomorrow(4.0); // refused; something new was measured: asks again
    let asks = f.ineffective();
    assert_eq!(asks.len(), 2, "{asks:#?}");
    assert!(f.suspended(), "an open ask is no answer");
    (f, asks[1].seq)
}

#[test]
fn an_approval_after_a_denial_restores_the_lever() {
    let _c = clock();
    let (mut f, again) = denied_then_asked_again();
    let runs = f.intents();
    f.k.approve(again, None).unwrap();
    assert!(
        !f.suspended(),
        "the latest word governs, not the first denial"
    );
    f.report_tomorrow(4.0);
    assert_eq!(f.intents(), runs + 1, "delegated again: it runs");
    // It misses again on readings that say nothing new: a relapse after
    // the approved ask suspends it, measured. Its first run read +1 a
    // report late, so this one is judged there too: a report more.
    f.report(4.0);
    f.report(4.0);
    f.report(4.0);
    assert!(f.suspended(), "{:#?}", f.entries());
    assert_eq!(f.ineffective().len(), 2, "a relapse, not a new ask");
}

#[test]
fn a_denial_never_answered_by_an_approval_stays_suspended() {
    let _c = clock();
    let (mut f, again) = denied_then_asked_again();
    f.k.deny(again, "still no").unwrap();
    assert!(f.suspended());
    let runs = f.intents();
    f.report_tomorrow(3.0);
    assert_eq!(f.intents(), runs, "refused, not run");
    assert!(f.suspended());
}

#[test]
fn an_approval_of_an_old_ask_never_answers_a_newer_one() {
    let _c = clock();
    let (mut f, ask) = suspended();
    f.k.deny(ask, "no").unwrap();
    f.report_tomorrow(4.0); // asks again
    assert_eq!(f.ineffective().len(), 2);
    // The first ask is decided and cannot be approved now; an approval
    // record of it, declared, still says nothing about the newer one.
    f.k.append(Record::Decision {
        claim: Claim::Declared,
        venture: "raiser".into(),
        verdict: Verdict::Approve { ask, payload: None },
        by: None,
    })
    .unwrap();
    assert!(f.suspended(), "the newest ask is still open");
}

/// The restored step of each intent of the lever, oldest first.
fn restored_steps(f: &Fixture) -> Vec<Option<(Seq, Seq)>> {
    f.entries()
        .iter()
        .filter_map(|e| match &e.record {
            Record::Intent { action, proof, .. } if action == "boost" => {
                Some(proof.steps.iter().find_map(|s| match s {
                    avsmd::gate::Step::Restored { ask, decision } => Some((*ask, *decision)),
                    _ => None,
                }))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn a_run_of_a_restored_lever_cites_the_restoring_approval() {
    let _c = clock();
    let (mut f, again) = denied_then_asked_again();
    let before = restored_steps(&f);
    assert!(
        before.iter().all(Option::is_none),
        "a lever never found ineffective, or held, runs on its own premises: {before:?}"
    );
    f.k.approve(again, None).unwrap();
    let decision = f
        .entries()
        .iter()
        .find(|e| {
            matches!(&e.record, Record::Decision { verdict: Verdict::Approve { ask, .. }, .. }
                if *ask == again)
        })
        .unwrap()
        .seq;
    f.report_tomorrow(4.0);
    let after = restored_steps(&f);
    assert_eq!(after.len(), before.len() + 1, "it runs");
    assert_eq!(
        after.last().copied().flatten(),
        Some((again, decision)),
        "the run cites the ask that found it ineffective and the approval that restored it, not the denied one"
    );
    assert_eq!(
        f.k.index().unwrap().restored("raiser", "boost"),
        Some((again, decision))
    );
    assert_eq!(
        avsmd::gate::reference::restored(&f.entries(), "raiser", "boost"),
        Some((again, decision))
    );
    let intent = f
        .entries()
        .into_iter()
        .rev()
        .find(|e| e.record.kind() == Kind::Intent)
        .unwrap();
    let why = avsmd::gate::why(&intent);
    assert!(
        why.contains(&format!("restored   #{decision}")),
        "why shows it: {why}"
    );
    // A relapse suspends it again, at the delay its first run showed:
    // no longer restored.
    f.report(4.0);
    f.report(4.0);
    f.report(4.0);
    assert!(f.suspended());
    assert_eq!(f.k.index().unwrap().restored("raiser", "boost"), None);
}

#[test]
fn a_fresh_miss_after_a_restore_suspends_again() {
    let _c = clock();
    let (mut f, ask) = suspended();
    f.k.approve(ask, None).unwrap();
    f.report_tomorrow(3.0); // runs
    f.report(3.0);
    f.report(3.0); // no effect again
    assert!(f.suspended());
    // Nothing new was measured since the ask: a relapse, not a new ask.
    assert_eq!(f.ineffective().len(), 1);
    let relapse = f
        .entries()
        .into_iter()
        .find(|e| matches!(&e.record, Record::Report { metric, .. } if metric == "suspension"))
        .expect("a relapse report");
    assert_eq!(relapse.record.claim(), Claim::Measured);
    assert!(matches!(&relapse.record, Record::Report { cites, .. } if cites == &vec![ask]));
    // A reading somewhere new is evidence: the next crossing asks once.
    f.report_tomorrow(4.0);
    assert_eq!(f.ineffective().len(), 2);
    assert!(f.suspended());
    // A re-adoption restores it as before.
    let dir = f.dir.clone();
    f.k.add(&dir).unwrap();
    assert!(!f.suspended());
}

#[test]
fn a_run_is_judged_net_of_the_drift_it_fights() {
    let _c = clock();
    // Evaporation pulls 10 an hour; a boost shows +4.5, so it added 14.5.
    let mut f = fixture(
        "delegated: true, expects: up 15 within 1",
        "echo boost >> boosts",
    );
    for v in [65.0, 55.0, 45.0, 35.0, 25.0, 15.0, 4.0] {
        f.report(v);
    }
    assert_eq!(f.intents(), 1, "4 crossed: boost ran");
    f.report(8.5);
    assert!(f.ineffective().is_empty(), "{:?}", f.ineffective());
    assert!(f.expectations().is_empty(), "met, net of the drift");
    assert!(!f.suspended());
}

#[test]
fn a_long_heat_wave_with_the_plant_alive_still_meets() {
    let _c = clock();
    let mut f = fixture(
        "delegated: true, expects: up 15 within 1",
        "echo boost >> boosts",
    );
    // Thirty hours falling 10 an hour, then a boost that shows +4.5.
    for i in 0..30 {
        f.report(304.0 - 10.0 * f64::from(i));
    }
    f.report(4.0);
    assert_eq!(f.intents(), 1);
    f.report(8.5);
    assert!(f.ineffective().is_empty(), "{:?}", f.ineffective());
    assert!(!f.suspended());
}

#[test]
fn after_a_heat_wave_a_dead_plant_is_no_effect_within_two_runs() {
    let _c = clock();
    let mut f = fixture(TEST_DELEGATED, "echo boost >> boosts");
    // A day and more of heat: 10 an hour lost.
    for i in 0..30 {
        f.report(305.0 - 10.0 * f64::from(i));
    }
    // Then dead: flat at 0, an hour apart. The boost runs once a day.
    for _ in 0..60 {
        f.report(0.0);
        if f.suspended() {
            break;
        }
    }
    assert!(f.intents() <= 2, "within two runs: {}", f.intents());
    assert!(f.suspended(), "a dead plant is no effect");
    let asks = f.ineffective();
    assert!(
        matches!(&asks[0].record, Record::Ask { signal: Some(Signal::Ineffective { why, .. }), .. }
            if why.contains("no effect")),
        "{asks:?}"
    );
}

#[test]
fn a_dead_metric_with_no_drift_is_no_effect() {
    let _c = clock();
    let mut f = fixture(TEST_DELEGATED, "echo boost >> boosts");
    for _ in 0..4 {
        f.report(0.0);
    }
    f.report_tomorrow(0.0); // runs again, with a history of no drift
    f.report(0.0);
    f.report(0.0);
    assert!(f.suspended(), "{:?}", f.entries());
    let asks = f.ineffective();
    assert!(
        matches!(&asks[0].record, Record::Ask { signal: Some(Signal::Ineffective { why, .. }), .. }
            if why.contains("no effect")),
        "{asks:?}"
    );
}

#[test]
fn an_approved_implausible_reading_never_makes_its_repeats_plausible() {
    let _c = clock();
    // Issue #39: 43.4, then the sensor stuck at 12.0. The human approves
    // the ask about the jump; the repeats of 12.0, with boost running
    // between them, are still stuck, and boost is never judged on them.
    let mut f = tomato();
    for v in [50.0, 48.0, 46.5, 44.0, 43.4] {
        f.report(v);
    }
    f.report(12.0);
    f.approve_open(implausible_ask);
    for _ in 0..10 {
        f.k.propose("raiser", "boost", None).unwrap();
        f.report(12.0);
    }
    let flags = f.flags();
    assert!(
        flags[6..]
            .iter()
            .all(|w| w.as_deref().is_some_and(|w| w.starts_with("stuck"))),
        "{flags:?}"
    );
    assert!(f.ineffective().is_empty(), "{:?}", f.ineffective());
    assert_eq!(f.intents(), 10, "boost ran between the readings");
    assert!(f.expectations().is_empty());
    assert!(!f.suspended());
}

#[test]
fn a_missing_reading_never_breaks_a_frozen_plateau_after_a_run() {
    let _c = clock();
    // The hostile quarter: the tomato died at 6.9 after a watering; a NaN
    // later made the next 6.9 look fresh, and the water ran again.
    let mut f = tomato();
    for v in [40.0, 42.0, 41.0, 43.0, 40.0] {
        f.report(v);
    }
    f.report(20.0); // crosses: boost runs
    assert_eq!(f.intents(), 1);
    f.report(20.0); // exactly the same after a run: stuck
    for _ in 0..12 {
        f.report(f64::NAN); // half a day of the sensor gone
    }
    f.report(20.0);
    for _ in 0..12 {
        f.report(250.0); // then absurd, out of range
    }
    f.report(20.0);
    f.report(20.0);
    let flags = f.flags();
    assert!(flags[6..].iter().all(Option::is_some), "{flags:?}");
    assert_eq!(f.intents(), 1, "never a premise for another run");
}

#[test]
fn a_sensor_that_recovers_never_suspends_a_lever_that_ran_while_it_was_stuck() {
    let _c = clock();
    let mut f = tomato();
    for v in [44.0, 43.0, 44.0, 43.0, 43.4] {
        f.report(v);
    }
    // Stuck low: every reading flagged; boost runs between them.
    for _ in 0..6 {
        f.k.propose("raiser", "boost", None).unwrap();
        f.report(12.0);
    }
    assert!(
        f.flags()[5..].iter().all(Option::is_some),
        "{:?}",
        f.flags()
    );
    // Back, and lower than before the runs: judged across the stuck
    // readings, that would be the wrong direction. The first reading
    // back is unconfirmed; the next agrees and confirms it.
    for v in [40.0, 41.0, 42.0] {
        f.report(v);
    }
    assert!(
        f.flags()[11]
            .as_deref()
            .is_some_and(|w| w.starts_with("unconfirmed")),
        "{:?}",
        f.flags()
    );
    assert!(
        f.flags()[12..].iter().all(Option::is_none),
        "{:?}",
        f.flags()
    );
    assert!(f.ineffective().is_empty(), "{:?}", f.ineffective());
    assert!(!f.suspended());
    let runs = f.intents();
    f.k.propose("raiser", "boost", None).unwrap();
    assert_eq!(f.intents(), runs + 1, "still delegated");
}

/// The tomato, dead at 0 after one run of boost: suspended, its one ask
/// open.
fn dead() -> Fixture {
    let mut f = tomato();
    for v in [40.0, 42.0, 41.0, 43.0, 40.0] {
        f.report(v);
    }
    f.report(20.0); // crosses: boost runs
    assert_eq!(f.intents(), 1);
    f.report(0.0); // down 20 where up 5 was expected
    assert!(f.suspended(), "{:#?}", f.entries());
    assert_eq!(f.ineffective().len(), 1);
    f
}

fn ineffective_ask(r: &Record) -> bool {
    matches!(
        r,
        Record::Ask {
            signal: Some(Signal::Ineffective { .. }),
            ..
        }
    )
}

#[test]
fn a_frozen_metric_after_an_ineffective_lever_gets_no_runs_and_no_asks() {
    let _c = clock();
    let mut f = dead();
    // Frozen at 0, crossing every hour; a human who approves whatever is
    // asked of them about running the lever.
    for _ in 0..10 {
        f.report(0.0);
        f.approve_lever_asks();
    }
    assert_eq!(f.intents(), 1, "no runs while suspended");
    assert!(f.lever_asks().is_empty(), "no per-run asks");
    assert_eq!(f.ineffective().len(), 1);
    // Refused, and the refusal is a record.
    assert!(f.entries().iter().any(|e| matches!(&e.record,
        Record::Refusal { action, reason, .. } if action == "boost" && reason.contains("NeedsDecision"))));
    // The human restores it: one trial run, which misses, and the lever
    // is suspended again with no new ask, for nothing new was measured.
    f.approve_open(ineffective_ask);
    assert!(!f.suspended());
    f.report(0.0);
    assert_eq!(f.intents(), 2, "the trial run");
    for _ in 0..10 {
        f.report(0.0);
        f.approve_lever_asks();
    }
    assert!(f.suspended(), "suspended again");
    assert_eq!(f.intents(), 2, "no runs on a frozen metric");
    assert!(f.lever_asks().is_empty());
    assert_eq!(f.ineffective().len(), 1, "no ask without new evidence");
    // A different reading is new evidence: one new ask, still no run.
    f.report(10.0);
    f.report(10.0);
    assert_eq!(f.ineffective().len(), 2, "{:#?}", f.ineffective());
    assert_eq!(f.intents(), 2);
}

#[test]
fn a_denied_ineffective_lever_asks_again_only_on_new_evidence() {
    let _c = clock();
    let mut f = dead();
    let ask = f.ineffective()[0].seq;
    f.k.deny(ask, "no").unwrap();
    for _ in 0..10 {
        f.report(0.0);
    }
    assert_eq!(f.ineffective().len(), 1, "frozen: nothing new to ask on");
    assert_eq!(f.intents(), 1);
    f.report(10.0);
    assert_eq!(f.ineffective().len(), 2, "a new reading asks once");
    f.report(11.0);
    assert_eq!(f.ineffective().len(), 2, "one open at a time");
    assert_eq!(f.intents(), 1);
}

#[test]
fn a_reading_stuck_after_a_run_is_no_effect_and_never_lets_it_run_again() {
    let _c = clock();
    // The hostile quarter: the plant died after a watering and the sensor
    // froze. The stuck reading is implausible, and the evidence the lever
    // did nothing; a later plausible reading must not run it again.
    let mut f = tomato();
    for v in [40.0, 42.0, 41.0, 43.0, 40.0] {
        f.report(v);
    }
    f.report(20.0); // crosses: boost runs
    assert_eq!(f.intents(), 1);
    f.report(20.0); // exactly as before the run: stuck, and no effect
    assert!(
        f.flags()[6]
            .as_deref()
            .is_some_and(|w| w.starts_with("stuck")),
        "{:?}",
        f.flags()
    );
    assert!(f.suspended(), "{:#?}", f.entries());
    assert_eq!(f.ineffective().len(), 1);
    assert!(
        matches!(&f.ineffective()[0].record, Record::Ask { signal: Some(Signal::Ineffective { why, .. }), .. }
            if why.contains("stuck at 20")),
        "{:?}",
        f.ineffective()
    );
    // Another sensor event, plausible and different, crosses the
    // threshold: unconfirmed after the stuck reading, then confirmed.
    f.report(25.0);
    f.report_tomorrow(22.0);
    assert!(
        f.flags()[7]
            .as_deref()
            .is_some_and(|w| w.starts_with("unconfirmed")),
        "{:?}",
        f.flags()
    );
    assert_eq!(f.flags()[8], None, "{:?}", f.flags());
    assert_eq!(f.intents(), 1, "suspended: not run again");
    assert!(f.lever_asks().is_empty(), "{:?}", f.lever_asks());
    assert!(f.suspended_refusals() > 0, "the refusal is a record");
}

/// The tomato with this extra line in its mandate head.
fn tomato_with(extra: &str) -> Fixture {
    fixture_in(
        &format!(
            "controller: {{kind: threshold, below: 35, action: boost}}\ncadence: 1h\n\
             range: [0, 100]\nmax_step: 30\n{extra}"
        ),
        "{name: boost, run: echo boost >> boosts, cap: 99/day, reversible: true, \
         delegated: true, expects: up 5 within 1}",
    )
}

/// Above the threshold, then a flap: 95, rejected, and back to 34, a
/// small step from the 36 before it that crosses the threshold.
fn flap(f: &mut Fixture) {
    for v in [40.0, 42.0, 41.0, 38.0, 36.0] {
        f.report(v);
    }
    assert_eq!(f.intents(), 0);
    f.report(95.0);
    f.report(34.0);
}

#[test]
fn the_first_reading_back_from_a_flap_is_never_a_premise() {
    let _c = clock();
    let mut f = tomato();
    flap(&mut f);
    let flags = f.flags();
    assert!(
        flags[6]
            .as_deref()
            .is_some_and(|w| w.starts_with("unconfirmed")),
        "{flags:?}"
    );
    assert_eq!(f.intents(), 0, "34 after a rejected 95 waits for the next");
    // Another implausible one keeps it unconfirmed, and so on.
    f.report(95.0);
    f.report(34.0);
    assert!(
        f.flags()[7..].iter().all(Option::is_some),
        "{:?}",
        f.flags()
    );
    assert_eq!(f.intents(), 0);
}

#[test]
fn a_second_reading_that_agrees_confirms_the_first_and_acts() {
    let _c = clock();
    let mut f = tomato();
    flap(&mut f);
    assert_eq!(f.intents(), 0);
    f.report(33.5); // within max_step of 34: confirmed, a premise
    assert_eq!(f.flags()[7], None, "{:?}", f.flags());
    assert_eq!(f.intents(), 1, "the confirmed reading crosses and acts");
}

#[test]
fn an_exact_repeat_or_a_far_reading_never_confirms() {
    let _c = clock();
    let mut f = tomato();
    flap(&mut f);
    f.report(34.0); // the same exactly: a stuck sensor's, no confirmation
    assert!(f.flags()[7].is_some(), "{:?}", f.flags());
    assert_eq!(f.intents(), 0);
    // Far from the unconfirmed reading, though near the last plausible
    // one: no confirmation either.
    let mut g = tomato();
    for v in [40.0, 42.0, 41.0, 38.0, 36.0] {
        g.report(v);
    }
    g.report(f64::NAN);
    g.report(10.0); // after a missing reading: unconfirmed
    g.report(60.0); // 24 from 36, 50 from 10
    let flags = g.flags();
    assert!(
        flags[5..].iter().all(|w| w.as_deref().is_some()),
        "{flags:?}"
    );
    assert!(
        flags[7]
            .as_deref()
            .is_some_and(|w| w.starts_with("unconfirmed")),
        "{flags:?}"
    );
    assert_eq!(g.intents(), 0);
}

#[test]
fn confirm_false_lets_the_first_reading_back_act() {
    let _c = clock();
    let mut f = tomato_with("confirm: false\n");
    flap(&mut f);
    assert_eq!(f.flags()[6], None, "{:?}", f.flags());
    assert_eq!(f.intents(), 1, "hesitating is the costlier choice here");
}

/// The declared decisions in the ledger.
fn declared(f: &Fixture) -> usize {
    f.entries()
        .iter()
        .filter(|e| {
            matches!(
                &e.record,
                Record::Decision {
                    claim: Claim::Declared,
                    ..
                }
            )
        })
        .count()
}

/// The probe's stuck_low day with the plant alive: the sensor jumps to 12,
/// a human vouches for it, boost runs on the 12 after it, and the next 12
/// is stuck at exactly the value from before the run: no effect seen.
fn stuck_after_a_vouched_run() -> Fixture {
    let mut f = tomato();
    for v in [44.0, 43.0, 44.0, 43.0, 43.4] {
        f.report(v);
    }
    f.report(12.0); // a jump: implausible, asks
    f.approve_open(implausible_ask);
    f.report(12.0); // vouched for: crosses, boost runs
    assert_eq!(f.intents(), 1, "{:#?}", f.entries());
    f.report(12.0); // stuck at the value from before the run
    assert!(f.suspended(), "{:#?}", f.entries());
    assert_eq!(f.ineffective().len(), 1);
    f
}

#[test]
fn a_no_effect_seen_only_on_a_stuck_sensor_ends_when_it_reads_again() {
    let _c = clock();
    let mut f = stuck_after_a_vouched_run();
    let decisions = declared(&f);
    // Frozen: held, and asks nothing more.
    for _ in 0..6 {
        f.report(12.0);
    }
    assert_eq!(f.intents(), 1, "held while frozen");
    assert!(f.suspended());
    // The sensor reads again: 30 is unconfirmed, 31 agrees and confirms
    // it. The plant is dry and alive; boost runs, and no human was asked.
    f.report(30.0);
    assert_eq!(f.intents(), 1, "unconfirmed: not yet");
    f.report(31.0);
    assert!(!f.suspended(), "{:#?}", f.entries());
    assert_eq!(f.intents(), 2, "runs again on a confirmed reading");
    assert_eq!(declared(&f), decisions, "without a declared approval");
    assert_eq!(f.ineffective().len(), 1, "and without another ask");
}

#[test]
fn a_dead_plant_frozen_after_a_run_never_gets_water_back_on_its_own() {
    let _c = clock();
    // The e177dd3 quarter: dead after a watering, the sensor frozen at 20.
    let mut f = tomato();
    for v in [40.0, 42.0, 41.0, 43.0, 40.0] {
        f.report(v);
    }
    f.report(20.0); // crosses: boost runs
    f.report(20.0); // stuck at the value from before the run
    assert!(f.suspended());
    let decisions = declared(&f);
    let mut hostile: Vec<f64> = vec![20.0; 10];
    // One hostile reading, plausible from 20, then repeated: stuck again.
    hostile.extend([15.0, 15.0, 15.0, 20.0, 20.0]);
    // A flap around the frozen value.
    hostile.extend([95.0, 5.0, 95.0, 5.0, 95.0, 20.0, 20.0]);
    for v in hostile {
        f.report(v);
        f.approve_lever_asks();
    }
    assert_eq!(f.intents(), 1, "{:#?}", f.flags());
    assert!(f.suspended());
    assert_eq!(declared(&f), decisions, "no human said anything");
    assert!(f.lever_asks().is_empty());
}

#[test]
fn a_relapse_seen_only_on_a_stuck_sensor_ends_when_it_reads_again() {
    let _c = clock();
    let mut f = stuck_after_a_vouched_run();
    // Restored by a human while the sensor is still frozen; a run, and the
    // next reading is stuck again: relapsed, measured, with no new ask.
    f.approve_open(ineffective_ask);
    assert!(!f.suspended());
    f.k.propose("raiser", "boost", None).unwrap();
    assert_eq!(f.intents(), 2);
    f.report(12.0);
    assert!(f.suspended(), "relapsed: {:#?}", f.entries());
    assert_eq!(f.ineffective().len(), 1);
    let decisions = declared(&f);
    f.report(30.0);
    f.report(31.0);
    assert!(!f.suspended(), "{:#?}", f.entries());
    assert_eq!(f.intents(), 3, "runs again on a confirmed reading");
    assert_eq!(declared(&f), decisions, "without a declared approval");
}

// ----- no run again while the last one's expectation waits -----

/// A boost that may run every hour and expects the count up 5 within two
/// reports.
fn eager() -> Fixture {
    fixture_in(
        "controller: {kind: threshold, below: 5, action: boost}\ncadence: 1h\n",
        "{name: boost, run: echo boost >> boosts, cap: 99/day, reversible: true, \
         delegated: true, expects: up 5 within 2}",
    )
}

/// Refusal records of the lever for waiting on its last run.
fn waiting_refusals(f: &Fixture) -> Vec<String> {
    f.entries()
        .iter()
        .filter_map(|e| match &e.record {
            Record::Refusal { action, reason, .. }
                if action == "boost" && reason.contains("Waiting(boost)") =>
            {
                Some(reason.clone())
            }
            _ => None,
        })
        .collect()
}

/// The gate's word, from the index and from the reference, which agree.
fn gate_now(f: &Fixture, cites: Vec<Seq>) -> Result<avsmd::gate::Proof, avsmd::gate::Refusal> {
    let candidate = avsmd::gate::Candidate {
        venture: "raiser".into(),
        action: "boost".into(),
        cites,
    };
    let adopted = f.k.adopted();
    let got = avsmd::gate::gate_in(
        &f.k.index().unwrap(),
        &candidate,
        &adopted,
        &Default::default(),
        f.now,
    );
    let want = avsmd::gate::reference::gate(&candidate, &f.entries(), &adopted, f.now);
    assert_eq!(got, want, "the index and the reference agree");
    got
}

/// The intents of the lever, oldest first.
fn intent_seqs(f: &Fixture) -> Vec<Seq> {
    f.entries()
        .iter()
        .filter(|e| e.record.kind() == Kind::Intent)
        .map(|e| e.seq)
        .collect()
}

#[test]
fn a_second_run_waits_until_the_first_is_judged() {
    let _c = clock();
    let mut f = eager();
    f.report(3.0); // crosses: boost runs
    let first = intent_seqs(&f);
    assert_eq!(first.len(), 1);
    f.report(4.0); // still below, one report of two since: refused, waiting
    assert_eq!(
        f.intents(),
        1,
        "not run again on one error: {:#?}",
        f.entries()
    );
    assert_eq!(waiting_refusals(&f).len(), 1, "a refusal record says why");
    match gate_now(&f, vec![]) {
        Err(r) => assert!(
            r.reasons.contains(&avsmd::gate::Reason::Waiting {
                action: "boost".into(),
                intent: first[0],
            }),
            "{r}"
        ),
        Ok(p) => panic!("allowed while waiting: {p}"),
    }
    assert!(f.lever_asks().is_empty(), "waiting asks nobody");
    f.report(4.0); // the second report: judged (the wrong size), runs again
    assert_eq!(f.intents(), 2, "{:#?}", f.entries());
}

#[test]
fn a_failed_run_is_not_waited_on() {
    let _c = clock();
    let mut f = fixture_in(
        "controller: {kind: threshold, below: 5, action: boost}\ncadence: 1h\n",
        "{name: boost, run: exit 1, cap: 99/day, reversible: true, \
         delegated: true, expects: up 5 within 2}",
    );
    f.report(3.0);
    f.report(3.0);
    assert_eq!(f.intents(), 2, "a failed run tells nothing to wait for");
    assert!(waiting_refusals(&f).is_empty());
}

#[test]
fn a_declared_approval_of_this_run_overrides_the_wait() {
    let _c = clock();
    let mut f = eager();
    f.report(3.0);
    f.report(4.0);
    assert_eq!(waiting_refusals(&f).len(), 1);
    // A human asked about this run and said yes: a declared decision may
    // loosen what code holds.
    let ask =
        f.k.append(Record::Ask {
            claim: Claim::Inferred,
            venture: "raiser".into(),
            action: Some("boost".into()),
            question: "boost again now?".into(),
            refusal: None,
            payload: None,
            signal: None,
            cites: vec![],
            packet: None,
        })
        .unwrap()
        .seq;
    let approval =
        f.k.append(Record::Decision {
            claim: Claim::Declared,
            venture: "raiser".into(),
            verdict: Verdict::Approve { ask, payload: None },
            by: None,
        })
        .unwrap()
        .seq;
    let proof = gate_now(&f, vec![approval]).expect("approved: allowed");
    assert_eq!(proof.decision(), Some(approval));
    // Not cited, the approval loosens nothing.
    assert!(gate_now(&f, vec![]).is_err());
}

// ----- the delay a lever's effect shows at -----

/// A lever that may run once a day and expects the count up 5 within one
/// report: its run has a day to show its effect.
fn daily() -> Fixture {
    daily_in("", 1)
}

/// The same, with more to the mandate's head, expecting it within `within`.
fn daily_in(head: &str, within: u32) -> Fixture {
    fixture_in(
        &format!("controller: {{kind: threshold, below: 5, action: boost}}\ncadence: 1h\n{head}"),
        &format!(
            "{{name: boost, run: echo boost >> boosts, cap: 1/day, reversible: true, \
             delegated: true, expects: up 5 within {within}}}"
        ),
    )
}

/// Restart the kernel on the same home, with this more to its config.
fn reopen(f: &mut Fixture, config: &str) {
    let home = f._tmp.path().join("home");
    std::fs::write(
        home.join("config.yaml"),
        format!(
            "nightly: never\ndefault: k\nbackends:\n  k: {{kind: command, run: echo nothing}}\n{config}"
        ),
    )
    .unwrap();
    let warden = LocalWarden::open(&home).unwrap();
    f.k = Kernel::with(&home, Box::new(FakeRole(VecDeque::new())), Box::new(warden)).unwrap();
}

/// Approve every open ask that says boost is ineffective.
fn restore(f: &mut Fixture) {
    f.approve_open(|r| {
        matches!(
            r,
            Record::Ask {
                signal: Some(Signal::Ineffective { .. }),
                ..
            }
        )
    });
    assert!(!f.suspended());
}

/// The inferred asks about a run that showed its effect late.
fn late_asks(f: &Fixture) -> Vec<Entry> {
    f.entries()
        .into_iter()
        .filter(|e| {
            matches!(
                &e.record,
                Record::Ask {
                    claim: Claim::Inferred,
                    action: None,
                    signal: None,
                    ..
                }
            )
        })
        .collect()
}

/// The latest intent's seq.
fn last_intent(f: &Fixture) -> Seq {
    f.entries()
        .iter()
        .rev()
        .find(|e| e.record.kind() == Kind::Intent)
        .unwrap()
        .seq
}

/// A run on a fall from 9 to 3, a day after the last, whose effect shows
/// on the fourth report after it: three reports late. `between` are the
/// readings in its place before that.
fn late_run(f: &mut Fixture, between: &[f64]) {
    let runs = f.intents();
    f.report_tomorrow(9.0);
    f.report(3.0);
    assert_eq!(f.intents(), runs + 1, "{:#?}", f.entries());
    for v in between {
        f.report(*v);
    }
    f.report(9.0);
}

#[test]
fn a_lever_whose_effect_shows_late_is_judged_at_its_learned_delay() {
    let _c = clock();
    let mut f = daily();
    for _ in 0..4 {
        f.report(9.0);
    }
    // Its effect shows on the fourth report after it: three reports late.
    f.report(3.0);
    assert_eq!(f.intents(), 1);
    for v in [3.0, 3.0, 3.0, 9.0] {
        f.report(v);
    }
    // Nothing on the next report: judged at the horizon it declares, with
    // no runs before to learn from, it is no effect, and a human restores it.
    assert_eq!(f.ineffective().len(), 1, "{:#?}", f.entries());
    restore(&mut f);
    // The next day the same: the first run showed its effect three
    // reports late, so this one is judged there too.
    f.report_tomorrow(9.0);
    f.report(3.0);
    assert_eq!(f.intents(), 2);
    for v in [3.0, 3.0, 3.0] {
        f.report(v);
        assert!(!f.suspended(), "{:#?}", f.entries());
    }
    f.report(9.0);
    assert!(!f.suspended(), "{:#?}", f.entries());
    assert_eq!(f.ineffective().len(), 1, "no second miss");
    assert!(late_asks(&f).is_empty(), "the delay its runs have shown");
}

#[test]
fn a_restart_never_suspends_a_run_judged_late() {
    let _c = clock();
    let mut f = daily();
    for _ in 0..4 {
        f.report(9.0);
    }
    late_run(&mut f, &[3.0, 3.0, 3.0]);
    restore(&mut f);
    late_run(&mut f, &[3.0, 3.0, 3.0]);
    assert!(
        !f.suspended(),
        "judged three reports late: {:#?}",
        f.entries()
    );
    // Three runs that show it at once.
    for _ in 0..3 {
        f.report_tomorrow(9.0);
        f.report(3.0);
        f.report(9.0);
        f.report(9.0);
    }
    assert_eq!(f.intents(), 5);
    assert!(!f.suspended());
    // A restart judges the runs since the suspension again, from the
    // ledger: the second still shows its effect late, and nothing misses.
    reopen(&mut f, "");
    f.report(9.0);
    assert!(!f.suspended(), "{:#?}", f.entries());
    assert_eq!(f.ineffective().len(), 1);
}

/// A run with its effect three reports late after a history of runs that
/// showed it at once: the sensor started lagging.
fn lagging() -> Fixture {
    let mut f = lag_after(daily());
    for v in [0.0, 0.0, 0.0] {
        f.report(v);
    }
    f
}

/// Three days of runs that show their effect at once, then a run on a
/// fall: the readings after it are the caller's.
fn lag_after(mut f: Fixture) -> Fixture {
    for _ in 0..4 {
        f.report(9.0);
    }
    for day in 0..3 {
        if day > 0 {
            f.report_tomorrow(9.0);
        }
        f.report(3.0);
        f.report(9.0); // at once
        f.report(9.0);
    }
    assert_eq!(f.intents(), 3);
    assert!(f.ineffective().is_empty(), "{:#?}", f.entries());
    // The sensor lags three reports: after the run it reads the fall that
    // came before it, the wrong way, then the run.
    f.report_tomorrow(9.0);
    f.report(6.0);
    f.report(3.0);
    assert_eq!(f.intents(), 4);
    f
}

#[test]
fn a_sensor_that_starts_lagging_asks_and_never_suspends_the_lever() {
    let _c = clock();
    let mut f = lagging();
    assert!(!f.suspended(), "waits for the window, not a miss");
    assert!(f.ineffective().is_empty());
    f.report(8.0);
    assert!(!f.suspended(), "{:#?}", f.entries());
    assert!(f.ineffective().is_empty(), "{:#?}", f.ineffective());
    let asks = late_asks(&f);
    assert_eq!(asks.len(), 1, "{:#?}", f.entries());
    match &asks[0].record {
        Record::Ask {
            cites, question, ..
        } => {
            assert_eq!(cites, &vec![last_intent(&f)]);
            assert!(question.contains("3 reports late"), "{question}");
        }
        other => panic!("{other:?}"),
    }
    // Once: the next report asks nothing more, nor does a restart that
    // judges the run again.
    f.report(8.0);
    assert_eq!(late_asks(&f).len(), 1);
    reopen(&mut f, "");
    f.report(8.0);
    assert_eq!(late_asks(&f).len(), 1, "{:#?}", f.entries());
}

#[test]
fn an_ask_about_a_late_run_keeps_to_the_asks_quota() {
    let _c = clock();
    let mut f = lagging();
    reopen(&mut f, "quotas: {asks: 1/day}\n");
    f.k.append(Record::Ask {
        claim: Claim::Inferred,
        venture: "raiser".into(),
        action: Some("boost".into()),
        question: "boost now?".into(),
        refusal: None,
        payload: None,
        signal: None,
        cites: vec![],
        packet: None,
    })
    .unwrap();
    f.report(8.0);
    assert!(!f.suspended(), "{:#?}", f.entries());
    assert!(late_asks(&f).is_empty(), "the quota is spent");
}

#[test]
fn a_late_span_over_an_implausible_reading_is_skipped() {
    let _c = clock();
    let mut f = lag_after(daily_in("range: [0, 100]\nconfirm: false\n", 1));
    // 200 is out of range: the span over it says nothing either way.
    for v in [0.0, 200.0, 0.0, 0.0] {
        f.report(v);
    }
    assert!(!f.suspended(), "{:#?}", f.entries());
    f.report(8.0);
    assert!(!f.suspended(), "{:#?}", f.entries());
    assert_eq!(late_asks(&f).len(), 1, "{:#?}", f.entries());
}

#[test]
fn a_run_its_horizon_cannot_judge_asks_nothing_late() {
    let _c = clock();
    let mut f = lag_after(daily_in("range: [0, 100]\nconfirm: false\n", 1));
    // The reading right after the run is implausible: the run is never
    // judged, so it shows no delay either.
    for v in [200.0, 0.0, 0.0, 0.0, 8.0, 8.0] {
        f.report(v);
    }
    assert!(!f.suspended(), "{:#?}", f.entries());
    assert!(late_asks(&f).is_empty(), "{:#?}", late_asks(&f));
}

#[test]
fn a_run_with_an_implausible_reading_in_its_window_teaches_no_delay() {
    let _c = clock();
    let mut f = daily_in("range: [0, 100]\nconfirm: false\n", 1);
    for _ in 0..4 {
        f.report(9.0);
    }
    // Three reports late, with a reading code doubts in its window: the
    // run is a miss at its horizon, and says nothing whole of its delay.
    late_run(&mut f, &[3.0, 200.0, 3.0, 3.0]);
    assert_eq!(f.ineffective().len(), 1, "{:#?}", f.entries());
    restore(&mut f);
    // So the next late run has nothing to be judged late on: a relapse.
    late_run(&mut f, &[3.0, 3.0, 3.0]);
    assert!(f.suspended(), "{:#?}", f.entries());
    // Adopted again, the third is judged late on the second alone.
    let dir = f.dir.clone();
    f.k.add(&dir).unwrap();
    assert!(!f.suspended());
    late_run(&mut f, &[3.0, 3.0, 3.0]);
    assert!(!f.suspended(), "{:#?}", f.entries());
}

#[test]
fn a_late_effect_is_not_drift_for_the_next_run_judged_late() {
    let _c = clock();
    // Expects up 5 within 4: a ramp of 2 a report, twenty reports late.
    let mut f = daily_in("", 4);
    for v in [9.0, 9.1, 8.9, 9.0, 9.2, 8.8, 9.0] {
        f.report(v);
    }
    let ramp = |f: &mut Fixture| {
        for i in 0..20 {
            f.report(if i % 2 == 0 { 3.1 } else { 2.9 });
        }
        for v in [5.0, 7.0, 9.0, 11.0] {
            f.report(v);
        }
    };
    f.report(3.0);
    assert_eq!(f.intents(), 1);
    ramp(&mut f);
    assert_eq!(f.ineffective().len(), 1, "{:#?}", f.entries());
    restore(&mut f);
    // The next run comes on the fall right after the ramp. Taken for drift,
    // the ramp would expect 8 of its own over a horizon: so a rise of 8 at
    // once is no effect, and so would the ramp twenty reports late be. The
    // ramp is the first run's, twenty reports late, not the metric's own.
    f.report(3.0);
    assert_eq!(f.intents(), 2, "{:#?}", f.entries());
    for v in [5.0, 7.0, 9.0, 11.0] {
        f.report(v);
    }
    for _ in 0..16 {
        f.report(11.0);
    }
    for v in [13.0, 15.0, 17.0] {
        f.report(v);
    }
    assert!(!f.suspended(), "waits for its learned delay");
    f.report(19.0);
    assert!(!f.suspended(), "{:#?}", f.entries());
    assert_eq!(f.ineffective().len(), 1);
}

#[test]
fn a_learned_delay_never_runs_a_lever() {
    let _c = clock();
    let mut f = lagging();
    f.report(8.0);
    let runs = f.intents();
    // Approving the ask about the delay runs nothing: it is inferred, and
    // names no lever.
    f.approve_open(|r| {
        matches!(
            r,
            Record::Ask {
                claim: Claim::Inferred,
                ..
            }
        )
    });
    f.report(8.0);
    f.report(8.0);
    assert_eq!(f.intents(), runs, "{:#?}", f.entries());
}

#[test]
fn a_lever_that_never_moves_the_metric_is_still_no_effect_at_once() {
    let _c = clock();
    let mut f = daily();
    for _ in 0..4 {
        f.report(9.0);
    }
    f.report(3.0);
    f.report(3.0);
    assert!(
        f.suspended(),
        "no effect waits on no delay its runs never showed"
    );
}
