//! [`FakeNetwork`]: the `Network` factory, and [`FakeControl`], what tests observe and steer.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use kourier_backend_api::{
    AbortAnswer, AccountContext, AccountHandle, AllocatedId, DurableCommand, Network,
    OutgoingRequest, StartMode, TakenIdentity, TransmitOutcome, spawn_account_thread,
};
use kourier_model::{
    AccountUuid, BackendError, ConvKey, ConversationChange, LocalMsgId, NetworkDescriptor,
    NetworkKind, UserId,
};
use tokio::sync::Notify;

use crate::flavor::Flavor;
use crate::script::Script;

/// The `(field id, value)` pairs of one `Command::SubmitLoginInput`.
type LoginInput = Vec<(String, String)>;

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

/// One `Command::AbortTransmit` the fake answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbortRecord {
    /// The message row the abort named.
    pub message: LocalMsgId,
    /// What the fake answered.
    pub answer: AbortAnswer,
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
    /// The conversation changes the actors heard, in order (what `commands` leaves out).
    changes: Mutex<Vec<(AccountUuid, ConversationChange)>>,
    /// The `taken` identities of every `Command::StartLogin` the actors received, in order.
    logins: Mutex<Vec<(AccountUuid, Vec<TakenIdentity>)>>,
    /// The flow id of every `Command::StartLogin`, in order.
    login_flows: Mutex<Vec<(AccountUuid, String)>>,
    /// The values of every `Command::SubmitLoginInput`, in order (personal data: the fake
    /// keeps them for a test to read).
    login_inputs: Mutex<Vec<(AccountUuid, LoginInput)>>,
    /// How many `pair-code` logins the actors of this network started (the n-th one gets the
    /// n-th refusal of [`crate::PairCodeScript::refusals`]).
    pair_attempts: Mutex<usize>,
    transmits: Mutex<Vec<(AccountUuid, TransmitRecord)>>,
    /// The aborts the actors answered, in order.
    aborts: Mutex<Vec<(AccountUuid, AbortRecord)>>,
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
    /// The doors of the running accounts, in the order they opened ([`FakeControl::arrive`]).
    pub(crate) doors: Mutex<Vec<crate::rehearsal::Door>>,
    /// The key each person's safety number is held at ([`crate::SafetyScript`]), per account,
    /// made from the account's script at the first ask (kept across actor restarts).
    safety: Mutex<HashMap<(AccountUuid, UserId), crate::safety::Held>>,
    /// Every `SetVerified` the actors heard, in order: (person, verified).
    verified: Mutex<Vec<(AccountUuid, UserId, bool)>>,
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
            changes: Mutex::new(Vec::new()),
            logins: Mutex::new(Vec::new()),
            login_flows: Mutex::new(Vec::new()),
            login_inputs: Mutex::new(Vec::new()),
            pair_attempts: Mutex::new(0),
            transmits: Mutex::new(Vec::new()),
            aborts: Mutex::new(Vec::new()),
            relinks: Mutex::new(HashMap::new()),
            progress: Mutex::new(HashMap::new()),
            durable: Mutex::new(Vec::new()),
            devices: Mutex::new(HashSet::new()),
            doors: Mutex::new(Vec::new()),
            safety: Mutex::new(HashMap::new()),
            verified: Mutex::new(Vec::new()),
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

    /// Runs `f` on the held safety state of `user` (made from `script` the first time);
    /// `None` when the script gives `user` no keys.
    pub(crate) fn with_safety<R>(
        &self,
        uuid: AccountUuid,
        script: &Script,
        user: &UserId,
        f: impl FnOnce(&mut crate::safety::Held) -> R,
    ) -> Option<R> {
        let mut held = lock(&self.safety);
        let key = (uuid, user.clone());
        if !held.contains_key(&key) {
            let entry = script.safety.iter().find(|s| s.user == *user)?;
            held.insert(
                key.clone(),
                crate::safety::Held::new(&entry.keys, entry.keyless_asks)?,
            );
        }
        held.get_mut(&key).map(f)
    }

    pub(crate) fn note_verified(&self, uuid: AccountUuid, user: UserId, verified: bool) {
        lock(&self.verified).push((uuid, user, verified));
        self.changed.notify_waiters();
    }

    /// The `SetVerified` commands the actors of `uuid` heard, in order: (person, verified).
    pub fn verified(&self, uuid: AccountUuid) -> Vec<(UserId, bool)> {
        lock(&self.verified)
            .iter()
            .filter(|(u, _, _)| *u == uuid)
            .map(|(_, user, v)| (user.clone(), *v))
            .collect()
    }

    pub(crate) fn change(&self, uuid: AccountUuid, change: ConversationChange) {
        lock(&self.changes).push((uuid, change));
        self.changed.notify_waiters();
    }

    /// The `ChangeConversation` changes the actors of `uuid` received, in the order they
    /// heard them (the network applies what it hears in that order).
    pub fn changes(&self, uuid: AccountUuid) -> Vec<ConversationChange> {
        lock(&self.changes)
            .iter()
            .filter(|(u, _)| *u == uuid)
            .map(|(_, c)| c.clone())
            .collect()
    }

    pub(crate) fn login(&self, uuid: AccountUuid, flow: String, taken: Vec<TakenIdentity>) {
        lock(&self.logins).push((uuid, taken));
        lock(&self.login_flows).push((uuid, flow));
        self.changed.notify_waiters();
    }

    /// The login flow ids (`"qr"`, `"pair-code"`) of every `Command::StartLogin` the actors of
    /// `uuid` received, in order.
    pub fn login_flows(&self, uuid: AccountUuid) -> Vec<String> {
        lock(&self.login_flows)
            .iter()
            .filter(|(u, _)| *u == uuid)
            .map(|(_, f)| f.clone())
            .collect()
    }

    pub(crate) fn login_input(&self, uuid: AccountUuid, values: LoginInput) {
        lock(&self.login_inputs).push((uuid, values));
        self.changed.notify_waiters();
    }

    /// The `(field id, value)` lists of every `Command::SubmitLoginInput` the actors of `uuid`
    /// received, in order.
    pub fn login_inputs(&self, uuid: AccountUuid) -> Vec<LoginInput> {
        lock(&self.login_inputs)
            .iter()
            .filter(|(u, _)| *u == uuid)
            .map(|(_, v)| v.clone())
            .collect()
    }

    /// Counts a `pair-code` login; the number of the ones before it.
    pub(crate) fn pair_attempt(&self) -> usize {
        let mut n = lock(&self.pair_attempts);
        let before = *n;
        *n += 1;
        before
    }

    /// The identities of the profile's other accounts that the core named in each
    /// `Command::StartLogin` the actors of `uuid` received, in order (what the backend may
    /// refuse a link for before it registers a device).
    pub fn login_taken(&self, uuid: AccountUuid) -> Vec<Vec<TakenIdentity>> {
        lock(&self.logins)
            .iter()
            .filter(|(u, _)| *u == uuid)
            .map(|(_, t)| t.clone())
            .collect()
    }

    /// The `(field id, value)` lists of every `Command::SubmitLoginInput` any actor received, in
    /// order (for a test that has no account's uuid at hand: a link that is still being made has
    /// no account the UI knows).
    pub fn all_login_inputs(&self) -> Vec<LoginInput> {
        lock(&self.login_inputs)
            .iter()
            .map(|(_, v)| v.clone())
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

    pub(crate) fn aborted(&self, uuid: AccountUuid, record: AbortRecord) {
        lock(&self.aborts).push((uuid, record));
        self.changed.notify_waiters();
    }

    /// The `AbortTransmit` commands of `uuid` the fake answered, in order.
    pub fn aborts(&self, uuid: AccountUuid) -> Vec<AbortRecord> {
        lock(&self.aborts)
            .iter()
            .filter(|(u, _)| *u == uuid)
            .map(|(_, a)| *a)
            .collect()
    }

    /// The transmits of `uuid` the fake answered, in order.
    pub fn transmits(&self, uuid: AccountUuid) -> Vec<TransmitRecord> {
        lock(&self.transmits)
            .iter()
            .filter(|(u, _)| *u == uuid)
            .map(|(_, t)| t.clone())
            .collect()
    }

    /// The frames this fake took that the WhatsApp run tool's budget counts (T2, D18: every
    /// send, react, edit, delete and admin revoke is one transmit, and a disappearing-timer
    /// set is one change), over all accounts. Receipts, typing and the read marker are not
    /// frames of that budget. The dev control's `state.frames` reports it in a rehearsal, so a
    /// run tool that sent nothing can SEE that the count stayed 0.
    pub fn frames(&self) -> u64 {
        let timers = lock(&self.changes)
            .iter()
            .filter(|(_, c)| matches!(c, ConversationChange::SetTimer { .. }))
            .count();
        u64::try_from(lock(&self.transmits).len() + timers).unwrap_or(u64::MAX)
    }
}

