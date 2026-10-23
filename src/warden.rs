//! The warden: the only process that holds keys. It checks the gate again,
//! runs actions, writes what happened, and sends prompts to backends with
//! the key attached.

use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::adapter::{self, Occasion};
use crate::config::{self, Backend, Config};
use crate::gate::{self, Candidate, DELEGATE, RESUME, STOP};
use crate::index::Index;
use crate::ledger::{Ledger, Written};
use crate::mandate::{FieldType, Mandate, ParentCall};
use crate::record::Verdict;
use crate::record::{self, Claim, Entry, Payload, Record, Seq};
use crate::research;
use crate::role::InferRequest;
use crate::simulate::Simulation;
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
    /// Run the check of a payload lever on a payload now, as `execute` will
    /// at action time, so a payload that would fail it is never asked
    /// about. `Ok` when the lever has no check, or the check passes;
    /// `Failure::Refused` when it says no.
    fn check(&mut self, _venture: &str, _action: &str, _payload: &Payload) -> Result<(), Failure> {
        Ok(())
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
    /// `execute`, except that a lever that runs a command or an adapter
    /// may come back as soon as its intent is written: the run goes on
    /// apart, and its outcome is written when it ends.
    fn start(&mut self, candidate: &Candidate) -> Result<Vec<Entry>, Failure> {
        self.execute(candidate)
    }
    /// Stop every run of a venture's levers still going: each is
    /// signalled, then killed, and its outcome written as failed. Returns
    /// how many it stopped.
    fn cancel(&mut self, _venture: &str) -> Result<u32, String> {
        Ok(0)
    }
    /// The pid of the process behind this warden, 0 when there is none.
    fn pid(&self) -> u32 {
        0
    }
    /// Is the process behind this warden alive? Err says why not.
    fn alive(&mut self) -> Result<(), String> {
        Ok(())
    }
    /// Start the process behind this warden again; its new pid.
    fn respawn(&mut self) -> Result<u32, String> {
        Err("this warden has no process to start".to_string())
    }
    /// The records this warden appended since it was last asked, number
    /// and hash: with the kernel's own, every record the daemon wrote.
    fn written(&mut self) -> Result<Vec<(Seq, String)>, String> {
        Ok(Vec::new())
    }
    /// The process groups this warden started that are still alive, each
    /// with its venture: the daemon ends them when the warden dies, when
    /// avsmd stops, and when their venture is paused.
    fn groups(&mut self) -> Vec<(i32, String)> {
        Vec::new()
    }
}

/// Is this the error of a read or a measure still going in the warden?
pub fn pending(error: &str) -> bool {
    error.starts_with(crate::kernel::PENDING)
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
    /// Set only by `simulate` and a forecast's branches: every lever run
    /// goes to the simulation, never to the lever.
    sim: Option<Arc<Simulation>>,
    /// A forecast branch's warden: it holds no keys, calls no backend,
    /// reads no adapter, runs no payload check and writes no mandate file.
    keyless: bool,
    /// Every record this warden appended, on any of its ledger
    /// connections, not yet handed to the kernel.
    written: Written,
}

impl LocalWarden {
    /// Open the ledger and read config and keys.
    pub fn open(home: &Path) -> Result<LocalWarden, String> {
        let mut ledger = Ledger::open(&home.join("ledger.db")).map_err(|e| e.to_string())?;
        let written = Written::default();
        ledger.track(written.clone());
        Ok(LocalWarden {
            home: home.to_path_buf(),
            ledger,
            index: Index::default(),
            config: Config::load(home)?,
            keys: config::load_keys(home)?,
            sim: None,
            keyless: false,
            written,
        })
    }

    /// The warden of a forecast branch: it never reads the keys file, and
    /// every lever run goes to the simulation.
    /// Its index starts from a copy of `head` when that is an index of a
    /// prefix of the branch's ledger.
    pub(crate) fn keyless(
        home: &Path,
        sim: Arc<Simulation>,
        head: &Index,
    ) -> Result<LocalWarden, String> {
        let mut ledger = Ledger::open(&home.join("ledger.db")).map_err(|e| e.to_string())?;
        let written = Written::default();
        ledger.track(written.clone());
        ledger.throwaway().map_err(|e| e.to_string())?;
        let index = head.seed_for(&ledger).map_err(|e| e.to_string())?;
        Ok(LocalWarden {
            home: home.to_path_buf(),
            ledger,
            index,
            config: Config::load(home)?,
            keys: BTreeMap::new(),
            sim: Some(sim),
            keyless: true,
            written,
        })
    }

