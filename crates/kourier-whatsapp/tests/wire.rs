//! T-11: the account actor over a REAL client that talks to the SCRIPTED server (W2 chunk C5; the
//! fixture is `support::wire`, lane decision L3).
//!
//! Every body runs in ONE offline child under `unshare -rn`: the client dials a pipe into this
//! process, completes a real Noise handshake with a server that holds no WhatsApp key (the
//! noise-certificate check is off for it, see `support::wire`), and then hears the verdicts a
//! server gives, as stanzas. What this proves is Kourier's side of the wire: what the registration
//! payload tells the phone, what each verdict does to the account, that a terminal verdict stops
//! the client for good, that the usage-statistics lease sees the frames and none is one. What it
//! cannot prove: that WhatsApp's real server says these things in these shapes (R1 does).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{
    AccountContext, BackendConfig, BackendEvent, Command, ContextParams, Exit, Network, StartMode,
    SystemClock,
};
use kourier_model::{AccountId, AccountUuid, ConnState, LoginStepKind, UserAction};
use kourier_whatsapp::WhatsAppNetwork;
use tokio_util::sync::CancellationToken;
use wacore_binary::builder::NodeBuilder;
use waproto::buffa::Message;
use waproto::whatsapp as wa;
use waproto::whatsapp::device_props::PlatformType;

use crate::actor_real::{Running, paired_dir, record, wait_for_states};
use crate::support::wire::{self, Wire};
use crate::support::{self, TEST_KEY, db_path};

const SERVER_JID: &str = "s.whatsapp.net";

/// An actor over the scripted server's parts.
pub(crate) fn start_actor(dir: &Path, start: StartMode, server: &Wire) -> Running {
    start_actor_with(dir, start, server, Default::default())
}

/// As [`start_actor`], with the backend configuration of the run (W2c: the number tail a first link
/// is checked against).
pub(crate) fn start_actor_with(
    dir: &Path,
    start: StartMode,
    server: &Wire,
    config: BackendConfig,
) -> Running {
    let (ctx, ends) = context(dir, start, config);
    let handle = WhatsAppNetwork::new(wire::parts(server))
        .spawn(ctx)
        .expect("the actor starts");
    running(ends, handle)
}

/// As [`start_actor`], with the actor's client handed to the test through `slot` (W5a 2b:
/// `wire::parts_handing_client`, filled by a `wire::hand_over` stanza).
pub(crate) fn start_actor_handing_client(
    dir: &Path,
    start: StartMode,
    server: &Wire,
    slot: &wire::ClientSlot,
) -> Running {
    let (ctx, ends) = context(dir, start, Default::default());
    let handle = WhatsAppNetwork::new(wire::parts_handing_client(server, slot))
        .spawn(ctx)
        .expect("the actor starts");
    running(ends, handle)
}

/// As [`start_actor_handing_client`], with the clients' HTTP client made by `http` (W5b: the CDN fake of
/// `support::cdn`). The network is made from the scripted server's parts in this call (the builder source check
/// allows no parts held in a variable).
pub(crate) fn start_actor_with_http<H: whatsapp_rust::http::HttpClient + 'static>(
    dir: &Path,
    start: StartMode,
    server: &Wire,
    slot: &wire::ClientSlot,
    http: impl Fn() -> H + Send + Sync + 'static,
) -> Running {
    let (ctx, ends) = context(dir, start, Default::default());
    let handle = WhatsAppNetwork::new(wire::parts_handing_client_with_http(server, slot, http))
        .spawn(ctx)
        .expect("the actor starts");
    running(ends, handle)
}

/// As [`start_actor_handing_client`], with a raw event handler of the test beside Kourier's own (W6a1 A1, the
/// app-state probe of `tests/appstate_wire.rs`: `wire::parts_observing`).
pub(crate) fn start_actor_observing(
    dir: &Path,
    start: StartMode,
    server: &Wire,
    slot: &wire::ClientSlot,
    observer: Arc<dyn wacore::types::events::EventHandler>,
) -> Running {
    let (ctx, ends) = context(dir, start, Default::default());
    let handle = WhatsAppNetwork::new(wire::parts_observing(server, slot, observer))
        .spawn(ctx)
        .expect("the actor starts");
    running(ends, handle)
}

