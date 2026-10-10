//! The network seam (§6.1, §6.4): the [`SignalTransport`] trait the rest of the backend talks to,
//! and [`libsignal::LibsignalTransport`], its implementation on libsignal-net. Every
//! libsignal-net type stays inside this module; the types below are Kourier's own, so an
//! lss-style transport over plain WebSocket/HTTP could replace libsignal-net without touching
//! receive or send logic.
//!
//! Desktop sources: `SD/ts/textsecure/SocketManager.preload.ts` (sockets, raw REST over the chat
//! websocket), `WebsocketResources.preload.ts` (incoming requests, acks), `WebAPI.preload.ts`
//! (storage and CDN over HTTPS).

pub(crate) mod env;
pub(crate) mod https;
pub(crate) mod libsignal;
mod runner;

#[cfg(test)]
pub(crate) use runner::UPLOAD_TAIL_STALLS;
pub(crate) use runner::{Net, json_request, parse_json, with_basic_auth};

use std::fmt;
use std::future::Future;
use std::time::Duration;

use bytes::Bytes;
use http::uri::PathAndQuery;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use libsignal_core::{Aci, DeviceId};
use tokio::sync::mpsc;
use zeroize::Zeroizing;

pub(crate) use env::SignalEnv;
pub use env::{LocalTestEnv, SignalEnvironment};
pub(crate) use https::{
    DownloadSink, HttpsClient, HttpsHost, HttpsRequest, HttpsResponse, UploadBody, UploadRequest,
};

/// Product token of the user agent. libsignal-net appends ` libsignal/<ver>`, giving the honest
/// `Kourier/<ver> libsignal/<ver>` of §6.1; Kourier never imitates Signal-Desktop.
pub(crate) const USER_AGENT_PRODUCT: &str = concat!("Kourier/", env!("CARGO_PKG_VERSION"));

/// The full user agent, as sent on chat sockets and HTTPS requests.
pub(crate) fn user_agent() -> String {
    format!("{USER_AGENT_PRODUCT} libsignal/{}", libsignal_core::VERSION)
}

/// Default timeout of a request over a chat socket (Desktop's `sendRequest` default is 10 s for
/// most calls; uploads and long polls pass their own).
pub(crate) const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Path of the keepalive request (`SD/ts/textsecure/WebsocketResources.preload.ts`).
pub(crate) const KEEPALIVE_PATH: &str = "/v1/keepalive";

/// Credentials of the authenticated chat socket: Basic auth `<aci>.<device_id>:<password>`.
pub(crate) struct AuthCredentials {
    /// Our ACI.
    pub(crate) aci: Aci,
    /// Our device id on the account.
    pub(crate) device_id: DeviceId,
    /// The device password chosen at link time.
    pub(crate) password: Zeroizing<String>,
}

impl fmt::Debug for AuthCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthCredentials")
            .field("aci", &crate::service_id::SignalUser::Aci(self.aci))
            .field("device_id", &u32::from(self.device_id))
            .finish_non_exhaustive()
    }
}

