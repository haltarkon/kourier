//! [`UiCommand::Forward`] (forward line F3, architecture §4.8): the plan is read ONCE, every
//! target then gets ONE job in its conversation's compose queue, and a joiner ends the request
//! with one [`UiEvent::ForwardEnded`].
//!
//! A job converts the going messages for its target ([`crate::forward::convert`]), copies the
//! blobs of their files into the TARGET account's blob store on the blocking pool (never inside
//! a write transaction), then stores everything in as many transactions as it needs
//! ([`outbox::enqueue_forward`]: the first is the G4 gate, `Part::First`, the others
//! `Part::Next`): all of it or none for that target. Whatever fails, the copies and the staged
//! rows are removed. Nothing here logs a text, a name, a number or a path.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use kourier_backend_api::Network;
use kourier_blob::{BlobId, BlobStore, FileKey};
use kourier_db::Priority;
use kourier_db::rusqlite::OptionalExtension;
use kourier_model::{
    AccountId, AttachmentId, AttachmentKind, BackendBlob, CannotSend, ConversationId, ForwardRules,
    Forwarded, MessageId, Text, UserId,
};
use tokio::sync::{mpsc, watch};

use super::{Queues, Subject, emit_changes, network};
use crate::attachments::import::{self, Imported, Written};
use crate::attachments::{core_path, core_store};
use crate::error::{CoreError, Refusal, Result};
use crate::facade::{
    ForwardLook, ForwardPart, ForwardPlan, ForwardRefusal, MessageRow, RequestId, UiEvent,
};
use crate::forward::{How, TargetFacts, convert, mention_names};
use crate::store::lane;
use crate::{outbox, query};

use crate::actions::ForwardVerdict;
use crate::dispatch::Signal;
use crate::supervisor::Control;

/// A thumbnail's path, key, size and type.
type SrcThumb = (PathBuf, [u8; 32], Option<u64>, Option<String>);

/// A file of a source message as the core stores it: what a copy needs.
#[derive(Clone)]
pub(super) struct SrcFile {
    path: Option<PathBuf>,
    key: Option<[u8; 32]>,
    size: Option<u64>,
    mime: Option<String>,
    thumb: Option<SrcThumb>,
    remote: Option<BackendBlob>,
}

/// What one read of the source gives (the query arm uses `plan` and `media` only).
pub(super) struct PlanRead {
    pub plan: ForwardPlan,
    pub media: Vec<query::RawMedia>,
    names: HashMap<UserId, String>,
    scores: HashMap<MessageId, Option<Forwarded>>,
    files: HashMap<AttachmentId, SrcFile>,
    src_account: Option<AccountId>,
}

fn key32(raw: Option<Vec<u8>>) -> Option<[u8; 32]> {
    raw?.try_into().ok()
}

