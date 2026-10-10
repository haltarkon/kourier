//! [`Net`]: the account actor's handle on the transport. Transport futures are `Send` and run on
//! the shared network runtime (`AccountContext::net_rt`); the actor awaits their join handles
//! from its `LocalSet` (§6.3). Also JSON helpers for REST over the chat socket.

use std::future::Future;
use std::sync::Arc;

use bytes::Bytes;
use http::{HeaderValue, Method};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{
    AuthCredentials, ChatEvent, ChatRequest, ChatResponse, ChatSocket, Connected, DownloadSink,
    HttpsRequest, HttpsResponse, MultiRecipientOutcome, MultiRecipientSend, ProvisioningEvent,
    SignalTransport, TransportError, UploadForm, UploadRequest, UploadTooLarge,
};

/// The `tracing` target of the request lines: one DEBUG event per request the backend makes
/// (`via` the chat socket, the Storage Service, a CDN or an upload URL; `method`; `route`,
/// the path with every segment that could carry an id, a number or a token replaced and
/// without its query; `status` or the transport's `error`; `ms`), and one per connect.
/// Nothing in them is forbidden by §9.1, so a run that must be diagnosed afterwards (the
/// first ones against a real server, `cargo xtask smoke`) turns the target on; it is off by
/// default, and then no route is formatted.
pub(crate) const REQUESTS_TARGET: &str = "kourier_signal::requests";

/// One request on its way, for its log line ([`REQUESTS_TARGET`]).
struct Traced {
    started: std::time::Instant,
    via: &'static str,
    method: Method,
    route: String,
}

impl Traced {
    /// `None` while the target is off: nothing is formatted then.
    fn start(via: &'static str, method: &Method, route: impl FnOnce() -> String) -> Option<Self> {
        tracing::enabled!(target: REQUESTS_TARGET, tracing::Level::DEBUG).then(|| Self {
            started: std::time::Instant::now(),
            via,
            method: method.clone(),
            route: route(),
        })
    }

    /// Logs how the request ended: the status of an answer, or why there was none.
    fn end<R>(
        traced: Option<Self>,
        result: &Result<R, TransportError>,
        status: impl FnOnce(&R) -> Option<u16>,
    ) {
        let Some(t) = traced else { return };
        let ms = u64::try_from(t.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match result {
            Ok(answer) => tracing::debug!(
                target: REQUESTS_TARGET,
                via = t.via,
                method = %t.method,
                route = %t.route,
                status = status(answer),
                ms,
                "request"
            ),
            Err(error) => tracing::debug!(
                target: REQUESTS_TARGET,
                via = t.via,
                method = %t.method,
                route = %t.route,
                %error,
                ms,
                "request failed"
            ),
        }
    }
}

/// The log name of an HTTPS service.
fn via(host: super::HttpsHost) -> &'static str {
    match host {
        super::HttpsHost::Storage => "storage",
        super::HttpsHost::Cdn(0) => "cdn0",
        super::HttpsHost::Cdn(2) => "cdn2",
        super::HttpsHost::Cdn(3) => "cdn3",
        super::HttpsHost::Cdn(_) => "cdn",
    }
}

/// The transport plus the runtime its futures run on. Cheap to clone.
pub(crate) struct Net<T: SignalTransport> {
    transport: Arc<T>,
    rt: tokio::runtime::Handle,
}

/// A task of the network runtime that ends with whoever waits for it.
struct AbortOnDrop<R>(tokio::task::JoinHandle<R>);

impl<R> Drop for AbortOnDrop<R> {
    fn drop(&mut self) {
        // Nothing happens to a task that finished.
        self.0.abort();
    }
}

impl<T: SignalTransport> Clone for Net<T> {
    fn clone(&self) -> Self {
        Self {
            transport: self.transport.clone(),
            rt: self.rt.clone(),
        }
    }
}

/// How many stall times a watched upload may stay quiet once every byte of its body is handed
/// to the request's queue (`Net::https_upload_watched`). From then on nothing counts the
/// bytes that are still on their way: up to the whole queue (`upload::QUEUE` chunks of 64 KiB
/// and the one in flight, 576 KiB) and what the connection buffers; an upload smaller than
/// that is in this state from its first moment. Five stall times (five minutes) are what the
/// queue's bytes take at 2 KiB/s, so a slow link that is alive is not cut off, and a dead
/// one is still noticed long before the request's own timeout of ten minutes and more.
pub(crate) const UPLOAD_TAIL_STALLS: u32 = 5;

impl<T: SignalTransport> Net<T> {
    /// Wraps a transport whose futures run on `rt`.
    pub(crate) fn new(transport: Arc<T>, rt: tokio::runtime::Handle) -> Self {
        Self { transport, rt }
    }

    /// The transport.
    pub(crate) fn transport(&self) -> &T {
        &self.transport
    }

