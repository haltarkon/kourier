//! The message machine (TG-3 C6a, C6b; brief §3.3, §3.5, §3.6, critic B1, H2, L5, L7, M1, L10,
//! decisions G-8, Q-L2, Q-L7, L-11).
//!
//! PURE like the chat machine: the actor (C7a/C7b) hands it each message update with the chat
//! machine and its clock; it hands back the rows, facts and gaps of ONE [`Staged`] batch and the
//! requests to send ([`Out`]).
//!
//! - ONE ROW per `updateNewMessage`: the author's contact patch (gated), the chat's conversation
//!   patch ([`Chats::row_upsert`]: restated with the chat's first row of the run), then the
//!   `Message`; the row's [`Delivered`] moves the cursor in the same commit. `local` is the full
//!   message id in decimal (`MsgIdScope::Conversation`), so a re-delivery (RC3: at most once more,
//!   only through the difference) is the same row and the core's idempotent ingest drops it.
//! - THE LINK HOLD: nothing is staged before the chat machine is linked (the fresh-link load picks
//!   those messages up as history, critic H1).
//! - DROPPED (L5): a scheduled message (`scheduling_state`), one being sent from this client
//!   (`sending_state`) and every id whose low 20 bits are not zero (TDLib's local, yet-unsent and
//!   scheduled id forms: a server message's id is `server_id << 20`). Messages of a held, undecided
//!   or secret chat are no rows; a message of a chat the machine never learnt waits (bounded) for
//!   the `getChat` it asks ([`Messages::on_chat`]).
//! - CATCH-UP (H2, Q-L7): `catch_up` is set for every message until the start sync ends
//!   ([`Messages::start_sync_done`]), while TDLib says `Updating` ([`Messages::set_updating`]), and
//!   for a message dated more than [`CATCH_UP_AGE_MS`] before the helper's start whatever the
//!   state (a supergroup's backlog streams while TDLib says Ready).
//! - THE SUPERGROUP TRIGGER (G-8, Q-L2): a supergroup has its own id sequence; a streamed message
//!   whose SERVER id is above the cursor's server id + 1 opens the gap `(cursor, id)` in the same
//!   batch as its row, whatever the connection state (after `channelDifferenceTooLong` TDLib streams
//!   only the newest message). No trigger while the cursor is 0 (the fresh-link rule covers that
//!   chat). A hole left by a deleted message costs the reconcile one request that finds nothing.
//! - THE COMMON-BOX CHECK (B1): private chats and basic groups share one id sequence, so their ids
//!   say nothing about a chat's continuity. In a helper run after an UNCLEAN end the first live
//!   message of such a chat opens the check gap `(verified_max, id)`; the reconcile (C9) pages it.
//! - `verified_max_id` follows each delivered message (a hole below it is a gap row from then on).
//! - Own messages (outgoing, or a call line one of our own devices said) are `OwnOtherDevice` with
//!   `initial_out_state: Sent` (L7: T3 sends nothing, so every own message is another device's),
//!   except in a PRIVATE chat at or below the peer's read position TDLib told
//!   (`last_read_outbox_message_id`): `Read` (Q-B4, live and history alike; the own chat and groups
//!   keep `Sent`).
//! - HISTORY ([`Messages::history`], C8): the fresh-link load's messages as `HistoryBatch` rows
//!   that move the cursor like live ones and never notify.
//! - THE FRESH-CHAT HOLD (TG-4 K5, critic H1, M5): a live row of a chat whose cursor `chat_state`
//!   held at the run's start is 0 never lands before that chat's fresh-link batch (its last message
//!   and unread page, one transaction: [`crate::sync`]). From the run's start
//!   ([`Messages::hold_fresh_chats`]) until the fresh-link load has run for it, such a chat's live
//!   rows are HELD in a store of their own (`held_rows`, never the waiting list that
//!   [`Messages::on_chat`] and [`Messages::release_decided`] empty); a chat with a page plan keeps
//!   them until the plan's batch ([`Messages::release_planned`]), a chat without one gets them
//!   right after its last message ([`Messages::end_lists_hold`]); a chat first seen after the
//!   fresh-link load is a normal live chat. Past [`WAITING_PER_CHAT`] the lost-ids rule applies
//!   (a snapshot gap from the lowest dropped id, staged in the output of the replay itself: U3). A held row is not yet durable: a clean stop
//!   with one is not clean ([`Messages::unsettled`]).
//! - THE RECONCILE'S ROWS ([`Messages::caught_up`], C9a, Q-D1): messages a reconcile page found
//!   inside a gap ABOVE the chat's delivered max are live rows with `catch_up: true` (decision
//!   G-8), in id order, without a gap trigger; those AT OR BELOW it (overtaken by a live row) are
//!   `HistoryBatch` rows (never notified, sorted by date).
//! - EDITS (M1): TDLib sends `updateMessageContent` THEN `updateMessageEdited` for a message it
//!   holds (`MessagesManager.cpp` `update_message`). The content is kept (at most [`EDIT_PAIRS`]) and
//!   the `Edited` is made when its `updateMessageEdited` comes; a content update alone (a link
//!   preview, a poll's results) and an `updateMessageEdited` without one (buttons, the date) are no
//!   edit. The author comes from the delivered messages the machine remembers ([`Messages::note_delivered`],
//!   seeded by C7a from `delivered`); without one ONE `getMessage` asks it ([`Fetch::Message`]),
//!   answered into [`Messages::on_message`]; a failed one drops the edit. An edit of a message
//!   TDLib no longer holds reaches the app as nothing (G-8, RC2/RC7): no fetch is made for it.
//! - DELETES (G-8, his "Keep them; KT-2 first"): `updateDeleteMessages` with `is_permanent` and not
//!   `from_cache` is PARKED (`deferred`) with every server id TDLib named, delivered or not; KT-2
//!   maps them. A delete is never a row. A cache delete or an inaccessible message is ignored, and
//!   so is a delete in a chat that is not mapped (held chats have no rows).
//! - READS: `updateChatReadInbox` (and a chat's `last_read_inbox_message_id` when TDLib tells the chat
//!   again) -> `ReadMarker` on the NEWEST DELIVERED id at or below the read position (a marker for a
//!   message Kourier never had would be parked two days and dropped by the core), once per rise;
//!   `chat_state.read_inbox_max_id` is the HIGHER of TDLib's position and the position of a read
//!   Kourier's own act confirmed (TG-4 K4, D-2: [`Messages::confirm_read`], staged in the commit of
//!   the command's confirm). At the lists' load [`Messages::lost_reads`] names the chats whose
//!   stored position is above what TDLib told IN THIS RUN: a read TDLib lost to a kill, sent
//!   again once per chat as ONE `viewMessages` of the highest INCOMING delivered id at or below
//!   the stored position, clamped to TDLib's told last message (the read re-assert, U2). The read
//!   act tells TDLib only INCOMING ids (TDLib takes the maximum of the ids it is told whatever
//!   their direction), so TDLib's echo, the server request and the stored position are all the
//!   act's highest incoming id; the compare still goes through the highest incoming id at or
//!   below the stored value, for a position a server stores on an outgoing id. L10: live rows sort by arrival and
//!   history rows by `sent_at`; a delivered message with a lower id sorts before the target unless it
//!   ARRIVED live after it (an out-of-order stream): such a row stays unread until the next marker.
//!   `updateChatReadOutbox` of a private chat -> `Receipts{Read}` from the peer for our own delivered
//!   messages in `(old, new]` (groups: KT-2's `PeerReadUpTo`; the own chat says nothing).
//! - TYPING (L-11): `updateChatAction` typing or cancel of a user in a mapped chat -> an ephemeral
//!   `Typing` ([`TYPING_TTL`]); our own ids, held chats, other actions and everything before the link
//!   are dropped.
//!
//! Nothing here logs a value; `Debug` of the outputs prints ids, kinds and counts.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{BackendEvent, IncomingMessage, MAX_HISTORY_BATCH};
use kourier_model::{
    Body, ConvKey, MsgTarget, Origin, OutgoingState, ReceiptKind, ReplyTo, SystemEvent,
    TargetAuthor, UserId,
};

