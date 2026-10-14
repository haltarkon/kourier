//! Asking the account's other device for files the network lost (Signal: attachment
//! backfill, S6). A click on an attachment that is "Not available on this device"
//! (`Unavailable`) or "No longer available" (`Expired`), and a download the user started that
//! ends "no longer available", store ONE durable request per message
//! (`DurableCommand::RequestAttachments`, `attachment_requests`) and set the asked attachments
//! `Asking`. The backend reports when the request went out (the wait starts: 30 s, 2 min more
//! after each PENDING answer, durable) and the other device's answers
//! (`BackendEvent::AttachmentRestore`), which give attachments new pointers or end the wait.
//!
//! One source of truth: the attachments the user asked for are those in `Asking`; the row of
//! `attachment_requests` is the wait and its deadline.

use std::sync::atomic::{AtomicI64, Ordering};

use kourier_backend_api::{DurableCommand, RestoreSlot, RestoreUpdate};
use kourier_db::rusqlite::{OptionalExtension, params};
use kourier_model::{
    AccountCaps, AccountState, AskFailure, AttachmentState, BackendBlob, Body, CmdId, MsgTarget,
    NetMsgId,
};

use super::Apply;
use super::attachment::{AttRow, Source, download_prefs, message_atts};
use super::message::MsgRow;
use crate::error::Result;
use crate::sql;

/// How long the core waits for the other device's first answer after the request went out
/// (Signal Android: 30 s).
pub(crate) const ANSWER_WAIT_MS: i64 = 30_000;

/// How long the core waits more after each answer that says "still uploading" (PENDING;
/// Signal Android: 2 min).
pub(crate) const PENDING_WAIT_MS: i64 = 120_000;

/// How long a request may wait to go out before the core gives up on it ("did not answer"):
/// the backend drops its own request job this long after the click (Signal:
/// `backfill::REQUEST_MAX_AGE`, 1 h) and reports it, and the core adds a margin of five
/// minutes, so a request that never went out (no connection, a logged-out account, a backend
/// that lost the command) still ends here, whatever the backend does. `Sent` replaces this
/// deadline by the 30 s one.
pub(crate) const UNSENT_BOUND_MS: i64 = 65 * 60_000;

/// The waits in force ([`Timeouts::restore_answer`](crate::Timeouts::restore_answer), set by
/// [`tune_waits`]); the constants above until a core starts.
static ANSWER_WAIT: AtomicI64 = AtomicI64::new(ANSWER_WAIT_MS);
static PENDING_WAIT: AtomicI64 = AtomicI64::new(PENDING_WAIT_MS);

/// Sets the waits for every core of this process (`Core::start`).
pub(crate) fn tune_waits(answer: std::time::Duration, pending: std::time::Duration) {
    let ms = |d: std::time::Duration| i64::try_from(d.as_millis()).unwrap_or(i64::MAX);
    ANSWER_WAIT.store(ms(answer), Ordering::Relaxed);
    PENDING_WAIT.store(ms(pending), Ordering::Relaxed);
}

