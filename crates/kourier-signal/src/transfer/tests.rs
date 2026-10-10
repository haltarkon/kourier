//! The transfer's state machine, its stored state across a reopen of `backend.db`, the
//! importer's contract and the archive file's place. The flow against a server is in
//! `tests/history_transfer.rs` (the mock server).

use std::os::unix::fs::PermissionsExt as _;
use std::sync::Mutex;

use kourier_backend_api::DbKey;
use libsignal_core::Aci;
use uuid::Uuid;

use super::*;
use crate::archive::writer::{ArchiveWriter, sample_frames, sample_info};

const NOW: i64 = 1_700_000_000_000;
const KEY: [u8; 32] = [0x42; 32];
const DB_KEY: [u8; 32] = [3; 32];

fn pointer() -> ArchivePointer {
    ArchivePointer {
        cdn: 3,
        key: "archive-object".to_owned(),
    }
}

fn steps() -> Vec<Step> {
    vec![
        Step::Pointer(pointer()),
        Step::PhoneDeclined,
        Step::RelinkRequested,
        Step::UnknownError,
        Step::Unreadable,
        Step::Refused,
        Step::DeadlinePassed,
        Step::UserSkipped,
        Step::Downloaded,
        Step::NotFound,
        Step::TooLarge,
        Step::DownloadFailed,
        Step::Read { frames: 7 },
        Step::ReadFailed(HistoryTransferFailure::Corrupt),
        Step::Internal,
    ]
}

/// The whole table, written out: every phase against every step.
#[test]
fn the_transitions_are_exactly_these() {
    use HistoryTransferFailure as Failure;
    let expected = |from: Phase, step: &Step| -> Option<Phase> {
        match (from, step) {
            (Phase::Waiting, Step::Pointer(_)) => Some(Phase::Downloading),
            (Phase::Waiting, Step::PhoneDeclined) => {
                Some(Phase::Skipped(SkipReason::PhoneDeclined))
            }
            (Phase::Waiting, Step::RelinkRequested) => Some(Phase::Failed(Failure::LinkAgain)),
            (Phase::Waiting, Step::UnknownError) => Some(Phase::Failed(Failure::UpdateApp)),
            (Phase::Waiting, Step::Unreadable) => Some(Phase::Failed(Failure::Download)),
            (Phase::Waiting, Step::Refused) => Some(Phase::Skipped(SkipReason::Refused)),
            (Phase::Waiting, Step::DeadlinePassed) => Some(Phase::Skipped(SkipReason::Deadline)),
            (Phase::Waiting | Phase::Downloading, Step::UserSkipped) => {
                Some(Phase::Skipped(SkipReason::User))
            }
            (Phase::Downloading, Step::Downloaded) => Some(Phase::Reading),
            (Phase::Downloading, Step::NotFound) => Some(Phase::Skipped(SkipReason::NotFound)),
            (Phase::Downloading, Step::TooLarge) => Some(Phase::Failed(Failure::TooLarge)),
            (Phase::Downloading, Step::DownloadFailed) => Some(Phase::Failed(Failure::Download)),
            (Phase::Reading, Step::Read { .. }) => Some(Phase::Done),
            (Phase::Reading, Step::ReadFailed(kind)) => Some(Phase::Failed(*kind)),
            (Phase::Waiting | Phase::Downloading | Phase::Reading, Step::Internal) => {
                Some(Phase::Failed(Failure::Internal))
            }
            _ => None,
        }
    };
    let phases = [
        Phase::Waiting,
        Phase::Downloading,
        Phase::Reading,
        Phase::Done,
        Phase::Skipped(SkipReason::User),
        Phase::Failed(Failure::Corrupt),
    ];
    for from in phases {
        for step in steps() {
            assert_eq!(
                advance(from, &step),
                expected(from, &step),
                "{from:?} + {step:?}"
            );
        }
    }
    // Nothing leaves an end, a read cannot be skipped, and no step goes backwards.
    for step in steps() {
        for over in [
            Phase::Done,
            Phase::Skipped(SkipReason::Deadline),
            Phase::Failed(Failure::LinkAgain),
        ] {
            assert_eq!(advance(over, &step), None);
        }
    }
    assert_eq!(advance(Phase::Reading, &Step::UserSkipped), None);
    assert_eq!(advance(Phase::Reading, &Step::Pointer(pointer())), None);
    assert_eq!(advance(Phase::Downloading, &Step::Pointer(pointer())), None);
    assert_eq!(advance(Phase::Waiting, &Step::Downloaded), None);
}

#[test]
fn every_phase_reads_back_from_its_stored_names() {
    let mut phases = vec![
        Phase::Waiting,
        Phase::Downloading,
        Phase::Reading,
        Phase::Done,
    ];
    phases.extend(SkipReason::ALL.map(Phase::Skipped));
    phases.extend(FAILURES.map(Phase::Failed));
    for phase in phases {
        assert_eq!(
            Phase::parse(phase.state_str(), phase.reason_str()),
            Some(phase)
        );
    }
    // An end a newer version wrote with a reason this one does not know is still an end.
    assert!(Phase::parse("failed", Some("something new")).is_some_and(Phase::is_over));
    assert!(Phase::parse("skipped", None).is_some_and(Phase::is_over));
    assert_eq!(Phase::parse("importing", None), None);
}

fn open(dir: &Path) -> kourier_db::ZoneDb {
    crate::db::open(&dir.join("backend.db"), &DbKey::new(DB_KEY)).unwrap()
}

fn stored(db: &kourier_db::ZoneDb) -> Stored {
    db.read(load).unwrap().expect("a stored transfer")
}

fn step(db: &kourier_db::ZoneDb, step: &Step) -> Option<Phase> {
    db.zone(|tx| apply(tx, step, NOW + 5)).unwrap()
}

