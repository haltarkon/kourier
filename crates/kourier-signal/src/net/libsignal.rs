//! [`SignalTransport`] on libsignal-net: Kourier's port of the bridge's `ConnectionManager`
//! (`LS/rust/bridge/shared/types/src/net.rs:116-170`, `net/chat.rs:530-620`) and of
//! `chat::test_support::simple_chat_connection` (`LS/rust/net/src/chat.rs:490-582`).
//!
//! Direct routes only for now; proxies and domain fronting are later settings (§6.4). The
//! remote config's `desktop.libsignalNet.grpc.*` entries choose, per typed request, between
//! gRPC and the websocket ([`grpc_overrides`], as Desktop's `LibsignalNetRemoteConfig`); for a
//! request without an entry libsignal-net uses gRPC.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use http::{HeaderName, StatusCode};
use libsignal_net::chat::server_requests::{
    DisconnectCause, ProvisioningEvent as LsProvisioningEvent, ResponseEnvelopeSender, ServerEvent,
};
use libsignal_net::chat::{
    self, AuthenticatedChatHeaders, ChatConnection, ChatHeaders, ConnectError, GrpcOverride,
    LanguageList, ReceiveStories, SendError, UnauthenticatedChatHeaders, ws,
};
use libsignal_net::connect_state::{
    ConnectState, ConnectionResources, DefaultConnectorFactory, PreconnectingFactory,
    SUGGESTED_CONNECT_CONFIG, SUGGESTED_TLS_PRECONNECT_LIFETIME,
};
use libsignal_net::env::UserAgent;
use libsignal_net::env::constants::{CHAT_PROVISIONING_PATH, CHAT_WEBSOCKET_PATH};
use libsignal_net::infra::dns::DnsResolver;
use libsignal_net::infra::route::{DirectOrProxyMode, DirectOrProxyProvider};
use libsignal_net::infra::ws::{WebSocketConnectError, WebSocketError};
use libsignal_net::infra::{EnableDomainFronting, OverrideNagleAlgorithm};
use tokio::sync::{mpsc, watch};

use super::{
    AuthCredentials, ChatEvent, ChatRequest, ChatResponse, ChatSocket, Connected, DeviceMismatch,
    DownloadSink, HttpsClient, HttpsRequest, HttpsResponse, IncomingEnvelope,
    MultiRecipientOutcome, MultiRecipientSend, ProvisioningEvent, ServerAck, SignalEnv,
    SignalTransport, StopReason, TransportError, USER_AGENT_PRODUCT, UploadForm, UploadRequest,
    UploadTooLarge,
};

/// Network changes closer together than this are coalesced (the bridge's debounce).
const NETWORK_CHANGE_DEBOUNCE: Duration = Duration::from_secs(1);

/// Where Desktop's remote config keeps libsignal-net's entries
/// (`SD/ts/LibsignalNetRemoteConfig.preload.ts` `convertToDesktopRemoteConfigKey`).
const LIBSIGNAL_NET_PREFIX: &str = "desktop.libsignalNet.";

/// libsignal-net's typed requests that have a transport switch: the remote-config key (after
/// [`LIBSIGNAL_NET_PREFIX`]) and the gRPC request name its override is stored under. Copied
/// from `LS/rust/bridge/shared/types/src/net/remote_config.rs:109-119` (v0.104.0; the bridge
/// crate is not a dependency): check it at every libsignal bump (§6.15). An entry libsignal
/// does not know would be ignored there as well.
const GRPC_SWITCHES: [(&str, &str); 9] = [
    (
        "grpc.AccountsAnonymousLookupUsernameHash",
        "AccountsAnonymousLookupUsernameHash",
    ),
    (
        "grpc.AccountsAnonymousLookupUsernameLink.2",
        "AccountsAnonymousLookupUsernameLink",
    ),
    (
        "grpc.AccountsAnonymousCheckAccountExistence.2",
        "AccountsAnonymousCheckAccountExistence",
    ),
    (
        "grpc.MessagesAnonymousSendMultiRecipientMessage.2",
        "MessagesAnonymousSendMultiRecipientMessage",
    ),
    (
        "grpc.MessagesAnonymousSendSingleRecipientMessage",
        "MessagesAnonymousSendSingleRecipientMessage",
    ),
    ("grpc.AttachmentsGetUploadForm", "AttachmentsGetUploadForm"),
    ("grpc.MessagesSendMessage", "MessagesSendMessage"),
    (
        "grpc.BackupsAnonymousGetUploadForm",
        "BackupsAnonymousGetUploadForm",
    ),
    (
        "grpc.KeyTransparencyQueryServiceSearchV2",
        "KeyTransparencyQueryServiceSearchV2",
    ),
];

/// The transport of each typed request the remote config names (the bridge's
/// `ConnectionManager::chat_grpc_overrides`): the value `ws` keeps a request on the websocket,
/// any other value of an enabled entry sends it over gRPC, and a request without an entry is
/// left to libsignal-net (gRPC). `/v2/config` lists enabled entries only; an explicit `false`
/// counts as absent, as everywhere in Kourier's reading of the config.
pub(crate) fn grpc_overrides(
    config: &BTreeMap<String, String>,
) -> HashMap<&'static str, GrpcOverride> {
    GRPC_SWITCHES
        .iter()
        .filter_map(|(key, request)| {
            let value = config.get(&format!("{LIBSIGNAL_NET_PREFIX}{key}"))?;
            if value.eq_ignore_ascii_case("false") {
                return None;
            }
            let transport = if value == "ws" {
                GrpcOverride::UseWs
            } else {
                GrpcOverride::UseGrpc
            };
            Some((*request, transport))
        })
        .collect()
}

