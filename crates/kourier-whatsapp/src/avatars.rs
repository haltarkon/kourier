//! Profile pictures (W8): one task per connection looks up the pictures of the account's chats.
//!
//! WhatsApp has no list of pictures: each one is an IQ of its own. So this goes slowly and on its own time, the way an
//! app does that fills in pictures as they come into view:
//! - only while the connection is `Online` (the task ends the moment it no longer is);
//! - one lookup per [`GAP_MS`] and up to [`JITTER_MS`] more, chosen by the clock so no two gaps are alike;
//! - at most [`PER_DAY`] lookups in any day, counted from the rows themselves, so a restart does not start the count over;
//! - the chats with the newest messages first; a picture already known is named in the lookup, so an unchanged one costs
//!   no download, and each chat is looked at again only after [`RECHECK_MS`];
//! - a 429 closes the walk for the delay the server named (else [`REFUSED_PAUSE_MS`]), for as long as the actor lives;
//!   a lookup that could not be made (not connected, a timeout) ends the task until the next `Online`.
//!
//! The picture is the small preview, written as a sealed file BEFORE the zone that records it; the file it replaces goes
//! after the commit (Signal's order, `kourier-signal/src/avatar.rs`). What is not a JPEG of a sane size is not kept.
//!
//! Counts and kinds only in the log: no chat, no id, no URL.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kourier_backend_api::{BackendEvent, Clock, encode_outbox_row};
use kourier_blob::{BlobId, BlobStore, FileKey};
use kourier_db::outbox;
use kourier_db::rusqlite::{Transaction, params};
use kourier_model::{BlobKey, BlobRef, ConnState, ContactPatch, ConvKey, ConversationPatch, Patch};
use tokio::sync::{Notify, watch};
use wacore::store::error::StoreError;

use crate::error;
use crate::send::address::jid_of;
use crate::session::{AskFailure, PictureAnswer, Session};
use crate::store::{KourierStore, exec, first, rows};

/// The least time between two lookups.
pub(crate) const GAP_MS: u64 = 6_000;
/// Up to this much is added to every gap.
pub(crate) const JITTER_MS: u64 = 4_000;
/// The most lookups in any 24 hours.
pub(crate) const PER_DAY: i64 = 240;
/// A chat is looked at again after this long.
pub(crate) const RECHECK_MS: i64 = 7 * DAY_MS;
/// How long a 429 that names no delay closes the walk.
pub(crate) const REFUSED_PAUSE_MS: u64 = 6 * 60 * 60 * 1000;
/// How long the walk rests when every chat has been looked at, or the day's lookups are spent.
pub(crate) const REST: Duration = Duration::from_secs(10 * 60);
/// The largest preview kept (WhatsApp's are a few kilobytes).
pub(crate) const MAX_PICTURE: usize = 256 * 1024;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// What the walk needs of the actor.
pub(crate) struct Parts {
    pub(crate) store: KourierStore,
    pub(crate) session: Arc<dyn Session>,
    pub(crate) conn: watch::Receiver<ConnState>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) blob_dir: PathBuf,
    pub(crate) wake: Arc<Notify>,
    /// Until when (ms) the server asked to be left alone. Lives as long as the actor.
    pub(crate) pause: Arc<AtomicU64>,
}

/// Runs until the connection leaves `Online`, or a lookup cannot be made.
pub(crate) async fn run(parts: Parts) {
    let mut conn = parts.conn.clone();
    let offline = conn.wait_for(|state| !matches!(state, ConnState::Online { .. }));
    tokio::select! {
        _ = offline => {}
        () = walk(&parts) => {}
    }
}

