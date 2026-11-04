//! The scripted server's SEND half checked by itself (W5a chunk 2a): the IQ rules of
//! `support::wire` answer in the shapes upstream's OWN request specs build and parse, the ack policy
//! answers a `<message>` frame as the server would, and a real client over the Wire reads the answers
//! of the IQs it asks by itself after a login (its own devices, the privacy settings, the reach-out
//! lock: critic H3). Chunk 2b: the PEER's decrypt half, over the wire: the actor's client is handed to
//! the test (`wire::parts_handing_client`), the test calls upstream's `Client::send` directly (Kourier's
//! send path comes later), and the peer reads the frame the client wrote, as the RESPONDER of a cold
//! chat (the `pkmsg` made from the bundle the `encrypt` rule handed out) and as the INITIATOR of a warm
//! one (the client answers on the session the peer's own `pkmsg` began: a `msg`). Chunk 2c: our OWN
//! PHONE reads its copy of what the client sends (a `DeviceSentMessage` in a `pkmsg` to device 0 of the
//! account's LID user, from the phone's bundle in the `encrypt` rule).
//!
//! The pure tests build each request with upstream's spec (`RequestUtils::build_iq_node`), give it to
//! the rule, and parse the rule's answer with the same spec's `parse_response`, so a rule that answers
//! in a shape upstream does not read fails here and not in a send test far away. The wire tests run
//! in ONE offline child under `unshare -rn`, as every test that builds a client.
//!
//! Every value is SYNTHETIC (the 555 block, the fixture's LIDs, keys made on the spot).

use std::time::{Duration, Instant};

use kourier_backend_api::{Command, Exit, StartMode};
use wacore::download::MediaType;
use wacore::iq::groups::{GroupMetadataOutcome, GroupQueryIq};
use wacore::iq::mediaconn::MediaConnSpec;
use wacore::iq::mex::{MexDoc, MexQuerySpec};
use wacore::iq::mex_operations::fetch_reachout_timelock;
use wacore::iq::prekeys::PreKeyFetchSpec;
use wacore::iq::privacy::{PrivacyCategory, PrivacySettingsSpec, PrivacyValue};
use wacore::iq::spec::IqSpec;
use wacore::iq::usync::{DeviceListSpec, LidQuerySpec};
use wacore::libsignal::protocol::KeyPair;
use wacore::net::{HttpClient, HttpRequest};
use wacore::proto_helpers::MessageBuilderExt;
use wacore::request::RequestUtils;
use wacore::upload::{EncryptedMediaInfo, encrypt_media_streaming_with_key};
use wacore_binary::builder::NodeBuilder;
use wacore_binary::{Jid, Node};
use waproto::buffa::MessageField;
use waproto::whatsapp as wa;
use whatsapp_rust::upload::UploadResponse;

use crate::receive::{account_dir, secrets};
use crate::support::capture::Capture;
use crate::support::cdn::{self, BodyEnd, Cdn, Check, Reply};
use crate::support::peer::{self, Peer};
use crate::support::wire::{self, Ack, Answer, Bundle};
use crate::support::{self};
use crate::wire::{start_actor, start_actor_handing_client, start_actor_with_http};

/// A synthetic contact's phone number and LID for the LID query (tails that no other value holds).
const CONTACT_PN: &str = "15550048213977";
const CONTACT_LID: &str = "222222261840735";
/// A synthetic group.
const GROUP: &str = "120363000071529604";

/// The request node upstream writes for `spec`.
fn request_of(spec: &impl IqSpec) -> Node {
    RequestUtils::new("rig".to_owned()).build_iq_node(spec.build_iq(), Some("rig-1".to_owned()))
}

/// `answer` as the `<iq type="result">` it is sent as.
fn result_of(answer: Option<Answer>) -> Node {
    match answer {
        Some(Answer::Result(children)) => NodeBuilder::new("iq")
            .attr("type", "result")
            .attr("id", "rig-1")
            .attr("from", "s.whatsapp.net")
            .children(children)
            .build(),
        other => panic!("the rule did not answer with a result: {other:?}"),
    }
}

fn jid(text: &str) -> Jid {
    text.parse().expect("a fixture jid")
}

#[test]
fn the_usync_rule_answers_device_lists_and_lids_upstream_reads() {
    let rule = wire::usync(
        vec![(peer::PEER_LID, vec![0, 3])],
        vec![(CONTACT_PN, CONTACT_LID)],
    );
    let devices = DeviceListSpec::new(
        vec![
            jid(&format!("{}@lid", peer::PEER_LID)),
            jid("15550000031527@s.whatsapp.net"),
        ],
        "sid",
    );
    let parsed = devices
        .parse_response(&result_of(rule(&request_of(&devices))).as_node_ref())
        .expect("upstream reads the device answer");
    assert_eq!(
        parsed.device_lists.len(),
        1,
        "only the user the rule knows is answered"
    );
    let list = &parsed.device_lists[0];
    assert_eq!(list.user.user, peer::PEER_LID);
    assert_eq!(
        list.devices.iter().map(|d| d.device).collect::<Vec<_>>(),
        vec![0, 3]
    );

    let lids = LidQuerySpec::new(vec![jid(&format!("{CONTACT_PN}@s.whatsapp.net"))], "sid");
    let parsed = lids
        .parse_response(&result_of(rule(&request_of(&lids))).as_node_ref())
        .expect("upstream reads the LID answer");
    assert_eq!(parsed.lid_mappings.len(), 1);
    assert_eq!(parsed.lid_mappings[0].phone_number, CONTACT_PN);
    assert_eq!(parsed.lid_mappings[0].lid, CONTACT_LID);
}

#[test]
fn the_privacy_rule_answers_the_settings_and_the_token_issue() {
    let rule = wire::privacy("none");
    let spec = PrivacySettingsSpec::new();
    let settings = spec
        .parse_response(&result_of(rule(&request_of(&spec))).as_node_ref())
        .expect("upstream reads the settings");
    assert_eq!(
        settings.get_value(&PrivacyCategory::ReadReceipts),
        Some(&PrivacyValue::None),
        "read receipts are off as the rule says"
    );
    let issue = NodeBuilder::new("iq")
        .attr("xmlns", "privacy")
        .attr("type", "set")
        .children([NodeBuilder::new("tokens").build()])
        .build();
    assert!(
        matches!(rule(&issue), Some(Answer::Result(_))),
        "the token issue is answered"
    );
    assert!(
        rule(&NodeBuilder::new("iq").build()).is_none(),
        "anything else is left to the 503"
    );
}

