//! THE LINE CONTRACT (TH6 chunk C1 = L4a): every fixed log text the run tool (`cargo xtask threema-run`, TR-lite) reads
//! from the app's stderr, one const each, and the per-socket frame counts its count line prints. TR keeps a twin of each
//! text and its test finds every one HERE, in these words: reword a line and the tool loses its eyes on the run (the
//! shapes, field by field, are written down for TR in `target/threema/th6-lines.md`). A line is the message followed by
//! its fields as `name=value` in the order the call names them (the app's log is tracing's plain format): a number bare,
//! a word bare (every word field is written with `%`), never a quoted string.
//!
//! Every line holds fixed words, kind names and numbers only: never an ID, a key, a nonce, a body, a nickname, the
//! alert's text, a cookie or the server's close reason.
//!
//! - THE COUNT LINE ([`L_COUNT`], [`Frames::log`]): the frames of ONE socket (the mediator's WebSocket, the chat
//!   server's leg inside it), counted from the socket's open: what the client READ (`in_*`, every payload after the
//!   handshakes, by kind) and what it WROTE (`out_*`: a reflect and a box by the ORIGIN of what wrote it, TR's D5). It is
//!   printed every [`COUNT_EVERY`] of the actor's clock while the socket stands (`when=running`: a partial read, never
//!   added to another line) and once at the socket's end (`when=end`), whatever ended it. A socket that never opened
//!   (a refused connect) has no line; a socket of a killed process has no end line. The D2M-only socket of a logout's
//!   drop has none either (its outcome line is [`D4_DROP_ALONE`]).
//! - THE KIND LINE ([`L_KIND`]): once per socket and kind, a payload of a kind Kourier does not handle; counted
//!   `in_other`.
//! - THE H1 GUARD's line ([`L_UNASKED`]): a chat server message on a socket that may not take one.
//! - THE STEP-ASIDE line ([`L_STEP_ASIDE`], T-H1): the leader closes once so a device online can lead the messages held
//!   for the phone.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use libthreema::csp::CspProtocolError;
use libthreema::csp::payload::IncomingPayload as CspIncoming;
use libthreema::d2m::D2mProtocolError;
use libthreema::d2m::payload::IncomingPayload as D2mIncoming;

use crate::send::queue::SendOrigin;

/// L-COUNT (INFO): the socket's frame counts (module doc); fields `when`, the twelve `in_*`, the fourteen `out_*`,
/// `held`, `held_by`, in [`Frames::log`]'s order.
pub(crate) const L_COUNT: &str = "socket frame counts";
/// L-KIND (INFO): a payload of a kind Kourier does not handle, once per kind and socket; fields `server=<chat|mediator>`
/// `kind=<libthreema's variant name>` (the chat server's `ServerAlert`, `DeviceCookieChangeIndication`, `UnknownPayload`;
/// the mediator's `DevicesInfo`, `UnknownPayload`).
pub(crate) const L_KIND: &str = "a server payload Kourier does not handle";
/// L-UNASKED (WARN): THE H1 GUARD (`TP/csp.struct.yml:688-689`): a chat server message on a socket that is not the leader
/// or whose unblock has not gone; the socket closes 1000 (`Outcome::Unasked`), nothing is acked. No fields.
pub(crate) const L_UNASKED: &str = "a chat server message came unasked; closing";
/// D1 (INFO): the leader socket wrote its one unblock (TH6 L4b). No fields.
pub(crate) const D1_UNBLOCKED: &str = "incoming messages were unblocked as the leader";
/// D2 (INFO): a chat server message the leader holds for the phone (no ack: the server redelivers it to whichever device
/// leads next); field `rule=<rule>`, the same word as the count line's `held_by` key (TH6 L4b and later).
pub(crate) const D2_HELD: &str = "a chat server message is held for the phone";
/// L_STEP_ASIDE (INFO, T-H1; TH6 FIX-B, TR's ask): the leader socket holds a message only the phone resolves and the
/// mediator showed another device connected: it closes 1000 ONCE so that device can lead (`Outcome::Yield`: a 2 min
/// pause, not counted, at most once every 15 min per actor). Field `held=<n>`: the messages held for the phone. The run
/// tool reads it before it judges `other` > 0 (the `GetDevicesInfo` ask counts `out_other`, its answer `in_other`).
pub(crate) const L_STEP_ASIDE: &str =
    "messages are held for the phone and another device is online; stepping aside so it can lead";
