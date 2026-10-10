//! Network shapes the fake imitates (descriptor and id allocation).

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::AllocatedId;
use kourier_model::{
    HistorySource, IdentifierKind, LoginFlow, MsgIdScope, NetworkDescriptor, TypingPolicy,
};

/// Which network the fake imitates. All flavors report `NetworkKind::Fake`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Flavor {
    /// Signal: account-wide `(author, timestamp)` ids, no history.
    SignalLike,
    /// WhatsApp: per-conversation stanza ids, history pushed after linking.
    WhatsAppLike,
    /// Threema: per-conversation u64 ids, references without authors.
    ThreemaLike,
}

impl Flavor {
    /// The static descriptor (§3.8 tier 1).
    pub fn descriptor(self) -> NetworkDescriptor {
        let (name, scope, history, start_chat) = match self {
            Self::SignalLike => (
                "Fake (Signal-like)",
                MsgIdScope::AccountAuthor,
                HistorySource::None,
                vec![IdentifierKind::Phone, IdentifierKind::Username],
            ),
            Self::WhatsAppLike => (
                "Fake (WhatsApp-like)",
                MsgIdScope::Conversation,
                HistorySource::PushedOnLink,
                vec![IdentifierKind::Phone],
            ),
            Self::ThreemaLike => (
                "Fake (Threema-like)",
                MsgIdScope::Conversation,
                HistorySource::None,
                vec![IdentifierKind::NetworkId],
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
            msg_id_scope: scope,
            typing: TypingPolicy {
                refresh: Duration::from_secs(10),
                stop_after: Duration::from_secs(3),
            },
            risk_notice: None,
        }
    }

    /// The id of a new own message: Signal-like uses the timestamp, the others a hex counter.
    pub fn allocate(self, now_ms: u64) -> AllocatedId {
        let local: Arc<str> = match self {
            Self::SignalLike => now_ms.to_string().into(),
            Self::WhatsAppLike => format!("3EB0{now_ms:016X}").into(),
            Self::ThreemaLike => format!("{now_ms:016x}").into(),
        };
        AllocatedId {
            local,
            sent_at: i64::try_from(now_ms).unwrap_or(i64::MAX),
        }
    }
}
