//! Thumbnails, blurhashes and dimensions derived from a downloaded file (§5.8, §6.11 "rendered
//! core-side"). The file work runs on the blocking pool (≤ 2 at a time); the results are
//! recorded in small transactions, each applying only while its attachment still has that
//! file. The results of one batch of downloads that are ready together share a transaction
//! ([`GROUP`]), so a catch-up with many pictures costs the writer one commit per group, not
//! one per picture.
//!
//! A thumbnail is written without fsync (`BlobStore::import_volatile`): it can be made
//! again from the picture, which is durable, and the two fsyncs of a durable write were
//! half of what a received picture cost the disk. After a power loss such a file can be
//! missing or cut short although its row was committed. [`broken_thumbs`] finds those by
//! their length and [`forget_thumbs`] hands their attachments back to the derive step; it
//! runs once after every start, in the background.
//!
//! A downloaded long text (`Attachment.long_text`) is joined here too: the file is the whole
//! text of a message whose body arrived trimmed. The text replaces the body's text (or caption)
//! and the attachment goes, with its row and file; the other attachments keep their order. If
//! the message was edited meanwhile, the whole text belongs to the original revision and the
//! edits stay on top.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kourier_blob::{BlobError, FileKey};
use kourier_db::rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use kourier_db::{Priority, failpoint};
use kourier_model::{AccountId, AttachmentState, Body, MessageId, MsgIdScope, Text};

use super::DeriveJob;
use super::policy::MAX_LONG_TEXT_BYTES;
use crate::error::Result;
use crate::ingest::{Apply, Changes};
use crate::media::DeriveHold;
use crate::runtime::Shared;
use crate::sql;
use crate::store::lane;

/// Kill point inside the thumbnail transaction, before its commit (§10.4): the thumbnail blob
/// exists but no row names it; the next start derives again.
pub const FAILPOINT_BEFORE_DERIVE_COMMIT: &str = "core_before_derive_commit";

/// What deriving found.
pub(crate) enum Outcome {
    /// A thumbnail was written (a new blob, not referenced until the commit).
    Thumb {
        path: PathBuf,
        key: FileKey,
        size: u64,
        mime: &'static str,
        width: u32,
        height: u32,
        blurhash: String,
    },
    /// The whole text of the message (a long text, read and decoded).
    LongText(String),
    /// Nothing to derive (not a picture, or one the core cannot decode).
    Nothing,
    /// The file is missing or does not decrypt: the attachment must be downloaded again.
    Unreadable,
}

/// Reads, decrypts and thumbnails the file of `job` (blocking).
pub(crate) fn compute(blob_dir: &Path, job: &DeriveJob) -> Outcome {
    let path = blob_dir.join(&job.path);
    let key = FileKey::from_bytes(*job.key.expose());
    if job.long_text {
        return match kourier_blob::read_all(&path, &key) {
            // Larger than any text, or empty: it stays a file.
            Ok(bytes) if bytes.is_empty() || bytes.len() as u64 > MAX_LONG_TEXT_BYTES => {
                Outcome::Nothing
            }
            Ok(bytes) => Outcome::LongText(String::from_utf8_lossy(&bytes).into_owned()),
            Err(e) => unreadable(job, &e),
        };
    }
    if !job.is_picture() {
        // Only check that the file is there and opens.
        return match kourier_blob::BlobReader::open(&path, &key) {
            Ok(_) => Outcome::Nothing,
            Err(e) => unreadable(job, &e),
        };
    }
    let bytes = match kourier_blob::read_all(&path, &key) {
        Ok(b) => b,
        Err(e) => return unreadable(job, &e),
    };
    let derived = match kourier_media::derive(&bytes) {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!(attachment = job.attachment.0, error = %e, "no thumbnail for an attachment");
            return Outcome::Nothing;
        }
    };
    let store = match super::core_store(blob_dir) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "opening the blob store failed");
            return Outcome::Nothing;
        }
    };
    // Not fsynced: it can be derived again (module docs).
    match store.import_volatile(&mut derived.thumbnail.bytes.as_slice()) {
        Ok((id, key, size)) => Outcome::Thumb {
            path: super::core_path(&id),
            key,
            size,
            mime: derived.thumbnail.mime,
            width: derived.width,
            height: derived.height,
            blurhash: derived.blurhash,
        },
        Err(e) => {
            tracing::warn!(error = %e, "writing a thumbnail failed");
            Outcome::Nothing
        }
    }
}

