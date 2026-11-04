//! Shared test setup: a core over a `FakeNetwork` in a temp directory with a `DirVault` and a
//! `FakeClock`.

#![allow(dead_code)]

pub mod judge;
pub mod send;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{FakeClock, NetworkRegistry};
use kourier_core::facade::{
    AccountRow, ConversationListQuery, ConversationRow, CoreHandle, MessageRow, Query, QueryData,
    TimelineAnchor, TimelineQuery, UiEvent,
};
use kourier_core::testing::DirVault;
use kourier_core::{AccountVault, Core, CoreConfig, RestartPolicy, Timeouts};
use kourier_fake::{FakeControl, FakeNetwork, Flavor, Script};
use kourier_model::{AccountId, ConversationId};
use tokio::sync::broadcast;

pub const CORE_KEY: [u8; 32] = [0xc0; 32];

/// Polls `f` every 5 ms until it holds; panics after `timeout`.
pub async fn eventually<F, Fut>(what: &str, timeout: Duration, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if f().await {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

pub const WAIT: Duration = Duration::from_secs(20);

/// The number of files below `dir` (the blobs of an account).
pub fn blob_files(dir: &Path) -> usize {
    let mut n = 0;
    let mut stack = vec![dir.to_owned()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            if e.path().is_dir() {
                stack.push(e.path());
            } else {
                n += 1;
            }
        }
    }
    n
}

pub struct Env {
    pub dir: tempfile::TempDir,
    pub clock: FakeClock,
    pub fake: FakeNetwork,
    pub control: Arc<FakeControl>,
    pub vault: Arc<DirVault>,
    pub core: Option<Core>,
    pub handle: CoreHandle,
    pub events: broadcast::Receiver<UiEvent>,
}

impl Env {
    pub fn data(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    pub fn core_db(&self) -> PathBuf {
        self.data().join("core.db")
    }

    /// The plaintext media cache (`run/media`, like `KOURIER_HOME`'s layout).
    pub fn media_dir(&self) -> PathBuf {
        self.dir.path().join("run").join("media")
    }

    pub async fn new(flavor: Flavor, script: Script) -> Self {
        Self::over(tempfile::tempdir().expect("tempdir"), flavor, script).await
    }

    /// A core whose profile is a directory made inside `parent` (the child of a kill test: the
    /// kill leaves it there for [`Env::adopt`]).
    pub async fn inside(parent: &Path, flavor: Flavor, script: Script) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("profile")
            .tempdir_in(parent)
            .expect("tempdir");
        Self::over(dir, flavor, script).await
    }

    /// The profile a killed child left in `parent` (made by [`Env::inside`]), moved into a
    /// directory of this process's own, and a core started over it.
    pub async fn adopt(parent: &Path, flavor: Flavor, script: Script) -> Self {
        let left = std::fs::read_dir(parent)
            .expect("read the parent")
            .flatten()
            .map(|e| e.path())
            .find(|p| p.is_dir() && p.join("data").exists())
            .expect("the child's profile");
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::remove_dir(dir.path()).expect("the empty directory");
        std::fs::rename(&left, dir.path()).expect("move the profile");
        Self::over(dir, flavor, script).await
    }

    /// A core over a fake that PLAYS the network `kind` (`NetworkKind::WhatsApp`: a network that
    /// has no links to people); its accounts are added with [`add_linked_as`].
    pub async fn playing(kind: kourier_model::NetworkKind, script: Script) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        Self::over_fake(dir, FakeNetwork::playing(kind, script)).await
    }

    async fn over(dir: tempfile::TempDir, flavor: Flavor, script: Script) -> Self {
        Self::over_fake(dir, FakeNetwork::new(flavor, script)).await
    }

    async fn over_fake(dir: tempfile::TempDir, fake: FakeNetwork) -> Self {
        let clock = FakeClock::new(u64::try_from(kourier_fake::scripts::T0).unwrap_or(0));
        let control = fake.control();
        let vault = Arc::new(DirVault::new(dir.path().join("data")));
        let core = start_core(dir.path(), &fake, &vault, &clock).await;
        let handle = core.handle();
        let events = handle.subscribe();
        Self {
            dir,
            clock,
            fake,
            control,
            vault,
            core: Some(core),
            handle,
            events,
        }
    }

    /// Shuts the core down cleanly.
    pub async fn stop(&mut self) {
        if let Some(core) = self.core.take() {
            core.shutdown().await;
        }
    }

    /// Stops and starts the core again on the same files.
    pub async fn restart(&mut self) {
        self.stop().await;
        let core = start_core(self.dir.path(), &self.fake, &self.vault, &self.clock).await;
        self.handle = core.handle();
        self.events = self.handle.subscribe();
        self.core = Some(core);
    }

    /// The next event matching `f` (others are skipped).
    pub async fn event<T>(&mut self, what: &str, mut f: impl FnMut(&UiEvent) -> Option<T>) -> T {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(left, self.events.recv()).await {
                Ok(Ok(e)) => {
                    if let Some(t) = f(&e) {
                        return t;
                    }
                }
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Err(broadcast::error::RecvError::Closed)) => {
                    panic!("event channel closed waiting for {what}")
                }
                Err(_) => panic!("timed out waiting for event {what}"),
            }
        }
    }

    pub async fn accounts(&self) -> Vec<AccountRow> {
        match self.handle.query(Query::Accounts).await.expect("accounts") {
            QueryData::Accounts(a) => a,
            other => panic!("unexpected {other:?}"),
        }
    }

    pub async fn account(&self, id: AccountId) -> Option<AccountRow> {
        self.accounts().await.into_iter().find(|a| a.id == id)
    }

    /// Every chat, the inbox's first and then the archive's, each in its view's order: ONE query
    /// of both views (`archived: None`, a snapshot) split by the row's flag. Two queries, the
    /// inbox's and then the archive's, missed a chat that moved between the views between them
    /// (an archive's echo ahead of a pin's: `roaming::with_the_flag_on_the_changes_go_through`,
    /// strike 1, 2026-10-29).
    pub async fn conversations(&self) -> Vec<ConversationRow> {
        let q = ConversationListQuery {
            archived: None,
            limit: 500,
            ..Default::default()
        };
        let rows = match self
            .handle
            .query(Query::Conversations(q))
            .await
            .expect("list")
        {
            QueryData::Conversations(p) => p.rows,
            other => panic!("unexpected {other:?}"),
        };
        let (mut rows, archived): (Vec<_>, Vec<_>) = rows.into_iter().partition(|r| !r.archived);
        rows.extend(archived);
        rows
    }

    pub async fn timeline(&self, conv: ConversationId) -> Vec<MessageRow> {
        let q = TimelineQuery {
            conv,
            anchor: TimelineAnchor::Newest,
            limit: 500,
        };
        match self
            .handle
            .query(Query::Timeline(q))
            .await
            .expect("timeline")
        {
            QueryData::Timeline(p) => p.rows,
            other => panic!("unexpected {other:?}"),
        }
    }

    /// Waits until the account's script ran to its end and everything was acked.
    pub async fn wait_finished(&self, account: AccountId) {
        let row = self.account(account).await.expect("account");
        let control = self.control.clone();
        let uuid = row.uuid;
        tokio::time::timeout(WAIT, control.wait_for(|c| c.is_finished(uuid)))
            .await
            .expect("script finished");
    }

    /// A read-only connection to core.db (for assertions on tables the facade does not show).
    pub fn peek<T>(&self, f: impl FnOnce(&kourier_db::rusqlite::Connection) -> T) -> T {
        let conn = kourier_db::open_read_only(&self.core_db(), &kourier_db::DbKey::from(CORE_KEY))
            .expect("open core.db");
        f(&conn)
    }

    pub fn account_dir(&self, uuid: kourier_model::AccountUuid) -> PathBuf {
        self.data().join("accounts").join(uuid.to_string())
    }

    pub fn secret_exists(&self, uuid: kourier_model::AccountUuid) -> bool {
        self.data().join("secrets").join(uuid.to_string()).exists()
    }

    pub fn backend_db_exists(&self, uuid: kourier_model::AccountUuid) -> bool {
        self.vault.backend_db(uuid).exists()
    }
}

