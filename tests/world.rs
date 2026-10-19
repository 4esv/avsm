//! World models in the kernel: predictions are inferred records, resolved
//! and scored by code on trusted, plausible reports; a ranking from a model
//! below its calibration bar asks; and no prediction ever authorizes.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use avsmd::config::Quotas;
use avsmd::gate::{self, Candidate, Reason};
use avsmd::kernel::predict;
use avsmd::kernel::Kernel;
use avsmd::record::{Claim, Entry, Kind, Quantiles, Record, Seq};
use avsmd::role::{Context, InferRequest, Role};
use avsmd::warden::LocalWarden;

/// The virtual clock is shared by the whole process: one test at a time.
static CLOCK: Mutex<()> = Mutex::new(());

const T0: i64 = 1_800_000_000;
const HOUR: i64 = 3_600;

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
    _tmp: tempfile::TempDir,
    k: Kernel,
    dir: PathBuf,
    now: i64,
}

/// A venture `shop` whose metric `count` is read from a file, with a
/// reversible, delegated lever `boost` that touches `boosted`, and the
/// given extra mandate lines.
fn fixture(controller: &str, extra: &str) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.yaml"),
        "nightly: never\ndefault: k\nbackends:\n  k: {kind: command, run: echo nothing}\n",
    )
    .unwrap();
    let warden = LocalWarden::open(&home).unwrap();
    let mut k = Kernel::with(&home, Box::new(NoRole), Box::new(warden)).unwrap();
    let dir = tmp.path().join("shop");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mandate.yaml"),
        format!(
            "purpose: Sell.\nmetric: count\ncontroller: {controller}\ncadence: 1h\n\
             permitted:\n- {{name: boost, run: touch boosted, cap: 99/day, reversible: true, delegated: true}}\n{extra}"
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
    fn report(&mut self, value: f64) {
        self.now += HOUR;
        avsmd::clock::set(self.now);
        std::fs::write(self.dir.join("value"), format!("{value}\n")).unwrap();
        self.k.report("shop", false);
    }

    fn entries(&self) -> Vec<Entry> {
        self.k.entries().unwrap()
    }

    fn of(&self, kind: Kind) -> Vec<Entry> {
        self.entries()
            .into_iter()
            .filter(|e| e.record.kind() == kind)
            .collect()
    }

    fn scores(&self) -> Vec<Entry> {
        self.entries()
            .into_iter()
            .filter(|e| {
                matches!(&e.record, Record::Report { metric, .. } if metric == "calibration.baseline")
            })
            .collect()
    }

    fn gate(&self, cites: &[Seq]) -> Result<gate::Proof, gate::Refusal> {
        gate::gate_in(
            &self.k.index().unwrap(),
            &Candidate {
                venture: "shop".to_string(),
                action: "boost".to_string(),
                cites: cites.to_vec(),
            },
            &self.k.adopted(),
            &Quotas::default(),
            self.now,
        )
    }
}

fn prediction(claim: Claim, resolves_at: i64, action: Option<&str>) -> Record {
    Record::Prediction {
        claim,
        venture: "shop".to_string(),
        model: "baseline".to_string(),
        metric: "count".to_string(),
        action: action.map(str::to_string),
        made_at: resolves_at - HOUR,
        resolves_at,
        quantiles: Quantiles::from_values([8.0, 9.0, 9.5, 10.0, 10.5, 11.0, 12.0]),
        events: vec![],
    }
}

