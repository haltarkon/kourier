//! The history transfer of a fresh link (docs/architecture.md §14.2 item 14; Desktop's
//! `ts/services/backups/index.preload.ts` `#doDownloadAndImport` and
//! `WebAPI.preload.ts` `getTransferArchive`): after a device was linked with `backup5` in its
//! linking URL and the phone's user chose to transfer, the phone uploads one encrypted
//! archive of the account's history. This module waits for it, downloads it and reads it
//! through, and survives a crash or a restart at every point.
//!
//! # The states
//!
//! One row of `backend.db` (`history_transfer`, schema 16 to 19), written in the
//! transaction that stores the linked account:
//!
//! - `waiting`: the new device long-polls `GET /v1/devices/transfer_archive?timeout=<s>` on
//!   its authenticated socket ([`POLL`] per request, [`WAIT`] in all from the link). A 204
//!   asks again, at once when the server held the request and after a backoff when it came
//!   back at once ([`QUICK_204`]: a server or a proxy that does not hold the request must
//!   not make this loop spin). A network error, a 5xx, a 408 or a 429 backs off and asks
//!   again; any other 4xx is the server's refusal for good and ends the wait as skipped.
//!   The answer is the archive's place (`{cdn, key}`), `CONTINUE_WITHOUT_UPLOAD` (the phone
//!   will not upload: skipped), `RELINK_REQUESTED` (failed: the UI offers to link again) or
//!   an error value this version does not know (failed: update Kourier). The deadline ends
//!   the wait as skipped, after the server was asked one last time ([`last_ask`]).
//! - `downloading`: `GET <cdn>/attachments/<key>` into `accounts/<uuid>/transfer/archive`
//!   (directory 0700, file 0600), appended to, so the bytes in the file are the offset the
//!   next request (after a broken transfer or a restart) asks from. At most
//!   [`MAX_ARCHIVE_BYTES`]. A 404 is skipped, as in Desktop. The download as a whole is
//!   bounded ([`DownloadBounds`]): it fails when it took too long or stayed too slow.
//! - `reading`: on a blocking thread, [`ArchiveReader::open`] (the HMAC over the whole
//!   file) and then every frame to an [`Importer`], which maps it into the stage ("The
//!   import" below).
//! - `done`, `skipped`, `failed`: over. The key and the archive's place are wiped from the
//!   row in the same write that ends the transfer, the WAL is checkpointed so that no
//!   earlier version of the row is left in it ("Secrets" below), and the file is removed.
//!   The row stays, with its state and reason, until `Command::DismissHistoryTransfer`
//!   removes it: every start and every `Command::Connect` reports it again.
//!
//! [`advance`] is the whole transition table; nothing else decides what follows what.
//! Every failure is an end in this version: nothing is tried again by itself.
//!
//! # The clock
//!
//! The wait's deadline is a time of this device's wall clock, which can be wrong or move.
//! What is left of the wait is never more than the whole wait ([`time_left`]: a deadline
//! further away is a clock that moved back, and the deadline is set anew). A clock that
//! jumped ahead, or a long suspend, ends the wait early: therefore the server is asked once
//! more before a wait ends by its deadline, so an archive that is ready is never skipped.
//!
//! # Receiving waits
//!
//! While the transfer is not over, the account's receive loop takes no envelope off its
//! socket (so none is acknowledged and the server keeps every one) and the Storage Service
//! sync does not run ([`TransferShared::holds`]; Desktop's order in
//! `background.preload.ts`, steps 3 to 5). The link's sync requests, key uploads and the
//! remote config are not held. Every end of the transfer lets receiving start, also a
//! failure: a transfer never blocks an account for good. The user ends a wait or a
//! download with `Command::SkipHistoryTransfer`.
//!
//! # A stop is a crash
//!
//! Stopping the task ([`run`]'s `stop`) changes no state: the row stays as it is and the
//! next start goes on from it, exactly as after a kill. A wait asks again, a download goes
//! on from the length of the file, a read starts over (its importer was aborted, or never
//! committed). One read does not start over: a stop that reaches a read which is past its
//! last frame finds a whole stage, and the stop runs the commit zone for it
//! ([`stop_reading`], "The import" below).
//!
//! One thing tells a stop from a crash. A read that never comes to an end (it ends the
//! process every time) must not hold receiving back at every start for good, so the starts
//! of a read are counted and the transfer fails after [`MAX_READ_STARTS`]. Only a start that
//! follows a read which ended with the process counts: a stop waits for the read to end at
//! its next frame and clears the row's `read_open` mark, so quitting the application during
//! a long read, however often, uses none of the starts.
//!
//! # The import
//!
//! Frames are not trusted until the reader has checked the archive to its last byte, so
//! nothing of an archive leaves the transfer directory before that. The read has two
//! halves with one write between them:
//!
//! 1. **Reading into the stage.** The importer ([`import::HistoryImporter`]) maps every
//!    frame ([`map`]) into rows of a private, encrypted file beside the archive, the stage
//!    ([`stage`]; its key is derived from the transfer's ephemeral key). The blocking
//!    thread writes that one file and nothing else: no row of `backend.db`, no event to the
//!    core, no file in the blob store. An archive that fails, a stop of the account and an
//!    importer that fails remove the stage ([`Importer::abort`]).
//! 2. **The commit zone** ([`commit_import`]): back on the account's thread, ONE write in
//!    `backend.db` records that the stage is whole, with its totals and the archive's frame
//!    count (`import_staged = 1`, migration 0018), and clears `read_open`. This write is
//!    the commit of the import. Before it, an abort, a quit or a kill costs one file and
//!    the next start reads the archive again from its first frame; from it on the content
//!    is authentic and final. Two places run it, both with the summary of a read that
//!    ended in the importer's commit in their hands ([`commit_read`]): the read's own task,
//!    and the stop of that task when the read came to its end while it was being stopped.
//!    A stage that the zone did not record there is removed there: a quit leaves either a
//!    recorded stage or none.
//! 3. **Delivery** ([`deliver`]): the stage goes to the core in small rows of
//!    `core_outbox`, one zone per row with a durable cursor, at most two rows unacknowledged
//!    at a time; a quit or a kill goes on at the cursor. The read ends, and the transfer
//!    becomes `done`, only when the core acknowledged the last row. Nothing of a committed
//!    import is ever withdrawn: a stage that is lost is read again from the archive (ids
//!    are deterministic and the core's ingest is idempotent), and a delivery that cannot
//!    finish ends the transfer as `Incomplete` with what was delivered left in place. An
//!    account that is logged out or unlinked while it is delivered keeps what the core
//!    already has (a known exception to "all or nothing").
//!
//! What a start does with what it finds ([`read`], [`clean_at_start`]):
//!
//! | Stored | Stage file | The start |
//! |---|---|---|
//! | `import_staged = 0` | none | reads the archive from its first frame |
//! | `import_staged = 0` | there (a kill during the read, a kill between the importer's commit and the commit zone, a quit whose read did not end in time) | removes it before anything else ([`clean_at_start`], when the actor starts: before the transfer's task exists), then reads from the first frame. It counts toward [`MAX_READ_STARTS`] only when the read before ended with the process |
//! | `import_staged = 1` | opens with the transfer's key, its totals equal the recorded ones | does not read the archive again and uses no read start: delivery goes on at its cursor. The stage is not counted or checked page by page at a start: a damaged page is refused when it is read (`deliver`, "What a start verifies") |
//! | `import_staged = 1` | gone, does not open, other totals, or a page fails while it is delivered | clears the record, resets the cursor, removes the file and reads the archive again (a read start); what was delivered before is not doubled |
//!
//! A start with a rebuilt `core.db` (`StartMode::Rebuild`) takes the record back as well
//! ([`deliver::rebuild`]): the new core must get the whole import, not its tail.
//!
//! # Secrets
//!
//! The ephemeral key, the CDN key and everything in the archive never reach a log or an
//! event: only states, counts and sizes do (§9.1). The stage holds the archive's content
//! under a key derived from the ephemeral key: the write that ends a transfer wipes that
//! key, and the end removes the stage with the directory.
//!
//! `backend.db` is in WAL mode: a write appends a new version of a page to
//! `backend.db-wal`, and the earlier versions, which hold the key, stay readable in the two
//! files until a checkpoint overwrites them. So the write that ends a transfer is followed
//! by `wal_checkpoint(TRUNCATE)` ([`scrub`]; asked again while a reader holds it back), a
//! start that finds the stored transfer over does the same (a kill between the end and its
//! checkpoint), and an account that is logged out or removed during a transfer has its row
//! deleted and the WAL truncated before the actor ends ([`forget`]). `secure_delete` zeroes
//! the freed bytes of the row's page in the main file. Proven on the files, page by page, in
//! `tests/history_transfer_attack.rs`.

pub(crate) mod deliver;
mod import;
mod map;
pub(crate) mod stage;

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use futures::FutureExt as _;
use kourier_backend_api::BackendEvent;
use kourier_db::rusqlite::{Connection, OptionalExtension};
use kourier_model::{ConnState, HistoryTransfer, HistoryTransferFailure};
use serde::Deserialize;
use tokio::sync::{Notify, watch};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::actor::Shared;
use crate::archive::{ArchiveError, ArchiveInfo, ArchiveKey, ArchiveReader, Frame, Limits};
use crate::connection::backoff::Backoff;
use crate::error::{Error, Result};
use crate::jobs::JobKind;
use crate::net::{
    ChatRequest, ChatResponse, DownloadSink, HttpsHost, HttpsRequest, SignalTransport,
    TransportError, parse_json,
};

pub(crate) use import::{ImportContext, history_importer};
pub(crate) use stage::StageTotals;

/// Makes the importer of one read of an archive. The app's is [`history_importer`]
/// (`Tuning::transfer_importer`); a test of this crate can put another in its place.
pub(crate) type ImporterFactory = fn(ImportContext) -> Box<dyn Importer>;

/// A kill point (`kourier_db::failpoint`) where a read of the archive is about to begin:
/// a stage that no commit recorded is gone, the start is not counted yet, the importer
/// does not exist yet.
pub(crate) const FAILPOINT_BEFORE_IMPORT_READ: &str = "history_import_before_read";