    /// Where this warden notes what it appends; a ledger opened on
    /// another thread of the warden notes there too.
    pub fn notes(&self) -> Written {
        self.written.clone()
    }

    /// How many keys this warden holds.
    pub fn keys_held(&self) -> usize {
        self.keys.len()
    }

    /// A warden for a simulation: it gates, writes the intent and the
    /// outcome as ever, and sends the run itself to the simulation.
    pub(crate) fn simulated(home: &Path, sim: Arc<Simulation>) -> Result<LocalWarden, String> {
        let mut warden = LocalWarden::open(home)?;
        warden.sim = Some(sim);
        Ok(warden)
    }
}

impl Warden for LocalWarden {
    fn written(&mut self) -> Result<Vec<(Seq, String)>, String> {
        let mut noted = self.written.lock().unwrap_or_else(|p| p.into_inner());
        Ok(std::mem::take(&mut *noted))
    }

    fn check(&mut self, venture: &str, action: &str, payload: &Payload) -> Result<(), Failure> {
        if self.keyless {
            return Ok(());
        }
        self.index
            .refresh(&self.ledger)
            .map_err(|e| e.to_string())?;
        let index = &self.index;
        let ventures = venture::from_adoptions(index.adoptions());
        let v = ventures
            .get(venture)
            .ok_or_else(|| format!("no venture named {venture}"))?;
        let perm = v
            .mandate
            .permitted(action)
            .ok_or_else(|| format!("{venture} does not permit {action}"))?;
        let dir = v.path.clone().unwrap_or_else(|| self.home.clone());
        // The schema comes first, as at action time: a payload that misses
        // it is the schema's to refuse then, not the check's now.
        if let Some(fields) = &perm.fields {
            if payload_schema(fields, payload).is_err() {
                return Ok(());
            }
        }
        match &perm.check {
            Some(check) => payload_check(&self.home, &dir, check, payload, index, venture),
            None => Ok(()),
        }
    }

    fn execute(&mut self, candidate: &Candidate) -> Result<Vec<Entry>, Failure> {
        match self.run_or_start(candidate, false)? {
            Started::Done(written) => Ok(written),
            // Only a start comes back running; should an execute ever, the
            // run happens here, recorded, then done, as ever.
            Started::Running(mut written, job) => {
                let (ok, detail, more) = job.run(&|| false);
                for record in std::iter::once(job.outcome(ok, detail)).chain(more) {
                    written.push(self.ledger.append(record).map_err(|e| e.to_string())?);
                }
                Ok(written)
            }
        }
    }

    fn infer(&mut self, request: &InferRequest) -> Result<String, String> {
        self.infer_now(request)
    }

    fn measure(&mut self, venture: &str) -> Result<(Option<f64>, String), String> {
        self.measure_now(venture)
    }

    fn read(&mut self, venture: &str, metric: &str) -> Result<(Option<f64>, String), String> {
        self.read_now(venture, metric)
    }

    fn confirm(&mut self, intent: Seq) -> Result<Option<(bool, String)>, String> {
        self.confirm_now(intent)
    }
}

/// What a start did: everything, or the intent of a run still to happen.
pub enum Started {
    Done(Vec<Entry>),
    Running(Vec<Entry>, Box<Job>),
}

/// What a lever run pulls.
#[derive(Clone, Debug)]
enum Pull {
    Run(String),
    Adapter {
        lever: adapter::Lever,
        secret: Option<String>,
        adapters: config::Adapters,
    },
    /// Ask the research backend a question.
    Research {
        config: Box<Config>,
        home: PathBuf,
        question: String,
        /// A forecast branch asks no backend.
        keyless: bool,
    },
    Nothing(String),
}

/// A lever run whose intent is written and which has not run yet: all
/// it needs, owned, so it can run on a thread of its own.
#[derive(Clone, Debug)]
pub struct Job {
    pub venture: String,
    pub action: String,
    /// The intent this run's outcome answers.
    pub intent: Seq,
    dir: PathBuf,
    pull: Pull,
    payload: Option<Payload>,
    key: String,
    proof: String,
    timeout: Duration,
}

