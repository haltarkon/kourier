//! The thin async shell of the fake (lane plan §5.1, S-mock §8): the crate's own `WsConnector`,
//! `WsSender` and `HttpsClient` over `async_channel`, in-process. No socket, no runtime of its own
//! and no task: what the client sends goes into the sans-I/O [`Mediator`] at once, and what the
//! fake has to send is moved into each connection's event channel by [`Net::flush`] (after every
//! frame of the client and after every script call of [`super::script`]).
//!
//! Hosts are `.invalid` (RFC 6761): the mediator is `mediator-<p4>.invalid`, the rendezvous relay
//! (TH4 J2, [`super::relay`]) `rendezvous-<p4>.invalid` (asked for as `WsKind::Rendezvous`, the
//! client is its RID); every other host (the blob mirror later) is REFUSED, counted and recorded as a
//! violation, and so is every HTTPS request but those to the DIRECTORY (TH6 L2b, [`super::directory`]:
//! `https://directory.invalid/`, served from the world's identity table, delayed by
//! [`Net::hold_directory`]). A misrouted real dial could not resolve. The scripted joiners of the world ([`super::joiner`]) are pumped at every
//! [`Net::flush`], so a client that speaks to the relay gets the joiner's answer at once.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use kourier_threema::{
    HttpsClient, HttpsRequest, HttpsResponse, NetError, ServerParams, ThreemaParts, WsConnector,
    WsEvent, WsKind, WsRequest, WsSender,
};

use super::d2m::{ConnId, Mediator, Out};
use super::directory::{Directory, is_directory};
use super::joiner::Joiner;
use super::relay::{RELAY_URL, Relay, Role, SideId, is_relay_host};
use super::{MockWorld, Violations, lock};

/// The mediator's URL template the fake's parts hand out (libthreema fills the placeholders).
pub const MEDIATOR_URL: &str =
    "wss://mediator-{deviceGroupIdPrefix4}.invalid/{deviceGroupIdPrefix8}/";
/// The rendezvous relay's URL template (TH4 J2: served by [`super::relay`]).
pub const RENDEZVOUS_URL: &str = RELAY_URL;
/// The directory's base URL (TH6 L2b: served by [`super::directory`]).
pub const DIRECTORY_URL: &str = "https://directory.invalid/";
/// The other server of the parts: a host the fake does not serve yet (refused and counted).
pub const BLOB_URL: &str = "https://blob.invalid/";

/// The host of a `wss://` or `https://` URL (between the scheme and the first `/`), if it has one.
pub fn host_of(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("https://"))?;
    let host = rest.split('/').next()?;
    (!host.is_empty()).then_some(host)
}

/// Whether `host` is the fake mediator's: `mediator-<one hex digit>.invalid`.
fn is_mediator_host(host: &str) -> bool {
    host.strip_prefix("mediator-")
        .and_then(|h| h.strip_suffix(".invalid"))
        .is_some_and(|p4| p4.len() == 1 && p4.bytes().all(|b| b.is_ascii_hexdigit()))
}

