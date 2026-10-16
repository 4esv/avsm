//! Mandates: what a venture is for, what number to watch, what it may do,
//! what it may not.

use std::fmt;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_yaml::{Mapping, Value};
use sha2::{Digest, Sha256};

pub use crate::record::Takes;

use crate::adapter;

/// A parsed mandate.yaml.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mandate {
    /// What the venture is for, in the human's words.
    pub purpose: String,
    /// The name of the number the report prints.
    pub metric: String,
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
    /// What to do when `run` dies.
    #[serde(default)]
    pub restart: Restart,
}

fn default_context_budget() -> u32 {
    8000
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
    "restart",
];
const THRESHOLD_FIELDS: &[&str] = &["kind", "above", "below", "action"];
const BACKEND_FIELDS: &[&str] = &["kind", "backend"];
const ACTION_FIELDS: &[&str] = &[
    "name",
    "run",
    "adapter",
    "cap",
    "reversible",
    "delegated",
    "takes",
];

/// Names no permitted action may take: they are built in.
pub const BUILT_IN: &[&str] = &[STOP, "resume", "delegate"];

/// The built-in lever that asks the research role a question. A mandate
/// permits it by name alone, with no run and no adapter.
pub const RESEARCH: &str = "research";

const KINDS: &str = "threshold, classifier or model";
const NEEDS_PURPOSE: &str = "mandate needs a purpose: what the venture is for, in your own words";
const NEEDS_METRIC: &str =
    "mandate needs a metric: the name of the number the report prints, like count";
const NEEDS_CONTROLLER: &str =
    "mandate needs a controller: what decides when to act, with a kind of threshold, classifier or model";
const NEEDS_CADENCE: &str =
    "mandate needs a cadence: how often the report runs, like 30s, 5m or 1h";
const NEEDS_ACTION: &str =
    "a threshold controller needs an action: the name of a permitted action, or stop";
const BUDGET: &str = "context_budget must be a whole number of tokens greater than 0, like 4000";

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
        let mandate: Mandate = serde_yaml::from_str(text).map_err(|e| MandateError(plain(&e)))?;
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
        match adapter::Metric::parse(&self.metric) {
            Ok(Some(_)) => {}
            Ok(None) if is_word(self.metric.trim()) => {}
            Ok(None) => {
                return fail(format!(
                    "metric must be one word like count or latency_ms, not {:?}",
                    self.metric
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
            match (&p.run, &p.adapter) {
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
                        "permitted action {:?} needs run (a command) or adapter (a built-in: {}); only research needs neither",
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
            if action != STOP && !names.contains(&action.as_str()) {
                let choices = if names.is_empty() {
                    "this mandate permits no actions, so the only choice is the built-in stop"
                        .to_string()
                } else {
                    format!(
                        "name one of the permitted actions ({}) or the built-in stop",
                        names.join(", ")
                    )
                };
                return fail(format!(
                    "threshold action {action:?} is not permitted; {choices}"
                ));
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
        Ok(())
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
            metric: String::new(),
            controller: Controller::Model { backend: None },
            permitted: vec![],
            forbidden: vec![],
            cadence: Duration::from_secs(60),
            context_budget: default_context_budget(),
            restart: Restart::default(),
        }
    }

    /// The permitted action with this name, if any.
    pub fn permitted(&self, name: &str) -> Option<&Permitted> {
        self.permitted.iter().find(|p| p.name == name)
    }

    /// The metric's name: the word before any adapter argument, as reports
    /// carry it.
    pub fn metric_name(&self) -> &str {
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
            .or_else(|| self.is_research().then_some(Takes::Text))
    }

    /// Is this the built-in research lever?
    pub fn is_research(&self) -> bool {
        self.name == RESEARCH && self.run.is_none() && self.adapter.is_none()
    }

    /// The built-in action, when the lever names one.
    pub fn lever_adapter(&self) -> Option<adapter::Lever> {
        self.adapter
            .as_deref()
            .and_then(|a| adapter::Lever::parse(a).ok())
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
    check_restart(map.get("restart"))
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
        Some(other) => fail(format!(
            "metric must be one word like count or latency_ms, not {}",
            describe(other)
        )),
    }
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
            if matches!(kind.as_str(), "threshold" | "classifier" | "model") =>
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
    let fields = if kind == "threshold" {
        THRESHOLD_FIELDS
    } else {
        BACKEND_FIELDS
    };
    if let Some(field) = unknown_field(map, fields)? {
        return fail(format!(
            "unknown field {field:?} in controller; a {kind} controller has these fields: {}",
            fields.join(", ")
        ));
    }
    if kind != "threshold" {
        return match map.get("backend") {
            None | Some(Value::Null) | Some(Value::String(_)) => Ok(()),
            Some(other) => fail(format!(
                "controller backend must be the name of a backend, as text, not {}",
                describe(other)
            )),
        };
    }
    for which in ["above", "below"] {
        match map.get(which) {
            None | Some(Value::Null) | Some(Value::Number(_)) => {}
            Some(other) => {
                return fail(format!(
                    "threshold {which} must be a number like 10, not {}",
                    describe(other)
                ))
            }
        }
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
            if map.get("name").and_then(Value::as_str) == Some(RESEARCH) => {}
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
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(t)) => t
            .parse::<Takes>()
            .map(|_| ())
            .map_err(|_| MandateError(format!("{who} takes is {}, not {t:?}", Takes::ALL))),
        Some(other) => fail(format!(
            "{who} takes is {}, not {}",
            Takes::ALL,
            describe(other)
        )),
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
    fn unknown_field_is_named_with_the_known_ones() {
        let msg = fails(&edit("metric: count", "metrik: count"));
        assert_eq!(
            msg,
            "unknown field \"metrik\"; a mandate has these fields: purpose, metric, controller, permitted, forbidden, cadence, context_budget, restart"
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
            "controller must be a set of fields starting with kind: threshold, classifier or model, not the text \"threshold\""
        );
        let msg = fails(&edit("  kind: threshold\n", ""));
        assert_eq!(
            msg,
            "controller needs a kind: threshold, classifier or model"
        );
        let msg = fails(&edit("kind: threshold", "kind: magic"));
        assert_eq!(
            msg,
            "controller kind must be threshold, classifier or model, not \"magic\""
        );
        let msg = fails(&edit("kind: threshold", "kind: 3"));
        assert!(msg.ends_with("not the number 3"), "{msg}");
    }

    #[test]
    fn controller_unknown_field_lists_the_fields_of_its_kind() {
        let msg = fails(&edit("  above: 10\n", "  above: 10\n  over: 10\n"));
        assert_eq!(
            msg,
            "unknown field \"over\" in controller; a threshold controller has these fields: kind, above, below, action"
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
                action: "reset".into()
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
                action: STOP.into()
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
            "unknown field \"undo\" in permitted action \"deploy\"; a permitted action has these fields: name, run, adapter, cap, reversible, delegated, takes"
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
}
