//! [`RealSession`]: the real WhatsApp client behind the [`Session`] trait (W2, chunk C4b).
//!
//! Placement (decision L2). Each client runs on a runtime of its OWN (two workers, built by the
//! factory on the actor's thread, never at the actor's start), apart from the shared network
//! runtime of Signal: upstream's calls spawn tasks on whatever runtime they are called from, and
//! W3a's inline handler blocks its dispatching thread for one commit. Every method of the trait
//! runs as a task on that runtime that the actor awaits (critic H1). The client is started with
//! `Bot::spawn`, the only public way to get the event handler subscribed and the app-state and
//! history sync worker started; the [`BotHandle`] is kept and ended with `shutdown`.
//!
//! Who supervises. Upstream's run loop does, with WA Web's reconnect schedule, its rate-limit
//! penalty and its pause/resume contract. Kourier does NOT replace it (an attempt with
//! auto-reconnect off makes upstream read the client as `Finished` between two runs, which every
//! upstream waiter that honours it would act on, and the schedule is not something an offline test
//! can prove). But the loop publishes NO event for a failed connect (critic H2): an offline start
//! would stay "Connecting" for ever. So the two things a connect needs, the transport factory and
//! the HTTP client, are wrapped here ([`ProbedTransport`], [`ProbedHttp`]), and a failure is read
//! where it happens, by TYPED position and never by text:
//!
//! - a transport that cannot be opened, or an open that upstream's own 20 s timeout cancels
//!   (the future is dropped: a drop guard reports it), says `WillRetry(Unreachable)`;
//! - a fetch of the version file that fails or answers with a non-2xx status says the same, except
//!   a 4xx answer, which says "outdated" (decision L5: `UpdateApp` only for 405 or a 4xx answer of
//!   the version file); any other request is not about the connection and is not watched.
//!
//! The retry time is upstream's and is not exposed: `Offline` carries no `retry_at_ms`, and
//! `reconnect_now` does nothing (the only public way to cut upstream's wait, `pause` then
//! `resume`, also clears its rate-limit penalty, which a mere network hint must not do).
//!
//! What the event handler holds. The [`Forwarder`] is a RAW handler (`with_event_handler`) and
//! holds no strong reference to the client, so the client's own event bus never keeps the client
//! alive and the store is released after `shutdown` (critic M9).
//!
//! The stanza interceptor (W2c, W3b). ONE interceptor per client ([`Stanzas`]: the pairing's claim of
//! `prepair` for an `<iq>`, the call claim of `calls` for a `<call>`) is registered between `build_bot`
//! and `Bot::spawn`, so it exists before the run loop dials: the calls of the offline queue are claimed,
//! and no `<pair-success>` can reach upstream's own handler before the rules judged it. Its
//! [`InterceptorHandle`] lives in [`Running`] as long as the client does and is dropped after
//! `bot.shutdown()` (upstream's shutdown waits for the run loop, so there is no moment in which the claim
//! is gone and the client still dispatches). The error iq that refuses a pairing is sent by a task on the
//! client's runtime ([`refusal_dispatch`]: the client held weakly, [`REFUSAL_WAIT`] of 5 s), never by the
//! read loop that calls the interceptor; it leaves BEFORE the login is told, because the login's end stops
//! the client. A refused pairing therefore ends here without a store write, a `<pair-device-sign>` or a
//! 515, and the core's `Logout` that follows finds no device ([`DeviceLogout::NoDevice`]).

use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use kourier_model::{BackendError, DeviceLogout};
use tokio::runtime::{Builder, Handle, Runtime};
use tokio::sync::OnceCell;
use wacore::iq::privacy::{PrivacyCategory, PrivacyValue};
use wacore::net::{
    HttpClient, HttpRequest, HttpResponse, StreamingHttpResponse, Transport, TransportEvent,
    TransportFactory, UploadBody,
};
use wacore::store::commands::DeviceCommand;
use wacore::types::events::{Event, EventHandler};
use wacore::types::message::AddressingMode;
use wacore_binary::Jid;
use whatsapp_rust::bot::BotHandle;
use whatsapp_rust::features::{BlockingError, ChatActivity, ChatStateError, GroupError};
use whatsapp_rust::pair_code::{PairCodeError, PairError};
use whatsapp_rust::request::IqError;
use whatsapp_rust::send::{SendError, SendOptions, SendRequest};
use whatsapp_rust::{
    Client, ClientError, DeregistrationOutcome, ErrorChainExt as _, Freshness, GroupLookupResult,
    GroupMetadataResult, InterceptorHandle, MessageId, SentFrameLease, StoreRelease,
};

use super::{
    AskFailure, Devices, Groups, Hooks, Outgoing, PairCodeFailure, ReadKind, Receive, Released,
    SendFailure, Session, SessionFactory, Stanzas, UploadFailure, Uploaded, Wiring,
};
use crate::NetParts;
use crate::appstate::{Activity, ResyncReport};
use crate::calls::{self, CallInterceptor};
use crate::history::capture::CaptureHook;
use crate::hook::{ClientNotes, Hook};
use crate::prepair::{Dispatch, PrePair};
use crate::props;
use crate::send::address::Namespace;
use crate::send::route::Shape;
use crate::send::upload::SealedSource;
use crate::tap::{Delivered, Tap};
use crate::translate::{self, HoldState, RetryWhy, WaSignal};
use crate::wam::Watch;

/// How long `logout` waits for a socket the server will answer on (an authenticated one, see
/// [`Reach`]) before it asks the server to remove the companion anyway (brief 3.6: a client started
/// only to log out needs a moment to connect; review F2: ~20 s bound).
const LOGOUT_CONNECT_WAIT: Duration = Duration::from_secs(20);

/// How long the error iq that refuses a pairing may take to leave (W2c). The login ends when it did or this
/// ran out; a hung socket must not keep a refused pairing's client running.
const REFUSAL_WAIT: Duration = Duration::from_secs(5);

/// How long `stop` waits for the store to be released (critic M9: a timeout is logged, it never
/// turns a finished logout into a failure).
pub(crate) const RELEASE_WAIT: Duration = Duration::from_secs(5);

/// The host of the file upstream reads the app version from. Only a request to it is about the
/// connection (see the module doc).
const VERSION_HOST_PREFIX: &str = "https://web.whatsapp.com/";

