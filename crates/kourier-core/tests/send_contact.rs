//! P5c Share Contact (architecture §4.2): the core builds the card (their PROFILE name, the
//! number their page shows, nothing else: BQ-7, SC1, SC3) and queues it like a text, only into
//! a chat of the person's own account, on a network that sends cards (SC4).

use std::time::Duration;

use kourier_backend_api::{BackendEvent, OutgoingKind};
use kourier_core::facade::{
    ActionSubject, LinkState, MAX_CARD_NAME_BYTES, Query, QueryData, ShareState, UiCommand, UiEvent,
};
use kourier_model::{
    AccountId, Body, ContactCard, ContactPatch, ConvKey, ConversationId, ConversationPatch,
    Identifier, IdentifierKind, Patch, Text, UserId,
};

use crate::people::link_detail;
use crate::support::send::untrusted;
use crate::support::{Env, WAIT, add_linked, eventually};

const ME: &str = "aci:00000000-0000-4000-8000-00000000000a";
const OTHER_ME: &str = "aci:00000000-0000-4000-8000-00000000000b";
const ADA: &str = "aci:00000000-0000-4000-8000-0000000a11ce";
const BOB: &str = "aci:00000000-0000-4000-8000-000000000b0b";
const CAROL: &str = "aci:00000000-0000-4000-8000-00000000ca01";
const EVE: &str = "aci:00000000-0000-4000-8000-0000000000e5";
const DAN: &str = "aci:00000000-0000-4000-8000-000000000da4";
const NUMBER: &str = "+15550001111";

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

fn phone(n: &str) -> Vec<Identifier> {
    vec![Identifier {
        kind: IdentifierKind::Phone,
        value: n.into(),
    }]
}

fn named(
    user: &str,
    shown: Option<bool>,
    number: Option<&str>,
    name: Option<&str>,
) -> kourier_core::facade::PersonDetail {
    let mut d = link_detail(user, shown, number);
    if let Some(c) = d.contact.as_mut() {
        c.profile_name = name.map(str::to_owned);
    }
    d
}

