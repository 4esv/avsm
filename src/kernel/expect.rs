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
//! then too. Tightening is measured; loosening is declared.

use crate::expect::{self, Judgement};
use crate::index::{Index, EXPECTATION, SUSPENSION};
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
    /// A reading the step spans is implausible: neither a hit nor a miss.
    Unjudged,
}

/// New measured evidence about a suspended lever since its last
/// ineffective ask: a plausible reading of the metric after the ask that
/// is further from the frozen value (the last plausible reading at the
/// ask) than the metric's noise then. Returns the two values, or None.
/// With no ask, anything is new.
pub(super) fn evidence(
    index: &Index,
    venture: &str,
    metric: &str,
    since: Option<Seq>,
) -> Option<(f64, f64)> {
    let since = since?;
    let series = index.series(venture, metric);
    let i = series.partition_point(|(s, _)| *s <= since);
    let frozen = series[..i].last()?.1;
    let values: Vec<f64> = series[..i].iter().map(|(_, x)| *x).collect();
    let noise = expect::noise(&values);
    series[i..]
        .iter()
        .find(|(_, x)| (x - frozen).abs() > noise)
        .map(|(_, x)| (frozen, *x))
}

impl Kernel {
    /// Judge the runs of every lever of a venture that has expectations
    /// or a `revoke_after`, and write what follows.
    pub(super) fn judge(&mut self, venture: &str) {
        let Some(v) = self.ventures.get(venture).cloned() else {
            return;
        };
        let metric = v.mandate.metric_name().to_string();
        for perm in &v.mandate.permitted {
            if perm.expects.is_none() && perm.revoke_after.is_none() {
                continue;
            }
            let mut memo: Vec<(Seq, Run)> = Vec::new();
            let mut records: Vec<Record> = Vec::new();
            let mut ask: Option<(String, Option<Seq>)> = None;
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
                                    Some((*x, implausible.as_deref()))
                                }
                                _ => None,
                            })
                            .take_while(|(_, flag)| flag.is_some())
                            .find(|(x, flag)| {
                                *x == frozen && flag.is_some_and(|f| f.starts_with("stuck"))
                            })
                            .map(|(x, _)| x)
                    });
                    let result = if !ok {
                        Run::Failed
                    } else if let Some(x) = stuck {
                        Run::Ineffective(format!(
                            "no effect seen: {metric} stuck at {x} after {} ran; {} expects {}",
                            perm.name,
                            perm.name,
                            perm.expects
                                .as_ref()
                                .map_or_else(String::new, |e| e.to_string()),
                        ))
                    } else if let Some(expects) = &perm.expects {
                        let series = index.series(venture, &metric);
                        let b = series.partition_point(|(s, _)| *s < intent);
                        let a = series.partition_point(|(s, _)| *s <= outcome);
                        let before: Vec<f64> = series[..b].iter().map(|(_, x)| *x).collect();
                        let after: Vec<f64> = series[a..].iter().map(|(_, x)| *x).collect();
                        // The counterfactual: what the metric would have
                        // moved on its own from the last report before the
                        // run to the one judged, by the recent drift before
                        // the run.
                        let at = |s: Seq| index.get(s).map_or(0, |e| e.at);
                        let ends = (
                            b.checked_sub(1).map(|i| series[i].0),
                            series.get(a + expects.within as usize - 1).map(|p| p.0),
                        );
                        let span = match ends {
                            (Some(s0), Some(s1)) => Some((at(s0), at(s1) - at(s0))),
                            _ => None,
                        };
                        // A step that spans an implausible reading says
                        // nothing about the run: the sensor, not the lever.
                        let doubted = match ends {
                            (Some(s0), Some(s1)) => index.doubted_between(venture, &metric, s0, s1),
                            _ => false,
                        };
                        let (drift, noise) = span.map_or((0.0, 0.0), |(t0, dt)| {
                            world::counterfactual(&index, venture, &v.mandate, intent, t0, dt)
                        });
                        match expect::judge(expects, &before, &after, drift, noise) {
                            Judgement::Waiting => break,
                            _ if doubted => Run::Unjudged,
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
                        Run::Ineffective(_) => misses.push(r),
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
                        Run::Ineffective(why) if misses.len() == 1 => why.clone(),
                        Run::Ineffective(why) => {
                            format!("{} misses in a row; the last: {why}", misses.len())
                        }
                        _ => format!("{} runs in a row failed", misses.len()),
                    };
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
                        Some(q) => match evidence(&index, venture, &metric, Some(q)) {
                            Some((was, now)) => (
                                format!("{why}; {metric} read {was} at ask #{q} and {now} since"),
                                None,
                            ),
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
            match ask {
                Some((why, None)) => self.ask_ineffective(venture, &perm.name, why),
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
                        cites: vec![q],
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

    /// Write the measured ask that suspends `lever`: one an episode.
    pub(super) fn ask_ineffective(&mut self, venture: &str, lever: &str, why: String) {
        let question = format!(
            "{lever} is ineffective, so it is refused and asks no more: {why}; approve to delegate it again"
        );
        let signal = Signal::Ineffective {
            action: lever.to_string(),
            why,
        };
        let packet = self.signal_packet(venture, &signal);
        match self.append(Record::Ask {
            cites: vec![],
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
            let last = index.last_ineffective(venture, lever);
            if last.is_some_and(|q| index.is_open(q)) {
                return;
            }
            evidence(&index, venture, &metric, last).map(|(was, now)| {
                format!(
                    "found ineffective before, at ask #{}; {metric} read {was} then and {now} since",
                    last.unwrap_or(0)
                )
            })
        };
        if let Some(why) = found {
            self.ask_ineffective(venture, lever, why);
        }
    }
}
