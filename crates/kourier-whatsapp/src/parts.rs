//! [`NetParts`]: the two things a WhatsApp client cannot be built without, a transport factory (the
//! socket to WhatsApp's servers) and an HTTP client (the version fetch, media), as a LAZY maker.
//!
//! whatsapp-rust is built without its default transport and HTTP client (root `Cargo.toml`), so its
//! builder REQUIRES both slots (lane decision 2: offline by construction). [`WhatsAppNetwork::new`]
//! takes a `NetParts` as a required argument; nothing in this crate can reach a real server unless
//! the one module listed in `REAL_CLIENT_MODULES` of `tests/builder.rs` was asked for the real ones,
//! and the source check keeps every other file, test files included, from asking.
//!
//! The parts are a MAKER, not a pair of values: the actor calls it on its own thread at the first
//! `Connect` or `StartLogin` of an account (decision L2/M4: nothing of upstream is touched at the
//! start of the application or of an actor, so start-up pays nothing for an account that never
//! connects), and once more for each client it makes again (a terminal verdict needs a new client).
//! A test hands in guards that refuse and count (`tests/support/guards.rs`), or makers that only
//! count.
//!
//! [`WhatsAppNetwork::new`]: crate::WhatsAppNetwork::new

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use wacore::net::{
    HttpClient, HttpRequest, HttpResponse, Transport, TransportEvent, TransportFactory,
};

use crate::session::Builder;

/// The maker the actor calls: one transport factory and one HTTP client per client.
type Make = dyn Fn() -> (Arc<dyn TransportFactory>, Arc<dyn HttpClient>) + Send + Sync;

/// A step the test fixture adds to the client's builder (the scripted server needs the noise
/// certificate check off and a fixed WhatsApp Web version; see `with_builder_hook`).
pub(crate) type Hook = Arc<dyn Fn(Builder) -> Builder + Send + Sync>;

/// What a client is made with. Cheap to clone (the maker is shared); it prints nothing.
#[derive(Clone)]
pub struct NetParts {
    make: Arc<Make>,
    /// How long a logout waits for a connection before it asks the server to remove the device;
    /// `None` is the default (`session::client`).
    logout_wait: Option<Duration>,
    /// Set by the scripted fixture of the tests only; `None` for every real client.
    hook: Option<Hook>,
    /// This computer's host name as the APPLICATION reads it (W2b): the one source of the name the
    /// phone lists this device by (`Kourier (<host>)`, `props::device_name`) and of the name the
    /// logout report gives the person. The crate reads no host name of its own. Empty: the name is
    /// plain `Kourier`.
    host_name: String,
}

impl NetParts {
    /// Parts from a maker of each. Neither is called here: `from_makers` builds nothing. Production
    /// code gets its parts from the one module in `REAL_CLIENT_MODULES`; a test hands in guards or
    /// counters.
    pub fn from_makers<T, H>(
        transport: impl Fn() -> T + Send + Sync + 'static,
        http: impl Fn() -> H + Send + Sync + 'static,
    ) -> Self
    where
        T: TransportFactory + 'static,
        H: HttpClient + 'static,
    {
        Self {
            make: Arc::new(move || {
                let transport: Arc<dyn TransportFactory> = Arc::new(transport());
                let http: Arc<dyn HttpClient> = Arc::new(http());
                (transport, http)
            }),
            logout_wait: None,
            hook: None,
            host_name: String::new(),
        }
    }

    /// The host name the device is listed by on the phone (`Kourier (<host>)`), as the application
    /// reads it: `kourier-app` has ONE rule for it. The name is cleaned and cut where the label is made
    /// (`props::device_name`).
    #[must_use]
    pub fn with_host_name(mut self, host: impl Into<String>) -> Self {
        self.host_name = host.into();
        self
    }

    /// Parts that refuse every call, for a test of the APPLICATION that must start the network and
    /// show that nothing was connected or made (W2b): the transport factory refuses to dial and the
    /// HTTP client refuses every request, and `made` counts how often the maker was called, which is
    /// the number of clients made (the actor calls it at its first `Connect` or `StartLogin`, never
    /// at the start). Nothing here reaches a server.
    #[doc(hidden)]
    pub fn refusing(made: Arc<AtomicUsize>) -> Self {
        Self::from_makers(
            move || {
                made.fetch_add(1, Ordering::SeqCst);
                RefusedDial
            },
            || RefusedHttp,
        )
    }

    /// How long a client waits for an authenticated session before a logout asks the server to
    /// remove the device (the default is 20 s). A test shortens it; nothing needs to lengthen it.
    #[must_use]
    pub fn with_logout_wait(mut self, wait: Duration) -> Self {
        self.logout_wait = Some(wait);
        self
    }

    /// THE SEAM OF THE SCRIPTED SERVER (decision L3, critic H4): a step applied to the client's
    /// builder just before it is built, so a test can switch the noise certificate check off and fix
    /// the WhatsApp Web version (no fetch) for a server that cannot sign a WhatsApp-rooted chain.
    /// Only `tests/support/upstream/wire.rs` calls it: the source check (`tests/builder.rs`) refuses
    /// the name in every other file of every crate, so no production path can weaken a client.
    #[doc(hidden)]
    #[must_use]
    pub fn with_builder_hook(
        mut self,
        hook: impl Fn(Builder) -> Builder + Send + Sync + 'static,
    ) -> Self {
        self.hook = Some(Arc::new(hook));
        self
    }

    /// The fixture's builder step, when one was set.
    pub(crate) fn builder_hook(&self) -> Option<Hook> {
        self.hook.clone()
    }

    /// The host name the application gave (empty: none).
    pub(crate) fn host_name(&self) -> &str {
        &self.host_name
    }

    /// The configured wait, when one was set.
    pub(crate) fn logout_wait(&self) -> Option<Duration> {
        self.logout_wait
    }

    /// Makes the pair for ONE client: the actor's call, on its own thread, at the first `Connect`
    /// or `StartLogin` (`RealFactory::make`).
    pub(crate) fn make(&self) -> (Arc<dyn TransportFactory>, Arc<dyn HttpClient>) {
        (self.make)()
    }
}

/// The transport factory of [`NetParts::refusing`].
struct RefusedDial;

#[async_trait]
impl TransportFactory for RefusedDial {
    async fn create_transport(
        &self,
    ) -> Result<(Arc<dyn Transport>, async_channel::Receiver<TransportEvent>), anyhow::Error> {
        Err(anyhow::anyhow!("the dial is refused"))
    }
}

/// The HTTP client of [`NetParts::refusing`].
struct RefusedHttp;

#[async_trait]
impl HttpClient for RefusedHttp {
    async fn execute(&self, _request: HttpRequest) -> anyhow::Result<HttpResponse> {
        Err(anyhow::anyhow!("the request is refused"))
    }
}

impl std::fmt::Debug for NetParts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NetParts")
    }
}
