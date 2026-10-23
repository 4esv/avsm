//! Refusals are records. The first refusal of a lever for a reason opens a
//! refusal record; repeats are counted here, in memory; a changed reason,
//! a new local day or a restart closes it with the count. A refusal is
//! measured, authorizes nothing and counts toward no cap or quota.
//!
//! Saturation is the algedonic channel: a threshold crossed while its lever
//! is refused for a cap, a budget or a quota writes a measured ask at once,
//! past the asks quota, once an episode per venture and lever. The episode
//! ends only when the lever goes one full cap window without being refused
//! for its limit; answering the ask does not end it.

use std::collections::BTreeMap;

use chrono::NaiveDate;

use super::{log, Kernel};
use crate::gate::Reason;
use crate::record::{Claim, Record, Seq, Signal};

/// What the asks quota refusing a lever is called in a reason key.
const ASKS_SPENT: &str = "AsksQuotaSpent";
/// What a question already open is called in a reason key.
const ALREADY_ASKED: &str = "AlreadyAsked";
/// What a lever found ineffective is called in a reason key: refused,
/// not asked about.
pub(super) const SUSPENDED: &str = "Suspended";
/// How a saturation question ends. Code never reads it; the signal field does.
const SATURATED: &str = "; the limits cannot keep up";
/// What a wait on an expectation is, when a limit is refusing too: not a
/// limit running out, so it may pass by itself.
const WAITING: &str = "; the run before it has not been judged yet, which holds it too, so this may pass without a change";
/// What answering a saturation ask does.
const APPROVING: &str = "; approving only records that you saw this and changes no limit: to give the lever room, raise the cap, budget or quota named above and run avsm add";

/// The question of a saturation ask.
fn saturation_question(
    metric: &str,
    bound: f64,
    lever: &str,
    detail: &str,
    waiting: bool,
    failed: Option<(usize, &str)>,
) -> String {
    match failed {
        Some((n, why)) => format!(
            "{metric} is past {bound} and {lever} is refused: {detail}; but its last {n} run{} in the window failed ({why}), so the cap is spent on failures: fix {lever}",
            if n == 1 { "" } else { "s" },
        ),
        None if waiting => {
            format!("{metric} is past {bound} and {lever} is refused: {detail}{WAITING}{APPROVING}")
        }
        None => {
            format!("{metric} is past {bound} and {lever} is refused: {detail}{SATURATED}{APPROVING}")
        }
    }
}

/// One refusal record not yet closed, and what is known of it.
struct Open {
    seq: Seq,
    key: String,
    count: u32,
    day: NaiveDate,
}

/// The refusals this process has seen, per venture and lever, and the
/// latest one.
#[derive(Default)]
pub(super) struct Tally {
    open: BTreeMap<(String, String), Open>,
    last: Option<Last>,
    /// Per venture and lever: when it was last refused for its limit with
    /// its threshold crossed, while a saturation episode is on.
    saturated: BTreeMap<(String, String), i64>,
}

/// The latest refusal: whether a limit, not a rule, said no.
struct Last {
    venture: String,
    lever: String,
    limit: bool,
    /// An expectation that is not judged yet holds the lever too.
    waiting: bool,
    detail: String,
}

/// A reason without its counters: the same refusal gives the same text.
fn reason_key(r: &Reason) -> String {
    match r {
        Reason::NoMandate { venture } => format!("NoMandate({venture})"),
        Reason::NotPermitted { action } => format!("NotPermitted({action})"),
        Reason::Forbidden { action } => format!("Forbidden({action})"),
        Reason::Hardwired { action, .. } => format!("Hardwired({action})"),
        Reason::Paused { venture } => format!("Paused({venture})"),
        Reason::OverCap { cap, .. } => format!("OverCap({cap})"),
        Reason::NeedsDecision { action, .. } => format!("NeedsDecision({action})"),
        Reason::NotJustified { action } => format!("NotJustified({action})"),
        Reason::OnlyInferred { .. } => "OnlyInferred".to_string(),
        Reason::UnknownRecord { seq } => format!("UnknownRecord({seq})"),
        Reason::NeedsDeclaredResume { venture } => format!("NeedsDeclaredResume({venture})"),
        Reason::NoPayload { action, takes } => format!("NoPayload({action},{takes})"),
        Reason::WrongPayload {
            action,
            takes,
            found,
        } => format!("WrongPayload({action},{takes},{found})"),
        Reason::OverBudget { cost, budget, .. } => format!("OverBudget({budget},{cost})"),
        Reason::OverQuota { what, cap, .. } => format!("OverQuota({what},{cap})"),
        Reason::Pending { intent } => format!("Pending({intent})"),
        Reason::Waiting { action, .. } => format!("Waiting({action})"),
        Reason::NotAChild { venture, child } => format!("NotAChild({venture},{child})"),
        Reason::BelowFloor { child, floor, .. } => format!("BelowFloor({child},{floor})"),
        Reason::OverTotal { child, total, .. } => format!("OverTotal({child},{total})"),
        Reason::Priority { child, spared, .. } => format!("Priority({child},{spared})"),
        Reason::InDeadband {
            child, deadband, ..
        } => format!("InDeadband({child},{deadband})"),
        Reason::Reversal { child, last, .. } => format!("Reversal({child},{last})"),
        Reason::Guarded { guard, .. } => format!("Guarded({guard})"),
    }
}

