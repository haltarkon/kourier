//! Smoke test of the mock-server harness and its Rust driver (`tests/support/harness.rs`),
//! without any Kourier Signal code: the harness starts, its handshake has everything a
//! libsignal-net client needs, primaries are created and exchange messages.
//!
//! Skips with a message when Node ≥ 24 or `tests/mock-harness/node_modules` is missing
//! (set `KOURIER_REQUIRE_MOCK_HARNESS=1` to fail instead).

use crate::support;

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use support::harness::{
    CreateGroup, CreatePrimary, GroupInfo, GroupSpec, Harness, HarnessError, HarnessOptions,
    PROTOCOL_VERSION, PhoneChange, Primary, SendText, Setup, StorageAccount, StorageContact,
    StorageGroup, StorageSpec, StorageSubject, StorageWrites, locate_or_skip, parse_node_major,
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

// ---------------------------------------------------------------------------------------------
// The Storage Service commands of A10c (`tests/mock-harness/harness.mjs`): each case asserts the
// command's own contract. Counts, ids' lengths and flags only; every value is synthetic.
// ---------------------------------------------------------------------------------------------

/// A phone with two contacts and a group, and what the cases need to name them.
struct StoragePhone {
    harness: Harness,
    phone: Primary,
    ada: Primary,
    bo: Primary,
    group: GroupInfo,
}

impl StoragePhone {
    async fn start() -> Result<Option<Self>, HarnessError> {
        let Some(harness) = Harness::start_or_skip().await? else {
            return Ok(None);
        };
        let ada = harness.create_primary(&CreatePrimary::named("Ada")).await?;
        let bo = harness.create_primary(&CreatePrimary::named("Bo")).await?;
        let phone = harness
            .create_primary(&CreatePrimary {
                contacts: vec![ada.device_ref.clone(), bo.device_ref.clone()],
                ..CreatePrimary::named("Phone")
            })
            .await?;
        let group = harness
            .create_group(&CreateGroup {
                from: phone.device_ref.clone(),
                title: String::from("Club"),
                members: vec![ada.device_ref.clone(), bo.device_ref.clone()],
                ..CreateGroup::default()
            })
            .await?;
        Ok(Some(Self {
            harness,
            phone,
            ada,
            bo,
            group,
        }))
    }

    /// The account record, the two contacts and the group, all with their defaults.
    fn plain(&self) -> StorageSpec {
        StorageSpec {
            primary: self.phone.device_ref.clone(),
            contacts: vec![
                StorageContact::of(&self.ada.device_ref),
                StorageContact::of(&self.bo.device_ref),
            ],
            groups: vec![StorageGroup {
                id: self.group.id.clone(),
                ..StorageGroup::default()
            }],
            ..StorageSpec::default()
        }
    }
}

/// An unknown field 999 (length-delimited, 8 bytes): what a newer phone's record can carry.
const UNKNOWN_FIELD: [u8; 11] = [
    0xBA, 0x3E, 0x08, b'U', b'N', b'K', b'N', b'O', b'W', b'N', b'X',
];

/// The tag of a `StorageRecord`'s one field and that field's message, length prefix checked.
fn split_record(raw: &[u8]) -> (u8, &[u8]) {
    let (mut length, mut shift, mut at) = (0_usize, 0_u32, 1_usize);
    loop {
        let byte = raw[at];
        at += 1;
        length |= usize::from(byte & 0x7F) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            break;
        }
    }
    assert_eq!(at + length, raw.len(), "one field and nothing after it");
    (raw[0], &raw[at..])
}

