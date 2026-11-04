//! The actor's own log (decisions D3 M5, layer (b)): what a person types into a login or a request
//! never reaches a log line, at `trace`. The actor runs on the test's thread, so the test's
//! subscriber sees every line it writes.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use kourier_backend_api::{AccountContext, ContextParams, DbKey, SystemClock};
use kourier_model::{
    AccountId, AccountUuid, Identifier, IdentifierKind, Patch, ProfilePatch, Redacted,
};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::test_log::Lines;

/// Synthetic values of the shape a person types (none is a real number, code or name).
const PHONE: &str = "+15550001234";
const CODE: &str = "24680";
const PASSWORD: &str = "synthetic-pass-31415";
const NAME: &str = "Quentin Synthetic";
const ABOUT: &str = "a synthetic text of a profile";

#[test]
fn nothing_typed_into_a_login_or_a_request_reaches_a_log_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let net = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let (ctx, ends) = AccountContext::new(ContextParams {
        account: AccountId(1),
        uuid: AccountUuid::new_random(),
        start: StartMode::Fresh,
        backend_db: dir.path().join("backend.db"),
        db_key: DbKey::new([7; 32]),
        blob_dir: dir.path().join("blobs"),
        net_rt: net.handle().clone(),
        cancel: CancellationToken::new(),
        clock: Arc::new(SystemClock),
        config: Default::default(),
        ui: Default::default(),
        local_prefs: Default::default(),
    });
    let (resolved, resolve_rx) = oneshot::channel();
    let (profiled, profile_rx) = oneshot::channel();
    let commands = [
        Command::StartLogin {
            flow: "phone".to_owned(),
            relink: None,
            taken: Vec::new(),
        },
        Command::SubmitLoginInput {
            txn: 1,
            values: vec![
                ("phone".to_owned(), Redacted::new(PHONE.to_owned())),
                ("code".to_owned(), Redacted::new(CODE.to_owned())),
                ("password".to_owned(), Redacted::new(PASSWORD.to_owned())),
            ],
        },
        Command::Resolve {
            id: Identifier {
                kind: IdentifierKind::Phone,
                value: PHONE.to_owned(),
            },
            reply: resolved,
        },
        Command::ChangeProfile {
            patch: ProfilePatch {
                name: Patch::Set(NAME.to_owned()),
                about: Patch::Set(ABOUT.to_owned()),
                ..ProfilePatch::default()
            },
            reply: profiled,
        },
        Command::CancelLogin,
        Command::Logout,
    ];
    for command in commands {
        ends.commands.try_send(command).expect("queued");
    }
    let made = Arc::new(AtomicUsize::new(0));
    let lines = Lines::default();
    let exit = {
        let _guard = tracing::subscriber::set_default(lines.clone());
        run(ctx, TgParts::refusing(made.clone()))
    };
    assert!(
        matches!(exit, Exit::LoggedOut { .. }),
        "the actor did not end logged out"
    );
    assert_eq!(made.load(Ordering::SeqCst), 0, "the actor started a helper");
    assert!(
        resolve_rx.blocking_recv().is_ok_and(|r| r.is_err()),
        "Resolve was not refused"
    );
    assert!(
        profile_rx.blocking_recv().is_ok_and(|r| r.is_err()),
        "ChangeProfile was not refused"
    );
    let all = lines.all();
    assert!(
        all.iter()
            .any(|l| l.contains("a Telegram login is not available")),
        "the capture saw none of the actor's lines, so it proves nothing"
    );
    for secret in [PHONE, CODE, PASSWORD, NAME, ABOUT] {
        assert!(
            !all.iter().any(|l| l.contains(secret)),
            "a value typed by the person reached a log line"
        );
    }
}

