// Own code after oxidezap/whatsapp-rust tests/handshake_integration.rs and the tests of src/pair.rs at rev
// 50a723d46fee6216f20d4daba9cd834a39261743 (MIT; the in-process responder, the zero-signed certificate
// chain and the phone's signed pair-success are rebuilt from the public API of `wacore`, re-read on
// every pin bump). REUSE.toml annotates this directory.
//
//! THE SCRIPTED SERVER of the offline wire tests (W2 chunk C5, lane decision L3): ONE fixture, shared
//! by W2a and W3a. It is a WhatsApp chat server that lives inside the test process: a Noise XX
//! responder behind a [`TransportFactory`], so a REAL client (built by Kourier's own `build_bot`,
//! with Kourier's own props, event handler and lease) runs its whole connect against it with no
//! socket at all, and the test then speaks to the client as the server would.
//!
//! What it is not. It holds no WhatsApp key: the certificate chain it serves is zero-signed, so the
//! client must run under the noise-certificate bypass; that bypass, and a fixed WhatsApp Web version
//! (so no HTTP fetch of the version file), are the ONE builder step [`hook`] hands to
//! `NetParts::with_builder_hook`. This file is the only file of the crate that may name the bypass
//! or that hook (the source check in `tests/builder.rs`). Nothing here can reach a network: the
//! transport factory is a pipe into this process, the HTTP client refuses and counts, and the tests
//! that use it still run in an `unshare -rn` child (see `support::offline`).
//!
//! The server side of the framing comes from the public API: after the handshake the server's
//! [`NoiseHandshake::finish`] gives the same two ciphers the client has, with the roles swapped
//! (the client's WRITE cipher decrypts what the client sends). Frames are the 3-byte length prefix
//! of `wacore::framing`, a counter as the nonce, and the marshalled node as the plaintext.
//!
//! W3a (C8) widened it for the receive path: [`Wire::answering`] makes the server answer like a quiet
//! WhatsApp (an error for every IQ, a result for `active` and then the end of the offline delivery),
//! [`success`] and [`offline_end`] build the nodes a test injects by hand, [`Seen`] keeps every
//! attribute and child tag of a frame, and [`Wire::log_to`] writes one line per frame, fsynced, so
//! that a killed child leaves what had left the client.
//!
//! W5a (chunk 2a) widened it for the send path: [`Wire::on_iq`] answers the IQs of one namespace by a
//! test's rule (ready-made rules: [`usync`], [`privacy`], [`reach_out`], [`group_info`], [`prekeys`];
//! what no rule answers still gets the 503, except the keepalive ping `w:p`, which gets a result), an ACK POLICY answers every `<message>` frame the client
//! writes ([`Ack`]: the server's time, a nack code, nothing, or the connection's end; [`Wire::ack_messages`],
//! [`Wire::ack_next`], [`Wire::ack_id`], a late [`Wire::ack_now`]), and [`Wire::frame_nodes`] keeps every
//! frame whole (children and `<enc>` bytes) for the tests that read what was sent. Chunk 2b:
//! [`parts_handing_client`] hands the actor's client to the test ([`ClientSlot`], [`hand_over`]), so the
//! rig's own test calls upstream's send directly and the peer (`support::peer`) decrypts the frame.
//! W5b (chunk B2): [`media_conn`] answers the media-connection IQ, and
//! [`parts_handing_client_with_http`] gives the clients the CDN fake of `support::cdn` as their HTTP
//! client instead of [`NoVersionFetch`] (the version stays fixed by [`hook`], so nothing else asks it).
//! W6a2 (chunk B1): groups and the blocklist as the server writes them: [`GroupSpec`] (a `<group>`, whole or as the
//! overview), [`BatchEntry`] and [`batch_answer`] / [`group_batch`] (the batch info answer: whole, truncated, refused),
//! [`participating`], [`blocklist`], and the `w:gp2` notifications [`Gp2`], [`gp2_people`], [`gp2_subject`],
//! [`gp2_description`], [`groups_dirty`].
//!
//! Everything the fixture records is a SYNTHETIC fact of the test (a tag, an id, a count); nothing
//! real passes through it, and the log file's lines carry no JID.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use kourier_whatsapp::NetParts;
use wacore::framing::{FrameDecoder, encode_frame};
use wacore::handshake::{NoiseCipher, NoiseHandshake};
use wacore::libsignal::protocol::{KeyPair, PrivateKey};
use wacore::net::{
    HttpClient, HttpRequest, HttpResponse, Transport, TransportEvent, TransportFactory,
};
use wacore_binary::builder::NodeBuilder;
use wacore_binary::consts::{NOISE_PATTERN_XX, WA_CONN_HEADER};
use wacore_binary::{Node, OwnedNodeRef, marshal};
use waproto::buffa::{Message, MessageField};
use waproto::whatsapp as wa;
use waproto::whatsapp::cert_chain::noise_certificate;
use whatsapp_rust::handshake::NoiseCertPolicy;

/// How an edge routing pre-intro begins.
const EDGE_ROUTING_MAGIC: [u8; 4] = *b"ED\x00\x01";

/// The WhatsApp Web version the fixture's clients are told to use: no version file is fetched.
const FIXED_VERSION: (u32, u32, u32) = (2, 3000, 1);

/// One frame a client sent after the handshake: only what the tests assert on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    /// The root node's tag.
    pub tag: String,
    /// The root node's `xmlns` attribute, when it has one.
    pub xmlns: Option<String>,
    /// Every attribute of the root node, in frame order (the fixture's synthetic ids included).
    pub attrs: Vec<(String, String)>,
    /// The tags of the root node's children.
    pub children: Vec<String>,
    /// Every attribute of the root node's FIRST child, in frame order (W2c: the `error` child of an
    /// `<iq type="error">` carries `code` and `text`). Synthetic values only.
    pub first_child_attrs: Vec<(String, String)>,
    /// The `id` attribute of every grandchild: a receipt for several messages names the first in its
    /// own `id` and the others in `<list><item id=..>`.
    pub items: Vec<String>,
}

impl Seen {
    /// The value of attribute `key` of the root node.
    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// The value of attribute `key` of the root node's first child.
    pub fn first_child_attr(&self, key: &str) -> Option<&str> {
        self.first_child_attrs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// One line for the log file: tag, type, class, namespace, id (never a JID: the file may be read
    /// by a test that greps for the fixture's synthetic numbers).
    fn line(&self) -> String {
        let field = |v: Option<&str>| v.unwrap_or("-").to_owned();
        format!(
            "{} type={} class={} xmlns={} id={} items={}",
            self.tag,
            field(self.attr("type")),
            field(self.attr("class")),
            field(self.xmlns.as_deref()),
            field(self.attr("id")),
            if self.items.is_empty() {
                "-".to_owned()
            } else {
                self.items.join(",")
            },
        )
    }
}

/// What a rule of [`Wire::on_iq`] answers to one IQ.
#[derive(Debug, Clone)]
pub enum Answer {
    /// `<iq type="result">` with these children.
    Result(Vec<Node>),
    /// `<iq type="error"><error code=".."/></iq>`.
    Error(u16),
}

/// What the server answers to one `<message>` frame of the client (W5a: the ack policy).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ack {
    /// `<ack class="message" id from t>`: the server took it, at `t` (seconds).
    Ok { t: u64 },
    /// `<ack class="message" id from error=code>`: a nack.
    Error(u16),
    /// Nothing: the ack is lost (a test may send it late with [`Wire::ack_now`]).
    Silent,
    /// The connection ends right after the frame was read, with no ack.
    DropConnection,
}

/// A rule that answers IQs of one namespace: `None` leaves the IQ to the next rule (and in the end to
/// the 503 of [`Wire::answering`]).
type Rule = Arc<dyn Fn(&Node) -> Option<Answer> + Send + Sync>;

/// Which [`Ack`] the next `<message>` frame gets: one set for an id first, then the queue, then the
/// default.
struct AckRules {
    default: Ack,
    queue: VecDeque<Ack>,
    by_id: HashMap<String, Ack>,
}

/// The server, shared by every connection the clients of one test make. A clone is the SAME server (W5b: a
/// CDN hook of `support::cdn` reads its frames in the middle of a POST).
#[derive(Clone)]
pub struct Wire {
    inner: Arc<Inner>,
}

struct Inner {
    identity: KeyPair,
    cert_chain: Vec<u8>,
    conns: Mutex<Vec<Arc<Conn>>>,
    http_calls: AtomicUsize,
    /// Answer the client's `get`/`set` IQs (see [`Wire::answering`]).
    answer_iqs: AtomicBool,
    /// When answering: send `<ib><offline count="0"/></ib>` behind the `active` IQ's result.
    offline_end: AtomicBool,
    /// Every frame a client sends is also written here, one line, with an fsync ([`Wire::log_to`]).
    log: Mutex<Option<std::fs::File>>,
    /// The rules of [`Wire::on_iq`], by namespace, in the order they were given.
    rules: Mutex<Vec<(String, Rule)>>,
    /// How many IQs of each namespace a rule answered.
    answered: Mutex<HashMap<String, usize>>,
    /// The ack policy of `<message>` frames.
    acks: Mutex<AckRules>,
    /// Namespaces whose answers are held back ([`Wire::hold_answers`]): the replies waiting to be sent.
    held: Mutex<HashMap<String, Vec<Node>>>,
}

