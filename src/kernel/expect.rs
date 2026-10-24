//! Expectations: what a delegated lever's runs did to the metric. A run
//! the mandate expects something of is judged once enough reports follow
//! it. The right way by the wrong size is a measured report, the input to
//! the baseline. No effect or the wrong way is a miss, and so is a failed
//! run when the lever says `revoke_after`. A run whose step spans a
//! reading code marked implausible is never judged, unless that reading is
//! stuck at the value from before the run: the lever did nothing, and the
//! reading that says so only restricts. Enough misses in a row
//! write a measured ask and the lever is suspended, reversible or not:
//! every run is refused, and none asks, until a declared approval of that
//! ask or a declared adoption of the mandate after it. One ask an episode:
//! a restored lever that misses again is suspended again, by a measured
//! relapse report with no ask unless the metric was measured somewhere new
//! since the last ask; a suspended lever that is refused asks again only
//! then too. Tightening is measured; loosening is declared, with one
//! exception: a miss seen only on readings stuck at the value from before
//! the run holds the lever only while the metric stays frozen. Its ask
//! cites those readings, and the first plausible reading since that is
//! away from the frozen value voids it: the lever runs again on its own
//! premises, with no human. A frozen sensor never gives one. Nor does
//! such a miss hold a tripped guard's declared unwind: the guard's own
//! reading is measured and plausible, and the frozen one cannot show
//! what the lever did. For the same reason an unwind that ran past it is
//! never judged, and never a miss that suspends the lever once the
//! sensor reads again.
//!
//! A run is judged first at the `within` its mandate declares. A miss
//! there may be late, not ineffective: a lever whose own cap holds it
//! once a period (its window) is judged again, within that window, at
//! the delay its runs have shown (learned from the metric, inferred), and
//! after a move the wrong way at any delay, since a lagging reading shows
//! the course from before the run. It is a miss only if no span meets.
//! A run that met at its declared horizon is never judged later, so a
//! learned delay only waits on or withholds a miss; it never authorizes,
//! and a run met at a delay its lever's runs had not shown asks, inferred,
//! with nothing to run on the answer.
//!
//! A run whose declared horizon reads nothing but exact repeats of the
//! value from before it saw no fresh reading. While a sensor that repeats
//! as often as the metric's history says would plausibly still hold that
//! value (the posterior predictive of that many repeats is at least
//! ALPHA, `expect::horizon`), the run is waited on, and the gate holds
//! the lever: the first reading that differs judges it. Repeated longer
//! than that, the sensor would have refreshed, and the miss stands.
//!
//! Absence of evidence is not evidence of no effect. A run whose history
//! is too thin to judge by, fewer than `world::MIN_SAMPLES` steps the
//! metric took on its own to learn its drift from, or too few steps to
//! say how long its sensor holds a reading, is not judged yet: neither a
//! hit nor a miss, and the lever may run again once its declared horizon
//! has read.

use crate::expect::{self, Horizon, Judgement};
use crate::index::{Index, EXPECTATION, SUSPENSION};
use crate::mandate::{Expects, Mandate, Permitted};
use crate::record::{Claim, Record, Seq, Signal};
use crate::world;

use super::{log, Kernel};

/// How one run went, as far as the lever's expectations care.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Run {
    /// As expected, or nothing was expected of it.
    Hit,
    /// The run failed.
    Failed,
    /// The metric did not move as expected; the words say how.
    Ineffective(String),
    /// No effect seen, from this reading flagged stuck at the value from
    /// before the run alone: a miss that holds only while the metric
    /// stays frozen.
    Frozen(String, Seq),
    /// A reading the step spans is implausible: neither a hit nor a miss.
    Unjudged,
}