/// KT-1b: a login start prints ONE fixed line, `a telegram login starts expected_number_check=..`, with
/// `on` when the parts carry an expected tail and `off` when they do not (an empty tail is still a
/// tail: it fails closed), the same words the fake that plays Telegram prints (`tests/source.rs`
/// compares the two files), and the tail itself reaches no line. The run tool reads the token before
/// it asks for a number or a scan. Mutant: `on` always.
#[test]
fn a_login_start_says_whether_the_number_is_checked_and_never_the_tail() {
    const TAIL: &str = "00518462";
    for (tail, want) in [(Some(TAIL), "on"), (Some(""), "on"), (None, "off")] {
        let dir = tempfile::tempdir().expect("tempdir");
        let net = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let (ctx, ends) = AccountContext::new(ContextParams {
            account: AccountId(1),
            uuid: AccountUuid::new_random(),
            start: StartMode::Fresh,
            backend_db: dir.path().join("backend.db"),
            db_key: DbKey::new([8; 32]),
            blob_dir: dir.path().join("blobs"),
            net_rt: net.handle().clone(),
            cancel: CancellationToken::new(),
            clock: Arc::new(SystemClock),
            config: kourier_backend_api::BackendConfig {
                expect_number_suffix: tail.map(|t| Redacted::new(t.to_owned())),
                ..Default::default()
            },
            ui: Default::default(),
            local_prefs: Default::default(),
        });
        for command in [
            Command::StartLogin {
                flow: "phone".to_owned(),
                relink: None,
                taken: Vec::new(),
            },
            Command::Logout,
        ] {
            ends.commands.try_send(command).expect("queued");
        }
        let lines = Lines::default();
        let exit = {
            let _guard = tracing::subscriber::set_default(lines.clone());
            run(ctx, TgParts::refusing(Arc::new(AtomicUsize::new(0))))
        };
        assert!(matches!(exit, Exit::LoggedOut { .. }));
        let all = lines.all();
        let starts: Vec<&String> = all
            .iter()
            .filter(|l| l.contains("a telegram login starts"))
            .collect();
        assert_eq!(starts.len(), 1, "{tail:?}: exactly one line: {starts:?}");
        assert!(
            starts[0].contains(&format!("expected_number_check=\"{want}\"")),
            "{tail:?}: the line says {want}: {}",
            starts[0]
        );
        assert!(
            starts[0].contains("target=kourier_telegram::actor"),
            "{}",
            starts[0]
        );
        if tail.is_some_and(|t| !t.is_empty()) {
            assert!(
                !all.iter().any(|l| l.contains(TAIL)),
                "the tail reached a log line"
            );
        }
    }
}

/// A fresh login never lands on an existing account: a `Fresh` start that finds an account row is
/// refused, and the row stays.
#[test]
fn a_fresh_start_on_an_existing_account_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("backend.db");
    let key = [9; 32];
    let row = db::AccountRow {
        env: crate::parts::TgEnv::Production,
        user_id: Some(77),
        pin: crate::pin::PIN.commit.to_owned(),
        created_ms: 1,
    };
    {
        let conn = db::open(&path, &kourier_db::DbKey::from_bytes(key)).expect("open");
        db::create_account(&conn, &row).expect("row");
    }
    let net = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let (ctx, _ends) = AccountContext::new(ContextParams {
        account: AccountId(1),
        uuid: AccountUuid::new_random(),
        start: StartMode::Fresh,
        backend_db: path.clone(),
        db_key: DbKey::new(key),
        blob_dir: dir.path().join("blobs"),
        net_rt: net.handle().clone(),
        cancel: CancellationToken::new(),
        clock: Arc::new(SystemClock),
        config: Default::default(),
        ui: Default::default(),
        local_prefs: Default::default(),
    });
    let exit = run(ctx, TgParts::refusing(Arc::new(AtomicUsize::new(0))));
    assert!(
        matches!(exit, Exit::Failed(_)),
        "a fresh start on an existing account was not refused"
    );
    let conn = db::open(&path, &kourier_db::DbKey::from_bytes(key)).expect("open");
    assert_eq!(
        db::account(&conn).expect("read"),
        Some(row),
        "the row changed"
    );
}

/// The context of an actor over `path` (no core command is queued).
fn context_on(
    path: &std::path::Path,
    key: [u8; 32],
    start: StartMode,
    net: &tokio::runtime::Runtime,
) -> (AccountContext, kourier_backend_api::CoreEnds) {
    AccountContext::new(ContextParams {
        account: AccountId(1),
        uuid: AccountUuid::new_random(),
        start,
        backend_db: path.to_path_buf(),
        db_key: DbKey::new(key),
        blob_dir: path.with_file_name("blobs"),
        net_rt: net.handle().clone(),
        cancel: CancellationToken::new(),
        clock: Arc::new(SystemClock),
        config: Default::default(),
        ui: Default::default(),
        local_prefs: Default::default(),
    })
}

