//! A chat read on this computer (W4 C17, decision D6).
//!
//! The phone states a chat's unread COUNT, and the delivery turns it into a hint once per chat
//! ([`super::chat::hint`]). A chat the user has read here since the link has no unread messages any
//! more, whatever the phone's old count says: the core tells the backend each read as a
//! `Command::SyncReadMarker`, and [`note_in`] writes it into the chat's `history_chats` row (made if the
//! history has not brought the chat yet), where the delivery zone looks before it states a hint and
//! the take-back of a row looks before it states one again.
//!
//! A read on the PHONE counts the same (W4 fix F7): the live path's own read of another device (`ReadSelf`,
//! `PlayedSelf`, [`crate::translate::read_elsewhere`]) is noted by [`note_in`] in the zone that writes its
//! receipts row ([`crate::events`]), so a chunk that names the chat afterwards states no hint for it.
//!
//! A read of a chat that has no unread message at all produces no marker (the core queues one only when
//! something became read), so this is the whole rule the core gives; the hint of such a chat can still
//! be stated, and the read that follows it clears it as any other. The chat is the core's conversation
//! (the form the marker names it by); a history chat placed by another form of the same person meets
//! it only after the merge, so a hint can still be stated for a chat read under its other form. That
//! errs toward a hint the next read clears, never toward a hidden message.

use kourier_db::rusqlite::Transaction;
use wacore::store::error::StoreError;

use crate::error::sql;

/// Notes, in the caller's zone, that the chat named by its encoded key was read here: no unread hint is stated
/// for it from now on (`send::receipts::confirm_read_marker` writes it with the command's confirmation).
pub(crate) fn note_in(tx: &Transaction<'_>, key: &str) -> Result<(), StoreError> {
    // `more = 0`: a chat the history has not touched owes no end; the delivery sets it.
    tx.prepare_cached(
        "INSERT INTO history_chats(conv, more, read_here) VALUES (?1, 0, 1) \
         ON CONFLICT (conv) DO UPDATE SET read_here = 1",
    )
    .and_then(|mut statement| statement.execute([key]))
    .map(drop)
    .map_err(sql("note a chat read here"))
}
