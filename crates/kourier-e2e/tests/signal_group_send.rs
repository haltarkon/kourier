//! S11 through the real core (docs/architecture.md §4.8, §6.7 step 5, §11 S11): a text the user
//! sends in a group reaches every member as one sender-key message and the timeline shows it
//! sent; when one member is rate-limited (and the group send token is refused, so every member
//! gets a 1:1 send) the core's outbox retries only that member as a recipient update, and the
//! other members get the message once. Exercises the core's `Partial` handling against the real
//! backend (the `kourier-signal` tests drive the follow-up by hand).
//!
//! S12 in a group ([`a_file_sent_to_the_group_reaches_every_member_from_one_upload`]): a file
//! with a caption is uploaded once and every member's phone decrypts it byte-identical from the
//! pointer in its sender-key message.
//!
//! Read receipts in a group ([`reading_group_messages_tells_each_author`], §6.7 step 6): when
//! the user reads a group's messages, each author gets one read receipt with the timestamps
//! of their own messages, sealed, as a 1:1 message that does not name the group; members who
//! wrote nothing get nothing; our phone gets the read sync. With the read-receipt setting
//! off, and to a member blocked on the phone, none goes out.
//!
//! A receipt that has to wait ([`a_receipt_that_waits_does_not_repeat_the_ones_already_sent`]):
//! while the server rate-limits sends to one author, the job is tried again and again; the
//! author served before it hears once, and the one who waited hears when the limit ends.
//!
//! Skips without Node ≥ 24 or the harness's `node_modules` (`KOURIER_REQUIRE_MOCK_HARNESS=1`
//! fails instead).

mod support;

use std::time::Duration;

use kourier_backend_api::InitialSync;
use kourier_core::facade::{ConversationListQuery, Query, QueryData, UiCommand};
use kourier_model::{AttachmentKind, Body, ConvKind, ConversationId, OutgoingState, Text};
use support::harness::{
    CreateGroup, CreatePrimary, DeviceInfo, GroupInfo, Harness, Primary, SendGroupText,
    StorageContact, StorageGroup, StorageSpec,
};
use support::{App, WAIT, World, init_tracing, local_env};

/// How long a test waits to be sure something did not arrive.
const QUIET: Duration = Duration::from_millis(1500);

/// The mock with our phone, three members and their group.
struct GroupWorld {
    world: World,
    members: Vec<Primary>,
    group: GroupInfo,
}

impl GroupWorld {
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

