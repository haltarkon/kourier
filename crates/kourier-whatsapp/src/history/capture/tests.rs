//! The capture commit: Ok only after the stage's commit, a chunk captured once, an error when the
//! stage is gone.

use std::sync::Arc;
use std::time::Duration;

use kourier_db::{DbKey, Priority};
use kourier_model::Redacted;
use tokio::sync::Notify;
use wacore::store::Device;
use wacore::store::traits::DeviceStore as _;

use super::{Capture, CaptureHook, Captured, Failed};
use crate::history::stage::{LazyStage, StageHandle};
use crate::link::{Gate, Rules};
use crate::store::StoreOwner;
use crate::tap::Tap;
use crate::translate::{Paired, WaSignal};

const BACKEND_KEY: [u8; 32] = [0x33; 32];
const OWNER: &str = "the account";

struct Rig {
    // Held for its files.
    _dir: tempfile::TempDir,
    stage: Arc<LazyStage>,
    wake: Arc<Notify>,
    capture: Capture,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().expect("dir");
    let stage = Arc::new(LazyStage::new(
        &dir.path().join("backend.db"),
        &DbKey::from(BACKEND_KEY),
    ));
    let wake = Arc::new(Notify::new());
    let capture = Capture::new(Arc::clone(&stage), Arc::clone(&wake));
    Rig {
        _dir: dir,
        stage,
        wake,
        capture,
    }
}

/// What the stage holds for `msg_id`: (state, bytes) or `None`.
fn chunk(handle: &StageHandle, msg_id: &str) -> Option<(String, Option<Vec<u8>>)> {
    use kourier_db::rusqlite::OptionalExtension as _;
    handle
        .reader()
        .expect("reader")
        .query_row(
            "SELECT state, (SELECT bytes FROM chunk_bytes b WHERE b.seq = chunks.seq) \
             FROM chunks WHERE msg_id = ?1",
            [msg_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .expect("query")
}

fn count(handle: &StageHandle) -> i64 {
    handle
        .reader()
        .expect("reader")
        .query_row("SELECT count(*) FROM chunks", [], |r| r.get(0))
        .expect("count")
}

#[tokio::test]
async fn the_ok_waits_for_the_stage_commit() {
    let rig = rig();
    let handle = rig.stage.handle(OWNER).expect("the stage");

    // The stage's writer is busy: its next job waits behind this one.
    let (release, parked) = std::sync::mpsc::channel::<()>();
    let (started, is_parked) = tokio::sync::oneshot::channel::<()>();
    let busy = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .write_at(Priority::High, "test park", None, move |_tx| {
                    let _ = started.send(());
                    let _ = parked.recv();
                    Ok(())
                })
                .await
        }
    });
    is_parked.await.expect("the writer took the parking job");

    let mut commit = Box::pin(rig.capture.commit(OWNER, "chunk-1", Some(0), vec![1, 2, 3]));
    let early = tokio::time::timeout(Duration::from_millis(300), &mut commit).await;
    assert!(
        early.is_err(),
        "the capture answered {early:?} while the stage could not commit"
    );
    assert!(
        chunk(&handle, "chunk-1").is_none(),
        "nothing is on disk yet"
    );

    release.send(()).expect("release the writer");
    let captured = commit.await.expect("the commit");
    assert_eq!(captured, Captured::New);
    let (state, bytes) = chunk(&handle, "chunk-1").expect("the chunk is on disk when Ok is given");
    assert_eq!(state, "captured");
    assert_eq!(bytes.as_deref(), Some(&[1u8, 2, 3][..]));
    busy.await.expect("task").expect("parking job");
}

#[tokio::test]
async fn a_second_notification_of_a_chunk_is_captured_once() {
    let rig = rig();
    assert_eq!(
        rig.capture
            .commit(OWNER, "chunk-1", Some(3), vec![9; 16])
            .await
            .expect("first"),
        Captured::New
    );
    let handle = rig.stage.handle(OWNER).expect("the stage");

    // The chunk went on to be delivered and settled: its bytes are gone and its state says so.
    handle
        .write_at(Priority::High, "test settle", None, |tx| {
            tx.execute(
                "UPDATE chunks SET state = 'done' WHERE msg_id = 'chunk-1'",
                [],
            )
            .map_err(crate::error::sql("test settle"))?;
            tx.execute("DELETE FROM chunk_bytes", [])
                .map_err(crate::error::sql("test settle"))?;
            Ok(())
        })
        .await
        .expect("settle");

    // Upstream hands the same notification to the hook again (the receipt of the first one was lost).
    let again = rig
        .capture
        .commit(OWNER, "chunk-1", Some(3), vec![9; 16])
        .await
        .expect("second");
    assert_eq!(again, Captured::Duplicate);
    assert_eq!(count(&handle), 1);
    let (state, bytes) = chunk(&handle, "chunk-1").expect("the chunk");
    assert_eq!(state, "done", "a settled chunk is not put back to captured");
    assert_eq!(bytes, None, "and its bytes do not come back");
}