    /// Runs `future` on the network runtime and waits for it. A panic or a runtime shutdown
    /// surfaces as [`TransportError::Disconnected`].
    pub(crate) async fn run<F, R>(&self, future: F) -> Result<R, TransportError>
    where
        F: Future<Output = Result<R, TransportError>> + Send + 'static,
        R: Send + 'static,
    {
        match self.rt.spawn(future).await {
            Ok(result) => result,
            Err(error) => {
                tracing::error!(panic = error.is_panic(), "network task did not finish");
                Err(TransportError::Disconnected)
            }
        }
    }

    /// Opens the authenticated chat socket.
    pub(crate) async fn connect_auth(
        &self,
        credentials: AuthCredentials,
    ) -> Result<Connected<T::Socket, ChatEvent>, TransportError> {
        let traced = Traced::start("chat", &Method::CONNECT, || "authenticated socket".into());
        let transport = self.transport.clone();
        let result = self
            .run(async move { transport.connect_auth(&credentials).await })
            .await;
        Traced::end(traced, &result, |_| None);
        result
    }

    /// Opens an unauthenticated chat socket.
    pub(crate) async fn connect_unauth(
        &self,
    ) -> Result<Connected<T::Socket, ChatEvent>, TransportError> {
        let traced = Traced::start("chat", &Method::CONNECT, || "unauthenticated socket".into());
        let transport = self.transport.clone();
        let result = self
            .run(async move { transport.connect_unauth().await })
            .await;
        Traced::end(traced, &result, |_| None);
        result
    }

    /// Opens a provisioning socket.
    pub(crate) async fn connect_provisioning(
        &self,
    ) -> Result<Connected<T::Socket, ProvisioningEvent>, TransportError> {
        let traced = Traced::start("chat", &Method::CONNECT, || "provisioning socket".into());
        let transport = self.transport.clone();
        let result = self
            .run(async move { transport.connect_provisioning().await })
            .await;
        Traced::end(traced, &result, |_| None);
        result
    }

    /// Sends one REST request over `socket`.
    pub(crate) async fn request(
        &self,
        socket: &Arc<T::Socket>,
        request: ChatRequest,
    ) -> Result<ChatResponse, TransportError> {
        let traced = Traced::start("chat", &request.method, || request.log_safe_path());
        let socket = socket.clone();
        let result = self.run(async move { socket.request(request).await }).await;
        Traced::end(traced, &result, |r| Some(r.status.as_u16()));
        result
    }

    /// Sends one multi-recipient sealed message over (unauthenticated) `socket`.
    pub(crate) async fn send_multi_recipient(
        &self,
        socket: &Arc<T::Socket>,
        send: MultiRecipientSend,
    ) -> Result<MultiRecipientOutcome, TransportError> {
        // A typed call of libsignal: its transport (gRPC or the websocket route) is the
        // remote config's choice, logged when the config is applied.
        let traced = Traced::start("chat", &Method::PUT, || {
            "/v1/messages/multi_recipient (typed)".into()
        });
        let socket = socket.clone();
        let result = self
            .run(async move { socket.send_multi_recipient(send).await })
            .await;
        Traced::end(traced, &result, |outcome| match outcome {
            MultiRecipientOutcome::Sent { .. } => Some(200),
            MultiRecipientOutcome::Unauthorized => Some(401),
            MultiRecipientOutcome::MismatchedDevices(_) => Some(409),
        });
        result
    }

    /// Sends one HTTPS request (Storage Service or CDN).
    pub(crate) async fn https(
        &self,
        request: HttpsRequest,
    ) -> Result<HttpsResponse, TransportError> {
        let traced = Traced::start(via(request.host), &request.method, || {
            request.log_safe_path()
        });
        let transport = self.transport.clone();
        let result = self
            .run(async move { transport.https(request).await })
            .await;
        Traced::end(traced, &result, |r| Some(r.status.as_u16()));
        result
    }

    /// Streams one HTTPS download into `sink` (see [`SignalTransport::https_download`]).
    pub(crate) async fn https_download(
        &self,
        request: HttpsRequest,
        sink: DownloadSink,
    ) -> Result<http::StatusCode, TransportError> {
        let traced = Traced::start(via(request.host), &request.method, || {
            request.log_safe_path()
        });
        let transport = self.transport.clone();
        // Unlike a request, a download is stopped when its caller goes away: the body is
        // streamed into the caller's sink, and a transfer nobody waits for must not go on
        // writing (a history transfer that was skipped or stopped, `transfer.rs`).
        let mut task = AbortOnDrop(
            self.rt
                .spawn(async move { transport.https_download(request, sink).await }),
        );
        let result = match (&mut task.0).await {
            Ok(result) => result,
            Err(error) => {
                tracing::error!(panic = error.is_panic(), "network task did not finish");
                Err(TransportError::Disconnected)
            }
        };
        Traced::end(traced, &result, |status| Some(status.as_u16()));
        result
    }

