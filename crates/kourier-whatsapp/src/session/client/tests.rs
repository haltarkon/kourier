//! What the real session decides from upstream's TYPED values and from the seams it watches (W2,
//! chunk C4b): a failed connect, a refused pair-code request, and what the event handler does with
//! an event. The client itself runs in `tests/actor_real.rs`, in an offline child.

use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use kourier_backend_api::SystemClock;
use kourier_model::ConvKey;
use tokio::sync::mpsc::UnboundedReceiver;
use wacore::request::IqError as CoreIqError;
use wacore::types::events::Connected;
use whatsapp_rust::request::IqError;

use super::*;

fn probe() -> (Probe, UnboundedReceiver<WaSignal>) {
    let (tap, signals) = Tap::channel(None);
    (Probe { tap }, signals)
}

fn told(signals: &mut UnboundedReceiver<WaSignal>) -> Vec<WaSignal> {
    std::iter::from_fn(|| signals.try_recv().ok()).collect()
}

/// A history capture over a stage of its own beside `dir`'s `backend.db` (nothing is made until a chunk comes).
fn capture_in(dir: &std::path::Path) -> crate::history::capture::Capture {
    let stage = Arc::new(crate::history::stage::LazyStage::new(
        &dir.join("backend.db"),
        &kourier_db::DbKey::from([7u8; 32]),
    ));
    crate::history::capture::Capture::new(stage, Arc::new(tokio::sync::Notify::new()))
}

/// The receive path over a store of its own, for a handler under test.
struct Rig {
    receive: Receive,
    owner: crate::store::StoreOwner,
    _dir: tempfile::TempDir,
}

impl Rig {
    fn new() -> Self {
        use crate::stage2::{Stage2, Timing};
        let dir = tempfile::tempdir().expect("temp dir");
        let owner = crate::store::StoreOwner::open(
            &dir.path().join("backend.db"),
            &kourier_db::DbKey::from([7u8; 32]),
            "wa-handler",
        )
        .expect("open");
        let store = owner.store();
        let clock: Arc<dyn kourier_backend_api::Clock> = Arc::new(SystemClock);
        let wake = Arc::new(tokio::sync::Notify::new());
        let receive = Receive {
            inbox: crate::inbox::Inbox::new(store.clone(), clock.clone()),
            writer: crate::events::EventWriter::new(store.clone(), clock.clone(), wake.clone()),
            stage2: Arc::new(Stage2::new(store, clock, wake, Timing::default())),
            capture: capture_in(dir.path()),
            acks: Arc::default(),
        };
        Self {
            receive,
            owner,
            _dir: dir,
        }
    }

    fn forwarder(&self, tap: Tap, core: Arc<Core>) -> Forwarder {
        Forwarder {
            activity: self.owner.store().activity().clone(),
            tap,
            clock: Arc::new(SystemClock),
            core,
            receive: self.receive.clone(),
        }
    }

    /// The events of every `core_outbox` row, in order.
    async fn outbox(&self) -> Vec<kourier_backend_api::BackendEvent> {
        self.owner
            .store()
            .read(|conn| {
                let rows = kourier_db::outbox::peek(conn, 1000)
                    .map_err(|_| crate::error::database("peek"))?;
                Ok(rows
                    .iter()
                    .flat_map(|row| {
                        kourier_backend_api::decode_outbox_row(&row.envelope).expect("decodes")
                    })
                    .collect())
            })
            .await
            .expect("read the outbox")
    }

    /// The origins of the inbox rows, in arrival order.
    async fn inbox(&self) -> Vec<String> {
        self.owner
            .store()
            .read(|conn| {
                let mut stmt = conn
                    .prepare("SELECT origin FROM inbox ORDER BY arrival")
                    .map_err(crate::error::sql("read inbox"))?;
                let rows = stmt
                    .query_map([], |r| r.get::<_, String>(0))
                    .map_err(crate::error::sql("read inbox"))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(crate::error::sql("read inbox"))?;
                Ok(rows)
            })
            .await
            .expect("read the inbox")
    }

    fn close(self) {
        self.owner.close();
    }
}

/// A transport that exists and does nothing.
struct NullTransport;

#[async_trait]
impl Transport for NullTransport {
    async fn send(&self, _data: Bytes) -> Result<(), anyhow::Error> {
        Ok(())
    }
    async fn disconnect(&self) {}
}

/// A factory that opens, refuses, or never answers.
enum Dial {
    Opens,
    Refuses,
    Hangs,
}

#[async_trait]
impl TransportFactory for Dial {
    async fn create_transport(
        &self,
    ) -> Result<(Arc<dyn Transport>, async_channel::Receiver<TransportEvent>), anyhow::Error> {
        match self {
            Self::Opens => {
                let (_tx, rx) = async_channel::unbounded();
                Ok((Arc::new(NullTransport) as Arc<dyn Transport>, rx))
            }
            Self::Refuses => Err(anyhow::anyhow!("refused")),
            Self::Hangs => std::future::pending().await,
        }
    }
}

#[tokio::test]
async fn a_transport_that_cannot_be_opened_or_is_cancelled_says_unreachable_and_one_that_opens_says_nothing()
 {
    let (probe, mut signals) = probe();
    let opens = ProbedTransport {
        inner: Arc::new(Dial::Opens),
        probe: probe.clone(),
    };
    assert!(opens.create_transport().await.is_ok());
    assert!(told(&mut signals).is_empty());

    let refuses = ProbedTransport {
        inner: Arc::new(Dial::Refuses),
        probe: probe.clone(),
    };
    assert!(refuses.create_transport().await.is_err());
    assert_eq!(
        told(&mut signals),
        [WaSignal::WillRetry(RetryWhy::Unreachable)]
    );

    // Upstream's 20 s timeout drops the future of a dial that hangs: the drop guard reports it.
    let hangs = ProbedTransport {
        inner: Arc::new(Dial::Hangs),
        probe,
    };
    let cancelled = tokio::time::timeout(Duration::from_millis(50), hangs.create_transport()).await;
    assert!(cancelled.is_err());
    assert_eq!(
        told(&mut signals),
        [WaSignal::WillRetry(RetryWhy::Unreachable)]
    );
}

/// An HTTP client that answers with a status, or fails.
struct Answers(Mutex<Option<anyhow::Result<u16>>>);

impl Answers {
    fn status(code: u16) -> Arc<dyn HttpClient> {
        Arc::new(Self(Mutex::new(Some(Ok(code)))))
    }
    fn fails() -> Arc<dyn HttpClient> {
        Arc::new(Self(Mutex::new(Some(Err(anyhow::anyhow!("no route"))))))
    }
}

#[async_trait]
impl HttpClient for Answers {
    async fn execute(&self, _request: HttpRequest) -> anyhow::Result<HttpResponse> {
        let next = self.0.lock().expect("lock").take().expect("one request");
        next.map(|status| HttpResponse::new(status, Vec::new()))
    }

    fn supports_streaming(&self) -> bool {
        true
    }
}

async fn fetch(url: &str, client: Arc<dyn HttpClient>) -> Vec<WaSignal> {
    let (probe, mut signals) = probe();
    let probed = ProbedHttp {
        inner: client,
        probe,
    };
    let _ = probed.execute(HttpRequest::get(url)).await;
    told(&mut signals)
}

const VERSION_FILE: &str = "https://web.whatsapp.com/sw.js";

