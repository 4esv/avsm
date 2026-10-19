//! Non-finite numbers in records: JSON has none, so they are written as
//! "inf", "-inf" and "nan" and read back as themselves, and a ledger never
//! stores a record it cannot read (ledger seq 6528 of the 365-day hostile
//! sim held `variance_ratio: null`, and every later read failed).

use avsmd::gate::{Proof, Reason, Refusal, Step};
use avsmd::ledger::Ledger;
use avsmd::record::*;

const NANS: [f64; 3] = [f64::INFINITY, f64::NEG_INFINITY, f64::NAN];

fn q(x: f64) -> Quantiles {
    Quantiles::from_values([x; 7])
}

fn ask(signal: Signal) -> Record {
    Record::Ask {
        claim: Claim::Measured,
        venture: "shop".into(),
        action: None,
        question: "q".into(),
        refusal: None,
        payload: None,
        signal: Some(signal),
        cites: vec![],
        packet: Some(Packet {
            premises: vec![],
            options: vec![PacketOption {
                name: "approve".into(),
                forecast: Some(OptionForecast {
                    metric: "m".into(),
                    days: 1,
                    seeds: 1,
                    p5: f64::NAN,
                    p50: f64::INFINITY,
                    p95: f64::NEG_INFINITY,
                    breach: f64::NAN,
                    spend: f64::INFINITY,
                }),
                cost: None,
            }],
            reason: None,
        }),
    }
}

fn records(x: f64) -> Vec<Record> {
    let alt = Alternative {
        action: "a".into(),
        quantiles: q(x),
        worst: x,
        propensity: x,
    };
    let steps = vec![
        Step::Report {
            seq: 1,
            claim: Claim::Measured,
            metric: "m".into(),
            value: x,
        },
        Step::Threshold {
            metric: "m".into(),
            bound: x,
            above: true,
            from: None,
        },
    ];
    vec![
        Record::Action {
            claim: Claim::Measured,
            venture: "shop".into(),
            action: "a".into(),
            proof: Proof {
                venture: "shop".into(),
                action: "a".into(),
                steps: steps.clone(),
            },
            ok: true,
            detail: String::new(),
            payload: None,
        },
        Record::Ask {
            claim: Claim::Measured,
            venture: "shop".into(),
            action: Some("a".into()),
            question: "q".into(),
            refusal: Some(Refusal {
                venture: "shop".into(),
                action: "a".into(),
                steps,
                reasons: vec![Reason::Guarded {
                    guard: "g".into(),
                    value: x,
                    above: true,
                    bound: x,
                }],
            }),
            payload: None,
            signal: None,
            cites: vec![],
            packet: None,
        },
        ask(Signal::Warning {
            metric: "m".into(),
            variance_ratio: x,
            autocorrelation_rise: x,
        }),
        ask(Signal::Guard {
            guard: "g".into(),
            value: x,
            bound: x,
        }),
        ask(Signal::Audit {
            audit: "a".into(),
            target_delta: x,
            audit_delta: x,
        }),
        ask(Signal::Capacity {
            metric: "m".into(),
            drift: x,
            most: x,
        }),
        Record::Report {
            claim: Claim::Measured,
            venture: "shop".into(),
            metric: "m".into(),
            value: Some(x),
            note: String::new(),
            source: None,
            confidence: Some(x),
            cites: vec![],
            implausible: None,
            facts: Some(avsmd::facts::Facts {
                metrics: [(
                    "m".to_string(),
                    avsmd::facts::MetricFacts {
                        last: x,
                        min: x,
                        max: x,
                        readings: 1,
                    },
                )]
                .into(),
                ..Default::default()
            }),
        },
        Record::Prediction {
            claim: Claim::Inferred,
            venture: "shop".into(),
            model: "baseline".into(),
            metric: "m".into(),
            action: None,
            made_at: 1,
            resolves_at: 2,
            quantiles: q(x),
            events: vec![Event {
                name: "e".into(),
                p: x,
            }],
        },
        Record::Branch {
            claim: Claim::Inferred,
            venture: "shop".into(),
            chosen: alt.clone(),
            alternatives: vec![alt],
            members: 1,
            horizon: 1,
            separated: true,
        },
        Record::Forecast {
            claim: Claim::Inferred,
            venture: "shop".into(),
            policy: "all".into(),
            horizon: 1,
            seeds: 1,
            variant: None,
            metrics: vec![Envelope {
                name: "m".into(),
                quantiles: q(x),
            }],
            guards: vec![Event {
                name: "g".into(),
                p: x,
            }],
            reached: Reached {
                spend: q(x),
                asks: q(x),
                refusals: q(x),
                halts: q(x),
                delegations: q(x),
            },
            cites: vec![],
        },
    ]
}

/// Equal, with NaN equal to NaN: compare the stored text.
fn same(a: &Record, b: &Record) -> bool {
    a.canonical() == b.canonical()
}

#[test]
fn non_finite_numbers_round_trip_through_the_ledger() {
    let mut ledger = Ledger::open_in_memory().unwrap();
    let mut written = vec![];
    for x in NANS {
        for r in records(x) {
            ledger.append(r.clone()).expect("append");
            written.push(r);
        }
    }
    let read = ledger.all().expect("every record reads back");
    assert_eq!(read.len(), written.len());
    for (e, w) in read.iter().zip(&written) {
        assert!(same(&e.record, w), "{:?} != {:?}", e.record, w);
    }
    ledger.verify().unwrap();
    // The infinity is infinity, not null and not NaN.
    match &read[2].record {
        Record::Ask {
            signal: Some(Signal::Warning { variance_ratio, .. }),
            ..
        } => assert_eq!(*variance_ratio, f64::INFINITY),
        other => panic!("{other:?}"),
    }
}

#[test]
fn finite_numbers_stay_plain_and_null_reads_as_nan() {
    let r = ask(Signal::Warning {
        metric: "m".into(),
        variance_ratio: 2.5,
        autocorrelation_rise: 0.25,
    });
    let text = r.canonical();
    assert!(text.contains(r#""variance_ratio":2.5"#), "{text}");
    let old = text.replace("2.5", "null");
    match serde_json::from_str::<Record>(&old).unwrap() {
        Record::Ask {
            signal: Some(Signal::Warning { variance_ratio, .. }),
            ..
        } => assert!(variance_ratio.is_nan()),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_record_that_cannot_be_read_back_is_not_written() {
    // serde_json writes any depth and refuses to read past 128.
    let mut deep = serde_json::Value::Null;
    for _ in 0..200 {
        deep = serde_json::Value::Array(vec![deep]);
    }
    let unreadable = Record::Proposal {
        claim: Claim::Inferred,
        venture: "shop".into(),
        action: "a".into(),
        reason: "r".into(),
        payload: Some(Payload::Json { json: deep }),
        cites: vec![],
    };
    assert!(serde_json::from_str::<Record>(&unreadable.canonical()).is_err());
    let mut ledger = Ledger::open_in_memory().unwrap();
    ledger.append(records(1.0).remove(0)).unwrap();
    assert!(ledger.append(unreadable).is_err());
    assert_eq!(ledger.len().unwrap(), 1, "nothing was written");
    ledger.all().expect("the ledger still reads");
    ledger.append(records(2.0).remove(0)).unwrap();
}