/// The row is what a restart goes on from: every state is read back from a database that
/// was closed and opened again, with the key until the end and without it from the end on.
#[test]
fn the_stored_transfer_survives_a_reopen_at_every_state() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = open(dir.path());
        assert!(
            db.read(load).unwrap().is_none(),
            "no transfer before a link"
        );
        db.zone(|tx| begin(tx, &KEY, NOW, Duration::from_secs(3600)))
            .unwrap();
    }
    {
        let db = open(dir.path());
        let waiting = stored(&db);
        assert_eq!(waiting.phase, Phase::Waiting);
        assert_eq!(waiting.key.as_deref(), Some(&KEY));
        assert_eq!(waiting.deadline_at, NOW + 3_600_000);
        assert_eq!(waiting.pointer, None);
        assert_eq!(waiting.report(), HistoryTransfer::Waiting);
        assert_eq!(
            step(&db, &Step::Pointer(pointer())),
            Some(Phase::Downloading)
        );
        db.zone(|tx| store_progress(tx, 4096, Some(10_000), Duration::from_secs(1), NOW + 6))
            .unwrap();
    }
    {
        let db = open(dir.path());
        let downloading = stored(&db);
        assert_eq!(downloading.phase, Phase::Downloading);
        assert_eq!(downloading.pointer, Some(pointer()));
        assert_eq!(downloading.file.as_deref(), Some(FILE));
        assert_eq!(downloading.key.as_deref(), Some(&KEY));
        assert_eq!(
            downloading.report(),
            HistoryTransfer::Downloading {
                done: 4096,
                total: Some(10_000)
            }
        );
        assert_eq!(step(&db, &Step::Downloaded), Some(Phase::Reading));
    }
    {
        let db = open(dir.path());
        let reading = stored(&db);
        assert_eq!(reading.phase, Phase::Reading);
        // A read that starts over after a restart needs the key and the file again.
        assert_eq!(reading.key.as_deref(), Some(&KEY));
        assert_eq!(reading.file.as_deref(), Some(FILE));
        assert_eq!(step(&db, &Step::Read { frames: 7 }), Some(Phase::Done));
    }
    let db = open(dir.path());
    let done = stored(&db);
    assert_eq!(done.phase, Phase::Done);
    assert_eq!(done.frames, Some(7));
    assert_eq!(done.report(), HistoryTransfer::Done);
    assert!(done.key.is_none() && done.pointer.is_none() && done.file.is_none());
    // Not only unread: the columns are empty.
    let left: i64 = db
        .read(|c| -> Result<i64> {
            Ok(c.query_row(
                "SELECT count(*) FROM history_transfer
                 WHERE key IS NOT NULL OR cdn IS NOT NULL OR cdn_key IS NOT NULL
                    OR file IS NOT NULL",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(left, 0);
}

/// Every end wipes the key and the archive's place in the write that ends the transfer.
#[test]
fn every_end_wipes_the_key_and_the_place() {
    let ends: [(&[Step], Phase); 7] = [
        (
            &[Step::PhoneDeclined],
            Phase::Skipped(SkipReason::PhoneDeclined),
        ),
        (
            &[Step::RelinkRequested],
            Phase::Failed(HistoryTransferFailure::LinkAgain),
        ),
        (
            &[Step::DeadlinePassed],
            Phase::Skipped(SkipReason::Deadline),
        ),
        (&[Step::UserSkipped], Phase::Skipped(SkipReason::User)),
        (
            &[Step::Pointer(pointer()), Step::UserSkipped],
            Phase::Skipped(SkipReason::User),
        ),
        (
            &[Step::Pointer(pointer()), Step::NotFound],
            Phase::Skipped(SkipReason::NotFound),
        ),
        (
            &[
                Step::Pointer(pointer()),
                Step::Downloaded,
                Step::ReadFailed(HistoryTransferFailure::UpdateApp),
            ],
            Phase::Failed(HistoryTransferFailure::UpdateApp),
        ),
    ];
    for (path, end) in ends {
        let dir = tempfile::tempdir().unwrap();
        let db = open(dir.path());
        db.zone(|tx| begin(tx, &KEY, NOW, WAIT)).unwrap();
        for s in path {
            assert!(step(&db, s).is_some(), "{s:?}");
        }
        let over = stored(&db);
        assert_eq!(over.phase, end);
        assert!(
            over.key.is_none() && over.pointer.is_none() && over.file.is_none(),
            "{end:?} keeps nothing"
        );
        assert!(over.report().is_over());
    }
}

/// A step that does not apply changes nothing: a skip that comes while the archive is read,
/// or anything after the end, leaves the row as it was.
#[test]
fn a_step_that_does_not_apply_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    assert_eq!(step(&db, &Step::UserSkipped), None, "no transfer at all");
    db.zone(|tx| begin(tx, &KEY, NOW, WAIT)).unwrap();
    step(&db, &Step::Pointer(pointer()));
    step(&db, &Step::Downloaded);
    assert_eq!(step(&db, &Step::UserSkipped), None, "a read is not skipped");
    let reading = stored(&db);
    assert_eq!(reading.phase, Phase::Reading);
    assert_eq!(reading.key.as_deref(), Some(&KEY));
    step(&db, &Step::Read { frames: 1 });
    assert_eq!(step(&db, &Step::Internal), None, "nothing follows the end");
    assert_eq!(stored(&db).phase, Phase::Done);
    // A second transfer for one account does not exist.
    assert!(db.zone(|tx| begin(tx, &KEY, NOW, WAIT)).is_err());
}

/// A read that is started again and again because it ends the process every time is
/// counted in the row, so a start can give the transfer up instead of holding receiving
/// back. (Each `note_read_start` without a stop between is a read that ended with its
/// process: nothing cleared its mark.)
#[test]
fn the_starts_of_a_read_are_counted_across_reopens() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = open(dir.path());
        db.zone(|tx| begin(tx, &KEY, NOW, WAIT)).unwrap();
        step(&db, &Step::Pointer(pointer()));
        step(&db, &Step::Downloaded);
        assert_eq!(db.zone(|tx| note_read_start(tx, NOW)).unwrap(), 1);
    }
    let db = open(dir.path());
    for expected in 2..=MAX_READ_STARTS + 1 {
        assert_eq!(db.zone(|tx| note_read_start(tx, NOW)).unwrap(), expected);
    }
    assert_eq!(
        stored(&db).phase,
        Phase::Reading,
        "counting changes no state"
    );
}

/// Finding 9. A read that was stopped in an orderly way (the user quit during a long read)
/// uses none of the starts, however often it happens; only a start after a read that ended
/// with its process counts.
#[test]
fn an_orderly_stop_of_a_read_does_not_count_as_a_start() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    db.zone(|tx| begin(tx, &KEY, NOW, WAIT)).unwrap();
    step(&db, &Step::Pointer(pointer()));
    step(&db, &Step::Downloaded);
    let starts = |db: &kourier_db::ZoneDb| db.zone(|tx| note_read_start(tx, NOW)).unwrap();
    let stop = |db: &kourier_db::ZoneDb| db.zone(|tx| note_read_stopped(tx, NOW)).unwrap();
    // Far more orderly quits than the limit: the count stays at the first start.
    for _ in 0..3 * MAX_READ_STARTS {
        assert_eq!(starts(&db), 1);
        stop(&db);
    }
    // A read that ends with its process (no stop) makes the next start count.
    assert_eq!(starts(&db), 1);
    assert_eq!(starts(&db), 2, "the read before this one was cut off");
    stop(&db);
    assert_eq!(starts(&db), 2, "the one before this was stopped");
    assert_eq!(starts(&db), 3);
    assert_eq!(stored(&db).phase, Phase::Reading);
    // An end clears the mark with everything else.
    step(&db, &Step::Read { frames: 1 });
    let open: i64 = db
        .read(|c| -> Result<i64> {
            Ok(c.query_row("SELECT read_open FROM history_transfer", [], |r| r.get(0))?)
        })
        .unwrap();
    assert_eq!(open, 0);
}

