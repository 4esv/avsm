//! The gate: an action passes only if a proof exists from measured and
//! declared facts under a mandate. The proof, or the failed attempt at one,
//! is the derivation `why` prints.

use serde::{Deserialize, Serialize};

use crate::mandate::{Cap, Mandate};
use crate::record::{Claim, Entry, Kind, Record, Seq};

/// A mandate as adopted: tied to the declared record that adopted it.
#[derive(Clone, Debug, PartialEq)]
pub struct Adopted {
    pub seq: Seq,
    pub venture: String,
    pub hash: String,
    pub mandate: Mandate,
}

/// One line of a derivation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum Step {
    /// The declared record adopting the mandate the proof rests on.
    Mandate { seq: Seq, hash: String },
    /// What the mandate says about the action.
    Permitted {
        action: String,
        cap: Cap,
        reversible: bool,
        delegated: bool,
    },
    /// A measured report the proof uses.
    Report {
        seq: Seq,
        metric: String,
        value: f64,
    },
    /// The threshold in the mandate that the report crosses.
    Threshold {
        metric: String,
        bound: f64,
        above: bool,
    },
    /// A declared decision the proof uses.
    Decision { seq: Seq, what: String },
    /// A record cited as the occasion for acting. It authorizes nothing.
    Occasion {
        seq: Seq,
        claim: Claim,
        kind: Kind,
        what: String,
    },
    /// How much of the cap is used.
    Cap { used: u32, cap: Cap },
    /// The venture is not paused.
    Running { venture: String },
    /// A rule that needs no premise.
    Axiom { text: String },
}

/// Why an action is refused.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Reason {
    NoMandate { venture: String },
    NotPermitted { action: String },
    Forbidden { action: String },
    Hardwired { action: String, why: String },
    Paused { venture: String },
    OverCap { used: u32, cap: Cap },
    NeedsDecision { action: String },
    NotJustified { action: String },
    OnlyInferred { seqs: Vec<Seq> },
    UnknownRecord { seq: Seq },
    NeedsDeclaredResume { venture: String },
}

/// A derivation that reaches the action.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Proof {
    pub venture: String,
    pub action: String,
    pub steps: Vec<Step>,
}

/// A derivation that does not.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Refusal {
    pub venture: String,
    pub action: String,
    pub steps: Vec<Step>,
    pub reasons: Vec<Reason>,
}

/// Decide whether `candidate` (an action or intent record not yet in the
/// ledger) may happen, given the ledger so far and the adopted mandates.
/// `now` is unix seconds, used for caps.
pub fn gate(
    _candidate: &Record,
    _ledger: &[Entry],
    _mandates: &[Adopted],
    _now: i64,
) -> Result<Proof, Refusal> {
    unimplemented!("issue 3")
}
