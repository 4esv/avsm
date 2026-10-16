//! The time avsm reads. Real time, unless a simulation has set a virtual
//! clock: then every record, cap, cadence and nightly cycle in this
//! process reads the virtual time, while model calls and commands still
//! take as long as they take. Only `avsm simulate` sets it, in its own
//! process; the daemon never does.

use std::sync::atomic::{AtomicI64, Ordering};

use chrono::{DateTime, Local, TimeZone};

/// Virtual unix seconds; `i64::MIN` means real time.
static VIRTUAL: AtomicI64 = AtomicI64::new(i64::MIN);

/// Unix seconds now. Before 1970 the clock reads negative, as it should.
pub fn now() -> i64 {
    match VIRTUAL.load(Ordering::SeqCst) {
        i64::MIN => match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(since) => since.as_secs() as i64,
            Err(before) => -(before.duration().as_secs() as i64),
        },
        t => t,
    }
}

/// Local time now, on the same clock.
pub fn local() -> DateTime<Local> {
    Local
        .timestamp_opt(now(), 0)
        .single()
        .unwrap_or_else(Local::now)
}

/// Is a virtual clock set?
pub fn is_virtual() -> bool {
    VIRTUAL.load(Ordering::SeqCst) != i64::MIN
}

/// Set the virtual clock to `t` unix seconds.
pub fn set(t: i64) {
    assert!(t != i64::MIN, "that value means real time");
    VIRTUAL.store(t, Ordering::SeqCst);
}

/// Move the virtual clock forward. Time never runs backwards.
pub fn advance(secs: i64) {
    assert!(is_virtual(), "advance needs a virtual clock");
    VIRTUAL.fetch_add(secs.max(0), Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_until_set_then_virtual_and_monotone() {
        assert!(!is_virtual());
        assert!((now() - chrono::Utc::now().timestamp()).abs() <= 2);
        set(1_700_000_000);
        assert_eq!(now(), 1_700_000_000);
        advance(3600);
        advance(-50);
        assert_eq!(now(), 1_700_003_600);
        assert_eq!(local().timestamp(), 1_700_003_600);
        VIRTUAL.store(i64::MIN, Ordering::SeqCst);
        assert!(!is_virtual());
    }
}
