//! The gate: an action passes only if a proof exists from measured and
//! declared facts under a mandate. Mandates and records are facts;
//! permissions are rules. The proof, or the failed attempt at one, is the
//! derivation `why` prints.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use ascent::ascent;
use serde::{Deserialize, Serialize};

use crate::config::Quotas;
use crate::index::{Index, Pulled};
use crate::mandate::{Cap, Controller, Mandate, ParentCall, Permitted};
use crate::record::{Claim, Entry, Kind, Payload, Record, Seq, Takes, Verdict};

/// The action that always passes.
pub const STOP: &str = crate::mandate::STOP;
/// The action that needs a declared decision.
pub const RESUME: &str = "resume";
/// The built-in action that adopts a changed mandate: self proposes it,
/// only a declared decision runs it.
pub const DELEGATE: &str = "delegate";
/// The built-in venture that watches avsm itself.
pub const SELF: &str = "self";

/// Actions the self venture can never take, whatever its mandate says.
pub const HARDWIRED: &[(&str, &str)] = &[
    (
        "edit-mandate",
        "the self venture cannot edit its own mandate",
    ),
    (DELEGATE, "the self venture cannot edit its own mandate"),
    ("merge-main", "the self venture cannot merge to main"),
    ("touch-keys", "the self venture cannot touch keys"),
];

/// Command fragments that mark a self action as one of the above.
const HARDWIRED_RUN: &[(&str, &str)] = &[
    (
        "mandate.yaml",
        "the self venture cannot edit its own mandate",
    ),
    ("git merge", "the self venture cannot merge to main"),
    ("git push", "the self venture cannot merge to main"),
    ("keys", "the self venture cannot touch keys"),
];

/// An action about to be taken: for which venture, which action, and the
/// records it rests on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub venture: String,
    pub action: String,
    pub cites: Vec<Seq>,
}

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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        takes: Option<Takes>,
        #[serde(default, skip_serializing_if = "is_zero")]
        cost: u32,
    },
    /// The record whose payload the lever consumes: the amended approval
    /// when a human changed it, else the ask, else the proposal.
    Payload {
        from: Seq,
        claim: Claim,
        kind: Takes,
        summary: String,
    },
    /// A report the proof uses.
    Report {
        seq: Seq,
        claim: Claim,
        metric: String,
        value: f64,
    },
    /// The threshold the report crosses: the mandate's, or the one the
    /// parent set with its `threshold` lever, cited as `from`.
    Threshold {
        metric: String,
        bound: f64,
        above: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<Seq>,
    },
    /// The budget the parent set for this venture, and what is spent of
    /// it today.
    Budget { from: Seq, spent: u32, budget: u32 },
    /// A shared quota that applies, and how much of it is used.
    Quota { what: String, used: u32, cap: Cap },
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
    NoMandate {
        venture: String,
    },
    NotPermitted {
        action: String,
    },
    Forbidden {
        action: String,
    },
    Hardwired {
        action: String,
        why: String,
    },
    Paused {
        venture: String,
    },
    OverCap {
        used: u32,
        cap: Cap,
    },
    NeedsDecision {
        action: String,
        /// The lever was found ineffective: every run asks.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        suspended: bool,
    },
    NotJustified {
        action: String,
    },
    OnlyInferred {
        seqs: Vec<Seq>,
    },
    UnknownRecord {
        seq: Seq,
    },
    NeedsDeclaredResume {
        venture: String,
    },
    NoPayload {
        action: String,
        takes: Takes,
    },
    WrongPayload {
        action: String,
        takes: Takes,
        found: Takes,
    },
    OverBudget {
        spent: u32,
        cost: u32,
        budget: u32,
    },
    OverQuota {
        what: String,
        used: u32,
        cap: Cap,
    },
    /// An earlier run of this lever has an intent and no outcome: it may
    /// or may not have happened, and it is never run again blindly.
    Pending {
        intent: Seq,
    },
    /// A parent lever names a venture that is not this parent's child.
    NotAChild {
        venture: String,
        child: String,
    },
    /// A budget under the child's floor.
    BelowFloor {
        child: String,
        budget: u32,
        floor: u32,
    },
    /// A cut to a child while a lower-priority child sits above its floor.
    Priority {
        child: String,
        spared: String,
        budget: u32,
        floor: u32,
    },
    /// A budget change smaller than the deadband.
    InDeadband {
        child: String,
        change: u32,
        deadband: u32,
    },
    /// A budget change that reverses the last one inside the hold.
    Reversal {
        child: String,
        last: Seq,
        hold: Duration,
    },
    /// A guard's latest reading is past its bound: no lever but stop runs
    /// until it is back inside, whatever approves or calls for it.
    Guarded {
        guard: String,
        value: f64,
        above: bool,
        bound: f64,
    },
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// A derivation that reaches the action.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Proof {
    pub venture: String,
    pub action: String,
    pub steps: Vec<Step>,
}

impl Proof {
    /// The record the payload comes from, when the lever takes one.
    pub fn payload_from(&self) -> Option<Seq> {
        self.steps.iter().find_map(|s| match s {
            Step::Payload { from, .. } => Some(*from),
            _ => None,
        })
    }

    /// The declared decision the proof rests on, if any.
    pub fn decision(&self) -> Option<Seq> {
        self.steps.iter().find_map(|s| match s {
            Step::Decision { seq, .. } => Some(*seq),
            _ => None,
        })
    }
}

/// A derivation that does not.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Refusal {
    pub venture: String,
    pub action: String,
    pub steps: Vec<Step>,
    pub reasons: Vec<Reason>,
}

/// A number that can be a Datalog fact (f64 is not hashable).
#[derive(Clone, Copy, Debug)]
struct Num(f64);

impl PartialEq for Num {
    fn eq(&self, other: &Num) -> bool {
        self.0.to_bits() == other.0.to_bits()
    }
}

impl Eq for Num {}

impl Hash for Num {
    fn hash<H: Hasher>(&self, h: &mut H) {
        self.0.to_bits().hash(h)
    }
}

/// The kind of a decision, as a fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Dk {
    Approve,
    Deny,
    Pause,
    Resume,
    Adopt,
}

ascent! {
    struct Rules;

    // What the ledger and the mandates say.
    relation candidate(String, String);
    relation cites(Seq);
    relation record(Seq, String, Claim, Kind);
    relation report(Seq, String, String, Num);
    relation decision(Seq, String, Dk, Seq);
    relation ask(Seq, String, String);
    relation asked(Seq, String);
    // an ask that code wrote, not a model
    relation code_ask(Seq);
    relation proposal(Seq, String, String);
    relation stopped(Seq, String);
    relation mandate(Seq, String, String);
    // venture, action, cap count, reversible, delegated
    relation permitted(String, String, u32, bool, bool);
    // venture, action: the lever consumes a payload
    relation takes(String, String);
    // venture, action, times run inside the cap window
    relation used(String, String, u32);
    relation forbidden(String, String);
    relation hardwired(String, String, String);
    // venture, metric, above, bound, action, the parent's action that set it
    relation threshold(String, String, bool, Num, String, Option<Seq>);
    // child, parent: both mandates say so
    relation child_of(String, String);
    // seq, parent, child: a parent pulled pause or resume
    relation parent_pause(Seq, String, String);
    relation parent_resume(Seq, String, String);
    // a venture whose mandate permits the parent lever `resume`
    relation parent_resume_lever(String);
    // venture, action: refused by its parent's budget, or a shared quota
    relation over_budget(String, String);
    relation over_quota(String, String);
    // a report code marked implausible
    relation implausible(Seq);
    // venture, guard metric, trips above, bound, trips at the bound too
    relation guard(String, String, bool, Num, bool);

    // Non-negotiable 2: only measured and declared records can authorize.
    relation trusted(Seq);
    trusted(s) <-- record(s, _, c, _), if c.authorizes();

    // Non-negotiable 5: stopping is always allowed; resuming needs a
    // declared record. A venture is paused after a declared pause or a
    // stop action, until a declared resume comes later.
    relation pause_at(String, Seq);
    pause_at(v, s) <-- decision(s, v, Dk::Pause, _), trusted(s);
    pause_at(v, s) <-- stopped(s, v);
    // Non-negotiable 10: a parent allocates. Its pause and resume reach a
    // child only when both mandates say they belong together.
    pause_at(c, s) <-- parent_pause(s, p, c), child_of(c, p);
    relation resume_at(String, Seq);
    resume_at(v, s) <-- decision(s, v, Dk::Resume, _), trusted(s);
    resume_at(c, s) <-- parent_resume(s, p, c), child_of(c, p);
    relation resumed_after(String, Seq);
    resumed_after(v, p) <-- pause_at(v, p), resume_at(v, r), if r > p;
    relation paused(String);
    paused(v) <-- pause_at(v, p), !resumed_after(v, p);

    // A report is stale once a trusted later report of the same metric exists.
    relation stale(Seq);
    stale(s) <-- report(s, v, m, _), report(t, v, m, _), trusted(t), if t > s;

    // What calls for the action. Each rests on trusted facts only; an
    // inferred record can be the occasion, never the premise.
    relation crossed(String, String, Seq);
    crossed(v, a, s) <--
        candidate(v, a),
        threshold(v, m, above, bound, a, _),
        report(s, v, m, val), cites(s), trusted(s), !stale(s),
        // A report code does not believe is never a premise.
        !implausible(s),
        if (*above && val.0 > bound.0) || (!*above && val.0 < bound.0);
    relation approved(String, String, Seq, Seq);
    approved(v, a, d, q) <--
        candidate(v, a),
        decision(d, v, Dk::Approve, q), cites(d), trusted(d),
        ask(q, v, a);
    relation proposed(String, String, Seq);
    proposed(v, a, s) <-- candidate(v, a), proposal(s, v, a), cites(s);

    relation justified(String, String);
    justified(v, a) <-- crossed(v, a, _);
    justified(v, a) <-- approved(v, a, _, _);
    justified(v, a) <-- proposed(v, a, _), permitted(v, a, _, rev, del), if *rev || *del;

    // Non-negotiable 6: irreversible actions default to asks. They pass
    // only when the mandate delegates them or a declared decision approves.
    relation irreversible_ok(String, String);
    irreversible_ok(v, a) <-- permitted(v, a, _, false, true);
    irreversible_ok(v, a) <-- permitted(v, a, _, false, _), approved(v, a, _, _);
    // venture, action: a measured ask says the lever is ineffective and no
    // declared decision has answered it
    relation suspended(String, String);
    relation needs_decision(String, String);
    // A suspended lever needs a declared decision for each run, whatever
    // its reversibility or delegation.
    needs_decision(v, a) <-- candidate(v, a), suspended(v, a), !approved(v, a, _, _);
    needs_decision(v, a) <-- candidate(v, a), permitted(v, a, _, false, _), !irreversible_ok(v, a);

    // Non-negotiable 9: content a model produced is inferred until a human
    // approves it. A lever that takes a payload runs from a declared
    // approval, from a proposal a human filed by hand, or when the mandate
    // delegates the lever; never from a model's proposal alone.
    relation declared_proposal(String, String, Seq);
    declared_proposal(v, a, s) <-- proposed(v, a, s), trusted(s);
    relation payload_ok(String, String);
    payload_ok(v, a) <-- takes(v, a), permitted(v, a, _, _, true);
    payload_ok(v, a) <-- takes(v, a), approved(v, a, _, _);
    payload_ok(v, a) <-- takes(v, a), declared_proposal(v, a, _);
    needs_decision(v, a) <-- candidate(v, a), takes(v, a), !payload_ok(v, a);
    // A parent's resume is declared-only: a human decides each time.
    needs_decision(v, a) <-- candidate(v, a), if a.as_str() == RESUME, parent_resume_lever(v), !approved(v, a, _, _);
    // Non-negotiable 12: self-modification is a proposal; only a declared
    // decision adopts one, and only on an ask that code wrote.
    needs_decision(v, a) <-- candidate(v, a), if a.as_str() == DELEGATE, !approved(v, a, _, _);

    // An ask stays open until a declared decision answers it.
    relation decided(Seq);
    decided(q) <-- decision(d, _, Dk::Approve, q), trusted(d);
    decided(q) <-- decision(d, _, Dk::Deny, q), trusted(d);
    relation open_ask(Seq, String);
    open_ask(q, v) <-- asked(q, v), !decided(q);

    // A guard's latest trusted reading past its bound guards the venture.
    // No score, approval or proposal gets a lever past a guard; only stop.
    relation breached(String, String, Seq);
    breached(v, g, s) <--
        guard(v, g, above, bound, at),
        report(s, v, g, val), trusted(s), !stale(s),
        if (*above && (val.0 > bound.0 || (*at && val.0 >= bound.0))) || (!*above && val.0 < bound.0);
    relation guarded(String);
    guarded(v) <-- breached(v, _, _);

    relation hard(String, String);
    hard(v, a) <-- hardwired(v, a, _);
    relation under_cap(String, String);
    under_cap(v, a) <-- permitted(v, a, cap, _, _), used(v, a, n), if n < cap;

    // What goes through the mandate: everything but stop and the venture's
    // own resume; a parent's resume lever is a lever like any other.
    relation plain(String, String);
    plain(v, a) <-- candidate(v, a), if a.as_str() != STOP && a.as_str() != RESUME && a.as_str() != DELEGATE;
    plain(v, a) <-- candidate(v, a), if a.as_str() == RESUME, parent_resume_lever(v);

    // Non-negotiable 1: no action without a mandate and a proof.
    relation allowed(String, String);
    allowed(v, a) <-- candidate(v, a), if a.as_str() == STOP;
    allowed(v, a) <--
        candidate(v, a), if a.as_str() == RESUME, !parent_resume_lever(v),
        decision(d, v, Dk::Resume, _), cites(d), trusted(d);
    allowed(v, a) <--
        candidate(v, a), if a.as_str() == DELEGATE,
        mandate(_, v, _),
        !hard(v, a),
        approved(v, a, _, q), code_ask(q),
        !guarded(v),
        !paused(v);
    allowed(v, a) <--
        plain(v, a),
        mandate(_, v, _),
        permitted(v, a, _, _, _),
        !forbidden(v, a),
        !hard(v, a),
        justified(v, a),
        !needs_decision(v, a),
        under_cap(v, a),
        !over_budget(v, a),
        !over_quota(v, a),
        !guarded(v),
        !paused(v);
}

/// The current facts: (seq, venture, topic), oldest first. Every report is
/// a fact about a topic. A measured or declared fact is superseded by a
/// later measured or declared fact on the same topic; any fact is
/// superseded by a later one on the same topic that cites it. The rest is
/// current.
pub fn current_facts(ledger: &[Entry]) -> Vec<(Seq, String, String)> {
    let mut facts: HashMap<Seq, (&str, &str, bool)> = HashMap::new();
    let mut latest: HashMap<(&str, &str), Seq> = HashMap::new();
    for e in ledger {
        if let Record::Report {
            claim,
            venture,
            metric,
            ..
        } = &e.record
        {
            facts.insert(e.seq, (venture, metric, claim.authorizes()));
            if claim.authorizes() {
                let l = latest.entry((venture, metric)).or_insert(e.seq);
                *l = (*l).max(e.seq);
            }
        }
    }
    let mut superseded: HashSet<Seq> = HashSet::new();
    for e in ledger {
        if let Record::Report {
            venture,
            metric,
            cites,
            ..
        } = &e.record
        {
            for c in cites {
                let same = facts
                    .get(c)
                    .is_some_and(|(v, t, _)| v == venture && t == metric);
                if *c < e.seq && same {
                    superseded.insert(*c);
                }
            }
        }
    }
    let mut out = Vec::new();
    for (&s, &(v, t, trusted)) in &facts {
        let later = trusted && latest.get(&(v, t)).is_some_and(|l| *l > s);
        if !later && !superseded.contains(&s) {
            out.push((s, v.to_string(), t.to_string()));
        }
    }
    out.sort_unstable();
    out
}

