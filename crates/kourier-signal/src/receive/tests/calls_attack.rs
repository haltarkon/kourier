//! Attacks on the call mapping of the Signal backend (`receive/calls.rs`), left by the
//! reviewer of the change. The era-id vectors are computed outside the code under test
//! (python3: `hashlib.sha256`, `struct.unpack('<q')`, `int(_, 16)`). The two ignored tests
//! state behaviour the review asks for and the code does not have yet; each says why.

use kourier_model::ConvKey;

use super::*;
use crate::receive::KIND_CONTENT;
use crate::receive::calls::{call_id_text, ring_id_of_era};
use crate::receive::dispatch::{Ctx, Outcome, dispatch};
use crate::receive::stage2::Row;

fn peer() -> Aci {
    Aci::from(Uuid::from_u128(0x3e6a_63a4_6d94_46f0_9ab4_8a35_c8f4_f5a1))
}

fn row(sender: ServiceId, timestamp: u64, content: &proto::Content) -> Row {
    Row {
        arrival: 7,
        received_at: 100,
        kind: KIND_CONTENT.to_owned(),
        sender: sender.service_id_string(),
        sender_device: 1,
        timestamp,
        server_timestamp: Some(i64::try_from(timestamp).unwrap() + 5),
        content: Some(content.encode_to_vec()),
        attempts: 0,
        sealed: true,
        decrypt_error: None,
        content_hint: None,
        group_id: None,
        delivered_at: None,
    }
}

/// The events of `dispatch`, the features of a chat included.
fn events(fixture: &Fixture, row: &Row) -> Vec<BackendEvent> {
    let outcome = fixture
        .db
        .zone(|tx| {
            dispatch(
                tx,
                &Ctx {
                    account: &fixture.account,
                    catch_up: false,
                    now: 1_700_000_060_123,
                    long_text: None,
                },
                row,
            )
        })
        .unwrap();
    match outcome {
        Outcome::Handled { events, .. } => events,
        failed @ Outcome::Failed { .. } => panic!("not handled: {failed:?}"),
    }
}

fn opaque_call(data: Vec<u8>, offer: Option<u64>) -> proto::Content {
    proto::Content {
        content: Some(proto::content::Content::CallMessage(proto::CallMessage {
            offer: offer.map(|id| proto::call_message::Offer {
                id: Some(id),
                r#type: None,
                opaque: None,
            }),
            opaque: Some(proto::call_message::Opaque {
                data: Some(data),
                urgency: None,
            }),
            ..Default::default()
        })),
        ..Default::default()
    }
}

/// Era ids and the call id text Signal Desktop prints for the same call
/// (`callIdFromEra(eraId).toString()`; the sync's uint64 `callId.toString()`).
#[test]
fn era_ids_computed_outside_the_code() {
    // Sixteen hex digits with the top bit set: the i64 is negative, the text is not.
    assert_eq!(ring_id_of_era("fedcba9876543210"), -81_985_529_216_486_896);
    assert_eq!(
        call_id_text(ring_id_of_era("fedcba9876543210")),
        "18364758544493064720"
    );
    assert_eq!(
        call_id_text(ring_id_of_era("8000000000000001")),
        "9223372036854775809"
    );
    // Zero is -1.
    assert_eq!(
        call_id_text(ring_id_of_era("0000000000000000")),
        "18446744073709551615"
    );
    // Sixteen characters that are no hex number: SHA-256, first eight bytes, little-endian.
    assert_eq!(
        ring_id_of_era("not-a-hex-era-id"),
        4_508_642_583_598_085_115
    );
    assert_eq!(
        call_id_text(ring_id_of_era("zzzzzzzzzzzzzzzz")),
        "15264388480390255008"
    );
}

/// Opaque bytes are the caller's: megabytes of nested messages, a ring cut short and a run
/// of group starts are no ring, no error and no line.
#[test]
fn hostile_opaque_bytes_are_no_ring_and_no_error() {
    let fixture = fixture();
    let sender = ServiceId::from(peer());
    let nested = [0x12, 0x7f].repeat(2 << 20);
    for data in [
        nested,
        vec![0x12, 45, 0x0a, 32, 1, 2, 3],
        vec![0x0b; 100_000],
    ] {
        let got = events(&fixture, &row(sender, T, &opaque_call(data, None)));
        assert!(got.is_empty(), "{} events", got.len());
    }
}

/// A `ring_intention` this version cannot read (a type RingRTC adds later, or one without
/// its ring id) beside an Offer is still a GROUP's signal: it must not become a 1:1 call of
/// the ringer that rings in the direct chat.
#[test]
fn a_ring_this_version_cannot_read_is_not_a_direct_call_of_the_ringer() {
    use crate::proto::ringrtc;
    let fixture = fixture();
    let sender = ServiceId::from(peer());
    let ring = |kind: Option<i32>, ring_id: Option<i64>| {
        ringrtc::CallMessage {
            group_call_message: None,
            ring_intention: Some(ringrtc::call_message::RingIntention {
                group_id: Some(vec![9; 32]),
                r#type: kind,
                ring_id,
            }),
        }
        .encode_to_vec()
    };
    for data in [ring(Some(2), Some(1)), ring(Some(0), None)] {
        let got = events(&fixture, &row(sender, T, &opaque_call(data, Some(1))));
        let direct_lines = got
            .iter()
            .filter(|event| {
                matches!(event, BackendEvent::Message(m)
                    if matches!(m.conv, ConvKey::Direct(_)) && m.body.call_line().is_some())
            })
            .count();
        assert_eq!(direct_lines, 0);
    }
}

/// The phone's call event for a contact this device has never seen. For a group the mapping
/// says "a line must not open a chat" and Desktop drops the event of an unknown conversation
/// (`callDisposition.preload.ts` `updateLocalCallHistory`); for a contact a chat is opened.
#[test]
fn a_call_event_for_a_contact_never_seen_opens_no_chat() {
    use proto::sync_message::call_event::{Direction, Event, Type};
    let fixture = fixture();
    let own = ServiceId::from(fixture.account.aci);
    let event = proto::Content {
        content: Some(proto::content::Content::SyncMessage(proto::SyncMessage {
            content: Some(proto::sync_message::Content::CallEvent(
                proto::sync_message::CallEvent {
                    conversation_id: Some(Uuid::from_u128(0xfeed_f00d).as_bytes().to_vec()),
                    call_id: Some(5),
                    timestamp: Some(T),
                    r#type: Some(Type::AudioCall.into()),
                    direction: Some(Direction::Outgoing.into()),
                    event: Some(Event::Accepted.into()),
                },
            )),
            ..Default::default()
        })),
        ..Default::default()
    };
    let got = events(&fixture, &row(own, T, &event));
    assert!(
        !got.iter()
            .any(|event| matches!(event, BackendEvent::ConversationUpsert(_))),
        "a chat was opened"
    );
    assert_eq!(count(&fixture.db, "peers"), 0);
}