#[tokio::test]
async fn storage_state_is_what_the_phone_reads() -> Result<(), HarnessError> {
    let Some(w) = StoragePhone::start().await? else {
        return Ok(());
    };
    let phone = w.phone.device_ref.as_str();
    let spec = StorageSpec {
        contacts: vec![
            StorageContact {
                archived: Some(true),
                marked_unread: Some(true),
                muted_until: Some(5_000),
                pinned: Some(true),
                identity_state: Some(String::from("verified")),
                ..StorageContact::of(&w.ada.device_ref)
            },
            StorageContact::of(&w.bo.device_ref),
        ],
        groups: vec![StorageGroup {
            id: w.group.id.clone(),
            muted_until: Some(9_000),
            pinned: Some(true),
            ..StorageGroup::default()
        }],
        account: Some(StorageAccount {
            note_to_self_archived: Some(true),
            ..StorageAccount::default()
        }),
        ..w.plain()
    };
    let written = w.harness.set_storage_state(&spec).await?;
    assert_eq!(written.records, 4, "account, two contacts, a group");

    let view = w.harness.storage_state(phone).await?;
    assert_eq!(view.version, Some(written.version));
    assert_eq!(view.records, 4);
    let ada = view.contact(&w.ada.device_ref);
    assert!(ada.archived && ada.marked_unread);
    assert_eq!(ada.muted_until_ms(), 5_000);
    let bo = view.contact(&w.bo.device_ref);
    assert!(!bo.archived && !bo.marked_unread);
    assert_eq!(bo.muted_until_ms(), 0);
    let group = view.group(&w.group.id);
    assert!(!group.archived && !group.marked_unread);
    assert_eq!(group.muted_until_ms(), 9_000);
    assert_eq!(
        view.pins,
        vec![
            format!("contact:{}", w.ada.device_ref),
            format!("group:{}", w.group.id)
        ],
        "the pins in the order they were pinned"
    );
    assert!(view.note_to_self.archived && !view.note_to_self.marked_unread);
    assert_eq!(
        view.identity_states
            .get(&w.ada.device_ref)
            .map(String::as_str),
        Some("verified")
    );
    assert_eq!(
        view.identity_states
            .get(&w.bo.device_ref)
            .map(String::as_str),
        Some("default")
    );
    assert_eq!(view.orphans, 0);

    // An item that no identifier lists is an orphan; the listed ones are still all readable.
    let put = w.harness.storage_put(phone, None, 2).await?;
    assert_eq!(put.status, 200);
    let view = w.harness.storage_state(phone).await?;
    assert_eq!(view.version, Some(put.version));
    assert_eq!(view.orphans, 2);
    assert_eq!(view.records, 4);
    Ok(())
}

#[tokio::test]
async fn storage_identifiers_list_ids_and_types_only() -> Result<(), HarnessError> {
    let Some(w) = StoragePhone::start().await? else {
        return Ok(());
    };
    let phone = w.phone.device_ref.as_str();
    assert_eq!(
        w.harness.storage_identifiers(phone).await?.version,
        None,
        "no manifest yet"
    );
    let written = w.harness.set_storage_state(&w.plain()).await?;
    let listed = w.harness.storage_identifiers(phone).await?;
    assert_eq!(listed.version, Some(written.version));
    let mut kinds: Vec<u32> = listed.ids.iter().map(|id| id.kind).collect();
    kinds.sort_unstable();
    assert_eq!(
        kinds,
        vec![1, 1, 3, 4],
        "two contacts, a group, the account"
    );
    for id in &listed.ids {
        assert_eq!(STANDARD.decode(&id.id).expect("base64").len(), 16);
    }
    assert_eq!(
        (
            listed.items,
            listed.orphans,
            listed.missing,
            listed.unreadable
        ),
        (4, 0, 0, 0)
    );

    w.harness.storage_put(phone, None, 3).await?;
    let listed = w.harness.storage_identifiers(phone).await?;
    assert_eq!(listed.ids.len(), 4, "the orphans are not listed");
    assert_eq!((listed.items, listed.orphans), (7, 3));
    Ok(())
}