/// Reads the plan (and, with `full`, what a forward of it needs of the source's rows). The one
/// read both [`Query::ForwardPlan`](crate::facade::Query::ForwardPlan) and the command use.
pub(super) async fn read_plan(
    sh: &Arc<super::Shared>,
    source: ConversationId,
    messages: Vec<MessageId>,
    targets: Vec<ConversationId>,
    full: bool,
) -> Result<PlanRead> {
    let now = sh.now();
    let sh2 = sh.clone();
    let uploading: std::collections::HashSet<MessageId> = sh
        .uploading
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter(|(_, (_, c))| *c == source)
        .map(|(m, _)| *m)
        .collect();
    let paused = sh
        .paused
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&source)
        .map(|(_, _, user)| user.clone());
    sh.store
        .read(move |c| {
            let registry = |kind| {
                sh2.networks.get(kind).map(|n| {
                    let d = n.descriptor();
                    crate::forward::NetFacts {
                        contact_cards: d.contact_cards,
                        forwards: d.forwards,
                    }
                })
            };
            let (plan, page) = crate::forward::plan(
                c, source, &messages, &targets, now, &uploading, paused, &registry,
            )?;
            let media = query::page_media(c, &page)?;
            let mut read = PlanRead {
                plan,
                media,
                names: HashMap::new(),
                scores: HashMap::new(),
                files: HashMap::new(),
                src_account: None,
            };
            if !full {
                return Ok(read);
            }
            let src = outbox::network_of(c, source)?;
            read.src_account = src.map(|(a, _)| a);
            let going: Vec<&MessageRow> = going_rows(&read.plan);
            read.names = mention_names(c, read.src_account, &going)?;
            for row in &going {
                let score: Option<Option<i64>> = c
                    .prepare_cached("SELECT forwarded FROM messages WHERE id = ?1")?
                    .query_row([row.id.0], |r| r.get(0))
                    .optional()?;
                let mark = score.flatten().map(|s| Forwarded {
                    score: u32::try_from(s).unwrap_or(u32::MAX),
                });
                read.scores.insert(row.id, mark);
                for a in crate::forward::files_of(&row.body) {
                    let Some(id) = a.id else { continue };
                    type Raw = (
                        Option<String>,
                        Option<Vec<u8>>,
                        Option<i64>,
                        Option<String>,
                        Option<String>,
                        Option<Vec<u8>>,
                        Option<i64>,
                        Option<String>,
                        Option<Vec<u8>>,
                    );
                    let raw: Option<Raw> = c
                        .prepare_cached(
                            "SELECT f.path, f.key, f.size, f.mime, t.path, t.key, t.size, t.mime,
                               a.remote
                             FROM attachments a
                               LEFT JOIN files f ON f.id = a.file_id
                               LEFT JOIN files t ON t.id = a.thumb_file_id
                             WHERE a.id = ?1",
                        )?
                        .query_row([id.0], |r| {
                            Ok((
                                r.get(0)?,
                                r.get(1)?,
                                r.get(2)?,
                                r.get(3)?,
                                r.get(4)?,
                                r.get(5)?,
                                r.get(6)?,
                                r.get(7)?,
                                r.get(8)?,
                            ))
                        })
                        .optional()?;
                    let Some((fp, fk, fs, fm, tp, tk, ts, tm, remote)) = raw else {
                        continue;
                    };
                    let thumb = match (tp, key32(tk)) {
                        (Some(p), Some(k)) => Some((
                            PathBuf::from(p),
                            k,
                            ts.and_then(|s| u64::try_from(s).ok()),
                            tm,
                        )),
                        _ => None,
                    };
                    read.files.insert(
                        id,
                        SrcFile {
                            path: fp.map(PathBuf::from),
                            key: key32(fk),
                            size: fs.and_then(|s| u64::try_from(s).ok()),
                            mime: fm,
                            thumb,
                            remote: crate::sql::blob_from_sql(remote),
                        },
                    );
                }
            }
            Ok(read)
        })
        .await
}

/// The rows that go, in timeline order.
fn going_rows(plan: &ForwardPlan) -> Vec<&MessageRow> {
    plan.messages
        .iter()
        .filter(|m| m.verdict == ForwardVerdict::Goes)
        .filter_map(|m| m.row.as_ref())
        .collect()
}

/// What every job of one command shares.
struct Ctx {
    read: PlanRead,
    look: ForwardLook,
    headers: HashMap<MessageId, Text>,
    comment: Option<Text>,
    pictures: Vec<PathBuf>,
    without: Vec<AttachmentId>,
}

/// The command's arguments.
pub(super) struct Ask {
    pub request: RequestId,
    pub source: ConversationId,
    pub messages: Vec<MessageId>,
    pub targets: Vec<ConversationId>,
    pub look: ForwardLook,
    pub headers: Vec<(MessageId, Text)>,
    pub comment: Option<Text>,
    pub pictures: Vec<PathBuf>,
    pub without: Vec<AttachmentId>,
}

/// Test door (doc-hidden through the facade): the next forward job of this conversation fails
/// right after its first part was stored, so the "nothing stays" path can be driven.
static FAULT_AFTER_FIRST_PART: Mutex<Option<ConversationId>> = Mutex::new(None);

pub(crate) fn inject_fault_after_first_part(conv: Option<ConversationId>) {
    *FAULT_AFTER_FIRST_PART
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = conv;
}

/// Armed only where the kill points are (`kourier_db::failpoint::ENABLED`: debug builds, or the
/// `failpoints` feature); a release build never takes the door.
fn fault_due(conv: ConversationId) -> bool {
    if !kourier_db::failpoint::ENABLED {
        return false;
    }
    let mut f = FAULT_AFTER_FIRST_PART
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if *f == Some(conv) {
        *f = None;
        true
    } else {
        false
    }
}

/// What the planner task hands the jobs.
struct Prepared {
    ctx: Option<Ctx>,
    /// Per target, in the order of the plan: its refusal, or `None` and whether it is native.
    verdicts: HashMap<ConversationId, (Option<ForwardRefusal>, bool)>,
}

