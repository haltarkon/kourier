//! The selection mode of the timeline: which messages of the open chat are picked, for Copy as
//! Text now and for the export and Forward later.
//!
//! The state lives here, below the front-end seam, and not in a delegate (delegates come and go as
//! the list scrolls, and the model resets on a large change) or in QML logic (the GUI is
//! swappable). A front-end shows it through the `selected` flag of each [`MessageRow`]
//! ([`Selection::mark`]) and the mode's `is_active` / `len`.
//!
//! Selectable rows are the rows with a message id (`RowKey::Message`: messages, notices, calls,
//! "deleted" lines), never a date header, the unread marker or the loading row.
//!
//! [`copy_text`] is the plain text of a set of rows, built from the rows the timeline itself
//! shows, so nothing is formatted twice.

use std::collections::BTreeSet;

use kourier_model::{ConversationId, MessageId};

use crate::preview::attachment_label;
use crate::time::TimeContext;
use crate::timeline::{Block, MessageRow, RowKey, RowKind};

/// The selection of one conversation.
#[derive(Clone, Debug, Default)]
pub struct Selection {
    /// The conversation the selection belongs to.
    conv: Option<ConversationId>,
    /// The mode is on (it stays on at 0 selected: Cancel or Escape ends it).
    active: bool,
    ids: BTreeSet<MessageId>,
    /// The row a Shift+click extends from.
    anchor: Option<MessageId>,
    /// The loaded selectable rows as the last row list had them, in the front-end's order.
    order: Vec<MessageId>,
}

impl Selection {
    /// An empty selection, the mode off.
    pub fn new() -> Self {
        Self::default()
    }

    /// The mode is on.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// How many messages are selected.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Nothing is selected (the mode may still be on).
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Whether `id` is selected.
    pub fn contains(&self, id: MessageId) -> bool {
        self.ids.contains(&id)
    }

    /// The selected ids, in id order (the core orders them for what it does with them).
    pub fn ids(&self) -> Vec<MessageId> {
        self.ids.iter().copied().collect()
    }

    /// The conversation the selection belongs to, while the mode is on.
    pub fn conversation(&self) -> Option<ConversationId> {
        self.conv.filter(|_| self.active)
    }

    /// The anchor of the next Shift+click.
    pub fn anchor(&self) -> Option<MessageId> {
        self.anchor
    }

    /// Starts the mode in `conv` with `id` the one selected row, and the anchor (the menu's
    /// "Select" and a Ctrl+click outside the mode). A start in another chat drops what an
    /// earlier mode held.
    pub fn start(&mut self, conv: ConversationId, id: MessageId) {
        if self.conv != Some(conv) {
            self.order.clear();
        }
        self.conv = Some(conv);
        self.active = true;
        self.ids.clear();
        self.ids.insert(id);
        self.anchor = Some(id);
    }

    /// Flips one row, and makes it the anchor. Nothing happens while the mode is off.
    pub fn toggle(&mut self, id: MessageId) {
        if !self.active {
            return;
        }
        if !self.ids.remove(&id) {
            self.ids.insert(id);
        }
        self.anchor = Some(id);
    }

    /// Shift+click: selects every row between the anchor and `id` in `order` (the loaded rows),
    /// both ends included; what was selected stays and the anchor stays. When the anchor is not
    /// in `order` (it is not loaded, or there is none) or `id` is not, it acts as
    /// [`Self::toggle`].
    pub fn extend(&mut self, id: MessageId, order: &[MessageId]) {
        if !self.active {
            return;
        }
        let from = self.anchor.and_then(|a| order.iter().position(|m| *m == a));
        let to = order.iter().position(|m| *m == id);
        let (Some(from), Some(to)) = (from, to) else {
            self.toggle(id);
            return;
        };
        let (lo, hi) = if from <= to { (from, to) } else { (to, from) };
        self.ids.extend(order[lo..=hi].iter().copied());
    }

    /// Ends the mode and forgets everything.
    pub fn clear(&mut self) {
        self.active = false;
        self.ids.clear();
        self.anchor = None;
    }

