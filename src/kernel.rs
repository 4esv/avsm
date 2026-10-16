//! The kernel: holds the ledger and the ventures, answers commands,
//! supervises each venture's `run`, reads each `report` on its cadence,
//! acts on declared decisions, and runs the nightly cycle through a role.
//! It never holds a key: execution and backend calls go through the warden.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{Local, NaiveDate, TimeZone, Timelike};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::gate::{self, Adopted, Candidate, Reason, State, Step, SELF};
use crate::ledger::Ledger;
use crate::mandate::{Controller, Permitted, Restart};
use crate::record::{Claim, Entry, Payload, Record, Seq, Verdict};
use crate::role::{self, ChildView, Context, InferRequest, ModelRole, Role, SUMMARY};
use crate::venture::{self, Venture};
use crate::warden::{self, FromWarden, LocalWarden, ToWarden, Warden};

/// How long a report may take.
pub const REPORT_TIMEOUT: Duration = Duration::from_secs(60);
/// The longest wait before restarting a dead `run`.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A `run` alive this long has its backoff forgiven.
const STEADY: Duration = Duration::from_secs(600);

/// The kernel's working state.
pub struct Kernel {
    pub home: PathBuf,
    pub ledger: Ledger,
    pub ventures: BTreeMap<String, Venture>,
    pub config: Config,
    role: Box<dyn Role + Send>,
    warden: Box<dyn Warden + Send>,
    runtime: BTreeMap<String, Runtime>,
    /// Declared decisions already acted on (or tried) in this process.
    handled: BTreeSet<Seq>,
    /// The local date each venture last had its nightly cycle.
    cycled: BTreeMap<String, NaiveDate>,
    /// Who is giving the command being handled, when the path it came in
    /// over knows: a Tailscale login. The socket knows nobody.
    by: Option<String>,
    /// Can a `run` be put in a network namespace of its own here? Found
    /// out once, the first time one needs it.
    namespaces: Option<bool>,
}

/// What the kernel knows about a venture beyond the ledger.
#[derive(Default)]
struct Runtime {
    run: Option<Child>,
    started: Option<Instant>,
    /// When the next report is due, unix seconds; 0 means now.
    next_report: i64,
    backoff: Duration,
    restart_at: Option<Instant>,
    /// The run exited and the policy says leave it.
    dead: bool,
    /// Something a human should know about how the run is running.
    note: Option<String>,
}

/// Unix seconds now.
pub fn now() -> i64 {
    warden::now()
}

/// Where avsm keeps its files: `$AVSM_HOME`, else `~/.avsm`.
pub fn default_home() -> PathBuf {
    if let Some(h) = std::env::var_os("AVSM_HOME") {
        return PathBuf::from(h);
    }
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join(".avsm")
}

/// The socket inside a home directory.
pub fn socket_path(home: &Path) -> PathBuf {
    home.join("sock")
}

/// A role for a kernel that has no roles process.
struct NoRole;

impl Role for NoRole {
    fn decide(
        &mut self,
        _ctx: &Context,
        _infer: &mut dyn FnMut(&InferRequest) -> Result<String, String>,
        _drill: &mut dyn FnMut(&str, &str) -> Result<String, String>,
    ) -> Result<Vec<Record>, String> {
        Err("no roles process; start avsm with: avsm start".to_string())
    }
}

/// A warden for a kernel that has no warden process.
struct NoWarden;

impl Warden for NoWarden {
    fn execute(&mut self, _candidate: &Candidate) -> Result<Vec<Entry>, String> {
        Err("no warden process; start avsm with: avsm start".to_string())
    }

    fn infer(&mut self, _request: &InferRequest) -> Result<String, String> {
        Err("no warden process; start avsm with: avsm start".to_string())
    }

    fn measure(&mut self, _venture: &str) -> Result<(Option<f64>, String), String> {
        Err("no warden process; start avsm with: avsm start".to_string())
    }
}

impl Kernel {
    /// Open the ledger in `home` (created if missing) with no role and no
    /// warden: enough to answer every command, not to act.
    pub fn open(home: &Path) -> Result<Kernel, String> {
        Kernel::with(home, Box::new(NoRole), Box::new(NoWarden))
    }

    /// Open the ledger in `home` with the given role and warden.
    pub fn with(
        home: &Path,
        role: Box<dyn Role + Send>,
        warden: Box<dyn Warden + Send>,
    ) -> Result<Kernel, String> {
        std::fs::create_dir_all(home)
            .map_err(|e| format!("cannot create {}: {e}", home.display()))?;
        let ledger = Ledger::open(&home.join("ledger.db")).map_err(|e| e.to_string())?;
        let config = Config::load(home)?;
        let mut kernel = Kernel {
            home: home.to_path_buf(),
            ledger,
            ventures: BTreeMap::new(),
            config,
            role,
            warden,
            runtime: BTreeMap::new(),
            handled: BTreeSet::new(),
            cycled: BTreeMap::new(),
            by: None,
            namespaces: None,
        };
        kernel.reload()?;
        Ok(kernel)
    }

    /// Rebuild the ventures from the latest declared Adopt record of each.
    fn reload(&mut self) -> Result<(), String> {
        let entries = self.entries()?;
        self.ventures = venture::load_all(&entries);
        Ok(())
    }

    /// Every entry, oldest first.
    pub fn entries(&self) -> Result<Vec<Entry>, String> {
        self.ledger.all().map_err(|e| e.to_string())
    }

    /// The mandates the gate may use: adopted and unchanged.
    pub fn adopted(&self) -> Vec<Adopted> {
        venture::adopted(&self.ventures)
    }

    /// Append a record now.
    pub fn append(&mut self, record: Record) -> Result<Entry, String> {
        self.ledger.append(record).map_err(|e| e.to_string())
    }

    /// Run one command from the socket. The first argument is the command.
    pub fn handle(&mut self, args: &[String]) -> Result<String, String> {
        self.handle_as(args, None)
    }

    /// Run one command given by a known human; their login goes on every
    /// declared record the command writes.
    pub fn handle_as(&mut self, args: &[String], by: Option<&str>) -> Result<String, String> {
        self.by = by.map(str::to_string);
        let result = self.dispatch(args);
        self.by = None;
        result
    }

    fn dispatch(&mut self, args: &[String]) -> Result<String, String> {
        let (cmd, rest) = args
            .split_first()
            .ok_or_else(|| "no command given".to_string())?;
        let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
        match (cmd.as_str(), rest.as_slice()) {
            ("add", [dir]) => self.add(Path::new(dir)),
            ("status", []) => self.status(),
            ("tail", []) => self.tail(20),
            ("tail", [n]) => self.tail(
                n.parse()
                    .map_err(|_| format!("tail takes a count, not {n:?}"))?,
            ),
            ("asks", []) => self.asks(),
            ("approve", [seq]) => self.approve(parse_seq(seq)?, None),
            ("approve", [seq, "--payload", text]) => self.approve(parse_seq(seq)?, Some(text)),
            ("payload", [seq]) => self.payload(parse_seq(seq)?),
            ("deny", [seq, reason @ ..]) => self.deny(parse_seq(seq)?, &reason.join(" ")),
            ("pause", [name]) => self.pause(name),
            ("resume", [name]) => self.resume(name),
            ("why", [seq]) => self.why(parse_seq(seq)?),
            ("know", [what]) => self.know(what),
            ("propose", [venture, lever]) => self.propose(venture, lever, None),
            ("propose", [venture, lever, payload @ ..]) => {
                self.propose(venture, lever, Some(&payload.join(" ")))
            }
            _ => Err(format!("avsm does not understand: {}", args.join(" "))),
        }
    }

