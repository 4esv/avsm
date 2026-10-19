//! Records: what the ledger holds.
//!
//! A record is one enum. Its claim (measured, declared, inferred) and its
//! kind (report, proposal, action, ask, decision, intent, outcome, refusal,
//! prediction) are typed, never strings.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::gate::{Proof, Refusal};

/// Position of a record in the ledger, starting at 1.
pub type Seq = u64;

/// The hash the first record chains to.
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Who produced a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Claim {
    /// Code measured it: a report script, the kernel, the warden.
    Measured,
    /// A human declared it: a command, a mandate.
    Declared,
    /// A model inferred it.
    Inferred,
}

impl Claim {
    pub fn as_str(self) -> &'static str {
        match self {
            Claim::Measured => "measured",
            Claim::Declared => "declared",
            Claim::Inferred => "inferred",
        }
    }

    /// Only measured and declared records can authorize an action.
    /// Inferred records only propose or ask.
    pub fn authorizes(self) -> bool {
        self != Claim::Inferred
    }
}

impl fmt::Display for Claim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// What a record is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Report,
    Proposal,
    Action,
    Ask,
    Decision,
    Intent,
    Outcome,
    Refusal,
    Escalation,
    Answer,
    Prediction,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Report => "report",
            Kind::Proposal => "proposal",
            Kind::Action => "action",
            Kind::Ask => "ask",
            Kind::Decision => "decision",
            Kind::Intent => "intent",
            Kind::Outcome => "outcome",
            Kind::Refusal => "refusal",
            Kind::Escalation => "escalation",
            Kind::Answer => "answer",
            Kind::Prediction => "prediction",
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// What a lever takes: the shape of a payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Takes {
    Text,
    Json,
    Diff,
    File,
}

impl Takes {
    pub fn as_str(self) -> &'static str {
        match self {
            Takes::Text => "text",
            Takes::Json => "json",
            Takes::Diff => "diff",
            Takes::File => "file",
        }
    }

    /// Every shape, for messages.
    pub const ALL: &'static str = "text, json, diff or file";
}

impl fmt::Display for Takes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

impl std::str::FromStr for Takes {
    type Err = String;

    fn from_str(s: &str) -> Result<Takes, String> {
        match s.trim() {
            "text" => Ok(Takes::Text),
            "json" => Ok(Takes::Json),
            "diff" => Ok(Takes::Diff),
            "file" => Ok(Takes::File),
            other => Err(format!("takes is {}, not {other:?}", Takes::ALL)),
        }
    }
}

/// Content a lever will consume: produced by a model or a human, carried by
/// a proposal, an ask, or an amended approval, and fed to the lever only
/// once a proof allows it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Payload {
    Text { text: String },
    Json { json: serde_json::Value },
    Diff { diff: String },
    File { path: String },
}

impl Payload {
    /// The most a payload may hold, in bytes of its text form.
    pub const MAX: usize = 64 * 1024;

    /// Read text as a payload of the given shape, checking it fits.
    pub fn parse(takes: Takes, text: &str) -> Result<Payload, String> {
        if text.len() > Payload::MAX {
            return Err(format!(
                "the payload is {} bytes; the most a payload may hold is {}",
                text.len(),
                Payload::MAX
            ));
        }
        if text.trim().is_empty() {
            return Err(format!("the payload is empty; this lever takes {takes}"));
        }
        Ok(match takes {
            Takes::Text => Payload::Text {
                text: text.to_string(),
            },
            Takes::Json => Payload::Json {
                json: serde_json::from_str(text)
                    .map_err(|e| format!("the payload is not JSON: {e}"))?,
            },
            Takes::Diff => {
                let looks_like_diff = text.lines().any(|l| {
                    l.starts_with("--- ") || l.starts_with("+++ ") || l.starts_with("@@ ")
                });
                if !looks_like_diff {
                    return Err(
                        "the payload is not a diff; a diff has ---, +++ and @@ lines".to_string(),
                    );
                }
                Payload::Diff {
                    diff: text.to_string(),
                }
            }
            Takes::File => {
                let path = text.trim();
                if path.lines().count() != 1 {
                    return Err("a file payload is one path on one line".to_string());
                }
                if path.starts_with('/') || path.split('/').any(|part| part == "..") {
                    return Err(format!(
                        "a file payload is a path inside the venture directory, not {path:?}"
                    ));
                }
                Payload::File {
                    path: path.to_string(),
                }
            }
        })
    }