struct Inner {
    mediator: Mediator,
    relay: Relay,
    /// TH6 L2b: the directory the HTTPS client serves.
    directory: Directory,
    /// TH6 L2b: while `true`, every directory request pends (a directory that is slow to answer) until it is set back;
    /// the client's own deadline drops a pending request.
    hold_directory: tokio::sync::watch::Sender<bool>,
    /// How many directory requests started to pend.
    directory_held: AtomicUsize,
    /// How many directory requests pend NOW (neither served nor dropped by the client).
    directory_pending: AtomicUsize,
    violations: Violations,
    /// The connector's open relay sides (the client is the RID) and each one's event channel.
    relay_open: Mutex<BTreeMap<SideId, async_channel::Sender<WsEvent>>>,
    /// Every relay side the connector opened, in order.
    relay_opened: Mutex<Vec<SideId>>,
    /// The scripted joiners pumped at every flush.
    joiners: Mutex<Vec<Joiner>>,
    /// The connector's open connections and the sending half of each one's event channel.
    open: Mutex<BTreeMap<ConnId, async_channel::Sender<WsEvent>>>,
    /// Every connection the connector opened, in order (open or closed).
    opened: Mutex<Vec<ConnId>>,
    refused_hosts: AtomicUsize,
    refused_https: AtomicUsize,
    made: AtomicUsize,
    /// TH5 round A (L-2): the sends the client tried on a mediator connection that was no longer open
    /// (the server's close had reached it), per connection.
    late_sends: Mutex<Vec<ConnId>>,
    /// TH3 N6: while set, every connect to the mediator fails with this error (no socket, not a
    /// violation: a server not reached, an untrusted certificate).
    fail_connects: Mutex<Option<NetError>>,
    /// Every connect the connector was asked for (opened, failed or refused).
    attempts: AtomicUsize,
    /// TH3 fix P-L9: while `true`, a connect never completes (DNS, TCP or TLS that hangs) until it
    /// is set back; the actor's deadline drops the pending connect.
    hold_connects: tokio::sync::watch::Sender<bool>,
    /// TH3 fix P-L8: the NEXT connect panics (the connector itself is a bug), once.
    panic_next_connect: AtomicBool,
    /// TH4 P3: while set, every connect to the RENDEZVOUS RELAY fails with this error (an untrusted
    /// certificate, a relay not reached), counted in `attempts`, no side.
    fail_relay_connects: Mutex<Option<NetError>>,
    /// TH4 P8: while `true`, every frame the client writes to the RELAY pends (a relay that stopped
    /// reading, TCP backpressure) and is not delivered; set back, the pending writes go through.
    stall_relay_sends: tokio::sync::watch::Sender<bool>,
    /// How many relay writes started to pend.
    stalled: AtomicUsize,
    /// RP-b (P-L1): while `true`, a CLOSE of a rendezvous socket by the client pends and is not delivered (a relay that
    /// stopped answering, TCP backpressure); set back, the pending closes go through. The client bounds its close.
    stall_relay_closes: tokio::sync::watch::Sender<bool>,
    /// TH6a S3 (C14): while `true`, every frame the client writes to the MEDIATOR pends and is not
    /// delivered (a socket that stopped reading, TCP backpressure); set back, the pending writes go
    /// through, in the order they started.
    stall_mediator_sends: tokio::sync::watch::Sender<bool>,
    /// How many mediator writes started to pend.
    stalled_mediator: AtomicUsize,
    changed: tokio::sync::Notify,
}

/// The fake's network: the connections the connector opened onto the world's mediator. Cheap to
/// clone; one per world (`world.net()`).
#[derive(Clone)]
pub struct Net(Arc<Inner>);

impl std::fmt::Debug for Net {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Net")
    }
}

impl Net {
    pub fn new(
        mediator: Mediator,
        relay: Relay,
        directory: Directory,
        violations: Violations,
    ) -> Self {
        Self(Arc::new(Inner {
            mediator,
            relay,
            directory,
            hold_directory: tokio::sync::watch::channel(false).0,
            directory_held: AtomicUsize::new(0),
            directory_pending: AtomicUsize::new(0),
            violations,
            relay_open: Mutex::new(BTreeMap::new()),
            relay_opened: Mutex::new(Vec::new()),
            joiners: Mutex::new(Vec::new()),
            open: Mutex::new(BTreeMap::new()),
            opened: Mutex::new(Vec::new()),
            refused_hosts: AtomicUsize::new(0),
            refused_https: AtomicUsize::new(0),
            made: AtomicUsize::new(0),
            late_sends: Mutex::new(Vec::new()),
            fail_connects: Mutex::new(None),
            attempts: AtomicUsize::new(0),
            hold_connects: tokio::sync::watch::channel(false).0,
            panic_next_connect: AtomicBool::new(false),
            fail_relay_connects: Mutex::new(None),
            stall_relay_sends: tokio::sync::watch::channel(false).0,
            stalled: AtomicUsize::new(0),
            stall_relay_closes: tokio::sync::watch::channel(false).0,
            stall_mediator_sends: tokio::sync::watch::channel(false).0,
            stalled_mediator: AtomicUsize::new(0),
            changed: tokio::sync::Notify::new(),
        }))
    }

    pub fn mediator(&self) -> &Mediator {
        &self.0.mediator
    }

    pub fn relay(&self) -> &Relay {
        &self.0.relay
    }

    /// TH6 L2b: the world's directory (its knobs and the requests it took).
    pub fn directory(&self) -> &Directory {
        &self.0.directory
    }

    /// TH6 L2b: while `held`, every directory request pends (counted in [`Net::directory_held`] when it starts) until
    /// this is called with `false`, then it is served by the knobs of that moment; a request the client gave up on is
    /// dropped.
    pub fn hold_directory(&self, held: bool) {
        self.0.hold_directory.send_replace(held);
    }

