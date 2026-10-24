//! A world can give an audit's or a reading's lines (`name: a@b c@d`), so a
//! payload check that reads the audit holds in a simulation, and the audit
//! program itself never runs.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use avsmd::simulate::{run, Human, Plan};

fn script(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn a_world_that_gives_an_audits_lines_lets_a_check_pass_a_listed_address_only() {
    std::env::set_var("TZ", "UTC");
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let t = tmp.path();
    // A model that replies to a listed sender, then to a stranger, in turn.
    script(
        t,
        "backend",
        &"cat > /dev/null; n=$(cat {T}/turn 2>/dev/null || echo 0); echo $((n + 1)) > {T}/turn\n\
         if [ $((n % 2)) -eq 0 ]; then to=alice@example.com; else to=mallory@evil.example; fi\n\
         printf '{\"choice\":\"action\",\"action\":\"reply\",\"reason\":\"answer\",\"payload\":\"%s\\\\nHello\"}\\n' \"$to\"".replace("{T}", &t.display().to_string()),
    );
    std::fs::write(
        home.join("config.yaml"),
        format!(
            "nightly: \"03:00\"\ndefault: k\nbackends:\n  k: {{kind: command, run: {}}}\nratchet: 9\n",
            t.join("backend").display()
        ),
    )
    .unwrap();
    let dir = t.join("inbox");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mandate.yaml"),
        "purpose: Answer the customers.\nmetric: unread\ncontroller: {kind: model}\ncadence: 1h\n\
         audits:\n  - {name: senders, run: ./senders, worse: up, better: down, tolerance: 3}\n\
         permitted:\n- {name: reply, run: ./reply, cap: 6/day, reversible: true, takes: text, check: ./check}\n",
    )
    .unwrap();
    // The audit program and the report are tripwires: they must never run.
    script(&dir, "senders", "echo ran >> tripwire; echo 2");
    script(&dir, "report", "echo ran >> tripwire; echo 2");
    script(&dir, "reply", "echo ran >> tripwire");
    // The check passes an address a measured report names as `from <it>`.
    script(
        &dir,
        "check",
        "want=$(head -n 1 | tr 'A-Z' 'a-z')\n\
         grep -qi \"from $want\" \"$AVSM_MEASURED\"",
    );
    let log = t.join("world.log");
    let world = format!(
        "if [ -n \"$AVSM_LEVER\" ]; then cat >> {log}; exit 0; fi; \
         echo inbox/unread=2; echo inbox/audit.senders=1; \
         echo 'inbox/audit.senders: alice@example.com not-an-address bad,one@x.y'",
        log = log.display()
    );
    let plan = Plan {
        days: 3,
        step: 3600,
        human: Human::ApproveAll,
        start: Some(1_767_225_600),
        add: vec![dir.clone()],
        world: Some(world),
    };
    let mut out = Vec::new();
    run(&home, &plan, &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();

    assert!(
        !dir.join("tripwire").exists(),
        "a venture program ran: {:?}\n{text}",
        std::fs::read_to_string(dir.join("tripwire"))
    );
    let sent = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(sent.contains("alice@example.com"), "{sent}\n{text}");
    // What is no address is stripped from the audit's note.
    assert!(!text.contains("from not-an-address"), "{text}");
    assert!(!text.contains("from bad,one"), "{text}");
    assert!(!sent.contains("mallory"), "{sent}\n{text}");
    assert!(
        std::fs::read_to_string(t.join("turn")).unwrap().trim() != "1",
        "the model was asked about the stranger too: {text}"
    );
}
