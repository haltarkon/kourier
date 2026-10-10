//! Smoke test of the mock-server harness and its Rust driver (`tests/support/harness.rs`),
//! without any Kourier Signal code: the harness starts, its handshake has everything a
//! libsignal-net client needs, primaries are created and exchange messages.
//!
//! Skips with a message when Node ≥ 24 or `tests/mock-harness/node_modules` is missing
//! (set `KOURIER_REQUIRE_MOCK_HARNESS=1` to fail instead).

mod support;

use std::time::Duration;

use support::harness::{
    CreatePrimary, GroupSpec, Harness, HarnessError, HarnessOptions, PROTOCOL_VERSION, SendText,
    Setup, locate_or_skip, parse_node_major,
};

const WAIT: Duration = Duration::from_secs(10);

#[test]
fn node_versions_parse() {
    assert_eq!(parse_node_major("v24.21.0\n"), Some(24));
    assert_eq!(parse_node_major("v22.1.0"), Some(22));
    assert_eq!(parse_node_major("24.21.0"), None);
    assert_eq!(parse_node_major(""), None);
}

#[tokio::test]
async fn handshake_is_complete() -> Result<(), HarnessError> {
    let Some(harness) = Harness::start_or_skip().await? else {
        return Ok(());
    };
    let handshake = harness.handshake();

    assert_eq!(handshake.protocol, PROTOCOL_VERSION);
    assert_ne!(handshake.port, 0);
    assert_eq!(handshake.host, "localhost");
    assert_eq!(handshake.ip, "127.0.0.1");
    assert_eq!(handshake.http_version, 2);

    let ca = handshake.ca_der()?;
    assert_eq!(ca.first(), Some(&0x30), "CA DER is an ASN.1 SEQUENCE");
    assert!(handshake.ca_pem.starts_with("-----BEGIN CERTIFICATE-----"));
    let trust_root = handshake.trust_root()?;
    assert_eq!(trust_root.len(), 33);
    assert_eq!(trust_root[0], 0x05, "DJB key type");
    assert!(!handshake.zk_public_params()?.is_empty());
    assert!(!handshake.generic_server_public_params()?.is_empty());
    assert!(!handshake.backup_server_public_params()?.is_empty());

    let base = format!("https://localhost:{}", handshake.port);
    assert_eq!(handshake.urls.chat, base);
    assert_eq!(handshake.urls.storage, format!("{base}/storageService"));
    assert_eq!(
        handshake.urls.cdn.get("3").map(String::as_str),
        Some(format!("{base}/cdn3").as_str())
    );
    assert_eq!(
        handshake.paths.provisioning_websocket,
        "/v1/websocket/provisioning/"
    );
    let send = &handshake.remote_config["desktop.libsignalNet.grpc.MessagesSendMessage"];
    assert!(send.enabled);
    assert_eq!(send.value.as_deref(), Some("ws"));

    assert_eq!(handshake.versions.mock_server.as_deref(), Some("27.0.0"));
    assert_eq!(
        handshake.versions.libsignal_client.as_deref(),
        Some("0.100.0")
    );
    assert!(handshake.state_dir.is_dir());

    assert_eq!(harness.ping().await?, PROTOCOL_VERSION);
    let state_dir = handshake.state_dir.clone();
    let status = harness.shutdown().await?;
    assert!(status.success(), "harness exit status {status}");
    assert!(!state_dir.exists(), "shutdown removes the state dir");
    Ok(())
}

#[tokio::test]
async fn creates_primaries_and_relays_text() -> Result<(), HarnessError> {
    let Some(harness) = Harness::start_or_skip().await? else {
        return Ok(());
    };

    let alice = harness
        .create_primary(&CreatePrimary::named("Alice"))
        .await?;
    assert_eq!(alice.device_id, 1);
    assert_eq!(alice.device_ref, format!("{}.1", alice.aci));
    assert!(alice.e164.is_some() && alice.pni.is_some());

    let phone = harness
        .create_primary(&CreatePrimary {
            profile_name: String::from("Phone"),
            contacts: vec![alice.aci.clone()],
            groups: vec![GroupSpec {
                title: String::from("Family"),
                members: vec![alice.device_ref.clone()],
            }],
            ..CreatePrimary::default()
        })
        .await?;
    assert_eq!(phone.contacts, vec![alice.device_ref.clone()]);
    assert_eq!(phone.groups.len(), 1);
    assert_eq!(phone.groups[0].title, "Family");

    // The wait is in flight before the send: responses come back out of request order.
    let send = SendText {
        timestamp: Some(1_700_000_000_000),
        await_delivery: true,
        ..SendText::new(&alice.aci, &phone.aci, "hi phone")
    };
    let (received, sent) = tokio::join!(harness.wait_for_message(&phone.aci, None, WAIT), async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        harness.send_text(&send).await
    });
    let (received, sent) = (received?, sent?);
    assert_eq!(sent.timestamp, 1_700_000_000_000);
    assert_eq!(sent.to, phone.device_ref);
    assert_eq!(received.text, "hi phone");
    assert_eq!(received.timestamp, 1_700_000_000_000);
    assert_eq!(received.source, alice.device_ref);
    assert!(!received.sealed);
    assert_eq!(received.group_id, None);

    // Sealed sender, in the group.
    let sealed = SendText {
        sealed: true,
        group: Some(phone.groups[0].id.clone()),
        await_delivery: true,
        ..SendText::new(&alice.aci, &phone.device_ref, "sealed hi")
    };
    let sent = harness.send_text(&sealed).await?;
    let received = harness
        .wait_for_message(&phone.aci, Some(&alice.aci), WAIT)
        .await?;
    assert_eq!(received.text, "sealed hi");
    assert_eq!(received.timestamp, sent.timestamp);
    assert!(received.sealed);
    assert_eq!(received.envelope_type, "SealedSender");
    assert_eq!(
        received.source, alice.device_ref,
        "unsealed to the real sender"
    );
    assert_eq!(
        received.group_id.as_deref(),
        Some(phone.groups[0].id.as_str())
    );

    // Errors come back per request with stable codes, and the harness keeps serving.
    let timeout = harness
        .wait_for_message(&alice.aci, None, Duration::from_millis(50))
        .await
        .err();
    assert_eq!(
        timeout.as_ref().and_then(HarnessError::remote_code),
        Some("timeout")
    );
    let unknown = harness
        .request_value("noSuchCommand", serde_json::json!({}))
        .await
        .err();
    assert_eq!(
        unknown.as_ref().and_then(HarnessError::remote_code),
        Some("unknownCommand")
    );
    assert_eq!(harness.ping().await?, PROTOCOL_VERSION);

    let status = harness.shutdown().await?;
    assert!(status.success(), "harness exit status {status}");
    Ok(())
}

