//! The fake's async shell (`support/mock/connector.rs`) and its script layer (`script.rs`): the
//! seam TH3's actor will use. The REAL patched `D2mProtocol` + `CspProtocol` run over the crate's
//! own `WsConnector`/`WsSender` traits on tokio's PAUSED clock (no wall time, no actor, so no
//! `unshare`): post-handshake, the CSP login and echo both ways; every close code of D8 as
//! `WsEvent::Closed` (proof 8's last leg); idle 4013 on the paused clock (proof 9); unknown hosts
//! and HTTPS refused and counted; `world.parts()`.

use std::time::Duration;

use kourier_threema::close::{LoggedOut, Stop, Verdict, verdict};
use kourier_threema::{
    HttpsClient, HttpsMethod, HttpsRequest, NetError, WsConnector, WsEvent, WsKind, WsRequest,
    WsSender,
};
use libthreema::common::config::MediatorServerBaseUrl;
use libthreema::common::keys::{ClientKey, DeviceGroupKey, PublicKey};
use libthreema::common::{ChatServerGroup, ClientInfo, D2xDeviceId, ThreemaId};
use libthreema::csp::payload::{EchoPayload, IncomingPayload, OutgoingPayload};
use libthreema::csp::{CspProtocol, CspProtocolContext, CspProtocolContextInit, CspStateUpdate};
use libthreema::d2m::payload::{IncomingPayload as D2mIncoming, OutgoingPayload as D2mOutgoing};
use libthreema::d2m::{D2mContext, D2mProtocol, D2mStateUpdate};

use super::d2m::{Group, Opts, SERVER_GROUP, group, new_device};
use super::joiner::{qr_text, relay_url, request};
use crate::support::mock::connector::{RENDEZVOUS_URL, host_of};
use crate::support::mock::d2m::{ConnId, DEFAULT_IDLE_TIMEOUT, datagram, kind};
use crate::support::mock::essential::{Spec, essential};
use crate::support::mock::joiner::{JoinStep, JoinerKnobs};
use crate::support::mock::relay::Role;
use crate::support::mock::script::Script;
use crate::support::mock::{MockWorld, TestIdentity, world};

const ME: &str = "KOURIER1";
const LABEL: &str = "Kourier (th2-connector)";
const WITHIN: Duration = Duration::from_secs(5);

/// One device over the connector: the real D2M client, optionally the real CSP client inside it,
/// the WebSocket's two halves, and what each saw.
struct Device {
    d2m: D2mProtocol,
    csp: Option<CspProtocol>,
    tx: Box<dyn WsSender>,
    rx: async_channel::Receiver<WsEvent>,
    online: bool,
    logged_in: bool,
    payloads: Vec<IncomingPayload>,
    /// Every event of the socket, in order (`Binary` by length only).
    events: Vec<WsEvent>,
    closed: Option<u16>,
    ended: bool,
    failed: bool,
}

/// The real D2M client of `o` with the URL template of the fake's PARTS.
fn d2m_client(w: &MockWorld, g: &Group, o: Opts) -> (D2mProtocol, String) {
    let context = D2mContext {
        mediator_server_url: MediatorServerBaseUrl::try_from(w.server_params().mediator_url)
            .expect("the parts' mediator URL is one libthreema takes"),
        device_group_key: DeviceGroupKey::from(g.dgk),
        csp_server_group: ChatServerGroup::try_from(SERVER_GROUP).expect("a server group"),
        device_id: D2xDeviceId(o.device),
        device_slots_exhausted_policy: o.exhausted,
        device_slot_expiration_policy: o.expiration,
        expected_device_slot_state: o.expected,
        client_info: ClientInfo::Libthreema,
        device_label: LABEL.to_owned(),
    };
    D2mProtocol::new(context)
}

/// The real CSP client with the chat server keys of the fake's PARTS.
fn csp_client(w: &MockWorld, me: &TestIdentity) -> (CspProtocol, Vec<u8>) {
    let context = CspProtocolContext::try_from(CspProtocolContextInit {
        permanent_server_keys: w
            .server_params()
            .chat_server_keys
            .into_iter()
            .map(PublicKey::from)
            .collect(),
        user_identity: ThreemaId::try_from(ME).expect("the test's ID"),
        client_key: ClientKey::from(me.ck.secret),
        client_info: ClientInfo::Libthreema,
        device_cookie: None,
        csp_device_id: None,
    })
    .expect("a CSP context");
    let (proto, hello) = CspProtocol::new(context);
    (proto, hello.0)
}