use super::chats::Chats;
use super::{Fetch, Out};
use crate::content;
use crate::markup::Me;
use crate::store::{
    ChatFacts, ChatKind, DELIVERED_KEEP_PER_CHAT, Delivered, Event, GapCause, GapChange, Parked,
    Row,
};
use crate::tdapi::{
    self, ChatAction, ChatActionUpdate, DeleteMessages, Message, MessageContent, MessageEdited,
    MessageSender, Raw, Receive,
};

/// A message dated more than this before the helper's start is catch-up whatever the state
/// (critic H2, decision Q-L7: 5 minutes).
pub(crate) const CATCH_UP_AGE_MS: i64 = 5 * 60 * 1000;

/// How many messages of a chat the machine never learnt wait for its `getChat` answer; past it
/// the lowest and highest dropped ids are remembered and a gap row covering them is opened with
/// the chat's next row (fix round TF3), so the reconcile pages them.
pub(crate) const WAITING_PER_CHAT: usize = 64;

/// How many `updateMessageContent`s wait for their `updateMessageEdited` (TDLib sends the pair
/// back to back; a content update alone is no edit and leaves the oldest out past this).
pub(crate) const EDIT_PAIRS: usize = 64;

/// How many sealed messages ([`Messages::sealed`]) the machine remembers.
const SEALED_KEEP: usize = 4096;

/// How long a `Typing` holds without a refresh. TDLib ends a chat action itself after 5.5 s
/// without one (`DialogActionManager::DIALOG_ACTION_TIMEOUT`) with a `chatActionCancel`; this is a
/// little longer, for a cancel that never comes.
pub(crate) const TYPING_TTL: Duration = Duration::from_secs(6);

/// The bits of a TDLib message id below the server id (`id = server_id << 20` for a server message).
const LOCAL_BITS: i64 = (1 << 20) - 1;

/// A message id's server part.
pub(crate) fn server_id(id: i64) -> i64 {
    id >> 20
}

/// A server message's id (scheduled, local and yet-unsent ids have low bits set; L5).
pub(crate) fn is_server_id(id: i64) -> bool {
    id > 0 && id & LOCAL_BITS == 0
}

/// A chat's cursor as the machine keeps it (seeded from `chat_state` at the start, C7a).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Cursor {
    /// `delivered_max_id`.
    pub(crate) delivered_max: i64,
    /// `verified_max_id`.
    pub(crate) verified_max: i64,
}

/// A chat's read positions as the machine keeps them.
#[derive(Debug, Clone, Copy, Default)]
struct Reads {
    /// TDLib's `last_read_inbox_message_id`, the highest it told (seeded from the store: TDLib's
    /// position of an earlier run, or Kourier's confirmed read, whichever is higher).
    told_in: i64,
    /// The highest inbox position TDLib told IN THIS RUN (`None`: it told none yet). Never
    /// seeded: the read re-assert compares the stored position with it (TG-4 K4, D-2).
    run_told_in: Option<i64>,
    /// `chat_state.read_inbox_max_id` (seeded, then staged): the higher of TDLib's told position
    /// and the position of a read Kourier's act confirmed (D-2).
    stored_in: i64,
    /// The newest id a `ReadMarker` targeted (the stored position at the start).
    marked_in: i64,
    /// TDLib's `last_read_outbox_message_id`, the highest it told.
    told_out: i64,
    /// `chat_state.read_outbox_max_id` (seeded, then staged).
    stored_out: i64,
}

/// A delivered message the machine remembers: its author (`tg:<id>`) and direction.
type Sent = (String, bool);

/// The message machine of one helper run (see the module doc).
#[derive(Debug)]
pub(crate) struct Messages {
    /// The helper's start (ms), for the catch-up date rule.
    start_ms: i64,
    /// The previous helper run did not end cleanly (B1's common-box check).
    after_unclean: bool,
    /// The start sync has not ended yet: every message is catch-up (H2).
    syncing: bool,
    /// TDLib says `Updating`.
    updating: bool,
    cursors: HashMap<i64, Cursor>,
    /// The cursors as `chat_state` held them at the run's start (C8: a chat seeded with 0, or not
    /// at all, is a FRESH link's chat).
    stored: HashMap<i64, i64>,
    /// Chats whose first live message of this run passed (B1's check is made once per chat).
    seen: HashSet<i64>,
    /// Messages of chats asked with `getChat`, in arrival order.
    waiting: HashMap<i64, Vec<Message>>,
    /// The fresh-chat hold (TG-4 K5, H1): the run's lists are not loaded and their fresh-link
    /// load has not run, so every chat with a stored cursor of 0 holds its live rows.
    lists_hold: bool,
    /// Chats with an open fresh-link page plan: their live rows are held until the plan's batch.
    planned: HashSet<i64>,
    /// The held live rows per chat, in arrival order (M5: apart from `waiting`, which
    /// [`Messages::on_chat`] and [`Messages::release_decided`] replay).
    held_rows: HashMap<i64, Vec<Message>>,
    /// Chats whose lost read was already handed out for re-sending (once per chat per run).
    reasserted: HashSet<i64>,
    /// Messages dropped (L5, held chats, the link hold, no author): counted, never logged.
    dropped: usize,
    /// Per chat the lowest and highest id of the messages dropped from its waiting list (full, or
    /// its `getChat` failed): the chat's next row opens a gap over them (TF3).
    lost: HashMap<i64, (i64, i64)>,
    /// The delivered messages per chat (the newest [`DELIVERED_KEEP_PER_CHAT`]), as `delivered`
    /// holds them: the read marker's target, the receipts' own messages, an edit's author.
    delivered: HashMap<i64, BTreeMap<i64, Sent>>,
    reads: HashMap<i64, Reads>,
    /// `updateMessageContent`s waiting for their `updateMessageEdited`, oldest first.
    contents: VecDeque<((i64, i64), Raw)>,
    /// Edits asked with `getMessage`: their `edit_date`.
    asked: HashMap<(i64, i64), i32>,
    /// Delivered messages an edit must not touch (TF14): view once, self-destructing, restricted.
    /// Bounded to the newest [`SEALED_KEEP`].
    sealed: BTreeSet<(i64, i64)>,
}

