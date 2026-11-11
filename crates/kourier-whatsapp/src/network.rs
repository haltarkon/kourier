//! `WhatsAppNetwork`: the descriptor, the id of an own message and the account actor's factory.
//!
//! The actor opens the store, serves the commands and drives the login machine over a `Session`; the
//! real client behind that `Session` is built from the parts at the first `Connect` or `StartLogin`
//! (`session::client`). The app registers it (W2b) in a debug build only, while the user has it on.
//!
//! It takes the transport factory and the HTTP client as a REQUIRED argument ([`NetParts`], lane
//! decision 2): there is no constructor without them, so no code path of this crate reaches a real
//! server unless the one module in `REAL_CLIENT_MODULES` made the parts.

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{
    AccountContext, AccountHandle, AllocatedId, Network, spawn_account_thread,
};
use kourier_model::{
    BackendError, ConvKey, EditTarget, HistoryReadState, HistorySource, LoginFlow, MsgIdScope,
    NetworkDescriptor, NetworkKind, OutboxPolicy, RiskNotice, ServerRules, TypingPolicy, UserId,
};
use wacore::request::RequestUtils;
use wacore_binary::Jid;

use crate::NetParts;
use crate::actor;

/// How long the core gives a WhatsApp actor to stop on its own at a quit (`NetworkDescriptor::
/// shutdown_grace`; R-Q1, the lead's answer in `w4-decisions.md`). The actor's `finish` waits up to
/// 15 s for a history chunk the phone announced and has not finished sending (the phone sends it
/// once), and only while one is outstanding, and then still stops its client and releases the
/// store. An ordinary quit, with nothing outstanding, is as fast as before.
pub(crate) const SHUTDOWN_GRACE: Duration = Duration::from_secs(20);

/// How long the core tries a WhatsApp message, its longest wait between two attempts, and how long a challenge
/// lives (W5a, decision D11): 24 h, 15 min, 24 h. A message the network cannot take within a day is failed for the
/// person to see (Send Again is one act away); the time the account was offline or held by the reach-out lock does
/// not count against the budget (the core counts only the time a send could go). WhatsApp has no challenge, so the
/// last number is never used; edits are bounded by the backend's slack rule (`send::attempt`), not by the budget.
/// The same numbers as Signal Desktop's, kept as a constant of their own so that a change to one network's is not a
/// change to the other's.
pub(crate) const OUTBOX: OutboxPolicy = OutboxPolicy {
    budget: Duration::from_secs(24 * 3600),
    max_backoff: Duration::from_secs(15 * 60),
    challenge_lifetime: Duration::from_secs(24 * 3600),
};

/// The WhatsApp network.
pub struct WhatsAppNetwork {
    descriptor: NetworkDescriptor,
    /// The maker of the transport and the HTTP client of a client. NOT called here, nor by `spawn`:
    /// the actor calls it on its own thread when its account first connects or starts a login.
    parts: NetParts,
}

impl WhatsAppNetwork {
    /// The WhatsApp backend over `parts` (the maker is not called: nothing of upstream is touched
    /// until an actor connects). Decisions that the descriptor carries:
    ///
    /// - `history` is `PushedOnLink` (architecture §7.1): the phone pushes the history in batches
    ///   after the link. Since K-A that value alone no longer offers Signal's "Transfer History
    ///   from Phone…" (a link and unlink cycle, a risk for the account): the screens key it on
    ///   `history_transfer`, which is `false` here, as its doc says for a network that pushes.
    /// - `experimental` is `true`: `kourier-app` registers the network only while its switch is on,
    ///   and the lists show it as "Off · experimental" until then (architecture §7.4).
    /// - `start_chat` is empty: Kourier never starts a chat with a new number on WhatsApp (his
    ///   decision D6); chats exist when the phone has them.
    /// - `risk_notice` is set (W2b): K-A's warning page asks first only for a descriptor that HAS a
    ///   notice (`NetworkChoice.asks_first` is `risk_notice.is_some()`). The UI writes its own words
    ///   (`kourier-viewmodel`, translated); these are the fallback, the same the fake's WhatsApp
    ///   flavour has, and a network's own words never replace the UI's.
    /// - Two login flows: `qr` and `pair-code`. The passkey confirmation is a step of a link, not a
    ///   flow (W9).
    /// - `display_name` names the network in the list of networks, never the application.
    pub fn new(parts: NetParts) -> Self {
        Self {
            parts,
            descriptor: NetworkDescriptor {
                display_name: "WhatsApp".to_owned(),
                // The key of the bundled network badge (`kourier-qt/icons/whatsapp.svg`), as the
                // viewmodel's `network_icon` names it. A badge, never the application's icon.
                icon_name: "whatsapp".to_owned(),
                login_flows: vec![
                    LoginFlow {
                        id: "qr".to_owned(),
                        label: "Link with a QR code".to_owned(),
                    },
                    LoginFlow {
                        id: "pair-code".to_owned(),
                        label: "Link with a phone number".to_owned(),
                    },
                ],
                start_chat: Vec::new(),
                history: HistorySource::PushedOnLink,
                // WhatsApp's backend states `read: true` for every history row (it cannot tell what the
                // person has seen) and gives the unread count as a hint: a copy never raises a row
                // (W4 fix F1).
                history_read: HistoryReadState::Unknown,
                history_transfer: false,
                experimental: true,
                // Stanza ids are unique per sender in a chat, not per chat (K-B1).
                msg_id_scope: MsgIdScope::ConversationAuthor,
                // As the fake network's WhatsApp-like flavour. WhatsApp's own clients repeat "composing" while
                // the person types and send "paused" when they stop; the backend sends each state the core asks
                // for as one chat state (`send::receipts::typing`), so these two numbers are the core's pacing.
                typing: TypingPolicy {
                    refresh: Duration::from_secs(10),
                    stop_after: Duration::from_secs(3),
                },
                risk_notice: Some(RiskNotice {
                    title: "Before You Link WhatsApp".to_owned(),
                    body: "WhatsApp does not allow unofficial apps and may restrict or ban an account linked to this one.".to_owned(),
                    url: None,
                }),
                // An edit is a protocol message whose key is the ORIGINAL message, whatever was
                // edited since (K-B1; whatsmeow `BuildEdit`, whatsapp-rust `edit_message`).
                edit_target: EditTarget::Original,
                outbox: OUTBOX,
                shutdown_grace: Some(SHUTDOWN_GRACE),
                safety_numbers: false,
                // WhatsApp's vCard send is a later item (P5c, SC4).
                contact_cards: false,
                // Every picture the media send reads whole leaves through `kourier_media::clean_still`
                // (`send::upload::still`; LOC F14, W5b G7b): its types up to the picture bound.
                cleaned_pictures: Some(crate::send::upload::stated_cleaning()),
                // WhatsApp's receipts and typing follow the account's own settings.
                receipts_always_sent: false,
                // The clients count what is unread and check the deletes (KT-2).
                server: ServerRules::default(),
            },
        }
    }
}

