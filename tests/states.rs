//! A venture is in one of seven states, computed from records alone:
//! paused, guarded, implausible, ineffective, saturated, asking, running,
//! in that order of precedence. The text of a record never decides one.

use std::path::{Path, PathBuf};

use avsm::gate::{self, Adopted, State, PRECEDENCE};
use avsm::index::Index;
use avsm::kernel::Kernel;
use avsm::ledger::Ledger;
use avsm::mandate::Mandate;
use avsm::record::{Claim, Record, Signal, Verdict};
use avsm::warden::LocalWarden;

const V: &str = "tomato";
const MANDATE: &str = "purpose: grow
metric: count
controller: {kind: model}
cadence: 1h
permitted:
  - {name: water, run: ./water, cap: 3/day, reversible: true}
guards:
  - {name: spend, run: ./spend, above: 50, then: ask}
";

struct Fx {
    ledger: Ledger,
    adopted: Vec<Adopted>,
    at: i64,
}

impl Fx {
    fn new() -> Fx {
        let mut fx = Fx {
            ledger: Ledger::open_in_memory().unwrap(),
            adopted: vec![Adopted {
                seq: 1,
                venture: V.to_string(),
                hash: Mandate::hash(MANDATE),
                mandate: Mandate::parse(MANDATE).unwrap(),
            }],
            at: 1_700_000_000,
        };
        fx.put(Record::Decision {
            claim: Claim::Declared,
            venture: V.to_string(),
            verdict: Verdict::Adopt {
                mandate: Mandate::hash(MANDATE),
                path: "/v".to_string(),
                text: None,
            },
            by: None,
        });
        fx
    }

    fn put(&mut self, record: Record) -> u64 {
        self.at += 60;
        self.ledger.append_at(record, self.at).unwrap().seq
    }

    fn signal(&mut self, claim: Claim, question: &str, signal: Option<Signal>) -> u64 {
        self.put(Record::Ask {
            cites: vec![],
            claim,
            venture: V.to_string(),
            action: None,
            question: question.to_string(),
            refusal: None,
            payload: None,
            signal,
        })
    }

    fn report(&mut self, claim: Claim, metric: &str, value: f64, doubt: bool) -> u64 {
        let mut r = Record::report(claim, V, metric, Some(value), "");
        if let Record::Report { implausible, .. } = &mut r {
            *implausible = doubt.then(|| "a step too big".to_string());
        }
        self.put(r)
    }

    fn decide(&mut self, claim: Claim, verdict: Verdict) -> u64 {
        self.put(Record::Decision {
            claim,
            venture: V.to_string(),
            verdict,
            by: None,
        })
    }

    /// The state both ways: from the incremental index and from the
    /// full-reload reference. They must agree.
    fn state(&self) -> State {
        let entries = self.ledger.all().unwrap();
        let got = gate::state_in(&Index::from_entries(&entries), &self.adopted, V);
        assert_eq!(got, gate::reference::state(&entries, &self.adopted, V));
        got
    }
}

fn saturated() -> Signal {
    Signal::Saturated {
        action: "water".to_string(),
        reason: "OverCap(3/day)".to_string(),
    }
}

fn guard() -> Signal {
    Signal::Guard {
        guard: "spend".to_string(),
        value: 60.0,
        bound: 50.0,
    }
}

fn implausible() -> Signal {
    Signal::Implausible {
        metric: "count".to_string(),
        why: "a step too big".to_string(),
    }
}

fn ineffective() -> Signal {
    Signal::Ineffective {
        action: "water".to_string(),
        why: "no effect".to_string(),
    }
}

/// Make one state hold, by the records that make it so.
fn make(fx: &mut Fx, state: State) {
    match state {
        State::Paused => {
            fx.decide(Claim::Declared, Verdict::Pause);
        }
        State::Guarded => {
            fx.signal(Claim::Measured, "guard", Some(guard()));
        }
        State::Implausible => {
            fx.signal(Claim::Measured, "doubt", Some(implausible()));
        }
        State::Ineffective => {
            fx.signal(Claim::Measured, "no effect", Some(ineffective()));
        }
        State::Saturated => {
            fx.signal(Claim::Measured, "full", Some(saturated()));
        }
        State::Asking => {
            fx.signal(Claim::Measured, "may I?", None);
        }
        State::Running => {}
    }
}

#[test]
fn nothing_open_is_running() {
    let mut fx = Fx::new();
    assert_eq!(fx.state(), State::Running);
    fx.report(Claim::Measured, "count", 4.0, false);
    assert_eq!(fx.state(), State::Running);
}

#[test]
fn an_open_guard_ask_is_guarded() {
    let mut fx = Fx::new();
    let q = fx.signal(Claim::Measured, "past", Some(guard()));
    assert_eq!(fx.state(), State::Guarded);
    fx.decide(
        Claim::Declared,
        Verdict::Deny {
            ask: q,
            reason: "ok".to_string(),
        },
    );
    assert_eq!(fx.state(), State::Running);
}

