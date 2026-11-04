//! P5c Share Contact through the real core (refresh §3.3, §6 item 19; SC1, SC3, BQ-7): the app's
//! core with the Signal backend against `@signalapp/mock-server`, the harness playing our phone
//! ("Myself"), Ada Lindqvist, Bruno Okafor (who shares his number in his profile) and Alice
//! (who does not).
//!
//! - [`a_contact_card_reaches_ada`]: Bruno's page offers his card (his profile name is known and
//!   his number may be shown); his click sends it into the chat with Ada; Ada's phone decodes ONE
//!   `DataMessage.contact` with Bruno's PROFILE name as `givenName`, his number as the one
//!   `Phone{MOBILE}`, and nothing else (no avatar, no organization, no email, no address, no
//!   body text). Alice's page says she does not share her number, and a `SendContact` for her
//!   is refused by the core: nothing reaches Ada (the number rule holds on the wire too).
//!
//! Names and numbers are compared in code, never printed. Skips with a message when Node ≥ 24 or
//! `tests/mock-harness/node_modules` is missing (`KOURIER_REQUIRE_MOCK_HARNESS=1` fails
//! instead).

use crate::support;

use std::collections::BTreeSet;
use std::time::Duration;

use serde_json::Value;

use kourier_core::facade::UiEvent;

use kourier_core::facade::{PersonDetail, Query, QueryData, ShareState, UiCommand};
use kourier_model::{AccountId, ConversationId, UserId};
use support::harness::{CreatePrimary, DeviceInfo, Primary, SendText};
use support::{App, WAIT, World, user};

/// `ContactDetails.Phone.Type.MOBILE` in `SignalService.proto`.
const MOBILE: u64 = 2;

/// `contact` writes to our device sealed and with their profile key (so Kourier fetches the
/// profile and can seal back), and our phone accepts the chat.
pub(crate) async fn chat_with(
    world: &World,
    app: &App,
    account: AccountId,
    device: &DeviceInfo,
    contact: &Primary,
    text: &str,
) -> ConversationId {
    world
        .harness
        .send_text(&SendText {
            sealed: true,
            with_profile_key: true,
            await_delivery: true,
            timeout: Some(60_000),
            ..SendText::new(&contact.aci, &device.device_ref, text)
        })
        .await
        .expect("sendText");
    let conv = app.conversation_with(account, &user(contact)).await;
    app.wait_text(conv, text).await;
    app.accept(world, account, device, contact).await
}

/// The names of the fields of a decoded protobuf message (the harness hands our phone's decoding over as
/// JSON) that hold something: not null, not an empty list, not an empty text, not an empty object. NAMES
/// only (SE3): a failed assertion prints which fields are set and never a value of a card. A field the
/// phone could not place (`$unknown`) shows up as a name when it holds anything.
pub(crate) fn fields_with_a_value(message: &Value) -> BTreeSet<&str> {
    let Some(fields) = message.as_object() else {
        return BTreeSet::new();
    };
    fields
        .iter()
        .filter(|(_, value)| match value {
            Value::Null => false,
            Value::Array(items) => !items.is_empty(),
            Value::String(text) => !text.is_empty(),
            Value::Object(inner) => !inner.is_empty(),
            Value::Bool(_) | Value::Number(_) => true,
        })
        .map(|(name, _)| name.as_str())
        .collect()
}

/// The exact set of field names a card message may carry (SE3): `fields_with_a_value(message) == names`.
pub(crate) fn assert_fields(message: &Value, names: &[&str], what: &str) {
    let want: BTreeSet<&str> = names.iter().copied().collect();
    assert_eq!(
        fields_with_a_value(message),
        want,
        "{what}: the fields that hold a value are not exactly the expected ones"
    );
}

/// How long the test waits to be sure nothing reached Ada.
const QUIET: Duration = Duration::from_millis(2000);

