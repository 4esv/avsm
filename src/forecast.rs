//! Forecasts: what the ventures would come to over the next days, from
//! forked branches of the ledger, never from the world.
//!
//! Each branch is a copy of the ledger at its head in a temporary home of
//! its own, run by the same kernel and simulate machinery on a clock of
//! its own thread: a keyless warden (no keys file read, no backend called,
//! no adapter read, no payload check run, no mandate file written) sends
//! every lever run to the venture's world model, and reports come from the
//! model's state. Governance is worst-case: a scripted role, seeded,
//! proposes any permitted lever with a placeholder payload, and no model
//! is ever called, so the scenarios come from code. The asks are answered
//! by a policy: all, none or mixed. Branches are never merged back; their
//! homes are deleted after.
//!
//! What comes back into the real ledger: one inferred `forecast` record
//! per venture, variant and policy, size-bounded, and for a venture whose
//! branches reached a wall (an irreversible delegation, a guard breach, a
//! quota or a halt) one measured ask with signal `forecast` showing the
//! envelope. Forecasts are inferred: they rank, veto and ask, and never
//! authorize (non-negotiable 14).
//!
//! Capacity is computed in code, without branches: the metric's drift a
//! day against the most the permitted levers can move it a day.
//!
//! A sweep tries a mandate field at several values, one forecast each;
//! the variants are compared by CSCV's probability of backtest overfitting
//! over the seeds × variants matrix, the trial count being the number of
//! values.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::clock;
use crate::config;
use crate::gate::{DELEGATE, RESUME, SELF, STOP};
use crate::index::Index;
use crate::kernel::Kernel;
use crate::ledger::Ledger;
use crate::mandate::{Controller, Direction, Mandate};
use crate::pbo::{self, Pbo};
use crate::record::{
    self, Claim, Entry, Envelope, Event, Payload, PolicyDay, Quantiles, Reached, Record, Seq,
    Signal, MAX_EVENTS, MAX_EVENT_NAME, MAX_METRICS, MAX_VARIANT,
};
use crate::role::{Context, InferRequest, Role};
use crate::simulate::{self, Human, Simulation};
use crate::venture;
use crate::world::{self, Baseline, MIN_SAMPLES};

/// The longest horizon, in days, and the most seeds per policy.
pub const MAX_DAYS: u32 = 366;
pub const MAX_SEEDS: u32 = 256;
/// The most values a sweep may try.
pub const MAX_VALUES: usize = 16;
/// The shortest step a branch's clock takes: at most one tick an hour,
/// whatever the cadence, so a month of a one-minute venture stays cheap.
pub const STEP_FLOOR: i64 = 3_600;
/// Who the declared records of a branch name: the policy, never a person.
pub const BY: &str = "forecast";
/// The text a placeholder payload carries.
pub const PLACEHOLDER: &str = "forecast placeholder";
/// The base seed when none is given.
pub const SEED: u64 = 0x005E_EDF0_CA57;

/// Who answers the asks in a branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Policy {
    All,
    None,
    Mixed,
}

impl Policy {
    pub const EVERY: [Policy; 3] = [Policy::All, Policy::None, Policy::Mixed];

    pub fn name(self) -> &'static str {
        match self {
            Policy::All => "all",
            Policy::None => "none",
            Policy::Mixed => "mixed",
        }
    }

    fn human(self) -> Human {
        match self {
            Policy::All => Human::ApproveAll,
            Policy::None => Human::Away,
            Policy::Mixed => Human::Mixed,
        }
    }
}

/// A mandate field and the values to try it at.
#[derive(Clone, Debug, PartialEq)]
pub struct Sweep {
    /// A dotted path, like `permitted.water.cap`: a list item by its name.
    pub field: String,
    pub values: Vec<String>,
}

/// What to forecast and how.
#[derive(Clone, Debug)]
pub struct Options {
    /// Ventures by name; empty means every venture but self.
    pub ventures: Vec<String>,
    pub days: u32,
    /// Seeds per policy.
    pub seeds: u32,
    /// The base seed: the same seeds give the same forecast.
    pub seed: u64,
    pub sweep: Option<Sweep>,
    /// Mandate text to run a venture under instead of its adopted one: a
    /// proposed mandate, before its ask is written.
    pub mandates: BTreeMap<String, String>,
    /// Where the branch homes go; the system's temporary directory when
    /// None. They are deleted after, whatever happens.
    pub scratch: Option<PathBuf>,
    /// The policies to run: all three, unless the caller reads fewer.
    pub policies: Vec<Policy>,
}

impl Options {
    pub fn new(ventures: Vec<String>, settings: config::Forecast) -> Options {
        Options {
            ventures,
            days: settings.days,
            seeds: settings.seeds.max(1),
            seed: SEED,
            sweep: None,
            mandates: BTreeMap::new(),
            scratch: None,
            policies: Policy::EVERY.to_vec(),
        }
    }

    /// What the `forecast` lever of a venture runs.
    pub fn for_lever(venture: &str, settings: config::Forecast) -> Options {
        Options::new(vec![venture.to_string()], settings)
    }

    /// `[venture...] [--days N] [--seeds K] [--sweep <field> <v1,v2,...>]`.
    pub fn parse(args: &[&str], settings: config::Forecast) -> Result<Options, String> {
        let mut opts = Options::new(vec![], settings);
        let mut it = args.iter();
        let number = |flag: &str, v: Option<&&str>| -> Result<u32, String> {
            v.and_then(|v| v.parse().ok())
                .ok_or_else(|| format!("{flag} takes a whole number"))
        };
        while let Some(a) = it.next() {
            match *a {
                "--days" => opts.days = number("--days", it.next())?,
                "--seeds" => opts.seeds = number("--seeds", it.next())?,
                "--sweep" => {
                    let field = it.next().ok_or("--sweep takes a field and values")?;
                    let values = it.next().ok_or("--sweep takes a field and values")?;
                    opts.sweep = Some(Sweep {
                        field: field.to_string(),
                        values: values
                            .split(',')
                            .map(|v| v.trim().to_string())
                            .filter(|v| !v.is_empty())
                            .collect(),
                    });
                }
                flag if flag.starts_with("--") => {
                    return Err(format!(
                        "forecast takes [venture...] [--days N] [--seeds K] [--sweep <field> <v1,v2,...>], not {flag}"
                    ))
                }
                name => opts.ventures.push(name.to_string()),
            }
        }
        Ok(opts)
    }

    fn check(&self) -> Result<(), String> {
        if self.policies.is_empty() {
            return Err("a forecast needs at least one policy".to_string());
        }
        if !(1..=MAX_DAYS).contains(&self.days) {
            return Err(format!("--days is 1 to {MAX_DAYS}, not {}", self.days));
        }
        if !(1..=MAX_SEEDS).contains(&self.seeds) {
            return Err(format!("--seeds is 1 to {MAX_SEEDS}, not {}", self.seeds));
        }
        if let Some(s) = &self.sweep {
            if s.values.is_empty() || s.values.len() > MAX_VALUES {
                return Err(format!(
                    "--sweep tries 1 to {MAX_VALUES} values, not {}",
                    s.values.len()
                ));
            }
        }
        Ok(())
    }
}

/// One venture's forecast under one policy, and one variant in a sweep.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub venture: String,
    pub policy: Policy,
    pub variant: Option<String>,
    pub metrics: Vec<Envelope>,
    pub guards: Vec<Event>,
    pub reached: Reached,
}

/// A wall a venture's branches reached.
#[derive(Clone, Debug, PartialEq)]
pub struct Wall {
    pub venture: String,
    /// `irreversible delegation`, `guard`, `quota` or `halt`.
    pub kind: String,
    /// The metric it hit.
    pub metric: String,
    /// The hit metric's p5..p95 at the horizon, under the policy that hit
    /// it first.
    pub envelope: Option<Quantiles>,
    /// The first day each policy hit it, if it did.
    pub days: Vec<PolicyDay>,
}

