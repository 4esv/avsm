//! The gate the kernel runs reads an index kept up to date record by
//! record, and loads only the facts a candidate can rest on. The gate as
//! first written loads every record on every call. On any ledger they
//! must agree: the same proof or refusal, word for word, the same state
//! for every venture, the same open asks.

use avsmd::config::Quotas;
use avsmd::gate::{self, reference, Adopted, Candidate, Proof, Step};
use avsmd::index::Index;
use avsmd::ledger::Ledger;
use avsmd::mandate::{Cap, Mandate};
use avsmd::record::{Claim, Payload, Record, Seq, Signal, Verdict};
use proptest::prelude::*;

const START: i64 = 1_700_000_000;
const HOUR: i64 = 3600;

/// The ventures: a parent and its child, a venture that names the parent
/// though the parent does not list it, and one on its own.
const VENTURES: &[&str] = &["parent", "child", "orphan", "solo"];
/// Names the records and candidates draw from, a venture nobody adopted
/// among them.
const NAMES: &[&str] = &["parent", "child", "orphan", "solo", "ghost"];
const ACTIONS: &[&str] = &[
    "reset",
    "launch",
    "post",
    "mail",
    "pause",
    "resume",
    "budget",
    "threshold",
    "delegate",
    "withdraw",
];

fn mandate_text(venture: &str) -> &'static str {
    match venture {
        "parent" => {
            "purpose: allocate
metric: total
controller: {kind: model}
cadence: 1h
children: [{name: child, priority: 1, floor: 4}]
allocation: {deadband: 2, hold: 1h}
permitted:
  - {name: pause, cap: 9/day, reversible: true}
  - {name: resume, cap: 9/day, reversible: true}
  - {name: budget, cap: 9/day, reversible: true, delegated: true}
  - {name: threshold, cap: 9/day, reversible: true, delegated: true}
  - {name: launch, run: ./launch, cap: 2/day, reversible: false, cost: 3}
"
        }
        "child" => {
            "purpose: keep the count low
metric: count
controller: {kind: threshold, above: 10, action: reset}
cadence: 1h
parent: parent
permitted:
  - {name: reset, run: ./reset, cap: 3/day, reversible: true, cost: 2}
  - {name: launch, run: ./launch, cap: 2/day, reversible: false}
  - {name: post, run: ./post, cap: 4/day, reversible: true, takes: text, cost: 1}
  - {name: mail, adapter: mail.send me@example.com, cap: 5/day, reversible: false, delegated: true}
forbidden: [withdraw]
guards:
  - {name: spend, run: ./spend, above: 50, then: ask}
"
        }
        "orphan" => {
            "purpose: keep the count up
metric: count
controller: {kind: threshold, below: 5, action: reset}
cadence: 1h
parent: parent
permitted:
  - {name: reset, run: ./reset, cap: 2/hour, reversible: true}
"
        }
        _ => {
            "purpose: do things
metric: count
controller: {kind: model}
cadence: 1h
permitted:
  - {name: reset, run: ./reset, cap: 3/day, reversible: true, cost: 1}
  - {name: launch, run: ./launch, cap: 2/day, reversible: false, delegated: true}
  - {name: post, run: ./post, cap: 2/day, reversible: false, takes: text}
  - {name: mail, adapter: mail.send me@example.com, cap: 5/day, reversible: false, delegated: true, cost: 2}
forbidden: [withdraw]
guards:
  - {name: spend, run: ./spend, below: 10}
"
        }
    }
}

/// Levers the mandates permit, by venture.
const LEVERS: &[(&str, &str)] = &[
    ("child", "reset"),
    ("child", "launch"),
    ("child", "post"),
    ("child", "mail"),
    ("solo", "reset"),
    ("solo", "launch"),
    ("solo", "post"),
    ("solo", "mail"),
    ("orphan", "reset"),
    ("parent", "launch"),
    ("parent", "budget"),
    ("parent", "threshold"),
    ("parent", "pause"),
    ("parent", "resume"),
];

/// A venture and an action: mostly a lever its mandate permits.
fn lever() -> impl Strategy<Value = (String, String)> {
    prop_oneof![
        3 => prop::sample::select(LEVERS).prop_map(|(v, a)| (v.to_string(), a.to_string())),
        1 => (name(), prop_oneof![8 => action(), 1 => Just("stop".to_string())]),
    ]
}

fn claim() -> impl Strategy<Value = Claim> {
    prop_oneof![
        Just(Claim::Measured),
        Just(Claim::Declared),
        Just(Claim::Inferred),
    ]
}