/// Opens the device's WebSocket through the connector; with `me`, its CSP client-hello goes out
/// at once in a Proxy payload (before the D2M handshake ended, as Desktop does).
async fn open(w: &MockWorld, g: &Group, o: Opts, me: Option<&TestIdentity>) -> Device {
    let (d2m, url) = d2m_client(w, g, o);
    let (tx, rx) = w
        .net()
        .connector()
        .connect(WsRequest {
            url,
            kind: WsKind::Mediator,
        })
        .await
        .expect("the connector opened the mediator's socket");
    let mut d = Device {
        d2m,
        csp: None,
        tx,
        rx,
        online: false,
        logged_in: false,
        payloads: Vec::new(),
        events: Vec::new(),
        closed: None,
        ended: false,
        failed: false,
    };
    if let Some(me) = me {
        let (csp, hello) = csp_client(w, me);
        d.csp = Some(csp);
        d.proxy(hello).await;
    }
    d
}

impl Device {
    /// Sends CSP bytes in one D2M Proxy payload.
    async fn proxy(&mut self, bytes: Vec<u8>) {
        match self.d2m.create_payload(D2mOutgoing::Proxy(bytes)) {
            Ok(i) => {
                let datagram = i.outgoing_datagram.expect("a datagram");
                if self.tx.send(datagram.0).await.is_err() {
                    self.failed = true;
                }
            }
            Err(_) => self.failed = true,
        }
    }

    fn take_event(&mut self, event: WsEvent) {
        match &event {
            WsEvent::Binary(bytes) => {
                if self.closed.is_none() && self.d2m.add_datagrams(vec![bytes.clone()]).is_err() {
                    self.failed = true;
                }
            }
            WsEvent::Closed { code, .. } => self.closed = Some(*code),
        }
        self.events.push(event);
    }

    /// Reads what has arrived and lets both clients answer, until nothing moves. The client
    /// answers the server's echo requests by itself.
    async fn drive(&mut self) {
        loop {
            let mut moved = false;
            loop {
                match self.rx.try_recv() {
                    Ok(event) => self.take_event(event),
                    Err(async_channel::TryRecvError::Closed) => {
                        self.ended = true;
                        break;
                    }
                    Err(async_channel::TryRecvError::Empty) => break,
                }
            }
            let mut proxied = Vec::new();
            while !self.failed && self.closed.is_none() {
                match self.d2m.poll() {
                    Ok(Some(i)) => {
                        if let Some(D2mStateUpdate::PostHandshake(_)) = i.state_update {
                            self.online = true;
                        }
                        if let Some(D2mIncoming::Proxy(bytes)) = i.incoming_payload {
                            proxied.push(bytes);
                        }
                        if let Some(d) = i.outgoing_datagram {
                            if self.tx.send(d.0).await.is_err() {
                                self.failed = true;
                            }
                            moved = true;
                        }
                    }
                    Ok(None) => break,
                    Err(_) => self.failed = true,
                }
            }
            let mut frames = Vec::new();
            if let Some(csp) = self.csp.as_mut() {
                for bytes in &proxied {
                    if csp.add_chunks(&[bytes]).is_err() {
                        self.failed = true;
                    }
                }
                while !self.failed && self.closed.is_none() {
                    match csp.poll() {
                        Ok(Some(i)) => {
                            if let Some(CspStateUpdate::PostHandshake(_)) = i.state_update {
                                self.logged_in = true;
                            }
                            if let Some(IncomingPayload::EchoRequest(q)) = &i.incoming_payload {
                                let answer = csp
                                    .create_payload(&OutgoingPayload::EchoResponse(EchoPayload(
                                        q.0.clone(),
                                    )))
                                    .expect("the client encodes the echo response");
                                frames.extend(answer.outgoing_frame.map(|f| f.0));
                            }
                            if let Some(p) = i.incoming_payload {
                                self.payloads.push(p);
                            }
                            frames.extend(i.outgoing_frame.map(|f| f.0));
                        }
                        Ok(None) => break,
                        Err(_) => self.failed = true,
                    }
                }
            }
            for frame in frames {
                self.proxy(frame).await;
                moved = true;
            }
            if !moved || self.failed {
                return;
            }
        }
    }