/// The card is the profile name and the shown number, whatever else the row holds (his
/// address-book name and nickname stay his: SC1); every state of the button follows the page's
/// number rule first, then the name.
#[test]
fn the_card_is_the_profile_name_and_the_shown_number() {
    let mut ada = named(ADA, Some(true), Some(NUMBER), Some("Ada Lovelace"));
    if let Some(c) = ada.contact.as_mut() {
        c.system_name = Some("Ada (work)".into());
        c.nickname = Some("Addy".into());
    }
    assert_eq!(ShareState::of(&ada, true), ShareState::Ready);
    assert_eq!(
        ada.contact_card(),
        Some(ContactCard {
            name: Some("Ada Lovelace".into()),
            identifiers: phone(NUMBER),
            ..Default::default()
        }),
        "the profile name and the one shown number, no picture"
    );
    let card = ada.contact_card().expect("a card");
    assert!(
        !format!("{card:?}").contains("Ada") && !format!("{card:?}").contains("555"),
        "the card's Debug hides name and number"
    );

    // The number rule (Copy Link's sentences).
    let bob = named(BOB, Some(false), Some(NUMBER), Some("Bob"));
    assert_eq!(ShareState::of(&bob, true), ShareState::NotShared);
    assert_eq!(
        bob.contact_card(),
        None,
        "a number not shown never goes into a card"
    );
    let never_said = named(BOB, None, Some(NUMBER), Some("Bob"));
    assert_eq!(ShareState::of(&never_said, true), ShareState::NoNumber);
    assert_eq!(never_said.contact_card(), None);
    let unknown = named(BOB, Some(true), None, Some("Bob"));
    assert_eq!(ShareState::of(&unknown, true), ShareState::NoNumber);

    // SD3: ONE number rule with the link: a shown number that is not E.164 ("+", 7 to 15 digits)
    // is no number for the card either (the receivers would read it as a number of THEIR region).
    for (value, ready) in [
        ("+6834002", true),
        ("+123456789012345", true),
        ("+683400", false),
        ("+1234567890123456", false),
        ("15550001111", false),
        ("+0555000111", false),
        ("+1555 000 1111", false),
        ("+1555000111x", false),
    ] {
        let d = named(BOB, Some(true), Some(value), Some("Bob"));
        let link = LinkState::of(&d, true, |n| {
            kourier_backend_api::is_e164(n).then(|| n.to_owned())
        });
        assert_eq!(
            ShareState::of(&d, true) == ShareState::Ready,
            ready,
            "the card's verdict on a shown number of {} chars",
            value.len()
        );
        assert_eq!(
            matches!(link, LinkState::Link(_)),
            ready,
            "the link and the card give one verdict"
        );
        if !ready {
            assert_eq!(ShareState::of(&d, true), ShareState::NoNumber);
            assert_eq!(
                d.contact_card(),
                None,
                "no card for a number nobody can use"
            );
        }
    }

    // SC1: no profile name, no card (never the address-book name or the nickname).
    let mut nameless = named(ADA, Some(true), Some(NUMBER), None);
    if let Some(c) = nameless.contact.as_mut() {
        c.system_name = Some("Ada (work)".into());
        c.nickname = Some("Addy".into());
    }
    assert_eq!(ShareState::of(&nameless, true), ShareState::NoName);
    assert_eq!(nameless.contact_card(), None);
    let blank = named(ADA, Some(true), Some(NUMBER), Some("   "));
    assert_eq!(ShareState::of(&blank, true), ShareState::NoName);
    // SC3: Kourier's cap of 512 bytes.
    let longest = named(
        ADA,
        Some(true),
        Some(NUMBER),
        Some(&"é".repeat(MAX_CARD_NAME_BYTES / 2)),
    );
    assert_eq!(ShareState::of(&longest, true), ShareState::Ready);
    let long = named(
        ADA,
        Some(true),
        Some(NUMBER),
        Some(&format!("{}a", "é".repeat(MAX_CARD_NAME_BYTES / 2))),
    );
    assert_eq!(ShareState::of(&long, true), ShareState::NoName);
    assert_eq!(long.contact_card(), None);

    // A person known by the number alone: the id is the number, but a name is still needed.
    let mut by_number = named(&format!("e164:{NUMBER}"), None, None, Some("Num"));
    assert_eq!(ShareState::of(&by_number, true), ShareState::Ready);
    assert_eq!(
        by_number.contact_card().map(|c| c.identifiers),
        Some(phone(NUMBER))
    );
    by_number.contact = None;
    assert_eq!(ShareState::of(&by_number, true), ShareState::NoName);

    // BQ-4 and SC4: no button on the own page or on a network without cards.
    let mut own = ada.clone();
    own.is_self = true;
    assert_eq!(ShareState::of(&own, true), ShareState::Hidden);
    assert_eq!(own.contact_card(), None, "no card of the user himself");
    assert_eq!(ShareState::of(&ada, false), ShareState::Hidden);
}

/// Two linked accounts of the Signal-like fake: the first has Ada (shares her number, profile
/// name, his own names for her), Bob (does not share it) and a chat with Carol; the second has
/// Eve (shares it).
async fn scene() -> (Env, AccountId, AccountId, ConversationId) {
    scene_with(Vec::new(), Vec::new()).await
}