impl Kernel {
    /// Judge the runs of every lever of a venture that has expectations
    /// or a `revoke_after`, and write what follows.
    pub(super) fn judge(&mut self, venture: &str) {
        let Some(v) = self.ventures.get(venture).cloned() else {
            return;
        };
        let metric = v.mandate.metric_name().to_string();
        // Simulated with neither a `--world` nor a `world:` of its own,
        // the venture reads the baseline, whose lever effects are only
        // what the ledger has measured before: a fresh home has none, and
        // the metric sits frozen. What it reads after a run is the model
        // agreeing with itself, no evidence of what the lever does, so the
        // run's expectation is not judged: neither a hit nor a miss.
        // A `--world` that printed no reading for the metric leaves the
        // baseline answering, and that is no evidence either.
        let baseline = self
            .sim
            .as_ref()
            .is_some_and(|s| !s.world_told(venture, &metric))
            && v.mandate.world.is_none();
        for perm in &v.mandate.permitted {
            if perm.expects.is_none() && perm.revoke_after.is_none() {
                continue;
            }
            self.hold_on_faults(venture, &perm.name, &metric);
            let mut memo: Vec<(Seq, Run)> = Vec::new();
            let mut records: Vec<Record> = Vec::new();
            let mut ask: Option<(String, Option<Seq>)> = None;
            let mut cites: Vec<Seq> = Vec::new();
            // Runs that showed their effect only at a delay their lever's
            // runs had not shown: the run, the delay, the learned one.
            let mut late: Vec<(Seq, usize, usize)> = Vec::new();
            {
                let Ok(index) = self.index() else { return };
                let floor = index.last_suspension(venture, &perm.name).unwrap_or(0);
                let mut seen: Vec<Run> = Vec::new();
                for &intent in index.intents_of(venture, &perm.name) {
                    if intent <= floor {
                        continue;
                    }
                    if let Some(known) = self.judged.get(&intent) {
                        seen.push(known.clone());
                        continue;
                    }
                    let Some((outcome, ok)) = index.settled(intent) else {
                        break;
                    };
                    // The first readings after the run, until a plausible one:
                    // one flagged stuck at the last plausible value from
                    // before the run says the lever did nothing. The
                    // reading stays implausible; it only restricts.
                    let stuck = perm.expects.as_ref().and_then(|_| {
                        let series = index.series(venture, &metric);
                        let b = series.partition_point(|(s, _)| *s < intent);
                        let frozen = series[..b].last()?.1;
                        index
                            .of(venture)
                            .filter(|e| e.seq > outcome)
                            .filter_map(|e| match &e.record {
                                Record::Report {
                                    metric: m,
                                    claim,
                                    value: Some(x),
                                    implausible,
                                    ..
                                } if *m == metric && claim.authorizes() => {
                                    Some((e.seq, *x, implausible.as_deref()))
                                }
                                _ => None,
                            })
                            .take_while(|(_, _, flag)| flag.is_some())
                            .find(|(_, x, flag)| {
                                *x == frozen && flag.is_some_and(|f| f.starts_with("stuck"))
                            })
                            .map(|(seq, x, _)| (seq, x))
                    });
                    // An unwind that passed a suspension seen only on
                    // stuck readings ran while the metric's sensor was
                    // found frozen: that sensor cannot judge it, whatever
                    // it reads next.
                    let past_stuck = index.get(intent).is_some_and(
                        |e| matches!(&e.record, Record::Intent { proof, .. } if proof.past_stuck()),
                    );
                    let result = if !ok {
                        Run::Failed
                    } else if past_stuck || (baseline && perm.expects.is_some()) {
                        Run::Unjudged
                    } else if let Some((seen, x)) = stuck {
                        Run::Frozen(
                            format!(
                                "no effect seen: {metric} stuck at {x} after {} ran; {} expects {}",
                                perm.name,
                                perm.name,
                                perm.expects
                                    .as_ref()
                                    .map_or_else(String::new, |e| e.to_string()),
                            ),
                            seen,
                        )
                    } else if let Some(expects) = &perm.expects {
                        let Some(seen) =
                            judged(&index, venture, &v.mandate, perm, expects, intent, outcome)
                        else {
                            break;
                        };
                        if let Some((d, learned)) = seen.late.filter(|(d, l)| d != l) {
                            late.push((intent, d, learned));
                        }
                        match seen.judgement {
                            Judgement::Waiting => break,
                            _ if seen.doubted || seen.thin => Run::Unjudged,
                            Judgement::Unknown | Judgement::Met => Run::Hit,
                            Judgement::WrongSize { observed, noise, .. } => {
                                if !index.expectation_reported(intent) {
                                    records.push(Record::Report {
                                        claim: Claim::Measured,
                                        venture: venture.to_string(),
                                        metric: EXPECTATION.to_string(),
                                        value: Some(observed),
                                        note: format!(
                                            "{} expects {expects}; {metric} moved {observed:+} (noise {noise})",
                                            perm.name
                                        ),
                                        source: None,
                                        confidence: None,
                                        cites: vec![intent],
                                        implausible: None,
                                        facts: None,
                                    });
                                }
                                Run::Hit
                            }
                            Judgement::NoEffect {
                                observed,
                                effect,
                                noise,
                            } => Run::Ineffective(format!(
                                "no effect: {} expects {expects}; {metric} moved {observed:+}, {effect:+} net of drift, against noise {noise}",
                                perm.name
                            )),
                            Judgement::WrongDirection {
                                observed,
                                effect,
                                noise,
                            } => Run::Ineffective(format!(
                                "wrong direction: {} expects {expects}; {metric} moved {observed:+}, {effect:+} net of drift, against noise {noise}",
                                perm.name
                            )),
                        }
                    } else {
                        Run::Hit
                    };
                    memo.push((intent, result.clone()));
                    seen.push(result);
                }
                // Misses in a row, newest first. A failed run is a miss
                // only where the lever says how many to bear.
                let needed = perm.revoke_after.unwrap_or(1) as usize;
                let mut misses: Vec<&Run> = Vec::new();
                for r in seen.iter().rev() {
                    match r {
                        Run::Ineffective(_) | Run::Frozen(..) => misses.push(r),
                        Run::Failed if perm.revoke_after.is_some() => misses.push(r),
                        Run::Unjudged => continue,
                        _ => break,
                    }
                }
                if misses.len() >= needed
                    && !misses.is_empty()
                    && !index.suspended(venture, &perm.name)
                {
                    let why = match misses[0] {
                        Run::Ineffective(why) | Run::Frozen(why, _) if misses.len() == 1 => {
                            why.clone()
                        }
                        Run::Ineffective(why) | Run::Frozen(why, _) => {
                            format!("{} misses in a row; the last: {why}", misses.len())
                        }
                        _ => format!("{} runs in a row failed", misses.len()),
                    };
                    // Misses seen only on stuck readings are cited: they
                    // hold the lever only while the metric stays frozen.
                    let seen: Vec<Seq> = misses
                        .iter()
                        .map(|r| match r {
                            Run::Frozen(_, s) => Some(*s),
                            _ => None,
                        })
                        .collect::<Option<_>>()
                        .unwrap_or_default();
                    cites = seen;
                    // The first miss of an episode asks. A restored lever
                    // that misses again asks only on new evidence since
                    // the last ask; otherwise it relapses, measured and
                    // silent. An adoption starts a new episode.
                    let adopted = index.adoptions().get(venture).map_or(0, |a| a.seq);
                    let last = index
                        .last_ineffective(venture, &perm.name)
                        .filter(|q| *q > adopted);
                    ask = Some(match last {
                        None => (why, None),
                        Some(q) => match index.fresh_evidence(venture, &metric, q, v.mandate.range)
                        {
                            Some((was, now, readings)) => {
                                cites.extend(readings);
                                (
                                    format!(
                                        "{why}; {metric} read {was} at ask #{q} and {now} since"
                                    ),
                                    None,
                                )
                            }
                            None => (why, Some(q)),
                        },
                    });
                }
            }
            self.judged.extend(memo);
            for record in records {
                if let Err(e) = self.append(record) {
                    log(&format!("{venture}: cannot write expectation: {e}"));
                }
            }
            for (intent, d, learned) in late {
                self.ask_late(venture, &perm.name, intent, d, learned);
            }
            match ask {
                Some((why, None)) => self.ask_ineffective(venture, &perm.name, why, cites),
                Some((why, Some(q))) => {
                    let record = Record::Report {
                        claim: Claim::Measured,
                        venture: venture.to_string(),
                        metric: SUSPENSION.to_string(),
                        value: None,
                        note: format!(
                            "{} missed again after ask #{q} restored it: {why}; suspended again, and nothing new was measured since to ask on",
                            perm.name
                        ),
                        source: None,
                        confidence: None,
                        cites: [vec![q], cites].concat(),
                        implausible: None,
                        facts: None,
                    };
                    match self.append(record) {
                        Ok(e) => log(&e.line()),
                        Err(e) => log(&format!("{venture}: cannot write relapse: {e}")),
                    }
                }
                None => {}
            }
        }
    }

