//! The warden: the only process that holds keys. It checks the gate again,
//! runs actions, writes what happened, and sends prompts to backends with
//! the key attached.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::adapter::{self, Occasion};
use crate::config::{self, Backend, Config};
use crate::gate::{self, Candidate, DELEGATE, RESUME, STOP};
use crate::index::Index;
use crate::ledger::Ledger;
use crate::mandate::{FieldType, Mandate, ParentCall};
use crate::record::Verdict;
use crate::record::{self, Claim, Entry, Payload, Record, Seq};
use crate::research;
use crate::role::InferRequest;
use crate::venture;

/// How long an action or a report may run.
pub const ACTION_TIMEOUT: Duration = Duration::from_secs(600);
/// The most a backend's reply may hold, in bytes.
pub const MAX_REPLY: usize = 256 * 1024;
/// What the error of a reply refused for size begins with, so the kernel
/// can tell it from a backend that failed, whatever the backend printed.
pub const OVERSIZE: &str = "reply too big: ";

/// Is this the error of a reply refused for size?
pub fn refused_for_size(error: &str) -> bool {
    error.starts_with(OVERSIZE)
}

/// What the kernel asks of the warden.
pub trait Warden {
    /// Gate the action again; if it passes, write the intent, run it, and
    /// write the outcome (a stop is one action record). Returns the
    /// records written.
    fn execute(&mut self, candidate: &Candidate) -> Result<Vec<Entry>, Failure>;
    /// For an intent with no outcome: ask the lever's adapter, by the
    /// intent's key, whether the run happened. Some((happened, what it
    /// found)) when the adapter can tell; None when it cannot, and a human
    /// must. An error means the warden could not look now; try later.
    fn confirm(&mut self, _intent: Seq) -> Result<Option<(bool, String)>, String> {
        Ok(None)
    }
    /// Send a prompt to a backend with its key and return the reply text.
    fn infer(&mut self, request: &InferRequest) -> Result<String, String>;
    /// Measure a venture's metric through its adapter, with the key the
    /// adapter is scoped to. The value, or None with a note saying why.
    fn measure(&mut self, venture: &str) -> Result<(Option<f64>, String), String>;
    /// Read one of a venture's guards or audits by its metric,
    /// `guard.<name>` or `audit.<name>`: its command or its built-in
    /// metric, run by the warden and never by the venture. The value, or
    /// None with a note saying why.
    fn read(&mut self, _venture: &str, metric: &str) -> Result<(Option<f64>, String), String> {
        Err(format!("this warden cannot read {metric}"))
    }
}

/// Why `execute` wrote nothing. A refusal is the warden's own typed
/// answer; text from a lever, a path or a name is only ever an error.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    /// A payload check said no. The kernel writes a refusal record.
    Refused { reason_key: String, detail: String },
    /// It could not run, for this reason.
    Error(String),
}

impl From<&str> for Failure {
    fn from(e: &str) -> Failure {
        Failure::Error(e.to_string())
    }
}

impl From<String> for Failure {
    fn from(e: String) -> Failure {
        Failure::Error(e)
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Refused { reason_key, detail } => write!(f, "{reason_key}: {detail}"),
            Failure::Error(e) => f.write_str(e),
        }
    }
}

/// The warden in this process.
pub struct LocalWarden {
    home: PathBuf,
    ledger: Ledger,
    /// The ledger as read so far; each request reads only what is new.
    index: Index,
    config: Config,
    keys: BTreeMap<String, String>,
}

impl LocalWarden {
    /// Open the ledger and read config and keys.
    pub fn open(home: &Path) -> Result<LocalWarden, String> {
        let ledger = Ledger::open(&home.join("ledger.db")).map_err(|e| e.to_string())?;
        Ok(LocalWarden {
            home: home.to_path_buf(),
            ledger,
            index: Index::default(),
            config: Config::load(home)?,
            keys: config::load_keys(home)?,
        })
    }
}

