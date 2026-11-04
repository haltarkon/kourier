//! Kill tests: a CHILD process (this binary again, `--exact kill::child`) performs one operation and
//! is aborted at a failpoint (`KOURIER_FAILPOINT=<name>:<n>`, like a power cut after the last fsync:
//! no destructor, no rollback, no flush). The parent reopens the file with the same key and holds the
//! state to "exactly the state before, or exactly the complete state after, never in between".
//!
//! Every test LOOPS over n = 1, 2, ... until the child exits normally. One kill point per operation
//! would not see an operation written as two transactions: both commits hit the same name, and a kill
//! at `:1` leaves exactly the old state. At `:2` such a mutant is torn, and the loop finds it. The
//! `AFTER` points are checked the same way (a multi-row operation must be whole after its commit).
//! The statement that the loop does hit the point (at least one kill) is part of every test.

use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Command;

use bytes::Bytes;
use kourier_db::rusqlite::Connection;
use kourier_whatsapp::failpoints as fp;
use kourier_whatsapp::{KourierStore, StoreOwner};
use wacore::appstate::hash::HashState;
use wacore::appstate::processor::AppStateMutationMAC;
use wacore::store::traits::{
    AppSyncStore, DeviceStore, MsgSecretEntry, MsgSecretStore, PendingInboundKey,
    PendingInboundLease, PendingInboundPayload, PendingInboundRevision, ProtocolStore, SignalStore,
};

use crate::support::{self, db_path, key};

const OP: &str = "WA_KILL_OP";
const DIR: &str = "WA_KILL_DIR";
/// The most hits of one failpoint any operation here makes. A loop that goes past it is a bug.
const MAX_HITS: u32 = 8;

fn session(byte: u8) -> Bytes {
    Bytes::from(vec![byte; 4])
}

fn secret(msg_id: &str, byte: u8, expires_at: i64, message_ts: i64) -> MsgSecretEntry {
    MsgSecretEntry {
        chat: "chat@g.us".into(),
        sender: "sender@lid".into(),
        msg_id: msg_id.into(),
        secret: [byte; 32],
        expires_at,
        message_ts,
    }
}

fn device_after() -> wacore::store::Device {
    let mut device = crate::device::fixture_device();
    device.push_name = "After".to_owned();
    device.next_pre_key_id += 100;
    device
}

/// The state before an operation: written by the parent, in process, through the store's own API.
async fn before(op: &str, s: &KourierStore) {
    match op {
        "sessions" => {
            s.put_session("a@lid", &[1, 1]).await.unwrap();
            s.put_session("keep@lid", &[2, 2]).await.unwrap();
        }
        "device" => s.save(&crate::device::fixture_device()).await.unwrap(),
        "secrets" => s
            .put_msg_secrets(vec![secret("m0", 1, 500, 10)])
            .await
            .map(drop)
            .unwrap(),
        "pending" => s
            .store_pending_inbound(PENDING.chat, PENDING.sender, PENDING.id, b"old")
            .await
            .unwrap(),
        "patch" => s
            .commit_patch("regular", hash_state(1), &[], &[mac(1, 1), mac(2, 2)])
            .await
            .unwrap(),
        "version" => s.set_version("regular", hash_state(1)).await.unwrap(),
        other => panic!("unknown operation {other}"),
    }
}

const PENDING: PendingInboundKey<'static> = PendingInboundKey {
    chat: "chat@g.us",
    sender: "200@lid",
    id: "KILL1",
};

fn hash_state(version: u64) -> HashState {
    HashState {
        version,
        hash: [version as u8; 128],
        ..HashState::default()
    }
}

fn mac(index: u8, value: u8) -> AppStateMutationMAC {
    AppStateMutationMAC {
        index_mac: vec![index; 32],
        value_mac: vec![value; 32],
    }
}

