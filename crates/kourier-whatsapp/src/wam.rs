//! No usage statistics, ever (his decision D4, W2 design 3.7): the run-time layer.
//!
//! WhatsApp's own client reports usage ("WAM") in stanzas of one namespace, `w:stats`, through a
//! separate plugin that Kourier never links (the lock-file test and the source check keep it out of
//! the build). This is the third layer, the one that watches what REALLY leaves: the real session
//! takes a lease on upstream's sent-frame event (`Client::acquire_sent_frame_forwarding`), the event
//! handler hands every plaintext frame to [`Watch::look`], and a frame whose root carries that
//! namespace is counted and logged as an ERROR. The expected count over a whole run is 0; the smoke
//! tool and the first real run (T1, R1) read [`wam_sent`] or grep the run's log for [`LOG_MARK`].
//!
//! Cost. The event is dispatched on upstream's noise sender task, which every send crosses and which
//! a slow handler stalls. So a frame is not decoded whole: [`NodeStream`] reads the HEAD of the root
//! node (its tag and attributes) and nothing below it, which is where the namespace of a statistics
//! stanza stands. A stanza of that namespace nested below an ordinary root is not looked for: WAM
//! never sends one.
//!
//! Nothing here keeps or logs a frame, a tag or an attribute: the line says only that it happened.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use wacore_binary::NodeStream;

/// The namespace of WhatsApp's usage-statistics stanzas. This file is the only one of `src/` that
/// may spell it (the source check in `tests/builder.rs`, `STATS_MODULES`).
const STATS_NAMESPACE: &str = "w:stats";

/// What the ERROR line starts with, for a program that greps a run's log.
pub const LOG_MARK: &str = "wam_sent";

/// Statistics frames that left a client of this process, ever.
static SENT: AtomicU64 = AtomicU64::new(0);

/// Frames of any kind the watches looked at, ever.
static LOOKED: AtomicU64 = AtomicU64::new(0);

/// How many usage-statistics frames the clients of this process have sent. The expected value is 0.
pub fn wam_sent() -> u64 {
    SENT.load(Ordering::Relaxed)
}

/// Frames of ANY kind the watches of this process have looked at. A run that expects [`wam_sent`] to
/// be 0 must also see this one grow, or its 0 proves nothing (the lease on upstream's sent-frame
/// event may not be taken). The scripted-server tests read it.
pub fn wam_looked() -> u64 {
    LOOKED.load(Ordering::Relaxed)
}

/// What one frame is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Look {
    /// The root carries the statistics namespace.
    Statistics,
    /// Any other stanza.
    Other,
    /// The bytes are not a stanza this decoder can read the head of.
    Unreadable,
}

impl Look {
    /// The namespace, for a test of another module that needs a statistics frame (this file is the
    /// only one of `src/` that may spell it).
    #[cfg(test)]
    pub(crate) fn namespace_for_tests() -> String {
        STATS_NAMESPACE.to_owned()
    }
}

/// Reads the head of the root node of one sent frame (`plaintext` keeps the format byte).
pub(crate) fn look_at(plaintext: &[u8]) -> Look {
    let Ok(mut stream) = NodeStream::from_packed(plaintext) else {
        return Look::Unreadable;
    };
    match stream.open() {
        Ok(Some(head)) => {
            if head.attr_str("xmlns").as_deref() == Some(STATS_NAMESPACE) {
                Look::Statistics
            } else {
                Look::Other
            }
        }
        _ => Look::Unreadable,
    }
}

/// The counters of ONE client. Shared between the session and its event handler.
#[derive(Default)]
pub(crate) struct Watch {
    seen: AtomicU64,
    sent: AtomicU64,
    unreadable: AtomicU64,
    said_unreadable: AtomicBool,
}

impl Watch {
    /// One frame that reached the transport.
    pub(crate) fn look(&self, plaintext: &[u8]) {
        self.seen.fetch_add(1, Ordering::Relaxed);
        LOOKED.fetch_add(1, Ordering::Relaxed);
        match look_at(plaintext) {
            Look::Other => {}
            Look::Statistics => {
                let total = SENT.fetch_add(1, Ordering::Relaxed) + 1;
                let mine = self.sent.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::error!(
                    wam_sent = total,
                    "{LOG_MARK}: a usage-statistics stanza left the client ({mine} from this one)"
                );
            }
            Look::Unreadable => {
                self.unreadable.fetch_add(1, Ordering::Relaxed);
                // Once is enough: a client that sends frames this cannot read is worth one line.
                if !self.said_unreadable.swap(true, Ordering::Relaxed) {
                    tracing::warn!("a sent frame could not be read by the statistics watch");
                }
            }
        }
    }

    /// Frames looked at, of any kind: proves the lease is taken (a test that expects 0 statistics
    /// frames must also see that frames pass here, or it proves nothing).
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "read by the tests today; the scripted fixture (chunk C5) reads it"
        )
    )]
    pub(crate) fn seen(&self) -> u64 {
        self.seen.load(Ordering::Relaxed)
    }

    /// Statistics frames this client sent.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "read by the tests today; the scripted fixture (chunk C5) reads it"
        )
    )]
    pub(crate) fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    /// Frames whose head could not be read.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "read by the tests today; the scripted fixture (chunk C5) reads it"
        )
    )]
    pub(crate) fn unreadable(&self) -> u64 {
        self.unreadable.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests;