    /// TH6 L2b: how many directory requests started to pend ([`Net::hold_directory`]).
    pub fn directory_held(&self) -> usize {
        self.0.directory_held.load(Ordering::SeqCst)
    }

    /// How many directory requests pend at this moment ([`Net::hold_directory`]): one that was served, or that the client
    /// gave up on (its future dropped), no longer counts. A client's lookup that pends keeps a timer of its own on the
    /// actor's clock (its bound), which [`Net::directory_held`] cannot tell from a lookup that ended.
    pub fn directory_pending(&self) -> usize {
        self.0.directory_pending.load(Ordering::SeqCst)
    }

    /// Pumps `joiner` at every [`Net::flush`] from now on.
    pub fn add_joiner(&self, joiner: Joiner) {
        lock(&self.0.joiners).push(joiner);
    }

    /// Every relay side the connector opened for a client, in order.
    pub fn relay_opened(&self) -> Vec<SideId> {
        lock(&self.0.relay_opened).clone()
    }

    /// Runs the scripted joiners until none of them has anything left to do.
    fn pump_joiners(&self) {
        let joiners = lock(&self.0.joiners).clone();
        for _ in 0..10_000 {
            let mut moved = false;
            for j in &joiners {
                moved |= j.pump();
            }
            if !moved {
                return;
            }
        }
        panic!("the scripted joiners did not settle");
    }

    /// Moves what the relay sent on the connector's relay sides into their event channels.
    fn flush_relay(&self) {
        let mut open = lock(&self.0.relay_open);
        let mut ended = Vec::new();
        for (side, events) in open.iter() {
            for out in self.0.relay.take(*side) {
                let event = match out {
                    Out::Datagram(d) => WsEvent::Binary(d),
                    Out::Close(code) => {
                        ended.push(*side);
                        WsEvent::Closed {
                            code,
                            reason: String::new(),
                        }
                    }
                };
                let _ = events.try_send(event);
                if ended.last() == Some(side) {
                    break;
                }
            }
        }
        for side in ended {
            open.remove(&side);
        }
    }

    /// Moves what the fake sent on each of the connector's open connections into its event
    /// channel, in order; a close ends the channel after its `Closed` event. Connections the
    /// connector did not open (the test side's `Phone`) are left alone.
    pub fn flush(&self) {
        self.pump_joiners();
        self.flush_relay();
        let mut open = lock(&self.0.open);
        let mut ended = Vec::new();
        for (conn, events) in open.iter() {
            for out in self.0.mediator.take(*conn) {
                let event = match out {
                    Out::Datagram(d) => WsEvent::Binary(d),
                    Out::Close(code) => {
                        ended.push(*conn);
                        WsEvent::Closed {
                            code,
                            reason: String::new(),
                        }
                    }
                };
                // An unbounded channel refuses only when the client dropped its receiver: what
                // was in flight is lost, as on a real socket.
                let _ = events.try_send(event);
                if ended.last() == Some(conn) {
                    break;
                }
            }
        }
        for conn in ended {
            // Dropping the sender ends the client's stream after the `Closed` event.
            open.remove(&conn);
        }
        drop(open);
        self.0.changed.notify_waiters();
    }

    /// Every connection the connector opened, in order.
    pub fn opened(&self) -> Vec<ConnId> {
        lock(&self.0.opened).clone()
    }

    /// TH5 round A (L-2): how often the client tried to SEND on `conn` after the connection was closed
    /// (the server's close had been delivered to it). Not a violation by itself (a frame can cross a
    /// close); a test that knows the client had read the close before it sent asserts 0.
    pub fn late_sends(&self, conn: ConnId) -> usize {
        lock(&self.0.late_sends)
            .iter()
            .filter(|c| **c == conn)
            .count()
    }

    /// The connector's connections that are still open.
    pub fn open_connections(&self) -> Vec<ConnId> {
        lock(&self.0.open).keys().copied().collect()
    }

    /// How many WebSocket connects to a host the fake does not serve were refused.
    pub fn refused_hosts(&self) -> usize {
        self.0.refused_hosts.load(Ordering::SeqCst)
    }

    /// How many HTTPS requests were refused (every one but the directory's, TH6 L2b).
    pub fn refused_https(&self) -> usize {
        self.0.refused_https.load(Ordering::SeqCst)
    }

    /// How often the parts' maker made a pair of transports (one per client).
    pub fn made(&self) -> usize {
        self.0.made.load(Ordering::SeqCst)
    }