/// The operation under test, run by the child.
async fn operation(op: &str, s: &KourierStore) {
    match op {
        "sessions" => s
            .put_sessions_batch(&[
                ("a@lid".into(), session(7)),
                ("b@lid".into(), session(8)),
                ("c@lid".into(), session(9)),
            ])
            .await
            .unwrap(),
        "device" => s.save(&device_after()).await.unwrap(),
        "secrets" => s
            .put_msg_secrets(vec![
                secret("m0", 9, 900, 5),
                secret("m1", 2, 0, 20),
                secret("m2", 3, 700, 30),
            ])
            .await
            .map(drop)
            .unwrap(),
        "pending" => {
            let replacement = PendingInboundPayload::new(
                Bytes::from_static(b"new"),
                PendingInboundLease::new(std::sync::Arc::new(())),
            );
            s.replace_pending_inbound_if(
                PENDING,
                PendingInboundRevision::for_bytes(b"old"),
                replacement,
                64,
            )
            .await
            .unwrap();
        }
        "patch" => s
            .commit_patch(
                "regular",
                hash_state(2),
                &[vec![1; 32]],
                &[mac(3, 3), mac(2, 9)],
            )
            .await
            .unwrap(),
        "version" => s.set_version("regular", hash_state(2)).await.unwrap(),
        other => panic!("unknown operation {other}"),
    }
}

