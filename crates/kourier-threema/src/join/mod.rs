//! The device join (TH4): Kourier links as a NEW device (ND) of a Threema ID by showing a QR code that the
//! phone (the existing device, ED) scans.
//!
//! - `qr`: the offer. The authentication key, the rendezvous path, the relay URL and the text of the QR
//!   code (`threema://device-group/join#<url-safe base64>`).
//! - `emoji`: the three emoji of the rendezvous path hash that both devices show for the user to compare.
//! - `essential` (J4a): the checks of the phone's `EssentialData` before anything is written, and the
//!   [`essential::Import`] that passes them (the keys in `Zeroizing`, the device ids drawn).
//! - `rules` (J4a): the joined ID against the login's relink, the profile's other accounts and the
//!   allow-list, in that order.
//! - `events` (J4b): what a joined account states to the core (`AccountReady` first, the settings, the
//!   contacts, the groups with their [`events::group_key`]), over a snapshot of the stored import: the
//!   join's zone 2 and `Rebuild` write the same rows.
//! - [`Join`] (J3), the JOIN TASK, the twin of `conn::Task` (`spawn_local` on the actor's thread, a stop,
//!   `next`): ONE rendezvous WebSocket through the parts' connector ([`WsKind::Rendezvous`], the offer's
//!   URL), libthreema's `RendezvousProtocol` as the Rendezvous Initiator Device (RID) and NOT the nominator
//!   (`new_as_rid(false, ..)`: the phone nominates, `TP/md-d2d-join.proto:52-55`, Desktop `newAsRid(false,
//!   ..)`), each outgoing frame as ONE binary message, each incoming message through `add_chunks` and
//!   `process_frame` until `None`; after the nomination the upper-layer protocol of
//!   `TP/md-d2d-join.proto` (`EdToNd`: `Begin`, `BlobData`*, `EssentialData`). It reports [`JoinEvent`]s to
//!   the actor and WRITES NOTHING (the import is the actor's, J4/J5). After `EssentialData` it waits for
//!   the actor's word: [`Join::register`] (the mediator's `ServerInfo` said `NEW` and zone 2 is written:
//!   `NdToEd { Registered }` as ONE frame, then closed with 1000, since Kourier asks for no history,
//!   `TP/md-d2d-join.proto` steps 7-8) or [`Join::stop`].
//!
//! THE DEADLINES are the actor's clock only (decision C8; no sleep, no wall clock here):
//! [`CONNECT_WITHIN`] for the relay's connect, [`SCAN_WITHIN`] from the QR shown to the phone's first
//! message (its `Hello`), [`NOMINATE_WITHIN`] from that `Hello` to the nomination, [`BEGIN_WITHIN`] from the
//! emoji to `Begin` (the person compares and taps), [`ESSENTIAL_WITHIN`] after the LAST message heard from
//! `Begin` on (every message pushes it out) until `EssentialData`.
//!
//! THE BOUNDS ON ARRIVAL (critic H4, S7): a `BlobData` whose id is not 16 bytes fails the join
//! `Unsupported` at once, before it is kept; a running count and byte sum (id + data) at EACH
//! `BlobData`; over [`MAX_BLOBS`] blobs or [`MAX_BLOB_BYTES`] in all fails the join `Unsupported` at once. `BlobData` or
//! `EssentialData` before `Begin`, a second `Begin`, a second `EssentialData`, a `BlobData` after
//! `EssentialData` are protocol errors (closed 4100).
//!
//! THE CLOSE CODES (critic H3, `TP/md-d2d-rendezvous.proto` "WebSocket Close Codes"): Kourier closes with
//! 1000 (a cancel, a timeout before the nomination), 4000 (the rendezvous protocol broken), 4100 (an
//! upper-layer error after the nomination). What the relay or the phone closes with: 4003 before the
//! phone's `Hello` -> `Expired`; 4004 at any step, and 1000 or a 41xx after the `Hello` -> the phone's
//! side, `Unauthorized`; 4000, 1006, anything else, and a connect that fails -> `Transient`; an untrusted
//! certificate -> `Unsupported`.
//!
//! A FRAME WRITE is bounded like the close ([`SEND_WITHIN`], P8): a stuck relay cannot hold the actor's
//! `CancelLogin`, `Logout` or `Shutdown` (they await the join).
//!
//! Nothing here logs a secret: the key, the path, the URL, the QR text, the path hash or a frame's content
//! never reach a log line, an error string or a `Debug` output; errors are `&'static str` names.

