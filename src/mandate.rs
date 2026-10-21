//! Mandates: what a venture is for, what number to watch, what it may do,
//! what it may not.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_yaml::{Mapping, Value};
use sha2::{Digest, Sha256};

pub use crate::record::Takes;

use crate::record::Payload;

use crate::adapter;

/// A parsed mandate.yaml.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mandate {
    /// What the venture is for, in the human's words.
    pub purpose: String,
    /// The name of the number the report prints, or a built-in metric;
    /// a shell metric may carry a label, which is the name reports bear.
    pub metric: MetricSpec,
    /// What decides when to act.
    pub controller: Controller,
    /// What the venture may do.
    #[serde(default)]
    pub permitted: Vec<Permitted>,
    /// What the venture may never do, by name.
    #[serde(default)]
    pub forbidden: Vec<String>,
    /// How often the report runs.
    #[serde(with = "duration")]
    pub cadence: Duration,
    /// How much of the ledger a model gets to see, in tokens.
    #[serde(default = "default_context_budget")]
    pub context_budget: u32,
    /// Past this many records since the last summary, the nightly call
    /// summarizes the older ones; the leaves stay in the ledger and leave
    /// the context.
    #[serde(default = "default_summarize_after")]
    pub summarize_after: u32,
    /// What to do when `run` dies.
    #[serde(default)]
    pub restart: Restart,
    /// The hosts the venture may reach: `none` for no network, a list for
    /// those hosts only, left out for no limit. Held to for adapters by the
    /// warden, and for `run` when it is `none`, by a network namespace
    /// where Linux allows one. Script levers are not held to it.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "network")]
    pub network: Option<Vec<String>>,
    /// The ventures this one allocates to. A parent sets their budgets and
    /// thresholds and pauses them; it never runs their levers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Child>,
    /// The venture that allocates to this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// How steadily a parent may move its children's budgets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allocation: Option<Allocation>,
    /// Readings the warden takes beside the metric; past its bound a
    /// guard pauses the venture or asks, and no lever but stop, and a
    /// tripped guard's declared unwind, passes until it is back inside.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub guards: Vec<Guard>,
    /// Readings that check the metric is not being gamed. A model never
    /// sees them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audits: Vec<Audit>,
    /// The values the metric can plausibly take, low and high.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<[f64; 2]>,
    /// The largest plausible change from one report to the next.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_step: Option<f64>,
    /// With max_step: a plausible report right after an implausible one
    /// is unconfirmed, never a premise, until the next agrees with it
    /// within max_step. False lets it act at once, where waiting costs
    /// more than a misreading.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub confirm: bool,
    /// A world model: a command run in the venture directory that steps
    /// the venture's world forward, a JSON state in and a JSON state with
    /// the metric out. What it says is inferred: it ranks, vetoes and
    /// asks, and never authorizes. Left out, the built-in baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub world: Option<String>,
    /// The CRPS skill score against persistence the world model's rolling
    /// mean must reach before its rankings may run; below it, they ask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration: Option<f64>,
    /// `offbox` lets the venture's escalation go to a backend that leaves
    /// the machine. Declared only: no proposal, ratchet or self change
    /// can add it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalate: Option<Escalate>,
    /// Watch the metric for critical slowing down: rising variance and
    /// rising lag-1 autocorrelation ask once per episode. Always on for a
    /// plan controller.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub early_warning: bool,
    /// Which way the target improves, declared: up or down. A plan's and
    /// an audit's `better` default to it. Capacity is checked against the
    /// threshold's side, else this; never guessed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub better: Option<Worse>,
}

/// Where an escalation may go beyond this machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Escalate {
    /// A backend that leaves the machine may answer for this venture.
    Offbox,
}

/// Does `after` give a venture the right to escalate off the box that
/// `before` did not? A change of mandate that does is never a proposal's to
/// make: the field is declared by a human editing the mandate and adding it.
pub fn adds_offbox(before: &Mandate, after: &Mandate) -> bool {
    after.escalate == Some(Escalate::Offbox) && before.escalate != Some(Escalate::Offbox)
}

/// Where `after` differs from `before` in anything but a lever turning
/// `delegated` from false to true: the path of the first field that
/// changed, like `permitted.announce.cap` or `guards.load`. None when the
/// only changes are the ones the improving loop may make. Parsed mandates
/// are compared, not their text.
pub fn edits_limits(before: &Mandate, after: &Mandate) -> Option<String> {
    let (Ok(b), Ok(a)) = (serde_json::to_value(before), serde_json::to_value(after)) else {
        return Some("mandate".to_string());
    };
    first_difference("", &b, &a)
}

fn first_difference(
    path: &str,
    before: &serde_json::Value,
    after: &serde_json::Value,
) -> Option<String> {
    use serde_json::Value;
    let at = |key: &str| {
        if path.is_empty() {
            key.to_string()
        } else {
            format!("{path}.{key}")
        }
    };
    match (before, after) {
        (Value::Object(b), Value::Object(a)) => {
            let mut keys: Vec<&String> = b.keys().chain(a.keys()).collect();
            keys.sort();
            keys.dedup();
            keys.into_iter().find_map(|k| {
                let (x, y) = (b.get(k), a.get(k));
                // A lever turning delegated on is the ratchet's one move:
                // `permitted.<lever>.delegated`, never the same word inside
                // a payload or anywhere else.
                let lever = path
                    .strip_prefix("permitted.")
                    .is_some_and(|name| !name.is_empty() && !name.contains('.'));
                if k == "delegated"
                    && lever
                    && x == Some(&Value::Bool(false))
                    && y == Some(&Value::Bool(true))
                {
                    return None;
                }
                match (x, y) {
                    (Some(x), Some(y)) => first_difference(&at(k), x, y),
                    _ => Some(at(k)),
                }
            })
        }
        (Value::Array(b), Value::Array(a)) => {
            let name = |v: &Value| v.get("name").and_then(Value::as_str).map(str::to_string);
            let named = b.iter().chain(a.iter()).all(|v| name(v).is_some());
            if !named {
                return (b != a).then(|| path.to_string());
            }
            if b.len() != a.len() {
                return Some(path.to_string());
            }
            // Order is part of the mandate; levers are matched by name.
            b.iter().zip(a.iter()).find_map(|(x, y)| {
                let n = name(x)?;
                if name(y).as_deref() != Some(n.as_str()) {
                    return Some(path.to_string());
                }
                first_difference(&format!("{path}.{n}"), x, y)
            })
        }
        _ => (before != after).then(|| path.to_string()),
    }
}

/// What a guard does past its bound.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Then {
    /// Stop the venture until a human resumes it.
    #[default]
    Pause,
    /// Ask once, and refuse every lever but stop while it is past.
    Ask,
}

/// A guard: a command or a built-in metric the warden reads, with a bound;
/// or the drawdown of the venture's own metric from its peak.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Guard {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub above: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub below: Option<f64>,
    #[serde(default)]
    pub then: Then,
    /// The fall from the peak, in percent, that trips the guard.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "percent")]
    pub drawdown: Option<f64>,
    /// The metric a drawdown is computed over: the venture's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub of: Option<String>,
    /// The values its readings can plausibly take, low and high.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<[f64; 2]>,
    /// The largest plausible change from one reading to the next.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_step: Option<f64>,
    /// With max_step: a reading right after an implausible one waits for
    /// the next to agree with it. False lets it count at once.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub confirm: bool,
    /// The one lever, with the one payload, the kernel runs when the guard
    /// trips: a delegation of exactly that payload, declared here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unwind: Option<Unwind>,
}

/// What a tripped guard runs: a permitted lever and the payload it takes,
/// declared in the mandate. It is the one lever besides stop a tripped
/// guard lets through, and only with exactly this payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Unwind {
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
}

impl Unwind {
    /// The payload as the lever takes it, checked against its takes and
    /// fields: None for a lever that takes none.
    pub fn payload_for(&self, perm: &Permitted) -> Result<Option<Payload>, String> {
        let lever = &self.action;
        let Some(takes) = perm.takes() else {
            return match &self.payload {
                None => Ok(None),
                Some(_) => Err(format!("{lever} takes no payload; leave payload out")),
            };
        };
        let Some(value) = &self.payload else {
            return Err(format!(
                "{lever} takes {takes}; give the payload it runs with"
            ));
        };
        let payload = match (takes, value) {
            (Takes::Json, serde_json::Value::Object(_)) => Payload::Json {
                json: value.clone(),
            },
            (Takes::Json, _) => {
                return Err(format!(
                    "{lever} takes json; the payload is a json object like {{side: sell, qty: 10}}"
                ))
            }
            (t, serde_json::Value::String(text)) => Payload::parse(t, text)?,
            (t, _) => return Err(format!("{lever} takes {t}; the payload is text")),
        };
        if payload.text().len() > Payload::MAX {
            return Err(format!("the payload is more than {} bytes", Payload::MAX));
        }
        if let (Some(fields), Payload::Json { json }) = (&perm.fields, &payload) {
            let empty = serde_json::Map::new();
            let object = json.as_object().unwrap_or(&empty);
            let mut problems = Vec::new();
            for (name, ty) in fields {
                match object.get(name) {
                    None => problems.push(format!("{name} is missing ({})", ty.as_str())),
                    Some(v) if !ty.holds(v) => {
                        problems.push(format!("{name} must be {}", ty.as_str()))
                    }
                    Some(_) => {}
                }
            }
            for name in object.keys().filter(|k| !fields.contains_key(*k)) {
                problems.push(format!("{name} is not a field of {lever}"));
            }
            if !problems.is_empty() {
                return Err(problems.join(", "));
            }
        }
        Ok(Some(payload))
    }
}

impl Guard {
    /// The metric its readings carry: `guard.<name>`.
    pub fn metric(&self) -> String {
        format!("guard.{}", self.name)
    }

    /// Whether the guard trips above its bound, and the bound. A drawdown
    /// trips at or above its percent.
    pub fn bound(&self) -> (bool, f64) {
        match (self.drawdown, self.above, self.below) {
            (Some(d), _, _) => (true, d),
            (_, Some(b), _) => (true, b),
            (_, _, Some(b)) => (false, b),
            _ => (true, f64::INFINITY),
        }
    }

    /// Is this reading past the bound?
    pub fn breached(&self, value: f64) -> bool {
        match self.bound() {
            _ if !value.is_finite() => false,
            (true, b) if self.drawdown.is_some() => value >= b,
            (true, b) => value > b,
            (false, b) => value < b,
        }
    }
}

/// A direction: which way an audit gets worse, or which way the target
/// improves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Worse {
    #[default]
    Up,
    Down,
}

/// An audit: a reading that should not get worse while the target gets
/// better.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Audit {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    pub worse: Worse,
    /// Which way the target improves: up, unless the metric is one where
    /// less is better, like unread mail.
    #[serde(default, skip_serializing_if = "is_up")]
    pub better: Worse,
    /// How far the audit may move the worse way over the window.
    #[serde(default)]
    pub tolerance: f64,
    /// How many audit readings the window spans, the latest included.
    #[serde(default = "default_window")]
    pub window: u32,
    /// The values its readings can plausibly take, low and high.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<[f64; 2]>,
    /// The largest plausible change from one reading to the next.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_step: Option<f64>,
    /// With max_step: a reading right after an implausible one waits for
    /// the next to agree with it. False lets it count at once.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub confirm: bool,
}

fn yes() -> bool {
    true
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_true(b: &bool) -> bool {
    *b
}

fn is_up(w: &Worse) -> bool {
    *w == Worse::Up
}

fn default_window() -> u32 {
    5
}

impl Audit {
    /// The metric its readings carry: `audit.<name>`.
    pub fn metric(&self) -> String {
        format!("audit.{}", self.name)
    }

    /// How much worse the audit got for a change of `delta`.
    pub fn worsened(&self, delta: f64) -> f64 {
        match self.worse {
            Worse::Up => delta,
            Worse::Down => -delta,
        }
    }

    /// How much the target improved for a change of `delta`.
    pub fn improved(&self, delta: f64) -> f64 {
        match self.better {
            Worse::Up => delta,
            Worse::Down => -delta,
        }
    }
}

/// A range is low then high, and a max_step is above 0. `who` prefixes
/// the message for a guard or an audit.
fn check_plausibility(
    who: &str,
    range: Option<[f64; 2]>,
    max_step: Option<f64>,
) -> Result<(), MandateError> {
    if let Some([lo, hi]) = range {
        if !lo.is_finite() || !hi.is_finite() || lo >= hi {
            return fail(format!(
                "{who}range is low then high, like [0, 1000], not [{lo}, {hi}]"
            ));
        }
    }
    if let Some(s) = max_step {
        if !s.is_finite() || s <= 0.0 {
            return fail(format!("{who}max_step must be a number above 0, not {s}"));
        }
    }
    Ok(())
}

/// The prefix of an audit's readings: never shown to a model.
pub const AUDIT_PREFIX: &str = "audit.";

/// `drawdown:` as `15%`.
pub mod percent {
    use super::*;

    pub fn serialize<S: Serializer>(p: &Option<f64>, s: S) -> Result<S::Ok, S::Error> {
        match p {
            Some(p) => s.serialize_str(&format!("{p}%")),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
        let Some(text) = Option::<String>::deserialize(d)? else {
            return Ok(None);
        };
        parse(&text).map(Some).map_err(serde::de::Error::custom)
    }

    pub fn parse(text: &str) -> Result<f64, String> {
        let n = text
            .trim()
            .strip_suffix('%')
            .and_then(|n| n.trim().parse::<f64>().ok())
            .ok_or_else(|| format!("drawdown is a percent like 15%, not {text:?}"))?;
        if !(n > 0.0 && n <= 100.0) {
            return Err(format!(
                "drawdown is a percent above 0% and at most 100%, not {text:?}"
            ));
        }
        Ok(n)
    }
}

/// A child as its parent lists it: a name, or a name with the order budgets
/// are cut in and the least a budget may be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Child {
    pub name: String,
    /// Higher is cut last: a child is not cut while a lower-priority
    /// sibling sits above its floor.
    pub priority: i32,
    /// No budget below this is set for the child.
    pub floor: u32,
}

impl Child {
    /// A child with no priority and no floor.
    pub fn named(name: &str) -> Child {
        Child {
            name: name.to_string(),
            priority: 0,
            floor: 0,
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ChildText {
    Name(String),
    Terms(ChildTerms),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChildTerms {
    name: String,
    #[serde(default)]
    priority: i32,
    #[serde(default)]
    floor: u32,
}

impl Serialize for Child {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if self.priority == 0 && self.floor == 0 {
            return s.serialize_str(&self.name);
        }
        ChildTerms {
            name: self.name.clone(),
            priority: self.priority,
            floor: self.floor,
        }
        .serialize(s)
    }
}

impl<'de> Deserialize<'de> for Child {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Child, D::Error> {
        Ok(match ChildText::deserialize(d)? {
            ChildText::Name(name) => Child::named(&name),
            ChildText::Terms(t) => Child {
                name: t.name,
                priority: t.priority,
                floor: t.floor,
            },
        })
    }
}

/// `allocation:` on a parent: the deadband and hold that keep it from
/// thrashing its children's budgets, and the total they may add up to.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Allocation {
    /// A budget change smaller than this is refused.
    #[serde(default)]
    pub deadband: u32,
    /// Inside this long after a child's budget moved, moving it back the
    /// other way is refused. Left out, no reversal is refused. With a
    /// total, a cut is never held: a budget past the total is undone at
    /// once.
    #[serde(default, with = "duration")]
    pub hold: Duration,
    /// The most the children's budgets may add up to, a child with none
    /// set counted at its floor. A budget that raises the sum past it is
    /// refused. Left out, no sum is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u32>,
}

/// `network:` as `none`, or a list of hosts.
pub mod network {
    use super::*;

    pub fn serialize<S: Serializer>(n: &Option<Vec<String>>, s: S) -> Result<S::Ok, S::Error> {
        match n {
            Some(hosts) if hosts.is_empty() => s.serialize_str("none"),
            Some(hosts) => hosts.serialize(s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<String>>, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Word(String),
            Hosts(Vec<String>),
        }
        Ok(match Option::<Raw>::deserialize(d)? {
            None => None,
            Some(Raw::Word(w)) if w.trim() == "none" => Some(vec![]),
            Some(Raw::Word(w)) => {
                return Err(serde::de::Error::custom(format!(
                    "network is none or a list of hosts, not {w:?}"
                )))
            }
            Some(Raw::Hosts(h)) => Some(h),
        })
    }
}

fn default_context_budget() -> u32 {
    8000
}

fn default_summarize_after() -> u32 {
    200
}

/// What decides when to act.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum Controller {
    /// Take `action` when the metric goes above or below a bound.
    /// Exactly one of `above` and `below` is set.
    Threshold {
        #[serde(default)]
        above: Option<f64>,
        #[serde(default)]
        below: Option<f64>,
        action: String,
        /// After acting, the metric must come back past the bound by this
        /// much before the threshold acts again.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deadband: Option<f64>,
        /// How many crossed reports in a row it takes to act.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hold: Option<u32>,
    },
    /// Hold the metric between `low` and `high`: run `below` when it is
    /// under `low` and `above` when it is over `high`. Each side is a
    /// threshold of its own, with the same deadband and hold.
    Band {
        low: f64,
        high: f64,
        below: String,
        above: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deadband: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hold: Option<u32>,
    },
    /// A backend picks one label: nothing, a permitted action, or ask.
    Classifier {
        #[serde(default)]
        backend: Option<String>,
    },
    /// A backend proposes an action or asks, with a reason.
    Model {
        #[serde(default)]
        backend: Option<String>,
    },
    /// Code searches lever sequences through the world model's ensemble
    /// and proposes the first lever of the best one by its worst member.
    /// Its branch and proposal are inferred, like a model controller's.
    Plan {
        /// How many steps of cadence a sequence looks ahead.
        #[serde(default = "default_horizon")]
        horizon: u32,
        /// The most levers a sequence pulls.
        #[serde(default = "default_depth")]
        depth: u32,
        /// How many ensemble members each sequence runs through.
        #[serde(default = "default_members")]
        members: u32,
        /// Reversible levers the planner may explore by Thompson sampling.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        explore: Vec<String>,
        /// The backend asked for payloads and when branches tie.
        #[serde(default)]
        backend: Option<String>,
        /// Which way the target improves.
        #[serde(default, skip_serializing_if = "is_up")]
        better: Worse,
    },
}

/// The planner's defaults: four steps ahead, two levers, eight members.
pub const DEFAULT_HORIZON: u32 = 4;
pub const DEFAULT_DEPTH: u32 = 2;
pub const DEFAULT_MEMBERS: u32 = 8;
/// The planner's bounds: what keeps a search small.
pub const MAX_HORIZON: u32 = 48;
pub const MAX_DEPTH: u32 = 3;
pub const MAX_MEMBERS: u32 = 64;

fn default_horizon() -> u32 {
    DEFAULT_HORIZON
}

fn default_depth() -> u32 {
    DEFAULT_DEPTH
}

fn default_members() -> u32 {
    DEFAULT_MEMBERS
}

/// An action the venture may take: a command of the venture's own, or a
/// built-in adapter.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Permitted {
    pub name: String,
    /// The command that runs, in the venture directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// The built-in action instead, like `github.issue owner/repo`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    /// How often it may run, like `3/day`.
    pub cap: Cap,
    /// Can it be undone?
    pub reversible: bool,
    /// Irreversible actions become asks unless the mandate delegates them.
    #[serde(default)]
    pub delegated: bool,
    /// The payload the lever consumes, if it takes one: text, json, diff
    /// or file. A lever that takes a payload runs a model's content only
    /// after a human approves it, unless the mandate delegates the lever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub takes: Option<Takes>,
    /// What one run spends, in whatever units the human counts in. Spend
    /// is held to the child's budget and to the shared quota.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cost: u32,
    /// A command the warden runs in the venture directory after the gate
    /// passes and before the intent is written, with the payload on
    /// standard input and `AVSM_MEASURED` naming a file of the venture's
    /// measured and declared reports. Exit 0 passes; anything else refuses
    /// the run, even a human-approved one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
    /// What a run should do to the venture's metric, like `up 5 within 3`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expects: Option<Expects>,
    /// On a delegated lever: this many misses in a row suspend the
    /// delegation until a human approves the ask it raises.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoke_after: Option<u32>,
    /// On a lever that takes json: the fields its payload must have, by
    /// name and type. The warden refuses a payload with a missing, extra
    /// or mistyped field before the check and the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<BTreeMap<String, FieldType>>,
}

/// What `metric:` says: a word, a built-in metric like `shell <cmd>`, or
/// `{shell: <cmd>, label: <name>}`, which is `shell <cmd>` whose reports
/// carry the label as the metric name.
#[derive(Clone, Debug, PartialEq)]
pub struct MetricSpec {
    spec: String,
    label: Option<String>,
}

impl MetricSpec {
    pub fn labelled(command: &str, label: &str) -> MetricSpec {
        MetricSpec {
            spec: format!("shell {}", command.trim()),
            label: Some(label.to_string()),
        }
    }

    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }
}

impl From<&str> for MetricSpec {
    fn from(spec: &str) -> MetricSpec {
        MetricSpec {
            spec: spec.to_string(),
            label: None,
        }
    }
}

impl From<String> for MetricSpec {
    fn from(spec: String) -> MetricSpec {
        MetricSpec { spec, label: None }
    }
}

impl std::ops::Deref for MetricSpec {
    type Target = str;
    fn deref(&self) -> &str {
        &self.spec
    }
}

impl fmt::Display for MetricSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.spec)
    }
}

