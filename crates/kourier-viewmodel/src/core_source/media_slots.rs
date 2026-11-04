//! The gallery slots of [`CoreSource`] (people pages P4): four places show the gallery of a chat
//! at a time ([`MediaSlot`]: the section of the group's page, the section of the person's page,
//! the grid and the list of the level of all media), each with its own chat, its own query
//! tracker, its rows and its follow-ups; a fifth, the picture viewer's, reads the album of ONE
//! message (the statement's cursor AT that message, `VIEWER_ASK` rows, only the message's own
//! kept) and follows that message, so a viewer opened from a section's square shows the whole
//! album and closes only when the message really lost its pictures, not when the section's six
//! squares moved on. A slot asks the core for a page of its kind
//! (`Query::ChatMedia`), shows the first rows, and goes on with the next page from the last
//! row's cursor when the front end asks ("more"); a change of its chat (`MessagesChanged`,
//! `TimelineInvalidated`) reads the rows again from the newest, as many as are loaded (up to what
//! one query can read: past that the rows after the window are dropped, so a deleted picture never
//! stays, and "more" loads them again). A change and a "more" that come while a query is in
//! flight are both served: the page asked for is sent after the fresh read. What the slot hands to
//! the front end is only what changed: an answer that equals what is on screen tells nothing (a
//! busy chat asks again with every message). A slot ends with its chat (removed or merged away)
//! and with its account (removed).

use std::collections::{HashMap, HashSet};

use kourier_core::facade::{
    ChatMediaQuery, MAX_MEDIA_PAGE, MediaCursor, Query, QueryData, UiCommand,
};
use kourier_model::{AccountId, AttachmentId, ConversationId, MessageId};

use super::{Changes, Completion, CoreSource, QueryToken, QueryTracker};
use crate::chat_media::{MediaRow, MediaSlot, MediaUpdate, PAGE, SECTION_TILES};
use crate::timeline::MediaState;

/// The query model id of slot `i`: this one and the next four.
pub const MODEL_CHAT_MEDIA: u32 = 11;

/// The slot a query result of `model` is for.
pub(super) fn slot_of_model(model: u32) -> Option<MediaSlot> {
    model
        .checked_sub(MODEL_CHAT_MEDIA)
        .and_then(|i| MediaSlot::ALL.get(i as usize))
        .copied()
}

/// What one query in flight was sent for.
#[derive(Clone, Copy)]
struct Asked {
    /// The chat the slot showed then (an answer for another chat is dropped).
    conv: ConversationId,
    /// A page after the last row (its rows are appended), else a read from the newest.
    appending: bool,
}

/// One slot.
pub(super) struct Slot {
    /// The chat shown; `None` while the slot is closed.
    conv: Option<ConversationId>,
    /// The account the chat belongs to (the slot ends with it).
    account: Option<AccountId>,
    /// The viewer's slot only: where its read starts, AT the message it shows (`idx: None`).
    anchor: Option<MediaCursor>,
    tracker: QueryTracker,
    /// Every row loaded, newest first (a section keeps the one more it asked for).
    rows: Vec<MediaRow>,
    /// More of the kind follow the last row.
    more: bool,
    /// The other kind has something.
    other: bool,
    /// A read from the newest is wanted (the slot was opened or its chat changed).
    fresh_wanted: bool,
    /// The page after the last row is wanted.
    more_wanted: bool,
    sent: HashMap<u64, Asked>,
    /// What was last handed to the front end: an equal answer changes nothing.
    handed: Option<MediaUpdate>,
    /// The live progress of the downloads of the rows (`Progress::Download`, 0 ... 1): ephemeral,
    /// kept while the row says it is downloading, laid over the row's own progress.
    live: HashMap<AttachmentId, f32>,
}

impl Slot {
    fn new(slot: MediaSlot) -> Self {
        Self {
            conv: None,
            account: None,
            anchor: None,
            tracker: QueryTracker::new(MODEL_CHAT_MEDIA + slot.index() as u32),
            rows: Vec::new(),
            more: false,
            other: false,
            fresh_wanted: false,
            more_wanted: false,
            sent: HashMap::new(),
            handed: None,
            live: HashMap::new(),
        }
    }

    /// Forgets the chat and everything of it.
    fn clear(&mut self) {
        self.conv = None;
        self.account = None;
        self.anchor = None;
        self.live.clear();
        self.rows.clear();
        self.more = false;
        self.other = false;
        self.fresh_wanted = false;
        self.more_wanted = false;
        self.sent.clear();
        self.handed = None;
        self.tracker.reset();
    }
}

/// The slots and the updates waiting for the front end.
pub(super) struct MediaSlots {
    slots: [Slot; MediaSlot::COUNT],
    /// The newest update of each slot not taken yet.
    answers: Vec<MediaUpdate>,
}

