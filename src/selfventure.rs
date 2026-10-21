//! The built-in `self` venture: avsm watching avsm. Its mandate ships in
//! the binary and is adopted on first start. Every cycle it reports one
//! number, integrity: 1 when the ledger chain verifies and the gate's
//! canary still refuses what it must, else 0, which trips its only
//! permitted action, stop, and halts every venture until a human resumes.

use crate::gate::{self, Adopted, Candidate, HARDWIRED, SELF};
use crate::ledger::{Ledger, LedgerError};
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
        note: None,
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
            text: Some(MANDATE.to_string()),
        },
        by: None,
    }
}

/// How often the whole chain is read again, at the most: a row changed
/// behind avsm's back is found within this, however it was changed.
pub const WHOLE_EVERY: i64 = 86_400;

/// What the chain check has verified: up to which entry, with what hash,
/// how many entries that is, and when and under which schema version the
/// whole chain was last read. Between whole reads only the entries
/// appended since are checked; the whole chain is read again after
/// `WHOLE_EVERY`, or at once when the schema changed (an UPDATE or a
/// DELETE has to drop a trigger first, and that changes it).
#[derive(Clone, Debug, Default)]
pub struct Checked {
    seq: Seq,
    hash: String,
    count: u64,
    whole: Option<(i64, i64)>,
}

impl Checked {
    /// Verify the chain as of `now`: only what is new since the last
    /// check, or the whole of it when that is due. Ok carries how many
    /// entries the chain holds. A failure forgets what was verified, so
    /// the next check reads the whole chain again.
    pub fn verify(&mut self, ledger: &Ledger, now: i64) -> Result<u64, String> {
        let r = self.check(ledger, now);
        if r.is_err() {
            *self = Checked::default();
        }
        r.map_err(|e| e.to_string().trim_start_matches("ledger: ").to_string())
    }

    fn check(&mut self, ledger: &Ledger, now: i64) -> Result<u64, LedgerError> {
        let schema = ledger.schema_version()?;
        let due = self
            .whole
            .is_none_or(|(at, s)| s != schema || now - at >= WHOLE_EVERY || now < at);
        if due {
            let (n, seq, hash) = ledger.verify_after(0, GENESIS)?;
            *self = Checked {
                seq,
                hash,
                count: n,
                whole: Some((now, schema)),
            };
        } else {
            let (n, seq, hash) = ledger.verify_after(self.seq, &self.hash)?;
            self.seq = seq;
            self.hash = hash;
            self.count += n;
        }
        Ok(self.count)
    }
}