/// A server that has not been dialled yet.
pub fn server() -> Wire {
    // Fixed keys: a fixture needs no randomness and the run is the same every time.
    let private = PrivateKey::deserialize(&[0x2b; 32]).expect("a 32-byte key");
    let public = private.public_key().expect("its public key");
    let identity = KeyPair::new(public, private);
    let static_pub: [u8; 32] = identity
        .public_key
        .public_key_bytes()
        .try_into()
        .expect("X25519 public keys are 32 bytes");
    Wire {
        inner: Arc::new(Inner {
            identity,
            cert_chain: cert_chain_bytes(&static_pub),
            conns: Mutex::new(Vec::new()),
            http_calls: AtomicUsize::new(0),
            answer_iqs: AtomicBool::new(false),
            offline_end: AtomicBool::new(true),
            log: Mutex::new(None),
            rules: Mutex::new(Vec::new()),
            answered: Mutex::new(HashMap::new()),
            acks: Mutex::new(AckRules {
                default: Ack::Silent,
                queue: VecDeque::new(),
                by_id: HashMap::new(),
            }),
            held: Mutex::new(HashMap::new()),
        }),
    }
}

/// The parts every client of the test is made from: the scripted transport, an HTTP client that
/// refuses and counts, and the builder step. THE parts of a wire test (`WhatsAppNetwork::new(wire::parts(..))`).
pub fn parts(wire: &Wire) -> NetParts {
    let for_transport = wire.inner.clone();
    let for_http = wire.inner.clone();
    NetParts::from_makers(
        move || Factory {
            inner: for_transport.clone(),
        },
        move || NoVersionFetch {
            inner: for_http.clone(),
        },
    )
    .with_builder_hook(hook)
    .with_host_name(WIRE_HOST)
}

/// As [`parts`], with a history-sync capture hook of the TEST registered on every client's builder (W4
/// C1, the Wire probe of `tests/history_wire.rs`). The fixture's step runs after `build_bot`'s own
/// registrations and upstream keeps the last registration, so this hook replaces any capture hook the
/// builder module registers (W4 C5): a test of Kourier's own adapter uses [`parts`].
pub fn parts_capturing(
    wire: &Wire,
    capture: Arc<dyn whatsapp_rust::HistorySyncCaptureHook>,
) -> NetParts {
    let for_transport = wire.inner.clone();
    let for_http = wire.inner.clone();
    NetParts::from_makers(
        move || Factory {
            inner: for_transport.clone(),
        },
        move || NoVersionFetch {
            inner: for_http.clone(),
        },
    )
    .with_builder_hook(move |builder| {
        hook(builder).with_history_sync_capture_hook_arc(capture.clone())
    })
    .with_host_name(WIRE_HOST)
}

/// The enc type of [`hand_over`]: no WhatsApp type, so only the fixture's handler takes it.
pub const HAND_OVER_ENC: &str = "kourier-rig-hand-over";

/// Where [`parts_handing_client`] leaves the client of the actor under test (W5a 2b): a WEAK handle,
/// so the slot never keeps a client alive after the actor stopped it (W4's critic L7).
#[derive(Clone, Default)]
pub struct ClientSlot(Arc<Mutex<std::sync::Weak<whatsapp_rust::Client>>>);

impl ClientSlot {
    /// The client, once a [`hand_over`] stanza reached it (at most `within`); `None` if it never did.
    pub async fn wait(&self, within: Duration) -> Option<Arc<whatsapp_rust::Client>> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(client) = self.0.lock().expect("lock").upgrade() {
                return Some(client);
            }
            if Instant::now() > deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// The fixture's handler of [`HAND_OVER_ENC`]: upstream calls a custom enc handler with the client.
struct HandOver(ClientSlot);

#[async_trait]
impl whatsapp_rust::types::enc_handler::EncHandler for HandOver {
    async fn handle(
        &self,
        client: Arc<whatsapp_rust::Client>,
        _enc: &Node,
        _info: &whatsapp_rust::types::message::MessageInfo,
    ) -> anyhow::Result<()> {
        *self.0.0.lock().expect("lock") = Arc::downgrade(&client);
        Ok(())
    }
}

/// As [`parts`], and the client of the actor under test is handed to the TEST through `slot` (W5a 2b: a
/// send test calls upstream's client directly before Kourier's send path exists): the builder gets one
/// custom enc handler for [`HAND_OVER_ENC`], and the test pushes [`hand_over`] to fill the slot. Nothing
/// else of the client changes (upstream keeps every other registration of `build_bot`).
pub fn parts_handing_client(wire: &Wire, slot: &ClientSlot) -> NetParts {
    let for_transport = wire.inner.clone();
    let for_http = wire.inner.clone();
    let for_hook = slot.clone();
    NetParts::from_makers(
        move || Factory {
            inner: for_transport.clone(),
        },
        move || NoVersionFetch {
            inner: for_http.clone(),
        },
    )
    .with_builder_hook(move |builder| {
        hook(builder).with_enc_handler(HAND_OVER_ENC, HandOver(for_hook.clone()))
    })
    .with_host_name(WIRE_HOST)
}

/// As [`parts_handing_client`], with the clients' HTTP client made by `http` instead of the refusing
/// [`NoVersionFetch`] (W5b: the media tests answer the CDN's POSTs with a fake). The version stays
/// fixed by [`hook`], so a version fetch never reaches `http` either.
pub fn parts_handing_client_with_http<H: HttpClient + 'static>(
    wire: &Wire,
    slot: &ClientSlot,
    http: impl Fn() -> H + Send + Sync + 'static,
) -> NetParts {
    let for_transport = wire.inner.clone();
    let for_hook = slot.clone();
    NetParts::from_makers(
        move || Factory {
            inner: for_transport.clone(),
        },
        http,
    )
    .with_builder_hook(move |builder| {
        hook(builder).with_enc_handler(HAND_OVER_ENC, HandOver(for_hook.clone()))
    })
    .with_host_name(WIRE_HOST)
}

/// As [`parts_handing_client`], with a raw event handler of the TEST registered beside Kourier's own (W6a1
/// A1, the app-state probe of `tests/appstate_wire.rs`): upstream's bus hands every raw handler every event,
/// in the order they were registered, and the fixture's step runs after `build_bot`'s registrations, so
/// `observer` sees each event right after Kourier's one handler was handed it. It runs inline on the
/// dispatch path: it must not wait.
pub fn parts_observing(
    wire: &Wire,
    slot: &ClientSlot,
    observer: Arc<dyn wacore::types::events::EventHandler>,
) -> NetParts {
    let for_transport = wire.inner.clone();
    let for_http = wire.inner.clone();
    let for_hook = slot.clone();
    NetParts::from_makers(
        move || Factory {
            inner: for_transport.clone(),
        },
        move || NoVersionFetch {
            inner: for_http.clone(),
        },
    )
    .with_builder_hook(move |builder| {
        hook(builder)
            .with_enc_handler(HAND_OVER_ENC, HandOver(for_hook.clone()))
            .with_event_handler(Observed(observer.clone()))
    })
    .with_host_name(WIRE_HOST)
}

/// The test's observer as upstream's builder takes a raw handler (by value).
struct Observed(Arc<dyn wacore::types::events::EventHandler>);

impl wacore::types::events::EventHandler for Observed {
    fn handle_event(&self, event: Arc<wacore::types::events::Event>) {
        self.0.handle_event(event);
    }
}

/// The stanza that hands the client to a [`ClientSlot`]: a `<message>` from `from` (a device JID text)
/// whose one `<enc>` is of [`HAND_OVER_ENC`] (no bytes, nothing to decrypt).
pub fn hand_over(from: &str) -> Node {
    NodeBuilder::new("message")
        .attr("from", from)
        .attr("id", "3EB0AAAAFFFFBEEF")
        .attr("t", "1700000050")
        .attr("type", "text")
        .children([NodeBuilder::new("enc")
            .attr("v", "2")
            .attr("type", HAND_OVER_ENC)
            .build()])
        .build()
}

/// The host name the scripted fixture's application gives its parts (W2b: the label comes from the
/// application).
pub const WIRE_HOST: &str = "lab-pc";

/// The one step the fixture adds to the client's builder: the bypass for a zero-signed chain, and
/// the fixed version.
fn hook(builder: kourier_whatsapp::Builder) -> kourier_whatsapp::Builder {
    builder
        .with_noise_cert_policy(NoiseCertPolicy::DangerSkipCertChainVerify)
        .with_version(FIXED_VERSION)
}

impl Wire {
    /// From now on the server answers like a quiet WhatsApp: every `get`/`set` IQ of the client
    /// gets an error 503 (upstream's own fixture does the same), except `active`, which gets a
    /// result and, behind it, the end of the offline delivery (`<ib><offline count="0"/></ib>`)
    /// unless [`Wire::hold_offline_end`] says the test sends it itself. Without this call the
    /// server answers nothing (the pairing and verdict tests need no answers). W3a (C8).
    pub fn answering(&self) -> &Self {
        self.inner.answer_iqs.store(true, Ordering::SeqCst);
        self
    }