impl PartialEq<&str> for MetricSpec {
    fn eq(&self, other: &&str) -> bool {
        self.label.is_none() && self.spec == *other
    }
}

impl Serialize for MetricSpec {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match &self.label {
            None => s.serialize_str(&self.spec),
            Some(label) => {
                let mut map = s.serialize_map(Some(2))?;
                map.serialize_entry(
                    "shell",
                    self.spec.strip_prefix("shell ").unwrap_or(&self.spec),
                )?;
                map.serialize_entry("label", label)?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for MetricSpec {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<MetricSpec, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Text(String),
            Labelled { shell: String, label: String },
        }
        Ok(match Raw::deserialize(d)? {
            Raw::Text(spec) => MetricSpec::from(spec),
            Raw::Labelled { shell, label } => MetricSpec::labelled(&shell, &label),
        })
    }
}

/// The type of one field of a json payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    String,
    Number,
    Integer,
    Bool,
}

impl FieldType {
    pub const ALL: &'static str = "string, number, integer or bool";

    pub fn as_str(self) -> &'static str {
        match self {
            FieldType::String => "string",
            FieldType::Number => "number",
            FieldType::Integer => "integer",
            FieldType::Bool => "bool",
        }
    }

    /// Is this value of that type?
    pub fn holds(self, v: &serde_json::Value) -> bool {
        match self {
            FieldType::String => v.is_string(),
            FieldType::Number => v.is_number(),
            FieldType::Integer => v.is_i64() || v.is_u64(),
            FieldType::Bool => v.is_boolean(),
        }
    }
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// At most `count` times per `per`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Cap {
    pub count: u32,
    pub per: Duration,
}

impl Cap {
    pub const HOUR: Duration = Duration::from_secs(60 * 60);
    pub const DAY: Duration = Duration::from_secs(24 * 60 * 60);
    pub const WEEK: Duration = Duration::from_secs(7 * 24 * 60 * 60);
}

impl fmt::Display for Cap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let per = if self.per == Cap::HOUR {
            "hour".to_string()
        } else if self.per == Cap::DAY {
            "day".to_string()
        } else if self.per == Cap::WEEK {
            "week".to_string()
        } else {
            humantime::format_duration(self.per).to_string()
        };
        write!(f, "{}/{}", self.count, per)
    }
}

impl FromStr for Cap {
    type Err = String;

    fn from_str(s: &str) -> Result<Cap, String> {
        let s = s.trim();
        let (count, per) = s
            .split_once('/')
            .ok_or_else(|| format!("a cap looks like 3/day or 1/hour, not {s:?}"))?;
        let count: u32 = count
            .trim()
            .parse()
            .map_err(|_| format!("a cap starts with a whole number, not {:?}", count.trim()))?;
        if count == 0 {
            return Err("a cap of 0 permits nothing; leave the action out instead".to_string());
        }
        let per = match per.trim() {
            "hour" | "h" => Cap::HOUR,
            "day" | "d" => Cap::DAY,
            "week" | "w" => Cap::WEEK,
            other => humantime::parse_duration(other)
                .map_err(|_| format!("a cap ends in hour, day, or week, not {other:?}"))?,
        };
        Ok(Cap { count, per })
    }
}

impl Serialize for Cap {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Cap {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Cap, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Which way a run should move the metric.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
}

/// What a run should do to the venture's metric: `up 5 within 3` or
/// `down within 2`. The size is optional; `within` counts reports.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Expects {
    pub direction: Direction,
    pub size: Option<f64>,
    pub within: u32,
}

impl fmt::Display for Expects {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let dir = match self.direction {
            Direction::Up => "up",
            Direction::Down => "down",
        };
        match self.size {
            Some(size) => write!(f, "{dir} {size} within {}", self.within),
            None => write!(f, "{dir} within {}", self.within),
        }
    }
}

impl FromStr for Expects {
    type Err = String;

    fn from_str(s: &str) -> Result<Expects, String> {
        let shape = || format!("expects looks like up 5 within 3 or down within 2, not {s:?}");
        let words: Vec<&str> = s.split_whitespace().collect();
        let (direction, rest) = match words.split_first() {
            Some((&"up", rest)) => (Direction::Up, rest),
            Some((&"down", rest)) => (Direction::Down, rest),
            _ => return Err(shape()),
        };
        let (size, rest) = match rest {
            [n, rest @ ..] if *n != "within" => {
                let size: f64 = n.parse().map_err(|_| shape())?;
                if !size.is_finite() || size <= 0.0 {
                    return Err(format!(
                        "the size in expects is a number above 0, not {n:?}; the direction says up or down"
                    ));
                }
                (Some(size), rest)
            }
            rest => (None, rest),
        };
        let within = match rest {
            ["within", n] => n.parse::<u32>().map_err(|_| shape())?,
            _ => return Err(shape()),
        };
        if within == 0 {
            return Err("expects needs at least 1 report to look at: within 1 or more".to_string());
        }
        Ok(Expects {
            direction,
            size,
            within,
        })
    }
}

impl Serialize for Expects {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Expects {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Expects, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// What to do when `run` dies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Restart {
    Always,
    #[default]
    OnFailure,
    Never,
}

/// Durations as text, like `30s`, `5m`, `1h`.
pub mod duration {
    use super::*;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&humantime::format_duration(*d).to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let s = String::deserialize(d)?;
        humantime::parse_duration(s.trim()).map_err(serde::de::Error::custom)
    }
}

/// Why a mandate could not be read.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct MandateError(pub String);

/// The one action a controller may name without the mandate permitting it.
pub const STOP: &str = "stop";

/// The fields of a mandate, in the order a human would write them.
const FIELDS: &[&str] = &[
    "purpose",
    "metric",
    "controller",
    "permitted",
    "forbidden",
    "cadence",
    "context_budget",
    "summarize_after",
    "restart",
    "network",
    "children",
    "parent",
    "allocation",
    "guards",
    "audits",
    "range",
    "max_step",
    "confirm",
    "escalate",
    "world",
    "calibration",
    "early_warning",
    "better",
];
const THRESHOLD_FIELDS: &[&str] = &["kind", "above", "below", "action", "deadband", "hold"];
const BAND_FIELDS: &[&str] = &["kind", "low", "high", "below", "above", "deadband", "hold"];
const GUARD_FIELDS: &[&str] = &[
    "name", "run", "adapter", "above", "below", "then", "drawdown", "of", "range", "max_step",
    "confirm", "unwind",
];
const UNWIND_FIELDS: &[&str] = &["action", "payload"];
const AUDIT_FIELDS: &[&str] = &[
    "name",
    "run",
    "adapter",
    "worse",
    "better",
    "tolerance",
    "window",
    "range",
    "max_step",
    "confirm",
];
const BACKEND_FIELDS: &[&str] = &["kind", "backend"];
const PLAN_FIELDS: &[&str] = &[
    "kind", "horizon", "depth", "members", "explore", "backend", "better",
];
const ACTION_FIELDS: &[&str] = &[
    "name",
    "run",
    "adapter",
    "cap",
    "reversible",
    "delegated",
    "takes",
    "cost",
    "check",
    "expects",
    "revoke_after",
    "fields",
];

/// Names no permitted action may take: they are built in.
pub const BUILT_IN: &[&str] = &[STOP, "delegate"];

/// The levers a parent pulls on its children, by name alone, each taking
/// text: `budget <child> <amount>`, `pause <child>`, `resume <child>`,
/// `threshold <child> above|below <number>`.
pub const PARENT_LEVERS: &[&str] = &["budget", "pause", "resume", "threshold"];

/// A parent lever as pulled: which child, and what for.
#[derive(Clone, Debug, PartialEq)]
pub enum ParentCall {
    Budget {
        child: String,
        amount: u32,
    },
    Pause {
        child: String,
    },
    Resume {
        child: String,
    },
    Threshold {
        child: String,
        above: bool,
        bound: f64,
    },
}

impl ParentCall {
    /// Read a parent lever's text payload.
    pub fn parse(lever: &str, text: &str) -> Result<ParentCall, String> {
        let words: Vec<&str> = text.split_whitespace().collect();
        let child = words
            .first()
            .filter(|w| is_word(w))
            .ok_or_else(|| format!("{lever} names a child first, like: {lever} shop"))?
            .to_string();
        match (lever, &words[1..]) {
            ("budget", [amount]) => Ok(ParentCall::Budget {
                child,
                amount: amount
                    .parse()
                    .map_err(|_| format!("budget takes a whole number, not {amount:?}"))?,
            }),
            ("budget", _) => Err("budget is written: budget <child> <amount>".to_string()),
            ("pause", []) => Ok(ParentCall::Pause { child }),
            ("pause", _) => Err("pause is written: pause <child>".to_string()),
            ("resume", []) => Ok(ParentCall::Resume { child }),
            ("resume", _) => Err("resume is written: resume <child>".to_string()),
            ("threshold", [which, bound]) if matches!(*which, "above" | "below") => {
                let bound: f64 = bound
                    .parse()
                    .ok()
                    .filter(|b: &f64| b.is_finite())
                    .ok_or_else(|| format!("threshold takes a number, not {bound:?}"))?;
                Ok(ParentCall::Threshold {
                    child,
                    above: *which == "above",
                    bound,
                })
            }
            ("threshold", _) => {
                Err("threshold is written: threshold <child> above|below <number>".to_string())
            }
            _ => Err(format!("{lever} is not a parent lever")),
        }
    }

    pub fn child(&self) -> &str {
        match self {
            ParentCall::Budget { child, .. }
            | ParentCall::Pause { child }
            | ParentCall::Resume { child }
            | ParentCall::Threshold { child, .. } => child,
        }
    }

    /// One line, as an action's detail.
    pub fn describe(&self) -> String {
        match self {
            ParentCall::Budget { child, amount } => format!("{child} may spend {amount} a day"),
            ParentCall::Pause { child } => format!("{child} is paused"),
            ParentCall::Resume { child } => format!("{child} may run again"),
            ParentCall::Threshold {
                child,
                above,
                bound,
            } => format!(
                "{child} acts {} {bound}",
                if *above { "above" } else { "below" }
            ),
        }
    }
}

/// The built-in lever that asks the research role a question. A mandate
/// permits it by name alone, with no run and no adapter.
pub const RESEARCH: &str = "research";

/// The built-in lever that forecasts the venture: forked branches of the
/// ledger, every lever stepping the world model. Permitted by name alone;
/// it only reads, and takes no payload.
pub const FORECAST: &str = "forecast";

const KINDS: &str = "threshold, band, classifier, model or plan";
const NEEDS_PURPOSE: &str = "mandate needs a purpose: what the venture is for, in your own words";
const NEEDS_METRIC: &str =
    "mandate needs a metric: the name of the number the report prints, like count";
const NEEDS_CONTROLLER: &str =
    "mandate needs a controller: what decides when to act, with a kind of threshold, band, classifier, model or plan";
const NEEDS_CADENCE: &str =
    "mandate needs a cadence: how often the report runs, like 30s, 5m or 1h";
const NEEDS_ACTION: &str =
    "a threshold controller needs an action: the name of a permitted action, or stop";
const NEEDS_BOUNDS: &str =
    "a band controller needs low and high: the bounds the metric is held between, numbers like 19 and 24";
const NEEDS_LEVERS: &str =
    "a band controller needs below and above: the action to run below low and the one to run above high, each a permitted action or stop";
const BUDGET: &str = "context_budget must be a whole number of tokens greater than 0, like 4000";
const SUMMARIZE: &str = "summarize_after must be a whole number of records, at least 2, like 200";

impl Mandate {
    /// Parse and check mandate text.
    ///
    /// The text is read as plain YAML first and its shape checked field by
    /// field, so that every message says what is wrong and what a correct
    /// value looks like. Only then is it read into a `Mandate`, whose values
    /// are checked with [`Mandate::validate`].
    pub fn parse(text: &str) -> Result<Mandate, MandateError> {
        let value: Value = serde_yaml::from_str(text).map_err(|e| MandateError(plain(&e)))?;
        check_shape(&value)?;
        // Read from the text again rather than from `value`: serde keeps
        // the line number that way, should anything slip past the checks.
        let mut mandate: Mandate =
            serde_yaml::from_str(text).map_err(|e| MandateError(plain(&e)))?;
        // A plan's and an audit's `better`, left out, are the mandate's.
        if let Some(b) = mandate.better {
            let unset = |v: Option<&Value>| v.and_then(|v| v.get("better")).is_none();
            if let Controller::Plan { better, .. } = &mut mandate.controller {
                if unset(value.get("controller")) {
                    *better = b;
                }
            }
            for (i, a) in mandate.audits.iter_mut().enumerate() {
                if unset(value.get("audits").and_then(|l| l.get(i))) {
                    a.better = b;
                }
            }
        }
        mandate.validate()?;
        Ok(mandate)
    }