/// Finding 2. The row of a transfer that is over stays, with its state and reason, and is
/// what every start reports; a dismissal removes it, and only when the transfer is over.
#[test]
fn a_dismissal_removes_only_a_transfer_that_is_over() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    assert!(!db.zone(|tx| dismiss(tx)).unwrap(), "no transfer at all");
    db.zone(|tx| begin(tx, &KEY, NOW, WAIT)).unwrap();
    assert!(!db.zone(|tx| dismiss(tx)).unwrap(), "waiting is not over");
    step(&db, &Step::Pointer(pointer()));
    assert!(
        !db.zone(|tx| dismiss(tx)).unwrap(),
        "downloading is not over"
    );
    step(&db, &Step::DownloadFailed);
    // The end stays readable for every later start, with its kind.
    drop(db);
    let db = open(dir.path());
    assert_eq!(
        stored(&db).report(),
        HistoryTransfer::Failed(HistoryTransferFailure::Download)
    );
    assert!(db.zone(|tx| dismiss(tx)).unwrap());
    assert!(
        db.read(load).unwrap().is_none(),
        "nothing is left to report"
    );
    assert!(
        !db.zone(|tx| dismiss(tx)).unwrap(),
        "a second dismissal finds nothing"
    );
}

/// An account that is logged out or removed during a transfer: the row goes, whatever its
/// state, and the caller is told when it held a secret (it then truncates the WAL).
#[test]
fn a_logout_removes_the_row_and_says_whether_it_held_a_secret() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    assert!(!db.zone(|tx| delete_row(tx)).unwrap(), "no transfer at all");
    db.zone(|tx| begin(tx, &KEY, NOW, WAIT)).unwrap();
    assert!(db.zone(|tx| delete_row(tx)).unwrap(), "the key was there");
    assert!(db.read(load).unwrap().is_none());
    db.zone(|tx| begin(tx, &KEY, NOW, WAIT)).unwrap();
    step(&db, &Step::PhoneDeclined);
    assert!(
        !db.zone(|tx| delete_row(tx)).unwrap(),
        "an end holds no secret"
    );
    assert!(
        db.read(load).unwrap().is_none(),
        "the row goes all the same"
    );
}

/// Finding 1, at the level of the files: the write that ends a transfer leaves the earlier
/// versions of the row's page in the WAL, and the checkpoint that follows it leaves none
/// (the WAL file is empty; what the main file holds is proven page by page in
/// `tests/history_transfer_attack.rs`).
#[test]
fn the_checkpoint_after_an_end_empties_the_wal() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let wal = dir.path().join("backend.db-wal");
    db.zone(|tx| begin(tx, &KEY, NOW, WAIT)).unwrap();
    step(&db, &Step::Pointer(pointer()));
    step(&db, &Step::UserSkipped);
    assert!(
        std::fs::metadata(&wal).unwrap().len() > 0,
        "the versions of the row with its key are in the WAL"
    );
    assert!(checkpoint(&db));
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
    // The same after a logout in the middle of a transfer.
    db.zone(|tx| dismiss(tx)).unwrap();
    db.zone(|tx| begin(tx, &KEY, NOW, WAIT)).unwrap();
    assert!(db.zone(|tx| delete_row(tx)).unwrap());
    assert!(std::fs::metadata(&wal).unwrap().len() > 0);
    assert!(checkpoint(&db));
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
}

/// Finding 6. What is left of the wait follows the wall clock, but never exceeds the whole
/// wait: a deadline further away is a clock that moved back, and the wait counts anew.
#[test]
fn the_time_left_of_a_wait_is_clamped_to_the_whole_wait() {
    let wait = Duration::from_secs(3600);
    let hour = 3_600_000;
    // The ordinary case: half the wait is left, nothing to correct.
    assert_eq!(
        time_left(NOW + hour / 2, NOW, wait),
        (Duration::from_secs(1800), None)
    );
    assert_eq!(time_left(NOW + hour, NOW, wait), (wait, None));
    // Passed, by a moment or by a day (a clock that jumped ahead, a long suspend): nothing
    // is left; the caller asks the server once more before it ends the wait.
    assert_eq!(time_left(NOW, NOW, wait), (Duration::ZERO, None));
    assert_eq!(
        time_left(NOW - 24 * hour, NOW, wait),
        (Duration::ZERO, None)
    );
    // The clock moved back a day after the deadline was written: the whole wait is left,
    // from now, and that is the deadline to store.
    assert_eq!(
        time_left(NOW + 25 * hour, NOW, wait),
        (wait, Some(NOW + hour))
    );
    assert_eq!(
        time_left(NOW + hour + 1, NOW, wait),
        (wait, Some(NOW + hour))
    );
    // The corrected deadline is stored for a waiting transfer only.
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    db.zone(|tx| begin(tx, &KEY, NOW + 24 * hour, wait))
        .unwrap();
    assert_eq!(stored(&db).deadline_at, NOW + 25 * hour);
    db.zone(|tx| store_deadline(tx, NOW + hour, NOW)).unwrap();
    assert_eq!(stored(&db).deadline_at, NOW + hour);
    step(&db, &Step::Pointer(pointer()));
    db.zone(|tx| store_deadline(tx, 5, NOW)).unwrap();
    assert_eq!(stored(&db).deadline_at, NOW + hour, "not while downloading");
}

/// Finding 4. A 204 is "not held" only when it came back much sooner than what was asked.
#[test]
fn a_204_that_was_not_held_is_told_from_one_that_was() {
    let ms = Duration::from_millis;
    let five_minutes = Duration::from_secs(300);
    assert!(came_at_once(ms(0), five_minutes));
    assert!(came_at_once(ms(999), five_minutes));
    assert!(!came_at_once(ms(1000), five_minutes));
    assert!(!came_at_once(five_minutes, five_minutes));
    // The shortest hold a server accepts is a second: held for it is held.
    assert!(came_at_once(ms(499), Duration::from_secs(1)));
    assert!(!came_at_once(ms(500), Duration::from_secs(1)));
    assert!(!came_at_once(ms(1000), Duration::from_secs(1)));
}

