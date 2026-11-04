//! The join tests' harness (TH4 J3): a world of the fake with the phone's device group and a scripted
//! joining phone (`world.joiner(..)`), the actor on a FRESH file with the fake's parts and an allow-list
//! that names the group's ID (or none), and the actor's `FakeClock` that starts where the fake's clock
//! does. Both clocks move together in steps of at most 1 s, the fake's through
//! `net().advance_virtual(..)` (it ticks the relay's init timeout and the joiner's nomination timer too).
//! J5a: the CORE SIDE of a whole join (the durable envelopes, held or acked by the test), the published
//! `ConnState`, and the file's rows after the import. Nothing here prints a key, an ID, a QR text or a
//! label.

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{
    AccountContext, AccountHandle, Ack, BackendConfig, BackendEvent, Clock, Command, ContextParams,
    CoreEnds, DbKey, Envelope, Exit, FakeClock, Network, Relink, StartMode, TakenIdentity,
};
use kourier_model::{
    AccountId, AccountUuid, ConnState, DeviceLogout, ErrorKind, LoginStep, LoginStepKind,
    LogoutReport, PersistError, UserId,
};
use kourier_threema::store::Identity;
use kourier_threema::{AllowList, ThreemaNetwork};

use crate::support::mock::connector::Net;
use crate::support::mock::crypto::DeviceGroupKeys;
use crate::support::mock::essential::{Spec, essential};
use crate::support::mock::joiner::{JoinStep, Joiner, JoinerKnobs};
use crate::support::mock::relay::SideId;
use crate::support::mock::{self, MockWorld, START_MS, world};
use crate::support::{self, TEST_KEY, db_path, key};

/// The synthetic Threema ID of the phone's device group.
pub const GROUP: &str = "KOURIER1";
/// The host name the parts carry: the device label is `Kourier (j5a-host)`.
pub const HOST: &str = "j5a-host";
/// The upper bound of a wait for the actor (never a measure): a phone that hands over 32 MiB of blobs
/// takes ~7 s in a debug build (the fake seals at the test crate's opt-level).
const WALL: Duration = Duration::from_secs(60);
/// The most one step of both clocks moves (call C13).
const STEP_MS: u64 = 1_000;

/// The join's deadlines (decision C8), as the tests expect them.
pub const CONNECT_WITHIN: u64 = 20_000;
pub const ESSENTIAL_WITHIN: u64 = 2 * 60_000;
/// How long a frame written to the relay may take (P8).
pub const SEND_WITHIN: u64 = 1_000;
pub const SCAN_WITHIN: u64 = 5 * 60_000;
pub const NOMINATE_WITHIN: u64 = 60_000;
pub const BEGIN_WITHIN: u64 = 5 * 60_000;

/// What a join test starts with.
pub struct Setup {
    pub seed: u64,
    /// The phone's own behaviour.
    pub joiner: JoinerKnobs,
    /// What the phone hands over.
    pub spec: Spec,
    /// Whether the parts' allow-list names the group's ID (`false`: an EMPTY list).
    pub allowed: bool,
    /// Whether `StartLogin` names the group's ID as another account of the profile (`taken`).
    pub taken: bool,
    /// J5b: the `taken` entry says that other account is logged out.
    pub taken_logged_out: bool,
    /// J5b: the parts' allow-list holds exactly these IDs (`allowed` is then not read): a NON-empty
    /// list without the group's ID.
    pub listed: Option<&'static [&'static str]>,
    /// J5b: `StartLogin` is a RELINK of the account whose primary own ID is this one.
    pub relink: Option<&'static str>,
    /// TH5 A7b: the phone's chat state on the handed contacts and group ([`phone_chats`]).
    pub chats: bool,
}

/// The mute end the phone hands for the first contact (`chats`), ms.
pub const CHATS_MUTED_UNTIL: u64 = START_MS + 7 * 86_400_000;