/// Measure integrity: the chain verifies and the canary holds. Returns
/// the value and a note saying what was checked. `checked` is what was
/// verified before; see `Checked`.
pub fn measure(ledger: &Ledger, checked: &mut Checked, now: i64) -> (Option<f64>, String) {
    let chain = checked.verify(ledger, now);
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
        metric: "integrity".into(),
        controller: Controller::Threshold {
            above: Some(0.0),
            below: None,
            action: "edit-mandate".to_string(),
            deadband: None,
            hold: None,
        },
        permitted: HARDWIRED
            .iter()
            .map(|(name, _)| Permitted {
                name: name.to_string(),
                run: Some("true".to_string()),
                adapter: None,
                cap: Cap {
                    count: 9,
                    per: Cap::DAY,
                },
                reversible: true,
                delegated: true,
                takes: None,
                cost: 0,
                check: None,
                expects: None,
                revoke_after: None,
                fields: None,
            })
            .collect(),
        forbidden: vec![],
        cadence: std::time::Duration::from_secs(60),
        context_budget: 1000,
        summarize_after: 200,
        restart: Restart::Never,
        network: None,
        children: vec![],
        parent: None,
        allocation: None,
        guards: vec![],
        audits: vec![],
        readings: vec![],
        range: None,
        max_step: None,
        confirm: true,
        escalate: None,
        world: None,
        calibration: None,
        early_warning: false,
        better: None,
    };
    let mandate_hash = Mandate::hash(&serde_yaml::to_string(&permissive).unwrap_or_default());
    let adopt = Record::Decision {
        claim: Claim::Declared,
        venture: SELF.to_string(),
        verdict: Verdict::Adopt {
            mandate: mandate_hash.clone(),
            path: PATH.to_string(),
            text: None,
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
        let (value, note) = measure(&ledger, &mut Checked::default(), 0);
        assert_eq!(value, Some(1.0));
        assert_eq!(note, "chain ok (1 records); canary ok");
    }

    const T: i64 = 1_800_000_000;

    fn report(v: f64) -> Record {
        Record::report(crate::record::Claim::Measured, "c", "n", Some(v), "")
    }

    fn file_ledger(dir: &std::path::Path, n: usize) -> Ledger {
        let mut ledger = Ledger::open(&dir.join("ledger.db")).unwrap();
        ledger.append(adopt_record()).unwrap();
        for i in 0..n {
            ledger.append(report(i as f64)).unwrap();
        }
        ledger
    }

    /// Edit a stored row through another connection and cover the
    /// tracks: put the trigger back and the schema version with it, so
    /// only reading the row again finds the edit.
    fn edit_quietly(dir: &std::path::Path, seq: Seq) {
        let conn = rusqlite::Connection::open(dir.join("ledger.db")).unwrap();
        let schema: i64 = conn
            .query_row("PRAGMA schema_version", [], |r| r.get(0))
            .unwrap();
        conn.execute_batch(&format!(
            "DROP TRIGGER records_no_update;
             UPDATE records SET at = at + 1 WHERE seq = {seq};
             CREATE TRIGGER records_no_update BEFORE UPDATE ON records
             BEGIN SELECT RAISE(ABORT, 'the ledger is append-only'); END;
             PRAGMA schema_version = {schema};"
        ))
        .unwrap();
    }

    #[test]
    fn the_incremental_check_counts_what_the_whole_check_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ledger = file_ledger(tmp.path(), 0);
        let mut checked = Checked::default();
        for i in 0..20 {
            for _ in 0..i % 3 {
                ledger.append(report(f64::from(i))).unwrap();
            }
            let now = T + i64::from(i) * 600;
            assert_eq!(
                checked.verify(&ledger, now),
                Ok(ledger.verify().unwrap()),
                "step {i}"
            );
        }
        assert_eq!(checked.whole.map(|(at, _)| at), Some(T), "read whole once");
    }

    #[test]
    fn a_row_edited_behind_avsms_back_is_found_within_a_day() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ledger = file_ledger(tmp.path(), 5);
        let mut checked = Checked::default();
        assert_eq!(measure(&ledger, &mut checked, T).0, Some(1.0));
        edit_quietly(tmp.path(), 2);
        ledger.append(report(9.0)).unwrap();
        const DAY: i64 = 86_400;
        // The edit left no trace in the schema: the next check reads only
        // the new entry, and does not see it yet.
        assert_eq!(measure(&ledger, &mut checked, T + 600).0, Some(1.0));
        // Every check after, every ten minutes: one within a day of the
        // whole read finds it.
        let mut found = None;
        let mut now = T + 600;
        while now <= T + DAY {
            now += 600;
            let (value, note) = measure(&ledger, &mut checked, now);
            if value == Some(0.0) {
                assert!(note.starts_with("chain broken: record #2"), "{note}");
                found = Some(now);
                break;
            }
        }
        let found = found.expect("an edited row is found within a day");
        assert!(found - T <= DAY, "found after {}s", found - T);
        // And it stays found: a failure forgets what was verified.
        assert_eq!(measure(&ledger, &mut checked, found + 60).0, Some(0.0));
    }

    #[test]
    fn a_dropped_trigger_makes_the_next_check_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = file_ledger(tmp.path(), 5);
        let mut checked = Checked::default();
        assert_eq!(measure(&ledger, &mut checked, T).0, Some(1.0));
        let conn = rusqlite::Connection::open(tmp.path().join("ledger.db")).unwrap();
        conn.execute_batch(
            "DROP TRIGGER records_no_update; UPDATE records SET at = at + 1 WHERE seq = 3;",
        )
        .unwrap();
        let (value, note) = measure(&ledger, &mut checked, T + 60);
        assert_eq!(value, Some(0.0));
        assert!(note.starts_with("chain broken: record #3"), "{note}");
    }
}
