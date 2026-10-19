//! Small findings (E19): payload approval, payload schema, digest
//! precision, shell metric label.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use avsm::kernel::Kernel;
use avsm::record::{Claim, Payload, Record};
use avsm::role::{Context, InferRequest, Role};
use avsm::warden::LocalWarden;

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