    /// Drives until `done` holds, waiting for the socket on tokio's (paused) clock for at most
    /// `within`; whether it held.
    async fn drive_until(&mut self, mut done: impl FnMut(&Self) -> bool, within: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            self.drive().await;
            if done(self) || self.ended || self.failed {
                return done(self);
            }
            match tokio::time::timeout_at(deadline, self.rx.recv()).await {
                Ok(Ok(event)) => self.take_event(event),
                Ok(Err(_)) => self.ended = true,
                Err(_) => return done(self),
            }
        }
    }

    /// Sends one CSP payload of the client.
    async fn send(&mut self, p: OutgoingPayload) {
        let frame = self
            .csp
            .as_mut()
            .expect("a device with a CSP client")
            .create_payload(&p)
            .expect("the client encodes the payload")
            .outgoing_frame
            .expect("a frame");
        self.proxy(frame.0).await;
    }
}

fn scene(seed: u64) -> (MockWorld, Group, TestIdentity) {
    let w = world(seed);
    let me = w.add_identity(ME);
    let g = group(&w);
    (w, g, me)
}

/// THE SEAM: the real D2M and CSP clients reach post-handshake and the CSP login over the
/// connector while the script waits for the device beside them; echo goes both ways (the
/// server's request through the declarative script); the mediator's close reaches the client as
/// `WsEvent::Closed` and ends the stream. Nothing was refused, nothing broke a rule.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_real_clients_log_in_and_echo_through_the_connector() {
    let (w, g, me) = scene(301);
    let net = w.net().clone();
    let mut d = open(&w, &g, new_device(7), Some(&me)).await;
    let (logged_in, conn) = tokio::join!(
        d.drive_until(|d| d.online && d.logged_in, WITHIN),
        net.wait_connected(7, WITHIN)
    );
    assert!(logged_in, "the clients did not log in over the connector");
    let conn = conn.expect("the script saw the device connect");
    assert_eq!(net.opened(), vec![conn], "one socket, the device's");
    assert!(net.mediator().csp_logged_in(conn), "the fake has no login");

    d.send(OutgoingPayload::EchoRequest(EchoPayload(vec![1, 2, 3])))
        .await;
    assert!(
        d.drive_until(|d| !d.payloads.is_empty(), WITHIN).await,
        "no echo response came back"
    );
    let echoed =
        matches!(d.payloads.first(), Some(IncomingPayload::EchoResponse(r)) if r.0 == [1, 2, 3]);
    assert!(
        echoed,
        "the echo response does not carry the request's data"
    );

    let script = Script::new()
        .send("an echo request of the server", |m, c| {
            m.csp_echo_request(c, &[4, 5]);
        })
        .expect("the client's echo response", |m, _| {
            m.csp_echo_responses() == vec![vec![4, 5]]
        })
        .close(4000);
    let (_, result) = tokio::join!(d.drive_until(|d| d.ended, WITHIN), script.run(&net, conn));
    result.expect("the script ran to its end");
    assert!(d.ended, "the stream did not end after the close");
    let last = d.events.last().expect("events");
    assert!(
        matches!(last, WsEvent::Closed { code: 4000, .. }),
        "the last event is not the mediator's close"
    );
    assert_eq!(verdict(4000, None), Verdict::Reconnect { warn: false });
    assert_eq!(net.open_connections(), Vec::<ConnId>::new());
    assert_eq!((net.refused_hosts(), net.refused_https()), (0, 0));
    w.violations().assert_empty();
}

