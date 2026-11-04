//! W4 (history at the link) over the scripted server. C1, THE WIRE PROBE (the refresh's first chunk,
//! verdict in `target/wa/w4/scout.md`): can a history chunk be driven through the REAL client at the
//! pin? Our own phone (device 0 of the account's own user, `Peer::own_phone`) sends a `pkmsg` whose
//! message is a `protocolMessage` of type `HISTORY_SYNC_NOTIFICATION` with an INLINE payload (a zlib
//! `HistorySync`, as `default_history_sync_config` asks the phone for the first chunk); the client
//! decrypts it, upstream detaches the notification (`decode_unpadded_detached_history_sync`), honours
//! it because it is from us (`is_from_me`), queues the history task, and the task calls the capture
//! hook BEFORE its `<receipt type="hist_sync">`.
//!
//! The hook here is the TEST's (`wire::parts_capturing`): it records what it was given and parks until
//! the test lets it go, then answers Ok or Err. What the probe pins:
//! - the hook is called once, with the stanza's id, the sync type and the payload byte for byte;
//! - no `hist_sync` receipt leaves while the hook is parked, exactly one after an Ok, none after an Err;
//! - the notification's OWN delivery receipt (a `peer_msg` receipt: the stanza is `category="peer"`)
//!   does NOT wait for the capture: it leaves while the hook is parked (the loss window of the brief's
//!   R1, which D2/H1's "announced" record answers), and after an Err as well (upstream does not retry);
//! - the notification's inbox row as the map leaves it (`deferred` when this was scouted; since W4 C7 the
//!   map announces it: the row ends `done` and `history_announced` holds its id).
//!
//! Both forms of our phone's id are probed: the LID device and the phone-number device (WA Web gates the
//! notice on `isMePrimaryNonLid`; upstream on `is_from_me`, which both match).
//!
//! C23 (W4, on the probe's GO) adds the tests of Kourier's OWN adapter below the probe's: the client is
//! made from `wire::parts` (the builder module's hook, no test hook) and the test plays the core, taking
//! every envelope and acknowledging it:
//! - `no_hist_sync_receipt_before_the_stage_commit`: the stage's writer is parked by a second connection's
//!   `BEGIN IMMEDIATE`; the chunk's `hist_sync` receipt does not leave until it is released (the notice's own
//!   receipt does), and then it is one receipt per chunk and the chunk's messages reach the core;
//! - `the_notice_and_live_messages_interleave`: live texts of the contact before and after the notice, and the
//!   chunk's messages, all reach the core once, under ONE conversation key; the notice's row ends `done` (not
//!   `deferred`), the chunk is recorded as announced, delivered and retired.
//!
//! Every body runs in ONE offline child under `unshare -rn`, with the synthetic numbers of
//! `support::peer`; nothing here touches a network.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kourier_backend_api::{BackendEvent, Command, StartMode};
use kourier_db::rusqlite::Connection;
use kourier_model::{ConvKey, Patch};
use kourier_whatsapp::StoreOwner;
use wacore::store::traits::{DeviceInfo, DeviceListRecord, ProtocolStore};
use waproto::buffa::{Message as _, MessageField};
use waproto::whatsapp as wa;

use crate::actor_real::{Running, record};
use crate::receive::{self, Rig, count, raw};
use crate::support::capture::Capture;
use crate::support::history::{Chat, Chunk, Msg, Status, SyncType};
use crate::support::peer::{self, Peer};
use crate::support::wire::{self, Wire};
use crate::support::{self, TEST_KEY, db_path, key};
use crate::wire::{start_actor, start_actor_capturing};

/// The id of the notification message (a phone's id; no digit run of the fixture's numbers).
pub(crate) const NOTICE: &str = "3AC0FFEE0000D001";
/// The id of a second notification (C23: a second chunk).
const NOTICE_2: &str = "3AC0FFEE0000D002";

/// What one call of the capture hook was given.
#[derive(Clone, Debug)]
pub(crate) struct Call {
    pub(crate) id: String,
    pub(crate) sync_type: Option<wa::message::HistorySyncType>,
    pub(crate) bytes: Vec<u8>,
}

