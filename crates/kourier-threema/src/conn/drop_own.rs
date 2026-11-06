//! RP-b: THE OWN DEVICE'S DROP at a logout (`Command::Logout`; the drop of OTHER devices, from the device list, stays
//! TH10's). Desktop's form (`TD/apps/desktop/src/common/network/protocol/task/d2m/drop-device.ts`, `selfKickFromMediator`
//! in `TD/apps/desktop/src/common/dom/network/protocol/connection.ts`): `DropDevice` with the own D2M id on the live
//! connection, waiting for `DropDeviceAck` of that id (`TP/md-d2m.proto`, 0x32 and 0x33: the ack says the slot "has been
//! free'd"); when no connection is up, a D2M-ONLY socket of its own that drops it.
//!
//! - THE LIVE SOCKET (`super::Task::drop_own`): a socket past `ServerInfo` writes the frame and its end tells: the ack of
//!   the own id, or the server's 4113 / 4114 after the ask, is [`Dropped::Removed`]. A socket before `ServerInfo` (or a
//!   connection between sockets) is closed with 1000 and the actor goes on with the D2M-only socket.
//! - THE D2M-ONLY SOCKET ([`alone`]): the stored identity's ClientHello with `expected_device_slot_state = EXISTING`, so a
//!   slot that is gone is NEVER made again: the mediator closes with 4115 "before being registered" (`TP/md-d2m.proto`,
//!   `expected_device_slot_state`), which is [`Dropped::Removed`] (the network no longer knew the device; but see THE FILE SAYS
//!   `new`). No CSP leg: no
//!   Proxy payload at all, so the chat server sees no login. After `ServerInfo` the frame, then the ack, or the server's
//!   4113 / 4114, is [`Dropped::Removed`]; a reflection, Dry or a promotion that comes meanwhile is left alone (nothing is
//!   acked: the slot is about to go with its queue).
//!   THE FILE SAYS `new` (P-M2): a join's NEW hello may still be in flight at the mediator when this socket's EXISTING
//!   hello is told 4115 (the spec orders frames only within ONE connection), and a late registration would leave an orphan
//!   slot. So while the file's slot state is `new`, a 4115 is final only after ONE more EXISTING socket about
//!   [`RETRY_AFTER`] (1 s) later, inside the bound: a second 4115, or the ack, is [`Dropped::Removed`] (the late slot, if
//!   there was one, is dropped by that socket); no time left for it is [`Dropped::NotReached`]. When the file says
//!   `existing` the first 4115 is final (the slot was there and is gone). EXISTING never registers, so the retry is safe.
//! - THE BOUND (P-L1, P-M1): the actor takes `by_ms` FIRST in `logout()` (before the join's stop, which can take up to two
//!   seconds on a stalled rendezvous): [`DROP_WITHIN`] of the actor's clock later, inside the core's 10 s
//!   `Timeouts::logout`. EVERY wait of the drop (the live socket's, the connect, a write, the silence, the pause before a
//!   retry) ends at [`wait_end`] = `by_ms` less the closing second ([`super::CLOSE_WITHIN`]), so the close after it ends
//!   by `by_ms`. Past it, a refused connect, another close or a broken socket: [`Dropped::NotReached`].
//! - Logged: kinds, codes and counts only; never an ID, a device id, a key or the server's close reason.
//!
//! [U: real server] whether the mediator closes the dropping connection with 4113 after the ack, whether a device may drop
//! itself (Desktop does), whether it takes a D2M connection that never proxies a CSP login (Desktop's D2M-only one does
//! not), how long after the first 4115 a NEW hello of an abandoned socket can still register (P-M2: the pause is Kourier's
//! 1 s), whether 4114 frees the slot (it is counted as `Removed`: the spec says only "dropped by server"), whether the
//! `DropDeviceAck` can queue behind a long reflection backlog on the D2M-only socket (a device offline for days may then
//! run into the bound), whether the mediator closes a live socket 4010 / 4012 instead of 4113 because reflection acks
//! and Proxy frames still follow `DropDevice` on it (harmless: the D2M-only socket then settles it), and whether an
//! EXISTING hello for a gone slot can be told 4111 before 4115 when the slots are full (`NotReached`: conservative).

use std::time::Duration;

use kourier_backend_api::Clock;
use libthreema::common::D2xDeviceId;
use libthreema::d2m::payload::{IncomingPayload as D2mIncoming, OutgoingPayload as D2mOutgoing};
use libthreema::d2m::{D2mProtocol, D2mProtocolError, D2mStateUpdate};
use tokio_util::sync::CancellationToken;