    /// Read a JSON value a model replied with as a payload of the given
    /// shape: a string for text, diff and file; an object or array for json.
    pub fn from_value(takes: Takes, value: &serde_json::Value) -> Result<Payload, String> {
        match (takes, value) {
            (Takes::Json, v) if v.is_object() || v.is_array() => {
                Payload::parse(takes, &v.to_string())
            }
            (_, serde_json::Value::String(s)) => Payload::parse(takes, s),
            (t, v) => Err(format!(
                "this lever takes {t}; the reply gave {}",
                match v {
                    serde_json::Value::Null => "nothing",
                    serde_json::Value::Bool(_) => "true or false",
                    serde_json::Value::Number(_) => "a number",
                    serde_json::Value::String(_) => "text",
                    serde_json::Value::Array(_) => "a list",
                    serde_json::Value::Object(_) => "a set of fields",
                }
            )),
        }
    }

    /// The shape.
    pub fn takes(&self) -> Takes {
        match self {
            Payload::Text { .. } => Takes::Text,
            Payload::Json { .. } => Takes::Json,
            Payload::Diff { .. } => Takes::Diff,
            Payload::File { .. } => Takes::File,
        }
    }

    /// The content as text: what a lever reads on its standard input.
    pub fn text(&self) -> String {
        match self {
            Payload::Text { text } => text.clone(),
            Payload::Json { json } => json.to_string(),
            Payload::Diff { diff } => diff.clone(),
            Payload::File { path } => path.clone(),
        }
    }

    /// The shape of a payload for telling alike payloads apart: the kind,
    /// and for json the top-level field names.
    pub fn shape(&self) -> String {
        match self {
            Payload::Json {
                json: serde_json::Value::Object(map),
            } => {
                let keys: Vec<&str> = map.keys().map(String::as_str).collect();
                format!("json({})", keys.join(","))
            }
            other => other.takes().to_string(),
        }
    }

    /// One short line: the kind and the start of the content.
    pub fn summary(&self) -> String {
        let text = self.text();
        let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        let lines = text.lines().count();
        let mut head: String = first.chars().take(60).collect();
        if first.chars().count() > 60 {
            head.push('…');
        }
        if lines > 1 {
            format!("{} {head:?} ({lines} lines)", self.takes())
        } else {
            format!("{} {head:?}", self.takes())
        }
    }
}

