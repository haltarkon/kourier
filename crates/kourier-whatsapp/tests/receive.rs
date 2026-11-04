//! W3a (chunks C8 to C10): the RECEIVE path over the scripted server. A real client, built by
//! Kourier's own `build_bot` with the durability hook and the one handler, talks to the Wire of
//! `support::wire`; a fictitious peer (`support::peer`) encrypts messages to the account's keys and
//! the server pushes them as the stanzas WhatsApp would. What this can show that the hook-level tests
//! (`src/receive_kill.rs`, no client) cannot: upstream's own pipeline around the hook, that is the
//! ORDER of what the client sends back (its delivery receipt) against what Kourier made durable.
//!
//! Every body runs in ONE offline child under `unshare -rn` (lane decision L3), with the synthetic
//! numbers of `support::peer`; nothing here touches a network.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kourier_backend_api::{BackendEvent, Command, Exit};
use kourier_db::rusqlite::Connection;
use kourier_model::{Body, ConnState, ConvKey, RangeKind, TargetAuthor, UserId};
use kourier_whatsapp::StoreOwner;
use wacore::proto_helpers::MessageBuilderExt as _;
use wacore::store::traits::{DeviceInfo, DeviceListRecord, DeviceStore, ProtocolStore};
use wacore_binary::builder::NodeBuilder;
use waproto::buffa::MessageField;
use waproto::whatsapp as wa;

use crate::actor_real::{Running, record, wait_for_states};
use crate::support::capture::Capture;
use crate::support::peer::{self, Peer};
use crate::support::wire::{self, Wire};
use crate::support::{self, db_path, key};
use crate::wire::start_actor;

/// A running account over the scripted server, and the peer that writes to it.
pub(crate) struct Rig {
    pub(crate) server: Wire,
    pub(crate) dir: tempfile::TempDir,
    pub(crate) actor: Running,
    pub(crate) peer: Peer,
    pub(crate) states: Arc<Mutex<Vec<ConnState>>>,
}

/// The account's directory: a paired device with real keys, and the peer's device known to it (an
/// unknown device would make the client ask the server for the contact's devices first).
pub(crate) async fn account_dir() -> (tempfile::TempDir, wacore::store::Device) {
    let dir = tempfile::tempdir().expect("temp dir");
    let device = peer::account_device();
    let owner = StoreOwner::open(&db_path(dir.path()), &key(), "wa-test").expect("open");
    let store = owner.store();
    store.save(&device).await.expect("the paired device");
    store
        .update_device_list(DeviceListRecord {
            user: Arc::from(peer::PEER_LID),
            devices: vec![DeviceInfo::new(0, None)].into_boxed_slice(),
            timestamp: 1_700_000_000,
            phash: None,
            raw_id: None,
        })
        .await
        .expect("the peer's devices");
    owner.close();
    (dir, device)
}

/// The account connected over the Wire: handshake done, `<success>` sent, nothing else yet.
pub(crate) async fn rig() -> Rig {
    rig_with(false).await
}

/// As [`rig`], with the end of the offline delivery held back: the test sends the stanzas of the
/// drain and `wire::offline_end` itself.
pub(crate) async fn rig_holding_offline() -> Rig {
    rig_with(true).await
}

async fn rig_with(hold_offline_end: bool) -> Rig {
    let (dir, device) = account_dir().await;
    let server = wire::server();
    server.answering();
    if hold_offline_end {
        server.hold_offline_end();
    }
    server.log_to(&dir.path().join("wire.log"));
    let actor = start_actor(dir.path(), kourier_backend_api::StartMode::Resume, &server);
    let states = record(actor.conn());
    actor.send(Command::Connect).await;
    assert!(
        server.wait_opened(1, Duration::from_secs(15)).await,
        "the handshake never completed"
    );
    server.send(&wire::success_as_the_server_writes_it(peer::ACCOUNT_LID));
    let peer = Peer::new(&device, None).await;
    Rig {
        server,
        dir,
        actor,
        peer,
        states,
    }
}

impl Rig {
    /// The account is `Online`: the connection is up and the drain (an empty one) is through.
    pub(crate) async fn online(&self) {
        wait_for_states(&self.states, 20, &[ConnState::Online { detail: None }]).await;
    }

