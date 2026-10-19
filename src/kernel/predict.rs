//! Predictions: what a venture's world model says the next report will
//! read, written as inferred records and scored by code once a trusted,
//! plausible report resolves them. The score is a measured report,
//! `calibration.<model>`, citing the prediction and the report.
//!
//! Non-negotiable 14: world models, rollouts and forecasts are inferred.
//! They rank, veto and ask, and never authorize. A model whose rolling
//! score is below the mandate's `calibration:` bar, or that has fewer than
//! five scores, only tightens: its rankings become asks, never runs.

use serde_json::Value;

use crate::index::{Index, CALIBRATION};
use crate::mandate::Mandate;
use crate::record::{Claim, Entry, Event, Quantiles, Record, Seq, MAX_EVENTS};
use crate::world::{self, Action};

use super::{log, now, Kernel};

/// How many of the latest scores the rolling mean takes.
pub const WINDOW: usize = 20;
/// The fewest scores before a model is trusted at all.
pub const MIN_SCORES: usize = 5;
/// The bar when the mandate sets none: beat persistence.
pub const DEFAULT_BAR: f64 = 0.0;
/// Ensemble members per prediction: the baseline is cheap, a world
/// command is a process each.
pub const BASELINE_MEMBERS: u64 = 100;
pub const WORLD_MEMBERS: u64 = 8;

/// The pinball loss of quantile `q` at level `tau` for outcome `x`.
fn pinball(tau: f64, q: f64, x: f64) -> f64 {
    let d = x - q;
    if d >= 0.0 {
        tau * d
    } else {
        (tau - 1.0) * d
    }
}

/// The CRPS of a forecast given by its quantiles, for outcome `x`: twice
/// the mean pinball loss over the levels. For a point forecast, every
/// quantile the same, it is the absolute error.
pub fn crps(q: &Quantiles, x: f64) -> f64 {
    let n = Quantiles::LEVELS.len() as f64;
    let sum: f64 = Quantiles::LEVELS
        .iter()
        .zip(q.values())
        .map(|(tau, v)| pinball(*tau, v, x))
        .sum();
    2.0 * sum / n
}

/// The CRPS of persistence: the last value, held, as a point forecast.
pub fn crps_persistence(last: f64, x: f64) -> f64 {
    crps(&Quantiles::from_values([last; 7]), x)
}

/// The skill against a reference: 1 is perfect, 0 no better, below 0
/// worse. Kept within -1 and 1, so one wild miss cannot sink a rolling
/// mean on its own; a perfect reference counts as a tie when the model is
/// perfect too, and as -1 when it is not.
pub fn crpss(model: f64, reference: f64) -> f64 {
    if reference > 0.0 {
        (1.0 - model / reference).clamp(-1.0, 1.0)
    } else if model <= 0.0 {
        0.0
    } else {
        -1.0
    }
}

/// The Brier score of events: the mean squared gap between the chance
/// given and what happened. None when no event resolved.
pub fn brier(events: &[(f64, bool)]) -> Option<f64> {
    if events.is_empty() {
        return None;
    }
    let sum: f64 = events
        .iter()
        .map(|(p, hit)| (p - if *hit { 1.0 } else { 0.0 }).powi(2))
        .sum();
    Some(sum / events.len() as f64)
}

/// Did the 80% interval, p10 to p90, hold the outcome?
pub fn covered(q: &Quantiles, x: f64) -> bool {
    q.p10 <= x && x <= q.p90
}

/// The rolling mean of a model's last `WINDOW` measured scores for a
/// venture, and how many it took.
pub fn rolling(index: &Index, venture: &str, model: &str) -> (Option<f64>, usize) {
    let metric = format!("{CALIBRATION}{model}");
    let scores: Vec<f64> = index
        .series(venture, &metric)
        .iter()
        .filter(|(s, _)| {
            index
                .get(*s)
                .is_some_and(|e| e.record.claim() == Claim::Measured)
        })
        .map(|(_, v)| *v)
        .collect();
    let from = scores.len().saturating_sub(WINDOW);
    let last = &scores[from..];
    if last.is_empty() {
        return (None, 0);
    }
    (
        Some(last.iter().sum::<f64>() / last.len() as f64),
        last.len(),
    )
}

