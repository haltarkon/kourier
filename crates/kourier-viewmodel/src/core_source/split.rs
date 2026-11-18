//! Split view (two chats side by side), the viewmodel's half.
//!
//! [`CoreSource`] keeps the fields of ONE open chat, the pane in hand, exactly as before the
//! split existed. The other pane's copy of those fields is [`PaneState`] in `side`; one swap
//! ([`CoreSource::swap_panes`]) exchanges them, so with `side == None` nothing here runs and
//! no line of the single-chat code changed. `here` says which pane (0 left, 1 right) is in hand;
//! it is the ACTIVE pane except during a visit ([`CoreSource::visit_pane`]), which lends the
//! in-hand fields to the inactive pane so the ordinary helpers can work on it.

use super::*;

/// The per-open-chat fields of [`CoreSource`], for the pane that is not in hand.
///
/// THE RULE: a field of `CoreSource` that belongs to the open chat is listed here AND in
/// [`CoreSource::swap_panes`]; a new one is added to both (the compiler checks the first
/// half through [`PaneState::new`], the test `pane_state_swaps_every_field` the second).
/// Fields of one-of-a-kind panels (person, group, gallery, revisions, search) and the
/// composer's notice are NOT per pane: the core keeps one of each.
pub(super) struct PaneState {
    pub(super) open: Option<ConversationId>,
    pub(super) detail_q: QueryTracker,
    pub(super) detail: Option<ConversationDetail>,
    pub(super) detail_seeded: bool,
    pub(super) timeline_q: QueryTracker,
    pub(super) fetching: Option<(u64, Fetch)>,
    pub(super) want_reload: bool,
    pub(super) want_older: bool,
    pub(super) want_newer: bool,
    pub(super) want_around: Option<MessageId>,
    pub(super) fetch_log: Vec<&'static str>,
    pub(super) around_hit: Option<MessageId>,
    pub(super) want_newest: bool,
    pub(super) following: bool,
    pub(super) window: Window,
    pub(super) read_more: ReadMore,
    pub(super) unread_from: Option<MessageId>,
    pub(super) flags_deadline: Option<Instant>,
    pub(super) last_marked: Option<(ConversationId, MessageId)>,
    pub(super) downloads: HashMap<AttachmentId, f32>,
    pub(super) send_phases: HashMap<MessageId, SendPhase>,
    pub(super) uploads: HashMap<AttachmentId, UploadBytes>,
    /// The marks of the rows (R3): per open chat, like the rows.
    pub(super) marks: std::cell::RefCell<MarkCache>,
}

impl PaneState {
    /// A closed pane whose queries count from `base`.
    pub(super) fn new(base: u64) -> Self {
        Self {
            open: None,
            detail_q: QueryTracker::with_base(MODEL_CONVERSATION, base),
            detail: None,
            detail_seeded: false,
            timeline_q: QueryTracker::with_base(MODEL_TIMELINE, base),
            fetching: None,
            want_reload: false,
            want_older: false,
            want_newer: false,
            want_around: None,
            fetch_log: Vec::new(),
            around_hit: None,
            want_newest: false,
            following: false,
            window: Window::default(),
            read_more: ReadMore::default(),
            unread_from: None,
            flags_deadline: None,
            last_marked: None,
            downloads: HashMap::new(),
            send_phases: HashMap::new(),
            uploads: HashMap::new(),
            marks: std::cell::RefCell::default(),
        }
    }

    /// The timeline holds an own message on its way.
    pub(super) fn sends_waiting(&self) -> bool {
        self.open.is_some() && self.window.rows.iter().any(|r| r.outbox.is_some())
    }
}