/// Proof 8's last leg: every close code of D8 reaches the client as `WsEvent::Closed{code}`, the
/// last event of its stream, and `close.rs` maps it (the verdicts written out).
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn every_close_code_reaches_the_client_as_a_closed_event() {
    let reconnect = Verdict::Reconnect { warn: false };
    let codes: [(u16, Verdict); 16] = [
        (1000, reconnect),
        (1001, reconnect),
        (1011, reconnect),
        (4000, reconnect),
        (4001, reconnect),
        (4009, reconnect),
        (4010, reconnect),
        (4011, reconnect),
        (4012, reconnect),
        (4013, reconnect),
        (4110, Verdict::Stop(Stop::UpdateApp)),
        (4111, Verdict::Stop(Stop::TooManyDevices)),
        (4112, Verdict::Stop(Stop::Reconnect)),
        (4113, Verdict::LoggedOut(LoggedOut::DroppedByOtherDevice)),
        (4114, Verdict::LoggedOut(LoggedOut::QueueOverflow)),
        (4115, Verdict::LoggedOut(LoggedOut::SlotStateMismatch)),
    ];
    let (w, g, _) = scene(302);
    w.mediator().knobs(|k| k.max_device_slots = 32);
    let net = w.net().clone();
    for (n, (code, want)) in codes.into_iter().enumerate() {
        let device = 100 + u64::try_from(n).expect("small");
        let mut d = open(&w, &g, new_device(device), None).await;
        assert!(d.drive_until(|d| d.online, WITHIN).await, "not online");
        assert!(net.close_device(device, code), "no connection to close");
        assert!(d.drive_until(|d| d.ended, WITHIN).await, "no end");
        let closed = matches!(d.events.last(), Some(WsEvent::Closed { code: c, .. }) if *c == code);
        assert!(closed, "the client did not get {code} as its last event");
        assert_eq!(d.closed, Some(code));
        assert_eq!(verdict(code, None), want, "close.rs maps {code}");
        assert!(
            d.tx.send(vec![0; 4]).await.is_err(),
            "a closed socket took a frame"
        );
    }
    w.violations().assert_empty();
}

/// Proof 9 on tokio's paused clock: `Net::advance` moves the world's clock and tokio's together;
/// one ms short of the idle timeout the socket stands, at it the client gets 4013.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn an_idle_socket_gets_4013_on_the_paused_clock() {
    let (w, g, _) = scene(303);
    let net = w.net().clone();
    let mut d = open(&w, &g, new_device(1), None).await;
    assert!(d.drive_until(|d| d.online, WITHIN).await, "not online");
    let start = tokio::time::Instant::now();
    net.advance(DEFAULT_IDLE_TIMEOUT - Duration::from_millis(1))
        .await;
    d.drive().await;
    assert_eq!(d.closed, None, "closed before the idle timeout");
    net.advance(Duration::from_millis(1)).await;
    assert!(d.drive_until(|d| d.ended, WITHIN).await, "no end");
    assert_eq!(d.closed, Some(4013), "the idle close");
    assert_eq!(
        start.elapsed(),
        DEFAULT_IDLE_TIMEOUT,
        "tokio's clock moved with the world's"
    );
    w.violations().assert_empty();
}

/// The client closing its socket reaches the mediator with its code; the sender refuses after.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_clients_close_reaches_the_mediator() {
    let (w, g, _) = scene(304);
    let net = w.net().clone();
    let mut d = open(&w, &g, new_device(1), None).await;
    assert!(d.drive_until(|d| d.online, WITHIN).await, "not online");
    let conn = net.connected(1).expect("connected");
    d.tx.close(1000, String::new())
        .await
        .expect("the close goes out");
    assert_eq!(net.mediator().closed(conn), Some(1000));
    assert!(
        d.tx.send(vec![0; 4]).await.is_err(),
        "a frame after the close"
    );
    assert!(
        d.tx.close(1000, String::new()).await.is_err(),
        "a second close"
    );
    w.violations().assert_empty();
}

