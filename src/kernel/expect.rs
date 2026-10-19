//! Expectations: what a delegated lever's runs did to the metric. A run
//! the mandate expects something of is judged once enough reports follow
//! it. The right way by the wrong size is a measured report, the input to
//! the baseline. No effect or the wrong way is a miss, and so is a failed
//! run when the lever says `revoke_after`. Enough misses in a row write a
//! measured ask and the lever is suspended, reversible or not: each run
//! needs a declared decision, until a declared approval of that ask or a
//! declared adoption of the mandate after it. Tightening is measured;
//! loosening is declared.

use crate::expect::{self, Judgement};
use crate::index::EXPECTATION;
use crate::record::{Claim, Record, Seq, Signal};

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
            let mut ask: Option<String> = None;
            {
                let Ok(index) = self.index() else { return };
                let floor = index.last_ineffective(venture, &perm.name).unwrap_or(0);
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
                    let result = if !ok {
                        Run::Failed
                    } else if let Some(expects) = &perm.expects {
                        let series = index.series(venture, &metric);
                        let b = series.partition_point(|(s, _)| *s < intent);
                        let a = series.partition_point(|(s, _)| *s <= outcome);
                        let before: Vec<f64> = series[..b].iter().map(|(_, x)| *x).collect();
                        let after: Vec<f64> = series[a..].iter().map(|(_, x)| *x).collect();
                        match expect::judge(expects, &before, &after) {
                            Judgement::Waiting => break,
                            Judgement::Unknown | Judgement::Met => Run::Hit,
                            Judgement::WrongSize { observed, noise } => {
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
                                    });
                                }
                                Run::Hit
                            }
                            Judgement::NoEffect { observed, noise } => Run::Ineffective(format!(
                                "no effect: {} expects {expects}; {metric} moved {observed:+} against noise {noise}",
                                perm.name
                            )),
                            Judgement::WrongDirection { observed, noise } => {
                                Run::Ineffective(format!(
                                    "wrong direction: {} expects {expects}; {metric} moved {observed:+} against noise {noise}",
                                    perm.name
                                ))
                            }
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
                    ask = Some(why);
                }
            }
            self.judged.extend(memo);
            for record in records {
                if let Err(e) = self.append(record) {
                    log(&format!("{venture}: cannot write expectation: {e}"));
                }
            }
            if let Some(why) = ask {
                let lever = perm.name.clone();
                let question = format!(
                    "{lever} is ineffective, so it asks each time again: {why}; approve to delegate it again"
                );
                match self.append(Record::Ask {
                    claim: Claim::Measured,
                    venture: venture.to_string(),
                    action: None,
                    question,
                    refusal: None,
                    payload: None,
                    signal: Some(Signal::Ineffective { action: lever, why }),
                }) {
                    Ok(e) => log(&e.line()),
                    Err(e) => log(&format!("{venture}: cannot write ask: {e}")),
                }
            }
        }
    }
}
