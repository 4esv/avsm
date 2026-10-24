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
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{Local, NaiveDate, TimeZone, Timelike};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::exit::{self, Exit};
use crate::gate::{self, Adopted, Candidate, Reason, State, DELEGATE, SELF};
use crate::index::Index;
use crate::ledger::Ledger;
use crate::mandate::{self, Cap, Controller, Permitted, Restart};
use crate::packet;
use crate::record::{self, Claim, Entry, Kind, Packet, Payload, Record, Seq, Signal, Verdict};
use crate::role::{self, ChildView, Context, InferRequest, ModelRole, Role, SUMMARY};
use crate::venture::{self, Venture};
use crate::warden::{self, Failure, FromWarden, LocalWarden, Started, ToWarden, Warden};

mod escalate;
mod expect;
mod guards;
pub mod plan;
pub mod predict;
mod refusals;

pub use guards::{ALPHA, WINDOW};

/// What a canary ask asks, each plausibly wrong on its face: it rests on
/// nothing measured. Taken in turn.
const CANARIES: [&str; 5] = [
    "reply to ops@vendor-example.test, an address seen only in an inferred record",
    "refund order 4412 in full, a figure that appears in no measured record",
    "delete the backups older than three days to make room, which no mandate names",
    "raise the cap to 40 a day on the strength of one inferred summary",
    "email every customer about the outage, which no report shows",
];

/// How long a report may take.
pub const REPORT_TIMEOUT: Duration = Duration::from_secs(60);
/// The longest wait before restarting a dead `run`.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A `run` alive this long has its backoff forgiven.
const STEADY: Duration = Duration::from_secs(600);
/// While the ledger cannot be written, how often a tick tries again.
const LEDGER_RETRY: Duration = Duration::from_secs(30);
/// The first wait before starting a dead warden or roles process again
/// when starting it failed; it doubles up to `MAX_BACKOFF`.
const RESPAWN_BACKOFF: Duration = Duration::from_secs(1);
/// A capacity episode closes after this many clean nightly checks in a
/// row: a week.
pub const QUIET_CHECKS: u32 = 7;

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
    /// When the digest was last written, Unix seconds.
    digested: Option<i64>,
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
    /// What each judged run did, the right way, net of drift, and the
    /// noise it was judged against: the evidence runs add up to.
    effects: BTreeMap<Seq, (f64, f64)>,
    /// Set only by `simulate`, when it builds the kernel: lever runs go to
    /// the simulation world, never to the levers. No config can set it.
    sim: Option<Arc<crate::simulate::Simulation>>,
    /// Forecast at add and before a ratchet ask, as config.yaml's
    /// `forecast:` says. The daemon sets it; a simulation and a forecast's
    /// own branches never do, so a forecast never forecasts.
    pub forecasts: bool,
    /// Forecasts running on threads of their own, each with what its
    /// records cite and what it is of: written as each comes in.
    forecasting: Vec<Forecasting>,
    /// The forecast a command asked for, taken from the head: the socket
    /// runs it with the kernel free, then writes it (`finish_forecast`).
    asked: Option<crate::forecast::Prepared>,
    /// Ventures whose metric is in a capacity episode, it drifting from
    /// the target faster than the levers can bring it back, each with the
    /// clean checks in a row since. Asked once an episode.
    short: BTreeMap<String, u32>,
    /// Each venture's last packet forecast, with the report of its metric
    /// it started from: asks before the next reading reuse it.
    packet_forecasts: RefCell<BTreeMap<String, (Seq, crate::forecast::Outcome)>>,
    /// What self's chain check has verified so far.
    checked: crate::selfventure::Checked,
    /// The local date each venture's capacity was last checked nightly.
    capacity_checked: BTreeMap<String, NaiveDate>,
    /// A forecast branch ticks only the ventures it forecasts: the rest,
    /// self among them, are neither read nor cycled there.
    pub(crate) scope: Option<BTreeSet<String>>,
    /// Set by the daemon alone: a lever runs apart in the warden and a
    /// `report` program on a thread, so a slow one holds up neither the
    /// socket nor the other ventures. A simulation and in-process callers
    /// run each to its end.
    pub apart: bool,
    /// Lever runs going on apart: each intent with what it runs.
    inflight: BTreeMap<Seq, Inflight>,
    /// Why the ledger cannot be written, while it cannot.
    fault: Option<Fault>,
    /// A warden or roles process found dead and not yet started again.
    dead: BTreeMap<&'static str, Dead>,
    /// The intents the command being handled started apart, for the
    /// socket to wait on the outcomes of once the kernel is free.
    started: Vec<Seq>,
    /// Set by the daemon alone: every record after this one must be one
    /// this kernel or its warden wrote. None: nobody keeps count.
    vetted: Option<Seq>,
    /// What this kernel and its warden appended past `vetted`, by number,
    /// with each record's hash.
    ours: BTreeMap<Seq, String>,
    /// Records found in the ledger that this daemon did not write while it
    /// ran. While any is unanswered nothing runs; a human's `resume self`
    /// answers them.
    foreign: Vec<Seq>,
}

/// A lever run going on apart.
struct Inflight {
    venture: String,
    action: String,
    since: Instant,
    /// Told to stop: its venture was paused.
    cancelled: bool,
}

/// The ledger failing to take a write.
struct Fault {
    error: String,
    since: i64,
    retry_at: Instant,
}

/// A child process of the daemon found dead.
struct Dead {
    why: String,
    retry_at: Instant,
    backoff: Duration,
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

/// A forecast running on a thread of its own: where its outcome will
/// come, what its records will cite, and what it is of, for the log.
struct Forecasting {
    coming: std::sync::mpsc::Receiver<Result<crate::forecast::Outcome, String>>,
    cites: Vec<Seq>,
    of: String,
}

/// Where a reading still coming will come from.
enum Coming {
    /// A `report` program running on a thread: where its reading will
    /// come, and the flag that stops it.
    Report(
        std::sync::mpsc::Receiver<(Option<f64>, String)>,
        Arc<AtomicBool>,
    ),
    /// An adapter the warden is still reading: asked again each tick.
    Warden,
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
    /// A `report` program running apart: since when, and where its
    /// reading will come.
    measuring: Option<(Instant, Coming)>,
    /// A reading the pause stopped: none starts again until the venture
    /// is resumed, or a hung one would start anew each cadence.
    held: bool,
}

/// Unix seconds now.
pub fn now() -> i64 {
    warden::now()
}

/// Where avsmd keeps its files: `$AVSMD_HOME`, else the platform's place
/// for a daemon's state.
pub fn default_home() -> PathBuf {
    home_for(std::env::consts::OS, |name| std::env::var_os(name))
}

/// The home on an operating system, with the environment read through
/// `env`: `$AVSMD_HOME` when set; on macOS
/// `~/Library/Application Support/avsmd`; elsewhere
/// `$XDG_STATE_HOME/avsmd`, which defaults to `~/.local/state/avsmd`.
pub fn home_for(os: &str, env: impl Fn(&str) -> Option<std::ffi::OsString>) -> PathBuf {
    let set = |name: &str| env(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(h) = set("AVSMD_HOME") {
        return h;
    }
    let home = set("HOME").unwrap_or_else(|| PathBuf::from("."));
    if os == "macos" {
        return home.join("Library/Application Support/avsmd");
    }
    // XDG says a relative XDG_STATE_HOME is invalid and is ignored.
    match set("XDG_STATE_HOME").filter(|p| p.is_absolute()) {
        Some(state) => state.join("avsmd"),
        None => home.join(".local/state/avsmd"),
    }
}

/// Where avsm 1.0 kept its files.
pub fn old_home(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    env("HOME")
        .filter(|v| !v.is_empty())
        .map(|h| PathBuf::from(h).join(".avsm"))
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
        Err("no roles process; start one with: avsmd".to_string())
    }
}

/// A warden for a kernel that has no warden process.
struct NoWarden;

impl Warden for NoWarden {
    fn execute(&mut self, _candidate: &Candidate) -> Result<Vec<Entry>, Failure> {
        Err("no warden process; start one with: avsmd"
            .to_string()
            .into())
    }

    fn infer(&mut self, _request: &InferRequest) -> Result<String, String> {
        Err("no warden process; start one with: avsmd".to_string())
    }

    fn measure(&mut self, _venture: &str) -> Result<(Option<f64>, String), String> {
        Err("no warden process; start one with: avsmd".to_string())
    }

    fn confirm(&mut self, _intent: Seq) -> Result<Option<(bool, String)>, String> {
        Err("no warden process; start one with: avsmd".to_string())
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
        Kernel::with_index(home, role, warden, None)
    }

    /// `with`, its index started from a copy of `seed` when that is an
    /// index of a prefix of the ledger in `home`.
    fn with_index(
        home: &Path,
        role: Box<dyn Role + Send>,
        warden: Box<dyn Warden + Send>,
        seed: Option<&Index>,
    ) -> Result<Kernel, String> {
        make_home(home).map_err(|e| format!("cannot create {}: {e}", home.display()))?;
        let ledger = Ledger::open(&home.join("ledger.db")).map_err(|e| e.to_string())?;
        let config = Config::load(home)?;
        let index = match seed {
            Some(seed) => seed.seed_for(&ledger).map_err(|e| e.to_string())?,
            None => Index::default(),
        };
        let mut kernel = Kernel {
            home: home.to_path_buf(),
            ledger,
            index: RefCell::new(index),
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
            effects: BTreeMap::new(),
            sim: None,
            forecasts: false,
            forecasting: Vec::new(),
            asked: None,
            short: BTreeMap::new(),
            capacity_checked: BTreeMap::new(),
            packet_forecasts: RefCell::new(BTreeMap::new()),
            checked: crate::selfventure::Checked::default(),
            scope: None,
            apart: false,
            inflight: BTreeMap::new(),
            fault: None,
            dead: BTreeMap::new(),
            started: Vec::new(),
            vetted: None,
            ours: BTreeMap::new(),
            foreign: Vec::new(),
        };
        kernel.reload()?;
        kernel.recover();
        Ok(kernel)
    }

    /// A forecast branch's kernel: a keyless warden that sends every lever
    /// run to the simulation, the given scripted role, and a ledger copy
    /// that is deleted after. Only a forecast builds one. `head` is the
    /// index of the ledger the copy was made from: the kernel and the
    /// warden start from it and read only what the branch appends.
    pub(crate) fn forecasting(
        home: &Path,
        sim: Arc<crate::simulate::Simulation>,
        role: Box<dyn Role + Send>,
        head: &Index,
    ) -> Result<Kernel, String> {
        let warden = LocalWarden::keyless(home, sim.clone(), head)?;
        let mut kernel = Kernel::with_index(home, role, Box::new(warden), Some(head))?;
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

    /// Append a record now. A failure is the ledger's fault until a write
    /// goes through again: said once, shown by status, and nothing runs
    /// while it lasts.
    pub fn append(&mut self, record: Record) -> Result<Entry, String> {
        match self.ledger.append(record) {
            Ok(e) => {
                if self.vetted.is_some() {
                    self.ours.insert(e.seq, e.hash.clone());
                }
                if let Some(f) = self.fault.take() {
                    log(&format!(
                        "the ledger takes writes again after {}",
                        ago(now() - f.since)
                    ));
                }
                Ok(e)
            }
            Err(e) => {
                let e = e.to_string();
                self.ledger_failed(&e);
                Err(e)
            }
        }
    }

    /// The ledger failed: say so the first time, and try again after
    /// `LEDGER_RETRY`.
    fn ledger_failed(&mut self, error: &str) {
        let retry_at = Instant::now() + LEDGER_RETRY;
        match &mut self.fault {
            Some(f) => f.retry_at = retry_at,
            None => {
                log(&format!(
                    "the ledger cannot be written: {error}; nothing runs until it can. see: avsm doctor"
                ));
                self.fault = Some(Fault {
                    error: error.to_string(),
                    since: now(),
                    retry_at,
                });
            }
        }
    }

    /// Run one command from the socket. The first argument is the command.
    pub fn handle(&mut self, args: &[String]) -> Result<String, String> {
        self.handle_as(args, None)
    }

    /// Run one command given by a known human; their login goes on every
    /// declared record the command writes.
    pub fn handle_as(&mut self, args: &[String], by: Option<&str>) -> Result<String, String> {
        self.by = by.map(str::to_string);
        self.started.clear();
        self.vet();
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
            ("tail", ["--since", seq]) => self.tail_since(parse_seq(seq)?),
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
            ("doctor", []) => Ok(self.doctor()),
            ("forecast", rest) => {
                let opts = crate::forecast::Options::parse(rest, self.config.forecast_settings())?;
                // The daemon takes what the forecast needs and lets the
                // socket run it with the kernel free (`take_forecast`).
                if self.apart {
                    let prepared = {
                        let index = self.index()?;
                        crate::forecast::prepare(&index, &self.ledger, &self.home, &opts)?
                    };
                    self.asked = Some(prepared);
                    return Ok(String::new());
                }
                let (outcome, written) = self.forecast(&opts, &[])?;
                Ok(rendered(&outcome, &written))
            }
            ("propose", [venture, lever]) => self.propose(venture, lever, None),
            ("propose", [venture, lever, payload @ ..]) => {
                self.propose(venture, lever, Some(&payload.join(" ")))
            }
            _ => Err(format!("avsmd does not understand: {}", args.join(" "))),
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
        if let Some(problem) = venture::dir_problem(dir) {
            return Err(problem.text().to_string());
        }
        let dir = dir
            .canonicalize()
            .map_err(|e| format!("cannot find {}: {}", dir.display(), venture::plain(&e)))?;
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
        // Worth saying, not worth refusing: a child is legitimately added
        // before its parent, and a program may be put in place after.
        let mut warnings = venture::missing_programs(&dir, &mandate);
        // The config in force is the daemon's, not the file's now: say so
        // when they differ, and refuse a backend only the file has.
        let on_disk = Config::load(&self.home);
        let changed = match &on_disk {
            Ok(d) if *d != self.config => {
                warnings.push("config.yaml changed on disk; restart avsmd to apply it".into());
                true
            }
            Err(e) => {
                warnings.push(format!(
                    "config.yaml on disk does not read ({e}); avsmd keeps the one in force"
                ));
                false
            }
            _ => false,
        };
        let named = match &mandate.controller {
            crate::mandate::Controller::Classifier { backend }
            | crate::mandate::Controller::Model { backend } => backend.as_deref(),
            crate::mandate::Controller::Plan { backend, .. } => backend.as_deref(),
            _ => None,
        };
        if let Some(b) = named.filter(|b| !self.config.backends.contains_key(*b)) {
            let known = on_disk.as_ref().is_ok_and(|d| d.backends.contains_key(b));
            return Err(format!(
                "{name}'s controller names the {b} backend, which the config in force does not have{}",
                if known && changed {
                    "; config.yaml changed on disk; restart avsmd to apply it"
                } else {
                    "; add it under backends in config.yaml, then restart avsmd"
                }
            ));
        }
        if let Some(p) = &mandate.parent {
            if !self.ventures.contains_key(p) {
                warnings.push(format!("{p} is not a venture yet"));
            }
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
        for warning in warnings {
            out.push_str(&format!("  warning: {warning}\n"));
        }
        out.push_str(&crate::teach::render(&mandate));
        // What the mandate can keep up with, before anything runs under
        // it. It follows the adoption in the ledger; the adoption stays the
        // last line shown, where scripts read it. The forecast waits for
        // the venture's first trusted, plausible reading under it.
        if let Some(e) = self.check_capacity(&name) {
            out.push_str(&format!("  {}\n", e.line()));
        }
        out.push_str(&format!("  {}\n", entry.line()));
        Ok(out)
    }

    /// When `report` is the venture's first trusted, plausible reading
    /// since its mandate was adopted, forecast it, citing the adoption and
    /// the report. A re-adoption of the same mandate is not a new one: it
    /// forecasts nothing new. From the ledger, so a restart repeats none;
    /// one still running on its thread when avsmd stops is not written.
    fn forecast_first_reading(&mut self, v: &Venture, metric: &str, report: Seq) {
        if v.name == SELF || !self.forecasts {
            return;
        }
        let Some(settings) = self.config.automatic_forecast() else {
            return;
        };
        let first = {
            let Ok(index) = self.index() else { return };
            let series = index.series(&v.name, metric);
            let first = |after: Seq| {
                let i = series.partition_point(|(s, _)| *s <= after);
                series.get(i).map(|(s, _)| *s)
            };
            // Cheap first: the first since this adoption at all.
            first(v.adopted) == Some(report)
                && first(index.unchanged_since(&v.name, v.adopted)) == Some(report)
        };
        if !first {
            return;
        }
        let opts = crate::forecast::Options::new(vec![v.name.clone()], settings);
        let cites = [v.adopted, report];
        let done = if self.apart {
            self.forecast_apart(&opts, &cites, &v.name)
        } else {
            self.forecast(&opts, &cites).map(|_| ())
        };
        if let Err(e) = done {
            log(&format!("{}: no forecast: {e}", v.name));
        }
    }

    /// Start a forecast on a thread of its own: what it needs is taken
    /// from the head now, and its records are written by the tick it
    /// comes in on (`forecasts_in`), each citing `cites`. The kernel, its
    /// commands and its guards go on meanwhile.
    fn forecast_apart(
        &mut self,
        opts: &crate::forecast::Options,
        cites: &[Seq],
        of: &str,
    ) -> Result<(), String> {
        let prepared = {
            let index = self.index()?;
            crate::forecast::prepare(&index, &self.ledger, &self.home, opts)?
        };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(prepared.run());
        });
        self.forecasting.push(Forecasting {
            coming: rx,
            cites: cites.to_vec(),
            of: of.to_string(),
        });
        Ok(())
    }

