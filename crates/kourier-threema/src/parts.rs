//! [`ThreemaParts`]: what a Threema client cannot be made without, as a LAZY maker: a WebSocket
//! connector (the mediator, the rendezvous relay of a link) and an HTTPS client (the directory, the
//! blob mirror), plus the servers' addresses and the chat server's public keys as plain data
//! ([`ServerParams`]).
//!
//! [`ThreemaNetwork::new`] takes the parts as a REQUIRED argument: there is no constructor without
//! them. The traits are this crate's own ([`WsConnector`], [`HttpsClient`]), so the protocol code
//! never names a socket: the one module that makes the real connectors is `src/dial/` (TH3),
//! and the source check (`tests/builder.rs`) keeps every other file, tests included, from naming a
//! socket type, a TLS or HTTP crate or a Threema host. A test hands in guards that refuse and count
//! (`tests/support/guards.rs`), or the offline fake (TH2).
//!
//! The maker is called by the actor on its own thread when an account first connects or starts a
//! link, never at the start of the application or of an actor.
//!
//! No libthreema type appears here: the actor turns [`ServerParams`] into libthreema's own
//! configuration where it makes a client.
//!
//! [`ThreemaNetwork::new`]: crate::ThreemaNetwork::new

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use crate::allow::AllowList;

/// The maker the actor calls: one connector and one HTTPS client per client.
type Make = dyn Fn() -> Transports + Send + Sync;

/// What one client is made with, from [`ThreemaParts`]'s maker.
#[derive(Clone)]
pub struct Transports {
    /// The WebSocket connector (mediator, rendezvous relay).
    pub ws: Arc<dyn WsConnector>,
    /// The HTTPS client (directory, blob mirror).
    pub https: Arc<dyn HttpsClient>,
}

impl std::fmt::Debug for Transports {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Transports")
    }
}

/// The servers a client talks to, as plain data: read ONLY from the parts, never from a constant of
/// the protocol code. The real values are libthreema's production configuration (`src/dial/`, TH3); the test
/// parts name `.invalid` hosts (RFC 6761), so a misrouted dial could not even resolve.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ServerParams {
    /// The mediator's WebSocket URL, with libthreema's placeholders for the device group.
    pub mediator_url: String,
    /// The rendezvous relay's WebSocket URL of a link.
    pub rendezvous_url: String,
    /// The directory server's base URL.
    pub directory_url: String,
    /// The blob mirror's download URL, with libthreema's placeholders for the device group and the blob.
    pub blob_url: String,
    /// The blob mirror's URL that marks a blob as downloaded, with the same placeholders.
    pub blob_done_url: String,
    /// The blob mirror's upload URL, with the placeholders for the device group.
    pub blob_upload_url: String,
    /// The chat server's public keys, the primary one first (X25519, 32 bytes each).
    pub chat_server_keys: Vec<[u8; 32]>,
}

impl std::fmt::Debug for ServerParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ServerParams")
    }
}

/// Which server a WebSocket goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WsKind {
    /// The mediator (the account's connection; the chat server is reached through it).
    Mediator,
    /// The rendezvous relay of a link.
    Rendezvous,
}

/// One WebSocket to open.
#[derive(Clone, PartialEq, Eq)]
pub struct WsRequest {
    /// The full URL, path included (the mediator's path names the device group).
    pub url: String,
    /// Which server it is.
    pub kind: WsKind,
}

impl std::fmt::Debug for WsRequest {
    // The URL's path names the device group: only the kind is printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsRequest")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// What arrives on an open WebSocket.
#[derive(Clone, PartialEq, Eq)]
pub enum WsEvent {
    /// One binary frame.
    Binary(Vec<u8>),
    /// The socket closed, with the close frame's code and reason (empty without one).
    Closed {
        /// The close code (the mediator's 4xxx codes say why).
        code: u16,
        /// The close reason.
        reason: String,
    },
}

impl std::fmt::Debug for WsEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Binary(frame) => write!(f, "Binary({} bytes)", frame.len()),
            Self::Closed { code, .. } => write!(f, "Closed({code})"),
        }
    }
}

/// Why a connection or a request failed. Log-safe: an operation's name and a kind, never a URL or
/// a payload.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NetError {
    /// The parts refuse every call (a guard, or the connectors that do not exist yet).
    #[error("{0}: refused")]
    Refused(&'static str),
    /// The connection or the request failed (the server was not reached, or the socket broke).
    #[error("{0}: failed")]
    Failed(&'static str),
    /// The server is not the one Kourier's pins name (TH3, decision C8): a held state for the user
    /// (`NeedsUserAction(UpdateApp)`), never a loop of attempts.
    #[error("certificate: untrusted ({0:?})")]
    Untrusted(UntrustedKind),
    /// A PINNED certificate outside its dates: an expired certificate, or a wrong local clock.
    /// Transient: retried with backoff (decision C8).
    #[error("certificate: outside its dates")]
    CertificateDates,
}

/// Why a server was not trusted (log-safe: a kind, never the certificate or the host).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UntrustedKind {
    /// The URL is not `wss://` / `https://`.
    NotTls,
    /// The host is not one label under the pinned domain (checked before any DNS lookup).
    UnpinnedHost,
    /// The leaf's key is none of the pins (every pin failed).
    PinMismatch,
    /// A pinned key under a CA no known root vouches for.
    UnknownIssuer,
    /// A pinned leaf for another host.
    BadName,
    /// Anything else wrong with the certificate.
    BadCertificate,
}