/// What `Setup::chats` makes the phone hand over (TH5 A7b, his QB / QC / QA): the first contact's chat
/// ARCHIVED and muted until [`CHATS_MUTED_UNTIL`]; the second's PINNED, PROTECTED, the default notification
/// policy; the third contact HIDDEN (`GROUP_OR_DELETED`) with an archived chat; the first group PINNED,
/// PROTECTED, "only when mentioned".
fn phone_chats(data: &mut libthreema::protobuf::d2d_join::EssentialData) {
    use libthreema::protobuf::common::Unit;
    use libthreema::protobuf::d2d_sync::{self, ConversationCategory, ConversationVisibility};
    let contact = |data: &mut libthreema::protobuf::d2d_join::EssentialData, i: usize| {
        data.contacts[i].contact.take().expect("a contact")
    };
    let mut c = contact(data, 0);
    c.conversation_visibility = Some(ConversationVisibility::Archived as i32);
    {
        use d2d_sync::contact::notification_trigger_policy_override as o;
        c.notification_trigger_policy_override =
            Some(d2d_sync::contact::NotificationTriggerPolicyOverride {
                r#override: Some(o::Override::Policy(o::Policy {
                    policy: o::policy::NotificationTriggerPolicy::Never as i32,
                    expires_at: Some(CHATS_MUTED_UNTIL),
                })),
            });
    }
    data.contacts[0].contact = Some(c);
    let mut c = contact(data, 1);
    c.conversation_visibility = Some(ConversationVisibility::Pinned as i32);
    c.conversation_category = Some(ConversationCategory::Protected as i32);
    {
        use d2d_sync::contact::notification_trigger_policy_override as o;
        c.notification_trigger_policy_override =
            Some(d2d_sync::contact::NotificationTriggerPolicyOverride {
                r#override: Some(o::Override::Default(Unit {})),
            });
    }
    data.contacts[1].contact = Some(c);
    let mut c = contact(data, 2);
    c.acquaintance_level = Some(d2d_sync::contact::AcquaintanceLevel::GroupOrDeleted as i32);
    c.conversation_visibility = Some(ConversationVisibility::Archived as i32);
    data.contacts[2].contact = Some(c);
    let g = data.groups[0].group.as_mut().expect("a group");
    g.conversation_visibility = Some(ConversationVisibility::Pinned as i32);
    g.conversation_category = Some(ConversationCategory::Protected as i32);
    {
        use d2d_sync::group::notification_trigger_policy_override as o;
        g.notification_trigger_policy_override =
            Some(d2d_sync::group::NotificationTriggerPolicyOverride {
                r#override: Some(o::Override::Policy(o::Policy {
                    policy: o::policy::NotificationTriggerPolicy::Mentioned as i32,
                    expires_at: None,
                })),
            });
    }
}

impl Setup {
    /// A phone that does everything right at once, a small account, the ID on the list.
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            joiner: JoinerKnobs::default(),
            spec: Spec {
                contacts: 2,
                groups: 1,
                hashed_nonces: 2,
                profile_picture: false,
                bad: Vec::new(),
            },
            allowed: true,
            taken: false,
            taken_logged_out: false,
            listed: None,
            relink: None,
            chats: false,
        }
    }
}

/// A running actor on a fresh file, the fake and its joining phone.
pub struct J {
    pub world: MockWorld,
    pub joiner: Joiner,
    pub clock: FakeClock,
    /// The device group's keys (its path key at the mediator, DGDIK for the device info).
    pub keys: DeviceGroupKeys,
    /// The label the actor's network gives the device.
    pub label: String,
    /// The device group's key and chat server group (TH5: a second phone of the group reflects).
    pub dgk: [u8; 32],
    pub server_group: String,
    taken: Option<bool>,
    relink: Option<&'static str>,
    ends: CoreEnds,
    handle: AccountHandle,
    dir: tempfile::TempDir,
}

/// The actor of `setup` over a NEW file (`Fresh`): no identity, nothing linked.
pub fn start(setup: Setup) -> J {
    // An actor runs only in the offline child, whoever calls this.
    assert!(
        support::offline::in_child(),
        "an actor test outside the unshare child"
    );
    let world = world(setup.seed);
    let group = world.device_group(GROUP);
    let mut handed = essential(&world, &group, &setup.spec);
    if setup.chats {
        phone_chats(&mut handed.data);
    }
    let joiner = world.joiner(handed, setup.joiner);
    let dir = tempfile::tempdir().expect("temp dir");
    let clock = FakeClock::new(START_MS);
    let (ctx, ends) = AccountContext::new(ContextParams {
        account: AccountId(1),
        uuid: AccountUuid::new_random(),
        start: StartMode::Fresh,
        backend_db: db_path(dir.path()),
        db_key: DbKey::new(TEST_KEY),
        blob_dir: dir.path().join("blobs"),
        net_rt: tokio::runtime::Handle::current(),
        cancel: tokio_util::sync::CancellationToken::new(),
        clock: Arc::new(clock.clone()),
        config: BackendConfig::default(),
        ui: Default::default(),
        local_prefs: Default::default(),
    });
    let allowed = match (setup.listed, setup.allowed) {
        (Some(ids), _) => AllowList::from_ids(ids),
        (None, true) => AllowList::from_ids(&[GROUP]),
        (None, false) => AllowList::default(),
    };
    let keys = DeviceGroupKeys::derive(&group.dgk);
    let network = ThreemaNetwork::new(
        mock::parts(&world)
            .with_allowed(allowed)
            .with_host_name(HOST),
    );
    let label = network.device_label();
    let handle = network.spawn(ctx).expect("the actor starts");
    J {
        world,
        joiner,
        clock,
        keys,
        label,
        dgk: group.dgk,
        server_group: group.server_group.clone(),
        taken: setup.taken.then_some(setup.taken_logged_out),
        relink: setup.relink,
        ends,
        handle,
        dir,
    }
}