impl CoreSource {
    /// Exchanges every per-open-chat field with the other pane's and flips `here`.
    pub(super) fn swap_panes(&mut self) {
        use std::mem::swap;
        let Some(side) = self.side.as_deref_mut() else {
            return;
        };
        swap(&mut self.open, &mut side.open);
        swap(&mut self.detail_q, &mut side.detail_q);
        swap(&mut self.detail, &mut side.detail);
        swap(&mut self.detail_seeded, &mut side.detail_seeded);
        swap(&mut self.timeline_q, &mut side.timeline_q);
        swap(&mut self.fetching, &mut side.fetching);
        swap(&mut self.want_reload, &mut side.want_reload);
        swap(&mut self.want_older, &mut side.want_older);
        swap(&mut self.want_newer, &mut side.want_newer);
        swap(&mut self.want_around, &mut side.want_around);
        swap(&mut self.fetch_log, &mut side.fetch_log);
        swap(&mut self.around_hit, &mut side.around_hit);
        swap(&mut self.want_newest, &mut side.want_newest);
        swap(&mut self.following, &mut side.following);
        swap(&mut self.window, &mut side.window);
        swap(&mut self.read_more, &mut side.read_more);
        swap(&mut self.unread_from, &mut side.unread_from);
        swap(&mut self.flags_deadline, &mut side.flags_deadline);
        swap(&mut self.last_marked, &mut side.last_marked);
        swap(&mut self.downloads, &mut side.downloads);
        swap(&mut self.send_phases, &mut side.send_phases);
        swap(&mut self.uploads, &mut side.uploads);
        swap(self.marks.get_mut(), side.marks.get_mut());
        self.here ^= 1;
    }

    /// A new pane's trackers count from a base of their own, so a token tells which pane
    /// asked, also after the panes swap or a pane is dropped and another made.
    fn next_pane_base(&mut self) -> u64 {
        self.pane_epoch += 1;
        self.pane_epoch << 40
    }

    /// Whether `token` was issued by the pane that is NOT in hand (its header or timeline
    /// query). Every other tracker is global.
    pub(super) fn parked_issued(&self, token: QueryToken) -> bool {
        if self.visiting {
            return false;
        }
        let Some(side) = self.side.as_ref() else {
            return false;
        };
        match token.model {
            MODEL_CONVERSATION => side.detail_q.issued(token),
            MODEL_TIMELINE => side.timeline_q.issued(token),
            _ => false,
        }
    }

    /// Starts a visit of the parked pane (internal: no pane argument).
    pub(super) fn begin_visit(&mut self) -> bool {
        if self.visiting || self.side.is_none() {
            return false;
        }
        self.swap_panes();
        self.visiting = true;
        true
    }

    pub(super) fn visit_pane(&mut self, pane: u8) {
        if pane != self.here {
            self.begin_visit();
        }
    }

    pub(super) fn end_visit(&mut self) {
        if !self.visiting {
            return;
        }
        self.swap_panes();
        self.visiting = false;
        self.send_presence();
    }

    pub(super) fn active_pane(&self) -> u8 {
        if self.visiting {
            self.here ^ 1
        } else {
            self.here
        }
    }

    pub(super) fn pane_of(&self, id: ConversationId) -> Option<u8> {
        if self.open == Some(id) {
            Some(self.here)
        } else if self.side.as_ref().is_some_and(|s| s.open == Some(id)) {
            Some(self.here ^ 1)
        } else {
            None
        }
    }

    pub(super) fn set_side_shown(&mut self, shown: bool) -> Changes {
        if self.side_shown != shown {
            self.side_shown = shown;
            self.send_presence();
        }
        Changes::default()
    }

    pub(super) fn activate_pane(&mut self, pane: u8) -> Changes {
        if self.visiting || self.side.is_none() || pane == self.here || pane > 1 {
            return Changes::default();
        }
        self.swap_panes();
        self.send_presence();
        self.maybe_mark_read();
        Changes {
            pane: true,
            ..Changes::default()
        }
    }

    pub(super) fn open_side(&mut self, id: ConversationId) -> Changes {
        if self.visiting {
            return Changes::default();
        }
        if let Some(pane) = self.pane_of(id) {
            return self.activate_pane(pane);
        }
        let mut ch = if self.side.is_some() {
            self.activate_pane(1)
        } else {
            let base = self.next_pane_base();
            self.side = Some(Box::new(PaneState::new(base)));
            // The new pane is pane 1, in hand and active; today's chat is parked as pane 0.
            self.swap_panes();
            Changes::default()
        };
        ch.merge(self.open_conversation(Some(id)));
        ch.pane = true;
        ch
    }