#[tokio::test]
async fn the_version_file_decides_outdated_or_unreachable_and_nothing_else_is_watched() {
    // It answered: fine.
    assert!(fetch(VERSION_FILE, Answers::status(200)).await.is_empty());
    // A 4xx answer: updating is the way on (decision L5).
    for status in [400, 403, 404, 410, 499] {
        assert_eq!(
            fetch(VERSION_FILE, Answers::status(status)).await,
            [WaSignal::Outdated],
            "{status}"
        );
    }
    // A redirect, a server error, or no answer at all: unreachable for now, not the person's to fix.
    for status in [301, 500, 503] {
        assert_eq!(
            fetch(VERSION_FILE, Answers::status(status)).await,
            [WaSignal::WillRetry(RetryWhy::Unreachable)],
            "{status}"
        );
    }
    assert_eq!(
        fetch(VERSION_FILE, Answers::fails()).await,
        [WaSignal::WillRetry(RetryWhy::Unreachable)]
    );
    // A media request that fails is not about the connection.
    let media = "https://mmg.whatsapp.net/v/t62/file";
    assert!(fetch(media, Answers::status(404)).await.is_empty());
    assert!(fetch(media, Answers::fails()).await.is_empty());
    // A look-alike host is not the version file's.
    assert!(
        fetch(
            "https://web.whatsapp.com.example.org/sw.js",
            Answers::fails()
        )
        .await
        .is_empty()
    );
}

#[test]
fn the_wrapped_http_client_keeps_the_capabilities_of_the_one_it_wraps() {
    let (probe, _signals) = probe();
    let probed = ProbedHttp {
        inner: Answers::status(200),
        probe,
    };
    assert!(
        probed.supports_streaming(),
        "a wrapper that drops streaming would change how media downloads"
    );
    assert!(!probed.supports_upload_streaming());
}

fn rejected(code: u16, text: &str, backoff: Option<u32>) -> PairError {
    PairError::RequestFailed(IqError::Unclassified(CoreIqError::ServerError {
        code,
        text: text.to_owned(),
        error_type: None,
        backoff,
    }))
}

#[test]
fn a_pair_code_failure_is_told_by_upstreams_typed_errors() {
    use PairCodeFailure::*;
    for local in [
        PairCodeError::PhoneNumberRequired,
        PairCodeError::PhoneNumberTooShort,
        PairCodeError::PhoneNumberNotInternational,
    ] {
        assert_eq!(pair_failure(&PairError::PairCode(local)), InvalidNumber);
    }
    assert_eq!(
        pair_failure(&PairError::PairCode(
            PairCodeError::CodeAlreadyOutstanding {
                remaining: Duration::from_secs(90)
            }
        )),
        AlreadyOutstanding
    );
    assert_eq!(
        pair_failure(&PairError::PairCode(PairCodeError::Cancelled)),
        Cancelled
    );
    // The server's answers keep their wire code and the delay it named.
    assert_eq!(
        pair_failure(&rejected(429, "rate-overlimit", Some(30))),
        Refused {
            code: 429,
            backoff: Some(Duration::from_secs(30))
        }
    );
    // An answer without a text is judged by its code alone (upstream's rule).
    assert_eq!(
        pair_failure(&rejected(452, "", None)),
        Refused {
            code: 452,
            backoff: None
        }
    );
    // A code with a text that contradicts it is no typed refusal at all.
    assert_eq!(
        pair_failure(&rejected(429, "something-else", None)),
        NoAnswer
    );
    // No answer: not connected, or a request that ran out of time.
    assert_eq!(
        pair_failure(&PairError::RequestFailed(IqError::Timeout)),
        NoAnswer
    );
    assert_eq!(
        pair_failure(&PairError::RequestFailed(IqError::NotConnected)),
        NoAnswer
    );
}

#[test]
fn the_handler_translates_what_it_hears_and_drops_what_w2_does_not_act_on() {
    let (tap, mut signals) = Tap::channel(None);
    let rig = Rig::new();
    let forwarder = rig.forwarder(tap, Arc::new(Core::default()));
    forwarder.handle_event(Arc::new(Event::Connected(Connected::builder().build())));
    assert_eq!(told(&mut signals), [WaSignal::Connected]);
    forwarder.handle_event(Arc::new(Event::StreamError(
        wacore::types::events::StreamError::builder()
            .code("429".to_owned())
            .build(),
    )));
    assert_eq!(
        told(&mut signals),
        [WaSignal::WillRetry(RetryWhy::RateLimited)]
    );
    forwarder.handle_event(Arc::new(Event::StreamError(
        wacore::types::events::StreamError::builder()
            .code("503".to_owned())
            .build(),
    )));
    assert!(told(&mut signals).is_empty());
    drop(forwarder);
    rig.close();
}

/// T-9, second half: a frame the client sent reaches the statistics watch through the handler, a
/// stanza of the statistics namespace is counted, and neither becomes a signal for the actor. (That a
/// real client's frames arrive here at all - the lease - needs a server: chunk C5.)
#[test]
fn the_handler_gives_every_sent_frame_to_the_statistics_watch_and_to_nobody_else() {
    use wacore::types::events::SentFrame;
    use wacore_binary::builder::NodeBuilder;
    use wacore_binary::marshal::marshal;

    let (tap, mut signals) = Tap::channel(None);
    let core = Arc::new(Core::default());
    let rig = Rig::new();
    let forwarder = rig.forwarder(tap, core.clone());
    let sent = |namespace: &str| {
        let node = NodeBuilder::new("iq")
            .attr("xmlns", namespace.to_owned())
            .build();
        Arc::new(Event::SentFrame(
            SentFrame::builder()
                .plaintext(Bytes::from(marshal(&node).expect("marshals")))
                .build(),
        ))
    };
    forwarder.handle_event(sent("usync"));
    assert_eq!((core.wam.seen(), core.wam.sent()), (1, 0));
    forwarder.handle_event(sent(&crate::wam::Look::namespace_for_tests()));
    assert_eq!((core.wam.seen(), core.wam.sent()), (2, 1));
    assert!(told(&mut signals).is_empty(), "a sent frame is no signal");
    drop(forwarder);
    rig.close();
}

/// Rule (b)'s quiet look counts app-state traffic only (W6a1, D4): the mutations the phone's sync dispatches and the
/// report of a sync that left collections unsynced. A connection event, a stream error, a receipt, typing, presence
/// and the sent frames are no traffic: in a busy group typing alone would hold the look at its bound for ever.
#[tokio::test]
async fn only_app_state_mutations_count_as_traffic() {
    use wacore::types::events::{AppStateSyncFailed, ChatPresenceUpdate, SentFrame};
    use wacore::types::presence::ChatActivity;
    use wacore_binary::builder::NodeBuilder;
    use wacore_binary::marshal::marshal;

    let (tap, _signals) = Tap::channel(None);
    let rig = Rig::new();
    let activity = rig.owner.store().activity().clone();
    let forwarder = rig.forwarder(tap, Arc::new(Core::default()));
    forwarder.handle_event(Arc::new(Event::Connected(Connected::builder().build())));
    forwarder.handle_event(Arc::new(Event::StreamError(
        wacore::types::events::StreamError::builder()
            .code("503".to_owned())
            .build(),
    )));
    forwarder.handle_event(delivery_receipt("3EB0AA"));
    forwarder.handle_event(Arc::new(Event::ChatPresence(
        ChatPresenceUpdate::builder()
            .source(wacore::types::message::MessageSource {
                chat: "31000000001@s.whatsapp.net".parse().expect("jid"),
                sender: "31000000001@s.whatsapp.net".parse().expect("jid"),
                ..wacore::types::message::MessageSource::default()
            })
            .state(ChatActivity::Typing)
            .build(),
    )));
    let node = NodeBuilder::new("iq")
        .attr("xmlns", "usync".to_owned())
        .build();
    forwarder.handle_event(Arc::new(Event::SentFrame(
        SentFrame::builder()
            .plaintext(Bytes::from(marshal(&node).expect("marshals")))
            .build(),
    )));
    assert_eq!(
        activity.sample().events,
        0,
        "no connection event, receipt, typing or sent frame is app-state traffic"
    );
    forwarder.handle_event(archive_update("77700481537209@lid", true));
    assert_eq!(activity.sample().events, 1, "a mutation is traffic");
    forwarder.handle_event(Arc::new(Event::AppStateSyncFailed(
        AppStateSyncFailed::builder()
            .fatal(Vec::new())
            .retryable(vec!["regular".to_owned()])
            .skipped(Vec::new())
            .connected(true)
            .build(),
    )));
    assert_eq!(
        activity.sample().events,
        2,
        "so is the report of a sync that left a collection unsynced"
    );
    drop(forwarder);
    rig.close();
}