/// The production [`SessionFactory`]: sessions over the real client, made from [`NetParts`].
pub(crate) struct RealFactory {
    parts: NetParts,
    logout_wait: Duration,
    hook: Option<crate::parts::Hook>,
    host_name: String,
}

impl RealFactory {
    pub(crate) fn new(parts: NetParts) -> Self {
        let logout_wait = parts.logout_wait().unwrap_or(LOGOUT_CONNECT_WAIT);
        let hook = parts.builder_hook();
        let host_name = parts.host_name().to_owned();
        Self {
            parts,
            logout_wait,
            hook,
            host_name,
        }
    }
}

impl SessionFactory for RealFactory {
    fn device_name(&self) -> String {
        crate::props::device_name(&self.host_name)
    }

    /// The actor's call, on its own thread, at the first `Connect` or `StartLogin` and once more
    /// for each client it makes again: the parts are made here (exactly once per client) and the
    /// client's runtime is built here, so nothing of upstream is touched before this.
    fn make(&mut self, wiring: Wiring) -> Arc<dyn Session> {
        let (transport, http) = self.parts.make();
        let probe = Probe {
            tap: wiring.tap.clone(),
        };
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("wa-client")
            .enable_all()
            .build();
        let (runtime, error) = match runtime {
            Ok(runtime) => (Some(runtime), None),
            Err(_) => (
                None,
                Some(BackendError::internal(
                    "the WhatsApp client's runtime did not start",
                )),
            ),
        };
        Arc::new(RealSession {
            handle: runtime.as_ref().map(|r| r.handle().clone()),
            runtime: Mutex::new(runtime),
            startup_error: error,
            transport: Mutex::new(Some(Arc::new(ProbedTransport {
                inner: transport,
                probe: probe.clone(),
            }))),
            http: Mutex::new(Some(Arc::new(ProbedHttp { inner: http, probe }))),
            wiring,
            hook: self.hook.clone(),
            host_name: self.host_name.clone(),
            core: Arc::new(Core {
                logout_wait: self.logout_wait,
                ..Core::default()
            }),
            running: Mutex::new(None),
            pending_release: Mutex::new(None),
        })
    }
}

/// Where a failed connect is reported (see the module doc).
#[derive(Clone)]
struct Probe {
    tap: Tap,
}

impl Probe {
    fn unreachable(&self) {
        self.tap.deliver(WaSignal::WillRetry(RetryWhy::Unreachable));
    }

    fn outdated(&self) {
        self.tap.deliver(WaSignal::Outdated);
    }
}

/// Reports "unreachable" when it is dropped still armed: the operation failed, or the future that
/// held it was cancelled (upstream's 20 s connect timeout drops it).
struct ReportOnDrop<'a> {
    probe: &'a Probe,
    armed: bool,
}

impl<'a> ReportOnDrop<'a> {
    fn new(probe: &'a Probe) -> Self {
        Self { probe, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ReportOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.probe.unreachable();
        }
    }
}

/// The transport factory upstream is given: the parts' own, watched.
struct ProbedTransport {
    inner: Arc<dyn TransportFactory>,
    probe: Probe,
}

#[async_trait]
impl TransportFactory for ProbedTransport {
    async fn create_transport(
        &self,
    ) -> Result<(Arc<dyn Transport>, async_channel::Receiver<TransportEvent>), anyhow::Error> {
        let mut report = ReportOnDrop::new(&self.probe);
        let opened = self.inner.create_transport().await;
        if opened.is_ok() {
            report.disarm();
        }
        opened
    }
}

/// The HTTP client upstream is given: the parts' own, with the version file watched. Everything
/// else is forwarded as it is, the optional capabilities included.
struct ProbedHttp {
    inner: Arc<dyn HttpClient>,
    probe: Probe,
}

#[async_trait]
impl HttpClient for ProbedHttp {
    async fn execute(&self, request: HttpRequest) -> anyhow::Result<HttpResponse> {
        if !request.url.starts_with(VERSION_HOST_PREFIX) {
            return self.inner.execute(request).await;
        }
        let mut report = ReportOnDrop::new(&self.probe);
        let answer = self.inner.execute(request).await;
        if let Ok(response) = &answer {
            match response.status_code {
                200..=299 => report.disarm(),
                // The server answered, and said no: updating is the way on (decision L5).
                400..=499 => {
                    report.disarm();
                    self.probe.outdated();
                }
                // A redirect or a server error: unreachable for now (the armed guard reports it).
                _ => {}
            }
        }
        answer
    }

    fn supports_streaming(&self) -> bool {
        self.inner.supports_streaming()
    }

    fn execute_streaming(&self, request: HttpRequest) -> anyhow::Result<StreamingHttpResponse> {
        self.inner.execute_streaming(request)
    }

    fn supports_upload_streaming(&self) -> bool {
        self.inner.supports_upload_streaming()
    }

    fn execute_upload(
        &self,
        request: HttpRequest,
        body: UploadBody,
        content_length: u64,
    ) -> anyhow::Result<HttpResponse> {
        self.inner.execute_upload(request, body, content_length)
    }

    fn resource_report(&self) -> Option<wacore::stats::HttpResourceReport> {
        self.inner.resource_report()
    }
}

/// What the logout waits for: a request sent NOW has a socket, an authenticated session (`<success>`
/// seen) and a reader to decode the answer. NOT full session readiness (review F2): upstream sets
/// that only after the post-login work (the critical app-state sync, the offline messages and their
/// receipts), so waiting for it is the longest window a refused account's client runs in; the
/// reachable socket exists as soon as `<success>` is through. A seam so a unit test can play both.
trait Reach {
    /// Resolves with `true` as soon as the client can reach the server; `false` when it never will
    /// (shut down, logged out, nothing reading, paused).
    fn until_reachable(&self) -> impl std::future::Future<Output = bool> + Send;
}

impl Reach for Client {
    async fn until_reachable(&self) -> bool {
        self.wait_until_reachable().await.is_reachable()
    }
}

/// Whether `client` became reachable within `wait`.
async fn reachable_within(client: &impl Reach, wait: Duration) -> bool {
    tokio::time::timeout(wait, client.until_reachable())
        .await
        .unwrap_or(false)
}