async fn walk(parts: &Parts) {
    loop {
        let now = parts.clock.now_ms();
        let until = parts.pause.load(Ordering::Relaxed);
        if until > now {
            parts.clock.sleep_until(until).await;
        }
        let now = parts.clock.now_ms();
        parts
            .clock
            .sleep_until(now + GAP_MS + now % JITTER_MS)
            .await;
        let now = wall(parts);
        match next(&parts.store, now).await {
            Ok(Step::Ask(asked)) => {
                if !ask(parts, asked).await {
                    return;
                }
            }
            Ok(Step::Rest) => parts.clock.sleep(REST).await,
            Err(e) => {
                tracing::warn!(error = %e, "the next picture to look up could not be read");
                return;
            }
        }
    }
}

fn wall(parts: &Parts) -> i64 {
    i64::try_from(parts.clock.now_ms()).unwrap_or(i64::MAX)
}

enum Step {
    Ask(Asked),
    /// Nothing is due, or the day's lookups are spent.
    Rest,
}

struct Asked {
    conv: ConvKey,
    known: Option<String>,
}

/// The chat to ask about next.
async fn next(store: &KourierStore, now: i64) -> Result<Step, StoreError> {
    store
        .read(move |conn| {
            let spent = first(
                conn,
                "count the day's picture lookups",
                "SELECT count(*) FROM avatars WHERE checked_at > ?1",
                params![now - DAY_MS],
                |r| r.get::<_, i64>(0),
            )?
            .unwrap_or(0);
            if spent >= PER_DAY {
                return Ok(Step::Rest);
            }
            let due = rows(
                conn,
                "read the next chat without a known picture",
                "SELECT c.conv, a.picture_id \
                 FROM (SELECT conv FROM features_stated UNION SELECT conv FROM history_chats) c \
                 LEFT JOIN chat_counted n ON n.chat = c.conv \
                 LEFT JOIN avatars a ON a.conv = c.conv \
                 WHERE a.conv IS NULL OR a.checked_at <= ?1 \
                 ORDER BY coalesce(n.newest_ms, 0) DESC, c.conv LIMIT 1",
                params![now - RECHECK_MS],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
            )?;
            let Some((conv, known)) = due.into_iter().next() else {
                return Ok(Step::Rest);
            };
            let conv = ConvKey::parse(&conv).map_err(|_| error::invalid("a stored chat"))?;
            Ok(Step::Ask(Asked { conv, known }))
        })
        .await
}

/// One lookup and what it records. `false`: the lookup could not be made, the walk ends for this connection.
async fn ask(parts: &Parts, asked: Asked) -> bool {
    let Asked { conv, known } = asked;
    let Ok(jid) = jid_of(&conv) else {
        // Not a person or a group (a channel, a list): it has no picture to ask for.
        return settle(parts, conv, Outcome::None).await;
    };
    let group = matches!(conv, ConvKey::Group(_));
    let outcome = match parts.session.profile_picture(jid, group, known).await {
        Ok(PictureAnswer::Unchanged) => Outcome::Unchanged,
        Ok(PictureAnswer::None) => Outcome::None,
        Ok(PictureAnswer::Found { id, bytes }) => {
            if !is_preview(&bytes) {
                tracing::debug!(
                    len = bytes.len(),
                    "a picture that is no preview was not kept"
                );
                Outcome::Unchanged
            } else {
                match write_off_thread(parts.blob_dir.clone(), bytes).await {
                    Ok(blob) => Outcome::Picture { id, blob },
                    Err(kind) => {
                        tracing::warn!(?kind, "a picture could not be written");
                        return false;
                    }
                }
            }
        }
        Err(AskFailure::Refused { code: 429, backoff }) => {
            let delay = backoff.map_or(REFUSED_PAUSE_MS, |d| {
                u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
            });
            let until = parts.clock.now_ms().saturating_add(delay);
            parts.pause.store(until, Ordering::Relaxed);
            tracing::info!("picture lookups were refused; they rest");
            return true;
        }
        Err(failure @ (AskFailure::NoClient | AskFailure::NotConnected | AskFailure::Timeout)) => {
            tracing::debug!(
                failure = failure.kind(),
                "picture lookups end for this connection"
            );
            return false;
        }
        // Any other answer is not worth asking again soon: the row waits for the next look.
        Err(failure) => {
            tracing::debug!(failure = failure.kind(), "a picture lookup failed");
            Outcome::Unchanged
        }
    };
    settle(parts, conv, outcome).await
}