/// A kill point (`kourier_db::failpoint`) between the importer's commit, which left a whole
/// stage on disk, and the commit zone that records it.
pub(crate) const FAILPOINT_BEFORE_IMPORT_COMMIT: &str = "history_import_before_commit";

/// A kill point right after the commit zone: the stage is recorded, nothing else happened.
pub(crate) const FAILPOINT_AFTER_IMPORT_COMMIT: &str = "history_import_after_commit";

/// How long the phone's upload is waited for, from the link (Desktop's `getTransferArchive`
/// default, an hour; the Android phone itself gives up its export far sooner).
pub(crate) const WAIT: Duration = Duration::from_secs(60 * 60);

/// The longest one long-poll asks the server to hold the request (Desktop: five minutes).
pub(crate) const POLL: Duration = Duration::from_secs(5 * 60);

/// A long-poll request's own timeout is what it asked for plus this (Desktop: 15 s).
const POLL_SLACK: Duration = Duration::from_secs(15);

/// A 204 that comes back sooner than this, and sooner than half of what was asked for, was
/// not held by the server (or something between it and us answers in its place): the next
/// request waits for the backoff, like after a failure. A server that holds the request
/// answers 204 only after the whole time asked for, which is at least a second.
pub(crate) const QUICK_204: Duration = Duration::from_secs(1);

/// How many requests the last question to the server may take when the wait's deadline has
/// passed ([`last_ask`]): a second one when the first failed below HTTP (a socket that did
/// not survive a suspend).
const LAST_ASK_TRIES: u32 = 2;

/// The timeout of the last question's request: the second it asks the server to hold it,
/// and this much for the way.
const LAST_ASK_SLACK: Duration = Duration::from_secs(4);

/// The largest archive that is downloaded. An archive of text history is megabytes to a few
/// gigabytes; the reader refuses a stream that decompresses to more than 8 GiB
/// (`archive::Limits::max_decompressed_bytes`), and a file that could be read is never
/// larger than what it decompresses to plus its padding. Enforced by Kourier only (the
/// server and the phone set no limit): a row of the limits table of architecture §13.
pub(crate) const MAX_ARCHIVE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// How long a download may go without a new byte, over every retry, before it fails for
/// good. Counted anew after a restart.
pub(crate) const STALL: Duration = Duration::from_secs(60 * 60);

/// The longest the download of the archive may run in all (the time it ran is stored, so a
/// restart does not start it anew, and the time the application was closed does not
/// count): receiving is held back for as long as it runs, and a body that trickles must not
/// hold it back for days. Two hours bring an archive of the largest size
/// ([`MAX_ARCHIVE_BYTES`]) at 1.2 MiB/s. Kourier's own limit (a row of the limits table of
/// architecture §13).
pub(crate) const DOWNLOAD_DEADLINE: Duration = Duration::from_secs(2 * 60 * 60);

/// The slowest average rate, in bytes per second since the download began, that is still a
/// download: 4 KiB/s, below every link a history of megabytes can be fetched over.
pub(crate) const MIN_DOWNLOAD_RATE: u64 = 4 * 1024;

/// The average rate is not judged before the download is this old (a start is slow).
pub(crate) const RATE_GRACE: Duration = Duration::from_secs(60);

/// How long the average rate may stay under [`MIN_DOWNLOAD_RATE`] before the download fails.
pub(crate) const SLOW_FOR: Duration = Duration::from_secs(10 * 60);

/// How often the download's bounds are looked at.
const BOUNDS_EVERY: Duration = Duration::from_millis(250);

/// The bounds of the download as a whole ([`DOWNLOAD_DEADLINE`], [`MIN_DOWNLOAD_RATE`]);
/// tests shorten them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DownloadBounds {
    /// The longest the download may take in all.
    pub(crate) deadline: Duration,
    /// The lowest average rate (bytes per second since the download began).
    pub(crate) min_rate: u64,
    /// The rate is not judged before the download is this old.
    pub(crate) rate_grace: Duration,
    /// How long the rate may stay under the floor.
    pub(crate) slow_for: Duration,
}

impl DownloadBounds {
    /// What the app uses.
    pub(crate) const DEFAULT: Self = Self {
        deadline: DOWNLOAD_DEADLINE,
        min_rate: MIN_DOWNLOAD_RATE,
        rate_grace: RATE_GRACE,
        slow_for: SLOW_FOR,
    };

    /// Whether a download that has run for `elapsed` and has `done` bytes is out of bounds:
    /// older than the deadline, or under the rate's floor ever since `slow_since` for
    /// longer than allowed. `slow_since` is the caller's memory between two looks: the age
    /// of the download when its average rate was first found under the floor.
    pub(crate) fn exceeded(
        &self,
        elapsed: Duration,
        done: u64,
        slow_since: &mut Option<Duration>,
    ) -> bool {
        if elapsed >= self.deadline {
            return true;
        }
        let floor = u128::from(self.min_rate) * elapsed.as_millis() / 1000;
        if elapsed < self.rate_grace || u128::from(done) >= floor {
            *slow_since = None;
            return false;
        }
        let since = *slow_since.get_or_insert(elapsed);
        elapsed.saturating_sub(since) >= self.slow_for
    }
}

/// What is left of a wait whose stored deadline is `deadline_at` when the wall clock says
/// `now` (both ms since the epoch), and the deadline to store in its place when the stored
/// one cannot be right: a deadline further away than the whole `wait` is a clock that
/// moved back since it was written, and the wait counts anew from now, so that a wrong
/// clock can never make a wait longer than `wait` from the moment it is noticed.
pub(crate) fn time_left(deadline_at: i64, now: i64, wait: Duration) -> (Duration, Option<i64>) {
    let whole = to_i64(u64::try_from(wait.as_millis()).unwrap_or(u64::MAX));
    let left = deadline_at.saturating_sub(now);
    if left > whole {
        (wait, Some(now.saturating_add(whole)))
    } else {
        (Duration::from_millis(to_u64(left)), None)
    }
}

/// How often a checkpoint that a reader held back is asked for again, and the pause between
/// two tries ([`scrub`]). The connection's own `busy_timeout` already waits for a reader
/// inside each try.
const CHECKPOINT_TRIES: u32 = 20;
const CHECKPOINT_RETRY: Duration = Duration::from_millis(250);

/// How long a stop waits for a running read to end at its next frame ([`run`]). The read
/// looks at its stop flag between two frames, but not while the reader opens the archive
/// (one pass over the whole file): a stop that comes then does not wait for it for long.
const READ_STOP_WAIT: Duration = Duration::from_secs(5);

/// How many times the read of one archive is started before the transfer fails. A read
/// that was cut off (the user quit, a crash) starts over at the next start; an archive
/// whose read ends the process every time would otherwise hold receiving back for good.
pub(crate) const MAX_READ_STARTS: u32 = 5;

/// How often download progress goes to the core.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

/// How often the bytes downloaded so far are written to the row.
const PROGRESS_STORED_EVERY: Duration = Duration::from_secs(1);

/// How often the time a download ran is stored while no byte arrives.
const RAN_STORED_EVERY: Duration = Duration::from_secs(30);

/// Downloaded chunks waiting for the file (a slow disk holds the download back).
const DOWNLOAD_QUEUE: usize = 16;

/// The directory of the archive inside the account's directory.
const DIR: &str = "transfer";

/// The archive's file name.
const FILE: &str = "archive";

/// Where a transfer stands, as stored (`history_transfer.state` and `.reason`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Waiting for the phone's upload.
    Waiting,
    /// Downloading the archive.
    Downloading,
    /// Verifying and reading the file.
    Reading,
    /// Read to its end.
    Done,
    /// No transfer, and no error.
    Skipped(SkipReason),
    /// Failed for good.
    Failed(HistoryTransferFailure),
}

/// Why a transfer was skipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SkipReason {
    /// The phone answered `CONTINUE_WITHOUT_UPLOAD`.
    PhoneDeclined,
    /// The user of this device skipped.
    User,
    /// The phone did not answer within [`WAIT`].
    Deadline,
    /// The CDN has no such archive (404), as in Desktop.
    NotFound,
    /// The server refused the long-poll for good (a 4xx that is not 408 or 429): it will
    /// never name an archive to this device.
    Refused,
}

impl SkipReason {
    const ALL: [Self; 5] = [
        Self::PhoneDeclined,
        Self::User,
        Self::Deadline,
        Self::NotFound,
        Self::Refused,
    ];

    const fn as_str(self) -> &'static str {
        match self {
            Self::PhoneDeclined => "phone_declined",
            Self::User => "user",
            Self::Deadline => "deadline",
            Self::NotFound => "not_found",
            Self::Refused => "refused",
        }
    }
}

const FAILURES: [HistoryTransferFailure; 7] = [
    HistoryTransferFailure::LinkAgain,
    HistoryTransferFailure::UpdateApp,
    HistoryTransferFailure::Corrupt,
    HistoryTransferFailure::TooLarge,
    HistoryTransferFailure::Download,
    HistoryTransferFailure::Internal,
    HistoryTransferFailure::Incomplete,
];

const fn failure_str(kind: HistoryTransferFailure) -> &'static str {
    match kind {
        HistoryTransferFailure::LinkAgain => "link_again",
        HistoryTransferFailure::UpdateApp => "update_app",
        HistoryTransferFailure::Corrupt => "corrupt",
        HistoryTransferFailure::TooLarge => "too_large",
        HistoryTransferFailure::Download => "download",
        HistoryTransferFailure::Internal => "internal",
        HistoryTransferFailure::Incomplete => "incomplete",
    }
}

impl Phase {
    /// Done, skipped or failed.
    pub(crate) const fn is_over(self) -> bool {
        matches!(self, Self::Done | Self::Skipped(_) | Self::Failed(_))
    }