    /// Ask about a run whose effect showed `d` reports late where its
    /// lever's runs had shown it `learned` late: the delay is inferred,
    /// and so is the ask. It cites the run, once, and nothing runs on it.
    fn ask_late(&mut self, venture: &str, lever: &str, intent: Seq, d: usize, learned: usize) {
        let (asked, may_ask, metric, expects) = {
            let Ok(index) = self.index() else { return };
            let Some(v) = self.ventures.get(venture) else {
                return;
            };
            let asked = index.of(venture).any(|e| {
                matches!(&e.record, Record::Ask { cites, action: None, signal: None, .. }
                    if cites.contains(&intent))
            });
            let expects = v
                .mandate
                .permitted(lever)
                .and_then(|p| p.expects.as_ref())
                .map_or_else(String::new, ToString::to_string);
            (
                asked,
                self.may_ask(&index),
                v.mandate.metric_name().to_string(),
                expects,
            )
        };
        if asked || !may_ask {
            return;
        }
        let question = format!(
            "{lever} ran at #{intent} and {metric} showed what it expects ({expects}) {d} reports late, where its runs have shown it {learned} late: the sensor may lag, or {lever} works slower; a run that shows late is not ineffective, and nothing runs on this ask"
        );
        let packet = self.packet(venture, None, &[intent], None, false);
        match self.append(Record::Ask {
            cites: vec![intent],
            claim: Claim::Inferred,
            venture: venture.to_string(),
            action: None,
            question,
            refusal: None,
            payload: None,
            signal: None,
            packet,
        }) {
            Ok(e) => log(&e.line()),
            Err(e) => log(&format!("{venture}: cannot write ask: {e}")),
        }
    }