    /// Waits until the group is in the core and writable; returns its conversation.
    async fn group_conv(&self, app: &App, account: kourier_model::AccountId) -> ConversationId {
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
                // The backend's group features arrived (not the core's defaults) and allow sends.
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

    /// Waits until member `i` got `text` in the group from our device; returns its envelope type.
    async fn received(&self, i: usize, device: &DeviceInfo, text: &str) -> String {
        let got = self
            .world
            .harness
            .wait_for_group_message(
                &self.members[i].aci,
                &self.group.id,
                Some(&device.device_ref),
                WAIT,
            )
            .await
            .unwrap_or_else(|e| panic!("member {i} got nothing: {e}"));
        assert_eq!(got.text, text, "member {i}");
        got.message_type
    }

    /// Waits until our message `text` in `conv` is sent to everyone.
    async fn wait_sent(app: &App, conv: ConversationId, text: &str) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let rows = app.rows_with_text(conv, text).await;
            if rows
                .iter()
                .any(|r| matches!(r.out_state, Some(OutgoingState::Sent { .. })))
            {
                assert_eq!(rows.len(), 1, "one row");
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "never sent: {:?}",
                rows.iter().map(|r| r.out_state.clone()).collect::<Vec<_>>()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn group_texts_through_the_core_with_a_rate_limited_member() {
    let Some(gw) = GroupWorld::start().await else {
        return;
    };
    let mut app = App::start_with(
        gw.world.tmp.path(),
        &gw.world.env,
        InitialSync::ContactsAndStorage,
    )
    .await;
    let (account, device) = app.link(&gw.world).await;
    let conv = gw.group_conv(&app, account).await;

    // A text to the group: one sender-key message reaches every member.
    let text = "Who brings the rope on Saturday?";
    app.send(UiCommand::SendText {
        conv,
        text: Text::plain(text),
        reply_to: None,
    });
    for i in 0..3 {
        assert_eq!(gw.received(i, &device, text).await, "SenderKey");
    }
    GroupWorld::wait_sent(&app, conv, text).await;

    // Chiara is rate-limited twice and the token is refused: Ada and Bruno get 1:1 copies, the
    // core retries Chiara alone after the server's Retry-After.
    let harness = &gw.world.harness;
    harness
        .reject_group_send_tokens(true)
        .await
        .expect("reject tokens");
    harness
        .rate_limit(&gw.members[2].device_ref, Some(2), 1)
        .await
        .expect("rate limit");
    let before = harness.send_stats().await.expect("stats");
    let text = "Meet at the gym at 8";
    app.send(UiCommand::SendText {
        conv,
        text: Text::plain(text),
        reply_to: None,
    });
    for i in 0..2 {
        assert_ne!(gw.received(i, &device, text).await, "SenderKey", "1:1");
    }
    // The first transcript covers Ada and Bruno; the follow-up only Chiara, as an update.
    let first = harness
        .wait_for_sync_message(&gw.world.phone.aci, Some(&device.device_ref), "sent", WAIT)
        .await
        .expect("first transcript");
    let first = &first.sync_message["content"]["sent"];
    // (The first group message's transcript may come first.)
    let first = if first["message"]["body"] == text {
        first.clone()
    } else {
        harness
            .wait_for_sync_message(&gw.world.phone.aci, Some(&device.device_ref), "sent", WAIT)
            .await
            .expect("first transcript")
            .sync_message["content"]["sent"]
            .clone()
    };
    assert_eq!(first["message"]["body"], text);
    assert_eq!(
        first["unidentifiedStatus"].as_array().map(Vec::len),
        Some(2)
    );
    assert_ne!(first["isRecipientUpdate"], true);
    harness
        .reject_group_send_tokens(false)
        .await
        .expect("accept tokens");
    gw.received(2, &device, text).await;
    let update = harness
        .wait_for_sync_message(&gw.world.phone.aci, Some(&device.device_ref), "sent", WAIT)
        .await
        .expect("update transcript");
    let update = &update.sync_message["content"]["sent"];
    assert_eq!(update["message"]["body"], text);
    assert_eq!(update["isRecipientUpdate"], true, "{update}");
    assert_eq!(
        update["unidentifiedStatus"].as_array().map(Vec::len),
        Some(1)
    );
    GroupWorld::wait_sent(&app, conv, text).await;
    // Ada and Bruno got it once; Chiara's sends: two refused, then the follow-up.
    for i in 0..2 {
        let more = harness
            .wait_for_group_message(&gw.members[i].aci, &gw.group.id, None, QUIET)
            .await;
        assert!(more.is_err(), "member {i} got the message twice: {more:?}");
    }
    let stats = harness.send_stats().await.expect("stats");
    let chiara = stats.single_to(&gw.members[2].aci) - before.single_to(&gw.members[2].aci);
    assert_eq!(chiara, 3, "two rate-limited sends, then the follow-up");
    app.stop().await;
    gw.world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_sent_to_the_group_reaches_every_member_from_one_upload() {
    let Some(gw) = GroupWorld::start().await else {
        return;
    };
    let mut app = App::start_with(
        gw.world.tmp.path(),
        &gw.world.env,
        InitialSync::ContactsAndStorage,
    )
    .await;
    let (account, device) = app.link(&gw.world).await;
    let conv = gw.group_conv(&app, account).await;
    let features = app.features(conv).await;
    assert!(
        features.files.contains_key(&AttachmentKind::File),
        "groups take files: {:?}",
        features.files
    );

    // The user's file lives outside the profile.
    let picked = tempfile::tempdir().expect("picked files");
    let plan: Vec<u8> = (0..120_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    let path = picked.path().join("route-plan.bin");
    std::fs::write(&path, &plan).expect("write the picked file");
    let harness = &gw.world.harness;
    let before = harness.send_stats().await.expect("stats");
    let caption = "The route plan for Saturday";
    app.send(UiCommand::SendFiles {
        request: Default::default(),
        as_file: Vec::new(),
        conv,
        paths: vec![path].into(),
        caption: Some(Text::plain(caption)),
        reply_to: None,
    });

    // One sender-key message for everyone, each with the pointer of the same upload.
    let mut digests = Vec::new();
    for (i, member) in gw.members.iter().enumerate() {
        let got = harness
            .wait_for_group_message(&member.aci, &gw.group.id, Some(&device.device_ref), WAIT)
            .await
            .unwrap_or_else(|e| panic!("member {i} got nothing: {e}"));
        assert_eq!(got.text, caption, "member {i}");
        assert_eq!(got.message_type, "SenderKey", "member {i}");
        let pointers = got.data_message["attachments"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert_eq!(pointers.len(), 1, "member {i}: {}", got.data_message);
        let pointer = &pointers[0];
        assert_eq!(pointer["fileName"], "route-plan.bin");
        let fetched = harness
            .fetch_attachment(pointer)
            .await
            .unwrap_or_else(|e| panic!("member {i} cannot download the file: {e}"));
        assert!(fetched.mac_valid && fetched.digest_valid, "{fetched:?}");
        assert_eq!(fetched.bytes(), plan, "byte-identical for member {i}");
        digests.push(pointer["digest"].clone());
    }
    assert!(digests.iter().all(|d| *d == digests[0]), "one blob for all");
    let after = harness.send_stats().await.expect("stats");
    assert_eq!(after.uploads - before.uploads, 1, "one upload");
    assert_eq!(after.multi - before.multi, 1, "one multi-recipient send");

    // Our phone's transcript names the same upload, and the timeline shows the file as sent.
    let transcript = harness
        .wait_for_sync_message(&gw.world.phone.aci, Some(&device.device_ref), "sent", WAIT)
        .await
        .expect("transcript");
    let synced = &transcript.sync_message["content"]["sent"]["message"];
    assert_eq!(synced["body"], caption);
    assert_eq!(synced["attachments"][0]["digest"], digests[0]);
    app.wait_row_where(conv, "the sent file", |row| {
        matches!(&row.body, Body::Media { caption: Some(c), .. } if c.body == caption)
            && matches!(row.out_state, Some(OutgoingState::Sent { .. }))
    })
    .await;
    app.stop().await;
    gw.world.shutdown().await;
}

/// The next read or viewed receipt member `i` got from our device (delivery receipts, which
/// the backend sends on its own, are passed over). Every one of them is checked for what
/// Desktop puts on the wire for a group message's receipt: no group in the sealed content,
/// the resendable hint, no PNI signature (`kourier-signal` `tests/group_send.rs` has the
/// sources and the case of a member who knows us by number).
async fn read_receipt(
    gw: &GroupWorld,
    i: usize,
    me: &str,
    wait: Duration,
) -> Option<(String, Vec<u64>, bool)> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let receipt = gw
            .world
            .harness
            .wait_for_receipt(
                &gw.members[i].aci,
                Some(me),
                left.max(Duration::from_millis(1)),
            )
            .await
            .ok()?;
        if receipt.kind != "delivery" {
            assert_eq!(receipt.sealed_group_id, None, "{receipt:?}");
            assert!(!receipt.pni_signature, "{receipt:?}");
            if receipt.sealed {
                assert_eq!(receipt.content_hint.as_deref(), Some("resendable"));
            }
            return Some((receipt.kind, receipt.timestamps, receipt.sealed));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reading_group_messages_tells_each_author() {
    let Some(gw) = GroupWorld::start().await else {
        return;
    };
    let mut app = App::start_with(
        gw.world.tmp.path(),
        &gw.world.env,
        InitialSync::ContactsAndStorage,
    )
    .await;
    let (account, device) = app.link(&gw.world).await;
    let me = device.device_ref.clone();
    let conv = gw.group_conv(&app, account).await;
    let harness = &gw.world.harness;
    let says = |i: usize, text: &'static str| {
        let spec = SendGroupText {
            from: gw.members[i].aci.clone(),
            group: gw.group.id.clone(),
            to: vec![me.clone()],
            text: text.to_owned(),
            ..SendGroupText::default()
        };
        async move {
            harness
                .send_group_text(&spec)
                .await
                .expect("group text")
                .timestamp
        }
    };

    // Ada writes twice, Bruno once, Chiara not at all.
    let ada_first = says(0, "Is the wall open on Sunday?").await;
    let bruno_only = says(1, "From ten, I think.").await;
    let ada_second = says(0, "Then I am in.").await;
    for text in [
        "Is the wall open on Sunday?",
        "From ten, I think.",
        "Then I am in.",
    ] {
        app.wait_text(conv, text).await;
    }

    // The user reads the group: each author hears of their own messages, and only of those.
    app.send(UiCommand::MarkRead { conv });
    let (kind, timestamps, sealed) = read_receipt(&gw, 0, &me, WAIT)
        .await
        .expect("a read receipt for Ada");
    assert_eq!(kind, "read");
    assert_eq!(
        timestamps,
        [ada_first, ada_second],
        "Ada's messages, in order"
    );
    assert!(sealed, "sealed, as Desktop sends receipts");
    let (kind, timestamps, sealed) = read_receipt(&gw, 1, &me, WAIT)
        .await
        .expect("a read receipt for Bruno");
    assert_eq!(
        (kind.as_str(), &timestamps[..]),
        ("read", &[bruno_only][..])
    );
    assert!(sealed);
    assert_eq!(
        read_receipt(&gw, 2, &me, QUIET).await,
        None,
        "Chiara wrote nothing and hears nothing"
    );
    assert_eq!(
        read_receipt(&gw, 0, &me, QUIET).await,
        None,
        "one receipt each"
    );
    // Our phone learns what was read, as in a 1:1 chat.
    let sync = harness
        .wait_for_sync_message(&gw.world.phone.aci, Some(&me), "read", WAIT)
        .await
        .expect("read sync");
    let read: Vec<u64> = sync.sync_message["read"]
        .as_array()
        .expect("read entries")
        .iter()
        .filter_map(|entry| {
            entry["timestamp"]
                .as_u64()
                .or_else(|| entry["timestamp"].as_str()?.parse().ok())
        })
        .collect();
    assert_eq!(read, [ada_first, bruno_only, ada_second]);

    // Bruno is blocked on the phone: he gets no receipt any more, Ada still does.
    harness
        .set_storage_state(&StorageSpec {
            primary: gw.world.phone.device_ref.clone(),
            contacts: gw
                .members
                .iter()
                .enumerate()
                .map(|(i, m)| StorageContact {
                    blocked: (i == 1).then_some(true),
                    ..StorageContact::of(m.device_ref.clone())
                })
                .collect(),
            groups: vec![StorageGroup {
                id: gw.group.id.clone(),
                ..StorageGroup::default()
            }],
            ..StorageSpec::default()
        })
        .await
        .expect("storage state");
    harness
        .send_fetch_latest(&gw.world.phone.device_ref, &me, "storageManifest")
        .await
        .expect("fetch latest");
    // The block is in the core once Bruno's messages are dropped: Ada's next one arrives, his
    // does not.
    let deadline = tokio::time::Instant::now() + WAIT;
    let mut round = 0u32;
    let ada_third = loop {
        round += 1;
        let _ = says(1, "Bruno, after the block").await;
        let at = says(0, "Ada, after the block").await;
        app.wait_row_where(conv, "Ada's message", |row| {
            row.sent_at == i64::try_from(at).unwrap_or_default()
        })
        .await;
        let from_bruno = app.rows_with_text(conv, "Bruno, after the block").await;
        if from_bruno.len() < usize::try_from(round).unwrap_or(usize::MAX) {
            break at;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the block never reached Kourier"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    // Whatever arrived before the block took hold is read with the rest.
    let _ = harness.drain_events().await;
    app.send(UiCommand::MarkRead { conv });
    let (kind, timestamps, _) = read_receipt(&gw, 0, &me, WAIT)
        .await
        .expect("a read receipt for Ada");
    assert_eq!(kind, "read");
    assert!(timestamps.contains(&ada_third), "{timestamps:?}");
    assert_eq!(
        read_receipt(&gw, 1, &me, QUIET).await,
        None,
        "nothing goes to a blocked contact"
    );

    // The phone turns read receipts off: reading still syncs to the phone, and tells nobody.
    harness
        .send_configuration(&gw.world.phone.device_ref, &me, false, true)
        .await
        .expect("configuration");
    let ada_last = says(0, "Last one from Ada").await;
    app.wait_text(conv, "Last one from Ada").await;
    // The configuration was acknowledged (it is durable in the backend) and is applied in a
    // lane of its own: give that lane a moment.
    tokio::time::sleep(QUIET).await;
    app.send(UiCommand::MarkRead { conv });
    // (The read sync of the round before is still in the phone's inbox.)
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let sync = harness
            .wait_for_sync_message(&gw.world.phone.aci, Some(&me), "read", left)
            .await
            .expect("the read sync of the last message");
        if sync.sync_message["read"]
            .to_string()
            .contains(&ada_last.to_string())
        {
            break;
        }
    }
    assert_eq!(
        read_receipt(&gw, 0, &me, QUIET).await,
        None,
        "read receipts are off"
    );
    app.stop().await;
    gw.world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_receipt_that_waits_does_not_repeat_the_ones_already_sent() {
    let Some(gw) = GroupWorld::start().await else {
        return;
    };
    let mut app = App::start_with(
        gw.world.tmp.path(),
        &gw.world.env,
        InitialSync::ContactsAndStorage,
    )
    .await;
    let (account, device) = app.link(&gw.world).await;
    let me = device.device_ref.clone();
    let conv = gw.group_conv(&app, account).await;
    let harness = &gw.world.harness;
    let says = |i: usize, text: &'static str| {
        let spec = SendGroupText {
            from: gw.members[i].aci.clone(),
            group: gw.group.id.clone(),
            to: vec![me.clone()],
            text: text.to_owned(),
            ..SendGroupText::default()
        };
        async move {
            harness
                .send_group_text(&spec)
                .await
                .expect("group text")
                .timestamp
        }
    };
    let from_ada = says(0, "Who brings the rope?").await;
    let from_bruno = says(1, "I do.").await;
    app.wait_text(conv, "Who brings the rope?").await;
    app.wait_text(conv, "I do.").await;
    // The delivery receipts Kourier sends on its own are out before the server starts to
    // refuse sends to Bruno, so the read receipt is the send that meets the limit.
    for i in [0, 1] {
        assert_eq!(
            read_receipt(&gw, i, &me, QUIET).await,
            None,
            "nothing read yet"
        );
    }
    harness
        .rate_limit(&gw.members[1].device_ref, None, 2)
        .await
        .expect("rate limit");

    // The user reads the group. Ada, the first author, gets her receipt; Bruno's is refused
    // (try again in two seconds), and refused again at every retry.
    app.send(UiCommand::MarkRead { conv });
    let (kind, timestamps, _) = read_receipt(&gw, 0, &me, WAIT)
        .await
        .expect("a read receipt for Ada");
    assert_eq!((kind.as_str(), &timestamps[..]), ("read", &[from_ada][..]));
    // Two retries later (the job waits the server's two seconds each time) Ada has still
    // heard once, and Bruno nothing.
    assert_eq!(
        read_receipt(&gw, 0, &me, Duration::from_secs(5)).await,
        None,
        "the job tried again for Bruno and told Ada a second time"
    );
    assert_eq!(
        read_receipt(&gw, 1, &me, QUIET).await,
        None,
        "still refused"
    );

    // The limit ends: Bruno hears of his message, and Ada of nothing more.
    harness
        .stop_rate_limit(&gw.members[1].device_ref)
        .await
        .expect("stop the rate limit");
    let (kind, timestamps, sealed) = read_receipt(&gw, 1, &me, WAIT)
        .await
        .expect("a read receipt for Bruno");
    assert_eq!(
        (kind.as_str(), &timestamps[..]),
        ("read", &[from_bruno][..])
    );
    assert!(sealed);
    assert_eq!(
        read_receipt(&gw, 0, &me, QUIET).await,
        None,
        "one receipt each"
    );

    app.stop().await;
    gw.world.shutdown().await;
}