/// The person's page, asked again until its share state is `want`.
pub(crate) async fn page_where(
    app: &App,
    account: AccountId,
    person: &UserId,
    want: ShareState,
) -> PersonDetail {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let answer = app
            .handle
            .query(Query::Person {
                account,
                user: person.clone(),
            })
            .await
            .expect("the person query");
        let QueryData::Person(detail) = answer else {
            panic!("not a person answer");
        };
        let last = detail.as_ref().map(|d| {
            let c = d.contact.as_ref();
            (
                d.share,
                c.is_some_and(|c| c.profile_name.is_some()),
                c.and_then(|c| c.number_shown),
                d.shown_number().is_some(),
            )
        });
        if let Some(detail) = detail
            && detail.share == want
        {
            return *detail;
        }
        // The state only (share, has a profile name, number_shown, has a shown number): never
        // the name or the number.
        assert!(
            tokio::time::Instant::now() < deadline,
            "the page's share state never became {want:?}: {last:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_contact_card_reaches_ada() {
    let Some(world) = World::start().await else {
        return;
    };
    let ada = world
        .harness
        .create_primary(&CreatePrimary::named("Ada Lindqvist"))
        .await
        .expect("Ada");
    let bruno = world
        .harness
        .create_primary(&CreatePrimary {
            phone_number_sharing: Some(true),
            ..CreatePrimary::named("Bruno Okafor")
        })
        .await
        .expect("Bruno");
    let mut app = App::start(world.tmp.path(), &world.env).await;
    let (account, device) = app.link(&world).await;
    let alice_says = "hello from Alice";
    chat_with(&world, &app, account, &device, &world.alice, alice_says).await;
    chat_with(&world, &app, account, &device, &bruno, "hello from Bruno").await;
    let ada_conv = chat_with(&world, &app, account, &device, &ada, "hello from Ada").await;

    let bruno_id = user(&bruno);
    let page = page_where(&app, account, &bruno_id, ShareState::Ready).await;
    let card = page.contact_card().expect("a Ready page has a card");
    assert_eq!(
        card.name.as_deref(),
        Some(bruno.profile_name.as_str()),
        "the card's name is Bruno's profile name"
    );

    // His click: Bruno's card into the chat with Ada.
    app.send(UiCommand::SendContact {
        conv: ada_conv,
        account,
        user: bruno_id,
    });
    let got = world
        .harness
        .wait_for_data(&ada.aci, "message", None, None, WAIT)
        .await
        .expect("Ada got a message");
    let data = &got.data_message;
    let cards = data["contact"].as_array().expect("a contact field");
    assert_eq!(cards.len(), 1, "one card per message");
    let sent = &cards[0];
    assert_eq!(
        sent["name"]["givenName"].as_str(),
        Some(bruno.profile_name.as_str()),
        "givenName is Bruno's profile name"
    );
    // The EXACT shape (SE3; experiment E2 of the re-check read it off Ada's phone): the card holds a name and
    // one number and nothing else (no avatar, organization, email or address), the name only `givenName` (no
    // family name, prefix, suffix, middle name or nickname), the number a value and a type, and the message
    // only the card, the usual timer fields, the profile key and the timestamp (no body, no attachments, no
    // preview, no quote). `$unknown` holds nothing at any of the four levels.
    assert_fields(sent, &["name", "number"], "the card");
    assert_fields(&sent["name"], &["givenName"], "the card's name");
    assert_fields(&sent["number"][0], &["type", "value"], "the card's number");
    assert_fields(
        data,
        &[
            "contact",
            "expireTimer",
            "expireTimerVersion",
            "profileKey",
            "timestamp",
        ],
        "the message",
    );
    let numbers = sent["number"].as_array().expect("a number list");
    assert_eq!(numbers.len(), 1, "exactly one number");
    assert_eq!(
        numbers[0]["value"].as_str(),
        bruno.e164.as_deref(),
        "the number is Bruno's"
    );
    let kind = &numbers[0]["type"];
    assert!(
        kind.as_u64() == Some(MOBILE) || kind.as_str() == Some("MOBILE"),
        "a MOBILE phone"
    );
    assert!(
        data["body"].as_str().is_none_or(str::is_empty),
        "a card has no text"
    );
    assert!(
        data["attachments"].as_array().is_none_or(Vec::is_empty),
        "a card has no attachments"
    );
    assert!(got.sealed, "sealed like a text");

    // Alice does not share her number: no card for her, and the core refuses one asked anyway.
    let alice = user(&world.alice);
    page_where(&app, account, &alice, ShareState::NotShared).await;
    app.send(UiCommand::SendContact {
        conv: ada_conv,
        account,
        user: alice,
    });
    app.event("the core refused Alice's card", |e| match e {
        UiEvent::ActionFailed {
            action: "SendContact",
            ..
        } => Some(()),
        _ => None,
    })
    .await;
    assert!(
        world
            .harness
            .wait_for_data(&ada.aci, "message", None, None, QUIET)
            .await
            .is_err(),
        "nothing reached Ada for a person who does not share her number"
    );

    app.stop().await;
    world.shutdown().await;
}