/// What a state looks like to the comparison: every row the operation can touch, as text.
fn dump(dir: &Path, op: &str) -> Vec<String> {
    let conn: Connection = kourier_db::open(&db_path(dir), &key()).expect("reopen with the key");
    let query = match op {
        "sessions" => "SELECT address || '=' || hex(record) FROM sessions ORDER BY address",
        "device" => "SELECT format || ':' || hex(record) FROM device",
        "secrets" => {
            "SELECT msg_id || '=' || hex(secret) || ',' || expires_at || ',' || message_ts \
             FROM msg_secrets ORDER BY msg_id"
        }
        "pending" => {
            "SELECT id || '=' || hex(message) || ',' || (inserted_at > 0) FROM pending_inbound_messages"
        }
        "patch" => {
            "SELECT 'v:' || name || '=' || hex(state) FROM app_state_versions \
             UNION ALL SELECT 'm:' || name || hex(index_mac) || '=' || hex(value_mac) || ',' || version \
               FROM app_state_mutation_macs \
             UNION ALL SELECT 'd:' || name || '=' || version FROM appstate_dirty ORDER BY 1"
        }
        "version" => {
            "SELECT 'v:' || name || '=' || hex(state) FROM app_state_versions \
             UNION ALL SELECT 'd:' || name || '=' || version FROM appstate_dirty ORDER BY 1"
        }
        other => panic!("unknown operation {other}"),
    };
    let mut stmt = conn.prepare(query).expect("prepare");
    stmt.query_map([], |r| r.get::<_, String>(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows")
}

/// The child: performs the operation named by the environment, or does nothing when it runs as an
/// ordinary test.
#[test]
fn child() {
    let (Ok(op), Ok(dir)) = (std::env::var(OP), std::env::var(DIR)) else {
        return;
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let owner = StoreOwner::open(&db_path(Path::new(&dir)), &key(), "wa-kill").expect("open");
        operation(&op, &owner.store()).await;
        owner.close();
    });
}

/// Runs the child on `dir`; `point` arms `KOURIER_FAILPOINT`. Returns how it ended and what it said.
fn run_child(dir: &Path, op: &str, point: Option<String>) -> (std::process::ExitStatus, String) {
    let mut cmd = Command::new(std::env::current_exe().expect("test binary"));
    cmd.arg("--exact")
        .arg(support::child_filter(module_path!(), "child"))
        .args(["--nocapture", "--test-threads=1"])
        .env(OP, op)
        .env(DIR, dir)
        .env_remove("KOURIER_FAILPOINT");
    if let Some(point) = point {
        cmd.env("KOURIER_FAILPOINT", point);
    }
    let out = cmd.output().expect("spawn the child");
    (
        out.status,
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A fresh directory in the state before the operation, closed.
async fn fresh(op: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    let owner = StoreOwner::open(&db_path(dir.path()), &key(), "wa-before").expect("open");
    before(op, &owner.store()).await;
    owner.close();
    dir
}

/// The loop: kills `op` at `point:1`, `point:2`, ... until the child ends normally. Every kill must
/// be an abort at that failpoint and leave exactly `before` or exactly `after`; `expect` says which
/// of the two this point stands for. Returns how many kills there were.
async fn kill_at_every_hit(op: &str, point: &str, expect: Expect) -> u32 {
    let reference = fresh(op).await;
    let state_before = dump(reference.path(), op);
    let (status, said) = run_child(reference.path(), op, None);
    assert!(status.success(), "the operation itself failed: {said}");
    let state_after = dump(reference.path(), op);
    assert_ne!(state_before, state_after, "the operation changed nothing");

    let mut kills = 0;
    for n in 1..=MAX_HITS + 1 {
        assert!(n <= MAX_HITS, "{point} is hit more than {MAX_HITS} times");
        let dir = fresh(op).await;
        let (status, said) = run_child(dir.path(), op, Some(format!("{point}:{n}")));
        let state = dump(dir.path(), op);
        if status.success() {
            assert_eq!(
                state, state_after,
                "{point}:{n} was never reached, and the state differs"
            );
            break;
        }
        assert_eq!(
            status.signal(),
            Some(6),
            "{point}:{n} must abort: {status:?} {said}"
        );
        assert!(said.contains("hit: aborting"), "{point}:{n}: {said}");
        assert!(
            state == state_before || state == state_after,
            "{point}:{n}: the file holds neither the state before nor the complete state after:\n{state:#?}"
        );
        match expect {
            Expect::Before => assert_eq!(state, state_before, "{point}:{n} is before the commit"),
            Expect::After => assert_eq!(state, state_after, "{point}:{n} is after the commit"),
        }
        kills += 1;
    }
    assert!(
        kills >= 1,
        "{point} was never hit: the kill test saw nothing"
    );
    kills
}

#[derive(Clone, Copy)]
enum Expect {
    Before,
    After,
}

#[tokio::test]
async fn a_batch_of_sessions_is_all_or_nothing_at_every_kill_point() {
    kill_at_every_hit("sessions", fp::INSIDE_SESSIONS_BATCH, Expect::Before).await;
    kill_at_every_hit("sessions", fp::AFTER_SESSIONS_BATCH, Expect::After).await;
}

#[tokio::test]
async fn the_device_save_is_all_or_nothing_at_every_kill_point() {
    kill_at_every_hit("device", fp::INSIDE_DEVICE_SAVE, Expect::Before).await;
    kill_at_every_hit("device", fp::AFTER_DEVICE_SAVE, Expect::After).await;
}

#[tokio::test]
async fn message_secrets_are_all_or_nothing_at_every_kill_point() {
    kill_at_every_hit("secrets", fp::INSIDE_MSG_SECRETS, Expect::Before).await;
    kill_at_every_hit("secrets", fp::AFTER_MSG_SECRETS, Expect::After).await;
}

#[tokio::test]
async fn a_pending_inbound_compare_and_swap_is_all_or_nothing_at_every_kill_point() {
    kill_at_every_hit("pending", fp::INSIDE_PENDING_CAS, Expect::Before).await;
    kill_at_every_hit("pending", fp::AFTER_PENDING_CAS, Expect::After).await;
}

#[tokio::test]
async fn a_patch_is_all_or_nothing_at_every_kill_point() {
    kill_at_every_hit("patch", fp::INSIDE_COMMIT_PATCH, Expect::Before).await;
    kill_at_every_hit("patch", fp::AFTER_COMMIT_PATCH, Expect::After).await;
}

#[tokio::test]
async fn a_version_and_its_dirty_row_are_all_or_nothing_at_every_kill_point() {
    kill_at_every_hit("version", fp::INSIDE_SET_VERSION, Expect::Before).await;
    kill_at_every_hit("version", fp::AFTER_SET_VERSION, Expect::After).await;
}
