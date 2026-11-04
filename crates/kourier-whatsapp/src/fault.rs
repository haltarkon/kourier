//! A crate-local seam that makes a zone FAIL (W3a, decision D3), as opposed to kourier-db's
//! failpoints, which only ABORT the process (`kourier_db::failpoint`).
//!
//! The receive path has a property a kill cannot show: when the durable write of a batch FAILS (a
//! full disk, a closed store), nothing may be acknowledged and the messages must come again. A kill
//! at `wa_inside_inbox_commit` also loses the ack, but it cannot tell a hook that returns `Err` from
//! one that returns `Ok` anyway and dies before it could ack. So the zones of the receive path call
//! [`hit`] with their commit still open; an armed hit returns `Err`, the zone rolls back, and the
//! caller sees the failure exactly as it would see a real one.
//!
//! Armed either from the environment of a child (`KOURIER_WA_FAIL=<name>[:<n>]`: the n-th hit of
//! `<name>` fails, once, then the seam is quiet) or in process with [`arm`]. It is active only where
//! kourier-db's kill points are (debug builds and the `failpoints` feature,
//! `kourier_db::failpoint::ENABLED`); in any other build [`hit`] is a constant `Ok` and [`arm`] does
//! nothing. The names are this module's constants.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use wacore::store::error::StoreError;

use crate::error;

/// The environment variable that arms a fault in a child process.
pub const ENV: &str = "KOURIER_WA_FAIL";

/// The inbox zone of one hook commit, right before `COMMIT`.
pub const INBOX_COMMIT: &str = "wa_fail_inbox_commit";

/// Stage 2's zone, right before `COMMIT`.
pub const STAGE2_COMMIT: &str = "wa_fail_stage2_commit";

/// The handler's group commit (`events`), right before `COMMIT`.
pub const EVENT_WRITE: &str = "wa_fail_event_write";

/// The history's delivery zone, right before `COMMIT` (W4): the row, its secrets, its kept items, its
/// entry and the cursor are written, none committed.
pub const HISTORY_ROW: &str = "wa_fail_history_row";

struct Armed {
    name: String,
    /// Hits still to pass before the failing one (0: the next hit fails).
    skip: u32,
}

static ARMED: Mutex<Option<Armed>> = Mutex::new(None);
static ENV_READ: AtomicBool = AtomicBool::new(false);

/// Parses `<name>[:<n>]`; `n` counts from 1.
fn parse(spec: &str) -> Option<(String, u32)> {
    let spec = spec.trim();
    let (name, n) = match spec.rsplit_once(':') {
        Some((name, n)) => (name, n.parse::<u32>().ok().filter(|n| *n >= 1)?),
        None => (spec, 1),
    };
    (!name.is_empty()).then(|| (name.to_owned(), n))
}

/// Makes the `n`-th hit (from 1) of `name` from now on fail, once. Replaces an earlier arming.
/// Does nothing where the kill points are off.
#[doc(hidden)]
pub fn arm(name: &str, n: u32) {
    if !kourier_db::failpoint::ENABLED {
        return;
    }
    // The environment is read once, before the first explicit arming wins over it.
    ENV_READ.store(true, Ordering::SeqCst);
    *ARMED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Armed {
        name: name.to_owned(),
        skip: n.saturating_sub(1),
    });
}

/// Disarms the seam.
#[doc(hidden)]
pub fn disarm() {
    ENV_READ.store(true, Ordering::SeqCst);
    *ARMED.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Marks a point of a zone. `Err` when `name` is armed and this is its failing hit.
#[inline]
pub(crate) fn hit(name: &'static str) -> Result<(), StoreError> {
    if !kourier_db::failpoint::ENABLED {
        return Ok(());
    }
    let mut armed = ARMED.lock().unwrap_or_else(|e| e.into_inner());
    if !ENV_READ.swap(true, Ordering::SeqCst)
        && let Some((name, n)) = std::env::var(ENV).ok().and_then(|s| parse(&s))
    {
        *armed = Some(Armed { name, skip: n - 1 });
    }
    let Some(current) = armed.as_mut() else {
        return Ok(());
    };
    if current.name != name {
        return Ok(());
    }
    if current.skip > 0 {
        current.skip -= 1;
        return Ok(());
    }
    *armed = None;
    Err(error::database("injected test fault"))
}

/// Tests that arm the seam (or commit while another test may have armed it) take this lock: under
/// `cargo test` the tests of a binary are threads of one process, and the seam is process-wide. Under
/// nextest every test has a process of its own and the lock is never contended.
#[cfg(test)]
pub(crate) fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::{INBOX_COMMIT, arm, disarm, hit, parse, serial};

    #[test]
    fn parses_name_and_count() {
        assert_eq!(parse("a"), Some(("a".into(), 1)));
        assert_eq!(parse("a:3"), Some(("a".into(), 3)));
        assert_eq!(parse("a:0"), None);
        assert_eq!(parse(":2"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn the_nth_hit_fails_once() {
        let _serial = serial();
        arm(INBOX_COMMIT, 2);
        assert!(hit(INBOX_COMMIT).is_ok());
        assert!(hit("some_other_point").is_ok());
        assert!(hit(INBOX_COMMIT).is_err());
        assert!(hit(INBOX_COMMIT).is_ok(), "quiet after the one failure");
        disarm();
    }
}
