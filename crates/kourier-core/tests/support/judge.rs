//! A backend that judges the clock the way a real one does at its connect, around the fake.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use kourier_backend_api::{
    AccountContext, AccountHandle, AllocatedId, Clock, Exit, Network, NetworkRegistry,
};
use kourier_core::Core;
use kourier_core::testing::DirVault;
use kourier_fake::FakeNetwork;
use kourier_model::{
    AccountId, BackendError, ConnState, ConvKey, NetworkDescriptor, NetworkKind, UserId,
};
use tokio::sync::watch;

/// The fake network, except that the next account starts through it are *judged*: instead of
/// the fake's actor, each runs a stand-in that publishes what the test tells it (for example
/// first `FixClock`, later `Online`; or `Offline` for a start without a network) once the
/// core says `Connect`, and does nothing else. The fake cannot open a start with a verdict on
/// the clock of its own: its actor goes `Online` as soon as it is connected.
///
/// A queued verdict is for the next start of any account ([`judge_next`](Self::judge_next))
/// or of one account ([`judge_account`](Self::judge_account): with several accounts the
/// supervisors spawn their backends in no fixed order). A start with nothing queued for it
/// is the fake's own.
#[derive(Clone)]
pub struct ClockJudge {
    pub inner: FakeNetwork,
    next: Arc<Mutex<VecDeque<Queued>>>,
}

/// The verdicts of one start, and whose (`None`: any account's).
type Queued = (Option<AccountId>, watch::Receiver<ConnState>);

impl ClockJudge {
    pub fn new(inner: FakeNetwork) -> Self {
        Self {
            inner,
            next: Arc::default(),
        }
    }

    /// The next account start is judged: its backend shows `first` once it is told to
    /// connect, then whatever is sent through the returned sender.
    pub fn judge_next(&self, first: ConnState) -> watch::Sender<ConnState> {
        self.queue(None, first)
    }

    /// [`judge_next`](Self::judge_next) for the next start of `account` only.
    pub fn judge_account(&self, account: AccountId, first: ConnState) -> watch::Sender<ConnState> {
        self.queue(Some(account), first)
    }

    fn queue(&self, account: Option<AccountId>, first: ConnState) -> watch::Sender<ConnState> {
        let (verdict, verdicts) = watch::channel(first);
        self.next
            .lock()
            .expect("lock")
            .push_back((account, verdicts));
        verdict
    }

    /// A core on `dir` whose only network is this one.
    pub async fn start(&self, dir: &Path, vault: &Arc<DirVault>, clock: Arc<dyn Clock>) -> Core {
        let networks = NetworkRegistry::new()
            .with(Arc::new(self.clone()))
            .expect("registry");
        super::start_core_on(dir, networks, vault, clock).await
    }
}

impl Network for ClockJudge {
    fn kind(&self) -> NetworkKind {
        self.inner.kind()
    }

    fn descriptor(&self) -> &NetworkDescriptor {
        self.inner.descriptor()
    }

    fn allocate_message_id(&self, own: &[UserId], conv: &ConvKey, now_ms: u64) -> AllocatedId {
        self.inner.allocate_message_id(own, conv, now_ms)
    }

    fn spawn(&self, ctx: AccountContext) -> Result<AccountHandle, BackendError> {
        let queued = {
            let mut next = self.next.lock().expect("lock");
            let at = next
                .iter()
                .position(|(account, _)| account.is_none_or(|a| a == ctx.account));
            at.and_then(|at| next.remove(at))
        };
        let Some((_, mut verdicts)) = queued else {
            return self.inner.spawn(ctx);
        };

        let (exit_tx, exited) = tokio::sync::oneshot::channel();
        ctx.net_rt.clone().spawn(async move {
            // The whole context: the supervisor reads a closed event channel as an actor
            // that is gone.
            let mut ctx = ctx;
            let mut connected = false;
            loop {
                tokio::select! {
                    c = ctx.commands.recv() => match c.as_ref().map(|c| c.name()) {
                        Some("Connect") => {
                            connected = true;
                            ctx.conn.send_replace(verdicts.borrow_and_update().clone());
                        }
                        Some("Shutdown") | None => break,
                        Some(_) => {}
                    },
                    r = verdicts.changed() => {
                        if r.is_err() {
                            break;
                        }
                        if connected {
                            ctx.conn.send_replace(verdicts.borrow_and_update().clone());
                        }
                    }
                }
            }
            let _ = exit_tx.send(Exit::Shutdown);
        });
        Ok(AccountHandle {
            exited,
            thread: None,
        })
    }
}
