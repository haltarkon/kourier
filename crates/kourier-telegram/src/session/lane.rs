//! The sync lane (TG-3 C7a; brief §4.5, §5 "no crawl"; decisions Q-L3, Q-L4): the receive's own
//! requests, ONE in flight per account, beside the login's slot and the side lane. PURE: the
//! session asks it what to send next and tells it each answer; the actor owns the future
//! (`Live.fetch`) and the timer (`Wake::LaneDue`).
//!
//! - **The read act first** (Q-L4): a `SyncReadMarker`'s `viewMessages` pages go AHEAD of every
//!   fetch and are not paced (a person's act); at most ONE read act waits per chat: a newer marker
//!   of the same chat replaces a waiting one (and confirms both commands when it is done).
//! - **Memory fetches** (`getChat`, `getUser`; fix round TF3): TDLib answers them from its own
//!   memory (no server request), so they are NOT paced and go AHEAD of the paced fetches, still
//!   one request in flight: a busy unknown chat's messages do not wait behind history pages.
//! - **Fetches** (`getMessage`; C8/C9 add the start sync's and the reconcile's pages): at most
//!   one per [`FETCH_PACE_MS`], in order.
//! - **A 429** (TDLib's "retry after N", read through `link::classify`) pauses the WHOLE lane for N
//!   seconds + 10 %, never sooner, and the same request goes again first.
//! - **A read act's answer decides** (fix round TF7): only TDLib's `ok` of the LAST page confirms
//!   the commands. A TDLib error other than 429 ends the act WITHOUT confirming (the command stays
//!   pending in the core, which sends it again at the next start); a lost link or a timeout puts
//!   the page back, like a restart does. The confirm carries the chat and the act's highest
//!   INCOMING id (`up_to`, TG-4 K4, critic M4), which the session stores in the same transaction
//!   as the command's `CommandPersisted`.
//! - **The read re-assert** (TG-4 K4, D-2): a read TDLib lost to a kill (it writes no
//!   `ReadHistoryOnServerLogEvent` with its message database off) is sent again at the lists'
//!   load as ONE `viewMessages` of the highest incoming delivered id at or below the stored position
//!   per chat ([`Lane::push_reassert`], U2); it is a
//!   person's act of earlier, so it goes right after the read act's pages, unpaced, and it has no
//!   command to confirm: its answer, whatever it is, ends it (a refused or lost answer is not
//!   retried in the run; a 429 pauses the lane and the request goes again like any other). A
//!   helper restart DROPS the waiting and the in-flight re-asserts: the next run computes its own
//!   from its own lists (so no re-assert goes twice, which D-2 would have allowed).
//! - **A lost start-load answer is no answer** (fix round U1, durability F1): the answer of a
//!   chat load (`loadChats`) or a fresh-link page (`getChatHistory` of the plan) that never came
//!   (`Outcome::Lost`: the link closed, the request timed out, the frame was not understood)
//!   puts the request back as the read act's page is put back, and does NOT end the list or
//!   complete the chat's plan: a plan completed on a lost page would commit the last message with
//!   a partial page and move the cursor, and the next run would never load the chat again. After
//!   a `Closed` the helper restart drops the requeued request (`on_restart`) and rebuilds the
//!   machines from the store (cursor 0: the next run loads the chat); after a `Timeout` the same
//!   request goes again in this run, paced like any fetch. There is NO cap on the retries: a
//!   network that stays down stops everything else the account asks too (the watchdog and the
//!   connection states are what tell the person), and a cap would give the answer up that the
//!   one-commit rule exists to keep. A TDLib error answer (`Refused`) is an answer: a refusal
//!   keeps what came (`sync.rs`).
//!
//! `Debug` prints kinds, chat ids and counts only.

use std::collections::VecDeque;

use kourier_model::CmdId;
use serde_json::{Value, json};

use crate::receive::Fetch;