/// Why a transport operation failed. Every message is log-safe (§9.1): details come from
/// libsignal's `LogSafeDisplay` errors or reqwest errors with the URL stripped.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum TransportError {
    /// The request or connection attempt timed out.
    #[error("timed out")]
    Timeout,
    /// The socket is closed.
    #[error("connection closed")]
    Disconnected,
    /// The server closed the socket with 4409: another client connected with our credentials.
    #[error("connected elsewhere with the same credentials")]
    ConnectedElsewhere,
    /// The server closed the socket with 4401.
    #[error("connection invalidated by the server")]
    ConnectionInvalidated,
    /// The server refused the authenticated socket with 403: this device was unlinked.
    #[error("device is not registered")]
    DeviceDeregistered,
    /// The server refused the socket with 499: this client version is too old.
    #[error("client version expired")]
    AppExpired,
    /// The server asked us to back off.
    #[error("rate limited; retry after {retry_after_secs} s")]
    RetryLater {
        /// Seconds from the `Retry-After` header.
        retry_after_secs: u32,
    },
    /// The server wants a challenge solved before it takes more sends (428 on a typed send).
    #[error("the server asks for a challenge")]
    Challenge {
        /// The challenge token (secret; redacted in `Debug`).
        token: kourier_model::Redacted<String>,
        /// A captcha is among the ways the server accepts: a person here can answer it.
        captcha: bool,
        /// Seconds from the `Retry-After` header, when the server sent one.
        retry_after_secs: Option<u32>,
    },
    /// The server refused the websocket upgrade with this HTTP status (e.g. 401).
    #[error("server rejected the connection with HTTP {status}")]
    Rejected {
        /// The HTTP status of the refused upgrade.
        status: u16,
    },
    /// No route reached the server.
    #[error("server unreachable: {0}")]
    Unreachable(String),
    /// The request could not be built (bad path or header).
    #[error("invalid request: {0}")]
    InvalidRequest(&'static str),
    /// The server's answer could not be understood.
    #[error("invalid response: {0}")]
    InvalidResponse(String),
    /// An HTTPS response body is longer than the request allowed.
    #[error("response body over the limit")]
    BodyTooLarge,
    /// The environment or TLS configuration is invalid.
    #[error("invalid transport configuration: {0}")]
    Config(String),
    /// An HTTPS (storage or CDN) request failed below HTTP.
    #[error("https: {0}")]
    Https(String),
    /// The server failed the request on its side (retry with backoff).
    #[error("server-side error")]
    ServerSide,
}

/// A REST request over a chat socket (`WebSocketRequestMessage`), the way Desktop sends every
/// chat-server call (`SocketManager.preload.ts:452-504`).
#[derive(Clone)]
pub(crate) struct ChatRequest {
    /// HTTP verb.
    pub(crate) method: Method,
    /// Path and query, e.g. `/v2/keys?identity=aci`.
    pub(crate) path: PathAndQuery,
    /// Extra headers (the socket adds auth and user agent itself).
    pub(crate) headers: HeaderMap,
    /// Request body.
    pub(crate) body: Option<Bytes>,
    /// How long to wait for the response.
    pub(crate) timeout: Duration,
}

impl ChatRequest {
    /// A request without body or extra headers, with [`DEFAULT_REQUEST_TIMEOUT`].
    pub(crate) fn new(method: Method, path: &str) -> Result<Self, TransportError> {
        if !path.starts_with('/') {
            return Err(TransportError::InvalidRequest("path must start with '/'"));
        }
        let path = PathAndQuery::try_from(path)
            .map_err(|_| TransportError::InvalidRequest("malformed path"))?;
        Ok(Self {
            method,
            path,
            headers: HeaderMap::new(),
            body: None,
            timeout: DEFAULT_REQUEST_TIMEOUT,
        })
    }

    /// `GET path`.
    pub(crate) fn get(path: &str) -> Result<Self, TransportError> {
        Self::new(Method::GET, path)
    }

    /// `method path` with a JSON body and `content-type: application/json`.
    pub(crate) fn json(
        method: Method,
        path: &str,
        body: impl Into<Bytes>,
    ) -> Result<Self, TransportError> {
        let mut request = Self::new(method, path)?;
        request.headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        request.body = Some(body.into());
        Ok(request)
    }

    /// `GET /v1/keepalive`, the liveness probe sent every 30 s (§6.4), answered within
    /// `timeout` or the socket counts as dead.
    pub(crate) fn keepalive(timeout: Duration) -> Self {
        Self {
            method: Method::GET,
            path: PathAndQuery::from_static(KEEPALIVE_PATH),
            headers: HeaderMap::new(),
            body: None,
            timeout,
        }
    }

    /// Adds a header.
    pub(crate) fn with_header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.headers.append(name, value);
        self
    }

    /// Replaces the timeout.
    pub(crate) fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The path with every segment that could carry an id, number or token replaced by `…`, and
    /// no query: `/v2/keys/<aci>/*` logs as `/v2/keys/…/…`.
    pub(crate) fn log_safe_path(&self) -> String {
        log_safe_path(self.path.path())
    }
}

/// Log-safe `Debug`: verb, redacted path, body size (§9.1).
impl fmt::Debug for ChatRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatRequest")
            .field("method", &self.method)
            .field("path", &self.log_safe_path())
            .field("body_len", &self.body.as_ref().map_or(0, Bytes::len))
            .finish_non_exhaustive()
    }
}