#[tokio::test]
async fn a_raw_record_keeps_the_fields_the_harness_does_not_know() -> Result<(), HarnessError> {
    let Some(w) = StoragePhone::start().await? else {
        return Ok(());
    };
    let phone = w.phone.device_ref.as_str();
    let extra = STANDARD.encode(UNKNOWN_FIELD);
    let spec = StorageSpec {
        contacts: vec![
            StorageContact {
                archived: Some(true),
                unknown_fields: Some(extra.clone()),
                ..StorageContact::of(&w.ada.device_ref)
            },
            StorageContact::of(&w.bo.device_ref),
        ],
        groups: vec![StorageGroup {
            id: w.group.id.clone(),
            unknown_fields: Some(extra.clone()),
            ..StorageGroup::default()
        }],
        account: Some(StorageAccount {
            unknown_fields: Some(extra),
            ..StorageAccount::default()
        }),
        ..w.plain()
    };
    w.harness.set_storage_state(&spec).await?;
    let listed = w.harness.storage_identifiers(phone).await?;

    // The field sits INSIDE the record's message (the length prefix counts it), at its end.
    for (subject, tag, kind) in [
        (StorageSubject::Contact(&w.ada.device_ref), 0x0A, 1),
        (StorageSubject::Group(&w.group.id), 0x1A, 3),
        (StorageSubject::Account, 0x22, 4),
    ] {
        let record = w.harness.raw_storage_record(phone, subject).await?;
        let raw = record.bytes();
        let (found_tag, message) = split_record(&raw);
        assert_eq!(found_tag, tag, "the oneof field of kind {kind}");
        assert!(message.ends_with(&UNKNOWN_FIELD), "kind {kind}");
        assert!(
            listed
                .ids
                .iter()
                .any(|id| id.id == record.id && id.kind == kind),
            "the record's id is the manifest's identifier of kind {kind}"
        );
    }
    // The flags come from the same bytes, and a record without the field has none of it.
    let ada = w
        .harness
        .raw_storage_record(phone, StorageSubject::Contact(&w.ada.device_ref))
        .await?;
    assert!(ada.flags.archived);
    let bo = w
        .harness
        .raw_storage_record(phone, StorageSubject::Contact(&w.bo.device_ref))
        .await?;
    assert!(
        !bo.bytes()
            .windows(UNKNOWN_FIELD.len())
            .any(|w| w == UNKNOWN_FIELD)
    );
    // The extra field does not make the record unreadable for the phone's decoder.
    assert!(
        w.harness
            .storage_state(phone)
            .await?
            .contact(&w.ada.device_ref)
            .archived
    );
    // A chat the phone has no record of is not found.
    let stranger = w
        .harness
        .create_primary(&CreatePrimary::named("Stranger"))
        .await?;
    let missing = w
        .harness
        .raw_storage_record(phone, StorageSubject::Contact(&stranger.device_ref))
        .await
        .err();
    assert_eq!(
        missing.as_ref().and_then(HarnessError::remote_code),
        Some("notFound")
    );
    Ok(())
}

#[tokio::test]
async fn an_undecryptable_item_stays_listed() -> Result<(), HarnessError> {
    let Some(w) = StoragePhone::start().await? else {
        return Ok(());
    };
    let phone = w.phone.device_ref.as_str();
    let spec = StorageSpec {
        primary: phone.to_owned(),
        contacts: vec![
            StorageContact {
                undecryptable: true,
                ..StorageContact::of(&w.ada.device_ref)
            },
            StorageContact::of(&w.bo.device_ref),
        ],
        ..StorageSpec::default()
    };
    w.harness.set_storage_state(&spec).await?;

    // The phone's own reading fails, the manifest still lists the item, the rest still opens.
    assert!(w.harness.storage_state(phone).await.is_err());
    let listed = w.harness.storage_identifiers(phone).await?;
    assert_eq!(listed.ids.len(), 3, "account and two contacts");
    assert_eq!((listed.items, listed.unreadable), (3, 1));
    let bo = w
        .harness
        .raw_storage_record(phone, StorageSubject::Contact(&w.bo.device_ref))
        .await?;
    assert!(!bo.bytes().is_empty());
    let ada = w
        .harness
        .raw_storage_record(phone, StorageSubject::Contact(&w.ada.device_ref))
        .await
        .err();
    assert_eq!(
        ada.as_ref().and_then(HarnessError::remote_code),
        Some("notFound")
    );
    Ok(())
}