pub(crate) mod emoji;
pub(crate) mod essential;
pub(crate) mod events;
pub(crate) mod qr;
pub(crate) mod rules;

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::Clock;
use kourier_model::{BackendError, ErrorKind};
use libthreema::d2d_rendezvous::{
    RendezvousAuthenticationKey, RendezvousOutgoingFrame, RendezvousPathProcessResult,
    RendezvousPathStateUpdate, RendezvousProtocol,
};
use libthreema::protobuf::{common, d2d_join};
use prost::Message as _;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, spawn_local};
use zeroize::{Zeroize as _, Zeroizing};

use crate::{NetError, WsConnector, WsEvent, WsKind, WsRequest, WsSender};

use essential::BLOB_ID_LEN;
use qr::{Offer, RELAY_PATH_ID};

/// The relay's connect (DNS, TCP, TLS, the upgrade) must finish within this much of the actor's
/// clock (Kourier's number, TH3's `HANDSHAKE_WITHIN`).
pub(crate) const CONNECT_WITHIN: Duration = Duration::from_secs(20);
/// From the QR shown to the phone's first message (C8).
pub(crate) const SCAN_WITHIN: Duration = Duration::from_secs(5 * 60);
/// From the phone's `Hello` to its nomination: the handshake and the nomination (C8).
pub(crate) const NOMINATE_WITHIN: Duration = Duration::from_secs(60);
/// From the emoji to `Begin`: the person compares and confirms on the phone (C8). [U: real phone] whether
/// the phone gives up earlier on its own.
pub(crate) const BEGIN_WITHIN: Duration = Duration::from_secs(5 * 60);
/// From `Begin` on, `EssentialData` must come within this much after the LAST message heard (C8).
pub(crate) const ESSENTIAL_WITHIN: Duration = Duration::from_secs(2 * 60);
/// The most `BlobData` of one join (C5, C7, H4).
pub(crate) const MAX_BLOBS: usize = 1_000;
/// The most bytes of `BlobData` of one join, in all (C5, C7, H4).
pub(crate) const MAX_BLOB_BYTES: usize = 32 * 1_048_576;
/// How long a stopping join waits for its close frame to be written.
const CLOSE_WITHIN: Duration = Duration::from_secs(1);
/// How long a frame may take to be written to the rendezvous socket (P8: a relay that stopped reading
/// must not hold the actor's `CancelLogin`, `Logout` or `Shutdown`, which await the join). The same
/// number as [`CLOSE_WITHIN`], Kourier's own: frames are small, so a write that takes a second of the
/// actor's clock is a stuck socket; it is marked broken and the join goes on to its stop or its deadline.
const SEND_WITHIN: Duration = CLOSE_WITHIN;

/// Kourier's close codes (`TP/md-d2d-rendezvous.proto`).
const NORMAL: u16 = 1000;
const RENDEZVOUS_ERROR: u16 = 4000;
const ULP_ERROR: u16 = 4100;

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// What the join tells the actor, in order.
pub(crate) enum JoinEvent {
    /// The rendezvous socket is open: the QR text to show (step `qr`).
    Qr(Zeroizing<String>),
    /// The phone nominated the path: the three emoji of its path hash (step `emoji`).
    Emoji([&'static str; 3]),
    /// `Begin` came: the person confirmed on the phone (step `linking`).
    Begun,
    /// `EssentialData` came, with the blobs before it. The rendezvous stays open: the actor answers
    /// with the import and `Registered` (J4/J5), or stops the join. [U: real phone] how long the phone
    /// waits for `Registered` (the mediator's `ServerInfo` comes first).
    Essential(Box<Received>),
    /// The join failed (its socket is closed); the actor reports it at the step it is at.
    Failed(BackendError),
}

/// What the phone handed over (MEMORY only, until the import; nothing of it is logged).
pub(crate) struct Received {
    /// The phone's `EssentialData` (keys inside: zeroized on drop).
    pub(crate) data: d2d_join::EssentialData,
    /// The `BlobData` that came before it, in order (C5: J4 keeps the referenced ones).
    pub(crate) blobs: Vec<common::BlobData>,
}

impl Drop for Received {
    fn drop(&mut self) {
        wipe_keys(&mut self.data);
    }
}

/// Wipes the keys `EssentialData` carries that are still in it: CK, the device cookie, DGK.
fn wipe_keys(data: &mut d2d_join::EssentialData) {
    if let Some(identity) = data.identity_data.as_mut() {
        identity.ck.zeroize();
        identity.csp_device_cookie.zeroize();
    }
    if let Some(group) = data.device_group_data.as_mut() {
        group.dgk.zeroize();
    }
}

/// Prints the counts only.
impl std::fmt::Debug for Received {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Received({} blobs)", self.blobs.len())
    }
}

/// What a join is made with (the actor's).
pub(crate) struct Setup {
    /// The connector of the actor's transports.
    pub(crate) ws: Arc<dyn WsConnector>,
    /// `ServerParams::rendezvous_url`, the relay's URL template.
    pub(crate) rendezvous_url: String,
    /// The actor's clock.
    pub(crate) clock: Arc<dyn Clock>,
}

/// The join task panicked (logged once, no content).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Panicked;

/// What [`Join::next`] gives the actor.
pub(crate) enum Next {
    /// One event of the join.
    Event(JoinEvent),
    /// The task ended (after a `Failed`, or by a panic).
    Ended(Result<(), Panicked>),
}

/// What the actor tells the running join (once).
enum Order {
    /// Close the socket with this code.
    Close(u16),
    /// Send `Registered`, then close with 1000; the answer says whether the frame went out (P5).
    Registered(oneshot::Sender<Registration>),
}

/// What became of the `Registered` frame (P5, [`Join::register`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Registration {
    /// The frame was written to the rendezvous socket.
    Sent,
    /// It was not: the phone's side was gone before (the join had ended, or the write failed or took
    /// too long). The device IS registered on the mediator all the same.
    NotSent,
}