/// What the session and the event handler share. It holds NO strong reference to the client (see
/// the module doc).
#[derive(Default)]
struct Core {
    /// How long a logout waits for an authenticated session first.
    logout_wait: Duration,
    /// The client, set once at start.
    weak: OnceLock<Weak<Client>>,
    /// The runtime the client runs on, set once at start.
    handle: OnceLock<Handle>,
    /// What the one logout of this client reached (shared by the refusal's logout and the actor's).
    logout: OnceCell<DeviceLogout>,
    /// What the client sends, watched for usage statistics (`crate::wam`).
    wam: Watch,
}

impl Core {
    /// The one logout of this client: waits up to `wait` for a socket the server answers on (the
    /// first `<success>` after the pairing's 515, [`Reach`]; at once when there is one), so the
    /// remove-companion request can go, then logs out (upstream's terminal stop: it cannot be tried
    /// twice, so it is tried when the socket can carry it, and at the bound whatever the state).
    /// The second
    /// caller gets the first caller's answer, so the refusal of a pairing (which logs out at once
    /// from the event handler) and the core's `Logout` that follows report the same outcome.
    async fn logout_once(&self, wait: Duration) -> DeviceLogout {
        *self
            .logout
            .get_or_init(|| async {
                let Some(client) = self.weak.get().and_then(Weak::upgrade) else {
                    return DeviceLogout::NotReached;
                };
                if !wait.is_zero() {
                    let _ = reachable_within(&*client, wait).await;
                }
                let report = client.logout().await;
                match report.deregistration {
                    DeregistrationOutcome::Confirmed => DeviceLogout::Removed,
                    _ => DeviceLogout::NotReached,
                }
            })
            .await
    }
}

/// The client's event handler: THE one handler of the client (a raw one; no `on_event` or
/// `on_message` closure is registered anywhere). Per event, in this order:
///
/// 1. `SentFrame` goes to the statistics watch and nothing else (it runs on upstream's noise sender
///    task: nothing may wait there).
/// 2. A message stanza's ack goes to the ack registry ([`translate::message_ack`], `send::acks`) and nothing
///    else; upstream's `Disconnected` ends the registry's connection first (its waits end, D5).
/// 3. A durable event ([`translate::durable`]: receipts, placeholders; a group's notice, [`translate::noticed`], with the
///    acts the group's row decides) is written and the handler WAITS
///    for the commit ([`crate::events::EventWriter::write_and_wait`]), because upstream acknowledges
///    the stanza right after the handler returns (rule (a)). A write that fails ends the connection
///    (decision D5): `signal_shutdown_sync` suppresses this ack and every later one, the actor is told
///    and makes a new client after a backoff, and the server sends the stanza again.
/// 4. What the phone's app state says about a chat ([`crate::appstate::map::map`]: archive, pin, mute, marked
///    unread, clears, deletes for me, the unarchive setting) is written with its mirror in one zone by the same
///    wait ([`crate::events::EventWriter::write_settings_and_wait`]); a write that fails also puts the event's
///    collection on [`crate::appstate::Activity::lose`]'s set, which no clearing point clears (critic B1 point 3).
///    Only app-state mutation events count as traffic for rule (b)'s quiet look ([`translate::is_app_state`]).
/// 5. The messages the durability hook did NOT commit (newsletters, PDO recoveries) are queued into
///    the inbox without waiting ([`translate::unhooked`]; at most once, decision D4). The messages the
///    hook committed are not written here: the inbox has them.
/// 6. The signal for the connection and the login ([`translate::translate`]) goes to the tap; the
///    drain's signals first write the catch-up boundary in the writer's order
///    ([`Forwarder::mark_the_drain`]).
///
/// The interest stays ALL (the default): upstream then handles every receipt in a task of its own
/// on the client's runtime, so a waiting handler parks one of that runtime's two workers, never the
/// read loop. A narrower interest would also let a kind silently go unseen when a later step
/// translates it.
struct Forwarder {
    /// Counts the app-state traffic the handler sees: rule (b)'s quiet look (`crate::appstate`).
    activity: Arc<Activity>,
    tap: Tap,
    clock: Arc<dyn kourier_backend_api::Clock>,
    core: Arc<Core>,
    receive: Receive,
}

/// A write the account cannot go on without failed: no ack goes out for this stanza or any later one of
/// the connection, and the actor makes a new client. The handler's and the call interceptor's way to end
/// the connection.
fn end_connection(core: &Core, tap: &Tap) {
    if let Some(client) = core.weak.get().and_then(Weak::upgrade) {
        client.signal_shutdown_sync();
    }
    tap.deliver(WaSignal::WriteFailed);
}

impl Forwarder {
    /// A write the account cannot go on without failed (see [`end_connection`]). Logs the fact, never an
    /// input.
    fn write_failed(&self) {
        tracing::error!("the handler's write failed; the connection is being ended");
        end_connection(&self.core, &self.tap);
    }

    /// The catch-up boundary is written in the writer's order, from here: a commit queued before it
    /// is inside the drain, one queued after it is not, which is what the `catch_up` flag of a row
    /// needs to be exact (`crate::catchup`). A drain that begins or ends badly forgets it; upstream's
    /// completion takes it. Queued, never waited for: a failed write is logged, and the actor takes
    /// the boundary itself when it finds none.
    fn mark_the_drain(&self, signal: &WaSignal) {
        let queued = match signal {
            WaSignal::OfflineDone { .. } => self.receive.inbox.take_boundary_detached(),
            WaSignal::OfflinePreview { .. }
            | WaSignal::OfflineInterrupted
            | WaSignal::Disconnected { .. } => self.receive.inbox.forget_boundary_detached(),
            _ => return,
        };
        if queued.is_err() {
            tracing::error!("the catch-up boundary could not be queued");
        }
    }
}

