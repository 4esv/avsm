//! The wire: the command line is the protocol. A client sends its
//! arguments as one JSON line over a Unix socket; the kernel answers with
//! one JSON line holding the text to print and whether it went well.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::kernel::Kernel;

/// The kernel's answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    pub text: String,
}

/// Send a command to a running avsm and wait for the answer.
pub fn request(sock: &Path, args: &[String]) -> Result<Response, String> {
    let stream = UnixStream::connect(sock).map_err(|e| match e.kind() {
        ErrorKind::NotFound | ErrorKind::ConnectionRefused => {
            "avsm is not running; start it with: avsm start".to_string()
        }
        _ => format!("cannot reach avsm at {}: {e}", sock.display()),
    })?;
    exchange(&stream, |s| s.shutdown(Shutdown::Write), args)
}

/// Send a command to an avsm listening on the tailnet at `host:port` and
/// wait for the answer. The command carries no identity: tailscaled tells
/// the daemon who is asking.
pub fn request_remote(at: &str, args: &[String]) -> Result<Response, String> {
    let stream = TcpStream::connect(at).map_err(|e| format!("cannot reach avsm at {at}: {e}"))?;
    exchange(&stream, |s| s.shutdown(Shutdown::Write), args)
}

fn exchange<S>(
    mut stream: &S,
    shutdown: impl Fn(&S) -> std::io::Result<()>,
    args: &[String],
) -> Result<Response, String>
where
    for<'a> &'a S: Read + Write,
{
    let line = serde_json::to_string(args).map_err(|e| e.to_string())?;
    stream
        .write_all(line.as_bytes())
        .and_then(|_| stream.write_all(b"\n"))
        .and_then(|_| shutdown(stream))
        .map_err(|e| format!("cannot talk to avsm: {e}"))?;
    let mut answer = String::new();
    BufReader::new(stream)
        .read_line(&mut answer)
        .map_err(|e| format!("avsm did not answer: {e}"))?;
    serde_json::from_str(&answer).map_err(|e| format!("avsm gave a bad answer: {e}"))
}

/// Is an avsm answering at this socket?
pub fn alive(sock: &Path) -> bool {
    UnixStream::connect(sock).is_ok()
}

/// Bind the socket, owner-only, replacing a stale one.
pub fn bind(sock: &Path) -> Result<UnixListener, String> {
    if sock.exists() {
        if alive(sock) {
            return Err(format!("avsm is already running at {}", sock.display()));
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

/// Answer commands until the listener closes. Each command runs with the
/// kernel locked, one at a time. The socket knows nobody's name.
pub fn serve(listener: UnixListener, kernel: Arc<Mutex<Kernel>>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => answer(stream, &kernel, None),
            Err(_) => return,
        }
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
                match kernel.handle_as(&args, by) {
                    Ok(text) => Response { ok: true, text },
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
