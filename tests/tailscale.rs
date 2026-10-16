//! Remote access with tailscaled stood in for: a stub whois decides who the
//! peer is, the daemon lets in only listed logins, and declared records
//! carry the login. The LocalAPI client is tried against a stub socket and
//! the `tailscale` command fallback against a stub script.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use avsm::kernel::Kernel;
use avsm::protocol::request_remote;
use avsm::tailscale::{self, Tailscaled, Whois};

/// Says what it was told to say about an address.
struct StubWhois(HashMap<String, Result<String, String>>);

impl Whois for StubWhois {
    fn whois(&self, peer: SocketAddr) -> Result<String, String> {
        self.0
            .get(&peer.ip().to_string())
            .cloned()
            .unwrap_or_else(|| Err("unknown peer".to_string()))
    }
}

fn serve_with(home: &Path, whois: StubWhois, allowed: &[&str]) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let at = listener.local_addr().unwrap().to_string();
    let kernel = Arc::new(Mutex::new(Kernel::open(home).unwrap()));
    let allowed: Vec<String> = allowed.iter().map(|s| s.to_string()).collect();
    std::thread::spawn(move || tailscale::serve(listener, Box::new(whois), allowed, kernel));
    at
}

fn args(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

#[test]
fn listed_logins_get_in_and_their_records_say_who() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let mut known = HashMap::new();
    known.insert("127.0.0.1".to_string(), Ok("alice@github".to_string()));
    let at = serve_with(
        &home,
        StubWhois(known),
        &["alice@github", "bob@example.com"],
    );

    let answer = request_remote(&at, &args(&["status"])).unwrap();
    assert!(answer.ok, "{}", answer.text);
    assert!(answer.text.starts_with("no ventures"), "{}", answer.text);

    let answer = request_remote(&at, &args(&["pause", "self"])).unwrap();
    assert!(answer.ok, "{}", answer.text);
    assert!(
        answer
            .text
            .contains("self         declared  decision  paused  by alice@github"),
        "{}",
        answer.text
    );
    let seq = answer
        .text
        .trim_start_matches('#')
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    let answer = request_remote(&at, &args(&["why", &seq])).unwrap();
    assert!(answer.ok);
    assert!(
        answer
            .text
            .contains("declared by alice@github, over the tailnet"),
        "{}",
        answer.text
    );
    let answer = request_remote(&at, &args(&["resume", "self"])).unwrap();
    assert!(
        answer.text.contains("resumed  by alice@github"),
        "{}",
        answer.text
    );

    // The command carries no identity of its own, so none can be claimed.
    let answer = request_remote(&at, &args(&["pause", "self", "--as", "bob@example.com"])).unwrap();
    assert!(!answer.ok);
    assert!(
        answer.text.contains("does not understand"),
        "{}",
        answer.text
    );

    // The same commands over the socket in the same ledger name nobody.
    let kernel = Kernel::open(&home).unwrap();
    let text = kernel.tail(5).unwrap();
    assert!(text.contains("paused  by alice@github"), "{text}");
    assert_eq!(text.matches("by alice@github").count(), 2, "{text}");
    assert_eq!(kernel.ledger.verify().unwrap(), 2);
}

#[test]
fn unlisted_and_unknown_peers_are_refused_before_any_command_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let mut known = HashMap::new();
    known.insert("127.0.0.1".to_string(), Ok("mallory@github".to_string()));
    let at = serve_with(&home, StubWhois(known), &["alice@github"]);
    let answer = request_remote(&at, &args(&["pause", "self"])).unwrap();
    assert!(!answer.ok);
    assert_eq!(answer.text, "mallory@github is not in remote_users\n");

    let at = serve_with(&home, StubWhois(HashMap::new()), &["alice@github"]);
    let answer = request_remote(&at, &args(&["pause", "self"])).unwrap();
    assert!(!answer.ok);
    assert_eq!(answer.text, "cannot tell who 127.0.0.1 is: unknown peer\n");

    let kernel = Kernel::open(&home).unwrap();
    assert!(kernel.ledger.is_empty().unwrap(), "nothing was written");
}

