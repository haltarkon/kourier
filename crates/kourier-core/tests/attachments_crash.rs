//! The attachment state machine across kills (§5.8, §10.4): a child process runs the core and
//! the fake through incoming attachments (a photo, a file, one the CDN lost) and aborts at the
//! n-th hit of a failpoint: in the fake, in ingest, between the download's commit and its
//! command, inside the thumbnail transaction. A clean second run must end in the uncrashed
//! state: every download answered once, every picture thumbnailed, no row naming a blob that is
//! missing or does not decrypt.

mod support;

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kourier_backend_api::{BackendEvent, NetworkRegistry, SystemClock};
use kourier_core::facade::{Query, QueryData, UiCommand};
use kourier_core::testing::{DirVault, DumpMode, dump};
use kourier_core::{AccountVault, Core, CoreConfig};
use kourier_fake::attachments::{expired_attachment, synthetic_file, synthetic_image};
use kourier_fake::scripts::{T0, remote};
use kourier_fake::{FakeNetwork, Flavor, Script};
use kourier_model::{AccountState, AttachmentKind, Body, NetworkKind};
use support::send::{ME, ME_PNI, alice, d_alice, setup, uid};
use support::{CORE_KEY, eventually};

const CHILD_HOME: &str = "KOURIER_TEST_ATTACHMENT_CRASH_HOME";
const SIGABRT: i32 = 6;
const CHILD_TIMEOUT: Duration = Duration::from_secs(120);

const FAILPOINTS: &[&str] = &[
    kourier_fake::FAILPOINT_AFTER_INBOX_COMMIT,
    kourier_fake::FAILPOINT_AFTER_BACKEND_EFFECTS_COMMIT,
    kourier_core::FAILPOINT_BEFORE_COMMIT,
    kourier_core::FAILPOINT_BEFORE_ACK,
    kourier_db::outbox::FAILPOINT_AFTER_CORE_COMMIT,
    kourier_core::FAILPOINT_BEFORE_DOWNLOAD_SEND,
    kourier_core::FAILPOINT_BEFORE_DERIVE_COMMIT,
];

fn script() -> Script {
    let mut photo = remote(&d_alice(), &alice(), &(T0 + 1).to_string(), T0 + 1, "");
    photo.body = Body::Media {
        attachments: vec![synthetic_image(640, 480, 3)],
        caption: None,
    };
    let mut docs = remote(&d_alice(), &alice(), &(T0 + 2).to_string(), T0 + 2, "");
    docs.body = Body::Media {
        attachments: vec![
            synthetic_file("a.bin", "application/octet-stream", 70_000, 4),
            expired_attachment(AttachmentKind::Image),
        ],
        caption: None,
    };
    Script::new("attachment-crash", vec![uid(ME), uid(ME_PNI)])
        .deliver(setup())
        .deliver(vec![BackendEvent::Message(photo)])
        .deliver(vec![BackendEvent::Message(docs)])
}

/// Whether every attachment reached its final state: two files local and derived (the photo
/// with a thumbnail), the lost one expired.
fn settled(db: &Path) -> bool {
    let Ok(conn) = kourier_db::open_read_only(db, &kourier_db::DbKey::from(CORE_KEY)) else {
        return false;
    };
    let q = "SELECT
        (SELECT count(*) FROM attachments WHERE state = '\"Local\"' AND derived_at IS NOT NULL),
        (SELECT count(*) FROM attachments WHERE thumb_file_id IS NOT NULL),
        (SELECT count(*) FROM attachments WHERE state = '\"Expired\"'),
        (SELECT count(*) FROM attachment_downloads)";
    conn.query_row(q, [], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })
    .is_ok_and(|n| n == (2, 1, 1, 0))
}