fn unreadable(job: &DeriveJob, e: &BlobError) -> Outcome {
    tracing::warn!(attachment = job.attachment.0, error = %e, "a downloaded attachment is unreadable");
    Outcome::Unreadable
}

/// Most results one transaction records ([`commit_all`]).
pub(crate) const GROUP: usize = 16;

/// Records `outcome` for `job` in one transaction ([`commit_all`] for one result).
#[cfg(test)]
pub(crate) fn commit(
    conn: &mut Connection,
    account: AccountId,
    scope: MsgIdScope,
    now: i64,
    job: &DeriveJob,
    outcome: Outcome,
) -> Result<Changes> {
    commit_all(conn, account, scope, now, vec![(job.clone(), outcome)])
}

/// Records the outcomes of `results` in one transaction, each if its attachment still has
/// the file its job read. A thumbnail that is not recorded is scheduled for unlinking.
pub(crate) fn commit_all(
    conn: &mut Connection,
    account: AccountId,
    scope: MsgIdScope,
    now: i64,
    results: Vec<(DeriveJob, Outcome)>,
) -> Result<Changes> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut apply = Apply::begin(&tx, account, scope, now)?;
    for (job, outcome) in results {
        record(&mut apply, account, now, &job, outcome)?;
    }
    apply.finish()?;
    let changes = std::mem::take(&mut apply.ch);
    drop(apply);
    failpoint::hit(FAILPOINT_BEFORE_DERIVE_COMMIT);
    tx.commit()?;
    Ok(changes)
}

/// Records `outcome` for `job` inside a transaction, if the attachment still has the file the
/// job read.
fn record(
    apply: &mut Apply<'_>,
    account: AccountId,
    now: i64,
    job: &DeriveJob,
    outcome: Outcome,
) -> Result<()> {
    let tx = apply.conn;
    let current: Option<(Option<i64>, Option<i64>)> = tx
        .prepare_cached("SELECT file_id, thumb_file_id FROM attachments WHERE id = ?1")?
        .query_row([job.attachment.0], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let old_thumb = match current {
        Some((Some(file), thumb)) if file == job.file_id => thumb,
        _ => {
            // Deleted or downloaded again meanwhile: the result is stale.
            if let Outcome::Thumb { path, .. } = outcome {
                apply.ch.unlink.push(path);
            }
            return Ok(());
        }
    };
    match outcome {
        Outcome::LongText(text) => {
            return join_long_text(apply, job, &text);
        }
        Outcome::Thumb {
            path,
            key,
            size,
            mime,
            width,
            height,
            blurhash,
        } => {
            tx.prepare_cached(
                "INSERT INTO files(account_id, path, key, size, mime, purpose)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'thumb')",
            )?
            .execute(params![
                account.0,
                path.to_string_lossy(),
                &key.as_bytes()[..],
                sql::i64_of(size),
                mime
            ])?;
            let thumb = tx.last_insert_rowid();
            // The decoded file is the truth for its display size (EXIF orientation applied);
            // the sender's pointer may claim anything. The sender's blurhash stays: it is what
            // the timeline showed while the file was downloading.
            tx.prepare_cached(
                "UPDATE attachments SET thumb_file_id = ?2, blurhash = coalesce(blurhash, ?3),
                   width = ?4, height = ?5, derived_at = ?6
                 WHERE id = ?1",
            )?
            .execute(params![
                job.attachment.0,
                thumb,
                blurhash,
                width,
                height,
                now
            ])?;
            if let Some(old) = old_thumb {
                let p: Option<String> = tx
                    .prepare_cached("SELECT path FROM files WHERE id = ?1")?
                    .query_row([old], |r| r.get(0))
                    .optional()?;
                tx.prepare_cached("DELETE FROM files WHERE id = ?1")?
                    .execute([old])?;
                apply.ch.unlink.extend(p.map(PathBuf::from));
            }
        }
        Outcome::Nothing => {
            tx.prepare_cached("UPDATE attachments SET derived_at = ?2 WHERE id = ?1")?
                .execute((job.attachment.0, now))?;
        }
        Outcome::Unreadable => {
            tx.prepare_cached(
                "UPDATE attachments SET file_id = NULL, state = ?2, derived_at = ?3 WHERE id = ?1",
            )?
            .execute((
                job.attachment.0,
                sql::to_json(&AttachmentState::Failed { retryable: true })?,
                now,
            ))?;
            tx.prepare_cached("DELETE FROM files WHERE id = ?1")?
                .execute([job.file_id])?;
            apply.ch.unlink.push(job.path.clone());
        }
    }
    apply.ch.media.insert(job.attachment);
    apply.ch.message(job.conv, job.message);
    Ok(())
}

/// One thumbnail row as the repair pass sees it: the `files` row, where the file is
/// (relative to the account's blob directory) and how much plaintext it holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ThumbFile {
    pub file_id: i64,
    pub path: PathBuf,
    pub size: u64,
}

