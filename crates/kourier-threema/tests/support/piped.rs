//! THE KILL-TEST HARNESS over the parent-resident fake (TH5a A3, `mock/pipe.rs`): the PARENT test
//! holds the world (mediator, chat server, phone) and the account's data directory, spawns the actor
//! in an `unshare -rn` CHILD (`offline::child_command`, rule 4) whose parts are the piped connector,
//! waits for the child's end (by itself, at a failpoint, or killed by the test), reads the child's
//! `backend.db` itself, and spawns the next child on the SAME world. The children never ack a
//! `core_outbox` row (M4): the parent counts `core_outbox` + `core_outbox_poison` rows.
//!
//! No wall clock and no sleep: the actor's `FakeClock` starts at the fake's virtual clock (the
//! parent's `Setup`), the parent moves the fake's clock ([`Parent::advance`]); the waits are wall
//! BOUNDS only ([`WALL`]): the bridge's end comes as a message (`recv_timeout`), and a child that
//! does not end within it is SIGKILLed by the parent (its own child, not yet reaped) and the test
//! fails. Nothing here prints a key, an ID or a label.

use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{
    AccountContext, AccountHandle, BackendConfig, Command, ContextParams, CoreEnds, DbKey, Exit,
    FakeClock, Network, StartMode,
};
use kourier_model::{AccountId, AccountUuid, ConnState};
use kourier_threema::ThreemaNetwork;
use kourier_threema::store::{Identity, SlotState, mark_join_ready, save_identity};
use zeroize::Zeroizing;

use super::mock::crypto::DeviceGroupKeys;
use super::mock::pipe::{self, Bridge, Setup};
use super::mock::{self, MockWorld, START_MS, TestIdentity, world};
use super::{TEST_KEY, db_path, key, offline};

/// The synthetic identity of the piped tests.
pub const ME: &str = "KOURIER5";
/// This device's D2M id.
pub const DEVICE: u64 = 0xF555_0000_0000_0005;
/// The stored label.
pub const LABEL: &str = "Kourier (th5-piped)";
/// The server group (the mediator's charset).
pub const SERVER_GROUP: &str = "ds7";
/// The upper bound of every wait of the parent and the child (never a measure).
pub const WALL: Duration = Duration::from_secs(60);
/// The child's data directory (the parent's temp dir, under the test's TMPDIR).
const DIR_ENV: &str = "THREEMA_PIPED_DIR";
/// The child's start mode (`rebuild` or unset).
const MODE_ENV: &str = "THREEMA_PIPED_MODE";

/// The world of a piped test and the account's file, both in the PARENT.
pub struct Parent {
    seed: u64,
    pub world: MockWorld,
    pub dgk: [u8; 32],
    pub keys: DeviceGroupKeys,
    pub me: TestIdentity,
    dir: tempfile::TempDir,
    /// How many children this parent spawned.
    children: std::cell::Cell<usize>,
}

/// The parent's scene of `seed`: the identity known to the chat server, the device group at the
/// mediator, and a linked `backend.db` (identity, slot `new`, zone 2's marker) in a temp directory.
pub fn parent(seed: u64) -> Parent {
    let (world, dgk, keys, me) = scene(seed);
    let dir = tempfile::tempdir().expect("temp dir");
    let cookie: [u8; 16] = world.random();
    let csp_device = world.random_u64();
    let identity = Identity {
        threema_id: ME.to_owned(),
        client_key: Zeroizing::new(me.ck.secret),
        server_group: SERVER_GROUP.to_owned(),
        device_group_key: Zeroizing::new(dgk),
        device_cookie: Zeroizing::new(cookie),
        d2m_device_id: DEVICE,
        csp_device_id: csp_device,
        slot_state: SlotState::New,
        device_label: LABEL.to_owned(),
        linked_at_ms: i64::try_from(START_MS).expect("ms fit i64"),
    };
    let db = kourier_threema::store::open(&db_path(dir.path()), &key()).expect("make the file");
    db.zone(|tx| {
        save_identity(tx, &identity)?;
        mark_join_ready(tx, 1)
    })
    .expect("save the identity");
    drop(db);
    Parent {
        seed,
        world,
        dgk,
        keys,
        me,
        dir,
        children: std::cell::Cell::new(0),
    }
}