/// A threshold as it stands for a venture: the mandate's, or the one its
/// parent set.
#[derive(Clone, Debug, PartialEq)]
pub struct Threshold {
    pub above: bool,
    pub bound: f64,
    pub action: String,
    /// The parent's action that set it, when a parent did.
    pub from: Option<Seq>,
}

/// Does both mandates' word make `child` a child of `parent`?
fn is_child_of(mandates: &[&Adopted], child: &str, parent: &str) -> bool {
    mandates
        .iter()
        .any(|m| m.venture == parent && m.mandate.child(child).is_some())
        && mandates
            .iter()
            .any(|m| m.venture == child && m.mandate.parent.as_deref() == Some(parent))
}

/// The levers `child`'s parent pulled on it, the latest of each, when both
/// mandates say they belong together.
fn pulled_by_parent<'a>(
    index: &'a Index,
    mandates: &[&Adopted],
    child: &str,
) -> Option<&'a Pulled> {
    let parent = mandates
        .iter()
        .find(|m| m.venture == child)
        .and_then(|m| m.mandate.parent.clone())?;
    if !is_child_of(mandates, child, &parent) {
        return None;
    }
    index.pulled(child, &parent)
}

/// The threshold a venture's controller starts from, before its parent
/// moves it.
fn mandate_threshold(mandates: &[&Adopted], venture: &str) -> Option<Threshold> {
    let m = mandates.iter().find(|m| m.venture == venture)?;
    let Controller::Threshold {
        above,
        below,
        action,
        ..
    } = &m.mandate.controller
    else {
        return None;
    };
    match (above, below) {
        (Some(b), _) => Some(Threshold {
            above: true,
            bound: *b,
            action: action.clone(),
            from: None,
        }),
        (_, Some(b)) => Some(Threshold {
            above: false,
            bound: *b,
            action: action.clone(),
            from: None,
        }),
        _ => None,
    }
}

/// The threshold that applies to a venture now, if its controller is one.
pub fn effective_threshold(
    ledger: &[Entry],
    mandates: &[Adopted],
    venture: &str,
) -> Option<Threshold> {
    effective_threshold_in(&Index::from_entries(ledger), mandates, venture)
}

/// The threshold that applies to a venture now, from the index.
pub fn effective_threshold_in(
    index: &Index,
    mandates: &[Adopted],
    venture: &str,
) -> Option<Threshold> {
    let refs: Vec<&Adopted> = mandates.iter().collect();
    let mut t = mandate_threshold(&refs, venture)?;
    if let Some((seq, above, bound)) =
        pulled_by_parent(index, &refs, venture).and_then(|p| p.threshold)
    {
        t.above = above;
        t.bound = bound;
        t.from = Some(seq);
    }
    Some(t)
}

/// The budget a venture's parent set for it, and the action that set it.
pub fn budget_of(ledger: &[Entry], mandates: &[&Adopted], venture: &str) -> Option<(Seq, u32)> {
    budget_of_in(&Index::from_entries(ledger), mandates, venture)
}

/// The budget a venture's parent set for it, from the index.
pub fn budget_of_in(index: &Index, mandates: &[&Adopted], venture: &str) -> Option<(Seq, u32)> {
    pulled_by_parent(index, mandates, venture).and_then(|p| p.budget)
}

/// The cost of an action a venture took, by its mandate as adopted now.
fn cost_of(mandates: &[&Adopted], venture: &str, action: &str) -> u32 {
    mandates
        .iter()
        .find(|m| m.venture == venture)
        .and_then(|m| m.mandate.permitted(action))
        .map_or(0, |p| p.cost)
}

/// What a venture spent today.
pub fn spent_today(ledger: &[Entry], mandates: &[&Adopted], venture: &str, now: i64) -> u32 {
    spent_today_in(&Index::from_entries(ledger), mandates, venture, now)
}

/// What a venture spent today, from the index.
pub fn spent_today_in(index: &Index, mandates: &[&Adopted], venture: &str, now: i64) -> u32 {
    index
        .runs_of_after(venture, now - Cap::DAY.as_secs() as i64)
        .iter()
        .map(|(_, a)| cost_of(mandates, venture, a))
        .sum()
}

/// How the shared quotas bear on an action: the steps that show them, and
/// the reasons when it would pass one. The kernel asks this before the
/// gate; the gate asks it again.
pub fn quota_check(
    ledger: &[Entry],
    mandates: &[Adopted],
    quotas: &Quotas,
    venture: &str,
    action: &str,
    now: i64,
) -> (Vec<Step>, Vec<Reason>) {
    quota_check_in(
        &Index::from_entries(ledger),
        mandates,
        quotas,
        venture,
        action,
        now,
    )
}

/// How the shared quotas bear on an action, from the index.
pub fn quota_check_in(
    index: &Index,
    mandates: &[Adopted],
    quotas: &Quotas,
    venture: &str,
    action: &str,
    now: i64,
) -> (Vec<Step>, Vec<Reason>) {
    let refs: Vec<&Adopted> = mandates.iter().collect();
    let runs = |per: Duration| {
        index
            .runs_after(now - per.as_secs() as i64)
            .iter()
            .map(|(_, v, a)| (v.as_str(), a.as_str()))
    };
    quota_steps(&refs, quotas, venture, action, runs)
}

/// The quota steps and reasons, over the runs inside each quota's window.
fn quota_steps<'a, I>(
    refs: &[&Adopted],
    quotas: &Quotas,
    venture: &str,
    action: &str,
    runs: impl Fn(Duration) -> I,
) -> (Vec<Step>, Vec<Reason>)
where
    I: Iterator<Item = (&'a str, &'a str)>,
{
    let mut steps = Vec::new();
    let mut reasons = Vec::new();
    let Some(perm) = refs
        .iter()
        .find(|m| m.venture == venture)
        .and_then(|m| m.mandate.permitted(action))
    else {
        return (steps, reasons);
    };
    if let (Some(cap), true) = (quotas.spend, perm.cost > 0) {
        let used: u32 = runs(cap.per).map(|(v, a)| cost_of(refs, v, a)).sum();
        steps.push(Step::Quota {
            what: "spend".to_string(),
            used,
            cap,
        });
        if used + perm.cost > cap.count {
            reasons.push(Reason::OverQuota {
                what: "spend".to_string(),
                used,
                cap,
            });
        }
    }
    if let (Some(cap), true) = (quotas.sends, perm.sends()) {
        let used = runs(cap.per)
            .filter(|(v, a)| {
                refs.iter()
                    .find(|m| m.venture == *v)
                    .and_then(|m| m.mandate.permitted(a))
                    .is_some_and(Permitted::sends)
            })
            .count() as u32;
        steps.push(Step::Quota {
            what: "sends".to_string(),
            used,
            cap,
        });
        if used >= cap.count {
            reasons.push(Reason::OverQuota {
                what: "sends".to_string(),
                used,
                cap,
            });
        }
    }
    (steps, reasons)
}

/// How many asks were written inside the asks quota's window.
pub fn asks_used(ledger: &[Entry], cap: Cap, now: i64) -> u32 {
    asks_used_in(&Index::from_entries(ledger), cap, now)
}

/// How many asks were written inside the asks quota's window, from the
/// index.
pub fn asks_used_in(index: &Index, cap: Cap, now: i64) -> u32 {
    index.asks_after(now - cap.per.as_secs() as i64)
}

/// Decide whether an action may happen, given the ledger so far and the
/// adopted mandates, with no shared quotas. `now` is unix seconds, used
/// for caps.
pub fn gate(
    candidate: &Candidate,
    ledger: &[Entry],
    mandates: &[Adopted],
    now: i64,
) -> Result<Proof, Refusal> {
    gate_with(candidate, ledger, mandates, &Quotas::default(), now)
}

/// Decide whether an action may happen, under the shared quotas too.
pub fn gate_with(
    candidate: &Candidate,
    ledger: &[Entry],
    mandates: &[Adopted],
    quotas: &Quotas,
    now: i64,
) -> Result<Proof, Refusal> {
    gate_in(
        &Index::from_entries(ledger),
        candidate,
        mandates,
        quotas,
        now,
    )
}

/// Decide whether an action may happen, from the index. The rules see the
/// records the candidate can rest on and no others: what it cites, the
/// asks the cited decisions answer, the latest trusted report of each
/// cited metric, and what pauses or resumes the venture. The rest of the
/// ledger cannot change what they derive for this candidate.
pub fn gate_in(
    index: &Index,
    candidate: &Candidate,
    mandates: &[Adopted],
    quotas: &Quotas,
    now: i64,
) -> Result<Proof, Refusal> {
    let v = candidate.venture.as_str();
    let a = candidate.action.as_str();
    let mut p = Rules::default();
    p.candidate.push((v.to_string(), a.to_string()));
    p.cites = candidate.cites.iter().map(|s| (*s,)).collect();
    let mut seqs = BTreeSet::new();
    for &s in &candidate.cites {
        let Some(e) = index.get(s) else { continue };
        seqs.insert(s);
        match &e.record {
            Record::Decision {
                verdict: Verdict::Approve { ask, .. } | Verdict::Deny { ask, .. },
                ..
            } => {
                seqs.insert(*ask);
            }
            Record::Report {
                venture,
                metric,
                value: Some(_),
                ..
            } => seqs.extend(index.latest_report(venture, metric)),
            _ => {}
        }
    }
    seqs.extend(index.pause_sources(v));
    // The latest reading of each of the venture's guards.
    for m in mandates.iter().filter(|m| m.venture == v) {
        for g in &m.mandate.guards {
            seqs.extend(index.latest_report(v, &g.metric()));
        }
    }
    for s in seqs {
        if let Some(e) = index.get(s) {
            load_entry(&mut p, e);
            load_call(&mut p, e, &|s| index.done(s));
        }
    }

    // Only mandates the ledger shows adopted by a declared decision count.
    let adopted: Vec<&Adopted> = mandates
        .iter()
        .filter(|m| index.get(m.seq).is_some_and(|e| adopts(e, m)))
        .collect();
    let pending = index.pending_of(v, a);
    load_mandates(
        &mut p,
        &adopted,
        now,
        |venture, action, since| index.used(venture, action, since),
        |venture| effective_threshold_in(index, mandates, venture),
        |venture, action| index.suspended(venture, action),
    );
    load_family_mandates(&mut p, &adopted);
    let budget = budget_of_in(index, &adopted, v);
    let spent = || spent_today_in(index, &adopted, v, now);
    let quota = quota_check_in(index, mandates, quotas, v, a, now);
    let (extra_steps, mut extra_reasons) = limits(&mut p, v, a, &adopted, budget, spent, quota);
    p.run();
    extra_reasons.extend(allocation_reasons(
        &p,
        &|s| index.get(s),
        (v, a),
        &adopted,
        &|child| index.budgets(child, v).to_vec(),
        now,
    ));
    assemble(
        &p,
        candidate,
        &|s| index.get(s),
        &adopted,
        pending,
        extra_steps,
        extra_reasons,
    )
}

/// Is this entry the declared decision that adopted this mandate?
fn adopts(e: &Entry, m: &Adopted) -> bool {
    matches!(&e.record,
        Record::Decision { claim: Claim::Declared, venture, verdict: Verdict::Adopt { mandate, .. }, .. }
        if *venture == m.venture && *mandate == m.hash)
}

/// The adopted mandates as facts: what each permits and forbids, how much
/// of each cap is used, the thresholds, and what self may never do.
fn load_mandates(
    p: &mut Rules,
    adopted: &[&Adopted],
    now: i64,
    used: impl Fn(&str, &str, i64) -> u32,
    threshold: impl Fn(&str) -> Option<Threshold>,
    suspended: impl Fn(&str, &str) -> bool,
) {
    for m in adopted {
        let venture = m.venture.clone();
        p.mandate.push((m.seq, venture.clone(), m.hash.clone()));
        for perm in &m.mandate.permitted {
            p.permitted.push((
                venture.clone(),
                perm.name.clone(),
                perm.cap.count,
                perm.reversible,
                perm.delegated,
            ));
            if suspended(&venture, &perm.name) {
                p.suspended.push((venture.clone(), perm.name.clone()));
            }
            let window = perm.cap.per.as_secs() as i64;
            let n = used(&venture, &perm.name, now - window);
            p.used.push((venture.clone(), perm.name.clone(), n));
            if perm.takes().is_some() {
                p.takes.push((venture.clone(), perm.name.clone()));
            }
            if venture == SELF {
                for (fragment, why) in HARDWIRED_RUN {
                    let command = perm
                        .run
                        .as_deref()
                        .or(perm.adapter.as_deref())
                        .unwrap_or("");
                    if command.contains(fragment) {
                        p.hardwired
                            .push((venture.clone(), perm.name.clone(), why.to_string()));
                    }
                }
            }
        }
        for f in &m.mandate.forbidden {
            p.forbidden.push((venture.clone(), f.clone()));
        }
        for g in &m.mandate.guards {
            let (above, bound) = g.bound();
            p.guard.push((
                venture.clone(),
                g.metric(),
                above,
                Num(bound),
                g.drawdown.is_some(),
            ));
        }
        if let Some(t) = threshold(&venture) {
            p.threshold.push((
                venture.clone(),
                m.mandate.metric_name().to_string(),
                t.above,
                Num(t.bound),
                t.action,
                t.from,
            ));
        }
    }
    for (name, why) in HARDWIRED {
        p.hardwired
            .push((SELF.to_string(), name.to_string(), why.to_string()));
    }
}

/// The parent's budget and the shared quotas, as facts and as steps.
fn limits(
    p: &mut Rules,
    v: &str,
    a: &str,
    adopted: &[&Adopted],
    budget: Option<(Seq, u32)>,
    spent: impl Fn() -> u32,
    (qsteps, qreasons): (Vec<Step>, Vec<Reason>),
) -> (Vec<Step>, Vec<Reason>) {
    let mut extra_steps = Vec::new();
    let mut extra_reasons = Vec::new();
    let cost = adopted
        .iter()
        .find(|m| m.venture == v)
        .and_then(|m| m.mandate.permitted(a))
        .map_or(0, |perm| perm.cost);
    if let Some((from, budget)) = budget {
        let spent = spent();
        extra_steps.push(Step::Budget {
            from,
            spent,
            budget,
        });
        if spent + cost > budget {
            p.over_budget.push((v.to_string(), a.to_string()));
            extra_reasons.push(Reason::OverBudget {
                spent,
                cost,
                budget,
            });
        }
    }
    if !qreasons.is_empty() {
        p.over_quota.push((v.to_string(), a.to_string()));
    }
    extra_steps.extend(qsteps);
    extra_reasons.extend(qreasons);
    (extra_steps, extra_reasons)
}

/// Parents and children as facts: who belongs to whom, and which parents
/// may resume.
fn load_family_mandates(p: &mut Rules, adopted: &[&Adopted]) {
    for m in adopted {
        for c in m.mandate.children.iter().map(|c| &c.name) {
            if is_child_of(adopted, c, &m.venture) {
                p.child_of.push((c.clone(), m.venture.clone()));
            }
        }
        if m.mandate
            .permitted(RESUME)
            .is_some_and(Permitted::is_parent_lever)
        {
            p.parent_resume_lever.push((m.venture.clone(),));
        }
    }
}

