//! The durability hook (W3a): upstream's `InboundDurabilityHook` over the [`Inbox`].
//!
//! Upstream calls [`Hook::on_messages`] for every decrypted end-to-end message BEFORE it tells the
//! server anything: live messages as a batch of one, the offline drain as batches of up to 400 /
//! 4 MiB / 3 s. `Ok` makes upstream acknowledge the whole batch; `Err` suppresses every ack and the
//! server sends the batch again. So the hook does one thing: it commits the batch ([`Inbox::commit`],
//! one zone) and returns only after COMMIT. It maps nothing (a mapping bug in the hook would return
//! `Err` for ever, the server would redeliver for ever and upstream would keep the plaintext resident:
//! stage 2 maps, and a change of the map never touches the durable part).
//!
//! A failed commit ends the connection (decision D5). Upstream's own reaction to an `Err` is to suppress
//! the acks of that batch and go on with the connection, which would leave the redelivery to a later
//! reconnect and let the next batch fail the same way; the hook therefore calls
//! `Client::signal_shutdown_sync` before it returns, which is terminal for the client (the actor makes a
//! new one after a backoff, a later chunk) and suppresses every later ack of the connection. The
//! messages stay unacked on the server and come again on the next connection, where the hook runs again
//! from upstream's own pending rows.
//!
//! The pieces that need a client are only `on_messages`' last line; [`Hook::commit`] is everything else
//! and is what the hook-level tests call, with hand-built batches and no client.

use std::sync::Arc;

use async_trait::async_trait;
use wacore::types::events::InboundMessage;
use whatsapp_rust::Client;
use whatsapp_rust::types::durability_hook::InboundDurabilityHook;

use crate::inbox::{Committed, Inbox, Origin};
use crate::stage2::Stage2;
use crate::tap::Tap;
use crate::translate::{self, WaSignal};

/// What the hook tells the rest of the crate, in the order things happen. Called on whatever task
/// upstream's pipeline runs the hook on; they must not block.
#[async_trait]
pub(crate) trait Notes: Send + Sync + 'static {
    /// A batch is committed (stage 2 may have rows to take).
    fn committed(&self, done: &Committed);
    /// A batch could not be committed and the connection is being ended.
    fn failed(&self);
    /// Resolves when stage 2 has room for more rows (decision D8): the hook waits here BEFORE it
    /// commits, so upstream's pipeline holds the acks while the backlog is full. Bounded by the
    /// implementation; the default never waits.
    async fn room(&self) {}
}

/// The notes of a real client's hook: a commit wakes stage 2 (and stage 2 holds the hook while its
/// backlog is full), and a failed commit is told to the actor through the client's tap, which makes
/// a new client after a backoff (decision D5).
pub(crate) struct ClientNotes {
    stage2: Arc<Stage2>,
    tap: Tap,
}

impl ClientNotes {
    pub(crate) fn new(stage2: Arc<Stage2>, tap: Tap) -> Self {
        Self { stage2, tap }
    }
}

#[async_trait]
impl Notes for ClientNotes {
    fn committed(&self, done: &Committed) {
        Notes::committed(&*self.stage2, done);
    }

    fn failed(&self) {
        self.tap.deliver(WaSignal::WriteFailed);
    }

    async fn room(&self) {
        Notes::room(&*self.stage2).await;
    }
}

/// Notes that note nothing: for a test that needs a hook and watches none of this.
#[cfg(test)]
#[async_trait]
impl Notes for () {
    fn committed(&self, _done: &Committed) {}

    fn failed(&self) {}
}

/// The hook of one client.
pub(crate) struct Hook {
    inbox: Inbox,
    notes: Arc<dyn Notes>,
}

impl Hook {
    pub(crate) fn new(inbox: Inbox, notes: Arc<dyn Notes>) -> Self {
        Self { inbox, notes }
    }

    /// Commits `batch` to the inbox and returns after COMMIT; `Err` (nothing was written) after
    /// telling the notes. Everything the hook does except ending the client.
    pub(crate) async fn commit(&self, batch: &[InboundMessage]) -> anyhow::Result<Committed> {
        self.notes.room().await;
        let arrivals = batch
            .iter()
            .map(|item| translate::arrival(item, Origin::E2e))
            .collect();
        match self.inbox.commit(arrivals, Vec::new()).await {
            Ok(done) => {
                self.notes.committed(&done);
                Ok(done)
            }
            Err(e) => {
                // The error names the operation, never an input.
                tracing::error!(error = %e, messages = batch.len(), "the inbox commit failed");
                self.notes.failed();
                Err(anyhow::anyhow!("the inbox commit failed"))
            }
        }
    }
}

#[async_trait]
impl InboundDurabilityHook for Hook {
    async fn on_messages(
        &self,
        client: Arc<Client>,
        batch: &[InboundMessage],
    ) -> anyhow::Result<()> {
        match self.commit(batch).await {
            Ok(_) => Ok(()),
            Err(e) => {
                client.signal_shutdown_sync();
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests;