/// The running join, as the actor holds it.
pub(crate) struct Join {
    events: mpsc::UnboundedReceiver<JoinEvent>,
    stop: Option<oneshot::Sender<Order>>,
    handle: JoinHandle<()>,
}

impl Join {
    /// Starts the join on the actor's `LocalSet`: it connects to the relay at once.
    pub(crate) fn start(setup: Setup) -> Self {
        let (tx, events) = mpsc::unbounded_channel();
        let (stop, stopped) = oneshot::channel();
        let handle = spawn_local(run(setup, tx, stopped));
        Self {
            events,
            stop: Some(stop),
            handle,
        }
    }

    /// Stops the join (an open rendezvous socket is closed with `code`) and waits for its end.
    pub(crate) async fn stop(mut self, code: u16) -> Result<(), Panicked> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(Order::Close(code));
        }
        joined(self.handle.await)
    }

    /// The device is registered (the mediator's first `ServerInfo` said `NEW`, the core applied
    /// `AccountReady`): `NdToEd { Registered }` goes to the phone as ONE frame on the nominated path,
    /// then the rendezvous is closed with 1000 (Kourier asks for no history); waits for the join's end
    /// and says whether the frame was written ([`Registration`], P5): a join that had ended already (the
    /// phone left first) or a write that failed is `NotSent`.
    pub(crate) async fn register(mut self) -> Result<Registration, Panicked> {
        let (reply, mut answer) = oneshot::channel();
        let ordered = self
            .stop
            .take()
            .is_some_and(|stop| stop.send(Order::Registered(reply)).is_ok());
        joined(self.handle.await)?;
        if !ordered {
            return Ok(Registration::NotSent);
        }
        // The task has ended: its answer is there, or it ended before it read the order.
        Ok(answer.try_recv().unwrap_or(Registration::NotSent))
    }

    /// The next event of the running join; pends forever without one.
    pub(crate) async fn next(join: &mut Option<Self>) -> Next {
        let Some(j) = join else {
            return std::future::pending().await;
        };
        if let Some(event) = j.events.recv().await {
            return Next::Event(event);
        }
        let ended = joined((&mut j.handle).await);
        *join = None;
        Next::Ended(ended)
    }
}

/// The task's result: it is never aborted, so a `JoinError` is its panic.
fn joined(result: Result<(), tokio::task::JoinError>) -> Result<(), Panicked> {
    result.map_err(|_| {
        tracing::error!("the join task panicked");
        Panicked
    })
}

/// Why one `EdToNd` was not taken (see [`decode_ulp`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UlpRefused {
    /// The bytes do not decode.
    Malformed,
    /// A count on the wire is over its bound (S2); the rule's name.
    Over(&'static str),
}

/// Decodes one `EdToNd` and wipes the frame it came from (S1): the decoded message holds the keys
/// from here on (`Received` and `checked` wipe those). BEFORE the decode (S2) the frame's wire bytes
/// are counted against the real bounds ([`essential::wire_check`]): a frame with more contacts, groups,
/// lists, hashed nonces or members of a group than C7 allows is refused without being decoded, because
/// the decoded structs are hundreds of times the size of their two wire bytes.
fn decode_ulp(frame: &mut Vec<u8>) -> Result<d2d_join::EdToNd, UlpRefused> {
    let decoded = match essential::wire_check(frame, &essential::Bounds::REAL) {
        Ok(()) => d2d_join::EdToNd::decode(frame.as_slice()).map_err(|_| UlpRefused::Malformed),
        Err(essential::Wire::Over(rule)) => Err(UlpRefused::Over(rule)),
        Err(essential::Wire::Malformed) => Err(UlpRefused::Malformed),
    };
    frame.zeroize();
    decoded
}