/// `StartLogin { "qr" }` of a fresh link.
pub fn start_login() -> Command {
    Command::StartLogin {
        flow: "qr".to_owned(),
        relink: None,
        taken: Vec::new(),
    }
}

impl J {
    pub fn net(&self) -> &Net {
        self.world.net()
    }

    pub fn now(&self) -> u64 {
        self.clock.now_ms()
    }

    pub async fn send(&self, command: Command) {
        self.ends
            .commands
            .send(command)
            .await
            .expect("the actor takes commands");
    }

    /// Sends a command that is answered, so every command before it was taken.
    pub async fn sync(&self) {
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.send(Command::ListOwnDevices { reply }).await;
        let answered = tokio::time::timeout(WALL, answer).await;
        assert!(
            matches!(answered, Ok(Ok(_))),
            "the actor did not answer in time"
        );
    }

    /// The next envelope of the actor (a wall BOUND): it must be ONE ephemeral login step.
    pub async fn step(&mut self) -> LoginStep {
        let envelope = tokio::time::timeout(WALL, self.ends.events.recv())
            .await
            .expect("the actor sent a step in time")
            .expect("the actor's events channel is open");
        assert!(envelope.ack.is_none(), "a login step was not ephemeral");
        assert_eq!(envelope.events.len(), 1, "a step envelope holds one event");
        match envelope.events.into_iter().next() {
            Some(BackendEvent::Login(step)) => step,
            _ => panic!("the actor sent something other than a login step"),
        }
    }

    /// The next step, which must be `id` with `txn`.
    pub async fn expect_step(&mut self, id: &str, txn: u64) -> LoginStep {
        let step = self.step().await;
        assert_eq!(step.step_id, id, "the step is not the expected one");
        assert_eq!(step.txn, txn, "the step's txn does not rise by one");
        step
    }

    /// The `qr` step (txn 1): one code that does not rotate; its text.
    pub async fn qr(&mut self) -> String {
        let step = self.expect_step("qr", 1).await;
        match step.kind {
            LoginStepKind::ShowQr { data, valid_for } => {
                assert_eq!(valid_for, None, "the Threema code was said to rotate");
                data.expose().clone()
            }
            _ => panic!("the qr step shows no code"),
        }
    }

    /// The next step must fail at `id` with `kind` (txn `txn`).
    pub async fn expect_failed(&mut self, id: &str, txn: u64, kind: &ErrorKind) {
        let step = self.expect_step(id, txn).await;
        match step.kind {
            LoginStepKind::Failed { error } => {
                assert_eq!(&error.kind, kind, "the login failed with another kind");
            }
            _ => panic!("the step at {id} is not a failure"),
        }
    }

    /// No envelope waits (after a `sync`).
    pub async fn no_more_steps(&mut self) {
        self.sync().await;
        assert!(
            self.ends.events.try_recv().is_err(),
            "the actor sent one more envelope"
        );
    }

    /// The phone scans `text` and the fake hands on what it sent.
    pub fn scan(&self, text: &str) {
        assert!(self.joiner.scan(text).is_ok(), "the phone could not scan");
        self.net().flush();
    }

