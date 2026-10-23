//! Every process avsmd starts leads a process group of its own, and no
//! group may outlive avsmd, its warden or its venture's pause (#52).
//!
//! Each process keeps the groups it started, with the venture each is
//! for. The warden tells the daemon about each of its groups as it starts
//! it, over a channel of their own (fd 3 in the warden), so the daemon
//! can end them when the warden dies: their timers died with it. On a
//! stop the daemon ends its own and the warden's; on a pause, the
//! venture's. Ending a group is SIGTERM, a grace, then SIGKILL.
//!
//! The same channel carries each record the warden appends, sent before
//! the record commits: a warden killed after the commit has already told
//! the daemon, which never takes the record for another writer's.
//!
//! On Linux the leader of each group also gets PR_SET_PDEATHSIG, so it
//! dies with the thread that started it, covering the moment between a
//! spawn and its note. macOS has no such call: the notes are all there is.
//!
//! Each group leads a session of its own too (setsid), so whatever it
//! starts keeps the session even when it leaves the group, and only an
//! explicit setsid leaves both. A connection from a process in a group or
//! session avsmd made is no human's (#56).
//!
//! What a kill -9 leaves behind is kept on disk: each group is written to
//! `<home>/groups/` as it starts, with its leader's start time and a mark
//! in its environment, so a pgid used again by another process is never
//! taken for ours. A warden whose daemon died ends that daemon's groups;
//! a daemon starting ends those of any daemon no longer alive (#56).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::record::Seq;

/// The groups this process started that may still be alive, each with
/// the venture it is for ("" for none) and its mark.
static GROUPS: Mutex<BTreeMap<i32, (String, String)>> = Mutex::new(BTreeMap::new());

/// The variable each group avsmd starts finds its mark in; whatever it
/// starts inherits it.
pub const MARK_VAR: &str = "AVSM_GROUP";

/// Held to read while a command is started, and to write while the
/// other end of a warden's notes channel is open in the daemon: no
/// command started meanwhile can be handed it, whatever the platform does
/// about close-on-exec.
pub static SPAWNING: RwLock<()> = RwLock::new(());

thread_local! {
    /// The venture work on this thread is for, and the flag that stops it.
    static WITHIN: RefCell<Option<(String, Arc<AtomicBool>)>> = const { RefCell::new(None) };
}

/// Run `f` as work for `venture`: every group it starts is the
/// venture's, and is ended once `stop` is set.
pub fn within<T>(venture: &str, stop: Arc<AtomicBool>, f: impl FnOnce() -> T) -> T {
    let before = WITHIN.with(|w| w.replace(Some((venture.to_string(), stop))));
    let out = f();
    WITHIN.with(|w| *w.borrow_mut() = before);
    out
}

/// Has the work on this thread been told to stop?
pub fn stopped() -> bool {
    WITHIN.with(|w| {
        w.borrow()
            .as_ref()
            .is_some_and(|(_, s)| s.load(Ordering::SeqCst))
    })
}

fn venture_here() -> String {
    WITHIN.with(|w| {
        w.borrow()
            .as_ref()
            .map(|(v, _)| v.clone())
            .unwrap_or_default()
    })
}

/// A fresh mark for a group about to start, to put in its environment
/// as `MARK_VAR`: this process, the time, and a count.
pub fn mark() -> String {
    static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "{}-{nanos:x}-{}",
        std::process::id(),
        COUNT.fetch_add(1, Ordering::SeqCst)
    )
}

/// A group this process just started in `dir`, with the mark in its
/// environment:
/// kept, written down in the home, and told to the daemon when this is
/// the warden. Called with `SPAWNING` still held, so the reaper of
/// adopted children never takes the leader for one.
pub fn started(pgid: i32, mark: &str, dir: &Path) {
    let venture = venture_here();
    note(&Note::Group(pgid, venture.clone()));
    keep(pgid, mark, &venture, dir);
    lock(&GROUPS).insert(pgid, (venture, mark.to_string()));
}

/// A group this process started that is gone.
pub fn ended(pgid: i32) {
    if let Some((_, mark)) = lock(&GROUPS).remove(&pgid) {
        kept_ended(&mark);
    }
    note(&Note::Ended(pgid));
}

