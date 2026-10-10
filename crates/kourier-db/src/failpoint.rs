//! Kill points for crash-safety tests (§10.4).
//!
//! `KOURIER_FAILPOINT=<name>[:<n>]` aborts the process at the n-th [`hit`] of `name` (default
//! the first). Aborting, not panicking, is the point: no destructor, no rollback, no flush runs,
//! like a power cut after the last fsync. Active in debug builds and with the `failpoints` cargo
//! feature; a no-op otherwise.

/// The environment variable that arms a failpoint.
pub const ENV: &str = "KOURIER_FAILPOINT";

/// Whether [`hit`] can abort in this build (debug builds, or the `failpoints` feature).
pub const ENABLED: bool = cfg!(any(debug_assertions, feature = "failpoints"));

/// Marks a kill point. Aborts the process when `name` is armed and this is its n-th hit.
#[inline]
pub fn hit(name: &str) {
    #[cfg(any(debug_assertions, feature = "failpoints"))]
    armed::hit(name);
    #[cfg(not(any(debug_assertions, feature = "failpoints")))]
    let _ = name;
}

/// Parses `<name>[:<n>]`; `n` counts from 1.
pub fn parse(spec: &str) -> Option<(String, u32)> {
    let spec = spec.trim();
    let (name, n) = match spec.rsplit_once(':') {
        Some((name, n)) => (name, n.parse().ok().filter(|n| *n >= 1)?),
        None => (spec, 1),
    };
    (!name.is_empty()).then(|| (name.to_owned(), n))
}

#[cfg(any(debug_assertions, feature = "failpoints"))]
mod armed {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU32, Ordering};

    static ARMED: OnceLock<Option<(String, u32)>> = OnceLock::new();
    static HITS: AtomicU32 = AtomicU32::new(0);

    pub(super) fn hit(name: &str) {
        let armed = ARMED.get_or_init(|| {
            std::env::var(super::ENV)
                .ok()
                .and_then(|s| super::parse(&s))
        });
        let Some((armed, n)) = armed else { return };
        if armed != name {
            return;
        }
        let count = HITS.fetch_add(1, Ordering::SeqCst) + 1;
        if count == *n {
            eprintln!("failpoint {name}:{n} hit: aborting");
            std::process::abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn parses_name_and_count() {
        assert_eq!(parse("before_ack"), Some(("before_ack".into(), 1)));
        assert_eq!(
            parse("after_core_commit:3"),
            Some(("after_core_commit".into(), 3))
        );
        assert_eq!(parse("x:0"), None);
        assert_eq!(parse("x:abc"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse(":2"), None);
    }

    #[test]
    fn unarmed_hit_is_a_no_op() {
        // The test process never sets KOURIER_FAILPOINT for this name.
        super::hit("never_armed_in_unit_tests");
    }
}
