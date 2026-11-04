//! The dial module's tests (TH3 N2): the pins and the verifier, with the fixed certificates of
//! `tests/certs.rs` and a fixed time (decision M7). They run in an `unshare -rn` CHILD of the lib test
//! binary (threema.md: every test of `dial::*` does), started by
//! [`the_dial_tests_pass_in_an_offline_child`]; each child test begins with `if !in_child() { return; }`.
//! `tests/builder.rs` rule 11 keeps sockets, the resolver and the real dial out of this file: the
//! connector tests (N3) go over `tokio::io::duplex` only, against an in-process rustls server and
//! tungstenite's `accept_async`.

mod certs;

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use rustls::client::danger::ServerCertVerifier as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::{CertificateError, Error, RootCertStore};
use sha2::{Digest, Sha256};

use futures_util::{SinkExt as _, StreamExt as _};
use rustls::ServerConfig;
use rustls::time_provider::TimeProvider;
use tokio::io::{AsyncWriteExt as _, DuplexStream};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use super::https::{LiveHttps, read_body};
use super::pins::{PinSet, PinnedVerifier, Refusal, THREEMA_CH_PINS, decode_pin, refusal};
use super::tls::{HTTPS_ALPN, TlsConfigs, net_error, pinned_target};
use super::ws::{
    CLOSE_TEXT_FRAME, LiveWs, MAX_MESSAGE, MAX_RENDEZVOUS_MESSAGE, connect_over, event_depth,
    max_message,
};
use crate::parts::{
    HttpsClient as _, HttpsMethod, HttpsRequest, NetError, UntrustedKind, WsConnector as _,
    WsEvent, WsKind, WsRequest, WsSender,
};

// ---- the offline child (a copy of `tests/support/offline.rs`'s gate for the lib binary) ----

/// Set in the child.
const CHILD: &str = "THREEMA_DIAL_OFFLINE_CHILD";

/// `1` makes a missing `unshare -rn` a failure (`cargo xtask ci` sets it).
const REQUIRE_ENV: &str = "KOURIER_REQUIRE_UNSHARE";

/// The prefix of every child test's name (the child runs exactly these).
const CHILD_PREFIX: &str = "dial::tests::offline_";

/// Every child test, by name after [`CHILD_PREFIX`]: the parent checks that each passed.
const OFFLINE: &[&str] = &[
    "offline_a_message_over_the_mediator_limit_is_taken_only_by_a_rendezvous_socket",
    "offline_one_byte_over_the_rendezvous_limit_closes_1009",
    "offline_the_rendezvous_queue_is_two_deep_and_the_mediators_64",
    "offline_the_fixtures_agree_with_each_other",
    "offline_the_production_table_is_desktops_six_pins",
    "offline_the_host_rule_is_one_label_under_the_domain",
    "offline_the_pinned_leaf_under_the_trusted_ca_is_accepted",
    "offline_a_pin_set_without_the_leafs_hash_is_a_pin_mismatch",
    "offline_the_production_pins_refuse_the_test_leaf",
    "offline_an_unpinned_host_is_refused_before_the_certificate_is_read",
    "offline_a_pinned_key_under_an_unknown_ca_is_untrusted",
    "offline_a_pinned_leaf_for_another_name_is_untrusted",
    "offline_the_same_verifier_accepts_in_time_and_refuses_a_date_error_after",
    "offline_only_a_date_error_is_transient",
    "offline_a_binary_frame_goes_both_ways_and_close_sends_the_code",
    "offline_a_server_close_arrives_with_its_code",
    "offline_an_end_without_a_close_frame_is_1006",
    "offline_a_text_frame_closes_with_4010",
    "offline_the_largest_message_is_taken_and_one_byte_more_closes_1009",
    "offline_a_handshake_against_the_pins_fails_untrusted_and_reads_no_frame",
    "offline_a_handshake_after_not_after_is_a_date_error",
    "offline_the_websocket_offers_no_h2_and_https_offers_h2_and_http1",
    "offline_the_websocket_dials_only_wss_to_a_pinned_host",
    "offline_the_websocket_connector_refuses_a_bad_target_before_any_dial",
    "offline_the_read_pump_ends_with_its_sender",
    "offline_https_refuses_plain_http_and_unpinned_hosts_before_any_io",
    "offline_an_https_body_over_its_limit_fails_and_stops_the_read",
    "offline_the_production_params_are_libthreemas_production_config",
    "offline_a_tls_refusal_is_found_inside_an_io_error",
];

fn in_child() -> bool {
    std::env::var_os(CHILD).is_some()
}

fn unshare_available() -> bool {
    Command::new("unshare")
        .args(["-rn", "true"])
        .status()
        .is_ok_and(|status| status.success())
}