/// [`scene`] with more events delivered to the first account (more chats and contacts) and
/// transmit rules (failures the fake plays for its sends).
async fn scene_with(
    extra: Vec<BackendEvent>,
    rules: Vec<kourier_fake::TransmitRule>,
) -> (Env, AccountId, AccountId, ConversationId) {
    let mut ada = ContactPatch::new(uid(ADA));
    ada.profile_name = Patch::Set("Ada Lovelace".into());
    ada.system_name = Patch::Set("Ada (work)".into());
    ada.nickname = Patch::Set("Addy".into());
    ada.number_shown = Some(true);
    ada.identifiers = Some(phone(NUMBER));
    let mut bob = ContactPatch::new(uid(BOB));
    bob.profile_name = Patch::Set("Bob".into());
    bob.number_shown = Some(false);
    bob.identifiers = Some(phone("+15550002222"));
    let mut carol = ContactPatch::new(uid(CAROL));
    carol.profile_name = Patch::Set("Carol".into());
    let chat = ConversationPatch::new(ConvKey::Direct(uid(CAROL)));
    let mut events = vec![
        BackendEvent::ContactUpsert(ada),
        BackendEvent::ContactUpsert(bob),
        BackendEvent::ContactUpsert(carol),
        BackendEvent::ConversationUpsert(chat),
    ];
    events.extend(extra);
    let mut script = kourier_fake::Script::new("share-contact", vec![uid(ME)])
        .deliver(events)
        .step(kourier_fake::Step::Sleep(Duration::from_secs(3600)));
    for rule in rules {
        script = script.fail_transmit(rule);
    }
    let mut env = Env::new(kourier_fake::Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let first = add_linked(&mut env).await;
    let mut eve = ContactPatch::new(uid(EVE));
    eve.profile_name = Patch::Set("Eve".into());
    eve.number_shown = Some(true);
    eve.identifiers = Some(phone("+15550003333"));
    env.control.push_script(
        kourier_fake::Script::new("share-contact-2", vec![uid(OTHER_ME)])
            .deliver(vec![BackendEvent::ContactUpsert(eve)])
            .step(kourier_fake::Step::Sleep(Duration::from_secs(3600))),
    );
    let second = add_linked(&mut env).await;
    assert_ne!(first, second);
    let env_ref = &env;
    let page = |account: AccountId, user: &'static str| async move {
        match env_ref
            .handle
            .query(Query::Person {
                account,
                user: uid(user),
            })
            .await
        {
            Ok(QueryData::Person(Some(d))) if d.contact.is_some() => Some(d.share),
            _ => None,
        }
    };
    eventually("the contacts landed", WAIT, || async move {
        page(first, ADA).await == Some(ShareState::Ready)
            && page(first, BOB).await == Some(ShareState::NotShared)
            && page(second, EVE).await == Some(ShareState::Ready)
    })
    .await;
    let carol_chat = uid(CAROL);
    let conv =
        env.conversations().await.into_iter().find_map(|r| {
            (r.account == first && r.peer.as_ref() == Some(&carol_chat)).then_some(r.id)
        });
    let conv = conv.expect("Carol's chat");
    (env, first, second, conv)
}

/// The cards the fake was asked to transmit for `account`.
fn cards(env: &Env, uuid: kourier_model::AccountUuid) -> Vec<ContactCard> {
    env.control
        .transmits(uuid)
        .into_iter()
        .filter_map(|t| match t.req.kind {
            OutgoingKind::Message {
                body: Body::Contact(card),
                ..
            } => Some(card),
            _ => None,
        })
        .collect()
}

/// The runtime fills the page's state, and SendContact sends the card the core built (the
/// profile name, never his names for her; the shown number), leaving the chat's draft alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_card_goes_out_with_the_profile_name_and_the_shown_number() {
    let (env, first, _second, conv) = scene().await;
    let uuid = env.account(first).await.expect("account").uuid;
    env.handle
        .send(UiCommand::SaveDraft {
            conv,
            draft: Some(Text::plain("half a thought")),
        })
        .expect("send");
    env.handle
        .send(UiCommand::SendContact {
            conv,
            account: first,
            user: uid(ADA),
        })
        .expect("send");
    let env_ref = &env;
    eventually("the card was transmitted", WAIT, || async move {
        !cards(env_ref, uuid).is_empty()
    })
    .await;
    assert_eq!(
        cards(&env, uuid),
        vec![ContactCard {
            name: Some("Ada Lovelace".into()),
            identifiers: phone(NUMBER),
            ..Default::default()
        }]
    );
    let draft: Option<String> = env.peek(|c| {
        c.query_row(
            "SELECT draft FROM conversations WHERE id = ?1",
            [conv.0],
            |r| r.get(0),
        )
        .expect("draft")
    });
    assert!(draft.is_some(), "the card leaves the chat's draft alone");
}