/// D4 (INFO, RP-b): the mediator acked the drop of the own device on the live socket. No fields.
pub(crate) const D4_DROP_ACKED: &str = "the mediator acked the own device's drop";
/// D4 (INFO, RP-b): the drop's wait ran out: on the live socket with no fields, on the D2M-only socket with
/// `written=<true|false>`.
pub(crate) const D4_DROP_NO_ANSWER: &str = "the mediator did not answer the drop in time";
/// D4 (INFO, RP-b): the live socket wrote `DropDevice`. No fields.
pub(crate) const D4_DROP_WENT: &str = "the own device's drop went on the live socket";
/// D4 (INFO, RP-b S-5): the live socket's write of `DropDevice` failed or stalled. No fields.
pub(crate) const D4_DROP_NOT_WRITTEN: &str =
    "the own device's drop could not be written to the live socket";
/// D4 (INFO, RP-b): the drop was asked of a socket before `ServerInfo` (or one that was going): closed 1000, the actor
/// drops the device on a D2M-only socket. No fields.
pub(crate) const D4_DROP_NO_SOCKET: &str = "no socket to drop the own device on: closed";
/// D4 (INFO, RP-b): the D2M-only socket's outcome, field `outcome=<Removed|NotReached>`.
pub(crate) const D4_DROP_ALONE: &str = "the own device's drop on a socket of its own";
/// E1 (INFO): the mediator closed the socket; fields `code=<n>` `meaning=<the spec's words>`.
pub(crate) const E1_CLOSED: &str = "the mediator closed the connection";
/// E2 (WARN): a server's clock is more than 20 minutes off; fields `server=<mediator|chat server>` `minutes=<n>`.
pub(crate) const E2_CLOCK: &str = "the clock is off by more than 20 minutes";
/// E3 (WARN): the TLS pins refused the server; field `kind=<the refusal's kind>`.
pub(crate) const E3_UNTRUSTED: &str = "the server's certificate is not one Kourier trusts";
/// E4 (WARN): libthreema refused what the server sent; the live socket's fields `what=<d2m|csp|csp chunks|csp encode>`
/// `unsupported=<bool>`, the D2M-only socket's `what=d2m` `code=<n>`; both then (RT1 attempt 1) the error's kind as
/// [`Refusal`] writes it: `kind=<word>` (the variant: `unexpected-payload-type`, `decoding-failed`, `decryption-failed`,
/// `invalid-message`, `server-error`, `contract-violation`, `conversion-error`, `internal-error`, `unsupported-version`),
/// `name=<libthreema's static message name>` only where the variant has one, `payload_type=<n>` only for
/// `unexpected-payload-type`. Never the error's text (`cause`) or anything else of it.
pub(crate) const E4_REFUSED: &str = "the server's message was refused";
/// E5 (ERROR): the connection task panicked. No fields.
pub(crate) const E5_PANICKED: &str = "the connection task panicked";
/// E7 (WARN): the join's first `ServerInfo` did not say NEW. No fields.
pub(crate) const E7_NOT_NEW: &str = "the mediator did not register the joined device as new";
/// E8 (WARN): the core refused the joined account's first row. No fields.
pub(crate) const E8_FIRST_ROW: &str =
    "the core refused the joined account's first row; the link ends";

/// D3: how often a standing socket prints its count line, on the actor's clock, from the socket's open.
pub(crate) const COUNT_EVERY: Duration = Duration::from_secs(60);

/// When the next running count line is due after the one due at `due_ms`, printed at `now_ms`: one period later, or one
/// period after now when the clock jumped past it (a suspend): never a burst of lines.
pub(crate) fn next_due(due_ms: u64, now_ms: u64) -> u64 {
    let every = u64::try_from(COUNT_EVERY.as_millis()).unwrap_or(u64::MAX);
    let next = due_ms.saturating_add(every);
    if next <= now_ms {
        now_ms.saturating_add(every)
    } else {
        next
    }
}

/// When a count line is printed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum When {
    /// Every [`COUNT_EVERY`] while the socket stands.
    Running,
    /// Once, at the socket's end.
    End,
}

impl When {
    fn word(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::End => "end",
        }
    }
}

