//! The S2 walking skeleton as a demo (docs/architecture.md §11 S2): against
//! `@signalapp/mock-server` (through `tests/mock-harness`), link Kourier as a secondary device,
//! receive one unsealed and one sealed text from a contact, reply, restart, and print a log-safe
//! transcript (ids shortened, no message text, no numbers, no keys).
//!
//! ```bash
//! (cd tests/mock-harness && ~/.nvm/versions/node/v24.21.0/bin/npm ci)   # once
//! cargo run -p kourier-signal --example skeleton
//! RUST_LOG=kourier_signal=debug cargo run -p kourier-signal --example skeleton   # + backend log
//! ```
//!
//! Only the local mock server is supported: runs against Signal's real servers need a dedicated
//! test account and the user's explicit OK (§10.1) and are not part of this example yet.

#[path = "../tests/support/mod.rs"]
mod support;

use std::process::ExitCode;
use std::time::Instant;

use kourier_backend_api::{Command, StartMode, TransmitOutcome};
use kourier_model::{LoginStepKind, UserId};
use support::fake_core::{AccountFiles, FakeCore, Sink, WAIT, local_env};
use support::harness::{CreatePrimary, Harness, SendText};

/// `aci:…1234`: the last four hex digits of a uuid (§9.1).
fn short(uuid: &str) -> String {
    let tail: String = uuid
        .chars()
        .filter(char::is_ascii_hexdigit)
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("aci:…{tail}")
}

struct Transcript(Instant);

impl Transcript {
    fn line(&self, text: impl AsRef<str>) {
        println!(
            "[{:>6.2}s] {}",
            self.0.elapsed().as_secs_f64(),
            text.as_ref()
        );
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> ExitCode {
    if std::env::args().len() > 1 {
        eprintln!("usage: skeleton (no arguments: only the local mock server is supported)");
        return ExitCode::from(2);
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("skeleton failed: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let t = Transcript(Instant::now());
    let Some(harness) = Harness::start_or_skip().await? else {
        return Err("the mock harness is not available (see tests/mock-harness/README.md)".into());
    };
    let versions = &harness.handshake().versions;
    t.line(format!(
        "mock server {} (libsignal-client {}) on localhost:{}",
        versions.mock_server.as_deref().unwrap_or("?"),
        versions.libsignal_client.as_deref().unwrap_or("?"),
        harness.handshake().port
    ));
    let tmp = tempfile::tempdir()?;
    let env = local_env(harness.handshake(), tmp.path())?;
    let alice = harness
        .create_primary(&CreatePrimary::named("Alice"))
        .await?;
    let phone = harness
        .create_primary(&CreatePrimary {
            profile_name: "Myself".to_owned(),
            contacts: vec![alice.device_ref.clone()],
            ..CreatePrimary::default()
        })
        .await?;
    t.line(format!(
        "phone {} and contact {} registered",
        short(&phone.aci),
        short(&alice.aci)
    ));

    let sink = Sink::open(&tmp.path().join("sink.db"))?;
    let files = AccountFiles::new(tmp.path())?;
    let mut core = FakeCore::start(&env, &files, StartMode::Fresh, &sink)?;
    let mut device = None;
    let (_, steps) = core
        .link(|url| {
            t.line(format!(
                "login: QR shown (sgnl://linkdevice?…, {} chars); the phone scans it",
                url.len()
            ));
            let device = &mut device;
            let (harness, phone) = (&harness, &phone);
            async move {
                *device = harness
                    .complete_provision(&url, &phone.device_ref, WAIT)
                    .await
                    .ok();
            }
        })
        .await;
    for step in &steps {
        let what = match &step.kind {
            LoginStepKind::Input { fields } => format!("input ({} field)", fields.len()),
            LoginStepKind::ShowQr { .. } => "QR code".to_owned(),
            LoginStepKind::Progress { label, .. } => format!("progress: {label}"),
            LoginStepKind::Done => "done".to_owned(),
            other => format!("{other:?}"),
        };
        t.line(format!("login step {:<12} {what}", step.step_id));
    }
    let device = device.ok_or("the phone did not complete provisioning")?;
    t.line(format!(
        "linked as device {} of {}; AccountReady committed by the core: {}",
        device.device_id,
        short(&device.aci),
        sink.events("AccountReady").len()
    ));
    harness
        .wait_for_sync(&phone.device_ref, &device.device_ref, WAIT)
        .await?;
    t.line("the phone answered our CONTACTS, CONFIGURATION and BLOCKED sync requests");
    core.wait_online().await;
    t.line(format!("connection: {}", core.conn_state().name()));

    let alice_id = UserId::parse(&format!("aci:{}", alice.aci))?;
    for (sealed, text) in [(false, "hello from Alice"), (true, "a sealed hello")] {
        let sent = harness
            .send_text(&SendText {
                sealed,
                await_delivery: true,
                ..SendText::new(&alice.aci, &device.device_ref, text)
            })
            .await?;
        let message = sink
            .wait_message(alice_id.as_str(), &sent.timestamp.to_string(), WAIT)
            .await
            .ok_or("the message did not reach the core")?;
        t.line(format!(
            "received {} message {} from {} device {}: {} chars, delivered to the core {}x",
            if sealed { "sealed" } else { "unsealed" },
            message.local,
            short(&alice.aci),
            message.device.unwrap_or(0),
            message.body.chars().count(),
            sink.deliveries(alice_id.as_str(), &message.local)
        ));
    }

    let (sent_at, outcome) = core
        .transmit_text(&alice_id, "hello back from Kourier")
        .await;
    let sent = matches!(outcome, TransmitOutcome::Sent { .. });
    t.line(format!(
        "reply {sent_at}: {}",
        if sent { "sent" } else { "failed" }
    ));
    let got = harness
        .wait_for_message(&alice.aci, Some(&device.device_ref), WAIT)
        .await?;
    t.line(format!(
        "{} received reply {} ({} chars, {} envelope)",
        short(&alice.aci),
        got.timestamp,
        got.text.chars().count(),
        got.message_type
    ));

    t.line(format!("shutdown: {:?}", core.shutdown().await));
    let mut core = FakeCore::start(&env, &files, StartMode::Resume, &sink)?;
    core.send(Command::Connect).await;
    core.wait_online().await;
    t.line("restarted from backend.db and reconnected: Online");
    t.line(format!("shutdown: {:?}", core.shutdown().await));
    harness.shutdown().await?;
    t.line("mock server stopped");
    Ok(())
}