#[tokio::test]
async fn two_clients_capture_one_id_once() {
    let rig = rig();
    // Two clients of one account (a reconnect that built a new Bot while the first one's task was still
    // running) hand the SAME notice to two captures that share the account's stage.
    let second = rig.capture.clone();
    let (a, b) = tokio::join!(
        rig.capture.commit(OWNER, "chunk-1", None, vec![7; 32]),
        second.commit(OWNER, "chunk-1", None, vec![7; 32]),
    );
    let mut outcomes = [a.expect("first"), b.expect("second")];
    outcomes.sort_by_key(|o| matches!(o, Captured::Duplicate));
    assert_eq!(outcomes, [Captured::New, Captured::Duplicate]);
    assert_eq!(count(&rig.stage.handle(OWNER).expect("stage")), 1);
}

#[tokio::test]
async fn a_new_chunk_wakes_the_driver_and_a_duplicate_does_not() {
    let rig = rig();
    rig.capture
        .commit(OWNER, "chunk-1", Some(0), vec![1])
        .await
        .expect("first");
    // A permit is stored: the driver that was not waiting yet still wakes.
    tokio::time::timeout(Duration::from_millis(500), rig.wake.notified())
        .await
        .expect("a new chunk wakes the driver");
    rig.capture
        .commit(OWNER, "chunk-1", Some(0), vec![1])
        .await
        .expect("again");
    assert!(
        tokio::time::timeout(Duration::from_millis(200), rig.wake.notified())
            .await
            .is_err(),
        "a chunk seen before wakes nobody"
    );
}

#[tokio::test]
async fn a_capture_after_the_stage_closed_is_an_error() {
    let rig = rig();
    rig.capture
        .commit(OWNER, "chunk-1", None, vec![1])
        .await
        .expect("first");
    rig.stage.close();
    let refused = rig.capture.commit(OWNER, "chunk-2", None, vec![2]).await;
    assert!(
        refused.is_err(),
        "a closed stage keeps nothing: {refused:?}"
    );
}

// ----- the adapter upstream calls -----

/// Fictitious numbers (the 555-01xx range).
const PN: &str = "15550100123";
const LID: &str = "99900100123";

/// A hook over a real store and a real stage in a directory of its own.
struct HookRig {
    _dir: tempfile::TempDir,
    _store: StoreOwner,
    stage: Arc<LazyStage>,
    hook: CaptureHook,
    signals: tokio::sync::mpsc::UnboundedReceiver<WaSignal>,
    tap: Tap,
}

/// `device`: what the store holds as the account's device; `gate`: the running login's, if any.
async fn hook_rig(device: Option<Device>, gate: Option<Arc<Gate>>) -> HookRig {
    let dir = tempfile::tempdir().expect("dir");
    let store = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from(BACKEND_KEY),
        "wa-test",
    )
    .expect("store");
    if let Some(device) = device {
        store.store().save(&device).await.expect("save the device");
    }
    let stage = Arc::new(LazyStage::new(
        &dir.path().join("backend.db"),
        &DbKey::from(BACKEND_KEY),
    ));
    let (tap, signals) = Tap::channel(gate);
    let capture = Capture::new(Arc::clone(&stage), Arc::new(Notify::new()));
    let hook = CaptureHook::new(capture, store.store(), tap.clone());
    HookRig {
        _dir: dir,
        _store: store,
        stage,
        hook,
        signals,
        tap,
    }
}

/// The device of a paired account.
fn paired_device() -> Device {
    let mut device = Device::new();
    device.pn = Some(format!("{PN}:3@s.whatsapp.net").parse().expect("jid"));
    device.lid = Some(format!("{LID}@lid").parse().expect("jid"));
    device
}

