//! Text, CLI and doctor papercuts (#54), through the real binaries.

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub fn avsm_out(home: &Path, args: &[&str]) -> (Option<i32>, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_avsm"))
        .arg("--home")
        .arg(home)
        .args(args)
        .output()
        .expect("run avsm");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

pub fn avsm(home: &Path, args: &[&str]) -> (bool, String) {
    let (code, out, err) = avsm_out(home, args);
    (code == Some(0), format!("{out}{err}"))
}

const SMALL_FORECASTS: &str = "forecast: {days: 2, seeds: 2}\n";

pub struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn start(home: &Path) -> Daemon {
    std::fs::create_dir_all(home).unwrap();
    if !home.join("config.yaml").exists() {
        std::fs::write(home.join("config.yaml"), SMALL_FORECASTS).unwrap();
    }
    let child = Command::new(env!("CARGO_BIN_EXE_avsmd"))
        .arg("--home")
        .arg(home)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(home.with_extension("log")).unwrap())
        .spawn()
        .expect("spawn avsmd");
    let sock = avsmd::kernel::socket_path(home);
    let began = Instant::now();
    while !avsmd::protocol::alive(&sock) {
        assert!(
            began.elapsed() < Duration::from_secs(20),
            "avsmd did not start: {}",
            std::fs::read_to_string(home.with_extension("log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    Daemon(child)
}

pub fn counter_venture(parent: &Path) -> PathBuf {
    let dir = parent.join("counter");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mandate.yaml"),
        "purpose: Keep the counter below ten.\n\
         metric: count\n\
         controller:\n  kind: threshold\n  above: 10\n  action: reset\n\
         cadence: 30s\n\
         permitted:\n  - name: reset\n    run: ./reset\n    cap: 3/day\n    reversible: true\n",
    )
    .unwrap();
    std::fs::write(dir.join("report"), "#!/bin/sh\necho 3\n").unwrap();
    std::fs::write(dir.join("reset"), "#!/bin/sh\ntrue\n").unwrap();
    for f in ["report", "reset"] {
        std::fs::set_permissions(dir.join(f), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

#[test]
fn a_closed_pipe_ends_the_client_quietly() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let _daemon = start(&home);
    let dir = counter_venture(tmp.path());
    assert!(avsm(&home, &["add", dir.to_str().unwrap()]).0);
    // A reader that is already gone: every write to stdout is a broken pipe.
    for args in [["tail", "1000"], ["status", ""]] {
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader);
        let mut child = Command::new(env!("CARGO_BIN_EXE_avsm"))
            .arg("--home")
            .arg(&home)
            .args(args.iter().filter(|a| !a.is_empty()))
            .stdout(writer)
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut err = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut err)
            .unwrap();
        let status = child.wait().unwrap();
        assert!(
            !err.contains("panicked") && !err.contains("Broken pipe"),
            "{args:?}: {err}"
        );
        assert_eq!(status.code(), Some(0), "{args:?}: {err}");
    }
}

#[test]
fn text_a_model_wrote_is_escaped_where_it_is_shown() {
    let shown = avsmd::protocol::display("a\u{1b}[2Jb\u{202e}c\u{2066}d\u{200b}e\u{feff}\tf\n");
    assert_eq!(
        shown,
        "a\\x1b[2Jb\\u{202e}c\\u{2066}d\\u{200b}e\\u{feff}\tf\n"
    );
    for n in (0x202a..=0x202e).chain(0x2066..=0x2069) {
        let c = char::from_u32(n).unwrap().to_string();
        assert!(!avsmd::protocol::display(&c).contains(&c), "{n:x}");
    }
    assert_eq!(avsmd::protocol::display("naïve café ✓"), "naïve café ✓");
}