    /// Takes the loaded selectable rows of the open chat (`None`: no chat is open), once for
    /// every new row list. Another chat (or none) ends the mode. Otherwise a selected row that
    /// was in the previous order and is missing from the new one is dropped only when it
    /// "went": the nearest rows of the previous order before and after it that are still
    /// loaded are both there (it was deleted or expired inside the window). A row at an edge of
    /// the window, or one whose neighbours all left with it (the window jumped: a search hit,
    /// Jump to Latest, the window cap), stays selected, as does every id outside the loaded
    /// window.
    pub fn observe(&mut self, conv: Option<ConversationId>, order: Vec<MessageId>) {
        if self.active && conv != self.conv {
            self.clear();
        }
        if !self.active {
            self.conv = conv;
        }
        if self.active {
            let now: std::collections::HashSet<MessageId> = order.iter().copied().collect();
            let old = std::mem::take(&mut self.order);
            let went: Vec<MessageId> = old
                .iter()
                .enumerate()
                .filter(|(i, id)| {
                    self.ids.contains(id)
                        && !now.contains(id)
                        && old[..*i].iter().any(|m| now.contains(m))
                        && old[*i + 1..].iter().any(|m| now.contains(m))
                })
                .map(|(_, id)| *id)
                .collect();
            for id in went {
                self.ids.remove(&id);
                if self.anchor == Some(id) {
                    self.anchor = None;
                }
            }
        }
        self.order = order;
    }

    /// Sets [`MessageRow::selected`] of every row: true for a selected message while the mode
    /// is on, false for everything else.
    pub fn mark(&self, rows: &mut [MessageRow]) {
        for row in rows {
            row.selected = self.active
                && match row.key {
                    RowKey::Message(id) => self.ids.contains(&id),
                    _ => false,
                };
        }
    }
}

/// The answer of Copy as Text: `text` for the clipboard and the toast.
pub fn copied(text: &str) -> crate::person::CopyUpdate {
    crate::person::CopyUpdate {
        text: Some(kourier_model::Redacted::new(text.to_owned())),
        told: crate::person::copied_text(),
    }
}

/// The answer of Copy as Text when the core could not read the messages: nothing is copied.
pub fn copy_failed() -> crate::person::CopyUpdate {
    crate::person::CopyUpdate {
        text: None,
        told: crate::i18n::trc(
            "@info:status toast, the selected messages could not be copied",
            "Could not copy the messages",
        )
        .into_owned(),
    }
}

/// The ids of the selectable rows of `rows`, in their order.
pub fn selectable(rows: &[MessageRow]) -> Vec<MessageId> {
    rows.iter().filter_map(MessageRow::message_id).collect()
}