/// THE PARENT: runs every `offline_*` test of this module in ONE child of this binary under
/// `unshare -rn` (no interface, no route) and fails unless each of [`OFFLINE`] passed there. Where
/// user namespaces are unavailable it skips with a message, or fails under [`REQUIRE_ENV`]`=1`.
#[test]
fn the_dial_tests_pass_in_an_offline_child() {
    if in_child() {
        return;
    }
    if !unshare_available() {
        assert!(
            std::env::var(REQUIRE_ENV).ok().as_deref() != Some("1"),
            "`unshare -rn` is not available and {REQUIRE_ENV}=1: the dial tests must run"
        );
        eprintln!(
            "SKIPPED: `unshare -rn` is not available (user namespaces); the dial tests need it"
        );
        return;
    }
    let exe = std::env::current_exe().expect("test binary");
    let out = Command::new("unshare")
        .arg("-rn")
        .arg(exe)
        .arg(CHILD_PREFIX)
        .args(["--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .expect("spawn the child under unshare");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let summary = format!("test result: ok. {} passed;", OFFLINE.len());
    assert!(
        out.status.success() && stdout.contains(&summary),
        "the offline child failed ({:?}), expected `{summary}`:\n{stdout}\n{stderr}",
        out.status
    );
    for name in OFFLINE {
        assert!(
            stdout.contains(&format!("dial::tests::{name} ... ok")),
            "{name} did not pass in the child"
        );
    }
}

// ---- helpers ----

/// The fixed verification time: 2027-01-15, inside the certificates' validity.
const T_S: u64 = 1_800_000_000;

fn at(seconds: u64) -> UnixTime {
    UnixTime::since_unix_epoch(Duration::from_secs(seconds))
}

fn name(host: &str) -> ServerName<'static> {
    ServerName::try_from(host).expect("a valid name").to_owned()
}

/// A trust store with the test CA only.
fn test_roots() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(certs::CA))
        .expect("the test CA");
    roots
}

fn verifier(pins: PinSet) -> PinnedVerifier {
    PinnedVerifier::new(
        pins,
        test_roots(),
        Arc::new(rustls::crypto::ring::default_provider()),
    )
    .expect("a verifier")
}

/// The pins of the test hosts: the leaf's key only.
fn test_pins() -> PinSet {
    PinSet::new("kourier.invalid", vec![certs::LEAF_SPKI_SHA256])
}

/// What the verifier says about `leaf` for `host` at `time`: `None` when it accepts.
fn verdict(verifier: &PinnedVerifier, leaf: &[u8], host: &str, time: u64) -> Option<Refusal> {
    match verifier.verify_server_cert(&CertificateDer::from(leaf), &[], &name(host), &[], at(time))
    {
        Ok(_) => None,
        Err(error) => Some(refusal(&error).expect("a certificate error")),
    }
}

// ---- the child tests ----

/// The fixed certificates agree with each other: each private key's public half hashes to the pin
/// openssl printed, and rustls' SPKI of the leaf (the whole DER SEQUENCE, as Desktop and RFC 7469
/// hash it) gives the same bytes.
#[test]
fn offline_the_fixtures_agree_with_each_other() {
    if !in_child() {
        return;
    }
    for (key, cert, pin) in [
        (certs::LEAF_KEY, certs::LEAF, certs::LEAF_SPKI_SHA256),
        (
            certs::OTHER_LEAF_KEY,
            certs::OTHER_LEAF,
            certs::OTHER_SPKI_SHA256,
        ),
    ] {
        let key = rustls::crypto::ring::sign::any_supported_type(&PrivateKeyDer::Pkcs8(
            PrivatePkcs8KeyDer::from(key),
        ))
        .expect("a P-256 key");
        let public = key.public_key().expect("the key's SPKI");
        assert_eq!(<[u8; 32]>::from(Sha256::digest(public.as_ref())), pin);
        let cert = CertificateDer::from(cert);
        let parsed = ParsedCertificate::try_from(&cert).expect("a certificate");
        let spki = parsed.subject_public_key_info();
        assert_eq!(<[u8; 32]>::from(Sha256::digest(spki.as_ref())), pin);
    }
    assert_ne!(certs::LEAF_SPKI_SHA256, certs::OTHER_SPKI_SHA256);
}

/// The production table is Desktop's six pins of `*.threema.ch`, each 32 bytes after base64.
/// The literals are copied from `threema-desktop/apps/desktop/config/vite.config.ts:366-371` at
/// `93a6615c`.
#[test]
fn offline_the_production_table_is_desktops_six_pins() {
    if !in_child() {
        return;
    }
    let desktop = [
        "vGQZ8hm2h+km+q7rnJ7kF9S17BwSY0rbhwjz6nIupf0=",
        "3L0bKTNfTwVUCjYqqhZXJIO03qC00bSnuxZFsb09OUo=",
        "NN6Lb+2AE7CN3HWZKoWOe6mmHROOnywWoKZYWL1oHIU=",
        "efJCZofFPR3oV/bBk0wmehqnhy3Vv+s9P+3sjhHem/E=",
        "2Vpy8qUQCqc2+Lg6BgRO8G6e6vh7NmvVHTljfwP/Pfk=",
        "KKBJHJn1PQSdNTmoAfhxqWTO61r8O8bPi/JeGtP/6gg=",
    ];
    assert_eq!(THREEMA_CH_PINS, desktop);
    let production = PinSet::production();
    let decoded: Vec<[u8; 32]> = desktop
        .iter()
        .map(|pin| decode_pin(pin).expect("32 bytes"))
        .collect();
    assert_eq!(production.hashes(), decoded.as_slice());
    assert_eq!(production.hashes().len(), 6);
    // A pin that is not 32 bytes of base64 pins nothing.
    assert_eq!(decode_pin("AAAA"), None);
    assert_eq!(decode_pin("not base64!"), None);
}

