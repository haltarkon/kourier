//! The gate's onward side: the [`Dialer`] the SOCKS5 gate hands an allowed CONNECT to, and the
//! dialer that refuses every connection. The refusing one is the application's default (decisions
//! D3 H2, TG-1b B-2): there is no dialer in this file that reaches an address. The one dialer that
//! does is `real.rs`'s `TcpDialer` (TR R0a): `Dialer`, `DialFuture` and `GateStream` are
//! crate-private, so no other crate can hand the gate a connecting dialer; the dialers of the tests
//! live in the test code.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tokio::io::{AsyncRead, AsyncWrite};

use super::ranges;

/// A connection the gate relays TDLib's bytes over.
pub(crate) trait GateStream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> GateStream for T {}

/// What [`Dialer::dial`] returns.
pub(crate) type DialFuture = Pin<Box<dyn Future<Output = io::Result<Box<dyn GateStream>>> + Send>>;

/// Opens the gate's onward connection to an allowed Telegram address (TG-1b). The application's
/// dialer refuses every dial unless the run tool's confirmation chose the TCP one (decisions D3 H2,
/// TR R0a D-11).
pub(crate) trait Dialer: Send + Sync {
    /// Connects to `to`, or refuses.
    fn dial(&self, to: SocketAddr) -> DialFuture;
}

/// The dialer that refuses every connection and counts it. The application's parts carry one
/// that also says, once, that Telegram is not contacted in this build.
pub(crate) struct RefusingDialer {
    refused: Arc<AtomicUsize>,
    tell: bool,
    told: AtomicBool,
}

/// The one sentence the application's refusing dialer logs (once per process of parts).
pub(crate) const NOT_CONTACTED: &str = "Telegram is not contacted in this build";

impl RefusingDialer {
    /// A refusing dialer counting into `refused`; `tell` logs [`NOT_CONTACTED`] at the first dial.
    pub(crate) fn new(refused: Arc<AtomicUsize>, tell: bool) -> Self {
        Self {
            refused,
            tell,
            told: AtomicBool::new(false),
        }
    }
}

impl Dialer for RefusingDialer {
    fn dial(&self, _to: SocketAddr) -> DialFuture {
        self.refused.fetch_add(1, Ordering::SeqCst);
        if self.tell && !self.told.swap(true, Ordering::SeqCst) {
            tracing::warn!("{NOT_CONTACTED}");
        }
        Box::pin(std::future::ready(Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            NOT_CONTACTED,
        ))))
    }
}

/// Whether a dial of the TCP dialer (`real.rs`) to `to` may open a socket: the address inside the
/// list AND the port among its ports. An `Err` is `PermissionDenied` and no socket exists before
/// this answers. It is the dialer's second wall, kept here (not in `real.rs`) so the test door
/// (`kourier_telegram::offline::a_dial_is_permitted`) can ask it without naming the dialer's file.
///
/// The TRIPWIRE (TR R0a fix G-1, review F1): in every build of the unit tests of this crate AND in
/// every build with the feature `test-support` (the integration tests of this crate, the tests of
/// `kourier-app` and of the real binary's tests all link it with that feature through their
/// dev-dependencies) a dial is also refused unless the process is the offline child (`unshare -rn`,
/// every address routed to `lo`; the marker AND a namespace with `lo` alone, so it cannot be faked
/// into a dial that leaves the machine). That keeps ANY test from reaching a real address by
/// mistake, whatever the test process inherits. It is not compiled into a build without either, which
/// is the build the run tool stages.
pub(crate) fn tcp_permission(to: SocketAddr) -> io::Result<()> {
    #[cfg(any(test, feature = "test-support"))]
    if !kourier_tdlib::spawn::in_offline_child() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "a test may dial only inside the offline child",
        ));
    }
    if !ranges::allowed(to.ip()) || !ranges::port_allowed(to.port()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the destination is outside the allowed list",
        ));
    }
    Ok(())
}