#[test]
fn the_reach_out_rule_answers_a_lock_or_none_and_nothing_else() {
    let doc = MexDoc {
        name: fetch_reachout_timelock::NAME,
        id: fetch_reachout_timelock::DOC_ID,
    };
    let spec = MexQuerySpec::new(doc, &fetch_reachout_timelock::Variables {}).expect("the spec");
    let read = |until: Option<u64>| {
        let answer = wire::reach_out(until)(&request_of(&spec));
        let response = spec
            .parse_response(&result_of(answer).as_node_ref())
            .expect("upstream reads the MEX answer");
        let data: fetch_reachout_timelock::Response =
            serde_json::from_value(response.data.expect("data")).expect("the timelock answer");
        data.xwa2_fetch_account_reachout_timelock
            .expect("the account's state")
    };
    let locked = read(Some(1_900_000_000));
    assert_eq!(locked.is_active, Some(true));
    assert_eq!(locked.time_enforcement_ends.as_deref(), Some("1900000000"));
    assert_eq!(read(None).is_active, Some(false));

    let other = MexQuerySpec::new(
        MexDoc {
            name: "WAWebMexOtherQuery",
            id: "10000000000000001",
        },
        &fetch_reachout_timelock::Variables {},
    )
    .expect("another spec");
    assert!(
        wire::reach_out(None)(&request_of(&other)).is_none(),
        "another MEX query is left to the 503"
    );
}

#[test]
fn the_group_rule_answers_its_group_with_its_addressing_mode() {
    let rule = wire::group_info(
        GROUP,
        "lid",
        vec![
            ("222222248517306@lid", Some("superadmin")),
            ("222222259264011@lid", None),
        ],
    );
    let spec = GroupQueryIq::new(&jid(&format!("{GROUP}@g.us")));
    let outcome = spec
        .parse_response(&result_of(rule(&request_of(&spec))).as_node_ref())
        .expect("upstream reads the group");
    let GroupMetadataOutcome::Full(group) = outcome else {
        panic!("the group came back as not modified");
    };
    assert_eq!(group.id.user, GROUP);
    assert_eq!(group.addressing_mode.as_str(), "lid");
    assert_eq!(group.participants.len(), 2);
    let other = GroupQueryIq::new(&jid("120363000099999999@g.us"));
    assert!(
        rule(&request_of(&other)).is_none(),
        "another group is left to the 503"
    );
}

#[test]
fn the_prekey_rule_answers_a_bundle_or_an_unregistered_device() {
    let mut rng = rand::make_rng::<rand::rngs::StdRng>();
    let key = |rng: &mut rand::rngs::StdRng| -> [u8; 32] {
        KeyPair::generate(rng)
            .public_key
            .public_key_bytes()
            .try_into()
            .expect("32 bytes")
    };
    let bundle = Bundle {
        registration_id: 4_242,
        identity: key(&mut rng),
        signed_id: 7,
        signed: key(&mut rng),
        signed_signature: [0; 64],
        one_time: Some((70_001, key(&mut rng))),
    };
    let known = format!("{}:0@lid", peer::PEER_LID);
    let unknown = "222222271139504:2@lid";
    let rule = wire::prekeys(vec![(known.clone(), bundle)]);
    let spec = PreKeyFetchSpec::new(vec![jid(&known), jid(unknown)]);
    let outcome = spec
        .parse_response(&result_of(rule(&request_of(&spec))).as_node_ref())
        .expect("upstream reads the bundles");
    assert_eq!(outcome.bundles.len(), 1, "one bundle");
    let read = outcome
        .bundles
        .get(&jid(&known))
        .expect("the known device's bundle");
    assert_eq!(read.registration_id().expect("registration"), 4_242);
    assert_eq!(u32::from(read.signed_pre_key_id().expect("signed id")), 7);
    assert_eq!(
        read.pre_key_id().expect("one-time id").map(u32::from),
        Some(70_001)
    );
    assert_eq!(
        outcome.rejected.len(),
        1,
        "the other device is unregistered"
    );
    assert_eq!(outcome.rejected[0].code, 406);
}

#[test]
fn the_ack_policy_takes_the_id_then_the_queue_then_the_default() {
    let server = wire::server();
    assert_eq!(server.take_ack("a"), Ack::Silent, "the default is no ack");
    server
        .ack_messages(Ack::Ok { t: 1_700_000_100 })
        .ack_next(Ack::Error(463))
        .ack_next(Ack::DropConnection)
        .ack_id("held", Ack::Silent);
    assert_eq!(
        server.take_ack("held"),
        Ack::Silent,
        "an id's own answer first"
    );
    assert_eq!(
        server.take_ack("b"),
        Ack::Error(463),
        "then the queue, in order"
    );
    assert_eq!(server.take_ack("held"), Ack::Silent, "an id's answer stays");
    assert_eq!(server.take_ack("c"), Ack::DropConnection);
    assert_eq!(
        server.take_ack("d"),
        Ack::Ok { t: 1_700_000_100 },
        "then the default"
    );
}

#[test]
fn an_ack_carries_the_servers_time_or_the_nack_code() {
    let to = format!("{}@lid", peer::PEER_LID);
    let ok = wire::message_ack("3EB0AAAA", &to, &Ack::Ok { t: 1_700_000_100 }).expect("an ack");
    assert_eq!(ok.tag, "ack");
    let attr = |n: &Node, k: &str| n.attrs.get(k).map(|v| v.as_str().into_owned());
    assert_eq!(attr(&ok, "class").as_deref(), Some("message"));
    assert_eq!(attr(&ok, "id").as_deref(), Some("3EB0AAAA"));
    assert_eq!(attr(&ok, "from"), Some(to.clone()));
    assert_eq!(attr(&ok, "t").as_deref(), Some("1700000100"));
    assert_eq!(attr(&ok, "error"), None);
    let nack = wire::message_ack("3EB0AAAA", &to, &Ack::Error(463)).expect("a nack");
    assert_eq!(attr(&nack, "error").as_deref(), Some("463"));
    assert_eq!(attr(&nack, "t"), None);
    assert!(wire::message_ack("3EB0AAAA", &to, &Ack::Silent).is_none());
    assert!(wire::message_ack("3EB0AAAA", &to, &Ack::DropConnection).is_none());
}

/// A synthetic end of a reach-out lock (seconds).
const LOCK_UNTIL: u64 = 1_900_000_000;

