//! Facts: the numbers of a summary, computed by code from the records it
//! stands for. A model writes the narrative; it never supplies a number.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::record::{Entry, Record, Verdict};

/// Decimals kept in a fact: enough for any metric avsm reads, few enough
/// that float noise never shows.
const DECIMALS: f64 = 1e6;

/// What happened to one metric over the span: plausible measured or
/// declared readings only.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetricFacts {
    pub last: f64,
    pub min: f64,
    pub max: f64,
    /// How many plausible readings the span held.
    pub readings: u32,
}

/// The counts and ranges of a span of records.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Facts {
    pub metrics: BTreeMap<String, MetricFacts>,
    /// Outcomes that went ok (and 1.0 actions that did).
    pub actions: u32,
    /// Refusals, each weighted by the repeats it counted.
    pub refusals: u32,
    pub denials: u32,
    pub asks: u32,
    /// Readings code did not believe, kept out of `metrics`.
    pub implausible: u32,
}

fn round(x: f64) -> f64 {
    (x * DECIMALS).round() / DECIMALS
}

impl Facts {
    /// The facts of `entries`, oldest first. A summary among them (the
    /// previous one) contributes its facts block and never its narrative.
    pub fn compute(entries: &[Entry]) -> Facts {
        let mut f = Facts::default();
        for e in entries {
            match &e.record {
                Record::Report {
                    facts: Some(prev), ..
                } => f.merge(prev),
                Record::Report {
                    claim,
                    metric,
                    value,
                    implausible,
                    ..
                } if claim.authorizes() => {
                    if implausible.is_some() {
                        f.implausible += 1;
                    } else if let Some(x) = value.filter(|x| x.is_finite()) {
                        f.read(metric, round(x), 1);
                    }
                }
                Record::Outcome { ok: true, .. } | Record::Action { ok: true, .. } => {
                    f.actions += 1
                }
                Record::Refusal { count, .. } => f.refusals += *count,
                Record::Decision {
                    verdict: Verdict::Deny { .. },
                    ..
                } => f.denials += 1,
                Record::Ask { .. } => f.asks += 1,
                _ => {}
            }
        }
        f
    }

    fn read(&mut self, metric: &str, x: f64, readings: u32) {
        let m = self
            .metrics
            .entry(metric.to_string())
            .or_insert(MetricFacts {
                last: x,
                min: x,
                max: x,
                readings: 0,
            });
        m.last = x;
        m.min = m.min.min(x);
        m.max = m.max.max(x);
        m.readings += readings;
    }

    /// Fold in the facts of an earlier span: counts add, ranges widen, and
    /// the later span's last stands.
    fn merge(&mut self, earlier: &Facts) {
        let mut base = earlier.clone();
        base.actions += self.actions;
        base.refusals += self.refusals;
        base.denials += self.denials;
        base.asks += self.asks;
        base.implausible += self.implausible;
        for (k, m) in std::mem::take(&mut self.metrics) {
            match base.metrics.get_mut(&k) {
                Some(b) => {
                    b.last = m.last;
                    b.min = b.min.min(m.min);
                    b.max = b.max.max(m.max);
                    b.readings += m.readings;
                }
                None => {
                    base.metrics.insert(k, m);
                }
            }
        }
        *self = base;
    }

    /// The facts as lines for a prompt.
    pub fn lines(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .metrics
            .iter()
            .map(|(k, m)| {
                format!(
                    "{k}: last {}, min {}, max {}, {} readings",
                    m.last, m.min, m.max, m.readings
                )
            })
            .collect();
        out.push(format!(
            "actions {}, refusals {}, denials {}, asks {}, implausible readings {}",
            self.actions, self.refusals, self.denials, self.asks, self.implausible
        ));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{Claim, GENESIS};

    fn e(seq: u64, record: Record) -> Entry {
        Entry::new(seq, seq as i64, GENESIS, record)
    }

    fn reading(claim: Claim, metric: &str, v: Option<f64>, bad: Option<&str>) -> Record {
        let mut r = Record::report(claim, "shop", metric, v, "");
        if let Record::Report { implausible, .. } = &mut r {
            *implausible = bad.map(String::from);
        }
        r
    }

    fn ledger() -> Vec<Entry> {
        let refusal = |count| Record::Refusal {
            claim: Claim::Measured,
            venture: "shop".into(),
            action: "wipe".into(),
            reason: "cap".into(),
            detail: "cap".into(),
            count,
            closes: None,
        };
        let ask = || Record::Ask {
            packet: None,
            claim: Claim::Inferred,
            venture: "shop".into(),
            action: None,
            question: "?".into(),
            refusal: None,
            payload: None,
            signal: None,
            cites: vec![],
        };
        let outcome = |ok| Record::Outcome {
            claim: Claim::Measured,
            venture: "shop".into(),
            intent: 1,
            ok,
            detail: String::new(),
        };
        let records = vec![
            reading(Claim::Measured, "orders", Some(10.0), None),
            reading(Claim::Measured, "orders", Some(4.0), None),
            reading(
                Claim::Measured,
                "orders",
                Some(999.0),
                Some("a step too big"),
            ),
            reading(Claim::Declared, "orders", Some(12.0), None),
            reading(Claim::Inferred, "orders", Some(5000.0), None),
            reading(Claim::Measured, "orders", Some(f64::NAN), None),
            reading(Claim::Measured, "orders", Some(7.1234567), None),
            reading(Claim::Measured, "refunds", Some(2.0), None),
            reading(Claim::Measured, "refunds", None, Some("missing")),
            outcome(true),
            outcome(true),
            outcome(false),
            refusal(3),
            refusal(2),
            Record::Decision {
                claim: Claim::Declared,
                venture: "shop".into(),
                verdict: Verdict::Deny {
                    ask: 20,
                    reason: "no".into(),
                },
                by: None,
            },
            ask(),
            ask(),
        ];
        records
            .into_iter()
            .enumerate()
            .map(|(i, r)| e(i as u64 + 1, r))
            .collect()
    }

    #[test]
    fn last_min_max_and_counts_match_a_hand_built_ledger() {
        let f = Facts::compute(&ledger());
        let m = &f.metrics["orders"];
        // 10, 4, 12 and 7.123457 count; 999 is implausible, 5000 is inferred, NaN is no number.
        assert_eq!((m.last, m.min, m.max, m.readings), (7.123457, 4.0, 12.0, 4));
        let r = &f.metrics["refunds"];
        assert_eq!((r.last, r.min, r.max, r.readings), (2.0, 2.0, 2.0, 1));
        assert_eq!(f.actions, 2);
        assert_eq!(f.refusals, 5);
        assert_eq!(f.denials, 1);
        assert_eq!(f.asks, 2);
        assert_eq!(f.implausible, 2);
    }

    #[test]
    fn a_previous_summarys_facts_carry_into_the_next() {
        let prev = Facts::compute(&ledger());
        let summary = Record::Report {
            claim: Claim::Inferred,
            venture: "shop".into(),
            metric: "summary".into(),
            value: None,
            note: "orders were 1000".into(),
            source: None,
            confidence: None,
            cites: vec![1],
            implausible: None,
            facts: Some(prev),
        };
        let span = vec![
            e(30, summary),
            e(31, reading(Claim::Measured, "orders", Some(20.0), None)),
        ];
        let f = Facts::compute(&span);
        let m = &f.metrics["orders"];
        assert_eq!((m.last, m.min, m.max, m.readings), (20.0, 4.0, 20.0, 5));
        assert_eq!(f.refusals, 5);
    }
}