/// Is this a limit running out, not a rule saying no?
fn is_limit(r: &Reason) -> bool {
    matches!(
        r,
        Reason::OverCap { .. } | Reason::OverBudget { .. } | Reason::OverQuota { .. }
    )
}

/// The stable key of a set of reasons, with any extra keys, and the words.
pub(super) fn describe(reasons: &[Reason], extra: &[&str]) -> (String, String, bool) {
    let mut keys: Vec<String> = reasons.iter().map(reason_key).collect();
    keys.extend(extra.iter().map(|k| k.to_string()));
    keys.sort();
    keys.dedup();
    let words: Vec<String> = reasons.iter().map(|r| r.to_string()).collect();
    let limit = reasons.iter().any(is_limit) || extra.contains(&ASKS_SPENT);
    (keys.join("; "), words.join("; "), limit)
}

/// The key and words for a refusal at the gate that went no further: the
/// ask is already open, or the asks quota is spent.
pub(super) fn describe_gate(
    reasons: &[Reason],
    already: bool,
    quota_spent: bool,
) -> (String, String, bool) {
    let mut extra = Vec::new();
    let mut tail = String::new();
    if quota_spent {
        extra.push(ASKS_SPENT);
        tail = "; the asks quota is used up, so no ask was written".to_string();
    } else if already {
        extra.push(ALREADY_ASKED);
        tail = "; already asked".to_string();
    }
    let (key, words, limit) = describe(reasons, &extra);
    (key, format!("{words}{tail}"), limit)
}

fn today() -> NaiveDate {
    crate::clock::local().date_naive()
}

impl Kernel {
    /// Record that `lever` of `venture` was refused. The first refusal for
    /// this reason writes an opening record; a repeat only counts; a new
    /// reason closes the one before. Also remembers the refusal for
    /// `saturate`. True when it opened a refusal record: the refusal is
    /// new, or its reason changed. Only then is it worth a line on stderr;
    /// a repeat is a count, as in the ledger.
    pub(super) fn refuse(
        &mut self,
        venture: &str,
        lever: &str,
        key: String,
        detail: String,
        limit: bool,
    ) -> bool {
        self.refuse_citing(venture, lever, key, detail, limit, vec![])
    }

    /// `refuse`, answering the declared approvals in `cites`: a run on
    /// them was refused, by the quotas, the gate or the warden. Such a
    /// refusal spends them,
    /// so it is always written, never only counted: a repeat for the same
    /// reason closes the open one and opens its own.
    pub(super) fn refuse_citing(
        &mut self,
        venture: &str,
        lever: &str,
        key: String,
        detail: String,
        limit: bool,
        cites: Vec<Seq>,
    ) -> bool {
        self.refusals.last = Some(Last {
            venture: venture.to_string(),
            lever: lever.to_string(),
            limit,
            waiting: key.contains("Waiting("),
            detail: detail.clone(),
        });
        let id = (venture.to_string(), lever.to_string());
        let day = today();
        if let Some(open) = self.refusals.open.get_mut(&id) {
            if open.key == key && open.day == day && cites.is_empty() {
                open.count = open.count.saturating_add(1);
                return false;
            }
            let why = if open.key == key {
                "an approval was refused"
            } else {
                "reason changed"
            };
            self.close_refusal(&id, why);
        }
        let record = Record::Refusal {
            claim: Claim::Measured,
            venture: venture.to_string(),
            action: lever.to_string(),
            reason: key.clone(),
            detail,
            count: 1,
            closes: None,
            cites,
        };
        match self.append(record) {
            Ok(e) => {
                self.refusals.open.insert(
                    id,
                    Open {
                        seq: e.seq,
                        key,
                        count: 1,
                        day,
                    },
                );
                true
            }
            Err(e) => {
                log(&format!("{venture}: cannot write refusal: {e}"));
                true
            }
        }
    }

    /// Write the closing record of an open refusal and forget it.
    fn close_refusal(&mut self, id: &(String, String), why: &str) {
        let Some(open) = self.refusals.open.remove(id) else {
            return;
        };
        let record = Record::Refusal {
            claim: Claim::Measured,
            venture: id.0.clone(),
            action: id.1.clone(),
            reason: open.key,
            detail: format!("{why}; refused {} times", open.count),
            count: open.count,
            closes: Some(open.seq),
            cites: vec![],
        };
        if let Err(e) = self.append(record) {
            log(&format!("{}: cannot write refusal: {e}", id.0));
        }
    }

