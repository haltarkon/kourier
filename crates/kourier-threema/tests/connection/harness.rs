//! The actor tests' harness: a world of the fake with this device's identity and group, the
//! `backend.db` seeded with the identity, the actor on the fake's parts and a `FakeClock` that starts
//! where the fake's clock does. Nothing here prints a key, an ID or a label.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{
    AccountContext, AccountHandle, BackendConfig, BackendEvent, Clock, Command, ContextParams,
    CoreEnds, DbKey, Envelope, Exit, FakeClock, IncomingMessage, Network, OutgoingRequest,
    StartMode, TransmitOutcome,
};
use kourier_db::rusqlite::params;
use kourier_model::{AccountId, AccountUuid, ConnState, LogoutReport};
use kourier_threema::ThreemaNetwork;
use kourier_threema::store::{Identity, NewJob, SlotState, mark_join_ready, save_identity};
use libthreema::protobuf::d2d_sync;
use prost::Message as _;
use tokio::sync::oneshot;
use zeroize::Zeroizing;

use crate::support::mock::connector::Net;
use crate::support::mock::crypto::DeviceGroupKeys;
use crate::support::mock::d2m::ConnId;
use crate::support::mock::envelope::{ContactDelta, GroupDelta};
use crate::support::mock::{self, MockWorld, START_MS, TestIdentity, world};
use crate::support::{self, TEST_KEY, db_path, key};

/// The synthetic identity of these tests.
pub const ME: &str = "KOURIER3";
/// This device's D2M id (above `i64::MAX`, as real ids can be).
pub const DEVICE: u64 = 0xF333_0000_0000_0003;
/// The stored label.
pub const LABEL: &str = "Kourier (th3-test)";
/// The default server group of the fake's tests (the mediator's charset).
pub const SERVER_GROUP: &str = "ds7";
/// The upper bound of a wait for the actor (never a measure).
const WALL: Duration = Duration::from_secs(10);
/// The most one step of both clocks moves (call C13).
const STEP_MS: u64 = 1_000;

/// A world with this device's identity in the chat server's table and its group at the mediator.
pub struct Scene {
    pub world: MockWorld,
    pub dgk: [u8; 32],
    pub keys: DeviceGroupKeys,
    pub me: TestIdentity,
    pub cookie: [u8; 16],
    pub csp_device: u64,
    /// The slot state the file says.
    pub slot: SlotState,
    /// The contacts the seeded file holds (`contact` rows; TH5 A6: a message of anyone else is dropped).
    pub contacts: Vec<String>,
    /// The groups the seeded file holds (`grp` rows): creator and group id.
    pub groups: Vec<(String, u64)>,
    /// The `deferred` rows the seeded file holds (TH5 A8): their `received_at` and plaintext, version
    /// [`Scene::deferred_version`], kind `envelope`, source `reflected`.
    pub deferred: Vec<(i64, Vec<u8>)>,
    /// The `map_version` of the seeded `deferred` rows: 1 (what TH5a deferred; the start of a version-2 build
    /// maps them) unless a test wants rows of the CURRENT version (2: the start leaves them, TH5b B1).
    pub deferred_version: u32,
    /// After the seed, `store::remap_with(version)` over the file (TH5 A8: rows of an older version mapped as a
    /// newer build's start would), so the actor's forwarder ships what the re-map wrote.
    pub remap_to: Option<u32>,
    /// TH6a S3: the real client key (CK public) of a contact the seeded file holds, by ID: the world
    /// identity's key (`Scene::with_peer`), so a box Kourier seals to it opens at the fake's peer. A
    /// contact without an entry gets the dummy key `[7; 32]` of the receive tests.
    pub contact_keys: std::collections::BTreeMap<String, [u8; 32]>,
    /// TH6a S3: the members of a seeded group (`Scene::with_members`), by (creator, group id). A group
    /// without an entry has none.
    pub members: std::collections::BTreeMap<(String, u64), Vec<String>>,
    /// TH6a S3: the nickname of the seeded `own_profile` row (`Scene::with_nickname`); `None`: no row.
    pub nickname: Option<String>,
    /// TH6a S3: the seeded `settings` row (`Scene::with_settings`); `None`: no row.
    pub settings: Option<d2d_sync::Settings>,
    /// TH6a S5a: the jobs of the seeded file (`Scene::job`), in queue order.
    pub jobs: Vec<NewJob>,
    /// TH6a S6: the actor's parts open the send core's door (`Scene::send_open`).
    pub send_open: bool,
    /// TH6 L5: a seeded contact's nickname and whether it is only a group's (`GROUP_OR_DELETED`), by ID
    /// (`Scene::with_contact_as`); a contact without an entry has no nickname and is direct.
    pub contact_shapes: std::collections::BTreeMap<String, (Option<String>, bool)>,
}

