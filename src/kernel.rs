//! The kernel: holds the ledger and the ventures, answers commands,
//! supervises each venture's `run`, reads each `report` on its cadence,
//! acts on declared decisions, and runs the nightly cycle through a role.
//! It never holds a key: execution and backend calls go through the warden.

use std::cell::{Ref, RefCell};
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
use crate::gate::{self, Adopted, Candidate, Reason, State, DELEGATE, SELF};
use crate::index::Index;
use crate::ledger::Ledger;
use crate::mandate::{self, Cap, Controller, Permitted, Restart};
use crate::record::{Claim, Entry, Kind, Payload, Record, Seq, Signal, Verdict};
use crate::role::{self, ChildView, Context, InferRequest, ModelRole, Role, SUMMARY};
use crate::venture::{self, Venture};
use crate::warden::{self, Failure, FromWarden, LocalWarden, ToWarden, Warden};

mod escalate;
mod expect;
mod guards;
pub mod plan;
pub mod predict;
mod refusals;

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
    /// The ledger as read so far and what the gate needs from it, brought
    /// up to date before each use with only the records appended since.
    index: RefCell<Index>,
    pub ventures: BTreeMap<String, Venture>,
    pub config: Config,
    role: Box<dyn Role + Send>,
    warden: Box<dyn Warden + Send>,
    runtime: BTreeMap<String, Runtime>,
    /// Declared decisions already acted on (or tried) in this process.
    handled: BTreeSet<Seq>,
    /// The local date each venture last had its nightly cycle.
    cycled: BTreeMap<String, NaiveDate>,
    /// The local date the digest was last written.
    digested: Option<NaiveDate>,
    /// Who is giving the command being handled, when the path it came in
    /// over knows: a Tailscale login. The socket knows nobody.
    by: Option<String>,
    /// Can a `run` be put in a network namespace of its own here? Found
    /// out once, the first time one needs it.
    namespaces: Option<bool>,
    /// Refusals written and not yet closed, with their counts.
    refusals: refusals::Tally,
    /// How each run of a lever with expectations went, once judged.
    judged: BTreeMap<Seq, expect::Run>,
    /// Set only by `simulate`, when it builds the kernel: lever runs go to
    /// the simulation world, never to the levers. No config can set it.
    sim: Option<Arc<crate::simulate::Simulation>>,
    /// Forecast at add and before a ratchet ask, as config.yaml's
    /// `forecast:` says. The daemon sets it; a simulation and a forecast's
    /// own branches never do, so a forecast never forecasts.
    pub forecasts: bool,
    /// Ventures whose metric is in a capacity episode: it drifts from the
    /// target faster than the levers can bring it back. Asked once each.
    short: BTreeSet<String>,
    /// The local date each venture's capacity was last checked nightly.
    capacity_checked: BTreeMap<String, NaiveDate>,
    /// A forecast branch ticks only the ventures it forecasts: the rest,
    /// self among them, are neither read nor cycled there.
    pub(crate) scope: Option<BTreeSet<String>>,
}