    /// `history_transfer.state`.
    const fn state_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Downloading => "downloading",
            Self::Reading => "reading",
            Self::Done => "done",
            Self::Skipped(_) => "skipped",
            Self::Failed(_) => "failed",
        }
    }

    /// `history_transfer.reason`.
    const fn reason_str(self) -> Option<&'static str> {
        match self {
            Self::Skipped(reason) => Some(reason.as_str()),
            Self::Failed(kind) => Some(failure_str(kind)),
            _ => None,
        }
    }

    /// From the stored pair. An end whose reason this version does not know keeps its state
    /// (a newer version wrote it): it is over either way.
    fn parse(state: &str, reason: Option<&str>) -> Option<Self> {
        Some(match state {
            "waiting" => Self::Waiting,
            "downloading" => Self::Downloading,
            "reading" => Self::Reading,
            "done" => Self::Done,
            "skipped" => Self::Skipped(
                SkipReason::ALL
                    .into_iter()
                    .find(|r| Some(r.as_str()) == reason)
                    .unwrap_or(SkipReason::User),
            ),
            "failed" => Self::Failed(
                FAILURES
                    .into_iter()
                    .find(|k| Some(failure_str(*k)) == reason)
                    .unwrap_or(HistoryTransferFailure::Internal),
            ),
            _ => return None,
        })
    }
}

/// Where the archive is, as the server answered. `Debug` hides the key.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ArchivePointer {
    /// The CDN number.
    pub(crate) cdn: u32,
    /// The object key (a credential: whoever has it can fetch the encrypted archive).
    pub(crate) key: String,
}

impl std::fmt::Debug for ArchivePointer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArchivePointer")
            .field("cdn", &self.cdn)
            .finish_non_exhaustive()
    }
}

/// What happened to a transfer: the input of [`advance`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// The server named the archive's place.
    Pointer(ArchivePointer),
    /// `CONTINUE_WITHOUT_UPLOAD`.
    PhoneDeclined,
    /// `RELINK_REQUESTED`.
    RelinkRequested,
    /// The server answered an `error` value this version does not know: a newer protocol.
    UnknownError,
    /// The server answered something that cannot be read (no JSON, a place that is not one).
    Unreadable,
    /// The server refused the long-poll for good.
    Refused,
    /// The wait for the phone ran out.
    DeadlinePassed,
    /// `Command::SkipHistoryTransfer`.
    UserSkipped,
    /// The file is complete.
    Downloaded,
    /// The CDN answered 404.
    NotFound,
    /// The archive is larger than the cap.
    TooLarge,
    /// The download was refused, stalled for too long, took too long in all or stayed too
    /// slow.
    DownloadFailed,
    /// The archive was read to its end.
    Read {
        /// Frames after the header.
        frames: u64,
    },
    /// The archive could not be read.
    ReadFailed(HistoryTransferFailure),
    /// The disk or the database failed.
    Internal,
}

/// The transition table: the phase `step` leads to from `from`, or `None` when it does not
/// apply there (a skip while reading, anything after the end).
pub(crate) fn advance(from: Phase, step: &Step) -> Option<Phase> {
    use HistoryTransferFailure as Failure;
    Some(match (from, step) {
        (Phase::Done | Phase::Skipped(_) | Phase::Failed(_), _) => return None,
        (_, Step::Internal) => Phase::Failed(Failure::Internal),
        (Phase::Waiting, Step::Pointer(_)) => Phase::Downloading,
        (Phase::Waiting, Step::PhoneDeclined) => Phase::Skipped(SkipReason::PhoneDeclined),
        (Phase::Waiting, Step::RelinkRequested) => Phase::Failed(Failure::LinkAgain),
        (Phase::Waiting, Step::UnknownError) => Phase::Failed(Failure::UpdateApp),
        (Phase::Waiting, Step::Unreadable) => Phase::Failed(Failure::Download),
        (Phase::Waiting, Step::Refused) => Phase::Skipped(SkipReason::Refused),
        (Phase::Waiting, Step::DeadlinePassed) => Phase::Skipped(SkipReason::Deadline),
        (Phase::Waiting | Phase::Downloading, Step::UserSkipped) => {
            Phase::Skipped(SkipReason::User)
        }
        (Phase::Downloading, Step::Downloaded) => Phase::Reading,
        (Phase::Downloading, Step::NotFound) => Phase::Skipped(SkipReason::NotFound),
        (Phase::Downloading, Step::TooLarge) => Phase::Failed(Failure::TooLarge),
        (Phase::Downloading, Step::DownloadFailed) => Phase::Failed(Failure::Download),
        (Phase::Reading, Step::Read { .. }) => Phase::Done,
        (Phase::Reading, Step::ReadFailed(kind)) => Phase::Failed(*kind),
        _ => return None,
    })
}

/// The stored transfer. `Debug` shows the state and the sizes only.
pub(crate) struct Stored {
    /// The state.
    pub(crate) phase: Phase,
    /// The ephemeral backup key, until the transfer is over.
    pub(crate) key: Option<Zeroizing<[u8; 32]>>,
    /// The archive's place, while downloading and reading.
    pub(crate) pointer: Option<ArchivePointer>,
    /// The archive's file name inside the transfer directory.
    pub(crate) file: Option<String>,
    /// Bytes downloaded when the row was last written.
    pub(crate) bytes_done: u64,
    /// The size the CDN stated.
    pub(crate) bytes_total: Option<u64>,
    /// Until when the phone's upload is waited for (ms).
    pub(crate) deadline_at: i64,
    /// How long the download has run so far, over every run of the application (the write
    /// that stored the archive's place set it to zero; [`store_progress`] writes it).
    pub(crate) download_ran: Duration,
    /// The frames of the archive, once read.
    pub(crate) frames: Option<u64>,
    /// The stage the commit zone recorded, while there is one.
    pub(crate) staged: Option<Recorded>,
}

/// What the commit zone recorded of a stage (`import_staged = 1`): the stage in the transfer
/// directory is whole and holds this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Recorded {
    /// Rows of every staged chat.
    pub(crate) messages: u64,
    /// Staged chats.
    pub(crate) chats: u64,
    /// The frames the archive held.
    pub(crate) frames: u64,
}

impl std::fmt::Debug for Stored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stored")
            .field("phase", &self.phase)
            .field("bytes_done", &self.bytes_done)
            .field("bytes_total", &self.bytes_total)
            .field("frames", &self.frames)
            .field("staged", &self.staged)
            .finish_non_exhaustive()
    }
}

impl Stored {
    /// The state as the core and a UI see it.
    pub(crate) fn report(&self) -> HistoryTransfer {
        match self.phase {
            Phase::Waiting => HistoryTransfer::Waiting,
            Phase::Downloading => HistoryTransfer::Downloading {
                done: self.bytes_done,
                total: self.bytes_total,
            },
            Phase::Reading => HistoryTransfer::Reading,
            Phase::Done => HistoryTransfer::Done,
            Phase::Skipped(_) => HistoryTransfer::Skipped,
            Phase::Failed(kind) => HistoryTransfer::Failed(kind),
        }
    }
}

fn to_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn to_u64(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

/// Starts a transfer: the `waiting` row with the phone's key. Called in the zone that stores
/// the linked account, so a link either has both or neither.
pub(crate) fn begin(conn: &Connection, key: &[u8; 32], now: i64, wait: Duration) -> Result<()> {
    let deadline = now.saturating_add(to_i64(u64::try_from(wait.as_millis()).unwrap_or(u64::MAX)));
    conn.execute(
        "INSERT INTO history_transfer(id, state, key, deadline_at, started_at, updated_at)
         VALUES (1, 'waiting', ?1, ?2, ?3, ?3)",
        (key.as_slice(), deadline, now),
    )?;
    Ok(())
}

/// The stored transfer, if this account ever had one.
pub(crate) fn load(conn: &Connection) -> Result<Option<Stored>> {
    type Row = (
        String,
        Option<String>,
        Option<Zeroizing<Vec<u8>>>,
        Option<u32>,
        Option<String>,
        Option<String>,
        i64,
        Option<i64>,
        i64,
        Option<i64>,
        i64,
        (bool, i64, i64, i64),
    );
    let row: Option<Row> = conn
        .query_row(
            "SELECT state, reason, key, cdn, cdn_key, file, bytes_done, bytes_total,
               deadline_at, frames, download_ran_ms, import_staged, stage_messages,
               stage_chats, stage_frames FROM history_transfer WHERE id = 1",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get::<_, Option<Vec<u8>>>(2)?.map(Zeroizing::new),
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                    r.get(10)?,
                    (r.get(11)?, r.get(12)?, r.get(13)?, r.get(14)?),
                ))
            },
        )
        .optional()?;
    let Some((
        state,
        reason,
        key,
        cdn,
        cdn_key,
        file,
        done,
        total,
        deadline_at,
        frames,
        download_ran_ms,
        (staged, stage_messages, stage_chats, stage_frames),
    )) = row
    else {
        return Ok(None);
    };
    let phase =
        Phase::parse(&state, reason.as_deref()).ok_or(Error::Decode("history transfer state"))?;
    let key = match key {
        Some(key) => Some(Zeroizing::new(
            <[u8; 32]>::try_from(key.as_slice())
                .map_err(|_| Error::Decode("history transfer key"))?,
        )),
        None => None,
    };
    Ok(Some(Stored {
        phase,
        key,
        pointer: cdn
            .zip(cdn_key)
            .map(|(cdn, key)| ArchivePointer { cdn, key }),
        file,
        bytes_done: to_u64(done),
        bytes_total: total.map(to_u64),
        deadline_at,
        download_ran: Duration::from_millis(to_u64(download_ran_ms)),
        frames: frames.map(to_u64),
        staged: staged.then(|| Recorded {
            messages: to_u64(stage_messages),
            chats: to_u64(stage_chats),
            frames: to_u64(stage_frames),
        }),
    }))
}