/// What a lookup came to.
enum Outcome {
    /// A new picture, its file written.
    Picture { id: String, blob: BlobRef },
    /// Nothing to record but the look.
    Unchanged,
    /// The chat has no picture this account may see.
    None,
}

/// Records `outcome` in one zone, then removes the file it replaced. `false`: the store is gone.
/// Forgets every picture and removes its file (a `Rebuild`: the core lost what it was told, so the next walk asks and states
/// them again).
pub(crate) async fn forget_all(store: &KourierStore, blob_dir: &Path) -> Result<(), StoreError> {
    const OP: &str = "forget the profile pictures";
    let paths: Vec<String> = store
        .write(OP, move |tx| {
            let paths = rows(
                tx,
                OP,
                "SELECT path FROM avatars WHERE path IS NOT NULL",
                [],
                |row| row.get(0),
            )?;
            exec(tx, OP, "DELETE FROM avatars", [])?;
            Ok(paths)
        })
        .await?;
    for path in paths {
        let _ = std::fs::remove_file(blob_dir.join(path));
    }
    Ok(())
}

async fn settle(parts: &Parts, conv: ConvKey, outcome: Outcome) -> bool {
    let written = match &outcome {
        Outcome::Picture { blob, .. } => Some(blob.path.clone()),
        _ => None,
    };
    let now = wall(parts);
    let recorded = parts
        .store
        .write("record a profile picture", move |tx| {
            record(tx, &conv, outcome, now)
        })
        .await;
    match recorded {
        Ok((told, replaced)) => {
            if told {
                parts.wake.notify_one();
            }
            if let Some(old) = replaced {
                let _ = std::fs::remove_file(parts.blob_dir.join(old));
            }
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, "a profile picture could not be recorded");
            if let Some(path) = written {
                let _ = std::fs::remove_file(parts.blob_dir.join(path));
            }
            false
        }
    }
}

/// The zone of one look: the row, and the statement to the core when the picture changed. Answers whether the core is
/// told something, and the file the zone replaced.
fn record(
    tx: &Transaction<'_>,
    conv: &ConvKey,
    outcome: Outcome,
    now: i64,
) -> Result<(bool, Option<String>), StoreError> {
    let key = conv.encode();
    let old = first(
        tx,
        "read a chat's picture",
        "SELECT path FROM avatars WHERE conv = ?1",
        params![key],
        |r| r.get::<_, Option<String>>(0),
    )?
    .flatten();
    let (statement, replaced) = match outcome {
        Outcome::Unchanged => {
            exec(
                tx,
                "note a look at a picture",
                "INSERT INTO avatars(conv, checked_at) VALUES (?1, ?2) \
                 ON CONFLICT (conv) DO UPDATE SET checked_at = excluded.checked_at",
                params![key, now],
            )?;
            (None, None)
        }
        Outcome::None => {
            exec(
                tx,
                "note a chat without a picture",
                "INSERT INTO avatars(conv, picture_id, path, checked_at) VALUES (?1, NULL, NULL, ?2) \
                 ON CONFLICT (conv) DO UPDATE SET picture_id = NULL, path = NULL, checked_at = excluded.checked_at",
                params![key, now],
            )?;
            (old.is_some().then_some(Patch::Clear), old)
        }
        Outcome::Picture { id, blob } => {
            let path = blob.path.to_string_lossy().into_owned();
            exec(
                tx,
                "record a chat's picture",
                "INSERT INTO avatars(conv, picture_id, path, checked_at) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT (conv) DO UPDATE SET picture_id = excluded.picture_id, path = excluded.path, \
                 checked_at = excluded.checked_at",
                params![key, id, path, now],
            )?;
            (Some(Patch::Set(blob)), old)
        }
    };
    let event = match statement {
        Some(avatar) if !brings_back_a_deleted_group(tx, conv, &key)? => statement_of(conv, avatar),
        _ => None,
    };
    let told = event.is_some();
    if let Some(event) = event {
        let row = encode_outbox_row(&[event])
            .map_err(|_| error::invalid("the picture could not be encoded"))?;
        outbox::append(tx, &row, now).map_err(|_| error::database("append a picture failed"))?;
    }
    Ok((told, replaced))
}

