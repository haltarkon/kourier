//! [`FakeNetwork`]: the `Network` factory, and [`FakeControl`], what tests observe and steer.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use kourier_backend_api::{
    AccountContext, AccountHandle, AllocatedId, DurableCommand, Network, OutgoingRequest,
    StartMode, TransmitOutcome, spawn_account_thread,
};
use kourier_model::{
    AccountUuid, BackendError, ConvKey, LocalMsgId, NetworkDescriptor, NetworkKind, UserId,
};
use tokio::sync::Notify;

use crate::flavor::Flavor;
use crate::script::Script;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One `Command::Transmit` the fake answered (in memory, for tests in the same process; the
/// durable server log is [`crate::server_log`]).
#[derive(Clone, Debug, PartialEq)]
pub struct TransmitRecord {
    /// The core's message row.
    pub message: LocalMsgId,
    /// The request as received.
    pub req: OutgoingRequest,
    /// The account clock when it arrived (ms).
    pub at_ms: u64,
    /// The answer.
    pub outcome: TransmitOutcome,
}

/// Shared between the [`FakeNetwork`], its actors and the test.
#[derive(Debug)]
pub struct FakeControl {
    default: Arc<Script>,
    queue: Mutex<VecDeque<Arc<Script>>>,
    assigned: Mutex<HashMap<AccountUuid, Arc<Script>>>,
    spawns: Mutex<HashMap<AccountUuid, Vec<StartMode>>>,
    finished: Mutex<HashSet<AccountUuid>>,
    violations: Mutex<Vec<String>>,
    commands: Mutex<Vec<(AccountUuid, &'static str)>>,
    transmits: Mutex<Vec<(AccountUuid, TransmitRecord)>>,
    /// Relinks tried per account (for `Script::fail_relinks`).
    relinks: Mutex<HashMap<AccountUuid, u32>>,
    /// How far each account got in its script (the cursor), kept here because an unlink wipes
    /// `backend.db`: a relink goes on from there.
    progress: Mutex<HashMap<AccountUuid, usize>>,
    /// The durable commands the actors persisted, in order.
    durable: Mutex<Vec<(AccountUuid, DurableCommand)>>,
    /// The accounts whose device the fake server has: linked, and neither logged out
    /// (`Command::Logout`) nor unlinked by a script.
    devices: Mutex<HashSet<AccountUuid>>,
    changed: Notify,
}

impl FakeControl {
    fn new(default: Script) -> Self {
        Self {
            default: Arc::new(default),
            queue: Mutex::new(VecDeque::new()),
            assigned: Mutex::new(HashMap::new()),
            spawns: Mutex::new(HashMap::new()),
            finished: Mutex::new(HashSet::new()),
            violations: Mutex::new(Vec::new()),
            commands: Mutex::new(Vec::new()),
            transmits: Mutex::new(Vec::new()),
            relinks: Mutex::new(HashMap::new()),
            progress: Mutex::new(HashMap::new()),
            durable: Mutex::new(Vec::new()),
            devices: Mutex::new(HashSet::new()),
            changed: Notify::new(),
        }
    }

    /// The fake server gained (`true`) or forgot (`false`) the device of `uuid`.
    pub(crate) fn device(&self, uuid: AccountUuid, linked: bool) {
        if linked {
            lock(&self.devices).insert(uuid);
        } else {
            lock(&self.devices).remove(&uuid);
        }
        self.changed.notify_waiters();
    }

    /// Whether the fake server still has a device of `uuid`: it finished a link and was not
    /// logged out since (`Command::Logout` to a linked actor) nor unlinked by its script. A
    /// test for "the core logs out a device it did not take" reads this.
    pub fn has_device(&self, uuid: AccountUuid) -> bool {
        lock(&self.devices).contains(&uuid)
    }

    /// The script the next freshly linked account gets (FIFO); the default script otherwise.
    pub fn push_script(&self, script: Script) {
        lock(&self.queue).push_back(Arc::new(script));
    }

    /// The script of an account: assigned at its first spawn and kept across restarts; a fresh
    /// start (link or relink) takes a pushed script when there is one.
    pub fn script_for(&self, uuid: AccountUuid, start: StartMode) -> Arc<Script> {
        self.script_for_spawn(uuid, start).0
    }

    /// [`script_for`](Self::script_for), and whether the script was pushed for this very
    /// start: a script the account ran before is not played again by a relink.
    pub(crate) fn script_for_spawn(
        &self,
        uuid: AccountUuid,
        start: StartMode,
    ) -> (Arc<Script>, bool) {
        let mut assigned = lock(&self.assigned);
        if start == StartMode::Fresh
            && let Some(s) = lock(&self.queue).pop_front()
        {
            assigned.insert(uuid, s.clone());
            // A new script starts at its first step.
            lock(&self.progress).remove(&uuid);
            return (s, true);
        }
        if let Some(s) = assigned.get(&uuid) {
            return (s.clone(), false);
        }
        // Another process assigned it; scripts are deterministic per test binary.
        let s = self.default.clone();
        assigned.insert(uuid, s.clone());
        (s, false)
    }

    /// Counts a relink of `uuid`; `true` while it is one of the first `fail` (the relinks a
    /// script makes fail).
    pub(crate) fn relink_fails(&self, uuid: AccountUuid, fail: u32) -> bool {
        let mut relinks = lock(&self.relinks);
        let n = relinks.entry(uuid).or_insert(0);
        *n = n.saturating_add(1);
        *n <= fail
    }

    /// Notes that `uuid` is at step `cursor` of its script.
    pub(crate) fn reached(&self, uuid: AccountUuid, cursor: usize) {
        lock(&self.progress).insert(uuid, cursor);
        self.changed.notify_waiters();
    }

    /// Whether an account ran the first `steps` steps of its script in this process (a
    /// script that goes on after the part a caller waits for).
    pub fn ran_steps(&self, steps: usize) -> bool {
        lock(&self.progress).values().any(|c| *c >= steps)
    }

    /// The step of its script `uuid` had reached when it was last seen in this process
    /// (`None`: it never ran here).
    pub fn script_progress(&self, uuid: AccountUuid) -> Option<usize> {
        lock(&self.progress).get(&uuid).copied()
    }

    pub(crate) fn persisted(&self, uuid: AccountUuid, command: DurableCommand) {
        lock(&self.durable).push((uuid, command));
        self.changed.notify_waiters();
    }

    /// The durable commands the actors of `uuid` took into their job table (and confirmed
    /// with `CommandPersisted`), in order: what a real backend would act on. A command the
    /// core sent again after a restart shows once (the job table is unique on its id).
    pub fn durable_commands(&self, uuid: AccountUuid) -> Vec<DurableCommand> {
        lock(&self.durable)
            .iter()
            .filter(|(u, _)| *u == uuid)
            .map(|(_, c)| c.clone())
            .collect()
    }

    pub(crate) fn spawned(&self, uuid: AccountUuid, start: StartMode) {
        lock(&self.spawns).entry(uuid).or_default().push(start);
        self.changed.notify_waiters();
    }

    /// How each actor of `uuid` was started, in order.
    pub fn spawns(&self, uuid: AccountUuid) -> Vec<StartMode> {
        lock(&self.spawns).get(&uuid).cloned().unwrap_or_default()
    }

    /// Total number of actor starts.
    pub fn total_spawns(&self) -> usize {
        lock(&self.spawns).values().map(Vec::len).sum()
    }

    pub(crate) fn mark_finished(&self, uuid: AccountUuid) {
        lock(&self.finished).insert(uuid);
        self.changed.notify_waiters();
    }

    /// Whether the account ran every step and the core acked everything.
    pub fn is_finished(&self, uuid: AccountUuid) -> bool {
        lock(&self.finished).contains(&uuid)
    }

    /// Accounts that finished their script.
    pub fn finished(&self) -> Vec<AccountUuid> {
        lock(&self.finished).iter().copied().collect()
    }

    /// Waits until `f` holds (re-checked after every state change of the fake).
    pub async fn wait_for(&self, mut f: impl FnMut(&Self) -> bool) {
        loop {
            let notified = self.changed.notified();
            if f(self) {
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn violation(&self, detail: String) {
        tracing::error!(%detail, "fake backend broke the contract");
        lock(&self.violations).push(detail);
    }

    /// Contract violations found in shipped envelopes (empty when the fake behaved).
    pub fn violations(&self) -> Vec<String> {
        lock(&self.violations).clone()
    }

    pub(crate) fn command(&self, uuid: AccountUuid, name: &'static str) {
        lock(&self.commands).push((uuid, name));
        self.changed.notify_waiters();
    }

    /// Command names the actors received, in order.
    pub fn commands(&self, uuid: AccountUuid) -> Vec<&'static str> {
        lock(&self.commands)
            .iter()
            .filter(|(u, _)| *u == uuid)
            .map(|(_, c)| *c)
            .collect()
    }

    /// Command names every actor received, in order (for a test that has no account's
    /// uuid at hand).
    pub fn all_commands(&self) -> Vec<&'static str> {
        lock(&self.commands).iter().map(|(_, c)| *c).collect()
    }

    pub(crate) fn transmitted(&self, uuid: AccountUuid, record: TransmitRecord) {
        lock(&self.transmits).push((uuid, record));
        self.changed.notify_waiters();
    }

    /// The transmits of `uuid` the fake answered, in order.
    pub fn transmits(&self, uuid: AccountUuid) -> Vec<TransmitRecord> {
        lock(&self.transmits)
            .iter()
            .filter(|(u, _)| *u == uuid)
            .map(|(_, t)| t.clone())
            .collect()
    }
}

/// The scripted backend (§10.2). One instance serves every fake account; each account actor
/// runs on its own thread with a current-thread runtime, owns a real `backend.db` with
/// `core_outbox`, and forwards through `kourier-db`'s forwarder, like a real backend.
#[derive(Clone, Debug)]
pub struct FakeNetwork {
    flavor: Flavor,
    descriptor: NetworkDescriptor,
    control: Arc<FakeControl>,
}

impl FakeNetwork {
    /// A fake network of `flavor` whose accounts run `default` unless a script is pushed.
    pub fn new(flavor: Flavor, default: Script) -> Self {
        Self {
            flavor,
            descriptor: flavor.descriptor(),
            control: Arc::new(FakeControl::new(default)),
        }
    }

    /// States `history` as the descriptor's history source instead of the flavor's own
    /// (`HistorySource::PushedOnLink`: an account of this network offers "Transfer History
    /// from Phone…" in a UI).
    pub fn with_history(mut self, history: kourier_model::HistorySource) -> Self {
        self.descriptor.history = history;
        self
    }

    /// The control shared with the actors.
    pub fn control(&self) -> Arc<FakeControl> {
        self.control.clone()
    }

    /// The flavor.
    pub fn flavor(&self) -> Flavor {
        self.flavor
    }
}

impl Network for FakeNetwork {
    fn kind(&self) -> NetworkKind {
        NetworkKind::Fake
    }

    fn descriptor(&self) -> &NetworkDescriptor {
        &self.descriptor
    }

    fn allocate_message_id(&self, _own: &[UserId], _conv: &ConvKey, now_ms: u64) -> AllocatedId {
        self.flavor.allocate(now_ms)
    }

    /// The fake names the attachments of own messages like those it delivers
    /// ([`crate::scripts::attachment_name`]), so a scripted `AttachmentDeleted` can name one.
    fn attachment_net_ids(&self, local: &str, index: usize) -> Vec<String> {
        vec![crate::scripts::attachment_name(local, index)]
    }

    fn spawn(&self, ctx: AccountContext) -> Result<AccountHandle, BackendError> {
        let (script, pushed) = self.control.script_for_spawn(ctx.uuid, ctx.start);
        let control = self.control.clone();
        let flavor = self.flavor;
        let name = format!("fake-{}", ctx.uuid.short());
        spawn_account_thread(name, move || {
            crate::actor::run(ctx, script, pushed, control, flavor)
        })
    }
}
