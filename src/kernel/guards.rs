//! What the kernel reads beside the metric, and what it does about it.
//!
//! Guards are readings the warden takes, each with a bound. Past it, a
//! guard pauses the venture or asks once; either way the gate refuses
//! every lever but stop until the reading is back inside. A guard's
//! declared unwind is the one exception: on the reading that trips it,
//! and on later ones past the bound until a run of it is ok, the kernel
//! runs that lever with that payload, once an episode.
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
//! An approval of a stuck ask vouches for the run it flagged, and for the
//! repeats that go on after it, until the metric reads another value. It
//! vouches for nothing else: those repeats are no step of the metric and
//! never history, so they never make repeats ordinary, and a stuck run at
//! any other value, or this one again after it moved, is judged by the
//! metric's unvouched history.
//! With max_step, a reading right after an implausible one is unconfirmed,
//! marked like one and never a premise, until the next reading agrees with
//! it within max_step (not by an exact repeat where the metric varies); a
//! re-anchor needs no more. `confirm: false` opts a reading out.
//! max_rise and max_fall bound a step one way each, beside max_step or in
//! its place: wherever max_step is said here, the bound the way the step
//! goes is meant, and two readings agree when the later is a plausible
//! step from the earlier.
//!
//! Stuck, away from a bound of its range: a reading exactly equal to the
//! readings before it, when the metric's own plausible history says so
//! long a run of exact repeats is less likely than ALPHA anywhere in a
//! stretch as long as that history; or equal to the
//! one before it when a lever expected to move the metric ran between,
//! where the metric varies. At a bound, a repeat is dry soil, and a lever
//! that does not move it is ineffective.
//! A flap is three steps in a row past max_step, each the other way, in
//! the raw readings whatever was believed.
//!
//! Deadband and hold keep a threshold from thrashing: it acts after `hold`
//! crossed reports in a row, and once it has acted it acts again only
//! after the metric comes back past the bound by the deadband.

use super::{log, Kernel};
use crate::gate::{Threshold, STOP};
use crate::index::Index;
use crate::mandate::{Guard, Steps, Then};
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

/// The significance level of the stuck test: a run of exact repeats is
/// stuck when the metric's history makes it less likely than this. The
/// chance of a repeat is not known, only how often one has happened: with
/// z repeats in n steps and no prior belief (uniform), the chance of a run
/// of r more is the posterior predictive, prod_{i<r} (z+1+i)/(n+2+i),
/// Laplace's rule of succession applied once per repeat. Each repeat in
/// the run is evidence for the next, so a thin history never makes a run
/// look rarer than it can say. Every reading is tested, so the run is
/// judged by the chance of one that long anywhere in as many steps as the
/// history: a run of r repeats can start at n - r + 1 of n steps, and the
/// chance of any of them is at most that many times the chance of one. A
/// sensor that is not stuck is then flagged in fewer than one of 1/ALPHA =
/// 100 stretches as long as its history, not on one reading in 100. On a
/// continuous sensor with 21 clean steps that makes the fourth identical
/// reading stuck (19 * 1/23 * 2/24 * 3/25 = 0.0083) and not the third
/// (0.072); with WINDOW clean steps, the third (255 * 1/258 * 2/259 =
/// 0.0076), not the second (0.99); on a metric that repeated in one of two
/// steps, the 23rd repeat, not the eighth.
pub const ALPHA: f64 = 0.01;

/// How many of the latest steps of the plausible history say how often
/// the metric repeats exactly. Stuck readings are implausible and never
/// enter it, vouched for or not, and a repeat at a bound of the range says nothing of the
/// sensor and is no step of it, so a metric frozen for weeks, or dry at
/// its floor, is still judged by the history from when it varied. 256
/// steps let a clean history make two repeats less likely than ALPHA
/// anywhere in it (it needs 194; one alone never is)
/// and keep the check one pass over at most 257 stretches.
pub const WINDOW: usize = 256;

/// How many of the latest readings, plausible or not, a reading is
/// checked against for agreeing and flapping.
const RECENT: usize = 3;

/// How far back a run of exact repeats is followed, to find whether a
/// lever ran during it.
const PLATEAU: usize = 256;

