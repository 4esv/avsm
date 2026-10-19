//! Forecasts: forked branches of the ledger, every lever stepping the
//! world model, worst-case governance from code. Nothing real runs, no
//! backend is called, only forecast and signal records come back, and the
//! branch homes are gone after.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use avsmd::config::{self, Quotas};
use avsmd::forecast::{self, Options, Policy, Sweep};
use avsmd::gate::{self, Candidate};
use avsmd::kernel::Kernel;
use avsmd::record::{Claim, Entry, Kind, Payload, Record, Signal};
use avsmd::role::{Context, InferRequest, Role};
use avsmd::warden::LocalWarden;

/// The virtual clock is shared by the whole process: one test at a time.
static CLOCK: Mutex<()> = Mutex::new(());

/// A Tuesday, 00:00 UTC.
const T0: i64 = 1_800_057_600;
const HOUR: i64 = 3_600;
const DAY: i64 = 86_400;

fn clock() -> MutexGuard<'static, ()> {
    let guard = CLOCK.lock().unwrap_or_else(|e| e.into_inner());
    avsmd::clock::set(T0);
    guard
}

struct NoRole;

impl Role for NoRole {
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

struct Fixture {
    tmp: tempfile::TempDir,
    k: Kernel,
    now: i64,
}

/// A home whose default, research and escalation backend is a command
/// that writes a line to `calls` each time it is asked.
fn home(tmp: &Path, extra: &str) -> PathBuf {
    let home = tmp.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let calls = tmp.join("calls");
    std::fs::write(
        home.join("config.yaml"),
        format!(
            "nightly: \"00:00\"\ndefault: rec\nresearch: rec\nescalate: rec\n\
             backends:\n  rec: {{kind: command, run: \"echo call >> {}; echo nothing\"}}\n{extra}",
            calls.display()
        ),
    )
    .unwrap();
    home
}

fn fixture(extra_config: &str) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let home = home(tmp.path(), extra_config);
    let warden = LocalWarden::open(&home).unwrap();
    let mut k = Kernel::with(&home, Box::new(NoRole), Box::new(warden)).unwrap();
    k.adopt_self().unwrap();
    Fixture { tmp, k, now: T0 }
}

impl Fixture {
    /// A venture whose metric is read from its `value` file, with the
    /// given mandate body after the purpose and metric.
    fn venture(&mut self, name: &str, metric: &str, body: &str) -> PathBuf {
        let dir = self.tmp.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("mandate.yaml"),
            format!("purpose: Grow.\nmetric: {metric}\n{body}"),
        )
        .unwrap();
        script(&dir, "report", "echo read >> reads; cat value");
        std::fs::write(dir.join("value"), "100\n").unwrap();
        self.k.add(&dir).unwrap();
        dir
    }

    /// Report `values` for a venture, one every `gap` seconds.
    fn history(&mut self, name: &str, values: &[f64], gap: i64) {
        let dir = self.tmp.path().join(name);
        for v in values {
            self.now += gap;
            avsmd::clock::set(self.now);
            std::fs::write(dir.join("value"), format!("{v}\n")).unwrap();
            self.k.report(name, false);
        }
    }

    fn entries(&self) -> Vec<Entry> {
        self.k.entries().unwrap()
    }

    fn opts(&self, ventures: &[&str], days: u32, seeds: u32) -> Options {
        let mut o = Options::new(
            ventures.iter().map(|s| s.to_string()).collect(),
            config::Forecast { days, seeds },
        );
        o.scratch = Some(self.scratch());
        o
    }

    fn scratch(&self) -> PathBuf {
        let s = self.tmp.path().join("scratch");
        std::fs::create_dir_all(&s).unwrap();
        s
    }
}

/// A garden whose water level falls an hour at a time, with a delegated
/// script lever that would write to `invoked` if it ever ran for real.
const GARDEN: &str = "controller: {kind: threshold, below: 50, action: water}\ncadence: 1h\n\
     permitted:\n- {name: water, run: echo ran >> invoked, cap: 4/day, reversible: true, delegated: true, expects: up 5 within 1}\n";

/// A shop run by a model controller on the recording backend, permitted
/// research and a script lever that would write to `invoked`.
const SHOP: &str = "controller: {kind: model}\ncadence: 1h\n\
     permitted:\n- {name: research, cap: 5/day, reversible: true, delegated: true}\n\
     - {name: boost, run: echo ran >> invoked, cap: 9/day, reversible: true}\n";

fn falling(from: f64, n: usize, by: f64) -> Vec<f64> {
    (1..=n).map(|i| from - by * i as f64).collect()
}