    /// The account is `Online` again on a connection that began after `from` states were seen.
    pub(crate) async fn online_after(&self, from: usize) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let found = self
                .states
                .lock()
                .expect("lock")
                .iter()
                .skip(from)
                .any(|s| matches!(s, ConnState::Online { .. }));
            if found {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the account never came back online: {:?}",
                self.states.lock().expect("lock")
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// How many stanzas with this tag and id the clients have sent in all.
    pub(crate) fn frames_with(&self, tag: &str, id: &str) -> usize {
        self.server
            .frames()
            .iter()
            .filter(|f| f.tag == tag && f.attr("id") == Some(id))
            .count()
    }

    /// How many receipts for message `id` the clients have sent in all.
    pub(crate) fn receipts_for(&self, id: &str) -> usize {
        self.server
            .frames()
            .iter()
            .filter(|f| f.tag == "receipt" && f.attr("id") == Some(id))
            .count()
    }

    /// The `Message` events of the next envelopes, acknowledged as the core would; stops once `n`
    /// messages were seen. Panics when they do not come.
    pub(crate) async fn messages(&mut self, n: usize) -> usize {
        self.events_where(n, |e| matches!(e, BackendEvent::Message(_)))
            .await
    }

    /// As [`Rig::messages`] for `Receipts` events.
    pub(crate) async fn receipts(&mut self, n: usize) -> usize {
        self.events_where(n, |e| matches!(e, BackendEvent::Receipts { .. }))
            .await
    }

    pub(crate) async fn events_where(
        &mut self,
        n: usize,
        wanted: impl Fn(&BackendEvent) -> bool,
    ) -> usize {
        let mut seen = 0;
        let deadline = Instant::now() + Duration::from_secs(20);
        while seen < n {
            let left = deadline.saturating_duration_since(Instant::now());
            let envelope = tokio::time::timeout(left, self.actor.ends.events.recv())
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "only {seen} of {n} events came (frames {:?}, states {:?})",
                        self.server.frames(),
                        self.states.lock().expect("lock")
                    )
                })
                .expect("an envelope");
            seen += envelope.events.iter().filter(|e| wanted(e)).count();
            if let Some(ack) = envelope.ack {
                ack.complete(Ok(()));
            }
        }
        seen
    }

    /// Waits (at most 15 s) for a frame the client sent, by its tag and id.
    pub(crate) async fn frame_with(&self, tag: &str, id: &str) -> wire::Seen {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(seen) = self
                .server
                .frames()
                .into_iter()
                .find(|f| f.tag == tag && f.attr("id") == Some(id))
            {
                return seen;
            }
            assert!(
                Instant::now() < deadline,
                "no <{tag}> for the message in {:?}",
                self.server.frames()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The end: the actor shuts down and the database can be read.
    pub(crate) async fn finish(mut self) -> Connection {
        self.actor.send(Command::Shutdown).await;
        assert_eq!(self.actor.exit().await.0, Exit::Shutdown);
        raw(self.dir.path())
    }
}

/// A read connection to the account's file.
pub(crate) fn raw(dir: &Path) -> Connection {
    kourier_db::open(&db_path(dir), &key()).expect("raw open")
}

pub(crate) fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).expect(sql)
}

/// C8, the GO/FALLBACK question: a client with Kourier's store, hook and handler reaches the
/// session over the Wire, decrypts a `pkmsg` of the fictitious peer, makes it durable and tells the
/// core (one `Message` event), and sends its delivery receipt.
async fn one_pkmsg_lands_as_one_done_row_and_one_event() {
    let mut rig = rig().await;
    rig.online().await;
    let stanza = rig
        .peer
        .text_with_id("synthetic hello", "3EB0000000000001")
        .await;
    rig.server.send(&stanza);
    assert_eq!(rig.messages(1).await, 1);
    rig.frame_with("receipt", "3EB0000000000001").await;
    let conn = rig.finish().await;
    assert_eq!(count(&conn, "SELECT count(*) FROM inbox"), 1);
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM inbox WHERE state = 'done' AND message IS NULL"
        ),
        1
    );
    assert_eq!(
        count(&conn, "SELECT count(*) FROM core_outbox"),
        0,
        "acked rows are gone"
    );
}

