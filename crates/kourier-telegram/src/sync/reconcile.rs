//! The reconcile (TD-3; TG-3 C9a; brief §4.5, critic B1, decisions G-5, G-8, Q-L1, M6). PURE:
//! a part of the start sync ([`super::StartLoad`]); its pages go on the sync lane (one in flight,
//! one per second, a 429 pauses the lane) like every other fetch.
//!
//! - THE GAPS IT PAGES: every OPEN gap row: those the store held at the run's start (a gap a
//!   previous run did not finish), those the message machine opens in a row's batch (B1's check
//!   gap after an unclean end, G-8's supergroup id jump), and those the START DECISION opens.
//! - THE START DECISION, once both chat lists are loaded (the same point as C8's fresh-link
//!   load): for every listed, mapped chat that had a cursor at the run's start, TDLib's last
//!   message (`td_last`, as `updateNewChat` / `updateChatLastMessage` told it) above the chat's
//!   cursor opens the gap `(cursor, td_last)` (cause `start`) and delivers `td_last` itself as a
//!   catch-up row, both in ONE batch. That gap row is the decision's SNAPSHOT (B1): the pages
//!   compare against the range the row holds, never against the live cursor, so a live message
//!   that moves the cursor meanwhile changes nothing. The row is staged before the page is asked,
//!   and every later row (a live one, a page's) comes in the same or a later transaction: a kill
//!   in between leaves either nothing (the next start decides again) or the gap row.
//! - A PAGE is `getChatHistory(from = the gap's lower end, offset 0, limit` [`PAGE_LIMIT`]`)`.
//!   Its messages INSIDE the gap become live rows with `catch_up: true` (G-8: "delivers the
//!   missing ones as catch-up rows") while they are above the chat's delivered max; those a live
//!   row overtook (at or below it: `td_last`, or a live message, came first) are history rows
//!   (Q-D1, [`Messages::caught_up`]); messages at or below the gap's `above` are the continuity
//!   point and below (delivered, or never loaded by the fresh-link rule: no crawl) and give
//!   nothing. A page that reaches `above`, or an empty one (the chat's history ends), CLOSES the
//!   gap in the same batch as its rows; otherwise the gap's lower end moves to the oldest id
//!   (`Paged`, its page count kept in the row) and the next page follows, until
//!   [`PAGE_CAP`] pages: then the gap stays `lost` for KT-3 and NOTHING is said (G-5, Q-L1: no
//!   `HistoryLost` for Telegram). A hole left by a deleted message costs one page that finds
//!   nothing.
//! - A page TDLib refuses leaves the gap open for the next run (no retry in this one; a 429 never
//!   reaches here: the lane waits and asks again).
//! - The start sync does NOT wait for the pages (brief §4.4 step 4): the decision's rows and gap
//!   rows are staged before the start sync ends, so they are acked before `Online`; the pages
//!   continue after it, their rows catch-up.
//! - SUPERGROUPS (C9b; critic H2, decisions G-8, Q-L8): a supergroup's backlog streams on its own
//!   (TDLib runs one channel difference per supergroup, while it says Ready), so its start
//!   decision WAITS [`QUIET_MS`] after the lists are loaded, then is taken over what TDLib and the
//!   stream have said by then: a difference that delivered up to `td_last` asks nothing. A gap
//!   G-8's id trigger opens (the stream brought only the newest message after
//!   `channelDifferenceTooLong`) is paged at once, from that message down to the cursor, on the
//!   paced lane like every page.
//! - AFTER `Updating -> Ready` (C9b; G-8): when TDLib shows that transition, the decision is taken
//!   again IN MEMORY (no `loadChats`) over every mapped chat with a cursor, the gap's cause
//!   `snapshot`; supergroups wait [`QUIET_MS`] again. Nothing waits for such a transition to
//!   come: TDLib folds short states (RC4), and nothing here needs it.
//!
//! Nothing here logs a value; `Debug` prints ids and counts.