/// Every refusal ends in `ActionFailed { "SendContact" }` and sends nothing: a person whose
/// page shows no number, the user himself, and a person of ANOTHER account (a card never leaves
/// through another account, even when that account knows her).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_contact_refusals() {
    let (mut env, first, second, conv) = scene().await;
    let uuid = env.account(first).await.expect("account").uuid;
    for (account, user, why) in [
        (first, BOB, "she does not share her number"),
        (first, ME, "the user's own card (BQ-4)"),
        (
            second,
            EVE,
            "a person of the other account, into this account's chat",
        ),
        (first, EVE, "a person this account does not know"),
    ] {
        env.handle
            .send(UiCommand::SendContact {
                conv,
                account,
                user: uid(user),
            })
            .expect("send");
        let subject = env
            .event(why, |e| match e {
                UiEvent::ActionFailed {
                    action: "SendContact",
                    subject,
                    ..
                } => Some(subject.clone()),
                _ => None,
            })
            .await;
        assert_eq!(subject, ActionSubject::Conversation(conv), "{why}");
    }
    // A last good card behind them proves the queue went past them and nothing else went out.
    env.handle
        .send(UiCommand::SendContact {
            conv,
            account: first,
            user: uid(ADA),
        })
        .expect("send");
    let env_ref = &env;
    eventually("the good card was transmitted", WAIT, || async move {
        !cards(env_ref, uuid).is_empty()
    })
    .await;
    assert_eq!(cards(&env, uuid).len(), 1, "only the good card went out");
}

/// P5c's picker lists only chats where the user can send: every row of `Query::Conversations`
/// says so from the chat's features (no `CannotSend`), a chat without features included.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_list_says_where_the_user_can_send() {
    let open = ConversationPatch::new(ConvKey::Direct(uid(ADA)));
    let mut left = ConversationPatch::new(ConvKey::Group("gv2:left".into()));
    left.title = Patch::Set("We left".into());
    left.features = Some(kourier_model::ConversationFeatures {
        cannot_send: Some(kourier_model::CannotSend::NotMember),
        ..Default::default()
    });
    let mut admins = ConversationPatch::new(ConvKey::Group("gv2:admins".into()));
    admins.title = Patch::Set("Admins only".into());
    admins.features = Some(kourier_model::ConversationFeatures {
        cannot_send: Some(kourier_model::CannotSend::AdminsOnly),
        ..Default::default()
    });
    let script = kourier_fake::Script::new("share-targets", vec![uid(ME)])
        .deliver(vec![
            BackendEvent::ConversationUpsert(open),
            BackendEvent::ConversationUpsert(left),
            BackendEvent::ConversationUpsert(admins),
        ])
        .step(kourier_fake::Step::Sleep(Duration::from_secs(3600)));
    let mut env = Env::new(kourier_fake::Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    add_linked(&mut env).await;
    let env_ref = &env;
    let rows = || async move {
        match env_ref
            .handle
            .query(Query::Conversations(
                kourier_core::facade::ConversationListQuery {
                    archived: None,
                    ..Default::default()
                },
            ))
            .await
        {
            Ok(QueryData::Conversations(page)) => page
                .rows
                .into_iter()
                .map(|r| (r.peer.is_some(), r.title.unwrap_or_default(), r.can_send))
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        }
    };
    eventually("the chats landed", WAIT, || async move {
        rows().await.len() == 3
    })
    .await;
    let mut got = rows().await;
    got.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(
        got,
        vec![
            (true, String::new(), true),
            (false, "Admins only".to_owned(), false),
            (false, "We left".to_owned(), false),
        ]
    );
}

