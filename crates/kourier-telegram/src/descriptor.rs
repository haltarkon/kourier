//! What the Telegram network says of itself (TG-1 plan §5.2; the critic's TSC M2 and the lead's D2).
//!
//! - `experimental` is `true`: `kourier-app` registers the network only in a debug build while
//!   `networks-on` names it, and Telegram stays ABSENT from Add Account until TG-2 can log in (D2).
//! - Two login flows, the QR code FIRST (TSC M2), then the phone number.
//! - `start_chat` is empty: Kourier never starts a chat with a new number or name on Telegram until
//!   he decides otherwise (TS TG10 default); chats exist when Telegram has them.
//! - `history` is `OnDemand`: Telegram serves older messages when they are asked for;
//!   `history_transfer` is `false` (that is Signal's archive transfer).
//! - Message ids are unique per conversation (`MsgIdScope::Conversation`): TDLib numbers messages
//!   per chat.
//! - `edit_target` is `Original`: an edit names the message, not its last revision.
//! - The risk notice is the English fallback; the UI writes its own translated words (KT-1b).
//! - `receipts_always_sent` is `true` (KT-1b): Telegram's rules; the fake that plays Telegram
//!   (`kourier-fake`, `Flavor::playing_descriptor`) copies every field but the id scope and the
//!   edit target and the shutdown grace, and `tests/network.rs` compares the two.

use std::time::Duration;

use kourier_model::{
    EditTarget, HistoryReadState, HistorySource, LoginFlow, MsgIdScope, NetworkDescriptor,
    OutboxPolicy, RiskNotice, TypingPolicy,
};

/// How long the core gives a Telegram actor to stop at a quit: the helper's close deadline (15 s)
/// plus the kill and the reap (decisions D3 M3), as WhatsApp's.
pub(crate) const SHUTDOWN_GRACE: Duration = Duration::from_secs(20);

/// The warning page's title (English fallback).
pub(crate) const RISK_TITLE: &str = "Before You Add Telegram";

/// The warning page's body (English fallback).
pub(crate) const RISK_BODY: &str = "Kourier is not made by Telegram. It uses the Telegram API as an independent app with its own API key.\n\nTelegram watches accounts used with new apps more closely; mass actions or unusual use can get an account limited or banned.\n\nKourier always sends read receipts, typing and online status for Telegram, as Telegram's rules require, and shows the sponsored messages Telegram puts in channels.";

/// The Telegram descriptor.
pub(crate) fn descriptor() -> NetworkDescriptor {
    NetworkDescriptor {
        display_name: "Telegram".to_owned(),
        // The key of the bundled network badge (`kourier-qt/icons/telegram.svg`): a neutral
        // speech bubble, never Telegram's logo.
        icon_name: "telegram".to_owned(),
        login_flows: vec![
            LoginFlow {
                id: "qr".to_owned(),
                label: "Scan a QR Code".to_owned(),
            },
            LoginFlow {
                id: "phone".to_owned(),
                label: "Phone Number".to_owned(),
            },
        ],
        start_chat: Vec::new(),
        history: HistorySource::OnDemand,
        history_read: HistoryReadState::Known,
        msg_id_scope: MsgIdScope::Conversation,
        // As Signal's until TG-2 measures Telegram's own chat actions.
        typing: TypingPolicy {
            refresh: Duration::from_secs(10),
            stop_after: Duration::from_secs(3),
        },
        risk_notice: Some(RiskNotice {
            title: RISK_TITLE.to_owned(),
            body: RISK_BODY.to_owned(),
            url: None,
        }),
        history_transfer: false,
        experimental: true,
        edit_target: EditTarget::Original,
        outbox: OutboxPolicy::SIGNAL,
        shutdown_grace: Some(SHUTDOWN_GRACE),
        // No safety numbers on Telegram's cloud chats (P5a); sending a contact card is a later
        // item (P5c).
        safety_numbers: false,
        contact_cards: false,
        // Telegram sends no files yet; a network that cleans on its server does not qualify
        // (LOC, F14).
        cleaned_pictures: None,
        // Telegram's API rules (HA TG6): its accounts always send read receipts, typing and
        // online status, and no setting turns them off; a UI says so in its privacy settings.
        receipts_always_sent: true,
        server: kourier_model::ServerRules::default(),
    }
}
