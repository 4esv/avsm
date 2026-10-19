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
//! Plausibility: a reading of the metric, a guard or an audit that is
//! NaN, missing, out of range, too big a step or a flap is kept, marked
//! implausible, and is never a premise: not for a crossed threshold, not
//! for a guard's breach either way, not for an audit's delta, not for what
//! a lever did. The first of an episode asks. A step is measured from the
//! last plausible reading, until three readings in a row agree within
//! max_step: the third re-anchors, so a sensor that was stuck and then
//! reads the truth is not locked out forever. A declared approval of the
//! ask vouches for one reading, the anchor; its repeats are still checked.
//!
//! Stuck, where the metric varies and away from a bound of its range: a
//! reading exactly equal to the three before it, or to the one before it
//! when a lever expected to move the metric ran between. At a bound, a
//! repeat is dry soil, and a lever that does not move it is ineffective.
//! A flap is three steps in a row past max_step, each the other way, in
//! the raw readings whatever was believed.
//!
//! Deadband and hold keep a threshold from thrashing: it acts after `hold`
//! crossed reports in a row, and once it has acted it acts again only
//! after the metric comes back past the bound by the deadband.

use super::{log, Kernel};
use crate::gate::{Threshold, STOP};
use crate::index::Index;
use crate::mandate::{Guard, Then};
use crate::record::{Claim, Entry, Record, Seq, Signal};
use crate::venture::Venture;

/// How a signal question ends. Code never reads it; the signal field does.
pub(crate) const SIGNALLED: &str = "; avsm asks on its own";

/// What a signal is about, to keep one open per venture and subject.
fn subject(s: &Signal) -> String {
    s.subject()
}

/// How many readings in a row that agree within max_step re-anchor a
/// step: the last of them is plausible, whatever the anchor was.
pub const AGREE: usize = 3;

/// A reading exactly equal to each of this many readings before it is
/// stuck, where the metric's plausible history varies.
pub const STUCK: usize = 3;

/// How many of the latest readings, plausible or not, a reading is
/// checked against for agreeing and flapping.
const RECENT: usize = 3;

/// How far back a run of exact repeats is followed, to find whether a
/// lever ran during it.
const PLATEAU: usize = 256;

/// How many plausible readings say whether a metric varies.
pub const HISTORY: usize = 20;

/// The median absolute step between consecutive readings, or 0 with
/// fewer than two.
fn median_step(history: &[f64]) -> f64 {
    let mut steps: Vec<f64> = history.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
    if steps.is_empty() {
        return 0.0;
    }
    steps.sort_by(f64::total_cmp);
    let n = steps.len();
    if n % 2 == 1 {
        steps[n / 2]
    } else {
        (steps[n / 2 - 1] + steps[n / 2]) / 2.0
    }
}