fn name() -> impl Strategy<Value = String> {
    prop::sample::select(NAMES).prop_map(str::to_string)
}

fn action() -> impl Strategy<Value = String> {
    prop::sample::select(ACTIONS).prop_map(str::to_string)
}

/// Text a parent lever reads, and text it does not.
fn text() -> impl Strategy<Value = Payload> {
    prop::sample::select(&[
        "child",
        "orphan",
        "solo",
        "child 4",
        "child 40",
        "child 3",
        "child 5",
        "child 6",
        "orphan 7",
        "self",
        "self 4",
        "child above 3",
        "child below 20",
        "orphan below 2",
        "hello",
    ])
    .prop_map(|t| Payload::Text {
        text: t.to_string(),
    })
}

fn payload() -> impl Strategy<Value = Option<Payload>> {
    prop_oneof![
        3 => Just(None),
        2 => text().prop_map(Some),
        1 => Just(Some(Payload::Json { json: serde_json::json!({"n": 1}) })),
    ]
}

/// A record number: usually one already written, sometimes one that is
/// not.
fn seq() -> impl Strategy<Value = u64> {
    0u64..1000
}

/// One record, with its record numbers as fractions of the ledger so far.
#[derive(Clone, Debug)]
enum Op {
    /// venture, metric, claim, value, cites, implausible
    Report(String, String, Claim, Option<f64>, Option<u64>, bool),
    Proposal(String, String, Claim, Option<Payload>),
    Ask(String, Option<String>, Claim, Option<Payload>),
    /// An algedonic ask: venture, claim, which signal.
    Signalled(String, Claim, u8),
    Approve(String, Claim, u64, Option<Payload>),
    Deny(String, Claim, u64),
    Pause(String, Claim),
    Resume(String, Claim),
    Action(String, String, bool, Option<Payload>, Option<u64>),
    Intent(String, String, Option<Payload>),
    /// An outcome of a record already written, of its venture or another,
    /// by any claim.
    Outcome(u64, Claim, bool, Option<String>),
    /// A lever run as 2.0 records it: an intent, then its outcome.
    Run(String, String, Claim, bool, Option<Payload>),
    /// A parent lever pulled, by the parent or by someone else.
    Pull(String, String, bool, Payload),
    Readopt(String, bool),
    /// An ask that a lever is ineffective, by any claim, and perhaps an
    /// approval of it by any claim and venture.
    Ineffective(String, String, Claim, Option<(Claim, String)>),
    /// An escalation, refused or not, by any claim.
    Escalation(String, Claim, bool),
    /// The answer to a record already written, by any claim, citing
    /// perhaps another.
    Answer(String, Claim, u64, bool, Option<u64>),
    /// A world model's prediction of what a lever would do, by any claim:
    /// never a premise.
    Prediction(String, String, Claim),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (
            prop_oneof![2 => Just("child".to_string()), 1 => name()],
            prop::sample::select(&["count", "count", "count", "count", "other", "guard.spend"][..]),
            claim(),
            prop::option::of(prop::sample::select(&[1.0, 7.0, 12.0, 12.0, 60.0][..])),
            prop::option::of(seq()),
            prop::bool::weighted(0.2),
        )
            .prop_map(|(v, m, c, x, cites, doubt)| Op::Report(v, m.to_string(), c, x, cites, doubt)),
        3 => (lever(), claim(), payload()).prop_map(|((v, a), c, p)| Op::Proposal(v, a, c, p)),
        3 => (lever(), any::<bool>(), claim(), payload()).prop_map(|((v, a), some, c, p)| Op::Ask(v, some.then_some(a), c, p)),
        3 => (name(), claim(), 0u8..6).prop_map(|(v, c, k)| Op::Signalled(v, c, k)),
        3 => (name(), claim(), seq(), payload()).prop_map(|(v, c, q, p)| Op::Approve(v, c, q, p)),
        2 => (name(), claim(), seq()).prop_map(|(v, c, q)| Op::Deny(v, c, q)),
        1 => (name(), claim()).prop_map(|(v, c)| Op::Pause(v, c)),
        3 => (name(), claim()).prop_map(|(v, c)| Op::Resume(v, c)),
        4 => (lever(), any::<bool>(), payload(), prop::option::of(seq()))
            .prop_map(|((v, a), ok, p, d)| Op::Action(v, a, ok, p, d)),
        2 => (lever(), payload()).prop_map(|((v, a), p)| Op::Intent(v, a, p)),
        2 => (seq(), claim(), prop::bool::weighted(0.8), prop::option::weighted(0.2, name()))
            .prop_map(|(x, c, ok, other)| Op::Outcome(x, c, ok, other)),
        3 => (
            prop_oneof![
                3 => (
                    prop_oneof![3 => Just("parent".to_string()), 1 => name()],
                    prop::sample::select(&["pause", "resume", "budget", "threshold"][..]).prop_map(str::to_string),
                ),
                1 => lever(),
            ],
            prop_oneof![4 => Just(Claim::Measured), 1 => claim()],
            prop::bool::weighted(0.8),
            prop_oneof![3 => text().prop_map(Some), 1 => payload()],
        )
            .prop_map(|((v, a), c, ok, p)| Op::Run(v, a, c, ok, p)),
        3 => (
            prop_oneof![3 => Just("parent".to_string()), 1 => name()],
            prop::sample::select(&["pause", "resume", "budget", "threshold"][..]).prop_map(str::to_string),
            prop::bool::weighted(0.8),
            text(),
        )
            .prop_map(|(v, lever, ok, text)| Op::Pull(v, lever, ok, text)),
        3 => (lever(), claim(), prop::option::of((claim(), name())))
            .prop_map(|((v, a), c, ok)| Op::Ineffective(v, a, c, ok)),
        1 => (prop::sample::select(VENTURES).prop_map(str::to_string), any::<bool>())
            .prop_map(|(v, same)| Op::Readopt(v, same)),
        2 => (name(), claim(), any::<bool>()).prop_map(|(v, c, r)| Op::Escalation(v, c, r)),
        2 => (name(), claim(), seq(), any::<bool>(), prop::option::of(seq()))
            .prop_map(|(v, c, e, ok, cite)| Op::Answer(v, c, e, ok, cite)),
        2 => (lever(), claim()).prop_map(|((v, a), c)| Op::Prediction(v, a, c)),
    ]
}