    /// Check the values of a mandate that already has the right shape: the
    /// rules a file with all the right fields can still break.
    pub fn validate(&self) -> Result<(), MandateError> {
        if self.purpose.trim().is_empty() {
            return fail(NEEDS_PURPOSE);
        }
        if self.metric.trim().is_empty() {
            return fail(NEEDS_METRIC);
        }
        if let Some(label) = self.metric.label() {
            if !is_word(label) {
                return fail(format!(
                    "metric label must be one word like temperature or latency_ms, not {label:?}"
                ));
            }
        }
        match adapter::Metric::parse(&self.metric) {
            Ok(Some(_)) => {}
            Ok(None) if is_word(self.metric.trim()) => {}
            Ok(None) => {
                return fail(format!(
                    "metric must be one word like count or latency_ms, not {:?}",
                    &*self.metric
                ))
            }
            Err(e) => return fail(format!("metric: {e}")),
        }
        let mut names: Vec<&str> = Vec::new();
        for p in &self.permitted {
            if !is_word(&p.name) {
                return fail(format!(
                    "a permitted action's name is one word like reset, not {:?}",
                    p.name
                ));
            }
            if names.contains(&p.name.as_str()) {
                return fail(format!(
                    "permitted action {:?} is listed twice; each action needs its own name",
                    p.name
                ));
            }
            if BUILT_IN.contains(&p.name.as_str()) {
                return fail(format!(
                    "{} is built in; a permitted action cannot be named {:?}",
                    p.name, p.name
                ));
            }
            names.push(&p.name);
            if p.check.is_some() && (p.takes().is_none() || p.is_parent_lever() || p.is_research())
            {
                return fail(format!(
                    "permitted action {:?} has a check, which reads a payload; only a lever that takes a payload, and not research or a parent lever, can have one",
                    p.name
                ));
            }
            match (&p.run, &p.adapter) {
                (None, None) if PARENT_LEVERS.contains(&p.name.as_str()) => {
                    if self.children.is_empty() {
                        return fail(format!(
                            "{} is a parent lever; list the ventures it applies to under children: first",
                            p.name
                        ));
                    }
                    if let Some(t) = p.takes {
                        if t != Takes::Text {
                            return fail(format!(
                                "permitted action {:?} takes text, like: {} <child> ...; leave takes out",
                                p.name, p.name
                            ));
                        }
                    }
                    if p.name == "resume" && p.delegated {
                        return fail(
                            "resume is declared-only: a human decides each time; remove delegated",
                        );
                    }
                }
                (Some(_), None) | (None, Some(_)) if p.name == "resume" => {
                    return fail(
                        "resume is built in; a permitted action cannot be named \"resume\"",
                    )
                }
                (None, None) if p.name == RESEARCH => {
                    if let Some(t) = p.takes {
                        if t != Takes::Text {
                            return fail(
                                "permitted action \"research\" takes text, the question; leave takes out",
                            );
                        }
                    }
                    if !p.reversible {
                        return fail(
                            "permitted action \"research\" only reads; say reversible: true",
                        );
                    }
                }
                (Some(_), None) | (None, Some(_)) if p.name == RESEARCH => {
                    return fail(
                        "research is built in; permit it by name alone, with no run and no adapter",
                    )
                }
                (None, None) if p.name == FORECAST => {
                    if p.takes.is_some() {
                        return fail(
                            "permitted action \"forecast\" takes no payload; leave takes out",
                        );
                    }
                    if !p.reversible {
                        return fail(
                            "permitted action \"forecast\" only reads; say reversible: true",
                        );
                    }
                }
                (Some(_), None) | (None, Some(_)) if p.name == FORECAST => {
                    return fail(
                        "forecast is built in; permit it by name alone, with no run and no adapter",
                    )
                }
                (Some(run), None) if run.trim().is_empty() => {
                    return fail(format!(
                        "permitted action {:?} has an empty run; give it the command to run, like ./reset.sh",
                        p.name
                    ));
                }
                (Some(_), None) => {}
                (None, Some(spec)) => {
                    let lever = adapter::Lever::parse(spec)
                        .map_err(|e| MandateError(format!("permitted action {:?}: {e}", p.name)))?;
                    if let Some(t) = p.takes {
                        if t != lever.takes() {
                            return fail(format!(
                                "permitted action {:?}: {} takes {}; leave takes out or say {}",
                                p.name,
                                lever.name(),
                                lever.takes(),
                                lever.takes()
                            ));
                        }
                    }
                    if p.reversible && !lever.reversible() {
                        return fail(format!(
                            "permitted action {:?}: {} cannot be undone; say reversible: false",
                            p.name,
                            lever.name()
                        ));
                    }
                }
                (Some(_), Some(_)) => {
                    return fail(format!(
                        "permitted action {:?} has both run and adapter; choose one",
                        p.name
                    ))
                }
                (None, None) => {
                    return fail(format!(
                        "permitted action {:?} needs run (a command) or adapter (a built-in: {}); only research, forecast and the parent levers need neither",
                        p.name,
                        adapter::LEVERS.join(", ")
                    ))
                }
            }
            if p.cap.count == 0 {
                return fail(format!(
                    "permitted action {:?}: a cap of 0 permits nothing; leave the action out instead",
                    p.name
                ));
            }
        }
        for f in &self.forbidden {
            if !is_word(f) {
                return fail(format!(
                    "forbidden names actions, one word each like rm, not {f:?}"
                ));
            }
            if names.contains(&f.as_str()) {
                return fail(format!(
                    "action {f:?} is both permitted and forbidden; choose one"
                ));
            }
        }
        if let Controller::Threshold {
            above,
            below,
            action,
            ..
        } = &self.controller
        {
            match (above, below) {
                (None, None) => {
                    return fail(
                        "a threshold controller needs a bound: above or below, a number like 10",
                    )
                }
                (Some(_), Some(_)) => {
                    return fail("a threshold controller takes one bound, above or below, not both")
                }
                _ => {}
            }
            for (which, bound) in [("above", above), ("below", below)] {
                if let Some(b) = bound {
                    if !b.is_finite() {
                        return fail(format!(
                            "threshold {which} must be an ordinary number like 10, not {b}"
                        ));
                    }
                }
            }
            if action.trim().is_empty() {
                return fail(NEEDS_ACTION);
            }
            check_lever("threshold action", action, &names)?;
        }
        if let Controller::Band {
            low,
            high,
            below,
            above,
            deadband,
            ..
        } = &self.controller
        {
            for (which, bound, like) in [("low", low, 19), ("high", high, 24)] {
                if !bound.is_finite() {
                    return fail(format!(
                        "band {which} must be an ordinary number like {like}, not {bound}"
                    ));
                }
            }
            if low >= high {
                return fail(format!(
                    "band low must be less than high; low is {low} and high is {high}"
                ));
            }
            if below.trim().is_empty() || above.trim().is_empty() {
                return fail(NEEDS_LEVERS);
            }
            check_lever("band below", below, &names)?;
            check_lever("band above", above, &names)?;
            // A side's lever brings the metric back into the band; one
            // that expects to push it further out was named on the wrong
            // side.
            let metric = self.metric_name();
            for (side, lever, way, past, bound, want) in [
                ("below", below, Direction::Up, "under", low, "up"),
                ("above", above, Direction::Down, "over", high, "down"),
            ] {
                if let Some(e) = self.permitted(lever).and_then(|p| p.expects.as_ref()) {
                    if e.direction != way {
                        return fail(format!(
                            "band {side} runs {lever} when {metric} is {past} {bound}, but {lever} expects {e}; a lever {side} the band expects {want}, or nothing"
                        ));
                    }
                }
            }
            if let Some(d) = deadband {
                let wide = high - low;
                if !d.is_finite() || *d < 0.0 || *d >= wide {
                    return fail(format!(
                        "band deadband must be a number 0 or more and less than the band is wide ({wide}), like 1, not {d}"
                    ));
                }
            }
        }
        if self.cadence < Duration::from_secs(1) {
            return fail(format!(
                "cadence must be at least 1s, not {}",
                humantime::format_duration(self.cadence)
            ));
        }
        if self.context_budget == 0 {
            return fail(format!("{BUDGET}, not 0"));
        }
        if self.summarize_after < 2 {
            return fail(format!("{SUMMARIZE}, not {}", self.summarize_after));
        }
        for child in &self.children {
            let c = &child.name;
            if !is_word(c) {
                return fail(format!("children names ventures, one word each, not {c:?}"));
            }
            if c == "self" {
                return fail("self cannot be a child; it belongs to no one".to_string());
            }
        }
        if let Some(total) = self.allocation.as_ref().and_then(|a| a.total) {
            let floors: u64 = self.children.iter().map(|c| u64::from(c.floor)).sum();
            if floors > u64::from(total) {
                return fail(format!(
                    "the children's floors add up to {floors}, past the allocation total of {total}; no budgets could be set"
                ));
            }
        }
        if let Some(p) = &self.parent {
            if !is_word(p) {
                return fail(format!("parent names a venture, one word, not {p:?}"));
            }
            if p == "self" {
                return fail("self cannot be a parent; it allocates nothing".to_string());
            }
            if self.child(p).is_some() {
                return fail(format!(
                    "{p} cannot be both this venture's parent and its child"
                ));
            }
        }
        if let Controller::Threshold {
            deadband: Some(d), ..
        } = &self.controller
        {
            if !d.is_finite() || *d < 0.0 {
                return fail(format!(
                    "threshold deadband must be a number 0 or more, like 2, not {d}"
                ));
            }
        }
        self.check_guards()?;
        self.check_audits()?;
        check_plausibility("", self.range, self.max_step)?;
        if let Some(world) = &self.world {
            if world.trim().is_empty() {
                return fail("world is a command like ./world; it is empty");
            }
            // A world model only says what would happen: it is never a
            // lever, so it can never be what a lever runs.
            if let Some(p) = self.permitted.iter().find(|p| {
                p.run.as_deref().is_some_and(|r| same_command(r, world))
                    || p.adapter.as_deref().is_some_and(|a| same_command(a, world))
            }) {
                return fail(format!(
                    "world runs {world:?}, the command of lever {:?}; a world model only says what would happen and is never a lever",
                    p.name
                ));
            }
        }
        if let Controller::Plan { explore, .. } = &self.controller {
            for name in explore {
                if self.permitted(name).is_none() {
                    return fail(format!(
                        "plan explore names {name:?}, which is not a permitted action"
                    ));
                }
            }
        }
        if let Some(c) = self.calibration {
            if !c.is_finite() || c > 1.0 {
                return fail(format!(
                    "calibration is a skill score of at most 1, like 0.1, not {c}"
                ));
            }
        }
        if let Some(hosts) = &self.network {
            for h in hosts {
                let ok = !h.is_empty()
                    && h.chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'));
                if !ok {
                    return fail(format!(
                        "network lists hosts, like api.stripe.com, not {h:?}"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Guards: each named once, read one way, with one bound; and none
    /// reading what a lever runs or what the metric reads, so a lever can
    /// never move its own guard.
    fn check_guards(&self) -> Result<(), MandateError> {
        let mut names: Vec<&str> = Vec::new();
        for g in &self.guards {
            let who = format!("guard {:?}", g.name);
            if !is_word(&g.name) {
                return fail(format!(
                    "a guard's name is one word like spend, not {:?}",
                    g.name
                ));
            }
            if names.contains(&g.name.as_str()) {
                return fail(format!(
                    "{who} is listed twice; each guard needs its own name"
                ));
            }
            names.push(&g.name);
            match (&g.run, &g.adapter, g.drawdown) {
                (Some(run), None, None) if run.trim().is_empty() => {
                    return fail(format!("{who} has an empty run; give it a command"))
                }
                (Some(_), None, None) => {}
                (None, Some(spec), None) => check_metric_spec(&who, spec)?,
                (None, None, Some(_)) => {
                    if g.above.is_some() || g.below.is_some() {
                        return fail(format!(
                            "{who} is a drawdown; it trips at its percent and takes no above or below"
                        ));
                    }
                    match g.of.as_deref() {
                        Some(of) if of.trim() == self.metric_name() => {}
                        Some(of) => {
                            return fail(format!(
                                "{who}: a drawdown is of the venture's metric, {}, not {of:?}",
                                self.metric_name()
                            ))
                        }
                        None => {
                            return fail(format!(
                                "{who}: a drawdown needs of: the metric, here {}",
                                self.metric_name()
                            ))
                        }
                    }
                }
                _ => {
                    return fail(format!(
                        "{who} needs exactly one of run (a command), adapter (a built-in metric) or drawdown (like 15%)"
                    ))
                }
            }
            if g.drawdown.is_none() {
                if g.of.is_some() {
                    return fail(format!("{who}: of goes only with drawdown"));
                }
                match (g.above, g.below) {
                    (Some(_), Some(_)) => {
                        return fail(format!("{who} takes one bound, above or below, not both"))
                    }
                    (None, None) => {
                        return fail(format!(
                            "{who} needs a bound: above or below, a number like 100"
                        ))
                    }
                    (Some(b), None) | (None, Some(b)) if !b.is_finite() => {
                        return fail(format!(
                            "{who}: the bound must be an ordinary number, not {b}"
                        ))
                    }
                    _ => {}
                }
            }
            if let Some(cmd) = g.run.as_deref().or(g.adapter.as_deref()) {
                self.apart(&who, cmd)?;
            }
            check_plausibility(&format!("{who}: "), g.range, g.max_step)?;
            if let Some(u) = &g.unwind {
                self.check_unwind(&who, u)?;
            }
        }
        Ok(())
    }

    /// An unwind names a permitted, unforbidden lever of the venture's own
    /// and the payload it takes, exactly as its takes and fields say.
    fn check_unwind(&self, who: &str, u: &Unwind) -> Result<(), MandateError> {
        let lever = u.action.as_str();
        if lever == STOP {
            return fail(format!(
                "{who}: stop always passes; a guard that stops says then: pause, not unwind"
            ));
        }
        let Some(perm) = self.permitted(lever) else {
            return fail(format!(
                "{who}: unwind names {lever:?}, which is not a permitted action"
            ));
        };
        if self.forbidden.iter().any(|f| f == lever) {
            return fail(format!("{who}: unwind names {lever:?}, which is forbidden"));
        }
        if perm.is_parent_lever() || perm.is_research() || perm.is_forecast() || lever == "delegate"
        {
            return fail(format!(
                "{who}: unwind names {lever:?}, a built-in lever; an unwind runs a lever of the venture's own"
            ));
        }
        u.payload_for(perm)
            .map(|_| ())
            .map_err(|e| MandateError(format!("{who}: unwind: {e}")))
    }

    /// Refuse a guard that reads what a lever runs or what the metric
    /// reads: a guard is read apart from what it guards.
    fn apart(&self, who: &str, cmd: &str) -> Result<(), MandateError> {
        let lever = self.permitted.iter().find(|p| {
            p.run.as_deref().is_some_and(|r| same_command(r, cmd))
                || p.adapter.as_deref().is_some_and(|a| same_command(a, cmd))
        });
        if let Some(p) = lever {
            return fail(format!(
                "{who} reads {cmd:?}, the command of lever {:?}; a guard is read apart from what it guards",
                p.name
            ));
        }
        let metric = self.metric.trim();
        let shell = metric.strip_prefix("shell ").unwrap_or(metric);
        if same_command(metric, cmd)
            || same_command(shell, cmd)
            || (self.metric_adapter().is_none() && same_command("report", cmd))
        {
            return fail(format!(
                "{who} reads {cmd:?}, which is how the metric is read; a guard is read apart from what it guards"
            ));
        }
        Ok(())
    }

    /// Audits: each named once, read one way.
    fn check_audits(&self) -> Result<(), MandateError> {
        let mut names: Vec<&str> = Vec::new();
        for a in &self.audits {
            let who = format!("audit {:?}", a.name);
            if !is_word(&a.name) {
                return fail(format!(
                    "an audit's name is one word like refunds, not {:?}",
                    a.name
                ));
            }
            if names.contains(&a.name.as_str()) {
                return fail(format!(
                    "{who} is listed twice; each audit needs its own name"
                ));
            }
            names.push(&a.name);
            match (&a.run, &a.adapter) {
                (Some(run), None) if !run.trim().is_empty() => {}
                (None, Some(spec)) => check_metric_spec(&who, spec)?,
                _ => {
                    return fail(format!(
                        "{who} needs exactly one of run (a command) or adapter (a built-in metric)"
                    ))
                }
            }
            check_plausibility(&format!("{who}: "), a.range, a.max_step)?;
            if !a.tolerance.is_finite() || a.tolerance < 0.0 {
                return fail(format!(
                    "{who}: tolerance is a number 0 or more, not {}",
                    a.tolerance
                ));
            }
            if a.window < 2 {
                return fail(format!(
                    "{who}: window is at least 2 readings, not {}",
                    a.window
                ));
            }
        }
        Ok(())
    }

    /// The guard with this name, if any.
    pub fn guard(&self, name: &str) -> Option<&Guard> {
        self.guards.iter().find(|g| g.name == name)
    }

    /// The audit with this name, if any.
    pub fn audit(&self, name: &str) -> Option<&Audit> {
        self.audits.iter().find(|a| a.name == name)
    }

    /// The threshold's or band's deadband and hold: 0 and 1 when unset or
    /// neither.
    pub fn deadband_hold(&self) -> (f64, u32) {
        match &self.controller {
            Controller::Threshold { deadband, hold, .. }
            | Controller::Band { deadband, hold, .. } => {
                (deadband.unwrap_or(0.0), hold.unwrap_or(1).max(1))
            }
            _ => (0.0, 1),
        }
    }

    /// May the venture reach this host? Yes when the mandate sets no
    /// limit; else only when the host is listed.
    pub fn allows_host(&self, host: &str) -> bool {
        match &self.network {
            None => true,
            Some(hosts) => hosts.iter().any(|h| h.eq_ignore_ascii_case(host)),
        }
    }

    /// Read and parse a mandate.yaml.
    pub fn load(path: &Path) -> Result<Mandate, MandateError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| MandateError(format!("cannot read {}: {e}", path.display())))?;
        Mandate::parse(&text)
    }

    /// The hash of mandate text, as recorded when a mandate is adopted.
    pub fn hash(text: &str) -> String {
        hex::encode(Sha256::digest(text.as_bytes()))
    }

    /// A mandate that permits nothing, for a venture whose mandate cannot
    /// be read.
    pub fn empty() -> Mandate {
        Mandate {
            purpose: String::new(),
            metric: MetricSpec::from(""),
            controller: Controller::Model { backend: None },
            permitted: vec![],
            forbidden: vec![],
            cadence: Duration::from_secs(60),
            context_budget: default_context_budget(),
            summarize_after: default_summarize_after(),
            restart: Restart::default(),
            network: None,
            children: vec![],
            parent: None,
            allocation: None,
            guards: vec![],
            audits: vec![],
            range: None,
            max_step: None,
            confirm: true,
            escalate: None,
            world: None,
            calibration: None,
            early_warning: false,
            better: None,
        }
    }

    /// Which way the target improves, as the mandate declares it: the
    /// threshold's side (below an `above` bound is better), else
    /// `better`. None when neither says. A band has two sides and so no
    /// one way of its own.
    pub fn direction(&self) -> Option<Worse> {
        match &self.controller {
            Controller::Threshold { above: Some(_), .. } => Some(Worse::Down),
            Controller::Threshold { below: Some(_), .. } => Some(Worse::Up),
            _ => self.better,
        }
    }

    /// The bounds the controller acts past: a threshold's one, a band's
    /// two (below first), each with whether it acts above it and the
    /// action it runs. Empty for any other controller.
    pub fn sides(&self) -> Vec<(bool, f64, &str)> {
        match &self.controller {
            Controller::Threshold {
                above: Some(b),
                action,
                ..
            } => vec![(true, *b, action)],
            Controller::Threshold {
                below: Some(b),
                action,
                ..
            } => vec![(false, *b, action)],
            Controller::Band {
                low,
                high,
                below,
                above,
                ..
            } => vec![(false, *low, below), (true, *high, above)],
            _ => vec![],
        }
    }

    /// The child of this name, if it is listed.
    pub fn child(&self, name: &str) -> Option<&Child> {
        self.children.iter().find(|c| c.name == name)
    }

    /// The permitted action with this name, if any.
    pub fn permitted(&self, name: &str) -> Option<&Permitted> {
        self.permitted.iter().find(|p| p.name == name)
    }

    /// The metric's name: the word before any adapter argument, as reports
    /// carry it.
    pub fn metric_name(&self) -> &str {
        if let Some(label) = self.metric.label() {
            return label;
        }
        self.metric
            .trim()
            .split(char::is_whitespace)
            .next()
            .unwrap_or("")
    }

    /// The built-in metric, when the metric names one.
    pub fn metric_adapter(&self) -> Option<adapter::Metric> {
        adapter::Metric::parse(&self.metric).ok().flatten()
    }
}

/// The mandate text with one lever delegated, the rest of the text as the
/// human wrote it: comments, order and spacing kept. When the text cannot
/// be edited in place, the mandate is written out afresh instead.
pub fn delegate_text(text: &str, lever: &str) -> Result<String, String> {
    let before = Mandate::parse(text).map_err(|e| e.0)?;
    let at = before
        .permitted
        .iter()
        .position(|p| p.name == lever)
        .ok_or_else(|| format!("the mandate does not permit {lever}"))?;
    if before.permitted[at].delegated {
        return Err(format!("{lever} is already delegated"));
    }
    let mut expected = before.clone();
    expected.permitted[at].delegated = true;
    expected
        .validate()
        .map_err(|e| format!("delegating {lever} makes the mandate invalid: {e}"))?;
    if let Some(edited) = edit_delegated(text, lever) {
        if Mandate::parse(&edited).ok().as_ref() == Some(&expected) {
            return Ok(edited);
        }
    }
    serde_yaml::to_string(&expected).map_err(|e| e.to_string())
}

/// Set `delegated: true` on one lever in mandate text, in place.
fn edit_delegated(text: &str, lever: &str) -> Option<String> {
    let names_lever = |line: &str| {
        line.split(['{', ',', '}'])
            .any(|part| part.trim() == format!("name: {lever}"))
    };
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    // Flow style: the lever's item sits on one line inside braces.
    if let Some(i) = lines
        .iter()
        .position(|l| l.contains('{') && l.contains('}') && names_lever(l))
    {
        let line = lines[i];
        out[i] = if line.contains("delegated: false") {
            line.replacen("delegated: false", "delegated: true", 1)
        } else {
            let close = line.rfind('}')?;
            format!(
                "{}, delegated: true{}",
                line[..close].trim_end(),
                &line[close..]
            )
        };
        return Some(out.join("\n") + if text.ends_with('\n') { "\n" } else { "" });
    }
    // Block style: find the line naming the lever, then the item around it.
    let name_at = lines.iter().position(|l| {
        let t = l.trim_start();
        t.strip_prefix("- ").unwrap_or(t).trim() == format!("name: {lever}")
    })?;
    let indent_of = |l: &str| l.len() - l.trim_start().len();
    // The item starts at the nearest line above (or this one) with a dash.
    let start = (0..=name_at)
        .rev()
        .find(|&i| lines[i].trim_start().starts_with("- "))?;
    let dash_indent = indent_of(lines[start]);
    let key_indent = {
        let t = lines[start].trim_start();
        dash_indent + (t.len() - t.trim_start_matches('-').trim_start().len())
    };
    let mut end = start + 1;
    while end < lines.len() {
        let l = lines[end];
        if l.trim().is_empty() || l.trim_start().starts_with('#') {
            end += 1;
            continue;
        }
        if indent_of(l) < key_indent {
            break;
        }
        end += 1;
    }
    // Trim trailing blank and comment lines back off the item.
    while end > start + 1
        && (lines[end - 1].trim().is_empty() || lines[end - 1].trim_start().starts_with('#'))
    {
        end -= 1;
    }
    if let Some(i) = (start..end).find(|&i| lines[i].trim() == "delegated: false") {
        out[i] = lines[i].replacen("delegated: false", "delegated: true", 1);
    } else {
        out.insert(end, format!("{}delegated: true", " ".repeat(key_indent)));
    }
    Some(out.join("\n") + if text.ends_with('\n') { "\n" } else { "" })
}

impl Permitted {
    /// The shape of payload the lever consumes: as declared, or as the
    /// built-in action takes it. Research takes the question as text.
    pub fn takes(&self) -> Option<Takes> {
        self.takes
            .or_else(|| {
                self.adapter
                    .as_deref()
                    .and_then(|a| adapter::Lever::parse(a).ok())
                    .map(|l| l.takes())
            })
            .or_else(|| (self.is_research() || self.is_parent_lever()).then_some(Takes::Text))
    }

    /// Is this the built-in research lever?
    pub fn is_research(&self) -> bool {
        self.name == RESEARCH && self.run.is_none() && self.adapter.is_none()
    }

    /// Is this the built-in forecast lever?
    pub fn is_forecast(&self) -> bool {
        self.name == FORECAST && self.run.is_none() && self.adapter.is_none()
    }

    /// Is this one of the parent levers?
    pub fn is_parent_lever(&self) -> bool {
        PARENT_LEVERS.contains(&self.name.as_str()) && self.run.is_none() && self.adapter.is_none()
    }

    /// Does pulling it send something out into the world? Those count
    /// toward the sends quota.
    pub fn sends(&self) -> bool {
        self.lever_adapter().is_some()
    }

    /// The built-in action, when the lever names one.
    pub fn lever_adapter(&self) -> Option<adapter::Lever> {
        self.adapter
            .as_deref()
            .and_then(|a| adapter::Lever::parse(a).ok())
    }
}

/// Do two commands run the same thing: the same words, a leading `./`
/// aside?
pub(crate) fn same_command(a: &str, b: &str) -> bool {
    let words = |c: &str| -> Vec<String> {
        c.split_whitespace()
            .map(|w| w.strip_prefix("./").unwrap_or(w).to_string())
            .collect()
    };
    let (a, b) = (words(a), words(b));
    !a.is_empty() && a == b
}

/// A built-in metric named by a guard or an audit.
fn check_metric_spec(who: &str, spec: &str) -> Result<(), MandateError> {
    match adapter::Metric::parse(spec) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => fail(format!(
            "{who}: {spec:?} is not a built-in metric; name one like {}",
            adapter::METRICS.join(", ")
        )),
        Err(e) => fail(format!("{who}: {e}")),
    }
}

fn fail<T>(msg: impl Into<String>) -> Result<T, MandateError> {
    Err(MandateError(msg.into()))
}

/// One word: letters, digits, `_`, `-` and `.`, and at least one of them.
fn is_word(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// A YAML value in plain words, for messages that say what was found.
fn describe(v: &Value) -> String {
    match v {
        Value::Null => "an empty value".to_string(),
        Value::Bool(b) => format!("the value {b}"),
        Value::Number(n) => format!("the number {n}"),
        Value::String(s) => format!("the text {s:?}"),
        Value::Sequence(_) => "a list".to_string(),
        Value::Mapping(_) => "a set of fields".to_string(),
        Value::Tagged(_) => "a tagged value".to_string(),
    }
}

/// Check the shape of the YAML before reading it into a `Mandate`: that it
/// is a set of fields, that the required ones are there, that each holds the
/// right kind of value, and that none is unknown. Everything serde would
/// otherwise reject is caught here, with a message in plain words.
fn check_shape(value: &Value) -> Result<(), MandateError> {
    let map = match value {
        Value::Null => return fail(
            "the mandate is empty; a mandate is a list of fields like purpose, metric, controller",
        ),
        Value::Mapping(map) => map,
        other => {
            return fail(format!(
                "a mandate is a list of fields like purpose, metric, controller, not {}",
                describe(other)
            ))
        }
    };
    if let Some(field) = unknown_field(map, FIELDS)? {
        return fail(format!(
            "unknown field {field:?}; a mandate has these fields: {}",
            FIELDS.join(", ")
        ));
    }
    check_purpose(map.get("purpose"))?;
    check_metric(map.get("metric"))?;
    check_controller(map.get("controller"))?;
    check_permitted(map.get("permitted"))?;
    check_forbidden(map.get("forbidden"))?;
    check_cadence(map.get("cadence"))?;
    check_context_budget(map.get("context_budget"))?;
    match map.get("summarize_after") {
        None => {}
        Some(value) => match value.as_u64() {
            Some(n) if (2..=u64::from(u32::MAX)).contains(&n) => {}
            _ => return fail(format!("{SUMMARIZE}, not {}", describe(value))),
        },
    }
    check_restart(map.get("restart"))?;
    check_network(map.get("network"))?;
    check_escalate(map.get("escalate"))?;
    match map.get("children") {
        None | Some(Value::Null) => {}
        Some(Value::Sequence(items)) => {
            for item in items {
                match item {
                    Value::String(_) => {}
                    Value::Mapping(terms) => check_child(terms)?,
                    other => {
                        return fail(format!(
                            "children names ventures, one word each, not {}",
                            describe(other)
                        ))
                    }
                }
            }
        }
        Some(other) => {
            return fail(format!(
                "children is a list of venture names, not {}",
                describe(other)
            ))
        }
    }
    match map.get("parent") {
        None | Some(Value::Null) | Some(Value::String(_)) => {}
        Some(other) => {
            return fail(format!(
                "parent is the name of a venture, not {}",
                describe(other)
            ))
        }
    }
    check_allocation(map.get("allocation"))?;
    check_list(
        map.get("guards"),
        "guards",
        "a guard",
        GUARD_FIELDS,
        "name, run or adapter, above or below, then: pause or ask",
    )?;
    check_unwinds(map.get("guards"))?;
    check_list(
        map.get("audits"),
        "audits",
        "an audit",
        AUDIT_FIELDS,
        "name, run or adapter, worse: up or down, tolerance",
    )?;
    check_directions(map.get("audits"))?;
    match map.get("range") {
        None | Some(Value::Null) => {}
        Some(Value::Sequence(items))
            if items.len() == 2 && items.iter().all(|i| matches!(i, Value::Number(_))) => {}
        Some(other) => {
            return fail(format!(
                "range is two numbers, low and high, like [0, 1000], not {}",
                describe(other)
            ))
        }
    }
    match map.get("max_step") {
        None | Some(Value::Null) | Some(Value::Number(_)) => {}
        Some(other) => {
            return fail(format!(
                "max_step is a number like 50, the largest plausible change between two reports, not {}",
                describe(other)
            ))
        }
    }
    match map.get("confirm") {
        None | Some(Value::Null) | Some(Value::Bool(_)) => {}
        Some(other) => {
            return fail(format!(
                "confirm is true or false: whether a report right after an implausible one waits for the next to agree, not {}",
                describe(other)
            ))
        }
    }
    match map.get("world") {
        None | Some(Value::Null) | Some(Value::String(_)) => {}
        Some(other) => {
            return fail(format!(
                "world is a command like ./world, run in the venture directory, not {}",
                describe(other)
            ))
        }
    }
    match map.get("calibration") {
        None | Some(Value::Null) | Some(Value::Number(_)) => {}
        Some(other) => {
            return fail(format!(
                "calibration is a skill score like 0.1, the bar the world model must reach, not {}",
                describe(other)
            ))
        }
    }
    match map.get("better") {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) if s == "up" || s == "down" => {}
        Some(other) => {
            return fail(format!(
                "better is up or down, which way the target improves, not {}",
                describe(other)
            ))
        }
    }
    match map.get("early_warning") {
        None | Some(Value::Null) | Some(Value::Bool(_)) => Ok(()),
        Some(other) => fail(format!(
            "early_warning is true or false, not {}",
            describe(other)
        )),
    }
}

/// A guard's `unwind` is `{action: <lever>, payload: <what it takes>}`.
fn check_unwinds(v: Option<&Value>) -> Result<(), MandateError> {
    let Some(Value::Sequence(items)) = v else {
        return Ok(());
    };
    for item in items {
        let Some(unwind) = item.get("unwind") else {
            continue;
        };
        let who = match item.get("name") {
            Some(Value::String(n)) => format!("guard {n:?}"),
            _ => "a guard".to_string(),
        };
        let terms = match unwind {
            Value::Null => continue,
            Value::Mapping(m) => m,
            other => {
                return fail(format!(
                    "{who}: unwind is {{action: <lever>, payload: <what it takes>}}, like {{action: trade, payload: {{side: sell, qty: 10}}}}; not {}",
                    describe(other)
                ))
            }
        };
        if let Some(f) = unknown_field(terms, UNWIND_FIELDS)? {
            return fail(format!(
                "{who}: unknown field {f:?} in unwind; an unwind has these fields: {}",
                UNWIND_FIELDS.join(", ")
            ));
        }
        if !matches!(terms.get("action"), Some(Value::String(_))) {
            return fail(format!(
                "{who}: unwind needs action, the permitted lever it runs"
            ));
        }
    }
    Ok(())
}

/// An audit's `worse` and `better` are up or down.
fn check_directions(v: Option<&Value>) -> Result<(), MandateError> {
    let Some(Value::Sequence(items)) = v else {
        return Ok(());
    };
    for item in items {
        for (field, what) in [
            ("worse", "which way the audit gets worse"),
            ("better", "which way the target improves"),
        ] {
            match item.get(field) {
                None => {}
                Some(Value::String(w)) if w == "up" || w == "down" => {}
                Some(other) => {
                    return fail(format!(
                        "an audit's {field} is up or down, {what}; not {}",
                        describe(other)
                    ))
                }
            }
        }
    }
    Ok(())
}

/// A list of sets of fields, each with only the known fields.
fn check_list(
    v: Option<&Value>,
    field: &str,
    one: &str,
    known: &[&str],
    example: &str,
) -> Result<(), MandateError> {
    let items = match v {
        None | Some(Value::Null) => return Ok(()),
        Some(Value::Sequence(items)) => items,
        Some(other) => {
            return fail(format!(
                "{field} is a list, each with {example}; not {}",
                describe(other)
            ))
        }
    };
    for item in items {
        let Value::Mapping(map) = item else {
            return fail(format!(
                "{one} in {field} is a set of fields ({example}), not {}",
                describe(item)
            ));
        };
        if let Some(f) = unknown_field(map, known)? {
            return fail(format!(
                "unknown field {f:?} in {field}; {one} has these fields: {}",
                known.join(", ")
            ));
        }
    }
    Ok(())
}

const CHILD_FIELDS: &[&str] = &["name", "priority", "floor"];

/// A child written as `{name: shop, priority: 2, floor: 3}`.
fn check_child(terms: &Mapping) -> Result<(), MandateError> {
    if let Some(field) = unknown_field(terms, CHILD_FIELDS)? {
        return fail(format!(
            "unknown child field {field:?}; a child has these fields: {}",
            CHILD_FIELDS.join(", ")
        ));
    }
    match terms.get("name") {
        Some(Value::String(_)) => {}
        _ => return fail("a child written with terms needs a name, like {name: shop, floor: 3}"),
    }
    match terms.get("priority") {
        None | Some(Value::Null) => {}
        Some(v) if v.as_i64().is_some_and(|n| i32::try_from(n).is_ok()) => {}
        Some(other) => {
            return fail(format!(
                "priority is a whole number, higher first, not {}",
                describe(other)
            ))
        }
    }
    match terms.get("floor") {
        None | Some(Value::Null) => Ok(()),
        Some(v) if v.as_u64().is_some_and(|n| u32::try_from(n).is_ok()) => Ok(()),
        Some(other) => fail(format!(
            "floor is a whole number, the least budget, not {}",
            describe(other)
        )),
    }
}

const ALLOCATION_FIELDS: &[&str] = &["deadband", "hold", "total"];

/// `allocation: {deadband: 2, hold: 1d, total: 6}`.
fn check_allocation(v: Option<&Value>) -> Result<(), MandateError> {
    let terms = match v {
        None | Some(Value::Null) => return Ok(()),
        Some(Value::Mapping(m)) => m,
        Some(other) => {
            return fail(format!(
                "allocation is {{deadband: 2, hold: 1d}}, not {}",
                describe(other)
            ))
        }
    };
    if let Some(field) = unknown_field(terms, ALLOCATION_FIELDS)? {
        return fail(format!(
            "unknown allocation field {field:?}; allocation has: {}",
            ALLOCATION_FIELDS.join(", ")
        ));
    }
    match terms.get("deadband") {
        None | Some(Value::Null) => {}
        Some(v) if v.as_u64().is_some_and(|n| u32::try_from(n).is_ok()) => {}
        Some(other) => {
            return fail(format!(
                "deadband is a whole number, the smallest budget change, not {}",
                describe(other)
            ))
        }
    }
    match terms.get("hold") {
        None => {}
        Some(Value::String(s)) if humantime::parse_duration(s.trim()).is_ok() => {}
        Some(other) => {
            return fail(format!(
                "hold is a length of time, like 1d, not {}",
                describe(other)
            ))
        }
    }
    match terms.get("total") {
        None | Some(Value::Null) => Ok(()),
        Some(v) if v.as_u64().is_some_and(|n| u32::try_from(n).is_ok()) => Ok(()),
        Some(other) => fail(format!(
            "total is a whole number, the most the children's budgets add up to, not {}",
            describe(other)
        )),
    }
}

fn check_escalate(v: Option<&Value>) -> Result<(), MandateError> {
    match v {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(s)) if s.trim() == "offbox" => Ok(()),
        Some(other) => fail(format!(
            "escalate is offbox, or left out for escalation on this machine only, not {}",
            describe(other)
        )),
    }
}

fn check_network(v: Option<&Value>) -> Result<(), MandateError> {
    match v {
        None => Ok(()),
        Some(Value::String(s)) if s.trim() == "none" => Ok(()),
        Some(Value::Sequence(items)) => {
            for item in items {
                if !matches!(item, Value::String(_)) {
                    return fail(format!(
                        "network lists hosts, like api.stripe.com, not {}",
                        describe(item)
                    ));
                }
            }
            Ok(())
        }
        Some(other) => fail(format!(
            "network is none, or a list of hosts the venture may reach, not {}",
            describe(other)
        )),
    }
}

/// The first field of `map` whose name is not in `known`, if any. A field
/// whose name is not even text is an error of its own.
fn unknown_field<'a>(map: &'a Mapping, known: &[&str]) -> Result<Option<&'a str>, MandateError> {
    for key in map.keys() {
        match key.as_str() {
            Some(name) if known.contains(&name) => {}
            Some(name) => return Ok(Some(name)),
            None => {
                return fail(format!(
                    "fields are named with words, not {}",
                    describe(key)
                ))
            }
        }
    }
    Ok(None)
}

