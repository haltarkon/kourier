//! The forward plan below the UI (forward line F2): `Query::ForwardPlan` on a core over two
//! accounts that play WhatsApp (the network that states `forwards`). Synthetic data only.

use crate::support;

use kourier_backend_api::BackendEvent;
use kourier_core::facade::{
    ForwardChange, ForwardPlan, ForwardRefusal, ForwardVerdict, MessageRow, Query, QueryData,
    UiCommand,
};
use kourier_fake::Script;
use kourier_fake::attachments::Pointer;
use kourier_fake::scripts::{T0, remote, whatsapp_features};
use kourier_model::{
    AccountId, Attachment, AttachmentKind, Body, CannotSend, ConvKey, ConversationId,
    ConversationPatch, Forwarded, MessageId, NetworkKind, SystemEvent, Text, UserId,
};
use support::send::{ME, alice, bob, d_alice, d_bob, uid};
use support::{Env, add_linked_as};

const ME2: &str = "aci:00000000-0000-4000-8000-0000000000cc";

const TEXT: i64 = 1;
const VIDEO: i64 = 2;
const ONCE: i64 = 3;
const NOTICE: i64 = 4;
const DELETED: i64 = 5;
const MANY: i64 = 6;

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

fn event(local: i64, body: Body, view_once: bool) -> kourier_backend_api::IncomingMessage {
    let mut m = remote(&d_alice(), &alice(), &local.to_string(), T0 + local, "");
    m.body = body;
    m.view_once = view_once;
    m
}