/// What a human decided.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "lowercase")]
pub enum Verdict {
    /// Yes to an ask, with the payload as amended when the human changed it.
    Approve {
        ask: Seq,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<Payload>,
    },
    /// No to an ask.
    Deny { ask: Seq, reason: String },
    /// Stop the venture until a human resumes it.
    Pause,
    /// Let a paused venture run again.
    Resume,
    /// Take on a mandate; `mandate` is the hash of its text, `path` its
    /// directory, `text` the whole mandate as adopted. A 1.0 adoption has
    /// no text; it is absent from the record, so old hashes still hold.
    Adopt {
        mandate: String,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
}

/// Why the kernel asked on its own: the algedonic channel. An ask carrying
/// one is recognised from this field, never from its question.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "signal", rename_all = "lowercase")]
pub enum Signal {
    /// A threshold was crossed while its lever was refused for a cap, a
    /// budget or a quota.
    Saturated { action: String, reason: String },
    /// A run of `action` has an intent and no outcome: whether it happened
    /// is a human's to say, and until then the lever is refused.
    Pending { intent: Seq, action: String },
    /// A lever ran and the metric did not move as the mandate expects, or
    /// its runs kept failing. Until a declared decision answers the ask,
    /// every run of the lever needs a declared decision of its own.
    Ineffective { action: String, why: String },
    /// A guard's reading is past its bound and the guard asks: every lever
    /// but stop is refused until the reading is back inside.
    Guard {
        guard: String,
        value: f64,
        bound: f64,
    },
    /// The target improved over an audit's window while the audit worsened
    /// past its tolerance.
    Audit {
        audit: String,
        target_delta: f64,
        audit_delta: f64,
    },
    /// A report of the metric is not to be believed: the first of an
    /// episode, which ends at the next plausible report.
    Implausible { metric: String, why: String },
}

/// One ledger record. Every variant carries its claim and its venture.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Record {
    /// A reading of the venture's metric, or any other fact about a topic.
    /// `value` is None when the report failed or the fact is not a number.
    /// A fact read from the open world names its source and how sure it
    /// is; a summary cites the records it stands for.
    Report {
        claim: Claim,
        venture: String,
        metric: String,
        value: Option<f64>,
        note: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confidence: Option<f64>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        cites: Vec<Seq>,
        /// Set when code read the value and does not believe it: NaN,
        /// missing, out of range, too big a step, or a flap. Such a report
        /// is kept, and is never a premise for a crossed threshold.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        implausible: Option<String>,
        /// On a summary: the numbers of the span, computed by code. The
        /// narrative in `note` is inferred; these are not.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        facts: Option<crate::facts::Facts>,
    },
    /// A model, or a human by hand, suggests an action by name, with the
    /// payload the lever would consume.
    Proposal {
        claim: Claim,
        venture: String,
        action: String,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<Payload>,
        /// The records this one answers to: an escalation, for a proposal
        /// a second backend made.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        cites: Vec<Seq>,
    },
    /// An action was taken, with the proof that allowed it and how it went.
    Action {
        claim: Claim,
        venture: String,
        action: String,
        proof: Proof,
        ok: bool,
        detail: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<Payload>,
    },
    /// A question for the human, optionally about a specific action, with
    /// the payload that action would consume.
    Ask {
        claim: Claim,
        venture: String,
        action: Option<String>,
        question: String,
        refusal: Option<Refusal>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<Payload>,
        /// Set when the kernel asks because a limit cannot keep up, not
        /// because a lever needs a decision. Read by code, never by text.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal: Option<Signal>,
        /// The records this one answers to: an escalation, for an ask a
        /// second backend made.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        cites: Vec<Seq>,
    },
    /// A human's verdict. `by` names the human when the record came in
    /// over a path that knows who they are.
    Decision {
        claim: Claim,
        venture: String,
        verdict: Verdict,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
    },
    /// A lever is about to run: stored before it runs, so a run never
    /// happens unrecorded. `key` is the run's idempotency key (see `key`),
    /// which the lever sees as `AVSM_KEY`; a 1.0 ledger's intents have none.
    Intent {
        claim: Claim,
        venture: String,
        action: String,
        proof: Proof,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<Payload>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
    /// How an intent went: measured when code saw it, declared when a
    /// human said so.
    Outcome {
        claim: Claim,
        venture: String,
        intent: Seq,
        ok: bool,
        detail: String,
    },
    /// The gate, a quota or the asks quota said no. The first refusal of a
    /// lever for a reason opens one; repeats are counted in memory; a
    /// changed reason, a new day or a restart closes it with the count,
    /// and the closing record names the opening one in `closes`. It
    /// authorizes nothing and counts toward nothing.
    Refusal {
        claim: Claim,
        venture: String,
        action: String,
        /// A stable key: the reason without its counters.
        reason: String,
        /// The reasons in words.
        detail: String,
        count: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        closes: Option<Seq>,
    },
    /// The first backend could not answer, so a second was asked (or, with
    /// `refused`, was not: the reason). Written by code before the call and
    /// authorizes nothing.
    Escalation {
        claim: Claim,
        venture: String,
        from: String,
        to: String,
        why: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refused: Option<String>,
    },
    /// How an escalation went: the records of the second backend's answer
    /// (each cites the escalation), or none when it too was unusable.
    /// Written by code after the call and authorizes nothing.
    Answer {
        claim: Claim,
        venture: String,
        escalation: Seq,
        ok: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        cites: Vec<Seq>,
        detail: String,
    },
    /// What a world model says the venture's metric will read at
    /// `resolves_at`, after `action` if one is about to run. Always
    /// inferred: it ranks, vetoes and asks, and never authorizes. The
    /// first trusted, plausible report of the metric at or after
    /// `resolves_at` resolves it, and code scores it.
    Prediction {
        claim: Claim,
        venture: String,
        /// `baseline` or `world`.
        model: String,
        metric: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action: Option<String>,
        made_at: i64,
        resolves_at: i64,
        quantiles: Quantiles,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        events: Vec<Event>,
    },
}

/// The quantiles of a forecast: the 5th to the 95th percentile.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Quantiles {
    pub p5: f64,
    pub p10: f64,
    pub p25: f64,
    pub p50: f64,
    pub p75: f64,
    pub p90: f64,
    pub p95: f64,
}