impl EventHandler for Forwarder {
    fn handle_event(&self, event: Arc<Event>) {
        // Runs on upstream's noise sender task: nothing here may wait (`crate::wam`).
        if let Event::SentFrame(sent) = &*event {
            self.core.wam.look(&sent.plaintext);
            return;
        }
        // Only app-state mutations are traffic for rule (b)'s quiet look (W6a1, D4): typing, presence and receipts in
        // a busy group would otherwise keep the look from ever finding quiet.
        if translate::is_app_state(&event) {
            self.activity.event();
        }
        // A message stanza's ack answers the send that waits for it (W5a, `send::acks`); nothing else reads it.
        // Never waits. Nobody waiting is no fault here: the actor's transmit decides about late acks (D5).
        if let Some(ack) = translate::message_ack(&event) {
            self.receive.acks.answer(ack);
            return;
        }
        // The connection ended: no ack of its frames will come (D5: its waits end, a resend may go on the next).
        if matches!(&*event, Event::Disconnected(_)) {
            self.receive.acks.connection_ended();
        }
        // A receipt type that changes no message, counted by its reason (brief §2.4): a `sent` one is worth a line
        // (the recipient cannot take our message: it stays Sent), the others are routine.
        if let Some((why, messages)) = translate::unmapped_receipt(&event) {
            if why == translate::Unmapped::NotDeliverable {
                tracing::info!(
                    why = why.name(),
                    messages,
                    "a receipt says the recipient cannot take the message"
                );
            } else {
                tracing::debug!(
                    why = why.name(),
                    messages,
                    "a receipt that changes no message"
                );
            }
        }
        // A sync upstream raised that left collections unsynced: counts only (D3). Nothing is asked in answer; the
        // flag stays and the next start's resync is the repair.
        if let Event::AppStateSyncFailed(failed) = &*event {
            tracing::info!(
                fatal = failed.fatal.len(),
                retryable = failed.retryable.len(),
                skipped = failed.skipped.len(),
                "an app-state sync left collections unsynced"
            );
        }
        // What the phone's app state says about a chat (W6a1): the mirror and the core events are one zone, written
        // and committed before this returns (rule (a): upstream's cursor committed BEFORE it dispatched, so a mutation
        // lost here is lost for good). A failed zone ends the connection AND keeps the collection's flag standing for
        // the rest of this process (critic B1 point 3).
        // A record of the phone's call log (W6a1, A6): its line is written the same way, before this returns, because
        // the collection's flag is cleared once its events are durable. The call log has no mirror and no reconcile.
        if let Event::CallLogSync(sync) = &*event {
            let seen = crate::calls::log::Seen::from_sync(sync);
            if self
                .receive
                .writer
                .write_call_log_and_wait(seen, self.receive.stage2.own_known())
                .is_err()
            {
                self.activity.lose(crate::appstate::map::CALL_LOG);
                self.write_failed();
            }
            return;
        }
        if let Some(mapped) = crate::appstate::map::map(&event) {
            let collection = mapped.collection;
            if self.receive.writer.write_settings_and_wait(mapped).is_err() {
                self.activity.lose(collection);
                self.write_failed();
            }
            return;
        }
        // The server's `groups_dirty` (W6a2, B6): the groups it names get a "fetch me" mark in one zone, the same wait; the
        // fetch policy asks for them under its bucket, after `Online`. Nothing is asked here.
        let dirty = translate::dirty_groups(&event);
        if !dirty.is_empty()
            && self
                .receive
                .writer
                .write_groups_dirty_and_wait(dirty)
                .is_err()
        {
            self.write_failed();
            return;
        }
        // A group's notification (W6a2, B5): its events and what the group's row makes of its acts, one zone, the same wait.
        if let Some(noticed) = translate::noticed(
            &event,
            self.clock.now_ms(),
            &self.receive.stage2.own_known(),
        ) && self.receive.writer.write_notice_and_wait(noticed).is_err()
        {
            self.write_failed();
            return;
        }
        if let Some(events) = translate::durable(
            &event,
            self.clock.now_ms(),
            &self.receive.stage2.own_known(),
        ) && self
            .receive
            .writer
            .write_and_wait_reading(&events, translate::read_elsewhere(&event).as_slice())
            .is_err()
        {
            self.write_failed();
            return;
        }
        let unhooked = translate::unhooked(&event);
        if !unhooked.is_empty() {
            let count = unhooked.len();
            if self.receive.inbox.commit_detached(unhooked).is_err() {
                // Already acknowledged upstream: nothing to hold back, nothing to retry.
                tracing::error!(
                    messages = count,
                    "messages upstream recovered could not be queued"
                );
            }
        }
        let Some(signal) = translate::translate(&event, self.clock.now_ms()) else {
            return;
        };
        self.mark_the_drain(&signal);
        if self.tap.deliver(signal) == Delivered::Refused {
            // The pairing was refused: log the companion out at once, not after a hop to the actor
            // (critic H7). The logout goes on the first authenticated socket the 515 reconnect
            // brings (not after the session's full readiness, review F2), bounded.
            let core = self.core.clone();
            if let Some(handle) = self.core.handle.get() {
                handle.spawn(async move {
                    let wait = core.logout_wait;
                    core.logout_once(wait).await;
                });
            }
        }
    }
}

/// The way a refused pairing's error iq reaches the wire (W2c): a task on the client's runtime sends it,
/// bounded by [`REFUSAL_WAIT`], and then runs `done` with whether it left. No runtime (the client is not
/// started or was stopped) or no client: nothing is sent and `done(false)` runs at once, so the pairing is
/// still refused and the login still ends (fail closed). The core holds the client WEAKLY: this keeps no
/// client alive.
fn refusal_dispatch(core: Arc<Core>) -> Dispatch {
    Arc::new(move |reply, done| {
        let Some(handle) = core.handle.get().cloned() else {
            done(false);
            return;
        };
        let core = core.clone();
        handle.spawn(async move {
            let left = match (reply, core.weak.get().and_then(Weak::upgrade)) {
                (Some(reply), Some(client)) => matches!(
                    tokio::time::timeout(REFUSAL_WAIT, client.send_node(reply)).await,
                    Ok(Ok(()))
                ),
                _ => false,
            };
            done(left);
        });
    })
}

/// The client once started.
struct Running {
    /// Ends the bot: `shutdown` consumes it.
    bot: BotHandle,
    /// Observes the release of the store; taken before anything is shut down.
    released: StoreRelease,
    /// Keeps upstream's sent-frame event on, for the statistics watch. It holds the client weakly.
    lease: SentFrameLease,
    /// Keeps the stanza interceptor registered for as long as the client runs: dropping it would hand the
    /// next call to upstream's own handler, which answers it (W3b), and the next pairing to upstream's,
    /// which completes it unjudged (W2c).
    stanzas: InterceptorHandle,
}

/// The real client of ONE account.
pub(crate) struct RealSession {
    /// Owns the client's runtime until `stop`.
    runtime: Mutex<Option<Runtime>>,
    handle: Option<Handle>,
    startup_error: Option<BackendError>,
    transport: Mutex<Option<Arc<dyn TransportFactory>>>,
    http: Mutex<Option<Arc<dyn HttpClient>>>,
    wiring: Wiring,
    /// The fixture's builder step (tests only; `None` otherwise).
    hook: Option<crate::parts::Hook>,
    /// The application's host name, for the device label the client is built with.
    host_name: String,
    core: Arc<Core>,
    running: Mutex<Option<Running>>,
    /// The observer of the release of a `stop` that timed out (sticky: it answers any later wait).
    pending_release: Mutex<Option<StoreRelease>>,
}