/// A gate whose rules refuse the pairing of PN (the run expects a number that ends otherwise).
fn refusing_gate() -> Arc<Gate> {
    Arc::new(Gate::new(Rules::new(
        Vec::new(),
        None,
        Some(Redacted::new("9999".to_owned())),
    )))
}

fn stranger() -> Paired {
    Paired {
        pn: Redacted::new(PN.to_owned()),
        lid: Redacted::new(LID.to_owned()),
        device: 3,
    }
}

/// A refused identity's history is never kept: the chunk gets an error (so no receipt), nothing is on
/// disk, not even the stage's file, and the failure is counted by its kind. A login whose pairing was
/// let through keeps its chunks. Mutant: the gate ignored.
#[tokio::test]
async fn a_refused_pairing_keeps_nothing() {
    // The pairing of another account's number is refused by the first line: the flag is set.
    let mut rig = hook_rig(Some(paired_device()), Some(refusing_gate())).await;
    let _ = rig.tap.judge_pairing(&[Some(stranger())]);
    assert!(rig.tap.is_refused(), "the gate did not refuse the stranger");

    let refused = rig.hook.keep("chunk-1", Some(0), &[1, 2, 3]).await;
    assert_eq!(refused, Err(Failed::Refused));
    assert!(!rig.stage.on_disk(), "a refused account made a stage file");
    assert_eq!(rig.hook.failures().count(Failed::Refused), 1);
    assert!(
        rig.signals.try_recv().is_err(),
        "a refused chunk ends nothing and tells nobody"
    );

    // The same hook over a gate that let the account through keeps the chunk.
    let open = Arc::new(Gate::new(Rules::new(Vec::new(), None, None)));
    let rig = hook_rig(Some(paired_device()), Some(open)).await;
    assert_eq!(
        rig.hook.keep("chunk-1", Some(0), &[1, 2, 3]).await,
        Ok(Captured::New)
    );
    assert!(rig.stage.on_disk());
}

/// The stage belongs to the account whose device is stored (its LID user), the chunk is in it with the
/// sync type, and the hook's second answer for the same id is the same row.
#[tokio::test]
async fn the_hook_keeps_a_chunk_in_the_stage_of_the_accounts_own_id() {
    let rig = hook_rig(Some(paired_device()), None).await;
    assert_eq!(
        rig.hook.keep("chunk-1", Some(2), &[5; 40]).await,
        Ok(Captured::New)
    );
    assert_eq!(
        rig.hook.keep("chunk-1", Some(2), &[5; 40]).await,
        Ok(Captured::Duplicate)
    );
    let handle = rig
        .stage
        .handle("anyone")
        .expect("the stage is open already");
    let reader = handle.reader().expect("reader");
    let owner: String = reader
        .query_row("SELECT owner FROM meta WHERE id = 1", [], |r| r.get(0))
        .expect("owner");
    assert_eq!(owner, LID, "the stage is not owned by the account's LID");
    let (sync_type, size): (Option<i64>, i64) = reader
        .query_row(
            "SELECT sync_type, size FROM chunks WHERE msg_id = 'chunk-1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("the chunk");
    assert_eq!((sync_type, size), (Some(2), 40));
    assert_eq!(count(&handle), 1);
    assert_eq!(rig.hook.failures().total(), 0);
}

/// LOW-2: a pair-success without a `lid` attribute stores a LID with no user; the owner falls through to the
/// number's user instead of there being none (before: every capture failed with `Failed::Owner`).
/// Mutant for the re-check: the empty filter of the LID taken out of `owner_of`.
#[tokio::test]
async fn an_empty_lid_falls_through_to_the_number_as_the_owner() {
    let mut device = paired_device();
    device.lid = Some(wacore_binary::Jid::lid(""));
    assert_eq!(super::owner_of(&device).as_deref(), Some(PN));
    let rig = hook_rig(Some(device), None).await;
    assert_eq!(
        rig.hook.keep("chunk-1", Some(2), &[5; 40]).await,
        Ok(Captured::New)
    );
    let handle = rig.stage.handle("anyone").expect("the stage");
    let owner: String = handle
        .reader()
        .expect("reader")
        .query_row("SELECT owner FROM meta WHERE id = 1", [], |r| r.get(0))
        .expect("owner");
    assert_eq!(owner, PN);
}

/// LOW-2: the owner is decided ONCE and kept: learning the LID after a chunk was captured does not turn the stage of
/// the number into a foreign one.
/// Mutant for the re-check: `decided_owner` recomputing from the device every time.
#[tokio::test]
async fn the_owner_is_decided_once_and_kept() {
    let dir = tempfile::tempdir().expect("dir");
    let store = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from(BACKEND_KEY),
        "wa-test",
    )
    .expect("store");
    let handle = store.store();
    let mut device = paired_device();
    device.lid = None;
    handle.save(&device).await.expect("save");
    assert_eq!(
        super::decided_owner(&handle)
            .await
            .expect("owner")
            .as_deref(),
        Some(PN)
    );
    // The LID is learned later: the owner stays the number's.
    handle.save(&paired_device()).await.expect("save again");
    assert_eq!(
        super::decided_owner(&handle)
            .await
            .expect("owner")
            .as_deref(),
        Some(PN)
    );
}