/// The host rule: ONE non-empty label under the domain, ASCII case ignored.
#[test]
fn offline_the_host_rule_is_one_label_under_the_domain() {
    if !in_child() {
        return;
    }
    let production = PinSet::production();
    for host in [
        "mediator-1.threema.ch",
        "ds-apip.threema.ch",
        "Mediator.THREEMA.ch",
    ] {
        assert!(production.covers(host), "{host}");
    }
    for host in [
        "a.b.threema.ch",
        "threema.ch",
        ".threema.ch",
        "xthreema.ch",
        "a.threema.com",
        "a.threema.ch.example.invalid",
        "example.invalid",
        "",
    ] {
        assert!(!production.covers(host), "{host}");
    }
    assert!(test_pins().covers(certs::HOST));
    assert!(!test_pins().covers("a.mediator-1.kourier.invalid"));
}

/// The pinned leaf, issued by the trusted CA, for its host, at a time inside its dates: accepted.
#[test]
fn offline_the_pinned_leaf_under_the_trusted_ca_is_accepted() {
    if !in_child() {
        return;
    }
    assert_eq!(
        verdict(&verifier(test_pins()), certs::LEAF, certs::HOST, T_S),
        None
    );
    // The other leaf, when it is the one pinned.
    let other = PinSet::new("kourier.invalid", vec![certs::OTHER_SPKI_SHA256]);
    assert_eq!(
        verdict(&verifier(other), certs::OTHER_LEAF, certs::HOST, T_S),
        None
    );
}

/// A valid chain is not enough: a leaf whose key is none of the pins is refused as a pin mismatch,
/// an untrusted refusal.
#[test]
fn offline_a_pin_set_without_the_leafs_hash_is_a_pin_mismatch() {
    if !in_child() {
        return;
    }
    let others = PinSet::new("kourier.invalid", vec![certs::OTHER_SPKI_SHA256]);
    let v = verifier(others);
    assert_eq!(
        verdict(&v, certs::LEAF, certs::HOST, T_S),
        Some(Refusal::PinMismatch)
    );
    assert_eq!(
        verdict(&verifier(test_pins()), certs::OTHER_LEAF, certs::HOST, T_S),
        Some(Refusal::PinMismatch)
    );
    // No pin at all pins nothing.
    let none = PinSet::new("kourier.invalid", Vec::new());
    assert_eq!(
        verdict(&verifier(none), certs::LEAF, certs::HOST, T_S),
        Some(Refusal::PinMismatch)
    );
}

/// Threema's six pins refuse the test leaf (under the test domain, with the test CA trusted).
#[test]
fn offline_the_production_pins_refuse_the_test_leaf() {
    if !in_child() {
        return;
    }
    let production = PinSet::new("kourier.invalid", PinSet::production().hashes().to_vec());
    assert_eq!(
        verdict(&verifier(production), certs::LEAF, certs::HOST, T_S),
        Some(Refusal::PinMismatch)
    );
    // And the production verifier as a whole (Mozilla roots) refuses the test host outright.
    let real = PinnedVerifier::production().expect("the production verifier");
    assert_eq!(
        verdict(&real, certs::LEAF, certs::HOST, T_S),
        Some(Refusal::UnpinnedHost)
    );
}

/// An unpinned host is refused BEFORE a byte of the certificate is read: bytes that are no
/// certificate at all still give `UnpinnedHost`, not an encoding error.
#[test]
fn offline_an_unpinned_host_is_refused_before_the_certificate_is_read() {
    if !in_child() {
        return;
    }
    let real = PinnedVerifier::production().expect("the production verifier");
    for host in [
        "a.b.threema.ch",
        "example.invalid",
        "threema.ch",
        "a.threema.com",
        "127.0.0.1",
    ] {
        for leaf in [&[0xde, 0xad][..], certs::LEAF] {
            assert_eq!(
                verdict(&real, leaf, host, T_S),
                Some(Refusal::UnpinnedHost),
                "{host}"
            );
        }
    }
    let test = verifier(test_pins());
    assert_eq!(
        verdict(&test, &[0xde, 0xad], "a.mediator-1.kourier.invalid", T_S),
        Some(Refusal::UnpinnedHost)
    );
    // A pinned host with bytes that are no certificate: an untrusted encoding error.
    assert_eq!(
        verdict(&real, &[0xde, 0xad], "mediator.threema.ch", T_S),
        Some(Refusal::BadCertificate)
    );
}

/// The chain step (decision M6): the PINNED key under a CA nobody trusts (an intercepting proxy
/// that copied the key) is refused as an unknown issuer, untrusted.
#[test]
fn offline_a_pinned_key_under_an_unknown_ca_is_untrusted() {
    if !in_child() {
        return;
    }
    let refused = verdict(
        &verifier(test_pins()),
        certs::LEAF_UNKNOWN_CA,
        certs::HOST,
        T_S,
    );
    assert_eq!(refused, Some(Refusal::UnknownIssuer));
    assert!(!refused.is_some_and(Refusal::is_transient));
    // The unknown CA itself as an intermediate does not help.
    let v = verifier(test_pins());
    let result = v.verify_server_cert(
        &CertificateDer::from(certs::LEAF_UNKNOWN_CA),
        &[CertificateDer::from(certs::UNKNOWN_CA)],
        &name(certs::HOST),
        &[],
        at(T_S),
    );
    assert_eq!(
        result.err().as_ref().and_then(refusal),
        Some(Refusal::UnknownIssuer)
    );
}

