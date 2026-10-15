//! The kernel: holds the ledger and the ventures, answers commands, and
//! (issue 5) supervises ventures and runs the nightly cycle.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::gate::{self, Adopted, Candidate};
use crate::ledger::Ledger;
use crate::record::{Claim, Entry, Record, Seq, Verdict};
use crate::venture::{self, Venture};

/// The kernel's working state.
pub struct Kernel {
    pub home: PathBuf,
    pub ledger: Ledger,
    pub ventures: BTreeMap<String, Venture>,
}

/// Unix seconds now.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Where avsm keeps its files: `$AVSM_HOME`, else `~/.avsm`.
pub fn default_home() -> PathBuf {
    if let Some(h) = std::env::var_os("AVSM_HOME") {
        return PathBuf::from(h);
    }
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join(".avsm")
}

/// The socket inside a home directory.
pub fn socket_path(home: &Path) -> PathBuf {
    home.join("sock")
}

impl Kernel {
    /// Open the ledger in `home` (created if missing) and rebuild the
    /// ventures from the declared records in it.
    pub fn open(home: &Path) -> Result<Kernel, String> {
        std::fs::create_dir_all(home)
            .map_err(|e| format!("cannot create {}: {e}", home.display()))?;
        let ledger = Ledger::open(&home.join("ledger.db")).map_err(|e| e.to_string())?;
        let mut kernel = Kernel {
            home: home.to_path_buf(),
            ledger,
            ventures: BTreeMap::new(),
        };
        kernel.reload()?;
        Ok(kernel)
    }

    /// Rebuild the ventures from the latest declared Adopt record of each.
    fn reload(&mut self) -> Result<(), String> {
        let entries = self.entries()?;
        self.ventures = venture::load_all(&entries);
        Ok(())
    }

    /// Every entry, oldest first.
    pub fn entries(&self) -> Result<Vec<Entry>, String> {
        self.ledger.all().map_err(|e| e.to_string())
    }

    /// The mandates the gate may use: adopted and unchanged.
    pub fn adopted(&self) -> Vec<Adopted> {
        venture::adopted(&self.ventures)
    }

    /// Append a record now.
    pub fn append(&mut self, record: Record) -> Result<Entry, String> {
        self.ledger.append(record).map_err(|e| e.to_string())
    }

    /// Run one command from the socket. The first argument is the command.
    pub fn handle(&mut self, args: &[String]) -> Result<String, String> {
        let (cmd, rest) = args
            .split_first()
            .ok_or_else(|| "no command given".to_string())?;
        let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
        match (cmd.as_str(), rest.as_slice()) {
            ("add", [dir]) => self.add(Path::new(dir)),
            ("status", []) => self.status(),
            ("tail", []) => self.tail(20),
            ("tail", [n]) => self.tail(
                n.parse()
                    .map_err(|_| format!("tail takes a count, not {n:?}"))?,
            ),
            ("asks", []) => self.asks(),
            ("approve", [seq]) => self.approve(parse_seq(seq)?),
            ("deny", [seq, reason @ ..]) => self.deny(parse_seq(seq)?, &reason.join(" ")),
            ("pause", [name]) => self.pause(name),
            ("resume", [name]) => self.resume(name),
            ("why", [seq]) => self.why(parse_seq(seq)?),
            _ => Err(format!("avsm does not understand: {}", args.join(" "))),
        }
    }

    /// Take on the venture in a directory: adopt its mandate.
    pub fn add(&mut self, dir: &Path) -> Result<String, String> {
        let dir = dir
            .canonicalize()
            .map_err(|e| format!("cannot find {}: {e}", dir.display()))?;
        let name = venture::name_of(&dir)?;
        let (mandate, hash) = venture::read_mandate(&dir)?;
        let entry = self.append(Record::Decision {
            claim: Claim::Declared,
            venture: name.clone(),
            verdict: Verdict::Adopt {
                mandate: hash.clone(),
                path: dir.to_string_lossy().into_owned(),
            },
        })?;
        let v = Venture {
            name: name.clone(),
            path: Some(dir.clone()),
            mandate: mandate.clone(),
            hash,
            adopted: entry.seq,
            problem: None,
        };
        self.ventures.insert(name.clone(), v);
        let mut out = format!("added {name} from {}\n", dir.display());
        out.push_str(&format!("  {}\n", mandate.purpose.trim()));
        out.push_str(&format!(
            "  watching {} every {}\n",
            mandate.metric,
            humantime::format_duration(mandate.cadence)
        ));
        for p in &mandate.permitted {
            out.push_str(&format!(
                "  may {}  {}  {}\n",
                p.name,
                p.cap,
                if p.reversible {
                    "reversible"
                } else if p.delegated {
                    "irreversible, delegated"
                } else {
                    "irreversible, will ask first"
                }
            ));
        }
        out.push_str(&format!("  {}\n", entry.line()));
        Ok(out)
    }