/// What a forecast found.
#[derive(Clone, Debug)]
pub struct Outcome {
    pub days: u32,
    pub seeds: u32,
    /// Seconds a branch's clock jumped each step.
    pub step: i64,
    /// Branches run, each in a home of its own, all deleted.
    pub branches: usize,
    pub summaries: Vec<Summary>,
    pub walls: Vec<Wall>,
    /// In a sweep: the field, the trial count (the number of values), the
    /// PBO of each policy, and where the outcome distribution changes.
    pub field: Option<String>,
    pub trials: usize,
    pub pbo: Vec<(Policy, Option<Pbo>)>,
    pub marks: Vec<String>,
    /// Runs of the `forecast` lever the branches tried, across all of
    /// them, and forecast records the branches wrote: always 0, since a
    /// branch never forecasts.
    pub tried: usize,
    pub nested: usize,
}

thread_local! {
    /// How deep in forecast branches this thread is: 0 outside any.
    static DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// This thread is running a forecast branch until the guard drops.
struct InBranch(u32);

impl InBranch {
    fn enter() -> InBranch {
        InBranch(DEPTH.with(|d| d.replace(d.get() + 1)))
    }
}

impl Drop for InBranch {
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(self.0));
    }
}

impl Outcome {
    /// The inferred forecast records, each citing `cites`.
    pub fn records(&self, cites: &[Seq]) -> Vec<Record> {
        self.summaries
            .iter()
            .map(|s| Record::Forecast {
                claim: Claim::Inferred,
                venture: s.venture.clone(),
                policy: s.policy.name().to_string(),
                horizon: self.days,
                seeds: self.seeds,
                variant: s.variant.as_deref().map(|v| clip(v, MAX_VARIANT)),
                metrics: s
                    .metrics
                    .iter()
                    .take(MAX_METRICS)
                    .map(|m| Envelope {
                        name: clip(&m.name, MAX_EVENT_NAME),
                        quantiles: m.quantiles,
                    })
                    .collect(),
                guards: s
                    .guards
                    .iter()
                    .take(MAX_EVENTS)
                    .map(|g| Event {
                        name: clip(&g.name, MAX_EVENT_NAME),
                        p: g.p,
                    })
                    .collect(),
                reached: s.reached,
                cites: cites.to_vec(),
            })
            .collect()
    }

    /// One line: what was forecast and the walls.
    pub fn describe(&self) -> String {
        let mut text = format!(
            "{} forecasts over {} days x {} seeds, {} branches",
            self.summaries.len(),
            self.days,
            self.seeds,
            self.branches
        );
        if self.walls.is_empty() {
            text.push_str("; no wall reached");
        } else {
            let walls: Vec<String> = self
                .walls
                .iter()
                .map(|w| format!("{} {} {}", w.venture, w.kind, w.metric))
                .collect();
            text.push_str(&format!("; walls: {}", walls.join(", ")));
        }
        text
    }

    /// The whole of it, for `avsm forecast`.
    pub fn render(&self) -> String {
        let mut out = format!(
            "forecast over {} days x {} seeds per policy, steps of {}s, {} branches forked and deleted\n",
            self.days, self.seeds, self.step, self.branches
        );
        for s in &self.summaries {
            let mut line = format!("  {:<12} {:<5}", s.venture, s.policy.name());
            if let Some(v) = &s.variant {
                line.push_str(&format!(" [{v}]"));
            }
            for m in s.metrics.iter().take(1) {
                line.push_str(&format!(
                    " {} p5 {} p50 {} p95 {}",
                    m.name,
                    record::round(m.quantiles.p5),
                    record::round(m.quantiles.p50),
                    record::round(m.quantiles.p95)
                ));
            }
            for g in &s.guards {
                line.push_str(&format!("; {} breach p={}", g.name, record::round(g.p)));
            }
            line.push_str(&format!(
                "; asks p50 {}, refusals p50 {}, halts p50 {}, delegations p50 {}, spend p50 {}",
                s.reached.asks.p50,
                s.reached.refusals.p50,
                s.reached.halts.p50,
                s.reached.delegations.p50,
                s.reached.spend.p50
            ));
            out.push_str(&line);
            out.push('\n');
        }
        for w in &self.walls {
            out.push_str(&format!(
                "  wall: {} {} {} ({})\n",
                w.venture,
                w.kind,
                w.metric,
                first_days(&w.days)
            ));
        }
        if let Some(field) = &self.field {
            out.push_str(&format!("sweep of {field}: {} trials\n", self.trials));
            for (policy, p) in &self.pbo {
                match p {
                    Some(p) => out.push_str(&format!(
                        "  PBO {} under {} ({} groups, {} splits)\n",
                        record::round(p.pbo),
                        policy.name(),
                        p.groups,
                        p.splits
                    )),
                    None => out.push_str(&format!(
                        "  PBO under {}: too few seeds or values to split\n",
                        policy.name()
                    )),
                }
            }
            for m in &self.marks {
                out.push_str(&format!("  changes: {m}\n"));
            }
        }
        out
    }
}