    /// TH3 N6: every later connect to the mediator fails with `error` until it is set to `None`
    /// (each failed attempt counted in [`Net::attempts`], no socket, no violation).
    pub fn fail_connects(&self, error: Option<NetError>) {
        *lock(&self.0.fail_connects) = error;
    }

    /// TH3 fix P-L9: while `held`, every connect to the connector pends (counted in
    /// [`Net::attempts`] when it starts, no socket, no violation) until this is called with
    /// `false`: a connect that hangs. A connect the actor gave up on is dropped, so it does not
    /// open a connection when the hold ends.
    pub fn hold_connects(&self, held: bool) {
        self.0.hold_connects.send_replace(held);
    }

    /// TH3 fix P-L8: the next connect panics, once (a bug in the transport; the actor's connection
    /// task panics with it).
    pub fn panic_next_connect(&self) {
        self.0.panic_next_connect.store(true, Ordering::SeqCst);
    }

    /// TH4 P3: every later connect to the rendezvous relay fails with `error` until it is set to
    /// `None` (counted in [`Net::attempts`], no side, no violation).
    pub fn fail_relay_connects(&self, error: Option<NetError>) {
        *lock(&self.0.fail_relay_connects) = error;
    }

    /// TH4 P8: while `stalled`, every frame the client writes to the rendezvous relay pends and is
    /// not delivered (the client's own bound drops the write); `false` lets the pending ones go.
    pub fn stall_relay_sends(&self, stalled: bool) {
        self.0.stall_relay_sends.send_replace(stalled);
    }

    /// RP-b (P-L1): while `stalled`, the client's CLOSE of a rendezvous socket pends and is not delivered (the client's own
    /// bound, one second of its clock, gives it up); `false` lets the pending ones go.
    pub fn stall_relay_closes(&self, stalled: bool) {
        self.0.stall_relay_closes.send_replace(stalled);
    }

    /// How many writes to the rendezvous relay started to pend ([`Net::stall_relay_sends`]).
    pub fn stalled_sends(&self) -> usize {
        self.0.stalled.load(Ordering::SeqCst)
    }

    /// TH6a S3 (C14): while `stalled`, every frame the client writes to the MEDIATOR (the CSP frames
    /// inside its Proxy payloads too) pends and is not delivered, so the client's own bound on a
    /// write has to end it; `false` lets the pending ones go. [U: real server] a real socket stalls
    /// when the peer's window is full, which the fake stands in for.
    pub fn stall_mediator_sends(&self, stalled: bool) {
        self.0.stall_mediator_sends.send_replace(stalled);
    }

    /// How many writes to the mediator started to pend ([`Net::stall_mediator_sends`]).
    pub fn stalled_mediator_sends(&self) -> usize {
        self.0.stalled_mediator.load(Ordering::SeqCst)
    }

    /// How many connects the connector was asked for in all (opened, failed or refused).
    pub fn attempts(&self) -> usize {
        self.0.attempts.load(Ordering::SeqCst)
    }

    /// The connector's connection that serves device `device` and finished the handshake.
    pub fn connected(&self, device: u64) -> Option<ConnId> {
        let m = &self.0.mediator;
        lock(&self.0.opened)
            .iter()
            .rev()
            .copied()
            .find(|c| m.is_online(*c) && m.device_of(*c) == Some(device))
    }

    /// Waits until `done` holds, re-checked after every change of the fake (a frame of a client, a
    /// script call), for at most `within` on tokio's clock (the PAUSED clock in the tests: no wall
    /// time). Whether it held.
    pub async fn wait(&self, mut done: impl FnMut() -> bool, within: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let changed = self.0.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if done() {
                return true;
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                return done();
            }
        }
    }

    /// The fake's connector (what the parts' maker hands out).
    pub fn connector(&self) -> MockConnector {
        MockConnector(self.clone())
    }

    /// The fake's HTTPS client: serves the directory (TH6 L2b) and refuses every other request.
    pub fn https(&self) -> MockHttps {
        MockHttps(self.clone())
    }
}

/// The in-process `WsConnector` (S-mock §8.1): the mediator's host opens a connection of the
/// world's mediator, any other host is refused and counted.
pub struct MockConnector(Net);