/// A number already in a ledger of `n` records, or now and then one past
/// its end.
fn pick(n: u64, x: u64) -> Seq {
    if x % 13 == 0 {
        n + 1 + x % 3
    } else {
        1 + x % n.max(1)
    }
}

fn proof(venture: &str, action: &str, decision: Option<Seq>) -> Proof {
    Proof {
        venture: venture.to_string(),
        action: action.to_string(),
        steps: decision
            .map(|seq| Step::Decision {
                seq,
                what: "approved".to_string(),
            })
            .into_iter()
            .collect(),
    }
}

fn record(op: &Op, n: u64) -> Record {
    match op.clone() {
        Op::Report(v, metric, claim, value, cites, doubt) => Record::Report {
            claim,
            metric,
            venture: v,
            value,
            note: String::new(),
            source: None,
            confidence: None,
            cites: cites.map(|x| pick(n, x)).into_iter().collect(),
            implausible: doubt.then(|| "a step too big".to_string()),
            facts: None,
        },
        Op::Proposal(venture, action, claim, payload) => Record::Proposal {
            cites: vec![],
            claim,
            venture,
            action,
            reason: "because".to_string(),
            payload,
        },
        Op::Ask(venture, action, claim, payload) => Record::Ask {
            packet: None,
            cites: vec![],
            claim,
            venture,
            action,
            question: "may I?".to_string(),
            refusal: None,
            signal: None,
            payload,
        },
        Op::Signalled(venture, claim, k) => Record::Ask {
            packet: None,
            cites: vec![],
            claim,
            venture,
            action: None,
            question: "signal".to_string(),
            refusal: None,
            payload: None,
            signal: Some(match k {
                0 => Signal::Saturated {
                    action: "reset".to_string(),
                    reason: "cap".to_string(),
                },
                1 => Signal::Pending {
                    intent: pick(n, 3),
                    action: "reset".to_string(),
                },
                2 => Signal::Guard {
                    guard: "spend".to_string(),
                    value: 60.0,
                    bound: 50.0,
                },
                3 => Signal::Audit {
                    audit: "quality".to_string(),
                    target_delta: 1.0,
                    audit_delta: -1.0,
                },
                4 => Signal::Implausible {
                    metric: "count".to_string(),
                    why: "a step too big".to_string(),
                },
                _ => Signal::Ineffective {
                    action: "reset".to_string(),
                    why: "no effect".to_string(),
                },
            }),
        },
        Op::Approve(venture, claim, q, payload) => Record::Decision {
            claim,
            venture,
            verdict: Verdict::Approve {
                ask: pick(n, q),
                payload,
            },
            by: None,
        },
        Op::Deny(venture, claim, q) => Record::Decision {
            claim,
            venture,
            verdict: Verdict::Deny {
                ask: pick(n, q),
                reason: "no".to_string(),
            },
            by: None,
        },
        Op::Pause(venture, claim) => Record::Decision {
            claim,
            venture,
            verdict: Verdict::Pause,
            by: None,
        },
        Op::Resume(venture, claim) => Record::Decision {
            claim,
            venture,
            verdict: Verdict::Resume,
            by: None,
        },
        Op::Action(venture, action, ok, payload, d) => Record::Action {
            claim: Claim::Measured,
            proof: proof(&venture, &action, d.map(|x| pick(n, x))),
            venture,
            action,
            ok,
            detail: String::new(),
            payload,
        },
        Op::Intent(venture, action, payload) => Record::Intent {
            claim: Claim::Measured,
            proof: proof(&venture, &action, None),
            key: Some(avsmd::record::key(&venture, &action, &[], payload.as_ref())),
            venture,
            action,
            payload,
        },
        Op::Escalation(venture, claim, refused) => Record::Escalation {
            claim,
            venture,
            from: "primary".to_string(),
            to: "second".to_string(),
            why: "unusable".to_string(),
            refused: refused.then(|| "it leaves the machine".to_string()),
        },
        Op::Answer(venture, claim, e, ok, cite) => Record::Answer {
            claim,
            venture,
            escalation: pick(n, e),
            ok,
            cites: cite.map(|x| pick(n, x)).into_iter().collect(),
            detail: String::new(),
        },
        Op::Outcome(..) | Op::Run(..) | Op::Ineffective(..) => unreachable!("written by build"),
        Op::Prediction(venture, action, claim) => Record::Prediction {
            claim,
            venture,
            model: "baseline".to_string(),
            metric: "count".to_string(),
            action: Some(action),
            made_at: START,
            resolves_at: START + HOUR,
            quantiles: avsmd::record::Quantiles::from_values([
                1.0, 2.0, 3.0, 12.0, 13.0, 14.0, 15.0,
            ]),
            events: vec![],
        },
        Op::Pull(venture, action, ok, text) => Record::Action {
            claim: Claim::Measured,
            proof: proof(&venture, &action, None),
            venture,
            action,
            ok,
            detail: String::new(),
            payload: Some(text),
        },
        Op::Readopt(venture, same) => Record::Decision {
            claim: Claim::Declared,
            verdict: Verdict::Adopt {
                mandate: if same {
                    Mandate::hash(mandate_text(&venture))
                } else {
                    "changed".to_string()
                },
                path: format!("/ventures/{venture}"),
                text: None,
            },
            venture,
            by: None,
        },
    }
}

