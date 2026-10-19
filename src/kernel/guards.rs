//! What the kernel reads beside the metric, and what it does about it.
//!
//! Guards are readings the warden takes, each with a bound. Past it, a
//! guard pauses the venture or asks once; either way the gate refuses
//! every lever but stop until the reading is back inside.
//!
//! Audits are readings that should not worsen while the target improves.
//! When the target rises over an audit's window and the audit worsens past
//! its tolerance, the kernel asks. No model ever sees an audit reading.
//!
//! Plausibility: a reading of the metric that is NaN, missing, out of
//! range, too big a step or a flap is kept, marked implausible, and is
//! never a premise for a crossed threshold. The first of an episode asks.
//!
//! Deadband and hold keep a threshold from thrashing: it acts after `hold`
//! crossed reports in a row, and once it has acted it acts again only
//! after the metric comes back past the bound by the deadband.

use super::{log, Kernel};
use crate::gate::{Threshold, STOP};
use crate::index::Index;
use crate::mandate::{Guard, Mandate, Then};
use crate::record::{Claim, Entry, Record, Seq, Signal};
use crate::venture::Venture;

/// How a signal question ends. Code never reads it; the signal field does.
const SIGNALLED: &str = "; avsm asks on its own";

/// What a signal is about, to keep one open per venture and subject.
fn subject(s: &Signal) -> String {
    match s {
        Signal::Saturated { action, .. } => format!("saturated:{action}"),
        Signal::Pending { intent, .. } => format!("pending:{intent}"),
        Signal::Ineffective { action, .. } => format!("ineffective:{action}"),
        Signal::Guard { guard, .. } => format!("guard:{guard}"),
        Signal::Audit { audit, .. } => format!("audit:{audit}"),
        Signal::Implausible { metric, .. } => format!("implausible:{metric}"),
    }
}

/// Why a reading of the metric is not to be believed, or None when it is.
/// `before` holds the latest plausible readings, oldest first.
pub fn implausible(value: Option<f64>, before: &[f64], mandate: &Mandate) -> Option<String> {
    let Some(x) = value else {
        return Some("missing".to_string());
    };
    if x.is_nan() {
        return Some("NaN".to_string());
    }
    if !x.is_finite() {
        return Some(format!("{x} is not a number a report can mean"));
    }
    if let Some([lo, hi]) = mandate.range {
        if x < lo || x > hi {
            return Some(format!("{x} is out of range [{lo}, {hi}]"));
        }
    }
    let s = mandate.max_step?;
    let n = before.len();
    if let Some(prev) = before.last() {
        let step = (x - prev).abs();
        if step > s {
            return Some(format!(
                "a step of {step} from {prev} is more than max_step {s}"
            ));
        }
    }
    // A flap: the direction reversed by more than max_step twice in a row.
    if n >= 3 {
        let d1 = before[n - 2] - before[n - 3];
        let d2 = before[n - 1] - before[n - 2];
        let d3 = x - before[n - 1];
        let reversed = |a: f64, b: f64| a * b < 0.0 && a.abs() + b.abs() > s;
        if reversed(d1, d2) && reversed(d2, d3) {
            return Some(format!(
                "flapping: reversed by more than max_step {s} twice in a row"
            ));
        }
    }
    None
}

/// A venture's trusted reports of a metric, newest first.
fn reports<'a>(
    index: &'a Index,
    venture: &'a str,
    metric: &'a str,
) -> impl Iterator<Item = &'a Entry> + 'a {
    index.of(venture).rev().filter(move |e| {
        matches!(&e.record, Record::Report { metric: m, claim, .. }
            if m == metric && claim.authorizes())
    })
}

/// The value of a report, when it is plausible and has one.
fn plausible(e: &Entry) -> Option<f64> {
    match &e.record {
        Record::Report {
            value: Some(x),
            implausible: None,
            ..
        } => Some(*x),
        _ => None,
    }
}

