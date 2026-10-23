//! World models: what would happen if. A world model steps a venture's
//! world forward from a state, with or without a lever, and says what the
//! metric would read. Everything it says is inferred: it ranks, vetoes and
//! asks, and never authorizes.
//!
//! The protocol, for a `world:` command run in the venture directory:
//!
//! - stdin: `{"state": <json|null>, "action": {"name", "payload"}|null,
//!   "seed": u64, "now": unix, "dt": secs, "history": [{"at", "value"}]}`,
//!   the history being the last 200 trusted, plausible reports of the
//!   metric;
//! - stdout: `{"state": <json>, "metric": f64, "guards": {name: f64}?}`.
//!
//! The reply is size-bounded and schema-validated, and anything that is
//! not a field is dropped (non-negotiable 11).
//!
//! Without a `world:` the baseline stands in: zero dependencies, its state
//! the metric value, its steps resampled from the ledger. An ensemble
//! member is a (model, seed) pair: the same seed gives the same step.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::index::{Index, EXPECTATION};
use crate::mandate::Mandate;
use crate::record::{Claim, Quantiles, Record, Seq};

/// The most a world command may print, in bytes.
pub const MAX_OUTPUT: usize = 64 * 1024;
/// How many reports of history a world command is shown.
pub const HISTORY: usize = 200;
/// How long one step of a world command may take.
pub const TIMEOUT: Duration = Duration::from_secs(30);
/// The fewest samples the baseline resamples from; with fewer it falls
/// back to persistence plus the drift's spread.
pub const MIN_SAMPLES: usize = 3;
/// The most guards a world command's reply may carry.
pub const MAX_GUARDS: usize = 16;
/// The model names a prediction carries.
pub const BASELINE: &str = "baseline";
pub const WORLD: &str = "world";

/// One step of splitmix64: advances `state` and returns the next number.
pub fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Two numbers mixed into one seed.
pub fn mix(a: u64, b: u64) -> u64 {
    let mut s = a ^ b.rotate_left(32) ^ 0xD6E8_FEB8_6659_FD93;
    splitmix64(&mut s)
}

/// A seed from text, stable across runs and builds (FNV-1a).
pub fn seed_of(text: &str) -> u64 {
    text.bytes().fold(0xCBF2_9CE4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01B3)
    })
}

/// The lever a step applies, with the payload it would consume.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Action {
    pub name: String,
    #[serde(default)]
    pub payload: Option<String>,
}

/// One trusted, plausible report: when, and what it read.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub at: i64,
    pub value: f64,
}

/// What a step says the world comes to.
#[derive(Clone, Debug, PartialEq)]
pub struct Next {
    pub state: Value,
    pub metric: f64,
    pub guards: BTreeMap<String, f64>,
}

/// Something that steps a venture's world.
pub trait WorldModel {
    /// `baseline` or `world`: what a prediction names as its model.
    fn name(&self) -> &'static str;
    /// One step: from `state`, with `action` if a lever runs, `dt` seconds
    /// on from `now`. Deterministic under `seed`.
    fn step(
        &self,
        state: &Value,
        action: Option<&Action>,
        seed: u64,
        now: i64,
        dt: i64,
    ) -> Result<Next, String>;
}

/// The last `HISTORY` trusted, plausible reports of a venture's metric.
pub fn history(index: &Index, venture: &str, metric: &str) -> Vec<Point> {
    let series = index.series(venture, metric);
    let from = series.len().saturating_sub(HISTORY);
    series[from..]
        .iter()
        .filter_map(|(s, v)| {
            index.get(*s).map(|e| Point {
                at: e.at,
                value: *v,
            })
        })
        .collect()
}

/// The world model of a venture: its `world:` command, else the baseline.
pub fn model_for(
    index: &Index,
    venture: &str,
    mandate: &Mandate,
    dir: Option<&Path>,
) -> Box<dyn WorldModel> {
    match (&mandate.world, dir) {
        (Some(cmd), Some(dir)) => Box::new(Command {
            dir: dir.to_path_buf(),
            cmd: cmd.clone(),
            history: history(index, venture, mandate.metric_name()),
        }),
        _ => Box::new(Baseline::from_index(index, venture, mandate)),
    }
}

/// The name of the model a venture's predictions come from.
pub fn model_name(mandate: &Mandate) -> &'static str {
    if mandate.world.is_some() {
        WORLD
    } else {
        BASELINE
    }
}

// ----- the baseline -----

/// One step of the metric between two reports with no lever run between
/// them: when it began, how long it took, and how far it moved.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Drift {
    pub at: i64,
    pub gap: i64,
    pub change: f64,
}

impl Drift {
    /// Steps of these changes, one `period` each, back to back from 0.
    pub fn every(period: i64, changes: &[f64]) -> Vec<Drift> {
        changes
            .iter()
            .enumerate()
            .map(|(i, c)| Drift {
                at: i as i64 * period,
                gap: period,
                change: *c,
            })
            .collect()
    }
}

/// The built-in world model. State is the metric value and the peak it
/// has reached: `{"value", "peak"}`, or a bare number.
///
/// Time moves the metric by its drift, learnt per second of elapsed time:
/// a step of `dt` adds the mean rate integrated over `dt`, and one
/// resampled residual scaled by √dt, as a random walk spreads. When the
/// history spans a day or more and every hour of the day has at least
/// `MIN_SAMPLES` steps, the mean rate is the one for each hour of the day
/// (UTC), and the value reverts, with a time constant of a day, to the
/// mean level of its hour of the day: a daily cycle comes back round
/// instead of compounding, and its spread stays bounded. A mandate's
/// `range:` bounds the value.
///
/// An action adds a resampled effect of that lever: the change from the
/// last report before a run to the report at its `expects` horizon, net
/// of the drift over that time, so drift is counted once at any horizon.
/// Drawdown guards are computed along the rolled path, from the peak so
/// far to the value now.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Baseline {
    /// The change of each drift step, report to report.
    pub drift: Vec<f64>,
    /// Per lever: what a run moved the metric, net of drift.
    pub effects: BTreeMap<String, Vec<f64>>,
    /// The latest value, the state when none is given.
    pub last: Option<f64>,
    /// The highest trusted, plausible value, the peak when none is given.
    pub peak: Option<f64>,
    /// The names of the drawdown guards over the metric.
    pub drawdowns: Vec<String>,
    /// Seconds between reports, typically.
    pub period: i64,
    /// The mandate's `range:`, low and high.
    pub range: Option<[f64; 2]>,
    /// Derived by `set_drift`: the mean drift a second, overall.
    pub rate: f64,
    /// Derived by `set_drift`: the mean drift a second for each hour of
    /// the day, when the history holds enough of every hour.
    pub by_hour: Option<[f64; 24]>,
    /// Derived by `set_drift`: each step's departure from the mean, a
    /// √second.
    pub noise: Vec<f64>,
    /// Derived by `set_levels`: the mean value at each hour of the day,
    /// which the value reverts to, when the history holds enough of
    /// every hour.
    pub levels: Option<[f64; 24]>,
}

const HOUR: i64 = 3_600;
const DAY: i64 = 86_400;