/// What to ask the gate: anything at all, or what the kernel would ask
/// on the occasion of a record already written.
#[derive(Clone, Debug)]
enum Cand {
    Any(String, String, Vec<u64>),
    On(u64, Vec<u64>),
}

fn cand() -> impl Strategy<Value = Cand> {
    prop_oneof![
        (lever(), prop::collection::vec(seq(), 0..3)).prop_map(|((v, a), c)| Cand::Any(v, a, c)),
        (seq(), prop::collection::vec(seq(), 0..2)).prop_map(|(k, c)| Cand::On(k, c)),
    ]
}

/// The candidate as the kernel would form it: a proposal's lever, an
/// approved ask's lever, a resume on a resume, the threshold's action on
/// a report.
fn resolve(cand: &Cand, entries: &[avsmd::record::Entry]) -> Candidate {
    let n = entries.len() as u64;
    let (venture, action, mut cites) = match cand {
        Cand::Any(v, a, c) => (
            v.clone(),
            a.clone(),
            c.iter().map(|x| pick(n, *x)).collect(),
        ),
        Cand::On(k, c) => {
            let at = pick(n, *k);
            let extra: Vec<Seq> = c.iter().map(|x| pick(n, *x)).collect();
            let (v, a) = match entries.get(at as usize - 1).map(|e| &e.record) {
                Some(Record::Proposal {
                    venture, action, ..
                }) => (venture.clone(), action.clone()),
                Some(Record::Decision {
                    venture,
                    verdict: Verdict::Approve { ask, .. },
                    ..
                }) => {
                    match entries
                        .get((*ask as usize).wrapping_sub(1))
                        .map(|e| &e.record)
                    {
                        Some(
                            r @ Record::Ask {
                                action: Some(a), ..
                            },
                        ) => (r.venture().to_string(), a.clone()),
                        _ => (venture.clone(), "launch".to_string()),
                    }
                }
                Some(Record::Decision {
                    venture,
                    verdict: Verdict::Resume,
                    ..
                }) => (venture.clone(), "resume".to_string()),
                Some(r) => (r.venture().to_string(), "reset".to_string()),
                None => ("child".to_string(), "reset".to_string()),
            };
            let mut cites = vec![at];
            cites.extend(extra);
            (v, a, cites)
        }
    };
    cites.dedup();
    Candidate {
        venture,
        action,
        cites,
    }
}