/// How many plausible readings say whether a metric varies, for a repeat
/// after a lever ran and for agreeing.
pub const HISTORY: usize = 20;

/// The run of exact repeats a reading ends, and what it is judged by.
#[derive(Clone, Copy, Debug, Default)]
pub struct Plateau<'a> {
    /// The readings just before that equal this one exactly, missing ones
    /// skipped: a missing reading neither counts as a repeat nor breaks a
    /// run of them.
    pub repeats: usize,
    /// A lever whose `expects` moves this metric ran, ok, since the first
    /// of those repeats.
    pub moved: bool,
    /// The run is one a declared approval of a stuck ask vouched for, and
    /// this reading continues it. The approval is for that run: it is not
    /// judged again, and nothing of it is history.
    pub vouched: bool,
    /// The plausible history as stretches of exactly equal readings, value
    /// and how many, oldest first; the latest WINDOW + 1 are enough.
    pub history: &'a [(f64, usize)],
}

/// A probability to two significant figures: 0.0021, or 3.4e-9.
fn two_figures(p: f64) -> String {
    if p >= 1e-4 {
        let digits = (1.0 - p.log10().floor()).max(2.0) as usize;
        format!("{p:.digits$}")
    } else {
        format!("{p:.1e}")
    }
}

/// What the history before a run of `repeats` exact repeats of `x` says
/// of how often the metric repeats: of its latest WINDOW steps between
/// consecutive readings, how many there are and how many are exactly
/// zero. The run's own repeats are what is judged, so they are not
/// evidence; the step into it is. A repeat at a bound is no step.
fn repeat_history(
    history: &[(f64, usize)],
    x: f64,
    repeats: usize,
    bound: impl Fn(f64) -> bool,
) -> (usize, usize) {
    let (mut steps, mut zero) = (0, 0);
    let last = history.len().saturating_sub(1);
    for (i, &(v, n)) in history.iter().enumerate().rev() {
        let n = if i == last && v == x {
            n - repeats.min(n.saturating_sub(1))
        } else {
            n
        };
        if !bound(v) {
            let z = (n - 1).min(WINDOW - steps);
            steps += z;
            zero += z;
        }
        if i > 0 && steps < WINDOW {
            steps += 1;
        }
        if steps >= WINDOW {
            break;
        }
    }
    (steps, zero)
}

