//! Export Selected, the core's half (architecture §5.8.3): the plan a UI builds its documents
//! from ([`plan`], [`crate::facade::Query::ExportPlan`]) and the job that writes the archive
//! ([`run`], [`crate::facade::UiCommand::ExportArchive`]).
//!
//! The rules are written once in [`crate::actions`] (`may_export_chat`, `export_verdict`,
//! `may_save_message`, `may_save_file`): the plan states what they say, the command asks them
//! again on the rows as they are NOW ([`recheck`]) and fails the whole export when anything it
//! was told to include may not be exported any more. Plaintext leaves the store only through
//! `kourier_blob::archive` (the writer). Nothing here logs a name, a path or a text.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use kourier_db::rusqlite::{Connection, OptionalExtension};
use kourier_model::{
    AccountId, AttachmentId, Body, ConversationFeatures, ConversationId, MessageId, MsgIdScope,
    Placeholder, UserId,
};

use crate::actions::{self, ExportVerdict};
use crate::error::{Refusal, Result};
use crate::facade::{
    ExportDocument, ExportFailure, ExportFile, ExportFileRefusal, ExportHead, ExportOutcome,
    ExportPlan, ExportStamp, MessageActions, PlannedFile, PlannedMessage, Progress, ReplyPreview,
    RequestId, SaveMode, SaveTarget, UiEvent,
};
use crate::query;
use crate::runtime::Shared;
use crate::sql;

/// What is left free on the target's file system on top of the archive's content.
const SPACE_MARGIN: u64 = 4 * 1024 * 1024;
/// The least time and the least bytes between two [`Progress::Export`] events.
const PROGRESS_EVERY: Duration = Duration::from_millis(200);
const PROGRESS_BYTES: u64 = 1024 * 1024;

fn features_of(conn: &Connection, conv: ConversationId) -> Result<Option<ConversationFeatures>> {
    let features: Option<Option<String>> = conn
        .prepare_cached("SELECT features FROM conversations WHERE id = ?1")?
        .query_row([conv.0], |r| r.get(0))
        .optional()?;
    sql::from_json_opt(features.flatten())
}

/// Whether the chat is protected on another device (`conversations.protected`).
fn protected_of(conn: &Connection, conv: ConversationId) -> Result<bool> {
    Ok(conn
        .prepare_cached("SELECT protected FROM conversations WHERE id = ?1")?
        .query_row([conv.0], |r| r.get(0))
        .optional()?
        .unwrap_or(false))
}

/// The files of a body, as the save rule sees them.
fn files_of(body: &Body) -> Vec<&kourier_model::Attachment> {
    match body {
        Body::Media { attachments, .. } => attachments.iter().collect(),
        Body::Sticker(a) => vec![a.as_ref()],
        _ => Vec::new(),
    }
}

/// What the export's rule needs of one message row, read the same way for the plan and the
/// command: ONE predicate for "disappearing" (`expiry_kind IS NOT NULL`: the plan used to ask
/// the timer's milliseconds too, which could differ).
struct RowFacts {
    conv: i64,
    deleted: Option<String>,
    view_once: bool,
    hidden: bool,
    timer: bool,
    revision: u16,
}

impl RowFacts {
    fn verdict(&self) -> ExportVerdict {
        actions::export_verdict(
            self.deleted.as_deref(),
            self.view_once,
            self.hidden,
            self.timer,
        )
    }
}

fn row_facts(conn: &Connection, id: i64) -> Result<Option<RowFacts>> {
    Ok(conn
        .prepare_cached(
            "SELECT conversation_id, deleted, view_once, expiry_hidden, expiry_kind IS NOT NULL,
                    revision
             FROM messages WHERE id = ?1",
        )?
        .query_row([id], |r| {
            Ok(RowFacts {
                conv: r.get(0)?,
                deleted: r.get(1)?,
                view_once: r.get(2)?,
                hidden: r.get(3)?,
                timer: r.get(4)?,
                revision: u16::try_from(r.get::<_, i64>(5)?.max(0)).unwrap_or(u16::MAX),
            })
        })
        .optional()?)
}

