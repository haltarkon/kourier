//! Coexistence check for architecture §5.3 (step S1): in ONE process, run
//!
//! 1. a libsignal-net TLS handshake (BoringSSL, statically linked) to a local rustls server,
//! 2. a SQLCipher query through `kourier-db` (RustCrypto provider, no OpenSSL),
//! 3. `QSslSocket::supportsSsl()` and one QtNetwork HTTPS GET (Qt's TLS plugin loads the system
//!    `libssl.so.3`),
//! 4. the libsignal handshake and the SQLCipher query again, now with libssl loaded,
//!
//! then print the process PSS. Exits non-zero on any failure. `tests/coexist.rs` runs it and
//! checks that `nm -D` of the binary exports no `SSL_`/`EVP_`/`CRYPTO_` symbol.
//!
//! Nothing leaves 127.0.0.1. The certificates in `fixtures/` are test-only (a P-256 CA and a
//! `localhost`/`127.0.0.1` leaf, valid until 2126), generated with `openssl req`/`x509`.

use kourier_db::DbKey;
use libsignal_net_infra::certs::RootCertificates;
use libsignal_net_infra::host::Host;
use libsignal_net_infra::route::{
    ComposedConnector, ConnectorExt as _, TcpRoute, TlsRoute, TlsRouteFragment,
};
use libsignal_net_infra::tcp_ssl::{StatelessTcp, StatelessTls};
use libsignal_net_infra::{Alpn, OverrideNagleAlgorithm};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::borrow::Cow;
use std::ffi::{CStr, c_char, c_int};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::num::NonZeroU16;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CA_DER: &[u8] = include_bytes!("../fixtures/ca.der");
const LEAF_DER: &[u8] = include_bytes!("../fixtures/leaf.der");
const LEAF_KEY_DER: &[u8] = include_bytes!("../fixtures/leaf.key.der");

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Mirrors `KourierQtResult` in `qt_check.cpp`.
#[repr(C)]
struct QtResult {
    supports_ssl: c_int,
    network_error: c_int,
    http_status: c_int,
    body_len: i64,
    tls_backend: [c_char; 64],
    ssl_library: [c_char; 128],
}

unsafe extern "C" {
    fn kourier_coexist_qt_https(
        port: u16,
        ca_der: *const u8,
        ca_len: usize,
        out: *mut QtResult,
    ) -> c_int;
}