    /// A declared decision by whoever is giving the command.
    fn decision(&self, venture: &str, verdict: Verdict) -> Record {
        Record::Decision {
            claim: Claim::Declared,
            venture: venture.to_string(),
            verdict,
            by: self.by.clone(),
        }
    }

    /// Take on the venture in a directory: adopt its mandate.
    pub fn add(&mut self, dir: &Path) -> Result<String, String> {
        let dir = dir
            .canonicalize()
            .map_err(|e| format!("cannot find {}: {e}", dir.display()))?;
        let name = venture::name_of(&dir)?;
        let (mandate, hash) = venture::read_mandate(&dir)?;
        self.check_family(&name, &mandate)?;
        if mandate.metric_adapter().is_none() && !dir.join("report").is_file() {
            return Err(format!(
                "{} watches {} but holds no report program to read it; add one, or name a built-in metric like {}",
                dir.display(),
                mandate.metric_name(),
                crate::adapter::METRICS.join(", ")
            ));
        }
        let entry = self.append(self.decision(
            &name,
            Verdict::Adopt {
                mandate: hash.clone(),
                path: dir.to_string_lossy().into_owned(),
            },
        ))?;
        let v = Venture {
            name: name.clone(),
            path: Some(dir.clone()),
            mandate: mandate.clone(),
            hash,
            adopted: entry.seq,
            problem: None,
        };
        self.ventures.insert(name.clone(), v);
        // A fresh mandate means a fresh run and a report right away.
        if let Some(mut rt) = self.runtime.remove(&name) {
            kill(&mut rt);
        }
        let mut out = format!("added {name} from {}\n", dir.display());
        out.push_str(&format!("  {}\n", mandate.purpose.trim()));
        out.push_str(&format!(
            "  watching {} every {}{}\n",
            mandate.metric_name(),
            humantime::format_duration(mandate.cadence),
            if mandate.metric_adapter().is_some() {
                " through the warden"
            } else {
                ""
            }
        ));
        for p in &mandate.permitted {
            out.push_str(&format!(
                "  may {}  {}  {}{}\n",
                p.name,
                p.cap,
                if p.reversible {
                    "reversible"
                } else if p.delegated {
                    "irreversible, delegated"
                } else {
                    "irreversible, will ask first"
                },
                match p.takes() {
                    Some(t) if p.delegated => format!(", takes {t}"),
                    Some(t) => format!(", takes {t} and will ask first"),
                    None => String::new(),
                }
            ));
        }
        if !mandate.children.is_empty() {
            out.push_str(&format!("  parent of {}\n", mandate.children.join(", ")));
        }
        if let Some(p) = &mandate.parent {
            out.push_str(&format!("  child of {p}\n"));
        }
        out.push_str(&format!("  {}\n", entry.line()));
        Ok(out)
    }

    /// Refuse a mandate whose parent and children do not agree with the
    /// mandates already adopted, or that would make a loop.
    fn check_family(&self, name: &str, mandate: &crate::mandate::Mandate) -> Result<(), String> {
        if mandate.children.iter().any(|c| c == name) {
            return Err(format!("{name} cannot be its own child"));
        }
        if mandate.parent.as_deref() == Some(name) {
            return Err(format!("{name} cannot be its own parent"));
        }
        let adopted: Vec<&Venture> = self
            .ventures
            .values()
            .filter(|v| v.name != name && v.problem.is_none())
            .collect();
        if let Some(p) = &mandate.parent {
            if let Some(parent) = adopted.iter().find(|v| v.name == *p) {
                if !parent.mandate.children.iter().any(|c| c == name) {
                    return Err(format!(
                        "{name} names {p} as its parent, but {p} does not list {name} under children; add it there and run avsm add on {p} first"
                    ));
                }
            }
        }
        for parent in adopted
            .iter()
            .filter(|v| v.mandate.children.iter().any(|c| c == name))
        {
            match mandate.parent.as_deref() {
                Some(q) if q == parent.name => {}
                Some(q) => {
                    return Err(format!(
                        "{} lists {name} under children, but {name} names {q} as its parent",
                        parent.name
                    ))
                }
                None => {
                    return Err(format!(
                        "{} lists {name} under children, but {name} names no parent; set parent: {} in its mandate",
                        parent.name, parent.name
                    ))
                }
            }
        }
        for child in adopted
            .iter()
            .filter(|v| v.mandate.parent.as_deref() == Some(name))
        {
            if !mandate.children.contains(&child.name) {
                return Err(format!(
                    "{} names {name} as its parent, but {name} does not list {} under children; remove parent from {} first",
                    child.name, child.name, child.name
                ));
            }
        }
        for c in &mandate.children {
            if let Some(child) = adopted.iter().find(|v| v.name == *c) {
                match child.mandate.parent.as_deref() {
                    Some(q) if q == name => {}
                    Some(q) => {
                        return Err(format!(
                            "{c} names {q} as its parent, not {name}; a venture has one parent"
                        ))
                    }
                    None => {
                        return Err(format!(
                            "{c} does not name {name} as its parent; set parent: {name} in its mandate and run avsm add on it"
                        ))
                    }
                }
            }
        }
        // Edges from parent to child, as every mandate has them; a path
        // from this venture back to itself is a loop.
        let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut add_edges = |who: &str, m: &crate::mandate::Mandate| {
            for c in &m.children {
                edges.entry(who.to_string()).or_default().insert(c.clone());
            }
            if let Some(p) = &m.parent {
                edges.entry(p.clone()).or_default().insert(who.to_string());
            }
        };
        for v in &adopted {
            add_edges(&v.name, &v.mandate);
        }
        add_edges(name, mandate);
        let mut path = vec![name.to_string()];
        let mut seen = BTreeSet::new();
        fn walk(
            at: &str,
            edges: &BTreeMap<String, BTreeSet<String>>,
            path: &mut Vec<String>,
            seen: &mut BTreeSet<String>,
        ) -> Option<Vec<String>> {
            for next in edges.get(at).into_iter().flatten() {
                if next == &path[0] {
                    let mut found = path.clone();
                    found.push(next.clone());
                    return Some(found);
                }
                if seen.insert(next.clone()) {
                    path.push(next.clone());
                    if let Some(found) = walk(next, edges, path, seen) {
                        return Some(found);
                    }
                    path.pop();
                }
            }
            None
        }
        if let Some(found) = walk(name, &edges, &mut path, &mut seen) {
            return Err(format!(
                "that would make a loop: {}; a venture cannot be its own ancestor",
                found.join(" -> ")
            ));
        }
        Ok(())
    }

    /// One line per venture.
    pub fn status(&self) -> Result<String, String> {
        if self.ventures.is_empty() {
            return Ok("no ventures; add one with: avsm add <directory>\n".to_string());
        }
        let entries = self.entries()?;
        let now = now();
        let mut out = String::new();
        for v in self.ventures.values() {
            let state = gate::state(&entries, &self.adopted(), &v.name);
            let last = entries.iter().rev().find(|e| {
                e.record.venture() == v.name && matches!(e.record, Record::Report { .. })
            });
            let mut detail = match &v.problem {
                Some(p) => p.clone(),
                None => match last {
                    Some(e) => format!("{}  reported {}", e.record.summary(), ago(now - e.at)),
                    None => "no report yet".to_string(),
                },
            };
            if let Some(note) = self.runtime.get(&v.name).and_then(|rt| rt.note.as_deref()) {
                detail.push_str(&format!("  ({note})"));
            }
            out.push_str(&format!("{:<12} {:<8} {detail}\n", v.name, state));
        }
        Ok(out)
    }