/// A failure is counted under its own kind, ends nothing (no signal to the actor: upstream keeps no
/// replay buffer, so a new connection would buy nothing) and the error is the same static text.
#[tokio::test]
async fn a_failure_is_counted_by_kind_and_ends_nothing() {
    // No device: nobody to own a stage.
    let mut rig = hook_rig(None, None).await;
    assert_eq!(
        rig.hook.keep("chunk-1", None, &[1]).await,
        Err(Failed::Owner)
    );
    // A device without an id of its own is no owner either.
    let rig_empty = hook_rig(Some(Device::new()), None).await;
    assert_eq!(
        rig_empty.hook.keep("chunk-1", None, &[1]).await,
        Err(Failed::Owner)
    );
    assert!(!rig_empty.stage.on_disk());

    // A stage that is closed keeps nothing.
    let mut rig2 = hook_rig(Some(paired_device()), None).await;
    rig2.hook.keep("chunk-1", None, &[1]).await.expect("first");
    rig2.stage.close();
    assert_eq!(
        rig2.hook.keep("chunk-2", None, &[2]).await,
        Err(Failed::Stage)
    );
    let failures = rig2.hook.failures();
    assert_eq!(failures.count(Failed::Stage), 1);
    assert_eq!(failures.count(Failed::Owner), 0);
    assert_eq!(failures.count(Failed::Refused), 0);
    assert_eq!(rig.hook.failures().count(Failed::Owner), 1);
    assert!(
        rig.signals.try_recv().is_err(),
        "a failed capture ended the connection"
    );
    assert!(
        rig2.signals.try_recv().is_err(),
        "a failed capture ended the connection"
    );
}

/// T1a (A5): a NEW chunk is logged as ONE fixed line that says whether the notice that announced it was
/// recorded before the chunk came (`history_announced`: the run tool reads it to learn whether a hard stop makes
/// the phone upload a chunk again), and the line holds no id and no size. A chunk seen before logs nothing.
/// Mutants: no line; the flag always false; a line for a duplicate.
#[tokio::test]
async fn a_capture_logs_one_fixed_line_without_an_id() {
    let rig = hook_rig(Some(paired_device()), None).await;
    rig._store
        .store()
        .write("test announce", |tx| {
            tx.execute(
                "INSERT INTO history_announced(msg_id, announced_at) VALUES ('NOTICE-ONE', 1)",
                [],
            )
            .map(drop)
            .map_err(crate::error::sql("test announce"))
        })
        .await
        .expect("announce");

    let lines = crate::test_log::Lines::default();
    let _guard = tracing::subscriber::set_default(lines.clone());
    assert_eq!(
        rig.hook.keep("NOTICE-ONE", Some(2), &[5; 40]).await,
        Ok(Captured::New)
    );
    assert_eq!(
        rig.hook.keep("NOTICE-TWO", Some(2), &[6; 41]).await,
        Ok(Captured::New)
    );
    assert_eq!(
        rig.hook.keep("NOTICE-ONE", Some(2), &[5; 40]).await,
        Ok(Captured::Duplicate)
    );

    let said = lines.containing("history chunk captured");
    assert_eq!(said.len(), 2, "one line per NEW chunk: {:?}", lines.all());
    assert!(
        said[0].contains("announced_earlier=true"),
        "its notice was recorded: {said:?}"
    );
    assert!(
        said[1].contains("announced_earlier=false"),
        "no notice was recorded for it: {said:?}"
    );
    for line in lines.all() {
        assert!(
            !line.contains("NOTICE") && !line.contains("40") && !line.contains("41"),
            "an id or a size in a line: {line}"
        );
    }
}
