//! The gate's tests (TG-1 plan §5.4 T-g1..T-g10, critic L1, TG-1b B-1): a raw SOCKS5 client on
//! loopback against a gate whose dialer is a counting in-memory echo (`tokio::io::duplex`), so no
//! test dials anything. The addresses that must pass are taken from [`ranges`] at run time: this
//! file names none of Telegram's.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use base64::Engine as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

use super::dialer::{DialFuture, Dialer, GateStream};
use super::{GateHandle, GateOptions, bind_acceptable, ranges, start, start_with_binder};
use crate::test_log::Lines;

/// How long a test waits for one answer of the gate.
const WAIT: Duration = Duration::from_secs(5);

/// A dialer that counts and hands back an in-memory echo; with `fail` it refuses.
#[derive(Default)]
struct EchoDialer {
    dials: AtomicUsize,
    fail: bool,
}

impl Dialer for EchoDialer {
    fn dial(&self, _to: SocketAddr) -> DialFuture {
        self.dials.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Box::pin(std::future::ready(Err(io::Error::from(
                io::ErrorKind::ConnectionRefused,
            ))));
        }
        let (near, far) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let (mut r, mut w) = tokio::io::split(far);
            let _ = tokio::io::copy(&mut r, &mut w).await;
        });
        Box::pin(std::future::ready(
            Ok(Box::new(near) as Box<dyn GateStream>),
        ))
    }
}

fn gate_with(dialer: Arc<EchoDialer>, opts: GateOptions) -> GateHandle {
    start(
        &tokio::runtime::Handle::current(),
        dialer,
        opts,
        CancellationToken::new(),
    )
    .expect("a gate")
}

fn gate(dialer: Arc<EchoDialer>) -> GateHandle {
    gate_with(dialer, GateOptions::default())
}

async fn connect(g: &GateHandle) -> TcpStream {
    TcpStream::connect((Ipv4Addr::LOCALHOST, g.port()))
        .await
        .expect("the gate accepts")
}

async fn read_n(s: &mut TcpStream, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    tokio::time::timeout(WAIT, s.read_exact(&mut buf))
        .await
        .expect("the gate answered in time")
        .expect("the gate answered");
    buf
}

/// Whether the gate closes `s` within `within` (EOF or a reset), reading what it still sends.
async fn closed_within(s: &mut TcpStream, within: Duration) -> bool {
    let mut buf = [0u8; 64];
    let deadline = tokio::time::Instant::now() + within;
    loop {
        match tokio::time::timeout_at(deadline, s.read(&mut buf)).await {
            Err(_) => return false,
            Ok(Ok(0) | Err(_)) => return true,
            Ok(Ok(_)) => {}
        }
    }
}

/// The greeting with these methods; returns the gate's choice.
async fn greet(s: &mut TcpStream, methods: &[u8]) -> u8 {
    let mut msg = vec![5, u8::try_from(methods.len()).expect("few methods")];
    msg.extend_from_slice(methods);
    s.write_all(&msg).await.expect("greeting");
    let got = read_n(s, 2).await;
    assert_eq!(got[0], 5, "not a SOCKS5 answer");
    got[1]
}

/// The RFC 1929 login; returns the status.
async fn send_login(s: &mut TcpStream, user: &[u8], pass: &[u8]) -> u8 {
    let mut msg = vec![1, u8::try_from(user.len()).expect("short")];
    msg.extend_from_slice(user);
    msg.push(u8::try_from(pass.len()).expect("short"));
    msg.extend_from_slice(pass);
    s.write_all(&msg).await.expect("login");
    read_n(s, 2).await[1]
}

/// A connection that is logged in.
async fn logged_in(g: &GateHandle) -> TcpStream {
    let mut s = connect(g).await;
    assert_eq!(greet(&mut s, &[2]).await, 2);
    assert_eq!(
        send_login(&mut s, g.user().as_bytes(), g.pass().as_bytes()).await,
        0
    );
    s
}

/// A request for `to` with command `cmd`; returns REP.
async fn request(s: &mut TcpStream, cmd: u8, to: SocketAddr) -> u8 {
    let mut msg = vec![5, cmd, 0];
    match to.ip() {
        IpAddr::V4(a) => {
            msg.push(1);
            msg.extend_from_slice(&a.octets());
        }
        IpAddr::V6(a) => {
            msg.push(4);
            msg.extend_from_slice(&a.octets());
        }
    }
    msg.extend_from_slice(&to.port().to_be_bytes());
    s.write_all(&msg).await.expect("request");
    let rep = read_n(s, 10).await;
    assert_eq!(rep[0], 5, "not a SOCKS5 reply");
    rep[1]
}