fn check_purpose(v: Option<&Value>) -> Result<(), MandateError> {
    match v {
        None | Some(Value::Null) => fail(NEEDS_PURPOSE),
        Some(Value::String(_)) => Ok(()),
        Some(other) => fail(format!(
            "purpose must be text saying what the venture is for, not {}",
            describe(other)
        )),
    }
}

fn check_metric(v: Option<&Value>) -> Result<(), MandateError> {
    match v {
        None | Some(Value::Null) => fail(NEEDS_METRIC),
        Some(Value::String(s)) if s.trim().is_empty() => fail(NEEDS_METRIC),
        Some(Value::String(_)) => Ok(()),
        Some(Value::Mapping(m)) if !m.contains_key("shell") => {
            fail("metric must be one word like count or latency_ms, not a set of fields")
        }
        Some(Value::Mapping(m)) => {
            const SHAPE: &str = "metric as a set of fields is {shell: <command>, label: <name>}";
            if let Some(field) = unknown_field(m, &["shell", "label"])? {
                return fail(format!("{SHAPE}, not a field {field:?}"));
            }
            match (m.get("shell"), m.get("label")) {
                (Some(Value::String(c)), Some(Value::String(l))) if !c.trim().is_empty() => {
                    if is_word(l.trim()) && l.trim() == l {
                        Ok(())
                    } else {
                        fail(format!(
                            "metric label must be one word like temperature or latency_ms, not {l:?}"
                        ))
                    }
                }
                _ => fail(format!("{SHAPE}; both are text")),
            }
        }
        Some(other) => fail(format!(
            "metric must be one word like count or latency_ms, not {}",
            describe(other)
        )),
    }
}