use std::collections::{BTreeMap, HashSet};

use serde_json::Value;

use kourier_db::failpoint;

use crate::error::LinkError;
use crate::receive::chats::Chats;
use crate::receive::messages::{Messages, is_server_id};
use crate::receive::{Fetch, Out};
use crate::store::{ChatKind, GapCause, GapChange, GapRow};
use crate::tdapi::Message;

/// A reconcile page's `limit` (brief §4.5).
pub(crate) const PAGE_LIMIT: i32 = 50;

/// At most this many pages per gap, over all runs (the plan's cap, `plan.md:366-368`); past it
/// the gap stays `lost` for KT-3.
pub(crate) const PAGE_CAP: i64 = 3;

/// The kill point when a reconcile page has arrived and nothing of it is staged (TG-3 C10a, brief
/// §4.7: between two fetched pages).
pub(crate) const IN_RECONCILE: &str = "tg-in-reconcile";

/// A supergroup's decision waits this long for its own stream (critic H2, decision Q-L8: 2 s
/// after `loadChats`; measured in TP).
pub(crate) const QUIET_MS: i64 = 2_000;

/// A gap the reconcile pages, keyed by `(chat_id, above)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Paging {
    /// The gap's lower end now (exclusive): the next page starts here.
    below: i64,
    /// Pages read so far (the row's count).
    pages: i64,
}

/// The reconcile of one helper run (see the module doc).
#[derive(Debug, Default)]
pub(crate) struct Reconcile {
    gaps: BTreeMap<(i64, i64), Paging>,
    /// Gaps lost at the cap (this run or an earlier one): never opened again (TF10).
    lost: HashSet<(i64, i64)>,
    /// Gaps with a page asked and not answered.
    asked: HashSet<(i64, i64)>,
    /// The start decision was taken: a gap opened from now on is paged at once.
    decided: bool,
    /// Supergroups whose decision waits for their stream: when, and the cause of its gap.
    quiet: BTreeMap<i64, (i64, GapCause)>,
}

/// A `getChatHistory` answer (`messages`, `td_api.tl`): TDLib may put a null in the list.
#[derive(serde::Deserialize)]
struct Page {
    #[serde(default)]
    messages: Vec<Option<Message>>,
}

impl Reconcile {
    /// The open gaps the store held at the run's start (lost ones are KT-3's).
    pub(crate) fn seed(&mut self, rows: &[GapRow]) {
        self.lost
            .extend(rows.iter().filter(|g| g.lost).map(|g| (g.chat_id, g.above)));
        for g in rows.iter().filter(|g| !g.lost) {
            self.gaps.insert(
                (g.chat_id, g.above),
                Paging {
                    below: g.below,
                    pages: g.pages,
                },
            );
        }
    }

    /// The gaps a machine opened in `out` (in its rows' batch): tracked, and paged at once once
    /// the start decision was taken (before it, the decision asks them).
    pub(crate) fn track(&mut self, out: &mut Out) {
        let mut fetch = Vec::new();
        for change in &out.staged.gaps {
            let GapChange::Open {
                chat_id,
                above,
                below,
                ..
            } = *change
            else {
                continue;
            };
            let key = (chat_id, above);
            // A LOST gap is never opened again, in the file and here alike (TF10, review dF10):
            // how KT-3 retries one is its decision.
            if self.lost.contains(&key) {
                continue;
            }
            let paging = self.gaps.entry(key).or_insert(Paging { below, pages: 0 });
            // The store widens an open gap to the larger end; so does the reconcile.
            paging.below = paging.below.max(below);
            if self.decided && self.asked.insert(key) {
                fetch.push(page_of(key, paging.below));
            }
        }
        out.fetch.extend(fetch);
    }

