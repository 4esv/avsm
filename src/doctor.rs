//! `avsmd -t` and `avsm doctor`: check a home and change nothing in it.
//!
//! `-t` tests what the daemon would run: the config and every adopted
//! mandate, with each mandate's teachback. `doctor` checks everything a
//! running daemon needs, and names the command that fixes each failure.

use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::exit;
use crate::ledger::Ledger;
use crate::record::Entry;
use crate::venture::{self, Venture};

/// The ledger file in a home.
pub const LEDGER_FILE: &str = "ledger.db";

/// What a check found: text for standard output, problems for standard
/// error, and the most severe exit code among them (0 when none).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Found {
    pub out: String,
    pub problems: Vec<String>,
    pub code: u8,
}

impl Found {
    fn ok(&mut self, line: impl AsRef<str>) {
        self.out.push_str("ok    ");
        self.out.push_str(line.as_ref());
        self.out.push('\n');
    }

    /// A failure, with the exact command that fixes it.
    fn fail(&mut self, code: u8, line: impl AsRef<str>, fix: impl AsRef<str>) {
        self.out.push_str("FAIL  ");
        self.out.push_str(line.as_ref());
        self.out.push_str("\n      fix: ");
        self.out.push_str(fix.as_ref());
        self.out.push('\n');
        self.problems.push(line.as_ref().to_string());
        self.code = exit::worst(self.code, code);
    }
}

/// Quote a path for a shell command line.
fn sh(path: &Path) -> String {
    let s = path.to_string_lossy();
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-~+@%:,".contains(c))
    {
        s.into_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Every entry of the home's ledger, read without writing: None when
/// there is no ledger yet.
fn read_ledger(home: &Path) -> Result<Option<Vec<Entry>>, String> {
    let path = home.join(LEDGER_FILE);
    if !path.is_file() {
        return Ok(None);
    }
    let ledger = Ledger::open_read_only(&path).map_err(|e| e.to_string())?;
    ledger.verify().map_err(|e| e.to_string())?;
    ledger.all().map(Some).map_err(|e| e.to_string())
}

/// The ventures a human adopted, self left out.
fn adopted(entries: &[Entry]) -> BTreeMap<String, Venture> {
    let mut ventures = venture::load_all(entries);
    ventures.remove(crate::gate::SELF);
    ventures
}

/// What is wrong with a venture's mandate file on disk, if anything:
/// a file that will not read or parse, or text other than the adopted.
enum Disk {
    Same,
    Changed,
    Broken(String),
    Gone,
}

fn on_disk(v: &Venture) -> Disk {
    let Some(dir) = &v.path else {
        return Disk::Same;
    };
    if !dir.is_dir() {
        return Disk::Gone;
    }
    match venture::read_mandate(dir) {
        Ok((_, hash, _)) if hash == v.hash => Disk::Same,
        Ok(_) => Disk::Changed,
        Err(e) => Disk::Broken(e),
    }
}

/// `avsmd -t`: the config parses, and every adopted mandate parses and
/// is the text on disk. Prints each mandate's teachback. Writes nothing.
pub fn test(home: &Path) -> Found {
    let mut found = Found::default();
    let path = crate::config::path(home);
    match crate::kernel::load_config(home) {
        Ok(_) => found
            .out
            .push_str(&format!("config {}: ok\n", path.display())),
        Err(e) => {
            found.problems.push(e.text);
            found.code = e.code;
        }
    }
    let entries = match read_ledger(home) {
        Ok(Some(entries)) => entries,
        Ok(None) => {
            found.out.push_str("no ledger yet: no mandate adopted\n");
            return found;
        }
        Err(e) => {
            found
                .problems
                .push(format!("the ledger in {}: {e}", home.display()));
            found.code = exit::worst(found.code, exit::DATAERR);
            return found;
        }
    };
    for (name, v) in adopted(&entries) {
        let dir = v.path.clone().unwrap_or_default();
        if let Some(problem) = &v.problem {
            found.problems.push(format!("{name}: {problem}"));
            found.code = exit::worst(found.code, exit::CONFIG);
            continue;
        }
        found
            .out
            .push_str(&format!("{name} from {}\n", dir.display()));
        found.out.push_str(&crate::teach::render(&v.mandate));
        match on_disk(&v) {
            Disk::Same => {}
            Disk::Changed => found.out.push_str(&format!(
                "  the file on disk differs from the adopted text; to adopt it: avsm add {}\n",
                sh(&dir)
            )),
            Disk::Broken(e) => {
                found.problems.push(format!("{name}: {e}"));
                found.code = exit::worst(found.code, exit::CONFIG);
            }
            Disk::Gone => {
                found
                    .problems
                    .push(format!("{name}: {} is not a directory", dir.display()));
                found.code = exit::worst(found.code, exit::NOINPUT);
            }
        }
    }
    found
}

/// Where `avsmd` is: beside this binary, else on the PATH.
fn find_daemon() -> Option<PathBuf> {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join("avsmd")));
    if let Some(p) = beside.filter(|p| p.is_file()) {
        return Some(p);
    }
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|d| d.join("avsmd"))
        .find(|p| p.is_file())
}