impl RealSession {
    /// The client, while it runs.
    fn client(&self) -> Option<Arc<Client>> {
        self.running
            .lock()
            .expect("lock")
            .as_ref()
            .map(|running| running.bot.client())
    }

    /// Runs `work` as a task on the client's runtime and awaits it; `None` when the runtime is
    /// gone or the task died.
    async fn on_runtime<T, F>(&self, work: F) -> Option<T>
    where
        T: Send + 'static,
        F: std::future::Future<Output = T> + Send + 'static,
    {
        self.handle.as_ref()?.spawn(work).await.ok()
    }

    /// [`on_runtime`](Self::on_runtime) for work that must END with its caller (W5b, G14: the upload): the task is
    /// aborted when the returned future is dropped (a stall, a stop, a quit), never left running on the client's
    /// runtime.
    async fn on_runtime_or_abort<T, F>(&self, work: F) -> Option<T>
    where
        T: Send + 'static,
        F: std::future::Future<Output = T> + Send + 'static,
    {
        Aborting(self.handle.as_ref()?.spawn(work)).join().await
    }
}

/// A task of the client's runtime that is aborted when this goes (an abort after the end does nothing).
struct Aborting<T>(tokio::task::JoinHandle<T>);

impl<T> Aborting<T> {
    /// The task's answer; `None` when it died or was aborted.
    async fn join(mut self) -> Option<T> {
        (&mut self.0).await.ok()
    }
}

impl<T> Drop for Aborting<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[async_trait]
impl Session for RealSession {
    async fn start(&self) -> Result<(), BackendError> {
        if let Some(error) = &self.startup_error {
            return Err(error.clone());
        }
        let transport = self.transport.lock().expect("lock").take();
        let http = self.http.lock().expect("lock").take();
        let (Some(transport), Some(http)) = (transport, http) else {
            return Err(BackendError::internal(
                "the WhatsApp client was started before",
            ));
        };
        let handler = Forwarder {
            activity: self.wiring.store.activity().clone(),
            tap: self.wiring.tap.clone(),
            clock: self.wiring.clock.clone(),
            core: self.core.clone(),
            receive: self.wiring.receive.clone(),
        };
        let durability = Hook::new(
            self.wiring.receive.inbox.clone(),
            Arc::new(ClientNotes::new(
                self.wiring.receive.stage2.clone(),
                self.wiring.tap.clone(),
            )),
        );
        // The history the phone pushes after a link is kept in the stage before the phone is told (W4).
        let capture = CaptureHook::new(
            self.wiring.receive.capture.clone(),
            self.wiring.store.clone(),
            self.wiring.tap.clone(),
        );
        // Calls are shown, never taken (W3b): this backend claims every `<call>` stanza.
        let claim = calls::claim(std::env::var(calls::BUILTIN_ENV).ok().as_deref());
        if claim == calls::Claim::LetBuiltInAnswer {
            tracing::warn!(
                "KOURIER_WA_CALL_BUILTIN is set: upstream's own call handler will answer calls (a debug test switch)"
            );
        }
        let stanzas = {
            let (core, tap) = (self.core.clone(), self.wiring.tap.clone());
            Stanzas {
                pairing: PrePair::new(self.wiring.tap.clone(), refusal_dispatch(self.core.clone())),
                calls: CallInterceptor::new(
                    self.wiring.receive.writer.clone(),
                    self.wiring.clock.clone(),
                    self.wiring.receive.stage2.clone(),
                    Arc::new(move || end_connection(&core, &tap)),
                    claim,
                ),
            }
        };
        let store = self.wiring.store.clone();
        let hook = self.hook.clone();
        let host_name = self.host_name.clone();
        let core = self.core.clone();
        if let Some(handle) = &self.handle {
            let _ = core.handle.set(handle.clone());
        }
        let started = self
            .on_runtime(async move {
                let bot = super::build_bot(
                    store,
                    transport,
                    http,
                    handler,
                    Hooks {
                        durability,
                        capture,
                    },
                    hook,
                    &host_name,
                )
                .await
                .map_err(|_| BackendError::internal("the WhatsApp client could not be built"))?;
                let client = bot.client();
                let _ = core.weak.set(Arc::downgrade(&client));
                // Before the run loop starts: the calls of the offline queue are claimed too, and no
                // pairing reaches upstream's own handler unjudged.
                let stanzas = super::register_stanzas(&bot, stanzas);
                let released = client.store_release();
                // Before the run loop starts, so no frame of the connection goes unwatched.
                let lease = client.acquire_sent_frame_forwarding();
                drop(client);
                // Starts the sync worker and subscribes the handler, then the run loop (upstream's
                // own supervision, see the module doc).
                Ok::<_, BackendError>(Running {
                    bot: bot.spawn(),
                    released,
                    lease,
                    stanzas,
                })
            })
            .await
            .ok_or_else(|| BackendError::internal("the WhatsApp client's runtime stopped"))??;
        *self.running.lock().expect("lock") = Some(started);
        Ok(())
    }

    async fn pause(&self) {
        if let Some(client) = self.client() {
            self.on_runtime(async move { client.pause().await }).await;
        }
    }

    async fn resume(&self) {
        if let Some(client) = self.client() {
            client.resume();
        }
    }

    async fn pair_with_code(&self, phone_number: &str) -> Result<(), PairCodeFailure> {
        let Some(client) = self.client() else {
            return Err(PairCodeFailure::NoAnswer);
        };
        let options = props::pair_code_options(phone_number);
        match self
            .on_runtime(async move { client.pair_with_code(options).await })
            .await
        {
            // The code reaches the machine as `PairingCode`, from the event.
            Some(Ok(_code)) => Ok(()),
            Some(Err(error)) => Err(pair_failure(&error)),
            None => Err(PairCodeFailure::NoAnswer),
        }
    }

    async fn cancel_pair_code(&self) {
        if let Some(client) = self.client() {
            self.on_runtime(async move { client.cancel_pair_code().await })
                .await;
        }
    }