    /// The start decision (module doc), once both chat lists are loaded; then the first page of
    /// every open gap.
    pub(crate) fn decide(
        &mut self,
        chats: &mut Chats,
        messages: &mut Messages,
        now_ms: i64,
    ) -> Out {
        let mut out = Out::default();
        if self.decided {
            return out;
        }
        self.decided = true;
        for chat_id in chats.listed_in_order() {
            if messages.stored_cursor(chat_id) == 0 {
                // A fresh link's chat: C8's load.
                continue;
            }
            self.decide_or_wait(chats, messages, chat_id, GapCause::Start, now_ms, &mut out);
        }
        for (&key, paging) in &self.gaps {
            // A chat held now (TF11) keeps its stored gap open, unpaged, for T7's lift (not marked
            // lost): a request for a held chat is the one thing H5 forbids.
            if chats.is_held(key.0) {
                continue;
            }
            if self.asked.insert(key) {
                out.fetch.push(page_of(key, paging.below));
            }
        }
        out
    }

    /// TDLib showed `Updating -> Ready` (module doc): the decision again, in memory, over every
    /// mapped chat with a cursor. Before the start decision nothing (that one covers it).
    pub(crate) fn redecide(
        &mut self,
        chats: &mut Chats,
        messages: &mut Messages,
        now_ms: i64,
    ) -> Out {
        let mut out = Out::default();
        if !self.decided {
            return out;
        }
        for chat_id in chats.listed_in_order() {
            if messages.cursor(chat_id).delivered_max == 0 {
                continue;
            }
            self.decide_or_wait(
                chats,
                messages,
                chat_id,
                GapCause::Snapshot,
                now_ms,
                &mut out,
            );
        }
        out
    }

    /// When the next supergroup's decision is due ([`Reconcile::on_tick`]).
    pub(crate) fn due_ms(&self) -> Option<i64> {
        self.quiet.values().map(|&(due, _)| due).min()
    }

    /// The clock reached [`Reconcile::due_ms`]: every supergroup whose wait is over is decided now.
    pub(crate) fn on_tick(
        &mut self,
        chats: &mut Chats,
        messages: &mut Messages,
        now_ms: i64,
    ) -> Out {
        let mut out = Out::default();
        let due: Vec<(i64, GapCause)> = self
            .quiet
            .iter()
            .filter(|(_, (at, _))| *at <= now_ms)
            .map(|(&chat_id, &(_, cause))| (chat_id, cause))
            .collect();
        for (chat_id, cause) in due {
            self.quiet.remove(&chat_id);
            self.decide_chat(chats, messages, chat_id, cause, now_ms, &mut out);
        }
        out
    }

    /// A supergroup waits [`QUIET_MS`] for its stream; any other chat is decided now.
    fn decide_or_wait(
        &mut self,
        chats: &mut Chats,
        messages: &mut Messages,
        chat_id: i64,
        cause: GapCause,
        now_ms: i64,
        out: &mut Out,
    ) {
        if matches!(
            chats.kind(chat_id),
            Some(ChatKind::Supergroup | ChatKind::Forum | ChatKind::Channel)
        ) {
            self.quiet
                .entry(chat_id)
                .or_insert((now_ms.saturating_add(QUIET_MS), cause));
        } else {
            self.decide_chat(chats, messages, chat_id, cause, now_ms, out);
        }
    }

    /// One chat's decision: TDLib's last message above the chat's cursor opens the gap
    /// `(cursor, td_last)` and is delivered as a catch-up row in the same batch; its first page
    /// is asked once the start decision was taken.
    fn decide_chat(
        &mut self,
        chats: &mut Chats,
        messages: &mut Messages,
        chat_id: i64,
        cause: GapCause,
        now_ms: i64,
        out: &mut Out,
    ) {
        if chats.conv(chat_id).is_none() {
            // Held or no longer mapped (Q-B3: a held chat opens no gap).
            return;
        }
        let Some(last) = chats
            .chat(chat_id)
            .and_then(|c| c.last_message.as_deref().cloned())
        else {
            return;
        };
        let cursor = messages.cursor(chat_id).delivered_max;
        if !is_server_id(last.id) || last.id <= cursor {
            return;
        }
        let key = (chat_id, cursor);
        out.staged.gaps.push(GapChange::Open {
            chat_id,
            above: cursor,
            below: last.id,
            cause,
        });
        let paging = self.gaps.entry(key).or_insert(Paging {
            below: last.id,
            pages: 0,
        });
        paging.below = paging.below.max(last.id);
        let below = paging.below;
        out.extend(messages.caught_up(chats, chat_id, std::slice::from_ref(&last), now_ms));
        if self.decided && self.asked.insert(key) {
            out.fetch.push(page_of(key, below));
        }
    }