fn restrictiveness(v: ExportVerdict) -> u8 {
    match v {
        ExportVerdict::Included => 0,
        ExportVerdict::NeedsTick => 1,
        ExportVerdict::LeftOut(_) => 2,
    }
}

/// THE place the quote rule lives (the lead's default until he answers question 1 of the
/// review: the quote of a view-once, hidden or disappearing original follows that original's
/// verdict). `None`: the message quotes nothing. The ORIGINAL's row is found by the quote's key,
/// hidden rows included ([`query::quote_original_facts`]); several rows (an ambiguous id) give
/// the most restrictive verdict. An original the core does not have is [`ExportVerdict::LeftOut`]
/// (`Gone`) when the core remembers the key as expired ([`crate::expiry::remembered`]), else
/// [`ExportVerdict::Included`]: the sender's snapshot, as the timeline shows it.
fn quote_verdict(
    conn: &Connection,
    conv: ConversationId,
    id: MessageId,
    scope: MsgIdScope,
    now: i64,
) -> Result<Option<ExportVerdict>> {
    let Some(facts) = query::quote_original_facts(conn, conv, id)? else {
        return Ok(None);
    };
    let known = facts
        .rows
        .iter()
        .map(|(d, v, h, t)| actions::export_verdict(d.as_deref(), *v, *h, *t))
        .max_by_key(|v| restrictiveness(*v));
    if let Some(v) = known {
        return Ok(Some(v));
    }
    let gone = crate::expiry::remembered(
        conn,
        AccountId(facts.account),
        scope,
        &facts.conv_key,
        facts.author.as_deref().unwrap_or(""),
        &facts.local,
        now,
    )?;
    Ok(Some(if gone {
        ExportVerdict::LeftOut(actions::LeftOut::Gone)
    } else {
        ExportVerdict::Included
    }))
}

/// A quote that is left out keeps that there was a quote and its author, nothing of its content.
fn strip_quote(r: &mut ReplyPreview) {
    r.snapshot = None;
    r.text = None;
    r.attachment = None;
    r.file_name = None;
    r.thumb = None;
}