pub fn scene(seed: u64) -> Scene {
    let world = world(seed);
    let me = world.add_identity(ME);
    let dgk = world.new_device_group_key();
    let keys = DeviceGroupKeys::derive(&dgk);
    world.mediator().add_group(keys.path_public, SERVER_GROUP);
    let cookie = world.random();
    let csp_device = world.random_u64();
    Scene {
        world,
        dgk,
        keys,
        me,
        cookie,
        csp_device,
        slot: SlotState::New,
        contacts: Vec::new(),
        groups: Vec::new(),
        deferred: Vec::new(),
        deferred_version: 1,
        remap_to: None,
        contact_keys: std::collections::BTreeMap::new(),
        members: std::collections::BTreeMap::new(),
        nickname: None,
        settings: None,
        jobs: Vec::new(),
        send_open: false,
        contact_shapes: std::collections::BTreeMap::new(),
    }
}

impl Scene {
    /// The file will hold these contacts.
    pub fn with_contacts(mut self, ids: &[&str]) -> Self {
        self.contacts.extend(ids.iter().map(|id| (*id).to_owned()));
        self
    }

    /// The file will hold this group (its creator should be a contact).
    pub fn with_group(mut self, creator: &str, group_id: u64) -> Self {
        self.groups.push((creator.to_owned(), group_id));
        self
    }

    /// TH6a S3: `id` is a contact of the seeded file AND an identity of the world with a real client key,
    /// so what Kourier seals to it opens at `world.peer(id)`. The identity is added to the world's table
    /// when it is not there yet (a second call, or a world that added it, changes nothing).
    #[track_caller]
    pub fn with_peer(mut self, id: &str) -> Self {
        let identity = match self.world.identity(id) {
            Some(known) => known,
            None => self.world.add_identity(id),
        };
        if !self.contacts.iter().any(|c| c == id) {
            self.contacts.push(id.to_owned());
        }
        self.contact_keys.insert(id.to_owned(), identity.ck.public);
        self
    }

    /// TH6a S3: the file will hold this group with these members. Every member (and the creator, unless it
    /// is this device's own ID) becomes a peer ([`Scene::with_peer`]), so a group message to them opens at
    /// their peers. The member list is the protocol's: the creator belongs in `members` when the creator is
    /// another user.
    #[track_caller]
    pub fn with_members(mut self, creator: &str, group_id: u64, members: &[&str]) -> Self {
        for id in members.iter().copied().chain([creator]) {
            if id != ME {
                self = self.with_peer(id);
            }
        }
        if !self
            .groups
            .iter()
            .any(|(c, g)| c == creator && *g == group_id)
        {
            self.groups.push((creator.to_owned(), group_id));
        }
        self.members.insert(
            (creator.to_owned(), group_id),
            members.iter().map(|id| (*id).to_owned()).collect(),
        );
        self
    }

    /// TH6 L5: the seeded contact `id` (a peer, [`Scene::with_peer`]) has this nickname and, when `hidden`, the
    /// acquaintance level `GROUP_OR_DELETED`.
    #[track_caller]
    pub fn with_contact_as(mut self, id: &str, nickname: Option<&str>, hidden: bool) -> Self {
        self = self.with_peer(id);
        self.contact_shapes
            .insert(id.to_owned(), (nickname.map(str::to_owned), hidden));
        self
    }