impl MediaSlots {
    pub(super) fn new() -> Self {
        Self {
            slots: MediaSlot::ALL.map(Slot::new),
            answers: Vec::new(),
        }
    }
}

impl CoreSource {
    fn slot(&self, slot: MediaSlot) -> &Slot {
        &self.media.slots[slot.index()]
    }

    fn slot_mut(&mut self, slot: MediaSlot) -> &mut Slot {
        &mut self.media.slots[slot.index()]
    }

    /// What the front end is told of the slot: a section's first rows (and "more" when one was
    /// left out), else every row loaded.
    fn media_update(&self, slot: MediaSlot) -> MediaUpdate {
        let s = self.slot(slot);
        // (A row that is downloading shows the live progress when it is further than the row's own.)
        let live = |r: &MediaRow| {
            let mut r = r.clone();
            if r.state == MediaState::Downloading
                && let Some(p) = s.live.get(&r.attachment)
            {
                r.progress = r.progress.max(*p);
            }
            r
        };
        let (rows, more) = if slot.is_section() {
            (
                s.rows.iter().take(SECTION_TILES).map(live).collect(),
                s.more || s.rows.len() > SECTION_TILES,
            )
        } else {
            (s.rows.iter().map(live).collect(), s.more)
        };
        MediaUpdate {
            slot,
            rows,
            more,
            other: s.other,
        }
    }

    /// Hands the slot's update over when it differs from the one the front end has.
    fn hand_media(&mut self, slot: MediaSlot, ch: &mut Changes) {
        // The live progress of a row that is not downloading (any more) is dropped.
        let s = self.slot_mut(slot);
        let rows = &s.rows;
        s.live.retain(|id, _| {
            rows.iter()
                .any(|r| r.attachment == *id && r.state == MediaState::Downloading)
        });
        let update = self.media_update(slot);
        let s = self.slot_mut(slot);
        if s.handed.as_ref() == Some(&update) {
            return;
        }
        s.handed = Some(update.clone());
        self.media.answers.retain(|u| u.slot != slot);
        self.media.answers.push(update);
        ch.chat_media = true;
    }

    /// Shows the gallery of `conv` (a chat of `account`) in `slot`. The same chat again only reads
    /// it again.
    pub(super) fn open_media_slot(
        &mut self,
        slot: MediaSlot,
        account: AccountId,
        conv: ConversationId,
    ) -> bool {
        if slot == MediaSlot::Viewer {
            // (The viewer's read starts at a message: `open_media_viewer_slot`.)
            return false;
        }
        let s = self.slot_mut(slot);
        if s.conv != Some(conv) {
            s.clear();
            s.conv = Some(conv);
        }
        s.account = Some(account);
        s.fresh_wanted = true;
        self.pump_media(slot);
        !self.stopped
    }

    /// Opens the viewer's slot for `message`, one of the rows of the slot `from`: the chat, the
    /// account and the message's place are the ones that row has. Whatever the viewer showed
    /// before is dropped, with the answers still on their way.
    pub(super) fn open_media_viewer_slot(&mut self, from: MediaSlot, message: MessageId) -> bool {
        if from == MediaSlot::Viewer {
            return false;
        }
        let found = {
            let s = self.slot(from);
            match (
                s.conv,
                s.account,
                s.rows.iter().find(|r| r.message == message),
            ) {
                (Some(conv), Some(account), Some(row)) => Some((conv, account, row.sort_key)),
                _ => None,
            }
        };
        let Some((conv, account, sort_key)) = found else {
            return false;
        };
        self.close_media_slot(MediaSlot::Viewer);
        let v = self.slot_mut(MediaSlot::Viewer);
        v.conv = Some(conv);
        v.account = Some(account);
        v.anchor = Some(MediaCursor {
            sort_key,
            message,
            idx: None,
        });
        v.fresh_wanted = true;
        self.pump_media(MediaSlot::Viewer);
        !self.stopped
    }

    /// The slot's next page after its last row (only the level's grid and list go on).
    pub(super) fn more_media_slot(&mut self, slot: MediaSlot) {
        let s = self.slot_mut(slot);
        if !slot.goes_on() || s.conv.is_none() || !s.more || s.rows.is_empty() {
            return;
        }
        s.more_wanted = true;
        self.pump_media(slot);
    }

    /// The slot shows nothing any more and asks about nothing.
    pub(super) fn close_media_slot(&mut self, slot: MediaSlot) {
        self.slot_mut(slot).clear();
        self.media.answers.retain(|u| u.slot != slot);
    }

    /// The slots' updates not taken yet.
    pub(super) fn take_media_updates(&mut self) -> Vec<MediaUpdate> {
        std::mem::take(&mut self.media.answers)
    }