thread_local! {
    /// Set on a forecast branch's thread: its kernel logs nothing.
    static QUIET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Silence this thread's log until the guard drops.
pub(crate) struct Quiet(bool);

pub(crate) fn quiet() -> Quiet {
    Quiet(QUIET.with(|q| q.replace(true)))
}

impl Drop for Quiet {
    fn drop(&mut self) {
        QUIET.with(|q| q.set(self.0));
    }
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
    fn execute(&mut self, _candidate: &Candidate) -> Result<Vec<Entry>, Failure> {
        Err("no warden process; start avsm with: avsm start"
            .to_string()
            .into())
    }

    fn infer(&mut self, _request: &InferRequest) -> Result<String, String> {
        Err("no warden process; start avsm with: avsm start".to_string())
    }

    fn measure(&mut self, _venture: &str) -> Result<(Option<f64>, String), String> {
        Err("no warden process; start avsm with: avsm start".to_string())
    }

    fn confirm(&mut self, _intent: Seq) -> Result<Option<(bool, String)>, String> {
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
            index: RefCell::new(Index::default()),
            ventures: BTreeMap::new(),
            config,
            role,
            warden,
            runtime: BTreeMap::new(),
            handled: BTreeSet::new(),
            cycled: BTreeMap::new(),
            digested: None,
            by: None,
            namespaces: None,
            refusals: refusals::Tally::default(),
            judged: BTreeMap::new(),
            sim: None,
            forecasts: false,
            short: BTreeSet::new(),
            capacity_checked: BTreeMap::new(),
            scope: None,
        };
        kernel.reload()?;
        kernel.recover();
        Ok(kernel)
    }

    /// A forecast branch's kernel: a keyless warden that sends every lever
    /// run to the simulation, the given scripted role, and a ledger copy
    /// that is deleted after. Only a forecast builds one.
    pub(crate) fn forecasting(
        home: &Path,
        sim: Arc<crate::simulate::Simulation>,
        role: Box<dyn Role + Send>,
    ) -> Result<Kernel, String> {
        let warden = LocalWarden::keyless(home, sim.clone())?;
        let mut kernel = Kernel::with(home, role, Box::new(warden))?;
        kernel.ledger.throwaway().map_err(|e| e.to_string())?;
        kernel.sim = Some(sim);
        Ok(kernel)
    }

    /// Adopt mandate text for a venture already taken on, as declared by
    /// `by`: a forecast branch trying a proposed mandate in its own copy.
    pub(crate) fn adopt_text(&mut self, name: &str, text: &str, by: &str) -> Result<(), String> {
        let path = self
            .ventures
            .get(name)
            .and_then(|v| v.path.clone())
            .ok_or_else(|| format!("{name} has no directory"))?;
        crate::mandate::Mandate::parse(text).map_err(|e| format!("{name}: {e}"))?;
        self.append(Record::Decision {
            claim: Claim::Declared,
            venture: name.to_string(),
            verdict: Verdict::Adopt {
                mandate: crate::mandate::Mandate::hash(text),
                path: path.to_string_lossy().into_owned(),
                text: Some(text.to_string()),
            },
            by: Some(by.to_string()),
        })?;
        self.reload()
    }

    /// A kernel for a simulation: its warden sends every lever run to the
    /// simulation, and so does the kernel. Only `simulate` builds one.
    pub(crate) fn simulated(
        home: &Path,
        sim: Arc<crate::simulate::Simulation>,
    ) -> Result<Kernel, String> {
        let warden = LocalWarden::simulated(home, sim.clone())?;
        let mut kernel = Kernel::with(home, Box::new(ModelRole), Box::new(warden))?;
        kernel.sim = Some(sim);
        Ok(kernel)
    }

    /// Rebuild the ventures from the latest declared Adopt record of each.
    fn reload(&mut self) -> Result<(), String> {
        let ventures = venture::from_adoptions(self.index()?.adoptions());
        self.ventures = ventures;
        Ok(())
    }

    /// Every entry, oldest first, read afresh from the ledger.
    pub fn entries(&self) -> Result<Vec<Entry>, String> {
        self.ledger.all().map_err(|e| e.to_string())
    }

    /// The index, brought up to date with the ledger: only what was
    /// appended since it last looked is read. Drop it before appending.
    pub fn index(&self) -> Result<Ref<'_, Index>, String> {
        self.index
            .borrow_mut()
            .refresh(&self.ledger)
            .map_err(|e| e.to_string())?;
        Ok(self.index.borrow())
    }

    /// Asks with no declared decision yet, oldest first.
    pub fn open_asks(&self) -> Result<Vec<Seq>, String> {
        Ok(self.index()?.open_asks())
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
            ("digest", []) => self.digest(),
            ("forecast", rest) => {
                let opts = crate::forecast::Options::parse(rest, self.config.forecast)?;
                let (outcome, written) = self.forecast(&opts, &[])?;
                let mut out = outcome.render();
                for e in &written {
                    out.push_str(&format!("  {}\n", e.line()));
                }
                Ok(out)
            }
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
        let (mandate, hash, text) = venture::read_mandate(&dir)?;
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
                text: Some(text),
            },
        ))?;
        let v = Venture {
            name: name.clone(),
            path: Some(dir.clone()),
            mandate: mandate.clone(),
            hash,
            adopted: entry.seq,
            problem: None,
            note: None,
        };
        self.ventures.insert(name.clone(), v);
        // A fresh mandate means a fresh run and a report right away.
        if let Some(mut rt) = self.runtime.remove(&name) {
            kill(&mut rt);
        }
        let mut out = format!("added {name} from {}\n", dir.display());
        out.push_str(&crate::teach::render(&mandate));
        // What the mandate can keep up with, and where it leads, before
        // anything runs under it. They follow the adoption in the ledger;
        // the adoption stays the last line shown, where scripts read it.
        if let Some(e) = self.check_capacity(&name) {
            out.push_str(&format!("  {}\n", e.line()));
        }
        if self.forecasts && self.config.forecast.seeds > 0 {
            let opts = crate::forecast::Options::new(vec![name.clone()], self.config.forecast);
            match self.forecast(&opts, &[entry.seq]) {
                Ok((_, written)) => {
                    for e in &written {
                        out.push_str(&format!("  {}\n", e.line()));
                    }
                }
                Err(e) => out.push_str(&format!("  no forecast: {e}\n")),
            }
        }
        out.push_str(&format!("  {}\n", entry.line()));
        Ok(out)
    }

    /// Forecast as `opts` says, and write what it found into the ledger:
    /// one inferred forecast record per venture, variant and policy, each
    /// citing `cites`, and a measured ask for each venture whose branches
    /// reached a wall. Nothing else of the branches comes back.
    pub fn forecast(
        &mut self,
        opts: &crate::forecast::Options,
        cites: &[Seq],
    ) -> Result<(crate::forecast::Outcome, Vec<Entry>), String> {
        let outcome = crate::forecast::run(&self.ledger, &self.home, opts)?;
        let mut written = Vec::new();
        let mut seqs: BTreeMap<String, Vec<Seq>> = BTreeMap::new();
        for record in outcome.records(cites) {
            let e = self.append(record)?;
            seqs.entry(e.record.venture().to_string())
                .or_default()
                .push(e.seq);
            written.push(e);
        }
        let asks = {
            let index = self.index()?;
            crate::forecast::asks(&index, &outcome, &seqs)
        };
        for record in asks {
            let e = self.append(record)?;
            log(&e.line());
            written.push(e);
        }
        Ok((outcome, written))
    }

    /// Check, in code, that the permitted levers can keep the venture's
    /// metric from drifting off the target: its drift a day against the
    /// most the levers can move it a day. When they cannot, ask once per
    /// episode, before any saturation; the episode ends at a check that
    /// finds them able. Returns the ask written, if one was.
    pub fn check_capacity(&mut self, name: &str) -> Option<Entry> {
        let v = self.ventures.get(name)?.clone();
        let gap = {
            let index = self.index().ok()?;
            crate::forecast::capacity(&index, name, &v.mandate)
        };
        let Some(gap) = gap else {
            self.short.remove(name);
            return None;
        };
        if !self.short.insert(name.to_string()) {
            return None;
        }
        let before = self.ledger_last();
        self.signal(
            name,
            format!("{}; the target is out of reach", gap.describe()),
            Signal::Capacity {
                metric: gap.metric.clone(),
                drift: gap.drift,
                most: gap.most,
            },
        );
        let index = self.index().ok()?;
        let found = index
            .of(name)
            .rev()
            .take_while(|e| e.seq > before)
            .find(|e| {
                matches!(
                    &e.record,
                    Record::Ask {
                        signal: Some(Signal::Capacity { .. }),
                        ..
                    }
                )
            })
            .cloned();
        found
    }

    /// Refuse a mandate whose parent and children do not agree with the
    /// mandates already adopted, or that would make a loop.
    fn check_family(&self, name: &str, mandate: &crate::mandate::Mandate) -> Result<(), String> {
        if mandate.child(name).is_some() {
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
                if parent.mandate.child(name).is_none() {
                    return Err(format!(
                        "{name} names {p} as its parent, but {p} does not list {name} under children; add it there and run avsm add on {p} first"
                    ));
                }
            }
        }
        for parent in adopted.iter().filter(|v| v.mandate.child(name).is_some()) {
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
            if mandate.child(&child.name).is_none() {
                return Err(format!(
                    "{} names {name} as its parent, but {name} does not list {} under children; remove parent from {} first",
                    child.name, child.name, child.name
                ));
            }
        }
        for c in mandate.children.iter().map(|c| &c.name) {
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
            for c in m.children.iter().map(|c| &c.name) {
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
        let index = self.index()?;
        let now = now();
        let adopted = self.adopted();
        let states: Vec<(&str, gate::State)> = self
            .ventures
            .values()
            .map(|v| (v.name.as_str(), gate::state_in(&index, &adopted, &v.name)))
            .collect();
        let mut out = caution(&states);
        for (v, (_, state)) in self.ventures.values().zip(&states) {
            let last = index
                .of(&v.name)
                .rev()
                .find(|e| matches!(e.record, Record::Report { .. }));
            let mut detail = match &v.problem {
                Some(p) => p.clone(),
                None => match last {
                    Some(e) => format!("{}  reported {}", e.record.summary(), ago(now - e.at)),
                    None => "no report yet".to_string(),
                },
            };
            if let Some(note) = &v.note {
                detail.push_str(&format!("  [{note}]"));
            }
            if let Some(note) = self.runtime.get(&v.name).and_then(|rt| rt.note.as_deref()) {
                detail.push_str(&format!("  ({note})"));
            }
            out.push_str(&format!("{:<12} {:<11} {detail}\n", v.name, state));
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
        let index = self.index()?;
        let open = index.open_asks();
        if open.is_empty() {
            return Ok("no open asks\n".to_string());
        }
        let mut out = String::new();
        for e in open.iter().filter_map(|q| index.get(*q)) {
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
        if !self.index()?.is_open(seq) {
            return Err(format!("#{seq} is not an open ask\n{}", entry.line()));
        }
        Ok(entry)
    }

    /// Say yes to an ask, with the payload as the human amended it when
    /// they did. The next tick acts on it.
    pub fn approve(&mut self, seq: Seq, amended: Option<&str>) -> Result<String, String> {
        let ask = self.open_ask(seq)?;
        let lever_takes = ask.record.action().and_then(|a| {
            self.ventures
                .get(ask.record.venture())
                .and_then(|v| v.mandate.permitted(a))
                .and_then(|p| p.takes().map(|t| (p.name.clone(), t)))
        });
        // A lever that takes a payload cannot run on an approval that has
        // none: refuse now, record nothing.
        if let (None, None, Some((name, _))) = (amended, ask.record.payload(), &lever_takes) {
            return Err(format!(
                "#{seq}: {name} takes a payload and the ask carries none; approve with --payload"
            ));
        }
        let payload = match amended {
            None => None,
            Some(text) => {
                let takes = ask
                    .record
                    .payload()
                    .map(Payload::takes)
                    .or(lever_takes.map(|(_, t)| t))
                    .ok_or_else(|| format!("#{seq} carries no payload to amend"))?;
                let parsed = Payload::parse(takes, text)?;
                if ask.record.action() == Some(DELEGATE) {
                    // A mandate is checked exactly as `add` would, before
                    // any decision is recorded.
                    let m = crate::mandate::Mandate::parse(text).map_err(|e| e.0)?;
                    self.check_family(ask.record.venture(), &m)?;
                    if m.metric_adapter().is_none() {
                        let report = self
                            .ventures
                            .get(ask.record.venture())
                            .and_then(|v| v.path.as_ref())
                            .is_some_and(|d| d.join("report").is_file());
                        if !report {
                            return Err(format!(
                                "{} watches {} but holds no report program to read it",
                                ask.record.venture(),
                                m.metric_name()
                            ));
                        }
                    }
                }
                Some(parsed)
            }
        };
        let entry = self
            .append(self.decision(ask.record.venture(), Verdict::Approve { ask: seq, payload }))?;
        let mut out = entry.line() + "\n";
        if let Some(intent) = pending_asked(&ask) {
            out.push_str(&self.settle(&ask, intent, true, "a human says it happened")?);
        }
        Ok(out)
    }

    /// The declared outcome a human's answer to a pending ask makes.
    fn settle(&mut self, ask: &Entry, intent: Seq, ok: bool, what: &str) -> Result<String, String> {
        let detail = match &self.by {
            Some(who) => format!("{what} ({who}, #{})", ask.seq),
            None => format!("{what} (#{})", ask.seq),
        };
        let entry = self.append(Record::Outcome {
            claim: Claim::Declared,
            venture: ask.record.venture().to_string(),
            intent,
            ok,
            detail,
        })?;
        Ok(entry.line() + "\n")
    }

    /// Settle every intent with no outcome: a run that may or may not
    /// have happened is never run again blindly. The warden asks the
    /// lever's adapter where one can check by the run's key, and the
    /// answer is a measured outcome; otherwise a human is asked, once,
    /// and the answer becomes a declared outcome. Until then the gate
    /// refuses the lever. When the warden cannot look now, it is left
    /// for later. The ask goes past the asks quota: a lever stuck with
    /// nobody told is the failure this removes. One open ask per intent.
    pub fn recover(&mut self) {
        let pending: Vec<(Seq, String, String)> = {
            let Ok(index) = self.index() else { return };
            let asked: BTreeSet<Seq> = index
                .open_signals()
                .into_iter()
                .filter_map(|(e, _)| pending_asked(e))
                .collect();
            index
                .pending()
                .filter(|(s, _, _)| !asked.contains(s))
                .map(|(s, v, a)| (s, v.to_string(), a.to_string()))
                .collect()
        };
        for (intent, venture, action) in pending {
            let record = match self.warden.confirm(intent) {
                Err(e) => {
                    log(&format!(
                        "{venture}: intent #{intent} has no outcome and cannot be checked now: {e}"
                    ));
                    continue;
                }
                Ok(Some((ok, found))) => Record::Outcome {
                    claim: Claim::Measured,
                    venture: venture.clone(),
                    intent,
                    ok,
                    detail: format!("checked after a restart: {found}"),
                },
                Ok(None) => Record::Ask { cites: vec![],
                        claim: Claim::Measured,
                        venture: venture.clone(),
                        action: None,
                        question: format!(
                            "did {action} happen? intent #{intent} has no outcome; approve if it did, deny if it did not"
                        ),
                        refusal: Some(gate::Refusal {
                            venture: venture.clone(),
                            action: action.clone(),
                            steps: vec![],
                            reasons: vec![Reason::Pending { intent }],
                        }),
                        payload: None,
                        signal: Some(Signal::Pending {
                            intent,
                            action: action.clone(),
                        }),
                    },
            };
            match self.append(record) {
                Ok(e) => log(&e.line()),
                Err(e) => log(&format!("{venture}: cannot write record: {e}")),
            }
        }
    }

    /// Say no to an ask.
    pub fn deny(&mut self, seq: Seq, reason: &str) -> Result<String, String> {
        let ask = self.open_ask(seq)?;
        let reason = if reason.trim().is_empty() {
            "no reason given".to_string()
        } else {
            reason.trim().to_string()
        };
        let entry = self.append(self.decision(
            ask.record.venture(),
            Verdict::Deny {
                ask: seq,
                reason: reason.clone(),
            },
        ))?;
        let mut out = entry.line() + "\n";
        if let Some(intent) = pending_asked(&ask) {
            let what = format!("a human says it did not happen: {reason}");
            out.push_str(&self.settle(&ask, intent, false, &what)?);
        }
        Ok(out)
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
            cites: vec![],
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
        let index = self.index()?;
        why_in(&index, &self.adopted(), &self.config.quotas, seq)
    }

    /// What is held true now about a venture, or about a topic across
    /// ventures: each current fact with its age, claim and leaves.
    pub fn know(&self, what: &str) -> Result<String, String> {
        let index = self.index()?;
        let ventures: BTreeSet<String> = self.ventures.keys().cloned().collect();
        Ok(know_in(&index, &ventures, what, false))
    }

    // ----- the loop -----

    /// One pass: supervise every run, read reports that are due, act on
    /// declared decisions, and run the nightly cycle when its time comes.
    /// Problems are logged and never stop the pass.
    pub fn tick(&mut self) {
        self.recover();
        let names: Vec<String> = self
            .ventures
            .keys()
            .filter(|n| self.scope.as_ref().is_none_or(|s| s.contains(*n)))
            .cloned()
            .collect();
        let adopted = self.adopted();
        // Every state as the ledger stood when the pass began.
        let (halted, paused) = {
            let index = match self.index() {
                Ok(i) => i,
                Err(e) => {
                    log(&format!("cannot read the ledger: {e}"));
                    return;
                }
            };
            let halted = gate::state_in(&index, &adopted, SELF) == State::Paused;
            let paused: Vec<bool> = names
                .iter()
                .map(|name| halted || gate::state_in(&index, &adopted, name) == State::Paused)
                .collect();
            (halted, paused)
        };
        let now = now();
        self.close_stale_refusals();
        for (name, paused) in names.iter().zip(paused) {
            self.supervise(name, paused);
            let due = self.runtime.entry(name.clone()).or_default().next_report <= now;
            if due && self.ventures[name].problem.is_none() {
                self.report(name, halted);
            }
        }
        if !halted {
            self.act_on_decisions();
            for name in &names {
                if self.capacity_due(name) {
                    self.check_capacity(name);
                }
            }
            self.ratchet();
            for name in &names {
                if self.nightly_due(name) {
                    self.cycle(name);
                }
            }
            // The digest is self's, and a forecast branch leaves self out.
            if self.scope.is_none() && self.digest_due() {
                match self.record_digest() {
                    Ok(e) => log(&e.line()),
                    Err(e) => log(&format!("cannot write the digest: {e}")),
                }
            }
        }
    }

    /// Self watches decisions: when a lever that asks each time has been
    /// approved `ratchet` times with the same shape of payload since the
    /// last word on delegating it, self proposes the mandate change that
    /// delegates it, with the approvals as evidence. The proposal goes
    /// through the gate like any other, and becomes an ask.
    fn ratchet(&mut self) {
        let n = self.config.ratchet as usize;
        if n == 0 {
            return;
        }
        let ventures: Vec<Venture> = self
            .ventures
            .values()
            .filter(|v| v.name != SELF && v.problem.is_none() && v.path.is_some())
            .cloned()
            .collect();
        for v in ventures {
            let asked_already = {
                let Ok(index) = self.index() else { return };
                let asked = index.open_of(&v.name).any(|q| {
                    index
                        .get(q)
                        .is_some_and(|e| e.record.action() == Some(DELEGATE))
                });
                asked
            };
            if asked_already {
                continue;
            }
            for perm in &v.mandate.permitted {
                // An irreversible lever is never proposed for delegation,
                // however often it was approved; only a human writes that.
                let asks_each_time = !perm.delegated && perm.reversible && perm.takes().is_some();
                if !asks_each_time || (perm.is_parent_lever() && perm.name == "resume") {
                    continue;
                }
                let found = {
                    let Ok(index) = self.index() else { return };
                    ripe(&index, &v.name, &perm.name, n)
                };
                let Some((shape, seqs)) = found else {
                    continue;
                };
                let Some(dir) = v.path.as_ref() else { continue };
                let text = match std::fs::read_to_string(dir.join(venture::MANDATE_FILE)) {
                    Ok(t) => t,
                    Err(e) => {
                        log(&format!("{}: cannot read the mandate: {e}", v.name));
                        continue;
                    }
                };
                let changed = match mandate::delegate_text(&text, &perm.name) {
                    Ok(t) => t,
                    Err(e) => {
                        log(&format!("{}: cannot delegate {}: {e}", v.name, perm.name));
                        continue;
                    }
                };
                let list: Vec<String> = seqs.iter().map(|s| format!("#{s}")).collect();
                let reason = format!(
                    "{} was approved {} times with {} ({}); delegating it ends the asks",
                    perm.name,
                    seqs.len(),
                    if shape == "none" {
                        "no payload".to_string()
                    } else {
                        format!("a {shape} payload")
                    },
                    list.join(", ")
                );
                let proposal = match self.append(Record::Proposal {
                    cites: vec![],
                    claim: Claim::Measured,
                    venture: v.name.clone(),
                    action: DELEGATE.to_string(),
                    reason,
                    payload: Some(Payload::Text {
                        text: changed.clone(),
                    }),
                }) {
                    Ok(e) => e,
                    Err(e) => {
                        log(&format!("{}: cannot write the proposal: {e}", v.name));
                        continue;
                    }
                };
                log(&proposal.line());
                // The forecast runs on the proposed mandate before the ask
                // is written, and the ask cites it.
                let mut forecast = vec![];
                if self.forecasts && self.config.forecast.seeds > 0 {
                    let mut opts =
                        crate::forecast::Options::new(vec![v.name.clone()], self.config.forecast);
                    opts.mandates.insert(v.name.clone(), changed.clone());
                    match self.forecast(&opts, &[proposal.seq]) {
                        Ok((_, written)) => {
                            forecast = written
                                .iter()
                                .filter(|e| e.record.kind() == Kind::Forecast)
                                .map(|e| e.seq)
                                .collect();
                        }
                        Err(e) => log(&format!("{}: no forecast of the delegation: {e}", v.name)),
                    }
                }
                self.act_citing(
                    &v.name,
                    DELEGATE,
                    vec![proposal.seq],
                    Claim::Measured,
                    forecast,
                );
                break;
            }
        }
    }

    /// Is the nightly capacity check due for a venture: past the nightly
    /// time, with none today? Marks it done.
    fn capacity_due(&mut self, name: &str) -> bool {
        if name == SELF {
            return false;
        }
        let Ok(Some(at)) = self.config.nightly_minutes() else {
            return false;
        };
        let local = crate::clock::local();
        if local.hour() * 60 + local.minute() < at {
            return false;
        }
        let today = local.date_naive();
        self.capacity_checked.insert(name.to_string(), today) != Some(today)
    }

    /// Is the nightly digest due: past the nightly time, with none written
    /// today?
    fn digest_due(&self) -> bool {
        let Ok(Some(at)) = self.config.nightly_minutes() else {
            return false;
        };
        let local = crate::clock::local();
        if local.hour() * 60 + local.minute() < at {
            return false;
        }
        let today = local.date_naive();
        if self.digested == Some(today) {
            return false;
        }
        let Ok(index) = self.index() else {
            return false;
        };
        !written_on(index.digests_at(), today)
    }

    /// What happened since the last digest, or over the last day: one
    /// line per venture with its metric and how it moved, the actions,
    /// the asks, the cost; and self's asks and incidents a week.
    pub fn digest(&self) -> Result<String, String> {
        let index = self.index()?;
        Ok(digest_text(&index, &self.ventures, now()))
    }

    /// Write the digest as a record of self, with self's weekly counts.
    pub fn record_digest(&mut self) -> Result<Entry, String> {
        let now = now();
        let (text, (asks, incidents)) = {
            let index = self.index()?;
            (
                digest_text(&index, &self.ventures, now),
                weekly(index.entries(), now),
            )
        };
        self.digested = Some(crate::clock::local().date_naive());
        let entry = self.append(Record::report(Claim::Measured, SELF, "digest", None, &text))?;
        self.append(Record::report(
            Claim::Measured,
            SELF,
            "asks_per_week",
            Some(f64::from(asks)),
            "",
        ))?;
        self.append(Record::report(
            Claim::Measured,
            SELF,
            "incidents_per_week",
            Some(f64::from(incidents)),
            "failed actions, broken chains and stops",
        ))?;
        Ok(entry)
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
        // A simulation never starts a venture's `run`: what it does is the
        // real world's, and a simulation only ever touches the world model.
        if self.sim.is_some() {
            if !rt.dead && v.run().is_some() {
                log(&format!("{name}: run is not started in a simulation"));
            }
            rt.dead = true;
            return;
        }
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
        // A reading code does not believe is kept, marked, and never a
        // premise; the first of an episode asks.
        let (implausible, fresh) = self.doubt(&v, value);
        let metric = v.mandate.metric_name().to_string();
        let entry = match self.append(Record::Report {
            claim: Claim::Measured,
            venture: name.to_string(),
            metric: metric.clone(),
            value: value.filter(|x| x.is_finite()),
            note,
            source: None,
            confidence: None,
            cites: vec![],
            implausible: implausible.clone(),
            facts: None,
        }) {
            Ok(e) => e,
            Err(e) => {
                log(&format!("{name}: cannot write report: {e}"));
                return;
            }
        };
        let readings = self.read_guards(&v);
        if halted {
            return;
        }
        if let (Some(why), true) = (&implausible, fresh) {
            self.signal(
                name,
                format!("{metric} reported something implausible: {why}"),
                Signal::Implausible {
                    metric: metric.clone(),
                    why: why.clone(),
                },
            );
        }
        self.enforce_guards(name, readings);
        self.check_audits(&v);
        // Forecasts this report resolves are scored, by code, before
        // anything acts on it.
        let forecast = name != SELF && predict::predicts(&v.mandate);
        if forecast {
            self.resolve(name);
        }
        if value.is_none() {
            return;
        }
        // What the lever runs did to the metric, judged on plausible
        // reports only.
        self.judge(name);
        if implausible.is_some() {
            return;
        }
        let plans = name != SELF && matches!(v.mandate.controller, Controller::Plan { .. });
        if name != SELF && (plans || v.mandate.early_warning) {
            self.early_warning(name, &metric);
        }
        // The bound is the mandate's, or the one the parent set.
        let threshold = match self.index() {
            Ok(index) => gate::effective_threshold_in(&index, &self.adopted(), name),
            Err(_) => return,
        };
        let mut ran = None;
        if let Some(t) = threshold {
            if self.threshold_fires(&v, &t) {
                self.forget_refusal();
                let before = self.ledger_last();
                self.act(name, &t.action, vec![entry.seq], Claim::Measured);
                if self.intent_since(name, &t.action, before) {
                    ran = Some(t.action.clone());
                }
                self.saturate(name, &t.action, &metric, t.bound);
            }
        }
        if plans {
            ran = self.plan(name).or(ran);
        }
        // One step ahead, with the lever that just ran, if any.
        if forecast {
            self.predict(name, ran.as_deref());
        }
    }

    /// The number of the last record in the ledger, 0 when empty.
    fn ledger_last(&self) -> Seq {
        self.index().map_or(0, |i| i.last())
    }

    /// Did an intent of this lever go into the ledger after `after`?
    fn intent_since(&self, venture: &str, action: &str, after: Seq) -> bool {
        self.index().is_ok_and(|i| {
            i.intents_of(venture, action)
                .last()
                .is_some_and(|s| *s > after)
        })
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
        self.recover_refusals();
        Ok(())
    }

    /// Read a venture's metric: through the warden when it names an
    /// adapter, else by running its `report` and taking the last line.
    /// The built-in self venture measures itself.
    fn measure(&mut self, v: &Venture) -> Option<(Option<f64>, String)> {
        if v.name == SELF && v.path.is_none() {
            return Some(crate::selfventure::measure(&self.ledger));
        }
        if let (Some(sim), Some(dir)) = (self.sim.clone(), v.path.as_deref()) {
            let simulated = {
                let index = self.index().ok()?;
                sim.measure(&index, &v.name, &v.mandate, dir)
            };
            if simulated.is_some() {
                return simulated;
            }
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
        // NaN and infinity parse but mean nothing; they are passed on so
        // the report says so, and are never stored as a value.
        let not_finite = ran
            .stdout
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .and_then(|l| l.trim().parse::<f64>().ok())
            .filter(|n| !n.is_finite());
        match crate::adapter::number_from(&ran.stdout) {
            Ok(n) => Some((Some(n), String::new())),
            Err(_) if not_finite.is_some() => Some((
                not_finite,
                format!("report printed {}", not_finite.unwrap_or(f64::NAN)),
            )),
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
        self.act_citing(name, action, cites, claim, vec![])
    }

    /// `act`, and an ask it writes cites `ask_cites`: the forecast of a
    /// delegation, say. They are never premises for the gate.
    fn act_citing(
        &mut self,
        name: &str,
        action: &str,
        cites: Vec<Seq>,
        claim: Claim,
        ask_cites: Vec<Seq>,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        let candidate = Candidate {
            venture: name.to_string(),
            action: action.to_string(),
            cites,
        };
        let adopted = self.adopted();
        // The shared quotas come first: past one, the gate is not asked.
        let over = match self.index() {
            Ok(index) => {
                gate::quota_check_in(&index, &adopted, &self.config.quotas, name, action, now()).1
            }
            Err(e) => {
                lines.push(format!("{name}: cannot read the ledger: {e}"));
                log(&lines[0]);
                return lines;
            }
        };
        if !over.is_empty() {
            let (key, detail, limit) = refusals::describe(&over, &[]);
            lines.push(format!(
                "{name}: {action} refused before the gate: {detail}"
            ));
            log(&lines[0]);
            self.refuse(name, action, key, detail, limit);
            return lines;
        }
        // What the gate says, and what an ask would carry, as the ledger
        // stands now.
        let judged = {
            let index = match self.index() {
                Ok(i) => i,
                Err(e) => {
                    lines.push(format!("{name}: cannot read the ledger: {e}"));
                    log(&lines[0]);
                    return lines;
                }
            };
            match gate::gate_in(&index, &candidate, &adopted, &self.config.quotas, now()) {
                Ok(proof) => Ok(proof),
                Err(refusal) => {
                    let already = index.open_of(name).any(|q| {
                        index
                            .get(q)
                            .is_some_and(|e| e.record.action() == Some(action))
                    });
                    let may_ask = self.may_ask(&index);
                    let cited: Vec<&Entry> = candidate
                        .cites
                        .iter()
                        .filter_map(|s| index.get(*s))
                        .collect();
                    // The ask carries the payload the lever would consume,
                    // from whichever cited record brought it.
                    let payload = cited.iter().find_map(|e| e.record.payload().cloned());
                    // The question carries the proposal's reason, so the
                    // human sees why without a drill.
                    let reason = cited
                        .iter()
                        .find_map(|e| match &e.record {
                            Record::Proposal { reason, .. } => Some(format!("  {reason}")),
                            _ => None,
                        })
                        .unwrap_or_default();
                    Err((refusal, already, may_ask, payload, reason))
                }
            }
        };
        match judged {
            Ok(_) => match self.warden.execute(&candidate) {
                Ok(written) => {
                    lines.extend(written.iter().map(Entry::line));
                    self.judge(name);
                    // A changed mandate adopted: take it on afresh, as
                    // `avsm add` would.
                    let adopted = written.iter().any(|e| {
                        matches!(
                            &e.record,
                            Record::Decision {
                                verdict: Verdict::Adopt { .. },
                                ..
                            }
                        )
                    });
                    if adopted {
                        if let Err(e) = self.reload() {
                            lines.push(format!("{name}: cannot reload: {e}"));
                        }
                        if let Some(mut rt) = self.runtime.remove(name) {
                            kill(&mut rt);
                        }
                    }
                }
                Err(Failure::Refused { reason_key, detail }) => {
                    // The check said no after the gate said yes: a refusal
                    // record, no intent, nothing run.
                    lines.push(format!("{name}: {action} refused: {detail}"));
                    log(lines.last().map_or("", String::as_str));
                    self.refuse(name, action, reason_key, detail, false);
                }
                Err(Failure::Error(e)) => {
                    lines.push(format!("{name}: {action} did not run: {e}"));
                    // The gate said yes and nothing ran: the ledger says so.
                    let first: String = e.lines().next().unwrap_or("").chars().take(200).collect();
                    let failed = Record::Report {
                        claim: Claim::Measured,
                        venture: name.to_string(),
                        metric: action.to_string(),
                        value: None,
                        note: format!("did not run: {first}"),
                        source: None,
                        confidence: None,
                        cites: candidate.cites.clone(),
                        implausible: None,
                        facts: None,
                    };
                    if let Err(e) = self.append(failed) {
                        lines.push(format!("{name}: cannot write record: {e}"));
                    }
                }
            },
            Err((refusal, already, may_ask, payload, reason)) => {
                // A paused or guarded venture is not asked about: an
                // approval could not run until a human resumes it, or the
                // guard is back inside, anyway.
                let paused = refusal
                    .reasons
                    .iter()
                    .any(|r| matches!(r, Reason::Paused { .. } | Reason::Guarded { .. }));
                let needs_human = !paused
                    && refusal
                        .reasons
                        .iter()
                        .any(|r| matches!(r, Reason::NeedsDecision { .. }));
                if needs_human && !already && !may_ask {
                    lines.push(format!(
                        "{name}: {action} needs a decision, but the asks quota is used up; not asking"
                    ));
                    let (key, detail, limit) =
                        refusals::describe_gate(&refusal.reasons, false, true);
                    self.refuse(name, action, key, detail, limit);
                } else if needs_human && !already {
                    let question = match &payload {
                        Some(_) => format!("may I {action} with this?{reason}"),
                        None if refusal.reasons.iter().any(|r| {
                            matches!(
                                r,
                                Reason::NeedsDecision {
                                    suspended: true,
                                    ..
                                }
                            )
                        }) =>
                        {
                            format!("may I {action}? it was found ineffective{reason}")
                        }
                        None => format!("may I {action}? it is irreversible{reason}"),
                    };
                    match self.append(Record::Ask {
                        cites: ask_cites,
                        claim,
                        venture: name.to_string(),
                        action: Some(action.to_string()),
                        question,
                        refusal: Some(refusal),
                        payload,
                        signal: None,
                    }) {
                        Ok(e) => lines.push(e.line()),
                        Err(e) => lines.push(format!("{name}: cannot write ask: {e}")),
                    }
                } else if needs_human {
                    lines.push(format!("{name}: {action} is already asked; see: avsm asks"));
                    let (key, detail, limit) =
                        refusals::describe_gate(&refusal.reasons, true, false);
                    self.refuse(name, action, key, detail, limit);
                } else {
                    let (key, detail, limit) = refusals::describe(&refusal.reasons, &[]);
                    lines.push(format!("{name}: {action} refused: {detail}"));
                    self.refuse(name, action, key, detail, limit);
                }
            }
        }
        for line in &lines {
            log(line);
        }
        lines
    }

    /// Is there room under the asks quota for one more ask?
    fn may_ask(&self, index: &Index) -> bool {
        match self.config.quotas.asks {
            Some(cap) => gate::asks_used_in(index, cap, now()) < cap.count,
            None => true,
        }
    }

    /// Act on declared approvals not yet acted on.
    fn act_on_decisions(&mut self) {
        let pending: Vec<(Seq, String, String)> = {
            let Ok(index) = self.index() else { return };
            index
                .unacted()
                .filter(|d| !self.handled.contains(d))
                .filter_map(|d| match &index.get(d)?.record {
                    Record::Decision {
                        venture,
                        verdict: Verdict::Approve { ask, .. },
                        ..
                    } => {
                        let action = index.get(*ask)?.record.action()?.to_string();
                        Some((d, venture.clone(), action))
                    }
                    _ => None,
                })
                // A lever whose last run has no outcome waits: its approval
                // is not spent, and runs on the tick after it is settled.
                .filter(|(_, venture, action)| index.pending_of(venture, action).is_none())
                .collect()
        };
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
        if v.problem.is_some()
            || matches!(
                v.mandate.controller,
                Controller::Threshold { .. } | Controller::Plan { .. }
            )
        {
            return false;
        }
        let Ok(Some(at)) = self.config.nightly_minutes() else {
            return false;
        };
        let local = crate::clock::local();
        if local.hour() * 60 + local.minute() < at {
            return false;
        }
        let today = local.date_naive();
        if self.cycled.get(name) == Some(&today) {
            return false;
        }
        // After a restart, an inferred record from today means it ran.
        let Ok(index) = self.index() else {
            return false;
        };
        !written_on(index.inferred_at(name), today)
    }

    /// Run the nightly cycle for a venture now: show the role the mandate
    /// and recent records, write what it says as inferred records, and try
    /// each proposal through the gate.
    pub fn cycle(&mut self, name: &str) {
        self.cycled
            .insert(name.to_string(), crate::clock::local().date_naive());
        let Some(v) = self.ventures.get(name).cloned() else {
            return;
        };
        let wanted = match &v.mandate.controller {
            Controller::Classifier { backend } | Controller::Model { backend } => backend.clone(),
            Controller::Threshold { .. } | Controller::Plan { .. } => return,
        };
        let index = match self.index() {
            Ok(i) => i,
            Err(e) => {
                log(&format!("{name}: cannot read the ledger: {e}"));
                return;
            }
        };
        let backend = match self.config.backend(wanted.as_deref()) {
            Ok((b, _)) => b,
            Err(e) => {
                drop(index);
                self.ask_once(name, &format!("{name} cannot run its nightly cycle: {e}"));
                return;
            }
        };
        // Audit readings never reach a model: not as records, not in a
        // summary, not as asks.
        let mine: Vec<Entry> = index
            .of(name)
            .filter(|e| !role::hidden(e))
            .cloned()
            .collect();
        // Context by budget: the mandate, the latest summary, the records
        // it does not stand for, and the open asks. Past the venture's
        // bound, the older records go to the role to be summarized first;
        // the previous summary goes with them so the new one carries it.
        let is_summary = role::is_summary;
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
        let asks: Vec<Entry> = mine
            .iter()
            .filter(|e| index.is_open(e.seq))
            .cloned()
            .collect();
        let uncovered: Vec<Entry> = mine
            .iter()
            .filter(|e| e.seq > covered && !is_summary(e) && !index.is_open(e.seq))
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
            .map(|c| &c.name)
            .filter_map(|c| {
                let child = self.ventures.get(c)?;
                if child.mandate.parent.as_deref() != Some(name) {
                    return None;
                }
                let theirs: Vec<&Entry> = index
                    .of(c)
                    .filter(|e| e.record.claim().authorizes() && !role::hidden(e))
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
                    state: gate::state_in(&index, &self.adopted(), c).to_string(),
                    metric,
                    lines,
                })
            })
            .collect();
        let ctx = Context {
            venture: name.to_string(),
            mandate: v.mandate.clone(),
            entries: role::trim_to_chars(&recent, limit),
            state: gate::state_in(&index, &self.adopted(), name).to_string(),
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
        drop(index);
        let records = {
            let Kernel {
                role,
                warden,
                ledger,
                index,
                ..
            } = self;
            // A drill reads the ledger as it stood when the cycle began,
            // and is itself a record: the index is not brought up to date
            // until the cycle is over.
            let mut drill = |what: &str, arg: &str| -> Result<String, String> {
                let index = index.borrow();
                let text = match what {
                    "know" => know_in(&index, &ventures, arg, true),
                    "why" => {
                        let seq = parse_seq(arg)?;
                        if index.get(seq).is_some_and(role::hidden) {
                            return Err(format!("record #{seq} is not shown to a model"));
                        }
                        why_in(&index, &adopted, &quotas, seq)?
                    }
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
                        implausible: None,
                        facts: None,
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
                let first: String = e.lines().next().unwrap_or("").chars().take(200).collect();
                let failed = Record::Report {
                    claim: Claim::Measured,
                    venture: name.to_string(),
                    metric: "cycle".to_string(),
                    value: None,
                    note: format!("the nightly cycle failed: {first}"),
                    source: None,
                    confidence: None,
                    cites: vec![],
                    implausible: None,
                    facts: None,
                };
                match self.append(failed) {
                    Ok(entry) => log(&entry.line()),
                    Err(e) => log(&format!("{name}: cannot write record: {e}")),
                }
                // A reply refused for size is one the primary could not
                // give; the escalation backend, if any, may answer.
                if warden::refused_for_size(&e) {
                    if let Err(e) = self.escalate(name, &first, &role::prompt(&ctx)) {
                        log(&format!("{name}: cannot escalate: {e}"));
                    }
                }
                return;
            }
        };
        // A reply that could not be used is the other trigger.
        // A reply with several choices of which some could be used already
        // has its answer: only a reply with nothing usable escalates.
        let chose = records.iter().any(|r| match r {
            Record::Proposal { .. } | Record::Ask { .. } => true,
            Record::Report { metric, .. } => metric == role::NOTHING,
            _ => false,
        });
        let unusable: Option<String> = records.iter().find_map(|r| match r {
            Record::Report { metric, note, .. } if metric == role::UNUSABLE && !chose => {
                Some(note.clone())
            }
            _ => None,
        });
        // A model turn has not happened until its inferred record is
        // stored: a turn that left none still leaves one.
        let records = if records.is_empty() {
            vec![role::nothing_record(
                name,
                Some("the role returned no record"),
            )]
        } else {
            records
        };
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
                        cites: vec![],
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
                        cites: vec![],
                        claim: Claim::Inferred,
                        venture: name.to_string(),
                        action,
                        question,
                        refusal: None,
                        signal: None,
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
                        // The numbers are computed here from the cited
                        // records, whatever the role said.
                        facts: Some(role::summary_facts(
                            &mine
                                .iter()
                                .filter(|e| cites.contains(&e.seq))
                                .cloned()
                                .collect::<Vec<_>>(),
                        )),
                        cites,
                        implausible: None,
                    }
                }
                Record::Report { metric, note, .. } if metric == role::NOTHING => {
                    role::nothing_record(name, Some(&note))
                }
                Record::Report { metric, note, .. } if metric == role::UNUSABLE => Record::Report {
                    claim: Claim::Inferred,
                    venture: name.to_string(),
                    metric,
                    value: None,
                    note: note.chars().take(300).collect(),
                    source: None,
                    confidence: None,
                    cites: vec![],
                    implausible: None,
                    facts: None,
                },
                other => Record::Report {
                    claim: Claim::Inferred,
                    venture: name.to_string(),
                    metric: role::UNUSABLE.to_string(),
                    value: None,
                    note: format!(
                        "the role returned a {} record; only proposals, asks and summaries count",
                        other.kind()
                    ),
                    source: None,
                    confidence: None,
                    cites: vec![],
                    implausible: None,
                    facts: None,
                },
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
        if let Some(why) = unusable {
            if let Err(e) = self.escalate(name, &why, &role::prompt(&ctx)) {
                log(&format!("{name}: cannot escalate: {e}"));
            }
        }
    }

    /// Write an ask with this question unless one is already open, or the
    /// asks quota is used up.
    fn ask_once(&mut self, name: &str, question: &str) {
        let (already, may_ask) = {
            let Ok(index) = self.index() else { return };
            let already = index.open_asks().into_iter().any(|q| {
                index.get(q).is_some_and(
                    |e| matches!(&e.record, Record::Ask { question: q, .. } if q == question),
                )
            });
            (already, self.may_ask(&index))
        };
        if already {
            return;
        }
        if !may_ask {
            log(&format!(
                "{name}: the asks quota is used up; not asking: {question}"
            ));
            self.refuse(
                name,
                "ask",
                "AsksQuotaSpent".to_string(),
                format!("the asks quota is used up; not asking: {question}"),
                false,
            );
            return;
        }
        match self.append(Record::Ask {
            cites: vec![],
            claim: Claim::Measured,
            venture: name.to_string(),
            action: None,
            question: question.to_string(),
            refusal: None,
            signal: None,
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
    #[cfg(target_os = "linux")]
    {
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
    #[cfg(not(target_os = "linux"))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "network namespaces are Linux only",
        ))
    }
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

/// The intent an ask is about, when it asks whether a run with no outcome
/// happened.
fn pending_asked(e: &Entry) -> Option<Seq> {
    match &e.record {
        Record::Ask {
            claim: Claim::Measured,
            action: None,
            signal: Some(Signal::Pending { intent, .. }),
            ..
        } => Some(*intent),
        _ => None,
    }
}

/// Does this ask, a delegate ask, name the lever? The question says so.
fn delegate_ask_names(index: &Index, ask: Seq, lever: &str) -> bool {
    index.get(ask).is_some_and(|e| {
        e.record.action() == Some(DELEGATE)
            && matches!(&e.record, Record::Proposal { reason, .. } | Record::Ask { question: reason, .. }
                if reason.starts_with(&format!("{lever} was approved")) || reason.contains(&format!(" {lever} was approved")))
    })
}

/// The approvals of a lever since the last word on delegating it, when
/// `n` of them share a shape of payload: the shape and the approvals.
fn ripe(index: &Index, venture: &str, lever: &str, n: usize) -> Option<(String, Vec<Seq>)> {
    let answers = index.answers(venture);
    // The last word on delegating this lever: an approval or denial of
    // an earlier delegate ask naming it.
    let since = answers
        .iter()
        .rev()
        .copied()
        .find(|d| {
            index.get(*d).is_some_and(|e| {
                matches!(&e.record, Record::Decision { verdict: Verdict::Approve { ask, .. } | Verdict::Deny { ask, .. }, .. }
                    if delegate_ask_names(index, *ask, lever))
            })
        })
        .unwrap_or(0);
    let mut by_shape: BTreeMap<String, Vec<Seq>> = BTreeMap::new();
    for d in answers.iter().filter(|d| **d > since) {
        let Some(Record::Decision {
            verdict: Verdict::Approve { ask, payload },
            ..
        }) = index.get(*d).map(|e| &e.record)
        else {
            continue;
        };
        let Some(asked) = index.get(*ask) else {
            continue;
        };
        if asked.record.action() != Some(lever) {
            continue;
        }
        let shape = payload
            .as_ref()
            .or(asked.record.payload())
            .map_or("none".to_string(), Payload::shape);
        by_shape.entry(shape).or_default().push(*d);
    }
    by_shape.into_iter().find(|(_, s)| s.len() >= n)
}

/// Was anything in this time-ordered list written on this local date?
/// Only times within a few days of now can fall on today.
fn written_on(times: &[i64], today: NaiveDate) -> bool {
    let since = now() - 3 * Cap::DAY.as_secs() as i64;
    times[times.partition_point(|t| *t < since)..]
        .iter()
        .any(|t| {
            Local
                .timestamp_opt(*t, 0)
                .single()
                .is_some_and(|t| t.date_naive() == today)
        })
}

/// Asks this week, and incidents this week: failed actions and outcomes,
/// a broken chain or canary, and stops.
fn weekly(entries: &[Entry], now: i64) -> (u32, u32) {
    let since = now - 7 * Cap::DAY.as_secs() as i64;
    let week = entries.iter().filter(|e| e.at > since);
    let mut asks = 0;
    let mut incidents = 0;
    for e in week {
        match &e.record {
            Record::Ask { .. } => asks += 1,
            Record::Action { ok: false, .. }
            | Record::Outcome { ok: false, .. }
            | Record::Escalation { .. } => incidents += 1,
            Record::Action { action, .. } if action == crate::gate::STOP => incidents += 1,
            Record::Report {
                venture,
                metric,
                value: Some(v),
                ..
            } if venture == SELF && metric == "integrity" && *v < 1.0 => incidents += 1,
            _ => {}
        }
    }
    (asks, incidents)
}

/// The most decimal places a metric's recent measured values use, at most
/// `PRECISION_MAX`: the precision a delta between two of them is shown in.
fn precision(values: impl Iterator<Item = f64>) -> usize {
    let mut recent: Vec<f64> = values.collect();
    let skip = recent.len().saturating_sub(PRECISION_SEEN);
    recent
        .drain(skip..)
        .map(|v| {
            let text = v.to_string();
            text.split_once('.').map_or(0, |(_, frac)| frac.len())
        })
        .max()
        .unwrap_or(0)
        .min(PRECISION_MAX)
}

/// How many recent measured values say what precision a metric has.
const PRECISION_SEEN: usize = 50;
/// The most decimal places a digest delta shows.
const PRECISION_MAX: usize = 6;

/// The digest text: since the last digest when there is one this week,
/// else the last day.
fn digest_text(index: &Index, ventures: &BTreeMap<String, Venture>, now: i64) -> String {
    let entries = index.entries();
    let day = Cap::DAY.as_secs() as i64;
    // Since the last digest record when there is one this week, by its
    // place in the ledger; else the last day, by time.
    let last_digest = entries
        .iter()
        .rev()
        .find(|e| matches!(&e.record, Record::Report { venture, metric, .. } if venture == SELF && metric == "digest"))
        .filter(|e| e.at > now - 7 * day);
    let since = last_digest.map_or(now - day, |e| e.at);
    let window: Vec<&Entry> = entries
        .iter()
        .filter(|e| match last_digest {
            Some(d) => e.seq > d.seq,
            None => e.at > since,
        })
        .collect();
    let count = |kinds: &[Kind], v: Option<&str>| {
        window
            .iter()
            .filter(|e| kinds.contains(&e.record.kind()))
            .filter(|e| v.is_none_or(|v| e.record.venture() == v))
            .count()
    };
    let cost_of = |e: &Entry| -> u32 {
        ventures
            .get(e.record.venture())
            .and_then(|v| e.record.action().and_then(|a| v.mandate.permitted(a)))
            .map_or(0, |p| p.cost)
    };
    let total_cost: u32 = window
        .iter()
        .filter(|e| matches!(e.record.kind(), Kind::Action | Kind::Intent))
        .map(|e| cost_of(e))
        .sum();
    let since_text = chrono::DateTime::from_timestamp(since, 0).map_or(since.to_string(), |t| {
        t.format("%Y-%m-%dT%H:%MZ").to_string()
    });
    let mut out = format!(
        "digest since {since_text}: {} ventures, {} actions, {} asks, cost {total_cost}
",
        ventures.len(),
        count(&[Kind::Action, Kind::Intent], None),
        count(&[Kind::Ask], None),
    );
    let mut names: Vec<&String> = ventures.keys().filter(|n| *n != SELF).collect();
    names.push(&ventures[SELF].name);
    let (asks_week, incidents_week) = weekly(entries, now);
    for name in names.into_iter().filter(|n| ventures.contains_key(*n)) {
        let v = &ventures[name];
        let metric = v.mandate.metric_name();
        let reading = |e: &&Entry| -> Option<f64> {
            match &e.record {
                Record::Report {
                    venture,
                    metric: m,
                    value,
                    ..
                } if venture == name && m == metric => *value,
                _ => None,
            }
        };
        let latest = window.iter().rev().find_map(reading);
        let before = entries
            .iter()
            .filter(|e| e.at <= since)
            .rev()
            .find_map(|e| reading(&e));
        let metric_text = match latest {
            Some(value) => {
                let places = precision(entries.iter().filter_map(|e| match &e.record {
                    Record::Report {
                        claim: Claim::Measured,
                        venture,
                        metric: m,
                        value,
                        ..
                    } if venture == name && m == metric => *value,
                    _ => None,
                }));
                let delta = match before {
                    Some(b) if (value - b).abs() < f64::EPSILON => " (=)".to_string(),
                    Some(b) => format!(" ({:+.places$})", value - b),
                    None => String::new(),
                };
                format!("{metric}={value}{delta}")
            }
            None => format!("{metric}: no report"),
        };
        if name == SELF {
            out.push_str(&format!(
                "{name:<12} {metric_text:<20} asks {asks_week}/week  incidents {incidents_week}/week
"
            ));
        } else {
            out.push_str(&format!(
                "{name:<12} {metric_text:<20} {} actions  {} asks  cost {}
",
                count(&[Kind::Action, Kind::Intent], Some(name)),
                count(&[Kind::Ask], Some(name)),
                window
                    .iter()
                    .filter(|e| e.record.venture() == name
                        && matches!(e.record.kind(), Kind::Action | Kind::Intent))
                    .map(|e| cost_of(e))
                    .sum::<u32>()
            ));
        }
    }
    let refused = window
        .iter()
        .filter(|e| matches!(&e.record, Record::Refusal { closes: None, .. }))
        .count();
    if refused > 0 {
        out.push_str(&format!("refusals: {refused} opened\n"));
    }
    let open = index.open_asks().len();
    out.push_str(&format!(
        "open asks: {open}{}
",
        if open > 0 { "  (avsm asks)" } else { "" }
    ));
    out
}

/// The proof behind a record, from a snapshot of the ledger.
fn why_in(
    index: &Index,
    adopted: &[Adopted],
    quotas: &crate::config::Quotas,
    seq: Seq,
) -> Result<String, String> {
    let entry = index.get(seq).ok_or_else(|| format!("no record #{seq}"))?;
    if let Record::Proposal {
        venture, action, ..
    } = &entry.record
    {
        let candidate = Candidate {
            venture: venture.clone(),
            action: action.clone(),
            cites: vec![seq],
        };
        let derivation = match gate::gate_in(index, &candidate, adopted, quotas, now()) {
            Ok(proof) => proof.to_string(),
            Err(refusal) => refusal.to_string(),
        };
        return Ok(format!("{}\n{derivation}", entry.line()));
    }
    // An outcome rests on its intent: the proof that let the run start.
    let mut text = match &entry.record {
        Record::Outcome { intent, .. } => match index.get(*intent) {
            Some(started) => format!(
                "{}\n  outcome of intent #{intent}\n{}",
                entry.line(),
                gate::why(started)
            ),
            None => gate::why(entry),
        },
        _ => gate::why(entry),
    };
    // The mandate version an action, intent or outcome rests on.
    let proof = match &entry.record {
        Record::Action { proof, .. } | Record::Intent { proof, .. } => Some(proof),
        Record::Outcome { intent, .. } => index.get(*intent).and_then(|e| match &e.record {
            Record::Intent { proof, .. } => Some(proof),
            _ => None,
        }),
        _ => None,
    };
    if let Some(line) = proof.and_then(|p| mandate_version_line(index, p)) {
        text.push_str(&format!("  authorized by {line}\n"));
    }
    Ok(text)
}

/// `<venture> mandate vN (#<adopt seq>, <short hash>)` for the mandate a
/// proof rests on, counting that venture's adoptions from 1.
fn mandate_version_line(index: &Index, proof: &gate::Proof) -> Option<String> {
    let (seq, hash) = proof.steps.iter().find_map(|s| match s {
        gate::Step::Mandate { seq, hash } => Some((*seq, hash)),
        _ => None,
    })?;
    let venture = index.get(seq)?.record.venture();
    let n = index.mandate_version(venture, seq)?;
    Some(format!(
        "{venture} mandate v{n} (#{seq}, {})",
        &hash[..12.min(hash.len())]
    ))
}

/// The current facts about a venture (when `what` names one) or a topic,
/// from a snapshot of the ledger: number, age, claim, the fact, and what
/// it rests on.
fn know_in(index: &Index, ventures: &BTreeSet<String>, what: &str, for_model: bool) -> String {
    let by_venture = ventures.contains(what) || what == SELF;
    let current = gate::current_facts(index.entries());
    let picked: Vec<&Entry> = current
        .iter()
        .filter(|(_, v, t)| if by_venture { v == what } else { t == what })
        .filter_map(|(s, _, _)| index.get(*s))
        .filter(|e| !(for_model && role::hidden(e)))
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
    if !QUIET.with(std::cell::Cell::get) {
        eprintln!("avsm: {line}");
    }
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
    fn execute(&mut self, candidate: &Candidate) -> Result<Vec<Entry>, Failure> {
        self.0.send(&ToWarden::Execute(candidate.clone()))?;
        match self.0.recv::<FromWarden>()? {
            FromWarden::Done(entries) => Ok(entries),
            FromWarden::Refused { reason_key, detail } => {
                Err(Failure::Refused { reason_key, detail })
            }
            FromWarden::Err(e) => Err(e.into()),
            _ => Err("the warden answered the wrong question".to_string().into()),
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

    fn read(&mut self, venture: &str, metric: &str) -> Result<(Option<f64>, String), String> {
        self.0
            .send(&ToWarden::Read(venture.to_string(), metric.to_string()))?;
        match self.0.recv::<FromWarden>()? {
            FromWarden::Measured(value, note) => Ok((value, note)),
            FromWarden::Err(e) => Err(e),
            _ => Err("the warden answered the wrong question".to_string()),
        }
    }

    fn confirm(&mut self, intent: Seq) -> Result<Option<(bool, String)>, String> {
        self.0.send(&ToWarden::Confirm(intent))?;
        match self.0.recv::<FromWarden>()? {
            FromWarden::Confirmed(found) => Ok(found),
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
                Err(Failure::Refused { reason_key, detail }) => {
                    FromWarden::Refused { reason_key, detail }
                }
                Err(Failure::Error(e)) => FromWarden::Err(e),
            },
            Ok(ToWarden::Infer(r)) => match warden.infer(&r) {
                Ok(t) => FromWarden::Text(t),
                Err(e) => FromWarden::Err(e),
            },
            Ok(ToWarden::Measure(v)) => match warden.measure(&v) {
                Ok((value, note)) => FromWarden::Measured(value, note),
                Err(e) => FromWarden::Err(e),
            },
            Ok(ToWarden::Confirm(i)) => match warden.confirm(i) {
                Ok(found) => FromWarden::Confirmed(found),
                Err(e) => FromWarden::Err(e),
            },
            Ok(ToWarden::Read(v, m)) => match warden.read(&v, &m) {
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
    kernel.forecasts = true;
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

/// The master caution line: which ventures need a human, or that none do.
fn caution(states: &[(&str, gate::State)]) -> String {
    let need: Vec<String> = states
        .iter()
        .filter(|(_, s)| *s != gate::State::Running)
        .map(|(v, s)| format!("{v} {s}"))
        .collect();
    if need.is_empty() {
        return "all running\n".to_string();
    }
    let n = need.len();
    let (noun, verb) = if n == 1 {
        ("venture", "needs")
    } else {
        ("ventures", "need")
    };
    format!("caution: {n} {noun} {verb} you: {}\n", need.join(", "))
}

#[cfg(test)]
mod tests {
    use super::precision;

    #[test]
    fn precision_is_the_most_places_seen_capped_at_six() {
        assert_eq!(precision([24.0, 27.6].into_iter()), 1);
        assert_eq!(precision([1.0, 2.0].into_iter()), 0);
        assert_eq!(precision(std::iter::empty()), 0);
        assert_eq!(precision([0.1234567891].into_iter()), 6);
    }
}