/// C9, the mandated mutant test: the zone of the hook's commit FAILS (the crate's own seam, as a full
/// disk would): upstream must send NO receipt, the connection ends (the write failed), the actor builds a new
/// client after its backoff, and the server's redelivery of the same stanza lands once. A hook that
/// ignores the error, or answers `Ok` before its zone ran, sends the receipt and loses the message.
async fn a_failed_commit_sends_no_receipt_and_the_redelivery_lands_once() {
    const ID: &str = "3EB0000000000002";
    let mut rig = rig().await;
    rig.online().await;
    let before = rig.states.lock().expect("lock").len();
    kourier_whatsapp::fault::arm(kourier_whatsapp::fault::INBOX_COMMIT, 1);
    let stanza = rig.peer.text_with_id("synthetic one", ID).await;
    rig.server.send(&stanza);
    // The failed write ends this connection; after the actor's backoff a NEW client dials.
    assert!(
        rig.server.wait_opened(2, Duration::from_secs(30)).await,
        "no new connection after the failed commit (frames {:?})",
        rig.server.frames()
    );
    assert_eq!(
        rig.receipts_for(ID),
        0,
        "a receipt left before the message was durable"
    );
    // The server did not get its acknowledgement, so it delivers the stanza again.
    rig.server
        .send(&wire::success_as_the_server_writes_it(peer::ACCOUNT_LID));
    rig.online_after(before).await;
    rig.server.send(&stanza);
    assert_eq!(rig.messages(1).await, 1);
    rig.frame_with("receipt", ID).await;
    assert_eq!(rig.receipts_for(ID), 1, "one receipt for one message");
    // Nothing else arrives: the message landed once.
    assert!(
        tokio::time::timeout(Duration::from_millis(500), rig.actor.ends.events.recv())
            .await
            .is_err(),
        "a second envelope followed the message"
    );
    kourier_whatsapp::fault::disarm();
    let conn = rig.finish().await;
    assert_eq!(count(&conn, "SELECT count(*) FROM inbox"), 1);
    assert_eq!(
        count(&conn, "SELECT count(*) FROM inbox WHERE state = 'done'"),
        1
    );
}

/// A `<receipt>` of the peer for one of the account's messages (`kind`: `read`, `played`; a
/// delivery receipt has none).
pub(crate) fn peer_receipt(id: &str, kind: &str) -> wacore_binary::Node {
    NodeBuilder::new("receipt")
        .attr("from", format!("{}:0@lid", peer::PEER_LID))
        .attr("id", id)
        .attr("t", "1700000200")
        .attr("type", kind)
        .build()
}

/// C9, the handler's rule (a) for a durable kind: the write of a receipt FAILS: upstream sends no
/// `<ack>` for the stanza, the connection ends, and the server's redelivery of the same receipt is
/// written and acknowledged on the next connection.
async fn a_failed_receipt_write_withholds_the_ack_and_the_redelivery_is_acked() {
    const ID: &str = "3EB0000000000003";
    let mut rig = rig().await;
    rig.online().await;
    let before = rig.states.lock().expect("lock").len();
    kourier_whatsapp::fault::arm(kourier_whatsapp::fault::EVENT_WRITE, 1);
    let receipt = peer_receipt(ID, "read");
    rig.server.send(&receipt);
    assert!(
        rig.server.wait_opened(2, Duration::from_secs(30)).await,
        "no new connection after the failed write (frames {:?})",
        rig.server.frames()
    );
    assert_eq!(
        rig.frames_with("ack", ID),
        0,
        "the receipt was acknowledged before it was durable"
    );
    rig.server
        .send(&wire::success_as_the_server_writes_it(peer::ACCOUNT_LID));
    rig.online_after(before).await;
    rig.server.send(&receipt);
    assert_eq!(rig.receipts(1).await, 1);
    rig.frame_with("ack", ID).await;
    kourier_whatsapp::fault::disarm();
    let _ = rig.finish().await;
}