/// The sending half of an open WebSocket.
#[async_trait]
pub trait WsSender: Send + Sync {
    /// Sends one binary frame. It may PEND as long as the peer does not read (TCP backpressure): the
    /// caller bounds it (the join's frame write and the close are bounded on the actor's clock, P8).
    async fn send(&self, frame: Vec<u8>) -> Result<(), NetError>;
    /// Closes the socket with a code and a reason (the same: the caller bounds it).
    async fn close(&self, code: u16, reason: String) -> Result<(), NetError>;
}

/// Opens WebSockets. The real one (TH3) pins the servers' certificates; a test's is a guard or the
/// fake's in-process pipe.
#[async_trait]
pub trait WsConnector: Send + Sync {
    /// Opens one WebSocket: its sending half and the stream of what arrives.
    async fn connect(
        &self,
        request: WsRequest,
    ) -> Result<(Box<dyn WsSender>, async_channel::Receiver<WsEvent>), NetError>;
}

/// The method of an HTTPS request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpsMethod {
    /// GET.
    Get,
    /// POST.
    Post,
    /// PUT.
    Put,
}

/// One HTTPS request.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpsRequest {
    /// The method.
    pub method: HttpsMethod,
    /// The full URL.
    pub url: String,
    /// The headers, in order.
    pub headers: Vec<(String, String)>,
    /// The body (empty for none).
    pub body: Vec<u8>,
    /// The largest response body taken (`None`: the real client's default, 4 MiB); a body over it
    /// fails the request (TH3 fix S-L2).
    pub max_body: Option<usize>,
}

impl std::fmt::Debug for HttpsRequest {
    // The URL and the body may name an ID or carry a key: only the method is printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpsRequest")
            .field("method", &self.method)
            .finish_non_exhaustive()
    }
}

/// One HTTPS response.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpsResponse {
    /// The status code.
    pub status: u16,
    /// The headers, in order.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl std::fmt::Debug for HttpsResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpsResponse")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

/// Makes HTTPS requests.
#[async_trait]
pub trait HttpsClient: Send + Sync {
    /// Sends one request and reads the whole response.
    async fn execute(&self, request: HttpsRequest) -> Result<HttpsResponse, NetError>;
}

/// What a client is made with. Cheap to clone (the maker is shared); it prints nothing.
#[derive(Clone)]
pub struct ThreemaParts {
    make: Arc<Make>,
    server: ServerParams,
    /// This computer's host name as the APPLICATION reads it: the one source of the label the phone
    /// lists this device by (`Kourier (<host>)`, [`crate::ThreemaNetwork::device_label`]). The crate
    /// reads no host name of its own. Empty: the label is plain `Kourier`.
    host_name: String,
    /// The Threema IDs a link may bring (M6; EMPTY by default: every link is refused at once, B1).
    allowed: AllowList,
}

impl ThreemaParts {
    /// Parts from a maker of each transport and the servers' data. Neither maker is called here:
    /// `from_makers` builds nothing.
    pub fn from_makers<W, H>(
        ws: impl Fn() -> W + Send + Sync + 'static,
        https: impl Fn() -> H + Send + Sync + 'static,
        server: ServerParams,
    ) -> Self
    where
        W: WsConnector + 'static,
        H: HttpsClient + 'static,
    {
        Self {
            make: Arc::new(move || Transports {
                ws: Arc::new(ws()),
                https: Arc::new(https()),
            }),
            server,
            host_name: String::new(),
            allowed: AllowList::default(),
        }
    }

    /// The host name the device is listed by on the phone (`Kourier (<host>)`), as the application
    /// reads it: `kourier-app` has ONE rule for it. The label is cleaned and cut where it is made.
    #[must_use]
    pub fn with_host_name(mut self, host: impl Into<String>) -> Self {
        self.host_name = host.into();
        self
    }

    /// The Threema IDs a link may bring (TH4 decision B1, M6): the application's list from a prepared
    /// run profile, a test's synthetic IDs. Without it the list is EMPTY and `StartLogin` fails at
    /// once, before any transport is made.
    #[must_use]
    pub fn with_allowed(mut self, allowed: AllowList) -> Self {
        self.allowed = allowed;
        self
    }

