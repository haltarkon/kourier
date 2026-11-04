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

/// A task that ends with whoever owns this guard (dropping a tokio `JoinHandle` alone detaches
/// its task): a network task of [`Net::run_aborting`], or a task of the actor's `LocalSet` that
/// must not outlive the future that made it.
pub(crate) struct AbortOnDrop<R>(pub(crate) tokio::task::JoinHandle<R>);

impl<R> Drop for AbortOnDrop<R> {
    fn drop(&mut self) {
        // Nothing happens to a task that finished.
        self.0.abort();
    }
}

/// What a network task came to: its result, or [`TransportError::Disconnected`] for a panic or
/// a runtime that shut down (logged).
fn joined<R>(
    task: Result<Result<R, TransportError>, tokio::task::JoinError>,
) -> Result<R, TransportError> {
    match task {
        Ok(result) => result,
        Err(error) => {
            tracing::error!(panic = error.is_panic(), "network task did not finish");
            Err(TransportError::Disconnected)
        }
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
    ///
    /// The future runs ON when the one that waits for it is dropped (dropping a tokio
    /// `JoinHandle` detaches its task): right for a request that may already be on the wire
    /// and whose effect is the same whoever listens, wrong for a connect, see
    /// [`Self::run_aborting`].
    pub(crate) async fn run<F, R>(&self, future: F) -> Result<R, TransportError>
    where
        F: Future<Output = Result<R, TransportError>> + Send + 'static,
        R: Send + 'static,
    {
        joined(self.rt.spawn(future).await)
    }

    /// [`Self::run`], but the task ENDS with whoever waits for it: when the returned future is
    /// dropped (a `select!` that went the other way, a `timeout`, a cancelled task) the task is
    /// aborted at its next await point. For the futures that open a socket. A connect that
    /// Kourier has stopped waiting for must stop: left running it still completes at the
    /// server, which then holds two sockets of the device and closes the older one with 4409
    /// ("connected elsewhere with the same credentials"), perhaps the one a later request rides
    /// on (the `DELETE /v1/devices/<id>` of a removal, F1). It narrows the window rather than
    /// closing it: an upgrade that has reached the server already completes there.
    pub(crate) async fn run_aborting<F, R>(&self, future: F) -> Result<R, TransportError>
    where
        F: Future<Output = Result<R, TransportError>> + Send + 'static,
        R: Send + 'static,
    {
        let mut task = AbortOnDrop(self.rt.spawn(future));
        joined((&mut task.0).await)
    }

    /// Opens the authenticated chat socket.
    pub(crate) async fn connect_auth(
        &self,
        credentials: AuthCredentials,
    ) -> Result<Connected<T::Socket, ChatEvent>, TransportError> {
        let traced = Traced::start("chat", &Method::CONNECT, || "authenticated socket".into());
        let transport = self.transport.clone();
        let result = self
            .run_aborting(async move { transport.connect_auth(&credentials).await })
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
            .run_aborting(async move { transport.connect_unauth().await })
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
            .run_aborting(async move { transport.connect_provisioning().await })
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
        let result = joined((&mut task.0).await);
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
        // Aborting: a stopped send drops the upload that waits here, and the request, with the
        // body it reads, must go with it (the producer of the body ends when its receiver does).
        let result = self
            .run_aborting(async move {
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

    /// Sends one upload request to a CDN. Aborting like [`Self::https_upload_watched`]: an
    /// upload that is stopped does not leave its requests running.
    pub(crate) async fn https_upload(
        &self,
        request: UploadRequest,
    ) -> Result<HttpsResponse, TransportError> {
        let traced = Traced::start("upload", &request.method, || "(signed upload URL)".into());
        let transport = self.transport.clone();
        let result = self
            .run_aborting(async move { transport.https_upload(request).await })
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::sync::Notify;

    use super::*;
    use crate::net::libsignal::LibsignalSocket;

    /// What the stand-in transport saw of its connects.
    #[derive(Default)]
    struct Seen {
        /// Connects begun.
        started: AtomicUsize,
        /// Connect futures dropped (finished or not).
        dropped: AtomicUsize,
        /// Connects that ran to their end.
        finished: AtomicUsize,
        /// Lets the connects that wait come to their end.
        release: Notify,
    }

    /// A transport whose three connects wait until the test releases them, then fail.
    struct Waiting(Arc<Seen>);

    struct OnDrop(Arc<Seen>);

    impl Drop for OnDrop {
        fn drop(&mut self) {
            self.0.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Waiting {
        async fn wait<R>(&self) -> Result<R, TransportError> {
            self.0.started.fetch_add(1, Ordering::SeqCst);
            let _guard = OnDrop(self.0.clone());
            self.0.release.notified().await;
            self.0.finished.fetch_add(1, Ordering::SeqCst);
            Err(TransportError::Disconnected)
        }
    }

    impl SignalTransport for Waiting {
        type Socket = LibsignalSocket;

        async fn connect_auth(
            &self,
            _credentials: &AuthCredentials,
        ) -> Result<Connected<LibsignalSocket, ChatEvent>, TransportError> {
            self.wait().await
        }

        async fn connect_unauth(
            &self,
        ) -> Result<Connected<LibsignalSocket, ChatEvent>, TransportError> {
            self.wait().await
        }

        async fn connect_provisioning(
            &self,
        ) -> Result<Connected<LibsignalSocket, ProvisioningEvent>, TransportError> {
            self.wait().await
        }

        async fn https(&self, _request: HttpsRequest) -> Result<HttpsResponse, TransportError> {
            self.wait().await
        }

        async fn https_download(
            &self,
            _request: HttpsRequest,
            _sink: DownloadSink,
        ) -> Result<http::StatusCode, TransportError> {
            self.wait().await
        }

        async fn https_upload(
            &self,
            _request: UploadRequest,
        ) -> Result<HttpsResponse, TransportError> {
            self.wait().await
        }

        fn network_changed(&self) {}
    }

    fn credentials() -> AuthCredentials {
        AuthCredentials {
            aci: libsignal_core::Aci::from(uuid::Uuid::from_u128(0xabcd_ef01)),
            device_id: libsignal_core::DeviceId::new(2).expect("a device id"),
            password: zeroize::Zeroizing::new("pw".to_owned()),
        }
    }

    /// Waits until `seen` satisfies `done`, at most two seconds.
    async fn until(seen: &Seen, what: &str, done: impl Fn(&Seen) -> bool) {
        for _ in 0..200 {
            if done(seen) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("never happened: {what}");
    }

    /// F1 (P1): a connect Kourier stops waiting for stops. The spawned task of a dropped
    /// `Net::run` future runs on (a tokio `JoinHandle` detaches), so the connect completed at
    /// the server after the caller had given it up, and the server closed the older of two
    /// sockets of the device with 4409. `run_aborting` ends the task with the future that
    /// waits for it. A request, which `run` serves, keeps running: its effect does not depend
    /// on who listens.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_connect_that_nobody_waits_for_any_more_stops() {
        for kind in ["auth", "unauth", "provisioning"] {
            let seen = Arc::new(Seen::default());
            let net = Net::new(
                Arc::new(Waiting(seen.clone())),
                tokio::runtime::Handle::current(),
            );
            // The connect is under way when its waiter is dropped (a timeout, a `select!`).
            let outcome = match kind {
                "auth" => tokio::time::timeout(
                    Duration::from_millis(150),
                    net.connect_auth(credentials()),
                )
                .await
                .map(drop),
                "unauth" => tokio::time::timeout(Duration::from_millis(150), net.connect_unauth())
                    .await
                    .map(drop),
                _ => tokio::time::timeout(Duration::from_millis(150), net.connect_provisioning())
                    .await
                    .map(drop),
            };
            assert!(outcome.is_err(), "{kind}: the connect is still waiting");
            assert_eq!(seen.started.load(Ordering::SeqCst), 1, "{kind}");
            // Not released: the task has to end because its waiter went, not because it was told.
            until(&seen, "the connect task was aborted", |s| {
                s.dropped.load(Ordering::SeqCst) == 1
            })
            .await;
            seen.release.notify_waiters();
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(
                seen.finished.load(Ordering::SeqCst),
                0,
                "{kind}: the connect ran on to its end"
            );
        }
    }

    /// The contrast that makes the rule above a choice: a plain request (`Net::run`) is not
    /// aborted when its waiter goes, since it may be on the wire already.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_request_that_nobody_waits_for_any_more_runs_on() {
        let seen = Arc::new(Seen::default());
        let net = Net::new(
            Arc::new(Waiting(seen.clone())),
            tokio::runtime::Handle::current(),
        );
        let request = HttpsRequest::get(crate::net::HttpsHost::Storage, "/v1/x");
        let outcome = tokio::time::timeout(Duration::from_millis(150), net.https(request))
            .await
            .map(drop);
        assert!(outcome.is_err(), "the request is still waiting");
        seen.release.notify_waiters();
        until(&seen, "the request ran to its end", |s| {
            s.finished.load(Ordering::SeqCst) == 1
        })
        .await;
    }
}
