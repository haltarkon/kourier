//! TG-3's receive machines (refresh §2 C5a..C6b): what Kourier makes of TDLib's chat, user and
//! message updates. PURE: state in, [`Out`] out (the rows, facts and gaps to commit in one
//! [`Staged`] and the requests to send); the actor wires them (C7a/C7b): it commits `staged` with
//! `store::commit_rows` and sends each [`Fetch`] on the sync lane.
//!
//! - [`chats`]: the chat machine. It LEARNS chats, users, supergroups and basic groups from
//!   TDLib's `Ready` on (critic H1: TDLib tells each once per instance) and STAGES only after the
//!   link; it decides which chats are conversations (the listing rule, L-13) and which are held
//!   until T7 (H5, L4).
//! - [`messages`]: the message machine. One row per new message (the author's and the chat's
//!   patches before it), catch-up during the start sync and for old messages (H2), the link hold,
//!   and the gap triggers in the row's batch: a supergroup's server-id jump (G-8) and the first live
//!   message of a private or basic-group chat after an unclean end (B1); edits by pairing (M1),
//!   permanent deletes parked for KT-2 (G-8), read markers, read receipts and typing (C6b).
//!
//! Nothing here logs a value: `Debug` of the outputs prints ids, kinds and counts.

pub(crate) mod chats;
pub(crate) mod messages;

use kourier_backend_api::BackendEvent;
use serde_json::{Value, json};

use crate::error::LinkError;
use crate::store::{Seed, Staged};
use crate::tdapi::{BasicGroup, Chat, Message, Receive, Supergroup, TgUser};

/// A request a machine needs answered (the sync lane, C7a: one in flight, paced).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fetch {
    /// `getChat`: a chat a fact or a message named that the machine never learnt (H1), answered
    /// into [`chats::Chats::on_chat`] or, on an error, [`chats::Chats::on_chat_failed`].
    Chat(i64),
    /// `getUser` (Q-B1): a user a private chat or a message names that the machine never learnt
    /// (told before the link past the learn bound: TDLib never tells an `updateUser` twice, so
    /// `getChat` alone cannot bring it back), answered from TDLib's own cache into
    /// [`chats::Chats::on_user`] or, on an error, [`chats::Chats::on_user_failed`]. Once per id
    /// per helper run.
    User(i64),
    /// `getSupergroup` (TF4): a supergroup chat whose supergroup object the machine never learnt
    /// (told before the link past the bound): TDLib never tells an `updateSupergroup` twice, so
    /// the chat stays undecided (public? a channel?) until this answer. Once per id per run,
    /// answered into [`chats::Chats::on_supergroup`].
    Supergroup(i64),
    /// `getBasicGroup` (TF4): the same for a basic group (`upgraded_to_supergroup_id` is the
    /// chat's `moved_to`), answered into [`chats::Chats::on_basic_group`].
    BasicGroup(i64),
    /// `getMessage`: the author of an edited message `delivered` does not hold (critic M1),
    /// answered into [`messages::Messages::on_message`] or, on an error (a 404: TDLib no longer
    /// holds it), [`messages::Messages::on_message_failed`].
    Message {
        /// TDLib's chat id.
        chat_id: i64,
        /// TDLib's full message id.
        msg_id: i64,
    },
    /// `loadChats(limit: 100)` of the main list or the archive (C8, the start sync), answered
    /// into [`crate::sync::StartLoad::on_load`]: `ok` = more may come, 404 = the list is complete.
    LoadChats {
        /// The archive list (else the main list).
        archive: bool,
    },
    /// `getChatHistory(chat_id, from_message_id, offset 0, limit, only_local: false)` (C8's
    /// fresh-link load), answered into [`crate::sync::StartLoad::on_history`].
    History {
        /// TDLib's chat id.
        chat_id: i64,
        /// The page starts below this message (TDLib's full id).
        from: i64,
        /// At most this many messages.
        limit: i32,
    },
    /// `getChatHistory(chat_id, from_message_id, offset 0, limit, only_local: false)`: a page of
    /// the RECONCILE (C9a) for the gap keyed `(chat_id, above)`, answered into
    /// [`crate::sync::reconcile::Reconcile::on_page`].
    Gap {
        /// TDLib's chat id.
        chat_id: i64,
        /// The gap's key: its upper bound of what is known delivered (exclusive).
        above: i64,
        /// The page starts below this message (TDLib's full id).
        from: i64,
        /// At most this many messages.
        limit: i32,
    },
}

