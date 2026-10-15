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
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let home = cli.home.unwrap_or_else(avsm::kernel::default_home);
    let args: Vec<String> = match cli.command {
        Command::Start => {
            return match avsm::kernel::start(&home) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("avsm: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Command::Add { dir } => {
            let dir = dir.canonicalize().unwrap_or(dir);
            vec!["add".to_string(), dir.to_string_lossy().into_owned()]
        }
        Command::Status => vec!["status".to_string()],
        Command::Tail { count } => vec!["tail".to_string(), count.to_string()],
        Command::Asks => vec!["asks".to_string()],
        Command::Approve { number } => vec!["approve".to_string(), number],
        Command::Deny { number, reason } => {
            let mut v = vec!["deny".to_string(), number];
            v.extend(reason);
            v
        }
        Command::Pause { venture } => vec!["pause".to_string(), venture],
        Command::Resume { venture } => vec!["resume".to_string(), venture],
        Command::Why { number } => vec!["why".to_string(), number],
    };
    match avsm::protocol::request(&avsm::kernel::socket_path(&home), &args) {
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