    /// TH6a S3: the file will hold an `own_profile` row with this nickname (what an outgoing message
    /// distributes when its type asks for the user profile).
    pub fn with_nickname(mut self, nickname: &str) -> Self {
        self.nickname = Some(nickname.to_owned());
        self
    }

    /// TH6a S3: the file will hold this `settings` row (the block list and the policies the send steps read).
    pub fn with_settings(mut self, settings: d2d_sync::Settings) -> Self {
        self.settings = Some(settings);
        self
    }

    /// TH6a S5a: the file will hold this job, sealed by the door a later step's receive zone calls, at the time of the
    /// world's start. Its conversation must be in the file: call this after `with_peer` and `with_members`.
    pub fn job(mut self, job: NewJob) -> Self {
        self.jobs.push(job);
        self
    }

    /// TH6a S6: the actor's parts open the send core's door, so a `Transmit` reaches it.
    pub fn send_open(mut self) -> Self {
        self.send_open = true;
        self
    }

    fn identity(&self) -> Identity {
        Identity {
            threema_id: ME.to_owned(),
            client_key: Zeroizing::new(self.me.ck.secret),
            server_group: SERVER_GROUP.to_owned(),
            device_group_key: Zeroizing::new(self.dgk),
            device_cookie: Zeroizing::new(self.cookie),
            d2m_device_id: DEVICE,
            csp_device_id: self.csp_device,
            slot_state: self.slot,
            device_label: LABEL.to_owned(),
            linked_at_ms: i64::try_from(START_MS).expect("ms fit i64"),
        }
    }

    /// Makes the account's file in `dir` as `start` seeds it (TH6a S3: public so the seed itself can be
    /// tested without an actor).
    pub fn seed_file(&self, dir: &Path) {
        let db = kourier_threema::store::open(&db_path(dir), &key()).expect("make the file");
        // A linked file: the identity AND zone 2's marker (S4: an identity alone is an unfinished join).
        db.zone(|tx| {
            save_identity(tx, &self.identity())?;
            mark_join_ready(tx, 1)
        })
        .expect("save the identity");
        // The contacts and groups a join would have imported: rows the receive path looks up (A6).
        db.zone(|tx| {
            for id in &self.contacts {
                let key = self.contact_keys.get(id).copied().unwrap_or([7; 32]);
                let mut delta = ContactDelta::create(id, key, START_MS);
                if let Some((nickname, hidden)) = self.contact_shapes.get(id) {
                    if let Some(nickname) = nickname {
                        delta = delta.nickname(nickname);
                    }
                    if *hidden {
                        delta = delta.hidden();
                    }
                }
                let sync = delta.build();
                tx.execute(
                    "INSERT INTO contact (identity, sync, last_update_at) VALUES (?1, ?2, NULL)",
                    params![id, sync.encode_to_vec()],
                )?;
            }
            for (creator, group_id) in &self.groups {
                let members: Vec<&str> = self
                    .members
                    .get(&(creator.clone(), *group_id))
                    .map(|ids| ids.iter().map(String::as_str).collect())
                    .unwrap_or_default();
                let sync = GroupDelta::create(
                    creator,
                    *group_id,
                    "synthetic group",
                    &members,
                    START_MS,
                )
                .build();
                tx.execute(
                    "INSERT INTO grp (creator, group_id, sync, last_update_at) VALUES (?1, ?2, ?3, 0)",
                    params![creator, i64::from_ne_bytes(group_id.to_ne_bytes()), sync.encode_to_vec()],
                )?;
            }
            // TH6a S3: the own profile and the shared settings the send steps read.
            if let Some(nickname) = &self.nickname {
                let profile = d2d_sync::UserProfile {
                    nickname: Some(nickname.clone()),
                    ..Default::default()
                };
                tx.execute(
                    "INSERT INTO own_profile (id, sync) VALUES (1, ?1)",
                    params![profile.encode_to_vec()],
                )?;
            }
            if let Some(settings) = &self.settings {
                tx.execute(
                    "INSERT INTO settings (id, sync) VALUES (1, ?1)",
                    params![settings.encode_to_vec()],
                )?;
            }
            Ok::<(), kourier_db::Error>(())
        })
        .expect("seed the contacts and groups");
        for (received_at, envelope) in &self.deferred {
            let row = kourier_threema::store::NewDeferred {
                source: kourier_threema::store::DeferredSource::Reflected,
                kind: "envelope",
                map_version: self.deferred_version,
                server_at: START_MS,
                flags: 0,
                envelope,
            };
            let wide = kourier_threema::store::DeferredBounds {
                rows: i64::MAX,
                bytes: i64::MAX,
                age_ms: i64::MAX,
            };
            kourier_threema::store::add_deferred_batch(&db, &[row], *received_at, &wide)
                .expect("seed a deferred row");
        }
        for job in &self.jobs {
            let now = i64::try_from(START_MS).expect("ms fit i64");
            db.zone(|tx| kourier_threema::store::enqueue_job(tx, job, now))
                .expect("seed a job");
        }
        if let Some(version) = self.remap_to {
            let wake = std::rc::Rc::new(tokio::sync::Notify::new());
            let now = i64::try_from(START_MS).expect("ms fit i64");
            kourier_threema::store::remap_with(&db, version, now, &wake).expect("the re-map");
        }
    }
}