    /// The last `n` records, one line each.
    pub fn tail(&self, n: usize) -> Result<String, String> {
        let entries = self.ledger.tail(n).map_err(|e| e.to_string())?;
        if entries.is_empty() {
            return Ok("the ledger is empty\n".to_string());
        }
        Ok(entries.iter().map(|e| e.line() + "\n").collect())
    }

    /// The questions waiting for a human, one line each, with the payload
    /// the lever would consume under each that carries one.
    pub fn asks(&self) -> Result<String, String> {
        let entries = self.entries()?;
        let open = gate::open_asks(&entries);
        if open.is_empty() {
            return Ok("no open asks\n".to_string());
        }
        let mut out = String::new();
        for e in entries.iter().filter(|e| open.contains(&e.seq)) {
            out.push_str(&e.line());
            out.push('\n');
            if let Some(p) = e.record.payload() {
                out.push_str(&indent(&p.text(), 40));
            }
        }
        Ok(out)
    }

    /// The payload of an open ask: its shape on the first line, then the
    /// content. For `approve --edit`.
    pub fn payload(&self, seq: Seq) -> Result<String, String> {
        let ask = self.open_ask(seq)?;
        match ask.record.payload() {
            Some(p) => Ok(format!("{}\n{}", p.takes(), p.text())),
            None => Err(format!("#{seq} carries no payload to edit")),
        }
    }

    fn open_ask(&self, seq: Seq) -> Result<Entry, String> {
        let entry = self.entry(seq)?;
        let entries = self.entries()?;
        if !gate::open_asks(&entries).contains(&seq) {
            return Err(format!("#{seq} is not an open ask\n{}", entry.line()));
        }
        Ok(entry)
    }

    /// Say yes to an ask, with the payload as the human amended it when
    /// they did. The next tick acts on it.
    pub fn approve(&mut self, seq: Seq, amended: Option<&str>) -> Result<String, String> {
        let ask = self.open_ask(seq)?;
        let payload = match amended {
            None => None,
            Some(text) => {
                let takes = ask
                    .record
                    .payload()
                    .map(Payload::takes)
                    .ok_or_else(|| format!("#{seq} carries no payload to amend"))?;
                Some(Payload::parse(takes, text)?)
            }
        };
        let entry = self
            .append(self.decision(ask.record.venture(), Verdict::Approve { ask: seq, payload }))?;
        Ok(entry.line() + "\n")
    }

    /// Say no to an ask.
    pub fn deny(&mut self, seq: Seq, reason: &str) -> Result<String, String> {
        let ask = self.open_ask(seq)?;
        let reason = if reason.trim().is_empty() {
            "no reason given".to_string()
        } else {
            reason.trim().to_string()
        };
        let entry =
            self.append(self.decision(ask.record.venture(), Verdict::Deny { ask: seq, reason }))?;
        Ok(entry.line() + "\n")
    }

    /// File a proposal by hand. It is declared, and it still goes through
    /// the gate right away: what the gate said comes back with the record.
    pub fn propose(
        &mut self,
        name: &str,
        lever: &str,
        payload: Option<&str>,
    ) -> Result<String, String> {
        let v = self
            .ventures
            .get(name)
            .ok_or_else(|| format!("no venture named {name}; see: avsm status"))?;
        let perm = v
            .mandate
            .permitted(lever)
            .ok_or_else(|| format!("{name} does not permit {lever}; see its mandate"))?;
        let payload = match (perm.takes(), payload) {
            (None, None) => None,
            (None, Some(_)) => return Err(format!("{lever} takes no payload")),
            (Some(t), None) => {
                return Err(format!("{lever} takes {t}; give it after the lever name"))
            }
            (Some(t), Some(text)) => Some(Payload::parse(t, text)?),
        };
        let entry = self.append(Record::Proposal {
            claim: Claim::Declared,
            venture: name.to_string(),
            action: lever.to_string(),
            reason: match &self.by {
                Some(who) => format!("proposed by {who}"),
                None => "proposed by hand".to_string(),
            },
            payload,
        })?;
        let mut out = entry.line() + "\n";
        for line in self.act(name, lever, vec![entry.seq], Claim::Declared) {
            out.push_str(&line);
            out.push('\n');
        }
        Ok(out)
    }

    fn known(&self, name: &str) -> Result<(), String> {
        if self.ventures.contains_key(name) || name == SELF {
            Ok(())
        } else {
            Err(format!("no venture named {name}; see: avsm status"))
        }
    }

    /// Stop a venture until a human resumes it. The next tick stops its run.
    pub fn pause(&mut self, name: &str) -> Result<String, String> {
        self.known(name)?;
        let entry = self.append(self.decision(name, Verdict::Pause))?;
        Ok(entry.line() + "\n")
    }

    /// Let a paused venture run again.
    pub fn resume(&mut self, name: &str) -> Result<String, String> {
        self.known(name)?;
        let entry = self.append(self.decision(name, Verdict::Resume))?;
        Ok(entry.line() + "\n")
    }

    fn entry(&self, seq: Seq) -> Result<Entry, String> {
        self.ledger
            .get(seq)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no record #{seq}"))
    }

    /// The proof behind a record. A proposal is gated afresh, so the text
    /// says what would happen if it were acted on now.
    pub fn why(&self, seq: Seq) -> Result<String, String> {
        let entries = self.entries()?;
        why_in(&entries, &self.adopted(), &self.config.quotas, seq)
    }

    /// What is held true now about a venture, or about a topic across
    /// ventures: each current fact with its age, claim and leaves.
    pub fn know(&self, what: &str) -> Result<String, String> {
        let entries = self.entries()?;
        let ventures: BTreeSet<String> = self.ventures.keys().cloned().collect();
        Ok(know_in(&entries, &ventures, what))
    }

    // ----- the loop -----

    /// One pass: supervise every run, read reports that are due, act on
    /// declared decisions, and run the nightly cycle when its time comes.
    /// Problems are logged and never stop the pass.
    pub fn tick(&mut self) {
        let names: Vec<String> = self.ventures.keys().cloned().collect();
        let entries = match self.entries() {
            Ok(e) => e,
            Err(e) => {
                log(&format!("cannot read the ledger: {e}"));
                return;
            }
        };
        let adopted = self.adopted();
        let halted = gate::state(&entries, &adopted, SELF) == State::Paused;
        let now = now();
        for name in &names {
            let paused = halted || gate::state(&entries, &adopted, name) == State::Paused;
            self.supervise(name, paused);
            let due = self.runtime.entry(name.clone()).or_default().next_report <= now;
            if due && self.ventures[name].problem.is_none() {
                self.report(name, halted);
            }
        }
        if !halted {
            self.act_on_decisions();
            for name in &names {
                if self.nightly_due(name) {
                    self.cycle(name);
                }
            }
        }
    }

