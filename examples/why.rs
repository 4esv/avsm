//! `why` over a ledger offline, for analysis: a dev tool, not a command.
//!
//!     cargo run --example why -- <home> <seq>...
//!
//! It opens the ledger through `Kernel::open`, the path every command
//! takes, so it is no more read-only than they are. Run it on a copy of a
//! home, never on a live daemon's.

use std::path::Path;

use avsmd::kernel::Kernel;
use avsmd::record::Seq;

/// `why` for each seq, one after another.
fn run(home: &Path, seqs: &[String]) -> Result<String, String> {
    if !home.join("ledger.db").is_file() {
        return Err(format!("no ledger at {}", home.join("ledger.db").display()));
    }
    let kernel = Kernel::open(home)?;
    let mut out = String::new();
    for s in seqs {
        let seq: Seq = s
            .trim_start_matches('#')
            .parse()
            .map_err(|_| format!("a seq is a number like 42, not {s:?}"))?;
        out.push_str(&format!("#{seq}\n{}\n", kernel.why(seq)?));
    }
    Ok(out)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((home, seqs)) = args.split_first().filter(|(_, s)| !s.is_empty()) else {
        eprintln!("usage: cargo run --example why -- <home> <seq>...");
        std::process::exit(2);
    };
    match run(Path::new(home), seqs) {
        Ok(text) => print!("{text}"),
        Err(e) => {
            eprintln!("why: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use avsmd::ledger::Ledger;
    use avsmd::record::{Claim, Record};

    #[test]
    fn it_prints_why_for_each_seq_of_a_ledger() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        assert!(run(home, &["1".into()]).unwrap_err().contains("no ledger"));
        let mut ledger = Ledger::open(&home.join("ledger.db")).unwrap();
        ledger
            .append(Record::report(Claim::Measured, "c", "count", Some(3.0), ""))
            .unwrap();
        drop(ledger);
        let text = run(home, &["#1".into()]).unwrap();
        assert!(text.starts_with("#1\n"), "{text}");
        assert!(run(home, &["one".into()]).unwrap_err().contains("a seq"));
    }
}