/// Applies `step` to the stored transfer and returns the phase it led to; `None` when there
/// is no transfer or the step does not apply to its phase (nothing is written then). An end
/// wipes the key, the archive's place and the file name in the same write: first
/// overwritten with zeroes of their length (the bytes in the row's page are replaced where
/// they stand), then set to NULL (`secure_delete` zeroes what that frees). The earlier
/// versions of the page in the WAL are the caller's to remove ([`scrub`]). An end also
/// takes back the record of a stage: the stage goes with the transfer directory, and its
/// key is derived from the key this write wipes.
pub(crate) fn apply(conn: &Connection, step: &Step, now: i64) -> Result<Option<Phase>> {
    let Some(stored) = load(conn)? else {
        return Ok(None);
    };
    let Some(next) = advance(stored.phase, step) else {
        return Ok(None);
    };
    if next.is_over() {
        let frames = match step {
            Step::Read { frames } => Some(to_i64(*frames)),
            _ => None,
        };
        conn.execute(
            "UPDATE history_transfer SET key = zeroblob(length(key)),
               cdn_key = CAST(zeroblob(length(CAST(cdn_key AS BLOB))) AS TEXT) WHERE id = 1",
            [],
        )?;
        conn.execute(
            "UPDATE history_transfer SET state = ?1, reason = ?2, key = NULL, cdn = NULL,
               cdn_key = NULL, file = NULL, frames = ?3, read_open = 0, import_staged = 0,
               updated_at = ?4
             WHERE id = 1",
            (next.state_str(), next.reason_str(), frames, now),
        )?;
        // The rows of a delivery that are still on their way to the core stay in
        // `core_outbox`; nobody waits for them any more.
        conn.execute("DELETE FROM history_delivery", [])?;
    } else if let Step::Pointer(pointer) = step {
        conn.execute(
            "UPDATE history_transfer SET state = ?1, cdn = ?2, cdn_key = ?3, file = ?4,
               bytes_done = 0, bytes_total = NULL, download_ran_ms = 0, updated_at = ?5
             WHERE id = 1",
            (next.state_str(), pointer.cdn, &pointer.key, FILE, now),
        )?;
    } else {
        conn.execute(
            "UPDATE history_transfer SET state = ?1, updated_at = ?2 WHERE id = 1",
            (next.state_str(), now),
        )?;
    }
    Ok(Some(next))
}

/// A read of the archive starts: marks it open and returns how many starts count so far.
/// The first start counts, and so does every start that finds the read before it still
/// marked open, which is a read that ended with the process (a crash, a kill). A start
/// after a read that was stopped in an orderly way ([`note_read_stopped`]) does not count.
fn note_read_start(conn: &Connection, now: i64) -> Result<u32> {
    Ok(conn.query_row(
        "UPDATE history_transfer
            SET read_starts = read_starts + (read_starts = 0 OR read_open = 1),
                read_open = 1, updated_at = ?1
          WHERE id = 1 RETURNING read_starts",
        [now],
        |r| r.get(0),
    )?)
}

/// A read was stopped in an orderly way (its importer was aborted and nothing of it is
/// left): the next start of the read does not count toward [`MAX_READ_STARTS`].
fn note_read_stopped(conn: &Connection, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE history_transfer SET read_open = 0, updated_at = ?1
          WHERE id = 1 AND state = 'reading'",
        [now],
    )?;
    Ok(())
}

/// THE COMMIT of an import (module docs, "The import"): records that the stage in the
/// transfer directory is whole and came from an archive that was authentic to its last
/// byte, with what it holds and the frames the archive had, and that the read is no longer
/// open. One write; `false` (and nothing written) when the transfer is not reading or a
/// stage is recorded already.
pub(crate) fn commit_import(
    conn: &Connection,
    totals: &StageTotals,
    frames: u64,
    now: i64,
) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE history_transfer
            SET import_staged = 1, stage_messages = ?1, stage_chats = ?2, stage_frames = ?3,
                read_open = 0, updated_at = ?4
          WHERE id = 1 AND state = 'reading' AND import_staged = 0",
        (
            to_i64(totals.messages),
            to_i64(totals.chats),
            to_i64(frames),
            now,
        ),
    )?;
    Ok(changed == 1)
}

/// Sets the wait's deadline anew ([`time_left`] found the stored one out of reach).
fn store_deadline(conn: &Connection, deadline_at: i64, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE history_transfer SET deadline_at = ?1, updated_at = ?2
          WHERE id = 1 AND state = 'waiting'",
        (deadline_at, now),
    )?;
    Ok(())
}

/// `Command::DismissHistoryTransfer`: removes the row of a transfer that is over and says
/// whether there was one. A transfer that is not over stays (nothing to dismiss yet).
pub(crate) fn dismiss(conn: &Connection) -> Result<bool> {
    let removed = conn.execute(
        "DELETE FROM history_transfer WHERE id = 1 AND state IN ('done', 'skipped', 'failed')",
        [],
    )?;
    Ok(removed > 0)
}

/// Removes the transfer's row whatever its state and says whether it held a key or an
/// archive's place: for an account that is logged out or removed ([`forget`]).
fn delete_row(conn: &Connection) -> Result<bool> {
    let secret: Option<bool> = conn
        .query_row(
            "SELECT key IS NOT NULL OR cdn_key IS NOT NULL FROM history_transfer WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if secret.is_some() {
        conn.execute(
            "UPDATE history_transfer SET key = zeroblob(length(key)),
               cdn_key = CAST(zeroblob(length(CAST(cdn_key AS BLOB))) AS TEXT) WHERE id = 1",
            [],
        )?;
        conn.execute("DELETE FROM history_transfer WHERE id = 1", [])?;
    }
    conn.execute("DELETE FROM history_delivery", [])?;
    Ok(secret.unwrap_or(false))
}

/// Stores how far the download is (for what a UI shows after a restart) and how long it has
/// run in all (for its bounds after a restart, [`DownloadBounds`]).
fn store_progress(
    conn: &Connection,
    done: u64,
    total: Option<u64>,
    ran: Duration,
    now: i64,
) -> Result<()> {
    let ran = to_i64(u64::try_from(ran.as_millis()).unwrap_or(u64::MAX));
    conn.execute(
        "UPDATE history_transfer SET bytes_done = ?1, bytes_total = coalesce(?2, bytes_total),
           download_ran_ms = ?3, updated_at = ?4 WHERE id = 1 AND state = 'downloading'",
        (to_i64(done), total.map(to_i64), ran, now),
    )?;
    Ok(())
}

/// How long a download has run: what was stored of its earlier runs and, on a monotonic
/// clock, this run. The time the application was closed is in neither.
#[derive(Clone, Copy)]
struct Ran {
    before: Duration,
    began: tokio::time::Instant,
}

impl Ran {
    fn starting_from(before: Duration) -> Self {
        Self {
            before,
            began: tokio::time::Instant::now(),
        }
    }

    fn so_far(&self) -> Duration {
        self.before.saturating_add(self.began.elapsed())
    }
}

/// The directory of an account's archive: beside its `backend.db`.
pub(crate) fn dir_beside(backend_db: &Path) -> PathBuf {
    backend_db
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(DIR)
}

/// Removes the transfer directory with whatever it holds (blocking; a missing one is fine).
pub(crate) fn remove_dir(dir: &Path) {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => tracing::debug!("the transfer directory was removed"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(error = %e.kind(), "removing the transfer directory failed"),
    }
}

/// What a starting actor does about the transfer directory (blocking): a transfer that is
/// downloading or reading keeps its file, everything else leaves no file behind (a leftover
/// of an account that was wiped, of a transfer that ended just before a crash, or of a wait).
/// Of a transfer that goes on, a stage that no commit recorded is removed here, before
/// anything else runs: it is what a read left that ended with the process.
pub(crate) fn clean_at_start(dir: &Path, stored: Option<&Stored>) {
    let keeps = stored.is_some_and(|s| matches!(s.phase, Phase::Downloading | Phase::Reading));
    if !keeps {
        remove_dir(dir);
    } else if stored.is_some_and(|s| s.staged.is_none()) {
        remove_unrecorded_stage(dir);
    }
}

/// Removes a stage that no commit recorded (blocking; none there is fine).
fn remove_unrecorded_stage(dir: &Path) {
    if stage::path_in(dir).exists() {
        tracing::info!("a stage that was never committed is removed");
    }
    if let Err(e) = stage::remove(dir) {
        tracing::warn!(error = %e, "removing a stage that was never committed failed");
    }
}

/// The transfer's part of the actor's shared state.
pub(crate) struct TransferShared {
    /// `accounts/<uuid>/transfer`.
    dir: PathBuf,
    /// Whether a transfer holds receiving and the Storage Service sync back.
    hold: watch::Sender<bool>,
    /// The user's skip, for the running task.
    skip: Notify,
    /// The read that is running on a blocking thread, with its stop flag: parked here so
    /// that a stop of the task can wait for the read's end ([`run`]).
    reading: std::cell::RefCell<Option<Reading>>,
    /// Whether a transfer of this run ended `done` (its archive was imported): the link then
    /// does not wait for the first Storage Service sync (memory only; a restart in the link's
    /// tail has no login waiting). Forgotten when a login starts ([`Self::forget_import`]).
    imported: std::cell::Cell<bool>,
}

/// A read of the archive on its blocking thread.
struct Reading {
    /// Ends the read at its next frame, also when this is dropped with its actor.
    stop: StopOnDrop,
    task: tokio::task::JoinHandle<std::result::Result<ImportSummary, ReadEnd>>,
}

/// Sets `stop` when dropped: a read whose owner went away stops at its next frame.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl TransferShared {
    /// For an actor whose stored transfer is active (`held`) or not.
    pub(crate) fn new(dir: PathBuf, held: bool) -> Self {
        Self {
            dir,
            hold: watch::Sender::new(held),
            skip: Notify::new(),
            reading: std::cell::RefCell::new(None),
            imported: std::cell::Cell::new(false),
        }
    }

    /// Whether a transfer of this run ended with its archive imported.
    pub(crate) fn imported(&self) -> bool {
        self.imported.get()
    }

    /// A login starts: it is a new link, and what an earlier transfer of this run imported
    /// says nothing about it (its `Done` waits for the Storage Service sync again unless its
    /// own transfer ends `done`).
    pub(crate) fn forget_import(&self) {
        self.imported.set(false);
    }

    /// Test hook: as if a transfer of this run had ended `done` (`release`).
    #[cfg(test)]
    pub(crate) fn mark_imported(&self) {
        self.imported.set(true);
    }

    /// The transfer directory.
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether receiving is held back now.
    pub(crate) fn holds(&self) -> bool {
        *self.hold.borrow()
    }

    /// Sees the hold begin and end.
    pub(crate) fn watch(&self) -> watch::Receiver<bool> {
        self.hold.subscribe()
    }

    /// A link stored a transfer: receiving waits from here on.
    pub(crate) fn hold(&self) {
        self.hold.send_replace(true);
    }

    fn release(&self) {
        self.hold.send_replace(false);
    }

    /// `Command::SkipHistoryTransfer` for a transfer that waits or downloads.
    pub(crate) fn skip(&self) {
        self.skip.notify_one();
    }

    /// Whether a skip came that nobody has taken yet, and takes it: a skip that arrives
    /// while the task is not waiting for one (the finished download syncs its file) is kept
    /// by the `Notify`, and the download looks here once before it becomes a read, which
    /// cannot be skipped.
    fn take_pending_skip(&self) -> bool {
        self.skip.notified().now_or_never().is_some()
    }
}