/// A pause or resume a parent pulled, as a fact: an ok action, as a 1.0
/// ledger has it, or an intent that `done` says has an ok outcome.
fn load_call(p: &mut Rules, e: &Entry, done: &dyn Fn(Seq) -> bool) {
    let (venture, action, text) = match &e.record {
        Record::Action {
            venture,
            action,
            ok: true,
            payload: Some(Payload::Text { text }),
            ..
        } => (venture, action, text),
        Record::Intent {
            venture,
            action,
            payload: Some(Payload::Text { text }),
            ..
        } if done(e.seq) => (venture, action, text),
        _ => return,
    };
    {
        match ParentCall::parse(action, text) {
            Ok(ParentCall::Pause { child }) => p.parent_pause.push((e.seq, venture.clone(), child)),
            Ok(ParentCall::Resume { child }) => {
                p.parent_resume.push((e.seq, venture.clone(), child))
            }
            _ => {}
        }
    }
}

/// One ledger entry as facts.
fn load_entry(p: &mut Rules, e: &Entry) {
    let r = &e.record;
    let venture = r.venture().to_string();
    p.record.push((e.seq, venture.clone(), r.claim(), r.kind()));
    if r.implausible().is_some() {
        p.implausible.push((e.seq,));
    }
    match r {
        Record::Report {
            metric,
            value: Some(val),
            ..
        } => p.report.push((e.seq, venture, metric.clone(), Num(*val))),
        Record::Decision { verdict, .. } => {
            let (dk, target) = match verdict {
                Verdict::Approve { ask, .. } => (Dk::Approve, *ask),
                Verdict::Deny { ask, .. } => (Dk::Deny, *ask),
                Verdict::Pause => (Dk::Pause, 0),
                Verdict::Resume => (Dk::Resume, 0),
                Verdict::Adopt { .. } => (Dk::Adopt, 0),
            };
            p.decision.push((e.seq, venture, dk, target));
        }
        Record::Ask { action, claim, .. } => {
            p.asked.push((e.seq, venture.clone()));
            if *claim == Claim::Measured {
                p.code_ask.push((e.seq,));
            }
            if let Some(act) = action {
                p.ask.push((e.seq, venture, act.clone()));
            }
        }
        Record::Proposal { action, .. } => p.proposal.push((e.seq, venture, action.clone())),
        Record::Action { action, .. } if action == STOP => p.stopped.push((e.seq, venture)),
        _ => {}
    }
}

/// What a venture is doing, as the ledger tells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Running,
    Asking,
    Paused,
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            State::Running => "running",
            State::Asking => "asking",
            State::Paused => "paused",
        })
    }
}

/// The state of a venture: paused after a declared pause, a stop action
/// or its parent's pause, until a declared resume; asking while an ask has
/// no declared decision; running otherwise.
pub fn state(ledger: &[Entry], mandates: &[Adopted], venture: &str) -> State {
    state_in(&Index::from_entries(ledger), mandates, venture)
}

/// The state of a venture, from the index. The rules see what pauses and
/// resumes it, the latest of each source, and decide whether it is paused.
pub fn state_in(index: &Index, mandates: &[Adopted], venture: &str) -> State {
    let mut p = Rules::default();
    for s in index.pause_sources(venture) {
        if let Some(e) = index.get(s) {
            load_entry(&mut p, e);
            load_call(&mut p, e, &|s| index.done(s));
        }
    }
    let adopted: Vec<&Adopted> = mandates.iter().collect();
    load_family_mandates(&mut p, &adopted);
    p.run();
    if p.paused.iter().any(|(v,)| v == venture) {
        State::Paused
    } else if index.open_of(venture).next().is_some() {
        State::Asking
    } else {
        State::Running
    }
}

/// Asks with no declared decision yet, oldest first.
pub fn open_asks(ledger: &[Entry]) -> Vec<Seq> {
    Index::from_entries(ledger).open_asks()
}

/// Turn the derived facts into a proof or a refusal that reads top to bottom.
fn assemble<'a>(
    p: &Rules,
    candidate: &Candidate,
    find: &dyn Fn(Seq) -> Option<&'a Entry>,
    adopted: &[&Adopted],
    pending: Option<Seq>,
    extra_steps: Vec<Step>,
    extra_reasons: Vec<Reason>,
) -> Result<Proof, Refusal> {
    let v = &candidate.venture;
    let a = &candidate.action;
    let allowed = p.allowed.iter().any(|(x, y)| x == v && y == a);
    let mut steps = Vec::new();
    let mut reasons = Vec::new();
    let occasion = |e: &Entry| Step::Occasion {
        seq: e.seq,
        claim: e.record.claim(),
        kind: e.record.kind(),
        what: e.record.summary(),
    };

    if a == STOP {
        steps.push(Step::Axiom {
            text: "stopping is always allowed".to_string(),
        });
        for &s in &candidate.cites {
            if let Some(e) = find(s) {
                steps.push(occasion(e));
            }
        }
        return Ok(Proof {
            venture: v.clone(),
            action: a.clone(),
            steps,
        });
    }

    // A run that may or may not have happened is settled before the lever
    // runs again: by an adapter that can check, or by a human.
    if let Some(intent) = pending {
        reasons.push(Reason::Pending { intent });
    }
    for &s in &candidate.cites {
        if find(s).is_none() {
            reasons.push(Reason::UnknownRecord { seq: s });
        }
    }

    if a == DELEGATE {
        steps.push(Step::Axiom {
            text: "a changed mandate is adopted only by a declared decision".to_string(),
        });
        if !adopted.iter().any(|m| m.venture == *v) {
            reasons.push(Reason::NoMandate { venture: v.clone() });
        }
        for (_, _, why) in p.hardwired.iter().filter(|(x, y, _)| x == v && y == a) {
            reasons.push(Reason::Hardwired {
                action: a.clone(),
                why: why.clone(),
            });
        }
        let mut approved_by_code = false;
        for (_, _, d, q) in p.approved.iter().filter(|(x, y, _, _)| x == v && y == a) {
            let by = find(*d)
                .and_then(|e| match &e.record {
                    Record::Decision { by: Some(who), .. } => Some(format!("  by {who}")),
                    _ => None,
                })
                .unwrap_or_default();
            steps.push(Step::Decision {
                seq: *d,
                what: format!("approved ask #{q}{by}"),
            });
            if let Some(e) = find(*q) {
                steps.push(occasion(e));
                approved_by_code |= e.record.claim() == Claim::Measured;
                if let Some((from, payload)) = find(*d)
                    .filter(|e| e.record.payload().is_some())
                    .or(Some(e))
                    .and_then(|e| e.record.payload().map(|pl| (e.seq, pl)))
                {
                    steps.push(Step::Payload {
                        from,
                        claim: find(from).map_or(Claim::Measured, |e| e.record.claim()),
                        kind: payload.takes(),
                        summary: payload.summary(),
                    });
                }
            }
        }
        for &s in &candidate.cites {
            if let Some(e) = find(s) {
                if !p
                    .approved
                    .iter()
                    .any(|(x, y, d, _)| x == v && y == a && *d == s)
                {
                    steps.push(occasion(e));
                }
            }
        }
        if p.needs_decision.iter().any(|(x, y)| x == v && y == a) {
            reasons.push(Reason::NeedsDecision {
                action: a.clone(),
                suspended: p.suspended.iter().any(|(x, y)| x == v && y == a),
            });
        } else if !approved_by_code {
            reasons.push(Reason::NotJustified { action: a.clone() });
        }
        reasons.extend(guarded(p, v, find));
        if p.paused.iter().any(|(x,)| x == v) {
            reasons.push(Reason::Paused { venture: v.clone() });
        }
        return finish(allowed, v, a, steps, reasons);
    }

    let parent_resume = p.parent_resume_lever.iter().any(|(x,)| x == v);
    if a == RESUME && !parent_resume {
        steps.push(Step::Axiom {
            text: "resuming needs a declared record".to_string(),
        });
        let mut found = false;
        for &s in &candidate.cites {
            if let Some(e) = find(s) {
                let declared_resume = matches!(&e.record,
                    Record::Decision { claim: Claim::Declared, venture, verdict: Verdict::Resume, .. }
                    if venture == v);
                if declared_resume {
                    let by = match &e.record {
                        Record::Decision { by: Some(who), .. } => format!("  by {who}"),
                        _ => String::new(),
                    };
                    steps.push(Step::Decision {
                        seq: e.seq,
                        what: format!("resumed{by}"),
                    });
                    found = true;
                } else {
                    steps.push(occasion(e));
                }
            }
        }
        if !found {
            reasons.push(Reason::NeedsDeclaredResume { venture: v.clone() });
        }
        return finish(allowed, v, a, steps, reasons);
    }

    let mandate = adopted.iter().find(|m| m.venture == *v);
    let permitted = mandate.and_then(|m| m.mandate.permitted(a));
    match mandate {
        Some(m) => {
            steps.push(Step::Mandate {
                seq: m.seq,
                hash: m.hash.clone(),
            });
            match permitted {
                Some(perm) => steps.push(Step::Permitted {
                    action: perm.name.clone(),
                    cap: perm.cap,
                    reversible: perm.reversible,
                    delegated: perm.delegated,
                    takes: perm.takes(),
                    cost: perm.cost,
                }),
                None => reasons.push(Reason::NotPermitted { action: a.clone() }),
            }
            if m.mandate.forbidden.iter().any(|f| f == a) {
                reasons.push(Reason::Forbidden { action: a.clone() });
            }
        }
        None => reasons.push(Reason::NoMandate { venture: v.clone() }),
    }
    let mut whys = BTreeSet::new();
    for (_, _, why) in p.hardwired.iter().filter(|(x, y, _)| x == v && y == a) {
        if whys.insert(why.clone()) {
            reasons.push(Reason::Hardwired {
                action: a.clone(),
                why: why.clone(),
            });
        }
    }

    let mut cited = BTreeSet::new();
    for (_, _, s) in p.crossed.iter().filter(|(x, y, _)| x == v && y == a) {
        if let Some(e) = find(*s) {
            if let Record::Report {
                metric,
                value: Some(value),
                ..
            } = &e.record
            {
                steps.push(Step::Report {
                    seq: e.seq,
                    claim: e.record.claim(),
                    metric: metric.clone(),
                    value: *value,
                });
                if let Some((_, _, above, bound, _, from)) = p
                    .threshold
                    .iter()
                    .find(|(x, m, _, _, act, _)| x == v && m == metric && act == a)
                {
                    steps.push(Step::Threshold {
                        metric: metric.clone(),
                        bound: bound.0,
                        above: *above,
                        from: *from,
                    });
                }
            }
            cited.insert(*s);
        }
    }
    for (_, _, d, q) in p.approved.iter().filter(|(x, y, _, _)| x == v && y == a) {
        let by = find(*d)
            .and_then(|e| match &e.record {
                Record::Decision { by: Some(who), .. } => Some(format!("  by {who}")),
                _ => None,
            })
            .unwrap_or_default();
        steps.push(Step::Decision {
            seq: *d,
            what: format!("approved ask #{q}{by}"),
        });
        cited.insert(*d);
        if let Some(e) = find(*q) {
            steps.push(occasion(e));
            cited.insert(*q);
        }
    }
    for (_, _, s) in p.proposed.iter().filter(|(x, y, _)| x == v && y == a) {
        if let Some(e) = find(*s) {
            steps.push(occasion(e));
            cited.insert(*s);
        }
    }
    for &s in &candidate.cites {
        if !cited.contains(&s) {
            if let Some(e) = find(s) {
                steps.push(occasion(e));
                cited.insert(s);
            }
        }
    }

    // The payload a lever consumes comes from the amended approval if the
    // human changed it, else from the ask the approval answers, else from
    // the proposal. A lever that takes one and gets none is refused.
    if let Some(takes) = permitted.and_then(Permitted::takes) {
        let source = payload_entry(p, find, v, a);
        match source.and_then(|e| e.record.payload().map(|pl| (e, pl))) {
            Some((e, payload)) => {
                steps.push(Step::Payload {
                    from: e.seq,
                    claim: e.record.claim(),
                    kind: payload.takes(),
                    summary: payload.summary(),
                });
                if payload.takes() != takes {
                    reasons.push(Reason::WrongPayload {
                        action: a.clone(),
                        takes,
                        found: payload.takes(),
                    });
                }
            }
            None => reasons.push(Reason::NoPayload {
                action: a.clone(),
                takes,
            }),
        }
    }

    let justified = p.justified.iter().any(|(x, y)| x == v && y == a);
    if !justified {
        let inferred: Vec<Seq> = candidate
            .cites
            .iter()
            .copied()
            .filter(|s| find(*s).is_some_and(|e| !e.record.claim().authorizes()))
            .collect();
        if !inferred.is_empty() && inferred.len() == candidate.cites.len() {
            reasons.push(Reason::OnlyInferred { seqs: inferred });
        }
        reasons.push(Reason::NotJustified { action: a.clone() });
    }
    if p.needs_decision.iter().any(|(x, y)| x == v && y == a) {
        reasons.push(Reason::NeedsDecision {
            action: a.clone(),
            suspended: p.suspended.iter().any(|(x, y)| x == v && y == a),
        });
    }
    if let Some(perm) = permitted {
        let used = p
            .used
            .iter()
            .find(|(x, y, _)| x == v && y == a)
            .map_or(0, |(_, _, n)| *n);
        steps.push(Step::Cap {
            used,
            cap: perm.cap,
        });
        if used >= perm.cap.count {
            reasons.push(Reason::OverCap {
                used,
                cap: perm.cap,
            });
        }
    }
    steps.extend(extra_steps);
    reasons.extend(extra_reasons);
    reasons.extend(guarded(p, v, find));
    if p.paused.iter().any(|(x,)| x == v) {
        reasons.push(Reason::Paused { venture: v.clone() });
    } else {
        steps.push(Step::Running { venture: v.clone() });
    }
    finish(allowed, v, a, steps, reasons)
}

/// The record a lever's payload comes from: the amended approval if the
/// human changed it, else the ask the approval answers, else the proposal.
fn payload_entry<'a>(
    p: &Rules,
    find: &dyn Fn(Seq) -> Option<&'a Entry>,
    v: &str,
    a: &str,
) -> Option<&'a Entry> {
    let with_payload = |seq: Seq| find(seq).filter(|e| e.record.payload().is_some());
    let mut source = None;
    for (_, _, d, q) in p.approved.iter().filter(|(x, y, _, _)| x == v && y == a) {
        source = with_payload(*d).or_else(|| with_payload(*q));
        if source.is_some() {
            break;
        }
    }
    if source.is_none() {
        source = p
            .proposed
            .iter()
            .filter(|(x, y, _)| x == v && y == a)
            .find_map(|(_, _, s)| with_payload(*s));
    }
    source
}

/// A budget a parent set: the record, when, and the amount.
type Budgeted = (Seq, i64, u32);

