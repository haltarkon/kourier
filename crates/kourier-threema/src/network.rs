//! `ThreemaNetwork`: the descriptor, the id of an own message and the account actor's factory.
//!
//! It takes the transports as a REQUIRED argument ([`ThreemaParts`]): there is no constructor
//! without them, so no code path of this crate reaches a server unless the entry of `dial/` made the
//! parts (only `kourier-app` calls it). The maker is not called here, nor by `spawn`: the actor
//! calls it on its own thread when its account first connects or starts a link.

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{
    AccountContext, AccountHandle, AllocatedId, Network, spawn_account_thread,
};
use kourier_model::{
    BackendError, ConvKey, EditTarget, HistoryReadState, HistorySource, LoginFlow, MsgIdScope,
    NetworkDescriptor, NetworkKind, OutboxPolicy, RiskNotice, ServerRules, TypingPolicy, UserId,
};

use crate::ThreemaParts;
use crate::{actor, msg_id};

/// The most characters of the host name in the device label: `Kourier (` and `)` take 10 of the 64
/// grapheme clusters libthreema recommends for `D2mContext::device_label`, and a character is never
/// more than one cluster.
const HOST_MAX_CHARS: usize = 54;

/// The Threema network.
pub struct ThreemaNetwork {
    descriptor: NetworkDescriptor,
    /// The maker of a client's transports. NOT called here, nor by `spawn`.
    parts: ThreemaParts,
}

impl ThreemaNetwork {
    /// The Threema backend over `parts` (the maker is not called). Decisions that the descriptor
    /// carries:
    ///
    /// - `experimental` is `true` (his decision of 2026-10-16): `kourier-app` registers the network
    ///   only while its switch is on, and the lists show it as "Off · experimental" until then.
    /// - One login flow, `qr`: the phone scans the link's QR code; the emoji comparison is a STEP of
    ///   that flow (`LoginStepKind::ConfirmEmoji`), not a flow of its own.
    /// - `history` is `None`: Threema pushes no history to a new device (History Exchange later).
    /// - `msg_id_scope` is `Conversation`: Threema ids are unique per conversation and a reference
    ///   names no author.
    /// - `edit_target` is `Original`: an edit names the message first sent.
    /// - `start_chat` is empty: a chat from a Threema ID comes later.
    /// - `risk_notice` is set, so K-A's warning page asks first; the UI writes its own translated
    ///   words, these are the English fallback.
    /// - `shutdown_grace` is the core's default.
    pub fn new(parts: ThreemaParts) -> Self {
        Self {
            parts,
            descriptor: NetworkDescriptor {
                display_name: "Threema".to_owned(),
                // The key of the bundled network badge (`kourier-qt/icons/threema.svg`).
                icon_name: "threema".to_owned(),
                login_flows: vec![LoginFlow {
                    id: "qr".to_owned(),
                    label: "Link with a QR code".to_owned(),
                }],
                start_chat: Vec::new(),
                history: HistorySource::None,
                history_read: HistoryReadState::Known,
                history_transfer: false,
                experimental: true,
                msg_id_scope: MsgIdScope::Conversation,
                // As the fake network's flavours; the typing step checks Threema's own numbers.
                typing: TypingPolicy {
                    refresh: Duration::from_secs(10),
                    stop_after: Duration::from_secs(3),
                },
                risk_notice: Some(RiskNotice {
                    title: "Before You Turn On Threema".to_owned(),
                    body: "Kourier is not an official Threema app. It links as another device of your Threema account, and Threema may end that link.".to_owned(),
                    url: None,
                }),
                edit_target: EditTarget::Original,
                outbox: OutboxPolicy::default(),
                shutdown_grace: None,
                // No safety numbers (P5a) and no contact cards (P5c) on Threema in Kourier yet.
                safety_numbers: false,
                contact_cards: false,
                // Threema sends no files yet (LOC).
                cleaned_pictures: None,
                receipts_always_sent: false,
                // The clients count what is unread and check the deletes (KT-2).
                server: ServerRules::default(),
            },
        }
    }