#[tokio::test]
async fn storage_writes_counts_the_linked_devices_puts_only() -> Result<(), HarnessError> {
    let Some(w) = StoragePhone::start().await? else {
        return Ok(());
    };
    let phone = w.phone.device_ref.as_str();
    let none = StorageWrites {
        writes: 0,
        conflicts: 0,
        refused: 0,
    };
    assert_eq!(w.harness.storage_writes().await?, none);

    // The phone's own writes are in neither count.
    let first = w.harness.set_storage_state(&w.plain()).await?;
    w.harness
        .update_storage_record(
            phone,
            StorageSubject::Contact(&w.ada.device_ref),
            &PhoneChange {
                archived: Some(true),
                ..PhoneChange::default()
            },
        )
        .await?;
    assert_eq!(w.harness.storage_writes().await?, none);

    // A PUT the server applied is a write; one against a manifest that is not the next is a
    // conflict, answered 409 with the manifest that stands.
    let put = w.harness.storage_put(phone, None, 0).await?;
    assert_eq!((put.status, put.version), (200, first.version + 2));
    assert_eq!(
        w.harness.storage_writes().await?,
        StorageWrites {
            writes: 1,
            conflicts: 0,
            refused: 0,
        }
    );
    let stale = w
        .harness
        .storage_put(phone, Some(put.version + 5), 0)
        .await?;
    assert_eq!((stale.status, stale.version), (409, put.version));
    assert_eq!(
        w.harness.storage_writes().await?,
        StorageWrites {
            writes: 1,
            conflicts: 1,
            refused: 0,
        }
    );
    let next = w.harness.storage_put(phone, None, 0).await?;
    assert_eq!((next.status, next.version), (200, put.version + 1));
    assert_eq!(
        w.harness.storage_writes().await?,
        StorageWrites {
            writes: 2,
            conflicts: 1,
            refused: 0,
        }
    );
    // V4: a PUT the mock refuses (a manifest without a version: 400) is neither a write nor a
    // conflict but it is counted, so an oracle of "nothing was written" cannot miss it.
    assert!(w.harness.storage_put(phone, Some(0), 0).await.is_err());
    assert_eq!(
        w.harness.storage_writes().await?,
        StorageWrites {
            writes: 2,
            conflicts: 1,
            refused: 1,
        }
    );
    // What a linked device wrote, the phone reads.
    let view = w.harness.storage_state(phone).await?;
    assert_eq!(view.version, Some(next.version));
    assert!(view.contact(&w.ada.device_ref).archived);
    Ok(())
}