pub async fn start_core(
    dir: &Path,
    fake: &FakeNetwork,
    vault: &Arc<DirVault>,
    clock: &FakeClock,
) -> Core {
    start_core_with(dir, fake, vault, Arc::new(clock.clone())).await
}

/// [`start_core`] on any clock (tests that model what a fake clock cannot, like timers that
/// do not run during a suspend).
pub async fn start_core_with(
    dir: &Path,
    fake: &FakeNetwork,
    vault: &Arc<DirVault>,
    clock: Arc<dyn kourier_backend_api::Clock>,
) -> Core {
    let networks = NetworkRegistry::new()
        .with(Arc::new(fake.clone()))
        .expect("registry");
    start_core_on(dir, networks, vault, clock).await
}

/// [`start_core_with`] on any set of networks (tests that wrap the fake in a backend of their
/// own).
pub async fn start_core_on(
    dir: &Path,
    networks: NetworkRegistry,
    vault: &Arc<DirVault>,
    clock: Arc<dyn kourier_backend_api::Clock>,
) -> Core {
    let timeouts = Timeouts {
        shutdown_grace: Duration::from_secs(5),
        logout: Duration::from_secs(5),
        ..Timeouts::default()
    };
    start_core_timed(dir, networks, vault, clock, timeouts).await
}