/// An address inside the first IPv4 block (taken at run time).
fn allowed_v4(port: u16) -> SocketAddr {
    let (net, _) = ranges::TELEGRAM_V4[0];
    SocketAddr::new(IpAddr::from(Ipv4Addr::from_bits(net.to_bits() + 1)), port)
}

/// An address inside the first IPv6 block (taken at run time).
fn allowed_v6(port: u16) -> SocketAddr {
    let (net, _) = ranges::TELEGRAM_V6[0];
    SocketAddr::new(IpAddr::from(Ipv6Addr::from_bits(net.to_bits() + 1)), port)
}

/// T-g1: a greeting that offers only "no authentication" gets `FF` and the connection ends.
#[tokio::test]
async fn no_auth_is_refused() {
    let dialer = Arc::new(EchoDialer::default());
    let g = gate(dialer.clone());
    let mut s = connect(&g).await;
    assert_eq!(greet(&mut s, &[0]).await, 0xFF);
    assert!(
        closed_within(&mut s, WAIT).await,
        "the gate kept the connection"
    );
    assert_eq!(g.stop().await.no_login, 1);
    assert_eq!(dialer.dials.load(Ordering::SeqCst), 0);
}

/// Critic L1: TDLib offers BOTH `00` and `02` when it has a login (`Socks5.cpp:21-31`); the gate
/// selects `02`, it does not refuse the greeting.
#[tokio::test]
async fn a_greeting_with_none_and_login_selects_login() {
    let g = gate(Arc::new(EchoDialer::default()));
    for methods in [&[0u8, 2][..], &[2, 0], &[0, 1, 2]] {
        let mut s = connect(&g).await;
        assert_eq!(
            greet(&mut s, methods).await,
            2,
            "the login was not selected"
        );
        assert_eq!(
            send_login(&mut s, g.user().as_bytes(), g.pass().as_bytes()).await,
            0
        );
    }
    assert_eq!(g.stop().await.no_login, 0);
}

/// T-g2: a wrong password (or user name) gets status 1 and is never dialled.
#[tokio::test]
async fn a_wrong_password_is_refused() {
    let dialer = Arc::new(EchoDialer::default());
    let g = gate(dialer.clone());
    let mut wrong_pass = g.pass().as_bytes().to_vec();
    wrong_pass[0] ^= 1;
    let mut wrong_user = g.user().as_bytes().to_vec();
    wrong_user[3] ^= 1;
    let cases: [(&[u8], &[u8]); 4] = [
        (g.user().as_bytes(), &wrong_pass),
        (&wrong_user, g.pass().as_bytes()),
        (g.user().as_bytes(), b""),
        (g.pass().as_bytes(), g.user().as_bytes()),
    ];
    for (user, pass) in cases {
        let mut s = connect(&g).await;
        assert_eq!(greet(&mut s, &[2]).await, 2);
        assert_eq!(
            send_login(&mut s, user, pass).await,
            1,
            "a wrong login passed"
        );
        assert!(
            closed_within(&mut s, WAIT).await,
            "the gate kept the connection"
        );
    }
    assert_eq!(g.stop().await.auth, 4);
    assert_eq!(dialer.dials.load(Ordering::SeqCst), 0);
}

/// T-g3: BIND (2) and UDP ASSOCIATE (3) get REP 07 and no dial.
#[tokio::test]
async fn bind_and_udp_associate_are_refused() {
    let dialer = Arc::new(EchoDialer::default());
    let g = gate(dialer.clone());
    for cmd in [2, 3] {
        let mut s = logged_in(&g).await;
        assert_eq!(request(&mut s, cmd, allowed_v4(443)).await, 7);
    }
    assert_eq!(g.stop().await.command, 2);
    assert_eq!(dialer.dials.load(Ordering::SeqCst), 0);
}