    /// Asks the core, or remembers to once the question in flight is answered.
    fn pump_media(&mut self, slot: MediaSlot) {
        let t = self.slot_mut(slot).tracker.request();
        self.send_media(slot, t);
    }

    /// Sends the slot's wanted query with `token`: a read from the newest first (as many rows
    /// as are loaded, at least the slot's first page), else the page after the last row.
    fn send_media(&mut self, slot: MediaSlot, token: Option<QueryToken>) {
        let Some(token) = token else { return };
        if self.slot(slot).conv.is_some_and(|c| self.gallery_hidden(c)) {
            // A protected chat's pictures and files show only inside the open chat (S-P): the
            // page of its person opened from a group shows no section. Nothing is asked; the
            // slot keeps its chat (and the viewer's anchor), so a change of the chat or of the
            // list ([`Self::rerun_media_slots`]) asks again and the open chat shows them.
            let s = self.slot_mut(slot);
            let (conv, account, anchor) = (s.conv, s.account, s.anchor);
            s.clear();
            s.conv = conv;
            s.account = account;
            s.anchor = anchor;
            self.media.answers.retain(|u| u.slot != slot);
            self.media.answers.push(MediaUpdate {
                slot,
                rows: Vec::new(),
                more: false,
                other: false,
            });
            return;
        }
        let s = self.slot_mut(slot);
        let Some(conv) = s.conv else {
            s.tracker.reset();
            return;
        };
        let (before, limit, appending) = if s.fresh_wanted {
            s.fresh_wanted = false;
            let limit = if slot.goes_on() {
                u32::try_from(s.rows.len())
                    .unwrap_or(u32::MAX)
                    .clamp(PAGE, MAX_MEDIA_PAGE)
            } else {
                slot.first_page()
            };
            // (The viewer's read starts AT its message; every other one from the newest.)
            (s.anchor, limit, false)
        } else if s.more_wanted
            && let Some(last) = s.rows.last()
        {
            let at = last.cursor();
            s.more_wanted = false;
            (Some(at), PAGE, true)
        } else {
            // Nothing is asked any more: the tracker must not wait for this token.
            s.more_wanted = false;
            s.tracker.reset();
            return;
        };
        s.sent.insert(token.generation, Asked { conv, appending });
        // A page asked for while this read was wanted is still wanted: the tracker would have no
        // follow-up for it (the request that came during the flight only marked it dirty, and
        // this token used it up), so mark it again; the answer's follow-up sends the page.
        if s.more_wanted {
            let _ = s.tracker.request();
        }
        self.send(UiCommand::Query {
            token,
            query: Query::ChatMedia(ChatMediaQuery {
                conv,
                kind: slot.kind(),
                before,
                limit,
            }),
        });
    }

    /// The chat changed (a message, a download, a deletion): every slot that shows it reads again.
    pub(super) fn refresh_media_of(&mut self, conv: ConversationId) {
        for slot in MediaSlot::ALL {
            if self.slot(slot).conv == Some(conv) {
                self.slot_mut(slot).fresh_wanted = true;
                self.pump_media(slot);
            }
        }
    }

    /// Whether the gallery of `conv` is withheld: outside the open chat a chat shows its pictures
    /// and file names only when the list says it is NOT protected. The rule FAILS CLOSED: a
    /// chat the list snapshot does not hold (not read yet, or hidden) counts as protected;
    /// [`Self::rerun_media_slots`] asks again when a list read changes the answer.
    fn gallery_hidden(&self, conv: ConversationId) -> bool {
        self.open != Some(conv) && !self.list.iter().any(|c| c.id == conv && !c.protected)
    }

    /// The slots whose chat's gallery is withheld now (taken before a list read replaces the list).
    pub(super) fn hidden_media_slots(&self) -> Vec<MediaSlot> {
        MediaSlot::ALL
            .into_iter()
            .filter(|s| self.slot(*s).conv.is_some_and(|c| self.gallery_hidden(c)))
            .collect()
    }

    /// A list read replaced the list: every slot whose chat's gallery was withheld and is not
    /// any more, or the other way round (the chat turned protected, was listed, left the list),
    /// runs again, so what a slot shows follows the list. `was_hidden` is
    /// [`Self::hidden_media_slots`] from before the read. Returns whether any slot ran again
    /// (an update may wait for the front end).
    pub(super) fn rerun_media_slots(&mut self, was_hidden: &[MediaSlot]) -> bool {
        let mut ran = false;
        for slot in MediaSlot::ALL {
            let Some(conv) = self.slot(slot).conv else {
                continue;
            };
            if self.gallery_hidden(conv) != was_hidden.contains(&slot) {
                self.slot_mut(slot).fresh_wanted = true;
                self.pump_media(slot);
                ran = true;
            }
        }
        ran
    }