impl Quantiles {
    /// The levels, in the order of `values`.
    pub const LEVELS: [f64; 7] = [0.05, 0.10, 0.25, 0.50, 0.75, 0.90, 0.95];

    /// The values, lowest level first.
    pub fn values(&self) -> [f64; 7] {
        [
            self.p5, self.p10, self.p25, self.p50, self.p75, self.p90, self.p95,
        ]
    }

    /// From seven values, lowest level first.
    pub fn from_values(v: [f64; 7]) -> Quantiles {
        Quantiles {
            p5: v[0],
            p10: v[1],
            p25: v[2],
            p50: v[3],
            p75: v[4],
            p90: v[5],
            p95: v[6],
        }
    }

    /// Every value finite and none below the one before.
    pub fn valid(&self) -> bool {
        let v = self.values();
        v.iter().all(|x| x.is_finite()) && v.windows(2).all(|w| w[0] <= w[1])
    }
}

/// The chance a forecast gives a named event, like `guard.spend` breaching.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub name: String,
    pub p: f64,
}

/// The most events a prediction may carry, and the longest name.
pub const MAX_EVENTS: usize = 16;
pub const MAX_EVENT_NAME: usize = 64;

/// The idempotency key of a lever run: sha256 over the venture, the
/// action, the cited records in order, and the payload text. The same run
/// asked for again has the same key; any other run has another.
pub fn key(venture: &str, action: &str, cites: &[Seq], payload: Option<&Payload>) -> String {
    let mut cites = cites.to_vec();
    cites.sort_unstable();
    let text = payload.map(Payload::text);
    let canonical = serde_json::to_string(&(venture, action, cites, text))
        .expect("strings and numbers always serialize");
    hex::encode(Sha256::digest(canonical.as_bytes()))
}

impl Record {
    pub fn claim(&self) -> Claim {
        match self {
            Record::Report { claim, .. }
            | Record::Proposal { claim, .. }
            | Record::Action { claim, .. }
            | Record::Ask { claim, .. }
            | Record::Decision { claim, .. }
            | Record::Intent { claim, .. }
            | Record::Outcome { claim, .. }
            | Record::Refusal { claim, .. }
            | Record::Escalation { claim, .. }
            | Record::Answer { claim, .. }
            | Record::Prediction { claim, .. } => *claim,
        }
    }

    pub fn venture(&self) -> &str {
        match self {
            Record::Report { venture, .. }
            | Record::Proposal { venture, .. }
            | Record::Action { venture, .. }
            | Record::Ask { venture, .. }
            | Record::Decision { venture, .. }
            | Record::Intent { venture, .. }
            | Record::Outcome { venture, .. }
            | Record::Refusal { venture, .. }
            | Record::Escalation { venture, .. }
            | Record::Answer { venture, .. }
            | Record::Prediction { venture, .. } => venture,
        }
    }

