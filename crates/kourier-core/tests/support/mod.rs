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
        let dir = tempfile::tempdir().expect("tempdir");
        let clock = FakeClock::new(u64::try_from(kourier_fake::scripts::T0).unwrap_or(0));
        let fake = FakeNetwork::new(flavor, script);
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

    pub async fn conversations(&self) -> Vec<ConversationRow> {
        let q = ConversationListQuery {
            limit: 500,
            ..Default::default()
        };
        let mut rows = match self
            .handle
            .query(Query::Conversations(q.clone()))
            .await
            .expect("list")
        {
            QueryData::Conversations(p) => p.rows,
            other => panic!("unexpected {other:?}"),
        };
        let archived = ConversationListQuery {
            archived: true,
            ..q
        };
        if let QueryData::Conversations(p) = self
            .handle
            .query(Query::Conversations(archived))
            .await
            .expect("list")
        {
            rows.extend(p.rows);
        }
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
    cfg.timeouts = Timeouts {
        shutdown_grace: Duration::from_secs(5),
        logout: Duration::from_secs(5),
        ..Timeouts::default()
    };
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
    env.handle
        .send(kourier_core::facade::UiCommand::AddAccount {
            network: kourier_model::NetworkKind::Fake,
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
