//! What avsm knows: `know` over a venture and over a topic, a summary
//! crystallized past the venture's bound with the leaves leaving the
//! context and not the ledger, and a model drilling into the ledger
//! through the roles process, every drill a record.

use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use avsm::kernel::{Kernel, RolesProcess, WardenProcess};
use avsm::record::{Claim, Kind, Record};
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

fn home(parent: &Path, config: &str) -> PathBuf {
    let home = parent.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.yaml"), config).unwrap();
    home
}

fn venture(parent: &Path, name: &str, mandate: &str) -> PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("mandate.yaml"), mandate).unwrap();
    dir
}

fn finding(venture: &str, topic: &str, note: &str, cites: Vec<u64>) -> Record {
    Record::Report {
        claim: Claim::Inferred,
        venture: venture.into(),
        metric: topic.into(),
        value: None,
        note: note.into(),
        source: Some("https://x.example".into()),
        confidence: Some(0.7),
        cites,
        implausible: None,
        facts: None,
    }
}

#[test]
fn know_lists_current_facts_by_venture_and_by_topic() {
    let tmp = tempfile::tempdir().unwrap();
    let home = home(tmp.path(), "nightly: never\n");
    let mut k = Kernel::with(
        &home,
        Box::new(Scripted {
            replies: VecDeque::new(),
            seen: Arc::new(Mutex::new(vec![])),
        }),
        Box::new(LocalWarden::open(&home).unwrap()),
    )
    .unwrap();
    for name in ["shop", "farm"] {
        k.add(&venture(
            tmp.path(),
            name,
            &format!("purpose: Run {name}.\nmetric: shell echo 5\ncontroller: {{kind: model}}\ncadence: 1h\n"),
        ))
        .unwrap();
    }
    k.report("shop", false);
    k.report("shop", false);
    let old = k
        .append(finding("shop", "pricing", "rivals charge 9", vec![]))
        .unwrap()
        .seq;
    let new = k
        .append(finding("shop", "pricing", "rivals now charge 8", vec![old]))
        .unwrap()
        .seq;
    let farm = k
        .append(finding("farm", "pricing", "hay is cheap", vec![]))
        .unwrap()
        .seq;

    let text = k.know("shop").unwrap();
    assert!(text.starts_with("shop: 2 current facts\n"), "{text}");
    assert!(text.contains("measured  shell=5"), "{text}");
    assert_eq!(
        text.matches("shell=5").count(),
        1,
        "only the latest reading: {text}"
    );
    assert!(
        !text.contains("rivals charge 9"),
        "superseded by citation: {text}"
    );
    assert!(
        text.contains(&format!("#{new:<5} ")) && text.contains("inferred  pricing=?  rivals now charge 8  (source https://x.example; confidence 0.7; leaves #{old})")
            || text.contains(&format!("leaves #{old}")),
        "{text}"
    );
    let text = k.know("pricing").unwrap();
    assert!(text.starts_with("pricing: 2 current facts\n"), "{text}");
    assert!(
        text.contains("shop  pricing=?  rivals now charge 8"),
        "{text}"
    );
    assert!(
        text.contains(&format!("#{farm:<5} ")) && text.contains("farm  pricing=?  hay is cheap"),
        "{text}"
    );
    let text = k.know("weather").unwrap();
    assert!(text.starts_with("nothing is known about weather"), "{text}");
    let text = k.handle(&["know".to_string(), "farm".to_string()]).unwrap();
    assert!(text.starts_with("farm: 1 current fact\n"), "{text}");
}