/// [`start_core_on`] with the lifecycle timeouts of the test's choosing (a stop grace short
/// enough to measure).
pub async fn start_core_timed(
    dir: &Path,
    networks: NetworkRegistry,
    vault: &Arc<DirVault>,
    clock: Arc<dyn kourier_backend_api::Clock>,
    timeouts: Timeouts,
) -> Core {
    std::fs::create_dir_all(dir.join("data")).expect("data dir");
    let mut cfg = CoreConfig::new(
        dir.join("data").join("core.db"),
        kourier_db::DbKey::from(CORE_KEY),
        networks,
        vault.clone(),
        clock,
        tokio::runtime::Handle::current(),
    );
    cfg.restart = RestartPolicy::default();
    cfg.media_cache = Some(dir.join("run").join("media"));
    cfg.timeouts = timeouts;
    Core::start(cfg).await.expect("core starts")
}

/// Adds an account (returns once the core created it; the login runs on).
pub async fn add_account(env: &mut Env) -> AccountId {
    env.handle
        .send(kourier_core::facade::UiCommand::AddAccount {
            network: kourier_model::NetworkKind::Fake,
            flow: "qr".into(),
        })
        .expect("send");
    env.event("AccountAdded", |e| match e {
        UiEvent::AccountAdded { account } => Some(*account),
        _ => None,
    })
    .await
}

/// Adds an account and waits until it is `Active`.
pub async fn add_linked(env: &mut Env) -> AccountId {
    add_linked_as(env, kourier_model::NetworkKind::Fake).await
}

/// [`add_linked`] for an account of the network `kind` (the one a fake plays, [`Env::playing`]).
pub async fn add_linked_as(env: &mut Env, kind: kourier_model::NetworkKind) -> AccountId {
    env.handle
        .send(kourier_core::facade::UiCommand::AddAccount {
            network: kind,
            flow: "qr".into(),
        })
        .expect("send");
    let account = env
        .event("AccountAdded", |e| match e {
            UiEvent::AccountAdded { account } => Some(*account),
            _ => None,
        })
        .await;
    let h = env.handle.clone();
    eventually("account active", WAIT, || {
        let h = h.clone();
        async move {
            matches!(h.query(Query::Accounts).await, Ok(QueryData::Accounts(a))
                if a.iter().any(|r| r.id == account && r.state == kourier_model::AccountState::Active))
        }
    })
    .await;
    account
}

/// The name libtest knows a test by in the merged binary `tests/it.rs`: the kill and crash
/// tests start this binary again with `--exact <name>`. `module` is `module_path!()` of the
/// calling file (`it::crash`), `test` the function (`crash_child`); the result is
/// `crash::crash_child`. The crate's own name is not part of a libtest name.
pub fn child_filter(module: &str, test: &str) -> String {
    match module.split_once("::") {
        Some((_crate, path)) => format!("{path}::{test}"),
        None => test.to_owned(),
    }
}

/// Set in the child of [`in_own_process!`].
const OWN_PROCESS: &str = "KOURIER_OWN_PROCESS";

/// The libtest name of the test inside which an item was declared, from the item's
/// `type_name`: `it::robustness::a_test::{{closure}}::here` (the body of a tokio test is a
/// closure) is `robustness::a_test`. The crate (`it`, the merged binary) and the item itself
/// are not part of a libtest name.
pub fn test_name_of(item_path: &str) -> String {
    let mut parts: Vec<&str> = item_path.split("::").collect();
    parts.pop();
    if !parts.is_empty() {
        parts.remove(0);
    }
    parts.retain(|part| *part != "{{closure}}");
    parts.join("::")
}

