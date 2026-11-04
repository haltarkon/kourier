//! `TelegramNetwork`: the descriptor, the id of an own message and the account actor's factory.
//!
//! It takes the helper parts as a REQUIRED argument ([`TgParts`]): there is no constructor without
//! them, so no code path of this crate starts a helper or dials anything unless `kourier-app` made
//! the parts with the `helper_parts` of this crate's `real` module. `spawn` starts the account
//! thread and NOTHING else: no helper, no socket (the actor starts a helper only when an account
//! needs TDLib, TG-1b).

use std::sync::Arc;

use kourier_backend_api::{
    AccountContext, AccountHandle, AllocatedId, Network, spawn_account_thread,
};
use kourier_model::{BackendError, ConvKey, NetworkDescriptor, NetworkKind, UserId};

use crate::actor;
use crate::descriptor::descriptor;
use crate::parts::TgParts;

/// The Telegram network.
pub struct TelegramNetwork {
    descriptor: NetworkDescriptor,
    /// What the account threads run with. NOT called here, nor by `spawn`.
    parts: TgParts,
}

impl TelegramNetwork {
    /// The Telegram backend over `parts` (nothing of them is called).
    pub fn new(parts: TgParts) -> Self {
        Self {
            descriptor: descriptor(),
            parts,
        }
    }
}

impl std::fmt::Debug for TelegramNetwork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TelegramNetwork")
    }
}

/// `k` and 16 random lower-case hex digits: TDLib's message ids are decimal numbers, so an id
/// Kourier allocated can never be taken for one of TDLib's.
pub(crate) fn own_message_id() -> String {
    format!("k{:016x}", rand::random::<u64>())
}

impl Network for TelegramNetwork {
    fn kind(&self) -> NetworkKind {
        NetworkKind::Telegram
    }

    fn descriptor(&self) -> &NetworkDescriptor {
        &self.descriptor
    }

    /// Random, not deterministic, which is allowed: the core persists the id before the first
    /// transmit, so a retry reuses it; no side effect, no I/O.
    fn allocate_message_id(&self, _own: &[UserId], _conv: &ConvKey, now_ms: u64) -> AllocatedId {
        AllocatedId {
            local: Arc::from(own_message_id()),
            sent_at: i64::try_from(now_ms).unwrap_or(i64::MAX),
        }
    }

    fn spawn(&self, ctx: AccountContext) -> Result<AccountHandle, BackendError> {
        let name = format!("telegram-{}", ctx.uuid.short());
        let parts = self.parts.clone();
        spawn_account_thread(name, move || actor::run(ctx, parts))
    }
}