    /// Close every refusal opened on an earlier local day.
    pub(super) fn close_stale_refusals(&mut self) {
        let day = today();
        let stale: Vec<(String, String)> = self
            .refusals
            .open
            .iter()
            .filter(|(_, o)| o.day != day)
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            self.close_refusal(&id, "the day ended");
        }
    }

    /// On start: close every refusal the ledger shows open, with what the
    /// ledger knows, which is the opening record's count. The count in
    /// memory died with the last process.
    pub(super) fn recover_refusals(&mut self) {
        let open: Vec<Seq> = match self.index() {
            Ok(i) => i.open_refusals(),
            Err(_) => return,
        };
        for seq in open {
            let Some(Record::Refusal {
                venture,
                action,
                reason,
                count,
                ..
            }) = self
                .index()
                .ok()
                .and_then(|i| i.get(seq).map(|e| e.record.clone()))
            else {
                continue;
            };
            let record = Record::Refusal {
                claim: Claim::Measured,
                venture: venture.clone(),
                action,
                reason,
                detail: format!("avsmd restarted; refused at least {count} times"),
                count,
                closes: Some(seq),
                cites: vec![],
            };
            if let Err(e) = self.append(record) {
                log(&format!("{venture}: cannot write refusal: {e}"));
            }
        }
    }

    /// Forget the latest refusal; a threshold is about to be acted on.
    pub(super) fn forget_refusal(&mut self) {
        self.refusals.last = None;
    }

    /// When every run of `lever` since `since` failed: how many, and what
    /// the latest said. None when there were none or one succeeded.
    fn failed_runs(&self, venture: &str, lever: &str, since: i64) -> Option<(usize, String)> {
        let index = self.index().ok()?;
        let (mut failed, mut detail) = (0usize, String::new());
        for e in index.of(venture).filter(|e| e.at >= since) {
            let (ok, text) = match &e.record {
                Record::Action {
                    action, ok, detail, ..
                } if action == lever => (*ok, detail),
                Record::Outcome {
                    intent, ok, detail, ..
                } if matches!(
                    index.get(*intent).map(|i| &i.record),
                    Some(Record::Intent { action, .. }) if action == lever
                ) =>
                {
                    (*ok, detail)
                }
                _ => continue,
            };
            if ok {
                return None;
            }
            failed += 1;
            detail = text.lines().next().unwrap_or("").to_string();
        }
        (failed > 0).then_some((failed, detail))
    }

    /// A threshold on `metric` was crossed and `lever` was tried. When
    /// that was refused for a cap, a budget or a quota, write a measured
    /// ask now, past the asks quota, unless one is open for this lever.
    pub(super) fn saturate(&mut self, venture: &str, lever: &str, metric: &str, bound: f64) {
        let Some(last) = self.refusals.last.take() else {
            return;
        };
        if !last.limit || last.venture != venture || last.lever != lever {
            return;
        }
        let now = super::now();
        let window = self
            .ventures
            .get(venture)
            .and_then(|v| v.mandate.permitted(lever))
            .map_or(86_400, |p| p.cap.per.as_secs() as i64)
            .max(1);
        // A cap spent by runs that failed is not a limit that cannot keep
        // up: the lever is what is wrong.
        let failed = self.failed_runs(venture, lever, now - window);
        let question = saturation_question(
            metric,
            bound,
            lever,
            &last.detail,
            last.waiting,
            failed.as_ref().map(|(n, d)| (*n, d.as_str())),
        );
        let id = (venture.to_string(), lever.to_string());
        // When the episode was last seen: in memory, or after a restart
        // the latest saturation ask the ledger holds for this lever.
        let seen = match self.refusals.saturated.get(&id) {
            Some(t) => Some(*t),
            None => {
                let Ok(index) = self.index() else { return };
                let at = index
                    .of(venture)
                    .rev()
                    .find(|e| {
                        e.record.claim() == Claim::Measured
                            && matches!(&e.record, Record::Ask {
                                signal: Some(Signal::Saturated { action, .. }), ..
                            } if action == lever)
                    })
                    .map(|e| e.at);
                at
            }
        };
        self.refusals.saturated.insert(id, now);
        if seen.is_some_and(|t| now - t < window) {
            return;
        }
        // A new episode, and still one open ask at most.
        let open = {
            let Ok(index) = self.index() else { return };
            let found = index.open_signals().into_iter().any(|(e, s)| {
                e.record.venture() == venture
                    && e.record.claim() == Claim::Measured
                    && matches!(s, Signal::Saturated { action, .. } if action == lever)
            });
            found
        };
        if open {
            return;
        }
        let signal = Signal::Saturated {
            action: lever.to_string(),
            reason: last.detail,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wait_is_not_blamed_on_the_limits_and_approving_is_explained() {
        let said = |waiting: bool, failed: Option<(usize, &str)>| {
            saturation_question("count", 10.0, "reset", "cap spent", waiting, failed)
        };
        let limits = said(false, None);
        assert!(limits.contains("the limits cannot keep up"), "{limits}");
        assert!(limits.contains("approving only records"), "{limits}");
        let waited = said(true, None);
        assert!(!waited.contains("cannot keep up"), "{waited}");
        assert!(waited.contains("not been judged yet"), "{waited}");
        assert!(waited.contains("approving only records"), "{waited}");
        let failed = said(true, Some((2, "exit 1")));
        assert!(failed.contains("fix reset") && !failed.contains("cannot keep up"));
    }
}