    /// Start, restart or stop a venture's `run` as its state and policy say.
    fn supervise(&mut self, name: &str, paused: bool) {
        let Some(v) = self.ventures.get(name).cloned() else {
            return;
        };
        let rt = self.runtime.entry(name.to_string()).or_default();
        if let Some(child) = rt.run.as_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    rt.run = None;
                    let again = match v.mandate.restart {
                        Restart::Always => true,
                        Restart::OnFailure => !status.success(),
                        Restart::Never => false,
                    };
                    if again {
                        let wait = rt.backoff.max(Duration::from_secs(1));
                        log(&format!(
                            "{name}: run exited ({status}); restarting in {}s",
                            wait.as_secs()
                        ));
                        rt.restart_at = Some(Instant::now() + wait);
                        rt.backoff = (wait * 2).min(MAX_BACKOFF);
                    } else {
                        log(&format!("{name}: run exited ({status}); leaving it"));
                        rt.dead = true;
                    }
                }
                Ok(None) => {
                    if paused {
                        log(&format!("{name}: paused; stopping run"));
                        kill(rt);
                    } else if rt.started.is_some_and(|s| s.elapsed() > STEADY) {
                        rt.backoff = Duration::ZERO;
                    }
                }
                Err(_) => rt.run = None,
            }
        }
        let waiting = rt.restart_at.is_some_and(|t| Instant::now() < t);
        if rt.run.is_none() && !paused && !rt.dead && !waiting {
            if let Some(path) = v.run() {
                // `network: none` means a network namespace of its own,
                // where Linux allows one; a list of hosts cannot be held
                // to for a run, only for adapters.
                let wants_none = v.mandate.network.as_ref().is_some_and(Vec::is_empty);
                let confine =
                    wants_none && *self.namespaces.get_or_insert_with(namespaces_available);
                let note = match &v.mandate.network {
                    Some(hosts) if hosts.is_empty() && !confine => Some(
                        "network: none is not enforced here; this Linux allows no network namespace, so run has network"
                            .to_string(),
                    ),
                    Some(hosts) if !hosts.is_empty() => Some(
                        "network list holds adapters only; run is not held to it".to_string(),
                    ),
                    _ => None,
                };
                let rt = self.runtime.entry(name.to_string()).or_default();
                match spawn_run(&self.home, name, &path, confine) {
                    Ok(child) => {
                        log(&format!(
                            "{name}: run started (pid {}){}",
                            child.id(),
                            if confine { " with no network" } else { "" }
                        ));
                        if let Some(n) = &note {
                            log(&format!("{name}: {n}"));
                        }
                        rt.run = Some(child);
                        rt.started = Some(Instant::now());
                        rt.restart_at = None;
                        rt.note = note;
                    }
                    Err(e) => {
                        log(&format!("{name}: cannot start run: {e}"));
                        rt.restart_at = Some(Instant::now() + MAX_BACKOFF);
                    }
                }
            }
        }
    }

    /// Read the venture's report now, write it, and act on a threshold.
    pub fn report(&mut self, name: &str, halted: bool) {
        let Some(v) = self.ventures.get(name).cloned() else {
            return;
        };
        let cadence = v.mandate.cadence.as_secs() as i64;
        self.runtime
            .entry(name.to_string())
            .or_default()
            .next_report = now() + cadence.max(1);
        let (value, note) = match self.measure(&v) {
            Some(m) => m,
            None => return,
        };
        let entry = match self.append(Record::report(
            Claim::Measured,
            name,
            v.mandate.metric_name(),
            value,
            &note,
        )) {
            Ok(e) => e,
            Err(e) => {
                log(&format!("{name}: cannot write report: {e}"));
                return;
            }
        };
        if halted {
            return;
        }
        let Some(val) = value else { return };
        let Ok(entries) = self.entries() else { return };
        // The bound is the mandate's, or the one the parent set.
        if let Some(t) = gate::effective_threshold(&entries, &self.adopted(), name) {
            let crossed = (t.above && val > t.bound) || (!t.above && val < t.bound);
            if crossed {
                self.act(name, &t.action, vec![entry.seq], Claim::Measured);
            }
        }
    }

    /// Make sure the built-in self venture is adopted under the mandate
    /// this binary ships. Called on start.
    pub fn adopt_self(&mut self) -> Result<(), String> {
        let current = self
            .ventures
            .get(SELF)
            .filter(|v| v.problem.is_none())
            .is_some();
        if !current {
            let entry = self.append(crate::selfventure::adopt_record())?;
            log(&entry.line());
            self.reload()?;
        }
        Ok(())
    }

    /// Read a venture's metric: through the warden when it names an
    /// adapter, else by running its `report` and taking the last line.
    /// The built-in self venture measures itself.
    fn measure(&mut self, v: &Venture) -> Option<(Option<f64>, String)> {
        if v.name == SELF && v.path.is_none() {
            return Some(crate::selfventure::measure(&self.ledger));
        }
        if v.mandate.metric_adapter().is_some() {
            return Some(match self.warden.measure(&v.name) {
                Ok(measured) => measured,
                Err(e) => (None, format!("cannot measure: {e}")),
            });
        }
        let dir = v.path.clone()?;
        let report = v.report()?;
        let ran = warden::run(&dir, &report.to_string_lossy(), &[], "", REPORT_TIMEOUT);
        if !ran.ok {
            return Some((
                None,
                format!("report failed: {}", ran.detail(REPORT_TIMEOUT)),
            ));
        }
        match crate::adapter::number_from(&ran.stdout) {
            Ok(n) => Some((Some(n), String::new())),
            Err(_) => Some((
                None,
                format!(
                    "report did not print a number: {}",
                    ran.detail(REPORT_TIMEOUT)
                ),
            )),
        }
    }

    /// Try to take an action: gate it, and if allowed have the warden do
    /// it; if it needs a human, ask once. Returns what happened, one line
    /// each, after logging it.
    fn act(&mut self, name: &str, action: &str, cites: Vec<Seq>, claim: Claim) -> Vec<String> {
        let mut lines = Vec::new();
        let entries = match self.entries() {
            Ok(e) => e,
            Err(e) => {
                lines.push(format!("{name}: cannot read the ledger: {e}"));
                log(&lines[0]);
                return lines;
            }
        };
        let candidate = Candidate {
            venture: name.to_string(),
            action: action.to_string(),
            cites,
        };
        // The shared quotas come first: past one, the gate is not asked.
        let (_, over) = gate::quota_check(
            &entries,
            &self.adopted(),
            &self.config.quotas,
            name,
            action,
            now(),
        );
        if !over.is_empty() {
            let why: Vec<String> = over.iter().map(|r| r.to_string()).collect();
            lines.push(format!(
                "{name}: {action} refused before the gate: {}",
                why.join("; ")
            ));
            log(&lines[0]);
            return lines;
        }
        match gate::gate_with(
            &candidate,
            &entries,
            &self.adopted(),
            &self.config.quotas,
            now(),
        ) {
            Ok(_) => match self.warden.execute(&candidate) {
                Ok(written) => lines.extend(written.iter().map(Entry::line)),
                Err(e) => lines.push(format!("{name}: {action} did not run: {e}")),
            },
            Err(refusal) => {
                let needs_human = refusal
                    .reasons
                    .iter()
                    .any(|r| matches!(r, Reason::NeedsDecision { .. }));
                let already = gate::open_asks(&entries).into_iter().any(|q| {
                    entries.iter().any(|e| {
                        e.seq == q
                            && e.record.venture() == name
                            && e.record.action() == Some(action)
                    })
                });
                if needs_human && !already && !self.may_ask(&entries) {
                    lines.push(format!(
                        "{name}: {action} needs a decision, but the asks quota is used up; not asking"
                    ));
                } else if needs_human && !already {
                    // The ask carries the payload the lever would consume,
                    // from whichever cited record brought it.
                    let payload = candidate
                        .cites
                        .iter()
                        .filter_map(|s| entries.iter().find(|e| e.seq == *s))
                        .find_map(|e| e.record.payload().cloned());
                    let question = match &payload {
                        Some(_) => format!("may I {action} with this?"),
                        None => format!("may I {action}? it is irreversible"),
                    };
                    match self.append(Record::Ask {
                        claim,
                        venture: name.to_string(),
                        action: Some(action.to_string()),
                        question,
                        refusal: Some(refusal),
                        payload,
                    }) {
                        Ok(e) => lines.push(e.line()),
                        Err(e) => lines.push(format!("{name}: cannot write ask: {e}")),
                    }
                } else if needs_human {
                    lines.push(format!("{name}: {action} is already asked; see: avsm asks"));
                } else {
                    let why: Vec<String> = refusal.reasons.iter().map(|r| r.to_string()).collect();
                    lines.push(format!("{name}: {action} refused: {}", why.join("; ")));
                }
            }
        }
        for line in &lines {
            log(line);
        }
        lines
    }

    /// Is there room under the asks quota for one more ask?
    fn may_ask(&self, entries: &[Entry]) -> bool {
        match self.config.quotas.asks {
            Some(cap) => gate::asks_used(entries, cap, now()) < cap.count,
            None => true,
        }
    }

    /// Act on declared approvals not yet acted on.
    fn act_on_decisions(&mut self) {
        let entries = match self.entries() {
            Ok(e) => e,
            Err(_) => return,
        };
        let acted: BTreeSet<Seq> = entries
            .iter()
            .filter_map(|e| match &e.record {
                Record::Action { proof, .. } | Record::Intent { proof, .. } => Some(proof),
                _ => None,
            })
            .flat_map(|p| p.steps.iter())
            .filter_map(|s| match s {
                Step::Decision { seq, .. } => Some(*seq),
                _ => None,
            })
            .collect();
        let pending: Vec<(Seq, String, String)> = entries
            .iter()
            .filter_map(|e| match &e.record {
                Record::Decision {
                    claim: Claim::Declared,
                    venture,
                    verdict: Verdict::Approve { ask, .. },
                    ..
                } if !acted.contains(&e.seq) && !self.handled.contains(&e.seq) => {
                    let action = entries
                        .iter()
                        .find(|a| a.seq == *ask)
                        .and_then(|a| a.record.action().map(str::to_string))?;
                    Some((e.seq, venture.clone(), action))
                }
                _ => None,
            })
            .collect();
        for (seq, venture, action) in pending {
            self.handled.insert(seq);
            self.act(&venture, &action, vec![seq], Claim::Declared);
        }
    }

    /// Is the nightly cycle due for a venture with a model or classifier
    /// controller: past the nightly time today, with no cycle yet today?
    fn nightly_due(&self, name: &str) -> bool {
        let Some(v) = self.ventures.get(name) else {
            return false;
        };
        if v.problem.is_some() || matches!(v.mandate.controller, Controller::Threshold { .. }) {
            return false;
        }
        let Ok(Some(at)) = self.config.nightly_minutes() else {
            return false;
        };
        let local = Local::now();
        if local.hour() * 60 + local.minute() < at {
            return false;
        }
        let today = local.date_naive();
        if self.cycled.get(name) == Some(&today) {
            return false;
        }
        // After a restart, an inferred record from today means it ran.
        let Ok(entries) = self.entries() else {
            return false;
        };
        !entries.iter().any(|e| {
            e.record.venture() == name
                && e.record.claim() == Claim::Inferred
                && Local
                    .timestamp_opt(e.at, 0)
                    .single()
                    .is_some_and(|t| t.date_naive() == today)
        })
    }

    /// Run the nightly cycle for a venture now: show the role the mandate
    /// and recent records, write what it says as inferred records, and try
    /// each proposal through the gate.
    pub fn cycle(&mut self, name: &str) {
        self.cycled
            .insert(name.to_string(), Local::now().date_naive());
        let Some(v) = self.ventures.get(name).cloned() else {
            return;
        };
        let wanted = match &v.mandate.controller {
            Controller::Classifier { backend } | Controller::Model { backend } => backend.clone(),
            Controller::Threshold { .. } => return,
        };
        let entries = match self.entries() {
            Ok(e) => e,
            Err(e) => {
                log(&format!("{name}: cannot read the ledger: {e}"));
                return;
            }
        };
        let backend = match self.config.backend(wanted.as_deref()) {
            Ok((b, _)) => b,
            Err(e) => {
                self.ask_once(
                    name,
                    &entries,
                    &format!("{name} cannot run its nightly cycle: {e}"),
                );
                return;
            }
        };
        let mine: Vec<Entry> = entries
            .iter()
            .filter(|e| e.record.venture() == name)
            .cloned()
            .collect();
        // Context by budget: the mandate, the latest summary, the records
        // it does not stand for, and the open asks. Past the venture's
        // bound, the older records go to the role to be summarized first;
        // the previous summary goes with them so the new one carries it.
        let is_summary = |e: &Entry| {
            matches!(&e.record, Record::Report { claim: Claim::Inferred, metric, cites, .. }
                if metric == SUMMARY && !cites.is_empty())
        };
        let summaries: Vec<Entry> = mine.iter().filter(|e| is_summary(e)).cloned().collect();
        let covered: Seq = summaries
            .iter()
            .flat_map(|s| s.record.cites().iter().copied())
            .max()
            .unwrap_or(0);
        let latest_summary: Vec<Entry> = summaries
            .last()
            .filter(|s| s.seq > covered)
            .cloned()
            .into_iter()
            .collect();
        let open = gate::open_asks(&entries);
        let asks: Vec<Entry> = mine
            .iter()
            .filter(|e| open.contains(&e.seq))
            .cloned()
            .collect();
        let uncovered: Vec<Entry> = mine
            .iter()
            .filter(|e| e.seq > covered && !is_summary(e) && !open.contains(&e.seq))
            .cloned()
            .collect();
        let bound = v.mandate.summarize_after as usize;
        let (to_summarize, recent) = if uncovered.len() > bound {
            let keep = bound / 2;
            let mut older: Vec<Entry> = latest_summary.clone();
            older.extend_from_slice(&uncovered[..uncovered.len() - keep]);
            (older, uncovered[uncovered.len() - keep..].to_vec())
        } else {
            (vec![], uncovered)
        };
        let fixed: usize = serde_yaml::to_string(&v.mandate).map_or(0, |t| t.len())
            + asks.iter().map(|e| e.line().len() + 1).sum::<usize>()
            + latest_summary
                .iter()
                .map(|e| e.record.summary().len() + 1)
                .sum::<usize>();
        let limit = (v.mandate.context_budget as usize * 4).saturating_sub(fixed);
        // A parent sees its children's metrics and records: what code
        // measured and humans declared, never a model's or the world's
        // raw input into them.
        let children: Vec<ChildView> = v
            .mandate
            .children
            .iter()
            .filter_map(|c| {
                let child = self.ventures.get(c)?;
                if child.mandate.parent.as_deref() != Some(name) {
                    return None;
                }
                let theirs: Vec<&Entry> = entries
                    .iter()
                    .filter(|e| e.record.venture() == *c && e.record.claim().authorizes())
                    .collect();
                let metric = theirs
                    .iter()
                    .rev()
                    .find(|e| e.record.kind() == crate::record::Kind::Report)
                    .map_or("no report yet".to_string(), |e| {
                        format!("{}  reported {}", e.record.summary(), ago(now() - e.at))
                    });
                let lines: Vec<String> = theirs
                    .iter()
                    .rev()
                    .take(12)
                    .map(|e| e.line())
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                Some(ChildView {
                    name: c.clone(),
                    state: gate::state(&entries, &self.adopted(), c).to_string(),
                    metric,
                    lines,
                })
            })
            .collect();
        let ctx = Context {
            venture: name.to_string(),
            mandate: v.mandate.clone(),
            entries: role::trim_to_chars(&recent, limit),
            state: gate::state(&entries, &self.adopted(), name).to_string(),
            now: now(),
            backend,
            children,
            summaries: latest_summary,
            asks,
            to_summarize,
        };
        let adopted = self.adopted();
        let ventures: BTreeSet<String> = self.ventures.keys().cloned().collect();
        let quotas = self.config.quotas.clone();
        let records = {
            let Kernel {
                role,
                warden,
                ledger,
                ..
            } = self;
            // A drill reads the ledger as it stood when the cycle began,
            // and is itself a record.
            let mut drill = |what: &str, arg: &str| -> Result<String, String> {
                let text = match what {
                    "know" => know_in(&entries, &ventures, arg),
                    "why" => why_in(&entries, &adopted, &quotas, parse_seq(arg)?)?,
                    _ => return Err(format!("a drill is know or why, not {what}")),
                };
                let note = format!("{arg}: {} lines shown", text.lines().count());
                ledger
                    .append(Record::Report {
                        claim: Claim::Inferred,
                        venture: name.to_string(),
                        metric: what.to_string(),
                        value: None,
                        note,
                        source: None,
                        confidence: None,
                        cites: vec![],
                    })
                    .map_err(|e| e.to_string())?;
                Ok(text.chars().take(6000).collect())
            };
            role.decide(&ctx, &mut |req| warden.infer(req), &mut drill)
        };
        let records = match records {
            Ok(r) => r,
            Err(e) => {
                log(&format!("{name}: nightly cycle failed: {e}"));
                return;
            }
        };
        if records.is_empty() {
            log(&format!("{name}: nightly cycle: nothing"));
        }
        for record in records {
            // A role may only propose or ask, and only for its own venture.
            // A payload is kept only when the lever takes that shape.
            let record = match record {
                Record::Proposal {
                    action,
                    reason,
                    payload,
                    ..
                } => {
                    let payload = fit_payload(&v.mandate, &action, payload, name);
                    Record::Proposal {
                        claim: Claim::Inferred,
                        venture: name.to_string(),
                        action,
                        reason,
                        payload,
                    }
                }
                Record::Ask {
                    action,
                    question,
                    payload,
                    ..
                } => {
                    let payload = match &action {
                        Some(a) => fit_payload(&v.mandate, a, payload, name),
                        None => None,
                    };
                    Record::Ask {
                        claim: Claim::Inferred,
                        venture: name.to_string(),
                        action,
                        question,
                        refusal: None,
                        payload,
                    }
                }
                Record::Report {
                    metric,
                    note,
                    cites,
                    ..
                } if metric == SUMMARY => {
                    // A summary stands only for this venture's own records.
                    let own = cites.iter().all(|c| mine.iter().any(|e| e.seq == *c));
                    if cites.is_empty() || !own || note.trim().is_empty() {
                        log(&format!(
                            "{name}: role returned a summary that cites no records of its own"
                        ));
                        continue;
                    }
                    Record::Report {
                        claim: Claim::Inferred,
                        venture: name.to_string(),
                        metric,
                        value: None,
                        note: note.chars().take(role::SUMMARY_MAX + 1).collect(),
                        source: None,
                        confidence: None,
                        cites,
                    }
                }
                other => {
                    log(&format!(
                        "{name}: role returned a {} record; only proposals, asks and summaries count",
                        other.kind()
                    ));
                    continue;
                }
            };
            let entry = match self.append(record) {
                Ok(e) => e,
                Err(e) => {
                    log(&format!("{name}: cannot write record: {e}"));
                    continue;
                }
            };
            log(&entry.line());
            if let Record::Proposal { action, .. } = &entry.record {
                self.act(name, action, vec![entry.seq], Claim::Inferred);
            }
        }
    }

    /// Write an ask with this question unless one is already open, or the
    /// asks quota is used up.
    fn ask_once(&mut self, name: &str, entries: &[Entry], question: &str) {
        let open = gate::open_asks(entries);
        let already = entries.iter().any(|e| {
            open.contains(&e.seq)
                && matches!(&e.record, Record::Ask { question: q, .. } if q == question)
        });
        if already {
            return;
        }
        if !self.may_ask(entries) {
            log(&format!(
                "{name}: the asks quota is used up; not asking: {question}"
            ));
            return;
        }
        match self.append(Record::Ask {
            claim: Claim::Measured,
            venture: name.to_string(),
            action: None,
            question: question.to_string(),
            refusal: None,
            payload: None,
        }) {
            Ok(e) => log(&e.line()),
            Err(e) => log(&format!("{name}: cannot write ask: {e}")),
        }
    }

    /// The pid of a venture's run, if one is alive.
    pub fn run_pid(&self, name: &str) -> Option<u32> {
        self.runtime
            .get(name)
            .and_then(|rt| rt.run.as_ref())
            .map(Child::id)
    }

    /// Stop every run. Called on shutdown.
    pub fn shutdown(&mut self) {
        for (name, rt) in self.runtime.iter_mut() {
            if rt.run.is_some() {
                log(&format!("{name}: stopping run"));
                kill(rt);
            }
        }
    }
}