/// The scripted backend (§10.2). One instance serves every fake account; each account actor
/// runs on its own thread with a current-thread runtime, owns a real `backend.db` with
/// `core_outbox`, and forwards through `kourier-db`'s forwarder, like a real backend.
#[derive(Clone, Debug)]
pub struct FakeNetwork {
    flavor: Flavor,
    kind: NetworkKind,
    descriptor: NetworkDescriptor,
    control: Arc<FakeControl>,
}

impl FakeNetwork {
    /// A fake network of `flavor` whose accounts run `default` unless a script is pushed.
    pub fn new(flavor: Flavor, default: Script) -> Self {
        Self {
            flavor,
            kind: NetworkKind::Fake,
            descriptor: flavor.descriptor(),
            control: Arc::new(FakeControl::new(default)),
        }
    }

    /// A fake that PLAYS the network `kind` (WhatsApp, since TH4 Threema, since KT-1b Telegram):
    /// `kind()` says so, the descriptor is that network's as the UI sees it (name, its login flows,
    /// its history, experimental, a risk notice, no way to start a chat), and the message ids stay
    /// the Signal-like ones, so the SAME script gives the SAME chats as the plain fake. The ids of
    /// its accounts are the script's. WhatsApp has the `qr` and `pair-code` flows, history pushed
    /// on link and no archive transfer; Threema the `qr` flow alone, whose scan is followed by the
    /// emoji step ([`crate::FAKE_EMOJI`]), and no history; Telegram the `qr` flow (a scan is
    /// followed by the account's password, when its script has one) and the `phone` flow
    /// ([`crate::PhoneLoginScript`]), history on demand, and always-sent receipts. The fake that
    /// plays Telegram stops at once (no `shutdown_grace`, unlike the real backend).
    ///
    /// # Panics
    /// For a kind it cannot play.
    pub fn playing(kind: NetworkKind, default: Script) -> Self {
        let flavor = Flavor::SignalLike;
        Self {
            flavor,
            kind,
            descriptor: flavor.playing_descriptor(kind),
            control: Arc::new(FakeControl::new(default)),
        }
    }