/// The name check runs on a PINNED leaf: the right key under the trusted CA, but for
/// `mediator-2.kourier.invalid` only, shown for `mediator-1`: refused, untrusted.
#[test]
fn offline_a_pinned_leaf_for_another_name_is_untrusted() {
    if !in_child() {
        return;
    }
    let refused = verdict(
        &verifier(test_pins()),
        certs::LEAF_OTHER_NAME,
        certs::HOST,
        T_S,
    );
    assert_eq!(refused, Some(Refusal::BadName));
    assert_eq!(
        verdict(
            &verifier(test_pins()),
            certs::LEAF_OTHER_NAME,
            "mediator-2.kourier.invalid",
            T_S
        ),
        None
    );
}

/// The time is the call's (decision M7): the SAME verifier accepts the pinned leaf at T, refuses it
/// one second after `notAfter` and one second before `notBefore` as a DATE error (not a pin error),
/// and accepts it again at T.
#[test]
fn offline_the_same_verifier_accepts_in_time_and_refuses_a_date_error_after() {
    if !in_child() {
        return;
    }
    let v = verifier(test_pins());
    assert_eq!(verdict(&v, certs::LEAF, certs::HOST, T_S), None);
    assert_eq!(
        verdict(&v, certs::LEAF, certs::HOST, certs::NOT_AFTER_S),
        None
    );
    assert_eq!(
        verdict(&v, certs::LEAF, certs::HOST, certs::NOT_AFTER_S + 1),
        Some(Refusal::Dates)
    );
    // 2026-01-01 00:00:00 UTC is the certificates' notBefore.
    assert_eq!(
        verdict(&v, certs::LEAF, certs::HOST, 1_767_225_600 - 1),
        Some(Refusal::Dates)
    );
    assert_eq!(verdict(&v, certs::LEAF, certs::HOST, T_S), None);
}

/// Every refusal but a date error is untrusted (C8, M6); an error that is not about the
/// certificate is no refusal.
#[test]
fn offline_only_a_date_error_is_transient() {
    if !in_child() {
        return;
    }
    for (refusal, transient) in [
        (Refusal::UnpinnedHost, false),
        (Refusal::PinMismatch, false),
        (Refusal::UnknownIssuer, false),
        (Refusal::BadName, false),
        (Refusal::BadCertificate, false),
        (Refusal::Dates, true),
    ] {
        assert_eq!(refusal.is_transient(), transient, "{refusal:?}");
    }
    for (error, expected) in [
        (CertificateError::Expired, Refusal::Dates),
        (CertificateError::NotValidYet, Refusal::Dates),
        (CertificateError::UnknownIssuer, Refusal::UnknownIssuer),
        (CertificateError::NotValidForName, Refusal::BadName),
        (CertificateError::BadSignature, Refusal::BadCertificate),
        (CertificateError::BadEncoding, Refusal::BadCertificate),
        (CertificateError::Revoked, Refusal::BadCertificate),
    ] {
        assert_eq!(
            refusal(&Error::InvalidCertificate(error.clone())),
            Some(expected),
            "{error:?}"
        );
    }
    assert_eq!(refusal(&Error::HandshakeNotComplete), None);
}

// ---- the connectors (N3): TLS + WebSocket over an in-memory pipe ----

/// The URL the connector tests open (the host of the test leaf).
const URL: &str = "wss://mediator-1.kourier.invalid/0a/0a1b/";

/// The server half of a test socket.
type ServerSocket = WebSocketStream<tokio_rustls::server::TlsStream<DuplexStream>>;

/// What [`connect_over`] gives the client.
type Client = (Box<dyn WsSender>, async_channel::Receiver<WsEvent>);

/// The client's clock in the handshake tests: a fixed time (decision M7).
#[derive(Debug)]
struct FixedTime(u64);

impl TimeProvider for FixedTime {
    fn current_time(&self) -> Option<UnixTime> {
        Some(at(self.0))
    }
}

/// Both client configurations over the test CA and `pins`, at `time`.
fn configs(pins: PinSet, time: u64) -> TlsConfigs {
    TlsConfigs::new(
        Arc::new(verifier(pins)),
        Arc::new(rustls::crypto::ring::default_provider()),
        Arc::new(FixedTime(time)),
    )
    .expect("the configurations")
}

/// The in-process server: the test leaf and its key.
fn server_config() -> Arc<ServerConfig> {
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certs::LEAF_KEY));
    Arc::new(
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("the server's versions")
            .with_no_client_auth()
            .with_single_cert(vec![CertificateDer::from(certs::LEAF)], key)
            .expect("the server's certificate"),
    )
}

/// The connector against the in-process server over a duplex pipe: the client's result and the
/// server's socket (`None` when the server never saw a WebSocket).
async fn open_with(configs: &TlsConfigs) -> (Result<Client, NetError>, Option<ServerSocket>) {
    open_kind_with(configs, WsKind::Mediator).await
}

/// [`open_with`] for a socket of `kind` (its message limit, decision C6).
async fn open_kind_with(
    configs: &TlsConfigs,
    kind: WsKind,
) -> (Result<Client, NetError>, Option<ServerSocket>) {
    let (client_io, server_io) = tokio::io::duplex(1 << 20);
    let acceptor = TlsAcceptor::from(server_config());
    let server = tokio::spawn(async move {
        let tls = acceptor.accept(server_io).await.ok()?;
        tokio_tungstenite::accept_async(tls).await.ok()
    });
    let client = connect_over(client_io, URL, kind, configs.ws.clone()).await;
    let server = server.await.expect("the server task");
    (client, server)
}

