//! What the kernel and the warden keep between ticks: every entry of the
//! ledger in order, and the facts the gate needs derived from them. It is
//! brought up to date by adding only the records appended since it last
//! looked, by this connection or another, so reading it costs nothing like
//! reading the whole ledger again.
//!
//! The index decides nothing. The gate still proves from the rules; the
//! index only says which records a proof can rest on, so the gate loads
//! those and not the rest.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use crate::ledger::{Ledger, LedgerError};
use crate::mandate::ParentCall;
use crate::record::{Claim, Entry, Kind, Payload, Record, Seq, Signal, Verdict};

/// The metric of the reports that say what a run did to the venture's metric.
pub const EXPECTATION: &str = "expectation";

/// The metric of the measured report that suspends a restored lever again
/// when it misses and nothing new was measured to ask on. It cites the
/// ineffective ask it follows.
pub const SUSPENSION: &str = "suspension";

/// The prefix of the metric of the reports that score a prediction:
/// `calibration.<model>`.
pub const CALIBRATION: &str = "calibration.";

/// The metric of the measured report that marks a canary ask answered.
pub const CANARY: &str = "canary";

/// Who approved when the decision came in over the socket, which knows
/// nobody.
pub const LOCAL: &str = "local";

/// The latest parent levers one venture pulled on another.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pulled {
    pub pause: Option<Seq>,
    pub resume: Option<Seq>,
    /// seq, above, bound
    pub threshold: Option<(Seq, bool, f64)>,
    /// seq, amount
    pub budget: Option<(Seq, u32)>,
    /// Every budget set: seq, when, amount; oldest first.
    pub budgets: Vec<(Seq, i64, u32)>,
}

/// A venture's latest declared adoption: the record, the hash of the
/// mandate, its directory and, from 2.0, the text as adopted.
#[derive(Clone, Debug, PartialEq)]
pub struct Adoption {
    pub seq: Seq,
    pub hash: String,
    pub path: String,
    pub text: Option<String>,
}

/// The ledger as read so far, and what follows from it.
#[derive(Clone, Default)]
pub struct Index {
    /// Shared, so a copy of the index (a forecast branch's) does not copy
    /// every record.
    entries: Vec<Arc<Entry>>,
    /// Set when an entry's number is not one more than the last one's, as
    /// in a hand-made ledger; lookups by number then search.
    scattered: bool,
    /// The latest trusted report with a value, per venture and metric:
    /// every earlier report of that metric is stale.
    reports: HashMap<String, HashMap<String, Seq>>,
    /// The latest trusted pause and resume decisions, and stop actions,
    /// per venture.
    paused: HashMap<String, Seq>,
    resumed: HashMap<String, Seq>,
    stopped: HashMap<String, Seq>,
    /// Parent levers pulled: child, then the venture that pulled them.
    pulled: HashMap<String, BTreeMap<String, Pulled>>,
    /// Asks a trusted approval or denial answered.
    decided: HashSet<Seq>,
    open: BTreeSet<Seq>,
    open_of: HashMap<String, BTreeSet<Seq>>,
    /// When each ask was written, oldest first.
    asks_at: Vec<i64>,
    /// When each ask the asks quota counts was written, oldest first: all
    /// but the measured asks carrying a signal.
    quota_asks_at: Vec<i64>,
    /// Actions and intents: (at, venture, action), oldest first; and the
    /// same per venture, and per venture and action.
    runs: Vec<(i64, String, String)>,
    runs_of: HashMap<String, Vec<(i64, String)>>,
    runs_by: HashMap<String, HashMap<String, Vec<i64>>>,
    /// Decisions a proof cites, and declared approvals no proof cites yet.
    acted: HashSet<Seq>,
    unacted: BTreeSet<Seq>,
    /// Declared approvals and denials, per venture.
    answers: HashMap<String, Vec<Seq>>,
    /// Every record, per venture.
    of: HashMap<String, Vec<Seq>>,
    /// When each inferred record was written, per venture, oldest first.
    inferred_at: HashMap<String, Vec<i64>>,
    /// When each of self's digests was written, oldest first.
    digests_at: Vec<i64>,
    /// Refusals written and not yet closed.
    refusals_open: BTreeSet<Seq>,
    /// The latest declared adoption per venture.
    adoptions: BTreeMap<String, Adoption>,
    /// Every declared adoption per venture, oldest first: its mandate versions.
    versions: HashMap<String, Vec<Seq>>,
    /// Intents with a trusted ok outcome: lever runs that are done.
    done: HashSet<Seq>,
    /// Intents with no trusted outcome yet: venture, action.
    pending: BTreeMap<Seq, (String, String)>,
    /// Every trusted, plausible report with a value, per venture and
    /// metric, by number.
    series: HashMap<String, HashMap<String, Vec<(Seq, f64)>>>,
    /// The same series as stretches of exactly equal readings: value and
    /// how many, oldest first.
    stretches: HashMap<String, HashMap<String, Vec<(f64, usize)>>>,
    /// Reports of what a run did to the metric, by the intent they cite.
    expected: HashSet<Seq>,
    /// Every intent per venture and action, by number.
    intents_of: HashMap<(String, String), Vec<Seq>>,
    /// The trusted outcome of an intent: its number, and whether it was ok.
    /// An ok outcome wins over a failed one.
    settled: HashMap<Seq, (Seq, bool)>,
    /// Measured asks that say a lever is ineffective, per venture and lever.
    ineffective: HashMap<(String, String), Vec<Seq>>,
    /// Measured reports that suspend a restored lever again without an
    /// ask, per venture and lever.
    relapses: HashMap<(String, String), Vec<Seq>>,
    /// Every trusted report code marked implausible, per venture and
    /// metric, by number.
    doubted: HashMap<String, HashMap<String, Vec<Seq>>>,
    /// The ventures whose declared approval answered an ask, each with
    /// its first such approval.
    approved_by: HashMap<Seq, HashMap<String, Seq>>,
    /// The highest trusted, plausible value of each venture's metrics: the
    /// peak a drawdown falls from.
    peaks: HashMap<String, HashMap<String, f64>>,
    /// Inferred predictions no measured score cites yet, per venture.
    predictions: HashMap<String, BTreeSet<Seq>>,
    /// The latest trusted report of each venture's metric, value or not,
    /// and whether code marked it implausible.
    flagged: HashMap<(String, String), (Seq, bool)>,
    /// Canary asks answered, per approver: (approved, answered). Counted
    /// from the measured canary report that cites the decision.
    canaries: BTreeMap<String, (u32, u32)>,
    /// Decisions already counted.
    canaried: HashSet<Seq>,
    /// The baseline's drift steps worked out so far (`world::drift_steps`).
    drift: crate::world::DriftMemo,
}