/// What a frame the socket wrote was (the count line's `out_*`).
#[allow(dead_code, reason = "TH6 L5 writes the transactions (`Tx`)")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Out {
    /// A `reflected-ack` of a reflected envelope.
    ReflectedAck,
    /// A `Reflect` of a send row, by the row's origin.
    Reflect(SendOrigin),
    /// A `Reflect` of an incoming message the leader took from the chat server.
    InboundReflect,
    /// An ephemeral `Reflect` (an incoming typing indicator, as leader).
    Ephemeral,
    /// A `message-with-metadata-box` of a send row, by the row's origin.
    Box(SendOrigin),
    /// A CSP `message-ack` of an incoming message.
    MessageAck,
    /// The CSP `unblock-incoming-messages`.
    Unblock,
    /// `BeginTransaction` or `CommitTransaction`.
    Tx,
    /// A CSP echo request or response.
    Echo,
    /// The CSP `set-connection-idle-timeout`.
    Idle,
    /// `DropDevice` (RP-b).
    Drop,
    /// Anything else after the handshakes (none is written today: a belt).
    Other,
}

/// Which server a payload came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Server {
    Mediator,
    Chat,
}

impl Server {
    fn word(self) -> &'static str {
        match self {
            Self::Mediator => "mediator",
            Self::Chat => "chat",
        }
    }
}

/// The frames of one socket (module doc), and the kinds it told already.
#[derive(Debug, Default)]
pub(crate) struct Frames {
    in_reflected: u64,
    in_reflect_ack: u64,
    in_dry: u64,
    in_promoted: u64,
    in_drop_ack: u64,
    in_tx: u64,
    in_csp_message: u64,
    in_message_ack: u64,
    in_queue_complete: u64,
    in_echo: u64,
    in_close_error: u64,
    in_other: u64,
    out_reflected_ack: u64,
    out_reflect_job: u64,
    out_reflect_transmit: u64,
    out_reflect_inbound: u64,
    out_ephemeral: u64,
    out_box_job: u64,
    out_box_transmit: u64,
    out_message_ack: u64,
    out_unblock: u64,
    out_tx: u64,
    out_echo: u64,
    out_idle: u64,
    out_drop: u64,
    out_other: u64,
    /// The messages held for the phone, by rule (D2).
    held: BTreeMap<&'static str, u64>,
    /// The unhandled kinds this socket told ([`L_KIND`]).
    told: BTreeSet<(Server, &'static str)>,
}

impl Frames {
    /// One payload of the mediator, read after its handshake. A Proxy payload is the chat server's leg: what it carries
    /// is counted by [`Frames::heard_chat`].
    pub(crate) fn heard_mediator(&mut self, payload: &D2mIncoming) {
        match payload {
            D2mIncoming::Proxy(_) => {}
            D2mIncoming::Reflected(_) => self.in_reflected += 1,
            D2mIncoming::ReflectAck(_) => self.in_reflect_ack += 1,
            D2mIncoming::ReflectionQueueDry => self.in_dry += 1,
            D2mIncoming::RolePromotedToLeader => self.in_promoted += 1,
            D2mIncoming::DropDeviceAck(_) => self.in_drop_ack += 1,
            D2mIncoming::BeginTransactionAck
            | D2mIncoming::CommitTransactionAck
            | D2mIncoming::TransactionRejected(_)
            | D2mIncoming::TransactionEnded(_) => self.in_tx += 1,
            D2mIncoming::DevicesInfo(_) | D2mIncoming::UnknownPayload { .. } => {
                self.unhandled(Server::Mediator, payload.variant_name());
            }
        }
    }

    /// One payload of the chat server, read after its login.
    pub(crate) fn heard_chat(&mut self, payload: &CspIncoming) {
        match payload {
            CspIncoming::EchoRequest(_) | CspIncoming::EchoResponse(_) => self.in_echo += 1,
            CspIncoming::MessageWithMetadataBox(_) => self.in_csp_message += 1,
            CspIncoming::MessageAck(_) => self.in_message_ack += 1,
            CspIncoming::QueueSendComplete => self.in_queue_complete += 1,
            CspIncoming::CloseError(_) => self.in_close_error += 1,
            CspIncoming::DeviceCookieChangeIndication
            | CspIncoming::ServerAlert(_)
            | CspIncoming::UnknownPayload { .. } => {
                self.unhandled(Server::Chat, payload.variant_name());
            }
        }
    }

