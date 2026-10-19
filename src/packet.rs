//! The decision packet: what a human is given to answer an ask. Built by
//! code, in this one place, whenever an ask is written. Premises are the
//! measured and declared records the ask rests on; options say what each
//! answer leads to; the reason, last, is a model's and is inferred.

use crate::forecast::{Outcome, Policy};
use crate::gate::{self, Adopted};
use crate::index::{Index, EXPECTATION};
use crate::mandate::{Controller, Mandate};
use crate::record::{
    OptionForecast, Packet, PacketOption, PolicyForecast, Premise, Record, Seq, Signal,
};

/// How many days and seeds a per-option forecast looks.
pub const DAYS: u32 = 7;
pub const SEEDS: u32 = 8;

/// The option names.
pub const APPROVE: &str = "approve";
pub const DENY: &str = "deny";
pub const EDIT: &str = "approve with edit";

/// What an ask rests on, and what it leads to.
#[derive(Clone, Debug, Default)]
pub struct Basis<'a> {
    pub venture: &'a str,
    /// The lever the ask is about, if it is about one.
    pub action: Option<&'a str>,
    /// Does the lever take a payload a human may edit?
    pub editable: bool,
    /// Records the ask cites. Only measured and declared ones become
    /// premises.
    pub cites: Vec<Seq>,
    /// The venture's metric: its latest reading is a premise when nothing
    /// else is cited.
    pub metric: Option<&'a str>,
    /// The model's words, when a model gave them.
    pub reason: Option<String>,
    /// Forecasts of approving and of denying, when forecasts are on.
    pub approve: Option<OptionForecast>,
    pub deny: Option<OptionForecast>,
}

/// Build the packet. Premises come first, then options, then the reason.
pub fn build(index: &Index, adopted: &[Adopted], now: i64, b: &Basis) -> Packet {
    let mut seqs = b.cites.clone();
    let measured = |s: &Seq| index.get(*s).is_some_and(|e| e.record.claim().authorizes());
    if !seqs.iter().any(measured) {
        seqs.extend(
            b.metric
                .and_then(|m| last_readings(index, b.venture, m, 1).into_iter().next()),
        );
    }
    let mut premises: Vec<Premise> = Vec::new();
    for seq in seqs {
        let Some(e) = index.get(seq).filter(|e| e.record.claim().authorizes()) else {
            continue;
        };
        if premises.iter().all(|p| p.seq != seq) {
            premises.push(Premise {
                seq,
                claim: e.record.claim(),
                summary: e.record.summary().chars().take(200).collect(),
            });
        }
    }
    let cost = b.action.map(|a| cost(index, adopted, now, b.venture, a));
    let mut options = vec![
        PacketOption {
            name: APPROVE.to_string(),
            forecast: b.approve.clone(),
            cost: cost.clone(),
        },
        PacketOption {
            name: DENY.to_string(),
            forecast: b.deny.clone(),
            cost: b.action.map(|_| "cost 0".to_string()),
        },
    ];
    if b.editable {
        options.insert(
            1,
            PacketOption {
                name: EDIT.to_string(),
                forecast: b.approve.clone(),
                cost,
            },
        );
    }
    Packet {
        premises,
        options,
        reason: b.reason.clone().filter(|r| !r.trim().is_empty()),
        policies: vec![],
    }
}

/// What a forecast found for the venture's metric under each policy, in
/// the order all, none, mixed, each against the mandate's threshold when
/// its controller has one.
pub fn policy_forecasts(
    outcome: &Outcome,
    venture: &str,
    mandate: &Mandate,
) -> Vec<PolicyForecast> {
    let metric = mandate.metric_name();
    let bound = match &mandate.controller {
        Controller::Threshold { above: Some(b), .. } => Some((*b, true)),
        Controller::Threshold { below: Some(b), .. } => Some((*b, false)),
        _ => None,
    };
    Policy::EVERY
        .iter()
        .filter_map(|policy| {
            let s = outcome
                .summaries
                .iter()
                .find(|s| s.venture == venture && s.policy == *policy && s.variant.is_none())?;
            let q = &s.metrics.iter().find(|m| m.name == metric)?.quantiles;
            let threshold = bound.map(|(b, above)| {
                let past = |x: f64| if above { x > b } else { x < b };
                let side = if above { "above" } else { "below" };
                let n = crate::record::round(b);
                if past(q.p95) && past(q.p5) {
                    format!("fails: {side} {n} even at p95")
                } else if !past(q.p5) && !past(q.p95) {
                    format!("holds: not {side} {n} even at p5")
                } else {
                    format!("may fail: {side} {n} in part of the seeds")
                }
            });
            Some(PolicyForecast {
                policy: policy.name().to_string(),
                metric: metric.to_string(),
                days: outcome.days,
                seeds: outcome.seeds,
                p5: crate::record::round(q.p5),
                p50: crate::record::round(q.p50),
                p95: crate::record::round(q.p95),
                threshold,
            })
        })
        .collect()
}