impl Messages {
    /// A machine for a helper run that started at `start_ms`; `after_unclean`: the previous run did
    /// not end cleanly (`store::start_run` said `Unclean`).
    pub(crate) fn new(start_ms: i64, after_unclean: bool) -> Self {
        Self {
            start_ms,
            after_unclean,
            syncing: true,
            updating: false,
            cursors: HashMap::new(),
            stored: HashMap::new(),
            seen: HashSet::new(),
            waiting: HashMap::new(),
            lists_hold: false,
            planned: HashSet::new(),
            held_rows: HashMap::new(),
            reasserted: HashSet::new(),
            dropped: 0,
            lost: HashMap::new(),
            delivered: HashMap::new(),
            reads: HashMap::new(),
            contents: VecDeque::new(),
            asked: HashMap::new(),
            sealed: BTreeSet::new(),
        }
    }

    /// The chat's stored read positions from `chat_state` (C7a, at the start).
    pub(crate) fn seed_read(
        &mut self,
        chat_id: i64,
        read_inbox_max_id: i64,
        read_outbox_max_id: i64,
    ) {
        let r = self.reads.entry(chat_id).or_default();
        r.stored_in = r.stored_in.max(read_inbox_max_id);
        r.marked_in = r.marked_in.max(read_inbox_max_id);
        r.told_in = r.told_in.max(read_inbox_max_id);
        r.stored_out = r.stored_out.max(read_outbox_max_id);
        r.told_out = r.told_out.max(read_outbox_max_id);
    }

    /// A message Kourier has a row of: seeded by C7a from `delivered`, and by the history paths
    /// (C8, C9) for each history row they commit. A live row notes itself.
    pub(crate) fn note_delivered(&mut self, d: &Delivered) {
        let kept = self.delivered.entry(d.chat_id).or_default();
        kept.insert(d.msg_id, (d.author.clone(), d.outgoing));
        let keep = usize::try_from(DELIVERED_KEEP_PER_CHAT).unwrap_or(usize::MAX);
        while kept.len() > keep {
            kept.pop_first();
        }
    }

    /// Remembers that edits of `(chat_id, id)` are dropped (TF14).
    fn seal(&mut self, chat_id: i64, id: i64) {
        self.sealed.insert((chat_id, id));
        while self.sealed.len() > SEALED_KEEP {
            self.sealed.pop_first();
        }
    }

    /// The author and direction of a delivered message the machine remembers.
    pub(crate) fn delivered(&self, chat_id: i64, msg_id: i64) -> Option<&Sent> {
        self.delivered.get(&chat_id)?.get(&msg_id)
    }

    /// The chat's cursor from `chat_state` (C7a, at the start).
    pub(crate) fn seed(&mut self, chat_id: i64, cursor: Cursor) {
        self.cursors.insert(chat_id, cursor);
        self.stored.insert(chat_id, cursor.delivered_max);
    }

    /// The chat's cursor as `chat_state` held it at the run's start (0: none; C8's fresh-link
    /// rule).
    pub(crate) fn stored_cursor(&self, chat_id: i64) -> i64 {
        self.stored.get(&chat_id).copied().unwrap_or(0)
    }

    /// The chat's cursor as the machine has moved it.
    pub(crate) fn cursor(&self, chat_id: i64) -> Cursor {
        self.cursors.get(&chat_id).copied().unwrap_or_default()
    }

    /// The start sync ended (C8): from now on only the date rule and `Updating` make catch-up.
    pub(crate) fn start_sync_done(&mut self) {
        self.syncing = false;
    }

    /// TDLib's connection state is (`true`) or is not `Updating`.
    pub(crate) fn set_updating(&mut self, updating: bool) {
        self.updating = updating;
    }

    /// Messages left out so far (tests only).
    #[cfg(test)]
    pub(crate) fn dropped(&self) -> usize {
        self.dropped
    }

    /// One receive update, AFTER the chat machine took it ([`Chats::on_update`]): the message,
    /// edit, delete, read and typing updates give rows, parked facts, requests or ephemeral events;
    /// every other update nothing.
    pub(crate) fn on_update(&mut self, chats: &mut Chats, update: &Receive, now_ms: i64) -> Out {
        match update {
            Receive::NewMessage(msg) => self.on_new_message(chats, msg, now_ms),
            Receive::MessageContent(u) => {
                self.on_content(u);
                Out::default()
            }
            Receive::MessageEdited(u) => self.on_edited(chats, *u),
            Receive::DeleteMessages(u) => self.on_delete(chats, u),
            Receive::NewChat(chat) => {
                self.told(
                    chat.id,
                    Some(chat.last_read_inbox_message_id),
                    chat.last_read_outbox_message_id,
                );
                self.mark_read(chats, chat.id, now_ms)
            }
            Receive::ChatReadInbox(u) => {
                self.told(u.chat_id, Some(u.last_read_inbox_message_id), 0);
                self.mark_read(chats, u.chat_id, now_ms)
            }
            Receive::ChatReadOutbox(u) => {
                self.told(u.chat_id, None, u.last_read_outbox_message_id);
                self.mark_read(chats, u.chat_id, now_ms)
            }
            Receive::ChatAction(u) => on_action(chats, u),
            _ => Out::default(),
        }
    }

    /// TDLib told read positions (`None` for the inbox: says nothing of that side; 0 for the
    /// outbox likewise).
    fn told(&mut self, chat_id: i64, inbox: Option<i64>, outbox: i64) {
        let r = self.reads.entry(chat_id).or_default();
        if let Some(inbox) = inbox {
            r.told_in = r.told_in.max(inbox);
            r.run_told_in = Some(r.run_told_in.unwrap_or(0).max(inbox));
        }
        r.told_out = r.told_out.max(outbox);
    }

