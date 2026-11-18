//! The forward command below the UI (forward line F3, architecture §4.8): `UiCommand::Forward`
//! on a core over two accounts that play WhatsApp (the network that states `forwards`).
//! Synthetic data only.

use crate::support;

use std::path::PathBuf;

use kourier_backend_api::{BackendEvent, OutgoingKind, TransmitOutcome};
use kourier_core::AccountVault;
use kourier_core::facade::{
    ForwardLook, ForwardRefusal, Query, QueryData, RequestId, UiCommand, UiEvent,
};
use kourier_fake::attachments::Pointer;
use kourier_fake::scripts::{T0, remote, whatsapp_features};
use kourier_fake::{Script, TransmitRecord};
use kourier_media::{SampleFormat, sample_image};
use kourier_model::{
    AccountId, AccountUuid, Attachment, AttachmentId, AttachmentKind, AttachmentState, Body,
    CannotSend, ConvKey, ConversationId, ConversationPatch, Forwarded, MessageId, NetworkKind,
    Text, UserId,
};
use support::send::{ME, alice, d_alice, uid};
use support::{Env, WAIT, add_linked_as};

const ME2: &str = "aci:00000000-0000-4000-8000-0000000000cc";

const TEXT: i64 = 1;
const VIDEO: i64 = 2;
const MANY: i64 = 3;
const TWICE: i64 = 4;
const DELETED: i64 = 5;

fn person(n: u8) -> UserId {
    uid(&format!(
        "aci:00000000-0000-4000-8000-0000000000{:02x}",
        0x20 + n
    ))
}

fn chat(key: ConvKey) -> BackendEvent {
    let mut p = ConversationPatch::new(key);
    p.features = Some(whatsapp_features(false));
    BackendEvent::ConversationUpsert(p)
}

fn event(local: i64, body: Body) -> kourier_backend_api::IncomingMessage {
    let mut m = remote(&d_alice(), &alice(), &local.to_string(), T0 + local, "");
    m.body = body;
    m
}

/// Seventy messages in one chat: the video first, then texts.
const BIG: usize = 70;

fn script_a() -> Script {
    let video = Attachment {
        kind: AttachmentKind::Video,
        mime: "video/mp4".into(),
        size: 5000,
        remote: Some(Pointer::Bytes { len: 5000, seed: 7 }.to_blob()),
        ..Attachment::default()
    };
    let mut many = event(MANY, Body::text("many"));
    many.forwarded = Some(Forwarded { score: 5 });
    let mut twice = event(TWICE, Body::text("twice"));
    twice.forwarded = Some(Forwarded { score: 2 });
    let mut events = vec![chat(d_alice())];
    for n in 1..=6 {
        events.push(chat(ConvKey::Direct(person(n))));
    }
    let mut closed = ConversationPatch::new(ConvKey::Direct(person(7)));
    let mut f = whatsapp_features(false);
    f.cannot_send = Some(CannotSend::NotMember);
    closed.features = Some(f);
    events.push(BackendEvent::ConversationUpsert(closed));
    events.push(chat(ConvKey::Direct(person(50))));
    events.extend([
        BackendEvent::Message(event(TEXT, Body::text("hello"))),
        BackendEvent::Message(event(
            VIDEO,
            Body::Media {
                attachments: vec![video.clone()],
                caption: None,
            },
        )),
        BackendEvent::Message(many),
        BackendEvent::Message(twice),
        BackendEvent::Message(event(DELETED, Body::text("deleted"))),
    ]);
    let big = ConvKey::Direct(person(50));
    events.push(BackendEvent::Message({
        let mut m = remote(&big, &person(50), "big0", T0 + 1000, "");
        m.body = Body::Media {
            attachments: vec![video],
            caption: None,
        };
        m
    }));
    for i in 1..BIG {
        events.push(BackendEvent::Message(remote(
            &big,
            &person(50),
            &format!("big{i}"),
            T0 + 1000 + i as i64,
            &format!("line {i}"),
        )));
    }
    Script::new("fwd-a", vec![uid(ME)])
        .deliver(events)
        .deliver(vec![BackendEvent::Deleted {
            target: kourier_fake::scripts::target(
                Some(&d_alice()),
                Some(&alice()),
                &DELETED.to_string(),
            ),
            by: Some(alice()),
            for_everyone: true,
            at: T0 + 50_000,
        }])
}