/// Every thumbnail `core.db` has for `account`.
pub(crate) fn thumbs(conn: &Connection, account: AccountId) -> Result<Vec<ThumbFile>> {
    let rows: Vec<(i64, String, Option<i64>)> = conn
        .prepare_cached(
            "SELECT id, path, size FROM files WHERE account_id = ?1 AND purpose = 'thumb'",
        )?
        .query_map([account.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(file_id, path, size)| {
            Some(ThumbFile {
                file_id,
                path: PathBuf::from(path),
                size: u64::try_from(size?).ok()?,
            })
        })
        .collect())
}

/// The thumbnails among `thumbs` whose file is missing or does not have the length of a
/// complete blob of its size (blocking: one `stat` each). Thumbnails are written without
/// fsync, so this is what a power loss right after one can leave.
pub(crate) fn broken_thumbs(blob_dir: &Path, thumbs: &[ThumbFile]) -> Vec<ThumbFile> {
    thumbs
        .iter()
        .filter(|t| {
            std::fs::metadata(blob_dir.join(&t.path))
                .map_or(true, |m| m.len() != kourier_blob::sealed_len(t.size))
        })
        .cloned()
        .collect()
}

/// Takes broken thumbnails off their attachments (one transaction): the `files` rows go,
/// the files are unlinked after the commit, and the attachments count as not derived, so
/// [`crate::ingest::pending_derives`] hands them to the derive step again.
pub(crate) fn forget_thumbs(
    conn: &mut Connection,
    account: AccountId,
    scope: MsgIdScope,
    now: i64,
    broken: &[ThumbFile],
) -> Result<Changes> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut apply = Apply::begin(&tx, account, scope, now)?;
    for thumb in broken {
        let still: bool = tx
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM files WHERE id = ?1 AND account_id = ?2
                   AND purpose = 'thumb')",
            )?
            .query_row((thumb.file_id, account.0), |r| r.get(0))?;
        if !still {
            continue;
        }
        let rows: Vec<(i64, i64, i64)> = tx
            .prepare_cached(
                "SELECT a.id, a.message_id, m.conversation_id FROM attachments a
                   JOIN messages m ON m.id = a.message_id WHERE a.thumb_file_id = ?1",
            )?
            .query_map([thumb.file_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        tx.prepare_cached(
            "UPDATE attachments SET thumb_file_id = NULL, derived_at = NULL
             WHERE thumb_file_id = ?1",
        )?
        .execute([thumb.file_id])?;
        apply.delete_file(thumb.file_id)?;
        for (attachment, message, conv) in rows {
            apply
                .ch
                .media
                .insert(kourier_model::AttachmentId(attachment));
            apply
                .ch
                .message(kourier_model::ConversationId(conv), MessageId(message));
        }
    }
    apply.finish()?;
    let changes = std::mem::take(&mut apply.ch);
    drop(apply);
    tx.commit()?;
    Ok(changes)
}

