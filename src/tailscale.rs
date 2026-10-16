//! Remote access over Tailscale. Off unless config says `listen:
//! tailscale`. The daemon binds only to its own tailnet address, asks
//! tailscaled who each peer is, and lets in only the logins listed under
//! `remote_users`. The client never says who it is; tailscaled does.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::kernel::Kernel;
use crate::protocol;

/// Where tailscaled answers on Linux.
pub const LOCALAPI_SOCKET: &str = "/var/run/tailscale/tailscaled.sock";
/// The port the remote listener takes unless config says another.
pub const DEFAULT_PORT: u16 = 2876;
/// How long to wait for tailscaled.
const TIMEOUT: Duration = Duration::from_secs(5);

/// Something that says who a peer is: a login like `alice@github`.
pub trait Whois: Send {
    fn whois(&self, peer: SocketAddr) -> Result<String, String>;
}

/// tailscaled itself: its LocalAPI over the unix socket, falling back to
/// the `tailscale` command.
pub struct Tailscaled {
    pub socket: PathBuf,
    pub cli: String,
}

impl Default for Tailscaled {
    fn default() -> Tailscaled {
        Tailscaled {
            socket: PathBuf::from(LOCALAPI_SOCKET),
            cli: "tailscale".to_string(),
        }
    }
}

impl Tailscaled {
    /// One GET on the LocalAPI, answered as JSON.
    pub fn localapi(&self, path: &str) -> Result<serde_json::Value, String> {
        let mut stream = UnixStream::connect(&self.socket)
            .map_err(|e| format!("cannot reach tailscaled at {}: {e}", self.socket.display()))?;
        stream
            .set_read_timeout(Some(TIMEOUT))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(TIMEOUT))
            .map_err(|e| e.to_string())?;
        // HTTP/1.0: the answer comes whole and the connection closes after.
        stream
            .write_all(
                format!(
                    "GET {path} HTTP/1.0\r\nHost: local-tailscaled.sock\r\nSec-Tailscale: localapi\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .map_err(|e| format!("tailscaled: {e}"))?;
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .map_err(|e| format!("tailscaled: {e}"))?;
        let text = String::from_utf8_lossy(&raw);
        let (head, body) = text
            .split_once("\r\n\r\n")
            .ok_or_else(|| "tailscaled gave no answer".to_string())?;
        let status: u16 = head
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| "tailscaled gave a bad answer".to_string())?;
        if status != 200 {
            return Err(format!(
                "tailscaled answered {status} to {path}: {}",
                body.trim().chars().take(200).collect::<String>()
            ));
        }
        serde_json::from_str(body).map_err(|e| format!("tailscaled did not answer JSON: {e}"))
    }

    /// The `tailscale` command, as JSON.
    fn cli_json(&self, args: &[&str]) -> Result<serde_json::Value, String> {
        let out = std::process::Command::new(&self.cli)
            .args(args)
            .output()
            .map_err(|e| format!("cannot run {}: {e}", self.cli))?;
        if !out.status.success() {
            return Err(format!(
                "{} {} failed: {}",
                self.cli,
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        serde_json::from_slice(&out.stdout)
            .map_err(|e| format!("{} did not answer JSON: {e}", self.cli))
    }

    /// Is tailscaled there at all?
    pub fn present(&self) -> bool {
        self.socket.exists() || self.cli_json(&["status", "--json"]).is_ok()
    }

    /// This machine's tailnet addresses.
    pub fn addresses(&self) -> Result<Vec<IpAddr>, String> {
        let status = self.localapi("/localapi/v0/status").or_else(|e| {
            self.cli_json(&["status", "--json"])
                .map_err(|c| format!("{e}; {c}"))
        })?;
        let ips = status["Self"]["TailscaleIPs"]
            .as_array()
            .or_else(|| status["TailscaleIPs"].as_array())
            .cloned()
            .unwrap_or_default();
        let found: Vec<IpAddr> = ips
            .iter()
            .filter_map(|v| v.as_str()?.parse().ok())
            .collect();
        if found.is_empty() {
            return Err(
                "tailscaled reports no tailnet address; is this machine logged in?".to_string(),
            );
        }
        Ok(found)
    }
}

impl Whois for Tailscaled {
    fn whois(&self, peer: SocketAddr) -> Result<String, String> {
        let reply = self
            .localapi(&format!("/localapi/v0/whois?addr={peer}"))
            .or_else(|e| {
                self.cli_json(&["whois", "--json", &peer.ip().to_string()])
                    .map_err(|c| format!("{e}; {c}"))
            })?;
        reply["UserProfile"]["LoginName"]
            .as_str()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("tailscaled does not know who {} is", peer.ip()))
    }
}

/// Is this address on a tailnet: 100.64.0.0/10 or fd7a:115c:a1e0::/48?
pub fn is_tailnet(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, _, _] = v4.octets();
            a == 100 && (64..128).contains(&b)
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

/// Bind to the tailnet address among `addresses`, and nothing else.
pub fn bind(addresses: &[IpAddr], port: u16) -> Result<TcpListener, String> {
    let ip = addresses
        .iter()
        .copied()
        .filter(|ip| is_tailnet(*ip))
        .min_by_key(|ip| ip.is_ipv6())
        .ok_or_else(|| {
            let list: Vec<String> = addresses.iter().map(ToString::to_string).collect();
            format!(
                "refusing to listen: none of {} is a tailnet address",
                if list.is_empty() {
                    "(no address)".to_string()
                } else {
                    list.join(", ")
                }
            )
        })?;
    let listener =
        TcpListener::bind((ip, port)).map_err(|e| format!("cannot listen at {ip}:{port}: {e}"))?;
    let bound = listener.local_addr().map_err(|e| e.to_string())?;
    if !is_tailnet(bound.ip()) {
        return Err(format!(
            "refusing to listen at {bound}: not a tailnet address"
        ));
    }
    Ok(listener)
}

/// Answer commands until the listener closes. Each connection is one
/// command; the peer's login comes from `whois`, never from the peer.
pub fn serve(
    listener: TcpListener,
    whois: Box<dyn Whois>,
    allowed: Vec<String>,
    kernel: Arc<Mutex<Kernel>>,
) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { return };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
        let peer = match stream.peer_addr() {
            Ok(p) => p,
            Err(_) => continue,
        };
        match whois.whois(peer) {
            Ok(login) if allowed.iter().any(|a| a == &login) => {
                protocol::answer(stream, &kernel, Some(&login));
            }
            Ok(login) => refuse(stream, &format!("{login} is not in remote_users")),
            Err(e) => refuse(stream, &format!("cannot tell who {} is: {e}", peer.ip())),
        }
    }
}