/// The world of `seed` with the identity and the device group (deterministic from the seed).
fn scene(seed: u64) -> (MockWorld, [u8; 32], DeviceGroupKeys, TestIdentity) {
    let world = world(seed);
    let me = world.add_identity(ME);
    let dgk = world.new_device_group_key();
    let keys = DeviceGroupKeys::derive(&dgk);
    world.mediator().add_group(keys.path_public, SERVER_GROUP);
    (world, dgk, keys, me)
}

/// How a child of the piped tests starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `StartMode::Resume` (the restart of a linked account).
    Resume,
    /// `StartMode::Rebuild` (states `AccountReady` again: `store::ready_join`'s failpoints).
    Rebuild,
}

impl Parent {
    /// The account's data directory (the child's).
    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// Spawns the child test `child` of `module` (`module_path!()`) under `unshare -rn` with the
    /// piped parts on THIS world and `failpoint` (`<name>:<n>`) armed; `None` when `unshare` is
    /// unavailable and nobody insisted (the test skips). The parent's copies of the child's pipe
    /// ends are closed right after the spawn.
    pub fn spawn(
        &self,
        module: &str,
        child: &str,
        mode: Mode,
        failpoint: Option<&str>,
    ) -> Option<Running> {
        self.spawn_with(module, child, mode, failpoint, &[])
    }

    /// As [`Parent::spawn`], with `env` set in the child (TH6a S8: a child that runs one case of several chosen by the
    /// parent, as `offline::run_child_output`).
    pub fn spawn_with(
        &self,
        module: &str,
        child: &str,
        mode: Mode,
        failpoint: Option<&str>,
        env: &[(&str, &str)],
    ) -> Option<Running> {
        if !offline::unshare_or_skip("this test never runs an actor on the real network") {
            return None;
        }
        let net = self.world.net().clone();
        let setup = Setup {
            params: self.world.server_params(),
            now_ms: self.world.clock().now_ms(),
        };
        let (ends, bridge) = pipe::serve(&net, &setup);
        let mut command = offline::child_command(module, child);
        command
            .env(DIR_ENV, self.dir.path())
            .env_remove("KOURIER_FAILPOINT")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if mode == Mode::Rebuild {
            command.env(MODE_ENV, "rebuild");
        }
        command.envs(env.iter().copied());
        if let Some(point) = failpoint {
            command.env("KOURIER_FAILPOINT", point);
        }
        ends.install(&mut command);
        let mut child = command.spawn().expect("spawn the child under unshare");
        drop(ends);
        self.children.set(self.children.get() + 1);
        let drain = |r: Option<Box<dyn std::io::Read + Send>>| {
            std::thread::spawn(move || {
                let mut text = String::new();
                if let Some(mut r) = r {
                    let _ = r.read_to_string(&mut text);
                }
                text
            })
        };
        let out = drain(child.stdout.take().map(|s| Box::new(s) as _));
        let err = drain(child.stderr.take().map(|s| Box::new(s) as _));
        Some(Running {
            child: Some(child),
            bridge,
            out: Some(out),
            err: Some(err),
        })
    }

    /// Moves the fake's virtual clock (the children's actor clocks are not moved: nothing in a kill
    /// test waits for a deadline).
    pub fn advance(&self, by: Duration) {
        self.world.net().act(|m| m.advance(by));
    }

    /// Waits (a wall BOUND) until `done` holds, re-checked at every change of the fake.
    pub async fn wait(&self, what: &str, done: impl FnMut() -> bool) {
        let net = self.world.net();
        assert!(net.wait(done, WALL).await, "the fake never saw {what}");
    }

    /// The child's file, opened by the parent (a second reader of the WAL file).
    pub fn open(&self) -> kourier_db::ZoneDb {
        kourier_threema::store::open(&db_path(self.dir.path()), &key()).expect("open the file")
    }