    /// A payload Kourier does not handle: counted `in_other`, told once per kind ([`L_KIND`]): its kind, never what it
    /// says.
    fn unhandled(&mut self, server: Server, kind: &'static str) {
        self.in_other += 1;
        if self.told.insert((server, kind)) {
            tracing::info!(server = %server.word(), kind = %kind, "{L_KIND}");
        }
    }

    /// A frame went on the socket.
    pub(crate) fn wrote(&mut self, out: Out) {
        let counter = match out {
            Out::ReflectedAck => &mut self.out_reflected_ack,
            Out::Reflect(SendOrigin::Job) => &mut self.out_reflect_job,
            Out::Reflect(SendOrigin::Transmit) => &mut self.out_reflect_transmit,
            Out::InboundReflect => &mut self.out_reflect_inbound,
            Out::Ephemeral => &mut self.out_ephemeral,
            Out::Box(SendOrigin::Job) => &mut self.out_box_job,
            Out::Box(SendOrigin::Transmit) => &mut self.out_box_transmit,
            Out::MessageAck => &mut self.out_message_ack,
            Out::Unblock => &mut self.out_unblock,
            Out::Tx => &mut self.out_tx,
            Out::Echo => &mut self.out_echo,
            Out::Idle => &mut self.out_idle,
            Out::Drop => &mut self.out_drop,
            Out::Other => &mut self.out_other,
        };
        *counter += 1;
    }

    /// How many CSP `message-ack`s of incoming messages this socket wrote (P-L6: whether it made progress).
    pub(crate) fn message_acks(&self) -> u64 {
        self.out_message_ack
    }

    /// D2: a chat server message is held for the phone by `rule` (a fixed word): its line, and its count.
    pub(crate) fn held(&mut self, rule: &'static str) {
        tracing::info!(rule = %rule, "{D2_HELD}");
        *self.held.entry(rule).or_default() += 1;
    }

    /// The count line ([`L_COUNT`]). The order of the fields is the contract's.
    pub(crate) fn log(&self, when: When) {
        let held: u64 = self.held.values().sum();
        tracing::info!(
            when = %when.word(),
            in_reflected = self.in_reflected,
            in_reflect_ack = self.in_reflect_ack,
            in_dry = self.in_dry,
            in_promoted = self.in_promoted,
            in_drop_ack = self.in_drop_ack,
            in_tx = self.in_tx,
            in_csp_message = self.in_csp_message,
            in_message_ack = self.in_message_ack,
            in_queue_complete = self.in_queue_complete,
            in_echo = self.in_echo,
            in_close_error = self.in_close_error,
            in_other = self.in_other,
            out_reflected_ack = self.out_reflected_ack,
            out_reflect_job = self.out_reflect_job,
            out_reflect_transmit = self.out_reflect_transmit,
            out_reflect_inbound = self.out_reflect_inbound,
            out_ephemeral = self.out_ephemeral,
            out_box_job = self.out_box_job,
            out_box_transmit = self.out_box_transmit,
            out_message_ack = self.out_message_ack,
            out_unblock = self.out_unblock,
            out_tx = self.out_tx,
            out_echo = self.out_echo,
            out_idle = self.out_idle,
            out_drop = self.out_drop,
            out_other = self.out_other,
            held,
            held_by = %HeldBy(&self.held),
            "{L_COUNT}"
        );
    }
}

/// E4's fixed words of a refusal (RT1 attempt 1: the WARN said only `what="d2m"`, so the one fact that proves the
/// diagnosis, WHICH error, was not in the log): the variant of libthreema's error as a word, the static message name where
/// the variant has one, and the payload type of an unexpected payload. Nothing else of the error: its `cause` is a
/// `String` that may hold data, its other `&'static str`s are prose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Refusal {
    /// The variant as a word (the E4 doc's list).
    pub(crate) kind: &'static str,
    /// libthreema's static name of the message, where the variant carries one.
    pub(crate) name: Option<&'static str>,
    /// The payload type byte, for `unexpected-payload-type` only.
    pub(crate) payload_type: Option<u8>,
}

impl Refusal {
    const fn word(kind: &'static str) -> Self {
        Self {
            kind,
            name: None,
            payload_type: None,
        }
    }

    const fn named(kind: &'static str, name: &'static str) -> Self {
        Self {
            kind,
            name: Some(name),
            payload_type: None,
        }
    }

