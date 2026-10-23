//! The wire: the command line is the protocol. A client sends its
//! arguments as one JSON line over a Unix socket; the kernel answers with
//! one JSON line holding the text to print and whether it went well.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::exit::{self, Exit};
use crate::kernel::Kernel;
use crate::ledger::Ledger;
use crate::record::{Entry, Record, Seq};

/// The kernel's answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    pub text: String,
}

/// Text as a terminal may safely be shown it: every control character
/// but the newline and the tab written out (`\x1b`, `\u{9b}`), so a
/// reading or a note cannot move the cursor, clear the screen, set the
/// title or type into the shell. The ledger keeps the bytes as they came;
/// only what is shown is changed.
pub fn display(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if (c.is_control() && c != '\n' && c != '\t') || invisible(c) {
            let n = c as u32;
            if n < 0x100 {
                out.push_str(&format!("\\x{n:02x}"));
            } else {
                out.push_str(&format!("\\u{{{n:x}}}"));
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// A formatting character that draws nothing or reorders what is around
/// it: the bidi controls (U+202A-202E, U+2066-2069 and the marks), zero
/// width characters, the soft hyphen, the invisible operators and the tag
/// characters.
fn invisible(c: char) -> bool {
    matches!(c as u32,
        0x00AD | 0x061C | 0x180E
        | 0x200B..=0x200F
        | 0x2028..=0x202E
        | 0x2060..=0x206F
        | 0xFEFF
        | 0xFFF9..=0xFFFB
        | 0xE0001 | 0xE0020..=0xE007F)
}

/// Print text to standard output as `display` shows it. A reader that
/// went away (`avsm tail | head -1`) ends the program quietly: it is not
/// a failure of ours.
pub fn emit(text: &str) {
    let mut out = std::io::stdout().lock();
    let shown = display(text);
    if let Err(e) = out.write_all(shown.as_bytes()).and_then(|()| out.flush()) {
        if e.kind() == ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
    }
}

/// A standard output that ends the program quietly when its reader has
/// gone away.
pub struct Stdout;

impl Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match std::io::stdout().write(buf) {
            Err(e) if e.kind() == ErrorKind::BrokenPipe => std::process::exit(0),
            other => other,
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match std::io::stdout().flush() {
            Err(e) if e.kind() == ErrorKind::BrokenPipe => std::process::exit(0),
            other => other,
        }
    }
}

/// Write a diagnostic line to standard error as `display` shows it. A
/// closed standard error loses the line; it never panics.
pub fn elog(line: &str) {
    let _ = writeln!(std::io::stderr(), "{}", display(line));
}

/// The longest socket path this system takes, in bytes: `sun_path` less
/// the terminating NUL (103 on macOS, 107 on Linux).
pub fn socket_limit() -> usize {
    // SAFETY: sockaddr_un is plain data; all zeroes is a valid value.
    let addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_path.len() - 1
}

/// Say so, with the way out, when a socket path is longer than the system
/// takes; None when it fits.
pub fn socket_too_long(sock: &Path) -> Option<String> {
    use std::os::unix::ffi::OsStrExt;
    let len = sock.as_os_str().as_bytes().len();
    let limit = socket_limit();
    (len > limit).then(|| {
        format!(
            "use a shorter --home or AVSMD_HOME (the socket path is {len} bytes, the limit is {limit}): {}",
            sock.display()
        )
    })
}

/// Send a command to a running avsmd and wait for the answer. No daemon
/// is EX_UNAVAILABLE, a socket this user may not open EX_NOPERM.
pub fn request(sock: &Path, args: &[String]) -> Result<Response, Exit> {
    if let Some(e) = socket_too_long(sock) {
        return Err(Exit::new(exit::USAGE, e));
    }
    let stream = UnixStream::connect(sock).map_err(|e| match e.kind() {
        ErrorKind::NotFound | ErrorKind::ConnectionRefused => Exit::new(
            exit::UNAVAILABLE,
            "avsmd is not running; start it with: avsmd",
        ),
        ErrorKind::PermissionDenied => Exit::new(
            exit::NOPERM,
            format!("cannot reach avsmd at {}: {e}", sock.display()),
        ),
        _ => Exit::new(
            exit::UNAVAILABLE,
            format!("cannot reach avsmd at {}: {e}", sock.display()),
        ),
    })?;
    exchange(&stream, |s| s.shutdown(Shutdown::Write), args)
}

/// Send a command to an avsmd listening on the tailnet at `host:port` and
/// wait for the answer. The command carries no identity: tailscaled tells
/// the daemon who is asking.
pub fn request_remote(at: &str, args: &[String]) -> Result<Response, Exit> {
    let stream = TcpStream::connect(at).map_err(|e| {
        Exit::new(
            exit::UNAVAILABLE,
            format!("cannot reach avsmd at {at}: {e}"),
        )
    })?;
    exchange(&stream, |s| s.shutdown(Shutdown::Write), args)
}

fn exchange<S>(
    mut stream: &S,
    shutdown: impl Fn(&S) -> std::io::Result<()>,
    args: &[String],
) -> Result<Response, Exit>
where
    for<'a> &'a S: Read + Write,
{
    let line = serde_json::to_string(args).map_err(|e| e.to_string())?;
    stream
        .write_all(line.as_bytes())
        .and_then(|_| stream.write_all(b"\n"))
        .and_then(|_| shutdown(stream))
        .map_err(|e| Exit::new(exit::UNAVAILABLE, format!("cannot talk to avsmd: {e}")))?;
    let mut answer = String::new();
    BufReader::new(stream)
        .read_line(&mut answer)
        .map_err(|e| Exit::new(exit::UNAVAILABLE, format!("avsmd did not answer: {e}")))?;
    serde_json::from_str(&answer)
        .map_err(|e| Exit::new(exit::SOFTWARE, format!("avsmd gave a bad answer: {e}")))
}

/// Is an avsmd answering at this socket?
pub fn alive(sock: &Path) -> bool {
    UnixStream::connect(sock).is_ok()
}

/// Bind the socket, owner-only, replacing a stale one.
pub fn bind(sock: &Path) -> Result<UnixListener, String> {
    if let Some(e) = socket_too_long(sock) {
        return Err(e);
    }
    if sock.exists() {
        if alive(sock) {
            return Err(format!("avsmd is already running at {}", sock.display()));
        }
        std::fs::remove_file(sock)
            .map_err(|e| format!("cannot replace {}: {e}", sock.display()))?;
    }
    let listener = UnixListener::bind(sock)
        .map_err(|e| format!("cannot listen at {}: {e}", sock.display()))?;
    std::fs::set_permissions(sock, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("cannot protect {}: {e}", sock.display()))?;
    Ok(listener)
}

/// How long a command waits, with the kernel free, for the outcome of a
/// lever it started; past it, the answer says the lever is still running.
pub const OUTCOME_WAIT: Duration = Duration::from_secs(60);

/// Answer commands until the listener closes, each connection on a thread
/// of its own. Each command runs with the kernel locked, one at a time.
/// The socket knows nobody's name.
pub fn serve(listener: UnixListener, kernel: Arc<Mutex<Kernel>>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                // Who connected, looked up at once, while the peer is
                // certainly there.
                let peer = peer(&stream);
                let kernel = Arc::clone(&kernel);
                std::thread::spawn(move || answer_from(stream, &kernel, None, peer));
            }
            Err(_) => return,
        }
    }
}

