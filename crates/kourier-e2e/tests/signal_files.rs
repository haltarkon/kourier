//! Many files in one send on Signal, through the real core (docs/architecture.md §3.8, §4.8
//! step 1, §6.7 steps 1-2): the app's core with the Signal backend against
//! `@signalapp/mock-server`, driven through the facade as the composer drives it. The harness
//! plays our phone ("Myself") and three contacts with their group.
//!
//! [`mixed_files_reach_every_phone_as_albums_and_single_files`]: one `SendFiles` of a GIF, two
//! pictures, two documents, an audio file and a video becomes the messages Signal's packing
//! rule needs (`kourier-signal` `features::file_packing`: pictures and GIFs share a message,
//! every other file travels alone, a video too), in the user's order, with the caption on the
//! first. The
//! contact's phone gets exactly those messages, every file decrypts byte-identical from the
//! mock's CDN (pictures as Desktop re-encodes them), our own phone gets a transcript of each,
//! and each file is uploaded once. The same send in the group reaches every member, with one
//! upload per file whatever the number of members. A send the server rate-limits is retried
//! by the outbox without a second upload.
//!
//! [`a_multi_message_send_survives_a_kill_in_the_middle`]: the app is aborted in the middle of
//! such a send (after the messages were stored; between the attempt and the request; after the
//! server took the second message but before the core recorded it). After the next start
//! every message has arrived exactly once, in order, and every file was uploaded exactly once.
//!
//! Skips with a message when Node ≥ 24 or `tests/mock-harness/node_modules` is missing
//! (`KOURIER_REQUIRE_MOCK_HARNESS=1` fails instead).

mod support;

