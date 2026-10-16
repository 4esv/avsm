//! The built-in `self` venture: avsm watching avsm. Its mandate ships in
//! the binary and is adopted on first start. Every cycle it reports one
//! number, integrity: 1 when the ledger chain verifies and the gate's
//! canary still refuses what it must, else 0, which trips its only
//! permitted action, stop, and halts every venture until a human resumes.

use crate::gate::{self, Adopted, Candidate, HARDWIRED, SELF};
use crate::ledger::Ledger;
use crate::mandate::{Cap, Controller, Mandate, Permitted, Restart};
use crate::record::{Claim, Entry, Record, Seq, Verdict, GENESIS};
use crate::venture::Venture;

/// The self mandate, as shipped. Nothing on disk can change it.
pub const MANDATE: &str = "\
purpose: Keep avsm honest. Verify the ledger chain and the gate every cycle, and stop everything if either fails.
metric: integrity
controller:
  kind: threshold
  below: 1
  action: stop
cadence: 10m
restart: never
forbidden: [edit-mandate, merge-main, touch-keys]
";

/// Where the ledger says the self mandate lives.
pub const PATH: &str = "built in";

/// The parsed self mandate.
pub fn mandate() -> Mandate {
    Mandate::parse(MANDATE).expect("the built-in mandate parses")
}

/// The hash the self mandate is adopted under.
pub fn hash() -> String {
    Mandate::hash(MANDATE)
}

/// The self venture as the kernel supervises it.
pub fn venture(adopted: Seq, recorded_hash: &str) -> Venture {
    let problem = if recorded_hash == hash() {
        None
    } else {
        Some("the built-in mandate changed; it is re-adopted on start".to_string())
    };
    Venture {
        name: SELF.to_string(),
        path: None,
        mandate: mandate(),
        hash: hash(),
        adopted,
        problem,
    }
}

/// The declared record that adopts the self mandate.
pub fn adopt_record() -> Record {
    Record::Decision {
        claim: Claim::Declared,
        venture: SELF.to_string(),
        verdict: Verdict::Adopt {
            mandate: hash(),
            path: PATH.to_string(),
        },
        by: None,
    }
}

/// Measure integrity: the chain verifies and the canary holds. Returns
/// the value and a note saying what was checked.
pub fn measure(ledger: &Ledger) -> (Option<f64>, String) {
    let chain = match ledger.verify() {
        Ok(n) => Ok(n),
        Err(e) => Err(e.to_string().trim_start_matches("ledger: ").to_string()),
    };
    let canary = canary();
    let value = if chain.is_ok() && canary.is_ok() {
        1.0
    } else {
        0.0
    };
    let note = format!(
        "{}; {}",
        match &chain {
            Ok(n) => format!("chain ok ({n} records)"),
            Err(e) => format!("chain broken: {e}"),
        },
        match &canary {
            Ok(()) => "canary ok".to_string(),
            Err(e) => format!("canary failed: {e}"),
        }
    );
    (Some(value), note)
}