/// An open socket: the test pins, the fixed time.
async fn open() -> (
    Box<dyn WsSender>,
    async_channel::Receiver<WsEvent>,
    ServerSocket,
) {
    open_kind(WsKind::Mediator).await
}

/// An open socket of `kind`: the test pins, the fixed time.
async fn open_kind(
    kind: WsKind,
) -> (
    Box<dyn WsSender>,
    async_channel::Receiver<WsEvent>,
    ServerSocket,
) {
    let (client, server) = open_kind_with(&configs(test_pins(), T_S), kind).await;
    let Ok((sender, events)) = client else {
        panic!("the client did not connect");
    };
    (sender, events, server.expect("the server accepted"))
}

/// The next event of the client.
async fn event(events: &async_channel::Receiver<WsEvent>) -> WsEvent {
    events.recv().await.expect("an event")
}

/// The code of the next message of the server, which must be a close frame.
async fn close_code(server: &mut ServerSocket) -> u16 {
    match server.next().await {
        Some(Ok(Message::Close(Some(frame)))) => frame.code.into(),
        _ => panic!("the server's next message is not a close frame with a code"),
    }
}

/// A binary frame each way; the sender's close reaches the server with its code.
#[tokio::test]
async fn offline_a_binary_frame_goes_both_ways_and_close_sends_the_code() {
    if !in_child() {
        return;
    }
    let (sender, events, mut server) = open().await;
    sender.send(b"up".to_vec()).await.expect("sent");
    let up = server.next().await.expect("a message").expect("read");
    assert_eq!(up, Message::binary(b"up".to_vec()));
    server
        .send(Message::binary(b"down".to_vec()))
        .await
        .expect("sent");
    assert_eq!(event(&events).await, WsEvent::Binary(b"down".to_vec()));
    sender.close(1000, String::new()).await.expect("closed");
    assert_eq!(close_code(&mut server).await, 1000);
}

/// The server's close frame arrives as `Closed` with its code and reason (the actor never shows
/// or logs the reason).
#[tokio::test]
async fn offline_a_server_close_arrives_with_its_code() {
    if !in_child() {
        return;
    }
    let (_sender, events, mut server) = open().await;
    server
        .close(Some(CloseFrame {
            code: CloseCode::from(4113),
            reason: "bye".into(),
        }))
        .await
        .expect("closed");
    assert_eq!(
        event(&events).await,
        WsEvent::Closed {
            code: 4113,
            reason: "bye".to_owned()
        }
    );
}

/// The server goes away without a close frame: `Closed { 1006 }`, the last event.
#[tokio::test]
async fn offline_an_end_without_a_close_frame_is_1006() {
    if !in_child() {
        return;
    }
    let (_sender, events, server) = open().await;
    drop(server);
    assert_eq!(
        event(&events).await,
        WsEvent::Closed {
            code: 1006,
            reason: String::new()
        }
    );
    assert!(events.recv().await.is_err(), "an event after the end");
}

/// A text frame (the mediator sends none): the connector closes with 4010 and reports it.
#[tokio::test]
async fn offline_a_text_frame_closes_with_4010() {
    if !in_child() {
        return;
    }
    let (_sender, events, mut server) = open().await;
    server.send(Message::text("text")).await.expect("sent");
    assert_eq!(
        event(&events).await,
        WsEvent::Closed {
            code: CLOSE_TEXT_FRAME,
            reason: String::new()
        }
    );
    assert_eq!(close_code(&mut server).await, 4010);
}

/// The D2M limit: a message of 65,536 bytes is taken, one of 65,537 closes the socket with 1009.
#[tokio::test]
async fn offline_the_largest_message_is_taken_and_one_byte_more_closes_1009() {
    if !in_child() {
        return;
    }
    assert_eq!(MAX_MESSAGE, 65_536);
    let (_sender, events, mut server) = open().await;
    server
        .send(Message::binary(vec![7_u8; MAX_MESSAGE]))
        .await
        .expect("sent");
    match event(&events).await {
        WsEvent::Binary(frame) => assert_eq!(frame.len(), MAX_MESSAGE),
        WsEvent::Closed { code, .. } => panic!("the largest message closed the socket ({code})"),
    }
    server
        .send(Message::binary(vec![7_u8; MAX_MESSAGE + 1]))
        .await
        .expect("sent");
    assert_eq!(
        event(&events).await,
        WsEvent::Closed {
            code: 1009,
            reason: String::new()
        }
    );
    assert_eq!(close_code(&mut server).await, 1009);
}

/// C6, the limit by kind: a message of 65,537 bytes (one over the mediator's limit) is taken on a
/// rendezvous socket, and the same message closes a mediator socket with 1009.
#[tokio::test]
async fn offline_a_message_over_the_mediator_limit_is_taken_only_by_a_rendezvous_socket() {
    if !in_child() {
        return;
    }
    let big = vec![7_u8; MAX_MESSAGE + 1];
    let (_sender, events, mut server) = open_kind(WsKind::Rendezvous).await;
    server
        .send(Message::binary(big.clone()))
        .await
        .expect("sent");
    match event(&events).await {
        WsEvent::Binary(frame) => assert_eq!(frame.len(), MAX_MESSAGE + 1),
        WsEvent::Closed { code, .. } => {
            panic!("a rendezvous socket refused a message under its limit ({code})")
        }
    }
    let (_sender, events, mut server) = open_kind(WsKind::Mediator).await;
    server.send(Message::binary(big)).await.expect("sent");
    assert_eq!(
        event(&events).await,
        WsEvent::Closed {
            code: 1009,
            reason: String::new()
        }
    );
    assert_eq!(close_code(&mut server).await, 1009);
    assert_eq!(max_message(WsKind::Mediator), 65_536);
    assert_eq!(max_message(WsKind::Rendezvous), 100 * 1_048_576 + 4);
}