    async fn logout(&self) -> DeviceLogout {
        let core = self.core.clone();
        let wait = core.logout_wait;
        self.on_runtime(async move { core.logout_once(wait).await })
            .await
            .unwrap_or(DeviceLogout::NotReached)
    }

    /// Nothing: upstream's schedule paces every reconnect (see the module doc).
    async fn reconnect_now(&self) {}

    async fn drop_connection(&self) {
        let Some(client) = self.client() else {
            return;
        };
        self.on_runtime(async move { client.reconnect().await })
            .await;
        // Upstream dispatches no `Disconnected` for a reconnect it was asked for: the drain's boundary and the
        // actor hear what an unclean end of the connection tells them (the Forwarder's `Disconnected` arm). The
        // ack registry's connection is ended by the caller (`send::acks::Acks::end`).
        if self
            .wiring
            .receive
            .inbox
            .forget_boundary_detached()
            .is_err()
        {
            tracing::error!("the catch-up boundary could not be queued");
        }
        self.wiring
            .tap
            .deliver(WaSignal::Disconnected { clean: false });
    }

    async fn fetch_hold(&self) -> Result<HoldState, BackendError> {
        let Some(client) = self.client() else {
            return Err(BackendError::transient("no client to ask"));
        };
        let answer = self
            .on_runtime(async move { client.mex().fetch_reachout_timelock().await })
            .await;
        match answer {
            Some(Ok(state)) => Ok(translate::hold_of(&state)),
            _ => Err(BackendError::transient(
                "the reach-out lock could not be read",
            )),
        }
    }

    async fn resync(&self, collections: Vec<String>) -> Result<ResyncReport, BackendError> {
        let Some(client) = self.client() else {
            return Err(BackendError::transient("no client to ask"));
        };
        let names = translate::collections_of(&collections);
        // Polled on the client's runtime: the replay's events reach the handler on the thread that polls this call.
        let report = self
            .on_runtime(async move {
                client
                    .resync_app_state(names, whatsapp_rust::AppStateResyncMode::Snapshot)
                    .await
            })
            .await;
        translate::resync_answer(report)
    }

    async fn stop(&self) -> Released {
        // Order: the observer of the release was taken at start; the bot is ended (upstream's
        // `shutdown`, which waits for the run loop); every strong reference to the client goes
        // with it; only then the release is awaited.
        let running = self.running.lock().expect("lock").take();
        let released = match running {
            Some(Running {
                bot,
                released,
                lease,
                stanzas,
            }) => {
                self.on_runtime(async move {
                    let _ = bot.shutdown().await;
                    drop(lease);
                    drop(stanzas);
                })
                .await;
                Some(released)
            }
            None => None,
        };
        let result = match released {
            Some(released) => match tokio::time::timeout(RELEASE_WAIT, released.wait()).await {
                Ok(()) => Released::Yes,
                Err(_) => {
                    // Kept for `release_wait`: the actor makes no new client until it has come.
                    *self.pending_release.lock().expect("lock") = Some(released);
                    Released::TimedOut
                }
            },
            None => Released::Yes,
        };
        if let Some(runtime) = self.runtime.lock().expect("lock").take() {
            runtime.shutdown_background();
        }
        result
    }

    async fn release_wait(&self, limit: Duration) -> Released {
        // The future owns nothing of the lock: it is taken under it and awaited outside it.
        let wait = self
            .pending_release
            .lock()
            .expect("lock")
            .as_ref()
            .map(StoreRelease::wait);
        let Some(wait) = wait else {
            return Released::Yes;
        };
        match tokio::time::timeout(limit, wait).await {
            Ok(()) => {
                *self.pending_release.lock().expect("lock") = None;
                Released::Yes
            }
            Err(_) => Released::TimedOut,
        }
    }

    /// THE crate's one `Client::send` (decision D2; `tests/builder.rs` counts it): every kind goes out
    /// here, with the core's local as the stanza id, so an edit, a revoke and a reaction get the same
    /// retry-cache row and acks as a message (never upstream's `edit_message`, `revoke_message` or
    /// `send_reaction`, which mint or borrow ids).
    async fn send(&self, out: Outgoing) -> Result<(), SendFailure> {
        let Some(client) = self.client() else {
            return Err(SendFailure::NoClient);
        };
        let Ok(id) = MessageId::new(out.id) else {
            return Err(SendFailure::InvalidRequest);
        };
        let mut options = SendOptions::default()
            .with_message_id(id)
            .with_device_freshness(match out.devices {
                Devices::Cached => Freshness::CachePreferred,
                Devices::Refresh => Freshness::Refresh,
            })
            .with_group_metadata_freshness(match out.groups {
                Groups::Cached => Freshness::CachePreferred,
                Groups::Refresh => Freshness::Refresh,
            });
        if let Some(seconds) = out.expiration {
            options = options.with_ephemeral_expiration(seconds);
        }
        let request = SendRequest::new(&out.to, out.message).with_options(options);
        match self
            .on_runtime(async move { client.send(request).await })
            .await
        {
            Some(Ok(_written)) => Ok(()),
            Some(Err(error)) => Err(send_failure(&error)),
            None => Err(SendFailure::NoClient),
        }
    }

    async fn upload(
        &self,
        source: SealedSource,
        info: wacore::upload::EncryptedMediaInfo,
        shape: Shape,
    ) -> Result<Uploaded, UploadFailure> {
        let Some(client) = self.client() else {
            return Err(UploadFailure::NoClient);
        };
        let kind = shape.media_type();
        match self
            .on_runtime_or_abort(async move { client.upload_stream(source, info, kind).await })
            .await
        {
            Some(Ok(response)) => Ok(Uploaded {
                url: response.url,
                direct_path: response.direct_path,
                media_key_timestamp: response.media_key_timestamp,
            }),
            Some(Err(error)) => Err(upload_failure(&error)),
            None => Err(UploadFailure::NoClient),
        }
    }

    async fn mark_read(
        &self,
        chat: Jid,
        sender: Option<Jid>,
        ids: Vec<String>,
        kind: ReadKind,
    ) -> Result<(), SendFailure> {
        let Some(client) = self.client() else {
            return Err(SendFailure::NoClient);
        };
        let done = self
            .on_runtime(async move {
                let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
                match kind {
                    ReadKind::Read => client.mark_as_read(&chat, sender.as_ref(), &ids).await,
                    ReadKind::Played => client.mark_as_played(&chat, sender.as_ref(), &ids).await,
                }
            })
            .await;
        match done {
            Some(Ok(())) => Ok(()),
            Some(Err(error)) => Err(anyhow_failure(&error)),
            None => Err(SendFailure::NoClient),
        }
    }