#[test]
fn the_binary_reaches_a_remote_daemon_with_a_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let mut known = HashMap::new();
    known.insert("127.0.0.1".to_string(), Ok("alice@github".to_string()));
    let at = serve_with(&home, StubWhois(known), &["alice@github"]);
    let out = Command::new(env!("CARGO_BIN_EXE_avsm"))
        .args(["--remote", &at, "pause", "self"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("paused  by alice@github"), "{text}");
    let out = Command::new(env!("CARGO_BIN_EXE_avsm"))
        .args(["--remote", "127.0.0.1:1", "status"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cannot reach avsm at 127.0.0.1:1"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A stub tailscaled: answers LocalAPI GETs on a unix socket and remembers
/// the requests.
fn stub_tailscaled(dir: &Path) -> (std::path::PathBuf, Arc<Mutex<Vec<String>>>) {
    let sock = dir.join("tailscaled.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let path = req.split_whitespace().nth(1).unwrap_or("").to_string();
            let (status, body) = if path == "/localapi/v0/status" {
                (
                    200,
                    r#"{"Self":{"TailscaleIPs":["100.101.102.103","fd7a:115c:a1e0::1"]}}"#,
                )
            } else if path.starts_with("/localapi/v0/whois?addr=100.64.0.9") {
                (
                    200,
                    r#"{"Node":{"Name":"laptop"},"UserProfile":{"LoginName":"alice@github","DisplayName":"Alice"}}"#,
                )
            } else if path.starts_with("/localapi/v0/whois") {
                (404, "no such peer")
            } else {
                (400, "bad request")
            };
            let _ = stream.write_all(
                format!("HTTP/1.0 {status} X\r\nContent-Type: application/json\r\n\r\n{body}")
                    .as_bytes(),
            );
            log.lock().unwrap().push(req);
        }
    });
    (sock, seen)
}

#[test]
fn the_localapi_client_asks_tailscaled_and_falls_back_to_the_command() {
    let tmp = tempfile::tempdir().unwrap();
    let (sock, seen) = stub_tailscaled(tmp.path());
    let daemon = tailscale::with_socket(&sock, "/nonexistent/tailscale");
    assert!(daemon.present());
    let addresses = daemon.addresses().unwrap();
    assert_eq!(addresses.len(), 2);
    assert_eq!(addresses[0].to_string(), "100.101.102.103");
    let login = daemon.whois("100.64.0.9:41000".parse().unwrap()).unwrap();
    assert_eq!(login, "alice@github");
    let err = daemon.whois("100.64.0.10:1".parse().unwrap()).unwrap_err();
    assert!(err.contains("answered 404"), "{err}");
    assert!(err.contains("cannot run /nonexistent/tailscale"), "{err}");
    let requests = seen.lock().unwrap().clone();
    assert!(
        requests[0].starts_with("GET /localapi/v0/status HTTP/1.0\r\n"),
        "{}",
        requests[0]
    );
    assert!(
        requests[0].contains("Sec-Tailscale: localapi\r\n"),
        "{}",
        requests[0]
    );
    assert!(
        requests[1].starts_with("GET /localapi/v0/whois?addr=100.64.0.9:41000 HTTP/1.0"),
        "{}",
        requests[1]
    );

    // No socket: the `tailscale` command answers instead.
    let cli = tmp.path().join("tailscale");
    std::fs::write(
        &cli,
        "#!/bin/sh\ncase \"$1 $2\" in\n\
         \"whois --json\") printf '%s' '{\"UserProfile\":{\"LoginName\":\"bob@example.com\"}}';;\n\
         \"status --json\") printf '%s' '{\"Self\":{\"TailscaleIPs\":[\"100.64.0.2\"]}}';;\n\
         *) echo unknown >&2; exit 1;;\nesac\n",
    )
    .unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
    let daemon = tailscale::with_socket(&tmp.path().join("missing.sock"), cli.to_str().unwrap());
    assert!(daemon.present());
    assert_eq!(daemon.addresses().unwrap()[0].to_string(), "100.64.0.2");
    assert_eq!(
        daemon.whois("100.64.0.9:41000".parse().unwrap()).unwrap(),
        "bob@example.com"
    );

    // Neither: not present, and every answer says so.
    let daemon = tailscale::with_socket(&tmp.path().join("missing.sock"), "/nonexistent/tailscale");
    assert!(!daemon.present());
    let err = daemon.addresses().unwrap_err();
    assert!(err.contains("cannot reach tailscaled"), "{err}");
    let _ = Tailscaled::default;
}

#[test]
fn without_tailscaled_the_daemon_says_so_and_keeps_its_socket() {
    if Path::new(tailscale::LOCALAPI_SOCKET).exists() {
        eprintln!("tailscaled is running here; skipping the absent case");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.yaml"),
        "listen: tailscale\nremote_users: [alice@github]\n",
    )
    .unwrap();
    let log = std::fs::File::create(tmp.path().join("log")).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_avsm"))
        .arg("--home")
        .arg(&home)
        .arg("start")
        .env("PATH", "/nonexistent")
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .unwrap();
    let sock = avsm::kernel::socket_path(&home);
    let began = Instant::now();
    while !avsm::protocol::alive(&sock) {
        assert!(
            began.elapsed() < Duration::from_secs(20),
            "avsm did not start"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = Command::new(env!("CARGO_BIN_EXE_avsm"))
        .arg("--home")
        .arg(&home)
        .arg("status")
        .output()
        .unwrap();
    assert!(out.status.success());
    let _ = child.kill();
    let _ = child.wait();
    let log = std::fs::read_to_string(tmp.path().join("log")).unwrap();
    assert!(
        log.contains("listen: tailscale is set, but tailscaled is not running"),
        "{log}"
    );
    assert!(log.contains("the remote listener is off"), "{log}");
}
