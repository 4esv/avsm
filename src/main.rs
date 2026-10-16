use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// The obedient paperclip maximizer: a daemon that pursues the goals you
/// give it, acts only within the limits you set, and has proof for
/// everything it does.
#[derive(Parser)]
#[command(name = "avsm", version, about, long_about = None)]
struct Cli {
    /// Where avsm keeps its files (default: $AVSM_HOME, else ~/.avsm)
    #[arg(long, global = true, value_name = "DIR")]
    home: Option<PathBuf>,
    /// Talk to an avsm on another machine over Tailscale, as host:port
    #[arg(long, global = true, value_name = "HOST:PORT")]
    remote: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run avsm in the foreground: watch every venture and answer the other commands
    Start,
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
        #[arg(default_value_t = 20)]
        count: usize,
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
    /// The warden process; avsm start runs it for you
    #[command(hide = true)]
    Warden,
    /// The roles process; avsm start runs it for you
    #[command(hide = true)]
    Roles,
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
        /// Venture directories to take on first
        ventures: Vec<PathBuf>,
    },
}

fn done(result: Result<(), String>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("avsm: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The payload given on the command line, or read from standard input
/// when it is `-` or when nothing was given and input is not a terminal.
fn payload_argument(words: Vec<String>) -> Result<Option<String>, String> {
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
        .map_err(|e| format!("cannot read the payload: {e}"))?;
    Ok(if text.trim().is_empty() {
        None
    } else {
        Some(text)
    })
}

/// Fetch an ask's payload, let the human change it in their editor, and
/// approve with what they saved.
fn approve_edited(
    sock: &std::path::Path,
    remote: Option<&str>,
    number: &str,
) -> Result<(), String> {
    let send = |args: &[String]| match remote {
        Some(at) => avsm::protocol::request_remote(at, args),
        None => avsm::protocol::request(sock, args),
    };
    let args = vec!["payload".to_string(), number.to_string()];
    let answer = send(&args)?;
    if !answer.ok {
        return Err(answer.text.trim_end().to_string());
    }
    let (kind, content) = answer
        .text
        .split_once('\n')
        .ok_or_else(|| "avsm gave a bad answer".to_string())?;
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
    std::fs::write(&path, content).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("editor")
        .arg(&path)
        .status()
        .map_err(|e| format!("cannot run the editor ({editor}): {e}"))?;
    if !status.success() {
        let _ = std::fs::remove_file(&path);
        return Err("the editor did not finish; nothing approved".to_string());
    }
    let edited = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&path);
    let args = vec![
        "approve".to_string(),
        number.to_string(),
        "--payload".to_string(),
        edited,
    ];
    let answer = send(&args)?;
    print!("{}", answer.text);
    if answer.ok {
        Ok(())
    } else {
        Err("not approved".to_string())
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let home = cli.home.unwrap_or_else(avsm::kernel::default_home);
    let remote = cli.remote;
    let args: Vec<String> = match cli.command {
        Command::Start => return done(avsm::kernel::start(&home)),
        Command::Warden => return done(avsm::kernel::warden_main(&home)),
        Command::Roles => return done(avsm::kernel::roles_main()),
        Command::Simulate {
            days,
            step,
            approve,
            ventures,
        } => {
            let step = match humantime::parse_duration(&step) {
                Ok(d) => d.as_secs() as i64,
                Err(_) => return done(Err(format!("step is a duration like 1h, not {step:?}"))),
            };
            let human = match approve.as_str() {
                "all" => avsm::simulate::Human::ApproveAll,
                "none" => avsm::simulate::Human::Away,
                "mixed" => avsm::simulate::Human::Mixed,
                other => return done(Err(format!("approve is all, none or mixed, not {other:?}"))),
            };
            let plan = avsm::simulate::Plan {
                days,
                step,
                human,
                start: None,
                add: ventures,
            };
            return done(avsm::simulate::run(&home, &plan, &mut std::io::stdout()));
        }
        Command::Add { dir } => {
            let dir = dir.canonicalize().unwrap_or(dir);
            vec!["add".to_string(), dir.to_string_lossy().into_owned()]
        }
        Command::Status => vec!["status".to_string()],
        Command::Tail { count } => vec!["tail".to_string(), count.to_string()],
        Command::Asks => vec!["asks".to_string()],
        Command::Approve {
            number,
            edit: false,
        } => vec!["approve".to_string(), number],
        Command::Approve { number, edit: true } => {
            let sock = avsm::kernel::socket_path(&home);
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
    };
    let answer = match &remote {
        Some(at) => avsm::protocol::request_remote(at, &args),
        None => avsm::protocol::request(&avsm::kernel::socket_path(&home), &args),
    };
    match answer {
        Ok(answer) => {
            print!("{}", answer.text);
            if answer.ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            eprintln!("avsm: {e}");
            ExitCode::FAILURE
        }
    }
}