    /// Write the measured ask that suspends `lever`: one an episode. It
    /// cites the stuck readings that alone saw a miss, if that is all.
    pub(super) fn ask_ineffective(
        &mut self,
        venture: &str,
        lever: &str,
        why: String,
        cites: Vec<Seq>,
    ) {
        // A lever that never ran on its own is let run again, not delegated.
        let delegated = self
            .ventures
            .get(venture)
            .and_then(|v| v.mandate.permitted(lever))
            .is_some_and(|p| p.delegated);
        let again = if delegated {
            "delegate it again"
        } else {
            "let it run again"
        };
        let question = format!(
            "{lever} is ineffective, so it is refused and asks no more: {why}; approve to {again}"
        );
        let signal = Signal::Ineffective {
            action: lever.to_string(),
            why,
        };
        let packet = self.signal_packet_citing(venture, &signal, &cites);
        match self.append(Record::Ask {
            cites,
            claim: Claim::Measured,
            venture: venture.to_string(),
            action: None,
            question,
            refusal: None,
            payload: None,
            signal: Some(signal),
            packet,
        }) {
            Ok(e) => log(&e.line()),
            Err(e) => log(&format!("{venture}: cannot write ask: {e}")),
        }
    }

    /// A suspended lever was refused: ask again when no ask about it is
    /// open and the metric was measured somewhere new since the last one.
    pub(super) fn reask_ineffective(&mut self, venture: &str, lever: &str) {
        let found = {
            let Ok(index) = self.index() else { return };
            let Some(v) = self.ventures.get(venture) else {
                return;
            };
            let metric = v.mandate.metric_name().to_string();
            let Some(last) = index.last_ineffective(venture, lever) else {
                return;
            };
            if index.is_open(last) {
                return;
            }
            index
                .fresh_evidence(venture, &metric, last, v.mandate.range)
                .map(|(was, now, readings)| {
                    (
                        format!(
                            "found ineffective before, at ask #{last}; {metric} read {was} then and {now} since"
                        ),
                        readings.to_vec(),
                    )
                })
        };
        if let Some((why, readings)) = found {
            self.ask_ineffective(venture, lever, why, readings);
        }
    }