impl Warden for LocalWarden {
    fn execute(&mut self, candidate: &Candidate) -> Result<Vec<Entry>, Failure> {
        self.index
            .refresh(&self.ledger)
            .map_err(|e| e.to_string())?;
        let index = &self.index;
        let ventures = venture::from_adoptions(index.adoptions());
        let adopted = venture::adopted(&ventures);
        let proof = gate::gate_in(index, candidate, &adopted, &self.config.quotas, now())
            .map_err(|r| r.to_string())?;
        let venture = candidate.venture.clone();
        let action = candidate.action.clone();
        let mut written = Vec::new();
        let mut append = |record: Record| -> Result<Entry, String> {
            self.ledger.append(record).map_err(|e| e.to_string())
        };
        let parent_lever = ventures
            .get(&venture)
            .and_then(|v| v.mandate.permitted(&action))
            .is_some_and(crate::mandate::Permitted::is_parent_lever);
        // The payload comes from the record the proof names, never from
        // the request: the gate chose the record, the warden only reads it.
        let payload = match proof.payload_from() {
            Some(seq) => index.get(seq).and_then(|e| e.record.payload().cloned()),
            None => None,
        };
        let key = record::key(&venture, &action, &candidate.cites, payload.as_ref());
        // Recorded, then done: every lever run stores its intent before it
        // runs and its outcome after. Only stop is a bare action.
        let intent = |proof: gate::Proof, payload: Option<Payload>| Record::Intent {
            claim: Claim::Measured,
            venture: venture.clone(),
            action: action.clone(),
            proof,
            payload,
            key: Some(key.clone()),
        };
        let outcome = |intent: &Entry, ok: bool, detail: String| Record::Outcome {
            claim: Claim::Measured,
            venture: venture.clone(),
            intent: intent.seq,
            ok,
            detail,
        };
        if action == DELEGATE {
            // The changed mandate is the payload the proof names; writing
            // it and adopting it is the action, and the adoption is
            // declared because the decision that allowed it was.
            let v = ventures
                .get(&venture)
                .ok_or_else(|| format!("no venture named {venture}"))?;
            let dir = v
                .path
                .clone()
                .ok_or_else(|| format!("{venture} has no mandate file to rewrite"))?;
            let text = payload
                .as_ref()
                .map(Payload::text)
                .ok_or("the proof names no mandate text")?;
            let mandate = Mandate::parse(&text).map_err(|e| format!("the new mandate: {e}"))?;
            // Escalating off the machine is declared by a human in the
            // mandate file and adopted with `add`; no proposal, ratchet or
            // self change gets it.
            if crate::mandate::adds_offbox(&v.mandate, &mandate) {
                return Err(
                    "the new mandate adds escalate: offbox, which only a human declares in the mandate file and adopts with: avsm add"
                        .to_string()
                        .into(),
                );
            }
            let by = proof
                .decision()
                .and_then(|seq| index.get(seq))
                .and_then(|e| match &e.record {
                    Record::Decision { by, .. } => by.clone(),
                    _ => None,
                });
            let path = dir.join(venture::MANDATE_FILE);
            let hash = Mandate::hash(&text);
            let delegated: Vec<&str> = mandate
                .permitted
                .iter()
                .filter(|p| p.delegated)
                .map(|p| p.name.as_str())
                .collect();
            // The mandate text is the payload the decision approved; the
            // intent cites it by the proof and does not carry it again.
            let started = append(intent(proof, None))?;
            let (ok, detail) = match std::fs::write(&path, &text) {
                Ok(()) => (
                    true,
                    format!(
                        "mandate rewritten; delegated: {}",
                        if delegated.is_empty() {
                            "nothing".to_string()
                        } else {
                            delegated.join(", ")
                        }
                    ),
                ),
                Err(e) => (false, format!("cannot write {}: {e}", path.display())),
            };
            let done = append(outcome(&started, ok, detail))?;
            written.push(started);
            written.push(done);
            if !ok {
                return Ok(written);
            }
            written.push(append(Record::Decision {
                claim: Claim::Declared,
                venture: venture.clone(),
                verdict: Verdict::Adopt {
                    mandate: hash,
                    path: dir.to_string_lossy().into_owned(),
                    text: Some(text),
                },
                by,
            })?);
            return Ok(written);
        }
        if action == STOP && !parent_lever {
            // Stopping runs nothing: the record is the stop.
            written.push(append(Record::Action {
                claim: Claim::Measured,
                venture: venture.clone(),
                action: action.clone(),
                proof,
                ok: true,
                detail: String::new(),
                payload: None,
            })?);
            return Ok(written);
        }
        if action == RESUME && !parent_lever {
            let started = append(intent(proof, None))?;
            written.push(append(outcome(&started, true, String::new()))?);
            written.insert(0, started);
            return Ok(written);
        }
        let v = ventures
            .get(&venture)
            .ok_or_else(|| format!("no venture named {venture}"))?;
        let perm = v
            .mandate
            .permitted(&action)
            .ok_or_else(|| format!("{venture} does not permit {action}"))?;
        let dir = v.path.clone().unwrap_or_else(|| self.home.clone());
        if perm.takes().is_some() && payload.is_none() {
            return Err(format!("{action} takes a payload and the proof names none").into());
        }
        if perm.is_parent_lever() {
            // Non-negotiable 10: a parent allocates. Pulling the lever is
            // the record itself; the child's gate reads the intent once its
            // outcome says it is done.
            let text = payload.as_ref().map(Payload::text).unwrap_or_default();
            let call = ParentCall::parse(&action, &text)?;
            let child = ventures
                .get(call.child())
                .ok_or_else(|| format!("{} is not a venture", call.child()))?;
            if v.mandate.child(call.child()).is_none()
                || child.mandate.parent.as_deref() != Some(venture.as_str())
            {
                return Err(format!(
                    "{} is not a child of {venture}: both mandates must say so",
                    call.child()
                )
                .into());
            }
            let started = append(intent(proof, payload))?;
            let done = append(outcome(&started, true, call.describe()))?;
            written.push(started);
            written.push(done);
            return Ok(written);
        }
        if perm.is_research() {
            let question = payload.as_ref().map(Payload::text).unwrap_or_default();
            let started = append(intent(proof, payload))?;
            let (ok, detail, findings) =
                match research(&self.config, &self.home, &question, &venture) {
                    Ok(f) => (
                        true,
                        format!(
                            "{} {}",
                            f.len(),
                            if f.len() == 1 { "finding" } else { "findings" }
                        ),
                        f,
                    ),
                    Err(e) => (false, e, vec![]),
                };
            let done = append(outcome(&started, ok, detail))?;
            let seq = started.seq;
            written.push(started);
            written.push(done);
            for record in research::records(&venture, &findings, seq) {
                written.push(append(record)?);
            }
            return Ok(written);
        }
        if let Some(lever) = perm.lever_adapter() {
            if let Some(host) = lever.host(&self.config.adapters) {
                if !v.mandate.allows_host(&host) {
                    return Err(format!(
                        "{action} would reach {host}, which is not in {venture}'s network list"
                    )
                    .into());
                }
            }
        }
        // The payload's fields come first: a payload of the wrong shape
        // reaches neither the check nor the lever.
        if let (Some(fields), Some(p)) = (&perm.fields, payload.as_ref()) {
            payload_schema(fields, p)?;
        }
        // A payload check runs after the gate, so after any approval, and
        // before the intent: a failure runs nothing and writes no intent.
        if let (Some(check), Some(p)) = (&perm.check, payload.as_ref()) {
            payload_check(&self.home, &dir, check, p, index, &venture)?;
        }
        let proof_text = proof.to_string();
        let pull = |payload: Option<&Payload>| -> (bool, String) {
            match (perm.lever_adapter(), &perm.run, payload) {
                (Some(lever), _, Some(p)) => lever.run(
                    &dir,
                    lever
                        .key_name()
                        .and_then(|k| self.keys.get(k))
                        .map(String::as_str),
                    &self.config.adapters,
                    p,
                    &Occasion {
                        venture: &venture,
                        action: &action,
                        proof: &proof_text,
                    },
                ),
                (Some(lever), _, None) => (false, format!("{} needs a payload", lever.name())),
                (None, Some(run), p) => run_lever(&dir, run, p, Some(&key), ACTION_TIMEOUT),
                (None, None, _) => (false, format!("{action} has no command and no adapter")),
            }
        };
        // Reversible or not, the intent is stored before the lever runs.
        let started = append(intent(proof, payload.clone()))?;
        let (ok, detail) = pull(payload.as_ref());
        let done = append(outcome(&started, ok, detail))?;
        written.push(started);
        written.push(done);
        Ok(written)
    }

