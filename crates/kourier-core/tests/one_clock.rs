//! A10b (critic M5): the core and every backend read ONE clock. A backend that compares the
//! `asked_at` of a command (the core's clock, `Shared::now`) with the time it took in what
//! another device changed (stamped by the backend itself) is right only if both are the clock
//! `CoreConfig` was started with: the supervisor hands the very same `Arc<dyn Clock>` to the
//! backend as `AccountContext::clock` (`supervisor.rs`, `ContextParams { clock: .. }`).
//!
//! The backend here only keeps the clock it was given. The core's side of the rule (`asked_at` is
//! the core's `Shared::now`) is `roaming_clear.rs`; the backend's side (what it takes in is
//! stamped with the clock of its context) is tested in that backend's own crate.

use crate::support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use kourier_backend_api::{
    AccountContext, AccountHandle, AllocatedId, Clock, Command, Exit, Network, NetworkRegistry,
};
use kourier_core::Timeouts;
use kourier_core::facade::UiCommand;
use kourier_fake::{FakeNetwork, Flavor, Script};
use kourier_model::{BackendError, ConvKey, NetworkDescriptor, NetworkKind, UserId};
use support::{Env, add_account};

/// A network whose actor keeps the clock of its context and ends at `Shutdown`.
#[derive(Clone)]
struct Spy {
    descriptor: NetworkDescriptor,
    clock: Arc<Mutex<Option<Arc<dyn Clock>>>>,
}

impl Network for Spy {
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
        *self.clock.lock().expect("lock") = Some(ctx.clock.clone());
        let thread = std::thread::spawn(move || {
            // The whole context stays here until the actor ends (see `stop_grace.rs`).
            let mut ctx = ctx;
            loop {
                match ctx.commands.blocking_recv() {
                    Some(Command::Shutdown) => break,
                    Some(_) => {}
                    None => return,
                }
            }
            let _ = exit_tx.send(Exit::Shutdown);
        });
        Ok(AccountHandle {
            exited,
            thread: Some(thread),
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_backend_reads_the_clock_the_core_was_started_with() {
    let spy = Spy {
        descriptor: Flavor::SignalLike.descriptor(),
        clock: Arc::default(),
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = kourier_backend_api::FakeClock::new(kourier_fake::scripts::T0 as u64);
    let vault = Arc::new(kourier_core::testing::DirVault::new(
        dir.path().join("data"),
    ));
    let networks = NetworkRegistry::new()
        .with(Arc::new(spy.clone()))
        .expect("registry");
    let timeouts = Timeouts {
        shutdown_grace: Duration::from_millis(500),
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

    // The account's actor is started a little after it is added.
    support::eventually("the backend to be started", support::WAIT, || async {
        spy.clock.lock().expect("lock").is_some()
    })
    .await;
    let given = spy
        .clock
        .lock()
        .expect("lock")
        .clone()
        .expect("the backend was started with a context");
    // A time of the test's own, set AFTER the backend has the clock: it sees it only if it holds
    // the core's clock and not a clock of its own.
    let later = kourier_fake::scripts::T0 as u64 + 7_000_123;
    env.clock.set(later);
    assert_eq!(given.now_ms(), later, "the backend reads the core's clock");
    env.stop().await;
}