    /// The slot state the file says.
    pub fn file_slot(&self) -> SlotState {
        self.open()
            .read(kourier_threema::store::load_identity)
            .expect("read the identity")
            .expect("an identity")
            .slot_state
    }

    /// M4: the rows the children wrote for the core, `core_outbox` + `core_outbox_poison` (no child
    /// acks, so nothing is deleted).
    pub fn outbox_rows(&self) -> usize {
        let conn = kourier_db::open(&db_path(self.dir.path()), &key()).expect("open the file");
        ["core_outbox", "core_outbox_poison"]
            .iter()
            .map(|table| {
                conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| {
                    r.get::<_, i64>(0)
                })
                .expect("count the rows")
            })
            .map(|n| usize::try_from(n).expect("a count"))
            .sum()
    }

    /// The seed of the world.
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// How many children this parent spawned (all on the SAME world).
    pub fn children(&self) -> usize {
        self.children.get()
    }
}

/// A child that runs; its end is [`Running::wait`] or [`Running::kill`]. A value dropped while the
/// child still runs (a failed assertion) SIGKILLs and reaps it.
pub struct Running {
    child: Option<std::process::Child>,
    bridge: Bridge,
    out: Option<std::thread::JoinHandle<String>>,
    err: Option<std::thread::JoinHandle<String>>,
}

/// How a child ended.
pub struct Ended {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl std::fmt::Debug for Ended {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ended")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl Ended {
    /// The child test passed (exactly one test, as `offline::run_child` insists).
    pub fn passed(&self) -> bool {
        self.status.success() && self.stdout.contains("test result: ok. 1 passed;")
    }

    /// The child aborted at an armed failpoint (`kourier_db::failpoint`: SIGABRT and its line).
    pub fn aborted_at_failpoint(&self) -> bool {
        use std::os::unix::process::ExitStatusExt as _;
        self.status.signal() == Some(6) && self.stderr.contains("hit: aborting")
    }

    /// The child was SIGKILLed.
    pub fn killed(&self) -> bool {
        use std::os::unix::process::ExitStatusExt as _;
        self.status.signal() == Some(9)
    }
}

impl Running {
    /// Waits for the child's end: the bridge's EOF (the child's ends closed: it is exiting) within
    /// [`WALL`], then its exit status. A child that does not end in time is SIGKILLed and the test
    /// fails.
    pub fn wait(mut self) -> Ended {
        if !self.bridge.wait_end(WALL) {
            self.reap(true);
            panic!("the child did not end, or its end never reached the parent as an EOF");
        }
        self.reap(false)
    }

    /// SIGKILLs the child (the test's own, not yet reaped), then [`Running::wait`].
    pub fn kill(mut self) -> Ended {
        if let Some(child) = self.child.as_mut() {
            child.kill().expect("kill the child");
        }
        self.wait()
    }