    /// An ineffective ask that may yet restore `lever`, open or approved,
    /// rested on a reading a later report found a fault (`impugned`): a
    /// stuck sensor's first reading is plausible when it comes. A measured
    /// relapse after the ask holds the lever again, so that ask restores
    /// nothing, and it is a miss the next packet cites. It cites the ask
    /// and the reading, never the flagged report, so no reading voids it;
    /// only a declared approval of a later ask restores the lever.
    fn hold_on_faults(&mut self, venture: &str, lever: &str, metric: &str) {
        let found = {
            let Ok(index) = self.index() else { return };
            let adopted = index.adoptions().get(venture).map_or(0, |a| a.seq);
            let Some(q) = index
                .last_ineffective(venture, lever)
                .filter(|q| *q > adopted && !index.voided(venture, *q))
            else {
                return;
            };
            // Held already, by a relapse since or by an answer of no.
            let relapsed = index.last_suspension(venture, lever) != Some(q);
            let live = index.is_open(q) || index.restored(venture, lever).is_some();
            if relapsed || !live {
                return;
            }
            let cites = index.get(q).map(|e| e.record.cites().to_vec());
            cites.into_iter().flatten().find_map(|s| {
                let Record::Report {
                    metric: m,
                    value: Some(x),
                    implausible: None,
                    ..
                } = &index.get(s)?.record
                else {
                    return None;
                };
                if m != metric {
                    return None;
                }
                let flag = index.impugned(venture, metric, s)?;
                Some((q, s, *x, flag))
            })
        };
        let Some((q, s, x, flag)) = found else { return };
        let record = Record::Report {
            claim: Claim::Measured,
            venture: venture.to_string(),
            metric: SUSPENSION.to_string(),
            value: None,
            note: format!(
                "ask #{q} about {lever} rested on {metric}={x} at #{s}, which #{flag} found a fault of the sensor; {lever} is suspended again, and that ask restores nothing"
            ),
            source: None,
            confidence: None,
            cites: vec![q, s],
            implausible: None,
            facts: None,
        };
        match self.append(record) {
            Ok(e) => log(&e.line()),
            Err(e) => log(&format!("{venture}: cannot write relapse: {e}")),
        }
    }
}

/// A run judged at the delay its effect shows.
struct Seen {
    judgement: Judgement,
    /// An implausible reading falls in the span the declared horizon reads.
    doubted: bool,
    /// The history before the run is too thin to judge it by: too few
    /// steps to learn the drift from, or to say how long the sensor holds
    /// a reading (`Horizon::Thin`). Not judged yet.
    thin: bool,
    /// The delay, in reports, at which a run that missed at its declared
    /// horizon showed its effect, and the delay its lever's runs had
    /// shown before.
    late: Option<(usize, usize)>,
}

/// One span a run is judged over.
enum Span {
    /// No report before the run to compare with.
    Unknown,
    /// Its last report has not come yet.
    Waiting,
    /// It ends after the lever may next run: past the run's window.
    Past,
    /// What the metric moved over it, the drift and noise over it, and
    /// whether a reading it spans is implausible.
    Read {
        observed: f64,
        drift: f64,
        noise: f64,
        doubted: bool,
    },
}

/// How long after a run its lever's own cap keeps it from running
/// again, in seconds: the window its effect may show late in. A lever
/// that may run once a period has that period; one that may run again
/// sooner (two a day may be two in a row) has none, since a run waited
/// on past the gate's own hold of `within` reports could be run again
/// before it is judged: acting twice on one error. A late judgement
/// never lets the lever run before it.
fn window(perm: &Permitted) -> i64 {
    match perm.cap.count {
        1 => perm.cap.per.as_secs() as i64,
        _ => 0,
    }
}