    /// The read re-assert (TG-4 K4, D-2; U2): `(chat, id)` for every listed, mapped, not held
    /// chat whose stored read position is ABOVE what TDLib told in this run, i.e. a read
    /// Kourier's act confirmed (or TDLib echoed) that TDLib lost to a kill: with its message
    /// database off it writes no `ReadHistoryOnServerLogEvent`, so nothing in TDLib ever sends
    /// that request again. `id` is the highest INCOMING delivered id at or below the stored
    /// position (the stored value can name an outgoing message: a server that stores a read on
    /// our own message, `rec-read-act` reads only incoming ones), CLAMPED to TDLib's told last
    /// message of the chat in this run when it told one (a top the peer deleted afterwards would
    /// otherwise repeat a request TDLib ignores at every start, durability F9: TDLib counts an id
    /// it does not hold only at or below its last, `MessagesManager.cpp:16786-16791`); the chat
    /// is named only when THAT id is above what TDLib told in this run. ONCE per chat per run (a
    /// second call names no chat twice); a chat TDLib told nothing of in this run is not asked, nor
    /// one with no incoming delivered id at or below the position. Called at the lists' load, in
    /// the start sync's list order.
    pub(crate) fn lost_reads(&mut self, chats: &Chats) -> Vec<(i64, i64)> {
        let mut lost = Vec::new();
        for chat_id in chats.listed_in_order() {
            let Some(r) = self.reads.get(&chat_id) else {
                continue;
            };
            let Some(told) = r.run_told_in else {
                continue;
            };
            let incoming = self.delivered.get(&chat_id).and_then(|kept| {
                kept.range(..=r.stored_in)
                    .rev()
                    .find(|(_, (_, outgoing))| !outgoing)
                    .map(|(&id, _)| id)
            });
            let Some(mut id) = incoming else {
                continue;
            };
            if let Some(last) = chats
                .chat(chat_id)
                .and_then(|c| c.last_message.as_deref())
                .map(|m| m.id)
                .filter(|&last| is_server_id(last))
            {
                id = id.min(last);
            }
            if id > told && self.reasserted.insert(chat_id) {
                lost.push((chat_id, id));
            }
        }
        lost
    }

    /// A read act of `chat_id` was confirmed by TDLib's `ok` up to its highest incoming id
    /// `up_to`: the fact that raises the stored read position, in the commit of the command's
    /// `CommandPersisted` (the session stages both), and `None` when the position is already
    /// there (TDLib's echo came first and [`Messages::mark_read`] staged it: one fact, never
    /// two).
    pub(crate) fn confirm_read(&mut self, chat_id: i64, up_to: i64) -> Option<ChatFacts> {
        let r = self.reads.entry(chat_id).or_default();
        if up_to <= r.stored_in {
            return None;
        }
        r.stored_in = up_to;
        Some(ChatFacts {
            chat_id,
            read_inbox_max_id: Some(up_to),
            ..ChatFacts::default()
        })
    }

    /// The read rule for one chat (module doc): the read facts, the `ReadMarker` on the newest
    /// delivered id at or below TDLib's read position when it rose, the receipts of a private
    /// chat. The history paths (C8, C9) call it after committing a chat's history rows.
    pub(crate) fn mark_read(&mut self, chats: &Chats, chat_id: i64, now_ms: i64) -> Out {
        let mut out = Out::default();
        if !chats.is_linked() {
            return out;
        }
        let Some(conv) = chats.conv(chat_id) else {
            return out;
        };
        let r = self.reads.entry(chat_id).or_default();
        let mut facts = ChatFacts {
            chat_id,
            ..ChatFacts::default()
        };
        let mut events = Vec::new();
        if r.told_in > r.stored_in {
            facts.read_inbox_max_id = Some(r.told_in);
            r.stored_in = r.told_in;
        }
        let kept = self.delivered.get(&chat_id);
        let target = kept.and_then(|k| k.range(..=r.told_in).next_back());
        if let Some((&id, (author, outgoing))) = target
            && id > r.marked_in
        {
            r.marked_in = id;
            let up_to = target_of(&conv, id, author, *outgoing);
            events.push(BackendEvent::ReadMarker {
                conv: Some(conv.clone()),
                up_to,
            });
        }
        if r.told_out > r.stored_out {
            let old = std::mem::replace(&mut r.stored_out, r.told_out);
            facts.read_outbox_max_id = Some(r.told_out);
            let peer = match &conv {
                ConvKey::Direct(peer) => Some(peer.clone()),
                _ => None,
            };
            let own_chat = chats.me().is_some_and(|me| me == chat_id);
            if let Some(by) = peer.filter(|_| !own_chat) {
                let targets: Vec<MsgTarget> = kept
                    .into_iter()
                    .flat_map(|k| k.range(old.saturating_add(1)..=r.told_out))
                    .filter(|(_, (_, outgoing))| *outgoing)
                    .map(|(&id, _)| MsgTarget {
                        conv: Some(conv.clone()),
                        author: TargetAuthor::Own,
                        local: Arc::from(id.to_string()),
                        alternates: Vec::new(),
                    })
                    .collect();
                if !targets.is_empty() {
                    events.push(BackendEvent::Receipts {
                        targets,
                        by,
                        kind: ReceiptKind::Read,
                        at: now_ms,
                    });
                }
            }
        }
        if facts.read_inbox_max_id.is_some() || facts.read_outbox_max_id.is_some() {
            out.staged.chats.push(facts);
        }
        for event in events {
            if event.validate_ids().is_ok() {
                out.staged.rows.push(Row {
                    events: vec![Event { event, gate: None }],
                    delivered: Vec::new(),
                });
            }
        }
        out
    }

    /// `updateMessageContent`: kept for the `updateMessageEdited` that follows (M1).
    fn on_content(&mut self, u: &MessageContent) {
        if !is_server_id(u.message_id) {
            return;
        }
        let key = (u.chat_id, u.message_id);
        self.contents.retain(|(k, _)| *k != key);
        self.contents.push_back((key, u.new_content.clone()));
        while self.contents.len() > EDIT_PAIRS {
            self.contents.pop_front();
        }
    }

    /// `updateMessageEdited`: the `Edited` of the content kept for it (M1), or the `getMessage`
    /// that asks the author of a message the machine does not remember.
    fn on_edited(&mut self, chats: &Chats, u: MessageEdited) -> Out {
        let mut out = Out::default();
        let key = (u.chat_id, u.message_id);
        let Some(pos) = self.contents.iter().position(|(k, _)| *k == key) else {
            // No content change: no edit.
            return out;
        };
        let raw = self.contents.remove(pos).map(|(_, raw)| raw);
        if self.sealed.contains(&key) {
            // View once, self-destructing or restricted (TF14): the hint stays, no pointer.
            return out;
        }
        let (Some(raw), true, Some(conv), Some(me)) =
            (raw, chats.is_linked(), chats.conv(u.chat_id), chats.me())
        else {
            return out;
        };
        let Some((author, outgoing)) = self.delivered(u.chat_id, u.message_id).cloned() else {
            self.asked.insert(key, u.edit_date);
            out.fetch.push(Fetch::Message {
                chat_id: u.chat_id,
                msg_id: u.message_id,
            });
            return out;
        };
        let by = (!outgoing).then(|| UserId::parse(&author).ok()).flatten();
        if !outgoing && by.is_none() {
            return out;
        }
        let usernames = chats.my_usernames();
        let body = content::content(
            &raw,
            Me {
                id: me,
                usernames: &usernames,
            },
        )
        .body;
        edited_row(&mut out, &conv, u.message_id, by, body, u.edit_date);
        out
    }