/// What an import made of an archive that was read to its end. Numbers only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ImportSummary {
    /// Frames after the header.
    pub(crate) frames: u64,
    /// The frames by [`Frame::kind`].
    pub(crate) by_kind: BTreeMap<&'static str, u64>,
    /// What the importer left in the stage; `None` for an importer that stages nothing.
    pub(crate) staged: Option<StageTotals>,
    /// What was not imported, or imported with less than it had, by reason
    /// (`map::reason`).
    pub(crate) skipped: BTreeMap<&'static str, u64>,
}

/// An importer could not take what it was given. Log-safe: a reason, never content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the import failed: {reason}")]
pub(crate) struct ImportError {
    /// What went wrong.
    pub(crate) reason: &'static str,
    /// How the transfer fails for it: what the user can do about it.
    pub(crate) kind: HistoryTransferFailure,
}

impl ImportError {
    /// A failure of this device (the disk, the database): `Internal`.
    pub(crate) const fn internal(reason: &'static str) -> Self {
        Self {
            reason,
            kind: HistoryTransferFailure::Internal,
        }
    }
}

/// What takes the frames of one archive. The contract is the reader's rule that frames are
/// not trusted until the end (`archive` module docs): the MAC of what the frames were
/// decrypted from is checked only when the reader returns `Ok(None)`.
///
/// The calls, on one blocking thread: [`begin`](Self::begin) once, [`frame`](Self::frame)
/// for every frame in the archive's order, and then exactly one of
/// [`commit`](Self::commit) (only after the reader returned `Ok(None)`) or
/// [`abort`](Self::abort) (the reader failed, the importer itself returned an error, or the
/// read was stopped). `abort` may come without a `begin`: the archive did not open, or the
/// read was stopped while it was opened.
///
/// A kill takes the place of both ends. An importer that stores anything must therefore
/// treat what it stored as provisional until `commit`, and remove in `begin` whatever an
/// earlier import of this account left without reaching `commit` or `abort`.
pub(crate) trait Importer: Send {
    /// The archive opened: its header. Nothing of the archive is trusted yet.
    fn begin(&mut self, info: &ArchiveInfo) -> std::result::Result<(), ImportError>;

    /// The next frame. Provisional until [`commit`](Self::commit).
    fn frame(&mut self, frame: Frame) -> std::result::Result<(), ImportError>;

    /// The archive was authentic to its last byte: what was imported becomes final.
    fn commit(self: Box<Self>) -> std::result::Result<ImportSummary, ImportError>;

    /// Nothing of this archive may stay: remove what was imported from it.
    fn abort(self: Box<Self>);
}

/// How a read of an archive ended without a committed import.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadEnd {
    /// It was stopped (the actor is going away); the importer was aborted.
    Stopped,
    /// The archive could not be read.
    Archive(ArchiveError),
    /// The importer refused.
    Import(ImportError),
}

/// Blocking: opens the archive at `path` and hands every frame to `importer`, which is
/// committed only after the reader returned `Ok(None)` and aborted in every other case
/// ([`Importer`]). `stop` is looked at between frames; `frame_delay` slows a test's read.
pub(crate) fn import_archive(
    path: &Path,
    key: &ArchiveKey,
    limits: Limits,
    mut importer: Box<dyn Importer>,
    stop: &AtomicBool,
    frame_delay: Duration,
) -> std::result::Result<ImportSummary, ReadEnd> {
    let mut reader = match ArchiveReader::open(path, key, limits) {
        Ok(reader) => reader,
        Err(e) => {
            importer.abort();
            return Err(ReadEnd::Archive(e));
        }
    };
    // A stop that came while the archive was opened (one pass over the whole file): the
    // importer has made nothing yet and must not start to.
    if stop.load(Ordering::Acquire) {
        importer.abort();
        return Err(ReadEnd::Stopped);
    }
    if let Err(e) = importer.begin(reader.info()) {
        importer.abort();
        return Err(ReadEnd::Import(e));
    }
    loop {
        if stop.load(Ordering::Acquire) {
            importer.abort();
            return Err(ReadEnd::Stopped);
        }
        match reader.next_frame() {
            Ok(Some(frame)) => {
                if let Err(e) = importer.frame(frame) {
                    importer.abort();
                    return Err(ReadEnd::Import(e));
                }
            }
            // Only here is the archive known to be the phone's, whole and unchanged.
            Ok(None) => return importer.commit().map_err(ReadEnd::Import),
            Err(e) => {
                importer.abort();
                return Err(ReadEnd::Archive(e));
            }
        }
        if !frame_delay.is_zero() {
            std::thread::sleep(frame_delay);
        }
    }
}

/// What the user can do about an archive that did not read.
pub(crate) fn failure_of(error: ArchiveError) -> HistoryTransferFailure {
    match error {
        // A newer phone writes a format this version does not know: "update Kourier".
        ArchiveError::UnsupportedVersion { .. } => HistoryTransferFailure::UpdateApp,
        ArchiveError::Io(_) => HistoryTransferFailure::Internal,
        ArchiveError::FrameTooLong { .. }
        | ArchiveError::TooLarge { .. }
        | ArchiveError::TooManyFrames { .. }
        | ArchiveError::FrameTooComplex { .. } => HistoryTransferFailure::TooLarge,
        ArchiveError::Hmac
        | ArchiveError::Truncated
        | ArchiveError::BadHeader
        | ArchiveError::BadPadding
        | ArchiveError::BadGzip
        | ArchiveError::BadFraming
        | ArchiveError::BadProtobuf
        | ArchiveError::MissingInfo => HistoryTransferFailure::Corrupt,
    }
}

/// The answer of `GET /v1/devices/transfer_archive` (Desktop's `TransferArchiveSchema`).
#[derive(Deserialize)]
struct Answer {
    cdn: Option<u32>,
    key: Option<String>,
    error: Option<String>,
}

/// What a 200 of the long-poll says.
fn read_answer(response: &ChatResponse) -> Step {
    let Ok(answer) = parse_json::<Answer>(response) else {
        return Step::Unreadable;
    };
    match (answer.error.as_deref(), answer.cdn, answer.key) {
        (Some("CONTINUE_WITHOUT_UPLOAD"), ..) => Step::PhoneDeclined,
        (Some("RELINK_REQUESTED"), ..) => Step::RelinkRequested,
        // A value a newer server or phone sends and this version does not know: not a
        // failed download but a protocol this version is too old for.
        (Some(_), ..) => Step::UnknownError,
        (None, Some(cdn), Some(key)) if crate::attachment::valid_cdn_key(&key) => {
            Step::Pointer(ArchivePointer { cdn, key })
        }
        _ => Step::Unreadable,
    }
}

/// Whether `status` of the long-poll is the server's refusal for good: a 4xx other than 408
/// (it gave the request up) and 429 (too many requests for now), which are asked again.
fn refused_for_good(status: http::StatusCode) -> bool {
    status.is_client_error()
        && status != http::StatusCode::REQUEST_TIMEOUT
        && status != http::StatusCode::TOO_MANY_REQUESTS
}

/// Whether a 204 that came `after` a request that asked the server to hold it for `asked`
/// came back at once ([`QUICK_204`]).
fn came_at_once(after: Duration, asked: Duration) -> bool {
    after < QUICK_204.min(asked / 2)
}

/// Tells the core where the transfer stands (ephemeral; a core that is gone hears nothing).
async fn report<T: SignalTransport>(shared: &Shared<T>, state: HistoryTransfer) {
    let _ = shared.emit(BackendEvent::HistoryTransfer(state)).await;
}

/// Tells the core the stored state again, whatever it is: an actor that starts and every
/// `Command::Connect` (sent after the first UI frame) say where the transfer stands, also
/// when it is over, so a UI that came up later, or was not listening when it ended, learns
/// of a failure and its kind. Nothing is said when there is no row (no transfer ever, or it
/// was dismissed).
pub(crate) async fn report_stored<T: SignalTransport>(shared: &Shared<T>) {
    match shared.db.read(load) {
        Ok(Some(stored)) => report(shared, stored.report()).await,
        Ok(None) => {}
        Err(e) => tracing::warn!(error = %e, "reading the history transfer failed"),
    }
}

/// One `wal_checkpoint(TRUNCATE)` of `backend.db`: `true` when the WAL is empty afterwards.
pub(crate) fn checkpoint(db: &kourier_db::ZoneDb) -> bool {
    match db.checkpoint_truncate() {
        Ok(done) => done,
        Err(e) => {
            tracing::warn!(error = %e, "the checkpoint after a history transfer failed");
            false
        }
    }
}

/// Removes the earlier versions of the transfer's row from the WAL: the write before this
/// took the key and the archive's place out of the row, and until a checkpoint has copied
/// that page into the database file and truncated the WAL, the versions that hold them are
/// readable in the two files (module docs, "Secrets"). A reader holds a checkpoint back for
/// a moment at most; it is asked again until it ran. One that never runs is warned about and
/// left to the next start, which does the same before anything else.
pub(crate) async fn scrub<T: SignalTransport>(shared: &Shared<T>) {
    for attempt in 1..=CHECKPOINT_TRIES {
        if checkpoint(&shared.db) {
            tracing::debug!(
                attempt,
                "the WAL holds nothing of the history transfer any more"
            );
            return;
        }
        tokio::time::sleep(CHECKPOINT_RETRY).await;
    }
    tracing::warn!("the checkpoint after a history transfer was held back; the next start does it");
}

/// The account is logged out or removed: nothing of a transfer may stay in its files until
/// the core deletes them. Removes the row, whatever its state, and truncates the WAL when
/// the row held the key or the archive's place. The caller has stopped the task and removes
/// the archive file.
pub(crate) async fn forget<T: SignalTransport>(shared: &Shared<T>) {
    match shared.db.zone(|tx| delete_row(tx)) {
        Ok(true) => scrub(shared).await,
        Ok(false) => {}
        Err(e) => tracing::warn!(error = %e, "removing the history transfer failed"),
    }
}