/// The probe's capture hook: records each call, parks until [`Probe::release`], then answers Ok, or Err
/// when `refuse` is set. Keeps no `Arc<Client>` (critic L7).
pub(crate) struct Probe {
    calls: Mutex<Vec<Call>>,
    gate: tokio::sync::Semaphore,
    refuse: AtomicBool,
}

impl Probe {
    fn new(refuse: bool) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            gate: tokio::sync::Semaphore::new(0),
            refuse: AtomicBool::new(refuse),
        })
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().expect("lock").clone()
    }

    /// Lets one parked (or the next) call of the hook answer.
    fn release(&self) {
        self.gate.add_permits(1);
    }

    /// Waits (at most 15 s) until the hook was called `n` times.
    async fn called(&self, n: usize, rig: &Rig) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while self.calls().len() < n {
            assert!(
                Instant::now() < deadline,
                "the capture hook was called {} of {n} times (frames {:?})",
                self.calls().len(),
                frames_of(rig)
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

#[async_trait::async_trait]
impl whatsapp_rust::HistorySyncCaptureHook for Probe {
    async fn on_history_sync(
        &self,
        _client: Arc<whatsapp_rust::Client>,
        message_id: &str,
        sync_type: Option<wa::message::HistorySyncType>,
        compressed: &[u8],
    ) -> anyhow::Result<()> {
        self.calls.lock().expect("lock").push(Call {
            id: message_id.to_owned(),
            sync_type,
            bytes: compressed.to_vec(),
        });
        self.gate.acquire().await?.forget();
        if self.refuse.load(Ordering::SeqCst) {
            Err(anyhow::anyhow!("the probe refuses the chunk"))
        } else {
            Ok(())
        }
    }
}

/// `data` as a zlib stream of STORED deflate blocks (no compression): a valid zlib `HistorySync` with no
/// dependency (W4 C2 brings `flate2` and the blob builder; this stays the probe's).
pub(crate) fn zlib_stored(data: &[u8]) -> Vec<u8> {
    // CMF 0x78 (deflate, 32 KiB window), FLG 0x01: 0x7801 is a multiple of 31, no dictionary.
    let mut out = vec![0x78, 0x01];
    if data.is_empty() {
        out.extend([1, 0, 0, 0xff, 0xff]);
    }
    let mut blocks = data.chunks(usize::from(u16::MAX)).peekable();
    while let Some(block) = blocks.next() {
        out.push(u8::from(blocks.peek().is_none()));
        let len = u16::try_from(block.len()).expect("a stored block is at most 65535 bytes");
        out.extend(len.to_le_bytes());
        out.extend((!len).to_le_bytes());
        out.extend_from_slice(block);
    }
    let (mut a, mut b) = (1_u32, 0_u32);
    for &byte in data {
        a = (a + u32::from(byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    out.extend(((b << 16) | a).to_be_bytes());
    out
}

/// A first chunk as the phone uploads it: an `INITIAL_BOOTSTRAP` `HistorySync` with one empty chat of
/// the fictitious peer, chunk 1 of 1 (progress 100), zlib-compressed.
pub(crate) fn blob() -> Vec<u8> {
    let mut history = wa::HistorySync::default();
    history.sync_type = wa::history_sync::HistorySyncType::INITIAL_BOOTSTRAP;
    history.chunk_order = Some(1);
    history.progress = Some(100);
    history.conversations = vec![{
        let mut chat = wa::Conversation::default();
        chat.id = format!("{}@lid", peer::PEER_LID);
        chat
    }];
    zlib_stored(&history.encode_to_vec())
}

/// The message our phone sends for a chunk: `protocolMessage { type: HISTORY_SYNC_NOTIFICATION,
/// historySyncNotification { syncType, chunkOrder, progress, initialHistBootstrapInlinePayload } }`.
pub(crate) fn notification(payload: &[u8]) -> wa::Message {
    let mut notice = wa::message::HistorySyncNotification::default();
    notice.sync_type = Some(wa::message::HistorySyncType::INITIAL_BOOTSTRAP);
    notice.chunk_order = Some(1);
    notice.progress = Some(100);
    notice.initial_hist_bootstrap_inline_payload = Some(payload.to_vec());
    let mut protocol = wa::message::ProtocolMessage::default();
    protocol.r#type = Some(wa::message::protocol_message::Type::HISTORY_SYNC_NOTIFICATION);
    protocol.history_sync_notification = MessageField::some(notice);
    let mut message = wa::Message::default();
    message.protocol_message = MessageField::some(protocol);
    message
}

/// The frames of the notification: (tag, type) of every frame the clients sent that names [`NOTICE`].
fn frames_of(rig: &Rig) -> Vec<(String, String)> {
    frames_named(rig, NOTICE)
}

/// As [`frames_of`] for the notification `id`.
fn frames_named(rig: &Rig, id: &str) -> Vec<(String, String)> {
    rig.server
        .frames()
        .iter()
        .filter(|f| f.attr("id") == Some(id))
        .map(|f| (f.tag.clone(), f.attr("type").unwrap_or("-").to_owned()))
        .collect()
}

fn hist_sync_receipts(rig: &Rig) -> usize {
    hist_syncs_of(rig, NOTICE)
}

/// How many `hist_sync` receipts for the notification `id` the clients have sent.
fn hist_syncs_of(rig: &Rig, id: &str) -> usize {
    frames_named(rig, id)
        .iter()
        .filter(|(tag, kind)| tag == "receipt" && kind == "hist_sync")
        .count()
}

/// The notification's own delivery receipt (any receipt of [`NOTICE`] that is not `hist_sync`), when one
/// left within `within`: its type.
async fn delivery_receipt(rig: &Rig, within: Duration) -> Option<String> {
    delivery_receipt_of(rig, NOTICE, within).await
}

/// As [`delivery_receipt`] for the notification `id`.
async fn delivery_receipt_of(rig: &Rig, id: &str, within: Duration) -> Option<String> {
    let deadline = Instant::now() + within;
    loop {
        if let Some((_, kind)) = frames_named(rig, id)
            .into_iter()
            .find(|(tag, kind)| tag == "receipt" && kind != "hist_sync")
        {
            return Some(kind);
        }
        if Instant::now() > deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The account of `receive::account_dir`, which also knows its own two devices (the phone 0 and us 1)
/// under both of its ids: an unknown own device would send a device-list query first.
async fn account_dir() -> (tempfile::TempDir, wacore::store::Device) {
    let (dir, device) = receive::account_dir().await;
    let owner = StoreOwner::open(&db_path(dir.path()), &key(), "wa-test").expect("open");
    let store = owner.store();
    for user in [peer::ACCOUNT_LID, peer::ACCOUNT_PN] {
        store
            .update_device_list(DeviceListRecord {
                user: Arc::from(user),
                devices: vec![DeviceInfo::new(0, None), DeviceInfo::new(1, None)]
                    .into_boxed_slice(),
                timestamp: 1_700_000_000,
                phash: None,
                raw_id: None,
            })
            .await
            .expect("the account's own devices");
    }
    owner.close();
    (dir, device)
}

/// The account online over the Wire with the probe's hook on its client, and its own phone.
async fn rig(probe: Arc<Probe>, by_number: bool) -> (Rig, Peer) {
    rig_started(
        |dir, server| start_actor_capturing(dir, StartMode::Resume, server, probe),
        by_number,
    )
    .await
}

/// The account online over the Wire with KOURIER'S OWN capture hook on its client (the builder module's,
/// no test hook), and its own phone (the LID device).
async fn adapter_rig() -> (Rig, Peer) {
    rig_started(
        |dir, server| start_actor(dir, StartMode::Resume, server),
        false,
    )
    .await
}

/// A rig whose actor `start` makes.
async fn rig_started(
    start: impl FnOnce(&std::path::Path, &Wire) -> Running,
    by_number: bool,
) -> (Rig, Peer) {
    let (dir, device) = account_dir().await;
    let server = wire::server();
    server.answering();
    server.log_to(&dir.path().join("wire.log"));
    let actor = start(dir.path(), &server);
    let states = record(actor.conn());
    actor.send(Command::Connect).await;
    assert!(
        server.wait_opened(1, Duration::from_secs(15)).await,
        "the handshake never completed"
    );
    server.send(&wire::success_as_the_server_writes_it(peer::ACCOUNT_LID));
    let contact = Peer::new(&device, None).await;
    let phone = Peer::own_phone(&device, by_number).await;
    let rig = Rig {
        server,
        dir,
        actor,
        peer: contact,
        states,
    };
    rig.online().await;
    (rig, phone)
}

/// Our phone's stanza for the chunk `payload`: a `pkmsg` of the notification, `category="peer"` as the
/// phone's own protocol messages to its companions are.
async fn notice_stanza(phone: &mut Peer, payload: &[u8]) -> wacore_binary::Node {
    notice_stanza_with(phone, NOTICE, payload).await
}

/// As [`notice_stanza`] for the notification `id`.
async fn notice_stanza_with(phone: &mut Peer, id: &str, payload: &[u8]) -> wacore_binary::Node {
    let mut stanza = phone
        .message_with_id(&notification(payload), id, "text")
        .await;
    stanza.attrs.insert("category", "peer");
    stanza
}

/// The inbox row of the notification after the run: its state.
fn notice_row(conn: &Connection) -> String {
    notice_row_of(conn, NOTICE)
}

/// As [`notice_row`] for the notification `id`.
fn notice_row_of(conn: &Connection, id: &str) -> String {
    conn.query_row("SELECT state FROM inbox WHERE id = ?1", [id], |r| {
        r.get::<_, String>(0)
    })
    .unwrap_or_else(|_| "absent".to_owned())
}

/// C1, case `ok`: the hook is reached with the chunk, its Ok releases ONE `hist_sync` receipt, and none
/// leaves before it.
async fn the_capture_is_reached_and_the_receipt_waits_for_its_ok(by_number: bool) {
    let probe = Probe::new(false);
    let (rig, mut phone) = rig(probe.clone(), by_number).await;
    let payload = blob();
    let sent = Instant::now();
    rig.server.send(&notice_stanza(&mut phone, &payload).await);
    probe.called(1, &rig).await;
    let reached_ms = sent.elapsed().as_millis();
    let call = probe.calls().remove(0);
    assert_eq!(call.id, NOTICE, "the hook names the notification's message");
    assert_eq!(
        call.sync_type,
        Some(wa::message::HistorySyncType::INITIAL_BOOTSTRAP)
    );
    assert!(
        call.bytes == payload,
        "the hook gets the inline payload byte for byte"
    );
    // Parked: the notification's own receipt may leave, the chunk's may not.
    let delivery = delivery_receipt(&rig, Duration::from_secs(5)).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        hist_sync_receipts(&rig),
        0,
        "a hist_sync receipt left while the capture was parked: {:?}",
        frames_of(&rig)
    );
    probe.release();
    let deadline = Instant::now() + Duration::from_secs(15);
    while hist_sync_receipts(&rig) == 0 {
        assert!(
            Instant::now() < deadline,
            "no hist_sync receipt after the capture's Ok: {:?}",
            frames_of(&rig)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        hist_sync_receipts(&rig),
        1,
        "one hist_sync receipt per chunk"
    );
    assert_eq!(probe.calls().len(), 1, "the hook was called once");
    let frames = frames_of(&rig);
    let conn = rig.finish().await;
    eprintln!(
        "probe: by_number={by_number} reached_ms={reached_ms} delivery_while_parked={delivery:?} \
         frames={frames:?} inbox_row={} inbox_rows={} core_outbox={}",
        notice_row(&conn),
        count(&conn, "SELECT count(*) FROM inbox"),
        count(&conn, "SELECT count(*) FROM core_outbox"),
    );
}

/// C1, case `refused`: a hook that answers Err withholds the `hist_sync` receipt; the notification's own
/// receipt is not withheld (upstream neither retries nor buffers the chunk).
async fn a_refused_capture_withholds_the_receipt() {
    let probe = Probe::new(true);
    probe.release();
    let (rig, mut phone) = rig(probe.clone(), false).await;
    let payload = blob();
    rig.server.send(&notice_stanza(&mut phone, &payload).await);
    probe.called(1, &rig).await;
    let delivery = delivery_receipt(&rig, Duration::from_secs(5)).await;
    tokio::time::sleep(Duration::from_millis(2000)).await;
    assert_eq!(
        hist_sync_receipts(&rig),
        0,
        "a hist_sync receipt left after the capture's Err: {:?}",
        frames_of(&rig)
    );
    assert_eq!(
        probe.calls().len(),
        1,
        "upstream does not call the hook again"
    );
    let frames = frames_of(&rig);
    let dials = rig.server.dials();
    let conn = rig.finish().await;
    eprintln!(
        "probe: refused delivery={delivery:?} frames={frames:?} dials={dials} inbox_row={}",
        notice_row(&conn)
    );
}

// ---------------------------------------------------------------------------------------------------
// C23: Kourier's OWN adapter over the Wire (the builder module's hook, the stage file, the driver). The
// test plays the core: it takes every envelope the actor ships and acknowledges it.
// ---------------------------------------------------------------------------------------------------

/// The contact's chat as the phone's history names it (the form the live stanza's sender maps to as well).
fn contact_chat() -> String {
    format!("{}@lid", peer::PEER_LID)
}

/// What the test, playing the core, was given: the messages of live envelopes and of history batches, in
/// the order they arrived, each with its conversation.
#[derive(Default)]
struct Core {
    live: Vec<(String, ConvKey)>,
    history: Vec<(String, ConvKey)>,
    /// How many address-book names the core was told (a `system_name` set; never the name itself).
    named: usize,
    /// The variant names of every event taken (a failure's text).
    seen: Vec<&'static str>,
}

impl Core {
    /// Takes the next envelope within `wait`, notes its messages and acknowledges it as the core would.
    /// `false` when none came.
    async fn take(&mut self, rig: &mut Rig, wait: Duration) -> bool {
        let Ok(Some(envelope)) = tokio::time::timeout(wait, rig.actor.ends.events.recv()).await
        else {
            return false;
        };
        for event in &envelope.events {
            self.seen.push(event.name());
            match event {
                BackendEvent::Message(m) => self.live.push((m.local.to_string(), m.conv.clone())),
                BackendEvent::HistoryBatch { messages, .. } => {
                    for m in messages {
                        self.history.push((m.local.to_string(), m.conv.clone()));
                    }
                }
                BackendEvent::ContactUpsert(patch)
                    if matches!(patch.system_name, Patch::Set(_)) =>
                {
                    self.named += 1;
                }
                _ => {}
            }
        }
        if let Some(ack) = envelope.ack {
            ack.complete(Ok(()));
        }
        true
    }

    /// Takes and acknowledges envelopes until `done` holds (at most 30 s).
    async fn until(&mut self, rig: &mut Rig, what: &str, done: impl Fn(&Core) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done(self) {
            assert!(
                Instant::now() < deadline,
                "gave up waiting for {what}: live {}, history {}, events {:?}, frames {:?}",
                self.live.len(),
                self.history.len(),
                self.seen,
                rig.server.frames()
            );
            self.take(rig, Duration::from_millis(50)).await;
        }
    }
}

/// How many chunks the delivery has retired (the books of `backend.db`).
fn retired(books: &Connection) -> i64 {
    count(
        books,
        "SELECT count(*) FROM history_chunks WHERE retired = 1",
    )
}

/// A second connection to the account's stage file (`history/stage.db`), opened with the key the stage
/// derives from the account's: HKDF-SHA256 of the backend key with the stage's info string. A stage that
/// changed its key derivation would not open here, which fails the test loudly.
fn stage_connection(dir: &std::path::Path) -> Connection {
    let mut derived = [0_u8; 32];
    hkdf::Hkdf::<sha2::Sha256>::new(None, &TEST_KEY)
        .expand(b"kourier whatsapp history stage 1", &mut derived)
        .expect("32 bytes are a valid HKDF output");
    kourier_db::open(
        &dir.join("history").join("stage.db"),
        &kourier_db::DbKey::from(derived),
    )
    .expect("the stage file")
}

/// A chunk with one text of the contact in his chat, for the park case.
fn one_text_chunk(order: u32, progress: u32, id: &str, at: u64) -> Vec<u8> {
    let chat = contact_chat();
    Chunk::new(SyncType::INITIAL_BOOTSTRAP, order, progress)
        .chat(Chat::new(&chat).active_at(at).message(Msg::text(
            &chat,
            id,
            at,
            "synthetic history text",
        )))
        .blob()
}

/// C23, case `park`: the phone is answered only after the chunk is in the stage. A first chunk makes the
/// stage file and is delivered and retired; then the stage's writer is parked (a second connection's
/// `BEGIN IMMEDIATE`, the pattern of `receive::no_ack_of_a_receipt_before_its_row`) while a second chunk
/// arrives: the notification's own receipt leaves, its `hist_sync` receipt does not, until the lock is
/// released; then there is exactly one, and the chunk's message reaches the core. A capture that answered
/// Ok before its commit, or a hook that was never registered, sends the receipt while the lock is held.
async fn the_stage_commit_comes_before_the_hist_sync_receipt_case() {
    let (mut rig, mut phone) = adapter_rig().await;
    let books = raw(rig.dir.path());
    let mut core = Core::default();

    let first = one_text_chunk(1, 50, "3AA0000000000001", 1_700_000_010);
    rig.server
        .send(&notice_stanza_with(&mut phone, NOTICE, &first).await);
    core.until(&mut rig, "the first chunk delivered and retired", |c| {
        c.history.len() == 1 && retired(&books) == 1
    })
    .await;
    assert_eq!(
        hist_syncs_of(&rig, NOTICE),
        1,
        "one hist_sync receipt for the first chunk: {:?}",
        frames_named(&rig, NOTICE)
    );

    let holder = stage_connection(rig.dir.path());
    holder
        .execute_batch("BEGIN IMMEDIATE")
        .expect("the stage's write lock");
    let second = one_text_chunk(2, 100, "3AA0000000000002", 1_700_000_020);
    rig.server
        .send(&notice_stanza_with(&mut phone, NOTICE_2, &second).await);
    // The notification reached the client (its own receipt left); the capture waits for the file.
    let delivery = delivery_receipt_of(&rig, NOTICE_2, Duration::from_secs(5)).await;
    assert!(
        delivery.is_some(),
        "the second notification never reached the client: {:?}",
        frames_named(&rig, NOTICE_2)
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        hist_syncs_of(&rig, NOTICE_2),
        0,
        "the phone was told the chunk arrived while the stage's writer was parked: {:?}",
        frames_named(&rig, NOTICE_2)
    );
    holder.execute_batch("ROLLBACK").expect("release");
    let deadline = Instant::now() + Duration::from_secs(15);
    while hist_syncs_of(&rig, NOTICE_2) == 0 {
        assert!(
            Instant::now() < deadline,
            "no hist_sync receipt after the stage's lock was released: {:?}",
            frames_named(&rig, NOTICE_2)
        );
        core.take(&mut rig, Duration::from_millis(20)).await;
    }
    core.until(&mut rig, "the second chunk delivered and retired", |c| {
        c.history.len() == 2 && retired(&books) == 2
    })
    .await;
    assert_eq!(
        hist_syncs_of(&rig, NOTICE_2),
        1,
        "one hist_sync receipt per chunk: {:?}",
        frames_named(&rig, NOTICE_2)
    );
    drop(holder);
    let conn = rig.finish().await;
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM history_announced WHERE lost = 0"
        ),
        2,
        "both chunks were announced, none lost"
    );
}

/// The name, first name and number the phone's address book gives the contact in the chunk (history kind 3, W6a2 PRIV-9):
/// synthetic, and none of them may be in a log line (`secrets()`).
const KIND3_NAME: &str = "Synthetic Kindthree";
const KIND3_FIRST: &str = "Kindfirst";
const KIND3_PN: &str = "15558360274";

/// The live texts and the history messages of the interleave case.
const LIVE_1: &str = "3EB0000000000021";
const LIVE_2: &str = "3EB0000000000022";
const HIST_1: &str = "3AA0000000000011";
const HIST_2: &str = "3AA0000000000012";
const HIST_3: &str = "3AA0000000000013";

/// C23, case `interleave`: a live text of the contact, the notification of a chunk that holds the same
/// contact's chat (two texts of his and one of ours), and another live text, all pushed at once. Every
/// message reaches the core exactly once, the live ones as live messages and the chunk's as history, all
/// under ONE conversation key (the live road and the history keep the same key, end to end); the
/// notification's row ends `done` (a map that left it `deferred` shows here), its id is recorded as
/// announced and not lost, the phone is told once, and the chunk is delivered and retired. The chunk also carries one
/// address-book entry for the contact (history kind 3): the core is told its name once, after the push, and neither the
/// name, the first name nor the number is in a log line (PRIV-9).
async fn the_notice_and_live_messages_interleave_case() {
    let (mut rig, mut phone) = adapter_rig().await;
    let books = raw(rig.dir.path());
    let mut core = Core::default();
    let chat = contact_chat();
    let mut chunk = Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 100).chat(
        Chat::new(&chat)
            .active_at(1_700_000_050)
            .message(Msg::text(&chat, HIST_3, 1_700_000_050, "synthetic third"))
            .message(Msg::mine(
                &chat,
                HIST_2,
                1_700_000_040,
                "synthetic second, ours",
                Status::READ,
            ))
            .message(Msg::text(&chat, HIST_1, 1_700_000_030, "synthetic first")),
    );
    // The phone's address book names the contact (kind 3): taken up once after the push, stated for the person his live
    // texts made known. Its name is text of a person: it must reach the core and no log (PRIV-9).
    let mut entry = wa::InlineContact::default();
    entry.pn_jid = Some(format!("{KIND3_PN}@s.whatsapp.net"));
    entry.lid_jid = Some(chat.clone());
    entry.full_name = Some(KIND3_NAME.to_owned());
    entry.first_name = Some(KIND3_FIRST.to_owned());
    chunk.sync.inline_contacts = vec![entry];
    let chunk = chunk.blob();

    let first = rig.peer.text_with_id("synthetic live one", LIVE_1).await;
    let second = rig.peer.text_with_id("synthetic live two", LIVE_2).await;
    rig.server.send(&first);
    rig.server
        .send(&notice_stanza_with(&mut phone, NOTICE, &chunk).await);
    rig.server.send(&second);

    core.until(
        &mut rig,
        "both live texts and the three history messages",
        |c| c.live.len() == 2 && c.history.len() == 3 && retired(&books) == 1 && c.named == 1,
    )
    .await;
    // Nothing more follows: every message came once.
    while core.take(&mut rig, Duration::from_millis(300)).await {}
    assert_eq!(core.named, 1, "the kind-3 name was stated once");

    let mut live: Vec<&str> = core.live.iter().map(|(id, _)| id.as_str()).collect();
    live.sort_unstable();
    assert_eq!(live, [LIVE_1, LIVE_2], "the live texts, once each");
    let mut history: Vec<&str> = core.history.iter().map(|(id, _)| id.as_str()).collect();
    history.sort_unstable();
    assert_eq!(
        history,
        [HIST_1, HIST_2, HIST_3],
        "the chunk's messages, once each"
    );
    let convs: std::collections::BTreeSet<&ConvKey> = core
        .live
        .iter()
        .chain(core.history.iter())
        .map(|(_, conv)| conv)
        .collect();
    assert_eq!(
        convs.len(),
        1,
        "the live texts and the history are one conversation: {convs:?}"
    );
    assert_eq!(
        hist_syncs_of(&rig, NOTICE),
        1,
        "the phone is told once: {:?}",
        frames_named(&rig, NOTICE)
    );
    for id in [LIVE_1, LIVE_2] {
        assert_eq!(rig.receipts_for(id), 1, "one receipt per live text");
    }

    let conn = rig.finish().await;
    assert_eq!(
        notice_row_of(&conn, NOTICE),
        "done",
        "the notification's row is mapped (announced), not deferred"
    );
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM history_announced WHERE lost = 0"
        ),
        1,
        "the chunk is recorded as announced"
    );
    assert_eq!(
        count(&conn, "SELECT delivered FROM history_counts"),
        3,
        "the chunk's messages are counted delivered"
    );
    assert_eq!(
        count(&conn, "SELECT count(*) FROM core_outbox"),
        0,
        "every row the core acknowledged is gone"
    );
}

/// What the child's output and logs must never hold (D10): the fixture's numbers, LIDs and name. The
/// notification's id is not among them: upstream's ERROR line of a refused capture names it (critic L2,
/// W4 C5 puts that shape into the log corpus).
fn secrets() -> Vec<&'static str> {
    vec![
        peer::ACCOUNT_PN,
        peer::ACCOUNT_LID,
        peer::PEER_LID,
        "Synthetic",
        KIND3_NAME,
        KIND3_FIRST,
        KIND3_PN,
    ]
}

const CASE: &str = "WA_HISTORY_WIRE_CASE";

/// One parent per probe case: the case runs in the offline child below, and nothing the child printed holds
/// a secret of the fixture.
fn parent(case: &str) {
    // The probe's chunk is taken by the test's own hook, never by Kourier's stage, so the notice stage 2 records stays
    // "announced and not captured": the quit would wait the whole 15 s for it (W4 C20). The child's quit does not wait.
    parent_with(case, &[("KOURIER_WA_QUIT_WAIT_MS", "0")]);
}

/// One parent per case of KOURIER'S adapter (C23): its chunks ARE captured, so the quit waits for nothing and
/// the child keeps the normal wait.
fn parent_adapter(case: &str) {
    parent_with(case, &[]);
}

fn parent_with(case: &str, env: &[(&str, &str)]) {
    let mut vars = vec![(CASE, case)];
    vars.extend_from_slice(env);
    let output = support::offline::run_child_output(module_path!(), "history_wire_child", &vars);
    for secret in secrets() {
        assert!(
            !output.contains(secret),
            "the child printed one of the fixture's secrets"
        );
    }
    // The probe's facts, for the scout (`--success-output immediate` shows them).
    for line in output.lines().filter(|l| l.starts_with("probe:")) {
        println!("{line}");
    }
}

#[test]
fn probe_a_chunk_from_our_lid_phone_reaches_the_capture_and_its_receipt_waits_for_the_ok() {
    parent("lid");
}

#[test]
fn probe_a_chunk_from_our_number_phone_reaches_the_capture_and_its_receipt_waits_for_the_ok() {
    parent("number");
}

#[test]
fn probe_a_refused_capture_withholds_the_hist_sync_receipt() {
    parent("refused");
}

#[test]
fn no_hist_sync_receipt_before_the_stage_commit() {
    parent_adapter("park");
}

#[test]
fn the_notice_and_live_messages_interleave() {
    parent_adapter("interleave");
}

/// The child (a no-op as an ordinary test): the case the parent named, with every log line recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_wire_child() {
    if !support::offline::in_child() {
        return;
    }
    let log = Capture::default();
    log.install();
    let case = std::env::var(CASE).expect("the parent names the case");
    eprintln!("case {case}");
    match case.as_str() {
        "lid" => the_capture_is_reached_and_the_receipt_waits_for_its_ok(false).await,
        "number" => the_capture_is_reached_and_the_receipt_waits_for_its_ok(true).await,
        "refused" => a_refused_capture_withholds_the_receipt().await,
        "park" => the_stage_commit_comes_before_the_hist_sync_receipt_case().await,
        "interleave" => the_notice_and_live_messages_interleave_case().await,
        other => panic!("unknown case {other}"),
    }
    assert!(
        log.own_lines() >= 1,
        "the recorder saw no line of the crate"
    );
    assert!(
        log.upstream_records() >= 1,
        "the recorder heard no record of upstream's crates"
    );
    assert_eq!(
        log.leaks(&secrets()),
        Vec::<String>::new(),
        "a log line holds a secret of the fixture (the targets are named)"
    );
}