    pub fn kind(&self) -> Kind {
        match self {
            Record::Report { .. } => Kind::Report,
            Record::Proposal { .. } => Kind::Proposal,
            Record::Action { .. } => Kind::Action,
            Record::Ask { .. } => Kind::Ask,
            Record::Decision { .. } => Kind::Decision,
            Record::Intent { .. } => Kind::Intent,
            Record::Outcome { .. } => Kind::Outcome,
            Record::Refusal { .. } => Kind::Refusal,
            Record::Escalation { .. } => Kind::Escalation,
            Record::Answer { .. } => Kind::Answer,
            Record::Prediction { .. } => Kind::Prediction,
        }
    }

    /// The action this record names, if it names one.
    pub fn action(&self) -> Option<&str> {
        match self {
            Record::Proposal { action, .. }
            | Record::Action { action, .. }
            | Record::Intent { action, .. } => Some(action),
            Record::Ask { action, .. } => action.as_deref(),
            _ => None,
        }
    }

    /// The payload this record carries, if any: a proposal's, an ask's, an
    /// amended approval's, or the one an action ran with.
    pub fn payload(&self) -> Option<&Payload> {
        match self {
            Record::Proposal { payload, .. }
            | Record::Ask { payload, .. }
            | Record::Action { payload, .. }
            | Record::Intent { payload, .. } => payload.as_ref(),
            Record::Decision {
                verdict: Verdict::Approve { payload, .. },
                ..
            } => payload.as_ref(),
            _ => None,
        }
    }

    /// The records this one cites, if it cites any.
    pub fn cites(&self) -> &[Seq] {
        match self {
            Record::Report { cites, .. }
            | Record::Proposal { cites, .. }
            | Record::Ask { cites, .. }
            | Record::Answer { cites, .. } => cites,
            _ => &[],
        }
    }

    /// A report with no extras: the common case.
    pub fn report(
        claim: Claim,
        venture: &str,
        metric: &str,
        value: Option<f64>,
        note: &str,
    ) -> Record {
        Record::Report {
            claim,
            venture: venture.to_string(),
            metric: metric.to_string(),
            value,
            note: note.to_string(),
            source: None,
            confidence: None,
            cites: vec![],
            implausible: None,
            facts: None,
        }
    }

    /// Is this a report code marked implausible?
    pub fn implausible(&self) -> Option<&str> {
        match self {
            Record::Report { implausible, .. } => implausible.as_deref(),
            _ => None,
        }
    }

    /// The bytes that get hashed. Field order is the declaration order, so
    /// the same record always gives the same bytes.
    pub fn canonical(&self) -> String {
        serde_json::to_string(self).expect("a record always serializes")
    }

