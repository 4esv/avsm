//! Simulated time: run the kernel over days in minutes. The clock jumps
//! forward a step at a time; everything inside a step, model calls
//! included, takes as long as it really takes. A policy stands in for the
//! human, and every decision it makes is recorded as declared by
//! `simulation`, so a ledger never claims a person decided what a script
//! did. Runs in one process with its own warden; never on a home a daemon
//! is serving, and never on the default home.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::clock;
use crate::index::Index;
use crate::kernel::{default_home, socket_path, Kernel};
use crate::mandate::{same_command, Mandate};
use crate::protocol;
use crate::record::{Payload, Seq};
use crate::world::{self, Action};

/// Who answers the asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Human {
    /// Approve every open ask, each step.
    ApproveAll,
    /// Leave every ask open.
    Away,
    /// Deny every third ask it answers; approve the rest, a text payload
    /// with a line added first, as `approve --edit` would (a mandate with
    /// the line as a comment, so it still parses).
    Mixed,
}

/// The line the mixed human adds to a text payload before approving it.
pub const EDIT: &str = "Edited by simulation.";

/// The name declared records carry when the policy decided.
pub const BY: &str = "simulation";

/// How long to simulate and how.
#[derive(Clone, Debug)]
pub struct Plan {
    pub days: u32,
    /// Seconds the clock jumps each step.
    pub step: i64,
    pub human: Human,
    /// Unix seconds to start at; None continues after the ledger's last
    /// record, or starts at local midnight today on an empty ledger.
    pub start: Option<i64>,
    /// Venture directories to take on at the start, if not taken on yet.
    pub add: Vec<std::path::PathBuf>,
    /// A command that moves the world on, run before every step with the
    /// simulated time in `AVSM_NOW` (Unix seconds) and `AVSM_DAY` (from
    /// 0). What it prints is shown as the world's; if it fails, the
    /// simulation stops. The world is not avsm's: it leaves no record.
    ///
    /// Every lever run goes to it too, never to the lever: it is called
    /// in the venture directory with `AVSM_LEVER=<venture>/<action>`,
    /// `AVSM_KEY` and the payload on standard input; its output is the
    /// outcome's detail and its exit code whether the run was ok. Without
    /// one, a lever run steps the venture's world model, and the reports
    /// come from the model's state.
    pub world: Option<String>,
}

/// Where a simulation sends lever runs instead of the levers. Built only
/// by `run` and by a forecast's branches, and handed to the kernel and the
/// warden as they are made: no configuration and no command can set it,
/// so a running daemon never routes a lever anywhere but to the lever.
pub(crate) struct Simulation {
    /// The `--world` command, if one was given.
    world: Option<String>,
    /// Each venture's world-model state, and when it was stepped last.
    states: Mutex<BTreeMap<String, (Value, i64)>>,
    /// Each venture's guard readings from its world model's last step.
    guards: Mutex<BTreeMap<String, BTreeMap<String, f64>>>,
    /// A forecast branch's seed, mixed into every step's; None for
    /// `avsm simulate`, whose steps stay as they were.
    seed: Option<u64>,
}

impl Simulation {
    pub(crate) fn new(world: Option<String>) -> Simulation {
        Simulation {
            world,
            states: Mutex::new(BTreeMap::new()),
            guards: Mutex::new(BTreeMap::new()),
            seed: None,
        }
    }

    /// The simulation of one forecast branch: no `--world`, and every
    /// step's seed mixed with the branch's.
    pub(crate) fn branch(seed: u64) -> Simulation {
        Simulation {
            seed: Some(seed),
            ..Simulation::new(None)
        }
    }

    /// A step's seed, mixed with the branch's when there is one.
    fn salt(&self, seed: u64) -> u64 {
        match self.seed {
            Some(branch) => world::mix(seed, branch),
            None => seed,
        }
    }

    /// A guard reading the venture's world model gave at its last step.
    pub(crate) fn guard(&self, venture: &str, name: &str) -> Option<f64> {
        let guards = self.guards.lock().unwrap_or_else(|e| e.into_inner());
        guards.get(venture)?.get(name).copied()
    }

    fn keep_guards(&self, venture: &str, guards: &BTreeMap<String, f64>) {
        if guards.is_empty() {
            return;
        }
        let mut all = self.guards.lock().unwrap_or_else(|e| e.into_inner());
        all.insert(venture.to_string(), guards.clone());
    }