/// A controller's action: a permitted one or the built-in stop. `who` is
/// how a message names it, like threshold action.
fn check_lever(who: &str, action: &str, names: &[&str]) -> Result<(), MandateError> {
    if action == STOP || names.contains(&action) {
        return Ok(());
    }
    let choices = if names.is_empty() {
        "this mandate permits no actions, so the only choice is the built-in stop".to_string()
    } else {
        format!(
            "name one of the permitted actions ({}) or the built-in stop",
            names.join(", ")
        )
    };
    fail(format!("{who} {action:?} is not permitted; {choices}"))
}

fn check_controller(v: Option<&Value>) -> Result<(), MandateError> {
    let map = match v {
        None | Some(Value::Null) => return fail(NEEDS_CONTROLLER),
        Some(Value::Mapping(map)) => map,
        Some(other) => {
            return fail(format!(
                "controller must be a set of fields starting with kind: {KINDS}, not {}",
                describe(other)
            ))
        }
    };
    let kind = match map.get("kind") {
        None | Some(Value::Null) => return fail(format!("controller needs a kind: {KINDS}")),
        Some(Value::String(kind))
            if matches!(
                kind.as_str(),
                "threshold" | "band" | "classifier" | "model" | "plan"
            ) =>
        {
            kind.as_str()
        }
        Some(Value::String(kind)) => {
            return fail(format!("controller kind must be {KINDS}, not {kind:?}"))
        }
        Some(other) => {
            return fail(format!(
                "controller kind must be {KINDS}, not {}",
                describe(other)
            ))
        }
    };
    let fields = match kind {
        "threshold" => THRESHOLD_FIELDS,
        "band" => BAND_FIELDS,
        "plan" => PLAN_FIELDS,
        _ => BACKEND_FIELDS,
    };
    if let Some(field) = unknown_field(map, fields)? {
        return fail(format!(
            "unknown field {field:?} in controller; a {kind} controller has these fields: {}",
            fields.join(", ")
        ));
    }
    if kind == "plan" {
        for (field, max, what) in [
            (
                "horizon",
                MAX_HORIZON,
                "steps of cadence a sequence looks ahead, like 4",
            ),
            ("depth", MAX_DEPTH, "levers a sequence pulls, like 2"),
            ("members", MAX_MEMBERS, "ensemble members, like 8"),
        ] {
            match map.get(field) {
                None | Some(Value::Null) => {}
                Some(Value::Number(n))
                    if n.as_u64()
                        .is_some_and(|n| (1..=u64::from(max)).contains(&n)) => {}
                Some(other) => {
                    return fail(format!(
                        "plan {field} is a whole number from 1 to {max}: the {what}; not {}",
                        describe(other)
                    ))
                }
            }
        }
        match map.get("explore") {
            None | Some(Value::Null) => {}
            Some(Value::Sequence(items)) if items.iter().all(|i| matches!(i, Value::String(_))) => {
            }
            Some(other) => {
                return fail(format!(
                    "plan explore is a list of permitted reversible levers, like [boost], not {}",
                    describe(other)
                ))
            }
        }
        match map.get("better") {
            None | Some(Value::Null) => {}
            Some(Value::String(s)) if s == "up" || s == "down" => {}
            Some(other) => {
                return fail(format!(
                    "plan better is up or down, which way the target improves, not {}",
                    describe(other)
                ))
            }
        }
    }
    if !matches!(kind, "threshold" | "band") {
        return match map.get("backend") {
            None | Some(Value::Null) | Some(Value::String(_)) => Ok(()),
            Some(other) => fail(format!(
                "controller backend must be the name of a backend, as text, not {}",
                describe(other)
            )),
        };
    }
    // A threshold's bounds are numbers it may leave out; a band's are
    // numbers it needs.
    let bounds: &[(&str, &str)] = if kind == "band" {
        &[("low", "19"), ("high", "24")]
    } else {
        &[("above", "10"), ("below", "10")]
    };
    for (which, like) in bounds {
        match map.get(*which) {
            None | Some(Value::Null) if kind == "band" => return fail(NEEDS_BOUNDS),
            None | Some(Value::Null) | Some(Value::Number(_)) => {}
            Some(other) => {
                return fail(format!(
                    "{kind} {which} must be a number like {like}, not {}",
                    describe(other)
                ))
            }
        }
    }
    match map.get("deadband") {
        None | Some(Value::Null) | Some(Value::Number(_)) => {}
        Some(other) => {
            return fail(format!(
                "{kind} deadband must be a number like 2, not {}",
                describe(other)
            ))
        }
    }
    match map.get("hold") {
        None | Some(Value::Null) => {}
        Some(Value::Number(n)) if n.as_u64().is_some_and(|n| (1..=1000).contains(&n)) => {}
        Some(other) => {
            return fail(format!(
                "{kind} hold must be a whole number of reports from 1 to 1000, like 3, not {}",
                describe(other)
            ))
        }
    }
    if kind == "band" {
        for which in ["below", "above"] {
            match map.get(which) {
                None | Some(Value::Null) => return fail(NEEDS_LEVERS),
                Some(Value::String(_)) => {}
                Some(other) => {
                    return fail(format!(
                        "band {which} must be the name of a permitted action, or stop, not {}",
                        describe(other)
                    ))
                }
            }
        }
        return Ok(());
    }
    match map.get("action") {
        None | Some(Value::Null) => fail(NEEDS_ACTION),
        Some(Value::String(_)) => Ok(()),
        Some(other) => fail(format!(
            "threshold action must be the name of a permitted action, or stop, not {}",
            describe(other)
        )),
    }
}

fn check_permitted(v: Option<&Value>) -> Result<(), MandateError> {
    let items = match v {
        None => return Ok(()),
        Some(Value::Null) => {
            return fail(
                "permitted is empty; list the actions the venture may take, or leave the field out",
            )
        }
        Some(Value::Sequence(items)) => items,
        Some(other) => {
            return fail(format!(
                "permitted must be a list of actions, each with a name, run, cap and reversible, not {}",
                describe(other)
            ))
        }
    };
    for (i, item) in items.iter().enumerate() {
        check_action(i + 1, item)?;
    }
    Ok(())
}

/// Check one permitted action; `position` counts from 1 and names the
/// action in messages until its name can.
fn check_action(position: usize, item: &Value) -> Result<(), MandateError> {
    let Value::Mapping(map) = item else {
        return fail(format!(
            "permitted action {position} must be a set of fields (name, run, cap, reversible), not {}",
            describe(item)
        ));
    };
    let who = match map.get("name") {
        Some(Value::String(name)) => format!("permitted action {name:?}"),
        _ => format!("permitted action {position}"),
    };
    if let Some(field) = unknown_field(map, ACTION_FIELDS)? {
        return fail(format!(
            "unknown field {field:?} in {who}; a permitted action has these fields: {}",
            ACTION_FIELDS.join(", ")
        ));
    }
    match map.get("name") {
        None | Some(Value::Null) => {
            return fail(format!("{who} needs a name, one word like reset"))
        }
        Some(Value::String(_)) => {}
        Some(other) => {
            return fail(format!(
                "{who} name must be one word like reset, not {}",
                describe(other)
            ))
        }
    }
    match (map.get("run"), map.get("adapter")) {
        (None | Some(Value::Null), None | Some(Value::Null))
            if map
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| n == RESEARCH || n == FORECAST || PARENT_LEVERS.contains(&n)) => {}
        (None | Some(Value::Null), None | Some(Value::Null)) => {
            return fail(format!(
                "{who} needs run: the command to run, like ./reset.sh; or adapter: a built-in action like github.issue owner/repo"
            ))
        }
        (Some(Value::String(_)), None | Some(Value::Null))
        | (None | Some(Value::Null), Some(Value::String(_))) => {}
        (Some(Value::String(_)), Some(Value::String(_))) => {
            return fail(format!("{who} has both run and adapter; choose one"))
        }
        (Some(other), _) if !matches!(other, Value::String(_) | Value::Null) => {
            return fail(format!(
                "{who} run must be the command to run, as text, not {}",
                describe(other)
            ))
        }
        (_, Some(other)) => {
            return fail(format!(
                "{who} adapter must be a built-in action, as text, not {}",
                describe(other)
            ))
        }
        _ => {}
    }
    match map.get("cap") {
        None | Some(Value::Null) => {
            return fail(format!(
                "{who} needs a cap: how often it may run, like 3/day"
            ))
        }
        Some(Value::String(cap)) => {
            cap.parse::<Cap>()
                .map_err(|e| MandateError(format!("{who} cap: {e}")))?;
        }
        Some(other) => {
            return fail(format!(
                "{who} cap must be text like 3/day or 1/hour, not {}",
                describe(other)
            ))
        }
    }
    match map.get("reversible") {
        None | Some(Value::Null) => {
            return fail(format!(
                "{who} needs reversible: true if it can be undone, false if not; that is for you to decide, not the daemon"
            ))
        }
        Some(Value::Bool(_)) => {}
        Some(other) => {
            return fail(format!(
                "{who} reversible must be true or false, not {}",
                describe(other)
            ))
        }
    }
    match map.get("delegated") {
        None | Some(Value::Bool(_)) => {}
        Some(other) => {
            return fail(format!(
                "{who} delegated must be true or false, not {}",
                describe(other)
            ))
        }
    }
    match map.get("takes") {
        None | Some(Value::Null) => {}
        Some(Value::String(t)) => {
            t.parse::<Takes>()
                .map_err(|_| MandateError(format!("{who} takes is {}, not {t:?}", Takes::ALL)))?;
        }
        Some(other) => {
            return fail(format!(
                "{who} takes is {}, not {}",
                Takes::ALL,
                describe(other)
            ))
        }
    }
    match map.get("fields") {
        None | Some(Value::Null) => {}
        Some(Value::Mapping(fields)) => {
            if map.get("takes").and_then(Value::as_str).map(str::trim) != Some("json") {
                return fail(format!(
                    "{who} fields describe a json payload, so the lever must say takes: json"
                ));
            }
            if fields.is_empty() {
                return fail(format!(
                    "{who} fields is empty; name each field and its type, like qty: integer, or leave fields out"
                ));
            }
            for (name, ty) in fields {
                let ok = matches!(ty.as_str(), Some("string" | "number" | "integer" | "bool"));
                if !ok || name.as_str().is_none() {
                    return fail(format!(
                        "{who} fields: {} is {}, not {}",
                        name.as_str().unwrap_or("a field"),
                        describe(ty),
                        FieldType::ALL
                    ));
                }
            }
        }
        Some(other) => {
            return fail(format!(
                "{who} fields is a map of field name to type, like {{qty: integer}}, not {}",
                describe(other)
            ))
        }
    }
    match map.get("check") {
        None | Some(Value::Null) => {}
        Some(Value::String(c)) if !c.trim().is_empty() => {}
        Some(other) => {
            return fail(format!(
                "{who} check must be the command to run, like ./check-sender, not {}",
                describe(other)
            ))
        }
    }
    match map.get("cost") {
        None => {}
        Some(value) => match value.as_u64() {
            Some(n) if n <= u64::from(u32::MAX) => {}
            _ => {
                return fail(format!(
                    "{who} cost is a whole number of what one run spends, not {}",
                    describe(value)
                ))
            }
        },
    }
    match map.get("expects") {
        None | Some(Value::Null) => {}
        Some(Value::String(e)) => {
            e.parse::<Expects>()
                .map_err(|e| MandateError(format!("{who} {e}")))?;
        }
        Some(other) => {
            return fail(format!(
                "{who} expects is text like up 5 within 3, not {}",
                describe(other)
            ))
        }
    }
    match map.get("revoke_after") {
        None | Some(Value::Null) => Ok(()),
        Some(value) => match value.as_u64() {
            Some(n) if (1..=u64::from(u32::MAX)).contains(&n) => {
                if map.get("delegated") == Some(&Value::Bool(true)) {
                    Ok(())
                } else {
                    fail(format!(
                        "{who} revoke_after takes back a delegation, so the lever must be delegated: true"
                    ))
                }
            }
            _ => fail(format!(
                "{who} revoke_after is a whole number of misses in a row, 1 or more, not {}",
                describe(value)
            )),
        },
    }
}

fn check_forbidden(v: Option<&Value>) -> Result<(), MandateError> {
    let items = match v {
        None => return Ok(()),
        Some(Value::Null) => {
            return fail(
                "forbidden is empty; list the actions the venture may never take, like [rm], or leave the field out",
            )
        }
        Some(Value::Sequence(items)) => items,
        Some(other) => {
            return fail(format!(
                "forbidden must be a list of action names, like [rm], not {}",
                describe(other)
            ))
        }
    };
    for item in items {
        if !matches!(item, Value::String(_)) {
            return fail(format!(
                "forbidden names actions, one word each like rm, not {}",
                describe(item)
            ));
        }
    }
    Ok(())
}

fn check_cadence(v: Option<&Value>) -> Result<(), MandateError> {
    match v {
        None | Some(Value::Null) => fail(NEEDS_CADENCE),
        Some(Value::Number(n)) => bare_cadence(n),
        Some(Value::String(s)) if s.trim().parse::<f64>().is_ok() => bare_cadence(&s.trim()),
        Some(Value::String(s)) => match humantime::parse_duration(s.trim()) {
            Ok(_) => Ok(()),
            Err(_) => fail(format!(
                "cadence must be a duration like 30s, 5m or 1h, not {s:?}"
            )),
        },
        Some(other) => fail(format!(
            "cadence must be a duration like 30s, 5m or 1h, not {}",
            describe(other)
        )),
    }
}

fn bare_cadence(n: &dyn fmt::Display) -> Result<(), MandateError> {
    fail(format!(
        "cadence is a duration like 30s, 5m or 1h; {n} alone does not say whether that is seconds or minutes"
    ))
}

fn check_context_budget(v: Option<&Value>) -> Result<(), MandateError> {
    match v {
        None => Ok(()),
        Some(value) => match value.as_u64() {
            Some(n) if (1..=u64::from(u32::MAX)).contains(&n) => Ok(()),
            _ => fail(format!("{BUDGET}, not {}", describe(value))),
        },
    }
}

fn check_restart(v: Option<&Value>) -> Result<(), MandateError> {
    match v {
        None => Ok(()),
        Some(Value::String(s)) if matches!(s.as_str(), "always" | "on-failure" | "never") => Ok(()),
        Some(Value::String(s)) => fail(format!(
            "restart must be always, on-failure or never, not {s:?}"
        )),
        Some(other) => fail(format!(
            "restart must be always, on-failure or never, not {}",
            describe(other)
        )),
    }
}

/// A YAML or serde error in plain words, with the line number when the
/// error has one. serde_yaml reports an unknown position as line 1 column 1
/// but leaves it out of its message, so the message is trusted over the
/// position: a line is named only when it is real.
fn plain(e: &serde_yaml::Error) -> String {
    let text = e.to_string();
    let msg = rewrite(&strip_marks(&text));
    match e.location() {
        Some(at) if text.contains(" at line ") => format!("line {}: {msg}", at.line()),
        _ => msg,
    }
}

/// `text` without its ` at line N column M` marks.
fn strip_marks(text: &str) -> String {
    const AT: &str = " at line ";
    const COLUMN: &str = " column ";
    let digits = |s: &str| s.len() - s.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(AT) {
        let after = &rest[i + AT.len()..];
        let line = digits(after);
        let after_line = &after[line..];
        if line > 0 && after_line.starts_with(COLUMN) {
            let after_column = &after_line[COLUMN.len()..];
            let column = digits(after_column);
            if column > 0 {
                out.push_str(&rest[..i]);
                rest = &after_column[column..];
                continue;
            }
        }
        out.push_str(&rest[..i + AT.len()]);
        rest = after;
    }
    out.push_str(rest);
    out
}

/// A serde message said plainly. The path serde puts in front of a message,
/// like `controller.kind: `, is kept; its jargon and backticks are not.
fn rewrite(msg: &str) -> String {
    if msg.contains("more than one document") {
        return "a mandate is one YAML document; remove the --- that starts another".to_string();
    }
    if msg.contains("did not match any variant") {
        return "this value does not have the expected shape".to_string();
    }
    if let Some((path, key)) = msg.split_once("duplicate entry with key ") {
        return format!("{path}the field {key} appears twice; keep one");
    }
    if msg == "EOF while parsing a value" {
        return "the text ends before the value is complete".to_string();
    }
    if msg.starts_with("found character that cannot start any token") {
        return "found a character YAML cannot read here; if it is a tab, indent with spaces instead"
            .to_string();
    }
    let mut out = msg.replace("unknown variant", "unknown choice");
    if let Some((path, rest)) = out
        .split_once("invalid type: ")
        .or_else(|| out.split_once("invalid value: "))
    {
        if let Some((found, expected)) = rest.split_once(", expected ") {
            out = format!(
                "{path}expected {}, found {}",
                expected_in_words(expected),
                found_in_words(found)
            );
        }
    }
    out.replace('`', "").replace("variant", "choice")
}

/// What serde says it expected, in plain words.
fn expected_in_words(expected: &str) -> String {
    match expected.trim_matches('`') {
        "a string" | "string" => "text".to_string(),
        "a boolean" | "bool" => "true or false".to_string(),
        "a sequence" => "a list".to_string(),
        "a map" => "a set of fields".to_string(),
        "f32" | "f64" => "a number".to_string(),
        "variant identifier" => "the name of a choice".to_string(),
        "field identifier" => "the name of a field".to_string(),
        e if e.starts_with("struct ") => "a set of fields".to_string(),
        "u8" | "u16" | "u32" | "u64" | "u128" | "usize" | "i8" | "i16" | "i32" | "i64" | "i128"
        | "isize" => "a whole number".to_string(),
        e => e.to_string(),
    }
}