/// A failure: the code Kourier closes with (`None`: the socket is gone already) and the error.
struct Fail {
    close: Option<u16>,
    error: BackendError,
}

impl Fail {
    fn new(close: Option<u16>, kind: ErrorKind, detail: &'static str) -> Self {
        Self {
            close,
            error: BackendError::new(kind, detail),
        }
    }

    fn transient(close: Option<u16>, detail: &'static str) -> Self {
        Self::new(close, ErrorKind::Transient { retry_after: None }, detail)
    }
}

/// The task's body: the failure, if any, is reported after the socket closed.
async fn run(
    setup: Setup,
    events: mpsc::UnboundedSender<JoinEvent>,
    stop: oneshot::Receiver<Order>,
) {
    if let Err(fail) = join(&setup, &events, stop).await {
        let _ = events.send(JoinEvent::Failed(fail.error));
    }
}

/// Where the join is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Phase {
    /// The QR is shown; nothing came from the phone yet.
    Shown,
    /// The phone's `Hello` came: the handshake and the nomination run.
    Hello,
    /// The phone nominated the path: the emoji are shown, `Begin` is awaited.
    Nominated,
    /// `Begin` came: blobs and `EssentialData` are awaited.
    Begun,
    /// `EssentialData` was handed to the actor.
    Delivered,
}

/// The join up to its end: `Ok` when the actor stopped it, else the failure (socket closed).
async fn join(
    setup: &Setup,
    events: &mpsc::UnboundedSender<JoinEvent>,
    mut stop: oneshot::Receiver<Order>,
) -> Result<(), Fail> {
    let clock = &*setup.clock;
    let offer = Offer::new(&mut rand::rng(), &setup.rendezvous_url).map_err(|_| {
        tracing::error!("the rendezvous URL template of the parts is invalid");
        Fail::new(None, ErrorKind::Internal, "the join offer")
    })?;
    let connect_by = clock.now_ms().saturating_add(ms(CONNECT_WITHIN));
    let connect = setup.ws.connect(WsRequest {
        url: offer.url().to_owned(),
        kind: WsKind::Rendezvous,
    });
    let (tx, rx) = tokio::select! {
        biased;
        _ = &mut stop => return Ok(()),
        () = clock.sleep_until(connect_by) => {
            tracing::info!("the rendezvous relay was not reached in time");
            return Err(Fail::transient(None, "rendezvous connect timed out"));
        }
        opened = connect => match opened {
            Ok(halves) => halves,
            Err(error) => {
                tracing::info!(%error, "the rendezvous relay was not reached");
                return Err(match error {
                    NetError::Untrusted(_) => {
                        Fail::new(None, ErrorKind::Unsupported, "rendezvous certificate untrusted")
                    }
                    NetError::Refused(_) | NetError::Failed(_) | NetError::CertificateDates => {
                        Fail::transient(None, "rendezvous connect failed")
                    }
                });
            }
        },
    };
    let protocol = RendezvousProtocol::new_as_rid(
        false,
        RendezvousAuthenticationKey(*offer.ak()),
        &[RELAY_PATH_ID],
    );
    let mut s = Session {
        clock: setup.clock.clone(),
        tx,
        broken: false,
        protocol,
        events: events.clone(),
        phase: Phase::Shown,
        deadline: Some(clock.now_ms().saturating_add(ms(SCAN_WITHIN))),
        blobs: Vec::new(),
        blob_bytes: 0,
    };
    let _ = events.send(JoinEvent::Qr(offer.text()));
    drop(offer);
    loop {
        let deadline = s.deadline;
        let step = tokio::select! {
            biased;
            order = &mut stop => {
                match order {
                    Ok(Order::Registered(reply)) => {
                        let _ = reply.send(s.registered().await);
                    }
                    Ok(Order::Close(code)) => s.close(code).await,
                    Err(_) => s.close(NORMAL).await,
                }
                return Ok(());
            }
            event = rx.recv() => match event {
                Ok(WsEvent::Binary(message)) => s.message(&message).await,
                Ok(WsEvent::Closed { code, reason: _ }) => Err(s.closed(code)),
                // The socket ended without a close frame.
                Err(_) => Err(s.closed(1006)),
            },
            () = clock.sleep_until(deadline.unwrap_or(u64::MAX)), if deadline.is_some() => {
                Err(s.timed_out())
            }
        };
        if let Err(fail) = step {
            if let Some(code) = fail.close {
                s.close(code).await;
            }
            return Err(fail);
        }
    }
}

/// One open rendezvous socket and the protocol over it.
struct Session {
    clock: Arc<dyn Clock>,
    tx: Box<dyn WsSender>,
    /// A send failed: the socket is going; its close event says how.
    broken: bool,
    protocol: RendezvousProtocol,
    events: mpsc::UnboundedSender<JoinEvent>,
    phase: Phase,
    /// When the current phase times out (`None`: no deadline, after `EssentialData`).
    deadline: Option<u64>,
    blobs: Vec<common::BlobData>,
    blob_bytes: usize,
}