/// The chance of a run of `repeats` exact repeats anywhere in `steps`
/// steps, after `zero` of them were repeats, under a uniform prior on the
/// chance of one: at most the n - r + 1 places a run of r can start, at
/// least one, times the posterior predictive of one run,
/// prod_{i<repeats} (zero+1+i)/(steps+2+i). It stops once below ALPHA,
/// the only question asked of it, so what it returns is at most the chance
/// and the work at most one term per repeat. A history with no step but
/// zero ones has not been seen to vary, and gives no chance: nothing to
/// judge the run against.
fn run_chance(steps: usize, zero: usize, repeats: usize) -> Option<f64> {
    if zero >= steps {
        return None;
    }
    let places = |r: usize| (steps + 1).saturating_sub(r).max(1) as f64;
    let mut chance = 1.0;
    for i in 0..repeats {
        chance *= (zero + 1 + i) as f64 / (steps + 2 + i) as f64;
        if chance * places(i + 1) < ALPHA {
            break;
        }
    }
    Some(chance * places(repeats))
}

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
/// holds the latest plausible readings a step is measured from, up to
/// HISTORY, oldest first; `recent` the values of the latest readings,
/// plausible or not, oldest first; `plateau` the run of exact repeats
/// this reading ends.
pub fn implausible(
    value: Option<f64>,
    before: &[f64],
    recent: &[Option<f64>],
    range: Option<[f64; 2]>,
    steps: Option<Steps>,
    plateau: Plateau,
) -> Option<String> {
    let Plateau {
        repeats,
        moved,
        vouched,
        ..
    } = plateau;
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
    let s = steps?;
    // Exact repeats are the stuck signature, as rare as the metric's own
    // history says they are: a run of them less likely than ALPHA anywhere
    // in a stretch as long as that history is stuck. A metric that seldom
    // varies repeats often and is never stuck, nor one that has never
    // varied, and neither is a reading at a declared bound of its range:
    // dry soil reads its floor.
    let varies = median_step(before) > 0.0;
    let bound = |x: f64| range.is_some_and(|[lo, hi]| x == lo || x == hi);
    if repeats >= 1 && !bound(x) && !vouched {
        let (steps, zero) = repeat_history(plateau.history, x, repeats, bound);
        let chance = run_chance(steps, zero, repeats);
        if let Some(chance) = chance.filter(|c| *c < ALPHA) {
            let before = if repeats == 1 {
                "the reading before it".to_string()
            } else {
                format!("the {repeats} readings before it")
            };
            return Some(format!(
                "stuck: {x} exactly, as {before}; after {zero} repeats in {steps} steps, a run of {repeats} anywhere in as many has chance at most {} < {ALPHA}",
                two_figures(chance)
            ));
        }
    }
    // The readings just before are exactly this one, and a lever that is
    // expected to move the metric ran among them. At a bound that is the
    // lever's to answer for (ineffective), not the sensor's.
    if varies && moved && !bound(x) && repeats >= 1 && !vouched {
        return Some(format!(
            "stuck: {x} exactly, as the reading before it, though a lever expected to move it ran since"
        ));
    }
    // A flap in the raw readings, whatever was believed: the last three
    // steps, this one included, each past its way's bound and each the
    // other way.
    // An approval vouches for one reading; it never lets a flap back in.
    if recent.len() >= 3 {
        let raw: Vec<f64> = recent[recent.len() - 3..]
            .iter()
            .filter_map(|r| r.filter(|v| v.is_finite()))
            .chain([x])
            .collect();
        if raw.len() == 4 {
            let d: Vec<f64> = raw.windows(2).map(|w| w[1] - w[0]).collect();
            if d.iter().all(|d| s.past(*d)) && d[0] * d[1] < 0.0 && d[1] * d[2] < 0.0 {
                return Some(format!(
                    "flapping: the last three steps each past {s}, each the other way"
                ));
            }
        }
    }
    let n = before.len();
    if let Some(prev) = before.last() {
        if let Some(why) = s.too_far(*prev, x - prev) {
            // Re-anchor: this reading and the AGREE - 1 before it, all in
            // range, agree with each other: each a plausible step from
            // every one before it. A flap never agrees; a single spike has
            // nothing to agree with. Where the metric varies, an exact
            // repeat is a stuck sensor's, so it never counts toward
            // agreeing.
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
            // `run` is newest first: a later reading minus an earlier one.
            let agree = run
                .iter()
                .enumerate()
                .all(|(i, later)| run[i + 1..].iter().all(|earlier| !s.past(later - earlier)));
            if run.len() == AGREE && agree && !(varies && repeats) {
                return None;
            }
            return Some(why);
        }
    }
    // A flap: the direction reversed by more than max_step twice in a row;
    // with a bound each way, by more than either, so a delivery between two
    // days of demand is no flap. A step past its bound between plausible
    // readings is a re-anchor, not a direction, so it starts the count
    // over.
    if n >= 3 {
        let d1 = before[n - 2] - before[n - 3];
        let d2 = before[n - 1] - before[n - 2];
        let d3 = x - before[n - 1];
        let swing = |a: f64, b: f64| a.abs() + b.abs();
        let reversed = |a: f64, b: f64| a * b < 0.0 && s.past(swing(a, b)) && s.past(-swing(a, b));
        let anchored = !s.past(d1) && !s.past(d2);
        if anchored && reversed(d1, d2) && reversed(d2, d3) {
            return Some(format!(
                "flapping: reversed by more than {s} twice in a row"
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

/// Why a reading code would otherwise believe is unconfirmed, if it is:
/// the report just before it, `last`, is implausible and no declared
/// approval since vouches for it. One that is itself unconfirmed is
/// confirmed by this reading when it is a plausible step from it by `s`;
/// where the metric varies, an exact repeat away from a bound of the
/// range is a stuck sensor's and never confirms. A flap's first reading
/// back is never a premise; the one after it, agreeing, is.
#[allow(clippy::too_many_arguments)]
fn unconfirmed(
    index: &Index,
    venture: &str,
    metric: &str,
    last: &Entry,
    x: f64,
    s: Steps,
    range: Option<[f64; 2]>,
    varies: bool,
) -> Option<String> {
    let Record::Report {
        value, implausible, ..
    } = &last.record
    else {
        return None;
    };
    let why = implausible.as_deref()?;
    let vouched = index
        .of(venture)
        .rev()
        .take_while(|e| e.seq > last.seq)
        .any(|e| vouches(index, e, venture, metric));
    if vouched {
        return None;
    }
    let bound = range.is_some_and(|[lo, hi]| x == lo || x == hi);
    if let Some(prev) = value.filter(|_| why.starts_with(UNCONFIRMED)) {
        let repeat = x == prev && varies && !bound;
        if !s.past(x - prev) && !repeat {
            return None;
        }
    }
    Some(format!(
        "{UNCONFIRMED}: {x} follows an implausible reading; it is a premise once the next agrees with it within {s}"
    ))
}

/// How the why of an unconfirmed reading begins.
pub const UNCONFIRMED: &str = "unconfirmed";

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
        self.signal_citing(venture, question, signal, &[], vec![]);
    }

    /// `signal`, its packet's premises also the measured and declared
    /// records among `cites`, and its packet carrying `policies`, what a
    /// forecast says under each policy for the asks.
    pub(super) fn signal_citing(
        &mut self,
        venture: &str,
        question: String,
        signal: Signal,
        cites: &[crate::record::Seq],
        policies: Vec<crate::record::PolicyForecast>,
    ) {
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
        let packet = self
            .signal_packet_citing(venture, &signal, cites)
            .map(|p| crate::record::Packet { policies, ..p });
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
        steps: Option<Steps>,
        confirm: bool,
    ) -> (Option<String>, bool) {
        let Ok(index) = self.index() else {
            return (None, false);
        };
        let before = anchored_before(&index, venture, metric, HISTORY);
        let history = index.stretches(venture, metric);
        let history = &history[history.len().saturating_sub(WINDOW + 1)..];
        let recent = recent(&index, venture, metric, RECENT);
        // The readings just before that equal this one exactly. A missing
        // or out-of-range reading says nothing about the value and is
        // skipped. Did a
        // lever that expects to move this metric run, ok, since the first
        // of them?
        let inside = |v: f64| v.is_finite() && range.is_none_or(|[lo, hi]| v >= lo && v <= hi);
        let plateau: Vec<Seq> = value.map_or_else(Vec::new, |x| {
            reports(&index, venture, metric)
                .filter_map(|e| match &e.record {
                    Record::Report { value: Some(v), .. } if inside(*v) => Some((e.seq, *v)),
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
            steps,
            Plateau {
                repeats: plateau.len(),
                moved,
                vouched: value.is_some_and(|x| index.vouched_run(venture, metric) == Some(x)),
                history,
            },
        );
        let last = reports(&index, venture, metric).next();
        let why = why.or_else(|| {
            let x = value?;
            let s = steps.filter(|_| confirm)?;
            // Believed past its bound from the last plausible reading, it
            // re-anchored: AGREE readings agree, the confirmation already.
            if before.last().is_some_and(|p| s.past(x - p)) {
                return None;
            }
            let varies = median_step(&before) > 0.0;
            unconfirmed(&index, venture, metric, last?, x, s, range, varies)
        });
        let fresh = last.is_none_or(|e| e.record.implausible().is_none());
        (why, fresh)
    }

    /// Write a guard's or an audit's reading as a measured report, marked
    /// when it is implausible. Returns the entry, and why when the reading
    /// starts an episode of implausible ones.
    #[allow(clippy::too_many_arguments)]
    fn write_reading(
        &mut self,
        v: &Venture,
        metric: &str,
        value: Option<f64>,
        note: &str,
        range: Option<[f64; 2]>,
        steps: Option<Steps>,
        confirm: bool,
    ) -> Option<(Entry, Option<String>)> {
        let (why, fresh) = self.doubt(&v.name, metric, value, range, steps, confirm);
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
                self.write_reading(v, &metric, value, &note, g.range, g.steps(), g.confirm)
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
                self.write_reading(v, &metric, value, &note, a.range, a.steps(), a.confirm)
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
            // The declared unwind first: with then: pause, the stop after
            // it would hold it.
            if let Some(u) = &g.unwind {
                if self.unwind_due(name, &g, e.seq) {
                    self.act(name, &u.action, vec![e.seq], Claim::Measured);
                }
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

    /// Is a guard's unwind to be tried on this reading past its bound? The
    /// rule: once per episode, an episode being the readings since the
    /// last plausible one inside the bound. It is tried on the reading
    /// that trips the guard and on each later one past it, until one run
    /// of it in the episode has an ok outcome. Not while the venture is
    /// paused (nothing but stop runs then), and not while a run of it has
    /// an intent and no outcome (never rerun blindly).
    fn unwind_due(&self, name: &str, g: &Guard, reading: Seq) -> bool {
        let Some(u) = &g.unwind else { return false };
        let Ok(index) = self.index() else {
            return false;
        };
        if crate::gate::state_in(&index, &self.adopted(), name) == crate::gate::State::Paused {
            return false;
        }
        let metric = g.metric();
        let start = reports(&index, name, &metric)
            .filter(|r| r.seq < reading)
            .find(|r| plausible(r).is_some_and(|x| !g.breached(x)))
            .map_or(0, |r| r.seq);
        let ran = index
            .intents_of(name, &u.action)
            .iter()
            .rev()
            .take_while(|i| **i > start)
            .filter(|i| {
                index.get(**i).is_some_and(|e| {
                    matches!(&e.record, Record::Intent { proof, .. }
                        if proof.unwind() == Some(g.name.as_str()))
                })
            })
            .any(|i| index.settled(*i).is_none_or(|(_, ok)| ok));
        !ran
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
        implausible(value, before, &[], m.range, m.steps(), Plateau::default())
    }

    /// Readings as the index keeps them: stretches of equal ones.
    fn stretches(values: &[f64]) -> Vec<(f64, usize)> {
        let mut out = Vec::new();
        for &x in values {
            crate::index::stretch(&mut out, x);
        }
        out
    }

    /// A run of `repeats` judged by `history`, with no lever run.
    fn run(repeats: usize, history: &[(f64, usize)]) -> Plateau<'_> {
        Plateau {
            repeats,
            moved: false,
            vouched: false,
            history,
        }
    }

    #[test]
    fn three_agreeing_readings_re_anchor_and_nothing_less_does() {
        let (range, step) = (Some([0.0, 1000.0]), Steps::of(Some(50.0), None, None));
        let anchor = [60.0, 60.0, 60.0];
        let history = stretches(&anchor);
        let at = |x: f64, recent: &[Option<f64>]| {
            implausible(
                Some(x),
                &anchor,
                recent,
                range,
                step,
                run(reps(x, recent), &history),
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
            implausible(Some(1.0), &after, &[], range, step, Plateau::default()),
            None
        );
    }

    #[test]
    fn exact_repeats_are_stuck_where_the_metric_varies_and_never_agree() {
        let (range, step) = (Some([0.0, 1000.0]), Steps::of(Some(50.0), None, None));
        let varied = [40.0, 42.0, 41.0, 43.0, 41.0];
        let history = stretches(&varied);
        let at = |x: f64, recent: &[Option<f64>]| {
            implausible(
                Some(x),
                &varied,
                recent,
                range,
                step,
                run(reps(x, recent), &history),
            )
        };
        // Four clean steps: three repeats have chance 1/6 * 2/7 * 3/8 =
        // 0.018, four 0.0079.
        assert_eq!(at(41.0, &[Some(41.0), Some(41.0), Some(41.0)]), None);
        let stuck = at(41.0, &[Some(41.0), Some(41.0), Some(41.0), Some(41.0)]).unwrap();
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
            implausible(
                Some(50.0),
                &flat,
                &same,
                range,
                step,
                run(3, &stretches(&flat))
            ),
            None
        );
        assert_eq!(median_step(&flat), 0.0);
        assert_eq!(median_step(&varied), 2.0);
    }

    /// Feed these readings in order as `doubt` would: each judged by the
    /// plausible ones before it, and kept as history only when plausible.
    /// Returns why each one is implausible.
    fn feed(readings: &[f64]) -> Vec<Option<String>> {
        let (range, step) = (Some([0.0, 100.0]), Steps::of(Some(30.0), None, None));
        let mut history: Vec<(f64, usize)> = Vec::new();
        let mut raw: Vec<Option<f64>> = Vec::new();
        let mut out = Vec::new();
        for &x in readings {
            let from = history.len().saturating_sub(WINDOW + 1);
            let flat: Vec<f64> = history
                .iter()
                .flat_map(|&(v, n)| std::iter::repeat_n(v, n))
                .collect();
            let before = &flat[flat.len().saturating_sub(HISTORY)..];
            let recent = &raw[raw.len().saturating_sub(RECENT)..];
            let why = implausible(
                Some(x),
                before,
                recent,
                range,
                step,
                run(reps(x, &raw), &history[from..]),
            );
            if why.is_none() {
                crate::index::stretch(&mut history, x);
            }
            raw.push(Some(x));
            out.push(why);
        }
        out
    }

    /// `n` readings of a continuous sensor that never repeat exactly.
    fn continuous(n: usize) -> Vec<f64> {
        (0..n)
            .map(|i| 50.0 + 20.0 * (i as f64 * 0.7).sin() + i as f64 * 1e-3)
            .collect()
    }

    #[test]
    fn a_continuous_sensor_is_stuck_on_its_fourth_identical_reading() {
        let mut r = continuous(21);
        let x = r[20] + 0.5;
        r.extend([x, x, x, x]);
        let why = feed(&r);
        assert!(why[..22].iter().all(Option::is_none), "{why:?}");
        assert_eq!(why[22], None, "two identical readings: p 20/23 = 0.87");
        assert_eq!(why[23], None, "three: p 19 * 1/23 * 2/24 = 0.072");
        let stuck = why[24].as_deref().unwrap();
        assert!(stuck.starts_with("stuck"), "{stuck}");
        assert!(
            stuck.contains(
                "after 0 repeats in 21 steps, a run of 3 anywhere in as many has chance at most 0.0083 < 0.01"
            ),
            "{stuck}"
        );
    }

    #[test]
    fn one_coincidental_repeat_after_a_long_clean_history_is_not_stuck() {
        // The held-out p95: read to 0.1 ms, it repeats by chance now and
        // then. After 0 repeats in 256 steps one repeat has chance 1/258,
        // but a run of one somewhere in 256 steps is all but certain.
        let mut r = continuous(300);
        let x = r[299] + 0.5;
        r.extend([x, x]);
        r.extend(&continuous(400)[300..]);
        let why = feed(&r);
        assert!(why.iter().all(Option::is_none), "{why:?}");
    }

    #[test]
    fn a_coarse_sensor_that_often_repeats_is_not_stuck_on_a_plateau() {
        // A third of its steps are exact repeats, and its median step is 1.
        let cycle = [
            40.0, 40.0, 41.0, 42.0, 42.0, 43.0, 44.0, 44.0, 43.0, 42.0, 42.0, 41.0,
        ];
        let mut r: Vec<f64> = cycle.iter().copied().cycle().take(60).collect();
        assert!(median_step(&r) > 0.0);
        r.extend([43.0; 4]);
        let why = feed(&r);
        assert!(why.iter().all(Option::is_none), "{why:?}");
    }

    #[test]
    fn a_metric_frozen_after_a_long_varied_history_stays_stuck() {
        // The dead plant: 300 varied readings, then the sensor reads one
        // value for good. Stuck readings never enter the history, so the
        // 50th is judged by the same varied steps as the third.
        let mut r = continuous(300);
        r.extend([r[299] + 0.5; 50]);
        let why = feed(&r);
        assert!(why[..300].iter().all(Option::is_none));
        let frozen = &why[300..];
        assert_eq!(frozen[..2], [None, None], "one repeat is a coincidence");
        assert!(
            frozen[2..]
                .iter()
                .all(|w| w.as_deref().is_some_and(|w| w.starts_with("stuck"))),
            "{frozen:?}"
        );
        // A clean history of WINDOW steps makes two repeats unlikely
        // anywhere in it: p 255 * 1/258 * 2/259.
        let third = frozen[2].as_deref().unwrap();
        assert!(
            third.contains(
                "in 256 steps, a run of 2 anywhere in as many has chance at most 0.0076 < 0.01"
            ),
            "{third}"
        );

        // Dry at its floor for longer than WINDOW, then reading 12 for
        // good: a repeat at the floor is no step, so 12 is judged by the
        // history from when the metric varied.
        let mut r = continuous(300);
        while r[r.len() - 1] > 0.0 {
            r.push((r[r.len() - 1] - 10.0).max(0.0));
        }
        let dry = r.len();
        r.extend([0.0; 400]);
        r.extend([12.0; 10]);
        let why = feed(&r);
        assert!(why[..dry + 402].iter().all(Option::is_none), "{why:?}");
        assert!(
            why[dry + 402..]
                .iter()
                .all(|w| w.as_deref().is_some_and(|w| w.starts_with("stuck"))),
            "{:?}",
            &why[dry + 400..]
        );
    }

    #[test]
    fn a_metric_that_never_varied_is_never_stuck_and_one_that_did_is() {
        assert!(feed(&[50.0; 300]).iter().all(Option::is_none));
        // Once varied, always varies: a few steps, then one value for good,
        // is stuck once the run is unlikely by those steps: after 0
        // repeats in 3, a run of 4 has chance 1/5 * 2/6 * 3/7 * 4/8 =
        // 0.014, of 5 0.0079.
        let mut r = vec![47.0, 48.0, 49.0, 50.0];
        r.extend([50.0; 100]);
        let why = feed(&r);
        assert!(why[..8].iter().all(Option::is_none), "{why:?}");
        assert!(why[8..].iter().all(Option::is_some), "{why:?}");
    }

    #[test]
    fn a_quiet_metric_that_changed_once_is_not_stuck_on_a_short_run() {
        // The held-out savings: one change in two steps says little of how
        // often it repeats. A run of r has chance 2/4 * 3/5 * ... =
        // 6/((r+2)(r+3)): 7 is 0.067, not the 0.0078 of (1/2)^7; 22 is
        // 0.010 and 23 the first below ALPHA, 0.0092.
        let mut r = vec![50.0, 50.0, 53.0];
        r.extend([53.0; 30]);
        let why = feed(&r);
        assert!(why[..2 + 23].iter().all(Option::is_none), "{why:?}");
        let stuck = why[2 + 23].as_deref().unwrap();
        assert!(
            stuck.contains(
                "after 1 repeats in 2 steps, a run of 23 anywhere in as many has chance at most 0.0092 < 0.01"
            ),
            "{stuck}"
        );
    }

    #[test]
    fn a_run_is_judged_by_the_posterior_predictive() {
        let close = |a: Option<f64>, b: f64| a.is_some_and(|a| (a - b).abs() < 1e-4);
        assert!(
            close(run_chance(2, 1, 7), 0.0667),
            "{:?}",
            run_chance(2, 1, 7)
        );
        // A run of r can start at n - r + 1 of n steps.
        assert!(close(run_chance(20, 0, 1), 20.0 / 22.0));
        assert!(close(run_chance(20, 0, 2), 19.0 / 22.0 * 2.0 / 23.0));
        assert!(close(run_chance(256, 0, 1), 256.0 / 258.0));
        assert!(close(run_chance(256, 0, 2), 255.0 / 258.0 * 2.0 / 259.0));
        // A run longer than the history can start in one place.
        assert!(close(
            run_chance(4, 0, 4),
            1.0 / 6.0 * 2.0 / 7.0 * 3.0 / 8.0 * 4.0 / 9.0
        ));
        // It stops once below ALPHA: a longer run says no more.
        assert_eq!(run_chance(4, 0, 6), run_chance(4, 0, 4));
        assert_eq!(run_chance(5, 5, 3), None, "never varied");
        assert_eq!(run_chance(0, 0, 3), None, "no history");
        assert!(run_chance(2, 1, 22).is_some_and(|c| c >= ALPHA));
        assert!(run_chance(2, 1, 23).is_some_and(|c| c < ALPHA));
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