/// A running actor on the fake.
pub struct H {
    pub s: Scene,
    pub clock: FakeClock,
    ends: CoreEnds,
    handle: AccountHandle,
    uuid: AccountUuid,
    dir: tempfile::TempDir,
}

/// The actor of `s` over a file that holds its identity (`Resume`).
pub fn start(s: Scene) -> H {
    begin(s, true)
}

/// The actor of `s` over a NEW file without an identity (`Fresh`, before TH4's link).
pub fn start_fresh(s: Scene) -> H {
    begin(s, false)
}

/// TH5b R17 (N-3): the actor of `s` over its seeded file (`Resume`), started with the core's cancel token
/// ALREADY cancelled (a shutdown that came while the core started it): the start's re-map must not run a page,
/// and the actor ends `Shutdown` by itself (`H::wait_shutdown`).
pub fn start_cancelled(s: Scene) -> H {
    let dir = tempfile::tempdir().expect("temp dir");
    s.seed_file(dir.path());
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    begin_with(s, dir, true, AccountUuid::new_random(), START_MS, cancel)
}

fn begin(s: Scene, with_identity: bool) -> H {
    let dir = tempfile::tempdir().expect("temp dir");
    if with_identity {
        s.seed_file(dir.path());
    }
    begin_in(s, dir, with_identity, AccountUuid::new_random(), START_MS)
}

fn begin_in(
    s: Scene,
    dir: tempfile::TempDir,
    with_identity: bool,
    uuid: AccountUuid,
    now_ms: u64,
) -> H {
    let cancel = tokio_util::sync::CancellationToken::new();
    begin_with(s, dir, with_identity, uuid, now_ms, cancel)
}

fn begin_with(
    s: Scene,
    dir: tempfile::TempDir,
    with_identity: bool,
    uuid: AccountUuid,
    now_ms: u64,
    cancel: tokio_util::sync::CancellationToken,
) -> H {
    // Critic M2: an actor runs only in the offline child, whoever calls this.
    assert!(
        support::offline::in_child(),
        "an actor test outside the unshare child"
    );
    let clock = FakeClock::new(now_ms);
    let (ctx, ends) = AccountContext::new(ContextParams {
        account: AccountId(1),
        uuid,
        start: if with_identity {
            StartMode::Resume
        } else {
            StartMode::Fresh
        },
        backend_db: db_path(dir.path()),
        db_key: DbKey::new(TEST_KEY),
        blob_dir: dir.path().join("blobs"),
        net_rt: tokio::runtime::Handle::current(),
        cancel,
        clock: Arc::new(clock.clone()),
        config: BackendConfig::default(),
        ui: Default::default(),
        local_prefs: Default::default(),
    });
    let network = if s.send_open {
        ThreemaNetwork::new(mock::parts(&s.world).with_send_open())
    } else {
        ThreemaNetwork::new(mock::parts(&s.world))
    };
    let handle = network.spawn(ctx).expect("the actor starts");
    H {
        s,
        clock,
        ends,
        handle,
        uuid,
        dir,
    }
}

