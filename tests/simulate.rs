//! Simulated days with a stub backend: the clock jumps, nights pass, the
//! scripted human approves, and the ratchet ends the asks. One test in
//! this file: the virtual clock belongs to the whole process.

use std::os::unix::fs::PermissionsExt;

use avsm::simulate::{run, Human, Plan, BY};

#[test]
fn four_days_in_moments_end_with_the_lever_delegated() {
    // Nightly is local time; pin local to UTC so the nights fall at 03:00Z.
    std::env::set_var("TZ", "UTC");
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let backend = tmp.path().join("backend");
    std::fs::write(
        &backend,
        "#!/bin/sh\ncat > /dev/null\nprintf '%s\\n' '{\"choice\":\"action\",\"action\":\"post\",\"reason\":\"one a day\",\"payload\":\"A title\\nA body\"}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&backend, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        home.join("config.yaml"),
        format!(
            "nightly: \"03:00\"\ndefault: k\nbackends:\n  k: {{kind: command, run: {}}}\nratchet: 2\n",
            backend.display()
        ),
    )
    .unwrap();
    let blog = tmp.path().join("blog");
    std::fs::create_dir_all(blog.join("posts")).unwrap();
    std::fs::write(
        blog.join("mandate.yaml"),
        "purpose: A post a day.\nmetric: shell ls posts | wc -l\ncontroller: {kind: model}\ncadence: 1h\n\
         permitted:\n- {name: post, run: \"cat > posts/$(ls posts | wc -l | tr -d ' ').md\", cap: 2/day, reversible: true, takes: text}\n",
    )
    .unwrap();
    let start = 1_767_225_600; // 2026-01-01T00:00:00Z
    let plan = Plan {
        days: 4,
        step: 3600,
        human: Human::ApproveAll,
        start: Some(start),
        add: vec![blog.clone()],
    };
    let began = std::time::Instant::now();
    let mut out = Vec::new();
    run(&home, &plan, &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(began.elapsed().as_secs() < 30, "days pass in moments");

    // Time was virtual: every record is dated inside the four days.
    let k = avsm::kernel::Kernel::open(&home).unwrap();
    let entries = k.entries().unwrap();
    assert!(
        entries
            .iter()
            .all(|e| e.at >= start && e.at < start + 4 * 86_400 + 3600),
        "{text}"
    );
    assert_eq!(k.ledger.verify().unwrap(), entries.len() as u64);
    let lines: Vec<&str> = text.lines().collect();

    // Nights 1 and 2: a post asks, the human approves, it runs.
    assert!(
        text.contains(
            "blog         inferred  proposal  post: one a day  [text \"A title\" (2 lines)]"
        ),
        "{text}"
    );
    assert!(text.contains("post? may I post with this?"), "{text}");
    assert!(
        text.contains("approved #") && text.contains(&format!("by {BY}")),
        "{text}"
    );
    // After two approvals self proposes delegating, the human approves,
    // the mandate is rewritten and adopted.
    assert!(
        text.contains("delegate: post was approved 2 times with a text payload"),
        "{text}"
    );
    assert!(
        text.contains("delegate: done  mandate rewritten; delegated: post"),
        "{text}"
    );
    let mandate = std::fs::read_to_string(blog.join("mandate.yaml")).unwrap();
    assert!(mandate.contains("delegated: true"), "{mandate}");
    // From then on the post runs without an ask: the asks have ended.
    let adopted_at = lines
        .iter()
        .position(|l| l.contains("delegate: done"))
        .unwrap();
    let later = &lines[adopted_at..];
    assert!(
        later
            .iter()
            .any(|l| l.contains("measured  action    post: done")),
        "{text}"
    );
    assert!(
        !later.iter().any(|l| l.contains("ask       post?")),
        "{text}"
    );
    // Four mornings, each with a digest.
    assert_eq!(text.matches("=== morning of ").count(), 4, "{text}");
    assert_eq!(
        std::fs::read_dir(blog.join("posts")).unwrap().count(),
        4,
        "{text}"
    );

    // It refuses the real home and a ledger it would overlap.
    let again = run(
        &home,
        &Plan {
            start: Some(start),
            ..plan.clone()
        },
        &mut Vec::new(),
    );
    assert!(again.unwrap_err().contains("already reaches"));
}