/// The case the child runs (set by the parent).
const CASE: &str = "WA_RIG_CASE";

/// Waits (at most 20 s) until `found` holds for the recorded lines.
/// Fix F17: the recorder reads the SHAPE of a key or a token (32 or more of `[A-Za-z0-9+/=_-]` in a row) in the crate's
/// own lines, because the fixture's keys and tokens are random per run and a grep for values cannot see them.
/// Mutants: the bound at 33; upstream's lines judged too; the check never run on a line.
#[test]
fn the_capture_flags_a_run_of_token_characters_in_the_crates_own_lines_only() {
    use support::capture::{TOKEN_RUN, has_token_run};
    let log = Capture::default();
    let token = "aB3+/=_-".repeat(TOKEN_RUN / 8);
    assert_eq!(token.len(), TOKEN_RUN);
    let almost = "x".repeat(TOKEN_RUN - 1);
    tracing::subscriber::with_default(log.clone(), || {
        tracing::info!(target: "kourier_whatsapp::send", "a key {token} was kept");
        tracing::info!(target: "kourier_whatsapp::send", "an id 3EB0C0DE5B3362C3A1 and {almost} and words");
        tracing::info!(target: "kourier_whatsapp::send", "a base64 run split by a space {} {}", &token[..20], &token[..20]);
        tracing::info!(target: "somewhere_else", "a key {token}");
    });
    assert_eq!(
        log.token_shaped(),
        vec!["kourier_whatsapp::send".to_owned()],
        "only the one line of the crate that holds a whole run is flagged"
    );
    assert!(has_token_run(&"0123456789abcdef".repeat(2)));
    assert!(!has_token_run(
        &"0123456789abcdef".repeat(2)[..TOKEN_RUN - 1]
    ));
    assert!(!has_token_run(
        "synthetic text with spaces and words, nothing long"
    ));
}