/// T-g4: a domain name gets REP 08 and no dial.
#[tokio::test]
async fn a_domain_name_is_refused() {
    let dialer = Arc::new(EchoDialer::default());
    let g = gate(dialer.clone());
    let mut s = logged_in(&g).await;
    let name = b"example.invalid";
    let mut msg = vec![5, 1, 0, 3, u8::try_from(name.len()).expect("short")];
    msg.extend_from_slice(name);
    msg.extend_from_slice(&443u16.to_be_bytes());
    s.write_all(&msg).await.expect("request");
    assert_eq!(read_n(&mut s, 10).await[1], 8);
    assert_eq!(g.stop().await.address_type, 1);
    assert_eq!(dialer.dials.load(Ordering::SeqCst), 0);
}

/// T-g5: loopback, a public address outside the list and a ULA get REP 02 and are NEVER
/// dialled.
#[tokio::test]
async fn a_destination_outside_the_list_is_refused_and_never_dialled() {
    let dialer = Arc::new(EchoDialer::default());
    let g = gate(dialer.clone());
    let outside: [SocketAddr; 4] = [
        "127.0.0.1:443".parse().expect("addr"),
        "1.1.1.1:443".parse().expect("addr"),
        "[fd00::1]:443".parse().expect("addr"),
        SocketAddr::new(IpAddr::from(ranges::TELEGRAM_V4[0].0.to_ipv6_mapped()), 443),
    ];
    for to in outside {
        let mut s = logged_in(&g).await;
        assert_eq!(request(&mut s, 1, to).await, 2, "an outside address passed");
    }
    let counts = g.stop().await;
    assert_eq!(counts.rule, 4);
    assert_eq!(counts.dials, 0);
    assert_eq!(
        dialer.dials.load(Ordering::SeqCst),
        0,
        "an outside address was dialled"
    );
}

/// T-g6: an allowed address on a port outside {443, 80, 5222} gets REP 02, no dial.
#[tokio::test]
async fn a_port_outside_the_three_is_refused() {
    let dialer = Arc::new(EchoDialer::default());
    let g = gate(dialer.clone());
    for to in [allowed_v4(22), allowed_v6(8443), allowed_v4(53)] {
        let mut s = logged_in(&g).await;
        assert_eq!(request(&mut s, 1, to).await, 2, "an outside port passed");
    }
    assert_eq!(g.stop().await.port, 3);
    assert_eq!(dialer.dials.load(Ordering::SeqCst), 0);
}

/// T-g7: an allowed destination (IPv4 and IPv6, from the list) is dialled once and bytes flow
/// both ways.
#[tokio::test]
async fn an_allowed_destination_is_dialled_once_and_bytes_flow() {
    let dialer = Arc::new(EchoDialer::default());
    let g = gate(dialer.clone());
    for (n, to) in [allowed_v4(443), allowed_v6(5222), allowed_v4(80)]
        .into_iter()
        .enumerate()
    {
        let mut s = logged_in(&g).await;
        assert_eq!(
            request(&mut s, 1, to).await,
            0,
            "an allowed destination failed"
        );
        assert_eq!(
            dialer.dials.load(Ordering::SeqCst),
            n + 1,
            "not dialled once"
        );
        s.write_all(b"ping-").await.expect("write");
        assert_eq!(read_n(&mut s, 5).await, b"ping-");
    }
    let counts = g.stop().await;
    assert_eq!(counts.dials, 3);
    assert_eq!(counts.rule + counts.port, 0);
}

/// A dial that fails answers REP 05 and is counted.
#[tokio::test]
async fn a_failed_dial_is_answered_and_counted() {
    let dialer = Arc::new(EchoDialer {
        fail: true,
        ..EchoDialer::default()
    });
    let g = gate(dialer.clone());
    let mut s = logged_in(&g).await;
    assert_eq!(request(&mut s, 1, allowed_v4(443)).await, 5);
    let counts = g.stop().await;
    assert_eq!((counts.dials, counts.dial_failures), (1, 1));
}