/// C9, rule (a) over the Wire: while the account's file is write-locked the handler's write waits,
/// and upstream sends no `<ack>` for the receipt until the row is committed. The lock is a second
/// connection's `BEGIN IMMEDIATE` (the writer waits in SQLite's busy handler, 5 s). A handler that
/// returns before its write is done acknowledges at once.
async fn no_ack_of_a_receipt_before_its_row() {
    const ID: &str = "3EB0000000000004";
    let mut rig = rig().await;
    rig.online().await;
    let holder = raw(rig.dir.path());
    holder
        .execute_batch("BEGIN IMMEDIATE")
        .expect("the write lock of the file");
    rig.server.send(&peer_receipt(ID, "read"));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        rig.frames_with("ack", ID),
        0,
        "the receipt was acknowledged while its write waited for the file"
    );
    holder.execute_batch("ROLLBACK").expect("release");
    rig.frame_with("ack", ID).await;
    assert_eq!(rig.receipts(1).await, 1, "the receipt's event arrives once");
    let _ = rig.finish().await;
}

/// The content events (a message, a reaction, an edit, a delete) of the next envelopes, one list per
/// envelope, each acknowledged as the core would; stops once `n` envelopes held some.
async fn content_envelopes(rig: &mut Rig, n: usize) -> Vec<Vec<BackendEvent>> {
    let mut got = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while got.len() < n {
        let left = deadline.saturating_duration_since(Instant::now());
        let envelope = tokio::time::timeout(left, rig.actor.ends.events.recv())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "only {} of {n} content envelopes came (frames {:?})",
                    got.len(),
                    rig.server.frames()
                )
            })
            .expect("an envelope");
        let content: Vec<BackendEvent> = envelope
            .events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    BackendEvent::Message(_)
                        | BackendEvent::Reaction { .. }
                        | BackendEvent::Edited { .. }
                        | BackendEvent::Deleted { .. }
                )
            })
            .cloned()
            .collect();
        if !content.is_empty() {
            got.push(content);
        }
        if let Some(ack) = envelope.ack {
            ack.complete(Ok(()));
        }
    }
    got
}

/// A message of the peer that the content case sends: the id, the stanza's type and the message.
fn content_messages() -> [(&'static str, &'static str, wa::Message); 5] {
    let account = format!("{}@lid", peer::ACCOUNT_LID);
    // The peer's own message as the peer's phone names it: from_me, in the chat with the account.
    let key = {
        let mut key = wa::MessageKey::default();
        key.id = Some(CONTENT_TEXT.into());
        key.from_me = Some(true);
        key.remote_jid = Some(account.clone());
        key
    };
    let mut context = wa::ContextInfo::default();
    context.mentioned_jid = vec![account];
    let mut extended = wa::message::ExtendedTextMessage::default();
    extended.text = Some(format!("synthetic *bold* @{}", peer::ACCOUNT_LID));
    extended.context_info = MessageField::some(context);
    let mut text = wa::Message::default();
    text.extended_text_message = MessageField::some(extended);

    let mut reaction = wa::message::ReactionMessage::default();
    reaction.key = MessageField::some(key.clone());
    reaction.text = Some("\u{1f99c}".into());
    let mut react = wa::Message::default();
    react.reaction_message = MessageField::some(reaction);

    let mut edit_protocol = wa::message::ProtocolMessage::default();
    edit_protocol.r#type = Some(wa::message::protocol_message::Type::MESSAGE_EDIT);
    edit_protocol.key = MessageField::some(key.clone());
    edit_protocol.edited_message = MessageField::some(wa::Message::text("synthetic edited"));
    let mut edit = wa::Message::default();
    edit.protocol_message = MessageField::some(edit_protocol);

    let mut revoke_protocol = wa::message::ProtocolMessage::default();
    revoke_protocol.r#type = Some(wa::message::protocol_message::Type::REVOKE);
    revoke_protocol.key = MessageField::some(key);
    let mut revoke = wa::Message::default();
    revoke.protocol_message = MessageField::some(revoke_protocol);

    let mut image = wa::message::ImageMessage::default();
    image.mimetype = Some("image/jpeg".into());
    image.direct_path = Some("/v/t62.7118-24/synthetic".into());
    image.media_key = Some(vec![7; 32]);
    image.file_sha256 = Some(vec![1; 32]);
    image.file_length = Some(1_000);
    image.width = Some(30);
    image.height = Some(20);
    image.caption = Some("synthetic caption".into());
    let mut picture = wa::Message::default();
    picture.image_message = MessageField::some(image);

    [
        (CONTENT_TEXT, "text", text),
        ("3EB0000000000006", "reaction", react),
        ("3EB0000000000007", "text", edit),
        ("3EB0000000000008", "text", revoke),
        ("3EB0000000000009", "media", picture),
    ]
}

