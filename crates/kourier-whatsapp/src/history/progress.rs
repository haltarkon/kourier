//! Progress and the end of a push (W4 C15): which chunks say `has_more`, in which order the chunks are
//! delivered, and when a sync type's push is over. PURE: the feeder ([`super::feeder`]) reads the facts
//! from the stage and `backend.db` and asks.
//!
//! WHAT THE PHONE SENDS. After a link the phone uploads several SYNC TYPES, each a series of chunks
//! numbered by `chunkOrder` with a `progress` of its own (0 to 100): INITIAL_BOOTSTRAP (the recent chats),
//! RECENT and FULL (more history), PUSH_NAME (names only), NON_BLOCKING_DATA, INITIAL_STATUS_V3 (status
//! updates, which are let go) and ON_DEMAND (the answer to a request for older history). Two tasks of the
//! receive path capture concurrently, so the order of capture is not the order of `chunkOrder`, and one
//! sync type may reach 100 % while another still goes on.
//!
//! EVERY BATCH of a chunk carries that chunk's own progress, per sync type, and `has_more` while the
//! chunk's sync type pushes chats ([`pushes_chats`]). PUSH_NAME, NON_BLOCKING_DATA, STATUS and ON_DEMAND
//! chunks never say `has_more` (the first three touch no chat; the last answers a request and ends by
//! itself), so nothing they do can leave the banner up. The account's own progress report is never sent
//! (a test scans the crate's source for its name): the viewmodel prefers it to the fill banner and would read
//! "Updating contacts and groups".
//!
//! THE PUSH OF A SYNC TYPE IS OVER ([`Facts::over`]) when a chunk of the type with progress 100 has been
//! delivered and nothing can still bring a lower one: no chunk of the type waits in the stage (captured or
//! planned and not delivered), and no announced chunk is unresolved (a notice we recorded whose chunk is
//! neither in the stage nor counted lost). Then every chat whose last chunk was of that type gets its end
//! (an empty batch, `has_more: false`). A chat the phone marked `endOfHistoryTransfer` ends when the chunk
//! that said so is delivered, whatever the type's push does. A chunk that arrives late, after an end, sets
//! `has_more` again for the chats it touches and the push ends again behind it: the contract allows it, the
//! banner returns for a moment.
//!
//! THE ORDER OF DELIVERY. One chunk at a time: the lowest [`rank`] of sync type first (names before the
//! rest, the bootstrap before the long history), then the lowest `chunkOrder`, then the order of capture.
//! A chunk captured meanwhile with a lower order is delivered when it comes: the bound above keeps the end
//! behind it.
//!
//! A PUSH CAN ALSO END WITHOUT ITS LAST CHUNK (W4 fix F6): when the 100 % chunk of a sync type is lost or does not read,
//! no delivered chunk of the type says 100 and [`Facts::over`] never turns true. So [`Facts::account_over`]: nothing
//! of the account can still arrive (no chunk waits, no announced chunk is unresolved) AND the push cannot be taken
//! for running (a chunk was lost or did not read, or the newest capture is older than [`PUSH_QUIET_MS`]): then every
//! chat that still owes an end gets it. A chunk that arrives after that opens its chats again, as after any end.
//!
//! Nothing here reads a file or logs.

use std::collections::HashSet;

/// The numbers of `HistorySync.HistorySyncType`.
pub(crate) mod sync {
    pub(crate) const INITIAL_BOOTSTRAP: i32 = 0;
    pub(crate) const INITIAL_STATUS_V3: i32 = 1;
    pub(crate) const FULL: i32 = 2;
    pub(crate) const RECENT: i32 = 3;
    pub(crate) const PUSH_NAME: i32 = 4;
    pub(crate) const NON_BLOCKING_DATA: i32 = 5;
    pub(crate) const ON_DEMAND: i32 = 6;
}

/// Whether the chunks of `sync_type` go on telling that more follows (`has_more`) and so owe an end. A type
/// this build does not know is treated as a push: its end then comes from the phone's 100 % or, at the
/// latest, from the viewmodel's idle rule.
pub(crate) fn pushes_chats(sync_type: Option<i32>) -> bool {
    !matches!(
        sync_type,
        Some(sync::INITIAL_STATUS_V3 | sync::PUSH_NAME | sync::NON_BLOCKING_DATA | sync::ON_DEMAND)
    )
}

/// The place of a sync type in the order of delivery (lower first).
pub(crate) fn rank(sync_type: Option<i32>) -> u8 {
    match sync_type {
        Some(sync::PUSH_NAME) => 0,
        Some(sync::INITIAL_BOOTSTRAP) => 1,
        Some(sync::INITIAL_STATUS_V3) => 2,
        Some(sync::RECENT) => 3,
        Some(sync::FULL) => 4,
        Some(sync::NON_BLOCKING_DATA) => 5,
        Some(sync::ON_DEMAND) => 6,
        _ => 7,
    }
}

