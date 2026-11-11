//! W5a chunk 14 (brief §5, decisions D2, D4, D6, D10): Kourier's SEND path end to end over the scripted
//! server. The core's `Transmit` reaches the actor, the attempt (ledger, D7, the hold, the registry) gives
//! the message to upstream's ONE `Client::send`, the Wire answers the frame with its ack policy, and the
//! answer comes back to the core; the peer (and our own phone) read what the client wrote.
//!
//! What this shows that the attempt's unit tests (over `FakeSession`) cannot: that the frame upstream
//! writes carries the core's `local` as its stanza id for every kind (the identity of brief §2.2: a
//! resend can never make a second message), that the attributes of an edit, a reaction and a revoke are
//! what the receivers expect, that the server's time of the ack is the core's `server_at`, that a 463 is
//! met ONCE (a resend, also after a restart, writes no frame), and that a read goes out as `read-self`
//! in a 1:1 chat when the phone's setting says so and as `read` in a group. Chunk 15a adds: a frame in
//! the air is written ONCE on its connection however often the core sends its local (critic H1), a lost
//! ack is transient and the resend goes out on the next connection with the same id (D5), and a group
//! text is one sender-key stanza answered `Sent`, never `Partial` (D13).
//!
//! Every body runs in ONE offline child under `unshare -rn` (KOURIER_REQUIRE_UNSHARE=1), the parent
//! checks that nothing the child printed holds a secret of the fixture, and the child that nothing it
//! logged does (D10). Every value is SYNTHETIC (the fixture's numbers, keys made on the spot, texts that
//! start with "synthetic ").

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kourier_backend_api::{
    AbortAnswer, BackendEvent, Command, Exit, OutgoingKind, OutgoingRequest, StartMode,
    TransmitOutcome,
};
use kourier_model::{
    Body, CmdId, ConnState, ConvKey, ErrorKind, MessageId, MsgTarget, ReceiptKind, TargetAuthor,
    Text, UserId,
};
use wacore_binary::builder::NodeBuilder;
use wacore_binary::{Jid, Node};
use waproto::whatsapp as wa;

use crate::actor_real::{Running, record, wait_for_states};
use crate::receive::{account_dir, count, raw, secrets};
use crate::support::capture::Capture;
use crate::support::peer::{self, Peer};
use crate::support::wire::{self, Ack, Wire};
use crate::support::{self};
use crate::wire::start_actor;

/// The ids the core gives its messages here (the `local` of each `Transmit`; no digit run of the
/// fixture's numbers, none a substring of another).
const TEXT_ID: &str = "3EB0C0DE5A1F40A1";
const EDIT_ID: &str = "3EB0C0DE5A2E51B2";
const REACT_ID: &str = "3EB0C0DE5A3D62C3";
const REVOKE_ID: &str = "3EB0C0DE5A4C73D4";
const COPY_ID: &str = "3EB0C0DE5A5B84E5";
const ACKED_ID: &str = "3EB0C0DE5A6A95F6";
const REFUSED_ID: &str = "3EB0C0DE5A79A607";
/// The id of the 475 case (fix F8).
const REFUSED_475_ID: &str = "3EB0C0DE5B2251B2";
/// The peer's first message of every case (it starts the chat, D7) and the group message read in the
/// reads case.
const FIRST_ID: &str = "3EB0C0DE5A88B718";
const GROUP_MSG_ID: &str = "3EB0C0DE5A97C829";
/// A second message of the group, read after the group's info could be asked again (fix F21).
const GROUP_READ_ID: &str = "3EB0C0DE5B4473D4";
/// The texts (D10: "synthetic " is one of the secrets).
const TEXT: &str = "synthetic send text 49158236";
const EDITED: &str = "synthetic edited text 83027415";
const EMOJI: &str = "\u{1f99c}";
/// A synthetic group (the reads case).
const GROUP: &str = "120363000049271836";
/// The peer's synthetic phone number (the migrated case: the store maps it to the peer's LID).
const PEER_PN: &str = "15550296417305";
/// The ids of the migrated case.
const NUMBER_TEXT_ID: &str = "3EB0C0DE5AA6D93A";
const NUMBER_EDIT_ID: &str = "3EB0C0DE5AB5EA4B";
/// The ids of the chunk-15a cases: a frame in the air (H1), a lost ack, a group text and a group text the
/// core asks for some members only.
const IN_AIR_ID: &str = "3EB0C0DE5AC4FB5C";
const LOST_ID: &str = "3EB0C0DE5AD30C6D";
const GROUP_TEXT_ID: &str = "3EB0C0DE5AE21D7E";
const GROUP_ONLY_ID: &str = "3EB0C0DE5AF12E8F";
/// The id of the slow case: a frame whose ack never comes (the real 75 s wait).
const SILENT_ID: &str = "3EB0C0DE5B003F90";
/// The id of the rate-limit case (fix F1): a text the server nacks with a 429.
const RATE_ID: &str = "3EB0C0DE5B1140A1";
/// A synthetic LID nobody started a chat with (the typing case, fix F17).
const STRANGER_LID: &str = "77700193507264";
/// The id of the token case (fix F17): a 1:1 text sent with a stored trusted-contact token.
const TOKEN_TEXT_ID: &str = "3EB0C0DE5B3362C3";
/// The stored token's bytes (synthetic; never printed).
const STORED_TOKEN: [u8; 32] = [0x6B; 32];
/// The own phone's synthetic key ids (its responder bundle in the `encrypt` rule).
const PHONE_SIGNED_ID: u32 = 13;
const PHONE_ONE_TIME_ID: u32 = 52_719;

/// The secrets of this module: the fixture's, the group's and the CDN fake's.
pub(crate) fn send_secrets() -> Vec<&'static str> {
    let mut all = secrets();
    all.push(GROUP);
    all.push(PEER_PN);
    // W5b (G20): the CDN fake's auth, url, direct_path and failure-body markers.
    all.extend(crate::support::cdn::secrets());
    all
}

/// The server's time now, in seconds (the slack rule of D9 judges an edit by its original's ack time, so
/// the acks carry the real time).
pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after 1970")
        .as_secs()
}

fn peer_user() -> UserId {
    UserId::from_parts("lid", peer::PEER_LID).expect("the peer's id")
}

/// The 1:1 chat with the peer, as the core keys it.
fn chat() -> ConvKey {
    ConvKey::Direct(peer_user())
}

/// The 1:1 chat with the peer under its phone number (the migrated case).
fn number_chat() -> ConvKey {
    ConvKey::Direct(UserId::from_parts("pn", PEER_PN).expect("the peer's number"))
}

/// The target of an own message of the chat.
fn own_target(local: &str) -> MsgTarget {
    target_in(chat(), local)
}

fn target_in(conv: ConvKey, local: &str) -> MsgTarget {
    MsgTarget {
        conv: Some(conv),
        author: TargetAuthor::Own,
        local: local.into(),
        alternates: Vec::new(),
    }
}

fn request(local: &str, kind: OutgoingKind) -> OutgoingRequest {
    OutgoingRequest {
        conv: chat(),
        local: local.into(),
        sent_at: i64::try_from(now_secs() * 1000).expect("time"),
        kind,
        only: None,
        is_recipient_update: false,
    }
}

pub(crate) fn text_request(local: &str) -> OutgoingRequest {
    request(
        local,
        OutgoingKind::Message {
            body: Body::Text(Text::plain(TEXT)),
            reply_to: None,
            expiry: None,
        },
    )
}

/// What the rig's server answers: our own devices, the phone's bundle (the copy case), the phone's
/// read-receipt setting, and every `<message>` frame's ack.
pub(crate) struct Script {
    pub(crate) own_devices: Vec<u16>,
    pub(crate) phone: Option<wire::Bundle>,
    pub(crate) read_receipts: &'static str,
    pub(crate) ack: Ack,
}

impl Script {
    pub(crate) fn acked() -> Self {
        Self {
            own_devices: vec![1],
            phone: None,
            read_receipts: "all",
            ack: Ack::Ok { t: now_secs() },
        }
    }
}