impl Fetch {
    /// TDLib answers this request from its own memory (an object it named): it reaches no
    /// server, so the sync lane does not pace it and sends it ahead of the paced fetches (fix
    /// round TF3).
    pub(crate) fn answered_from_memory(self) -> bool {
        matches!(
            self,
            Self::Chat(_) | Self::User(_) | Self::Supergroup(_) | Self::BasicGroup(_)
        )
    }

    /// A request of the start sync's loads (`loadChats`, the fresh-link page): a LOST answer of
    /// it is asked again, never taken for the end of the list or of the chat's plan (fix round
    /// U1; `session::lane`).
    pub(crate) fn is_start_load(self) -> bool {
        matches!(self, Self::LoadChats { .. } | Self::History { .. })
    }

    /// The td_api request.
    pub(crate) fn request(self) -> Value {
        match self {
            Self::Chat(chat_id) => json!({"@type": "getChat", "chat_id": chat_id}),
            Self::User(user_id) => json!({"@type": "getUser", "user_id": user_id}),
            Self::Supergroup(id) => json!({"@type": "getSupergroup", "supergroup_id": id}),
            Self::BasicGroup(id) => json!({"@type": "getBasicGroup", "basic_group_id": id}),
            Self::Message { chat_id, msg_id } => {
                json!({"@type": "getMessage", "chat_id": chat_id, "message_id": msg_id})
            }
            Self::LoadChats { archive } => {
                let list = if archive {
                    "chatListArchive"
                } else {
                    "chatListMain"
                };
                json!({"@type": "loadChats", "chat_list": {"@type": list},
                    "limit": crate::sync::LOAD_CHATS_LIMIT})
            }
            Self::History {
                chat_id,
                from,
                limit,
            }
            | Self::Gap {
                chat_id,
                from,
                limit,
                ..
            } => json!({"@type": "getChatHistory", "chat_id": chat_id, "from_message_id": from,
                "offset": 0, "limit": limit, "only_local": false}),
        }
    }
}

/// What one call of a machine hands the actor.
#[derive(Debug, Default)]
pub(crate) struct Out {
    /// To commit, in one transaction.
    pub(crate) staged: Staged,
    /// To send, in order.
    pub(crate) fetch: Vec<Fetch>,
    /// Reads TDLib lost, to send again: `(chat id, message id of the stored read position)`
    /// (TG-4 K4, D-2). Not a [`Fetch`]: `Fetch` is `Copy` and its answers go to
    /// [`Receiver::on_fetched`], while a re-assert's answer changes nothing in the machines (the
    /// lane's own `LaneAsk::Reassert`).
    pub(crate) reassert: Vec<(i64, i64)>,
    /// Ephemeral events (typing) for the core at once, never stored (`Envelope::ephemeral`).
    pub(crate) ephemeral: Vec<BackendEvent>,
}

impl Out {
    /// Nothing to commit and nothing to send (tests only).
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.staged.is_empty()
            && self.fetch.is_empty()
            && self.reassert.is_empty()
            && self.ephemeral.is_empty()
    }

    /// Appends `other` after what is here.
    pub(crate) fn extend(&mut self, other: Out) {
        let Out {
            staged,
            fetch,
            reassert,
            ephemeral,
        } = other;
        self.staged.chats.extend(staged.chats);
        self.staged.rows.extend(staged.rows);
        self.staged.gaps.extend(staged.gaps);
        self.staged.parked.extend(staged.parked);
        self.staged.clean_end |= staged.clean_end;
        self.fetch.extend(fetch);
        self.reassert.extend(reassert);
        self.ephemeral.extend(ephemeral);
    }
}