/// What a parent lever, about to be pulled, would break: a venture that is
/// not the parent's child, a budget under a floor, a cut that spares a
/// lower-priority child above its floor, a change inside the deadband, a
/// reversal inside the hold. These hold whatever the proof rests on,
/// declared approvals included. `budgets` lists what the parent has set for
/// a child: seq, when, amount; oldest first.
fn allocation_reasons<'a>(
    p: &Rules,
    find: &dyn Fn(Seq) -> Option<&'a Entry>,
    candidate: (&str, &str),
    adopted: &[&Adopted],
    budgets: &dyn Fn(&str) -> Vec<Budgeted>,
    now: i64,
) -> Vec<Reason> {
    let (v, a) = candidate;
    let Some(parent) = adopted.iter().find(|m| m.venture == v) else {
        return vec![];
    };
    if !parent
        .mandate
        .permitted(a)
        .is_some_and(Permitted::is_parent_lever)
    {
        return vec![];
    }
    let Some(Payload::Text { text }) =
        payload_entry(p, find, v, a).and_then(|e| e.record.payload())
    else {
        return vec![];
    };
    let Ok(call) = ParentCall::parse(a, text) else {
        return vec![];
    };
    let child = call.child();
    if !is_child_of(adopted, child, v) {
        return vec![Reason::NotAChild {
            venture: v.to_string(),
            child: child.to_string(),
        }];
    }
    let ParentCall::Budget { amount, .. } = call else {
        return vec![];
    };
    let mut reasons = Vec::new();
    let Some(terms) = parent.mandate.child(child) else {
        return reasons;
    };
    if amount < terms.floor {
        reasons.push(Reason::BelowFloor {
            child: child.to_string(),
            budget: amount,
            floor: terms.floor,
        });
    }
    let history = budgets(child);
    let Some(&(last, last_at, current)) = history.last() else {
        return reasons;
    };
    if amount < current {
        // A cut: not while a lower-priority child sits above its floor.
        for other in &parent.mandate.children {
            if other.name == child
                || other.priority >= terms.priority
                || !is_child_of(adopted, &other.name, v)
            {
                continue;
            }
            if let Some(&(_, _, theirs)) = budgets(&other.name).last() {
                if theirs > other.floor {
                    reasons.push(Reason::Priority {
                        child: child.to_string(),
                        spared: other.name.clone(),
                        budget: theirs,
                        floor: other.floor,
                    });
                    break;
                }
            }
        }
    }
    if let Some(alloc) = &parent.mandate.allocation {
        let change = amount.abs_diff(current);
        if change < alloc.deadband {
            reasons.push(Reason::InDeadband {
                child: child.to_string(),
                change,
                deadband: alloc.deadband,
            });
        }
        let before = history.len().checked_sub(2).map(|i| history[i].2);
        if let Some(before) = before {
            let went = current.cmp(&before);
            let goes = amount.cmp(&current);
            let inside = now - last_at < alloc.hold.as_secs() as i64;
            if inside && went.is_ne() && goes.is_ne() && went != goes {
                reasons.push(Reason::Reversal {
                    child: child.to_string(),
                    last,
                    hold: alloc.hold,
                });
            }
        }
    }
    reasons
}

/// The guards that hold a venture, as reasons.
fn guarded<'a>(p: &Rules, v: &str, find: &dyn Fn(Seq) -> Option<&'a Entry>) -> Vec<Reason> {
    let mut out: Vec<Reason> = p
        .breached
        .iter()
        .filter(|(x, _, _)| x == v)
        .filter_map(|(_, g, s)| {
            let value = match find(*s).map(|e| &e.record) {
                Some(Record::Report {
                    value: Some(val), ..
                }) => *val,
                _ => return None,
            };
            let (_, _, above, bound, _) = p.guard.iter().find(|(x, m, ..)| x == v && m == g)?;
            Some(Reason::Guarded {
                guard: g.strip_prefix("guard.").unwrap_or(g).to_string(),
                value,
                above: *above,
                bound: bound.0,
            })
        })
        .collect();
    out.sort_by_key(|r| r.to_string());
    out
}

fn finish(
    allowed: bool,
    venture: &str,
    action: &str,
    steps: Vec<Step>,
    mut reasons: Vec<Reason>,
) -> Result<Proof, Refusal> {
    if allowed && reasons.is_empty() {
        return Ok(Proof {
            venture: venture.to_string(),
            action: action.to_string(),
            steps,
        });
    }
    if reasons.is_empty() {
        reasons.push(Reason::NotJustified {
            action: action.to_string(),
        });
    }
    Err(Refusal {
        venture: venture.to_string(),
        action: action.to_string(),
        steps,
        reasons,
    })
}

fn short(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::Mandate { seq, hash } => {
                write!(
                    f,
                    "mandate    #{seq}  declared  adopted mandate {}",
                    short(hash)
                )
            }
            Step::Permitted {
                action,
                cap,
                reversible,
                delegated,
                takes,
                cost,
            } => write!(
                f,
                "permitted  {action}  {cap}  {}{}{}{}",
                if *reversible {
                    "reversible"
                } else {
                    "irreversible"
                },
                if *delegated { ", delegated" } else { "" },
                match takes {
                    Some(t) => format!(", takes {t}"),
                    None => String::new(),
                },
                if *cost > 0 {
                    format!(", costs {cost}")
                } else {
                    String::new()
                }
            ),
            Step::Payload {
                from,
                claim,
                kind: _,
                summary,
            } => write!(f, "payload    #{from}  {claim}  {summary}"),
            Step::Report {
                seq,
                claim,
                metric,
                value,
            } => write!(f, "report     #{seq}  {claim}  {metric}={value}"),
            Step::Threshold {
                metric,
                bound,
                above,
                from,
            } => write!(
                f,
                "threshold  {metric} {} {bound}{}",
                if *above { "above" } else { "below" },
                match from {
                    Some(s) => format!("  set by the parent in #{s}"),
                    None => String::new(),
                }
            ),
            Step::Budget {
                from,
                spent,
                budget,
            } => write!(
                f,
                "budget     {spent} of {budget} a day spent  set by the parent in #{from}"
            ),
            Step::Quota { what, used, cap } => write!(f, "quota      {what} {cap}, {used} used"),
            Step::Decision { seq, what } => write!(f, "decision   #{seq}  declared  {what}"),
            Step::Occasion {
                seq,
                claim,
                kind,
                what,
            } => write!(
                f,
                "occasion   #{seq}  {claim}  {kind}: {what}{}",
                if claim.authorizes() {
                    ""
                } else {
                    "  (authorizes nothing)"
                }
            ),
            Step::Cap { used, cap } => write!(f, "cap        {used} of {cap} used"),
            Step::Running { venture } => write!(f, "running    {venture} is not paused"),
            Step::Axiom { text } => write!(f, "axiom      {text}"),
        }
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::NoMandate { venture } => write!(f, "{venture} has no adopted mandate"),
            Reason::NotPermitted { action } => write!(f, "the mandate does not permit {action}"),
            Reason::Forbidden { action } => write!(f, "the mandate forbids {action}"),
            Reason::Hardwired { why, .. } => write!(f, "{why}"),
            Reason::Paused { venture } => write!(f, "{venture} is paused"),
            Reason::OverCap { used, cap } => {
                write!(f, "the cap is {cap} and {used} are already used")
            }
            Reason::NeedsDecision {
                action,
                suspended: true,
            } => write!(
                f,
                "{action} was found ineffective; each run needs a declared decision"
            ),
            Reason::NeedsDecision { action, .. } => write!(
                f,
                "{action} is irreversible and no declared decision approves it"
            ),
            Reason::NotJustified { action } => {
                write!(f, "nothing measured or declared calls for {action}")
            }
            Reason::OnlyInferred { seqs } => {
                let list: Vec<String> = seqs.iter().map(|s| format!("#{s}")).collect();
                write!(
                    f,
                    "the cited records ({}) are all inferred; inferred records cannot authorize",
                    list.join(", ")
                )
            }
            Reason::UnknownRecord { seq } => write!(f, "record #{seq} is not in the ledger"),
            Reason::NeedsDeclaredResume { venture } => {
                write!(f, "resuming {venture} needs a declared decision")
            }
            Reason::NoPayload { action, takes } => {
                write!(f, "{action} takes {takes} and nothing cited carries one")
            }
            Reason::WrongPayload {
                action,
                takes,
                found,
            } => write!(f, "{action} takes {takes}, not {found}"),
            Reason::OverBudget {
                spent,
                cost,
                budget,
            } => write!(
                f,
                "the parent's budget is {budget} a day, {spent} is spent and this costs {cost}"
            ),
            Reason::OverQuota { what, used, cap } => {
                write!(f, "the {what} quota is {cap} and {used} are already used")
            }
            Reason::Pending { intent } => write!(
                f,
                "intent #{intent} has no outcome; until it is known whether it happened, it does not run again"
            ),
            Reason::NotAChild { venture, child } => write!(
                f,
                "{child} is not a child of {venture}: both mandates must say so"
            ),
            Reason::BelowFloor {
                child,
                budget,
                floor,
            } => write!(
                f,
                "a budget of {budget} is below {child}'s floor of {floor}"
            ),
            Reason::Priority {
                child,
                spared,
                budget,
                floor,
            } => write!(
                f,
                "{child} is not cut while {spared}, of lower priority, sits at {budget} above its floor of {floor}"
            ),
            Reason::InDeadband {
                child,
                change,
                deadband,
            } => write!(
                f,
                "a change of {change} to {child}'s budget is inside the deadband of {deadband}"
            ),
            Reason::Reversal { child, last, hold } => write!(
                f,
                "{child}'s budget moved the other way in #{last}, and the hold is {}",
                humantime::format_duration(*hold)
            ),
            Reason::Guarded {
                guard,
                value,
                above,
                bound,
            } => write!(
                f,
                "guard {guard} reads {value}, past its bound ({} {bound}); no lever but stop runs until it is back inside",
                if *above { "above" } else { "below" }
            ),
        }
    }
}

impl fmt::Display for Proof {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{}: {} is allowed", self.venture, self.action)?;
        for s in &self.steps {
            writeln!(f, "  {s}")?;
        }
        Ok(())
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{}: {} is refused", self.venture, self.action)?;
        for s in &self.steps {
            writeln!(f, "  {s}")?;
        }
        for r in &self.reasons {
            writeln!(f, "  because    {r}")?;
        }
        Ok(())
    }
}

/// The derivation behind a record, in plain text: the proof for an action
/// or intent, the failed derivation behind an ask, or where any other
/// record came from.
pub fn why(entry: &Entry) -> String {
    let head = entry.line();
    match &entry.record {
        Record::Action { proof, .. } | Record::Intent { proof, .. } => format!("{head}\n{proof}"),
        Record::Ask {
            refusal: Some(r), ..
        } => format!("{head}\n{r}"),
        Record::Ask { question, .. } => format!("{head}\n  asked: {question}\n"),
        Record::Report {
            claim,
            source,
            confidence,
            cites,
            ..
        } => {
            let mut text = format!("{head}\n  {claim} by {}\n", by(*claim));
            if let Some(s) = source {
                text.push_str(&format!("  source     {s}\n"));
            }
            if let Some(c) = confidence {
                text.push_str(&format!("  confidence {c}\n"));
            }
            if !cites.is_empty() {
                let list: Vec<String> = cites.iter().map(|s| format!("#{s}")).collect();
                text.push_str(&format!("  cites      {}\n", list.join(", ")));
            }
            if !claim.authorizes() {
                text.push_str("  authorizes nothing on its own\n");
            }
            text
        }
        Record::Proposal {
            claim: Claim::Declared,
            reason,
            ..
        } => format!("{head}\n  declared by a human: {reason}\n  the gate still decides\n"),
        Record::Proposal { reason, .. } => {
            format!("{head}\n  inferred by a model: {reason}\n  authorizes nothing on its own\n")
        }
        Record::Decision {
            claim,
            by: Some(who),
            ..
        } => format!("{head}\n  {claim} by {who}, over the tailnet\n"),
        Record::Decision { claim, .. } => format!("{head}\n  {claim} by {}\n", by(*claim)),
        Record::Outcome { intent, .. } => format!("{head}\n  outcome of intent #{intent}\n"),
        Record::Refusal {
            reason,
            detail,
            closes,
            ..
        } => {
            let mut text = format!("{head}\n  reason     {reason}\n  detail     {detail}\n");
            if let Some(open) = closes {
                text.push_str(&format!("  closes     #{open}\n"));
            }
            text.push_str("  authorizes nothing and counts toward nothing\n");
            text
        }
        Record::Escalation {
            from,
            to,
            why,
            refused,
            ..
        } => {
            let mut text =
                format!("{head}\n  from       {from}\n  to         {to}\n  why        {why}\n");
            if let Some(reason) = refused {
                text.push_str(&format!("  refused    {reason}\n"));
            }
            text.push_str("  written by code before the call; authorizes nothing\n");
            text
        }
        Record::Answer {
            escalation,
            cites,
            detail,
            ..
        } => {
            let list: Vec<String> = cites.iter().map(|s| format!("#{s}")).collect();
            let mut text = format!("{head}\n  answer to escalation #{escalation}: {detail}\n");
            if !list.is_empty() {
                text.push_str(&format!("  cites      {}\n", list.join(", ")));
            }
            text.push_str("  the inferred records it cites authorize nothing\n");
            text
        }
    }
}

fn by(claim: Claim) -> &'static str {
    match claim {
        Claim::Measured => "code",
        Claim::Declared => "a human",
        Claim::Inferred => "a model",
    }
}

/// The gate as first written: every call loads every record of the ledger
/// into the rules and derives from all of them. Slow on a long ledger and
/// plainly right, it is the oracle the gate above is tested against.
pub mod reference {
    use super::*;

    ascent! {
        /// What the ledger holds true now. Every report is a fact about a
        /// topic. A measured fact is superseded by a later measured fact on
        /// the same topic; any fact is superseded by a later one that cites
        /// it. The rest is current.
        struct Facts;

        // seq, venture, topic, measured or declared
        relation fact(Seq, String, String, bool);
        relation cites(Seq, Seq);
        relation superseded(Seq);
        superseded(o) <-- fact(o, v, t, true), fact(n, v, t, true), if n > o;
        superseded(o) <-- fact(o, v, t, _), fact(n, v, t, _), cites(n, o), if n > o;
        relation current(Seq, String, String);
        current(s, v, t) <-- fact(s, v, t, _), !superseded(s);
    }

    /// The current facts: (seq, venture, topic), oldest first.
    pub fn current_facts(ledger: &[Entry]) -> Vec<(Seq, String, String)> {
        let mut f = Facts::default();
        for e in ledger {
            if let Record::Report {
                claim,
                venture,
                metric,
                cites,
                ..
            } = &e.record
            {
                f.fact
                    .push((e.seq, venture.clone(), metric.clone(), claim.authorizes()));
                for c in cites {
                    f.cites.push((e.seq, *c));
                }
            }
        }
        f.run();
        let mut out: Vec<(Seq, String, String)> = f.current.into_iter().collect();
        out.sort_unstable();
        out
    }

    /// The outcomes that settle an intent: trusted, later, of the same
    /// venture. (intent, ok) for each.
    fn settled(ledger: &[Entry]) -> Vec<(Seq, bool)> {
        ledger
            .iter()
            .filter_map(|o| match &o.record {
                Record::Outcome {
                    claim,
                    venture,
                    intent,
                    ok,
                    ..
                } if claim.authorizes() && *intent < o.seq => ledger
                    .iter()
                    .find(|i| i.seq == *intent)
                    .filter(
                        |i| matches!(&i.record, Record::Intent { venture: v, .. } if v == venture),
                    )
                    .map(|_| (*intent, *ok)),
                _ => None,
            })
            .collect()
    }

    /// Intents with an ok outcome: lever runs that are done.
    fn done(ledger: &[Entry]) -> HashSet<Seq> {
        settled(ledger)
            .into_iter()
            .filter(|(_, ok)| *ok)
            .map(|(i, _)| i)
            .collect()
    }

