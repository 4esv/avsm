//! Expectations: a delegated lever whose runs do not move the metric as
//! the mandate expects loses its delegation. The loss is measured and
//! the gift back is declared: only a declared approval of the ask that
//! says so restores it.

use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use avsm::kernel::Kernel;
use avsm::record::{Claim, Entry, Kind, Record, Seq, Signal, Verdict};
use avsm::role::{Context, InferRequest, Role};
use avsm::warden::LocalWarden;

/// The virtual clock is shared by the whole process: one test at a time.
static CLOCK: Mutex<()> = Mutex::new(());

const T0: i64 = 1_800_000_000;
const DAY: i64 = 86_400;
const HOUR: i64 = 3_600;

fn clock() -> MutexGuard<'static, ()> {
    let guard = CLOCK.lock().unwrap_or_else(|e| e.into_inner());
    avsm::clock::set(T0);
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
        format!(
            "purpose: Keep the count up.\nmetric: count\n\
             controller: {{kind: threshold, below: 5, action: boost}}\ncadence: 1h\n\
             permitted:\n- {{name: boost, run: {run}, cap: 1/day, reversible: {reversible}, {lever}}}\n"
        ),
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
        avsm::clock::set(self.now);
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
        self.k.index().unwrap().suspended("raiser", "boost")
    }
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
    // The delegation is suspended: tomorrow's crossing asks instead of
    // running.
    let runs = f.intents();
    f.report_tomorrow(3.0);
    assert_eq!(f.intents(), runs, "an ask instead of a run");
    assert_eq!(f.lever_asks().len(), 1);
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
    assert_eq!(f.lever_asks().len(), 1, "one open ask, not fifty");
    assert_eq!(f.ineffective().len(), 1);
    let asked = format!("{:?}", f.lever_asks()[0].record);
    assert!(
        asked.contains("suspended: true"),
        "the refusal says why: {asked}"
    );
    assert!(!asked.contains("is irreversible"), "{asked}");
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

#[test]
fn a_fresh_miss_after_a_restore_suspends_again() {
    let _c = clock();
    let (mut f, ask) = suspended();
    f.k.approve(ask, None).unwrap();
    f.report_tomorrow(3.0); // runs
    f.report(3.0);
    f.report(3.0); // no effect again
    assert!(f.suspended());
    assert_eq!(f.ineffective().len(), 2);
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