fn main() {
    match run() {
        Ok(()) => println!("coexist: OK"),
        Err(e) => {
            eprintln!("coexist: FAILED: {e}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<(), BoxError> {
    let server = Server::start()?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(libsignal_handshake(server.addr))?;
    println!("libsignal-net TLS (BoringSSL): handshake + GET ok");
    sqlcipher_check()?;
    println!("SQLCipher: query ok, no plaintext header, wrong key refused");

    let qt = qt_https(server.addr.port())?;
    println!(
        "QtNetwork: supportsSsl = true, backend {}, {}, HTTPS GET 200 ({} bytes)",
        qt.0, qt.1, qt.2
    );
    // The point of the check: the system OpenSSL is mapped next to the static BoringSSL.
    let libssl = loaded_libssl()?.ok_or("Qt's TLS backend did not load libssl.so")?;
    println!("libssl loaded by Qt: {libssl}");

    rt.block_on(libsignal_handshake(server.addr))?;
    sqlcipher_check()?;
    println!("libsignal-net TLS and SQLCipher still ok with libssl loaded");

    let handshakes = server.handshakes.load(Ordering::SeqCst);
    if handshakes != 3 {
        return Err(format!("server saw {handshakes} TLS handshakes, expected 3").into());
    }
    let (pss, rss) = memory_kb()?;
    println!("PSS: {pss} kB (RSS {rss} kB)");
    Ok(())
}

/// libsignal-net's own connector stack: TCP, then BoringSSL with the test CA as the only root.
async fn libsignal_handshake(addr: SocketAddr) -> Result<(), BoxError> {
    let port = NonZeroU16::new(addr.port()).ok_or("port 0")?;
    let route = TlsRoute {
        fragment: TlsRouteFragment {
            root_certs: RootCertificates::FromDer(Cow::Borrowed(CA_DER)),
            sni: Host::Domain("localhost".into()),
            alpn: Some(Alpn::Http1_1),
            min_protocol_version: None,
        },
        inner: TcpRoute {
            address: addr.ip(),
            port,
            override_nagle_algorithm: OverrideNagleAlgorithm::UseSystemDefault,
        },
    };
    let connector = ComposedConnector::<StatelessTls, StatelessTcp>::default();
    let mut stream =
        tokio::time::timeout(Duration::from_secs(10), connector.connect(route, "coexist"))
            .await
            .map_err(|_| "libsignal TLS connect timed out")?
            .map_err(|e| format!("libsignal TLS connect: {e}"))?;
    stream
        .write_all(b"GET /libsignal HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    if !response.starts_with(b"HTTP/1.1 200 OK") {
        return Err("libsignal TLS: unexpected response".into());
    }
    Ok(())
}

fn sqlcipher_check() -> Result<(), BoxError> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("coexist.db");
    let key = DbKey::from([0x42; 32]);
    {
        let conn = kourier_db::open(&path, &key)?;
        conn.execute_batch("CREATE TABLE t(x TEXT); INSERT INTO t VALUES ('coexist');")?;
        let x: String = conn.query_row("SELECT x FROM t", [], |r| r.get(0))?;
        let provider: String = conn.query_row("PRAGMA cipher_provider", [], |r| r.get(0))?;
        if x != "coexist" || provider != "RustCrypto" {
            return Err(format!("SQLCipher: got {x:?} from provider {provider:?}").into());
        }
        kourier_db::checkpoint_truncate(&conn)?;
    }
    let header = std::fs::read(&path)?;
    if header.starts_with(b"SQLite format 3\0") {
        return Err("SQLCipher wrote a plaintext header".into());
    }
    match kourier_db::open(&path, &DbKey::from([0x43; 32])) {
        Err(kourier_db::Error::WrongKey) => Ok(()),
        Err(e) => Err(format!("wrong key gave {e}").into()),
        Ok(_) => Err("wrong key opened the database".into()),
    }
}

fn qt_https(port: u16) -> Result<(String, String, i64), BoxError> {
    // SAFETY: all-zero is a valid `QtResult` (integers and C strings).
    let mut out: QtResult = unsafe { std::mem::zeroed() };
    // SAFETY: CA_DER is valid for `len` bytes, `out` is a valid exclusive pointer, and the shim
    // only writes within `QtResult`; it runs on the main thread like a QApplication would.
    let rc = unsafe { kourier_coexist_qt_https(port, CA_DER.as_ptr(), CA_DER.len(), &mut out) };
    // SAFETY: the shim NUL-terminates both buffers (or leaves them zeroed).
    let backend = unsafe { CStr::from_ptr(out.tls_backend.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    // SAFETY: as above.
    let library = unsafe { CStr::from_ptr(out.ssl_library.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    if out.supports_ssl == 0 {
        return Err(format!("QSslSocket::supportsSsl() is false (backend {backend:?})").into());
    }
    if rc != 0 {
        return Err(format!(
            "QtNetwork HTTPS GET failed: network error {}, HTTP {}",
            out.network_error, out.http_status
        )
        .into());
    }
    Ok((backend, library, out.body_len))
}

/// The `libssl.so*` mapped into this process, if any.
fn loaded_libssl() -> Result<Option<String>, BoxError> {
    let maps = std::fs::read_to_string("/proc/self/maps")?;
    Ok(maps
        .lines()
        .filter_map(|l| l.split_whitespace().nth(5))
        .find(|p| {
            p.rsplit('/')
                .next()
                .is_some_and(|f| f.starts_with("libssl.so"))
        })
        .map(str::to_owned))
}

/// (PSS, RSS) of this process in kB, from `/proc/self/smaps_rollup`.
fn memory_kb() -> Result<(u64, u64), BoxError> {
    let rollup = std::fs::read_to_string("/proc/self/smaps_rollup")?;
    let field = |name: &str| {
        rollup
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|n| n.parse::<u64>().ok())
            .ok_or(format!("{name} missing from smaps_rollup"))
    };
    Ok((field("Pss:")?, field("Rss:")?))
}

/// A local HTTPS/1.1 server (rustls + ring) that answers every request with 200.
struct Server {
    addr: SocketAddr,
    handshakes: Arc<AtomicUsize>,
}

impl Server {
    fn start() -> Result<Self, BoxError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(
                vec![
                    CertificateDer::from(LEAF_DER.to_vec()),
                    CertificateDer::from(CA_DER.to_vec()),
                ],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(LEAF_KEY_DER.to_vec())),
            )?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let config = Arc::new(config);
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let addr = listener.local_addr()?;
        let handshakes = Arc::new(AtomicUsize::new(0));
        let counter = handshakes.clone();
        std::thread::Builder::new()
            .name("tls-server".into())
            .spawn(move || {
                for tcp in listener.incoming().flatten() {
                    let config = config.clone();
                    let counter = counter.clone();
                    std::thread::spawn(move || {
                        if let Err(e) = serve(config, tcp, &counter) {
                            eprintln!("tls-server: {e}");
                        }
                    });
                }
            })?;
        Ok(Self { addr, handshakes })
    }
}

fn serve(
    config: Arc<rustls::ServerConfig>,
    mut tcp: std::net::TcpStream,
    handshakes: &AtomicUsize,
) -> Result<(), BoxError> {
    tcp.set_read_timeout(Some(Duration::from_secs(15)))?;
    let mut conn = rustls::ServerConnection::new(config)?;
    while conn.is_handshaking() {
        conn.complete_io(&mut tcp)?;
    }
    handshakes.fetch_add(1, Ordering::SeqCst);
    let mut tls = rustls::Stream::new(&mut conn, &mut tcp);
    let mut request = Vec::new();
    let mut buf = [0u8; 1024];
    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = tls.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        request.extend_from_slice(&buf[..n]);
    }
    let body = b"kourier coexist\n";
    write!(
        tls,
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    tls.write_all(body)?;
    tls.flush()?;
    conn.send_close_notify();
    conn.complete_io(&mut tcp)?;
    Ok(())
}