/// The drift steps of a venture's metric: each change from one plausible
/// report to the next that no lever run is still judged over, before
/// `upto` when given. A run is judged over the reports up to its
/// `expects within`-th after it (the one after it with no `expects`), so
/// a lever that keeps moving the metric over its horizon is not drift.
///
/// Each step is worked out once per index and kept (`DriftMemo`): a step
/// between two reports already in the index never changes, since a run
/// that comes later has a later number than both.
pub fn drift_steps(
    index: &Index,
    venture: &str,
    mandate: &Mandate,
    upto: Option<Seq>,
) -> Vec<Drift> {
    let metric = mandate.metric_name();
    let series = index.series(venture, metric);
    let end = upto.map_or(series.len(), |u| series.partition_point(|(s, _)| *s < u));
    let levers = horizons(mandate);
    let names: Vec<String> = levers.iter().map(|(l, w)| format!("{l} {w}")).collect();
    let key = (venture.to_string(), metric.to_string(), names.join("\n"));
    let mut memos = index.drift_memo().lock().unwrap_or_else(|e| e.into_inner());
    // The memo lives with the index, whose series only ever grow.
    let memo = memos.entry(key).or_default();
    let at = |s: Seq| index.get(s).map_or(0, |e| e.at);
    let want = end.saturating_sub(1);
    for i in memo.windows..want {
        let quiet = levers
            .iter()
            .all(|(l, w)| !judged_over(series, i, index.intents_of(venture, l), *w));
        if quiet {
            let (w0, w1) = (series[i].0, series[i + 1].0);
            memo.steps.push((
                i,
                Drift {
                    at: at(w0),
                    gap: at(w1) - at(w0),
                    change: series[i + 1].1 - series[i].1,
                },
            ));
        }
        memo.windows = i + 1;
    }
    let n = memo.steps.partition_point(|(i, _)| *i < want);
    memo.steps[..n].iter().map(|(_, d)| *d).collect()
}

/// Each permitted lever and how many reports after a run it is judged
/// over: its `expects within`, else 1.
fn horizons(mandate: &Mandate) -> Vec<(&str, usize)> {
    let mut levers: Vec<(&str, usize)> = mandate
        .permitted
        .iter()
        .map(|p| {
            let w = p.expects.as_ref().map_or(1, |e| e.within.max(1) as usize);
            (p.name.as_str(), w)
        })
        .collect();
    // A mandate names each lever once; sorted, the memo's key is stable.
    levers.sort_unstable();
    levers
}

/// Whether the step from report `i` to `i + 1` of `series` falls within
/// `within` reports after the latest of these `runs` before its end.
fn judged_over(series: &[(Seq, f64)], i: usize, runs: &[Seq], within: usize) -> bool {
    let end = series[i + 1].0;
    let Some(&run) = runs[..runs.partition_point(|s| *s < end)].last() else {
        return false;
    };
    // The reports after the run, up to and including the step's end.
    let after = i + 2 - series.partition_point(|(s, _)| *s <= run);
    after <= within
}

/// The drift steps worked out so far for one venture, metric and set of
/// permitted levers: how many windows of the series (pairs of reports in
/// a row) were looked at, and the steps among them, each with its window.
#[derive(Clone, Debug, Default)]
pub struct Memo {
    windows: usize,
    steps: Vec<(usize, Drift)>,
}

/// Every venture's drift steps worked out so far, kept with an index.
/// A copy of the index copies them.
#[derive(Debug, Default)]
pub struct DriftMemo(std::sync::Mutex<std::collections::HashMap<(String, String, String), Memo>>);

impl std::ops::Deref for DriftMemo {
    type Target = std::sync::Mutex<std::collections::HashMap<(String, String, String), Memo>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Clone for DriftMemo {
    fn clone(&self) -> DriftMemo {
        let memos = self.0.lock().unwrap_or_else(|e| e.into_inner());
        DriftMemo(std::sync::Mutex::new(memos.clone()))
    }
}

/// `drift_steps` worked out from the whole series every time: the
/// reference the memo is tested against.
pub fn drift_steps_whole(
    index: &Index,
    venture: &str,
    mandate: &Mandate,
    upto: Option<Seq>,
) -> Vec<Drift> {
    let series = index.series(venture, mandate.metric_name());
    let end = upto.map_or(series.len(), |u| series.partition_point(|(s, _)| *s < u));
    let series = &series[..end];
    let at = |s: Seq| index.get(s).map_or(0, |e| e.at);
    let levers = horizons(mandate);
    let whole = index.series(venture, mandate.metric_name());
    series
        .windows(2)
        .enumerate()
        .filter(|(i, _)| {
            levers
                .iter()
                .all(|(l, w)| !judged_over(whole, *i, index.intents_of(venture, l), *w))
        })
        .map(|(_, w)| Drift {
            at: at(w[0].0),
            gap: at(w[1].0) - at(w[0].0),
            change: w[1].1 - w[0].1,
        })
        .collect()
}

/// How many recent lever-free steps make the local drift a run is judged
/// against.
pub const RECENT: usize = 6;

/// The median of these numbers; None when there are none.
fn median(mut xs: Vec<f64>) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    xs.sort_by(f64::total_cmp);
    let n = xs.len();
    Some(if n % 2 == 1 {
        xs[n / 2]
    } else {
        (xs[n / 2 - 1] + xs[n / 2]) / 2.0
    })
}

/// The mean drift a second at each hour of the day (UTC), learnt from
/// these steps, each spread over the hours it covers: so steps longer
/// than an hour still tell the hours apart. None unless every hour is
/// covered by at least `MIN_SAMPLES` steps.
fn daily_profile(steps: &[Drift]) -> Option<[f64; 24]> {
    let mut change = [0.0; 24];
    let mut time = [0i64; 24];
    let mut count = [0usize; 24];
    for d in steps.iter().filter(|d| d.gap > 0) {
        let mut t = d.at;
        let end = d.at + d.gap;
        while t < end {
            let upto = ((t.div_euclid(HOUR) + 1) * HOUR).min(end);
            let h = hour_of(t);
            change[h] += d.change * (upto - t) as f64 / d.gap as f64;
            time[h] += upto - t;
            count[h] += 1;
            t = upto;
        }
    }
    if count.iter().any(|n| *n < MIN_SAMPLES) {
        return None;
    }
    Some(std::array::from_fn(|h| change[h] / time[h] as f64))
}

/// What the metric would have moved on its own over `dt` seconds from
/// `t0`, and how far it strays from that: the counterfactual a run of a
/// lever is judged against. It is local: the median rate of the latest
/// `RECENT` lever-free steps within the day before `upto`, and the spread
/// of those same steps, so a regime that has changed (a heat wave that
/// ended in a dead plant) does not stand in for now. Where the history
/// before `upto` holds a daily cycle (`daily_profile`), it is taken out
/// of those steps before their median and put back over `dt`: the half
/// of a day the latest steps fell in does not stand in for the whole
/// horizon. With fewer than `MIN_SAMPLES` such steps, the baseline's
/// drift from all the history before `upto`. Either way the drift stops
/// at the mandate's `range:`.
pub fn counterfactual(
    index: &Index,
    venture: &str,
    mandate: &Mandate,
    upto: Seq,
    t0: i64,
    dt: i64,
) -> (f64, f64) {
    let steps = drift_steps(index, venture, mandate, Some(upto));
    let now = index.get(upto).map_or(0, |e| e.at);
    let local: Vec<Drift> = steps
        .iter()
        .rev()
        .filter(|d| d.gap > 0)
        .take_while(|d| d.at >= now - DAY)
        .take(RECENT)
        .copied()
        .collect();
    let (drift, spread) = if local.len() >= MIN_SAMPLES {
        // The history's drift by hour over a span, nothing without it.
        // Its level cancels: what it adds over `dt` the median of the
        // local steps less it takes back, so only its shape is kept.
        let cycle = Baseline {
            by_hour: daily_profile(&steps),
            ..Baseline::default()
        };
        let own = |d: &Drift| d.change - cycle.integrate(d.at, d.gap);
        let rate = median(local.iter().map(|d| own(d) / d.gap as f64).collect()).unwrap_or(0.0);
        // The spread is the steps' own, cycle and all: the cycle taken
        // out is inferred from the history, and a judge that trusts it
        // to narrow the noise would call a lever ineffective on it.
        let raw = median(local.iter().map(|d| d.change / d.gap as f64).collect()).unwrap_or(0.0);
        let spread = median(
            local
                .iter()
                .map(|d| ((d.change - raw * d.gap as f64) / (d.gap as f64).sqrt()).abs())
                .collect(),
        )
        .unwrap_or(0.0);
        (
            rate * dt as f64 + cycle.integrate(t0, dt),
            spread * (dt.max(0) as f64).sqrt(),
        )
    } else {
        let base = Baseline::drift_before(index, venture, mandate, upto);
        (base.integrate(t0, dt), base.spread(dt))
    };
    // The metric cannot drift past its declared range: a stock that would
    // fall a week's demand from 200 falls 200.
    let series = index.series(venture, mandate.metric_name());
    let last = series[..series.partition_point(|(s, _)| *s < upto)].last();
    match (mandate.range, last) {
        (Some([lo, hi]), Some((_, x))) => ((x + drift).clamp(lo.min(*x), hi.max(*x)) - x, spread),
        _ => (drift, spread),
    }
}