impl Drop for Kernel {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn kill(rt: &mut Runtime) {
    if let Some(mut child) = rt.run.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    rt.started = None;
}

/// Can this process put a child in a network namespace of its own: as
/// root with a new network namespace, or as anyone with a new user
/// namespace around it? Found out by trying on a child that does nothing.
pub fn namespaces_available() -> bool {
    let mut cmd = Command::new("true");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: the hook only calls unshare, which is async-signal-safe.
    unsafe {
        cmd.pre_exec(unshare_network);
    }
    cmd.status().is_ok_and(|s| s.success())
}

/// Leave the network behind: a new network namespace, through a new user
/// namespace when not root. Runs in the child before exec.
fn unshare_network() -> std::io::Result<()> {
    // SAFETY: unshare has no memory preconditions.
    if unsafe { libc::unshare(libc::CLONE_NEWNET) } == 0 {
        return Ok(());
    }
    // SAFETY: as above.
    if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) } == 0 {
        return Ok(());
    }
    Err(std::io::Error::last_os_error())
}

/// Start a venture's `run` in its directory, with a clean environment, its
/// output appended to `<home>/log/<name>.log`, and with no network when
/// `confine` is set.
fn spawn_run(home: &Path, name: &str, path: &Path, confine: bool) -> Result<Child, String> {
    let dir = path
        .parent()
        .ok_or_else(|| "run has no directory".to_string())?;
    let logs = home.join("log");
    std::fs::create_dir_all(&logs).map_err(|e| format!("cannot create {}: {e}", logs.display()))?;
    let file = std::fs::File::options()
        .append(true)
        .create(true)
        .open(logs.join(format!("{name}.log")))
        .map_err(|e| format!("cannot open log: {e}"))?;
    let err = file.try_clone().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(path);
    cmd.current_dir(dir)
        .env_clear()
        .env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string()),
        )
        .env("HOME", dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(err));
    if confine {
        // SAFETY: the hook only calls unshare, which is async-signal-safe.
        unsafe {
            cmd.pre_exec(unshare_network);
        }
    }
    cmd.spawn()
        .map_err(|e| format!("cannot run {}: {e}", path.display()))
}