/// May the venture's world model rank levers that then run? Only with
/// `MIN_SCORES` scores or more and a rolling mean at or above the bar.
pub fn trusted(index: &Index, venture: &str, mandate: &Mandate) -> bool {
    let bar = mandate.calibration.unwrap_or(DEFAULT_BAR);
    match rolling(index, venture, world::model_name(mandate)) {
        (Some(mean), n) => n >= MIN_SCORES && mean >= bar,
        (None, _) => false,
    }
}

/// Does a venture get predictions once per report? With a world model, a
/// calibration bar, or a planner, whose trust rests on them.
pub fn predicts(mandate: &Mandate) -> bool {
    mandate.world.is_some()
        || mandate.calibration.is_some()
        || matches!(mandate.controller, crate::mandate::Controller::Plan { .. })
}

/// The first trusted, plausible report of `metric` after `after` and at or
/// after `at`: what resolves a prediction.
fn first_at(index: &Index, venture: &str, metric: &str, after: Seq, at: i64) -> Option<(Seq, f64)> {
    index
        .series(venture, metric)
        .iter()
        .filter(|(s, _)| *s > after)
        .find(|(s, _)| index.get(*s).is_some_and(|e| e.at >= at))
        .copied()
}

/// The score of a resolved prediction, as a measured report citing the
/// prediction and the report that resolved it; None while unresolved.
pub fn score(index: &Index, prediction: &Entry, mandate: &Mandate) -> Option<Record> {
    let Record::Prediction {
        venture,
        model,
        metric,
        resolves_at,
        quantiles,
        events,
        ..
    } = &prediction.record
    else {
        return None;
    };
    let (by, x) = first_at(index, venture, metric, prediction.seq, *resolves_at)?;
    let crps = crps(quantiles, x);
    let series = index.series(venture, metric);
    let before = series.partition_point(|(s, _)| *s < prediction.seq);
    let last = before.checked_sub(1).map(|i| series[i].1);
    let reference = last.map(|l| crps_persistence(l, x));
    let skill = reference.map(|r| crpss(crps, r));
    let resolved: Vec<(f64, bool)> = events
        .iter()
        .filter_map(|e| {
            let name = e.name.strip_prefix("guard.")?;
            let guard = mandate.guard(name)?;
            let (_, reading) = first_at(index, venture, &e.name, prediction.seq, *resolves_at)?;
            Some((e.p, guard.breached(reading)))
        })
        .collect();
    let brier = brier(&resolved);
    let hit = covered(quantiles, x);
    let mut note = format!("crps {crps:.4}");
    match (reference, skill) {
        (Some(r), Some(s)) => note.push_str(&format!(" persistence {r:.4} crpss {s:.4}")),
        _ => note.push_str(" persistence none"),
    }
    note.push_str(if hit { " 80% hit" } else { " 80% miss" });
    if let Some(b) = brier {
        note.push_str(&format!(" brier {b:.4} over {}", resolved.len()));
    }
    note.push_str(&format!("; {metric}={x} against p50 {}", quantiles.p50));
    Some(Record::Report {
        claim: Claim::Measured,
        venture: venture.clone(),
        metric: format!("{CALIBRATION}{model}"),
        value: skill,
        note,
        source: None,
        confidence: None,
        cites: vec![prediction.seq, by],
        implausible: None,
        facts: None,
    })
}

impl Kernel {
    /// May this venture's world model rank levers that then run?
    pub fn trusted(&self, venture: &str) -> bool {
        let Some(v) = self.ventures.get(venture) else {
            return false;
        };
        self.index()
            .is_ok_and(|index| trusted(&index, venture, &v.mandate))
    }

    /// The rolling mean of the venture's world model's scores, and how
    /// many it took.
    pub fn calibration(&self, venture: &str) -> (Option<f64>, usize) {
        let Some(v) = self.ventures.get(venture) else {
            return (None, 0);
        };
        match self.index() {
            Ok(index) => rolling(&index, venture, world::model_name(&v.mandate)),
            Err(_) => (None, 0),
        }
    }

