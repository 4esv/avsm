//! Simulated time: run the kernel over days in minutes. The clock jumps
//! forward a step at a time; everything inside a step, model calls
//! included, takes as long as it really takes. A policy stands in for the
//! human, and every decision it makes is recorded as declared by
//! `simulation`, so a ledger never claims a person decided what a script
//! did. Runs in one process with its own warden; never on a home a daemon
//! is serving, and never on the default home.

use std::io::Write;
use std::path::Path;

use crate::clock;
use crate::gate;
use crate::kernel::{default_home, socket_path, Kernel};
use crate::protocol;
use crate::record::Seq;
use crate::role::ModelRole;
use crate::warden::LocalWarden;

/// Who answers the asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Human {
    /// Approve every open ask, each step.
    ApproveAll,
    /// Leave every ask open.
    Away,
}

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
    let warden = LocalWarden::open(home)?;
    let mut kernel = Kernel::with(home, Box::new(ModelRole), Box::new(warden))?;
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
            }
        ),
    );
    while clock::now() < end {
        kernel.tick();
        if plan.human == Human::ApproveAll {
            let entries = kernel.entries()?;
            for q in gate::open_asks(&entries) {
                let args = vec!["approve".to_string(), q.to_string()];
                if let Err(e) = kernel.handle_as(&args, Some(BY)) {
                    say(out, &format!("  (could not approve #{q}: {})\n", e.trim()));
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

use chrono::Timelike;