/// After a start, in the background: thumbnails a power loss cut off or lost are derived
/// again (module docs). Costs one `stat` per thumbnail of the account, off the startup path.
pub(crate) fn repair_thumbs(
    sh: &Arc<Shared>,
    account: AccountId,
    scope: MsgIdScope,
    blob_dir: PathBuf,
) {
    let sh = sh.clone();
    tokio::spawn(async move {
        let Ok(all) = sh.store.read_media(move |c| thumbs(c, account)).await else {
            return;
        };
        if all.is_empty() {
            return;
        }
        let dir = blob_dir.clone();
        let Ok(broken) = tokio::task::spawn_blocking(move || broken_thumbs(&dir, &all)).await
        else {
            return;
        };
        if broken.is_empty() {
            return;
        }
        tracing::info!(
            account = account.0,
            thumbnails = broken.len(),
            "thumbnails were lost or cut off; deriving them again"
        );
        let now = sh.now();
        let forgotten = sh
            .store
            .write(Priority::Low, lane(account), move |c| {
                let ch = forget_thumbs(c, account, scope, now, &broken)?;
                Ok((ch, crate::ingest::pending_derives(c, account)?))
            })
            .await;
        match forgotten {
            Ok((ch, jobs)) => {
                // Counted before the events that tell the UI the thumbnails are gone.
                let hold = sh.media.reserve(jobs.len());
                sh.publish_local(account, &blob_dir, ch).await;
                spawn(&sh, account, scope, blob_dir, jobs, hold);
            }
            Err(e) => {
                tracing::warn!(account = account.0, error = %e, "forgetting broken thumbnails failed")
            }
        }
    });
}

/// `body` without its attachment number `idx` (the text or caption stays as it is). A `Media`
/// body left without attachments becomes a text.
pub(crate) fn without_attachment(body: Body, idx: usize) -> Body {
    without_long_text(body, idx, None)
}

/// `body` without its attachment number `idx`; with `whole`, that text replaces the text or
/// caption (ranges stay when the old text is its beginning). A `Media` body left without
/// attachments becomes a text.
fn without_long_text(body: Body, idx: usize, whole: Option<&str>) -> Body {
    let Body::Media {
        mut attachments,
        caption,
    } = body
    else {
        return body;
    };
    if idx < attachments.len() {
        attachments.remove(idx);
    }
    let caption = match whole {
        Some(whole) => {
            let ranges = caption
                .filter(|old| whole.starts_with(old.body.as_str()))
                .map(|old| old.ranges)
                .unwrap_or_default();
            let mut text = Text {
                body: whole.to_owned(),
                ranges,
            };
            text.sanitize();
            Some(text)
        }
        None => caption,
    };
    if attachments.is_empty() {
        Body::Text(caption.unwrap_or_default())
    } else {
        Body::Media {
            attachments,
            caption,
        }
    }
}