/// C6: one byte over the rendezvous limit closes the socket with 1009. The server writes only the
/// header of a binary frame that claims `MAX_RENDEZVOUS_MESSAGE + 1` bytes (tungstenite checks the
/// limit at the header), so no 100 MiB are moved.
#[tokio::test]
async fn offline_one_byte_over_the_rendezvous_limit_closes_1009() {
    if !in_child() {
        return;
    }
    let (_sender, events, mut server) = open_kind(WsKind::Rendezvous).await;
    let length = u64::try_from(MAX_RENDEZVOUS_MESSAGE + 1).expect("the limit fits u64");
    // FIN + binary, no mask, the 64-bit length form.
    let mut header = vec![0x82_u8, 127];
    header.extend_from_slice(&length.to_be_bytes());
    let tls = server.get_mut();
    tls.write_all(&header).await.expect("the header is written");
    tls.flush().await.expect("flushed");
    assert_eq!(
        event(&events).await,
        WsEvent::Closed {
            code: 1009,
            reason: String::new()
        }
    );
}

/// S8: the queue between the read pump and the actor is two deep on a rendezvous socket (messages of
/// up to 100 MiB) and 64 deep on a mediator socket: the server sends ten more small messages than the
/// depth while nobody reads, and exactly `depth` of them wait in the queue (the pump holds one more
/// and stops reading; the rest stays in the pipe).
#[tokio::test]
async fn offline_the_rendezvous_queue_is_two_deep_and_the_mediators_64() {
    if !in_child() {
        return;
    }
    assert_eq!(event_depth(WsKind::Rendezvous), 2);
    assert_eq!(event_depth(WsKind::Mediator), 64);
    for kind in [WsKind::Rendezvous, WsKind::Mediator] {
        let depth = event_depth(kind);
        let (_sender, events, mut server) = open_kind(kind).await;
        assert_eq!(events.capacity(), Some(depth), "the queue's bound");
        for i in 0..depth + 10 {
            let byte = u8::try_from(i).expect("fits");
            server
                .send(Message::binary(vec![byte]))
                .await
                .expect("sent");
        }
        // The pump runs on this runtime: give it every chance to read as far as it may.
        for _ in 0..2_000 {
            tokio::task::yield_now().await;
        }
        assert_eq!(events.len(), depth, "the queue holds more than its depth");
    }
}

/// The whole TLS handshake with a pin set that lacks the leaf: the connector fails UNTRUSTED (pin
/// mismatch) and the server never sees a WebSocket.
#[tokio::test]
async fn offline_a_handshake_against_the_pins_fails_untrusted_and_reads_no_frame() {
    if !in_child() {
        return;
    }
    let others = PinSet::new("kourier.invalid", vec![certs::OTHER_SPKI_SHA256]);
    let (client, server) = open_with(&configs(others, T_S)).await;
    assert_eq!(
        client.err(),
        Some(NetError::Untrusted(UntrustedKind::PinMismatch))
    );
    assert!(server.is_none(), "the server saw a WebSocket");
    // Threema's own pins refuse the test leaf the same way.
    let production = PinSet::new("kourier.invalid", PinSet::production().hashes().to_vec());
    let (client, server) = open_with(&configs(production, T_S)).await;
    assert_eq!(
        client.err(),
        Some(NetError::Untrusted(UntrustedKind::PinMismatch))
    );
    assert!(server.is_none(), "the server saw a WebSocket");
}

/// The client's clock one second after the leaf's `notAfter`: a DATE error (transient), not an
/// untrusted one; the time comes from the configuration's time provider (decision M7).
#[tokio::test]
async fn offline_a_handshake_after_not_after_is_a_date_error() {
    if !in_child() {
        return;
    }
    let (client, server) = open_with(&configs(test_pins(), certs::NOT_AFTER_S + 1)).await;
    assert_eq!(client.err(), Some(NetError::CertificateDates));
    assert!(server.is_none(), "the server saw a WebSocket");
    let (client, server) = open_with(&configs(test_pins(), certs::NOT_AFTER_S)).await;
    assert!(client.is_ok() && server.is_some(), "refused at notAfter");
}

/// Decision M8: the WebSocket's configuration offers no ALPN (no `h2`), the HTTPS client's
/// `h2` and `http/1.1`; production's the same.
#[test]
fn offline_the_websocket_offers_no_h2_and_https_offers_h2_and_http1() {
    if !in_child() {
        return;
    }
    let production = TlsConfigs::production().expect("the production configurations");
    for configs in [configs(test_pins(), T_S), production] {
        assert!(configs.ws.alpn_protocols.is_empty());
        assert_eq!(
            configs.https.alpn_protocols,
            [b"h2".to_vec(), b"http/1.1".to_vec()]
        );
    }
    assert_eq!(HTTPS_ALPN, [&b"h2"[..], &b"http/1.1"[..]]);
}