/// `avsm doctor`: check the home and its permissions, the config, the
/// ledger and its chain, each adopted mandate, the daemon, the warden
/// and roles executable, network namespaces on Linux, and a leftover 1.0
/// home. Every failure names the command that fixes it. Writes nothing.
pub fn doctor(home: &Path, old_home: Option<&Path>) -> Found {
    let mut found = Found::default();
    let h = sh(home);

    // The home.
    match std::fs::metadata(home) {
        Ok(m) if !m.is_dir() => found.fail(
            exit::CANTCREAT,
            format!("{} is not a directory", home.display()),
            format!("mv {h} {h}.moved && avsmd --home {h}"),
        ),
        Ok(m) => {
            // SAFETY: geteuid has no preconditions.
            let me = unsafe { libc::geteuid() };
            if m.uid() != me {
                found.fail(
                    exit::NOPERM,
                    format!("{} belongs to uid {}, not you", home.display(), m.uid()),
                    format!("sudo chown -R \"$(id -u)\" {h}"),
                );
            } else if m.permissions().mode() & 0o700 != 0o700 {
                found.fail(
                    exit::NOPERM,
                    format!(
                        "{} is mode {:o}; you need read, write and search",
                        home.display(),
                        m.permissions().mode() & 0o777
                    ),
                    format!("chmod u+rwx {h}"),
                );
            } else {
                found.ok(format!("home {}", home.display()));
            }
        }
        Err(_) => found.fail(
            exit::NOINPUT,
            format!("no home at {}", home.display()),
            format!("avsmd --home {h}"),
        ),
    }
    let keys = home.join(crate::config::KEYS_FILE);
    if let Ok(m) = std::fs::metadata(&keys) {
        if m.permissions().mode() & 0o077 != 0 {
            found.fail(
                exit::NOPERM,
                format!("{} is readable by others", keys.display()),
                format!("chmod 600 {}", sh(&keys)),
            );
        } else {
            found.ok(format!("keys {} readable by you alone", keys.display()));
        }
    }

    // The config.
    let config = crate::config::path(home);
    match crate::kernel::load_config(home) {
        Ok(_) if config.is_file() => found.ok(format!("config {}", config.display())),
        Ok(_) => found.ok(format!("config: none at {}, defaults", config.display())),
        Err(e) => found.fail(
            e.code,
            e.text,
            format!("${{EDITOR:-vi}} {}  # then: avsmd -t", sh(&config)),
        ),
    }

    // The ledger and its chain.
    let ledger = home.join(LEDGER_FILE);
    let entries = match read_ledger(home) {
        Ok(Some(entries)) => {
            found.ok(format!(
                "ledger {}: {} records, chain verified",
                ledger.display(),
                entries.len()
            ));
            entries
        }
        Ok(None) => {
            found.ok(format!(
                "ledger: none yet at {}; avsmd makes one",
                ledger.display()
            ));
            Vec::new()
        }
        Err(e) => {
            found.fail(
                exit::DATAERR,
                format!("the ledger {}: {e}", ledger.display()),
                format!(
                    "mv {l} {l}.broken  # keeps it for a look; avsmd starts a new one",
                    l = sh(&ledger)
                ),
            );
            Vec::new()
        }
    };

    // Each adopted mandate.
    let ventures = adopted(&entries);
    for (name, v) in &ventures {
        let dir = v.path.clone().unwrap_or_default();
        let d = sh(&dir);
        if let Some(problem) = &v.problem {
            found.fail(
                exit::CONFIG,
                format!("{name}: {problem}"),
                format!("avsm add {d}"),
            );
            continue;
        }
        match on_disk(v) {
            Disk::Same => found.ok(format!("mandate {name} at {}", dir.display())),
            Disk::Changed => found.fail(
                exit::CONFIG,
                format!(
                    "{name}: {}/mandate.yaml differs from the adopted text",
                    dir.display()
                ),
                format!("avsm add {d}"),
            ),
            Disk::Broken(e) => found.fail(
                exit::CONFIG,
                format!("{name}: {e}"),
                format!("${{EDITOR:-vi}} {d}/mandate.yaml && avsm add {d}"),
            ),
            Disk::Gone => found.fail(
                exit::NOINPUT,
                format!("{name}: {} is not a directory", dir.display()),
                format!("avsm pause {name}  # or put the directory back at {d}"),
            ),
        }
    }

    // The daemon.
    let sock = crate::kernel::socket_path(home);
    if crate::protocol::alive(&sock) {
        found.ok(format!("avsmd answering at {}", sock.display()));
    } else {
        found.fail(
            exit::UNAVAILABLE,
            "avsmd is not running",
            format!("avsmd --home {h}"),
        );
    }

    // The warden and roles: both are avsmd, started by itself.
    match find_daemon() {
        Some(p) => {
            let mode = std::fs::metadata(&p).map_or(0, |m| m.permissions().mode());
            if mode & 0o111 == 0 {
                found.fail(
                    exit::NOPERM,
                    format!("{} is not executable", p.display()),
                    format!("chmod +x {}", sh(&p)),
                );
            } else {
                found.ok(format!("warden and roles executable {}", p.display()));
            }
        }
        None => found.fail(
            exit::UNAVAILABLE,
            "no avsmd beside avsm or on the PATH: no warden or roles process can start",
            "cargo install avsmd",
        ),
    }

    // Network namespaces, for a `run` under `network: none`.
    if cfg!(target_os = "linux") {
        let wanted: Vec<&str> = ventures
            .iter()
            .filter(|(_, v)| v.mandate.network.as_ref().is_some_and(Vec::is_empty))
            .map(|(n, _)| n.as_str())
            .collect();
        if crate::kernel::namespaces_available() {
            found.ok("network namespaces available");
        } else if !wanted.is_empty() {
            found.fail(
                exit::NOPERM,
                format!(
                    "no network namespace here, so network: none is not enforced for {}",
                    wanted.join(", ")
                ),
                "sudo sysctl -w kernel.unprivileged_userns_clone=1 user.max_user_namespaces=15000",
            );
        } else {
            found.ok("network namespaces unavailable; no mandate needs one");
        }
    }

    // A home avsm 1.0 left behind.
    if let Some(old) = old_home.filter(|o| *o != home && o.is_dir()) {
        let o = sh(old);
        let fix = if home.exists() {
            format!("mv {o} {o}-1.0  # {h} exists already; keep the old home aside")
        } else {
            format!("mkdir -p \"$(dirname {h})\" && mv {o} {h}")
        };
        found.fail(
            exit::CONFIG,
            format!(
                "an avsm 1.0 home is left at {}; avsmd reads {}",
                old.display(),
                home.display()
            ),
            fix,
        );
    }
    found
}