    /// One line, no sequence number or time, for `tail` and friends. A
    /// multi-line note shows its first line.
    pub fn summary(&self) -> String {
        match self {
            Record::Report {
                metric,
                value,
                note,
                implausible,
                ..
            } => {
                let mut text = match value {
                    Some(v) => format!("{metric}={v}"),
                    None => format!("{metric}=?"),
                };
                if let Some(why) = implausible {
                    text.push_str(&format!("  implausible: {why}"));
                }
                if !note.is_empty() || value.is_none() {
                    text.push_str(&format!("  {}", first_line(note)));
                }
                text
            }
            Record::Proposal {
                action,
                reason,
                payload,
                ..
            } => match payload {
                Some(p) => format!("{action}: {reason}  [{}]", p.summary()),
                None => format!("{action}: {reason}"),
            },
            Record::Action {
                action, ok, detail, ..
            } => format!(
                "{action}: {}{}",
                if *ok { "done" } else { "failed" },
                if detail.is_empty() {
                    String::new()
                } else {
                    format!("  {detail}")
                }
            ),
            Record::Ask {
                action: Some(a),
                question,
                payload,
                ..
            } => match payload {
                Some(p) => format!("{a}? {question}  [{}]", p.summary()),
                None => format!("{a}? {question}"),
            },
            Record::Ask {
                action: None,
                question,
                ..
            } => question.clone(),
            Record::Decision { verdict, by, .. } => {
                let text = match verdict {
                    Verdict::Approve { ask, payload: None } => format!("approved #{ask}"),
                    Verdict::Approve {
                        ask,
                        payload: Some(p),
                    } => format!("approved #{ask} with amended {}", p.summary()),
                    Verdict::Deny { ask, reason } => format!("denied #{ask}: {reason}"),
                    Verdict::Pause => "paused".to_string(),
                    Verdict::Resume => "resumed".to_string(),
                    Verdict::Adopt { mandate, path, .. } => {
                        format!(
                            "adopted mandate {} from {path}",
                            &mandate[..12.min(mandate.len())]
                        )
                    }
                };
                match by {
                    Some(who) => format!("{text}  by {who}"),
                    None => text,
                }
            }
            Record::Intent { action, .. } => format!("{action}: starting"),
            Record::Outcome {
                intent, ok, detail, ..
            } => format!(
                "#{intent} {}{}",
                if *ok { "done" } else { "failed" },
                if detail.is_empty() {
                    String::new()
                } else {
                    format!("  {detail}")
                }
            ),
            Record::Refusal {
                action,
                detail,
                count,
                closes: None,
                ..
            } => format!("{action} refused ({count}): {detail}"),
            Record::Refusal {
                action,
                detail,
                count,
                closes: Some(open),
                ..
            } => format!("#{open} {action} refused {count} times: {detail}"),
            Record::Escalation {
                from,
                to,
                why,
                refused: None,
                ..
            } => format!("escalated {from} -> {to}: {}", first_line(why)),
            Record::Escalation {
                from,
                to,
                why,
                refused: Some(reason),
                ..
            } => format!(
                "escalation {from} -> {to} refused: {} ({})",
                first_line(reason),
                first_line(why)
            ),
            Record::Answer {
                escalation,
                ok,
                detail,
                ..
            } => format!(
                "#{escalation} answer {}  {detail}",
                if *ok { "usable" } else { "unusable" }
            ),
            Record::Prediction {
                model,
                metric,
                action,
                resolves_at,
                quantiles: q,
                ..
            } => format!(
                "{model}: {metric} p50 {} (p10 {} to p90 {}){} by {}",
                q.p50,
                q.p10,
                q.p90,
                match action {
                    Some(a) => format!(" after {a}"),
                    None => String::new(),
                },
                resolves_at
            ),
        }
    }
}

fn first_line(note: &str) -> String {
    match note.split_once('\n') {
        Some((first, _)) => format!("{first} …"),
        None => note.to_string(),
    }
}

/// A record as stored: numbered, timed, and chained to the one before it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub seq: Seq,
    /// Unix seconds.
    pub at: i64,
    /// Hash of the previous entry; `GENESIS` for the first.
    pub prev: String,
    /// Hash of this entry.
    pub hash: String,
    /// Trust weight. Unused in v0: trust is binary and this is always 1.
    pub weight: f64,
    pub record: Record,
}

impl Entry {
    /// Build an entry and compute its hash.
    pub fn new(seq: Seq, at: i64, prev: &str, record: Record) -> Entry {
        let weight = 1.0;
        let hash = Entry::digest(seq, at, prev, weight, &record);
        Entry {
            seq,
            at,
            prev: prev.to_string(),
            hash,
            weight,
            record,
        }
    }

    /// The hash of an entry's contents.
    pub fn digest(seq: Seq, at: i64, prev: &str, weight: f64, record: &Record) -> String {
        let mut h = Sha256::new();
        h.update(seq.to_string());
        h.update(b"\n");
        h.update(at.to_string());
        h.update(b"\n");
        h.update(prev);
        h.update(b"\n");
        h.update(weight.to_string());
        h.update(b"\n");
        h.update(record.canonical());
        hex::encode(h.finalize())
    }

    /// Does the stored hash match the contents?
    pub fn hash_ok(&self) -> bool {
        self.hash == Entry::digest(self.seq, self.at, &self.prev, self.weight, &self.record)
    }

    /// Time as RFC 3339 in UTC.
    pub fn time(&self) -> String {
        match chrono::DateTime::from_timestamp(self.at, 0) {
            Some(t) => t.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            None => self.at.to_string(),
        }
    }

