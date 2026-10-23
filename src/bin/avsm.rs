use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::error::ErrorKind;
use clap::{Parser, Subcommand};

use avsmd::exit::{self, Exit};

/// The obedient paperclip maximizer: talk to avsmd, the daemon that
/// pursues the goals you give it, acts only within the limits you set,
/// and has proof for everything it does.
#[derive(Parser)]
#[command(name = "avsm", version, about, long_about = None)]
struct Cli {
    /// Where avsmd keeps its files (default: $AVSMD_HOME, else
    /// ~/Library/Application Support/avsmd on macOS and
    /// $XDG_STATE_HOME/avsmd, or ~/.local/state/avsmd, elsewhere)
    #[arg(long, global = true, value_name = "DIR")]
    home: Option<PathBuf>,
    /// Talk to avsmd on another machine over Tailscale, as host:port
    #[arg(long, global = true, value_name = "HOST:PORT")]
    remote: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Take on a venture: a directory with a mandate.yaml in it
    Add {
        /// The venture directory
        dir: PathBuf,
    },
    /// Show every venture and what it is doing
    Status,
    /// Show the latest records
    Tail {
        /// How many records to show
        #[arg(default_value_t = 20, conflicts_with = "since")]
        count: usize,
        /// Show every record after this record number instead
        #[arg(long, value_name = "NUMBER")]
        since: Option<String>,
    },
    /// List the questions waiting for you
    Asks,
    /// Say yes to a question, by its record number
    Approve {
        /// The record number of the question
        number: String,
        /// Open the content the lever would consume in your editor first; what you save is what runs
        #[arg(long)]
        edit: bool,
    },
    /// Say no to a question, by its record number, with a reason
    Deny {
        /// The record number of the question
        number: String,
        /// Why not
        reason: Vec<String>,
    },
    /// Stop a venture; it stays stopped until you resume it
    Pause {
        /// The venture's name
        venture: String,
    },
    /// Let a paused venture run again
    Resume {
        /// The venture's name
        venture: String,
    },
    /// Show the proof behind a record, by its number
    Why {
        /// The record number
        number: String,
    },
    /// Show what happened since the last digest: actions, asks, cost, one line per venture
    Digest,
    /// Show what is held true now about a venture, or a topic across ventures
    Know {
        /// A venture's name, or a topic like pricing or digest
        what: String,
    },
    /// Propose an action yourself; it still has to pass the gate
    Propose {
        /// The venture's name
        venture: String,
        /// The name of one of its permitted actions
        lever: String,
        /// What the action consumes, if it takes something; - reads it from standard input
        payload: Vec<String>,
    },
    /// Forecast the ventures over the next days from forked copies of the ledger; nothing real runs
    Forecast {
        /// The ventures to forecast (default: every one)
        ventures: Vec<String>,
        /// How many days ahead (default: forecast.days in config.yaml, 30)
        #[arg(long)]
        days: Option<u32>,
        /// Seeds per policy (default: forecast.seeds in config.yaml, 16)
        #[arg(long)]
        seeds: Option<u32>,
        /// Try a mandate field at several values, like: --sweep permitted.water.cap 2/day,4/day,8/day
        #[arg(long, num_args = 2, value_names = ["FIELD", "VALUES"])]
        sweep: Option<Vec<String>>,
    },
    /// Check this machine's avsmd: its home, config, ledger, mandates and
    /// the daemon itself; change nothing, and say how to fix what fails
    Doctor {
        /// Check that the ledger still holds this head, exactly as the
        /// digest's last line prints it: kept off this machine, it catches
        /// a rewrite by the file's owner or a rewind
        #[arg(long, value_name = "HEAD")]
        since: Option<String>,
        /// With avsmd stopped, back the ledger up whole and cut it just
        /// before this record, keeping every record before it; the cut is
        /// recorded. The one thing doctor writes
        #[arg(long, value_name = "NUMBER", conflicts_with = "since")]
        cut: Option<String>,
        /// Cut a ledger that has no bad record, or cut before record 1
        #[arg(long, requires = "cut")]
        force: bool,
    },
    /// Run the ventures in a home of their own over simulated days
    #[command(hide = true)]
    Simulate {
        /// How many days to simulate
        #[arg(long, default_value_t = 7)]
        days: u32,
        /// How far the clock jumps each step, like 1h or 15m
        #[arg(long, default_value = "1h")]
        step: String,
        /// Who answers the asks: all (approve every one), none, or mixed
        /// (deny every third, edit text payloads, approve the rest)
        #[arg(long, default_value = "all")]
        approve: String,
        /// A command run before every step to move the world on; it gets
        /// the simulated time in AVSM_NOW and the day in AVSM_DAY. Every
        /// lever run goes to it instead of the lever, with AVSM_LEVER,
        /// AVSM_KEY and the payload on stdin; without it, levers step the
        /// venture's world model
        #[arg(long)]
        world: Option<String>,
        /// Unix seconds to start at on an empty ledger, so two runs see
        /// the same world
        #[arg(long)]
        start: Option<i64>,
        /// Venture directories to take on first
        ventures: Vec<PathBuf>,
    },
}

