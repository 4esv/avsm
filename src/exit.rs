//! Exit codes, as sysexits.h has them, and a failure that carries one.

/// The command line was used wrong.
pub const USAGE: u8 = 64;
/// The input data was wrong: a record, a ledger, an answer.
pub const DATAERR: u8 = 65;
/// An input file does not exist or cannot be read.
pub const NOINPUT: u8 = 66;
/// A service is not there: no daemon answering.
pub const UNAVAILABLE: u8 = 69;
/// Something went wrong inside.
pub const SOFTWARE: u8 = 70;
/// A file or directory cannot be created.
pub const CANTCREAT: u8 = 73;
/// Try again later.
pub const TEMPFAIL: u8 = 75;
/// Not permitted.
pub const NOPERM: u8 = 77;
/// The configuration is wrong: config.yaml or a mandate.
pub const CONFIG: u8 = 78;

/// A failure and the code to exit with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exit {
    pub code: u8,
    pub text: String,
}

impl Exit {
    pub fn new(code: u8, text: impl Into<String>) -> Exit {
        Exit {
            code,
            text: text.into(),
        }
    }
}

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// A failure that names no code is a failure inside.
impl From<String> for Exit {
    fn from(text: String) -> Exit {
        Exit::new(SOFTWARE, text)
    }
}

/// How severe a code is, to pick the one to exit with when several
/// things failed: a broken ledger before a broken config, a broken config
/// before a missing permission, a missing permission before a daemon
/// that is not running.
pub fn severity(code: u8) -> u8 {
    match code {
        0 => 0,
        SOFTWARE => 10,
        DATAERR => 9,
        CONFIG => 8,
        NOPERM => 7,
        CANTCREAT => 6,
        NOINPUT => 5,
        UNAVAILABLE => 4,
        TEMPFAIL => 3,
        USAGE => 2,
        _ => 1,
    }
}

/// The most severe of two codes.
pub fn worst(a: u8, b: u8) -> u8 {
    if severity(b) > severity(a) {
        b
    } else {
        a
    }
}
