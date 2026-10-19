use std::path::PathBuf;
use std::process::ExitCode;

use clap::error::ErrorKind;
use clap::{Parser, Subcommand};

use avsmd::exit;

/// The obedient paperclip maximizer's daemon: it pursues the goals you
/// give it, acts only within the limits you set, and has proof for
/// everything it does. It runs in the foreground until SIGTERM or SIGINT.
#[derive(Parser)]
#[command(name = "avsmd", version, about, long_about = None)]
struct Cli {
    /// Where avsmd keeps its files (default: $AVSMD_HOME, else
    /// ~/Library/Application Support/avsmd on macOS and
    /// $XDG_STATE_HOME/avsmd, or ~/.local/state/avsmd, elsewhere)
    #[arg(long, global = true, value_name = "DIR")]
    home: Option<PathBuf>,
    /// Test the config and every adopted mandate, say what each mandate
    /// lets avsmd do, and exit
    #[arg(short = 't')]
    test: bool,
    /// Print the config in force and exit
    #[arg(short = 'T')]
    show: bool,
    /// Read the config from this file instead of config.yaml in the home
    #[arg(short = 'f', global = true, value_name = "FILE")]
    file: Option<PathBuf>,
    /// Print verbose diagnostics to standard error
    #[arg(short = 'd', global = true)]
    debug: bool,
    #[command(subcommand)]
    process: Option<Process>,
}

/// The processes avsmd starts of itself.
#[derive(Subcommand)]
enum Process {
    /// The warden process
    #[command(hide = true)]
    Warden,
    /// The roles process
    #[command(hide = true)]
    Roles,
}

fn fail(e: exit::Exit) -> ExitCode {
    eprintln!("avsmd: {}", e.text.trim_end());
    ExitCode::from(e.code)
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return match e.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => ExitCode::SUCCESS,
                _ => ExitCode::from(exit::USAGE),
            };
        }
    };
    let home = cli.home.unwrap_or_else(avsmd::kernel::default_home);
    if let Some(file) = cli.file {
        let file = if file.is_absolute() {
            file
        } else {
            std::env::current_dir().unwrap_or_default().join(file)
        };
        avsmd::config::choose(file);
    }
    if cli.debug {
        avsmd::kernel::set_debug();
    }
    let ran = match cli.process {
        Some(Process::Warden) => avsmd::kernel::warden_main(&home).map_err(exit::Exit::from),
        Some(Process::Roles) => avsmd::kernel::roles_main().map_err(exit::Exit::from),
        None if cli.test => {
            let found = avsmd::doctor::test(&home);
            print!("{}", found.out);
            for p in &found.problems {
                eprintln!("avsmd: {p}");
            }
            return ExitCode::from(found.code);
        }
        None if cli.show => match avsmd::kernel::load_config(&home) {
            Ok(config) => match serde_yaml::to_string(&config) {
                Ok(text) => {
                    println!("# home {}", home.display());
                    println!("# config {}", avsmd::config::path(&home).display());
                    print!("{text}");
                    Ok(())
                }
                Err(e) => Err(exit::Exit::new(exit::SOFTWARE, e.to_string())),
            },
            Err(e) => Err(e),
        },
        None => avsmd::kernel::start(&home),
    };
    match ran {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(e),
    }
}