/// What serde says it found, in plain words.
fn found_in_words(found: &str) -> String {
    let found = found.replace('`', "");
    match found.as_str() {
        "unit value" | "unit" => "nothing".to_string(),
        "sequence" => "a list".to_string(),
        "map" => "a set of fields".to_string(),
        _ => found
            .trim_start_matches("string ")
            .trim_start_matches("integer ")
            .trim_start_matches("floating point ")
            .trim_start_matches("boolean ")
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "\
purpose: Keep the counter from running away.
metric: count
controller:
  kind: threshold
  above: 10
  action: reset
cadence: 30s
context_budget: 4000
restart: on-failure
permitted:
  - name: reset
    run: ./reset.sh
    cap: 3/day
    reversible: true
  - name: deploy
    run: ./deploy.sh
    cap: 1/day
    reversible: false
    delegated: false
forbidden: [rm]
";

    /// The valid text with one piece replaced; an empty `to` removes it.
    fn edit(from: &str, to: &str) -> String {
        assert!(VALID.contains(from), "the valid text has no {from:?}");
        VALID.replace(from, to)
    }

    /// The message a text fails with, checked for jargon.
    fn fails(text: &str) -> String {
        let msg = Mandate::parse(text).expect_err("should not parse").0;
        for jargon in ["variant", "serde", "untagged", "invalid type", "`"] {
            assert!(!msg.contains(jargon), "{jargon:?} in: {msg}");
        }
        msg
    }

    #[test]
    fn guards_audits_range_deadband_and_hold_parse_and_round_trip() {
        let text = edit(
            "  action: reset\n",
            "  action: reset\n  deadband: 2\n  hold: 3\n",
        ) + "guards:
  - {name: spend, run: ./spend, above: 100}
  - {name: errors, adapter: http.status https://example.com/health, above: 299, then: ask}
  - {name: dd, drawdown: 15%, of: count}
audits:
  - {name: refunds, run: ./refunds, worse: up, tolerance: 2}
range: [0, 100000]
max_step: 5000
";
        let m = Mandate::parse(&text).unwrap();
        assert_eq!(m.deadband_hold(), (2.0, 3));
        assert_eq!(m.guards[0].then, Then::Pause);
        assert_eq!(m.guards[1].then, Then::Ask);
        assert_eq!(m.guards[2].bound(), (true, 15.0));
        assert!(m.guards[2].breached(15.0) && !m.guards[2].breached(14.9));
        assert!(m.guards[0].breached(101.0) && !m.guards[0].breached(100.0));
        assert_eq!(m.audits[0].window, 5);
        assert_eq!(m.range, Some([0.0, 100000.0]));
        let back: Mandate = serde_yaml::from_str(&serde_yaml::to_string(&m).unwrap()).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn guards_audits_and_ranges_say_what_is_wrong() {
        let with = |extra: &str| fails(&format!("{VALID}{extra}"));
        for (extra, says) in [
            ("guards:\n  - {name: g, run: ./g}\n", "needs a bound"),
            (
                "guards:\n  - {name: g, run: ./g, above: 1, below: 0}\n",
                "not both",
            ),
            ("guards:\n  - {name: g, above: 1}\n", "exactly one of run"),
            (
                "guards:\n  - {name: g, drawdown: 15, of: count}\n",
                "a percent like 15%",
            ),
            (
                "guards:\n  - {name: g, drawdown: 15%, of: other}\n",
                "of the venture's metric",
            ),
            ("guards:\n  - {name: g, drawdown: 15%}\n", "needs of"),
            (
                "guards:\n  - {name: g, run: ./g, above: 1, bogus: 2}\n",
                "unknown field \"bogus\" in guards",
            ),
            (
                "guards:\n  - {name: g, run: ./reset.sh, above: 1}\n",
                "the command of lever \"reset\"",
            ),
            (
                "guards:\n  - {name: g, run: report, above: 1}\n",
                "how the metric is read",
            ),
            (
                "guards:\n  - {name: g, run: a, above: 1}\n  - {name: g, run: b, above: 1}\n",
                "listed twice",
            ),
            ("guards: 3\n", "guards is a list"),
            (
                "audits:\n  - {name: a, run: ./a, worse: sideways}\n",
                "worse is up or down",
            ),
            (
                "audits:\n  - {name: a, run: ./a, worse: up, better: left}\n",
                "better is up or down",
            ),
            ("audits:\n  - {name: a, worse: up}\n", "exactly one of run"),
            (
                "audits:\n  - {name: a, run: ./a, worse: up, tolerance: -1}\n",
                "tolerance",
            ),
            (
                "audits:\n  - {name: a, run: ./a, worse: up, window: 1}\n",
                "window",
            ),
            ("range: [5, 1]\n", "low then high"),
            (
                "guards:\n  - {name: g, run: ./g, above: 1, range: [5, 1]}\n",
                "guard \"g\": range is low then high",
            ),
            (
                "audits:\n  - {name: a, run: ./a, worse: up, max_step: 0}\n",
                "audit \"a\": max_step must be a number above 0",
            ),
            ("range: 5\n", "two numbers"),
            ("max_step: 0\n", "above 0"),
            ("max_step: big\n", "max_step is a number"),
        ] {
            let msg = with(extra);
            assert!(msg.contains(says), "{extra:?}: {msg}");
        }
        let msg = fails(&edit("  action: reset\n", "  action: reset\n  hold: 0\n"));
        assert!(msg.contains("hold must be a whole number"), "{msg}");
        let msg = fails(&edit(
            "  action: reset\n",
            "  action: reset\n  deadband: -1\n",
        ));
        assert!(msg.contains("deadband must be a number 0 or more"), "{msg}");
    }

    #[test]
    fn valid_text_parses() {
        let m = Mandate::parse(VALID).unwrap();
        assert_eq!(m.metric, "count");
        assert_eq!(m.cadence, Duration::from_secs(30));
        assert_eq!(
            m.permitted("reset").unwrap().cap,
            Cap {
                count: 3,
                per: Cap::DAY
            }
        );
        assert!(m.permitted("rm").is_none());
        assert_eq!(m.restart, Restart::OnFailure);
    }

    #[test]
    fn defaults_apply_when_fields_are_left_out() {
        let text = edit("context_budget: 4000\n", "").replace("restart: on-failure\n", "");
        let m = Mandate::parse(&text).unwrap();
        assert_eq!(m.context_budget, 8000);
        assert_eq!(m.restart, Restart::OnFailure);
        let m =
            Mandate::parse("purpose: watch\nmetric: n\ncontroller: {kind: model}\ncadence: 1h\n")
                .unwrap();
        assert!(m.permitted.is_empty());
        assert!(m.forbidden.is_empty());
    }

    #[test]
    fn empty_text_is_not_a_mandate() {
        for text in ["", "   \n", "# only a comment\n"] {
            let msg = fails(text);
            assert!(msg.contains("the mandate is empty"), "{msg}");
            assert!(msg.contains("a mandate is a list of fields like purpose, metric, controller"));
        }
    }

    #[test]
    fn text_that_is_not_a_mapping_is_not_a_mandate() {
        let msg = fails("- purpose\n- metric\n");
        assert_eq!(
            msg,
            "a mandate is a list of fields like purpose, metric, controller, not a list"
        );
        let msg = fails("just words");
        assert!(msg.ends_with("not the text \"just words\""), "{msg}");
    }

    #[test]
    fn confirm_defaults_on_turns_off_only_by_a_human_and_is_a_limit() {
        let on = Mandate::parse(&(VALID.to_string() + "max_step: 5\n")).unwrap();
        assert!(on.confirm);
        let text = VALID.to_string()
            + "max_step: 5\nconfirm: false\nguards:\n  - {name: g, run: ./g, above: 1, max_step: 2, confirm: false}\naudits:\n  - {name: a, run: ./a, worse: up, max_step: 2, confirm: false}\n";
        let off = Mandate::parse(&text).unwrap();
        assert!(!off.confirm && !off.guards[0].confirm && !off.audits[0].confirm);
        // Waiting is a guard: turning it off, or on, is a limit edit,
        // never the improving loop's.
        assert_eq!(edits_limits(&on, &off).as_deref(), Some("audits"));
        let mut only = on.clone();
        only.confirm = false;
        assert_eq!(edits_limits(&on, &only).as_deref(), Some("confirm"));
        assert_eq!(edits_limits(&only, &on).as_deref(), Some("confirm"));
        assert!(fails(&(VALID.to_string() + "confirm: maybe\n")).contains("true or false"));
    }

    #[test]
    fn unknown_field_is_named_with_the_known_ones() {
        let msg = fails(&edit("metric: count", "metrik: count"));
        assert_eq!(
            msg,
            "unknown field \"metrik\"; a mandate has these fields: purpose, metric, controller, permitted, forbidden, cadence, context_budget, summarize_after, restart, network, children, parent, allocation, guards, audits, range, max_step, confirm, escalate, world, calibration, early_warning, better"
        );
        let msg = fails("1: a\n");
        assert_eq!(msg, "fields are named with words, not the number 1");
    }

    #[test]
    fn purpose_is_required_text() {
        assert_eq!(
            fails(&edit("purpose: Keep the counter from running away.\n", "")),
            NEEDS_PURPOSE
        );
        assert_eq!(
            fails(&edit(
                "purpose: Keep the counter from running away.",
                "purpose:"
            )),
            NEEDS_PURPOSE
        );
        assert_eq!(
            fails(&edit(
                "purpose: Keep the counter from running away.",
                "purpose: \"\""
            )),
            NEEDS_PURPOSE
        );
        let msg = fails(&edit(
            "purpose: Keep the counter from running away.",
            "purpose: 42",
        ));
        assert_eq!(
            msg,
            "purpose must be text saying what the venture is for, not the number 42"
        );
    }

    #[test]
    fn missing_metric_says_what_a_metric_is() {
        for text in [
            edit("metric: count\n", ""),
            edit("metric: count", "metric:"),
            edit("metric: count", "metric: \"\""),
            edit("metric: count", "metric: ~"),
        ] {
            assert_eq!(fails(&text), NEEDS_METRIC);
        }
    }

    #[test]
    fn expects_parses_with_and_without_a_size() {
        let e: Expects = "up 5 within 3".parse().unwrap();
        assert_eq!(
            e,
            Expects {
                direction: Direction::Up,
                size: Some(5.0),
                within: 3
            }
        );
        let e: Expects = "down within 2".parse().unwrap();
        assert_eq!(e.direction, Direction::Down);
        assert_eq!(e.size, None);
        assert_eq!(e.to_string(), "down within 2");
        for bad in [
            "",
            "sideways 5 within 3",
            "up 5",
            "up 0 within 3",
            "up -2 within 3",
            "up 5 within 0",
            "up 5 within",
        ] {
            assert!(bad.parse::<Expects>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn expects_and_revoke_after_are_checked_in_the_mandate() {
        let with = |fields: &str| {
            edit(
                "    delegated: false\n",
                &format!("    delegated: true\n    {fields}\n"),
            )
        };
        let m = Mandate::parse(&with("expects: up 5 within 3\n    revoke_after: 2")).unwrap();
        let p = m.permitted("deploy").unwrap();
        assert_eq!(p.expects.unwrap().within, 3);
        assert_eq!(p.revoke_after, Some(2));
        assert!(fails(&with("expects: sideways")).contains("expects looks like"));
        assert!(fails(&with("revoke_after: 0")).contains("revoke_after"));
        let msg = fails(&edit("    delegated: false\n", "    revoke_after: 2\n"));
        assert!(msg.contains("must be delegated: true"), "{msg}");
        // Old mandates parse unchanged.
        assert!(Mandate::parse(VALID)
            .unwrap()
            .permitted("deploy")
            .unwrap()
            .expects
            .is_none());
    }

    #[test]
    fn a_shell_metric_may_carry_a_label() {
        let with = |metric: &str| edit("metric: count", &format!("metric: {metric}"));
        let m = Mandate::parse(&with("{shell: \"echo 7\", label: temperature}")).unwrap();
        assert_eq!(m.metric_name(), "temperature");
        assert_eq!(m.metric_adapter().unwrap().name(), "shell");
        assert_eq!(m.metric.label(), Some("temperature"));
        // It survives a round trip.
        let back = Mandate::parse(&serde_yaml::to_string(&m).unwrap()).unwrap();
        assert_eq!(back, m);
        // The text form still works and is named shell.
        let plain = Mandate::parse(&with("shell echo 7")).unwrap();
        assert_eq!(plain.metric_name(), "shell");
        assert_eq!(plain.metric.label(), None);
        for (bad, says) in [
            ("{shell: echo 7}", "label: <name>"),
            ("{label: t}", "not a set of fields"),
            ("{shell: echo 7, label: two words}", "one word"),
            ("{shell: echo 7, label: t, more: 1}", "more"),
            ("{shell: \"\", label: t}", "label: <name>"),
        ] {
            let msg = fails(&with(bad));
            assert!(msg.contains(says), "{bad}: {msg}");
        }
    }

    #[test]
    fn fields_describe_a_json_payload() {
        let text = VALID.replace(
            "    delegated: false\n",
            "    delegated: false\n    takes: json\n    fields: {qty: integer, note: string, ok: bool, w: number}\n",
        );
        let m = Mandate::parse(&text).unwrap();
        let f = m.permitted("deploy").unwrap().fields.clone().unwrap();
        assert_eq!(f["qty"], FieldType::Integer);
        assert_eq!(f.len(), 4);
        let bad = |fields: &str, takes: &str| {
            fails(&VALID.replace(
                "    delegated: false\n",
                &format!("    delegated: false\n{takes}    fields: {fields}\n"),
            ))
        };
        assert!(bad("{qty: integer}", "").contains("must say takes: json"));
        assert!(bad("{qty: integer}", "    takes: text\n").contains("must say takes: json"));
        assert!(bad("{qty: int}", "    takes: json\n").contains("string, number, integer or bool"));
        assert!(bad("{}", "    takes: json\n").contains("fields is empty"));
        assert!(bad("[qty]", "    takes: json\n").contains("map of field name to type"));
        // Old mandates parse unchanged.
        assert!(Mandate::parse(VALID)
            .unwrap()
            .permitted("deploy")
            .unwrap()
            .fields
            .is_none());
    }

    #[test]
    fn metric_must_be_one_word() {
        let msg = fails(&edit("metric: count", "metric: request count"));
        assert_eq!(
            msg,
            "metric must be one word like count or latency_ms, not \"request count\""
        );
        let msg = fails(&edit("metric: count", "metric: 42"));
        assert_eq!(
            msg,
            "metric must be one word like count or latency_ms, not the number 42"
        );
        let msg = fails(&edit("metric: count", "metric: [count, errors]"));
        assert_eq!(
            msg,
            "metric must be one word like count or latency_ms, not a list"
        );
        let msg = fails(&edit("metric: count", "metric: {name: count}"));
        assert!(msg.ends_with("not a set of fields"), "{msg}");
        for word in ["latency_ms", "p99.latency", "errors-per-minute", "count2"] {
            let m = Mandate::parse(&edit("metric: count", &format!("metric: {word}"))).unwrap();
            assert_eq!(m.metric, word);
        }
    }

    #[test]
    fn controller_is_required_with_a_known_kind() {
        let text = edit(
            "controller:\n  kind: threshold\n  above: 10\n  action: reset\n",
            "",
        );
        assert_eq!(fails(&text), NEEDS_CONTROLLER);
        let text = edit(
            "controller:\n  kind: threshold\n  above: 10\n  action: reset\n",
            "controller: threshold\n",
        );
        assert_eq!(
            fails(&text),
            "controller must be a set of fields starting with kind: threshold, band, classifier, model or plan, not the text \"threshold\""
        );
        let msg = fails(&edit("  kind: threshold\n", ""));
        assert_eq!(
            msg,
            "controller needs a kind: threshold, band, classifier, model or plan"
        );
        let msg = fails(&edit("kind: threshold", "kind: magic"));
        assert_eq!(
            msg,
            "controller kind must be threshold, band, classifier, model or plan, not \"magic\""
        );
        let msg = fails(&edit("kind: threshold", "kind: 3"));
        assert!(msg.ends_with("not the number 3"), "{msg}");
    }

    #[test]
    fn controller_unknown_field_lists_the_fields_of_its_kind() {
        let msg = fails(&edit("  above: 10\n", "  above: 10\n  over: 10\n"));
        assert_eq!(
            msg,
            "unknown field \"over\" in controller; a threshold controller has these fields: kind, above, below, action, deadband, hold"
        );
        let text = edit(
            "controller:\n  kind: threshold\n  above: 10\n  action: reset\n",
            "controller:\n  kind: model\n  action: reset\n",
        );
        let msg = fails(&text);
        assert_eq!(
            msg,
            "unknown field \"action\" in controller; a model controller has these fields: kind, backend"
        );
    }

    #[test]
    fn threshold_needs_exactly_one_numeric_bound() {
        let msg = fails(&edit("  above: 10\n", ""));
        assert_eq!(
            msg,
            "a threshold controller needs a bound: above or below, a number like 10"
        );
        let msg = fails(&edit("  above: 10\n", "  above: 10\n  below: 2\n"));
        assert_eq!(
            msg,
            "a threshold controller takes one bound, above or below, not both"
        );
        let msg = fails(&edit("above: 10", "above: ten"));
        assert_eq!(
            msg,
            "threshold above must be a number like 10, not the text \"ten\""
        );
        let msg = fails(&edit("above: 10", "below: [1]"));
        assert_eq!(msg, "threshold below must be a number like 10, not a list");
        let msg = fails(&edit("above: 10", "above: .inf"));
        assert_eq!(
            msg,
            "threshold above must be an ordinary number like 10, not inf"
        );
        let m = Mandate::parse(&edit("above: 10", "below: -2.5")).unwrap();
        assert_eq!(
            m.controller,
            Controller::Threshold {
                above: None,
                below: Some(-2.5),
                action: "reset".into(),
                deadband: None,
                hold: None,
            }
        );
    }

    #[test]
    fn threshold_action_is_a_permitted_action_or_stop() {
        let msg = fails(&edit("  action: reset\n", ""));
        assert_eq!(msg, NEEDS_ACTION);
        let msg = fails(&edit("action: reset", "action: \"\""));
        assert_eq!(msg, NEEDS_ACTION);
        let msg = fails(&edit("action: reset", "action: [reset]"));
        assert_eq!(
            msg,
            "threshold action must be the name of a permitted action, or stop, not a list"
        );
        let msg = fails(&edit("action: reset", "action: restart"));
        assert_eq!(
            msg,
            "threshold action \"restart\" is not permitted; name one of the permitted actions (reset, deploy) or the built-in stop"
        );
        let msg = fails("purpose: watch\nmetric: n\ncontroller: {kind: threshold, above: 1, action: reset}\ncadence: 1h\n");
        assert_eq!(
            msg,
            "threshold action \"reset\" is not permitted; this mandate permits no actions, so the only choice is the built-in stop"
        );
        let m = Mandate::parse(&edit("action: reset", "action: stop")).unwrap();
        assert_eq!(
            m.controller,
            Controller::Threshold {
                above: Some(10.0),
                below: None,
                action: STOP.into(),
                deadband: None,
                hold: None,
            }
        );
    }

    #[test]
    fn classifier_and_model_take_an_optional_backend() {
        let threshold = "controller:\n  kind: threshold\n  above: 10\n  action: reset\n";
        let m = Mandate::parse(&edit(threshold, "controller:\n  kind: classifier\n")).unwrap();
        assert_eq!(m.controller, Controller::Classifier { backend: None });
        let m = Mandate::parse(&edit(
            threshold,
            "controller:\n  kind: model\n  backend: local\n",
        ))
        .unwrap();
        assert_eq!(
            m.controller,
            Controller::Model {
                backend: Some("local".into())
            }
        );
        let msg = fails(&edit(
            threshold,
            "controller:\n  kind: model\n  backend: [a, b]\n",
        ));
        assert_eq!(
            msg,
            "controller backend must be the name of a backend, as text, not a list"
        );
    }

    #[test]
    fn permitted_must_be_a_list_of_actions() {
        let msg = fails(&edit("permitted:\n", "permitted: reset\n").replace("  - name: reset\n    run: ./reset.sh\n    cap: 3/day\n    reversible: true\n  - name: deploy\n    run: ./deploy.sh\n    cap: 1/day\n    reversible: false\n    delegated: false\n", ""));
        assert_eq!(
            msg,
            "permitted must be a list of actions, each with a name, run, cap and reversible, not the text \"reset\""
        );
        let msg = fails(&edit(
            "  - name: deploy\n",
            "  - deploy\n  - name: deploy\n",
        ));
        assert_eq!(
            msg,
            "permitted action 2 must be a set of fields (name, run, cap, reversible), not the text \"deploy\""
        );
        let msg = fails(&edit(
            "  - name: deploy\n",
            "  - name: deploy\n    undo: ./undo.sh\n",
        ));
        assert_eq!(
            msg,
            "unknown field \"undo\" in permitted action \"deploy\"; a permitted action has these fields: name, run, adapter, cap, reversible, delegated, takes, cost, check, expects, revoke_after, fields"
        );
    }

    #[test]
    fn permitted_names_are_words_and_unique() {
        let msg = fails(&edit(
            "  - name: deploy\n    run: ./deploy.sh\n",
            "  - run: ./deploy.sh\n",
        ));
        assert_eq!(msg, "permitted action 2 needs a name, one word like reset");
        let msg = fails(&edit("  - name: deploy\n", "  - name: 7\n"));
        assert_eq!(
            msg,
            "permitted action 2 name must be one word like reset, not the number 7"
        );
        let msg = fails(&edit("  - name: deploy\n", "  - name: deploy now\n"));
        assert_eq!(
            msg,
            "a permitted action's name is one word like reset, not \"deploy now\""
        );
        let msg = fails(&edit("  - name: deploy\n", "  - name: reset\n"));
        assert_eq!(
            msg,
            "permitted action \"reset\" is listed twice; each action needs its own name"
        );
    }

    #[test]
    fn permitted_run_is_a_command() {
        let msg = fails(&edit("    run: ./deploy.sh\n", ""));
        assert_eq!(
            msg,
            "permitted action \"deploy\" needs run: the command to run, like ./reset.sh; or adapter: a built-in action like github.issue owner/repo"
        );
        let msg = fails(&edit("run: ./deploy.sh", "run: \"  \""));
        assert_eq!(
            msg,
            "permitted action \"deploy\" has an empty run; give it the command to run, like ./reset.sh"
        );
        let msg = fails(&edit("run: ./deploy.sh", "run: [./deploy.sh]"));
        assert_eq!(
            msg,
            "permitted action \"deploy\" run must be the command to run, as text, not a list"
        );
    }

    #[test]
    fn permitted_cap_is_text_like_three_per_day() {
        let msg = fails(&edit("    cap: 1/day\n", ""));
        assert_eq!(
            msg,
            "permitted action \"deploy\" needs a cap: how often it may run, like 3/day"
        );
        let msg = fails(&edit("cap: 1/day", "cap: 1"));
        assert_eq!(
            msg,
            "permitted action \"deploy\" cap must be text like 3/day or 1/hour, not the number 1"
        );
        // The cap's own message, as is.
        let msg = fails(&edit("cap: 1/day", "cap: once a day"));
        assert_eq!(
            msg,
            format!(
                "permitted action \"deploy\" cap: {}",
                "once a day".parse::<Cap>().unwrap_err()
            )
        );
        assert!(msg.contains("a cap looks like 3/day or 1/hour"));
        let msg = fails(&edit("cap: 1/day", "cap: 0/day"));
        assert!(
            msg.ends_with("a cap of 0 permits nothing; leave the action out instead"),
            "{msg}"
        );
        let m = Mandate::parse(&edit("cap: 1/day", "cap: 2/week")).unwrap();
        assert_eq!(
            m.permitted("deploy").unwrap().cap,
            Cap {
                count: 2,
                per: Cap::WEEK
            }
        );
    }

    #[test]
    fn permitted_reversible_is_the_humans_call() {
        let msg = fails(&edit("    reversible: false\n", ""));
        assert_eq!(
            msg,
            "permitted action \"deploy\" needs reversible: true if it can be undone, false if not; that is for you to decide, not the daemon"
        );
        let msg = fails(&edit("reversible: false", "reversible: no"));
        assert_eq!(
            msg,
            "permitted action \"deploy\" reversible must be true or false, not the text \"no\""
        );
        let msg = fails(&edit("delegated: false", "delegated: yes"));
        assert_eq!(
            msg,
            "permitted action \"deploy\" delegated must be true or false, not the text \"yes\""
        );
        let m = Mandate::parse(&edit("delegated: false", "delegated: true")).unwrap();
        assert!(m.permitted("deploy").unwrap().delegated);
    }

    #[test]
    fn metric_may_name_an_adapter_with_its_argument() {
        let m = Mandate::parse(&edit("metric: count", "metric: stripe.mrr")).unwrap();
        assert_eq!(m.metric_name(), "stripe.mrr");
        assert_eq!(m.metric_adapter(), Some(adapter::Metric::StripeMrr));
        let m = Mandate::parse(&edit(
            "metric: count",
            "metric: github.open_issues 4esv/avsm",
        ))
        .unwrap();
        assert_eq!(m.metric_name(), "github.open_issues");
        assert!(m.metric_adapter().is_some());
        let m = Mandate::parse(&edit(
            "metric: count",
            "metric: shell df --output=pcent / | tail -1",
        ))
        .unwrap();
        assert_eq!(m.metric_name(), "shell");
        let plain = Mandate::parse(VALID).unwrap();
        assert_eq!(plain.metric_name(), "count");
        assert_eq!(plain.metric_adapter(), None);
        let msg = fails(&edit("metric: count", "metric: github.open_issues"));
        assert_eq!(
            msg,
            "metric: github.open_issues needs a repository after it, like: metric: github.open_issues owner/repo"
        );
        let msg = fails(&edit("metric: count", "metric: http.status example.com"));
        assert!(
            msg.starts_with("metric: an address starts with http://"),
            "{msg}"
        );
    }

    #[test]
    fn permitted_may_name_an_adapter_instead_of_a_command() {
        let text = edit(
            "    run: ./deploy.sh\n",
            "    adapter: mail.send me@example.com\n",
        );
        let m = Mandate::parse(&text).unwrap();
        let p = m.permitted("deploy").unwrap();
        assert_eq!(p.run, None);
        assert_eq!(p.takes(), Some(Takes::Text));
        assert!(p.lever_adapter().is_some());
        let msg = fails(&edit(
            "    run: ./deploy.sh\n",
            "    adapter: mail.send me@example.com\n    takes: json\n",
        ));
        assert_eq!(
            msg,
            "permitted action \"deploy\": mail.send takes text; leave takes out or say text"
        );
        let msg = fails(&edit(
            "    run: ./deploy.sh\n    cap: 1/day\n    reversible: false\n",
            "    adapter: mail.send me@example.com\n    cap: 1/day\n    reversible: true\n",
        ));
        assert_eq!(
            msg,
            "permitted action \"deploy\": mail.send cannot be undone; say reversible: false"
        );
        let msg = fails(&edit(
            "    run: ./deploy.sh\n",
            "    run: ./deploy.sh\n    adapter: mail.send me@example.com\n",
        ));
        assert_eq!(
            msg,
            "permitted action \"deploy\" has both run and adapter; choose one"
        );
        let msg = fails(&edit(
            "    run: ./deploy.sh\n",
            "    adapter: slack.post x\n",
        ));
        assert!(msg.contains("not a built-in action"), "{msg}");
        let msg = fails(&edit("    run: ./deploy.sh\n", "    adapter: [x]\n"));
        assert_eq!(
            msg,
            "permitted action \"deploy\" adapter must be a built-in action, as text, not a list"
        );
        // A mandate with nothing but adapters needs no files beside it.
        let m = Mandate::parse(
            "purpose: Watch the shop.\nmetric: stripe.mrr\ncontroller: {kind: model}\ncadence: 1h\n\
             permitted:\n  - {name: nudge, adapter: mail.send me@example.com, cap: 1/day, reversible: false}\n",
        )
        .unwrap();
        assert!(
            m.metric_adapter().is_some() && m.permitted("nudge").unwrap().lever_adapter().is_some()
        );
    }

    #[test]
    fn research_is_permitted_by_name_alone() {
        let text = edit(
            "  - name: deploy\n    run: ./deploy.sh\n    cap: 1/day\n    reversible: false\n    delegated: false\n",
            "  - name: research\n    cap: 5/day\n    reversible: true\n    delegated: true\n",
        );
        let m = Mandate::parse(&text).unwrap();
        let p = m.permitted("research").unwrap();
        assert!(p.is_research());
        assert_eq!(p.takes(), Some(Takes::Text));
        assert!(p.delegated);
        let msg = fails(&text.replace(
            "    reversible: true\n    delegated: true\n",
            "    reversible: false\n",
        ));
        assert_eq!(
            msg,
            "permitted action \"research\" only reads; say reversible: true"
        );
        let msg = fails(&text.replace(
            "  - name: research\n",
            "  - name: research\n    run: ./find\n",
        ));
        assert!(msg.starts_with("research is built in"), "{msg}");
        let msg = fails(&text.replace("    cap: 5/day\n", "    cap: 5/day\n    takes: json\n"));
        assert!(msg.contains("takes text, the question"), "{msg}");
    }

    #[test]
    fn a_check_needs_a_lever_that_takes_a_payload() {
        let m = Mandate::parse(&edit(
            "    reversible: false\n",
            "    reversible: false\n    takes: text\n    check: ./check-sender\n",
        ))
        .unwrap();
        assert_eq!(
            m.permitted("deploy").unwrap().check.as_deref(),
            Some("./check-sender")
        );
        assert_eq!(m.permitted("reset").unwrap().check, None);
        let msg = fails(&edit(
            "    reversible: false\n",
            "    reversible: false\n    check: ./check-sender\n",
        ));
        assert!(msg.contains("has a check, which reads a payload"), "{msg}");
        let msg = fails(&edit(
            "    reversible: false\n",
            "    reversible: false\n    takes: text\n    check: [x]\n",
        ));
        assert!(msg.contains("check must be the command to run"), "{msg}");
    }

    #[test]
    fn permitted_takes_is_one_of_four_shapes() {
        let m = Mandate::parse(&edit(
            "    reversible: false\n",
            "    reversible: false\n    takes: text\n",
        ))
        .unwrap();
        assert_eq!(m.permitted("deploy").unwrap().takes, Some(Takes::Text));
        assert_eq!(m.permitted("reset").unwrap().takes, None);
        for (value, found) in [("blob", "\"blob\""), ("[text]", "a list")] {
            let msg = fails(&edit(
                "    reversible: false\n",
                &format!("    reversible: false\n    takes: {value}\n"),
            ));
            assert_eq!(
                msg,
                format!(
                    "permitted action \"deploy\" takes is text, json, diff or file, not {found}"
                )
            );
        }
        let msg = fails(&edit("  - name: deploy\n", "  - name: stop\n"));
        assert_eq!(
            msg,
            "stop is built in; a permitted action cannot be named \"stop\""
        );
        let msg = fails(&edit("  - name: deploy\n", "  - name: delegate\n"));
        assert!(msg.starts_with("delegate is built in"), "{msg}");
        let msg = fails(&edit("  - name: deploy\n", "  - name: resume\n"));
        assert!(msg.starts_with("resume is built in"), "{msg}");
    }

    #[test]
    fn forbidden_is_a_list_of_names_not_also_permitted() {
        let msg = fails(&edit("forbidden: [rm]", "forbidden: rm"));
        assert_eq!(
            msg,
            "forbidden must be a list of action names, like [rm], not the text \"rm\""
        );
        let msg = fails(&edit("forbidden: [rm]", "forbidden:"));
        assert!(msg.starts_with("forbidden is empty"), "{msg}");
        let msg = fails(&edit("forbidden: [rm]", "forbidden: [rm, 3]"));
        assert_eq!(
            msg,
            "forbidden names actions, one word each like rm, not the number 3"
        );
        let msg = fails(&edit("forbidden: [rm]", "forbidden: [rm -rf]"));
        assert_eq!(
            msg,
            "forbidden names actions, one word each like rm, not \"rm -rf\""
        );
        let msg = fails(&edit("forbidden: [rm]", "forbidden: [rm, deploy]"));
        assert_eq!(
            msg,
            "action \"deploy\" is both permitted and forbidden; choose one"
        );
    }

    #[test]
    fn cadence_is_a_required_duration_of_at_least_a_second() {
        assert_eq!(fails(&edit("cadence: 30s\n", "")), NEEDS_CADENCE);
        let msg = fails(&edit("cadence: 30s", "cadence: 30"));
        assert_eq!(
            msg,
            "cadence is a duration like 30s, 5m or 1h; 30 alone does not say whether that is seconds or minutes"
        );
        let msg = fails(&edit("cadence: 30s", "cadence: \"30\""));
        assert!(msg.starts_with("cadence is a duration like 30s"), "{msg}");
        let msg = fails(&edit("cadence: 30s", "cadence: every minute"));
        assert_eq!(
            msg,
            "cadence must be a duration like 30s, 5m or 1h, not \"every minute\""
        );
        let msg = fails(&edit("cadence: 30s", "cadence: [30s]"));
        assert_eq!(
            msg,
            "cadence must be a duration like 30s, 5m or 1h, not a list"
        );
        let msg = fails(&edit("cadence: 30s", "cadence: 500ms"));
        assert_eq!(msg, "cadence must be at least 1s, not 500ms");
        let msg = fails(&edit("cadence: 30s", "cadence: 0s"));
        assert_eq!(msg, "cadence must be at least 1s, not 0s");
        for (text, secs) in [("30s", 30), ("5m", 300), ("1h", 3600), ("1h 30m", 5400)] {
            let m = Mandate::parse(&edit("cadence: 30s", &format!("cadence: {text}"))).unwrap();
            assert_eq!(m.cadence, Duration::from_secs(secs), "{text}");
        }
    }

    #[test]
    fn context_budget_is_a_whole_number_of_tokens() {
        for (value, found) in [
            ("0", "the number 0"),
            ("-5", "the number -5"),
            ("1.5", "the number 1.5"),
            ("4294967296", "the number 4294967296"),
            ("lots", "the text \"lots\""),
            ("", "an empty value"),
        ] {
            let msg = fails(&edit(
                "context_budget: 4000",
                &format!("context_budget: {value}"),
            ));
            assert_eq!(msg, format!("{BUDGET}, not {found}"));
        }
    }

    #[test]
    fn summarize_after_is_a_count_of_records() {
        assert_eq!(Mandate::parse(VALID).unwrap().summarize_after, 200);
        let m = Mandate::parse(&format!("{VALID}summarize_after: 40\n")).unwrap();
        assert_eq!(m.summarize_after, 40);
        let msg = fails(&format!("{VALID}summarize_after: 1\n"));
        assert_eq!(msg, format!("{SUMMARIZE}, not the number 1"));
        let msg = fails(&format!("{VALID}summarize_after: many\n"));
        assert_eq!(msg, format!("{SUMMARIZE}, not the text \"many\""));
    }

    #[test]
    fn restart_is_one_of_three_words() {
        let msg = fails(&edit("restart: on-failure", "restart: sometimes"));
        assert_eq!(
            msg,
            "restart must be always, on-failure or never, not \"sometimes\""
        );
        let msg = fails(&edit("restart: on-failure", "restart: true"));
        assert_eq!(
            msg,
            "restart must be always, on-failure or never, not the value true"
        );
        let m = Mandate::parse(&edit("restart: on-failure", "restart: never")).unwrap();
        assert_eq!(m.restart, Restart::Never);
    }

    #[test]
    fn children_are_names_or_names_with_terms() {
        let with = |children: &str| format!("{VALID}children: {children}\n");
        let m = Mandate::parse(&with("[a, b]")).unwrap();
        assert_eq!(m.children, vec![Child::named("a"), Child::named("b")]);
        let m = Mandate::parse(&with("[{name: a, priority: 2, floor: 3}, b]")).unwrap();
        assert_eq!(
            m.children,
            vec![
                Child {
                    name: "a".into(),
                    priority: 2,
                    floor: 3
                },
                Child::named("b")
            ]
        );
        let m = Mandate::parse(&with("[{name: a}, {name: b, floor: 1}]")).unwrap();
        assert_eq!(m.child("a"), Some(&Child::named("a")));
        assert_eq!(m.child("b").map(|c| (c.priority, c.floor)), Some((0, 1)));
        // The terms survive being written out and read again; a bare name
        // is written bare.
        let m = Mandate::parse(&with("[{name: a, priority: -1, floor: 3}, b]")).unwrap();
        let text = serde_yaml::to_string(&m).unwrap();
        assert!(text.contains("- b\n"), "{text}");
        assert_eq!(Mandate::parse(&text).unwrap(), m);

        assert_eq!(
            fails(&with("[{priority: 2}]")),
            "a child written with terms needs a name, like {name: shop, floor: 3}"
        );
        assert_eq!(
            fails(&with("[{name: a, flor: 3}]")),
            "unknown child field \"flor\"; a child has these fields: name, priority, floor"
        );
        assert_eq!(
            fails(&with("[{name: a, floor: -1}]")),
            "floor is a whole number, the least budget, not the number -1"
        );
        assert_eq!(
            fails(&with("[{name: a, priority: high}]")),
            "priority is a whole number, higher first, not the text \"high\""
        );
        assert_eq!(
            fails(&with("[{name: self}]")),
            "self cannot be a child; it belongs to no one"
        );
        assert_eq!(
            fails(&with("[{name: two words}]")),
            "children names ventures, one word each, not \"two words\""
        );
        assert_eq!(
            fails(&with("[3]")),
            "children names ventures, one word each, not the number 3"
        );
    }

    #[test]
    fn allocation_is_a_deadband_and_a_hold() {
        let with = |allocation: &str| format!("{VALID}allocation: {allocation}\n");
        let m = Mandate::parse(&with("{deadband: 2, hold: 1d}")).unwrap();
        assert_eq!(
            m.allocation,
            Some(Allocation {
                deadband: 2,
                hold: Duration::from_secs(86_400),
                total: None,
            })
        );
        let m = Mandate::parse(&with("{deadband: 2}")).unwrap();
        assert_eq!(m.allocation.unwrap().hold, Duration::ZERO);
        assert_eq!(Mandate::parse(VALID).unwrap().allocation, None);
        let m = Mandate::parse(&with("{deadband: 2, hold: 90m}")).unwrap();
        assert_eq!(
            Mandate::parse(&serde_yaml::to_string(&m).unwrap()).unwrap(),
            m
        );
        assert_eq!(
            fails(&with("{deadbnd: 2}")),
            "unknown allocation field \"deadbnd\"; allocation has: deadband, hold, total"
        );
        assert_eq!(
            fails(&with("{deadband: -2}")),
            "deadband is a whole number, the smallest budget change, not the number -2"
        );
        assert_eq!(
            fails(&with("{hold: soon}")),
            "hold is a length of time, like 1d, not the text \"soon\""
        );
        assert_eq!(
            fails(&with("3")),
            "allocation is {deadband: 2, hold: 1d}, not the number 3"
        );
    }

    #[test]
    fn an_allocation_total_parses_and_holds_the_floors() {
        let parent = |terms: &str| {
            format!(
                "{VALID}children:\n  - {{name: shop, floor: 3}}\n  - {{name: farm, floor: 1}}\n\
                 allocation: {terms}\n"
            )
        };
        let m = Mandate::parse(&parent("{deadband: 2, hold: 5d, total: 6}")).unwrap();
        assert_eq!(m.allocation.as_ref().and_then(|a| a.total), Some(6));
        assert_eq!(
            Mandate::parse(&serde_yaml::to_string(&m).unwrap()).unwrap(),
            m
        );
        // The floors alone may fill the total, not pass it.
        assert!(Mandate::parse(&parent("{total: 4}")).is_ok());
        assert_eq!(
            fails(&parent("{total: 3}")),
            "the children's floors add up to 4, past the allocation total of 3; no budgets could be set"
        );
        assert_eq!(
            fails(&parent("{total: -6}")),
            "total is a whole number, the most the children's budgets add up to, not the number -6"
        );
        assert_eq!(
            fails(&parent("{total: lots}")),
            "total is a whole number, the most the children's budgets add up to, not the text \"lots\""
        );
        // The total is a limit: the improving loop never edits it.
        let raised = Mandate::parse(&parent("{deadband: 2, hold: 5d, total: 20}")).unwrap();
        assert_eq!(
            edits_limits(&m, &raised).as_deref(),
            Some("allocation.total")
        );
        let dropped = Mandate::parse(&parent("{deadband: 2, hold: 5d}")).unwrap();
        assert_eq!(
            edits_limits(&m, &dropped).as_deref(),
            Some("allocation.total")
        );
    }

    #[test]
    fn parents_list_children_and_pull_parent_levers() {
        let parent =
            "purpose: Allocate.\nmetric: shell echo 1\ncontroller: {kind: model}\ncadence: 1h\n\
                      children: [shop, farm]\n\
                      permitted:\n\
                      - {name: budget, cap: 9/day, reversible: true, delegated: true}\n\
                      - {name: pause, cap: 9/day, reversible: true}\n\
                      - {name: resume, cap: 9/day, reversible: true}\n\
                      - {name: threshold, cap: 9/day, reversible: true}\n";
        let m = Mandate::parse(parent).unwrap();
        assert_eq!(m.children, vec![Child::named("shop"), Child::named("farm")]);
        for lever in PARENT_LEVERS {
            let p = m.permitted(lever).unwrap();
            assert!(p.is_parent_lever());
            assert_eq!(p.takes(), Some(Takes::Text));
        }
        let child = Mandate::parse(&format!("{VALID}parent: portfolio\n")).unwrap();
        assert_eq!(child.parent.as_deref(), Some("portfolio"));
        let m =
            Mandate::parse(&edit("    cap: 3/day\n", "    cap: 3/day\n    cost: 10\n")).unwrap();
        assert_eq!(m.permitted("reset").unwrap().cost, 10);
        assert!(serde_yaml::to_string(&m).unwrap().contains("cost: 10"));

        let msg = fails(&parent.replace("children: [shop, farm]\n", ""));
        assert!(
            msg.starts_with("budget is a parent lever; list the ventures"),
            "{msg}"
        );
        let msg = fails(&parent.replace(
            "- {name: resume, cap: 9/day, reversible: true}",
            "- {name: resume, cap: 9/day, reversible: true, delegated: true}",
        ));
        assert!(msg.starts_with("resume is declared-only"), "{msg}");
        let msg = fails(&parent.replace("children: [shop, farm]", "children: [shop, self]"));
        assert_eq!(msg, "self cannot be a child; it belongs to no one");
        let msg = fails(&parent.replace("children: [shop, farm]", "children: [two words]"));
        assert_eq!(
            msg,
            "children names ventures, one word each, not \"two words\""
        );
        let msg = fails(&parent.replace("children: [shop, farm]", "children: shop"));
        assert_eq!(
            msg,
            "children is a list of venture names, not the text \"shop\""
        );
        let msg = fails(&format!("{VALID}parent: [a]\n"));
        assert_eq!(msg, "parent is the name of a venture, not a list");
        let msg = fails(&format!("{parent}parent: shop\n"));
        assert_eq!(
            msg,
            "shop cannot be both this venture's parent and its child"
        );
        let msg = fails(&edit("    cap: 3/day\n", "    cap: 3/day\n    cost: -1\n"));
        assert!(msg.contains("cost is a whole number"), "{msg}");

        assert_eq!(
            ParentCall::parse("budget", "shop 40").unwrap(),
            ParentCall::Budget {
                child: "shop".into(),
                amount: 40
            }
        );
        assert_eq!(
            ParentCall::parse("pause", " shop ").unwrap(),
            ParentCall::Pause {
                child: "shop".into()
            }
        );
        assert_eq!(
            ParentCall::parse("resume", "shop").unwrap().describe(),
            "shop may run again"
        );
        assert_eq!(
            ParentCall::parse("threshold", "farm above 25.5").unwrap(),
            ParentCall::Threshold {
                child: "farm".into(),
                above: true,
                bound: 25.5
            }
        );
        assert_eq!(
            ParentCall::parse("budget", "shop").unwrap_err(),
            "budget is written: budget <child> <amount>"
        );
        assert_eq!(
            ParentCall::parse("budget", "shop lots").unwrap_err(),
            "budget takes a whole number, not \"lots\""
        );
        assert_eq!(
            ParentCall::parse("threshold", "farm over 3").unwrap_err(),
            "threshold is written: threshold <child> above|below <number>"
        );
        assert_eq!(
            ParentCall::parse("pause", "").unwrap_err(),
            "pause names a child first, like: pause shop"
        );
    }

    #[test]
    fn delegating_a_lever_keeps_the_text_as_written() {
        let block = "# the shop\npurpose: Sell.\nmetric: orders\ncontroller: {kind: model}\ncadence: 1h\n\
                     permitted:\n  # talk\n  - name: announce\n    run: ./announce\n    cap: 3/day\n    reversible: true\n    takes: text\n\n  - name: deploy\n    run: ./deploy\n    cap: 1/day\n    reversible: false\n    delegated: false\n";
        let text = delegate_text(block, "announce").unwrap();
        assert_eq!(
            text,
            "# the shop\npurpose: Sell.\nmetric: orders\ncontroller: {kind: model}\ncadence: 1h\n\
             permitted:\n  # talk\n  - name: announce\n    run: ./announce\n    cap: 3/day\n    reversible: true\n    takes: text\n    delegated: true\n\n  - name: deploy\n    run: ./deploy\n    cap: 1/day\n    reversible: false\n    delegated: false\n"
        );
        assert!(
            Mandate::parse(&text)
                .unwrap()
                .permitted("announce")
                .unwrap()
                .delegated
        );
        let text = delegate_text(block, "deploy").unwrap();
        assert!(
            text.ends_with("    reversible: false\n    delegated: true\n"),
            "{text}"
        );
        assert!(text.starts_with("# the shop\n"));

        let flow = "purpose: Sell.\nmetric: orders\ncontroller: {kind: model}\ncadence: 1h\n\
                    permitted:\n- {name: announce, run: ./announce, cap: 3/day, reversible: true, takes: text}  # hi\n- {name: wipe, run: ./wipe, cap: 1/day, reversible: false, delegated: false}\n";
        let text = delegate_text(flow, "announce").unwrap();
        assert!(text.contains("- {name: announce, run: ./announce, cap: 3/day, reversible: true, takes: text, delegated: true}  # hi\n"), "{text}");
        let text = delegate_text(flow, "wipe").unwrap();
        assert!(
            text.contains("reversible: false, delegated: true}"),
            "{text}"
        );
        assert!(
            Mandate::parse(&text)
                .unwrap()
                .permitted("wipe")
                .unwrap()
                .delegated
        );

        assert!(delegate_text(block, "nothing")
            .unwrap_err()
            .contains("does not permit nothing"));
        assert!(
            delegate_text(&delegate_text(block, "announce").unwrap(), "announce")
                .unwrap_err()
                .contains("already delegated")
        );
        // The parent lever resume cannot be delegated: the mandate would be invalid.
        let parent = "purpose: Allocate.\nmetric: shell echo 1\ncontroller: {kind: model}\ncadence: 1h\nchildren: [shop]\npermitted:\n- {name: resume, cap: 9/day, reversible: true}\n";
        assert!(delegate_text(parent, "resume")
            .unwrap_err()
            .contains("declared-only"));
    }

    #[test]
    fn escalate_is_offbox_or_left_out_and_a_change_cannot_add_it() {
        let plain = Mandate::parse(VALID).unwrap();
        assert_eq!(plain.escalate, None);
        let off = Mandate::parse(&format!("{VALID}escalate: offbox\n")).unwrap();
        assert_eq!(off.escalate, Some(Escalate::Offbox));
        assert!(fails(&format!("{VALID}escalate: anywhere\n")).contains("escalate is offbox"));
        assert!(adds_offbox(&plain, &off));
        assert!(!adds_offbox(&off, &off), "keeping it is not adding it");
        assert!(!adds_offbox(&off, &plain), "dropping it is not adding it");
        assert!(!adds_offbox(&plain, &plain));
    }

    #[test]
    fn network_is_none_a_list_or_left_out() {
        let m = Mandate::parse(VALID).unwrap();
        assert_eq!(m.network, None);
        assert!(m.allows_host("anything.example"));
        let m = Mandate::parse(&format!("{VALID}network: none\n")).unwrap();
        assert_eq!(m.network, Some(vec![]));
        assert!(!m.allows_host("api.stripe.com"));
        let m = Mandate::parse(&format!("{VALID}network: [api.stripe.com, 127.0.0.1]\n")).unwrap();
        assert!(m.allows_host("API.stripe.com") && m.allows_host("127.0.0.1"));
        assert!(!m.allows_host("api.github.com"));
        assert!(serde_yaml::to_string(&m)
            .unwrap()
            .contains("network:\n- api.stripe.com"));
        let msg = fails(&format!("{VALID}network: all\n"));
        assert_eq!(
            msg,
            "network is none, or a list of hosts the venture may reach, not the text \"all\""
        );
        let msg = fails(&format!("{VALID}network: [1]\n"));
        assert_eq!(
            msg,
            "network lists hosts, like api.stripe.com, not the number 1"
        );
        let msg = fails(&format!("{VALID}network: [\"api stripe\"]\n"));
        assert_eq!(
            msg,
            "network lists hosts, like api.stripe.com, not \"api stripe\""
        );
    }

    #[test]
    fn yaml_that_cannot_be_read_names_the_line() {
        let msg = fails(&edit("forbidden: [rm]", "forbidden: [rm"));
        assert!(msg.starts_with("line 21: "), "{msg}");
        assert!(!msg.contains("column"), "{msg}");
        let msg = fails(&edit("  - name: reset\n", "\t- name: reset\n"));
        assert_eq!(
            msg,
            "line 11: found a character YAML cannot read here; if it is a tab, indent with spaces instead"
        );
        // libyaml has more than one message for a tab; the others already
        // read plainly and keep their line.
        let msg = fails(&edit("    run: ./deploy.sh\n", "\trun: ./deploy.sh\n"));
        assert!(msg.starts_with("line 16: found a tab character"), "{msg}");
        let msg = fails(&edit("metric: count\n", "metric: count\nmetric: errors\n"));
        assert_eq!(msg, "the field \"metric\" appears twice; keep one");
        let msg = fails(&edit(
            "    run: ./deploy.sh\n",
            "    run: ./deploy.sh\n    run: ./again.sh\n",
        ));
        assert!(msg.starts_with("line "), "{msg}");
        assert!(
            msg.ends_with("permitted[1]: the field \"run\" appears twice; keep one"),
            "{msg}"
        );
        let msg = fails(&format!("{VALID}---\npurpose: another\n"));
        assert_eq!(
            msg,
            "a mandate is one YAML document; remove the --- that starts another"
        );
    }

    #[test]
    fn serde_messages_are_said_plainly_with_the_line_when_known() {
        #[derive(Debug, Deserialize)]
        #[serde(untagged)]
        #[allow(dead_code)]
        enum Either {
            Number(u32),
            Flag(bool),
        }
        let msg = plain(&serde_yaml::from_str::<Either>("x").unwrap_err());
        assert_eq!(msg, "this value does not have the expected shape");
        let msg = plain(&serde_yaml::from_str::<Vec<String>>("~").unwrap_err());
        assert!(msg.ends_with("expected a list, found nothing"), "{msg}");
        let msg = plain(&serde_yaml::from_str::<Restart>("sometimes").unwrap_err());
        assert!(
            msg.ends_with("unknown choice sometimes, expected one of always, on-failure, never"),
            "{msg}"
        );
        let msg = plain(
            &serde_yaml::from_str::<Permitted>("name: x\nrun: y\ncap: 1/day\nreversible: yes\n")
                .unwrap_err(),
        );
        assert_eq!(
            msg,
            "line 4: reversible: expected true or false, found \"yes\""
        );
        let msg = plain(&serde_yaml::from_str::<Mandate>("- a\n").unwrap_err());
        assert_eq!(msg, "expected a set of fields, found a list");
        let msg = plain(&serde_yaml::from_str::<u8>("300").unwrap_err());
        assert_eq!(msg, "expected a whole number, found 300");
        let msg = plain(&serde_yaml::from_str::<Mandate>("purpose: x\n").unwrap_err());
        assert_eq!(msg, "missing field metric");
        for msg in [
            plain(&serde_yaml::from_str::<Controller>("kind: 5\n").unwrap_err()),
            plain(
                &serde_yaml::from_str::<Controller>("kind: threshold\nabove: ten\naction: stop\n")
                    .unwrap_err(),
            ),
            plain(&serde_yaml::from_str::<Controller>("kind: model\nabove: 1\n").unwrap_err()),
        ] {
            for jargon in ["variant", "serde", "untagged", "invalid type", "`"] {
                assert!(!msg.contains(jargon), "{jargon:?} in: {msg}");
            }
        }
    }

    #[test]
    fn marks_are_stripped_wherever_they_are() {
        assert_eq!(
            strip_marks("missing field `n` at line 2 column 4"),
            "missing field `n`"
        );
        assert_eq!(
            strip_marks("did not find expected ',' or ']' at line 2 column 7, while parsing a flow sequence at line 1 column 10"),
            "did not find expected ',' or ']', while parsing a flow sequence"
        );
        assert_eq!(strip_marks("look at line one"), "look at line one");
        assert_eq!(strip_marks("no marks"), "no marks");
    }

    #[test]
    fn validate_checks_a_mandate_built_in_code() {
        let mut m = Mandate::parse(VALID).unwrap();
        assert_eq!(m.validate(), Ok(()));
        m.permitted[0].cap.count = 0;
        assert!(m
            .validate()
            .unwrap_err()
            .0
            .contains("a cap of 0 permits nothing"));
        m.permitted[0].cap.count = 3;
        m.controller = Controller::Threshold {
            above: None,
            below: None,
            action: "reset".into(),
            deadband: None,
            hold: None,
        };
        assert!(m.validate().unwrap_err().0.contains("needs a bound"));
        m.controller = Controller::Classifier { backend: None };
        m.context_budget = 0;
        assert_eq!(m.validate().unwrap_err().0, format!("{BUDGET}, not 0"));
        m.context_budget = 1;
        m.purpose = " ".into();
        assert_eq!(m.validate().unwrap_err().0, NEEDS_PURPOSE);
    }

    #[test]
    fn load_and_hash_still_work() {
        let msg = Mandate::load(Path::new("/nonexistent/mandate.yaml"))
            .unwrap_err()
            .0;
        assert!(
            msg.starts_with("cannot read /nonexistent/mandate.yaml: "),
            "{msg}"
        );
        assert_eq!(
            Mandate::hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(Mandate::hash(VALID), Mandate::hash(VALID));
        assert_ne!(
            Mandate::hash(VALID),
            Mandate::hash(&edit("above: 10", "above: 11"))
        );
    }

    #[test]
    fn a_plan_controller_parses_with_its_defaults_and_says_what_is_wrong() {
        let threshold = "controller:\n  kind: threshold\n  above: 10\n  action: reset\n";
        let m = Mandate::parse(&edit(threshold, "controller: {kind: plan}\n")).unwrap();
        assert_eq!(
            m.controller,
            Controller::Plan {
                horizon: DEFAULT_HORIZON,
                depth: DEFAULT_DEPTH,
                members: DEFAULT_MEMBERS,
                explore: vec![],
                backend: None,
                better: Worse::Up,
            }
        );
        assert!(!m.early_warning);
        let m = Mandate::parse(&edit(
            threshold,
            "controller: {kind: plan, horizon: 6, depth: 1, members: 16, explore: [reset], backend: local, better: down}\nearly_warning: true\n",
        ))
        .unwrap();
        assert!(m.early_warning);
        assert!(
            matches!(&m.controller, Controller::Plan { horizon: 6, depth: 1, members: 16, explore, backend: Some(b), better: Worse::Down } if explore == &["reset"] && b == "local")
        );
        // Round trip.
        let text = serde_yaml::to_string(&m).unwrap();
        assert_eq!(Mandate::parse(&text).unwrap(), m);
        for (bad, says) in [
            ("{kind: plan, horizon: 0}", "plan horizon is a whole number from 1 to 48"),
            ("{kind: plan, depth: 9}", "plan depth is a whole number from 1 to 3"),
            ("{kind: plan, members: 1000}", "plan members is a whole number from 1 to 64"),
            ("{kind: plan, explore: reset}", "plan explore is a list"),
            ("{kind: plan, better: left}", "plan better is up or down"),
            ("{kind: plan, explore: [nope]}", "plan explore names \"nope\", which is not a permitted action"),
            ("{kind: plan, above: 3}", "a plan controller has these fields: kind, horizon, depth, members, explore, backend, better"),
        ] {
            let msg = fails(&edit(threshold, &format!("controller: {bad}\n")));
            assert!(msg.contains(says), "{bad}: {msg}");
        }
        let msg = fails(&format!("{VALID}early_warning: yes please\n"));
        assert!(msg.contains("early_warning is true or false"), "{msg}");
    }
}