    /// The answer of the page `(chat_id, above, from)`.
    pub(crate) fn on_page(
        &mut self,
        (chat_id, above, from): (i64, i64, i64),
        result: Result<Value, LinkError>,
        chats: &mut Chats,
        messages: &mut Messages,
        now_ms: i64,
    ) -> Out {
        let mut out = Out::default();
        let key = (chat_id, above);
        self.asked.remove(&key);
        let Some(&paging) = self.gaps.get(&key) else {
            return out;
        };
        // The chat became held while the page was asked (TF11): its rows are not delivered and no
        // next page is asked; the gap stays open for T7's lift.
        if chats.is_held(chat_id) {
            return out;
        }
        // A page arrived, nothing of it staged yet (between two fetched pages).
        failpoint::hit(IN_RECONCILE);
        let Ok(answer) = result else {
            tracing::debug!(
                chat_id,
                "TDLib refused a reconcile page; the gap stays open"
            );
            return out;
        };
        let page: Vec<Message> = serde_json::from_value::<Page>(answer)
            .map(|p| p.messages.into_iter().flatten().collect())
            .unwrap_or_default();
        let page: Vec<Message> = page.into_iter().filter(|m| m.chat_id == chat_id).collect();
        // The SNAPSHOT: the gap row's range, never the live cursor (B1).
        let inside: Vec<Message> = page
            .iter()
            .filter(|m| m.id > above && m.id < paging.below)
            .cloned()
            .collect();
        out.extend(messages.caught_up(chats, chat_id, &inside, now_ms));
        if from != paging.below {
            // Widened while the page was asked: the part above `from` first.
            self.asked.insert(key);
            out.fetch.push(page_of(key, paging.below));
            return out;
        }
        let oldest = page.iter().map(|m| m.id).filter(|&id| id > 0).min();
        let pages = paging.pages.saturating_add(1);
        match oldest {
            None => self.close(key, &mut out),
            Some(oldest) if oldest <= above => self.close(key, &mut out),
            Some(oldest) => {
                out.staged.gaps.push(GapChange::Paged {
                    chat_id,
                    above,
                    below: oldest,
                });
                if pages >= PAGE_CAP {
                    // G-5, Q-L1: kept for KT-3; no notice.
                    out.staged.gaps.push(GapChange::Lost { chat_id, above });
                    self.gaps.remove(&key);
                    self.lost.insert(key);
                } else {
                    self.gaps.insert(
                        key,
                        Paging {
                            below: oldest,
                            pages,
                        },
                    );
                    self.asked.insert(key);
                    out.fetch.push(page_of(key, oldest));
                }
            }
        }
        out
    }

    /// The gap `key` is filled: closed in the batch of its last rows.
    fn close(&mut self, (chat_id, above): (i64, i64), out: &mut Out) {
        self.gaps.remove(&(chat_id, above));
        out.staged.gaps.push(GapChange::Close { chat_id, above });
    }

    /// Open gaps tracked (paged or waiting for the decision).
    #[cfg(test)]
    pub(crate) fn open(&self) -> usize {
        self.gaps.len()
    }
}

/// The page request of gap `key` from `from`.
fn page_of((chat_id, above): (i64, i64), from: i64) -> Fetch {
    Fetch::Gap {
        chat_id,
        above,
        from,
        limit: PAGE_LIMIT,
    }
}