/// An archive mutation of the phone, as upstream hands it to the handler.
fn archive_update(chat: &str, archived: bool) -> Arc<Event> {
    use waproto::whatsapp::sync_action_value::ArchiveChatAction;
    let mut action = ArchiveChatAction::default();
    action.archived = Some(archived);
    Arc::new(Event::ArchiveUpdate(
        wacore::types::events::ArchiveUpdate::builder()
            .jid(chat.parse().expect("jid"))
            .timestamp(wacore::chrono::DateTime::from_timestamp(1_760_000_999, 0).expect("time"))
            .action_timestamp(
                wacore::chrono::DateTime::from_timestamp(1_760_000_111, 0).expect("time"),
            )
            .action(Box::new(action))
            .from_full_sync(false)
            .build(),
    ))
}

/// The core knows `chat`: its features were stated, so a value of the phone's app state for it reaches the core.
async fn core_knows(rig: &Rig, chat: &ConvKey) {
    let chat = chat.clone();
    rig.owner
        .store()
        .write("known", move |tx| {
            crate::send::started::mark_stated(tx, &chat, crate::features::VERSION, true)
        })
        .await
        .expect("mark stated");
}

fn lid_chat(user: &str) -> ConvKey {
    ConvKey::Direct(kourier_model::UserId::from_parts("lid", user).expect("a user"))
}

/// D7: a sent message's payload (kept for retry receipts) lives two hours. Kourier states the value
/// itself; upstream's default is the same today, and a pin bump that moves it shows here.
#[test]
fn the_payload_of_a_sent_message_is_kept_two_hours() {
    assert_eq!(super::super::SENT_MESSAGE_TTL_SECS, 7200);
    assert_eq!(super::super::cache_config().sent_message_ttl_secs, 7200);
    assert_eq!(
        whatsapp_rust::CacheConfig::default().sent_message_ttl_secs,
        7200,
        "upstream's default moved: decide whether Kourier follows it"
    );
}

/// Fix F6 (decision D7): the secret of a message is kept as long as the message, so `Full`; upstream's
/// default (`Managed`) prunes after 30 and 90 days.
#[test]
fn the_secret_of_a_message_is_kept_as_long_as_the_message() {
    use wacore::msg_secret::MsgSecretPolicy;
    assert_eq!(super::super::MSG_SECRET_POLICY, MsgSecretPolicy::Full);
    assert_eq!(
        super::super::cache_config().msg_secret_policy,
        MsgSecretPolicy::Full,
        "the policy the client is built with"
    );
}

/// Critic H7 / decision L6: a pairing the gate refuses makes the handler start the logout at once on
/// the client's runtime - not after a hop to the actor - and the actor's own `logout` later shares
/// that one answer. (With no client behind the core the answer is `NotReached`; what a real logout
/// reaches needs a server.)
#[tokio::test]
async fn a_refused_pairing_starts_the_logout_in_the_handler_and_the_actors_logout_shares_it() {
    use crate::link::{Gate, Rules};
    use kourier_backend_api::TakenIdentity;
    use kourier_model::UserId;
    use wacore::types::events::PairSuccess;
    use wacore_binary::Jid;

    let gate = Arc::new(Gate::new(Rules::new(
        vec![TakenIdentity {
            user: UserId::from_parts("pn", "15555550100").expect("id"),
            logged_out: false,
        }],
        None,
        None,
    )));
    let (tap, mut signals) = Tap::channel(Some(gate));
    let core = Arc::new(Core::default());
    core.handle.set(Handle::current()).expect("set once");
    let rig = Rig::new();
    let forwarder = rig.forwarder(tap, core.clone());
    let mut id = Jid::pn("15555550100");
    id.device = 7;
    forwarder.handle_event(Arc::new(Event::PairSuccess(
        PairSuccess::builder()
            .id(id)
            .lid(Jid::lid("100000000000001"))
            .business_name(String::new())
            .platform("android".to_owned())
            .build(),
    )));
    // The signal is handed on (the machine ends the login) ...
    assert!(matches!(
        told(&mut signals).as_slice(),
        [WaSignal::PairSuccess(_)]
    ));
    // ... and the logout was started by the handler: its answer appears with no actor involved.
    let started = std::time::Instant::now();
    while core.logout.get().is_none() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the handler started no logout"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(core.logout.get(), Some(&DeviceLogout::NotReached));
    // The actor's call gets that very answer (a second logout would find no client either way,
    // so the shared cell is what is checked: it is set once and never recomputed).
    assert_eq!(
        core.logout_once(Duration::ZERO).await,
        DeviceLogout::NotReached
    );
    drop(forwarder);
    rig.close();
}

/// A client that becomes reachable after `after`, or never.
struct Reaches {
    after: Option<Duration>,
}

impl Reach for Reaches {
    async fn until_reachable(&self) -> bool {
        match self.after {
            Some(after) => {
                tokio::time::sleep(after).await;
                true
            }
            None => std::future::pending().await,
        }
    }
}

/// Review F2: the logout of a refused pairing goes the moment the client can reach the server (a
/// socket, `<success>` seen, a reader), which the client says by `wait_until_reachable` and not by
/// its full session readiness (that comes after the post-login work), and it is bounded: when the
/// socket never comes, the wait ends at the bound with `false` and the logout is tried anyway.
#[tokio::test]
async fn the_logout_waits_for_a_reachable_socket_and_no_longer_than_its_bound() {
    let started = std::time::Instant::now();
    let reachable = Reaches {
        after: Some(Duration::from_millis(80)),
    };
    assert!(reachable_within(&reachable, Duration::from_secs(30)).await);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "it went when the socket was there, not at the bound"
    );

    // Never reachable: the bound ends the wait (an outer limit keeps a missing bound from hanging).
    let started = std::time::Instant::now();
    let never = Reaches { after: None };
    let answer = tokio::time::timeout(
        Duration::from_secs(20),
        reachable_within(&never, Duration::from_millis(200)),
    )
    .await
    .expect("the bound ended the wait");
    assert!(!answer);
    assert!(started.elapsed() >= Duration::from_millis(200));

    // A socket that comes after the bound is too late.
    let late = Reaches {
        after: Some(Duration::from_millis(600)),
    };
    assert!(!reachable_within(&late, Duration::from_millis(100)).await);
}

/// Set in the child of [`the_built_client_tells_the_phone_kourier_and_not_upstreams_default`].
const OFFLINE_CHILD: &str = "WA_CLIENT_TEST_OFFLINE_CHILD";

