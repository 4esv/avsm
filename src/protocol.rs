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

/// Send a command to a running avsmd and wait for the answer. No daemon
/// is EX_UNAVAILABLE, a socket this user may not open EX_NOPERM.
pub fn request(sock: &Path, args: &[String]) -> Result<Response, Exit> {
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
                let kernel = Arc::clone(&kernel);
                std::thread::spawn(move || answer(stream, &kernel, None));
            }
            Err(_) => return,
        }
    }
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
pub fn answer<S: Read + Write>(mut stream: S, kernel: &Arc<Mutex<Kernel>>, by: Option<&str>) {
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