/// The proof behind a record, from a snapshot of the ledger.
fn why_in(
    entries: &[Entry],
    adopted: &[Adopted],
    quotas: &crate::config::Quotas,
    seq: Seq,
) -> Result<String, String> {
    let entry = entries
        .iter()
        .find(|e| e.seq == seq)
        .ok_or_else(|| format!("no record #{seq}"))?;
    if let Record::Proposal {
        venture, action, ..
    } = &entry.record
    {
        let candidate = Candidate {
            venture: venture.clone(),
            action: action.clone(),
            cites: vec![seq],
        };
        let derivation = match gate::gate_with(&candidate, entries, adopted, quotas, now()) {
            Ok(proof) => proof.to_string(),
            Err(refusal) => refusal.to_string(),
        };
        return Ok(format!("{}\n{derivation}", entry.line()));
    }
    Ok(gate::why(entry))
}

/// The current facts about a venture (when `what` names one) or a topic,
/// from a snapshot of the ledger: number, age, claim, the fact, and what
/// it rests on.
fn know_in(entries: &[Entry], ventures: &BTreeSet<String>, what: &str) -> String {
    let by_venture = ventures.contains(what) || what == SELF;
    let current = gate::current_facts(entries);
    let picked: Vec<&Entry> = current
        .iter()
        .filter(|(_, v, t)| if by_venture { v == what } else { t == what })
        .filter_map(|(s, _, _)| entries.iter().find(|e| e.seq == *s))
        .collect();
    if picked.is_empty() {
        return format!(
            "nothing is known about {what}; it is {}\n",
            if by_venture {
                "a venture with no report yet"
            } else {
                "neither a venture nor a topic any record carries"
            }
        );
    }
    let now = now();
    let mut out = format!(
        "{what}: {} current {}\n",
        picked.len(),
        if picked.len() == 1 { "fact" } else { "facts" }
    );
    for e in picked {
        let mut rests = Vec::new();
        if let Record::Report {
            source,
            confidence,
            cites,
            ..
        } = &e.record
        {
            if let Some(s) = source {
                rests.push(format!("source {s}"));
            }
            if let Some(c) = confidence {
                rests.push(format!("confidence {c}"));
            }
            if !cites.is_empty() {
                let list: Vec<String> = cites.iter().map(|c| format!("#{c}")).collect();
                rests.push(format!("leaves {}", list.join(" ")));
            }
        }
        let line = if by_venture {
            e.record.summary()
        } else {
            format!("{}  {}", e.record.venture(), e.record.summary())
        };
        out.push_str(&format!(
            "#{:<5} {:<8} {:<9} {line}{}\n",
            e.seq,
            ago(now - e.at),
            e.record.claim(),
            if rests.is_empty() {
                String::new()
            } else {
                format!("  ({})", rests.join("; "))
            }
        ));
    }
    out
}

