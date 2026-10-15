//! The warden: the only process that holds keys. It checks the gate again,
//! runs actions, writes what happened, and sends prompts to backends with
//! the key attached.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::{self, Backend, Config};
use crate::gate::{self, Candidate, RESUME, STOP};
use crate::ledger::Ledger;
use crate::record::{Claim, Entry, Record};
use crate::role::InferRequest;
use crate::venture;

/// How long an action or a report may run.
pub const ACTION_TIMEOUT: Duration = Duration::from_secs(600);

/// What the kernel asks of the warden.
pub trait Warden {
    /// Gate the action again; if it passes, run it and write the action
    /// record (or intent then outcome). Returns the records written.
    fn execute(&mut self, candidate: &Candidate) -> Result<Vec<Entry>, String>;
    /// Send a prompt to a backend with its key and return the reply text.
    fn infer(&mut self, request: &InferRequest) -> Result<String, String>;
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
        let proof = gate::gate(candidate, &entries, &adopted, now()).map_err(|r| r.to_string())?;
        let venture = candidate.venture.clone();
        let action = candidate.action.clone();
        let mut written = Vec::new();
        let mut append = |record: Record| -> Result<Entry, String> {
            self.ledger.append(record).map_err(|e| e.to_string())
        };
        if action == STOP || action == RESUME {
            written.push(append(Record::Action {
                claim: Claim::Measured,
                venture,
                action,
                proof,
                ok: true,
                detail: String::new(),
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
        if perm.reversible {
            let (ok, detail) = run_command(&dir, &perm.run, "", ACTION_TIMEOUT);
            written.push(append(Record::Action {
                claim: Claim::Measured,
                venture,
                action,
                proof,
                ok,
                detail,
            })?);
        } else {
            let intent = append(Record::Intent {
                claim: Claim::Measured,
                venture: venture.clone(),
                action,
                proof,
            })?;
            let (ok, detail) = run_command(&dir, &perm.run, "", ACTION_TIMEOUT);
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
                    ("x-api-key", key.as_str()),
                    ("anthropic-version", "2023-06-01"),
                ];
                if fallbacks_apply(model) {
                    body["fallbacks"] = serde_json::json!("default");
                    headers.push(("anthropic-beta", "server-side-fallback-2026-07-01"));
                }
                let reply = post_json(url, &headers, &body)?;
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
                let bearer;
                let mut headers = Vec::new();
                if let Some(k) = key {
                    let value = self.keys.get(k).ok_or_else(|| {
                        format!("backend {name} needs a key named {k} in the keys file")
                    })?;
                    bearer = format!("Bearer {value}");
                    headers.push(("authorization", bearer.as_str()));
                }
                let reply = post_json(url, &headers, &body)?;
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
                let (ok, out) = run_command(&self.home, run, &stdin, ACTION_TIMEOUT);
                if ok {
                    Ok(out)
                } else {
                    Err(format!("backend {name} failed: {out}"))
                }
            }
        }
    }
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

fn post_json(
    url: &str,
    headers: &[(&str, &str)],
    body: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(ACTION_TIMEOUT))
        .http_status_as_error(false)
        .build()
        .into();
    let mut req = agent.post(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let mut resp = req
        .send_json(body)
        .map_err(|e| format!("cannot reach {url}: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("bad answer from {url}: {e}"))?;
    if status >= 300 {
        return Err(format!(
            "{url} answered {status}: {}",
            text.chars().take(400).collect::<String>()
        ));
    }
    serde_json::from_str(&text).map_err(|e| format!("{url} did not answer JSON: {e}"))
}

/// Run a shell command in a directory with a clean environment, feeding it
/// `stdin`, and wait up to `timeout`. Returns whether it exited 0 and the
/// tail of what it printed.
pub fn run_command(dir: &Path, run: &str, stdin: &str, timeout: Duration) -> (bool, String) {
    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(run)
        .current_dir(dir)
        .env_clear()
        .env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string()),
        )
        .env("HOME", dir)
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
        Err(e) => return (false, format!("cannot run {run:?}: {e}")),
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
    let (out, err) = reader.join().unwrap_or_default();
    let mut detail = out.trim().to_string();
    let err = err.trim();
    if !err.is_empty() {
        if !detail.is_empty() {
            detail.push_str("  ");
        }
        detail.push_str(err);
    }
    match status {
        Some(s) if s.success() => (true, tail(&detail)),
        Some(s) => (
            false,
            tail(format!("exit {}  {detail}", s.code().unwrap_or(-1)).trim()),
        ),
        None => (
            false,
            tail(&format!("killed after {}s  {detail}", timeout.as_secs())),
        ),
    }
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
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// What the kernel sends to a warden process.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToWarden {
    Execute(Candidate),
    Infer(InferRequest),
}

/// What a warden process answers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FromWarden {
    Done(Vec<Entry>),
    Text(String),
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
    fn fallbacks_only_for_models_that_take_them() {
        assert!(fallbacks_apply("claude-opus-5-5"));
        assert!(fallbacks_apply("claude-fable-5-1"));
        assert!(!fallbacks_apply("claude-haiku-5-5"));
        assert!(!fallbacks_apply("claude-sonnet-4-6"));
    }
}
