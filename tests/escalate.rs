//! Escalation: when the first backend's reply cannot be used, or is too big,
//! the `escalate:` backend is asked, recorded first, and its answer is
//! inferred like any other. A backend that leaves the machine is asked only
//! when the mandate says `escalate: offbox`, which no proposal can add.

use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use avsmd::kernel::Kernel;
use avsmd::record::{Claim, Entry, Kind, Payload, Record};
use avsmd::role::{Context, InferRequest, ModelRole, Role};
use avsmd::warden::LocalWarden;

/// Everything a scenario sets: what each backend prints and how it is
/// configured.
struct Setup {
    /// A shell script body: what the primary backend does.
    primary: &'static str,
    /// What the second backend does; it also keeps its request and leaves
    /// a `called` file.
    second: &'static str,
    /// Is `escalate:` set in config.yaml?
    configured: bool,
    /// Is the second backend flagged `offbox: true`?
    offbox: bool,
    /// Does the mandate say `escalate: offbox`?
    allowed: bool,
}

impl Default for Setup {
    fn default() -> Setup {
        Setup {
            primary: "echo banana",
            second: ASK,
            configured: true,
            offbox: false,
            allowed: false,
        }
    }
}

const ASK: &str = r#"echo '{"choice":"ask","question":"Should the shop deploy?"}'"#;
const DEPLOY: &str = r#"echo '{"choice":"action","action":"deploy","reason":"it looks ready"}'"#;

struct World {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    kernel: Kernel,
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn world(s: Setup) -> World {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let primary = root.join("primary.sh");
    script(&primary, &format!("cat > /dev/null\n{}", s.primary));
    let second = root.join("second.sh");
    script(
        &second,
        &format!(
            "cat > {r}/request.json\ntouch {r}/called\ncat {r}/home/ledger.db* | grep -aic escalation > {r}/in_ledger_at_call\n{}",
            s.second,
            r = root.display()
        ),
    );
    let config = format!(
        "nightly: never\ndefault: primary\n{}backends:\n  primary: {{kind: command, run: {}}}\n  second: {{kind: command, run: {}, offbox: {}}}\n",
        if s.configured { "escalate: second\n" } else { "" },
        primary.display(),
        second.display(),
        s.offbox
    );
    std::fs::write(home.join("config.yaml"), config).unwrap();
    let dir = root.join("shop");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mandate.yaml"),
        format!(
            "purpose: Ship.\nmetric: shell echo 5\ncontroller: {{kind: model}}\ncadence: 1h\n{}permitted:\n  - name: deploy\n    run: touch deployed\n    cap: 9/day\n    reversible: false\n",
            if s.allowed { "escalate: offbox\n" } else { "" }
        ),
    )
    .unwrap();
    let mut kernel = Kernel::with(
        &home,
        Box::new(ModelRole),
        Box::new(LocalWarden::open(&home).unwrap()),
    )
    .unwrap();
    kernel.adopt_self().unwrap();
    kernel.add(&dir).unwrap();
    World {
        _tmp: tmp,
        dir,
        kernel,
    }
}

impl World {
    fn called(&self) -> bool {
        self.dir.parent().unwrap().join("called").exists()
    }

    fn entries(&self) -> Vec<Entry> {
        self.kernel.entries().unwrap()
    }

    fn lines(&self) -> String {
        self.entries()
            .iter()
            .map(|e| e.line())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The first escalation, asked or refused.
    fn escalation(&self) -> Option<Entry> {
        self.entries()
            .into_iter()
            .find(|e| e.record.kind() == Kind::Escalation)
    }

    /// The first report of this metric.
    fn report(&self, metric: &str) -> Option<Entry> {
        self.entries()
            .into_iter()
            .find(|e| matches!(&e.record, Record::Report { metric: m, .. } if m == metric))
    }
}

/// The digest's incidents a week, as the last digest wrote it.
fn incidents(w: &World) -> Option<f64> {
    w.entries().iter().rev().find_map(|e| match &e.record {
        Record::Report {
            metric,
            value: Some(v),
            ..
        } if metric == "incidents_per_week" => Some(*v),
        _ => None,
    })
}

#[test]
fn a_venture_whose_metric_is_named_escalation_has_no_escalations() {
    let mut w = world(Setup {
        configured: false,
        ..Setup::default()
    });
    // A venture of its own, reporting a metric called escalation.
    let dir = w.dir.parent().unwrap().join("odd");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mandate.yaml"),
        "purpose: Count.\nmetric: escalation\ncontroller: {kind: model}\ncadence: 1h\n",
    )
    .unwrap();
    script(&dir.join("report"), "echo 7");
    w.kernel.add(&dir).unwrap();
    w.kernel.tick();
    let reports = w
        .entries()
        .iter()
        .filter(|e| matches!(&e.record, Record::Report { venture, metric, .. } if venture == "odd" && metric == "escalation"))
        .count();
    assert!(reports > 0, "the venture reported: {}", w.lines());
    assert!(w.escalation().is_none(), "{}", w.lines());
    w.kernel.record_digest().unwrap();
    assert_eq!(incidents(&w), Some(0.0), "{}", w.lines());
}

