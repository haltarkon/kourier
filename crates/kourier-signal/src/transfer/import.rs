//! The importer of a history transfer: every frame of the archive through the mapper into
//! the stage (docs/architecture.md §14.2 item 14; `transfer.rs`, "The import").
//!
//! It runs on the blocking thread that reads the archive and touches nothing but the stage
//! file: not `backend.db` (whose connection belongs to the account's thread), not the core.
//! So the importer's rule, "provisional until `commit`, and `begin` removes what an earlier
//! import left", is met by one file: [`begin`](Importer::begin) removes a stage that is
//! there and makes a new one, [`abort`](Importer::abort) removes it,
//! [`commit`](Importer::commit) writes its totals and closes it. A commit says only that
//! the stage is whole; that it may be USED is recorded by the account's thread afterwards
//! (`transfer.rs`, the commit zone), and a stage without that record is removed at the next
//! start.

use std::path::PathBuf;

use kourier_model::HistoryTransferFailure;
use libsignal_core::{Aci, Pni};
use zeroize::Zeroizing;

use super::map::{MapLimits, Mapper, MediaRules, Staged};
use super::stage::{StageError, StageKey, StageLimits, StageWriter};
use super::{ImportError, ImportSummary, Importer};
use crate::archive::{ArchiveInfo, Frame};

/// What the read of an archive hands the importer it makes ([`super::ImporterFactory`]).
/// `Debug` shows nothing of the key.
pub(crate) struct ImportContext {
    /// The transfer directory (`accounts/<uuid>/transfer`, 0700): the archive is in it.
    pub(crate) dir: PathBuf,
    /// The transfer's ephemeral backup key: the stage's key is derived from it.
    pub(crate) key: Zeroizing<[u8; 32]>,
    /// Our ACI.
    pub(crate) aci: Aci,
    /// Our PNI.
    pub(crate) pni: Pni,
    /// The time of the import (ms, the backend's clock).
    pub(crate) now: i64,
    /// The server's message queue time from the remote config, when one is stored (step S5:
    /// how old an attachment's place may be, `map::MediaRules`; none: the 30-day floor).
    pub(crate) queue_time: Option<std::time::Duration>,
    /// The largest attachment the server lets this account receive (bytes).
    pub(crate) receive_limit: u64,
}

impl std::fmt::Debug for ImportContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportContext").finish_non_exhaustive()
    }
}

/// The importer the app uses: [`HistoryImporter`] with the default bounds.
pub(crate) fn history_importer(ctx: ImportContext) -> Box<dyn Importer> {
    Box::new(HistoryImporter::new(
        ctx,
        MapLimits::default(),
        StageLimits::default(),
    ))
}

impl From<StageError> for ImportError {
    fn from(error: StageError) -> Self {
        match error {
            StageError::TooLarge => Self {
                reason: "the stage is larger than its limit",
                kind: HistoryTransferFailure::TooLarge,
            },
            StageError::Io(_) => Self::internal("a file of the stage failed"),
            StageError::Db(_) => Self::internal("the stage's database failed"),
            StageError::Unreadable => Self::internal("the stage is unreadable"),
        }
    }
}

/// Maps the frames of one archive into its stage.
pub(crate) struct HistoryImporter {
    ctx: ImportContext,
    map_limits: MapLimits,
    stage_limits: StageLimits,
    /// The mapper and the stage, from `begin` on.
    open: Option<(Mapper, StageWriter)>,
    summary: ImportSummary,
    /// What one frame made, between the mapper and the stage.
    rows: Vec<Staged>,
}

impl HistoryImporter {
    /// An importer for the transfer of `ctx`, with these bounds.
    pub(crate) fn new(
        ctx: ImportContext,
        map_limits: MapLimits,
        stage_limits: StageLimits,
    ) -> Self {
        Self {
            ctx,
            map_limits,
            stage_limits,
            open: None,
            summary: ImportSummary::default(),
            rows: Vec::new(),
        }
    }
}

impl Importer for HistoryImporter {
    fn begin(&mut self, info: &ArchiveInfo) -> Result<(), ImportError> {
        // The header's media root key is compared with nothing: this version has no use for
        // it, and the archive's own MAC under the ephemeral key authenticates the header.
        // Its export time is the reference of the attachments' age (`MediaRules`).
        self.summary = ImportSummary::default();
        let media = MediaRules::new(
            self.ctx.now,
            info.backup_time_ms,
            self.ctx.queue_time,
            self.ctx.receive_limit,
        );
        tracing::info!(
            queue_time_stored = self.ctx.queue_time.is_some(),
            window_days = media.window_ms / (24 * 60 * 60 * 1000),
            "the import judges the age of attachments"
        );
        let mapper = Mapper::new(self.ctx.aci, self.ctx.pni, self.ctx.now, self.map_limits)
            .ok_or(ImportError::internal("the account has no user id"))?
            .with_media(media);
        // `create` removes a stage that is there: what an import left that was killed
        // before its commit was recorded.
        let writer = StageWriter::create(
            &self.ctx.dir,
            &StageKey::derive(&self.ctx.key),
            self.stage_limits,
        )?;
        self.open = Some((mapper, writer));
        Ok(())
    }

    fn frame(&mut self, frame: Frame) -> Result<(), ImportError> {
        let Some((mapper, writer)) = self.open.as_mut() else {
            return Err(ImportError::internal("a frame before the import began"));
        };
        self.summary.frames += 1;
        *self.summary.by_kind.entry(frame.kind()).or_insert(0) += 1;
        mapper.frame(frame, &mut self.rows);
        for row in self.rows.drain(..) {
            match row {
                Staged::Contact { n, contact } => writer.contact(n, &contact)?,
                Staged::Chat {
                    idx,
                    conv,
                    chat,
                    pinned,
                } => writer.chat(idx, &conv, &chat, pinned)?,
                Staged::Item { chat, n, msg } => writer.item(chat, n, &msg)?,
            }
        }
        Ok(())
    }

    fn commit(mut self: Box<Self>) -> Result<ImportSummary, ImportError> {
        let Some((mapper, writer)) = self.open.take() else {
            return Err(ImportError::internal("a commit before the import began"));
        };
        let end = mapper.finish();
        // On an error the writer has removed its files: nothing of a stage that is not
        // whole stays.
        let mut writer = writer;
        for idx in &end.unpinned {
            if let Err(e) = writer.unpin(*idx) {
                writer.abort();
                return Err(e.into());
            }
        }
        let totals = writer.commit(&end.chats, self.summary.frames)?;
        let mut summary = std::mem::take(&mut self.summary);
        summary.staged = Some(totals);
        summary.skipped = end.counts;
        Ok(summary)
    }

    fn abort(mut self: Box<Self>) {
        // Without a `begin` (the archive did not open, the read was stopped before its
        // first frame) this import made no file; a `begin` that failed removed what it had
        // made. A stage an EARLIER import left is not this call's to remove: the start of
        // every read does that, before the importer exists.
        if let Some((_, writer)) = self.open.take() {
            writer.abort();
        }
    }
}