impl Index {
    /// An index of a ledger read some other way.
    pub fn from_entries(entries: &[Entry]) -> Index {
        let mut index = Index::default();
        for e in entries {
            index.apply(e.clone());
        }
        index
    }

    /// A copy of this index to start a reader of `ledger` from, when it
    /// is an index of a prefix of that ledger: its last entry is the
    /// ledger's entry of that number, hash and all. Else an empty index,
    /// which reads the ledger whole.
    pub fn seed_for(&self, ledger: &Ledger) -> Result<Index, LedgerError> {
        let Some(last) = self.entries.last() else {
            return Ok(Index::default());
        };
        Ok(match ledger.get(last.seq)? {
            Some(e) if e.hash == last.hash && !self.scattered => self.clone(),
            _ => Index::default(),
        })
    }

    /// Add what was appended to the ledger since the index last looked.
    pub fn refresh(&mut self, ledger: &Ledger) -> Result<(), LedgerError> {
        for e in ledger.after(self.last())? {
            self.apply(e);
        }
        Ok(())
    }

    /// The number of the last entry read, 0 when none.
    pub fn last(&self) -> Seq {
        self.entries.last().map_or(0, |e| e.seq)
    }

    /// Every entry, oldest first.
    pub fn entries(
        &self,
    ) -> impl DoubleEndedIterator<Item = &Entry> + ExactSizeIterator + Clone + '_ {
        self.entries.iter().map(|e| &**e)
    }

    /// One entry by number.
    pub fn get(&self, seq: Seq) -> Option<&Entry> {
        if self.scattered {
            return self.entries().find(|e| e.seq == seq);
        }
        let i = usize::try_from(seq.checked_sub(1)?).ok()?;
        self.entries.get(i).map(|e| &**e)
    }

    /// Take one more entry in.
    pub fn apply(&mut self, e: Entry) {
        if e.seq != self.entries.len() as Seq + 1 {
            self.scattered = true;
        }
        let seq = e.seq;
        let at = e.at;
        let r = &e.record;
        let venture = r.venture().to_string();
        self.of.entry(venture.clone()).or_default().push(seq);
        // A forecast is not the nightly cycle: it does not say the cycle ran.
        if r.claim() == Claim::Inferred && !matches!(r.kind(), Kind::Prediction | Kind::Forecast) {
            by_time(
                self.inferred_at.entry(venture.clone()).or_default(),
                at,
                |t| *t,
            );
        }
        if matches!(r.kind(), Kind::Action | Kind::Intent) {
            if let Some(action) = r.action() {
                by_time(
                    &mut self.runs,
                    (at, venture.clone(), action.to_string()),
                    |r| r.0,
                );
                by_time(
                    self.runs_of.entry(venture.clone()).or_default(),
                    (at, action.to_string()),
                    |r| r.0,
                );
                by_time(
                    self.runs_by
                        .entry(venture.clone())
                        .or_default()
                        .entry(action.to_string())
                        .or_default(),
                    at,
                    |t| *t,
                );
            }
        }
        match r {
            Record::Report {
                claim,
                metric,
                value,
                cites,
                implausible,
                ..
            } => {
                // An implausible report is never a premise: not for what a
                // run did to the metric, not for a peak.
                if let (Some(x), true, None) = (value, claim.authorizes(), implausible) {
                    let list = self
                        .series
                        .entry(venture.clone())
                        .or_default()
                        .entry(metric.clone())
                        .or_default();
                    let i = list.partition_point(|(s, _)| *s <= seq);
                    list.insert(i, (seq, *x));
                    let st = self
                        .stretches
                        .entry(venture.clone())
                        .or_default()
                        .entry(metric.clone())
                        .or_default();
                    if i + 1 == list.len() {
                        stretch(st, *x);
                    } else {
                        // Out of order: rare, so built again.
                        st.clear();
                        for (_, v) in list.iter() {
                            stretch(st, *v);
                        }
                    }
                    if x.is_finite() {
                        let peak = self
                            .peaks
                            .entry(venture.clone())
                            .or_default()
                            .entry(metric.clone())
                            .or_insert(*x);
                        *peak = peak.max(*x);
                    }
                }
                if let (true, Some(_)) = (claim.authorizes(), implausible) {
                    let list = self
                        .doubted
                        .entry(venture.clone())
                        .or_default()
                        .entry(metric.clone())
                        .or_default();
                    let i = list.partition_point(|s| *s <= seq);
                    list.insert(i, seq);
                }
                if metric == SUSPENSION && *claim == Claim::Measured {
                    for c in cites {
                        let lever = self.ineffective.iter().find_map(|((v, a), asks)| {
                            (*v == venture && asks.contains(c)).then(|| a.clone())
                        });
                        if let Some(a) = lever {
                            self.relapses
                                .entry((venture.clone(), a))
                                .or_default()
                                .push(seq);
                        }
                    }
                }
                if claim.authorizes() {
                    let slot = self
                        .flagged
                        .entry((venture.clone(), metric.clone()))
                        .or_insert((seq, implausible.is_some()));
                    if seq >= slot.0 {
                        *slot = (seq, implausible.is_some());
                    }
                }
                if metric == EXPECTATION && *claim == Claim::Measured {
                    self.expected.extend(cites.iter().copied());
                }
                if metric == CANARY && *claim == Claim::Measured {
                    for c in cites {
                        self.score_canary(*c);
                    }
                }
                if metric.starts_with(CALIBRATION) && *claim == Claim::Measured {
                    if let Some(open) = self.predictions.get_mut(&venture) {
                        for c in cites {
                            open.remove(c);
                        }
                    }
                }
                if value.is_some() && claim.authorizes() {
                    latest(
                        self.reports
                            .entry(venture.clone())
                            .or_default()
                            .entry(metric.clone())
                            .or_insert(seq),
                        seq,
                    );
                }
                if venture == crate::gate::SELF && metric == "digest" {
                    by_time(&mut self.digests_at, at, |t| *t);
                }
            }
            Record::Decision { claim, verdict, .. } => {
                let trusted = claim.authorizes();
                match verdict {
                    Verdict::Pause if trusted => {
                        latest(self.paused.entry(venture).or_insert(seq), seq)
                    }
                    Verdict::Resume if trusted => {
                        latest(self.resumed.entry(venture).or_insert(seq), seq)
                    }
                    Verdict::Approve { ask, .. } | Verdict::Deny { ask, .. } => {
                        if trusted {
                            self.decide(*ask);
                        }
                        if *claim == Claim::Declared {
                            if let Verdict::Approve { ask, .. } = verdict {
                                self.approved_by
                                    .entry(*ask)
                                    .or_default()
                                    .entry(venture.clone())
                                    .or_insert(seq);
                            }
                            self.answers.entry(venture).or_default().push(seq);
                            if matches!(verdict, Verdict::Approve { .. })
                                && !self.acted.contains(&seq)
                            {
                                self.unacted.insert(seq);
                            }
                        }
                    }
                    Verdict::Adopt {
                        mandate,
                        path,
                        text,
                    } if *claim == Claim::Declared => {
                        self.versions.entry(venture.clone()).or_default().push(seq);
                        self.adoptions.insert(
                            venture,
                            Adoption {
                                seq,
                                hash: mandate.clone(),
                                path: path.clone(),
                                text: text.clone(),
                            },
                        );
                    }
                    _ => {}
                }
            }
            Record::Ask { claim, signal, .. } => {
                if let (Claim::Measured, Some(Signal::Ineffective { action, .. })) = (claim, signal)
                {
                    self.ineffective
                        .entry((venture.clone(), action.clone()))
                        .or_default()
                        .push(seq);
                }
                by_time(&mut self.asks_at, at, |t| *t);
                if counts_toward_quota(*claim, signal.as_ref()) {
                    by_time(&mut self.quota_asks_at, at, |t| *t);
                }
                if !self.decided.contains(&seq) {
                    self.open.insert(seq);
                    self.open_of.entry(venture).or_default().insert(seq);
                }
            }
            Record::Action {
                action,
                ok,
                payload,
                proof,
                ..
            } => {
                if action == crate::gate::STOP {
                    latest(self.stopped.entry(venture.clone()).or_insert(seq), seq);
                }
                if *ok {
                    self.pull(venture, seq, at, action, payload.as_ref());
                }
                let cited = cited_decisions(&proof.steps);
                self.cite(cited);
            }
            Record::Intent { proof, action, .. } => {
                self.intents_of
                    .entry((venture.clone(), action.clone()))
                    .or_default()
                    .push(seq);
                self.pending.insert(seq, (venture, action.clone()));
                let cited = cited_decisions(&proof.steps);
                self.cite(cited);
            }
            Record::Prediction {
                claim: Claim::Inferred,
                ..
            } => {
                self.predictions.entry(venture).or_default().insert(seq);
            }
            Record::Refusal { closes, .. } => match closes {
                None => {
                    self.refusals_open.insert(seq);
                }
                Some(open) => {
                    self.refusals_open.remove(open);
                }
            },
            Record::Outcome {
                claim, intent, ok, ..
            } => {
                // Only a trusted outcome of an earlier intent of the same
                // venture settles it.
                let of = match self.get(*intent).map(|i| &i.record) {
                    Some(Record::Intent {
                        venture: v,
                        action,
                        payload,
                        ..
                    }) if *v == venture && *intent < seq && claim.authorizes() => {
                        Some((action.clone(), payload.clone()))
                    }
                    _ => None,
                };
                if let Some((action, payload)) = of {
                    self.pending.remove(intent);
                    if *ok {
                        self.settled.insert(*intent, (seq, true));
                    } else {
                        self.settled.entry(*intent).or_insert((seq, false));
                    }
                    if *ok && self.done.insert(*intent) {
                        let started = self.get(*intent).map_or(at, |i| i.at);
                        self.pull(venture, *intent, started, &action, payload.as_ref());
                    }
                }
            }
            _ => {}
        }
        self.entries.push(Arc::new(e));
    }

    /// A declared decision on a canary ask, counted once for whoever gave
    /// it. Anything else is not a canary answer.
    fn score_canary(&mut self, decision: Seq) {
        let Some(Record::Decision {
            claim: Claim::Declared,
            verdict,
            by,
            ..
        }) = self.get(decision).map(|e| &e.record)
        else {
            return;
        };
        let (ask, approved) = match verdict {
            Verdict::Approve { ask, .. } => (*ask, true),
            Verdict::Deny { ask, .. } => (*ask, false),
            _ => return,
        };
        let who = by.clone().unwrap_or_else(|| LOCAL.to_string());
        let canary = self
            .get(ask)
            .is_some_and(|e| matches!(&e.record, Record::Ask { action: Some(a), .. } if a == crate::gate::CANARY));
        if !canary || !self.canaried.insert(decision) {
            return;
        }
        let slot = self.canaries.entry(who).or_default();
        slot.1 += 1;
        if approved {
            slot.0 += 1;
        }
    }

    /// The share of the canary asks this approver answered that they
    /// approved, 0 when they answered none.
    pub fn canary_rate(&self, by: &str) -> f64 {
        match self.canaries.get(by) {
            Some((approved, answered)) if *answered > 0 => {
                f64::from(*approved) / f64::from(*answered)
            }
            _ => 0.0,
        }
    }

    /// Canary asks written, per venture, and the number of real asks
    /// written since the last one. For the kernel's schedule.
    pub fn canary_due(&self, venture: &str, every: u32) -> bool {
        let mut since = 0;
        let mut open = false;
        for e in self.of(venture) {
            if let Record::Ask { action, signal, .. } = &e.record {
                if action.as_deref() == Some(crate::gate::CANARY) {
                    since = 0;
                    open = self.is_open(e.seq);
                } else if signal.is_none() {
                    since += 1;
                }
            }
        }
        !open && since >= every
    }

    /// How many canary asks a venture has been given.
    pub fn canaries_asked(&self, venture: &str) -> usize {
        self.of(venture)
            .filter(|e| matches!(&e.record, Record::Ask { action: Some(a), .. } if a == crate::gate::CANARY))
            .count()
    }

    /// A parent lever done: an ok action, or an intent with an ok outcome,
    /// known by its number.
    fn pull(
        &mut self,
        venture: String,
        seq: Seq,
        at: i64,
        action: &str,
        payload: Option<&Payload>,
    ) {
        let Some(Payload::Text { text }) = payload else {
            return;
        };
        let Ok(call) = ParentCall::parse(action, text) else {
            return;
        };
        let pulled = self
            .pulled
            .entry(call.child().to_string())
            .or_default()
            .entry(venture)
            .or_default();
        // The latest by number wins, whatever order outcomes come in.
        match call {
            ParentCall::Pause { .. } => latest(pulled.pause.get_or_insert(seq), seq),
            ParentCall::Resume { .. } => latest(pulled.resume.get_or_insert(seq), seq),
            ParentCall::Threshold { above, bound, .. } => {
                if pulled.threshold.is_none_or(|(s, _, _)| s < seq) {
                    pulled.threshold = Some((seq, above, bound))
                }
            }
            ParentCall::Budget { amount, .. } => {
                if let Err(i) = pulled.budgets.binary_search_by_key(&seq, |b| b.0) {
                    pulled.budgets.insert(i, (seq, at, amount));
                }
                if pulled.budget.is_none_or(|(s, _)| s < seq) {
                    pulled.budget = Some((seq, amount))
                }
            }
        }
    }

    /// Is this an intent with a trusted ok outcome: a lever run that is done?
    pub fn done(&self, intent: Seq) -> bool {
        self.done.contains(&intent)
    }

    /// Intents with no trusted outcome yet, oldest first, as (seq,
    /// venture, action): runs that may or may not have happened.
    pub fn pending(&self) -> impl Iterator<Item = (Seq, &str, &str)> + '_ {
        self.pending
            .iter()
            .map(|(s, (v, a))| (*s, v.as_str(), a.as_str()))
    }

    /// The oldest intent of this lever with no outcome yet.
    pub fn pending_of(&self, venture: &str, action: &str) -> Option<Seq> {
        self.pending()
            .find(|(_, v, a)| *v == venture && *a == action)
            .map(|(s, _, _)| s)
    }

    /// The trusted reports of a venture's metric with a value, oldest first.
    pub fn series(&self, venture: &str, metric: &str) -> &[(Seq, f64)] {
        self.series
            .get(venture)
            .and_then(|m| m.get(metric))
            .map_or(&[], Vec::as_slice)
    }

    /// The trusted reports of a venture's metric with a value, as
    /// stretches of exactly equal readings, oldest first.
    pub fn stretches(&self, venture: &str, metric: &str) -> &[(f64, usize)] {
        self.stretches
            .get(venture)
            .and_then(|m| m.get(metric))
            .map_or(&[], Vec::as_slice)
    }

    /// A venture's predictions no score has resolved yet, oldest first.
    pub fn open_predictions(&self, venture: &str) -> Vec<Seq> {
        self.predictions
            .get(venture)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default()
    }

    /// The baseline's drift steps worked out so far, for `world`.
    pub(crate) fn drift_memo(&self) -> &crate::world::DriftMemo {
        &self.drift
    }

    /// Every intent of a venture's lever, by number.
    pub fn intents_of(&self, venture: &str, action: &str) -> &[Seq] {
        self.intents_of
            .get(&(venture.to_string(), action.to_string()))
            .map_or(&[], Vec::as_slice)
    }

    /// The trusted outcome of an intent: its number and whether it was ok.
    pub fn settled(&self, intent: Seq) -> Option<(Seq, bool)> {
        self.settled.get(&intent).copied()
    }

    /// Has a report already said what this intent did to the metric?
    pub fn expectation_reported(&self, intent: Seq) -> bool {
        self.expected.contains(&intent)
    }

    /// The latest measured ask saying this lever is ineffective.
    pub fn last_ineffective(&self, venture: &str, action: &str) -> Option<Seq> {
        self.ineffective
            .get(&(venture.to_string(), action.to_string()))?
            .iter()
            .copied()
            .max()
    }

    /// The latest measured record that suspended this lever: its latest
    /// ineffective ask or its latest relapse. Runs are judged after it.
    pub fn last_suspension(&self, venture: &str, action: &str) -> Option<Seq> {
        let key = (venture.to_string(), action.to_string());
        let relapse = self.relapses.get(&key).and_then(|r| r.iter().max());
        self.last_ineffective(venture, action).max(relapse.copied())
    }

    /// Is this lever suspended: the latest measured ask since the last
    /// declared adoption of the venture's mandate that says it is
    /// ineffective has no declared approval by the same venture; or a
    /// measured relapse after the latest such ask says the restored lever
    /// missed again. The latest word governs: a denial keeps it
    /// suspended, and an approval of a later ask restores it. While it
    /// is, every run is refused. Tightening is measured; only a declared
    /// decision loosens, but for a miss seen only on stuck readings, which
    /// a plausible reading away from the frozen value voids (`voided`).
    pub fn suspended(&self, venture: &str, action: &str) -> bool {
        let (latest, relapsed) = self.governing(venture, action);
        relapsed || latest.is_some_and(|q| self.approval_of(venture, q).is_none())
    }

    /// The ask and the declared decision that restored this lever: it had
    /// a measured ineffective ask since the last adoption, and is not
    /// suspended because a declared approval by the venture answered the
    /// latest one. A run of it then is the human's word, not code's
    /// alone, and its proof says so.
    pub fn restored(&self, venture: &str, action: &str) -> Option<(Seq, Seq)> {
        match self.governing(venture, action) {
            (Some(q), false) => self.approval_of(venture, q).map(|d| (q, d)),
            _ => None,
        }
    }

    /// The first declared approval of an ask by this venture.
    fn approval_of(&self, venture: &str, ask: Seq) -> Option<Seq> {
        self.approved_by.get(&ask)?.get(venture).copied()
    }

    /// The latest non-void ineffective ask after the last declared
    /// adoption, whose answer governs, and whether a measured relapse
    /// after the latest ask suspended the lever again.
    fn governing(&self, venture: &str, action: &str) -> (Option<Seq>, bool) {
        // A declared adoption of the mandate after the ask answers it too.
        let adopted = self.adoptions.get(venture).map_or(0, |a| a.seq);
        let key = (venture.to_string(), action.to_string());
        let asks = self.ineffective.get(&key);
        let latest = asks.and_then(|asks| {
            asks.iter()
                .copied()
                .filter(|q| *q > adopted && !self.voided(venture, *q))
                .max()
        });
        let last_ask = asks.and_then(|a| a.iter().max()).copied().unwrap_or(0);
        let relapsed = self.relapses.get(&key).is_some_and(|r| {
            r.iter()
                .any(|r| *r > adopted && *r > last_ask && !self.voided(venture, *r))
        });
        (latest, relapsed)
    }

    /// Is this measured ineffective ask or relapse void? It is when the
    /// miss it says came only from readings flagged stuck, the ones it
    /// cites, and a plausible reading of their metric since is new
    /// evidence: the metric is not frozen. A miss measured on plausible
    /// readings cites none, and is never void.
    pub fn voided(&self, venture: &str, seq: Seq) -> bool {
        let Some(e) = self.get(seq) else {
            return false;
        };
        let metric = e
            .record
            .cites()
            .iter()
            .find_map(|c| match &self.get(*c)?.record {
                Record::Report {
                    metric,
                    claim,
                    implausible: Some(why),
                    ..
                } if claim.authorizes() && why.starts_with("stuck") => Some(metric.as_str()),
                _ => None,
            });
        metric.is_some_and(|m| self.evidence(venture, m, Some(seq)).is_some())
    }

    /// New measured evidence about a metric since a record: a plausible
    /// reading after it further from the frozen value (the last plausible
    /// reading before it) than the metric's noise then. Returns the two
    /// values, or None. With no record, anything is new.
    pub fn evidence(&self, venture: &str, metric: &str, since: Option<Seq>) -> Option<(f64, f64)> {
        let since = since?;
        let series = self.series(venture, metric);
        let i = series.partition_point(|(s, _)| *s <= since);
        let frozen = series[..i].last()?.1;
        let values: Vec<f64> = series[..i].iter().map(|(_, x)| *x).collect();
        let noise = crate::expect::noise(&values);
        series[i..]
            .iter()
            .find(|(_, x)| (x - frozen).abs() > noise)
            .map(|(_, x)| (frozen, *x))
    }

    /// Is any trusted report of this metric between these two numbers,
    /// both excluded, marked implausible?
    pub fn doubted_between(&self, venture: &str, metric: &str, from: Seq, to: Seq) -> bool {
        self.doubted
            .get(venture)
            .and_then(|m| m.get(metric))
            .is_some_and(|list| {
                let i = list.partition_point(|s| *s <= from);
                list.get(i).is_some_and(|s| *s < to)
            })
    }

    fn decide(&mut self, ask: Seq) {
        if self.decided.insert(ask) && self.open.remove(&ask) {
            if let Some(v) = self.get(ask).map(|e| e.record.venture().to_string()) {
                if let Some(open) = self.open_of.get_mut(&v) {
                    open.remove(&ask);
                }
            }
        }
    }

    fn cite(&mut self, decisions: Vec<Seq>) {
        for d in decisions {
            self.acted.insert(d);
            self.unacted.remove(&d);
        }
    }

    /// The highest trusted, plausible value a venture's metric has had.
    pub fn peak(&self, venture: &str, metric: &str) -> Option<f64> {
        self.peaks.get(venture)?.get(metric).copied()
    }

    /// The latest trusted report of a metric with a value.
    pub fn latest_report(&self, venture: &str, metric: &str) -> Option<Seq> {
        self.reports.get(venture)?.get(metric).copied()
    }

    /// Is the latest trusted report of any of a venture's metrics flagged
    /// implausible?
    pub fn implausible_now(&self, venture: &str) -> bool {
        self.flagged
            .iter()
            .any(|((v, _), (_, flag))| v == venture && *flag)
    }

    /// Does a venture have a suspended lever?
    pub fn any_suspended(&self, venture: &str) -> bool {
        self.ineffective
            .keys()
            .any(|(v, a)| v == venture && self.suspended(v, a))
    }

    /// The signals of a venture's open asks that code measured, oldest
    /// first. An ask that is not measured says nothing about the venture.
    pub fn signals_of(&self, venture: &str) -> Vec<&Signal> {
        self.open_of(venture)
            .filter_map(|s| self.get(s))
            .filter_map(|e| match &e.record {
                Record::Ask {
                    claim: Claim::Measured,
                    signal: Some(sig),
                    ..
                } => Some(sig),
                _ => None,
            })
            .collect()
    }

    /// The records that pause or resume a venture, the latest of each
    /// source: its declared pause and resume, its stop, and each venture
    /// that pulled pause or resume on it. Whether those count is the
    /// rules' to say.
    pub fn pause_sources(&self, venture: &str) -> Vec<Seq> {
        let mut seqs: Vec<Seq> = [&self.paused, &self.resumed, &self.stopped]
            .iter()
            .filter_map(|m| m.get(venture).copied())
            .collect();
        for p in self
            .pulled
            .get(venture)
            .into_iter()
            .flat_map(|m| m.values())
        {
            seqs.extend(p.pause);
            seqs.extend(p.resume);
        }
        seqs
    }

    /// The levers `parent` pulled on `child`, the latest of each.
    pub fn pulled(&self, child: &str, parent: &str) -> Option<&Pulled> {
        self.pulled.get(child)?.get(parent)
    }

    /// The budgets `parent` set for `child`: seq, when, amount; oldest first.
    pub fn budgets(&self, child: &str, parent: &str) -> &[(Seq, i64, u32)] {
        self.pulled(child, parent).map_or(&[], |p| &p.budgets)
    }

    /// Asks with no trusted decision yet, oldest first.
    pub fn open_asks(&self) -> Vec<Seq> {
        self.open.iter().copied().collect()
    }

    /// Open asks that carry a signal, oldest first, with the signal.
    pub fn open_signals(&self) -> Vec<(&Entry, &Signal)> {
        self.open
            .iter()
            .filter_map(|s| self.get(*s))
            .filter_map(|e| match &e.record {
                Record::Ask {
                    signal: Some(sig), ..
                } => Some((e, sig)),
                _ => None,
            })
            .collect()
    }

    /// A venture's open asks, oldest first.
    pub fn open_of(&self, venture: &str) -> impl Iterator<Item = Seq> + '_ {
        self.open_of
            .get(venture)
            .into_iter()
            .flat_map(|s| s.iter().copied())
    }

    /// Refusals no closing record names yet, oldest first.
    pub fn open_refusals(&self) -> Vec<Seq> {
        self.refusals_open.iter().copied().collect()
    }

    /// Is this ask open?
    pub fn is_open(&self, seq: Seq) -> bool {
        self.open.contains(&seq)
    }

    /// Asks written after `since`.
    pub fn asks_after(&self, since: i64) -> u32 {
        after(&self.asks_at, since, |t| *t).len() as u32
    }

    /// Asks the asks quota counts written after `since`: signal asks sit
    /// outside it.
    pub fn quota_asks_after(&self, since: i64) -> u32 {
        after(&self.quota_asks_at, since, |t| *t).len() as u32
    }

    /// Actions and intents after `since`, as (at, venture, action).
    pub fn runs_after(&self, since: i64) -> &[(i64, String, String)] {
        after(&self.runs, since, |r| r.0)
    }

    /// A venture's actions and intents after `since`, as (at, action).
    pub fn runs_of_after(&self, venture: &str, since: i64) -> &[(i64, String)] {
        self.runs_of
            .get(venture)
            .map_or(&[], |r| after(r, since, |r| r.0))
    }

    /// How many times a venture ran an action after `since`.
    pub fn used(&self, venture: &str, action: &str, since: i64) -> u32 {
        self.runs_by
            .get(venture)
            .and_then(|m| m.get(action))
            .map_or(0, |t| after(t, since, |t| *t).len() as u32)
    }

    /// Declared approvals no action or intent cites yet, oldest first.
    pub fn unacted(&self) -> impl Iterator<Item = Seq> + '_ {
        self.unacted.iter().copied()
    }

    /// A venture's declared approvals and denials, oldest first.
    pub fn answers(&self, venture: &str) -> &[Seq] {
        self.answers.get(venture).map_or(&[], Vec::as_slice)
    }

    /// A venture's records, oldest first.
    pub fn of(&self, venture: &str) -> impl DoubleEndedIterator<Item = &Entry> + '_ {
        self.of
            .get(venture)
            .into_iter()
            .flat_map(|s| s.iter())
            .filter_map(|s| self.get(*s))
    }

    /// When a venture's inferred records were written, oldest first.
    pub fn inferred_at(&self, venture: &str) -> &[i64] {
        self.inferred_at.get(venture).map_or(&[], Vec::as_slice)
    }

    /// When self's digests were written, oldest first.
    pub fn digests_at(&self) -> &[i64] {
        &self.digests_at
    }

    /// The latest declared adoption of each venture.
    pub fn adoptions(&self) -> &BTreeMap<String, Adoption> {
        &self.adoptions
    }

    /// Which mandate version a declared adoption is: how many adoptions of
    /// that venture came up to and including it, counting from 1.
    pub fn mandate_version(&self, venture: &str, adopted: Seq) -> Option<usize> {
        let seqs = self.versions.get(venture)?;
        seqs.binary_search(&adopted).ok().map(|i| i + 1)
    }
}