fn refuse(mut stream: TcpStream, why: &str) {
    // Read the command so the client sees the refusal, not a reset.
    let mut line = String::new();
    let _ = BufReader::new(Read::by_ref(&mut stream)).read_line(&mut line);
    let response = protocol::Response {
        ok: false,
        text: format!("{why}\n"),
    };
    if let Ok(mut out) = serde_json::to_string(&response) {
        out.push('\n');
        let _ = stream.write_all(out.as_bytes());
    }
}

/// Start the remote listener if config asks for one. Says what it did.
pub fn start(
    config: &crate::config::Config,
    kernel: Arc<Mutex<Kernel>>,
) -> Result<Option<SocketAddr>, String> {
    if config.listen.as_deref() != Some("tailscale") {
        return Ok(None);
    }
    let daemon = Tailscaled::default();
    if !daemon.present() {
        return Err(format!(
            "listen: tailscale is set, but tailscaled is not running ({} is missing); the remote listener is off",
            daemon.socket.display()
        ));
    }
    let addresses = daemon.addresses()?;
    let listener = bind(&addresses, config.port)?;
    let at = listener.local_addr().map_err(|e| e.to_string())?;
    let allowed = config.remote_users.clone();
    std::thread::spawn(move || serve(listener, Box::new(daemon), allowed, kernel));
    Ok(Some(at))
}

/// Unused address families, named for messages.
pub fn example_addresses() -> (Ipv4Addr, Ipv6Addr) {
    (
        Ipv4Addr::new(100, 64, 0, 1),
        Ipv6Addr::new(0xfd7a, 0x115c, 0xa1e0, 0, 0, 0, 0, 1),
    )
}

/// The LocalAPI socket path, for tests that stand a stub in its place.
pub fn with_socket(socket: &Path, cli: &str) -> Tailscaled {
    Tailscaled {
        socket: socket.to_path_buf(),
        cli: cli.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailnet_addresses_are_recognized() {
        assert!(is_tailnet("100.64.0.1".parse().unwrap()));
        assert!(is_tailnet("100.127.255.254".parse().unwrap()));
        assert!(!is_tailnet("100.128.0.1".parse().unwrap()));
        assert!(!is_tailnet("100.63.255.255".parse().unwrap()));
        assert!(!is_tailnet("127.0.0.1".parse().unwrap()));
        assert!(!is_tailnet("192.168.1.2".parse().unwrap()));
        assert!(is_tailnet("fd7a:115c:a1e0::1".parse().unwrap()));
        assert!(!is_tailnet("fd7a:115c:a1e1::1".parse().unwrap()));
        assert!(!is_tailnet("::1".parse().unwrap()));
        let (v4, v6) = example_addresses();
        assert!(is_tailnet(v4.into()) && is_tailnet(v6.into()));
    }

    #[test]
    fn bind_refuses_everything_but_a_tailnet_address() {
        let err = bind(&["127.0.0.1".parse().unwrap(), "::1".parse().unwrap()], 0).unwrap_err();
        assert_eq!(
            err,
            "refusing to listen: none of 127.0.0.1, ::1 is a tailnet address"
        );
        let err = bind(&[], 0).unwrap_err();
        assert!(err.contains("(no address)"), "{err}");
        // A tailnet address this machine does not have cannot be bound, and
        // the message says where it tried; a loopback never gets tried.
        let err = bind(
            &["0.0.0.0".parse().unwrap(), "100.64.0.1".parse().unwrap()],
            0,
        )
        .unwrap_err();
        assert!(err.starts_with("cannot listen at 100.64.0.1:0"), "{err}");
    }
}
