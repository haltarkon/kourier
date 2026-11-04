//! Network shapes the fake imitates (descriptor and id allocation).

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::AllocatedId;
use kourier_model::{
    CleanedPictures, EditTarget, HistoryReadState, HistorySource, IdentifierKind, LoginFlow,
    MsgIdScope, NetworkDescriptor, NetworkKind, OutboxPolicy, RiskNotice, ServerRules,
    TypingPolicy,
};

/// The retry budget of [`Flavor::WhatsAppLike`] (K-B1 B6): six hours, a test value.
pub const WHATSAPP_LIKE_BUDGET: Duration = Duration::from_secs(6 * 3600);

/// Which network the fake imitates. All flavors report `NetworkKind::Fake`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Flavor {
    /// Signal: account-wide `(author, timestamp)` ids, no history.
    SignalLike,
    /// WhatsApp: stanza ids unique per sender in a chat (K-B1), history pushed after linking.
    WhatsAppLike,
    /// Threema: per-conversation u64 ids, references without authors.
    ThreemaLike,
    /// Telegram (KT-2): the server names the messages, numbers them in their chat, counts what is
    /// unread and checks the deletes. The core allocates `k<hex>` ids that the server replaces
    /// (`TransmitOutcome::SentAs`, one echo under the server's id), history is asked for.
    TelegramLike,
}

impl Flavor {
    /// The static descriptor (§3.8 tier 1).
    pub fn descriptor(self) -> NetworkDescriptor {
        let (name, scope, history, start_chat, edit_target) = match self {
            Self::SignalLike => (
                "Fake (Signal-like)",
                MsgIdScope::AccountAuthor,
                HistorySource::None,
                vec![IdentifierKind::Phone, IdentifierKind::Username],
                EditTarget::LatestRevision,
            ),
            Self::WhatsAppLike => (
                "Fake (WhatsApp-like)",
                MsgIdScope::ConversationAuthor,
                HistorySource::PushedOnLink,
                vec![IdentifierKind::Phone],
                EditTarget::Original,
            ),
            Self::ThreemaLike => (
                "Fake (Threema-like)",
                MsgIdScope::Conversation,
                HistorySource::None,
                vec![IdentifierKind::NetworkId],
                EditTarget::Original,
            ),
            Self::TelegramLike => (
                "Fake (Telegram-like)",
                MsgIdScope::Conversation,
                HistorySource::OnDemand,
                // Kourier starts no chat on Telegram: a chat exists because Telegram has it.
                Vec::new(),
                EditTarget::Original,
            ),
        };
        NetworkDescriptor {
            display_name: name.into(),
            icon_name: "network-wireless".into(),
            login_flows: vec![LoginFlow {
                id: "qr".into(),
                label: "Link with a QR code".into(),
            }],
            start_chat,
            history,
            // The fake plays the contract with known read states for every flavour; a test that
            // needs WhatsApp's unknown ones says so on the descriptor it hands the core.
            history_read: HistoryReadState::Known,
            msg_id_scope: scope,
            typing: TypingPolicy {
                refresh: Duration::from_secs(10),
                stop_after: Duration::from_secs(3),
            },
            risk_notice: None,
            history_transfer: false,
            experimental: false,
            edit_target,
            outbox: self.outbox_policy(),
            // The core's default for every flavor; a test that needs a longer stop states it
            // on the descriptor it hands the core.
            shutdown_grace: None,
            // Only the Signal-like fake answers `Command::SafetyNumber` (a script decides what).
            safety_numbers: matches!(self, Self::SignalLike),
            // Only the Signal-like fake sends contact cards (P5c, SC4); it transmits any body.
            contact_cards: matches!(self, Self::SignalLike),
            // The Signal-like fake states Signal's cleaned pictures (LOC), so the core's verdict
            // runs as on Signal. It sends nothing off the PC: the one named exception of the
            // xtask source check (`cleaned_pictures` only beside `clean_still`).
            cleaned_pictures: matches!(self, Self::SignalLike)
                .then(|| CleanedPictures::stills(kourier_media::CLEANED_TYPES)),
            // No flavour of the plain fake always sends receipts; only the fake that plays
            // Telegram says so (`playing_telegram`).
            receipts_always_sent: false,
            // The server owns nothing in the plain fake but Telegram-like's: the clients count what
            // is unread and check the deletes (KT-2).
            server: match self {
                Self::TelegramLike => ServerRules {
                    owns_unread: true,
                    validates_deletes: true,
                },
                Self::SignalLike | Self::WhatsAppLike | Self::ThreemaLike => ServerRules::default(),
            },
        }
    }