impl Baseline {
    /// The drift alone, learned from the history before `upto`: what the
    /// metric does on its own, the counterfactual a lever run is judged
    /// against.
    pub fn drift_before(index: &Index, venture: &str, mandate: &Mandate, upto: Seq) -> Baseline {
        let mut b = Baseline {
            range: mandate.range,
            ..Baseline::default()
        };
        b.set_drift(drift_steps(index, venture, mandate, Some(upto)));
        b
    }

    /// How far the metric strays from its drift over `dt` seconds,
    /// typically: the median residual, scaled as a random walk spreads.
    /// 0 with fewer than `MIN_SAMPLES` steps.
    pub fn spread(&self, dt: i64) -> f64 {
        if self.noise.len() < MIN_SAMPLES || dt <= 0 {
            return 0.0;
        }
        let mut r: Vec<f64> = self.noise.iter().map(|x| x.abs()).collect();
        r.sort_by(f64::total_cmp);
        let n = r.len();
        let median = if n % 2 == 1 {
            r[n / 2]
        } else {
            (r[n / 2 - 1] + r[n / 2]) / 2.0
        };
        median * (dt as f64).sqrt()
    }

    /// Learn the drift and the effects from the ledger.
    pub fn from_index(index: &Index, venture: &str, mandate: &Mandate) -> Baseline {
        let metric = mandate.metric_name();
        let series = index.series(venture, metric);
        let at = |s: Seq| index.get(s).map_or(0, |e| e.at);
        let steps = drift_steps(index, venture, mandate, None);
        let mut gaps: Vec<i64> = series
            .windows(2)
            .map(|w| at(w[1].0) - at(w[0].0))
            .filter(|g| *g > 0)
            .collect();
        gaps.sort_unstable();
        let period = gaps
            .get(gaps.len() / 2)
            .copied()
            .unwrap_or(mandate.cadence.as_secs() as i64)
            .max(1);
        let mut b = Baseline {
            last: series.last().map(|(_, v)| *v),
            peak: index.peak(venture, metric),
            drawdowns: mandate
                .guards
                .iter()
                .filter(|g| g.drawdown.is_some())
                .map(|g| g.name.clone())
                .collect(),
            period,
            range: mandate.range,
            ..Baseline::default()
        };
        b.set_drift(steps);
        let points: Vec<(i64, f64)> = series.iter().map(|(s, v)| (at(*s), *v)).collect();
        b.set_levels(&points);
        // Measured `expectation` reports say what a run did; where one
        // exists it stands for that run.
        let mut said: BTreeMap<Seq, f64> = BTreeMap::new();
        for (s, value) in index.series(venture, EXPECTATION) {
            if let Some(e) = index.get(*s) {
                if e.record.claim() == Claim::Measured {
                    if let Record::Report { cites, .. } = &e.record {
                        for c in cites {
                            said.insert(*c, *value);
                        }
                    }
                }
            }
        }
        for p in &mandate.permitted {
            let within = p.expects.map_or(1, |e| e.within.max(1)) as usize;
            let mut samples = Vec::new();
            for &intent in index.intents_of(venture, &p.name) {
                let Some((outcome, true)) = index.settled(intent) else {
                    continue;
                };
                let i = series.partition_point(|(s, _)| *s < intent);
                let a = series.partition_point(|(s, _)| *s <= outcome);
                let base = i.checked_sub(1).and_then(|i| series.get(i));
                // At the horizon the expectation judge uses: past a span
                // of stale repeats, the first reading the sensor
                // refreshed. A run still waiting on one, or on a history
                // too thin to say, is no sample yet.
                let within = match index.horizon(
                    venture,
                    &p.name,
                    metric,
                    intent,
                    outcome,
                    within as u32,
                    mandate.range,
                ) {
                    crate::expect::Horizon::At(w) => w,
                    crate::expect::Horizon::Waiting | crate::expect::Horizon::Thin => continue,
                };
                let then = series.get(a + within - 1);
                // What the metric would have moved over the horizon anyway.
                let carried = match (base, then) {
                    (Some(b0), Some(t)) => b.integrate(at(b0.0), at(t.0) - at(b0.0)),
                    _ => b.integrate(at(intent), period * within as i64),
                };
                match (said.get(&intent), base, then) {
                    (Some(x), _, _) => samples.push(*x - carried),
                    (None, Some(b0), Some(t)) => samples.push(t.1 - b0.1 - carried),
                    _ => {}
                }
            }
            if !samples.is_empty() {
                b.effects.insert(p.name.clone(), samples);
            }
        }
        b
    }

    /// Learn the drift from these steps: the mean rate, by hour of the day
    /// where the history allows, and the residuals. With fewer than
    /// `MIN_SAMPLES` steps the mean is taken as none: persistence plus
    /// the spread of what there is.
    pub fn set_drift(&mut self, steps: Vec<Drift>) {
        let steps: Vec<Drift> = steps.into_iter().filter(|d| d.gap > 0).collect();
        self.drift = steps.iter().map(|d| d.change).collect();
        let total: i64 = steps.iter().map(|d| d.gap).sum();
        let mean = if total > 0 {
            steps.iter().map(|d| d.change).sum::<f64>() / total as f64
        } else {
            0.0
        };
        let first = steps.iter().map(|d| d.at).min().unwrap_or(0);
        let last = steps.iter().map(|d| d.at + d.gap).max().unwrap_or(0);
        let mut by_hour = None;
        if last - first >= DAY {
            let mut change = [0.0; 24];
            let mut time = [0i64; 24];
            let mut count = [0usize; 24];
            for d in &steps {
                let h = hour_of(d.at);
                change[h] += d.change;
                time[h] += d.gap;
                count[h] += 1;
            }
            if count.iter().all(|n| *n >= MIN_SAMPLES) {
                by_hour = Some(std::array::from_fn(|h| change[h] / time[h] as f64));
            }
        }
        self.by_hour = by_hour;
        self.rate = if steps.len() >= MIN_SAMPLES {
            mean
        } else {
            0.0
        };
        self.noise = steps
            .iter()
            .map(|d| {
                let r = by_hour.map_or(mean, |b| b[hour_of(d.at)]);
                (d.change - r * d.gap as f64) / (d.gap as f64).sqrt()
            })
            .collect();
    }

    /// Learn the mean value at each hour of the day from these readings,
    /// when they span a day and hold at least `MIN_SAMPLES` of every hour.
    pub fn set_levels(&mut self, points: &[(i64, f64)]) {
        self.levels = None;
        let (Some(first), Some(last)) = (
            points.iter().map(|p| p.0).min(),
            points.iter().map(|p| p.0).max(),
        ) else {
            return;
        };
        if last - first < DAY {
            return;
        }
        let mut sum = [0.0; 24];
        let mut count = [0usize; 24];
        for (t, v) in points {
            sum[hour_of(*t)] += v;
            count[hour_of(*t)] += 1;
        }
        if count.iter().all(|n| *n >= MIN_SAMPLES) {
            self.levels = Some(std::array::from_fn(|h| sum[h] / count[h] as f64));
        }
    }

    /// The level at time `t`: between the hours' levels, each taken at
    /// the middle of its hour.
    fn level(&self, levels: &[f64; 24], t: i64) -> f64 {
        let s = (t - HOUR / 2).rem_euclid(DAY);
        let h = (s / HOUR) as usize;
        let f = (s % HOUR) as f64 / HOUR as f64;
        levels[h] + (levels[(h + 1) % 24] - levels[h]) * f
    }