    /// The answer of a [`Fetch::Message`]: the edit with the message's author and its content.
    pub(crate) fn on_message(&mut self, chats: &Chats, msg: &Message, now_ms: i64) -> Out {
        let mut out = Out::default();
        let Some(edit_date) = self.asked.remove(&(msg.chat_id, msg.id)) else {
            return out;
        };
        let (Some(conv), Some(me)) = (chats.conv(msg.chat_id), chats.me()) else {
            return out;
        };
        let by = if msg.is_outgoing {
            None
        } else {
            let id = match msg.sender_id {
                MessageSender::User { user_id } if user_id != me => user_id,
                MessageSender::Chat { chat_id } if chat_id != 0 => chat_id,
                MessageSender::User { .. }
                | MessageSender::Chat { .. }
                | MessageSender::Unknown => {
                    return out;
                }
            };
            match UserId::from_parts("tg", &id.to_string()) {
                Ok(by) => Some(by),
                Err(_) => return out,
            }
        };
        let usernames = chats.my_usernames();
        // Through `content::message`, which knows view once and restrictions (TF14).
        let mapped = content::message(
            msg,
            Me {
                id: me,
                usernames: &usernames,
            },
            now_ms,
        );
        if mapped.sealed() {
            return out;
        }
        let body = mapped.body;
        edited_row(
            &mut out,
            &conv,
            msg.id,
            by,
            body,
            msg.edit_date.max(edit_date),
        );
        out
    }

    /// A [`Fetch::Message`] failed (TDLib no longer holds the message): the edit is dropped.
    pub(crate) fn on_message_failed(&mut self, chat_id: i64, msg_id: i64) {
        self.asked.remove(&(chat_id, msg_id));
    }

    /// `updateDeleteMessages`: a permanent delete of a mapped chat is parked (module doc).
    fn on_delete(&self, chats: &Chats, u: &DeleteMessages) -> Out {
        let mut out = Out::default();
        if u.from_cache || !u.is_permanent || !chats.is_linked() || chats.conv(u.chat_id).is_none()
        {
            return out;
        }
        let mut ids: Vec<i64> = u
            .message_ids
            .iter()
            .copied()
            .filter(|id| is_server_id(*id))
            .collect();
        ids.dedup();
        if !ids.is_empty() {
            out.staged.parked.push(Parked::Delete {
                chat_id: u.chat_id,
                ids,
            });
        }
        out
    }

    /// A `getChat` answer reached the chat machine (after [`Chats::on_chat`]): the chat's waiting
    /// messages, in arrival order.
    pub(crate) fn on_chat(&mut self, chats: &mut Chats, chat_id: i64, now_ms: i64) -> Out {
        let mut out = Out::default();
        for msg in self.waiting.remove(&chat_id).unwrap_or_default() {
            out.extend(self.on_new_message(chats, &msg, now_ms));
        }
        out
    }

    /// The waiting messages of every chat that is known and decided by now (TF4: a `getUser`,
    /// `getSupergroup` or `updateUser` answer decided it), replayed in arrival order. Cheap when
    /// nothing waits.
    pub(crate) fn release_decided(&mut self, chats: &mut Chats, now_ms: i64) -> Out {
        let mut out = Out::default();
        if self.waiting.is_empty() {
            return out;
        }
        let mut ready: Vec<i64> = self
            .waiting
            .keys()
            .copied()
            .filter(|id| chats.knows(*id) && !chats.deciding(*id))
            .collect();
        ready.sort_unstable();
        for chat_id in ready {
            out.extend(self.on_chat(chats, chat_id, now_ms));
        }
        out
    }

    /// A `getChat` failed: the chat's waiting messages are dropped (its cursor did not move) and
    /// their ids remembered: the chat's next row opens a gap over them (TF3), and a run that
    /// stops with such a memory is not clean ([`Messages::unsettled`]).
    pub(crate) fn on_chat_failed(&mut self, chat_id: i64) {
        if let Some(msgs) = self.waiting.remove(&chat_id) {
            self.dropped += msgs.len();
            for msg in &msgs {
                self.lose(chat_id, msg.id);
            }
        }
    }

    /// Messages wait for a chat's answer, ids were dropped that no row has covered with a gap yet
    /// (TF3), or live rows are held for a fresh chat's batch (TG-4 K5): a clean stop would hide
    /// them from the next start's checks.
    pub(crate) fn unsettled(&self) -> bool {
        !self.waiting.is_empty() || !self.lost.is_empty() || !self.held_rows.is_empty()
    }

    /// The fresh-chat hold begins (module doc): called with the run's machines, before any live
    /// row. Ended by [`Messages::end_lists_hold`].
    pub(crate) fn hold_fresh_chats(&mut self) {
        self.lists_hold = true;
    }

    /// `chat_id` has a fresh-link page plan: its live rows wait for the plan's batch, whatever the
    /// lists' hold says.
    pub(crate) fn hold_planned(&mut self, chat_id: i64) {
        self.planned.insert(chat_id);
    }

    /// The plan of `chat_id` delivered its batch: the live rows held for it follow, in arrival
    /// order, in this output (so in the batch's transaction).
    pub(crate) fn release_planned(&mut self, chats: &mut Chats, chat_id: i64, now_ms: i64) -> Out {
        self.planned.remove(&chat_id);
        self.replay_held(chats, chat_id, now_ms)
    }

    /// The fresh-link load has run for every listed chat: the lists' hold is over, and the live
    /// rows of every chat WITHOUT a page plan follow its last message (or, for a chat the load
    /// did not take, stand as live rows as before the hold existed). A planned chat keeps its
    /// rows until its plan's batch.
    pub(crate) fn end_lists_hold(&mut self, chats: &mut Chats, now_ms: i64) -> Out {
        self.lists_hold = false;
        let mut ready: Vec<i64> = self
            .held_rows
            .keys()
            .copied()
            .filter(|id| !self.planned.contains(id))
            .collect();
        ready.sort_unstable();
        let mut out = Out::default();
        for chat_id in ready {
            out.extend(self.replay_held(chats, chat_id, now_ms));
        }
        out
    }

