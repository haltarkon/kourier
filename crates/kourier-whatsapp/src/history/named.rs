//! What the live path named since the link (W4 fix F3).
//!
//! A chunk is a snapshot the phone took BEFORE the delivery, and the delivery can come much later (the stage keeps a
//! chunk across a quit or a failure). If the live path stated a newer title for a group or a newer push name for a
//! person meanwhile, the chunk's older value must not overwrite it: the core applies a title or a profile name as it
//! comes (only the timer carries a version). So the zone that states a name notes it ([`record`], from the events:
//! [`notes_of`]) and the history delivery leaves the field alone for the noted ([`titled`], [`named`]). No contract
//! change: the delivery sends `Unchanged`.
//!
//! The notes are written only while a push can still state a hint ([`super::live::wanted`]) and forgotten with the live
//! notes ([`super::live::tidy`]). Nothing here logs an id or a name.

use std::collections::HashSet;

use kourier_backend_api::BackendEvent;
use kourier_db::rusqlite::{Connection, Transaction};
use kourier_model::Patch;
use wacore::store::error::StoreError;

use crate::error::sql;

/// A conversation title (`key` is the encoded conversation).
const TITLE: i64 = 0;
/// A person's push name (`key` is the user id).
const PUSH_NAME: i64 = 1;

/// A name the live path stated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Note {
    kind: i64,
    key: String,
}

/// The names `events` state: a conversation upsert that sets a title, a contact upsert that sets a profile (push) name.
pub(crate) fn notes_of(events: &[BackendEvent]) -> Vec<Note> {
    events
        .iter()
        .filter_map(|event| match event {
            BackendEvent::ConversationUpsert(patch) if matches!(patch.title, Patch::Set(_)) => {
                Some(Note {
                    kind: TITLE,
                    key: patch.key.encode(),
                })
            }
            BackendEvent::ContactUpsert(patch) if matches!(patch.profile_name, Patch::Set(_)) => {
                Some(Note {
                    kind: PUSH_NAME,
                    key: patch.user.as_str().to_owned(),
                })
            }
            _ => None,
        })
        .collect()
}

/// Notes `notes` at `now_ms`. A name noted again moves to the new time.
pub(crate) fn record(tx: &Transaction<'_>, notes: &[Note], now_ms: i64) -> Result<(), StoreError> {
    for note in notes {
        tx.prepare_cached(
            "INSERT INTO history_named(kind, key, at) VALUES (?1, ?2, ?3) \
             ON CONFLICT (kind, key) DO UPDATE SET at = excluded.at",
        )
        .and_then(|mut s| s.execute((note.kind, &note.key, now_ms)))
        .map_err(sql("note a name the live path stated"))?;
    }
    Ok(())
}

/// Whether the live path stated the title of the conversation `key` (encoded).
pub(crate) fn titled(conn: &Connection, key: &str) -> Result<bool, StoreError> {
    exists(conn, TITLE, key)
}

/// The users of `events`' contact upserts whose push name the live path stated.
pub(crate) fn named(conn: &Connection, users: &[&str]) -> Result<HashSet<String>, StoreError> {
    let mut out = HashSet::new();
    for user in users {
        if exists(conn, PUSH_NAME, user)? {
            out.insert((*user).to_owned());
        }
    }
    Ok(out)
}

fn exists(conn: &Connection, kind: i64, key: &str) -> Result<bool, StoreError> {
    conn.prepare_cached("SELECT EXISTS (SELECT 1 FROM history_named WHERE kind = ?1 AND key = ?2)")
        .and_then(|mut s| s.query_row((kind, key), |r| r.get::<_, bool>(0)))
        .map_err(sql("look whether the live path named something"))
}

/// Forgets the notes made before `before_ms`.
pub(crate) fn prune(tx: &Transaction<'_>, before_ms: i64) -> Result<usize, StoreError> {
    tx.prepare_cached("DELETE FROM history_named WHERE at < ?1")
        .and_then(|mut s| s.execute([before_ms]))
        .map_err(sql("forget old names"))
}

#[cfg(test)]
mod tests;