/// `all day 3, none never, mixed day 5`.
fn first_days(days: &[PolicyDay]) -> String {
    days.iter()
        .map(|d| match d.day {
            Some(n) => format!("{} day {n}", d.policy),
            None => format!("{} never", d.policy),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

// ----- the worst-case role -----

/// The scripted role of a branch: each nightly cycle it proposes, for the
/// ventures being forecast, a seeded pick of their permitted levers, each
/// with a placeholder payload. It never calls a backend.
pub(crate) struct Worst {
    seed: u64,
    ventures: BTreeSet<String>,
}

/// A payload of the shape a lever takes, carrying nothing real.
pub fn placeholder(takes: record::Takes) -> Option<Payload> {
    let text = match takes {
        record::Takes::Json => "{}",
        record::Takes::Diff => {
            "--- a/forecast\n+++ b/forecast\n@@ -0,0 +1 @@\n+forecast placeholder\n"
        }
        record::Takes::File => "forecast-placeholder",
        record::Takes::Text => PLACEHOLDER,
    };
    Payload::parse(takes, text).ok()
}

impl Role for Worst {
    fn decide(
        &mut self,
        ctx: &Context,
        _infer: &mut dyn FnMut(&InferRequest) -> Result<String, String>,
        _drill: &mut dyn FnMut(&str, &str) -> Result<String, String>,
    ) -> Result<Vec<Record>, String> {
        if !self.ventures.contains(&ctx.venture) {
            return Ok(vec![]);
        }
        // NOTE: a plan controller has no nightly cycle: in a branch its
        // proposals come from the planner on each report, as in the
        // kernel, and the keyless warden refuses the backend it asks on
        // a tie, so no model is called there either.
        let levers: Vec<&crate::mandate::Permitted> = ctx.mandate.permitted.iter().collect();
        if levers.is_empty() {
            return Ok(vec![]);
        }
        let mut rng = world::mix(
            world::mix(self.seed, world::seed_of(&ctx.venture)),
            ctx.now as u64,
        );
        let mut order: Vec<usize> = (0..levers.len()).collect();
        for i in (1..order.len()).rev() {
            let j = (world::splitmix64(&mut rng) % (i as u64 + 1)) as usize;
            order.swap(i, j);
        }
        let take = 1 + (world::splitmix64(&mut rng) % levers.len() as u64) as usize;
        Ok(order
            .into_iter()
            .take(take)
            .map(|i| {
                let p = levers[i];
                Record::Proposal {
                    claim: Claim::Inferred,
                    venture: ctx.venture.clone(),
                    action: p.name.clone(),
                    reason: "the forecast's worst-case role proposes any permitted lever"
                        .to_string(),
                    payload: p.takes().and_then(placeholder),
                    cites: vec![],
                }
            })
            .collect())
    }
}

// ----- mandate variants -----

/// Mandate text with a dotted field set to a value: maps by key, lists by
/// an item's name (or position). The result must parse as a mandate.
pub fn set_field(text: &str, field: &str, value: &str) -> Result<String, String> {
    use serde_yaml::Value;
    let mut doc: Value = serde_yaml::from_str(text).map_err(|e| e.to_string())?;
    let parts: Vec<&str> = field.split('.').collect();
    if parts.iter().any(|p| p.is_empty()) {
        return Err(format!(
            "{field:?} is not a field path like permitted.water.cap"
        ));
    }
    let new: Value = serde_yaml::from_str(value).unwrap_or(Value::String(value.to_string()));
    let mut at = &mut doc;
    for (i, part) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        at = match at {
            Value::Mapping(map) => {
                let key = Value::String(part.to_string());
                if last {
                    if !map.contains_key(&key) {
                        return Err(format!("the mandate has no {field}"));
                    }
                    map.insert(key, new);
                    break;
                }
                map.get_mut(&key)
                    .ok_or_else(|| format!("the mandate has no {field}"))?
            }
            Value::Sequence(items) if !last => {
                let pos = items
                    .iter()
                    .position(|item| item.get("name").and_then(Value::as_str) == Some(*part));
                let pos = pos
                    .or_else(|| part.parse::<usize>().ok().filter(|n| *n < items.len()))
                    .ok_or_else(|| format!("the mandate has no {field}"))?;
                &mut items[pos]
            }
            _ => return Err(format!("the mandate has no {field}")),
        };
    }
    let out = serde_yaml::to_string(&doc).map_err(|e| e.to_string())?;
    Mandate::parse(&out).map_err(|e| format!("{field} at {value}: {e}"))?;
    Ok(out)
}

/// One variant: a label in a sweep, and the mandate text each venture
/// runs under when it is not the adopted one.
struct Variant {
    label: Option<String>,
    mandates: BTreeMap<String, String>,
}

// ----- the fork -----

/// The scratch directory of one forecast; removed on drop, whatever
/// happened.
struct Scratch(PathBuf);

impl Scratch {
    fn new(under: Option<&Path>) -> Result<Scratch, String> {
        static N: AtomicU64 = AtomicU64::new(0);
        let base = under.map_or_else(std::env::temp_dir, Path::to_path_buf);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let dir = base.join(format!(
            "avsm-forecast-{}-{nanos}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        Ok(Scratch(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A branch to run: a variant, a policy and a seed.
struct Branch {
    variant: usize,
    policy: Policy,
    seed: u64,
}

/// What every branch shares.
struct Shared<'a> {
    head: PathBuf,
    /// The index of the head: each branch's kernel and warden start
    /// from a copy of it instead of reading the head again.
    index: &'a Index,
    config: Option<String>,
    start: i64,
    end: i64,
    step: i64,
    names: &'a [String],
    variants: &'a [Variant],
    mandates: &'a [BTreeMap<String, Mandate>],
}

/// What a branch came to, venture by venture, or why it failed.
type Traced = Result<BTreeMap<String, Trace>, String>;

/// What one venture came to in one branch.
#[derive(Clone, Debug, Default)]
struct Trace {
    /// The last value of the metric and of each guard.
    last: BTreeMap<String, f64>,
    /// The first day each guard was breached.
    breached: BTreeMap<String, u32>,
    spend: f64,
    asks: f64,
    refusals: f64,
    halts: f64,
    delegations: f64,
    /// The first day each (kind, metric) wall was hit.
    walls: BTreeMap<(String, String), u32>,
    /// Intents of the `forecast` lever, and forecast records, in the
    /// branch.
    tried: usize,
    nested: usize,
}

/// Run a forecast against the ledger at its head. Nothing is written to
/// `source`; the caller writes the records the outcome gives.
pub fn run(source: &Ledger, home: &Path, opts: &Options) -> Result<Outcome, String> {
    let entries = source.all().map_err(|e| e.to_string())?;
    run_in(&Index::from_entries(&entries), source, home, opts)
}

/// Run a forecast as `run` does, given the index of `source` up to its
/// head (the caller's), so that neither this nor any branch reads the
/// whole ledger again: each branch starts from a copy of it.
pub fn run_in(
    index: &Index,
    source: &Ledger,
    home: &Path,
    opts: &Options,
) -> Result<Outcome, String> {
    // The depth guard: whatever path reaches here from inside a branch,
    // a branch never forecasts, so a forecast never recurses.
    if DEPTH.with(std::cell::Cell::get) > 0 {
        return Err("a forecast branch never forecasts".to_string());
    }
    opts.check()?;
    let ventures = venture::from_adoptions(index.adoptions());
    let names: Vec<String> = if opts.ventures.is_empty() {
        ventures
            .values()
            .filter(|v| v.name != SELF && v.problem.is_none() && v.path.is_some())
            .map(|v| v.name.clone())
            .collect()
    } else {
        for n in &opts.ventures {
            match ventures.get(n) {
                None => return Err(format!("no venture named {n}")),
                Some(v) if v.name == SELF => {
                    return Err("self is not forecast: it measures avsm itself".to_string())
                }
                Some(v) if v.problem.is_some() || v.path.is_none() => {
                    return Err(format!(
                        "{n} cannot be forecast now: {}",
                        v.problem.clone().unwrap_or_default()
                    ))
                }
                Some(_) => {}
            }
        }
        let mut n = opts.ventures.clone();
        n.dedup();
        n
    };
    if names.is_empty() {
        return Err("nothing to forecast: no venture is taken on".to_string());
    }
    // The text each venture runs under before any sweep.
    let base: BTreeMap<String, String> = names
        .iter()
        .map(|n| {
            let text = opts.mandates.get(n).cloned().unwrap_or_else(|| {
                index
                    .adoptions()
                    .get(n)
                    .and_then(|a| a.text.clone())
                    .unwrap_or_else(|| {
                        serde_yaml::to_string(&ventures[n].mandate).unwrap_or_default()
                    })
            });
            (n.clone(), text)
        })
        .collect();
    let variants: Vec<Variant> = match &opts.sweep {
        None => vec![Variant {
            label: None,
            mandates: opts
                .mandates
                .iter()
                .filter(|(n, _)| names.contains(n))
                .map(|(n, t)| (n.clone(), t.clone()))
                .collect(),
        }],
        Some(s) => {
            let mut out = Vec::new();
            for value in &s.values {
                let mut mandates = BTreeMap::new();
                let mut errors = Vec::new();
                for n in &names {
                    match set_field(&base[n], &s.field, value) {
                        Ok(t) => {
                            mandates.insert(n.clone(), t);
                        }
                        Err(e) => errors.push(format!("{n}: {e}")),
                    }
                }
                if mandates.is_empty() {
                    return Err(errors.join("; "));
                }
                out.push(Variant {
                    label: Some(format!("{}={value}", s.field)),
                    mandates,
                });
            }
            out
        }
    };
    // The mandate each venture runs under in each variant.
    let mandates: Vec<BTreeMap<String, Mandate>> = variants
        .iter()
        .map(|v| {
            names
                .iter()
                .map(|n| {
                    let m = match v.mandates.get(n) {
                        Some(t) => Mandate::parse(t).map_err(|e| format!("{n}: {e}"))?,
                        None => ventures[n].mandate.clone(),
                    };
                    Ok((n.clone(), m))
                })
                .collect::<Result<BTreeMap<_, _>, String>>()
        })
        .collect::<Result<_, String>>()?;
    let step = names
        .iter()
        .map(|n| ventures[n].mandate.cadence.as_secs() as i64)
        .min()
        .unwrap_or(STEP_FLOOR)
        .max(STEP_FLOOR);
    let start = clock::now().max(index.entries().next_back().map_or(i64::MIN, |e| e.at)) + step;
    let end = start + i64::from(opts.days) * 86_400;
    let scratch = Scratch::new(opts.scratch.as_deref())?;
    let head = scratch.0.join("head.db");
    source
        .copy_to(&head)
        .map_err(|e| format!("cannot fork the ledger: {e}"))?;
    let shared = Shared {
        head,
        index,
        config: std::fs::read_to_string(home.join(config::CONFIG_FILE)).ok(),
        start,
        end,
        step,
        names: &names,
        variants: &variants,
        mandates: &mandates,
    };
    let mut branches = Vec::new();
    for variant in 0..variants.len() {
        for policy in opts.policies.iter().copied() {
            for i in 0..opts.seeds {
                branches.push(Branch {
                    variant,
                    policy,
                    seed: world::member_seed(opts.seed, u64::from(i)),
                });
            }
        }
    }
    let results: Vec<Mutex<Option<Traced>>> = branches.iter().map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(branches.len())
        .max(1);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                let Some(b) = branches.get(i) else { break };
                let home = scratch.0.join(format!("branch-{i}"));
                let r = run_branch(&home, b, &shared);
                let _ = std::fs::remove_dir_all(&home);
                *results[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
            });
        }
    });
    let mut paths = Vec::with_capacity(branches.len());
    for r in results {
        match r.into_inner().unwrap_or_else(|e| e.into_inner()) {
            Some(Ok(p)) => paths.push(p),
            Some(Err(e)) => return Err(format!("a forecast branch failed: {e}")),
            None => return Err("a forecast branch did not run".to_string()),
        }
    }
    drop(scratch);
    Ok(aggregate(
        opts, step, &names, &variants, &mandates, &branches, &paths,
    ))
}

/// Run one branch in `home`: copy the head, run the kernel over the days,
/// read what happened after the head.
fn run_branch(home: &Path, b: &Branch, shared: &Shared) -> Result<BTreeMap<String, Trace>, String> {
    std::fs::create_dir_all(home).map_err(|e| format!("{}: {e}", home.display()))?;
    std::fs::copy(&shared.head, home.join("ledger.db"))
        .map_err(|e| format!("cannot copy the ledger: {e}"))?;
    if let Some(text) = &shared.config {
        std::fs::write(home.join(config::CONFIG_FILE), text)
            .map_err(|e| format!("cannot copy config: {e}"))?;
    }
    let _clock = clock::scoped(shared.start);
    let _quiet = crate::kernel::quiet();
    let _depth = InBranch::enter();
    let sim = Arc::new(Simulation::branch(b.seed));
    let role = Worst {
        seed: b.seed,
        ventures: shared.names.iter().cloned().collect(),
    };
    let mut k = Kernel::forecasting(home, sim, Box::new(role), shared.index)?;
    k.scope = Some(shared.names.iter().cloned().collect());
    for (name, text) in &shared.variants[b.variant].mandates {
        k.adopt_text(name, text, BY)?;
    }
    let head = k.index()?.last();
    let human = b.policy.human();
    let mut answered = 0u64;
    while clock::now() < shared.end {
        k.tick();
        if human != Human::Away {
            for q in k.open_asks()? {
                let args = simulate::answer(&k, human, q, answered);
                answered += 1;
                let _ = k.handle_as(&args, Some(BY));
            }
        }
        clock::advance(shared.step);
    }
    let after = k.ledger.after(head).map_err(|e| e.to_string())?;
    k.shutdown();
    drop(k);
    Ok(read_branch(
        &after,
        shared.start,
        shared.names,
        &shared.mandates[b.variant],
    ))
}

/// What each venture came to in a branch, from its records after the head.
fn read_branch(
    after: &[Entry],
    start: i64,
    names: &[String],
    mandates: &BTreeMap<String, Mandate>,
) -> BTreeMap<String, Trace> {
    let day = |at: i64| ((at - start).max(0) / 86_400) as u32;
    let mut out: BTreeMap<String, Trace> = names
        .iter()
        .map(|n| (n.clone(), Trace::default()))
        .collect();
    // Refusal episodes: the opening record's count, until a closing one
    // gives the total.
    let mut refused: BTreeMap<Seq, (String, u32)> = BTreeMap::new();
    let mut halted_self: Option<u32> = None;
    for e in after {
        let v = e.record.venture();
        if v == SELF {
            if let Record::Action { action, .. } = &e.record {
                if action == STOP {
                    halted_self.get_or_insert(day(e.at));
                }
            }
            continue;
        }
        let (Some(p), Some(m)) = (out.get_mut(v), mandates.get(v)) else {
            continue;
        };
        let wall = |p: &mut Trace, kind: &str, metric: &str| {
            p.walls
                .entry((kind.to_string(), metric.to_string()))
                .or_insert(day(e.at));
        };
        match &e.record {
            Record::Report {
                claim,
                metric,
                value: Some(x),
                implausible: None,
                ..
            } if claim.authorizes() => {
                if metric == m.metric_name() {
                    p.last.insert(metric.clone(), *x);
                } else if let Some(name) = metric.strip_prefix("guard.") {
                    p.last.insert(metric.clone(), *x);
                    if m.guard(name).is_some_and(|g| g.breached(*x)) {
                        p.breached.entry(metric.clone()).or_insert(day(e.at));
                        wall(p, "guard", metric);
                    }
                }
            }
            Record::Ask { .. } => p.asks += 1.0,
            Record::Forecast { .. } => p.nested += 1,
            Record::Refusal {
                reason,
                count,
                closes,
                ..
            } => {
                let quota = ["OverQuota", "OverBudget", "AsksQuotaSpent"]
                    .iter()
                    .any(|k| reason.contains(k));
                if quota {
                    wall(p, "quota", m.metric_name());
                }
                match closes {
                    None => {
                        refused.insert(e.seq, (v.to_string(), *count));
                    }
                    Some(open) => {
                        refused.insert(*open, (v.to_string(), *count));
                    }
                }
            }
            Record::Action { action, .. } if action == STOP => {
                p.halts += 1.0;
                wall(p, "halt", m.metric_name());
            }
            Record::Intent { action, proof, .. }
                if ![DELEGATE, STOP, RESUME].contains(&action.as_str()) =>
            {
                let perm = m.permitted(action);
                if perm.is_some_and(crate::mandate::Permitted::is_forecast) {
                    p.tried += 1;
                }
                p.spend += f64::from(perm.map_or(0, |p| p.cost));
                if proof.decision().is_none() {
                    p.delegations += 1.0;
                    if perm.is_some_and(|p| !p.reversible) {
                        wall(p, "irreversible delegation", m.metric_name());
                    }
                }
            }
            _ => {}
        }
    }
    for (venture, count) in refused.into_values() {
        if let Some(p) = out.get_mut(&venture) {
            p.refusals += f64::from(count);
        }
    }
    // A halt of self halts every venture.
    if let Some(d) = halted_self {
        for (n, p) in out.iter_mut() {
            p.halts += 1.0;
            let metric = mandates.get(n).map_or("metric", |m| m.metric_name());
            p.walls
                .entry(("halt".to_string(), metric.to_string()))
                .or_insert(d);
        }
    }
    out
}

/// Quantiles of a sample; zeros when it is empty.
fn qs(values: &[f64]) -> Quantiles {
    world::quantiles(values).unwrap_or(Quantiles::from_values([0.0; 7]))
}

/// Which way is better for a venture's metric: -1 when lower is (a
/// threshold above a bound, or levers that push it down), else 1.
pub fn better(m: &Mandate) -> f64 {
    match target(m) {
        Some(Direction::Down) => -1.0,
        _ => 1.0,
    }
}

/// The way the venture wants its metric to go, as the mandate declares
/// it: the threshold's side, else `better`. Never guessed.
pub fn target(m: &Mandate) -> Option<Direction> {
    m.direction().map(|w| match w {
        crate::mandate::Worse::Up => Direction::Up,
        crate::mandate::Worse::Down => Direction::Down,
    })
}

#[allow(clippy::too_many_arguments)]
fn aggregate(
    opts: &Options,
    step: i64,
    names: &[String],
    variants: &[Variant],
    mandates: &[BTreeMap<String, Mandate>],
    branches: &[Branch],
    paths: &[BTreeMap<String, Trace>],
) -> Outcome {
    let mut summaries = Vec::new();
    // (variant, venture, policy) -> the paths of its seeds, in seed order.
    let of = |variant: usize, venture: &str, policy: Policy| -> Vec<&Trace> {
        branches
            .iter()
            .zip(paths)
            .filter(|(b, _)| b.variant == variant && b.policy == policy)
            .filter_map(|(_, p)| p.get(venture))
            .collect()
    };
    for (vi, variant) in variants.iter().enumerate() {
        for name in names {
            // A venture the sweep's field does not touch is forecast once,
            // with the first variant.
            if variant.label.is_some() && !variant.mandates.contains_key(name) && vi > 0 {
                continue;
            }
            let m = &mandates[vi][name];
            for policy in opts.policies.iter().copied() {
                let seeds = of(vi, name, policy);
                let mut metric_names = vec![m.metric_name().to_string()];
                metric_names.extend(m.guards.iter().map(|g| g.metric()));
                let metrics = metric_names
                    .iter()
                    .filter_map(|metric| {
                        let values: Vec<f64> = seeds
                            .iter()
                            .filter_map(|p| p.last.get(metric).copied())
                            .collect();
                        world::quantiles(&values).map(|quantiles| Envelope {
                            name: metric.clone(),
                            quantiles,
                        })
                    })
                    .collect();
                let n = seeds.len().max(1) as f64;
                let guards = m
                    .guards
                    .iter()
                    .map(|g| Event {
                        name: g.metric(),
                        p: seeds
                            .iter()
                            .filter(|p| p.breached.contains_key(&g.metric()))
                            .count() as f64
                            / n,
                    })
                    .collect();
                let col =
                    |f: fn(&Trace) -> f64| qs(&seeds.iter().map(|p| f(p)).collect::<Vec<_>>());
                summaries.push(Summary {
                    venture: name.clone(),
                    policy,
                    variant: variant.label.clone(),
                    metrics,
                    guards,
                    reached: Reached {
                        spend: col(|p| p.spend),
                        asks: col(|p| p.asks),
                        refusals: col(|p| p.refusals),
                        halts: col(|p| p.halts),
                        delegations: col(|p| p.delegations),
                    },
                });
            }
        }
    }
    // Walls, outside a sweep: a sweep tries what-ifs, it does not warn.
    let mut walls = Vec::new();
    if opts.sweep.is_none() {
        for name in names {
            let mut hit: BTreeMap<(String, String), Vec<PolicyDay>> = BTreeMap::new();
            for policy in opts.policies.iter().copied() {
                let seeds = of(0, name, policy);
                let mut first: BTreeMap<(String, String), u32> = BTreeMap::new();
                for p in &seeds {
                    for (k, d) in &p.walls {
                        let e = first.entry(k.clone()).or_insert(*d);
                        *e = (*e).min(*d);
                    }
                }
                for (k, d) in first {
                    hit.entry(k).or_default().push(PolicyDay {
                        policy: policy.name().to_string(),
                        day: Some(d),
                    });
                }
            }
            for ((kind, metric), mut days) in hit {
                for policy in opts.policies.iter().copied() {
                    if !days.iter().any(|d| d.policy == policy.name()) {
                        days.push(PolicyDay {
                            policy: policy.name().to_string(),
                            day: None,
                        });
                    }
                }
                days.sort_by_key(|d| Policy::EVERY.iter().position(|p| p.name() == d.policy));
                let earliest = days
                    .iter()
                    .filter_map(|d| d.day.map(|n| (n, d.policy.clone())))
                    .min()
                    .map(|(_, p)| p);
                let envelope = earliest.and_then(|policy| {
                    let s = summaries.iter().find(|s| {
                        s.venture == *name && s.variant.is_none() && s.policy.name() == policy
                    })?;
                    s.metrics
                        .iter()
                        .find(|m| m.name == metric)
                        .map(|m| m.quantiles)
                });
                walls.push(Wall {
                    venture: name.clone(),
                    kind,
                    metric,
                    envelope,
                    days,
                });
            }
        }
        walls.sort_by_key(|w| w.days.iter().filter_map(|d| d.day).min());
    }
    // A sweep: PBO by CSCV over seeds × values for each policy, and where
    // the outcome changes from one value to the next.
    let mut pbo_by = Vec::new();
    let mut marks = Vec::new();
    if let Some(sweep) = &opts.sweep {
        let swept = names.iter().find(|n| variants[0].mandates.contains_key(*n));
        if let Some(name) = swept {
            let sign = better(&mandates[0][name]);
            let metric = mandates[0][name].metric_name().to_string();
            for policy in opts.policies.iter().copied() {
                let columns: Vec<Vec<&Trace>> =
                    (0..variants.len()).map(|v| of(v, name, policy)).collect();
                let rows: Vec<Vec<f64>> = (0..opts.seeds as usize)
                    .filter_map(|seed| {
                        columns
                            .iter()
                            .map(|c| {
                                c.get(seed)
                                    .and_then(|p| p.last.get(&metric))
                                    .map(|x| sign * x)
                            })
                            .collect::<Option<Vec<f64>>>()
                    })
                    .collect();
                pbo_by.push((policy, pbo::cscv(&rows)));
                let row = |v: usize| {
                    summaries.iter().find(|s| {
                        s.venture == *name && s.policy == policy && s.variant == variants[v].label
                    })
                };
                for v in 1..variants.len() {
                    let (Some(a), Some(b)) = (row(v - 1), row(v)) else {
                        continue;
                    };
                    let from = &sweep.values[v - 1];
                    let to = &sweep.values[v];
                    for (ga, gb) in a.guards.iter().zip(&b.guards) {
                        if (ga.p > 0.5) != (gb.p > 0.5) {
                            marks.push(format!(
                                "{name} {} {from} -> {to}: {} breach p {} -> {} ({})",
                                sweep.field,
                                ga.name,
                                record::round(ga.p),
                                record::round(gb.p),
                                policy.name()
                            ));
                        }
                    }
                    if let (Some(ma), Some(mb)) = (a.metrics.first(), b.metrics.first()) {
                        let spread = ma.quantiles.p75 - ma.quantiles.p25;
                        let moved = mb.quantiles.p50 - ma.quantiles.p50;
                        if moved.abs() > spread {
                            marks.push(format!(
                                "{name} {} {from} -> {to}: median {} moves {} -> {}, more than its spread {} ({})",
                                sweep.field,
                                metric,
                                record::round(ma.quantiles.p50),
                                record::round(mb.quantiles.p50),
                                record::round(spread),
                                policy.name()
                            ));
                        }
                    }
                }
            }
        }
    }
    Outcome {
        days: opts.days,
        seeds: opts.seeds,
        step,
        branches: branches.len(),
        summaries,
        walls,
        field: opts.sweep.as_ref().map(|s| s.field.clone()),
        trials: opts.sweep.as_ref().map_or(0, |s| s.values.len()),
        pbo: pbo_by,
        marks,
        tried: paths.iter().flat_map(|p| p.values()).map(|t| t.tried).sum(),
        nested: paths
            .iter()
            .flat_map(|p| p.values())
            .map(|t| t.nested)
            .sum(),
    }
}

/// The measured asks a forecast's walls answer with: one per venture that
/// hit one, past the asks quota like every signal, and none while an ask
/// of the same signal is open for that venture. Each cites the venture's
/// forecast records in `seqs`.
pub fn asks(index: &Index, outcome: &Outcome, seqs: &BTreeMap<String, Vec<Seq>>) -> Vec<Record> {
    let mut out = Vec::new();
    let ventures: BTreeSet<&str> = outcome.walls.iter().map(|w| w.venture.as_str()).collect();
    for venture in ventures {
        let open = index.open_signals().into_iter().any(|(e, s)| {
            e.record.venture() == venture
                && e.record.claim() == Claim::Measured
                && matches!(s, Signal::Forecast { .. })
        });
        if open {
            continue;
        }
        let walls: Vec<&Wall> = outcome
            .walls
            .iter()
            .filter(|w| w.venture == venture)
            .collect();
        let first = walls[0];
        let envelope = first.envelope.unwrap_or(Quantiles::from_values([0.0; 7]));
        let v = envelope.values().map(record::round);
        let mut question = format!(
            "a forecast over {} days x {} seeds reaches a wall: {} on {}\n  envelope   {} at day {}: p5 {} p10 {} p25 {} p50 {} p75 {} p90 {} p95 {}\n  first hit  {}\n",
            outcome.days,
            outcome.seeds,
            first.kind,
            first.metric,
            first.metric,
            outcome.days,
            v[0], v[1], v[2], v[3], v[4], v[5], v[6],
            first_days(&first.days)
        );
        for w in walls.iter().skip(1).take(7) {
            question.push_str(&format!(
                "  also       {} on {} ({})\n",
                w.kind,
                w.metric,
                first_days(&w.days)
            ));
        }
        question
            .push_str("  the forecast is inferred and authorizes nothing; avsm asks on its own");
        let signal = Signal::Forecast {
            wall: first.kind.clone(),
            metric: first.metric.clone(),
            envelope,
            days: first.days.clone(),
        };
        out.push(Record::Ask {
            claim: Claim::Measured,
            venture: venture.to_string(),
            action: None,
            question,
            refusal: None,
            payload: None,
            packet: Some(crate::packet::signal_packet(
                index,
                venture,
                &signal,
                &first.metric,
            )),
            signal: Some(signal),
            cites: seqs.get(venture).cloned().unwrap_or_default(),
        });
    }
    out
}

// ----- capacity -----

/// The metric drifts off the target faster than the levers can bring it
/// back: both a day.
#[derive(Clone, Debug, PartialEq)]
pub struct Gap {
    pub metric: String,
    /// How far the metric moves a day on its own, signed.
    pub drift: f64,
    /// The most the permitted levers can move it back a day: each one's
    /// effect a run times the fewer of its cap a day and the turns the
    /// controller gives it a day.
    pub most: f64,
    pub levers: Vec<String>,
    pub toward: Direction,
    /// What holds the levers back, in words: their caps, or how often the
    /// controller acts.
    pub limit: String,
    /// The measured and declared records the gap was read from, beyond the
    /// metric's latest reading: the readings, runs, denials and refusals.
    pub cites: Vec<Seq>,
    /// The whole finding in words, when it is not a rate against a rate:
    /// a cap spent with no margin, or what held the levers back.
    pub said: Option<String>,
}

/// A number as short as it goes, to two places.
fn num(x: f64) -> String {
    let r = (x * 100.0).round() / 100.0;
    if r == r.trunc() {
        format!("{}", r as i64)
    } else {
        format!("{r}")
    }
}

impl Gap {
    /// `unread grows 20/day; reply can remove at most 5/day: the cap`.
    pub fn describe(&self) -> String {
        if let Some(said) = &self.said {
            return said.clone();
        }
        let (moves, back) = match self.toward {
            Direction::Down => ("grows", "remove"),
            Direction::Up => ("falls", "add"),
        };
        let levers = if self.levers.is_empty() {
            "no permitted lever".to_string()
        } else {
            self.levers.join(" and ")
        };
        format!(
            "{} {moves} {}/day; {levers} can {back} at most {}/day: {}",
            self.metric,
            num(self.drift.abs()),
            num(self.most),
            self.limit
        )
    }
}

/// How many times a day the controller can run a lever on its own, and
/// what that is in words: a model or classifier is asked once a night;
/// a threshold acts at most once a report, and only its own action.
fn turns(mandate: &Mandate, lever: &str) -> (f64, &'static str) {
    match &mandate.controller {
        Controller::Threshold { action, .. } if action == lever => (
            86_400.0 / mandate.cadence.as_secs().max(1) as f64,
            "the threshold acts at most once a report",
        ),
        Controller::Threshold { .. } => (0.0, "the threshold runs only its own action"),
        Controller::Plan { .. } => (
            86_400.0 / mandate.cadence.as_secs().max(1) as f64,
            "the planner acts at most once a report",
        ),
        // A model's nightly reply may carry several independent choices.
        Controller::Model { .. } => (
            crate::role::MAX_CHOICES as f64,
            "the model is asked once a night, for at most 5 choices",
        ),
        Controller::Classifier { .. } => (1.0, "the classifier is asked once a night"),
    }
}

/// The capacity of a venture's mandate, in code: the baseline's drift a
/// day against each permitted lever's effect times its runs a day, the
/// fewer of its cap a day and the controller's turns a day; the effect
/// measured (the median of at least `MIN_SAMPLES` runs) or else declared
/// by `expects`. Some when the drift is away from the target and
/// the levers cannot keep up; None when they can, or nothing says.
pub fn capacity(index: &Index, venture: &str, mandate: &Mandate) -> Option<Gap> {
    capacity_with(index, venture, mandate, None)
}

/// `capacity`, with the asks quota a day when there is one: a lever that
/// needs a decision each time runs at most as often as it can be asked.
/// The quota is shared by every venture, so the whole of it is an upper
/// bound, never what one venture gets.
pub fn capacity_with(
    index: &Index,
    venture: &str,
    mandate: &Mandate,
    asks_a_day: Option<f64>,
) -> Option<Gap> {
    let base = Baseline::from_index(index, venture, mandate);
    if base.drift.len() < 2 {
        return None;
    }
    let effects: Vec<(&crate::mandate::Permitted, f64)> = mandate
        .permitted
        .iter()
        .filter(|p| !(p.is_research() || p.is_forecast() || p.is_parent_lever()))
        .filter_map(|p| Some((p, effect_of(&base, p)?)))
        .collect();
    // Which way is better is declared or there is no check: a guessed
    // direction makes a debatable ask, and a debatable ask is noise.
    let toward = target(mandate)?;
    let want = if toward == Direction::Down { -1.0 } else { 1.0 };
    let drift = base.drift_per_day();
    if drift * want >= 0.0 {
        return None;
    }
    let mut most = 0.0;
    let mut levers = Vec::new();
    let mut limits = BTreeSet::new();
    for (p, effect) in effects {
        if effect * want <= 0.0 {
            continue;
        }
        let cap = f64::from(p.cap.count) * 86_400.0 / p.cap.per.as_secs().max(1) as f64;
        let (turns, why) = turns(mandate, &p.name);
        let mut runs_a_day = cap.min(turns);
        let mut limit = if turns < cap { why } else { "the cap" };
        // Each run of a lever that asks each time is an ask.
        let asks_each_time = !p.reversible || (p.takes().is_some() && !p.delegated);
        if let (true, Some(quota)) = (asks_each_time, asks_a_day) {
            if quota < runs_a_day {
                runs_a_day = quota;
                limit = "the asks quota, shared by every venture, at most";
            }
        }
        limits.insert(limit);
        most += effect.abs() * runs_a_day;
        levers.push(p.name.clone());
    }
    // No lever with a known effect toward the target, measured or
    // declared, is nothing to compare against: not a gap to ask about.
    if levers.is_empty() || most >= drift.abs() {
        return None;
    }
    Some(Gap {
        metric: mandate.metric_name().to_string(),
        drift,
        most,
        levers,
        toward,
        limit: limits.into_iter().collect::<Vec<_>>().join("; "),
        cites: vec![],
        said: None,
    })
}

/// How many cap windows running a lever's cap must be spent, with the
/// metric still past its threshold, before the cap is called the limit.
pub const SPENT_WINDOWS: i64 = 2;
/// The days a metric must worsen, each worse than the one before, before
/// what held its levers back is named.
pub const HELD_DAYS: i64 = 7;
/// The most records a gap cites.
const MAX_CITES: usize = 8;
/// What the asks quota refusing a lever is called in a refusal's reason.
const ASKS_SPENT: &str = "AsksQuotaSpent";
const DAY_SECS: i64 = 86_400;

/// The cap binds: in each of the last `SPENT_WINDOWS` cap windows, the
/// metric was read past a threshold controller's bound while its lever had
/// already been run its cap's count in the cap window before the reading,
/// so the reading went unanswered. Zero margin, measured; nothing here
/// foresees what the world does next.
pub fn cap_spent(index: &Index, venture: &str, mandate: &Mandate, now: i64) -> Option<Gap> {
    let Controller::Threshold {
        above,
        below,
        action,
        ..
    } = &mandate.controller
    else {
        return None;
    };
    let p = mandate.permitted(action)?;
    let (bound, side) = match (above, below) {
        (Some(b), _) => (*b, "above"),
        (None, Some(b)) => (*b, "below"),
        (None, None) => return None,
    };
    let past = |x: f64| {
        if side == "above" {
            x > bound
        } else {
            x < bound
        }
    };
    let metric = mandate.metric_name();
    let per = p.cap.per.as_secs().max(1) as i64;
    let series = index.series(venture, metric);
    let at = |s: Seq| index.get(s).map_or(i64::MIN, |e| e.at);
    let mut cites = Vec::new();
    let mut latest = None;
    // The runs in the cap window before `t`, not counting one at `t`, the
    // answer to the reading itself.
    let spent = |t: i64| {
        index.used(venture, action, t - per) - index.used(venture, action, t - 1) >= p.cap.count
    };
    for k in 0..SPENT_WINDOWS {
        let (from, to) = (now - (k + 1) * per, now - k * per);
        let lo = series.partition_point(|(s, _)| at(*s) <= from);
        let hi = series.partition_point(|(s, _)| at(*s) <= to);
        let (seq, x) = *series[lo..hi]
            .iter()
            .rev()
            .find(|(s, x)| past(*x) && spent(at(*s)))?;
        latest.get_or_insert(x);
        cites.push(seq);
    }
    // The runs that spent the last window, newest first, after the readings.
    let room = MAX_CITES.saturating_sub(cites.len());
    cites.extend(
        index
            .of(venture)
            .rev()
            .take_while(|e| e.at > now - per)
            .filter(|e| {
                e.record.kind() == record::Kind::Intent && e.record.action() == Some(action)
            })
            .map(|e| e.seq)
            .take(room),
    );
    let cap = f64::from(p.cap.count) * 86_400.0 / per as f64;
    let base = Baseline::from_index(index, venture, mandate);
    let effect = effect_of(&base, p).map_or(0.0, f64::abs);
    let windows = if p.cap.per == crate::mandate::Cap::DAY {
        format!("{SPENT_WINDOWS} days")
    } else {
        format!(
            "{SPENT_WINDOWS} windows of {}",
            humantime::format_duration(p.cap.per)
        )
    };
    Some(Gap {
        metric: metric.to_string(),
        drift: 0.0,
        most: effect * cap,
        levers: vec![action.clone()],
        toward: target(mandate)?,
        limit: "the cap".to_string(),
        cites,
        said: Some(format!(
            "{action}'s cap {} was spent in each of the last {windows} with {metric} still {side} {} (latest {}): no margin; the cap is the limit",
            p.cap,
            num(bound),
            num(latest?)
        )),
    })
}

/// What a lever's records over a span show held it back.
#[derive(Default)]
struct Held {
    asks: BTreeSet<Seq>,
    denied: Vec<Seq>,
    quota: u32,
    refusals: Vec<Seq>,
    proposed: u32,
}

/// What held the levers back: the metric worse each of the last
/// `HELD_DAYS` days than the day before (daily means of its trusted,
/// plausible readings), and for each lever that could move it back, what
/// the records of those days show stopped it: its cap spent, its asks
/// denied, the asks quota refusing it, or else a controller that proposed
/// it too seldom, each with its count.
pub fn held_back(index: &Index, venture: &str, mandate: &Mandate, now: i64) -> Option<Gap> {
    let toward = target(mandate)?;
    let worse = |a: f64, b: f64| {
        if toward == Direction::Down {
            b > a
        } else {
            b < a
        }
    };
    let metric = mandate.metric_name();
    let span = HELD_DAYS * DAY_SECS;
    let start = now - span;
    let series = index.series(venture, metric);
    let at = |s: Seq| index.get(s).map_or(i64::MIN, |e| e.at);
    let lo = series.partition_point(|(s, _)| at(*s) <= start);
    let mut days = vec![(0.0, 0u32); HELD_DAYS as usize];
    for (s, x) in &series[lo..] {
        let d = ((at(*s) - start - 1) / DAY_SECS).clamp(0, HELD_DAYS - 1) as usize;
        days[d].0 += x;
        days[d].1 += 1;
    }
    if days.iter().any(|(_, n)| *n == 0) {
        return None;
    }
    let means: Vec<f64> = days.iter().map(|(t, n)| t / f64::from(*n)).collect();
    let (first, last) = (means[0], means[means.len() - 1]);
    if !worse(first, last) || means.windows(2).any(|w| worse(w[1], w[0])) {
        return None;
    }
    // A threshold is the target: on its right side, nothing is held back.
    match &mandate.controller {
        Controller::Threshold { above: Some(b), .. } if last <= *b => return None,
        Controller::Threshold {
            above: None,
            below: Some(b),
            ..
        } if last >= *b => return None,
        _ => {}
    }
    let base = Baseline::from_index(index, venture, mandate);
    let want = if toward == Direction::Down { -1.0 } else { 1.0 };
    let levers: Vec<&crate::mandate::Permitted> = mandate
        .permitted
        .iter()
        .filter(|p| !(p.is_research() || p.is_forecast() || p.is_parent_lever()))
        // A lever known to move the metric the wrong way holds nothing back.
        .filter(|p| effect_of(&base, p).is_none_or(|e| e * want > 0.0))
        .collect();
    if levers.is_empty() {
        return None;
    }
    let window: Vec<&Entry> = {
        let mut w: Vec<&Entry> = index
            .of(venture)
            .rev()
            .take_while(|e| e.at > start)
            .collect();
        w.reverse();
        w
    };
    // A refusal is opened with count 1 and closed with its whole count.
    let closed: BTreeMap<Seq, u32> = window
        .iter()
        .filter_map(|e| match &e.record {
            Record::Refusal {
                closes: Some(open),
                count,
                ..
            } => Some((*open, *count)),
            _ => None,
        })
        .collect();
    let mut held: BTreeMap<&str, Held> = BTreeMap::new();
    for e in &window {
        match &e.record {
            Record::Ask {
                action: Some(a),
                signal: None,
                ..
            } => {
                held.entry(a.as_str()).or_default().asks.insert(e.seq);
            }
            Record::Decision {
                verdict: crate::record::Verdict::Deny { ask, .. },
                ..
            } => {
                let lever = index.get(*ask).and_then(|a| match &a.record {
                    Record::Ask {
                        action: Some(a),
                        signal: None,
                        ..
                    } => Some(a.as_str()),
                    _ => None,
                });
                if let Some(h) = lever.and_then(|l| held.get_mut(l)) {
                    if h.asks.contains(ask) {
                        h.denied.push(e.seq);
                    }
                }
            }
            Record::Refusal {
                action,
                reason,
                closes: None,
                ..
            } if reason.split("; ").any(|r| r == ASKS_SPENT) => {
                let h = held.entry(action.as_str()).or_default();
                h.quota += closed.get(&e.seq).copied().unwrap_or(1);
                h.refusals.push(e.seq);
            }
            Record::Proposal { action, .. } => {
                held.entry(action.as_str()).or_default().proposed += 1;
            }
            _ => {}
        }
    }
    let mut parts = Vec::new();
    let mut limits = BTreeSet::new();
    let mut cites = Vec::new();
    let mut most = 0.0;
    let none = Held::default();
    for p in &levers {
        let h = held.get(p.name.as_str()).unwrap_or(&none);
        let per = p.cap.per.as_secs().max(1) as i64;
        let room = (f64::from(p.cap.count) * span as f64 / per as f64).floor() as u32;
        let ran = index.used(venture, &p.name, start);
        most += effect_of(&base, p).map_or(0.0, f64::abs) * f64::from(p.cap.count) * 86_400.0
            / per as f64;
        let mut why = Vec::new();
        if ran >= room {
            why.push("the cap".to_string());
            limits.insert("the cap");
        }
        if !h.denied.is_empty() {
            why.push(format!(
                "{} of {} asks denied",
                h.denied.len(),
                h.asks.len()
            ));
            limits.insert("denied asks");
            cites.extend(h.denied.iter().rev().take(MAX_CITES / 2));
        }
        if h.quota > 0 {
            why.push(format!(
                "refused {} times: the asks quota was spent",
                h.quota
            ));
            limits.insert("the asks quota");
            cites.extend(h.refusals.iter().rev().take(MAX_CITES / 2));
        }
        if why.is_empty() {
            why.push(format!("the controller proposed it {} times", h.proposed));
            limits.insert("the controller");
        }
        parts.push(format!(
            "{} ran {ran} of {room}: {}",
            p.name,
            why.join(", ")
        ));
    }
    cites.truncate(MAX_CITES);
    let moved = if last > first { "rose" } else { "fell" };
    Some(Gap {
        metric: metric.to_string(),
        drift: (last - first) / (HELD_DAYS - 1) as f64,
        most,
        levers: levers.iter().map(|p| p.name.clone()).collect(),
        toward,
        limit: limits.into_iter().collect::<Vec<_>>().join("; "),
        cites,
        said: Some(format!(
            "{metric} {moved} from {} to {} over {HELD_DAYS} days, worse each day; {}",
            num(first),
            num(last),
            parts.join("; ")
        )),
    })
}

/// A lever's effect a run: the median of its measured runs, at least
/// `MIN_SAMPLES` of them, or else what its `expects` declares.
fn effect_of(base: &Baseline, p: &crate::mandate::Permitted) -> Option<f64> {
    {
        // NOTE: kept as a block so the two sources read side by side.
        let measured = base
            .effects
            .get(&p.name)
            .filter(|s| s.len() >= MIN_SAMPLES)
            .map(|s| {
                let mut s = s.clone();
                s.sort_by(f64::total_cmp);
                s[s.len() / 2]
            });
        let declared = p.expects.and_then(|e| {
            let size = e.size?;
            Some(if e.direction == Direction::Up {
                size
            } else {
                -size
            })
        });
        measured.or(declared)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANDATE: &str = "purpose: p\nmetric: count\ncontroller: {kind: model}\ncadence: 1h\n\
        permitted:\n- {name: water, run: ./water, cap: 2/day, reversible: true}\n";

    #[test]
    fn a_field_is_set_by_path_and_the_result_must_parse() {
        let t = set_field(MANDATE, "permitted.water.cap", "8/day").unwrap();
        let m = Mandate::parse(&t).unwrap();
        assert_eq!(m.permitted("water").unwrap().cap.count, 8);
        let t = set_field(MANDATE, "cadence", "2h").unwrap();
        assert_eq!(Mandate::parse(&t).unwrap().cadence.as_secs(), 7200);
        assert!(set_field(MANDATE, "permitted.fire.cap", "1/day")
            .unwrap_err()
            .contains("no permitted.fire.cap"));
        assert!(set_field(MANDATE, "permitted.water.cap", "lots").is_err());
        assert!(set_field(MANDATE, "nope", "1").is_err());
    }

    #[test]
    fn inside_a_branch_the_depth_guard_refuses_a_forecast() {
        let ledger = Ledger::open_in_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let opts = Options::new(vec![], config::Forecast::default());
        let outside = run(&ledger, tmp.path(), &opts).unwrap_err();
        assert!(outside.contains("nothing to forecast"), "{outside}");
        {
            let _branch = InBranch::enter();
            let inside = run(&ledger, tmp.path(), &opts).unwrap_err();
            assert_eq!(inside, "a forecast branch never forecasts");
        }
        assert!(run(&ledger, tmp.path(), &opts)
            .unwrap_err()
            .contains("nothing to forecast"));
    }

    #[test]
    fn the_keyless_warden_loads_no_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let keys = tmp.path().join(config::KEYS_FILE);
        std::fs::write(&keys, "anthropic=sk-secret\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&keys, std::fs::Permissions::from_mode(0o600)).unwrap();
        let open = crate::warden::LocalWarden::open(tmp.path()).unwrap();
        assert_eq!(open.keys_held(), 1, "the real warden reads the keys file");
        let w = crate::warden::LocalWarden::keyless(
            tmp.path(),
            Arc::new(Simulation::branch(1)),
            &Index::default(),
        )
        .unwrap();
        assert_eq!(w.keys_held(), 0);
    }

    #[test]
    fn the_worst_case_role_proposes_permitted_levers_and_never_infers() {
        let m = Mandate::parse(
            "purpose: p\nmetric: count\ncontroller: {kind: model}\ncadence: 1h\n\
             permitted:\n- {name: a, run: ./a, cap: 2/day, reversible: true}\n\
             - {name: b, run: ./b, cap: 2/day, reversible: true, takes: json}\n",
        )
        .unwrap();
        let ctx = Context {
            venture: "shop".to_string(),
            mandate: m,
            entries: vec![],
            state: "running".to_string(),
            now: 1_000,
            backend: "k".to_string(),
            children: vec![],
            summaries: vec![],
            asks: vec![],
            to_summarize: vec![],
        };
        let mut role = Worst {
            seed: 3,
            ventures: ["shop".to_string()].into(),
        };
        let mut calls = 0;
        let mut seen = BTreeSet::new();
        for now in 0..40 {
            let ctx = Context {
                now: now * 86_400,
                ..ctx.clone()
            };
            let out = role
                .decide(
                    &ctx,
                    &mut |_| {
                        calls += 1;
                        Err("no".to_string())
                    },
                    &mut |_, _| Err("no".to_string()),
                )
                .unwrap();
            assert!(!out.is_empty());
            for r in out {
                let Record::Proposal {
                    claim,
                    action,
                    payload,
                    ..
                } = r
                else {
                    panic!("{r:?}")
                };
                assert_eq!(claim, Claim::Inferred);
                if action == "b" {
                    assert_eq!(payload.unwrap().takes(), record::Takes::Json);
                }
                seen.insert(action);
            }
        }
        assert_eq!(calls, 0);
        assert_eq!(seen.len(), 2, "{seen:?}");
        // Another venture's cycle gets nothing from it.
        let other = Context {
            venture: "self".to_string(),
            ..ctx
        };
        assert!(role
            .decide(&other, &mut |_| Err(String::new()), &mut |_, _| Err(
                String::new()
            ))
            .unwrap()
            .is_empty());
    }
}