    /// The live rows held for `chat_id`, replayed in arrival order, except the ones the batch
    /// delivered already (U4: no row reaches the core twice in a normal run). A replayed row below the
    /// chat's delivered max is a HISTORY row (TF2) and never reaches the lost-ids code of
    /// [`Messages::new_message`], so after the replay a chat whose rows were dropped past the
    /// hold's bound opens the snapshot gap over them here, in the same output (U3, durability
    /// F7: without it the gap lived only in memory and a run that ended first left a silent hole
    /// between delivered rows).
    fn replay_held(&mut self, chats: &mut Chats, chat_id: i64, now_ms: i64) -> Out {
        let mut out = Out::default();
        for msg in self.held_rows.remove(&chat_id).unwrap_or_default() {
            // U4 (durability F6): a row the batch just delivered (the chat's told last message,
            // or a message inside the page) is not shipped a second time as a live message.
            if self.delivered(chat_id, msg.id).is_some() {
                continue;
            }
            out.extend(self.on_new_message(chats, &msg, now_ms));
        }
        if chats.conv(chat_id).is_some() {
            out.staged.gaps.extend(self.lost_gap(chat_id));
        } else {
            // A chat held or secret by now has no rows and so no gap (the rule of `new_message`).
            self.lost.remove(&chat_id);
        }
        out
    }

    /// The snapshot gap over the ids dropped for `chat_id` while it had no row to carry the news
    /// (TF3: an unknown chat's full waiting list, U3: a full hold), consuming what was
    /// remembered: a chat never delivered (stored cursor 0) is covered from the lowest dropped
    /// id only, else from the stored cursor. `None` when nothing was dropped.
    fn lost_gap(&mut self, chat_id: i64) -> Option<GapChange> {
        let (lowest, highest) = self.lost.remove(&chat_id)?;
        let stored = self.stored_cursor(chat_id);
        let above = if stored > 0 { stored } else { lowest - 1 };
        (above < highest + 1).then_some(GapChange::Open {
            chat_id,
            above,
            below: highest + 1,
            cause: GapCause::Snapshot,
        })
    }

    /// A live row of `chat_id` must wait for its fresh-link batch (module doc).
    fn holds(&self, chat_id: i64) -> bool {
        self.planned.contains(&chat_id) || (self.lists_hold && self.stored_cursor(chat_id) == 0)
    }

    /// Remembers that message `id` of `chat_id` was dropped.
    fn lose(&mut self, chat_id: i64, id: i64) {
        let range = self.lost.entry(chat_id).or_insert((id, id));
        range.0 = range.0.min(id);
        range.1 = range.1.max(id);
    }

    /// `updateNewMessage`: one row (module doc).
    pub(crate) fn on_new_message(&mut self, chats: &mut Chats, msg: &Message, now_ms: i64) -> Out {
        self.new_message(chats, msg, now_ms, false)
    }

    /// Messages of `chat_id` the RECONCILE fetched inside a gap (C9a; decisions G-8, Q-D1): each
    /// one not delivered yet ABOVE the chat's delivered max at this moment is a live row with
    /// `catch_up: true` (summarised, not notified one by one), in id order, with NO gap trigger
    /// (the gap row is the reconcile's); one AT OR BELOW it (a live row overtook it) is a HISTORY
    /// row ([`Messages::history`]: never notified, sorted by its date, so it does not show below
    /// the newer row that overtook it; the core sorts a live row behind everything already in the
    /// chat). The read rule after them. The chat's first live message of the run has passed for
    /// B1's check (the reconcile covers what lay below it).
    pub(crate) fn caught_up(
        &mut self,
        chats: &mut Chats,
        chat_id: i64,
        msgs: &[Message],
        now_ms: i64,
    ) -> Out {
        let mut out = Out::default();
        let mut msgs: Vec<&Message> = msgs
            .iter()
            .filter(|m| m.chat_id == chat_id && self.delivered(chat_id, m.id).is_none())
            .collect();
        msgs.sort_unstable_by_key(|m| m.id);
        msgs.dedup_by_key(|m| m.id);
        if msgs.is_empty() {
            return out;
        }
        self.seen.insert(chat_id);
        // Q-D1: overtaken by a live row -> history; above the delivered max -> live catch-up.
        let delivered_max = self.cursor(chat_id).delivered_max;
        let (older, newer): (Vec<&Message>, Vec<&Message>) =
            msgs.into_iter().partition(|m| m.id <= delivered_max);
        if !older.is_empty() {
            let older: Vec<Message> = older.into_iter().cloned().collect();
            out.extend(self.history(chats, chat_id, &older, now_ms));
        }
        for msg in newer {
            out.extend(self.new_message(chats, msg, now_ms, true));
        }
        out.extend(self.mark_read(chats, chat_id, now_ms));
        out
    }

