//! Starting and running the core: opening `core.db`, reconciling accounts with the vault
//! (§4.3 steps 3 and 8), one supervisor per account, and the task that serves [`UiCommand`]s.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kourier_backend_api::{BackendConfig, Clock, Command, Network, NetworkRegistry, StartMode};
use kourier_db::rusqlite::OptionalExtension;
use kourier_db::{Priority, failpoint};
use kourier_model::{
    AccountId, AccountState, AccountUuid, ConnState, ConvKey, ConversationId, MessageId,
    NetworkKind, Text,
};
use tokio::sync::{Semaphore, broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::attachments::playback::Playback;
use crate::attachments::{self, derive, import};
use crate::dispatch::Signal;
use crate::error::{CoreError, Refusal, Result};
use crate::expiry::ExpiryPolicy;
use crate::facade::{
    ActionSubject, CoreHandle, Internal, Progress, Query, QueryData, RefusedFiles, RequestId,
    UI_EVENT_CAPACITY, UiCommand, UiEvent,
};
use crate::ingest::{Changes, NotifyCandidate};
use crate::media::{MediaEntry, MediaHandles};
use crate::store::{GLOBAL_LANE, Store, lane};
use crate::supervisor::{
    Control, FAILPOINT_AFTER_PENDING_COMMAND, Hint, Plan, RestartPolicy, Supervisor, Timeouts,
};
use crate::vault::{AccountVault, VaultError};
use crate::{calls, expiry, local, modify, outbox, query};

/// Everything the core needs from the composition root.
pub struct CoreConfig {
    /// `core.db`.
    pub core_db: PathBuf,
    /// Its key (HKDF of the master key, §5.2).
    pub core_key: kourier_db::DbKey,
    /// The compiled-in networks.
    pub networks: NetworkRegistry,
    /// Per-account keys and directories.
    pub vault: Arc<dyn AccountVault>,
    /// Time source of every timer (§9.6).
    pub clock: Arc<dyn Clock>,
    /// The runtime for backends' network I/O.
    pub net_rt: tokio::runtime::Handle,
    /// Backend configuration (environment, proxy, …).
    pub backend: BackendConfig,
    /// The app's cancellation token; account tokens are its children.
    pub cancel: CancellationToken,
    /// Restart policy for crashed backends.
    pub restart: RestartPolicy,
    /// Lifecycle timeouts.
    pub timeouts: Timeouts,
    /// The plaintext media cache for playback files (`$XDG_RUNTIME_DIR/kourier/media`, tmpfs,
    /// §5.8). The core uses only its `attachments/` subdirectory, which it wipes at start and
    /// exit; other caches beside it (the app's decrypted avatars) are left alone. `None`:
    /// `OpenMedia` is refused.
    pub media_cache: Option<PathBuf>,
}

impl CoreConfig {
    /// A configuration with the default policy and timeouts.
    pub fn new(
        core_db: PathBuf,
        core_key: kourier_db::DbKey,
        networks: NetworkRegistry,
        vault: Arc<dyn AccountVault>,
        clock: Arc<dyn Clock>,
        net_rt: tokio::runtime::Handle,
    ) -> Self {
        Self {
            core_db,
            core_key,
            networks,
            vault,
            clock,
            net_rt,
            backend: BackendConfig::default(),
            cancel: CancellationToken::new(),
            restart: RestartPolicy::default(),
            timeouts: Timeouts::default(),
            media_cache: None,
        }
    }
}

impl std::fmt::Debug for CoreConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreConfig")
            .field("networks", &self.networks)
            .field("restart", &self.restart)
            .field("timeouts", &self.timeouts)
            .field("media_cache", &self.media_cache.is_some())
            .finish_non_exhaustive()
    }
}

/// What the UI shows (notification focus rule, backend presence).
#[derive(Clone, Debug, Default)]
pub(crate) struct Presence {
    pub focused: bool,
    pub visible: Option<(AccountId, ConversationId, ConvKey)>,
}

/// State shared by the core task and the supervisors.
pub(crate) struct Shared {
    pub store: Store,
    pub networks: NetworkRegistry,
    pub vault: Arc<dyn AccountVault>,
    pub clock: Arc<dyn Clock>,
    pub events: broadcast::Sender<UiEvent>,
    pub net_rt: tokio::runtime::Handle,
    pub backend: BackendConfig,
    pub cancel: CancellationToken,
    pub restart: RestartPolicy,
    pub timeouts: Timeouts,
    pub conn_states: Mutex<HashMap<AccountId, ConnState>>,
    pub presence: watch::Sender<Presence>,
    pub ui_ready: watch::Sender<bool>,
    /// The media handle table (§8.6).
    pub media: MediaHandles,
    /// Plaintext playback files (§5.8).
    pub playback: Playback,
    /// At most this many thumbnail jobs decode at the same time.
    pub derive_permits: Arc<Semaphore>,
    /// Wakes the disappearing-message sweeper ([`expiry`]) after a write that started a timer.
    pub expiry_wake: tokio::sync::Notify,
    /// Wakes the deadline task of ringing call lines ([`calls`]) after a write that stored
    /// one, and after a resume.
    pub calls_wake: tokio::sync::Notify,
    /// Who may be told of a missed call at once, and the missed calls that wait for a
    /// catch-up summary ([`calls::Watch`]).
    pub calls_watch: Mutex<calls::Watch>,
    /// Accounts whose backend found the local clock wrong (§9.6) and has not been connected
    /// since: the sweeper does not delete their messages. Mirrors `settings` rows, so it holds
    /// across a restart, before any backend reported anything.
    pub clock_wrong: Mutex<std::collections::BTreeSet<AccountId>>,
    /// Accounts that were linked when the core started and whose backend has not said
    /// anything about the clock since (§9.6): the sweeper does not delete their messages yet.
    /// A clock that is ahead makes every timer look run out at the first look, and no hold is
    /// stored for a clock that went wrong while Kourier was not running. An account leaves the
    /// set at its backend's first verdict (connected, or `FixClock`), or when no backend runs
    /// for it any more (logged out, halted).
    pub clock_unverified: Mutex<std::collections::BTreeSet<AccountId>>,
    /// What happens to a due message of an account in one of the two sets above
    /// ([`ExpiryPolicy`]). Mirrors a `settings` row; changed by `UiCommand::SetExpiryPolicy`,
    /// only on the writer thread and only behind the commit that stored the new value: the
    /// sweeper reads it there too, so it never erases by an option that is not stored.
    pub expiry_policy: Mutex<ExpiryPolicy>,
    /// The core is shutting down. The supervisors that end now leave the two sets as they
    /// are, and the sweeper does nothing more: an account whose clock nobody confirmed in
    /// this run must not be swept because its backend was told to stop.
    pub shutting_down: std::sync::atomic::AtomicBool,
    /// The control channel of every account's supervisor, for writers outside the core task
    /// (the sweeper) that took rows out of an account's outbox.
    pub controls: Mutex<HashMap<AccountId, mpsc::UnboundedSender<Control>>>,
}

/// Thumbnail jobs that decode at the same time.
const DERIVE_PARALLEL: usize = 2;

impl Shared {
    pub(crate) fn emit(&self, e: UiEvent) {
        let _ = self.events.send(e);
    }

    pub(crate) fn now(&self) -> i64 {
        i64::try_from(self.clock.now_ms()).unwrap_or(i64::MAX)
    }

    /// Announces what a committed change wants announced (§8.7), unless the user is looking
    /// at that conversation: the window has the focus and shows it.
    pub(crate) fn notify(&self, account: AccountId, candidates: &[NotifyCandidate]) {
        let presence = self.presence.borrow().clone();
        for n in candidates {
            let focused_here = presence.focused
                && presence
                    .visible
                    .as_ref()
                    .is_some_and(|(_, c, _)| *c == n.conv);
            if !focused_here {
                self.emit(UiEvent::Notify(crate::facade::Notification {
                    account,
                    conv: n.conv,
                    message: n.message,
                    mentions_me: n.mentions_me,
                }));
            }
        }
    }

    /// Whether the core is going down ([`Shared::shutting_down`], or its token was
    /// cancelled).
    pub(crate) fn stopping(&self) -> bool {
        self.shutting_down.load(std::sync::atomic::Ordering::SeqCst) || self.cancel.is_cancelled()
    }

