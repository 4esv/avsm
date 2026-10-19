//! Escalation: when the venture's backend cannot be used, a second one
//! named by `escalate:` in config.yaml is asked the same question.
//!
//! Recorded, then done. A measured `escalation` record (from, to, why) is
//! written before the call. The answer goes through the same parse path as
//! any reply and its inferred records cite the escalation. The escalation
//! then gets a measured answer record: the cited seqs, or `unusable`. An answer
//! is inferred like any other: it proposes or asks and never authorizes.
//!
//! A backend that leaves the machine is asked only for a venture whose
//! mandate says `escalate: offbox`, a field no proposal can add.

use super::{fit_payload, log, now, Kernel};
use crate::mandate::Escalate;
use crate::record::{Claim, Entry, Record, Seq};
use crate::role::{self, Context, InferRequest};

/// What an escalation's outcome says when the second backend failed too.
pub const UNANSWERED: &str = role::UNUSABLE;
/// The most characters the reason of an escalation may hold.
const WHY_MAX: usize = 300;

impl Kernel {
    /// Ask the escalation backend what the primary could not answer, for
    /// `venture`. `why` says what went wrong in avsm's own words; `request`
    /// is the prompt the primary was given. Returns the records written, in
    /// order: the escalation, the answer, the outcome. Nothing when config
    /// names no `escalate:` backend or it is the one that just failed; one
    /// refusal when it leaves the machine and the mandate does not allow it.
    pub fn escalate(
        &mut self,
        venture: &str,
        why: &str,
        request: &InferRequest,
    ) -> Result<Vec<Entry>, String> {
        let Some(to) = self.config.escalate.clone() else {
            return Ok(vec![]);
        };
        if to == request.backend {
            return Ok(vec![]);
        }
        let v = self
            .ventures
            .get(venture)
            .cloned()
            .ok_or_else(|| format!("no venture named {venture}"))?;
        let offbox = self.config.backend(Some(&to))?.1.offbox();
        let why: String = why
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(WHY_MAX)
            .collect();
        let report = |metric: &str, note: String, cites: Vec<Seq>, claim: Claim| Record::Report {
            claim,
            venture: venture.to_string(),
            metric: metric.to_string(),
            value: None,
            note,
            source: None,
            confidence: None,
            cites,
            implausible: None,
            facts: None,
        };
        let escalation = |refused: Option<String>| Record::Escalation {
            claim: Claim::Measured,
            venture: venture.to_string(),
            from: request.backend.clone(),
            to: to.clone(),
            why: why.clone(),
            refused,
        };
        if offbox && v.mandate.escalate != Some(Escalate::Offbox) {
            let refused = self.append(escalation(Some(format!(
                "{to} leaves the machine; the mandate does not say escalate: offbox"
            ))))?;
            log(&refused.line());
            return Ok(vec![refused]);
        }

        // Recorded, then done.
        let started = self.append(escalation(None))?;
        log(&started.line());
        let mut written = vec![started.clone()];
        let ctx = Context {
            venture: venture.to_string(),
            mandate: v.mandate.clone(),
            entries: vec![],
            state: String::new(),
            now: now(),
            backend: to.clone(),
            children: vec![],
            summaries: vec![],
            asks: vec![],
            to_summarize: vec![],
        };
        let asked = InferRequest {
            backend: to.clone(),
            ..request.clone()
        };
        let mut answers: Vec<Record> = Vec::new();
        match self.warden.infer(&asked) {
            Err(e) => log(&format!("{venture}: escalation to {to} failed: {e}")),
            Ok(text) => match role::read_reply(&ctx, &text) {
                Ok(records) => answers = records,
                Err(w) => answers.push(role::unusable_record(&ctx, &w, &text)),
            },
        }

        let mut cited: Vec<Seq> = Vec::new();
        let mut proposals: Vec<Entry> = Vec::new();
        let mut usable = false;
        for record in answers {
            let record = match record {
                Record::Proposal {
                    action,
                    reason,
                    payload,
                    ..
                } => Record::Proposal {
                    cites: vec![started.seq],
                    claim: Claim::Inferred,
                    venture: venture.to_string(),
                    payload: fit_payload(&v.mandate, &action, payload, venture),
                    action,
                    reason,
                },
                Record::Ask {
                    action,
                    question,
                    payload,
                    ..
                } => Record::Ask {
                    cites: vec![started.seq],
                    claim: Claim::Inferred,
                    venture: venture.to_string(),
                    payload: action
                        .as_deref()
                        .and_then(|a| fit_payload(&v.mandate, a, payload, venture)),
                    action,
                    question,
                    refusal: None,
                    signal: None,
                },
                Record::Report { metric, note, .. } if metric == role::NOTHING => {
                    let nothing = role::nothing_record(venture, Some(&note));
                    cite(nothing, started.seq)
                }
                Record::Report { metric, note, .. } if metric == role::UNUSABLE => report(
                    role::UNUSABLE,
                    note.chars().take(WHY_MAX).collect(),
                    vec![started.seq],
                    Claim::Inferred,
                ),
                // A reply is read into proposals, asks and nothing; any
                // other record is not an answer.
                _ => continue,
            };
            let counts =
                !matches!(&record, Record::Report { metric, .. } if metric == role::UNUSABLE);
            let entry = self.append(record)?;
            log(&entry.line());
            if counts {
                usable = true;
                cited.push(entry.seq);
            }
            if matches!(entry.record, Record::Proposal { .. }) {
                proposals.push(entry.clone());
            }
            written.push(entry);
        }

        let detail = if usable {
            let list: Vec<String> = cited.iter().map(|s| format!("#{s}")).collect();
            format!("answer: {}", list.join(", "))
        } else {
            UNANSWERED.to_string()
        };
        let done = self.append(Record::Answer {
            claim: Claim::Measured,
            venture: venture.to_string(),
            escalation: started.seq,
            ok: usable,
            cites: cited,
            detail,
        })?;
        log(&done.line());
        written.push(done);

        // An answer proposes; the gate judges it as it would any inferred
        // proposal, and an inferred record alone never passes.
        for p in proposals {
            if let Some(action) = p.record.action() {
                let action = action.to_string();
                self.act(venture, &action, vec![p.seq], Claim::Inferred);
            }
        }
        Ok(written)
    }
}

/// A report of a turn that chose nothing, citing the escalation.
fn cite(record: Record, seq: Seq) -> Record {
    match record {
        Record::Report {
            claim,
            venture,
            metric,
            value,
            note,
            source,
            confidence,
            implausible,
            ..
        } => Record::Report {
            claim,
            venture,
            metric,
            value,
            note,
            source,
            confidence,
            cites: vec![seq],
            implausible,
            facts: None,
        },
        other => other,
    }
}