/// Critic H2: a stored environment binds. A resume of a test-server account with production parts,
/// and of a production account with test parts, ends `Failed` before any helper is made (both
/// ways), and the row stays.
#[test]
fn a_resume_of_another_environment_is_refused_before_any_helper() {
    use crate::parts::TgEnv;
    for (stored, parts_env, said) in [
        (TgEnv::Test, TgEnv::Production, TEST_ACCOUNT),
        (TgEnv::Production, TgEnv::Test, PRODUCTION_ACCOUNT),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("backend.db");
        let key = [5; 32];
        let row = db::AccountRow {
            env: stored,
            user_id: Some(77),
            pin: crate::pin::PIN.commit.to_owned(),
            created_ms: 1,
        };
        {
            let conn = db::open(&path, &kourier_db::DbKey::from_bytes(key)).expect("open");
            db::create_account(&conn, &row).expect("row");
        }
        let net = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let (ctx, ends) = context_on(&path, key, StartMode::Resume, &net);
        // A Connect is queued: the refusal must come before it is ever read. The core's ends go
        // at once, so an actor that read it ends `Shutdown` (never hangs) and the test fails.
        ends.commands.try_send(Command::Connect).expect("queued");
        drop(ends);
        let made = Arc::new(AtomicUsize::new(0));
        let parts = TgParts::refusing(made.clone())
            .with_api(crate::ApiCredentials::DUMMY)
            .with_env(parts_env)
            .with_files_root(dir.path().join("media"));
        let exit = run(ctx, parts);
        // J3: UNSUPPORTED with a user message, so the core halts the account with these words at
        // once (Signal's rule) instead of restarting it until it gives up.
        assert_eq!(
            exit,
            Exit::Failed(wrong_environment(stored)),
            "a resume of another environment was not refused"
        );
        match &exit {
            Exit::Failed(e) => {
                assert_eq!(e.kind, kourier_model::ErrorKind::Unsupported);
                assert_eq!(e.detail, said);
                assert!(e.user_message.is_some(), "no words for the person");
            }
            _ => unreachable!("asserted above"),
        }
        assert_eq!(made.load(Ordering::SeqCst), 0, "a helper was made");
        let conn = db::open(&path, &kourier_db::DbKey::from_bytes(key)).expect("open");
        assert_eq!(
            db::account(&conn).expect("read"),
            Some(row),
            "the row changed"
        );
    }
}

/// The first login of a `Fresh` actor writes the account row (the parts' environment, no user id,
/// the pinned commit) BEFORE the helper starts: the refusing parts' one spawn comes after it.
#[test]
fn the_first_login_of_a_fresh_actor_writes_the_row_before_the_helper() {
    use crate::parts::TgEnv;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("backend.db");
    let key = [6; 32];
    // The gate starts on the network runtime: it needs its own threads and the IO driver.
    let net = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");
    let (ctx, ends) = context_on(&path, key, StartMode::Fresh, &net);
    for command in [
        Command::StartLogin {
            flow: "phone".to_owned(),
            relink: None,
            taken: Vec::new(),
        },
        Command::Logout,
    ] {
        ends.commands.try_send(command).expect("queued");
    }
    let made = Arc::new(AtomicUsize::new(0));
    let parts = TgParts::refusing(made.clone())
        .with_api(crate::ApiCredentials::DUMMY)
        .with_env(TgEnv::Test)
        .with_files_root(dir.path().join("media"));
    let exit = run(ctx, parts);
    assert!(
        matches!(exit, Exit::LoggedOut { .. }),
        "the actor did not end logged out"
    );
    assert_eq!(
        made.load(Ordering::SeqCst),
        1,
        "not exactly one helper spawn"
    );
    let conn = db::open(&path, &kourier_db::DbKey::from_bytes(key)).expect("open");
    let row = db::account(&conn).expect("read").expect("no account row");
    assert_eq!(row.env, TgEnv::Test);
    assert_eq!(row.user_id, None);
    assert_eq!(row.pin, crate::pin::PIN.commit);
}

// ---- TG-3 C7b: the group commit --------------------------------------------------------------

fn one_row() -> store::Staged {
    store::Staged {
        rows: vec![store::Row {
            events: Vec::new(),
            delivered: Vec::new(),
        }],
        ..store::Staged::default()
    }
}

/// The loop's order over a burst: before each frame a FULL group commits, each frame stages one
/// row; the queue empties after the burst (one last commit). Returns the rows per commit.
fn burst(frames: usize, ms_per_frame: u64) -> Vec<usize> {
    let mut group = Group::default();
    let mut commits = Vec::new();
    let mut now = 0;
    for _ in 0..frames {
        if group.full(now) {
            commits.push(group.take().rows.len());
        }
        group.on_frame();
        group.stage(one_row(), now);
        now += ms_per_frame;
    }
    if !group.is_empty() {
        commits.push(group.take().rows.len());
    }
    commits
}

/// Brief §4.3: a burst is committed in groups, not per frame: 64 frames (plus the one that
/// staged first) per commit while the queue holds frames, the rest when it empties; 50 ms since
/// the first staged item also closes a group.
#[test]
fn one_commit_per_burst() {
    assert_eq!(
        burst(1, 0),
        [1],
        "a single frame commits when the queue is empty"
    );
    assert_eq!(burst(200, 0), [65, 65, 65, 5]);
    assert_eq!(burst(30, 10), [5; 6], "50 ms close a slow group");
    let mut group = Group::default();
    group.on_frame();
    assert!(!group.full(1_000), "nothing staged: no commit");
    group.stage(store::Staged::default(), 0);
    assert!(group.is_empty(), "an empty stage stages nothing");
}