impl Session {
    fn after(&self, d: Duration) -> Option<u64> {
        Some(self.clock.now_ms().saturating_add(ms(d)))
    }

    /// Kourier closes the socket (bounded by [`CLOSE_WITHIN`] of the actor's clock).
    async fn close(&mut self, code: u16) {
        let close = self.tx.close(code, String::new());
        let clock = self.clock.clone();
        tokio::select! {
            _ = close => {}
            () = clock.sleep(CLOSE_WITHIN) => {}
        }
    }

    /// One frame of libthreema as ONE binary message (header, then payload). A failed send, or one
    /// that takes more than [`SEND_WITHIN`] of the actor's clock (P8), marks the socket broken: the
    /// close event that follows decides (the phone's close code may wait behind the message that was
    /// answered).
    async fn send(&mut self, frame: &RendezvousOutgoingFrame) {
        if self.broken {
            return;
        }
        let (header, payload) = frame.encode();
        let mut message = Vec::with_capacity(header.len() + payload.len());
        message.extend_from_slice(&header);
        message.extend_from_slice(payload);
        let clock = self.clock.clone();
        let sent = {
            let write = self.tx.send(message);
            tokio::select! {
                written = write => written.is_ok(),
                () = clock.sleep(SEND_WITHIN) => {
                    tracing::warn!("the rendezvous socket took no frame in time");
                    false
                }
            }
        };
        if !sent {
            self.broken = true;
        }
    }

    /// `NdToEd { Registered }` as ONE frame on the nominated path, then the socket closed with 1000
    /// ("the ULP exited successfully"). Only after `EssentialData` (the actor's order comes then);
    /// a frame libthreema cannot make closes with 4100. `Sent` only when the write succeeded (P5).
    async fn registered(&mut self) -> Registration {
        let registered = d2d_join::NdToEd {
            content: Some(d2d_join::nd_to_ed::Content::Registered(
                d2d_join::Registered {},
            )),
        }
        .encode_to_vec();
        let frame = if self.phase == Phase::Delivered {
            self.protocol
                .create_ulp_frame(registered)
                .ok()
                .and_then(|r| r.outgoing_frame)
        } else {
            None
        };
        let Some(frame) = frame else {
            tracing::warn!("the Registered frame could not be made");
            self.close(ULP_ERROR).await;
            return Registration::NotSent;
        };
        self.send(&frame).await;
        let sent = !self.broken;
        if sent {
            tracing::info!("the device is registered: the join is done");
        } else {
            // The actor logs the one WARN of this (P5); not a second one here.
            tracing::info!("the Registered frame was not written");
        }
        self.close(NORMAL).await;
        if sent {
            Registration::Sent
        } else {
            Registration::NotSent
        }
    }

    /// The rendezvous protocol is broken (a frame that does not open or decode, one out of order):
    /// the operation's name is logged, never the error's text.
    fn rendezvous_error(&self, what: &'static str) -> Fail {
        tracing::warn!(what, "the rendezvous was refused");
        Fail::new(
            Some(RENDEZVOUS_ERROR),
            ErrorKind::Unauthorized,
            "rendezvous protocol error",
        )
    }

    /// The phone's blobs are over Kourier's bounds (H4, S7): `Unsupported`, closed 4100.
    fn over_the_bounds() -> Fail {
        Fail::new(
            Some(ULP_ERROR),
            ErrorKind::Unsupported,
            "the phone's blobs are over the bounds",
        )
    }

    /// The upper-layer protocol of the join is broken.
    fn ulp_error(what: &'static str) -> Fail {
        tracing::warn!(what, "the phone's join data was refused");
        Fail::new(
            Some(ULP_ERROR),
            ErrorKind::Unauthorized,
            "join protocol error",
        )
    }

    /// One message of the relay: its chunks into the protocol, then every whole frame.
    async fn message(&mut self, message: &[u8]) -> Result<(), Fail> {
        match self.phase {
            Phase::Shown => {
                self.phase = Phase::Hello;
                self.deadline = self.after(NOMINATE_WITHIN);
            }
            // Progress: every message after `Begin` pushes the deadline out.
            Phase::Begun => self.deadline = self.after(ESSENTIAL_WITHIN),
            Phase::Hello | Phase::Nominated | Phase::Delivered => {}
        }
        if self.protocol.add_chunks(RELAY_PATH_ID, &[message]).is_err() {
            return Err(self.rendezvous_error("add chunks"));
        }
        loop {
            match self.protocol.process_frame(RELAY_PATH_ID) {
                Ok(Some(result)) => self.result(result).await?,
                Ok(None) => return Ok(()),
                Err(_) => return Err(self.rendezvous_error("process frame")),
            }
        }
    }

