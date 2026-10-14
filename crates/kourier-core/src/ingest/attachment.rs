//! Attachment rows in ingest (§5.8): the auto-download decision for new rows, download requests
//! with fresh blob targets, `AttachmentDone`, and deleting rows with their files. The state
//! machine is described in [`crate::attachments`].

use std::path::PathBuf;

use kourier_blob::{BlobId, FileKey};
use kourier_db::rusqlite::{Connection, OptionalExtension, params};
use kourier_model::{
    AccountCaps, AccountId, AccountLocalPrefs, AttachmentId, AttachmentKind, AttachmentMeta,
    AttachmentState, BackendError, BlobKey, Body, ConversationId, ErrorKind, MessageId,
};

use super::Apply;
use crate::attachments::policy::{self, Candidate, Click, Decision};
use crate::attachments::{DeriveJob, DownloadJob, MAX_DOWNLOAD_RESENDS};
use crate::error::{CoreError, Refusal, Result};
use crate::sql;

/// One attachment row as the download rules see it.
pub(super) struct AttRow {
    pub(super) id: AttachmentId,
    pub(super) message: MessageId,
    pub(super) conv: ConversationId,
    pub(super) kind: AttachmentKind,
    pub(super) size: u64,
    pub(super) mime: Option<String>,
    pub(super) state: AttachmentState,
    pub(super) remote: Option<kourier_model::BackendBlob>,
    pub(super) file_id: Option<i64>,
    pub(super) sticker: bool,
    pub(super) long_text: bool,
    /// Its place in the message's attachments (`attachments.idx`).
    pub(super) idx: usize,
    /// Its pointer came from the other device's answer (`attachments.restored`).
    pub(super) restored: bool,
}

/// Who started a download (`attachment_downloads.source`): only a click that ends "no longer
/// available" asks the other device at once (`ingest::restore`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Source {
    /// The user clicked.
    User,
    /// The auto-download policy.
    Auto,
    /// The other device's answer to a request the user started.
    Restore,
}

impl Source {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Auto => "auto",
            Self::Restore => "restore",
        }
    }
}

const ATT_SELECT: &str = "SELECT a.id, a.message_id, m.conversation_id, a.kind, a.size, a.mime,
    a.state, a.remote, a.file_id, m.body_kind, a.long_text, a.idx, a.restored
    FROM attachments a JOIN messages m ON m.id = a.message_id";

fn att_row(r: &kourier_db::rusqlite::Row<'_>) -> kourier_db::rusqlite::Result<RawAtt> {
    Ok(RawAtt {
        id: r.get(0)?,
        message: r.get(1)?,
        conv: r.get(2)?,
        kind: r.get(3)?,
        size: r.get(4)?,
        mime: r.get(5)?,
        state: r.get(6)?,
        remote: r.get(7)?,
        file_id: r.get(8)?,
        body_kind: r.get(9)?,
        long_text: r.get(10)?,
        idx: r.get(11)?,
        restored: r.get(12)?,
    })
}

struct RawAtt {
    id: i64,
    message: i64,
    conv: i64,
    kind: String,
    size: Option<i64>,
    mime: Option<String>,
    state: String,
    remote: Option<Vec<u8>>,
    file_id: Option<i64>,
    body_kind: String,
    long_text: bool,
    idx: i64,
    restored: bool,
}

impl RawAtt {
    fn parse(self) -> Result<AttRow> {
        Ok(AttRow {
            id: AttachmentId(self.id),
            message: MessageId(self.message),
            conv: ConversationId(self.conv),
            kind: sql::from_enum_name(&self.kind)?,
            size: self.size.and_then(|s| u64::try_from(s).ok()).unwrap_or(0),
            mime: self.mime,
            state: sql::from_json(&self.state)?,
            remote: sql::blob_from_sql(self.remote),
            file_id: self.file_id,
            sticker: self.body_kind == "Sticker",
            long_text: self.long_text,
            idx: usize::try_from(self.idx).unwrap_or(usize::MAX),
            restored: self.restored,
        })
    }
}