/// `Command::DismissHistoryTransfer`: the user has seen how the transfer ended. Removes the
/// row of a transfer that is over; from then on nothing is reported at a start or a connect.
pub(crate) fn dismiss_stored<T: SignalTransport>(shared: &Shared<T>) {
    match shared.db.zone(|tx| dismiss(tx)) {
        Ok(true) => tracing::info!("the ended history transfer was dismissed"),
        Ok(false) => tracing::debug!("no ended history transfer to dismiss"),
        Err(e) => tracing::warn!(error = %e, "dismissing the history transfer failed"),
    }
}

/// Runs the stored transfer to its end, or until `stop` (which changes no state: module
/// docs). Whatever the end, receiving is let go when this returns without having been
/// stopped. A stop while the archive is read waits for the read to end at its next frame,
/// so its importer has dropped what it had, and marks the read as stopped in an orderly way.
pub(crate) async fn run<T: SignalTransport>(shared: Rc<Shared<T>>, stop: CancellationToken) {
    tokio::select! {
        () = stop.cancelled() => {
            tracing::debug!("the history transfer was stopped");
            stop_reading(&shared).await;
        }
        () = drive(&shared) => {}
    }
}

/// The orderly end of a read that the stopped task left running (module docs, "A stop is a
/// crash"): asks it to stop, waits for it, and clears the row's `read_open` mark. A read
/// that does not end in time keeps its mark and counts as cut off.
///
/// The stop can reach a read that is past its last frame: the reader has checked the
/// archive to its last byte, the importer has committed, and the task ends with a summary
/// and a whole stage on disk. That read is not thrown away and its stage is not left
/// lying unrecorded while the application is closed: the commit zone runs here, as it
/// would have a moment later in the task that was stopped, and the next start goes on
/// after it. Should the zone not record the stage, the stage is removed.
async fn stop_reading<T: SignalTransport>(shared: &Shared<T>) {
    let Some(Reading { stop, task }) = shared.transfer.reading.borrow_mut().take() else {
        return;
    };
    // The read looks at the flag before its next frame.
    drop(stop);
    let ended = match tokio::time::timeout(READ_STOP_WAIT, task).await {
        Ok(ended) => ended,
        Err(_) => {
            tracing::warn!("the archive's read did not stop in time");
            return;
        }
    };
    if let Ok(Ok(summary)) = ended {
        match commit_read(shared, &summary) {
            // The commit zone cleared `read_open` itself.
            Ok(_) => {
                tracing::debug!(
                    "the archive's read ended as it was stopped; its stage is recorded"
                );
                return;
            }
            Err(_) => {
                // Not recorded (the zone failed, or a test's importer staged nothing): a
                // stage that nothing records does not stay behind a quit.
                let dir = shared.transfer.dir.clone();
                let _ = tokio::task::spawn_blocking(move || remove_unrecorded_stage(&dir)).await;
            }
        }
    }
    let now = shared.now();
    match shared.db.zone(|tx| note_read_stopped(tx, now)) {
        Ok(()) => tracing::debug!("the archive's read was stopped in an orderly way"),
        Err(e) => tracing::warn!(error = %e, "storing the read's stop failed"),
    }
}

/// THE COMMIT ZONE, with what stands around it: `summary` is what a read returned whose
/// reader gave `Ok(None)` (the archive was the phone's, whole and unchanged) and whose
/// importer committed (a whole stage is on disk). Records the stage ([`commit_import`]);
/// this one write makes the import final, and nothing of it was written anywhere but the
/// stage before. `Err` is the step the read ends with otherwise: `Step::Read` for an
/// importer that staged nothing, `Step::Internal` when the zone did not record the stage.
///
/// No await in here, and none between a read's result and this call: a stop cannot fall
/// between the end of a read and the record of its stage.
fn commit_read<T: SignalTransport>(
    shared: &Shared<T>,
    summary: &ImportSummary,
) -> std::result::Result<Recorded, Step> {
    // Counts by kind and by reason only: nothing of a frame is ever logged.
    tracing::info!(
        frames = summary.frames,
        kinds = ?summary.by_kind,
        skipped = ?summary.skipped,
        "the transfer archive was read to its end"
    );
    let Some(totals) = summary.staged else {
        // An importer that stages nothing (a test's): nothing to commit, nothing to deliver.
        return Err(Step::Read {
            frames: summary.frames,
        });
    };
    kourier_db::failpoint::hit(FAILPOINT_BEFORE_IMPORT_COMMIT);
    let now = shared.now();
    match shared
        .db
        .zone(|tx| commit_import(tx, &totals, summary.frames, now))
    {
        Ok(true) => {}
        Ok(false) => {
            tracing::error!("the commit of the history import did not apply");
            return Err(Step::Internal);
        }
        Err(e) => {
            tracing::error!(error = %e, "storing the commit of the history import failed");
            return Err(Step::Internal);
        }
    }
    kourier_db::failpoint::hit(FAILPOINT_AFTER_IMPORT_COMMIT);
    tracing::info!(
        messages = totals.messages,
        chats = totals.chats,
        shown_chats = totals.shown_chats,
        contacts = totals.contacts,
        bytes = totals.bytes,
        "the history import is committed"
    );
    Ok(Recorded {
        messages: totals.messages,
        chats: totals.chats,
        frames: summary.frames,
    })
}

async fn drive<T: SignalTransport>(shared: &Rc<Shared<T>>) {
    let mut first = true;
    loop {
        let stored = match shared.db.read(load) {
            Ok(Some(stored)) if !stored.phase.is_over() => stored,
            Ok(_) => break,
            Err(e) => {
                // Nothing can be stored either: let this run receive, the next start tries
                // the transfer again from the row.
                tracing::error!(error = %e, "reading the history transfer failed");
                break;
            }
        };
        if std::mem::take(&mut first) {
            // Where this run takes the transfer up (every later state is reported below).
            report(shared, stored.report()).await;
        }
        let step = match stored.phase {
            Phase::Waiting => wait_for_phone(shared, &stored).await,
            Phase::Downloading => download(shared, &stored).await,
            Phase::Reading => read(shared, &stored).await,
            Phase::Done | Phase::Skipped(_) | Phase::Failed(_) => break,
        };
        let now = shared.now();
        let applied = shared.db.zone(|tx| apply(tx, &step, now));
        let next = match applied {
            Ok(Some(next)) => next,
            Ok(None) => {
                tracing::error!(from = ?stored.phase, "a history transfer step did not apply");
                break;
            }
            Err(e) => {
                tracing::error!(error = %e, "storing the history transfer failed");
                break;
            }
        };
        tracing::info!(
            from = stored.phase.state_str(),
            to = next.state_str(),
            reason = next.reason_str(),
            "history transfer"
        );
        if next.is_over() {
            // The key and the archive's place are gone from the row: now from the WAL, which
            // still holds the row's earlier versions, and then the file.
            if shared.tuning.transfer_end_checkpoint {
                scrub(shared).await;
            }
            let dir = shared.transfer.dir.clone();
            let _ = tokio::task::spawn_blocking(move || remove_dir(&dir)).await;
        }
        match shared.db.read(load) {
            Ok(Some(stored)) => report(shared, stored.report()).await,
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "reading the history transfer failed"),
        }
    }
    release(shared);
}

/// The transfer is over (or cannot go on in this run): live receiving and the Storage
/// Service sync start.
fn release<T: SignalTransport>(shared: &Shared<T>) {
    // The wait for the phone's contact sync counts from here: its answer sat in the held
    // envelopes, and a wait that ran out meanwhile would report the link done without it.
    let now = shared.now();
    let wait = to_i64(u64::try_from(crate::contact_sync::LINK_WAIT.as_millis()).unwrap_or(0));
    let moved = shared.db.zone(|tx| -> Result<usize> {
        Ok(tx.execute(
            "UPDATE jobs SET next_at = max(next_at, ?1) WHERE kind = ?2",
            (now.saturating_add(wait), JobKind::ContactSyncWait.as_str()),
        )?)
    });
    if let Err(e) = moved {
        tracing::warn!(error = %e, "moving the contact sync wait failed");
    }
    // An imported archive named every chat: the link's `Done` does not wait for the first
    // Storage Service sync (Desktop: "If backup was imported, we do not need to await the
    // storage service sync"); the sync still runs and wins where it differs (D10).
    match shared.db.read(|c| -> Result<bool> {
        Ok(c.query_row(
            "SELECT EXISTS(SELECT 1 FROM history_transfer WHERE id = 1 AND state = 'done')",
            [],
            |r| r.get(0),
        )?)
    }) {
        Ok(done) => shared.transfer.imported.set(done),
        Err(e) => tracing::warn!(error = %e, "reading the history transfer's end failed"),
    }
    shared.transfer.release();
    shared.jobs_wake.notify_one();
    tracing::info!("history transfer over: receiving starts");
}