#[test]
fn a_forecast_runs_nothing_real_calls_no_backend_and_leaves_only_its_records() {
    let _c = clock();
    let mut f = fixture("");
    let garden = f.venture("garden", "level", GARDEN);
    let shop = f.venture("shop", "count", SHOP);
    f.history("garden", &falling(100.0, 12, 1.0), HOUR);
    f.history("shop", &[10.0, 11.0, 12.0, 13.0], HOUR);
    let before = f.entries();
    assert!(!garden.join("invoked").exists() && !shop.join("invoked").exists());

    let opts = f.opts(&["garden", "shop"], 3, 4);
    let (outcome, written) = f.k.forecast(&opts, &[]).unwrap();

    // Not one real lever ran, not one backend was called.
    assert!(
        !garden.join("invoked").exists(),
        "a script lever ran for real"
    );
    assert!(
        !shop.join("invoked").exists(),
        "a script lever ran for real"
    );
    assert!(!f.tmp.path().join("calls").exists(), "a backend was called");
    // The levers did run, in the branches: the garden crossed its bound.
    let all = outcome
        .summaries
        .iter()
        .find(|s| s.venture == "garden" && s.policy == Policy::All)
        .unwrap();
    assert!(all.reached.delegations.p50 > 0.0, "{all:?}");

    // The ledger before is untouched, and after it only forecast records
    // and signal asks.
    let after = f.entries();
    assert_eq!(&after[..before.len()], &before[..]);
    assert_eq!(after.len(), before.len() + written.len());
    for e in &after[before.len()..] {
        let ok = e.record.kind() == Kind::Forecast
            || matches!(
                &e.record,
                Record::Ask {
                    claim: Claim::Measured,
                    signal: Some(Signal::Forecast { .. } | Signal::Capacity { .. }),
                    ..
                }
            );
        assert!(ok, "{}", e.line());
    }
    f.k.ledger.verify().unwrap();

    // One forecast record per venture and policy, with the decided fields.
    let forecasts: Vec<&Entry> = written
        .iter()
        .filter(|e| e.record.kind() == Kind::Forecast)
        .collect();
    assert_eq!(forecasts.len(), 6);
    for venture in ["garden", "shop"] {
        let policies: Vec<String> = forecasts
            .iter()
            .filter_map(|e| match &e.record {
                Record::Forecast {
                    claim,
                    venture: v,
                    policy,
                    horizon,
                    seeds,
                    metrics,
                    reached,
                    variant,
                    ..
                } if v == venture => {
                    assert_eq!(*claim, Claim::Inferred);
                    assert_eq!((*horizon, *seeds, variant.clone()), (3, 4, None));
                    assert!(metrics[0].quantiles.valid(), "{metrics:?}");
                    assert!(reached.asks.valid() && reached.spend.valid());
                    assert!(reached.refusals.valid() && reached.halts.valid());
                    assert!(reached.delegations.valid());
                    Some(policy.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(policies, ["all", "none", "mixed"], "{venture}");
    }
    let garden_metric = forecasts
        .iter()
        .find_map(|e| match &e.record {
            Record::Forecast {
                venture, metrics, ..
            } if venture == "garden" => Some(metrics[0].name.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(garden_metric, "level");

    // The branch homes are gone.
    let left: Vec<_> = std::fs::read_dir(f.scratch()).unwrap().collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn a_plan_venture_is_forecast_through_its_planner_with_no_backend() {
    let _c = clock();
    let mut f = fixture("");
    let dir = f.venture(
        "shop",
        "count",
        "controller: {kind: plan, horizon: 2, depth: 1, members: 4}\ncadence: 1h\n\
         permitted:\n- {name: boost, run: echo ran >> invoked, cap: 9/day, reversible: true, delegated: true}\n",
    );
    f.history("shop", &[10.0, 9.0, 8.0, 7.0, 6.0, 5.0], HOUR);
    // The real kernel's planner may ask the backend on a tie, and its
    // lever may run; the forecast's never.
    let calls = |f: &Fixture| {
        std::fs::read_to_string(f.tmp.path().join("calls")).map_or(0, |t| t.lines().count())
    };
    let runs = || std::fs::read_to_string(dir.join("invoked")).map_or(0, |t| t.lines().count());
    let (calls_before, runs_before) = (calls(&f), runs());
    let opts = f.opts(&["shop"], 2, 3);
    let (outcome, _) = f.k.forecast(&opts, &[]).unwrap();
    assert_eq!(outcome.summaries.len(), 3);
    assert_eq!(runs(), runs_before, "a script lever ran for real");
    assert_eq!(calls(&f), calls_before, "a backend was called");
}

#[test]
fn a_forecast_is_deterministic_under_its_seeds() {
    let _c = clock();
    let mut f = fixture("");
    f.venture("garden", "level", GARDEN);
    f.history(
        "garden",
        &[100.0, 98.0, 99.0, 96.0, 97.0, 94.0, 95.0, 92.0],
        HOUR,
    );
    let opts = f.opts(&["garden"], 2, 4);
    let one = forecast::run(&f.k.ledger, &f.k.home, &opts).unwrap();
    let two = forecast::run(&f.k.ledger, &f.k.home, &opts).unwrap();
    assert_eq!(one.records(&[]), two.records(&[]));
    // Another base seed is another forecast.
    let mut other = opts.clone();
    other.seed ^= 1;
    let three = forecast::run(&f.k.ledger, &f.k.home, &other).unwrap();
    assert_ne!(one.records(&[]), three.records(&[]));
}

#[test]
fn a_forecast_from_the_kernels_index_is_the_forecast_from_the_ledger() {
    let _c = clock();
    let mut f = fixture("");
    f.venture("garden", "level", GARDEN);
    f.history(
        "garden",
        &[100.0, 98.0, 99.0, 96.0, 97.0, 94.0, 95.0, 92.0],
        HOUR,
    );
    let opts = f.opts(&["garden"], 2, 4);
    let read = forecast::run(&f.k.ledger, &f.k.home, &opts).unwrap();
    let seeded = {
        let index = f.k.index().unwrap();
        forecast::run_in(&index, &f.k.ledger, &f.k.home, &opts).unwrap()
    };
    assert_eq!(read.records(&[]), seeded.records(&[]));
}

#[test]
fn a_forecast_that_hits_a_guard_asks_with_the_envelope() {
    let _c = clock();
    let mut f = fixture("");
    f.venture(
        "fund",
        "equity",
        "controller: {kind: threshold, below: 1, action: rebalance}\ncadence: 1h\n\
         permitted:\n- {name: rebalance, run: echo ran >> invoked, cap: 1/day, reversible: true}\n\
         guards:\n- {name: dd, drawdown: 10%, of: equity, then: ask}\n",
    );
    // Falling a point an hour from 100: 4% down, with no lever between.
    f.history("fund", &falling(100.0, 4, 1.0), HOUR);
    let opts = f.opts(&["fund"], 2, 4);
    let (outcome, written) = f.k.forecast(&opts, &[]).unwrap();
    let wall = outcome
        .walls
        .iter()
        .find(|w| w.kind == "guard")
        .unwrap_or_else(|| panic!("{:?}", outcome.walls));
    assert_eq!(wall.metric, "guard.dd");
    let asks: Vec<&Entry> = written
        .iter()
        .filter(|e| {
            matches!(
                &e.record,
                Record::Ask {
                    signal: Some(Signal::Forecast { .. }),
                    ..
                }
            )
        })
        .collect();
    assert_eq!(asks.len(), 1);
    let Record::Ask {
        claim,
        question,
        signal:
            Some(Signal::Forecast {
                wall,
                metric,
                envelope,
                days,
            }),
        cites,
        ..
    } = &asks[0].record
    else {
        unreachable!()
    };
    assert_eq!(*claim, Claim::Measured);
    assert_eq!((wall.as_str(), metric.as_str()), ("guard", "guard.dd"));
    assert!(envelope.valid() && envelope.p95 >= 10.0, "{envelope:?}");
    assert_eq!(days.len(), 3);
    assert!(days.iter().any(|d| d.day.is_some()), "{days:?}");
    assert!(question.contains("envelope   guard.dd"), "{question}");
    assert!(question.contains("first hit"), "{question}");
    // It cites the venture's forecast records, all three.
    assert_eq!(cites.len(), 3);
    for c in cites {
        assert!(written
            .iter()
            .any(|e| e.seq == *c && e.record.kind() == Kind::Forecast));
    }
    // One open per venture: a second forecast does not ask again.
    let (_, again) = f.k.forecast(&opts, &[]).unwrap();
    assert!(again.iter().all(|e| e.record.kind() == Kind::Forecast));
}

#[test]
fn a_forecast_record_cited_alone_never_authorizes() {
    let _c = clock();
    let mut f = fixture("");
    f.venture("garden", "level", GARDEN);
    f.history("garden", &falling(100.0, 6, 1.0), HOUR);
    let opts = f.opts(&["garden"], 1, 2);
    let (_, written) = f.k.forecast(&opts, &[]).unwrap();
    let index = f.k.index().unwrap();
    for e in written.iter().filter(|e| e.record.kind() == Kind::Forecast) {
        let gated = gate::gate_in(
            &index,
            &Candidate {
                venture: "garden".to_string(),
                action: "water".to_string(),
                cites: vec![e.seq],
            },
            &f.k.adopted(),
            &Quotas::default(),
            f.now,
        );
        assert!(gated.is_err(), "a forecast authorized water: {gated:?}");
    }
}

#[test]
fn caps_that_cannot_keep_up_get_a_capacity_ask_before_any_saturation() {
    let _c = clock();
    let mut f = fixture("");
    f.venture(
        "inbox",
        "unread",
        "controller: {kind: threshold, above: 500, action: reply}\ncadence: 1h\n\
         permitted:\n- {name: reply, run: echo ran >> invoked, cap: 5/day, reversible: true, delegated: true, expects: down 1 within 1}\n",
    );
    // Twenty more unread a day, far below the bound: nothing has fired.
    f.history("inbox", &[100.0, 120.0, 140.0, 160.0, 180.0], DAY);
    // Past the nightly time, before the next report is due.
    f.now += HOUR / 2;
    avsmd::clock::set(f.now);
    f.k.tick();
    let entries = f.entries();
    let capacity: Vec<&Entry> = entries
        .iter()
        .filter(|e| {
            matches!(
                &e.record,
                Record::Ask {
                    signal: Some(Signal::Capacity { .. }),
                    ..
                }
            )
        })
        .collect();
    assert_eq!(capacity.len(), 1, "{entries:#?}");
    let Record::Ask {
        question,
        claim,
        signal:
            Some(Signal::Capacity {
                metric,
                drift,
                most,
            }),
        ..
    } = &capacity[0].record
    else {
        unreachable!()
    };
    assert_eq!(*claim, Claim::Measured);
    assert_eq!((metric.as_str(), *drift, *most), ("unread", 20.0, 5.0));
    assert!(
        question.contains("unread grows 20/day; reply can remove at most 5/day: the cap"),
        "{question}"
    );
    assert!(!entries.iter().any(|e| matches!(
        &e.record,
        Record::Ask {
            signal: Some(Signal::Saturated { .. }),
            ..
        }
    )));
    // Once per episode: the next night says nothing more.
    f.history("inbox", &[200.0], DAY);
    f.k.tick();
    let again = f
        .entries()
        .iter()
        .filter(|e| {
            matches!(
                &e.record,
                Record::Ask {
                    signal: Some(Signal::Capacity { .. }),
                    ..
                }
            )
        })
        .count();
    assert_eq!(again, 1);
}

#[test]
fn with_no_direction_declared_capacity_is_not_checked() {
    let _c = clock();
    let mut f = fixture("");
    // The same inbox, losing ground at seven a day, but nothing in the
    // mandate says which way is better: reply's expects moves unread down,
    // and that is not a declaration.
    let body = "controller: {kind: model}\ncadence: 1d\n\
         permitted:\n- {name: reply, run: echo ran >> invoked, cap: 20/day, reversible: true, delegated: true, expects: down 1 within 1}\n";
    let dir = f.tmp.path().join("inbox");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mandate.yaml"),
        format!("purpose: Grow.\nmetric: unread\n{body}"),
    )
    .unwrap();
    script(&dir, "report", "cat value");
    std::fs::write(dir.join("value"), "3\n").unwrap();
    let added = f.k.add(&dir).unwrap();
    assert!(
        added.contains("no direction declared; capacity is not checked"),
        "{added}"
    );
    f.history("inbox", &[3.0, 10.0, 17.0, 24.0, 31.0, 38.0], DAY);
    let m = &f.k.ventures["inbox"].mandate;
    assert!(forecast::capacity(&f.k.index().unwrap(), "inbox", m).is_none());
    f.now += HOUR;
    avsmd::clock::set(f.now);
    f.k.tick();
    assert!(!f.entries().iter().any(|e| matches!(
        &e.record,
        Record::Ask {
            signal: Some(Signal::Capacity { .. }),
            ..
        }
    )));

    // Declared, the same history asks, and teachback says so.
    let text = std::fs::read_to_string(dir.join("mandate.yaml")).unwrap();
    std::fs::write(
        dir.join("mandate.yaml"),
        text.replace("cadence: 1d\n", "cadence: 1d\nbetter: down\n"),
    )
    .unwrap();
    let added = f.k.add(&dir).unwrap();
    assert!(
        added.contains("better: unread going down is the target improving"),
        "{added}"
    );
    assert!(f
        .entries()
        .iter()
        .any(|e| matches!(&e.record, Record::Ask { signal: Some(Signal::Capacity { metric, .. }), .. } if metric == "unread")));
}

#[test]
fn a_declared_direction_with_no_known_lever_effect_asks_nothing() {
    let _c = clock();
    let mut f = fixture("");
    // Unread grows and better is down, but reply has no measured runs
    // and declares no expects: there is nothing to weigh the drift
    // against, so no ask.
    f.venture(
        "inbox",
        "unread",
        "controller: {kind: model}\ncadence: 1d\nbetter: down\n\
         permitted:\n- {name: reply, run: echo ran >> invoked, cap: 5/day, reversible: true}\n",
    );
    f.history("inbox", &[3.0, 10.0, 17.0, 24.0], DAY);
    let m = &f.k.ventures["inbox"].mandate;
    assert!(forecast::capacity(&f.k.index().unwrap(), "inbox", m).is_none());
}

#[test]
fn the_mandates_better_is_the_default_for_a_plan_and_an_audit() {
    use avsmd::mandate::{Controller, Mandate, Worse};
    let m = Mandate::parse(
        "purpose: p\nmetric: unread\nbetter: down\n\
         controller: {kind: plan}\ncadence: 1h\n\
         permitted:\n- {name: reply, run: ./reply, cap: 5/day, reversible: true}\n\
         audits:\n- {name: a, run: ./a, worse: up}\n- {name: b, run: ./b, worse: up, better: up}\n",
    )
    .unwrap();
    assert!(matches!(
        m.controller,
        Controller::Plan {
            better: Worse::Down,
            ..
        }
    ));
    assert_eq!(m.audits[0].better, Worse::Down);
    assert_eq!(m.audits[1].better, Worse::Up, "its own field still wins");
    let own = Mandate::parse(
        "purpose: p\nmetric: unread\nbetter: down\n\
         controller: {kind: plan, better: up}\ncadence: 1h\n\
         permitted:\n- {name: reply, run: ./reply, cap: 5/day, reversible: true}\n",
    )
    .unwrap();
    assert!(matches!(
        own.controller,
        Controller::Plan {
            better: Worse::Up,
            ..
        }
    ));
    assert!(Mandate::parse(
        "purpose: p\nmetric: unread\nbetter: left\ncontroller: {kind: model}\ncadence: 1h\n"
    )
    .unwrap_err()
    .0
    .contains("better is up or down"));
}

#[test]
fn a_controller_asked_once_a_night_counts_once_a_day_against_the_drift() {
    let _c = clock();
    let mut f = fixture("");
    // Seven more unread a day; the cap allows twenty replies a day, but
    // the model is asked once a night for at most five choices.
    f.venture(
        "inbox",
        "unread",
        "controller: {kind: model}\ncadence: 1d\nbetter: down\n\
         permitted:\n- {name: reply, run: echo ran >> invoked, cap: 20/day, reversible: true, delegated: true, expects: down 1 within 1}\n",
    );
    f.history("inbox", &[3.0, 10.0, 17.0, 24.0], DAY);
    let gap = forecast::capacity(
        &f.k.index().unwrap(),
        "inbox",
        &f.k.ventures["inbox"].mandate,
    )
    .expect("five a day cannot keep up with seven");
    assert_eq!((gap.drift, gap.most), (7.0, 5.0));
    assert_eq!(
        gap.describe(),
        "unread grows 7/day; reply can remove at most 5/day: the model is asked once a night, for at most 5 choices"
    );
    // A drift the turns keep up with asks nothing.
    f.history("inbox", &[24.0; 8], DAY);
    assert!(forecast::capacity(
        &f.k.index().unwrap(),
        "inbox",
        &f.k.ventures["inbox"].mandate
    )
    .is_none());
}

#[test]
fn a_sweep_reports_its_trial_count_and_a_pbo() {
    let _c = clock();
    let mut f = fixture("");
    f.venture("garden", "level", GARDEN);
    f.history("garden", &falling(100.0, 8, 2.0), HOUR);
    let mut opts = f.opts(&["garden"], 2, 4);
    opts.sweep = Some(Sweep {
        field: "permitted.water.cap".to_string(),
        values: vec!["2/day".into(), "4/day".into(), "8/day".into()],
    });
    let (outcome, written) = f.k.forecast(&opts, &[]).unwrap();
    assert_eq!(outcome.trials, 3);
    assert_eq!(outcome.pbo.len(), 3);
    for (_, p) in &outcome.pbo {
        let p = p.as_ref().expect("four seeds split into groups");
        assert!((0.0..=1.0).contains(&p.pbo), "{p:?}");
        assert_eq!(p.splits, 6);
    }
    // One forecast per value and policy, each naming its value.
    let variants: Vec<String> = written
        .iter()
        .filter_map(|e| match &e.record {
            Record::Forecast { variant, .. } => variant.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(variants.len(), 9);
    assert_eq!(
        variants
            .iter()
            .filter(|v| *v == "permitted.water.cap=8/day")
            .count(),
        3
    );
    let text = outcome.render();
    assert!(
        text.contains("sweep of permitted.water.cap: 3 trials"),
        "{text}"
    );
    assert!(text.contains("PBO "), "{text}");
    // And over the socket's words.
    let out =
        f.k.handle(&[
            "forecast".to_string(),
            "garden".to_string(),
            "--days".to_string(),
            "1".to_string(),
            "--seeds".to_string(),
            "2".to_string(),
            "--sweep".to_string(),
            "permitted.water.cap".to_string(),
            "2/day,4/day,8/day".to_string(),
        ])
        .unwrap();
    assert!(out.contains("3 trials"), "{out}");
}

/// Simulate a day of the garden in a fresh home with this config, and
/// return its ledger.
fn simulate_garden(config: &str) -> Vec<Entry> {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.yaml"), config).unwrap();
    let dir = tmp.path().join("garden");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mandate.yaml"),
        format!("purpose: Grow.\nmetric: level\n{GARDEN}"),
    )
    .unwrap();
    script(&dir, "report", "echo 80");
    let plan = avsmd::simulate::Plan {
        days: 1,
        step: 6 * HOUR,
        human: avsmd::simulate::Human::ApproveAll,
        start: Some(T0),
        add: vec![dir],
        world: None,
    };
    let mut out = Vec::new();
    avsmd::simulate::run(&home, &plan, &mut out).unwrap();
    Kernel::open(&home).unwrap().entries().unwrap()
}

#[test]
fn a_simulation_forecasts_at_add_as_its_config_says() {
    let _c = clock();
    let entries = simulate_garden("nightly: never\nforecast: {days: 10, seeds: 4}\n");
    let adopt = entries
        .iter()
        .find(|e| e.record.venture() == "garden" && e.record.kind() == Kind::Decision)
        .unwrap()
        .seq;
    let first = entries
        .iter()
        .find(|e| matches!(&e.record, Record::Report { venture, metric, .. } if venture == "garden" && metric == "level"))
        .unwrap()
        .seq;
    let forecasts: Vec<(String, u32, u32, Vec<u64>)> = entries
        .iter()
        .filter_map(|e| match &e.record {
            Record::Forecast {
                policy,
                horizon,
                seeds,
                cites,
                ..
            } => Some((policy.clone(), *horizon, *seeds, cites.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        forecasts,
        ["all", "none", "mixed"].map(|p| (p.to_string(), 10, 4, vec![adopt, first]))
    );
    // Off only with seeds 0.
    let entries = simulate_garden("nightly: never\nforecast: {days: 10, seeds: 0}\n");
    assert!(!entries.iter().any(|e| e.record.kind() == Kind::Forecast));
}

#[test]
fn with_no_forecast_key_a_simulation_forecasts_with_the_defaults() {
    let _c = clock();
    let none = avsmd::config::Config::parse("nightly: never\n").unwrap();
    assert_eq!(
        none.automatic_forecast(),
        Some(config::Forecast {
            days: 30,
            seeds: 16
        })
    );
    let off = avsmd::config::Config::parse("forecast: {seeds: 0}\n").unwrap();
    assert_eq!(off.automatic_forecast(), None);
    let entries = simulate_garden("nightly: never\n");
    let forecasts: Vec<(String, u32, u32)> = entries
        .iter()
        .filter_map(|e| match &e.record {
            Record::Forecast {
                policy,
                horizon,
                seeds,
                ..
            } => Some((policy.clone(), *horizon, *seeds)),
            _ => None,
        })
        .collect();
    assert_eq!(
        forecasts,
        ["all", "none", "mixed"].map(|p| (p.to_string(), 30, 16))
    );
}

#[test]
fn a_capacity_gap_that_comes_and_goes_asks_once_a_quiet_week_closes_it() {
    let _c = clock();
    let mut f = fixture("");
    f.venture(
        "inbox",
        "unread",
        "controller: {kind: threshold, above: 900, action: reply}\ncadence: 1h\n\
         permitted:\n- {name: reply, run: echo ran >> invoked, cap: 5/day, reversible: true, delegated: true, expects: down 1 within 1}\n",
    );
    // The mean drift a day is (x_n - 100) / n: 7 on odd days, past the 5
    // replies can remove, and 3 on even days, within it.
    let asks = |f: &Fixture| {
        f.entries()
            .iter()
            .filter(|e| {
                matches!(
                    &e.record,
                    Record::Ask {
                        signal: Some(Signal::Capacity { .. }),
                        ..
                    }
                )
            })
            .count()
    };
    // Each night: the report, the nightly check, and a human who answers
    // every ask at once, so no open ask holds a repeat back.
    let night = |f: &mut Fixture, n: i64, per_day: i64| {
        f.history("inbox", &[(100 + per_day * n) as f64], DAY);
        f.now += HOUR / 2;
        avsmd::clock::set(f.now);
        f.k.tick();
        for q in f.k.open_asks().unwrap() {
            f.k.deny(q, "seen").unwrap();
        }
        f.now -= HOUR / 2;
    };
    f.history("inbox", &[100.0], DAY);
    for n in 1..=14 {
        night(&mut f, n, if n % 2 == 1 { 7 } else { 3 });
    }
    assert_eq!(asks(&f), 1, "a gap that flips daily asks once");
    // Seven clean nights in a row close the episode, and the next gap
    // asks again.
    for n in 15..=21 {
        night(&mut f, n, 3);
    }
    assert_eq!(asks(&f), 1);
    night(&mut f, 22, 7);
    assert_eq!(asks(&f), 2, "a new episode after a quiet week asks");
}

#[test]
fn a_branch_never_forecasts() {
    let _c = clock();
    // The branches get this config, forecasts on; the worst-case role
    // proposes the delegated forecast lever every night in every branch.
    let mut f = fixture("forecast: {days: 1, seeds: 2}\n");
    f.venture(
        "blog",
        "posts",
        "controller: {kind: model}\ncadence: 6h\n\
         permitted:\n- {name: forecast, cap: 3/day, reversible: true, delegated: true}\n",
    );
    f.history("blog", &[1.0, 2.0, 3.0], 6 * HOUR);
    let opts = f.opts(&["blog"], 3, 2);
    let (outcome, _) = f.k.forecast(&opts, &[]).unwrap();
    assert!(outcome.tried > 0, "the lever was never tried in a branch");
    assert_eq!(outcome.nested, 0, "a branch wrote a forecast");
}

/// A role that proposes posting, with a payload, every night.
struct Poster;

impl Role for Poster {
    fn decide(
        &mut self,
        ctx: &Context,
        _infer: &mut dyn FnMut(&InferRequest) -> Result<String, String>,
        _drill: &mut dyn FnMut(&str, &str) -> Result<String, String>,
    ) -> Result<Vec<Record>, String> {
        Ok(vec![Record::Proposal {
            cites: vec![],
            claim: Claim::Inferred,
            venture: ctx.venture.clone(),
            action: "post".into(),
            reason: "news".into(),
            payload: Some(Payload::Text {
                text: format!("hello {}", ctx.now),
            }),
        }])
    }
}

const BLOG: &str = "controller: {kind: model}\ncadence: 1h\n\
     permitted:\n- {name: post, run: echo ran >> invoked, cap: 9/day, reversible: true, takes: text}\n\
     - {name: forecast, cap: 3/day, reversible: true, delegated: true}\n";

/// A role that proposes posting each of these texts, in one night.
struct Posts(Vec<&'static str>);

impl Role for Posts {
    fn decide(
        &mut self,
        ctx: &Context,
        _infer: &mut dyn FnMut(&InferRequest) -> Result<String, String>,
        _drill: &mut dyn FnMut(&str, &str) -> Result<String, String>,
    ) -> Result<Vec<Record>, String> {
        Ok(self
            .0
            .iter()
            .map(|t| Record::Proposal {
                cites: vec![],
                claim: Claim::Inferred,
                venture: ctx.venture.clone(),
                action: "post".into(),
                reason: "news".into(),
                payload: Some(Payload::Text {
                    text: t.to_string(),
                }),
            })
            .collect())
    }
}

/// The open asks of `post` after one night of these proposals.
fn post_asks(texts: Vec<&'static str>) -> usize {
    let tmp = tempfile::tempdir().unwrap();
    let home = home(tmp.path(), "");
    let warden = LocalWarden::open(&home).unwrap();
    let k = Kernel::with(&home, Box::new(Posts(texts)), Box::new(warden)).unwrap();
    let mut f = Fixture { tmp, k, now: T0 };
    f.venture("blog", "posts", BLOG);
    f.k.cycle("blog");
    let index = f.k.index().unwrap();
    index
        .open_asks()
        .into_iter()
        .filter(|q| {
            index
                .get(*q)
                .is_some_and(|e| e.record.action() == Some("post"))
        })
        .count()
}

#[test]
fn asks_are_one_per_lever_and_payload() {
    let _c = clock();
    assert_eq!(post_asks(vec!["a", "b", "c", "d", "e"]), 5);
    assert_eq!(post_asks(vec!["same", "same", "same"]), 1);
}

#[test]
fn a_lever_that_asks_each_time_runs_no_more_than_the_asks_quota() {
    let _c = clock();
    let mut f = fixture("");
    // Seven a day; reply asks each time (it takes a payload and is not
    // delegated): twenty a day by its cap, five by the model's choices,
    // two by an asks quota of 2/day.
    f.venture(
        "inbox",
        "unread",
        "controller: {kind: model}\ncadence: 1d\nbetter: down\n\
         permitted:\n- {name: reply, run: echo ran >> invoked, cap: 20/day, reversible: true, takes: text, expects: down 1 within 1}\n",
    );
    f.history("inbox", &[3.0, 10.0, 17.0, 24.0], DAY);
    let m = &f.k.ventures["inbox"].mandate;
    let index = f.k.index().unwrap();
    let gap = forecast::capacity_with(&index, "inbox", m, Some(2.0)).unwrap();
    assert_eq!((gap.drift, gap.most), (7.0, 2.0));
    assert!(
        gap.describe()
            .contains("the asks quota, shared by every venture, at most"),
        "{}",
        gap.describe()
    );
    // Without a quota the choices bind.
    assert_eq!(forecast::capacity(&index, "inbox", m).unwrap().most, 5.0);
}

#[test]
fn forecasts_at_add_and_as_a_lever() {
    let _c = clock();
    let mut f = fixture("forecast: {days: 1, seeds: 2}\n");
    f.k.forecasts = true;
    // At add: nothing yet, and the report program never runs for it.
    let dir = f.venture("blog", "posts", BLOG);
    assert!(
        !dir.join("reads").exists(),
        "a forecast ran the report program"
    );
    assert!(!f
        .entries()
        .iter()
        .any(|e| e.record.kind() == Kind::Forecast));
    // On the first trusted reading: the forecast of the add, citing the
    // adoption and that report, its metric's quantiles filled.
    f.history("blog", &[1.0], HOUR);
    let entries = f.entries();
    let adopt = entries
        .iter()
        .rfind(|e| e.record.venture() == "blog" && e.record.kind() == Kind::Decision)
        .unwrap()
        .seq;
    let report = entries
        .iter()
        .find(|e| matches!(&e.record, Record::Report { venture, metric, .. } if venture == "blog" && metric == "posts"))
        .unwrap()
        .seq;
    let at_add: Vec<&Entry> = entries
        .iter()
        .filter(|e| e.record.kind() == Kind::Forecast)
        .collect();
    assert_eq!(at_add.len(), 3);
    for e in &at_add {
        assert_eq!(e.record.cites(), [adopt, report]);
        let Record::Forecast { metrics, .. } = &e.record else {
            unreachable!()
        };
        assert_eq!(metrics[0].name, "posts");
        assert!(metrics[0].quantiles.valid(), "{metrics:?}");
    }
    // Only the first reading: the next two add none.
    f.history("blog", &[2.0, 3.0], HOUR);
    assert_eq!(
        f.entries()
            .iter()
            .filter(|e| e.record.kind() == Kind::Forecast)
            .count(),
        3
    );

    // As a lever: delegated, so a proposal runs it, recorded then done,
    // and its forecast records cite the intent.
    let lines = f.k.propose("blog", "forecast", None).unwrap();
    let entries = f.entries();
    let intent = entries
        .iter()
        .rfind(|e| matches!(&e.record, Record::Intent { action, .. } if action == "forecast"))
        .unwrap_or_else(|| panic!("{lines}"));
    let outcome = entries
        .iter()
        .find(|e| matches!(&e.record, Record::Outcome { intent: i, ok: true, .. } if *i == intent.seq))
        .unwrap_or_else(|| panic!("{lines}"));
    let by_lever: Vec<&Entry> = entries
        .iter()
        .filter(|e| e.record.kind() == Kind::Forecast && e.record.cites() == [intent.seq])
        .collect();
    assert_eq!(by_lever.len(), 3, "{lines}");
    assert!(by_lever.iter().all(|e| e.seq > outcome.seq));
    assert!(!dir.join("invoked").exists(), "a forecast ran a lever");
    assert!(!f.tmp.path().join("calls").exists(), "a backend was called");
}

#[test]
fn a_ratchet_delegate_ask_cites_the_forecast_of_the_proposed_mandate() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let home = home(tmp.path(), "ratchet: 2\nforecast: {days: 1, seeds: 2}\n");
    let warden = LocalWarden::open(&home).unwrap();
    let mut k = Kernel::with(&home, Box::new(Poster), Box::new(warden)).unwrap();
    k.adopt_self().unwrap();
    k.forecasts = true;
    let mut f = Fixture { tmp, k, now: T0 };
    let dir = f.venture("blog", "posts", BLOG);
    f.history("blog", &[1.0, 2.0, 3.0], HOUR);
    for _ in 0..2 {
        f.k.cycle("blog");
        let q =
            f.k.open_asks()
                .unwrap()
                .into_iter()
                .rfind(|q| {
                    f.k.index()
                        .unwrap()
                        .get(*q)
                        .is_some_and(|e| e.record.action() == Some("post"))
                })
                .expect("post asks each time");
        f.k.approve(q, None).unwrap();
        f.now += 60;
        avsmd::clock::set(f.now);
        f.k.tick();
    }
    let entries = f.entries();
    let ask = entries
        .iter()
        .find(|e| matches!(&e.record, Record::Ask { action: Some(a), .. } if a == "delegate"))
        .unwrap_or_else(|| panic!("{entries:#?}"));
    let proposal = entries
        .iter()
        .rfind(|e| matches!(&e.record, Record::Proposal { action, .. } if action == "delegate"))
        .unwrap()
        .seq;
    let cited = ask.record.cites();
    assert_eq!(cited.len(), 3, "{}", ask.line());
    for c in cited {
        let e = &entries[*c as usize - 1];
        assert!(e.seq > proposal && e.seq < ask.seq);
        assert_eq!(e.record.kind(), Kind::Forecast);
        assert_eq!(e.record.cites(), [proposal]);
    }
    // The approved posts ran for real, twice; the forecasts ran none, and
    // the mandate file is as the human wrote it.
    assert_eq!(
        std::fs::read_to_string(dir.join("invoked"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    assert!(!std::fs::read_to_string(dir.join("mandate.yaml"))
        .unwrap()
        .contains("delegated: true, takes"));
}

/// The time a 30-day, 16-seed forecast of examples/greenhouse takes, on a
/// day of its history. Run with: cargo test --release --test forecast -- --ignored --nocapture
#[test]
#[ignore]
fn time_a_month_of_the_greenhouse() {
    let _c = clock();
    let mut f = fixture("");
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples/greenhouse")
        .canonicalize()
        .unwrap();
    let copy = f.tmp.path().join("greenhouse");
    std::fs::create_dir_all(&copy).unwrap();
    for file in ["mandate.yaml", "report", "vent"] {
        std::fs::copy(dir.join(file), copy.join(file)).unwrap();
    }
    f.k.add(&copy).unwrap();
    for _ in 0..(24 * 60) {
        f.now += 60;
        avsmd::clock::set(f.now);
        f.k.report("greenhouse", false);
    }
    let opts = f.opts(&["greenhouse"], 30, 16);
    let started = std::time::Instant::now();
    let (outcome, _) = f.k.forecast(&opts, &[]).unwrap();
    let secs = started.elapsed().as_secs_f64();
    println!("{}", outcome.render());
    println!("greenhouse: 30 days x 16 seeds x 3 policies in {secs:.2}s");
}

// ----- the binding limit (#39) -----

fn capacity_asks(f: &Fixture) -> Vec<Entry> {
    f.entries()
        .into_iter()
        .filter(|e| {
            matches!(
                &e.record,
                Record::Ask {
                    signal: Some(Signal::Capacity { .. }),
                    ..
                }
            )
        })
        .collect()
}

fn question(e: &Entry) -> &str {
    match &e.record {
        Record::Ask { question, .. } => question,
        _ => unreachable!(),
    }
}

/// A tomato on a threshold, its water capped at 4 a day.
const TOMATO: &str = "controller: {kind: threshold, below: 35, action: water}\ncadence: 2h\n\
     permitted:\n- {name: water, run: echo ran >> invoked, cap: 4/day, reversible: true, delegated: true}\n";

/// `days` days of moisture every 2h, `day(i)` the readings of day i,
/// with the nightly check after each day.
fn tomato_days(f: &mut Fixture, days: usize, day: impl Fn(usize) -> Vec<f64>) {
    for d in 0..days {
        f.history("tomato", &day(d), 2 * HOUR);
        f.k.check_capacity("tomato");
    }
}

/// Three low readings a day: water runs three times, one to spare.
fn three_low(_: usize) -> Vec<f64> {
    (0..12)
        .map(|i| if i % 4 == 0 { 30.0 } else { 50.0 })
        .collect()
}

#[test]
fn a_cap_spent_two_days_running_with_the_metric_past_its_threshold_asks_once_naming_the_cap() {
    let _c = clock();
    let mut f = fixture("");
    f.venture("tomato", "moisture", TOMATO);
    // Dry all day, every day: water runs at each low reading until the cap
    // is spent, and the metric is still below 35 after.
    tomato_days(&mut f, 3, |_| vec![30.0; 12]);
    let asks = capacity_asks(&f);
    assert_eq!(asks.len(), 1, "{:#?}", f.entries());
    let q = question(&asks[0]);
    assert!(q.contains("water's cap 4/day"), "{q}");
    assert!(q.contains("no margin"), "{q}");
    assert!(q.contains("below 35"), "{q}");
    let Record::Ask { claim, packet, .. } = &asks[0].record else {
        unreachable!()
    };
    assert_eq!(*claim, Claim::Measured);
    // The packet rests on the runs and the readings, never on a model.
    let packet = packet.as_ref().unwrap();
    assert!(packet.premises.len() > 1, "{packet:?}");
    // Not again within the episode, however long the cap stays spent.
    tomato_days(&mut f, 3, |_| vec![30.0; 12]);
    assert_eq!(capacity_asks(&f).len(), 1);
}

#[test]
fn a_capacity_ask_says_under_each_policy_whether_the_metric_holds_its_threshold() {
    let _c = clock();
    let mut f = fixture("forecast: {days: 3, seeds: 2}\n");
    f.k.forecasts = true;
    f.venture("tomato", "moisture", TOMATO);
    tomato_days(&mut f, 3, |_| vec![30.0; 12]);
    let asks = capacity_asks(&f);
    assert_eq!(asks.len(), 1, "{:#?}", f.entries());
    let Record::Ask {
        packet: Some(p), ..
    } = &asks[0].record
    else {
        unreachable!()
    };
    let names: Vec<&str> = p.policies.iter().map(|x| x.policy.as_str()).collect();
    assert_eq!(names, ["all", "none", "mixed"], "{p:?}");
    for x in &p.policies {
        assert_eq!(x.metric, "moisture");
        let t = x.threshold.as_deref().unwrap();
        assert!(t.contains("below 35"), "{t}");
    }
    assert!(p.render().contains("forecast by policy (inferred):"));
}

#[test]
fn a_cap_spent_while_the_metric_is_on_the_right_side_gets_no_capacity_ask() {
    let _c = clock();
    let mut f = fixture("");
    f.venture("tomato", "moisture", TOMATO);
    // Four runs a day, every day, from somewhere other than the threshold
    // (a human, say), while moisture reads 50 throughout.
    for _ in 0..3 {
        for i in 0..12 {
            f.history("tomato", &[50.0], 2 * HOUR);
            if i % 3 == 0 {
                f.k.append(Record::Intent {
                    claim: Claim::Measured,
                    venture: "tomato".into(),
                    action: "water".into(),
                    proof: gate::Proof {
                        venture: "tomato".into(),
                        action: "water".into(),
                        steps: vec![],
                    },
                    payload: None,
                    key: None,
                })
                .unwrap();
            }
        }
        f.k.check_capacity("tomato");
    }
    let m = f.k.ventures["tomato"].mandate.clone();
    let used = f.k.index().unwrap().used("tomato", "water", f.now - DAY);
    assert_eq!(used, 4, "the cap is spent");
    assert!(forecast::cap_spent(&f.k.index().unwrap(), "tomato", &m, f.now).is_none());
    assert!(capacity_asks(&f).is_empty(), "{:#?}", f.entries());
}

#[test]
fn a_cap_with_margin_left_gets_no_capacity_ask() {
    let _c = clock();
    let mut f = fixture("");
    f.venture("tomato", "moisture", TOMATO);
    tomato_days(&mut f, 4, three_low);
    assert!(capacity_asks(&f).is_empty(), "{:#?}", f.entries());
    // One day spent is not two days running.
    tomato_days(&mut f, 1, |_| vec![30.0; 12]);
    tomato_days(&mut f, 1, three_low);
    assert!(capacity_asks(&f).is_empty(), "{:#?}", f.entries());
    // Four low readings a day, each answered: the cap is used up, but no
    // reading went unanswered, so it never bound.
    tomato_days(&mut f, 3, |_| {
        (0..12)
            .map(|i| if i % 3 == 0 { 30.0 } else { 50.0 })
            .collect()
    });
    let used = f.k.index().unwrap().used("tomato", "water", f.now - DAY);
    assert_eq!(used, 4, "the cap is used up");
    assert!(capacity_asks(&f).is_empty(), "{:#?}", f.entries());
}

/// An inbox whose reply asks each time; nobody proposes anything.
const INBOX: &str = "controller: {kind: model}\ncadence: 6h\nbetter: down\n\
     permitted:\n- {name: reply, run: echo ran >> invoked, cap: 5/day, reversible: false, takes: text}\n";

fn reply_ask(f: &mut Fixture) -> u64 {
    f.k.append(Record::Ask {
        claim: Claim::Inferred,
        venture: "inbox".to_string(),
        action: Some("reply".to_string()),
        question: "reply to customer1?".to_string(),
        refusal: None,
        payload: Some(Payload::Text {
            text: "customer1@example.com\nthanks".to_string(),
        }),
        signal: None,
        cites: vec![],
        packet: None,
    })
    .unwrap()
    .seq
}

/// A refusal of reply for the asks quota, opened and closed with `count`.
fn quota_refusal(f: &mut Fixture, count: u32) {
    let refusal = |count: u32, closes: Option<u64>| Record::Refusal {
        claim: Claim::Measured,
        venture: "inbox".to_string(),
        action: "reply".to_string(),
        reason: "AsksQuotaSpent; NeedsDecision(reply)".to_string(),
        detail: "the asks quota is used up, so no ask was written".to_string(),
        count,
        closes,
    };
    let open = f.k.append(refusal(1, None)).unwrap().seq;
    f.k.append(refusal(count, Some(open))).unwrap();
}

/// Unread rising 3 a day, four reports a day, `each` run once a day.
fn inbox_days(f: &mut Fixture, days: std::ops::Range<usize>, mut each: impl FnMut(&mut Fixture)) {
    for d in days {
        let base = 10.0 + 3.0 * d as f64;
        f.history(
            "inbox",
            &[base, base + 1.0, base + 2.0, base + 2.0],
            6 * HOUR,
        );
        each(f);
    }
}

#[test]
fn a_better_down_metric_rising_while_its_asks_are_denied_asks_naming_the_denials() {
    let _c = clock();
    let mut f = fixture("");
    f.venture("inbox", "unread", INBOX);
    inbox_days(&mut f, 0..8, |f| {
        let a = reply_ask(f);
        f.k.deny(a, "not now").unwrap();
    });
    let m = f.k.ventures["inbox"].mandate.clone();
    assert!(
        forecast::capacity(&f.k.index().unwrap(), "inbox", &m).is_none(),
        "the rate check alone sees no gap"
    );
    f.k.check_capacity("inbox");
    let asks = capacity_asks(&f);
    assert_eq!(asks.len(), 1, "{:#?}", f.entries());
    let q = question(&asks[0]);
    assert!(q.contains("unread rose"), "{q}");
    assert!(q.contains("reply ran 0 of 35"), "{q}");
    assert!(q.contains("7 of 7 asks denied"), "{q}");
    let Record::Ask { packet, .. } = &asks[0].record else {
        unreachable!()
    };
    // The denials are premises: declared records a human can check.
    let premises = &packet.as_ref().unwrap().premises;
    assert!(
        premises.iter().any(|p| p.claim == Claim::Declared),
        "{premises:?}"
    );
}

#[test]
fn a_better_down_metric_rising_while_the_asks_quota_refuses_asks_naming_the_quota() {
    let _c = clock();
    let mut f = fixture("");
    f.venture("inbox", "unread", INBOX);
    inbox_days(&mut f, 0..8, |f| quota_refusal(f, 3));
    f.k.check_capacity("inbox");
    let asks = capacity_asks(&f);
    assert_eq!(asks.len(), 1, "{:#?}", f.entries());
    let q = question(&asks[0]);
    assert!(
        q.contains("refused 21 times: the asks quota was spent"),
        "{q}"
    );
    assert!(!q.contains("denied"), "{q}");
}

#[test]
fn a_rising_metric_whose_controller_proposes_nothing_names_the_controller_and_asks_once() {
    let _c = clock();
    let mut f = fixture("");
    f.venture("inbox", "unread", INBOX);
    inbox_days(&mut f, 0..8, |_| {});
    f.k.check_capacity("inbox");
    let asks = capacity_asks(&f);
    assert_eq!(asks.len(), 1, "{:#?}", f.entries());
    let q = question(&asks[0]);
    assert!(q.contains("the controller proposed it 0 times"), "{q}");
    // Answered, and still rising: the episode is not over, no new ask.
    for q in f.k.open_asks().unwrap() {
        f.k.deny(q, "seen").unwrap();
    }
    inbox_days(&mut f, 8..12, |f| {
        f.k.check_capacity("inbox");
    });
    assert_eq!(capacity_asks(&f).len(), 1);
}

#[test]
fn a_metric_not_worse_every_day_gets_no_held_back_ask() {
    let _c = clock();
    let mut f = fixture("");
    f.venture("inbox", "unread", INBOX);
    // Three days rising: too short a history to say.
    for d in 0..3 {
        f.history("inbox", &[10.0 + 3.0 * d as f64; 4], 6 * HOUR);
        let a = reply_ask(&mut f);
        f.k.deny(a, "not now").unwrap();
        f.k.check_capacity("inbox");
    }
    assert!(capacity_asks(&f).is_empty(), "{:#?}", f.entries());
    // Rising each day it was read, but one day of the week unread: not
    // known to be worse each day.
    for d in 3..10 {
        if d == 6 {
            f.now += DAY;
            continue;
        }
        f.history("inbox", &[10.0 + 3.0 * d as f64; 4], 6 * HOUR);
    }
    f.k.check_capacity("inbox");
    assert!(capacity_asks(&f).is_empty(), "{:#?}", f.entries());
    // Worse over the week, but up two and down one: not worse each day.
    for x in [16.0, 20.0, 18.0, 22.0, 26.0, 24.0, 28.0] {
        f.history("inbox", &[x; 4], 6 * HOUR);
        let a = reply_ask(&mut f);
        f.k.deny(a, "not now").unwrap();
    }
    f.k.check_capacity("inbox");
    assert!(capacity_asks(&f).is_empty(), "{:#?}", f.entries());
}

#[test]
fn a_rising_metric_whose_lever_ran_its_cap_names_the_cap() {
    let _c = clock();
    let mut f = fixture("");
    f.venture(
        "inbox",
        "unread",
        "controller: {kind: model}\ncadence: 6h\nbetter: down\n\
         permitted:\n- {name: archive, run: echo ran >> invoked, cap: 1/day, reversible: true, delegated: true}\n",
    );
    inbox_days(&mut f, 0..8, |f| {
        f.k.append(Record::Intent {
            claim: Claim::Measured,
            venture: "inbox".into(),
            action: "archive".into(),
            proof: gate::Proof {
                venture: "inbox".into(),
                action: "archive".into(),
                steps: vec![],
            },
            payload: None,
            key: None,
        })
        .unwrap();
    });
    f.k.check_capacity("inbox");
    let asks = capacity_asks(&f);
    assert_eq!(asks.len(), 1, "{:#?}", f.entries());
    let q = question(&asks[0]);
    assert!(q.contains("archive ran 7 of 7: the cap"), "{q}");
}