    async fn typing(&self, chat: Jid, on: bool) -> Result<(), SendFailure> {
        let Some(client) = self.client() else {
            return Err(SendFailure::NoClient);
        };
        let state = if on {
            ChatActivity::Typing
        } else {
            ChatActivity::Idle
        };
        let done = self
            .on_runtime(async move { client.chatstate().send(&chat, state).await })
            .await;
        match done {
            Some(Ok(())) => Ok(()),
            Some(Err(ChatStateError::Client(error))) => Err(client_failure(&error)),
            Some(Err(_)) => Err(SendFailure::Internal),
            None => Err(SendFailure::NoClient),
        }
    }

    async fn privacy_known(&self) -> Result<(), SendFailure> {
        let Some(client) = self.client() else {
            return Err(SendFailure::NoClient);
        };
        let done = self
            .on_runtime(async move {
                let settings = client.fetch_privacy_settings().await?;
                // Upstream's background init applies the answer in exactly this way (`src/client/node_io.rs`
                // at the pin): the field its `read`/`read-self` decision reads. Applying it here as well makes
                // the setting the client's own before our first receipt, whichever of the two fetches ends first.
                let disabled = matches!(
                    settings.get_value(&PrivacyCategory::ReadReceipts),
                    Some(PrivacyValue::None)
                );
                let manager = client.persistence_manager();
                if manager.get_device_snapshot().read_receipts_disabled != disabled {
                    manager
                        .process_command(DeviceCommand::SetReadReceiptsDisabled(disabled))
                        .await;
                    if manager.flush().await.is_err() {
                        tracing::debug!("the read-receipt setting could not be saved");
                    }
                }
                Ok::<(), IqError>(())
            })
            .await;
        match done {
            Some(Ok(())) => Ok(()),
            Some(Err(error)) => Err(iq_failure(&error)),
            None => Err(SendFailure::NoClient),
        }
    }

    async fn lid_migrated(&self) -> bool {
        let Some(client) = self.client() else {
            return false;
        };
        self.on_runtime(async move { client.is_lid_migrated().await })
            .await
            .unwrap_or(false)
    }

    async fn group_mode(&self, group: Jid) -> Result<Namespace, SendFailure> {
        let Some(client) = self.client() else {
            return Err(SendFailure::NoClient);
        };
        let info = self
            .on_runtime(async move { client.groups().routing_info(&group).await })
            .await;
        match info {
            Some(Ok(info)) => Ok(namespace_of(info.addressing_mode)),
            Some(Err(error)) => Err(group_failure(&error)),
            None => Err(SendFailure::NoClient),
        }
    }

    async fn participating(&self) -> Result<Vec<Jid>, AskFailure> {
        let Some(client) = self.client() else {
            return Err(AskFailure::NoClient);
        };
        // The ONE call of `list_participating` (`tests/builder.rs`).
        let answer = self
            .on_runtime(async move { client.groups().list_participating().await })
            .await;
        match answer {
            Some(Ok(list)) => Ok(list.into_iter().map(|overview| overview.id).collect()),
            Some(Err(error)) => Err(group_ask_failure(&error)),
            None => Err(AskFailure::NoClient),
        }
    }

    async fn fetch_groups(&self, groups: Vec<Jid>) -> Result<Vec<GroupMetadataResult>, AskFailure> {
        let Some(client) = self.client() else {
            return Err(AskFailure::NoClient);
        };
        let answer = self
            .on_runtime(async move {
                let api = client.groups();
                // The ONE call of `fetch_metadata_batch` (`tests/builder.rs`): one IQ for every group asked.
                let mut results = api.fetch_metadata_batch(&groups).await?;
                // The numbers the server left out of a LID group's members, from the pairs the store already holds
                // (no IQ, and no pair is learned or written: D7, H4).
                for result in &mut results {
                    if let GroupLookupResult::Found(metadata) = result {
                        api.resolve_participant_addresses(metadata).await;
                    }
                }
                Ok::<_, GroupError>(results)
            })
            .await;
        match answer {
            Some(Ok(results)) => Ok(results),
            Some(Err(error)) => Err(group_ask_failure(&error)),
            None => Err(AskFailure::NoClient),
        }
    }

    async fn fetch_group(&self, group: Jid) -> Result<GroupMetadataResult, AskFailure> {
        let Some(client) = self.client() else {
            return Err(AskFailure::NoClient);
        };
        let asked = group.clone();
        let answer = self
            .on_runtime(async move {
                let api = client.groups();
                // The ONE call of `fetch_metadata` (`tests/builder.rs`): one single-query IQ.
                let mut metadata = api.fetch_metadata(&group).await?;
                // The numbers the server left out of a LID group's members, from the store's pairs (no IQ, no pair
                // learned: D7, H4).
                api.resolve_participant_addresses(&mut metadata).await;
                Ok::<_, GroupError>(metadata)
            })
            .await;
        match answer {
            Some(Ok(metadata)) => Ok(GroupLookupResult::Found(Box::new(metadata))),
            // A refusal that says we are not in the group is an answer, as in the batch.
            Some(Err(GroupError::Iq(IqError::ServerError { code: 403, .. }))) => {
                Ok(GroupLookupResult::Forbidden(asked))
            }
            Some(Err(GroupError::Iq(IqError::ServerError { code: 404, .. }))) => {
                Ok(GroupLookupResult::NotFound(asked))
            }
            Some(Err(error)) => Err(group_ask_failure(&error)),
            None => Err(AskFailure::NoClient),
        }
    }

    async fn blocklist(&self) -> Result<Vec<Jid>, AskFailure> {
        let Some(client) = self.client() else {
            return Err(AskFailure::NoClient);
        };
        // The ONE call of `get_blocklist` (`tests/builder.rs`).
        let answer = self
            .on_runtime(async move { client.blocking().get_blocklist().await })
            .await;
        match answer {
            Some(Ok(entries)) => Ok(entries.into_iter().map(|entry| entry.jid).collect()),
            Some(Err(error)) => Err(blocking_ask_failure(&error)),
            None => Err(AskFailure::NoClient),
        }
    }
}