    /// The mean drift over `dt` seconds from `t0`.
    pub fn integrate(&self, t0: i64, dt: i64) -> f64 {
        if dt <= 0 {
            return 0.0;
        }
        let Some(b) = self.by_hour else {
            return self.rate * dt as f64;
        };
        let day: f64 = b.iter().map(|r| r * HOUR as f64).sum();
        let mut sum = (dt / DAY) as f64 * day;
        let mut t = t0 + (dt / DAY) * DAY;
        let end = t0 + dt;
        while t < end {
            let next = ((t.div_euclid(HOUR)) + 1) * HOUR;
            let upto = next.min(end);
            sum += b[hour_of(t)] * (upto - t) as f64;
            t = upto;
        }
        sum
    }

    /// The mean drift a day, from the hours of the day where known.
    pub fn drift_per_day(&self) -> f64 {
        self.integrate(0, DAY)
    }

    fn bound(&self, x: f64) -> f64 {
        match self.range {
            Some([lo, hi]) => x.clamp(lo, hi),
            None => x,
        }
    }
}

/// The hour of the day, UTC, of a time in unix seconds.
fn hour_of(t: i64) -> usize {
    (t.rem_euclid(DAY) / HOUR) as usize
}

impl WorldModel for Baseline {
    fn name(&self) -> &'static str {
        BASELINE
    }

    fn step(
        &self,
        state: &Value,
        action: Option<&Action>,
        seed: u64,
        now: i64,
        dt: i64,
    ) -> Result<Next, String> {
        let mut x = state
            .as_f64()
            .or_else(|| state.get("value").and_then(Value::as_f64))
            .or(self.last)
            .ok_or("the baseline has no state and no history to start from")?;
        let mut peak = state
            .get("peak")
            .and_then(Value::as_f64)
            .or(self.peak)
            .unwrap_or(x)
            .max(x);
        let mut rng = mix(seed, now as u64);
        if dt > 0 {
            // A scaled sample, not a sum of resampled steps: one draw
            // whatever dt is, exact for steps of any length, and the mean
            // and spread of the sum of independent steps covering dt.
            x += self.integrate(now, dt);
            if !self.noise.is_empty() {
                let i = (splitmix64(&mut rng) % self.noise.len() as u64) as usize;
                x += self.noise[i] * (dt as f64).sqrt();
            }
            // A cyclic metric reverts to its level for the hour.
            if let Some(levels) = &self.levels {
                let to = self.level(levels, now + dt);
                x = to + (x - to) * (-(dt as f64) / DAY as f64).exp();
            }
            x = self.bound(x);
            peak = peak.max(x);
        }
        if let Some(a) = action {
            let samples = self.effects.get(&a.name).map_or(&[][..], Vec::as_slice);
            // Fewer than MIN_SAMPLES runs: no effect known, persistence.
            if samples.len() >= MIN_SAMPLES {
                x += samples[(splitmix64(&mut rng) % samples.len() as u64) as usize];
            }
            x = self.bound(x);
            peak = peak.max(x);
        }
        // A drawdown as the kernel reads one: percent below the peak.
        let guards = if peak > 0.0 {
            self.drawdowns
                .iter()
                .map(|g| (g.clone(), (peak - x) / peak * 100.0))
                .collect()
        } else {
            BTreeMap::new()
        };
        Ok(Next {
            state: serde_json::json!({ "value": x, "peak": peak }),
            metric: x,
            guards,
        })
    }
}

// ----- a world command -----

/// A venture's `world:` command.
pub struct Command {
    pub dir: PathBuf,
    pub cmd: String,
    pub history: Vec<Point>,
}

#[derive(Serialize)]
struct Input<'a> {
    state: &'a Value,
    action: Option<&'a Action>,
    seed: u64,
    now: i64,
    dt: i64,
    history: &'a [Point],
}

impl WorldModel for Command {
    fn name(&self) -> &'static str {
        WORLD
    }

    fn step(
        &self,
        state: &Value,
        action: Option<&Action>,
        seed: u64,
        now: i64,
        dt: i64,
    ) -> Result<Next, String> {
        let input = serde_json::to_string(&Input {
            state,
            action,
            seed,
            now,
            dt,
            history: &self.history,
        })
        .map_err(|e| e.to_string())?;
        let ran = crate::warden::run_bounded(
            &self.dir,
            "sh",
            &["-c", &self.cmd],
            &[],
            &input,
            TIMEOUT,
            MAX_OUTPUT + 1,
        );
        if !ran.ok {
            return Err(format!("world failed: {}", ran.detail(TIMEOUT)));
        }
        parse_output(&ran.stdout)
    }
}

/// Read a world command's reply: `state`, a finite `metric`, and optional
/// `guards` by name. Anything else is dropped.
pub fn parse_output(text: &str) -> Result<Next, String> {
    if text.len() > MAX_OUTPUT {
        return Err(format!(
            "world replied with more than {MAX_OUTPUT} bytes; the most a reply may hold is {MAX_OUTPUT}"
        ));
    }
    let value: Value =
        serde_json::from_str(text.trim()).map_err(|e| format!("world reply is not JSON: {e}"))?;
    let Value::Object(map) = value else {
        return Err("world reply is not a set of fields".to_string());
    };
    let state = map
        .get("state")
        .cloned()
        .ok_or("world reply has no state")?;
    let metric = map
        .get("metric")
        .and_then(Value::as_f64)
        .filter(|x| x.is_finite())
        .ok_or("world reply has no finite metric")?;
    let mut guards = BTreeMap::new();
    match map.get("guards") {
        None | Some(Value::Null) => {}
        Some(Value::Object(g)) => {
            if g.len() > MAX_GUARDS {
                return Err(format!("world reply has more than {MAX_GUARDS} guards"));
            }
            for (name, v) in g {
                let ok_name = !name.is_empty()
                    && name.len() <= 64
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
                let x = v.as_f64().filter(|x| x.is_finite());
                match (ok_name, x) {
                    (true, Some(x)) => {
                        guards.insert(name.clone(), x);
                    }
                    _ => return Err(format!("world reply has a bad guard {name:?}")),
                }
            }
        }
        Some(_) => return Err("world reply's guards are not a set of fields".to_string()),
    }
    Ok(Next {
        state,
        metric,
        guards,
    })
}

// ----- ensembles -----

/// The seed of ensemble member `i` of a forecast seeded `base`.
pub fn member_seed(base: u64, i: u64) -> u64 {
    mix(base, i)
}

/// Step `members` ensemble members: the same model, a seed each.
pub fn ensemble(
    model: &dyn WorldModel,
    state: &Value,
    action: Option<&Action>,
    base: u64,
    now: i64,
    dt: i64,
    members: u64,
) -> Result<Vec<Next>, String> {
    (0..members)
        .map(|i| model.step(state, action, member_seed(base, i), now, dt))
        .collect()
}