#[test]
fn attachment_crash_child() {
    let Ok(home) = std::env::var(CHILD_HOME) else {
        return;
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter("kourier_core=debug,kourier_fake=debug,kourier_db=info")
        .with_writer(std::io::stderr)
        .try_init();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(child(PathBuf::from(home)));
}

async fn child(home: PathBuf) {
    let fake = FakeNetwork::new(Flavor::SignalLike, script());
    let vault = Arc::new(DirVault::new(home.join("data")));
    std::fs::create_dir_all(home.join("data")).expect("data dir");
    let networks = NetworkRegistry::new()
        .with(Arc::new(fake))
        .expect("registry");
    let mut cfg = CoreConfig::new(
        home.join("data").join("core.db"),
        kourier_db::DbKey::from(CORE_KEY),
        networks,
        vault,
        Arc::new(SystemClock),
        tokio::runtime::Handle::current(),
    );
    cfg.media_cache = Some(home.join("run").join("media"));
    let core = Core::start(cfg).await.expect("core");
    let h = core.handle();
    h.send(UiCommand::UiReady).expect("send");
    let accounts = |h: kourier_core::facade::CoreHandle| async move {
        match h.query(Query::Accounts).await.expect("accounts") {
            QueryData::Accounts(a) => a,
            _ => Vec::new(),
        }
    };
    if !accounts(h.clone())
        .await
        .iter()
        .any(|a| a.state == AccountState::Active)
    {
        h.send(UiCommand::AddAccount {
            network: NetworkKind::Fake,
            flow: "qr".into(),
        })
        .expect("send");
    }
    let db = home.join("data").join("core.db");
    eventually("attachments settled", Duration::from_secs(60), || {
        let db = db.clone();
        async move {
            tokio::task::spawn_blocking(move || settled(&db))
                .await
                .unwrap_or(false)
        }
    })
    .await;
    core.shutdown().await;
}

fn run_child(home: &Path, failpoint: Option<&str>) -> ExitStatus {
    let exe = std::env::current_exe().expect("test binary");
    let mut cmd = Command::new(exe);
    cmd.args([
        "--exact",
        "attachment_crash_child",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(CHILD_HOME, home)
    .env_remove(kourier_db::failpoint::ENV)
    .stdout(Stdio::null())
    .stderr(Stdio::from(
        std::fs::File::create(home.join(format!("child-{}.log", failpoint.unwrap_or("clean"))))
            .expect("log file"),
    ));
    if let Some(fp) = failpoint {
        cmd.env(kourier_db::failpoint::ENV, fp);
    }
    let mut child = cmd.spawn().expect("spawn child");
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("wait") {
            return status;
        }
        if start.elapsed() > CHILD_TIMEOUT {
            let _ = child.kill();
            panic!("child hung (failpoint {failpoint:?})");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The canonical dump, after checking that every `files` row's blob exists and decrypts.
fn state(home: &Path) -> String {
    let conn = kourier_db::open_read_only(
        &home.join("data").join("core.db"),
        &kourier_db::DbKey::from(CORE_KEY),
    )
    .expect("open core.db");
    let vault = DirVault::new(home.join("data"));
    let uuid: String = conn
        .query_row("SELECT uuid FROM accounts", [], |r| r.get(0))
        .expect("account");
    let blobs = vault.blob_dir(uuid.parse().expect("uuid"));
    let mut stmt = conn
        .prepare("SELECT path, key FROM files")
        .expect("prepare");
    let files: Vec<(String, Vec<u8>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert_eq!(files.len(), 3, "photo, its thumbnail, the file");
    for (path, key) in files {
        let key: [u8; 32] = key.try_into().expect("key");
        kourier_blob::read_all(&blobs.join(path), &kourier_blob::FileKey::from_bytes(key))
            .expect("every recorded blob decrypts");
    }
    dump(&conn, DumpMode::Exact).expect("dump")
}

#[test]
fn kills_anywhere_in_the_attachment_pipeline_give_the_uncrashed_state() {
    if std::env::var(CHILD_HOME).is_ok() || !kourier_db::failpoint::ENABLED {
        return;
    }
    let base = tempfile::tempdir().expect("tempdir");
    assert!(run_child(base.path(), None).success(), "uncrashed run");
    let baseline = state(base.path());
    let mut kills = 0;
    for name in FAILPOINTS {
        let mut n = 1;
        loop {
            let dir = tempfile::tempdir().expect("tempdir");
            let spec = format!("{name}:{n}");
            let status = run_child(dir.path(), Some(&spec));
            if status.success() {
                assert!(n > 1, "{name} was never hit");
                break;
            }
            assert_eq!(status.signal(), Some(SIGABRT), "{spec}: {status:?}");
            kills += 1;
            if !run_child(dir.path(), None).success() {
                let log =
                    std::fs::read_to_string(dir.path().join("child-clean.log")).unwrap_or_default();
                let tail: Vec<&str> = log.lines().rev().take(60).collect();
                let tail: Vec<&str> = tail.into_iter().rev().collect();
                panic!("{spec}: restart run failed; log tail:\n{}", tail.join("\n"));
            }
            let got = state(dir.path());
            assert!(
                got == baseline,
                "{spec}: state differs from the uncrashed run\n--- uncrashed\n{baseline}\n--- got\n{got}"
            );
            n += 1;
        }
    }
    assert!(kills >= FAILPOINTS.len());
}