/// Sends `SendContact` and waits for its refusal (`ActionFailed { "SendContact" }` on `conv`).
async fn refused(env: &mut Env, conv: ConversationId, account: AccountId, user: &str, why: &str) {
    env.handle
        .send(UiCommand::SendContact {
            conv,
            account,
            user: uid(user),
        })
        .expect("send");
    let subject = env
        .event(why, |e| match e {
            UiEvent::ActionFailed {
                action: "SendContact",
                subject,
                ..
            } => Some(subject.clone()),
            _ => None,
        })
        .await;
    assert_eq!(subject, ActionSubject::Conversation(conv), "{why}");
}

/// SD1 (P5c protocol review MEDIUM-1, SC4): a network without `contact_cards` refuses
/// `SendContact` in the core, not only by hiding the button. The account is on the fake PLAYING
/// WhatsApp (descriptor `contact_cards: false`) and the person passes every other check (shares
/// her number, has a profile name, the chat takes messages), so the descriptor's check alone
/// stands between the command and the wire: the refusal is `ActionFailed { "SendContact" }`,
/// nothing is queued and nothing is transmitted (kills: the check removed).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_network_without_cards_refuses_send_contact() {
    use crate::support::add_linked_as;
    use kourier_model::NetworkKind;

    let mut ada = ContactPatch::new(uid(ADA));
    ada.profile_name = Patch::Set("Ada Lovelace".into());
    ada.number_shown = Some(true);
    ada.identifiers = Some(phone(NUMBER));
    let chat = ConversationPatch::new(ConvKey::Direct(uid(CAROL)));
    let script = kourier_fake::Script::new("share-contact-whatsapp", vec![uid(ME)])
        .deliver(vec![
            BackendEvent::ContactUpsert(ada),
            BackendEvent::ConversationUpsert(chat),
        ])
        .step(kourier_fake::Step::Sleep(Duration::from_secs(3600)));
    let mut env = Env::playing(NetworkKind::WhatsApp, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked_as(&mut env, NetworkKind::WhatsApp).await;
    let env_ref = &env;
    let page = || async move {
        match env_ref
            .handle
            .query(Query::Person {
                account,
                user: uid(ADA),
            })
            .await
        {
            Ok(QueryData::Person(Some(d))) if d.contact.is_some() => Some(d),
            _ => None,
        }
    };
    eventually("her contact landed", WAIT, || async move {
        page().await.is_some()
    })
    .await;
    let detail = page().await.expect("her page");
    assert_eq!(
        ShareState::of(&detail, true),
        ShareState::Ready,
        "every check but the network's passes: the number rule and the name"
    );
    assert_eq!(
        detail.share,
        ShareState::Hidden,
        "and the runtime hides the button on a network without cards"
    );
    let carol = uid(CAROL);
    let conv = env
        .conversations()
        .await
        .into_iter()
        .find_map(|r| (r.peer.as_ref() == Some(&carol)).then_some(r.id))
        .expect("the chat");
    refused(
        &mut env,
        conv,
        account,
        ADA,
        "a card on a network without cards",
    )
    .await;
    let uuid = env.account(account).await.expect("account").uuid;
    assert!(
        cards(&env, uuid).is_empty(),
        "nothing was transmitted for the refused card"
    );
    assert!(
        env.timeline(conv).await.is_empty(),
        "and no message was queued"
    );
}