/// Starts the command: the jobs are queued HERE, in the order the UI sent its commands (one
/// per distinct target, in that chat's compose lane); a task reads the plan once for all of
/// them, and a joiner ends the request with one [`UiEvent::ForwardEnded`].
pub(super) fn start(
    sh: &Arc<super::Shared>,
    queues: &Queues,
    senders: HashMap<AccountId, mpsc::UnboundedSender<Control>>,
    ask: Ask,
) {
    let request = ask.request;
    let mut order: Vec<ConversationId> = Vec::new();
    for t in &ask.targets {
        if !order.contains(t) {
            order.push(*t);
        }
    }
    let (ptx, prx) = watch::channel::<Option<Arc<Prepared>>>(None);
    let (rtx, mut rrx) = mpsc::unbounded_channel::<(ConversationId, Option<ForwardRefusal>)>();
    for conv in &order {
        let conv = *conv;
        let (sh2, senders2, tx2, mut rx2) = (sh.clone(), senders.clone(), rtx.clone(), prx.clone());
        queues.compose(sh, Subject::Conversation(conv), async move {
            let prepared = match rx2.wait_for(|p| p.is_some()).await {
                Ok(p) => p.clone(),
                Err(_) => None,
            };
            let res = match prepared {
                None => Some(ForwardRefusal::Failed),
                Some(p) => match (p.verdicts.get(&conv), &p.ctx) {
                    (Some((Some(why), _)), _) => Some(why.clone()),
                    (Some((None, native)), Some(ctx)) => {
                        job(&sh2, &senders2, ctx, conv, *native).await.err()
                    }
                    _ => Some(ForwardRefusal::Failed),
                },
            };
            let _ = tx2.send((conv, res));
        });
    }
    drop(rtx);
    // The planner: one read of the source for every job.
    let sh3 = sh.clone();
    let (ask_messages, ask_targets) = (ask.messages.clone(), order.clone());
    tokio::spawn(async move {
        let read = read_plan(&sh3, ask.source, ask_messages.clone(), ask_targets, true).await;
        let prepared = match read {
            Err(e) => {
                tracing::warn!(error = %e, "reading a forward's plan failed");
                Prepared {
                    ctx: None,
                    verdicts: HashMap::new(),
                }
            }
            Ok(read) => {
                let all_go = ask_messages.iter().all(|id| {
                    read.plan
                        .messages
                        .iter()
                        .find(|m| m.id == *id)
                        .is_some_and(|m| m.verdict == ForwardVerdict::Goes)
                });
                let file_missing = read
                    .plan
                    .to_download
                    .iter()
                    .chain(&read.plan.lost)
                    .any(|id| !ask.without.contains(id));
                let picture_missing = ask.look == ForwardLook::Picture && ask.pictures.is_empty();
                let mut verdicts = HashMap::new();
                for t in &read.plan.targets {
                    let why = if let Some(r) = &t.refused {
                        Some(r.clone())
                    } else if !all_go {
                        Some(ForwardRefusal::SourceChanged)
                    } else if file_missing {
                        Some(ForwardRefusal::FileNotHere)
                    } else if picture_missing && !t.native {
                        Some(ForwardRefusal::NothingToSend)
                    } else {
                        None
                    };
                    verdicts.insert(t.conv, (why, t.native));
                }
                Prepared {
                    ctx: Some(Ctx {
                        look: ask.look,
                        headers: ask.headers.into_iter().collect(),
                        comment: ask.comment.filter(|t| !t.body.trim().is_empty()),
                        pictures: ask.pictures,
                        without: ask.without,
                        read,
                    }),
                    verdicts,
                }
            }
        };
        let _ = ptx.send(Some(Arc::new(prepared)));
    });
    let sh4 = sh.clone();
    tokio::spawn(async move {
        let mut done: HashMap<ConversationId, Option<ForwardRefusal>> = HashMap::new();
        while done.len() < order.len() {
            match rrx.recv().await {
                Some((c, r)) => {
                    done.insert(c, r);
                }
                // A job that never ran (the core is stopping).
                None => break,
            }
        }
        let (mut sent, mut refused) = (Vec::new(), Vec::new());
        for c in order {
            match done.remove(&c) {
                Some(None) => sent.push(c),
                Some(Some(why)) => refused.push((c, why)),
                None => refused.push((c, ForwardRefusal::Failed)),
            }
        }
        for (c, why) in &refused {
            tracing::info!(conv = c.0, refusal = ?why, "a forward target was refused");
        }
        sh4.emit(UiEvent::ForwardEnded {
            request,
            sent,
            refused,
        });
    });
}

