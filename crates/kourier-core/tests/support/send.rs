//! Shared setup of the outbox tests: a Signal-like account with two direct chats and a group,
//! errors by kind, and readers for `core.db` and the fake's `backend.db`.

use std::path::Path;
use std::time::Duration;

use kourier_backend_api::BackendEvent;
use kourier_fake::Script;
use kourier_fake::scripts::{T0, remote, signal_features};
use kourier_model::{
    AccountSettingsPatch, BackendError, ConvKey, ConversationId, ConversationPatch, ErrorKind,
    Member, MemberList, MemberRole, MessageId, OutgoingState, Patch, Redacted, UserId,
};

pub const ME: &str = "aci:00000000-0000-4000-8000-0000000000aa";
pub const ME_PNI: &str = "pni:00000000-0000-4000-8000-0000000000ab";
pub const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";
pub const BOB: &str = "aci:00000000-0000-4000-8000-000000000b0b";
pub const GROUP: &str = "gv2:b3V0Ym94LXRlc3QtZ3JvdXA";

pub fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("valid user id")
}

pub fn alice() -> UserId {
    uid(ALICE)
}

pub fn bob() -> UserId {
    uid(BOB)
}

pub fn d_alice() -> ConvKey {
    ConvKey::Direct(alice())
}

pub fn d_bob() -> ConvKey {
    ConvKey::Direct(bob())
}

pub fn group() -> ConvKey {
    ConvKey::Group(GROUP.into())
}

pub fn transient() -> BackendError {
    BackendError::new(ErrorKind::Transient { retry_after: None }, "socket closed")
}

pub fn rate_limited(retry_after: Option<Duration>) -> BackendError {
    BackendError::new(ErrorKind::RateLimited { retry_after }, "rate limited")
}

pub fn untrusted(user: UserId) -> BackendError {
    BackendError::new(ErrorKind::UntrustedIdentity { user }, "identity changed")
}

pub fn challenge() -> BackendError {
    BackendError::new(
        ErrorKind::Challenge {
            token: Redacted::new("token".into()),
            url: "https://example.invalid/challenge".into(),
            retry_after: None,
        },
        "challenge required",
    )
}

pub fn unregistered() -> BackendError {
    BackendError::new(ErrorKind::PeerUnregistered, "not registered")
}

fn conversation(key: ConvKey, members: Option<Vec<Member>>) -> BackendEvent {
    let mut p = ConversationPatch::new(key);
    p.members = members.map(MemberList::snapshot);
    p.features = Some(signal_features(p.members.is_some()));
    if p.members.is_some() {
        p.title = Patch::Set("Outbox Test".into());
    }
    BackendEvent::ConversationUpsert(p)
}

/// The setup delivery: settings (receipts and typing on), two direct chats, a group of alice,
/// bob and us, and one unread message from alice.
pub fn setup() -> Vec<BackendEvent> {
    let member = |user: UserId, role| Member { user, role };
    vec![
        BackendEvent::AccountSettings(AccountSettingsPatch {
            read_receipts: Some(true),
            typing_indicators: Some(true),
            ..Default::default()
        }),
        conversation(d_alice(), None),
        conversation(d_bob(), None),
        conversation(
            group(),
            Some(vec![
                member(alice(), MemberRole::Admin),
                member(bob(), MemberRole::Member),
                member(uid(ME), MemberRole::Member),
            ]),
        ),
        BackendEvent::Message(remote(
            &d_alice(),
            &alice(),
            &(T0 - 60_000).to_string(),
            T0 - 60_000,
            "are you there?",
        )),
    ]
}

/// A Signal-like account whose script only delivers [`setup`].
pub fn script(name: &str) -> Script {
    Script::new(name, vec![uid(ME), uid(ME_PNI)]).deliver(setup())
}

/// Reads `core.db` (read-only).
pub fn core_db<T>(path: &Path, f: impl FnOnce(&kourier_db::rusqlite::Connection) -> T) -> T {
    let conn = kourier_db::open_read_only(path, &kourier_db::DbKey::from(super::CORE_KEY))
        .expect("open core.db");
    f(&conn)
}

/// The conversation row of `key`.
pub fn conv_id(core: &Path, key: &ConvKey) -> Option<ConversationId> {
    core_db(core, |c| {
        c.query_row(
            "SELECT id FROM conversations WHERE conv_key = ?1",
            [key.encode()],
            |r| r.get(0).map(ConversationId),
        )
        .ok()
    })
}

/// An own message as stored.
#[derive(Clone, Debug, PartialEq)]
pub struct Own {
    pub id: MessageId,
    pub local: String,
    pub sent_at: i64,
    pub out_state: Option<OutgoingState>,
}

/// Own messages of a conversation, oldest first.
pub fn own_messages(core: &Path, conv: ConversationId) -> Vec<Own> {
    core_db(core, |c| {
        let mut stmt = c
            .prepare(
                "SELECT id, net_local, sent_at, out_state FROM messages
                 WHERE conversation_id = ?1 AND net_author = '' ORDER BY id",
            )
            .expect("prepare");
        stmt.query_map([conv.0], |r| {
            let state: Option<String> = r.get(3)?;
            Ok(Own {
                id: MessageId(r.get(0)?),
                local: r.get(1)?,
                sent_at: r.get(2)?,
                out_state: state.map(|s| serde_json::from_str(&s).expect("out_state json")),
            })
        })
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows")
    })
}

/// Number of outbox rows.
pub fn outbox_len(core: &Path) -> i64 {
    core_db(core, |c| {
        c.query_row("SELECT count(*) FROM outbox", [], |r| r.get(0))
            .expect("count")
    })
}

/// `send_state` of a message: (user, state JSON), by user.
pub fn send_state(core: &Path, message: MessageId) -> Vec<(String, String)> {
    core_db(core, |c| {
        let mut stmt = c
            .prepare("SELECT user_id, state FROM send_state WHERE message_id = ?1 ORDER BY user_id")
            .expect("prepare");
        stmt.query_map([message.0], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("query")
            .collect::<Result<Vec<_>, _>>()
            .expect("rows")
    })
}

/// Opens the fake's `backend.db` of an account read-only.
pub fn backend_db(
    path: &Path,
    key: &kourier_backend_api::DbKey,
) -> kourier_db::rusqlite::Connection {
    kourier_db::open_read_only(path, &kourier_db::DbKey::from_bytes(*key.expose()))
        .expect("open backend.db")
}

/// The fake server's log of accepted copies.
pub fn server_log(
    path: &Path,
    key: &kourier_backend_api::DbKey,
) -> Vec<kourier_fake::ServerDelivery> {
    kourier_fake::server_log(path, &kourier_db::DbKey::from_bytes(*key.expose()))
        .expect("server log")
}