/// Both machines of one helper run, wired as run B's API says (C7a): every receive update goes
/// to the chat machine FIRST, then to the message machine; a fetch's answer goes to the machine(s)
/// that asked it.
#[derive(Debug)]
pub(crate) struct Receiver {
    pub(crate) chats: chats::Chats,
    pub(crate) messages: messages::Messages,
    /// The start sync's chat load and fresh-link load (C8).
    pub(crate) sync: crate::sync::StartLoad,
}

impl Receiver {
    /// The machines of a helper run that started at `start_ms` (`me`: the account's user id when
    /// it is known; `after_unclean`: `store::start_run` said `Unclean`).
    pub(crate) fn new(me: Option<i64>, start_ms: i64, after_unclean: bool) -> Self {
        Self {
            chats: chats::Chats::new(me),
            messages: messages::Messages::new(start_ms, after_unclean),
            sync: crate::sync::StartLoad::default(),
        }
        .fresh_chats_held()
    }

    /// The fresh-chat hold begins with the run (TG-4 K5, critic H1): a chat with a stored cursor
    /// of 0 keeps its live rows until the start sync's fresh-link load has run for it.
    fn fresh_chats_held(mut self) -> Self {
        self.messages.hold_fresh_chats();
        self
    }

    /// Messages wait for an answer, or dropped ids are not covered by a gap row yet (TF3): the
    /// run must not be called clean.
    pub(crate) fn unsettled(&self) -> bool {
        self.messages.unsettled()
    }

    /// The machines start from what the store holds (C8): every chat's cursor and read
    /// positions, the delivered messages, and the incoming unread rows above the read position.
    /// Called before the link.
    pub(crate) fn seed(&mut self, seed: &Seed) {
        let mut read_in = std::collections::HashMap::new();
        for c in &seed.chats {
            self.messages.seed(
                c.chat_id,
                messages::Cursor {
                    delivered_max: c.delivered_max_id,
                    verified_max: c.verified_max_id,
                },
            );
            self.messages
                .seed_read(c.chat_id, c.read_inbox_max_id, c.read_outbox_max_id);
            read_in.insert(c.chat_id, c.read_inbox_max_id);
        }
        self.sync.reconcile.seed(&seed.gaps);
        for d in &seed.delivered {
            self.messages.note_delivered(d);
            if !d.outgoing && d.msg_id > read_in.get(&d.chat_id).copied().unwrap_or(0) {
                self.chats.seed_unread(d.chat_id, [d.msg_id]);
            }
        }
    }

    /// The start sync begins (the run's first `Ready` of a linked account).
    pub(crate) fn begin_sync(&mut self) -> Out {
        self.sync.begin()
    }

    /// The start sync's loads are over.
    pub(crate) fn sync_done(&self) -> bool {
        self.sync.is_done()
    }

    /// TDLib showed `Updating -> Ready` (C9b): the reconcile's in-memory decision.
    pub(crate) fn redecide(&mut self, now_ms: i64) -> Out {
        self.chats.set_now(now_ms);
        self.sync
            .reconcile
            .redecide(&mut self.chats, &mut self.messages, now_ms)
    }

    /// When [`Receiver::on_tick`] is due (a supergroup's decision waiting for its stream, C9b).
    pub(crate) fn due_ms(&self) -> Option<i64> {
        self.sync.reconcile.due_ms()
    }

    /// The clock reached [`Receiver::due_ms`].
    pub(crate) fn on_tick(&mut self, now_ms: i64) -> Out {
        self.chats.set_now(now_ms);
        self.sync
            .reconcile
            .on_tick(&mut self.chats, &mut self.messages, now_ms)
    }