#[test]
fn reports_bring_predictions_and_code_scores_them() {
    let _c = clock();
    let mut f = fixture("{kind: model}", "calibration: 0.2\n");
    for x in [10.0, 11.0, 10.5, 11.5, 11.0, 12.0] {
        f.report(x);
    }
    // One prediction a report, each inferred, each a step ahead.
    let preds = f.of(Kind::Prediction);
    assert_eq!(preds.len(), 6);
    for p in &preds {
        match &p.record {
            Record::Prediction {
                claim,
                model,
                resolves_at,
                quantiles,
                ..
            } => {
                assert_eq!(*claim, Claim::Inferred);
                assert_eq!(model, "baseline");
                assert_eq!(*resolves_at, p.at + HOUR);
                assert!(quantiles.valid(), "{quantiles:?}");
            }
            other => panic!("{other:?}"),
        }
    }
    // Every prediction but the last is resolved by the next report and
    // scored: a measured report citing the prediction and the report.
    let scores = f.scores();
    assert_eq!(scores.len(), 5, "{scores:?}");
    for (s, p) in scores.iter().zip(&preds) {
        match &s.record {
            Record::Report {
                claim, cites, note, ..
            } => {
                assert_eq!(*claim, Claim::Measured);
                assert_eq!(cites[0], p.seq);
                assert!(
                    matches!(&f.k.index().unwrap().get(cites[1]).unwrap().record,
                        Record::Report { metric, claim: Claim::Measured, .. } if metric == "count"),
                    "{s:?}"
                );
                assert!(note.contains("crps "), "{note}");
                assert!(note.contains("80% "), "{note}");
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(f.k.index().unwrap().open_predictions("shop").len(), 1);
    // Five scores: enough to judge, and the rolling mean is what decides.
    let (mean, n) = f.k.calibration("shop");
    assert_eq!(n, 5);
    assert_eq!(f.k.trusted("shop"), mean.unwrap() >= 0.2);
}

#[test]
fn only_the_first_trusted_plausible_report_at_or_after_resolves_at_resolves() {
    let _c = clock();
    let mut f = fixture("{kind: model}", "range: [0, 100]\n");
    f.report(10.0);
    let mandate = f.k.ventures["shop"].mandate.clone();
    let due = f.now + 2 * HOUR;
    let p = f.k.append(prediction(Claim::Inferred, due, None)).unwrap();
    let scored = |f: &Fixture| -> Option<Record> {
        let index = f.k.index().unwrap();
        predict::score(&index, index.get(p.seq).unwrap(), &mandate)
    };
    // Before resolves_at: not yet.
    f.report(10.5);
    assert!(scored(&f).is_none());
    // At resolves_at, but inferred: never.
    f.now += HOUR;
    avsmd::clock::set(f.now);
    f.k.append(Record::report(
        Claim::Inferred,
        "shop",
        "count",
        Some(10.0),
        "a model's guess",
    ))
    .unwrap();
    assert!(scored(&f).is_none());
    // Measured, but implausible: never.
    f.report(1000.0);
    assert!(scored(&f).is_none());
    // The first trusted, plausible one resolves it, and only it.
    f.report(11.0);
    let first = f.entries().last().unwrap().seq;
    f.report(13.0);
    match scored(&f).unwrap() {
        Record::Report {
            claim,
            metric,
            value,
            cites,
            note,
            ..
        } => {
            assert_eq!(claim, Claim::Measured);
            assert_eq!(metric, "calibration.baseline");
            assert_eq!(cites, vec![p.seq, first]);
            // Persistence said 10, the outcome was 11: CRPS of the
            // quantiles against an error of 1.
            let q = Quantiles::from_values([8.0, 9.0, 9.5, 10.0, 10.5, 11.0, 12.0]);
            let skill = predict::crpss(predict::crps(&q, 11.0), 1.0);
            assert_eq!(value, Some(skill));
            assert!(note.contains("80% hit"), "{note}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn below_the_calibration_bar_a_ranking_is_an_ask_not_a_run() {
    let _c = clock();
    let mut f = fixture("{kind: model}", "calibration: 0.5\n");
    f.report(10.0);
    let proposal =
        f.k.append(Record::Proposal {
            claim: Claim::Inferred,
            venture: "shop".to_string(),
            action: "boost".to_string(),
            reason: "the world model ranks it first".to_string(),
            payload: None,
            cites: vec![],
        })
        .unwrap();
    // The gate alone would let it run: reversible, delegated, proposed.
    assert!(f.gate(&[proposal.seq]).is_ok());
    // No scores yet: the model is not trusted; the ranking asks.
    assert!(!f.k.trusted("shop"));
    f.k.follow("shop", &["boost".to_string()], vec![proposal.seq]);
    assert!(f.of(Kind::Intent).is_empty());
    assert!(!f.dir.join("boosted").exists());
    let asks = f.of(Kind::Ask);
    assert_eq!(asks.len(), 1);
    assert!(
        matches!(&asks[0].record, Record::Ask { action: Some(a), claim: Claim::Measured, question, .. }
            if a == "boost" && question.contains("under the bar of 0.5")),
        "{asks:?}"
    );
    // Five scores under the bar: still an ask (the one already open).
    for _ in 0..5 {
        f.k.append(Record::report(
            Claim::Measured,
            "shop",
            "calibration.baseline",
            Some(0.4),
            "",
        ))
        .unwrap();
    }
    assert!(!f.k.trusted("shop"));
    f.k.follow("shop", &["boost".to_string()], vec![proposal.seq]);
    assert!(f.of(Kind::Intent).is_empty());
    // Declared or inferred scores do not count; measured ones over the bar
    // do.
    for _ in 0..20 {
        f.k.append(Record::report(
            Claim::Inferred,
            "shop",
            "calibration.baseline",
            Some(1.0),
            "",
        ))
        .unwrap();
    }
    assert!(!f.k.trusted("shop"));
    for _ in 0..20 {
        f.k.append(Record::report(
            Claim::Measured,
            "shop",
            "calibration.baseline",
            Some(0.9),
            "",
        ))
        .unwrap();
    }
    assert!(f.k.trusted("shop"));
    f.k.follow("shop", &["boost".to_string()], vec![proposal.seq]);
    assert_eq!(f.of(Kind::Intent).len(), 1);
    assert!(f.dir.join("boosted").exists());
}

#[test]
fn a_prediction_never_authorizes() {
    let _c = clock();
    let mut f = fixture("{kind: threshold, below: 5, action: boost}", "");
    f.report(10.0);
    // An inferred prediction that the lever would do well, cited alone.
    let p =
        f.k.append(prediction(Claim::Inferred, f.now + HOUR, Some("boost")))
            .unwrap();
    let refusal = f.gate(&[p.seq]).unwrap_err();
    assert!(
        refusal
            .reasons
            .iter()
            .any(|r| matches!(r, Reason::NotJustified { .. })),
        "{refusal}"
    );
    assert!(
        refusal
            .reasons
            .iter()
            .any(|r| matches!(r, Reason::OnlyInferred { seqs } if *seqs == vec![p.seq])),
        "{refusal}"
    );
    // Not even one a hand claimed was measured: a prediction is no premise.
    let forged =
        f.k.append(prediction(Claim::Measured, f.now + HOUR, Some("boost")))
            .unwrap();
    let refusal = f.gate(&[forged.seq]).unwrap_err();
    assert!(
        refusal
            .reasons
            .iter()
            .any(|r| matches!(r, Reason::NotJustified { .. })),
        "{refusal}"
    );
    // Nor beside a report that crosses nothing.
    let last = f.of(Kind::Report).last().unwrap().seq;
    assert!(f.gate(&[last, p.seq]).is_err());
    // `why` says what it is.
    let why = f.k.why(p.seq).unwrap();
    assert!(why.contains("authorizes nothing"), "{why}");
}

#[test]
fn add_refuses_a_world_that_is_a_lever() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = tmp.path().join("shop");
    std::fs::create_dir_all(&dir).unwrap();
    script(&dir, "report", "echo 1");
    std::fs::write(
        dir.join("mandate.yaml"),
        "purpose: Sell.\nmetric: count\ncontroller: {kind: model}\ncadence: 1h\n\
         permitted:\n- {name: boost, run: ./boost --hard, cap: 1/day, reversible: true}\n\
         world: boost --hard\n",
    )
    .unwrap();
    let mut k = Kernel::open(&home).unwrap();
    let err = k.add(&dir).unwrap_err();
    assert!(
        err.contains("the command of lever \"boost\"") && err.contains("never a lever"),
        "{err}"
    );
    assert!(k.entries().unwrap().is_empty());
    // Another command is a world model like any other.
    std::fs::write(
        dir.join("mandate.yaml"),
        "purpose: Sell.\nmetric: count\ncontroller: {kind: model}\ncadence: 1h\n\
         permitted:\n- {name: boost, run: ./boost --hard, cap: 1/day, reversible: true}\n\
         world: ./world\ncalibration: 0.1\n",
    )
    .unwrap();
    let said = k.add(&dir).unwrap();
    assert!(
        said.contains(
            "world model: ./world forecasts count; it ranks, vetoes and asks, and never authorizes"
        ),
        "{said}"
    );
    assert!(said.contains("at least 0.1; until then they ask"), "{said}");
}