    /// Follow a ranking of levers, best first, from the venture's world
    /// model. A trusted model's best lever goes to the gate like any
    /// proposal; an untrusted one's becomes an ask, never a run. The
    /// ranking is never the premise: `cites` is.
    pub fn follow(&mut self, venture: &str, ranked: &[String], cites: Vec<Seq>) -> Vec<String> {
        let Some(best) = ranked.first() else {
            return vec![];
        };
        if self.trusted(venture) {
            return self.act(venture, best, cites, Claim::Inferred);
        }
        let Some(v) = self.ventures.get(venture).cloned() else {
            return vec![format!("{venture}: no such venture")];
        };
        let model = world::model_name(&v.mandate);
        let (mean, n) = self.calibration(venture);
        let bar = v.mandate.calibration.unwrap_or(DEFAULT_BAR);
        let (already, may_ask, payload) = {
            let Ok(index) = self.index() else {
                return vec![format!("{venture}: cannot read the ledger")];
            };
            let payload = cites
                .iter()
                .filter_map(|s| index.get(*s))
                .find_map(|e| e.record.payload().cloned());
            let already = super::asked_already(&index, venture, best, payload.as_ref());
            (already, self.may_ask(&index), payload)
        };
        // An approval of this run cannot cure a suspended lever, a guard, a
        // cap or a payload the lever's check refuses: no ask, a refusal.
        let refused =
            self.unaskable(venture, best, &cites)
                .or_else(|| match (&payload, already, may_ask) {
                    (Some(p), false, true) => self.precheck(venture, best, p),
                    _ => None,
                });
        let line = if let Some(line) = refused {
            line
        } else if already {
            format!("{venture}: {best} is already asked; see: avsm asks")
        } else if !may_ask {
            format!("{venture}: {best} would ask, but the asks quota is used up; not asking")
        } else {
            let standing = match mean {
                Some(m) => format!("a mean skill of {m:.3} over {n} scores"),
                None => "no scores yet".to_string(),
            };
            let packet = self.packet(venture, Some(best), &cites, None, payload.is_some());
            match self.append(Record::Ask {
                packet,
                claim: Claim::Measured,
                venture: venture.to_string(),
                action: Some(best.clone()),
                question: format!(
                    "may I {best}? the {model} world model ranks it first, but it has {standing}, under the bar of {bar} over at least {MIN_SCORES}; until it is calibrated its rankings ask"
                ),
                refusal: None,
                payload,
                signal: None,
                cites: vec![],
            }) {
                Ok(e) => e.line(),
                Err(e) => format!("{venture}: cannot write ask: {e}"),
            }
        };
        log(&line);
        vec![line]
    }

    /// One step ahead: what the venture's world model says the next report
    /// will read, with `action` if a lever just ran. Written inferred.
    pub(super) fn predict(&mut self, name: &str, action: Option<&str>) {
        let Some(v) = self.ventures.get(name).cloned() else {
            return;
        };
        let made_at = now();
        let dt = (v.mandate.cadence.as_secs() as i64).max(1);
        let model = {
            let Ok(index) = self.index() else { return };
            world::model_for(&index, name, &v.mandate, v.path.as_deref())
        };
        let members = if model.name() == world::BASELINE {
            BASELINE_MEMBERS
        } else {
            WORLD_MEMBERS
        };
        let act = action.map(|a| Action {
            name: a.to_string(),
            payload: None,
        });
        let base = world::mix(world::seed_of(name), made_at as u64);
        let steps = match world::ensemble(
            model.as_ref(),
            &Value::Null,
            act.as_ref(),
            base,
            made_at,
            dt,
            members,
        ) {
            Ok(s) => s,
            Err(e) => {
                log(&format!("{name}: no prediction: {e}"));
                return;
            }
        };
        let Some(quantiles) = world::quantiles(&steps.iter().map(|n| n.metric).collect::<Vec<_>>())
        else {
            return;
        };
        let mut events = Vec::new();
        for g in &v.mandate.guards {
            let readings: Vec<f64> = steps
                .iter()
                .filter_map(|n| n.guards.get(&g.name).copied())
                .collect();
            if readings.is_empty() || events.len() >= MAX_EVENTS {
                continue;
            }
            let breached = readings.iter().filter(|r| g.breached(**r)).count();
            events.push(Event {
                name: g.metric(),
                p: breached as f64 / readings.len() as f64,
            });
        }
        let record = Record::Prediction {
            claim: Claim::Inferred,
            venture: name.to_string(),
            model: model.name().to_string(),
            metric: v.mandate.metric_name().to_string(),
            action: action.map(str::to_string),
            made_at,
            resolves_at: made_at + dt,
            quantiles,
            events,
        };
        if let Err(e) = self.append(record) {
            log(&format!("{name}: cannot write prediction: {e}"));
        }
    }

