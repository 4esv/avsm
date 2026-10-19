//! The improving loop never edits limits: a delegate mandate that nobody
//! amended may only turn `delegated` from false to true.

use std::collections::VecDeque;

use avsmd::kernel::Kernel;
use avsmd::record::{Claim, Entry, Kind, Payload, Record};
use avsmd::role::{Context, InferRequest, Role};
use avsmd::warden::LocalWarden;

struct Scripted {
    replies: VecDeque<Vec<Record>>,
}

impl Role for Scripted {
    fn decide(
        &mut self,
        _ctx: &Context,
        _infer: &mut dyn FnMut(&InferRequest) -> Result<String, String>,
        _drill: &mut dyn FnMut(&str, &str) -> Result<String, String>,
    ) -> Result<Vec<Record>, String> {
        Ok(self.replies.pop_front().unwrap_or_default())
    }
}

const SHOP: &str = "purpose: Sell more.\nmetric: shell echo 5\ncontroller: {kind: model}\ncadence: 1h\n\
permitted:\n  - name: announce\n    run: \"cat >> announced\"\n    cap: 9/day\n    reversible: true\n    takes: text\n\
\x20 - name: tweet\n    run: \"cat >> tweeted\"\n    cap: 2/day\n    reversible: true\n    delegated: true\n    takes: text\n";

fn open_ask(k: &Kernel, lever: &str) -> Option<u64> {
    k.asks()
        .unwrap()
        .lines()
        .find(|l| l.contains(&format!(" {lever}? ")))
        .map(|l| {
            l.trim_start_matches('#')
                .split_whitespace()
                .next()
                .unwrap()
                .parse()
                .unwrap()
        })
}

struct Shop {
    _tmp: tempfile::TempDir,
    dir: std::path::PathBuf,
    k: Kernel,
}

/// A shop whose announce lever was approved once, and whose mandate file
/// on disk then says `on_disk`; the next tick is the ratchet's, and what it
/// proposes is the adopted mandate, never the file. Returns the shop and
/// the delegate ask.
fn ratcheted(on_disk: &str) -> (Shop, u64) {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.yaml"),
        "nightly: never\ndefault: k\nbackends:\n  k: {kind: command, run: echo nothing}\nratchet: 1\n",
    )
    .unwrap();
    let dir = tmp.path().join("shop");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("mandate.yaml"), SHOP).unwrap();
    let announce = vec![Record::Proposal {
        cites: vec![],
        claim: Claim::Inferred,
        venture: "shop".into(),
        action: "announce".into(),
        reason: "news".into(),
        payload: Some(Payload::Text {
            text: "Hello".into(),
        }),
    }];
    let role = Scripted {
        replies: VecDeque::from(vec![announce]),
    };
    let mut k = Kernel::with(
        &home,
        Box::new(role),
        Box::new(LocalWarden::open(&home).unwrap()),
    )
    .unwrap();
    k.adopt_self().unwrap();
    k.add(&dir).unwrap();
    k.cycle("shop");
    let ask = open_ask(&k, "announce").expect("announce asks each time");
    k.approve(ask, None).unwrap();
    std::fs::write(dir.join("mandate.yaml"), on_disk).unwrap();
    k.tick();
    let ask = open_ask(&k, "delegate").expect("the ratchet proposed");
    (Shop { _tmp: tmp, dir, k }, ask)
}

/// A delegate ask for `text` that no human amended, written as the
/// improving loop writes one: what a broken or compromised loop could ask.
fn forged(s: &mut Shop, text: &str) -> u64 {
    s.k.append(Record::Ask {
        claim: Claim::Measured,
        venture: "shop".into(),
        action: Some("delegate".into()),
        question: "may I delegate?".into(),
        refusal: None,
        payload: Some(Payload::Text { text: text.into() }),
        signal: None,
        packet: None,
        cites: vec![],
    })
    .unwrap()
    .seq
}

fn delegated(text: &str) -> String {
    text.replacen(
        "    takes: text\n",
        "    takes: text\n    delegated: true\n",
        1,
    )
}