    /// The user's choice for due messages on an unconfirmed clock.
    pub(crate) fn expiry_policy(&self) -> ExpiryPolicy {
        *self
            .expiry_policy
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The accounts whose backend found the local clock wrong, or has not said anything about
    /// it since the core started (§9.6): no backend has confirmed their clock.
    pub(crate) fn clock_unconfirmed(&self) -> Vec<AccountId> {
        let mut held = self
            .clock_wrong
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        held.extend(
            self.clock_unverified
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .copied(),
        );
        held.into_iter().collect()
    }

    /// What the disappearing sweeper works from right now.
    pub(crate) fn expiry_hold(&self) -> expiry::Hold {
        expiry::Hold {
            policy: self.expiry_policy(),
            unconfirmed: self.clock_unconfirmed(),
        }
    }

    /// Makes `account`'s dispatcher look at its outbox again: rows went from it (a message
    /// expired, a clear), and what waited behind them is at the head of its conversation now.
    /// Without it they would start only when the timer armed for a row that is gone fires
    /// (up to the longest backoff, 15 minutes).
    pub(crate) fn kick_outbox(&self, account: AccountId) {
        let controls = self.controls.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(tx) = controls.get(&account) {
            let _ = tx.send(Control::Outbox(Signal::Kick));
        }
    }

    /// Forgets the media handles of attachments that changed or went, and the playback files
    /// of those that went (a view playing a file keeps its open descriptor).
    pub(crate) fn forget_media(&self, ch: &Changes) {
        if !ch.media.is_empty() {
            self.media.forget(ch.media.iter().copied());
        }
        if !ch.media_gone.is_empty() {
            self.playback.forget(ch.media_gone.iter().copied());
        }
    }

    /// Publishes a core-local write of `account` (outside a supervisor): invalidations, media
    /// handles, unlinking dropped blobs and deriving downloaded files. Downloads it requested
    /// are the caller's to hand to the supervisor.
    pub(crate) async fn publish_local(
        self: &Arc<Self>,
        account: AccountId,
        blob_dir: &Path,
        ch: Changes,
    ) {
        emit_changes(self, &ch);
        if !ch.unlink.is_empty() {
            let (dir, paths) = (blob_dir.to_owned(), ch.unlink.clone());
            let _ = tokio::task::spawn_blocking(move || attachments::unlink(&dir, &paths)).await;
        }
        if !ch.derive.is_empty() {
            let scope = self.scope_of(account).await;
            derive::spawn(self, account, scope, blob_dir.to_owned(), ch.derive);
        }
    }

    /// The message-id scope of `account`'s network (Signal-like when unknown).
    async fn scope_of(&self, account: AccountId) -> kourier_model::MsgIdScope {
        let kind = self
            .store
            .read(move |c| {
                Ok(local::accounts(c)?
                    .into_iter()
                    .find(|a| a.id == account)
                    .map(|a| a.network))
            })
            .await
            .ok()
            .flatten();
        kind.and_then(|k| self.networks.get(k))
            .map_or(kourier_model::MsgIdScope::AccountAuthor, |n| {
                n.descriptor().msg_id_scope
            })
    }

    /// The blob directory of `account`.
    pub(crate) async fn blob_dir_of(&self, account: AccountId) -> Result<PathBuf> {
        let uuid = self
            .store
            .read(move |c| {
                Ok(local::accounts(c)?
                    .into_iter()
                    .find(|a| a.id == account)
                    .map(|a| a.uuid))
            })
            .await?
            .ok_or(CoreError::UnknownAccount(account))?;
        Ok(self.vault.blob_dir(uuid))
    }

    /// Makes a stored media handle absolute and registers it.
    fn register_media(&self, raw: query::RawMedia) -> MediaEntry {
        let dir = self.vault.blob_dir(raw.uuid);
        let mut entry = raw.entry;
        for blob in [&mut entry.file, &mut entry.thumb].into_iter().flatten() {
            blob.path = dir.join(&blob.path);
        }
        self.media.insert(entry.clone());
        entry
    }
}

/// The running core. Dropping it without [`Core::shutdown`] cancels the account tokens.
pub struct Core {
    handle: CoreHandle,
    task: Option<JoinHandle<()>>,
    /// The disappearing-message sweeper.
    sweeper: Option<JoinHandle<()>>,
    /// The deadline task of ringing call lines.
    calls: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Core {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Core").finish_non_exhaustive()
    }
}

struct Slot {
    control: mpsc::UnboundedSender<Control>,
    task: JoinHandle<()>,
}

impl Core {
    /// Opens and migrates `core.db`, reconciles the accounts with the vault and starts one
    /// supervisor per account. Call inside a tokio runtime.
    ///
    /// Reconciliation (§4.3): accounts still `Linking` are discarded (a login never finishes
    /// across a restart), `Removing` ones are finished, `LoggedOut` ones are wiped again (a relink
    /// may have been half done), `Active` ones resume. Every `key.wrap` without a row — all of
    /// them when `core.db` was deleted — is recreated as an `Active` account started with
    /// `StartMode::Rebuild` when its secret still opens it, and removed when it does not.
    pub async fn start(cfg: CoreConfig) -> Result<Self> {
        let (path, key) = (cfg.core_db.clone(), cfg.core_key.clone());
        let media_dir = cfg.media_cache.clone();
        let (store, fresh, playback) = tokio::task::spawn_blocking(move || {
            let (store, fresh) = Store::open(&path, &key)?;
            Ok::<_, CoreError>((store, fresh, Playback::new(media_dir.as_deref())))
        })
        .await
        .map_err(|_| CoreError::Stopped)??;
        if fresh {
            tracing::info!("core.db created");
        }
        // What a kill left of a large send that was still being stored goes before anything
        // reads the rows (and before the blob sweep, which then removes its files).
        let dropped = store
            .write(Priority::High, GLOBAL_LANE, outbox::discard_all_staged)
            .await?;
        if dropped > 0 {
            tracing::info!(messages = dropped, "removed an unfinished send of files");
        }
        let clock_wrong = store.read(local::clock_wrong_accounts).await?;
        // Read before the first sweep of this start, with no UI anywhere near (§9.6).
        let expiry_policy = store.read(expiry::stored_policy).await?;
        let (events, _) = broadcast::channel(UI_EVENT_CAPACITY);
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            store,
            networks: cfg.networks,
            vault: cfg.vault,
            clock: cfg.clock,
            events: events.clone(),
            net_rt: cfg.net_rt,
            backend: cfg.backend,
            cancel: cfg.cancel,
            restart: cfg.restart,
            timeouts: cfg.timeouts,
            conn_states: Mutex::new(HashMap::new()),
            presence: watch::channel(Presence::default()).0,
            ui_ready: watch::channel(false).0,
            media: MediaHandles::new(tx.clone()),
            playback,
            derive_permits: Arc::new(Semaphore::new(DERIVE_PARALLEL)),
            expiry_wake: tokio::sync::Notify::new(),
            calls_wake: tokio::sync::Notify::new(),
            calls_watch: Mutex::new(calls::Watch::default()),
            clock_wrong: Mutex::new(clock_wrong),
            clock_unverified: Mutex::new(std::collections::BTreeSet::new()),
            expiry_policy: Mutex::new(expiry_policy),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            controls: Mutex::new(HashMap::new()),
        });
        let plans = reconcile(&shared).await?;
        // Before any supervisor or the sweeper runs: the accounts whose backend starts now
        // and can still object to the clock.
        shared
            .clock_unverified
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(plans.iter().filter_map(|(id, _, network, plan)| {
                (matches!(plan, Plan::Run(_)) && shared.networks.get(*network).is_some())
                    .then_some(*id)
            }));
        // And before any query is served: what came due while Kourier was not running is
        // hidden from the first read on, where the user chose that.
        let (hold, now) = (shared.expiry_hold(), shared.now());
        shared
            .store
            .write(Priority::High, GLOBAL_LANE, move |c| {
                expiry::settle_at_start(c, now, &hold)
            })
            .await?;
        for (id, uuid, network, _) in &plans {
            startup_files(&shared, *id, *uuid, *network).await;
        }
        let handle = CoreHandle {
            commands: tx,
            events,
            media: shared.media.clone(),
        };
        let mut slots = HashMap::new();
        for (id, uuid, network, plan) in plans {
            match shared.networks.get(network) {
                Some(net) => {
                    slots.insert(id, spawn_supervisor(&shared, id, uuid, net.clone(), plan));
                }
                None => {
                    tracing::warn!(account = id.0, %network, "network not in this build; account not started");
                }
            }
        }
        let task = tokio::spawn(run_core(shared.clone(), rx, slots));
        let sweeper = tokio::spawn(expiry::run(shared.clone()));
        let calls = tokio::spawn(calls::run(shared.clone()));
        Ok(Self {
            handle,
            task: Some(task),
            sweeper: Some(sweeper),
            calls: Some(calls),
            shared,
        })
    }

    /// A handle for commands, events and queries.
    pub fn handle(&self) -> CoreHandle {
        self.handle.clone()
    }

    /// Stops every account actor (`Shutdown`, grace, cancel), finishes queued writes and closes
    /// `core.db`.
    pub async fn shutdown(mut self) {
        let (tx, rx) = oneshot::channel();
        if self.handle.commands.send(Internal::Shutdown(tx)).is_ok() {
            let _ = rx.await;
        }
        if let Some(t) = self.task.take() {
            let _ = t.await;
        }
        for t in [self.sweeper.take(), self.calls.take()]
            .into_iter()
            .flatten()
        {
            t.abort();
            let _ = t.await;
        }
        let shared = self.shared.clone();
        let _ = tokio::task::spawn_blocking(move || shared.store.close()).await;
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        if self.task.is_some() {
            self.shared.cancel.cancel();
        }
        for t in [self.sweeper.take(), self.calls.take()]
            .into_iter()
            .flatten()
        {
            t.abort();
        }
    }
}

fn spawn_supervisor(
    shared: &Arc<Shared>,
    id: AccountId,
    uuid: AccountUuid,
    network: Arc<dyn kourier_backend_api::Network>,
    plan: Plan,
) -> Slot {
    let (tx, rx) = mpsc::unbounded_channel();
    shared
        .controls
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(id, tx.clone());
    let sup = Supervisor::new(shared.clone(), id, uuid, network, rx);
    Slot {
        control: tx,
        task: tokio::spawn(sup.run(plan)),
    }
}