/// Critic M6: what the phone is told is set by the BUILD of the client (not by a helper nobody
/// calls), from the host's name and this crate's version, and upstream's default identity (`os:
/// "rust"`, `UNKNOWN`) never survives it. The store keeps no device props - they are applied at every
/// build - so this reads the built client's own snapshot. A unit test that BUILDS a client runs in a
/// child under `unshare -rn` like every other (review F4): the parent starts this binary again for
/// [`the_built_client_runs_offline`]; where user namespaces are unavailable it is skipped, never run
/// on the real network.
#[test]
fn the_built_client_tells_the_phone_kourier_and_not_upstreams_default() {
    if !std::process::Command::new("unshare")
        .args(["-rn", "true"])
        .status()
        .is_ok_and(|status| status.success())
    {
        // `cargo xtask ci` sets KOURIER_REQUIRE_UNSHARE=1: a skip there would be a green run with
        // nothing run (tests/support/offline.rs has the same rule for the integration tests).
        assert_ne!(
            std::env::var("KOURIER_REQUIRE_UNSHARE").as_deref(),
            Ok("1"),
            "`unshare -rn` is not available and KOURIER_REQUIRE_UNSHARE=1"
        );
        eprintln!("SKIPPED: `unshare -rn` is not available; no client is built outside it");
        return;
    }
    let out = std::process::Command::new("unshare")
        .arg("-rn")
        .arg(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "session::client::tests::the_built_client_runs_offline",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(OFFLINE_CHILD, "1")
        .output()
        .expect("spawn the child under unshare");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("test result: ok. 1 passed;"),
        "the child failed ({:?}):\n{stdout}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The child: builds the client (never runs it) over a transport that refuses and an HTTP client that
/// fails, and reads what it would tell the phone.
#[tokio::test]
async fn the_built_client_runs_offline() {
    if std::env::var_os(OFFLINE_CHILD).is_none() {
        return;
    }
    use waproto::whatsapp::device_props::PlatformType;

    let dir = tempfile::tempdir().expect("temp dir");
    let owner = crate::store::StoreOwner::open(
        &dir.path().join("backend.db"),
        &kourier_db::DbKey::from([7u8; 32]),
        "wa-test",
    )
    .expect("open");
    let (probe, _signals) = probe();
    let (tap, _tap_signals) = Tap::channel(None);
    let capture_tap = tap.clone();
    let clock: Arc<dyn kourier_backend_api::Clock> = Arc::new(SystemClock);
    let inbox = crate::inbox::Inbox::new(owner.store(), clock.clone());
    let bot = super::super::build_bot(
        owner.store(),
        Arc::new(ProbedTransport {
            inner: Arc::new(Dial::Refuses),
            probe: probe.clone(),
        }),
        Answers::fails(),
        Forwarder {
            activity: owner.store().activity().clone(),
            tap,
            clock,
            core: Arc::new(Core::default()),
            receive: Receive {
                inbox: inbox.clone(),
                writer: crate::events::EventWriter::new(
                    owner.store(),
                    Arc::new(SystemClock),
                    Arc::new(tokio::sync::Notify::new()),
                ),
                stage2: Arc::new(crate::stage2::Stage2::new(
                    owner.store(),
                    Arc::new(SystemClock),
                    Arc::new(tokio::sync::Notify::new()),
                    crate::stage2::Timing::default(),
                )),
                capture: capture_in(dir.path()),
                acks: Arc::default(),
            },
        },
        super::super::Hooks {
            durability: Hook::new(inbox, Arc::new(())),
            capture: crate::history::capture::CaptureHook::new(
                capture_in(dir.path()),
                owner.store(),
                capture_tap,
            ),
        },
        None,
        "testhost",
    )
    .await
    .expect("the client builds (it is never run here)");
    let device = bot.client().persistence_manager().get_device_snapshot();
    let os = device.device_props.os.clone().unwrap_or_default();
    assert_eq!(
        os, "Kourier (testhost)",
        "the host the app gave is the label"
    );
    assert_ne!(os, "rust");
    assert_eq!(
        device.device_props.platform_type,
        Some(PlatformType::DESKTOP)
    );
    assert_eq!(device.device_props.require_full_sync, Some(false));
    drop(bot);
    owner.close();
}

// ----- the handler's writes (W3a, chunk C6) -----

fn delivery_receipt(id: &str) -> Arc<Event> {
    use wacore::types::message::MessageSource;
    use wacore::types::presence::ReceiptType;
    Arc::new(Event::Receipt(
        wacore::types::events::Receipt::builder()
            .source(MessageSource {
                chat: "31000000001@s.whatsapp.net".parse().expect("jid"),
                sender: "31000000001@s.whatsapp.net".parse().expect("jid"),
                ..MessageSource::default()
            })
            .message_ids(vec![id.into()])
            .timestamp(wacore::chrono::DateTime::from_timestamp(1_700_000_005, 0).expect("time"))
            .r#type(ReceiptType::Delivered)
            .offline(false)
            .build(),
    ))
}

fn messages(hook_committed: bool, chat: &str, id: &str) -> Arc<Event> {
    use wacore::types::events::{BatchOrigin, InboundMessage, MessageBatch};
    use wacore::types::message::{MessageInfo, MessageSource};
    let mut message = waproto::whatsapp::Message::default();
    message.conversation = Some("synthetic text".to_owned());
    let item = InboundMessage::builder()
        .message(Arc::new(message))
        .info(Arc::new(MessageInfo {
            id: id.into(),
            source: MessageSource {
                chat: chat.parse().expect("jid"),
                sender: chat.parse().expect("jid"),
                ..MessageSource::default()
            },
            ..MessageInfo::default()
        }))
        .build();
    Arc::new(Event::Messages(
        MessageBatch::builder()
            .messages(Arc::from(vec![item]))
            .origin(BatchOrigin::Live)
            .hook_committed(hook_committed)
            .build(),
    ))
}

/// Parks the store's writer until the returned sender is used.
async fn park(store: &crate::store::KourierStore) -> std::sync::mpsc::Sender<()> {
    let (release, wait) = std::sync::mpsc::channel::<()>();
    let (started_tx, started) = std::sync::mpsc::channel::<()>();
    let store = store.clone();
    tokio::spawn(async move {
        let _ = store
            .raw(kourier_db::Priority::High, move |_| {
                let _ = started_tx.send(());
                let _ = wait.recv();
                Ok(())
            })
            .await;
    });
    for _ in 0..500 {
        if started.try_recv().is_ok() {
            return release;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the writer did not park");
}

/// Rule (a), through the handler (decision D1): the handler does not return before the receipt's row
/// is committed - upstream acknowledges the stanza right after it returns - and the receipt is no
/// signal for the actor.
#[tokio::test]
async fn the_handler_returns_only_after_the_receipts_row_is_committed() {
    let rig = Rig::new();
    let (tap, mut signals) = Tap::channel(None);
    let forwarder = Arc::new(rig.forwarder(tap, Arc::new(Core::default())));
    let release = park(&rig.owner.store()).await;
    let (done_tx, done) = std::sync::mpsc::channel();
    let handler = {
        let forwarder = forwarder.clone();
        std::thread::spawn(move || {
            forwarder.handle_event(delivery_receipt("3EB0AA"));
            let _ = done_tx.send(());
        })
    };
    // The writer is parked: the handler is blocked, and nothing is written.
    assert!(
        done.recv_timeout(Duration::from_millis(300)).is_err(),
        "the handler returned before the commit"
    );
    release.send(()).expect("release");
    done.recv_timeout(Duration::from_secs(10))
        .expect("the handler returns after the commit");
    handler.join().expect("join");
    let events = rig.outbox().await;
    assert!(
        matches!(
            events.as_slice(),
            [kourier_backend_api::BackendEvent::Receipts { targets, .. }]
                if &*targets[0].local == "3EB0AA"
        ),
        "{events:?}"
    );
    assert!(told(&mut signals).is_empty(), "a receipt is no signal");
    drop(forwarder);
    rig.close();
}

/// Decision D5: a write that fails ends the connection. Here, with no client behind the core, what is
/// seen is the actor being told (`WriteFailed`), nothing else being handed on, and nothing being
/// written; that the ack is withheld is shown over the Wire (a later chunk).
#[tokio::test]
#[allow(
    clippy::await_holding_lock,
    reason = "the seam is process-wide: a test holds fault::serial() for its whole run"
)]
async fn a_failed_write_tells_the_actor_and_hands_on_nothing_else() {
    let _serial = crate::fault::serial();
    let rig = Rig::new();
    let (tap, mut signals) = Tap::channel(None);
    let forwarder = rig.forwarder(tap, Arc::new(Core::default()));
    crate::fault::arm(crate::fault::EVENT_WRITE, 1);
    forwarder.handle_event(delivery_receipt("3EB0AA"));
    crate::fault::disarm();
    assert_eq!(told(&mut signals), [WaSignal::WriteFailed]);
    assert!(rig.outbox().await.is_empty(), "a failed zone wrote");
    // The redelivery goes through.
    forwarder.handle_event(delivery_receipt("3EB0AA"));
    assert!(told(&mut signals).is_empty());
    assert_eq!(rig.outbox().await.len(), 1);
    drop(forwarder);
    rig.close();
}

// ----- the phone's app state (W6a1, chunk A4) -----

const PHONE_CHAT: &str = "77700481537209";

/// A mutation of the phone's app state is a durable event (rule (a), D6): upstream's cursor committed BEFORE it
/// dispatched, so a mutation lost here is lost for good. The handler does not return before the zone that holds the
/// mirror and the core event committed, the mutation is no signal for the actor, and the same mutation again (a replay)
/// writes nothing more.
#[tokio::test]
async fn an_app_state_mutation_returns_only_after_its_zone_committed() {
    let rig = Rig::new();
    let chat = lid_chat(PHONE_CHAT);
    core_knows(&rig, &chat).await;
    let (tap, mut signals) = Tap::channel(None);
    let forwarder = Arc::new(rig.forwarder(tap, Arc::new(Core::default())));
    let release = park(&rig.owner.store()).await;
    let (done_tx, done) = std::sync::mpsc::channel();
    let handler = {
        let forwarder = forwarder.clone();
        std::thread::spawn(move || {
            forwarder.handle_event(archive_update("77700481537209@lid", true));
            let _ = done_tx.send(());
        })
    };
    assert!(
        done.recv_timeout(Duration::from_millis(300)).is_err(),
        "the handler returned before the commit"
    );
    release.send(()).expect("release");
    done.recv_timeout(Duration::from_secs(10))
        .expect("the handler returns after the commit");
    handler.join().expect("join");
    let mut patch = kourier_model::ConversationPatch::new(chat);
    patch.archived = Some(true);
    assert_eq!(
        rig.outbox().await,
        [kourier_backend_api::BackendEvent::ConversationUpsert(patch)]
    );
    assert!(told(&mut signals).is_empty(), "a mutation is no signal");
    forwarder.handle_event(archive_update("77700481537209@lid", true));
    assert_eq!(
        rig.outbox().await.len(),
        1,
        "a replay of the same mutation tells the core nothing"
    );
    drop(forwarder);
    rig.close();
}

/// A zone that fails ends the connection AND the collection's flag stands for the rest of the process: a quiet quit
/// after the new client ran would otherwise clear a flag that vouches for an event that was lost (critic B1 point 3).
/// Nothing is redelivered: upstream's cursor was committed before the dispatch, so the mutation that failed is lost for
/// good and the flag (the resync it brings at the next start) is the repair. The next write goes through.
#[tokio::test]
#[allow(
    clippy::await_holding_lock,
    reason = "the seam is process-wide: a test holds fault::serial() for its whole run"
)]
async fn a_failed_app_state_zone_ends_the_connection_and_keeps_the_flag_at_a_quiet_quit() {
    use crate::appstate::{Outcome, Quiet, Why, stop_quietly};
    use crate::session::fake::FakeSession;
    use wacore::appstate::hash::HashState;
    use wacore::store::traits::AppSyncStore;

    let _serial = crate::fault::serial();
    let rig = Rig::new();
    let store = rig.owner.store();
    // The version moved (upstream's cursor committed): the collection is dirty.
    store
        .set_version(
            "regular_low",
            HashState {
                version: 3,
                ..HashState::default()
            },
        )
        .await
        .expect("version");
    let (tap, mut signals) = Tap::channel(None);
    let forwarder = rig.forwarder(tap, Arc::new(Core::default()));
    crate::fault::arm(crate::fault::EVENT_WRITE, 1);
    forwarder.handle_event(archive_update("77700481537209@lid", true));
    crate::fault::disarm();
    assert_eq!(told(&mut signals), [WaSignal::WriteFailed]);
    assert_eq!(store.activity().lost(), ["regular_low"]);
    assert!(rig.outbox().await.is_empty(), "a failed zone wrote");

    // The redelivery goes through; the mark stays.
    forwarder.handle_event(archive_update("77700481537209@lid", true));
    assert!(told(&mut signals).is_empty());
    assert_eq!(store.activity().lost(), ["regular_low"]);

    let session = FakeSession::new();
    let (_, outcome) =
        stop_quietly(&store, &session, Quiet::STANDARD, false, &["regular_low"]).await;
    assert_eq!(outcome, Outcome::Kept(Why::Lost));
    assert_eq!(
        store.dirty_collections().await.expect("dirty"),
        [("regular_low".to_owned(), 3)],
        "the flag of the collection that lost an event stands"
    );
    drop(forwarder);
    rig.close();
}

