//! The planner in the kernel: a branch record, then an inferred proposal
//! citing it, through the gate as a model controller's would go. A branch
//! never authorizes; below calibration the plan asks; an irreversible,
//! undelegated lever asks; branches that cannot be separated go to the
//! backend, and an unusable backend escalates. Critical slowing down asks
//! once.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use avsmd::config::Quotas;
use avsmd::gate::{self, Candidate, Proof, Reason};
use avsmd::kernel::Kernel;
use avsmd::record::{Claim, Entry, Kind, Record, Seq, Signal, MAX_ALTERNATIVES};
use avsmd::role::ModelRole;
use avsmd::warden::LocalWarden;

/// The virtual clock is shared by the whole process: one test at a time.
static CLOCK: Mutex<()> = Mutex::new(());

const T0: i64 = 1_800_000_000;
const HOUR: i64 = 3_600;

fn clock() -> MutexGuard<'static, ()> {
    let guard = CLOCK.lock().unwrap_or_else(|e| e.into_inner());
    avsmd::clock::set(T0);
    guard
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A world command: the metric after one step is 10 with no lever, else
/// what `effects` says for the lever.
fn world_script(effects: &[(&str, f64)]) -> String {
    let mut body = String::from("read -r line\ncase \"$line\" in\n");
    for (lever, x) in effects {
        body.push_str(&format!("  *'\"name\":\"{lever}\"'*) m={x} ;;\n"));
    }
    body.push_str("  *) m=10 ;;\nesac\necho \"{\\\"state\\\": null, \\\"metric\\\": $m}\"");
    body
}

struct Setup {
    /// The levers, as mandate lines under `permitted:`.
    levers: &'static str,
    /// What each lever does to the metric in the world.
    effects: &'static [(&'static str, f64)],
    /// Extra fields of the plan controller.
    controller: &'static str,
    /// Extra mandate lines.
    extra: &'static str,
    /// What the backend does: a shell script body.
    backend: &'static str,
    /// What the escalation backend does, if one is configured.
    second: Option<&'static str>,
    /// Is there a `world:` command, or does the baseline stand in?
    world: bool,
}

const BOOST: &str =
    "- {name: boost, run: touch boosted, cap: 99/day, reversible: true, delegated: true}\n";

impl Default for Setup {
    fn default() -> Setup {
        Setup {
            levers: BOOST,
            effects: &[("boost", 15.0)],
            controller: "",
            extra: "calibration: -1\n",
            backend: "echo nothing",
            second: None,
            world: true,
        }
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    k: Kernel,
    root: PathBuf,
    dir: PathBuf,
    now: i64,
}

fn fixture(s: Setup) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let backend = root.join("backend.sh");
    script(
        &backend,
        &format!(
            "cat > {r}/request.txt\ntouch {r}/called\n{}",
            s.backend,
            r = root.display()
        ),
    );
    let mut config = format!(
        "nightly: never\ndefault: primary\nbackends:\n  primary: {{kind: command, run: {}}}\n",
        backend.display()
    );
    if let Some(second) = s.second {
        let path = root.join("second.sh");
        script(
            &path,
            &format!(
                "cat > /dev/null\ntouch {}/escalated\n{second}",
                root.display()
            ),
        );
        config.push_str(&format!(
            "  second: {{kind: command, run: {}}}\nescalate: second\n",
            path.display()
        ));
    }
    std::fs::write(home.join("config.yaml"), config).unwrap();
    let warden = LocalWarden::open(&home).unwrap();
    let mut k = Kernel::with(&home, Box::new(ModelRole), Box::new(warden)).unwrap();
    let dir = root.join("shop");
    std::fs::create_dir_all(&dir).unwrap();
    script(&dir.join("world"), &world_script(s.effects));
    script(&dir.join("report"), "cat value");
    std::fs::write(
        dir.join("mandate.yaml"),
        format!(
            "purpose: Sell.\nmetric: count\ncontroller: {{kind: plan, horizon: 1, depth: 1, members: 2{}}}\ncadence: 1h\n\
             permitted:\n{}{}{}",
            s.controller,
            s.levers,
            if s.world { "world: ./world\n" } else { "" },
            s.extra
        ),
    )
    .unwrap();
    k.add(&dir).unwrap();
    Fixture {
        _tmp: tmp,
        k,
        root,
        dir,
        now: T0,
    }
}