/// The packet of an ask the kernel wrote on its own: its premises are the
/// readings that triggered it. `metric` is the venture's own.
pub fn signal_packet(index: &Index, venture: &str, signal: &Signal, metric: &str) -> Packet {
    signal_packet_citing(index, venture, signal, metric, &[])
}

/// `signal_packet`, its premises also the measured and declared records
/// among `extra`: what the kernel counted to raise it.
pub fn signal_packet_citing(
    index: &Index,
    venture: &str,
    signal: &Signal,
    metric: &str,
    extra: &[Seq],
) -> Packet {
    let mut cites = readings(index, venture, signal, metric);
    cites.extend(
        extra
            .iter()
            .filter(|s| !cites.contains(s))
            .collect::<Vec<_>>(),
    );
    build(
        index,
        &[],
        0,
        &Basis {
            venture,
            cites,
            metric: Some(metric),
            ..Basis::default()
        },
    )
}

/// The newest reports of a metric that are measured or declared, oldest
/// first.
fn last_readings(index: &Index, venture: &str, metric: &str, n: usize) -> Vec<Seq> {
    let mut found: Vec<Seq> = index
        .of(venture)
        .rev()
        .filter(|e| {
            matches!(&e.record, Record::Report { claim, metric: m, .. }
                if claim.authorizes() && m == metric)
        })
        .take(n)
        .map(|e| e.seq)
        .collect();
    found.reverse();
    found
}

/// The readings a signal was raised from.
pub fn readings(index: &Index, venture: &str, signal: &Signal, metric: &str) -> Vec<Seq> {
    match signal {
        Signal::Saturated { .. } => last_readings(index, venture, metric, 1),
        Signal::Pending { intent, .. } => vec![*intent],
        Signal::Ineffective { .. } => last_readings(index, venture, EXPECTATION, 3),
        Signal::Guard { guard, .. } => last_readings(index, venture, &format!("guard.{guard}"), 1),
        Signal::Audit { audit, .. } => {
            let mut s = last_readings(index, venture, &format!("audit.{audit}"), 1);
            s.extend(last_readings(index, venture, metric, 1));
            s
        }
        Signal::Implausible { metric, .. }
        | Signal::Capacity { metric, .. }
        | Signal::Forecast { metric, .. } => last_readings(index, venture, metric, 1),
        Signal::Warning { metric, .. } => last_readings(index, venture, metric, 3),
    }
}

/// What a run costs and what is left after it.
fn cost(index: &Index, adopted: &[Adopted], now: i64, venture: &str, action: &str) -> String {
    let refs: Vec<&Adopted> = adopted.iter().collect();
    let Some(perm) = refs
        .iter()
        .find(|m| m.venture == venture)
        .and_then(|m| m.mandate.permitted(action))
    else {
        return "cost unknown".to_string();
    };
    let used = index.used(venture, action, now - perm.cap.per.as_secs() as i64);
    let mut text = format!(
        "cost {}; cap {} left after, of {}",
        perm.cost,
        perm.cap.count.saturating_sub(used + 1),
        perm.cap
    );
    if let Some((_, budget)) = gate::budget_of_in(index, &refs, venture) {
        let spent = gate::spent_today_in(index, &refs, venture, now);
        text.push_str(&format!(
            "; budget {} left after, of {budget}",
            i64::from(budget) - i64::from(spent) - i64::from(perm.cost)
        ));
    }
    text
}

/// What a forecast found for one policy: approving is the policy that
/// approves every ask, denying the one that answers none.
pub fn option_forecast(outcome: &Outcome, venture: &str, policy: Policy) -> Option<OptionForecast> {
    let s = outcome
        .summaries
        .iter()
        .find(|s| s.venture == venture && s.policy == policy && s.variant.is_none())?;
    let target = s.metrics.first()?;
    Some(OptionForecast {
        metric: target.name.clone(),
        days: outcome.days,
        seeds: outcome.seeds,
        p5: crate::record::round(target.quantiles.p5),
        p50: crate::record::round(target.quantiles.p50),
        p95: crate::record::round(target.quantiles.p95),
        breach: s.guards.iter().map(|g| g.p).fold(0.0, f64::max),
        spend: crate::record::round(s.reached.spend.p50),
    })
}