/// The latest `n` plausible readings of a metric, oldest first.
pub fn plausible_before(index: &Index, venture: &str, metric: &str, n: usize) -> Vec<f64> {
    let mut out: Vec<f64> = reports(index, venture, metric)
        .filter_map(plausible)
        .take(n)
        .collect();
    out.reverse();
    out
}

/// Does the latest plausible report act on the threshold, with this
/// deadband and hold? Without a deadband, every report that ends `hold`
/// crossed reports in a row acts. With one, the threshold acts once and
/// re-arms only when a report comes back past the bound by the deadband.
pub fn fires(values_newest_first: &[f64], t: &Threshold, deadband: f64, hold: u32) -> bool {
    let crossed = |x: f64| (t.above && x > t.bound) || (!t.above && x < t.bound);
    let rearms = |x: f64| {
        if t.above {
            x < t.bound - deadband
        } else {
            x > t.bound + deadband
        }
    };
    let hold = hold.max(1) as usize;
    if deadband <= 0.0 {
        return values_newest_first.len() >= hold
            && values_newest_first[..hold].iter().all(|x| crossed(*x));
    }
    // Since the last report that re-armed it, oldest first.
    let mut span: Vec<f64> = values_newest_first
        .iter()
        .copied()
        .take_while(|x| !rearms(*x))
        .collect();
    span.reverse();
    let mut armed = true;
    let mut streak = 0;
    let last = span.len().wrapping_sub(1);
    for (i, x) in span.into_iter().enumerate() {
        if crossed(x) {
            streak += 1;
            if armed && streak >= hold {
                if i == last {
                    return true;
                }
                armed = false;
            }
        } else {
            streak = 0;
        }
    }
    false
}

impl Kernel {
    /// Write an ask carrying a signal: measured, past the asks quota, and
    /// only when none is open for this venture and subject.
    pub(super) fn signal(&mut self, venture: &str, question: String, signal: Signal) {
        let key = subject(&signal);
        let already = {
            let Ok(index) = self.index() else { return };
            let found = index.open_signals().into_iter().any(|(e, s)| {
                e.record.venture() == venture
                    && e.record.claim() == Claim::Measured
                    && subject(s) == key
            });
            found
        };
        if already {
            return;
        }
        match self.append(Record::Ask {
            claim: Claim::Measured,
            venture: venture.to_string(),
            action: None,
            question: format!("{question}{SIGNALLED}"),
            refusal: None,
            payload: None,
            signal: Some(signal),
        }) {
            Ok(e) => log(&e.line()),
            Err(e) => log(&format!("{venture}: cannot write ask: {e}")),
        }
    }

    /// Why the reading about to be written is implausible, if it is, and
    /// whether it starts an episode: the last report of the metric before
    /// it was plausible, or there was none.
    pub(super) fn doubt(&self, v: &Venture, value: Option<f64>) -> (Option<String>, bool) {
        let Ok(index) = self.index() else {
            return (None, false);
        };
        let metric = v.mandate.metric_name();
        let before = plausible_before(&index, &v.name, metric, 3);
        let why = implausible(value, &before, &v.mandate);
        let fresh = reports(&index, &v.name, metric)
            .next()
            .is_none_or(|e| e.record.implausible().is_none());
        (why, fresh)
    }