/// The id of the text the content case sends (the target of its reaction, edit and revoke).
const CONTENT_TEXT: &str = "3EB0000000000005";

/// W3b (C9a), the content path end to end through the real client: a formatted text that mentions the
/// account, then a reaction to it, an edit of it, a revoke of it and a picture, each from the peer and
/// sent one at a time (one hook batch, one stage-2 step, ONE `core_outbox` row each). Each row carries
/// the event the map made of it, every inbox row ends `done`, and the child's log (checked by the
/// parent) holds none of the texts, the emoji, the numbers or the ids.
async fn content_lands_as_one_row_each_with_its_event() {
    let mut rig = rig().await;
    rig.online().await;
    let mut rows = Vec::new();
    for (id, kind, message) in content_messages() {
        let stanza = rig.peer.message_with_id(&message, id, kind).await;
        rig.server.send(&stanza);
        rows.extend(content_envelopes(&mut rig, 1).await);
    }
    let peer_id = UserId::from_parts("lid", peer::PEER_LID).expect("a user id");
    let at = 1_700_000_100_000;
    assert_eq!(
        rows.iter().map(Vec::len).collect::<Vec<_>>(),
        [1; 5],
        "one event per row: {rows:?}"
    );
    let BackendEvent::Message(text) = &rows[0][0] else {
        panic!("a message first: {:?}", rows[0]);
    };
    assert_eq!(text.conv, ConvKey::Direct(peer_id.clone()));
    assert_eq!(&*text.local, CONTENT_TEXT);
    assert!(text.mentions_me, "the text mentions the account");
    let Body::Text(shown) = &text.body else {
        panic!("a text: {:?}", text.body);
    };
    assert!(
        !shown.body.contains(peer::ACCOUNT_LID),
        "the mention is a placeholder, not the number"
    );
    assert!(
        shown.ranges.iter().any(|r| r.kind == RangeKind::Bold),
        "the markers became a bold range"
    );
    assert!(
        shown
            .ranges
            .iter()
            .any(|r| matches!(r.kind, RangeKind::Mention(_))),
        "the mention is a range"
    );
    assert!(!shown.body.contains('*'), "the markers are not in the body");
    let target = |target: &kourier_model::MsgTarget| {
        assert_eq!(target.conv, Some(ConvKey::Direct(peer_id.clone())));
        assert_eq!(&*target.local, CONTENT_TEXT);
        assert_eq!(
            target.author,
            TargetAuthor::User(peer_id.clone()),
            "the peer's own message (from_me on his phone)"
        );
    };
    let BackendEvent::Reaction {
        target: t,
        by,
        emoji,
        remove,
        at: reacted_at,
    } = &rows[1][0]
    else {
        panic!("a reaction: {:?}", rows[1]);
    };
    target(t);
    assert_eq!(by.as_ref(), Some(&peer_id));
    assert_eq!((emoji.as_str(), *remove), ("\u{1f99c}", false));
    assert_eq!(*reacted_at, at, "the server's time");
    let BackendEvent::Edited {
        target: t,
        by,
        new_local,
        body,
        ..
    } = &rows[2][0]
    else {
        panic!("an edit: {:?}", rows[2]);
    };
    target(t);
    assert_eq!(by.as_ref(), Some(&peer_id));
    assert_eq!(new_local.as_deref(), Some("3EB0000000000007"));
    assert!(matches!(body, Body::Text(t) if t.body == "synthetic edited"));
    let BackendEvent::Deleted { target: t, by, .. } = &rows[3][0] else {
        panic!("a delete: {:?}", rows[3]);
    };
    target(t);
    assert_eq!(by.as_ref(), Some(&peer_id));
    let BackendEvent::Message(picture) = &rows[4][0] else {
        panic!("a picture: {:?}", rows[4]);
    };
    assert_eq!(&*picture.local, "3EB0000000000009");
    let Body::Media {
        attachments,
        caption,
    } = &picture.body
    else {
        panic!("a media message: {:?}", picture.body);
    };
    assert_eq!(attachments.len(), 1, "one picture, as a pointer");
    assert!(caption.is_some());
    let conn = rig.finish().await;
    assert_eq!(count(&conn, "SELECT count(*) FROM inbox"), 5);
    assert_eq!(
        count(&conn, "SELECT count(*) FROM inbox WHERE state = 'done'"),
        5,
        "every row is mapped, none is held back"
    );
    assert_eq!(
        count(&conn, "SELECT count(*) FROM core_outbox"),
        0,
        "acked rows are gone"
    );
}