/// Keeps path segments that look like fixed route words (`v1`, `keepalive`, `devices`), and
/// replaces anything else (UUIDs, numbers, base64, timestamps) with `…`.
fn log_safe_path(path: &str) -> String {
    let fixed_word = |segment: &str| {
        let mut bytes = segment.bytes();
        let first_ok = bytes.next().is_some_and(|b| b.is_ascii_lowercase());
        let digits = segment.bytes().filter(u8::is_ascii_digit).count();
        first_ok
            && segment.len() <= 24
            && digits <= 1
            && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    };
    path.split('/')
        .map(|segment| {
            if segment.is_empty() || fixed_word(segment) {
                segment
            } else {
                "…"
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// The server's response to a [`ChatRequest`].
#[derive(Clone)]
pub(crate) struct ChatResponse {
    /// HTTP status.
    pub(crate) status: StatusCode,
    /// The status message, when the server sent one.
    #[allow(dead_code, reason = "diagnostics of failed requests")]
    pub(crate) message: Option<String>,
    /// Response headers.
    #[allow(dead_code, reason = "Retry-After and challenge handling arrive in S8")]
    pub(crate) headers: HeaderMap,
    /// Response body (empty when absent).
    pub(crate) body: Bytes,
}

impl fmt::Debug for ChatResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatResponse")
            .field("status", &self.status)
            .field("body_len", &self.body.len())
            .finish_non_exhaustive()
    }
}

/// The one-shot answer to a server push (an envelope, a provisioning address or message).
/// Dropping it without answering leaves the push unacknowledged, so the server redelivers it
/// on the next connection: this is how "nothing is acked before it is durable" is kept (§5.6).
pub(crate) struct ServerAck(Box<dyn FnOnce(StatusCode) -> Result<(), TransportError> + Send>);

impl ServerAck {
    /// Wraps a transport's responder.
    pub(crate) fn new(
        respond: impl FnOnce(StatusCode) -> Result<(), TransportError> + Send + 'static,
    ) -> Self {
        Self(Box::new(respond))
    }

    /// Acknowledges with 200: the server deletes the push.
    pub(crate) fn ack(self) -> Result<(), TransportError> {
        self.respond(StatusCode::OK)
    }

    /// Answers with any status (Desktop answers undecodable requests with 400/500).
    pub(crate) fn respond(self, status: StatusCode) -> Result<(), TransportError> {
        (self.0)(status)
    }
}

impl fmt::Debug for ServerAck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ServerAck")
    }
}

/// An envelope pushed on the authenticated socket (`PUT /api/v1/message`). `Debug` shows only
/// sizes: unsealed envelopes carry the sender's service id in the clear.
pub(crate) struct IncomingEnvelope {
    /// The serialized `Envelope` (may be empty or malformed; it still has to be acked).
    pub(crate) bytes: Bytes,
    /// `x-signal-timestamp` of the delivery, in ms since the epoch (0 when absent).
    pub(crate) server_delivery_timestamp: u64,
    /// Explicit acknowledgment, sent only after the envelope is durable.
    pub(crate) ack: ServerAck,
}

impl fmt::Debug for IncomingEnvelope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IncomingEnvelope")
            .field("len", &self.bytes.len())
            .field("server_delivery_timestamp", &self.server_delivery_timestamp)
            .finish_non_exhaustive()
    }
}

/// Why a socket stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StopReason {
    /// We disconnected it.
    Local,
    /// Close 4409: another client connected with our credentials (no auto-reconnect, §6.4).
    ConnectedElsewhere,
    /// Close 4401.
    ConnectionInvalidated,
    /// The server or network closed it; reconnect with backoff.
    Closed(String),
}

/// What a chat socket (authenticated or not) reports, in server order.
#[derive(Debug)]
pub(crate) enum ChatEvent {
    /// An envelope to process and then ack.
    Envelope(IncomingEnvelope),
    /// The server has delivered everything that was queued (`PUT /api/v1/queue/empty`).
    QueueEmpty,
    /// The server's clock, ms since the epoch (clock-skew check, §6.4).
    ServerTimestamp(u64),
    /// Alerts from the connect response (`x-signal-alert`), e.g. `idle-primary-device`.
    Alerts(Vec<String>),
    /// The socket is gone; no more events follow.
    Stopped(StopReason),
}