/// The libsignal-net transport of one environment. Shared by all accounts on that environment;
/// sockets run their reader/writer tasks on `runtime` (the network runtime of §2.5).
pub(crate) struct LibsignalTransport {
    env: SignalEnv,
    user_agent: UserAgent,
    dns_resolver: DnsResolver,
    connect_state: Mutex<ConnectState<PreconnectingFactory>>,
    network_change_tx: watch::Sender<()>,
    last_network_change: Mutex<Option<Instant>>,
    /// The transport of typed requests on sockets opened from now on ([`grpc_overrides`]).
    grpc_overrides: Mutex<HashMap<&'static str, GrpcOverride>>,
    runtime: tokio::runtime::Handle,
    https: HttpsClient,
}

impl LibsignalTransport {
    /// Builds the transport. Nothing connects until a socket is requested.
    pub(crate) fn new(
        env: SignalEnv,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self, TransportError> {
        let (network_change_tx, network_change_rx) = watch::channel(());
        // System DNS first, then Cloudflare DoH, then the static addresses (libsignal's default
        // order). A local test server's `localhost` resolves from /etc/hosts; a static-map-only
        // resolver exists only behind libsignal-net's `test-util` feature.
        let dns_resolver =
            DnsResolver::new_with_static_fallback(env.chat_static_dns(), &network_change_rx);
        let https = HttpsClient::new(&env)?;
        Ok(Self {
            user_agent: UserAgent::with_libsignal_version(USER_AGENT_PRODUCT),
            dns_resolver,
            connect_state: ConnectState::new_with_transport_connector(
                SUGGESTED_CONNECT_CONFIG,
                PreconnectingFactory::new(
                    DefaultConnectorFactory,
                    SUGGESTED_TLS_PRECONNECT_LIFETIME,
                ),
            ),
            network_change_tx,
            last_network_change: Mutex::new(None),
            grpc_overrides: Mutex::new(HashMap::new()),
            runtime,
            https,
            env,
        })
    }

    /// The environment this transport serves.
    #[allow(
        dead_code,
        reason = "remote config and groups read the environment (S6, S10)"
    )]
    pub(crate) fn env(&self) -> &SignalEnv {
        &self.env
    }

    /// Opens a chat websocket on `path` and hands its events to `listener`.
    async fn establish(
        &self,
        kind: &'static str,
        path: &'static str,
        headers: Option<ChatHeaders>,
        listener: ws::EventListener,
    ) -> Result<ChatConnection, TransportError> {
        let network_change_event = self.network_change_tx.subscribe();
        let chat_connect = &self.env.chat.connect;
        let resources = ConnectionResources {
            connect_state: &self.connect_state,
            dns_resolver: &self.dns_resolver,
            network_change_event: &network_change_event,
            confirmation_header_name: chat_connect
                .confirmation_header_name
                .map(HeaderName::from_static),
        };
        let route_provider = DirectOrProxyProvider {
            inner: chat_connect.route_provider(
                EnableDomainFronting::No,
                OverrideNagleAlgorithm::OverrideToOff,
            ),
            mode: DirectOrProxyMode::DirectOnly,
        };
        tracing::info!(kind, "connecting chat socket");
        let pending = ChatConnection::start_connect_with(
            resources,
            chat_connect.service,
            route_provider,
            path,
            &self.user_agent,
            self.env.chat_ws_config,
            headers,
            kind,
        )
        .await
        .map_err(|error| {
            tracing::warn!(kind, %error, "chat socket failed to connect");
            connect_error(error)
        })?;
        tracing::info!(kind, "chat socket connected");
        let overrides = self
            .grpc_overrides
            .lock()
            .map(|overrides| overrides.clone())
            .unwrap_or_default();
        Ok(ChatConnection::finish_connect(
            self.runtime.clone(),
            pending,
            overrides,
            listener,
        ))
    }
}

impl SignalTransport for LibsignalTransport {
    type Socket = LibsignalSocket;

    async fn connect_auth(
        &self,
        credentials: &AuthCredentials,
    ) -> Result<Connected<LibsignalSocket, ChatEvent>, TransportError> {
        let headers = ChatHeaders::Auth(AuthenticatedChatHeaders {
            aci: credentials.aci,
            device_id: credentials.device_id,
            password: credentials.password.as_str().to_owned(),
            receive_stories: ReceiveStories::from(false),
            languages: LanguageList::default(),
        });
        let (tx, events) = mpsc::unbounded_channel();
        let stopped = Stopped::default();
        let inner = self
            .establish(
                "auth",
                CHAT_WEBSOCKET_PATH,
                Some(headers),
                chat_listener(tx, stopped.clone()),
            )
            .await?;
        Ok(Connected {
            socket: LibsignalSocket { inner, stopped },
            events,
        })
    }

