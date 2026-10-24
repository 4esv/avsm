//! With `--world`, no venture program runs either: the report is never
//! read for real, and the reading is what the world printed.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn script(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A venture whose report counts its runs in ./count and leaves a line in
/// ./tripwire; a world that prints `world_says` (or nothing).
fn venture(tmp: &Path, world_says: &str) -> (PathBuf, PathBuf) {
    let dir = tmp.join("counter");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mandate.yaml"),
        "purpose: Keep it up.\nmetric: count\n\
         controller: {kind: threshold, below: 5, action: touch}\ncadence: 1h\n\
         permitted:\n- {name: touch, run: touch LEVER_RAN, cap: 1/day, reversible: true}\n",
    )
    .unwrap();
    std::fs::write(dir.join("count"), "88\n").unwrap();
    std::fs::write(dir.join("tripwire"), "").unwrap();
    script(
        &dir,
        "report",
        "n=$(cat count); echo $((n+1)) > count; echo \"REPORT cwd=$PWD\" >> tripwire; echo $n",
    );
    script(
        tmp,
        "world.sh",
        &format!(
            "[ -z \"$AVSM_LEVER\" ] || echo \"$AVSM_LEVER\" >> {}/lever.log; {world_says}",
            tmp.display()
        ),
    );
    (dir, tmp.join("world.sh"))
}

fn simulate(tmp: &Path, home: &Path, dir: &Path, world: &Path, env_home: Option<&Path>) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_avsm"));
    c.args(["simulate"])
        .arg(dir)
        .args([
            "--days",
            "1",
            "--step",
            "2h",
            "--approve",
            "all",
            "--start",
            "1767225600",
        ])
        .arg("--home")
        .arg(home)
        .arg("--world")
        .arg(world)
        .env("HOME", tmp)
        .env("TZ", "UTC")
        .env_remove("XDG_STATE_HOME")
        .stdin(Stdio::null());
    match env_home {
        Some(h) => c.env("AVSMD_HOME", h),
        None => c.env_remove("AVSMD_HOME"),
    };
    c.output().unwrap()
}

#[test]
fn with_a_world_the_report_never_runs_and_the_world_gives_the_reading() {
    let tmp = tempfile::tempdir().unwrap();
    let (dir, world) = venture(tmp.path(), "echo count=3");
    let home = tmp.path().join("simhome");
    let out = simulate(tmp.path(), &home, &dir, &world, None);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert_eq!(
        std::fs::read_to_string(dir.join("tripwire")).unwrap(),
        "",
        "{text}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("count")).unwrap(),
        "88\n",
        "{text}"
    );
    assert!(
        text.contains("count=3"),
        "the world's reading is the measured one:\n{text}"
    );
    assert!(text.contains("simulated: world"), "{text}");
    // Levers still go to the world.
    assert!(!dir.join("LEVER_RAN").exists(), "{text}");
    let log = std::fs::read_to_string(tmp.path().join("lever.log")).unwrap();
    assert!(log.contains("counter/touch"), "{log}");
}

#[test]
fn with_a_silent_world_the_report_does_not_run_either() {
    let tmp = tempfile::tempdir().unwrap();
    let (dir, world) = venture(tmp.path(), "true");
    let home = tmp.path().join("simhome");
    let out = simulate(tmp.path(), &home, &dir, &world, None);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{text}");
    assert_eq!(
        std::fs::read_to_string(dir.join("tripwire")).unwrap(),
        "",
        "{text}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("count")).unwrap(),
        "88\n",
        "{text}"
    );
    assert!(text.contains("no reading yet to start from"), "{text}");
}

#[test]
fn a_home_that_is_the_avsmd_home_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let (dir, world) = venture(tmp.path(), "true");
    let home = tmp.path().join("fresh");
    std::fs::create_dir_all(&home).unwrap();
    let out = simulate(tmp.path(), &home, &dir, &world, Some(&home));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{err}");
    let want = format!(
        "{0} is your AVSMD_HOME; pass a --home other than it, or unset AVSMD_HOME",
        home.display()
    );
    assert!(err.contains(&want), "{err}");
}