    /// While answering, do not send the end of the offline delivery after the `active` IQ: the
    /// test sends a preview, stanzas and `<ib><offline count=N/>` itself.
    pub fn hold_offline_end(&self) -> &Self {
        self.inner.offline_end.store(false, Ordering::SeqCst);
        self
    }

    /// Answers the client's IQs of namespace `xmlns` by `rule` (W5a): the first rule of the namespace
    /// that returns an [`Answer`] answers; when none does, the IQ gets the 503 of [`Wire::answering`].
    /// Turns answering on. The `active` IQ keeps its own answer.
    pub fn on_iq(
        &self,
        xmlns: &str,
        rule: impl Fn(&Node) -> Option<Answer> + Send + Sync + 'static,
    ) -> &Self {
        self.inner
            .rules
            .lock()
            .expect("lock")
            .push((xmlns.to_owned(), Arc::new(rule)));
        self.answering()
    }

    /// From now on the answers of the rules of namespace `xmlns` are not sent: the IQ counts as answered
    /// ([`Wire::answered`]) but its reply waits for [`Wire::release_answers`] (a server slow to answer).
    pub fn hold_answers(&self, xmlns: &str) -> &Self {
        self.inner
            .held
            .lock()
            .expect("lock")
            .insert(xmlns.to_owned(), Vec::new());
        self
    }

    /// Sends the replies [`Wire::hold_answers`] kept for `xmlns`, in order, and answers on time from now on.
    pub fn release_answers(&self, xmlns: &str) {
        let waiting = self
            .inner
            .held
            .lock()
            .expect("lock")
            .remove(xmlns)
            .unwrap_or_default();
        for node in waiting {
            self.send(&node);
        }
    }

    /// How many IQs of namespace `xmlns` a rule of [`Wire::on_iq`] answered.
    pub fn answered(&self, xmlns: &str) -> usize {
        self.inner
            .answered
            .lock()
            .expect("lock")
            .get(xmlns)
            .copied()
            .unwrap_or(0)
    }

    /// The answer every `<message>` frame gets unless [`Wire::ack_next`] or [`Wire::ack_id`] says
    /// otherwise (the default is [`Ack::Silent`]: a server that never acks, as before W5a).
    pub fn ack_messages(&self, ack: Ack) -> &Self {
        self.inner.acks.lock().expect("lock").default = ack;
        self
    }

    /// The answer of the NEXT `<message>` frame that has no answer of its own id; several calls queue
    /// up, one frame each, before the default.
    pub fn ack_next(&self, ack: Ack) -> &Self {
        self.inner.acks.lock().expect("lock").queue.push_back(ack);
        self
    }

    /// The answer of every `<message>` frame with this id (a resend of the same id gets it again).
    pub fn ack_id(&self, id: &str, ack: Ack) -> &Self {
        self.inner
            .acks
            .lock()
            .expect("lock")
            .by_id
            .insert(id.to_owned(), ack);
        self
    }

    /// The answer a `<message>` frame with this id gets now: its own, else the next of the queue
    /// (taken), else the default. What the fixture itself asks for each frame.
    pub fn take_ack(&self, id: &str) -> Ack {
        let mut acks = self.inner.acks.lock().expect("lock");
        if let Some(ack) = acks.by_id.get(id) {
            return ack.clone();
        }
        match acks.queue.pop_front() {
            Some(ack) => ack,
            None => acks.default.clone(),
        }
    }

    /// Sends an ack for the message `id` (written to `to`) on the newest open connection NOW: a late
    /// ack after [`Ack::Silent`]. Only [`Ack::Ok`] and [`Ack::Error`] make a node; the others do nothing.
    pub fn ack_now(&self, id: &str, to: &str, ack: &Ack) {
        if let Some(node) = message_ack(id, to, ack) {
            self.send(&node);
        }
    }

    /// Writes every frame a client sends to `path`, one line each, flushed to disk before the
    /// client's send call returns: a child that is killed leaves exactly the frames that had left
    /// it (the acknowledgements a kill test asks about). Each line is ONE write; a line a killed child left unfinished
    /// there is cut first (`kill_log::open_append`), and the readers take complete lines only (`kill_log::lines`).
    pub fn log_to(&self, path: &std::path::Path) {
        let file = super::kill_log::open_append(path);
        *self.inner.log.lock().expect("lock") = Some(file);
    }

    /// How many times a client dialled.
    pub fn dials(&self) -> usize {
        self.inner.conns.lock().expect("lock").len()
    }

    /// Whether the CLIENT closed the `n`th connection (its transport's `disconnect`), not the server (fix F11).
    pub fn client_closed(&self, n: usize) -> bool {
        self.inner
            .conns
            .lock()
            .expect("lock")
            .get(n)
            .is_some_and(|c| c.client_closed.load(Ordering::SeqCst))
    }

    /// HTTP requests the clients made (0 when the fixed version works).
    pub fn http_calls(&self) -> usize {
        self.inner.http_calls.load(Ordering::SeqCst)
    }

    /// How many handshakes completed (the server read a ClientFinish).
    pub fn opened(&self) -> usize {
        self.inner
            .conns
            .lock()
            .expect("lock")
            .iter()
            .filter(|c| c.client_payload.lock().expect("lock").is_some())
            .count()
    }

    /// The `ClientPayload` the `n`th connection's ClientFinish carried.
    pub fn client_payload(&self, n: usize) -> Option<wa::ClientPayload> {
        let conns = self.inner.conns.lock().expect("lock");
        let bytes = conns.get(n)?.client_payload.lock().expect("lock").clone()?;
        wa::ClientPayload::decode_from_slice(bytes.as_slice()).ok()
    }

    /// The edge routing bytes the `n`th connection began with (the device's stored routing info).
    pub fn edge_routing(&self, n: usize) -> Option<Vec<u8>> {
        let conns = self.inner.conns.lock().expect("lock");
        conns.get(n)?.edge_routing.lock().expect("lock").clone()
    }

    /// Every frame the clients sent after their handshakes, in arrival order per connection.
    pub fn frames(&self) -> Vec<Seen> {
        self.inner
            .conns
            .lock()
            .expect("lock")
            .iter()
            .flat_map(|c| c.seen.lock().expect("lock").clone())
            .collect()
    }

    /// Every frame the clients sent after their handshakes, WHOLE (every attribute, child and byte, the
    /// `<enc>` payloads included), in arrival order per connection. A frame that could not be read is
    /// missing here (it is in [`Wire::frames`] as `<unreadable>`).
    pub fn frame_nodes(&self) -> Vec<Node> {
        self.inner
            .conns
            .lock()
            .expect("lock")
            .iter()
            .flat_map(|c| c.nodes.lock().expect("lock").clone())
            .collect()
    }