    /// The D2M error's words.
    pub(crate) fn of_d2m(error: &D2mProtocolError) -> Self {
        match error {
            D2mProtocolError::ConversionError(_) => Self::word("conversion-error"),
            D2mProtocolError::ContractViolation(_) => Self::word("contract-violation"),
            D2mProtocolError::InternalError(_) => Self::word("internal-error"),
            D2mProtocolError::DecryptionFailed { name } => Self::named("decryption-failed", name),
            D2mProtocolError::DecodingFailed { name, source: _ } => {
                Self::named("decoding-failed", name)
            }
            D2mProtocolError::UnexpectedPayloadType(t) => Self {
                kind: "unexpected-payload-type",
                name: None,
                payload_type: Some(*t),
            },
            D2mProtocolError::InvalidMessage { name, cause: _ } => {
                Self::named("invalid-message", name)
            }
            D2mProtocolError::ServerError(_) => Self::word("server-error"),
            D2mProtocolError::UnsupportedVersion(_) => Self::word("unsupported-version"),
        }
    }

    /// The CSP error's words (the same words; CSP has no unexpected payload type or version of its own, and its
    /// `ConversionError` exists only with libthreema's `uniffi` feature, which Kourier never turns on: rule 8).
    pub(crate) fn of_csp(error: &CspProtocolError) -> Self {
        match error {
            CspProtocolError::ContractViolation(_) => Self::word("contract-violation"),
            CspProtocolError::InternalError(_) => Self::word("internal-error"),
            CspProtocolError::DecryptionFailed { name } => Self::named("decryption-failed", name),
            CspProtocolError::DecodingFailed { name, source: _ } => {
                Self::named("decoding-failed", name)
            }
            CspProtocolError::InvalidMessage { name, cause: _ } => {
                Self::named("invalid-message", name)
            }
            CspProtocolError::ServerError(_) => Self::word("server-error"),
        }
    }
}

/// The count line's `held_by`: `none`, or `<rule>:<n>` joined by `,` in the rules' order (no space: one field).
struct HeldBy<'a>(&'a BTreeMap<&'static str, u64>);

