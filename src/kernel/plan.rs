//! The planner as a controller. On each plausible report of a venture with
//! `controller: {kind: plan}`, code enumerates the candidate levers, the
//! pure search in `crate::plan` weighs every sequence through the world
//! model's ensemble, and the kernel writes what it found: an inferred
//! `branch` record, then an inferred proposal citing it, which goes through
//! `follow` and the gate as a model controller's would. A branch never
//! authorizes; below its calibration bar the proposal asks; an
//! irreversible, undelegated lever asks.
//!
//! When the best cannot be told apart from the runner-up, the backend
//! chooses among the top three; it also writes the payload of a lever that
//! takes one. A reply that cannot be used, or that chooses what was not
//! offered, escalates; the escalation's proposals go through `follow` too,
//! so below the calibration bar they ask.
//!
//! Critical slowing down is checked here too, for plan ventures and any
//! mandate with `early_warning: true`.

use super::{fit_payload, log, now, Kernel};
use crate::gate::{self, Candidate, Reason, Step};
use crate::mandate::{Controller, Mandate, Worse};
use crate::plan::{self, Lever, Plan, Problem, NOTHING};
use crate::record::{Claim, Payload, Record, Seq, Signal};
use crate::role::{self, Context, InferRequest};
use crate::slowing;
use crate::world;

/// The reasons a probe of the gate may give for a lever that stays a
/// candidate: it needs a premise, a decision or a payload, and nothing
/// refuses it outright.
fn weighable(r: &Reason) -> bool {
    matches!(
        r,
        Reason::NotJustified { .. }
            | Reason::OnlyInferred { .. }
            | Reason::NeedsDecision { .. }
            | Reason::NoPayload { .. }
            | Reason::WrongPayload { .. }
    )
}

/// What a planning turn settled on before it goes to the gate.
enum Settled {
    /// A lever, with the payload a backend wrote for it, and why.
    Lever(String, Option<Payload>, String),
    /// Nothing, and why.
    Nothing(String),
    /// The backend asked, or could not be used: done.
    Done,
}

impl Kernel {
    /// The levers a plan may weigh for a venture now: each permitted lever
    /// not forbidden, hardwired, past a quota or refused outright by the
    /// gate. One that needs a decision stays, marked as asking.
    fn plan_levers(&self, name: &str, mandate: &Mandate, explore: &[String]) -> Vec<Lever> {
        let Ok(index) = self.index() else {
            return vec![];
        };
        let adopted = self.adopted();
        let t = now();
        let mut out = Vec::new();
        for p in &mandate.permitted {
            if mandate.forbidden.contains(&p.name) {
                continue;
            }
            let (_, over) =
                gate::quota_check_in(&index, &adopted, &self.config.quotas, name, &p.name, t);
            if !over.is_empty() {
                continue;
            }
            let probe = Candidate {
                venture: name.to_string(),
                action: p.name.clone(),
                cites: vec![],
            };
            let (steps, reasons) =
                match gate::gate_in(&index, &probe, &adopted, &self.config.quotas, t) {
                    Ok(proof) => (proof.steps, vec![]),
                    Err(r) => (r.steps, r.reasons),
                };
            if !reasons.iter().all(weighable) {
                continue;
            }
            let left = steps
                .iter()
                .find_map(|s| match s {
                    Step::Cap { used, cap } => Some(cap.count.saturating_sub(*used)),
                    _ => None,
                })
                .unwrap_or(p.cap.count);
            out.push(Lever {
                name: p.name.clone(),
                reversible: p.reversible,
                explore: explore.contains(&p.name),
                asks: reasons
                    .iter()
                    .any(|r| matches!(r, Reason::NeedsDecision { .. })),
                left,
            });
        }
        out
    }