    /// The oldest intent of this lever with no outcome.
    fn pending(ledger: &[Entry], venture: &str, action: &str) -> Option<Seq> {
        let settled: HashSet<Seq> = settled(ledger).into_iter().map(|(i, _)| i).collect();
        ledger
            .iter()
            .filter(|e| matches!(&e.record, Record::Intent { venture: v, action: a, .. } if v == venture && a == action))
            .map(|e| e.seq)
            .filter(|s| !settled.contains(s))
            .min()
    }

    /// The parent levers pulled on `child` by its parent, oldest first:
    /// each successful action's, or done intent's, sequence number and
    /// what it set.
    fn parent_calls(
        ledger: &[Entry],
        mandates: &[&Adopted],
        child: &str,
    ) -> Vec<(Seq, ParentCall)> {
        let Some(parent) = mandates
            .iter()
            .find(|m| m.venture == child)
            .and_then(|m| m.mandate.parent.clone())
        else {
            return vec![];
        };
        if !is_child_of(mandates, child, &parent) {
            return vec![];
        }
        let done = done(ledger);
        let mut calls: Vec<(Seq, ParentCall)> = ledger
            .iter()
            .filter_map(|e| match &e.record {
                Record::Action {
                    venture,
                    action,
                    ok: true,
                    payload: Some(Payload::Text { text }),
                    ..
                }
                | Record::Intent {
                    venture,
                    action,
                    payload: Some(Payload::Text { text }),
                    ..
                } if *venture == parent
                    && (e.record.kind() == Kind::Action || done.contains(&e.seq)) =>
                {
                    ParentCall::parse(action, text)
                        .ok()
                        .filter(|c| c.child() == child)
                        .map(|c| (e.seq, c))
                }
                _ => None,
            })
            .collect();
        calls.sort_by_key(|(s, _)| *s);
        calls
    }

    /// The budgets set for `child` by its parent: seq, when, amount; oldest first.
    fn budgets_set(ledger: &[Entry], mandates: &[&Adopted], child: &str) -> Vec<(Seq, i64, u32)> {
        parent_calls(ledger, mandates, child)
            .into_iter()
            .filter_map(|(seq, call)| match call {
                ParentCall::Budget { amount, .. } => {
                    let at = ledger.iter().find(|e| e.seq == seq).map_or(0, |e| e.at);
                    Some((seq, at, amount))
                }
                _ => None,
            })
            .collect()
    }

    /// The threshold that applies to a venture now, if its controller is one.
    pub fn effective_threshold(
        ledger: &[Entry],
        mandates: &[Adopted],
        venture: &str,
    ) -> Option<Threshold> {
        let refs: Vec<&Adopted> = mandates.iter().collect();
        let mut t = mandate_threshold(&refs, venture)?;
        for (seq, call) in parent_calls(ledger, &refs, venture) {
            if let ParentCall::Threshold { above, bound, .. } = call {
                t.above = above;
                t.bound = bound;
                t.from = Some(seq);
            }
        }
        Some(t)
    }

    /// The budget a venture's parent set for it, and the action that set it.
    pub fn budget_of(ledger: &[Entry], mandates: &[&Adopted], venture: &str) -> Option<(Seq, u32)> {
        parent_calls(ledger, mandates, venture)
            .into_iter()
            .filter_map(|(seq, call)| match call {
                ParentCall::Budget { amount, .. } => Some((seq, amount)),
                _ => None,
            })
            .next_back()
    }