    /// Read every guard and audit of a venture and write each reading as
    /// a measured report. A drawdown is computed from the peak of the
    /// metric's plausible reports; the rest the warden reads.
    pub(super) fn read_guards(&mut self, v: &Venture) -> Vec<(Guard, Entry)> {
        let mut out = Vec::new();
        for g in v.mandate.guards.clone() {
            let metric = g.metric();
            let (value, note) = match g.drawdown {
                Some(_) => {
                    let Ok(index) = self.index() else { continue };
                    let of = v.mandate.metric_name();
                    let now = reports(&index, &v.name, of).next().and_then(plausible);
                    match (now, index.peak(&v.name, of)) {
                        (Some(x), Some(peak)) if peak > 0.0 => (
                            Some((peak - x) / peak * 100.0),
                            format!("{of} {x}, peak {peak}"),
                        ),
                        (Some(_), Some(_)) => {
                            (None, format!("{of} has no peak above 0 to fall from"))
                        }
                        _ => continue,
                    }
                }
                None => match self.warden.read(&v.name, &metric) {
                    Ok(read) => read,
                    Err(e) => (None, format!("cannot read: {e}")),
                },
            };
            match self.append(Record::report(
                Claim::Measured,
                &v.name,
                &metric,
                value,
                &note,
            )) {
                Ok(e) => out.push((g, e)),
                Err(e) => log(&format!("{}: cannot write {metric}: {e}", v.name)),
            }
        }
        for a in v.mandate.audits.clone() {
            let metric = a.metric();
            let (value, note) = match self.warden.read(&v.name, &metric) {
                Ok(read) => read,
                Err(e) => (None, format!("cannot read: {e}")),
            };
            if let Err(e) = self.append(Record::report(
                Claim::Measured,
                &v.name,
                &metric,
                value,
                &note,
            )) {
                log(&format!("{}: cannot write {metric}: {e}", v.name));
            }
        }
        out
    }

    /// Act on guard readings past their bounds: pause the venture with a
    /// stop, which always passes, or ask once per breach.
    pub(super) fn enforce_guards(&mut self, name: &str, readings: Vec<(Guard, Entry)>) {
        for (g, e) in readings {
            let Record::Report {
                value: Some(value), ..
            } = e.record
            else {
                continue;
            };
            if !g.breached(value) {
                continue;
            }
            let (above, bound) = g.bound();
            match g.then {
                Then::Pause => {
                    let paused = match self.index() {
                        Ok(index) => {
                            crate::gate::state_in(&index, &self.adopted(), name)
                                == crate::gate::State::Paused
                        }
                        Err(_) => continue,
                    };
                    if !paused {
                        self.act(name, STOP, vec![e.seq], Claim::Measured);
                    }
                }
                Then::Ask => {
                    let before = {
                        let Ok(index) = self.index() else { continue };
                        let metric = g.metric();
                        let prev = reports(&index, name, &metric)
                            .filter(|r| r.seq < e.seq)
                            .find_map(|r| match &r.record {
                                Record::Report { value: Some(x), .. } => Some(*x),
                                _ => None,
                            });
                        prev
                    };
                    // One ask per breach: the reading before was inside.
                    if before.is_some_and(|x| g.breached(x)) {
                        continue;
                    }
                    self.signal(
                        name,
                        format!(
                            "guard {} reads {value}, past its bound ({} {bound}); every lever but stop is refused until it is back inside",
                            g.name,
                            if above { "above" } else { "below" }
                        ),
                        Signal::Guard {
                            guard: g.name.clone(),
                            value,
                            bound,
                        },
                    );
                }
            }
        }
    }

    /// Ask when the target rose over an audit's window while the audit
    /// worsened past its tolerance.
    pub(super) fn check_audits(&mut self, v: &Venture) {
        let target = v.mandate.metric_name();
        for a in &v.mandate.audits {
            let found = {
                let Ok(index) = self.index() else { return };
                let metric = a.metric();
                let window: Vec<(Seq, f64)> = reports(&index, &v.name, &metric)
                    .filter_map(|e| match &e.record {
                        Record::Report { value: Some(x), .. } => Some((e.seq, *x)),
                        _ => None,
                    })
                    .take(a.window as usize)
                    .collect();
                let (Some(&(new_seq, new)), Some(&(old_seq, old))) =
                    (window.first(), window.last())
                else {
                    continue;
                };
                if window.len() < 2 {
                    continue;
                }
                let target_at = |at: Seq| {
                    reports(&index, &v.name, target)
                        .filter(|e| e.seq < at)
                        .find_map(plausible)
                };
                match (target_at(old_seq), target_at(new_seq)) {
                    (Some(t0), Some(t1)) => Some((t1 - t0, new - old)),
                    _ => None,
                }
            };
            let Some((target_delta, audit_delta)) = found else {
                continue;
            };
            if a.improved(target_delta) > 0.0 && a.worsened(audit_delta) > a.tolerance {
                self.signal(
                    &v.name,
                    format!(
                        "{target} got better by {} while audit {} got worse by {}, past its tolerance {}",
                        a.improved(target_delta),
                        a.name,
                        a.worsened(audit_delta),
                        a.tolerance
                    ),
                    Signal::Audit {
                        audit: a.name.clone(),
                        target_delta,
                        audit_delta,
                    },
                );
            }
        }
    }

