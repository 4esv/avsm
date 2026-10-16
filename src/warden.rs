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
use crate::ledger::Ledger;
use crate::mandate::{Mandate, ParentCall};
use crate::record::Verdict;
use crate::record::{Claim, Entry, Payload, Record};
use crate::research;
use crate::role::InferRequest;
use crate::venture;

/// How long an action or a report may run.
pub const ACTION_TIMEOUT: Duration = Duration::from_secs(600);
/// The most a backend's reply may hold, in bytes.
pub const MAX_REPLY: usize = 256 * 1024;

/// What the kernel asks of the warden.
pub trait Warden {
    /// Gate the action again; if it passes, run it and write the action
    /// record (or intent then outcome). Returns the records written.
    fn execute(&mut self, candidate: &Candidate) -> Result<Vec<Entry>, String>;
    /// Send a prompt to a backend with its key and return the reply text.
    fn infer(&mut self, request: &InferRequest) -> Result<String, String>;
    /// Measure a venture's metric through its adapter, with the key the
    /// adapter is scoped to. The value, or None with a note saying why.
    fn measure(&mut self, venture: &str) -> Result<(Option<f64>, String), String>;
}

/// The warden in this process.
pub struct LocalWarden {
    home: PathBuf,
    ledger: Ledger,
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
            config: Config::load(home)?,
            keys: config::load_keys(home)?,
        })
    }
}

impl Warden for LocalWarden {
    fn execute(&mut self, candidate: &Candidate) -> Result<Vec<Entry>, String> {
        let entries = self.ledger.all().map_err(|e| e.to_string())?;
        let ventures = venture::load_all(&entries);
        let adopted = venture::adopted(&ventures);
        let proof = gate::gate_with(candidate, &entries, &adopted, &self.config.quotas, now())
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
            let text = proof
                .payload_from()
                .and_then(|seq| entries.iter().find(|e| e.seq == seq))
                .and_then(|e| e.record.payload().map(Payload::text))
                .ok_or("the proof names no mandate text")?;
            let mandate = Mandate::parse(&text).map_err(|e| format!("the new mandate: {e}"))?;
            let by = proof
                .decision()
                .and_then(|seq| entries.iter().find(|e| e.seq == seq))
                .and_then(|e| match &e.record {
                    Record::Decision { by, .. } => by.clone(),
                    _ => None,
                });
            let path = dir.join(venture::MANDATE_FILE);
            std::fs::write(&path, &text)
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            let hash = Mandate::hash(&text);
            let delegated: Vec<&str> = mandate
                .permitted
                .iter()
                .filter(|p| p.delegated)
                .map(|p| p.name.as_str())
                .collect();
            written.push(append(Record::Action {
                claim: Claim::Measured,
                venture: venture.clone(),
                action,
                proof,
                ok: true,
                detail: format!(
                    "mandate rewritten; delegated: {}",
                    if delegated.is_empty() {
                        "nothing".to_string()
                    } else {
                        delegated.join(", ")
                    }
                ),
                payload: None,
            })?);
            written.push(append(Record::Decision {
                claim: Claim::Declared,
                venture,
                verdict: Verdict::Adopt {
                    mandate: hash,
                    path: dir.to_string_lossy().into_owned(),
                },
                by,
            })?);
            return Ok(written);
        }
        if (action == STOP || action == RESUME) && !parent_lever {
            written.push(append(Record::Action {
                claim: Claim::Measured,
                venture,
                action,
                proof,
                ok: true,
                detail: String::new(),
                payload: None,
            })?);
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
        // The payload comes from the record the proof names, never from
        // the request: the gate chose the record, the warden only reads it.
        let payload = match proof.payload_from() {
            Some(seq) => entries
                .iter()
                .find(|e| e.seq == seq)
                .and_then(|e| e.record.payload().cloned()),
            None => None,
        };
        if perm.takes().is_some() && payload.is_none() {
            return Err(format!("{action} takes a payload and the proof names none"));
        }
        if perm.is_parent_lever() {
            // Non-negotiable 10: a parent allocates. Pulling the lever is
            // the record itself; the child's gate reads it from the ledger.
            let text = payload.as_ref().map(Payload::text).unwrap_or_default();
            let call = ParentCall::parse(&action, &text)?;
            let child = ventures
                .get(call.child())
                .ok_or_else(|| format!("{} is not a venture", call.child()))?;
            if !v.mandate.children.iter().any(|c| c == call.child())
                || child.mandate.parent.as_deref() != Some(venture.as_str())
            {
                return Err(format!(
                    "{} is not a child of {venture}: both mandates must say so",
                    call.child()
                ));
            }
            written.push(append(Record::Action {
                claim: Claim::Measured,
                venture,
                action,
                proof,
                ok: true,
                detail: call.describe(),
                payload,
            })?);
            return Ok(written);
        }
        if perm.is_research() {
            let question = payload.as_ref().map(Payload::text).unwrap_or_default();
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
            let done = append(Record::Action {
                claim: Claim::Measured,
                venture: venture.clone(),
                action,
                proof,
                ok,
                detail,
                payload,
            })?;
            let seq = done.seq;
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
                    ));
                }
            }
        }
        let proof_text = proof.to_string();
        let (for_venture, for_action) = (venture.clone(), action.clone());
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
                        venture: &for_venture,
                        action: &for_action,
                        proof: &proof_text,
                    },
                ),
                (Some(lever), _, None) => (false, format!("{} needs a payload", lever.name())),
                (None, Some(run), p) => run_lever(&dir, run, p, ACTION_TIMEOUT),
                (None, None, _) => (false, format!("{for_action} has no command and no adapter")),
            }
        };
        if perm.reversible {
            let (ok, detail) = pull(payload.as_ref());
            written.push(append(Record::Action {
                claim: Claim::Measured,
                venture,
                action,
                proof,
                ok,
                detail,
                payload,
            })?);
        } else {
            let intent = append(Record::Intent {
                claim: Claim::Measured,
                venture: venture.clone(),
                action,
                proof,
                payload: payload.clone(),
            })?;
            let (ok, detail) = pull(payload.as_ref());
            let outcome = append(Record::Outcome {
                claim: Claim::Measured,
                venture,
                intent: intent.seq,
                ok,
                detail,
            })?;
            written.push(intent);
            written.push(outcome);
        }
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
            Backend::Command { run } => {
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
                        "backend {name} replied with {} bytes; the most a reply may hold is {MAX_REPLY}",
                        ran.stdout.len()
                    ));
                }
                Ok(ran.stdout)
            }
        }
    }

    fn measure(&mut self, venture: &str) -> Result<(Option<f64>, String), String> {
        let entries = self.ledger.all().map_err(|e| e.to_string())?;
        self.measure_in(&entries, venture)
    }
}