    /// The steps of `RendezvousPathProcessResult`'s doc (`LT/d2d_rendezvous/mod.rs:150-178`).
    async fn result(&mut self, result: RendezvousPathProcessResult) -> Result<(), Fail> {
        let RendezvousPathProcessResult {
            state_update,
            outgoing_frame,
            incoming_ulp_data,
        } = result;
        if self.phase < Phase::Nominated {
            if incoming_ulp_data.is_some() {
                return Err(self.rendezvous_error("data before the nomination"));
            }
            if let Some(frame) = outgoing_frame {
                self.send(&frame).await;
            }
            match state_update {
                None => {}
                // Kourier is not the nominator: the phone nominates.
                Some(RendezvousPathStateUpdate::AwaitingNominate { measured_rtt: _ }) => {
                    tracing::debug!("the rendezvous path awaits the phone's nomination");
                }
                Some(RendezvousPathStateUpdate::Nominated { rph }) => {
                    let mut hash = rph.0;
                    let glyphs = emoji::emoji_of(&hash);
                    hash.zeroize();
                    self.phase = Phase::Nominated;
                    self.deadline = self.after(BEGIN_WITHIN);
                    let _ = self.events.send(JoinEvent::Emoji(glyphs));
                }
            }
            return Ok(());
        }
        if state_update.is_some() {
            return Err(self.rendezvous_error("a path state after the nomination"));
        }
        if let Some(frame) = outgoing_frame {
            self.send(&frame).await;
        }
        match incoming_ulp_data {
            // S1: libthreema decrypted the frame IN PLACE, so this buffer is the one plaintext copy of
            // the keys of `EssentialData`: it is wiped when it is done with, on every path.
            Some(frame) => self.ulp(&mut Zeroizing::new(frame)),
            None => Ok(()),
        }
    }

    /// One `EdToNd` of the phone (`frame` is wiped by the decode).
    fn ulp(&mut self, frame: &mut Vec<u8>) -> Result<(), Fail> {
        use d2d_join::ed_to_nd::Content;
        let message = match decode_ulp(frame) {
            Ok(message) => message,
            Err(UlpRefused::Malformed) => {
                return Err(Self::ulp_error("an EdToNd that does not decode"));
            }
            // S2: counted on the wire, never decoded; `Unsupported` like every bound (E3).
            Err(UlpRefused::Over(rule)) => {
                tracing::warn!(rule, "the phone's join data was refused");
                return Err(Fail {
                    close: Some(ULP_ERROR),
                    error: essential::Refused(rule).error(),
                });
            }
        };
        match (message.content, self.phase) {
            (Some(Content::Begin(d2d_join::Begin {})), Phase::Nominated) => {
                self.phase = Phase::Begun;
                self.deadline = self.after(ESSENTIAL_WITHIN);
                let _ = self.events.send(JoinEvent::Begun);
                Ok(())
            }
            (Some(Content::BlobData(blob)), Phase::Begun) => {
                // S7: a blob id is exactly 16 bytes (`BLOB_ID_LEN`), refused ON ARRIVAL, before the
                // blob is kept: an id of any size would otherwise be held until `EssentialData`
                // outside the byte sum below, which counts the id with the data.
                if blob.id.len() != BLOB_ID_LEN {
                    tracing::warn!("a blob of the phone has an id that is not 16 bytes");
                    return Err(Self::over_the_bounds());
                }
                let count = self.blobs.len().saturating_add(1);
                let bytes = self
                    .blob_bytes
                    .saturating_add(blob.id.len())
                    .saturating_add(blob.data.len());
                if count > MAX_BLOBS || bytes > MAX_BLOB_BYTES {
                    tracing::warn!(count, "the phone's blobs are over Kourier's bounds");
                    return Err(Self::over_the_bounds());
                }
                self.blob_bytes = bytes;
                self.blobs.push(blob);
                Ok(())
            }
            (Some(Content::EssentialData(data)), Phase::Begun) => {
                self.phase = Phase::Delivered;
                self.deadline = None;
                let received = Received {
                    data,
                    blobs: std::mem::take(&mut self.blobs),
                };
                let _ = self.events.send(JoinEvent::Essential(Box::new(received)));
                Ok(())
            }
            (Some(Content::Begin(_)), _) => Err(Self::ulp_error("Begin out of order")),
            (Some(Content::BlobData(_)), _) => Err(Self::ulp_error("BlobData out of order")),
            (Some(Content::EssentialData(data)), _) => {
                // Refused, but its keys are wiped like a delivered one's (S1): `Received`'s drop.
                drop(Received {
                    data,
                    blobs: Vec::new(),
                });
                Err(Self::ulp_error("EssentialData out of order"))
            }
            (None, _) => Err(Self::ulp_error("an empty EdToNd")),
        }
    }