impl Apply<'_> {
    /// Whether the account's other device can be asked for `row`: the account is linked, its
    /// network can ask (`AccountCaps::FLAG_ATTACHMENT_RESTORE`), it is a file and not the rest
    /// of a long text (asked only with the files of a request the user started), and its
    /// message can be named (it has a network id) and is not view-once or deleted. The size
    /// limit is [`crate::attachments::policy::may_click`]'s.
    pub(super) fn can_ask(&self, row: &AttRow) -> Result<bool> {
        if row.long_text || self.acc.state != AccountState::Active {
            return Ok(false);
        }
        let caps: Option<String> = self
            .conn
            .prepare_cached("SELECT caps FROM accounts WHERE id = ?1")?
            .query_row([self.acc.id.0], |r| r.get(0))?;
        let caps: Option<AccountCaps> = sql::from_json_opt(caps)?;
        if !caps.is_some_and(|c| c.restores_attachments()) {
            return Ok(false);
        }
        let Some(msg) = self.msg_row(row.message)? else {
            return Ok(false);
        };
        let ok: bool = self
            .conn
            .prepare_cached("SELECT view_once = 0 FROM messages WHERE id = ?1")?
            .query_row([msg.id.0], |r| r.get(0))?;
        // A chat or an author known by phone number alone: the answer could not be placed.
        Ok(ok
            && !msg.net_local.is_empty()
            && msg.deleted.is_none()
            && matches!(msg.body_kind.as_str(), "Media" | "Sticker")
            && !sql::conv_key(&msg.conv_key)?.is_number_only()
            && !sql::user(&msg.net_author).is_ok_and(|u| u.is_phone_number()))
    }

    /// Asks the other device for `row` (the caller checked [`Self::can_ask`]): the attachment
    /// becomes `Asking`, and the message's request is stored unless one is open already (a
    /// second click, or a click on another file of the message, joins it).
    pub(super) fn ask_restore(&mut self, row: &AttRow) -> Result<()> {
        let inserted = self
            .conn
            .prepare_cached(
                "INSERT OR IGNORE INTO attachment_requests(message_id, account_id, created_at, deadline)
                 VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![
                row.message.0,
                self.acc.id.0,
                self.now,
                self.now.saturating_add(UNSENT_BOUND_MS)
            ])?;
        if inserted > 0 {
            let (conv, target) = self.target_of(row)?;
            let cmd = self.queue_command(DurableCommand::RequestAttachments { conv, target })?;
            // The request knows its command: the backend's reports name it.
            self.conn
                .prepare_cached("UPDATE attachment_requests SET cmd = ?2 WHERE message_id = ?1")?
                .execute(params![row.message.0, cmd.0.0])?;
            self.ch.commands.push(cmd);
            // The task sleeps towards the nearest deadline: this one may be it.
            self.ch.restoring = true;
        }
        if row.state != AttachmentState::Asking {
            self.set_att_state(row, &AttachmentState::Asking)?;
        }
        Ok(())
    }

    /// The conversation key and the message reference of `row`'s message.
    fn target_of(&self, row: &AttRow) -> Result<(kourier_model::ConvKey, MsgTarget)> {
        let msg = self
            .msg_row(row.message)?
            .ok_or(crate::error::CoreError::refused(
                crate::error::Refusal::Gone,
                "unknown attachment",
            ))?;
        let key = sql::conv_key(&msg.conv_key)?;
        let author = (!msg.net_author.is_empty())
            .then(|| sql::user(&msg.net_author))
            .transpose()?;
        let target = MsgTarget::to_id(
            Some(key.clone()),
            &NetMsgId {
                author,
                local: msg.net_local.as_str().into(),
            },
        );
        Ok((key, target))
    }
}

/// What one answer did, for the debug log (counts only).
#[derive(Debug, Default)]
struct Tally {
    applied: usize,
    unmatched: usize,
    too_large: usize,
    unverifiable: usize,
    pending: usize,
    long_pending: usize,
    gone: usize,
}

