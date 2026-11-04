//! What the integration tests share: an account actor over refusing parts, with the core's ends.
//!
//! Every network here is made from `TgParts::refusing`: a helper spawn is refused and counted, a
//! dial is refused. TG-1a's actor starts no helper and dials nothing, so no test of this binary
//! needs an offline child; the TG-1b tests that run a helper add `offline.rs`.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use kourier_backend_api::{
    AccountContext, AccountHandle, ContextParams, CoreEnds, DbKey, Network, StartMode, SystemClock,
};
use kourier_model::{AccountId, AccountUuid};
use kourier_telegram::{TelegramNetwork, TgParts};
use tokio_util::sync::CancellationToken;

/// The key of every test account's `backend.db`.
pub const TEST_KEY: [u8; 32] = [0x5a; 32];

/// A started account actor and the core's ends of its channels.
pub struct Running {
    pub ends: CoreEnds,
    pub handle: AccountHandle,
    /// How many helpers the actor tried to start.
    pub made: Arc<AtomicUsize>,
}

impl Running {
    pub fn helpers_made(&self) -> usize {
        self.made.load(Ordering::SeqCst)
    }
}

/// Spawns one account of a `TelegramNetwork` over refusing parts, its `backend.db` in `dir`.
/// Call it inside a tokio runtime (the context takes its handle).
pub fn start(dir: &Path, start: StartMode, key: [u8; 32]) -> Running {
    let made = Arc::new(AtomicUsize::new(0));
    let network = TelegramNetwork::new(TgParts::refusing(made.clone()));
    let cancel = CancellationToken::new();
    let (ctx, ends) = AccountContext::new(ContextParams {
        account: AccountId(1),
        uuid: AccountUuid::new_random(),
        start,
        backend_db: dir.join("backend.db"),
        db_key: DbKey::new(key),
        blob_dir: dir.join("blobs"),
        net_rt: tokio::runtime::Handle::current(),
        cancel,
        clock: Arc::new(SystemClock),
        config: Default::default(),
        ui: Default::default(),
        local_prefs: Default::default(),
    });
    let handle = network.spawn(ctx).expect("the account thread starts");
    Running { ends, handle, made }
}

/// A multi-thread runtime for a test's own awaits.
pub fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
}