fn script_b() -> Script {
    Script::new("fwd-b", vec![uid(ME2)]).deliver(vec![
        chat(ConvKey::Direct(person(40))),
        BackendEvent::Message(remote(
            &ConvKey::Direct(person(40)),
            &person(40),
            "x",
            T0 + 1,
            "hi",
        )),
    ])
}

struct Scene {
    env: Env,
    source: ConversationId,
    big: ConversationId,
    open: Vec<ConversationId>,
    closed: ConversationId,
    b_chat: ConversationId,
    account_a: AccountId,
    account_b: AccountId,
    uuid_a: AccountUuid,
    uuid_b: AccountUuid,
    text: MessageId,
    video: MessageId,
    many: MessageId,
    twice: MessageId,
    deleted: MessageId,
    request: u64,
}

impl Scene {
    async fn new() -> Self {
        let mut env = Env::playing(NetworkKind::WhatsApp, script_a()).await;
        env.handle.send(UiCommand::UiReady).expect("send");
        let account_a = add_linked_as(&mut env, NetworkKind::WhatsApp).await;
        env.wait_finished(account_a).await;
        env.control.push_script(script_b());
        let account_b = add_linked_as(&mut env, NetworkKind::WhatsApp).await;
        env.wait_finished(account_b).await;
        let core = env.core_db();
        let id = |key: ConvKey| {
            support::send::conv_id(&core, &key).unwrap_or_else(|| panic!("chat {key:?}"))
        };
        let source = id(d_alice());
        let big = id(ConvKey::Direct(person(50)));
        let deadline = tokio::time::Instant::now() + WAIT * 3;
        let rows = loop {
            let rows = env.timeline(source).await;
            let bigrows = env.timeline(big).await;
            if rows.len() == 5
                && rows
                    .iter()
                    .any(|r| r.sent_at == T0 + DELETED && r.deleted.is_some())
                && bigrows.len() == BIG
            {
                break rows;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the scene did not settle"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        let at = |n: i64| rows.iter().find(|r| r.sent_at == T0 + n).expect("row").id;
        let accounts = env.accounts().await;
        let uuid = |a: AccountId| accounts.iter().find(|r| r.id == a).expect("account").uuid;
        Self {
            text: at(TEXT),
            video: at(VIDEO),
            many: at(MANY),
            twice: at(TWICE),
            deleted: at(DELETED),
            open: (1..=6).map(|n| id(ConvKey::Direct(person(n)))).collect(),
            closed: id(ConvKey::Direct(person(7))),
            b_chat: id(ConvKey::Direct(person(40))),
            uuid_a: uuid(account_a),
            uuid_b: uuid(account_b),
            source,
            big,
            account_a,
            account_b,
            env,
            request: 0,
        }
    }

    fn video_attachment(&self) -> AttachmentId {
        self.env.peek(|c| {
            c.query_row(
                "SELECT a.id FROM attachments a WHERE a.message_id = ?1",
                [self.video.0],
                |r| r.get::<_, i64>(0),
            )
            .map(AttachmentId)
            .expect("attachment")
        })
    }

    /// Downloads the video of the source chat and of the big chat.
    async fn download(&self) {
        let ids: Vec<AttachmentId> = self.env.peek(|c| {
            c.prepare("SELECT id FROM attachments ORDER BY id")
                .unwrap()
                .query_map([], |r| r.get::<_, i64>(0))
                .unwrap()
                .map(|r| AttachmentId(r.unwrap()))
                .collect()
        });
        for id in ids {
            self.env
                .handle
                .send(UiCommand::DownloadAttachment { attachment: id })
                .expect("send");
        }
        let env = &self.env;
        support::eventually("downloads", WAIT, || async {
            env.peek(|c| {
                c.query_row(
                    "SELECT count(*) FROM attachments WHERE state <> '\"Local\"'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap()
                    == 0
            })
        })
        .await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn forward(
        &mut self,
        messages: Vec<MessageId>,
        targets: Vec<ConversationId>,
        look: ForwardLook,
        comment: Option<&str>,
        headers: Vec<(MessageId, Text)>,
        pictures: Vec<PathBuf>,
        without: Vec<AttachmentId>,
    ) -> (Vec<ConversationId>, Vec<(ConversationId, ForwardRefusal)>) {
        self.forward_from(
            self.source,
            messages,
            targets,
            look,
            comment,
            headers,
            pictures,
            without,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn forward_from(
        &mut self,
        source: ConversationId,
        messages: Vec<MessageId>,
        targets: Vec<ConversationId>,
        look: ForwardLook,
        comment: Option<&str>,
        headers: Vec<(MessageId, Text)>,
        pictures: Vec<PathBuf>,
        without: Vec<AttachmentId>,
    ) -> (Vec<ConversationId>, Vec<(ConversationId, ForwardRefusal)>) {
        self.request += 1;
        let request = RequestId(self.request);
        self.env
            .handle
            .send(UiCommand::Forward {
                request,
                source,
                messages,
                targets,
                look,
                headers,
                comment: comment.map(Text::plain),
                pictures: pictures.into(),
                without,
            })
            .expect("send");
        self.env
            .event("the end of the forward", |e| match e {
                UiEvent::ForwardEnded {
                    request: r,
                    sent,
                    refused,
                } if *r == request => Some((sent.clone(), refused.clone())),
                _ => None,
            })
            .await
    }

    /// The own messages of `conv` in storage order: (id, stored forward score, body).
    fn own(&self, conv: ConversationId) -> Vec<(MessageId, Option<u32>, Body)> {
        self.env.peek(|c| {
            c.prepare(
                "SELECT id, forwarded, body FROM messages WHERE conversation_id = ?1
                   AND origin = 'OwnThisDevice' ORDER BY id",
            )
            .or_else(|_| {
                c.prepare("SELECT id, forwarded, body FROM messages WHERE conversation_id = ?1 AND author IS NULL ORDER BY id")
            })
            .unwrap()
            .query_map([conv.0], |r| {
                let body: String = r.get(2)?;
                Ok((
                    MessageId(r.get(0)?),
                    r.get::<_, Option<i64>>(1)?.map(|s| s as u32),
                    serde_json::from_str(&body).expect("body"),
                ))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect()
        })
    }

    async fn wait_sent(&self, uuid: AccountUuid, key: &ConvKey, n: usize) -> Vec<TransmitRecord> {
        let control = self.env.control.clone();
        let k = key.clone();
        let pick = move |c: &kourier_fake::FakeControl| -> Vec<TransmitRecord> {
            c.transmits(uuid)
                .into_iter()
                .filter(|t| {
                    t.req.conv == k
                        && matches!(t.req.kind, OutgoingKind::Message { .. })
                        && matches!(t.outcome, TransmitOutcome::Sent { .. })
                })
                .collect()
        };
        tokio::time::timeout(WAIT, control.wait_for(|c| pick(c).len() >= n))
            .await
            .unwrap_or_else(|_| panic!("{n} messages sent"));
        pick(&self.env.control)
    }
}

fn text_of(body: &Body) -> String {
    match body {
        Body::Text(t) => t.body.clone(),
        Body::Media { caption, .. } => caption.as_ref().map(|t| t.body.clone()).unwrap_or_default(),
        other => panic!("{other:?}"),
    }
}

fn forward_of(t: &TransmitRecord) -> Option<Forwarded> {
    match &t.req.kind {
        OutgoingKind::Message { forward, .. } => *forward,
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_native_forward_marks_its_rows_and_requests_and_keeps_the_pointer_only_in_the_request() {
    let mut s = Scene::new().await;
    s.download().await;
    let target = s.open[0];
    let (sent, refused) = s
        .forward(
            vec![s.text, s.video, s.twice],
            vec![target],
            ForwardLook::Plain,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .await;
    assert_eq!((sent, refused), (vec![target], vec![]));
    let rows = s.own(target);
    assert_eq!(rows.len(), 3);
    // Score 0 for a first forward, `prev + 1` for one that was a forward (2 -> 3).
    assert_eq!(
        rows.iter().map(|r| r.1).collect::<Vec<_>>(),
        vec![Some(0), Some(0), Some(3)]
    );
    // The stored attachment row keeps no pointer and is Local.
    let (remote, state): (Option<Vec<u8>>, String) = s.env.peek(|c| {
        c.query_row(
            "SELECT remote, state FROM attachments WHERE message_id = ?1",
            [rows[1].0.0],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    });
    assert!(remote.is_none());
    assert_eq!(state, "\"Local\"");
    let sent = s.wait_sent(s.uuid_a, &ConvKey::Direct(person(1)), 3).await;
    assert_eq!(
        sent.iter().map(forward_of).collect::<Vec<_>>(),
        vec![
            Some(Forwarded { score: 0 }),
            Some(Forwarded { score: 0 }),
            Some(Forwarded { score: 3 })
        ]
    );
    let OutgoingKind::Message {
        body: Body::Media { attachments, .. },
        ..
    } = &sent[1].req.kind
    else {
        panic!("media")
    };
    assert!(
        attachments[0].remote.is_some(),
        "the source's pointer is in the request"
    );
    assert!(matches!(attachments[0].state, AttachmentState::Local));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_converted_forward_with_a_header_has_no_mark_and_the_comment_goes_first() {
    let mut s = Scene::new().await;
    let header = Text::plain("Forwarded from Alice");
    let (sent, refused) = s
        .forward(
            vec![s.text],
            vec![s.b_chat],
            ForwardLook::Header,
            Some("look at this"),
            vec![(s.text, header)],
            Vec::new(),
            Vec::new(),
        )
        .await;
    assert_eq!((sent, refused), (vec![s.b_chat], vec![]));
    let rows = s.own(s.b_chat);
    assert_eq!(rows.len(), 2);
    assert_eq!(text_of(&rows[0].2), "look at this");
    assert_eq!(text_of(&rows[1].2), "Forwarded from Alice\nhello");
    assert!(rows.iter().all(|r| r.1.is_none()));
    let sent = s.wait_sent(s.uuid_b, &ConvKey::Direct(person(40)), 2).await;
    assert!(sent.iter().all(|t| forward_of(t).is_none()));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_comment_goes_first_as_a_text_in_every_target_and_has_no_mark() {
    let mut s = Scene::new().await;
    let (sent, _) = s
        .forward(
            vec![s.text],
            vec![s.open[0], s.b_chat],
            ForwardLook::Plain,
            Some("first"),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .await;
    assert_eq!(sent.len(), 2);
    for t in [s.open[0], s.b_chat] {
        let rows = s.own(t);
        assert_eq!(text_of(&rows[0].2), "first");
        assert_eq!(rows[0].1, None, "the comment is no forward");
        assert_eq!(text_of(&rows[1].2), "hello");
    }
    assert_eq!(s.own(s.open[0])[1].1, Some(0));
    assert_eq!(s.own(s.b_chat)[1].1, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_blob_is_copied_into_the_target_accounts_store_and_outlives_the_source() {
    let mut s = Scene::new().await;
    s.download().await;
    let (sent, _) = s
        .forward(
            vec![s.video],
            vec![s.open[0], s.b_chat],
            ForwardLook::Plain,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .await;
    assert_eq!(sent.len(), 2);
    let file_of = |msg: MessageId| -> (PathBuf, [u8; 32], i64) {
        s.env.peek(|c| {
            c.query_row(
                "SELECT f.path, f.key, f.account_id FROM attachments a JOIN files f ON f.id = a.file_id
                 WHERE a.message_id = ?1",
                [msg.0],
                |r| {
                    let k: Vec<u8> = r.get(1)?;
                    Ok((PathBuf::from(r.get::<_, String>(0)?), k.try_into().unwrap(), r.get(2)?))
                },
            )
            .unwrap()
        })
    };
    let dir = |a: AccountUuid| s.env.vault.blob_dir(a);
    let (src_path, src_key, _) = file_of(s.video);
    let src_abs = dir(s.uuid_a).join(&src_path);
    let src_bytes = std::fs::read(&src_abs).expect("source file");
    let plain = kourier_blob::read_all(&src_abs, &kourier_blob::FileKey::from_bytes(src_key))
        .expect("decrypt")
        .to_vec();
    for (conv, uuid, account) in [
        (s.open[0], s.uuid_a, s.account_a),
        (s.b_chat, s.uuid_b, s.account_b),
    ] {
        let msg = s.own(conv)[0].0;
        let (path, key, owner) = file_of(msg);
        assert_eq!(owner, account.0, "a files row of the TARGET account");
        assert_ne!(path, src_path, "a new blob");
        let abs = dir(uuid).join(&path);
        assert_eq!(
            std::fs::read(&abs).expect("copy"),
            src_bytes,
            "byte for byte"
        );
        assert_eq!(key, src_key, "the same key");
    }
    // The source goes away: the forwarded ones stay readable.
    std::fs::remove_file(&src_abs).unwrap();
    for (conv, uuid) in [(s.open[0], s.uuid_a), (s.b_chat, s.uuid_b)] {
        let (path, key, _) = file_of(s.own(conv)[0].0);
        let got = kourier_blob::read_all(
            &dir(uuid).join(path),
            &kourier_blob::FileKey::from_bytes(key),
        )
        .expect("still readable");
        assert_eq!(got.to_vec(), plain);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn three_targets_the_middle_one_closed_two_sent_one_refused_one_event() {
    let mut s = Scene::new().await;
    let (sent, refused) = s
        .forward(
            vec![s.text],
            vec![s.open[0], s.closed, s.open[1]],
            ForwardLook::Plain,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .await;
    assert_eq!(sent, vec![s.open[0], s.open[1]]);
    assert_eq!(
        refused,
        vec![(s.closed, ForwardRefusal::CannotSend(CannotSend::NotMember))]
    );
    assert_eq!(s.own(s.closed).len(), 0);
    assert_eq!(s.own(s.open[0]).len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sixth_whatsapp_chat_and_the_second_native_chat_of_a_many_message_are_refused() {
    let mut s = Scene::new().await;
    let (sent, refused) = s
        .forward(
            vec![s.text],
            s.open.clone(),
            ForwardLook::Plain,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .await;
    assert_eq!(sent, s.open[..5].to_vec());
    assert_eq!(refused, vec![(s.open[5], ForwardRefusal::TooManyChats)]);
    let (sent, refused) = s
        .forward(
            vec![s.many],
            vec![s.open[0], s.open[1]],
            ForwardLook::Plain,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .await;
    assert_eq!(sent, vec![s.open[0]]);
    assert_eq!(refused, vec![(s.open[1], ForwardRefusal::TooManyChats)]);
    // Score 5 forwarded again is 6.
    assert_eq!(s.own(s.open[0]).last().unwrap().1, Some(6));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_that_no_longer_goes_refuses_every_target_and_stores_nothing() {
    let mut s = Scene::new().await;
    let (sent, refused) = s
        .forward(
            vec![s.text, s.deleted],
            vec![s.open[0], s.b_chat],
            ForwardLook::Plain,
            Some("hi"),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .await;
    assert!(sent.is_empty());
    assert_eq!(
        refused,
        vec![
            (s.open[0], ForwardRefusal::SourceChanged),
            (s.b_chat, ForwardRefusal::SourceChanged)
        ]
    );
    assert!(s.own(s.open[0]).is_empty() && s.own(s.b_chat).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_that_is_not_here_refuses_unless_the_user_left_it_out() {
    let mut s = Scene::new().await;
    let att = s.video_attachment();
    let (sent, refused) = s
        .forward(
            vec![s.text, s.video],
            vec![s.open[0]],
            ForwardLook::Plain,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .await;
    assert!(sent.is_empty());
    assert_eq!(refused, vec![(s.open[0], ForwardRefusal::FileNotHere)]);
    assert!(s.own(s.open[0]).is_empty());
    let (sent, refused) = s
        .forward(
            vec![s.text, s.video],
            vec![s.open[0]],
            ForwardLook::Plain,
            None,
            Vec::new(),
            Vec::new(),
            vec![att],
        )
        .await;
    assert_eq!((sent, refused), (vec![s.open[0]], vec![]));
    let rows = s.own(s.open[0]);
    assert_eq!(rows.len(), 1, "the video was left out, the text went");
    assert_eq!(text_of(&rows[0].2), "hello");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failure_in_a_later_part_leaves_nothing_of_that_target_and_no_blob_copy() {
    let mut s = Scene::new().await;
    s.download().await;
    let ids: Vec<MessageId> = {
        let mut rows = s.env.timeline(s.big).await;
        rows.sort_by_key(|r| r.sent_at);
        rows.iter().map(|r| r.id).collect()
    };
    assert_eq!(ids.len(), BIG);
    let blobs = |s: &Scene, u| support::blob_files(&s.env.vault.blob_dir(u));
    let before = blobs(&s, s.uuid_a);
    kourier_core::facade::inject_forward_fault(Some(s.open[0]));
    let (sent, refused) = s
        .forward_from(
            s.big,
            ids,
            vec![s.open[0], s.open[1]],
            ForwardLook::Plain,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .await;
    assert_eq!(sent, vec![s.open[1]]);
    assert_eq!(refused, vec![(s.open[0], ForwardRefusal::Failed)]);
    assert!(
        s.own(s.open[0]).is_empty(),
        "nothing of the failed target stays"
    );
    assert_eq!(s.own(s.open[1]).len(), BIG);
    let outbox_staged: i64 = s.env.peek(|c| {
        c.query_row(
            "SELECT count(*) FROM outbox WHERE state = 'Staged'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    });
    assert_eq!(outbox_staged, 0);
    // The other target's copy is the only new blob (plus nothing from the failed one).
    assert_eq!(blobs(&s, s.uuid_a), before + 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_picture_look_sends_the_given_pictures_to_a_converted_target_only() {
    let mut s = Scene::new().await;
    let dir = s.env.dir.path().join("pictures");
    std::fs::create_dir_all(&dir).unwrap();
    let pictures: Vec<PathBuf> = (0..2u8)
        .map(|i| {
            let p = dir.join(format!("p{i}.png"));
            std::fs::write(
                &p,
                sample_image(48, 36, u64::from(i), SampleFormat::Png).expect("png"),
            )
            .unwrap();
            p
        })
        .collect();
    let (sent, refused) = s
        .forward(
            vec![s.text],
            vec![s.b_chat, s.open[0]],
            ForwardLook::Picture,
            Some("as a picture"),
            Vec::new(),
            pictures,
            Vec::new(),
        )
        .await;
    assert_eq!((sent, refused), (vec![s.b_chat, s.open[0]], vec![]));
    // WhatsApp packs one picture per message: the chat's own rule, the caption on the first.
    let b = s.own(s.b_chat);
    assert_eq!(b.len(), 2, "the two pictures, nothing else");
    for (i, m) in b.iter().enumerate() {
        let Body::Media {
            attachments,
            caption,
        } = &m.2
        else {
            panic!("media")
        };
        assert_eq!(attachments.len(), 1);
        assert_eq!(
            caption.as_ref().map(|t| t.body.as_str()),
            (i == 0).then_some("as a picture")
        );
        assert_eq!(m.1, None);
    }
    // The native target got the comment as a text first, then the native forward.
    let a = s.own(s.open[0]);
    assert_eq!(a.len(), 2);
    assert_eq!(text_of(&a[0].2), "as a picture");
    assert_eq!(a[0].1, None);
    assert_eq!((text_of(&a[1].2), a[1].1), ("hello".to_owned(), Some(0)));
    let _ = Query::Conversation;
    let _: Option<QueryData> = None;
}