    /// One planning turn. Returns the lever that ran, if one did.
    pub(super) fn plan(&mut self, name: &str) -> Option<String> {
        let v = self.ventures.get(name).cloned()?;
        let Controller::Plan {
            horizon,
            depth,
            members,
            explore,
            backend,
            better,
        } = v.mandate.controller.clone()
        else {
            return None;
        };
        let levers = self.plan_levers(name, &v.mandate, &explore);
        let t = now();
        let base = world::mix(world::seed_of(name), t as u64);
        let found = {
            let index = self.index().ok()?;
            let model = world::model_for(&index, name, &v.mandate, v.path.as_deref());
            drop(index);
            let ms = plan::members(model.as_ref(), base, members);
            plan::search(
                &ms,
                &Problem {
                    state: serde_json::Value::Null,
                    now: t,
                    dt: (v.mandate.cadence.as_secs() as i64).max(1),
                    horizon,
                    depth,
                    better,
                    guards: &v.mandate.guards,
                    levers: levers.clone(),
                    seed: world::mix(base, 1),
                },
            )
        };
        let found = match found {
            Ok(p) => p,
            Err(e) => {
                log(&format!("{name}: no plan: {e}"));
                return None;
            }
        };
        let branch = match self.append(branch_record(name, &found, members, horizon, better)) {
            Ok(e) => e,
            Err(e) => {
                log(&format!("{name}: cannot write branch: {e}"));
                return None;
            }
        };
        log(&branch.line());
        let chosen = found.choice().action().to_string();
        let takes = v
            .mandate
            .permitted(&chosen)
            .and_then(|p| p.takes())
            .is_some();
        let settled = if !found.separated && !found.explored {
            let offered: Vec<String> = found.top().iter().map(|s| s.to_string()).collect();
            self.plan_backend(
                name,
                &v.mandate,
                backend.as_deref(),
                &found,
                &offered,
                branch.seq,
                better,
            )
        } else if takes {
            self.plan_backend(
                name,
                &v.mandate,
                backend.as_deref(),
                &found,
                std::slice::from_ref(&chosen),
                branch.seq,
                better,
            )
        } else if chosen == NOTHING {
            Settled::Nothing(format!(
                "the planner ranks doing nothing first over {horizon} steps"
            ))
        } else {
            let c = found.choice();
            let why = if found.explored {
                format!(
                    "the planner explores it, with propensity {}",
                    found.propensities[found.chosen]
                )
            } else {
                format!(
                    "the planner ranks it first: worst member {} and p50 {} in {horizon} steps",
                    c.worst_target(better),
                    c.quantiles().p50
                )
            };
            Settled::Lever(chosen, None, why)
        };
        match settled {
            Settled::Done => None,
            Settled::Nothing(why) => {
                let record = with_cites(role::nothing_record(name, Some(&why)), branch.seq);
                match self.append(record) {
                    Ok(e) => log(&e.line()),
                    Err(e) => log(&format!("{name}: cannot write record: {e}")),
                }
                None
            }
            Settled::Lever(action, payload, reason) => {
                let proposal = match self.append(Record::Proposal {
                    claim: Claim::Inferred,
                    venture: name.to_string(),
                    action: action.clone(),
                    reason,
                    payload,
                    cites: vec![branch.seq],
                }) {
                    Ok(e) => e,
                    Err(e) => {
                        log(&format!("{name}: cannot write proposal: {e}"));
                        return None;
                    }
                };
                log(&proposal.line());
                let before = proposal.seq;
                self.follow(name, std::slice::from_ref(&action), vec![proposal.seq]);
                self.intent_since(name, &action, before).then_some(action)
            }
        }
    }

