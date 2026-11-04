//! The start sync's loads (TG-3 C8; brief §4.4, §4.5 "new chat", his first-load answer "Last +
//! unread, up to 20", decisions M6, M7, Q-B3, Q-L2; "no crawl"). PURE: the receive machines'
//! third part ([`crate::receive::Receiver::sync`]); its requests go on the sync lane (one in
//! flight, one per second, a 429 pauses the lane) like every other fetch.
//!
//! - THE CHAT LOAD: `loadChats(main, 100)` until TDLib answers 404 (the list is complete, golden
//!   at the pin), then `loadChats(archive, 100)` until 404. TDLib tells each page's chats as
//!   `updateNewChat`s, which the chat machine maps as they come. Any other error ends the whole
//!   load (a TDLib that refuses `loadChats` would refuse the archive too); the reconcile's
//!   decision follows either way. A LOST answer is no refusal: the lane asks it again (U1).
//! - THE FRESH-LINK LOAD (TG-4 K5, ONE COMMIT PER CHAT), once both lists are done, for every
//!   listed, mapped chat whose cursor `chat_state` held at the run's start is 0 (a chat Kourier
//!   never delivered from), in the list's order: the chat's LAST message as TDLib told it
//!   (`chat.last_message` / `updateChatLastMessage`, no request); for a chat with unread messages
//!   BELOW its last one, `getChatHistory(from = the last id, offset 0, limit = min(below, 20))`
//!   where `below` is exactly those messages (U5: TDLib's `unread_count` counts the last message
//!   itself when it is incoming and above the read position, so `below = unread - 1` then, else
//!   `unread`: [`unread_below_last`]), and again from the oldest id returned while an answer is
//!   SHORT and NOT EMPTY and the cap is not reached (which call reaches the server is TDLib's
//!   timing, RC1). The last message is NOT
//!   delivered before the pages: the plan keeps it and the pages' rows, and when the plan is
//!   complete (an empty page, a full one, the cap, or a TDLib refusal, which keeps what came)
//!   ONE `HistoryBatch` of the rows and the last message goes out in one [`Out`] (one
//!   transaction: the cursor moves with the whole batch, so a failed commit or a kill leaves the
//!   chat with cursor 0 and the next run loads it again; gate (5) of "Before R1"). A LOST answer
//!   (no answer at all: the link closed, the request timed out) is NOT a refusal and completes
//!   nothing: the sync lane asks the same request again (U1, `session::lane`), and after a
//!   closed link the restart drops it and the next run loads the chat from cursor 0. The same
//!   holds for a lost `loadChats` answer: it never ends a list. A chat with no unread message
//!   below its last one delivers its last message at once, nothing to wait for. The rows are history
//!   (never notified, `has_more: false`), the unread messages without rows stay the chat's
//!   `unread_hint` (the chat machine counts the rows). Held chats are never fetched (Q-B3); a
//!   chat with a cursor is the reconcile's ([`reconcile`], C9a), not crawled here: its start
//!   decision is taken at the same point.
//! - THE FRESH-CHAT HOLD (H1, M5; [`Messages::hold_fresh_chats`]): a live row of a chat with a
//!   stored cursor of 0 waits from its first one of the run until its fresh-link batch (a chat
//!   with a plan) or until the load has run for it (a chat without one), and is replayed in the
//!   same [`Out`] right after the batch: it is a catch-up row (no notification) on a first link
//!   (the cost: those live rows wait until the lists are loaded plus the chat's page). A chat
//!   first seen after the load is a normal live chat; a late load (TF4) holds its rows the same
//!   way from its plan on.
//! - THE READ RE-ASSERT (TG-4 K4, D-2; U2), taken with the fresh-link load: every listed, mapped
//!   chat whose stored read position is above what TDLib told in this run gets ONE `viewMessages`
//!   of the highest incoming delivered id at or below the stored position (clamped to TDLib's
//!   told last message) again (a read TDLib lost to a kill: with its message database off it
//!   keeps nothing to replay); [`Messages::lost_reads`].
//! - The start sync is over ([`StartLoad::is_done`]) when both lists are loaded and every page
//!   was answered; the session then ends it (`Act::Synced`: Online after the acks, L-6). The
//!   reconcile's pages go on after it (brief §4.4 step 4).
//!
//! Nothing here logs a value; `Debug` prints the phase and counts.

pub(crate) mod reconcile;

use std::collections::{BTreeSet, HashMap};

use serde::Deserialize;
use serde_json::Value;

use crate::error::LinkError;
use crate::receive::chats::Chats;
use crate::receive::messages::{Messages, is_server_id};
use crate::receive::{Fetch, Out};
use crate::tdapi::Message;

/// `loadChats`' `limit` (TDLib loads 100 dialogs per call whatever it says, brief §2).
pub(crate) const LOAD_CHATS_LIMIT: i32 = 100;

/// His first-load answer: at most this many unread messages per chat at a fresh link.
pub(crate) const FRESH_UNREAD_CAP: i32 = 20;

