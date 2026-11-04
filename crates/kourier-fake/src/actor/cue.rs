//! The acts of the rehearsal's phone on cue (T2, T-5; [`crate::FakeControl::phone`]): the
//! settings it changes, a call, and the other person's react, edit and delete of the text that
//! arrived last. Each is the event the real backend states for the same thing, committed like an
//! arrival (one `core_outbox` row); the app is told nothing else and the fake takes no frame.

use kourier_backend_api::BackendEvent;
use kourier_model::{Body, CallMedia, ConvKey, ConversationPatch, NetworkKind, Patch, UserId};

use super::{Actor, BackendError, ErrorKind};
use crate::calls::Call;
use crate::rehearsal::{ARRIVAL_WORDS, Arrived, CALL_RINGS_FOR, Injection, PhoneAct};
use crate::scripts::target;

/// The other person's reaction.
const REACTION: &str = "👍";

impl Actor {
    /// Plays `act` in the chat of `at`. Only the fake that plays WhatsApp takes cues: the fake
    /// playing Telegram keeps its own arrival shapes and ignores them.
    pub(super) async fn cue(&self, act: PhoneAct, at: Injection) -> Result<(), BackendError> {
        if self.kind == NetworkKind::Telegram {
            tracing::warn!(
                act = act.name(),
                "the fake playing Telegram plays no phone act"
            );
            return Ok(());
        }
        let Injection { conv, author, .. } = at;
        match act {
            PhoneAct::Archive | PhoneAct::Unarchive => {
                let mut patch = ConversationPatch::new(conv);
                patch.archived = Some(act == PhoneAct::Archive);
                self.commit_events(&[BackendEvent::ConversationUpsert(patch)], |_| Ok(()))
            }
            PhoneAct::Mute | PhoneAct::Unmute => {
                let mut patch = ConversationPatch::new(conv);
                // The phone's "for ever" is the largest time.
                patch.muted_until = if act == PhoneAct::Mute {
                    Patch::Set(i64::MAX)
                } else {
                    Patch::Clear
                };
                self.commit_events(&[BackendEvent::ConversationUpsert(patch)], |_| Ok(()))
            }
            PhoneAct::Pin | PhoneAct::Unpin => {
                // The network states the whole pin list, the newest pin first.
                let mut list = self.pinned()?;
                list.retain(|k| *k != conv);
                if act == PhoneAct::Pin {
                    list.insert(0, conv);
                }
                self.commit_events(&[BackendEvent::PinnedConversations(list)], |_| Ok(()))
            }
            PhoneAct::Call => self.call_in(conv, author).await,
            PhoneAct::PeerReact | PhoneAct::PeerEdit | PhoneAct::PeerDelete => {
                let arrived = self.arrived_in(&conv).ok_or_else(|| {
                    BackendError::new(
                        ErrorKind::InvalidInput,
                        "the phone has no text of this start to act on",
                    )
                })?;
                let now = self.now();
                let id = target(Some(&conv), Some(&author), &arrived.local);
                let event = match act {
                    PhoneAct::PeerReact => BackendEvent::Reaction {
                        target: id,
                        by: Some(author),
                        emoji: REACTION.into(),
                        remove: false,
                        at: now,
                    },
                    PhoneAct::PeerEdit => BackendEvent::Edited {
                        target: id,
                        by: Some(author),
                        // WhatsApp keeps the original id through an edit.
                        new_local: None,
                        body: Body::text(format!("{ARRIVAL_WORDS} {} edited", arrived.n)),
                        edited_at: now,
                        keeps_quote: None,
                    },
                    _ => BackendEvent::Deleted {
                        target: id,
                        by: Some(author),
                        for_everyone: true,
                        at: now,
                    },
                };
                self.commit_events(&[event], |_| Ok(()))
            }
        }
    }

    /// A 1:1 call of the other person: it rings, and [`CALL_RINGS_FOR`] later the phone's own
    /// call log says it was answered there (the signal W6a1 maps from the app state).
    async fn call_in(&self, conv: ConvKey, caller: UserId) -> Result<(), BackendError> {
        let n = self.bump("phone_calls")?;
        let id = u64::try_from(n).unwrap_or(0);
        let mut call = Call::direct(&caller, id, CallMedia::Voice);
        // A prefixed id, so the run tool's leak search can tell the rehearsal's call ids from any other text.
        call.id = format!("{}{id}", crate::rehearsal::CALL_ID_PREFIX);
        debug_assert_eq!(call.conv, conv);
        self.commit_events(&[call.ring(self.now())], |_| Ok(()))?;
        crate::rehearsal::say_call_offer_to_upstream();
        tokio::time::sleep(CALL_RINGS_FOR).await;
        self.commit_events(&[call.answered_elsewhere(self.now())], |_| Ok(()))
    }

    /// Remembers the text [`Actor::arrive`] committed as the newest of its chat.
    pub(super) fn note_arrived(&self, conv: &ConvKey, local: String, n: i64) {
        let mut known = self
            .known
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(chat) = known.iter_mut().find(|c| c.conv == *conv) {
            chat.newest = Some(Arrived { local, n });
        }
    }

    /// The newest text that arrived in `conv` in this start.
    fn arrived_in(&self, conv: &ConvKey) -> Option<Arrived> {
        self.known
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|c| c.conv == *conv)
            .and_then(|c| c.newest.clone())
            .filter(|a| !a.local.is_empty())
    }
}