    async fn connect_unauth(
        &self,
    ) -> Result<Connected<LibsignalSocket, ChatEvent>, TransportError> {
        let headers = ChatHeaders::Unauth(UnauthenticatedChatHeaders {
            languages: LanguageList::default(),
        });
        let (tx, events) = mpsc::unbounded_channel();
        let stopped = Stopped::default();
        let inner = self
            .establish(
                "unauth",
                CHAT_WEBSOCKET_PATH,
                Some(headers),
                chat_listener(tx, stopped.clone()),
            )
            .await?;
        Ok(Connected {
            socket: LibsignalSocket { inner, stopped },
            events,
        })
    }

    async fn connect_provisioning(
        &self,
    ) -> Result<Connected<LibsignalSocket, ProvisioningEvent>, TransportError> {
        let (tx, events) = mpsc::unbounded_channel();
        let stopped = Stopped::default();
        // No headers at all on the provisioning socket (bridge `net/chat.rs:322-339`).
        let inner = self
            .establish(
                "provisioning",
                CHAT_PROVISIONING_PATH,
                None,
                provisioning_listener(tx, stopped.clone()),
            )
            .await?;
        Ok(Connected {
            socket: LibsignalSocket { inner, stopped },
            events,
        })
    }

    async fn https(&self, request: HttpsRequest) -> Result<HttpsResponse, TransportError> {
        self.https.send(request).await
    }

    async fn https_download(
        &self,
        request: HttpsRequest,
        sink: DownloadSink,
    ) -> Result<StatusCode, TransportError> {
        self.https.download(request, sink).await
    }

    async fn https_upload(&self, request: UploadRequest) -> Result<HttpsResponse, TransportError> {
        self.https.upload(request).await
    }

    fn network_changed(&self) {
        let now = Instant::now();
        {
            let Ok(mut last) = self.last_network_change.lock() else {
                return;
            };
            if last
                .is_some_and(|last| now.saturating_duration_since(last) < NETWORK_CHANGE_DEBOUNCE)
            {
                return;
            }
            *last = Some(now);
        }
        tracing::info!("network changed");
        self.network_change_tx.send_replace(());
        // The resolver's reset takes a blocking lock, which panics on a runtime thread (the
        // bridge calls it from the app's own thread); run it on the blocking pool instead.
        let dns_resolver = self.dns_resolver.clone();
        self.runtime
            .spawn_blocking(move || dns_resolver.on_network_change(now.into()));
        if let Ok(mut connect_state) = self.connect_state.lock() {
            connect_state.network_changed(now.into());
        }
    }

    fn remote_config_changed(&self, config: &BTreeMap<String, String>) {
        let overrides = grpc_overrides(config);
        if let Ok(mut current) = self.grpc_overrides.lock()
            && *current != overrides
        {
            let over_websocket = overrides
                .values()
                .filter(|transport| **transport == GrpcOverride::UseWs)
                .count();
            tracing::info!(
                named = overrides.len(),
                over_websocket,
                "typed request transports from the remote config"
            );
            *current = overrides;
        }
    }
}

/// How long [`ChatSocket::disconnect`] waits for the socket's last event to reach its
/// listener.
const STOP_WAIT: Duration = Duration::from_secs(2);

/// Set once a socket's listener was handed its last event.
///
/// libsignal-net runs the listener on the blocking pool; when the runtime shuts down while an
/// event is on its way there, libsignal logs "listener panicked on event" (the cancelled task
/// looks like a panic to it). A disconnect therefore returns only once the listener has seen
/// the end, so a clean shutdown leaves no event in flight (open since phase F).
#[derive(Clone)]
struct Stopped(std::sync::Arc<watch::Sender<bool>>);

impl Default for Stopped {
    fn default() -> Self {
        Self(std::sync::Arc::new(watch::Sender::new(false)))
    }
}

impl Stopped {
    fn set(&self) {
        self.0.send_replace(true);
    }

    async fn wait(&self) {
        let mut seen = self.0.subscribe();
        let _ = tokio::time::timeout(STOP_WAIT, seen.wait_for(|stopped| *stopped)).await;
    }
}

/// A libsignal-net chat socket.
pub(crate) struct LibsignalSocket {
    inner: ChatConnection,
    stopped: Stopped,
}

impl ChatSocket for LibsignalSocket {
    async fn request(&self, request: ChatRequest) -> Result<ChatResponse, TransportError> {
        let ChatRequest {
            method,
            path,
            headers,
            body,
            timeout,
        } = request;
        let response = self
            .inner
            .send(
                chat::Request {
                    method,
                    path,
                    headers,
                    body,
                },
                timeout,
            )
            .await
            .map_err(send_error)?;
        Ok(ChatResponse {
            status: response.status,
            message: response.message,
            headers: response.headers,
            body: response.body.unwrap_or_default(),
        })
    }