    /// The fake that rehearses a real WhatsApp run (T1a, [`crate::rehearsal`]): WhatsApp as the
    /// UI sees it but WITHOUT the risk notice, a QR login of two short codes that honours the
    /// expected number tail like the real backend, a pushed history over about twenty seconds, a
    /// resumed account that goes online and replays nothing, and [`FakeControl::arrive`] for one
    /// message on cue. Every id and name is synthetic.
    ///
    /// # Panics
    /// Never: the script's ids are constants of a valid shape.
    pub fn rehearsal() -> Self {
        Self::rehearsal_with(crate::rehearsal::Timings::REAL)
    }

    /// [`rehearsal`](Self::rehearsal) with other waits (tests use milliseconds).
    ///
    /// # Panics
    /// Never: the script's ids are constants of a valid shape.
    pub fn rehearsal_with(timings: crate::rehearsal::Timings) -> Self {
        let script = crate::rehearsal::script(timings).expect("the rehearsal's ids are valid");
        let mut net = Self::playing(NetworkKind::WhatsApp, script);
        net.descriptor.risk_notice = None;
        net
    }

    /// The descriptor without its risk notice: a rehearsal's network has no warning page to tick
    /// (the warning page is the UI's and has tests of its own).
    pub(crate) fn without_risk_notice(mut self) -> Self {
        self.descriptor.risk_notice = None;
        self
    }

    /// States whether a fresh link of this network can bring the account's history as one
    /// archive the phone uploads (`NetworkDescriptor::history_transfer`): a UI then offers
    /// "Transfer History from Phone…" for its accounts.
    pub fn with_history_transfer(mut self, on: bool) -> Self {
        self.descriptor.history_transfer = on;
        self
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
        self.kind
    }

    fn descriptor(&self) -> &NetworkDescriptor {
        &self.descriptor
    }