/// A payload as the lever takes it: dropped when the lever takes none,
/// re-read into the lever's shape when the role gave another.
fn fit_payload(
    mandate: &crate::mandate::Mandate,
    action: &str,
    payload: Option<Payload>,
    name: &str,
) -> Option<Payload> {
    let takes = mandate.permitted(action).and_then(Permitted::takes)?;
    let payload = payload?;
    if payload.takes() == takes {
        return Some(payload);
    }
    match Payload::parse(takes, &payload.text()) {
        Ok(p) => Some(p),
        Err(e) => {
            log(&format!("{name}: {action}: dropped the payload: {e}"));
            None
        }
    }
}

/// Text indented by four spaces, at most `max` lines.
fn indent(text: &str, max: usize) -> String {
    let mut out = String::new();
    let lines: Vec<&str> = text.lines().collect();
    for line in lines.iter().take(max) {
        out.push_str("    ");
        out.push_str(line);
        out.push('\n');
    }
    if lines.len() > max {
        out.push_str(&format!("    ({} more lines)\n", lines.len() - max));
    }
    out
}

fn parse_seq(s: &str) -> Result<Seq, String> {
    s.trim_start_matches('#')
        .parse()
        .map_err(|_| format!("a record number looks like 12 or #12, not {s:?}"))
}

/// A duration in seconds as "3s ago", "5m ago", "2h ago", "3d ago".
pub fn ago(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s ago")
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else if s < 86_400 {
        format!("{}h ago", s / 3600)
    } else {
        format!("{}d ago", s / 86_400)
    }
}

fn log(line: &str) {
    eprintln!("avsm: {line}");
}

// ----- the warden and roles processes -----

/// What the kernel sends to a roles process.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToRoles {
    Decide(Box<Context>),
    Text(String),
    Err(String),
}

/// What a roles process sends back.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FromRoles {
    Infer(InferRequest),
    Drill(String, String),
    Records(Vec<Record>),
    Err(String),
}

/// A child process of this binary, spoken to in JSON lines.
pub struct Process {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Process {
    /// Spawn `avsm --home <home> <what>` with a clean environment. With
    /// `drop` and root privileges, the child runs as `nobody`.
    pub fn spawn(home: &Path, what: &str, drop: bool) -> Result<Process, String> {
        let exe = std::env::current_exe().map_err(|e| format!("cannot find avsm: {e}"))?;
        Process::spawn_exe(&exe, home, what, drop)
    }

    /// Spawn a given avsm binary; see `spawn`.
    pub fn spawn_exe(exe: &Path, home: &Path, what: &str, drop: bool) -> Result<Process, String> {
        let build = |drop: bool| {
            let mut cmd = Command::new(exe);
            cmd.arg("--home")
                .arg(home)
                .arg(what)
                .env_clear()
                .env(
                    "PATH",
                    std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string()),
                )
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit());
            for var in ["TZ", "LANG"] {
                if let Ok(v) = std::env::var(var) {
                    cmd.env(var, v);
                }
            }
            if drop {
                if let Some((uid, gid)) = unprivileged() {
                    cmd.uid(uid).gid(gid);
                }
            }
            cmd
        };
        let mut child = match build(drop).spawn() {
            Ok(c) => c,
            Err(e) if drop && unprivileged().is_some() => {
                log(&format!(
                    "cannot start the {what} process as nobody ({e}); running it as root instead.                      Put avsm somewhere nobody can read, like /usr/local/bin."
                ));
                build(false)
                    .spawn()
                    .map_err(|e| format!("cannot start the {what} process: {e}"))?
            }
            Err(e) => return Err(format!("cannot start the {what} process: {e}")),
        };
        let stdin = child.stdin.take().ok_or("no pipe to the child")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("no pipe from the child")?);
        Ok(Process {
            child,
            stdin,
            stdout,
        })
    }

    fn send<T: Serialize>(&mut self, msg: &T) -> Result<(), String> {
        let mut line = serde_json::to_string(msg).map_err(|e| e.to_string())?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .map_err(|e| format!("the child process is gone: {e}"))
    }

    fn recv<T: for<'de> Deserialize<'de>>(&mut self) -> Result<T, String> {
        let mut line = String::new();
        let n = self
            .stdout
            .read_line(&mut line)
            .map_err(|e| format!("the child process is gone: {e}"))?;
        if n == 0 {
            return Err("the child process is gone".to_string());
        }
        serde_json::from_str(line.trim()).map_err(|e| format!("bad message from child: {e}"))
    }

    /// The child's pid.
    pub fn id(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The uid and gid to drop to when running as root: `nobody`, else 65534.
fn unprivileged() -> Option<(u32, u32)> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return None;
    }
    let name = c"nobody";
    // SAFETY: getpwnam takes a valid C string and returns a pointer to a
    // static buffer that we only read, immediately, on this thread.
    let pw = unsafe { libc::getpwnam(name.as_ptr()) };
    if pw.is_null() {
        return Some((65534, 65534));
    }
    // SAFETY: pw points to a valid passwd record.
    let (uid, gid) = unsafe { ((*pw).pw_uid, (*pw).pw_gid) };
    Some((uid, gid))
}

/// The warden, running in a child process.
pub struct WardenProcess(Process);

impl WardenProcess {
    pub fn spawn(home: &Path) -> Result<WardenProcess, String> {
        Process::spawn(home, "warden", false).map(WardenProcess)
    }

    /// Spawn from a given avsm binary.
    pub fn spawn_exe(exe: &Path, home: &Path) -> Result<WardenProcess, String> {
        Process::spawn_exe(exe, home, "warden", false).map(WardenProcess)
    }

    pub fn id(&self) -> u32 {
        self.0.id()
    }
}

