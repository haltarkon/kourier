//! tg-fakedc: the lane's fake Telegram data centre for TDLib (TD-4a; carried over from the tdlib-t0
//! spike, 2026-10-16). TEST ONLY. It speaks the obfuscated2 transport, the real MTProto auth-key
//! handshake with OUR test key, the MTProto 2.0 encrypted session and a handful of API answers.
//!
//! - `tg-fakedc keygen --out <dir>`: the two keys of the test-only library ([`keys`]).
//! - `tg-fakedc serve --key <test-key.pem> --port-file <file> [--wire-log <file>]
//!   [--control <socket>] [--world-json <file>]`: listens on
//!   `127.0.0.1` and a port the kernel chooses, writes the port to `--port-file` (complete or not
//!   at all), and answers every connection. Each connection starts with the test dialer's line
//!   `KFDC1 <ip> <port>`: the fake answers as the DC that was dialled. It is started only inside
//!   the offline namespace (`kourier_tdlib::spawn::spawn_offline_companion`, which also sets its
//!   parent-death signal); it prints nothing but errors without values. `--control` serves the
//!   control socket ([`control`]: the world, pushes, drops, the QR scan, marks); `--world-json`
//!   is rewritten with the world's originals after every control command (the scrub reads it).

mod control;
mod handshake;
mod keys;
mod push;
mod rpc;
mod session;
mod tlb;
mod transport;
mod wire;
mod world;

use std::path::PathBuf;
use std::sync::Arc;

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

pub fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

const USAGE: &str = "usage: tg-fakedc keygen --out <dir> | serve --key <pem> --port-file <file> [--wire-log <file>] [--control <socket>] [--world-json <file>]";

struct ServeArgs {
    key: PathBuf,
    port_file: PathBuf,
    wire_log: Option<PathBuf>,
    control: Option<PathBuf>,
    world_json: Option<PathBuf>,
}

fn parse_serve(args: &[String]) -> Result<ServeArgs, String> {
    let (mut key, mut port_file, mut wire_log) = (None, None, None);
    let (mut control, mut world_json) = (None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || {
            it.next()
                .map(PathBuf::from)
                .ok_or(format!("{a} needs a path"))
        };
        match a.as_str() {
            "--key" => key = Some(value()?),
            "--port-file" => port_file = Some(value()?),
            "--wire-log" => wire_log = Some(value()?),
            "--control" => control = Some(value()?),
            "--world-json" => world_json = Some(value()?),
            _ => return Err(USAGE.into()),
        }
    }
    Ok(ServeArgs {
        key: key.ok_or(USAGE)?,
        port_file: port_file.ok_or(USAGE)?,
        wire_log,
        control,
        world_json,
    })
}

async fn serve(a: ServeArgs) -> Result<(), String> {
    let pem = std::fs::read_to_string(&a.key).map_err(|e| format!("read the key: {e}"))?;
    let rsa = keys::parse_private_pem(&pem).map_err(|e| format!("the key: {e}"))?;
    let wire = match &a.wire_log {
        Some(p) => Some(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .map_err(|e| format!("open the wire log: {e}"))?,
        ),
        None => None,
    };
    let shared = Arc::new(session::Shared::new(rsa, wire, a.world_json.clone()));
    shared.write_world_json();
    if let Some(path) = a.control.clone() {
        // Bound before the port file appears: a driver that read the port can send commands.
        let listener = control::bind(&path)?;
        tokio::spawn(control::serve(listener, shared.clone()));
    }
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| format!("bind: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("local address: {e}"))?
        .port();
    let tmp = a.port_file.with_extension("tmp");
    std::fs::write(&tmp, format!("{port}\n"))
        .and_then(|()| std::fs::rename(&tmp, &a.port_file))
        .map_err(|e| format!("write the port file: {e}"))?;
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        tokio::spawn(session::handle_conn(stream, shared.clone()));
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("keygen") => match args.get(1..) {
            Some([flag, dir]) if flag == "--out" => keys::keygen(std::path::Path::new(dir)),
            _ => Err(USAGE.into()),
        },
        Some("serve") => parse_serve(&args[1..]).and_then(|a| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .map_err(|e| format!("runtime: {e}"))?
                .block_on(serve(a))
        }),
        _ => Err(USAGE.into()),
    };
    if let Err(e) = result {
        eprintln!("tg-fakedc: {e}");
        std::process::exit(2);
    }
}