/// A scripted server with the rules of `script`: the login's own IQs (usync, privacy, the reach-out lock
/// with no lock, so the hold of D8 opens) and the ack policy.
pub(crate) fn server_of(script: &Script) -> Wire {
    let server = wire::server();
    server
        .on_iq(
            "usync",
            wire::usync(
                vec![
                    (peer::ACCOUNT_PN, script.own_devices.clone()),
                    (peer::ACCOUNT_LID, script.own_devices.clone()),
                    (peer::PEER_LID, vec![0]),
                ],
                Vec::new(),
            ),
        )
        .on_iq("privacy", wire::privacy(script.read_receipts))
        .on_iq("w:mex", wire::reach_out(None))
        .ack_messages(script.ack.clone());
    if let Some(bundle) = &script.phone {
        server.on_iq(
            "encrypt",
            wire::prekeys(vec![(
                format!("{}:0@lid", peer::ACCOUNT_LID),
                bundle.clone(),
            )]),
        );
    }
    server
}

/// A running account over the scripted server, the core's side of it (every envelope acknowledged, its
/// events kept) and the peer.
pub(crate) struct Rig {
    pub(crate) server: Wire,
    actor: Running,
    events: Arc<Mutex<Vec<BackendEvent>>>,
    states: Arc<Mutex<Vec<ConnState>>>,
    drain: tokio::task::JoinHandle<()>,
}

impl Rig {
    /// The account resumed over `server`, online (the handshake, `<success>` as the server writes it,
    /// an empty drain).
    pub(crate) async fn start(dir: &Path, server: Wire) -> Self {
        let actor = start_actor(dir, StartMode::Resume, &server);
        Self::online(server, actor).await
    }

    /// As [`Rig::start`], with the CDN fake as the clients' HTTP client (W5b: `tests/media_wire.rs`).
    pub(crate) async fn start_with_cdn(
        dir: &Path,
        server: Wire,
        cdn: &crate::support::cdn::Cdn,
    ) -> Self {
        let slot = wire::ClientSlot::default();
        let for_maker = cdn.clone();
        let actor =
            crate::wire::start_actor_with_http(dir, StartMode::Resume, &server, &slot, move || {
                for_maker.clone()
            });
        Self::online(server, actor).await
    }