impl Warden for WardenProcess {
    fn execute(&mut self, candidate: &Candidate) -> Result<Vec<Entry>, String> {
        self.0.send(&ToWarden::Execute(candidate.clone()))?;
        match self.0.recv::<FromWarden>()? {
            FromWarden::Done(entries) => Ok(entries),
            FromWarden::Err(e) => Err(e),
            _ => Err("the warden answered the wrong question".to_string()),
        }
    }

    fn infer(&mut self, request: &InferRequest) -> Result<String, String> {
        self.0.send(&ToWarden::Infer(request.clone()))?;
        match self.0.recv::<FromWarden>()? {
            FromWarden::Text(t) => Ok(t),
            FromWarden::Err(e) => Err(e),
            _ => Err("the warden answered the wrong question".to_string()),
        }
    }

    fn measure(&mut self, venture: &str) -> Result<(Option<f64>, String), String> {
        self.0.send(&ToWarden::Measure(venture.to_string()))?;
        match self.0.recv::<FromWarden>()? {
            FromWarden::Measured(value, note) => Ok((value, note)),
            FromWarden::Err(e) => Err(e),
            _ => Err("the warden answered the wrong question".to_string()),
        }
    }
}

/// The roles, running in a child process that holds no keys.
pub struct RolesProcess(Process);

impl RolesProcess {
    pub fn spawn(home: &Path) -> Result<RolesProcess, String> {
        Process::spawn(home, "roles", true).map(RolesProcess)
    }

    /// Spawn from a given avsm binary.
    pub fn spawn_exe(exe: &Path, home: &Path) -> Result<RolesProcess, String> {
        Process::spawn_exe(exe, home, "roles", true).map(RolesProcess)
    }

    pub fn id(&self) -> u32 {
        self.0.id()
    }
}

impl Role for RolesProcess {
    fn decide(
        &mut self,
        ctx: &Context,
        infer: &mut dyn FnMut(&InferRequest) -> Result<String, String>,
        drill: &mut dyn FnMut(&str, &str) -> Result<String, String>,
    ) -> Result<Vec<Record>, String> {
        self.0.send(&ToRoles::Decide(Box::new(ctx.clone())))?;
        loop {
            match self.0.recv::<FromRoles>()? {
                FromRoles::Infer(req) => {
                    let reply = match infer(&req) {
                        Ok(t) => ToRoles::Text(t),
                        Err(e) => ToRoles::Err(e),
                    };
                    self.0.send(&reply)?;
                }
                FromRoles::Drill(what, arg) => {
                    let reply = match drill(&what, &arg) {
                        Ok(t) => ToRoles::Text(t),
                        Err(e) => ToRoles::Err(e),
                    };
                    self.0.send(&reply)?;
                }
                FromRoles::Records(r) => return Ok(r),
                FromRoles::Err(e) => return Err(e),
            }
        }
    }
}

/// The body of the warden process: answer the kernel until it hangs up.
pub fn warden_main(home: &Path) -> Result<(), String> {
    let mut warden = LocalWarden::open(home)?;
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        let reply = match serde_json::from_str::<ToWarden>(line.trim()) {
            Ok(ToWarden::Execute(c)) => match warden.execute(&c) {
                Ok(e) => FromWarden::Done(e),
                Err(e) => FromWarden::Err(e),
            },
            Ok(ToWarden::Infer(r)) => match warden.infer(&r) {
                Ok(t) => FromWarden::Text(t),
                Err(e) => FromWarden::Err(e),
            },
            Ok(ToWarden::Measure(v)) => match warden.measure(&v) {
                Ok((value, note)) => FromWarden::Measured(value, note),
                Err(e) => FromWarden::Err(e),
            },
            Err(e) => FromWarden::Err(format!("bad request: {e}")),
        };
        let mut text = serde_json::to_string(&reply).map_err(|e| e.to_string())?;
        text.push('\n');
        out.write_all(text.as_bytes())
            .and_then(|_| out.flush())
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The body of the roles process: decide when asked, with backend calls
/// routed back through the kernel to the warden. Holds no keys.
pub fn roles_main() -> Result<(), String> {
    let mut role = ModelRole;
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let out = Mutex::new(std::io::stdout());
    let say = |msg: &FromRoles| -> Result<(), String> {
        let mut text = serde_json::to_string(msg).map_err(|e| e.to_string())?;
        text.push('\n');
        let mut out = out.lock().unwrap_or_else(|p| p.into_inner());
        out.write_all(text.as_bytes())
            .and_then(|_| out.flush())
            .map_err(|e| e.to_string())
    };
    while let Some(line) = lines.next() {
        let line = line.map_err(|e| e.to_string())?;
        let ctx = match serde_json::from_str::<ToRoles>(line.trim()) {
            Ok(ToRoles::Decide(ctx)) => *ctx,
            Ok(_) => {
                say(&FromRoles::Err("expected a decision request".to_string()))?;
                continue;
            }
            Err(e) => {
                say(&FromRoles::Err(format!("bad request: {e}")))?;
                continue;
            }
        };
        let lines = std::cell::RefCell::new(&mut lines);
        let ask = |msg: &FromRoles| -> Result<String, String> {
            say(msg)?;
            let line = lines
                .borrow_mut()
                .next()
                .ok_or_else(|| "the kernel hung up".to_string())?
                .map_err(|e| e.to_string())?;
            match serde_json::from_str::<ToRoles>(line.trim()) {
                Ok(ToRoles::Text(t)) => Ok(t),
                Ok(ToRoles::Err(e)) => Err(e),
                Ok(ToRoles::Decide(_)) => Err("expected a reply, got a request".to_string()),
                Err(e) => Err(format!("bad reply: {e}")),
            }
        };
        let mut infer = |req: &InferRequest| ask(&FromRoles::Infer(req.clone()));
        let mut drill =
            |what: &str, arg: &str| ask(&FromRoles::Drill(what.to_string(), arg.to_string()));
        let reply = match role.decide(&ctx, &mut infer, &mut drill) {
            Ok(records) => FromRoles::Records(records),
            Err(e) => FromRoles::Err(e),
        };
        say(&reply)?;
    }
    Ok(())
}

// ----- start -----

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

/// Run avsm in the foreground: spawn the warden and the roles process,
/// answer commands on the socket, and tick once a second until a signal.
pub fn start(home: &Path) -> Result<(), String> {
    std::fs::create_dir_all(home).map_err(|e| format!("cannot create {}: {e}", home.display()))?;
    Config::load(home)?;
    let sock = socket_path(home);
    let listener = crate::protocol::bind(&sock)?;
    let warden = WardenProcess::spawn(home)?;
    let roles = RolesProcess::spawn(home)?;
    log(&format!(
        "warden pid {}, roles pid {}",
        warden.id(),
        roles.id()
    ));
    let mut kernel = Kernel::with(home, Box::new(roles), Box::new(warden))?;
    kernel.adopt_self()?;
    let kernel = Arc::new(Mutex::new(kernel));
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
    log(&format!("listening at {}", sock.display()));
    let server = Arc::clone(&kernel);
    std::thread::spawn(move || crate::protocol::serve(listener, server));
    let config = Config::load(home)?;
    match crate::tailscale::start(&config, Arc::clone(&kernel)) {
        Ok(Some(at)) => log(&format!(
            "listening on the tailnet at {at} for {}",
            config.remote_users.join(", ")
        )),
        Ok(None) => {}
        Err(e) => log(&e),
    }
    while !STOP.load(Ordering::SeqCst) {
        {
            let mut k = kernel.lock().unwrap_or_else(|p| p.into_inner());
            k.tick();
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    log("stopping");
    kernel.lock().unwrap_or_else(|p| p.into_inner()).shutdown();
    let _ = std::fs::remove_file(&sock);
    Ok(())
}