/// The commands that answer an ask, resume a venture or take one on: a
/// human's alone. A process avsmd started (a lever, a report, a role) is
/// refused them; it may still read, and pause (#51).
pub const HUMAN_ONLY: [&str; 5] = ["approve", "deny", "resume", "add", "propose"];

/// Who is at the other end of a socket connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Peer {
    /// A process this daemon started, or one of theirs: pid.
    Started(u32),
    /// Gone before it could be looked up. A client of avsm waits for its
    /// answer, so a peer gone already is no human's.
    Gone(u32),
    /// A process this daemon did not start.
    Other,
    /// Its pid or its parents could not be read here: allowed.
    Unknown,
}

/// Look up the peer of a connection: its pid from the kernel
/// (LOCAL_PEERPID on macOS, SO_PEERCRED on Linux), then its parents up
/// to this daemon or to the first process.
pub fn peer(stream: &UnixStream) -> Peer {
    use std::os::fd::AsRawFd;
    match peer_pid(stream.as_raw_fd()) {
        Some(pid) => descends(pid, std::process::id()),
        None => Peer::Unknown,
    }
}

/// Does `pid` descend from `ancestor`? Walks the parent of each process
/// until it reaches `ancestor`, the first process, or one it cannot read.
pub fn descends(pid: u32, ancestor: u32) -> Peer {
    let mut at = pid;
    for _ in 0..256 {
        if at == ancestor {
            return Peer::Started(pid);
        }
        if at <= 1 {
            return Peer::Other;
        }
        match parent(at) {
            Ok(p) => at = p,
            Err(gone) if at == pid && gone => return Peer::Gone(pid),
            Err(_) => return Peer::Unknown,
        }
    }
    Peer::Unknown
}