/// At most one fetch per this many ms (brief §4.5 "at most ONE request per second on the sync
/// lane" [I: Telegram publishes no per-method limit]; TP measures).
pub(crate) const FETCH_PACE_MS: u64 = 1_000;

/// Message ids per `viewMessages` (Q-L3: pages of 100).
pub(crate) const VIEW_PAGE: usize = 100;

/// The share added to a 429's wait, in percent (brief §4.5: "+10 %").
const RETRY_MARGIN_PCT: u64 = 10;

/// What one request of the lane is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LaneAsk {
    /// A machine's fetch.
    Fetch(Fetch),
    /// One page of a read act: `viewMessages(chat_id, ids, force_read: true)`.
    View {
        /// TDLib's chat id.
        chat_id: i64,
        /// TDLib's full message ids.
        ids: Vec<i64>,
    },
    /// A read TDLib lost, sent again (TG-4 K4): the same request as one page of a read act, for
    /// the one id of the stored read position (the highest incoming delivered id at or below it,
    /// U2).
    Reassert {
        /// TDLib's chat id.
        chat_id: i64,
        /// TDLib's full message id of the stored read position.
        id: i64,
    },
}

impl LaneAsk {
    /// The td_api request.
    pub(crate) fn request(&self) -> Value {
        match self {
            Self::Fetch(fetch) => fetch.request(),
            Self::View { chat_id, ids } => view_request(*chat_id, ids),
            Self::Reassert { chat_id, id } => view_request(*chat_id, std::slice::from_ref(id)),
        }
    }
}

/// `viewMessages`. `source: null`: Kourier has no message source to name; `force_read`: the chat
/// is not opened (no `openChat` in T3), so TDLib must mark them read anyway (critic H3).
fn view_request(chat_id: i64, ids: &[i64]) -> Value {
    json!({
        "@type": "viewMessages",
        "chat_id": chat_id,
        "message_ids": ids,
        "source": null,
        "force_read": true,
    })
}

/// One `SyncReadMarker` of a chat (or several, merged).
#[derive(Debug)]
struct ReadAct {
    chat_id: i64,
    /// The pages still to send, in order.
    pages: VecDeque<Vec<i64>>,
    /// The commands it confirms when its last page was answered.
    cmds: Vec<CmdId>,
    /// The highest INCOMING id of the act (K4, critic M4); `None` when it holds only our own
    /// messages (no fact to store).
    up_to: Option<i64>,
}

/// The lane (see the module doc).
#[derive(Debug, Default)]
pub(crate) struct Lane {
    /// The read act being sent.
    active: Option<ReadAct>,
    /// Read acts waiting, at most one per chat.
    reads: VecDeque<ReadAct>,
    /// The fetches TDLib answers from memory ([`Fetch::answered_from_memory`]): not paced, first.
    quick: VecDeque<Fetch>,
    fetches: VecDeque<Fetch>,
    /// The read re-asserts waiting (chat id, message id), at most one per chat.
    reasserts: VecDeque<(i64, i64)>,
    in_flight: Option<LaneAsk>,
    /// No request before this clock ms (a 429).
    paused_until: Option<u64>,
    /// No fetch before this clock ms (the pace).
    next_fetch: u64,
}

/// How a lane request was answered (TF7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// TDLib answered `ok` (or the object asked for).
    Ok,
    /// A 429 with its "retry after N" seconds, when it names one.
    TooMany(Option<u32>),
    /// TDLib (or the helper) refused: an error other than 429.
    Refused,
    /// No answer came: the link closed, the request timed out, or the frame was not understood.
    Lost,
}

/// What an answer of the lane finished.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Done {
    /// Nothing for the caller (a page of a read act that goes on, or a 429 that requeued it).
    Nothing,
    /// A fetch's answer, for the machines.
    Fetch(Fetch),
    /// The read act's last page was answered: confirm these commands, and store `up_to` as the
    /// chat's read position in the same commit (`None`: the act held only our own messages).
    Read {
        /// The commands to confirm.
        cmds: Vec<CmdId>,
        /// TDLib's chat id.
        chat_id: i64,
        /// The act's highest incoming id.
        up_to: Option<i64>,
    },
}