    /// Run a lever in the simulation: through the `--world` command, or a
    /// step of the venture's world model. Returns whether it was ok and
    /// the outcome's detail, which names where it went.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn pull(
        &self,
        index: &Index,
        venture: &str,
        mandate: &Mandate,
        dir: &Path,
        action: &str,
        key: &str,
        payload: Option<&Payload>,
    ) -> (bool, String) {
        let now = clock::now();
        if let Some(cmd) = &self.world {
            // The time is in AVSM_NOW, as for every command the warden runs.
            let lever = format!("{venture}/{action}");
            let mut env = vec![("AVSM_LEVER", lever.as_str()), ("AVSM_KEY", key)];
            let shape = payload.map(|p| p.takes().to_string());
            if let Some(s) = &shape {
                env.push(("AVSM_PAYLOAD", s.as_str()));
            }
            let stdin = payload.map(Payload::text).unwrap_or_default();
            let timeout = crate::warden::ACTION_TIMEOUT;
            let ran = crate::warden::run_with(dir, "sh", &["-c", cmd], &env, &stdin, timeout);
            let detail = ran.detail(timeout);
            return (
                ran.ok,
                format!("simulated: world  {detail}").trim().to_string(),
            );
        }
        let model = world::model_for(index, venture, mandate, Some(dir));
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        let state = states.get(venture).map_or(Value::Null, |(s, _)| s.clone());
        let act = Action {
            name: action.to_string(),
            payload: payload.map(Payload::text),
        };
        match model.step(&state, Some(&act), self.salt(world::seed_of(key)), now, 0) {
            Ok(next) => {
                self.keep_guards(venture, &next.guards);
                states.insert(venture.to_string(), (next.state, now));
                (
                    true,
                    format!(
                        "simulated: {}  {}={}",
                        model.name(),
                        mandate.metric_name(),
                        next.metric
                    ),
                )
            }
            Err(e) => (false, format!("simulated: {}  {e}", model.name())),
        }
    }

    /// Read a venture's metric from its world model's state, when there
    /// is no `--world` to move the real files. None means read it for
    /// real: there is a `--world`, or the baseline has nothing yet to
    /// start from, so the first reading seeds it.
    pub(crate) fn measure(
        &self,
        index: &Index,
        venture: &str,
        mandate: &Mandate,
        dir: &Path,
    ) -> Option<(Option<f64>, String)> {
        if self.world.is_some() {
            return None;
        }
        let now = clock::now();
        let model = world::model_for(index, venture, mandate, Some(dir));
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        let (state, at) = states
            .get(venture)
            .cloned()
            .unwrap_or((Value::Null, now - mandate.cadence.as_secs() as i64));
        let seed = self.salt(world::mix(world::seed_of(venture), now as u64));
        match model.step(&state, None, seed, now, now - at) {
            Ok(next) => {
                self.keep_guards(venture, &next.guards);
                states.insert(venture.to_string(), (next.state, now));
                Some((Some(next.metric), format!("simulated: {}", model.name())))
            }
            // A forecast branch never reads for real: a report program is
            // the venture's, and may change what it reads.
            Err(_) if model.name() == world::BASELINE && state.is_null() && self.seed.is_some() => {
                Some((None, "simulated: no reading yet to start from".to_string()))
            }
            Err(_) if model.name() == world::BASELINE && state.is_null() => None,
            Err(e) => Some((None, format!("simulated: {}  {e}", model.name()))),
        }
    }
}