#[tokio::test]
async fn update_storage_record_moves_one_record_under_a_new_id() -> Result<(), HarnessError> {
    let Some(w) = StoragePhone::start().await? else {
        return Ok(());
    };
    let phone = w.phone.device_ref.as_str();
    let first = w.harness.set_storage_state(&w.plain()).await?;
    let before = w.harness.storage_identifiers(phone).await?;

    // Beyond a double: the mute time travels as a decimal string.
    let beyond: u64 = (1 << 53) + 1;
    let version = w
        .harness
        .update_storage_record(
            phone,
            StorageSubject::Contact(&w.ada.device_ref),
            &PhoneChange {
                archived: Some(true),
                muted_until: Some(beyond),
                ..PhoneChange::default()
            },
        )
        .await?;
    assert_eq!(version, first.version + 1);
    let view = w.harness.storage_state(phone).await?;
    assert_eq!(view.version, Some(version));
    let ada = view.contact(&w.ada.device_ref);
    assert!(ada.archived && !ada.marked_unread);
    assert_eq!(ada.muted_until.as_deref(), Some("9007199254740993"));
    let bo = view.contact(&w.bo.device_ref);
    assert!(
        !bo.archived && bo.muted_until_ms() == 0,
        "the other record is as it was"
    );
    assert_eq!(view.orphans, 0, "the old id is deleted");

    // Exactly one identifier changed: a contact's.
    let after = w.harness.storage_identifiers(phone).await?;
    assert_eq!(after.ids.len(), before.ids.len());
    let gone: Vec<_> = before
        .ids
        .iter()
        .filter(|id| !after.ids.contains(id))
        .collect();
    let new: Vec<_> = after
        .ids
        .iter()
        .filter(|id| !before.ids.contains(id))
        .collect();
    assert_eq!((gone.len(), new.len()), (1, 1));
    assert_eq!((gone[0].kind, new[0].kind), (1, 1));
    assert_eq!((after.items, after.orphans), (4, 0));

    // A group's pin and the account's Note to Self move the same way.
    w.harness
        .update_storage_record(
            phone,
            StorageSubject::Group(&w.group.id),
            &PhoneChange {
                pinned: Some(true),
                marked_unread: Some(true),
                ..PhoneChange::default()
            },
        )
        .await?;
    let version = w
        .harness
        .update_storage_record(
            phone,
            StorageSubject::Account,
            &PhoneChange {
                archived: Some(true),
                ..PhoneChange::default()
            },
        )
        .await?;
    let view = w.harness.storage_state(phone).await?;
    assert_eq!(view.version, Some(version));
    assert_eq!(view.pins, vec![format!("group:{}", w.group.id)]);
    assert!(view.group(&w.group.id).marked_unread);
    assert!(view.note_to_self.archived);
    assert!(
        view.contact(&w.ada.device_ref).archived,
        "an earlier change stays"
    );

    // The phone changes nothing before it has a state.
    let fresh = w
        .harness
        .create_primary(&CreatePrimary::named("Fresh"))
        .await?;
    let refused = w
        .harness
        .update_storage_record(
            &fresh.device_ref,
            StorageSubject::Account,
            &PhoneChange::default(),
        )
        .await
        .err();
    assert_eq!(
        refused.as_ref().and_then(HarnessError::remote_code),
        Some("badRequest")
    );
    Ok(())
}

#[tokio::test]
async fn conflict_next_storage_write_makes_the_next_put_meet_the_phones_change()
-> Result<(), HarnessError> {
    let Some(w) = StoragePhone::start().await? else {
        return Ok(());
    };
    let phone = w.phone.device_ref.as_str();
    let first = w.harness.set_storage_state(&w.plain()).await?;
    let change = PhoneChange {
        archived: Some(true),
        ..PhoneChange::default()
    };
    w.harness
        .conflict_next_storage_write(phone, StorageSubject::Contact(&w.ada.device_ref), &change)
        .await?;
    // Armed, not applied; one change at a time.
    let view = w.harness.storage_state(phone).await?;
    assert_eq!(view.version, Some(first.version));
    assert!(!view.contact(&w.ada.device_ref).archived);
    let twice = w
        .harness
        .conflict_next_storage_write(phone, StorageSubject::Contact(&w.bo.device_ref), &change)
        .await
        .err();
    assert_eq!(
        twice.as_ref().and_then(HarnessError::remote_code),
        Some("badRequest")
    );

    // The next write was made against the old manifest: the phone's change comes first.
    let met = w.harness.storage_put(phone, None, 0).await?;
    assert_eq!((met.status, met.version), (409, first.version + 1));
    assert_eq!(
        w.harness.storage_writes().await?,
        StorageWrites {
            writes: 0,
            conflicts: 1,
            refused: 0,
        }
    );
    let view = w.harness.storage_state(phone).await?;
    assert_eq!(view.version, Some(met.version));
    assert!(view.contact(&w.ada.device_ref).archived);
    assert!(
        !view.contact(&w.bo.device_ref).archived,
        "the refused arm changed nothing"
    );

    // One change only: the write after it goes through.
    let put = w.harness.storage_put(phone, None, 0).await?;
    assert_eq!((put.status, put.version), (200, met.version + 1));
    assert_eq!(
        w.harness.storage_writes().await?,
        StorageWrites {
            writes: 1,
            conflicts: 1,
            refused: 0,
        }
    );
    Ok(())
}