    /// Does the report just written act on the threshold, by its deadband
    /// and hold?
    pub(super) fn threshold_fires(&self, v: &Venture, t: &Threshold) -> bool {
        let Ok(index) = self.index() else {
            return false;
        };
        let (deadband, hold) = v.mandate.deadband_hold();
        let metric = v.mandate.metric_name();
        let values: Vec<f64> = if deadband > 0.0 {
            reports(&index, &v.name, metric)
                .filter_map(plausible)
                .collect()
        } else {
            reports(&index, &v.name, metric)
                .filter_map(plausible)
                .take(hold as usize)
                .collect()
        };
        fires(&values, t, deadband, hold)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn above(bound: f64) -> Threshold {
        Threshold {
            above: true,
            bound,
            action: "reset".into(),
            from: None,
        }
    }

    /// How many of these reports, in order, act.
    fn acts(values: &[f64], d: f64, hold: u32) -> usize {
        (1..=values.len())
            .filter(|&n| {
                let newest_first: Vec<f64> = values[..n].iter().rev().copied().collect();
                fires(&newest_first, &above(10.0), d, hold)
            })
            .count()
    }

    #[test]
    fn a_deadband_fires_once_on_an_oscillation_and_rearms_past_it() {
        let wobble = [11.0, 9.0, 11.0, 9.5, 11.0, 9.0, 12.0];
        assert_eq!(
            acts(&wobble, 0.0, 1),
            4,
            "without a deadband each crossing acts"
        );
        assert_eq!(acts(&wobble, 2.0, 1), 1, "with one, the wobble acts once");
        assert_eq!(
            acts(&[11.0, 7.0, 11.0], 2.0, 1),
            2,
            "7 is past 10 - 2: re-armed"
        );
    }

    #[test]
    fn hold_needs_that_many_crossed_reports_in_a_row() {
        assert_eq!(acts(&[11.0, 11.0], 0.0, 3), 0);
        assert_eq!(acts(&[11.0, 11.0, 9.0, 11.0, 11.0], 0.0, 3), 0);
        assert_eq!(acts(&[11.0, 11.0, 11.0], 0.0, 3), 1);
        assert_eq!(acts(&[11.0, 11.0, 11.0, 11.0], 2.0, 3), 1);
    }

    fn mandate(extra: &str) -> Mandate {
        Mandate::parse(&format!(
            "purpose: p\nmetric: m\ncontroller: {{kind: model}}\ncadence: 1h\n{extra}"
        ))
        .unwrap()
    }

    #[test]
    fn implausible_readings_say_why() {
        let m = mandate("range: [0, 1000]\nmax_step: 50\n");
        assert_eq!(implausible(Some(5.0), &[], &m), None);
        assert_eq!(implausible(None, &[], &m).as_deref(), Some("missing"));
        assert_eq!(implausible(Some(f64::NAN), &[], &m).as_deref(), Some("NaN"));
        assert!(implausible(Some(-1.0), &[], &m)
            .unwrap()
            .contains("out of range"));
        assert!(implausible(Some(1e9), &[], &m)
            .unwrap()
            .contains("out of range"));
        assert!(implausible(Some(200.0), &[100.0], &m)
            .unwrap()
            .contains("max_step"));
        assert_eq!(implausible(Some(140.0), &[100.0], &m), None);
        let flap = implausible(Some(140.0), &[100.0, 140.0, 100.0], &m).unwrap();
        assert!(flap.contains("flapping"), "{flap}");
        assert_eq!(implausible(Some(110.0), &[100.0, 105.0, 100.0], &m), None);
        let bare = mandate("");
        assert_eq!(implausible(Some(1e12), &[0.0], &bare), None);
    }
}