#[test]
fn past_the_bound_the_older_records_are_summarized_and_leave_the_context() {
    let tmp = tempfile::tempdir().unwrap();
    let home = home(
        tmp.path(),
        "nightly: never\ndefault: k\nbackends:\n  k: {kind: command, run: echo nothing}\n",
    );
    let seen = Arc::new(Mutex::new(vec![]));
    let dir = venture(
        tmp.path(),
        "shop",
        "purpose: Sell.\nmetric: shell echo 5\ncontroller: {kind: model}\ncadence: 1h\nsummarize_after: 4\n\
         permitted:\n- {name: wipe, run: \"true\", cap: 9/day, reversible: false}\n",
    );
    // Eight reports: past the bound of four, the oldest six go to the
    // role, the newest two stay in the context as they are.
    let role = Scripted {
        replies: VecDeque::from(vec![vec![Record::Report {
            claim: Claim::Measured,      // the kernel makes it inferred regardless
            venture: "elsewhere".into(), // and the venture its own
            metric: "summary".into(),
            value: Some(1.0),
            note: "Eight readings of five.".into(),
            source: None,
            confidence: None,
            cites: (2..=7).collect(),
            implausible: None,
            facts: None,
        }]]),
        seen: Arc::clone(&seen),
    };
    let mut k = Kernel::with(
        &home,
        Box::new(role),
        Box::new(LocalWarden::open(&home).unwrap()),
    )
    .unwrap();
    k.add(&dir).unwrap();
    for _ in 0..8 {
        k.report("shop", false);
    }
    k.propose("shop", "wipe", None).unwrap(); // an open ask, always in context
    k.cycle("shop");
    let ctx = seen.lock().unwrap().pop().unwrap();
    let seqs = |v: &Vec<avsm::record::Entry>| v.iter().map(|e| e.seq).collect::<Vec<_>>();
    // #1 adopted, #2..#9 reports, #10 proposal, #11 ask: uncovered are 1..10 (11 is open).
    assert_eq!(
        seqs(&ctx.to_summarize),
        (1..=8).collect::<Vec<_>>(),
        "{:?}",
        seqs(&ctx.to_summarize)
    );
    assert_eq!(seqs(&ctx.entries), vec![9, 10]);
    assert_eq!(seqs(&ctx.asks), vec![11]);
    assert!(ctx.summaries.is_empty());
    let entries = k.entries().unwrap();
    let summary = entries.last().unwrap();
    assert!(
        matches!(&summary.record, Record::Report { claim: Claim::Inferred, venture, metric, value: None, cites, .. }
            if venture == "shop" && metric == "summary" && *cites == (2..=7).collect::<Vec<_>>()),
        "{}",
        summary.line()
    );
    assert_eq!(
        summary.record.summary(),
        "summary=?  Eight readings of five."
    );

    // The next call: the summary stands for #2..#7; they are in the ledger, not the context.
    k.cycle("shop");
    let ctx = seen.lock().unwrap().pop().unwrap();
    assert_eq!(seqs(&ctx.summaries), vec![summary.seq]);
    assert_eq!(seqs(&ctx.entries), vec![8, 9, 10]);
    assert!(ctx.to_summarize.is_empty());
    // The second turn gave no record, and still left one.
    let quiet = k.entries().unwrap().last().unwrap().clone();
    assert!(
        matches!(&quiet.record, Record::Report { claim: Claim::Inferred, metric, .. } if metric == "nothing"),
        "{}",
        quiet.line()
    );
    assert_eq!(
        k.tail(20).unwrap().lines().count(),
        13,
        "nothing left the ledger"
    );
    let text = k.know("shop").unwrap();
    assert!(
        text.contains("summary=?  Eight readings of five.  (leaves #2 #3 #4 #5 #6 #7)"),
        "{text}"
    );
    assert_eq!(k.ledger.verify().unwrap(), 13);
}