/// The groups this process started that are still alive.
pub fn live() -> Vec<(i32, String)> {
    let mut groups = lock(&GROUPS);
    groups.retain(|g, _| alive(*g));
    groups.iter().map(|(g, (v, _))| (*g, v.clone())).collect()
}

/// Is anything left in a process group?
pub fn alive(pgid: i32) -> bool {
    if pgid <= 1 {
        return false;
    }
    // SAFETY: kill with signal 0 only checks.
    let r = unsafe { libc::kill(-pgid, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn signal(pgid: i32, sig: libc::c_int) {
    if pgid > 1 {
        // SAFETY: kill has no memory preconditions.
        unsafe {
            libc::kill(-pgid, sig);
        }
    }
}

/// End process groups: SIGTERM now, then SIGKILL for any still alive at
/// `deadline`. Returns how many needed the SIGKILL.
pub fn end(pgids: &[i32], deadline: Instant) -> usize {
    let live: Vec<i32> = pgids.iter().copied().filter(|g| alive(*g)).collect();
    for g in &live {
        signal(*g, libc::SIGTERM);
    }
    while Instant::now() < deadline && live.iter().any(|g| alive(*g)) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let left: Vec<i32> = live.into_iter().filter(|g| alive(*g)).collect();
    for g in &left {
        signal(*g, libc::SIGKILL);
    }
    left.len()
}

/// `end`, on a thread of its own, with `grace` between the signals.
pub fn end_apart(pgids: Vec<i32>, grace: Duration) {
    if pgids.is_empty() {
        return;
    }
    std::thread::spawn(move || end(&pgids, Instant::now() + grace));
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// For a command about to start, in the child after fork and before
/// exec: lead a session of its own, and so a process group, and on Linux
/// die with the thread that started it.
pub fn lead_session() -> std::io::Result<()> {
    // SAFETY: setsid is async-signal-safe.
    if unsafe { libc::setsid() } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    die_with_parent()
}

/// For a command about to start, in the child after fork and before
/// exec: on Linux, die with the thread that started it.
pub fn die_with_parent() -> std::io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: prctl and getppid are async-signal-safe.
        unsafe {
            let parent = libc::getppid();
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // The parent died before the call: nobody would ever kill it.
            if parent == 1 {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
        }
    }
    Ok(())
}

// ----- the groups kept on disk -----

/// Where this process writes down the groups it starts, and the daemon
/// they are for.
struct Keeping {
    dir: PathBuf,
    daemon: Daemon,
}

static KEEPING: OnceLock<Keeping> = OnceLock::new();

/// A daemon, told from a later process with its pid by its start time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Daemon {
    pub pid: i32,
    pub start: Option<u64>,
}

impl Daemon {
    /// The process `pid` as it is now.
    pub fn of(pid: i32) -> Daemon {
        Daemon {
            pid,
            start: crate::procs::get(pid).and_then(|p| p.start),
        }
    }

    /// Is this very process still alive? Not when its pid now names
    /// another, started later.
    pub fn alive(&self) -> bool {
        match crate::procs::get(self.pid) {
            Some(p) => self.start.is_none() || p.start == self.start,
            None => false,
        }
    }
}

/// A group as written down in the home.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kept {
    pub pgid: i32,
    /// Its leader's start time.
    pub start: Option<u64>,
    /// The mark in its environment.
    pub mark: String,
    pub venture: String,
    /// The directory it was started in.
    #[serde(default)]
    pub dir: Option<PathBuf>,
    /// The daemon it was started for.
    pub daemon: Daemon,
    /// When it was seen gone, in unix seconds.
    #[serde(default)]
    pub ended: Option<i64>,
}

/// The directory in a home the groups are kept in.
pub const KEPT_DIR: &str = "groups";

/// How long a group gone is still kept: a connection from a process that
/// was in it is no human's for that long.
pub const RECENT: Duration = Duration::from_secs(60);

/// From now on write down in `home` each group this process starts, for
/// the daemon `daemon`.
pub fn keep_in(home: &Path, daemon: Daemon) {
    use std::os::unix::fs::DirBuilderExt;
    let dir = home.join(KEPT_DIR);
    let _ = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir);
    let _ = KEEPING.set(Keeping { dir, daemon });
}

fn kept_path(dir: &Path, mark: &str) -> PathBuf {
    dir.join(format!("{mark}.json"))
}