/// SD2 (P5c protocol review MEDIUM-2, the privacy gate): a card is never queued into a chat that
/// cannot take a message. The picker lists only sendable chats, but that is the UI's filter; the
/// outbox's `cannot_send` check is the core's own gate (a card into a chat the phone never
/// accepted would also hand our profile key to a stranger, features.rs `NotAccepted`). The
/// person passes every other check (Ada: a shown number and a profile name, the same account);
/// the chats are a 1:1 chat not accepted on the phone, a group we left and an admins-only group:
/// each is `ActionFailed { "SendContact" }` with nothing queued, and a good card into a chat
/// that takes messages behind them is the only one transmitted (kills: the check removed).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_card_is_never_queued_into_a_chat_that_cannot_send() {
    use kourier_model::{CannotSend, ConversationFeatures};

    let mut dan = ContactPatch::new(uid(DAN));
    dan.profile_name = Patch::Set("Dan".into());
    let mut unaccepted = ConversationPatch::new(ConvKey::Direct(uid(DAN)));
    unaccepted.features = Some(ConversationFeatures {
        cannot_send: Some(CannotSend::NotAccepted),
        ..Default::default()
    });
    let mut left = ConversationPatch::new(ConvKey::Group("gv2:left".into()));
    left.title = Patch::Set("We left".into());
    left.features = Some(ConversationFeatures {
        cannot_send: Some(CannotSend::NotMember),
        ..Default::default()
    });
    let mut admins = ConversationPatch::new(ConvKey::Group("gv2:admins".into()));
    admins.title = Patch::Set("Admins only".into());
    admins.features = Some(ConversationFeatures {
        cannot_send: Some(CannotSend::AdminsOnly),
        ..Default::default()
    });
    let (mut env, first, _second, carol_chat) = scene_with(
        vec![
            BackendEvent::ContactUpsert(dan),
            BackendEvent::ConversationUpsert(unaccepted),
            BackendEvent::ConversationUpsert(left),
            BackendEvent::ConversationUpsert(admins),
        ],
        Vec::new(),
    )
    .await;
    let uuid = env.account(first).await.expect("account").uuid;
    let env_ref = &env;
    eventually("the chats landed", WAIT, || async move {
        env_ref.conversations().await.len() == 4
    })
    .await;
    let rows = env.conversations().await;
    let dan_id = uid(DAN);
    let chats = [
        (
            rows.iter()
                .find(|r| r.peer.as_ref() == Some(&dan_id))
                .map(|r| (r.id, r.can_send)),
            "a 1:1 chat the phone never accepted",
        ),
        (
            rows.iter()
                .find(|r| r.title.as_deref() == Some("We left"))
                .map(|r| (r.id, r.can_send)),
            "a group we left",
        ),
        (
            rows.iter()
                .find(|r| r.title.as_deref() == Some("Admins only"))
                .map(|r| (r.id, r.can_send)),
            "a group where only admins write",
        ),
    ];
    for (found, why) in chats {
        let (conv, can_send) = found.unwrap_or_else(|| panic!("{why}: no row"));
        assert!(!can_send, "{why}: the list says it cannot take a message");
        refused(&mut env, conv, first, ADA, why).await;
        assert!(
            env.timeline(conv).await.is_empty(),
            "{why}: no message was queued"
        );
    }
    // A good card into a chat that takes messages: the queue went past the refusals and only it
    // reached the network.
    env.handle
        .send(UiCommand::SendContact {
            conv: carol_chat,
            account: first,
            user: uid(ADA),
        })
        .expect("send");
    let env_ref = &env;
    eventually("the good card was transmitted", WAIT, || async move {
        !cards(env_ref, uuid).is_empty()
    })
    .await;
    assert_eq!(cards(&env, uuid).len(), 1, "only the good card went out");
}

/// What the fake was asked to transmit for `account`, in order: "card" for a `Body::Contact`,
/// "text" for any other message body.
fn transmitted(env: &Env, uuid: kourier_model::AccountUuid) -> Vec<&'static str> {
    env.control
        .transmits(uuid)
        .into_iter()
        .filter_map(|t| match t.req.kind {
            OutgoingKind::Message {
                body: Body::Contact(_),
                ..
            } => Some("card"),
            OutgoingKind::Message { .. } => Some("text"),
            _ => None,
        })
        .collect()
}