fn failed(e: &CoreError) -> ForwardRefusal {
    tracing::warn!(error = %e, "a forward target failed");
    ForwardRefusal::Failed
}

/// The refusal a failed transaction stands for (the gate is the truth: the plan was a read).
async fn refusal_of(
    sh: &Arc<super::Shared>,
    conv: ConversationId,
    e: &CoreError,
) -> ForwardRefusal {
    match e {
        CoreError::UnknownConversation(_) => ForwardRefusal::UnknownChat,
        CoreError::InvalidState(outbox::NOT_LINKED) => ForwardRefusal::AccountNotLinked,
        CoreError::Refused {
            reason: Refusal::Blocked,
            ..
        } => ForwardRefusal::CannotSend(CannotSend::Blocked),
        CoreError::Refused {
            reason: Refusal::CannotSend,
            ..
        } => {
            let why = sh
                .store
                .read(move |c| outbox::features_of(c, conv))
                .await
                .ok()
                .flatten()
                .and_then(|f| f.cannot_send);
            match why {
                Some(c) => ForwardRefusal::CannotSend(c),
                None => failed(e),
            }
        }
        other => failed(other),
    }
}

/// A converted piece before the blobs are copied: files by source attachment.
enum Draft {
    Text(Text),
    Files {
        files: Vec<(AttachmentId, bool)>,
        caption: Option<Text>,
    },
    Contact(kourier_model::ContactCard),
}