#[derive(Clone, Debug)]
struct Case {
    ops: Vec<(Op, i64)>,
    /// After how many records the kernel's index looks again.
    looks: Vec<usize>,
    candidates: Vec<Cand>,
    later: i64,
    quotas: (Option<u32>, Option<u32>, Option<u32>),
    /// Hand solo's mandate to the gate with a record number that did not
    /// adopt it.
    solo_unadopted: bool,
}

fn case() -> impl Strategy<Value = Case> {
    (
        prop::collection::vec(
            (
                op(),
                prop_oneof![8 => 0..HOUR, 2 => -HOUR..0, 1 => 0..48 * HOUR],
            ),
            0..80,
        ),
        prop::collection::vec(0usize..80, 0..6),
        prop::collection::vec(cand(), 1..10),
        prop_oneof![3 => 0..HOUR, 1 => 0..3 * 24 * HOUR],
        (
            prop::option::of(0u32..12),
            prop::option::of(0u32..4),
            prop::option::of(0u32..6),
        ),
        any::<bool>(),
    )
        .prop_map(
            |(ops, looks, candidates, later, quotas, solo_unadopted)| Case {
                ops,
                looks,
                candidates,
                later,
                quotas,
                solo_unadopted,
            },
        )
}

/// The ledger the case writes, the index the kernel would hold after
/// looking at it now and then, the mandates and the time.
fn build(case: &Case) -> (Ledger, Index, Vec<Adopted>, i64) {
    let mut ledger = Ledger::open_in_memory().unwrap();
    let mut index = Index::default();
    let mut at = START;
    let mut adopted = Vec::new();
    for v in VENTURES {
        let text = mandate_text(v);
        let hash = Mandate::hash(text);
        let e = ledger
            .append_at(
                Record::Decision {
                    claim: Claim::Declared,
                    venture: v.to_string(),
                    verdict: Verdict::Adopt {
                        mandate: hash.clone(),
                        path: format!("/ventures/{v}"),
                        text: None,
                    },
                    by: None,
                },
                at,
            )
            .unwrap();
        adopted.push(Adopted {
            seq: if *v == "solo" && case.solo_unadopted {
                1
            } else {
                e.seq
            },
            venture: v.to_string(),
            hash,
            mandate: Mandate::parse(text).unwrap(),
        });
    }
    for (i, (op, step)) in case.ops.iter().enumerate() {
        if case.looks.contains(&i) {
            index.refresh(&ledger).unwrap();
        }
        at += step;
        let n = ledger.len().unwrap();
        match op {
            Op::Outcome(x, claim, ok, other) => {
                let intent = pick(n, *x);
                let venture = other
                    .clone()
                    .or_else(|| {
                        ledger
                            .get(intent)
                            .unwrap()
                            .map(|e| e.record.venture().to_string())
                    })
                    .unwrap_or_else(|| "child".to_string());
                let outcome = Record::Outcome {
                    claim: *claim,
                    venture,
                    intent,
                    ok: *ok,
                    detail: String::new(),
                };
                ledger.append_at(outcome, at).unwrap();
            }
            Op::Run(venture, action, claim, ok, payload) => {
                let started = ledger
                    .append_at(
                        record(
                            &Op::Intent(venture.clone(), action.clone(), payload.clone()),
                            n,
                        ),
                        at,
                    )
                    .unwrap();
                let outcome = Record::Outcome {
                    claim: *claim,
                    venture: venture.clone(),
                    intent: started.seq,
                    ok: *ok,
                    detail: String::new(),
                };
                ledger.append_at(outcome, at).unwrap();
            }
            Op::Ineffective(venture, action, claim, approval) => {
                let ask = ledger
                    .append_at(
                        Record::Ask {
                            packet: None,
                            cites: vec![],
                            claim: *claim,
                            venture: venture.clone(),
                            action: None,
                            question: "ineffective".to_string(),
                            refusal: None,
                            payload: None,
                            signal: Some(Signal::Ineffective {
                                action: action.clone(),
                                why: "no effect".to_string(),
                            }),
                        },
                        at,
                    )
                    .unwrap();
                if let Some((claim, by)) = approval {
                    let verdict = Verdict::Approve {
                        ask: ask.seq,
                        payload: None,
                    };
                    ledger
                        .append_at(
                            Record::Decision {
                                claim: *claim,
                                venture: by.clone(),
                                verdict,
                                by: None,
                            },
                            at,
                        )
                        .unwrap();
                }
            }
            _ => {
                ledger.append_at(record(op, n), at).unwrap();
            }
        }
    }
    index.refresh(&ledger).unwrap();
    (ledger, index, adopted, at + case.later)
}