/// Why a reading is not to be believed, or None when it is. `before`
/// holds the latest plausible readings, up to HISTORY, oldest first;
/// `recent` the values of the latest readings, plausible or not, oldest
/// first. `repeats` counts the readings just before that equal this one
/// exactly, missing ones skipped: a missing reading neither counts as a
/// repeat nor breaks a run of them. `moved` says a lever whose `expects`
/// moves this metric ran, ok, since the first of those repeats.
pub fn implausible(
    value: Option<f64>,
    before: &[f64],
    recent: &[Option<f64>],
    range: Option<[f64; 2]>,
    max_step: Option<f64>,
    repeats: usize,
    moved: bool,
) -> Option<String> {
    let Some(x) = value else {
        return Some("missing".to_string());
    };
    if x.is_nan() {
        return Some("NaN".to_string());
    }
    if !x.is_finite() {
        return Some(format!("{x} is not a number a report can mean"));
    }
    let inside = |x: f64| range.is_none_or(|[lo, hi]| x >= lo && x <= hi);
    if let Some([lo, hi]) = range {
        if !inside(x) {
            return Some(format!("{x} is out of range [{lo}, {hi}]"));
        }
    }
    let s = max_step?;
    // Exact repeats are the stuck signature, where the metric varies. A
    // metric that never varies is not stuck, and neither is a reading at
    // a declared bound of its range: dry soil reads its floor.
    let varies = median_step(before) > 0.0;
    let bound = |x: f64| range.is_some_and(|[lo, hi]| x == lo || x == hi);
    if varies && !bound(x) && repeats >= STUCK {
        return Some(format!(
            "stuck: {x} exactly, as the {STUCK} readings before it, where the metric varies"
        ));
    }
    // The readings just before are exactly this one, and a lever that is
    // expected to move the metric ran among them. At a bound that is the
    // lever's to answer for (ineffective), not the sensor's.
    if varies && moved && !bound(x) && repeats >= 1 {
        return Some(format!(
            "stuck: {x} exactly, as the reading before it, though a lever expected to move it ran since"
        ));
    }
    // A flap in the raw readings, whatever was believed: the last three
    // steps, this one included, each past max_step and each the other way.
    // An approval vouches for one reading; it never lets a flap back in.
    if recent.len() >= 3 {
        let raw: Vec<f64> = recent[recent.len() - 3..]
            .iter()
            .filter_map(|r| r.filter(|v| v.is_finite()))
            .chain([x])
            .collect();
        if raw.len() == 4 {
            let d: Vec<f64> = raw.windows(2).map(|w| w[1] - w[0]).collect();
            if d.iter().all(|d| d.abs() > s) && d[0] * d[1] < 0.0 && d[1] * d[2] < 0.0 {
                return Some(format!(
                    "flapping: the last three steps each past max_step {s}, each the other way"
                ));
            }
        }
    }
    let n = before.len();
    if let Some(prev) = before.last() {
        let step = (x - prev).abs();
        if step > s {
            // Re-anchor: this reading and the AGREE - 1 before it, all in
            // range, agree with each other within max_step. A flap never
            // agrees; a single spike has nothing to agree with. Where the
            // metric varies, an exact repeat is a stuck sensor's, so it
            // never counts toward agreeing.
            let k = AGREE - 1;
            let mut run = vec![x];
            if recent.len() >= k {
                for r in recent[recent.len() - k..].iter().rev() {
                    match r {
                        Some(v) if v.is_finite() && inside(*v) => run.push(*v),
                        _ => break,
                    }
                }
            }
            let repeats = run.windows(2).any(|w| w[0] == w[1] && !bound(w[0]));
            let lo = run.iter().copied().fold(f64::INFINITY, f64::min);
            let hi = run.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            if run.len() == AGREE && hi - lo <= s && !(varies && repeats) {
                return None;
            }
            return Some(format!(
                "a step of {step} from {prev} is more than max_step {s}"
            ));
        }
    }
    // A flap: the direction reversed by more than max_step twice in a row.
    // A step past max_step between plausible readings is a re-anchor, not
    // a direction, so it starts the count over.
    if n >= 3 {
        let d1 = before[n - 2] - before[n - 3];
        let d2 = before[n - 1] - before[n - 2];
        let d3 = x - before[n - 1];
        let reversed = |a: f64, b: f64| a * b < 0.0 && a.abs() + b.abs() > s;
        let anchored = d1.abs() <= s && d2.abs() <= s;
        if anchored && reversed(d1, d2) && reversed(d2, d3) {
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
            facts: None,
            ..
        } => Some(*x),
        _ => None,
    }
}

/// The values of the latest `n` reports of a metric, plausible or not,
/// oldest first.
fn recent(index: &Index, venture: &str, metric: &str, n: usize) -> Vec<Option<f64>> {
    let mut out: Vec<Option<f64>> = reports(index, venture, metric)
        .take(n)
        .map(|e| match &e.record {
            Record::Report { value, .. } => *value,
            _ => None,
        })
        .collect();
    out.reverse();
    out
}