/// [`crate::facade::Query::ExportPlan`]. The rows come THROUGH [`query::messages_by_ids`]; the
/// `sniffed` type of each file is left `None` here (the runtime reads the first bytes of the
/// files that are here, on the blocking pool).
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan(
    conn: &Connection,
    conv: ConversationId,
    ids: &[MessageId],
    now: i64,
    registered: bool,
    uploading: &HashSet<MessageId>,
    paused: Option<UserId>,
    scope: MsgIdScope,
) -> Result<ExportPlan> {
    let features = features_of(conn, conv)?;
    let protected = protected_of(conn, conv)?;
    let chat = actions::may_export_chat(features.as_ref(), protected)
        .map_err(|e| e.refusal().unwrap_or(Refusal::ExportUnstated));
    let members: i64 = conn
        .prepare_cached(
            "SELECT count(*) FROM members WHERE conversation_id = ?1
             AND role IN ('Member', 'Admin')",
        )?
        .query_row([conv.0], |r| r.get(0))?;
    let mut messages = Vec::new();
    // A chat that may not be exported plans no message.
    if chat.is_ok() {
        let page =
            query::messages_by_ids(conn, conv, ids, now, registered, uploading, paused, None)?;
        for mut row in page.rows {
            let Some(facts) = row_facts(conn, row.id.0)? else {
                continue;
            };
            let verdict = facts.verdict();
            let stamp = ExportStamp {
                id: row.id,
                deleted: row.deleted.is_some(),
                revision: row.revision,
            };
            if matches!(verdict, ExportVerdict::LeftOut(_)) {
                // A left-out row carries nothing of its content (M1): its stamp, times, author
                // and verdict. The body is the model's neutral placeholder `Expired`.
                row.body = Body::Placeholder(Placeholder::Expired);
                row.reply = None;
                row.reactions = Vec::new();
                row.author_avatar = None;
                row.device = None;
                row.edited_at = None;
                row.out_state = None;
                row.read_state = None;
                row.expiry = None;
                row.mentions_me = false;
                row.outbox = None;
                row.actions = MessageActions::default();
                row.revision = 0;
                messages.push(PlannedMessage {
                    row,
                    stamp,
                    verdict,
                    quote: None,
                    files: Vec::new(),
                    revisions: Vec::new(),
                });
                continue;
            }
            let quote = quote_verdict(conn, conv, row.id, scope, now)?;
            if matches!(quote, Some(ExportVerdict::LeftOut(_)))
                && let Some(reply) = row.reply.as_mut()
            {
                strip_quote(reply);
            }
            let files = files_of(&row.body)
                .into_iter()
                .filter_map(|a| {
                    let id = a.id?;
                    let accepted = actions::may_save_file(a.state, a.long_text).map_err(|_| {
                        if a.long_text {
                            ExportFileRefusal::LongText
                        } else {
                            ExportFileRefusal::NotHere
                        }
                    });
                    Some(PlannedFile {
                        attachment: id,
                        kind: a.kind,
                        mime: a.mime.clone(),
                        size: a.size,
                        file_name: a.file_name.clone(),
                        sniffed: None,
                        accepted,
                    })
                })
                .collect();
            let revisions = if row.revision > 0 {
                query::revisions(conn, row.id)?
                    .map(|r| r.revisions)
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            messages.push(PlannedMessage {
                row,
                stamp,
                verdict,
                quote,
                files,
                revisions,
            });
        }
    }
    Ok(ExportPlan {
        conv,
        chat,
        members: u32::try_from(members.max(0)).unwrap_or(u32::MAX),
        messages,
        now,
    })
}

/// The command's second look (step 3 and 4 of [`crate::facade::UiCommand::ExportArchive`]):
/// every listed message is in `conv`, still visible, and its verdict (with the tick) includes
/// it; every listed file belongs to a listed message and the save rule still accepts it.
/// `Ok(None)` is "changed"; else the plaintext size the store states for each file, in order.
pub(crate) fn recheck(
    conn: &Connection,
    conv: ConversationId,
    messages: &[ExportStamp],
    files: &[AttachmentId],
    with_disappearing: bool,
) -> Result<Option<Vec<u64>>> {
    // Deduplicated by id; two stamps of one id that differ are a changed export.
    let mut stamps: HashMap<i64, ExportStamp> = HashMap::new();
    for m in messages {
        if let Some(first) = stamps.insert(m.id.0, *m)
            && first != *m
        {
            return Ok(None);
        }
    }
    let listed: HashSet<i64> = stamps.keys().copied().collect();
    for (id, stamp) in &stamps {
        let Some(row) = row_facts(conn, *id)? else {
            return Ok(None);
        };
        // (Deleted for me: the timeline shows nothing of it.) A message deleted for everyone or
        // edited since the plan is not what the documents carry.
        if row.conv != conv.0
            || row.deleted.as_deref() == Some("ForMe")
            || row.deleted.is_some() != stamp.deleted
            || row.revision != stamp.revision
        {
            return Ok(None);
        }
        match row.verdict() {
            ExportVerdict::Included => {}
            ExportVerdict::NeedsTick if with_disappearing => {}
            _ => return Ok(None),
        }
    }
    let mut file = conn.prepare_cached(
        "SELECT a.message_id, a.state, a.long_text, a.size, m.deleted, m.view_once,
                m.expiry_hidden
         FROM attachments a JOIN messages m ON m.id = a.message_id WHERE a.id = ?1",
    )?;
    let mut sizes = Vec::with_capacity(files.len());
    for id in files {
        type Row = (i64, String, bool, Option<i64>, Option<String>, bool, bool);
        let row: Option<Row> = file
            .query_row([id.0], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })
            .optional()?;
        let Some((message_id, state, long_text, size, deleted, view_once, hidden)) = row else {
            return Ok(None);
        };
        let accepted = listed.contains(&message_id)
            && actions::may_save_message(deleted.as_deref(), view_once, hidden).is_ok()
            && actions::may_save_file(sql::from_json(&state)?, long_text).is_ok();
        if !accepted {
            return Ok(None);
        }
        sizes.push(u64::try_from(size.unwrap_or(0).max(0)).unwrap_or(0));
    }
    Ok(Some(sizes))
}