/// Finding 5 (b). Which answers of the long-poll are the server's refusal for good.
#[test]
fn a_4xx_other_than_408_and_429_refuses_the_long_poll_for_good() {
    let status = |code: u16| http::StatusCode::from_u16(code).unwrap();
    for code in [400, 401, 403, 404, 405, 410, 422, 428] {
        assert!(refused_for_good(status(code)), "{code}");
    }
    for code in [200, 204, 408, 429, 500, 502, 503, 504] {
        assert!(!refused_for_good(status(code)), "{code}");
    }
}

/// Finding 5 (a). The download as a whole: its deadline, and its average rate since it
/// began, judged after the first minute and failed after ten minutes under the floor.
#[test]
fn a_download_is_bounded_by_its_deadline_and_its_average_rate() {
    let bounds = DownloadBounds::DEFAULT;
    assert_eq!(bounds.deadline, Duration::from_secs(2 * 60 * 60));
    assert_eq!(bounds.rate_grace, Duration::from_secs(60));
    assert_eq!(bounds.slow_for, Duration::from_secs(600));
    let s = Duration::from_secs;
    let floor = |secs: u64| MIN_DOWNLOAD_RATE * secs;

    // The deadline, whatever the rate.
    let mut slow = None;
    assert!(!bounds.exceeded(s(7199), u64::MAX, &mut slow));
    assert!(bounds.exceeded(s(7200), u64::MAX, &mut slow));

    // Nothing is judged in the first minute, not even a download without a byte.
    let mut slow = None;
    assert!(!bounds.exceeded(s(59), 0, &mut slow));
    assert_eq!(slow, None);
    // A body that trickles: under the floor from the first judged moment on, it fails ten
    // minutes later, not before.
    assert!(!bounds.exceeded(s(60), 100, &mut slow));
    assert_eq!(slow, Some(s(60)));
    assert!(!bounds.exceeded(s(659), 1000, &mut slow));
    assert!(bounds.exceeded(s(660), 1000, &mut slow));

    // A download that recovers its average in between starts the ten minutes anew.
    let mut slow = None;
    assert!(!bounds.exceeded(s(100), 0, &mut slow));
    assert!(!bounds.exceeded(s(400), floor(400), &mut slow));
    assert_eq!(slow, None, "at the floor is not under it");
    assert!(!bounds.exceeded(s(500), floor(500) - 1, &mut slow));
    assert!(!bounds.exceeded(s(1099), floor(500), &mut slow));
    assert!(bounds.exceeded(s(1100), floor(500), &mut slow));

    // A fast download is never judged slow.
    let mut slow = None;
    for at in [60, 600, 3600, 7199] {
        assert!(!bounds.exceeded(s(at), floor(at) * 100, &mut slow));
    }
}

/// The time the download ran is stored with its progress, so a restart does not make the
/// download young again; and it is the time it RAN: however late the next start comes, the
/// time the application was closed is not in it.
#[test]
fn the_time_a_download_ran_is_stored_and_a_restart_goes_on_from_it() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = open(dir.path());
        db.zone(|tx| begin(tx, &KEY, NOW, WAIT)).unwrap();
        assert_eq!(stored(&db).download_ran, Duration::ZERO);
        step(&db, &Step::Pointer(pointer()));
        assert_eq!(stored(&db).download_ran, Duration::ZERO);
        db.zone(|tx| store_progress(tx, 4096, None, Duration::from_secs(90), NOW + 90_000))
            .unwrap();
    }
    let db = open(dir.path());
    let before = stored(&db).download_ran;
    assert_eq!(before, Duration::from_secs(90));
    // A day later (the wall clock is not asked): the run that starts counts from 90 s.
    let ran = Ran::starting_from(before);
    let so_far = ran.so_far();
    assert!(
        so_far >= before && so_far < before + Duration::from_secs(5),
        "{so_far:?}"
    );
    db.zone(|tx| store_progress(tx, 8192, None, so_far, NOW + 86_400_000))
        .unwrap();
    assert!(stored(&db).download_ran < Duration::from_secs(95));
}

/// Finding 8. A skip that comes while nobody waits for one (the downloaded file is being
/// synced) is kept, found by the look the download takes before it becomes a read, and
/// taken only once.
#[test]
fn a_skip_that_nobody_waited_for_is_found_once() {
    let shared = TransferShared::new(std::env::temp_dir().join("kourier-unused"), true);
    assert!(!shared.take_pending_skip(), "no skip came");
    shared.skip();
    assert!(shared.take_pending_skip(), "the skip that came meanwhile");
    assert!(!shared.take_pending_skip(), "taken once");
    // Two skips in a row are one.
    shared.skip();
    shared.skip();
    assert!(shared.take_pending_skip());
    assert!(!shared.take_pending_skip());
}

/// What an importer was asked to do, in order.
#[derive(Clone, Default)]
struct Calls(Arc<Mutex<Vec<String>>>);

impl Calls {
    fn push(&self, call: impl Into<String>) {
        self.0.lock().unwrap().push(call.into());
    }

    fn take(&self) -> Vec<String> {
        std::mem::take(&mut self.0.lock().unwrap())
    }
}

struct Recording {
    calls: Calls,
    /// Refuse the frame with this index.
    refuse_at: Option<usize>,
    seen: usize,
}

impl Recording {
    fn boxed(calls: &Calls, refuse_at: Option<usize>) -> Box<dyn Importer> {
        Box::new(Self {
            calls: calls.clone(),
            refuse_at,
            seen: 0,
        })
    }
}

impl Importer for Recording {
    fn begin(&mut self, info: &ArchiveInfo) -> std::result::Result<(), ImportError> {
        self.calls.push(format!("begin v{}", info.version));
        Ok(())
    }

    fn frame(&mut self, frame: Frame) -> std::result::Result<(), ImportError> {
        if self.refuse_at == Some(self.seen) {
            self.calls.push("refuse");
            return Err(ImportError::internal("refused by the test"));
        }
        self.seen += 1;
        self.calls.push(frame.kind());
        Ok(())
    }

    fn commit(self: Box<Self>) -> std::result::Result<ImportSummary, ImportError> {
        self.calls.push("commit");
        Ok(ImportSummary {
            frames: self.seen as u64,
            ..ImportSummary::default()
        })
    }

    fn abort(self: Box<Self>) {
        self.calls.push("abort");
    }
}

fn aci() -> Aci {
    Aci::from(Uuid::from_bytes([7; 16]))
}

fn archive_key() -> ArchiveKey {
    ArchiveKey::for_transfer(&KEY, aci())
}