/// Does this record approve, by a declared decision, an ask about an
/// implausible reading of this metric? Only a human says a doubted
/// reading is real.
fn vouches(index: &Index, e: &Entry, venture: &str, metric: &str) -> bool {
    let Record::Decision {
        claim: Claim::Declared,
        verdict: crate::record::Verdict::Approve { ask, .. },
        ..
    } = &e.record
    else {
        return false;
    };
    index.get(*ask).is_some_and(|q| {
        matches!(&q.record, Record::Ask {
            venture: v,
            signal: Some(Signal::Implausible { metric: m, .. }),
            ..
        } if v == venture && m == metric)
    })
}

/// The latest `n` readings a step is measured from, oldest first: the
/// plausible ones, where the latest reading before the latest declared
/// approval of an implausible ask about the metric counts as one of them,
/// plausible or not. The approval vouches for that one reading only: the
/// plausible history before it still says whether the metric varies.
fn anchored_before(index: &Index, venture: &str, metric: &str, n: usize) -> Vec<f64> {
    let mut out: Vec<f64> = Vec::new();
    // Searching for the approval; then for the reading it vouches for;
    // then the plausible ones before that.
    let mut vouched = false;
    let mut anchored = false;
    for e in index.of(venture).rev() {
        if out.len() >= n {
            break;
        }
        if !vouched && vouches(index, e, venture, metric) {
            vouched = true;
            continue;
        }
        let Record::Report {
            metric: m,
            claim,
            value,
            ..
        } = &e.record
        else {
            continue;
        };
        if m != metric || !claim.authorizes() {
            continue;
        }
        if vouched && !anchored {
            if let Some(x) = value.filter(|x| x.is_finite()) {
                out.push(x);
                anchored = true;
            }
        } else if let Some(x) = plausible(e) {
            out.push(x);
        }
    }
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
        let packet = self.signal_packet(venture, &signal);
        match self.append(Record::Ask {
            cites: vec![],
            claim: Claim::Measured,
            venture: venture.to_string(),
            action: None,
            question: format!("{question}{SIGNALLED}"),
            refusal: None,
            payload: None,
            signal: Some(signal),
            packet,
        }) {
            Ok(e) => log(&e.line()),
            Err(e) => log(&format!("{venture}: cannot write ask: {e}")),
        }
    }

    /// Why the reading of a metric about to be written is implausible, if
    /// it is, and whether it starts an episode: the last report of the
    /// metric before it was plausible, or there was none.
    pub(super) fn doubt(
        &self,
        venture: &str,
        metric: &str,
        value: Option<f64>,
        range: Option<[f64; 2]>,
        max_step: Option<f64>,
    ) -> (Option<String>, bool) {
        let Ok(index) = self.index() else {
            return (None, false);
        };
        let before = anchored_before(&index, venture, metric, HISTORY);
        let recent = recent(&index, venture, metric, RECENT);
        // The first of the readings just before that equal this one
        // exactly, missing ones skipped: did a lever that expects to move
        // this metric run, ok, since?
        let plateau: Vec<Seq> = value.map_or_else(Vec::new, |x| {
            reports(&index, venture, metric)
                .filter_map(|e| match &e.record {
                    Record::Report { value: Some(v), .. } => Some((e.seq, *v)),
                    _ => None,
                })
                .take_while(|(_, v)| *v == x)
                .take(PLATEAU)
                .map(|(seq, _)| seq)
                .collect()
        });
        let moved = plateau.last().is_some_and(|&last| {
            self.ventures.get(venture).is_some_and(|v| {
                v.mandate.metric_name() == metric
                    && v.mandate
                        .permitted
                        .iter()
                        .filter(|p| p.expects.is_some())
                        .any(|p| {
                            index
                                .intents_of(venture, &p.name)
                                .iter()
                                .rev()
                                .take_while(|i| **i > last)
                                .any(|i| index.settled(*i).is_some_and(|(_, ok)| ok))
                        })
            })
        });
        let why = implausible(
            value,
            &before,
            &recent,
            range,
            max_step,
            plateau.len(),
            moved,
        );
        let fresh = reports(&index, venture, metric)
            .next()
            .is_none_or(|e| e.record.implausible().is_none());
        (why, fresh)
    }

    /// Write a guard's or an audit's reading as a measured report, marked
    /// when it is implausible. Returns the entry, and why when the reading
    /// starts an episode of implausible ones.
    fn write_reading(
        &mut self,
        v: &Venture,
        metric: &str,
        value: Option<f64>,
        note: &str,
        range: Option<[f64; 2]>,
        max_step: Option<f64>,
    ) -> Option<(Entry, Option<String>)> {
        let (why, fresh) = self.doubt(&v.name, metric, value, range, max_step);
        let mut record = Record::report(
            Claim::Measured,
            &v.name,
            metric,
            value.filter(|x| x.is_finite()),
            note,
        );
        if let Record::Report { implausible, .. } = &mut record {
            implausible.clone_from(&why);
        }
        match self.append(record) {
            Ok(e) => Some((e, why.filter(|_| fresh))),
            Err(e) => {
                log(&format!("{}: cannot write {metric}: {e}", v.name));
                None
            }
        }
    }

    /// Ask once an episode about implausible guard and audit readings.
    pub(super) fn doubt_readings(&mut self, venture: &str, doubts: Vec<(String, String)>) {
        for (metric, why) in doubts {
            self.signal(
                venture,
                format!("{metric} reported something implausible: {why}"),
                Signal::Implausible { metric, why },
            );
        }
    }

    /// Read every guard and audit of a venture and write each reading as
    /// a measured report. A drawdown is computed from the peak of the
    /// metric's plausible reports; the rest the warden reads. Also returns
    /// the metrics whose implausible reading starts an episode, and why.
    #[allow(clippy::type_complexity)]
    pub(super) fn read_guards(
        &mut self,
        v: &Venture,
    ) -> (Vec<(Guard, Entry)>, Vec<(String, String)>) {
        let mut out = Vec::new();
        let mut doubts = Vec::new();
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
            if let Some((e, why)) =
                self.write_reading(v, &metric, value, &note, g.range, g.max_step)
            {
                doubts.extend(why.map(|w| (metric, w)));
                out.push((g, e));
            }
        }
        for a in v.mandate.audits.clone() {
            let metric = a.metric();
            let (value, note) = match self.warden.read(&v.name, &metric) {
                Ok(read) => read,
                Err(e) => (None, format!("cannot read: {e}")),
            };
            if let Some((_, why)) =
                self.write_reading(v, &metric, value, &note, a.range, a.max_step)
            {
                doubts.extend(why.map(|w| (metric, w)));
            }
        }
        (out, doubts)
    }

    /// Act on guard readings past their bounds: pause the venture with a
    /// stop, which always passes, or ask once per breach. An implausible
    /// reading is never past a bound.
    pub(super) fn enforce_guards(&mut self, name: &str, readings: Vec<(Guard, Entry)>) {
        for (g, e) in readings {
            let Some(value) = plausible(&e) else {
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
                            .find_map(plausible);
                        prev
                    };
                    // One ask per breach: the plausible reading before was
                    // inside.
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
                    .filter_map(|e| plausible(e).map(|x| (e.seq, x)))
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
    use crate::mandate::Mandate;

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

    /// How many of these readings, newest first, equal `x`, missing ones
    /// skipped: what `doubt` counts from the ledger.
    fn reps(x: f64, recent: &[Option<f64>]) -> usize {
        recent
            .iter()
            .rev()
            .flatten()
            .take_while(|v| **v == x)
            .count()
    }

    /// Why a reading of the metric is implausible, with no recent run.
    fn doubted(value: Option<f64>, before: &[f64], m: &Mandate) -> Option<String> {
        implausible(value, before, &[], m.range, m.max_step, 0, false)
    }

    #[test]
    fn three_agreeing_readings_re_anchor_and_nothing_less_does() {
        let (range, step) = (Some([0.0, 1000.0]), Some(50.0));
        let anchor = [60.0, 60.0, 60.0];
        let at = |x: f64, recent: &[Option<f64>]| {
            implausible(
                Some(x),
                &anchor,
                recent,
                range,
                step,
                reps(x, recent),
                false,
            )
        };
        // Stuck at 60, then the truth near 0: the third agreeing re-anchors.
        assert!(at(2.0, &[Some(60.0), Some(60.0)]).is_some(), "one jump");
        assert!(at(1.0, &[Some(60.0), Some(2.0)]).is_some(), "two agree");
        assert_eq!(at(0.0, &[Some(2.0), Some(1.0)]), None, "three agree");
        // A flap never agrees; a spike has nothing to agree with.
        assert!(at(200.0, &[Some(200.0), Some(0.0)]).is_some(), "flap");
        assert!(at(900.0, &[Some(60.0), Some(60.0)]).is_some(), "spike");
        // A run broken by a missing or out-of-range reading does not agree.
        assert!(at(0.0, &[None, Some(1.0)]).is_some(), "missing breaks it");
        // Agreeing means within max_step of each other, not of the last.
        assert!(at(0.0, &[Some(80.0), Some(40.0)]).is_some(), "a drift");
        // Re-anchored, a small wiggle back is no flap.
        let after = [60.0, 60.0, 0.0];
        assert_eq!(
            implausible(Some(1.0), &after, &[], range, step, 0, false),
            None
        );
    }

    #[test]
    fn exact_repeats_are_stuck_where_the_metric_varies_and_never_agree() {
        let (range, step) = (Some([0.0, 1000.0]), Some(50.0));
        let varied = [40.0, 42.0, 41.0, 43.0, 41.0];
        let at = |x: f64, recent: &[Option<f64>]| {
            implausible(
                Some(x),
                &varied,
                recent,
                range,
                step,
                reps(x, recent),
                false,
            )
        };
        let stuck = at(41.0, &[Some(41.0), Some(41.0), Some(41.0)]).unwrap();
        assert!(stuck.starts_with("stuck"), "{stuck}");
        assert_eq!(
            at(41.0, &[Some(43.0), Some(41.0), Some(41.0)]),
            None,
            "two repeats"
        );
        // Three identical readings after a jump never re-anchor.
        assert!(at(200.0, &[Some(41.0), Some(200.0), Some(200.0)]).is_some());
        // Three that agree but differ do.
        assert_eq!(at(301.0, &[Some(200.0), Some(300.0), Some(302.0)]), None);
        // A metric that never varies is never stuck, and its repeats agree.
        let flat = [50.0; 10];
        let same = [Some(50.0), Some(50.0), Some(50.0)];
        assert_eq!(
            implausible(Some(50.0), &flat, &same, range, step, 3, false),
            None
        );
        assert_eq!(median_step(&flat), 0.0);
        assert_eq!(median_step(&varied), 2.0);
    }

    #[test]
    fn implausible_readings_say_why() {
        let m = mandate("range: [0, 1000]\nmax_step: 50\n");
        assert_eq!(doubted(Some(5.0), &[], &m), None);
        assert_eq!(doubted(None, &[], &m).as_deref(), Some("missing"));
        assert_eq!(doubted(Some(f64::NAN), &[], &m).as_deref(), Some("NaN"));
        assert!(doubted(Some(-1.0), &[], &m)
            .unwrap()
            .contains("out of range"));
        assert!(doubted(Some(1e9), &[], &m)
            .unwrap()
            .contains("out of range"));
        assert!(doubted(Some(200.0), &[100.0], &m)
            .unwrap()
            .contains("max_step"));
        assert_eq!(doubted(Some(140.0), &[100.0], &m), None);
        let flap = doubted(Some(140.0), &[100.0, 140.0, 100.0], &m).unwrap();
        assert!(flap.contains("flapping"), "{flap}");
        assert_eq!(doubted(Some(110.0), &[100.0, 105.0, 100.0], &m), None);
        let bare = mandate("");
        assert_eq!(doubted(Some(1e12), &[0.0], &bare), None);
    }
}