/// The unread messages BELOW a chat's last message (U5, his "Last + unread, up to 20"): TDLib's
/// `unread_count` counts the last message itself when it is incoming and above the read position
/// (`last_read`), so the page below it has one message fewer than the count says; when the last
/// message is our own or already read, every unread message lies below it.
pub(crate) fn unread_below_last(unread: i32, last: &Message, last_read: i64) -> i32 {
    let counted = !last.is_outgoing && last.id > last_read;
    (unread - i32::from(counted)).max(0)
}

/// Where the start sync's loads stand.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Phase {
    /// Not begun (the run's first Ready of a linked account begins it).
    #[default]
    Idle,
    /// `loadChats` of the main list (`false`) or the archive (`true`) runs.
    Lists { archive: bool },
    /// The fresh-link pages run.
    Pages,
    /// Over.
    Done,
}

/// One chat's fresh-link page plan.
#[derive(Debug, Clone)]
struct Plan {
    /// min(the unread messages below the last one, [`FRESH_UNREAD_CAP`]).
    cap: i32,
    /// Messages the pages brought so far.
    loaded: i32,
    /// The chat's last message as TDLib told it: delivered with the pages' rows, never before.
    last: Message,
    /// The pages' messages so far (not delivered until the plan is complete).
    rows: Vec<Message>,
}

/// The start sync's loads of one helper run (see the module doc).
#[derive(Debug, Default)]
pub(crate) struct StartLoad {
    phase: Phase,
    plans: HashMap<i64, Plan>,
    /// Chats that were known but NOT decided when the fresh-link load ran (TF4: they wait for a
    /// user or a supergroup object); each gets its own load once decided ([`StartLoad::late_loads`]).
    undecided: BTreeSet<i64>,
    /// Pages asked and not answered yet.
    pending: usize,
    /// The reconcile (C9a): its decision is taken with the fresh-link load; its pages do not hold
    /// the start sync.
    pub(crate) reconcile: reconcile::Reconcile,
}

/// A `getChatHistory` answer (`messages`, `td_api.tl`): TDLib may put a null in the list.
#[derive(Deserialize)]
struct Page {
    #[serde(default)]
    messages: Vec<Option<Message>>,
}

/// TDLib's error code of an answer.
fn code(result: &Result<Value, LinkError>) -> Option<i32> {
    match result {
        Err(LinkError::Td { code } | LinkError::Auth { code, .. }) => Some(*code),
        _ => None,
    }
}

impl StartLoad {
    /// The loads are over.
    pub(crate) fn is_done(&self) -> bool {
        self.phase == Phase::Done
    }

    /// Begins the chat load (once per run; a second call asks nothing).
    pub(crate) fn begin(&mut self) -> Out {
        let mut out = Out::default();
        if self.phase == Phase::Idle {
            self.phase = Phase::Lists { archive: false };
            out.fetch.push(Fetch::LoadChats { archive: false });
        }
        out
    }

    /// The answer of `loadChats` of a list.
    pub(crate) fn on_load(
        &mut self,
        archive: bool,
        result: &Result<Value, LinkError>,
        chats: &mut Chats,
        messages: &mut Messages,
        now_ms: i64,
    ) -> Out {
        let mut out = Out::default();
        if self.phase != (Phase::Lists { archive }) {
            return out;
        }
        match (result, code(result)) {
            (Ok(_), _) => out.fetch.push(Fetch::LoadChats { archive }),
            (Err(_), Some(404)) if !archive => {
                self.phase = Phase::Lists { archive: true };
                out.fetch.push(Fetch::LoadChats { archive: true });
            }
            (Err(_), Some(404)) => out.extend(self.fresh_load(chats, messages, now_ms)),
            (Err(_), _) => {
                tracing::debug!("TDLib refused the chat load; the start sync goes on without it");
                out.extend(self.fresh_load(chats, messages, now_ms));
            }
        }
        out
    }

    /// Both lists are loaded: the fresh-link load of every new chat (module doc).
    fn fresh_load(&mut self, chats: &mut Chats, messages: &mut Messages, now_ms: i64) -> Out {
        let mut out = Out::default();
        self.phase = Phase::Pages;
        self.undecided = chats.deciding_ids().into_iter().collect();
        for chat_id in chats.listed_in_order() {
            out.extend(self.fresh_chat(chat_id, chats, messages, now_ms));
        }
        // The fresh-chat hold (K5, H1): the live rows of every fresh chat WITHOUT a page plan
        // follow its last message in this batch; a planned chat's wait for its own batch.
        out.extend(messages.end_lists_hold(chats, now_ms));
        // The reconcile's start decision over the chats Kourier delivered from before (C9a): its
        // gap rows and `td_last` rows go in this batch, before the start sync ends.
        out.extend(self.reconcile.decide(chats, messages, now_ms));
        // The read re-assert (TG-4 K4, D-2): a read Kourier confirmed that TDLib lost to a kill
        // is sent again, once per chat; it is not a page and does not hold the start sync.
        out.reassert.extend(messages.lost_reads(chats));
        self.finish_if_idle();
        out
    }