    /// The relay (or the phone through it) closed the socket with `code` (H3, P7). [U: real phone] which
    /// code the phone closes with when the person declines the emoji or cancels (1000 or a 41xx: both
    /// are the phone's side here); [U: real server] whether the relay sends 4003 at all before its peer.
    fn closed(&self, code: u16) -> Fail {
        tracing::info!(code, phase = ?self.phase, "the rendezvous relay closed the connection");
        let hello = self.phase >= Phase::Hello;
        match code {
            4003 if !hello => Fail::new(None, ErrorKind::Expired, "nobody scanned in time"),
            // The phone went (4004: at any step, also one that scanned and left before its `Hello`
            // came, P7), or ended (1000) or refused (41xx) after it scanned.
            4004 => Fail::new(None, ErrorKind::Unauthorized, "the phone ended the join"),
            1000 | 4100..=4199 if hello => {
                Fail::new(None, ErrorKind::Unauthorized, "the phone ended the join")
            }
            _ => Fail::transient(None, "the rendezvous connection closed"),
        }
    }

    /// The current phase's deadline passed.
    fn timed_out(&self) -> Fail {
        tracing::info!(phase = ?self.phase, "the join timed out");
        match self.phase {
            Phase::Shown => Fail::new(Some(NORMAL), ErrorKind::Expired, "nobody scanned in time"),
            Phase::Hello => Fail::new(
                Some(NORMAL),
                ErrorKind::Unauthorized,
                "the phone did not nominate in time",
            ),
            Phase::Nominated => Fail::new(
                Some(ULP_ERROR),
                ErrorKind::Unauthorized,
                "the phone did not begin in time",
            ),
            Phase::Begun | Phase::Delivered => {
                Fail::transient(Some(ULP_ERROR), "the phone's data did not come in time")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libthreema::protobuf::d2d_sync;

    /// Synthetic key bytes, each a pattern of no other test value.
    fn essential() -> d2d_join::EssentialData {
        d2d_join::EssentialData {
            identity_data: Some(d2d_join::essential_data::IdentityData {
                identity: "S1TEST01".to_owned(),
                ck: vec![0xc7; 32],
                csp_device_cookie: vec![0x6d; 16],
                csp_server_group: "ds7".to_owned(),
            }),
            device_group_data: Some(d2d_join::essential_data::DeviceGroupData {
                dgk: vec![0x93; 32],
            }),
            ..Default::default()
        }
    }

    /// S1: the frame libthreema decrypted in place (the one plaintext copy of the keys) is wiped by the
    /// decode, whether the decode worked or not; the decoded message still holds the data.
    #[test]
    fn the_decrypted_frame_is_wiped_by_the_decode() {
        let mut frame = d2d_join::EdToNd {
            content: Some(d2d_join::ed_to_nd::Content::EssentialData(essential())),
        }
        .encode_to_vec();
        assert!(frame.len() > 80, "the frame holds the keys");
        let decoded = decode_ulp(&mut frame).expect("a frame that decodes");
        assert!(frame.is_empty(), "the decrypted frame was not wiped");
        match decoded.content {
            Some(d2d_join::ed_to_nd::Content::EssentialData(data)) => {
                assert!(
                    data.identity_data
                        .as_ref()
                        .is_some_and(|i| i.ck == vec![0xc7; 32]),
                    "the decoded message lost the key"
                );
            }
            _ => panic!("the decode gave another message"),
        }
        let mut garbage = vec![0xff; 40];
        assert_eq!(
            decode_ulp(&mut garbage).err(),
            Some(UlpRefused::Malformed),
            "garbage decoded"
        );
        assert!(garbage.is_empty(), "a frame that did not decode was kept");
    }

    /// An `EdToNd { EssentialData }` frame (the real prost encoding) whose data `fill` sets up.
    fn essential_frame(fill: impl FnOnce(&mut d2d_join::EssentialData)) -> Vec<u8> {
        let mut data = d2d_join::EssentialData::default();
        fill(&mut data);
        d2d_join::EdToNd {
            content: Some(d2d_join::ed_to_nd::Content::EssentialData(data)),
        }
        .encode_to_vec()
    }

    /// S2 at the real wiring: `decode_ulp` counts the frame on the wire against C7's bounds BEFORE it is
    /// decoded. N contacts decode; N + 1 are refused with the rule and the frame is wiped all the same.
    /// (The count is the first thing `decode_ulp` does: a refusal here never allocated the structs.)
    #[test]
    fn a_frame_over_the_real_bounds_is_refused_before_it_is_decoded() {
        use d2d_join::essential_data::{
            AugmentedContact, AugmentedDistributionList, AugmentedGroup,
        };
        let n = essential::Bounds::REAL.contacts;
        let mut at = essential_frame(|d| d.contacts = vec![AugmentedContact::default(); n]);
        let decoded = decode_ulp(&mut at).expect("100,000 contacts decode");
        assert!(at.is_empty(), "the frame was not wiped");
        match decoded.content {
            Some(d2d_join::ed_to_nd::Content::EssentialData(data)) => {
                assert_eq!(data.contacts.len(), n);
            }
            _ => panic!("the decode gave another message"),
        }
        let mut over = essential_frame(|d| d.contacts = vec![AugmentedContact::default(); n + 1]);
        assert_eq!(
            decode_ulp(&mut over).err(),
            Some(UlpRefused::Over(essential::rule::TOO_MANY_CONTACTS)),
            "100,001 contacts were not refused on the wire"
        );
        assert!(over.is_empty(), "a refused frame was not wiped");
        // The members of one group (10,000).
        let members = essential::Bounds::REAL.members;
        let group = |count: usize| AugmentedGroup {
            group: Some(d2d_sync::Group {
                member_identities: Some(common::Identities {
                    identities: vec![String::new(); count],
                }),
                ..Default::default()
            }),
            last_update_at: 0,
        };
        let mut at = essential_frame(|d| d.groups = vec![group(members)]);
        assert!(decode_ulp(&mut at).is_ok(), "10,000 members decode");
        let mut over = essential_frame(|d| d.groups = vec![group(members + 1)]);
        assert_eq!(
            decode_ulp(&mut over).err(),
            Some(UlpRefused::Over(essential::rule::TOO_MANY_MEMBERS)),
            "10,001 members were not refused on the wire"
        );
        // C1: the member entries of ALL groups and lists together (1,000,000).
        let total = essential::Bounds::REAL.members_total;
        let groups = |extra_in_a_list: usize| {
            essential_frame(|d| {
                d.groups = vec![group(members); total / members];
                if extra_in_a_list > 0 {
                    d.distribution_lists = vec![AugmentedDistributionList {
                        distribution_list: Some(d2d_sync::DistributionList {
                            member_identities: Some(common::Identities {
                                identities: vec![String::new(); extra_in_a_list],
                            }),
                            ..Default::default()
                        }),
                        last_update_at: 0,
                    }];
                }
            })
        };
        assert!(
            decode_ulp(&mut groups(0)).is_ok(),
            "1,000,000 members in all decode"
        );
        let mut over = groups(1);
        assert_eq!(
            decode_ulp(&mut over).err(),
            Some(UlpRefused::Over(essential::rule::TOO_MANY_MEMBERS_IN_ALL)),
            "1,000,001 members in all were not refused on the wire"
        );
        assert!(over.is_empty(), "a refused frame was not wiped");
        // C2: the settings' two ID lists (100,000 each).
        let ids = essential::Bounds::REAL.identities;
        let settings = |blocked: usize, excluded: usize| {
            essential_frame(|d| {
                d.settings = Some(d2d_sync::Settings {
                    blocked_identities: Some(common::Identities {
                        identities: vec![String::new(); blocked],
                    }),
                    exclude_from_sync_identities: Some(common::Identities {
                        identities: vec![String::new(); excluded],
                    }),
                    ..Default::default()
                });
            })
        };
        assert!(
            decode_ulp(&mut settings(ids, ids)).is_ok(),
            "100,000 blocked and 100,000 excluded decode"
        );
        assert_eq!(
            decode_ulp(&mut settings(ids + 1, 0)).err(),
            Some(UlpRefused::Over(essential::rule::TOO_MANY_BLOCKED)),
            "100,001 blocked IDs were not refused on the wire"
        );
        assert_eq!(
            decode_ulp(&mut settings(0, ids + 1)).err(),
            Some(UlpRefused::Over(essential::rule::TOO_MANY_EXCLUDED)),
            "100,001 excluded IDs were not refused on the wire"
        );
    }

    /// S1: what is left of the keys in a message that is dropped (an out-of-order `EssentialData`, a
    /// `Received` nobody took) is wiped by the one function both use.
    #[test]
    fn wipe_keys_empties_ck_the_cookie_and_dgk() {
        let mut data = essential();
        wipe_keys(&mut data);
        let identity = data.identity_data.as_ref().expect("identity data stays");
        assert!(
            identity.ck.is_empty() && identity.csp_device_cookie.is_empty(),
            "CK or the cookie stayed"
        );
        assert!(
            data.device_group_data
                .as_ref()
                .is_some_and(|g| g.dgk.is_empty()),
            "DGK stayed"
        );
    }
}
