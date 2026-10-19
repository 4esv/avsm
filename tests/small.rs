//! Small findings (E19): payload approval, payload schema, digest
//! precision, shell metric label.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use avsmd::kernel::Kernel;
use avsmd::record::{Claim, Kind, Payload, Record};
use avsmd::role::{Context, InferRequest, Role};
use avsmd::warden::LocalWarden;

struct Scripted {
    replies: VecDeque<Vec<Record>>,
    seen: Arc<Mutex<Vec<Context>>>,
}

impl Role for Scripted {
    fn decide(
        &mut self,
        ctx: &Context,
        _infer: &mut dyn FnMut(&InferRequest) -> Result<String, String>,
        _drill: &mut dyn FnMut(&str, &str) -> Result<String, String>,
    ) -> Result<Vec<Record>, String> {
        self.seen.lock().unwrap().push(ctx.clone());
        Ok(self.replies.pop_front().unwrap_or_default())
    }
}

fn kernel(parent: &Path, mandate: &str, replies: Vec<Vec<Record>>) -> (Kernel, PathBuf) {
    let home = parent.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.yaml"),
        "nightly: never\ndefault: k\nbackends:\n  k: {kind: command, run: echo nothing}\n",
    )
    .unwrap();
    let role = Scripted {
        replies: replies.into(),
        seen: Arc::new(Mutex::new(vec![])),
    };
    let mut k = Kernel::with(
        &home,
        Box::new(role),
        Box::new(LocalWarden::open(&home).unwrap()),
    )
    .unwrap();
    k.adopt_self().unwrap();
    let dir = parent.join("shop");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("mandate.yaml"), mandate).unwrap();
    k.add(&dir).unwrap();
    (k, dir)
}

fn proposal(action: &str, payload: Option<Payload>) -> Vec<Record> {
    vec![Record::Proposal {
        claim: Claim::Inferred,
        venture: "shop".into(),
        action: action.into(),
        reason: "because".into(),
        payload,
        cites: vec![],
    }]
}

fn first_ask(k: &Kernel) -> u64 {
    k.asks()
        .unwrap()
        .lines()
        .find(|l| l.starts_with('#'))
        .map(|l| {
            l.trim_start_matches('#')
                .split_whitespace()
                .next()
                .unwrap()
                .parse()
                .unwrap()
        })
        .expect("an open ask")
}

const HEAD: &str =
    "purpose: Sell more.\nmetric: shell echo 5\ncontroller: {kind: model}\ncadence: 1h\npermitted:\n";

#[test]
fn approving_an_ask_that_needs_a_payload_and_has_none_refuses_and_records_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let mandate = format!(
        "{HEAD}  - name: announce\n    run: \"cat >> announced\"\n    cap: 9/day\n    reversible: false\n    takes: text\n"
    );
    let (mut k, dir) = kernel(tmp.path(), &mandate, vec![proposal("announce", None)]);
    k.cycle("shop");
    let ask = first_ask(&k);
    let before = k.entries().unwrap().len();
    let err = k.approve(ask, None).unwrap_err();
    assert!(
        err.contains("takes a payload") && err.contains("none"),
        "{err}"
    );
    assert_eq!(k.entries().unwrap().len(), before, "nothing recorded");
    assert!(!dir.join("announced").exists());
}

#[test]
fn a_payload_given_at_approval_carries_a_payloadless_ask_through() {
    let tmp = tempfile::tempdir().unwrap();
    let mandate = format!(
        "{HEAD}  - name: announce\n    run: \"cat >> announced\"\n    cap: 9/day\n    reversible: false\n    takes: text\n"
    );
    let (mut k, dir) = kernel(tmp.path(), &mandate, vec![proposal("announce", None)]);
    k.cycle("shop");
    let ask = first_ask(&k);
    k.approve(ask, Some("hello")).unwrap();
    k.tick();
    assert_eq!(
        std::fs::read_to_string(dir.join("announced")).unwrap(),
        "hello"
    );
}

fn order_refusals(k: &Kernel) -> Vec<(String, String)> {
    k.entries()
        .unwrap()
        .iter()
        .filter_map(|e| match &e.record {
            Record::Refusal {
                action,
                reason,
                detail,
                closes: None,
                ..
            } if action == "order" => Some((reason.clone(), detail.clone())),
            _ => None,
        })
        .collect()
}

fn order_intents(k: &Kernel) -> usize {
    k.entries()
        .unwrap()
        .iter()
        .filter(|e| e.record.kind() == Kind::Intent && e.record.action() == Some("order"))
        .count()
}