    /// Starts the login (with the group's ID as `taken`, and as a relink, when the setup says so) and
    /// returns the QR text.
    pub async fn shown(&mut self) -> String {
        let user = |id: &str| UserId::from_parts("threema", id).expect("a valid id");
        let taken = self
            .taken
            .map(|logged_out| TakenIdentity {
                user: user(GROUP),
                logged_out,
            })
            .into_iter()
            .collect();
        let relink = self.relink.map(|id| Relink {
            account: AccountId(1),
            expect_own: vec![user(id)],
        });
        self.send(Command::StartLogin {
            flow: "qr".to_owned(),
            relink,
            taken,
        })
        .await;
        self.qr().await
    }

    /// The next envelope of the actor, ephemeral or durable (a wall BOUND).
    pub async fn envelope(&mut self) -> Envelope {
        tokio::time::timeout(WALL, self.ends.events.recv())
            .await
            .expect("the actor sent an envelope in time")
            .expect("the actor's events channel is open")
    }

    /// The next envelope, which must be DURABLE: its events and its ack (held by the caller).
    pub async fn durable(&mut self) -> (Vec<BackendEvent>, Ack) {
        let envelope = self.envelope().await;
        let ack = envelope
            .ack
            .expect("the core side got an ephemeral envelope where a durable one was due");
        (envelope.events, ack)
    }

    /// The core side REFUSES the envelope for good (a contract violation: `retryable: false`). The
    /// forwarder then ships the rows of that batch one by one, so the next envelopes are single rows.
    pub fn refuse(ack: Ack) {
        ack.complete(Err(PersistError {
            retryable: false,
            detail: "refused by the test".to_owned(),
        }));
    }

    /// The core side refuses the envelope for good with `detail` (what a core's non-login refusal says, e.g.
    /// "json"; the actor never reads it).
    pub fn refuse_as(ack: Ack, detail: &str) {
        ack.complete(Err(PersistError {
            retryable: false,
            detail: detail.to_owned(),
        }));
    }

    /// Zone 2's first batch (all of its rows in ONE envelope) is refused for good, so the rows come one
    /// by one: returns row 1 alone (`AccountReady` first, then the settings) and its ack, held.
    pub async fn row_one(&mut self) -> (Vec<BackendEvent>, Ack) {
        let (all, ack) = self.durable().await;
        assert!(
            matches!(all.first(), Some(BackendEvent::AccountReady { .. })),
            "the core side's first event is not AccountReady"
        );
        Self::refuse(ack);
        let (one, ack) = self.durable().await;
        assert!(
            matches!(one.first(), Some(BackendEvent::AccountReady { .. })) && one.len() < all.len(),
            "row 1 did not come alone"
        );
        (one, ack)
    }

    /// The next envelope must be the ephemeral `done` step with `txn`.
    pub async fn expect_done(&mut self, txn: u64) {
        let step = self.expect_step("done", txn).await;
        assert!(
            matches!(step.kind, LoginStepKind::Done),
            "the done step is not Done"
        );
    }

    /// Spoils the stored contacts (their `sync` BLOBs no longer decode) through a second connection to the
    /// file, so the next read of the joined account fails (P4, T1).
    pub fn spoil_contacts(&self) {
        let db =
            kourier_threema::store::open(&db_path(self.dir.path()), &key()).expect("open the file");
        db.zone(|tx| {
            tx.execute("UPDATE contact SET sync = X'ff'", [])
                .map_err(kourier_db::Error::from)
        })
        .expect("spoil the contacts");
    }

    /// The core side of the end of a join: every durable envelope is acked and its events kept,
    /// until the ephemeral `done` step (txn `txn`) comes; the events in order.
    pub async fn ack_until_done(
        &mut self,
        mut got: Vec<BackendEvent>,
        txn: u64,
    ) -> Vec<BackendEvent> {
        loop {
            let envelope = self.envelope().await;
            match envelope.ack {
                Some(ack) => {
                    got.extend(envelope.events);
                    ack.complete(Ok(()));
                }
                None => {
                    let step = match envelope.events.as_slice() {
                        [BackendEvent::Login(step)] => step.clone(),
                        _ => panic!("an ephemeral envelope that is no login step"),
                    };
                    assert_eq!(
                        step.step_id, "done",
                        "the step after the import is not done"
                    );
                    assert_eq!(step.txn, txn, "the done step's txn does not rise by one");
                    assert!(
                        matches!(step.kind, LoginStepKind::Done),
                        "the done step is not Done"
                    );
                    return got;
                }
            }
        }
    }

    /// The state the actor published last.
    pub fn state(&self) -> ConnState {
        self.ends.conn.borrow().clone()
    }