    fn reap(&mut self, kill: bool) -> Ended {
        let mut child = self.child.take().expect("the child is reaped once");
        if kill {
            let _ = child.kill();
        }
        let status = child.wait().expect("reap the child");
        let text = |h: Option<std::thread::JoinHandle<String>>| {
            h.map(|h| h.join().unwrap_or_default()).unwrap_or_default()
        };
        Ended {
            status,
            stdout: text(self.out.take()),
            stderr: text(self.err.take()),
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The child.

/// The actor of the CHILD on the piped parts (rule 4: only inside the `unshare -rn` child).
pub struct Child {
    pub clock: FakeClock,
    ends: CoreEnds,
    handle: AccountHandle,
}

/// IN THE CHILD: the actor over the parent's file (`Resume`, or `Rebuild` as the parent asked) on
/// the piped parts, its clock at the fake's virtual time of the spawn. It acks nothing (M4).
pub async fn child() -> Child {
    start_child(false).await
}

/// IN THE CHILD: [`child`] with the parts' test door open, so a `Command::Transmit` reaches the send core (TH6a S8; R7).
pub async fn child_send_open() -> Child {
    start_child(true).await
}

/// IN THE CHILD: the parent's file on a connection of the child's own, beside the actor's: for a child that waits until a
/// row stands as it must.
pub fn child_file() -> kourier_db::ZoneDb {
    let dir = PathBuf::from(std::env::var_os(DIR_ENV).expect("the parent's data directory"));
    kourier_threema::store::open(&db_path(&dir), &key()).expect("open the file")
}

async fn start_child(send_open: bool) -> Child {
    assert!(
        offline::in_child(),
        "an actor test outside the unshare child"
    );
    let dir = PathBuf::from(std::env::var_os(DIR_ENV).expect("the parent's data directory"));
    let pipe = pipe::open_child().await;
    let clock = FakeClock::new(pipe.setup().now_ms);
    let rebuild = std::env::var(MODE_ENV).is_ok_and(|m| m == "rebuild");
    let (ctx, ends) = AccountContext::new(ContextParams {
        account: AccountId(1),
        uuid: AccountUuid::new_random(),
        start: if rebuild {
            StartMode::Rebuild
        } else {
            StartMode::Resume
        },
        backend_db: db_path(&dir),
        db_key: DbKey::new(TEST_KEY),
        blob_dir: dir.join("blobs"),
        net_rt: tokio::runtime::Handle::current(),
        cancel: tokio_util::sync::CancellationToken::new(),
        clock: Arc::new(clock.clone()),
        config: BackendConfig::default(),
        ui: Default::default(),
        local_prefs: Default::default(),
    });
    let network = if send_open {
        ThreemaNetwork::new(mock::pipe::parts(&pipe).with_send_open())
    } else {
        ThreemaNetwork::new(mock::pipe::parts(&pipe))
    };
    let handle = network.spawn(ctx).expect("the actor starts");
    Child {
        clock,
        ends,
        handle,
    }
}

impl Child {
    pub async fn send(&self, command: Command) {
        self.ends
            .commands
            .send(command)
            .await
            .expect("the actor takes commands");
    }

    pub fn state(&self) -> ConnState {
        self.ends.conn.borrow().clone()
    }

    /// Waits (a wall BOUND) until the published state is `what`.
    pub async fn wait_state(&mut self, what: &str, done: impl Fn(&ConnState) -> bool) {
        let reached = matches!(
            tokio::time::timeout(WALL, self.ends.conn.wait_for(|s| done(s))).await,
            Ok(Ok(_))
        );
        assert!(
            reached,
            "the state never became {what}; it is {:?}",
            self.state()
        );
    }

    /// Connects and waits for `Online` (either role).
    pub async fn online(&mut self) {
        self.send(Command::Connect).await;
        self.wait_state("Online", |s| matches!(s, ConnState::Online { .. }))
            .await;
    }

    /// Connects and waits for `Syncing` (TH5 A5b: a file whose `core_outbox` rows the core never acks,
    /// as a kill child's, stays there after Dry: its boundary is never reached).
    pub async fn syncing(&mut self) {
        self.send(Command::Connect).await;
        self.wait_state("Syncing", |s| matches!(s, ConnState::Syncing { .. }))
            .await;
    }

    /// Shutdown: the actor ends `Shutdown` and its thread joins (the parent checks the fake: the
    /// socket closed with 1000, the violations).
    pub async fn finish(mut self) {
        self.send(Command::Shutdown).await;
        let exited = tokio::time::timeout(WALL, &mut self.handle.exited)
            .await
            .expect("the actor ends in time");
        assert_eq!(Exit::from_channel(exited), Exit::Shutdown);
        if let Some(thread) = self.handle.thread.take() {
            thread.join().expect("the actor's thread joins");
        }
    }

    /// Waits for the parent's SIGKILL; a child still alive after [`WALL`] fails.
    pub async fn finish_killed(self) {
        let _ = tokio::time::timeout(WALL, std::future::pending::<()>()).await;
        panic!("the parent never killed this child");
    }
}