    /// Asks for an attachment upload form over (authenticated) `socket`.
    pub(crate) async fn get_upload_form(
        &self,
        socket: &Arc<T::Socket>,
        upload_length: u64,
    ) -> Result<Result<UploadForm, UploadTooLarge>, TransportError> {
        let traced = Traced::start("chat", &Method::GET, || {
            "/v4/attachments/form/upload (typed)".into()
        });
        let socket = socket.clone();
        let result = self
            .run(async move { socket.get_upload_form(upload_length).await })
            .await;
        Traced::end(traced, &result, |form| match form {
            Ok(_) => Some(200),
            Err(_) => Some(413),
        });
        result
    }

    /// Sends one upload request that carries a blob, watching that it moves. `moved` counts
    /// the body bytes handed to the request's queue (by the task that produces the body);
    /// `goal` is the value it has when every byte of this body is handed over. When no byte
    /// moves for `stall` while some are left, the request is dropped, which closes its
    /// connection, and the upload fails as a timeout that the caller resumes. Once every
    /// byte is handed over, what is still on its way (the queue and the connection's
    /// buffers, then the server's answer) is counted by nothing, so that tail gets
    /// [`UPLOAD_TAIL_STALLS`] times as long. The watch runs on the network runtime with the
    /// request, because only dropping the request there stops it.
    pub(crate) async fn https_upload_watched(
        &self,
        request: UploadRequest,
        moved: Arc<std::sync::atomic::AtomicU64>,
        goal: u64,
        stall: std::time::Duration,
    ) -> Result<HttpsResponse, TransportError> {
        use std::sync::atomic::Ordering;
        // The URL is a credential the server handed out: never in a log.
        let traced = Traced::start("upload", &request.method, || "(signed upload URL)".into());
        let transport = self.transport.clone();
        let result = self
            .run(async move {
                let send = transport.https_upload(request);
                tokio::pin!(send);
                let step = (stall / 4).max(std::time::Duration::from_millis(10));
                let mut seen = moved.load(Ordering::Relaxed);
                let mut quiet = std::time::Duration::ZERO;
                loop {
                    tokio::select! {
                        answer = &mut send => return answer,
                        () = tokio::time::sleep(step) => {}
                    }
                    let now = moved.load(Ordering::Relaxed);
                    if now != seen {
                        seen = now;
                        quiet = std::time::Duration::ZERO;
                        continue;
                    }
                    quiet += step;
                    let allowed = if now >= goal {
                        stall.saturating_mul(UPLOAD_TAIL_STALLS)
                    } else {
                        stall
                    };
                    if quiet >= allowed {
                        return Err(TransportError::Timeout);
                    }
                }
            })
            .await;
        Traced::end(traced, &result, |r| Some(r.status.as_u16()));
        result
    }

    /// Sends one upload request to a CDN.
    pub(crate) async fn https_upload(
        &self,
        request: UploadRequest,
    ) -> Result<HttpsResponse, TransportError> {
        let traced = Traced::start("upload", &request.method, || "(signed upload URL)".into());
        let transport = self.transport.clone();
        let result = self
            .run(async move { transport.https_upload(request).await })
            .await;
        Traced::end(traced, &result, |r| Some(r.status.as_u16()));
        result
    }

    /// Closes `socket` (best effort).
    pub(crate) async fn disconnect(&self, socket: &Arc<T::Socket>) {
        let socket = socket.clone();
        let _ = self
            .run(async move {
                socket.disconnect().await;
                Ok(())
            })
            .await;
    }
}

/// `method path` with `body` serialized as JSON.
pub(crate) fn json_request<B: Serialize>(
    method: Method,
    path: &str,
    body: &B,
) -> Result<ChatRequest, TransportError> {
    let bytes = serde_json::to_vec(body)
        .map_err(|_| TransportError::InvalidRequest("request body does not serialize"))?;
    ChatRequest::json(method, path, Bytes::from(bytes))
}

/// Adds `Authorization: Basic base64(<username>:<password>)` (the link request carries the new
/// device's credentials on the unauthenticated socket, `SD/ts/textsecure/WebAPI.preload.ts`).
pub(crate) fn with_basic_auth(
    request: ChatRequest,
    username: &str,
    password: &str,
) -> Result<ChatRequest, TransportError> {
    use base64::Engine as _;
    let token = zeroize::Zeroizing::new(
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}")),
    );
    let mut value = HeaderValue::from_str(&format!("Basic {}", token.as_str()))
        .map_err(|_| TransportError::InvalidRequest("credentials are not a header value"))?;
    value.set_sensitive(true);
    Ok(request.with_header(http::header::AUTHORIZATION, value))
}

/// Decodes a JSON response body.
pub(crate) fn parse_json<R: DeserializeOwned>(
    response: &ChatResponse,
) -> Result<R, TransportError> {
    serde_json::from_slice(&response.body).map_err(|e| {
        // serde_json errors can quote input: keep only the position.
        TransportError::InvalidResponse(format!(
            "JSON {:?} error at line {} column {}",
            e.classify(),
            e.line(),
            e.column()
        ))
    })
}