fn refusal(entries: &[Entry]) -> Option<String> {
    entries.iter().find_map(|e| match &e.record {
        Record::Refusal { reason, detail, .. } if reason == "EditsLimits" => Some(detail.clone()),
        _ => None,
    })
}

fn lines(entries: &[Entry]) -> String {
    entries
        .iter()
        .map(|e| e.line())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_ratchets_own_proposal_is_adopted() {
    let (mut s, ask) = ratcheted(SHOP);
    s.k.approve(ask, None).unwrap();
    s.k.tick();
    let entries = s.k.entries().unwrap();
    assert!(refusal(&entries).is_none(), "{}", lines(&entries));
    assert_eq!(
        std::fs::read_to_string(s.dir.join("mandate.yaml")).unwrap(),
        delegated(SHOP),
        "{}",
        lines(&entries)
    );
}

#[test]
fn a_proposal_that_also_raises_a_cap_is_refused() {
    let raised = SHOP.replace("cap: 9/day", "cap: 90/day");
    let (mut s, _) = ratcheted(SHOP);
    let ask = forged(&mut s, &raised);
    s.k.approve(ask, None).unwrap();
    s.k.tick();
    let entries = s.k.entries().unwrap();
    let why = refusal(&entries).unwrap_or_else(|| panic!("no EditsLimits\n{}", lines(&entries)));
    assert!(why.contains("cap"), "{why}");
    assert_eq!(
        std::fs::read_to_string(s.dir.join("mandate.yaml")).unwrap(),
        SHOP,
        "the file is not rewritten"
    );
    assert!(
        !entries
            .iter()
            .any(|e| e.record.summary().contains("mandate rewritten")),
        "nothing adopted it\n{}",
        lines(&entries)
    );
}

#[test]
fn a_guard_change_is_refused_too() {
    let guarded = SHOP.replace(
        "cadence: 1h\n",
        "cadence: 1h\nguards:\n  - {name: load, run: echo 1, above: 5}\n",
    );
    let (mut s, _) = ratcheted(SHOP);
    let ask = forged(&mut s, &guarded);
    s.k.approve(ask, None).unwrap();
    s.k.tick();
    let entries = s.k.entries().unwrap();
    let why = refusal(&entries).unwrap_or_else(|| panic!("no EditsLimits\n{}", lines(&entries)));
    assert!(why.contains("guards"), "{why}");
}

#[test]
fn the_same_change_amended_by_a_human_is_adopted() {
    let raised = SHOP.replace("cap: 9/day", "cap: 90/day");
    let (mut s, ask) = ratcheted(SHOP);
    let amended = delegated(&raised);
    s.k.approve(ask, Some(&amended)).unwrap();
    s.k.tick();
    let entries = s.k.entries().unwrap();
    assert!(refusal(&entries).is_none(), "{}", lines(&entries));
    assert_eq!(
        std::fs::read_to_string(s.dir.join("mandate.yaml")).unwrap(),
        amended,
        "{}",
        lines(&entries)
    );
    assert!(entries.iter().any(|e| e.record.kind() == Kind::Outcome));
}

#[test]
fn turning_delegated_off_is_refused_too() {
    let off = SHOP.replace("    delegated: true\n", "");
    let (mut s, _) = ratcheted(SHOP);
    let ask = forged(&mut s, &off);
    s.k.approve(ask, None).unwrap();
    s.k.tick();
    let entries = s.k.entries().unwrap();
    let why = refusal(&entries).unwrap_or_else(|| panic!("no EditsLimits\n{}", lines(&entries)));
    assert!(why.contains("delegated"), "{why}");
    assert_eq!(
        std::fs::read_to_string(s.dir.join("mandate.yaml")).unwrap(),
        SHOP
    );
}

#[test]
fn an_edit_on_disk_nobody_adopted_never_rides_along() {
    let raised = SHOP.replace("cap: 9/day", "cap: 90/day");
    let (s, ask) = ratcheted(&raised);
    let payload =
        s.k.entries()
            .unwrap()
            .into_iter()
            .find(|e| e.seq == ask)
            .and_then(|e| e.record.payload().map(|p| p.text()))
            .unwrap();
    assert_eq!(
        payload,
        delegated(SHOP),
        "the proposal is the adopted mandate, delegated"
    );
}