pub(super) fn load_att(
    conn: &Connection,
    account: AccountId,
    id: AttachmentId,
) -> Result<Option<AttRow>> {
    conn.prepare_cached(&format!(
        "{ATT_SELECT} WHERE a.id = ?1 AND m.account_id = ?2"
    ))?
    .query_row((id.0, account.0), att_row)
    .optional()?
    .map(RawAtt::parse)
    .transpose()
}

/// Every attachment of `message`, in its order (`attachments.idx`).
pub(super) fn message_atts(conn: &Connection, message: MessageId) -> Result<Vec<AttRow>> {
    conn.prepare_cached(&format!(
        "{ATT_SELECT} WHERE a.message_id = ?1 ORDER BY a.idx"
    ))?
    .query_map([message.0], att_row)?
    .collect::<kourier_db::rusqlite::Result<Vec<_>>>()?
    .into_iter()
    .map(RawAtt::parse)
    .collect()
}

/// The account's auto-download preferences and the network's receive limit.
pub(super) fn download_prefs(
    conn: &Connection,
    account: AccountId,
) -> Result<(kourier_model::AutoDownloadPolicy, Option<u64>)> {
    let (prefs, caps): (String, Option<String>) = conn
        .prepare_cached("SELECT local_prefs, caps FROM accounts WHERE id = ?1")?
        .query_row([account.0], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let prefs: AccountLocalPrefs = sql::from_json(&prefs)?;
    let caps: Option<AccountCaps> = sql::from_json_opt(caps)?;
    Ok((
        prefs.auto_download,
        caps.and_then(|c| c.max_receive_attachment_bytes),
    ))
}

/// The names (`Attachment::net_ids`) of attachment number `idx` of `body`, empty when the
/// body has none for it.
pub(super) fn net_ids_in(body: &Body, idx: usize) -> Vec<String> {
    match body {
        Body::Media { attachments, .. } => attachments
            .get(idx)
            .map(|a| a.net_ids.clone())
            .unwrap_or_default(),
        Body::Sticker(a) if idx == 0 => a.net_ids.clone(),
        _ => Vec::new(),
    }
}

fn key_of(bytes: Vec<u8>) -> Result<BlobKey> {
    let key: [u8; 32] = bytes
        .try_into()
        .map_err(|_| CoreError::Json("stored file key is not 32 bytes".into()))?;
    Ok(BlobKey::new(key))
}

/// The rows of messages (alias `m`) whose time has not come and that the expiry policy has
/// not hidden, at `?2` (now): the only ones whose attachments are fetched without a click.
const NOT_DUE: &str = "m.expiry_hidden = 0 AND (m.expires_at IS NULL OR m.expires_at > ?2)";

impl Apply<'_> {
    /// Applies the auto-download policy to the attachments of a message that was just stored.
    pub(crate) fn plan_downloads(
        &mut self,
        message: MessageId,
        conv: ConversationId,
        history: bool,
    ) -> Result<()> {
        let rows: Vec<RawAtt> = self
            .conn
            .prepare_cached(&format!(
                "{ATT_SELECT} WHERE a.message_id = ?1 ORDER BY a.idx"
            ))?
            .query_map([message.0], att_row)?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        if rows.is_empty() {
            return Ok(());
        }
        let (prefs, limit) = download_prefs(self.conn, self.acc.id)?;
        let untrusted: bool = self
            .conn
            .prepare_cached(
                "SELECT blocked <> 0 OR request_pending <> 0 FROM conversations WHERE id = ?1",
            )?
            .query_row([conv.0], |r| r.get(0))?;
        // A message that is due or hidden by the expiry policy is gone for the user
        // ([`Self::user_download`] refuses it), so nothing is fetched for it on its own
        // either: an imported disappearing message whose timer ran out on the phone arrives
        // with its expiry while the clock is unconfirmed, and its long text would be asked
        // from the CDN before the sweeper hides the row.
        let gone: bool = self
            .conn
            .prepare_cached(&format!(
                "SELECT NOT ({NOT_DUE}) FROM messages m WHERE m.id = ?1"
            ))?
            .query_row(params![message.0, self.now], |r| r.get(0))?;
        // Parsed when the first download starts, once for the whole message: an album of 31
        // pictures would parse its body 31 times.
        let mut body: Option<Body> = None;
        for raw in rows {
            let row = raw.parse()?;
            let cand = Candidate {
                kind: row.kind,
                size: row.size,
                sticker: row.sticker,
                long_text: row.long_text,
                state: row.state,
                has_remote: row.remote.is_some(),
                history,
                untrusted,
            };
            match policy::decide(&cand, &prefs, limit) {
                Decision::Auto if gone => {}
                Decision::Auto => {
                    let body = match &body {
                        Some(body) => body,
                        None => body.insert(self.body_of(message)?),
                    };
                    let names = net_ids_in(body, row.idx);
                    self.start_download(&row, Source::Auto, names)?;
                }
                Decision::TooLarge if history => {
                    // The import judged by a limit older than the account's own (the remote
                    // configuration came later, or differs): nothing here can fetch this, and
                    // the click path refuses it, so it is "not available on this device" like
                    // the mapper's own too-large pointers, and keeps no key.
                    self.conn
                        .prepare_cached("UPDATE attachments SET remote = NULL WHERE id = ?1")?
                        .execute([row.id.0])?;
                    self.set_att_state(&row, &AttachmentState::Unavailable)?;
                }
                Decision::TooLarge => {
                    self.set_att_state(&row, &AttachmentState::Failed { retryable: false })?;
                }
                Decision::OnClick => {}
            }
        }
        Ok(())
    }

    /// `conv` is no longer a blocked chat or an unanswered message request: the long texts of
    /// its messages (`Attachment.long_text`, the rest of a text that arrived cut off) are
    /// fetched now. Nothing is fetched from a stranger before the user accepts, so until
    /// then such a message showed only the beginning of its text, and nothing would
    /// complete it afterwards: the policy looks at an attachment once, when its message
    /// is stored. Only what the policy fetches on its own (the size of a text, a pointer
    /// that is still `Remote`; imported long texts too, which the policy fetches like live
    /// ones) and only for a message that is not due or hidden by the expiry policy;
    /// pictures and files of the chat stay a click away.
    pub(crate) fn fetch_long_texts(&mut self, conv: ConversationId) -> Result<()> {
        let rows: Vec<RawAtt> = self
            .conn
            .prepare_cached(&format!(
                "{ATT_SELECT} WHERE m.conversation_id = ?1 AND a.long_text <> 0
                   AND m.deleted IS NULL AND {NOT_DUE} ORDER BY a.id"
            ))?
            .query_map(params![conv.0, self.now], att_row)?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        if rows.is_empty() {
            return Ok(());
        }
        let (prefs, limit) = download_prefs(self.conn, self.acc.id)?;
        for raw in rows {
            let row = raw.parse()?;
            let cand = Candidate {
                kind: row.kind,
                size: row.size,
                sticker: row.sticker,
                long_text: true,
                state: row.state,
                has_remote: row.remote.is_some(),
                history: false,
                untrusted: false,
            };
            if policy::decide(&cand, &prefs, limit) == Decision::Auto {
                self.request_download(&row, Source::Auto)?;
            }
        }
        Ok(())
    }

    /// The user asked for `attachment` (§5.8 "on click"); also retries a failed download.
    /// Returns whether a download was requested.
    pub(crate) fn user_download(&mut self, attachment: AttachmentId) -> Result<bool> {
        let row = load_att(self.conn, self.acc.id, attachment)?
            .ok_or(CoreError::refused(Refusal::Gone, "unknown attachment"))?;
        let (deleted, hidden): (bool, bool) = self
            .conn
            .prepare_cached(
                "SELECT deleted IS NOT NULL, expiry_hidden FROM messages WHERE id = ?1",
            )?
            .query_row([row.message.0], |r| Ok((r.get(0)?, r.get(1)?)))?;
        if hidden {
            // Hidden until its clock is confirmed ([`crate::expiry`]): gone for the user.
            return Err(CoreError::refused(Refusal::Gone, "unknown attachment"));
        }
        if deleted {
            return Err(CoreError::refused(
                Refusal::Deleted,
                "the message is deleted",
            ));
        }
        let (_, limit) = download_prefs(self.conn, self.acc.id)?;
        let can_ask = self.can_ask(&row)?;
        match policy::may_click(row.state, row.remote.is_some(), row.size, limit, can_ask)
            .map_err(CoreError::InvalidInput)?
        {
            Click::Download => self.request_download(&row, Source::User),
            Click::Ask => {
                self.ask_restore(&row)?;
                Ok(false)
            }
        }
    }

    /// The names of `row` (`Attachment::net_ids` of the message's stored body), empty when
    /// the body has none for it.
    pub(super) fn net_ids_of(&self, row: &AttRow) -> Result<Vec<String>> {
        Ok(net_ids_in(&self.body_of(row.message)?, row.idx))
    }

    /// Starts a download of `row` with a fresh blob target, unless one is in flight already.
    /// A caller that starts several downloads of one message parses the body once and uses
    /// [`Self::start_download`].
    pub(super) fn request_download(&mut self, row: &AttRow, source: Source) -> Result<bool> {
        if row.remote.is_none() {
            return Ok(false);
        }
        let net_ids = self.net_ids_of(row)?;
        self.start_download(row, source, net_ids)
    }

    /// [`Self::request_download`] with the names of `row` (`net_ids_in` of its message's
    /// body) given.
    pub(super) fn start_download(
        &mut self,
        row: &AttRow,
        source: Source,
        net_ids: Vec<String>,
    ) -> Result<bool> {
        let Some(remote) = row.remote.clone() else {
            return Ok(false);
        };
        let (id, key) = (BlobId::new_random(), FileKey::generate());
        let path = crate::attachments::core_path(&id);
        let path_col = path.to_string_lossy().into_owned();
        let inserted = self
            .conn
            .prepare_cached(
                "INSERT OR IGNORE INTO attachment_downloads(attachment_id, account_id, path, key,
                   requested_at, attempts, source) VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)",
            )?
            .execute(params![
                row.id.0,
                self.acc.id.0,
                path_col,
                &key.as_bytes()[..],
                self.now,
                source.as_str(),
            ])?;
        if inserted == 0 {
            return Ok(false);
        }
        let request = kourier_model::DownloadId(self.conn.last_insert_rowid());
        self.set_att_state(
            row,
            &AttachmentState::Downloading {
                done: 0,
                total: row.size,
            },
        )?;
        self.ch.downloads.push(DownloadJob {
            attachment: row.id,
            request,
            remote,
            path,
            key: BlobKey::new(*key.as_bytes()),
            net_ids,
        });
        Ok(true)
    }

    pub(super) fn set_att_state(&mut self, row: &AttRow, state: &AttachmentState) -> Result<()> {
        self.conn
            .prepare_cached("UPDATE attachments SET state = ?2 WHERE id = ?1")?
            .execute((row.id.0, sql::to_json(state)?))?;
        self.ch.media.insert(row.id);
        self.ch.message(row.conv, row.message);
        Ok(())
    }

    /// A download finished (§4.2 `AttachmentDone`, durable). Applies only to the download in
    /// flight: a replayed or stale answer changes nothing, and an error never undoes a file that
    /// is already here. An answer naming another request than the one in flight (a late reply
    /// to an earlier attempt of the same attachment) is ignored; one without a request id (a
    /// row written before the field existed) applies to the download in flight.
    pub(crate) fn attachment_done(
        &mut self,
        attachment: AttachmentId,
        request: Option<kourier_model::DownloadId>,
        result: std::result::Result<AttachmentMeta, BackendError>,
    ) -> Result<()> {
        let Some(row) = load_att(self.conn, self.acc.id, attachment)? else {
            // Deleted meanwhile. Its target was unlinked then, but the backend may have
            // finished the file only now: it goes at once, not at the next start's sweep.
            return self.late_download(attachment, request);
        };
        let target: Option<(i64, String, Vec<u8>, String)> = self
            .conn
            .prepare_cached(
                "SELECT id, path, key, source FROM attachment_downloads WHERE attachment_id = ?1",
            )?
            .query_row([attachment.0], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .optional()?;
        if let Some(request) = request
            && target.as_ref().is_none_or(|(id, ..)| *id != request.0)
        {
            tracing::debug!(
                attachment = attachment.0,
                request = request.0,
                "stale download answer ignored"
            );
            // The file of a download whose row went before its answer came.
            return self.late_download(attachment, Some(request));
        }
        let Some((_, path, key, source)) = target else {
            if let Err(e) = result
                && row.file_id.is_none()
                && !matches!(row.state, AttachmentState::Local)
            {
                // The network says no without a request in flight (a pointer it knows is gone).
                self.set_att_state(&row, &failed_state(&e))?;
            }
            return Ok(());
        };
        self.conn
            .prepare_cached("DELETE FROM attachment_downloads WHERE attachment_id = ?1")?
            .execute([attachment.0])?;
        match result {
            Ok(meta) => {
                let mime = meta.mime.clone().or(row.mime.clone());
                self.conn
                    .prepare_cached(
                        "INSERT INTO files(account_id, path, key, size, mime, purpose)
                         VALUES (?1, ?2, ?3, ?4, ?5, 'attachment')",
                    )?
                    .execute(params![
                        self.acc.id.0,
                        path,
                        key,
                        sql::i64_of(meta.size),
                        mime
                    ])?;
                let file_id = self.conn.last_insert_rowid();
                if let Some(old) = row.file_id {
                    self.delete_file(old)?;
                }
                self.conn
                    .prepare_cached(
                        "UPDATE attachments SET state = ?2, file_id = ?3, size = ?4,
                           mime = coalesce(?5, mime), width = coalesce(?6, width),
                           height = coalesce(?7, height), duration_ms = coalesce(?8, duration_ms),
                           derived_at = NULL WHERE id = ?1",
                    )?
                    .execute(params![
                        attachment.0,
                        sql::to_json(&AttachmentState::Local)?,
                        file_id,
                        sql::i64_of(meta.size),
                        meta.mime,
                        meta.dims.map(|d| d.0),
                        meta.dims.map(|d| d.1),
                        meta.duration_ms,
                    ])?;
                self.ch.derive.push(DeriveJob {
                    attachment,
                    message: row.message,
                    conv: row.conv,
                    file_id,
                    path: PathBuf::from(path),
                    key: key_of(key)?,
                    mime,
                    kind: row.kind,
                    sticker: row.sticker,
                    long_text: row.long_text,
                });
                self.ch.media.insert(attachment);
                self.ch.message(row.conv, row.message);
            }
            Err(e) => {
                self.ch.unlink.push(PathBuf::from(path));
                let state = failed_state(&e);
                // A download the USER started that the network no longer has asks the other
                // device at once, without a second click; an automatic one asks nothing, and
                // one whose pointer the other device gave does not ask again by itself (a
                // click still does).
                if state == AttachmentState::Expired
                    && source == Source::User.as_str()
                    && !row.restored
                    && self.can_ask(&row)?
                {
                    self.ask_restore(&row)?;
                } else {
                    self.set_att_state(&row, &state)?;
                }
            }
        }
        Ok(())
    }

    /// The attachments of `message` as an outgoing request carries them (an edit of a caption
    /// re-sends them unchanged): `stored` (the body's list, in order) with what the rows have of
    /// each, the network pointer it arrived with and the core's blobs. Long texts are left out:
    /// they are the rest of a text, not a file.
    pub(crate) fn request_attachments(
        &self,
        message: MessageId,
        stored: &[kourier_model::Attachment],
    ) -> Result<Vec<kourier_model::Attachment>> {
        type Blob = (Option<String>, Option<Vec<u8>>);
        type Row = (
            i64,
            Option<String>,
            Option<i64>,
            Option<Vec<u8>>,
            Blob,
            Blob,
        );
        let rows: Vec<Row> = self
            .conn
            .prepare_cached(
                "SELECT a.idx, a.mime, a.size, a.remote, f.path, f.key, t.path, t.key
                 FROM attachments a LEFT JOIN files f ON f.id = a.file_id
                   LEFT JOIN files t ON t.id = a.thumb_file_id
                 WHERE a.message_id = ?1",
            )?
            .query_map([message.0], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    (r.get(4)?, r.get(5)?),
                    (r.get(6)?, r.get(7)?),
                ))
            })?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        let blob = |(path, key): Blob| -> Option<kourier_model::BlobRef> {
            Some(kourier_model::BlobRef {
                path: PathBuf::from(path?),
                key: BlobKey::new(key?.try_into().ok()?),
            })
        };
        let mut out = Vec::new();
        for (idx, a) in stored.iter().enumerate() {
            if a.long_text {
                continue;
            }
            let mut a = a.clone();
            let idx = i64::try_from(idx).unwrap_or(i64::MAX);
            if let Some((_, mime, size, remote, file, thumb)) =
                rows.iter().find(|r| r.0 == idx).cloned()
            {
                if let Some(m) = mime {
                    a.mime = m;
                }
                if let Some(size) = size.and_then(|s| u64::try_from(s).ok()) {
                    a.size = size;
                }
                a.remote = sql::blob_from_sql(remote);
                a.local = blob(file);
                a.thumb = blob(thumb);
            }
            out.push(a);
        }
        Ok(out)
    }

    /// Before the row of attachment `att` goes: a download of it that is still in flight is
    /// remembered (`download_orphans`) and its target unlinked after the commit. The
    /// backend may be writing the file right now and rename it into place after that
    /// unlink; its answer names the request, and [`Self::late_download`] removes the file
    /// then. (The download row itself goes with the attachment row.)
    pub(crate) fn orphan_download(&mut self, att: i64) -> Result<()> {
        let target: Option<(i64, String)> = self
            .conn
            .prepare_cached("SELECT id, path FROM attachment_downloads WHERE attachment_id = ?1")?
            .query_row([att], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let Some((request, path)) = target else {
            return Ok(());
        };
        self.conn
            .prepare_cached(
                "INSERT OR REPLACE INTO download_orphans(request, account_id, attachment_id, path,
                   dropped_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?
            .execute(params![request, self.acc.id.0, att, path, self.now])?;
        self.ch.unlink.push(path.into());
        Ok(())
    }

    /// The answer to a download whose attachment row is gone, or which is no longer the
    /// one in flight: whatever the backend left at its target is unlinked after the commit.
    /// An answer without a request id (a row written before the field existed) is matched
    /// by the attachment.
    fn late_download(
        &mut self,
        attachment: AttachmentId,
        request: Option<kourier_model::DownloadId>,
    ) -> Result<()> {
        let paths: Vec<String> = match request {
            Some(request) => self
                .conn
                .prepare_cached(
                    "DELETE FROM download_orphans WHERE request = ?1 AND account_id = ?2
                     RETURNING path",
                )?
                .query_map((request.0, self.acc.id.0), |r| r.get(0))?
                .collect::<kourier_db::rusqlite::Result<_>>()?,
            None => self
                .conn
                .prepare_cached(
                    "DELETE FROM download_orphans WHERE attachment_id = ?1 AND account_id = ?2
                     RETURNING path",
                )?
                .query_map((attachment.0, self.acc.id.0), |r| r.get(0))?
                .collect::<kourier_db::rusqlite::Result<_>>()?,
        };
        self.ch.unlink.extend(paths.into_iter().map(PathBuf::from));
        Ok(())
    }

    /// Deletes a `files` row and schedules its file for unlinking after the commit.
    pub(crate) fn delete_file(&mut self, id: i64) -> Result<()> {
        let path: Option<String> = self
            .conn
            .prepare_cached("SELECT path FROM files WHERE id = ?1")?
            .query_row([id], |r| r.get(0))
            .optional()?;
        self.conn
            .prepare_cached("DELETE FROM files WHERE id = ?1")?
            .execute([id])?;
        if let Some(p) = path {
            self.ch.unlink.push(p.into());
        }
        Ok(())
    }

    /// Deletes a message's attachment rows, their files rows and download targets; the files are
    /// unlinked after the commit.
    pub(crate) fn delete_attachments(&mut self, id: MessageId) -> Result<()> {
        let files: Vec<(i64, String)> = self
            .conn
            .prepare_cached(
                "SELECT f.id, f.path FROM files f JOIN attachments a
                   ON f.id = a.file_id OR f.id = a.thumb_file_id
                 WHERE a.message_id = ?1",
            )?
            .query_map([id.0], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        let atts: Vec<i64> = self
            .conn
            .prepare_cached("SELECT id FROM attachments WHERE message_id = ?1")?
            .query_map([id.0], |r| r.get(0))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        for att in &atts {
            self.orphan_download(*att)?;
        }
        // The cascade drops the download rows.
        self.conn
            .prepare_cached("DELETE FROM attachments WHERE message_id = ?1")?
            .execute([id.0])?;
        for (fid, path) in files {
            self.conn
                .prepare_cached("DELETE FROM files WHERE id = ?1")?
                .execute([fid])?;
            self.ch.unlink.push(path.into());
        }
        for att in atts {
            self.ch.media.insert(AttachmentId(att));
            self.ch.media_gone.insert(AttachmentId(att));
        }
        Ok(())
    }
}

fn failed_state(e: &BackendError) -> AttachmentState {
    match e.kind {
        ErrorKind::Expired => AttachmentState::Expired,
        _ => AttachmentState::Failed {
            retryable: e.is_retryable(),
        },
    }
}

/// The newest download request of `account` (0 when none is in flight).
pub(crate) fn newest_download(conn: &Connection, account: AccountId) -> Result<i64> {
    Ok(conn
        .prepare_cached(
            "SELECT coalesce(max(id), 0) FROM attachment_downloads WHERE account_id = ?1",
        )?
        .query_row([account.0], |r| r.get(0))?)
}

/// After an actor start (at its catch-up boundary): every download an earlier actor left in
/// flight (request ids up to `up_to`) is sent again with its target and request id, except
/// those resent [`MAX_DOWNLOAD_RESENDS`] times already, which become `Failed{retryable}` (their
/// targets are unlinked). One transaction.
pub(crate) fn resume_downloads(
    conn: &mut Connection,
    account: AccountId,
    scope: kourier_model::MsgIdScope,
    now: i64,
    up_to: i64,
) -> Result<(Vec<DownloadJob>, super::Changes)> {
    let tx =
        conn.transaction_with_behavior(kourier_db::rusqlite::TransactionBehavior::Immediate)?;
    let mut apply = Apply::begin(&tx, account, scope, now)?;
    let rows: Vec<(i64, i64, String, Vec<u8>, i64)> = tx
        .prepare_cached(
            "SELECT id, attachment_id, path, key, attempts FROM attachment_downloads
             WHERE account_id = ?1 AND id <= ?2 ORDER BY requested_at, id",
        )?
        .query_map([account.0, up_to], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    let mut jobs = Vec::new();
    // The stored body of each message with downloads in flight, parsed once however many of
    // its attachments are (a 31-picture album is 31 downloads).
    let mut bodies: std::collections::HashMap<MessageId, Body> = std::collections::HashMap::new();
    for (request, att, path, key, attempts) in rows {
        let row = load_att(&tx, account, AttachmentId(att))?;
        let remote = row.as_ref().and_then(|r| r.remote.clone());
        match (row, remote) {
            (Some(row), Some(remote)) if attempts < MAX_DOWNLOAD_RESENDS => {
                tx.prepare_cached(
                    "UPDATE attachment_downloads SET attempts = attempts + 1
                     WHERE attachment_id = ?1",
                )?
                .execute([att])?;
                let body = match bodies.entry(row.message) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        e.insert(apply.body_of(row.message)?)
                    }
                };
                jobs.push(DownloadJob {
                    attachment: row.id,
                    request: kourier_model::DownloadId(request),
                    remote,
                    path: PathBuf::from(path),
                    key: key_of(key)?,
                    net_ids: net_ids_in(body, row.idx),
                });
            }
            (row, _) => {
                tx.prepare_cached("DELETE FROM attachment_downloads WHERE attachment_id = ?1")?
                    .execute([att])?;
                apply.ch.unlink.push(PathBuf::from(path));
                if let Some(row) = row {
                    tracing::info!(
                        attachment = att,
                        "a download was never answered; marked failed"
                    );
                    apply.set_att_state(&row, &AttachmentState::Failed { retryable: true })?;
                }
            }
        }
    }
    apply.finish()?;
    let changes = std::mem::take(&mut apply.ch);
    drop(apply);
    tx.commit()?;
    Ok((jobs, changes))
}

/// Downloaded files of `account` whose thumbnail and blurhash were not derived yet (a kill came
/// between the download and the derive step).
pub(crate) fn pending_derives(conn: &Connection, account: AccountId) -> Result<Vec<DeriveJob>> {
    type Raw = (
        i64,
        i64,
        i64,
        i64,
        String,
        Vec<u8>,
        Option<String>,
        String,
        String,
        bool,
    );
    let rows: Vec<Raw> = conn
        .prepare_cached(
            "SELECT a.id, a.message_id, m.conversation_id, f.id, f.path, f.key, a.mime, a.kind,
                    m.body_kind, a.long_text
             FROM attachments a JOIN messages m ON m.id = a.message_id
             JOIN files f ON f.id = a.file_id
             WHERE a.derived_at IS NULL AND m.account_id = ?1 ORDER BY a.id",
        )?
        .query_map([account.0], |r| {
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
                r.get(9)?,
            ))
        })?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    rows.into_iter()
        .map(
            |(att, msg, conv, fid, path, key, mime, kind, body_kind, long_text)| {
                Ok(DeriveJob {
                    attachment: AttachmentId(att),
                    message: MessageId(msg),
                    conv: ConversationId(conv),
                    file_id: fid,
                    path: PathBuf::from(path),
                    key: key_of(key)?,
                    mime,
                    kind: sql::from_enum_name(&kind)?,
                    sticker: body_kind == "Sticker",
                    long_text,
                })
            },
        )
        .collect()
}

/// At a start, after the blob sweep: the answers of an earlier run's downloads cannot leave
/// a file behind any more (the sweep removed every blob no row names).
pub(crate) fn forget_orphans(conn: &Connection, account: AccountId) -> Result<usize> {
    Ok(conn
        .prepare_cached("DELETE FROM download_orphans WHERE account_id = ?1")?
        .execute([account.0])?)
}