    /// Write each forecast that came in from its thread.
    fn forecasts_in(&mut self) {
        let mut still = Vec::new();
        for f in std::mem::take(&mut self.forecasting) {
            match f.coming.try_recv() {
                Err(std::sync::mpsc::TryRecvError::Empty) => still.push(f),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    log(&format!("{}: no forecast: its thread ended", f.of));
                }
                Ok(Err(e)) => log(&format!("{}: no forecast: {e}", f.of)),
                Ok(Ok(outcome)) => {
                    if let Err(e) = self.write_forecast(&outcome, &f.cites) {
                        log(&format!("{}: cannot write the forecast: {e}", f.of));
                    }
                }
            }
        }
        self.forecasting.extend(still);
    }

    /// The forecast a command asked for, to be run with the kernel free.
    pub fn take_forecast(&mut self) -> Option<crate::forecast::Prepared> {
        self.asked.take()
    }

    /// Write the forecast a command asked for, run with the kernel free,
    /// and say what it found.
    pub fn finish_forecast(
        &mut self,
        ran: Result<crate::forecast::Outcome, String>,
    ) -> Result<String, String> {
        let outcome = ran?;
        let written = self.write_forecast(&outcome, &[])?;
        Ok(rendered(&outcome, &written))
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
        let outcome = {
            let index = self.index()?;
            crate::forecast::run_in(&index, &self.ledger, &self.home, opts)?
        };
        let written = self.write_forecast(&outcome, cites)?;
        Ok((outcome, written))
    }