impl H {
    pub fn net(&self) -> &Net {
        self.s.world.net()
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
        let (reply, answer) = oneshot::channel();
        self.send(Command::ListOwnDevices { reply }).await;
        let answered = tokio::time::timeout(WALL, answer).await;
        assert!(
            matches!(answered, Ok(Ok(_))),
            "the actor did not answer in time"
        );
    }

    pub fn state(&self) -> ConnState {
        self.ends.conn.borrow().clone()
    }

    /// Waits (a wall BOUND) until the published state is `what`.
    pub async fn wait_state(&mut self, what: &str, done: impl Fn(&ConnState) -> bool) -> ConnState {
        let got = tokio::time::timeout(WALL, self.ends.conn.wait_for(|s| done(s)))
            .await
            .ok()
            .and_then(Result::ok)
            .map(|state| state.clone());
        match got {
            Some(state) => state,
            None => panic!("the state never became {what}; it is {:?}", self.state()),
        }
    }

    /// The fake's connection of this device that finished the handshake.
    pub fn conn(&self) -> ConnId {
        self.net()
            .connected(DEVICE)
            .expect("this device has a connection")
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

    /// Waits (a wall BOUND) until the actor is PARKED, and returns what it waits for on its clock (earliest first):
    /// exactly one timer of its own (the socket's select, the handshake's, or the pause between two sockets) and, beside
    /// it, the bound of each directory lookup that pends at the fake ([`Net::directory_pending`]). WHY: [`H::sync`]
    /// alone is no barrier for the connection. The actor's command loop is the MAIN future of its `LocalSet`, which
    /// tokio polls BEFORE the spawned connection task, so `sync` can be answered while a wake the clock gave the
    /// connection is still queued and its timer not armed again; a list read then held only a pending lookup's bound
    /// (30 s ahead), and moving the clocks there skipped the keep-alive (the fake's chat server closes a socket silent
    /// for 30 s with 4000: TH6's post-rebase strike of `leader_contacts`). A SECOND `sync` is answered only in the
    /// `LocalSet`'s next round, after the round that ran every task queued before the first one, so the reads around
    /// it see no wake of the clock still unhandled; the two reads must agree (a pass that was running during the first
    /// shows a transient timer: `Session::close`'s unbiased 1 s bound) and hold one timer plus the pending lookups'.
    /// A test that reads the next deadline from the list instead of naming it ([`H::advance_to`] with an expected
    /// time) reads it here.
    pub async fn wait_parked(&self) -> Vec<u64> {
        let read = || (self.net().directory_pending(), self.clock.deadlines());
        let reached = tokio::time::timeout(WALL, async {
            loop {
                self.sync().await;
                let first = read();
                self.sync().await;
                let (pending, deadlines) = read();
                if first == (pending, deadlines.clone()) && deadlines.len() == 1 + pending {
                    return deadlines;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        match reached {
            Ok(deadlines) => deadlines,
            Err(_) => panic!(
                "the actor never parked: it waits for {:?}, {} lookups pend",
                self.clock
                    .deadlines()
                    .iter()
                    .map(|d| d.saturating_sub(START_MS))
                    .collect::<Vec<_>>(),
                self.net().directory_pending()
            ),
        }
    }

    /// Waits (a wall BOUND) until the actor waits for `deadline` on its clock, then moves BOTH
    /// clocks there in steps of at most 1 s, the fake's first (critic M5: an EXPECTED deadline,
    /// never "any sleep").
    pub async fn advance_to(&self, deadline: u64) {
        self.wait_deadline(deadline).await;
        assert!(
            self.clock.deadlines().iter().all(|d| *d >= deadline),
            "the actor waits for something before the expected deadline"
        );
        self.step_to(deadline).await;
    }

    /// Moves BOTH clocks to `target` in steps of at most 1 s while every deadline the actor waits
    /// for lies AFTER `target`: time passes and the actor must not act on it (TH3 fix P-L7: a
    /// frame heard at +15 s moves a 20 s deadline). Call it only once the actor waits for what it
    /// will wait for (`wait_deadline`).
    pub async fn advance_through(&self, target: u64) {
        assert!(
            self.clock.deadlines().iter().all(|d| *d > target),
            "the actor waits for something at or before +{} ms",
            target.saturating_sub(START_MS)
        );
        self.step_to(target).await;
    }

    /// Moves BOTH clocks to `target` in steps of at most 1 s, whatever the actor waits for: for a test that does not say
    /// which deadline comes next ([`H::advance_to`] does).
    pub async fn step_to(&self, target: u64) {
        while self.now() < target {
            let step = (target - self.now()).min(STEP_MS);
            self.net().act(|m| m.advance(Duration::from_millis(step)));
            self.clock.set(self.now() + step);
        }
    }

    /// The account's uuid (its "Connect Anyway" flag).
    pub fn uuid(&self) -> AccountUuid {
        self.uuid
    }

    /// The rows of `table` in the account's file (a second reader).
    pub fn file_rows(&self, table: &str) -> i64 {
        let db =
            kourier_threema::store::open(&db_path(self.dir.path()), &key()).expect("open the file");
        db.read(|conn: &kourier_db::rusqlite::Connection| {
            conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .map_err(kourier_db::Error::from)
        })
        .expect(table)
    }

    /// The account's file through a connection of its own (a second reader, and a second writer where a test writes what
    /// a later step's zone would).
    pub fn db(&self) -> kourier_db::ZoneDb {
        kourier_threema::store::open(&db_path(self.dir.path()), &key()).expect("open the file")
    }

    /// TH6a S5a: a job written while the actor runs, as a later step's receive zone would; its task's id.
    pub fn job(&self, job: &NewJob) -> i64 {
        let now = i64::try_from(self.now()).expect("ms fit i64");
        self.db()
            .zone(|tx| kourier_threema::store::enqueue_job(tx, job, now))
            .expect("write a job")
    }

    /// TH6a S6: a `Transmit`, as the core's dispatcher sends it; the far end of its reply. The message row is the core's and
    /// not read here.
    pub async fn transmit(&self, req: OutgoingRequest) -> oneshot::Receiver<TransmitOutcome> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Transmit {
            local: kourier_model::MessageId(1),
            req: Box::new(req),
            reply,
        })
        .await;
        answer
    }

    /// The next envelope the actor shipped to the core (a wall BOUND).
    pub async fn envelope(&mut self) -> Envelope {
        tokio::time::timeout(WALL, self.ends.events.recv())
            .await
            .expect("the actor shipped an envelope in time")
            .expect("the actor's events channel is open")
    }

    /// RP-b: the next envelope is the logout's report, alone (a wall BOUND); the report.
    pub async fn logout_report(&mut self) -> LogoutReport {
        match self.envelope().await.events.as_slice() {
            [BackendEvent::Logout(report)] => report.clone(),
            _ => panic!("the logout was not reported as one event"),
        }
    }

    /// The core side of TH5 A6: reads envelopes until `n` messages came, ACKING each (the core applied it)
    /// and checking that nothing but messages was shipped; the messages in the order they came.
    pub async fn messages(&mut self, n: usize) -> Vec<IncomingMessage> {
        let mut got = Vec::new();
        while got.len() < n {
            let envelope = self.envelope().await;
            let ack = envelope.ack.expect("a durable envelope");
            for event in envelope.events {
                match event {
                    BackendEvent::Message(m) => got.push(m),
                    _ => panic!("the actor shipped an event that is no message"),
                }
            }
            ack.complete(Ok(()));
        }
        assert_eq!(got.len(), n, "more messages than expected");
        got
    }

    /// TH6 L4b: the core side that ACKS everything it gets (a durable envelope is applied; an ephemeral one has no ack)
    /// until the published state is `what` (a wall BOUND); the events it got, in order.
    pub async fn ack_until(
        &mut self,
        what: &str,
        done: impl Fn(&ConnState) -> bool,
    ) -> Vec<BackendEvent> {
        let mut events = Vec::new();
        let ends = &mut self.ends;
        let reached = tokio::time::timeout(WALL, async {
            loop {
                if done(&ends.conn.borrow_and_update()) {
                    return;
                }
                tokio::select! {
                    envelope = ends.events.recv() => {
                        let Some(envelope) = envelope else { return };
                        if let Some(ack) = envelope.ack {
                            ack.complete(Ok(()));
                        }
                        events.extend(envelope.events);
                    }
                    _ = ends.conn.changed() => {}
                }
            }
        })
        .await;
        assert!(
            reached.is_ok() && done(&self.ends.conn.borrow()),
            "the state never became {what}; it is {:?}",
            self.state()
        );
        events
    }

    /// Nothing waits for the core (call after `sync()`).
    pub fn nothing_for_the_core(&mut self) {
        assert!(
            self.ends.events.try_recv().is_err(),
            "the actor shipped an envelope that was not expected"
        );
    }

    /// The account's `backend.db` (a second reader may open it, e.g. in the fake's ack hook).
    pub fn db_file(&self) -> std::path::PathBuf {
        db_path(self.dir.path())
    }

    /// The slot state the account's file says now (a second reader of the WAL file).
    pub fn file_slot(&self) -> SlotState {
        let db =
            kourier_threema::store::open(&db_path(self.dir.path()), &key()).expect("open the file");
        db.read(kourier_threema::store::load_identity)
            .expect("read the identity")
            .expect("an identity")
            .slot_state
    }

    /// Shutdown and its checks (an open socket closed with 1000, `Exit::Shutdown`, the thread
    /// joined), without the violations check.
    async fn shut_down(&mut self) {
        let open = self.net().open_connections();
        self.send(Command::Shutdown).await;
        self.ended(Exit::Shutdown).await;
        for conn in open {
            assert_eq!(
                self.s.world.mediator().closed(conn),
                Some(1000),
                "a socket open at the shutdown was not closed with 1000"
            );
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

    /// A NEW actor of the same account over the same file (`Resume`), its clock where this one's
    /// is: the restart a crash or the core's restart of a failed actor gives.
    pub async fn restart(mut self) -> H {
        self.shut_down().await;
        let now = self.now();
        begin_in(self.s, self.dir, true, self.uuid, now)
    }

    /// Shuts the actor down and gives the world back (its slots kept), for a new file.
    pub async fn stop(mut self) -> Scene {
        self.shut_down().await;
        self.s
    }

    /// Moves BOTH clocks `by_ms` in steps of at most 1 s while the actor waits for nothing on its
    /// clock (a held connection): every step checks that no deadline is armed.
    pub async fn advance_idle(&self, by_ms: u64) {
        let until = self.now() + by_ms;
        while self.now() < until {
            self.sync().await;
            assert!(
                self.clock.deadlines().is_empty(),
                "the actor waits on its clock although it holds"
            );
            let step = (until - self.now()).min(STEP_MS);
            self.net().act(|m| m.advance(Duration::from_millis(step)));
            self.clock.set(self.now() + step);
        }
        self.sync().await;
    }

    /// Waits (a wall BOUND) until the fake saw `n` connections in all.
    pub async fn wait_opened(&self, n: usize) {
        let net = self.net();
        assert!(
            net.wait(|| net.opened().len() >= n, WALL).await,
            "the fake never saw {n} connections"
        );
        assert_eq!(net.opened().len(), n, "more connections than expected");
    }

    /// Waits (a wall BOUND) until the connector was asked for `n` connects in all (opened, failed
    /// or refused), and not more.
    pub async fn wait_attempts(&self, n: usize) {
        let net = self.net();
        assert!(
            net.wait(|| net.attempts() >= n, WALL).await,
            "the connector never saw {n} connects (it saw {})",
            net.attempts()
        );
        assert_eq!(net.attempts(), n, "more connects than expected");
    }

    /// Shutdown: the actor closes its open socket with 1000 and ends `Shutdown`; then the fake's
    /// violations are empty (the LAST check).
    pub async fn finish(mut self) {
        self.shut_down().await;
        self.violations_checked();
    }

    /// Waits (a wall BOUND) until the fake's mediator finished the handshake of this device (it
    /// sent `ServerInfo`); that connection. The actor reads `ServerInfo` a moment later.
    pub async fn wait_connected(&self) -> ConnId {
        let net = self.net();
        assert!(
            net.wait(|| net.connected(DEVICE).is_some(), WALL).await,
            "this device never finished the D2M handshake"
        );
        self.conn()
    }

    /// The actor ended by itself with `Exit::Failed(_)` (a panicked connection task, P-L8); then the
    /// fake's violations are empty (the LAST check).
    pub async fn finish_failed(mut self) {
        let exited = tokio::time::timeout(WALL, &mut self.handle.exited)
            .await
            .expect("the actor ends in time");
        assert!(
            matches!(Exit::from_channel(exited), Exit::Failed(_)),
            "the actor did not end Failed"
        );
        if let Some(thread) = self.handle.thread.take() {
            thread.join().expect("the actor's thread joins");
        }
        self.violations_checked();
    }

    /// Waits until the actor ended BY ITSELF with `Exit::Shutdown` (the core's token was cancelled:
    /// `start_cancelled`) and its thread joined; the file can then be read with nothing writing it.
    pub async fn wait_shutdown(&mut self) {
        self.ended(Exit::Shutdown).await;
    }

    /// Waits (a wall BOUND) until the actor ended BY ITSELF, joins its thread and gives its exit (TH5b
    /// S-M1: a start that fails); the file can then be read with nothing writing it.
    pub async fn wait_exit(&mut self) -> Exit {
        let exited = tokio::time::timeout(WALL, &mut self.handle.exited)
            .await
            .expect("the actor ends in time");
        if let Some(thread) = self.handle.thread.take() {
            thread.join().expect("the actor's thread joins");
        }
        Exit::from_channel(exited)
    }

    /// A command the core QUEUES at the start (it sends `Connect` right after it started the actor): taken
    /// when the actor reads its commands, lost with the channel when the actor ended first (as the core's).
    pub fn queue(&self, command: Command) {
        let _ = self.ends.commands.try_send(command);
    }

    /// After [`H::wait_exit`] (the actor ended by itself): a NEW actor of the same account over the same file
    /// (`Resume`), its clock where this one's is: the core's restart after its back-off.
    pub fn restart_after_exit(self) -> H {
        assert!(
            self.handle.thread.is_none(),
            "restart_after_exit before wait_exit"
        );
        let now = self.now();
        begin_in(self.s, self.dir, true, self.uuid, now)
    }

    /// After [`H::wait_shutdown`] or [`H::wait_exit`]: the fake's violations are empty (the LAST check).
    pub fn finish_after_shutdown(self) {
        assert!(
            self.handle.thread.is_none(),
            "finish_after_shutdown before wait_shutdown"
        );
        self.violations_checked();
    }

    /// The actor ended by itself with `Exit::LoggedOut { reason }`; then the fake's violations are
    /// empty (the LAST check).
    pub async fn finish_logged_out(mut self, reason: &str) {
        self.ended(Exit::LoggedOut {
            reason: reason.to_owned(),
        })
        .await;
        self.violations_checked();
    }
}

impl H {
    /// THE LAST CHECK of every finisher: THE GUARD ORACLE over the leader's work (TH6 L4b: every `message-ack` the actor
    /// sent follows the reflect-ack of its message's reflection, or the message is a declared discard; L2a's
    /// `leader_violations`, the phone of the tests' scenes skipped), then the fake's violations are empty.
    fn violations_checked(&self) {
        self.s
            .world
            .leader_violations(&self.s.keys, super::reflect::PHONE);
        self.s.world.violations().assert_empty();
    }
}

/// The trace lines of `conn` whose kind is `kind` and that went `dir` ("in" from the client, "out"
/// from the fake).
pub fn traced(world: &MockWorld, conn: ConnId, dir: &str, kind: &str) -> usize {
    world
        .trace()
        .lines()
        .iter()
        .filter(|line| {
            let mut parts = line.split(' ').skip(1);
            parts.next() == Some(conn.0.to_string().as_str())
                && parts.next() == Some(dir)
                && parts.next() == Some(kind)
        })
        .count()
}