/// The sample archive at `path`; `broken_after` puts a frame that does not decode there.
fn write_archive(path: &Path, version: u64, broken_after: Option<usize>) {
    let mut info = sample_info();
    info.version = version;
    let mut writer = ArchiveWriter::new(&info);
    for (i, frame) in sample_frames().iter().enumerate() {
        if broken_after == Some(i) {
            writer.raw_frame(&[0x0a, 0x7f, 0x01]);
        }
        writer.frame(frame);
    }
    writer.write(path, &archive_key()).unwrap();
}

fn import(
    path: &Path,
    importer: Box<dyn Importer>,
    stop: bool,
) -> std::result::Result<ImportSummary, ReadEnd> {
    import_archive(
        path,
        &archive_key(),
        Limits::default(),
        importer,
        &AtomicBool::new(stop),
        Duration::ZERO,
    )
}

/// The reader's rule in the importer's interface: `commit` only after the reader returned
/// `Ok(None)`, `abort` in every other case, never both, never neither.
#[test]
fn an_importer_is_committed_only_after_the_last_frame_verified() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archive");
    let calls = Calls::default();
    let frames = sample_frames().len();

    // A good archive: begin, every frame in order, commit.
    write_archive(&path, 1, None);
    let summary = import(&path, Recording::boxed(&calls, None), false).unwrap();
    assert_eq!(summary.frames, frames as u64);
    let seen = calls.take();
    assert_eq!(seen.first().map(String::as_str), Some("begin v1"));
    assert_eq!(seen.last().map(String::as_str), Some("commit"));
    assert_eq!(seen.len(), frames + 2);
    assert_eq!(seen[1], "account");
    assert!(!seen.contains(&"abort".to_owned()));

    // An authentic archive that fails half-way: the frames before the break were handed
    // over, and then everything is taken back.
    write_archive(&path, 1, Some(3));
    let end = import(&path, Recording::boxed(&calls, None), false).unwrap_err();
    assert_eq!(end, ReadEnd::Archive(ArchiveError::BadProtobuf));
    let seen = calls.take();
    assert_eq!(
        seen,
        ["begin v1", "account", "recipient", "recipient", "abort"]
    );

    // One flipped byte: nothing is handed over at all (the HMAC pass comes first).
    write_archive(&path, 1, None);
    let mut bytes = std::fs::read(&path).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x01;
    std::fs::write(&path, &bytes).unwrap();
    let end = import(&path, Recording::boxed(&calls, None), false).unwrap_err();
    assert_eq!(end, ReadEnd::Archive(ArchiveError::Hmac));
    assert_eq!(calls.take(), ["abort"]);

    // Another version: aborted without a begin.
    write_archive(&path, 2, None);
    let end = import(&path, Recording::boxed(&calls, None), false).unwrap_err();
    assert_eq!(
        end,
        ReadEnd::Archive(ArchiveError::UnsupportedVersion { found: 2 })
    );
    assert_eq!(calls.take(), ["abort"]);

    // The importer refuses a frame: aborted, and the reader is not asked further.
    write_archive(&path, 1, None);
    let end = import(&path, Recording::boxed(&calls, Some(2)), false).unwrap_err();
    assert_eq!(
        end,
        ReadEnd::Import(ImportError::internal("refused by the test"))
    );
    assert_eq!(
        calls.take(),
        ["begin v1", "account", "recipient", "refuse", "abort"]
    );

    // Stopped (the actor goes away) while the archive was opened: aborted before the
    // importer began, so it makes nothing.
    let end = import(&path, Recording::boxed(&calls, None), true).unwrap_err();
    assert_eq!(end, ReadEnd::Stopped);
    assert_eq!(calls.take(), ["abort"]);

    // A file that is not there.
    let end = import(
        &dir.path().join("none"),
        Recording::boxed(&calls, None),
        false,
    )
    .unwrap_err();
    assert!(
        matches!(end, ReadEnd::Archive(ArchiveError::Io(_))),
        "{end:?}"
    );
    assert_eq!(calls.take(), ["abort"]);
}

/// Each reason an archive does not read is one thing for the user to do.
#[test]
fn an_unreadable_archive_maps_to_what_the_user_can_do() {
    use HistoryTransferFailure as Failure;
    assert_eq!(
        failure_of(ArchiveError::UnsupportedVersion { found: 2 }),
        Failure::UpdateApp
    );
    for corrupt in [
        ArchiveError::Hmac,
        ArchiveError::Truncated,
        ArchiveError::BadHeader,
        ArchiveError::BadPadding,
        ArchiveError::BadGzip,
        ArchiveError::BadFraming,
        ArchiveError::BadProtobuf,
        ArchiveError::MissingInfo,
    ] {
        assert_eq!(failure_of(corrupt), Failure::Corrupt, "{corrupt:?}");
    }
    for large in [
        ArchiveError::TooLarge { max: 1 },
        ArchiveError::TooManyFrames { max: 1 },
        ArchiveError::FrameTooLong { len: 2, max: 1 },
        ArchiveError::FrameTooComplex { max: 1 },
    ] {
        assert_eq!(failure_of(large), Failure::TooLarge, "{large:?}");
    }
    assert_eq!(
        failure_of(ArchiveError::Io(std::io::ErrorKind::PermissionDenied)),
        Failure::Internal
    );
}

fn answer(body: &str) -> Step {
    read_answer(&ChatResponse {
        status: http::StatusCode::OK,
        message: None,
        headers: http::HeaderMap::new(),
        body: bytes::Bytes::copy_from_slice(body.as_bytes()),
    })
}