/// [`crate::facade::UiCommand::ExportArchive`]: the whole job; the caller emits the outcome.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    sh: &Arc<Shared>,
    request: RequestId,
    conv: ConversationId,
    target: SaveTarget,
    mode: SaveMode,
    with_disappearing: bool,
    messages: Vec<ExportStamp>,
    files: Vec<ExportFile>,
    documents: Vec<ExportDocument>,
    head: ExportHead,
    cancel: Arc<AtomicBool>,
) -> ExportOutcome {
    use ExportOutcome::{Cancelled, Failed};
    // (1) The names and an absolute folder.
    if kourier_blob::check_name(&target.name).is_err() || !target.dir.is_absolute() {
        return Failed(ExportFailure::NoFolder);
    }
    if cancel.load(Ordering::Relaxed) {
        return Cancelled;
    }
    // (2) The chat's CURRENT features.
    let (features, protected) = match sh
        .store
        .read(move |c| Ok((features_of(c, conv)?, protected_of(c, conv)?)))
        .await
    {
        Ok(f) => f,
        Err(_) => return Failed(ExportFailure::Io),
    };
    if let Err(e) = actions::may_export_chat(features.as_ref(), protected) {
        return Failed(ExportFailure::ChatRefused(
            e.refusal().unwrap_or(Refusal::ExportUnstated),
        ));
    }
    // (3), (4) The messages and the files, as they are now.
    let ids: Vec<AttachmentId> = files.iter().map(|f| f.attachment).collect();
    let by_entry: HashMap<String, AttachmentId> = files
        .iter()
        .map(|f| (f.entry.clone(), f.attachment))
        .collect();
    let distinct = messages
        .iter()
        .map(|m| m.id.0)
        .collect::<HashSet<_>>()
        .len();
    let listed = messages;
    let sizes = match sh
        .store
        .read(move |c| recheck(c, conv, &listed, &ids, with_disappearing))
        .await
    {
        Ok(Some(sizes)) => sizes,
        Ok(None) => return Failed(ExportFailure::Changed),
        Err(_) => return Failed(ExportFailure::Io),
    };
    // The blobs (a file that is not here any more fails the export like any other change).
    let mut items = Vec::with_capacity(files.len() + documents.len());
    let mut content = 0u64;
    for (file, size) in files.into_iter().zip(sizes) {
        let blob = match sh.media.resolve(file.attachment).await {
            Ok(Some(entry)) => entry.file,
            Ok(None) => None,
            Err(_) => return Failed(ExportFailure::Io),
        };
        let Some(blob) = blob else {
            return Failed(ExportFailure::Changed);
        };
        content = content.saturating_add(size);
        items.push(kourier_blob::ArchiveItem {
            name: file.entry,
            source: kourier_blob::Source::Blob {
                path: blob.path.clone(),
                key: kourier_blob::FileKey::from_bytes(*blob.key.expose()),
                size,
            },
        });
    }
    for doc in documents {
        content = content.saturating_add(doc.bytes.len() as u64);
        items.push(kourier_blob::ArchiveItem {
            name: doc.entry,
            source: kourier_blob::Source::Bytes(doc.bytes),
        });
    }
    if cancel.load(Ordering::Relaxed) {
        return Cancelled;
    }
    // (5) Room for it.
    let dir = target.dir.clone();
    if kourier_blob::free_space(&dir)
        .is_some_and(|free| free < content.saturating_add(SPACE_MARGIN))
    {
        return Failed(ExportFailure::NoSpace);
    }
    // (6), (7) The archive and its manifest, on the blocking pool.
    let (sh2, name, total) = (sh.clone(), target.name, content);
    let pause = sh.timeouts.export_entry_pause;
    let written = tokio::task::spawn_blocking(move || {
        let mut options = kourier_blob::ArchiveOptions::new(&cancel);
        options.mode = mode;
        options.entry_pause = pause;
        let (mut last, mut last_done) = (Instant::now(), 0u64);
        let mut progress = |done: u64| {
            if last.elapsed() >= PROGRESS_EVERY || done.saturating_sub(last_done) >= PROGRESS_BYTES
            {
                (last, last_done) = (Instant::now(), done);
                sh2.emit(UiEvent::Progress(Progress::Export {
                    request,
                    done: done.min(total),
                    total,
                }));
            }
        };
        let mut manifest = |entries: &[kourier_blob::EntryInfo], failed: &[String]| {
            let mut object = head.0.clone();
            object.insert(
                "entries".to_owned(),
                serde_json::Value::Array(
                    entries
                        .iter()
                        .map(|e| {
                            serde_json::json!({
                                "path": e.name, "size": e.size, "sha256": e.sha256,
                            })
                        })
                        .collect(),
                ),
            );
            object.insert("files_failed".to_owned(), serde_json::json!(failed));
            let bytes =
                serde_json::to_vec_pretty(&serde_json::Value::Object(object)).unwrap_or_default();
            ("manifest.json".to_owned(), bytes)
        };
        kourier_blob::write_archive(&dir, &name, items, &options, &mut progress, &mut manifest)
    })
    .await;
    match written {
        Err(_) => Failed(ExportFailure::Io),
        Ok(Err(kourier_blob::SaveError::Cancelled)) => Cancelled,
        Ok(Err(e)) => Failed(match e {
            kourier_blob::SaveError::Exists => ExportFailure::Exists,
            kourier_blob::SaveError::NoFolder => ExportFailure::NoFolder,
            kourier_blob::SaveError::NotWritable => ExportFailure::NotWritable,
            kourier_blob::SaveError::NoSpace => ExportFailure::NoSpace,
            _ => ExportFailure::Io,
        }),
        Ok(Ok(report)) => {
            sh.emit(UiEvent::Progress(Progress::Export {
                request,
                done: total,
                total,
            }));
            let failed: Vec<AttachmentId> = report
                .failed
                .iter()
                .filter_map(|name| by_entry.get(name).copied())
                .collect();
            let in_archive = report
                .entries
                .iter()
                .filter(|e| e.name.starts_with("files/"))
                .count();
            ExportOutcome::Done {
                messages: u32::try_from(distinct).unwrap_or(u32::MAX),
                files: u32::try_from(in_archive).unwrap_or(u32::MAX),
                failed,
                bytes: report.bytes,
            }
        }
    }
}

/// The running exports' cancel flags, by request.
#[derive(Default)]
pub(crate) struct Running(std::sync::Mutex<HashMap<RequestId, Arc<AtomicBool>>>);

impl Running {
    /// A flag for a new export; `None` when `request` is running already (the first keeps its
    /// flag, so it can still be cancelled).
    pub(crate) fn start(&self, request: RequestId) -> Option<Arc<AtomicBool>> {
        let mut map = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if map.contains_key(&request) {
            return None;
        }
        let flag = Arc::new(AtomicBool::new(false));
        map.insert(request, flag.clone());
        Some(flag)
    }

    /// The export ended.
    pub(crate) fn finish(&self, request: RequestId) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&request);
    }

    /// Asks the export to stop (nothing for an unknown one).
    pub(crate) fn cancel(&self, request: RequestId) {
        if let Some(flag) = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&request)
        {
            flag.store(true, Ordering::Relaxed);
        }
    }
}