    /// One receive update.
    pub(crate) fn on_update(&mut self, update: &Receive, now_ms: i64) -> Out {
        self.chats.set_now(now_ms);
        let mut out = self.chats.on_update(update);
        out.extend(self.messages.on_update(&mut self.chats, update, now_ms));
        out.extend(self.after_decisions(now_ms));
        self.sync.reconcile.track(&mut out);
        out
    }

    /// Chats this update or answer decided (TF4): a chat the fresh-link load had to skip as
    /// undecided gets its own load first (its last message and unread page, from no cursor),
    /// then the messages that waited for the decision are replayed.
    fn after_decisions(&mut self, now_ms: i64) -> Out {
        let mut out = self
            .sync
            .late_loads(&mut self.chats, &mut self.messages, now_ms);
        out.extend(self.messages.release_decided(&mut self.chats, now_ms));
        out
    }

    /// The answer of `fetch` (an error, or an answer that does not parse as the object asked
    /// for, is the fetch's failure).
    pub(crate) fn on_fetched(
        &mut self,
        fetch: Fetch,
        result: Result<Value, LinkError>,
        now_ms: i64,
    ) -> Out {
        self.chats.set_now(now_ms);
        let mut out = Out::default();
        match fetch {
            Fetch::Chat(chat_id) => match parse::<Chat>(result).filter(|c| c.id == chat_id) {
                Some(chat) => {
                    out.extend(self.chats.on_chat(chat));
                    out.extend(self.messages.on_chat(&mut self.chats, chat_id, now_ms));
                }
                None => {
                    self.chats.on_chat_failed(chat_id);
                    self.messages.on_chat_failed(chat_id);
                }
            },
            Fetch::User(user_id) => match parse::<TgUser>(result).filter(|u| u.id == user_id) {
                Some(user) => out.extend(self.chats.on_user(user)),
                None => self.chats.on_user_failed(user_id),
            },
            Fetch::Supergroup(id) => {
                if let Some(sg) = parse::<Supergroup>(result).filter(|s| s.id == id) {
                    out.extend(self.chats.on_supergroup(sg));
                }
            }
            Fetch::BasicGroup(id) => {
                if let Some(bg) = parse::<BasicGroup>(result).filter(|b| b.id == id) {
                    out.extend(self.chats.on_basic_group(bg));
                }
            }
            Fetch::Message { chat_id, msg_id } => {
                match parse::<Message>(result).filter(|m| m.chat_id == chat_id && m.id == msg_id) {
                    Some(msg) => out.extend(self.messages.on_message(&self.chats, &msg, now_ms)),
                    None => self.messages.on_message_failed(chat_id, msg_id),
                }
            }
            Fetch::LoadChats { archive } => out.extend(self.sync.on_load(
                archive,
                &result,
                &mut self.chats,
                &mut self.messages,
                now_ms,
            )),
            Fetch::History {
                chat_id,
                from,
                limit,
            } => out.extend(self.sync.on_history(
                (chat_id, from, limit),
                result,
                &mut self.chats,
                &mut self.messages,
                now_ms,
            )),
            Fetch::Gap {
                chat_id,
                above,
                from,
                ..
            } => out.extend(self.sync.reconcile.on_page(
                (chat_id, above, from),
                result,
                &mut self.chats,
                &mut self.messages,
                now_ms,
            )),
        }
        out.extend(self.after_decisions(now_ms));
        // A gap a message opened in this output (a `getChat` answer's waiting messages).
        self.sync.reconcile.track(&mut out);
        out
    }
}

/// A fetch's answer as `T`, `None` for an error or another shape (never logged).
fn parse<T: serde::de::DeserializeOwned>(result: Result<Value, LinkError>) -> Option<T> {
    serde_json::from_value(result.ok()?).ok()
}

#[cfg(test)]
mod tests;