    fn infer(&mut self, request: &InferRequest) -> Result<String, String> {
        let (name, backend) = self.config.backend(Some(&request.backend))?;
        match backend {
            Backend::Anthropic {
                url,
                model,
                key,
                effort,
            } => {
                let key = self.keys.get(key).ok_or_else(|| {
                    format!("backend {name} needs a key named {key} in the keys file")
                })?;
                let mut body = serde_json::json!({
                    "model": model,
                    "max_tokens": request.max_tokens,
                    "system": request.system,
                    "messages": [{"role": "user", "content": request.user}],
                });
                if let Some(e) = effort {
                    body["output_config"] = serde_json::json!({ "effort": e });
                }
                let mut headers = vec![
                    ("x-api-key".to_string(), key.clone()),
                    ("anthropic-version".to_string(), "2023-06-01".to_string()),
                ];
                if fallbacks_apply(model) {
                    body["fallbacks"] = serde_json::json!("default");
                    headers.push((
                        "anthropic-beta".to_string(),
                        "server-side-fallback-2026-07-01".to_string(),
                    ));
                }
                let reply = adapter::post_json(url, &headers, &body)?;
                if reply["stop_reason"].as_str() == Some("refusal") {
                    return Err(format!("backend {name} declined to answer"));
                }
                let text: Vec<&str> = reply["content"]
                    .as_array()
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|b| b["type"].as_str() == Some("text"))
                            .filter_map(|b| b["text"].as_str())
                            .collect()
                    })
                    .unwrap_or_default();
                if text.is_empty() {
                    return Err(format!("backend {name} answered without text: {reply}"));
                }
                Ok(text.join("\n"))
            }
            Backend::Openai { url, model, key } => {
                let body = serde_json::json!({
                    "model": model,
                    "messages": [
                        {"role": "system", "content": request.system},
                        {"role": "user", "content": request.user}
                    ],
                    "max_tokens": request.max_tokens,
                });
                let mut headers = Vec::new();
                if let Some(k) = key {
                    let value = self.keys.get(k).ok_or_else(|| {
                        format!("backend {name} needs a key named {k} in the keys file")
                    })?;
                    headers.push(("authorization".to_string(), format!("Bearer {value}")));
                }
                let reply = adapter::post_json(url, &headers, &body)?;
                reply["choices"][0]["message"]["content"]
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("backend {name} answered without text: {reply}"))
            }
            Backend::Command { run, .. } => {
                let stdin = serde_json::to_string(&serde_json::json!({
                    "backend": name,
                    "system": request.system,
                    "user": request.user,
                }))
                .map_err(|e| e.to_string())?;
                // The reply is the whole of standard output, newlines and
                // all, up to a bound; only a failure is cut to one line.
                let ran = self::run(&self.home, "sh", &["-c", run], &stdin, ACTION_TIMEOUT);
                if !ran.ok {
                    return Err(format!(
                        "backend {name} failed: {}",
                        ran.detail(ACTION_TIMEOUT)
                    ));
                }
                if ran.stdout.len() > MAX_REPLY {
                    return Err(format!(
                        "{OVERSIZE}backend {name} replied with {} bytes; the most a reply may hold is {MAX_REPLY}",
                        ran.stdout.len()
                    ));
                }
                Ok(ran.stdout)
            }
        }
    }

    fn measure(&mut self, venture: &str) -> Result<(Option<f64>, String), String> {
        self.index
            .refresh(&self.ledger)
            .map_err(|e| e.to_string())?;
        let ventures = venture::from_adoptions(self.index.adoptions());
        self.measure_in(&ventures, venture)
    }

    fn read(&mut self, venture: &str, metric: &str) -> Result<(Option<f64>, String), String> {
        self.index
            .refresh(&self.ledger)
            .map_err(|e| e.to_string())?;
        let ventures = venture::from_adoptions(self.index.adoptions());
        self.read_in(&ventures, venture, metric)
    }

    fn confirm(&mut self, intent: Seq) -> Result<Option<(bool, String)>, String> {
        self.index
            .refresh(&self.ledger)
            .map_err(|e| e.to_string())?;
        let Some(Record::Intent {
            venture,
            action,
            key: Some(key),
            ..
        }) = self.index.get(intent).map(|e| &e.record)
        else {
            return Ok(None);
        };
        let ventures = venture::from_adoptions(self.index.adoptions());
        let Some(lever) = ventures
            .get(venture)
            .and_then(|v| v.mandate.permitted(action))
            .and_then(|p| p.lever_adapter())
        else {
            return Ok(None);
        };
        let secret = lever
            .key_name()
            .and_then(|k| self.keys.get(k))
            .map(String::as_str);
        Ok(lever.confirm(secret, &self.config.adapters, key))
    }
}