    pub(super) fn close_pane(&mut self, pane: u8) -> Changes {
        if self.visiting {
            return Changes::default();
        }
        if self.side.is_none() {
            return self.open_conversation(None);
        }
        if pane == self.here {
            self.drop_in_hand();
            Changes {
                timeline: true,
                header: true,
                pane: true,
                ..Changes::default()
            }
        } else {
            self.drop_parked();
            Changes {
                pane: true,
                ..Changes::default()
            }
        }
    }

    /// Split view: the two chats exchange places (the left one is the right one now and the other
    /// way round). The pane in hand stays in hand, so every per-chat field (queries and their
    /// trackers, rows, flags, marks) stays where it is: only the pane index of each chat flips.
    /// Tokens are keyed on the trackers, never on an index, and presence names chats, not
    /// indices, so nothing else needs to move. Nothing during a visit or without a second pane.
    pub(super) fn swap_sides(&mut self) -> Changes {
        if self.visiting || self.side.is_none() {
            return Changes::default();
        }
        self.here ^= 1;
        Changes {
            pane: true,
            ..Changes::default()
        }
    }

    /// The typing "on" of a chat that goes: told off.
    fn typing_off_for(&mut self, conv: Option<ConversationId>) {
        if let Some(conv) = conv
            && self.typing_sent.is_some_and(|(c, _)| c == conv)
        {
            self.typing_sent = None;
            self.send(UiCommand::SetTyping { conv, on: false });
        }
    }

    /// The split ends and the pane that is not in hand is gone: the pane in hand is the single
    /// pane 0.
    fn drop_parked(&mut self) {
        let gone = self.side.as_ref().and_then(|s| s.open);
        self.typing_off_for(gone);
        self.side = None;
        self.here = 0;
        self.send_presence();
    }

    /// The split ends and the pane in hand is gone (closed, or its chat went): the parked pane
    /// moves into the single pane 0.
    fn drop_in_hand(&mut self) {
        let gone = self.open;
        self.typing_off_for(gone);
        self.revisions_shown = None;
        self.swap_panes();
        self.side = None;
        self.here = 0;
        self.send_presence();
        self.maybe_mark_read();
    }

    /// After an event: a pane whose chat went collapses the split; a merge of one pane's chat
    /// into the other's does too (otherwise the parked pane switches by `side.switch_to`).
    pub(super) fn settle_split(&mut self, event: &UiEvent, ch: &mut Changes) {
        if let UiEvent::ConversationMerged { from, into } = event
            && let Some(parked) = self.side.as_ref().map(|s| s.open)
        {
            let here = self.open;
            if here == Some(*from) && parked == Some(*into) {
                // Today's chat became the parked pane's: the parked chat survives alone.
                ch.switch_to = None;
                self.drop_in_hand();
                *ch = Changes {
                    timeline: true,
                    header: true,
                    pane: true,
                    ..*ch
                };
                ch.side = PaneChanges::NONE;
                return;
            }
            if parked == Some(*from) && here == Some(*into) {
                self.drop_parked();
                ch.side = PaneChanges::NONE;
                ch.pane = true;
                return;
            }
            if parked == Some(*from) {
                ch.side.switch_to = Some(*into);
            }
        }
        let Some(side) = self.side.as_ref() else {
            return;
        };
        let (here_gone, parked_gone) = (self.open.is_none(), side.open.is_none());
        if parked_gone {
            self.drop_parked();
            ch.side = PaneChanges::NONE;
            ch.pane = true;
        } else if here_gone {
            self.drop_in_hand();
            ch.side = PaneChanges::NONE;
            ch.timeline = true;
            ch.header = true;
            ch.pane = true;
        }
    }

    /// `reload`'s part for the pane that is not in hand.
    pub(super) fn reload_side(&mut self) -> Changes {
        if !self.begin_visit() {
            return Changes::default();
        }
        self.window.failed = false;
        self.request_detail();
        self.reload_timeline();
        self.end_visit();
        Changes::default()
    }

    /// One event with a split: the pane in hand reacts as ever (`on_event`, entered again with
    /// `in_event` set), then the pane that is not in hand ([`Self::side_reaction`]), and the
    /// split settles (a chat that went, a merge: [`Self::settle_split`]).
    pub(super) fn on_split_event(&mut self, event: &UiEvent) -> Changes {
        self.in_event = true;
        let mut ch = UiSource::on_event(self, event);
        self.in_event = false;
        if self.side.is_some() {
            ch.merge(self.side_reaction(event));
            self.settle_split(event, &mut ch);
        }
        ch
    }

