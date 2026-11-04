//! The SOCKS5 gate (TG-1 plan §2.5, TG-1b B-1): the ONE way TDLib's connections leave the
//! helper. One gate per account, in the app process, on the network runtime; TDLib gets its port
//! and login only in the helper's own `addProxy` (the `G` frame).
//!
//! - Listener: `127.0.0.1` on a port the OS chooses, never a fixed one.
//! - Login: a username and a password of 16 random bytes each (base64url, 22 bytes), new at every
//!   start; a greeting without username/password is refused, so no other local process can use
//!   the gate; compared in constant time.
//! - Forwarded: CONNECT to an IPv4 or IPv6 literal inside [`ranges`] AND on one of
//!   [`ranges::PORTS`]; anything else gets REP 02 (or 07/08) and is NEVER dialled.
//! - Limits: a handshake must finish within [`GateOptions::handshake_timeout`] (5 s), and at most
//!   [`GateOptions::max_pending`] (16) connections may be in their handshake at once; one more is
//!   closed at once.
//! - Logging: counts only ([`Counts`]), at the stop and at most every 10 minutes; never an
//!   address, a port or a credential.
//!
//! The dialer behind it is the account's [`Dialer`]; the application's refuses every dial in
//! TG-1 (B-2).

pub(crate) mod dialer;
pub(crate) mod ranges;
mod socks5;
#[cfg(test)]
mod tests;

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine as _;
use rand::RngCore as _;
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Handle;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use self::dialer::Dialer;
use self::socks5::{Login, Refusal, Rep};

/// The gate's limits.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GateOptions {
    /// From the accept to a complete request (greeting, login, CONNECT).
    pub(crate) handshake_timeout: Duration,
    /// Connections in their handshake at once.
    pub(crate) max_pending: usize,
    /// The bound of one onward dial.
    pub(crate) dial_timeout: Duration,
    /// How often the counts are logged while the gate runs.
    pub(crate) report_every: Duration,
}

impl Default for GateOptions {
    fn default() -> Self {
        Self {
            handshake_timeout: Duration::from_secs(5),
            max_pending: 16,
            dial_timeout: Duration::from_secs(10),
            report_every: Duration::from_secs(600),
        }
    }
}

/// What the gate counted. Every field is a number; nothing else of a connection is kept.
#[derive(Default)]
struct Counts {
    accepted: AtomicU64,
    busy: AtomicU64,
    timed_out: AtomicU64,
    protocol: AtomicU64,
    no_login: AtomicU64,
    auth: AtomicU64,
    command: AtomicU64,
    address_type: AtomicU64,
    rule: AtomicU64,
    port: AtomicU64,
    dials: AtomicU64,
    dial_failures: AtomicU64,
    bytes_up: AtomicU64,
    bytes_down: AtomicU64,
}

/// A copy of the counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GateCounts {
    /// TCP connections accepted (the busy ones included).
    pub(crate) accepted: u64,
    /// Closed at once: [`GateOptions::max_pending`] handshakes were running.
    pub(crate) busy: u64,
    /// The handshake did not finish in time.
    pub(crate) timed_out: u64,
    /// Not SOCKS5, or closed half way.
    pub(crate) protocol: u64,
    /// The greeting offered no username/password (`FF`).
    pub(crate) no_login: u64,
    /// A wrong login.
    pub(crate) auth: u64,
    /// BIND or UDP ASSOCIATE (REP 07).
    pub(crate) command: u64,
    /// A domain name or another address type (REP 08).
    pub(crate) address_type: u64,
    /// A destination outside the list (REP 02, not dialled).
    pub(crate) rule: u64,
    /// A port outside the three (REP 02, not dialled).
    pub(crate) port: u64,
    /// Onward dials asked of the dialer.
    pub(crate) dials: u64,
    /// Dials that failed or timed out.
    pub(crate) dial_failures: u64,
    /// Bytes from TDLib onward.
    pub(crate) bytes_up: u64,
    /// Bytes from onward to TDLib.
    pub(crate) bytes_down: u64,
}