/// A record of the phone's call log, as upstream hands it to the handler: an incoming 1:1 call the peer placed, missed.
fn call_log_sync(creator: &str, id: &str) -> Arc<Event> {
    call_log_sync_at(creator, id, 1_760_000_100)
}

/// [`call_log_sync`] for a call that started at second `start`.
fn call_log_sync_at(creator: &str, id: &str, start: i64) -> Arc<Event> {
    use waproto::whatsapp::call_log_record::CallResult;
    let mut record = waproto::whatsapp::CallLogRecord::default();
    record.call_result = Some(CallResult::MISSED);
    record.start_time = Some(start);
    record.is_video = Some(false);
    Arc::new(Event::CallLogSync(
        wacore::types::events::CallLogSync::builder()
            .call_creator_jid(creator.parse().expect("jid"))
            .call_id(id.to_owned())
            .from_me(false)
            .timestamp(wacore::chrono::DateTime::from_timestamp(1_760_000_999, 0).expect("time"))
            .record(Box::new(record))
            .from_full_sync(false)
            .build(),
    ))
}

/// A record of the phone's call log is a durable event too (rule (a), D6): the collection's flag is cleared once its
/// events are durable, so the handler does not return before the zone that holds the chat's statement and the quiet line
/// committed; the record is no signal for the actor.
#[tokio::test]
async fn a_call_log_record_returns_only_after_its_zone_committed() {
    let rig = Rig::new();
    let (tap, mut signals) = Tap::channel(None);
    let forwarder = Arc::new(rig.forwarder(tap, Arc::new(Core::default())));
    let release = park(&rig.owner.store()).await;
    let (done_tx, done) = std::sync::mpsc::channel();
    let handler = {
        let forwarder = forwarder.clone();
        std::thread::spawn(move || {
            forwarder.handle_event(call_log_sync("77700481537209@lid", "CALL-LOG-61820001"));
            let _ = done_tx.send(());
        })
    };
    assert!(
        done.recv_timeout(Duration::from_millis(300)).is_err(),
        "the handler returned before the commit"
    );
    release.send(()).expect("release");
    done.recv_timeout(Duration::from_secs(10))
        .expect("the handler returns after the commit");
    handler.join().expect("join");
    let events = rig.outbox().await;
    assert!(
        matches!(
            events.as_slice(),
            [
                kourier_backend_api::BackendEvent::ConversationUpsert(_),
                kourier_backend_api::BackendEvent::HistoryBatch { .. }
            ]
        ),
        "the chat is stated, then the line follows: {}",
        events.len()
    );
    assert!(
        told(&mut signals).is_empty(),
        "a call-log record is no signal"
    );
    drop(forwarder);
    rig.close();
}