/// The quantiles of a sample, by linear interpolation between order
/// statistics. None for an empty sample.
pub fn quantiles(values: &[f64]) -> Option<Quantiles> {
    let mut v: Vec<f64> = values.iter().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let at = |q: f64| {
        let h = (v.len() - 1) as f64 * q;
        let lo = h.floor() as usize;
        let hi = h.ceil() as usize;
        v[lo] + (v[hi] - v[lo]) * (h - lo as f64)
    };
    Some(Quantiles::from_values(Quantiles::LEVELS.map(at)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::Proof;
    use crate::record::{Entry, GENESIS};

    /// examples/greenhouse/report's curve: 18 at 02:00 UTC, 34 at 14:00,
    /// to one decimal.
    fn greenhouse_at(now: i64) -> f64 {
        let m = (now % 86_400) / 60;
        let mut d = (m - 840).abs();
        if d > 720 {
            d = 1440 - d;
        }
        let t = 18.0 + 16.0 * (720 - d) as f64 / 720.0;
        (t * 10.0).round() / 10.0
    }

    const MIDNIGHT: i64 = 1_767_225_600; // 2026-01-01T00:00:00Z

    /// `days` of greenhouse reports a minute apart, from midnight.
    fn greenhouse(days: i64, range: &str) -> (Index, Mandate) {
        let m = Mandate::parse(&format!(
            "purpose: p\ncontroller: {{kind: model}}\nmetric: temperature\ncadence: 1m\n{range}"
        ))
        .unwrap();
        let mut entries: Vec<Entry> = Vec::new();
        for i in 0..days * 1440 {
            let at = MIDNIGHT + i * 60;
            let prev = entries
                .last()
                .map_or(GENESIS.to_string(), |e| e.hash.clone());
            let r = Record::report(
                Claim::Measured,
                V,
                "temperature",
                Some(greenhouse_at(at)),
                "",
            );
            entries.push(Entry::new(entries.len() as Seq + 1, at, &prev, r));
        }
        (Index::from_entries(&entries), m)
    }

    /// Roll every member from the end of the history, `dt` a step, to
    /// `days` on; the quantiles of where they end.
    fn roll(b: &Baseline, from: i64, days: i64, dt: i64, members: u64) -> Quantiles {
        let mut ends = Vec::new();
        for i in 0..members {
            let mut state = Value::Null;
            let mut t = from;
            let mut x = f64::NAN;
            for s in 0..days * 86_400 / dt {
                let next = b
                    .step(&state, None, member_seed(9, i) ^ s as u64, t, dt)
                    .unwrap();
                x = next.metric;
                state = next.state;
                t += dt;
            }
            ends.push(x);
        }
        quantiles(&ends).unwrap()
    }

    #[test]
    fn the_greenhouse_forecast_stays_in_its_band_for_thirty_days() {
        let (index, m) = greenhouse(2, "");
        let b = Baseline::from_index(&index, V, &m);
        assert!(b.by_hour.is_some() && b.levels.is_some(), "a daily cycle");
        let from = MIDNIGHT + 2 * 86_400 - 60;
        let q = roll(&b, from, 30, 3600, 50);
        // The curve runs 18 to 34; a margin of 2 either side.
        for v in q.values() {
            assert!((16.0..=36.0).contains(&v), "{q:?}");
        }
        // At 23:59 the curve reads 20.7.
        assert!((q.p50 - greenhouse_at(from)).abs() < 1.5, "{q:?}");
    }

    #[test]
    fn under_a_day_of_history_the_drift_is_not_by_hour() {
        let (index, m) = greenhouse(1, "");
        let b = Baseline::from_index(&index, V, &m);
        assert!(b.by_hour.is_none() && b.levels.is_none());
    }

    #[test]
    fn a_step_moves_by_the_drift_of_the_time_it_covers() {
        // A steady 2 an hour, learnt from hourly reports.
        let mut b = Baseline {
            last: Some(10.0),
            period: 3600,
            ..Baseline::default()
        };
        b.set_drift(Drift::every(3600, &[2.0; 6]));
        let ten = b.step(&Value::Null, None, 1, 0, 10 * 3600).unwrap();
        assert!((ten.metric - 30.0).abs() < 1e-9, "{}", ten.metric);
        // Learnt from reports a minute apart, the same rate.
        let mut fine = b.clone();
        fine.set_drift(Drift::every(60, &[2.0 / 60.0; 600]));
        let hour = fine.step(&Value::Null, None, 1, 0, 3600).unwrap();
        assert!((hour.metric - 12.0).abs() < 1e-9, "{}", hour.metric);
        // Reports of mixed gaps give the rate a second, not a step.
        let mut mixed = b.clone();
        let mut steps = Drift::every(60, &[2.0 / 60.0; 60]);
        steps.extend(Drift::every(3600, &[2.0; 3]).into_iter().map(|d| Drift {
            at: d.at + 3600,
            ..d
        }));
        mixed.set_drift(steps);
        let day = mixed.step(&Value::Null, None, 1, 0, 86_400).unwrap();
        assert!((day.metric - 58.0).abs() < 1e-9, "{}", day.metric);
        // The greenhouse: twelve hours on from 02:00 is the afternoon peak.
        let (index, m) = greenhouse(2, "");
        let g = Baseline::from_index(&index, V, &m);
        let at2 = MIDNIGHT + 2 * 86_400 + 2 * 3600;
        let members: Vec<f64> = (0..50)
            .map(|i| {
                g.step(&Value::from(18.0), None, member_seed(3, i), at2, 12 * 3600)
                    .unwrap()
                    .metric
            })
            .collect();
        let q = quantiles(&members).unwrap();
        assert!((q.p50 - 34.0).abs() < 1.5, "{q:?}");
    }

    #[test]
    fn the_range_bounds_every_step() {
        let (_, m) = greenhouse(1, "range: [10, 40]\n");
        let (index, _) = greenhouse(1, "");
        assert_eq!(
            Baseline::from_index(&index, V, &m).range,
            Some([10.0, 40.0])
        );
        let mut b = Baseline {
            last: Some(90.0),
            period: 3600,
            range: Some([0.0, 100.0]),
            ..Baseline::default()
        };
        b.set_drift(Drift::every(3600, &[1.0, 3.0, 2.0, 2.5, 1.5, 2.0]));
        let ends: Vec<f64> = (0..50)
            .map(|i| {
                b.step(&Value::Null, None, member_seed(5, i), 0, 86_400)
                    .unwrap()
                    .metric
            })
            .collect();
        let q = quantiles(&ends).unwrap();
        assert_eq!(q.p95, 100.0, "{q:?}");
        assert!(ends.iter().all(|x| (0.0..=100.0).contains(x)), "{ends:?}");
        // A lever's effect is bounded too.
        b.effects.insert("boost".to_string(), vec![50.0; 3]);
        let boost = Action {
            name: "boost".to_string(),
            payload: None,
        };
        let after = b.step(&Value::Null, Some(&boost), 1, 0, 0).unwrap();
        assert_eq!(after.metric, 100.0);
    }

    const V: &str = "shop";

    fn mandate() -> Mandate {
        Mandate::parse(
            "purpose: p\nmetric: count\ncontroller: {kind: model}\ncadence: 1h\n\
             permitted:\n- {name: boost, run: ./boost, cap: 9/day, reversible: true}\n",
        )
        .unwrap()
    }

    struct Ledger {
        entries: Vec<Entry>,
        at: i64,
    }

    impl Ledger {
        fn push(&mut self, record: Record) -> Seq {
            self.at += 3600;
            let seq = self.entries.len() as Seq + 1;
            let prev = self
                .entries
                .last()
                .map_or(GENESIS.to_string(), |e| e.hash.clone());
            self.entries.push(Entry::new(seq, self.at, &prev, record));
            seq
        }

        fn report(&mut self, x: f64) -> Seq {
            self.push(Record::report(Claim::Measured, V, "count", Some(x), ""))
        }

        /// A run at the time of the last report: intent and outcome take
        /// no time of their own.
        fn run(&mut self, lever: &str) {
            self.at -= 3600;
            let i = self.push(Record::Intent {
                claim: Claim::Measured,
                venture: V.to_string(),
                action: lever.to_string(),
                proof: Proof {
                    venture: V.to_string(),
                    action: lever.to_string(),
                    steps: vec![],
                },
                payload: None,
                key: None,
            });
            self.at -= 3600;
            self.push(Record::Outcome {
                claim: Claim::Measured,
                venture: V.to_string(),
                intent: i,
                ok: true,
                detail: String::new(),
            });
        }
    }

    /// A metric that wanders by small steps, with `boost` runs that each
    /// add `effect`.
    fn history(effect: f64) -> Index {
        let mut l = Ledger {
            entries: vec![],
            at: 1_800_000_000,
        };
        let wander = [0.4, -0.3, 0.2, -0.5, 0.1, 0.3, -0.2, 0.0, -0.1, 0.4];
        let mut x = 100.0;
        for round in 0..6 {
            for w in wander {
                x += w;
                l.report(x);
            }
            l.run("boost");
            x += effect + wander[round];
            l.report(x);
        }
        Index::from_entries(&l.entries)
    }

    #[test]
    fn splitmix64_matches_the_reference() {
        // The first outputs for seed 0, from the reference implementation.
        let mut s = 0u64;
        assert_eq!(splitmix64(&mut s), 0xE220_A839_7B1D_CDAF);
        assert_eq!(splitmix64(&mut s), 0x6E78_9E6A_A1B9_65F4);
    }

    #[test]
    fn the_baseline_is_deterministic_under_a_seed_and_differs_across_seeds() {
        let index = history(10.0);
        let b = Baseline::from_index(&index, V, &mandate());
        let state = Value::from(120.0);
        let boost = Action {
            name: "boost".to_string(),
            payload: None,
        };
        for seed in 0..20 {
            let one = b.step(&state, Some(&boost), seed, 1000, 3600).unwrap();
            let two = b.step(&state, Some(&boost), seed, 1000, 3600).unwrap();
            assert_eq!(one, two, "seed {seed}");
        }
        let outcomes: std::collections::BTreeSet<u64> = (0..20)
            .map(|seed| {
                b.step(&state, None, seed, 1000, 3600)
                    .unwrap()
                    .metric
                    .to_bits()
            })
            .collect();
        assert!(outcomes.len() > 3, "{outcomes:?}");
        // The same seed through an ensemble gives the same members.
        let a = ensemble(&b, &state, None, 7, 1000, 3600, 10).unwrap();
        let c = ensemble(&b, &state, None, 7, 1000, 3600, 10).unwrap();
        let d = ensemble(&b, &state, None, 8, 1000, 3600, 10).unwrap();
        assert_eq!(a, c);
        assert_ne!(a, d);
    }

    #[test]
    fn the_baseline_p50_after_a_lever_is_the_measured_effect_within_noise() {
        let index = history(10.0);
        let b = Baseline::from_index(&index, V, &mandate());
        assert_eq!(b.effects["boost"].len(), 6, "{b:?}");
        let x = b.last.unwrap();
        let boost = Action {
            name: "boost".to_string(),
            payload: None,
        };
        let members = ensemble(&b, &Value::from(x), Some(&boost), 1, 5000, 0, 200).unwrap();
        let q = quantiles(&members.iter().map(|n| n.metric).collect::<Vec<_>>()).unwrap();
        let series: Vec<f64> = index.series(V, "count").iter().map(|(_, v)| *v).collect();
        let noise = crate::expect::noise(&series[..10]);
        assert!(noise > 0.0);
        assert!(
            (q.p50 - x - 10.0).abs() <= noise,
            "p50 {} from {x}: an effect of {} against 10 and noise {noise}",
            q.p50,
            q.p50 - x
        );
        // With the lever run a step later, a drift is added too.
        let later = ensemble(&b, &Value::from(x), Some(&boost), 1, 5000, 3600, 200).unwrap();
        let q = quantiles(&later.iter().map(|n| n.metric).collect::<Vec<_>>()).unwrap();
        assert!((q.p50 - x - 10.0).abs() <= 2.0 * noise, "{q:?}");
        // No lever: the median stays near where it is.
        let still = ensemble(&b, &Value::from(x), None, 1, 5000, 3600, 200).unwrap();
        let q = quantiles(&still.iter().map(|n| n.metric).collect::<Vec<_>>()).unwrap();
        assert!((q.p50 - x).abs() <= noise, "{q:?}");
    }

    /// One thing that happens to the ledger, or a look at the drift.
    #[derive(Clone, Debug)]
    enum Op {
        Report(f64),
        Implausible(f64),
        Run(&'static str),
        Look(Option<u8>),
        Copy,
    }

    fn op() -> impl proptest::strategy::Strategy<Value = Op> {
        use proptest::prelude::*;
        prop_oneof![
            4 => (-5.0..5.0f64).prop_map(Op::Report),
            1 => (-5.0..5.0f64).prop_map(Op::Implausible),
            2 => prop::sample::select(vec!["boost", "other"]).prop_map(Op::Run),
            2 => prop::option::of(any::<u8>()).prop_map(Op::Look),
            1 => Just(Op::Copy),
        ]
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(300))]

        /// The drift steps kept with an index are the ones worked out
        /// from the whole series, however the index grew, whichever
        /// levers the mandate permits, up to any report, in any copy.
        #[test]
        fn kept_drift_steps_are_the_whole_series_drift_steps(
            ops in proptest::collection::vec(op(), 1..80),
        ) {
            let both = Mandate::parse(
                "purpose: p\nmetric: count\ncontroller: {kind: model}\ncadence: 1h\n\
                 permitted:\n- {name: boost, run: ./boost, cap: 9/day, reversible: true}\n\
                 - {name: other, run: ./other, cap: 9/day, reversible: true}\n",
            )
            .unwrap();
            let horizon = Mandate::parse(
                "purpose: p\nmetric: count\ncontroller: {kind: model}\ncadence: 1h\n\
                 permitted:\n- {name: boost, run: ./boost, cap: 9/day, reversible: true, \
                 expects: up within 3}\n\
                 - {name: other, run: ./other, cap: 9/day, reversible: true}\n",
            )
            .unwrap();
            let mandates = [mandate(), both, horizon];
            let mut l = Ledger { entries: vec![], at: 1_800_000_000 };
            let mut index = Index::default();
            let mut x = 0.0;
            for op in ops {
                match op {
                    Op::Report(d) => {
                        x += d;
                        l.report(x);
                    }
                    Op::Implausible(d) => {
                        let mut r = Record::report(Claim::Measured, V, "count", Some(x + d), "");
                        if let Record::Report { implausible, .. } = &mut r {
                            *implausible = Some("a test".to_string());
                        }
                        l.push(r);
                    }
                    Op::Run(lever) => l.run(lever),
                    Op::Copy => index = index.clone(),
                    Op::Look(upto) => {
                        for e in &l.entries[index.last() as usize..] {
                            index.apply(e.clone());
                        }
                        let upto = upto.map(|u| Seq::from(u) % (index.last() + 2));
                        for m in &mandates {
                            proptest::prop_assert_eq!(
                                drift_steps(&index, V, m, upto),
                                drift_steps_whole(&index, V, m, upto)
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn drift_skips_steps_with_a_run_between_them() {
        let index = history(10.0);
        let b = Baseline::from_index(&index, V, &mandate());
        assert!(b.drift.iter().all(|d| d.abs() < 1.0), "{:?}", b.drift);
        assert_eq!(b.period, 3600);
    }

    /// A metric that climbs a steady 2 a report, with `boost` runs that
    /// each move it 7 over the one report after: 5 of their own.
    fn steady(within: Option<u32>) -> (Index, Mandate) {
        let expects = within.map_or(String::new(), |w| format!(", expects: up within {w}"));
        let m = Mandate::parse(&format!(
            "purpose: p\nmetric: count\ncontroller: {{kind: model}}\ncadence: 1h\n\
             permitted:\n- {{name: boost, run: ./boost, cap: 9/day, reversible: true{expects}}}\n"
        ))
        .unwrap();
        let mut l = Ledger {
            entries: vec![],
            at: 1_800_000_000,
        };
        let mut x = 100.0;
        for _ in 0..5 {
            for _ in 0..4 {
                x += 2.0;
                l.report(x);
            }
            l.run("boost");
            x += 7.0;
            l.report(x);
            // The rest of the horizon, if any, drifts as ever.
            for _ in 1..within.unwrap_or(1) {
                x += 2.0;
                l.report(x);
            }
        }
        (Index::from_entries(&l.entries), m)
    }

    fn p50_after(b: &Baseline, x: f64, dt: i64) -> f64 {
        let boost = Action {
            name: "boost".to_string(),
            payload: None,
        };
        let members = ensemble(b, &Value::from(x), Some(&boost), 4, 9000, dt, 100).unwrap();
        quantiles(&members.iter().map(|n| n.metric).collect::<Vec<_>>())
            .unwrap()
            .p50
    }

    #[test]
    fn drift_is_counted_once_on_a_lever_step() {
        let (index, m) = steady(None);
        let b = Baseline::from_index(&index, V, &m);
        assert!(b.drift.iter().all(|d| *d == 2.0), "{:?}", b.drift);
        // The runs moved it 7 over a step that drifts 2: 5 is the lever's.
        assert!(b.effects["boost"].iter().all(|e| *e == 5.0), "{b:?}");
        let x = b.last.unwrap();
        let moved = p50_after(&b, x, b.period) - x;
        assert!(
            (moved - 7.0).abs() < 1e-9,
            "one step after the lever: {moved:+}, not 9"
        );
        // No time passing: the lever's own 5.
        assert!((p50_after(&b, x, 0) - x - 5.0).abs() < 1e-9);
    }

    #[test]
    fn over_a_longer_horizon_drift_is_still_counted_once_a_step() {
        // The lever's runs are judged three reports on: 7 + 2 + 2 = 11
        // measured, of which 6 is three steps of drift.
        let (index, m) = steady(Some(3));
        let b = Baseline::from_index(&index, V, &m);
        assert!(b.effects["boost"].iter().all(|e| *e == 5.0), "{b:?}");
        let x = b.last.unwrap();
        let moved = p50_after(&b, x, 3 * b.period) - x;
        assert!((moved - 11.0).abs() < 1e-9, "three steps after: {moved:+}");
    }

    #[test]
    fn a_lever_still_moving_the_metric_within_its_horizon_is_not_drift() {
        // Each run moves it 5 more on every one of the three reports it
        // is judged over: 7, 7, 7, where drift alone is 2.
        let m = Mandate::parse(
            "purpose: p\nmetric: count\ncontroller: {kind: model}\ncadence: 1h\n\
             permitted:\n- {name: boost, run: ./boost, cap: 9/day, reversible: true, \
             expects: up within 3}\n",
        )
        .unwrap();
        let mut l = Ledger {
            entries: vec![],
            at: 1_800_000_000,
        };
        let mut x = 100.0;
        for _ in 0..5 {
            for _ in 0..4 {
                x += 2.0;
                l.report(x);
            }
            l.run("boost");
            for _ in 0..3 {
                x += 7.0;
                l.report(x);
            }
        }
        let index = Index::from_entries(&l.entries);
        let steps = drift_steps(&index, V, &m, None);
        assert!(steps.iter().all(|d| d.change == 2.0), "{steps:?}");
        assert_eq!(steps, drift_steps_whole(&index, V, &m, None));
        // Without expects only the step across the run is the lever's.
        let bare = mandate();
        assert!(drift_steps(&index, V, &bare, None)
            .iter()
            .any(|d| d.change == 7.0));
    }

    #[test]
    fn a_drawdown_guard_is_computed_along_the_rolled_path() {
        let m = Mandate::parse(
            "purpose: p\nmetric: count\ncontroller: {kind: model}\ncadence: 1h\n\
             guards:\n- {name: dd, drawdown: 15%, of: count}\n",
        )
        .unwrap();
        let b = Baseline::from_index(&Index::default(), V, &m);
        assert_eq!(b.drawdowns, vec!["dd".to_string()]);
        // 100, up 20, down 19: the path peaks at 120 and ends at 101.
        let mut up = b.clone();
        up.set_drift(Drift::every(3600, &[20.0; 3]));
        let mut down = b.clone();
        down.set_drift(Drift::every(3600, &[-19.0; 3]));
        let first = up.step(&Value::from(100.0), None, 1, 0, 3600).unwrap();
        assert_eq!(first.metric, 120.0);
        assert_eq!(first.guards["dd"], 0.0);
        let then = down.step(&first.state, None, 1, 3600, 3600).unwrap();
        assert_eq!(then.metric, 101.0);
        let dd = then.guards["dd"];
        assert!((dd - 19.0 / 120.0 * 100.0).abs() < 1e-9, "{dd}");
        assert!((dd - 15.8).abs() < 0.05, "{dd}");
        // Past 15%: the guard would trip on this path.
        assert!(m.guard("dd").unwrap().breached(dd));
        // The peak the ledger holds counts too.
        let mut l = Ledger {
            entries: vec![],
            at: 1_800_000_000,
        };
        for x in [100.0, 120.0, 101.0] {
            l.report(x);
        }
        let from_ledger = Baseline::from_index(&Index::from_entries(&l.entries), V, &m);
        let now = from_ledger.step(&Value::Null, None, 1, 0, 0).unwrap();
        assert!((now.guards["dd"] - 19.0 / 120.0 * 100.0).abs() < 1e-9);
    }

    /// Reports every `step` hours of `f` of the hours since the first,
    /// up to `hours`, then a run of `boost`; the run's intent, which a
    /// run is judged from, and the time of the last report before it.
    fn series(step: i64, hours: i64, f: impl Fn(i64) -> f64) -> (Index, Seq, i64) {
        let mut l = Ledger {
            entries: vec![],
            at: 1_800_000_000,
        };
        for h in (0..=hours).step_by(step as usize) {
            if h > 0 {
                l.at += (step - 1) * HOUR;
            }
            l.report(f(h));
        }
        let now = l.at;
        l.run("boost");
        let upto = l.entries[l.entries.len() - 2].seq;
        (Index::from_entries(&l.entries), upto, now)
    }

    /// No trend, a swing of ±50 a day.
    fn daily(h: i64) -> f64 {
        100.0 + 50.0 * (std::f64::consts::TAU * h as f64 / 24.0).cos()
    }

    #[test]
    fn a_daily_cycle_is_not_a_trend_over_the_horizon() {
        let m = mandate();
        // Read every 2 hours, judged at a trough after the falling half
        // of the day: those steps all fall, and still whole days net
        // nothing, and half a day climbs back to the peak.
        for step in [1, 2] {
            let (index, upto, now) = series(step, 3 * 24 + 12, daily);
            let (drift, spread) = counterfactual(&index, V, &m, upto, now, 3 * DAY);
            assert!(drift.abs() < 1.0, "every {step}h, over 3 days: {drift:+}");
            // The noise is still the steps' own: the cycle is inferred.
            assert!(spread > 10.0, "every {step}h: {spread}");
            let (half, _) = counterfactual(&index, V, &m, upto, now, DAY / 2);
            assert!(
                (half - 100.0).abs() < 5.0,
                "every {step}h, half a day: {half:+}"
            );
        }
        // A sawtooth, up 1 an hour for 20 hours and down 5 an hour for
        // 4: most steps rise, and still a day nets nothing.
        let saw = |h: i64| {
            let x = h % 24;
            if x <= 20 {
                x as f64
            } else {
                20.0 - 5.0 * (x - 20) as f64
            }
        };
        let (index, upto, now) = series(1, 3 * 24, saw);
        let (drift, _) = counterfactual(&index, V, &m, upto, now, 3 * DAY);
        assert!(drift.abs() < 1.0, "a sawtooth over 3 days: {drift:+}");
        // Two days hold under `MIN_SAMPLES` of each hour: no cycle is
        // known, and the latest steps' fall stands, as it always has.
        let (index, upto, now) = series(1, 36, daily);
        let (drift, _) = counterfactual(&index, V, &m, upto, now, 3 * DAY);
        assert!(drift < -500.0, "{drift:+}");
    }

    #[test]
    fn the_drift_a_run_is_judged_against_stays_in_the_range() {
        let m = Mandate::parse(
            "purpose: p\nmetric: count\ncontroller: {kind: model}\ncadence: 1h\n\
             range: [0, 1000]\n\
             permitted:\n- {name: boost, run: ./boost, cap: 9/day, reversible: true}\n",
        )
        .unwrap();
        let stock = |h: i64| 600.0 - 100.0 * (h / 24) as f64;
        // A stock counted once a day and read every 6 hours, down 100 a
        // day to 200: on its own it falls 200 over 5 days, not 500.
        let (index, upto, now) = series(6, 4 * 24, stock);
        let (drift, _) = counterfactual(&index, V, &m, upto, now, 5 * DAY);
        assert_eq!(drift, -200.0);
        // Read once a day: too few steps within a day for the local drift.
        let (index, upto, now) = series(24, 4 * 24, stock);
        let (drift, _) = counterfactual(&index, V, &m, upto, now, 5 * DAY);
        assert_eq!(drift, -200.0);
        // A reading past the range, as one from before the range was
        // narrowed: its drift is not cut to the range at once.
        let (index, upto, now) = series(6, 4 * 24, |h| 1000.0 + stock(h));
        let (drift, _) = counterfactual(&index, V, &m, upto, now, DAY);
        assert_eq!(drift, -100.0);
    }

    #[test]
    fn a_regime_change_under_a_daily_cycle_is_seen_within_hours() {
        let m = mandate();
        // The same cycle, falling a further 10 an hour since 12 hours
        // ago: the local steps say so, and the cycle is not counted in.
        let fall = |h: i64| daily(h) - 10.0 * (h - 72).max(0) as f64;
        let (index, upto, now) = series(2, 3 * 24 + 12, fall);
        let (drift, _) = counterfactual(&index, V, &m, upto, now, 3 * DAY);
        assert!(
            (drift + 720.0).abs() < 0.2 * 720.0,
            "10 an hour for 3 days: {drift:+}"
        );
    }

    #[test]
    fn under_three_samples_the_baseline_is_persistence_plus_spread() {
        let b = Baseline {
            drift: vec![1.0, 3.0],
            effects: BTreeMap::from([("boost".to_string(), vec![50.0])]),
            last: Some(10.0),
            period: 60,
            ..Baseline::default()
        };
        let boost = Action {
            name: "boost".to_string(),
            payload: None,
        };
        let members = ensemble(&b, &Value::Null, Some(&boost), 3, 0, 60, 100).unwrap();
        let values: Vec<f64> = members.iter().map(|n| n.metric).collect();
        // One effect sample is not enough: no 50, only 10 plus or minus
        // the spread of the drift about its mean, twice.
        assert!(
            values.iter().all(|v| (*v - 10.0).abs() <= 2.0),
            "{values:?}"
        );
        let q = quantiles(&values).unwrap();
        assert!((q.p50 - 10.0).abs() <= 1.0, "{q:?}");
    }

    #[test]
    fn quantiles_interpolate() {
        let v: Vec<f64> = (0..=100).map(f64::from).collect();
        let q = quantiles(&v).unwrap();
        assert_eq!(q.values(), [5.0, 10.0, 25.0, 50.0, 75.0, 90.0, 95.0]);
        assert!(quantiles(&[]).is_none());
    }

    #[test]
    fn a_world_reply_keeps_only_its_fields() {
        let n = parse_output(
            r#"{"state": {"n": 3}, "metric": 4.5, "guards": {"spend": 2}, "run": "rm -rf /"}"#,
        )
        .unwrap();
        assert_eq!(n.metric, 4.5);
        assert_eq!(n.state, serde_json::json!({"n": 3}));
        assert_eq!(n.guards["spend"], 2.0);
        for bad in [
            "not json",
            "[1]",
            r#"{"metric": 1}"#,
            r#"{"state": null, "metric": "high"}"#,
            r#"{"state": null, "metric": 1, "guards": {"a b": 1}}"#,
            r#"{"state": null, "metric": 1, "guards": [1]}"#,
        ] {
            assert!(parse_output(bad).is_err(), "{bad}");
        }
        let big = format!(r#"{{"state": "{}", "metric": 1}}"#, "x".repeat(MAX_OUTPUT));
        assert!(parse_output(&big).unwrap_err().contains("bytes"));
    }

    #[test]
    fn a_world_command_steps_over_the_protocol() {
        let tmp = tempfile::tempdir().unwrap();
        let w = Command {
            dir: tmp.path().to_path_buf(),
            cmd: "read -r line; echo \"$line\" > seen; echo '{\"state\": 1, \"metric\": 7}'"
                .to_string(),
            history: vec![Point { at: 5, value: 2.0 }],
        };
        let boost = Action {
            name: "boost".to_string(),
            payload: Some("go".to_string()),
        };
        let n = w.step(&Value::Null, Some(&boost), 9, 100, 60).unwrap();
        assert_eq!(n.metric, 7.0);
        let seen: Value =
            serde_json::from_str(&std::fs::read_to_string(tmp.path().join("seen")).unwrap())
                .unwrap();
        assert_eq!(seen["action"]["name"], "boost");
        assert_eq!(seen["action"]["payload"], "go");
        assert_eq!(seen["seed"], 9);
        assert_eq!(seen["dt"], 60);
        assert_eq!(seen["history"][0]["value"], 2.0);
        let failing = Command {
            cmd: "exit 4".to_string(),
            ..w
        };
        assert!(failing
            .step(&Value::Null, None, 0, 0, 0)
            .unwrap_err()
            .contains("world failed"));
    }

    /// A cleanup behind a sensor that refreshes every 5 reports (#53, and
    /// Power 3 of #55): usage grows 2 a report, a run right after a
    /// refresh frees 25, and the next 4 reports repeat the stale reading.
    /// `cap` runs a day at most.
    fn lagging(cap: u32) -> (Index, Mandate) {
        let m = Mandate::parse(&format!(
            "purpose: p\nmetric: count\ncontroller: {{kind: threshold, above: 80, action: cleanup}}\n\
             cadence: 1h\npermitted:\n- {{name: cleanup, run: ./c, cap: {cap}/day, reversible: true, \
             expects: down within 3}}\n"
        ))
        .unwrap();
        let mut l = Ledger {
            entries: vec![],
            at: 1_800_000_000,
        };
        let (mut actual, mut cached) = (10.0, 10.0);
        let mut n = 0;
        let mut report = |l: &mut Ledger, actual: &mut f64, cached: &mut f64| {
            n += 1;
            *actual += 2.0;
            if n % 5 == 0 {
                *cached = *actual;
            }
            l.report(*cached);
        };
        // A history of the sensor's holds before any run.
        for _ in 0..20 {
            report(&mut l, &mut actual, &mut cached);
        }
        for _ in 0..6 {
            l.run("cleanup");
            actual -= 25.0;
            for _ in 0..30 {
                report(&mut l, &mut actual, &mut cached);
            }
        }
        // A last run whose effect the sensor has not shown yet: waiting.
        l.run("cleanup");
        actual -= 25.0;
        for _ in 0..3 {
            report(&mut l, &mut actual, &mut cached);
        }
        (Index::from_entries(&l.entries), m)
    }

    #[test]
    fn a_run_behind_a_lagging_sensor_is_measured_at_the_reading_that_refreshed() {
        let (index, m) = lagging(2);
        let b = Baseline::from_index(&index, V, &m);
        // Each run is measured at the first refreshed reading: 25 freed,
        // net of the drift, not 0 from the stale reading at `within`; the
        // last run, still waiting on a refresh, is no sample yet.
        let effects = &b.effects["cleanup"];
        assert_eq!(effects.len(), 6, "{effects:?}");
        for e in effects {
            assert!(*e < -15.0, "{effects:?}");
        }
        // Two runs a day free more than the usage grows: no false 'out
        // of reach'.
        assert_eq!(crate::forecast::capacity(&index, V, &m), None);
        // One a day does not, and the check still says so.
        let (index, m) = lagging(1);
        assert!(crate::forecast::capacity(&index, V, &m).is_some());
    }
}
