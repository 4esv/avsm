//! The mandate parser against the fixture files in tests/fixtures/mandates.

use std::path::{Path, PathBuf};
use std::time::Duration;

use avsm::mandate::{Cap, Controller, Mandate, Permitted, Restart};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/mandates")
        .join(name)
}

/// The message a fixture fails with, checked for jargon.
fn fails(name: &str) -> String {
    let msg = Mandate::load(&fixture(name))
        .expect_err("the fixture should not parse")
        .0;
    for jargon in ["variant", "serde", "untagged", "invalid type"] {
        assert!(!msg.contains(jargon), "{jargon:?} in: {msg}");
    }
    msg
}

#[test]
fn valid_fixture_parses_field_by_field() {
    let m = Mandate::load(&fixture("valid.yaml")).expect("valid.yaml parses");
    let expected = Mandate {
        purpose: "Keep the counter from running away.".into(),
        metric: "count".into(),
        controller: Controller::Threshold {
            above: Some(10.0),
            below: None,
            action: "reset".into(),
            deadband: None,
            hold: None,
        },
        permitted: vec![
            Permitted {
                name: "reset".into(),
                run: Some("./reset.sh".into()),
                adapter: None,
                cap: Cap {
                    count: 3,
                    per: Cap::DAY,
                },
                reversible: true,
                delegated: false,
                takes: None,
                cost: 0,
                check: None,
                expects: None,
                revoke_after: None,
                fields: None,
            },
            Permitted {
                name: "deploy".into(),
                run: Some("./deploy.sh".into()),
                adapter: None,
                cap: Cap {
                    count: 1,
                    per: Cap::DAY,
                },
                reversible: false,
                delegated: false,
                takes: None,
                cost: 0,
                check: None,
                expects: None,
                revoke_after: None,
                fields: None,
            },
        ],
        forbidden: vec!["rm".into()],
        cadence: Duration::from_secs(30),
        context_budget: 4000,
        summarize_after: 200,
        restart: Restart::OnFailure,
        network: None,
        children: vec![],
        parent: None,
        allocation: None,
        guards: vec![],
        audits: vec![],
        range: None,
        max_step: None,
        escalate: None,
        world: None,
        calibration: None,
        early_warning: false,
        better: None,
    };
    assert_eq!(m.purpose, expected.purpose);
    assert_eq!(m.metric, expected.metric);
    assert_eq!(m.controller, expected.controller);
    assert_eq!(m.permitted, expected.permitted);
    assert_eq!(m.forbidden, expected.forbidden);
    assert_eq!(m.cadence, expected.cadence);
    assert_eq!(m.context_budget, expected.context_budget);
    assert_eq!(m.restart, expected.restart);
    assert_eq!(m, expected);
}

#[test]
fn missing_metric_fixture_says_what_a_metric_is() {
    let msg = fails("missing-metric.yaml");
    assert_eq!(
        msg,
        "mandate needs a metric: the name of the number the report prints, like count"
    );
}

#[test]
fn bad_metric_fixture_says_a_metric_is_one_word() {
    let msg = fails("bad-metric.yaml");
    assert_eq!(
        msg,
        "metric must be one word like count or latency_ms, not \"the current count\""
    );
}

#[test]
fn forbidden_overlap_fixture_names_the_action() {
    let msg = fails("forbidden-overlap.yaml");
    assert_eq!(
        msg,
        "action \"deploy\" is both permitted and forbidden; choose one"
    );
}

#[test]
fn hash_of_fixture_text_is_stable_sha256() {
    let text = std::fs::read_to_string(fixture("valid.yaml")).unwrap();
    let hash = Mandate::hash(&text);
    assert_eq!(hash.len(), 64);
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(hash, Mandate::hash(&text));
    assert_ne!(hash, Mandate::hash(&format!("{text}\n")));
}

#[test]
fn load_of_a_missing_file_says_so() {
    let path = fixture("no-such-mandate.yaml");
    let msg = Mandate::load(&path).unwrap_err().0;
    assert!(
        msg.starts_with(&format!("cannot read {}: ", path.display())),
        "{msg}"
    );
}