    /// Waits (a wall BOUND) until the published state is `what`.
    pub async fn wait_state(&mut self, what: &str, done: impl Fn(&ConnState) -> bool) {
        let got = tokio::time::timeout(WALL, self.ends.conn.wait_for(|s| done(s)))
            .await
            .ok()
            .and_then(Result::ok)
            .is_some();
        assert!(
            got,
            "the state never became {what}; it is {:?}",
            self.state()
        );
    }

    /// The identity row of the account's file (a second reader of the WAL file).
    pub fn file_identity(&self) -> Option<Identity> {
        let db =
            kourier_threema::store::open(&db_path(self.dir.path()), &key()).expect("open the file");
        db.read(kourier_threema::store::load_identity)
            .expect("read the identity")
    }

    /// The account's `backend.db` (a second reader may open it, e.g. in the fake's ack hook).
    pub fn db_file(&self) -> std::path::PathBuf {
        db_path(self.dir.path())
    }

    /// The rows of `table` in the account's file.
    pub fn file_rows(&self, table: &str) -> i64 {
        let db =
            kourier_threema::store::open(&db_path(self.dir.path()), &key()).expect("open the file");
        db.read(|conn: &kourier_db::rusqlite::Connection| {
            conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .map_err(kourier_db::Error::from)
        })
        .expect(table)
    }