/// What the provisioning socket reports (§6.5 steps 2-3). `Debug` hides the address (QR
/// content, §9.1).
pub(crate) enum ProvisioningEvent {
    /// The provisioning address for the QR code (`PUT /v1/address`); ack with 200.
    Address {
        /// Opaque address; not log-safe (it is QR content).
        address: String,
        /// Acknowledgment.
        ack: ServerAck,
    },
    /// The phone's encrypted `ProvisionEnvelope` (`PUT /v1/message`); ack with 200.
    Envelope {
        /// The serialized `ProvisionEnvelope`.
        bytes: Bytes,
        /// Acknowledgment.
        ack: ServerAck,
    },
    /// The server's clock, ms since the epoch.
    ServerTimestamp(u64),
    /// The socket is gone.
    Stopped(StopReason),
}

impl fmt::Debug for ProvisioningEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Address { .. } => f.write_str("Address([redacted])"),
            Self::Envelope { bytes, .. } => write!(f, "Envelope({} bytes)", bytes.len()),
            Self::ServerTimestamp(timestamp) => {
                f.debug_tuple("ServerTimestamp").field(timestamp).finish()
            }
            Self::Stopped(reason) => f.debug_tuple("Stopped").field(reason).finish(),
        }
    }
}

/// A connected socket and the receiver of everything it pushes. The channel is unbounded on
/// purpose (review fix in §6.4): the listener runs synchronously inside libsignal-net and can
/// neither await nor drop an envelope; backpressure is applied by delaying acks instead.
pub(crate) struct Connected<S, E> {
    /// The socket for requests.
    pub(crate) socket: S,
    /// Server pushes in order; closed after [`ChatEvent::Stopped`]/[`ProvisioningEvent::Stopped`].
    pub(crate) events: mpsc::UnboundedReceiver<E>,
}

/// A sealed sender-key message for several recipients (§6.7 step 5), sent on an
/// unauthenticated socket with a group send token.
#[derive(Clone)]
pub(crate) struct MultiRecipientSend {
    /// The `sealed_sender_multi_recipient_encrypt` output.
    pub(crate) payload: Bytes,
    /// The message timestamp (ms).
    pub(crate) timestamp: u64,
    /// The serialized `GroupSendFullToken` for exactly the recipients in `payload` (secret).
    pub(crate) group_send_token: Zeroizing<Vec<u8>>,
    /// Only to connected devices (typing).
    pub(crate) online: bool,
    /// The server may wake the recipients' phones.
    pub(crate) urgent: bool,
}

impl fmt::Debug for MultiRecipientSend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MultiRecipientSend")
            .field("payload_len", &self.payload.len())
            .field("timestamp", &self.timestamp)
            .field("online", &self.online)
            .field("urgent", &self.urgent)
            .finish_non_exhaustive()
    }
}

/// A recipient's devices did not match the server's (409/410 of a multi-recipient send).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeviceMismatch {
    /// The recipient.
    pub(crate) account: libsignal_core::ServiceId,
    /// Devices the recipient has and the payload lacked.
    pub(crate) missing: Vec<DeviceId>,
    /// Devices in the payload the recipient no longer has.
    pub(crate) extra: Vec<DeviceId>,
    /// Devices whose registration id changed (re-registered or re-linked).
    pub(crate) stale: Vec<DeviceId>,
}

/// The server's answer to a [`MultiRecipientSend`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MultiRecipientOutcome {
    /// Accepted for every recipient except `unregistered` (no longer on Signal).
    Sent {
        /// Recipients that are not registered.
        unregistered: Vec<libsignal_core::ServiceId>,
    },
    /// The group send token was refused (401): the members get 1:1 sends instead.
    Unauthorized,
    /// Some recipients' device lists are out of date; nothing was sent.
    MismatchedDevices(Vec<DeviceMismatch>),
}