    /// One chat's fresh-link load (module doc): the page plan for its unread messages, or, with
    /// at most one unread message, its last message as history at once.
    fn fresh_chat(
        &mut self,
        chat_id: i64,
        chats: &mut Chats,
        messages: &mut Messages,
        now_ms: i64,
    ) -> Out {
        let mut out = Out::default();
        if messages.stored_cursor(chat_id) != 0 {
            // Delivered from before: the reconcile's (C9), no crawl here.
            return out;
        }
        let Some(chat) = chats.chat(chat_id) else {
            return out;
        };
        let unread = chat.unread_count;
        let last_read = chat.last_read_inbox_message_id;
        let Some(last) = chat.last_message.as_deref().cloned() else {
            return out;
        };
        if !is_server_id(last.id) {
            return out;
        }
        let below = unread_below_last(unread, &last, last_read);
        if below > 0 {
            // One commit (K5): the last message waits for the pages, and the chat's live rows
            // wait for the batch (`Messages::hold_planned`).
            let cap = below.min(FRESH_UNREAD_CAP);
            let from = last.id;
            messages.hold_planned(chat_id);
            self.plans.insert(
                chat_id,
                Plan {
                    cap,
                    loaded: 0,
                    last,
                    rows: Vec::new(),
                },
            );
            self.pending += 1;
            out.fetch.push(Fetch::History {
                chat_id,
                from,
                limit: cap,
            });
        } else {
            out.extend(messages.history(chats, chat_id, std::slice::from_ref(&last), now_ms));
        }
        out
    }

    /// The chats the fresh-link load skipped as undecided and that are decided by now (TF4): a
    /// listed, mapped chat with no cursor and no row of this run gets its own load, the same
    /// one. Cheap when none was skipped.
    pub(crate) fn late_loads(
        &mut self,
        chats: &mut Chats,
        messages: &mut Messages,
        now_ms: i64,
    ) -> Out {
        let mut out = Out::default();
        if self.undecided.is_empty() {
            return out;
        }
        let decided: Vec<i64> = self
            .undecided
            .iter()
            .copied()
            .filter(|id| !chats.deciding(*id))
            .collect();
        for chat_id in decided {
            self.undecided.remove(&chat_id);
            if chats.is_listed_mapped(chat_id) && messages.cursor(chat_id).delivered_max == 0 {
                out.extend(self.fresh_chat(chat_id, chats, messages, now_ms));
            }
        }
        out
    }

    /// The answer of a fresh-link page `(chat_id, from, limit)`.
    pub(crate) fn on_history(
        &mut self,
        (chat_id, _from, limit): (i64, i64, i32),
        result: Result<Value, LinkError>,
        chats: &mut Chats,
        messages: &mut Messages,
        now_ms: i64,
    ) -> Out {
        let mut out = Out::default();
        // A late load (TF4) asks pages after the start sync ended (`Done`).
        if !matches!(self.phase, Phase::Pages | Phase::Done) {
            return out;
        }
        self.pending = self.pending.saturating_sub(1);
        let page: Vec<Message> = result
            .ok()
            .and_then(|v| serde_json::from_value::<Page>(v).ok())
            .map(|m| m.messages.into_iter().flatten().collect())
            .unwrap_or_default();
        let page: Vec<Message> = page.into_iter().filter(|m| m.chat_id == chat_id).collect();
        let n = i32::try_from(page.len()).unwrap_or(i32::MAX);
        let oldest = page.iter().map(|m| m.id).filter(|id| *id > 0).min();
        let Some(plan) = self.plans.get_mut(&chat_id) else {
            // Only a plan asks pages, so this is not reached; deliver what came, as a plain batch.
            out.extend(messages.history(chats, chat_id, &page, now_ms));
            self.finish_if_idle();
            return out;
        };
        plan.loaded = plan.loaded.saturating_add(n);
        plan.rows.extend(page);
        // A short page is not the end (TDLib answers what it holds in memory first); an empty
        // one is.
        if let Some(oldest) = oldest
            && n > 0
            && n < limit
            && plan.loaded < plan.cap
        {
            self.pending += 1;
            out.fetch.push(Fetch::History {
                chat_id,
                from: oldest,
                limit: plan.cap - plan.loaded,
            });
        } else if let Some(plan) = self.plans.remove(&chat_id) {
            // The plan is complete (an empty or full page, the cap, or a refusal, which keeps
            // what came): ONE batch of the pages' rows and the last message, then the chat's
            // held live rows, all in this output = one transaction (K5, H1).
            let Plan { last, mut rows, .. } = plan;
            rows.push(last);
            out.extend(messages.history(chats, chat_id, &rows, now_ms));
            out.extend(messages.release_planned(chats, chat_id, now_ms));
        }
        self.finish_if_idle();
        out
    }

    fn finish_if_idle(&mut self) {
        if self.phase == Phase::Pages && self.pending == 0 {
            self.phase = Phase::Done;
        }
    }
}

#[cfg(test)]
mod tests;