impl LocalWarden {
    /// Read a guard's or an audit's command or built-in metric.
    fn read_in(
        &self,
        ventures: &BTreeMap<String, venture::Venture>,
        venture: &str,
        metric: &str,
    ) -> Result<(Option<f64>, String), String> {
        let v = ventures
            .get(venture)
            .ok_or_else(|| format!("no venture named {venture}"))?;
        let m = &v.mandate;
        let (run, spec) = if let Some(name) = metric.strip_prefix("guard.") {
            let g = m
                .guard(name)
                .ok_or_else(|| format!("{venture} has no guard {name}"))?;
            (g.run.clone(), g.adapter.clone())
        } else if let Some(name) = metric.strip_prefix(crate::mandate::AUDIT_PREFIX) {
            let a = m
                .audit(name)
                .ok_or_else(|| format!("{venture} has no audit {name}"))?;
            (a.run.clone(), a.adapter.clone())
        } else {
            return Err(format!("{metric} is neither a guard nor an audit"));
        };
        let dir = v.path.clone().unwrap_or_else(|| self.home.clone());
        if let Some(cmd) = run {
            let ran = run_command_raw(&dir, &cmd, ACTION_TIMEOUT);
            if !ran.ok {
                return Ok((
                    None,
                    format!("{metric} failed: {}", ran.detail(ACTION_TIMEOUT)),
                ));
            }
            return Ok(match adapter::number_from(&ran.stdout) {
                Ok(n) => (Some(n), String::new()),
                Err(_) => (
                    None,
                    format!(
                        "{metric} did not print a number: {}",
                        ran.detail(ACTION_TIMEOUT)
                    ),
                ),
            });
        }
        let spec = spec.ok_or_else(|| format!("{metric} reads nothing"))?;
        let reading = adapter::Metric::parse(&spec)?
            .ok_or_else(|| format!("{metric}: {spec:?} is not a built-in metric"))?;
        if let Some(host) = reading.host(&self.config.adapters) {
            if !m.allows_host(&host) {
                return Ok((
                    None,
                    format!(
                        "{} would reach {host}, which is not in the venture's network list",
                        reading.name()
                    ),
                ));
            }
        }
        let key = reading
            .key_name()
            .and_then(|k| self.keys.get(k))
            .map(String::as_str);
        Ok(reading.measure(&dir, key, &self.config.adapters))
    }