use crate::store::{Identity, SlotState};
use crate::{ServerParams, Transports, WsEvent, WsKind, WsRequest, WsSender};

/// How long the whole drop may take on the actor's clock, from the logout: the live socket and the D2M-only socket
/// together (Kourier's number, inside the core's 10 s for a logout).
pub(crate) const DROP_WITHIN: Duration = Duration::from_secs(8);

/// P-M2: how long after a first 4115 (the file says `new`) the D2M-only socket waits before it asks once more.
pub(crate) const RETRY_AFTER: Duration = Duration::from_secs(1);

/// When the drop is given up (ms of the actor's clock) for a logout that began at `now_ms`: [`DROP_WITHIN`] later.
pub(crate) fn deadline(now_ms: u64) -> u64 {
    now_ms.saturating_add(super::ms(DROP_WITHIN))
}

/// Where every wait of the drop ends: `by_ms` less [`super::CLOSE_WITHIN`], so that the close that follows a deadline
/// ends by `by_ms` itself (P-L1).
pub(crate) fn wait_end(by_ms: u64) -> u64 {
    by_ms.saturating_sub(super::ms(super::CLOSE_WITHIN))
}

/// What became of the own device's slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dropped {
    /// The mediator acked the drop, closed the device's socket with 4113 or 4114, or no longer knew the slot (4115).
    Removed,
    /// No answer within [`DROP_WITHIN`], no socket, or another end: the slot may still be there.
    NotReached,
}

/// The one `DropDevice` frame of the own device `own`: BOTH sockets write it through here. `None`: libthreema did not
/// encode it (one WARN; the caller ends its socket).
pub(crate) fn frame(d2m: &mut D2mProtocol, own: u64) -> Option<Vec<u8>> {
    match d2m.create_payload(D2mOutgoing::DropDevice(D2xDeviceId(own))) {
        Ok(instruction) => instruction.outgoing_datagram.map(|datagram| datagram.0),
        Err(_) => {
            tracing::warn!("the own device's drop could not be encoded");
            None
        }
    }
}

/// What the D2M-only socket is made with: the actor's.
pub(crate) struct Alone<'a> {
    /// The parts' transports (made for the logout when the account never connected).
    pub(crate) transports: &'a Transports,
    /// The servers.
    pub(crate) server: &'a ServerParams,
    /// The stored identity: zone 1's for a join that did not finish, else the linked one.
    pub(crate) identity: &'a Identity,
    /// The actor's clock.
    pub(crate) clock: &'a dyn Clock,
    /// When the drop is given up (ms of `clock`); the waits end at [`wait_end`] of it.
    pub(crate) by_ms: u64,
    /// Whether the file's slot state is still `new` (P-M2: a join's hello may be in flight; a first 4115 is not final).
    pub(crate) file_new: bool,
    /// The actor's cancel token (the core gave up on the actor).
    pub(crate) stop: &'a CancellationToken,
}

/// What one D2M-only socket settled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    /// The ack of the own id, or 4113 / 4114: the mediator dropped the slot.
    Removed,
    /// 4115 before any registration: the mediator held no slot for the device at that moment.
    Absent,
    /// The deadline, a refused connect, a protocol error, a broken socket or another close.
    NotReached,
}

/// The D2M-only socket of the drop (module doc): connect with EXISTING, `DropDevice` after `ServerInfo`, the answer; and
/// (P-M2) while the file says `new`, ONE more socket after a first 4115.
pub(crate) async fn alone(a: Alone<'_>) -> Dropped {
    let dropped = match socket(&a).await {
        Answer::Removed => Dropped::Removed,
        Answer::NotReached => Dropped::NotReached,
        // The slot was there once (the file says `existing`) and is gone: the network no longer knows the device.
        Answer::Absent if !a.file_new => Dropped::Removed,
        Answer::Absent => again(&a).await,
    };
    tracing::info!(outcome = ?dropped, "{}", super::lines::D4_DROP_ALONE);
    dropped
}

/// P-M2: the first EXISTING hello was told 4115 while the file still says `new` (a join's NEW hello may be in flight
/// at the mediator): ONE more EXISTING socket after [`RETRY_AFTER`], inside the bound. A second 4115 or the ack is
/// `Removed` (a late slot is dropped by this socket); no time left for it is `NotReached`.
async fn again(a: &Alone<'_>) -> Dropped {
    let end = wait_end(a.by_ms);
    let at = a.clock.now_ms().saturating_add(super::ms(RETRY_AFTER));
    if at >= end {
        tracing::info!("no time was left to ask the mediator once more for the drop");
        return Dropped::NotReached;
    }
    tracing::info!(
        "the mediator held no slot while the file says new: one more socket after a pause"
    );
    tokio::select! {
        biased;
        () = a.stop.cancelled() => return Dropped::NotReached,
        () = a.clock.sleep_until(at) => {}
    }
    match socket(a).await {
        Answer::Removed | Answer::Absent => Dropped::Removed,
        Answer::NotReached => Dropped::NotReached,
    }
}