/// Only the mediator's host is served: another host, the rendezvous relay (TH4) and every HTTPS
/// request are REFUSED, counted and recorded; a mediator URL of a group the world does not know
/// fails at the mediator (recorded there).
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn other_hosts_and_https_are_refused_and_counted() {
    let w = world(305);
    let net = w.net().clone();
    let connector = net.connector();
    let refused = |r: Result<_, NetError>| matches!(r, Err(NetError::Refused("websocket")));
    for (url, kind) in [
        ("wss://elsewhere.invalid/ab/cd", WsKind::Mediator),
        ("wss://mediator-1.example/12/cd", WsKind::Mediator),
        (RENDEZVOUS_URL, WsKind::Rendezvous),
        // The mediator's host asked for as the relay is no mediator socket either.
        ("wss://mediator-1.invalid/12/cd", WsKind::Rendezvous),
    ] {
        let r = connector
            .connect(WsRequest {
                url: url.to_owned(),
                kind,
            })
            .await;
        assert!(
            refused(r.map(|_| ())),
            "a host the fake does not serve was opened"
        );
    }
    assert_eq!(net.refused_hosts(), 4);
    let https = net
        .https()
        .execute(HttpsRequest {
            method: HttpsMethod::Get,
            url: "https://directory.invalid/identity/ABCDEFGH".to_owned(),
            headers: Vec::new(),
            body: Vec::new(),
            max_body: None,
        })
        .await;
    assert_eq!(https.map(|_| ()), Err(NetError::Refused("https")));
    assert_eq!(net.refused_https(), 1);
    // The mediator's host, a group nobody added (a wrong device group key).
    let unknown = connector
        .connect(WsRequest {
            url: "wss://mediator-1.invalid/12/00".to_owned(),
            kind: WsKind::Mediator,
        })
        .await;
    assert!(
        matches!(unknown.map(|_| ()), Err(NetError::Failed("websocket"))),
        "an unknown group was opened"
    );
    assert_eq!(net.opened(), Vec::<ConnId>::new(), "a socket was opened");
    assert_eq!(
        w.violations().rules(),
        vec![
            "connect-to-a-host-the-fake-does-not-serve",
            "connect-to-a-host-the-fake-does-not-serve",
            "connect-to-a-host-the-fake-does-not-serve",
            "connect-to-a-host-the-fake-does-not-serve",
            "https-request-the-fake-does-not-serve",
            "mediator-connect-refused",
        ]
    );
}

/// TH3 N6's knob: while `fail_connects` is set, a connect to the mediator's host fails with that
/// error (counted in `attempts`, no socket, no violation); cleared, the same URL opens. A host the
/// fake does not serve is still refused first.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_failing_connector_fails_with_its_error_until_cleared() {
    let (w, g, _me) = scene(308);
    let net = w.net().clone();
    let (_d2m, url) = d2m_client(&w, &g, new_device(1));
    let connector = net.connector();
    let request = || WsRequest {
        url: url.clone(),
        kind: WsKind::Mediator,
    };
    for error in [
        NetError::Failed("websocket"),
        NetError::Untrusted(kourier_threema::UntrustedKind::PinMismatch),
        NetError::CertificateDates,
    ] {
        net.fail_connects(Some(error.clone()));
        let r = connector.connect(request()).await;
        assert_eq!(r.map(|_| ()).err(), Some(error), "the knob's error");
    }
    let other = connector
        .connect(WsRequest {
            url: "wss://elsewhere.invalid/ab/cd".to_owned(),
            kind: WsKind::Mediator,
        })
        .await;
    assert!(matches!(
        other.map(|_| ()),
        Err(NetError::Refused("websocket"))
    ));
    assert_eq!(net.attempts(), 4);
    assert_eq!(net.opened(), Vec::<ConnId>::new(), "a socket was opened");
    net.fail_connects(None);
    assert!(connector.connect(request()).await.is_ok(), "cleared: opens");
    assert_eq!(net.attempts(), 5);
    assert_eq!(net.opened().len(), 1);
    assert_eq!(
        w.violations().rules(),
        vec!["connect-to-a-host-the-fake-does-not-serve"]
    );
}

/// `world.parts()`: the maker is not called by building or cloning the parts; the servers are
/// `.invalid` hosts and the chat server key is the world's SK.public; nothing prints a key.
#[test]
fn the_parts_carry_the_fakes_servers_and_make_nothing_until_asked() {
    let w = world(306);
    let parts = w.parts();
    let _copy = parts.clone();
    assert_eq!(w.net().made(), 0, "building the parts made a client");
    let p = w.server_params();
    for url in [
        &p.mediator_url,
        &p.rendezvous_url,
        &p.directory_url,
        &p.blob_url,
    ] {
        let host = host_of(url).expect("a URL with a host");
        assert!(
            host.ends_with(".invalid"),
            "a server of the parts can resolve"
        );
    }
    assert!(
        p.chat_server_keys == vec![w.server_key().public],
        "the parts' chat server keys"
    );
    assert_eq!(
        format!("{parts:?} {p:?}"),
        "ThreemaParts ServerParams",
        "the parts print a URL or a key"
    );
    w.violations().assert_empty();
}