/// Joins the downloaded long text of `job` into its message: the attachment row and its file go
/// (the rows behind it move up, as in the body's list), and the whole text replaces the text of
/// the original body: the message row's, or revision 0 once the message was edited (the edits
/// keep their own texts and lose only the attachment entry). An imported edited message has no
/// body of its revision 0: there the text is the newest revision's.
fn join_long_text(apply: &mut Apply<'_>, job: &DeriveJob, whole: &str) -> Result<()> {
    let conn = apply.conn;
    let message = MessageId(job.message.0);
    let idx: i64 = conn
        .prepare_cached("SELECT idx FROM attachments WHERE id = ?1")?
        .query_row([job.attachment.0], |r| r.get(0))?;
    let at = usize::try_from(idx).unwrap_or(usize::MAX);
    let (body, deleted): (String, Option<String>) = conn
        .prepare_cached("SELECT body, deleted FROM messages WHERE id = ?1")?
        .query_row([message.0], |r| Ok((r.get(0)?, r.get(1)?)))?;
    if deleted.is_some() {
        return Ok(());
    }
    // The row and its file, then the rows behind it (ascending, so `idx` stays unique).
    conn.prepare_cached("DELETE FROM attachments WHERE id = ?1")?
        .execute([job.attachment.0])?;
    apply.delete_file(job.file_id)?;
    let later: Vec<i64> = conn
        .prepare_cached(
            "SELECT id FROM attachments WHERE message_id = ?1 AND idx > ?2 ORDER BY idx",
        )?
        .query_map((message.0, idx), |r| r.get(0))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    for id in later {
        conn.prepare_cached("UPDATE attachments SET idx = idx - 1 WHERE id = ?1")?
            .execute([id])?;
    }
    apply.ch.media.insert(job.attachment);
    apply.ch.media_gone.insert(job.attachment);

    let revisions: Vec<(i64, Option<String>, bool)> = conn
        .prepare_cached(
            "SELECT rowid, body, edited_at IS NULL FROM message_revisions WHERE message_id = ?1
             ORDER BY revision",
        )?
        .query_map([message.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    // The revision the text belongs to: the original's (revision 0) when it has a body, as for
    // a live message edited before its rest arrived. An imported edited message has no body
    // of its original (only the newest version's content is in the archive): its text is that
    // of the NEWEST revision with a body, the one the row shows.
    let whole_into = revisions
        .iter()
        .find(|(_, body, original)| *original && body.is_some())
        .or_else(|| revisions.iter().rev().find(|(_, body, _)| body.is_some()))
        .map(|(rowid, _, _)| *rowid);
    if revisions.is_empty() {
        let joined = without_long_text(sql::from_json(&body)?, at, Some(whole));
        conn.prepare_cached(
            "UPDATE messages SET body = ?2, body_kind = ?3, preview = ?4 WHERE id = ?1",
        )?
        .execute(params![
            message.0,
            sql::to_json(&joined)?,
            joined.kind_name(),
            crate::ingest::preview(&joined)
        ])?;
    } else {
        for (rowid, body, _) in revisions {
            let Some(body) = body else { continue };
            let text = (Some(rowid) == whole_into).then_some(whole);
            let joined = without_long_text(sql::from_json(&body)?, at, text);
            conn.prepare_cached("UPDATE message_revisions SET body = ?2 WHERE rowid = ?1")?
                .execute((rowid, sql::to_json(&joined)?))?;
        }
        // The latest revision is the row's body again.
        apply.reindex_revisions(message)?;
    }
    apply.ch.message(job.conv, message);
    Ok(())
}

/// Derives `jobs` of `account` in the background, one after another (at most two batches work at
/// the same time), and records and publishes the results in groups: a result waits for the
/// rest of its group only while those are being computed, never for a result that is not
/// on its way.
///
/// `hold` counts these jobs in [`MediaHandles::deriving`](crate::media::MediaHandles::deriving)
/// (reserved by the caller before the UI events of the `Changes` they came with, one job each);
/// the task releases a job when the group that recorded it has been published, and the rest
/// when it ends, however it ends.
pub(crate) fn spawn(
    sh: &Arc<Shared>,
    account: AccountId,
    scope: MsgIdScope,
    blob_dir: PathBuf,
    jobs: Vec<DeriveJob>,
    mut hold: DeriveHold,
) {
    if jobs.is_empty() {
        return;
    }
    let sh = sh.clone();
    tokio::spawn(async move {
        let mut jobs = jobs.into_iter().peekable();
        let mut group: Vec<(DeriveJob, Outcome)> = Vec::new();
        // Jobs taken from the list whose group has not been published yet.
        let mut unpublished = 0;
        let mut open = sh.media.derives_open();
        while let Some(job) = jobs.next() {
            // (Held only by a test: `MediaHandles::hold_derives`.)
            if open.wait_for(|open| *open).await.is_err() {
                return;
            }
            unpublished += 1;
            let Ok(permit) = sh.derive_permits.clone().acquire_owned().await else {
                return;
            };
            // A removed account's files are going and its rows with them: nothing to derive,
            // nothing to record (link feedback L4, F6).
            if sh.is_removing(account) {
                tracing::debug!(
                    account = account.0,
                    "the account is being removed: its thumbnails stop"
                );
                return;
            }
            let (dir, j) = (blob_dir.clone(), job.clone());
            let outcome = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                compute(&dir, &j)
            })
            .await;
            if let Ok(outcome) = outcome {
                group.push((job, outcome));
            }
            if group.len() < GROUP && jobs.peek().is_some() {
                continue;
            }
            let results = std::mem::take(&mut group);
            if results.is_empty() {
                continue;
            }
            let thumbs: Vec<PathBuf> = results
                .iter()
                .filter_map(|(_, outcome)| match outcome {
                    Outcome::Thumb { path, .. } => Some(path.clone()),
                    _ => None,
                })
                .collect();
            if sh.is_removing(account) {
                // Written but never recorded: the account's directory goes with them.
                return;
            }
            let now = sh.now();
            let res = sh
                .store
                .write(Priority::Low, lane(account), move |c| {
                    commit_all(c, account, scope, now, results)
                })
                .await;
            match res {
                Ok(ch) => sh.publish_local(account, &blob_dir, ch).await,
                Err(e) => {
                    tracing::warn!(account = account.0, error = %e, "recording thumbnails failed");
                    if !thumbs.is_empty() {
                        let dir = blob_dir.clone();
                        let _ =
                            tokio::task::spawn_blocking(move || super::unlink(&dir, &thumbs)).await;
                    }
                }
            }
            // The URLs of this group's pictures have changed (or nothing more will for them).
            hold.release(std::mem::take(&mut unpublished));
        }
        // `hold` goes with the task: what is left (jobs that failed to compute, a task that
        // stopped early) is released here, on every `return` above, and when the task is dropped.
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::IngestHarness;
    use kourier_backend_api::BackendEvent;
    use kourier_fake::attachments::{Pointer, synthetic_image};
    use kourier_fake::scripts::{remote, signal_features};
    use kourier_model::{
        AttachmentId, AttachmentMeta, Body, ConvKey, ConversationPatch, NetworkKind, Text, UserId,
    };

    const ME: &str = "aci:00000000-0000-4000-8000-0000000000aa";
    const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";

    /// A sender's pointer may claim any size; once the file is here, its decoded dimensions win
    /// (the timeline lays the picture out by them).
    #[test]
    fn the_decoded_file_decides_the_dimensions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blobs = dir.path().join("blobs");
        let alice = UserId::parse(ALICE).expect("id");
        let conv = ConvKey::Direct(alice.clone());
        let mut h = IngestHarness::open(
            &dir.path().join("core.db"),
            &kourier_db::DbKey::from([5; 32]),
            NetworkKind::Fake,
            MsgIdScope::AccountAuthor,
            &[UserId::parse(ME).expect("id")],
        )
        .expect("harness");
        let mut patch = ConversationPatch::new(conv.clone());
        patch.features = Some(signal_features(false));
        let mut photo = synthetic_image(64, 48, 1);
        photo.dims = Some((1, 9999));
        let pointer = Pointer::from_blob(photo.remote.as_ref().expect("remote")).expect("ours");
        let mut m = remote(&conv, &alice, "1", 1_000, "");
        m.body = Body::Media {
            attachments: vec![photo],
            caption: None,
        };
        h.apply(vec![
            BackendEvent::ConversationUpsert(patch),
            BackendEvent::Message(m),
        ])
        .expect("apply");

        // The policy asked for it: write the file the way a backend does, then answer without
        // dimensions (Signal's pointer is all a backend knows).
        let (att, path, key): (i64, String, Vec<u8>) = h
            .connection()
            .query_row(
                "SELECT attachment_id, path, key FROM attachment_downloads",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("a download in flight");
        let plain = pointer.plaintext().expect("bytes").expect("ok");
        let target = blobs.join(&path);
        std::fs::create_dir_all(target.parent().expect("shard")).expect("shard dir");
        let key: [u8; 32] = key.try_into().expect("key");
        kourier_blob::write_all(&target, &FileKey::from_bytes(key), &plain).expect("blob");
        h.apply(vec![BackendEvent::AttachmentDone {
            attachment: AttachmentId(att),
            request: None,
            result: Ok(AttachmentMeta {
                size: plain.len() as u64,
                mime: None,
                dims: None,
                duration_ms: None,
            }),
        }])
        .expect("done");
        let dims = |h: &IngestHarness| -> (i64, i64) {
            h.connection()
                .query_row("SELECT width, height FROM attachments", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .expect("dims")
        };
        assert_eq!(dims(&h), (1, 9999), "until derived: the sender's claim");

        let account = h.account();
        let jobs = crate::ingest::pending_derives(h.connection(), account).expect("jobs");
        assert_eq!(jobs.len(), 1);
        let outcome = compute(&blobs, &jobs[0]);
        assert!(matches!(outcome, Outcome::Thumb { .. }));
        commit(
            h.connection_mut(),
            account,
            MsgIdScope::AccountAuthor,
            2_000,
            &jobs[0],
            outcome,
        )
        .expect("commit");
        assert_eq!(dims(&h), (64, 48), "the decoded file wins");
        let (thumb, blurhash): (bool, Option<String>) = h
            .connection()
            .query_row(
                "SELECT thumb_file_id IS NOT NULL, blurhash FROM attachments",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("row");
        assert!(thumb && blurhash.is_some());
        assert!(
            crate::ingest::pending_derives(h.connection(), account)
                .expect("jobs")
                .is_empty()
        );
    }

    /// A long text that arrives after the message was edited belongs to the original: revision
    /// 0 gets the whole text, the edit stays the message's text, and the attachment is gone
    /// from the rows and from every revision. Found again after a kill, too (it is a pending
    /// derive until then).
    #[test]
    fn a_long_text_joined_after_an_edit_goes_to_the_original() {
        use kourier_fake::attachments::synthetic_long_text;
        use kourier_model::{MsgTarget, NetMsgId};
        let dir = tempfile::tempdir().expect("tempdir");
        let blobs = dir.path().join("blobs");
        let alice = UserId::parse(ALICE).expect("id");
        let conv = ConvKey::Direct(alice.clone());
        let mut h = IngestHarness::open(
            &dir.path().join("core.db"),
            &kourier_db::DbKey::from([5; 32]),
            NetworkKind::Fake,
            MsgIdScope::AccountAuthor,
            &[UserId::parse(ME).expect("id")],
        )
        .expect("harness");
        let whole = "the whole text. ".repeat(200);
        let long = synthetic_long_text(&whole);
        let pointer = Pointer::from_blob(long.remote.as_ref().expect("remote")).expect("ours");
        let mut m = remote(&conv, &alice, "1000", 1_000, "");
        m.body = Body::Media {
            attachments: vec![long],
            caption: Some(Text::plain(&whole[..2048])),
        };
        let id = NetMsgId {
            author: Some(alice.clone()),
            local: "1000".into(),
        };
        h.apply(vec![
            BackendEvent::Message(m),
            BackendEvent::Edited {
                target: MsgTarget::to_id(Some(conv.clone()), &id),
                by: Some(alice.clone()),
                new_local: Some("2000".into()),
                body: Body::text("edited before the rest arrived"),
                edited_at: 2_000,
                keeps_quote: None,
            },
        ])
        .expect("apply");
        let kind = |h: &IngestHarness| -> String {
            h.connection()
                .query_row("SELECT body_kind FROM messages", [], |r| r.get(0))
                .expect("kind")
        };
        assert_eq!(kind(&h), "Media", "the edit kept the pending long text");

        let (att, path, key): (i64, String, Vec<u8>) = h
            .connection()
            .query_row(
                "SELECT attachment_id, path, key FROM attachment_downloads",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("the long text is fetched without a click");
        let plain = pointer.plaintext().expect("bytes").expect("ok");
        let target = blobs.join(&path);
        std::fs::create_dir_all(target.parent().expect("shard")).expect("shard dir");
        let key: [u8; 32] = key.try_into().expect("key");
        kourier_blob::write_all(&target, &FileKey::from_bytes(key), &plain).expect("blob");
        h.apply(vec![BackendEvent::AttachmentDone {
            attachment: AttachmentId(att),
            request: None,
            result: Ok(AttachmentMeta {
                size: plain.len() as u64,
                ..AttachmentMeta::default()
            }),
        }])
        .expect("done");

        let account = h.account();
        let jobs = crate::ingest::pending_derives(h.connection(), account).expect("jobs");
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].long_text);
        let outcome = compute(&blobs, &jobs[0]);
        assert!(matches!(&outcome, Outcome::LongText(t) if *t == whole));
        let ch = commit(
            h.connection_mut(),
            account,
            MsgIdScope::AccountAuthor,
            3_000,
            &jobs[0],
            outcome,
        )
        .expect("commit");
        assert_eq!(ch.unlink.len(), 1, "the file goes after the commit");
        assert!(ch.media_gone.contains(&AttachmentId(att)));

        let texts: Vec<String> = {
            let mut stmt = h
                .connection()
                .prepare("SELECT body FROM message_revisions ORDER BY revision")
                .expect("prepare");
            stmt.query_map([], |r| r.get::<_, String>(0))
                .expect("query")
                .map(
                    |b| match serde_json::from_str(&b.expect("row")).expect("body") {
                        Body::Text(t) => t.body,
                        other => panic!("an attachment is left: {other:?}"),
                    },
                )
                .collect()
        };
        assert_eq!(texts, [whole.as_str(), "edited before the rest arrived"]);
        let (body, revision): (String, i64) = h
            .connection()
            .query_row("SELECT body, revision FROM messages", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .expect("row");
        assert_eq!(
            serde_json::from_str::<Body>(&body).expect("body"),
            Body::text("edited before the rest arrived")
        );
        assert_eq!((kind(&h).as_str(), revision), ("Text", 1));
        let left: (i64, i64) = h
            .connection()
            .query_row(
                "SELECT (SELECT count(*) FROM attachments), (SELECT count(*) FROM files)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("counts");
        assert_eq!(left, (0, 0));
    }

    /// An imported edited message (`IncomingMessage.edits`) has no body of its original
    /// revision: the archive holds only the newest version. Its long text, fetched by itself
    /// (user decision, 2026-10-10), completes the NEWEST version, which is what the row shows;
    /// joined into revision 0 as for a live edit, the whole text went nowhere and the row
    /// kept its first 2 KiB.
    #[test]
    fn a_long_text_of_an_imported_edited_message_completes_its_newest_version() {
        use kourier_backend_api::HistoryEdit;
        use kourier_fake::attachments::synthetic_long_text;
        let dir = tempfile::tempdir().expect("tempdir");
        let blobs = dir.path().join("blobs");
        let alice = UserId::parse(ALICE).expect("id");
        let conv = ConvKey::Direct(alice.clone());
        let mut h = IngestHarness::open(
            &dir.path().join("core.db"),
            &kourier_db::DbKey::from([5; 32]),
            NetworkKind::Fake,
            MsgIdScope::AccountAuthor,
            &[UserId::parse(ME).expect("id")],
        )
        .expect("harness");
        let whole = "the whole text of the newest version. ".repeat(100);
        let long = synthetic_long_text(&whole);
        let pointer = Pointer::from_blob(long.remote.as_ref().expect("remote")).expect("ours");
        let mut m = remote(&conv, &alice, "1000", 1_000, "");
        m.body = Body::Media {
            attachments: vec![long],
            caption: Some(Text::plain(&whole[..2048])),
        };
        m.read = true;
        m.edits = vec![HistoryEdit {
            local: "2000".into(),
            at: 2_000,
        }];
        h.apply(vec![BackendEvent::HistoryBatch {
            conv: conv.clone(),
            messages: vec![m],
            has_more: false,
            progress: None,
        }])
        .expect("history");
        let (att, path, key): (i64, String, Vec<u8>) = h
            .connection()
            .query_row(
                "SELECT attachment_id, path, key FROM attachment_downloads",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("an imported long text is fetched without a click");
        let plain = pointer.plaintext().expect("bytes").expect("ok");
        let target = blobs.join(&path);
        std::fs::create_dir_all(target.parent().expect("shard")).expect("shard dir");
        let key: [u8; 32] = key.try_into().expect("key");
        kourier_blob::write_all(&target, &FileKey::from_bytes(key), &plain).expect("blob");
        h.apply(vec![BackendEvent::AttachmentDone {
            attachment: AttachmentId(att),
            request: None,
            result: Ok(AttachmentMeta {
                size: plain.len() as u64,
                ..AttachmentMeta::default()
            }),
        }])
        .expect("done");
        let account = h.account();
        let jobs = crate::ingest::pending_derives(h.connection(), account).expect("jobs");
        assert_eq!(jobs.len(), 1);
        let outcome = compute(&blobs, &jobs[0]);
        commit(
            h.connection_mut(),
            account,
            MsgIdScope::AccountAuthor,
            3_000,
            &jobs[0],
            outcome,
        )
        .expect("commit");

        let (body, kind): (String, String) = h
            .connection()
            .query_row("SELECT body, body_kind FROM messages", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .expect("row");
        assert_eq!(kind, "Text");
        assert_eq!(
            serde_json::from_str::<Body>(&body).expect("body"),
            Body::text(&whole),
            "the row shows the whole text"
        );
        let revisions: Vec<Option<String>> = {
            let mut stmt = h
                .connection()
                .prepare("SELECT body FROM message_revisions ORDER BY revision")
                .expect("prepare");
            stmt.query_map([], |r| r.get::<_, Option<String>>(0))
                .expect("query")
                .map(|b| b.expect("row"))
                .collect()
        };
        let newest = revisions
            .last()
            .cloned()
            .flatten()
            .expect("the newest has a body");
        assert_eq!(
            serde_json::from_str::<Body>(&newest).expect("body"),
            Body::text(&whole),
            "the newest revision holds the whole text"
        );
        let left: i64 = h
            .connection()
            .query_row("SELECT count(*) FROM attachments", [], |r| r.get(0))
            .expect("count");
        assert_eq!(left, 0);
    }
}