impl WhatsAppNetwork {
    /// What the phone lists this device as, and what the logout report names to the person:
    /// `Kourier (<host>)` from the host name the application gave to [`NetParts::with_host_name`],
    /// plain `Kourier` without one.
    pub fn device_label(&self) -> String {
        crate::props::device_name(self.parts.host_name())
    }
}

impl std::fmt::Debug for WhatsAppNetwork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WhatsAppNetwork")
    }
}

/// The phone-number JID of the account, from its own ids (`pn:<digits>`), when it has one.
fn own_pn(own: &[UserId]) -> Option<Jid> {
    own.iter()
        .find(|id| id.scheme() == "pn")
        .and_then(|id| format!("{}@s.whatsapp.net", id.value()).parse().ok())
}

/// WhatsApp's link to a person by an E.164 number (`https://wa.me/<digits>`, no plus sign);
/// `None` for anything else.
fn contact_link(number: &str) -> Option<String> {
    kourier_backend_api::is_e164(number).then(|| format!("https://wa.me/{}", &number[1..]))
}

impl Network for WhatsAppNetwork {
    fn kind(&self) -> NetworkKind {
        NetworkKind::WhatsApp
    }

    fn descriptor(&self) -> &NetworkDescriptor {
        &self.descriptor
    }

    fn contact_links(&self) -> bool {
        true
    }

    fn group_invites(&self) -> kourier_model::GroupInvites {
        kourier_model::GroupInvites::Reset
    }

    fn contact_link(&self, _user: &UserId, number: &str) -> Option<String> {
        contact_link(number)
    }

    /// whatsapp-rust's own derivation (`RequestUtils::message_id_at`): `3EB0` and 18 upper-case hex
    /// digits, a hash of the second, the account's phone number and 16 random bytes. It is random,
    /// not deterministic, which is allowed: the core persists the result before the first transmit,
    /// so a retry reuses it, and the function itself has no side effect and does no I/O. Before the
    /// link the account has no `pn:` id and the digest is made without it (W5 decides what a send
    /// before the link means). `sent_at` is the time the core gave.
    fn allocate_message_id(&self, own: &[UserId], _conv: &ConvKey, now_ms: u64) -> AllocatedId {
        let pn = own_pn(own);
        AllocatedId {
            local: Arc::from(RequestUtils::message_id_at(pn.as_ref(), now_ms / 1000)),
            sent_at: i64::try_from(now_ms).unwrap_or(i64::MAX),
        }
    }

    fn spawn(&self, ctx: AccountContext) -> Result<AccountHandle, BackendError> {
        let name = format!("whatsapp-{}", ctx.uuid.short());
        let parts = self.parts.clone();
        spawn_account_thread(name, move || actor::run(ctx, parts))
    }
}

#[cfg(test)]
mod link_tests {
    use super::contact_link;

    #[test]
    fn a_person_link_is_the_number_without_its_plus_sign() {
        assert_eq!(
            contact_link("+15551234567").as_deref(),
            Some("https://wa.me/15551234567")
        );
    }

    #[test]
    fn only_an_e164_number_makes_a_link() {
        for not in ["15551234567", "+0551234567", "+1555", "", "+1555abc4567"] {
            assert_eq!(contact_link(not), None, "{not}");
        }
    }
}