impl fmt::Display for HeldBy<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str("none");
        }
        for (i, (rule, n)) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(",")?;
            }
            write!(f, "{rule}:{n}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The run tool's twin test compares its constants with these words: every line, pinned whole.
    #[test]
    fn every_line_the_run_tool_reads_is_pinned_in_its_exact_words() {
        let pinned: [(&str, &str); 19] = [
            (L_COUNT, "socket frame counts"),
            (L_KIND, "a server payload Kourier does not handle"),
            (L_UNASKED, "a chat server message came unasked; closing"),
            (
                D1_UNBLOCKED,
                "incoming messages were unblocked as the leader",
            ),
            (D2_HELD, "a chat server message is held for the phone"),
            (
                L_STEP_ASIDE,
                "messages are held for the phone and another device is online; stepping aside so it can lead",
            ),
            (D4_DROP_ACKED, "the mediator acked the own device's drop"),
            (
                D4_DROP_NO_ANSWER,
                "the mediator did not answer the drop in time",
            ),
            (
                D4_DROP_WENT,
                "the own device's drop went on the live socket",
            ),
            (
                D4_DROP_NOT_WRITTEN,
                "the own device's drop could not be written to the live socket",
            ),
            (
                D4_DROP_NO_SOCKET,
                "no socket to drop the own device on: closed",
            ),
            (
                D4_DROP_ALONE,
                "the own device's drop on a socket of its own",
            ),
            (E1_CLOSED, "the mediator closed the connection"),
            (E2_CLOCK, "the clock is off by more than 20 minutes"),
            (
                E3_UNTRUSTED,
                "the server's certificate is not one Kourier trusts",
            ),
            (E4_REFUSED, "the server's message was refused"),
            (E5_PANICKED, "the connection task panicked"),
            (
                E7_NOT_NEW,
                "the mediator did not register the joined device as new",
            ),
            (
                E8_FIRST_ROW,
                "the core refused the joined account's first row; the link ends",
            ),
        ];
        for (line, words) in pinned {
            assert_eq!(line, words);
        }
        assert_eq!(COUNT_EVERY, Duration::from_secs(60), "D3's period");
        assert_eq!(When::Running.word(), "running");
        assert_eq!(When::End.word(), "end");
        assert_eq!(Server::Chat.word(), "chat");
        assert_eq!(Server::Mediator.word(), "mediator");
    }

    /// `held_by`: `none`, or the rules in order with their counts, one field with no space.
    #[test]
    fn held_by_is_none_or_the_rules_in_order() {
        let mut held = BTreeMap::new();
        assert_eq!(HeldBy(&held).to_string(), "none");
        held.insert("group-sync-needed", 2);
        held.insert("csp-unknown-type", 1);
        assert_eq!(
            HeldBy(&held).to_string(),
            "csp-unknown-type:1,group-sync-needed:2"
        );
    }

    /// D3: a line every 60 s from the open; after a jump of the clock the next one is a period after now, never a burst.
    #[test]
    fn the_running_line_comes_every_sixty_seconds_and_once_after_a_jump() {
        assert_eq!(next_due(60_000, 60_000), 120_000);
        assert_eq!(next_due(60_000, 61_000), 120_000, "a late wake");
        assert_eq!(next_due(60_000, 400_000), 460_000, "a suspend");
    }

    /// Every write is counted in its own field, a reflect and a box by the origin of the row (D5).
    #[test]
    fn a_frame_is_counted_in_its_field() {
        let mut f = Frames::default();
        for out in [
            Out::ReflectedAck,
            Out::Reflect(SendOrigin::Job),
            Out::Reflect(SendOrigin::Job),
            Out::Reflect(SendOrigin::Transmit),
            Out::InboundReflect,
            Out::Ephemeral,
            Out::Box(SendOrigin::Job),
            Out::Box(SendOrigin::Transmit),
            Out::MessageAck,
            Out::Unblock,
            Out::Tx,
            Out::Echo,
            Out::Idle,
            Out::Drop,
            Out::Other,
        ] {
            f.wrote(out);
        }
        assert_eq!(
            [
                f.out_reflected_ack,
                f.out_reflect_job,
                f.out_reflect_transmit,
                f.out_reflect_inbound,
                f.out_ephemeral,
                f.out_box_job,
                f.out_box_transmit,
                f.out_message_ack,
                f.out_unblock,
                f.out_tx,
                f.out_echo,
                f.out_idle,
                f.out_drop,
                f.out_other,
            ],
            [1, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1]
        );
        f.held("csp-unknown-type");
        f.held("csp-unknown-type");
        assert_eq!(f.held.get("csp-unknown-type"), Some(&2));
    }

    // ---- E4's kind words (RT1 attempt 1): one test per variant word, and never the cause ------------------------

    use libthreema::csp::CspProtocolInternalErrorCause;
    use libthreema::d2m::{D2mProtocolDecodingErrorSource, D2mProtocolInternalErrorCause};
    use libthreema::utils::bytes::ByteReaderError;

    /// A cause text the words must never carry.
    const SECRET: &str = "ECHOECHO-data-of-the-server";

    fn d2m_decoding() -> D2mProtocolDecodingErrorSource {
        D2mProtocolDecodingErrorSource::ByteReaderError(ByteReaderError::UnexpectedRemaining {
            remaining: 1,
            length: 2,
        })
    }

    fn csp_decoding() -> ByteReaderError {
        ByteReaderError::UnexpectedRemaining {
            remaining: 1,
            length: 2,
        }
    }

    /// The words as the line prints them, for the "never the cause" checks.
    fn printed(r: Refusal) -> String {
        format!("{r:?}")
    }

    #[test]
    fn e4_unexpected_payload_type_says_the_byte_and_no_name() {
        let r = Refusal::of_d2m(&D2mProtocolError::UnexpectedPayloadType(0x00));
        assert_eq!(
            r,
            Refusal {
                kind: "unexpected-payload-type",
                name: None,
                payload_type: Some(0),
            }
        );
        assert_eq!(
            Refusal::of_d2m(&D2mProtocolError::UnexpectedPayloadType(0x31)).payload_type,
            Some(0x31)
        );
    }

    #[test]
    fn e4_decoding_failed_says_the_name() {
        let r = Refusal::of_d2m(&D2mProtocolError::DecodingFailed {
            name: "ServerInfo",
            source: d2m_decoding(),
        });
        assert_eq!(r, Refusal::named("decoding-failed", "ServerInfo"));
        let c = Refusal::of_csp(&CspProtocolError::DecodingFailed {
            name: "ServerHello",
            source: csp_decoding(),
        });
        assert_eq!(c, Refusal::named("decoding-failed", "ServerHello"));
    }

    #[test]
    fn e4_decryption_failed_says_the_name() {
        let r = Refusal::of_d2m(&D2mProtocolError::DecryptionFailed { name: "Reflected" });
        assert_eq!(r, Refusal::named("decryption-failed", "Reflected"));
        let c = Refusal::of_csp(&CspProtocolError::DecryptionFailed { name: "LoginAck" });
        assert_eq!(c, Refusal::named("decryption-failed", "LoginAck"));
    }

    #[test]
    fn e4_invalid_message_says_the_name_never_the_cause() {
        let r = Refusal::of_d2m(&D2mProtocolError::InvalidMessage {
            name: "ServerHello",
            cause: SECRET.to_owned(),
        });
        assert_eq!(r, Refusal::named("invalid-message", "ServerHello"));
        assert!(!printed(r).contains(SECRET));
        let c = Refusal::of_csp(&CspProtocolError::InvalidMessage {
            name: "ServerHello",
            cause: SECRET.to_owned(),
        });
        assert_eq!(c, Refusal::named("invalid-message", "ServerHello"));
        assert!(!printed(c).contains(SECRET));
    }

    #[test]
    fn e4_server_error_is_the_word_only() {
        let r = Refusal::of_d2m(&D2mProtocolError::ServerError(SECRET));
        assert_eq!(r, Refusal::word("server-error"));
        assert!(!printed(r).contains(SECRET));
        let c = Refusal::of_csp(&CspProtocolError::ServerError(SECRET));
        assert_eq!(c, Refusal::word("server-error"));
    }

    #[test]
    fn e4_contract_violation_is_the_word_only() {
        let r = Refusal::of_d2m(&D2mProtocolError::ContractViolation(SECRET));
        assert_eq!(r, Refusal::word("contract-violation"));
        assert!(!printed(r).contains(SECRET));
        let c = Refusal::of_csp(&CspProtocolError::ContractViolation(SECRET));
        assert_eq!(c, Refusal::word("contract-violation"));
    }

    #[test]
    fn e4_conversion_error_is_the_word_only() {
        let r = Refusal::of_d2m(&D2mProtocolError::ConversionError(SECRET));
        assert_eq!(r, Refusal::word("conversion-error"));
        assert!(!printed(r).contains(SECRET));
    }

    #[test]
    fn e4_internal_error_is_the_word_only() {
        let r = Refusal::of_d2m(&D2mProtocolError::InternalError(
            D2mProtocolInternalErrorCause::EncryptionFailed { name: "Reflect" },
        ));
        assert_eq!(r, Refusal::word("internal-error"));
        let c = Refusal::of_csp(&CspProtocolError::InternalError(
            CspProtocolInternalErrorCause::EncryptionFailed { name: "Login" },
        ));
        assert_eq!(c, Refusal::word("internal-error"));
    }

    #[test]
    fn e4_unsupported_version_is_the_word_only() {
        let r = Refusal::of_d2m(&D2mProtocolError::UnsupportedVersion(7));
        assert_eq!(r, Refusal::word("unsupported-version"));
    }

    /// Every word is a protocol word the run tool's parser keeps bare (letters, digits, `-`), and all differ.
    #[test]
    fn e4_every_kind_is_a_bare_word_and_all_differ() {
        let words = [
            Refusal::of_d2m(&D2mProtocolError::UnexpectedPayloadType(0)).kind,
            Refusal::of_d2m(&D2mProtocolError::DecodingFailed {
                name: "X",
                source: d2m_decoding(),
            })
            .kind,
            Refusal::of_d2m(&D2mProtocolError::DecryptionFailed { name: "X" }).kind,
            Refusal::of_d2m(&D2mProtocolError::InvalidMessage {
                name: "X",
                cause: String::new(),
            })
            .kind,
            Refusal::of_d2m(&D2mProtocolError::ServerError("x")).kind,
            Refusal::of_d2m(&D2mProtocolError::ContractViolation("x")).kind,
            Refusal::of_d2m(&D2mProtocolError::ConversionError("x")).kind,
            Refusal::of_d2m(&D2mProtocolError::InternalError(
                D2mProtocolInternalErrorCause::EncryptionFailed { name: "X" },
            ))
            .kind,
            Refusal::of_d2m(&D2mProtocolError::UnsupportedVersion(0)).kind,
        ];
        let set: BTreeSet<&str> = words.iter().copied().collect();
        assert_eq!(set.len(), words.len(), "{words:?}");
        for w in words {
            assert!(
                w.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'),
                "{w}"
            );
        }
    }
}
