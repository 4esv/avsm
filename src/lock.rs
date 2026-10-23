//! One avsmd per home: an exclusive lock on a file in the home, held for
//! the daemon's life, with the holder's pid in it (#44).
//!
//! The socket alone cannot say whether a daemon runs: deleted, it lets a
//! second one start on the same ledger. The lock goes with the process,
//! however it ends, so a stale lock file means nothing.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::time::{Duration, Instant};

/// The lock file in a home.
pub const LOCK_FILE: &str = "avsmd.lock";

/// How long a starting daemon tries for the lock: `avsm doctor` holds it
/// shared for a moment to look, and that must not turn a start away.
const PATIENCE: Duration = Duration::from_secs(1);

/// The lock, held until dropped.
pub struct Held(File);

impl Held {
    /// Is the file at `path` still the one this lock is on? Not when it
    /// was deleted or another put in its place.
    pub fn still_at(&self, path: &Path) -> bool {
        let held = self.0.metadata().ok().map(|m| (m.dev(), m.ino()));
        held.is_some() && identity(path) == held
    }
}

/// Why the lock was not taken.
pub enum Refused {
    /// Another process holds it; its pid when the file names one.
    Running(Option<i32>),
    /// The lock file cannot be opened or locked.
    Io(String),
}

fn flock(file: &File, op: libc::c_int) -> bool {
    // SAFETY: flock takes a file descriptor this File owns.
    unsafe { libc::flock(file.as_raw_fd(), op | libc::LOCK_NB) == 0 }
}

fn pid_in(file: &mut File) -> Option<i32> {
    let mut text = String::new();
    file.rewind().ok()?;
    file.read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

/// Take the home's lock and write this process's pid into it.
pub fn acquire(home: &Path) -> Result<Held, Refused> {
    let path = home.join(LOCK_FILE);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| Refused::Io(format!("cannot open {}: {e}", path.display())))?;
    let began = Instant::now();
    loop {
        while !flock(&file, libc::LOCK_EX) {
            if began.elapsed() >= PATIENCE {
                return Err(Refused::Running(pid_in(&mut file)));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // The file locked must still be the one at the path: deleted or
        // replaced between the open and the lock, it locks out nobody.
        let held = Held(file);
        if held.still_at(&path) {
            file = held.0;
            break;
        }
        if began.elapsed() >= PATIENCE {
            return Err(Refused::Io(format!(
                "{} keeps being replaced",
                path.display()
            )));
        }
        file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| Refused::Io(format!("cannot open {}: {e}", path.display())))?;
    }
    let wrote = file
        .set_len(0)
        .and_then(|()| file.rewind())
        .and_then(|()| writeln!(file, "{}", std::process::id()));
    wrote.map_err(|e| Refused::Io(format!("cannot write {}: {e}", path.display())))?;
    Ok(Held(file))
}

/// The pid of the avsmd holding the home's lock, if one does; 0 when the
/// file names none. Writes nothing and creates nothing.
pub fn holder(home: &Path) -> Option<i32> {
    let mut file = File::open(home.join(LOCK_FILE)).ok()?;
    if flock(&file, libc::LOCK_SH) {
        flock(&file, libc::LOCK_UN);
        return None;
    }
    Some(pid_in(&mut file).unwrap_or(0))
}

/// The home whose lock a writer checks before each append, and the pid
/// of the daemon it writes for.
static GUARD: std::sync::OnceLock<(std::path::PathBuf, i32)> = std::sync::OnceLock::new();

/// From now on, in this process, refuse every append to the ledger once
/// another process than the daemon `daemon` holds `home`'s lock (#56).
/// The daemon calls it for itself, the warden for its daemon.
pub fn guard_writes(home: &Path, daemon: i32) {
    let _ = GUARD.set((home.join(LOCK_FILE), daemon));
}

/// May this process append to its home's ledger? Err names the process
/// holding the lock in its daemon's place. Called inside the append's
/// write transaction, so a steal seen here lands nothing after it.
///
/// A lock file gone, or one nobody holds, keeps nobody out: the daemon
/// takes it again within a tick, and until another takes it, no other
/// writes. Only a file at the path that another process holds says the
/// home is lost.
pub fn may_write() -> Result<(), String> {
    let Some((path, daemon)) = GUARD.get() else {
        return Ok(());
    };
    let Ok(mut file) = File::open(path) else {
        return Ok(());
    };
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return Ok(());
    }
    if flock(&file, libc::LOCK_SH) {
        flock(&file, libc::LOCK_UN);
        return Ok(());
    }
    // Held. A lock taken this moment has no pid in it yet: it is read
    // again for a little while before it is called nobody's.
    let mut holder = None;
    for _ in 0..10 {
        holder = pid_in(&mut file).filter(|p| *p > 0);
        if holder.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    match holder {
        Some(p) if p == *daemon => Ok(()),
        Some(p) => Err(format!(
            "avsmd (pid {p}) holds this home's lock now; this avsmd writes nothing more to its ledger"
        )),
        None => Err(
            "another process holds this home's lock now; this avsmd writes nothing more to its ledger"
                .to_string(),
        ),
    }
}

/// A file's device and inode, to tell it from one put in its place.
pub fn identity(path: &Path) -> Option<(u64, u64)> {
    std::fs::symlink_metadata(path)
        .ok()
        .map(|m| (m.dev(), m.ino()))
}