impl Lane {
    /// A request is in flight.
    #[cfg(test)]
    pub(crate) fn in_flight(&self) -> Option<&LaneAsk> {
        self.in_flight.as_ref()
    }

    /// Fetches waiting (the one in flight not counted).
    #[cfg(test)]
    pub(crate) fn waiting(&self) -> usize {
        self.quick.len()
            + self.fetches.len()
            + self.reasserts.len()
            + self.reads.iter().map(|r| r.pages.len()).sum::<usize>()
            + self.active.as_ref().map_or(0, |r| r.pages.len())
    }

    /// Queues a machine's fetch.
    pub(crate) fn push_fetch(&mut self, fetch: Fetch) {
        if fetch.answered_from_memory() {
            self.quick.push_back(fetch);
        } else {
            self.fetches.push_back(fetch);
        }
    }

    /// Queues the read act of `cmd` over `ids` (the INCOMING delivered ids up to the marker, any
    /// order: U2, the session drops the outgoing ones); `up_to` is the highest of them (what its
    /// confirm stores). With no id there is nothing to tell: `cmd` is returned to be confirmed
    /// at once.
    pub(crate) fn push_read(
        &mut self,
        chat_id: i64,
        mut ids: Vec<i64>,
        cmd: CmdId,
        up_to: Option<i64>,
    ) -> Option<CmdId> {
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() {
            return Some(cmd);
        }
        let pages: VecDeque<Vec<i64>> = ids.chunks(VIEW_PAGE).map(<[i64]>::to_vec).collect();
        match self.reads.iter_mut().find(|r| r.chat_id == chat_id) {
            // Q-L4: the newer marker replaces the waiting one and confirms both.
            Some(waiting) => {
                waiting.pages = pages;
                waiting.cmds.push(cmd);
                waiting.up_to = waiting.up_to.max(up_to);
            }
            None => self.reads.push_back(ReadAct {
                chat_id,
                pages,
                cmds: vec![cmd],
                up_to,
            }),
        }
        None
    }

    /// Queues the re-assert of a read TDLib lost (module doc): `viewMessages(chat_id, [id])`,
    /// at most one waiting per chat.
    pub(crate) fn push_reassert(&mut self, chat_id: i64, id: i64) {
        if !self.reasserts.iter().any(|(chat, _)| *chat == chat_id) {
            self.reasserts.push_back((chat_id, id));
        }
    }

    /// When the lane must be asked again ([`Lane::next`]), `None` while one is in flight or
    /// nothing waits.
    pub(crate) fn due_ms(&self) -> Option<u64> {
        if self.in_flight.is_some() {
            return None;
        }
        let paused = self.paused_until.unwrap_or(0);
        let reads = self.active.as_ref().is_some_and(|r| !r.pages.is_empty())
            || !self.reads.is_empty()
            || !self.reasserts.is_empty();
        if reads {
            return Some(paused);
        }
        if !self.quick.is_empty() {
            return Some(paused);
        }
        if self.fetches.is_empty() {
            return None;
        }
        Some(paused.max(self.next_fetch))
    }

    /// The next request to send now, if one may go: the read act's pages first, then the read
    /// re-asserts, then a fetch TDLib answers from memory (not paced), then a paced fetch.
    pub(crate) fn next(&mut self, now_ms: u64) -> Option<LaneAsk> {
        if self.in_flight.is_some() || self.paused_until.is_some_and(|until| now_ms < until) {
            return None;
        }
        self.paused_until = None;
        if self.active.is_none() {
            self.active = self.reads.pop_front();
        }
        if let Some(act) = self.active.as_mut()
            && let Some(ids) = act.pages.pop_front()
        {
            let ask = LaneAsk::View {
                chat_id: act.chat_id,
                ids,
            };
            self.in_flight = Some(ask.clone());
            return Some(ask);
        }
        if let Some((chat_id, id)) = self.reasserts.pop_front() {
            let ask = LaneAsk::Reassert { chat_id, id };
            self.in_flight = Some(ask.clone());
            return Some(ask);
        }
        if let Some(fetch) = self.quick.pop_front() {
            let ask = LaneAsk::Fetch(fetch);
            self.in_flight = Some(ask.clone());
            return Some(ask);
        }
        if now_ms < self.next_fetch {
            return None;
        }
        let fetch = self.fetches.pop_front()?;
        self.next_fetch = now_ms.saturating_add(FETCH_PACE_MS);
        let ask = LaneAsk::Fetch(fetch);
        self.in_flight = Some(ask.clone());
        Some(ask)
    }