#[async_trait]
impl WsConnector for MockConnector {
    async fn connect(
        &self,
        request: WsRequest,
    ) -> Result<(Box<dyn WsSender>, async_channel::Receiver<WsEvent>), NetError> {
        let (sender, events, _) = self.0.open(request).await?;
        Ok((sender, events))
    }
}

/// What [`Net::open`] gives: the sending half, the stream of events and the mediator's connection
/// (`None` for a relay side).
pub type Opened = (
    Box<dyn WsSender>,
    async_channel::Receiver<WsEvent>,
    Option<ConnId>,
);

impl Net {
    /// [`MockConnector`]'s connect, which also says WHICH mediator connection it opened: the
    /// parent-resident fake (`pipe.rs`, TH5a A3) ends exactly that one with 1006 when its child dies.
    pub async fn open(&self, request: WsRequest) -> Result<Opened, NetError> {
        let net = self;
        net.0.attempts.fetch_add(1, Ordering::SeqCst);
        net.0.changed.notify_waiters();
        assert!(
            !net.0.panic_next_connect.swap(false, Ordering::SeqCst),
            "the fake connector was told to panic"
        );
        // A connect that hangs while the hold is up (the actor's deadline drops this future).
        let mut hold = net.0.hold_connects.subscribe();
        let _ = hold.wait_for(|held| !*held).await;
        let host = host_of(&request.url);
        if request.kind == WsKind::Rendezvous && host.is_some_and(is_relay_host) {
            let failing = lock(&net.0.fail_relay_connects).clone();
            if let Some(error) = failing {
                net.0.changed.notify_waiters();
                return Err(error);
            }
            // A URL the relay does not take is recorded there (`rendezvous-*`) and its side closed
            // 4000 (P10: upgraded, then closed, as a real relay does).
            let side = net.0.relay.connect(&request.url, Role::Rid);
            let (events, stream) = async_channel::unbounded();
            lock(&net.0.relay_open).insert(side, events);
            lock(&net.0.relay_opened).push(side);
            net.flush();
            return Ok((
                Box::new(MockSender {
                    net: net.clone(),
                    to: To::Relay(side),
                }),
                stream,
                None,
            ));
        }
        if request.kind != WsKind::Mediator || !host.is_some_and(is_mediator_host) {
            net.0.refused_hosts.fetch_add(1, Ordering::SeqCst);
            net.0
                .violations
                .record("connect-to-a-host-the-fake-does-not-serve", &[]);
            return Err(NetError::Refused("websocket"));
        }
        let failing = lock(&net.0.fail_connects).clone();
        if let Some(error) = failing {
            net.0.changed.notify_waiters();
            return Err(error);
        }
        // A URL the mediator does not take is recorded there (`mediator-connect-refused`).
        let conn = net
            .0
            .mediator
            .connect(&request.url)
            .map_err(|_| NetError::Failed("websocket"))?;
        let (events, stream) = async_channel::unbounded();
        lock(&net.0.open).insert(conn, events);
        lock(&net.0.opened).push(conn);
        net.flush();
        Ok((
            Box::new(MockSender {
                net: net.clone(),
                to: To::Mediator(conn),
            }),
            stream,
            Some(conn),
        ))
    }

    /// The client of `conn` went away without a close frame (its process died): the mediator ends
    /// it with 1006 ([`Mediator::lost`]) and the connector forgets it (its event stream ends).
    pub fn lose(&self, conn: ConnId) {
        self.0.mediator.lost(conn);
        lock(&self.0.open).remove(&conn);
        self.flush();
    }
}

/// Which server a connection of the connector speaks to.
#[derive(Clone, Copy)]
enum To {
    Mediator(ConnId),
    Relay(SideId),
}

/// The sending half of one connection: a frame goes into the mediator (or the relay) at once.
struct MockSender {
    net: Net,
    to: To,
}

impl MockSender {
    fn is_open(&self) -> bool {
        match self.to {
            To::Mediator(conn) => lock(&self.net.0.open).contains_key(&conn),
            To::Relay(side) => lock(&self.net.0.relay_open).contains_key(&side),
        }
    }
}

