//! An adoption stores the mandate text; the ledger's text is the mandate in
//! force; `why` names the mandate version that authorized a record.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use avsmd::kernel::Kernel;
use avsmd::mandate::Mandate;
use avsmd::record::{Claim, Entry, Record, Verdict};
use avsmd::role::{Context, InferRequest, Role};
use avsmd::selfventure;
use avsmd::warden::LocalWarden;

const MANDATE: &str = "purpose: Keep the counter below ten.\n\
     metric: count\n\
     controller:\n  kind: threshold\n  above: 10\n  action: reset\n\
     cadence: 30s\n\
     permitted:\n  - name: reset\n    run: ./reset\n    cap: 9/day\n    reversible: true\n\
     \x20 - name: post\n    run: ./post\n    cap: 9/day\n    reversible: false\n    delegated: true\n";

fn venture(parent: &Path, mandate: &str) -> PathBuf {
    let dir = parent.join("counter");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("mandate.yaml"), mandate).unwrap();
    for name in ["report", "reset", "post"] {
        std::fs::write(dir.join(name), "#!/bin/sh\necho 3\n").unwrap();
        std::fs::set_permissions(dir.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

struct Quiet;

impl Role for Quiet {
    fn decide(
        &mut self,
        _ctx: &Context,
        _infer: &mut dyn FnMut(&InferRequest) -> Result<String, String>,
        _drill: &mut dyn FnMut(&str, &str) -> Result<String, String>,
    ) -> Result<Vec<Record>, String> {
        Ok(Vec::new())
    }
}

fn kernel(home: &Path) -> Kernel {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(home.join("config.yaml"), "nightly: never\n").unwrap();
    let warden = LocalWarden::open(home).unwrap();
    Kernel::with(home, Box::new(Quiet), Box::new(warden)).unwrap()
}

fn adoptions(k: &Kernel, venture: &str) -> Vec<Entry> {
    k.entries()
        .unwrap()
        .into_iter()
        .filter(|e| {
            matches!(&e.record, Record::Decision { venture: v, verdict: Verdict::Adopt { .. }, .. } if v == venture)
        })
        .collect()
}

fn text_of(e: &Entry) -> Option<String> {
    match &e.record {
        Record::Decision {
            verdict: Verdict::Adopt { text, .. },
            ..
        } => text.clone(),
        _ => panic!("not an adoption"),
    }
}

fn seq_of(out: &str) -> u64 {
    out.lines()
        .find(|l| l.contains("intent    "))
        .unwrap_or_else(|| out.lines().next().unwrap())
        .trim_start_matches('#')
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn add_stores_the_text_and_the_ledger_text_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = venture(tmp.path(), MANDATE);
    {
        let mut k = kernel(&home);
        k.add(&dir).unwrap();
        let adopted = adoptions(&k, "counter");
        assert_eq!(text_of(&adopted[0]).as_deref(), Some(MANDATE));
        let Record::Decision {
            verdict: Verdict::Adopt { mandate, .. },
            ..
        } = &adopted[0].record
        else {
            unreachable!()
        };
        assert_eq!(*mandate, Mandate::hash(MANDATE), "the hash is of the text");
    }

    // The file changes; a restart reads the ledger, not the file.
    let changed = MANDATE.replace("below ten", "below twenty");
    std::fs::write(dir.join("mandate.yaml"), &changed).unwrap();
    let mut k = kernel(&home);
    let status = k.status().unwrap();
    assert!(status.contains("changed on disk"), "{status}");
    assert!(status.contains("avsm add"), "{status}");
    let in_force = k.adopted();
    assert_eq!(in_force.len(), 1, "the adopted text still runs");
    assert_eq!(in_force[0].mandate.purpose, "Keep the counter below ten.");
    let out = k.propose("counter", "reset", None).unwrap();
    assert!(
        out.contains("intent    reset: starting") && out.contains(" done"),
        "{out}"
    );

    // It takes `add` to adopt the new text.
    k.add(&dir).unwrap();
    assert_eq!(adoptions(&k, "counter").len(), 2);
    assert_eq!(
        text_of(&adoptions(&k, "counter")[1]).as_deref(),
        Some(changed.as_str())
    );
    assert_eq!(
        k.adopted()[0].mandate.purpose,
        "Keep the counter below twenty."
    );
    assert!(!k.status().unwrap().contains("changed on disk"));
}

#[test]
fn a_venture_runs_from_the_ledger_when_its_directory_is_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = venture(tmp.path(), MANDATE);
    {
        let mut k = kernel(&home);
        k.add(&dir).unwrap();
    }
    std::fs::remove_dir_all(&dir).unwrap();
    let k = kernel(&home);
    let status = k.status().unwrap();
    assert!(
        status.contains("cannot read"),
        "reported, not hidden: {status}"
    );
    assert_eq!(
        k.adopted().len(),
        1,
        "the adopted text is still the mandate"
    );
}

#[test]
fn why_names_the_mandate_version() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = venture(tmp.path(), MANDATE);
    let mut k = kernel(&home);
    k.add(&dir).unwrap();
    let first = adoptions(&k, "counter")[0].clone();
    let out = k.propose("counter", "reset", None).unwrap();
    let action = seq_of(&out);
    let why = k.why(action).unwrap();
    let want = format!(
        "counter mandate v1 (#{}, {})",
        first.seq,
        &Mandate::hash(MANDATE)[..12]
    );
    assert!(why.contains(&want), "{want} in {why}");

    // A second adoption is version two, and the next action rests on it.
    std::fs::write(
        dir.join("mandate.yaml"),
        MANDATE.replace("below ten", "below twenty"),
    )
    .unwrap();
    k.add(&dir).unwrap();
    let second = adoptions(&k, "counter")[1].clone();
    let out = k.propose("counter", "reset", None).unwrap();
    let why2 = k.why(seq_of(&out)).unwrap();
    assert!(
        why2.contains(&format!("counter mandate v2 (#{}, ", second.seq)),
        "{why2}"
    );
    // The first action still names the version it rested on.
    assert!(k.why(action).unwrap().contains(&want));
}

#[test]
fn why_names_the_version_on_an_intent_and_its_outcome() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = venture(tmp.path(), MANDATE);
    let mut k = kernel(&home);
    k.add(&dir).unwrap();
    let adopt = adoptions(&k, "counter")[0].seq;
    // `post` is irreversible and delegated: it runs as an intent and an
    // outcome.
    k.propose("counter", "post", None).unwrap();
    let entries = k.entries().unwrap();
    let want = format!("counter mandate v1 (#{adopt}, ");
    let mut seen = 0;
    for e in &entries {
        if matches!(e.record, Record::Intent { .. } | Record::Outcome { .. }) {
            let why = k.why(e.seq).unwrap();
            assert!(why.contains(&want), "{why}");
            seen += 1;
        }
    }
    assert!(seen >= 2, "an intent and an outcome were written: {seen}");
}

#[test]
fn the_self_adoption_stores_its_text() {
    let Record::Decision {
        claim: Claim::Declared,
        verdict: Verdict::Adopt { mandate, text, .. },
        ..
    } = selfventure::adopt_record()
    else {
        panic!("not an adoption")
    };
    assert_eq!(text.as_deref(), Some(selfventure::MANDATE));
    assert_eq!(mandate, Mandate::hash(selfventure::MANDATE));
}