/// Decision M10: the WebSocket dials only `wss://` to a host one label under the pinned domain;
/// the check is pure (it runs before any DNS lookup in `LiveWs::connect`).
#[test]
fn offline_the_websocket_dials_only_wss_to_a_pinned_host() {
    if !in_child() {
        return;
    }
    let pins = PinSet::production();
    let pinned = ["mediator-0", ".threema.ch"].concat();
    assert_eq!(
        pinned_target(&format!("wss://{pinned}/0a/0a1b/"), "wss", &pins),
        Ok((pinned.clone(), 443))
    );
    assert_eq!(
        pinned_target(&format!("wss://{pinned}:8443/"), "wss", &pins),
        Ok((pinned.clone(), 8443))
    );
    for (url, kind) in [
        (format!("ws://{pinned}/"), UntrustedKind::NotTls),
        (format!("https://{pinned}/"), UntrustedKind::NotTls),
        (format!("wss://a.{pinned}/"), UntrustedKind::UnpinnedHost),
        (
            format!("wss://{pinned}@example.invalid/"),
            UntrustedKind::UnpinnedHost,
        ),
        (
            "wss://example.invalid/".to_owned(),
            UntrustedKind::UnpinnedHost,
        ),
        ("wss://127.0.0.1/".to_owned(), UntrustedKind::UnpinnedHost),
        ("wss://[::1]/".to_owned(), UntrustedKind::UnpinnedHost),
        ("not a url".to_owned(), UntrustedKind::UnpinnedHost),
    ] {
        assert_eq!(
            pinned_target(&url, "wss", &pins),
            Err(NetError::Untrusted(kind)),
            "{url}"
        );
    }
}

/// `LiveHttps` refuses `http://` and an unpinned host before reqwest sees the request (the hosts
/// are loopback literals, so even without the check nothing would leave this namespace); the
/// client itself is made (reqwest took the preconfigured rustls configuration).
#[tokio::test]
async fn offline_https_refuses_plain_http_and_unpinned_hosts_before_any_io() {
    if !in_child() {
        return;
    }
    let client = LiveHttps::new(&configs(test_pins(), T_S).https, test_pins());
    assert!(client.is_ready(), "reqwest refused the TLS configuration");
    // The production TLS configuration builds a client too (with the TEST pins: rule 11 never
    // builds a connector that covers Threema's hosts).
    let production = TlsConfigs::production().expect("the production configurations");
    assert!(LiveHttps::new(&production.https, test_pins()).is_ready());
    for (url, kind) in [
        ("http://mediator-1.kourier.invalid/", UntrustedKind::NotTls),
        ("https://127.0.0.1/", UntrustedKind::UnpinnedHost),
        ("https://[::1]:8443/", UntrustedKind::UnpinnedHost),
        (
            "https://mediator-1.kourier.invalid@127.0.0.1/",
            UntrustedKind::UnpinnedHost,
        ),
    ] {
        let result = client
            .execute(HttpsRequest {
                method: HttpsMethod::Get,
                url: url.to_owned(),
                headers: Vec::new(),
                body: Vec::new(),
                max_body: None,
            })
            .await;
        assert_eq!(result.err(), Some(NetError::Untrusted(kind)), "{url}");
    }
}

/// TH3 fix S-M2 (security review S3): `LiveWs::connect` checks the target BEFORE any DNS lookup or
/// TCP connect. Each refused URL ends in `Untrusted` (a connector that dialed first would answer
/// `Failed("websocket: tcp")` instead, and a `wss` check written as another scheme would let the
/// `https://` URL through to the dial); the URLs are those a hostile mediator URL could be: another
/// scheme, a host outside the pinned domain, a userinfo trick, an IP literal.
#[tokio::test]
async fn offline_the_websocket_connector_refuses_a_bad_target_before_any_dial() {
    if !in_child() {
        return;
    }
    let ws = LiveWs::new(configs(test_pins(), T_S).ws, test_pins());
    for (url, kind) in [
        ("ws://mediator-1.kourier.invalid/", UntrustedKind::NotTls),
        ("https://mediator-1.kourier.invalid/", UntrustedKind::NotTls),
        ("wss://example.invalid/", UntrustedKind::UnpinnedHost),
        (
            "wss://mediator-1.kourier.invalid@example.invalid/",
            UntrustedKind::UnpinnedHost,
        ),
        ("wss://127.0.0.1/", UntrustedKind::UnpinnedHost),
        ("wss://[::1]:8443/", UntrustedKind::UnpinnedHost),
    ] {
        let result = ws
            .connect(WsRequest {
                url: url.to_owned(),
                kind: WsKind::Mediator,
            })
            .await;
        assert_eq!(result.err(), Some(NetError::Untrusted(kind)), "{url}");
    }
}

/// TH3 fix S-L1 (security review S5): the read pump ends with its owner. After the sender and the
/// event receiver are dropped, the server's socket ends on its own, with no frame from the server
/// (a silent peer would otherwise keep the pump, and so the socket, until the kernel gave up). The
/// 5 s is a failure BOUND on the wall clock, not a measure.
#[tokio::test]
async fn offline_the_read_pump_ends_with_its_sender() {
    if !in_child() {
        return;
    }
    let (sender, events, mut server) = open().await;
    drop(sender);
    drop(events);
    let seen = tokio::time::timeout(Duration::from_secs(5), server.next()).await;
    assert!(
        matches!(seen, Ok(None | Some(Err(_)))),
        "the client's socket stayed open after its owner was gone"
    );
}