/// Where and how to upload one attachment blob (`GET /v4/attachments/form/upload`, §6.7 step 2).
/// `Debug` shows only the CDN: the key, headers and signed URL are credentials.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct UploadForm {
    /// The CDN number (2: GCS resumable, 3: TUS).
    pub(crate) cdn: u32,
    /// The CDN key the pointer names.
    pub(crate) key: String,
    /// Headers every upload request carries.
    pub(crate) headers: Vec<(String, String)>,
    /// The signed upload location.
    pub(crate) signed_upload_url: String,
}

impl fmt::Debug for UploadForm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UploadForm")
            .field("cdn", &self.cdn)
            .finish_non_exhaustive()
    }
}

/// The server refused an upload form: the blob is larger than it accepts (413).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct UploadTooLarge;

/// One open chat socket. Requests are answered in any order; the socket is `Sync`, so callers
/// can share it.
pub(crate) trait ChatSocket: Send + Sync + 'static {
    /// Sends a REST request over the socket and waits for its response.
    fn request(
        &self,
        request: ChatRequest,
    ) -> impl Future<Output = Result<ChatResponse, TransportError>> + Send;

    /// Sends a multi-recipient sealed message (an unauthenticated socket only). Uses the
    /// transport's typed API (gRPC where the server offers it, else the websocket).
    fn send_multi_recipient(
        &self,
        send: MultiRecipientSend,
    ) -> impl Future<Output = Result<MultiRecipientOutcome, TransportError>> + Send;

    /// Asks for an attachment upload form for a blob of `upload_length` bytes (an
    /// authenticated socket only), over the transport's typed API.
    fn get_upload_form(
        &self,
        upload_length: u64,
    ) -> impl Future<Output = Result<Result<UploadForm, UploadTooLarge>, TransportError>> + Send;

    /// Closes the socket; its event stream ends with `Stopped(Local)`.
    fn disconnect(&self) -> impl Future<Output = ()> + Send;
}

