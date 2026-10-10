//! S13 through the real core against the mock Signal server (docs/architecture.md §3.5, §4.8
//! step 7, §6.7, §6.13, §11 S13): what the user does in the timeline (edit, delete, react,
//! reply with a quote, change the timer) reaches the contact and our phone, and what they do
//! shows in the timeline.
//!
//! - [`edit_delete_react_and_quote_through_the_core`]: every action both ways in a 1:1 chat,
//!   and an edit and a reaction over sender keys in a group.
//! - [`modifiers_apply_whatever_arrives_first_and_late_edits_are_ignored`]: an edit, a reaction
//!   and a delete that arrive before their target apply when it arrives (and the other way
//!   round); an edit more than 48 hours after its message changes nothing.
//! - [`a_hostile_member_cannot_touch_someone_elses_message`]: an edit or delete of another
//!   member's message and a reaction naming a wrong author leave that message as it was; so
//!   does a modifier with the right author and timestamp that is sent in another chat (her
//!   own edit, delete and reaction sent 1:1 for her group message, and a delete and a
//!   reaction sent in the group for her 1:1 message).
//! - A message sent to a number
//!   ([`a_reaction_and_a_receipt_from_the_account_find_a_message_sent_to_the_number`]): our
//!   phone wrote to a contact it knows by number only (a chat keyed by their PNI); their
//!   reaction and delivery receipt come from their ACI and name a chat the message is not in.
//!   Once the backend learns that the PNI and the ACI are one contact (the phone's contact
//!   record carries both; or, for a second contact, a sealed sender's certificate states the
//!   number the phone's record gives the PNI), it tells the core, the two chats become one,
//!   and the reaction and the receipt land on the message.
//! - [`a_timer_set_from_either_side_expires_messages_on_time`]: short real timers; received
//!   messages go after they were read (here or on the phone), own ones after they were sent.
//! - [`an_edit_and_a_reaction_are_sent_once_across_a_kill`]: the app is killed between the
//!   backend's encrypt commit and the request; the next start sends each exactly once.
//!
//! Skips without Node ≥ 24 or the harness's `node_modules` (`KOURIER_REQUIRE_MOCK_HARNESS=1`
//! fails instead).

mod support;