    fn measure_in(
        &self,
        ventures: &BTreeMap<String, venture::Venture>,
        venture: &str,
    ) -> Result<(Option<f64>, String), String> {
        let v = ventures
            .get(venture)
            .ok_or_else(|| format!("no venture named {venture}"))?;
        let metric = v
            .mandate
            .metric_adapter()
            .ok_or_else(|| format!("{venture} watches {}, not an adapter", v.mandate.metric))?;
        if let Some(host) = metric.host(&self.config.adapters) {
            if !v.mandate.allows_host(&host) {
                return Ok((
                    None,
                    format!(
                        "{} would reach {host}, which is not in the venture's network list",
                        metric.name()
                    ),
                ));
            }
        }
        let dir = v.path.clone().unwrap_or_else(|| self.home.clone());
        let key = metric
            .key_name()
            .and_then(|k| self.keys.get(k))
            .map(String::as_str);
        Ok(metric.measure(&dir, key, &self.config.adapters))
    }
}

/// Ask the research command a question and read its findings. The command
/// is the provider; the warden only extracts what fits the record schema.
fn research(
    config: &Config,
    home: &Path,
    question: &str,
    venture: &str,
) -> Result<Vec<research::Finding>, String> {
    let name = config
        .research
        .as_deref()
        .ok_or("no research backend; name a command backend under research: in config.yaml")?;
    let (_, backend) = config.backend(Some(name))?;
    let Backend::Command { run: command, .. } = backend else {
        return Err(format!("research backend {name} must be kind: command"));
    };
    if question.trim().is_empty() {
        return Err("research needs a question".to_string());
    }
    let stdin = research::request(question, venture);
    let ran = run(home, "sh", &["-c", command], &stdin, ACTION_TIMEOUT);
    if !ran.ok {
        return Err(format!(
            "research command failed: {}",
            ran.detail(ACTION_TIMEOUT)
        ));
    }
    research::parse(&ran.stdout)
}