    async fn send_multi_recipient(
        &self,
        send: MultiRecipientSend,
    ) -> Result<MultiRecipientOutcome, TransportError> {
        use libsignal_net_chat::api::messages::{
            MultiRecipientSendAuthorization, MultiRecipientSendFailure, UnauthenticatedChatApi as _,
        };
        use libsignal_net_chat::api::{DisconnectedError, RequestError, Unauth};
        let token: zkgroup::groups::GroupSendFullToken =
            zkgroup::deserialize(&send.group_send_token)
                .map_err(|_| TransportError::InvalidRequest("group send token"))?;
        let unauth: &Unauth<ChatConnection> = (&self.inner).into();
        let result = unauth
            .send_multi_recipient_message(
                send.payload,
                libsignal_protocol::Timestamp::from_epoch_millis(send.timestamp),
                MultiRecipientSendAuthorization::Group(token),
                send.online,
                send.urgent,
            )
            .await;
        match result {
            Ok(response) => Ok(MultiRecipientOutcome::Sent {
                unregistered: response.unregistered_ids,
            }),
            Err(RequestError::Other(MultiRecipientSendFailure::Unauthorized)) => {
                Ok(MultiRecipientOutcome::Unauthorized)
            }
            Err(RequestError::Other(MultiRecipientSendFailure::MismatchedDevices(entries))) => {
                Ok(MultiRecipientOutcome::MismatchedDevices(
                    entries
                        .into_iter()
                        .map(|entry| DeviceMismatch {
                            account: entry.account,
                            missing: entry.missing_devices,
                            extra: entry.extra_devices,
                            stale: entry.stale_devices,
                        })
                        .collect(),
                ))
            }
            Err(RequestError::Timeout) => Err(TransportError::Timeout),
            Err(RequestError::Disconnected(DisconnectedError::ConnectedElsewhere)) => {
                Err(TransportError::ConnectedElsewhere)
            }
            Err(RequestError::Disconnected(DisconnectedError::ConnectionInvalidated)) => {
                Err(TransportError::ConnectionInvalidated)
            }
            Err(RequestError::Disconnected(_)) => Err(TransportError::Disconnected),
            Err(RequestError::RetryLater(retry)) => Err(TransportError::RetryLater {
                retry_after_secs: retry.retry_after_seconds,
            }),
            Err(RequestError::Challenge(challenge)) => Err(challenge_error(challenge)),
            Err(RequestError::ServerSideError) => Err(TransportError::ServerSide),
            Err(RequestError::Unexpected { log_safe }) => {
                Err(TransportError::InvalidResponse(log_safe))
            }
        }
    }

    async fn get_upload_form(
        &self,
        upload_length: u64,
    ) -> Result<Result<UploadForm, UploadTooLarge>, TransportError> {
        use libsignal_net_chat::api::messages::AuthenticatedChatApi as _;
        use libsignal_net_chat::api::{Auth, DisconnectedError, RequestError};
        let auth: &Auth<ChatConnection> = (&self.inner).into();
        match auth.get_upload_form(upload_length).await {
            Ok(form) => Ok(Ok(UploadForm {
                cdn: form.cdn,
                key: form.key,
                headers: form.headers,
                signed_upload_url: form.signed_upload_url,
            })),
            Err(RequestError::Other(_)) => Ok(Err(UploadTooLarge)),
            Err(RequestError::Timeout) => Err(TransportError::Timeout),
            Err(RequestError::Disconnected(DisconnectedError::ConnectedElsewhere)) => {
                Err(TransportError::ConnectedElsewhere)
            }
            Err(RequestError::Disconnected(DisconnectedError::ConnectionInvalidated)) => {
                Err(TransportError::ConnectionInvalidated)
            }
            Err(RequestError::Disconnected(_)) => Err(TransportError::Disconnected),
            Err(RequestError::RetryLater(retry)) => Err(TransportError::RetryLater {
                retry_after_secs: retry.retry_after_seconds,
            }),
            Err(RequestError::Challenge(challenge)) => Err(challenge_error(challenge)),
            Err(RequestError::ServerSideError) => Err(TransportError::ServerSide),
            Err(RequestError::Unexpected { log_safe }) => {
                Err(TransportError::InvalidResponse(log_safe))
            }
        }
    }

    async fn disconnect(&self) {
        self.inner.disconnect().await;
        self.stopped.wait().await;
    }
}

/// A rate-limit challenge a typed request met (`RateLimitChallenge`: the 428 of the send
/// routes): its token, whether a captcha lifts it, and the server's `Retry-After`. The send
/// path turns it into `ErrorKind::Challenge` (`crate::error`).
///
/// libsignal-net v0.104.0 reports one only for authenticated requests
/// (`AuthenticatedChatApi::ALLOW_RATE_LIMIT_CHALLENGES`). On an unauthenticated one (the
/// multi-recipient send) it drops the 428's body and reports an unexpected status, which
/// `send::group` answers with 1:1 sends; their 428 comes through the REST path with its token
/// (`send::status_error`). Should a later libsignal hand the challenge over here, it is one.
fn challenge_error(challenge: libsignal_net_chat::api::RateLimitChallenge) -> TransportError {
    use libsignal_net_chat::api::ChallengeOption;
    let captcha = !challenge.token.is_empty()
        && challenge
            .options
            .iter()
            .any(|option| matches!(option, ChallengeOption::Captcha));
    TransportError::Challenge {
        token: kourier_model::Redacted::new(challenge.token),
        captcha,
        retry_after_secs: challenge.retry_later.map(|retry| retry.retry_after_seconds),
    }
}