use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use kourier_backend_api::{BackendEnv, InitialSync};
use kourier_core::facade::{ConversationListQuery, MessageRow, Query, QueryData, UiCommand};
use kourier_model::{
    AccountId, AttachmentKind, Body, ConvKind, ConversationId, OutgoingState, Text,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use support::harness::{
    CreateGroup, CreatePrimary, DeviceInfo, GroupInfo, Harness, Primary, StorageContact,
    StorageGroup, StorageSpec,
};
use support::{App, WAIT, World, init_tracing, local_env, user};
use tokio::io::{AsyncBufReadExt as _, BufReader};

/// How long nothing more must arrive.
const QUIET: Duration = Duration::from_millis(1500);
/// Environment variable with the helper process's [`ChildSpec`].
const CHILD_ENV: &str = "KOURIER_E2E_FILES_CHILD";
/// Prefix of the helper's status lines.
const CHILD_LINE: &str = "KOURIER_FILES_CHILD";

/// Deterministic bytes that occur in no other test input.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// Every `"attachments"` array inside `value` (pointers in a DataMessage or a transcript).
fn pointers_in(value: &Value) -> Vec<Value> {
    let mut found = Vec::new();
    match value {
        Value::Object(map) => {
            for (key, item) in map {
                if key == "attachments"
                    && let Value::Array(items) = item
                {
                    found.extend(items.iter().cloned());
                } else {
                    found.extend(pointers_in(item));
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                found.extend(pointers_in(item));
            }
        }
        _ => {}
    }
    found
}

/// One file the user picked.
struct Picked {
    path: PathBuf,
    bytes: Vec<u8>,
    kind: AttachmentKind,
    /// What its content is, as the core reads it from the first bytes.
    mime: &'static str,
}

/// Writes `bytes` as `name` below `dir`.
fn pick(dir: &Path, name: &str, bytes: Vec<u8>, kind: AttachmentKind) -> Picked {
    let path = dir.join(name);
    std::fs::write(&path, &bytes).expect("write a picked file");
    let mime = kourier_media::sniff_mime(&bytes).unwrap_or("application/octet-stream");
    Picked {
        path,
        bytes,
        kind,
        mime,
    }
}

/// How the chat packs `files`, as the core's preview and its send plan it (by format).
fn plan_of(
    features: &kourier_model::ConversationFeatures,
    files: &[Picked],
    caption: Option<&str>,
) -> kourier_model::FilePlan {
    let planned: Vec<kourier_model::PlanFile<'_>> = files
        .iter()
        .map(|f| kourier_model::PlanFile::new(f.kind, f.mime))
        .collect();
    let caption = caption.map(Text::plain);
    features.plan(&planned, caption.as_ref(), false)
}

/// An ISO-media file of `brand` (what the sniffer reads the format from) and noise.
fn iso_media(brand: &[u8; 4], len: usize, seed: u64) -> Vec<u8> {
    let mut bytes = vec![0, 0, 0, 0x18];
    bytes.extend_from_slice(b"ftyp");
    bytes.extend_from_slice(brand);
    bytes.extend_from_slice(b"\0\0\0\0mif1isom");
    bytes.extend(noise(len, seed));
    bytes
}

/// A JPEG with a comment segment right after its start: any re-encode drops it.
fn jpeg_with_a_note(width: u32, height: u32, seed: u64) -> Vec<u8> {
    let plain = kourier_media::sample_image(width, height, seed, kourier_media::SampleFormat::Jpeg)
        .expect("sample JPEG");
    let note = b"kourier original, do not touch";
    let mut bytes = plain[..2].to_vec();
    bytes.extend_from_slice(&[0xff, 0xfe]);
    bytes.extend_from_slice(&u16::try_from(note.len() + 2).expect("short").to_be_bytes());
    bytes.extend_from_slice(note);
    bytes.extend_from_slice(&plain[2..]);
    bytes
}

fn gif(dir: &Path, name: &str, seed: u64) -> Picked {
    let bytes = kourier_media::sample_image(48, 32, seed, kourier_media::SampleFormat::Gif)
        .expect("sample GIF");
    pick(dir, name, bytes, AttachmentKind::Gif)
}

fn png(dir: &Path, name: &str, seed: u64) -> Picked {
    let bytes = kourier_media::sample_image(320, 200, seed, kourier_media::SampleFormat::Png)
        .expect("sample PNG");
    pick(dir, name, bytes, AttachmentKind::Image)
}

fn document(dir: &Path, name: &str, seed: u64) -> Picked {
    pick(dir, name, noise(30_000, seed), AttachmentKind::File)
}

fn mp3(dir: &Path, name: &str, seed: u64) -> Picked {
    let mut bytes = b"ID3\x03\0\0\0\0\0\0".to_vec();
    bytes.extend(noise(20_000, seed));
    pick(dir, name, bytes, AttachmentKind::Audio)
}

fn mp4(dir: &Path, name: &str, seed: u64) -> Picked {
    let mut bytes = vec![0, 0, 0, 0x18];
    bytes.extend_from_slice(b"ftypmp42\0\0\0\0mp42isom");
    bytes.extend(noise(60_000, seed));
    pick(dir, name, bytes, AttachmentKind::Video)
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

    /// The 1:1 chat with `peer` (who writes first, so the chat exists), once the core can
    /// send there.
    async fn chat(
        &self,
        app: &App,
        account: AccountId,
        peer: &Primary,
        device: &DeviceInfo,
    ) -> ConversationId {
        self.world
            .harness
            .send_data(serde_json::json!({
                "from": peer.aci, "to": device.device_ref, "text": "ready when you are",
                "withProfileKey": true, "awaitDelivery": true, "timeout": 60_000,
            }))
            .await
            .expect("sendData");
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

    /// The timestamps of the next `count` sync transcripts our phone got from our device.
    async fn transcripts(&self, device: &DeviceInfo, count: usize) -> Vec<u64> {
        let mut seen = Vec::new();
        for _ in 0..count {
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
            seen.push(
                sync.sync_message["content"]["sent"]["timestamp"]
                    .as_u64()
                    .or_else(|| {
                        sync.sync_message["content"]["sent"]["timestamp"]
                            .as_str()
                            .and_then(|t| t.parse().ok())
                    })
                    .expect("transcript timestamp"),
            );
        }
        seen
    }

    /// Asserts `who` gets no further message from our device for a while.
    async fn nothing_more(&self, who: &Primary, device: &DeviceInfo) {
        let message = self
            .world
            .harness
            .wait_for_message(&who.aci, Some(&device.device_ref), QUIET)
            .await;
        assert!(message.is_err(), "another message: {message:?}");
    }

    async fn uploads(&self) -> u64 {
        self.world
            .harness
            .send_stats()
            .await
            .expect("send stats")
            .uploads
    }
}

/// What the phone of `who` must get for the selection `files` split as `plan`: one message per
/// entry, in order, each with exactly its files. Returns the messages' timestamps.
async fn phone_gets(
    club: &Club,
    who: &Primary,
    me: &str,
    group: Option<&str>,
    files: &[Picked],
    plan: &[Vec<usize>],
    caption: Option<&str>,
) -> Vec<u64> {
    let harness = &club.world.harness;
    let mut stamps = Vec::new();
    for (n, positions) in plan.iter().enumerate() {
        let got = harness
            .wait_for_data(&who.aci, "message", Some(me), group, WAIT)
            .await
            .unwrap_or_else(|e| panic!("message {n} for {}: {e:?}", who.aci));
        let message = &got.data_message;
        let pointers = pointers_in(message);
        assert_eq!(
            pointers.len(),
            positions.len(),
            "message {n} carries its files and no others: {message}"
        );
        let body = message["body"].as_str().unwrap_or("");
        match caption.filter(|_| n == 0) {
            Some(caption) => assert_eq!(body, caption, "the caption is on the first message"),
            None => assert_eq!(body, "", "message {n} has no text"),
        }
        for (pointer, position) in pointers.iter().zip(positions) {
            let file = &files[*position];
            let fetched = harness
                .fetch_attachment(pointer)
                .await
                .unwrap_or_else(|e| panic!("file {position} on the phone: {e:?}"));
            assert!(fetched.mac_valid && fetched.digest_valid, "{fetched:?}");
            let bytes = fetched.bytes();
            match file.kind {
                // Still pictures are re-encoded as Desktop does (metadata dropped): the same
                // picture, not the same bytes.
                AttachmentKind::Image => {
                    let sent = kourier_media::decode(&file.bytes).expect("the picked picture");
                    let arrived = kourier_media::decode(&bytes).expect("a picture arrived");
                    assert_eq!(
                        (arrived.width(), arrived.height()),
                        (sent.width(), sent.height()),
                        "file {position}"
                    );
                    assert!(
                        pointer.get("fileName").is_none_or(Value::is_null),
                        "pictures travel without their names: {pointer}"
                    );
                }
                AttachmentKind::Gif => {
                    assert_eq!(bytes, file.bytes, "file {position} is byte-identical");
                    assert_eq!(pointer["contentType"], "image/gif");
                    assert!(
                        pointer.get("flags").is_none_or(|f| f.is_null() || f == 0),
                        "a GIF file is no looping video: {pointer}"
                    );
                }
                AttachmentKind::Video => {
                    assert_eq!(bytes, file.bytes, "file {position} is byte-identical");
                    assert_eq!(fetched.incremental_mac_valid, Some(true));
                }
                _ => {
                    assert_eq!(bytes, file.bytes, "file {position} is byte-identical");
                    let name = file.path.file_name().and_then(|n| n.to_str());
                    assert_eq!(pointer["fileName"].as_str(), name, "{pointer}");
                }
            }
        }
        stamps.push(got.timestamp);
    }
    assert!(
        stamps.windows(2).all(|pair| pair[0] < pair[1]),
        "in sending order, each with its own id: {stamps:?}"
    );
    stamps
}

/// The own rows of `conv` that carry files, oldest first, once all of them are sent.
async fn sent_file_rows(app: &App, conv: ConversationId, count: usize) -> Vec<MessageRow> {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        // A timeline page is oldest first.
        let rows: Vec<MessageRow> = app
            .rows(conv)
            .await
            .into_iter()
            .filter(|row| row.out_state.is_some() && matches!(row.body, Body::Media { .. }))
            .collect();
        let sent = rows.iter().all(|row| {
            matches!(
                row.out_state,
                Some(OutgoingState::Sent { .. } | OutgoingState::Delivered | OutgoingState::Read)
            )
        });
        if rows.len() == count && sent {
            return rows;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{count} sent file messages: {rows:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mixed_files_reach_every_phone_as_albums_and_single_files() {
    let Some(club) = Club::start().await else {
        return;
    };
    let mut app = club.app().await;
    let (account, device) = app.link(&club.world).await;
    let me = device.device_ref.clone();
    let ada = club.ada();
    let conv = club.chat(&app, account, ada, &device).await;
    let picked = tempfile::tempdir().expect("picked files");
    let dir = picked.path();

    // What the chat says about packing, and the split it gives this selection.
    let files = [
        gif(dir, "wave.gif", 1),
        png(dir, "summit.png", 2),
        document(dir, "route.gpx", 11),
        document(dir, "gear list.txt", 12),
        mp3(dir, "radio.mp3", 13),
        mp4(dir, "descent.mp4", 14),
        png(dir, "hut.png", 3),
    ];
    let features = app.features(conv).await;
    let plan = plan_of(&features, &files, Some("the trip"));
    assert_eq!(
        plan.messages,
        [vec![0, 1], vec![2], vec![3], vec![4], vec![5, 6]],
        "albums of pictures and MP4 videos, every other file alone, in the user's order"
    );
    assert!(!plan.lead_text, "the album carries the caption");

    // 1:1: one send, five messages on Ada's phone, seven uploads.
    let uploads = club.uploads().await;
    app.send(UiCommand::SendFiles {
        request: Default::default(),
        as_file: Vec::new(),
        conv,
        paths: files
            .iter()
            .map(|f| f.path.clone())
            .collect::<Vec<_>>()
            .into(),
        caption: Some(Text::plain("the trip")),
        reply_to: None,
    });
    let stamps = phone_gets(
        &club,
        ada,
        &me,
        None,
        &files,
        &plan.messages,
        Some("the trip"),
    )
    .await;
    club.nothing_more(ada, &device).await;
    assert_eq!(
        club.transcripts(&device, stamps.len()).await,
        stamps,
        "our phone gets a transcript of each message, in order"
    );
    let rows = sent_file_rows(&app, conv, plan.messages.len()).await;
    let shapes: Vec<usize> = rows
        .iter()
        .map(|row| match &row.body {
            Body::Media { attachments, .. } => attachments.len(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        shapes,
        [2, 1, 1, 1, 2],
        "the timeline shows the same messages"
    );
    assert!(matches!(&rows[0].body, Body::Media { caption: Some(c), .. } if c.body == "the trip"));
    assert_eq!(
        club.uploads().await - uploads,
        7,
        "each file is uploaded once"
    );

    // The group: every member's phone gets the album and each document; the files are
    // uploaded once, not once per member.
    let group_conv = club.group_conv(&app, account).await;
    let group_files = [
        png(dir, "topo.png", 4),
        gif(dir, "knot.gif", 5),
        document(dir, "rota.ods", 21),
        document(dir, "minutes.pdf.txt", 22),
    ];
    let plan = plan_of(
        &app.features(group_conv).await,
        &group_files,
        Some("for Saturday"),
    );
    assert_eq!(plan.messages, [vec![0, 1], vec![2], vec![3]]);
    let uploads = club.uploads().await;
    app.send(UiCommand::SendFiles {
        request: Default::default(),
        as_file: Vec::new(),
        conv: group_conv,
        paths: group_files
            .iter()
            .map(|f| f.path.clone())
            .collect::<Vec<_>>()
            .into(),
        caption: Some(Text::plain("for Saturday")),
        reply_to: None,
    });
    let mut per_member = Vec::new();
    for member in &club.members {
        per_member.push(
            phone_gets(
                &club,
                member,
                &me,
                Some(&club.group.id),
                &group_files,
                &plan.messages,
                Some("for Saturday"),
            )
            .await,
        );
    }
    assert!(
        per_member.windows(2).all(|pair| pair[0] == pair[1]),
        "every member gets the same three messages: {per_member:?}"
    );
    sent_file_rows(&app, group_conv, plan.messages.len()).await;
    assert_eq!(
        club.uploads().await - uploads,
        4,
        "one upload per file, whatever the number of members"
    );

    // A retry: the server rate-limits the first two sends to Ada. The outbox sends both
    // messages again; neither file is uploaded twice.
    let retried = [document(dir, "a.bin", 31), document(dir, "b.bin", 32)];
    let uploads = club.uploads().await;
    club.world
        .harness
        .rate_limit(&ada.device_ref, Some(2), 1)
        .await
        .expect("rate limit");
    app.send(UiCommand::SendFiles {
        request: Default::default(),
        as_file: Vec::new(),
        conv,
        paths: retried
            .iter()
            .map(|f| f.path.clone())
            .collect::<Vec<_>>()
            .into(),
        caption: None,
        reply_to: None,
    });
    phone_gets(&club, ada, &me, None, &retried, &[vec![0], vec![1]], None).await;
    club.nothing_more(ada, &device).await;
    assert_eq!(
        club.uploads().await - uploads,
        2,
        "a retried send reuses its upload"
    );

    app.stop().await;
    club.world.shutdown().await;
}

/// The packing goes by format, and a file sent "as file" is untouched (§3.8, §5.8): one send
/// of a JPEG, an MP4, a HEIC picture, a PNG and a JPEG sent as a file reaches Ada's phone as
/// the album of the JPEG and the MP4 (every client draws both), the HEIC picture alone (in an
/// album Desktop and iOS would show only the album's first item), the PNG alone because its
/// neighbour is a file, and the file: the original bytes under their name and true type,
/// where the same picture sent normally arrives re-encoded and nameless.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn formats_decide_the_packing_and_a_file_sent_as_file_is_untouched() {
    let Some(club) = Club::start().await else {
        return;
    };
    let mut app = club.app().await;
    let (account, device) = app.link(&club.world).await;
    let me = device.device_ref.clone();
    let ada = club.ada();
    let conv = club.chat(&app, account, ada, &device).await;
    let picked = tempfile::tempdir().expect("picked files");
    let dir = picked.path();
    let harness = &club.world.harness;

    let original = jpeg_with_a_note(640, 400, 51);
    let files = [
        pick(dir, "view.jpg", original.clone(), AttachmentKind::Image),
        mp4(dir, "clip.mp4", 52),
        pick(
            dir,
            "live.heic",
            iso_media(b"heic", 40_000, 53),
            AttachmentKind::Image,
        ),
        png(dir, "map.png", 54),
        // The same picture again, this time as the file it is.
        pick(dir, "original.jpg", original.clone(), AttachmentKind::File),
    ];
    assert_eq!(
        files.iter().map(|f| f.mime).collect::<Vec<_>>(),
        [
            "image/jpeg",
            "video/mp4",
            "image/heic",
            "image/png",
            "image/jpeg"
        ]
    );
    let as_file = vec![false, false, false, false, true];

    // The preview the composer shows and the send agree.
    let features = app.features(conv).await;
    let album = &features.file_packing.albums[0];
    assert!(album.takes(AttachmentKind::Video, "video/mp4"));
    assert!(!album.takes(AttachmentKind::Image, "image/heic"));
    assert!(!album.takes(AttachmentKind::Image, "image/heif"));
    assert!(!album.takes(AttachmentKind::Video, "video/quicktime"));
    let preview = match app
        .handle
        .query(Query::FilePlan(kourier_core::facade::FilePlanQuery {
            conv,
            paths: files
                .iter()
                .map(|f| f.path.clone())
                .collect::<Vec<_>>()
                .into(),
            as_file: as_file.clone(),
            caption: false,
            caption_text: None,
            reply: false,
        }))
        .await
        .expect("file plan")
    {
        QueryData::FilePlan(preview) => preview,
        other => panic!("unexpected {other:?}"),
    };
    assert!(preview.refused.is_empty(), "{:?}", preview.refused);
    let expected = [vec![0, 1], vec![2], vec![3], vec![4]];
    assert_eq!(preview.plan.messages, expected);

    let uploads = club.uploads().await;
    app.send(UiCommand::SendFiles {
        request: Default::default(),
        as_file,
        conv,
        paths: files
            .iter()
            .map(|f| f.path.clone())
            .collect::<Vec<_>>()
            .into(),
        caption: None,
        reply_to: None,
    });
    let mut messages = Vec::new();
    for n in 0..expected.len() {
        let got = harness
            .wait_for_data(&ada.aci, "message", Some(&me), None, WAIT)
            .await
            .unwrap_or_else(|e| panic!("message {n}: {e:?}"));
        messages.push(pointers_in(&got.data_message));
    }
    club.nothing_more(ada, &device).await;
    assert_eq!(
        messages.iter().map(Vec::len).collect::<Vec<_>>(),
        [2, 1, 1, 1],
        "an album, then each other file alone"
    );
    let fetch = |pointer: &Value| {
        let pointer = pointer.clone();
        async move {
            let fetched = harness
                .fetch_attachment(&pointer)
                .await
                .unwrap_or_else(|e| panic!("on the phone: {e:?}"));
            assert!(fetched.mac_valid && fetched.digest_valid, "{fetched:?}");
            fetched
        }
    };

    // The album: the picture re-encoded (its note is gone, it has no name), the video as it is.
    let (picture, video) = (&messages[0][0], &messages[0][1]);
    assert_eq!(picture["contentType"], "image/jpeg");
    assert!(
        picture.get("fileName").is_none_or(Value::is_null),
        "{picture}"
    );
    let arrived = fetch(picture).await.bytes();
    assert_ne!(arrived, original, "a normal picture is re-encoded");
    assert!(
        !arrived.windows(16).any(|w| w == &b"kourier original"[..]),
        "and loses its metadata"
    );
    assert_eq!(video["contentType"], "video/mp4");
    let clip = fetch(video).await;
    assert_eq!(clip.bytes(), files[1].bytes, "the video is byte-identical");
    assert_eq!(clip.incremental_mac_valid, Some(true));

    // The HEIC picture: alone, as it is (Kourier cannot decode it), under its own type.
    let heic = &messages[1][0];
    assert_eq!(heic["contentType"], "image/heic");
    assert_eq!(fetch(heic).await.bytes(), files[2].bytes);
    assert_eq!(messages[2][0]["contentType"], "image/png");

    // The file: the original bytes, its name and its true type; no size and no preview that
    // Kourier would have had to decode it for.
    let file = &messages[3][0];
    assert_eq!(file["contentType"], "image/jpeg", "{file}");
    assert_eq!(file["fileName"], "original.jpg", "{file}");
    assert_eq!(
        fetch(file).await.bytes(),
        original,
        "sent as a file: byte for byte what the user picked"
    );
    assert!(file.get("blurHash").is_none_or(Value::is_null), "{file}");
    assert!(
        file.get("width").is_none_or(|w| w.is_null() || w == 0),
        "{file}"
    );
    assert!(
        file.get("flags").is_none_or(|f| f.is_null() || f == 0),
        "{file}"
    );
    assert_eq!(club.uploads().await - uploads, 5, "each file once");

    // The timeline shows the same four messages, the last as a file with its name.
    let rows = sent_file_rows(&app, conv, expected.len()).await;
    let last = match &rows[3].body {
        Body::Media { attachments, .. } => attachments[0].clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(last.kind, AttachmentKind::File);
    assert_eq!(last.mime, "image/jpeg");
    assert_eq!(last.file_name.as_deref(), Some("original.jpg"));

    app.stop().await;
    club.world.shutdown().await;
}

#[derive(Serialize, Deserialize)]
struct ChildSpec {
    dir: PathBuf,
    env: BackendEnv,
    account: i64,
    conv: i64,
    paths: Vec<PathBuf>,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_multi_message_send_survives_a_kill_in_the_middle() {
    let Some(club) = Club::start().await else {
        return;
    };
    let mut app = club.app().await;
    let (account, device) = app.link(&club.world).await;
    let me = device.device_ref.clone();
    let ada = club.ada();
    let conv = club.chat(&app, account, ada, &device).await;
    let features = app.features(conv).await;
    app.stop().await;
    let picked = tempfile::tempdir().expect("picked files");
    let harness = &club.world.harness;

    // Where the app dies: after the three messages were stored; with the second message's
    // attempt recorded and nothing of it sent; with the second message on the server and its
    // outcome not recorded.
    let kills = [
        (kourier_core::FAILPOINT_AFTER_ENQUEUE, 1u32),
        (kourier_core::FAILPOINT_AFTER_ATTEMPT, 2),
        (kourier_core::FAILPOINT_BEFORE_OUTCOME_COMMIT, 2),
    ];
    for (round, (point, hit)) in kills.into_iter().enumerate() {
        let seed = 100 + 10 * round as u64;
        let dir = picked.path().join(format!("round-{round}"));
        std::fs::create_dir_all(&dir).expect("round dir");
        let files = [
            gif(&dir, "one.gif", seed),
            gif(&dir, "two.gif", seed + 1),
            document(&dir, "three.bin", seed + 2),
            document(&dir, "four.bin", seed + 3),
        ];
        let plan = plan_of(&features, &files, None);
        assert_eq!(plan.messages, [vec![0, 1], vec![2], vec![3]]);
        let uploads = club.uploads().await;

        let spec = ChildSpec {
            dir: club.world.tmp.path().to_owned(),
            env: club.world.env.clone(),
            account: account.0,
            conv: conv.0,
            paths: files.iter().map(|f| f.path.clone()).collect(),
        };
        let mut child = tokio::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "files_child_process",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, serde_json::to_string(&spec).expect("spec"))
            .env("KOURIER_FAILPOINT", format!("{point}:{hit}"))
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
        let sending = tokio::time::timeout(WAIT, async {
            while let Some(line) = lines.next_line().await.expect("child stdout") {
                if line.trim() == format!("{CHILD_LINE} sending") {
                    return true;
                }
            }
            false
        })
        .await;
        assert_eq!(sending, Ok(true), "{point}: the helper app sent the files");
        let status = tokio::time::timeout(WAIT, child.wait())
            .await
            .unwrap_or_else(|_| panic!("{point}: the helper app did not end"))
            .expect("helper status");
        assert_eq!(
            status.signal(),
            Some(6),
            "{point}: aborted at the failpoint"
        );

        // The next start finishes the send: every message once, in order, every file intact.
        let app = club.app().await;
        app.wait_online(account).await;
        phone_gets(&club, ada, &me, None, &files, &plan.messages, None).await;
        club.nothing_more(ada, &device).await;
        let rows = sent_file_rows(&app, conv, 3 * (round + 1)).await;
        assert_eq!(rows.len(), 3 * (round + 1), "{point}: no message twice");
        assert_eq!(
            club.uploads().await - uploads,
            4,
            "{point}: every file is uploaded exactly once across the kill"
        );
        app.stop().await;
    }
    // Our phone heard of every message (a transcript may be repeated after a kill, never
    // missing).
    let mut transcripts = std::collections::BTreeSet::new();
    while let Ok(sync) = harness
        .wait_for_sync_message(&club.world.phone.aci, Some(&me), "sent", QUIET)
        .await
    {
        let stamp = &sync.sync_message["content"]["sent"]["timestamp"];
        transcripts.insert(
            stamp
                .as_u64()
                .or_else(|| stamp.as_str().and_then(|t| t.parse().ok()))
                .expect("transcript timestamp"),
        );
    }
    assert_eq!(transcripts.len(), 9, "{transcripts:?}");
    club.world.shutdown().await;
}

/// The helper process of the kill test: the app over the given profile, online, then one
/// `SendFiles`, until the armed failpoint aborts it.
#[test]
fn files_child_process() {
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
        println!("{CHILD_LINE} sending");
        app.send(UiCommand::SendFiles {
            request: Default::default(),
            as_file: Vec::new(),
            conv: ConversationId(spec.conv),
            paths: spec.paths.into(),
            caption: None,
            reply_to: None,
        });
        tokio::time::sleep(Duration::from_secs(60)).await;
        // Only reached when the failpoint did not fire.
        println!("{CHILD_LINE} done");
        app.stop().await;
    });
}