/// One D2M-only socket, from its connect to its end.
async fn socket(a: &Alone<'_>) -> Answer {
    // EXISTING whatever the file says: a slot that is gone is not made again (4115).
    let context = match super::d2m::context(a.identity, a.server, SlotState::Existing) {
        Ok(context) => context,
        Err(field) => {
            tracing::error!(field, "the drop's socket could not be set up");
            return Answer::NotReached;
        }
    };
    let until = wait_end(a.by_ms);
    let (proto, url) = D2mProtocol::new(context);
    let connect = a.transports.ws.connect(WsRequest {
        url,
        kind: WsKind::Mediator,
    });
    let (tx, rx) = tokio::select! {
        biased;
        () = a.stop.cancelled() => return Answer::NotReached,
        () = a.clock.sleep_until(until) => {
            tracing::info!("the mediator was not reached in time for the drop");
            return Answer::NotReached;
        }
        opened = connect => match opened {
            Ok(halves) => halves,
            Err(error) => {
                tracing::info!(%error, "the mediator was not reached for the drop");
                return Answer::NotReached;
            }
        },
    };
    let mut socket = AloneSocket {
        proto,
        tx,
        own: a.identity.d2m_device_id,
        clock: a.clock,
        until,
        stop: a.stop,
        server_info: false,
        written: false,
    };
    socket.run(&rx).await
}

/// The D2M-only socket while it runs.
struct AloneSocket<'a> {
    proto: D2mProtocol,
    tx: Box<dyn WsSender>,
    own: u64,
    clock: &'a dyn Clock,
    /// Where its waits end ([`wait_end`] of the drop's deadline).
    until: u64,
    stop: &'a CancellationToken,
    /// `ServerInfo` came: the slot exists.
    server_info: bool,
    /// The `DropDevice` frame went.
    written: bool,
}

impl AloneSocket<'_> {
    async fn run(&mut self, rx: &async_channel::Receiver<WsEvent>) -> Answer {
        loop {
            match self.turn().await {
                Turn::Acked => {
                    self.close(1000).await;
                    return Answer::Removed;
                }
                Turn::Failed(code) => {
                    self.close(code).await;
                    return Answer::NotReached;
                }
                Turn::Broken => return Answer::NotReached,
                Turn::Waits => {}
            }
            if self.server_info && !self.written {
                self.written = true;
                let Some(datagram) = frame(&mut self.proto, self.own) else {
                    self.close(1000).await;
                    return Answer::NotReached;
                };
                if !self.write(datagram).await {
                    return Answer::NotReached;
                }
            }
            let event = tokio::select! {
                biased;
                () = self.stop.cancelled() => {
                    self.close(1000).await;
                    return Answer::NotReached;
                }
                () = self.clock.sleep_until(self.until) => {
                    tracing::info!(written = self.written, "{}", super::lines::D4_DROP_NO_ANSWER);
                    self.close(1000).await;
                    return Answer::NotReached;
                }
                event = rx.recv() => event,
            };
            match event {
                Ok(WsEvent::Binary(datagram)) => {
                    if let Err(error) = self.proto.add_datagrams(vec![datagram]) {
                        let code = refusal(&error);
                        refused(code, &error);
                        self.close(code).await;
                        return Answer::NotReached;
                    }
                }
                Ok(WsEvent::Closed { code, reason: _ }) => return closed(code),
                Err(_) => return closed(1006),
            }
        }
    }

    /// libthreema's turn: the handshake's frames go out, `ServerInfo` and the own ack are noted, the rest is left alone.
    async fn turn(&mut self) -> Turn {
        loop {
            match self.proto.poll() {
                Ok(Some(instruction)) => {
                    if let Some(D2mStateUpdate::PostHandshake(_)) = instruction.state_update {
                        self.server_info = true;
                    }
                    let acked = matches!(
                        instruction.incoming_payload,
                        Some(D2mIncoming::DropDeviceAck(D2xDeviceId(id))) if id == self.own && self.written
                    );
                    if let Some(datagram) = instruction.outgoing_datagram
                        && !self.write(datagram.0).await
                    {
                        return Turn::Broken;
                    }
                    if acked {
                        return Turn::Acked;
                    }
                }
                Ok(None) => return Turn::Waits,
                Err(error) => {
                    let code = refusal(&error);
                    refused(code, &error);
                    return Turn::Failed(code);
                }
            }
        }
    }

    /// One frame, bounded by the drop's wait end and the stop. Whether it went.
    async fn write(&self, datagram: Vec<u8>) -> bool {
        tokio::select! {
            biased;
            sent = self.tx.send(datagram) => sent.is_ok(),
            () = self.stop.cancelled() => false,
            () = self.clock.sleep_until(self.until) => {
                tracing::info!("a write of the drop's socket stalled");
                false
            }
        }
    }

    /// The client closes, bounded by one second of the actor's clock (as the connection's close).
    async fn close(&self, code: u16) {
        let close = self.tx.close(code, String::new());
        tokio::select! {
            _ = close => {}
            () = self.clock.sleep(super::CLOSE_WITHIN) => {}
        }
    }
}

