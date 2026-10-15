//! Mandates: what a venture is for, what number to watch, what it may do,
//! what it may not.

use std::fmt;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

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

/// An action the venture may take.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Permitted {
    pub name: String,
    /// The command that runs, in the venture directory.
    pub run: String,
    /// How often it may run, like `3/day`.
    pub cap: Cap,
    /// Can it be undone?
    pub reversible: bool,
    /// Irreversible actions become asks unless the mandate delegates them.
    #[serde(default)]
    pub delegated: bool,
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

impl Mandate {
    /// Parse and check mandate text.
    pub fn parse(text: &str) -> Result<Mandate, MandateError> {
        // Issue 2 adds validation and plain messages.
        serde_yaml::from_str(text).map_err(|e| MandateError(e.to_string()))
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
}
