//! Kill at every point (§10.4): the whole stack (core, writer, supervisor, fake backend with
//! inbox, `core_outbox` and forwarder) runs in a child process that aborts at the n-th hit of a
//! failpoint, like a power cut after the last fsync. A second, clean run finishes the script;
//! the resulting `core.db` must equal the uncrashed run's (every message exactly once, system
//! messages, merges and read syncs included). Every failpoint is tried at every hit count.

mod support;

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kourier_backend_api::{NetworkRegistry, SystemClock};
use kourier_core::facade::{Query, QueryData, UiCommand};
use kourier_core::testing::{DirVault, DumpMode, dump};
use kourier_core::{Core, CoreConfig};
use kourier_fake::{FakeNetwork, Flavor, scripts};
use kourier_model::{AccountState, NetworkKind};
use support::{CORE_KEY, eventually};

const CHILD_HOME: &str = "KOURIER_TEST_CRASH_HOME";
const SIGABRT: i32 = 6;
const CHILD_TIMEOUT: Duration = Duration::from_secs(120);

/// Every kill point on the receive and read-sync paths.
const FAILPOINTS: &[&str] = &[
    kourier_fake::FAILPOINT_AFTER_INBOX_COMMIT,
    kourier_fake::FAILPOINT_BEFORE_ACK,
    kourier_fake::FAILPOINT_AFTER_BACKEND_EFFECTS_COMMIT,
    kourier_core::FAILPOINT_BEFORE_COMMIT,
    kourier_core::FAILPOINT_BEFORE_ACK,
    kourier_db::outbox::FAILPOINT_AFTER_CORE_COMMIT,
    kourier_core::FAILPOINT_AFTER_PENDING_COMMAND,
];

/// The child: start (or resume) the core, link an account if there is none, wait until the
/// script ran and every unread conversation was read (durable read sync), shut down.
#[test]
fn crash_child() {
    let Ok(home) = std::env::var(CHILD_HOME) else {
        return;
    };
    // The parent keeps this log and prints it when a run fails.
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
    let script = scripts::signal_like().expect("script");
    let fake = FakeNetwork::new(Flavor::SignalLike, script);
    let control = fake.control();
    let vault = Arc::new(DirVault::new(home.join("data")));
    std::fs::create_dir_all(home.join("data")).expect("data dir");
    let networks = NetworkRegistry::new()
        .with(Arc::new(fake))
        .expect("registry");
    let cfg = CoreConfig::new(
        home.join("data").join("core.db"),
        kourier_db::DbKey::from(CORE_KEY),
        networks,
        vault,
        Arc::new(SystemClock),
        tokio::runtime::Handle::current(),
    );
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
    let wait = Duration::from_secs(60);
    let h2 = h.clone();
    eventually("an active account", wait, || {
        let h = h2.clone();
        async move {
            accounts(h)
                .await
                .iter()
                .any(|a| a.state == AccountState::Active)
        }
    })
    .await;
    let uuid = accounts(h.clone())
        .await
        .into_iter()
        .find(|a| a.state == AccountState::Active)
        .expect("active")
        .uuid;
    tokio::time::timeout(wait, control.wait_for(|c| c.is_finished(uuid)))
        .await
        .expect("script finished");
    // Read every conversation (durable read sync and receipts), then wait for the backend to
    // persist the commands.
    let convs = match h
        .query(Query::Conversations(Default::default()))
        .await
        .expect("list")
    {
        QueryData::Conversations(p) => p.rows,
        _ => Vec::new(),
    };
    for c in convs.iter().filter(|c| c.unread > 0) {
        h.send(UiCommand::MarkRead { conv: c.id }).expect("send");
    }
    let db = home.join("data").join("core.db");
    eventually("read commands persisted", wait, || {
        let db = db.clone();
        let h = h.clone();
        async move {
            let unread = match h.query(Query::Conversations(Default::default())).await {
                Ok(QueryData::Conversations(p)) => p.rows.iter().map(|r| r.unread).sum::<u32>(),
                _ => 1,
            };
            let pending: i64 = tokio::task::spawn_blocking(move || {
                kourier_db::open_read_only(&db, &kourier_db::DbKey::from(CORE_KEY))
                    .and_then(|c| {
                        Ok(c.query_row("SELECT count(*) FROM pending_commands", [], |r| r.get(0))?)
                    })
                    .unwrap_or(1)
            })
            .await
            .unwrap_or(1);
            unread == 0 && pending == 0
        }
    })
    .await;
    core.shutdown().await;
}

fn run_child(home: &Path, failpoint: Option<&str>) -> ExitStatus {
    let exe = std::env::current_exe().expect("test binary");
    let mut cmd = Command::new(exe);
    cmd.args(["--exact", "crash_child", "--nocapture", "--test-threads=1"])
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

fn state(home: &Path) -> String {
    let conn = kourier_db::open_read_only(
        &home.join("data").join("core.db"),
        &kourier_db::DbKey::from(CORE_KEY),
    )
    .expect("open core.db");
    dump(&conn, DumpMode::Exact).expect("dump")
}

fn differing_lines(a: &str, b: &str) -> String {
    let x: std::collections::BTreeSet<&str> = a.lines().collect();
    let y: std::collections::BTreeSet<&str> = b.lines().collect();
    let mut out = String::new();
    for l in x.difference(&y) {
        out.push_str(&format!("- {}\n", &l[..l.len().min(400)]));
    }
    for l in y.difference(&x) {
        out.push_str(&format!("+ {}\n", &l[..l.len().min(400)]));
    }
    out
}

#[test]
fn kill_at_every_point_gives_the_uncrashed_state() {
    if std::env::var(CHILD_HOME).is_ok() || !kourier_db::failpoint::ENABLED {
        return;
    }
    let base_dir = tempfile::tempdir().expect("tempdir");
    assert!(run_child(base_dir.path(), None).success(), "uncrashed run");
    let baseline = state(base_dir.path());
    assert!(
        baseline.lines().filter(|l| l.starts_with("msg|")).count() >= 8,
        "{baseline}"
    );

    let mut kills = 0;
    for name in FAILPOINTS {
        let mut n = 1;
        loop {
            let dir = tempfile::tempdir().expect("tempdir");
            let spec = format!("{name}:{n}");
            let status = run_child(dir.path(), Some(&spec));
            if status.success() {
                // Fewer than n hits: this failpoint is exhausted; the run is uncrashed.
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
                "{spec}: state differs from the uncrashed run\n{}",
                differing_lines(&baseline, &got)
            );
            n += 1;
        }
    }
    assert!(kills >= FAILPOINTS.len());
}