impl Apply<'_> {
    /// `AttachmentRestore` (durable): news about the request for the files of one message.
    /// A message this account does not have is ignored (counted in the debug log; never a
    /// wait like a modifier's: the other device answers about messages it has).
    pub(super) fn attachment_restore(
        &mut self,
        target: &MsgTarget,
        update: RestoreUpdate,
        cmd: Option<CmdId>,
    ) -> Result<()> {
        let canon = self.canon_target(target)?;
        let Some(msg) = self.resolve_target(&canon)? else {
            tracing::debug!(
                account = self.acc.id.0,
                "restore news for a message that is not here: ignored"
            );
            return Ok(());
        };
        let ours = self.request_open(&msg)?;
        match update {
            RestoreUpdate::Sent => {
                // The wait starts when the request went out; a second report changes nothing.
                // Only for the request this command made: a report about an earlier request
                // (an expired job, a command sent again) must not start the wait of a newer one.
                let armed = self
                    .conn
                    .prepare_cached(
                        "UPDATE attachment_requests SET sent_at = ?2, deadline = ?3
                         WHERE message_id = ?1 AND sent_at IS NULL AND cmd = ?4",
                    )?
                    .execute(params![
                        msg.id.0,
                        self.now,
                        self.now.saturating_add(ANSWER_WAIT.load(Ordering::Relaxed)),
                        cmd.map(|c| c.0)
                    ])?;
                if armed > 0 {
                    self.ch.restoring = true;
                }
                Ok(())
            }
            // The request that was not sent ends its own wait; a newer wait of the message
            // (a second click after the first ended) is not its business.
            RestoreUpdate::NotSent => {
                if self.request_is(&msg, cmd)? {
                    self.end_wait(&msg, AskFailure::NoAnswer)
                } else {
                    Ok(())
                }
            }
            RestoreUpdate::MessageNotFound => {
                if ours {
                    self.end_wait(&msg, AskFailure::NoMessage)
                } else {
                    Ok(())
                }
            }
            RestoreUpdate::Answer {
                attachments,
                long_text,
            } => self.restore_answer(&msg, ours, attachments, long_text),
        }
    }

    /// Whether this device waits for an answer about `msg` (a request of its own is open).
    fn request_open(&self, msg: &MsgRow) -> Result<bool> {
        Ok(self
            .conn
            .prepare_cached("SELECT 1 FROM attachment_requests WHERE message_id = ?1")?
            .query_row([msg.id.0], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// Whether the open request of `msg` is the one `cmd` made.
    fn request_is(&self, msg: &MsgRow, cmd: Option<CmdId>) -> Result<bool> {
        let Some(cmd) = cmd else {
            return Ok(false);
        };
        Ok(self
            .conn
            .prepare_cached("SELECT 1 FROM attachment_requests WHERE message_id = ?1 AND cmd = ?2")?
            .query_row(params![msg.id.0, cmd.0], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// Ends the wait for `msg`: its request goes, with its command when no backend has
    /// confirmed that yet (a request nobody waits for must not be sent later: the phone would
    /// upload for nothing and the report would reach a newer wait), and every attachment
    /// still asked for ends with `why`.
    pub(crate) fn end_wait(&mut self, msg: &MsgRow, why: AskFailure) -> Result<()> {
        let cmd: Option<i64> = self
            .conn
            .prepare_cached("SELECT cmd FROM attachment_requests WHERE message_id = ?1")?
            .query_row([msg.id.0], |r| r.get(0))
            .optional()?
            .flatten();
        let gone = self
            .conn
            .prepare_cached("DELETE FROM attachment_requests WHERE message_id = ?1")?
            .execute([msg.id.0])?;
        if gone > 0 {
            self.ch.restoring = true;
        }
        if let Some(cmd) = cmd {
            self.conn
                .prepare_cached("DELETE FROM pending_commands WHERE id = ?1 AND account_id = ?2")?
                .execute(params![cmd, self.acc.id.0])?;
        }
        self.fail_asking(msg, why)
    }

    /// Every attachment of `msg` still `Asking` ends with `why`.
    fn fail_asking(&mut self, msg: &MsgRow, why: AskFailure) -> Result<()> {
        for row in message_atts(self.conn, msg.id)? {
            if row.state == AttachmentState::Asking {
                self.set_att_state(&row, &AttachmentState::AskFailed { why })?;
            }
        }
        Ok(())
    }

    /// The other device's answer for `msg`. `ours`: this device asked (a request row is
    /// open), so the attachments it asked for (`Asking`) download and the long text it
    /// brings too; an answer to another device's request only gives files the network lost
    /// here a new pointer (they download on a click).
    ///
    /// Matching in two passes. First every file slot by its names (`uuid:`, `digest:`),
    /// priority over all attachments as for a delete. Then, for what no name placed, by
    /// position, only where the order is PROVEN: the answer has exactly as many slots as the
    /// message has files (Signal's mapper drops entries without a pointer and cuts at 1,000:
    /// equal counts prove nothing was dropped), every slot a name placed sits on its own index
    /// (the phone lists its files in the order Kourier has them) and every file slot that
    /// would be placed by position has the size of the file there (no exception for an
    /// unknown size). Otherwise a PENDING or "gone" slot and a slot without a name match
    /// nothing. A "gone" slot states nothing about a file but its place, so it is believed
    /// only when a NAME placed a slot too (`named`): an answer of status slots alone, or of
    /// file slots placed by their sizes alone, proves nothing about its order (a message of
    /// one file has no order: its one slot is its own).
    ///
    /// A final answer (nothing is PENDING) ends the wait. Only an attachment a "gone" slot
    /// was MATCHED to ends as "not on your phone"; one still asked for that no slot placed
    /// (or whose pointer could not be used) ends as "did not answer", which can be asked
    /// again: Kourier's matching gap is not a statement about the phone.
    fn restore_answer(
        &mut self,
        msg: &MsgRow,
        ours: bool,
        slots: Vec<RestoreSlot>,
        long_text: Option<RestoreSlot>,
    ) -> Result<()> {
        if msg.deleted.is_some() {
            return Ok(());
        }
        let all = message_atts(self.conn, msg.id)?;
        let (long, files): (Vec<AttRow>, Vec<AttRow>) = all.into_iter().partition(|a| a.long_text);
        let names: Vec<Vec<String>> = files
            .iter()
            .map(|a| self.net_ids_of(a))
            .collect::<Result<_>>()?;
        let (_, limit) = download_prefs(self.conn, self.acc.id)?;
        let mut tally = Tally::default();
        let mut taken = vec![false; files.len()];
        // Pass 1: the names.
        let mut placed: Vec<Option<usize>> = vec![None; slots.len()];
        for (i, slot) in slots.iter().enumerate() {
            if let RestoreSlot::File { net_ids, .. } = slot {
                let found = net_ids
                    .expose()
                    .iter()
                    .find_map(|n| (0..files.len()).find(|&j| !taken[j] && names[j].contains(n)));
                if let Some(j) = found {
                    placed[i] = Some(j);
                    taken[j] = true;
                }
            }
        }
        // Pass 2: the positions, where the order is proven.
        // A name placed a slot: the phone's order is then checked against ours by something
        // that names content. Without one (every slot placed by size alone, or none placed)
        // a pointer may still be taken by its size, but a "gone" slot, which states nothing
        // about the file but its place, is not believed. Except for a message of ONE file:
        // with one slot for one file there is no order to get wrong (the counts agree), and
        // a name cannot come from a status slot, so "not on your phone" would be out of reach
        // for the commonest message there is.
        let named = placed.iter().any(Option::is_some) || files.len() == 1;
        let by_position = slots.len() == files.len()
            && placed
                .iter()
                .enumerate()
                .all(|(i, p)| p.is_none_or(|j| j == i))
            && slots.iter().enumerate().all(|(i, slot)| match slot {
                RestoreSlot::File { size, .. } if placed[i].is_none() => files[i].size == *size,
                _ => true,
            });
        if by_position {
            for (i, slot) in slots.iter().enumerate() {
                let status_only = matches!(slot, RestoreSlot::Gone | RestoreSlot::Pending);
                if placed[i].is_none()
                    && !matches!(slot, RestoreSlot::Skip)
                    && !(status_only && !named)
                    && !taken[i]
                {
                    placed[i] = Some(i);
                    taken[i] = true;
                }
            }
        }
        for (slot, at) in slots.into_iter().zip(placed) {
            if matches!(slot, RestoreSlot::Pending) {
                tally.pending += 1;
            }
            let Some(at) = at else {
                if !matches!(slot, RestoreSlot::Skip | RestoreSlot::Pending) {
                    tally.unmatched += 1;
                }
                continue;
            };
            let row = &files[at];
            match slot {
                RestoreSlot::File {
                    remote,
                    net_ids,
                    size,
                } => {
                    if limit.is_some_and(|l| size > l) {
                        tally.too_large += 1;
                    } else if !self.verifiable(row, net_ids.expose())? {
                        tally.unverifiable += 1;
                    } else if self.take_pointer(msg, row, ours, remote, net_ids.expose())? {
                        tally.applied += 1;
                    }
                }
                RestoreSlot::Gone => {
                    tally.gone += 1;
                    if row.state == AttachmentState::Asking {
                        self.set_att_state(
                            row,
                            &AttachmentState::AskFailed {
                                why: AskFailure::NoFile,
                            },
                        )?;
                    }
                }
                RestoreSlot::Pending | RestoreSlot::Skip => {}
            }
        }
        // The rest of a long text: only inside a request this device made (the user clicked
        // a file of the message), never one of its own. Not for an EDITED message: the
        // request names the original sent time, and the phone answers from the row of exactly
        // that time (Signal Android `MessageTable` looks up `DATE_SENT`), whose long text is
        // the FIRST version's; the newest text lives on the newest edit's row. Pasting it
        // would show an old text as the current one (Signal Desktop's flaw).
        let long_row = long.first().filter(|row| {
            matches!(
                row.state,
                AttachmentState::Unavailable
                    | AttachmentState::Expired
                    | AttachmentState::Failed { .. }
                    | AttachmentState::AskFailed { .. }
            )
        });
        if ours
            && msg.revision == 0
            && let Some(row) = long_row
        {
            match long_text {
                Some(RestoreSlot::File {
                    remote,
                    net_ids,
                    size,
                }) => {
                    if limit.is_some_and(|l| size > l) {
                        tally.too_large += 1;
                    } else if !self.verifiable(row, net_ids.expose())? {
                        tally.unverifiable += 1;
                    } else {
                        self.set_pointer(msg, row, &remote, net_ids.expose())?;
                        let mut row = load_again(self, row)?;
                        row.state = AttachmentState::Remote;
                        self.request_download(&row, Source::Restore)?;
                        tally.applied += 1;
                    }
                }
                // The text is still being uploaded: the wait stays open for it, or the files
                // that come first would end the request and the text, asked for with them,
                // would never be taken ("long texts: with the files").
                Some(RestoreSlot::Pending) => {
                    tally.pending += 1;
                    tally.long_pending += 1;
                }
                Some(RestoreSlot::Gone | RestoreSlot::Skip) | None => {}
            }
        }
        tracing::debug!(
            account = self.acc.id.0,
            ours,
            applied = tally.applied,
            unmatched = tally.unmatched,
            too_large = tally.too_large,
            unverifiable = tally.unverifiable,
            pending = tally.pending,
            long_pending = tally.long_pending,
            gone = tally.gone,
            "other device's answer applied"
        );
        if !ours {
            return Ok(());
        }
        if tally.pending > 0 {
            // Still uploading: wait 2 min more from now.
            self.conn
                .prepare_cached(
                    "UPDATE attachment_requests SET deadline = ?2,
                       sent_at = coalesce(sent_at, ?3) WHERE message_id = ?1",
                )?
                .execute(params![
                    msg.id.0,
                    self.now
                        .saturating_add(PENDING_WAIT.load(Ordering::Relaxed)),
                    self.now
                ])?;
            self.ch.restoring = true;
            return Ok(());
        }
        // A final answer: what it did not give, and no slot said was not there, may be asked
        // again.
        self.end_wait(msg, AskFailure::NoAnswer)
    }

    /// Whether a pointer with these `net_ids` can be checked when it is downloaded for `row`:
    /// it states a digest of its blob (`digest:`), or the attachment has the hash of its
    /// plaintext (`plaintext:`, an imported one). A pointer that states neither would be
    /// refused at the download (the file would stay "retry" for ever): it is not taken, and
    /// never replaces a pointer that works.
    fn verifiable(&self, row: &AttRow, net_ids: &[String]) -> Result<bool> {
        Ok(net_ids.iter().any(|n| n.starts_with("digest:"))
            || self
                .net_ids_of(row)?
                .iter()
                .any(|n| n.starts_with("plaintext:")))
    }

    /// Gives `row` the pointer of a file slot. `ours`: an attachment this device asked for
    /// downloads at once; otherwise only a file the network lost here (or whose ask ended)
    /// takes it and becomes downloadable on a click. A file that is here or downloading is
    /// left alone. Returns whether the pointer was taken.
    fn take_pointer(
        &mut self,
        msg: &MsgRow,
        row: &AttRow,
        ours: bool,
        remote: BackendBlob,
        net_ids: &[String],
    ) -> Result<bool> {
        let lost = matches!(
            row.state,
            AttachmentState::Unavailable
                | AttachmentState::Expired
                | AttachmentState::AskFailed { .. }
        );
        let asked = ours && row.state == AttachmentState::Asking;
        let other = ours
            && matches!(
                row.state,
                AttachmentState::Remote | AttachmentState::Failed { .. }
            );
        if !(lost || asked || other) {
            return Ok(false);
        }
        self.set_pointer(msg, row, &remote, net_ids)?;
        let mut fresh = load_again(self, row)?;
        if asked {
            fresh.state = AttachmentState::Remote;
            self.set_att_state(&fresh, &AttachmentState::Remote)?;
            self.request_download(&fresh, Source::Restore)?;
        } else if lost {
            self.set_att_state(&fresh, &AttachmentState::Remote)?;
        }
        Ok(true)
    }

    /// Stores `remote` as `row`'s pointer (marked as the other device's) and adds the names
    /// it lacks to the stored body (the message's and its newest revision's).
    fn set_pointer(
        &mut self,
        msg: &MsgRow,
        row: &AttRow,
        remote: &BackendBlob,
        net_ids: &[String],
    ) -> Result<()> {
        self.conn
            .prepare_cached("UPDATE attachments SET remote = ?2, restored = 1 WHERE id = ?1")?
            .execute(params![row.id.0, sql::blob_to_sql(remote)])?;
        let old: String = self
            .conn
            .prepare_cached("SELECT body FROM messages WHERE id = ?1")?
            .query_row([msg.id.0], |r| r.get(0))?;
        let mut body: Body = sql::from_json(&old)?;
        let att = match &mut body {
            Body::Media { attachments, .. } => attachments.get_mut(row.idx),
            Body::Sticker(a) if row.idx == 0 => Some(a.as_mut()),
            _ => None,
        };
        if let Some(att) = att {
            for n in net_ids {
                if !att.net_ids.contains(n) {
                    att.net_ids.push(n.clone());
                }
            }
            att.remote = Some(remote.clone());
            let new = sql::to_json(&body)?;
            self.conn
                .prepare_cached("UPDATE messages SET body = ?2 WHERE id = ?1")?
                .execute(params![msg.id.0, new])?;
            self.conn
                .prepare_cached(
                    "UPDATE message_revisions SET body = ?3 WHERE message_id = ?1 AND body = ?2",
                )?
                .execute(params![msg.id.0, old, new])?;
        }
        self.ch.media.insert(row.id);
        self.ch.message(row.conv, row.message);
        Ok(())
    }
}

/// `row` as stored now (after its pointer changed).
fn load_again(apply: &Apply<'_>, row: &AttRow) -> Result<AttRow> {
    super::attachment::load_att(apply.conn, apply.acc.id, row.id)?.ok_or(
        crate::error::CoreError::refused(crate::error::Refusal::Gone, "unknown attachment"),
    )
}
