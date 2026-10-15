//! Records: what the ledger holds.
//!
//! A record is one enum. Its claim (measured, declared, inferred) and its
//! kind (report, proposal, action, ask, decision, intent, outcome) are
//! typed, never strings.

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
        f.write_str(self.as_str())
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
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a human decided.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "lowercase")]
pub enum Verdict {
    /// Yes to an ask.
    Approve { ask: Seq },
    /// No to an ask.
    Deny { ask: Seq, reason: String },
    /// Stop the venture until a human resumes it.
    Pause,
    /// Let a paused venture run again.
    Resume,
    /// Take on a mandate; `mandate` is the hash of its text, `path` its directory.
    Adopt { mandate: String, path: String },
}

/// One ledger record. Every variant carries its claim and its venture.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Record {
    /// A reading of the venture's metric. `value` is None when the report failed.
    Report {
        claim: Claim,
        venture: String,
        metric: String,
        value: Option<f64>,
        note: String,
    },
    /// A model suggests an action by name.
    Proposal {
        claim: Claim,
        venture: String,
        action: String,
        reason: String,
    },
    /// An action was taken, with the proof that allowed it and how it went.
    Action {
        claim: Claim,
        venture: String,
        action: String,
        proof: Proof,
        ok: bool,
        detail: String,
    },
    /// A question for the human, optionally about a specific action.
    Ask {
        claim: Claim,
        venture: String,
        action: Option<String>,
        question: String,
        refusal: Option<Refusal>,
    },
    /// A human's verdict.
    Decision {
        claim: Claim,
        venture: String,
        verdict: Verdict,
    },
    /// An irreversible action is about to run.
    Intent {
        claim: Claim,
        venture: String,
        action: String,
        proof: Proof,
    },
    /// How an intent went.
    Outcome {
        claim: Claim,
        venture: String,
        intent: Seq,
        ok: bool,
        detail: String,
    },
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
            | Record::Outcome { claim, .. } => *claim,
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
            | Record::Outcome { venture, .. } => venture,
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

    /// The bytes that get hashed. Field order is the declaration order, so
    /// the same record always gives the same bytes.
    pub fn canonical(&self) -> String {
        serde_json::to_string(self).expect("a record always serializes")
    }

    /// One line, no sequence number or time, for `tail` and friends.
    pub fn summary(&self) -> String {
        match self {
            Record::Report {
                metric,
                value: Some(v),
                note,
                ..
            } => {
                if note.is_empty() {
                    format!("{metric}={v}")
                } else {
                    format!("{metric}={v}  {note}")
                }
            }
            Record::Report {
                metric,
                value: None,
                note,
                ..
            } => format!("{metric}=?  {note}"),
            Record::Proposal { action, reason, .. } => format!("{action}: {reason}"),
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
                ..
            } => format!("{a}? {question}"),
            Record::Ask {
                action: None,
                question,
                ..
            } => question.clone(),
            Record::Decision { verdict, .. } => match verdict {
                Verdict::Approve { ask } => format!("approved #{ask}"),
                Verdict::Deny { ask, reason } => format!("denied #{ask}: {reason}"),
                Verdict::Pause => "paused".to_string(),
                Verdict::Resume => "resumed".to_string(),
                Verdict::Adopt { mandate, path } => {
                    format!(
                        "adopted mandate {} from {path}",
                        &mandate[..12.min(mandate.len())]
                    )
                }
            },
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
        }
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
        Record::Report {
            claim: Claim::Measured,
            venture: "counter".into(),
            metric: "count".into(),
            value: Some(v),
            note: String::new(),
        }
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
}