/// The seam between the Signal backend and the network (§6.1), implemented by
/// [`libsignal::LibsignalTransport`]. Futures are `Send`: the account actor runs them on the
/// shared network runtime and awaits the join handle (§6.3).
pub(crate) trait SignalTransport: Send + Sync + 'static {
    /// The socket type this transport opens.
    type Socket: ChatSocket;

    /// Opens the authenticated chat socket. Its events carry incoming envelopes.
    fn connect_auth(
        &self,
        credentials: &AuthCredentials,
    ) -> impl Future<Output = Result<Connected<Self::Socket, ChatEvent>, TransportError>> + Send;

    /// Opens an unauthenticated chat socket (sealed sends, prekey and profile fetches, link).
    fn connect_unauth(
        &self,
    ) -> impl Future<Output = Result<Connected<Self::Socket, ChatEvent>, TransportError>> + Send;

    /// Opens a provisioning socket for linking.
    fn connect_provisioning(
        &self,
    ) -> impl Future<Output = Result<Connected<Self::Socket, ProvisioningEvent>, TransportError>> + Send;

    /// Sends an HTTPS request to the Storage Service or a CDN.
    fn https(
        &self,
        request: HttpsRequest,
    ) -> impl Future<Output = Result<HttpsResponse, TransportError>> + Send;

    /// Streams the body of a successful HTTPS answer into `sink` (attachment downloads, §6.11);
    /// returns the status (the body of a failure is not read).
    fn https_download(
        &self,
        request: HttpsRequest,
        sink: DownloadSink,
    ) -> impl Future<Output = Result<StatusCode, TransportError>> + Send;

    /// Sends an upload request to a CDN's signed location (§6.7 step 2).
    fn https_upload(
        &self,
        request: UploadRequest,
    ) -> impl Future<Output = Result<HttpsResponse, TransportError>> + Send;

    /// Tells the transport the network changed (resets connection-attempt history and DNS).
    fn network_changed(&self);

    /// Hands the transport the account's remote config (`GET /v2/config`, the entries Kourier
    /// keeps): what in it steers the transport itself (libsignal-net's gRPC switches, §6.4)
    /// takes effect for sockets opened from now on.
    fn remote_config_changed(&self, _config: &std::collections::BTreeMap<String, String>) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agent_is_honest_and_pins_libsignal() {
        assert_eq!(
            libsignal_core::VERSION,
            "0.104.0",
            "bump §6.15 docs together"
        );
        assert_eq!(
            user_agent(),
            format!("Kourier/{} libsignal/0.104.0", env!("CARGO_PKG_VERSION"))
        );
        assert!(!user_agent().contains("Signal-Desktop"));
    }

    #[test]
    fn requests_validate_paths_and_build_json() {
        assert_eq!(
            ChatRequest::get("v1/config").unwrap_err(),
            TransportError::InvalidRequest("path must start with '/'")
        );
        assert!(ChatRequest::get("/v1/a b").is_err());

        let request = ChatRequest::json(Method::PUT, "/v1/devices/link", b"{}".to_vec())
            .unwrap()
            .with_timeout(Duration::from_secs(3));
        assert_eq!(request.method, Method::PUT);
        assert_eq!(request.path.as_str(), "/v1/devices/link");
        assert_eq!(
            request.headers[http::header::CONTENT_TYPE],
            "application/json"
        );
        assert_eq!(request.body.as_deref(), Some(&b"{}"[..]));
        assert_eq!(request.timeout, Duration::from_secs(3));

        let keepalive = ChatRequest::keepalive(Duration::from_secs(30));
        assert_eq!(keepalive.path.as_str(), KEEPALIVE_PATH);
        assert_eq!(keepalive.method, Method::GET);
        assert_eq!(keepalive.timeout, Duration::from_secs(30));
    }

    #[test]
    fn request_debug_redacts_ids_and_queries() {
        let request =
            ChatRequest::get("/v2/keys/9d0652a3-dcc3-4d11-975f-74d61598733f/*?pq=true").unwrap();
        let debug = format!("{request:?}");
        assert!(debug.contains("/v2/keys/…/…"), "{debug}");
        assert!(!debug.contains("9d06") && !debug.contains("pq"), "{debug}");
        assert_eq!(log_safe_path("/v1/profile/+14155550101"), "/v1/profile/…");
        assert_eq!(
            log_safe_path("/v1/messages/1712345678901"),
            "/v1/messages/…"
        );
        assert_eq!(
            log_safe_path("/v1/devices/capabilities"),
            "/v1/devices/capabilities"
        );
        assert_eq!(log_safe_path("/v1/storage/aGVsbG8"), "/v1/storage/…");
    }

    #[test]
    fn credentials_debug_hides_password_and_aci() {
        let credentials = AuthCredentials {
            aci: Aci::from(uuid::Uuid::from_u128(0xabcd_ef01)),
            device_id: DeviceId::new(2).unwrap(),
            password: Zeroizing::new("hunter2".to_owned()),
        };
        let debug = format!("{credentials:?}");
        assert!(
            !debug.contains("hunter2") && !debug.contains("abcdef01"),
            "{debug}"
        );
        assert!(debug.contains("ef01"), "{debug}");
    }

    #[test]
    fn push_debug_is_log_safe() {
        let ack = || ServerAck::new(|_| Ok(()));
        let address = ProvisioningEvent::Address {
            address: "b2ZmLXRoZS1yZWNvcmQ".to_owned(),
            ack: ack(),
        };
        assert_eq!(format!("{address:?}"), "Address([redacted])");
        let envelope = ChatEvent::Envelope(IncomingEnvelope {
            bytes: Bytes::from_static(b"aci:9d0652a3"),
            server_delivery_timestamp: 5,
            ack: ack(),
        });
        let debug = format!("{envelope:?}");
        assert!(
            !debug.contains("9d06") && debug.contains("len: 12"),
            "{debug}"
        );
    }

    #[test]
    fn server_ack_delivers_the_chosen_status_once() {
        let (tx, rx) = std::sync::mpsc::channel();
        let ack =
            ServerAck::new(move |status| tx.send(status).map_err(|_| TransportError::Disconnected));
        ack.respond(StatusCode::BAD_REQUEST).unwrap();
        assert_eq!(rx.recv().unwrap(), StatusCode::BAD_REQUEST);
        assert!(rx.try_recv().is_err());
    }
}