    /// What the phone lists this device as: `Kourier (<host>)` from the host name the application
    /// gave to [`ThreemaParts::with_host_name`] (his decision D3, the same as WhatsApp's), plain
    /// `Kourier` without one. Control and bidi characters are dropped and the host is cut at
    /// [`HOST_MAX_CHARS`] characters, so the label stays within libthreema's 64 clusters.
    pub fn device_label(&self) -> String {
        device_label_of(self.parts.host_name())
    }
}

/// [`ThreemaNetwork::device_label`] of the host name `host` (the actor's join stores it).
pub(crate) fn device_label_of(host: &str) -> String {
    let host: String = host
        .chars()
        .filter(|c| !c.is_control() && !is_bidi_control(*c))
        .collect();
    let host: String = host.trim().chars().take(HOST_MAX_CHARS).collect();
    let host = host.trim();
    if host.is_empty() {
        "Kourier".to_owned()
    } else {
        format!("Kourier ({host})")
    }
}

/// The marks that reorder or isolate text (U+061C, U+200E, U+200F, U+202A-202E, U+2066-2069).
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

impl std::fmt::Debug for ThreemaNetwork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ThreemaNetwork")
    }
}

/// Threema's link to a person (`https://threema.id/<ID>`, Threema's "Share Link"): eight characters
/// of `A-Z` and `0-9`, or a gateway ID's leading `*` and seven of them; `None` for anything else.
fn id_link(id: &str) -> Option<String> {
    let body = id.strip_prefix('*').unwrap_or(id);
    let ok = id.len() == 8
        && body
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
    ok.then(|| format!("https://threema.id/{id}"))
}

impl Network for ThreemaNetwork {
    fn kind(&self) -> NetworkKind {
        NetworkKind::Threema
    }

    fn descriptor(&self) -> &NetworkDescriptor {
        &self.descriptor
    }

    fn contact_links(&self) -> bool {
        true
    }

    fn contact_id_link(&self, user: &UserId) -> Option<String> {
        (user.scheme() == "threema")
            .then(|| id_link(user.value()))
            .flatten()
    }

    /// A random message id (libthreema's generator) in libthreema's text form (`msg_id`): the
    /// trait allows a random id, the core persists it before the first transmit, so a retry reuses
    /// it. No I/O, no side effect; `own` is not needed. `sent_at` is the time the core gave.
    fn allocate_message_id(&self, _own: &[UserId], _conv: &ConvKey, now_ms: u64) -> AllocatedId {
        AllocatedId {
            local: Arc::from(msg_id::random()),
            sent_at: i64::try_from(now_ms).unwrap_or(i64::MAX),
        }
    }

    /// The account actor on a thread of its own (`threema-<uuid8>`, `crate::actor`). The parts go
    /// with it; their maker is not called here, nor by the actor in TH1.
    fn spawn(&self, ctx: AccountContext) -> Result<AccountHandle, BackendError> {
        let name = format!("threema-{}", ctx.uuid.short());
        let parts = self.parts.clone();
        spawn_account_thread(name, move || actor::run(ctx, parts))
    }
}

#[cfg(test)]
mod link_tests {
    use super::id_link;

    #[test]
    fn a_person_link_is_the_id_on_threemas_share_link_host() {
        assert_eq!(
            id_link("ECHOECHO").as_deref(),
            Some("https://threema.id/ECHOECHO")
        );
        assert_eq!(
            id_link("*GATEWAY").as_deref(),
            Some("https://threema.id/*GATEWAY")
        );
    }

    #[test]
    fn only_the_shape_of_an_id_makes_a_link() {
        for not in ["ECHO", "ECHOECHOX", "echoecho", "", "ECHO/CHO", "*ECHO"] {
            assert_eq!(id_link(not), None, "{not}");
        }
    }
}
