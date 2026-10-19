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

/// The built-in world model. State is the metric value. A step resamples
/// a drift, the change between two reports with no lever run between
/// them; an action adds a resampled effect of that lever, the change from
/// the last report before a run to the report at its `expects` horizon.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Baseline {
    pub drift: Vec<f64>,
    pub effects: BTreeMap<String, Vec<f64>>,
    /// The latest value, the state when none is given.
    pub last: Option<f64>,
    /// Seconds between reports, typically: one drift per period.
    pub period: i64,
}

impl Baseline {
    /// Learn the drift and the effects from the ledger.
    pub fn from_index(index: &Index, venture: &str, mandate: &Mandate) -> Baseline {
        let metric = mandate.metric_name();
        let series = index.series(venture, metric);
        let mut runs: Vec<Seq> = mandate
            .permitted
            .iter()
            .flat_map(|p| index.intents_of(venture, &p.name).iter().copied())
            .collect();
        runs.sort_unstable();
        let drift = series
            .windows(2)
            .filter(|w| {
                let i = runs.partition_point(|s| *s <= w[0].0);
                runs.get(i).is_none_or(|s| *s >= w[1].0)
            })
            .map(|w| w[1].1 - w[0].1)
            .collect();
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
        let mut effects = BTreeMap::new();
        for p in &mandate.permitted {
            let within = p.expects.map_or(1, |e| e.within.max(1)) as usize;
            let mut samples = Vec::new();
            for &intent in index.intents_of(venture, &p.name) {
                let Some((outcome, true)) = index.settled(intent) else {
                    continue;
                };
                if let Some(x) = said.get(&intent) {
                    samples.push(*x);
                    continue;
                }
                let b = series.partition_point(|(s, _)| *s < intent);
                let a = series.partition_point(|(s, _)| *s <= outcome);
                if let (Some(base), Some(then)) = (
                    b.checked_sub(1).and_then(|i| series.get(i)),
                    series.get(a + within - 1),
                ) {
                    samples.push(then.1 - base.1);
                }
            }
            if !samples.is_empty() {
                effects.insert(p.name.clone(), samples);
            }
        }
        let mut gaps: Vec<i64> = series
            .windows(2)
            .filter_map(|w| Some(index.get(w[1].0)?.at - index.get(w[0].0)?.at))
            .filter(|g| *g > 0)
            .collect();
        gaps.sort_unstable();
        let period = gaps
            .get(gaps.len() / 2)
            .copied()
            .unwrap_or(mandate.cadence.as_secs() as i64)
            .max(1);
        Baseline {
            drift,
            effects,
            last: series.last().map(|(_, v)| *v),
            period,
        }
    }

    /// One draw from `samples`, or with fewer than `MIN_SAMPLES`, from
    /// the drift's spread about nothing: persistence.
    fn draw(&self, samples: &[f64], rng: &mut u64) -> f64 {
        if samples.len() >= MIN_SAMPLES {
            return samples[(splitmix64(rng) % samples.len() as u64) as usize];
        }
        if self.drift.is_empty() {
            return 0.0;
        }
        let mean = self.drift.iter().sum::<f64>() / self.drift.len() as f64;
        let i = (splitmix64(rng) % self.drift.len() as u64) as usize;
        self.drift[i] - mean
    }
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
            .or(self.last)
            .ok_or("the baseline has no state and no history to start from")?;
        let mut rng = mix(seed, now as u64);
        let steps = if dt <= 0 {
            0
        } else {
            ((dt as f64 / self.period as f64).round() as i64).max(1)
        };
        for _ in 0..steps.min(10_000) {
            x += self.draw(&self.drift, &mut rng);
        }
        if let Some(a) = action {
            let samples = self.effects.get(&a.name).map_or(&[][..], Vec::as_slice);
            x += if samples.len() >= MIN_SAMPLES {
                self.draw(samples, &mut rng)
            } else if steps == 0 {
                // No effect known, and no time passing to spread over.
                0.0
            } else {
                self.draw(&[], &mut rng)
            };
        }
        Ok(Next {
            state: Value::from(x),
            metric: x,
            guards: BTreeMap::new(),
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

        fn run(&mut self, lever: &str) {
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

    #[test]
    fn drift_skips_steps_with_a_run_between_them() {
        let index = history(10.0);
        let b = Baseline::from_index(&index, V, &mandate());
        assert!(b.drift.iter().all(|d| d.abs() < 1.0), "{:?}", b.drift);
        assert_eq!(b.period, 3600);
    }

    #[test]
    fn under_three_samples_the_baseline_is_persistence_plus_spread() {
        let b = Baseline {
            drift: vec![1.0, 3.0],
            effects: BTreeMap::from([("boost".to_string(), vec![50.0])]),
            last: Some(10.0),
            period: 60,
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
}