    /// One stable line: seq, time, venture, claim, kind, summary.
    pub fn line(&self) -> String {
        format!(
            "#{:<5} {}  {:<12} {:<9} {:<9} {}",
            self.seq,
            self.time(),
            self.record.venture(),
            self.record.claim(),
            self.record.kind(),
            self.record.summary()
        )
    }
}

/// Where a chain fails to verify.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("record #{seq}: {what}")]
pub struct Tamper {
    pub seq: Seq,
    pub what: String,
}

/// Verify a chain of entries, oldest first: sequence numbers run 1, 2, 3...,
/// each entry chains to the hash of the one before, and every hash matches
/// its contents.
pub fn verify_chain(entries: &[Entry]) -> Result<(), Tamper> {
    let mut prev = GENESIS.to_string();
    for (i, e) in entries.iter().enumerate() {
        let expect = i as Seq + 1;
        if e.seq != expect {
            return Err(Tamper {
                seq: e.seq,
                what: format!("expected record #{expect}"),
            });
        }
        if e.prev != prev {
            return Err(Tamper {
                seq: e.seq,
                what: "does not chain to the record before it".to_string(),
            });
        }
        if !e.hash_ok() {
            return Err(Tamper {
                seq: e.seq,
                what: "hash does not match its contents".to_string(),
            });
        }
        prev = e.hash.clone();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(v: f64) -> Record {
        Record::report(Claim::Measured, "counter", "count", Some(v), "")
    }

    #[test]
    fn claim_and_kind_are_typed_and_round_trip() {
        let r = report(1.0);
        assert_eq!(r.claim(), Claim::Measured);
        assert_eq!(r.kind(), Kind::Report);
        let json = r.canonical();
        assert!(json.contains("\"kind\":\"report\""));
        assert!(json.contains("\"claim\":\"measured\""));
        let back: Record = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn chain_verifies_and_detects_change() {
        let a = Entry::new(1, 10, GENESIS, report(1.0));
        let b = Entry::new(2, 11, &a.hash, report(2.0));
        assert_eq!(verify_chain(&[a.clone(), b.clone()]), Ok(()));
        let mut bad = b.clone();
        bad.at += 1;
        assert!(verify_chain(&[a.clone(), bad]).is_err());
        let mut bad = b;
        bad.record = report(3.0);
        assert!(verify_chain(&[a, bad]).is_err());
    }

    #[test]
    fn records_from_0_1_read_back_byte_for_byte() {
        // Text a 0.1.0 ledger stored: the new optional fields are absent,
        // and must stay absent when the record is written again, or every
        // old hash would break.
        let old = [
            r#"{"kind":"report","claim":"measured","venture":"counter","metric":"count","value":11.0,"note":""}"#,
            r#"{"kind":"proposal","claim":"inferred","venture":"shop","action":"tidy","reason":"slow day"}"#,
            r#"{"kind":"ask","claim":"inferred","venture":"shop","action":"wipe","question":"may I wipe?","refusal":null}"#,
            r#"{"kind":"decision","claim":"declared","venture":"shop","verdict":{"verdict":"approve","ask":9}}"#,
            r#"{"kind":"decision","claim":"declared","venture":"shop","verdict":{"verdict":"adopt","mandate":"abc","path":"/x"}}"#,
            r#"{"kind":"action","claim":"measured","venture":"shop","action":"tidy","proof":{"venture":"shop","action":"tidy","steps":[]},"ok":true,"detail":""}"#,
            r#"{"kind":"intent","claim":"measured","venture":"shop","action":"deploy","proof":{"venture":"shop","action":"deploy","steps":[]}}"#,
            r#"{"kind":"outcome","claim":"measured","venture":"shop","intent":6,"ok":true,"detail":""}"#,
        ];
        for text in old {
            let record: Record = serde_json::from_str(text).unwrap();
            assert_eq!(record.canonical(), text);
            assert!(record.payload().is_none());
        }
    }

    #[test]
    fn an_adoption_with_text_round_trips_and_one_without_hashes_as_before() {
        let with = Record::Decision {
            claim: Claim::Declared,
            venture: "shop".into(),
            verdict: Verdict::Adopt {
                mandate: "abc".into(),
                path: "/x".into(),
                text: Some("purpose: p\n".into()),
            },
            by: None,
        };
        let back: Record = serde_json::from_str(&with.canonical()).unwrap();
        assert_eq!(back, with);
        let without = r#"{"kind":"decision","claim":"declared","venture":"shop","verdict":{"verdict":"adopt","mandate":"abc","path":"/x"}}"#;
        let record: Record = serde_json::from_str(without).unwrap();
        assert!(matches!(
            &record,
            Record::Decision {
                verdict: Verdict::Adopt { text: None, .. },
                ..
            }
        ));
        assert_eq!(record.canonical(), without);
        assert_ne!(record.canonical(), with.canonical());
    }

    #[test]
    fn payloads_parse_by_shape_and_keep_their_bounds() {
        let p = Payload::parse(Takes::Text, "hello\nworld").unwrap();
        assert_eq!(p.takes(), Takes::Text);
        assert_eq!(p.text(), "hello\nworld");
        assert_eq!(p.summary(), "text \"hello\" (2 lines)");
        assert_eq!(p.shape(), "text");

        let p = Payload::parse(Takes::Json, r#"{"b": 1, "a": "x"}"#).unwrap();
        assert_eq!(p.text(), r#"{"a":"x","b":1}"#, "json is canonical");
        assert_eq!(p.shape(), "json(a,b)");
        assert!(Payload::parse(Takes::Json, "not json")
            .unwrap_err()
            .contains("not JSON"));

        let diff = "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(
            Payload::parse(Takes::Diff, diff).unwrap().takes(),
            Takes::Diff
        );
        assert!(Payload::parse(Takes::Diff, "just words")
            .unwrap_err()
            .contains("not a diff"));

        assert_eq!(
            Payload::parse(Takes::File, "out/report.csv\n").unwrap(),
            Payload::File {
                path: "out/report.csv".into()
            }
        );
        assert!(Payload::parse(Takes::File, "../etc/passwd").is_err());
        assert!(Payload::parse(Takes::File, "/etc/passwd").is_err());

        assert!(Payload::parse(Takes::Text, "  ")
            .unwrap_err()
            .contains("empty"));
        let big = "x".repeat(Payload::MAX + 1);
        assert!(Payload::parse(Takes::Text, &big)
            .unwrap_err()
            .contains("the most a payload may hold"));

        let v = serde_json::json!({"to": "x"});
        assert_eq!(
            Payload::from_value(Takes::Json, &v).unwrap().shape(),
            "json(to)"
        );
        assert!(Payload::from_value(Takes::Text, &v)
            .unwrap_err()
            .contains("takes text"));
        assert_eq!(
            Payload::from_value(Takes::Text, &serde_json::json!("hi")).unwrap(),
            Payload::Text { text: "hi".into() }
        );
        assert_eq!("json".parse::<Takes>().unwrap(), Takes::Json);
        assert!("blob".parse::<Takes>().is_err());
    }

    #[test]
    fn summaries_show_payloads_and_identities() {
        let p = Record::Proposal {
            cites: vec![],
            claim: Claim::Inferred,
            venture: "shop".into(),
            action: "announce".into(),
            reason: "new stock".into(),
            payload: Some(Payload::Text {
                text: "Hello all".into(),
            }),
        };
        assert_eq!(p.summary(), "announce: new stock  [text \"Hello all\"]");
        let d = Record::Decision {
            claim: Claim::Declared,
            venture: "shop".into(),
            verdict: Verdict::Approve {
                ask: 4,
                payload: Some(Payload::Text {
                    text: "Hello".into(),
                }),
            },
            by: Some("alice@github".into()),
        };
        assert_eq!(
            d.summary(),
            "approved #4 with amended text \"Hello\"  by alice@github"
        );
        let r = Record::report(Claim::Measured, "self", "digest", None, "two lines\nsecond");
        assert_eq!(r.summary(), "digest=?  two lines …");
    }
}