    /// The outbox numbers of this flavor: Signal's, except that the WhatsApp-like fake gives up
    /// a send after [`WHATSAPP_LIKE_BUDGET`], a test value (not WhatsApp's: W5 decides those
    /// with its windows) so that a core test can tell a per-network budget from the core's
    /// constant.
    pub fn outbox_policy(self) -> OutboxPolicy {
        match self {
            Self::WhatsAppLike => OutboxPolicy {
                budget: WHATSAPP_LIKE_BUDGET,
                ..OutboxPolicy::SIGNAL
            },
            Self::SignalLike | Self::ThreemaLike | Self::TelegramLike => OutboxPolicy::SIGNAL,
        }
    }

    /// The descriptor of this flavor as the network `kind` shows it
    /// ([`crate::FakeNetwork::playing`]).
    ///
    /// Everything not stated here is the flavor's own, so the fixture's WhatsApp (played on
    /// [`Flavor::SignalLike`]) keeps Signal's id scope, edit target and outbox numbers on
    /// purpose: its pages show the same ids and chats as before K-B1, and the picture digest
    /// must not move. Do not give it the WhatsApp-like flavor's. The fixture's Threema is played
    /// the same way (TH4 KH1c).
    ///
    /// # Panics
    /// For a kind the fake cannot play (WhatsApp, Threema and Telegram).
    pub fn playing_descriptor(self, kind: NetworkKind) -> NetworkDescriptor {
        match kind {
            NetworkKind::WhatsApp => self.playing_whatsapp(),
            NetworkKind::Threema => self.playing_threema(),
            NetworkKind::Telegram => self.playing_telegram(),
            other => panic!("the fake plays WhatsApp, Threema and Telegram only, not {other}"),
        }
    }

    /// WhatsApp as the UI sees it ([`Self::playing_descriptor`]).
    fn playing_whatsapp(self) -> NetworkDescriptor {
        NetworkDescriptor {
            display_name: "WhatsApp".into(),
            icon_name: "network-wireless".into(),
            login_flows: vec![
                LoginFlow {
                    id: "qr".into(),
                    label: "Link with a QR code".into(),
                },
                LoginFlow {
                    id: "pair-code".into(),
                    label: "Link with a phone number".into(),
                },
            ],
            // Kourier never starts a chat on WhatsApp (decision D6): a chat exists only
            // because the phone has it.
            start_chat: Vec::new(),
            history: HistorySource::PushedOnLink,
            risk_notice: Some(RiskNotice {
                title: "Before You Link WhatsApp".into(),
                body: "WhatsApp does not allow unofficial apps and may restrict or ban an account linked to this one.".into(),
                url: None,
            }),
            history_transfer: false,
            experimental: true,
            // WhatsApp has no safety numbers in Kourier (its security code is a later item).
            safety_numbers: false,
            // Nor contact cards (P5c, SC4).
            contact_cards: false,
            // WhatsApp's statement (W5b G7b): the rehearsal's and the fixture's composer ask what WhatsApp's does.
            cleaned_pictures: Some(crate::scripts::whatsapp_cleaned_pictures()),
            // The real backend's rules (KT-2), whatever the flavour that plays it.
            server: ServerRules::default(),
            ..self.descriptor()
        }
    }

    /// Threema as the UI sees it ([`Self::playing_descriptor`]): the fields the real backend
    /// states in `kourier-threema`'s `ThreemaNetwork::new` that a UI reads, copied (the risk
    /// notice's words too). `kourier-threema`'s `tests/network.rs` compares the two, so a
    /// change on either side fails there (TH4 decision L7). Not copied, on purpose: the id
    /// scope, the edit target and the outbox numbers, which stay the flavor's, so the same
    /// script gives the same chats and ids as the plain fake.
    fn playing_threema(self) -> NetworkDescriptor {
        NetworkDescriptor {
            display_name: "Threema".into(),
            // The key of the bundled network badge (`kourier-qt/icons/threema.svg`).
            icon_name: "threema".into(),
            // The emoji comparison is a STEP of this flow, not a flow of its own.
            login_flows: vec![LoginFlow {
                id: "qr".into(),
                label: "Link with a QR code".into(),
            }],
            // A chat from a Threema ID comes later.
            start_chat: Vec::new(),
            // Threema pushes no history to a new device.
            history: HistorySource::None,
            risk_notice: Some(RiskNotice {
                title: "Before You Turn On Threema".into(),
                body: "Kourier is not an official Threema app. It links as another device of your Threema account, and Threema may end that link.".into(),
                url: None,
            }),
            history_transfer: false,
            experimental: true,
            // No safety numbers (P5a), no contact cards (P5c) and no files yet (LOC).
            safety_numbers: false,
            contact_cards: false,
            cleaned_pictures: None,
            // The real backend's rules (KT-2), whatever the flavour that plays it.
            server: ServerRules::default(),
            ..self.descriptor()
        }
    }