#[test]
fn an_unusable_reply_escalates_in_order() {
    let mut w = world(Setup::default());
    w.kernel.cycle("shop");
    assert!(w.called(), "{}", w.lines());
    let entries = w.entries();

    let at = |pred: &dyn Fn(&Entry) -> bool| entries.iter().position(pred).unwrap();
    let unusable = at(
        &|e| matches!(&e.record, Record::Report { metric, claim: Claim::Inferred, .. } if metric == "unusable"),
    );
    let escalation = at(&|e| {
        matches!(
            &e.record,
            Record::Escalation {
                claim: Claim::Measured,
                refused: None,
                ..
            }
        )
    });
    let answer = at(&|e| {
        matches!(
            &e.record,
            Record::Ask {
                claim: Claim::Inferred,
                ..
            }
        )
    });
    let outcome = at(&|e| matches!(&e.record, Record::Answer { .. }));
    assert!(
        unusable < escalation && escalation < answer && answer < outcome,
        "unusable {unusable}, escalation {escalation}, answer {answer}, outcome {outcome}\n{}",
        w.lines()
    );

    let esc = &entries[escalation];
    let Record::Escalation { from, to, why, .. } = &esc.record else {
        unreachable!()
    };
    assert_eq!((from.as_str(), to.as_str()), ("primary", "second"));
    assert!(why.contains("neither the JSON asked for"), "{why}");
    // The answer's records cite the escalation; the Answer cites them and
    // nothing else.
    assert_eq!(entries[answer].record.cites(), &[esc.seq]);
    let Record::Answer {
        claim,
        escalation: of,
        ok,
        cites,
        detail,
        ..
    } = &entries[outcome].record
    else {
        unreachable!()
    };
    assert_eq!(*claim, Claim::Measured);
    assert_eq!(*of, esc.seq);
    assert!(*ok);
    assert_eq!(cites, &vec![entries[answer].seq]);
    assert_eq!(*detail, format!("answer: #{}", entries[answer].seq));
    for c in cites {
        let cited = entries.iter().find(|e| e.seq == *c).unwrap();
        assert!(cited.record.cites().contains(&esc.seq), "{}", cited.line());
    }
    // An escalation is an incident in the digest.
    w.kernel.record_digest().unwrap();
    assert_eq!(incidents(&w), Some(1.0), "{}", w.lines());
    // Recorded, then done: the escalation was already stored when the
    // second backend was called.
    let in_ledger: u32 = std::fs::read_to_string(w.dir.parent().unwrap().join("in_ledger_at_call"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(in_ledger > 0, "the escalation record came after the call");
    // The second backend was asked the same question.
    let seen: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(w.dir.parent().unwrap().join("request.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(seen["backend"], "second");
    assert!(seen["user"].as_str().unwrap().contains("deploy"), "{seen}");
}

#[test]
fn an_oversized_reply_escalates() {
    let mut w = world(Setup {
        primary: "head -c 300000 /dev/zero | tr '\\0' x",
        ..Setup::default()
    });
    w.kernel.cycle("shop");
    assert!(w.called(), "{}", w.lines());
    let esc = w.escalation().expect("an escalation");
    let Record::Escalation { why, claim, .. } = &esc.record else {
        unreachable!()
    };
    assert_eq!(*claim, Claim::Measured);
    assert!(why.contains("reply too big"), "{why}");
    assert!(
        w.entries().iter().any(|e| e.seq > esc.seq
            && matches!(
                &e.record,
                Record::Ask {
                    claim: Claim::Inferred,
                    ..
                }
            )),
        "{}",
        w.lines()
    );
}

#[test]
fn without_escalate_configured_there_is_no_escalation() {
    let mut w = world(Setup {
        configured: false,
        ..Setup::default()
    });
    w.kernel.cycle("shop");
    assert!(!w.called(), "{}", w.lines());
    assert!(w.escalation().is_none(), "{}", w.lines());
    assert!(w.report("unusable").is_some(), "{}", w.lines());
    // Asking by hand gives nothing either.
    let request = avsmd::role::InferRequest {
        backend: "primary".into(),
        system: String::new(),
        user: String::new(),
        max_tokens: 10,
    };
    assert!(w
        .kernel
        .escalate("shop", "by hand", &request)
        .unwrap()
        .is_empty());
}

#[test]
fn an_offbox_backend_needs_the_mandate_to_say_so() {
    let mut w = world(Setup {
        offbox: true,
        allowed: false,
        ..Setup::default()
    });
    w.kernel.cycle("shop");
    assert!(!w.called(), "nothing left the machine\n{}", w.lines());
    let refused = w.escalation().expect("a refusal");
    let Record::Escalation { refused, claim, .. } = &refused.record else {
        unreachable!()
    };
    assert_eq!(*claim, Claim::Measured);
    let reason = refused.as_deref().expect("refused");
    assert!(reason.contains("escalate: offbox"), "{reason}");
    assert!(
        !w.entries().iter().any(|e| e.record.kind() == Kind::Answer),
        "no answer to a refusal"
    );

    // The same setup with the mandate's say-so asks the backend.
    let mut w = world(Setup {
        offbox: true,
        allowed: true,
        ..Setup::default()
    });
    w.kernel.cycle("shop");
    assert!(w.called(), "{}", w.lines());
    let asked = w.escalation().expect("an escalation");
    assert!(matches!(
        &asked.record,
        Record::Escalation { refused: None, .. }
    ));
}

#[test]
fn an_answer_is_inferred_and_never_authorizes() {
    let mut w = world(Setup {
        second: DEPLOY,
        offbox: true,
        allowed: true,
        ..Setup::default()
    });
    w.kernel.cycle("shop");
    let entries = w.entries();
    let esc = w.escalation().expect("an escalation");
    let proposal = entries
        .iter()
        .find(|e| matches!(&e.record, Record::Proposal { action, .. } if action == "deploy"))
        .unwrap_or_else(|| panic!("{}", w.lines()));
    assert_eq!(proposal.record.claim(), Claim::Inferred);
    assert_eq!(proposal.record.cites(), &[esc.seq]);
    // Nothing ran: no intent, no action, no file.
    assert!(
        !entries
            .iter()
            .any(|e| matches!(e.record.kind(), Kind::Intent | Kind::Action)
                && e.record.action() == Some("deploy")),
        "{}",
        w.lines()
    );
    assert!(!w.dir.join("deployed").exists());
    // Every record the answer left is inferred.
    assert!(entries
        .iter()
        .filter(|e| e.record.cites().contains(&esc.seq))
        .all(|e| e.record.claim() == Claim::Inferred));
}

#[test]
fn a_second_backend_that_fails_too_leaves_an_unusable_outcome() {
    for second in ["echo banana", "echo no >&2; exit 1"] {
        let mut w = world(Setup {
            second,
            ..Setup::default()
        });
        w.kernel.cycle("shop");
        let entries = w.entries();
        let esc = w.escalation().expect("an escalation");
        let outcome = entries
            .iter()
            .find(
                |e| matches!(&e.record, Record::Answer { escalation, .. } if *escalation == esc.seq),
            )
            .unwrap_or_else(|| panic!("{second}: {}", w.lines()));
        let Record::Answer {
            claim,
            ok,
            detail,
            cites,
            ..
        } = &outcome.record
        else {
            unreachable!()
        };
        assert_eq!(*claim, Claim::Measured);
        assert!(!ok);
        assert_eq!(detail, "unusable", "{second}");
        assert!(cites.is_empty(), "{second}");
        assert!(
            !entries
                .iter()
                .any(|e| matches!(e.record.kind(), Kind::Proposal | Kind::Ask)),
            "{second}: {}",
            w.lines()
        );
    }
}

// ----- a mandate change cannot add escalate: offbox -----

struct Scripted {
    replies: VecDeque<Vec<Record>>,
    _seen: Arc<Mutex<Vec<Context>>>,
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
permitted:\n  - name: announce\n    run: \"cat >> announced\"\n    cap: 9/day\n    reversible: true\n    takes: text\n";

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

#[test]
fn a_ratchet_proposal_cannot_add_escalate_offbox() {
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
        _seen: Arc::new(Mutex::new(vec![])),
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
    k.tick();

    // Self proposed delegating announce; the text the approval carries
    // also adds the right to escalate off the machine.
    let ask = open_ask(&k, "delegate").expect("the ratchet proposed");
    let bad = format!("escalate: offbox\n{SHOP}").replace(
        "reversible: true\n",
        "reversible: true\n    delegated: true\n",
    );
    k.approve(ask, Some(&bad)).unwrap();
    k.tick();
    let entries = k.entries().unwrap();
    let lines: Vec<String> = entries.iter().map(|e| e.line()).collect();
    let refused = entries.iter().any(|e| {
        e.record.kind() == Kind::Report
            && e.record.claim() == Claim::Measured
            && e.record.summary().contains("delegate=?  did not run")
            && e.record.summary().contains("escalate: offbox")
    });
    assert!(refused, "{}", lines.join("\n"));
    assert_eq!(
        std::fs::read_to_string(dir.join("mandate.yaml")).unwrap(),
        SHOP,
        "the mandate was not rewritten"
    );
    assert!(
        !entries.iter().any(|e| matches!(
            &e.record,
            Record::Decision { verdict: avsmd::record::Verdict::Adopt { text: Some(t), .. }, .. }
                if t.contains("offbox")
        )),
        "nothing adopted it"
    );
}