/// What the logs and the output of the cases must never hold (decision D10): the fixture's numbers,
/// LIDs, name, texts, emoji and message ids.
pub(crate) fn secrets() -> Vec<&'static str> {
    vec![
        peer::ACCOUNT_PN,
        peer::ACCOUNT_LID,
        peer::PEER_LID,
        "Synthetic",
        "synthetic ",
        "3EB000000000000",
        // The reaction of the content case.
        "\u{1f99c}",
    ]
}

/// The case the child runs (set by the parent).
const CASE: &str = "WA_RECEIVE_CASE";

/// One parent per case: the case runs in the offline child below, and nothing the child printed holds
/// the fixture's number, LID, name or texts (D10).
fn parent(case: &str) {
    let output =
        support::offline::run_child_output(module_path!(), "receive_child", &[(CASE, case)]);
    for secret in secrets() {
        assert!(
            !output.contains(secret),
            "the child printed one of the fixture's secrets"
        );
    }
}

#[test]
fn one_pkmsg_lands_as_one_done_row_and_one_event_over_the_wire() {
    parent("pkmsg");
}

#[test]
fn a_failed_commit_sends_no_receipt_and_the_redelivery_lands_once_over_the_wire() {
    parent("commit");
}

#[test]
fn a_failed_receipt_write_withholds_the_ack_and_the_redelivery_is_acked_over_the_wire() {
    parent("write");
}

#[test]
fn no_ack_of_a_receipt_before_its_row_over_the_wire() {
    parent("park");
}

#[test]
fn content_lands_as_one_row_each_with_its_event_over_the_wire() {
    parent("content");
}

/// The child (a no-op as an ordinary test): the case the parent named. Every log line of the
/// process is recorded, and the case may have made none that holds a secret of the fixture.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receive_child() {
    if !support::offline::in_child() {
        return;
    }
    let log = Capture::default();
    log.install();
    let case = std::env::var(CASE).expect("the parent names the case");
    eprintln!("case {case}");
    match case.as_str() {
        "pkmsg" => one_pkmsg_lands_as_one_done_row_and_one_event().await,
        "commit" => a_failed_commit_sends_no_receipt_and_the_redelivery_lands_once().await,
        "write" => a_failed_receipt_write_withholds_the_ack_and_the_redelivery_is_acked().await,
        "park" => no_ack_of_a_receipt_before_its_row().await,
        "content" => content_lands_as_one_row_each_with_its_event().await,
        other => panic!("unknown case {other}"),
    }
    // Every case logs at least the connection's states: a check that saw no line proves nothing. The
    // same for upstream's own records (fix F6): the `log` bridge is installed and heard some, so the
    // grep below covers what whatsapp-rust writes, not only what Kourier does.
    assert!(
        log.own_lines() >= 1,
        "the recorder saw {} lines of the crate",
        log.own_lines()
    );
    assert!(
        log.upstream_records() >= 1,
        "the recorder heard no record of upstream's crates: the `log` bridge is not working"
    );
    assert_eq!(
        log.leaks(&secrets()),
        Vec::<String>::new(),
        "a log line holds a secret of the fixture (the targets are named)"
    );
}
