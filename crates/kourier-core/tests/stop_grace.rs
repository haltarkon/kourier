//! R-Q1 (W4): a network states how long its actor may take to stop (`NetworkDescriptor::
//! shutdown_grace`), and a stop gives that actor exactly that long before it cancels its token;
//! a network that states none gets the core's default (`Timeouts::shutdown_grace`).
//!
//! The actor here is stubborn: it ignores everything until `Command::Shutdown`, then ignores
//! that too until its cancel token fires, and says how long that took. The core's default is
//! made short (100 ms) and the stating network's grace long against it (2 s), so a stop that
//! reads the default for both is told apart by a margin no scheduler hiccup closes.

use crate::support;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kourier_backend_api::{
    AccountContext, AccountHandle, AllocatedId, Command, Exit, Network, NetworkRegistry,
};
use kourier_core::Timeouts;
use kourier_core::facade::UiCommand;
use kourier_fake::{FakeNetwork, Flavor, Script};
use kourier_model::{BackendError, ConvKey, NetworkDescriptor, NetworkKind, UserId};
use support::{Env, add_account};

/// The core's default stop grace in these tests.
const DEFAULT: Duration = Duration::from_millis(100);
/// What the stating network asks for.
const STATED: Duration = Duration::from_secs(2);
/// How much earlier than its grace a stop may seem to end, measured from the actor's side (it
/// notices `Shutdown` a little after the core sent it).
const SLACK: Duration = Duration::from_millis(100);

/// A network whose actor ignores `Shutdown` until it is cancelled, and records how long that
/// took after it noticed `Shutdown`.
#[derive(Clone)]
struct Stubborn {
    descriptor: NetworkDescriptor,
    waited: Arc<Mutex<Option<Duration>>>,
}

impl Stubborn {
    fn new(stated: Option<Duration>) -> Self {
        let mut descriptor = Flavor::SignalLike.descriptor();
        descriptor.shutdown_grace = stated;
        Self {
            descriptor,
            waited: Arc::default(),
        }
    }

    fn waited(&self) -> Option<Duration> {
        *self.waited.lock().expect("lock")
    }
}

impl Network for Stubborn {
    fn kind(&self) -> NetworkKind {
        NetworkKind::Fake
    }

    fn descriptor(&self) -> &NetworkDescriptor {
        &self.descriptor
    }

    fn allocate_message_id(&self, _own: &[UserId], _conv: &ConvKey, now_ms: u64) -> AllocatedId {
        Flavor::SignalLike.allocate(now_ms)
    }

    fn spawn(&self, ctx: AccountContext) -> Result<AccountHandle, BackendError> {
        let (exit_tx, exited) = tokio::sync::oneshot::channel();
        let waited = self.waited.clone();
        let thread = std::thread::spawn(move || {
            // The whole context stays here until the actor ends: a closure that named only two of
            // its fields would leave the rest (the event sender, the connection state) to be
            // dropped when `spawn` returns, which the core reads as an actor whose events are over.
            let mut ctx = ctx;
            loop {
                match ctx.commands.blocking_recv() {
                    Some(Command::Shutdown) => break,
                    Some(_) => {}
                    None => return,
                }
            }
            let asked = Instant::now();
            while !ctx.cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(1));
            }
            *waited.lock().expect("lock") = Some(asked.elapsed());
            let _ = exit_tx.send(Exit::Shutdown);
        });
        Ok(AccountHandle {
            exited,
            thread: Some(thread),
        })
    }
}

/// How long a core with `stubborn` registered, one account on it and `DEFAULT` as its own grace,
/// leaves the actor alone after `Shutdown`.
async fn grace_given(stubborn: &Stubborn) -> Duration {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = kourier_backend_api::FakeClock::new(kourier_fake::scripts::T0 as u64);
    let vault = Arc::new(kourier_core::testing::DirVault::new(
        dir.path().join("data"),
    ));
    let networks = NetworkRegistry::new()
        .with(Arc::new(stubborn.clone()))
        .expect("registry");
    let timeouts = Timeouts {
        shutdown_grace: DEFAULT,
        logout: Duration::from_secs(5),
        ..Timeouts::default()
    };
    let core = support::start_core_timed(
        dir.path(),
        networks,
        &vault,
        Arc::new(clock.clone()),
        timeouts,
    )
    .await;
    let fake = FakeNetwork::new(Flavor::SignalLike, Script::new("unused", Vec::new()));
    let handle = core.handle();
    let mut env = Env {
        dir,
        clock,
        control: fake.control(),
        fake,
        vault,
        events: handle.subscribe(),
        handle,
        core: Some(core),
    };
    env.handle.send(UiCommand::UiReady).expect("send");
    add_account(&mut env).await;
    env.stop().await;
    stubborn
        .waited()
        .expect("the actor was told to shut down and then cancelled")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_network_that_states_no_grace_gets_the_cores_default() {
    let waited = grace_given(&Stubborn::new(None)).await;
    assert!(
        waited + SLACK >= DEFAULT && waited < STATED - Duration::from_millis(500),
        "the default grace is {DEFAULT:?}, the actor was left alone for {waited:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_network_that_states_a_grace_is_left_alone_that_long() {
    let waited = grace_given(&Stubborn::new(Some(STATED))).await;
    assert!(
        waited + SLACK >= STATED,
        "the network asked for {STATED:?}, the actor was cancelled after {waited:?}"
    );
}