    /// Waits (at most `within`) until `n` handshakes completed.
    pub async fn wait_opened(&self, n: usize, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while self.opened() < n {
            if Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        true
    }

    /// Waits (at most `within`) until the clients have sent `n` frames after their handshakes.
    pub async fn wait_frames(&self, n: usize, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while self.frames().len() < n {
            if Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        true
    }

    /// Sends `node` to the newest connection whose handshake completed, as the server would.
    /// Panics when there is none (a test sends only after `wait_opened`).
    pub fn send(&self, node: &Node) {
        let conns = self.inner.conns.lock().expect("lock");
        let conn = conns
            .iter()
            .rev()
            .find(|c| c.client_payload.lock().expect("lock").is_some())
            .expect("no connection is open: wait_opened first");
        conn.push(node);
    }

    /// Ends the newest connection from the server's side (the transport reports a disconnect).
    pub fn drop_connection(&self) {
        let conns = self.inner.conns.lock().expect("lock");
        if let Some(conn) = conns.last() {
            conn.closed.store(true, Ordering::SeqCst);
            let _ = conn.events.try_send(TransportEvent::Disconnected(
                wacore::net::DisconnectReason::StreamEnded,
            ));
        }
    }
}

/// The transport factory of the clients: every dial is answered by a new [`Conn`].
struct Factory {
    inner: Arc<Inner>,
}

#[async_trait]
impl TransportFactory for Factory {
    async fn create_transport(
        &self,
    ) -> Result<(Arc<dyn Transport>, async_channel::Receiver<TransportEvent>), anyhow::Error> {
        let (events, receiver) = async_channel::unbounded();
        let conn = Arc::new(Conn {
            server: self.inner.clone(),
            events: events.clone(),
            phase: Mutex::new(Phase::Hello),
            decoder: Mutex::new(FrameDecoder::new()),
            header_seen: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            client_closed: AtomicBool::new(false),
            edge_routing: Mutex::new(None),
            client_payload: Mutex::new(None),
            seen: Mutex::new(Vec::new()),
            nodes: Mutex::new(Vec::new()),
        });
        self.inner.conns.lock().expect("lock").push(conn.clone());
        let _ = events.try_send(TransportEvent::Connected);
        Ok((conn, receiver))
    }
}

/// The HTTP client of the clients: the version is fixed, so nothing should ask; it refuses and counts.
struct NoVersionFetch {
    inner: Arc<Inner>,
}

#[async_trait]
impl HttpClient for NoVersionFetch {
    async fn execute(&self, _request: HttpRequest) -> anyhow::Result<HttpResponse> {
        self.inner.http_calls.fetch_add(1, Ordering::SeqCst);
        Err(anyhow::anyhow!("offline test: the request is refused"))
    }
}

/// Where a connection stands.
enum Phase {
    /// Waiting for the ClientHello.
    Hello,
    /// ServerHello sent; waiting for the ClientFinish.
    Finish {
        noise: Box<NoiseHandshake>,
        ephemeral: KeyPair,
    },
    /// The handshake is over; frames are encrypted with a counter as the nonce.
    Open {
        /// Decrypts what the client sends.
        read: NoiseCipher,
        /// Encrypts what the server sends.
        write: NoiseCipher,
        received: u32,
        sent: u32,
    },
    /// A frame could not be understood: the connection is dead.
    Dead,
}

/// One connection: the server's end of the pipe.
struct Conn {
    server: Arc<Inner>,
    events: async_channel::Sender<TransportEvent>,
    phase: Mutex<Phase>,
    decoder: Mutex<FrameDecoder>,
    header_seen: AtomicBool,
    closed: AtomicBool,
    /// The client itself closed the transport (`Transport::disconnect`), as against the server ending it.
    client_closed: AtomicBool,
    /// The edge routing bytes the client began the connection with, when it did.
    edge_routing: Mutex<Option<Vec<u8>>>,
    /// The bytes of the ClientPayload, once the ClientFinish was read.
    client_payload: Mutex<Option<Vec<u8>>>,
    seen: Mutex<Vec<Seen>>,
    /// The frames whole ([`Wire::frame_nodes`]).
    nodes: Mutex<Vec<Node>>,
}

#[async_trait]
impl Transport for Conn {
    async fn send(&self, data: Bytes) -> anyhow::Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(anyhow::anyhow!("offline test: the connection is closed"));
        }
        let mut data = data.to_vec();
        // The very first bytes of a connection are WhatsApp's connection header, behind the edge
        // routing pre-intro ("ED", 0, 1, a 3-byte length, the routing bytes) when the device holds some.
        if !self.header_seen.swap(true, Ordering::SeqCst) {
            if data.starts_with(&EDGE_ROUTING_MAGIC) && data.len() >= 7 {
                let len = (usize::from(data[4]) << 16)
                    | (usize::from(data[5]) << 8)
                    | usize::from(data[6]);
                if data.len() >= 7 + len {
                    *self.edge_routing.lock().expect("lock") = Some(data[7..7 + len].to_vec());
                    data.drain(..7 + len);
                }
            }
            if data.starts_with(&WA_CONN_HEADER) {
                data.drain(..WA_CONN_HEADER.len());
            }
        }
        let mut decoder = self.decoder.lock().expect("lock");
        decoder.feed(&data);
        while let Some(frame) = decoder.decode_frame() {
            self.on_frame(frame.to_vec());
        }
        Ok(())
    }

    async fn disconnect(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.client_closed.store(true, Ordering::SeqCst);
    }
}

impl Conn {
    fn on_frame(&self, frame: Vec<u8>) {
        let mut phase = self.phase.lock().expect("lock");
        match std::mem::replace(&mut *phase, Phase::Dead) {
            Phase::Hello => match self.serve_hello(&frame) {
                Some(next) => *phase = next,
                None => eprintln!(
                    "wire: the ClientHello could not be served ({} bytes)",
                    frame.len()
                ),
            },
            Phase::Finish { noise, ephemeral } => {
                match self.serve_finish(*noise, &ephemeral, &frame) {
                    Some(next) => *phase = next,
                    None => eprintln!(
                        "wire: the ClientFinish could not be read ({} bytes)",
                        frame.len()
                    ),
                }
            }
            Phase::Open {
                read,
                write,
                received,
                sent,
            } => {
                let mut plain = frame;
                if read
                    .decrypt_in_place_with_counter(received, &mut plain)
                    .is_ok()
                {
                    let (seen, node, replies, end) = self.answer(&plain);
                    self.record(seen, node);
                    *phase = Phase::Open {
                        read,
                        write,
                        received: received + 1,
                        sent,
                    };
                    // The replies are pushed with the phase unlocked (`push` locks it).
                    drop(phase);
                    for reply in replies {
                        self.push(&reply);
                    }
                    if end {
                        // `Ack::DropConnection`: the server ends the connection after the frame.
                        self.closed.store(true, Ordering::SeqCst);
                        let _ = self.events.try_send(TransportEvent::Disconnected(
                            wacore::net::DisconnectReason::StreamEnded,
                        ));
                    }
                }
            }
            Phase::Dead => {}
        }
    }

    /// Keeps a frame the client sent: in memory, and in the log file with an fsync.
    fn record(&self, seen: Seen, node: Option<Node>) {
        if let Some(file) = self.server.log.lock().expect("lock").as_mut() {
            let line = format!("{}\n", seen.line());
            file.write_all(line.as_bytes())
                .and_then(|()| file.sync_data())
                .expect("the wire log is written");
        }
        self.seen.lock().expect("lock").push(seen);
        if let Some(node) = node {
            self.nodes.lock().expect("lock").push(node);
        }
    }

    /// What the server answers to one frame: the IQ answers (nothing unless [`Wire::answering`]) and
    /// the ack policy's answer to a `<message>`; `true` last when the connection must end after it.
    fn answer(&self, plain: &[u8]) -> (Seen, Option<Node>, Vec<Node>, bool) {
        let (seen, active, node) = look(plain);
        let mut replies = Vec::new();
        let mut end = false;
        let is_request = matches!(seen.attr("type"), Some("get" | "set"));
        if self.server.answer_iqs.load(Ordering::SeqCst)
            && seen.tag == "iq"
            && is_request
            && let Some(id) = seen.attr("id")
        {
            let reply = NodeBuilder::new("iq")
                .attr("id", id)
                .attr("from", "s.whatsapp.net");
            let ruled = if active {
                None
            } else {
                node.as_ref()
                    .and_then(|n| self.ruled(seen.xmlns.as_deref(), n))
            };
            if active {
                replies.push(reply.attr("type", "result").build());
                if self.server.offline_end.load(Ordering::SeqCst) {
                    replies.push(offline_end(0));
                }
            } else {
                let code = match ruled {
                    Some(Answer::Result(children)) => {
                        let reply = reply.attr("type", "result").children(children).build();
                        let kept = seen.xmlns.as_deref().is_some_and(|xmlns| {
                            self.server
                                .held
                                .lock()
                                .expect("lock")
                                .get_mut(xmlns)
                                .map(|waiting| waiting.push(reply.clone()))
                                .is_some()
                        });
                        if !kept {
                            replies.push(reply);
                        }
                        return (seen, node, replies, end);
                    }
                    Some(Answer::Error(code)) => code,
                    // A real server answers upstream's keepalive ping (fix F11). With the 503 below upstream
                    // counts three failed pings (every 15-30 s on an idle link) and reconnects BY ITSELF, with
                    // no `Disconnected`: a test that idles longer than about 45 s then sees a second connection
                    // whether or not the code under test ended the first one (the 75 s ack case could not see
                    // `drop_connection`). A rule of `on_iq("w:p", ..)` still overrides this.
                    None if seen.xmlns.as_deref() == Some("w:p") => {
                        replies.push(reply.attr("type", "result").build());
                        return (seen, node, replies, end);
                    }
                    None => 503,
                };
                replies.push(
                    reply
                        .attr("type", "error")
                        .children([NodeBuilder::new("error")
                            .attr("code", code.to_string())
                            .attr("text", "unsupported fixture IQ")
                            .build()])
                        .build(),
                );
            }
        }
        if seen.tag == "message"
            && let Some(id) = seen.attr("id")
        {
            let ack = Wire {
                inner: self.server.clone(),
            }
            .take_ack(id);
            if ack == Ack::DropConnection {
                end = true;
            } else if let Some(node) = message_ack(id, seen.attr("to").unwrap_or("-"), &ack) {
                replies.push(node);
            }
        }
        (seen, node, replies, end)
    }

    /// The answer of the first rule of `xmlns` that answers `node`, counted.
    fn ruled(&self, xmlns: Option<&str>, node: &Node) -> Option<Answer> {
        let xmlns = xmlns?;
        let rules: Vec<Rule> = self
            .server
            .rules
            .lock()
            .expect("lock")
            .iter()
            .filter(|(ns, _)| ns == xmlns)
            .map(|(_, rule)| rule.clone())
            .collect();
        let answer = rules.iter().find_map(|rule| rule(node))?;
        *self
            .server
            .answered
            .lock()
            .expect("lock")
            .entry(xmlns.to_owned())
            .or_insert(0) += 1;
        Some(answer)
    }