/// One target. `Ok` = everything is stored and queued; `Err` = nothing of it stays.
async fn job(
    sh: &Arc<super::Shared>,
    senders: &HashMap<AccountId, mpsc::UnboundedSender<Control>>,
    shared: &Ctx,
    conv: ConversationId,
    native: bool,
) -> std::result::Result<(), ForwardRefusal> {
    let facts = sh
        .store
        .read(move |c| crate::forward::target_facts(c, conv))
        .await
        .map_err(|e| failed(&e))?
        .ok_or(ForwardRefusal::UnknownChat)?;
    let (account, kind) = (facts.account, facts.kind);
    let net: Arc<dyn Network> = network(sh, kind).map_err(|e| failed(&e))?;
    let descriptor = net.descriptor();
    let rules: Option<ForwardRules> = descriptor.forwards;
    let target = TargetFacts {
        features: facts.features.as_ref(),
        contact_cards: descriptor.contact_cards,
    };
    let name_of = |u: &UserId| {
        shared
            .read
            .names
            .get(u)
            .cloned()
            .unwrap_or_else(|| "someone".to_owned())
    };

    // What goes, as drafts with the mark each message carries.
    let mut drafts: Vec<(Draft, Option<Forwarded>)> = Vec::new();
    let pictures = !native && shared.look == ForwardLook::Picture;
    if !pictures {
        if let Some(c) = &shared.comment {
            drafts.push((Draft::Text(c.clone()), None));
        }
        for row in going_rows(&shared.read.plan) {
            let header = shared.headers.get(&row.id);
            let converted = convert(
                &row.body,
                &target,
                How {
                    native,
                    look: shared.look,
                    header,
                    names: &name_of,
                },
            );
            // The mark: only a native forward on a network that states one.
            let mark = (native && rules.is_some())
                .then(|| Forwarded::next(shared.read.scores.get(&row.id).copied().flatten()));
            for part in converted.parts {
                let draft = match part {
                    ForwardPart::Text(t) => Draft::Text(t),
                    ForwardPart::Contact(c) => Draft::Contact(c),
                    ForwardPart::Files { files, caption } => {
                        let files: Vec<(AttachmentId, bool)> = files
                            .into_iter()
                            .filter(|f| !shared.without.contains(&f.attachment))
                            .map(|f| (f.attachment, f.as_file))
                            .collect();
                        if files.is_empty() {
                            match caption {
                                Some(t) => Draft::Text(t),
                                None => continue,
                            }
                        } else {
                            Draft::Files { files, caption }
                        }
                    }
                };
                drafts.push((draft, mark));
            }
        }
    }
    if drafts.is_empty() && !pictures {
        return Err(ForwardRefusal::NothingToSend);
    }

    // The copies, outside any transaction, in the TARGET account's blob store.
    let dir = sh.blob_dir_of(account).await.map_err(|e| failed(&e))?;
    let imported: Vec<Imported>;
    let mut pieces: Vec<outbox::MarkedPiece> = Vec::new();
    if pictures {
        let paths = shared.pictures.clone();
        let n = paths.len();
        let (d, agreed) = (dir.clone(), vec![true; n]);
        let done = tokio::task::spawn_blocking(move || {
            let location = import::LocationRule {
                agreed: &agreed,
                cleaned: None,
            };
            import::import_files(&d, &paths, &[], location, None, &mut |_| {})
        })
        .await
        .map_err(|_| ForwardRefusal::Failed)?
        .map_err(|e| match e {
            import::ImportError::Refused(_) => ForwardRefusal::NothingToSend,
            import::ImportError::Other(e) => failed(&e),
        })?;
        imported = done;
        pieces.push(outbox::MarkedPiece {
            piece: outbox::Piece::Files {
                files: (0..imported.len()).collect(),
                caption: shared.comment.clone(),
            },
            forward: None,
        });
    } else {
        // The files to copy, in the order the pieces use them.
        let mut wanted: Vec<(AttachmentId, bool)> = Vec::new();
        let mut spans: Vec<Option<Vec<usize>>> = Vec::new();
        for (d, _) in &drafts {
            match d {
                Draft::Files { files, .. } => {
                    let at: Vec<usize> = files
                        .iter()
                        .map(|f| {
                            wanted.push(*f);
                            wanted.len() - 1
                        })
                        .collect();
                    spans.push(Some(at));
                }
                _ => spans.push(None),
            }
        }
        let mut sources: Vec<(AttachmentId, bool, SrcFile, kourier_model::Attachment)> = Vec::new();
        for (id, as_file) in &wanted {
            let src = shared
                .read
                .files
                .get(id)
                .cloned()
                .ok_or(ForwardRefusal::FileNotHere)?;
            let attachment = attachment_of(shared, *id).ok_or(ForwardRefusal::FileNotHere)?;
            sources.push((*id, *as_file, src, attachment));
        }
        let src_dir = match shared.read.src_account {
            Some(a) => sh.blob_dir_of(a).await.map_err(|e| failed(&e))?,
            None => return Err(ForwardRefusal::UnknownChat),
        };
        let (d, keep_remote) = (dir.clone(), native);
        imported = if sources.is_empty() {
            Vec::new()
        } else {
            tokio::task::spawn_blocking(move || copy_all(&src_dir, &d, sources, keep_remote))
                .await
                .map_err(|_| ForwardRefusal::Failed)??
        };
        for ((d, mark), span) in drafts.into_iter().zip(spans) {
            let piece = match (d, span) {
                (Draft::Text(t), _) => outbox::Piece::Text(t),
                (Draft::Contact(c), _) => outbox::Piece::Contact(c),
                (Draft::Files { caption, .. }, Some(at)) => {
                    outbox::Piece::Files { files: at, caption }
                }
                (Draft::Files { .. }, None) => continue,
            };
            pieces.push(outbox::MarkedPiece {
                piece,
                forward: mark,
            });
        }
    }

    // Stored in as many transactions as it needs; all of it or none.
    let files: Arc<[Imported]> = imported.into();
    let (stored, now, n) = (files.clone(), sh.now(), net.clone());
    let mut step = sh
        .store
        .write(Priority::High, lane(account), move |c| {
            Ok(outbox::enqueue_forward(
                c,
                n.as_ref(),
                conv,
                stored,
                pieces,
                now,
            ))
        })
        .await
        .map_err(outbox::FilesError::Other)
        .and_then(|s| s);
    let mut staged: Vec<MessageId> = Vec::new();
    let (written, _messages) = loop {
        match step {
            Ok(outbox::FilesStep::Done(written, messages)) => break (written, messages),
            Ok(outbox::FilesStep::More(send)) => {
                staged.clear();
                staged.extend_from_slice(send.stored());
                if fault_due(conv) {
                    step = Err(outbox::FilesError::Other(CoreError::InvalidState(
                        "an injected fault",
                    )));
                    continue;
                }
                let (now, n) = (sh.now(), net.clone());
                step = sh
                    .store
                    .write(Priority::High, lane(account), move |c| {
                        outbox::enqueue_files_more(c, n.as_ref(), send, now)
                    })
                    .await
                    .map_err(outbox::FilesError::Other);
            }
            Err(e) => {
                if !staged.is_empty() {
                    let (now, scope) = (sh.now(), net.descriptor().msg_id_scope);
                    let dropped = sh
                        .store
                        .write(Priority::High, lane(account), move |c| {
                            outbox::discard_staged(c, scope, &staged, now)
                        })
                        .await;
                    match dropped {
                        Ok(ch) => sh.publish_local(account, &dir, ch).await,
                        Err(e) => tracing::warn!(
                            account = account.0,
                            error = %e,
                            "dropping a half-stored forward failed"
                        ),
                    }
                }
                let d = dir.clone();
                let _ = tokio::task::spawn_blocking(move || import::discard(&d, &files)).await;
                let e = match e {
                    outbox::FilesError::Refused(_) => CoreError::InvalidInput("a file is refused"),
                    outbox::FilesError::Other(e) => e,
                };
                return Err(refusal_of(sh, conv, &e).await);
            }
        }
    };
    emit_changes(sh, &written.changes);
    if let Some(tx) = senders.get(&written.account) {
        let _ = tx.send(Control::Outbox(Signal::Kick));
    }
    Ok(())
}