fn quotas(case: &Case) -> Quotas {
    let cap = |count| Cap {
        count,
        per: Cap::DAY,
    };
    Quotas {
        spend: case.quotas.0.map(cap),
        sends: case.quotas.1.map(cap),
        asks: case.quotas.2.map(cap),
    }
}

fn rendered(r: &Result<Proof, gate::Refusal>) -> String {
    match r {
        Ok(p) => p.to_string(),
        Err(r) => r.to_string(),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    #[test]
    fn the_incremental_gate_agrees_with_the_reference(case in case()) {
        let (ledger, index, mandates, now) = build(&case);
        let entries = ledger.all().unwrap();
        prop_assert_eq!(index.entries().cloned().collect::<Vec<_>>(), entries.clone());
        let quotas = quotas(&case);
        for cand in &case.candidates {
            let candidate = resolve(cand, &entries);
            let (venture, action) = (&candidate.venture, &candidate.action);
            let want = reference::gate_with(&candidate, &entries, &mandates, &quotas, now);
            let got = gate::gate_in(&index, &candidate, &mandates, &quotas, now);
            prop_assert_eq!(rendered(&got), rendered(&want));
            prop_assert_eq!(&got, &want);
            let got = gate::gate_with(&candidate, &entries, &mandates, &quotas, now);
            prop_assert_eq!(&got, &want);
            prop_assert_eq!(
                gate::quota_check_in(&index, &mandates, &quotas, venture, action, now),
                reference::quota_check(&entries, &mandates, &quotas, venture, action, now)
            );
        }
        for v in NAMES {
            prop_assert_eq!(
                gate::state_in(&index, &mandates, v),
                reference::state(&entries, &mandates, v),
                "state of {}", v
            );
            prop_assert_eq!(
                gate::effective_threshold_in(&index, &mandates, v),
                reference::effective_threshold(&entries, &mandates, v)
            );
            let refs: Vec<&Adopted> = mandates.iter().collect();
            prop_assert_eq!(
                gate::budget_of_in(&index, &refs, v),
                reference::budget_of(&entries, &refs, v)
            );
            prop_assert_eq!(
                gate::spent_today_in(&index, &refs, v, now),
                reference::spent_today(&entries, &refs, v, now)
            );
        }
        let names: Vec<String> = NAMES.iter().map(|v| v.to_string()).collect();
        let worst = NAMES
            .iter()
            .map(|v| reference::state(&entries, &mandates, v))
            .min_by_key(|s| gate::PRECEDENCE.iter().position(|p| p == s).unwrap_or(99))
            .unwrap();
        prop_assert_eq!(gate::system_state(&index, &mandates, &names), worst);
        for (v, a) in LEVERS {
            prop_assert_eq!(
                index.suspended(v, a),
                reference::suspended(&entries, v, a),
                "suspended {} {}", v, a
            );
            prop_assert_eq!(
                index.restored(v, a),
                reference::restored(&entries, v, a),
                "restored {} {}", v, a
            );
        }
        prop_assert_eq!(index.open_asks(), reference::open_asks(&entries));
        prop_assert_eq!(gate::open_asks(&entries), reference::open_asks(&entries));
        let cap = Cap { count: 3, per: Cap::DAY };
        prop_assert_eq!(gate::asks_used_in(&index, cap, now), reference::asks_used(&entries, cap, now));
        prop_assert_eq!(gate::current_facts(&entries), reference::current_facts(&entries));
    }
}