    /// One row of a streamed (`fetched: false`) or reconcile-fetched (`fetched: true`) message.
    fn new_message(&mut self, chats: &mut Chats, msg: &Message, now_ms: i64, fetched: bool) -> Out {
        let mut out = Out::default();
        let chat_id = msg.chat_id;
        if !chats.is_linked()
            || msg.scheduling_state.is_some()
            || msg.sending_state.is_some()
            || !is_server_id(msg.id)
        {
            self.dropped += 1;
            return out;
        }
        if fetched && (!chats.knows(chat_id) || chats.deciding(chat_id)) {
            self.dropped += 1;
            return out;
        }
        // An unknown chat, or a known one still waiting for its user or supergroup (TF4), holds
        // its messages until it is known and decided (`on_chat`, `release_decided`).
        if !chats.knows(chat_id) || chats.deciding(chat_id) {
            let waiting = self.waiting.entry(chat_id).or_default();
            if waiting.len() < WAITING_PER_CHAT {
                waiting.push(msg.clone());
            } else {
                self.dropped += 1;
                self.lose(chat_id, msg.id);
            }
            out.extend(chats.reference(chat_id));
            return out;
        }
        let Some(conv) = chats.conv(chat_id) else {
            // Held or secret (the chat is known and decided here): no row (H5, L4), so none will
            // ever open a gap over the ids dropped while it was unknown (TF3), and a run that
            // remembers them would never count as clean (D4 of the re-check).
            self.dropped += 1;
            self.lost.remove(&chat_id);
            return out;
        };
        // The fresh-chat hold (TG-4 K5, H1): a chat Kourier never delivered from keeps its live
        // rows out of every transaction before its fresh-link batch, so its cursor never moves
        // ahead of the unread page. (A fetched row belongs to a reconcile gap, which a chat with
        // no cursor has none of.)
        if !fetched && self.holds(chat_id) {
            let held = self.held_rows.entry(chat_id).or_default();
            if held.len() < WAITING_PER_CHAT {
                held.push(msg.clone());
            } else {
                self.dropped += 1;
                self.lose(chat_id, msg.id);
            }
            return out;
        }
        // TF2 (review dF2, Q-D1 on EVERY path): a STREAMED message not delivered yet whose id is
        // at or below the chat's delivered max (an earlier decision's row overtook it) is a
        // HISTORY row, like a reconcile page's: the core sorts a live row behind everything
        // already in the chat and notifies it, an older one must do neither. A re-delivery of
        // an id the machine holds stays what it was (the core drops the repeat).
        if !fetched
            && msg.id <= self.cursor(chat_id).delivered_max
            && self.delivered(chat_id, msg.id).is_none()
        {
            out.extend(self.history(chats, chat_id, std::slice::from_ref(msg), now_ms));
            out.extend(self.mark_read(chats, chat_id, now_ms));
            return out;
        }
        let Some(me) = chats.me() else {
            self.dropped += 1;
            return out;
        };
        let usernames = chats.my_usernames();
        let mapped = content::message(
            msg,
            Me {
                id: me,
                usernames: &usernames,
            },
            now_ms,
        );
        let Some(origin) = content::origin(msg, &mapped) else {
            self.dropped += 1;
            return out;
        };
        let read_out = self.read_by_peer(chats, msg);
        let sealed = mapped.sealed();
        let Some(mut message) = self.incoming(msg, conv, origin.clone(), mapped, read_out, now_ms)
        else {
            self.dropped += 1;
            return out;
        };
        if sealed {
            self.seal(chat_id, msg.id);
        }
        message.catch_up |= fetched;

        // The gap triggers, before the cursor moves (a fetched message is inside the
        // reconcile's gap: none).
        let cursor = self.cursor(chat_id);
        let first_live = self.seen.insert(chat_id);
        match chats.kind(chat_id).filter(|_| !fetched) {
            Some(ChatKind::Supergroup | ChatKind::Forum | ChatKind::Channel) => {
                if cursor.delivered_max > 0
                    && server_id(msg.id) > server_id(cursor.delivered_max) + 1
                {
                    out.staged.gaps.push(GapChange::Open {
                        chat_id,
                        above: cursor.delivered_max,
                        below: msg.id,
                        cause: GapCause::IdJump,
                    });
                }
            }
            Some(ChatKind::Direct | ChatKind::Group) => {
                if self.after_unclean
                    && first_live
                    && cursor.verified_max > 0
                    && msg.id > cursor.verified_max
                {
                    out.staged.gaps.push(GapChange::Open {
                        chat_id,
                        above: cursor.verified_max,
                        below: msg.id,
                        cause: GapCause::UncleanEnd,
                    });
                }
            }
            Some(ChatKind::Unknown) | None => {}
        }

        // Ids dropped while the chat was unknown (TF3): one gap over them, in this row's batch.
        out.staged.gaps.extend(self.lost_gap(chat_id));

        // The row: the author's contact, the chat's patch, the message.
        out.extend(chats.on_delivered(chat_id, msg.id, msg.is_outgoing));
        let mut events = Vec::new();
        if !msg.is_outgoing {
            let author = match msg.sender_id {
                MessageSender::User { user_id } if user_id != me => {
                    let upsert = chats.author_upsert(user_id);
                    if upsert.is_none() {
                        // Q-B1: an author TDLib told past the bound is asked once.
                        out.extend(chats.reference_user(user_id));
                    }
                    upsert
                }
                MessageSender::Chat { chat_id: sender } if sender != 0 => {
                    chats.sender_chat_upsert(sender)
                }
                MessageSender::User { .. }
                | MessageSender::Chat { .. }
                | MessageSender::Unknown => None,
            };
            events.extend(author);
        }
        events.extend(chats.row_upsert(chat_id));
        events.push(Event {
            event: BackendEvent::Message(message),
            gate: None,
        });
        let author = match &origin {
            Origin::Remote { author, .. } => author.as_str().to_owned(),
            _ => format!("tg:{me}"),
        };
        let delivered = Delivered {
            chat_id,
            msg_id: msg.id,
            author,
            outgoing: msg.is_outgoing,
        };
        self.note_delivered(&delivered);
        out.staged.rows.push(Row {
            events,
            delivered: vec![delivered],
        });
        if msg.id > cursor.verified_max {
            out.staged.chats.push(ChatFacts {
                chat_id,
                verified_max_id: Some(msg.id),
                ..ChatFacts::default()
            });
        }
        self.cursors.insert(
            chat_id,
            Cursor {
                delivered_max: cursor.delivered_max.max(msg.id),
                verified_max: cursor.verified_max.max(msg.id),
            },
        );
        out
    }

    /// Q-B4: `msg` is our own message in a PRIVATE chat (not the own chat) at or below the
    /// peer's read position TDLib told (`last_read_outbox_message_id`). Groups keep `Sent`.
    fn read_by_peer(&self, chats: &Chats, msg: &Message) -> bool {
        let own_chat = chats.me().is_some_and(|me| me == msg.chat_id);
        msg.is_outgoing
            && !own_chat
            && chats.kind(msg.chat_id) == Some(ChatKind::Direct)
            && self
                .reads
                .get(&msg.chat_id)
                .is_some_and(|r| msg.id <= r.told_out)
    }