/// The source attachment (its metadata) of an id of a going message.
fn attachment_of(shared: &Ctx, id: AttachmentId) -> Option<kourier_model::Attachment> {
    going_rows(&shared.read.plan)
        .into_iter()
        .flat_map(|r| crate::forward::files_of(&r.body))
        .find(|a| a.id == Some(id))
        .cloned()
}

/// Copies every wanted file (and thumbnail) byte for byte from the source account's blob
/// directory into the target's, as [`Imported`]s; on any failure removes what was copied.
fn copy_all(
    src_dir: &std::path::Path,
    dir: &std::path::Path,
    sources: Vec<(AttachmentId, bool, SrcFile, kourier_model::Attachment)>,
    keep_remote: bool,
) -> std::result::Result<Vec<Imported>, ForwardRefusal> {
    let store = core_store(dir).map_err(|e| failed(&CoreError::Blob(e.into())))?;
    let mut done: Vec<Imported> = Vec::with_capacity(sources.len());
    for (_, as_file, src, a) in sources {
        let one = (|| {
            let (Some(path), Some(key)) = (src.path.as_ref(), src.key) else {
                return Err(ForwardRefusal::FileNotHere);
            };
            let id = copy_blob(src_dir, path, &store)?;
            let file = Written {
                path: core_path(&id),
                key: FileKey::from_bytes(key),
                size: src.size.unwrap_or(a.size),
                mime: src.mime.clone().unwrap_or_else(|| a.mime.clone()),
            };
            // A thumbnail is only for a file that goes as what it is.
            let thumb = match (&src.thumb, as_file) {
                (Some((tp, tk, ts, tm)), false) => match copy_blob(src_dir, tp, &store) {
                    Ok(tid) => Some(Written {
                        path: core_path(&tid),
                        key: FileKey::from_bytes(*tk),
                        size: ts.unwrap_or(0),
                        mime: tm.clone().unwrap_or_default(),
                    }),
                    // A thumbnail is derived again (`derive::repair_thumbs`).
                    Err(_) => None,
                },
                _ => None,
            };
            Ok(Imported {
                kind: if as_file {
                    AttachmentKind::File
                } else {
                    a.kind
                },
                content: a.kind,
                file_name: a.file_name.clone(),
                dims: if as_file { None } else { a.dims },
                blurhash: if as_file { None } else { a.blurhash.clone() },
                file,
                thumb,
                remote: if keep_remote && !as_file {
                    src.remote.clone()
                } else {
                    None
                },
            })
        })();
        match one {
            Ok(i) => done.push(i),
            Err(e) => {
                import::discard(dir, &done);
                return Err(e);
            }
        }
    }
    if let Err(e) = import::make_durable(dir, &done) {
        import::discard(dir, &done);
        return Err(failed(&CoreError::Blob(e.into())));
    }
    Ok(done)
}

/// One encrypted blob copied as it is (same key) to a new id of the target's store: written
/// under a `.tmp` name and renamed, so a kill leaves nothing the sweep would keep.
fn copy_blob(
    src_dir: &std::path::Path,
    rel: &std::path::Path,
    store: &BlobStore,
) -> std::result::Result<BlobId, ForwardRefusal> {
    let id = BlobId::new_random();
    let io = |e: std::io::Error| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ForwardRefusal::FileNotHere
        } else {
            tracing::warn!(error = %e, "copying a blob for a forward failed");
            ForwardRefusal::Failed
        }
    };
    let dest = store.prepare(&id).map_err(io)?;
    let mut tmp = dest.clone().into_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    if let Err(e) = std::fs::copy(src_dir.join(rel), &tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(io(e));
    }
    if let Err(e) = std::fs::rename(&tmp, &dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(io(e));
    }
    Ok(id)
}