    /// Score every open prediction of the venture that a report resolves.
    pub(super) fn resolve(&mut self, name: &str) {
        let Some(v) = self.ventures.get(name).cloned() else {
            return;
        };
        let scores: Vec<Record> = {
            let Ok(index) = self.index() else { return };
            index
                .open_predictions(name)
                .into_iter()
                .filter_map(|p| index.get(p))
                .filter_map(|p| score(&index, p, &v.mandate))
                .collect()
        };
        for s in scores {
            if let Err(e) = self.append(s) {
                log(&format!("{name}: cannot write score: {e}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(v: [f64; 7]) -> Quantiles {
        Quantiles::from_values(v)
    }

    #[test]
    fn crps_of_known_quantiles_matches_the_hand_computed_value() {
        // Quantiles 1..7 at levels .05 .10 .25 .50 .75 .90 .95, outcome 4.5:
        // below the outcome (x - q) * tau: 3.5*.05 + 2.5*.10 + 1.5*.25 + .5*.5
        //   = .175 + .25 + .375 + .25 = 1.05
        // above it (q - x) * (1 - tau): .5*.25 + 1.5*.10 + 2.5*.05
        //   = .125 + .15 + .125 = .4
        // CRPS = 2 / 7 * 1.45 = 0.414285714...
        let c = crps(&q([1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]), 4.5);
        assert!((c - 2.9 / 7.0).abs() < 1e-12, "{c}");
        // A point forecast scores its absolute error.
        assert!((crps_persistence(3.0, 7.5) - 4.5).abs() < 1e-12);
        assert!((crps_persistence(3.0, -1.0) - 4.0).abs() < 1e-12);
    }

    #[test]
    fn crpss_against_persistence_has_the_right_sign_on_a_toy_series() {
        // A series that climbs by 1 a step. Persistence always misses by 1;
        // a forecast centred on the next value beats it, one centred two
        // steps back loses to it.
        let series: Vec<f64> = (0..10).map(f64::from).collect();
        let mut good = Vec::new();
        let mut bad = Vec::new();
        for w in series.windows(2) {
            let (last, x) = (w[0], w[1]);
            let reference = crps_persistence(last, x);
            let near = q([-0.5, -0.3, -0.1, 0.0, 0.1, 0.3, 0.5].map(|d| last + 1.0 + d));
            let far = q([-0.5, -0.3, -0.1, 0.0, 0.1, 0.3, 0.5].map(|d| last - 1.0 + d));
            good.push(crpss(crps(&near, x), reference));
            bad.push(crpss(crps(&far, x), reference));
        }
        assert!(good.iter().all(|s| *s > 0.0), "{good:?}");
        assert!(bad.iter().all(|s| *s < 0.0), "{bad:?}");
        assert_eq!(crpss(0.0, 0.0), 0.0);
        assert_eq!(crpss(1.0, 0.0), -1.0);
        assert_eq!(crpss(100.0, 1.0), -1.0);
    }

    #[test]
    fn brier_and_coverage() {
        assert_eq!(brier(&[]), None);
        assert!((brier(&[(0.8, true), (0.3, false)]).unwrap() - (0.04 + 0.09) / 2.0).abs() < 1e-12);
        let qs = q([0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        assert!(covered(&qs, 1.0) && covered(&qs, 5.0) && covered(&qs, 3.0));
        assert!(!covered(&qs, 0.5) && !covered(&qs, 5.5));
    }
}