/// What one turn of libthreema left.
enum Turn {
    /// The mediator acked the drop of the own id.
    Acked,
    /// libthreema refused what the server sent: the close code the client answers with ([`refusal`]).
    Failed(u16),
    /// A write failed: the socket is gone.
    Broken,
    /// Nothing more to do until the server says something.
    Waits,
}

/// E4 of the D2M-only socket: `what=d2m`, the close code, then the error's fixed words ([`super::lines::Refusal`], RT1
/// attempt 1), never its text.
fn refused(code: u16, error: &D2mProtocolError) {
    let why = super::lines::Refusal::of_d2m(error);
    tracing::warn!(
        what = "d2m",
        code,
        kind = %why.kind,
        name = why.name.map(tracing::field::display),
        payload_type = why.payload_type,
        "{}",
        super::lines::E4_REFUSED
    );
}

/// The close code of a refusal of what the server sent (`TP/md-d2m.struct.yml:71-73`: 4110 for a version Kourier does not
/// speak, else 4010), as the live socket's `Session::protocol_error`.
fn refusal(error: &D2mProtocolError) -> u16 {
    if matches!(error, D2mProtocolError::UnsupportedVersion(_)) {
        4110
    } else {
        4010
    }
}

/// The server closed the D2M-only socket with `code`: 4113 and 4114 (the device was dropped) say the slot is gone; 4115
/// (the slot was not there: never made again by an EXISTING hello) says it was not there at that moment; anything else
/// says nothing.
fn closed(code: u16) -> Answer {
    let meaning = crate::close::sentence(code).unwrap_or("a code the spec does not name");
    tracing::info!(code, meaning, "the mediator closed the drop's socket");
    match code {
        4113 | 4114 => Answer::Removed,
        4115 => Answer::Absent,
        _ => Answer::NotReached,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P-L3: a server version Kourier does not speak is closed 4110 (`TP/md-d2m.struct.yml:71-73`) on the D2M-only socket
    /// as on the live one; everything else libthreema refuses is 4010. (The fake cannot offer a version below the
    /// client's minimum, which is 0, so the mapping is tested where it is made.)
    #[test]
    fn the_d2m_only_socket_closes_4110_for_an_unsupported_version_and_4010_otherwise() {
        assert_eq!(refusal(&D2mProtocolError::UnsupportedVersion(0)), 4110);
        assert_eq!(refusal(&D2mProtocolError::UnexpectedPayloadType(1)), 4010);
        assert_eq!(refusal(&D2mProtocolError::ServerError("x")), 4010);
    }

    /// The close table of the D2M-only socket: 4113 / 4114 `Removed`, 4115 `Absent`, the rest `NotReached`.
    #[test]
    fn the_d2m_only_socket_reads_the_close_codes_as_the_table_says() {
        for (code, want) in [
            (4113, Answer::Removed),
            (4114, Answer::Removed),
            (4115, Answer::Absent),
            (4110, Answer::NotReached),
            (4111, Answer::NotReached),
            (4112, Answer::NotReached),
            (4010, Answer::NotReached),
            (1000, Answer::NotReached),
            (1006, Answer::NotReached),
            (4999, Answer::NotReached),
        ] {
            assert_eq!(closed(code), want, "{code}");
        }
    }

    /// The waits end one closing second before the deadline, and never before the start of time.
    #[test]
    fn the_waits_end_one_closing_second_before_the_deadline() {
        assert_eq!(deadline(5_000), 13_000);
        assert_eq!(wait_end(13_000), 12_000);
        assert_eq!(wait_end(300), 0);
    }
}