/// H6 (DUR-5): a line of the phone's call log NEVER brings an archived chat back, in either form: the history row of a
/// chat with no known cut, and the live row of a call after the chat's cut (INTEROP-4). The phone's setting says a new
/// message unarchives, the chat is archived and stated, and the call is newer than the archive: not one outbox row holds
/// `archived: false` and the mirror still holds the archive. Mutant: the call-log arm of the writer routes its events
/// through `zone::prepare` (the live line then counts as a message that unarchives).
#[tokio::test]
async fn a_call_log_line_never_unarchives_a_chat_in_either_form() {
    use crate::store::chat_state::{self, Field};
    use kourier_backend_api::BackendEvent;

    let rig = Rig::new();
    let store = rig.owner.store();
    let chat = lid_chat(PHONE_CHAT);
    core_knows(&rig, &chat).await;
    store
        .write("test: the phone unarchives", |tx| {
            chat_state::set_account_flag(tx, chat_state::UNARCHIVE_CHATS, true).map(drop)
        })
        .await
        .expect("setting");
    let (tap, mut signals) = Tap::channel(None);
    let forwarder = rig.forwarder(tap, Arc::new(Core::default()));
    // Archived on the phone at second 1_760_000_111; the calls below are later.
    forwarder.handle_event(archive_update("77700481537209@lid", true));
    assert_eq!(rig.outbox().await.len(), 1, "the archive reached the core");

    // The history form: no cut is known.
    forwarder.handle_event(call_log_sync_at(
        "77700481537209@lid",
        "CALL-LOG-61820003",
        1_760_000_500,
    ));
    let events = rig.outbox().await;
    assert!(
        matches!(events.last(), Some(BackendEvent::HistoryBatch { .. })),
        "the first line is a history row"
    );

    // The live form: the chat was cleared on the phone before the call.
    let cleared = chat.clone();
    store
        .write("test: cut", move |tx| {
            chat_state::raise_cut(tx, &cleared, 1_760_000_400_000)
        })
        .await
        .expect("cut");
    forwarder.handle_event(call_log_sync_at(
        "77700481537209@lid",
        "CALL-LOG-61820004",
        1_760_000_600,
    ));
    let events = rig.outbox().await;
    let Some(BackendEvent::Message(live)) = events.last() else {
        panic!("a call after the cut is a live row");
    };
    assert!(live.silent && live.read, "quiet, whatever its form");

    let unarchives: Vec<_> = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                BackendEvent::ConversationUpsert(patch) if patch.archived == Some(false)
            )
        })
        .collect();
    assert!(
        unarchives.is_empty(),
        "a call-log line brought the chat back out of the archive"
    );
    let held = {
        let chat = chat.clone();
        store
            .read(move |conn| chat_state::get(conn, &chat, Field::Archived))
            .await
            .expect("get")
    };
    assert_eq!(
        held.map(|row| row.value),
        Some(1),
        "the mirror still holds the archive"
    );
    assert!(
        told(&mut signals).is_empty(),
        "a call-log record is no signal"
    );
    drop(forwarder);
    rig.close();
}

/// A call-log zone that fails ends the connection and the `regular` collection's flag stands for the rest of the
/// process, like every app-state event (critic B1 point 3). Mutant: the handler does not mark the collection lost.
#[tokio::test]
#[allow(
    clippy::await_holding_lock,
    reason = "the seam is process-wide: a test holds fault::serial() for its whole run"
)]
async fn a_failed_call_log_zone_ends_the_connection_and_keeps_the_flag_at_a_quiet_quit() {
    use crate::appstate::{Outcome, Quiet, Why, stop_quietly};
    use crate::session::fake::FakeSession;
    use wacore::appstate::hash::HashState;
    use wacore::store::traits::AppSyncStore;

    let _serial = crate::fault::serial();
    let rig = Rig::new();
    let store = rig.owner.store();
    store
        .set_version(
            "regular",
            HashState {
                version: 3,
                ..HashState::default()
            },
        )
        .await
        .expect("version");
    let (tap, mut signals) = Tap::channel(None);
    let forwarder = rig.forwarder(tap, Arc::new(Core::default()));
    crate::fault::arm(crate::fault::EVENT_WRITE, 1);
    forwarder.handle_event(call_log_sync("77700481537209@lid", "CALL-LOG-61820002"));
    crate::fault::disarm();
    assert_eq!(told(&mut signals), [WaSignal::WriteFailed]);
    assert_eq!(store.activity().lost(), ["regular"]);
    assert!(rig.outbox().await.is_empty(), "a failed zone wrote");

    let session = FakeSession::new();
    let (_, outcome) = stop_quietly(&store, &session, Quiet::STANDARD, false, &["regular"]).await;
    assert_eq!(outcome, Outcome::Kept(Why::Lost));
    assert_eq!(
        store.dirty_collections().await.expect("dirty"),
        [("regular".to_owned(), 3)],
        "the flag of the collection that lost an event stands"
    );
    drop(forwarder);
    rig.close();
}

/// M6: a full sync dispatched while the link hold is closed (the link's own sync starts on the new connection before
/// `AccountReady` is recorded) waits for the hold like every handler write: it lands AFTER the row that opened the
/// hold, and nothing ends the connection.
#[tokio::test]
async fn a_full_sync_while_the_link_hold_is_closed_lands_after_account_ready() {
    let rig = Rig::new();
    let store = rig.owner.store();
    let chat = lid_chat(PHONE_CHAT);
    core_knows(&rig, &chat).await;
    store.hold().set_closed(true);
    let (tap, mut signals) = Tap::channel(None);
    let forwarder = Arc::new(rig.forwarder(tap, Arc::new(Core::default())));
    let (done_tx, done) = std::sync::mpsc::channel();
    let handler = {
        let forwarder = forwarder.clone();
        std::thread::spawn(move || {
            forwarder.handle_event(archive_update("77700481537209@lid", true));
            let _ = done_tx.send(());
        })
    };
    for _ in 0..500 {
        if store.hold().parked() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(store.hold().parked(), 1, "the handler never parked");
    assert!(
        done.try_recv().is_err(),
        "the handler went on past the hold"
    );
    assert!(
        rig.outbox().await.is_empty(),
        "written behind a closed hold"
    );
    // What opens the hold: the row that must lead the link, then the hold itself.
    let marker = kourier_backend_api::BackendEvent::ConversationRemoved {
        conv: lid_chat("77700592640318"),
    };
    let row = kourier_backend_api::encode_outbox_row(std::slice::from_ref(&marker)).expect("row");
    store
        .write("lead", move |tx| {
            kourier_db::outbox::append(tx, &row, 1)
                .map(drop)
                .map_err(|_| crate::error::database("append"))
        })
        .await
        .expect("lead row");
    store.hold().set_closed(false);
    done.recv_timeout(Duration::from_secs(10))
        .expect("the handler returns once the hold opened");
    handler.join().expect("join");
    let events = rig.outbox().await;
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0], marker, "the lead row is first");
    assert!(
        matches!(&events[1], kourier_backend_api::BackendEvent::ConversationUpsert(p) if p.archived == Some(true)),
        "{events:?}"
    );
    assert!(
        told(&mut signals).is_empty(),
        "nothing ended the connection"
    );
    drop(forwarder);
    rig.close();
}