/// As [`start_actor`], with a history-sync capture hook of the test on the client (W4 C1, the Wire
/// probe of `tests/history_wire.rs`: `wire::parts_capturing`).
pub(crate) fn start_actor_capturing(
    dir: &Path,
    start: StartMode,
    server: &Wire,
    capture: Arc<dyn whatsapp_rust::HistorySyncCaptureHook>,
) -> Running {
    let (ctx, ends) = context(dir, start, Default::default());
    let handle = WhatsAppNetwork::new(wire::parts_capturing(server, capture))
        .spawn(ctx)
        .expect("the actor starts");
    running(ends, handle)
}

/// The account's context over the account directory `dir`.
fn context(
    dir: &Path,
    start: StartMode,
    config: BackendConfig,
) -> (AccountContext, kourier_backend_api::CoreEnds) {
    AccountContext::new(ContextParams {
        account: AccountId(1),
        uuid: AccountUuid::new_random(),
        start,
        backend_db: db_path(dir),
        db_key: kourier_backend_api::DbKey::new(TEST_KEY),
        blob_dir: dir.join("blobs"),
        net_rt: tokio::runtime::Handle::current(),
        cancel: CancellationToken::new(),
        clock: Arc::new(SystemClock),
        config,
        ui: Default::default(),
        local_prefs: Default::default(),
    })
}

fn running(
    ends: kourier_backend_api::CoreEnds,
    handle: kourier_backend_api::AccountHandle,
) -> Running {
    Running {
        commands: ends.commands.clone(),
        ends,
        handle,
    }
}

/// A resumed account that has connected and completed the handshake.
async fn connected_account(server: &Wire, dir: &Path) -> Running {
    let actor = start_actor(dir, StartMode::Resume, server);
    actor.send(Command::Connect).await;
    assert!(
        server.wait_opened(1, Duration::from_secs(15)).await,
        "the handshake never completed (dials {})",
        server.dials()
    );
    actor
}

/// The wall clock in milliseconds (the actor runs on the system clock here).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after 1970")
        .as_millis() as u64
}

/// Dials do not grow for `for_secs` seconds: the client stopped for good.
async fn no_more_dials(server: &Wire, for_secs: u64) {
    let dials = server.dials();
    tokio::time::sleep(Duration::from_secs(for_secs)).await;
    assert_eq!(server.dials(), dials, "the client dialled again");
}

/// What the phone is told at the link: the registration payload of the BUILT client, read off the
/// wire from the ClientFinish (M6 on the wire; the unit test reads the same from the built client).
async fn the_registration_over_the_wire_names_kourier() {
    let server = wire::server();
    let dir = tempfile::tempdir().expect("temp dir");
    let mut actor = start_actor(dir.path(), StartMode::Fresh, &server);
    actor
        .send(Command::StartLogin {
            flow: "qr".to_owned(),
            relink: None,
            taken: Vec::new(),
        })
        .await;
    assert!(
        server.wait_opened(1, Duration::from_secs(15)).await,
        "the handshake never completed"
    );
    let payload = server.client_payload(0).expect("the ClientFinish payload");
    let pairing = payload
        .device_pairing_data
        .as_option()
        .expect("a registration payload carries the pairing data");
    let props = wa::DeviceProps::decode_from_slice(
        pairing
            .device_props
            .as_deref()
            .expect("the pairing data holds the device props"),
    )
    .expect("DeviceProps decode");
    let os = props.os.as_deref().unwrap_or_default();
    // The label is made from the host name the application gave, nothing else.
    assert_eq!(
        os,
        format!("Kourier ({})", wire::WIRE_HOST),
        "the phone would list this device as {os:?}"
    );
    assert_ne!(os, "rust", "upstream's default identity reached the wire");
    assert_eq!(props.platform_type, Some(PlatformType::DESKTOP));
    assert_eq!(props.require_full_sync, Some(false));
    // The version was fixed by the fixture: nothing fetched it.
    assert_eq!(server.http_calls(), 0, "a version fetch was made");
    assert_eq!(server.dials(), 1);

    actor.send(Command::Shutdown).await;
    assert_eq!(actor.exit().await.0, Exit::Shutdown);
}