/// `waiting`: the long-poll, until the server answers or refuses, the deadline passes or the
/// user skips.
async fn wait_for_phone<T: SignalTransport>(shared: &Rc<Shared<T>>, stored: &Stored) -> Step {
    let mut conn = shared.conn_watch();
    let mut backoff = Backoff::new();
    let mut deadline_at = stored.deadline_at;
    loop {
        let now = shared.now();
        let (left, moved) = time_left(deadline_at, now, shared.tuning.transfer_wait);
        if let Some(new) = moved {
            tracing::warn!("the history transfer's deadline was out of reach; counted anew");
            deadline_at = new;
            if let Err(e) = shared.db.zone(|tx| store_deadline(tx, new, now)) {
                tracing::warn!(error = %e, "storing the transfer's deadline failed");
            }
        }
        if left.is_zero() {
            // By this device's clock. The server has the last word.
            return last_ask(shared, &mut conn).await;
        }
        let socket = shared.socket.borrow().clone();
        let Some(socket) = socket else {
            // Not connected: the wait goes on, and so does its deadline.
            tokio::select! {
                () = shared.transfer.skip.notified() => return Step::UserSkipped,
                // The clock is read again at the top: it decides, not this timer.
                () = tokio::time::sleep(left) => continue,
                changed = conn.changed() => {
                    if changed.is_err() {
                        return Step::Internal;
                    }
                    continue;
                }
            }
        };
        // Whole seconds, at least one: the server holds the request that long at most.
        let ask = Duration::from_secs(shared.tuning.transfer_poll.min(left).as_secs().max(1));
        let request = poll_request(ask, ask + POLL_SLACK);
        let Ok(request) = request else {
            return Step::Internal;
        };
        let asked = tokio::time::Instant::now();
        let answer = tokio::select! {
            () = shared.transfer.skip.notified() => return Step::UserSkipped,
            () = tokio::time::sleep(left) => continue,
            answer = shared.net.request(&socket, request) => answer,
        };
        match answer {
            Ok(response) if response.status == http::StatusCode::NO_CONTENT => {
                if !came_at_once(asked.elapsed(), ask) {
                    // Held for the time asked: nothing yet, ask again.
                    backoff.reset();
                    continue;
                }
                tracing::warn!(
                    after_ms = asked.elapsed().as_millis() as u64,
                    "the transfer archive request was not held by the server; asking again \
                     after a pause"
                );
            }
            Ok(response) if response.status == http::StatusCode::OK => {
                return read_answer(&response);
            }
            Ok(response) if refused_for_good(response.status) => {
                tracing::warn!(
                    status = response.status.as_u16(),
                    "the transfer archive request was refused for good"
                );
                return Step::Refused;
            }
            Ok(response) => tracing::warn!(
                status = response.status.as_u16(),
                "the transfer archive request was refused; asking again"
            ),
            Err(e) => tracing::info!(
                error = %e,
                "the transfer archive request failed; asking again"
            ),
        }
        let pause = backoff.next(false).min(left);
        tokio::select! {
            () = shared.transfer.skip.notified() => return Step::UserSkipped,
            () = tokio::time::sleep(pause) => {}
        }
    }
}

/// `GET /v1/devices/transfer_archive` that asks the server to hold the request for `hold`
/// (whole seconds) and gives up by itself after `timeout`.
fn poll_request(
    hold: Duration,
    timeout: Duration,
) -> std::result::Result<ChatRequest, TransportError> {
    let path = format!("/v1/devices/transfer_archive?timeout={}", hold.as_secs());
    Ok(ChatRequest::get(&path)?.with_timeout(timeout))
}