/// Run the plan, writing what happens to `out`: every new record as a
/// ledger line, the human's answers, and the digest each simulated day.
pub fn run(home: &Path, plan: &Plan, out: &mut dyn Write) -> Result<(), String> {
    if home == default_home() {
        return Err(format!(
            "refusing to simulate in {}: that is your real home; pass --home <a directory for the simulation>",
            home.display()
        ));
    }
    if protocol::alive(&socket_path(home)) {
        return Err(format!(
            "avsm is running at {}; a simulation needs a home of its own",
            home.display()
        ));
    }
    if plan.step < 1 {
        return Err("step must be at least one second".to_string());
    }
    // Open on real time to read where the ledger stands, then set the clock.
    let probe = Kernel::open(home)?;
    let last = probe.entries()?.last().map(|e| e.at);
    // The simulation world stands in for every lever; a venture's world
    // model only forecasts. The two are never the same command.
    if let Some(cmd) = &plan.world {
        let mut worlds: Vec<(String, String)> = probe
            .ventures
            .values()
            .filter_map(|v| Some((v.name.clone(), v.mandate.world.clone()?)))
            .collect();
        for dir in &plan.add {
            let (m, _, _) = crate::venture::read_mandate(dir)?;
            if let Some(w) = m.world {
                worlds.push((dir.display().to_string(), w));
            }
        }
        if let Some((who, _)) = worlds.iter().find(|(_, w)| same_command(w, cmd)) {
            return Err(format!(
                "--world {cmd:?} is {who}'s world model; the simulation world stands in for the levers and must be another command"
            ));
        }
    }
    drop(probe);
    let start = match (plan.start, last) {
        (Some(t), Some(l)) if t <= l => {
            return Err(format!("the ledger already reaches {l}; start after it"))
        }
        (Some(t), _) => t,
        (None, Some(l)) => l + plan.step,
        (None, None) => {
            let now = clock::local();
            now.timestamp() - i64::from(now.num_seconds_from_midnight())
        }
    };
    clock::set(start);
    let sim = Arc::new(Simulation::new(plan.world.clone()));
    let mut kernel = Kernel::simulated(home, sim)?;
    kernel.adopt_self()?;
    for dir in &plan.add {
        let name = crate::venture::name_of(
            &dir.canonicalize()
                .map_err(|e| format!("{}: {e}", dir.display()))?,
        )?;
        if !kernel.ventures.contains_key(&name) {
            let args = vec!["add".to_string(), dir.to_string_lossy().into_owned()];
            kernel.handle_as(&args, Some(BY))?;
        }
    }
    let end = start + i64::from(plan.days) * 86_400;
    let mut shown: Seq = kernel.entries()?.last().map_or(0, |e| e.seq);
    let mut day = clock::local().date_naive();
    let say = |out: &mut dyn Write, text: &str| {
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
    };
    say(
        out,
        &format!(
            "simulating {} days from {} in steps of {}s; the human {}\n",
            plan.days,
            clock::local().format("%Y-%m-%d %H:%M %Z"),
            plan.step,
            match plan.human {
                Human::ApproveAll => "approves every ask",
                Human::Away => "is away",
                Human::Mixed => "denies every third ask and edits text payloads",
            }
        ),
    );
    let mut answered = 0u64;
    while clock::now() < end {
        if let Some(cmd) = &plan.world {
            let ran = std::process::Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .env("AVSM_NOW", clock::now().to_string())
                .env("AVSM_DAY", ((clock::now() - start) / 86_400).to_string())
                .output()
                .map_err(|e| format!("cannot run the world ({cmd}): {e}"))?;
            for line in String::from_utf8_lossy(&ran.stdout).lines() {
                say(out, &format!("  world: {line}\n"));
            }
            if !ran.status.success() {
                kernel.shutdown();
                return Err(format!(
                    "the world failed at {}: {}",
                    clock::local().format("%Y-%m-%d %H:%M"),
                    String::from_utf8_lossy(&ran.stderr).trim()
                ));
            }
        }
        kernel.tick();
        if plan.human != Human::Away {
            for q in kernel.open_asks()? {
                let args = answer(&kernel, plan.human, q, answered);
                answered += 1;
                if let Err(e) = kernel.handle_as(&args, Some(BY)) {
                    say(
                        out,
                        &format!("  (could not {} #{q}: {})\n", args[0], e.trim()),
                    );
                }
            }
        }
        for e in kernel.ledger.after(shown).map_err(|e| e.to_string())? {
            say(out, &format!("{}\n", e.line()));
            shown = e.seq;
        }
        clock::advance(plan.step);
        let today = clock::local().date_naive();
        if today != day {
            say(
                out,
                &format!("\n=== morning of {today} ===\n{}\n", kernel.digest()?),
            );
            day = today;
        }
    }
    say(out, &format!("=== end ===\n{}", kernel.status()?));
    kernel.shutdown();
    Ok(())
}

/// What the human says to ask `q`, the `n`th it has answered.
pub(crate) fn answer(kernel: &Kernel, human: Human, seq: Seq, n: u64) -> Vec<String> {
    let q = seq.to_string();
    if human == Human::Mixed {
        if n % 3 == 2 {
            return vec![
                "deny".to_string(),
                q,
                "the simulation denies every third ask".to_string(),
            ];
        }
        if let Some(text) = kernel
            .payload(seq)
            .ok()
            .and_then(|p| p.strip_prefix("text\n").map(str::to_string))
        {
            return vec![
                "approve".to_string(),
                q,
                "--payload".to_string(),
                edited(&text),
            ];
        }
    }
    vec!["approve".to_string(), q]
}

/// A text payload as the mixed human saves it: a line added at the end,
/// or, when the text is a mandate, the same line as a YAML comment, so
/// the mandate it approves still parses.
pub fn edited(text: &str) -> String {
    let text = text.trim_end();
    if Mandate::parse(text).is_ok() {
        format!("{text}\n# {EDIT}\n")
    } else {
        format!("{text}\n\n{EDIT}\n")
    }
}

use chrono::Timelike;

#[cfg(test)]
mod tests {
    use super::*;

    /// A mandate the ratchet would put in a delegate ask, with a lever
    /// list a bare line would break.
    const MANDATE: &str = "purpose: Keep the tomato alive.\nmetric: moisture\n\
        controller: {kind: threshold, below: 35, action: water}\ncadence: 2h\n\
        permitted:\n  - name: water\n    run: ./water\n    cap: 4/day\n    reversible: true\n    delegated: true\n";

    #[test]
    fn the_mixed_human_edits_a_mandate_with_a_comment_and_it_still_parses() {
        let before = Mandate::parse(MANDATE).unwrap();
        let text = edited(MANDATE);
        assert!(text.contains(&format!("# {EDIT}")), "{text}");
        let after = Mandate::parse(&text).unwrap_or_else(|e| panic!("{e}:\n{text}"));
        assert_eq!(before, after, "a comment changes nothing");
    }

    #[test]
    fn the_mixed_human_edits_other_text_with_a_line() {
        assert_eq!(
            edited("A title\nA body\n"),
            format!("A title\nA body\n\n{EDIT}\n")
        );
    }
}