impl Fixture {
    fn report(&mut self, value: f64) {
        self.now += HOUR;
        avsmd::clock::set(self.now);
        std::fs::write(self.dir.join("value"), format!("{value}\n")).unwrap();
        self.k.report("shop", false);
    }

    /// Five measured scores: enough to trust the world model when the bar
    /// is -1.
    fn calibrate(&mut self) {
        for _ in 0..5 {
            self.k
                .append(Record::report(
                    Claim::Measured,
                    "shop",
                    "calibration.world",
                    Some(0.5),
                    "",
                ))
                .unwrap();
        }
        assert!(self.k.trusted("shop"));
    }

    fn entries(&self) -> Vec<Entry> {
        self.k.entries().unwrap()
    }

    fn of(&self, kind: Kind) -> Vec<Entry> {
        self.entries()
            .into_iter()
            .filter(|e| e.record.kind() == kind && e.record.venture() == "shop")
            .collect()
    }

    fn lines(&self) -> String {
        self.entries()
            .iter()
            .map(Entry::line)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn gate(&self, action: &str, cites: &[Seq]) -> Result<gate::Proof, gate::Refusal> {
        gate::gate_in(
            &self.k.index().unwrap(),
            &Candidate {
                venture: "shop".to_string(),
                action: action.to_string(),
                cites: cites.to_vec(),
            },
            &self.k.adopted(),
            &Quotas::default(),
            self.now,
        )
    }
}

#[test]
fn a_trusted_plan_writes_a_branch_then_a_proposal_citing_it_and_runs() {
    let _c = clock();
    let mut f = fixture(Setup::default());
    f.calibrate();
    f.report(10.0);
    let branches = f.of(Kind::Branch);
    assert_eq!(branches.len(), 1, "{}", f.lines());
    let b = &branches[0];
    assert_eq!(b.record.claim(), Claim::Inferred);
    let proposals = f.of(Kind::Proposal);
    assert_eq!(proposals.len(), 1, "{}", f.lines());
    match &proposals[0].record {
        Record::Proposal {
            claim,
            action,
            cites,
            reason,
            ..
        } => {
            assert_eq!(*claim, Claim::Inferred);
            assert_eq!(action, "boost");
            assert_eq!(cites, &vec![b.seq]);
            assert!(reason.contains("ranks it first"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    assert!(proposals[0].seq > b.seq);
    // Reversible and delegated, from a trusted model: it runs.
    assert_eq!(f.of(Kind::Intent).len(), 1, "{}", f.lines());
    assert!(f.dir.join("boosted").exists());
    // The backend was not asked: the branches were separated.
    assert!(!f.root.join("called").exists());
}

#[test]
fn a_branch_never_authorizes_even_cited_alone() {
    let _c = clock();
    let mut f = fixture(Setup::default());
    f.report(10.0);
    let b = f.of(Kind::Branch).pop().expect("a branch");
    // boost is reversible and delegated: a proposal would let it run.
    let refusal = f.gate("boost", &[b.seq]).unwrap_err();
    assert!(
        refusal
            .reasons
            .iter()
            .any(|r| matches!(r, Reason::NotJustified { .. })),
        "{refusal}"
    );
    assert!(
        refusal
            .reasons
            .iter()
            .any(|r| matches!(r, Reason::OnlyInferred { seqs } if *seqs == vec![b.seq])),
        "{refusal}"
    );
    // Not even a branch a hand claimed was measured.
    let mut forged = b.record.clone();
    if let Record::Branch { claim, .. } = &mut forged {
        *claim = Claim::Measured;
    }
    let forged = f.k.append(forged).unwrap();
    let refusal = f.gate("boost", &[forged.seq]).unwrap_err();
    assert!(
        refusal
            .reasons
            .iter()
            .any(|r| matches!(r, Reason::NotJustified { .. })),
        "{refusal}"
    );
    let why = f.k.why(b.seq).unwrap();
    assert!(why.contains("authorizes nothing"), "{why}");
}

#[test]
fn below_calibration_the_plan_asks() {
    let _c = clock();
    let mut f = fixture(Setup {
        extra: "calibration: 0.5\n",
        ..Setup::default()
    });
    f.report(10.0);
    assert!(!f.k.trusted("shop"));
    assert_eq!(f.of(Kind::Branch).len(), 1);
    assert_eq!(f.of(Kind::Proposal).len(), 1);
    assert!(f.of(Kind::Intent).is_empty(), "{}", f.lines());
    assert!(!f.dir.join("boosted").exists());
    let asks = f.of(Kind::Ask);
    assert_eq!(asks.len(), 1, "{}", f.lines());
    assert!(
        matches!(&asks[0].record, Record::Ask { action: Some(a), question, .. }
            if a == "boost" && question.contains("under the bar of 0.5")),
        "{asks:?}"
    );
}

#[test]
fn an_irreversible_undelegated_lever_the_plan_chooses_asks() {
    let _c = clock();
    let mut f = fixture(Setup {
        levers:
            "- {name: boost, run: touch boosted, cap: 99/day, reversible: true, delegated: true}\n\
                 - {name: deploy, run: touch deployed, cap: 9/day, reversible: false}\n",
        effects: &[("boost", 15.0), ("deploy", 20.0)],
        ..Setup::default()
    });
    f.calibrate();
    f.report(10.0);
    let b = f.of(Kind::Branch).pop().unwrap();
    assert!(
        matches!(&b.record, Record::Branch { chosen, separated: true, .. } if chosen.action == "deploy"),
        "{b:?}"
    );
    assert!(f.of(Kind::Intent).is_empty(), "{}", f.lines());
    assert!(!f.dir.join("deployed").exists());
    let asks = f.of(Kind::Ask);
    assert_eq!(asks.len(), 1, "{}", f.lines());
    assert!(
        matches!(&asks[0].record, Record::Ask { action: Some(a), question, refusal: Some(_), .. }
            if a == "deploy" && question.contains("irreversible")),
        "{asks:?}"
    );
}

const TIED: &[(&str, f64)] = &[("boost", 15.0), ("tidy", 15.0)];
const TWO: &str = "- {name: boost, run: touch boosted, cap: 99/day, reversible: true, delegated: true}\n\
                   - {name: tidy, run: touch tidied, cap: 99/day, reversible: true, delegated: true}\n";

#[test]
fn branches_that_cannot_be_separated_go_to_the_backend() {
    let _c = clock();
    let mut f = fixture(Setup {
        levers: TWO,
        effects: TIED,
        backend: r#"echo '{"choice":"action","action":"tidy","reason":"it is cheaper"}'"#,
        ..Setup::default()
    });
    f.calibrate();
    f.report(10.0);
    let b = f.of(Kind::Branch).pop().unwrap();
    assert!(
        matches!(
            &b.record,
            Record::Branch {
                separated: false,
                ..
            }
        ),
        "{b:?}"
    );
    assert!(f.root.join("called").exists(), "{}", f.lines());
    let request = std::fs::read_to_string(f.root.join("request.txt")).unwrap();
    assert!(
        request.contains("boost") && request.contains("tidy"),
        "{request}"
    );
    let p = f.of(Kind::Proposal).pop().unwrap();
    assert!(
        matches!(&p.record, Record::Proposal { action, cites, reason, .. }
            if action == "tidy" && *cites == vec![b.seq] && reason.contains("it is cheaper")),
        "{p:?}"
    );
    assert!(f.dir.join("tidied").exists(), "{}", f.lines());
    assert!(f.of(Kind::Escalation).is_empty());
}

#[test]
fn an_unusable_backend_on_a_tie_escalates() {
    let _c = clock();
    let mut f = fixture(Setup {
        levers: TWO,
        effects: TIED,
        backend: "echo banana",
        second: Some(r#"echo '{"choice":"ask","question":"Which one?"}'"#),
        ..Setup::default()
    });
    f.calibrate();
    f.report(10.0);
    assert!(f.root.join("called").exists());
    let unusable = f
        .entries()
        .into_iter()
        .find(|e| matches!(&e.record, Record::Report { metric, .. } if metric == "unusable"))
        .expect("an unusable report");
    let escalation = f.of(Kind::Escalation).pop().expect("an escalation");
    assert!(escalation.seq > unusable.seq);
    assert!(f.root.join("escalated").exists(), "{}", f.lines());
    // Nothing ran.
    assert!(f.of(Kind::Intent).is_empty(), "{}", f.lines());
    // A backend that chooses what was not offered is unusable as well.
    let mut g = fixture(Setup {
        levers: TWO,
        effects: TIED,
        backend: r#"echo '{"choice":"action","action":"boost","reason":"x"}' | sed s/boost/stop/"#,
        second: Some("echo nothing"),
        ..Setup::default()
    });
    g.calibrate();
    g.report(10.0);
    assert!(!g.of(Kind::Escalation).is_empty(), "{}", g.lines());
}

#[test]
fn a_branch_has_the_chosen_the_top_three_alternatives_and_propensities() {
    let _c = clock();
    let mut f = fixture(Setup {
        levers: "- {name: a, run: touch a, cap: 9/day, reversible: true, delegated: true}\n\
                 - {name: b, run: touch b, cap: 9/day, reversible: true, delegated: true}\n\
                 - {name: c, run: touch c, cap: 9/day, reversible: true, delegated: true}\n\
                 - {name: d, run: touch d, cap: 9/day, reversible: true, delegated: true}\n",
        effects: &[("a", 30.0), ("b", 20.0), ("c", 15.0), ("d", 12.0)],
        ..Setup::default()
    });
    f.report(10.0);
    let b = f.of(Kind::Branch).pop().unwrap();
    let Record::Branch {
        chosen,
        alternatives,
        members,
        horizon,
        separated,
        ..
    } = &b.record
    else {
        panic!("{b:?}")
    };
    assert_eq!(chosen.action, "a");
    assert_eq!(chosen.quantiles.p50, 30.0);
    assert_eq!(chosen.worst, 30.0);
    assert_eq!(chosen.propensity, 1.0);
    assert_eq!(alternatives.len(), MAX_ALTERNATIVES);
    let names: Vec<&str> = alternatives.iter().map(|a| a.action.as_str()).collect();
    assert_eq!(names, ["b", "c", "d"]);
    for (a, x) in alternatives.iter().zip([20.0, 15.0, 12.0]) {
        assert!(a.quantiles.valid());
        assert_eq!(a.quantiles.p50, x);
        assert_eq!(a.worst, x);
        assert_eq!(a.propensity, 0.0);
    }
    assert_eq!((*members, *horizon, *separated), (2, 1, true));
    // Size-bounded: four levers and nothing weighed, three alternatives kept.
    assert!(
        b.record.canonical().len() < 2048,
        "{}",
        b.record.canonical()
    );
}

#[test]
fn the_same_seed_gives_the_same_branch_record() {
    use avsmd::mandate::Worse;
    use avsmd::plan::{self, Lever, Problem};
    use avsmd::world::{Baseline, Drift};
    use std::collections::BTreeMap;
    let mut world = Baseline {
        effects: BTreeMap::from([
            ("boost".to_string(), vec![4.0, 6.0, 5.0, 3.0]),
            ("tidy".to_string(), vec![1.0, 9.0, 2.0, 8.0]),
        ]),
        last: Some(10.0),
        period: 3600,
        ..Baseline::default()
    };
    world.set_drift(Drift::every(3600, &[0.5, -0.3, 0.2, -0.4, 0.1, 0.3]));
    let lever = |name: &str| Lever {
        name: name.to_string(),
        reversible: true,
        explore: true,
        asks: false,
        left: 9,
    };
    let branch = |base: u64| {
        let ms = plan::members(&world, base, 8);
        let found = plan::search(
            &ms,
            &Problem {
                state: serde_json::Value::Null,
                now: T0,
                dt: HOUR,
                horizon: 4,
                depth: 2,
                better: Worse::Up,
                guards: &[],
                levers: vec![lever("boost"), lever("tidy")],
                seed: avsmd::world::mix(base, 1),
            },
        )
        .unwrap();
        avsmd::kernel::plan::branch_record("shop", &found, 8, 4, Worse::Up)
    };
    assert_eq!(branch(7), branch(7));
    assert_eq!(branch(7).canonical(), branch(7).canonical());
    assert_ne!(branch(7), branch(8));
}

// ----- critical slowing down -----

fn splitmix64(state: &mut u64) -> u64 {
    avsmd::world::splitmix64(state)
}

fn normal(rng: &mut u64) -> f64 {
    let u = |rng: &mut u64| ((splitmix64(rng) >> 11) as f64 + 0.5) / (1u64 << 53) as f64;
    let (a, b) = (u(rng), u(rng));
    (-2.0 * a.ln()).sqrt() * (std::f64::consts::TAU * b).cos()
}

/// AR(1), its coefficient ramping 0.2 to 0.95, its shocks 1 to 3.
fn ramp(n: usize, seed: u64) -> Vec<f64> {
    let mut rng = seed;
    let mut x = 0.0;
    (0..n)
        .map(|i| {
            let t = i as f64 / (n - 1) as f64;
            x = (0.2 + 0.75 * t) * x + (1.0 + 2.0 * t) * normal(&mut rng);
            100.0 + x
        })
        .collect()
}

fn white(n: usize, seed: u64) -> Vec<f64> {
    let mut rng = seed;
    (0..n).map(|_| 100.0 + normal(&mut rng)).collect()
}

/// A model-controlled venture with `early_warning: true`, fed `series`.
fn warned(series: &[f64]) -> Vec<Entry> {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.yaml"),
        "nightly: never\ndefault: k\nbackends:\n  k: {kind: command, run: echo nothing}\n",
    )
    .unwrap();
    let warden = LocalWarden::open(&home).unwrap();
    let mut k = Kernel::with(&home, Box::new(ModelRole), Box::new(warden)).unwrap();
    let dir = tmp.path().join("shop");
    std::fs::create_dir_all(&dir).unwrap();
    script(&dir.join("report"), "cat value");
    std::fs::write(
        dir.join("mandate.yaml"),
        "purpose: Sell.\nmetric: count\ncontroller: {kind: model}\ncadence: 1h\nearly_warning: true\n",
    )
    .unwrap();
    k.add(&dir).unwrap();
    let mut now = T0;
    for x in series {
        now += HOUR;
        avsmd::clock::set(now);
        std::fs::write(dir.join("value"), format!("{x}\n")).unwrap();
        k.report("shop", false);
    }
    k.entries()
        .unwrap()
        .into_iter()
        .filter(|e| {
            matches!(
                &e.record,
                Record::Ask {
                    signal: Some(Signal::Warning { .. }),
                    ..
                }
            )
        })
        .collect()
}

#[test]
fn a_ramping_ar1_asks_one_warning_and_white_noise_none() {
    let _c = clock();
    let asks = warned(&ramp(60, 2));
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert!(
        matches!(&asks[0].record, Record::Ask { claim: Claim::Measured, action: None, signal: Some(Signal::Warning { metric, variance_ratio, autocorrelation_rise }), .. }
            if metric == "count" && *variance_ratio > 1.5 && *autocorrelation_rise > 0.2),
        "{asks:?}"
    );
    assert!(warned(&white(60, 2)).is_empty());
}

#[test]
fn an_escalated_answer_below_calibration_asks_and_does_not_run() {
    let _c = clock();
    let mut f = fixture(Setup {
        levers: TWO,
        effects: TIED,
        extra: "calibration: 0.5\n",
        backend: "echo banana",
        second: Some(r#"echo '{"choice":"action","action":"tidy","reason":"it is cheaper"}'"#),
        ..Setup::default()
    });
    f.report(10.0);
    assert!(!f.k.trusted("shop"));
    let escalation = f.of(Kind::Escalation).pop().expect("an escalation");
    assert!(f.root.join("escalated").exists());
    // The answer proposes tidy, citing the escalation.
    let answer = f
        .of(Kind::Proposal)
        .into_iter()
        .find(|e| e.record.cites() == [escalation.seq])
        .expect("the answer's proposal");
    assert_eq!(answer.record.action(), Some("tidy"));
    // Reversible and delegated, so the gate alone would run it; the model
    // is below its bar, so it asks.
    assert!(f.gate("tidy", &[answer.seq]).is_ok());
    assert!(f.of(Kind::Intent).is_empty(), "{}", f.lines());
    assert!(!f.dir.join("tidied").exists());
    let asks = f.of(Kind::Ask);
    assert_eq!(asks.len(), 1, "{}", f.lines());
    assert!(
        matches!(&asks[0].record, Record::Ask { action: Some(a), question, .. }
            if a == "tidy" && question.contains("under the bar of 0.5")),
        "{asks:?}"
    );
}

#[test]
fn a_backend_choosing_what_the_plan_did_not_offer_is_unusable_and_escalates() {
    let _c = clock();
    // dump is permitted, but worse than nothing: not in the top three.
    let mut f = fixture(Setup {
        levers: "- {name: boost, run: touch boosted, cap: 99/day, reversible: true, delegated: true}\n\
                 - {name: tidy, run: touch tidied, cap: 99/day, reversible: true, delegated: true}\n\
                 - {name: dump, run: touch dumped, cap: 99/day, reversible: true, delegated: true}\n",
        effects: &[("boost", 15.0), ("tidy", 15.0), ("dump", 5.0)],
        backend: r#"echo '{"choice":"action","action":"dump","reason":"why not"}'"#,
        second: Some("echo nothing"),
        ..Setup::default()
    });
    f.calibrate();
    f.report(10.0);
    let b = f.of(Kind::Branch).pop().unwrap();
    let Record::Branch {
        chosen,
        alternatives,
        separated: false,
        ..
    } = &b.record
    else {
        panic!("{b:?}")
    };
    let top: Vec<&str> = std::iter::once(chosen)
        .chain(alternatives.iter())
        .take(3)
        .map(|a| a.action.as_str())
        .collect();
    assert!(!top.contains(&"dump"), "{top:?}");
    let request = std::fs::read_to_string(f.root.join("request.txt")).unwrap();
    assert!(!request.contains("dump"), "{request}");
    let unusable = f
        .entries()
        .into_iter()
        .find_map(|e| match e.record {
            Record::Report {
                metric,
                note,
                cites,
                ..
            } if metric == "unusable" => Some((note, cites)),
            _ => None,
        })
        .expect("an unusable report");
    assert!(
        unusable
            .0
            .contains("it chose dump, which the planner did not offer"),
        "{unusable:?}"
    );
    assert_eq!(unusable.1, vec![b.seq]);
    assert!(!f.of(Kind::Escalation).is_empty(), "{}", f.lines());
    assert!(f.of(Kind::Proposal).is_empty(), "{}", f.lines());
    assert!(f.of(Kind::Intent).is_empty());
    assert!(!f.dir.join("dumped").exists());
}

/// A baseline venture with a history: three rounds of dump, which drops
/// the metric 40, then boost, which brings it back.
fn drawdown_fixture(guards: &'static str) -> Fixture {
    let mut f = fixture(Setup {
        levers: "- {name: boost, run: touch boosted, cap: 99/day, reversible: true, delegated: true}\n\
                 - {name: dump, run: touch dumped, cap: 99/day, reversible: true, delegated: true}\n",
        world: false,
        extra: guards,
        ..Setup::default()
    });
    let mut at = f.now;
    let mut push = |f: &mut Fixture, r: Record| {
        at += HOUR;
        avsmd::clock::set(at);
        f.k.append(r).unwrap()
    };
    let run =
        |f: &mut Fixture, push: &mut dyn FnMut(&mut Fixture, Record) -> Entry, lever: &str| {
            let intent = push(
                f,
                Record::Intent {
                    claim: Claim::Measured,
                    venture: "shop".to_string(),
                    action: lever.to_string(),
                    proof: Proof {
                        venture: "shop".to_string(),
                        action: lever.to_string(),
                        steps: vec![],
                    },
                    payload: None,
                    key: None,
                },
            );
            push(
                f,
                Record::Outcome {
                    claim: Claim::Measured,
                    venture: "shop".to_string(),
                    intent: intent.seq,
                    ok: true,
                    detail: String::new(),
                },
            );
        };
    let report = |x: f64| Record::report(Claim::Measured, "shop", "count", Some(x), "");
    for _ in 0..3 {
        push(&mut f, report(100.0));
        push(&mut f, report(100.2));
        run(&mut f, &mut push, "dump");
        push(&mut f, report(60.2));
        run(&mut f, &mut push, "boost");
        push(&mut f, report(100.2));
        push(&mut f, report(100.0));
    }
    f.now = at;
    f
}

fn weighed(f: &Fixture) -> Vec<String> {
    let b = f.of(Kind::Branch).pop().expect("a branch");
    let Record::Branch {
        chosen,
        alternatives,
        ..
    } = &b.record
    else {
        panic!("{b:?}")
    };
    std::iter::once(chosen)
        .chain(alternatives.iter())
        .map(|a| a.action.clone())
        .collect()
}

#[test]
fn a_baseline_drawdown_guard_vetoes_a_plan_sequence() {
    let _c = clock();
    // Without the guard, dump is weighed: the baseline knows it drops 40.
    let mut f = drawdown_fixture("calibration: -1\n");
    f.report(100.0);
    let all = weighed(&f);
    assert!(all.contains(&"dump".to_string()), "{all:?}\n{}", f.lines());
    // With a 10% drawdown guard, every member of dump's rollout breaches
    // it: dump is vetoed and never reaches the branch.
    let mut g = drawdown_fixture(
        "calibration: -1\nguards:\n- {name: dd, drawdown: 10%, of: count, then: ask}\n",
    );
    g.report(100.0);
    let kept = weighed(&g);
    assert!(
        !kept.contains(&"dump".to_string()),
        "{kept:?}\n{}",
        g.lines()
    );
    assert!(kept.contains(&"boost".to_string()), "{kept:?}");
    assert!(kept.contains(&"nothing".to_string()), "{kept:?}");
    assert!(!g.dir.join("dumped").exists());
}