/// The span of a run, `intent` to `outcome`, `d` reports late: from the
/// last report before the run (`d` 0, the declared horizon) or the
/// `d`-th after it, to `within` reports on. A late span ends inside the
/// run's window or is past it.
#[allow(clippy::too_many_arguments)]
fn span(
    index: &Index,
    venture: &str,
    mandate: &Mandate,
    perm: &Permitted,
    expects: &Expects,
    intent: Seq,
    outcome: Seq,
    d: usize,
) -> Span {
    let metric = mandate.metric_name();
    let series = index.series(venture, metric);
    let b = series.partition_point(|(s, _)| *s < intent);
    let a = series.partition_point(|(s, _)| *s <= outcome);
    let at = |s: Seq| index.get(s).map_or(0, |e| e.at);
    let base = match d {
        0 => match b.checked_sub(1) {
            Some(i) => i,
            None => return Span::Unknown,
        },
        _ => a + d - 1,
    };
    let end = at(intent) + window(perm);
    let Some(&(s0, x0)) = series.get(base) else {
        return Span::Waiting;
    };
    if d > 0 && at(s0) >= end {
        return Span::Past;
    }
    let Some(&(s1, x1)) = series.get(a + d + expects.within as usize - 1) else {
        return Span::Waiting;
    };
    if d > 0 && at(s1) > end {
        return Span::Past;
    }
    // The counterfactual: what the metric would have moved on its own
    // over the span, by the recent drift before the run.
    let (drift, noise) =
        world::counterfactual(index, venture, mandate, intent, at(s0), at(s1) - at(s0));
    Span::Read {
        observed: x1 - x0,
        drift,
        noise,
        // A step that spans an implausible reading says nothing about
        // the run: the sensor, not the lever.
        doubted: index.doubted_between(venture, metric, s0, s1),
    }
}

/// The delay at which the runs of `perm` before `intent` show their
/// effect best (`expect::delay`), over the windows they have whole: a
/// suspension does not forget how late its runs were. Inferred from the
/// metric, never more.
fn learned_delay(
    index: &Index,
    venture: &str,
    mandate: &Mandate,
    perm: &Permitted,
    expects: &Expects,
    intent: Seq,
) -> usize {
    // A lever with no window is judged at its declared horizon alone.
    if window(perm) == 0 {
        return 0;
    }
    let mut runs = Vec::new();
    for &r in index.intents_of(venture, &perm.name) {
        if r >= intent {
            break;
        }
        let Some((outcome, true)) = index.settled(r) else {
            continue;
        };
        let mut line = Vec::new();
        for d in 0.. {
            match span(index, venture, mandate, perm, expects, r, outcome, d) {
                Span::Read {
                    observed,
                    drift,
                    noise,
                    doubted: false,
                } => line.push((expect::toward(expects, observed - drift), noise)),
                Span::Past => {
                    runs.push(line);
                    break;
                }
                // Not whole yet, or a reading in it is doubted.
                _ => break,
            }
        }
    }
    expect::delay(&runs)
}