/// A payload goes through the schema; the lever is `cat > ordered`.
fn order(payload: &str, check: &str) -> (Vec<(String, String)>, usize, bool) {
    let tmp = tempfile::tempdir().unwrap();
    let mandate = format!(
        "{HEAD}  - name: order\n    run: \"cat > ordered\"\n    cap: 9/day\n    reversible: false\n    takes: json\n    fields: {{qty: integer, sku: string, rush: bool, w: number}}\n{check}"
    );
    let (mut k, dir) = kernel(tmp.path(), &mandate, vec![]);
    let out = k.propose("shop", "order", Some(payload)).unwrap();
    let ask = out
        .lines()
        .find(|l| l.contains(" order? "))
        .unwrap()
        .trim_start_matches('#')
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    k.approve(ask, None).unwrap();
    k.tick();
    (
        order_refusals(&k),
        order_intents(&k),
        dir.join("ordered").exists(),
    )
}

#[test]
fn a_payload_that_fits_its_fields_runs() {
    let (refusals, intents, ran) = order(r#"{"qty": 3, "sku": "a", "rush": false, "w": 1.5}"#, "");
    assert!(refusals.is_empty(), "{refusals:?}");
    assert_eq!(intents, 1);
    assert!(ran);
}

#[test]
fn a_payload_that_misses_the_fields_is_a_typed_refusal_before_the_intent() {
    for (payload, says) in [
        (
            r#"{"quantity": 3, "sku": "a", "rush": false, "w": 1}"#,
            "qty",
        ),
        (
            r#"{"qty": 3, "sku": "a", "rush": false, "w": 1, "extra": 1}"#,
            "extra",
        ),
        (r#"{"qty": "3", "sku": "a", "rush": false, "w": 1}"#, "qty"),
        (
            r#"{"qty": 3.5, "sku": "a", "rush": false, "w": 1}"#,
            "integer",
        ),
        (r#"[1, 2]"#, "object"),
    ] {
        let (refusals, intents, ran) = order(payload, "");
        assert_eq!(refusals.len(), 1, "{payload}: {refusals:?}");
        assert_eq!(refusals[0].0, "PayloadSchema", "{payload}");
        assert!(refusals[0].1.contains(says), "{payload}: {refusals:?}");
        assert_eq!(intents, 0, "{payload}: an intent was written");
        assert!(!ran, "{payload}: the lever ran");
    }
}

#[test]
fn the_schema_is_checked_before_the_payload_check() {
    // The check would fail too; the schema is what is said.
    let (refusals, _, _) = order(r#"{"quantity": 3}"#, "    check: \"exit 1\"\n");
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert_eq!(refusals[0].0, "PayloadSchema");
}

#[test]
fn a_digest_delta_has_the_precision_of_the_metric() {
    let tmp = tempfile::tempdir().unwrap();
    let mandate = "purpose: Cool.\nmetric: shell echo 24\ncontroller: {kind: model}\ncadence: 1h\n";
    let (mut k, _) = kernel(tmp.path(), mandate, vec![]);
    let now = avsmd::kernel::now();
    k.ledger
        .append_at(
            Record::report(Claim::Measured, "shop", "shell", Some(27.6), ""),
            now - 2 * 86_400,
        )
        .unwrap();
    k.report("shop", false);
    let text = k.digest().unwrap();
    let line = text.lines().find(|l| l.starts_with("shop")).unwrap();
    assert!(line.contains("shell=24 (-3.6)"), "{text}");
    assert!(!line.contains("0000"), "{text}");
}

fn reported(mandate: &str) -> Vec<String> {
    let tmp = tempfile::tempdir().unwrap();
    let (mut k, _) = kernel(tmp.path(), mandate, vec![]);
    k.report("shop", false);
    k.entries()
        .unwrap()
        .iter()
        .filter_map(|e| match &e.record {
            Record::Report {
                venture,
                metric,
                value,
                ..
            } if venture == "shop" => Some(format!("{metric}={}", value.unwrap())),
            _ => None,
        })
        .collect()
}

#[test]
fn a_shell_metric_with_a_label_reports_under_the_label() {
    let labelled = "purpose: Cool.\nmetric: {shell: echo 7, label: temperature}\ncontroller: {kind: model}\ncadence: 1h\n";
    assert_eq!(reported(labelled), vec!["temperature=7"]);
    let plain = "purpose: Cool.\nmetric: shell echo 7\ncontroller: {kind: model}\ncadence: 1h\n";
    assert_eq!(reported(plain), vec!["shell=7"]);
}