    /// Telegram as the UI sees it ([`Self::playing_descriptor`], KT-1b): the fields the real
    /// backend states in `kourier-telegram`'s `descriptor()` that a UI reads, copied (the risk
    /// notice's words too; no type of `kourier-telegram` is used here, the fake holds no Telegram
    /// code). `kourier-telegram`'s own tests compare the two, so a change on either side fails
    /// there. Not copied, on purpose: the id scope, the edit target and the outbox numbers, which
    /// stay the flavor's (the same script gives the same chats and ids as the plain fake), and the
    /// shutdown grace: the real backend states 20 s for its helper's close, the fake stops at once
    /// and keeps the core's default, or every fixture page and window test that plays Telegram would
    /// allow a 20 s quit for nothing (the core's limit follows the largest registered grace).
    fn playing_telegram(self) -> NetworkDescriptor {
        NetworkDescriptor {
            display_name: "Telegram".into(),
            // The key of the bundled network badge (`kourier-qt/icons/telegram.svg`).
            icon_name: "telegram".into(),
            // The QR code first, then the phone number (the real order).
            login_flows: vec![
                LoginFlow {
                    id: "qr".into(),
                    label: "Scan a QR Code".into(),
                },
                LoginFlow {
                    id: "phone".into(),
                    label: "Phone Number".into(),
                },
            ],
            // Kourier starts no chat on Telegram: a chat exists because Telegram has it.
            start_chat: Vec::new(),
            // Telegram's servers keep the history and serve it when it is asked for.
            history: HistorySource::OnDemand,
            risk_notice: Some(RiskNotice {
                title: "Before You Add Telegram".into(),
                body: "Kourier is not made by Telegram. It uses the Telegram API as an independent app with its own API key.\n\nTelegram watches accounts used with new apps more closely; mass actions or unusual use can get an account limited or banned.\n\nKourier always sends read receipts, typing and online status for Telegram, as Telegram's rules require, and shows the sponsored messages Telegram puts in channels.".into(),
                url: None,
            }),
            history_transfer: false,
            experimental: true,
            // No safety numbers on Telegram's cloud chats, no contact cards and no files yet.
            safety_numbers: false,
            contact_cards: false,
            cleaned_pictures: None,
            // Telegram's API rules: its accounts always send receipts, typing and online status.
            receipts_always_sent: true,
            // The real Telegram descriptor keeps both flags off until its lane maps onto them
            // (KT-2), whatever the flavour that plays it.
            server: ServerRules::default(),
            ..self.descriptor()
        }
    }