    /// What an event does to the pane that is not in hand: it visits that pane and repeats
    /// every reaction of `CoreSource::on_event` (for the pane in hand) that touches a per-pane field. The
    /// flags found go to [`Changes::side`].
    ///
    /// Covered, with the arm of `on_event` each repeats (state shared by both panes is
    /// changed by that arm once and is not repeated here):
    /// - `AccountHold`, `ConnState`: the chat's account is the event's -> timeline lines
    ///   (waits) and, for `ConnState`, the header (the composer says when it cannot send).
    /// - `AccountsChanged`, `ContactsChanged`: header and timeline read again.
    /// - `ConversationsChanged`: header read again when the chat is named.
    /// - `ConversationsRemoved`: the chat closes (`settle_split` then collapses the split).
    /// - `MessagesChanged`, `TimelineInvalidated`: the timeline reads again.
    /// - `Progress::Transmit` / `Upload` / `Download`: the send phase, the rings and lines,
    ///   the download percentage of the rows of that chat's window.
    /// - `Typing`, `HistoryProgress`: the header of that chat.
    /// - `QueryResult`: NOT here: `on_query_result` routes by the token's pane.
    /// - `ConversationMerged`: `settle_split` (collapse or `side.switch_to`).
    ///
    /// Not covered, on purpose: the account and login events, transfers, notices, export,
    /// media, filters, saves and the gallery (global, or one-of-a-kind panels); the edit
    /// history refresh (its panel belongs to one pane); `CancelTooLate` (the composer's one
    /// notice slot is the active pane's: a note for the parked chat would offer Delete for
    /// Everyone in the wrong composer); the list read's archive move and mark-read (the active
    /// pane only).
    pub(super) fn side_reaction(&mut self, event: &UiEvent) -> Changes {
        if !self.begin_visit() {
            return Changes::default();
        }
        let ch = match event {
            UiEvent::AccountHold { account, .. } => Changes {
                timeline: self.open_account() == Some(*account) && self.sends_waiting(),
                ..Changes::default()
            },
            UiEvent::ConnState { account, .. } => {
                let here = self.open_account() == Some(*account);
                Changes {
                    timeline: here && self.sends_waiting(),
                    header: here,
                    ..Changes::default()
                }
            }
            UiEvent::AccountsChanged | UiEvent::ContactsChanged { .. } => {
                self.request_detail();
                self.reload_timeline();
                Changes::default()
            }
            UiEvent::ConversationsChanged { ids } => {
                if self.open.is_some_and(|o| ids.contains(&o)) {
                    self.request_detail();
                }
                Changes::default()
            }
            UiEvent::ConversationsRemoved { ids } => {
                if self.open.is_some_and(|o| ids.contains(&o)) {
                    self.close()
                } else {
                    Changes::default()
                }
            }
            UiEvent::MessagesChanged { conv, .. } | UiEvent::TimelineInvalidated { conv } => {
                if self.open == Some(*conv) {
                    self.reload_timeline();
                }
                Changes::default()
            }
            UiEvent::Progress(Progress::Transmit {
                message,
                progress,
                phase,
            }) => {
                let mut ch = self.on_send_phase(*message, *phase);
                ch.merge(self.on_uploads_over(*message, *progress, *phase));
                ch
            }
            UiEvent::Progress(Progress::Upload {
                message,
                attachment,
                done,
                total,
            }) => self.on_upload_progress(*message, *attachment, *done, *total),
            UiEvent::Progress(Progress::Download {
                attachment,
                done,
                total,
            }) => Changes {
                timeline: self.pane_download_progress(*attachment, *done, *total),
                ..Changes::default()
            },
            UiEvent::Typing { conv, .. } | UiEvent::HistoryProgress { conv, .. } => Changes {
                header: self.open == Some(*conv),
                ..Changes::default()
            },
            _ => Changes::default(),
        };
        self.end_visit();
        ch.moved_to_side()
    }
}