/// SD4 (P5c protocol review LOW-4): a card queued behind a P5a pause waits like any message of
/// the chat. A text to Carol fails `UntrustedIdentity` (her safety number changed) and pauses
/// the chat's queue; the card the user shares into the chat meanwhile is queued BEHIND it and is
/// not transmitted until `ConfirmIdentity`; then the text goes first and the card after it
/// (the queue's order).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_card_waits_behind_a_paused_head_until_the_safety_number_is_confirmed() {
    let rule = kourier_fake::TransmitRule::attempts(untrusted(uid(CAROL)), 1)
        .in_conv(ConvKey::Direct(uid(CAROL)));
    let (mut env, first, _second, conv) = scene_with(Vec::new(), vec![rule]).await;
    let uuid = env.account(first).await.expect("account").uuid;
    env.handle
        .send(UiCommand::SendText {
            conv,
            text: Text::plain("the head"),
            reply_to: None,
        })
        .expect("send");
    let named = env
        .event("the identity notice", |e| match e {
            UiEvent::Notice {
                notice: kourier_backend_api::BackendNotice::IdentityChanged { user },
                ..
            } => Some(user.clone()),
            _ => None,
        })
        .await;
    assert_eq!(named, uid(CAROL), "the pause names whose number to confirm");
    env.handle
        .send(UiCommand::SendContact {
            conv,
            account: first,
            user: uid(ADA),
        })
        .expect("send");
    let env_ref = &env;
    eventually("the card is queued behind the head", WAIT, || async move {
        env_ref.timeline(conv).await.len() == 2
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        transmitted(&env, uuid),
        vec!["text"],
        "only the head's failed attempt: the card waits behind the pause"
    );
    env.handle
        .send(UiCommand::ConfirmIdentity {
            account: first,
            user: uid(CAROL),
            key: kourier_model::IdentityFingerprint(vec![5; 33]),
        })
        .expect("send");
    let env_ref = &env;
    eventually("the card went out after the confirm", WAIT, || async move {
        !cards(env_ref, uuid).is_empty()
    })
    .await;
    assert_eq!(
        transmitted(&env, uuid),
        vec!["text", "text", "card"],
        "the head's retry first, then the card: the queue's order"
    );
}

/// SD5 (P5c protocol review LOW-5): a reply to one of his own cards quotes it as Signal Android
/// quotes a card (`Contact.quoteText`): the text "👤 " and the card's NAME, so the receivers draw
/// a quote line, not an empty bubble; the number is never part of a quote.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reply_to_a_card_quotes_its_name_and_never_the_number() {
    let (env, first, _second, conv) = scene().await;
    let uuid = env.account(first).await.expect("account").uuid;
    env.handle
        .send(UiCommand::SendContact {
            conv,
            account: first,
            user: uid(ADA),
        })
        .expect("send");
    let env_ref = &env;
    eventually("the card was transmitted", WAIT, || async move {
        !cards(env_ref, uuid).is_empty()
    })
    .await;
    let card_message = env.control.transmits(uuid)[0].message;
    env.handle
        .send(UiCommand::SendText {
            conv,
            text: Text::plain("see below"),
            reply_to: Some(card_message),
        })
        .expect("send");
    let env_ref = &env;
    eventually("the reply was transmitted", WAIT, || async move {
        env_ref.control.transmits(uuid).len() >= 2
    })
    .await;
    let transmits = env.control.transmits(uuid);
    let OutgoingKind::Message { reply_to, .. } = &transmits[1].req.kind else {
        panic!("not a message");
    };
    let snapshot = reply_to
        .as_ref()
        .and_then(|r| r.snapshot.as_ref())
        .expect("a snapshot of the card");
    let quoted = snapshot.text.as_ref().map(|t| t.body.as_str());
    // Compared, never printed (a name is personal data even in a fixture).
    assert!(
        quoted == Some("\u{1F464} Ada Lovelace"),
        "the quote line of a card is its name behind the person emoji"
    );
    assert!(
        !quoted.unwrap_or_default().contains("555"),
        "never the number"
    );
    assert_eq!(snapshot.attachment, None, "a card has no attachment");
}