/// TH3 fix S-L2 (security review S6): a response body is taken up to its limit and no further. The
/// limit itself is taken, one byte more is `Failed("https: body too large")`, and a body in many
/// chunks stops at the chunk that crosses the line (the stream is not read to its end).
#[tokio::test]
async fn offline_an_https_body_over_its_limit_fails_and_stops_the_read() {
    if !in_child() {
        return;
    }
    use std::sync::atomic::{AtomicUsize, Ordering};

    let too_large = NetError::Failed("https: body too large");
    let response = |body: Vec<u8>| reqwest::Response::from(http::Response::new(body));
    assert_eq!(
        read_body(response(vec![7; 1000]), 1000).await,
        Ok(vec![7; 1000])
    );
    assert_eq!(
        read_body(response(vec![7; 1001]), 1000).await,
        Err(too_large.clone())
    );
    assert_eq!(read_body(response(Vec::new()), 0).await, Ok(Vec::new()));
    assert_eq!(
        read_body(response(vec![7]), 0).await,
        Err(too_large.clone())
    );
    // 1,000 chunks of 100 bytes, a limit of 1,000: ten chunks fit, the eleventh crosses.
    let polled = Arc::new(AtomicUsize::new(0));
    let counter = polled.clone();
    let stream = futures_util::stream::repeat_with(move || {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok::<_, std::io::Error>(vec![1_u8; 100])
    })
    .take(1000);
    let chunked = reqwest::Response::from(http::Response::new(reqwest::Body::wrap_stream(stream)));
    assert_eq!(read_body(chunked, 1000).await, Err(too_large));
    let polled = polled.load(Ordering::SeqCst);
    assert!(
        (11..=12).contains(&polled),
        "the body was read for {polled} chunks of 1000"
    );
    // A chunked body that fits is whole.
    let stream = futures_util::stream::iter([
        Ok::<_, std::io::Error>(vec![1_u8; 500]),
        Ok(vec![1_u8; 500]),
    ]);
    let fits = reqwest::Response::from(http::Response::new(reqwest::Body::wrap_stream(stream)));
    assert_eq!(read_body(fits, 1000).await, Ok(vec![1_u8; 1000]));
}

/// The production servers are libthreema's production configuration, field by field, the chat
/// server's two keys in its order; every URL, its placeholders filled, passes the target check
/// under Threema's pins.
#[test]
fn offline_the_production_params_are_libthreemas_production_config() {
    if !in_child() {
        return;
    }
    use libthreema::common::config::{Config, ConfigEnvironment};
    use libthreema::common::keys::PublicKey;

    let params = super::production_params();
    let config = Config::from(ConfigEnvironment::Production);
    let md = config.multi_device.as_ref().expect("multi-device servers");
    assert_eq!(params.mediator_url, md.mediator_server_url.as_str());
    assert_eq!(params.rendezvous_url, md.rendezvous_server_url.as_str());
    assert_eq!(params.directory_url, config.directory_server_url.as_str());
    assert_eq!(params.blob_url, md.blob_mirror_server.download_url.as_str());
    assert_eq!(config.chat_server_public_keys.len(), 2);
    let keys: Vec<PublicKey> = params
        .chat_server_keys
        .iter()
        .map(|bytes| PublicKey::from(*bytes))
        .collect();
    assert_eq!(keys, config.chat_server_public_keys);

    let pins = PinSet::production();
    for (url, scheme) in [
        (&params.mediator_url, "wss"),
        (&params.rendezvous_url, "wss"),
        (&params.directory_url, "https"),
        (&params.blob_url, "https"),
    ] {
        let filled = fill_placeholders(url);
        assert!(
            pinned_target(&filled, scheme, &pins).is_ok(),
            "a production URL is not on a pinned host"
        );
    }
}

/// Every `{placeholder}` replaced by `0a`.
fn fill_placeholders(url: &str) -> String {
    let mut out = String::new();
    let mut rest = url;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        out.push_str("0a");
        rest = rest[open..]
            .find('}')
            .map_or("", |close| &rest[open + close + 1..]);
    }
    out.push_str(rest);
    out
}

/// A certificate refusal is found where tokio-rustls and reqwest put it (inside an `io::Error`,
/// below a wrapping error); a date error is transient; anything else is the operation's failure.
#[test]
fn offline_a_tls_refusal_is_found_inside_an_io_error() {
    if !in_child() {
        return;
    }
    #[derive(Debug, thiserror::Error)]
    #[error("wrapper")]
    struct Wrapper(#[source] std::io::Error);

    let wrapped = |error: CertificateError| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            Error::InvalidCertificate(error),
        )
    };
    assert_eq!(
        net_error("op", &wrapped(CertificateError::UnknownIssuer)),
        NetError::Untrusted(UntrustedKind::UnknownIssuer)
    );
    assert_eq!(
        net_error("op", &wrapped(CertificateError::Expired)),
        NetError::CertificateDates
    );
    assert_eq!(
        net_error("op", &Wrapper(wrapped(CertificateError::NotValidForName))),
        NetError::Untrusted(UntrustedKind::BadName)
    );
    assert_eq!(
        net_error(
            "op",
            &Error::InvalidCertificate(CertificateError::BadEncoding)
        ),
        NetError::Untrusted(UntrustedKind::BadCertificate)
    );
    assert_eq!(
        net_error("op", &std::io::Error::other("plain")),
        NetError::Failed("op")
    );
    assert_eq!(
        net_error("op", &Error::HandshakeNotComplete),
        NetError::Failed("op")
    );
}