    /// Messages of `chat_id` fetched as HISTORY (C8's fresh-link load, C9's reconcile below the
    /// cursor): ONE row per [`MAX_HISTORY_BATCH`] messages, each a `HistoryBatch` (never notified,
    /// `has_more: false`: KT-3 owns "more") after the authors' contact patches and the chat's
    /// patch; `read` from TDLib's read position (incoming) or `true` (own). Messages already
    /// delivered, local ids, scheduled ones and a chat that is not mapped give nothing. The rows
    /// move the cursor; the read rule runs after them ([`Messages::mark_read`]).
    pub(crate) fn history(
        &mut self,
        chats: &mut Chats,
        chat_id: i64,
        msgs: &[Message],
        now_ms: i64,
    ) -> Out {
        let mut out = Out::default();
        let (true, Some(conv), Some(me)) = (chats.is_linked(), chats.conv(chat_id), chats.me())
        else {
            return out;
        };
        let mut msgs: Vec<&Message> = msgs
            .iter()
            .filter(|m| {
                m.chat_id == chat_id
                    && is_server_id(m.id)
                    && m.scheduling_state.is_none()
                    && m.sending_state.is_none()
                    && self.delivered(chat_id, m.id).is_none()
            })
            .collect();
        msgs.sort_unstable_by_key(|m| m.id);
        msgs.dedup_by_key(|m| m.id);
        let read_in = self.reads.get(&chat_id).map_or(0, |r| r.told_in);
        let usernames = chats.my_usernames();
        for chunk in msgs.chunks(MAX_HISTORY_BATCH) {
            let mut events = Vec::new();
            let mut authors = HashSet::new();
            let mut batch = Vec::new();
            let mut delivered = Vec::new();
            for msg in chunk {
                let mapped = content::message(
                    msg,
                    Me {
                        id: me,
                        usernames: &usernames,
                    },
                    now_ms,
                );
                let Some(origin) = content::origin(msg, &mapped) else {
                    self.dropped += 1;
                    continue;
                };
                let read_out = self.read_by_peer(chats, msg);
                let sealed = mapped.sealed();
                let Some(mut m) =
                    self.incoming(msg, conv.clone(), origin.clone(), mapped, read_out, now_ms)
                else {
                    self.dropped += 1;
                    continue;
                };
                if sealed {
                    self.seal(chat_id, msg.id);
                }
                m.catch_up = false;
                m.read = msg.is_outgoing || msg.id <= read_in;
                if !msg.is_outgoing {
                    let author = match msg.sender_id {
                        MessageSender::User { user_id } if user_id != me => {
                            if authors.insert(user_id) {
                                let upsert = chats.author_upsert(user_id);
                                if upsert.is_none() {
                                    out.extend(chats.reference_user(user_id));
                                }
                                upsert
                            } else {
                                None
                            }
                        }
                        MessageSender::Chat { chat_id: sender }
                            if sender != 0 && authors.insert(sender) =>
                        {
                            chats.sender_chat_upsert(sender)
                        }
                        MessageSender::User { .. }
                        | MessageSender::Chat { .. }
                        | MessageSender::Unknown => None,
                    };
                    events.extend(author);
                }
                out.extend(chats.on_delivered(chat_id, msg.id, msg.is_outgoing));
                let author = match &origin {
                    Origin::Remote { author, .. } => author.as_str().to_owned(),
                    _ => format!("tg:{me}"),
                };
                delivered.push(Delivered {
                    chat_id,
                    msg_id: msg.id,
                    author,
                    outgoing: msg.is_outgoing,
                });
                batch.push(m);
            }
            if batch.is_empty() {
                continue;
            }
            events.extend(chats.row_upsert(chat_id));
            let event = BackendEvent::HistoryBatch {
                conv: conv.clone(),
                messages: batch,
                has_more: false,
                progress: None,
            };
            if event.validate_ids().is_err() {
                self.dropped += delivered.len();
                continue;
            }
            events.push(Event { event, gate: None });
            let cursor = self.cursor(chat_id);
            let newest = delivered.iter().map(|d| d.msg_id).max().unwrap_or(0);
            for d in &delivered {
                self.note_delivered(d);
            }
            if newest > cursor.verified_max {
                out.staged.chats.push(ChatFacts {
                    chat_id,
                    verified_max_id: Some(newest),
                    ..ChatFacts::default()
                });
            }
            self.cursors.insert(
                chat_id,
                Cursor {
                    delivered_max: cursor.delivered_max.max(newest),
                    verified_max: cursor.verified_max.max(newest),
                },
            );
            out.staged.rows.push(Row { events, delivered });
        }
        out.extend(self.mark_read(chats, chat_id, now_ms));
        out
    }

    /// The `IncomingMessage` of a mapped message; `None` when an id fails the contract's check.
    fn incoming(
        &self,
        msg: &Message,
        conv: ConvKey,
        origin: Origin,
        mapped: content::Mapped,
        read_out: bool,
        now_ms: i64,
    ) -> Option<IncomingMessage> {
        let local = msg.id.to_string();
        let date_ms = i64::from(msg.date).saturating_mul(1000);
        let own = !matches!(origin, Origin::Remote { .. });
        let reply_to = match msg.reply_to {
            Some(tdapi::ReplyTo::Message {
                chat_id,
                message_id,
            }) if (chat_id == 0 || chat_id == msg.chat_id) && is_server_id(message_id) => {
                Some(ReplyTo {
                    target: MsgTarget {
                        conv: Some(conv.clone()),
                        author: TargetAuthor::Unknown,
                        local: Arc::from(message_id.to_string()),
                        alternates: Vec::new(),
                    },
                    author: None,
                    snapshot: None,
                })
            }
            _ => None,
        };
        let mut m = match mapped.body {
            Body::System(SystemEvent::Call(line)) => {
                IncomingMessage::call(conv, &local, origin, line, date_ms, now_ms)
            }
            body => {
                let mut m = IncomingMessage::new(conv, local, origin, body, date_ms, now_ms);
                m.reply_to = reply_to;
                m.expiry = mapped.expiry;
                m.view_once = mapped.view_once;
                m
            }
        };
        m.server_at = Some(date_ms);
        m.mentions_me = !own && (mapped.mentions_me || msg.contains_unread_mention);
        m.catch_up = self.syncing
            || self.updating
            || self.start_ms.saturating_sub(date_ms) > CATCH_UP_AGE_MS;
        m.meta = mapped.meta;
        if own {
            // Q-B4: an own message the peer of a private chat has read is `Read`.
            m.initial_out_state = Some(if read_out {
                OutgoingState::Read
            } else {
                OutgoingState::Sent {
                    server_at: Some(date_ms),
                }
            });
        }
        let event = BackendEvent::Message(m);
        event.validate_ids().ok()?;
        match event {
            BackendEvent::Message(m) => Some(m),
            _ => None,
        }
    }
}

/// A delivered message as a target: our own, or its author's.
fn target_of(conv: &ConvKey, id: i64, author: &str, outgoing: bool) -> MsgTarget {
    let author = if outgoing {
        TargetAuthor::Own
    } else {
        UserId::parse(author).map_or(TargetAuthor::Unknown, TargetAuthor::User)
    };
    MsgTarget {
        conv: Some(conv.clone()),
        author,
        local: Arc::from(id.to_string()),
        alternates: Vec::new(),
    }
}

/// The row of one `Edited` (the target's author: `by`, or our own account for `None`).
fn edited_row(
    out: &mut Out,
    conv: &ConvKey,
    msg_id: i64,
    by: Option<UserId>,
    body: Body,
    edit_date: i32,
) {
    let author = by.clone().map_or(TargetAuthor::Own, TargetAuthor::User);
    let event = BackendEvent::Edited {
        target: MsgTarget {
            conv: Some(conv.clone()),
            author,
            local: Arc::from(msg_id.to_string()),
            alternates: Vec::new(),
        },
        by,
        new_local: None,
        body,
        edited_at: i64::from(edit_date).saturating_mul(1000),
        keeps_quote: None,
    };
    if event.validate_ids().is_ok() {
        out.staged.rows.push(Row {
            events: vec![Event { event, gate: None }],
            delivered: Vec::new(),
        });
    }
}

/// `updateChatAction`: an ephemeral `Typing` (module doc).
fn on_action(chats: &Chats, u: &ChatActionUpdate) -> Out {
    let mut out = Out::default();
    let (true, Some(conv), Some(me)) = (chats.is_linked(), chats.conv(u.chat_id), chats.me())
    else {
        return out;
    };
    let MessageSender::User { user_id } = u.sender_id else {
        return out;
    };
    if user_id == me || user_id <= 0 {
        return out;
    }
    let on = match u.action {
        ChatAction::Typing => true,
        ChatAction::Cancel => false,
        _ => return out,
    };
    let Ok(by) = UserId::from_parts("tg", &user_id.to_string()) else {
        return out;
    };
    let event = BackendEvent::Typing {
        conv,
        by,
        device: None,
        on,
        ttl: TYPING_TTL,
    };
    if event.validate_ids().is_ok() {
        out.ephemeral.push(event);
    }
    out
}