async fn until_lines(log: &Capture, what: &str, found: impl Fn(&[support::capture::Line]) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !found(&log.lines()) {
        assert!(Instant::now() < deadline, "never seen: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The reach-out line of the facts (`LogFacts`; chunk 10 replaces it by the contract's event).
fn hold_line(lines: &[support::capture::Line], active: bool) -> bool {
    lines.iter().any(|l| {
        l.target.starts_with("kourier_whatsapp")
            && l.text.contains("whatsapp reach-out lock")
            && l.text.contains(&format!("active={active}"))
    })
}

/// An upstream WARN whose text holds `words`.
fn upstream_warn(lines: &[support::capture::Line], words: &str) -> bool {
    lines
        .iter()
        .any(|l| l.target.starts_with("log:") && l.text.contains(words))
}

const PRIVACY_FAILED: &str = "Failed to fetch privacy settings";
const DEVICES_FAILED: &str = "Failed sync own device list";

/// A resumed account over the Wire, answered by the rules (`answered`) or by the 503 alone (the
/// control: the same checks then see the failures, so they can see something).
async fn the_client_reads_the_rigs_answers_after_its_login(log: &Capture, answered: bool) {
    let (dir, _device) = account_dir().await;
    let server = wire::server();
    server.answering();
    if answered {
        server
            .on_iq(
                "usync",
                wire::usync(
                    vec![
                        (peer::ACCOUNT_PN, vec![0, 1]),
                        (peer::ACCOUNT_LID, vec![0, 1]),
                    ],
                    Vec::new(),
                ),
            )
            .on_iq("privacy", wire::privacy("all"))
            .on_iq("w:mex", wire::reach_out(Some(LOCK_UNTIL)));
    } else {
        server.on_iq("w:mex", wire::reach_out(None));
    }
    let mut actor = start_actor(dir.path(), StartMode::Resume, &server);
    actor.send(Command::Connect).await;
    assert!(
        server.wait_opened(1, Duration::from_secs(15)).await,
        "the handshake never completed"
    );
    server.send(&wire::success_as_the_server_writes_it(peer::ACCOUNT_LID));
    until_lines(log, "the reach-out lock stated", |l| hold_line(l, answered)).await;
    assert_eq!(
        server.answered("w:mex"),
        1,
        "one pull, answered by the rule"
    );
    if answered {
        let deadline = Instant::now() + Duration::from_secs(20);
        while server.answered("usync") == 0 || server.answered("privacy") == 0 {
            assert!(
                Instant::now() < deadline,
                "the client's own queries were not answered (usync {}, privacy {})",
                server.answered("usync"),
                server.answered("privacy")
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // The answers are read after they arrive: a moment for the client to fail if it could not.
        tokio::time::sleep(Duration::from_secs(1)).await;
        let lines = log.lines();
        assert!(
            !upstream_warn(&lines, PRIVACY_FAILED),
            "upstream could not read the privacy answer"
        );
        assert!(
            !upstream_warn(&lines, DEVICES_FAILED),
            "upstream could not read the own-device answer"
        );
    } else {
        until_lines(log, "the failed privacy and device queries", |l| {
            upstream_warn(l, PRIVACY_FAILED) && upstream_warn(l, DEVICES_FAILED)
        })
        .await;
    }
    // The whole frames: the own-device query is there with every child, the same frames as `frames`.
    let nodes = server.frame_nodes();
    assert_eq!(
        nodes.len(),
        server.frames().len(),
        "one whole node per frame"
    );
    let usync = nodes
        .iter()
        .find(|n| n.attrs.get("xmlns").is_some_and(|v| v.as_str() == "usync"))
        .expect("the own-device query was sent");
    let query = usync
        .get_optional_child("usync")
        .and_then(|u| u.get_optional_child("query"))
        .expect("the query's children are kept");
    assert!(query.get_optional_child("devices").is_some());

    actor.send(Command::Shutdown).await;
    assert_eq!(actor.exit().await.0, Exit::Shutdown);
}

/// The text the client sends in the decrypt cases (D10: "synthetic " is one of the secrets) and the
/// ids of the two sends (no digit run of the fixture's numbers).
const SENT_TEXT: &str = "synthetic rig text 70318452";
const SENT_ID: &str = "3EB0C0DE0000A2B1";
/// The peer's own synthetic key ids (a responder's signed and one-time prekeys).
const PEER_SIGNED_ID: u32 = 9;
const PEER_ONE_TIME_ID: u32 = 80_917;

/// The `<message>` frame the client wrote with `id`, waited for (at most 20 s).
async fn sent_frame(server: &wire::Wire, id: &str) -> Node {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(frame) = server
            .frame_nodes()
            .into_iter()
            .find(|n| n.tag == "message" && n.attrs.get("id").is_some_and(|v| v.as_str() == id))
        {
            return frame;
        }
        assert!(
            Instant::now() < deadline,
            "the client wrote no frame for the send"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The `<enc type>` of the peer's `<to>` in `frame`.
fn enc_type_to(frame: &Node, device: &Jid) -> Option<String> {
    frame
        .get_optional_child("participants")?
        .get_children_by_tag("to")
        .find(|to| {
            to.attrs
                .get("jid")
                .and_then(|v| v.as_str().parse::<Jid>().ok())
                .as_ref()
                == Some(device)
        })?
        .get_optional_child("enc")?
        .attrs
        .get("type")
        .map(|v| v.as_str().into_owned())
}

/// The account sends [`SENT_TEXT`] to the peer through upstream's own `Client::send` (the client handed
/// over by the fixture), and the peer reads it from the frame: `warm` = the peer wrote first (a `pkmsg`
/// of [`Peer::new`]) and the answer is a `msg` on that session; else the peer is a [`Peer::responder`]
/// and the client's frame is a `pkmsg` made from the bundle the `encrypt` rule handed out.
async fn the_peer_reads_what_the_client_sends(warm: bool) {
    let (dir, device) = account_dir().await;
    let server = wire::server();
    let mut peer = if warm {
        Peer::new(&device, None).await
    } else {
        Peer::responder(PEER_SIGNED_ID, PEER_ONE_TIME_ID).await
    };
    let peer_device = peer.jid().clone();
    server
        .on_iq(
            "usync",
            wire::usync(
                vec![
                    (peer::ACCOUNT_PN, vec![1]),
                    (peer::ACCOUNT_LID, vec![1]),
                    (peer::PEER_LID, vec![0]),
                ],
                Vec::new(),
            ),
        )
        .on_iq("privacy", wire::privacy("all"))
        .on_iq("w:mex", wire::reach_out(None))
        .ack_messages(Ack::Ok { t: 1_700_000_200 });
    if !warm {
        server.on_iq(
            "encrypt",
            wire::prekeys(vec![(peer_device.to_string(), peer.bundle())]),
        );
    }
    let slot = wire::ClientSlot::default();
    let mut actor = start_actor_handing_client(dir.path(), StartMode::Resume, &server, &slot);
    actor.send(Command::Connect).await;
    assert!(
        server.wait_opened(1, Duration::from_secs(15)).await,
        "the handshake never completed"
    );
    server.send(&wire::success_as_the_server_writes_it(peer::ACCOUNT_LID));
    server.send(&wire::hand_over(&peer_device.to_string()));
    let client = slot
        .wait(Duration::from_secs(20))
        .await
        .expect("the client was never handed over");
    if warm {
        // The peer writes first; the client's delivery receipt says the session is in its store.
        let first = peer.text("synthetic first word").await;
        let first_id = first
            .attrs
            .get("id")
            .map(|v| v.as_str().into_owned())
            .expect("the peer's id");
        server.send(&first);
        let deadline = Instant::now() + Duration::from_secs(20);
        while !server
            .frames()
            .iter()
            .any(|f| f.tag == "receipt" && f.attr("id") == Some(first_id.as_str()))
        {
            assert!(
                Instant::now() < deadline,
                "the peer's first message was never receipted"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    // The login's own-device query answered and applied before the send reads the registry.
    let deadline = Instant::now() + Duration::from_secs(20);
    while server.answered("usync") == 0 {
        assert!(
            Instant::now() < deadline,
            "our own devices were never asked"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let to: Jid = format!("{}@lid", peer::PEER_LID).parse().expect("the chat");
    let sent = client
        .send(
            whatsapp_rust::SendRequest::new(&to, waproto::whatsapp::Message::text(SENT_TEXT))
                .with_options(
                    whatsapp_rust::SendOptions::default()
                        .with_message_id(whatsapp_rust::MessageId::new(SENT_ID).expect("an id")),
                ),
        )
        .await;
    // Compared, not printed: an error of the send may name the chat.
    assert!(sent.is_ok(), "upstream did not send the text");
    drop(client);
    let frame = sent_frame(&server, SENT_ID).await;
    assert_eq!(
        enc_type_to(&frame, &peer_device).as_deref(),
        Some(if warm { "msg" } else { "pkmsg" }),
        "the peer's <enc> is not of the session's kind"
    );
    if !warm {
        assert!(
            server.answered("encrypt") >= 1,
            "the peer's bundle was never fetched"
        );
    }
    let read = peer.read(&frame).await.expect("the peer reads the frame");
    assert!(
        read.conversation.as_deref() == Some(SENT_TEXT),
        "the peer read another text than the one sent"
    );
    // The same frame read twice is a replay to the peer's ratchet: refused, as a phone refuses it.
    assert!(
        peer.read(&frame).await.is_err(),
        "a replayed frame decrypted again"
    );
    assert!(
        frame
            .get_optional_child("participants")
            .is_some_and(|p| p.get_children_by_tag("to").count() == 1),
        "the frame is for the peer alone (our own device list is this device)"
    );

    actor.send(Command::Shutdown).await;
    assert_eq!(actor.exit().await.0, Exit::Shutdown);
}

/// The own phone's synthetic key ids (its responder bundle in the `encrypt` rule).
const PHONE_SIGNED_ID: u32 = 11;
const PHONE_ONE_TIME_ID: u32 = 61_407;
const COPY_ID: &str = "3EB0C0DE0000C3D4";

/// The account sends a text to the peer (a cold chat) and its OWN PHONE gets the copy: the client's
/// frame has a `<to>` for the phone (device 0 of the account's LID user) whose `<enc>` the phone reads
/// as a `DeviceSentMessage` naming the chat and holding the text (brief §5: the phone's copy).
async fn the_phone_gets_its_copy() {
    let (dir, _device) = account_dir().await;
    let server = wire::server();
    let mut peer = Peer::responder(PEER_SIGNED_ID, PEER_ONE_TIME_ID).await;
    let mut phone = Peer::own_phone_responder(PHONE_SIGNED_ID, PHONE_ONE_TIME_ID).await;
    let peer_device = peer.jid().clone();
    let phone_device = phone.jid().clone();
    server
        .on_iq(
            "usync",
            wire::usync(
                vec![
                    (peer::ACCOUNT_PN, vec![0, 1]),
                    (peer::ACCOUNT_LID, vec![0, 1]),
                    (peer::PEER_LID, vec![0]),
                ],
                Vec::new(),
            ),
        )
        .on_iq("privacy", wire::privacy("all"))
        .on_iq("w:mex", wire::reach_out(None))
        .on_iq(
            "encrypt",
            wire::prekeys(vec![
                (peer_device.to_string(), peer.bundle()),
                (phone_device.to_string(), phone.bundle()),
            ]),
        )
        .ack_messages(Ack::Ok { t: 1_700_000_200 });
    let slot = wire::ClientSlot::default();
    let mut actor = start_actor_handing_client(dir.path(), StartMode::Resume, &server, &slot);
    actor.send(Command::Connect).await;
    assert!(
        server.wait_opened(1, Duration::from_secs(15)).await,
        "the handshake never completed"
    );
    server.send(&wire::success_as_the_server_writes_it(peer::ACCOUNT_LID));
    server.send(&wire::hand_over(&peer_device.to_string()));
    let client = slot
        .wait(Duration::from_secs(20))
        .await
        .expect("the client was never handed over");
    let deadline = Instant::now() + Duration::from_secs(20);
    while server.answered("usync") == 0 {
        assert!(
            Instant::now() < deadline,
            "our own devices were never asked"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let to: Jid = format!("{}@lid", peer::PEER_LID).parse().expect("the chat");
    let sent = client
        .send(
            whatsapp_rust::SendRequest::new(&to, waproto::whatsapp::Message::text(SENT_TEXT))
                .with_options(
                    whatsapp_rust::SendOptions::default()
                        .with_message_id(whatsapp_rust::MessageId::new(COPY_ID).expect("an id")),
                ),
        )
        .await;
    assert!(sent.is_ok(), "upstream did not send the text");
    drop(client);
    let frame = sent_frame(&server, COPY_ID).await;
    let read = peer.read(&frame).await.expect("the peer reads the frame");
    assert!(
        read.conversation.as_deref() == Some(SENT_TEXT),
        "the peer read another text than the one sent"
    );
    assert_eq!(
        enc_type_to(&frame, &phone_device).as_deref(),
        Some("pkmsg"),
        "the frame has no pkmsg for our own phone"
    );
    let copy = phone.read(&frame).await.expect("the phone reads its copy");
    let sent_copy = copy
        .device_sent_message
        .as_option()
        .expect("the phone's copy is a DeviceSentMessage");
    assert!(
        sent_copy.destination_jid.as_deref() == Some(to.to_string().as_str()),
        "the copy names another chat"
    );
    assert!(
        sent_copy
            .message
            .as_option()
            .and_then(|m| m.conversation.as_deref())
            == Some(SENT_TEXT),
        "the copy holds another text"
    );
    assert!(
        peer.read(&frame).await.is_err() && phone.read(&frame).await.is_err(),
        "a replayed frame decrypted again"
    );

    actor.send(Command::Shutdown).await;
    assert_eq!(actor.exit().await.0, Exit::Shutdown);
}

// --- W5b chunk B2: the media half of the rig (the `w:m` rule and the CDN fake, `support::cdn`) ---

/// The text of the captioned document (D10: "synthetic " is one of the secrets).
const MEDIA_CAPTION: &str = "synthetic rig caption 50628137";
/// The ids of the two media sends.
const PICTURE_ID: &str = "3EB0C0DE0000E5F6";
const DOCUMENT_ID: &str = "3EB0C0DE0000F7A8";
/// The fixed media key of the logged upload (the parent derives the same ciphertext).
const LOGGED_KEY: [u8; 32] = [0x4D; 32];
/// Where the logged child writes the CDN's log.
const CDN_LOG: &str = "WA_RIG_CDN_LOG";

/// `len` synthetic bytes (no file of anybody's).
fn plaintext(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// The ciphertext and the facts of `plain` as upstream encrypts it (a fresh key, or `key`).
fn encrypted(
    plain: &[u8],
    kind: MediaType,
    key: Option<&[u8; 32]>,
) -> (std::sync::Arc<[u8]>, EncryptedMediaInfo) {
    let mut cipher = Vec::new();
    let info =
        encrypt_media_streaming_with_key(std::io::Cursor::new(plain), &mut cipher, kind, key, None)
            .expect("upstream encrypts");
    (cipher.into(), info)
}

#[test]
fn the_media_conn_rule_answers_auth_ttl_and_hosts_upstream_reads() {
    let rule = wire::media_conn(cdn::AUTH, &[cdn::HOST_A, cdn::HOST_B], 300);
    let spec = MediaConnSpec::new();
    let parsed = spec
        .parse_response(&result_of(rule(&request_of(&spec))).as_node_ref())
        .expect("upstream reads the media connection");
    assert!(parsed.auth == cdn::AUTH, "another auth than the rule's");
    assert_eq!(parsed.ttl, 300);
    assert_eq!(
        parsed
            .hosts
            .iter()
            .map(|h| h.hostname.as_str())
            .collect::<Vec<_>>(),
        vec![cdn::HOST_A, cdn::HOST_B],
        "both hosts, in order"
    );
    let other = NodeBuilder::new("iq").attr("xmlns", "w:m").build();
    assert!(
        rule(&other).is_none(),
        "an IQ without <media_conn> is left alone"
    );
}

/// The media key types of a fake media part, for `download` without the Wire: the oracle must refuse
/// a blob decrypted under the uploader's type when the message's shape says another.
#[test]
fn the_peers_download_takes_the_key_type_from_the_messages_shape() {
    let fake = Cdn::default();
    let plain = plaintext(3_000, 7);
    // Uploaded under the VIDEO type, sent as a document: a receiver cannot decrypt it.
    let (cipher, info) = encrypted(&plain, MediaType::Video, None);
    let url = format!(
        "https://{}/mms/video/t?auth={}&token=t",
        cdn::HOST_A,
        cdn::AUTH
    );
    let response = fake
        .execute_upload(
            HttpRequest::post(url),
            Box::new(std::io::Cursor::new(cipher.to_vec())),
            cipher.len() as u64,
        )
        .expect("the fake answers");
    let answer: serde_json::Value = serde_json::from_slice(&response.body).expect("json");
    let direct_path = answer["direct_path"].as_str().expect("a path").to_owned();
    let mut doc = wa::message::DocumentMessage::default();
    doc.direct_path = Some(direct_path.clone());
    doc.media_key = Some(info.media_key.to_vec());
    doc.file_sha256 = Some(info.file_sha256.to_vec());
    doc.file_enc_sha256 = Some(info.file_enc_sha256.to_vec());
    doc.file_length = Some(info.file_length);
    doc.mimetype = Some("application/octet-stream".into());
    let mut as_document = wa::Message::default();
    as_document.document_message = MessageField::some(doc);
    assert_eq!(
        fake.download(&as_document).err(),
        Some("the blob does not decrypt under the message's key and type")
    );
    let mut video = wa::message::VideoMessage::default();
    video.direct_path = Some(direct_path);
    video.media_key = Some(info.media_key.to_vec());
    video.file_sha256 = Some(info.file_sha256.to_vec());
    video.file_enc_sha256 = Some(info.file_enc_sha256.to_vec());
    video.file_length = Some(info.file_length);
    video.mimetype = Some("video/mp4".into());
    let mut as_video = wa::Message::default();
    as_video.video_message = MessageField::some(video);
    let (kind, got) = fake.download(&as_video).expect("the right shape decrypts");
    assert_eq!(kind, MediaType::Video);
    assert!(got == plain, "another plaintext");
}

/// A resumed account over the Wire with the `w:m` rule and the CDN fake as the clients' HTTP client; the
/// client is handed to the test. Returns what the case needs; `peer` gets the cold chat's bundle.
async fn media_rig(
    dir: &std::path::Path,
    peer: &Peer,
    fake: &Cdn,
) -> (
    wire::Wire,
    crate::actor_real::Running,
    std::sync::Arc<whatsapp_rust::Client>,
) {
    let server = wire::server();
    let peer_device = peer.jid().clone();
    server
        .on_iq(
            "usync",
            wire::usync(
                vec![
                    (peer::ACCOUNT_PN, vec![1]),
                    (peer::ACCOUNT_LID, vec![1]),
                    (peer::PEER_LID, vec![0]),
                ],
                Vec::new(),
            ),
        )
        .on_iq("privacy", wire::privacy("all"))
        .on_iq("w:mex", wire::reach_out(None))
        .on_iq(
            "w:m",
            wire::media_conn(cdn::AUTH, &[cdn::HOST_A, cdn::HOST_B], 300),
        )
        .on_iq(
            "encrypt",
            wire::prekeys(vec![(peer_device.to_string(), peer.bundle())]),
        )
        .ack_messages(Ack::Ok { t: 1_700_000_200 });
    let slot = wire::ClientSlot::default();
    let for_maker = fake.clone();
    let actor = start_actor_with_http(dir, StartMode::Resume, &server, &slot, move || {
        for_maker.clone()
    });
    actor.send(Command::Connect).await;
    assert!(
        server.wait_opened(1, Duration::from_secs(15)).await,
        "the handshake never completed"
    );
    server.send(&wire::success_as_the_server_writes_it(peer::ACCOUNT_LID));
    server.send(&wire::hand_over(&peer_device.to_string()));
    let client = slot
        .wait(Duration::from_secs(20))
        .await
        .expect("the client was never handed over");
    let deadline = Instant::now() + Duration::from_secs(20);
    while server.answered("usync") == 0 {
        assert!(
            Instant::now() < deadline,
            "our own devices were never asked"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    (server, actor, client)
}

/// The media fields of `r` into an image message (a synthetic mimetype: the bytes are no picture).
fn image_of(r: &UploadResponse) -> wa::Message {
    let mut image = wa::message::ImageMessage::default();
    image.url = Some(r.url.clone());
    image.direct_path = Some(r.direct_path.clone());
    image.media_key = Some(r.media_key.to_vec());
    image.file_sha256 = Some(r.file_sha256.to_vec());
    image.file_enc_sha256 = Some(r.file_enc_sha256.to_vec());
    image.file_length = Some(r.file_length);
    image.media_key_timestamp = Some(r.media_key_timestamp);
    image.mimetype = Some("image/jpeg".into());
    let mut message = wa::Message::default();
    message.image_message = MessageField::some(image);
    message
}

/// The media fields of `r` into a document with a caption, in the wrapper the phones send it in.
fn captioned_document_of(r: &UploadResponse) -> wa::Message {
    let mut doc = wa::message::DocumentMessage::default();
    doc.url = Some(r.url.clone());
    doc.direct_path = Some(r.direct_path.clone());
    doc.media_key = Some(r.media_key.to_vec());
    doc.file_sha256 = Some(r.file_sha256.to_vec());
    doc.file_enc_sha256 = Some(r.file_enc_sha256.to_vec());
    doc.file_length = Some(r.file_length);
    doc.media_key_timestamp = Some(r.media_key_timestamp);
    doc.mimetype = Some("application/pdf".into());
    doc.file_name = Some("rig.pdf".into());
    doc.caption = Some(MEDIA_CAPTION.into());
    let mut inner = wa::Message::default();
    inner.document_message = MessageField::some(doc);
    let mut wrapper = wa::message::FutureProofMessage::default();
    wrapper.message = MessageField::some(inner);
    let mut message = wa::Message::default();
    message.document_with_caption_message = MessageField::some(wrapper);
    message
}

/// The `mediatype` of the `<enc>` of `frame`'s first `<to>`.
fn enc_mediatype(frame: &Node) -> Option<String> {
    frame
        .get_optional_child("participants")?
        .get_children_by_tag("to")
        .next()?
        .get_optional_child("enc")?
        .attrs
        .get("mediatype")
        .map(|v| v.as_str().into_owned())
}

/// Sends `message` under `id` with upstream's own send and returns what the peer read of the frame.
async fn send_and_read(
    client: &whatsapp_rust::Client,
    server: &wire::Wire,
    peer: &mut Peer,
    message: wa::Message,
    id: &str,
) -> (Node, wa::Message) {
    let to: Jid = format!("{}@lid", peer::PEER_LID).parse().expect("the chat");
    let sent = client
        .send(
            whatsapp_rust::SendRequest::new(&to, message).with_options(
                whatsapp_rust::SendOptions::default()
                    .with_message_id(whatsapp_rust::MessageId::new(id).expect("an id")),
            ),
        )
        .await;
    assert!(sent.is_ok(), "upstream did not send the media message");
    let frame = sent_frame(server, id).await;
    let read = peer.read(&frame).await.expect("the peer reads the frame");
    (frame, read)
}

/// THE MEDIA RIG over the Wire (B2): upstream's own `upload_stream` through the CDN fake, the peer's
/// download of what it received (G18), the 500 to the next host, the 401 refetch, the resume of a
/// broken 6 MiB POST and the `complete` answer (G16).
async fn the_peer_downloads_what_the_client_uploads() {
    let (dir, _device) = account_dir().await;
    let mut peer = Peer::responder(PEER_SIGNED_ID, PEER_ONE_TIME_ID).await;
    let fake = Cdn::default();
    let (server, mut actor, client) = media_rig(dir.path(), &peer, &fake).await;

    // A picture: a 500 on host A, host B takes it; ONE w:m answer serves the upload.
    fake.reply(Reply::Status(500)).reply(Reply::Ok);
    let plain = plaintext(3_000, 11);
    let (cipher, info) = encrypted(&plain, MediaType::Image, None);
    let picture = client
        .upload_stream(cipher.clone(), info, MediaType::Image)
        .await
        .map_err(|_| "upload failed")
        .expect("the picture is uploaded");
    assert_eq!(server.answered("w:m"), 1, "one media connection asked");
    let posts = fake.posts();
    assert_eq!(posts.len(), 2);
    assert_eq!(
        (posts[0].host.as_str(), posts[0].status),
        (cdn::HOST_A, Some(500))
    );
    assert_eq!(
        (posts[1].host.as_str(), posts[1].status),
        (cdn::HOST_B, Some(200))
    );
    for post in &posts {
        assert_eq!(post.path, "/mms/image");
        assert!(
            post.token_in_path && post.auth_ok,
            "token or auth not as upstream writes them"
        );
        assert_eq!(post.keys, vec!["auth", "token"]);
        assert_eq!(post.content_length, cipher.len() as u64, "the exact length");
        assert_eq!(post.received, cipher.len() as u64);
    }
    assert!(fake.checks().is_empty(), "no resume check below 5 MiB");
    let (frame, read) =
        send_and_read(&client, &server, &mut peer, image_of(&picture), PICTURE_ID).await;
    assert_eq!(
        frame
            .attrs
            .get("type")
            .map(|v| v.as_str().into_owned())
            .as_deref(),
        Some("media")
    );
    assert_eq!(enc_mediatype(&frame).as_deref(), Some("image"));
    let (kind, got) = fake
        .download(&read)
        .expect("the peer downloads the picture");
    assert_eq!(kind, MediaType::Image);
    assert!(got == plain, "the peer got other bytes");

    // A document with its caption in the wrapper; a 401 first: one more w:m, host A again.
    fake.reply(Reply::Status(401)).reply(Reply::Ok);
    let plain = plaintext(5_000, 23);
    let (cipher, info) = encrypted(&plain, MediaType::Document, None);
    let document = client
        .upload_stream(cipher, info, MediaType::Document)
        .await
        .map_err(|_| "upload failed")
        .expect("the document is uploaded");
    assert_eq!(
        server.answered("w:m"),
        2,
        "the 401 asked for the media connection again"
    );
    let posts = fake.posts();
    assert_eq!(
        posts[2..]
            .iter()
            .map(|p| (p.host.as_str(), p.path.as_str(), p.status))
            .collect::<Vec<_>>(),
        vec![
            (cdn::HOST_A, "/mms/document", Some(401)),
            (cdn::HOST_A, "/mms/document", Some(200)),
        ]
    );
    let (frame, read) = send_and_read(
        &client,
        &server,
        &mut peer,
        captioned_document_of(&document),
        DOCUMENT_ID,
    )
    .await;
    assert_eq!(enc_mediatype(&frame).as_deref(), Some("document"));
    let inner = read
        .document_with_caption_message
        .as_option()
        .and_then(|w| w.message.as_option())
        .and_then(|m| m.document_message.as_option())
        .expect("the wrapper arrives");
    assert!(
        inner.caption.as_deref() == Some(MEDIA_CAPTION),
        "another caption"
    );
    let (kind, got) = fake
        .download(&read)
        .expect("the peer downloads the document");
    assert_eq!(kind, MediaType::Document);
    assert!(got == plain, "the peer got other bytes");

    // 6 MiB of video: the POST to host A breaks at 2 MiB; host B's check says "resume at 1 MiB"
    // and the POST from there completes the file (G16).
    let before = fake.posts().len();
    fake.reply(Reply::FailAt(2 * 1024 * 1024))
        .reply(Reply::Ok)
        .check(Check::NotFound)
        .check(Check::Resume(1024 * 1024));
    let plain = plaintext(6 * 1024 * 1024, 37);
    let key = [0x5A; 32];
    let (cipher, info) = encrypted(&plain, MediaType::Video, Some(&key));
    let video = client
        .upload_stream(cipher.clone(), info, MediaType::Video)
        .await
        .map_err(|_| "upload failed")
        .expect("the video is uploaded");
    let posts = fake.posts()[before..].to_vec();
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[0].read, BodyEnd::Broken);
    assert_eq!(posts[0].received, 2 * 1024 * 1024);
    assert_eq!(posts[1].host, cdn::HOST_B);
    assert_eq!(posts[1].file_offset, Some(1024 * 1024));
    assert_eq!(posts[1].content_length, cipher.len() as u64 - 1024 * 1024);
    assert_eq!(posts[1].keys, vec!["auth", "token", "file_offset"]);
    let checks = fake.checks();
    assert_eq!(
        checks
            .iter()
            .map(|c| (c.host.as_str(), c.answer))
            .collect::<Vec<_>>(),
        vec![
            (cdn::HOST_A, Check::NotFound),
            (cdn::HOST_B, Check::Resume(1024 * 1024)),
        ]
    );
    assert!(
        checks
            .iter()
            .all(|c| c.keys == vec!["auth", "token", "resume"])
    );
    assert!(
        fake.blob(&video.direct_path).as_deref() == Some(&cipher[..]),
        "the resumed blob is not the whole ciphertext"
    );
    let mut as_video = wa::message::VideoMessage::default();
    as_video.direct_path = Some(video.direct_path.clone());
    as_video.media_key = Some(video.media_key.to_vec());
    as_video.file_sha256 = Some(video.file_sha256.to_vec());
    as_video.file_enc_sha256 = Some(video.file_enc_sha256.to_vec());
    as_video.file_length = Some(video.file_length);
    as_video.mimetype = Some("video/mp4".into());
    let mut message = wa::Message::default();
    message.video_message = MessageField::some(as_video);
    let (_, got) = fake.download(&message).expect("the resumed video decrypts");
    assert!(got == plain, "the resumed video is other bytes");

    // The same file again (same key, same token): the check says "complete", no POST, same place.
    let posts_before = fake.posts().len();
    fake.check(Check::Complete);
    let (cipher, info) = encrypted(&plain, MediaType::Video, Some(&key));
    let again = client
        .upload_stream(cipher, info, MediaType::Video)
        .await
        .map_err(|_| "upload failed")
        .expect("the complete answer is an upload");
    assert_eq!(
        fake.posts().len(),
        posts_before,
        "a complete file is not posted again"
    );
    assert!(
        again.direct_path == video.direct_path,
        "complete names another place"
    );
    assert_eq!(
        server.http_calls(),
        0,
        "the refusing client was never asked"
    );

    drop(client);
    actor.send(Command::Shutdown).await;
    assert_eq!(actor.exit().await.0, Exit::Shutdown);
}

/// THE KILLED CHILD'S UPLOAD (G19, the two-process self-test): the child uploads one picture under
/// [`LOGGED_KEY`] with the CDN's log on, and ABORTS the moment upstream returned. The parent reads the
/// log: the line was on disk before the answer, so a kill test's next child can trust it.
async fn the_cdn_logs_an_upload_before_it_answers() {
    let (dir, _device) = account_dir().await;
    let peer = Peer::responder(PEER_SIGNED_ID, PEER_ONE_TIME_ID).await;
    let fake = Cdn::default();
    fake.log_to(std::path::Path::new(
        &std::env::var(CDN_LOG).expect("the parent names the log"),
    ));
    let (_server, _actor, client) = media_rig(dir.path(), &peer, &fake).await;
    let (cipher, info) = encrypted(&plaintext(4_000, 41), MediaType::Image, Some(&LOGGED_KEY));
    let uploaded = client
        .upload_stream(cipher, info, MediaType::Image)
        .await
        .is_ok();
    assert!(uploaded, "the picture was not uploaded");
    eprintln!("uploaded: aborting");
    std::process::abort();
}

/// One parent per media case; the child's output holds none of the fixture's or the CDN's secrets.
fn media_parent(case: &str, env: &[(&str, &str)]) -> Option<(std::process::ExitStatus, String)> {
    if !support::offline::unshare_or_skip("this test never runs an actor on the real network") {
        return None;
    }
    let mut command = support::offline::child_command(module_path!(), "rig_child");
    command.env(CASE, case).envs(env.iter().copied());
    let out = command.output().expect("spawn the child under unshare");
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    for secret in secrets().into_iter().chain(cdn::secrets()) {
        assert!(
            !said.contains(secret),
            "the child printed a secret of the fixture"
        );
    }
    Some((out.status, said))
}

#[test]
fn the_peer_downloads_what_the_client_uploads_over_the_wire() {
    if let Some((status, said)) = media_parent("media", &[]) {
        assert!(
            status.success() && said.contains("test result: ok. 1 passed;"),
            "the child failed ({status:?}):\n{said}"
        );
    }
}

#[test]
fn a_killed_child_leaves_the_upload_it_was_answered_in_the_cdn_log() {
    use std::os::unix::process::ExitStatusExt;
    let tmp = tempfile::tempdir().expect("temp dir");
    let log = tmp.path().join("cdn.log");
    let Some((status, said)) =
        media_parent("logged", &[(CDN_LOG, log.to_str().expect("a utf-8 path"))])
    else {
        return;
    };
    assert_eq!(
        status.signal(),
        Some(6),
        "the child must abort: {status:?}\n{said}"
    );
    assert!(
        said.contains("uploaded: aborting"),
        "the child never got its answer:\n{said}"
    );
    let (cipher, _) = encrypted(&plaintext(4_000, 41), MediaType::Image, Some(&LOGGED_KEY));
    let lines = cdn::read_log(&log);
    assert_eq!(lines.len(), 1, "one upload, one line");
    assert_eq!(lines[0].path, "/mms/image");
    assert!(
        lines[0]
            .direct_path
            .starts_with(&format!("/v/t62.7118-24/{}", cdn::DIRECT_PATH_MARKER)),
        "the line names another place"
    );
    assert!(
        lines[0].sha256 == cdn::sha256_hex(&cipher),
        "the line names other bytes"
    );
}

/// One parent per case; the child's output holds none of the fixture's secrets (D10).
fn parent(case: &str) {
    let output = support::offline::run_child_output(module_path!(), "rig_child", &[(CASE, case)]);
    for secret in secrets() {
        assert!(
            !output.contains(secret),
            "the child printed one of the fixture's secrets"
        );
    }
}

#[test]
fn the_client_reads_the_rigs_answers_after_its_login_over_the_wire() {
    parent("answered");
}

#[test]
fn without_the_rules_the_client_sees_the_503_over_the_wire() {
    parent("unanswered");
}

#[test]
fn the_responder_peer_reads_the_clients_first_frame_over_the_wire() {
    parent("cold");
}

#[test]
fn the_initiator_peer_reads_the_clients_answer_over_the_wire() {
    parent("warm");
}

#[test]
fn our_phone_reads_its_copy_of_what_the_client_sends_over_the_wire() {
    parent("copy");
}

/// The child (a no-op as an ordinary test): every log line is recorded, and none may hold a secret.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rig_child() {
    if !support::offline::in_child() {
        return;
    }
    let log = Capture::default();
    log.install();
    let case = std::env::var(CASE).expect("the parent names the case");
    eprintln!("case {case}");
    match case.as_str() {
        "answered" => the_client_reads_the_rigs_answers_after_its_login(&log, true).await,
        "unanswered" => the_client_reads_the_rigs_answers_after_its_login(&log, false).await,
        "cold" => the_peer_reads_what_the_client_sends(false).await,
        "warm" => the_peer_reads_what_the_client_sends(true).await,
        "copy" => the_phone_gets_its_copy().await,
        "media" => the_peer_downloads_what_the_client_uploads().await,
        "logged" => the_cdn_logs_an_upload_before_it_answers().await,
        other => panic!("unknown case {other}"),
    }
    assert!(
        log.upstream_records() >= 1,
        "the `log` bridge heard nothing"
    );
    let all: Vec<&str> = secrets().into_iter().chain(cdn::secrets()).collect();
    assert_eq!(
        log.leaks(&all),
        Vec::<String>::new(),
        "a log line holds a secret of the fixture (the targets are named)"
    );
}