/// The decisions a proof cites.
fn cited_decisions(steps: &[crate::gate::Step]) -> Vec<Seq> {
    steps
        .iter()
        .filter_map(|s| match s {
            crate::gate::Step::Decision { seq, .. } => Some(*seq),
            _ => None,
        })
        .collect()
}

/// Does the asks quota count this ask? A measured ask carrying a signal
/// is the algedonic channel and sits outside it, so it can never starve
/// the plain asks; any other ask counts.
pub fn counts_toward_quota(claim: Claim, signal: Option<&Signal>) -> bool {
    !(claim == Claim::Measured && signal.is_some())
}

/// Keep the larger sequence number.
fn latest(slot: &mut Seq, seq: Seq) {
    *slot = (*slot).max(seq);
}

/// Insert keeping the list in time order; a record written in order goes
/// on the end.
fn by_time<T>(list: &mut Vec<T>, item: T, at: impl Fn(&T) -> i64) {
    let t = at(&item);
    let i = list.partition_point(|x| at(x) <= t);
    list.insert(i, item);
}

/// The part of a time-ordered list after `since`.
fn after<T>(list: &[T], since: i64, at: impl Fn(&T) -> i64) -> &[T] {
    &list[list.partition_point(|x| at(x) <= since)..]
}

/// Add a reading to stretches of exactly equal readings.
pub fn stretch(stretches: &mut Vec<(f64, usize)>, x: f64) {
    match stretches.last_mut() {
        Some((v, n)) if *v == x => *n += 1,
        _ => stretches.push((x, 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(value: f64) -> Record {
        Record::report(Claim::Measured, "garden", "level", Some(value), "")
    }

    fn ledger(values: &[f64]) -> Ledger {
        let mut l = Ledger::open_in_memory().unwrap();
        for v in values {
            l.append(report(*v)).unwrap();
        }
        l
    }

    #[test]
    fn a_seed_of_a_prefix_reads_on_to_the_whole_ledger() {
        let mut l = ledger(&[1.0, 2.0, 3.0]);
        let head = Index::from_entries(&l.all().unwrap());
        l.append(report(4.0)).unwrap();
        let mut seeded = head.seed_for(&l).unwrap();
        assert_eq!(seeded.last(), 3, "a seed of a prefix is kept");
        seeded.refresh(&l).unwrap();
        assert!(seeded
            .entries()
            .eq(Index::from_entries(&l.all().unwrap()).entries()));
        assert_eq!(seeded.series("garden", "level").len(), 4);
    }

    #[test]
    fn a_seed_of_another_ledger_is_dropped() {
        let other = Index::from_entries(&ledger(&[1.0, 2.0, 9.0]).all().unwrap());
        let l = ledger(&[1.0, 2.0, 3.0]);
        let seeded = other.seed_for(&l).unwrap();
        assert_eq!(seeded.last(), 0, "an index of another ledger is not a seed");
        let longer = Index::from_entries(&ledger(&[1.0, 2.0, 3.0, 4.0]).all().unwrap());
        assert_eq!(
            longer.seed_for(&l).unwrap().last(),
            0,
            "nor one past its head"
        );
    }
}