/// Desktop's `TransferArchiveSchema`: a place, or one of two error values. Finding 3: an
/// error value this version does not know is a newer protocol ("update Kourier"), not a
/// download that failed.
#[test]
fn the_servers_answer_is_a_place_or_one_of_two_errors() {
    assert_eq!(
        answer(r#"{"cdn":3,"key":"abc-DEF_123"}"#),
        Step::Pointer(ArchivePointer {
            cdn: 3,
            key: "abc-DEF_123".to_owned()
        })
    );
    assert_eq!(
        answer(r#"{"error":"CONTINUE_WITHOUT_UPLOAD"}"#),
        Step::PhoneDeclined
    );
    assert_eq!(
        answer(r#"{"error":"RELINK_REQUESTED"}"#),
        Step::RelinkRequested
    );
    // An error value this version does not know, also beside a place: a newer protocol.
    for unknown in [
        r#"{"error":"SOMETHING_NEW"}"#,
        r#"{"error":""}"#,
        r#"{"error":"SOMETHING_NEW","cdn":3,"key":"abc"}"#,
    ] {
        assert_eq!(answer(unknown), Step::UnknownError, "{unknown}");
        assert_eq!(
            advance(Phase::Waiting, &answer(unknown)),
            Some(Phase::Failed(HistoryTransferFailure::UpdateApp))
        );
    }
    // A key that could leave the path, half a place, or no JSON: not understood, never
    // guessed at.
    for unreadable in [
        r#"{"cdn":3,"key":"../../v1/keys"}"#,
        r#"{"cdn":3,"key":""}"#,
        r#"{"cdn":3}"#,
        r#"{"key":"abc"}"#,
        "{}",
        "not json",
    ] {
        assert_eq!(answer(unreadable), Step::Unreadable, "{unreadable}");
    }
    // The place never shows in a debug print.
    let shown = format!("{:?}", answer(r#"{"cdn":3,"key":"secret-object-key"}"#));
    assert!(!shown.contains("secret-object-key"), "{shown}");
}

/// The archive file lives in a directory only the user can read and write, and a download
/// that goes on appends to what is there.
#[test]
fn the_archive_file_is_private_and_appended_to() {
    let dir = tempfile::tempdir().unwrap();
    let transfer = dir_beside(&dir.path().join("backend.db"));
    assert_eq!(transfer, dir.path().join(DIR));
    // A directory that was left open to others is closed again.
    std::fs::create_dir(&transfer).unwrap();
    std::fs::set_permissions(&transfer, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = transfer.join(FILE);
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;

    let (file, len) = open_archive(&transfer, &path).unwrap();
    assert_eq!(len, 0);
    assert_eq!(mode(&transfer), 0o700);
    assert_eq!(mode(&path), 0o600);
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let written = AtomicU64::new(0);
    tx.blocking_send(bytes::Bytes::from_static(b"first"))
        .unwrap();
    drop(tx);
    write_chunks(file, rx, &written).unwrap();
    assert_eq!(written.load(Ordering::Acquire), 5);

    // The next run goes on behind the bytes that are there.
    let (file, len) = open_archive(&transfer, &path).unwrap();
    assert_eq!(len, 5);
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tx.blocking_send(bytes::Bytes::from_static(b" second"))
        .unwrap();
    drop(tx);
    write_chunks(file, rx, &written).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"first second");
}

/// A start keeps the file of a transfer that downloads or reads and removes any other.
#[test]
fn a_start_keeps_only_the_file_of_a_transfer_that_needs_it() {
    let at = |phase: Phase| Stored {
        phase,
        key: None,
        pointer: None,
        file: None,
        bytes_done: 0,
        bytes_total: None,
        deadline_at: 0,
        download_ran: Duration::ZERO,
        frames: None,
        staged: None,
    };
    let cases = [
        (None, false),
        (Some(at(Phase::Waiting)), false),
        (Some(at(Phase::Downloading)), true),
        (Some(at(Phase::Reading)), true),
        (Some(at(Phase::Done)), false),
        (Some(at(Phase::Skipped(SkipReason::User))), false),
        (
            Some(at(Phase::Failed(HistoryTransferFailure::Corrupt))),
            false,
        ),
    ];
    for (stored, kept) in cases {
        let dir = tempfile::tempdir().unwrap();
        let transfer = dir.path().join(DIR);
        std::fs::create_dir(&transfer).unwrap();
        std::fs::write(transfer.join(FILE), b"leftover").unwrap();
        clean_at_start(&transfer, stored.as_ref());
        assert_eq!(
            transfer.join(FILE).exists(),
            kept,
            "{:?}",
            stored.map(|s| s.phase)
        );
    }
    // No directory at all is fine.
    clean_at_start(Path::new("/nonexistent/kourier-transfer"), None);
}

/// The hold on the Storage Service sync: while a transfer is not over, the jobs that wait
/// for it are not due, and every other job is.
#[test]
fn the_storage_sync_is_not_due_while_a_transfer_holds_it() {
    use crate::jobs::{self, JobKind};
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    db.zone(|tx| -> Result<()> {
        jobs::enqueue(tx, JobKind::StorageSync, "", 0, None)?;
        jobs::schedule_at(tx, JobKind::ContactSyncWait, "", 0)?;
        Ok(())
    })
    .unwrap();
    let due = |held: bool| {
        db.read(|c| jobs::next_due(c, 10, true, held))
            .unwrap()
            .map(|job| job.kind)
    };
    assert_eq!(due(true), None);
    assert_eq!(db.read(|c| jobs::next_at(c, true, true)).unwrap(), None);
    assert!(due(false).is_some());
    db.zone(|tx| jobs::enqueue(tx, JobKind::SyncRequest, "contacts", 0, None))
        .unwrap();
    assert_eq!(
        due(true),
        Some(JobKind::SyncRequest),
        "sync requests go out"
    );
    assert!(JobKind::StorageSync.waits_for_transfer());
    assert!(JobKind::ContactSyncWait.waits_for_transfer());
    assert!(!JobKind::SyncRequest.waits_for_transfer());
    assert!(!JobKind::RemoteConfig.waits_for_transfer());
}

// ---- The import: the importer, the stage it leaves, the commit zone -------------------

/// What the read of an archive hands its importer, for a transfer directory `dir`.
fn import_context(dir: &Path) -> ImportContext {
    ImportContext {
        dir: dir.to_owned(),
        key: Zeroizing::new(KEY),
        aci: aci(),
        pni: libsignal_core::Pni::from(Uuid::from_bytes([8; 16])),
        now: NOW,
    }
}

fn stage_files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with(stage::FILE))
        .collect();
    names.sort();
    names
}

fn open_stage(dir: &Path) -> std::result::Result<stage::StageReader, stage::StageError> {
    stage::StageReader::open(dir, &stage::StageKey::derive(&KEY))
}

/// The importer the app uses, on the sample archive: every frame counted by kind, the
/// chat's three texts in the stage, the stage whole and closed.
#[test]
fn the_history_importer_stages_the_sample_and_counts_by_kind() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archive");
    write_archive(&path, 1, None);
    // Made the way `read()` makes it: by the factory of the default tuning.
    let make = crate::Tuning::default().transfer_importer;
    let summary = import(&path, make(import_context(dir.path())), false).unwrap();
    assert_eq!(summary.frames, sample_frames().len() as u64);
    assert_eq!(summary.by_kind.get("account"), Some(&1));
    assert_eq!(summary.by_kind.get("recipient"), Some(&2));
    assert_eq!(summary.by_kind.get("chat"), Some(&1));
    assert_eq!(summary.by_kind.get("chat_item"), Some(&3));
    assert_eq!(summary.by_kind.values().sum::<u64>(), summary.frames);
    let totals = summary.staged.expect("the importer staged");
    assert_eq!(
        (
            totals.messages,
            totals.chats,
            totals.shown_chats,
            totals.contacts
        ),
        (3, 1, 1, 1)
    );
    assert_eq!(
        summary.skipped,
        BTreeMap::from([(map::reason::REACTIONS_NOT_YET, 1)])
    );

    // One file, closed, this user's only; nothing of a text in its bytes.
    assert_eq!(stage_files(dir.path()), [stage::FILE]);
    let stage_path = stage::path_in(dir.path());
    assert_eq!(
        std::fs::metadata(&stage_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let bytes = std::fs::read(&stage_path).unwrap();
    assert!(!bytes.windows(5).any(|w| w == b"first"));
    let reader = open_stage(dir.path()).unwrap();
    assert_eq!(reader.totals().unwrap(), totals);
    assert_eq!(reader.count().unwrap(), totals);
    assert_eq!(reader.frames().unwrap(), summary.frames);
    let chats = reader.chats().unwrap();
    assert_eq!(chats.len(), 1);
    assert_eq!((chats[0].counters.n_rows, chats[0].counters.n_loud), (3, 3));
    let rows = reader.items(0, None, 500, u64::MAX).unwrap();
    assert_eq!(
        rows.iter()
            .filter(|row| !row.msg.read && !row.msg.origin.is_own())
            .count(),
        1,
        "the unread message says so itself"
    );
    let locals: Vec<&str> = rows.iter().map(|row| &*row.msg.local).collect();
    assert_eq!(locals, ["3000", "3100", "3200"]);
    // The rows carry the number of the frame they came from.
    assert_eq!(rows.iter().map(|row| row.n).collect::<Vec<_>>(), [5, 6, 7]);
}

/// Before the commit an import is one file, and every end but the commit removes it.
#[test]
fn an_import_that_fails_or_is_stopped_leaves_no_stage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archive");
    let importer = || history_importer(import_context(dir.path()));

    // An authentic archive that breaks after two of its three messages were staged.
    write_archive(&path, 1, Some(6));
    let end = import(&path, importer(), false).unwrap_err();
    assert_eq!(end, ReadEnd::Archive(ArchiveError::BadProtobuf));
    assert_eq!(stage_files(dir.path()), Vec::<String>::new());

    // An archive that does not open: the importer never began, and makes nothing.
    write_archive(&path, 2, None);
    assert!(import(&path, importer(), false).is_err());
    assert_eq!(stage_files(dir.path()), Vec::<String>::new());

    // Stopped before the first frame.
    write_archive(&path, 1, None);
    assert_eq!(
        import(&path, importer(), true).unwrap_err(),
        ReadEnd::Stopped
    );
    assert_eq!(stage_files(dir.path()), Vec::<String>::new());

    // Stopped in the middle: the stop flag is set by the importer under test after its
    // fifth frame, as the account's stop sets it between two frames.
    struct StopAfter {
        inner: Box<dyn Importer>,
        left: usize,
        stop: Arc<AtomicBool>,
    }
    impl Importer for StopAfter {
        fn begin(&mut self, info: &ArchiveInfo) -> std::result::Result<(), ImportError> {
            self.inner.begin(info)
        }
        fn frame(&mut self, frame: Frame) -> std::result::Result<(), ImportError> {
            self.left -= 1;
            if self.left == 0 {
                self.stop.store(true, Ordering::Release);
            }
            self.inner.frame(frame)
        }
        fn commit(self: Box<Self>) -> std::result::Result<ImportSummary, ImportError> {
            self.inner.commit()
        }
        fn abort(self: Box<Self>) {
            self.inner.abort();
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    let end = import_archive(
        &path,
        &archive_key(),
        Limits::default(),
        Box::new(StopAfter {
            inner: importer(),
            left: 5,
            stop: stop.clone(),
        }),
        &stop,
        Duration::ZERO,
    )
    .unwrap_err();
    assert_eq!(end, ReadEnd::Stopped);
    assert_eq!(stage_files(dir.path()), Vec::<String>::new());

    // A stage over its cap fails the import as "too large", and nothing stays.
    let small = import::HistoryImporter::new(
        import_context(dir.path()),
        map::MapLimits::default(),
        stage::StageLimits {
            max_bytes: 600,
            commit_every: 100,
        },
    );
    let end = import(&path, Box::new(small), false).unwrap_err();
    let ReadEnd::Import(error) = end else {
        panic!("{end:?}");
    };
    assert_eq!(error.kind, HistoryTransferFailure::TooLarge);
    assert_eq!(stage_files(dir.path()), Vec::<String>::new());

    // The archive itself was never touched by any of it.
    assert!(import(&path, importer(), false).is_ok());
}

/// A kill takes the place of commit and abort: the next import of the account starts by
/// removing what the killed one left, whatever it is.
#[test]
fn begin_removes_the_stage_an_earlier_import_left() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archive");
    write_archive(&path, 1, None);
    // A whole stage of an earlier read that nobody recorded, and the WAL of a killed one.
    import(&path, history_importer(import_context(dir.path())), false).unwrap();
    std::fs::write(dir.path().join(format!("{}-wal", stage::FILE)), b"leftover").unwrap();
    let summary = import(&path, history_importer(import_context(dir.path())), false).unwrap();
    assert_eq!(
        summary.staged.unwrap().messages,
        3,
        "not six: the old rows are gone"
    );
    assert_eq!(stage_files(dir.path()), [stage::FILE]);
    assert_eq!(open_stage(dir.path()).unwrap().count().unwrap().messages, 3);
}

fn reading(db: &kourier_db::ZoneDb) {
    db.zone(|tx| begin(tx, &KEY, NOW, WAIT)).unwrap();
    assert_eq!(
        step(db, &Step::Pointer(pointer())),
        Some(Phase::Downloading)
    );
    assert_eq!(step(db, &Step::Downloaded), Some(Phase::Reading));
}

fn column(db: &kourier_db::ZoneDb, name: &str) -> i64 {
    db.read(|conn| -> Result<i64> {
        Ok(conn.query_row(
            &format!("SELECT {name} FROM history_transfer WHERE id = 1"),
            [],
            |r| r.get(0),
        )?)
    })
    .unwrap()
}

/// The commit of an import is one write, applies once and only to a transfer that reads,
/// survives a reopen, and every end of the transfer takes the record back.
#[test]
fn the_commit_zone_records_the_stage_once_and_an_end_takes_the_record_back() {
    let dir = tempfile::tempdir().unwrap();
    let totals = StageTotals {
        messages: 1234,
        chats: 56,
        shown_chats: 50,
        contacts: 60,
        bytes: 99_000,
    };
    let recorded = Recorded {
        messages: 1234,
        chats: 56,
        frames: 2000,
    };
    {
        let db = open(dir.path());
        // Not while the transfer waits or downloads.
        db.zone(|tx| begin(tx, &KEY, NOW, WAIT)).unwrap();
        assert!(!db.zone(|tx| commit_import(tx, &totals, 2000, NOW)).unwrap());
        assert_eq!(stored(&db).staged, None);
        assert_eq!(
            step(&db, &Step::Pointer(pointer())),
            Some(Phase::Downloading)
        );
        assert!(!db.zone(|tx| commit_import(tx, &totals, 2000, NOW)).unwrap());
        assert_eq!(step(&db, &Step::Downloaded), Some(Phase::Reading));
        // The read is open; the commit records the stage and closes the read in one write.
        assert_eq!(db.zone(|tx| note_read_start(tx, NOW)).unwrap(), 1);
        assert_eq!(column(&db, "read_open"), 1);
        assert!(
            db.zone(|tx| commit_import(tx, &totals, 2000, NOW + 1))
                .unwrap()
        );
        assert_eq!(stored(&db).staged, Some(recorded));
        assert_eq!(column(&db, "read_open"), 0);
        // Once: a second commit changes nothing.
        let other = StageTotals {
            messages: 1,
            ..totals
        };
        assert!(!db.zone(|tx| commit_import(tx, &other, 1, NOW + 2)).unwrap());
        assert_eq!(stored(&db).staged, Some(recorded));
        assert!(stored(&db).key.is_some(), "the key stays until the end");
    }
    let db = open(dir.path());
    assert_eq!(stored(&db).staged, Some(recorded), "it survives a reopen");

    // A recorded stage that cannot be used is forgotten, and the read that follows counts
    // as a start although the read before it had ended in an orderly way.
    db.zone(|tx| deliver::forget(tx, NOW + 3)).unwrap();
    assert_eq!(stored(&db).staged, None);
    assert_eq!(column(&db, "stage_messages"), 0);
    assert_eq!(db.zone(|tx| note_read_start(tx, NOW + 4)).unwrap(), 2);
    assert!(
        db.zone(|tx| commit_import(tx, &totals, 2000, NOW + 5))
            .unwrap()
    );

    // The end: the record goes with the key; the counts stay as what was imported.
    assert_eq!(step(&db, &Step::Read { frames: 2000 }), Some(Phase::Done));
    let end = stored(&db);
    assert_eq!(end.staged, None);
    assert!(end.key.is_none());
    assert_eq!(end.frames, Some(2000));
    assert_eq!(column(&db, "stage_messages"), 1234);

    // Every other end takes the record back as well.
    for failing in [
        Step::ReadFailed(HistoryTransferFailure::Internal),
        Step::Internal,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let db = open(dir.path());
        reading(&db);
        assert!(db.zone(|tx| commit_import(tx, &totals, 2000, NOW)).unwrap());
        assert!(step(&db, &failing).is_some_and(Phase::is_over));
        assert_eq!(stored(&db).staged, None);
    }
}

/// What a start does about the stage: one that no commit recorded goes before anything
/// else runs, a recorded one stays, and the archive stays in both cases.
#[test]
fn a_start_removes_a_stage_that_no_commit_recorded() {
    let at = |staged: Option<Recorded>| Stored {
        phase: Phase::Reading,
        key: None,
        pointer: None,
        file: None,
        bytes_done: 0,
        bytes_total: None,
        deadline_at: 0,
        download_ran: Duration::ZERO,
        frames: None,
        staged,
    };
    let recorded = Recorded {
        messages: 3,
        chats: 1,
        frames: 7,
    };
    for (staged, kept) in [(None, false), (Some(recorded), true)] {
        let dir = tempfile::tempdir().unwrap();
        let transfer = dir.path().join(DIR);
        std::fs::create_dir(&transfer).unwrap();
        write_archive(&transfer.join(FILE), 1, None);
        import(
            &transfer.join(FILE),
            history_importer(import_context(&transfer)),
            false,
        )
        .unwrap();
        // What a killed writer leaves beside the database.
        std::fs::write(transfer.join(format!("{}-wal", stage::FILE)), b"").unwrap();
        std::fs::write(transfer.join(format!("{}-shm", stage::FILE)), b"").unwrap();
        clean_at_start(&transfer, Some(&at(staged)));
        assert!(transfer.join(FILE).exists(), "the archive stays");
        assert_eq!(
            !stage_files(&transfer).is_empty(),
            kept,
            "recorded: {}",
            staged.is_some()
        );
    }
}

/// A rough measurement, not a test of anything: reads a generated archive of 50,000
/// messages over 200 chats into the stage and prints the time and the file sizes.
/// `cargo test -p kourier-signal --lib import_rate -- --ignored --nocapture`
#[test]
#[ignore = "a measurement; run it by name"]
fn import_rate_of_fifty_thousand_messages_into_the_stage() {
    use crate::archive::writer::{GeneratedSpec, HistorySpec, write_history};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archive");
    let spec = HistorySpec {
        generated: Some(GeneratedSpec {
            chats: 200,
            messages: 50_000,
            text: "a generated message of the usual length of a chat line, number".to_owned(),
            first_sent_at: 1_600_000_000_000,
        }),
        ..HistorySpec::default()
    };
    let frames = write_history(&path, &KEY, aci(), &spec).unwrap();
    let archive = std::fs::metadata(&path).unwrap().len();
    let began = std::time::Instant::now();
    let summary = import(&path, history_importer(import_context(dir.path())), false).unwrap();
    let took = began.elapsed();
    let totals = summary.staged.unwrap();
    let stage_file = std::fs::metadata(stage::path_in(dir.path())).unwrap().len();
    let began = std::time::Instant::now();
    let counted = open_stage(dir.path()).unwrap().count().unwrap();
    let check = began.elapsed();
    assert_eq!(summary.frames, frames);
    assert_eq!((totals.messages, totals.chats), (50_000, 200));
    assert_eq!(counted, totals);
    println!(
        "IMPORT RATE ({} build): {} frames, {} messages over {} chats; archive {} KiB; \
         verify + read + map + stage {:.2} s ({:.0} messages/s); stage file {} KiB, {} KiB of \
         payload ({} bytes a message); checking and counting the stage {:.2} s",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        summary.frames,
        totals.messages,
        totals.chats,
        archive / 1024,
        took.as_secs_f64(),
        50_000.0 / took.as_secs_f64(),
        stage_file / 1024,
        totals.bytes / 1024,
        totals.bytes / totals.messages,
        check.as_secs_f64(),
    );
}