    /// The id of a new own message: Signal-like uses the timestamp, the others a hex counter. The
    /// WhatsApp-like id has WhatsApp's shape and length: `3EB0` and 18 upper-case hex digits, 22
    /// characters (upstream's `RequestUtils::message_id_at`, which the WhatsApp backend calls). The
    /// Telegram-like id is `k` and 16 hex digits: never a number, because the id the server gives
    /// that flavour's message IS one (`sent_at` in decimal), and a test that confuses the two fails.
    pub fn allocate(self, now_ms: u64) -> AllocatedId {
        let local: Arc<str> = match self {
            Self::SignalLike => now_ms.to_string().into(),
            Self::WhatsAppLike => format!("3EB0{now_ms:018X}").into(),
            Self::ThreemaLike => format!("{now_ms:016x}").into(),
            Self::TelegramLike => format!("k{now_ms:016x}").into(),
        };
        AllocatedId {
            local,
            sent_at: i64::try_from(now_ms).unwrap_or(i64::MAX),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FakeNetwork, Script};
    use kourier_backend_api::Network;

    /// KT-2 (E): the Telegram-like flavour says what Telegram's server does: ids per conversation,
    /// history on demand, edits name the original, the server counts what is unread and checks the
    /// deletes; it starts no chat and is a fake like the others. No other flavour owns anything.
    #[test]
    fn the_telegram_like_descriptor_says_what_the_flavour_says() {
        let d = Flavor::TelegramLike.descriptor();
        assert_eq!(d.msg_id_scope, MsgIdScope::Conversation);
        assert_eq!(d.history, HistorySource::OnDemand);
        assert_eq!(d.edit_target, EditTarget::Original);
        assert_eq!(d.outbox, OutboxPolicy::SIGNAL);
        assert!(d.start_chat.is_empty(), "it starts no chat");
        assert!(!d.receipts_always_sent);
        assert_eq!(
            d.server,
            ServerRules {
                owns_unread: true,
                validates_deletes: true
            }
        );
        // (the rest is what every flavour of the plain fake states)
        assert!(!d.safety_numbers && !d.contact_cards && d.cleaned_pictures.is_none());
        for other in [
            Flavor::SignalLike,
            Flavor::WhatsAppLike,
            Flavor::ThreemaLike,
        ] {
            assert_eq!(
                other.descriptor().server,
                ServerRules::default(),
                "{other:?}"
            );
        }
        let net = FakeNetwork::new(Flavor::TelegramLike, Script::new("flavour", Vec::new()));
        assert_eq!(net.kind(), NetworkKind::Fake, "like every flavour");
        assert_eq!(net.descriptor().server, d.server);
    }

    /// KT-2 (E): the fake that PLAYS a network states the real backend's server rules (it owns
    /// nothing), whatever the flavour it plays on, so the rehearsals and the fixture pages do not
    /// move. The test can fail (R2-L1): the flavour that DOES own something, `TelegramLike`, plays
    /// each network and must hand over the default, so a `playing_*` that kept the flavour's `server`
    /// shows; and the fake the core is given (`FakeNetwork::playing`) says the default too.
    /// Mutant: `playing_whatsapp`, `playing_threema` and `playing_telegram` take the flavour's
    /// `server` (their `server: ServerRules::default(),` lines removed).
    #[test]
    fn the_fake_that_plays_telegram_owns_nothing() {
        assert_eq!(
            Flavor::TelegramLike.descriptor().server,
            ServerRules {
                owns_unread: true,
                validates_deletes: true
            },
            "the premise: this flavour owns what the server of its network owns"
        );
        for kind in [
            NetworkKind::WhatsApp,
            NetworkKind::Threema,
            NetworkKind::Telegram,
        ] {
            for flavor in [Flavor::TelegramLike, Flavor::SignalLike] {
                assert_eq!(
                    flavor.playing_descriptor(kind).server,
                    ServerRules::default(),
                    "{kind} on {flavor:?}"
                );
            }
            let net = FakeNetwork::playing(kind, Script::new("plays", Vec::new()));
            assert_eq!(net.descriptor().server, ServerRules::default(), "{kind}");
        }
    }

    /// KT-2 (E): the allocated id of the Telegram-like flavour is `k` and 16 hex digits, never a
    /// number (the id the server gives is one), and rises with the clock like the others' do.
    /// Mutant: `allocate` answers the timestamp in decimal.
    #[test]
    fn a_telegram_like_id_is_never_a_number_and_rises() {
        let mut last = None;
        for now_ms in [1u64, 9, 10, 255, 256, 1_760_000_000_000, 1_760_000_000_001] {
            let id = Flavor::TelegramLike.allocate(now_ms);
            let local: &str = &id.local;
            assert!(local.parse::<i64>().is_err(), "{local} reads as a number");
            assert_eq!(local.len(), 17, "{local}");
            assert!(local.starts_with('k'), "{local}");
            assert!(
                local[1..]
                    .chars()
                    .all(|c| matches!(c, '0'..='9' | 'a'..='f')),
                "{local}"
            );
            assert_eq!(id.sent_at, i64::try_from(now_ms).expect("fits"));
            assert!(last.is_none_or(|l: Arc<str>| l < id.local), "{local}");
            last = Some(id.local);
        }
        // The other flavours' ids are untouched: Signal's is the timestamp.
        assert_eq!(&*Flavor::SignalLike.allocate(77).local, "77");
        assert_eq!(
            &*Flavor::ThreemaLike.allocate(255).local,
            "00000000000000ff"
        );
    }
}