    /// One line per venture.
    pub fn status(&self) -> Result<String, String> {
        if self.ventures.is_empty() {
            return Ok("no ventures; add one with: avsm add <directory>\n".to_string());
        }
        let entries = self.entries()?;
        let now = now();
        let mut out = String::new();
        for v in self.ventures.values() {
            let state = gate::state(&entries, &v.name);
            let last = entries.iter().rev().find(|e| {
                e.record.venture() == v.name && matches!(e.record, Record::Report { .. })
            });
            let detail = match &v.problem {
                Some(p) => p.clone(),
                None => match last {
                    Some(e) => format!("{}  reported {}", e.record.summary(), ago(now - e.at)),
                    None => "no report yet".to_string(),
                },
            };
            out.push_str(&format!(
                "{:<12} {:<8} {detail}\n",
                v.name,
                state.to_string()
            ));
        }
        Ok(out)
    }

    /// The last `n` records, one line each.
    pub fn tail(&self, n: usize) -> Result<String, String> {
        let entries = self.ledger.tail(n).map_err(|e| e.to_string())?;
        if entries.is_empty() {
            return Ok("the ledger is empty\n".to_string());
        }
        Ok(entries.iter().map(|e| e.line() + "\n").collect())
    }

    /// The questions waiting for a human, one line each.
    pub fn asks(&self) -> Result<String, String> {
        let entries = self.entries()?;
        let open = gate::open_asks(&entries);
        if open.is_empty() {
            return Ok("no open asks\n".to_string());
        }
        Ok(entries
            .iter()
            .filter(|e| open.contains(&e.seq))
            .map(|e| e.line() + "\n")
            .collect())
    }

    fn open_ask(&self, seq: Seq) -> Result<Entry, String> {
        let entry = self.entry(seq)?;
        let entries = self.entries()?;
        if !gate::open_asks(&entries).contains(&seq) {
            return Err(format!("#{seq} is not an open ask\n{}", entry.line()));
        }
        Ok(entry)
    }

    /// Say yes to an ask.
    pub fn approve(&mut self, seq: Seq) -> Result<String, String> {
        let ask = self.open_ask(seq)?;
        let entry = self.append(Record::Decision {
            claim: Claim::Declared,
            venture: ask.record.venture().to_string(),
            verdict: Verdict::Approve { ask: seq },
        })?;
        Ok(entry.line() + "\n")
    }

    /// Say no to an ask.
    pub fn deny(&mut self, seq: Seq, reason: &str) -> Result<String, String> {
        let ask = self.open_ask(seq)?;
        let reason = if reason.trim().is_empty() {
            "no reason given".to_string()
        } else {
            reason.trim().to_string()
        };
        let entry = self.append(Record::Decision {
            claim: Claim::Declared,
            venture: ask.record.venture().to_string(),
            verdict: Verdict::Deny { ask: seq, reason },
        })?;
        Ok(entry.line() + "\n")
    }

    fn known(&self, name: &str) -> Result<(), String> {
        if self.ventures.contains_key(name) || name == gate::SELF {
            Ok(())
        } else {
            Err(format!("no venture named {name}; see: avsm status"))
        }
    }

    /// Stop a venture until a human resumes it.
    pub fn pause(&mut self, name: &str) -> Result<String, String> {
        self.known(name)?;
        let entry = self.append(Record::Decision {
            claim: Claim::Declared,
            venture: name.to_string(),
            verdict: Verdict::Pause,
        })?;
        Ok(entry.line() + "\n")
    }

    /// Let a paused venture run again.
    pub fn resume(&mut self, name: &str) -> Result<String, String> {
        self.known(name)?;
        let entry = self.append(Record::Decision {
            claim: Claim::Declared,
            venture: name.to_string(),
            verdict: Verdict::Resume,
        })?;
        Ok(entry.line() + "\n")
    }

    fn entry(&self, seq: Seq) -> Result<Entry, String> {
        self.ledger
            .get(seq)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no record #{seq}"))
    }

    /// The proof behind a record. A proposal is gated afresh, so the text
    /// says what would happen if it were acted on now.
    pub fn why(&self, seq: Seq) -> Result<String, String> {
        let entry = self.entry(seq)?;
        if let Record::Proposal {
            venture, action, ..
        } = &entry.record
        {
            let entries = self.entries()?;
            let candidate = Candidate {
                venture: venture.clone(),
                action: action.clone(),
                cites: vec![seq],
            };
            let derivation = match gate::gate(&candidate, &entries, &self.adopted(), now()) {
                Ok(proof) => proof.to_string(),
                Err(refusal) => refusal.to_string(),
            };
            return Ok(format!("{}\n{derivation}", entry.line()));
        }
        Ok(gate::why(&entry))
    }
}

fn parse_seq(s: &str) -> Result<Seq, String> {
    s.trim_start_matches('#')
        .parse()
        .map_err(|_| format!("a record number looks like 12 or #12, not {s:?}"))
}

/// A duration in seconds as "3s ago", "5m ago", "2h ago", "3d ago".
pub fn ago(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s ago")
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else if s < 86_400 {
        format!("{}h ago", s / 3600)
    } else {
        format!("{}d ago", s / 86_400)
    }
}

/// Run avsm in the foreground: answer commands on the socket. Issue 5 adds
/// venture supervision and the nightly cycle.
pub fn start(home: &Path) -> Result<(), String> {
    let kernel = Kernel::open(home)?;
    let sock = socket_path(home);
    let listener = crate::protocol::bind(&sock)?;
    let kernel = Arc::new(Mutex::new(kernel));
    eprintln!("avsm: listening at {}", sock.display());
    crate::protocol::serve(listener, kernel);
    Ok(())
}
