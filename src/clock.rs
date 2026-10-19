//! The time avsm reads. Real time, unless a simulation has set a virtual
//! clock: then every record, cap, cadence and nightly cycle in this
//! process reads the virtual time, while model calls and commands still
//! take as long as they take. Only `avsm simulate` sets it, in its own
//! process; the daemon never does.
//!
//! A forecast branch runs on a clock of its own thread instead: `scoped`
//! sets it for that thread alone and puts it back on drop, so the daemon
//! beside it keeps real time.

use std::cell::Cell;
use std::sync::atomic::{AtomicI64, Ordering};

use chrono::{DateTime, Local, TimeZone};

/// Virtual unix seconds; `i64::MIN` means real time.
static VIRTUAL: AtomicI64 = AtomicI64::new(i64::MIN);

thread_local! {
    /// This thread's virtual unix seconds; `i64::MIN` means none.
    static LOCAL: Cell<i64> = const { Cell::new(i64::MIN) };
}

/// Unix seconds now. Before 1970 the clock reads negative, as it should.
pub fn now() -> i64 {
    let local = LOCAL.with(Cell::get);
    if local != i64::MIN {
        return local;
    }
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

/// Is a virtual clock set, for the process or for this thread?
pub fn is_virtual() -> bool {
    LOCAL.with(Cell::get) != i64::MIN || VIRTUAL.load(Ordering::SeqCst) != i64::MIN
}

/// Set the virtual clock to `t` unix seconds.
pub fn set(t: i64) {
    assert!(t != i64::MIN, "that value means real time");
    VIRTUAL.store(t, Ordering::SeqCst);
}

/// Move the virtual clock forward: this thread's own when it has one.
/// Time never runs backwards.
pub fn advance(secs: i64) {
    assert!(is_virtual(), "advance needs a virtual clock");
    let local = LOCAL.with(Cell::get);
    if local != i64::MIN {
        LOCAL.with(|c| c.set(local + secs.max(0)));
        return;
    }
    VIRTUAL.fetch_add(secs.max(0), Ordering::SeqCst);
}

/// A clock of this thread's own, until it drops.
pub struct Scoped {
    before: i64,
}

/// Give this thread a clock of its own at `t` unix seconds until the
/// guard drops.
pub fn scoped(t: i64) -> Scoped {
    assert!(t != i64::MIN, "that value means real time");
    let before = LOCAL.with(|c| c.replace(t));
    Scoped { before }
}

impl Drop for Scoped {
    fn drop(&mut self) {
        LOCAL.with(|c| c.set(self.before));
    }
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

    #[test]
    fn a_scoped_clock_is_the_threads_own() {
        std::thread::spawn(|| {
            {
                let _g = scoped(5_000);
                advance(10);
                assert_eq!(now(), 5_010);
                let elsewhere = std::thread::spawn(now).join().unwrap();
                assert_ne!(elsewhere, 5_010, "another thread keeps its own time");
            }
            assert_eq!(LOCAL.with(Cell::get), i64::MIN);
        })
        .join()
        .unwrap();
    }
}