/// The listener of an authenticated or unauthenticated socket: converts each event and forwards
/// it into the unbounded channel. It runs synchronously inside libsignal-net, so it must never
/// block or drop an envelope (§6.4).
fn chat_listener(tx: mpsc::UnboundedSender<ChatEvent>, stopped: Stopped) -> ws::EventListener {
    Box::new(move |event| {
        if matches!(event, ws::ListenerEvent::Finished(_)) {
            stopped.set();
        }
        let event = match ServerEvent::try_from(event) {
            Ok(event) => event,
            Err(error) => {
                // Unknown verbs and paths are dropped unanswered, as libsignal's bridge does.
                tracing::warn!(%error, "ignoring unexpected request on chat socket");
                return;
            }
        };
        // A closed receiver means the account actor is gone; whatever it carried stays unacked
        // and is redelivered on the next connection.
        let _ = tx.send(chat_event(event));
    })
}

fn chat_event(event: ServerEvent) -> ChatEvent {
    match event {
        ServerEvent::QueueEmpty => ChatEvent::QueueEmpty,
        ServerEvent::IncomingMessage {
            request_id: _,
            envelope,
            server_delivery_timestamp,
            send_ack,
        } => ChatEvent::Envelope(IncomingEnvelope {
            bytes: envelope,
            server_delivery_timestamp: server_delivery_timestamp.epoch_millis(),
            ack: server_ack(send_ack),
        }),
        ServerEvent::ServerTimestamp(timestamp) => {
            ChatEvent::ServerTimestamp(timestamp.epoch_millis())
        }
        ServerEvent::Alerts(alerts) => ChatEvent::Alerts(alerts),
        ServerEvent::Stopped(cause) => ChatEvent::Stopped(stop_reason(cause)),
    }
}

/// The listener of a provisioning socket. Alerts are not expected there and are dropped.
fn provisioning_listener(
    tx: mpsc::UnboundedSender<ProvisioningEvent>,
    stopped: Stopped,
) -> ws::EventListener {
    Box::new(move |event| {
        if matches!(event, ws::ListenerEvent::Finished(_)) {
            stopped.set();
        }
        if let ws::ListenerEvent::ReceivedAlerts(alerts) = &event {
            if !alerts.is_empty() {
                tracing::warn!(
                    count = alerts.len(),
                    "unexpected alerts on provisioning socket"
                );
            }
            return;
        }
        let event = match LsProvisioningEvent::try_from(event) {
            Ok(event) => event,
            Err(error) => {
                tracing::warn!(%error, "ignoring unexpected request on provisioning socket");
                return;
            }
        };
        let event = match event {
            LsProvisioningEvent::ReceivedAddress { address, send_ack } => {
                ProvisioningEvent::Address {
                    address,
                    ack: server_ack(send_ack),
                }
            }
            LsProvisioningEvent::ReceivedEnvelope { envelope, send_ack } => {
                ProvisioningEvent::Envelope {
                    bytes: envelope,
                    ack: server_ack(send_ack),
                }
            }
            LsProvisioningEvent::ServerTimestamp(timestamp) => {
                ProvisioningEvent::ServerTimestamp(timestamp.epoch_millis())
            }
            LsProvisioningEvent::Stopped(cause) => ProvisioningEvent::Stopped(stop_reason(cause)),
        };
        let _ = tx.send(event);
    })
}

fn server_ack(send_ack: ResponseEnvelopeSender) -> ServerAck {
    ServerAck::new(move |status| send_ack(status).map_err(send_error))
}

fn stop_reason(cause: DisconnectCause) -> StopReason {
    match cause {
        DisconnectCause::LocalDisconnect => StopReason::Local,
        DisconnectCause::Error(SendError::ConnectedElsewhere) => StopReason::ConnectedElsewhere,
        DisconnectCause::Error(SendError::ConnectionInvalidated) => {
            StopReason::ConnectionInvalidated
        }
        DisconnectCause::Error(error) => StopReason::Closed(error.to_string()),
    }
}

/// libsignal's `SendError` is `LogSafeDisplay`, so its text may go into the error.
fn send_error(error: SendError) -> TransportError {
    match error {
        SendError::RequestTimedOut => TransportError::Timeout,
        SendError::Disconnected => TransportError::Disconnected,
        SendError::ConnectedElsewhere => TransportError::ConnectedElsewhere,
        SendError::ConnectionInvalidated => TransportError::ConnectionInvalidated,
        SendError::RequestHasInvalidHeader => {
            TransportError::InvalidRequest("header names and values must be ASCII")
        }
        SendError::IncomingDataInvalid => {
            TransportError::InvalidResponse("undecodable response".to_owned())
        }
        SendError::WebSocket(error) => TransportError::Unreachable(error.to_string()),
    }
}

/// Maps connect failures; a refused upgrade keeps its status so 401 can trigger the unlink
/// procedure (§4.3, §6.4).
fn connect_error(error: ConnectError) -> TransportError {
    match error {
        ConnectError::Timeout => TransportError::Timeout,
        ConnectError::AppExpired => TransportError::AppExpired,
        ConnectError::DeviceDeregistered => TransportError::DeviceDeregistered,
        ConnectError::RetryLater(retry) => TransportError::RetryLater {
            retry_after_secs: retry.retry_after_seconds,
        },
        ConnectError::InvalidConnectionConfiguration => {
            TransportError::Config("invalid connection configuration".to_owned())
        }
        ConnectError::WebSocket(WebSocketConnectError::WebSocketError(WebSocketError::Http(
            response,
        ))) => TransportError::Rejected {
            status: response.status().as_u16(),
        },
        error @ (ConnectError::AllAttemptsFailed | ConnectError::WebSocket(_)) => {
            TransportError::Unreachable(error.to_string())
        }
    }
}