    /// Actions and intents of `venture` (every venture when None) since
    /// `since`, as (venture, action).
    fn runs_since<'a>(
        ledger: &'a [Entry],
        venture: Option<&str>,
        since: i64,
    ) -> Vec<(&'a str, &'a str)> {
        ledger
            .iter()
            .filter(|e| e.at > since)
            .filter(|e| matches!(e.record.kind(), Kind::Action | Kind::Intent))
            .filter(|e| venture.is_none_or(|v| e.record.venture() == v))
            .filter_map(|e| e.record.action().map(|a| (e.record.venture(), a)))
            .collect()
    }

    /// What a venture spent today.
    pub fn spent_today(ledger: &[Entry], mandates: &[&Adopted], venture: &str, now: i64) -> u32 {
        runs_since(ledger, Some(venture), now - Cap::DAY.as_secs() as i64)
            .into_iter()
            .map(|(v, a)| cost_of(mandates, v, a))
            .sum()
    }

    /// How the shared quotas bear on an action.
    pub fn quota_check(
        ledger: &[Entry],
        mandates: &[Adopted],
        quotas: &Quotas,
        venture: &str,
        action: &str,
        now: i64,
    ) -> (Vec<Step>, Vec<Reason>) {
        let refs: Vec<&Adopted> = mandates.iter().collect();
        let runs = |per: Duration| runs_since(ledger, None, now - per.as_secs() as i64).into_iter();
        quota_steps(&refs, quotas, venture, action, runs)
    }

    /// How many asks were written inside the asks quota's window.
    pub fn asks_used(ledger: &[Entry], cap: Cap, now: i64) -> u32 {
        ledger
            .iter()
            .filter(|e| e.at > now - cap.per.as_secs() as i64)
            .filter(|e| e.record.kind() == Kind::Ask)
            .count() as u32
    }

    /// Decide whether an action may happen, with no shared quotas.
    pub fn gate(
        candidate: &Candidate,
        ledger: &[Entry],
        mandates: &[Adopted],
        now: i64,
    ) -> Result<Proof, Refusal> {
        gate_with(candidate, ledger, mandates, &Quotas::default(), now)
    }

    /// Decide whether an action may happen, under the shared quotas too.
    pub fn gate_with(
        candidate: &Candidate,
        ledger: &[Entry],
        mandates: &[Adopted],
        quotas: &Quotas,
        now: i64,
    ) -> Result<Proof, Refusal> {
        let v = candidate.venture.as_str();
        let a = candidate.action.as_str();
        let mut p = Rules::default();
        p.candidate.push((v.to_string(), a.to_string()));
        p.cites = candidate.cites.iter().map(|s| (*s,)).collect();
        load_ledger(&mut p, ledger);

        // Only mandates the ledger shows adopted by a declared decision count.
        let adopted: Vec<&Adopted> = mandates
            .iter()
            .filter(|m| ledger.iter().any(|e| e.seq == m.seq && adopts(e, m)))
            .collect();
        load_mandates(
            &mut p,
            &adopted,
            now,
            |venture, action, since| {
                ledger
                    .iter()
                    .filter(|e| {
                        e.record.venture() == venture
                            && e.record.action() == Some(action)
                            && matches!(e.record.kind(), Kind::Action | Kind::Intent)
                            && e.at > since
                    })
                    .count() as u32
            },
            |venture| effective_threshold(ledger, mandates, venture),
            |venture, action| suspended(ledger, venture, action),
        );
        load_family(&mut p, ledger, &adopted);
        let budget = budget_of(ledger, &adopted, v);
        let spent = || spent_today(ledger, &adopted, v, now);
        let quota = quota_check(ledger, mandates, quotas, v, a, now);
        let (extra_steps, mut extra_reasons) = limits(&mut p, v, a, &adopted, budget, spent, quota);
        p.run();
        extra_reasons.extend(allocation_reasons(
            &p,
            &|s| ledger.iter().find(|e| e.seq == s),
            (v, a),
            &adopted,
            &|child| budgets_set(ledger, &adopted, child),
            now,
        ));
        assemble(
            &p,
            candidate,
            &|s| ledger.iter().find(|e| e.seq == s),
            &adopted,
            pending(ledger, v, a),
            extra_steps,
            extra_reasons,
        )
    }

    /// Is the lever suspended: some measured ask says it is ineffective
    /// and neither a declared approval of it by the same venture nor a
    /// declared adoption of the venture's mandate after it answers it?
    pub fn suspended(ledger: &[Entry], venture: &str, action: &str) -> bool {
        ledger.iter().any(|e| {
            matches!(&e.record, Record::Ask {
                claim: Claim::Measured,
                venture: v,
                signal: Some(crate::record::Signal::Ineffective { action: a, .. }),
                ..
            } if v == venture && a == action)
                && !ledger.iter().any(|d| {
                    matches!(&d.record, Record::Decision {
                        claim: Claim::Declared,
                        venture: dv,
                        verdict: Verdict::Approve { ask, .. },
                        ..
                    } if dv == venture && *ask == e.seq)
                })
                && !ledger.iter().any(|d| {
                    d.seq > e.seq
                        && matches!(&d.record, Record::Decision {
                            claim: Claim::Declared,
                            venture: dv,
                            verdict: Verdict::Adopt { .. },
                            ..
                        } if dv == venture)
                })
        })
    }

    /// Parents and children as facts, and every pause and resume a parent
    /// pulled.
    fn load_family(p: &mut Rules, ledger: &[Entry], adopted: &[&Adopted]) {
        load_family_mandates(p, adopted);
        let done = done(ledger);
        for e in ledger {
            load_call(p, e, &|s| done.contains(&s));
        }
    }

    /// Every ledger entry as facts.
    fn load_ledger(p: &mut Rules, ledger: &[Entry]) {
        for e in ledger {
            load_entry(p, e);
        }
    }

    /// The state of a venture.
    pub fn state(ledger: &[Entry], mandates: &[Adopted], venture: &str) -> State {
        let mut p = Rules::default();
        load_ledger(&mut p, ledger);
        let adopted: Vec<&Adopted> = mandates.iter().collect();
        load_family(&mut p, ledger, &adopted);
        p.run();
        if p.paused.iter().any(|(v,)| v == venture) {
            State::Paused
        } else if p.open_ask.iter().any(|(_, v)| v == venture) {
            State::Asking
        } else {
            State::Running
        }
    }

    /// Asks with no declared decision yet, oldest first.
    pub fn open_asks(ledger: &[Entry]) -> Vec<Seq> {
        let mut p = Rules::default();
        load_ledger(&mut p, ledger);
        p.run();
        let mut open: Vec<Seq> = p.open_ask.iter().map(|(q, _)| *q).collect();
        open.sort_unstable();
        open
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mandate::{Allocation, Child, Restart};
    use crate::record::{Payload, GENESIS};
    use std::time::Duration;

    const NOW: i64 = 1_700_000_000;
    const DAY: i64 = 86_400;
    const V: &str = "counter";

    fn permit(name: &str, cap: u32, reversible: bool, delegated: bool) -> Permitted {
        Permitted {
            name: name.to_string(),
            run: Some(format!("./{name}")),
            adapter: None,
            cap: Cap {
                count: cap,
                per: Cap::DAY,
            },
            reversible,
            delegated,
            takes: None,
            cost: 0,
            check: None,
            expects: None,
            revoke_after: None,
            fields: None,
        }
    }

    fn mandate(controller: Controller, permitted: Vec<Permitted>, forbidden: &[&str]) -> Mandate {
        Mandate {
            purpose: "keep the counter low".to_string(),
            metric: "count".to_string(),
            controller,
            permitted,
            forbidden: forbidden.iter().map(|s| s.to_string()).collect(),
            cadence: Duration::from_secs(60),
            context_budget: 8000,
            summarize_after: 200,
            restart: Restart::OnFailure,
            network: None,
            children: vec![],
            parent: None,
            allocation: None,
            guards: vec![],
            audits: vec![],
            range: None,
            max_step: None,
            escalate: None,
        }
    }

    fn above(bound: f64, action: &str) -> Controller {
        Controller::Threshold {
            above: Some(bound),
            below: None,
            action: action.to_string(),
            deadband: None,
            hold: None,
        }
    }

    fn model() -> Controller {
        Controller::Model { backend: None }
    }

    fn report(claim: Claim, value: f64) -> Record {
        Record::report(claim, V, "count", Some(value), "")
    }

    fn proposal(claim: Claim, action: &str) -> Record {
        Record::Proposal {
            cites: vec![],
            claim,
            venture: V.to_string(),
            action: action.to_string(),
            reason: "it seemed right".to_string(),
            payload: None,
        }
    }

    fn proposal_with(claim: Claim, action: &str, payload: Payload) -> Record {
        Record::Proposal {
            cites: vec![],
            claim,
            venture: V.to_string(),
            action: action.to_string(),
            reason: "it seemed right".to_string(),
            payload: Some(payload),
        }
    }

    fn ask(claim: Claim, action: &str) -> Record {
        Record::Ask {
            cites: vec![],
            claim,
            venture: V.to_string(),
            action: Some(action.to_string()),
            question: format!("may I {action}?"),
            refusal: None,
            signal: None,
            payload: None,
        }
    }

    fn ask_with(claim: Claim, action: &str, payload: Payload) -> Record {
        Record::Ask {
            cites: vec![],
            claim,
            venture: V.to_string(),
            action: Some(action.to_string()),
            question: format!("may I {action}?"),
            refusal: None,
            signal: None,
            payload: Some(payload),
        }
    }

    fn decision(claim: Claim, verdict: Verdict) -> Record {
        Record::Decision {
            claim,
            venture: V.to_string(),
            verdict,
            by: None,
        }
    }

    fn approve(ask: Seq) -> Verdict {
        Verdict::Approve { ask, payload: None }
    }

    fn text(t: &str) -> Payload {
        Payload::Text {
            text: t.to_string(),
        }
    }

    fn done(action: &str) -> Record {
        Record::Action {
            claim: Claim::Measured,
            venture: V.to_string(),
            action: action.to_string(),
            proof: Proof {
                venture: V.to_string(),
                action: action.to_string(),
                steps: vec![],
            },
            ok: true,
            detail: String::new(),
            payload: None,
        }
    }

    /// A hand-written ledger.
    struct Fx {
        entries: Vec<Entry>,
        adopted: Vec<Adopted>,
        at: i64,
    }

    impl Fx {
        fn new() -> Fx {
            Fx {
                entries: vec![],
                adopted: vec![],
                at: NOW - 3600,
            }
        }

        fn push_at(&mut self, at: i64, record: Record) -> Seq {
            let seq = self.entries.len() as Seq + 1;
            let prev = self
                .entries
                .last()
                .map_or(GENESIS.to_string(), |e| e.hash.clone());
            self.entries.push(Entry::new(seq, at, &prev, record));
            seq
        }

        fn push(&mut self, record: Record) -> Seq {
            self.at += 1;
            let at = self.at;
            self.push_at(at, record)
        }

        fn adopt(&mut self, venture: &str, m: Mandate) -> Seq {
            let hash = Mandate::hash(&serde_yaml::to_string(&m).unwrap());
            let seq = self.push(Record::Decision {
                claim: Claim::Declared,
                venture: venture.to_string(),
                verdict: Verdict::Adopt {
                    mandate: hash.clone(),
                    path: format!("/ventures/{venture}"),
                    text: None,
                },
                by: None,
            });
            self.adopted.push(Adopted {
                seq,
                venture: venture.to_string(),
                hash,
                mandate: m,
            });
            seq
        }

        fn gate(&self, venture: &str, action: &str, cites: &[Seq]) -> Result<Proof, Refusal> {
            gate(
                &Candidate {
                    venture: venture.to_string(),
                    action: action.to_string(),
                    cites: cites.to_vec(),
                },
                &self.entries,
                &self.adopted,
                NOW,
            )
        }
    }

    fn reasons(r: &Refusal) -> Vec<String> {
        r.reasons.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn threshold_crossed_on_measured_report_is_allowed() {
        let mut fx = Fx::new();
        let m = fx.adopt(
            V,
            mandate(
                above(10.0, "reset"),
                vec![permit("reset", 3, true, false)],
                &[],
            ),
        );
        let r = fx.push(report(Claim::Measured, 12.0));
        let proof = fx.gate(V, "reset", &[r]).expect("allowed");
        assert!(matches!(proof.steps[0], Step::Mandate { seq, .. } if seq == m));
        assert!(proof
            .steps
            .iter()
            .any(|s| matches!(s, Step::Report { seq, claim: Claim::Measured, .. } if *seq == r)));
        assert!(proof
            .steps
            .iter()
            .any(|s| matches!(s, Step::Threshold { above: true, .. })));
        let text = proof.to_string();
        assert!(text.starts_with("counter: reset is allowed\n"), "{text}");
        assert!(text.contains("report     #2  measured  count=12"), "{text}");
    }

    #[test]
    fn threshold_not_crossed_is_refused() {
        let mut fx = Fx::new();
        fx.adopt(
            V,
            mandate(
                above(10.0, "reset"),
                vec![permit("reset", 3, true, false)],
                &[],
            ),
        );
        let r = fx.push(report(Claim::Measured, 9.0));
        let refusal = fx.gate(V, "reset", &[r]).unwrap_err();
        assert!(
            reasons(&refusal).contains(&"nothing measured or declared calls for reset".to_string())
        );
    }

    #[test]
    fn inferred_only_chain_is_refused() {
        // A model-written report cannot cross a threshold.
        let mut fx = Fx::new();
        fx.adopt(
            V,
            mandate(
                above(10.0, "reset"),
                vec![permit("reset", 3, true, false)],
                &[],
            ),
        );
        let r = fx.push(report(Claim::Inferred, 99.0));
        let refusal = fx.gate(V, "reset", &[r]).unwrap_err();
        let why = refusal.to_string();
        assert!(
            why.contains("(#2) are all inferred; inferred records cannot authorize"),
            "{why}"
        );
        assert!(
            why.contains("occasion   #2  inferred  report: count=99  (authorizes nothing)"),
            "{why}"
        );
        assert!(refusal
            .reasons
            .iter()
            .any(|r| matches!(r, Reason::NotJustified { .. })));

        // A model-written decision cannot approve an irreversible action.
        let mut fx = Fx::new();
        fx.adopt(
            V,
            mandate(model(), vec![permit("deploy", 1, false, false)], &[]),
        );
        let q = fx.push(ask(Claim::Inferred, "deploy"));
        let d = fx.push(decision(Claim::Inferred, approve(q)));
        let refusal = fx.gate(V, "deploy", &[q, d]).unwrap_err();
        assert!(refusal
            .reasons
            .iter()
            .any(|r| matches!(r, Reason::NeedsDecision { .. })));
        assert!(refusal
            .reasons
            .iter()
            .any(|r| matches!(r, Reason::OnlyInferred { seqs } if *seqs == vec![q, d])));
    }

    #[test]
    fn declared_approval_allows_irreversible_action() {
        let mut fx = Fx::new();
        fx.adopt(
            V,
            mandate(model(), vec![permit("deploy", 1, false, false)], &[]),
        );
        let q = fx.push(ask(Claim::Inferred, "deploy"));
        let d = fx.push(decision(Claim::Declared, approve(q)));
        let proof = fx.gate(V, "deploy", &[d]).expect("approved");
        assert!(proof
            .steps
            .iter()
            .any(|s| matches!(s, Step::Decision { seq, .. } if *seq == d)));
        // A denial is not an approval.
        let mut fx = Fx::new();
        fx.adopt(
            V,
            mandate(model(), vec![permit("deploy", 1, false, false)], &[]),
        );
        let q = fx.push(ask(Claim::Inferred, "deploy"));
        let d = fx.push(decision(
            Claim::Declared,
            Verdict::Deny {
                ask: q,
                reason: "no".into(),
            },
        ));
        assert!(fx.gate(V, "deploy", &[d]).is_err());
    }

    #[test]
    fn proposal_justifies_reversible_permitted_action() {
        let mut fx = Fx::new();
        fx.adopt(
            V,
            mandate(model(), vec![permit("tidy", 3, true, false)], &[]),
        );
        let s = fx.push(proposal(Claim::Inferred, "tidy"));
        let proof = fx.gate(V, "tidy", &[s]).expect("allowed");
        let text = proof.to_string();
        assert!(
            text.contains(
                "occasion   #2  inferred  proposal: tidy: it seemed right  (authorizes nothing)"
            ),
            "{text}"
        );
        assert!(text.contains("cap        0 of 3/day used"), "{text}");
    }

    #[test]
    fn proposal_for_irreversible_action_needs_decision_unless_delegated() {
        let mut fx = Fx::new();
        fx.adopt(
            V,
            mandate(model(), vec![permit("deploy", 1, false, false)], &[]),
        );
        let s = fx.push(proposal(Claim::Inferred, "deploy"));
        let refusal = fx.gate(V, "deploy", &[s]).unwrap_err();
        assert!(reasons(&refusal)
            .contains(&"deploy is irreversible and no declared decision approves it".to_string()));

        let mut fx = Fx::new();
        fx.adopt(
            V,
            mandate(model(), vec![permit("deploy", 1, false, true)], &[]),
        );
        let s = fx.push(proposal(Claim::Inferred, "deploy"));
        let proof = fx.gate(V, "deploy", &[s]).expect("delegated");
        assert!(proof.to_string().contains("irreversible, delegated"));
    }

    #[test]
    fn cap_exceeding_action_is_refused() {
        let mut fx = Fx::new();
        fx.adopt(
            V,
            mandate(
                above(10.0, "reset"),
                vec![permit("reset", 3, true, false)],
                &[],
            ),
        );
        for _ in 0..3 {
            fx.push(done("reset"));
        }
        let r = fx.push(report(Claim::Measured, 12.0));
        let refusal = fx.gate(V, "reset", &[r]).unwrap_err();
        assert!(refusal.reasons.contains(&Reason::OverCap {
            used: 3,
            cap: Cap {
                count: 3,
                per: Cap::DAY
            }
        }));
        assert!(refusal
            .to_string()
            .contains("because    the cap is 3/day and 3 are already used"));

        // Runs older than the cap window do not count.
        let mut fx = Fx::new();
        fx.adopt(
            V,
            mandate(
                above(10.0, "reset"),
                vec![permit("reset", 3, true, false)],
                &[],
            ),
        );
        for i in 0..3 {
            fx.push_at(NOW - 2 * DAY + i, done("reset"));
        }
        let r = fx.push_at(NOW - 1, report(Claim::Measured, 12.0));
        assert!(fx.gate(V, "reset", &[r]).is_ok());
    }

    #[test]
    fn stop_always_passes() {
        // No mandate, paused, forbidden, cited only inferred records: still allowed.
        let mut fx = Fx::new();
        let s = fx.push(proposal(Claim::Inferred, STOP));
        let proof = fx.gate(V, STOP, &[s]).expect("stop");
        assert!(matches!(proof.steps[0], Step::Axiom { .. }));

        let mut fx = Fx::new();
        fx.adopt(V, mandate(model(), vec![], &[STOP]));
        fx.push(decision(Claim::Declared, Verdict::Pause));
        assert!(fx.gate(V, STOP, &[]).is_ok());
    }

    #[test]
    fn resume_without_declared_record_is_refused() {
        let mut fx = Fx::new();
        fx.adopt(
            V,
            mandate(model(), vec![permit("tidy", 3, true, false)], &[]),
        );
        fx.push(decision(Claim::Declared, Verdict::Pause));
        let fake = fx.push(decision(Claim::Inferred, Verdict::Resume));
        let refusal = fx.gate(V, RESUME, &[fake]).unwrap_err();
        assert!(
            reasons(&refusal).contains(&"resuming counter needs a declared decision".to_string())
        );
        assert!(fx.gate(V, RESUME, &[]).is_err());

        let real = fx.push(decision(Claim::Declared, Verdict::Resume));
        let proof = fx.gate(V, RESUME, &[real]).expect("declared resume");
        assert!(proof
            .steps
            .iter()
            .any(|s| matches!(s, Step::Decision { seq, .. } if *seq == real)));
    }

    #[test]
    fn paused_venture_refuses_actions_until_declared_resume() {
        let mut fx = Fx::new();
        fx.adopt(
            V,
            mandate(
                above(10.0, "reset"),
                vec![permit("reset", 3, true, false)],
                &[],
            ),
        );
        fx.push(decision(Claim::Declared, Verdict::Pause));
        let r = fx.push(report(Claim::Measured, 12.0));
        let refusal = fx.gate(V, "reset", &[r]).unwrap_err();
        assert!(refusal
            .reasons
            .contains(&Reason::Paused { venture: V.into() }));

        // An inferred resume changes nothing; a declared one does.
        fx.push(decision(Claim::Inferred, Verdict::Resume));
        let r = fx.push(report(Claim::Measured, 12.0));
        assert!(fx.gate(V, "reset", &[r]).is_err());
        fx.push(decision(Claim::Declared, Verdict::Resume));
        let r = fx.push(report(Claim::Measured, 12.0));
        assert!(fx.gate(V, "reset", &[r]).is_ok());

        // A stop action pauses too.
        fx.push(done(STOP));
        let r = fx.push(report(Claim::Measured, 12.0));
        assert!(fx.gate(V, "reset", &[r]).is_err());
    }

    #[test]
    fn forbidden_and_unpermitted_actions_are_refused() {
        let mut fx = Fx::new();
        fx.adopt(
            V,
            mandate(
                model(),
                vec![
                    permit("tidy", 3, true, false),
                    permit("wipe", 1, true, false),
                ],
                &["wipe"],
            ),
        );
        let s = fx.push(proposal(Claim::Inferred, "wipe"));
        let refusal = fx.gate(V, "wipe", &[s]).unwrap_err();
        assert!(refusal.reasons.contains(&Reason::Forbidden {
            action: "wipe".into()
        }));
        let s = fx.push(proposal(Claim::Inferred, "launch"));
        let refusal = fx.gate(V, "launch", &[s]).unwrap_err();
        assert!(refusal.reasons.contains(&Reason::NotPermitted {
            action: "launch".into()
        }));
    }

    #[test]
    fn no_mandate_or_mismatched_mandate_is_refused() {
        let mut fx = Fx::new();
        let r = fx.push(report(Claim::Measured, 12.0));
        let refusal = fx.gate(V, "reset", &[r]).unwrap_err();
        assert!(refusal
            .reasons
            .contains(&Reason::NoMandate { venture: V.into() }));

        // The adopted hash must match the declared record.
        let mut fx = Fx::new();
        fx.adopt(
            V,
            mandate(
                above(10.0, "reset"),
                vec![permit("reset", 3, true, false)],
                &[],
            ),
        );
        fx.adopted[0].hash = "edited-on-disk".to_string();
        let r = fx.push(report(Claim::Measured, 12.0));
        let refusal = fx.gate(V, "reset", &[r]).unwrap_err();
        assert!(refusal
            .reasons
            .contains(&Reason::NoMandate { venture: V.into() }));
    }

    #[test]
    fn stale_report_does_not_justify() {
        let mut fx = Fx::new();
        fx.adopt(
            V,
            mandate(
                above(10.0, "reset"),
                vec![permit("reset", 3, true, false)],
                &[],
            ),
        );
        let old = fx.push(report(Claim::Measured, 12.0));
        fx.push(report(Claim::Measured, 3.0));
        assert!(fx.gate(V, "reset", &[old]).is_err());
    }

    #[test]
    fn unknown_cited_record_is_refused() {
        let mut fx = Fx::new();
        fx.adopt(
            V,
            mandate(
                above(10.0, "reset"),
                vec![permit("reset", 3, true, false)],
                &[],
            ),
        );
        let refusal = fx.gate(V, "reset", &[42]).unwrap_err();
        assert!(refusal.reasons.contains(&Reason::UnknownRecord { seq: 42 }));
    }

    #[test]
    fn self_hardwired_list_is_refused_even_when_permitted() {
        for (name, why) in HARDWIRED {
            let mut fx = Fx::new();
            fx.adopt(
                SELF,
                mandate(model(), vec![permit(name, 9, true, true)], &[]),
            );
            let s = fx.push(Record::Proposal {
                cites: vec![],
                claim: Claim::Inferred,
                venture: SELF.into(),
                action: name.to_string(),
                reason: "why not".into(),
                payload: None,
            });
            let refusal = fx.gate(SELF, name, &[s]).unwrap_err();
            assert!(reasons(&refusal).contains(&why.to_string()), "{refusal}");
        }
        // A differently named self action whose command touches keys is caught too.
        let mut fx = Fx::new();
        let mut perm = permit("rotate", 9, true, true);
        perm.run = Some("cat ~/.avsm/keys".to_string());
        fx.adopt(SELF, mandate(model(), vec![perm], &[]));
        let s = fx.push(Record::Proposal {
            cites: vec![],
            claim: Claim::Inferred,
            venture: SELF.into(),
            action: "rotate".into(),
            reason: "why not".into(),
            payload: None,
        });
        let refusal = fx.gate(SELF, "rotate", &[s]).unwrap_err();
        assert!(reasons(&refusal).contains(&"the self venture cannot touch keys".to_string()));
        // The same action name in another venture is fine.
        let mut fx = Fx::new();
        fx.adopt(
            V,
            mandate(model(), vec![permit("merge-main", 9, true, false)], &[]),
        );
        let s = fx.push(proposal(Claim::Inferred, "merge-main"));
        assert!(fx.gate(V, "merge-main", &[s]).is_ok());
    }

    #[test]
    fn why_renders_every_kind() {
        let mut fx = Fx::new();
        fx.adopt(
            V,
            mandate(
                above(10.0, "reset"),
                vec![permit("reset", 3, true, false)],
                &[],
            ),
        );
        let r = fx.push(report(Claim::Measured, 12.0));
        let proof = fx.gate(V, "reset", &[r]).unwrap();
        let a = fx.push(Record::Action {
            claim: Claim::Measured,
            venture: V.into(),
            action: "reset".into(),
            proof,
            ok: true,
            detail: String::new(),
            payload: None,
        });
        let text = why(&fx.entries[(a - 1) as usize]);
        assert!(text.starts_with(&format!("#{a}")), "{text}");
        assert!(text.contains("counter: reset is allowed"), "{text}");
        assert!(text.contains("threshold  count above 10"), "{text}");
        for e in &fx.entries {
            assert!(!why(e).is_empty());
        }
        let q = fx.push(Record::Ask {
            cites: vec![],
            claim: Claim::Inferred,
            venture: V.into(),
            action: Some("deploy".into()),
            question: "may I?".into(),
            refusal: Some(Refusal {
                venture: V.into(),
                action: "deploy".into(),
                steps: vec![],
                reasons: vec![Reason::NeedsDecision {
                    action: "deploy".into(),
                    suspended: false,
                }],
            }),
            payload: None,
            signal: None,
        });
        let text = why(&fx.entries[(q - 1) as usize]);
        assert!(text.contains("because    deploy is irreversible"), "{text}");
    }

    #[test]
    fn state_and_open_asks_follow_the_ledger() {
        let mut fx = Fx::new();
        fx.adopt(
            V,
            mandate(model(), vec![permit("deploy", 1, false, false)], &[]),
        );
        assert_eq!(state(&fx.entries, &fx.adopted, V), State::Running);
        let q = fx.push(ask(Claim::Inferred, "deploy"));
        assert_eq!(state(&fx.entries, &fx.adopted, V), State::Asking);
        assert_eq!(open_asks(&fx.entries), vec![q]);
        fx.push(decision(
            Claim::Inferred,
            Verdict::Deny {
                ask: q,
                reason: "model says no".into(),
            },
        ));
        assert_eq!(
            open_asks(&fx.entries),
            vec![q],
            "an inferred decision does not answer an ask"
        );
        fx.push(decision(
            Claim::Declared,
            Verdict::Deny {
                ask: q,
                reason: "no".into(),
            },
        ));
        assert_eq!(open_asks(&fx.entries), Vec::<Seq>::new());
        assert_eq!(state(&fx.entries, &fx.adopted, V), State::Running);
        fx.push(decision(Claim::Declared, Verdict::Pause));
        let q2 = fx.push(ask(Claim::Inferred, "deploy"));
        assert_eq!(
            state(&fx.entries, &fx.adopted, V),
            State::Paused,
            "paused wins over asking"
        );
        assert_eq!(open_asks(&fx.entries), vec![q2]);
        assert_eq!(state(&fx.entries, &fx.adopted, "other"), State::Running);
    }

    #[test]
    fn proof_round_trips_through_json() {
        let mut fx = Fx::new();
        fx.adopt(
            V,
            mandate(
                above(10.0, "reset"),
                vec![permit("reset", 3, true, false)],
                &[],
            ),
        );
        let r = fx.push(report(Claim::Measured, 12.0));
        let proof = fx.gate(V, "reset", &[r]).unwrap();
        let json = serde_json::to_string(&proof).unwrap();
        let back: Proof = serde_json::from_str(&json).unwrap();
        assert_eq!(back, proof);
    }

    fn takes(name: &str, takes: Takes, reversible: bool, delegated: bool) -> Permitted {
        let mut p = permit(name, 3, reversible, delegated);
        p.takes = Some(takes);
        p
    }

    #[test]
    fn payload_lever_is_refused_on_a_proposal_alone() {
        let mut fx = Fx::new();
        fx.adopt(
            V,
            mandate(
                model(),
                vec![takes("announce", Takes::Text, true, false)],
                &[],
            ),
        );
        let s = fx.push(proposal_with(Claim::Inferred, "announce", text("Hello")));
        let refusal = fx.gate(V, "announce", &[s]).unwrap_err();
        assert!(
            reasons(&refusal).contains(
                &"announce is irreversible and no declared decision approves it".to_string()
            ) || refusal
                .reasons
                .iter()
                .any(|r| matches!(r, Reason::NeedsDecision { .. })),
            "{refusal}"
        );
        assert!(
            refusal
                .to_string()
                .contains("payload    #2  inferred  text \"Hello\""),
            "{refusal}"
        );
        // Without any payload at all the refusal says so too.
        let s = fx.push(proposal(Claim::Inferred, "announce"));
        let refusal = fx.gate(V, "announce", &[s]).unwrap_err();
        assert!(reasons(&refusal)
            .contains(&"announce takes text and nothing cited carries one".to_string()));
    }

    #[test]
    fn payload_lever_runs_from_a_declared_approval_citing_the_ask() {
        let mut fx = Fx::new();
        fx.adopt(
            V,
            mandate(
                model(),
                vec![takes("announce", Takes::Text, true, false)],
                &[],
            ),
        );
        let s = fx.push(proposal_with(Claim::Inferred, "announce", text("Hello")));
        let q = fx.push(ask_with(Claim::Inferred, "announce", text("Hello")));
        let d = fx.push(decision(Claim::Declared, approve(q)));
        let proof = fx.gate(V, "announce", &[d]).expect("approved");
        assert_eq!(proof.decision(), Some(d));
        assert_eq!(proof.payload_from(), Some(q), "the ask carries the payload");
        let text = proof.to_string();
        assert!(
            text.contains(&format!("decision   #{d}  declared  approved ask #{q}")),
            "{text}"
        );
        assert!(
            text.contains(&format!("payload    #{q}  inferred  text \"Hello\"")),
            "{text}"
        );
        // The proposal alone still does not pass.
        assert!(fx.gate(V, "announce", &[s]).is_err());
        // An inferred approval does not either.
        let fake = fx.push(decision(Claim::Inferred, approve(q)));
        assert!(fx.gate(V, "announce", &[fake]).is_err());
    }

    #[test]
    fn an_amended_payload_on_the_approval_is_the_one_cited() {
        let mut fx = Fx::new();
        fx.adopt(
            V,
            mandate(
                model(),
                vec![takes("announce", Takes::Text, true, false)],
                &[],
            ),
        );
        fx.push(proposal_with(Claim::Inferred, "announce", text("Hello")));
        let q = fx.push(ask_with(Claim::Inferred, "announce", text("Hello")));
        let d = fx.push(decision(
            Claim::Declared,
            Verdict::Approve {
                ask: q,
                payload: Some(text("Hello, amended")),
            },
        ));
        let proof = fx.gate(V, "announce", &[d]).expect("approved");
        assert_eq!(proof.payload_from(), Some(d));
        assert!(
            proof.to_string().contains(&format!(
                "payload    #{d}  declared  text \"Hello, amended\""
            )),
            "{proof}"
        );
    }

    #[test]
    fn a_human_proposal_proves_and_a_wrong_shape_does_not() {
        let mut fx = Fx::new();
        fx.adopt(
            V,
            mandate(
                model(),
                vec![takes("announce", Takes::Text, true, false)],
                &[],
            ),
        );
        let s = fx.push(proposal_with(Claim::Declared, "announce", text("Hello")));
        let proof = fx.gate(V, "announce", &[s]).expect("a human filed it");
        assert_eq!(proof.payload_from(), Some(s));
        assert!(
            proof
                .to_string()
                .contains(&format!("occasion   #{s}  declared  proposal")),
            "{proof}"
        );
        // A declared proposal with the wrong shape of payload is refused.
        let wrong = fx.push(proposal_with(
            Claim::Declared,
            "announce",
            Payload::Json {
                json: serde_json::json!({"to": "all"}),
            },
        ));
        let refusal = fx.gate(V, "announce", &[wrong]).unwrap_err();
        assert!(reasons(&refusal).contains(&"announce takes text, not json".to_string()));
        // An irreversible lever still asks even when a human proposed it.
        let mut fx = Fx::new();
        fx.adopt(
            V,
            mandate(model(), vec![takes("send", Takes::Text, false, false)], &[]),
        );
        let s = fx.push(proposal_with(Claim::Declared, "send", text("Hello")));
        let refusal = fx.gate(V, "send", &[s]).unwrap_err();
        assert!(refusal
            .reasons
            .iter()
            .any(|r| matches!(r, Reason::NeedsDecision { .. })));
    }

    #[test]
    fn a_delegated_payload_lever_runs_on_a_proposal() {
        let mut fx = Fx::new();
        fx.adopt(
            V,
            mandate(
                model(),
                vec![takes("announce", Takes::Text, true, true)],
                &[],
            ),
        );
        let s = fx.push(proposal_with(Claim::Inferred, "announce", text("Hello")));
        let proof = fx.gate(V, "announce", &[s]).expect("delegated");
        assert_eq!(proof.payload_from(), Some(s));
        assert!(
            proof
                .to_string()
                .contains("reversible, delegated, takes text"),
            "{proof}"
        );
    }

    /// A parent with one child, both adopted and agreeing.
    fn family(fx: &mut Fx, parent_levers: Vec<Permitted>, child: Mandate) -> (Seq, Seq) {
        let mut pm = mandate(model(), parent_levers, &[]);
        pm.children = vec![Child::named(V)];
        let p = fx.adopt("portfolio", pm);
        let mut cm = child;
        cm.parent = Some("portfolio".to_string());
        let c = fx.adopt(V, cm);
        (p, c)
    }

    fn parent_action(parent: &str, lever: &str, text: &str) -> Record {
        Record::Action {
            claim: Claim::Measured,
            venture: parent.to_string(),
            action: lever.to_string(),
            proof: Proof {
                venture: parent.to_string(),
                action: lever.to_string(),
                steps: vec![],
            },
            ok: true,
            detail: String::new(),
            payload: Some(self::text(text)),
        }
    }

    fn parent_lever(name: &str, delegated: bool) -> Permitted {
        let mut p = permit(name, 9, true, delegated);
        p.run = None;
        p
    }

    #[test]
    fn a_child_paused_by_its_parent_cannot_act_until_resumed() {
        let mut fx = Fx::new();
        family(
            &mut fx,
            vec![parent_lever("pause", true), parent_lever("resume", false)],
            mandate(
                above(10.0, "reset"),
                vec![permit("reset", 3, true, false)],
                &[],
            ),
        );
        let r = fx.push(report(Claim::Measured, 12.0));
        assert!(fx.gate(V, "reset", &[r]).is_ok());
        fx.push(parent_action("portfolio", "pause", V));
        let r = fx.push(report(Claim::Measured, 12.0));
        let refusal = fx.gate(V, "reset", &[r]).unwrap_err();
        assert!(
            refusal
                .reasons
                .contains(&Reason::Paused { venture: V.into() }),
            "{refusal}"
        );
        assert_eq!(state(&fx.entries, &fx.adopted, V), State::Paused);
        // A stranger's pause does nothing: it is not this child's parent.
        let mut fx2 = Fx::new();
        fx2.adopt(
            V,
            mandate(
                above(10.0, "reset"),
                vec![permit("reset", 3, true, false)],
                &[],
            ),
        );
        fx2.push(parent_action("stranger", "pause", V));
        let r = fx2.push(report(Claim::Measured, 12.0));
        assert!(fx2.gate(V, "reset", &[r]).is_ok());
        // The parent's resume needs a declared decision; then the child acts again.
        let s = fx.push(Record::Proposal {
            cites: vec![],
            claim: Claim::Inferred,
            venture: "portfolio".into(),
            action: "resume".into(),
            reason: "it is fine now".into(),
            payload: Some(text(V)),
        });
        let refusal = fx.gate("portfolio", "resume", &[s]).unwrap_err();
        assert!(
            refusal
                .reasons
                .iter()
                .any(|r| matches!(r, Reason::NeedsDecision { .. })),
            "{refusal}"
        );
        let q = fx.push(Record::Ask {
            cites: vec![],
            claim: Claim::Inferred,
            venture: "portfolio".into(),
            action: Some("resume".into()),
            question: "resume?".into(),
            refusal: None,
            signal: None,
            payload: Some(text(V)),
        });
        let d = fx.push(Record::Decision {
            claim: Claim::Declared,
            venture: "portfolio".into(),
            verdict: approve(q),
            by: None,
        });
        let proof = fx.gate("portfolio", "resume", &[d]).expect("declared");
        assert!(proof.to_string().contains("decision"), "{proof}");
        fx.push(parent_action("portfolio", "resume", V));
        assert_eq!(state(&fx.entries, &fx.adopted, V), State::Running);
        let r = fx.push(report(Claim::Measured, 12.0));
        assert!(fx.gate(V, "reset", &[r]).is_ok());
    }

    #[test]
    fn a_budget_set_by_the_parent_bounds_what_the_child_spends() {
        let mut fx = Fx::new();
        let mut spend = permit("spend", 9, true, true);
        spend.cost = 10;
        family(
            &mut fx,
            vec![parent_lever("budget", true)],
            mandate(model(), vec![spend], &[]),
        );
        let s = fx.push(proposal(Claim::Inferred, "spend"));
        assert!(fx.gate(V, "spend", &[s]).is_ok(), "no budget set: no bound");
        let b = fx.push(parent_action("portfolio", "budget", &format!("{V} 15")));
        let s = fx.push(proposal(Claim::Inferred, "spend"));
        let proof = fx.gate(V, "spend", &[s]).expect("under budget");
        assert!(
            proof.to_string().contains(&format!(
                "budget     0 of 15 a day spent  set by the parent in #{b}"
            )),
            "{proof}"
        );
        assert!(proof.to_string().contains("costs 10"), "{proof}");
        fx.push(done("spend"));
        let s = fx.push(proposal(Claim::Inferred, "spend"));
        let refusal = fx.gate(V, "spend", &[s]).unwrap_err();
        assert!(
            reasons(&refusal).contains(
                &"the parent's budget is 15 a day, 10 is spent and this costs 10".to_string()
            ),
            "{refusal}"
        );
        // A bigger budget lets it through again.
        fx.push(parent_action("portfolio", "budget", &format!("{V} 30")));
        let s = fx.push(proposal(Claim::Inferred, "spend"));
        assert!(fx.gate(V, "spend", &[s]).is_ok());
    }

    /// A parent with two children, shop and farm, and the terms it lists
    /// them under.
    fn siblings(fx: &mut Fx, shop: Child, farm: Child, allocation: Option<Allocation>) {
        let mut pm = mandate(model(), vec![parent_lever("budget", true)], &[]);
        pm.children = vec![shop, farm];
        pm.allocation = allocation;
        fx.adopt("portfolio", pm);
        for name in ["shop", "farm"] {
            let mut cm = mandate(model(), vec![], &[]);
            cm.parent = Some("portfolio".to_string());
            fx.adopt(name, cm);
        }
    }

    fn term(name: &str, priority: i32, floor: u32) -> Child {
        Child {
            name: name.to_string(),
            priority,
            floor,
        }
    }

    fn set_budget(fx: &mut Fx, child: &str, amount: u32) {
        fx.push(parent_action(
            "portfolio",
            "budget",
            &format!("{child} {amount}"),
        ));
    }

    /// The parent proposes a lever with a payload; the gate decides.
    fn parent_gate(fx: &mut Fx, lever: &str, payload: &str) -> Result<Proof, Refusal> {
        let s = fx.push(Record::Proposal {
            cites: vec![],
            claim: Claim::Inferred,
            venture: "portfolio".into(),
            action: lever.into(),
            reason: "it seemed right".into(),
            payload: Some(text(payload)),
        });
        fx.gate("portfolio", lever, &[s])
    }

    fn has(r: &Refusal, f: impl Fn(&Reason) -> bool) -> bool {
        r.reasons.iter().any(f)
    }

    #[test]
    fn a_budget_below_the_floor_is_refused_even_with_a_declared_approval() {
        let mut fx = Fx::new();
        siblings(&mut fx, term("shop", 0, 3), Child::named("farm"), None);
        let refusal = parent_gate(&mut fx, "budget", "shop 2").unwrap_err();
        assert!(
            has(
                &refusal,
                |r| matches!(r, Reason::BelowFloor { child, budget: 2, floor: 3 } if child == "shop")
            ),
            "{refusal}"
        );
        assert!(
            reasons(&refusal).contains(&"a budget of 2 is below shop's floor of 3".to_string()),
            "{refusal}"
        );
        // A human approves the ask. The floor still holds.
        let q = fx.push(Record::Ask {
            cites: vec![],
            claim: Claim::Inferred,
            venture: "portfolio".into(),
            action: Some("budget".into()),
            question: "budget shop 2?".into(),
            refusal: None,
            signal: None,
            payload: Some(text("shop 2")),
        });
        let d = fx.push(Record::Decision {
            claim: Claim::Declared,
            venture: "portfolio".into(),
            verdict: approve(q),
            by: None,
        });
        let refusal = fx.gate("portfolio", "budget", &[d]).unwrap_err();
        assert!(
            has(&refusal, |r| matches!(r, Reason::BelowFloor { .. })),
            "{refusal}"
        );
        // At the floor, and for a child with no floor, it passes.
        assert!(parent_gate(&mut fx, "budget", "shop 3").is_ok());
        assert!(parent_gate(&mut fx, "budget", "farm 0").is_ok());
    }

    #[test]
    fn a_high_priority_child_is_not_cut_while_a_lower_one_is_above_its_floor() {
        let mut fx = Fx::new();
        siblings(&mut fx, term("shop", 2, 0), term("farm", 0, 1), None);
        set_budget(&mut fx, "shop", 10);
        set_budget(&mut fx, "farm", 5);
        let refusal = parent_gate(&mut fx, "budget", "shop 8").unwrap_err();
        assert!(
            has(
                &refusal,
                |r| matches!(r, Reason::Priority { child, spared, budget: 5, floor: 1 } if child == "shop" && spared == "farm")
            ),
            "{refusal}"
        );
        // Raising the high-priority child is no cut; cutting the low one is
        // allowed, down to its floor.
        assert!(parent_gate(&mut fx, "budget", "shop 12").is_ok());
        assert!(parent_gate(&mut fx, "budget", "farm 1").is_ok());
        // With the farm at its floor, the shop may be cut.
        set_budget(&mut fx, "farm", 1);
        assert!(parent_gate(&mut fx, "budget", "shop 8").is_ok());
    }

    fn allocation(deadband: u32, hold: Duration) -> Option<Allocation> {
        Some(Allocation { deadband, hold })
    }

    #[test]
    fn a_budget_change_inside_the_deadband_is_refused() {
        let mut fx = Fx::new();
        siblings(
            &mut fx,
            Child::named("shop"),
            Child::named("farm"),
            allocation(3, Duration::ZERO),
        );
        // The first budget is no change.
        assert!(parent_gate(&mut fx, "budget", "shop 10").is_ok());
        set_budget(&mut fx, "shop", 10);
        for n in [9, 10, 12] {
            let refusal = parent_gate(&mut fx, "budget", &format!("shop {n}")).unwrap_err();
            assert!(
                has(&refusal, |r| matches!(
                    r,
                    Reason::InDeadband { deadband: 3, .. }
                )),
                "{n}: {refusal}"
            );
        }
        assert!(parent_gate(&mut fx, "budget", "shop 13").is_ok());
        assert!(parent_gate(&mut fx, "budget", "shop 7").is_ok());
    }

    #[test]
    fn reversing_a_budget_inside_the_hold_is_refused() {
        let mut fx = Fx::new();
        siblings(
            &mut fx,
            Child::named("shop"),
            Child::named("farm"),
            allocation(1, Duration::from_secs(DAY as u64)),
        );
        set_budget(&mut fx, "shop", 3);
        set_budget(&mut fx, "shop", 6);
        // 3 -> 6 -> 3 inside the hold.
        let refusal = parent_gate(&mut fx, "budget", "shop 3").unwrap_err();
        assert!(
            has(
                &refusal,
                |r| matches!(r, Reason::Reversal { child, .. } if child == "shop")
            ),
            "{refusal}"
        );
        // Going on the same way is no reversal.
        assert!(parent_gate(&mut fx, "budget", "shop 8").is_ok());
        // The other child has no history.
        assert!(parent_gate(&mut fx, "budget", "farm 1").is_ok());

        // Once the hold has passed, the reverse is allowed.
        let mut fx = Fx::new();
        siblings(
            &mut fx,
            Child::named("shop"),
            Child::named("farm"),
            allocation(1, Duration::from_secs(DAY as u64)),
        );
        let long_ago = NOW - 2 * DAY;
        for amount in [3, 6] {
            fx.push_at(
                long_ago,
                parent_action("portfolio", "budget", &format!("shop {amount}")),
            );
        }
        assert!(parent_gate(&mut fx, "budget", "shop 3").is_ok());
    }

    #[test]
    fn a_parent_lever_naming_a_non_child_is_refused_and_shown_in_why() {
        let mut fx = Fx::new();
        let mut pm = mandate(
            model(),
            vec![
                parent_lever("pause", true),
                parent_lever("budget", true),
                parent_lever("threshold", true),
            ],
            &[],
        );
        pm.children = vec![Child::named("shop")];
        fx.adopt("portfolio", pm);
        let mut sm = mandate(model(), vec![], &[]);
        sm.parent = Some("portfolio".to_string());
        fx.adopt("shop", sm);
        fx.adopt(SELF, mandate(model(), vec![], &[]));
        fx.adopt("stranger", mandate(model(), vec![], &[]));
        for (lever, payload) in [
            ("pause", "self"),
            ("pause", "stranger"),
            ("pause", "nobody"),
            ("budget", "self 5"),
            ("threshold", "stranger above 3"),
        ] {
            let refusal = parent_gate(&mut fx, lever, payload).unwrap_err();
            let child = payload.split_whitespace().next().unwrap();
            assert!(
                has(
                    &refusal,
                    |r| matches!(r, Reason::NotAChild { venture, child: c } if venture == "portfolio" && c == child)
                ),
                "{lever} {payload}: {refusal}"
            );
            let shown = why(&Entry::new(
                1,
                NOW,
                GENESIS,
                Record::Ask {
                    cites: vec![],
                    claim: Claim::Measured,
                    venture: "portfolio".into(),
                    action: Some(lever.into()),
                    question: String::new(),
                    refusal: Some(refusal),
                    signal: None,
                    payload: None,
                },
            ));
            assert!(
                shown.contains(&format!(
                    "because    {child} is not a child of portfolio: both mandates must say so"
                )),
                "{shown}"
            );
        }
        // A child is a child.
        assert!(parent_gate(&mut fx, "pause", "shop").is_ok());
        // A child only the parent lists is not one: both mandates must say so.
        let mut fx = Fx::new();
        let mut pm = mandate(model(), vec![parent_lever("pause", true)], &[]);
        pm.children = vec![Child::named("shop")];
        fx.adopt("portfolio", pm);
        fx.adopt("shop", mandate(model(), vec![], &[]));
        let refusal = parent_gate(&mut fx, "pause", "shop").unwrap_err();
        assert!(
            has(&refusal, |r| matches!(r, Reason::NotAChild { .. })),
            "{refusal}"
        );
    }

    #[test]
    fn the_parent_may_move_the_childs_threshold() {
        let mut fx = Fx::new();
        family(
            &mut fx,
            vec![parent_lever("threshold", true)],
            mandate(
                above(30.0, "vent"),
                vec![permit("vent", 9, true, false)],
                &[],
            ),
        );
        let r = fx.push(report(Claim::Measured, 27.0));
        assert!(fx.gate(V, "vent", &[r]).is_err());
        let t = fx.push(parent_action(
            "portfolio",
            "threshold",
            &format!("{V} above 25"),
        ));
        assert_eq!(
            effective_threshold(&fx.entries, &fx.adopted, V),
            Some(Threshold {
                above: true,
                bound: 25.0,
                action: "vent".into(),
                from: Some(t)
            })
        );
        let r = fx.push(report(Claim::Measured, 27.0));
        let proof = fx.gate(V, "vent", &[r]).expect("over the parent's bound");
        assert!(
            proof.to_string().contains(&format!(
                "threshold  count above 25  set by the parent in #{t}"
            )),
            "{proof}"
        );
    }

    #[test]
    fn shared_quotas_refuse_before_and_inside_the_gate() {
        let mut fx = Fx::new();
        let mut costly = permit("buy", 9, true, true);
        costly.cost = 10;
        let mut mail = permit("mail", 9, false, true);
        mail.run = None;
        mail.adapter = Some("mail.send me@example.com".to_string());
        fx.adopt(V, mandate(model(), vec![costly, mail], &[]));
        let quotas = Quotas {
            asks: Some(Cap {
                count: 2,
                per: Cap::DAY,
            }),
            spend: Some(Cap {
                count: 15,
                per: Cap::DAY,
            }),
            sends: Some(Cap {
                count: 1,
                per: Cap::DAY,
            }),
        };
        let s = fx.push(proposal(Claim::Inferred, "buy"));
        let candidate = |action: &str, cites: Vec<Seq>| Candidate {
            venture: V.into(),
            action: action.into(),
            cites,
        };
        let proof = gate_with(
            &candidate("buy", vec![s]),
            &fx.entries,
            &fx.adopted,
            &quotas,
            NOW,
        )
        .unwrap();
        assert!(
            proof
                .to_string()
                .contains("quota      spend 15/day, 0 used"),
            "{proof}"
        );
        fx.push(done("buy"));
        let s = fx.push(proposal(Claim::Inferred, "buy"));
        let refusal = gate_with(
            &candidate("buy", vec![s]),
            &fx.entries,
            &fx.adopted,
            &quotas,
            NOW,
        )
        .unwrap_err();
        assert!(
            reasons(&refusal)
                .contains(&"the spend quota is 15/day and 10 are already used".to_string()),
            "{refusal}"
        );
        let (steps, why) = quota_check(&fx.entries, &fx.adopted, &quotas, V, "buy", NOW);
        assert_eq!(steps.len(), 1);
        assert_eq!(why.len(), 1);
        // Sends: one mail is fine, the second is one too many.
        let s = fx.push(proposal_with(Claim::Inferred, "mail", text("hi")));
        assert!(gate_with(
            &candidate("mail", vec![s]),
            &fx.entries,
            &fx.adopted,
            &quotas,
            NOW
        )
        .is_ok());
        fx.push(Record::Intent {
            claim: Claim::Measured,
            venture: V.into(),
            action: "mail".into(),
            proof: Proof {
                venture: V.into(),
                action: "mail".into(),
                steps: vec![],
            },
            payload: Some(text("hi")),
            key: None,
        });
        let i = fx.entries.len() as Seq;
        fx.push(Record::Outcome {
            claim: Claim::Measured,
            venture: V.into(),
            intent: i,
            ok: true,
            detail: String::new(),
        });
        let s = fx.push(proposal_with(Claim::Inferred, "mail", text("again")));
        let refusal = gate_with(
            &candidate("mail", vec![s]),
            &fx.entries,
            &fx.adopted,
            &quotas,
            NOW,
        )
        .unwrap_err();
        assert!(
            reasons(&refusal)
                .contains(&"the sends quota is 1/day and 1 are already used".to_string()),
            "{refusal}"
        );
        assert!(
            refusal
                .to_string()
                .contains("quota      sends 1/day, 1 used"),
            "{refusal}"
        );
        // Asks are counted for the kernel.
        fx.push(ask(Claim::Inferred, "mail"));
        assert_eq!(asks_used(&fx.entries, quotas.asks.unwrap(), NOW), 1);
        // Without quotas nothing of this applies.
        let s = fx.push(proposal(Claim::Inferred, "buy"));
        assert!(fx.gate(V, "buy", &[s]).is_ok());
    }

    #[test]
    fn current_facts_follow_time_for_measured_and_citation_for_the_rest() {
        let mut fx = Fx::new();
        let a = fx.push(report(Claim::Measured, 1.0));
        let b = fx.push(report(Claim::Measured, 2.0));
        let finding = |note: &str, cites: Vec<Seq>| Record::Report {
            claim: Claim::Inferred,
            venture: V.into(),
            metric: "pricing".into(),
            value: None,
            note: note.into(),
            source: None,
            confidence: None,
            cites,
            implausible: None,
            facts: None,
        };
        let f1 = fx.push(finding("rivals charge 9", vec![]));
        let f2 = fx.push(finding("rivals charge 8", vec![]));
        let f3 = fx.push(finding("rivals charge 7, superseding", vec![f1]));
        let summary = fx.push(Record::Report {
            claim: Claim::Inferred,
            venture: V.into(),
            metric: "summary".into(),
            value: None,
            note: "all of it".into(),
            source: None,
            confidence: None,
            cites: vec![a, b, f1, f2],
            implausible: None,
            facts: None,
        });
        let current: Vec<Seq> = current_facts(&fx.entries)
            .into_iter()
            .map(|(s, _, _)| s)
            .collect();
        assert!(
            !current.contains(&a),
            "an older reading is superseded by a newer one"
        );
        assert!(current.contains(&b));
        assert!(
            !current.contains(&f1),
            "cited by a later fact on the same topic"
        );
        assert!(current.contains(&f2), "a finding is not superseded by time");
        assert!(current.contains(&f3));
        assert!(
            current.contains(&summary),
            "a summary supersedes no leaf: it has its own topic"
        );
        assert_eq!(current, vec![b, f2, f3, summary]);
    }

    #[test]
    fn delegate_runs_only_from_a_declared_approval_of_an_ask_code_wrote() {
        let mut fx = Fx::new();
        fx.adopt(
            V,
            mandate(
                model(),
                vec![takes("announce", Takes::Text, true, false)],
                &[],
            ),
        );
        let new_text = text("purpose: ...\nmetric: count\n");
        let s = fx.push(Record::Proposal {
            cites: vec![],
            claim: Claim::Measured,
            venture: V.into(),
            action: DELEGATE.into(),
            reason: "announce was approved 5 times".into(),
            payload: Some(new_text.clone()),
        });
        let refusal = fx.gate(V, DELEGATE, &[s]).unwrap_err();
        assert!(
            refusal
                .reasons
                .iter()
                .any(|r| matches!(r, Reason::NeedsDecision { .. })),
            "{refusal}"
        );
        assert!(
            refusal
                .to_string()
                .contains("axiom      a changed mandate is adopted only by a declared decision"),
            "{refusal}"
        );
        let q = fx.push(Record::Ask {
            cites: vec![],
            claim: Claim::Measured,
            venture: V.into(),
            action: Some(DELEGATE.into()),
            question: "delegate announce?".into(),
            refusal: None,
            signal: None,
            payload: Some(new_text.clone()),
        });
        let fake = fx.push(decision(Claim::Inferred, approve(q)));
        assert!(
            fx.gate(V, DELEGATE, &[fake]).is_err(),
            "an inferred approval is nothing"
        );
        let d = fx.push(decision(Claim::Declared, approve(q)));
        let proof = fx.gate(V, DELEGATE, &[d]).expect("declared");
        assert_eq!(proof.decision(), Some(d));
        assert_eq!(proof.payload_from(), Some(q));
        assert!(
            proof
                .to_string()
                .contains(&format!("payload    #{q}  measured  text")),
            "{proof}"
        );
        // An ask a model wrote cannot carry a mandate change, approved or not.
        let mq = fx.push(Record::Ask {
            cites: vec![],
            claim: Claim::Inferred,
            venture: V.into(),
            action: Some(DELEGATE.into()),
            question: "delegate everything?".into(),
            refusal: None,
            signal: None,
            payload: Some(new_text.clone()),
        });
        let md = fx.push(decision(Claim::Declared, approve(mq)));
        let refusal = fx.gate(V, DELEGATE, &[md]).unwrap_err();
        assert!(
            refusal
                .reasons
                .iter()
                .any(|r| matches!(r, Reason::NotJustified { .. })),
            "{refusal}"
        );
        // Self can never have its mandate changed this way.
        let mut fx = Fx::new();
        fx.adopt(SELF, mandate(model(), vec![], &[]));
        let q = fx.push(Record::Ask {
            cites: vec![],
            claim: Claim::Measured,
            venture: SELF.into(),
            action: Some(DELEGATE.into()),
            question: "delegate?".into(),
            refusal: None,
            signal: None,
            payload: Some(new_text),
        });
        let d = fx.push(Record::Decision {
            claim: Claim::Declared,
            venture: SELF.into(),
            verdict: approve(q),
            by: None,
        });
        let refusal = fx.gate(SELF, DELEGATE, &[d]).unwrap_err();
        assert!(
            reasons(&refusal).contains(&"the self venture cannot edit its own mandate".to_string()),
            "{refusal}"
        );
    }
}
