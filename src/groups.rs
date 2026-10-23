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

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::record::Seq;

/// The groups this process started that may still be alive, each with
/// the venture it is for ("" for none).
static GROUPS: Mutex<BTreeMap<i32, String>> = Mutex::new(BTreeMap::new());

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

/// A group this process just started: kept, and told to the daemon when
/// this is the warden.
pub fn started(pgid: i32) {
    let venture = venture_here();
    note(&Note::Group(pgid, venture.clone()));
    lock(&GROUPS).insert(pgid, venture);
}

/// A group this process started that is gone.
pub fn ended(pgid: i32) {
    lock(&GROUPS).remove(&pgid);
    note(&Note::Ended(pgid));
}

/// The groups this process started that are still alive.
pub fn live() -> Vec<(i32, String)> {
    let mut groups = lock(&GROUPS);
    groups.retain(|g, _| alive(*g));
    groups.iter().map(|(g, v)| (*g, v.clone())).collect()
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
}