/// Whether process `pid` has exited (gone, or a zombie not yet reaped).
fn process_exited(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // The state follows the parenthesised command name: `<pid> (<comm>) <state> ...`.
        Ok(stat) => stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with(['Z', 'X'])),
        Err(_) => true,
    }
}

#[tokio::test]
async fn dropping_the_driver_kills_the_harness() -> Result<(), HarnessError> {
    let Some(harness) = Harness::start_or_skip().await? else {
        return Ok(());
    };
    let pid = harness.handshake().pid;
    let state_dir = harness.handshake().state_dir.clone();
    assert!(!process_exited(pid));
    drop(harness);

    assert!(!state_dir.exists(), "the state dir goes with the driver");
    let deadline = tokio::time::Instant::now() + WAIT;
    while !process_exited(pid) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "harness {pid} still running"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

#[tokio::test]
async fn requests_fail_with_closed_once_the_harness_dies() -> Result<(), HarnessError> {
    let Some(harness) = Harness::start_or_skip().await? else {
        return Ok(());
    };
    let phone = harness
        .create_primary(&CreatePrimary::named("Phone"))
        .await?;
    let pid = harness.handshake().pid;

    // A request in flight when the process dies fails at once, not after its timeout.
    let started = tokio::time::Instant::now();
    let (in_flight, killed) = tokio::join!(
        harness.wait_for_message(&phone.aci, None, Duration::from_secs(60)),
        async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            std::process::Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status()
        }
    );
    assert!(killed?.success(), "kill -KILL {pid}");
    assert!(
        matches!(in_flight, Err(HarnessError::Closed(_))),
        "in-flight request: {in_flight:?}"
    );
    assert!(
        started.elapsed() < WAIT,
        "failed without waiting for its timeout"
    );

    // So does every later one.
    let later = harness.ping().await;
    assert!(
        matches!(later, Err(HarnessError::Closed(_))),
        "later request: {later:?}"
    );
    Ok(())
}

/// Starts a stand-in `harness.mjs` with `script` as its source.
async fn start_fake(script: &str) -> Result<Option<Result<Harness, HarnessError>>, HarnessError> {
    let Some(setup) = locate_or_skip().await? else {
        return Ok(None);
    };
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("harness.mjs"), script)?;
    let fake = Setup {
        node: setup.node,
        dir: dir.path().to_path_buf(),
    };
    let options = HarnessOptions {
        startup_timeout: WAIT,
        ..HarnessOptions::default()
    };
    Ok(Some(Harness::start_with(&fake, options).await))
}

#[tokio::test]
async fn startup_failures_carry_the_reason() -> Result<(), HarnessError> {
    // Exits before the handshake: the error carries the harness's stderr.
    let Some(started) =
        start_fake("console.error('fake harness: no mock server here'); process.exit(3);").await?
    else {
        return Ok(());
    };
    match started {
        Err(HarnessError::Startup(message)) => assert!(
            message.contains("fake harness: no mock server here"),
            "{message}"
        ),
        other => panic!("expected a startup error, got {:?}", other.err()),
    }

    // Something other than the handshake on stdout.
    let Some(started) = start_fake("console.log('hello'); setInterval(() => {}, 1000);").await?
    else {
        return Ok(());
    };
    match started {
        Err(HarnessError::Startup(message)) => {
            assert!(message.contains("bad handshake line"), "{message}");
        }
        other => panic!("expected a startup error, got {:?}", other.err()),
    }
    Ok(())
}