    /// Write a forecast's records, each citing `cites`, then the asks its
    /// walls raise.
    fn write_forecast(
        &mut self,
        outcome: &crate::forecast::Outcome,
        cites: &[Seq],
    ) -> Result<Vec<Entry>, String> {
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
            crate::forecast::asks(&index, outcome, &seqs)
        };
        for record in asks {
            let e = self.append(record)?;
            log(&e.line());
            written.push(e);
        }
        Ok(written)
    }

    /// Check, in code, that the permitted levers can keep the venture's
    /// metric from drifting off the target: its drift a day against the
    /// most the levers can move it a day. When they cannot, ask once per
    /// episode, before any saturation; the episode ends only after
    /// `QUIET_CHECKS` checks in a row find them able, so a gap that comes
    /// and goes asks once. Returns the ask written, if one was.
    pub fn check_capacity(&mut self, name: &str) -> Option<Entry> {
        let v = self.ventures.get(name)?.clone();
        let gap = {
            let index = self.index().ok()?;
            let asks = self
                .config
                .quotas
                .asks
                .map(|c| f64::from(c.count) * 86_400.0 / c.per.as_secs().max(1) as f64);
            let now = now();
            let adopted = self.adopted();
            let refs: Vec<&Adopted> = adopted.iter().collect();
            let budget = gate::budget_of_in(&index, &refs, name).map(|(_, b)| b);
            let parent = v.mandate.parent.as_deref().unwrap_or("the parent");
            crate::forecast::capacity_with(
                &index,
                name,
                &v.mandate,
                asks,
                budget.map(|b| (parent, b)),
            )
            .or_else(|| crate::forecast::cap_spent(&index, name, &v.mandate, now))
            .or_else(|| crate::forecast::held_back(&index, name, &v.mandate, now))
        };
        let Some(gap) = gap else {
            if let Some(clean) = self.short.get_mut(name) {
                *clean += 1;
                if *clean >= QUIET_CHECKS {
                    self.short.remove(name);
                }
            }
            return None;
        };
        if let Some(clean) = self.short.get_mut(name) {
            *clean = 0;
            return None;
        }
        self.short.insert(name.to_string(), 0);
        let before = self.ledger_last();
        let policies = self.policy_forecasts(&v);
        self.signal_citing(
            name,
            format!("{}; the target is out of reach", gap.describe()),
            Signal::Capacity {
                metric: gap.metric.clone(),
                drift: gap.drift,
                most: gap.most,
            },
            &gap.cites,
            policies,
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
        let alarms = self.alarms();
        if self.ventures.is_empty() {
            return Ok(alarms + "no ventures; add one with: avsm add <directory>\n");
        }
        let index = self
            .index()
            .map_err(|e| format!("{alarms}cannot read the ledger: {e}"))?;
        let now = now();
        let adopted = self.adopted();
        // A paused self runs no lever of any venture, and the tick treats
        // every one as paused: so does its row.
        let halted = gate::state_in(&index, &adopted, SELF) == gate::State::Paused;
        let held_by = if self.foreign.is_empty() {
            "self"
        } else {
            "alarm"
        };
        let held = |name: &str, s: gate::State| halted && name != SELF && s != gate::State::Paused;
        let mut states: Vec<(&str, gate::State)> = Vec::new();
        let mut labels: Vec<String> = Vec::new();
        for v in self.ventures.values() {
            let s = gate::state_in(&index, &adopted, &v.name);
            if held(&v.name, s) {
                states.push((v.name.as_str(), gate::State::Paused));
                labels.push(format!("paused ({held_by})"));
            } else {
                states.push((v.name.as_str(), s));
                labels.push(s.to_string());
            }
        }
        let mut out = alarms;
        let mut master = caution(&states);
        // A venture whose adopted mandate cannot be read runs nothing,
        // whatever its records say: it is never "running".
        let broken: Vec<&str> = self
            .ventures
            .values()
            .filter(|v| v.problem.is_some())
            .map(|v| v.name.as_str())
            .collect();
        if !broken.is_empty() {
            let line = format!(
                "caution: {} cannot run: {}\n",
                if broken.len() == 1 {
                    "1 venture"
                } else {
                    "ventures"
                },
                broken.join(", ")
            );
            master = if master == "all running\n" {
                line
            } else {
                master + &line
            };
        }
        if out.is_empty() || master != "all running\n" {
            out.push_str(&master);
        }
        for ((v, (_, state)), label) in self.ventures.values().zip(&states).zip(&labels) {
            // The metric's own latest reading, not another fact of the
            // venture (a guard's, an expectation's).
            let metric = v.mandate.metric_name();
            let last = index
                .of(&v.name)
                .rev()
                .find(|e| matches!(&e.record, Record::Report { metric: m, .. } if m == metric));
            let mut detail = match &v.problem {
                Some(p) => p.clone(),
                None => match last {
                    Some(e) => format!("{}  reported {}", e.record.summary(), ago(now - e.at)),
                    None => "no report yet".to_string(),
                },
            };
            if *state == gate::State::Implausible {
                detail.push_str(&held_implausible(&index, &adopted, &v.name));
            }
            // Suspended with nothing to answer (denied, or suspended again
            // with nothing new measured to ask on): a re-adoption restores
            // it.
            if *state == gate::State::Ineffective && index.open_of(&v.name).next().is_none() {
                let levers: Vec<&str> = v
                    .mandate
                    .permitted
                    .iter()
                    .filter(|p| index.suspended(&v.name, &p.name))
                    .map(|p| p.name.as_str())
                    .collect();
                if let Some(dir) = &v.path {
                    detail.push_str(&format!(
                        "  ({} {} suspended and no ask is open; re-adopt to restore it: avsm add {})",
                        levers.join(", "),
                        if levers.len() == 1 { "is" } else { "are" },
                        crate::doctor::sh(dir)
                    ));
                }
            }
            // A cap that binds now, whatever the state says: approving the
            // saturation ask answers it and leaves the cap as it is.
            let mut cap_spent = false;
            for p in v.mandate.permitted.iter().filter(|p| p.cap.count > 0) {
                let per = p.cap.per.as_secs() as i64;
                if index.used(&v.name, &p.name, now - per, now) >= p.cap.count {
                    cap_spent = true;
                    let first = index
                        .runs_of_after(&v.name, now - per, now)
                        .iter()
                        .find(|(_, a)| *a == p.name)
                        .map_or(now, |(at, _)| *at);
                    detail.push_str(&format!(
                        "  ({} cap {} spent; the next run may start in {})",
                        p.name,
                        p.cap,
                        span((first + per - now).max(0))
                    ));
                }
            }
            if let Some(note) = &v.note {
                detail.push_str(&format!("  [{note}]"));
            }
            if let Some(note) = self.runtime.get(&v.name).and_then(|rt| rt.note.as_deref()) {
                detail.push_str(&format!("  ({note})"));
            }
            for (seq, f) in self.inflight.iter().filter(|(_, f)| f.venture == v.name) {
                detail.push_str(&format!(
                    "  ({} running {}, intent #{seq}{})",
                    f.action,
                    ago(f.since.elapsed().as_secs() as i64).trim_end_matches(" ago"),
                    if f.cancelled { ", stopping" } else { "" }
                ));
            }
            if let Some((since, _)) = self
                .runtime
                .get(&v.name)
                .and_then(|rt| rt.measuring.as_ref())
            {
                if since.elapsed() >= Duration::from_secs(5) {
                    detail.push_str(&format!(
                        "  (report running {})",
                        ago(since.elapsed().as_secs() as i64).trim_end_matches(" ago")
                    ));
                }
            }
            // Saturated is a state only while the controller wants to act
            // and its lever is refused: a spent cap with nothing waiting is
            // still running, and the row says so.
            let state = match &v.problem {
                Some(_) => "cannot run".to_string(),
                None if *state == gate::State::Running && cap_spent => {
                    "running (cap spent; nothing waiting)".to_string()
                }
                None => label.clone(),
            };
            out.push_str(&format!("{:<12} {:<11} {detail}\n", v.name, state));
        }
        Ok(out)
    }

    /// The last `n` records, one line each.
    pub fn tail(&self, n: usize) -> Result<String, String> {
        if n == 0 {
            return Err("tail takes a count of 1 or more; 0 shows nothing".to_string());
        }
        let entries = self.ledger.tail(n).map_err(|e| e.to_string())?;
        if entries.is_empty() {
            return Ok("the ledger is empty\n".to_string());
        }
        Ok(entries.iter().map(tail_line).collect())
    }

    /// Every record after `seq`, one line each, oldest first.
    pub fn tail_since(&self, seq: Seq) -> Result<String, String> {
        let entries = self.ledger.after(seq).map_err(|e| e.to_string())?;
        Ok(entries.iter().map(tail_line).collect())
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
            if let Record::Ask {
                packet: Some(packet),
                signal,
                ..
            } = &e.record
            {
                out.push_str(&packet.render_for(signal.as_ref()));
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
        let index = self.index()?;
        if !index.is_open(seq) {
            let answered = index
                .of(entry.record.venture())
                .find_map(|e| match &e.record {
                    Record::Decision {
                        verdict: Verdict::Approve { ask, .. },
                        ..
                    } if *ask == seq => Some((e.seq, "approved")),
                    Record::Decision {
                        verdict: Verdict::Deny { ask, .. },
                        ..
                    } if *ask == seq => Some((e.seq, "denied")),
                    _ => None,
                });
            return Err(match answered {
                Some((by, what)) => {
                    format!("#{seq} was already {what} by #{by}\n{}", entry.line())
                }
                None => format!("#{seq} is not an open ask\n{}", entry.line()),
            });
        }
        Ok(entry)
    }

    /// Say yes to an ask, with the payload as the human amended it when
    /// they did. The next tick acts on it.
    pub fn approve(&mut self, seq: Seq, amended: Option<&str>) -> Result<String, String> {
        let ask = self.open_ask(seq)?;
        // The gate spends an approval its paused venture cannot run on, so
        // one given now would be gone by the resume. Saying whether a run
        // happened runs nothing, and is answered whatever the state.
        let venture = ask.record.venture().to_string();
        if pending_asked(&ask).is_none() {
            let index = self.index()?;
            if gate::state_in(&index, &self.adopted(), &venture) == gate::State::Paused {
                return Err(format!(
                    "{venture} is paused; resume it first (avsm resume {venture}), then approve: \
                     an approval given while paused is spent without running"
                ));
            }
        }
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
        self.mark_canary(&ask, &entry);
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
                .filter(|(s, _, _)| !asked.contains(s) && !self.inflight.contains_key(s))
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
                Ok(None) => {
                    let signal = Signal::Pending {
                        intent,
                        action: action.clone(),
                    };
                    Record::Ask {
                        cites: vec![],
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
                        packet: self.signal_packet(&venture, &signal),
                        signal: Some(signal),
                    }
                }
            };
            if let Ok(e) = self.append(record) {
                log(&e.line());
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
        self.mark_canary(&ask, &entry);
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
        let mut refused = false;
        let prefix = format!("{name}: {lever} ");
        for line in self.act(name, lever, vec![entry.seq], Claim::Declared) {
            // The gate, a quota or the warden said no: the proposal is
            // recorded, the command fails, and the client exits 65.
            refused |= line.strip_prefix(&prefix).is_some_and(|rest| {
                rest.starts_with("refused")
                    || rest.starts_with("did not run")
                    || rest.starts_with("not run:")
            }) || line.contains("asks quota is used up; not asking");
            out.push_str(&line);
            out.push('\n');
        }
        if refused {
            Err(out)
        } else {
            Ok(out)
        }
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
        if self.paused_by_declared(name)? {
            return Ok(format!("{name} is already paused\n"));
        }
        let entry = self.append(self.decision(name, Verdict::Pause))?;
        // What it runs is stopped now, not at the next tick: a tick can
        // wait behind a forecast.
        self.cancel_inflight(name);
        self.stop_reading(name);
        Ok(entry.line() + "\n")
    }

    /// Let a paused venture run again.
    pub fn resume(&mut self, name: &str) -> Result<String, String> {
        self.known(name)?;
        if !self.paused(name)? {
            return Ok(format!("{name} is not paused\n"));
        }
        let entry = self.append(self.decision(name, Verdict::Resume))?;
        // A human resuming self has seen the alarm: what it named stays
        // spent, and the kernel acts again.
        if name == SELF && !self.foreign.is_empty() {
            log(&format!(
                "foreign records {} answered by a human's resume of self (#{})",
                seqs(&self.foreign),
                entry.seq
            ));
            self.foreign.clear();
        }
        let mut out = entry.line() + "\n";
        // Resuming is the human's, so it stands; but a guard still past
        // its bound will pause it again at the next reading.
        if let (Some(v), Ok(index)) = (self.ventures.get(name), self.index()) {
            for g in &v.mandate.guards {
                let Some(&(_, x)) = index.series(name, &g.metric()).last() else {
                    continue;
                };
                if g.breached(x) {
                    let (above, bound) = g.bound();
                    out.push_str(&format!(
                        "warning: guard {} is still past its bound ({x} {} {bound}); the guard will pause it again\n",
                        g.name,
                        if above { ">" } else { "<" }
                    ));
                }
            }
        }
        Ok(out)
    }

    /// Is the venture paused now, as the records tell it? A guard's stop,
    /// self's alarm and a parent's pause all hold it, not only a declared
    /// pause.
    fn paused(&self, name: &str) -> Result<bool, String> {
        let index = self.index()?;
        Ok(gate::state_in(&index, &self.adopted(), name) == State::Paused)
    }

    /// Is the venture paused by a human's declared pause, as against a
    /// guard's stop, self's alarm or a parent's pause? Pausing one of
    /// those still records the human's decision; a second declared pause
    /// records nothing.
    fn paused_by_declared(&self, name: &str) -> Result<bool, String> {
        if !self.paused(name)? {
            return Ok(false);
        }
        let index = self.index()?;
        let mut pause: Option<Seq> = None;
        let mut resume: Option<Seq> = None;
        let mut declared = false;
        for seq in index.pause_sources(name) {
            let Some(e) = index.get(seq) else { continue };
            match &e.record {
                Record::Decision {
                    claim,
                    verdict: Verdict::Pause,
                    ..
                } => {
                    if pause.is_none_or(|p| seq > p) {
                        pause = Some(seq);
                        declared = *claim == Claim::Declared;
                    }
                }
                Record::Decision {
                    verdict: Verdict::Resume,
                    ..
                } => {
                    if resume.is_none_or(|r| seq > r) {
                        resume = Some(seq);
                    }
                }
                Record::Action { action, .. } if action == crate::gate::STOP => {
                    if pause.is_none_or(|p| seq > p) {
                        pause = Some(seq);
                        declared = false;
                    }
                }
                Record::Action {
                    action,
                    payload: Some(Payload::Text { text }),
                    ..
                } => {
                    let call = crate::mandate::ParentCall::parse(action, text).ok();
                    match call {
                        Some(crate::mandate::ParentCall::Pause { .. })
                            if pause.is_none_or(|p| seq > p) =>
                        {
                            pause = Some(seq);
                            declared = false;
                        }
                        Some(crate::mandate::ParentCall::Resume { .. })
                            if resume.is_none_or(|r| seq > r) =>
                        {
                            resume = Some(seq);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        let Some(p) = pause else { return Ok(false) };
        Ok(declared && resume.is_none_or(|r| p > r))
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
        // Nothing runs while the ledger cannot record it; try again now
        // and then.
        if self
            .fault
            .as_ref()
            .is_some_and(|f| Instant::now() < f.retry_at)
        {
            return;
        }
        self.vet();
        self.watch();
        self.settle_inflight();
        self.forecasts_in();
        // A dead warden cannot check an intent; it is checked once one is
        // back.
        if !self.dead.contains_key("warden") {
            self.recover();
        }
        let names: Vec<String> = self
            .ventures
            .keys()
            .filter(|n| self.scope.as_ref().is_none_or(|s| s.contains(*n)))
            .cloned()
            .collect();
        let adopted = self.adopted();
        // A ledger that cannot be read cannot be written either.
        let unread = self.index().err();
        if let Some(e) = unread {
            self.ledger_failed(&format!("cannot read it: {e}"));
            return;
        }
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
            // Draining: what is in flight finishes; nothing new starts.
            if stopping() {
                return;
            }
            self.supervise(name, paused);
            if paused {
                self.cancel_inflight(name);
                self.stop_reading(name);
            } else if let Some(rt) = self.runtime.get_mut(name) {
                rt.held = false;
            }
            // A reading still coming, or a lever still running, holds this
            // venture's next report; it holds no other venture's.
            if self.reading_in(name, halted) || self.busy(name) {
                continue;
            }
            let rt = self.runtime.entry(name.clone()).or_default();
            let due = rt.next_report <= now && !rt.held;
            if due && self.ventures[name].problem.is_none() {
                self.report(name, halted);
            }
            if self.fault.is_some() {
                return;
            }
        }
        if !halted && !stopping() {
            self.act_on_decisions();
            for name in &names {
                if self.capacity_due(name) {
                    self.check_capacity(name);
                }
            }
            self.canary();
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
                // The mandate in force is the adopted text; an edit on disk
                // nobody adopted never rides along. A 1.0 adoption has no
                // text in the ledger, and its file is the mandate.
                let adopted = {
                    let Ok(index) = self.index() else { return };
                    index.adoptions().get(&v.name).and_then(|a| a.text.clone())
                };
                let text = match adopted {
                    Some(t) => t,
                    None => match std::fs::read_to_string(dir.join(venture::MANDATE_FILE)) {
                        Ok(t) => t,
                        Err(e) => {
                            log(&format!("{}: cannot read the mandate: {e}", v.name));
                            continue;
                        }
                    },
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
                if let (true, Some(settings)) = (self.forecasts, self.config.automatic_forecast()) {
                    let mut opts = crate::forecast::Options::new(vec![v.name.clone()], settings);
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

    /// Write a canary ask for each venture that is due one: `every` asks
    /// since its last. The ask can never run; whoever answers it is
    /// measured by it. Off unless config.yaml says `canary_asks`.
    fn canary(&mut self) {
        let Some(every) = self.config.canary_asks.map(|c| c.every) else {
            return;
        };
        let names: Vec<String> = self
            .ventures
            .values()
            .filter(|v| v.name != SELF && v.problem.is_none())
            .map(|v| v.name.clone())
            .collect();
        for name in names {
            let (due, n, may_ask) = {
                let Ok(index) = self.index() else { return };
                (
                    index.canary_due(&name, every),
                    index.canaries_asked(&name),
                    self.may_ask(&index),
                )
            };
            if !due || !may_ask {
                continue;
            }
            let content = CANARIES[n % CANARIES.len()];
            // It looks like any other ask: the model's words last.
            let packet = self.packet(&name, None, &[], Some(content.to_string()), false);
            match self.append(Record::Ask {
                cites: vec![],
                claim: Claim::Measured,
                venture: name.clone(),
                action: Some(gate::CANARY.to_string()),
                question: format!("may I {content}?"),
                refusal: None,
                payload: None,
                signal: None,
                packet,
            }) {
                Ok(e) => log(&e.line()),
                Err(e) => log(&format!("{name}: cannot write ask: {e}")),
            }
        }
    }

    /// The answer to a canary ask is a measured fact: who approved or
    /// denied what could never run. It cites the decision.
    fn mark_canary(&mut self, ask: &Entry, decision: &Entry) {
        if ask.record.action() != Some(gate::CANARY) {
            return;
        }
        let approved = matches!(
            &decision.record,
            Record::Decision {
                verdict: Verdict::Approve { .. },
                ..
            }
        );
        let who = self
            .by
            .clone()
            .unwrap_or_else(|| crate::index::LOCAL.to_string());
        let record = Record::Report {
            claim: Claim::Measured,
            venture: ask.record.venture().to_string(),
            metric: crate::index::CANARY.to_string(),
            value: Some(if approved { 1.0 } else { 0.0 }),
            note: format!(
                "canary ask #{} {} by {who}",
                ask.seq,
                if approved { "approved" } else { "denied" }
            ),
            source: None,
            confidence: None,
            cites: vec![decision.seq],
            implausible: None,
            facts: None,
        };
        if let Err(e) = self.append(record) {
            log(&format!("cannot write the canary record: {e}"));
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

    /// Is the nightly digest due: past tonight's nightly time, with none
    /// written since it? A digest written earlier the same day (at first
    /// start, or under another nightly time) is the night before's, and
    /// does not stand for tonight's.
    fn digest_due(&self) -> bool {
        let Ok(Some(at)) = self.config.nightly_minutes() else {
            return false;
        };
        let local = crate::clock::local();
        let minutes = local.hour() * 60 + local.minute();
        if minutes < at {
            return false;
        }
        // Tonight's nightly, by the wall clock.
        let now = now();
        let since = now - i64::from(minutes - at) * 60 - i64::from(local.second());
        let after = |t: i64| t >= since && t <= now;
        if self.digested.is_some_and(after) {
            return false;
        }
        let Ok(index) = self.index() else {
            return false;
        };
        let times = index.digests_at();
        !times[times.partition_point(|t| *t < since)..]
            .iter()
            .any(|t| after(*t))
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
        self.digested = Some(now);
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
                    // What it started and left behind in its group goes
                    // with it: a run started again starts afresh.
                    // SAFETY: kill has no memory preconditions.
                    unsafe {
                        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
                    }
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
                    Some(hosts) if hosts.is_empty() && !confine => Some(network_none_note()),
                    Some(hosts) if !hosts.is_empty() => {
                        Some("network list holds adapters only; run is not held to it".to_string())
                    }
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
        if self.apart {
            if let Some(coming) = self.measure_apart(&v) {
                self.runtime.entry(name.to_string()).or_default().measuring =
                    Some((Instant::now(), coming));
                return;
            }
        }
        let (value, note) = match self.measure(&v) {
            Some(Err(e)) if self.apart && warden::pending(&e) => {
                // The warden reads it apart: the reading comes in a later
                // tick, and holds this venture's report till then.
                self.runtime.entry(name.to_string()).or_default().measuring =
                    Some((Instant::now(), Coming::Warden));
                return;
            }
            Some(Err(e)) => (None, format!("cannot measure: {e}")),
            Some(Ok(m)) => m,
            None => return,
        };
        self.reported(&v, halted, value, note);
    }

    /// Write a venture's reading as its report and act on it.
    fn reported(&mut self, v: &Venture, halted: bool, value: Option<f64>, note: String) {
        let name = v.name.as_str();
        // A reading code does not believe is kept, marked, and never a
        // premise; the first of an episode asks.
        let metric = v.mandate.metric_name().to_string();
        let (implausible, fresh) = self.doubt(
            name,
            &metric,
            value,
            v.mandate.range,
            v.mandate.steps(),
            v.mandate.confirm,
        );
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
            // The ledger's fault, said once by `append`.
            Err(_) => return,
        };
        let (readings, doubts) = self.read_guards(v);
        if halted {
            return;
        }
        self.doubt_readings(name, doubts);
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
        self.check_audits(v);
        self.read_readings(v);
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
        // The forecast of a fresh adoption runs on its first trusted,
        // plausible reading, so the branches start from a value.
        self.forecast_first_reading(v, &metric, entry.seq);
        let plans = name != SELF && matches!(v.mandate.controller, Controller::Plan { .. });
        if name != SELF && (plans || v.mandate.early_warning) {
            self.early_warning(name, &metric);
        }
        // The bound is the mandate's, or the one the parent set; a band
        // has one on each side, and a report crosses at most one of them.
        let thresholds = match self.index() {
            Ok(index) => gate::effective_thresholds_in(&index, &self.adopted(), name),
            Err(_) => return,
        };
        let mut ran = None;
        for t in thresholds {
            if self.threshold_fires(v, &t) {
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

    /// Read every reading of a venture through the warden and write each
    /// as an inferred report: it reads the open world, so a model may see
    /// it and nothing may act on it. The note is the fields the warden
    /// kept, or empty when the reading failed.
    fn read_readings(&mut self, v: &Venture) {
        for r in &v.mandate.readings {
            let metric = r.metric();
            let (value, note) = self.warden.read(&v.name, &metric).unwrap_or_default();
            let record = Record::report(
                Claim::Inferred,
                &v.name,
                &metric,
                value.filter(|x| x.is_finite()),
                &note,
            );
            if let Err(e) = self.append(record) {
                log(&format!("{}: cannot write {metric}: {e}", v.name));
            }
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
    fn measure(&mut self, v: &Venture) -> Option<Result<(Option<f64>, String), String>> {
        if v.name == SELF && v.path.is_none() {
            return Some(Ok(crate::selfventure::measure(
                &self.ledger,
                &mut self.checked,
                now(),
            )));
        }
        if let (Some(sim), Some(dir)) = (self.sim.clone(), v.path.as_deref()) {
            let simulated = {
                let index = self.index().ok()?;
                sim.measure(&index, &v.name, &v.mandate, dir)
            };
            if simulated.is_some() {
                return simulated.map(Ok);
            }
        }
        if v.mandate.metric_adapter().is_some() {
            return Some(self.warden.measure(&v.name));
        }
        let dir = v.path.clone()?;
        let report = v.report()?;
        Some(Ok(read_report(&dir, &report)))
    }

    /// Start reading a venture's `report` program on a thread of its own,
    /// when that is how its metric is read: where the reading will come.
    /// None when it is read some other way, here and now.
    fn measure_apart(&mut self, v: &Venture) -> Option<Coming> {
        if (v.name == SELF && v.path.is_none())
            || self.sim.is_some()
            || v.mandate.metric_adapter().is_some()
        {
            return None;
        }
        let dir = v.path.clone()?;
        let report = v.report()?;
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let name = v.name.clone();
        std::thread::spawn(move || {
            let read = crate::groups::within(&name, stopping, || read_report(&dir, &report));
            let _ = tx.send(read);
        });
        Some(Coming::Report(rx, stop))
    }

    /// A paused venture's reading still coming is stopped: its `report`
    /// program is signalled, then killed, and what the warden still reads
    /// for it is cancelled there. Nothing of it is written.
    fn stop_reading(&mut self, name: &str) {
        if let Some(rt) = self.runtime.get_mut(name) {
            if let Some((_, coming)) = rt.measuring.take() {
                if let Coming::Report(_, stop) = coming {
                    stop.store(true, Ordering::SeqCst);
                }
                log(&format!("{name}: paused; stopping its report"));
                rt.held = true;
            }
        }
        let busy = self.warden.groups().iter().any(|(_, v)| v == name);
        if busy {
            // With no lever of its running, what the warden runs for it
            // is a read.
            if !self.busy(name) {
                self.runtime.entry(name.to_string()).or_default().held = true;
            }
            if let Err(e) = self.warden.cancel(name) {
                log(&format!(
                    "{name}: paused, and cannot stop what the warden runs for it: {e}"
                ));
            }
        }
    }

    /// Take a reading that came in from a `report` program running apart,
    /// and write and act on it. True while one is still coming.
    fn reading_in(&mut self, name: &str, halted: bool) -> bool {
        let Some(rt) = self.runtime.get_mut(name) else {
            return false;
        };
        let got = match &rt.measuring {
            None => return false,
            Some((_, Coming::Report(coming, _))) => coming.try_recv(),
            Some((_, Coming::Warden)) => match self.warden.measure(name) {
                Err(e) if warden::pending(&e) => return true,
                Err(e) => Ok((None, format!("cannot measure: {e}"))),
                Ok(m) => Ok(m),
            },
        };
        let Some(rt) = self.runtime.get_mut(name) else {
            return false;
        };
        match got {
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                rt.measuring = None;
                false
            }
            Ok((value, note)) => {
                rt.measuring = None;
                if let Some(v) = self.ventures.get(name).cloned() {
                    self.reported(&v, halted, value, note);
                }
                false
            }
        }
    }

    /// The intents the last command started apart, whose outcomes are
    /// still to come.
    pub fn take_started(&mut self) -> Vec<Seq> {
        std::mem::take(&mut self.started)
    }

    /// Does a venture have a lever running apart?
    fn busy(&self, name: &str) -> bool {
        self.inflight.values().any(|f| f.venture == name)
    }

    /// Keep each intent a start wrote with no outcome yet: its run goes on
    /// apart.
    fn track(&mut self, written: &[Entry]) {
        for e in written {
            // The warden's own word on what it wrote, kept at once: a
            // warden killed before it is next asked takes its notes with it.
            if self.vetted.is_some() {
                self.ours.insert(e.seq, e.hash.clone());
            }
            if let Record::Intent {
                venture, action, ..
            } = &e.record
            {
                let done = written.iter().any(
                    |o| matches!(&o.record, Record::Outcome { intent, .. } if *intent == e.seq),
                );
                if !done {
                    self.started.push(e.seq);
                    self.inflight.insert(
                        e.seq,
                        Inflight {
                            venture: venture.clone(),
                            action: action.clone(),
                            since: Instant::now(),
                            cancelled: false,
                        },
                    );
                }
            }
        }
    }

    /// Forget the runs going on apart whose outcome is now in the ledger.
    fn settle_inflight(&mut self) {
        if self.inflight.is_empty() {
            return;
        }
        let pending: BTreeSet<Seq> = match self.index() {
            Ok(index) => index.pending().map(|(s, _, _)| s).collect(),
            Err(_) => return,
        };
        self.inflight.retain(|s, _| pending.contains(s));
    }

    /// A paused venture's levers running apart are stopped: the warden
    /// signals each and writes its outcome.
    fn cancel_inflight(&mut self, name: &str) {
        let todo: Vec<Seq> = self
            .inflight
            .iter()
            .filter(|(_, f)| f.venture == name && !f.cancelled)
            .map(|(s, _)| *s)
            .collect();
        if todo.is_empty() {
            return;
        }
        match self.warden.cancel(name) {
            Ok(_) => {
                for s in todo {
                    if let Some(f) = self.inflight.get_mut(&s) {
                        log(&format!(
                            "{name}: paused; stopping {} (intent #{s})",
                            f.action
                        ));
                        f.cancelled = true;
                    }
                }
            }
            Err(e) => log(&format!("{name}: paused, and cannot stop its lever: {e}")),
        }
    }

    /// See that the warden and roles processes are alive. One found dead
    /// is said, recorded as a measured report of self's, and started
    /// again; a lever it was running is left to `recover`, which asks
    /// whether it happened. Starting one again that fails is tried again,
    /// each wait twice the last, and status says so meanwhile.
    fn watch(&mut self) {
        for which in ["warden", "roles"] {
            let gone = match which {
                "warden" => self.warden.alive(),
                _ => self.role.alive(),
            };
            let Err(why) = gone else { continue };
            let now_i = Instant::now();
            if !self.dead.contains_key(which) {
                log(&format!("the {which} process died: {why}"));
                self.dead.insert(
                    which,
                    Dead {
                        why: why.clone(),
                        retry_at: now_i,
                        backoff: RESPAWN_BACKOFF,
                    },
                );
                if which == "warden" {
                    // What it started outlives it, and their timers died
                    // with it: each group it told of is ended here, SIGTERM
                    // then SIGKILL after the grace, on a thread of its own.
                    let groups: Vec<i32> =
                        self.warden.groups().into_iter().map(|(g, _)| g).collect();
                    if !groups.is_empty() {
                        log(&format!(
                            "ending {} process {} the dead warden started",
                            groups.len(),
                            if groups.len() == 1 { "group" } else { "groups" }
                        ));
                    }
                    crate::groups::end_apart(groups, warden::DRAIN_GRACE);
                    // Its runs died with it: each intent is settled the
                    // way a restart settles one, never run again blindly.
                    for (s, f) in std::mem::take(&mut self.inflight) {
                        log(&format!(
                            "{}: {} (intent #{s}) was running in the dead warden",
                            f.venture, f.action
                        ));
                    }
                }
            }
            let Some(dead) = self.dead.get_mut(which) else {
                continue;
            };
            if now_i < dead.retry_at {
                continue;
            }
            let respawned = match which {
                "warden" => self.warden.respawn(),
                _ => self.role.respawn(),
            };
            match respawned {
                Err(e) => {
                    log(&format!("{e}; trying again in {}s", dead.backoff.as_secs()));
                    dead.retry_at = now_i + dead.backoff;
                    dead.backoff = (dead.backoff * 2).min(MAX_BACKOFF);
                }
                Ok(pid) => {
                    let why = dead.why.clone();
                    self.dead.remove(which);
                    if which == "warden" {
                        WARDEN_PID.store(pid as i32, Ordering::SeqCst);
                    }
                    log(&format!(
                        "the {which} process respawned; warden pid {}, roles pid {}",
                        self.warden.pid(),
                        self.role.pid()
                    ));
                    let record = Record::report(
                        Claim::Measured,
                        SELF,
                        which,
                        None,
                        &format!("the {which} process died ({why}); respawned as pid {pid}"),
                    );
                    if let Ok(e) = self.append(record) {
                        log(&e.line());
                    }
                }
            }
        }
    }

    /// What a human must know before any venture: the ledger failing, a
    /// warden or roles process dead. Empty when all is well.
    fn alarms(&self) -> String {
        let mut out = String::new();
        if !self.foreign.is_empty() {
            out.push_str(&format!(
                "caution: records {} were not written by this avsmd while it ran; no lever runs \
                 and every venture is paused (reports go on: they are measurements, and nothing \
                 acts on them). read them (avsm why {}), then: avsm resume self\n",
                seqs(&self.foreign),
                self.foreign[0]
            ));
        }
        if let Some(f) = &self.fault {
            out.push_str(&format!(
                "caution: the ledger cannot be written ({}): {}; nothing runs until it can\n",
                ago(now() - f.since),
                f.error
            ));
        }
        for (which, d) in &self.dead {
            out.push_str(&format!(
                "caution: the {which} process is gone ({}); avsmd is starting it again\n",
                d.why
            ));
        }
        out
    }

    /// The daemon's part of `avsm doctor`: its pid, then one line per
    /// check, `ok <what>` or `fail <what>`.
    fn doctor(&mut self) -> String {
        let mut out = format!("daemon {}\n", std::process::id());
        let checks = [
            ("warden", self.warden.alive(), self.warden.pid()),
            ("roles", self.role.alive(), self.role.pid()),
        ];
        for (which, alive, pid) in checks {
            match alive {
                Ok(()) if pid == 0 => {}
                Ok(()) => out.push_str(&format!("ok {which} pid {pid}\n")),
                Err(why) => out.push_str(&format!(
                    "fail the {which} process is gone ({why}); avsmd is respawning it\n"
                )),
            }
        }
        if let Some(f) = &self.fault {
            out.push_str(&format!("fail the ledger cannot be written: {}\n", f.error));
        }
        if let Some(first) = self.foreign.first() {
            out.push_str(&format!(
                "alarm {first} records {} were not written by this avsmd while it ran\n",
                seqs(&self.foreign)
            ));
        }
        out
    }

    /// Count from here on what this kernel and its warden write: every
    /// later record must be one of theirs. The daemon alone does this; a
    /// home is written by one avsmd at a time, so any other writer while
    /// it runs is foreign.
    pub fn guard_foreign(&mut self) -> Result<(), String> {
        let head = self.ledger_last();
        self.warden.written()?;
        self.vetted = Some(head);
        self.ours.clear();
        Ok(())
    }

    /// Check every record appended since the last check against what this
    /// kernel and its warden wrote. One neither wrote is foreign: it is
    /// never acted on, self writes a measured alarm citing it (which
    /// spends any approval among them) and pauses everything, and stderr
    /// and status say so until a human resumes self. True when nothing
    /// foreign is unanswered.
    fn vet(&mut self) -> bool {
        let Some(from) = self.vetted else {
            return true;
        };
        let after = match self.ledger.after(from) {
            Ok(after) => after,
            Err(e) => {
                log(&format!("cannot read the ledger to vet it: {e}"));
                return false;
            }
        };
        let Some(last) = after.last().map(|e| e.seq) else {
            return self.foreign.is_empty();
        };
        let unknown = |ours: &BTreeMap<Seq, String>| -> Vec<Seq> {
            after
                .iter()
                .filter(|e| ours.get(&e.seq) != Some(&e.hash))
                .map(|e| e.seq)
                .collect()
        };
        if !unknown(&self.ours).is_empty() {
            match self.warden.written() {
                Ok(theirs) => self.ours.extend(theirs),
                Err(e) => {
                    // Asked again at the next look; nothing runs till then.
                    log(&format!("cannot ask the warden what it wrote: {e}"));
                    return false;
                }
            }
        }
        let foreign = unknown(&self.ours);
        self.vetted = Some(last);
        self.ours.retain(|s, _| *s > last);
        if !foreign.is_empty() {
            self.alarm_foreign(foreign);
        }
        self.foreign.is_empty()
    }

    /// Records this daemon did not write are in its ledger: say so loudly,
    /// write self's alarm citing them, and pause everything.
    fn alarm_foreign(&mut self, foreign: Vec<Seq>) {
        let list = seqs(&foreign);
        log(&format!(
            "ALARM: records {list} were not written by this avsmd: another process wrote the \
             ledger while it runs. nothing they say is acted on and every venture is paused; \
             read them (avsm why {}), then: avsm resume self",
            foreign[0]
        ));
        self.foreign.extend(&foreign);
        let alarm = Record::Report {
            claim: Claim::Measured,
            venture: SELF.to_string(),
            metric: crate::index::FOREIGN.to_string(),
            value: Some(foreign.len() as f64),
            note: format!("records {list} were not written by this avsmd while it ran"),
            source: None,
            confidence: None,
            cites: foreign,
            implausible: None,
            facts: None,
        };
        let pause = Record::Decision {
            claim: Claim::Measured,
            venture: SELF.to_string(),
            verdict: Verdict::Pause,
            by: None,
        };
        for record in [alarm, pause] {
            match self.append(record) {
                Ok(e) => log(&e.line()),
                Err(e) => log(&format!("cannot write the alarm: {e}")),
            }
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
        if !self.vet() {
            // Fail closed: a record another process wrote may be among
            // what this run would rest on.
            lines.push(format!(
                "{name}: {action} not run: records {} were not written by this avsmd; \
                 see: avsm status",
                seqs(&self.foreign)
            ));
            return lines;
        }
        if let Some(f) = &self.fault {
            // Fail closed: what cannot be recorded does not run.
            lines.push(format!(
                "{name}: {action} not run: the ledger cannot be written: {}",
                f.error
            ));
            return lines;
        }
        let candidate = Candidate {
            venture: name.to_string(),
            action: action.to_string(),
            cites,
        };
        let adopted = self.adopted();
        // The declared approvals this run rests on. Refused for any reason,
        // they are spent: the refusal cites them, and a changed situation
        // needs a fresh ask, never a rerun of an old approval at the next
        // start (#51).
        let spends: Vec<Seq>;
        // The shared quotas come first: past one, the gate is not asked.
        let over = match self.index() {
            Ok(index) => {
                spends = index.approvals_among(&candidate.cites, name, action);
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
            if self.refuse_citing(name, action, key, detail, limit, spends) {
                log(&lines[0]);
            }
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
                    let may_ask = self.may_ask(&index);
                    let cited: Vec<&Entry> = candidate
                        .cites
                        .iter()
                        .filter_map(|s| index.get(*s))
                        .collect();
                    // The ask carries the payload the lever would consume,
                    // from whichever cited record brought it.
                    let payload = cited.iter().find_map(|e| e.record.payload().cloned());
                    let already = asked_already(&index, name, action, payload.as_ref());
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
        if judged.is_ok() && stopping() {
            // Draining: no lever starts once the daemon is stopping. The
            // proof stays in the ledger and the next start runs the gate
            // again.
            lines.push(format!("{name}: {action} not run: avsmd is stopping"));
            log(lines.last().map_or("", String::as_str));
            return lines;
        }
        // A refusal that only counts toward one already open: stderr said
        // it when it opened, as the ledger did, and says it again only when
        // its reason changes.
        let mut repeat = false;
        match judged {
            Ok(_) => match if self.apart {
                self.warden.start(&candidate)
            } else {
                self.warden.execute(&candidate)
            } {
                Ok(written) => {
                    lines.extend(written.iter().map(Entry::line));
                    self.track(&written);
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
                    // record, no intent, nothing run. It cites what
                    // authorized the run, which it spends: an approval is
                    // used once, and a changed situation needs a fresh one.
                    lines.push(format!("{name}: {action} refused: {detail}"));
                    repeat = !self.refuse_citing(
                        name,
                        action,
                        reason_key,
                        detail,
                        false,
                        candidate.cites.clone(),
                    );
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
            Err((refusal, ..)) if !spends.is_empty() => {
                // An approval the gate refuses is spent, whatever the
                // reason: no ask here, and the next cycle asks afresh when
                // the situation allows.
                let (key, detail, limit) = refusals::describe(&refusal.reasons, &[]);
                lines.push(format!("{name}: {action} refused: {detail}"));
                repeat = !self.refuse_citing(name, action, key, detail, limit, spends);
            }
            Err((refusal, already, may_ask, payload, reason)) => {
                // No ask is written for a lever the gate refuses for a
                // reason an approval of this run does not cure: a pause, a
                // guard, a cap, a budget, a floor. An approval could not
                // run it anyway. A suspended lever is the one exception
                // handled below: refused, and asked about once an episode.
                let blocked = refusal.reasons.iter().any(|r| {
                    !r.cured_by_approval()
                        && !matches!(
                            r,
                            Reason::NeedsDecision {
                                suspended: true,
                                ..
                            }
                        )
                });
                let needs_human = !blocked
                    && refusal
                        .reasons
                        .iter()
                        .any(|r| matches!(r, Reason::NeedsDecision { .. }));
                // A lever found ineffective is refused, never asked about
                // run by run: its one ask an episode is the signal, which
                // asks again only on new evidence.
                let suspended = refusal.reasons.iter().any(|r| {
                    matches!(
                        r,
                        Reason::NeedsDecision {
                            suspended: true,
                            ..
                        }
                    )
                });
                // A payload lever's check runs before its ask is written: a
                // payload it refuses is a refusal record and no ask. It
                // runs again at action time.
                let checked = match (&payload, needs_human && !suspended && !already && may_ask) {
                    (Some(p), true) => self.precheck(name, action, p),
                    _ => None,
                };
                if needs_human && suspended {
                    let (key, detail, limit) =
                        refusals::describe(&refusal.reasons, &[refusals::SUSPENDED]);
                    lines.push(format!("{name}: {action} refused: {detail}"));
                    repeat = !self.refuse(name, action, key, detail, limit);
                    self.reask_ineffective(name, action);
                } else if let Some(line) = checked {
                    // The check logged it, when it opened.
                    lines.push(line);
                    repeat = true;
                } else if needs_human && !already && !may_ask {
                    lines.push(format!(
                        "{name}: {action} needs a decision, but the asks quota is used up; not asking"
                    ));
                    let (key, detail, limit) =
                        refusals::describe_gate(&refusal.reasons, false, true);
                    repeat = !self.refuse(name, action, key, detail, limit);
                } else if needs_human && !already {
                    let question = match &payload {
                        Some(_) => format!("may I {action} with this?{reason}"),
                        None => format!("may I {action}? it is irreversible{reason}"),
                    };
                    // A model's proposal gives the reason, labeled inferred;
                    // a measured or declared one is a premise.
                    let model_reason = {
                        let index = self.index().ok();
                        index.and_then(|index| {
                            candidate
                                .cites
                                .iter()
                                .find_map(|s| match &index.get(*s)?.record {
                                    Record::Proposal {
                                        claim: Claim::Inferred,
                                        reason,
                                        ..
                                    } => Some(reason.clone()),
                                    _ => None,
                                })
                        })
                    };
                    let packet = self.packet(
                        name,
                        Some(action),
                        &candidate.cites,
                        model_reason,
                        payload.is_some(),
                    );
                    match self.append(Record::Ask {
                        cites: ask_cites,
                        claim,
                        venture: name.to_string(),
                        action: Some(action.to_string()),
                        question,
                        refusal: Some(refusal),
                        payload,
                        signal: None,
                        packet,
                    }) {
                        Ok(e) => lines.push(e.line()),
                        Err(e) => lines.push(format!("{name}: cannot write ask: {e}")),
                    }
                } else if needs_human {
                    lines.push(format!("{name}: {action} is already asked; see: avsm asks"));
                    let (key, detail, limit) =
                        refusals::describe_gate(&refusal.reasons, true, false);
                    repeat = !self.refuse(name, action, key, detail, limit);
                } else {
                    let (key, detail, limit) = refusals::describe(&refusal.reasons, &[]);
                    lines.push(format!("{name}: {action} refused: {detail}"));
                    repeat = !self.refuse(name, action, key, detail, limit);
                }
            }
        }
        for line in lines.iter().filter(|_| !repeat) {
            log(line);
        }
        lines
    }

    /// The packet of an ask about `action` (or about nothing in
    /// particular), built here for every ask a lever or a model raises.
    /// `cites` are the records it rests on; `reason` is a model's, if any.
    /// With forecasts on, a short forecast of approving and of denying is
    /// run for a lever; otherwise each option says no forecast.
    pub(crate) fn packet(
        &self,
        venture: &str,
        action: Option<&str>,
        cites: &[Seq],
        reason: Option<String>,
        has_payload: bool,
    ) -> Option<Packet> {
        let v = self.ventures.get(venture)?;
        let takes = action
            .and_then(|a| v.mandate.permitted(a))
            .and_then(Permitted::takes);
        let (mut approve, mut deny) = (None, None);
        if let Some(a) = action {
            if let (true, true, Some(settings)) =
                (a != DELEGATE, self.forecasts, self.config.packet_forecast())
            {
                let mut opts = crate::forecast::Options::new(vec![venture.to_string()], settings);
                // A packet reads approve-all against deny-all, and the
                // same of every ask of the venture: one forecast per
                // reading of its metric serves them all.
                opts.policies = vec![crate::forecast::Policy::All, crate::forecast::Policy::None];
                let reading = self.index().ok().and_then(|i| {
                    i.series(venture, v.mandate.metric_name())
                        .last()
                        .map(|(s, _)| *s)
                });
                let cached = reading.and_then(|r| {
                    self.packet_forecasts
                        .borrow()
                        .get(venture)
                        .filter(|(at, _)| *at == r)
                        .map(|(_, o)| o.clone())
                });
                let ran = match cached {
                    Some(o) => Ok(o),
                    None => self
                        .index()
                        .and_then(|i| crate::forecast::run_in(&i, &self.ledger, &self.home, &opts))
                        .inspect(|o| {
                            if let Some(r) = reading {
                                self.packet_forecasts
                                    .borrow_mut()
                                    .insert(venture.to_string(), (r, o.clone()));
                            }
                        }),
                };
                match ran {
                    Ok(outcome) => {
                        approve = packet::option_forecast(
                            &outcome,
                            venture,
                            crate::forecast::Policy::All,
                        );
                        deny = packet::option_forecast(
                            &outcome,
                            venture,
                            crate::forecast::Policy::None,
                        );
                    }
                    Err(e) => log(&format!("{venture}: no forecast of {a}: {e}")),
                }
            }
        }
        let index = self.index().ok()?;
        let adopted = self.adopted();
        Some(packet::build(
            &index,
            &adopted,
            now(),
            &packet::Basis {
                venture,
                action,
                editable: has_payload || takes.is_some(),
                cites: cites.to_vec(),
                metric: Some(v.mandate.metric_name()),
                reason,
                approve,
                deny,
            },
        ))
    }

    /// What a packet-sized forecast says the venture's metric comes to
    /// under each policy for the asks, all, none and mixed, for a capacity
    /// ask. One per reading of the metric: it goes in the packet cache, so
    /// the lever asks of the same reading reuse it, and a cached one of
    /// the same reading that has every policy is reused here. Empty when
    /// forecasts are off or it fails.
    fn policy_forecasts(&self, v: &Venture) -> Vec<crate::record::PolicyForecast> {
        let (true, Some(settings)) = (self.forecasts, self.config.packet_forecast()) else {
            return vec![];
        };
        let reading = self.index().ok().and_then(|i| {
            i.series(&v.name, v.mandate.metric_name())
                .last()
                .map(|(s, _)| *s)
        });
        let every = |o: &crate::forecast::Outcome| {
            crate::forecast::Policy::EVERY
                .iter()
                .all(|p| o.summaries.iter().any(|s| s.policy == *p))
        };
        let cached = reading.and_then(|r| {
            self.packet_forecasts
                .borrow()
                .get(&v.name)
                .filter(|(at, o)| *at == r && every(o))
                .map(|(_, o)| o.clone())
        });
        let outcome = match cached {
            Some(o) => o,
            None => {
                let mut opts = crate::forecast::Options::new(vec![v.name.clone()], settings);
                opts.policies = crate::forecast::Policy::EVERY.to_vec();
                let ran = self
                    .index()
                    .and_then(|i| crate::forecast::run_in(&i, &self.ledger, &self.home, &opts));
                match ran {
                    Ok(o) => {
                        if let Some(r) = reading {
                            self.packet_forecasts
                                .borrow_mut()
                                .insert(v.name.clone(), (r, o.clone()));
                        }
                        o
                    }
                    Err(e) => {
                        log(&format!("{}: no forecast by policy: {e}", v.name));
                        return vec![];
                    }
                }
            }
        };
        packet::policy_forecasts(&outcome, &v.name, &v.mandate)
    }

    /// The packet of an ask the kernel raises on its own.
    pub(crate) fn signal_packet(&self, venture: &str, signal: &Signal) -> Option<Packet> {
        self.signal_packet_citing(venture, signal, &[])
    }

    /// `signal_packet`, its premises also the measured and declared
    /// records among `cites`.
    pub(crate) fn signal_packet_citing(
        &self,
        venture: &str,
        signal: &Signal,
        cites: &[Seq],
    ) -> Option<Packet> {
        let metric = self
            .ventures
            .get(venture)?
            .mandate
            .metric_name()
            .to_string();
        let index = self.index().ok()?;
        Some(packet::signal_packet_citing(
            &index, venture, signal, &metric, cites,
        ))
    }

    /// Run a payload lever's check on the payload an ask would carry. When
    /// it says no, write the PayloadCheck refusal with the check's output
    /// as its detail and return the line: no ask is written. A check that
    /// cannot run is not a refusal; the ask goes on, and the check runs
    /// again at action time.
    pub(crate) fn precheck(
        &mut self,
        name: &str,
        action: &str,
        payload: &Payload,
    ) -> Option<String> {
        match self.warden.check(name, action, payload) {
            Err(Failure::Refused { reason_key, detail }) => {
                let line = format!("{name}: {action} refused: {detail}; not asking");
                if self.refuse(name, action, reason_key, detail, false) {
                    log(&line);
                }
                Some(line)
            }
            Err(Failure::Error(e)) => {
                log(&format!("{name}: cannot run the check of {action}: {e}"));
                None
            }
            Ok(()) => None,
        }
    }

    /// Would the gate refuse `action` for a reason an approval of this run
    /// does not cure? Then the refusal is recorded and no ask is written;
    /// the line says so. The one place every writer of a ranked or backend
    /// ask asks first.
    pub(crate) fn unaskable(&mut self, name: &str, action: &str, cites: &[Seq]) -> Option<String> {
        let refusal = {
            let index = self.index().ok()?;
            let adopted = self.adopted();
            let candidate = Candidate {
                venture: name.to_string(),
                action: action.to_string(),
                cites: cites.to_vec(),
            };
            let t = now();
            let over =
                gate::quota_check_in(&index, &adopted, &self.config.quotas, name, action, t).1;
            if over.is_empty() {
                gate::gate_in(&index, &candidate, &adopted, &self.config.quotas, t)
                    .err()?
                    .reasons
            } else {
                over
            }
        };
        let suspended = refusal.iter().any(|r| {
            matches!(
                r,
                Reason::NeedsDecision {
                    suspended: true,
                    ..
                }
            )
        });
        if !suspended && refusal.iter().all(Reason::cured_by_approval) {
            return None;
        }
        let extra: &[&str] = if suspended {
            &[refusals::SUSPENDED]
        } else {
            &[]
        };
        let (key, detail, limit) = refusals::describe(&refusal, extra);
        let line = format!("{name}: {action} refused: {detail}; not asking");
        if self.refuse(name, action, key, detail, limit) {
            log(&line);
        }
        if suspended {
            self.reask_ineffective(name, action);
        }
        Some(line)
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
        let pending: Vec<(Seq, String, String, bool)> = {
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
                        // An approval written before the lever was suspended
                        // vouched for a run the lever was still trusted
                        // for; it is not a decision to run it now.
                        let stale = index.suspended(venture, &action)
                            && index.last_suspension(venture, &action) > Some(*ask);
                        Some((d, venture.clone(), action, stale))
                    }
                    _ => None,
                })
                // A lever whose last run has no outcome waits: its approval
                // is not spent, and runs on the tick after it is settled.
                .filter(|(_, venture, action, _)| index.pending_of(venture, action).is_none())
                .collect()
        };
        for (seq, venture, action, stale) in pending {
            self.handled.insert(seq);
            if stale {
                let (key, detail, limit) = refusals::describe(
                    &[Reason::NeedsDecision {
                        action: action.clone(),
                        suspended: true,
                    }],
                    &[refusals::SUSPENDED],
                );
                let line = format!(
                    "{venture}: {action} refused: approved #{seq} predates the suspension; {detail}"
                );
                // Refused, and so spent: the refusal cites it.
                if self.refuse_citing(&venture, &action, key, detail, limit, vec![seq]) {
                    log(&line);
                }
                continue;
            }
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
                Controller::Threshold { .. } | Controller::Band { .. } | Controller::Plan { .. }
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
            Controller::Threshold { .. } | Controller::Band { .. } | Controller::Plan { .. } => {
                return
            }
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
                // The child's metric, as status shows it: not a guard's
                // reading, an expectation, or any other report of it.
                let own = child.mandate.metric_name();
                let metric = theirs
                    .iter()
                    .rev()
                    .find(|e| matches!(&e.record, Record::Report { metric: m, .. } if m == own))
                    .map_or("no report yet".to_string(), |e| {
                        format!("{}  reported {}", e.record.summary(), ago(now() - e.at))
                    });
                let lines = child_lines(&theirs, own, CHILD_LINES);
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
                    // No ask for a lever the gate refuses for a reason an
                    // approval does not cure, nor for a payload its check
                    // refuses: the refusal is the record.
                    if let Some(a) = &action {
                        if self.unaskable(name, a, &[]).is_some()
                            || payload
                                .as_ref()
                                .and_then(|p| self.precheck(name, a, p))
                                .is_some()
                        {
                            continue;
                        }
                    }
                    let packet = self.packet(
                        name,
                        action.as_deref(),
                        &[],
                        Some(question.clone()),
                        payload.is_some(),
                    );
                    Record::Ask {
                        cites: vec![],
                        claim: Claim::Inferred,
                        venture: name.to_string(),
                        action,
                        question,
                        refusal: None,
                        signal: None,
                        payload,
                        packet,
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
            if self.refuse(
                name,
                "ask",
                "AsksQuotaSpent".to_string(),
                format!("the asks quota is used up; not asking: {question}"),
                false,
            ) {
                log(&format!(
                    "{name}: the asks quota is used up; not asking: {question}"
                ));
            }
            return;
        }
        let packet = self.packet(name, None, &[], None, false);
        match self.append(Record::Ask {
            cites: vec![],
            claim: Claim::Measured,
            venture: name.to_string(),
            action: None,
            question: question.to_string(),
            refusal: None,
            signal: None,
            payload: None,
            packet,
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
        // SAFETY: kill has no memory preconditions; the run leads a group.
        unsafe {
            libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
        if !crate::groups::alive(child.id() as i32) {
            crate::groups::ended(child.id() as i32);
        }
    }
    rt.started = None;
}

/// Run a venture's `report` program and read its last line as the
/// reading, or None with a note saying why not.
fn read_report(dir: &Path, report: &Path) -> (Option<f64>, String) {
    let ran = warden::run(dir, &report.to_string_lossy(), &[], "", REPORT_TIMEOUT);
    if ran.unstarted {
        return (None, format!("report {}", ran.detail(REPORT_TIMEOUT)));
    }
    if !ran.ok {
        return (
            None,
            format!("report failed: {}", ran.detail(REPORT_TIMEOUT)),
        );
    }
    // NaN and infinity parse but mean nothing; they are passed on so the
    // report says so, and are never stored as a value.
    let not_finite = ran
        .stdout
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .and_then(|l| l.trim().parse::<f64>().ok())
        .filter(|n| !n.is_finite());
    match crate::adapter::number_from(&ran.stdout) {
        Ok(n) => (Some(n), String::new()),
        Err(_) if not_finite.is_some() => (
            not_finite,
            format!("report printed {}", not_finite.unwrap_or(f64::NAN)),
        ),
        Err(_) => (
            None,
            format!(
                "report did not print a number: {}",
                ran.detail(REPORT_TIMEOUT)
            ),
        ),
    }
}

/// Why `network: none` is not enforced on this system, naming it.
pub fn network_none_note() -> String {
    let os = match std::env::consts::OS {
        "macos" => "macOS",
        "linux" => "Linux",
        other => other,
    };
    if cfg!(target_os = "linux") {
        format!("network: none is not enforced here; this {os} allows no network namespace, so run has network")
    } else {
        format!("network: none is not enforced here; {os} has no network namespaces, so run has network")
    }
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
    // A session of its own, and so a group: a stop or a pause reaches
    // what it started, and what it leaves behind still shows as avsmd's.
    // SAFETY: the hook only calls setsid, prctl and getppid, which are
    // async-signal-safe.
    unsafe {
        cmd.pre_exec(crate::groups::lead_session);
    }
    let mark = crate::groups::mark();
    cmd.env(crate::groups::MARK_VAR, &mark);
    let _spawning = crate::groups::SPAWNING
        .read()
        .unwrap_or_else(|p| p.into_inner());
    let child = cmd
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", path.display()))?;
    crate::groups::within(name, Arc::new(AtomicBool::new(false)), || {
        crate::groups::started(child.id() as i32, &mark, dir)
    });
    Ok(child)
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

/// Is an ask open for this lever of this venture with this payload? Asks
/// are one per (venture, lever, payload): five different replies are five
/// asks, the same reply twice is one.
pub(crate) fn asked_already(
    index: &Index,
    venture: &str,
    action: &str,
    payload: Option<&Payload>,
) -> bool {
    let key = payload.map(|p| record::key(venture, action, &[], Some(p)));
    index.open_of(venture).any(|q| {
        index.get(q).is_some_and(|e| {
            e.record.action() == Some(action)
                && e.record
                    .payload()
                    .map(|p| record::key(venture, action, &[], Some(p)))
                    == key
        })
    })
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
    // An approval counts as 1, less the share of canary asks its approver
    // approved: someone who approves what can never be right counts for
    // less, so delegating needs more of their approvals.
    let mut by_shape: BTreeMap<String, Vec<(Seq, f64)>> = BTreeMap::new();
    for d in answers.iter().filter(|d| **d > since) {
        let Some(Record::Decision {
            verdict: Verdict::Approve { ask, payload },
            by,
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
        let who = by.as_deref().unwrap_or(crate::index::LOCAL);
        by_shape
            .entry(shape)
            .or_default()
            .push((*d, 1.0 - index.canary_rate(who)));
    }
    by_shape
        .into_iter()
        .find(|(_, s)| s.iter().map(|(_, w)| w).sum::<f64>() + 1e-9 >= n as f64)
        .map(|(shape, s)| (shape, s.into_iter().map(|(d, _)| d).collect()))
}

/// Was anything in this time-ordered list written on this local date?
/// Only times within a few days of now can fall on today.
/// What `avsm forecast` prints: the forecast, then the records written.
fn rendered(outcome: &crate::forecast::Outcome, written: &[Entry]) -> String {
    let mut out = outcome.render();
    for e in written {
        out.push_str(&format!("  {}\n", e.line()));
    }
    out
}

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
fn weekly<'a>(entries: impl Iterator<Item = &'a Entry>, now: i64) -> (u32, u32) {
    let since = now - 7 * Cap::DAY.as_secs() as i64;
    let week = entries.filter(|e| e.at > since && e.at <= now);
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

/// The metric of the latest declared adoption of a venture whose text
/// parses, whatever its hash: what to name when the mandate in force
/// cannot be read.
fn last_adopted_metric(index: &Index, venture: &str) -> Option<String> {
    index.of(venture).rev().find_map(|e| match &e.record {
        Record::Decision {
            claim: Claim::Declared,
            verdict: Verdict::Adopt {
                text: Some(text), ..
            },
            ..
        } => crate::mandate::Mandate::parse(text)
            .ok()
            .map(|m| m.metric_name().to_string()),
        _ => None,
    })
}

/// The digest text: since the last digest when there is one this week,
/// else the last day.
fn digest_text(index: &Index, ventures: &BTreeMap<String, Venture>, now: i64) -> String {
    let entries: Vec<&Entry> = index.entries().collect();
    let day = Cap::DAY.as_secs() as i64;
    // Since the last digest record when there is one this week, by its
    // place in the ledger; else the last day, by time.
    let last_digest = entries
        .iter()
        .copied()
        .rev()
        .filter(|e| e.at <= now)
        .find(|e| matches!(&e.record, Record::Report { venture, metric, .. } if venture == SELF && metric == "digest"))
        .filter(|e| e.at > now - 7 * day);
    let since = last_digest.map_or(now - day, |e| e.at);
    // The window ends now: a record dated after it is not in it yet.
    let window: Vec<&Entry> = entries
        .iter()
        .copied()
        .filter(|e| e.at <= now)
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
    if let Some((n, first, last, latest)) = dated_ahead(entries.iter().copied(), now) {
        out.push_str(&format!(
            "{n} records dated in the future, #{first} to #{last}, up to {}, are not counted\n",
            utc(latest)
        ));
    }
    let mut names: Vec<&String> = ventures.keys().filter(|n| *n != SELF).collect();
    if let Some(me) = ventures.get(SELF) {
        names.push(&me.name);
    }
    let (asks_week, incidents_week) = weekly(entries.iter().copied(), now);
    for name in names.into_iter().filter(|n| ventures.contains_key(*n)) {
        let v = &ventures[name];
        // A mandate that cannot be read names no metric: the last adopted
        // mandate in the ledger that parses does.
        let last_adopted = v
            .problem
            .as_ref()
            .and_then(|_| last_adopted_metric(index, name));
        let metric = last_adopted
            .as_deref()
            .unwrap_or_else(|| v.mandate.metric_name());
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
            .find_map(reading);
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
    out.push_str(&head_line(entries.last().copied()));
    out
}

/// The anchor line: the head as `seq:hash`, the hash cut to its first 16
/// hex digits (64 bits), which a rewrite would have to match by search.
/// Kept off the machine, it catches what the chain cannot: a rewrite by
/// the file's owner, or a rewind (#44).
fn head_line(head: Option<&Entry>) -> String {
    let anchor = match head {
        Some(e) => format!("{}:{}", e.seq, &e.hash[..e.hash.len().min(16)]),
        None => format!("0:{}", &crate::record::GENESIS[..16]),
    };
    format!(
        "head {anchor}; keep it off this machine and check it with: avsm doctor --since {anchor}\n"
    )
}

/// The proof behind a record, from a snapshot of the ledger.
fn why_in(
    index: &Index,
    adopted: &[Adopted],
    quotas: &crate::config::Quotas,
    seq: Seq,
) -> Result<String, String> {
    let entry = index.get(seq).ok_or_else(|| format!("no record #{seq}"))?;
    // Its claim is what another writer put there: say who did not write it.
    if let Some(alarm) = index.alarm_naming(seq) {
        return Ok(format!(
            "{}
  not written by avsmd: the foreign-record alarm #{alarm} named it.              another process wrote it into the ledger while avsmd ran; its claim ({}) is that              process's word, not a human's or avsmd's, and nothing acts on it
               read the alarm: avsm why {alarm}
",
            entry.line(),
            entry.record.claim()
        ));
    }
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
    // An ask a decision answered no longer waits: "Unanswered means no"
    // is for the one still open.
    if let Record::Ask { venture, .. } = &entry.record {
        if !index.is_open(seq) {
            let mut text = gate::why(entry);
            if let Some(rest) = text.strip_suffix(&format!("{}\n", crate::record::UNANSWERED)) {
                text = rest.to_string();
            }
            let decision = index.of(venture).find(|e| {
                e.record.claim().authorizes()
                    && matches!(&e.record, Record::Decision {
                        verdict: Verdict::Approve { ask, .. } | Verdict::Deny { ask, .. }, ..
                    } if *ask == seq)
            });
            match decision {
                Some(d) => {
                    text.push_str(&format!("Answered by #{}: {}\n", d.seq, d.record.summary()))
                }
                None => text.push_str("No longer open; no decision names it.\n"),
            }
            return Ok(text);
        }
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

/// Record numbers as a list: `#4, #9`.
fn seqs(list: &[Seq]) -> String {
    list.iter()
        .map(|s| format!("#{s}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn parse_seq(s: &str) -> Result<Seq, String> {
    s.trim_start_matches('#')
        .parse()
        .map_err(|_| format!("a record number looks like 12 or #12, not {s:?}"))
}

/// A duration in seconds as "3s ago", "5m ago", "2h ago", "3d ago".
pub fn ago(secs: i64) -> String {
    // A record dated after now says so; it never reads as just now.
    if secs < 0 {
        return format!("{} ahead", span(-secs));
    }
    format!("{} ago", span(secs))
}

/// A span of seconds in its largest whole unit: 40s, 3m, 5h, 2d.
fn span(s: i64) -> String {
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86_400)
    }
}

/// A duration as a person reads it: 500ms, 1.5s, 12s, 3m.
fn waiting(d: Duration) -> String {
    if d < Duration::from_secs(1) {
        return format!("{}ms", d.as_millis());
    }
    if d < Duration::from_secs(60) {
        let secs = (d.as_secs_f64() * 10.0).round() / 10.0;
        if secs == secs.trunc() {
            return format!("{}s", secs as i64);
        }
        return format!("{secs}s");
    }
    span(d.as_secs() as i64)
}

/// One line of `tail`: a record dated after now is marked, so the future
/// a clock that ran ahead wrote is never read as the past.
fn tail_line(e: &Entry) -> String {
    let ahead = e.at - now();
    if ahead > 0 {
        format!("{}  [dated {}]\n", e.line(), ago(-ahead))
    } else {
        e.line() + "\n"
    }
}

/// The records dated after `now`: how many, the first and last of them by
/// number, and the latest date. None when there are none.
pub fn dated_ahead<'a>(
    entries: impl Iterator<Item = &'a Entry>,
    now: i64,
) -> Option<(usize, Seq, Seq, i64)> {
    entries
        .filter(|e| e.at > now)
        .fold(None, |acc, e| match acc {
            None => Some((1, e.seq, e.seq, e.at)),
            Some((n, a, b, t)) => Some((n + 1, a.min(e.seq), b.max(e.seq), t.max(e.at))),
        })
}

/// A unix time as `2026-10-29T20:00Z`.
pub fn utc(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0)
        .map_or(t.to_string(), |d| d.format("%Y-%m-%dT%H:%MZ").to_string())
}

pub(crate) fn log(line: &str) {
    if !QUIET.with(std::cell::Cell::get) {
        crate::protocol::elog(&format!("avsmd: {line}"));
    }
}

/// Verbose diagnostics, on with `avsmd -d`.
static DEBUG: AtomicBool = AtomicBool::new(false);

/// Turn verbose diagnostics on, for this process and the ones it starts.
pub fn set_debug() {
    DEBUG.store(true, Ordering::SeqCst);
}

/// A diagnostic only `avsmd -d` prints.
pub(crate) fn debug(line: &str) {
    if DEBUG.load(Ordering::Relaxed) {
        log(line);
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
    /// How it was started, to start it again.
    exe: PathBuf,
    home: PathBuf,
    what: String,
    drop: bool,
    /// What the warden tells the daemon on a channel of its own: the
    /// records it appends and the process groups it starts. Kept across
    /// respawns.
    notes: Option<crate::groups::Notes>,
}

impl Process {
    /// Spawn `avsmd --home <home> <what>` with a clean environment, and
    /// the config file and debug flag this process was given. With `drop`
    /// and root privileges, the child runs as `nobody`.
    pub fn spawn(home: &Path, what: &str, drop: bool) -> Result<Process, String> {
        let exe = std::env::current_exe().map_err(|e| format!("cannot find avsmd: {e}"))?;
        Process::spawn_exe(&exe, home, what, drop)
    }

    /// Spawn a given avsmd binary; see `spawn`. A warden gets a notes
    /// channel at fd 3.
    pub fn spawn_exe(exe: &Path, home: &Path, what: &str, drop: bool) -> Result<Process, String> {
        let notes = (what == "warden").then(crate::groups::Notes::default);
        Process::spawn_with(exe, home, what, drop, notes)
    }

    fn spawn_with(
        exe: &Path,
        home: &Path,
        what: &str,
        drop: bool,
        notes: Option<crate::groups::Notes>,
    ) -> Result<Process, String> {
        let _alone = notes.is_some().then(|| {
            crate::groups::SPAWNING
                .write()
                .unwrap_or_else(|p| p.into_inner())
        });
        let channel = match &notes {
            Some(n) => Some(
                n.attach()
                    .map_err(|e| format!("cannot open the {what}'s notes channel: {e}"))?,
            ),
            None => None,
        };
        let channel_fd = channel.as_ref().map(std::os::fd::AsRawFd::as_raw_fd);
        let build = |drop: bool| {
            let mut cmd = Command::new(exe);
            cmd.arg("--home").arg(home);
            if let Some(file) = crate::config::chosen() {
                cmd.arg("-f").arg(file);
            }
            if DEBUG.load(Ordering::Relaxed) {
                cmd.arg("-d");
            }
            cmd.arg(what)
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
            #[cfg(debug_assertions)]
            if let Some(v) = std::env::var_os("AVSMD_TEST_HOLD_OUTCOME") {
                cmd.env("AVSMD_TEST_HOLD_OUTCOME", v);
            }
            if let Some(fd) = channel_fd {
                // SAFETY: the hook only calls dup2 and fcntl, which are
                // async-signal-safe.
                unsafe {
                    cmd.pre_exec(move || notes_at_fd3(fd));
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
                    "cannot start the {what} process as nobody ({e}); running it as root instead. \
                     Put avsmd somewhere nobody can read, like /usr/local/bin."
                ));
                build(false)
                    .spawn()
                    .map_err(|e| format!("cannot start the {what} process: {e}"))?
            }
            Err(e) => return Err(format!("cannot start the {what} process: {e}")),
        };
        // The child has its end; ours goes, so the channel ends with it.
        std::mem::drop(channel);
        let stdin = child.stdin.take().ok_or("no pipe to the child")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("no pipe from the child")?);
        Ok(Process {
            child,
            stdin,
            stdout,
            exe: exe.to_path_buf(),
            home: home.to_path_buf(),
            what: what.to_string(),
            drop,
            notes,
        })
    }

    /// Is the child alive? Err says how it ended.
    fn alive(&mut self) -> Result<(), String> {
        match self.child.try_wait() {
            Ok(None) => Ok(()),
            Ok(Some(status)) => Err(format!("pid {} {status}", self.child.id())),
            Err(e) => Err(format!("pid {}: {e}", self.child.id())),
        }
    }

    /// Start the child again from the binary it was started from, in
    /// place of this one; the new pid.
    fn respawn(&mut self) -> Result<u32, String> {
        let fresh = Process::spawn_with(
            &self.exe,
            &self.home,
            &self.what,
            self.drop,
            self.notes.clone(),
        )?;
        *self = fresh;
        Ok(self.id())
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

/// In a warden about to start, after fork: its notes channel at fd 3,
/// kept across exec.
fn notes_at_fd3(fd: i32) -> std::io::Result<()> {
    let target = crate::groups::NOTES_FD;
    // SAFETY: dup2 and fcntl are async-signal-safe and touch only fds.
    let r = unsafe {
        if fd == target {
            libc::fcntl(target, libc::F_SETFD, 0)
        } else {
            libc::dup2(fd, target)
        }
    };
    if r == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
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

    /// Spawn from a given avsmd binary.
    pub fn spawn_exe(exe: &Path, home: &Path) -> Result<WardenProcess, String> {
        Process::spawn_exe(exe, home, "warden", false).map(WardenProcess)
    }

    pub fn id(&self) -> u32 {
        self.0.id()
    }
}

impl Warden for WardenProcess {
    fn start(&mut self, candidate: &Candidate) -> Result<Vec<Entry>, Failure> {
        self.0.send(&ToWarden::Start(candidate.clone()))?;
        match self.0.recv::<FromWarden>()? {
            FromWarden::Done(entries) => Ok(entries),
            FromWarden::Refused { reason_key, detail } => {
                Err(Failure::Refused { reason_key, detail })
            }
            FromWarden::Err(e) => Err(e.into()),
            _ => Err("the warden answered the wrong question".to_string().into()),
        }
    }

    /// What the warden said it appended, on its notes channel: heard
    /// whether or not it is still alive (#52).
    fn written(&mut self) -> Result<Vec<(Seq, String)>, String> {
        Ok(self
            .0
            .notes
            .as_ref()
            .map(crate::groups::Notes::take_written)
            .unwrap_or_default())
    }

    fn groups(&mut self) -> Vec<(i32, String)> {
        self.0
            .notes
            .as_ref()
            .map(crate::groups::Notes::groups)
            .unwrap_or_default()
    }

    fn cancel(&mut self, venture: &str) -> Result<u32, String> {
        self.0.send(&ToWarden::Cancel(venture.to_string()))?;
        match self.0.recv::<FromWarden>()? {
            FromWarden::Cancelled(n) => Ok(n),
            FromWarden::Err(e) => Err(e),
            _ => Err("the warden answered the wrong question".to_string()),
        }
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }

    fn alive(&mut self) -> Result<(), String> {
        self.0.alive()
    }

    fn respawn(&mut self) -> Result<u32, String> {
        self.0.respawn()
    }

    fn check(&mut self, venture: &str, action: &str, payload: &Payload) -> Result<(), Failure> {
        self.0.send(&ToWarden::Check(
            venture.to_string(),
            action.to_string(),
            payload.clone(),
        ))?;
        match self.0.recv::<FromWarden>()? {
            FromWarden::Checked => Ok(()),
            FromWarden::Refused { reason_key, detail } => {
                Err(Failure::Refused { reason_key, detail })
            }
            FromWarden::Err(e) => Err(e.into()),
            _ => Err("the warden answered the wrong question".to_string().into()),
        }
    }

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
            FromWarden::Pending(e) | FromWarden::Err(e) => Err(e),
            _ => Err("the warden answered the wrong question".to_string()),
        }
    }

    fn read(&mut self, venture: &str, metric: &str) -> Result<(Option<f64>, String), String> {
        self.0
            .send(&ToWarden::Read(venture.to_string(), metric.to_string()))?;
        match self.0.recv::<FromWarden>()? {
            FromWarden::Measured(value, note) => Ok((value, note)),
            FromWarden::Pending(e) | FromWarden::Err(e) => Err(e),
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

    /// Spawn from a given avsmd binary.
    pub fn spawn_exe(exe: &Path, home: &Path) -> Result<RolesProcess, String> {
        Process::spawn_exe(exe, home, "roles", true).map(RolesProcess)
    }

    pub fn id(&self) -> u32 {
        self.0.id()
    }
}

impl Role for RolesProcess {
    fn pid(&self) -> u32 {
        self.0.id()
    }

    fn alive(&mut self) -> Result<(), String> {
        self.0.alive()
    }

    fn respawn(&mut self) -> Result<u32, String> {
        self.0.respawn()
    }

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

/// How long the warden waits on a read or a measure before it answers
/// that it is still going: the kernel waits on the answer, and holds the
/// tick while it does (#45).
pub const QUICK: Duration = Duration::from_millis(500);

/// A read that ended this long before the kernel asked again is stale:
/// it is dropped, and read afresh.
pub const STALE: Duration = Duration::from_secs(60);

/// What the warden answers while a read is still going.
pub const PENDING: &str = "still running";

/// Work of the warden's going on apart: a lever run, or a read or a
/// measure that took longer than `QUICK`.
struct Apart {
    venture: String,
    stop: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

/// A read or a measure going on apart, and where its answer will come.
struct Reading {
    apart: Apart,
    since: Instant,
    done: std::sync::mpsc::Receiver<(Instant, FromWarden)>,
}

/// Answer a read or a measure: the answer of the one already going for
/// `key` once it is in, else `work` started on a thread of its own and
/// waited on up to `QUICK`. Past that, the answer is that it is still
/// running, and the next ask for `key` takes it up.
fn read_apart(
    readings: &mut BTreeMap<String, Reading>,
    key: String,
    venture: &str,
    work: Result<warden::Work<warden::Measured>, String>,
) -> FromWarden {
    if let Some(r) = readings.get(&key) {
        match r.done.try_recv() {
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                return FromWarden::Pending(format!(
                    "{PENDING} after {}",
                    waiting(r.since.elapsed())
                ));
            }
            Ok((at, answer)) => {
                readings.remove(&key);
                if at.elapsed() < STALE {
                    return answer;
                }
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                readings.remove(&key);
            }
        }
    }
    let work = match work {
        Ok(w) => w,
        Err(e) => return FromWarden::Err(e),
    };
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = {
        let venture = venture.to_string();
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let answer = match crate::groups::within(&venture, stop, work) {
                Ok((value, note)) => FromWarden::Measured(value, note),
                Err(e) => FromWarden::Err(e),
            };
            let _ = tx.send((Instant::now(), answer));
        })
    };
    let since = Instant::now();
    match rx.recv_timeout(QUICK) {
        Ok((_, answer)) => {
            let _ = handle.join();
            answer
        }
        Err(_) => {
            readings.insert(
                key,
                Reading {
                    apart: Apart {
                        venture: venture.to_string(),
                        stop,
                        handle,
                    },
                    since,
                    done: rx,
                },
            );
            FromWarden::Pending(format!("{PENDING} after {}", waiting(QUICK)))
        }
    }
}

/// The body of the warden process: answer the kernel until it hangs up.
pub fn warden_main(home: &Path) -> Result<(), String> {
    // A stop drains: the run in flight is signalled and waited on, and its
    // outcome written; the warden then answers until the kernel hangs up.
    warden::drain_on_signals();
    // The daemon's channel for what this warden appends and starts.
    crate::groups::take_notes();
    // The daemon that started this warden: its groups are written down
    // as its, and nothing is appended once another holds the home (#56).
    // SAFETY: getppid has no preconditions.
    let daemon = unsafe { libc::getppid() };
    if daemon > 1 && crate::groups::noting() {
        crate::lock::guard_writes(home, daemon);
        crate::groups::keep_in(home, crate::groups::Daemon::of(daemon));
        crate::groups::adopt_orphans();
    }
    let mut warden = LocalWarden::open(home)?;
    let ledger = home.join("ledger.db");
    // Lever runs going on apart, each on a thread of its own.
    let mut runs: Vec<Apart> = Vec::new();
    // Reads and measures going on apart, by what they read.
    let mut readings: BTreeMap<String, Reading> = BTreeMap::new();
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        runs.retain(|a| !a.handle.is_finished());
        let reply = match serde_json::from_str::<ToWarden>(line.trim()) {
            Ok(ToWarden::Start(c)) => match warden.run_or_start(&c, true) {
                Ok(Started::Done(e)) => FromWarden::Done(e),
                Ok(Started::Running(e, job)) => {
                    let stop = Arc::new(AtomicBool::new(false));
                    let stopped = Arc::clone(&stop);
                    let ledger = ledger.clone();
                    let venture = job.venture.clone();
                    let notes = warden.notes();
                    let handle = std::thread::spawn(move || {
                        let flag = Arc::clone(&stopped);
                        let (ok, detail, more) =
                            crate::groups::within(&job.venture.clone(), flag, || {
                                job.run(&|| stopped.load(Ordering::SeqCst))
                            });
                        let wrote = Ledger::open(&ledger)
                            .and_then(|mut l| {
                                l.track(notes);
                                for record in std::iter::once(job.outcome(ok, detail)).chain(more) {
                                    l.append(record)?;
                                }
                                Ok(())
                            })
                            .map_err(|e| e.to_string());
                        if let Err(e) = wrote {
                            log(&format!(
                                "warden: cannot write the outcome of intent #{}: {e}",
                                job.intent
                            ));
                        }
                    });
                    runs.push(Apart {
                        venture,
                        stop,
                        handle,
                    });
                    FromWarden::Done(e)
                }
                Err(Failure::Refused { reason_key, detail }) => {
                    FromWarden::Refused { reason_key, detail }
                }
                Err(Failure::Error(e)) => FromWarden::Err(e),
            },
            Ok(ToWarden::Cancel(v)) => {
                // Its reads still going join its lever runs, to be waited
                // on as runs are and asked for afresh; then each is
                // signalled, and killed after the grace, as a drain does.
                let (gone, kept) = std::mem::take(&mut readings)
                    .into_iter()
                    .partition(|(_, r)| r.apart.venture == v);
                readings = kept;
                runs.extend(gone.into_values().map(|r: Reading| r.apart));
                let mut n = 0;
                for a in runs
                    .iter()
                    .filter(|a| a.venture == v && !a.handle.is_finished())
                {
                    a.stop.store(true, Ordering::SeqCst);
                    n += 1;
                }
                FromWarden::Cancelled(n)
            }
            Ok(ToWarden::Execute(c)) => match warden.execute(&c) {
                Ok(e) => FromWarden::Done(e),
                Err(Failure::Refused { reason_key, detail }) => {
                    FromWarden::Refused { reason_key, detail }
                }
                Err(Failure::Error(e)) => FromWarden::Err(e),
            },
            Ok(ToWarden::Check(v, a, p)) => match warden.check(&v, &a, &p) {
                Ok(()) => FromWarden::Checked,
                Err(Failure::Refused { reason_key, detail }) => {
                    FromWarden::Refused { reason_key, detail }
                }
                Err(Failure::Error(e)) => FromWarden::Err(e),
            },
            Ok(ToWarden::Infer(r)) => match warden.infer(&r) {
                Ok(t) => FromWarden::Text(t),
                Err(e) => FromWarden::Err(e),
            },
            Ok(ToWarden::Measure(v)) => {
                let work = warden.measure_work(&v);
                read_apart(&mut readings, format!("measure {v}"), &v, work)
            }
            // No adapter can check a run yet, so confirming starts nothing
            // and is answered here.
            Ok(ToWarden::Confirm(i)) => match warden.confirm(i) {
                Ok(found) => FromWarden::Confirmed(found),
                Err(e) => FromWarden::Err(e),
            },
            Ok(ToWarden::Read(v, m)) => {
                let work = warden.read_work(&v, &m);
                read_apart(&mut readings, format!("read {v} {m}"), &v, work)
            }
            Err(e) => FromWarden::Err(format!("bad request: {e}")),
        };
        let mut text = serde_json::to_string(&reply).map_err(|e| e.to_string())?;
        text.push('\n');
        out.write_all(text.as_bytes())
            .and_then(|_| out.flush())
            .map_err(|e| e.to_string())?;
    }
    // The kernel hung up. Without a stop first, the daemon died: what it
    // started itself (its reports, its runs) has nobody left to end it,
    // and is ended here. Work still going here drains, as on a stop, and
    // each lever's outcome is written before the warden goes.
    warden::daemon_died();
    let orphans = crate::groups::left_by_my_daemon();
    let ending = (!orphans.is_empty()).then(|| {
        std::thread::spawn(move || {
            crate::groups::end(&orphans, Instant::now() + warden::DRAIN_GRACE)
        })
    });
    let left: Vec<Apart> = runs
        .into_iter()
        .chain(readings.into_values().map(|r| r.apart))
        .collect();
    for a in left {
        let _ = a.handle.join();
    }
    if let Some(e) = ending {
        let _ = e.join();
    }
    Ok(())
}

/// The body of the roles process: decide when asked, with backend calls
/// routed back through the kernel to the warden. Holds no keys.
pub fn roles_main() -> Result<(), String> {
    // The kernel decides when roles stop: it hangs up.
    // SAFETY: setting a disposition to SIG_IGN runs no code in the handler.
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
        libc::signal(libc::SIGINT, libc::SIG_IGN);
    }
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

/// How long a stop may take: the tick in flight finishes, and runs and
/// the lever in flight are signalled and waited on this long.
pub const DRAIN: Duration = Duration::from_secs(10);

static STOP: AtomicBool = AtomicBool::new(false);

/// The warden process's pid, for the signal handler to pass a stop on to.
static WARDEN_PID: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(_: libc::c_int) {
    stop_now();
}

/// Stop as SIGTERM does: drain, and pass the stop on to the warden. Only
/// atomics and kill, so a signal handler may call it.
fn stop_now() {
    STOP.store(true, Ordering::SeqCst);
    // The `report` programs this process runs drain as the warden's runs
    // do: SIGTERM now, SIGKILL after the grace.
    warden::drain_now();
    let pid = WARDEN_PID.load(Ordering::SeqCst);
    if pid > 0 {
        // SAFETY: kill is async-signal-safe.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
}

/// Is the daemon draining after SIGTERM or SIGINT?
pub(crate) fn stopping() -> bool {
    STOP.load(Ordering::Relaxed)
}

impl Kernel {
    /// Stop every venture's `run`: SIGTERM, then SIGKILL for any still
    /// running at `deadline`.
    fn drain(&mut self, deadline: Instant) {
        for (name, rt) in self.runtime.iter() {
            if let Some(child) = &rt.run {
                log(&format!("{name}: stopping run"));
                // SAFETY: kill has no memory preconditions.
                unsafe {
                    libc::kill(-(child.id() as libc::pid_t), libc::SIGTERM);
                }
            }
        }
        for rt in self.runtime.values_mut() {
            if let Some(child) = rt.run.as_mut() {
                while Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            kill(rt);
        }
        // The levers running apart drain in the warden, which had the
        // signal too; each outcome is waited for up to the deadline.
        self.settle_inflight();
        while !self.inflight.is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            self.settle_inflight();
        }
        for (s, f) in &self.inflight {
            log(&format!(
                "{}: {} (intent #{s}) has no outcome yet; the next start asks whether it happened",
                f.venture, f.action
            ));
        }
        // Nothing avsmd started outlives it: what this process and the
        // warden started and is still going had its SIGTERM; at the
        // deadline it has SIGKILL.
        let left: Vec<i32> = crate::groups::live()
            .into_iter()
            .chain(self.warden.groups())
            .map(|(g, _)| g)
            .collect();
        let killed = crate::groups::end(&left, deadline);
        if killed > 0 {
            log(&format!(
                "killed {killed} process {} that ignored the stop",
                if killed == 1 { "group" } else { "groups" }
            ));
        }
    }
}

/// Make the home directory, for its owner alone (0700) when it is made
/// here; one that exists is left as it is.
pub fn make_home(home: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(home)
}

/// Run avsmd in the foreground: spawn the warden and the roles process,
/// answer commands on the socket, and tick once a second until SIGTERM or
/// SIGINT; then drain and return.
pub fn start(home: &Path) -> Result<(), Exit> {
    if let Some(e) = crate::protocol::socket_too_long(&socket_path(home)) {
        return Err(Exit::new(exit::USAGE, e));
    }
    make_home(home).map_err(|e| {
        Exit::new(
            exit::CANTCREAT,
            format!("cannot create {}: {e}", home.display()),
        )
    })?;
    let config = load_config(home)?;
    let sock = socket_path(home);
    // One daemon per home, and a ledger it can read, before anything
    // else: a second daemon must write nothing (#44).
    let mut lock = crate::doctor::hold_home(home)?;
    // Nothing this process or its warden appends lands once another
    // avsmd holds the home's lock (#56).
    crate::lock::guard_writes(home, std::process::id() as i32);
    crate::doctor::ledger_before_start(home)?;
    // What a daemon killed outright left running is ended, unless it is
    // still alive to end it itself; each group this one starts is
    // written down for the next.
    let me = crate::groups::Daemon::of(std::process::id() as i32);
    let left = crate::groups::left_by_the_dead(home);
    if !left.is_empty() {
        log(&format!(
            "ending {} process {} a dead avsmd left running",
            left.len(),
            if left.len() == 1 { "group" } else { "groups" }
        ));
        crate::groups::end_apart(left, warden::DRAIN_GRACE);
    }
    crate::groups::keep_in(home, me);
    crate::groups::adopt_orphans();
    let listener = crate::protocol::bind(&sock).map_err(|e| {
        let code = if crate::protocol::alive(&sock) {
            exit::TEMPFAIL
        } else {
            exit::CANTCREAT
        };
        Exit::new(code, e)
    })?;
    let mut ours = crate::lock::identity(&sock);
    let warden = WardenProcess::spawn(home)?;
    let roles = RolesProcess::spawn(home)?;
    log(&format!(
        "warden pid {}, roles pid {}",
        warden.id(),
        roles.id()
    ));
    WARDEN_PID.store(warden.id() as i32, Ordering::SeqCst);
    // SAFETY: the handler only stores to atomics and calls kill.
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
    let mut kernel = Kernel::with(home, Box::new(roles), Box::new(warden))?;
    kernel.forecasts = true;
    kernel.apart = true;
    kernel.adopt_self()?;
    kernel.guard_foreign()?;
    // A venture whose adopted mandate cannot be read does not stop the
    // others: avsmd starts, runs nothing of it, and says so here and in
    // status.
    for v in kernel.ventures.values() {
        if let Some(problem) = &v.problem {
            log(&format!("{}: cannot run: {problem}", v.name));
        }
    }
    let kernel = Arc::new(Mutex::new(kernel));
    log(&format!("listening at {}", sock.display()));
    let server = Arc::clone(&kernel);
    std::thread::spawn(move || crate::protocol::serve(listener, server));
    // Why this daemon stops on its own: another took its home.
    let mut lost: Option<String> = None;
    match crate::tailscale::start(&config, Arc::clone(&kernel)) {
        Ok(Some(at)) => log(&format!(
            "listening on the tailnet at {at} for {}",
            config.remote_users.join(", ")
        )),
        Ok(None) => {}
        Err(e) => log(&e),
    }
    // When the stop came: the tick in flight finishes first.
    let stopped = loop {
        if stopping() {
            break Instant::now();
        }
        if let Err(e) = keep_home(home, &mut lock, &sock, &mut ours, &kernel) {
            log(&format!("ALARM: {e}; stopping"));
            lost = Some(e);
            stop_now();
            break Instant::now();
        }
        {
            let mut k = kernel.lock().unwrap_or_else(|p| p.into_inner());
            let began = Instant::now();
            k.tick();
            debug(&format!("tick took {:?}", began.elapsed()));
        }
        // A second between ticks, in short steps so a stop is seen at once.
        for _ in 0..20 {
            if stopping() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    // No new commands: the socket goes first, and a command already
    // waiting on the kernel is told the daemon is stopping. Only our own
    // socket: one put in its place is another's.
    if ours.is_some() && crate::lock::identity(&sock) == ours {
        let _ = std::fs::remove_file(&sock);
    }
    log("stopping");
    kernel
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .drain(stopped + DRAIN - Duration::from_secs(1));
    log("stopped");
    match lost {
        Some(e) => Err(Exit::new(exit::TEMPFAIL, e)),
        None => Ok(()),
    }
}

/// Once a tick: the home's lock file and socket must still be this
/// daemon's (#52). Deleted, they would let a second avsmd start on the
/// same ledger, and doctor see nothing. A lock file deleted or replaced
/// is taken again: whichever daemon gets the new file's lock first keeps
/// the home, since flock on one file is one daemon's, and this one stops
/// if it lost. A socket deleted or replaced is bound again, once the lock
/// is surely this daemon's.
fn keep_home(
    home: &Path,
    lock: &mut crate::lock::Held,
    sock: &Path,
    ours: &mut Option<(u64, u64)>,
    kernel: &Arc<Mutex<Kernel>>,
) -> Result<(), String> {
    // Said once while the lock file cannot be made again, not each tick.
    static UNMADE: AtomicBool = AtomicBool::new(false);
    let path = home.join(crate::lock::LOCK_FILE);
    if !lock.still_at(&path) {
        match crate::lock::acquire(home) {
            Ok(held) => {
                *lock = held;
                UNMADE.store(false, Ordering::Relaxed);
                log(&format!(
                    "the lock file {} was deleted or replaced; this avsmd holds it again",
                    path.display()
                ));
            }
            Err(crate::lock::Refused::Running(pid)) => {
                let who = pid.map_or("another avsmd".to_string(), |p| format!("avsmd (pid {p})"));
                return Err(format!(
                    "{who} took this home's lock after its lock file {} was deleted or replaced \
                     under this one; two daemons must never write one ledger",
                    path.display()
                ));
            }
            Err(crate::lock::Refused::Io(e)) => {
                if !UNMADE.swap(true, Ordering::Relaxed) {
                    log(&format!(
                        "the lock file {} is gone and cannot be made again: {e}",
                        path.display()
                    ));
                }
                return Ok(());
            }
        }
    }
    if crate::lock::identity(sock) != *ours {
        // The lock is this daemon's, so whatever is at the socket's path
        // is no daemon's of this home: it is put back.
        let _ = std::fs::remove_file(sock);
        match crate::protocol::bind(sock) {
            Ok(listener) => {
                *ours = crate::lock::identity(sock);
                log(&format!(
                    "the socket {} was deleted or replaced; listening there again",
                    sock.display()
                ));
                let server = Arc::clone(kernel);
                std::thread::spawn(move || crate::protocol::serve(listener, server));
            }
            Err(e) => log(&format!("the socket was deleted or replaced, and: {e}")),
        }
    }
    Ok(())
}

/// The config in force, or the exit code that says why not: a file
/// `avsmd -f` named that is not there, or one that does not parse.
pub fn load_config(home: &Path) -> Result<Config, Exit> {
    let path = crate::config::path(home);
    if crate::config::chosen().is_some() && !path.is_file() {
        return Err(Exit::new(
            exit::NOINPUT,
            format!("no config file at {}", path.display()),
        ));
    }
    Config::load(home).map_err(|e| Exit::new(exit::CONFIG, e))
}

/// How many of a child's records its parent's prompt shows.
const CHILD_LINES: usize = 12;

/// A child's recent records for its parent's prompt, oldest first, at most
/// `n`: its metric readings, lever runs and refusals, asks and decisions
/// first; each guard's and audit's readings collapsed to the latest, with
/// how many in a row read the same; other reports only in what is left.
fn child_lines(theirs: &[&Entry], metric: &str, n: usize) -> Vec<String> {
    let side = |m: &str| m.starts_with("guard.") || m.starts_with(crate::mandate::AUDIT_PREFIX);
    let mut first: Vec<(Seq, String)> = Vec::new();
    let mut rest: Vec<(Seq, String)> = Vec::new();
    // Per guard or audit: its latest line, the value it read, how many in
    // a row read the same, and whether that run has ended.
    let mut sides: BTreeMap<String, (Seq, String, Option<f64>, usize, bool)> = BTreeMap::new();
    for e in theirs.iter().rev() {
        match &e.record {
            Record::Report {
                metric: m, value, ..
            } if side(m) => match sides.get_mut(m) {
                None => {
                    sides.insert(m.clone(), (e.seq, e.line(), *value, 1, false));
                }
                Some((_, _, v, same, ended)) => {
                    if !*ended && *v == *value {
                        *same += 1;
                    } else {
                        *ended = true;
                    }
                }
            },
            Record::Report { metric: m, .. } if m != metric => rest.push((e.seq, e.line())),
            _ => first.push((e.seq, e.line())),
        }
    }
    let mut collapsed: Vec<(Seq, String)> = sides
        .into_values()
        .map(|(seq, line, _, same, _)| {
            let line = if same > 1 {
                format!("{line}  (the last {same} readings the same)")
            } else {
                line
            };
            (seq, line)
        })
        .collect();
    collapsed.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
    collapsed.truncate(n / 3);
    let room = n.saturating_sub(collapsed.len());
    let mut out: Vec<(Seq, String)> = first.into_iter().take(room).collect();
    let left = room.saturating_sub(out.len());
    out.extend(rest.into_iter().take(left));
    out.extend(collapsed);
    out.sort_by_key(|(s, _)| *s);
    out.into_iter().map(|(_, l)| l).collect()
}

/// What holds a venture implausible, for its status row: each reading
/// its mandate reads whose latest trusted report is flagged, why, and
/// what clears it.
fn held_implausible(index: &Index, adopted: &[gate::Adopted], venture: &str) -> String {
    let mut out = String::new();
    for (metric, seq) in gate::implausible_held(index, adopted, venture) {
        let why = index
            .get(seq)
            .and_then(|e| e.record.implausible().map(str::to_string))
            .unwrap_or_default();
        let clears = clears_when(&metric, &why);
        let why = why.split(':').next().unwrap_or_default();
        out.push_str(&format!(
            "  (held by {metric} #{seq}, {why}; clears when {clears})"
        ));
    }
    out
}

/// What clears a reading marked implausible for this why, by the rules
/// that marked it.
fn clears_when(metric: &str, why: &str) -> String {
    if why == "missing" || why == "NaN" || why.ends_with("is not a number a report can mean") {
        format!("{metric} reads a number again (see avsm tail for why it fails)")
    } else if why.contains("out of range") {
        format!("{metric} reads inside its range")
    } else if why.starts_with("stuck") {
        format!("{metric} reads a value that differs")
    } else if why.starts_with("flapping") {
        format!("{metric} stops swinging past its step")
    } else if why.starts_with(guards::UNCONFIRMED) {
        format!("the next {metric} reading agrees with it")
    } else {
        format!(
            "{metric} reads within its step of the last trusted reading, or {} readings in a row agree",
            guards::AGREE
        )
    }
}

/// The master caution line: which ventures need a human, or that none do.
/// A paused venture is listed apart: a human paused it, or self's pause
/// holds it and the alarm line says so; it waits for a resume, not an
/// answer.
fn caution(states: &[(&str, gate::State)]) -> String {
    let need: Vec<String> = states
        .iter()
        .filter(|(_, s)| !matches!(s, gate::State::Running | gate::State::Paused))
        .map(|(v, s)| format!("{v} {s}"))
        .collect();
    let paused: Vec<&str> = states
        .iter()
        .filter(|(_, s)| *s == gate::State::Paused)
        .map(|(v, _)| *v)
        .collect();
    let paused = (!paused.is_empty()).then(|| format!("paused: {}", paused.join(", ")));
    if need.is_empty() {
        return match paused {
            Some(p) => format!("{p}\n"),
            None => "all running\n".to_string(),
        };
    }
    let n = need.len();
    let (noun, verb) = if n == 1 {
        ("venture", "needs")
    } else {
        ("ventures", "need")
    };
    let paused = paused.map(|p| format!("; {p}")).unwrap_or_default();
    format!(
        "caution: {n} {noun} {verb} you: {}{paused}\n",
        need.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::precision;

    #[test]
    fn a_childs_lines_prefer_its_metric_runs_and_asks_and_collapse_guard_readings() {
        // Monkey round 3: the parent saw six lines of guard.restarts=0.
        use crate::ledger::Ledger;
        use crate::record::{Claim, Record};
        let mut ledger = Ledger::open_in_memory().unwrap();
        let put = |l: &mut Ledger, r: Record| l.append(r).unwrap();
        put(
            &mut ledger,
            Record::report(Claim::Measured, "svc", "up", Some(1.0), ""),
        );
        for _ in 0..8 {
            put(
                &mut ledger,
                Record::report(Claim::Measured, "svc", "guard.restarts", Some(0.0), ""),
            );
        }
        put(
            &mut ledger,
            Record::report(Claim::Measured, "svc", "up", Some(0.0), ""),
        );
        put(
            &mut ledger,
            Record::Ask {
                packet: None,
                cites: vec![],
                claim: Claim::Measured,
                venture: "svc".into(),
                action: Some("restart".into()),
                question: "may I restart?".into(),
                refusal: None,
                payload: None,
                signal: None,
            },
        );
        for _ in 0..4 {
            put(
                &mut ledger,
                Record::report(Claim::Measured, "svc", "guard.restarts", Some(0.0), ""),
            );
        }
        let entries = ledger.all().unwrap();
        let theirs: Vec<&crate::record::Entry> = entries.iter().collect();
        let lines = super::child_lines(&theirs, "up", 6);
        let guards: Vec<&String> = lines
            .iter()
            .filter(|l| l.contains("guard.restarts"))
            .collect();
        assert_eq!(guards.len(), 1, "{lines:#?}");
        assert!(
            guards[0].contains("the last 12 readings the same"),
            "{lines:#?}"
        );
        assert_eq!(
            lines.iter().filter(|l| l.contains("up=")).count(),
            2,
            "{lines:#?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("may I restart?")),
            "{lines:#?}"
        );
        // Oldest first.
        assert!(lines[0].contains("up=1"), "{lines:#?}");
    }

    #[test]
    fn precision_is_the_most_places_seen_capped_at_six() {
        assert_eq!(precision([24.0, 27.6].into_iter()), 1);
        assert_eq!(precision([1.0, 2.0].into_iter()), 0);
        assert_eq!(precision(std::iter::empty()), 0);
        assert_eq!(precision([0.1234567891].into_iter()), 6);
    }

    #[test]
    fn a_pending_read_names_the_real_elapsed_time_not_whole_seconds() {
        use std::time::Duration;
        assert_eq!(super::waiting(Duration::from_millis(500)), "500ms");
        assert_eq!(super::waiting(Duration::from_millis(999)), "999ms");
        assert_eq!(super::waiting(Duration::from_millis(1500)), "1.5s");
        assert_eq!(super::waiting(Duration::from_secs(12)), "12s");
        assert_eq!(super::waiting(Duration::from_secs(90)), "1m");
    }

    #[test]
    fn the_default_home_is_the_platforms_and_avsmd_home_overrides_it() {
        use super::home_for;
        use std::path::PathBuf;
        fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<std::ffi::OsString> {
            let vars: Vec<(String, String)> = vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            move |name: &str| {
                vars.iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| std::ffi::OsString::from(v))
            }
        }
        let home = [("HOME", "/u/ada")];
        assert_eq!(
            home_for("macos", env(&home)),
            PathBuf::from("/u/ada/Library/Application Support/avsmd")
        );
        assert_eq!(
            home_for("linux", env(&home)),
            PathBuf::from("/u/ada/.local/state/avsmd")
        );
        let xdg = [("HOME", "/u/ada"), ("XDG_STATE_HOME", "/var/st")];
        assert_eq!(home_for("linux", env(&xdg)), PathBuf::from("/var/st/avsmd"));
        // XDG is a Linux convention; macOS keeps its own place.
        assert_eq!(
            home_for("macos", env(&xdg)),
            PathBuf::from("/u/ada/Library/Application Support/avsmd")
        );
        // A relative or empty XDG_STATE_HOME is ignored, as XDG says.
        for bad in [
            [("HOME", "/u/ada"), ("XDG_STATE_HOME", "state")],
            [("HOME", "/u/ada"), ("XDG_STATE_HOME", "")],
        ] {
            assert_eq!(
                home_for("linux", env(&bad)),
                PathBuf::from("/u/ada/.local/state/avsmd")
            );
        }
        let set = [
            ("HOME", "/u/ada"),
            ("XDG_STATE_HOME", "/var/st"),
            ("AVSMD_HOME", "/srv/avsmd"),
        ];
        for os in ["macos", "linux", "freebsd"] {
            assert_eq!(home_for(os, env(&set)), PathBuf::from("/srv/avsmd"));
        }
        // 1.0's variable means nothing now.
        let old = [("HOME", "/u/ada"), ("AVSM_HOME", "/srv/avsm")];
        assert_eq!(
            home_for("linux", env(&old)),
            PathBuf::from("/u/ada/.local/state/avsmd")
        );
    }
}