/// The messages of the hook are in the inbox already, so the handler writes none of them (a second
/// copy would be a second event if the ledger did not absorb it); the others are queued without
/// waiting (decision D4), in the order they arrive.
#[tokio::test]
async fn the_handler_writes_only_the_messages_the_hook_did_not_commit() {
    let rig = Rig::new();
    let (tap, mut signals) = Tap::channel(None);
    let forwarder = rig.forwarder(tap, Arc::new(Core::default()));
    forwarder.handle_event(messages(true, "31000000001@s.whatsapp.net", "HOOKED-1"));
    forwarder.handle_event(messages(false, "120363000000000002@newsletter", "NEWS-1"));
    forwarder.handle_event(messages(false, "31000000002@s.whatsapp.net", "PDO-1"));
    // A read queues behind the detached writes (one lane, one priority).
    assert_eq!(rig.inbox().await, ["newsletter", "pdo"]);
    assert!(rig.outbox().await.is_empty());
    assert!(told(&mut signals).is_empty());
    drop(forwarder);
    rig.close();
}

// ----- the drain's boundary (W3a, chunk C7) -----

fn live_arrival(id: &str, offline: bool) -> crate::inbox::Arrival {
    crate::inbox::Arrival {
        chat: "31000000001@s.whatsapp.net".into(),
        sender: "31000000001@s.whatsapp.net".into(),
        id: id.into(),
        origin: crate::inbox::Origin::E2e,
        message: vec![1, 2, 3],
        meta: crate::inbox::InboxMeta {
            v: crate::inbox::InboxMeta::VERSION,
            timestamp: 1_700_000_000,
            from_me: false,
            is_group: false,
            push_name: String::new(),
            lid_addressing: false,
            sender_alt: None,
            recipient_alt: None,
            ephemeral_expiration: None,
            is_offline: offline,
        },
    }
}