impl Job {
    /// Pull the lever. `stop` says to stop it now: avsmd is stopping, or
    /// the venture was paused. Returns whether it went well, one line of
    /// what happened, and the records that go after its outcome.
    pub fn run(&self, stop: &dyn Fn() -> bool) -> (bool, String, Vec<Record>) {
        if let Pull::Research {
            config,
            home,
            question,
            keyless,
        } = &self.pull
        {
            let asked = if *keyless {
                Err("a forecast asks no backend".to_string())
            } else {
                research(config, home, question, &self.venture)
            };
            return match asked {
                Ok(f) => (
                    true,
                    format!(
                        "{} {}",
                        f.len(),
                        if f.len() == 1 { "finding" } else { "findings" }
                    ),
                    research::records(&self.venture, &f, self.intent),
                ),
                Err(e) => (false, e, vec![]),
            };
        }
        let (ok, detail) = self.pull(stop);
        (ok, detail, vec![])
    }

    fn pull(&self, stop: &dyn Fn() -> bool) -> (bool, String) {
        match (&self.pull, &self.payload) {
            (
                Pull::Adapter {
                    lever,
                    secret,
                    adapters,
                },
                Some(p),
            ) => lever.run(
                &self.dir,
                secret.as_deref(),
                adapters,
                p,
                &Occasion {
                    venture: &self.venture,
                    action: &self.action,
                    proof: &self.proof,
                },
            ),
            (Pull::Adapter { lever, .. }, None) => {
                (false, format!("{} needs a payload", lever.name()))
            }
            (Pull::Run(run), p) => run_lever_until(
                &self.dir,
                run,
                p.as_ref(),
                Some(&self.key),
                self.timeout,
                stop,
            ),
            (Pull::Nothing(why), _) => (false, why.clone()),
            (Pull::Research { .. }, _) => (false, "research runs apart".to_string()),
        }
    }

    /// The outcome of this run.
    pub fn outcome(&self, ok: bool, detail: String) -> Record {
        Record::Outcome {
            claim: Claim::Measured,
            venture: self.venture.clone(),
            intent: self.intent,
            ok,
            detail,
        }
    }
}