fn write_kept(dir: &Path, k: &Kept) {
    let Ok(json) = serde_json::to_vec(k) else {
        return;
    };
    let path = kept_path(dir, &k.mark);
    let tmp = dir.join(format!(".{}.tmp", k.mark));
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn keep(pgid: i32, mark: &str, venture: &str, dir: &Path) {
    let Some(keeping) = KEEPING.get() else {
        return;
    };
    let k = Kept {
        pgid,
        start: crate::procs::get(pgid).and_then(|p| p.start),
        mark: mark.to_string(),
        venture: venture.to_string(),
        dir: Some(std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())),
        daemon: keeping.daemon,
        ended: None,
    };
    write_kept(&keeping.dir, &k);
    prune(&keeping.dir);
}

fn kept_ended(mark: &str) {
    let Some(keeping) = KEEPING.get() else {
        return;
    };
    let path = kept_path(&keeping.dir, mark);
    if let Some(mut k) = read_one(&path) {
        k.ended = Some(crate::clock::now());
        write_kept(&keeping.dir, &k);
    }
}

fn read_one(path: &Path) -> Option<Kept> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Every group written down in `dir`, with its file.
fn read_kept(dir: &Path) -> Vec<(PathBuf, Kept)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| read_one(&p).map(|k| (p, k)))
        .collect()
}

/// Forget the groups gone for longer than `RECENT`.
fn prune(dir: &Path) {
    for (path, k) in read_kept(dir) {
        if alive(k.pgid) {
            continue;
        }
        let since = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok());
        if since.is_none_or(|s| s > RECENT) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Is `id`, a process group or a session, one this process or the
/// processes it keeps groups with started, alive or gone within
/// `RECENT`? Each group avsmd starts leads its session, so a session's id
/// is its group's.
pub fn made(id: i32) -> bool {
    if id <= 1 {
        return false;
    }
    if lock(&GROUPS).contains_key(&id) {
        return true;
    }
    KEEPING
        .get()
        .is_some_and(|k| read_kept(&k.dir).iter().any(|(_, kept)| kept.pgid == id))
}

/// Is the live group of `k` really the one written down, and not
/// another that took its pgid? Its leader, started when it was, is in
/// it; or a process in it carries its mark (where the system shows
/// another process's environment: Linux does, macOS mostly not); or a
/// process in it started no earlier than the leader did and works in the
/// directory the group was started in.
fn really(k: &Kept, table: &[crate::procs::Proc]) -> bool {
    let want = format!("{MARK_VAR}={}", k.mark);
    table.iter().filter(|p| p.pgid == k.pgid).any(|p| {
        (p.pid == k.pgid && k.start.is_some() && p.start == k.start)
            || crate::procs::env_of(p.pid).contains(&want)
            || (k.start.is_none_or(|s| p.start.is_some_and(|t| t >= s))
                && k.dir.is_some()
                && p.cwd
                    .as_ref()
                    .map(|c| std::fs::canonicalize(c).unwrap_or_else(|_| c.clone()))
                    == k.dir)
    })
}

/// The live groups written down in `dir` that `whose` picks and that are
/// really the ones written down; those gone are forgotten.
fn left(dir: &Path, whose: &dyn Fn(&Kept) -> bool) -> Vec<i32> {
    let kept = read_kept(dir);
    if kept.is_empty() {
        return Vec::new();
    }
    let table = crate::procs::all();
    let mut found = Vec::new();
    for (path, k) in kept {
        if !whose(&k) {
            continue;
        }
        if !alive(k.pgid) {
            let _ = std::fs::remove_file(&path);
        } else if really(&k, &table) {
            found.push(k.pgid);
        }
    }
    found
}

/// On a daemon's start: the groups written down in `home` for a daemon
/// that is no longer alive, still running and really theirs. A daemon
/// alive is left to end its own: it may be one this one took the home
/// from.
pub fn left_by_the_dead(home: &Path) -> Vec<i32> {
    let me = std::process::id() as i32;
    left(&home.join(KEPT_DIR), &|k| {
        k.daemon.pid != me && !k.daemon.alive()
    })
}

/// In a warden whose daemon died: the groups written down for that
/// daemon by anyone but this process, which ends its own runs itself.
pub fn left_by_my_daemon() -> Vec<i32> {
    let Some(keeping) = KEEPING.get() else {
        return Vec::new();
    };
    let mine: Vec<i32> = lock(&GROUPS).keys().copied().collect();
    let daemon = keeping.daemon;
    left(&keeping.dir, &|k| {
        k.daemon == daemon && !mine.contains(&k.pgid)
    })
}

// ----- children adopted on Linux -----

/// Make this process the subreaper of what it starts (Linux): a process
/// a lever or report leaves behind, by a double fork or a setsid, is
/// reparented to it rather than to init, so it is still found in avsmd's
/// tree. The children so adopted are reaped on a thread of their own.
///
/// The reaper waits on one pid at a time, never on any child: a wait on
/// any would take the exit of a child `std::process` waits on. It takes
/// only a zombie child that is in a session other than this process's
/// and is no group this process started. Every command avsmd starts in a
/// session of its own is one of its groups, written down while
/// `SPAWNING` is held, and the reaper holds it to write while it looks;
/// every other command std starts stays in this process's session.
pub fn adopt_orphans() {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: prctl with these arguments only sets a flag.
        if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
            return;
        }
        std::thread::spawn(|| loop {
            std::thread::sleep(Duration::from_secs(1));
            reap_adopted();
        });
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn reap_adopted() {
    let _alone = SPAWNING.write().unwrap_or_else(|p| p.into_inner());
    // SAFETY: getpid and getsid(0) have no preconditions.
    let (me, session) = unsafe { (libc::getpid(), libc::getsid(0)) };
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return;
    };
    for pid in dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok()) {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // pid (comm) state ppid pgrp session ...
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let f: Vec<&str> = rest.split_whitespace().take(4).collect();
        let [state, ppid, _, sid] = f[..] else {
            continue;
        };
        let (Ok(ppid), Ok(sid)) = (ppid.parse::<i32>(), sid.parse::<i32>()) else {
            continue;
        };
        if state == "Z" && ppid == me && sid != session && !lock(&GROUPS).contains_key(&pid) {
            let mut status = 0;
            // SAFETY: waitpid on one pid, with a valid status pointer.
            unsafe {
                libc::waitpid(pid, &mut status, libc::WNOHANG);
            }
        }
    }
}