/// The plain text of `rows` as Copy as Text puts it on the clipboard. `rows` come as the
/// timeline builds them (newest first); the text is oldest first. Date headers, the unread
/// marker and the loading row are not messages and add nothing.
///
/// One message is `[<date> <time>] <Sender>: <text>`: the whole text of a long message, its
/// newlines kept, spoilers written out, mentions as names (the row's plain text is that). Each
/// attachment is a line `[<file name>]` (a picture has no name here: the kind word the
/// timeline uses). A notice or a call is `[<date> <time>] <its timeline text>` with no sender.
/// A deleted message says what the timeline says. Lines are joined with `\n`.
pub fn copy_text(rows: &[MessageRow], time: &TimeContext<'_>) -> String {
    let mut lines: Vec<String> = Vec::new();
    for row in rows.iter().rev() {
        let stamp = format!(
            "[{} {}]",
            time.short_date(row.sent_at),
            time.clock(row.sent_at)
        );
        match row.kind {
            RowKind::System => lines.push(format!("{stamp} {}", row.text)),
            RowKind::Message => {
                let mut parts: Vec<String> = Vec::new();
                for block in &row.blocks {
                    match block {
                        Block::Text(t) => parts.push(t.full_plain().to_owned()),
                        Block::Placeholder(s) => parts.push(s.text.clone()),
                        Block::File(f) => parts.push(format!("[{}]", f.name)),
                        Block::Image(_) | Block::Album(_) => {
                            for i in block.images() {
                                let word = if i.sticker {
                                    crate::i18n::trc("@info:preview kind of message", "Sticker")
                                        .into_owned()
                                } else {
                                    attachment_label(i.kind, 1).text
                                };
                                parts.push(format!("[{word}]"));
                            }
                        }
                        // A quote is context, not part of what the message says.
                        Block::Quote(_) => {}
                    }
                }
                let mut parts = parts.into_iter();
                let first = parts.next().unwrap_or_default();
                lines.push(format!("{stamp} {}: {first}", row.author));
                lines.extend(parts);
            }
            RowKind::DateHeader | RowKind::UnreadMarker | RowKind::Loading => {}
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(n: i64) -> MessageId {
        MessageId(n)
    }
    fn c(n: i64) -> ConversationId {
        ConversationId(n)
    }
    fn order(ids: &[i64]) -> Vec<MessageId> {
        ids.iter().map(|n| m(*n)).collect()
    }
    fn started(ids: &[i64], first: i64) -> Selection {
        let mut s = Selection::new();
        s.observe(Some(c(1)), order(ids));
        s.start(c(1), m(first));
        s
    }

    #[test]
    fn start_selects_one_row_and_makes_it_the_anchor() {
        let mut s = Selection::new();
        assert!(!s.is_active());
        s.start(c(1), m(5));
        assert!(s.is_active());
        assert_eq!(s.len(), 1);
        assert!(s.contains(m(5)));
        assert_eq!(s.anchor(), Some(m(5)));
        assert_eq!(s.conversation(), Some(c(1)));
    }

    #[test]
    fn toggle_flips_one_row_and_the_mode_stays_on_at_zero() {
        let mut s = started(&[1, 2, 3], 2);
        s.toggle(m(3));
        assert_eq!(s.ids(), order(&[2, 3]));
        assert_eq!(s.anchor(), Some(m(3)));
        s.toggle(m(2));
        s.toggle(m(3));
        assert_eq!(s.len(), 0);
        assert!(s.is_empty());
        assert!(
            s.is_active(),
            "Cancel or Escape ends the mode, not a last click"
        );
    }

    #[test]
    fn toggle_and_extend_do_nothing_while_the_mode_is_off() {
        let mut s = Selection::new();
        s.toggle(m(1));
        s.extend(m(2), &order(&[1, 2]));
        assert_eq!(s.len(), 0);
        assert!(!s.is_active());
    }

    #[test]
    fn extend_selects_the_range_in_both_directions_and_keeps_the_anchor() {
        let ord = order(&[1, 2, 3, 4, 5, 6, 7]);
        let mut s = started(&[1, 2, 3, 4, 5, 6, 7], 3);
        s.extend(m(5), &ord);
        assert_eq!(s.ids(), order(&[3, 4, 5]));
        assert_eq!(s.anchor(), Some(m(3)));
        // The other direction from the same anchor adds, it does not replace.
        s.extend(m(1), &ord);
        assert_eq!(s.ids(), order(&[1, 2, 3, 4, 5]));
        assert_eq!(s.anchor(), Some(m(3)));
    }

    #[test]
    fn extend_follows_the_order_it_is_given_not_the_id_order() {
        // The timeline's store runs newest first.
        let ord = order(&[9, 7, 5, 3, 1]);
        let mut s = started(&[9, 7, 5, 3, 1], 9);
        s.extend(m(5), &ord);
        assert_eq!(s.ids(), order(&[5, 7, 9]));
    }

    #[test]
    fn extend_from_an_unloaded_anchor_acts_as_toggle() {
        let mut s = started(&[10, 11, 12], 1);
        s.extend(m(11), &order(&[10, 11, 12]));
        assert_eq!(s.ids(), order(&[1, 11]));
        assert_eq!(s.anchor(), Some(m(11)), "the toggled row is the anchor now");
    }

    #[test]
    fn extend_to_an_unloaded_row_acts_as_toggle() {
        let mut s = started(&[1, 2, 3], 1);
        s.extend(m(40), &order(&[1, 2, 3]));
        assert_eq!(s.ids(), order(&[1, 40]));
    }

    #[test]
    fn a_row_deleted_in_the_middle_is_dropped() {
        let mut s = started(&[1, 2, 3, 4, 5], 3);
        s.observe(Some(c(1)), order(&[1, 2, 4, 5]));
        assert_eq!(s.len(), 0);
        assert!(s.is_active());
    }

    #[test]
    fn several_adjacent_rows_deleted_together_are_dropped() {
        let mut s = started(&[1, 2, 3, 4, 5], 2);
        s.toggle(m(3));
        s.toggle(m(4));
        s.observe(Some(c(1)), order(&[1, 5]));
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn a_row_at_an_edge_of_the_window_stays() {
        // The newest row of the window leaves it (a cap), or the oldest does.
        let mut s = started(&[1, 2, 3, 4, 5], 5);
        s.toggle(m(1));
        s.observe(Some(c(1)), order(&[2, 3, 4]));
        assert_eq!(s.ids(), order(&[1, 5]));
    }

    #[test]
    fn a_jump_of_the_window_keeps_everything() {
        let mut s = started(&[1, 2, 3, 4, 5], 2);
        s.toggle(m(4));
        // A search hit far away: nothing of the old window is loaded any more.
        s.observe(Some(c(1)), order(&[100, 101, 102]));
        assert_eq!(s.ids(), order(&[2, 4]));
        // And back: the rows are there again, still selected.
        s.observe(Some(c(1)), order(&[1, 2, 3, 4, 5]));
        assert_eq!(s.ids(), order(&[2, 4]));
    }

    #[test]
    fn a_window_that_moves_on_keeps_what_left_it() {
        let mut s = started(&[1, 2, 3, 4, 5, 6], 1);
        s.toggle(m(2));
        s.observe(Some(c(1)), order(&[3, 4, 5, 6, 7, 8]));
        assert_eq!(s.ids(), order(&[1, 2]));
    }

    #[test]
    fn ids_outside_the_loaded_window_stay_selected() {
        let mut s = started(&[10, 11, 12], 1);
        s.observe(Some(c(1)), order(&[10, 12]));
        assert!(s.contains(m(1)));
    }

    #[test]
    fn another_conversation_or_none_ends_the_mode() {
        let mut s = started(&[1, 2], 1);
        s.observe(Some(c(2)), order(&[7, 8]));
        assert!(!s.is_active());
        assert_eq!(s.len(), 0);
        let mut s = started(&[1, 2], 1);
        s.observe(None, Vec::new());
        assert!(!s.is_active());
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn clear_ends_the_mode() {
        let mut s = started(&[1, 2], 1);
        s.clear();
        assert!(!s.is_active());
        assert_eq!(s.len(), 0);
        assert_eq!(s.anchor(), None);
        assert_eq!(s.conversation(), None);
    }

    #[test]
    fn a_new_row_list_does_not_start_the_mode() {
        let mut s = Selection::new();
        s.observe(Some(c(1)), order(&[1, 2, 3]));
        assert!(!s.is_active());
    }

    fn row(key: RowKey, kind: RowKind) -> MessageRow {
        MessageRow::marker(key, kind, String::new())
    }

    #[test]
    fn mark_sets_the_flag_of_selected_messages_only() {
        let mut rows = vec![
            row(RowKey::Message(m(3)), RowKind::Message),
            row(RowKey::Date(5), RowKind::DateHeader),
            row(RowKey::Message(m(2)), RowKind::System),
            row(RowKey::Message(m(1)), RowKind::Message),
            row(RowKey::Loading, RowKind::Loading),
        ];
        assert_eq!(selectable(&rows), order(&[3, 2, 1]));
        let mut s = Selection::new();
        s.observe(Some(c(1)), selectable(&rows));
        s.start(c(1), m(2));
        s.toggle(m(1));
        s.mark(&mut rows);
        let flags: Vec<bool> = rows.iter().map(|r| r.selected).collect();
        assert_eq!(flags, [false, false, true, true, false]);
        s.clear();
        s.mark(&mut rows);
        assert!(rows.iter().all(|r| !r.selected));
    }

    fn at(h: i64) -> i64 {
        // 2026-01-01 00:00 UTC plus `h` hours.
        1_767_225_600_000 + h * 3_600_000
    }

    #[test]
    fn copy_text_is_oldest_first_with_whole_texts_attachments_and_notices() {
        use crate::preview::Summary;
        use crate::richtext::RichText;
        use crate::timeline::{FileBlock, ImageBlock};
        use kourier_model::AttachmentKind;
        let cal = crate::time::FixedOffset(0);
        let time = TimeContext::new(at(48), &cal);
        let long = RichText {
            plain: "cut".into(),
            more: true,
            whole: "line one\nline two, whole".into(),
            ..RichText::default()
        };
        let mut a = row(RowKey::Message(m(3)), RowKind::Message);
        a.author = "Ann".into();
        a.sent_at = at(3);
        a.blocks = vec![Block::Text(long)];
        let mut b = row(RowKey::Message(m(2)), RowKind::Message);
        b.author = "You".into();
        b.sent_at = at(2);
        b.blocks = vec![
            Block::Image(ImageBlock {
                kind: AttachmentKind::Image,
                ..ImageBlock::default()
            }),
            Block::File(FileBlock {
                name: "report.pdf".into(),
                ..FileBlock::default()
            }),
        ];
        let mut n = row(RowKey::Message(m(1)), RowKind::System);
        n.text = "Bob joined".into();
        n.sent_at = at(1);
        let mut gone = row(RowKey::Message(m(0)), RowKind::Message);
        gone.author = "Bob".into();
        gone.sent_at = at(0);
        gone.deleted = true;
        gone.blocks = vec![Block::Placeholder(Summary {
            text: "This message was deleted".into(),
            icon: "trash-empty".into(),
        })];
        let rows = vec![
            a,
            row(RowKey::Date(1), RowKind::DateHeader),
            b,
            n,
            gone,
            row(RowKey::Loading, RowKind::Loading),
        ];
        let text = copy_text(&rows, &time);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 6, "{text}");
        assert!(
            lines[0].ends_with("Bob: This message was deleted"),
            "{text}"
        );
        assert!(lines[1].ends_with("] Bob joined"), "{text}");
        assert!(lines[2].ends_with("] You: [Photo]"), "{text}");
        assert_eq!(lines[3], "[report.pdf]");
        assert!(lines[4].ends_with("] Ann: line one"), "{text}");
        assert_eq!(lines[5], "line two, whole");
        assert!(lines[0].starts_with("[1 Jan "), "{text}");
        assert!(!text.contains("cut"));
    }
}