/// Models whose API takes `fallbacks: "default"`.
fn fallbacks_apply(model: &str) -> bool {
    [
        "claude-opus-5",
        "claude-sonnet-5-5",
        "claude-fable",
        "claude-mythos",
    ]
    .iter()
    .any(|p| model.starts_with(p))
}

/// What a refusal by a payload's fields is called in its reason key.
pub const PAYLOAD_SCHEMA: &str = "PayloadSchema";

/// Check a json payload against the fields its lever names: an object
/// with exactly those fields, each of its type.
fn payload_schema(fields: &BTreeMap<String, FieldType>, payload: &Payload) -> Result<(), Failure> {
    let refused = |detail: String| Failure::Refused {
        reason_key: PAYLOAD_SCHEMA.to_string(),
        detail,
    };
    let Payload::Json { json } = payload else {
        return Err(refused(format!(
            "the lever takes a json object with fields, not {}",
            payload.takes()
        )));
    };
    let Some(object) = json.as_object() else {
        return Err(refused("the payload must be a json object".to_string()));
    };
    let mut problems = Vec::new();
    for (name, ty) in fields {
        match object.get(name) {
            None => problems.push(format!("{name} is missing ({ty})", ty = ty.as_str())),
            Some(v) if !ty.holds(v) => problems.push(format!("{name} must be {}", ty.as_str())),
            Some(_) => {}
        }
    }
    for name in object.keys().filter(|k| !fields.contains_key(*k)) {
        problems.push(format!("{name} is not a field of this lever"));
    }
    if problems.is_empty() {
        return Ok(());
    }
    let wanted: Vec<String> = fields
        .iter()
        .map(|(n, t)| format!("{n}: {}", t.as_str()))
        .collect();
    Err(refused(format!(
        "{}; the fields are {{{}}}",
        problems.join(", "),
        wanted.join(", ")
    )))
}

/// What a refusal by a payload check is called in its reason key.
pub const PAYLOAD_CHECK: &str = "PayloadCheck";
/// The most reports a check sees.
pub const CHECK_REPORTS: usize = 500;
/// The most of a check's stdout that is read, and the most of it that
/// goes into a refusal.
pub const CHECK_READ: usize = 4096;
const CHECK_DETAIL: usize = 200;

/// Run a lever's check. `Ok` when it exits 0. Otherwise a typed refusal,
/// with the check's bounded stdout in its detail.
fn payload_check(
    home: &Path,
    dir: &Path,
    check: &str,
    payload: &Payload,
    index: &Index,
    venture: &str,
) -> Result<(), Failure> {
    let refused = |detail: String| Failure::Refused {
        reason_key: PAYLOAD_CHECK.to_string(),
        detail,
    };
    // Measured and declared reports only: an inferred record never
    // authorizes, so a check never sees one.
    let mut seen: Vec<&Entry> = index
        .of(venture)
        .filter(|e| e.record.kind() == record::Kind::Report && e.record.claim() != Claim::Inferred)
        .collect();
    let skip = seen.len().saturating_sub(CHECK_REPORTS);
    seen.drain(..skip);
    let json = serde_json::to_string(&seen).map_err(|e| refused(e.to_string()))?;
    let file = home.join(format!("measured-{}-{}.json", std::process::id(), now()));
    std::fs::write(&file, json).map_err(|e| refused(format!("cannot write {e}")))?;
    let path = file.to_string_lossy().into_owned();
    let ran = run_bounded(
        dir,
        "sh",
        &["-c", check],
        &[
            ("AVSM_MEASURED", path.as_str()),
            ("AVSM_PAYLOAD", payload.takes().to_string().as_str()),
        ],
        &payload.text(),
        ACTION_TIMEOUT,
        CHECK_READ,
    );
    let _ = std::fs::remove_file(&file);
    if ran.ok {
        return Ok(());
    }
    let out: String = ran
        .stdout
        .trim()
        .replace('\n', "  ")
        .chars()
        .take(CHECK_DETAIL)
        .collect();
    let how = if ran.killed {
        "timed out".to_string()
    } else {
        format!("exit {}", ran.code.unwrap_or(-1))
    };
    Err(refused(
        format!("{check} failed ({how})  {out}").trim().to_string(),
    ))
}