use std::os::unix::process::ExitStatusExt as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use kourier_backend_api::{BackendEnv, InitialSync};
use kourier_core::facade::{
    ConversationListQuery, MessageRow, Query, QueryData, QuoteState, UiCommand, UiEvent,
};
use kourier_model::{
    AccountId, Body, ConvKind, ConversationId, Deletion, MessageId, OutgoingState, SystemEvent,
    Text, UserId,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use support::harness::{
    CreateGroup, CreatePrimary, DeviceInfo, GroupInfo, Harness, Primary, StorageContact,
    StorageGroup, StorageSpec,
};
use support::{App, WAIT, World, init_tracing, local_env, user};
use tokio::io::{AsyncBufReadExt as _, BufReader};

/// How long a test waits to be sure something did not arrive.
const QUIET: Duration = Duration::from_millis(1500);

/// A binary ACI as the harness prints it (base64 of the 16 uuid bytes).
fn aci_b64(aci: &str) -> String {
    let hex: String = aci.chars().filter(char::is_ascii_hexdigit).collect();
    let bytes: Vec<u8> = (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex"))
        .collect();
    STANDARD.encode(bytes)
}

fn now_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(0)
}

fn text_of(row: &MessageRow) -> Option<&str> {
    match &row.body {
        Body::Text(text) => Some(text.body.as_str()),
        _ => None,
    }
}

/// The mock with our phone, three contacts the phone has accepted, and their group.
struct Club {
    world: World,
    members: Vec<Primary>,
    group: GroupInfo,
}

impl Club {
    async fn start() -> Option<Self> {
        init_tracing();
        let harness = Harness::start_or_skip().await.expect("mock harness")?;
        let tmp = tempfile::tempdir().expect("temp dir");
        let env = local_env(harness.handshake(), tmp.path());
        let mut members = Vec::new();
        for name in ["Ada Lindqvist", "Bruno Okafor", "Chiara Rossi"] {
            members.push(
                harness
                    .create_primary(&CreatePrimary::named(name))
                    .await
                    .expect("member"),
            );
        }
        let phone = harness
            .create_primary(&CreatePrimary {
                contacts: members.iter().map(|m| m.device_ref.clone()).collect(),
                ..CreatePrimary::named("Myself")
            })
            .await
            .expect("phone");
        let group = harness
            .create_group(&CreateGroup {
                from: phone.device_ref.clone(),
                title: "Climbing club".to_owned(),
                members: members.iter().map(|m| m.device_ref.clone()).collect(),
                ..CreateGroup::default()
            })
            .await
            .expect("create group");
        harness
            .set_storage_state(&StorageSpec {
                primary: phone.device_ref.clone(),
                contacts: members
                    .iter()
                    .map(|m| StorageContact::of(m.device_ref.clone()))
                    .collect(),
                groups: vec![StorageGroup {
                    id: group.id.clone(),
                    ..StorageGroup::default()
                }],
                ..StorageSpec::default()
            })
            .await
            .expect("storage state");
        let alice = members[0].clone();
        Some(Self {
            world: World {
                harness,
                tmp,
                env,
                phone,
                alice,
            },
            members,
            group,
        })
    }

    fn ada(&self) -> &Primary {
        &self.members[0]
    }

    /// The app over the test profile, as the real app starts it.
    async fn app(&self) -> App {
        App::start_with(
            self.world.tmp.path(),
            &self.world.env,
            InitialSync::ContactsAndStorage,
        )
        .await
    }

    /// `sendData` with delivery awaited; returns the timestamp.
    async fn send(&self, mut args: Value) -> u64 {
        args["awaitDelivery"] = json!(true);
        args["timeout"] = json!(60_000);
        self.world
            .harness
            .send_data(args)
            .await
            .expect("sendData")
            .timestamp
    }

    /// A text from `from` to our device.
    async fn says(&self, from: &Primary, device: &DeviceInfo, text: &str) -> u64 {
        self.send(json!({
            "from": from.aci, "to": device.device_ref, "text": text, "withProfileKey": true,
        }))
        .await
    }

    /// A text from `from` in the group, over sender keys.
    async fn says_in_group(&self, from: &Primary, device: &DeviceInfo, text: &str) -> u64 {
        self.send(json!({
            "from": from.aci, "to": [device.device_ref], "group": self.group.id,
            "senderKey": true, "text": text,
        }))
        .await
    }

    /// The 1:1 chat with `peer`, once the core can send there.
    async fn chat(&self, app: &App, account: AccountId, peer: &Primary) -> ConversationId {
        let conv = app.conversation_with(account, &user(peer)).await;
        app.wait_cannot_send(conv, None).await;
        conv
    }

    /// Waits until the group is in the core and writable; returns its conversation.
    async fn group_conv(&self, app: &App, account: AccountId) -> ConversationId {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let query = ConversationListQuery {
                account: Some(account),
                limit: 100,
                ..ConversationListQuery::default()
            };
            let rows = match app
                .handle
                .query(Query::Conversations(query))
                .await
                .expect("conversations")
            {
                QueryData::Conversations(page) => page.rows,
                other => panic!("unexpected {other:?}"),
            };
            if let Some(row) = rows.into_iter().find(|r| {
                matches!(r.kind, ConvKind::Group) && r.title.as_deref() == Some("Climbing club")
            }) {
                let features = app.features(row.id).await;
                if features.max_text_bytes.is_some() && features.cannot_send.is_none() {
                    return row.id;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the group never became writable"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The next sync transcript (`sent`) the phone got from our device.
    async fn transcript(&self, device: &DeviceInfo) -> Value {
        let sync = self
            .world
            .harness
            .wait_for_sync_message(
                &self.world.phone.aci,
                Some(&device.device_ref),
                "sent",
                WAIT,
            )
            .await
            .expect("sync transcript");
        sync.sync_message["content"]["sent"].clone()
    }

    /// Asserts `who` gets no further message or edit from our device for a while.
    async fn nothing_more(&self, who: &Primary, device: &DeviceInfo) {
        let harness = &self.world.harness;
        let message = harness
            .wait_for_message(&who.aci, Some(&device.device_ref), QUIET)
            .await;
        assert!(message.is_err(), "another message: {message:?}");
        let edit = harness
            .wait_for_edit(
                &who.aci,
                Some(&device.device_ref),
                None,
                Duration::from_millis(100),
            )
            .await;
        assert!(edit.is_err(), "another edit: {edit:?}");
    }
}

/// The row with id `id`, if the timeline still has it.
async fn row_by_id(app: &App, conv: ConversationId, id: MessageId) -> Option<MessageRow> {
    app.rows(conv).await.into_iter().find(|row| row.id == id)
}

/// The conversation's disappearing-message timer.
async fn timer_of(app: &App, conv: ConversationId) -> Option<Duration> {
    match app
        .handle
        .query(Query::Conversation(conv))
        .await
        .expect("conversation")
    {
        QueryData::Conversation(Some(detail)) => detail.timer,
        other => panic!("unexpected {other:?}"),
    }
}

/// Sends `text` in `conv` and returns its row once the network took it.
async fn send_text(
    app: &App,
    conv: ConversationId,
    text: &str,
    reply_to: Option<MessageId>,
) -> MessageRow {
    app.send(UiCommand::SendText {
        conv,
        text: Text::plain(text),
        reply_to,
    });
    app.wait_row(conv, text, "sent", |row| {
        matches!(
            row.out_state,
            Some(OutgoingState::Sent { .. } | OutgoingState::Delivered | OutgoingState::Read)
        )
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn edit_delete_react_and_quote_through_the_core() {
    let Some(club) = Club::start().await else {
        return;
    };
    let mut app = club.app().await;
    let (account, device) = app.link(&club.world).await;
    let ada = club.ada();
    let ada_user = user(ada);
    let harness = &club.world.harness;
    let me = &device.device_ref;

    let ping = club.says(ada, &device, "ping").await;
    let conv = club.chat(&app, account, ada).await;
    let ping_row = app.wait_text(conv, "ping").await.remove(0);
    // The real features let the user do all of it (§3.8).
    assert!(
        ping_row.actions.reply && ping_row.actions.react,
        "{:?}",
        ping_row.actions
    );
    assert!(!ping_row.actions.edit, "not our message");

    // -- our message, then its edit
    let sent = send_text(&app, conv, "pnog", None).await;
    assert!(
        sent.actions.edit && sent.actions.delete_for_everyone,
        "{:?}",
        sent.actions
    );
    let got = harness
        .wait_for_data(&ada.aci, "message", Some(me), None, WAIT)
        .await
        .expect("Ada got it");
    assert_eq!(got.text, "pnog");
    let original_at = got.timestamp;
    club.transcript(&device).await;
    app.send(UiCommand::EditMessage {
        message: sent.id,
        text: Text::plain("pong"),
    });
    let edited = app
        .wait_row(conv, "pong", "edited", |row| row.revision == 1)
        .await;
    assert_eq!(edited.id, sent.id, "the same message");
    assert!(edited.edited_at.is_some());
    let edit = harness
        .wait_for_edit(&ada.aci, Some(me), None, WAIT)
        .await
        .expect("Ada got the edit");
    assert_eq!(edit.target_timestamp, original_at);
    assert_eq!(edit.text, "pong");
    assert!(edit.sealed);
    let transcript = club.transcript(&device).await;
    assert_eq!(
        transcript["editMessage"]["targetSentTimestamp"],
        original_at
    );
    assert_eq!(transcript["editMessage"]["dataMessage"]["body"], "pong");

    // -- a reaction to her message, replaced by another (one per user), then removed
    let react = |emoji: &str, remove: bool| UiCommand::React {
        message: ping_row.id,
        emoji: emoji.to_owned(),
        remove,
    };
    app.send(react("👍", false));
    let reaction = harness
        .wait_for_reaction(&ada.aci, Some(me), WAIT)
        .await
        .expect("Ada got the reaction");
    assert_eq!(reaction.data_message["reaction"]["emoji"], "👍");
    assert_eq!(
        reaction.data_message["reaction"]["targetSentTimestamp"],
        ping
    );
    assert_eq!(
        reaction.data_message["reaction"]["targetAuthorAciBinary"],
        aci_b64(&ada.aci)
    );
    assert_eq!(
        club.transcript(&device).await["message"]["reaction"]["emoji"],
        "👍"
    );
    let row = app
        .wait_row(conv, "ping", "our reaction", |row| {
            row.reactions.iter().any(|r| r.emoji == "👍" && r.by_me())
        })
        .await;
    assert_eq!(row.reactions.len(), 1);
    app.send(react("👍", true));
    let removal = harness
        .wait_for_reaction(&ada.aci, Some(me), WAIT)
        .await
        .expect("Ada got the removal");
    assert_eq!(removal.data_message["reaction"]["remove"], true);
    club.transcript(&device).await;
    app.wait_row(conv, "ping", "reaction removed", |row| {
        row.reactions.is_empty()
    })
    .await;

    // -- a reply that quotes her message
    let reply = send_text(&app, conv, "pong, I said", Some(ping_row.id)).await;
    let preview = reply.reply.as_ref().expect("our reply shows its quote");
    assert_eq!(preview.state, QuoteState::Known);
    assert_eq!(preview.target, Some(ping_row.id));
    let got = harness
        .wait_for_data(&ada.aci, "message", Some(me), None, WAIT)
        .await
        .expect("Ada got the reply");
    let quote = &got.data_message["quote"];
    assert_eq!(quote["id"], ping, "{quote}");
    assert_eq!(quote["authorAciBinary"], aci_b64(&ada.aci));
    assert_eq!(quote["text"], "ping");
    assert_eq!(
        club.transcript(&device).await["message"]["quote"]["id"],
        ping
    );

    // -- delete for everyone: a tombstone here, the delete at Ada's and on the phone
    app.send(UiCommand::DeleteMessage {
        message: sent.id,
        for_everyone: true,
    });
    let delete = harness
        .wait_for_delete(&ada.aci, Some(me), WAIT)
        .await
        .expect("Ada got the delete");
    assert_eq!(
        delete.data_message["delete"]["targetSentTimestamp"], original_at,
        "the message's own id"
    );
    let transcript = club.transcript(&device).await;
    assert_eq!(
        transcript["message"]["delete"]["targetSentTimestamp"],
        original_at
    );
    app.wait_row_where(conv, "our tombstone", |row| {
        row.id == sent.id && matches!(row.deleted, Some(Deletion::ForEveryone { by: None }))
    })
    .await;

    // -- delete for me: only the phone hears of it
    app.send(UiCommand::DeleteMessage {
        message: ping_row.id,
        for_everyone: false,
    });
    let sync = harness
        .wait_for_sync_message(&club.world.phone.aci, Some(me), "deleteForMe", WAIT)
        .await
        .expect("deleteForMe sync");
    let deletes = &sync.sync_message["content"]["deleteForMe"]["messageDeletes"];
    assert_eq!(
        deletes[0]["messages"][0]["sentTimestamp"], ping,
        "{deletes}"
    );
    assert!(
        deletes.to_string().contains(&aci_b64(&ada.aci)),
        "{deletes}"
    );
    club.nothing_more(ada, &device).await;

    // -- the other direction: her message, her edit, her reaction to ours, her quote, her delete
    let hers = club.says(ada, &device, "see you at 5").await;
    let her_row = app.wait_text(conv, "see you at 5").await.remove(0);
    club.send(json!({
        "from": ada.aci, "to": me, "text": "see you at 6", "edit": { "targetTimestamp": hers },
    }))
    .await;
    let edited = app
        .wait_row(conv, "see you at 6", "her edit", |row| row.revision == 1)
        .await;
    assert_eq!(edited.id, her_row.id);
    club.send(json!({
        "from": ada.aci, "to": me, "sealed": true,
        "reaction": { "emoji": "🎉", "targetAuthor": club.world.phone.aci,
                      "targetTimestamp": reply.sent_at },
    }))
    .await;
    app.wait_row(conv, "pong, I said", "her reaction", |row| {
        row.reactions
            .iter()
            .any(|r| r.emoji == "🎉" && r.users.contains(&Some(ada_user.clone())))
    })
    .await;
    club.send(json!({
        "from": ada.aci, "to": me, "text": "what did you say?",
        "quote": { "id": reply.sent_at, "author": club.world.phone.aci, "text": "pong, I said" },
    }))
    .await;
    let quoting = app.wait_text(conv, "what did you say?").await.remove(0);
    let preview = quoting.reply.expect("her quote");
    assert_eq!(preview.state, QuoteState::Known);
    assert_eq!(preview.target, Some(reply.id));
    assert!(preview.own, "she quoted our message");
    club.send(json!({ "from": ada.aci, "to": me, "delete": { "targetTimestamp": hers } }))
        .await;
    app.wait_row_where(conv, "her tombstone", |row| {
        row.id == her_row.id
            && matches!(&row.deleted, Some(Deletion::ForEveryone { by: Some(by) }) if *by == ada_user)
    })
    .await;

    // -- the group: our message, its edit and a reaction go out as sender-key messages
    let group = club.group_conv(&app, account).await;
    let ours = send_text(&app, group, "Who brings the rope?", None).await;
    for member in &club.members {
        harness
            .wait_for_data(&member.aci, "message", Some(me), Some(&club.group.id), WAIT)
            .await
            .expect("group message");
    }
    club.transcript(&device).await;
    app.send(UiCommand::EditMessage {
        message: ours.id,
        text: Text::plain("Who brings the rope and the chalk?"),
    });
    for member in &club.members {
        let edit = harness
            .wait_for_edit(&member.aci, Some(me), Some(&club.group.id), WAIT)
            .await
            .expect("group edit");
        assert_eq!(edit.message_type, "SenderKey");
        assert_eq!(edit.text, "Who brings the rope and the chalk?");
    }
    let in_group = club.says_in_group(ada, &device, "I do").await;
    let her_row = app.wait_text(group, "I do").await.remove(0);
    app.send(UiCommand::React {
        message: her_row.id,
        emoji: "🙌".to_owned(),
        remove: false,
    });
    for member in &club.members {
        let reaction = harness
            .wait_for_data(
                &member.aci,
                "reaction",
                Some(me),
                Some(&club.group.id),
                WAIT,
            )
            .await
            .expect("group reaction");
        assert_eq!(reaction.message_type, "SenderKey");
        assert_eq!(
            reaction.data_message["reaction"]["targetSentTimestamp"],
            in_group
        );
        assert_eq!(
            reaction.data_message["reaction"]["targetAuthorAciBinary"],
            aci_b64(&ada.aci)
        );
    }
    // Nothing was refused on the way.
    while let Ok(event) = app.events.try_recv() {
        assert!(
            !matches!(event, UiEvent::ActionFailed { .. }),
            "an action failed: {event:?}"
        );
    }
    app.stop().await;
    club.world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn modifiers_apply_whatever_arrives_first_and_late_edits_are_ignored() {
    let Some(club) = Club::start().await else {
        return;
    };
    let mut app = club.app().await;
    let (account, device) = app.link(&club.world).await;
    let ada = club.ada();
    let ada_user = user(ada);
    let me = &device.device_ref;
    club.says(ada, &device, "hello").await;
    let conv = club.chat(&app, account, ada).await;
    app.wait_text(conv, "hello").await;

    // -- modifiers first: an edit and a reaction for a message that is still on its way
    let later = now_ms() + 5;
    club.send(json!({
        "from": ada.aci, "to": me, "timestamp": later + 2,
        "reaction": { "emoji": "🚀", "targetAuthor": ada.aci, "targetTimestamp": later },
    }))
    .await;
    let edit_at = club
        .send(json!({
            "from": ada.aci, "to": me, "timestamp": later + 3, "text": "the edited text",
            "edit": { "targetTimestamp": later },
        }))
        .await;
    // A reaction to the revision the edit made, also before anything is here.
    club.send(json!({
        "from": ada.aci, "to": me, "timestamp": later + 4,
        "reaction": { "emoji": "🚀", "targetAuthor": ada.aci, "targetTimestamp": edit_at },
    }))
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        app.rows_with_text(conv, "the edited text").await.is_empty(),
        "nothing to show yet"
    );
    club.send(json!({ "from": ada.aci, "to": me, "timestamp": later, "text": "the first text" }))
        .await;
    let row = app
        .wait_row(
            conv,
            "the edited text",
            "the edit applied on arrival",
            |row| row.revision == 1 && !row.reactions.is_empty(),
        )
        .await;
    assert_eq!(row.sent_at, i64::try_from(later).unwrap());
    assert_eq!(row.reactions.len(), 1);
    assert_eq!(row.reactions[0].users, [Some(ada_user.clone())]);
    assert!(app.rows_with_text(conv, "the first text").await.is_empty());

    // -- a delete before its target: the message arrives as a tombstone
    let gone = now_ms() + 20;
    club.send(json!({
        "from": ada.aci, "to": me, "timestamp": gone + 1, "delete": { "targetTimestamp": gone },
    }))
    .await;
    club.send(json!({ "from": ada.aci, "to": me, "timestamp": gone, "text": "never shown" }))
        .await;
    app.wait_row_where(conv, "a tombstone on arrival", |row| {
        row.sent_at == i64::try_from(gone).unwrap()
            && matches!(row.deleted, Some(Deletion::ForEveryone { .. }))
    })
    .await;
    assert!(app.rows_with_text(conv, "never shown").await.is_empty());

    // -- the usual order gives the same
    let first = club.says(ada, &device, "in order").await;
    app.wait_text(conv, "in order").await;
    club.send(json!({
        "from": ada.aci, "to": me,
        "reaction": { "emoji": "👌", "targetAuthor": ada.aci, "targetTimestamp": first },
    }))
    .await;
    club.send(json!({
        "from": ada.aci, "to": me, "text": "in order, edited", "edit": { "targetTimestamp": first },
    }))
    .await;
    app.wait_row(conv, "in order, edited", "edit and reaction", |row| {
        row.revision == 1 && row.reactions.len() == 1
    })
    .await;

    // -- an edit more than 48 hours after its message is ignored; one within is not
    let hour = 3_600_000u64;
    let old = now_ms() - 49 * hour;
    let recent = now_ms() - 47 * hour;
    // The window runs from the server's time of the message to the server's time of the edit
    // (the sender's own dates do not count), so the mock stamps these two as old as they say.
    club.send(json!({
        "from": ada.aci, "to": me, "timestamp": old, "serverTimestamp": old,
        "text": "two days old",
    }))
    .await;
    club.send(json!({
        "from": ada.aci, "to": me, "timestamp": recent, "serverTimestamp": recent,
        "text": "almost two days old",
    }))
    .await;
    app.wait_text(conv, "two days old").await;
    app.wait_text(conv, "almost two days old").await;
    club.send(json!({
        "from": ada.aci, "to": me, "text": "rewritten history", "edit": { "targetTimestamp": old },
    }))
    .await;
    club.send(json!({
        "from": ada.aci, "to": me, "text": "a late correction", "edit": { "targetTimestamp": recent },
    }))
    .await;
    // The second edit was sent after the first: once it shows, the first was handled too.
    app.wait_row(
        conv,
        "a late correction",
        "the edit within the window",
        |row| row.revision == 1,
    )
    .await;
    let untouched = app.wait_text(conv, "two days old").await.remove(0);
    assert_eq!(untouched.revision, 0, "an edit after 48 h is ignored");
    assert!(
        app.rows_with_text(conv, "rewritten history")
            .await
            .is_empty()
    );
    // The sender cannot date the edit back into the window: the server's time counts.
    club.send(json!({
        "from": ada.aci, "to": me, "timestamp": old + hour, "text": "backdated",
        "edit": { "targetTimestamp": old },
    }))
    .await;
    let marker = club.says(ada, &device, "after the backdated edit").await;
    app.wait_text(conv, "after the backdated edit").await;
    assert!(marker > 0);
    assert!(app.rows_with_text(conv, "backdated").await.is_empty());
    app.stop().await;
    club.world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hostile_member_cannot_touch_someone_elses_message() {
    let Some(club) = Club::start().await else {
        return;
    };
    let mut app = club.app().await;
    let (account, device) = app.link(&club.world).await;
    let [ada, bruno, _] = &club.members[..] else {
        panic!("three members");
    };
    let me = &device.device_ref;
    let group = club.group_conv(&app, account).await;
    let hers = club.says_in_group(ada, &device, "The summit is at 8").await;
    let row = app.wait_text(group, "The summit is at 8").await.remove(0);
    let in_group = |mut args: Value| {
        args["from"] = json!(bruno.aci);
        args["to"] = json!([me]);
        args["group"] = json!(club.group.id);
        args["senderKey"] = json!(true);
        args
    };

    // Bruno edits and deletes "her" message, and reacts to it naming himself as its author.
    club.send(in_group(json!({
        "text": "The summit is cancelled", "edit": { "targetTimestamp": hers },
    })))
    .await;
    club.send(in_group(json!({ "delete": { "targetTimestamp": hers } })))
        .await;
    club.send(in_group(json!({
        "reaction": { "emoji": "😈", "targetAuthor": bruno.aci, "targetTimestamp": hers },
    })))
    .await;
    // An honest message after them: once it shows, the three were handled.
    club.says_in_group(bruno, &device, "just kidding").await;
    app.wait_text(group, "just kidding").await;

    let after = row_by_id(&app, group, row.id)
        .await
        .expect("her message is still there");
    assert_eq!(text_of(&after), Some("The summit is at 8"), "not edited");
    assert_eq!(after.revision, 0);
    assert_eq!(after.deleted, None, "not deleted");
    assert!(after.reactions.is_empty(), "no reaction landed on it");
    assert!(
        app.rows_with_text(group, "The summit is cancelled")
            .await
            .is_empty()
    );

    // An honest reaction of his does land, and the 1:1 chats are no back door either: the same
    // edit sent 1:1 names another conversation and another author.
    club.send(in_group(json!({
        "reaction": { "emoji": "👍", "targetAuthor": ada.aci, "targetTimestamp": hers },
    })))
    .await;
    app.wait_row(group, "The summit is at 8", "his honest reaction", |row| {
        row.reactions.len() == 1
    })
    .await;
    club.send(json!({
        "from": bruno.aci, "to": me, "text": "The summit is cancelled",
        "edit": { "targetTimestamp": hers },
    }))
    .await;
    club.says_in_group(bruno, &device, "really").await;
    app.wait_text(group, "really").await;
    let after = row_by_id(&app, group, row.id).await.expect("still there");
    assert_eq!(text_of(&after), Some("The summit is at 8"));

    // --- The right author and the right timestamp, in the wrong chat. Ada herself sends an
    // edit, a delete and a reaction for her group message in her 1:1 chat with us: a
    // modifier belongs to the conversation it is sent in, so none of them touches the group
    // message (Desktop: `isMessageAMatchForReaction` and the conversation checks of its edit
    // and delete handlers).
    club.says(ada, &device, "about Saturday").await;
    let direct = club.chat(&app, account, ada).await;
    app.wait_text(direct, "about Saturday").await;
    club.send(json!({
        "from": ada.aci, "to": me, "text": "The summit moved to 9",
        "edit": { "targetTimestamp": hers },
    }))
    .await;
    club.send(json!({ "from": ada.aci, "to": me, "delete": { "targetTimestamp": hers } }))
        .await;
    club.send(json!({
        "from": ada.aci, "to": me,
        "reaction": { "emoji": "🙃", "targetAuthor": ada.aci, "targetTimestamp": hers },
    }))
    .await;
    // And the other way round: a delete and a reaction sent in the group for a message of
    // her 1:1 chat.
    let private = club.says(ada, &device, "between us").await;
    let private_row = app.wait_text(direct, "between us").await.remove(0);
    club.send(json!({
        "from": ada.aci, "to": [me], "group": club.group.id, "senderKey": true,
        "delete": { "targetTimestamp": private },
    }))
    .await;
    club.send(in_group(json!({
        "reaction": { "emoji": "👀", "targetAuthor": ada.aci, "targetTimestamp": private },
    })))
    .await;
    // Honest messages in both chats after them: once they show, all five were handled.
    club.says(ada, &device, "see you there").await;
    app.wait_text(direct, "see you there").await;
    club.says_in_group(ada, &device, "bring the rope").await;
    app.wait_text(group, "bring the rope").await;
    let after = row_by_id(&app, group, row.id)
        .await
        .expect("her group message is still there");
    assert_eq!(
        text_of(&after),
        Some("The summit is at 8"),
        "an edit sent in another chat does not edit it"
    );
    assert_eq!(after.revision, 0);
    assert_eq!(after.deleted, None, "nor does a delete");
    assert_eq!(
        after.reactions.len(),
        1,
        "only Bruno's honest reaction: {:?}",
        after.reactions
    );
    let after = row_by_id(&app, direct, private_row.id)
        .await
        .expect("her 1:1 message is still there");
    assert_eq!(text_of(&after), Some("between us"));
    assert_eq!(
        after.deleted, None,
        "a delete sent in the group does not reach it"
    );
    assert!(after.reactions.is_empty(), "{:?}", after.reactions);
    for chat in [group, direct] {
        assert!(
            app.rows_with_text(chat, "The summit moved to 9")
                .await
                .is_empty(),
            "the edit shows nowhere"
        );
    }
    app.stop().await;
    club.world.shutdown().await;
}

/// The rows of `conv` whose text is `text`, once there is one that satisfies `ready`.
async fn wait_ready(
    app: &App,
    conv: ConversationId,
    text: &str,
    what: &str,
    ready: impl Fn(&MessageRow) -> bool,
) -> MessageRow {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let rows = app.rows_with_text(conv, text).await;
        if let Some(row) = rows.iter().find(|row| ready(row)) {
            return row.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what}: {:?}",
            rows.iter()
                .map(|r| (r.out_state.clone(), r.reactions.len()))
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reaction_and_a_receipt_from_the_account_find_a_message_sent_to_the_number() {
    let Some(world) = World::start().await else {
        return;
    };
    let harness = &world.harness;
    let bob = harness
        .create_primary(&CreatePrimary::named("Bob"))
        .await
        .expect("Bob");
    let by_number = |who: &Primary| {
        let pni = who.pni.clone().expect("a number");
        UserId::parse(&format!(
            "pni:{}",
            pni.trim_start_matches("PNI:").to_lowercase()
        ))
        .expect("PNI user id")
    };
    // What the phone's contact list says: Bob always by his number only; Alice by her number
    // only at first, later with her account joined to it.
    let contacts = |alice_joined: bool| StorageSpec {
        primary: world.phone.device_ref.clone(),
        contacts: vec![
            StorageContact {
                system_given_name: Some("Alice from the club".to_owned()),
                by_pni: !alice_joined,
                with_pni: alice_joined,
                ..StorageContact::of(world.alice.device_ref.clone())
            },
            StorageContact {
                system_given_name: Some("Bob the plumber".to_owned()),
                by_pni: true,
                ..StorageContact::of(bob.device_ref.clone())
            },
        ],
        ..StorageSpec::default()
    };
    harness
        .set_storage_state(&contacts(false))
        .await
        .expect("storage state");
    let mut app = App::start_with(
        world.tmp.path(),
        &world.env,
        InitialSync::ContactsAndStorage,
    )
    .await;
    let (account, device) = app.link(&world).await;
    let me = device.device_ref.clone();
    // The phone writes to a number: the transcript names the PNI, and so does the chat.
    let phone_writes = |to: &Primary, text: &'static str| {
        let args = json!({
            "from": world.phone.device_ref, "to": me, "destination": to.aci,
            "destinationKind": "PNI", "text": text, "awaitDelivery": true, "timeout": 60_000,
        });
        async move {
            harness
                .request_value("sendSyncSent", args)
                .await
                .expect("sendSyncSent")["timestamp"]
                .as_u64()
                .expect("timestamp")
        }
    };
    let delivered = |row: &MessageRow| row.out_state == Some(OutgoingState::Delivered);

    // --- Alice: the phone's contact record joins her number and her account.
    let sent_at = phone_writes(&world.alice, "Are you coming on Saturday?").await;
    let number_chat = app
        .conversation_with(account, &by_number(&world.alice))
        .await;
    let row = app
        .wait_text(number_chat, "Are you coming on Saturday?")
        .await
        .remove(0);
    assert!(
        matches!(row.origin, kourier_core::facade::OriginKind::OwnOtherDevice),
        "our own message, sent from the phone: {:?}",
        row.origin
    );
    // She answers from her account: a delivery receipt, a reaction to our message and a
    // text. They name the chat with her ACI, where the message is not.
    harness
        .send_receipt(&world.alice.aci, &me, "delivery", &[sent_at], false)
        .await
        .expect("delivery receipt");
    harness
        .send_data(json!({
            "from": world.alice.aci, "to": me, "awaitDelivery": true, "timeout": 60_000,
            "reaction": {
                "emoji": "👍", "targetAuthor": world.phone.aci, "targetTimestamp": sent_at,
            },
        }))
        .await
        .expect("reaction");
    world.alice_says(&device, "Yes, with Bruno").await;
    let account_chat = app.conversation_with(account, &user(&world.alice)).await;
    app.wait_text(account_chat, "Yes, with Bruno").await;
    assert_ne!(
        account_chat, number_chat,
        "two chats until the ids are joined"
    );
    let before = row_by_id(&app, number_chat, row.id)
        .await
        .expect("the message");
    assert!(before.reactions.is_empty(), "the reaction waits");
    assert!(
        !delivered(&before),
        "so does the receipt: {:?}",
        before.out_state
    );

    // The phone joins the contact (its record now carries the ACI and the PNI) and says so.
    harness
        .set_storage_state(&contacts(true))
        .await
        .expect("storage state");
    harness
        .send_fetch_latest(&world.phone.device_ref, &me, "storageManifest")
        .await
        .expect("fetch latest");
    // One chat: both messages are in the chat with her account, and the reaction and the
    // receipt that waited found the message.
    let joined = wait_ready(
        &app,
        account_chat,
        "Are you coming on Saturday?",
        "the message with its reaction and receipt",
        |row| !row.reactions.is_empty() && delivered(row),
    )
    .await;
    assert_eq!(joined.reactions.len(), 1);
    assert_eq!(
        app.rows_with_text(account_chat, "Yes, with Bruno")
            .await
            .len(),
        1
    );

    // --- Bob: nobody tells us, but his sealed receipt comes with the server's certificate
    // of his account, which states his number, the number the phone's record gives his PNI.
    let sent_at = phone_writes(&bob, "The tap is dripping again").await;
    let number_chat = app.conversation_with(account, &by_number(&bob)).await;
    app.wait_text(number_chat, "The tap is dripping again")
        .await;
    harness
        .send_receipt(&bob.aci, &me, "delivery", &[sent_at], true)
        .await
        .expect("sealed delivery receipt");
    let bob_chat = app.conversation_with(account, &user(&bob)).await;
    wait_ready(
        &app,
        bob_chat,
        "The tap is dripping again",
        "the receipt on the message sent to Bob's number",
        delivered,
    )
    .await;

    // Each contact has one chat now: the chats by number became the chats with the accounts.
    let query = ConversationListQuery {
        account: Some(account),
        limit: 100,
        ..ConversationListQuery::default()
    };
    let rows = match app
        .handle
        .query(Query::Conversations(query))
        .await
        .expect("conversations")
    {
        QueryData::Conversations(page) => page.rows,
        other => panic!("unexpected {other:?}"),
    };
    let peers: Vec<Option<UserId>> = rows
        .iter()
        .filter(|r| matches!(r.kind, ConvKind::Direct))
        .map(|r| r.peer.clone())
        .collect();
    assert_eq!(peers.len(), 2, "one chat per contact: {peers:?}");
    assert!(peers.contains(&Some(user(&world.alice))) && peers.contains(&Some(user(&bob))));
    app.stop().await;
    world.shutdown().await;
}

/// Waits until the row `id` is gone and returns how long that took from `since`.
async fn gone_after(app: &App, conv: ConversationId, id: MessageId, since: Instant) -> Duration {
    let deadline = Instant::now() + Duration::from_secs(20);
    while row_by_id(app, conv, id).await.is_some() {
        assert!(Instant::now() < deadline, "the message never expired");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    since.elapsed()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_timer_set_from_either_side_expires_messages_on_time() {
    let Some(club) = Club::start().await else {
        return;
    };
    let mut app = club.app().await;
    let (account, device) = app.link(&club.world).await;
    let ada = club.ada();
    let ada_user = user(ada);
    let harness = &club.world.harness;
    let me = &device.device_ref;
    club.says(ada, &device, "hello").await;
    let conv = club.chat(&app, account, ada).await;
    app.wait_text(conv, "hello").await;
    app.send(UiCommand::MarkRead { conv });
    assert_eq!(timer_of(&app, conv).await, None);
    let timer = Duration::from_secs(3);
    let timer_notice = |by: Option<UserId>, timer: Option<Duration>| {
        move |row: &MessageRow| {
            matches!(&row.body, Body::System(SystemEvent::TimerChanged { by: b, timer: t })
                if *b == by && *t == timer)
        }
    };

    // -- Ada turns the timer on (3 s): the chat has it, with her notice
    club.send(json!({
        "from": ada.aci, "to": me, "flags": 2, "expireTimer": 3, "expireTimerVersion": 2,
    }))
    .await;
    app.wait_row_where(
        conv,
        "her timer notice",
        timer_notice(Some(ada_user.clone()), Some(timer)),
    )
    .await;
    assert_eq!(timer_of(&app, conv).await, Some(timer));

    // -- our message carries the timer and goes 3 s after it was sent
    let sent = send_text(&app, conv, "this message will vanish", None).await;
    let since = Instant::now();
    let got = harness
        .wait_for_data(&ada.aci, "message", Some(me), None, WAIT)
        .await
        .expect("Ada got it");
    assert_eq!(got.data_message["expireTimer"], 3);
    assert_eq!(got.data_message["expireTimerVersion"], 2);
    assert!(club.transcript(&device).await["expirationStartTimestamp"].is_u64());
    assert!(
        row_by_id(&app, conv, sent.id).await.is_some(),
        "still here right after sending"
    );
    let took = gone_after(&app, conv, sent.id, since).await;
    assert!(
        took >= Duration::from_millis(1500) && took <= Duration::from_secs(8),
        "expired {took:?} after it was sent (timer 3 s)"
    );

    // -- her message waits until it is read, then goes after 3 s
    club.send(json!({
        "from": ada.aci, "to": me, "text": "read me and I vanish",
        "expireTimer": 3, "expireTimerVersion": 2,
    }))
    .await;
    let hers = app.wait_text(conv, "read me and I vanish").await.remove(0);
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(
        row_by_id(&app, conv, hers.id).await.is_some(),
        "unread: the timer has not started"
    );
    app.send(UiCommand::MarkRead { conv });
    let since = Instant::now();
    let took = gone_after(&app, conv, hers.id, since).await;
    assert!(
        took >= Duration::from_millis(1500) && took <= Duration::from_secs(8),
        "expired {took:?} after it was read"
    );

    // -- read on the phone: the read sync starts the timer here
    let on_phone = club
        .send(json!({
            "from": ada.aci, "to": me, "text": "read on the phone",
            "expireTimer": 3, "expireTimerVersion": 2,
        }))
        .await;
    let row = app.wait_text(conv, "read on the phone").await.remove(0);
    harness
        .send_sync(json!({
            "from": club.world.phone.aci, "to": me, "awaitDelivery": true,
            "read": [{ "sender": ada.aci, "timestamp": on_phone }],
        }))
        .await
        .expect("read sync");
    let since = Instant::now();
    let took = gone_after(&app, conv, row.id, since).await;
    assert!(
        took >= Duration::from_millis(1500) && took <= Duration::from_secs(8),
        "expired {took:?} after the phone read it"
    );

    // -- our own message from the phone: its timer started when the phone sent it
    let started = now_ms() - 2_000;
    club.send(json!({
        "from": club.world.phone.aci, "to": me, "text": "sent from the phone",
        "timestamp": started, "expireTimer": 3, "expireTimerVersion": 2, "withProfileKey": true,
        "sync": { "destination": ada.aci, "expirationStartTimestamp": started },
    }))
    .await;
    let since = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = None;
    loop {
        let rows = app.rows_with_text(conv, "sent from the phone").await;
        match (rows.first(), seen) {
            (Some(row), None) => seen = Some(row.id),
            (None, Some(_)) => break,
            _ => {}
        }
        assert!(
            Instant::now() < deadline,
            "the phone's message never expired"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        since.elapsed() <= Duration::from_secs(4),
        "one second of its timer was left, not three: {:?}",
        since.elapsed()
    );

    // -- we change the timer: Ada and the phone get the update, the chat and timeline show it
    let half_minute = Duration::from_secs(30);
    app.send(UiCommand::SetConversationTimer {
        conv,
        timer: Some(half_minute),
    });
    let update = harness
        .wait_for_data(&ada.aci, "timer", Some(me), None, WAIT)
        .await
        .expect("Ada got the timer update");
    assert_eq!(update.data_message["flags"], 2);
    assert_eq!(update.data_message["expireTimer"], 30);
    assert_eq!(
        update.data_message["expireTimerVersion"], 3,
        "one above hers"
    );
    let transcript = club.transcript(&device).await;
    assert_eq!(transcript["message"]["expireTimer"], 30);
    let notice = app
        .wait_row_where(
            conv,
            "our timer notice",
            timer_notice(None, Some(half_minute)),
        )
        .await;
    assert_eq!(
        notice.out_state, None,
        "a notice is not an outgoing message"
    );
    assert_eq!(
        notice.sent_at,
        i64::try_from(update.timestamp).unwrap(),
        "one id for both"
    );
    assert_eq!(timer_of(&app, conv).await, Some(half_minute));
    let next = send_text(&app, conv, "half a minute", None).await;
    assert_eq!(next.expiry.map(|e| e.timer), Some(half_minute));
    let got = harness
        .wait_for_data(&ada.aci, "message", Some(me), None, WAIT)
        .await
        .expect("Ada got it");
    assert_eq!(got.data_message["expireTimer"], 30);
    assert_eq!(got.data_message["expireTimerVersion"], 3);

    // -- and off again
    app.send(UiCommand::SetConversationTimer { conv, timer: None });
    let update = harness
        .wait_for_data(&ada.aci, "timer", Some(me), None, WAIT)
        .await
        .expect("Ada got the second update");
    assert_eq!(update.data_message["expireTimer"], 0);
    assert_eq!(update.data_message["expireTimerVersion"], 4);
    app.wait_row_where(conv, "the off notice", timer_notice(None, None))
        .await;
    assert_eq!(timer_of(&app, conv).await, None);

    // -- a timer that is on no menu: the chat takes any whole second up to the largest one
    // Android reads as it was sent (a signed 32-bit number of seconds; `kourier-signal`
    // `features.rs` has the sources), so 95 seconds go out like any other timer. Four weeks,
    // where the official clients' pickers end, is no limit: five weeks are taken too.
    let custom = Duration::from_secs(95);
    let features = app.features(conv).await;
    assert!(features.takes_timer(custom));
    assert!(features.takes_timer(Duration::from_secs(4 * 7 * 24 * 3600)));
    let five_weeks = Duration::from_secs(5 * 7 * 24 * 3600);
    assert!(features.takes_timer(five_weeks));
    assert!(features.takes_timer(Duration::from_secs(2_147_483_647)));
    let too_long = Duration::from_secs(2_147_483_648);
    assert!(!features.takes_timer(too_long));
    app.send(UiCommand::SetConversationTimer {
        conv,
        timer: Some(five_weeks),
    });
    let update = harness
        .wait_for_data(&ada.aci, "timer", Some(me), None, WAIT)
        .await
        .expect("Ada got the five weeks");
    assert_eq!(update.data_message["expireTimer"], 5 * 7 * 24 * 3600);
    assert_eq!(update.data_message["expireTimerVersion"], 5);
    app.wait_row_where(
        conv,
        "the five-week notice",
        timer_notice(None, Some(five_weeks)),
    )
    .await;
    assert_eq!(timer_of(&app, conv).await, Some(five_weeks));
    app.send(UiCommand::SetConversationTimer {
        conv,
        timer: Some(custom),
    });
    let update = harness
        .wait_for_data(&ada.aci, "timer", Some(me), None, WAIT)
        .await
        .expect("Ada got the custom timer");
    assert_eq!(update.data_message["expireTimer"], 95);
    assert_eq!(update.data_message["expireTimerVersion"], 6);
    app.wait_row_where(
        conv,
        "the custom timer notice",
        timer_notice(None, Some(custom)),
    )
    .await;
    assert_eq!(timer_of(&app, conv).await, Some(custom));

    // -- a timer above the range is refused before anything is sent
    while app.events.try_recv().is_ok() {}
    app.send(UiCommand::SetConversationTimer {
        conv,
        timer: Some(too_long),
    });
    app.event("a refused timer", |e| match e {
        UiEvent::ActionFailed { action, .. } if *action == "SetConversationTimer" => Some(()),
        _ => None,
    })
    .await;
    assert!(
        harness
            .wait_for_data(&ada.aci, "timer", Some(me), None, QUIET)
            .await
            .is_err(),
        "nothing went to Ada"
    );
    assert_eq!(timer_of(&app, conv).await, Some(custom));
    // A group's timer belongs to the group state: no menu, so the same refusal.
    let group = club.group_conv(&app, account).await;
    assert!(app.features(group).await.disappearing.is_empty());
    app.send(UiCommand::SetConversationTimer {
        conv: group,
        timer: Some(half_minute),
    });
    app.event("a refused group timer", |e| match e {
        UiEvent::ActionFailed { action, .. } if *action == "SetConversationTimer" => Some(()),
        _ => None,
    })
    .await;
    app.stop().await;
    club.world.shutdown().await;
}

/// Set (to a JSON [`ChildSpec`]) in the helper process of the kill test;
/// [`modifier_child_process`] does nothing without it.
const CHILD_ENV: &str = "KOURIER_E2E_MODIFIER_CHILD";
/// Prefix of the helper process's protocol lines on stdout.
const CHILD_LINE: &str = "KOURIER-E2E-MODIFIER";

/// What the helper process runs: the app over this profile, then one action on `message`.
#[derive(Serialize, Deserialize)]
struct ChildSpec {
    dir: PathBuf,
    env: BackendEnv,
    account: i64,
    message: i64,
    /// `edit` or `react`.
    action: String,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_edit_and_a_reaction_are_sent_once_across_a_kill() {
    let Some(club) = Club::start().await else {
        return;
    };
    let mut app = club.app().await;
    let (account, device) = app.link(&club.world).await;
    let ada = club.ada();
    let harness = &club.world.harness;
    let me = device.device_ref.clone();
    let ping = club.says(ada, &device, "ping").await;
    let conv = club.chat(&app, account, ada).await;
    let ping_row = app.wait_text(conv, "ping").await.remove(0);
    let sent = send_text(&app, conv, "first draft", None).await;
    let original_at = harness
        .wait_for_data(&ada.aci, "message", Some(&me), None, WAIT)
        .await
        .expect("Ada got it")
        .timestamp;
    club.transcript(&device).await;
    app.stop().await;

    for (action, message) in [("edit", sent.id), ("react", ping_row.id)] {
        let spec = ChildSpec {
            dir: club.world.tmp.path().to_owned(),
            env: club.world.env.clone(),
            account: account.0,
            message: message.0,
            action: action.to_owned(),
        };
        let point = kourier_signal::failpoints::AFTER_MODIFIER_ENCRYPT_COMMIT;
        let mut child = tokio::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "modifier_child_process",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, serde_json::to_string(&spec).expect("spec"))
            .env("KOURIER_FAILPOINT", format!("{point}:1"))
            .env(
                "RUST_LOG",
                std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".to_owned()),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn the helper process");
        let mut lines = BufReader::new(child.stdout.take().expect("stdout")).lines();
        let acted = tokio::time::timeout(WAIT, async {
            while let Some(line) = lines.next_line().await.expect("child stdout") {
                if line.trim() == format!("{CHILD_LINE} acting") {
                    return true;
                }
            }
            false
        })
        .await;
        assert_eq!(acted, Ok(true), "{action}: the helper app acted");
        let status = tokio::time::timeout(WAIT, child.wait())
            .await
            .unwrap_or_else(|_| panic!("{action}: the helper app did not end"))
            .expect("helper status");
        assert_eq!(
            status.signal(),
            Some(6),
            "{action}: aborted between the encrypt commit and the request"
        );
        // Nothing left before the kill.
        club.nothing_more(ada, &device).await;

        // The next start sends it: once.
        let app = club.app().await;
        app.wait_online(account).await;
        match action {
            "edit" => {
                let edit = harness
                    .wait_for_edit(&ada.aci, Some(&me), None, WAIT)
                    .await
                    .expect("the edit after the restart");
                assert_eq!(edit.target_timestamp, original_at);
                assert_eq!(edit.text, "second draft");
                let row = app
                    .wait_row(conv, "second draft", "the edit is kept", |row| {
                        row.revision == 1
                    })
                    .await;
                assert_eq!(row.id, sent.id);
            }
            _ => {
                let reaction = harness
                    .wait_for_reaction(&ada.aci, Some(&me), WAIT)
                    .await
                    .expect("the reaction after the restart");
                assert_eq!(reaction.data_message["reaction"]["emoji"], "👍");
                assert_eq!(
                    reaction.data_message["reaction"]["targetSentTimestamp"],
                    ping
                );
                app.wait_row(conv, "ping", "the reaction is kept", |row| {
                    row.reactions.iter().any(|r| r.by_me())
                })
                .await;
            }
        }
        // The phone hears of it once, too.
        club.transcript(&device).await;
        club.nothing_more(ada, &device).await;
        let again = harness
            .wait_for_sync_message(&club.world.phone.aci, Some(&me), "sent", QUIET)
            .await;
        assert!(again.is_err(), "{action}: a second transcript: {again:?}");
        app.stop().await;
    }
    club.world.shutdown().await;
}

/// The helper process of the kill test: the app over the given profile, online, then the edit
/// or the reaction, until the armed failpoint aborts it.
#[test]
fn modifier_child_process() {
    let Ok(spec) = std::env::var(CHILD_ENV) else {
        return;
    };
    init_tracing();
    let spec: ChildSpec = serde_json::from_str(&spec).expect("child spec");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async move {
        let app = App::start_with(&spec.dir, &spec.env, InitialSync::ContactsAndStorage).await;
        app.wait_online(AccountId(spec.account)).await;
        let message = MessageId(spec.message);
        println!("{CHILD_LINE} acting");
        app.send(match spec.action.as_str() {
            "edit" => UiCommand::EditMessage {
                message,
                text: Text::plain("second draft"),
            },
            _ => UiCommand::React {
                message,
                emoji: "👍".to_owned(),
                remove: false,
            },
        });
        tokio::time::sleep(Duration::from_secs(60)).await;
        // Only reached when the failpoint did not fire.
        println!("{CHILD_LINE} done");
        app.stop().await;
    });
}