/// The phone's own 0 to 100 as the contract's 0.0 to 1.0.
pub(crate) fn fraction(progress: Option<u32>) -> Option<f32> {
    progress.map(|p| p.min(100) as f32 / 100.0)
}

/// Where a chunk of the stage stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    Captured,
    Planned,
    Bad,
    Done,
}

impl State {
    /// The stage's text of a state.
    pub(crate) fn of_text(text: &str) -> Self {
        match text {
            "captured" => Self::Captured,
            "planned" => Self::Planned,
            "done" => Self::Done,
            _ => Self::Bad,
        }
    }
}

/// How long after the newest capture a push with nothing waiting and nothing announced is taken as over (F6).
pub(crate) const PUSH_QUIET_MS: i64 = 6 * 60 * 60 * 1000;

/// A chunk as the stage holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Staged {
    pub(crate) seq: i64,
    pub(crate) msg_id: String,
    pub(crate) sync_type: Option<i32>,
    pub(crate) order: Option<u32>,
    pub(crate) state: State,
    /// When it was captured (wall clock, ms).
    pub(crate) captured_at: i64,
}

/// A chunk whose content was all appended (`history_chunks`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Delivered {
    pub(crate) chunk: i64,
    pub(crate) sync_type: Option<i32>,
    pub(crate) order: Option<u32>,
    pub(crate) progress: Option<u32>,
}

/// Everything the end of a push is judged from.
#[derive(Clone, Debug, Default)]
pub(crate) struct Facts {
    /// Every chunk of the stage.
    pub(crate) staged: Vec<Staged>,
    /// The chunks delivered.
    pub(crate) delivered: Vec<Delivered>,
    /// The ids of the notices recorded and not counted lost (`history_announced`).
    pub(crate) announced: Vec<String>,
    /// A chunk of the account was counted lost (`history_counts.lost > 0`).
    pub(crate) lost: bool,
    /// The time now (wall clock, ms).
    pub(crate) now_ms: i64,
}

impl Facts {
    /// Chunks that wait to be delivered: in the stage, readable or not yet read, and not delivered.
    fn waiting(&self) -> impl Iterator<Item = &Staged> {
        let delivered: HashSet<i64> = self.delivered.iter().map(|d| d.chunk).collect();
        self.staged
            .iter()
            .filter(move |c| matches!(c.state, State::Captured | State::Planned))
            .filter(move |c| !delivered.contains(&c.seq))
    }

    /// Whether the chunk's content is done but it is not recorded as delivered yet: it is in the stage,
    /// readable, and not among the delivered.
    pub(crate) fn owes_record(&self, chunk: i64) -> bool {
        self.staged
            .iter()
            .any(|c| c.seq == chunk && c.state == State::Planned)
            && self.delivered.iter().all(|d| d.chunk != chunk)
    }

    /// Announced chunks that are neither in the stage nor counted lost: they may still arrive.
    pub(crate) fn unresolved(&self) -> usize {
        let staged: HashSet<&str> = self.staged.iter().map(|c| c.msg_id.as_str()).collect();
        self.announced
            .iter()
            .filter(|id| !staged.contains(id.as_str()))
            .count()
    }

    /// Whether the push of `sync_type` is over (module docs).
    pub(crate) fn over(&self, sync_type: Option<i32>) -> bool {
        let last = self
            .delivered
            .iter()
            .any(|d| d.sync_type == sync_type && d.progress.is_some_and(|p| p >= 100));
        last && self
            .waiting()
            // A chunk of an unknown type could be of this one.
            .all(|w| w.sync_type.is_some() && w.sync_type != sync_type)
            && self.unresolved() == 0
    }

    /// Whether nothing of the account can still arrive and the push cannot be taken for running (module docs, F6):
    /// then every chat that owes an end gets it, whatever its sync type did.
    pub(crate) fn account_over(&self) -> bool {
        if self.waiting().next().is_some() || self.unresolved() > 0 {
            return false;
        }
        let Some(newest) = self.staged.iter().map(|c| c.captured_at).max() else {
            return false;
        };
        self.lost
            || self.staged.iter().any(|c| c.state == State::Bad)
            || newest.saturating_add(PUSH_QUIET_MS) <= self.now_ms
    }

    /// Every sync type whose push is over.
    pub(crate) fn over_types(&self) -> Vec<Option<i32>> {
        let mut types: Vec<Option<i32>> = Vec::new();
        for d in &self.delivered {
            if !types.contains(&d.sync_type) && self.over(d.sync_type) {
                types.push(d.sync_type);
            }
        }
        types
    }

    /// The chunk to deliver next: the planned one with the lowest rank of sync type, then the lowest
    /// `chunkOrder` (none last), then the oldest capture.
    pub(crate) fn next_chunk(&self) -> Option<i64> {
        self.waiting()
            .filter(|c| c.state == State::Planned)
            .min_by_key(|c| (rank(c.sync_type), c.order.unwrap_or(u32::MAX), c.seq))
            .map(|c| c.seq)
    }
}

#[cfg(test)]
mod tests;