/// What running something came to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ran {
    /// Exited 0.
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
    /// Exit code, if it exited on its own.
    pub code: Option<i32>,
    /// Killed at the timeout.
    pub killed: bool,
}

impl Ran {
    /// One line of what happened, for a record: the tail of the output,
    /// with the exit code or the timeout when it failed.
    pub fn detail(&self, timeout: Duration) -> String {
        let mut text = self.stdout.trim().to_string();
        let err = self.stderr.trim();
        if !err.is_empty() {
            if !text.is_empty() {
                text.push_str("  ");
            }
            text.push_str(err);
        }
        let text = text.replace('\n', "  ");
        if self.killed {
            tail(format!("killed after {}s  {text}", timeout.as_secs()).trim())
        } else if self.ok {
            tail(&text)
        } else {
            tail(format!("exit {}  {text}", self.code.unwrap_or(-1)).trim())
        }
    }
}

/// Run a program in a directory with a clean environment, feeding it
/// `stdin`, and wait up to `timeout`.
pub fn run(dir: &Path, program: &str, args: &[&str], stdin: &str, timeout: Duration) -> Ran {
    run_with(dir, program, args, &[], stdin, timeout)
}

/// Like `run`, with a few extra variables in the environment.
pub fn run_with(
    dir: &Path,
    program: &str,
    args: &[&str],
    env: &[(&str, &str)],
    stdin: &str,
    timeout: Duration,
) -> Ran {
    run_bounded(dir, program, args, env, stdin, timeout, usize::MAX)
}

/// Like `run_with`, keeping at most `cap` bytes of each of stdout and
/// stderr; the rest is read and dropped so the child never blocks.
pub fn run_bounded(
    dir: &Path,
    program: &str,
    args: &[&str],
    env: &[(&str, &str)],
    stdin: &str,
    timeout: Duration,
    cap: usize,
) -> Ran {
    let mut child = match Command::new(program)
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string()),
        )
        .env("HOME", dir)
        .envs(env.iter().copied())
        .stdin(if stdin.is_empty() {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return Ran {
                stderr: format!("cannot run {program}: {e}"),
                ..Ran::default()
            }
        }
    };
    if !stdin.is_empty() {
        if let Some(mut input) = child.stdin.take() {
            use std::io::Write;
            let _ = input.write_all(stdin.as_bytes());
        }
    }
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        if let Some(s) = stdout.as_mut() {
            read_capped(s, cap, &mut out);
        }
        let mut err = String::new();
        if let Some(s) = stderr.as_mut() {
            read_capped(s, cap, &mut err);
        }
        (out, err)
    });
    let began = Instant::now();
    // PERF: most children exit within milliseconds; poll fast, then back off.
    let mut wait = Duration::from_millis(1);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if began.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => {
                std::thread::sleep(wait);
                wait = (wait * 2).min(Duration::from_millis(20));
            }
            Err(_) => break None,
        }
    };
    let (stdout, stderr) = reader.join().unwrap_or_default();
    Ran {
        ok: status.is_some_and(|s| s.success()),
        stdout,
        stderr,
        code: status.and_then(|s| s.code()),
        killed: status.is_none(),
    }
}

/// Read all of `from`, keeping the first `cap` bytes as text.
fn read_capped(from: &mut impl Read, cap: usize, into: &mut String) {
    let mut kept = Vec::new();
    let _ = from.by_ref().take(cap as u64).read_to_end(&mut kept);
    let _ = std::io::copy(from, &mut std::io::sink());
    *into = String::from_utf8_lossy(&kept).into_owned();
}

/// Run a shell command in a directory; see `run`. Returns whether it
/// exited 0 and one line of what happened.
/// Run a command line with `sh -c` and keep what it printed.
fn run_command_raw(dir: &Path, command: &str, timeout: Duration) -> Ran {
    run(dir, "sh", &["-c", command], "", timeout)
}

pub fn run_command(dir: &Path, command: &str, stdin: &str, timeout: Duration) -> (bool, String) {
    let ran = run(dir, "sh", &["-c", command], stdin, timeout);
    (ran.ok, ran.detail(timeout))
}