/// The wait's deadline has passed by this device's clock. Before the wait ends as skipped
/// the server is asked once more, for the shortest time it holds a request: a clock that
/// jumped ahead, or a suspend that lasted past the deadline, must not skip an archive the
/// phone has uploaded meanwhile. Without a socket the question waits for one (an account
/// that is not connected receives nothing either, so nothing is held back by waiting, and
/// the user can still skip); a request that fails below HTTP is asked once more
/// ([`LAST_ASK_TRIES`]), and any answer that names no archive ends the wait.
async fn last_ask<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    conn: &mut watch::Receiver<ConnState>,
) -> Step {
    let mut failures = 0;
    loop {
        let socket = shared.socket.borrow().clone();
        let Some(socket) = socket else {
            tokio::select! {
                () = shared.transfer.skip.notified() => return Step::UserSkipped,
                changed = conn.changed() => {
                    if changed.is_err() {
                        return Step::Internal;
                    }
                    continue;
                }
            }
        };
        let hold = Duration::from_secs(1);
        let Ok(request) = poll_request(hold, hold + LAST_ASK_SLACK) else {
            return Step::Internal;
        };
        let answer = tokio::select! {
            () = shared.transfer.skip.notified() => return Step::UserSkipped,
            answer = shared.net.request(&socket, request) => answer,
        };
        match answer {
            Ok(response) if response.status == http::StatusCode::OK => {
                tracing::info!("the server answered the last question of the wait");
                return read_answer(&response);
            }
            Ok(response) if refused_for_good(response.status) => return Step::Refused,
            // Nothing is ready (204), or the server cannot say now: the wait is over.
            Ok(_) => return Step::DeadlinePassed,
            Err(e) => {
                tracing::info!(error = %e, "the last transfer archive request failed");
                failures += 1;
                if failures >= LAST_ASK_TRIES {
                    return Step::DeadlinePassed;
                }
                // The socket may be one that did not survive a suspend: asked again once
                // the connection has replaced it, or after a moment.
                tokio::select! {
                    () = shared.transfer.skip.notified() => return Step::UserSkipped,
                    changed = conn.changed() => {
                        if changed.is_err() {
                            return Step::Internal;
                        }
                    }
                    () = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
            }
        }
    }
}

/// How one download ended.
enum Fetched {
    Complete,
    NotFound,
    TooLarge,
    Failed,
    Skipped,
}

/// Opens the archive file for appending, creating it and its directory for this user only;
/// returns it with its length (blocking).
fn open_archive(dir: &Path, path: &Path) -> std::io::Result<(std::fs::File, u64)> {
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    // A directory that was there already keeps its mode: set it.
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    // One writer at a time: the writer of a download that was stopped a moment ago (in this
    // process) may still be putting its last chunks down, and the length read below is the
    // offset the next request asks from. The lock goes with the file handle.
    file.lock()?;
    let len = file.metadata()?.len();
    Ok((file, len))
}

/// Blocking: appends the chunks from `rx` to `file` and syncs it when the stream ends.
fn write_chunks(
    mut file: std::fs::File,
    mut rx: tokio::sync::mpsc::Receiver<bytes::Bytes>,
    written: &AtomicU64,
) -> std::io::Result<()> {
    while let Some(chunk) = rx.blocking_recv() {
        file.write_all(&chunk)?;
        written.fetch_add(chunk.len() as u64, Ordering::AcqRel);
    }
    file.sync_all()
}

/// `downloading`: the archive into its file, from the byte the file ends at.
async fn download<T: SignalTransport>(shared: &Rc<Shared<T>>, stored: &Stored) -> Step {
    let (Some(pointer), Some(file)) = (stored.pointer.clone(), stored.file.clone()) else {
        tracing::error!("a downloading history transfer without its place");
        return Step::Internal;
    };
    let dir = shared.transfer.dir.clone();
    let path = dir.join(file);
    let opened = tokio::task::spawn_blocking(move || open_archive(&dir, &path)).await;
    let (file, offset) = match opened {
        Ok(Ok(opened)) => opened,
        Ok(Err(e)) => {
            tracing::error!(error = %e.kind(), "opening the archive file failed");
            return Step::Internal;
        }
        Err(_) => return Step::Internal,
    };
    let cap = shared.tuning.transfer_max_bytes;
    if offset > cap {
        return Step::TooLarge;
    }
    tracing::info!(have = offset, "downloading the transfer archive");
    if offset != stored.bytes_done {
        // The file decides how far the download is, not the row (written now and then).
        report(
            shared,
            HistoryTransfer::Downloading {
                done: offset,
                total: stored.bytes_total,
            },
        )
        .await;
    }
    let written = Arc::new(AtomicU64::new(offset));
    let ran = Ran::starting_from(stored.download_ran);
    let (tx, rx) = tokio::sync::mpsc::channel(DOWNLOAD_QUEUE);
    let counter = written.clone();
    let writer = tokio::task::spawn_blocking(move || write_chunks(file, rx, &counter));
    let sink = DownloadSink::starting_at(tx, offset);
    let fetched = tokio::select! {
        () = shared.transfer.skip.notified() => Fetched::Skipped,
        fetched = fetch(shared, &pointer, &sink, cap) => fetched,
        () = progress(shared, &written, &sink, stored.bytes_total, ran) => Fetched::Failed,
        () = out_of_bounds(shared, &written, ran) => Fetched::Failed,
    };
    let total = sink.total().or(stored.bytes_total);
    // The writer ends, and syncs the file, when the last sender is gone.
    drop(sink);
    let wrote = writer.await;
    if !shared.tuning.transfer_sync_delay.is_zero() {
        // A test's slow disk: the sync of a large file takes this long.
        tokio::time::sleep(shared.tuning.transfer_sync_delay).await;
    }
    let done = written.load(Ordering::Acquire);
    let now = shared.now();
    let ran = ran.so_far();
    if let Err(e) = shared
        .db
        .zone(|tx| store_progress(tx, done, total, ran, now))
    {
        tracing::warn!(error = %e, "storing the download's progress failed");
    }
    if matches!(fetched, Fetched::Complete) {
        // A download faster than the progress reports still shows its size once.
        report(shared, HistoryTransfer::Downloading { done, total }).await;
    }
    // A skip that came while the file was synced (nothing above waited for one) would be
    // lost in the read that follows, which cannot be skipped: it is honoured here. Nothing
    // is awaited between this look and the write of the next state.
    let fetched = if shared.transfer.take_pending_skip() {
        tracing::info!("the user skipped while the downloaded archive was synced");
        Fetched::Skipped
    } else {
        fetched
    };
    match (fetched, wrote) {
        (Fetched::Skipped, _) => Step::UserSkipped,
        (_, Ok(Err(e))) => {
            tracing::error!(error = %e.kind(), "writing the archive file failed");
            Step::Internal
        }
        (_, Err(_)) => Step::Internal,
        (Fetched::Complete, Ok(Ok(()))) => {
            tracing::info!(bytes = done, "the transfer archive is downloaded");
            Step::Downloaded
        }
        (Fetched::NotFound, _) => Step::NotFound,
        (Fetched::TooLarge, _) => Step::TooLarge,
        (Fetched::Failed, _) => Step::DownloadFailed,
    }
}

/// Requests the archive until it is complete, refused for good, or stalled for
/// `Tuning::transfer_stall`: a transfer that breaks off goes on with a range request
/// (`attachment::fetch_blob`), and what that gives up on is asked for again after a backoff.
async fn fetch<T: SignalTransport>(
    shared: &Shared<T>,
    pointer: &ArchivePointer,
    sink: &DownloadSink,
    cap: u64,
) -> Fetched {
    let request = || {
        let mut request = HttpsRequest::get(
            HttpsHost::Cdn(pointer.cdn),
            format!("/attachments/{}", pointer.key),
        );
        request.timeout = crate::attachment::transfer_timeout(cap);
        request.max_body = cap;
        request
    };
    let mut backoff = Backoff::new();
    let mut moved = tokio::time::Instant::now();
    loop {
        let before = sink.taken();
        let outcome = crate::attachment::fetch_blob(
            &shared.net,
            &request,
            sink,
            shared.tuning.download_resume,
        )
        .await;
        match outcome {
            Ok(status) if status.is_success() => return Fetched::Complete,
            Ok(http::StatusCode::NOT_FOUND) => return Fetched::NotFound,
            Err(TransportError::BodyTooLarge) => return Fetched::TooLarge,
            // Not the rest of the file that is there, or a CDN this build does not know.
            Err(e @ (TransportError::InvalidResponse(_) | TransportError::InvalidRequest(_))) => {
                tracing::warn!(error = %e, "the transfer archive download cannot go on");
                return Fetched::Failed;
            }
            Ok(status) if status.is_client_error() && status.as_u16() != 429 => {
                tracing::warn!(
                    status = status.as_u16(),
                    "the transfer archive download was refused"
                );
                return Fetched::Failed;
            }
            Ok(status) => tracing::warn!(
                status = status.as_u16(),
                "the transfer archive download was refused for now"
            ),
            Err(e) => tracing::info!(error = %e, "the transfer archive download broke off"),
        }
        if sink.taken() > before {
            moved = tokio::time::Instant::now();
            backoff.reset();
        }
        let stalled = moved.elapsed();
        if stalled >= shared.tuning.transfer_stall {
            tracing::warn!(
                stalled_s = stalled.as_secs(),
                "the transfer archive download made no progress for too long"
            );
            return Fetched::Failed;
        }
        let pause = backoff
            .next(false)
            .min(shared.tuning.transfer_stall - stalled);
        tokio::time::sleep(pause).await;
    }
}

/// Ends when the download as a whole is out of its bounds ([`DownloadBounds`]): it ran
/// longer than the deadline in all, or its average rate stayed under the floor for too
/// long. Its age is the time it ran ([`Ran`]): a restart does not make it young again, and
/// neither the wall clock nor the time the application was closed is part of it.
async fn out_of_bounds<T: SignalTransport>(shared: &Shared<T>, written: &AtomicU64, ran: Ran) {
    let bounds = shared.tuning.transfer_download;
    let mut slow_since = None;
    loop {
        let elapsed = ran.so_far();
        let done = written.load(Ordering::Acquire);
        if bounds.exceeded(elapsed, done, &mut slow_since) {
            tracing::warn!(
                elapsed_s = elapsed.as_secs(),
                bytes = done,
                "the transfer archive download took too long or stayed too slow"
            );
            return;
        }
        tokio::time::sleep(BOUNDS_EVERY).await;
    }
}

/// Reports the download's progress while it moves, and stores it now and then, with the
/// time the download ran: also while nothing moves, or a download that stalls and is cut
/// off would have run for no time at all. Never ends.
async fn progress<T: SignalTransport>(
    shared: &Shared<T>,
    written: &AtomicU64,
    sink: &DownloadSink,
    known_total: Option<u64>,
    ran: Ran,
) {
    let mut last = (written.load(Ordering::Acquire), known_total);
    let mut stored = tokio::time::Instant::now();
    let mut unsaved = false;
    loop {
        tokio::time::sleep(PROGRESS_EVERY).await;
        let now = (
            written.load(Ordering::Acquire),
            sink.total().or(known_total),
        );
        if now != last {
            last = now;
            unsaved = true;
            let (done, total) = now;
            report(shared, HistoryTransfer::Downloading { done, total }).await;
        }
        let since = stored.elapsed();
        if (unsaved && since >= PROGRESS_STORED_EVERY) || since >= RAN_STORED_EVERY {
            stored = tokio::time::Instant::now();
            unsaved = false;
            let (done, total) = last;
            let (ran, at) = (ran.so_far(), shared.now());
            if let Err(e) = shared
                .db
                .zone(|tx| store_progress(tx, done, total, ran, at))
            {
                tracing::warn!(error = %e, "storing the download's progress failed");
            }
        }
    }
}

/// `reading`: the archive into the stage, the commit zone, and the delivery of the stage to
/// the core (module docs, "The import"). A recorded stage is not read again; a stage that
/// was recorded and is lost is forgotten and the archive read again, which the count of
/// read starts bounds. From the first delivery row on, whatever goes wrong ends the
/// transfer as `Incomplete`: the core holds a part of the history, and it stays.
async fn read<T: SignalTransport>(shared: &Rc<Shared<T>>, stored: &Stored) -> Step {
    let (Some(key), Some(file)) = (stored.key.clone(), stored.file.clone()) else {
        tracing::error!("a reading history transfer without its key");
        return Step::Internal;
    };
    let mut recorded = stored.staged;
    loop {
        if let Some(staged) = recorded.take() {
            match deliver::deliver(shared, &key, staged).await {
                deliver::Delivered::Done { frames } => return Step::Read { frames },
                deliver::Delivered::Incomplete => {
                    return Step::ReadFailed(HistoryTransferFailure::Incomplete);
                }
                deliver::Delivered::Internal => {
                    return after_delivery_began(shared, Step::Internal);
                }
                // The record was taken back: the archive is read again.
                deliver::Delivered::StageLost => {}
            }
        }
        match read_into_stage(shared, &key, &file).await {
            Ok(staged) => recorded = Some(staged),
            Err(step) => return after_delivery_began(shared, step),
        }
    }
}

/// How a read that failed ends: as it is while the core has nothing of the import, and as
/// `Incomplete` once the core was given a part of it (the stage was lost during the
/// delivery and the archive could not be read again): nothing that was delivered is
/// withdrawn, and the user is told that the history is not whole.
fn after_delivery_began<T: SignalTransport>(shared: &Shared<T>, step: Step) -> Step {
    if !matches!(step, Step::ReadFailed(_) | Step::Internal) {
        return step;
    }
    match shared.db.read(deliver::began) {
        Ok(true) => {
            tracing::warn!("the history import cannot be finished; what was delivered stays");
            Step::ReadFailed(HistoryTransferFailure::Incomplete)
        }
        Ok(false) => step,
        Err(e) => {
            tracing::warn!(error = %e, "reading the delivery's state failed");
            step
        }
    }
}

/// The first half of the import and its commit: the file through the reader into the
/// importer on a blocking thread, then the commit zone. `Ok` is the stage the commit zone
/// recorded; `Err` is how the read ended otherwise.
async fn read_into_stage<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    key: &Zeroizing<[u8; 32]>,
    file: &str,
) -> std::result::Result<Recorded, Step> {
    let Some(account) = shared.account() else {
        return Err(Step::Internal);
    };
    let dir = shared.transfer.dir.clone();
    // No commit records a stage here: one that is there is a stage whose record was just
    // taken back (`deliver::forget`: gone, damaged, not the recorded one), or what a read left
    // that never got as far as its record. It goes before anything else of the read, also
    // before the start is counted, and long before the importer's `begin` would replace it
    // (the archive is opened first, one pass over the whole file). The start of the actor
    // has done the same for what a dead process left ([`clean_at_start`]).
    {
        let dir = dir.clone();
        let _ = tokio::task::spawn_blocking(move || remove_unrecorded_stage(&dir)).await;
    }
    kourier_db::failpoint::hit(FAILPOINT_BEFORE_IMPORT_READ);
    let now = shared.now();
    match shared.db.zone(|tx| note_read_start(tx, now)) {
        Ok(starts) if starts > MAX_READ_STARTS => {
            tracing::error!(
                starts,
                "the transfer archive's read never came to an end; giving the transfer up"
            );
            return Err(Step::ReadFailed(HistoryTransferFailure::Internal));
        }
        Ok(_) => {}
        Err(e) => {
            tracing::error!(error = %e, "storing the read's start failed");
            return Err(Step::Internal);
        }
    }
    let path = dir.join(file);
    let stop = Arc::new(AtomicBool::new(false));
    let delay = shared.tuning.transfer_frame_delay;
    let pause = shared.tuning.transfer_commit_pause;
    let aci = account.aci;
    let flag = stop.clone();
    let make = shared.tuning.transfer_importer;
    // What the age rule of attachments needs from `backend.db` (step S5), read here on the
    // account's thread: the reading thread has no database handle. A remote config that is
    // not stored yet gives the floors; a database ERROR fails the read (it must not judge
    // every attachment of the import by floors that may be wrong).
    let (queue_time, receive_limit) = match shared.db.read(crate::maintenance::import_media_config)
    {
        Ok(config) => config,
        Err(e) => {
            tracing::error!(error = %e, "reading the remote configuration failed");
            return Err(Step::Internal);
        }
    };
    let context = ImportContext {
        dir,
        key: key.clone(),
        aci,
        pni: account.pni,
        now,
        queue_time,
        receive_limit,
    };
    let key = key.clone();
    let task = tokio::task::spawn_blocking(move || {
        let key = ArchiveKey::for_transfer(&key, aci);
        let ended = import_archive(&path, &key, Limits::default(), make(context), &flag, delay);
        // A test's pause between the importer's commit and the end of the read's task.
        if ended.is_ok() && !pause.is_zero() {
            std::thread::sleep(pause);
        }
        ended
    });
    // The read is parked where a stop of this task finds it: it is ended and waited for
    // there (`run`), never left behind by a dropped future.
    *shared.transfer.reading.borrow_mut() = Some(Reading {
        stop: StopOnDrop(stop),
        task,
    });
    let ended = std::future::poll_fn(|cx| {
        let mut reading = shared.transfer.reading.borrow_mut();
        match reading.as_mut() {
            Some(running) => std::pin::Pin::new(&mut running.task).poll(cx),
            // Taken by a stop: this future is about to be dropped.
            None => std::task::Poll::Pending,
        }
    })
    .await;
    shared.transfer.reading.borrow_mut().take();
    let summary = match ended {
        Ok(Ok(summary)) => summary,
        Ok(Err(ReadEnd::Archive(e))) => {
            tracing::warn!(error = %e, "the transfer archive could not be read");
            return Err(Step::ReadFailed(failure_of(e)));
        }
        Ok(Err(ReadEnd::Import(e))) => {
            tracing::error!(error = %e, "importing the transfer archive failed");
            return Err(Step::ReadFailed(e.kind));
        }
        Ok(Err(ReadEnd::Stopped)) | Err(_) => return Err(Step::Internal),
    };
    // THE COMMIT ZONE, with nothing awaited since the read's result was taken.
    commit_read(shared, &summary)
}

#[cfg(test)]
mod tests;