/// Startup reconciliation of account rows and `key.wrap` files.
async fn reconcile(
    sh: &Arc<Shared>,
) -> Result<Vec<(AccountId, AccountUuid, kourier_model::NetworkKind, Plan)>> {
    let rows = sh.store.read(local::accounts).await?;
    let mut plans = Vec::new();
    let mut known = HashSet::new();
    for row in rows {
        known.insert(row.uuid);
        match row.state {
            AccountState::Linking | AccountState::Removing => {
                tracing::info!(
                    account = row.id.0,
                    "discarding an unfinished link or removal"
                );
                if let Err(e) = sh.vault.shred(row.uuid).await {
                    if row.state == AccountState::Removing {
                        // The shred is what removal promises (§5.2): keep the row and try again
                        // at the next start rather than drop the account with its secret alive.
                        tracing::warn!(account = row.id.0, error = %e, "shredding a removed account failed; retried at the next start");
                        continue;
                    }
                    tracing::debug!(error = %e, "shred during reconciliation");
                }
                if let Err(e) = sh.vault.remove_dir(row.uuid).await {
                    tracing::warn!(account = row.id.0, error = %e, "removing the account directory failed; retried at the next start");
                    continue;
                }
                let id = row.id;
                sh.store
                    .write(Priority::High, GLOBAL_LANE, move |c| {
                        local::delete_account(c, id)
                    })
                    .await?;
            }
            AccountState::LoggedOut { .. } => {
                if let Err(e) = sh.vault.wipe_credentials(row.uuid).await {
                    tracing::warn!(account = row.id.0, error = %e, "wiping logged-out credentials failed");
                }
                plans.push((row.id, row.uuid, row.network, Plan::Idle));
            }
            AccountState::Active => {
                plans.push((row.id, row.uuid, row.network, Plan::Run(StartMode::Resume)))
            }
        }
    }
    for entry in sh.vault.list().await? {
        if known.contains(&entry.uuid) {
            continue;
        }
        match sh.vault.unwrap(entry.uuid).await {
            Ok(_) => {
                if sh.networks.get(entry.network).is_none() {
                    tracing::warn!(network = %entry.network, "key.wrap of a network not in this build; left alone");
                    continue;
                }
                tracing::info!(uuid = %entry.uuid.short(), "rebuilding the account from key.wrap");
                let (uuid, network, now) = (entry.uuid, entry.network, sh.now());
                let id = sh
                    .store
                    .write(Priority::High, GLOBAL_LANE, move |c| {
                        local::insert_account(c, uuid, network, &AccountState::Active, now)
                    })
                    .await?;
                plans.push((id, uuid, network, Plan::Run(StartMode::Rebuild)));
            }
            Err(VaultError::SecretMissing | VaultError::Missing) => {
                tracing::info!(uuid = %entry.uuid.short(), "removing a shredded account directory");
                sh.vault.remove_dir(entry.uuid).await?;
            }
            Err(VaultError::WrongKey) => {
                // Not shredded: the secret is there and this profile's key does not open the
                // wrap (a master key that is not the one the account was made with). The
                // directory holds the account's only keys, so it stays as it is.
                tracing::error!(
                    uuid = %entry.uuid.short(),
                    "an account directory does not open with this profile's key; left untouched (restore the profile's key, or remove the directory by hand)"
                );
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(plans)
}

/// A UI command whose effects must follow the order the UI sent it in (compose commands).
type Ordered = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Runs [`Ordered`] jobs one after another. Compose commands are routed this way into the
/// lane of their conversation ([`Lanes`]), so two `SendText` for one conversation are stored,
/// and get their ids, in the order they were typed (one FIFO per conversation, §4.8 step 2),
/// and a `SaveDraft` sent right before a `SendText` never lands after it (which would
/// resurrect the sent text as a draft).
async fn run_ordered(mut rx: mpsc::UnboundedReceiver<Ordered>) {
    while let Some(job) = rx.recv().await {
        job.await;
    }
}

/// The compose queues: one FIFO per conversation, so what the user does in one chat keeps
/// its order (a text typed behind a send of files is stored behind it, a draft saved before
/// a send never lands after it) while a long job of one chat (the import of thousands of
/// files) delays nothing in the others. A lane exists while it has work.
#[derive(Clone, Default)]
struct Lanes {
    /// Conversation → its queue (`None`: commands whose conversation could not be found).
    /// Jobs are added, and a lane is ended, only under this lock.
    lanes: Arc<Mutex<HashMap<Option<ConversationId>, mpsc::UnboundedSender<Ordered>>>>,
}

impl Lanes {
    /// Queues `job` behind the jobs of `conv` that came before it.
    fn push(&self, conv: Option<ConversationId>, job: Ordered) {
        let mut lanes = self.lanes.lock().unwrap_or_else(PoisonError::into_inner);
        let job = match lanes.get(&conv) {
            Some(tx) => match tx.send(job) {
                Ok(()) => return,
                // Its worker died with a job that panicked: a new lane takes over.
                Err(back) => back.0,
            },
            None => job,
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(job);
        lanes.insert(conv, tx);
        tokio::spawn(self.clone().run(conv, rx));
    }

    /// The next job of the lane of `conv`; `None` ends the lane (nothing waits).
    fn next(
        &self,
        conv: Option<ConversationId>,
        rx: &mut mpsc::UnboundedReceiver<Ordered>,
    ) -> Option<Ordered> {
        if let Ok(job) = rx.try_recv() {
            return Some(job);
        }
        let mut lanes = self.lanes.lock().unwrap_or_else(PoisonError::into_inner);
        // Looked at again under the lock: a job added before it is not lost.
        match rx.try_recv() {
            Ok(job) => Some(job),
            Err(_) => {
                lanes.remove(&conv);
                None
            }
        }
    }

    async fn run(self, conv: Option<ConversationId>, mut rx: mpsc::UnboundedReceiver<Ordered>) {
        while let Some(job) = self.next(conv, &mut rx) {
            job.await;
        }
    }

    /// How many conversations have compose work right now.
    #[cfg(test)]
    fn busy(&self) -> usize {
        self.lanes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

/// What a compose command is about: the lane it runs in is that of its conversation.
#[derive(Clone, Copy, Debug)]
enum Subject {
    Conversation(ConversationId),
    Message(MessageId),
    Attachment(kourier_model::AttachmentId),
}

/// The core task: serves commands and queries.
async fn run_core(
    sh: Arc<Shared>,
    mut rx: mpsc::UnboundedReceiver<Internal>,
    mut slots: HashMap<AccountId, Slot>,
) {
    // Compose commands pass through one short queue that only finds their conversation, in
    // the order the UI sent them, and then wait in that conversation's lane.
    let (ordered, ordered_rx) = mpsc::unbounded_channel();
    tokio::spawn(run_ordered(ordered_rx));
    // Opens and closes of playback files keep their order too (a close never overtakes the open
    // it answers), on their own queue so a decryption never delays a send.
    let (media_q, media_rx) = mpsc::unbounded_channel();
    tokio::spawn(run_ordered(media_rx));
    let queues = Queues {
        route: ordered,
        lanes: Lanes::default(),
        media: media_q,
    };
    while let Some(msg) = rx.recv().await {
        slots.retain(|_, s| !s.task.is_finished());
        match msg {
            Internal::Ui(cmd) => ui_command(&sh, &mut slots, &queues, cmd).await,
            Internal::Query(q, reply) => {
                let sh = sh.clone();
                tokio::spawn(async move {
                    let _ = reply.send(run_query(&sh, q).await);
                });
            }
            Internal::Media(id, reply) => {
                let sh = sh.clone();
                tokio::spawn(async move {
                    let found = sh
                        .store
                        .read_media(move |c| query::media_entry(c, id))
                        .await;
                    let _ = reply.send(found.map(|raw| raw.map(|r| sh.register_media(r))));
                });
            }
            Internal::Shutdown(done) => {
                // Before any supervisor is told: none of them releases its account's clock
                // hold on the way out, and the sweeper stops.
                sh.shutting_down
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                let mut waits = Vec::new();
                for (_, slot) in slots.drain() {
                    let (tx, rx) = oneshot::channel();
                    if slot.control.send(Control::Shutdown(tx)).is_ok() {
                        waits.push((rx, slot.task));
                    }
                }
                for (rx, task) in waits {
                    let _ = rx.await;
                    let _ = task.await;
                }
                let sh2 = sh.clone();
                let _ = tokio::task::spawn_blocking(move || sh2.playback.clear()).await;
                let _ = done.send(());
                return;
            }
        }
    }
}

async fn run_query(sh: &Arc<Shared>, q: Query) -> Result<QueryData> {
    match q {
        Query::Accounts => {
            let states = sh
                .conn_states
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            sh.store
                .read(move |c| query::accounts(c, &states))
                .await
                .map(QueryData::Accounts)
        }
        Query::Conversations(q) => sh
            .store
            .read(move |c| query::conversations(c, &q))
            .await
            .map(QueryData::Conversations),
        Query::Timeline(q) => {
            let now = sh.now();
            let (page, media) = sh
                .store
                .read(move |c| {
                    let page = query::timeline(c, &q, now)?;
                    let media = query::page_media(c, &page)?;
                    Ok((page, media))
                })
                .await?;
            for m in media {
                sh.register_media(m);
            }
            Ok(QueryData::Timeline(page))
        }
        Query::Conversation(id) => sh
            .store
            .read(move |c| query::conversation(c, id))
            .await
            .map(|d| QueryData::Conversation(d.map(Box::new))),
        Query::Attachment(id) => sh
            .store
            .read_media(move |c| query::attachment(c, id))
            .await
            .map(QueryData::Attachment),
        Query::FilePlan(q) => {
            let conv = q.conv;
            let rules = sh
                .store
                .read(move |c| file_rules(c, conv))
                .await?
                .ok_or(CoreError::UnknownConversation(conv))?;
            let paths = q.paths.into_inner();
            // The files are only looked at, off the async threads: 64 bytes of each.
            tokio::task::spawn_blocking(move || {
                let features = rules.features.as_ref();
                let (seen, refused) = import::inspect_all(&paths, &q.as_file, rules.max, features);
                // The plan of what would be sent, in the positions of the selection.
                let sendable: Vec<(usize, &import::Inspected)> = seen
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| !refused.iter().any(|r| r.index == *i))
                    .filter_map(|(i, s)| Some((i, s.as_ref()?)))
                    .collect();
                let of: Vec<_> = sendable.iter().map(|(_, s)| s.plan_file()).collect();
                // Without the caption's text a caption counts as one message.
                let some = Text::plain("-");
                let caption = match &q.caption_text {
                    Some(text) => Some(text),
                    None => q.caption.then_some(&some),
                };
                let mut plan = attachments::plan_files(features, &of, caption, q.reply);
                for position in plan.messages.iter_mut().flatten() {
                    if let Some((index, _)) = sendable.get(*position) {
                        *position = *index;
                    }
                }
                QueryData::FilePlan(crate::facade::FilesPreview {
                    kinds: seen.iter().map(|s| s.as_ref().map(|s| s.content)).collect(),
                    refused,
                    plan,
                })
            })
            .await
            .map_err(|_| CoreError::Stopped)
        }
        Query::Revisions(id) => sh
            .store
            .read(move |c| query::revisions(c, id))
            .await
            .map(QueryData::Revisions),
        Query::ExpiryPolicy => Ok(QueryData::ExpiryPolicy(sh.expiry_policy())),
    }
}

/// What a send of files into a conversation is checked against.
struct FileRules {
    account: AccountId,
    network: NetworkKind,
    /// The account's send limit (`AccountCaps::max_attachment_bytes`).
    max: Option<u64>,
    /// The conversation's features, when the backend reported them.
    features: Option<kourier_model::ConversationFeatures>,
}

/// The [`FileRules`] of `conv`; `None` when it does not exist.
fn file_rules(
    c: &kourier_db::rusqlite::Connection,
    conv: ConversationId,
) -> Result<Option<FileRules>> {
    let Some((account, network)) = outbox::network_of(c, conv)? else {
        return Ok(None);
    };
    let caps: Option<String> = c
        .prepare_cached("SELECT caps FROM accounts WHERE id = ?1")?
        .query_row([account.0], |r| r.get(0))?;
    let caps: Option<kourier_model::AccountCaps> = crate::sql::from_json_opt(caps)?;
    let features: Option<String> = c
        .prepare_cached("SELECT features FROM conversations WHERE id = ?1")?
        .query_row([conv.0], |r| r.get(0))?;
    Ok(Some(FileRules {
        account,
        network,
        max: caps.and_then(|c| c.max_attachment_bytes),
        features: crate::sql::from_json_opt(features)?,
    }))
}

fn control(slots: &HashMap<AccountId, Slot>, account: AccountId, c: Control) -> Result<()> {
    slots
        .get(&account)
        .ok_or(CoreError::UnknownAccount(account))?
        .control
        .send(c)
        .map_err(|_| CoreError::UnknownAccount(account))
}

/// The ordered job queues of the core task.
struct Queues {
    /// Where compose commands (`SendText`, `SendFiles`, drafts, …) are routed to the lane
    /// of their conversation, in the order they came.
    route: mpsc::UnboundedSender<Ordered>,
    /// The compose lanes.
    lanes: Lanes,
    /// `OpenMedia` / `CloseMedia`.
    media: mpsc::UnboundedSender<Ordered>,
}

impl Queues {
    /// Queues a compose command behind the ones about the same conversation that were sent
    /// before it (see [`Lanes`]).
    fn compose(
        &self,
        sh: &Arc<Shared>,
        subject: Subject,
        job: impl Future<Output = ()> + Send + 'static,
    ) {
        let (sh, lanes) = (sh.clone(), self.lanes.clone());
        enqueue(&self.route, async move {
            let conv = match subject {
                Subject::Conversation(conv) => Some(conv),
                // On the third read connection: a long UI query must not hold up the
                // routing of what the user sends.
                Subject::Message(message) => sh
                    .store
                    .read_media(move |c| query::conversation_of(c, message))
                    .await
                    .ok()
                    .flatten(),
                Subject::Attachment(attachment) => sh
                    .store
                    .read_media(move |c| query::conversation_of_attachment(c, attachment))
                    .await
                    .ok()
                    .flatten(),
            };
            lanes.push(conv, Box::pin(job));
        });
    }
}

async fn ui_command(
    sh: &Arc<Shared>,
    slots: &mut HashMap<AccountId, Slot>,
    queues: &Queues,
    cmd: UiCommand,
) {
    let fail = |account: Option<AccountId>, action: &'static str, e: CoreError| {
        action_failed(sh, account, action, e, ActionSubject::None);
    };
    match cmd {
        UiCommand::UiReady => {
            sh.ui_ready.send_replace(true);
        }
        UiCommand::AddAccount { network, flow } => match add_account(sh, network).await {
            Ok((id, uuid, net)) => {
                // Announced before the supervisor runs, so the UI sees the account before its
                // first login step.
                sh.emit(UiEvent::AccountAdded { account: id });
                sh.emit(UiEvent::AccountsChanged);
                let plan = Plan::Login { flow, relink: None };
                slots.insert(id, spawn_supervisor(sh, id, uuid, net, plan));
            }
            Err(e) => fail(None, "AddAccount", e),
        },
        UiCommand::SubmitLoginInput {
            account,
            txn,
            values,
        } => {
            if let Err(e) = control(slots, account, Control::LoginInput { txn, values }) {
                fail(Some(account), "SubmitLoginInput", e);
            }
        }
        UiCommand::CancelLogin { account } => {
            if let Err(e) = control(slots, account, Control::CancelLogin) {
                fail(Some(account), "CancelLogin", e);
            }
        }
        UiCommand::Relink { account, flow } => {
            if let Err(e) = control(slots, account, Control::Relink { flow }) {
                fail(Some(account), "Relink", e);
            }
        }
        UiCommand::RemoveAccount { account } => {
            if let Err(e) = control(slots, account, Control::Remove) {
                fail(Some(account), "RemoveAccount", e);
            }
        }
        UiCommand::RestartAccount { account } => {
            if let Err(e) = control(slots, account, Control::Restart) {
                fail(Some(account), "RestartAccount", e);
            }
        }
        UiCommand::SetPresence { focused, visible } => {
            let resolved = match visible {
                Some(conv) => sh
                    .store
                    .read(move |c| query::conversation_key(c, conv))
                    .await
                    .ok()
                    .flatten()
                    .map(|(a, k)| (a, conv, k)),
                None => None,
            };
            sh.presence.send_replace(Presence {
                focused,
                visible: resolved,
            });
        }
        UiCommand::MarkRead { conv } => {
            let sh2 = sh.clone();
            let senders: HashMap<AccountId, mpsc::UnboundedSender<Control>> =
                slots.iter().map(|(a, s)| (*a, s.control.clone())).collect();
            tokio::spawn(async move {
                if let Err(e) = mark_read(&sh2, &senders, conv).await {
                    let subject = ActionSubject::Conversation(conv);
                    action_failed(&sh2, None, "MarkRead", e, subject);
                }
            });
        }
        UiCommand::ChangeConversation { conv, change } => {
            let sh2 = sh.clone();
            let senders: HashMap<AccountId, mpsc::UnboundedSender<Control>> =
                slots.iter().map(|(a, s)| (*a, s.control.clone())).collect();
            tokio::spawn(async move {
                let res = change_conversation(&sh2, &senders, conv, change).await;
                if let Err((account, e)) = res {
                    let subject = ActionSubject::Conversation(conv);
                    action_failed(&sh2, account, "ChangeConversation", e, subject);
                }
            });
        }
        UiCommand::SendText {
            conv,
            text,
            reply_to,
        } => {
            let (sh2, senders) = (sh.clone(), senders(slots));
            queues.compose(sh, Subject::Conversation(conv), async move {
                if let Err((account, e)) = send_text(&sh2, &senders, conv, text, reply_to).await {
                    let subject = ActionSubject::Conversation(conv);
                    action_failed(&sh2, account, "SendText", e, subject);
                }
            });
        }
        UiCommand::SendFiles {
            conv,
            request,
            paths,
            as_file,
            caption,
            reply_to,
        } => {
            let (sh2, senders) = (sh.clone(), senders(slots));
            queues.compose(sh, Subject::Conversation(conv), async move {
                let files = SendFiles {
                    conv,
                    request,
                    paths: paths.into_inner(),
                    as_file,
                    caption,
                    reply_to,
                };
                if let Err(failed) = send_files(&sh2, &senders, files.clone()).await {
                    // Nothing of it was queued: the whole request goes back to the composer,
                    // with the files that stood in its way named.
                    let subject = ActionSubject::Files(Arc::new(RefusedFiles {
                        conv,
                        request,
                        paths: files.paths.into(),
                        as_file: files.as_file,
                        caption: files.caption,
                        reply_to,
                        refused: failed.refused,
                    }));
                    action_failed(&sh2, failed.account, "SendFiles", failed.error, subject);
                }
            });
        }
        UiCommand::DownloadAttachment { attachment } => {
            let (sh2, senders) = (sh.clone(), senders(slots));
            tokio::spawn(async move {
                if let Err((account, e)) = download_attachment(&sh2, &senders, attachment).await {
                    let subject = ActionSubject::Attachment(attachment);
                    action_failed(&sh2, account, "DownloadAttachment", e, subject);
                }
            });
        }
        UiCommand::OpenMedia { attachment } => {
            let sh2 = sh.clone();
            enqueue(&queues.media, async move {
                match open_media(&sh2, attachment).await {
                    Ok(path) => sh2.emit(UiEvent::MediaOpened { attachment, path }),
                    Err(e) => {
                        let subject = ActionSubject::Attachment(attachment);
                        action_failed(&sh2, None, "OpenMedia", e, subject);
                    }
                }
            });
        }
        UiCommand::CloseMedia { attachment } => {
            let sh2 = sh.clone();
            enqueue(&queues.media, async move {
                let _ = tokio::task::spawn_blocking(move || sh2.playback.close(attachment)).await;
            });
        }
        UiCommand::RetryMessage { message } => {
            let (sh2, senders) = (sh.clone(), senders(slots));
            queues.compose(sh, Subject::Message(message), async move {
                if let Err((account, e)) = retry_message(&sh2, &senders, message).await {
                    let subject = ActionSubject::Message(message);
                    action_failed(&sh2, account, "RetryMessage", e, subject);
                }
            });
        }
        UiCommand::CancelMessage { message } => {
            let (sh2, senders) = (sh.clone(), senders(slots));
            queues.compose(sh, Subject::Message(message), async move {
                if let Err((account, e)) = cancel_message(&sh2, &senders, message).await {
                    let subject = ActionSubject::Message(message);
                    action_failed(&sh2, account, "CancelMessage", e, subject);
                }
            });
        }
        UiCommand::EditMessage { message, text } => {
            let (sh2, senders) = (sh.clone(), senders(slots));
            queues.compose(sh, Subject::Message(message), async move {
                let change = modify::Change::Edit(text);
                if let Err((account, e)) = modify_message(&sh2, &senders, message, change).await {
                    let subject = ActionSubject::Message(message);
                    action_failed(&sh2, account, "EditMessage", e, subject);
                }
            });
        }
        UiCommand::DeleteMessage {
            message,
            for_everyone,
        } => {
            let (sh2, senders) = (sh.clone(), senders(slots));
            queues.compose(sh, Subject::Message(message), async move {
                let change = modify::Change::Delete { for_everyone };
                if let Err((account, e)) = modify_message(&sh2, &senders, message, change).await {
                    let subject = ActionSubject::Message(message);
                    action_failed(&sh2, account, "DeleteMessage", e, subject);
                }
            });
        }
        UiCommand::React {
            message,
            emoji,
            remove,
        } => {
            let (sh2, senders) = (sh.clone(), senders(slots));
            queues.compose(sh, Subject::Message(message), async move {
                let change = modify::Change::React { emoji, remove };
                if let Err((account, e)) = modify_message(&sh2, &senders, message, change).await {
                    let subject = ActionSubject::Message(message);
                    action_failed(&sh2, account, "React", e, subject);
                }
            });
        }
        UiCommand::SetConversationTimer { conv, timer } => {
            let (sh2, senders) = (sh.clone(), senders(slots));
            tokio::spawn(async move {
                if let Err((account, e)) = set_timer(&sh2, &senders, conv, timer).await {
                    let subject = ActionSubject::Conversation(conv);
                    action_failed(&sh2, account, "SetConversationTimer", e, subject);
                }
            });
        }
        UiCommand::ClearConversation { conv } => {
            let (sh2, senders) = (sh.clone(), senders(slots));
            queues.compose(sh, Subject::Conversation(conv), async move {
                if let Err((account, e)) = clear_conversation(&sh2, &senders, conv, false).await {
                    let subject = ActionSubject::Conversation(conv);
                    action_failed(&sh2, account, "ClearConversation", e, subject);
                }
            });
        }
        UiCommand::DeleteConversation { conv } => {
            let (sh2, senders) = (sh.clone(), senders(slots));
            queues.compose(sh, Subject::Conversation(conv), async move {
                if let Err((account, e)) = clear_conversation(&sh2, &senders, conv, true).await {
                    let subject = ActionSubject::Conversation(conv);
                    action_failed(&sh2, account, "DeleteConversation", e, subject);
                }
            });
        }
        UiCommand::DeleteAttachment { attachment } => {
            let (sh2, senders) = (sh.clone(), senders(slots));
            queues.compose(sh, Subject::Attachment(attachment), async move {
                if let Err((account, e)) = delete_attachment(&sh2, &senders, attachment).await {
                    let subject = ActionSubject::Attachment(attachment);
                    action_failed(&sh2, account, "DeleteAttachment", e, subject);
                }
            });
        }
        UiCommand::SaveDraft { conv, draft } => {
            let sh2 = sh.clone();
            queues.compose(sh, Subject::Conversation(conv), async move {
                if let Err((account, e)) = save_draft(&sh2, conv, draft).await {
                    let subject = ActionSubject::Conversation(conv);
                    action_failed(&sh2, account, "SaveDraft", e, subject);
                }
            });
        }
        UiCommand::SetTyping { conv, on } => {
            let (sh2, senders) = (sh.clone(), senders(slots));
            queues.compose(sh, Subject::Conversation(conv), async move {
                if let Err((account, e)) = set_typing(&sh2, &senders, conv, on).await {
                    let subject = ActionSubject::Conversation(conv);
                    action_failed(&sh2, account, "SetTyping", e, subject);
                }
            });
        }
        UiCommand::ConfirmIdentity { account, user } => {
            if let Err(e) = control(slots, account, Control::ConfirmIdentity(user)) {
                fail(Some(account), "ConfirmIdentity", e);
            }
        }
        UiCommand::SubmitChallenge { account, answer } => {
            let (sh2, senders) = (sh.clone(), senders(slots));
            tokio::spawn(async move {
                if let Err(e) = submit_challenge(&sh2, &senders, account, answer).await {
                    action_failed(
                        &sh2,
                        Some(account),
                        "SubmitChallenge",
                        e,
                        ActionSubject::None,
                    );
                }
            });
        }
        UiCommand::SkipHistoryTransfer { account } => {
            // Only a linked account has a transfer; the supervisor drops the command for any
            // other (`Control::Backend`).
            if let Err(e) = control(
                slots,
                account,
                Control::Backend(Command::SkipHistoryTransfer),
            ) {
                fail(Some(account), "SkipHistoryTransfer", e);
            }
        }
        UiCommand::DismissHistoryTransfer { account } => {
            // As a skip: only a running backend has a transfer to forget.
            if let Err(e) = control(
                slots,
                account,
                Control::Backend(Command::DismissHistoryTransfer),
            ) {
                fail(Some(account), "DismissHistoryTransfer", e);
            }
        }
        UiCommand::NetworkChanged => {
            for s in slots.values() {
                let _ = s.control.send(Control::Hint(Hint::NetworkChanged));
            }
        }
        UiCommand::Resumed => {
            // The sweeper's sleep did not run while the machine slept either, and neither
            // did the one towards the deadline of a call that rang.
            sh.expiry_wake.notify_one();
            // A ring that ran out during the suspend is not announced alone: what the phone
            // synced meanwhile comes with the reconnect.
            sh.calls_resumed();
            sh.calls_wake.notify_one();
            for s in slots.values() {
                let _ = s.control.send(Control::Hint(Hint::Resumed));
            }
        }
        // Awaited here, not spawned: a query sent right behind the command reads the new
        // option and rows that already follow it.
        UiCommand::SetExpiryPolicy { policy } => {
            if let Err(e) = set_expiry_policy(sh, policy).await {
                fail(None, "SetExpiryPolicy", e);
            }
        }
        UiCommand::ReleaseMemory => {
            let sh2 = sh.clone();
            tokio::spawn(async move {
                if let Err(e) = sh2.store.release_memory().await {
                    tracing::debug!(error = %e, "releasing memory failed");
                }
            });
        }
        UiCommand::Query { token, query } => {
            let sh2 = sh.clone();
            tokio::spawn(async move {
                let result = run_query(&sh2, query).await.map_err(Arc::new);
                sh2.emit(UiEvent::QueryResult { token, result });
            });
        }
    }
}

/// Queues a compose command behind the ones sent before it (see [`run_ordered`]).
fn enqueue(
    ordered: &mpsc::UnboundedSender<Ordered>,
    job: impl Future<Output = ()> + Send + 'static,
) {
    if ordered.send(Box::pin(job)).is_err() {
        tracing::warn!("the core is stopping; UI command dropped");
    }
}

/// The control senders of every running supervisor (for tasks spawned by UI commands).
fn senders(slots: &HashMap<AccountId, Slot>) -> HashMap<AccountId, mpsc::UnboundedSender<Control>> {
    slots.iter().map(|(a, s)| (*a, s.control.clone())).collect()
}

/// Reports a failed UI command (log-safe) with what it was about.
fn action_failed(
    sh: &Shared,
    account: Option<AccountId>,
    action: &'static str,
    e: CoreError,
    subject: ActionSubject,
) {
    tracing::warn!(?account, action, error = %e, "UI command failed");
    sh.emit(UiEvent::ActionFailed {
        account,
        action,
        error: Arc::new(e),
        subject,
    });
}

/// [`UiCommand::SetExpiryPolicy`]: the option is stored and, in the same transaction, what is
/// due now on an unconfirmed clock is hidden or shown as it says; then the sweeper erases
/// what the option no longer holds.
///
/// The option in memory ([`Shared::expiry_policy`]) changes on the writer thread, right
/// behind the commit that stored it. The sweeper reads it where its own write runs, on that
/// same thread, so no sweep ever follows an option that is not stored: a write that fails
/// (a full disk) leaves the option, and every message, as it was. Set before the write and
/// taken back after a failure, "erase" could delete what is due in the moment between, for
/// good, under an option the user was then told had not changed.
async fn set_expiry_policy(sh: &Arc<Shared>, policy: ExpiryPolicy) -> Result<()> {
    let state = sh.clone();
    let changed = sh
        .store
        .write(Priority::High, GLOBAL_LANE, move |c| {
            // Who is unconfirmed is read where the write runs (a verdict may have come).
            let changed = expiry::set_policy(c, policy, state.now(), state.clock_unconfirmed())?;
            *state
                .expiry_policy
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = policy;
            Ok(changed)
        })
        .await?;
    tracing::info!(policy = policy.as_str(), "the expiry option changed");
    for (_, ch) in changed {
        emit_changes(sh, &ch);
    }
    sh.expiry_wake.notify_one();
    Ok(())
}

/// [`UiCommand::SubmitChallenge`]: the backend submits the answer; once the network accepted
/// it, the stored challenge is forgotten, then the account's outbox pause is lifted and the
/// waiting rows are sent.
///
/// The challenge is kept in `core.db` so a restart takes the wait up (§4.8 step 3). An
/// answer the network accepted ends it, so it is forgotten by the write that follows the
/// answer, before anything else learns of it: a kill behind that write finds no challenge
/// at the next start. (Forgotten only by the dispatcher's next look, a kill in between
/// showed a solved challenge again until its retry time, a day, or the user ended it.)
async fn submit_challenge(
    sh: &Arc<Shared>,
    senders: &HashMap<AccountId, mpsc::UnboundedSender<Control>>,
    account: AccountId,
    answer: kourier_model::Redacted<String>,
) -> Result<()> {
    if answer.expose().trim().is_empty() {
        return Err(CoreError::InvalidInput("the challenge answer is empty"));
    }
    let tx = senders
        .get(&account)
        .ok_or(CoreError::UnknownAccount(account))?;
    let (reply, rx) = oneshot::channel();
    tx.send(Control::SubmitChallenge { answer, reply })
        .map_err(|_| CoreError::UnknownAccount(account))?;
    match rx.await {
        Ok(Ok(())) => {
            let forgotten = sh
                .store
                .write(Priority::High, lane(account), move |c| {
                    outbox::store_challenge(c, account, None)
                })
                .await;
            if let Err(e) = forgotten {
                // The dispatcher forgets it at its next look, as it does for a wait that
                // ended any other way.
                tracing::warn!(account = account.0, error = %e, "forgetting the answered challenge failed");
            }
            failpoint::hit(outbox::FAILPOINT_AFTER_CHALLENGE_ANSWER);
            let _ = tx.send(Control::Outbox(Signal::ChallengeSolved));
            Ok(())
        }
        Ok(Err(e)) => Err(CoreError::from(e)),
        Err(_) => Err(CoreError::InvalidState("the account is not running")),
    }
}

/// One [`UiCommand::SendFiles`].
#[derive(Clone)]
struct SendFiles {
    conv: ConversationId,
    request: RequestId,
    paths: Vec<PathBuf>,
    as_file: Vec<bool>,
    caption: Option<Text>,
    reply_to: Option<MessageId>,
}

/// Invalidations for a core-local write (the supervisor publishes ingest's own).
pub(crate) fn emit_changes(sh: &Shared, ch: &Changes) {
    sh.forget_media(ch);
    if ch.expiring {
        sh.expiry_wake.notify_one();
    }
    if ch.ringing {
        sh.calls_wake.notify_one();
    }
    if !ch.removed.is_empty() {
        sh.emit(UiEvent::ConversationsRemoved {
            ids: ch.removed.iter().copied().collect(),
        });
    }
    if !ch.conversations.is_empty() {
        sh.emit(UiEvent::ConversationsChanged {
            ids: ch.conversations.iter().copied().collect(),
        });
    }
    for (conv, ids) in &ch.messages {
        if !ch.timelines.contains(conv) {
            sh.emit(UiEvent::MessagesChanged {
                conv: *conv,
                ids: ids.iter().copied().collect(),
            });
        }
    }
    for conv in &ch.timelines {
        sh.emit(UiEvent::TimelineInvalidated { conv: *conv });
    }
}

type Failure = (Option<AccountId>, CoreError);

/// The account and network of a conversation or message, for a UI command.
fn network(sh: &Shared, kind: NetworkKind) -> Result<Arc<dyn Network>> {
    sh.networks
        .get(kind)
        .cloned()
        .ok_or(CoreError::UnknownNetwork(kind))
}

/// §4.8 step 1, then a kick of the account's dispatcher.
async fn send_text(
    sh: &Arc<Shared>,
    senders: &HashMap<AccountId, mpsc::UnboundedSender<Control>>,
    conv: ConversationId,
    text: Text,
    reply_to: Option<MessageId>,
) -> std::result::Result<(), Failure> {
    let (account, kind) = sh
        .store
        .read(move |c| outbox::network_of(c, conv))
        .await
        .map_err(|e| (None, e))?
        .ok_or((None, CoreError::UnknownConversation(conv)))?;
    let net = network(sh, kind).map_err(|e| (Some(account), e))?;
    let now = sh.now();
    let written = sh
        .store
        .write(Priority::High, lane(account), move |c| {
            outbox::enqueue_text(c, net.as_ref(), conv, text, reply_to, now)
        })
        .await
        .map_err(|e| (Some(account), e))?;
    emit_changes(sh, &written.changes);
    if let Some(tx) = senders.get(&written.account) {
        let _ = tx.send(Control::Outbox(Signal::Kick));
    }
    Ok(())
}

/// §4.8 step 5 "Retry".
async fn retry_message(
    sh: &Arc<Shared>,
    senders: &HashMap<AccountId, mpsc::UnboundedSender<Control>>,
    message: MessageId,
) -> std::result::Result<(), Failure> {
    let (account, kind) = sh
        .store
        .read(move |c| outbox::message_network(c, message))
        .await
        .map_err(|e| (None, e))?
        .ok_or((None, CoreError::refused(Refusal::Gone, "unknown message")))?;
    let scope = network(sh, kind)
        .map_err(|e| (Some(account), e))?
        .descriptor()
        .msg_id_scope;
    let now = sh.now();
    let written = sh
        .store
        .write(Priority::High, lane(account), move |c| {
            outbox::retry(c, scope, message, now)
        })
        .await
        .map_err(|e| (Some(account), e))?;
    emit_changes(sh, &written.changes);
    if let Some(tx) = senders.get(&account) {
        let _ = tx.send(Control::Outbox(Signal::Retried { conv: written.conv }));
    }
    Ok(())
}

/// §4.8 step 5: drop a message that has not reached anyone.
async fn cancel_message(
    sh: &Arc<Shared>,
    senders: &HashMap<AccountId, mpsc::UnboundedSender<Control>>,
    message: MessageId,
) -> std::result::Result<(), Failure> {
    let (account, kind) = sh
        .store
        .read(move |c| outbox::message_network(c, message))
        .await
        .map_err(|e| (None, e))?
        .ok_or((None, CoreError::refused(Refusal::Gone, "unknown message")))?;
    let scope = network(sh, kind)
        .map_err(|e| (Some(account), e))?
        .descriptor()
        .msg_id_scope;
    let now = sh.now();
    let written = sh
        .store
        .write(Priority::High, lane(account), move |c| {
            outbox::cancel(c, scope, message, now)
        })
        .await
        .map_err(|e| (Some(account), e))?;
    let dir = sh
        .blob_dir_of(account)
        .await
        .map_err(|e| (Some(account), e))?;
    sh.publish_local(account, &dir, written.changes).await;
    if let Some(tx) = senders.get(&account) {
        let _ = tx.send(Control::Outbox(Signal::Kick));
    }
    Ok(())
}

/// [`UiCommand::EditMessage`], [`UiCommand::DeleteMessage`], [`UiCommand::React`]: validate,
/// apply locally and queue in one transaction ([`modify`]), then unlink deleted files and kick
/// the account's dispatcher.
async fn modify_message(
    sh: &Arc<Shared>,
    senders: &HashMap<AccountId, mpsc::UnboundedSender<Control>>,
    message: MessageId,
    change: modify::Change,
) -> std::result::Result<(), Failure> {
    let (account, kind) = sh
        .store
        .read(move |c| outbox::message_network(c, message))
        .await
        .map_err(|e| (None, e))?
        .ok_or((None, CoreError::refused(Refusal::Gone, "unknown message")))?;
    let net = network(sh, kind).map_err(|e| (Some(account), e))?;
    let now = sh.now();
    let written = sh
        .store
        .write(Priority::High, lane(account), move |c| {
            modify::modify(c, net.as_ref(), message, change, now)
        })
        .await
        .map_err(|e| (Some(account), e))?;
    let dir = sh
        .blob_dir_of(account)
        .await
        .map_err(|e| (Some(account), e))?;
    sh.publish_local(account, &dir, written.changes).await;
    if let Some(tx) = senders.get(&account) {
        let _ = tx.send(Control::Outbox(Signal::Kick));
    }
    Ok(())
}

/// [`UiCommand::SetConversationTimer`]: only timers the chat's features take (an offered
/// value or one of the stated range); the backend confirms with a `ConversationUpsert` after
/// its network transaction.
async fn set_timer(
    sh: &Arc<Shared>,
    senders: &HashMap<AccountId, mpsc::UnboundedSender<Control>>,
    conv: ConversationId,
    timer: Option<Duration>,
) -> std::result::Result<(), Failure> {
    let found = sh
        .store
        .read(move |c| {
            let Some((account, _)) = query::conversation_key(c, conv)? else {
                return Ok(None);
            };
            let features: Option<String> = c
                .prepare_cached("SELECT features FROM conversations WHERE id = ?1")?
                .query_row([conv.0], |r| r.get(0))?;
            Ok(Some((
                account,
                crate::sql::from_json_opt::<kourier_model::ConversationFeatures>(features)?,
            )))
        })
        .await
        .map_err(|e| (None, e))?;
    let (account, features) = found.ok_or((None, CoreError::UnknownConversation(conv)))?;
    // One of the offered values, or any value of the range the network states (a "custom
    // time"): the list is a menu, not a limit. Off wherever the chat has timers at all.
    let allowed = features.as_ref().is_some_and(|f| match timer {
        None => f.has_timers(),
        Some(d) => f.takes_timer(d),
    });
    if !allowed {
        return Err((
            Some(account),
            CoreError::refused(
                Refusal::NotAllowedHere,
                "this chat does not offer that disappearing timer",
            ),
        ));
    }
    let change = kourier_model::ConversationChange::SetTimer { timer };
    change_conversation(sh, senders, conv, change).await
}

/// Stores or clears the composer draft (core-local).
async fn save_draft(
    sh: &Arc<Shared>,
    conv: ConversationId,
    draft: Option<Text>,
) -> std::result::Result<(), Failure> {
    let (account, _) = sh
        .store
        .read(move |c| query::conversation_key(c, conv))
        .await
        .map_err(|e| (None, e))?
        .ok_or((None, CoreError::UnknownConversation(conv)))?;
    sh.store
        .write(Priority::High, lane(account), move |c| {
            outbox::save_draft(c, conv, draft.as_ref())
        })
        .await
        .map_err(|e| (Some(account), e))?;
    sh.emit(UiEvent::ConversationsChanged { ids: vec![conv] });
    Ok(())
}

/// Typing indicators are fire-and-forget and follow the account's typing setting. Nobody we
/// blocked, nobody whose message request we did not accept and no chat we cannot send in
/// learns that we are typing: like a read receipt, it would tell them that we are there and
/// looking at their chat (Signal Desktop sends none into a chat it would not send a message to,
/// `SD/ts/jobs/helpers/shouldSendToConversation.preload.ts`, nor to ourselves,
/// `SD/ts/models/conversations.preload.ts` `sendTypingMessage`).
async fn set_typing(
    sh: &Arc<Shared>,
    senders: &HashMap<AccountId, mpsc::UnboundedSender<Control>>,
    conv: ConversationId,
    on: bool,
) -> std::result::Result<(), Failure> {
    let found = sh
        .store
        .read(move |c| {
            let Some((account, key)) = query::conversation_key(c, conv)? else {
                return Ok(None);
            };
            let info = crate::sql::account_info(c, account)?;
            let (blocked, request_pending, is_self, features): (bool, bool, bool, Option<String>) =
                c.prepare_cached(
                    "SELECT blocked, request_pending, is_self, features FROM conversations
                     WHERE id = ?1",
                )?
                .query_row([conv.0], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })?;
            let features: Option<kourier_model::ConversationFeatures> =
                crate::sql::from_json_opt(features)?;
            let silent = blocked
                || request_pending
                || is_self
                || features.is_some_and(|f| f.cannot_send.is_some() || !f.typing);
            Ok(Some((account, key, info, silent)))
        })
        .await
        .map_err(|e| (None, e))?;
    let (account, key, info, silent) = found.ok_or((None, CoreError::UnknownConversation(conv)))?;
    let enabled = info.is_some_and(|i| i.typing_indicators && i.state == AccountState::Active);
    if !enabled || silent {
        return Ok(());
    }
    if let Some(tx) = senders.get(&account) {
        let _ = tx.send(Control::Backend(Command::SetTyping { conv: key, on }));
    }
    Ok(())
}

/// §4.3 step 1: the row first (so a crash leaves a `Linking` row that startup discards), then the
/// account directory, secret and `key.wrap`.
async fn add_account(
    sh: &Arc<Shared>,
    network: kourier_model::NetworkKind,
) -> Result<(
    AccountId,
    AccountUuid,
    Arc<dyn kourier_backend_api::Network>,
)> {
    let net = sh
        .networks
        .get(network)
        .cloned()
        .ok_or(CoreError::UnknownNetwork(network))?;
    let uuid = AccountUuid::new_random();
    let now = sh.now();
    let id = sh
        .store
        .write(Priority::High, GLOBAL_LANE, move |c| {
            local::insert_account(c, uuid, network, &AccountState::Linking, now)
        })
        .await?;
    if let Err(e) = sh.vault.create(uuid, network).await {
        let _ = sh
            .store
            .write(Priority::High, GLOBAL_LANE, move |c| {
                local::delete_account(c, id)
            })
            .await;
        let _ = sh.vault.remove_dir(uuid).await;
        return Err(e.into());
    }
    Ok((id, uuid, net))
}

async fn mark_read(
    sh: &Arc<Shared>,
    senders: &HashMap<AccountId, mpsc::UnboundedSender<Control>>,
    conv: ConversationId,
) -> Result<()> {
    let account = sh
        .store
        .read(move |c| query::conversation_key(c, conv))
        .await?
        .ok_or(CoreError::UnknownConversation(conv))?
        .0;
    let now = sh.now();
    let done = sh
        .store
        .write(Priority::High, lane(account), move |c| {
            local::mark_read(c, conv, now)
        })
        .await?;
    let Some(done) = done else { return Ok(()) };
    // Reading starts `AfterRead` timers.
    sh.expiry_wake.notify_one();
    failpoint::hit(FAILPOINT_AFTER_PENDING_COMMAND);
    if let Some(tx) = senders.get(&done.account) {
        let _ = tx.send(Control::Durable(done.commands));
        // The mark another device set is gone here; the network hears of it so the other
        // devices can follow. Its answer is not waited for: the chat is read on this device
        // whatever the network says (a backend that cannot write roaming state refuses).
        if let Some(key) = done.mark_cleared {
            let (reply, _) = oneshot::channel();
            let _ = tx.send(Control::Backend(Command::ChangeConversation {
                conv: key,
                change: kourier_model::ConversationChange::SetMarkedUnread {
                    marked_unread: false,
                },
                id: None,
                reply,
            }));
        }
    }
    sh.emit(UiEvent::ConversationsChanged {
        ids: vec![done.conv],
    });
    sh.emit(UiEvent::MessagesChanged {
        conv: done.conv,
        ids: done.messages,
    });
    Ok(())
}

async fn change_conversation(
    sh: &Arc<Shared>,
    senders: &HashMap<AccountId, mpsc::UnboundedSender<Control>>,
    conv: ConversationId,
    change: kourier_model::ConversationChange,
) -> std::result::Result<(), (Option<AccountId>, CoreError)> {
    let (account, key) = sh
        .store
        .read(move |c| query::conversation_key(c, conv))
        .await
        .map_err(|e| (None, e))?
        .ok_or((None, CoreError::UnknownConversation(conv)))?;
    let tx = senders
        .get(&account)
        .ok_or((Some(account), CoreError::UnknownAccount(account)))?;
    // A timer change is a message of its own on some networks: it gets its id from the
    // allocator of own messages, so the row the backend writes for it cannot be taken for
    // another own message (§4.2 `ChangeConversation`).
    let id = if matches!(change, kourier_model::ConversationChange::SetTimer { .. }) {
        let kind = sh
            .store
            .read(move |c| outbox::network_of(c, conv))
            .await
            .map_err(|e| (Some(account), e))?
            .ok_or((Some(account), CoreError::UnknownConversation(conv)))?
            .1;
        let net = network(sh, kind).map_err(|e| (Some(account), e))?;
        let now = sh.now();
        let id = sh
            .store
            .write(Priority::High, lane(account), move |c| {
                outbox::allocate_for_change(c, net.as_ref(), conv, now)
            })
            .await
            .map_err(|e| (Some(account), e))?;
        Some(id)
    } else {
        None
    };
    let (reply, rx) = oneshot::channel();
    tx.send(Control::Backend(Command::ChangeConversation {
        conv: key,
        change,
        id,
        reply,
    }))
    .map_err(|_| (Some(account), CoreError::UnknownAccount(account)))?;
    match rx.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err((Some(account), CoreError::from(e))),
        Err(_) => Err((
            Some(account),
            CoreError::InvalidState("the account is not running"),
        )),
    }
}

/// Startup work on an account's files before its actor runs: the blob-directory sweep and the
/// thumbnails a kill left underived (§5.8).
async fn startup_files(
    sh: &Arc<Shared>,
    account: AccountId,
    uuid: AccountUuid,
    network: NetworkKind,
) {
    let dir = sh.vault.blob_dir(uuid);
    let found = sh
        .store
        .read(move |c| {
            Ok((
                attachments::sweep::referenced(c, account)?,
                crate::ingest::pending_derives(c, account)?,
            ))
        })
        .await;
    let (refs, derives) = match found {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(account = account.0, error = %e, "reading the account's files failed");
            return;
        }
    };
    let d = dir.clone();
    let _ = tokio::task::spawn_blocking(move || attachments::sweep::sweep(&d, &refs)).await;
    // No answer of an earlier run's download can leave a file behind any more.
    if let Err(e) = sh
        .store
        .write(Priority::Low, lane(account), move |c| {
            crate::ingest::forget_orphans(c, account)
        })
        .await
    {
        tracing::warn!(account = account.0, error = %e, "forgetting old download targets failed");
    }
    let scope = sh
        .networks
        .get(network)
        .map_or(kourier_model::MsgIdScope::AccountAuthor, |n| {
            n.descriptor().msg_id_scope
        });
    derive::spawn(sh, account, scope, dir.clone(), derives);
    // Thumbnails are written without fsync: what a power loss took is made again, off the
    // startup path.
    derive::repair_thumbs(sh, account, scope, dir);
}

/// Why a [`UiCommand::SendFiles`] queued nothing.
struct FilesFailure {
    account: Option<AccountId>,
    error: CoreError,
    /// The files that stood in the way, by position (empty when it was not about a file).
    refused: Vec<crate::facade::RefusedFile>,
}

impl FilesFailure {
    fn new(account: Option<AccountId>, error: CoreError) -> Self {
        Self {
            account,
            error,
            refused: Vec::new(),
        }
    }

    fn refused(account: AccountId, refused: Vec<crate::facade::RefusedFile>) -> Self {
        Self {
            account: Some(account),
            error: import::refusal_error(&refused),
            refused,
        }
    }
}

/// The shortest time between two [`Progress::Import`] events of one send that are neither
/// its first nor its last: the copy reports after every chunk it read, the UI hears a few
/// times a second.
const IMPORT_PROGRESS_EVERY: Duration = Duration::from_millis(100);

/// [`UiCommand::SendFiles`]: look at every file, import them (reporting progress), then §4.8
/// step 1 for as many messages as the conversation's packing rule makes of them (one
/// transaction), [`UiEvent::FilesStored`] and a kick of the dispatcher. Whatever fails,
/// nothing stays behind: no message and no blob.
async fn send_files(
    sh: &Arc<Shared>,
    senders: &HashMap<AccountId, mpsc::UnboundedSender<Control>>,
    files: SendFiles,
) -> std::result::Result<(), FilesFailure> {
    let SendFiles {
        conv,
        request,
        paths,
        as_file,
        caption,
        reply_to,
    } = files;
    let rules = sh
        .store
        .read(move |c| file_rules(c, conv))
        .await
        .map_err(|e| FilesFailure::new(None, e))?
        .ok_or(FilesFailure::new(
            None,
            CoreError::UnknownConversation(conv),
        ))?;
    let account = rules.account;
    let fail = |e: CoreError| FilesFailure::new(Some(account), e);
    let net = network(sh, rules.network).map_err(fail)?;
    if paths.is_empty() {
        return Err(fail(CoreError::InvalidInput("no files to send")));
    }
    // Every file is looked at before any is copied: all the refusals at once, each with its
    // position, and no gigabytes encrypted for a send that will not happen.
    let (look, flags, max, features) = (paths.clone(), as_file.clone(), rules.max, rules.features);
    let (seen, refused) = tokio::task::spawn_blocking(move || {
        import::inspect_all(&look, &flags, max, features.as_ref())
    })
    .await
    .map_err(|_| fail(CoreError::Stopped))?;
    if !refused.is_empty() {
        return Err(FilesFailure::refused(account, refused));
    }
    let dir = sh.blob_dir_of(account).await.map_err(fail)?;
    let d = dir.clone();
    let total_files = u32::try_from(paths.len()).unwrap_or(u32::MAX);
    let total_bytes: u64 = seen
        .iter()
        .flatten()
        .fold(0u64, |sum, s| sum.saturating_add(s.size));
    let sh2 = sh.clone();
    let imported = tokio::task::spawn_blocking(move || {
        let mut last: Option<std::time::Instant> = None;
        let mut report = |p: import::ImportProgress| {
            let files_done = u32::try_from(p.files_done).unwrap_or(u32::MAX);
            let ends = last.is_none() || files_done == total_files;
            if !ends && last.is_some_and(|t| t.elapsed() < IMPORT_PROGRESS_EVERY) {
                return;
            }
            last = Some(std::time::Instant::now());
            sh2.emit(UiEvent::Progress(Progress::Import {
                conv,
                request,
                files_done,
                files: total_files,
                // A file that grew while it was read must not show more than everything.
                bytes_done: p.bytes_done.min(total_bytes),
                bytes: total_bytes,
            }));
        };
        import::import_files(&d, &paths, &as_file, max, &mut report)
    })
    .await
    .map_err(|_| fail(CoreError::Stopped))?
    .map_err(|e| match e {
        // A file changed between the look and the copy.
        import::ImportError::Refused(refused) => FilesFailure::refused(account, refused),
        import::ImportError::Other(e) => fail(e),
    })?;
    // Stored in as many transactions as the send needs (`outbox::ENQUEUE_CHUNK` messages
    // each), with the writer free for everything else in between; all of it or none.
    let files: Arc<[import::Imported]> = imported.into();
    let (stored, now, n) = (files.clone(), sh.now(), net.clone());
    let mut step = sh
        .store
        .write(Priority::High, lane(account), move |c| {
            Ok(outbox::enqueue_files(
                c,
                n.as_ref(),
                conv,
                stored,
                caption,
                reply_to,
                now,
            ))
        })
        .await
        .map_err(outbox::FilesError::Other)
        .and_then(|step| step);
    // What is stored of a send in parts so far: dropped again when a later part fails.
    let mut staged: Vec<MessageId> = Vec::new();
    let (written, messages) = loop {
        match step {
            Ok(outbox::FilesStep::Done(written, messages)) => break (written, messages),
            Ok(outbox::FilesStep::More(send)) => {
                staged.clear();
                staged.extend_from_slice(send.stored());
                let (now, n) = (sh.now(), net.clone());
                step = sh
                    .store
                    .write(Priority::High, lane(account), move |c| {
                        outbox::enqueue_files_more(c, n.as_ref(), send, now)
                    })
                    .await
                    .map_err(outbox::FilesError::Other);
            }
            Err(e) => {
                if !staged.is_empty() {
                    // The parts that were stored go again: nothing of the send stays.
                    let (now, scope) = (sh.now(), net.descriptor().msg_id_scope);
                    let dropped = sh
                        .store
                        .write(Priority::High, lane(account), move |c| {
                            outbox::discard_staged(c, scope, &staged, now)
                        })
                        .await;
                    match dropped {
                        Ok(ch) => sh.publish_local(account, &dir, ch).await,
                        // The next start removes them (`outbox::discard_all_staged`).
                        Err(e) => {
                            tracing::warn!(account = account.0, error = %e, "dropping a half-stored send failed")
                        }
                    }
                }
                let _ = tokio::task::spawn_blocking(move || import::discard(&dir, &files)).await;
                return Err(match e {
                    outbox::FilesError::Refused(refused) => FilesFailure::refused(account, refused),
                    outbox::FilesError::Other(e) => fail(e),
                });
            }
        }
    };
    emit_changes(sh, &written.changes);
    // After the invalidations: a UI that reads the rows on this event finds them.
    sh.emit(UiEvent::FilesStored {
        conv: written.conv,
        request,
        messages,
    });
    if let Some(tx) = senders.get(&written.account) {
        let _ = tx.send(Control::Outbox(Signal::Kick));
    }
    Ok(())
}

/// A delete made on this device is published: invalidations, files unlinked, the durable
/// commands handed to the account's supervisor (its next start resends what is stored), and
/// the dispatcher looks again when outbox rows went with the messages.
async fn publish_delete(
    sh: &Arc<Shared>,
    senders: &HashMap<AccountId, mpsc::UnboundedSender<Control>>,
    done: crate::ingest::LocalDelete,
) -> Result<()> {
    let crate::ingest::LocalDelete {
        account,
        changes,
        commands,
    } = done;
    let kick = changes.outbox;
    if !commands.is_empty() {
        failpoint::hit(FAILPOINT_AFTER_PENDING_COMMAND);
    }
    let dir = sh.blob_dir_of(account).await?;
    sh.publish_local(account, &dir, changes).await;
    if let Some(tx) = senders.get(&account) {
        if !commands.is_empty() {
            let _ = tx.send(Control::Durable(commands));
        }
        if kick {
            let _ = tx.send(Control::Outbox(Signal::Kick));
        }
    }
    Ok(())
}

/// [`UiCommand::ClearConversation`] and, with `remove`, [`UiCommand::DeleteConversation`]:
/// the messages go and the durable command for our other devices is stored in one
/// transaction ([`crate::ingest::clear_conversation`]).
async fn clear_conversation(
    sh: &Arc<Shared>,
    senders: &HashMap<AccountId, mpsc::UnboundedSender<Control>>,
    conv: ConversationId,
    remove: bool,
) -> std::result::Result<(), Failure> {
    let (account, kind) = sh
        .store
        .read(move |c| outbox::network_of(c, conv))
        .await
        .map_err(|e| (None, e))?
        .ok_or((None, CoreError::UnknownConversation(conv)))?;
    let scope = network(sh, kind)
        .map_err(|e| (Some(account), e))?
        .descriptor()
        .msg_id_scope;
    let now = sh.now();
    let done = sh
        .store
        .write(Priority::High, lane(account), move |c| {
            crate::ingest::clear_conversation(c, scope, conv, remove, now)
        })
        .await
        .map_err(|e| (Some(account), e))?;
    publish_delete(sh, senders, done)
        .await
        .map_err(|e| (Some(account), e))
}

/// [`UiCommand::DeleteAttachment`]: the attachment goes and the durable command for our other
/// devices is stored in one transaction ([`crate::ingest::delete_attachment`]).
async fn delete_attachment(
    sh: &Arc<Shared>,
    senders: &HashMap<AccountId, mpsc::UnboundedSender<Control>>,
    attachment: kourier_model::AttachmentId,
) -> std::result::Result<(), Failure> {
    let (account, kind) = sh
        .store
        .read(move |c| crate::ingest::attachment_account(c, attachment))
        .await
        .map_err(|e| (None, e))?
        .ok_or((
            None,
            CoreError::refused(Refusal::Gone, "unknown attachment"),
        ))?;
    let kind = crate::sql::network(&kind).map_err(|e| (Some(account), e))?;
    let scope = network(sh, kind)
        .map_err(|e| (Some(account), e))?
        .descriptor()
        .msg_id_scope;
    let now = sh.now();
    let done = sh
        .store
        .write(Priority::High, lane(account), move |c| {
            crate::ingest::delete_attachment(c, scope, attachment, now)
        })
        .await
        .map_err(|e| (Some(account), e))?;
    publish_delete(sh, senders, done)
        .await
        .map_err(|e| (Some(account), e))
}

/// [`UiCommand::DownloadAttachment`]: one transaction marks it `Downloading` with a fresh
/// target; the supervisor sends the command (or its next start does).
async fn download_attachment(
    sh: &Arc<Shared>,
    senders: &HashMap<AccountId, mpsc::UnboundedSender<Control>>,
    attachment: kourier_model::AttachmentId,
) -> std::result::Result<(), Failure> {
    let (account, kind) = sh
        .store
        .read(move |c| crate::ingest::attachment_account(c, attachment))
        .await
        .map_err(|e| (None, e))?
        .ok_or((
            None,
            CoreError::refused(Refusal::Gone, "unknown attachment"),
        ))?;
    let kind = crate::sql::network(&kind).map_err(|e| (Some(account), e))?;
    let scope = network(sh, kind)
        .map_err(|e| (Some(account), e))?
        .descriptor()
        .msg_id_scope;
    let now = sh.now();
    let mut ch = sh
        .store
        .write(Priority::High, lane(account), move |c| {
            let tx =
                c.transaction_with_behavior(kourier_db::rusqlite::TransactionBehavior::Immediate)?;
            let mut apply = crate::ingest::Apply::begin(&tx, account, scope, now)?;
            apply.user_download(attachment)?;
            apply.finish()?;
            let ch = std::mem::take(&mut apply.ch);
            drop(apply);
            tx.commit()?;
            Ok(ch)
        })
        .await
        .map_err(|e| (Some(account), e))?;
    let jobs = std::mem::take(&mut ch.downloads);
    emit_changes(sh, &ch);
    if !jobs.is_empty()
        && let Some(tx) = senders.get(&account)
    {
        let _ = tx.send(Control::Downloads(jobs));
    }
    Ok(())
}

/// [`UiCommand::OpenMedia`]: the attachment's plaintext file in the media cache.
async fn open_media(sh: &Arc<Shared>, attachment: kourier_model::AttachmentId) -> Result<PathBuf> {
    let entry = sh
        .media
        .resolve(attachment)
        .await?
        .ok_or(CoreError::refused(Refusal::Gone, "unknown attachment"))?;
    let file = entry
        .file
        .clone()
        .ok_or(CoreError::InvalidInput("the attachment is not downloaded"))?;
    // Looked at again in the rows: the handle may be a moment older than a delete, or than
    // the sweeper hiding the message on an unconfirmed clock, and no plaintext copy is made
    // of what is gone for the user.
    let name: Option<Option<String>> = sh
        .store
        .read_media(move |c| {
            Ok(c.prepare_cached(
                "SELECT a.file_name FROM attachments a JOIN messages m ON m.id = a.message_id
                 WHERE a.id = ?1 AND m.expiry_hidden = 0",
            )?
            .query_row([attachment.0], |r| r.get(0))
            .optional()?)
        })
        .await?;
    let name = name.ok_or(CoreError::refused(Refusal::Gone, "unknown attachment"))?;
    let sh2 = sh.clone();
    tokio::task::spawn_blocking(move || {
        sh2.playback.open(
            attachment,
            &file.path,
            &file.key,
            entry.mime.as_deref().or(file.mime.as_deref()),
            name.as_deref(),
        )
    })
    .await
    .map_err(|_| CoreError::Stopped)?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The compose lanes: what is about one conversation runs in the order it came, a long
    /// job of one conversation delays nothing in another, and a lane exists only while it
    /// has work.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn compose_lanes_keep_order_per_conversation_and_do_not_wait_for_each_other() {
        let lanes = Lanes::default();
        let log = Arc::new(Mutex::new(Vec::new()));
        let (release, gate) = oneshot::channel::<()>();
        let (a, b) = (Some(ConversationId(1)), Some(ConversationId(2)));
        let note = |tag: &'static str| {
            let log = log.clone();
            move || log.lock().unwrap_or_else(PoisonError::into_inner).push(tag)
        };
        // A long job in chat 1 (the import of many files), and a text typed behind it.
        let first = note("a: files");
        lanes.push(
            a,
            Box::pin(async move {
                let _ = gate.await;
                first();
            }),
        );
        let second = note("a: text behind the files");
        lanes.push(a, Box::pin(async move { second() }));
        // Chat 2 and a command without a conversation do not wait for it.
        let (done_tx, done_rx) = oneshot::channel::<()>();
        let other = note("b: text");
        lanes.push(b, Box::pin(async move { other() }));
        let stray = note("no conversation");
        lanes.push(
            None,
            Box::pin(async move {
                stray();
                let _ = done_tx.send(());
            }),
        );
        tokio::time::timeout(Duration::from_secs(10), done_rx)
            .await
            .expect("the other lanes ran")
            .expect("ran");
        tokio::time::sleep(Duration::from_millis(50)).await;
        {
            let seen = log.lock().unwrap_or_else(PoisonError::into_inner).clone();
            assert!(seen.contains(&"b: text") && seen.contains(&"no conversation"));
            assert!(!seen.iter().any(|t| t.starts_with("a:")), "{seen:?}");
        }
        assert_eq!(lanes.busy(), 1, "only chat 1 still has work");
        release.send(()).expect("release");
        for _ in 0..200 {
            if lanes.busy() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(lanes.busy(), 0, "idle lanes end");
        let seen = log.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let of_a: Vec<&str> = seen
            .iter()
            .copied()
            .filter(|t| t.starts_with("a:"))
            .collect();
        assert_eq!(of_a, ["a: files", "a: text behind the files"]);
        // A lane that ended is started again by the next job.
        let (again_tx, again_rx) = oneshot::channel::<()>();
        lanes.push(
            a,
            Box::pin(async move {
                let _ = again_tx.send(());
            }),
        );
        tokio::time::timeout(Duration::from_secs(10), again_rx)
            .await
            .expect("ran")
            .expect("ran");
    }
}
