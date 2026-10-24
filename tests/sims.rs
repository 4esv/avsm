//! The hostile kit in sims/hostile: it makes a world, three days of it run
//! under the scripted model, and analyze.py reads the result into JSON.
//! The kit is Python; without python3 these tests say so and pass.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn kit() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("sims/hostile")
}

/// Is python3 here? If not, say why the test is skipped.
fn python(test: &str) -> bool {
    let ok = Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("skipping {test}: python3 is not on PATH, and the hostile kit is Python");
    }
    ok
}

fn run(cmd: &mut Command, stdin: &str) -> Output {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "exit {:?}\nstdout:\n{}\nstderr:\n{}",
        o.status.code(),
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

/// The top-level keys analyze.py promises: the 1.0 kit's, then 2.0's.
const KEYS: &[&str] = &[
    "days",
    "records",
    "chain",
    "tomato",
    "book",
    "inbox",
    "breaches",
    "asks",
    "approved",
    "denied",
    "delegations_proposed",
    "delegations_adopted",
    "unusable",
    "cycle_failed",
    "did_not_run",
    "measured_none",
    "refusals",
    "refusals_by_reason",
    "actions_done",
    "parent_actions",
    "events",
    "settled",
    "signals",
    "escalations",
    "branches",
    "forecasts",
    "predictions",
    "calibration",
    "states",
    "budgets",
    "implausible",
    "replies",
    "poison",
    "checks",
];

#[test]
fn three_scripted_days_of_the_hostile_kit_analyze_to_json() {
    if !python("three_scripted_days_of_the_hostile_kit_analyze_to_json") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("run");
    let world = dir.join("world");
    let model = format!(
        "python3 -I {} {}",
        kit().join("model.py").display(),
        world.display()
    );
    let made = run(
        Command::new("sh")
            .arg(kit().join("make.sh"))
            .arg(&dir)
            .arg(&model),
        "",
    );
    assert!(made.status.success(), "{}", text(&made));

    let sim = run(
        Command::new(env!("CARGO_BIN_EXE_avsm"))
            .current_dir(&dir)
            .env("TZ", "UTC")
            .args(["--home", "home", "simulate", "--days", "3", "--step", "2h"])
            .args(["--approve", "mixed", "--start", "1792281600", "--world"])
            .arg(format!(
                "python3 -I {} {}",
                kit().join("world.py").display(),
                world.display()
            ))
            .args([
                "ventures/garden",
                "ventures/tomato",
                "ventures/trader",
                "ventures/inbox",
            ]),
        "",
    );
    assert!(sim.status.success(), "{}", text(&sim));

    let analyzed = run(
        Command::new("python3")
            .arg("-I")
            .arg(kit().join("analyze.py"))
            .arg(&dir),
        "",
    );
    assert!(analyzed.status.success(), "{}", text(&analyzed));
    let out: serde_json::Value = serde_json::from_slice(&analyzed.stdout)
        .unwrap_or_else(|e| panic!("analyze.py printed no JSON ({e}):\n{}", text(&analyzed)));
    for key in KEYS {
        assert!(out.get(key).is_some(), "no {key:?} in {out:#}");
    }
    assert_eq!(out["days"], 3, "{out:#}");
    assert_eq!(out["chain"]["ok"], true, "{out:#}");
    assert!(out["records"].as_u64().unwrap() > 100, "{out:#}");
    // The levers go to the world, and the readings come from it.
    assert!(
        out["actions_done"]["tomato/water"].as_u64().unwrap_or(0) >= 1,
        "{out:#}"
    );
    // NOTE: the sender audit (./customers) no longer runs in a simulation,
    // so no reply passes its check here: an audit's addresses are lines of
    // its program, and the world prints numbers only.
}

/// A world with mail in its inbox: world.py has taken one step.
fn world_with_mail(tmp: &Path, ids: &[u32]) -> PathBuf {
    let world = tmp.join("world");
    std::fs::create_dir_all(world.join("inbox")).unwrap();
    let step = run(
        Command::new("python3")
            .arg("-I")
            .arg(kit().join("world.py"))
            .arg(&world)
            .env("AVSM_NOW", "1792281600")
            .env("AVSM_DAY", "0"),
        "",
    );
    assert!(step.status.success(), "{}", text(&step));
    for id in ids {
        let mail = serde_json::json!({"id": id, "from": format!("customer{id}@example.com"),
            "subject": "Question", "body": "When does it ship?"});
        std::fs::write(world.join(format!("inbox/{id:05}.json")), mail.to_string()).unwrap();
    }
    world
}

#[test]
fn archive_takes_the_mail_its_payload_names() {
    if !python("archive_takes_the_mail_its_payload_names") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let world = world_with_mail(tmp.path(), &[7, 79550]);
    // Digits elsewhere in the payload are not the mail's number.
    let ran = run(
        Command::new("python3")
            .arg("-I")
            .arg(kit().join("world.py"))
            .arg(&world)
            .env("AVSM_NOW", "1792285200")
            .env("AVSM_LEVER", "inbox/archive")
            .env("AVSM_KEY", "k"),
        "mail 7: spam, sent 95500 seconds ago (00042.json)\n",
    );
    assert!(ran.status.success(), "{}", text(&ran));
    assert!(world.join("archive/00007.json").exists(), "{}", text(&ran));
    assert!(world.join("inbox/79550.json").exists(), "{}", text(&ran));
}

#[test]
fn research_reads_every_mail_in_the_inbox() {
    if !python("research_reads_every_mail_in_the_inbox") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let ids: Vec<u32> = (100..130).collect();
    let world = world_with_mail(tmp.path(), &ids);
    let mails = std::fs::read_dir(world.join("inbox")).unwrap().count();
    assert!(mails > 20, "{mails}");
    let ran = run(
        Command::new("python3")
            .arg("-I")
            .arg(kit().join("research.py"))
            .arg(&world),
        "{\"venture\": \"inbox\", \"question\": \"What is in the inbox?\"}",
    );
    assert!(ran.status.success(), "{}", text(&ran));
    let out: serde_json::Value = serde_json::from_slice(&ran.stdout).unwrap();
    let results = out["results"].as_array().unwrap();
    assert_eq!(results.len(), mails, "{out:#}");
    assert!(
        results
            .iter()
            .any(|r| r["text"].as_str().unwrap().starts_with("mail 129 ")),
        "the newest mail too: {out:#}"
    );
}