/// The libtest name of the test that calls this (`robustness::a_test`), found from the path of
/// an item declared in its body: no string to keep in step with the function's name.
macro_rules! test_name {
    () => {{
        fn here() {}
        $crate::support::test_name_of(std::any::type_name_of_val(&here))
    }};
}
pub(crate) use test_name;

/// For a test that reads a process-wide log or needs anything else a second test of the
/// process would disturb; the first statement of the test:
/// `if support::in_own_process!() { return; }`. Under nextest every test is a process of its
/// own (`NEXTEST` is set) and this is `false`: run the body. Under `cargo test` the tests of this
/// binary are threads of ONE process (and the first global tracing subscriber wins): the call
/// starts the binary again for this test alone ([`run_in_child`]), is `true` once that passed
/// (the caller returns) and panics with the child's output when it failed; in the child it is
/// `false`. The name of the test comes from `test_name!`, so renaming the function cannot
/// leave the child running another (or no) test.
macro_rules! in_own_process {
    () => {
        $crate::support::in_own_process_named(&$crate::support::test_name!())
    };
}
pub(crate) use in_own_process;

/// What [`in_own_process!`] does with the name it found.
pub fn in_own_process_named(name: &str) -> bool {
    if std::env::var_os("NEXTEST").is_some() || std::env::var_os(OWN_PROCESS).is_some() {
        return false;
    }
    if let Err(why) = run_in_child(name) {
        panic!("{name} failed in a process of its own: {why}");
    }
    true
}

/// Starts this binary again for the one test `name` (`--exact`); the reason when it did not
/// pass exactly that test.
pub fn run_in_child(name: &str) -> Result<(), String> {
    let out = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .arg("--exact")
        .arg(name)
        .args(["--nocapture", "--test-threads=1"])
        .env(OWN_PROCESS, "1")
        .output()
        .expect("spawn the test again");
    let stdout = String::from_utf8_lossy(&out.stdout);
    child_verdict(out.status.success(), &stdout).map_err(|why| {
        format!(
            "{why} ({:?}):\n{stdout}{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

/// What the child's exit status and output say. A name that matches no test makes libtest exit
/// 0 after "running 0 tests"; that is no pass here (T3 review F3).
fn child_verdict(success: bool, stdout: &str) -> Result<(), &'static str> {
    if !success {
        return Err("the child failed");
    }
    if !stdout.contains("test result: ok. 1 passed;") {
        return Err("the child did not pass exactly one test (a name that matches no test?)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{child_filter, child_verdict, run_in_child, test_name_of};

    #[test]
    fn a_child_is_named_with_its_module() {
        assert_eq!(
            child_filter("it::crash", "crash_child"),
            "crash::crash_child"
        );
        assert_eq!(child_filter("it", "x"), "x");
    }

    #[test]
    fn a_test_finds_its_own_name() {
        assert_eq!(
            super::test_name!(),
            "support::tests::a_test_finds_its_own_name"
        );
        // A tokio test's body is a closure; a test of the binary's root has no module.
        assert_eq!(
            test_name_of("it::robustness::a_test::{{closure}}::here"),
            "robustness::a_test"
        );
        assert_eq!(test_name_of("it::a_root_test::here"), "a_root_test");
    }

    #[test]
    fn a_child_that_ran_no_test_is_not_a_pass() {
        // libtest exits 0 after "running 0 tests" for a name that matches nothing.
        let why = run_in_child("support::tests::there_is_no_such_test").expect_err("no pass");
        assert!(why.contains("exactly one test"), "{why}");
        let why = run_in_child("there_is_no_such_test").expect_err("no pass");
        assert!(why.contains("exactly one test"), "{why}");
    }

    #[test]
    fn a_child_that_passed_one_test_is_a_pass() {
        run_in_child("support::tests::a_test_that_passes").expect("a pass");
    }

    #[test]
    fn a_test_that_passes() {}

    #[test]
    fn the_verdict_wants_exactly_one_passed_test() {
        let one = "running 1 test\ntest x ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored";
        let none = "running 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored";
        assert_eq!(child_verdict(true, one), Ok(()));
        assert!(child_verdict(true, none).is_err());
        assert!(child_verdict(false, one).is_err());
        assert!(child_verdict(true, "").is_err());
    }
}