/// Judge a run at its declared horizon and, if it missed there, at the
/// delay its effect shows: the delay its lever's runs have shown
/// (`learned_delay`), and, after a move the wrong way, any delay inside
/// the run's window. A move the wrong way is what a reading that has not
/// caught up with the run shows; a lever that did nothing reads as
/// nothing at any delay its runs have not shown. The first span that
/// meets the expectation judges the run; if none does, the miss at the
/// declared horizon stands. A delay only ever waits on or withholds a
/// miss: a run that met at its horizon is never judged later. None while
/// a span it waits on is not whole.
#[allow(clippy::too_many_arguments)]
fn judged(
    index: &Index,
    venture: &str,
    mandate: &Mandate,
    perm: &Permitted,
    expects: &Expects,
    intent: Seq,
    outcome: Seq,
) -> Option<Seen> {
    // A span of exact repeats is judged at the first reading that
    // differs, while a sensor that repeats as often as the metric has
    // would plausibly still hold it: a sensor that has not refreshed is
    // no evidence of no effect, and nor is a history too thin to say.
    let thin = Seen {
        judgement: Judgement::Unknown,
        doubted: false,
        thin: true,
        late: None,
    };
    let w = match index.horizon(
        venture,
        &perm.name,
        mandate.metric_name(),
        intent,
        outcome,
        expects.within,
        mandate.range,
    ) {
        Horizon::Waiting => return None,
        Horizon::Thin => return Some(thin),
        Horizon::At(w) => w,
    };
    let extended;
    let (mandate, perm, expects) = if w > expects.within as usize {
        let mut m = mandate.clone();
        for p in m.permitted.iter_mut().filter(|p| p.name == perm.name) {
            if let Some(e) = p.expects.as_mut() {
                e.within = w as u32;
            }
        }
        let p = m.permitted(&perm.name).cloned()?;
        let e = p.expects?;
        extended = (m, p, e);
        (&extended.0, &extended.1, &extended.2)
    } else {
        (mandate, perm, expects)
    };
    let at = |d| span(index, venture, mandate, perm, expects, intent, outcome, d);
    let first = at(0);
    if matches!(first, Span::Waiting) {
        return None;
    }
    // Too few steps the metric took on its own to learn its drift from:
    // the counterfactual would be no drift at all, which is no evidence
    // either way. Not judged yet.
    let quiet = world::drift_steps(index, venture, mandate, Some(intent))
        .iter()
        .filter(|d| d.gap > 0)
        .count();
    if quiet < world::MIN_SAMPLES {
        return Some(thin);
    }
    let (judgement, doubted) = match first {
        Span::Waiting => return None,
        Span::Unknown | Span::Past => (Judgement::Unknown, false),
        Span::Read {
            observed,
            drift,
            noise,
            doubted,
        } => (
            expect::judge_change(expects, observed, drift, noise),
            doubted,
        ),
    };
    let seen = |judgement| Seen {
        judgement,
        doubted,
        thin: false,
        late: None,
    };
    let wrong_way = match judgement {
        Judgement::NoEffect { .. } => false,
        Judgement::WrongDirection { .. } => true,
        _ => return Some(seen(judgement)),
    };
    if doubted {
        return Some(seen(judgement));
    }
    let learned = learned_delay(index, venture, mandate, perm, expects, intent);
    // Past runs of a lever that shows its effect `learned` reports late
    // moved the metric over that many more reports than `within`: those
    // steps are not drift for a span judged late (as `drift_steps` keeps
    // a run's horizon out of it). The declared span keeps the declared
    // horizon, so a learned delay never makes a run that met there miss.
    let longer = (learned > 0).then(|| {
        let mut m = mandate.clone();
        for p in m.permitted.iter_mut().filter(|p| p.name == perm.name) {
            if let Some(e) = p.expects.as_mut() {
                e.within += learned as u32;
            }
        }
        m
    });
    let late = |d| {
        let m = longer.as_ref().unwrap_or(mandate);
        span(index, venture, m, perm, expects, intent, outcome, d)
    };
    let shifts: Box<dyn Iterator<Item = usize>> = if wrong_way {
        Box::new(1..)
    } else {
        Box::new((learned > 0).then_some(learned).into_iter())
    };
    for d in shifts {
        match late(d) {
            Span::Waiting => return None,
            Span::Past | Span::Unknown => break,
            Span::Read { doubted: true, .. } => continue,
            Span::Read {
                observed,
                drift,
                noise,
                ..
            } => {
                let shown = expect::judge_change(expects, observed, drift, noise);
                if matches!(shown, Judgement::Met | Judgement::WrongSize { .. }) {
                    return Some(Seen {
                        judgement: shown,
                        doubted,
                        thin: false,
                        late: Some((d, learned)),
                    });
                }
            }
        }
    }
    Some(seen(judgement))
}