impl Counts {
    fn bump(field: &AtomicU64) {
        field.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> GateCounts {
        let l = |f: &AtomicU64| f.load(Ordering::Relaxed);
        GateCounts {
            accepted: l(&self.accepted),
            busy: l(&self.busy),
            timed_out: l(&self.timed_out),
            protocol: l(&self.protocol),
            no_login: l(&self.no_login),
            auth: l(&self.auth),
            command: l(&self.command),
            address_type: l(&self.address_type),
            rule: l(&self.rule),
            port: l(&self.port),
            dials: l(&self.dials),
            dial_failures: l(&self.dial_failures),
            bytes_up: l(&self.bytes_up),
            bytes_down: l(&self.bytes_down),
        }
    }

    fn refused(&self, why: Refusal) {
        Self::bump(match why {
            Refusal::Protocol => &self.protocol,
            Refusal::NoLogin => &self.no_login,
            Refusal::Auth => &self.auth,
            Refusal::Command => &self.command,
            Refusal::AddressType => &self.address_type,
        });
    }
}

impl GateCounts {
    /// Logs the counts: numbers only.
    fn log(&self, when: &'static str) {
        tracing::info!(
            when,
            accepted = self.accepted,
            busy = self.busy,
            timed_out = self.timed_out,
            protocol = self.protocol,
            no_login = self.no_login,
            auth = self.auth,
            command = self.command,
            address_type = self.address_type,
            rule = self.rule,
            port = self.port,
            dials = self.dials,
            dial_failures = self.dial_failures,
            bytes_up = self.bytes_up,
            bytes_down = self.bytes_down,
            "telegram gate counts"
        );
    }
}

/// The login of one gate start.
struct Secret {
    user: Zeroizing<String>,
    pass: Zeroizing<String>,
}

impl Secret {
    fn random() -> Self {
        let token = || {
            let mut raw = Zeroizing::new([0u8; 16]);
            rand::rng().fill_bytes(raw.as_mut_slice());
            let mut out = Zeroizing::new(String::new());
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode_string(raw.as_slice(), &mut out);
            out
        };
        Self {
            user: token(),
            pass: token(),
        }
    }
}

/// A running gate. Dropping it stops it (without the counts' log line: call [`GateHandle::stop`]).
pub(crate) struct GateHandle {
    port: u16,
    secret: Arc<Secret>,
    counts: Arc<Counts>,
    cancel: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl GateHandle {
    /// The listener's port on `127.0.0.1`.
    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    /// The username TDLib must send.
    pub(crate) fn user(&self) -> &str {
        &self.secret.user
    }

    /// The password TDLib must send.
    pub(crate) fn pass(&self) -> &str {
        &self.secret.pass
    }

    /// The counts so far (the tests read them; the stop logs them).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn counts(&self) -> GateCounts {
        self.counts.snapshot()
    }

    /// Stops the listener and every relay, logs the counts and returns them.
    pub(crate) async fn stop(mut self) -> GateCounts {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        let counts = self.counts.snapshot();
        counts.log("stop");
        counts
    }
}

impl Drop for GateHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl std::fmt::Debug for GateHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Neither the port nor the login.
        f.write_str("GateHandle")
    }
}

/// Starts a gate on `rt`: binds `127.0.0.1:0` now, accepts on `rt`. `cancel` (a child of the
/// account's token) stops it too.
pub(crate) fn start(
    rt: &Handle,
    dialer: Arc<dyn Dialer>,
    opts: GateOptions,
    cancel: CancellationToken,
) -> io::Result<GateHandle> {
    start_with_binder(rt, dialer, opts, cancel, || {
        std::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
    })
}

/// [`start`] with the listener made by `bind` (the seam of TG-1b's LOW D1: a test hands a
/// data-centre port first and sees it refused). The port filter is
/// [`kourier_tdlib::protocol::gate_port_allowed`], here and nowhere else.
pub(crate) fn start_with_binder(
    rt: &Handle,
    dialer: Arc<dyn Dialer>,
    opts: GateOptions,
    cancel: CancellationToken,
    bind: impl FnMut() -> io::Result<std::net::TcpListener>,
) -> io::Result<GateHandle> {
    let listener = bind_acceptable(bind, kourier_tdlib::protocol::gate_port_allowed)?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let listener = {
        let _in_rt = rt.enter();
        TcpListener::from_std(listener)?
    };
    let secret = Arc::new(Secret::random());
    let counts = Arc::new(Counts::default());
    let task = rt.spawn(accept_loop(
        listener,
        secret.clone(),
        dialer,
        counts.clone(),
        opts,
        cancel.clone(),
    ));
    Ok(GateHandle {
        port,
        secret,
        counts,
        cancel,
        task: Some(task),
    })
}

/// How many system-chosen ports [`bind_acceptable`] looks at before it gives up.
const BIND_TRIES: usize = 64;