fn script_a() -> Script {
    let video = Attachment {
        kind: AttachmentKind::Video,
        mime: "video/mp4".into(),
        size: 5000,
        remote: Some(Pointer::Bytes { len: 5000, seed: 7 }.to_blob()),
        ..Attachment::default()
    };
    let mut many = event(MANY, Body::text("many"), false);
    many.forwarded = Some(Forwarded { score: 5 });
    let mut events = vec![chat(d_alice()), chat(d_bob())];
    // Six more chats and one we cannot send in.
    for n in 1..=6 {
        events.push(chat(ConvKey::Direct(person(n))));
    }
    let mut closed = ConversationPatch::new(ConvKey::Direct(person(7)));
    let mut f = whatsapp_features(false);
    f.cannot_send = Some(CannotSend::NotMember);
    closed.features = Some(f);
    events.push(BackendEvent::ConversationUpsert(closed));
    events.extend([
        BackendEvent::Message(event(TEXT, Body::text("hello"), false)),
        BackendEvent::Message(event(
            VIDEO,
            Body::Media {
                attachments: vec![video],
                caption: None,
            },
            false,
        )),
        BackendEvent::Message(event(ONCE, Body::text("once"), true)),
        BackendEvent::Message(event(
            NOTICE,
            Body::System(SystemEvent::TimerChanged {
                by: Some(alice()),
                timer: None,
            }),
            false,
        )),
        BackendEvent::Message(event(DELETED, Body::text("deleted"), false)),
        BackendEvent::Message(many),
        BackendEvent::Message(remote(&d_bob(), &bob(), "b1", T0 + 100, "in another chat")),
    ]);
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
    rows: Vec<MessageRow>,
    other: MessageId,
    /// Six chats of account A we can send in, the closed one, and account B's chat.
    open: Vec<ConversationId>,
    closed: ConversationId,
    b_chat: ConversationId,
    account_a: AccountId,
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
        let deadline = tokio::time::Instant::now() + support::WAIT * 3;
        let rows = loop {
            let rows: Vec<MessageRow> = env.timeline(source).await;
            if rows.len() == 6
                && rows
                    .iter()
                    .any(|r| r.sent_at == T0 + DELETED && r.deleted.is_some())
            {
                break rows;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the scene did not settle: {} rows, sent_at {:?}, deleted {:?}",
                rows.len(),
                rows.iter().map(|r| r.sent_at - T0).collect::<Vec<_>>(),
                rows.iter().map(|r| r.deleted.is_some()).collect::<Vec<_>>()
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        let other = env.timeline(id(d_bob())).await[0].id;
        Self {
            open: (1..=6).map(|n| id(ConvKey::Direct(person(n)))).collect(),
            closed: id(ConvKey::Direct(person(7))),
            b_chat: id(ConvKey::Direct(person(40))),
            env,
            source,
            rows,
            other,
            account_a,
        }
    }

    fn row(&self, n: i64) -> &MessageRow {
        self.rows.iter().find(|r| r.sent_at == T0 + n).expect("row")
    }

    async fn plan(&self, messages: Vec<MessageId>, targets: Vec<ConversationId>) -> ForwardPlan {
        match self
            .env
            .handle
            .query(Query::ForwardPlan {
                source: self.source,
                messages,
                targets,
            })
            .await
        {
            Ok(QueryData::ForwardPlan(p)) => *p,
            other => panic!("{other:?}"),
        }
    }
}

fn verdict(p: &ForwardPlan, id: MessageId) -> ForwardVerdict {
    p.messages
        .iter()
        .find(|m| m.id == id)
        .expect("asked")
        .verdict
}

#[tokio::test(flavor = "multi_thread")]
async fn the_verdicts_of_a_mixed_selection_and_the_flag_agree() {
    let s = Scene::new().await;
    let mut ids: Vec<MessageId> = s.rows.iter().map(|r| r.id).collect();
    ids.push(s.other);
    let p = s.plan(ids, vec![s.open[0]]).await;
    assert_eq!(verdict(&p, s.row(TEXT).id), ForwardVerdict::Goes);
    assert_eq!(verdict(&p, s.row(VIDEO).id), ForwardVerdict::Goes);
    assert_eq!(verdict(&p, s.row(ONCE).id), ForwardVerdict::ViewOnce);
    assert_eq!(verdict(&p, s.row(NOTICE).id), ForwardVerdict::Notice);
    assert_eq!(verdict(&p, s.row(DELETED).id), ForwardVerdict::Deleted);
    assert_eq!(verdict(&p, s.other), ForwardVerdict::Gone);
    // `Goes` iff the row's flag says so.
    for m in &p.messages {
        if let Some(row) = &m.row {
            assert_eq!(
                m.verdict == ForwardVerdict::Goes,
                row.actions.forward,
                "{:?}",
                m.verdict
            );
        }
    }
    // Timeline order, the gone one last.
    let order: Vec<MessageId> = p.messages.iter().map(|m| m.id).collect();
    assert_eq!(order.last(), Some(&s.other));
    assert!(p.limits.iter().any(|(a, _)| *a == s.account_a));
}

#[tokio::test(flavor = "multi_thread")]
async fn native_and_converted_targets_and_a_closed_chat() {
    let s = Scene::new().await;
    let p = s
        .plan(
            vec![s.row(TEXT).id],
            vec![s.open[0], s.b_chat, s.closed, ConversationId(987_654)],
        )
        .await;
    assert!(p.targets[0].native && p.targets[0].refused.is_none());
    assert!(!p.targets[1].native && p.targets[1].refused.is_none());
    assert_eq!(
        p.targets[2].refused,
        Some(ForwardRefusal::CannotSend(CannotSend::NotMember))
    );
    assert_eq!(p.targets[3].refused, Some(ForwardRefusal::UnknownChat));
    assert!(p.targets[0].changes.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sixth_chat_of_one_account_is_refused() {
    let s = Scene::new().await;
    let p = s.plan(vec![s.row(TEXT).id], s.open.clone()).await;
    for t in &p.targets[..5] {
        assert_eq!(t.refused, None);
    }
    assert_eq!(p.targets[5].refused, Some(ForwardRefusal::TooManyChats));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_forwarded_many_times_goes_to_one_native_chat() {
    let s = Scene::new().await;
    let p = s
        .plan(vec![s.row(MANY).id], vec![s.open[0], s.open[1], s.b_chat])
        .await;
    assert!(p.many);
    assert_eq!(p.targets[0].refused, None);
    assert_eq!(p.targets[1].refused, Some(ForwardRefusal::TooManyChats));
    assert_eq!(p.targets[2].refused, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_that_is_not_here_is_to_download_and_nothing_to_send_refuses() {
    let s = Scene::new().await;
    let video = &s.row(VIDEO);
    let Body::Media { attachments, .. } = &video.body else {
        panic!("media")
    };
    let id = attachments[0].id.expect("id");
    let p = s.plan(vec![video.id], vec![s.open[0]]).await;
    assert_eq!(p.to_download, vec![id]);
    assert!(p.lost.is_empty());
    let p = s.plan(vec![s.row(ONCE).id], vec![s.open[0]]).await;
    assert_eq!(p.targets[0].refused, Some(ForwardRefusal::NothingToSend));
    assert!(p.to_download.is_empty());
    let _: Option<ForwardChange> = None;
    let _: Option<Text> = None;
}