// ----- the warden's notes to the daemon -----

/// What the warden tells the daemon, one JSON line each.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Note {
    /// A record about to commit: its number and hash.
    Record(Seq, String),
    /// A process group started, and its venture.
    Group(i32, String),
    /// A process group gone.
    Ended(i32),
}

/// The fd the warden finds its notes channel on.
pub const NOTES_FD: i32 = 3;

/// The warden's end of the channel, once it has taken it.
static NOTES: OnceLock<Mutex<UnixStream>> = OnceLock::new();

/// In the warden: take the notes channel at fd 3, if the daemon gave one,
/// and keep it from every process the warden starts.
pub fn take_notes() {
    // SAFETY: fcntl F_GETFD only reads the flags of a descriptor.
    if unsafe { libc::fcntl(NOTES_FD, libc::F_GETFD) } == -1 {
        return;
    }
    // SAFETY: fd 3 is open, given by the daemon for this alone; nothing
    // else in this process owns it.
    let stream = unsafe { UnixStream::from_raw_fd(NOTES_FD) };
    // SAFETY: as above; FD_CLOEXEC keeps it from levers and commands.
    unsafe {
        libc::fcntl(stream.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
    }
    let _ = NOTES.set(Mutex::new(stream));
}

/// Tell the daemon, when this is a warden it listens to. False when the
/// note could not be sent; true when there is nobody to send it to.
pub fn note(n: &Note) -> bool {
    let Some(out) = NOTES.get() else {
        return true;
    };
    let Ok(mut line) = serde_json::to_string(n) else {
        return false;
    };
    line.push('\n');
    lock(out).write_all(line.as_bytes()).is_ok()
}

/// Is this a warden with a daemon listening?
pub fn noting() -> bool {
    NOTES.get().is_some()
}

/// A record about to commit, told to the daemon first.
pub fn note_record(seq: Seq, hash: &str) {
    note(&Note::Record(seq, hash.to_string()));
}

/// What the daemon heard from its warden, warden after warden.
#[derive(Default)]
struct Heard {
    /// The channel of the warden now running.
    stream: Option<Arc<UnixStream>>,
    /// A line not yet whole.
    partial: Vec<u8>,
    written: Vec<(Seq, String)>,
    groups: BTreeMap<i32, String>,
}

impl Heard {
    /// Read all there is now, without waiting. True at the end of the
    /// stream.
    fn drain(&mut self) -> bool {
        let Some(stream) = self.stream.clone() else {
            return true;
        };
        let mut buf = [0u8; 8192];
        let mut end = false;
        loop {
            match (&*stream).read(&mut buf) {
                Ok(0) => {
                    end = true;
                    break;
                }
                Ok(n) => self.partial.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        while let Some(at) = self.partial.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=at).collect();
            match serde_json::from_slice::<Note>(&line) {
                Ok(Note::Record(seq, hash)) => self.written.push((seq, hash)),
                Ok(Note::Group(g, v)) => {
                    self.groups.insert(g, v);
                }
                Ok(Note::Ended(g)) => {
                    self.groups.remove(&g);
                }
                Err(_) => {}
            }
        }
        end
    }
}

/// The daemon's end of the notes channel, kept across wardens.
#[derive(Clone, Default)]
pub struct Notes(Arc<Mutex<Heard>>);

impl Notes {
    /// A fresh channel for a warden about to start: its end, to hand the
    /// warden as fd 3. What the last warden said is read to the end
    /// first and kept.
    pub fn attach(&self) -> std::io::Result<UnixStream> {
        let (ours, theirs) = UnixStream::pair()?;
        ours.set_nonblocking(true)?;
        let ours = Arc::new(ours);
        {
            let mut heard = lock(&self.0);
            heard.drain();
            heard.partial.clear();
            heard.stream = Some(Arc::clone(&ours));
        }
        // A reader, so a warden never blocks on a full channel while the
        // daemon waits on it; `drain` reads whatever it has not.
        let me = self.clone();
        std::thread::spawn(move || loop {
            let mut fd = libc::pollfd {
                fd: ours.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: fd is a valid pollfd for one descriptor `ours` keeps open.
            unsafe {
                libc::poll(&mut fd, 1, 1000);
            }
            let mut heard = lock(&me.0);
            if !heard.stream.as_ref().is_some_and(|s| Arc::ptr_eq(s, &ours)) {
                return;
            }
            if heard.drain() {
                return;
            }
        });
        Ok(theirs)
    }

    /// The records the warden said it appended since last asked.
    pub fn take_written(&self) -> Vec<(Seq, String)> {
        let mut heard = lock(&self.0);
        heard.drain();
        std::mem::take(&mut heard.written)
    }

    /// The warden's process groups still alive, with their ventures.
    pub fn groups(&self) -> Vec<(i32, String)> {
        let mut heard = lock(&self.0);
        heard.drain();
        heard.groups.retain(|g, _| alive(*g));
        heard.groups.iter().map(|(g, v)| (*g, v.clone())).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    #[test]
    fn notes_sent_before_the_sender_dies_are_heard_after() {
        let notes = Notes::default();
        let theirs = notes.attach().unwrap();
        let mut w = &theirs;
        for n in [
            Note::Record(7, "ab".into()),
            Note::Group(4242, "v".into()),
            Note::Ended(4242),
        ] {
            let mut line = serde_json::to_string(&n).unwrap();
            line.push('\n');
            w.write_all(line.as_bytes()).unwrap();
        }
        drop(theirs);
        assert_eq!(notes.take_written(), vec![(7, "ab".to_string())]);
        assert!(notes.groups().is_empty());
        // A second warden's channel keeps what the first said unread.
        let theirs = notes.attach().unwrap();
        (&theirs).write_all(b"{\"record\":[8,\"cd\"]}\n").unwrap();
        let _again = notes.attach().unwrap();
        assert_eq!(notes.take_written(), vec![(8, "cd".to_string())]);
    }

    #[test]
    fn a_group_that_ignores_sigterm_is_killed_at_the_deadline() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "trap '' TERM; sleep 30 & wait"])
            .process_group(0)
            .spawn()
            .unwrap();
        let g = child.id() as i32;
        std::thread::sleep(Duration::from_millis(200));
        let began = Instant::now();
        let killed = end(&[g], Instant::now() + Duration::from_millis(500));
        let _ = child.wait();
        assert_eq!(killed, 1);
        assert!(began.elapsed() >= Duration::from_millis(500));
        let gone = Instant::now();
        while alive(g) && gone.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(g));
    }

    /// A leader in a session of its own, with `mark` in its environment,
    /// that leaves a sleep behind by a double fork: the leader and the
    /// sleep's pid.
    fn leave_behind(mark: &str, dir: &Path) -> (std::process::Child, i32) {
        use std::io::BufRead;
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "( sleep 30 & echo $! ); sleep 30"])
            .current_dir(dir)
            .env(MARK_VAR, mark)
            .stdout(std::process::Stdio::piped());
        // SAFETY: the hook only calls setsid, prctl and getppid.
        unsafe {
            cmd.pre_exec(lead_session);
        }
        let mut child = cmd.spawn().unwrap();
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        (child, line.trim().parse().unwrap())
    }

    fn finish(mut child: std::process::Child) {
        signal(child.id() as i32, libc::SIGKILL);
        let _ = child.wait();
    }

    #[test]
    fn a_double_forked_peer_is_known_by_the_group_and_session_avsmd_made() {
        let (child, orphan) = leave_behind("t1", Path::new("/"));
        let leader = child.id() as i32;
        // SAFETY: getpgid and getsid only read.
        let (pgid, sid) = unsafe { (libc::getpgid(orphan), libc::getsid(orphan)) };
        let by_group = crate::protocol::made_by(orphan as u32, &|id| id == leader);
        let by_none = crate::protocol::made_by(orphan as u32, &|_| false);
        let me = crate::protocol::made_by(std::process::id(), &|id| id == leader);
        finish(child);
        assert_eq!((pgid, sid), (leader, leader));
        assert_eq!(by_group, crate::protocol::Peer::Made(orphan as u32, leader));
        assert_eq!(by_none, crate::protocol::Peer::Other);
        assert_eq!(me, crate::protocol::Peer::Other);
    }

    fn kept(pgid: i32, start: Option<u64>, mark: &str, dir: &Path) -> Kept {
        Kept {
            pgid,
            start,
            mark: mark.to_string(),
            venture: "v".into(),
            dir: Some(std::fs::canonicalize(dir).unwrap()),
            daemon: Daemon {
                pid: 1,
                start: None,
            },
            ended: None,
        }
    }

    #[test]
    fn a_group_left_running_is_ended_only_when_it_is_really_ours() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("kept");
        let (va, vb, elsewhere) = (
            tmp.path().join("a"),
            tmp.path().join("b"),
            tmp.path().join("c"),
        );
        for d in [&dir, &va, &vb, &elsewhere] {
            std::fs::create_dir_all(d).unwrap();
        }
        let dir = dir.as_path();
        let (a, _) = leave_behind("mark-a", &va);
        let (b, _) = leave_behind("mark-b", &vb);
        let (ga, gb) = (a.id() as i32, b.id() as i32);
        let start_a = crate::procs::get(ga).and_then(|p| p.start);
        let start_b = crate::procs::get(gb).and_then(|p| p.start);
        // Its leader, started when it was: ours.
        write_kept(dir, &kept(ga, start_a, "x1", &elsewhere));
        // A leader started at another time, no mark of ours, and nothing
        // in the group started after it in its directory: the pgid was
        // used again, and is never signalled.
        write_kept(dir, &kept(gb, start_b.map(|s| s + 7), "x2", &elsewhere));
        // Started in its directory no earlier than the leader was: ours,
        // whatever the leader's start says.
        write_kept(dir, &kept(gb, Some(1), "x4", &vb));
        // Gone: forgotten.
        write_kept(dir, &kept(999_999, None, "x3", &elsewhere));
        let mut found = left(dir, &|_| true);
        found.sort_unstable();
        let picked = left(dir, &|k| k.mark == "x2");
        let files: Vec<String> = read_kept(dir).into_iter().map(|(_, k)| k.mark).collect();
        finish(a);
        finish(b);
        let mut want = vec![ga, gb];
        want.sort_unstable();
        assert_eq!(found, want);
        assert!(picked.is_empty(), "{picked:?}");
        assert!(!files.contains(&"x3".to_string()), "{files:?}");
        assert_eq!(files.len(), 3, "{files:?}");
    }
}