#[test]
fn a_model_drills_through_the_roles_process_and_each_drill_is_a_record() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    // The backend answers from a script: a look-up, then a look-up, then a choice.
    let backend = tmp.path().join("backend");
    std::fs::write(
        &backend,
        format!(
            "#!/bin/sh\nn=$(cat {0}/n 2>/dev/null || echo 0)\nn=$((n + 1))\necho $n > {0}/n\ncat > {0}/prompt-$n.json\n\
             case $n in\n\
             1) echo '{{\"choice\": \"know\", \"topic\": \"shop\"}}';;\n\
             2) echo '{{\"choice\": \"why\", \"record\": 2}}';;\n\
             *) echo '{{\"choice\": \"action\", \"action\": \"tidy\", \"reason\": \"looked\"}}';;\n\
             esac\n",
            tmp.path().display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&backend, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        home.join("config.yaml"),
        format!(
            "nightly: never\ndefault: k\nbackends:\n  k: {{kind: command, run: {}}}\n",
            backend.display()
        ),
    )
    .unwrap();
    let dir = venture(
        tmp.path(),
        "shop",
        "purpose: Sell.\nmetric: shell echo 5\ncontroller: {kind: model}\ncadence: 1h\n\
         permitted:\n- {name: tidy, run: touch tidied, cap: 3/day, reversible: true}\n",
    );
    let exe = Path::new(env!("CARGO_BIN_EXE_avsm"));
    let warden = WardenProcess::spawn_exe(exe, &home).unwrap();
    let roles = RolesProcess::spawn_exe(exe, &home).unwrap();
    let mut k = Kernel::with(&home, Box::new(roles), Box::new(warden)).unwrap();
    k.add(&dir).unwrap();
    k.report("shop", false);
    k.cycle("shop");
    let kinds: Vec<String> = k
        .entries()
        .unwrap()
        .iter()
        .map(|e| {
            format!(
                "{} {} {}",
                e.record.claim(),
                e.record.kind(),
                e.record.summary()
            )
        })
        .collect();
    assert_eq!(
        kinds[2], "inferred report know=?  shop: 2 lines shown",
        "{kinds:?}"
    );
    assert_eq!(
        kinds[3], "inferred report why=?  2: 2 lines shown",
        "{kinds:?}"
    );
    assert!(
        kinds[4].starts_with("inferred proposal tidy: looked"),
        "{kinds:?}"
    );
    assert_eq!(kinds[5], "measured intent tidy: starting", "{kinds:?}");
    assert!(
        kinds[6].starts_with("measured outcome #6 done"),
        "{kinds:?}"
    );
    assert!(dir.join("tidied").exists());
    let second: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(tmp.path().join("prompt-2.json")).unwrap())
            .unwrap();
    let user = second["user"].as_str().unwrap();
    assert!(
        user.contains("What you looked up:\nknow shop:\n  shop: 1 current fact\n"),
        "{user}"
    );
    let third: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(tmp.path().join("prompt-3.json")).unwrap())
            .unwrap();
    let user = third["user"].as_str().unwrap();
    assert!(user.contains("why 2:\n  #2 "), "{user}");
    assert!(user.contains("measured by code"), "{user}");
    assert_eq!(
        k.entries()
            .unwrap()
            .iter()
            .filter(|e| e.record.kind() == Kind::Report)
            .count(),
        3
    );
}

#[test]
fn the_kernel_computes_a_summarys_facts_and_ignores_the_roles() {
    use avsm::facts::{Facts, MetricFacts};
    let tmp = tempfile::tempdir().unwrap();
    let home = home(
        tmp.path(),
        "nightly: never\ndefault: k\nbackends:\n  k: {kind: command, run: echo nothing}\n",
    );
    let seen = Arc::new(Mutex::new(vec![]));
    let dir = venture(
        tmp.path(),
        "shop",
        "purpose: Sell.\nmetric: shell echo 5\ncontroller: {kind: model}\ncadence: 1h\nsummarize_after: 4\n",
    );
    let lie = Facts {
        metrics: [(
            "shell echo 5".to_string(),
            MetricFacts {
                last: 1.0,
                min: 1.0,
                max: 99999.0,
                readings: 77,
            },
        )]
        .into(),
        actions: 40,
        ..Facts::default()
    };
    let role = Scripted {
        replies: VecDeque::from(vec![vec![Record::Report {
            claim: Claim::Inferred,
            venture: "shop".into(),
            metric: "summary".into(),
            value: None,
            note: "Orders hit 99999 and the min was 1.".into(),
            source: None,
            confidence: None,
            cites: (2..=7).collect(),
            implausible: None,
            facts: Some(lie.clone()),
        }]]),
        seen: Arc::clone(&seen),
    };
    let mut k = Kernel::with(
        &home,
        Box::new(role),
        Box::new(LocalWarden::open(&home).unwrap()),
    )
    .unwrap();
    k.add(&dir).unwrap();
    for _ in 0..8 {
        k.report("shop", false);
    }
    k.cycle("shop");
    let entries = k.entries().unwrap();
    let summary = entries.last().unwrap();
    let stored = match &summary.record {
        Record::Report { facts, .. } => facts.clone().unwrap(),
        _ => panic!("{}", summary.line()),
    };
    let cited: Vec<_> = entries
        .iter()
        .filter(|e| (2..=7).contains(&e.seq))
        .cloned()
        .collect();
    assert_eq!(stored, Facts::compute(&cited));
    assert_ne!(stored, lie);
    assert_eq!(stored.metrics.len(), 1, "{stored:?}");
    let m = stored.metrics.values().next().unwrap();
    assert_eq!((m.last, m.min, m.max, m.readings), (5.0, 5.0, 5.0, 6));
}