    /// The answer of `ask` came, `outcome` says how (a 429 without a wait is waited as one
    /// second).
    pub(crate) fn on_answer(&mut self, ask: LaneAsk, outcome: Outcome, now_ms: u64) -> Done {
        if self.in_flight.as_ref() == Some(&ask) {
            self.in_flight = None;
        }
        if let Outcome::TooMany(wait) = outcome {
            let wait_ms = u64::from(wait.unwrap_or(1)).saturating_mul(1_000);
            let margin = wait_ms.saturating_mul(RETRY_MARGIN_PCT) / 100;
            self.paused_until = Some(now_ms.saturating_add(wait_ms).saturating_add(margin));
            self.requeue(ask);
            return Done::Nothing;
        }
        match ask {
            // A lost answer of the chat load or of a fresh page is no answer (module doc, U1).
            LaneAsk::Fetch(fetch) if outcome == Outcome::Lost && fetch.is_start_load() => {
                self.requeue(ask);
                Done::Nothing
            }
            LaneAsk::Fetch(fetch) => Done::Fetch(fetch),
            // A re-assert has no command to confirm and is never retried: any answer ends it.
            LaneAsk::Reassert { .. } => Done::Nothing,
            // The read act's answer decides (TF7): a refusal ends the act without confirming it,
            // a lost answer sends the page again.
            LaneAsk::View { .. } if outcome == Outcome::Refused => {
                self.active = None;
                Done::Nothing
            }
            LaneAsk::View { .. } if outcome == Outcome::Lost => {
                self.requeue(ask);
                Done::Nothing
            }
            LaneAsk::View { .. } => match self.active.as_mut() {
                Some(act) if act.pages.is_empty() => {
                    let cmds = std::mem::take(&mut act.cmds);
                    let (chat_id, up_to) = (act.chat_id, act.up_to);
                    self.active = None;
                    Done::Read {
                        cmds,
                        chat_id,
                        up_to,
                    }
                }
                _ => Done::Nothing,
            },
        }
    }

    /// The helper went away: what was in flight is lost. A read act's page goes again with the
    /// next helper (viewing twice is harmless); the fetches (a requeued chat load or fresh page
    /// among them, U1) and the read re-asserts belonged to the old helper's machines and are
    /// dropped (the next run computes its own re-asserts and loads).
    pub(crate) fn on_restart(&mut self) {
        if let Some(ask) = self.in_flight.take()
            && matches!(ask, LaneAsk::View { .. })
        {
            self.requeue(ask);
        }
        self.quick.clear();
        self.fetches.clear();
        self.reasserts.clear();
        self.paused_until = None;
    }

    fn requeue(&mut self, ask: LaneAsk) {
        match ask {
            LaneAsk::Fetch(fetch) if fetch.answered_from_memory() => self.quick.push_front(fetch),
            LaneAsk::Fetch(fetch) => self.fetches.push_front(fetch),
            LaneAsk::View { chat_id, ids } => match self.active.as_mut() {
                Some(act) if act.chat_id == chat_id => act.pages.push_front(ids),
                _ => {}
            },
            LaneAsk::Reassert { chat_id, id } => self.reasserts.push_front((chat_id, id)),
        }
    }
}

#[cfg(test)]
mod tests;