/// The `catch_up` flag of every inbox row, in arrival order.
async fn flags(store: &crate::store::KourierStore) -> Vec<(String, i64)> {
    store
        .read(|conn| {
            let mut stmt = conn
                .prepare("SELECT id, catch_up FROM inbox ORDER BY arrival")
                .map_err(crate::error::sql("read inbox"))?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .map_err(crate::error::sql("read inbox"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(crate::error::sql("read inbox"))?;
            Ok(rows)
        })
        .await
        .expect("read the inbox")
}

/// The handler writes the catch-up boundary in the writer's order (the machine of the actor reads
/// it): a drain that begins forgets it, upstream's completion takes it at that point, so a live
/// message in the drain's window is catch-up, one after the completion is not, and a late stanza of
/// the offline queue is catch-up whenever it comes.
#[tokio::test]
async fn the_handler_writes_the_boundary_in_the_writers_order() {
    let rig = Rig::new();
    let store = rig.owner.store();
    let (tap, mut signals) = Tap::channel(None);
    let forwarder = rig.forwarder(tap, Arc::new(Core::default()));
    let inbox = rig.receive.inbox.clone();

    forwarder.handle_event(Arc::new(Event::OfflineSyncPreview(
        wacore::types::events::OfflineSyncPreview::builder()
            .total(2)
            .app_data_changes(0)
            .messages(2)
            .notifications(0)
            .receipts(0)
            .calls(0)
            .statuses(0)
            .build(),
    )));
    assert_eq!(crate::catchup::boundary(&store).await.expect("read"), None);
    // A live message while the drain runs is catch-up: the drain is not known to be over.
    inbox
        .commit(vec![live_arrival("DURING", false)], Vec::new())
        .await
        .expect("commit");
    forwarder.handle_event(Arc::new(Event::OfflineSyncCompleted(
        wacore::types::events::OfflineSyncCompleted::builder()
            .count(2)
            .build(),
    )));
    assert_eq!(
        crate::catchup::boundary(&store).await.expect("read"),
        Some(1),
        "the counter at the completion"
    );
    inbox
        .commit(
            vec![live_arrival("LIVE", false), live_arrival("TAIL", true)],
            Vec::new(),
        )
        .await
        .expect("commit");
    assert_eq!(
        flags(&store).await,
        [
            ("DURING".to_owned(), 1),
            ("LIVE".to_owned(), 0),
            ("TAIL".to_owned(), 1)
        ]
    );
    // The connection ends: the next drain is not known to be over.
    forwarder.handle_event(Arc::new(Event::Disconnected(
        wacore::types::events::Disconnected::builder()
            .reason(wacore::net::DisconnectReason::ReadError(
                "synthetic".to_owned(),
            ))
            .build(),
    )));
    assert_eq!(crate::catchup::boundary(&store).await.expect("read"), None);
    assert_eq!(
        told(&mut signals),
        [
            WaSignal::OfflinePreview { total: 2 },
            WaSignal::OfflineDone { count: 2 },
            WaSignal::Disconnected { clean: false }
        ]
    );
    drop(forwarder);
    rig.close();
}

/// W5a chunk 7: a failed `Client::send` is read by upstream's TYPED error, so `send::error_of` (D6) sees a
/// socket failure apart from a refusal before the wire, and an error wrapped in `anyhow` is not lost.
#[test]
fn a_failed_send_is_read_by_its_typed_error() {
    use whatsapp_rust::send::SendError;
    let cases: Vec<(SendError, SendFailure, &str)> = vec![
        (
            SendError::NotLoggedIn,
            SendFailure::NotLoggedIn,
            "not logged in",
        ),
        (
            SendError::Client(whatsapp_rust::ClientError::NotConnected),
            SendFailure::NotConnected,
            "not connected",
        ),
        (
            SendError::Client(whatsapp_rust::ClientError::NotLoggedIn),
            SendFailure::NotLoggedIn,
            "the client's not logged in",
        ),
        (
            SendError::Iq(IqError::Timeout),
            SendFailure::IqTimeout,
            "an IQ timeout",
        ),
        (
            SendError::Iq(IqError::NotConnected),
            SendFailure::NotConnected,
            "an IQ without a connection",
        ),
        (
            SendError::InvalidRequest(String::from("refused")),
            SendFailure::InvalidRequest,
            "an invalid request",
        ),
        (
            SendError::NoRecipientDevice(wacore::send::NoRecipientDeviceError::Unresolved),
            SendFailure::NoRecipientDevice,
            "no recipient device",
        ),
        (
            SendError::PrimaryDeviceRejected(wacore::send::PrimaryDeviceRejected::new(404)),
            SendFailure::PrimaryDeviceRejected,
            "the primary device refused",
        ),
        (
            SendError::Internal(anyhow::Error::new(whatsapp_rust::ClientError::NotConnected)),
            SendFailure::NotConnected,
            "a client error inside the catch-all",
        ),
        (
            SendError::Internal(anyhow::Error::new(
                wacore::send::NoRecipientDeviceError::Unresolved,
            )),
            SendFailure::NoRecipientDevice,
            "no recipient device inside the catch-all",
        ),
        (
            SendError::Internal(anyhow::anyhow!("something else")),
            SendFailure::Internal,
            "an untyped failure",
        ),
    ];
    for (error, want, what) in cases {
        assert!(send_failure(&error) == want, "{what} is read wrongly");
    }
}

/// Fix F21 (re-check N1): a failed group-info ask is read by upstream's TYPED error, so the receipt runner can tell the
/// connection's or the server's trouble (the job waits for the next connection: `receipts::connectivity`) from a failure
/// of the job itself (it is dropped). The `w:g2` IQ's own failures are read like any IQ's; the rest is internal.
/// Mutant: every group error is internal (the Wire case `a_group_receipt_waits_for_a_group_info_that_fails` is the
/// other half: the real session, the server's error).
#[test]
fn a_failed_group_info_ask_is_read_by_its_typed_error() {
    use crate::send::receipts::connectivity;
    let cases: Vec<(GroupError, SendFailure, bool, &str)> = vec![
        (
            GroupError::Iq(IqError::Timeout),
            SendFailure::IqTimeout,
            true,
            "a w:g2 IQ that timed out",
        ),
        (
            GroupError::Iq(IqError::NotConnected),
            SendFailure::NotConnected,
            true,
            "a w:g2 IQ without a connection",
        ),
        (
            GroupError::InvalidRequest(String::from("refused")),
            SendFailure::Internal,
            false,
            "a malformed request",
        ),
        (
            GroupError::DescriptionConflict,
            SendFailure::Internal,
            false,
            "a description conflict",
        ),
        (
            GroupError::Internal(anyhow::anyhow!("something else")),
            SendFailure::Internal,
            false,
            "an untyped failure",
        ),
    ];
    for (error, want, waits, what) in cases {
        let failure = group_failure(&error);
        assert!(failure == want, "{what} is read wrongly");
        assert_eq!(
            connectivity(failure),
            waits,
            "{what}: whether a receipt job waits for it"
        );
    }
}

/// W5a chunk 7 (D20): a group's addressing mode reaches `send::address` as the namespace it names.
#[test]
fn a_groups_addressing_mode_is_its_namespace() {
    use crate::send::address::Namespace;
    assert!(namespace_of(wacore::types::message::AddressingMode::Lid) == Namespace::Lid);
    assert!(namespace_of(wacore::types::message::AddressingMode::Pn) == Namespace::Pn);
}

/// W5a (brief §2.3, D5): the handler hands a message stanza's ack to the account's ack registry (and to no
/// one else: no signal), and upstream's `Disconnected` ends the registry's connection, so a wait of the old
/// connection hears `Ended` and the next wait of the same local is first. Mutants: the ack arm removed; the
/// connection's end not told.
#[tokio::test]
async fn the_handler_answers_a_waiting_send_with_its_ack_and_ends_the_waits_at_a_disconnect() {
    use crate::send::acks::{Answer, Waited};
    use wacore::net::DisconnectReason;
    use wacore::types::events::{Disconnected, ServerAck};

    const LOCAL: &str = "3EB0F8B1D46A29375016";
    let (tap, mut signals) = Tap::channel(None);
    let rig = Rig::new();
    let acks = rig.receive.acks.clone();
    let forwarder = rig.forwarder(tap, Arc::new(Core::default()));
    let wait = acks.wait(LOCAL);
    forwarder.handle_event(Arc::new(Event::ServerAck(
        ServerAck::builder()
            .id(LOCAL.to_owned())
            .class("message".to_owned())
            .timestamp(wacore::chrono::DateTime::from_timestamp(1_760_000_789, 0).expect("a time"))
            .build(),
    )));
    assert_eq!(
        wait.answered().await,
        Waited::Answered(Answer::Acked {
            server_t: Some(1_760_000_789)
        })
    );
    assert!(told(&mut signals).is_empty(), "an ack is no signal");
    // An acked local is remembered for the connection (no second frame); another local's frame is in the air.
    assert!(!acks.wait(LOCAL).first());
    const OTHER: &str = "3EB0E9C27A4D15F83062";
    let old = acks.wait(OTHER);
    forwarder.handle_event(Arc::new(Event::Disconnected(
        Disconnected::builder()
            .reason(DisconnectReason::StreamEnded)
            .build(),
    )));
    assert_eq!(acks.connection(), 1, "the disconnect ends the connection");
    assert_eq!(
        old.answered().await,
        Waited::Ended,
        "no ack comes on a connection that ended"
    );
    assert!(
        acks.wait(LOCAL).first() && acks.wait(OTHER).first(),
        "the next connection remembers nothing of the last one"
    );
    drop(forwarder);
    rig.close();
}

/// Fix F12: upstream's `Connected` (a new `<success>`, dispatched after the offline wait) does NOT end the registry's
/// connection: a frame written on the new socket before that event is registered under the old number, and ending the
/// connection there would end its wait and let the core's resend write a second frame of the local on the same
/// socket (module docs of `send::acks`, "THE LAG"). Mutant: the handler ends the connection at `Connected`.
#[tokio::test]
async fn a_connected_event_does_not_end_the_registrys_connection() {
    use crate::send::acks::Answer;
    use wacore::types::events::ServerAck;

    const LOCAL: &str = "3EB0F8B1D46A29375017";
    let (tap, _signals) = Tap::channel(None);
    let rig = Rig::new();
    let acks = rig.receive.acks.clone();
    let forwarder = rig.forwarder(tap, Arc::new(Core::default()));
    let wait = acks.wait(LOCAL);
    assert!(wait.first(), "the frame is written");
    forwarder.handle_event(Arc::new(Event::Connected(Connected::builder().build())));
    assert_eq!(acks.connection(), 0, "a Connected moved the registry on");
    assert!(
        !acks.wait(LOCAL).first(),
        "after a Connected the local in the air is written again on its socket"
    );
    // The ack of the frame still reaches its wait.
    forwarder.handle_event(Arc::new(Event::ServerAck(
        ServerAck::builder()
            .id(LOCAL.to_owned())
            .class("message".to_owned())
            .build(),
    )));
    assert_eq!(
        wait.answered().await,
        crate::send::acks::Waited::Answered(Answer::Acked { server_t: None })
    );
    drop(forwarder);
    rig.close();
}

/// W5b (brief §2.6): a failed upload is read by TYPE: the media server's status from anywhere in the chain (upstream
/// wraps it in its "Upload failed <status> body=<the CDN's answer>" context, and a caller may wrap that again), and
/// "nothing finished" for every chain without one (an IQ timeout, a read of the source that failed after a stop).
/// Mutants: the status read only at the top of the chain; every failure without a status taken as a 500.
#[test]
fn an_upload_failure_is_its_http_status_or_nothing_finished() {
    use whatsapp_rust::http::HttpStatusError;

    let wrapped = |status: u16| {
        anyhow::Error::new(HttpStatusError { status })
            .context(format!(
                "Upload failed {status} body=synthetic-cdn-body-36184027"
            ))
            .context("the outer step")
    };
    for status in [401, 403, 413, 500] {
        assert_eq!(
            upload_failure(&wrapped(status)),
            UploadFailure::Status(status)
        );
    }
    assert_eq!(
        upload_failure(&anyhow::Error::new(HttpStatusError { status: 429 })),
        UploadFailure::Status(429)
    );
    assert_eq!(
        upload_failure(&anyhow::Error::new(IqError::Timeout)),
        UploadFailure::Incomplete
    );
    assert_eq!(
        upload_failure(
            &anyhow::Error::new(std::io::Error::other("the upload was stopped"))
                .context("the body")
        ),
        UploadFailure::Incomplete
    );
}

/// W6a1 (D2, scout T6): the replay of a resync is handed to the handler on the thread that polls the call, and the
/// handler parks that thread until each zone committed, so the snapshot is asked ONLY inside the task `on_runtime`
/// spawns on the client's runtime, never on the actor's thread. A source check of the one place that asks
/// (`tests/appstate_wire.rs` observes the thread on the Wire). Kills "the call awaited on the caller's thread".
#[test]
fn the_snapshot_is_asked_only_inside_the_task_on_the_clients_runtime() {
    let code = include_str!("../client.rs");
    let asks: Vec<usize> = code
        .match_indices(".resync_app_state(")
        .map(|(at, _)| at)
        .collect();
    assert_eq!(asks.len(), 1, "one call site of upstream's resync");
    let ask = asks[0];
    let before = &code[..ask];
    let start = before
        .rfind("async fn resync(")
        .expect("the session's resync");
    let task = before[start..]
        .rfind(".on_runtime(")
        .expect("spawned on the runtime");
    let between = &before[start + task..];
    assert!(
        !between.contains(';'),
        "the call stands inside the `on_runtime` future, with nothing ended between them"
    );
}