/// T-g8: every start listens on loopback on its own OS port (never 1080) with its own login of 16
/// random bytes each.
#[tokio::test]
async fn each_start_has_its_own_credentials_on_an_os_port() {
    let a = gate(Arc::new(EchoDialer::default()));
    let b = gate(Arc::new(EchoDialer::default()));
    for g in [&a, &b] {
        assert_ne!(g.port(), 1080);
        assert_ne!(g.port(), 0);
        for token in [g.user(), g.pass()] {
            let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(token)
                .expect("base64url");
            assert_eq!(raw.len(), 16, "a credential is not 16 bytes");
            assert!((1..=127).contains(&token.len()), "outside TDLib's limits");
        }
        assert_ne!(g.user(), g.pass());
        // On loopback: it answers there.
        let mut s = connect(g).await;
        assert_eq!(greet(&mut s, &[2]).await, 2);
    }
    assert_ne!(a.port(), b.port());
    assert_ne!(a.user(), b.user());
    assert_ne!(a.pass(), b.pass());
    assert_eq!(format!("{a:?}"), "GateHandle");
}

/// G6: the gate's port is never a data-centre port or below 1024 (Landlock names a port, never an
/// address): the bind goes again while the system's pick is not acceptable, and keeps the rejected
/// listeners open until one passes, so the same port cannot come back. The list the helper refuses
/// is the list the gate forwards to.
#[test]
fn the_gate_binds_again_while_the_port_is_not_acceptable() {
    let bind = std::cell::RefCell::new(Vec::new());
    let calls = std::cell::Cell::new(0usize);
    let listener = bind_acceptable(
        || {
            let l = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
            bind.borrow_mut().push(l.local_addr()?.port());
            Ok(l)
        },
        // The first three ports are "data-centre ports" for this test.
        |_| {
            calls.set(calls.get() + 1);
            calls.get() > 3
        },
    )
    .expect("an acceptable port");
    let tried = bind.borrow();
    assert_eq!(tried.len(), 4, "the bind did not go again three times");
    assert_eq!(listener.local_addr().expect("addr").port(), tried[3]);
    let distinct: std::collections::BTreeSet<_> = tried.iter().collect();
    assert_eq!(distinct.len(), 4, "a rejected port came back");

    // Nothing acceptable: the bind gives up (it does not loop for ever).
    let refused = bind_acceptable(
        || std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)),
        |_| false,
    );
    assert_eq!(
        refused.err().map(|e| e.kind()),
        Some(io::ErrorKind::AddrNotAvailable)
    );

    // The rule itself: what the system hands out passes, the three data-centre ports and the
    // privileged ports do not; the forwarded ports are the refused ones.
    assert_eq!(
        ranges::PORTS,
        kourier_tdlib::protocol::DC_PORTS,
        "the gate and the helper name different data-centre ports"
    );
    for port in [443, 80, 5222, 0, 1, 1023] {
        assert!(
            !kourier_tdlib::protocol::gate_port_allowed(port),
            "port {port} may be a gate's"
        );
    }
    for port in [1024, 32768, 40001, 60999, 65535] {
        assert!(
            kourier_tdlib::protocol::gate_port_allowed(port),
            "port {port} may not be a gate's"
        );
    }
}

/// T-g9: what the gate logs is counts: no address, no port, no credential.
#[tokio::test]
async fn the_log_holds_counts_only() {
    let lines = Lines::default();
    let _guard = tracing::subscriber::set_default(lines.clone());
    let g = gate(Arc::new(EchoDialer::default()));
    let port = g.port().to_string();
    let (user, pass) = (g.user().to_owned(), g.pass().to_owned());
    let ok = allowed_v4(443);
    let mut s = logged_in(&g).await;
    assert_eq!(request(&mut s, 1, ok).await, 0);
    s.write_all(b"x").await.expect("write");
    let _ = read_n(&mut s, 1).await;
    drop(s);
    let mut s = logged_in(&g).await;
    let outside: SocketAddr = "1.1.1.1:443".parse().expect("addr");
    assert_eq!(request(&mut s, 1, outside).await, 2);
    g.stop().await;
    let all = lines.all();
    assert!(
        all.iter().any(|l| l.contains("telegram gate counts")),
        "the gate logged no counts"
    );
    for line in &all {
        for secret in [
            &port,
            &user,
            &pass,
            &ok.ip().to_string(),
            &"1.1.1.1".to_owned(),
        ] {
            assert!(
                !line.contains(secret.as_str()),
                "a log line holds connection data"
            );
        }
        assert!(!line.contains("443"), "a log line holds a port");
    }
}