impl LocalWarden {
    fn measure_in(
        &self,
        entries: &[Entry],
        venture: &str,
    ) -> Result<(Option<f64>, String), String> {
        let ventures = venture::load_all(entries);
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
    let Backend::Command { run: command } = backend else {
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
            let _ = s.read_to_string(&mut out);
        }
        let mut err = String::new();
        if let Some(s) = stderr.as_mut() {
            let _ = s.read_to_string(&mut err);
        }
        (out, err)
    });
    let began = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if began.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
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

/// Run a shell command in a directory; see `run`. Returns whether it
/// exited 0 and one line of what happened.
pub fn run_command(dir: &Path, command: &str, stdin: &str, timeout: Duration) -> (bool, String) {
    let ran = run(dir, "sh", &["-c", command], stdin, timeout);
    (ran.ok, ran.detail(timeout))
}

/// Run a lever's command with its payload, if it takes one, on standard
/// input: the text, the JSON, the diff, or the path of the file. The
/// shape is in `AVSM_PAYLOAD` so a script can tell which it got.
pub fn run_lever(
    dir: &Path,
    command: &str,
    payload: Option<&Payload>,
    timeout: Duration,
) -> (bool, String) {
    let (env, stdin): (Vec<(&str, String)>, String) = match payload {
        Some(p) => (vec![("AVSM_PAYLOAD", p.takes().to_string())], p.text()),
        None => (vec![], String::new()),
    };
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
}

/// What a warden process answers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FromWarden {
    Done(Vec<Entry>),
    Text(String),
    Measured(Option<f64>, String),
    Err(String),
}

#[cfg(test)]
mod tests {
    use super::*;

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
            Duration::from_secs(5),
        );
        assert!(ok);
        assert_eq!(out, "json: {\"n\":1}");
        let (ok, out) = run_lever(
            tmp.path(),
            "echo \"[$AVSM_PAYLOAD]\"",
            None,
            Duration::from_secs(5),
        );
        assert!(ok);
        assert_eq!(out, "[]");
    }

    #[test]
    fn fallbacks_only_for_models_that_take_them() {
        assert!(fallbacks_apply("claude-opus-5-5"));
        assert!(fallbacks_apply("claude-fable-5-1"));
        assert!(!fallbacks_apply("claude-haiku-5-5"));
        assert!(!fallbacks_apply("claude-sonnet-4-6"));
    }
}