#[cfg(target_os = "macos")]
fn peer_pid(fd: std::os::fd::RawFd) -> Option<u32> {
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: pid and len are valid for writes of the sizes given.
    let r = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut len,
        )
    };
    (r == 0 && pid > 0).then_some(pid as u32)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_pid(fd: std::os::fd::RawFd) -> Option<u32> {
    // SAFETY: ucred is plain data; all zeroes is a valid value.
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: cred and len are valid for writes of the sizes given.
    let r = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (r == 0 && cred.pid > 0).then_some(cred.pid as u32)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
fn peer_pid(_fd: std::os::fd::RawFd) -> Option<u32> {
    None
}

/// The parent of a process; Err(true) when there is no such process.
#[cfg(target_os = "macos")]
fn parent(pid: u32) -> Result<u32, bool> {
    // SAFETY: proc_bsdinfo is plain data; all zeroes is a valid value.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: info is valid for writes of `size` bytes.
    let n = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if n == size {
        return Ok(info.pbi_ppid);
    }
    let gone = std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    Err(gone)
}

/// The parent of a process; Err(true) when there is no such process.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn parent(pid: u32) -> Result<u32, bool> {
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => s,
        Err(e) => return Err(e.kind() == ErrorKind::NotFound),
    };
    // pid (comm) state ppid ...; comm may hold anything but ends at the
    // last ')'.
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().nth(1))
        .and_then(|p| p.parse().ok())
        .ok_or(false)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
fn parent(_pid: u32) -> Result<u32, bool> {
    Err(false)
}

/// Wait for the outcomes of the intents a command started, up to
/// `OUTCOME_WAIT`, reading the ledger alone: one line each, or a line
/// saying which lever is still running.
fn outcomes(ledger: &Path, started: &[Seq]) -> String {
    let Some(&first) = started.iter().min() else {
        return String::new();
    };
    let Ok(ledger) = Ledger::open_read_only(ledger) else {
        return String::new();
    };
    let began = Instant::now();
    loop {
        let found: Vec<Entry> = ledger
            .after(first)
            .unwrap_or_default()
            .into_iter()
            .filter(
                |e| matches!(&e.record, Record::Outcome { intent, .. } if started.contains(intent)),
            )
            .collect();
        if found.len() >= started.len() || began.elapsed() >= OUTCOME_WAIT {
            let mut out = String::new();
            for e in &found {
                out.push_str(&e.line());
                out.push('\n');
            }
            for s in started {
                let done = found
                    .iter()
                    .any(|e| matches!(&e.record, Record::Outcome { intent, .. } if intent == s));
                if !done {
                    out.push_str(&format!("intent #{s} is still running; see: avsm tail\n"));
                }
            }
            return out;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Read one command from a stream, run it as `by` when the path knows who
/// is asking, and write the answer.
pub fn answer<S: Read + Write>(stream: S, kernel: &Arc<Mutex<Kernel>>, by: Option<&str>) {
    answer_from(stream, kernel, by, Peer::Unknown)
}

/// The refusal of a command a human alone may give, when a process avsmd
/// started, or a peer gone before it was looked up, gave it.
fn not_human(args: &[String], peer: Peer) -> Option<String> {
    let cmd = args.first()?;
    if !HUMAN_ONLY.contains(&cmd.as_str()) {
        return None;
    }
    match peer {
        Peer::Started(pid) => Some(format!(
            "refused: `{cmd}` came from pid {pid}, a process started by avsmd (a lever, a \
             report or a role); only a human gives it"
        )),
        Peer::Gone(pid) => Some(format!(
            "refused: `{cmd}` came from pid {pid}, gone before avsmd could see who started it"
        )),
        Peer::Other | Peer::Unknown => None,
    }
}

/// `answer`, from a peer looked up when it connected.
pub fn answer_from<S: Read + Write>(
    mut stream: S,
    kernel: &Arc<Mutex<Kernel>>,
    by: Option<&str>,
    peer: Peer,
) {
    let mut line = String::new();
    let read = BufReader::new(Read::by_ref(&mut stream)).read_line(&mut line);
    let response = match read {
        Err(e) => Response {
            ok: false,
            text: format!("could not read the command: {e}"),
        },
        Ok(_) => match serde_json::from_str::<Vec<String>>(line.trim()) {
            Err(e) => Response {
                ok: false,
                text: format!("could not read the command: {e}"),
            },
            Ok(args) if not_human(&args, peer).is_some() => {
                let text = not_human(&args, peer).unwrap_or_default();
                crate::kernel::log(&format!("{text}: {}", args.join(" ")));
                Response { ok: false, text }
            }
            Ok(args) => {
                let mut kernel = kernel
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                crate::kernel::debug(&format!("command: {}", args.join(" ")));
                // Draining: a command that waited out the last tick is
                // not run.
                let handled = if crate::kernel::stopping() {
                    Err("avsmd is stopping; nothing done".to_string())
                } else {
                    kernel.handle_as(&args, by)
                };
                let started = kernel.take_started();
                let ledger = kernel.home.join("ledger.db");
                // The kernel is free while the levers it started run.
                drop(kernel);
                match handled {
                    Ok(mut text) => {
                        text.push_str(&outcomes(&ledger, &started));
                        Response { ok: true, text }
                    }
                    Err(text) => Response { ok: false, text },
                }
            }
        },
    };
    if let Ok(mut out) = serde_json::to_string(&response) {
        out.push('\n');
        let _ = stream.write_all(out.as_bytes());
    }
}