    async fn online(server: Wire, mut actor: Running) -> Self {
        let states = record(actor.conn());
        let (_, empty) = tokio::sync::mpsc::channel(1);
        let mut envelopes = std::mem::replace(&mut actor.ends.events, empty);
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        // W6a2 B11 (the kill sample, `tests/groups_flow.rs`): a path in this variable gets one line per `Deleted` event the core
        // is given, fsynced BEFORE the envelope is acknowledged (a killed process leaves the evidence of what it delivered).
        let delivered_log = std::env::var_os("WA_RIG_DELIVERED_LOG").map(std::path::PathBuf::from);
        let drain = tokio::spawn(async move {
            while let Some(envelope) = envelopes.recv().await {
                if let Some(path) = &delivered_log {
                    let deleted = envelope
                        .events
                        .iter()
                        .filter(|e| matches!(e, BackendEvent::Deleted { .. }))
                        .count();
                    if deleted > 0 {
                        use std::io::Write;
                        let mut file = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(path)
                            .expect("the delivery log opens");
                        file.write_all("deleted\n".repeat(deleted).as_bytes())
                            .expect("the delivery log writes");
                        file.sync_all().expect("the delivery log syncs");
                    }
                }
                sink.lock().expect("lock").extend(envelope.events);
                if let Some(ack) = envelope.ack {
                    ack.complete(Ok(()));
                }
            }
        });
        actor.send(Command::Connect).await;
        assert!(
            server.wait_opened(1, Duration::from_secs(15)).await,
            "the handshake never completed"
        );
        server.send(&wire::success_as_the_server_writes_it(peer::ACCOUNT_LID));
        wait_for_states(&states, 20, &[ConnState::Online { detail: None }]).await;
        // The login's own IQs answered (our devices, the phone's settings, the reach-out lock).
        let deadline = Instant::now() + Duration::from_secs(20);
        while server.answered("usync") == 0
            || server.answered("privacy") == 0
            || server.answered("w:mex") == 0
        {
            assert!(
                Instant::now() < deadline,
                "the login's own IQs were never asked"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        Self {
            server,
            actor,
            events,
            states,
            drain,
        }
    }

    /// Everything the core has heard so far, in order (W6a2 B11: `tests/groups_flow.rs`).
    pub(crate) fn events(&self) -> Vec<BackendEvent> {
        self.events.lock().expect("lock").clone()
    }

    /// A command to the actor, as the core sends it (W6a2 B11: the 1:1 timer set).
    pub(crate) async fn command(&self, command: Command) {
        self.actor.send(command).await;
    }

    /// Waits (at most 20 s) until the core has heard `n` events that `wanted` picks.
    pub(crate) async fn heard(&self, n: usize, wanted: impl Fn(&BackendEvent) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while self
            .events
            .lock()
            .expect("lock")
            .iter()
            .filter(|e| wanted(e))
            .count()
            < n
        {
            assert!(
                Instant::now() < deadline,
                "the core never heard the events it waits for"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The peer writes first (its `pkmsg` begins the session, so the client answers with a `msg` the
    /// peer reads), and the chat counts as started on the phone (D7): the message reaches the core.
    pub(crate) async fn peer_writes_first(&self, peer: &mut Peer) {
        self.server
            .send(&peer.text_with_id("synthetic first word", FIRST_ID).await);
        self.heard(1, |e| matches!(e, BackendEvent::Message(_)))
            .await;
        self.frame(|f| is_tag_id(f, "receipt", FIRST_ID)).await;
    }

    /// The core's `Transmit` of `req`, answered. An answer that holds the send back for a while (the hold
    /// before the first pull, D8) is asked again, as the core would; any other answer is returned (a failed upload
    /// carries a floor too, W5b fix F2, and is NOT a hold: the test judges that attempt's own answer).
    pub(crate) async fn transmit(&self, req: &OutgoingRequest) -> TransmitOutcome {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let (reply, answer) = tokio::sync::oneshot::channel();
            self.actor
                .send(Command::Transmit {
                    local: MessageId(1),
                    req: Box::new(req.clone()),
                    reply,
                })
                .await;
            let outcome = tokio::time::timeout(Duration::from_secs(100), answer)
                .await
                .expect("the actor answers the Transmit")
                .expect("an answer");
            let held = matches!(
                &outcome,
                TransmitOutcome::Failed(e)
                    if matches!(e.kind, ErrorKind::Transient { retry_after: Some(_) })
                        && e.detail == "the account may not send yet"
            );
            if !held || Instant::now() >= deadline {
                return outcome;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    /// The core's `AbortTransmit` of the attempt [`Rig::transmit`] runs (its row is `MessageId(1)`), answered
    /// (W5b: `tests/media_wire.rs`, the stop pressed mid-POST).
    pub(crate) async fn abort(&self) -> AbortAnswer {
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.actor
            .send(Command::AbortTransmit {
                local: MessageId(1),
                reply,
            })
            .await;
        tokio::time::timeout(Duration::from_secs(10), answer)
            .await
            .expect("the actor answers the AbortTransmit")
            .expect("an answer")
    }

    /// How many connection states the rig has recorded (a mark for [`Rig::online_after`]).
    pub(crate) fn state_count(&self) -> usize {
        self.states.lock().expect("lock").len()
    }

    /// The `<message>` frames the client wrote with stanza id `id`, whole.
    pub(crate) fn message_frames(&self, id: &str) -> Vec<Node> {
        self.server
            .frame_nodes()
            .into_iter()
            .filter(|n| is_tag_id(n, "message", id))
            .collect()
    }

    /// Waits (at most 20 s) for a frame the client wrote that `wanted` picks.
    async fn frame(&self, wanted: impl Fn(&Node) -> bool) -> Node {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(frame) = self.server.frame_nodes().into_iter().find(|n| wanted(n)) {
                return frame;
            }
            assert!(
                Instant::now() < deadline,
                "the client never wrote the frame the test waits for"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Waits (at most 20 s) until the account is online again after the `from`th state it recorded.
    pub(crate) async fn online_after(&self, from: usize) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !self
            .states
            .lock()
            .expect("lock")
            .iter()
            .skip(from)
            .any(|s| matches!(s, ConnState::Online { .. }))
        {
            assert!(
                Instant::now() < deadline,
                "the account never came back online"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The core's `Transmit` of `req` (row `MessageId(1)`), sent and NOT awaited: the answer's receiver (W5b:
    /// `tests/media_wire.rs`, the quit while the CDN holds a POST, where the answer may never come).
    pub(crate) async fn transmit_detached(
        &self,
        req: &OutgoingRequest,
    ) -> tokio::sync::oneshot::Receiver<TransmitOutcome> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.actor
            .send(Command::Transmit {
                local: MessageId(1),
                req: Box::new(req.clone()),
                reply,
            })
            .await;
        answer
    }

    /// As [`Rig::finish`], returning how long the actor took to exit after the `Shutdown` (the wait fails after 60 s; the caller judges the time).
    pub(crate) async fn finish_timed(mut self) -> Duration {
        self.actor.send(Command::Shutdown).await;
        let started = Instant::now();
        let exited = tokio::time::timeout(Duration::from_secs(60), &mut self.actor.handle.exited)
            .await
            .expect("the actor ends within 60 s");
        let took = started.elapsed();
        assert_eq!(Exit::from_channel(exited), Exit::Shutdown);
        if let Some(thread) = self.actor.handle.thread.take() {
            thread.join().expect("the actor's thread joins");
        }
        self.drain.abort();
        took
    }

    /// Asks for a file (W7) and waits for its durable answer.
    pub(crate) async fn download(
        &self,
        remote: kourier_model::BackendBlob,
        dest: kourier_backend_api::BlobTarget,
    ) -> Result<kourier_model::AttachmentMeta, kourier_model::BackendError> {
        self.actor
            .send(Command::DownloadAttachment {
                attachment: kourier_model::AttachmentId(7),
                request: kourier_model::DownloadId(70),
                remote,
                dest,
                net_ids: Vec::new().into(),
            })
            .await;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let answer = self
                .events
                .lock()
                .expect("lock")
                .iter()
                .find_map(|event| match event {
                    BackendEvent::AttachmentDone { result, .. } => Some(result.clone()),
                    _ => None,
                });
            if let Some(answer) = answer {
                return answer;
            }
            assert!(Instant::now() < deadline, "the download was never answered");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The end: the actor shuts down, the core's side stops.
    pub(crate) async fn finish(mut self) {
        self.actor.send(Command::Shutdown).await;
        assert_eq!(self.actor.exit().await.0, Exit::Shutdown);
        self.drain.abort();
    }
}

fn attr(node: &Node, key: &str) -> Option<String> {
    node.attrs.get(key).map(|v| v.as_str().into_owned())
}

fn is_tag_id(node: &Node, tag: &str, id: &str) -> bool {
    node.tag == tag && attr(node, "id").as_deref() == Some(id)
}

/// The `decrypt-fail` of the `<enc>` the peer's `<to>` carries.
fn decrypt_fail_to_peer(frame: &Node) -> Option<String> {
    let peer_device: Jid = format!("{}:0@lid", peer::PEER_LID)
        .parse()
        .expect("the peer's device");
    frame
        .get_optional_child("participants")?
        .get_children_by_tag("to")
        .find(|to| {
            to.attrs
                .get("jid")
                .and_then(|v| v.as_str().parse::<Jid>().ok())
                .as_ref()
                == Some(&peer_device)
        })?
        .get_optional_child("enc")?
        .attrs
        .get("decrypt-fail")
        .map(|v| v.as_str().into_owned())
}

/// A receipt of a read (not the client's own delivery receipt, which is plain or `inactive`).
fn is_a_read(receipt: &Node) -> bool {
    matches!(
        attr(receipt, "type").as_deref(),
        Some("read" | "read-self" | "played")
    )
}

/// The key id of the protocol message (edit, revoke) the peer read, and its type.
fn protocol_of(read: &wa::Message) -> Option<(wa::message::protocol_message::Type, String)> {
    let protocol = read.protocol_message.as_option()?;
    Some((protocol.r#type?, protocol.key.as_option()?.id.clone()?))
}

/// The target of a message the peer wrote in the 1:1 chat.
fn peer_target(local: &str) -> MsgTarget {
    MsgTarget {
        conv: None,
        author: TargetAuthor::User(peer_user()),
        local: local.into(),
        alternates: Vec::new(),
    }
}

fn sent_at(t: u64) -> TransmitOutcome {
    TransmitOutcome::Sent {
        server_at: Some(i64::try_from(t * 1000).expect("time")),
        per_recipient: Vec::new(),
    }
}

/// `the_stanza_id_is_the_cores_local_for_every_kind`: a text, an edit of it, a reaction to it and a revoke
/// of it go out through the actor, each as ONE `<message>` whose id is the core's `local`; the edit is
/// `edit="1"` with `decrypt-fail="hide"`, the reaction `type="reaction"` with hide, the revoke `edit="7"`
/// without it; the peer reads each, keyed by the ORIGINAL's id; each is answered `Sent` at the ack's time
/// and is `acked` in the ledger; the acked revoke took the retry-cache rows of the text and of its edit
/// (D4), the reaction and the revoke keep theirs.
async fn the_stanza_id_is_the_cores_local_for_every_kind() {
    let (dir, device) = account_dir().await;
    let script = Script::acked();
    let Ack::Ok { t } = script.ack else {
        unreachable!("an acked script")
    };
    let rig = Rig::start(dir.path(), server_of(&script)).await;
    let mut peer = Peer::new(&device, None).await;
    rig.peer_writes_first(&mut peer).await;

    let kinds = [
        (TEXT_ID, text_request(TEXT_ID)),
        (
            EDIT_ID,
            request(
                EDIT_ID,
                OutgoingKind::Edit {
                    target: own_target(TEXT_ID),
                    body: Body::Text(Text::plain(EDITED)),
                },
            ),
        ),
        (
            REACT_ID,
            request(
                REACT_ID,
                OutgoingKind::React {
                    target: own_target(TEXT_ID),
                    emoji: EMOJI.into(),
                    remove: false,
                },
            ),
        ),
        (
            REVOKE_ID,
            request(
                REVOKE_ID,
                OutgoingKind::Delete {
                    target: own_target(TEXT_ID),
                    for_everyone: true,
                },
            ),
        ),
    ];
    for (id, req) in &kinds {
        assert_eq!(
            rig.transmit(req).await,
            sent_at(t),
            "a send is not answered Sent at the ack's time"
        );
        let frames = rig.message_frames(id);
        assert_eq!(
            frames.len(),
            1,
            "a send is not ONE frame with the core's id as its stanza id"
        );
        let frame = &frames[0];
        let read = peer.read(frame).await.expect("the peer reads the frame");
        match *id {
            TEXT_ID => {
                assert_eq!(attr(frame, "type").as_deref(), Some("text"));
                assert_eq!(attr(frame, "edit"), None, "a text is no edit");
                assert!(
                    read.conversation.as_deref() == Some(TEXT),
                    "the peer read another text"
                );
            }
            EDIT_ID => {
                assert_eq!(
                    attr(frame, "edit").as_deref(),
                    Some("1"),
                    "an edit's attribute"
                );
                assert_eq!(decrypt_fail_to_peer(frame).as_deref(), Some("hide"));
                let (kind, key) = protocol_of(&read).expect("the peer reads a protocol message");
                assert_eq!(kind, wa::message::protocol_message::Type::MESSAGE_EDIT);
                assert_eq!(key, TEXT_ID, "the edit is keyed by the original's id");
                let edited = read
                    .protocol_message
                    .as_option()
                    .and_then(|p| p.edited_message.as_option())
                    .and_then(|m| m.conversation.clone());
                assert!(
                    edited.as_deref() == Some(EDITED),
                    "the peer read another edited text"
                );
            }
            REACT_ID => {
                assert_eq!(attr(frame, "type").as_deref(), Some("reaction"));
                assert_eq!(attr(frame, "edit"), None, "a reaction is no edit");
                assert_eq!(decrypt_fail_to_peer(frame).as_deref(), Some("hide"));
                let reaction = read
                    .reaction_message
                    .as_option()
                    .expect("the peer reads a reaction");
                assert_eq!(
                    reaction.key.as_option().and_then(|k| k.id.as_deref()),
                    Some(TEXT_ID),
                    "the reaction is keyed by the original's id"
                );
                assert!(reaction.text.as_deref() == Some(EMOJI), "another emoji");
            }
            _ => {
                assert_eq!(
                    attr(frame, "edit").as_deref(),
                    Some("7"),
                    "a revoke's attribute"
                );
                assert_eq!(decrypt_fail_to_peer(frame), None, "a revoke is not hidden");
                let (kind, key) = protocol_of(&read).expect("the peer reads a protocol message");
                assert_eq!(kind, wa::message::protocol_message::Type::REVOKE);
                assert_eq!(key, TEXT_ID, "the revoke is keyed by the original's id");
            }
        }
    }
    rig.finish().await;

    let conn = raw(dir.path());
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM send_outcomes WHERE outcome = 'acked' AND server_t IS NOT NULL"
        ),
        4,
        "every kind is acked in the ledger with the server's time"
    );
    let cached = |id: &str| {
        count(
            &conn,
            &format!("SELECT count(*) FROM sent_messages WHERE message_id = '{id}'"),
        )
    };
    assert_eq!(
        cached(REACT_ID),
        1,
        "the reaction keeps its retry-cache row"
    );
    assert_eq!(cached(REVOKE_ID), 1, "the revoke keeps its retry-cache row");
    assert_eq!(
        cached(TEXT_ID) + cached(EDIT_ID),
        0,
        "the acked revoke left the deleted text in the retry cache"
    );
}

/// `the_peer_reads_the_text_and_our_phone_gets_its_copy`: our own phone (device 0, a cold session from
/// its bundle) is in the account's device list; the actor's frame for the core's text has a `<to>` for
/// the peer, who reads the text, and one for the phone, which reads a `DeviceSentMessage` naming the chat.
async fn the_peer_reads_the_text_and_our_phone_gets_its_copy() {
    let (dir, device) = account_dir().await;
    let mut phone = Peer::own_phone_responder(PHONE_SIGNED_ID, PHONE_ONE_TIME_ID).await;
    let script = Script {
        own_devices: vec![0, 1],
        phone: Some(phone.bundle()),
        ..Script::acked()
    };
    let rig = Rig::start(dir.path(), server_of(&script)).await;
    let mut peer = Peer::new(&device, None).await;
    rig.peer_writes_first(&mut peer).await;

    let outcome = rig.transmit(&text_request(COPY_ID)).await;
    assert!(
        matches!(outcome, TransmitOutcome::Sent { .. }),
        "the text is not answered Sent"
    );
    let frames = rig.message_frames(COPY_ID);
    assert_eq!(frames.len(), 1, "one frame for one text");
    let read = peer
        .read(&frames[0])
        .await
        .expect("the peer reads the frame");
    assert!(
        read.conversation.as_deref() == Some(TEXT),
        "the peer read another text"
    );
    let copy = phone
        .read(&frames[0])
        .await
        .expect("our phone reads its copy");
    let sent = copy
        .device_sent_message
        .as_option()
        .expect("the phone's copy is a DeviceSentMessage");
    assert!(
        sent.destination_jid.as_deref() == Some(format!("{}@lid", peer::PEER_LID).as_str()),
        "the copy names another chat"
    );
    assert!(
        sent.message
            .as_option()
            .and_then(|m| m.conversation.as_deref())
            == Some(TEXT),
        "the copy holds another text"
    );
    rig.finish().await;
}

/// `an_ack_is_sent_with_the_servers_time`: the server's ack `t` (seconds) is the core's `server_at`
/// (milliseconds), and the ledger keeps it as `server_t`.
async fn an_ack_is_sent_with_the_servers_time() {
    let (dir, device) = account_dir().await;
    // A server time an hour behind ours: the answer carries the SERVER's time, not the clock's.
    let t = now_secs() - 3_600;
    let script = Script {
        ack: Ack::Ok { t },
        ..Script::acked()
    };
    let rig = Rig::start(dir.path(), server_of(&script)).await;
    let mut peer = Peer::new(&device, None).await;
    rig.peer_writes_first(&mut peer).await;
    assert_eq!(rig.transmit(&text_request(ACKED_ID)).await, sent_at(t));
    rig.finish().await;
    let conn = raw(dir.path());
    assert_eq!(
        count(
            &conn,
            &format!(
                "SELECT count(*) FROM send_outcomes WHERE local = '{ACKED_ID}' \
                 AND outcome = 'acked' AND server_t = {t}"
            )
        ),
        1,
        "the ledger does not keep the server's time of the ack"
    );
}

pub(crate) fn refused(outcome: &TransmitOutcome) -> bool {
    matches!(outcome, TransmitOutcome::Failed(e) if e.kind == ErrorKind::ReachOutRefused)
}

/// `a_463_fails_for_good_and_a_resend_sends_no_frame`: the server refuses the text with a 463; the answer
/// is `ReachOutRefused`; the core's resend of the same `local` on the same connection AND after a restart
/// (a new connection, the ledger only) is answered the same with NO frame: one `<message>` in all.
async fn a_463_fails_for_good_and_a_resend_sends_no_frame() {
    a_refusal_fails_for_good_and_a_resend_sends_no_frame(463, REFUSED_ID).await;
}

/// `a_475_fails_for_good_and_a_resend_sends_no_frame` (fix F8): the same for a 475 (a cap on messages to new chats,
/// met like a 463 until a real run shows what it means).
async fn a_475_fails_for_good_and_a_resend_sends_no_frame() {
    a_refusal_fails_for_good_and_a_resend_sends_no_frame(475, REFUSED_475_ID).await;
}

async fn a_refusal_fails_for_good_and_a_resend_sends_no_frame(code: u16, local: &str) {
    let (dir, device) = account_dir().await;
    let script = Script {
        ack: Ack::Error(code),
        ..Script::acked()
    };
    let rig = Rig::start(dir.path(), server_of(&script)).await;
    let mut peer = Peer::new(&device, None).await;
    rig.peer_writes_first(&mut peer).await;
    let req = text_request(local);
    assert!(
        refused(&rig.transmit(&req).await),
        "a refusal is not ReachOutRefused"
    );
    assert!(
        refused(&rig.transmit(&req).await),
        "the resend on the same connection is not ReachOutRefused"
    );
    assert_eq!(
        rig.message_frames(local).len(),
        1,
        "the resend wrote a frame"
    );
    rig.finish().await;

    // A new run, a new connection: only the ledger knows.
    let again = Rig::start(dir.path(), server_of(&script)).await;
    assert!(
        refused(&again.transmit(&req).await),
        "the resend after a restart is not ReachOutRefused"
    );
    assert_eq!(
        again.message_frames(local).len(),
        0,
        "the resend after a restart wrote a frame"
    );
    again.finish().await;
    let conn = raw(dir.path());
    assert_eq!(
        count(
            &conn,
            &format!(
                "SELECT count(*) FROM send_outcomes WHERE local = '{local}' AND outcome = 'refused'"
            )
        ),
        1
    );
}

/// `a_429_is_not_answered_by_a_second_frame` (fix F1, the interop BLOCKER; amends D21): the server nacks the text
/// with a 429; the answer is `RateLimited` with the floor (five minutes) as `retry_after`; the core's retry at
/// once on the same connection writes NO second frame and hears the same failure with what is left of the
/// floor: one `<message>` of the local. A later connection (a new run) may write it.
async fn a_429_is_not_answered_by_a_second_frame() {
    let (dir, device) = account_dir().await;
    let script = Script {
        ack: Ack::Error(429),
        ..Script::acked()
    };
    let rig = Rig::start(dir.path(), server_of(&script)).await;
    let mut peer = Peer::new(&device, None).await;
    rig.peer_writes_first(&mut peer).await;
    let req = text_request(RATE_ID);
    let floor = Duration::from_secs(300);
    for (n, what) in ["the first answer", "the retry at once"]
        .into_iter()
        .enumerate()
    {
        match rig.transmit(&req).await {
            TransmitOutcome::Failed(e) => match e.kind {
                ErrorKind::RateLimited {
                    retry_after: Some(left),
                } => assert!(
                    left <= floor && left > Duration::ZERO,
                    "{what}: the floor left is out of range"
                ),
                other => panic!("{what} is not RateLimited with a floor: attempt {n}, {other:?}"),
            },
            other => panic!("{what} did not fail: {other:?}"),
        }
    }
    assert_eq!(
        rig.message_frames(RATE_ID).len(),
        1,
        "the retry of a 429 wrote a second frame on the same connection"
    );
    rig.finish().await;

    // A later connection may write it again (the nack is known, nothing of it is on the server).
    let again = Rig::start(dir.path(), server_of(&script)).await;
    assert!(matches!(
        again.transmit(&req).await,
        TransmitOutcome::Failed(e) if matches!(e.kind, ErrorKind::RateLimited { .. })
    ));
    assert_eq!(
        again.message_frames(RATE_ID).len(),
        1,
        "a later connection writes the frame"
    );
    again.finish().await;
}

/// `a_read_goes_out_as_read_or_read_self_by_the_phones_setting`: the phone says read receipts are off
/// (`readreceipts` = none): the core's read of the peer's message in the 1:1 chat goes out as
/// `read-self` (his phone learns, the peer does not); a read in a group goes out as `read`, with the
/// author as `participant` (WhatsApp's rule: groups always tell).
async fn a_read_goes_out_as_read_or_read_self_by_the_phones_setting() {
    let (dir, device) = account_dir().await;
    let script = Script {
        read_receipts: "none",
        ..Script::acked()
    };
    let server = server_of(&script);
    // The group's addressing mode is known before its receipt is planned (fix F14: a guess is no plan).
    let own: &'static str = format!("{}@lid", peer::ACCOUNT_LID).leak();
    let member: &'static str = format!("{}@lid", peer::PEER_LID).leak();
    server.on_iq(
        "w:g2",
        wire::group_info(
            GROUP,
            "lid",
            vec![(own, Some("superadmin")), (member, None)],
        ),
    );
    let rig = Rig::start(dir.path(), server).await;
    let mut peer = Peer::new(&device, None).await;
    rig.peer_writes_first(&mut peer).await;
    let of_peer = |local: &str| MsgTarget {
        conv: None,
        author: TargetAuthor::User(peer_user()),
        local: local.into(),
        alternates: Vec::new(),
    };
    rig.actor
        .send(Command::SendReceipts {
            cmd: CmdId(71),
            conv: chat(),
            targets: vec![of_peer(FIRST_ID)],
            kind: ReceiptKind::Read,
        })
        .await;
    let read = rig
        .frame(|f| is_tag_id(f, "receipt", FIRST_ID) && is_a_read(f))
        .await;
    assert_eq!(
        attr(&read, "type").as_deref(),
        Some("read-self"),
        "a 1:1 read with read receipts off is not read-self"
    );
    rig.actor
        .send(Command::SendReceipts {
            cmd: CmdId(72),
            conv: ConvKey::Group(format!("wag:{GROUP}").into()),
            targets: vec![of_peer(GROUP_MSG_ID)],
            kind: ReceiptKind::Read,
        })
        .await;
    let group_read = rig.frame(|f| is_tag_id(f, "receipt", GROUP_MSG_ID)).await;
    assert_eq!(
        attr(&group_read, "type").as_deref(),
        Some("read"),
        "a group read is not read"
    );
    assert!(
        attr(&group_read, "to").as_deref() == Some(format!("{GROUP}@g.us").as_str()),
        "the group read is not written to the group"
    );
    assert!(
        attr(&group_read, "participant")
            .is_some_and(|p| p.starts_with(peer::PEER_LID) && p.ends_with("@lid")),
        "the group read does not name the author"
    );
    rig.heard(2, |e| matches!(e, BackendEvent::CommandPersisted { .. }))
        .await;
    rig.finish().await;
    let conn = raw(dir.path());
    assert_eq!(
        count(&conn, "SELECT count(*) FROM receipt_jobs"),
        0,
        "a written receipt's job is left"
    );
}

/// `a_group_receipt_waits_for_a_group_info_that_fails` (fix F21, re-check N1): the group's info ask (`w:g2`) is
/// refused with a 503 (the server's own trouble) while the person reads the group's message. The REAL session reads
/// that failure by upstream's typed error, so the receipt job is KEPT (no receipt is written, the job is not dropped as
/// one that can never be sent); when the server answers again, the next drain writes it, then the new job. With the
/// failure read as internal the job would be lost and the first receipt never written.
async fn a_group_receipt_waits_for_a_group_info_that_fails() {
    let (dir, _device) = account_dir().await;
    let server = server_of(&Script::acked());
    let own: &'static str = format!("{}@lid", peer::ACCOUNT_LID).leak();
    let member: &'static str = format!("{}@lid", peer::PEER_LID).leak();
    let info = wire::group_info(
        GROUP,
        "lid",
        vec![(own, Some("superadmin")), (member, None)],
    );
    let healthy = Arc::new(AtomicBool::new(false));
    let switch = healthy.clone();
    server.on_iq("w:g2", move |request| {
        if switch.load(Ordering::SeqCst) {
            info(request)
        } else {
            Some(wire::Answer::Error(503))
        }
    });
    let rig = Rig::start(dir.path(), server).await;
    let group = ConvKey::Group(format!("wag:{GROUP}").into());
    let read = |cmd, local: &str| Command::SendReceipts {
        cmd: CmdId(cmd),
        conv: group.clone(),
        targets: vec![peer_target(local)],
        kind: ReceiptKind::Read,
    };
    rig.actor.send(read(76, GROUP_MSG_ID)).await;
    rig.heard(1, |e| matches!(e, BackendEvent::CommandPersisted { .. }))
        .await;
    // The runner asked for the group's info and was refused; nothing is written for the group.
    let deadline = Instant::now() + Duration::from_secs(20);
    while rig.server.answered("w:g2") == 0 {
        assert!(
            Instant::now() < deadline,
            "the group's info was never asked"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !rig.server
            .frame_nodes()
            .iter()
            .any(|f| is_tag_id(f, "receipt", GROUP_MSG_ID)),
        "a receipt went out although the group's addressing mode was unknown"
    );
    // The server answers again: the next drain writes the job the failure kept, then the new one.
    healthy.store(true, Ordering::SeqCst);
    rig.actor.send(read(77, GROUP_READ_ID)).await;
    rig.heard(2, |e| matches!(e, BackendEvent::CommandPersisted { .. }))
        .await;
    for id in [GROUP_MSG_ID, GROUP_READ_ID] {
        let sent = rig.frame(|f| is_tag_id(f, "receipt", id)).await;
        assert_eq!(
            attr(&sent, "type").as_deref(),
            Some("read"),
            "a group read is not read"
        );
        assert!(
            attr(&sent, "participant")
                .is_some_and(|p| p.starts_with(peer::PEER_LID) && p.ends_with("@lid")),
            "the group read does not name the author"
        );
    }
    rig.finish().await;
    let conn = raw(dir.path());
    assert_eq!(
        count(&conn, "SELECT count(*) FROM receipt_jobs"),
        0,
        "a written receipt's job is left"
    );
}

/// `a_first_read_waits_for_the_phones_setting` (fix F5, privacy LOW): the phone's settings answer is held back
/// (upstream's background fetch is slow) while the person reads the peer's message: NO read of any kind goes
/// out, however long we wait (it would say `read` on a phone that turned read receipts off); when the answer
/// comes (`readreceipts` = none) the receipt goes out as `read-self`.
async fn a_first_read_waits_for_the_phones_setting() {
    let (dir, device) = account_dir().await;
    let script = Script {
        read_receipts: "none",
        ..Script::acked()
    };
    let server = server_of(&script);
    server.hold_answers("privacy");
    let rig = Rig::start(dir.path(), server).await;
    let mut peer = Peer::new(&device, None).await;
    rig.peer_writes_first(&mut peer).await;
    rig.actor
        .send(Command::SendReceipts {
            cmd: CmdId(75),
            conv: chat(),
            targets: vec![peer_target(FIRST_ID)],
            kind: ReceiptKind::Read,
        })
        .await;
    rig.heard(1, |e| matches!(e, BackendEvent::CommandPersisted { .. }))
        .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !rig.server
            .frame_nodes()
            .iter()
            .any(|f| f.tag == "receipt" && is_a_read(f)),
        "a read went out before the phone's setting was known"
    );
    rig.server.release_answers("privacy");
    let read = rig
        .frame(|f| is_tag_id(f, "receipt", FIRST_ID) && is_a_read(f))
        .await;
    assert_eq!(
        attr(&read, "type").as_deref(),
        Some("read-self"),
        "the read ignored the setting that came"
    );
    rig.finish().await;
}

/// D20 (critic M2): a 1:1-LID-MIGRATED account whose store maps the peer's number to its LID writes a chat
/// the core keys by the NUMBER as the LID everywhere it writes the chat itself: the key of an edit (the
/// peer reads `remoteJid` = the LID) and the `to` of a read receipt. Upstream's own message stanza goes to
/// the LID too (its `resolve_dm_wire_jid`).
async fn a_migrated_account_writes_a_number_chat_as_its_lid() {
    let (dir, device) = account_dir().await;
    {
        let owner = kourier_whatsapp::StoreOwner::open(
            &support::db_path(dir.path()),
            &support::key(),
            "wa-test",
        )
        .expect("open");
        let mut migrated = device.clone();
        migrated.lid_migrated = true;
        wacore::store::traits::DeviceStore::save(&owner.store(), &migrated)
            .await
            .expect("the migrated device");
        owner.close();
        raw(dir.path())
            .execute(
                "INSERT INTO lid_pn_mapping(lid, phone, created_at, updated_at, learning_source) \
                 VALUES (?1, ?2, 1700000000, 1700000000, 'usync')",
                [peer::PEER_LID, PEER_PN],
            )
            .expect("the peer's number and LID");
    }
    let script = Script::acked();
    let rig = Rig::start(dir.path(), server_of(&script)).await;
    let mut peer = Peer::new(&device, None).await;
    rig.peer_writes_first(&mut peer).await;
    let in_number = |local: &str, kind| OutgoingRequest {
        conv: number_chat(),
        ..request(local, kind)
    };
    let text = in_number(
        NUMBER_TEXT_ID,
        OutgoingKind::Message {
            body: Body::Text(Text::plain(TEXT)),
            reply_to: None,
            expiry: None,
        },
    );
    assert!(
        matches!(rig.transmit(&text).await, TransmitOutcome::Sent { .. }),
        "the text into the number chat is not answered Sent"
    );
    let lid_chat = format!("{}@lid", peer::PEER_LID);
    let frames = rig.message_frames(NUMBER_TEXT_ID);
    assert_eq!(frames.len(), 1, "one frame for one text");
    assert!(
        attr(&frames[0], "to").as_deref() == Some(lid_chat.as_str()),
        "the message stanza is not written to the LID"
    );
    let _ = peer
        .read(&frames[0])
        .await
        .expect("the peer reads the text");
    // Fix F9: the peer's delivery receipt comes back from the LID the text was written to; the core holds the chat
    // under the number, so the backend states it for the chat the ledger holds for that message, by the number.
    rig.server.send(
        &NodeBuilder::new("receipt")
            .attr("from", lid_chat.clone())
            .attr("id", NUMBER_TEXT_ID)
            .attr("t", now_secs().to_string())
            .build(),
    );
    rig.heard(1, |e| {
        matches!(e, BackendEvent::Receipts { targets, .. }
            if targets.iter().any(|t| &*t.local == NUMBER_TEXT_ID))
    })
    .await;
    let heard: Vec<(Option<ConvKey>, UserId)> = rig
        .events
        .lock()
        .expect("lock")
        .iter()
        .filter_map(|e| match e {
            BackendEvent::Receipts { targets, by, .. }
                if targets.iter().any(|t| &*t.local == NUMBER_TEXT_ID) =>
            {
                Some((targets[0].conv.clone(), by.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        heard,
        [(
            Some(number_chat()),
            UserId::from_parts("pn", PEER_PN).expect("the peer's number")
        )],
        "the delivery receipt from the LID is not stated for the number chat the core holds"
    );
    let edit = in_number(
        NUMBER_EDIT_ID,
        OutgoingKind::Edit {
            target: target_in(number_chat(), NUMBER_TEXT_ID),
            body: Body::Text(Text::plain(EDITED)),
        },
    );
    assert!(
        matches!(rig.transmit(&edit).await, TransmitOutcome::Sent { .. }),
        "the edit in the number chat is not answered Sent"
    );
    let frames = rig.message_frames(NUMBER_EDIT_ID);
    assert_eq!(frames.len(), 1, "one frame for one edit");
    let read = peer
        .read(&frames[0])
        .await
        .expect("the peer reads the edit");
    let key = read
        .protocol_message
        .as_option()
        .and_then(|p| p.key.as_option())
        .expect("the edit carries the original's key");
    assert!(
        key.remote_jid.as_deref() == Some(lid_chat.as_str()),
        "the edit's key names the chat by its number"
    );
    assert_eq!(key.id.as_deref(), Some(NUMBER_TEXT_ID));
    rig.actor
        .send(Command::SendReceipts {
            cmd: CmdId(73),
            conv: number_chat(),
            targets: vec![MsgTarget {
                conv: None,
                author: TargetAuthor::User(
                    UserId::from_parts("pn", PEER_PN).expect("the peer's number"),
                ),
                local: FIRST_ID.into(),
                alternates: Vec::new(),
            }],
            kind: ReceiptKind::Read,
        })
        .await;
    let receipt = rig
        .frame(|f| is_tag_id(f, "receipt", FIRST_ID) && is_a_read(f))
        .await;
    assert!(
        attr(&receipt, "to").as_deref() == Some(lid_chat.as_str()),
        "the read receipt is not written to the LID"
    );
    rig.finish().await;
}

fn transient_now(outcome: &TransmitOutcome) -> bool {
    matches!(
        outcome,
        TransmitOutcome::Failed(e) if e.kind == ErrorKind::Transient { retry_after: None }
    )
}

/// Critic H1 (D5): a frame of a local is in the air (the server keeps its ack back) when the core sends the
/// same local again: the second `Transmit` writes NO frame on the connection and waits for the first
/// frame's answer; the late ack is a 463, and BOTH answers are `ReachOutRefused` from ONE frame (a 463 is
/// met once); the ledger keeps one `refused` row.
async fn a_frame_in_the_air_is_written_once_and_its_ack_answers_every_transmit() {
    let (dir, device) = account_dir().await;
    let script = Script::acked();
    let server = server_of(&script);
    server.ack_id(IN_AIR_ID, Ack::Silent);
    let rig = Rig::start(dir.path(), server).await;
    let mut peer = Peer::new(&device, None).await;
    rig.peer_writes_first(&mut peer).await;
    let req = text_request(IN_AIR_ID);
    let (first, second) = tokio::join!(rig.transmit(&req), async {
        let frame = rig.frame(|f| is_tag_id(f, "message", IN_AIR_ID)).await;
        // The core's resend of the same local while the first frame waits for its ack.
        let resend = rig.transmit(&req);
        let ack = async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert_eq!(
                rig.message_frames(IN_AIR_ID).len(),
                1,
                "a second frame of the local went out on the connection"
            );
            let to = attr(&frame, "to").expect("the frame's to");
            rig.server.ack_now(IN_AIR_ID, &to, &Ack::Error(463));
        };
        tokio::join!(resend, ack).0
    });
    assert!(refused(&first), "the first Transmit is not ReachOutRefused");
    assert!(
        refused(&second),
        "the Transmit that waited for the frame in the air is not ReachOutRefused"
    );
    assert_eq!(
        rig.message_frames(IN_AIR_ID).len(),
        1,
        "the local was written more than once"
    );
    rig.finish().await;
    let conn = raw(dir.path());
    assert_eq!(
        count(
            &conn,
            &format!(
                "SELECT count(*) FROM send_outcomes WHERE local = '{IN_AIR_ID}' AND outcome = 'refused'"
            )
        ),
        1,
        "the late 463 is not one refused row"
    );
}

/// D5 (brief L4): the server ends the connection right after the frame, with no ack (the ack is lost): the
/// answer is `Transient{retry_after: None}`; the client connects again, and the core's resend goes out on
/// the NEW connection with the SAME id (his answer "Resend, same id": a second message is impossible), the
/// peer reads it, and it is answered `Sent` and `acked`.
async fn a_lost_ack_is_transient_and_the_resend_uses_the_same_id() {
    let (dir, device) = account_dir().await;
    let script = Script::acked();
    let Ack::Ok { t } = script.ack else {
        unreachable!("an acked script")
    };
    let rig = Rig::start(dir.path(), server_of(&script)).await;
    let mut peer = Peer::new(&device, None).await;
    rig.peer_writes_first(&mut peer).await;
    let before = rig.states.lock().expect("lock").len();
    rig.server.ack_next(Ack::DropConnection);
    let req = text_request(LOST_ID);
    assert!(
        transient_now(&rig.transmit(&req).await),
        "a lost ack is not Transient with no wait"
    );
    assert!(
        rig.server.wait_opened(2, Duration::from_secs(30)).await,
        "the client never connected again"
    );
    rig.server
        .send(&wire::success_as_the_server_writes_it(peer::ACCOUNT_LID));
    rig.online_after(before).await;
    assert_eq!(
        rig.transmit(&req).await,
        sent_at(t),
        "the resend on the new connection is not answered Sent"
    );
    let frames = rig.message_frames(LOST_ID);
    assert_eq!(
        frames.len(),
        2,
        "the frame and its resend do not both carry the core's id"
    );
    let read = peer
        .read(&frames[1])
        .await
        .expect("the peer reads the resend");
    assert!(
        read.conversation.as_deref() == Some(TEXT),
        "the peer read another text"
    );
    rig.finish().await;
    let conn = raw(dir.path());
    assert_eq!(
        count(
            &conn,
            &format!(
                "SELECT count(*) FROM send_outcomes WHERE local = '{LOST_ID}' AND outcome = 'acked'"
            )
        ),
        1,
        "the resend is not acked in the ledger"
    );
}

/// D5 with the REAL wait (the slow set: 75 s of wall time, the non-test `ACK_WAIT`): the server keeps the ack
/// of a frame back for ever on a live connection: after `ACK_WAIT` the answer is `Transient{retry_after:
/// None}`, the session ENDS the connection itself (`RealSession::drop_connection`: upstream's reconnect), the
/// client connects again, and the core's resend goes out on the new connection with the SAME id and is acked.
async fn a_silent_ack_ends_the_connection_and_the_resend_uses_the_same_id() {
    let (dir, device) = account_dir().await;
    let script = Script::acked();
    let Ack::Ok { t } = script.ack else {
        unreachable!("an acked script")
    };
    let rig = Rig::start(dir.path(), server_of(&script)).await;
    let mut peer = Peer::new(&device, None).await;
    rig.peer_writes_first(&mut peer).await;
    let before = rig.states.lock().expect("lock").len();
    rig.server.ack_next(Ack::Silent);
    let req = text_request(SILENT_ID);
    let asked = Instant::now();
    assert!(
        transient_now(&rig.transmit(&req).await),
        "a frame with no ack in time is not Transient with no wait"
    );
    assert!(
        asked.elapsed() >= Duration::from_secs(70),
        "the answer did not wait for the ack"
    );
    // The session ends the connection at once (upstream's reconnect waits its own few seconds): well before
    // anything else of upstream would notice a quiet socket.
    assert!(
        rig.server.wait_opened(2, Duration::from_secs(15)).await,
        "the connection was not ended and opened again"
    );
    // Fix F11: the rig answers upstream's keepalive ping now, so upstream does not reconnect by itself during the
    // silence: the second connection exists because the SESSION ended the first one (`drop_connection`), and
    // it closed that transport itself.
    assert!(
        rig.server.client_closed(0),
        "the first connection was not closed by the client: something else ended it"
    );
    assert_eq!(
        rig.message_frames(SILENT_ID).len(),
        1,
        "the local was written again on the connection that never acked it"
    );
    rig.server
        .send(&wire::success_as_the_server_writes_it(peer::ACCOUNT_LID));
    rig.online_after(before).await;
    assert_eq!(
        rig.transmit(&req).await,
        sent_at(t),
        "the resend on the new connection is not answered Sent"
    );
    assert_eq!(
        rig.message_frames(SILENT_ID).len(),
        2,
        "the frame and its resend do not both carry the core's id"
    );
    assert_eq!(
        rig.server.dials(),
        2,
        "the client dialled more than the one reconnect the session asked for"
    );
    rig.finish().await;
}

/// D13 (brief L10): a group text is ONE `<message>` to the group whose content is a sender-key message
/// (`<enc type="skmsg">`), with the sender key handed to the member's device in its `<to>` (the peer reads a
/// `SenderKeyDistributionMessage` for the group); it is answered `Sent`, never `Partial`. A text the core
/// asks for some members only (`only`) is sent to the whole group, also `Sent`.
async fn a_group_text_is_one_stanza_with_sender_keys() {
    let (dir, device) = account_dir().await;
    let script = Script::acked();
    let server = server_of(&script);
    let own: &'static str = format!("{}@lid", peer::ACCOUNT_LID).leak();
    let member: &'static str = format!("{}@lid", peer::PEER_LID).leak();
    server.on_iq(
        "w:g2",
        wire::group_info(
            GROUP,
            "lid",
            vec![(own, Some("superadmin")), (member, None)],
        ),
    );
    let rig = Rig::start(dir.path(), server).await;
    let mut peer = Peer::new(&device, None).await;
    rig.peer_writes_first(&mut peer).await;
    let group = ConvKey::Group(format!("wag:{GROUP}").into());
    let in_group = |local: &str, only| OutgoingRequest {
        conv: group.clone(),
        only,
        ..text_request(local)
    };
    let whole = |outcome: &TransmitOutcome| matches!(outcome, TransmitOutcome::Sent { per_recipient, .. } if per_recipient.is_empty());
    assert!(
        whole(&rig.transmit(&in_group(GROUP_TEXT_ID, None)).await),
        "a group text is not answered Sent for the whole group"
    );
    let frames = rig.message_frames(GROUP_TEXT_ID);
    assert_eq!(frames.len(), 1, "a group text is not ONE stanza");
    let frame = &frames[0];
    let group_jid = format!("{GROUP}@g.us");
    assert!(
        attr(frame, "to").as_deref() == Some(group_jid.as_str()),
        "the group text is not written to the group"
    );
    assert_eq!(attr(frame, "type").as_deref(), Some("text"));
    let skmsg = frame
        .get_children_by_tag("enc")
        .filter(|enc| attr(enc, "type").as_deref() == Some("skmsg"))
        .count();
    assert_eq!(skmsg, 1, "the group text is not one sender-key message");
    let read = peer
        .read(frame)
        .await
        .expect("the member's device reads its <to>");
    let distribution = read
        .sender_key_distribution_message
        .as_option()
        .expect("the member gets the sender key");
    assert!(
        distribution.group_id.as_deref() == Some(group_jid.as_str()),
        "the sender key names another group"
    );
    // `only` (a retry to some members) never happens in W5: sent whole, answered Sent.
    assert!(
        whole(
            &rig.transmit(&in_group(GROUP_ONLY_ID, Some(vec![peer_user()])))
                .await
        ),
        "a group text for some members is not answered Sent"
    );
    let frames = rig.message_frames(GROUP_ONLY_ID);
    assert_eq!(
        frames.len(),
        1,
        "a group text for some members is not ONE stanza"
    );
    assert!(
        attr(&frames[0], "to").as_deref() == Some(group_jid.as_str()),
        "a group text for some members is not written to the whole group"
    );
    rig.finish().await;
}

/// Fix F17 (privacy LOW-6): a typing state goes into a chat the phone started (`composing`, then `paused`) and into
/// no other: nothing is written for a chat nobody started. The D10 grep of the child also reads what upstream and
/// Kourier log on this path (upstream's chatstate line names the JID at DEBUG; the app's rule drops it).
async fn a_typing_state_goes_only_into_a_chat_the_phone_started() {
    let (dir, device) = account_dir().await;
    let rig = Rig::start(dir.path(), server_of(&Script::acked())).await;
    let mut peer = Peer::new(&device, None).await;
    rig.peer_writes_first(&mut peer).await;
    // A send is answered `Sent` only when the hold gate is open: typing is dropped while the lock is unknown (D8).
    assert!(
        matches!(
            rig.transmit(&text_request(TEXT_ID)).await,
            TransmitOutcome::Sent { .. }
        ),
        "the first text is not answered Sent"
    );
    let stranger = ConvKey::Direct(UserId::from_parts("lid", STRANGER_LID).expect("a user"));
    let stranger_jid = format!("{STRANGER_LID}@lid");
    let peer_jid = format!("{}@lid", peer::PEER_LID);
    // The stranger first: its state would be written before the peer's if anything wrote it.
    rig.actor
        .send(Command::SetTyping {
            conv: stranger.clone(),
            on: true,
        })
        .await;
    rig.actor
        .send(Command::SetTyping {
            conv: chat(),
            on: true,
        })
        .await;
    let composing = rig
        .frame(|f| f.tag == "chatstate" && f.get_optional_child("composing").is_some())
        .await;
    assert_eq!(
        attr(&composing, "to").as_deref(),
        Some(peer_jid.as_str()),
        "the typing state is not written to the started chat"
    );
    rig.actor
        .send(Command::SetTyping {
            conv: chat(),
            on: false,
        })
        .await;
    rig.actor
        .send(Command::SetTyping {
            conv: stranger,
            on: false,
        })
        .await;
    rig.frame(|f| f.tag == "chatstate" && f.get_optional_child("paused").is_some())
        .await;
    // The states that were not written would have left by now: the actor handled the stranger's commands first.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let states: Vec<Node> = rig
        .server
        .frame_nodes()
        .into_iter()
        .filter(|f| f.tag == "chatstate")
        .collect();
    assert_eq!(states.len(), 2, "one `composing` and one `paused`, no more");
    assert!(
        states
            .iter()
            .all(|f| attr(f, "to").as_deref() != Some(stranger_jid.as_str())),
        "a typing state was written into a chat nobody started"
    );
    rig.finish().await;
}

/// Fix F17: a 1:1 text sent while a trusted-contact token of the peer is stored carries it in a `<tctoken>` child
/// (upstream's own rule), and the child's logs, Kourier's and upstream's, show no run of token characters and none of
/// the fixture's values (the token is random per run in production, so the grep can only read its shape).
async fn a_stored_token_goes_out_with_the_text() {
    let (dir, device) = account_dir().await;
    let now = i64::try_from(now_secs()).expect("time");
    raw(dir.path())
        .execute(
            "INSERT INTO tc_tokens(jid, token, token_timestamp, sender_timestamp) VALUES (?1, ?2, ?3, NULL)",
            kourier_db::rusqlite::params![peer::PEER_LID, STORED_TOKEN.as_slice(), now],
        )
        .expect("the peer's stored token");
    let rig = Rig::start(dir.path(), server_of(&Script::acked())).await;
    let mut peer = Peer::new(&device, None).await;
    rig.peer_writes_first(&mut peer).await;
    assert!(
        matches!(
            rig.transmit(&text_request(TOKEN_TEXT_ID)).await,
            TransmitOutcome::Sent { .. }
        ),
        "the text is not answered Sent"
    );
    let frames = rig.message_frames(TOKEN_TEXT_ID);
    assert_eq!(frames.len(), 1, "one frame for one text");
    let token = frames[0]
        .get_optional_child("tctoken")
        .expect("the frame carries the stored token");
    assert!(
        matches!(&token.content, Some(wacore_binary::NodeContent::Bytes(b)) if b.as_slice() == STORED_TOKEN),
        "the frame carries another token than the stored one"
    );
    rig.finish().await;
}

/// The case the child runs (set by the parent).
const CASE: &str = "WA_SEND_WIRE_CASE";

/// One parent per case; nothing the child printed holds a secret of the fixture (D10).
fn parent(case: &str) {
    let output = support::offline::run_child_output(module_path!(), "send_child", &[(CASE, case)]);
    for secret in send_secrets() {
        assert!(
            !output.contains(secret),
            "the child printed one of the fixture's secrets"
        );
    }
}

#[test]
fn the_stanza_id_is_the_cores_local_for_every_kind_over_the_wire() {
    parent("kinds");
}

#[test]
fn the_peer_reads_the_text_and_our_phone_gets_its_copy_over_the_wire() {
    parent("copy");
}

#[test]
fn an_ack_is_sent_with_the_servers_time_over_the_wire() {
    parent("ack");
}

#[test]
fn a_463_fails_for_good_and_a_resend_sends_no_frame_over_the_wire() {
    parent("refused");
}

#[test]
fn a_475_fails_for_good_and_a_resend_sends_no_frame_over_the_wire() {
    parent("refused475");
}

#[test]
fn a_429_is_not_answered_by_a_second_frame_over_the_wire() {
    parent("rate");
}

#[test]
fn a_read_goes_out_as_read_or_read_self_by_the_phones_setting_over_the_wire() {
    parent("reads");
}

#[test]
fn a_first_read_waits_for_the_phones_setting_over_the_wire() {
    parent("privacy");
}

#[test]
fn a_group_receipt_waits_for_a_group_info_that_fails_over_the_wire() {
    parent("group_info");
}

#[test]
fn a_migrated_account_writes_a_number_chat_as_its_lid_over_the_wire() {
    parent("migrated");
}

#[test]
fn a_frame_in_the_air_is_written_once_and_its_ack_answers_every_transmit_over_the_wire() {
    parent("in_air");
}

#[test]
fn a_lost_ack_is_transient_and_the_resend_uses_the_same_id_over_the_wire() {
    parent("lost");
}

/// In the slow set (`xtask/src/slow.rs`): it waits the real 75 s.
#[test]
fn a_silent_ack_ends_the_connection_and_the_resend_uses_the_same_id_over_the_wire() {
    parent("silent");
}

#[test]
fn a_group_text_is_one_stanza_with_sender_keys_over_the_wire() {
    parent("group");
}

#[test]
fn a_typing_state_goes_only_into_a_chat_the_phone_started_over_the_wire() {
    parent("typing");
}

#[test]
fn a_stored_token_goes_out_with_the_text_over_the_wire() {
    parent("token");
}

/// The child (a no-op as an ordinary test): every log line is recorded, and none may hold a secret.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_child() {
    if !support::offline::in_child() {
        return;
    }
    let log = Capture::default();
    log.install();
    let case = std::env::var(CASE).expect("the parent names the case");
    eprintln!("case {case}");
    match case.as_str() {
        "kinds" => the_stanza_id_is_the_cores_local_for_every_kind().await,
        "copy" => the_peer_reads_the_text_and_our_phone_gets_its_copy().await,
        "ack" => an_ack_is_sent_with_the_servers_time().await,
        "refused" => a_463_fails_for_good_and_a_resend_sends_no_frame().await,
        "refused475" => a_475_fails_for_good_and_a_resend_sends_no_frame().await,
        "rate" => a_429_is_not_answered_by_a_second_frame().await,
        "reads" => a_read_goes_out_as_read_or_read_self_by_the_phones_setting().await,
        "privacy" => a_first_read_waits_for_the_phones_setting().await,
        "group_info" => a_group_receipt_waits_for_a_group_info_that_fails().await,
        "migrated" => a_migrated_account_writes_a_number_chat_as_its_lid().await,
        "in_air" => a_frame_in_the_air_is_written_once_and_its_ack_answers_every_transmit().await,
        "lost" => a_lost_ack_is_transient_and_the_resend_uses_the_same_id().await,
        "group" => a_group_text_is_one_stanza_with_sender_keys().await,
        "typing" => a_typing_state_goes_only_into_a_chat_the_phone_started().await,
        "token" => a_stored_token_goes_out_with_the_text().await,
        "silent" => a_silent_ack_ends_the_connection_and_the_resend_uses_the_same_id().await,
        other => panic!("unknown case {other}"),
    }
    assert!(
        log.own_lines() >= 1,
        "the recorder saw no line of the crate"
    );
    assert!(
        log.upstream_records() >= 1,
        "the `log` bridge heard nothing"
    );
    assert_eq!(
        log.leaks(&send_secrets()),
        Vec::<String>::new(),
        "a log line holds a secret of the fixture (the targets are named)"
    );
    // Keys and tokens are random per run: only their SHAPE can be read (fix F17).
    assert_eq!(
        log.token_shaped(),
        Vec::<String>::new(),
        "a line of the crate holds a run of token characters (the targets are named)"
    );
}
