//! A simulated human that says no: every third ask is denied, and a text
//! payload is edited before it is approved. One test in this file: the
//! virtual clock belongs to the whole process.

use std::os::unix::fs::PermissionsExt;

use avsm::simulate::{run, Human, Plan, BY, EDIT};

/// A world that says when each simulated day begins, and takes the
/// lever runs: a post lands in posts/ as the real lever would write it.
const WORLD: &str =
    "if [ -n \"$AVSM_LEVER\" ]; then cat > posts/$(ls posts | wc -l | tr -d ' ').md; exit; fi; \
     [ $((AVSM_NOW % 86400)) -eq 0 ] && echo \"day $AVSM_DAY begins\"; true";

#[test]
fn the_mixed_human_denies_every_third_ask_and_edits_what_runs() {
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
            "nightly: \"03:00\"\ndefault: k\nbackends:\n  k: {{kind: command, run: {}}}\nratchet: 9\n",
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
    let plan = Plan {
        days: 3,
        step: 3600,
        human: Human::Mixed,
        start: Some(1_767_225_600), // 2026-01-01T00:00:00Z
        add: vec![blog.clone()],
        world: Some(WORLD.to_string()),
    };
    let mut out = Vec::new();
    run(&home, &plan, &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();

    // Three nights, three asks: approved, approved, denied.
    assert_eq!(text.matches("ask       post?").count(), 3, "{text}");
    assert_eq!(text.matches("denied #").count(), 1, "{text}");
    assert!(text.contains(&format!("by {BY}")), "{text}");
    // Both approved runs went to the world, never to the lever.
    assert_eq!(text.matches(" done  simulated: world").count(), 2, "{text}");
    // What ran is what the human saved, not what the model proposed.
    let mut posts: Vec<_> = std::fs::read_dir(blog.join("posts"))
        .unwrap()
        .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap())
        .collect();
    posts.sort();
    assert_eq!(posts.len(), 2, "{text}");
    for p in &posts {
        assert_eq!(p, &format!("A title\nA body\n\n{EDIT}\n"), "{text}");
    }
    // The world moved on simulated time, once a day at midnight.
    assert!(text.contains("  world: day 0 begins\n"), "{text}");
    assert!(text.contains("  world: day 2 begins\n"), "{text}");
    assert_eq!(text.matches("  world: day ").count(), 3, "{text}");
    let k = avsm::kernel::Kernel::open(&home).unwrap();
    assert_eq!(
        k.ledger.verify().unwrap(),
        k.entries().unwrap().len() as u64
    );
    drop(k);

    // A world that fails stops the simulation.
    let broken = Plan {
        start: None,
        world: Some("echo flood >&2; exit 3".to_string()),
        ..plan
    };
    let err = run(&home, &broken, &mut Vec::new()).unwrap_err();
    assert!(
        err.contains("the world failed") && err.contains("flood"),
        "{err}"
    );
}