/// An authenticated-socket stand-in connected to libsignal-net's fake remote end (tests): the
/// socket, the remote, and the events the real listener produces.
#[cfg(test)]
pub(crate) fn fake_chat(
    runtime: tokio::runtime::Handle,
) -> (
    LibsignalSocket,
    libsignal_net::chat::fake::FakeChatRemote,
    mpsc::UnboundedReceiver<ChatEvent>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let stopped = Stopped::default();
    let (inner, remote) =
        ChatConnection::new_fake(runtime, chat_listener(tx, stopped.clone()), [], []);
    (LibsignalSocket { inner, stopped }, remote, rx)
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use http::{Method, StatusCode};
    use libsignal_net::chat::fake::{FakeChatRemote, ReceiveRequestError};
    use libsignal_net::chat::{RequestProto, ResponseProto};
    use libsignal_net::infra::AsStaticHttpHeader as _;

    use super::*;
    use crate::net::SignalEnvironment;
    use crate::net::env::tests::dummy_local_env;

    fn fake_chat() -> (
        LibsignalSocket,
        FakeChatRemote,
        mpsc::UnboundedReceiver<ChatEvent>,
    ) {
        super::fake_chat(tokio::runtime::Handle::current())
    }

    /// The next event after the connect-time server timestamp and (empty) alerts.
    async fn next_event(events: &mut mpsc::UnboundedReceiver<ChatEvent>) -> Option<ChatEvent> {
        loop {
            match events.recv().await {
                Some(ChatEvent::ServerTimestamp(_) | ChatEvent::Alerts(_)) => {}
                other => return other,
            }
        }
    }

    fn push(id: u64, verb: &str, path: &str, body: &[u8], timestamp: u64) -> RequestProto {
        RequestProto {
            verb: Some(verb.to_owned()),
            path: Some(path.to_owned()),
            body: Some(Bytes::copy_from_slice(body)),
            headers: vec![format!("X-Signal-Timestamp: {timestamp}")],
            id: Some(id),
        }
    }

    /// Whether the fake server receives a response within `wait`.
    async fn server_got_response(remote: &FakeChatRemote, wait: Duration) -> bool {
        match tokio::time::timeout(wait, remote.receive_request()).await {
            Err(_elapsed) => false,
            Ok(Err(ReceiveRequestError::GotResponse)) => true,
            Ok(other) => panic!("unexpected message from client: {other:?}"),
        }
    }

    #[tokio::test]
    async fn envelope_is_acked_only_when_the_ack_is_sent() {
        let (_socket, remote, mut events) = fake_chat();
        remote
            .send_request(push(
                7,
                "PUT",
                "/api/v1/message",
                b"envelope",
                1_700_000_000_123,
            ))
            .unwrap();

        let Some(ChatEvent::Envelope(envelope)) = next_event(&mut events).await else {
            panic!("expected an envelope");
        };
        assert_eq!(envelope.bytes.as_ref(), b"envelope");
        assert_eq!(envelope.server_delivery_timestamp, 1_700_000_000_123);
        // Durable commit pending: the server hears nothing.
        assert!(!server_got_response(&remote, Duration::from_millis(200)).await);

        envelope.ack.ack().unwrap();
        assert!(server_got_response(&remote, Duration::from_secs(2)).await);
    }

    #[tokio::test]
    async fn dropping_the_ack_leaves_the_envelope_unacked() {
        let (_socket, remote, mut events) = fake_chat();
        remote
            .send_request(push(1, "PUT", "/api/v1/message", b"x", 1))
            .unwrap();
        let Some(ChatEvent::Envelope(envelope)) = next_event(&mut events).await else {
            panic!("expected an envelope");
        };
        drop(envelope);
        assert!(!server_got_response(&remote, Duration::from_millis(200)).await);
    }

    #[tokio::test]
    async fn a_thousand_queued_envelopes_are_neither_dropped_nor_stalled() {
        let (_socket, remote, mut events) = fake_chat();
        for id in 0..1000u64 {
            remote
                .send_request(push(id, "PUT", "/api/v1/message", &id.to_be_bytes(), id))
                .unwrap();
        }
        remote
            .send_request(push(1000, "PUT", "/api/v1/queue/empty", b"", 0))
            .unwrap();

        // A slow stage 1: nothing is consumed until everything has been pushed.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut acks = Vec::new();
        for id in 0..1000u64 {
            match next_event(&mut events).await {
                Some(ChatEvent::Envelope(envelope)) => {
                    assert_eq!(envelope.bytes.as_ref(), id.to_be_bytes());
                    acks.push(envelope.ack);
                }
                other => panic!("envelope {id}: got {other:?}"),
            }
        }
        assert!(matches!(
            next_event(&mut events).await,
            Some(ChatEvent::QueueEmpty)
        ));
        for ack in acks {
            ack.ack().unwrap();
        }
        for _ in 0..1000 {
            assert!(server_got_response(&remote, Duration::from_secs(2)).await);
        }
    }

    #[tokio::test]
    async fn unexpected_requests_are_ignored_and_close_codes_are_mapped() {
        let (_socket, remote, mut events) = fake_chat();
        remote
            .send_request(push(1, "GET", "/api/v1/message", b"", 0))
            .unwrap();
        remote
            .send_request(push(2, "PUT", "/api/v1/unknown", b"", 0))
            .unwrap();
        remote.send_close(Some(4409)).unwrap();
        match next_event(&mut events).await {
            Some(ChatEvent::Stopped(StopReason::ConnectedElsewhere)) => {}
            other => panic!("expected Stopped(ConnectedElsewhere), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn provisioning_socket_reports_address_and_envelope_with_explicit_acks() {
        let (tx, mut events) = mpsc::unbounded_channel();
        let stopped = Stopped::default();
        let (inner, remote) = ChatConnection::new_fake(
            tokio::runtime::Handle::current(),
            provisioning_listener(tx, stopped.clone()),
            [],
            [],
        );
        let socket = LibsignalSocket { inner, stopped };
        // The fake's connect response carries a server clock (500 ms) and no alerts; the empty
        // alerts event is dropped by the listener.
        assert!(matches!(
            events.recv().await,
            Some(ProvisioningEvent::ServerTimestamp(500))
        ));

        // `ProvisioningAddress { address = 1 }`, encoded by hand.
        let address = "dGVzdC1hZGRyZXNz";
        let mut body = vec![0x0a, u8::try_from(address.len()).unwrap()];
        body.extend_from_slice(address.as_bytes());
        remote
            .send_request(push(1, "PUT", "/v1/address", &body, 0))
            .unwrap();
        let Some(ProvisioningEvent::Address { address: got, ack }) = events.recv().await else {
            panic!("expected the provisioning address");
        };
        assert_eq!(got, address);
        assert!(!server_got_response(&remote, Duration::from_millis(200)).await);
        ack.ack().unwrap();
        assert!(server_got_response(&remote, Duration::from_secs(2)).await);

        // A wrong verb, an unknown path and an undecodable address produce no event and no
        // response; the envelope after them does, and is answered only by its ack.
        for (id, verb, path, body) in [
            (2, "GET", "/v1/message", &b""[..]),
            (3, "PUT", "/v1/unknown", b""),
            (4, "PUT", "/v1/address", b"\xff"),
            (5, "PUT", "/v1/message", b"provision envelope"),
        ] {
            remote.send_request(push(id, verb, path, body, 0)).unwrap();
        }
        let Some(ProvisioningEvent::Envelope { bytes, ack }) = events.recv().await else {
            panic!("expected the provisioning envelope");
        };
        assert_eq!(bytes.as_ref(), b"provision envelope");
        assert!(!server_got_response(&remote, Duration::from_millis(200)).await);
        ack.ack().unwrap();
        assert!(server_got_response(&remote, Duration::from_secs(2)).await);

        socket.disconnect().await;
        assert!(matches!(
            events.recv().await,
            Some(ProvisioningEvent::Stopped(StopReason::Local))
        ));
        // Nothing follows `Stopped`: the stream ends.
        let end = tokio::time::timeout(Duration::from_secs(2), events.recv()).await;
        assert!(matches!(end, Ok(None)), "{end:?}");
    }

    #[tokio::test]
    async fn requests_round_trip_over_the_socket() {
        let (socket, remote, _events) = fake_chat();
        let request =
            ChatRequest::json(Method::PUT, "/v1/devices/link", b"{\"a\":1}".to_vec()).unwrap();
        let client = tokio::spawn(async move { socket.request(request).await });

        let sent = remote.receive_request().await.unwrap().unwrap();
        assert_eq!(sent.verb.as_deref(), Some("PUT"));
        assert_eq!(sent.path.as_deref(), Some("/v1/devices/link"));
        assert_eq!(sent.body.as_deref(), Some(&b"{\"a\":1}"[..]));
        assert!(
            sent.headers
                .iter()
                .any(|h| h.eq_ignore_ascii_case("content-type: application/json")),
            "{:?}",
            sent.headers
        );
        remote
            .send_response(ResponseProto {
                id: sent.id,
                status: Some(409),
                message: Some("Conflict".to_owned()),
                headers: vec!["Content-Type: application/json".to_owned()],
                body: Some(Bytes::from_static(b"{}")),
            })
            .unwrap();

        let response = client.await.unwrap().unwrap();
        assert_eq!(response.status, StatusCode::CONFLICT);
        assert_eq!(response.message.as_deref(), Some("Conflict"));
        assert_eq!(response.body.as_ref(), b"{}");
        assert_eq!(
            response.headers[http::header::CONTENT_TYPE],
            "application/json"
        );
    }

    #[tokio::test]
    async fn request_times_out_without_a_response() {
        let (socket, remote, _events) = fake_chat();
        let request = ChatRequest::get("/v1/config")
            .unwrap()
            .with_timeout(Duration::from_millis(50));
        let result = socket.request(request).await;
        assert_eq!(result.unwrap_err(), TransportError::Timeout);
        drop(remote);
    }

    #[tokio::test]
    async fn transports_build_without_touching_the_network() {
        for environment in [
            SignalEnvironment::Production,
            SignalEnvironment::Staging,
            SignalEnvironment::LocalTest(dummy_local_env(4433)),
        ] {
            let env = SignalEnv::resolve(&environment).unwrap();
            let transport =
                LibsignalTransport::new(env, tokio::runtime::Handle::current()).unwrap();
            assert_eq!(
                transport.user_agent.header_value(),
                format!("Kourier/{} libsignal/0.104.0", env!("CARGO_PKG_VERSION"))
            );
            transport.network_changed();
            transport.network_changed(); // debounced
        }
    }

    #[tokio::test]
    async fn connecting_to_a_closed_local_port_fails_cleanly() {
        // A port nobody listens on: every socket kind goes through route resolution, the
        // connector and error mapping, and fails without leaving the machine.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let env = SignalEnv::resolve(&SignalEnvironment::LocalTest(dummy_local_env(port))).unwrap();
        let transport = LibsignalTransport::new(env, tokio::runtime::Handle::current()).unwrap();
        let wait = Duration::from_secs(60);
        let error = tokio::time::timeout(wait, transport.connect_unauth())
            .await
            .unwrap()
            .err()
            .unwrap();
        assert!(
            matches!(
                error,
                TransportError::Unreachable(_) | TransportError::Timeout
            ),
            "{error:?}"
        );
        let error = tokio::time::timeout(wait, transport.connect_provisioning())
            .await
            .unwrap()
            .err()
            .unwrap();
        assert!(
            matches!(
                error,
                TransportError::Unreachable(_) | TransportError::Timeout
            ),
            "{error:?}"
        );
    }

    /// §6.4: the remote config's gRPC switches reach libsignal-net as Desktop passes them.
    #[test]
    fn grpc_switches_follow_the_remote_config() {
        let config = |entries: &[(&str, &str)]| -> BTreeMap<String, String> {
            entries
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect()
        };
        assert!(
            grpc_overrides(&BTreeMap::new()).is_empty(),
            "no entry: libsignal decides"
        );
        // The mock's config (Desktop's test bootstrap): sends on the websocket, the upload
        // form and multi-recipient sends on gRPC.
        let overrides = grpc_overrides(&config(&[
            ("desktop.libsignalNet.grpc.MessagesSendMessage", "ws"),
            (
                "desktop.libsignalNet.grpc.MessagesAnonymousSendMultiRecipientMessage.2",
                "true",
            ),
            ("desktop.libsignalNet.grpc.AttachmentsGetUploadForm", "true"),
            (
                "desktop.libsignalNet.grpc.KeyTransparencyQueryServiceSearchV2",
                "false",
            ),
            // Not libsignal-net's: other prefixes, other keys, an old key version.
            ("desktop.grpc.MessagesSendMessage", "ws"),
            ("desktop.libsignalNet.grpc.SomethingNew", "ws"),
            (
                "desktop.libsignalNet.grpc.MessagesAnonymousSendMultiRecipientMessage",
                "ws",
            ),
            ("global.attachments.maxBytes", "104857600"),
        ]));
        assert_eq!(overrides.len(), 3, "{overrides:?}");
        assert_eq!(overrides["MessagesSendMessage"], GrpcOverride::UseWs);
        assert_eq!(
            overrides["MessagesAnonymousSendMultiRecipientMessage"],
            GrpcOverride::UseGrpc,
            "the request name has no version suffix"
        );
        assert_eq!(overrides["AttachmentsGetUploadForm"], GrpcOverride::UseGrpc);

        // The request names are the ones libsignal-net-chat looks its override up under.
        for (key, request) in GRPC_SWITCHES {
            let stem = key.strip_prefix("grpc.").expect("a gRPC key");
            assert!(
                stem == request
                    || stem.strip_prefix(request).is_some_and(|rest| {
                        rest.strip_prefix('.')
                            .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
                    }),
                "{key} -> {request}"
            );
        }
    }

    #[tokio::test]
    async fn a_transport_takes_its_request_transports_from_the_remote_config() {
        let env = SignalEnv::resolve(&SignalEnvironment::LocalTest(dummy_local_env(4433))).unwrap();
        let transport = LibsignalTransport::new(env, tokio::runtime::Handle::current()).unwrap();
        assert!(transport.grpc_overrides.lock().unwrap().is_empty());
        let config = BTreeMap::from([(
            "desktop.libsignalNet.grpc.AttachmentsGetUploadForm".to_owned(),
            "ws".to_owned(),
        )]);
        transport.remote_config_changed(&config);
        assert_eq!(
            transport.grpc_overrides.lock().unwrap()["AttachmentsGetUploadForm"],
            GrpcOverride::UseWs
        );
        transport.remote_config_changed(&BTreeMap::new());
        assert!(transport.grpc_overrides.lock().unwrap().is_empty());
    }

    #[test]
    fn connect_errors_keep_the_upgrade_status() {
        let response = http::Response::builder().status(401).body(None).unwrap();
        let error = ConnectError::WebSocket(WebSocketConnectError::WebSocketError(
            WebSocketError::Http(Box::new(response)),
        ));
        assert_eq!(
            connect_error(error),
            TransportError::Rejected { status: 401 }
        );
        assert_eq!(
            connect_error(ConnectError::DeviceDeregistered),
            TransportError::DeviceDeregistered
        );
        assert_eq!(
            connect_error(ConnectError::AppExpired),
            TransportError::AppExpired
        );
        assert!(matches!(
            connect_error(ConnectError::AllAttemptsFailed),
            TransportError::Unreachable(_)
        ));
    }
}