    /// Ask the plan's backend to choose among `offered`, writing a payload
    /// where the lever takes one. A reply that cannot be used, or chooses
    /// what was not offered, escalates.
    #[allow(clippy::too_many_arguments)]
    fn plan_backend(
        &mut self,
        name: &str,
        mandate: &Mandate,
        wanted: Option<&str>,
        found: &Plan,
        offered: &[String],
        branch: Seq,
        better: Worse,
    ) -> Settled {
        let backend = match self.config.backend(wanted) {
            Ok((b, _)) => b,
            Err(e) => {
                log(&format!("{name}: plan backend: {e}"));
                wanted.unwrap_or("default").to_string()
            }
        };
        let request = plan_request(name, mandate, &backend, found, offered, better);
        let ctx = Context {
            venture: name.to_string(),
            mandate: mandate.clone(),
            entries: vec![],
            state: String::new(),
            now: now(),
            backend: backend.clone(),
            children: vec![],
            summaries: vec![],
            asks: vec![],
            to_summarize: vec![],
        };
        let read = match self.config.backend(wanted) {
            Err(e) => Err(e),
            Ok(_) => self
                .warden
                .infer(&request)
                .and_then(|text| role::read_reply(&ctx, &text)),
        };
        let mut unusable: Option<String> = None;
        let mut settled = Settled::Done;
        match read {
            Err(e) => unusable = Some(e),
            Ok(records) => {
                for record in records {
                    match record {
                        Record::Proposal {
                            action,
                            reason,
                            payload,
                            ..
                        } if offered.contains(&action) => {
                            let payload = fit_payload(mandate, &action, payload, name);
                            settled = Settled::Lever(
                                action,
                                payload,
                                format!(
                                    "{backend} chose it among the planner's {}: {reason}",
                                    offered.join(", ")
                                ),
                            );
                        }
                        Record::Proposal { action, .. } => {
                            unusable = Some(format!(
                                "it chose {action}, which the planner did not offer"
                            ));
                        }
                        Record::Report { metric, note, .. } if metric == role::NOTHING => {
                            settled = Settled::Nothing(note);
                        }
                        Record::Report { metric, note, .. } if metric == role::UNUSABLE => {
                            unusable = Some(note);
                        }
                        Record::Ask {
                            action,
                            question,
                            payload,
                            ..
                        } => {
                            let payload = action
                                .as_deref()
                                .and_then(|a| fit_payload(mandate, a, payload, name));
                            let (already, may_ask) = {
                                let Ok(index) = self.index() else {
                                    return Settled::Done;
                                };
                                let already = action.as_deref().is_some_and(|a| {
                                    super::asked_already(&index, name, a, payload.as_ref())
                                });
                                (already, self.may_ask(&index))
                            };
                            let refused = action
                                .as_deref()
                                .and_then(|a| self.unaskable(name, a, &[branch]))
                                .or_else(|| {
                                    match (action.as_deref(), &payload, already, may_ask) {
                                        (Some(a), Some(p), false, true) => {
                                            self.precheck(name, a, p)
                                        }
                                        _ => None,
                                    }
                                });
                            if refused.is_some() {
                                // The refusal is the record; no ask.
                            } else if already {
                                log(&format!("{name}: the plan's ask is already open"));
                            } else if !may_ask {
                                log(&format!(
                                    "{name}: the plan's backend would ask, but the asks quota is used up; not asking"
                                ));
                                self.refuse(
                                    name,
                                    action.as_deref().unwrap_or("ask"),
                                    "AsksQuotaSpent".to_string(),
                                    "the asks quota is used up".to_string(),
                                    false,
                                );
                            } else {
                                let packet = self.packet(
                                    name,
                                    action.as_deref(),
                                    &[],
                                    Some(question.clone()),
                                    payload.is_some(),
                                );
                                match self.append(Record::Ask {
                                    packet,
                                    claim: Claim::Inferred,
                                    venture: name.to_string(),
                                    action,
                                    question,
                                    refusal: None,
                                    payload,
                                    signal: None,
                                    cites: vec![branch],
                                }) {
                                    Ok(e) => log(&e.line()),
                                    Err(e) => log(&format!("{name}: cannot write ask: {e}")),
                                }
                            }
                            settled = Settled::Done;
                        }
                        _ => {}
                    }
                }
            }
        }
        if let Some(why) = unusable {
            let why: String = why.chars().take(300).collect();
            let record = Record::Report {
                claim: Claim::Inferred,
                venture: name.to_string(),
                metric: role::UNUSABLE.to_string(),
                value: None,
                note: format!("{backend}: {why}"),
                source: None,
                confidence: None,
                cites: vec![branch],
                implausible: None,
                facts: None,
            };
            match self.append(record) {
                Ok(e) => log(&e.line()),
                Err(e) => log(&format!("{name}: cannot write record: {e}")),
            }
            if let Err(e) = self.escalate_calibrated(name, &why, &request) {
                log(&format!("{name}: cannot escalate: {e}"));
            }
            return Settled::Done;
        }
        settled
    }