/// What a failed read of the groups is, by upstream's typed error (L5: never its text).
fn group_ask_failure(error: &GroupError) -> AskFailure {
    match error {
        GroupError::Iq(error) => iq_ask_failure(error),
        _ => AskFailure::Internal,
    }
}

/// What a failed read of the blocklist is, by upstream's typed error (L5: never its text).
fn blocking_ask_failure(error: &BlockingError) -> AskFailure {
    match error {
        BlockingError::Iq(error) => iq_ask_failure(error),
        _ => AskFailure::Internal,
    }
}

/// An IQ error of a read by position. A server rejection keeps its code and its `backoff` (seconds), not its text.
fn iq_ask_failure(error: &IqError) -> AskFailure {
    match error {
        IqError::Timeout => AskFailure::Timeout,
        IqError::NotConnected
        | IqError::Disconnected(_)
        | IqError::Socket(_)
        | IqError::EncryptSend(_)
        | IqError::ClientState(_)
        | IqError::InternalChannelClosed => AskFailure::NotConnected,
        IqError::ServerError { code, backoff, .. } => AskFailure::Refused {
            code: *code,
            backoff: backoff.map(|seconds| Duration::from_secs(u64::from(seconds))),
        },
        IqError::ParseError(_) => AskFailure::Unreadable,
        // `non_exhaustive`: a variant a pin bump adds is internal until it is read here.
        _ => AskFailure::Internal,
    }
}

/// What a failed group-info ask is, by upstream's TYPED error (fix F21): the `w:g2` IQ's own failures are read like any
/// IQ's (a timeout, a dropped socket or a 5xx is the connection's or the server's trouble, so a receipt job waits for
/// it: `send::receipts::connectivity`), everything else (a MEX failure, a malformed request, an internal one, and any
/// variant a pin bump adds: `non_exhaustive`) is internal.
fn group_failure(error: &GroupError) -> SendFailure {
    match error {
        GroupError::Iq(error) => iq_failure(error),
        _ => SendFailure::Internal,
    }
}

/// A group's addressing mode as the send path names it.
pub(crate) fn namespace_of(mode: AddressingMode) -> Namespace {
    match mode {
        AddressingMode::Pn => Namespace::Pn,
        AddressingMode::Lid => Namespace::Lid,
    }
}

/// What a failed `Client::send` is, by upstream's TYPED errors (never their text).
pub(crate) fn send_failure(error: &SendError) -> SendFailure {
    match error {
        SendError::Client(error) => client_failure(error),
        SendError::NotLoggedIn => SendFailure::NotLoggedIn,
        SendError::Iq(error) => iq_failure(error),
        SendError::NoRecipientDevice(_) => SendFailure::NoRecipientDevice,
        SendError::PrimaryDeviceRejected(_) => SendFailure::PrimaryDeviceRejected,
        SendError::MessageRef(_) | SendError::InvalidSecret(_) | SendError::InvalidRequest(_) => {
            SendFailure::InvalidRequest
        }
        SendError::Internal(error) => anyhow_failure(error),
        // `non_exhaustive`: a variant a pin bump adds is internal until it is read here.
        _ => SendFailure::Internal,
    }
}

/// A [`ClientError`] by position.
fn client_failure(error: &ClientError) -> SendFailure {
    match error {
        ClientError::NotConnected => SendFailure::NotConnected,
        ClientError::NotLoggedIn => SendFailure::NotLoggedIn,
        ClientError::Socket(_) | ClientError::EncryptSend(_) => SendFailure::Socket,
        ClientError::Iq(error) => iq_failure(error),
        // `Internal`, and any variant a pin bump adds (`non_exhaustive`).
        _ => SendFailure::Internal,
    }
}

/// An IQ error of a send by position.
fn iq_failure(error: &IqError) -> SendFailure {
    match error {
        IqError::Timeout => SendFailure::IqTimeout,
        IqError::NotConnected | IqError::Disconnected(_) => SendFailure::NotConnected,
        IqError::Socket(_) | IqError::EncryptSend(_) => SendFailure::Socket,
        IqError::ClientState(error) => client_failure(error),
        IqError::ServerError { code, .. } => SendFailure::IqRefused { code: *code },
        _ => SendFailure::Internal,
    }
}

/// An `anyhow` error (receipts, the catch-all of a send): the typed errors it may carry, else internal.
fn anyhow_failure(error: &anyhow::Error) -> SendFailure {
    if let Some(error) = error.downcast_ref::<SendError>() {
        send_failure(error)
    } else if let Some(error) = error.downcast_ref::<ClientError>() {
        client_failure(error)
    } else if let Some(error) = error.downcast_ref::<IqError>() {
        iq_failure(error)
    } else if error
        .downcast_ref::<wacore::send::NoRecipientDeviceError>()
        .is_some()
    {
        SendFailure::NoRecipientDevice
    } else {
        SendFailure::Internal
    }
}

/// What a failed upload is, by upstream's TYPED error chain: the HTTP status the media server answered
/// (`ErrorChainExt::http_status`), else nothing finished. The chain is never formatted: it carries the CDN's answer
/// body and can carry the URL with its `auth`.
pub(crate) fn upload_failure(error: &anyhow::Error) -> UploadFailure {
    let chain: &(dyn std::error::Error + Send + Sync + 'static) = error.as_ref();
    match chain.http_status() {
        Some(status) => UploadFailure::Status(status),
        None => UploadFailure::Incomplete,
    }
}

/// What a failed pair-code request is, by upstream's TYPED errors (never their text).
pub(crate) fn pair_failure(error: &PairError) -> PairCodeFailure {
    match error {
        PairError::PairCode(
            PairCodeError::PhoneNumberRequired
            | PairCodeError::PhoneNumberTooShort
            | PairCodeError::PhoneNumberNotInternational,
        ) => PairCodeFailure::InvalidNumber,
        PairError::PairCode(PairCodeError::CodeAlreadyOutstanding { .. }) => {
            PairCodeFailure::AlreadyOutstanding
        }
        PairError::PairCode(PairCodeError::Cancelled) => PairCodeFailure::Cancelled,
        _ => match error.rejection() {
            Some(rejection) => PairCodeFailure::Refused {
                code: whatsapp_rust::pair_code::PairCodeRejection::code(&rejection),
                backoff: error.backoff(),
            },
            None => PairCodeFailure::NoAnswer,
        },
    }
}

#[cfg(test)]
mod tests;