impl LocalWarden {
    /// Gate the action again and, if it passes, write its intent. With
    /// `apart`, a lever that runs a command or an adapter comes back
    /// running, for the caller to run and write the outcome of; else
    /// everything is done here.
    pub fn run_or_start(&mut self, candidate: &Candidate, apart: bool) -> Result<Started, Failure> {
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
        // A tripped guard's unwind takes the payload its adopted mandate
        // declares, and no other.
        let payload = match (proof.unwind(), proof.payload_from()) {
            (Some(guard), _) => ventures.get(&venture).and_then(|v| {
                let unwind = v.mandate.guard(guard)?.unwind.as_ref()?;
                unwind
                    .payload_for(v.mandate.permitted(&action)?)
                    .ok()
                    .flatten()
            }),
            (None, Some(seq)) => index.get(seq).and_then(|e| e.record.payload().cloned()),
            (None, None) => None,
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
            // The improving loop never edits limits: unless a human amended
            // the payload at approve, which was validated there and is
            // theirs, the only change is a lever turning delegated on.
            let amended = proof
                .payload_from()
                .and_then(|seq| index.get(seq))
                .is_some_and(|e| e.record.kind() == crate::record::Kind::Decision);
            if !amended {
                if let Some(field) = crate::mandate::edits_limits(&v.mandate, &mandate) {
                    return Err(Failure::Refused {
                        reason_key: EDITS_LIMITS.to_string(),
                        detail: format!(
                            "the new mandate changes {field}, which is not the improving loop's to edit; only a delegated lever turning on is; a human amends the payload at approve to change it"
                        ),
                    });
                }
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
            // A forecast branch adopts the text in its own ledger alone:
            // the venture's mandate file is the real one.
            let wrote = if self.keyless {
                Ok(())
            } else {
                std::fs::write(&path, &text)
            };
            let (ok, detail) = match wrote {
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
                return Ok(Started::Done(written));
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
            return Ok(Started::Done(written));
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
            return Ok(Started::Done(written));
        }
        if action == RESUME && !parent_lever {
            let started = append(intent(proof, None))?;
            written.push(append(outcome(&started, true, String::new()))?);
            written.insert(0, started);
            return Ok(Started::Done(written));
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
            return Ok(Started::Done(written));
        }
        if perm.is_forecast() {
            // Recorded, then done, like research: the forecast forks the
            // ledger at its head, and only its records and its asks come
            // back into this one.
            let mut append = |record: Record| self.ledger.append(record).map_err(|e| e.to_string());
            let started = append(intent(proof, None))?;
            if self.keyless {
                written.push(append(outcome(
                    &started,
                    true,
                    "a forecast branch does not forecast".to_string(),
                ))?);
                written.insert(0, started);
                return Ok(Started::Done(written));
            }
            let opts =
                crate::forecast::Options::for_lever(&venture, self.config.forecast_settings());
            let ran = crate::forecast::run(&self.ledger, &self.home, &opts);
            let mut append = |record: Record| self.ledger.append(record).map_err(|e| e.to_string());
            let (ok, detail) = match &ran {
                Ok(o) => (true, o.describe()),
                Err(e) => (false, e.clone()),
            };
            let done = append(outcome(&started, ok, detail))?;
            let seq = started.seq;
            written.push(started);
            written.push(done);
            if let Ok(o) = ran {
                let mut seqs = BTreeMap::new();
                for record in o.records(&[seq]) {
                    let e = append(record)?;
                    seqs.entry(e.record.venture().to_string())
                        .or_insert_with(Vec::new)
                        .push(e.seq);
                    written.push(e);
                }
                for record in crate::forecast::asks(index, &o, &seqs) {
                    written.push(append(record)?);
                }
            }
            return Ok(Started::Done(written));
        }
        if perm.is_research() {
            // Recorded, then done, like a lever: the backend's command may
            // take its time, so with `apart` it runs on a thread of its
            // own and its outcome and findings are written when it ends.
            let question = payload.as_ref().map(Payload::text).unwrap_or_default();
            let proof_text = proof.to_string();
            let started = append(intent(proof, payload.clone()))?;
            let job = Job {
                venture: venture.clone(),
                action: action.clone(),
                intent: started.seq,
                dir: self.home.clone(),
                pull: Pull::Research {
                    config: Box::new(self.config.clone()),
                    home: self.home.clone(),
                    question,
                    keyless: self.keyless,
                },
                payload,
                key,
                proof: proof_text,
                timeout: ACTION_TIMEOUT,
            };
            written.push(started);
            if apart {
                return Ok(Started::Running(written, Box::new(job)));
            }
            let (ok, detail, more) = job.run(&|| false);
            for record in std::iter::once(job.outcome(ok, detail)).chain(more) {
                written.push(append(record)?);
            }
            return Ok(Started::Done(written));
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
        // A forecast branch runs no check: its payload is a placeholder,
        // and the check is a command of the venture's.
        if let (Some(check), Some(p), false) = (&perm.check, payload.as_ref(), self.keyless) {
            payload_check(&self.home, &dir, check, p, index, &venture)?;
        }
        let pull = match (perm.lever_adapter(), &perm.run) {
            (Some(lever), _) => Pull::Adapter {
                secret: lever.key_name().and_then(|k| self.keys.get(k)).cloned(),
                lever,
                adapters: self.config.adapters.clone(),
            },
            (None, Some(run)) => Pull::Run(run.clone()),
            (None, None) => Pull::Nothing(format!("{action} has no command and no adapter")),
        };
        let proof_text = proof.to_string();
        // Reversible or not, the intent is stored before the lever runs.
        let started = append(intent(proof, payload.clone()))?;
        let (ok, detail) = if let Some(sim) = self.sim.as_deref() {
            sim.pull(
                index,
                &venture,
                &v.mandate,
                &dir,
                &action,
                &key,
                payload.as_ref(),
            )
        } else {
            let job = Job {
                venture: venture.clone(),
                action: action.clone(),
                intent: started.seq,
                dir,
                pull,
                payload,
                key,
                proof: proof_text,
                timeout: perm.timeout.unwrap_or(ACTION_TIMEOUT),
            };
            if apart {
                written.push(started);
                return Ok(Started::Running(written, Box::new(job)));
            }
            job.pull(&|| false)
        };
        let done = append(outcome(&started, ok, detail))?;
        written.push(started);
        written.push(done);
        Ok(Started::Done(written))
    }
}

impl LocalWarden {
    fn infer_now(&mut self, request: &InferRequest) -> Result<String, String> {
        if self.keyless {
            return Err("a forecast calls no backend".to_string());
        }
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

    fn measure_now(&mut self, venture: &str) -> Result<(Option<f64>, String), String> {
        self.measure_work(venture).and_then(|work| work())
    }

    fn read_now(&mut self, venture: &str, metric: &str) -> Result<(Option<f64>, String), String> {
        self.read_work(venture, metric).and_then(|work| work())
    }

    /// Measuring a venture's metric through its adapter, as work that
    /// owns all it needs: the warden process runs it on a thread of its
    /// own, so a slow adapter holds up nothing else (#45).
    pub fn measure_work(&mut self, venture: &str) -> Result<Work<Measured>, String> {
        if self.keyless {
            return Err("a forecast reads no adapter".to_string());
        }
        self.index
            .refresh(&self.ledger)
            .map_err(|e| e.to_string())?;
        let ventures = venture::from_adoptions(self.index.adoptions());
        self.measure_in(&ventures, venture)
    }

    /// Reading one of a venture's guards, audits or readings, as work
    /// that owns all it needs; see `measure_work`.
    pub fn read_work(&mut self, venture: &str, metric: &str) -> Result<Work<Measured>, String> {
        if self.keyless {
            // A forecast reads a guard from the world model's step, never
            // from the guard's command or adapter.
            let name = metric.strip_prefix("guard.").unwrap_or(metric);
            let read = match self.sim.as_ref().and_then(|s| s.guard(venture, name)) {
                Some(x) => Ok((Some(x), "simulated: world".to_string())),
                None => Err(format!(
                    "a forecast reads {metric} from the world model alone, and it gave none"
                )),
            };
            return Ok(Box::new(move || read));
        }
        self.index
            .refresh(&self.ledger)
            .map_err(|e| e.to_string())?;
        let ventures = venture::from_adoptions(self.index.adoptions());
        self.read_in(&ventures, venture, metric)
    }

    fn confirm_now(&mut self, intent: Seq) -> Result<Option<(bool, String)>, String> {
        if self.keyless {
            return Ok(None);
        }
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

/// A value, or None with a note saying why.
pub type Measured = (Option<f64>, String);

/// Work the warden does apart from its loop, owning all it needs.
pub type Work<T> = Box<dyn FnOnce() -> Result<T, String> + Send>;

/// Work whose answer is already known.
fn known<T: Send + 'static>(answer: T) -> Result<Work<T>, String> {
    Ok(Box::new(move || Ok(answer)))
}

impl LocalWarden {
    /// Read a guard's or an audit's command or built-in metric.
    fn read_in(
        &self,
        ventures: &BTreeMap<String, venture::Venture>,
        venture: &str,
        metric: &str,
    ) -> Result<Work<Measured>, String> {
        let v = ventures
            .get(venture)
            .ok_or_else(|| format!("no venture named {venture}"))?;
        let m = &v.mandate;
        if let Some(name) = metric.strip_prefix(crate::mandate::READING_PREFIX) {
            let r = m
                .readings
                .iter()
                .find(|r| r.name == name)
                .ok_or_else(|| format!("{venture} has no reading {name}"))?
                .clone();
            let dir = v.path.clone().unwrap_or_else(|| self.home.clone());
            // An audit names what may be kept: its latest trusted reading,
            // written by the kernel just before.
            let named: Option<String> = r.audit.as_deref().map(|a| {
                let at = self
                    .index
                    .latest_report(venture, &format!("{}{a}", crate::mandate::AUDIT_PREFIX));
                match at.and_then(|s| self.index.get(s)).map(|e| &e.record) {
                    Some(Record::Report { note, .. }) => note.clone(),
                    _ => String::new(),
                }
            });
            return Ok(Box::new(move || {
                // The note is the fields the reading names and nothing else
                // of what its command printed: not its other lines, and not
                // why it failed, since a model sees it.
                let ran = run_command_raw(&dir, &r.run, ACTION_TIMEOUT);
                let n = match adapter::number_from(&ran.stdout) {
                    Ok(n) if ran.ok => n,
                    _ => return Ok((None, String::new())),
                };
                // The number is no field, so `keep` drops it with the rest.
                Ok((Some(n), r.keep(&ran.stdout, named.as_deref())))
            }));
        }
        let audit = metric.starts_with(crate::mandate::AUDIT_PREFIX);
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
            let metric = metric.to_string();
            return Ok(Box::new(move || {
                let ran = run_command_raw(&dir, &cmd, ACTION_TIMEOUT);
                if !ran.ok {
                    return Ok((
                        None,
                        format!("{metric} failed: {}", ran.detail(ACTION_TIMEOUT)),
                    ));
                }
                Ok(match adapter::number_from(&ran.stdout) {
                    // An audit's lines above its number are its note, like
                    // the senders of the mail it counts: no model sees an
                    // audit, and a payload check may read them. A guard
                    // keeps only its number.
                    Ok(n) if audit => (Some(n), above_number(&ran.stdout)),
                    Ok(n) => (Some(n), String::new()),
                    Err(_) => (
                        None,
                        format!(
                            "{metric} did not print a number: {}",
                            ran.detail(ACTION_TIMEOUT)
                        ),
                    ),
                })
            }));
        }
        let spec = spec.ok_or_else(|| format!("{metric} reads nothing"))?;
        let reading = adapter::Metric::parse(&spec)?
            .ok_or_else(|| format!("{metric}: {spec:?} is not a built-in metric"))?;
        if let Some(host) = reading.host(&self.config.adapters) {
            if !m.allows_host(&host) {
                return known((
                    None,
                    format!(
                        "{} would reach {host}, which is not in the venture's network list",
                        reading.name()
                    ),
                ));
            }
        }
        let key = reading.key_name().and_then(|k| self.keys.get(k)).cloned();
        let adapters = self.config.adapters.clone();
        Ok(Box::new(move || {
            Ok(reading.measure(&dir, key.as_deref(), &adapters))
        }))
    }

    fn measure_in(
        &self,
        ventures: &BTreeMap<String, venture::Venture>,
        venture: &str,
    ) -> Result<Work<Measured>, String> {
        let v = ventures
            .get(venture)
            .ok_or_else(|| format!("no venture named {venture}"))?;
        let metric = v
            .mandate
            .metric_adapter()
            .ok_or_else(|| format!("{venture} watches {}, not an adapter", v.mandate.metric))?;
        if let Some(host) = metric.host(&self.config.adapters) {
            if !v.mandate.allows_host(&host) {
                return known((
                    None,
                    format!(
                        "{} would reach {host}, which is not in the venture's network list",
                        metric.name()
                    ),
                ));
            }
        }
        let dir = v.path.clone().unwrap_or_else(|| self.home.clone());
        let key = metric.key_name().and_then(|k| self.keys.get(k)).cloned();
        let adapters = self.config.adapters.clone();
        Ok(Box::new(move || {
            Ok(metric.measure(&dir, key.as_deref(), &adapters))
        }))
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

/// The improving loop never edits limits.
pub const EDITS_LIMITS: &str = "EditsLimits";
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
    // Absolute: the check runs in the venture directory, and a home given
    // as `--home home` is relative to where avsm runs.
    let file = home.join(format!("measured-{}-{}.json", std::process::id(), now()));
    let file = std::path::absolute(&file).unwrap_or(file);
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
    /// Killed: at the timeout, or because avsmd was stopping (`stopped`).
    pub killed: bool,
    /// Killed because avsmd or its warden was stopping, not because the
    /// run took too long: the timeout is no part of the story.
    pub stopped: bool,
    /// The program could not be started at all; `stderr` says why and how
    /// to fix it.
    pub unstarted: bool,
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
        if self.unstarted {
            tail(&text)
        } else if self.killed && self.stopped {
            tail(format!("killed: avsmd was stopping  {text}").trim())
        } else if self.killed {
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

/// Set in the warden process when avsmd is stopping.
static DRAINING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// How long a run in flight has, once avsmd is stopping, between SIGTERM
/// and SIGKILL: inside the daemon's drain bound, with time left to write
/// the outcome.
pub const DRAIN_GRACE: Duration = Duration::from_secs(7);

extern "C" fn on_drain(_: libc::c_int) {
    DRAINING.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// In the warden process: SIGTERM and SIGINT drain instead of killing it.
/// The run in flight gets SIGTERM, then SIGKILL after `DRAIN_GRACE`, and
/// its outcome is written either way.
pub fn drain_on_signals() {
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGTERM, on_drain as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_drain as *const () as libc::sighandler_t);
    }
}

/// In the warden process: drain now, as a SIGTERM would. For when the
/// kernel hangs up with runs still going.
pub fn drain_now() {
    DRAINING.store(true, std::sync::atomic::Ordering::SeqCst);
}

fn draining() -> bool {
    DRAINING.load(std::sync::atomic::Ordering::Relaxed)
}

/// Signal a child and everything it started: it leads a process group of
/// its own.
fn signal_group(child: &std::process::Child, signal: libc::c_int) {
    // SAFETY: kill has no memory preconditions.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), signal);
    }
}

/// Is anything left in the group `child` leads?
fn group_alive(child: &std::process::Child) -> bool {
    // SAFETY: kill with signal 0 only checks; it has no memory preconditions.
    unsafe { libc::kill(-(child.id() as libc::pid_t), 0) == 0 }
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
    run_draining(dir, program, args, env, stdin, timeout, cap, &|| {
        draining() || crate::groups::stopped()
    })
}

/// `run_bounded`, told by `draining` when avsmd is stopping.
#[allow(clippy::too_many_arguments)]
fn run_draining(
    dir: &Path,
    program: &str,
    args: &[&str],
    env: &[(&str, &str)],
    stdin: &str,
    timeout: Duration,
    cap: usize,
    draining: &dyn Fn() -> bool,
) -> Ran {
    let mut command = Command::new(program);
    // SAFETY: the hook only calls prctl and getppid, which are
    // async-signal-safe.
    unsafe {
        command.pre_exec(crate::groups::die_with_parent);
    }
    let spawning = crate::groups::SPAWNING
        .read()
        .unwrap_or_else(|p| p.into_inner());
    let spawned = command
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string()),
        )
        .env("HOME", dir)
        // The time avsm reads: real, or a simulation's.
        .env("AVSM_NOW", crate::clock::now().to_string())
        .envs(env.iter().copied())
        .stdin(if stdin.is_empty() {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A group of its own, so a stop or a timeout reaches what it
        // started too, and a ^C at the terminal reaches only avsmd.
        .process_group(0)
        .spawn();
    std::mem::drop(spawning);
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            return Ran {
                stderr: did_not_run(program, &e),
                unstarted: true,
                ..Ran::default()
            };
        }
    };
    // Kept, and told to the daemon from the warden, until it is gone: a
    // warden that dies takes the run's timer with it, and the daemon ends
    // the group in its place.
    let pgid = child.id() as i32;
    crate::groups::started(pgid);
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
    // When this run was told avsmd is stopping.
    let mut drained: Option<Instant> = None;
    // PERF: most children exit within milliseconds; poll fast, then back off.
    let mut wait = Duration::from_millis(1);
    // The leader's exit, once it has exited.
    let mut exited = None;
    let mut killed = false;
    let mut stopped = false;
    loop {
        if exited.is_none() {
            match child.try_wait() {
                Ok(Some(status)) => exited = Some(status),
                Ok(None) => {}
                Err(_) => break,
            }
        }
        // NOTE: a drain ends with the group, not with its leader. A leader
        // that forks (dash's `sh -c ./lever` does) dies on the SIGTERM
        // while what it started ignores it and holds the output pipes:
        // the drain still owes that group its SIGKILL.
        let alive = exited.is_none() || (drained.is_some() && group_alive(&child));
        if !alive {
            break;
        }
        if began.elapsed() > timeout || drained.is_some_and(|d| d.elapsed() > DRAIN_GRACE) {
            stopped = began.elapsed() <= timeout;
            signal_group(&child, libc::SIGKILL);
            let _ = child.kill();
            let _ = child.wait();
            killed = true;
            break;
        }
        if drained.is_none() && draining() {
            signal_group(&child, libc::SIGTERM);
            drained = Some(Instant::now());
            continue;
        }
        std::thread::sleep(wait);
        wait = (wait * 2).min(Duration::from_millis(20));
    }
    let status = exited.filter(|_| !killed);
    let (stdout, stderr) = reader.join().unwrap_or_default();
    // What the leader left behind in its group stays known until it goes.
    if !crate::groups::alive(pgid) {
        crate::groups::ended(pgid);
    }
    Ran {
        ok: status.is_some_and(|s| s.success()),
        stdout,
        stderr,
        code: status.and_then(|s| s.code()),
        killed: status.is_none(),
        stopped: stopped && status.is_none(),
        unstarted: false,
    }
}

/// Why a program did not start, and the fix: the system's own words for
/// it say "No such file or directory" even when the file is there and its
/// first line names an interpreter that does not exist, as a `#!/bin/sh`
/// with a Windows line ending does.
fn did_not_run(program: &str, e: &std::io::Error) -> String {
    use std::io::ErrorKind;
    let path = Path::new(program);
    match e.kind() {
        ErrorKind::PermissionDenied => format!(
            "did not run: {program} is not executable (permission denied); fix: chmod +x {program}"
        ),
        ErrorKind::NotFound if crate::venture::crlf_shebang(path) => format!(
            "did not run: {program} starts with a Windows line ending (#!...\\r), so the system cannot find its interpreter; fix: {}",
            crate::venture::crlf_fix(path)
        ),
        ErrorKind::NotFound => {
            format!("did not run: {program} does not exist; fix: create it, or check the path")
        }
        _ => format!("did not run: {program}: {e}"),
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

/// `run_lever`, stopped as a drain stops it once `stop` says so: the
/// venture was paused. The outcome then says it was stopped, not timed
/// out.
pub fn run_lever_until(
    dir: &Path,
    command: &str,
    payload: Option<&Payload>,
    key: Option<&str>,
    timeout: Duration,
    stop: &dyn Fn() -> bool,
) -> (bool, String) {
    let (mut env, stdin): (Vec<(&str, String)>, String) = match payload {
        Some(p) => (vec![("AVSM_PAYLOAD", p.takes().to_string())], p.text()),
        None => (vec![], String::new()),
    };
    if let Some(k) = key {
        env.push(("AVSM_KEY", k.to_string()));
    }
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let ran = run_draining(
        dir,
        "sh",
        &["-c", command],
        &env,
        &stdin,
        timeout,
        usize::MAX,
        &|| draining() || stop() || crate::groups::stopped(),
    );
    if !ran.ok && stop() && !draining() {
        let detail = Ran {
            killed: false,
            code: ran.code.or(Some(-1)),
            ..ran
        }
        .detail(timeout);
        return (
            false,
            tail(&format!("stopped: its venture was paused  {detail}")),
        );
    }
    (ran.ok, ran.detail(timeout))
}

/// The most an audit's note keeps of the lines above its number.
pub const AUDIT_NOTE: usize = 2000;

/// The non-empty lines above the last one, joined by two spaces, at most
/// `AUDIT_NOTE` characters from the top.
fn above_number(stdout: &str) -> String {
    let lines: Vec<&str> = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let above = lines.len().saturating_sub(1);
    lines[..above].join("  ").chars().take(AUDIT_NOTE).collect()
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
    /// Execute, but a lever run goes on apart once its intent is written.
    Start(Candidate),
    /// Stop a venture's lever runs still going.
    Cancel(String),
    Infer(InferRequest),
    Measure(String),
    Confirm(Seq),
    /// A venture and a guard's or an audit's metric.
    Read(String, String),
    /// A venture, a lever and a payload: run the lever's check.
    Check(String, String, Payload),
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
    /// The check passed, or the lever has none.
    Checked,
    /// How many runs a cancel stopped.
    Cancelled(u32),
    /// A read or a measure still going, and for how long; ask again.
    Pending(String),
    Err(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--home home` is relative to where avsm runs; a check runs in the
    /// venture directory and must still find the reports it is handed.
    #[test]
    fn a_check_finds_its_reports_when_the_home_is_relative() {
        // The home under this process's working directory, named relative
        // to it; the venture elsewhere.
        let cwd = std::env::current_dir().unwrap();
        let here = tempfile::tempdir_in(&cwd).unwrap();
        let relative = here.path().strip_prefix(&cwd).unwrap().join("home");
        std::fs::create_dir_all(&relative).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("venture");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(relative.is_relative());
        let check = "test -s \"$AVSM_MEASURED\"";
        let payload = Payload::Text {
            text: "hi".to_string(),
        };
        let index = Index::from_entries(&[]);
        payload_check(&relative, &dir, check, &payload, &index, "v")
            .unwrap_or_else(|e| panic!("{e}"));
    }

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

    /// The leader forks, dies on the drain's SIGTERM, and leaves a child
    /// that ignores it holding stdout: the drain still ends the group
    /// within its grace, and the run reads as killed.
    #[test]
    fn a_drain_kills_what_the_leader_left_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let began = Instant::now();
        let ran = run_draining(
            tmp.path(),
            "sh",
            &[
                "-c",
                "sh -c \"trap '' TERM; touch started; sleep 60\"; true",
            ],
            &[],
            "",
            Duration::from_secs(120),
            usize::MAX,
            &|| tmp.path().join("started").exists(),
        );
        let took = began.elapsed();
        assert!(ran.killed && !ran.ok, "{ran:?}");
        assert!(
            took < DRAIN_GRACE + Duration::from_secs(3),
            "took {took:?}, past the grace"
        );
    }

    #[test]
    fn a_run_stopped_with_avsmd_is_not_blamed_on_its_timeout() {
        let huge = Duration::from_secs(3_155_760_000);
        let stopped = Ran {
            killed: true,
            stopped: true,
            ..Ran::default()
        };
        let d = stopped.detail(huge);
        assert!(
            !d.contains("killed after") && d.contains("avsmd was stopping"),
            "{d}"
        );
        let late = Ran {
            killed: true,
            ..Ran::default()
        };
        assert!(late.detail(huge).starts_with("killed after 3155760000s"));
        let unstarted = Ran {
            unstarted: true,
            stderr: "did not run: x".into(),
            ..Ran::default()
        };
        assert_eq!(unstarted.detail(huge), "did not run: x");
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
        // Clean but for the time (AVSM_NOW, deliberate: the greenhouse reads it).
        let avsm: Vec<&str> = out
            .split_whitespace()
            .filter(|w| w.starts_with("AVSM"))
            .map(|w| w.split('=').next().unwrap())
            .collect();
        assert_eq!(avsm, vec!["AVSM_NOW"], "clean environment: {out}");
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