    /// The flavour's id; the fake PLAYING WhatsApp ([`FakeNetwork::playing`], the fixture's WhatsApp and the
    /// rehearsal) gives WhatsApp's shape whatever its flavour (W5a D16: 22 characters, as the real backend).
    fn allocate_message_id(&self, _own: &[UserId], _conv: &ConvKey, now_ms: u64) -> AllocatedId {
        if self.kind == NetworkKind::WhatsApp {
            Flavor::WhatsAppLike.allocate(now_ms)
        } else {
            self.flavor.allocate(now_ms)
        }
    }

    /// The fake names the attachments of own messages like those it delivers
    /// ([`crate::scripts::attachment_name`]), so a scripted `AttachmentDeleted` can name one. The
    /// Telegram-like flavour names none before the send (its server assigns the message's id, so
    /// a name made from the allocated one would be stale after the re-key, KT-2): the trait's
    /// default.
    fn attachment_net_ids(&self, local: &str, index: usize) -> Vec<String> {
        if self.flavor == Flavor::TelegramLike {
            return Vec::new();
        }
        vec![crate::scripts::attachment_name(local, index)]
    }

    /// The plain Signal-like fake links a person by number as Signal does (`signal.me/#p/`);
    /// the other flavors and a fake PLAYING another network (WhatsApp) have no links (P5b).
    fn contact_links(&self) -> bool {
        self.flavor == Flavor::SignalLike && self.kind == NetworkKind::Fake
    }

    fn group_invites(&self) -> kourier_model::GroupInvites {
        self.flavor.group_invites(self.kind)
    }

    fn contact_link(&self, _user: &UserId, number: &str) -> Option<String> {
        (self.contact_links() && kourier_backend_api::is_e164(number))
            .then(|| format!("https://signal.me/#p/{number}"))
    }

    fn spawn(&self, ctx: AccountContext) -> Result<AccountHandle, BackendError> {
        let (script, pushed) = self.control.script_for_spawn(ctx.uuid, ctx.start);
        let control = self.control.clone();
        let flavor = self.flavor;
        let name = format!("fake-{}", ctx.uuid.short());
        let kind = self.kind;
        spawn_account_thread(name, move || {
            crate::actor::run(ctx, script, pushed, control, flavor, kind)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kourier_backend_api::OutgoingKind;
    use kourier_model::{Body, MessageId};
    use std::time::Duration;

    fn record(text: &str) -> TransmitRecord {
        TransmitRecord {
            message: MessageId(1),
            req: OutgoingRequest {
                conv: ConvKey::Direct(UserId::from_parts("pn", "999000481726").expect("id")),
                local: Arc::from("3EB0TEST"),
                sent_at: 1,
                kind: OutgoingKind::Message {
                    body: Body::text(text),
                    reply_to: None,
                    expiry: None,
                },
                only: None,
                is_recipient_update: false,
            },
            at_ms: 1,
            outcome: TransmitOutcome::Sent {
                server_at: None,
                per_recipient: Vec::new(),
            },
        }
    }

    /// T2 (C3): `frames` is what the run tool's budget counts: a transmit (send, react, edit,
    /// delete, revoke) and a timer set, over all accounts, and nothing else the actors hear
    /// (a pin, an archive, a mute, a read marker). It starts at 0.
    #[test]
    fn the_frames_are_the_transmits_and_the_timer_sets_and_nothing_else() {
        let control = FakeNetwork::rehearsal().control();
        let (one, two) = (AccountUuid::new_random(), AccountUuid::new_random());
        assert_eq!(control.frames(), 0);

        control.change(one, ConversationChange::SetPinned { pinned: true });
        control.change(one, ConversationChange::SetArchived { archived: true });
        control.change(one, ConversationChange::SetMuted { until: Some(1) });
        control.change(
            one,
            ConversationChange::SetMarkedUnread {
                marked_unread: true,
            },
        );
        assert_eq!(control.frames(), 0, "settings are not frames");

        control.transmitted(one, record("one"));
        control.transmitted(two, record("two"));
        assert_eq!(control.frames(), 2, "every account's transmits count");
        control.change(
            two,
            ConversationChange::SetTimer {
                timer: Some(Duration::from_secs(60)),
            },
        );
        assert_eq!(control.frames(), 3, "a timer set is a frame");
        control.change(two, ConversationChange::SetTimer { timer: None });
        assert_eq!(control.frames(), 4);
    }
}