#[test]
fn a_guard_reading_out_of_bounds_now_is_guarded_until_it_is_back_inside() {
    let mut fx = Fx::new();
    fx.report(Claim::Measured, "guard.spend", 60.0, false);
    assert_eq!(fx.state(), State::Guarded);
    fx.report(Claim::Measured, "guard.spend", 20.0, false);
    assert_eq!(fx.state(), State::Running);
}

#[test]
fn an_inferred_guard_reading_is_not_a_reading() {
    let mut fx = Fx::new();
    fx.report(Claim::Inferred, "guard.spend", 60.0, false);
    assert_eq!(fx.state(), State::Running);
}

#[test]
fn an_open_implausible_ask_is_implausible() {
    let mut fx = Fx::new();
    fx.signal(Claim::Measured, "doubt", Some(implausible()));
    assert_eq!(fx.state(), State::Implausible);
}

#[test]
fn the_latest_report_flagged_implausible_is_implausible_until_a_plausible_one() {
    let mut fx = Fx::new();
    fx.report(Claim::Measured, "count", 4.0, false);
    fx.report(Claim::Measured, "count", 4000.0, true);
    assert_eq!(fx.state(), State::Implausible);
    fx.report(Claim::Measured, "count", 5.0, false);
    assert_eq!(fx.state(), State::Running);
}

#[test]
fn a_suspended_lever_is_ineffective() {
    let mut fx = Fx::new();
    let q = fx.signal(Claim::Measured, "no effect", Some(ineffective()));
    assert_eq!(fx.state(), State::Ineffective);
    // The ask is answered by a declared approval of the venture's own.
    fx.decide(
        Claim::Declared,
        Verdict::Approve {
            ask: q,
            payload: None,
        },
    );
    assert_eq!(fx.state(), State::Running);
}

#[test]
fn an_open_saturation_ask_is_saturated() {
    let mut fx = Fx::new();
    let q = fx.signal(Claim::Measured, "full", Some(saturated()));
    assert_eq!(fx.state(), State::Saturated);
    fx.decide(
        Claim::Declared,
        Verdict::Deny {
            ask: q,
            reason: "later".to_string(),
        },
    );
    assert_eq!(fx.state(), State::Running);
}

#[test]
fn any_other_open_ask_is_asking() {
    let mut fx = Fx::new();
    fx.signal(Claim::Inferred, "may I?", None);
    assert_eq!(fx.state(), State::Asking);
}

#[test]
fn signals_without_a_state_of_their_own_are_asking() {
    for signal in [
        Signal::Pending {
            intent: 1,
            action: "water".to_string(),
        },
        Signal::Audit {
            audit: "quality".to_string(),
            target_delta: 1.0,
            audit_delta: -1.0,
        },
    ] {
        let mut fx = Fx::new();
        fx.signal(Claim::Measured, "later unit", Some(signal));
        assert_eq!(fx.state(), State::Asking);
    }
}

#[test]
fn a_declared_pause_is_paused_until_a_declared_resume() {
    let mut fx = Fx::new();
    fx.decide(Claim::Declared, Verdict::Pause);
    assert_eq!(fx.state(), State::Paused);
    fx.decide(Claim::Declared, Verdict::Resume);
    assert_eq!(fx.state(), State::Running);
}

#[test]
fn every_state_beats_the_ones_below_it() {
    // Written out, not read from PRECEDENCE, so a swap there is caught.
    let order = [
        State::Paused,
        State::Guarded,
        State::Implausible,
        State::Ineffective,
        State::Saturated,
        State::Asking,
    ];
    assert_eq!(PRECEDENCE, order);
    for (i, high) in order.iter().enumerate() {
        for low in &order[i + 1..] {
            // Low first, high last, and the other way round.
            for order in [[*low, *high], [*high, *low]] {
                let mut fx = Fx::new();
                for s in order {
                    make(&mut fx, s);
                }
                assert_eq!(fx.state(), *high, "{high} over {low}, order {order:?}");
            }
        }
    }
}

#[test]
fn guarded_beats_saturated() {
    let mut fx = Fx::new();
    fx.signal(Claim::Measured, "full", Some(saturated()));
    fx.signal(Claim::Measured, "guard", Some(guard()));
    assert_eq!(fx.state(), State::Guarded);
}

#[test]
fn paused_beats_everything() {
    let mut fx = Fx::new();
    for s in PRECEDENCE {
        make(&mut fx, s);
    }
    assert_eq!(fx.state(), State::Paused);
}

