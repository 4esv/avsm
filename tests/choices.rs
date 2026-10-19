//! A model reply may carry up to five independent choices (#38). Each is
//! read as a single choice is, and each proposal goes through the gate alone.

use std::os::unix::fs::PermissionsExt;

use avsm::kernel::Kernel;
use avsm::record::{Claim, Kind, Record};
use avsm::warden::LocalWarden;

const MANDATE: &str =
    "purpose: Answer the mail.\nmetric: unread\ncontroller: {kind: model}\ncadence: 1h\n\
    permitted:\n\
    - {name: tidy, run: touch tidied, cap: 1/day, reversible: true}\n\
    - {name: reply, run: touch replied, cap: 5/day, reversible: true}\n\
    - {name: archive, run: touch archived, cap: 5/day, reversible: true}\n\
    forbidden: [close]\n";

/// A venture whose model backend prints `reply`, run through one cycle.
fn cycle(reply: &str) -> Vec<avsm::record::Entry> {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let reply_file = tmp.path().join("reply.txt");
    std::fs::write(&reply_file, reply).unwrap();
    std::fs::write(
        home.join("config.yaml"),
        format!(
            "nightly: never\ndefault: k\nbackends:\n  k: {{kind: command, run: \"cat > /dev/null; cat {}\"}}\n",
            reply_file.display()
        ),
    )
    .unwrap();
    let dir = tmp.path().join("shop");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("mandate.yaml"), MANDATE).unwrap();
    let report = dir.join("report");
    std::fs::write(&report, "#!/bin/sh\necho 5\n").unwrap();
    std::fs::set_permissions(&report, std::fs::Permissions::from_mode(0o755)).unwrap();
    let warden = LocalWarden::open(&home).unwrap();
    let mut k = Kernel::with(&home, Box::new(avsm::role::ModelRole), Box::new(warden)).unwrap();
    k.add(&dir).unwrap();
    k.cycle("shop");
    k.entries().unwrap()
}

fn count(entries: &[avsm::record::Entry], kind: Kind) -> usize {
    entries.iter().filter(|e| e.record.kind() == kind).count()
}

fn unusable(entries: &[avsm::record::Entry]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|e| match &e.record {
            Record::Report {
                metric,
                note,
                claim,
                ..
            } if metric == "unusable" => {
                assert_eq!(*claim, Claim::Inferred);
                Some(note.clone())
            }
            _ => None,
        })
        .collect()
}

fn ran_lever(entries: &[avsm::record::Entry], name: &str) -> bool {
    entries
        .iter()
        .any(|e| e.record.kind() == Kind::Intent && e.record.summary().starts_with(name))
}

#[test]
fn three_choices_make_three_proposals_each_gated_alone() {
    // tidy is capped at one a day: the second tidy is refused, and neither
    // the first tidy nor the reply after it is stopped by that.
    let e = cycle(
        r#"{"choices":[
            {"choice":"action","action":"tidy","reason":"a"},
            {"choice":"action","action":"tidy","reason":"b"},
            {"choice":"action","action":"reply","reason":"c"}]}"#,
    );
    assert_eq!(count(&e, Kind::Proposal), 3);
    assert_eq!(count(&e, Kind::Intent), 2, "tidy once and reply once");
    assert_eq!(count(&e, Kind::Refusal), 1);
    assert!(ran_lever(&e, "tidy") && ran_lever(&e, "reply"));
    assert!(unusable(&e).is_empty());
}

#[test]
fn a_sixth_choice_is_dropped_and_says_so_in_one_unusable_record() {
    // Five are used; the sixth is not run, and the cut is on the ledger
    // rather than silent. Refusing the whole reply would throw away five
    // good choices for one extra.
    let one = r#"{"choice":"action","action":"archive","reason":"x"},"#;
    let e = cycle(&format!(
        r#"{{"choices":[{}{}{}{}{}{{"choice":"action","action":"reply","reason":"sixth"}}]}}"#,
        one, one, one, one, one
    ));
    assert_eq!(count(&e, Kind::Proposal), 5);
    assert_eq!(count(&e, Kind::Intent), 5);
    assert!(!ran_lever(&e, "reply"), "the sixth never ran");
    let u = unusable(&e);
    assert_eq!(u.len(), 1, "{u:?}");
    assert!(
        u[0].contains("6 choices") && u[0].contains("at most 5"),
        "{u:?}"
    );
}

#[test]
fn a_malformed_choice_among_good_ones_leaves_its_own_unusable_record() {
    let e = cycle(
        r#"{"choices":[
            {"choice":"action","action":"reply","reason":"a"},
            {"choice":"dance"},
            "not an object",
            {"choice":"ask","question":"is it fine?"},
            {"choice":"action","action":"archive","reason":"b"}]}"#,
    );
    assert_eq!(count(&e, Kind::Proposal), 2);
    assert_eq!(count(&e, Kind::Ask), 1);
    assert_eq!(count(&e, Kind::Intent), 2);
    assert_eq!(unusable(&e).len(), 2, "{:?}", unusable(&e));
    // A reply with usable choices is answered; it does not escalate.
    assert_eq!(count(&e, Kind::Escalation), 0);
}

#[test]
fn the_single_form_is_unchanged() {
    let e = cycle(r#"{"choice":"action","action":"reply","reason":"one"}"#);
    assert_eq!(count(&e, Kind::Proposal), 1);
    assert_eq!(count(&e, Kind::Intent), 1);
    assert!(unusable(&e).is_empty());
    let e = cycle(r#"{"choice":"nothing","reason":"quiet"}"#);
    assert_eq!(count(&e, Kind::Proposal), 0);
    assert_eq!(count(&e, Kind::Intent), 0);
}

#[test]
fn an_injected_choice_naming_a_forbidden_lever_is_refused_like_a_single_one() {
    let single = cycle(r#"{"choice":"action","action":"close","reason":"injected"}"#);
    assert_eq!(count(&single, Kind::Proposal), 0);
    assert_eq!(unusable(&single).len(), 1);
    let many = cycle(
        r#"{"choices":[
            {"choice":"action","action":"reply","reason":"ok"},
            {"choice":"action","action":"close","reason":"injected"}]}"#,
    );
    assert_eq!(count(&many, Kind::Proposal), 1, "only the permitted one");
    assert!(!ran_lever(&many, "close"));
    let u = unusable(&many);
    assert_eq!(u.len(), 1);
    assert!(
        u[0].contains("close") && u[0].contains("does not permit"),
        "{u:?}"
    );
    assert!(ran_lever(&many, "reply"));
}

#[test]
fn an_empty_or_wholly_malformed_list_is_unusable_as_a_whole_reply_is() {
    let e = cycle(r#"{"choices":[]}"#);
    assert_eq!(unusable(&e).len(), 1);
    let e = cycle(r#"{"choices":[{"choice":"dance"},{"choice":"hop"}]}"#);
    assert_eq!(unusable(&e).len(), 2);
    assert_eq!(count(&e, Kind::Proposal), 0);
}
