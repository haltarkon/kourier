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
    NetworkKind, SafetyAnswer, SafetyUnavailable, Text, UserId,
};
use tokio::sync::{Semaphore, broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::actions;
use crate::attachments::playback::Playback;
use crate::attachments::{self, derive, import};
use crate::dispatch::Signal;
use crate::error::{CoreError, Refusal, Result};
use crate::expiry::ExpiryPolicy;
use crate::facade::{
    ActionSubject, CoreHandle, Internal, LinkState, Progress, Query, QueryData, RefusedFiles,
    RequestId, ShareState, UI_EVENT_CAPACITY, UiCommand, UiEvent,
};
use crate::ingest::{Changes, NotifyCandidate};
use crate::media::{MediaEntry, MediaHandles};
use crate::rules::NetRules;
use crate::store::{GLOBAL_LANE, Store, lane};
use crate::supervisor::{
    Control, FAILPOINT_AFTER_PENDING_COMMAND, Hint, Plan, RestartPolicy, Supervisor, Timeouts,
};
use crate::vault::{AccountVault, VaultError};
use crate::{calls, expiry, local, modify, outbox, query, restore, roaming};

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
    /// The send hold each account's backend stated and has not lifted (`Query::Accounts`).
    pub holds: Mutex<HashMap<AccountId, kourier_model::AccountHold>>,
    /// The messages whose attempt is in flight AND whose backend reports an upload that is
    /// still moving (`AttachmentProgress` with `done < total`): the message → its account and
    /// conversation. THE fact behind a message's "stoppable" ([`crate::actions::stoppable`]):
    /// only a backend that reports per-file progress can answer an `AbortTransmit`. Filled and
    /// emptied by the account's supervisor (when the uploads are over, when the attempt ends,
    /// when the actor stops), read by the timeline query. In memory on purpose: a restart
    /// starts the attempt over and the next one reports again.
    pub uploading: Mutex<HashMap<MessageId, (AccountId, ConversationId)>>,
    /// The conversations whose queue waits for the user to confirm a person's changed safety
    /// number (the dispatcher's identity pauses, P5a BQ-3): the conversation → its account, the
    /// paused head's outbox row and the person. Copied from the dispatcher by the account's
    /// supervisor before it publishes (`Supervisor::share_pauses`), read by the timeline query
    /// (`MessageActions::safety_number` of every own row that still waits, the head and the
    /// rows behind it, U4) and by the safety-number answer
    /// (`QueryData::SafetyNumber::waits`). In memory like the pauses themselves (a restart
    /// retries once and pauses again).
    pub paused: Mutex<HashMap<ConversationId, (AccountId, i64, UserId)>>,
    pub presence: watch::Sender<Presence>,
    pub ui_ready: watch::Sender<bool>,
    /// The media handle table (§8.6).
    pub media: MediaHandles,
    /// Plaintext playback files (§5.8).
    pub playback: Playback,
    /// At most this many thumbnail jobs decode at the same time.
    pub derive_permits: Arc<Semaphore>,
    /// Accounts being removed in this run (link feedback L4, `buttons.md` F6): their background
    /// work (the thumbnails of downloaded attachments) stops instead of reading files that are
    /// being deleted and writing rows of an account that is gone, and a login that starts
    /// meanwhile is not told of them (`taken`). An id comes here when the removal is ASKED for
    /// (`UiCommand::RemoveAccount` accepted by the core task) and again when the supervisor
    /// begins it. Account ids are never reused (`AUTOINCREMENT`), so an id stays here for the
    /// rest of the run.
    pub removing: Mutex<std::collections::BTreeSet<AccountId>>,
    /// The `LogoutOutcome`s told before the front-end said `UiReady` (`Some` until then): a
    /// removal or unlink that a quit cut off is finished while the core starts, before any
    /// front-end listens, and the user must still be told what became of the device. They are
    /// sent again at `UiReady` (same `seq`) and the list is dropped. Bounded.
    pub logout_outcomes: Mutex<Option<Vec<UiEvent>>>,
    /// Each account's last `BackendNotice::HistoryLost` count of this run (W4 fix F2): a
    /// backend appends the lost-history notice at its start, before the front-end listens
    /// (an actor runs before `UiReady`), and the core's ack is what "told" means to the
    /// backend, so the line must not depend on a receiver being there at that moment. It is
    /// state, not an event: kept for the run and told again at every `UiReady`, like the
    /// `LogoutOutcome`s (the viewmodel keeps ONE line per account, so a repeat changes
    /// nothing).
    pub lost_history: Mutex<std::collections::BTreeMap<AccountId, u32>>,
    /// The `seq` of the next `LogoutOutcome`.
    pub outcome_seq: std::sync::atomic::AtomicU64,
    /// The `UiCommand::MarkRead`s sent through [`CoreHandle::send`] and not finished
    /// ([`CoreHandle::reads_in_flight`]): counted up by the send, down by [`ReadDone`] after
    /// the reading's events were emitted.
    pub reads: Arc<std::sync::atomic::AtomicUsize>,
    /// Wakes the disappearing-message sweeper ([`expiry`]) after a write that started a timer.
    pub expiry_wake: tokio::sync::Notify,
    /// Wakes the deadline task of ringing call lines ([`calls`]) after a write that stored
    /// one, and after a resume.
    pub calls_wake: tokio::sync::Notify,
    /// Wakes the task of the waits for the other device's answer (`crate::restore`).
    pub restore_wake: tokio::sync::Notify,
    /// Which accounts' due waits may be judged: not before their backend replayed its outbox
    /// ([`restore::Gate`]).
    pub restore_gate: Mutex<restore::Gate>,
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
/// [`DERIVE_PARALLEL`] as the semaphore counts it: holding this many permits means no
/// thumbnail is being computed.
pub(crate) const DERIVE_PARALLEL_PERMITS: u32 = DERIVE_PARALLEL as u32;

/// How many outcomes wait for the front-end ([`Shared::logout_outcomes`]).
const LOGOUT_OUTCOMES_KEPT: usize = 32;

impl Shared {
    /// Tells the front-ends what became of an account's device ([`UiEvent::LogoutOutcome`]),
    /// once now, and again at `UiReady` when nobody was ready yet.
    pub(crate) fn emit_logout_outcome(
        &self,
        account: AccountId,
        removed: bool,
        report: kourier_model::LogoutReport,
        who: crate::facade::AccountLabel,
    ) {
        // The number is taken and the event sent under the lock, so outcomes reach every
        // receiver in the order of their numbers, also against a replay at `UiReady`.
        let mut kept = self
            .logout_outcomes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let seq = self
            .outcome_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let event = UiEvent::LogoutOutcome {
            seq,
            account,
            removed,
            report,
            who,
        };
        if let Some(list) = kept.as_mut() {
            if list.len() >= LOGOUT_OUTCOMES_KEPT {
                list.remove(0);
            }
            list.push(event.clone());
        }
        self.emit(event);
    }

    /// The front-end is ready: what was told before it is told again.
    fn replay_logout_outcomes(&self) {
        // Sent under the lock: a new outcome cannot overtake the ones told again.
        let mut kept = self
            .logout_outcomes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for event in kept.take().into_iter().flatten() {
            self.emit(event);
        }
    }

    /// Keeps `account`'s last lost-history count of this run ([`Shared::lost_history`]).
    pub(crate) fn note_history_lost(&self, account: AccountId, count: u32) {
        self.lost_history
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(account, count);
    }

    /// The front-end is ready: the lost-history count of each account that is still linked
    /// and not being removed is told again ([`Shared::lost_history`]).
    fn replay_lost_history(&self) {
        let kept: Vec<(AccountId, u32)> = self
            .lost_history
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(a, c)| (*a, *c))
            .collect();
        for (account, count) in kept {
            let logged_out = self
                .conn_states
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&account)
                .is_some_and(|c| matches!(c, ConnState::LoggedOut { .. }));
            if logged_out || self.is_removing(account) {
                continue;
            }
            self.emit(UiEvent::Notice {
                account,
                notice: kourier_backend_api::BackendNotice::HistoryLost { count },
            });
        }
    }

    /// Whether `account` is being removed in this run ([`Shared::removing`]).
    pub(crate) fn is_removing(&self, account: AccountId) -> bool {
        self.removing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&account)
    }

    /// The accounts being removed in this run, as a list ([`Shared::removing`]).
    pub(crate) fn removing_accounts(&self) -> Vec<AccountId> {
        self.removing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .copied()
            .collect()
    }
}