    /// Critical slowing down: on the first report of an episode whose last
    /// `2W` plausible readings show rising variance and autocorrelation,
    /// ask once.
    pub(super) fn early_warning(&mut self, name: &str, metric: &str) {
        let rise = {
            let Ok(index) = self.index() else { return };
            let values: Vec<f64> = index.series(name, metric).iter().map(|(_, v)| *v).collect();
            slowing::starts(&values)
        };
        let Some(r) = rise else { return };
        let (ratio, up) = (r.variance_ratio(), r.autocorrelation_rise());
        self.signal(
            name,
            format!(
                "{metric} is slowing down: over the last {} reports its variance rose {ratio:.2}x and its lag-1 autocorrelation by {up:+.2}; it may be near a tipping point",
                slowing::SPAN
            ),
            Signal::Warning {
                metric: metric.to_string(),
                variance_ratio: ratio,
                autocorrelation_rise: up,
            },
        );
    }
}

/// The branch record of a plan: inferred, size-bounded.
pub fn branch_record(
    venture: &str,
    found: &Plan,
    members: u32,
    horizon: u32,
    better: Worse,
) -> Record {
    let (chosen, alternatives) = found.alternatives(better);
    Record::Branch {
        claim: Claim::Inferred,
        venture: venture.to_string(),
        chosen,
        alternatives,
        members,
        horizon,
        separated: found.separated,
    }
}

/// A record citing one more.
fn with_cites(record: Record, seq: Seq) -> Record {
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
            facts,
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
            facts,
        },
        other => other,
    }
}

/// The prompt that asks a backend to choose among the planner's options.
fn plan_request(
    name: &str,
    mandate: &Mandate,
    backend: &str,
    found: &Plan,
    offered: &[String],
    better: Worse,
) -> InferRequest {
    let tie = offered.len() > 1;
    let system = format!(
        "You are the planner's {} for a supervised venture. You cannot act. \
         The planner ran each option through an ensemble of a world model; {} \
         You may choose one of the options, ask the human one question, or do nothing. \
         Everything you say is recorded as inferred and never authorizes anything by itself. \
         Reply with one JSON object on one line and nothing else, in one of these forms:\n\
         {{\"choice\": \"nothing\", \"reason\": \"<one sentence>\"}}\n\
         {{\"choice\": \"action\", \"action\": \"<one of: {}>\", \"reason\": \"<one sentence>\"}}\n\
         {{\"choice\": \"ask\", \"question\": \"<one sentence>\", \"action\": \"<one of the options, or omit>\"}}\n\
         An action that takes a payload needs a \"payload\" field too: a string for text, \
         diff or file; an object for json. A human reads the payload before it runs.",
        if tie { "tiebreaker" } else { "writer" },
        if tie {
            "it could not tell the best apart from the runner-up."
        } else {
            "it chose this one, and it takes a payload for you to write."
        },
        offered
            .iter()
            .filter(|o| o.as_str() != NOTHING)
            .cloned()
            .collect::<Vec<_>>()
            .join(", "),
    );
    let mut user = String::new();
    user.push_str(&format!("Venture: {name}\n"));
    user.push_str(&format!("Purpose: {}\n", mandate.purpose.trim()));
    user.push_str(&format!(
        "Metric: {} ({} is better)\n",
        mandate.metric_name(),
        match better {
            Worse::Up => "higher",
            Worse::Down => "lower",
        }
    ));
    user.push_str("Options, with the metric at the horizon over the ensemble:\n");
    for w in found
        .ranked
        .iter()
        .filter(|w| offered.iter().any(|o| o == w.action()))
    {
        let q = w.quantiles();
        let what = match mandate.permitted(w.action()) {
            None => String::new(),
            Some(p) => format!(
                ", {}{}",
                if p.reversible {
                    "reversible"
                } else if p.delegated {
                    "irreversible, delegated"
                } else {
                    "irreversible, needs human approval"
                },
                p.takes()
                    .map(|t| format!(", takes {t}"))
                    .unwrap_or_default()
            ),
        };
        user.push_str(&format!(
            "  {}: p10 {} p50 {} p90 {}, worst member {}{what}\n",
            w.action(),
            q.p10,
            q.p50,
            q.p90,
            w.worst_target(better)
        ));
    }
    user.push_str("Your choice:");
    InferRequest {
        backend: backend.to_string(),
        system,
        user,
        max_tokens: 1024,
    }
}