/// Critic L1: a connection that does not finish its handshake in time is closed and counted.
#[tokio::test]
async fn a_silent_connection_is_closed_after_the_handshake_timeout() {
    let g = gate_with(
        Arc::new(EchoDialer::default()),
        GateOptions {
            handshake_timeout: Duration::from_millis(200),
            ..GateOptions::default()
        },
    );
    let mut s = connect(&g).await;
    // Half a greeting, then nothing.
    s.write_all(&[5]).await.expect("write");
    assert!(
        closed_within(&mut s, Duration::from_secs(3)).await,
        "a silent connection was kept"
    );
    assert_eq!(g.stop().await.timed_out, 1);
}

/// Critic L1: at most `max_pending` connections may be in their handshake; one more is closed at
/// once, and a freed slot is usable again.
#[tokio::test]
async fn connections_beyond_the_pending_cap_are_closed_at_once() {
    let g = gate_with(
        Arc::new(EchoDialer::default()),
        GateOptions {
            max_pending: 2,
            handshake_timeout: Duration::from_secs(30),
            ..GateOptions::default()
        },
    );
    let first = connect(&g).await;
    let _second = connect(&g).await;
    let until = tokio::time::Instant::now() + WAIT;
    while g.counts().accepted < 2 {
        assert!(
            tokio::time::Instant::now() < until,
            "the gate did not accept"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut third = connect(&g).await;
    assert!(
        closed_within(&mut third, Duration::from_secs(2)).await,
        "a connection beyond the cap was kept"
    );
    assert_eq!(g.counts().busy, 1);
    // Freeing a slot: the next one gets through.
    drop(first);
    let until = tokio::time::Instant::now() + WAIT;
    while g.counts().protocol < 1 {
        assert!(
            tokio::time::Instant::now() < until,
            "the slot was not freed"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let _fourth = logged_in(&g).await;
    assert_eq!(g.stop().await.busy, 1);
}

/// The variable that makes [`gate_port_child`] run.
const GATE_PORT_CHILD_ENV: &str = "KOURIER_TG_GATE_PORT_CHILD";

/// TG-1b LOW D1 (the survivor of the re-check: `start`'s filter replaced by `|_| true` passed every
/// test): in the offline child (an empty network namespace, so the port is free and nothing else
/// listens), the binder hands `127.0.0.1:5222` (a data-centre port) first: the gate refuses it and
/// binds again, and its port is the second, acceptable one. SKIPPED where `unshare -rn` is missing,
/// a failure then with `KOURIER_REQUIRE_UNSHARE=1`.
#[test]
fn the_gate_never_takes_a_data_centre_port() {
    if !kourier_tdlib::spawn::offline_available() {
        assert!(
            !std::env::var("KOURIER_REQUIRE_UNSHARE").is_ok_and(|v| v == "1"),
            "`unshare -rn` (or `ip`) is unavailable and KOURIER_REQUIRE_UNSHARE=1"
        );
        println!("SKIPPED: `unshare -rn` or `ip` is unavailable");
        return;
    }
    let run = kourier_tdlib::spawn::run_self_offline(
        "gate::tests::gate_port_child",
        &[(GATE_PORT_CHILD_ENV, "1")],
        Duration::from_secs(60),
    )
    .expect("start the offline child");
    assert!(
        run.status.success() && run.output.contains("1 passed"),
        "the offline child failed ({:?}):\n{}",
        run.status,
        run.output
    );
}

/// The offline child's half of [`the_gate_never_takes_a_data_centre_port`].
#[test]
fn gate_port_child() {
    if std::env::var_os(GATE_PORT_CHILD_ENV).is_none() {
        return;
    }
    assert!(
        kourier_tdlib::spawn::in_offline_child(),
        "not in the offline child"
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let calls = std::cell::Cell::new(0usize);
    let gate = start_with_binder(
        rt.handle(),
        Arc::new(EchoDialer::default()),
        GateOptions::default(),
        CancellationToken::new(),
        || {
            calls.set(calls.get() + 1);
            let port = if calls.get() == 1 { 5222 } else { 0 };
            std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        },
    )
    .expect("a gate");
    assert_eq!(
        calls.get(),
        2,
        "the gate did not bind again after the data-centre port"
    );
    assert_ne!(gate.port(), 5222, "the gate listens on a data-centre port");
    assert!(kourier_tdlib::protocol::gate_port_allowed(gate.port()));
    rt.block_on(gate.stop());
}