impl Shared {
    pub(crate) fn emit(&self, e: UiEvent) {
        let _ = self.events.send(e);
    }

    pub(crate) fn now(&self) -> i64 {
        i64::try_from(self.clock.now_ms()).unwrap_or(i64::MAX)
    }

    /// Whether the account's send hold stands now ([`kourier_model::AccountHold`]): one the
    /// backend stated (or the core took up from the last run) whose date, if it has one, has
    /// not passed.
    pub(crate) fn account_held(&self, account: AccountId) -> bool {
        let now = u64::try_from(self.now().max(0)).unwrap_or(0);
        self.holds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&account)
            .is_some_and(|h| h.until_ms.is_none_or(|until| now < until))
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
        // Counted before the UI events: a reader that saw them sees the jobs until their
        // results are published (`MediaHandles::deriving`).
        let hold = self.media.reserve(ch.derive.len());
        emit_changes(self, &ch);
        if !ch.unlink.is_empty() {
            let (dir, paths) = (blob_dir.to_owned(), ch.unlink.clone());
            let _ = tokio::task::spawn_blocking(move || attachments::unlink(&dir, &paths)).await;
        }
        if !ch.derive.is_empty() {
            let scope = self.scope_of(account).await;
            derive::spawn(self, account, scope, blob_dir.to_owned(), ch.derive, hold);
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
    /// The deadline task of the waits for the other device's answer ([`restore`]).
    restores: Option<JoinHandle<()>>,
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
        crate::ingest::restore::tune_waits(
            cfg.timeouts.restore_answer,
            cfg.timeouts.restore_pending,
        );
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
            holds: Mutex::new(HashMap::new()),
            uploading: Mutex::new(HashMap::new()),
            paused: Mutex::new(HashMap::new()),
            presence: watch::channel(Presence::default()).0,
            ui_ready: watch::channel(false).0,
            media: MediaHandles::new(tx.clone()),
            playback,
            derive_permits: Arc::new(Semaphore::new(DERIVE_PARALLEL)),
            removing: Mutex::new(std::collections::BTreeSet::new()),
            logout_outcomes: Mutex::new(Some(Vec::new())),
            lost_history: Mutex::new(std::collections::BTreeMap::new()),
            outcome_seq: std::sync::atomic::AtomicU64::new(0),
            reads: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            expiry_wake: tokio::sync::Notify::new(),
            calls_wake: tokio::sync::Notify::new(),
            restore_wake: tokio::sync::Notify::new(),
            restore_gate: Mutex::new(restore::Gate::default()),
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
        // The accounts that will get an actor are "starting" for the waits for the other
        // device's answer (`restore::Gate`) from here on, before the task that judges them is
        // spawned: its first look comes at once, and a wait that ran out while Kourier was
        // closed must not end before the account's actor was registered and its backend
        // replayed the answer it kept (the supervisor first unwraps the key, reads the
        // preferences and spawns the network; its own mark replaces this one).
        for (id, _, network, plan) in &plans {
            if matches!(plan, Plan::Run(_)) && shared.networks.get(*network).is_some() {
                shared.restore_actor_started(*id);
            }
        }
        // And before any query is served: what came due while Kourier was not running is
        // hidden from the first read on, where the user chose that.
        let (hold, now) = (shared.expiry_hold(), shared.now());
        shared
            .store
            .write(Priority::High, GLOBAL_LANE, move |c| {
                expiry::settle_at_start(c, now, &hold)
            })
            .await?;
        for (id, uuid, network, plan) in &plans {
            // A removed account's files are going: no work is started for them.
            if !matches!(plan, Plan::Remove) {
                startup_files(&shared, *id, *uuid, *network).await;
            }
        }
        let handle = CoreHandle {
            commands: tx,
            events,
            media: shared.media.clone(),
            reads: shared.reads.clone(),
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
        let restores = tokio::spawn(restore::run(shared.clone()));
        Ok(Self {
            handle,
            task: Some(task),
            sweeper: Some(sweeper),
            calls: Some(calls),
            restores: Some(restores),
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
        for t in [self.sweeper.take(), self.calls.take(), self.restores.take()]
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
        for t in [self.sweeper.take(), self.calls.take(), self.restores.take()]
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
            // A removal that a quit cut off: its supervisor tries the logout once, then
            // finishes it (link feedback L4: the phone must not keep listing the device).
            AccountState::Removing if sh.networks.get(row.network).is_some() => {
                tracing::info!(account = row.id.0, "finishing an unfinished removal");
                plans.push((row.id, row.uuid, row.network, Plan::Remove));
            }
            // The same two with their network off: no supervisor can run them, so they end here
            // (K-A fix C8): the removal tells the outcome kept on the row, the unlink ends
            // logged out with the chats kept.
            AccountState::Removing => {
                tracing::info!(
                    account = row.id.0,
                    "finishing a removal with the network off"
                );
                crate::networks::finish_removal(sh, row);
            }
            AccountState::Unlinking if sh.networks.get(row.network).is_none() => {
                crate::networks::finish_unlink(sh, row);
            }
            // An unlink that a quit cut off: one more logout attempt, then logged out with the
            // chats kept.
            AccountState::Unlinking => {
                tracing::info!(account = row.id.0, "finishing an unfinished unlink");
                plans.push((row.id, row.uuid, row.network, Plan::Unlink));
            }
            AccountState::Linking => {
                tracing::info!(account = row.id.0, "discarding an unfinished link");
                if let Err(e) = sh.vault.shred(row.uuid).await {
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
    // Saves of copies into the user's folders: one at a time, in the order they were asked.
    let (saves_q, saves_rx) = mpsc::unbounded_channel();
    tokio::spawn(run_ordered(saves_rx));
    let queues = Queues {
        route: ordered,
        lanes: Lanes::default(),
        media: media_q,
        saves: saves_q,
        net_order: NetOrder::default(),
    };
    while let Some(msg) = rx.recv().await {
        slots.retain(|_, s| !s.task.is_finished());
        match msg {
            Internal::Ui(cmd) => ui_command(&sh, &mut slots, &queues, cmd).await,
            Internal::Query(Query::SafetyNumber { account, user }, reply) => {
                let answer = ask_safety(&sh, &slots, account, user);
                tokio::spawn(async move {
                    let _ = reply.send(answer.await);
                });
            }
            Internal::Query(
                Query::GroupInvite {
                    conversation,
                    action,
                },
                reply,
            ) => {
                let answer = ask_group_invite(&sh, &slots, conversation, action);
                tokio::spawn(async move {
                    let _ = reply.send(answer.await);
                });
            }
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

/// The answer to [`Query::Person`]: the person, with what their network offers for them.
async fn person_answer(sh: &Arc<Shared>, account: AccountId, user: UserId) -> Result<QueryData> {
    let (detail, kind) = sh
        .store
        .read(move |c| {
            Ok((
                query::person(c, account, &user)?,
                query::account_network(c, account)?,
            ))
        })
        .await?;
    // A static capability of the network (P5a critic F5); a network that is not
    // registered at this start (turned off) offers nothing.
    let net = kind.and_then(|k| network(sh, k).ok());
    let safety = net.as_ref().is_some_and(|n| n.descriptor().safety_numbers);
    Ok(QueryData::Person(detail.map(|mut d| {
        d.safety_numbers = safety;
        d.safety_waits = sends_wait_for(sh, d.account, &d.user);
        // P5b: the number rule is `LinkState::of`'s; the network only formats.
        d.link = match net.as_ref() {
            Some(n) => {
                let by_id = n.contact_id_link(&d.user);
                LinkState::of(&d, n.contact_links(), by_id, |num| {
                    n.contact_link(&d.user, num)
                })
            }
            None => LinkState::Hidden,
        };
        // P5c: the same number rule, and the profile name (SC1).
        let cards = net.as_ref().is_some_and(|n| n.descriptor().contact_cards);
        d.share = ShareState::of(&d, cards);
        Box::new(d)
    })))
}

async fn run_query(sh: &Arc<Shared>, q: Query) -> Result<QueryData> {
    match q {
        Query::Accounts => {
            let states = sh
                .conn_states
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            let holds = sh
                .holds
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            sh.store
                .read(move |c| query::accounts(c, &states, &holds))
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
            let sh2 = sh.clone();
            // The messages of this chat whose upload is reported moving (a snapshot: a change
            // publishes the row again, `Supervisor::upload_moving`).
            let uploading: std::collections::HashSet<MessageId> = sh
                .uploading
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .filter(|(_, (_, conv))| *conv == q.conv)
                .map(|(m, _)| *m)
                .collect();
            // The identity pause of this chat (a snapshot: a change publishes the head again,
            // `Supervisor::share_pauses`).
            let paused = sh
                .paused
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&q.conv)
                .map(|(_, _, user)| user.clone());
            let (page, media) = sh
                .store
                .read(move |c| {
                    // A paused account (its network turned off) offers nothing that needs it.
                    let registered = outbox::network_of(c, q.conv)?
                        .is_none_or(|(_, kind)| sh2.networks.get(kind).is_some());
                    let page = query::timeline(c, &q, now, registered, &uploading, paused)?;
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
        Query::Person { account, user } => person_answer(sh, account, user).await,
        Query::PersonOf(target) => {
            let who = sh.store.read(move |c| query::person_of(c, target)).await?;
            match who {
                Some((account, user)) => person_answer(sh, account, user).await,
                None => Ok(QueryData::Person(None)),
            }
        }
        Query::Group(id) => {
            let (detail, kind) = sh
                .store
                .read(move |c| {
                    let detail = query::group(c, id)?;
                    let kind = match &detail {
                        Some(d) => query::account_network(c, d.account)?,
                        None => None,
                    };
                    Ok((detail, kind))
                })
                .await?;
            let offered = kind
                .and_then(|k| network(sh, k).ok())
                .map_or(kourier_model::GroupInvites::None, |n| n.group_invites());
            Ok(QueryData::Group(detail.map(|mut d| {
                let admin = d
                    .members
                    .iter()
                    .any(|m| m.is_self && m.role == kourier_model::MemberRole::Admin);
                let ended = d.features.group_info.is_some_and(|i| i.terminated);
                if admin && !ended {
                    d.invites = offered;
                }
                Box::new(d)
            })))
        }
        Query::ChatMedia(q) => {
            let (page, media) = sh
                .store
                .read(move |c| {
                    let page = query::chat_media(c, &q)?;
                    let media = query::chat_media_handles(c, &page)?;
                    Ok((page, media))
                })
                .await?;
            // As for a timeline page: `image://kourier/thumb/<id>` of a square needs its handle.
            for m in media {
                sh.register_media(m);
            }
            Ok(QueryData::ChatMedia(page))
        }
        Query::Attachment(id) => sh
            .store
            .read_media(move |c| query::attachment(c, id))
            .await
            .map(QueryData::Attachment),
        Query::SaveFacts(id) => {
            let facts = sh
                .store
                .read_media(move |c| query::save_facts(c, id))
                .await?;
            let Some(mut facts) = facts else {
                return Ok(QueryData::SaveFacts(None));
            };
            let entry = sh.media.resolve(id).await?;
            if let Some(file) = entry.and_then(|e| e.file) {
                facts.sniffed = tokio::task::spawn_blocking(move || {
                    use std::io::Read as _;
                    let key = kourier_blob::FileKey::from_bytes(*file.key.expose());
                    let mut reader = kourier_blob::BlobReader::open(&file.path, &key).ok()?;
                    let mut head = [0u8; 16];
                    let mut got = 0;
                    while got < head.len() {
                        match reader.read(&mut head[got..]) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => got += n,
                        }
                    }
                    kourier_media::sniff_mime(&head[..got]).map(str::to_owned)
                })
                .await
                .map_err(|_| CoreError::Stopped)?;
            }
            Ok(QueryData::SaveFacts(Some(facts)))
        }
        Query::FilePlan(q) => {
            let conv = q.conv;
            let rules = sh
                .store
                .read(move |c| file_rules(c, conv))
                .await?
                .ok_or(CoreError::UnknownConversation(conv))?;
            let paths = q.paths.into_inner();
            // What the network states it cleans of a picture's location (LOC, N9). A network
            // that is not registered (paused, off) states nothing: the verdict asks.
            let cleaned = network(sh, rules.network)
                .ok()
                .and_then(|n| n.descriptor().cleaned_pictures.clone());
            // The files are only looked at, off the async threads: 64 bytes of each, and the
            // look for a location (bounded, kept by file).
            tokio::task::spawn_blocking(move || {
                let features = rules.features.as_ref();
                let (seen, refused) = import::inspect_all(&paths, &q.as_file, rules.max, features);
                let location = seen
                    .iter()
                    .enumerate()
                    .map(|(i, s)| match s {
                        Some(s) if !refused.iter().any(|r| r.index == i) => {
                            s.leaves_with_location(cleaned.as_ref())
                        }
                        _ => false,
                    })
                    .collect();
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
                    location,
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
        // Answered through the account's actor (`ask_group_invite`) before `run_query`.
        Query::GroupInvite { .. } => Ok(QueryData::GroupInvite(None)),
        // Answered through the account's actor (`ask_safety`) before `run_query`.
        Query::SafetyNumber { .. } => Ok(QueryData::SafetyNumber {
            answer: Err(SafetyUnavailable::NotRunning),
            waits: false,
        }),
        Query::QuietNotices => sh
            .store
            .read(crate::notices::shown)
            .await
            .map(QueryData::QuietNotices),
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
    /// `SaveAttachment`: a copy of a big file takes seconds, and it never delays an open.
    saves: mpsc::UnboundedSender<Ordered>,
    /// The order in which the network hears of what the UI changed in one conversation.
    net_order: NetOrder,
}

/// The order in which the network is told of the changes of one conversation (R1, t3 11e): the
/// clearing of a mark when the chat is read (`mark_read`) and a change the UI asks for
/// (`change_conversation`) each run in a task of their own, and the network applies what it
/// hears in the order it hears it. Without this, a mark set right after a chat was read could
/// reach the network before the clearing, which then came last: the phone ended unmarked and
/// the row on this side marked. Each command takes its [`Turn`] in `ui_command`, which sees
/// the commands in the order the UI sent them; the task waits for the turn before it, and only
/// for the telling, not for the network's answer.
///
/// The order is transitive: a turn is over only when the turn before it is. Every task waits
/// for its turn as its first statement, and a turn that ends without having waited (its task
/// returned early, failed or never ran) hands the wait on, so a later turn never goes
/// ahead of an earlier one because a middle one had nothing to tell.
#[derive(Default)]
struct NetOrder {
    /// Per conversation, the end of the turn that was taken last.
    tails: Mutex<HashMap<ConversationId, oneshot::Receiver<()>>>,
    /// The marks the UI set (`SetMarkedUnread { true }`) and no reading has taken since, per
    /// conversation. The row turns marked only when the backend's echo is ingested, so a reading
    /// right behind the mark would find none to clear and the network would end marked; the
    /// reading counts this as the mark (R1 fix, `turn_for_reading`). Shared with the
    /// [`MarkTicket`]s, which forget a mark the backend refused (N-D).
    marks: Arc<Mutex<MarkBook>>,
}

/// The marks the UI told the network and no reading has taken, as a COUNTER per conversation
/// (N-D): two marks in a row are two, so the refusal of one of them (the other may be taken)
/// does not forget the mark that still stands, and a mark the backend refused is forgotten
/// (the next reading would otherwise tell "unmarked" for a mark the network never took). A
/// reading, or a mark taken back, ends the run of marks: it starts a new EPOCH, and the
/// refusal of a mark of an older epoch (its answer came late) changes nothing.
#[derive(Default)]
struct MarkBook {
    /// The epoch the next run of marks gets.
    next_epoch: u64,
    /// Per conversation: the run's epoch and how many of its marks the network has not refused.
    told: HashMap<ConversationId, (u64, u32)>,
}

impl MarkBook {
    /// The UI set a mark: counts it; the run's epoch.
    fn told(&mut self, conv: ConversationId) -> u64 {
        let next = &mut self.next_epoch;
        let run = self.told.entry(conv).or_insert_with(|| {
            *next += 1;
            (*next, 0)
        });
        run.1 += 1;
        run.0
    }

    /// A mark taken back or a reading: the run ends. Whether a mark stood.
    fn ended(&mut self, conv: ConversationId) -> bool {
        self.told.remove(&conv).is_some()
    }

    /// The backend refused a mark of the run `epoch`.
    fn refused(&mut self, conv: ConversationId, epoch: u64) {
        if let Some((run, count)) = self.told.get_mut(&conv)
            && *run == epoch
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.told.remove(&conv);
            }
        }
    }
}

/// A mark the UI set, until the backend has answered it (N-D): it is counted in
/// [`NetOrder`]'s [`MarkBook`] from the moment the UI sent it, in the UI's order, and forgotten
/// by [`MarkTicket::forget`] when the backend refuses it. Dropped before [`MarkTicket::sent`]
/// (the command failed before it reached the supervisor: nobody was told) it forgets too; after
/// it only the backend's refusal does.
struct MarkTicket {
    marks: Arc<Mutex<MarkBook>>,
    conv: ConversationId,
    epoch: u64,
    /// Forget on drop: the command never reached the supervisor.
    armed: bool,
}

impl MarkTicket {
    /// The command reached the supervisor; the backend's answer decides from here.
    fn sent(&mut self) {
        self.armed = false;
    }

    /// The mark is not (or no more) one the network took: it is not counted any more.
    fn forget(&mut self) {
        self.armed = false;
        self.marks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .refused(self.conv, self.epoch);
    }
}

impl Drop for MarkTicket {
    fn drop(&mut self) {
        if self.armed {
            self.forget();
        }
    }
}

impl NetOrder {
    /// The next turn of `conv`, after the one taken before it.
    fn turn(&self, conv: ConversationId) -> Turn {
        let (done, tail) = oneshot::channel();
        let mut tails = self.tails.lock().unwrap_or_else(PoisonError::into_inner);
        // A turn that is over (its sender is gone) is not waited for and needs no entry.
        tails.retain(|_, tail| matches!(tail.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
        let after = tails.insert(conv, tail);
        Turn {
            after,
            done: Some(done),
        }
    }

    /// The turn of a change the UI asks for; a mark it sets is counted for the reading behind
    /// it (the ticket forgets it again if the backend refuses it) and a mark it takes back ends
    /// the run. Called in the order the UI sent the commands.
    fn turn_for_change(
        &self,
        conv: ConversationId,
        change: &kourier_model::ConversationChange,
    ) -> (Turn, Option<MarkTicket>) {
        let mut ticket = None;
        if let kourier_model::ConversationChange::SetMarkedUnread { marked_unread } = change {
            let mut marks = self.marks.lock().unwrap_or_else(PoisonError::into_inner);
            if *marked_unread {
                ticket = Some(MarkTicket {
                    marks: self.marks.clone(),
                    conv,
                    epoch: marks.told(conv),
                    armed: true,
                });
            } else {
                marks.ended(conv);
            }
        }
        (self.turn(conv), ticket)
    }

    /// The turn of a reading, and whether the UI set a mark since the last reading that the
    /// backend has not refused (which the reading then clears, whether or not the row shows it
    /// yet). Called in the order the UI sent the commands.
    fn turn_for_reading(&self, conv: ConversationId) -> (Turn, bool) {
        let told_mark = self
            .marks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .ended(conv);
        (self.turn(conv), told_mark)
    }
}

/// A command's place in [`NetOrder`]; dropped, it ends the turn (a command that fails before
/// it tells the network holds nobody up, and one that never waited does not let the turn
/// after it go before the turn before it).
struct Turn {
    after: Option<oneshot::Receiver<()>>,
    done: Option<oneshot::Sender<()>>,
}

impl Turn {
    /// Waits until the turn before this one is over.
    async fn wait(&mut self) {
        if let Some(after) = self.after.take() {
            let _ = after.await;
        }
    }

    /// Ends the turn: the network has been told. A turn that never waited ends when the turn
    /// before it does (a task of its own waits for that); without a runtime nothing is left
    /// to order and the turn ends at once.
    fn end(&mut self) {
        let Some(done) = self.done.take() else { return };
        if let (Some(after), Ok(runtime)) =
            (self.after.take(), tokio::runtime::Handle::try_current())
        {
            runtime.spawn(async move {
                let _ = after.await;
                drop(done);
            });
        }
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        self.end();
    }
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
            sh.replay_logout_outcomes();
            sh.replay_lost_history();
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
            match control(slots, account, Control::Remove) {
                // Counted as being removed from here, not from the moment its supervisor gets
                // to the command and writes `Removing` (that one may be busy, or still
                // waiting for its key): a login the UI starts right behind this command (the
                // window's "Remove and Link Again" sends both) must not be told that this
                // account is one of the profile's identities (R1 fix 5; the supervisor's
                // read of the other identities leaves `Shared::removing` out).
                Ok(()) => {
                    sh.removing
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(account);
                }
                // No supervisor: an account of a network this run does not have (the user
                // turned it off) is removed here, locally (`networks.rs`); anything else is
                // refused as before.
                Err(e) => {
                    if !crate::networks::remove_without_network(sh, account).await {
                        fail(Some(account), "RemoveAccount", e);
                    }
                }
            }
        }
        UiCommand::Unlink { account } => {
            if let Err(e) = control(slots, account, Control::Unlink) {
                fail(Some(account), "Unlink", e);
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
            // Taken here, in the order the UI sent the commands (see `NetOrder`).
            let (turn, told_mark) = queues.net_order.turn_for_reading(conv);
            // The moment the user read the chat, on the core's clock, read in the same
            // synchronous step as the turn: the reading may wait for its turn behind an earlier
            // change, and a phone's mark that is merged meanwhile is NEWER than this moment
            // (`DurableCommand::ChangeConversation::asked_at`, A10a N-C).
            let asked_at = sh.now();
            // Counts the send's `reads` down however the task ends, after its events.
            let done = ReadDone(sh.reads.clone());
            tokio::spawn(async move {
                let _done = done;
                if let Err(e) = mark_read(&sh2, &senders, conv, turn, told_mark, asked_at).await {
                    let subject = ActionSubject::Conversation(conv);
                    action_failed(&sh2, None, "MarkRead", e, subject);
                }
            });
        }
        UiCommand::ChangeConversation { conv, change } => {
            let sh2 = sh.clone();
            let senders: HashMap<AccountId, mpsc::UnboundedSender<Control>> =
                slots.iter().map(|(a, s)| (*a, s.control.clone())).collect();
            let (turn, mark) = queues.net_order.turn_for_change(conv, &change);
            tokio::spawn(async move {
                let res = change_conversation(&sh2, &senders, conv, change, turn, mark).await;
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
            with_location,
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
                    with_location,
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
                        with_location: files.with_location,
                        caption: files.caption,
                        reply_to,
                        refused: failed.refused,
                    }));
                    action_failed(&sh2, failed.account, "SendFiles", failed.error, subject);
                }
            });
        }
        UiCommand::SendContact {
            conv,
            account,
            user,
        } => {
            let (sh2, senders) = (sh.clone(), senders(slots));
            queues.compose(sh, Subject::Conversation(conv), async move {
                if let Err((failed, e)) = send_contact(&sh2, &senders, conv, account, user).await {
                    let subject = ActionSubject::Conversation(conv);
                    action_failed(&sh2, failed, "SendContact", e, subject);
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
        UiCommand::SaveAttachment {
            attachment,
            target,
            mode,
        } => {
            let sh2 = sh.clone();
            enqueue(&queues.saves, async move {
                match save_attachment(&sh2, attachment, target, mode).await {
                    Ok(bytes) => sh2.emit(UiEvent::AttachmentSaved { attachment, bytes }),
                    Err(e) => {
                        let subject = ActionSubject::Attachment(attachment);
                        action_failed(&sh2, None, "SaveAttachment", e, subject);
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
            let turn = queues.net_order.turn(conv);
            tokio::spawn(async move {
                if let Err((account, e)) = set_timer(&sh2, &senders, conv, timer, turn).await {
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
        UiCommand::ConfirmIdentity { account, user, key } => {
            // Handed to the supervisor HERE (in the order of the commands, as `SetVerified`);
            // only the wait for the backend's answer is spawned. The queues paused on `user`
            // resume after `Ok` only (the lead's RC6): a refusal (the number changed after it
            // was shown) leaves them paused, with no flicker, and the page shows the new number.
            let (reply, answer) = oneshot::channel();
            let sent = control(
                slots,
                account,
                Control::ConfirmIdentity {
                    user: user.clone(),
                    key,
                    reply,
                },
            );
            let (sh2, senders) = (sh.clone(), senders(slots));
            tokio::spawn(async move {
                let result = match sent {
                    Err(e) => Err(e),
                    Ok(()) => match answer.await {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(e)) => Err(CoreError::Backend(Box::new(e))),
                        // Dropped: the actor stopped with the command in hand.
                        Err(_) => Err(CoreError::InvalidState("the account is not running")),
                    },
                };
                match result {
                    Ok(()) => {
                        if let Some(tx) = senders.get(&account) {
                            let _ = tx.send(Control::Released(user));
                        }
                    }
                    Err(e) => {
                        let subject = ActionSubject::Person(user);
                        action_failed(&sh2, Some(account), "ConfirmIdentity", e, subject);
                    }
                }
            });
        }
        UiCommand::SetVerified {
            account,
            user,
            verified,
            key,
        } => {
            // Handed to the supervisor HERE, not from a spawned task (critic H3): its control
            // channel and the actor's command loop are FIFO, so a Mark and a Clear clicked one
            // after the other reach the backend, and the phone, in that order. Only the wait
            // for the answer is spawned.
            let (reply, answer) = oneshot::channel();
            let cmd = Command::SetVerified {
                user: user.clone(),
                verified,
                key,
                reply,
            };
            let sent = control(slots, account, Control::Backend(cmd));
            let (sh2, senders) = (sh.clone(), senders(slots));
            tokio::spawn(async move {
                let result = match sent {
                    Err(e) => Err(e),
                    Ok(()) => match answer.await {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(e)) => Err(CoreError::Backend(Box::new(e))),
                        // Dropped: the account is not linked or its actor stopped.
                        Err(_) => Err(CoreError::InvalidState("the account is not running")),
                    },
                };
                match result {
                    // Whatever the state was (critic F2): a DEFAULT contact's paused send has
                    // no other way out, as with `ConfirmIdentity`.
                    Ok(()) => {
                        if let Some(tx) = senders.get(&account) {
                            let _ = tx.send(Control::Released(user));
                        }
                    }
                    Err(e) => {
                        let subject = ActionSubject::Person(user);
                        action_failed(&sh2, Some(account), "SetVerified", e, subject);
                    }
                }
            });
        }
        UiCommand::SetRoamingWrites { account, on } => {
            // Handed to the supervisor HERE, in the order of the commands (a switch turned on and
            // off again reaches the backend in that order); only the wait for the backend's
            // answer is spawned. The core changes nothing itself: the backend states the new
            // value in the account's caps. An account that is not running or linked drops the
            // command (`Control::Backend`) and answers a failure, as `SetVerified`.
            let (reply, answer) = oneshot::channel();
            let sent = control(
                slots,
                account,
                Control::Backend(Command::SetRoamingWrites { on, reply }),
            );
            let sh2 = sh.clone();
            tokio::spawn(async move {
                let result = match sent {
                    Err(e) => Err(e),
                    Ok(()) => match answer.await {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(e)) => Err(CoreError::Backend(Box::new(e))),
                        // Dropped: the account is not linked or its actor stopped.
                        Err(_) => Err(CoreError::InvalidState("the account is not running")),
                    },
                };
                if let Err(e) = result {
                    action_failed(
                        &sh2,
                        Some(account),
                        "SetRoamingWrites",
                        e,
                        ActionSubject::None,
                    );
                }
            });
        }
        UiCommand::RefreshContact { account, user } => {
            // A page asked for a fresher profile: the backend decides whether it is due
            // (Signal: not within a day), a running linked one only.
            if let Err(e) = control(
                slots,
                account,
                Control::Backend(Command::RefreshContact { user }),
            ) {
                fail(Some(account), "RefreshContact", e);
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
            sh.restore_wake.notify_one();
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
        // Awaited like the option above: the list read right behind it has the new order.
        UiCommand::SetQuietNotices { shown } => {
            if let Err(e) = set_quiet_notices(sh, shown).await {
                fail(None, "SetQuietNotices", e);
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
        // The one query a backend answers (P5a): it needs the account's control channel,
        // which `run_query` has not.
        UiCommand::Query {
            token,
            query: Query::SafetyNumber { account, user },
        } => {
            let answer = ask_safety(sh, slots, account, user);
            let sh2 = sh.clone();
            tokio::spawn(async move {
                let result = answer.await.map_err(Arc::new);
                sh2.emit(UiEvent::QueryResult { token, result });
            });
        }
        // As the safety number: the account's backend answers.
        UiCommand::Query {
            token,
            query:
                Query::GroupInvite {
                    conversation,
                    action,
                },
        } => {
            let answer = ask_group_invite(sh, slots, conversation, action);
            let sh2 = sh.clone();
            tokio::spawn(async move {
                let result = answer.await.map_err(Arc::new);
                sh2.emit(UiEvent::QueryResult { token, result });
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

/// Whether a queue of `account` waits for the user to confirm `user`'s safety number (a
/// snapshot of [`Shared::paused`]): what the safety-number answer and the person's page say.
fn sends_wait_for(sh: &Shared, account: AccountId, user: &UserId) -> bool {
    sh.paused
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .values()
        .any(|(a, _, u)| *a == account && u == user)
}

/// The answer to [`Query::SafetyNumber`]: an account without a running, linked actor (no
/// slot, or the command dropped) is `NotRunning`, its own sentence (critic M7), never the
/// backend's "no key yet".
///
/// The command is handed to the supervisor at once (in the order of the core's commands); the
/// returned future only waits for the answer. `waits` is read from [`Shared::paused`] when the
/// answer is there (P5a BQ-3): whether sends to `user` wait for the user's confirm.
fn ask_safety(
    sh: &Arc<Shared>,
    slots: &HashMap<AccountId, Slot>,
    account: AccountId,
    user: UserId,
) -> impl Future<Output = Result<QueryData>> + Send + 'static {
    let (reply, answer) =
        oneshot::channel::<std::result::Result<SafetyAnswer, kourier_model::BackendError>>();
    let cmd = Command::SafetyNumber {
        user: user.clone(),
        reply,
    };
    let sent = control(slots, account, Control::Backend(cmd)).is_ok();
    let sh = sh.clone();
    async move {
        let not_running = QueryData::SafetyNumber {
            answer: Err(SafetyUnavailable::NotRunning),
            waits: false,
        };
        if !sent {
            return Ok(not_running);
        }
        match answer.await {
            Ok(Ok(answer)) => {
                let waits = sends_wait_for(&sh, account, &user);
                Ok(QueryData::SafetyNumber { answer, waits })
            }
            Ok(Err(e)) => Err(CoreError::Backend(Box::new(e))),
            Err(_) => Ok(not_running),
        }
    }
}

/// The answer to [`Query::GroupInvite`]: the account's backend asks its network. An account that
/// does not run is a failure of the query, as a network that cannot be reached is.
fn ask_group_invite(
    sh: &Arc<Shared>,
    slots: &HashMap<AccountId, Slot>,
    conversation: ConversationId,
    action: kourier_model::GroupInviteAction,
) -> impl Future<Output = Result<QueryData>> + Send + 'static {
    let sh = sh.clone();
    let senders = senders(slots);
    async move {
        let (account, group) = sh
            .store
            .read(move |c| query::conversation_key(c, conversation))
            .await?
            .ok_or(CoreError::UnknownConversation(conversation))?;
        let sender = senders
            .get(&account)
            .ok_or(CoreError::UnknownAccount(account))?;
        let (reply, answer) = oneshot::channel();
        sender
            .send(Control::Backend(Command::GroupInvite {
                group,
                action,
                reply,
            }))
            .map_err(|_| CoreError::UnknownAccount(account))?;
        match answer.await {
            Ok(Ok(link)) => Ok(QueryData::GroupInvite(link)),
            Ok(Err(e)) => Err(CoreError::Backend(Box::new(e))),
            Err(_) => Err(CoreError::UnknownAccount(account)),
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
    // "Erase" erases what is due before the command returns, in the sweeper's own batches: a
    // "keep" right behind it (the next command, which waits for this one) must not find the
    // rows only hidden and show them again (the sweeper's look could come after it). A failed
    // batch is left to the sweeper, which retries.
    if policy == ExpiryPolicy::EraseByLocalClock {
        while let Some(true) = expiry::sweep_once(sh).await {}
    }
    sh.expiry_wake.notify_one();
    Ok(())
}

/// [`UiCommand::SetQuietNotices`]: the choice is stored and, in the same transaction, every
/// conversation gets the sort time its newest listed row says ([`crate::notices::set`]);
/// then the conversations whose sort time changed are announced. Nothing outside SQL reads
/// the choice (ingest never does: a quiet notice opens nothing whatever it says), so there
/// is no copy of it in memory to keep in step.
async fn set_quiet_notices(sh: &Arc<Shared>, shown: bool) -> Result<()> {
    let changed = sh
        .store
        .write(Priority::High, GLOBAL_LANE, move |c| {
            crate::notices::set(c, shown)
        })
        .await?;
    // (No ids and no names in the log.)
    tracing::info!(
        shown,
        conversations = changed.len(),
        "the quiet notices option changed"
    );
    let mut ch = Changes::default();
    ch.conversations.extend(changed);
    emit_changes(sh, &ch);
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
    with_location: Vec<bool>,
    caption: Option<Text>,
    reply_to: Option<MessageId>,
}

/// Invalidations for a core-local write (the supervisor publishes ingest's own).
pub(crate) fn emit_changes(sh: &Shared, ch: &Changes) {
    sh.media.check_reserved(ch.derive.len());
    sh.forget_media(ch);
    if ch.expiring {
        sh.expiry_wake.notify_one();
    }
    if ch.ringing {
        sh.calls_wake.notify_one();
    }
    if ch.restoring {
        sh.restore_wake.notify_one();
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

/// P5c: a contact card of `user` (a person of `account`) into `conv`. The checks, in order: the
/// chat's account is `account` (a card never leaves through another account), its network
/// sends cards, the person's page allows one ([`ShareState::of`]: not the user, a number the
/// page shows, a profile name); then the card is built from the contact row here, in the core,
/// and queued like a text (`outbox::enqueue_contact`). Nothing about the card is logged.
async fn send_contact(
    sh: &Arc<Shared>,
    senders: &HashMap<AccountId, mpsc::UnboundedSender<Control>>,
    conv: ConversationId,
    account: AccountId,
    user: UserId,
) -> std::result::Result<(), Failure> {
    let (conv_account, kind) = sh
        .store
        .read(move |c| outbox::network_of(c, conv))
        .await
        .map_err(|e| (None, e))?
        .ok_or((None, CoreError::UnknownConversation(conv)))?;
    if conv_account != account {
        return Err((
            Some(conv_account),
            CoreError::InvalidInput("a contact card goes only to a chat of its own account"),
        ));
    }
    let net = network(sh, kind).map_err(|e| (Some(account), e))?;
    if !net.descriptor().contact_cards {
        return Err((
            Some(account),
            CoreError::refused(
                Refusal::NotAllowedHere,
                "the network sends no contact cards",
            ),
        ));
    }
    let detail = sh
        .store
        .read(move |c| query::person(c, account, &user))
        .await
        .map_err(|e| (Some(account), e))?
        .ok_or((Some(account), CoreError::InvalidInput("unknown person")))?;
    let card = detail.contact_card().ok_or((
        Some(account),
        CoreError::InvalidInput("this person's page offers no contact card"),
    ))?;
    let now = sh.now();
    let written = sh
        .store
        .write(Priority::High, lane(account), move |c| {
            outbox::enqueue_contact(c, net.as_ref(), conv, card, now)
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
    let rules = NetRules::of(
        network(sh, kind)
            .map_err(|e| (Some(account), e))?
            .descriptor(),
    );
    let now = sh.now();
    let written = sh
        .store
        .write(Priority::High, lane(account), move |c| {
            outbox::retry(c, rules, message, now)
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
    // A take-back reaches nobody and needs no network: also of a paused account (its network
    // turned off), where the scope only matters to rows the ingest writes (K-A fix C2).
    let scope = match network(sh, kind) {
        Ok(net) => net.descriptor().msg_id_scope,
        Err(CoreError::UnknownNetwork(_)) => kourier_model::MsgIdScope::default(),
        Err(e) => return Err((Some(account), e)),
    };
    let now = sh.now();
    // The set of reported uploads is read INSIDE the write: the rule (`actions::stoppable`)
    // is judged on the row as the write finds it.
    let shared = Arc::clone(sh);
    let cancelled = sh
        .store
        .write(Priority::High, lane(account), move |c| {
            let uploading = shared
                .uploading
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(&message);
            outbox::cancel(c, scope, message, now, uploading)
        })
        .await
        .map_err(|e| (Some(account), e))?;
    let (written, asked) = match cancelled {
        outbox::Cancelled::TakenBack(written) => (written, false),
        outbox::Cancelled::Asked(written) => (written, true),
    };
    let dir = sh
        .blob_dir_of(account)
        .await
        .map_err(|e| (Some(account), e))?;
    sh.publish_local(account, &dir, written.changes).await;
    if let Some(tx) = senders.get(&account) {
        // A stop is asked of the backend by the account's dispatcher; a take-back only frees
        // the chat's queue.
        let signal = if asked {
            Signal::Abort(message)
        } else {
            Signal::Kick
        };
        let _ = tx.send(Control::Outbox(signal));
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
    mut turn: Turn,
) -> std::result::Result<(), Failure> {
    // First, before a refusal or an error can end the turn (`NetOrder`).
    turn.wait().await;
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
    // A timer is no mark: no ticket.
    change_conversation(sh, senders, conv, change, turn, None).await
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

/// Counts one `MarkRead` of [`Shared::reads`] down when dropped: at the end of its task,
/// after [`mark_read`] emitted the reading's events (an early return, an error or a panic
/// count it down too, so a screenshot never waits for a read that ended).
struct ReadDone(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for ReadDone {
    fn drop(&mut self) {
        // Every `UiCommand` reaches the core through `CoreHandle::send`, which counted it.
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

async fn mark_read(
    sh: &Arc<Shared>,
    senders: &HashMap<AccountId, mpsc::UnboundedSender<Control>>,
    conv: ConversationId,
    mut turn: Turn,
    told_mark: bool,
    asked_at: i64,
) -> Result<()> {
    // First, before an early return can end the turn: after what the UI asked for in this
    // conversation before the reading (and before what it asks for after it: those wait for
    // this turn). The reading decides whether there is a mark to clear only now, so a mark
    // the UI set right before it has been told to the network (`NetOrder`).
    turn.wait().await;
    let account = sh
        .store
        .read(move |c| query::conversation_key(c, conv))
        .await?
        .ok_or(CoreError::UnknownConversation(conv))?
        .0;
    // `now` is the time of the read state and of the `AfterRead` timers (after the wait);
    // `asked_at` is the time the user asked (before it).
    let now = sh.now();
    let done = sh
        .store
        .write(Priority::High, lane(account), move |c| {
            local::mark_read(c, conv, now, asked_at, told_mark)
        })
        .await?;
    let Some(done) = done else { return Ok(()) };
    // Reading starts `AfterRead` timers.
    sh.expiry_wake.notify_one();
    failpoint::hit(FAILPOINT_AFTER_PENDING_COMMAND);
    if let Some(tx) = senders.get(&done.account) {
        // The commands the reading stored (A10a: for an account whose caps state
        // `roaming_writes`, the clearing of the mark is one of them, durable: resent from
        // `pending_commands` at every start until the backend answers `CommandPersisted`).
        let _ = tx.send(Control::Durable(done.commands));
        // The mark another device set (or the UI itself, whose echo has not come yet) is gone
        // here; the network hears of it so the other devices can follow. For an account that
        // does not state `roaming_writes` this is the plain, best-effort form (`mark_cleared`
        // is only set for it): its answer is not waited for, the chat is read on this device
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
    turn.end();
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
    mut turn: Turn,
    mut mark: Option<MarkTicket>,
) -> std::result::Result<(), (Option<AccountId>, CoreError)> {
    // First, before an error can end the turn: after the telling of what came before in this
    // conversation (`NetOrder`); the network's answer is not part of the turn.
    turn.wait().await;
    let (account, key) = sh
        .store
        .read(move |c| query::conversation_key(c, conv))
        .await
        .map_err(|e| (None, e))?
        .ok_or((None, CoreError::UnknownConversation(conv)))?;
    let tx = senders
        .get(&account)
        .ok_or((Some(account), CoreError::UnknownAccount(account)))?;
    // The settings the network keeps for the account's other devices (archive, pin, mute, marked
    // unread): the core's second line, the backend is the gate. Refused here when the chat's own
    // account does not state that it writes them (or has the switch off), when the network does
    // not list the chat, and for the account's one pin too many; the pins are counted per account
    // (a blended list holds several, and the change goes to the chat's own). Nothing reaches the
    // backend; the turn ends and a mark that was counted is forgotten when `turn` and `mark` drop.
    if change.is_roaming() {
        let probe = change.clone();
        sh.store
            .read(move |c| roaming::check(c, conv, &probe))
            .await
            .map_err(|e| (Some(account), e))?;
    }
    // During the account's send hold a timer change is refused with the reason named (the timer
    // menu says it too): it is one more message to the network, and it is not queued. The other
    // changes (archive, pin, mute, block, mark, accept, leave) are not held.
    if matches!(change, kourier_model::ConversationChange::SetTimer { .. })
        && sh.account_held(account)
    {
        return Err((
            Some(account),
            CoreError::refused(
                Refusal::Held,
                "the network holds back what is sent from this account for now",
            ),
        ));
    }
    // A timer change is a message of its own on some networks: it gets its id from the
    // allocator of own messages, so the row the backend writes for it cannot be taken for
    // another own message (§4.2 `ChangeConversation`).
    let id = if matches!(change, kourier_model::ConversationChange::SetTimer { .. }) {
        // A chat that cannot send takes no timer change either: it is one more message to the
        // network (a message of its own on some networks), refused before an id is allocated
        // and before anything reaches the backend, by the rule of a new send and of "Send
        // Again" (`actions::cannot_send`; the timer menu is hidden for such a chat, this is
        // the safety net for a stale UI or a second front-end).
        let features = sh
            .store
            .read(move |c| outbox::features_of(c, conv))
            .await
            .map_err(|e| (Some(account), e))?;
        crate::actions::cannot_send(features.as_ref()).map_err(|e| (Some(account), e))?;
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
    if let Some(mark) = mark.as_mut() {
        mark.sent();
    }
    turn.end();
    match rx.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            // N-D: a mark the backend refused is not one the network has; the next reading
            // must not tell "unmarked" for it. (A dropped reply, below, is not known to be a
            // refusal: the mark stays counted, and a clearing for nothing is harmless.)
            if let Some(mark) = mark.as_mut() {
                mark.forget();
            }
            Err((Some(account), CoreError::from(e)))
        }
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
    let hold = sh.media.reserve(derives.len());
    derive::spawn(sh, account, scope, dir.clone(), derives, hold);
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
        with_location,
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
    // What the network states it cleans of a picture's location (LOC): a file that would leave
    // with where it was taken goes only with the user's agreement (`with_location`, BQ1).
    let cleaned = net.descriptor().cleaned_pictures.clone();
    let (agreed, c) = (with_location.clone(), cleaned.clone());
    let (seen, refused) = tokio::task::spawn_blocking(move || {
        let (seen, mut refused) = import::inspect_all(&look, &flags, max, features.as_ref());
        for (index, s) in seen.iter().enumerate() {
            let Some(s) = s else { continue };
            if !refused.iter().any(|r| r.index == index)
                && s.leaves_with_location(c.as_ref())
                && !import::flagged(&agreed, index)
            {
                refused.push(crate::facade::RefusedFile {
                    index,
                    reason: crate::facade::FileRefusal::Location,
                });
            }
        }
        refused.sort_by_key(|r| r.index);
        (seen, refused)
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
        let location = import::LocationRule {
            agreed: &with_location,
            cleaned: cleaned.as_ref(),
        };
        import::import_files(&d, &paths, &as_file, location, max, &mut report)
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
    let commands = std::mem::take(&mut ch.commands);
    emit_changes(sh, &ch);
    if let Some(tx) = senders.get(&account) {
        // A request to the other device (a click on a file the network lost): its row is
        // stored, the account's next start resends it if this one is lost.
        if !commands.is_empty() {
            let _ = tx.send(Control::Durable(commands));
        }
        if !jobs.is_empty() {
            let _ = tx.send(Control::Downloads(jobs));
        }
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

/// [`UiCommand::SaveAttachment`]: a plaintext copy of the attachment in the user's folder
/// (`kourier_blob::save_copy`, on the blocking pool: the Qt thread and the core's task never
/// wait for the disk). The rule is [`actions::save_attachments`]'s two functions, asked again
/// on the attachment's own row: the handle may be a moment older than a delete, than the
/// sweeper hiding the message on an unconfirmed clock, or than the dialog the user took his
/// time with, and no plaintext copy is made of what is gone for the user. The log says that a
/// save ran and how big it was (in the writer), never a path or a name.
async fn save_attachment(
    sh: &Arc<Shared>,
    attachment: kourier_model::AttachmentId,
    target: crate::facade::SaveTarget,
    mode: kourier_blob::SaveMode,
) -> Result<u64> {
    kourier_blob::check_name(&target.name)?;
    // An absolute folder only: a relative one would be opened against the process's working
    // directory, which no front-end means (the dialog gives a full path).
    if !target.dir.is_absolute() {
        return Err(kourier_blob::SaveError::NoFolder.into());
    }
    let entry = sh
        .media
        .resolve(attachment)
        .await?
        .ok_or(CoreError::refused(Refusal::Gone, "unknown attachment"))?;
    type Row = (Option<String>, bool, bool, String, bool);
    let row: Option<Row> = sh
        .store
        .read_media(move |c| {
            Ok(c.prepare_cached(
                "SELECT m.deleted, m.view_once, m.expiry_hidden, a.state, a.long_text
                 FROM attachments a JOIN messages m ON m.id = a.message_id WHERE a.id = ?1",
            )?
            .query_row([attachment.0], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .optional()?)
        })
        .await?;
    let (deleted, view_once, hidden, state, long_text) =
        row.ok_or(CoreError::refused(Refusal::Gone, "unknown attachment"))?;
    actions::may_save_message(deleted.as_deref(), view_once, hidden)?;
    actions::may_save_file(crate::sql::from_json(&state)?, long_text)?;
    let file = entry
        .file
        .clone()
        .ok_or(CoreError::InvalidInput("the attachment is not downloaded"))?;
    tokio::task::spawn_blocking(move || {
        kourier_blob::save_copy(
            &file.path,
            &kourier_blob::FileKey::from_bytes(*file.key.expose()),
            &target.dir,
            &target.name,
            mode,
        )
    })
    .await
    .map_err(|_| CoreError::Stopped)?
    .map_err(CoreError::from)
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

    /// Polls `turn.wait()` for a moment: did the turn before it end?
    async fn went(turn: &mut Turn) -> bool {
        tokio::time::timeout(Duration::from_millis(150), turn.wait())
            .await
            .is_ok()
    }

    /// R1 fix: the order of the turns of one conversation is transitive. A turn that ends
    /// without having waited (its task returned early: a reading with no mark to clear, a
    /// refused timer, a failed read) must not let the turn after it go before the turn
    /// before it, or two changes with a reading between them would reach the network in
    /// either order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_turn_that_ends_without_waiting_does_not_let_the_next_one_pass_the_first() {
        let order = NetOrder::default();
        let c = ConversationId(1);
        let mut first = order.turn(c);
        let second = order.turn(c);
        let mut third = order.turn(c);
        let mut other = order.turn(ConversationId(2));
        // The middle turn is dropped at once: it had nothing to tell and never waited.
        drop(second);
        assert!(
            !went(&mut third).await,
            "the third turn went while the first was still open"
        );
        // Another conversation does not wait for it.
        assert!(went(&mut other).await, "chat 2 waits for nobody");
        first.wait().await;
        first.end();
        assert!(went(&mut third).await, "the first is over: the third goes");
    }

    /// The same through `end()` (a task that tells nothing but finishes normally), a chain of
    /// several, and a first turn that is dropped with the others waiting.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_turns_of_a_conversation_end_in_the_order_they_were_taken() {
        let order = NetOrder::default();
        let c = ConversationId(7);
        let mut first = order.turn(c);
        let mut second = order.turn(c);
        let mut third = order.turn(c);
        let mut fourth = order.turn(c);
        second.end();
        third.end();
        assert!(
            !went(&mut fourth).await,
            "two ended turns hold nobody up before the first"
        );
        // A turn that waited and ended passes the order on as before.
        first.end();
        assert!(went(&mut fourth).await);
        // No runtime: a turn dropped there ends at once and does not panic.
        let lone = order.turn(c);
        std::thread::spawn(move || drop(lone))
            .join()
            .expect("dropped");
    }

    /// R1 fix: a mark the UI sets is remembered for the reading behind it, in the UI's order.
    #[tokio::test]
    async fn a_reading_behind_a_mark_clears_it_and_only_once() {
        use kourier_model::ConversationChange::{SetMarkedUnread, SetPinned};
        let order = NetOrder::default();
        let (a, b) = (ConversationId(1), ConversationId(2));
        let mark = |on| SetMarkedUnread { marked_unread: on };
        assert!(!order.turn_for_reading(a).1, "no mark was set");
        // The tickets live as long as the commands do (a dropped ticket is a mark that never
        // reached the network).
        let _mark_a = order.turn_for_change(a, &mark(true));
        let _pin_b = order.turn_for_change(b, &SetPinned { pinned: true });
        assert!(
            !order.turn_for_reading(b).1,
            "another kind of change is no mark"
        );
        assert!(
            order.turn_for_reading(a).1,
            "the mark waits for the reading"
        );
        assert!(!order.turn_for_reading(a).1, "and a reading takes it once");
        // Set and taken back: nothing to clear.
        let _set = order.turn_for_change(a, &mark(true));
        let _back = order.turn_for_change(a, &mark(false));
        assert!(!order.turn_for_reading(a).1);
    }

    /// N-D: the told marks are a COUNTER per conversation. The refusal of a mark forgets that
    /// mark only: of two marks in a row the second still stands, and the reading after the
    /// refusal of the only one tells nothing. A refusal that comes late, after a reading or a
    /// mark taken back, belongs to an older run of marks and changes nothing of the newer one;
    /// a command that never reached the supervisor forgets its mark by being dropped, one the
    /// supervisor took does not.
    #[tokio::test]
    async fn the_told_marks_are_counted_and_a_refusal_forgets_only_its_own() {
        use kourier_model::ConversationChange::SetMarkedUnread;
        let mark = |on| SetMarkedUnread { marked_unread: on };
        let order = NetOrder::default();
        let (a, b) = (ConversationId(1), ConversationId(2));
        let told = |order: &NetOrder, conv| {
            let (_turn, ticket) = order.turn_for_change(conv, &mark(true));
            ticket.expect("a mark has a ticket")
        };

        // One mark, refused: the reading behind it has nothing to clear.
        let mut only = told(&order, a);
        only.sent();
        only.forget();
        assert!(!order.turn_for_reading(a).1, "a refused mark is forgotten");

        // Two marks in a row, the first refused: the second stands.
        let mut first = told(&order, a);
        let mut second = told(&order, a);
        first.sent();
        second.sent();
        first.forget();
        assert!(order.turn_for_reading(a).1, "the second mark still stands");
        assert!(!order.turn_for_reading(a).1, "and a reading takes both");
        // Refused after the reading took them: nothing to undo, nothing breaks.
        second.forget();
        assert!(!order.turn_for_reading(a).1);

        // An answer that comes late does not forget the NEWER run's mark.
        let mut old = told(&order, b);
        old.sent();
        assert!(order.turn_for_reading(b).1, "a reading ends the run");
        let mut new = told(&order, b);
        new.sent();
        old.forget();
        assert!(
            order.turn_for_reading(b).1,
            "the old run's refusal left the new run's mark alone"
        );
        let mut again = told(&order, b);
        again.sent();
        let (_turn, taken_back) = order.turn_for_change(b, &mark(false));
        assert!(taken_back.is_none(), "taking a mark back needs no ticket");
        new.forget();
        again.forget();
        assert!(!order.turn_for_reading(b).1);

        // Never reached the supervisor (dropped before `sent`): not counted any more.
        drop(told(&order, a));
        assert!(!order.turn_for_reading(a).1, "a mark nobody was told of");
        // Reached it (`sent`) and dropped: still counted, the answer may be a refusal later.
        let mut kept = told(&order, a);
        kept.sent();
        drop(kept);
        assert!(order.turn_for_reading(a).1, "an answered-later mark stands");
    }
}