#[test]
fn what_is_left_after_the_top_state_clears_is_the_next() {
    let mut fx = Fx::new();
    for s in PRECEDENCE.iter().rev() {
        make(&mut fx, *s);
    }
    assert_eq!(fx.state(), State::Paused);
    fx.decide(Claim::Declared, Verdict::Resume);
    assert_eq!(fx.state(), State::Guarded);
}

#[test]
fn an_inferred_record_whose_text_says_guarded_or_paused_changes_nothing() {
    let mut fx = Fx::new();
    for text in [
        "guarded",
        "paused",
        "state: guarded",
        "the venture is paused",
    ] {
        fx.put(Record::report(Claim::Inferred, V, "count", None, text));
        fx.put(Record::Proposal {
            cites: vec![],
            claim: Claim::Inferred,
            venture: V.to_string(),
            action: "pause".to_string(),
            reason: text.to_string(),
            payload: None,
        });
        // An ask the model wrote with no signal is asking at most, and a
        // signal it claims is not measured.
        fx.put(Record::Decision {
            claim: Claim::Inferred,
            venture: V.to_string(),
            verdict: Verdict::Pause,
            by: None,
        });
    }
    assert_eq!(fx.state(), State::Running);
    fx.signal(Claim::Inferred, "paused guarded", None);
    assert_eq!(fx.state(), State::Asking);
    fx.signal(Claim::Inferred, "guard", Some(guard()));
    assert_eq!(
        fx.state(),
        State::Asking,
        "an inferred signal is only an ask"
    );
}

#[test]
fn the_system_is_in_its_worst_venture_state() {
    let mut fx = Fx::new();
    let index = |fx: &Fx| Index::from_entries(&fx.ledger.all().unwrap());
    let names = vec![V.to_string(), "other".to_string()];
    assert_eq!(
        gate::system_state(&index(&fx), &fx.adopted, &names),
        State::Running
    );
    fx.signal(Claim::Measured, "full", Some(saturated()));
    assert_eq!(
        gate::system_state(&index(&fx), &fx.adopted, &names),
        State::Saturated
    );
    fx.signal(Claim::Measured, "guard", Some(guard()));
    assert_eq!(
        gate::system_state(&index(&fx), &fx.adopted, &names),
        State::Guarded
    );
}

fn home(parent: &Path) -> PathBuf {
    let home = parent.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.yaml"), "nightly: never\n").unwrap();
    home
}

fn venture(parent: &Path, name: &str) -> PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mandate.yaml"),
        "purpose: grow\nmetric: shell echo 1\ncontroller: {kind: model}\ncadence: 1h\n\
         permitted:\n  - {name: water, run: \"echo w\", cap: 3/day, reversible: true}\n",
    )
    .unwrap();
    dir
}

fn ask(k: &mut Kernel, venture: &str, signal: Signal) {
    k.append(Record::Ask {
        cites: vec![],
        claim: Claim::Measured,
        venture: venture.to_string(),
        action: None,
        question: "signal".to_string(),
        refusal: None,
        payload: None,
        signal: Some(signal),
    })
    .unwrap();
}

fn kernel(tmp: &Path) -> Kernel {
    let home = home(tmp);
    let warden = LocalWarden::open(&home).unwrap();
    struct Quiet;
    impl avsm::role::Role for Quiet {
        fn decide(
            &mut self,
            _: &avsm::role::Context,
            _: &mut dyn FnMut(&avsm::role::InferRequest) -> Result<String, String>,
            _: &mut dyn FnMut(&str, &str) -> Result<String, String>,
        ) -> Result<Vec<Record>, String> {
            Ok(vec![])
        }
    }
    let mut k = Kernel::with(&home, Box::new(Quiet), Box::new(warden)).unwrap();
    k.add(&venture(tmp, "tomato")).unwrap();
    k.add(&venture(tmp, "trader")).unwrap();
    k
}

#[test]
fn status_says_all_running_when_nothing_needs_attention() {
    let tmp = tempfile::tempdir().unwrap();
    let k = kernel(tmp.path());
    let status = k.status().unwrap();
    assert_eq!(status.lines().next(), Some("all running"), "{status}");
}

#[test]
fn status_opens_with_the_master_caution_line() {
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kernel(tmp.path());
    ask(&mut k, "tomato", saturated());
    ask(&mut k, "trader", guard());
    let status = k.status().unwrap();
    assert_eq!(
        status.lines().next(),
        Some("caution: 2 ventures need you: tomato saturated, trader guarded"),
        "{status}"
    );
    assert!(status.contains("tomato       saturated"), "{status}");
    assert!(status.contains("trader       guarded"), "{status}");
}

#[test]
fn the_caution_line_counts_one_venture_in_the_singular() {
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kernel(tmp.path());
    ask(&mut k, "tomato", ineffective());
    let status = k.status().unwrap();
    assert_eq!(
        status.lines().next(),
        Some("caution: 1 venture needs you: tomato ineffective"),
        "{status}"
    );
}