    /// The chat was merged into another one: the slots that showed it show the survivor.
    pub(super) fn merge_media_chat(&mut self, from: ConversationId, into: ConversationId) {
        for slot in MediaSlot::ALL {
            if self.slot(slot).conv == Some(from) {
                self.slot_mut(slot).conv = Some(into);
            }
        }
        self.refresh_media_of(into);
    }

    /// The slot ends: it closes and is told to show nothing.
    fn end_media_slot(&mut self, slot: MediaSlot) {
        self.close_media_slot(slot);
        self.media.answers.push(MediaUpdate {
            slot,
            rows: Vec::new(),
            more: false,
            other: false,
        });
    }

    /// Chats were removed: the slots that showed one end (nothing to show).
    pub(super) fn end_media_of(&mut self, ids: &[ConversationId]) -> bool {
        let mut ended = false;
        for slot in MediaSlot::ALL {
            if self.slot(slot).conv.is_some_and(|c| ids.contains(&c)) {
                self.end_media_slot(slot);
                ended = true;
            }
        }
        ended
    }

    /// An account was removed: the slots that showed one of its chats end, as the person's and the
    /// group's pages do (one origin rule: what belongs to an account goes with it).
    pub(super) fn end_media_of_account(&mut self, account: AccountId) -> bool {
        let mut ended = false;
        for slot in MediaSlot::ALL {
            if self.slot(slot).account == Some(account) {
                self.end_media_slot(slot);
                ended = true;
            }
        }
        ended
    }

    /// Live progress of a download (`Progress::Download`): the rows of the slots that are
    /// downloading it show it when the shown percentage changes (the rows are read again at the
    /// state's changes only, so without this a square would say "0%" until the file is here).
    pub(super) fn media_download_progress(
        &mut self,
        attachment: AttachmentId,
        done: u64,
        total: u64,
    ) -> Changes {
        let mut ch = Changes::default();
        if total == 0 {
            return ch;
        }
        let p = (done as f64 / total as f64).clamp(0.0, 1.0) as f32;
        for slot in MediaSlot::ALL {
            let s = self.slot_mut(slot);
            if !s
                .rows
                .iter()
                .any(|r| r.attachment == attachment && r.state == MediaState::Downloading)
            {
                continue;
            }
            let old = s.live.insert(attachment, p).unwrap_or(0.0);
            if (p * 100.0).floor() != (old * 100.0).floor() {
                self.hand_media(slot, &mut ch);
            }
        }
        ch
    }

    /// The answer of a gallery query for `slot`.
    pub(super) fn on_media_result(
        &mut self,
        slot: MediaSlot,
        token: QueryToken,
        result: &Result<QueryData, std::sync::Arc<impl std::fmt::Display>>,
    ) -> Changes {
        let mut ch = Changes::default();
        let asked = self.slot_mut(slot).sent.remove(&token.generation);
        let applies = self.slot_mut(slot).tracker.complete(token) == Completion::Apply;
        if applies
            && let Some(asked) = asked
            && self.slot(slot).conv == Some(asked.conv)
        {
            match result {
                Ok(QueryData::ChatMedia(page)) => {
                    let mut fresh = MediaRow::rows(&page.items, slot.kind());
                    let s = self.slot_mut(slot);
                    if let (MediaSlot::Viewer, Some(anchor)) = (slot, s.anchor) {
                        // The read goes on into the older messages: only the message's own
                        // attachments are the viewer's album.
                        fresh.retain(|r| r.message == anchor.message);
                        s.rows = fresh;
                        s.more = false;
                        s.other = false;
                    } else if asked.appending {
                        let known: HashSet<_> = s.rows.iter().map(MediaRow::key).collect();
                        s.rows
                            .extend(fresh.into_iter().filter(|r| !known.contains(&r.key())));
                        s.more = page.more;
                    } else {
                        // A read of the first rows replaces everything loaded: past what one query
                        // reads (`MAX_MEDIA_PAGE`) the rows after its window go too, so a picture
                        // deleted there never stays, and "more" loads them again.
                        s.more = page.more;
                        s.rows = fresh;
                        s.other = page.other;
                    }
                    self.hand_media(slot, &mut ch);
                }
                // The query failed (the core is going away): nothing to show.
                _ => {
                    let s = self.slot_mut(slot);
                    s.rows.clear();
                    s.more = false;
                    s.other = false;
                    self.hand_media(slot, &mut ch);
                }
            }
        }
        let t = self.slot_mut(slot).tracker.follow_up();
        self.send_media(slot, t);
        ch
    }
}