    /// The Threema IDs a link may bring.
    #[must_use]
    pub fn allowed(&self) -> &AllowList {
        &self.allowed
    }

    /// Parts that refuse every call, for a test of the APPLICATION that must start the network and
    /// show that nothing was connected or made: the connector refuses every WebSocket, the HTTPS
    /// client every request, and `made` counts how often the maker was called (the number of
    /// clients made). The servers' data is empty. Nothing here reaches a server.
    #[doc(hidden)]
    pub fn refusing(made: Arc<AtomicUsize>) -> Self {
        Self::from_makers(
            move || {
                made.fetch_add(1, Ordering::SeqCst);
                Refused
            },
            || Refused,
            ServerParams::default(),
        )
    }

    /// The servers' data (read where a client is made, TH3).
    pub(crate) fn server(&self) -> &ServerParams {
        &self.server
    }

    /// The host name the application gave (empty: none).
    pub(crate) fn host_name(&self) -> &str {
        &self.host_name
    }

    /// Makes the transports of ONE client: the actor's call, on its own thread, at the first
    /// connect of an account (TH3; a link, TH4).
    pub(crate) fn make(&self) -> Transports {
        (self.make)()
    }
}

impl std::fmt::Debug for ThreemaParts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ThreemaParts")
    }
}

/// The connector and HTTPS client of [`ThreemaParts::refusing`] and of the application's parts
/// (`ThreemaParts::refusing`): every call is refused.
pub(crate) struct Refused;

#[async_trait]
impl WsConnector for Refused {
    async fn connect(
        &self,
        _request: WsRequest,
    ) -> Result<(Box<dyn WsSender>, async_channel::Receiver<WsEvent>), NetError> {
        Err(NetError::Refused("websocket"))
    }
}

#[async_trait]
impl HttpsClient for Refused {
    async fn execute(&self, _request: HttpsRequest) -> Result<HttpsResponse, NetError> {
        Err(NetError::Refused("https"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime")
            .block_on(f)
    }

    /// The refusing parts and the transports they hand out. The maker is NOT called here (review F7):
    /// a test that made a client would belong in the offline child of `tests/network.rs`, and none
    /// needs it; the refusals are checked on [`Refused`] itself.
    #[test]
    fn the_refusing_parts_make_nothing_and_refuse_every_call() {
        let made = Arc::new(AtomicUsize::new(0));
        let parts = ThreemaParts::refusing(made.clone());
        let _copy = parts.clone();
        assert_eq!(
            made.load(Ordering::SeqCst),
            0,
            "building or cloning the parts made a client"
        );
        let ws = run(Refused.connect(WsRequest {
            url: "wss://mediator.invalid/".to_owned(),
            kind: WsKind::Mediator,
        }));
        assert!(
            matches!(ws, Err(NetError::Refused(_))),
            "the refusing connector opened a socket"
        );
        let https = run(Refused.execute(HttpsRequest {
            method: HttpsMethod::Get,
            url: "https://directory.invalid/".to_owned(),
            headers: Vec::new(),
            body: Vec::new(),
            max_body: None,
        }));
        assert_eq!(https, Err(NetError::Refused("https")));
        assert_eq!(made.load(Ordering::SeqCst), 0, "a refusal made a client");
        assert_eq!(parts.server(), &ServerParams::default());
    }

    #[test]
    fn nothing_of_the_parts_prints_a_url_a_key_or_a_body() {
        let server = ServerParams {
            mediator_url: "wss://secret-host.invalid/".to_owned(),
            chat_server_keys: vec![[0x77; 32]],
            ..ServerParams::default()
        };
        let request = HttpsRequest {
            method: HttpsMethod::Post,
            url: "https://secret-host.invalid/TH1QW7KZ".to_owned(),
            headers: vec![("x".to_owned(), "secret-header".to_owned())],
            body: b"secret-body".to_vec(),
            max_body: None,
        };
        let printed = [
            format!("{server:?}"),
            format!("{request:?}"),
            format!(
                "{:?}",
                WsRequest {
                    url: "wss://secret-host.invalid/".to_owned(),
                    kind: WsKind::Rendezvous
                }
            ),
            format!("{:?}", WsEvent::Binary(b"secret-frame".to_vec())),
            format!(
                "{:?}",
                WsEvent::Closed {
                    code: 4000,
                    reason: "secret-reason".to_owned()
                }
            ),
            format!(
                "{:?}",
                HttpsResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: b"secret-body".to_vec()
                }
            ),
            format!(
                "{:?}",
                ThreemaParts::refusing(Arc::default()).with_host_name("secret-host")
            ),
        ];
        for text in &printed {
            assert!(
                !text.contains("secret") && !text.contains("TH1QW7KZ") && !text.contains("119"),
                "a Debug form prints a URL, a key, a header or a body"
            );
        }
    }
}