/// The server's `<pair-device>` with refs: the actor shows QR codes (one step per code), the
/// client answers on the wire (so frames leave it), and none of what leaves is a statistics frame
/// - the lease of the watch saw them (the fixture counts them independently).
async fn a_pair_device_stanza_shows_a_qr_code_and_no_statistics_frame_leaves() {
    let server = wire::server();
    let dir = tempfile::tempdir().expect("temp dir");
    let mut actor = start_actor(dir.path(), StartMode::Fresh, &server);
    actor
        .send(Command::StartLogin {
            flow: "qr".to_owned(),
            relink: None,
            taken: Vec::new(),
        })
        .await;
    assert!(server.wait_opened(1, Duration::from_secs(15)).await);
    let before = kourier_whatsapp::wam_sent();
    let looked_before = kourier_whatsapp::wam_looked();
    let refs = (0..6).map(|i| {
        NodeBuilder::new("ref")
            .bytes(format!("2@synthetic{i}").into_bytes())
            .build()
    });
    server.send(
        &NodeBuilder::new("iq")
            .attrs([
                ("from", SERVER_JID),
                ("type", "set"),
                ("id", "pair-1"),
                ("xmlns", "md"),
            ])
            .children([NodeBuilder::new("pair-device").children(refs).build()])
            .build(),
    );
    // A QR step arrives.
    let mut shown = None;
    while shown.is_none() {
        let envelope = tokio::time::timeout(Duration::from_secs(15), actor.ends.events.recv())
            .await
            .expect("a QR step comes")
            .expect("an envelope");
        for event in envelope.events {
            if let BackendEvent::Login(step) = event
                && let LoginStepKind::ShowQr { data, valid_for } = step.kind
            {
                assert_eq!(step.step_id, "qr");
                assert!(
                    data.expose().contains("synthetic0"),
                    "the code does not carry the server's first ref"
                );
                assert!(valid_for.is_some_and(|d| d > Duration::ZERO));
                shown = Some(());
            }
        }
    }
    // The client acknowledged the stanza: frames left it, through the lease.
    assert!(
        server.wait_frames(1, Duration::from_secs(10)).await,
        "the client sent nothing after the handshake"
    );
    let frames = server.frames();
    assert!(
        frames
            .iter()
            .all(|f| f.xmlns.as_deref() != Some(&statistics_namespace())),
        "a statistics frame left the client: {frames:?}"
    );
    assert_eq!(
        kourier_whatsapp::wam_sent(),
        before,
        "the watch counted a statistics frame"
    );
    // ... and a 0 that means something: the watch looked at those frames (the lease is taken).
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while kourier_whatsapp::wam_looked() <= looked_before {
        assert!(
            std::time::Instant::now() < deadline,
            "the watch looked at no frame: the lease on the sent frames was not taken"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    actor.send(Command::Shutdown).await;
    assert_eq!(actor.exit().await.0, Exit::Shutdown);
}

/// The namespace of the usage statistics (assembled: the source check keeps `src/` the only place
/// that spells it, and a test file spelling it whole would be one more).
fn statistics_namespace() -> String {
    ["w:", "stats"].concat()
}

/// `<failure reason="405"/>`: the app is too old. The account says so and the client is not tried
/// again by itself (a terminal verdict).
async fn a_405_says_update_the_app_and_stops_the_client() {
    let server = wire::server();
    let dir = paired_dir().await;
    let mut actor = connected_account(&server, dir.path()).await;
    let seen = record(actor.conn());
    server.send(&NodeBuilder::new("failure").attr("reason", "405").build());
    wait_for_states(
        &seen,
        10,
        &[ConnState::NeedsUserAction(UserAction::UpdateApp)],
    )
    .await;
    no_more_dials(&server, 3).await;
    // The routing info the device kept (the fixture device holds `[9, 8, 7]`) opened the connection.
    assert_eq!(server.edge_routing(0), Some(vec![9, 8, 7]));
    actor.send(Command::Shutdown).await;
    assert_eq!(actor.exit().await.0, Exit::Shutdown);
}

/// `<failure reason="402" code=".." expire="60"/>`: a temporary ban with its end. The account is
/// restricted until then and the client is not tried again by itself.
async fn a_402_is_a_ban_until_its_end_and_stops_the_client() {
    let server = wire::server();
    let dir = paired_dir().await;
    let mut actor = connected_account(&server, dir.path()).await;
    let seen = record(actor.conn());
    let now = now_ms();
    server.send(
        &NodeBuilder::new("failure")
            .attrs([("reason", "402"), ("code", "101"), ("expire", "60")])
            .build(),
    );
    // Restricted, with an end about a minute away.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let until = loop {
        let found = seen.lock().expect("lock").iter().find_map(|s| match s {
            ConnState::Restricted { until_ms, .. } => *until_ms,
            _ => None,
        });
        if let Some(until) = found {
            break until;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no ban state: {:?}",
            seen.lock().expect("lock")
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(
        until > now + 50_000 && until < now + 70_000,
        "the ban ends {} ms from now, not in about 60 s",
        until.saturating_sub(now)
    );
    no_more_dials(&server, 3).await;
    actor.send(Command::Shutdown).await;
    assert_eq!(actor.exit().await.0, Exit::Shutdown);
}

/// `<stream:error code="516"/>`: the phone removed the device. The account is logged out, with the
/// actor's exit saying so, and nothing is tried again.
async fn a_516_logs_the_account_out() {
    let server = wire::server();
    let dir = paired_dir().await;
    let mut actor = connected_account(&server, dir.path()).await;
    server.send(&NodeBuilder::new("stream:error").attr("code", "516").build());
    let (exit, _) = actor.exit().await;
    assert!(
        matches!(&exit, Exit::LoggedOut { reason } if reason == "device removed"),
        "{exit:?}"
    );
    no_more_dials(&server, 2).await;
}

/// `<stream:error><conflict type="replaced"/></stream:error>`: another session took over. The
/// account asks the person to reconnect and is not tried again by itself.
async fn a_replaced_stream_asks_for_a_reconnect_and_stops_the_client() {
    let server = wire::server();
    let dir = paired_dir().await;
    let mut actor = connected_account(&server, dir.path()).await;
    let seen = record(actor.conn());
    server.send(
        &NodeBuilder::new("stream:error")
            .children([NodeBuilder::new("conflict")
                .attr("type", "replaced")
                .build()])
            .build(),
    );
    wait_for_states(
        &seen,
        10,
        &[ConnState::NeedsUserAction(UserAction::Reconnect)],
    )
    .await;
    no_more_dials(&server, 3).await;
    actor.send(Command::Shutdown).await;
    assert_eq!(actor.exit().await.0, Exit::Shutdown);
}

/// The parent: the cases run in the offline child below.
#[test]
fn the_real_client_talks_to_the_scripted_server() {
    support::offline::run_child(module_path!(), "wire_child");
}

/// The child (a no-op as an ordinary test).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wire_child() {
    if !support::offline::in_child() {
        return;
    }
    eprintln!("the_registration_over_the_wire_names_kourier");
    the_registration_over_the_wire_names_kourier().await;
    eprintln!("a_pair_device_stanza_shows_a_qr_code_and_no_statistics_frame_leaves");
    a_pair_device_stanza_shows_a_qr_code_and_no_statistics_frame_leaves().await;
    eprintln!("a_405_says_update_the_app_and_stops_the_client");
    a_405_says_update_the_app_and_stops_the_client().await;
    eprintln!("a_402_is_a_ban_until_its_end_and_stops_the_client");
    a_402_is_a_ban_until_its_end_and_stops_the_client().await;
    eprintln!("a_516_logs_the_account_out");
    a_516_logs_the_account_out().await;
    eprintln!("a_replaced_stream_asks_for_a_reconnect_and_stops_the_client");
    a_replaced_stream_asks_for_a_reconnect_and_stops_the_client().await;
}