    /// ClientHello in, ServerHello out (the responder half of Noise XX).
    fn serve_hello(&self, frame: &[u8]) -> Option<Phase> {
        let message = wa::HandshakeMessage::decode_from_slice(frame).ok()?;
        let hello = message.client_hello.into_option()?;
        let client_ephemeral: [u8; 32] = hello.ephemeral?.try_into().ok()?;

        let mut noise = NoiseHandshake::new(NOISE_PATTERN_XX, &WA_CONN_HEADER).ok()?;
        noise.authenticate(&client_ephemeral);
        let ephemeral = ephemeral_pair();
        let ephemeral_pub: [u8; 32] = ephemeral.public_key.public_key_bytes().try_into().ok()?;
        noise.authenticate(&ephemeral_pub);
        noise
            .mix_shared_secret(ephemeral.private_key.serialize(), &client_ephemeral)
            .ok()?;
        let static_pub = self.server.identity.public_key.public_key_bytes();
        let encrypted_static = noise.encrypt(static_pub).ok()?;
        noise
            .mix_shared_secret(
                self.server.identity.private_key.serialize(),
                &client_ephemeral,
            )
            .ok()?;
        let encrypted_chain = noise.encrypt(&self.server.cert_chain).ok()?;

        let mut hello = wa::handshake_message::ServerHello::default();
        hello.ephemeral = Some(ephemeral_pub.to_vec());
        hello.r#static = Some(encrypted_static);
        hello.payload = Some(encrypted_chain);
        let mut reply = wa::HandshakeMessage::default();
        reply.server_hello = MessageField::some(hello);
        let framed = encode_frame(&reply.encode_to_vec(), None).ok()?;
        let _ = self
            .events
            .try_send(TransportEvent::DataReceived(framed.into()));
        Some(Phase::Finish {
            noise: Box::new(noise),
            ephemeral,
        })
    }