/// Binds with `bind` (an OS-chosen port) until the port passes `allowed` (G6: not a data-centre port
/// and not below 1024; Landlock names a port, never an address, so the helper's one open port must
/// not also be a port a data centre listens on). The listeners that did not pass stay open until
/// one does, so the system cannot hand the same port out again.
fn bind_acceptable(
    mut bind: impl FnMut() -> io::Result<std::net::TcpListener>,
    allowed: impl Fn(u16) -> bool,
) -> io::Result<std::net::TcpListener> {
    let mut rejected = Vec::new();
    for _ in 0..BIND_TRIES {
        let listener = bind()?;
        if allowed(listener.local_addr()?.port()) {
            return Ok(listener);
        }
        rejected.push(listener);
    }
    Err(io::Error::new(
        io::ErrorKind::AddrNotAvailable,
        "no acceptable port for the gate",
    ))
}

async fn accept_loop(
    listener: TcpListener,
    secret: Arc<Secret>,
    dialer: Arc<dyn Dialer>,
    counts: Arc<Counts>,
    opts: GateOptions,
    cancel: CancellationToken,
) {
    let pending = Arc::new(Semaphore::new(opts.max_pending));
    let mut report = tokio::time::interval_at(
        tokio::time::Instant::now() + opts.report_every,
        opts.report_every,
    );
    let mut relays = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            _ = report.tick() => counts.snapshot().log("running"),
            Some(_) = relays.join_next(), if !relays.is_empty() => {}
            got = listener.accept() => match got {
                Ok((sock, _peer)) => {
                    Counts::bump(&counts.accepted);
                    let Ok(permit) = pending.clone().try_acquire_owned() else {
                        Counts::bump(&counts.busy);
                        drop(sock);
                        continue;
                    };
                    relays.spawn(serve(
                        sock,
                        permit,
                        secret.clone(),
                        dialer.clone(),
                        counts.clone(),
                        opts,
                        cancel.clone(),
                    ));
                }
                // Out of descriptors and the like: wait a little instead of spinning.
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            },
        }
    }
    relays.shutdown().await;
}

/// One connection: the handshake under the timeout and the pending cap, the checks, the dial,
/// the relay.
async fn serve(
    mut sock: TcpStream,
    permit: OwnedSemaphorePermit,
    secret: Arc<Secret>,
    dialer: Arc<dyn Dialer>,
    counts: Arc<Counts>,
    opts: GateOptions,
    cancel: CancellationToken,
) {
    let _ = sock.set_nodelay(true);
    let login = Login {
        user: secret.user.as_bytes(),
        pass: secret.pass.as_bytes(),
    };
    let to =
        match tokio::time::timeout(opts.handshake_timeout, socks5::handshake(&mut sock, &login))
            .await
        {
            Err(_) => return Counts::bump(&counts.timed_out),
            Ok(Err(why)) => return counts.refused(why),
            Ok(Ok(to)) => to,
        };
    // Authenticated: the slot goes to the next unauthenticated connection.
    drop(permit);
    if !ranges::allowed(to.ip()) {
        Counts::bump(&counts.rule);
        let _ = socks5::reply(&mut sock, Rep::NotAllowed).await;
        return;
    }
    if !ranges::port_allowed(to.port()) {
        Counts::bump(&counts.port);
        let _ = socks5::reply(&mut sock, Rep::NotAllowed).await;
        return;
    }
    Counts::bump(&counts.dials);
    let mut onward = match tokio::time::timeout(opts.dial_timeout, dialer.dial(to)).await {
        Ok(Ok(onward)) => onward,
        Ok(Err(e)) => {
            Counts::bump(&counts.dial_failures);
            let rep = if e.kind() == io::ErrorKind::ConnectionRefused {
                Rep::Refused
            } else {
                Rep::GeneralFailure
            };
            let _ = socks5::reply(&mut sock, rep).await;
            return;
        }
        Err(_) => {
            Counts::bump(&counts.dial_failures);
            let _ = socks5::reply(&mut sock, Rep::GeneralFailure).await;
            return;
        }
    };
    if socks5::reply(&mut sock, Rep::Succeeded).await.is_err() {
        return;
    }
    tokio::select! {
        () = cancel.cancelled() => {}
        moved = tokio::io::copy_bidirectional(&mut sock, &mut onward) => {
            if let Ok((up, down)) = moved {
                counts.bytes_up.fetch_add(up, Ordering::Relaxed);
                counts.bytes_down.fetch_add(down, Ordering::Relaxed);
            }
        }
    }
}