#[async_trait]
impl WsSender for MockSender {
    async fn send(&self, frame: Vec<u8>) -> Result<(), NetError> {
        if let To::Relay(_) = self.to {
            let mut stall = self.net.0.stall_relay_sends.subscribe();
            if *stall.borrow() {
                self.net.0.stalled.fetch_add(1, Ordering::SeqCst);
                self.net.0.changed.notify_waiters();
                let _ = stall.wait_for(|stalled| !*stalled).await;
            }
        }
        if let To::Mediator(_) = self.to {
            let mut stall = self.net.0.stall_mediator_sends.subscribe();
            if *stall.borrow() {
                self.net.0.stalled_mediator.fetch_add(1, Ordering::SeqCst);
                self.net.0.changed.notify_waiters();
                let _ = stall.wait_for(|stalled| !*stalled).await;
            }
        }
        if !self.is_open() {
            if let To::Mediator(conn) = self.to {
                lock(&self.net.0.late_sends).push(conn);
            }
            return Err(NetError::Failed("websocket send"));
        }
        match self.to {
            To::Mediator(conn) => self.net.0.mediator.receive(conn, frame),
            To::Relay(side) => self.net.0.relay.receive(side, frame),
        }
        self.net.flush();
        Ok(())
    }

    async fn close(&self, code: u16, _reason: String) -> Result<(), NetError> {
        if let To::Relay(_) = self.to {
            let mut stall = self.net.0.stall_relay_closes.subscribe();
            let _ = stall.wait_for(|stalled| !*stalled).await;
        }
        if !self.is_open() {
            return Err(NetError::Failed("websocket close"));
        }
        match self.to {
            To::Mediator(conn) => {
                self.net.0.mediator.client_close(conn, code);
                lock(&self.net.0.open).remove(&conn);
            }
            To::Relay(side) => {
                self.net.0.relay.client_close(side, code);
                lock(&self.net.0.relay_open).remove(&side);
            }
        }
        self.net.flush();
        Ok(())
    }
}

/// One request in [`Net::directory_pending`] while it lives: counted when made, uncounted when dropped (served, or the
/// client's future dropped).
struct Pending<'a>(&'a AtomicUsize);

impl<'a> Pending<'a> {
    fn count(gauge: &'a AtomicUsize) -> Self {
        gauge.fetch_add(1, Ordering::SeqCst);
        Self(gauge)
    }
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The in-process `HttpsClient`: the directory's host is served by [`super::directory`] (TH6 L2b; a request pends while
/// [`Net::hold_directory`] holds); every other request (the blob mirror: TH8) is refused, counted and recorded.
pub struct MockHttps(Net);

#[async_trait]
impl HttpsClient for MockHttps {
    async fn execute(&self, request: HttpsRequest) -> Result<HttpsResponse, NetError> {
        let net = &self.0.0;
        if is_directory(&request.url) {
            let mut hold = net.hold_directory.subscribe();
            if *hold.borrow() {
                net.directory_held.fetch_add(1, Ordering::SeqCst);
                let _pending = Pending::count(&net.directory_pending);
                net.changed.notify_waiters();
                let _ = hold.wait_for(|held| !*held).await;
            }
            let answer = net.directory.serve(&request);
            net.changed.notify_waiters();
            return answer;
        }
        self.0.0.refused_https.fetch_add(1, Ordering::SeqCst);
        self.0
            .0
            .violations
            .record("https-request-the-fake-does-not-serve", &[]);
        Err(NetError::Refused("https"))
    }
}

impl MockWorld {
    /// The world's network (the connector's connections onto its mediator).
    pub fn net(&self) -> &Net {
        &self.net
    }

    /// The servers of the fake as the parts carry them: `.invalid` hosts and the world's SK.public.
    pub fn server_params(&self) -> ServerParams {
        ServerParams {
            mediator_url: MEDIATOR_URL.to_owned(),
            rendezvous_url: RENDEZVOUS_URL.to_owned(),
            directory_url: DIRECTORY_URL.to_owned(),
            blob_url: BLOB_URL.to_owned(),
            chat_server_keys: vec![self.server_key.public],
        }
    }

    /// `ThreemaParts` with the fake's connector and HTTPS client (counted in [`Net::made`]) and
    /// [`MockWorld::server_params`]: what TH3's actor is started with.
    pub fn parts(&self) -> ThreemaParts {
        let (ws, https) = (self.net.clone(), self.net.clone());
        ThreemaParts::from_makers(
            move || {
                ws.0.made.fetch_add(1, Ordering::SeqCst);
                ws.connector()
            },
            move || https.https(),
            self.server_params(),
        )
    }
}

/// The parts of `world` (TH3 critic M1): what an actor test hands `ThreemaNetwork::new`, as
/// `mock::parts(&world)`, the form the source check's rule 3 takes.
pub fn parts(world: &MockWorld) -> ThreemaParts {
    world.parts()
}
