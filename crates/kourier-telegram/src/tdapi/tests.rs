//! The serde subset over the scrubbed fixture of the pinned TDLib (TG-2 C4; critic M14) and over
//! synthetic lines. Nothing here prints a parsed value.

use serde_json::{Value, json};

use super::*;

const FLOW: &str = include_str!("../../testdata/login-flow.jsonl");

/// The synthetic number of the fixture (decisions: synthetic values only).
const OWN_NUMBER: &str = "+999068204719";

fn updates() -> Vec<Update> {
    FLOW.lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| l.contains("\"@type\":\"update"))
        .map(|l| parse_update(l).expect("every update of the fixture parses"))
        .collect()
}

/// Every `updateAuthorizationState` of the fixture is a known state, in TDLib's order, and
/// WaitCode's code info parses (type, length, no next type).
#[test]
fn every_authorization_state_of_the_fixture_parses() {
    let states: Vec<AuthorizationState> = updates()
        .into_iter()
        .filter_map(|u| match u {
            Update::AuthorizationState(s) => Some(s),
            _ => None,
        })
        .collect();
    let names: Vec<&str> = states.iter().map(AuthorizationState::name).collect();
    assert_eq!(
        names,
        [
            "WaitTdlibParameters",
            "WaitPhoneNumber",
            "WaitCode",
            "Ready",
            "Closing",
            "Closed"
        ]
    );
    match &states[2] {
        AuthorizationState::WaitCode { code_info } => {
            assert_eq!(code_info.r#type, CodeType::TelegramMessage { length: 5 });
            assert_eq!(code_info.next_type, None);
            assert_eq!(code_info.timeout, 0);
            assert!(
                code_info.phone_number == OWN_NUMBER,
                "not the synthetic number"
            );
        }
        _ => panic!("the third state is not WaitCode"),
    }
}

/// The fixture's connection states parse (Connecting, Updating, Ready twice).
#[test]
fn the_connection_states_of_the_fixture_parse() {
    let states: Vec<ConnectionState> = updates()
        .into_iter()
        .filter_map(|u| match u {
            Update::ConnectionState(s) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(states.len(), 4);
    for s in [
        ConnectionState::Connecting,
        ConnectionState::Updating,
        ConnectionState::Ready,
    ] {
        assert!(states.contains(&s), "{s:?} is missing");
    }
    assert!(!states.contains(&ConnectionState::Unknown));
}

/// M14: every `updateOption` line parses; every integer option is an int64 STRING in td_json and
/// is read as a number; `my_id` is the own user's id (100 in the fixture).
#[test]
fn every_option_parses_and_my_id_is_read_from_its_string() {
    let options: Vec<Update> = updates()
        .into_iter()
        .filter(|u| matches!(u, Update::Option { .. }))
        .collect();
    assert_eq!(options.len(), 136, "4 booleans, 129 integers, 3 strings");
    let integers = options
        .iter()
        .filter(|u| {
            matches!(
                u,
                Update::Option {
                    value: OptionValue::Integer { .. },
                    ..
                }
            )
        })
        .count();
    assert_eq!(integers, 129);
    assert!(
        options.iter().all(|u| !matches!(
            u,
            Update::Option {
                value: OptionValue::Unknown,
                ..
            }
        )),
        "an option value of an unknown type"
    );
    let ids: Vec<i64> = options.iter().filter_map(Update::my_id).collect();
    assert_eq!(ids, [100]);
    // The raw line holds the id as a string: a number reader would have missed it.
    let raw = FLOW
        .lines()
        .find(|l| l.contains("\"name\":\"my_id\""))
        .expect("my_id in the fixture");
    let v: Value = serde_json::from_str(raw).unwrap();
    assert!(v["value"]["value"].is_string());
    // Both spellings of an int64 are taken.
    let as_number = json!({"@type": "updateOption", "name": "my_id",
                           "value": {"@type": "optionValueInteger", "value": 7}});
    assert_eq!(
        parse_update(&as_number.to_string()).and_then(|u| u.my_id()),
        Some(7)
    );
}

/// The other updates of the fixture are `Other`; a line that is no JSON object with a `@type` is
/// `None`; a known type that does not parse is `None`.
#[test]
fn an_unknown_type_is_other_and_a_broken_line_is_none() {
    // TG-3 C1: a new message is a receive update now (every field defaulted); a send result
    // (T5) stays `Other`.
    assert_eq!(
        parse_update(r#"{"@type":"updateNewMessage","message":{}}"#),
        Some(Update::Receive(Receive::NewMessage(Box::default())))
    );
    assert_eq!(
        parse_update(r#"{"@type":"updateMessageSendSucceeded","message":{}}"#),
        Some(Update::Other)
    );
    assert_eq!(
        parse_update(r#"{"@type":"updateSomethingNew2030"}"#),
        Some(Update::Other)
    );
    assert_eq!(parse_update("not json"), None);
    assert_eq!(parse_update(r#"{"no_type":1}"#), None);
    assert_eq!(parse_update(r#"{"@type":"updateConnectionState"}"#), None);
    // An unknown state inside a known update is the state's `Unknown`.
    assert_eq!(
        parse_update(
            r#"{"@type":"updateAuthorizationState","authorization_state":{"@type":"authorizationStateWaitSomething"}}"#
        ),
        Some(Update::AuthorizationState(AuthorizationState::Unknown))
    );
    let others = updates()
        .iter()
        .filter(|u| matches!(u, Update::Other))
        .count();
    assert!(others > 0);
}

/// A synthetic freeze update parses (L8: the appeal link names no Telegram host).
#[test]
fn a_freeze_update_parses() {
    let line = json!({"@type": "updateFreezeState", "is_frozen": true, "freezing_date": 1700000000,
                      "deletion_date": 1702592000, "appeal_link": "https://example.invalid/appeal"});
    match parse_update(&line.to_string()) {
        Some(Update::FreezeState(f)) => {
            assert!(f.is_frozen);
            assert_eq!(f.freezing_date, 1_700_000_000);
            assert_eq!(f.deletion_date, 1_702_592_000);
        }
        _ => panic!("the freeze update did not parse"),
    }
    let lifted = json!({"@type": "updateFreezeState", "is_frozen": false});
    assert_eq!(
        parse_update(&lifted.to_string()),
        Some(Update::FreezeState(FreezeState::default()))
    );
}

/// Every state, the code types the machine maps, the e-mail states and `getMe`'s user parse from
/// synthetic objects; unknown fields are ignored.
#[test]
fn the_login_states_parse_from_synthetic_objects() {
    let s = |v: Value| AuthorizationState::from_value(v);
    assert_eq!(
        s(
            json!({"@type": "authorizationStateWaitPassword", "password_hint": "synthetic hint",
                 "has_recovery_email_address": true, "has_passport_data": false,
                 "recovery_email_address_pattern": "a***@example.invalid", "new_field_2030": 1})
        ),
        AuthorizationState::WaitPassword {
            password_hint: "synthetic hint".to_owned(),
            has_recovery_email_address: true,
            recovery_email_address_pattern: "a***@example.invalid".to_owned(),
        }
    );
    assert_eq!(
        s(
            json!({"@type": "authorizationStateWaitOtherDeviceConfirmation", "link": "tg://login?token=c3ludGhldGlj"})
        ),
        AuthorizationState::WaitOtherDeviceConfirmation {
            link: "tg://login?token=c3ludGhldGlj".to_owned()
        }
    );
    assert_eq!(
        s(
            json!({"@type": "authorizationStateWaitEmailAddress", "allow_apple_id": true, "allow_google_id": false})
        ),
        AuthorizationState::WaitEmailAddress {
            allow_apple_id: true,
            allow_google_id: false
        }
    );
    assert_eq!(
        s(
            json!({"@type": "authorizationStateWaitEmailCode", "allow_apple_id": false, "allow_google_id": false,
                 "code_info": {"@type": "emailAddressAuthenticationCodeInfo", "email_address_pattern": "a***@example.invalid", "length": 6},
                 "email_address_reset_state": {"@type": "emailAddressResetStateAvailable", "wait_period": 60}})
        ),
        AuthorizationState::WaitEmailCode {
            code_info: EmailCodeInfo {
                email_address_pattern: "a***@example.invalid".to_owned(),
                length: 6
            }
        }
    );
    assert_eq!(
        s(json!({"@type": "authorizationStateWaitRegistration",
                 "terms_of_service": {"@type": "termsOfService", "min_user_age": 0, "show_popup": false}})),
        AuthorizationState::WaitRegistration
    );
    assert_eq!(
        s(json!({"@type": "authorizationStateWaitPremiumPurchase", "store_product_id": "x"})),
        AuthorizationState::WaitPremiumPurchase
    );
    assert_eq!(
        s(json!({"@type": "authorizationStateLoggingOut"})),
        AuthorizationState::LoggingOut
    );
    assert_eq!(s(json!({"@type": "ok"})), AuthorizationState::Unknown);
    assert_eq!(s(json!("not an object")), AuthorizationState::Unknown);
    let ty = |v: Value| -> CodeType { serde_json::from_value(v).expect("a code type") };
    assert_eq!(
        ty(json!({"@type": "authenticationCodeTypeSms", "length": 6})),
        CodeType::Sms { length: 6 }
    );
    assert_eq!(
        ty(json!({"@type": "authenticationCodeTypeFlashCall", "pattern": "+999*"})),
        CodeType::FlashCall
    );
    assert_eq!(
        ty(
            json!({"@type": "authenticationCodeTypeFragment", "url": "https://example.invalid/", "length": 5})
        ),
        CodeType::Fragment { length: 5 }
    );
    assert_eq!(
        ty(json!({"@type": "authenticationCodeTypeNew2030"})),
        CodeType::Unknown
    );
    let user = User::from_value(json!({"@type": "user", "id": 100, "first_name": "Ada",
        "last_name": "Test", "phone_number": "999068204719", "usernames": null}))
    .expect("a user");
    assert_eq!(
        (user.id, user.first_name.as_str(), user.last_name.as_str()),
        (100, "Ada", "Test")
    );
    assert!(user.phone_number == "999068204719");
    assert_eq!(User::from_value(json!({"@type": "chat", "id": 100})), None);
}

/// `Debug` of the types that hold a number, a name, a hint, a pattern, a link or a code word
/// shows none of them.
#[test]
fn debug_shows_no_number_name_hint_link_or_pattern() {
    let wait_code = AuthorizationState::WaitCode {
        code_info: AuthenticationCodeInfo {
            phone_number: OWN_NUMBER.to_owned(),
            r#type: CodeType::SmsWord {
                first_letter: "q".to_owned(),
            },
            next_type: Some(CodeType::SmsPhrase {
                first_word: "synthword".to_owned(),
            }),
            timeout: 60,
        },
    };
    let password = AuthorizationState::WaitPassword {
        password_hint: "synthetic hint".to_owned(),
        has_recovery_email_address: true,
        recovery_email_address_pattern: "a***@example.invalid".to_owned(),
    };
    let qr = AuthorizationState::WaitOtherDeviceConfirmation {
        link: "tg://login?token=c3ludGhldGlj".to_owned(),
    };
    let email = AuthorizationState::WaitEmailCode {
        code_info: EmailCodeInfo {
            email_address_pattern: "a***@example.invalid".to_owned(),
            length: 6,
        },
    };
    let user = User {
        id: 100,
        first_name: "Ada".to_owned(),
        last_name: "Test".to_owned(),
        phone_number: "999068204719".to_owned(),
    };
    let freeze = FreezeState {
        is_frozen: true,
        freezing_date: 1,
        deletion_date: 2,
        appeal_link: "https://example.invalid/appeal".to_owned(),
    };
    let option = Update::Option {
        name: "t_me_url".to_owned(),
        value: OptionValue::String {
            value: "https://example.invalid/".to_owned(),
        },
    };
    let my_id = Update::Option {
        name: "my_id".to_owned(),
        value: OptionValue::Integer {
            value: 4_242_424_242,
        },
    };
    let text = format!(
        "{wait_code:?} {password:?} {qr:?} {email:?} {user:?} {freeze:?} {option:?} {my_id:?}"
    );
    for secret in [
        "999068204719",
        "68204719",
        "synthetic hint",
        "example.invalid",
        "token",
        "c3ludGhldGlj",
        "synthword",
        "Ada",
        "4242424242",
    ] {
        assert!(
            !text.contains(secret),
            "Debug shows a secret (index of {})",
            secret.len()
        );
    }
    // The first letter of an SMS word is one character: not findable by substring; the code
    // types print only their name and length.
    assert_eq!(
        format!(
            "{:?}",
            CodeType::SmsWord {
                first_letter: "q".to_owned()
            }
        ),
        "SmsWord(length 0)"
    );
    // What a log may hold: the state's name, the code's type and its timeout.
    assert!(text.contains("WaitCode") && text.contains("SmsWord") && text.contains("60"));
}

/// Every `*.jsonl` of `testdata/` that is TDLib output or synthetic receive lines (the rules file
/// left out), read now: (name, text).
fn receive_files() -> Vec<(String, String)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
    let mut files: Vec<(String, String)> = std::fs::read_dir(&dir)
        .expect("testdata/")
        .filter_map(|e| {
            let p = e.ok()?.path();
            let name = p.file_name()?.to_str()?.to_owned();
            let wanted = name.ends_with(".jsonl")
                && (name.starts_with("rec-")
                    || name.starts_with("receive-")
                    || name == "login-flow.jsonl");
            wanted.then(|| (name, std::fs::read_to_string(&p).expect("read")))
        })
        .collect();
    files.sort();
    files
}

fn line_type(line: &str) -> String {
    serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|v| v["@type"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// TG-3 C1: every update line of the nineteen recordings, the login flow and the synthetic receive
/// lines parses, and every line of a receive type is a `Receive`, never `Other` (a dropped arm
/// fails here); every receive type occurs in at least one file.
#[test]
fn every_recording_and_receive_line_parses() {
    let files = receive_files();
    assert_eq!(
        files.iter().filter(|(n, _)| n.starts_with("rec-")).count(),
        19
    );
    assert!(files.iter().any(|(n, _)| n.starts_with("receive-")));
    let mut seen = std::collections::BTreeSet::new();
    for (name, text) in &files {
        for (n, line) in text
            .lines()
            .enumerate()
            .filter(|(_, l)| !l.trim().is_empty())
        {
            let ty = line_type(line);
            if !ty.starts_with("update") {
                continue;
            }
            let parsed = parse_update(line);
            assert!(parsed.is_some(), "{name}:{}: a {ty} does not parse", n + 1);
            if RECEIVE_TYPES.contains(&ty.as_str()) {
                assert!(
                    matches!(parsed, Some(Update::Receive(_))),
                    "{name}:{}: a {ty} is not a receive update",
                    n + 1
                );
                seen.insert(ty);
            }
        }
    }
    let missing: Vec<&&str> = RECEIVE_TYPES
        .iter()
        .filter(|t| !seen.contains(**t))
        .collect();
    assert!(missing.is_empty(), "never in testdata: {missing:?}");
}

/// The values the recordings and the synthetic files carry that `Debug` must never print: the
/// synthetic names, titles, numbers, usernames and texts.
const NEVER_PRINTED: [&str; 10] = [
    "Ada",
    "Bo Peer",
    "Sample",
    "synthetic",
    "999068204719",
    "999075390162",
    "999081736254",
    "Test",
    "https://",
    "messageText",
];

/// What of [`NEVER_PRINTED`] (and a title `Group <n>` / `Channel <n>`) `shown` holds.
fn printed_words(shown: &str) -> Vec<String> {
    let mut hits: Vec<String> = NEVER_PRINTED
        .iter()
        .filter(|w| shown.contains(**w))
        .map(|w| (*w).to_owned())
        .collect();
    for d in 0..10 {
        for title in [format!("Group {d}"), format!("Channel {d}")] {
            if shown.contains(&title) {
                hits.push(title);
            }
        }
    }
    hits
}

/// TG-3 C1: the `Debug` of every parsed receive update prints no title, text, name, number,
/// username or content (only ids, kinds and counts; a content shows as `Raw(<type>)`, which is
/// the one `@type` it may name).
#[test]
fn debug_prints_no_title_text_name_or_number() {
    let mut printed = 0;
    for (name, text) in receive_files() {
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            if let Some(u @ Update::Receive(_)) = parse_update(line) {
                let shown = format!("{u:?}").replace("Raw(messageText)", "Raw(..)");
                let hits = printed_words(&shown);
                assert!(
                    hits.is_empty(),
                    "{name}: Debug of a {} shows {hits:?}",
                    line_type(line)
                );
                printed += 1;
            }
        }
    }
    assert!(
        printed > 500,
        "the receive updates of testdata were printed"
    );
    // The wrappers themselves.
    let t: Text = serde_json::from_value(json!("Ada Test +999068204719")).expect("text");
    assert_eq!(format!("{t:?}"), "Text(..)");
    assert_eq!(t.as_str(), "Ada Test +999068204719");
    let r: Raw = serde_json::from_value(
        json!({"@type": "messageText", "text": {"@type": "formattedText", "text": "synthetic text 1"}}),
    )
    .expect("raw");
    assert_eq!(format!("{r:?}"), "Raw(messageText)");
}

/// TG-3 C1: td_json's int64 fields (an album id, a chat order, a custom emoji id) take a string
/// (what TDLib writes) and a number.
#[test]
fn int64_fields_take_strings_and_numbers() {
    let message = |album: Value| {
        let line = json!({"@type": "updateNewMessage", "message": {"@type": "message", "id": 1,
            "chat_id": 201, "media_album_id": album,
            "content": {"@type": "messageText"}}});
        match parse_update(&line.to_string()) {
            Some(Update::Receive(Receive::NewMessage(m))) => m.media_album_id,
            other => panic!("not a new message: {other:?}"),
        }
    };
    assert_eq!(message(json!("12345678901234")), 12_345_678_901_234);
    assert_eq!(message(json!(42)), 42);
    let line = json!({"@type": "updateChatPosition", "chat_id": 201, "position": {
        "@type": "chatPosition", "list": {"@type": "chatListArchive"},
        "order": "7301444416084901892", "is_pinned": true}});
    match parse_update(&line.to_string()) {
        Some(Update::Receive(Receive::ChatPosition(p))) => {
            assert_eq!(p.position.order, 7_301_444_416_084_901_892);
            assert_eq!(p.position.list, ChatList::Archive);
            assert!(p.position.is_pinned);
        }
        other => panic!("not a position: {other:?}"),
    }
    let entity: TextEntity = serde_json::from_value(json!({"@type": "textEntity", "offset": 0,
        "length": 2, "type": {"@type": "textEntityTypeCustomEmoji", "custom_emoji_id": "77"}}))
    .expect("entity");
    assert_eq!(
        entity.r#type,
        TextEntityType::CustomEmoji {
            custom_emoji_id: 77
        }
    );
}

/// TG-3 C1: an update outside the receive list (folders, a user's status, counters) is `Other`,
/// never `None`; a receive type that does not parse is `None`.
#[test]
fn an_unknown_update_is_other_not_none() {
    for ty in [
        "updateChatFolders",
        "updateUserStatus",
        "updateUnreadChatCount",
        "updateUserFullInfo",
        "updateHavePendingNotifications",
    ] {
        let line = json!({"@type": ty, "chat_folders": [], "user_id": 201}).to_string();
        assert_eq!(parse_update(&line), Some(Update::Other), "{ty}");
    }
    let broken = json!({"@type": "updateChatReadInbox", "chat_id": "not a number"}).to_string();
    assert_eq!(parse_update(&broken), None);
}

/// TG-3 C1: the synthetic kinds parse into what the machines will read: a bot user, a public, a
/// direct-messages and a broadcast supergroup, a scheduled and a pending message, a view-once
/// reply in a protected chat, a channel post by the chat itself, a from-cache delete.
#[test]
fn the_synthetic_kinds_parse_as_written() {
    let text = receive_files()
        .into_iter()
        .find(|(n, _)| n == "receive-kinds.jsonl")
        .expect("receive-kinds.jsonl")
        .1;
    let all: Vec<Receive> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| match parse_update(l) {
            Some(Update::Receive(r)) => r,
            other => panic!("not a receive update: {other:?}"),
        })
        .collect();
    let users: Vec<&TgUser> = all
        .iter()
        .filter_map(|r| match r {
            Receive::User(u) => Some(&**u),
            _ => None,
        })
        .collect();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0].r#type, UserType::Bot);
    assert_eq!(
        users[0]
            .usernames
            .as_ref()
            .map(|u| u.active_usernames.len()),
        Some(1)
    );
    assert!(users[0].phone_number.is_empty());
    let groups: Vec<&Supergroup> = all
        .iter()
        .filter_map(|r| match r {
            Receive::Supergroup(s) => Some(&**s),
            _ => None,
        })
        .collect();
    let public = |s: &Supergroup| {
        s.usernames
            .as_ref()
            .is_some_and(|u| !u.active_usernames.is_empty())
    };
    assert_eq!(
        groups
            .iter()
            .map(|s| (s.id, public(s), s.is_direct_messages_group, s.is_channel))
            .collect::<Vec<_>>(),
        [
            (404, true, false, false),
            (405, false, true, false),
            (406, false, false, true)
        ]
    );
    let messages: Vec<&Message> = all
        .iter()
        .filter_map(|r| match r {
            Receive::NewMessage(m) => Some(&**m),
            _ => None,
        })
        .collect();
    assert_eq!(messages.len(), 5);
    assert!(matches!(
        messages[0].scheduling_state,
        Some(SchedulingState::SendAtDate {
            send_date: 1_700_000_600
        })
    ));
    assert!(matches!(
        messages[1].sending_state,
        Some(SendingState::Pending { sending_id: 1 })
    ));
    assert_eq!(
        messages[2], messages[3],
        "the re-delivery is the same message"
    );
    assert_eq!(
        messages[2].self_destruct_type,
        Some(SelfDestructType::Immediately)
    );
    assert_eq!(
        messages[2].reply_to,
        Some(ReplyTo::Message {
            chat_id: -301,
            message_id: 2_097_152
        })
    );
    assert!(!messages[2].can_be_saved);
    assert_eq!(messages[2].content.type_name(), "messageText");
    assert_eq!(
        messages[4].sender_id,
        MessageSender::Chat {
            chat_id: -1_000_000_000_406
        }
    );
    assert!(messages[4].is_channel_post);
    let chats: Vec<&Chat> = all
        .iter()
        .filter_map(|r| match r {
            Receive::NewChat(c) => Some(&**c),
            _ => None,
        })
        .collect();
    assert_eq!(
        chats.iter().map(|c| c.r#type).collect::<Vec<_>>(),
        [
            ChatType::Private { user_id: 205 },
            ChatType::BasicGroup {
                basic_group_id: 301
            },
            ChatType::Supergroup {
                supergroup_id: 406,
                is_channel: true
            },
        ]
    );
    assert_eq!(chats[1].notification_settings.mute_for, 3600);
    assert!(chats[1].positions[0].is_pinned);
    assert_eq!(chats[2].positions[0].list, ChatList::Archive);
    assert!(all.iter().any(|r| matches!(r,
        Receive::DeleteMessages(d) if d.from_cache && !d.is_permanent && d.message_ids == [3_145_728])));
    assert!(all.iter().any(|r| matches!(r,
        Receive::ChatAction(a) if a.action == ChatAction::Typing
            && a.sender_id == MessageSender::User { user_id: 202 })));
    assert!(all.iter().any(|r| matches!(r,
        Receive::ScopeNotificationSettings(s) if s.scope == NotificationScope::Group
            && s.notification_settings.mute_for == 600)));
    assert!(
        all.iter()
            .any(|r| matches!(r, Receive::ChatPhoto(p) if p.photo.is_none()))
    );
}