    /// ClientFinish in: the client's static key and its payload; the ciphers come out.
    fn serve_finish(
        &self,
        mut noise: NoiseHandshake,
        ephemeral: &KeyPair,
        frame: &[u8],
    ) -> Option<Phase> {
        let message = wa::HandshakeMessage::decode_from_slice(frame).ok()?;
        let finish = message.client_finish.into_option()?;
        let client_static = noise.decrypt(&finish.r#static?).ok()?;
        noise
            .mix_shared_secret(ephemeral.private_key.serialize(), &client_static)
            .ok()?;
        let payload = noise.decrypt(&finish.payload?).ok()?;
        // The client's WRITE cipher is what decrypts its frames; the server writes with the client's READ.
        let (client_write, client_read) = noise.finish().ok()?;
        *self.client_payload.lock().expect("lock") = Some(payload);
        Some(Phase::Open {
            read: client_write,
            write: client_read,
            received: 0,
            sent: 0,
        })
    }

    /// Sends one node to the client (only in the open phase).
    fn push(&self, node: &Node) {
        let mut phase = self.phase.lock().expect("lock");
        let Phase::Open { write, sent, .. } = &mut *phase else {
            panic!("the connection is not open");
        };
        let plain = marshal(node).expect("a node marshals");
        let cipher = write
            .encrypt_with_counter(*sent, &plain)
            .expect("the frame encrypts");
        *sent += 1;
        let framed = encode_frame(&cipher, None).expect("the frame fits");
        let _ = self
            .events
            .try_send(TransportEvent::DataReceived(framed.into()));
    }
}

/// The root of one decrypted frame (the format byte is still on it), whether it is the `active` IQ
/// that ends a connection's login, and the whole node.
fn look(plain: &[u8]) -> (Seen, bool, Option<Node>) {
    let unreadable = || Seen {
        tag: "<unreadable>".to_owned(),
        xmlns: None,
        attrs: Vec::new(),
        children: Vec::new(),
        first_child_attrs: Vec::new(),
        items: Vec::new(),
    };
    let Ok(unpacked) = wacore_binary::util::unpack(plain) else {
        return (unreadable(), false, None);
    };
    let Ok(owned) = OwnedNodeRef::new(unpacked.into_owned()) else {
        return (unreadable(), false, None);
    };
    let node = owned.get();
    let attrs: Vec<(String, String)> = node
        .attrs_iter()
        .map(|(k, v)| (k.to_string(), v.as_str().into_owned()))
        .collect();
    let children: Vec<String> = node
        .children()
        .map(|c| c.iter().map(|n| n.tag.to_string()).collect())
        .unwrap_or_default();
    let first_child_attrs: Vec<(String, String)> = node
        .children()
        .and_then(|c| c.iter().next())
        .map(|first| {
            first
                .attrs_iter()
                .map(|(k, v)| (k.to_string(), v.as_str().into_owned()))
                .collect()
        })
        .unwrap_or_default();
    let items: Vec<String> = node
        .children()
        .into_iter()
        .flatten()
        .flat_map(|c| c.children().into_iter().flatten())
        .filter_map(|g| g.get_attr("id").map(|v| v.as_str().into_owned()))
        .collect();
    let xmlns = attrs
        .iter()
        .find(|(k, _)| k == "xmlns")
        .map(|(_, v)| v.clone());
    let active = children.iter().any(|c| c == "active");
    (
        Seen {
            tag: node.tag.to_string(),
            xmlns,
            attrs,
            children,
            first_child_attrs,
            items,
        },
        active,
        Some(node.to_owned()),
    )
}

/// `<ib><offline count=N/></ib>`: the server says the offline delivery is over (it sends this
/// after the `active` IQ even when it holds nothing).
pub fn offline_end(count: u32) -> Node {
    NodeBuilder::new("ib")
        .children([NodeBuilder::new("offline")
            .attr("count", count.to_string())
            .build()])
        .build()
}

/// The server's ack of the `<message>` `id` that was written to `to`: `<ack class="message" id from t>`
/// for [`Ack::Ok`], `<ack class="message" id from error>` for [`Ack::Error`], nothing for the others
/// (upstream matches an ack by its id; `from` is the chat, as the real server writes it).
pub fn message_ack(id: &str, to: &str, ack: &Ack) -> Option<Node> {
    let node = NodeBuilder::new("ack")
        .attr("class", "message")
        .attr("id", id)
        .attr("from", to);
    match ack {
        Ack::Ok { t } => Some(node.attr("t", t.to_string()).build()),
        Ack::Error(code) => Some(node.attr("error", code.to_string()).build()),
        Ack::Silent | Ack::DropConnection => None,
    }
}

/// The value of attribute `key` of `node`, as text.
fn attr_of(node: &Node, key: &str) -> Option<String> {
    node.attrs.get(key).map(|v| v.as_str().into_owned())
}

/// The user part of a JID text (`<user>[:<device>]@<server>` -> `<user>`).
fn user_of(jid: &str) -> &str {
    let bare = jid.split('@').next().unwrap_or(jid);
    bare.split(':').next().unwrap_or(bare)
}

/// A rule for `usync` (W5a): every `<user jid>` of the request's `<list>` whose user part `devices` names
/// gets its device list when the query asks for `<devices>`, and its LID when the query asks for
/// `<lid>` and `lids` names it (a phone-number user -> its LID's user part). A user the rule knows
/// nothing of is left out of the answer (upstream then keeps what it had). Answers only a `<usync>`
/// request.
pub fn usync(
    devices: Vec<(&'static str, Vec<u16>)>,
    lids: Vec<(&'static str, &'static str)>,
) -> impl Fn(&Node) -> Option<Answer> + Send + Sync + 'static {
    move |request| {
        let usync = request.get_optional_child("usync")?;
        let query = usync.get_optional_child("query")?;
        let wants_devices = query.get_optional_child("devices").is_some();
        let wants_lid = query.get_optional_child("lid").is_some();
        let users = usync
            .get_optional_child("list")
            .map(|list| {
                list.get_children_by_tag("user")
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut answered = Vec::new();
        for user in users {
            let Some(jid) = attr_of(&user, "jid") else {
                continue;
            };
            let name = user_of(&jid);
            let mut children = Vec::new();
            if wants_lid && let Some((_, lid)) = lids.iter().find(|(pn, _)| *pn == name) {
                children.push(
                    NodeBuilder::new("lid")
                        .attr("val", format!("{lid}@lid"))
                        .build(),
                );
            }
            if wants_devices && let Some((_, ids)) = devices.iter().find(|(u, _)| *u == name) {
                children.push(
                    NodeBuilder::new("devices")
                        .attr("version", "2")
                        .children([
                            NodeBuilder::new("device-list")
                                .children(ids.iter().map(|id| {
                                    NodeBuilder::new("device")
                                        .attr("id", id.to_string())
                                        .build()
                                }))
                                .build(),
                            key_index_list(ids),
                        ])
                        .build(),
                );
            }
            if !children.is_empty() {
                answered.push(
                    NodeBuilder::new("user")
                        .attr("jid", jid.clone())
                        .children(children)
                        .build(),
                );
            }
        }
        Some(Answer::Result(vec![
            NodeBuilder::new("usync")
                .children([NodeBuilder::new("list").children(answered).build()])
                .build(),
        ]))
    }
}

/// The `<key-index-list>` of a device list (upstream keeps a companion only when the answer has one):
/// an UNSIGNED `ADVSignedKeyIndexList` naming every device, as upstream's own usync fixture builds it
/// (`wacore/src/iq/usync.rs` `build_test_key_index_list_node` at the pin).
fn key_index_list(ids: &[u16]) -> Node {
    let mut list = wa::ADVKeyIndexList::default();
    list.raw_id = Some(1);
    list.timestamp = Some(1000);
    list.valid_indexes = ids.iter().map(|id| u32::from(*id)).collect();
    list.current_index = Some(list.valid_indexes.iter().copied().max().unwrap_or(0));
    let mut signed = wa::ADVSignedKeyIndexList::default();
    signed.details = Some(list.encode_to_vec());
    NodeBuilder::new("key-index-list")
        .attr("ts", "1000")
        .bytes(signed.encode_to_vec())
        .build()
}

/// A rule for `privacy` (W5a): the settings `get` is answered with every category `all` but
/// `readreceipts`, which says `read_receipts` (`"all"` or `"none"`: upstream turns a DM read into
/// `read-self` for `"none"`); the token issue (`set` with `<tokens>`) is answered with no token back.
pub fn privacy(read_receipts: &'static str) -> impl Fn(&Node) -> Option<Answer> + Send + Sync {
    move |request| {
        if request.get_optional_child("tokens").is_some() {
            return Some(Answer::Result(vec![NodeBuilder::new("tokens").build()]));
        }
        request.get_optional_child("privacy")?;
        let category = |name: &'static str, value: &'static str| {
            NodeBuilder::new("category")
                .attr("name", name)
                .attr("value", value)
                .build()
        };
        Some(Answer::Result(vec![
            NodeBuilder::new("privacy")
                .children([
                    category("last", "all"),
                    category("online", "all"),
                    category("profile", "all"),
                    category("status", "all"),
                    category("groupadd", "all"),
                    category("readreceipts", read_receipts),
                    category("calladd", "all"),
                ])
                .build(),
        ]))
    }
}

/// The persisted-query id of upstream's reach-out lock query (`fetch_reachout_timelock::DOC_ID`).
const REACH_OUT_DOC: &str = wacore::iq::mex_operations::fetch_reachout_timelock::DOC_ID;

/// A rule for `w:mex` (W5a, critic H3): the reach-out lock query is answered with no lock (`None`) or
/// a lock until `Some(t)` (seconds); any other MEX query is left to the 503.
pub fn reach_out(until_secs: Option<u64>) -> impl Fn(&Node) -> Option<Answer> + Send + Sync {
    move |request| {
        let query = request.get_optional_child("query")?;
        if attr_of(query, "query_id").as_deref() != Some(REACH_OUT_DOC) {
            return None;
        }
        let state = match until_secs {
            None => serde_json::json!({ "is_active": false }),
            Some(t) => serde_json::json!({
                "is_active": true,
                "time_enforcement_ends": t.to_string(),
                "enforcement_type": "DEFAULT",
            }),
        };
        let body = serde_json::json!({
            "data": { "xwa2_fetch_account_reachout_timelock": state }
        });
        Some(Answer::Result(vec![
            NodeBuilder::new("result")
                .bytes(body.to_string().into_bytes())
                .build(),
        ]))
    }
}

/// A rule for `w:m` (W5b): the media-connection `set` (`<media_conn/>`, what upstream asks before an
/// upload) is answered with `auth`, the `ttl` in seconds (0 = upstream keeps nothing and asks again on
/// the next upload) and one `<host hostname>` per `hosts`, in order. How many it answered is
/// [`Wire::answered`]`("w:m")` (a 401 costs one more, upstream's forced refetch). `auth` and the host
/// names are SYNTHETIC (`support::cdn`); the host names end in `.example.test`, which resolves nowhere.
pub fn media_conn(
    auth: &'static str,
    hosts: &[&'static str],
    ttl: u32,
) -> impl Fn(&Node) -> Option<Answer> + Send + Sync + 'static {
    let hosts = hosts.to_vec();
    move |request| {
        request.get_optional_child("media_conn")?;
        Some(Answer::Result(vec![
            NodeBuilder::new("media_conn")
                .attr("auth", auth)
                .attr("ttl", ttl.to_string())
                .attr("max_buckets", "12")
                .children(
                    hosts
                        .iter()
                        .map(|h| NodeBuilder::new("host").attr("hostname", *h).build()),
                )
                .build(),
        ]))
    }
}

/// A rule for `w:g2` (W5a): the group info `get` (`<query>`) addressed to the group `group_user`
/// (`<group_user>@g.us`) is answered with that group, its `addressing_mode` (`"pn"` or `"lid"`) and
/// its participants (full JID texts, with `type` `admin`/`superadmin` or none for a member).
pub fn group_info(
    group_user: &'static str,
    addressing_mode: &'static str,
    participants: Vec<(&'static str, Option<&'static str>)>,
) -> impl Fn(&Node) -> Option<Answer> + Send + Sync {
    move |request| {
        request.get_optional_child("query")?;
        let to = attr_of(request, "to")?;
        if user_of(&to) != group_user {
            return None;
        }
        let members = participants.iter().map(|(jid, kind)| {
            let node = NodeBuilder::new("participant").attr("jid", *jid);
            match kind {
                Some(kind) => node.attr("type", *kind).build(),
                None => node.build(),
            }
        });
        Some(Answer::Result(vec![
            NodeBuilder::new("group")
                .attr("id", group_user)
                .attr("subject", "Fixture group")
                .attr("creation", "1700000000")
                .attr("addressing_mode", addressing_mode)
                .children(members)
                .build(),
        ]))
    }
}

// ---- W6a2 B1: groups as the server writes them (`tests/groups_wire.rs`) ------------------------------
//
// The attribute and child names are the server's, as upstream's parser reads them at the pin
// (`wacore/src/iq/groups.rs` `GroupMetadataResponse::try_from_node_ref`, the batch's `BatchGetGroupInfoIq`, the
// overview's `GroupOverviewData`, `wacore/src/iq/blocklist.rs`, `wacore/src/stanza/groups.rs`) and whatsmeow's
// (`group.go` `parseGroupNode`, `parseGroupChange`) agree on. Built by hand, never with upstream's own encoders, so a
// test reads what upstream makes of the server's shape and not of its own. Every value is a test's synthetic one.

/// One member of a [`GroupSpec`]: `jid` and `phone_number` are full JID texts; `kind` is the `type` attribute
/// (`admin`, `superadmin`, or none for a member).
#[derive(Debug, Clone)]
pub struct Member {
    pub jid: String,
    pub phone_number: Option<String>,
    pub kind: Option<&'static str>,
}

/// A group's `<description id participant t><body>..</body></description>` (`t` in seconds).
#[derive(Debug, Clone)]
pub struct Topic {
    pub id: String,
    pub body: String,
    pub participant: Option<String>,
    pub t: u64,
}

/// One group as a `w:g2` answer carries it. Times are seconds (`s_t`, `creation`, the description's `t`); the
/// ephemeral pair is `(expiration seconds, trigger)`; `linked_parent` is the community's user part.
#[derive(Debug, Clone, Default)]
pub struct GroupSpec {
    /// The `<user>` of `<user>@g.us` (the `id` attribute, as the server writes it: no server part).
    pub user: String,
    pub subject: Option<String>,
    pub subject_time: Option<u64>,
    pub subject_owner: Option<String>,
    pub creation: Option<u64>,
    pub creator: Option<String>,
    /// `pn` or `lid`; empty = no attribute (upstream then reads `pn`).
    pub addressing_mode: &'static str,
    pub size: Option<u32>,
    pub participants: Vec<Member>,
    pub description: Option<Topic>,
    pub locked: bool,
    pub announcement: bool,
    pub ephemeral: Option<(u32, Option<u32>)>,
    /// `admin_add` or `all_member_add`, the child's text.
    pub member_add_mode: Option<&'static str>,
    /// A community parent (`<parent/>`).
    pub parent: bool,
    /// A community's subgroup: the parent's user part (`<linked_parent jid>`).
    pub linked_parent: Option<String>,
    pub default_sub_group: bool,
}

impl GroupSpec {
    /// The `<group>` node: whole (members, description, settings) or as the overview projection
    /// (`<participating>` without `<participants>`/`<description>`: id, subject, size, times and the hierarchy).
    pub fn node(&self, whole: bool) -> Node {
        let mut group = NodeBuilder::new("group").attr("id", self.user.as_str());
        let attrs: [(&str, Option<String>); 6] = [
            ("subject", self.subject.clone()),
            ("s_t", self.subject_time.map(|t| t.to_string())),
            ("s_o", self.subject_owner.clone().filter(|_| whole)),
            ("creation", self.creation.map(|t| t.to_string())),
            ("creator", self.creator.clone().filter(|_| whole)),
            ("size", self.size.map(|n| n.to_string())),
        ];
        for (key, value) in attrs {
            if let Some(value) = value {
                group = group.attr(key, value);
            }
        }
        if whole && !self.addressing_mode.is_empty() {
            group = group.attr("addressing_mode", self.addressing_mode);
        }
        let mut children = Vec::new();
        if whole {
            for m in &self.participants {
                let mut node = NodeBuilder::new("participant").attr("jid", m.jid.as_str());
                if let Some(pn) = &m.phone_number {
                    node = node.attr("phone_number", pn.as_str());
                }
                if let Some(kind) = m.kind {
                    node = node.attr("type", kind);
                }
                children.push(node.build());
            }
            if let Some(topic) = &self.description {
                let mut node = NodeBuilder::new("description")
                    .attr("id", topic.id.as_str())
                    .attr("t", topic.t.to_string());
                if let Some(by) = &topic.participant {
                    node = node.attr("participant", by.as_str());
                }
                children.push(
                    node.children([NodeBuilder::new("body")
                        .string_content(topic.body.as_str())
                        .build()])
                        .build(),
                );
            }
            if self.locked {
                children.push(NodeBuilder::new("locked").build());
            }
            if self.announcement {
                children.push(NodeBuilder::new("announcement").build());
            }
            if let Some((expiration, trigger)) = self.ephemeral {
                let mut node =
                    NodeBuilder::new("ephemeral").attr("expiration", expiration.to_string());
                if let Some(trigger) = trigger {
                    node = node.attr("trigger", trigger.to_string());
                }
                children.push(node.build());
            }
            if let Some(mode) = self.member_add_mode {
                children.push(
                    NodeBuilder::new("member_add_mode")
                        .string_content(mode)
                        .build(),
                );
            }
        }
        if self.parent {
            children.push(NodeBuilder::new("parent").build());
        }
        if let Some(parent) = &self.linked_parent {
            children.push(
                NodeBuilder::new("linked_parent")
                    .attr("jid", format!("{parent}@g.us"))
                    .build(),
            );
        }
        if self.default_sub_group {
            children.push(NodeBuilder::new("default_sub_group").build());
        }
        group.children(children).build()
    }
}

/// One `<group>` of a batch answer (`<groups>`): the whole group, a truncated one (`truncated="true" size`), or a
/// refusal (`error="403"` forbidden, `"404"` not found; another code is no shape the server documents).
#[derive(Debug, Clone)]
pub enum BatchEntry {
    Found(Box<GroupSpec>),
    Truncated { user: String, size: u32 },
    Refused { user: String, code: u16 },
}

impl BatchEntry {
    fn node(&self) -> Node {
        match self {
            BatchEntry::Found(spec) => spec.node(true),
            BatchEntry::Truncated { user, size } => NodeBuilder::new("group")
                .attr("id", user.as_str())
                .attr("truncated", "true")
                .attr("size", size.to_string())
                .build(),
            BatchEntry::Refused { user, code } => NodeBuilder::new("group")
                .attr("id", user.as_str())
                .attr("error", code.to_string())
                .build(),
        }
    }
}

/// Whether `request` is the batch group info `get` (`fetch_metadata_batch`): a `<query>` with `<group jid>`
/// children (the single group query is a bare `<query request="interactive"/>` addressed to the group).
pub fn is_group_batch(request: &Node) -> bool {
    request
        .get_optional_child("query")
        .is_some_and(|query| query.get_children_by_tag("group").next().is_some())
}

/// The users (`<user>` of `<user>@g.us`) a batch request asks, in its order.
pub fn batch_asked(request: &Node) -> Vec<String> {
    request
        .get_optional_child("query")
        .map(|query| {
            query
                .get_children_by_tag("group")
                .filter_map(|g| attr_of(g, "jid"))
                .map(|jid| user_of(&jid).to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// The answer to a batch request: `<groups>` with `entries` in this order (a group asked and not named here is
/// simply missing from the answer).
pub fn batch_answer(entries: &[BatchEntry]) -> Answer {
    Answer::Result(vec![
        NodeBuilder::new("groups")
            .children(entries.iter().map(BatchEntry::node))
            .build(),
    ])
}

/// A rule for `w:g2` (W6a2): the batch group info `get` is answered with `entries` ([`batch_answer`]); every
/// other `w:g2` request is left to the next rule.
pub fn group_batch(entries: Vec<BatchEntry>) -> impl Fn(&Node) -> Option<Answer> + Send + Sync {
    move |request| is_group_batch(request).then(|| batch_answer(&entries))
}

/// A rule for `w:g2` (W6a2): the participating `get` (`<participating>`) is answered with `groups` in
/// `<groups>`, whole when the request asks `<participants>` and as the overview projection otherwise
/// (`list_participating` asks neither).
pub fn participating(groups: Vec<GroupSpec>) -> impl Fn(&Node) -> Option<Answer> + Send + Sync {
    move |request| {
        let asked = request.get_optional_child("participating")?;
        let whole = asked.get_optional_child("participants").is_some();
        Some(Answer::Result(vec![
            NodeBuilder::new("groups")
                .children(groups.iter().map(|g| g.node(whole)))
                .build(),
        ]))
    }
}

/// A rule for `blocklist` (W6a2): the `get` (no children) is answered with `<list>` of `<item jid t>` (`t` in
/// seconds; an item with no `jid` is written without one, a malformed entry). A block or an unblock (`set`) is
/// left to the 503.
pub fn blocklist(
    entries: Vec<(Option<String>, Option<u64>)>,
) -> impl Fn(&Node) -> Option<Answer> + Send + Sync {
    move |request| {
        if attr_of(request, "type").as_deref() != Some("get") {
            return None;
        }
        let items = entries.iter().map(|(jid, t)| {
            let mut item = NodeBuilder::new("item");
            if let Some(jid) = jid {
                item = item.attr("jid", jid.as_str());
            }
            if let Some(t) = t {
                item = item.attr("t", t.to_string());
            }
            item.build()
        });
        Some(Answer::Result(vec![
            NodeBuilder::new("list").children(items).build(),
        ]))
    }
}

/// A `<notification type="w:gp2">` from the group `<group>@g.us` (W6a2): the actor (`participant`, with
/// `participant_pn` in a LID group), the time `t` (seconds), `notify` (the actor's display name), and one child
/// per action ([`gp2_people`], [`gp2_subject`], [`gp2_description`], or a bare tag such as `announcement`).
#[derive(Debug, Clone, Default)]
pub struct Gp2 {
    pub group: String,
    pub id: String,
    pub t: u64,
    pub participant: Option<String>,
    pub participant_pn: Option<String>,
    pub notify: Option<String>,
    pub lid: bool,
    pub children: Vec<Node>,
}

impl Gp2 {
    pub fn node(&self) -> Node {
        let mut node = NodeBuilder::new("notification")
            .attr("type", "w:gp2")
            .attr("from", format!("{}@g.us", self.group))
            .attr("id", self.id.as_str())
            .attr("t", self.t.to_string());
        let attrs = [
            ("participant", self.participant.as_ref()),
            ("participant_pn", self.participant_pn.as_ref()),
            ("notify", self.notify.as_ref()),
        ];
        for (key, value) in attrs {
            if let Some(value) = value {
                node = node.attr(key, value.as_str());
            }
        }
        if self.lid {
            node = node.attr("addressing_mode", "lid");
        }
        node.children(self.children.clone()).build()
    }
}

/// An action naming people (`add`, `remove`, `promote`, `demote`): one `<participant jid [phone_number]
/// [type]>` each (`(jid, phone_number, type)`, full JID texts).
pub fn gp2_people(tag: &'static str, people: &[(String, Option<String>, Option<&str>)]) -> Node {
    NodeBuilder::new(tag)
        .children(people.iter().map(|(jid, pn, kind)| {
            let mut node = NodeBuilder::new("participant").attr("jid", jid.as_str());
            if let Some(pn) = pn {
                node = node.attr("phone_number", pn.as_str());
            }
            if let Some(kind) = kind {
                node = node.attr("type", *kind);
            }
            node.build()
        }))
        .build()
}

/// `<subject subject [s_t]>` (a rename; `s_t` in seconds, written only when given).
pub fn gp2_subject(subject: &str, subject_time: Option<u64>) -> Node {
    let mut node = NodeBuilder::new("subject").attr("subject", subject);
    if let Some(t) = subject_time {
        node = node.attr("s_t", t.to_string());
    }
    node.build()
}

/// `<description id><body>text</body></description>`, or with `<delete/>` when `body` is `None`.
pub fn gp2_description(id: &str, body: Option<&str>) -> Node {
    let child = match body {
        Some(text) => NodeBuilder::new("body").string_content(text).build(),
        None => NodeBuilder::new("delete").build(),
    };
    NodeBuilder::new("description")
        .attr("id", id)
        .children([child])
        .build()
}

/// The server's `<notification type="w:gp2" from="g.us"><groups_dirty><group jid>..` (W6a2): these groups' cached
/// metadata went stale. `groups` are user parts.
pub fn groups_dirty(id: &str, t: u64, groups: &[&str]) -> Node {
    NodeBuilder::new("notification")
        .attr("type", "w:gp2")
        .attr("from", "g.us")
        .attr("id", id)
        .attr("t", t.to_string())
        .children([NodeBuilder::new("groups_dirty")
            .children(groups.iter().map(|g| {
                NodeBuilder::new("group")
                    .attr("jid", format!("{g}@g.us"))
                    .build()
            }))
            .build()])
        .build()
}

/// The published keys of one device, as the `encrypt` answer carries them.
#[derive(Debug, Clone)]
pub struct Bundle {
    pub registration_id: u32,
    /// The identity key (32 bytes, no type byte).
    pub identity: [u8; 32],
    pub signed_id: u32,
    pub signed: [u8; 32],
    pub signed_signature: [u8; 64],
    /// A one-time prekey (id, key), when the device still has one.
    pub one_time: Option<(u32, [u8; 32])>,
}

/// A 3-byte big-endian key id.
fn key_id(id: u32) -> Vec<u8> {
    id.to_be_bytes()[1..].to_vec()
}

/// A rule for `encrypt` (W5a): the prekey fetch (`<key><user jid>..</key>`) is answered with the
/// bundle of every device `bundles` names (a device JID text, compared as a JID) and `<error
/// code="406">` for the others (an unregistered device). The prekey count, upload and digest are left
/// to the 503.
pub fn prekeys(bundles: Vec<(String, Bundle)>) -> impl Fn(&Node) -> Option<Answer> + Send + Sync {
    move |request| {
        let key = request.get_optional_child("key")?;
        let users = key
            .get_children_by_tag("user")
            .filter_map(|u| attr_of(u, "jid"))
            .map(|jid| {
                let node = NodeBuilder::new("user").attr("jid", jid.clone());
                // By the device, not the text: upstream writes device 0 without its `:0`.
                let device = jid.parse::<wacore_binary::Jid>().ok();
                let Some((_, b)) = bundles
                    .iter()
                    .find(|(named, _)| named.parse::<wacore_binary::Jid>().ok() == device)
                else {
                    return node
                        .children([NodeBuilder::new("error")
                            .attr("code", "406")
                            .attr("text", "not-acceptable")
                            .build()])
                        .build();
                };
                let mut children = vec![
                    NodeBuilder::new("registration")
                        .bytes(b.registration_id.to_be_bytes().to_vec())
                        .build(),
                    NodeBuilder::new("type").bytes(vec![5]).build(),
                    NodeBuilder::new("identity")
                        .bytes(b.identity.to_vec())
                        .build(),
                ];
                if let Some((id, value)) = b.one_time {
                    children.push(
                        NodeBuilder::new("key")
                            .children([
                                NodeBuilder::new("id").bytes(key_id(id)).build(),
                                NodeBuilder::new("value").bytes(value.to_vec()).build(),
                            ])
                            .build(),
                    );
                }
                children.push(
                    NodeBuilder::new("skey")
                        .children([
                            NodeBuilder::new("id").bytes(key_id(b.signed_id)).build(),
                            NodeBuilder::new("value").bytes(b.signed.to_vec()).build(),
                            NodeBuilder::new("signature")
                                .bytes(b.signed_signature.to_vec())
                                .build(),
                        ])
                        .build(),
                );
                node.children(children).build()
            })
            .collect::<Vec<_>>();
        Some(Answer::Result(vec![
            NodeBuilder::new("list").children(users).build(),
        ]))
    }
}

/// `<success lid=".." t="..">`: the server accepts the login. Upstream's post-login work (the
/// `active` IQ, the wait for the offline delivery) starts from this node.
pub fn success(lid: &str) -> Node {
    NodeBuilder::new("success")
        .attr("lid", lid)
        .attr("t", "1700000000")
        .build()
}

/// [`success`] as the real server writes it: the `lid` attribute is the DEVICE's LID JID
/// (`<user>:<device>@lid`, here device 1, the account device of `peer::account_device`). With a bare user
/// upstream cannot parse it and logs a WARN that prints the number (W3b fix F6: the cases that read
/// upstream's own log lines use this one, so a number in an upstream WARN is a finding, not the fixture).
/// W5a 2c: upstream STORES this JID as the device's own LID (`node_io.rs` `SetLid`), so a `<user>@lid`
/// made the client believe it is device 0, the phone, and every send dropped the phone's copy as "the
/// sending device" (`wacore/src/send/dm.rs` `is_exact_dm_sender_device`); whatsmeow compares the stored
/// device LID with this attribute and calls a difference "should probably never happen"
/// (`connectionevents.go` `handleConnectSuccess`), so the server writes the device.
pub fn success_as_the_server_writes_it(lid_user: &str) -> Node {
    success(&format!("{lid_user}:1@lid"))
}

/// The phone's half of a pairing (W2c): a `<pair-success>` that upstream WOULD complete, built from
/// what the scanned code carries. `qr_data` is the string the QR step showed
/// (`<ref>,<noise>,<identity>,<adv secret>,<type>`: fields 2 and 3, counting from 0, are the device's
/// identity key and its ADV secret); `id` is the iq's id; `jid` and `lid` are the account's ids as the
/// server writes them (`<user>:<device>@s.whatsapp.net` and `<user>:<device>@lid`). It signs like
/// upstream's own test helper (`src/pair.rs` at the pin): a throw-away account key signs
/// `[6, 0] ++ details ++ the device's identity key`, and the container carries the HMAC of the signed
/// identity under the ADV secret. Without the right key and secret upstream answers an error and
/// never signs, which a refusal test cannot tell from the claim doing it: `tests/prepair.rs` therefore
/// runs the control (a pairing the rules accept) first.
pub fn signed_pair_success(qr_data: &str, id: &str, jid: &str, lid: &str) -> Node {
    let fields: Vec<&str> = qr_data.split(',').collect();
    assert!(fields.len() >= 4, "the code has {} fields", fields.len());
    let identity_pub = unbase64(fields[2]);
    let adv_secret = unbase64(fields[3]);

    let mut rng = rand::make_rng::<rand::rngs::StdRng>();
    let account = KeyPair::generate(&mut rng);
    let mut device = wa::ADVDeviceIdentity::default();
    device.key_index = Some(1);
    let details = device.encode_to_vec();
    let signature = account
        .private_key
        .calculate_signature(&[&[6, 0][..], &details, &identity_pub].concat(), &mut rng)
        .expect("the account key signs");
    let mut signed = wa::ADVSignedDeviceIdentity::default();
    signed.details = Some(details);
    signed.account_signature_key = Some(account.public_key.public_key_bytes().to_vec());
    signed.account_signature = Some(signature.to_vec());
    let identity = waproto::codec::adv_signed_device_identity_to_vec(&signed);
    let mut container = wa::ADVSignedDeviceIdentityHMAC::default();
    container.hmac = Some(wacore::libsignal::crypto::hmac_sha256(&adv_secret, &identity).to_vec());
    container.details = Some(identity);
    NodeBuilder::new("iq")
        .attrs([("from", "s.whatsapp.net"), ("type", "set"), ("id", id)])
        .children([NodeBuilder::new("pair-success")
            .children([
                NodeBuilder::new("device-identity")
                    .bytes(container.encode_to_vec())
                    .build(),
                NodeBuilder::new("device")
                    .attrs([("jid", jid), ("lid", lid)])
                    .build(),
            ])
            .build()])
        .build()
}

/// Standard base64 (the alphabet and padding the code's keys are written in), decoded by hand so the
/// fixture needs no dependency of its own. A character outside the alphabet panics: the code is the
/// client's own.
fn unbase64(text: &str) -> Vec<u8> {
    let sextet = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        other => panic!("not a base64 character: {other}"),
    };
    let mut out = Vec::new();
    let (mut bits, mut held) = (0_u32, 0_u32);
    for c in text.bytes().filter(|c| *c != b'=') {
        bits = (bits << 6) | u32::from(sextet(c));
        held += 6;
        if held >= 8 {
            held -= 8;
            out.push(u8::try_from(bits >> held).expect("eight bits"));
            bits &= (1 << held) - 1;
        }
    }
    out
}

/// A `CertChain` whose leaf key is the server's static key and whose signatures are zero: what the
/// bypass accepts and the strict policy refuses.
fn cert_chain_bytes(static_pub: &[u8; 32]) -> Vec<u8> {
    let details = |serial: u32, issuer: u32, key: Vec<u8>, from: u64, to: u64| {
        let mut d = noise_certificate::Details::default();
        d.serial = Some(serial);
        d.issuer_serial = Some(issuer);
        d.key = Some(key);
        d.not_before = Some(from);
        d.not_after = Some(to);
        d.encode_to_vec()
    };
    let certificate = |details: Vec<u8>| {
        let mut c = wa::cert_chain::NoiseCertificate::default();
        c.details = Some(details);
        c.signature = Some(vec![0u8; 64]);
        c
    };
    let mut chain = wa::CertChain::default();
    chain.intermediate = MessageField::some(certificate(details(
        1,
        0,
        vec![0xCC; 32],
        1_700_000_000,
        1_900_000_000,
    )));
    chain.leaf = MessageField::some(certificate(details(
        2,
        1,
        static_pub.to_vec(),
        1_700_000_500,
        1_899_999_500,
    )));
    chain.encode_to_vec()
}

/// A key pair for the server's ephemeral key: derived from a counter, never random. The fixture
/// needs a different pair per connection in the sense of Noise (a fresh one), not an unpredictable
/// one, and a fixed derivation keeps the crate free of a `rand` dependency and the runs the same.
fn ephemeral_pair() -> KeyPair {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    let mut seed = [0x3d_u8; 32];
    seed[..8].copy_from_slice(&(NEXT.fetch_add(1, Ordering::SeqCst) as u64).to_le_bytes());
    let private = PrivateKey::deserialize(&seed).expect("a 32-byte key");
    let public = private.public_key().expect("its public key");
    KeyPair::new(public, private)
}
