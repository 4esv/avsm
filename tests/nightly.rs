//! The nightly digest: a record each nightly, as the README says, even
//! when one was written earlier the same day (at first start, or under
//! another nightly time). Monkey round 3 of #55 set `nightly: "10:59"`;
//! the model calls ran at 10:59, and the only digest record was the one
//! written at first start.

use std::path::Path;

use avsmd::kernel::Kernel;
use avsmd::record::{Claim, Record};

/// 2026-01-01T00:00:00Z, a Thursday.
const DAY0: i64 = 1_767_225_600;
const HOUR: i64 = 3_600;
const MINUTE: i64 = 60;

fn digests(home: &Path) -> Vec<i64> {
    Kernel::open(home)
        .unwrap()
        .entries()
        .unwrap()
        .iter()
        .filter(|e| matches!(&e.record, Record::Report { venture, metric, .. } if venture == "self" && metric == "digest"))
        .map(|e| e.at)
        .collect()
}

/// Start avsmd's kernel at `at` under this config, and tick once.
fn tick_at(home: &Path, config: &str, at: i64) {
    std::fs::write(home.join("config.yaml"), config).unwrap();
    avsmd::clock::set(at);
    let mut k = Kernel::open(home).unwrap();
    k.adopt_self().unwrap();
    k.tick();
    k.shutdown();
}

#[test]
fn a_digest_is_written_each_nightly_even_after_one_earlier_that_day() {
    std::env::set_var("TZ", "UTC");
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    // First start at 10:30, past the 03:00 nightly: the night's digest,
    // late.
    let three = "nightly: \"03:00\"\n";
    tick_at(&home, three, DAY0 + 10 * HOUR + 30 * MINUTE);
    assert_eq!(digests(&home).len(), 1);
    // The nightly moves to 10:59 and avsmd restarts: nothing is due
    // before it, and at it the digest is written although one was
    // written today.
    let late = "nightly: \"10:59\"\n";
    tick_at(&home, late, DAY0 + 10 * HOUR + 58 * MINUTE);
    assert_eq!(digests(&home).len(), 1);
    tick_at(&home, late, DAY0 + 11 * HOUR);
    assert_eq!(digests(&home).len(), 2, "the 10:59 nightly wrote none");
    // Once a nightly: a restart after it writes no second.
    tick_at(&home, late, DAY0 + 11 * HOUR + 5 * MINUTE);
    assert_eq!(digests(&home).len(), 2);
    // And the next night, one more.
    tick_at(&home, late, DAY0 + 86_400 + 10 * HOUR + 58 * MINUTE);
    assert_eq!(digests(&home).len(), 2);
    tick_at(&home, late, DAY0 + 86_400 + 11 * HOUR);
    assert_eq!(digests(&home).len(), 3);
    // A digest record dated in the future stands for no nightly.
    let mut k = Kernel::open(&home).unwrap();
    let forged = Record::report(Claim::Measured, "self", "digest", None, "forged");
    k.ledger.append_at(forged, DAY0 + 9 * 86_400).unwrap();
    drop(k);
    tick_at(&home, late, DAY0 + 2 * 86_400 + 11 * HOUR);
    assert_eq!(digests(&home).len(), 5);
}