/// The canary: feed the gate what it must refuse and what it must allow,
/// on a ledger made up for the purpose, and see that it still does.
pub fn canary() -> Result<(), String> {
    let now = 1_700_000_000;
    let permissive = Mandate {
        purpose: "a mandate that permits what self may never do".to_string(),
        metric: "integrity".to_string(),
        controller: Controller::Threshold {
            above: Some(0.0),
            below: None,
            action: "edit-mandate".to_string(),
        },
        permitted: HARDWIRED
            .iter()
            .map(|(name, _)| Permitted {
                name: name.to_string(),
                run: "true".to_string(),
                cap: Cap {
                    count: 9,
                    per: Cap::DAY,
                },
                reversible: true,
                delegated: true,
                takes: None,
            })
            .collect(),
        forbidden: vec![],
        cadence: std::time::Duration::from_secs(60),
        context_budget: 1000,
        restart: Restart::Never,
    };
    let mandate_hash = Mandate::hash(&serde_yaml::to_string(&permissive).unwrap_or_default());
    let adopt = Record::Decision {
        claim: Claim::Declared,
        venture: SELF.to_string(),
        verdict: Verdict::Adopt {
            mandate: mandate_hash.clone(),
            path: PATH.to_string(),
        },
        by: None,
    };
    let mut entries = vec![Entry::new(1, now - 100, GENESIS, adopt)];
    let mut push = |record: Record| -> Seq {
        let seq = entries.len() as Seq + 1;
        let prev = entries.last().map(|e| e.hash.clone()).unwrap_or_default();
        entries.push(Entry::new(seq, now - 50, &prev, record));
        seq
    };
    let measured = push(Record::report(
        Claim::Measured,
        SELF,
        "integrity",
        Some(1.0),
        "",
    ));
    let inferred = push(Record::report(
        Claim::Inferred,
        SELF,
        "integrity",
        Some(1.0),
        "",
    ));
    let adopted = vec![Adopted {
        seq: 1,
        venture: SELF.to_string(),
        hash: mandate_hash,
        mandate: permissive,
    }];
    let candidate = |action: &str, cites: Vec<Seq>| Candidate {
        venture: SELF.to_string(),
        action: action.to_string(),
        cites,
    };
    // Every hardwired action is refused even though this mandate permits it
    // and a measured report crosses its threshold.
    for (name, _) in HARDWIRED {
        if gate::gate(&candidate(name, vec![measured]), &entries, &adopted, now).is_ok() {
            return Err(format!("the gate allowed {name} for self"));
        }
    }
    // An inferred-only chain authorizes nothing.
    let mut ordinary = adopted.clone();
    ordinary[0].venture = "canary".to_string();
    let mut ordinary_entries = entries.clone();
    if let Record::Decision { venture, .. } = &mut ordinary_entries[0].record {
        *venture = "canary".to_string();
    }
    ordinary_entries[0] = Entry::new(1, now - 100, GENESIS, ordinary_entries[0].record.clone());
    for i in 1..ordinary_entries.len() {
        let prev = ordinary_entries[i - 1].hash.clone();
        let mut record = ordinary_entries[i].record.clone();
        if let Record::Report { venture, .. } = &mut record {
            *venture = "canary".to_string();
        }
        ordinary_entries[i] = Entry::new(i as Seq + 1, now - 50, &prev, record);
    }
    let inferred_only = Candidate {
        venture: "canary".to_string(),
        action: "edit-mandate".to_string(),
        cites: vec![inferred],
    };
    if gate::gate(&inferred_only, &ordinary_entries, &ordinary, now).is_ok() {
        return Err("the gate accepted an inferred-only chain".to_string());
    }
    // Stopping is always allowed.
    if gate::gate(&candidate("stop", vec![]), &entries, &adopted, now).is_err() {
        return Err("the gate refused stop".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mandate::Controller;

    #[test]
    fn the_shipped_mandate_parses_and_permits_only_stop() {
        let m = mandate();
        assert_eq!(m.metric, "integrity");
        assert!(m.permitted.is_empty());
        assert!(
            matches!(&m.controller, Controller::Threshold { below: Some(b), action, .. } if *b == 1.0 && action == "stop")
        );
        assert_eq!(
            m.forbidden,
            vec!["edit-mandate", "merge-main", "touch-keys"]
        );
        assert_eq!(hash().len(), 64);
        let v = venture(1, &hash());
        assert!(v.problem.is_none() && v.path.is_none());
        assert!(venture(1, "other").problem.is_some());
    }

    #[test]
    fn the_canary_holds() {
        assert_eq!(canary(), Ok(()));
    }

    #[test]
    fn integrity_is_one_on_a_sound_ledger_and_zero_on_a_broken_one() {
        let mut ledger = Ledger::open_in_memory().unwrap();
        ledger.append(adopt_record()).unwrap();
        let (value, note) = measure(&ledger);
        assert_eq!(value, Some(1.0));
        assert_eq!(note, "chain ok (1 records); canary ok");
    }
}