/// Run a lever's command with its payload, if it takes one, on standard
/// input: the text, the JSON, the diff, or the path of the file. The
/// shape is in `AVSM_PAYLOAD` so a script can tell which it got, and the
/// run's idempotency key in `AVSM_KEY` so a script can tell a run it has
/// already done.
pub fn run_lever(
    dir: &Path,
    command: &str,
    payload: Option<&Payload>,
    key: Option<&str>,
    timeout: Duration,
) -> (bool, String) {
    let (mut env, stdin): (Vec<(&str, String)>, String) = match payload {
        Some(p) => (vec![("AVSM_PAYLOAD", p.takes().to_string())], p.text()),
        None => (vec![], String::new()),
    };
    if let Some(k) = key {
        env.push(("AVSM_KEY", k.to_string()));
    }
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let ran = run_with(dir, "sh", &["-c", command], &env, &stdin, timeout);
    (ran.ok, ran.detail(timeout))
}

fn tail(s: &str) -> String {
    let s = s.replace('\n', "  ");
    if s.chars().count() <= 400 {
        s
    } else {
        let skip = s.chars().count() - 400;
        s.chars().skip(skip).collect()
    }
}

/// Unix seconds now.
pub fn now() -> i64 {
    crate::clock::now()
}

/// What the kernel sends to a warden process.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToWarden {
    Execute(Candidate),
    Infer(InferRequest),
    Measure(String),
    Confirm(Seq),
    /// A venture and a guard's or an audit's metric.
    Read(String, String),
}

/// What a warden process answers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FromWarden {
    Done(Vec<Entry>),
    /// A payload check refused the run.
    Refused {
        reason_key: String,
        detail: String,
    },
    Text(String),
    Measured(Option<f64>, String),
    Confirmed(Option<(bool, String)>),
    Err(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bounded_run_keeps_the_cap_and_does_not_block_the_child() {
        let tmp = tempfile::tempdir().unwrap();
        let ran = run_bounded(
            tmp.path(),
            "sh",
            &["-c", "yes | head -c 3000000"],
            &[],
            "",
            Duration::from_secs(20),
            100,
        );
        assert!(ran.ok && !ran.killed);
        assert_eq!(ran.stdout.len(), 100);
    }

    #[test]
    fn commands_run_in_the_directory_with_a_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let (ok, out) = run_command(tmp.path(), "echo hi; pwd", "", Duration::from_secs(5));
        assert!(ok);
        assert!(out.starts_with("hi  "), "{out}");
        let (ok, out) = run_command(
            tmp.path(),
            "echo oops >&2; exit 3",
            "",
            Duration::from_secs(5),
        );
        assert!(!ok);
        assert_eq!(out, "exit 3  oops");
        let (ok, out) = run_command(tmp.path(), "cat", "from stdin", Duration::from_secs(5));
        assert!(ok);
        assert_eq!(out, "from stdin");
        let (ok, out) = run_command(tmp.path(), "sleep 5", "", Duration::from_millis(200));
        assert!(!ok);
        assert!(out.starts_with("killed after 0s"), "{out}");
        let (ok, out) = run_command(tmp.path(), "env", "", Duration::from_secs(5));
        assert!(ok);
        assert!(!out.contains("AVSM"), "clean environment: {out}");
    }

    #[test]
    fn a_lever_reads_its_payload_on_stdin() {
        let tmp = tempfile::tempdir().unwrap();
        let payload = Payload::Json {
            json: serde_json::json!({"n": 1}),
        };
        let (ok, out) = run_lever(
            tmp.path(),
            "echo \"$AVSM_PAYLOAD: $(cat)\"",
            Some(&payload),
            None,
            Duration::from_secs(5),
        );
        assert!(ok);
        assert_eq!(out, "json: {\"n\":1}");
        let (ok, out) = run_lever(
            tmp.path(),
            "echo \"[$AVSM_PAYLOAD][$AVSM_KEY]\"",
            None,
            Some("k1"),
            Duration::from_secs(5),
        );
        assert!(ok);
        assert_eq!(out, "[][k1]");
    }

    #[test]
    fn fallbacks_only_for_models_that_take_them() {
        assert!(fallbacks_apply("claude-opus-5-5"));
        assert!(fallbacks_apply("claude-fable-5-1"));
        assert!(!fallbacks_apply("claude-haiku-5-5"));
        assert!(!fallbacks_apply("claude-sonnet-4-6"));
    }
}