/// A patch of a group states the group: one the user cleared or deleted is left alone rather than brought back.
fn brings_back_a_deleted_group(
    tx: &Transaction<'_>,
    conv: &ConvKey,
    key: &str,
) -> Result<bool, StoreError> {
    if !matches!(conv, ConvKey::Group(_)) {
        return Ok(false);
    }
    Ok(first(
        tx,
        "read a chat's cut",
        "SELECT 1 FROM chat_cut WHERE chat = ?1",
        params![key],
        |r| r.get::<_, i64>(0),
    )?
    .is_some())
}

/// What the core is told of a picture.
fn statement_of(conv: &ConvKey, avatar: Patch<BlobRef>) -> Option<BackendEvent> {
    match conv {
        ConvKey::Direct(user) => {
            let mut patch = ContactPatch::new(user.clone());
            patch.avatar = avatar;
            Some(BackendEvent::ContactUpsert(patch))
        }
        ConvKey::Group(_) => {
            let mut patch = ConversationPatch::new(conv.clone());
            patch.avatar = avatar;
            Some(BackendEvent::ConversationUpsert(patch))
        }
        ConvKey::Broadcast(_) | ConvKey::Channel(_) => None,
    }
}

/// Whether `bytes` can be a preview: a JPEG of a sane size.
pub(crate) fn is_preview(bytes: &[u8]) -> bool {
    bytes.len() <= MAX_PICTURE && bytes.starts_with(&[0xFF, 0xD8, 0xFF])
}

async fn write_off_thread(
    blob_dir: PathBuf,
    bytes: Vec<u8>,
) -> Result<BlobRef, std::io::ErrorKind> {
    tokio::task::spawn_blocking(move || write_picture(&blob_dir, &bytes))
        .await
        .map_err(|_| std::io::ErrorKind::Other)?
        .map_err(|e| e.kind())
}

/// Seals `image` into a new blob below `blob_dir` (atomic, 0600).
fn write_picture(blob_dir: &Path, image: &[u8]) -> std::io::Result<BlobRef> {
    let store = BlobStore::open(blob_dir)?;
    let id = BlobId::new_random();
    let key = FileKey::generate();
    let mut writer = store.create(&id, &key).map_err(std::io::Error::other)?;
    std::io::Write::write_all(&mut writer, image)?;
    writer.finish().map_err(std::io::Error::other)?;
    Ok(BlobRef {
        path: id.relative_path(),
        key: BlobKey::new(*key.as_bytes()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_jpeg_of_a_sane_size_is_a_preview() {
        assert!(is_preview(&[0xFF, 0xD8, 0xFF, 0xE0]));
        assert!(!is_preview(b"<svg></svg>"));
        assert!(!is_preview(&[]));
        let mut large = vec![0xFF, 0xD8, 0xFF];
        large.resize(MAX_PICTURE + 1, 0);
        assert!(!is_preview(&large));
    }

    #[test]
    fn a_person_is_stated_as_a_contact_and_a_group_as_a_chat() {
        let person = ConvKey::parse("direct:lid:77700172839405").expect("a person");
        assert!(matches!(
            statement_of(&person, Patch::Clear),
            Some(BackendEvent::ContactUpsert(_))
        ));
        let group = ConvKey::Group("wag:120363000081726354".into());
        assert!(matches!(
            statement_of(&group, Patch::Clear),
            Some(BackendEvent::ConversationUpsert(_))
        ));
        let channel = ConvKey::Channel("wan:120363000081726355".into());
        assert!(statement_of(&channel, Patch::Clear).is_none());
    }
}