/// The declarative script names the first step that was not met (on the paused clock, at once).
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_script_names_the_first_step_that_was_not_met() {
    let (w, g, _) = scene(307);
    let net = w.net().clone();
    let mut d = open(&w, &g, new_device(1), None).await;
    assert!(d.drive_until(|d| d.online, WITHIN).await, "not online");
    let conn = net.connected(1).expect("connected");
    let start = tokio::time::Instant::now();
    let result = Script::new()
        .expect("the device is online", |m, c| m.is_online(c))
        .expect("an echo response nobody asked for", |m, _| {
            !m.csp_echo_responses().is_empty()
        })
        .close(1000)
        .run(&net, conn)
        .await;
    assert_eq!(
        result,
        Err("step 2 (expect an echo response nobody asked for) was not met within 5 s".to_owned())
    );
    assert_eq!(start.elapsed(), WITHIN, "the wait ran on the paused clock");
    assert!(
        net.mediator().is_online(conn),
        "a step after the failed one ran"
    );
    w.violations().assert_empty();
}

/// TH4 J2: a `WsKind::Rendezvous` connect to the relay's host opens a relay side as the RID; the
/// joiner that scans the QR of that path is pumped at every flush, so its Hello reaches the
/// client's event channel, and the client's close 1000 is reflected to it.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_rendezvous_url_opens_a_relay_side_and_the_joiner_answers_through_it() {
    let w = world(307);
    let group = w.device_group("KOURIER3");
    let joiner = w.joiner(
        essential(&w, &group, &Spec::default()),
        JoinerKnobs::default(),
    );
    let (ak, path): ([u8; 32], [u8; 32]) = (w.random(), w.random());
    let url = relay_url(&path);
    let (tx, rx) = w
        .net()
        .connector()
        .connect(WsRequest {
            url: url.clone(),
            kind: WsKind::Rendezvous,
        })
        .await
        .expect("the relay's path opened");
    let sides = w.net().relay_opened();
    assert_eq!(sides.len(), 1, "one relay side");
    assert_eq!(
        w.relay().role(sides[0]),
        Some(Role::Rid),
        "the client is the RID"
    );
    joiner
        .scan(&qr_text(&ak, &url, request()))
        .expect("scanned");
    w.net().flush();
    let hello = rx
        .try_recv()
        .expect("the joiner's Hello reached the client");
    assert!(
        matches!(&hello, WsEvent::Binary(b) if b.len() > 4),
        "the Hello is no binary frame"
    );
    assert_eq!(joiner.step(), JoinStep::Scanned, "the joiner's step");
    tx.close(1000, String::new()).await.expect("closed");
    assert_eq!(joiner.closed(), Some(1000), "the close was not reflected");
    assert_eq!(w.net().refused_hosts(), 0, "the relay's host was refused");
    w.violations().assert_empty();
}

/// TH6a S3 (C14): while `stall_mediator_sends` is on, a write to the mediator pends and reaches
/// nothing (the client's own bound has to end it); set back, the pending write goes through. A write
/// that started before and one after the stall are not touched.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_stalled_write_to_the_mediator_pends_until_the_stall_is_lifted() {
    let (w, g, _) = scene(306);
    let net = w.net().clone();
    let mut d = open(&w, &g, new_device(1), None).await;
    assert!(d.drive_until(|d| d.online, WITHIN).await, "not online");
    let proxies = || {
        w.trace()
            .lines()
            .iter()
            .filter(|l| l.contains(" in proxy "))
            .count()
    };
    let frame = datagram(kind::PROXY, &[0x42]);
    d.tx.send(frame.clone()).await.expect("an ordinary write");
    assert_eq!(proxies(), 1, "the ordinary write arrived");

    net.stall_mediator_sends(true);
    let write = d.tx.send(frame.clone());
    tokio::pin!(write);
    let pending = tokio::time::timeout(Duration::from_secs(30), &mut write).await;
    assert!(pending.is_err(), "the write did not pend");
    assert_eq!(net.stalled_mediator_sends(), 1, "the stall was counted");
    assert_eq!(proxies(), 1, "the stalled write reached the mediator");

    net.stall_mediator_sends(false);
    write.await.expect("the lifted write goes through");
    assert_eq!(proxies(), 2, "the pending write arrived");
    d.tx.send(frame).await.expect("a write after the stall");
    assert_eq!(proxies(), 3);
    assert_eq!(net.stalled_mediator_sends(), 1, "no new stall");
    w.violations().assert_empty();
}