    /// Waits (a wall BOUND) until `table` of the account's file holds `rows` rows. For a table the
    /// FORWARDER empties: it deletes an acked `core_outbox` row in a task of its own, after the core's
    /// ack was completed, so a read right after `Ack::complete` (or after anything that only follows the
    /// refusal of another row) can still see the row. A `sync` does not wait for that task; `Done`
    /// does (it follows the forwarder's `acked`, which is published after the delete).
    pub async fn wait_rows(&self, table: &str, rows: i64) {
        let got = tokio::time::timeout(WALL, async {
            while self.file_rows(table) != rows {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(
            got.is_ok(),
            "the table {table} never held {rows} rows; it holds {}",
            self.file_rows(table)
        );
    }

    /// What the file states to the core (zone 2's rows, as a `Rebuild` states them).
    pub fn file_states(&self) -> Vec<BackendEvent> {
        let db =
            kourier_threema::store::open(&db_path(self.dir.path()), &key()).expect("open the file");
        db.read(kourier_threema::store::stated_events)
            .expect("the file states its events")
            .into_iter()
            .flatten()
            .collect()
    }

    /// Starts the login, scans, and waits for the `emoji` step (txn 2): its glyphs.
    pub async fn nominated(&mut self) -> Vec<String> {
        let qr = self.shown().await;
        self.scan(&qr);
        match self.expect_step("emoji", 2).await.kind {
            LoginStepKind::ConfirmEmoji { emoji } => emoji,
            _ => panic!("the emoji step shows no emoji"),
        }
    }

    /// Waits (a wall BOUND) until the phone reached `step` (the actor answers on its own thread).
    pub async fn wait_joiner(&self, step: JoinStep) {
        let net = self.net();
        assert!(
            net.wait(|| self.joiner.step() == step, WALL).await,
            "the phone never reached {step:?} (it is at {:?})",
            self.joiner.step()
        );
    }

    /// The relay side the connector opened for the actor (the one rendezvous socket).
    pub fn side(&self) -> SideId {
        let opened = self.net().relay_opened();
        assert_eq!(opened.len(), 1, "not exactly one rendezvous socket");
        opened[0]
    }

    /// The code the actor's rendezvous socket ended with.
    pub fn rendezvous_closed(&self) -> Option<u16> {
        self.world.relay().closed(self.side())
    }

    /// No WebSocket but the one rendezvous socket was asked for (no Mediator request), and the
    /// transports were made at most once.
    pub fn only_the_rendezvous(&self) {
        let net = self.net();
        assert!(net.opened().is_empty(), "a mediator connection was opened");
        assert_eq!(
            net.attempts(),
            net.relay_opened().len(),
            "a connect other than the rendezvous socket was asked for"
        );
        assert!(net.made() <= 1, "the transports were made more than once");
    }

    /// Whether the account's file holds an identity (a second reader of the WAL file).
    pub fn identity_exists(&self) -> bool {
        let db =
            kourier_threema::store::open(&db_path(self.dir.path()), &key()).expect("open the file");
        db.read(kourier_threema::store::load_identity)
            .expect("read the identity")
            .is_some()
    }

    /// Waits (a wall BOUND) until the actor waits for `deadline` on its clock.
    pub async fn wait_deadline(&self, deadline: u64) {
        let armed = tokio::time::timeout(WALL, async {
            while !self.clock.deadlines().contains(&deadline) {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            armed.is_ok(),
            "the actor never waited for +{} ms (it waits for {:?})",
            deadline.saturating_sub(START_MS),
            self.clock
                .deadlines()
                .iter()
                .map(|d| d.saturating_sub(START_MS))
                .collect::<Vec<_>>()
        );
    }

    /// Waits until the actor waits for `deadline`, nothing before it, then moves BOTH clocks there
    /// in steps of at most 1 s (an EXPECTED deadline, never "any sleep").
    pub async fn advance_to(&self, deadline: u64) {
        self.wait_deadline(deadline).await;
        assert!(
            self.clock.deadlines().iter().all(|d| *d >= deadline),
            "the actor waits for something before the expected deadline"
        );
        self.step_to(deadline).await;
    }

    /// Moves BOTH clocks to `target` in steps of at most 1 s: the fake's virtual clock first (its
    /// relay and joiner timers run), then the actor's.
    pub async fn step_to(&self, target: u64) {
        while self.now() < target {
            let step = (target - self.now()).min(STEP_MS);
            self.net().advance_virtual(Duration::from_millis(step));
            self.clock.set(self.now() + step);
            tokio::task::yield_now().await;
        }
    }

    async fn ended(&mut self, expected: Exit) {
        let exited = tokio::time::timeout(WALL, &mut self.handle.exited)
            .await
            .expect("the actor ends in time");
        assert_eq!(Exit::from_channel(exited), expected);
        if let Some(thread) = self.handle.thread.take() {
            thread.join().expect("the actor's thread joins");
        }
    }

    /// Shutdown: `Exit::Shutdown`, a rendezvous socket still open closed with 1000; then the fake's
    /// violations are empty (the LAST check).
    pub async fn finish(mut self) {
        let open = self.net().relay_opened();
        self.send(Command::Shutdown).await;
        self.ended(Exit::Shutdown).await;
        for side in open {
            assert!(
                self.world.relay().closed(side).is_some(),
                "a rendezvous socket stayed open after the shutdown"
            );
        }
        self.world.violations().assert_empty();
    }

    /// The core's `Logout` after a cancelled link: no device was linked (`NoDevice`), the actor ends
    /// logged out; then the fake's violations are empty (the LAST check).
    pub async fn finish_logged_out(self) {
        self.finish_logout(DeviceLogout::NoDevice, None).await;
    }

    /// The core's `Logout`: it reports `outcome` for the device named `device_name`, the actor ends
    /// logged out; then the fake's violations are empty (the LAST check).
    pub async fn finish_logout(mut self, outcome: DeviceLogout, device_name: Option<&str>) {
        self.logout(outcome, device_name).await;
        self.world.violations().assert_empty();
    }

    /// RP-b: the core's `Logout` as [`J::finish_logout`] has it, WITHOUT the violations check, so a test can look at
    /// the mediator afterwards (its slots, the drops it took); the test then ends with the violations check itself.
    pub async fn logout(&mut self, outcome: DeviceLogout, device_name: Option<&str>) {
        self.send(Command::Logout).await;
        self.reported(outcome, device_name).await;
    }

    /// RP-b: the report of a `Logout` already sent (a test that moves the clock between the command and the report), and
    /// the actor's end logged out: as [`J::logout`] has them after the command.
    pub async fn reported(&mut self, outcome: DeviceLogout, device_name: Option<&str>) {
        let envelope = tokio::time::timeout(WALL, self.ends.events.recv())
            .await
            .expect("the logout is reported in time")
            .expect("the actor's events channel is open");
        let want = LogoutReport {
            outcome,
            device_name: device_name.map(str::to_owned),
        };
        assert!(
            matches!(envelope.events.as_slice(), [BackendEvent::Logout(got)] if *got == want),
            "the logout did not report what the test expects of the device"
        );
        self.ended(Exit::LoggedOut {
            reason: "logged out on request".to_owned(),
        })
        .await;
    }

    /// The actor ended BY ITSELF with `exit` (a logout verdict of the connection); then the fake's
    /// violations are empty (the LAST check).
    pub async fn finish_ended(mut self, exit: Exit) {
        self.ended(exit).await;
        self.world.violations().assert_empty();
    }
}