fn done(result: Result<(), Exit>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            avsmd::protocol::elog(&format!("avsm: {}", e.text.trim_end()));
            ExitCode::from(e.code)
        }
    }
}

/// A command avsmd answered with a no: the input named something it
/// could not take.
fn refused(text: impl Into<String>) -> Exit {
    Exit::new(exit::DATAERR, text)
}

/// The payload given on the command line, or read from standard input
/// when it is `-` or when nothing was given and input is not a terminal.
fn payload_argument(words: Vec<String>) -> Result<Option<String>, Exit> {
    let joined = words.join(" ");
    let from_stdin = joined == "-" || (joined.is_empty() && !std::io::stdin().is_terminal());
    if !from_stdin {
        return Ok(if joined.is_empty() {
            None
        } else {
            Some(joined)
        });
    }
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .map_err(|e| Exit::new(exit::NOINPUT, format!("cannot read the payload: {e}")))?;
    Ok(if text.trim().is_empty() {
        None
    } else {
        Some(text)
    })
}

/// Fetch an ask's payload, let the human change it in their editor, and
/// approve with what they saved.
fn approve_edited(sock: &std::path::Path, remote: Option<&str>, number: &str) -> Result<(), Exit> {
    let send = |args: &[String]| match remote {
        Some(at) => avsmd::protocol::request_remote(at, args),
        None => avsmd::protocol::request(sock, args),
    };
    let args = vec!["payload".to_string(), number.to_string()];
    let answer = send(&args)?;
    if !answer.ok {
        return Err(refused(answer.text.trim_end()));
    }
    let (kind, content) = answer
        .text
        .split_once('\n')
        .ok_or_else(|| Exit::new(exit::SOFTWARE, "avsmd gave a bad answer"))?;
    let ext = match kind.trim() {
        "json" => "json",
        "diff" => "diff",
        _ => "txt",
    };
    let path = std::env::temp_dir().join(format!(
        "avsm-{}-{}.{ext}",
        number.trim_start_matches('#'),
        std::process::id()
    ));
    std::fs::write(&path, content).map_err(|e| {
        Exit::new(
            exit::CANTCREAT,
            format!("cannot write {}: {e}", path.display()),
        )
    })?;
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("editor")
        .arg(&path)
        .status()
        .map_err(|e| {
            Exit::new(
                exit::UNAVAILABLE,
                format!("cannot run the editor ({editor}): {e}"),
            )
        })?;
    if !status.success() {
        let _ = std::fs::remove_file(&path);
        return Err(Exit::new(
            exit::TEMPFAIL,
            "the editor did not finish; nothing approved",
        ));
    }
    let edited = std::fs::read_to_string(&path).map_err(|e| {
        Exit::new(
            exit::NOINPUT,
            format!("cannot read {}: {e}", path.display()),
        )
    })?;
    let _ = std::fs::remove_file(&path);
    let args = vec![
        "approve".to_string(),
        number.to_string(),
        "--payload".to_string(),
        edited,
    ];
    let answer = send(&args)?;
    avsmd::protocol::emit(&answer.text);
    if answer.ok {
        Ok(())
    } else {
        Err(refused("not approved"))
    }
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            // `avsm approve #30` in zsh is `avsm approve`: # starts a comment.
            let numbered =
                std::env::args().any(|a| matches!(a.as_str(), "approve" | "deny" | "why"));
            if e.kind() == ErrorKind::MissingRequiredArgument && numbered {
                avsmd::protocol::elog("hint: zsh treats # as a comment; write 12 or '#12'");
            }
            return match e.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => ExitCode::SUCCESS,
                _ => ExitCode::from(exit::USAGE),
            };
        }
    };
    let home = cli.home.unwrap_or_else(avsmd::kernel::default_home);
    let remote = cli.remote;
    // A mistyped --home is not "avsmd is not running".
    let needs_home = |remote: &Option<String>| {
        if remote.is_none() && !home.is_dir() {
            Err(Exit::new(
                exit::NOINPUT,
                format!(
                    "no home at {}; check --home or AVSMD_HOME, or start avsmd there with: avsmd --home {}",
                    home.display(),
                    home.display()
                ),
            ))
        } else {
            Ok(())
        }
    };
    let args: Vec<String> = match cli.command {
        Command::Doctor { since, cut, force } => {
            if remote.is_some() {
                return done(Err(Exit::new(
                    exit::USAGE,
                    "doctor checks this machine; run it where avsmd runs",
                )));
            }
            if let Some(seq) = cut {
                let seq = match seq.trim_start_matches('#').parse::<u64>() {
                    Ok(n) if n > 0 => n,
                    _ => {
                        return done(Err(Exit::new(
                            exit::USAGE,
                            format!("--cut takes a record number, not {seq:?}"),
                        )))
                    }
                };
                return done(
                    avsmd::doctor::cut(&home, seq, force).map(|text| avsmd::protocol::emit(&text)),
                );
            }
            let since = match since.as_deref().map(avsmd::doctor::parse_anchor) {
                Some(Err(e)) => return done(Err(Exit::new(exit::USAGE, e))),
                Some(Ok(anchor)) => Some(anchor),
                None => None,
            };
            let found = avsmd::doctor::doctor(
                &home,
                avsmd::kernel::old_home(|v| std::env::var_os(v)).as_deref(),
                since.as_ref(),
            );
            avsmd::protocol::emit(&found.out);
            return ExitCode::from(found.code);
        }
        Command::Simulate {
            days,
            step,
            approve,
            world,
            start,
            ventures,
        } => {
            let step = match humantime::parse_duration(&step) {
                Ok(d) => d.as_secs() as i64,
                Err(_) => {
                    return done(Err(Exit::new(
                        exit::USAGE,
                        format!("step is a duration like 1h, not {step:?}"),
                    )))
                }
            };
            let human = match approve.as_str() {
                "all" => avsmd::simulate::Human::ApproveAll,
                "none" => avsmd::simulate::Human::Away,
                "mixed" => avsmd::simulate::Human::Mixed,
                other => {
                    return done(Err(Exit::new(
                        exit::USAGE,
                        format!("approve is all, none or mixed, not {other:?}"),
                    )))
                }
            };
            let plan = avsmd::simulate::Plan {
                days,
                step,
                human,
                start,
                add: ventures,
                world,
            };
            return done(
                avsmd::simulate::run(&home, &plan, &mut avsmd::protocol::Stdout)
                    .map_err(Exit::from),
            );
        }
        Command::Add { dir } => {
            // A path on this machine is checked here, so a missing one
            // exits 66 as doctor does; a remote avsmd checks its own.
            if remote.is_none() {
                match avsmd::venture::dir_problem(&dir) {
                    Some(avsmd::venture::DirProblem::Missing(t)) => {
                        return done(Err(Exit::new(exit::NOINPUT, t)))
                    }
                    Some(avsmd::venture::DirProblem::Wrong(t)) => {
                        return done(Err(Exit::new(exit::DATAERR, t)))
                    }
                    None => {}
                }
            }
            let dir = dir.canonicalize().unwrap_or(dir);
            vec!["add".to_string(), dir.to_string_lossy().into_owned()]
        }
        Command::Status => vec!["status".to_string()],
        Command::Tail {
            since: Some(seq), ..
        } => vec!["tail".to_string(), "--since".to_string(), seq],
        Command::Tail { count, .. } => vec!["tail".to_string(), count.to_string()],
        Command::Asks => vec!["asks".to_string()],
        Command::Approve {
            number,
            edit: false,
        } => vec!["approve".to_string(), number],
        Command::Approve { number, edit: true } => {
            if let Err(e) = needs_home(&remote) {
                return done(Err(e));
            }
            let sock = avsmd::kernel::socket_path(&home);
            return done(approve_edited(&sock, remote.as_deref(), &number));
        }
        Command::Propose {
            venture,
            lever,
            payload,
        } => {
            let mut v = vec!["propose".to_string(), venture, lever];
            match payload_argument(payload) {
                Ok(Some(text)) => v.push(text),
                Ok(None) => {}
                Err(e) => return done(Err(e)),
            }
            v
        }
        Command::Deny { number, reason } => {
            let mut v = vec!["deny".to_string(), number];
            v.extend(reason);
            v
        }
        Command::Pause { venture } => vec!["pause".to_string(), venture],
        Command::Resume { venture } => vec!["resume".to_string(), venture],
        Command::Why { number } => vec!["why".to_string(), number],
        Command::Know { what } => vec!["know".to_string(), what],
        Command::Digest => vec!["digest".to_string()],
        Command::Forecast {
            ventures,
            days,
            seeds,
            sweep,
        } => {
            let mut v = vec!["forecast".to_string()];
            v.extend(ventures);
            if let Some(d) = days {
                v.extend(["--days".to_string(), d.to_string()]);
            }
            if let Some(k) = seeds {
                v.extend(["--seeds".to_string(), k.to_string()]);
            }
            if let Some(s) = sweep {
                v.push("--sweep".to_string());
                v.extend(s);
            }
            v
        }
    };
    if let Err(e) = needs_home(&remote) {
        return done(Err(e));
    }
    let answer = match &remote {
        Some(at) => avsmd::protocol::request_remote(at, &args),
        None => avsmd::protocol::request(&avsmd::kernel::socket_path(&home), &args),
    };
    match answer {
        Ok(answer) if answer.ok => {
            avsmd::protocol::emit(&answer.text);
            ExitCode::SUCCESS
        }
        // A no is a diagnostic: it goes to standard error.
        Ok(answer) => done(Err(refused(answer.text))),
        Err(e) => done(Err(e)),
    }
}
