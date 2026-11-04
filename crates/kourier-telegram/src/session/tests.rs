//! The session's handlers on synthetic updates and answers (TG-2 C8a; critic H1, M3, M4, M7).
//! Synthetic values only (the number `+999068204719`, the user id 7001, the code `12345`); nothing
//! here prints a typed value: the assertions compare kinds and step ids.

use kourier_model::{ConnState, ErrorKind, LoginStepKind};
use serde_json::{Value, json};

use super::*;
use crate::login::{Flow, Rules};
use crate::supervisor::AuthKind;

const OWN_DIGITS: &str = "999068204719";
const OWN_ID: i64 = 7_001;
const CODE: &str = "12345";

fn frame(v: &Value) -> Zeroizing<String> {
    Zeroizing::new(v.to_string())
}

fn first(frames: &[Value], dropped: usize) -> FirstState {
    FirstState {
        state: AuthKind::Other,
        frames: frames.iter().map(frame).collect(),
        dropped,
    }
}

fn auth(name: &str) -> Value {
    json!({"@type": "updateAuthorizationState", "authorization_state": {"@type": name}})
}

fn wait_code() -> Value {
    json!({
        "@type": "updateAuthorizationState",
        "authorization_state": {
            "@type": "authorizationStateWaitCode",
            "code_info": {
                "@type": "authenticationCodeInfo",
                "phone_number": format!("+{OWN_DIGITS}"),
                "type": {"@type": "authenticationCodeTypeTelegramMessage", "length": 5},
                "next_type": null,
                "timeout": 0,
            }
        }
    })
}

fn conn(name: &str) -> Value {
    json!({"@type": "updateConnectionState", "state": {"@type": name}})
}

fn freeze(is_frozen: bool) -> Value {
    json!({
        "@type": "updateFreezeState",
        "is_frozen": is_frozen,
        "freezing_date": 1_700_000_000,
        "deletion_date": 1_800_000_000,
        "appeal_link": "https://example.invalid/",
    })
}

fn my_id(id: i64) -> Value {
    json!({
        "@type": "updateOption",
        "name": "my_id",
        "value": {"@type": "optionValueInteger", "value": id.to_string()},
    })
}

fn me() -> Value {
    json!({
        "@type": "user",
        "id": OWN_ID,
        "first_name": "Ada",
        "last_name": "Test",
        "phone_number": OWN_DIGITS,
    })
}

fn ok() -> Value {
    json!({"@type": "ok"})
}

fn typed(field: &str, value: &str) -> Vec<(String, Redacted<String>)> {
    vec![(field.to_owned(), Redacted::new(value.to_owned()))]
}

fn inbound(s: &mut Session, v: &Value) -> Vec<Act> {
    s.on_inbound(&v.to_string(), 0)
}

/// Each act as a short label (kinds and step ids only).
fn labels(acts: &[Act]) -> Vec<String> {
    acts.iter().map(label).collect()
}

fn label(act: &Act) -> String {
    match act {
        Act::Step(step) => match &step.kind {
            LoginStepKind::Failed { .. } => format!("failed:{}", step.step_id),
            _ => format!("step:{}", step.step_id),
        },
        Act::Send { asked, .. } => match asked {
            Asked::Login(kind) => format!("send:{}", kind.type_name()),
            Asked::State => "send:getAuthorizationState".to_owned(),
        },
        Act::AppendReady { user_id, .. } => format!("ready:{user_id}"),
        Act::Conn(state) => match state {
            ConnState::Offline { .. } => "conn:offline",
            ConnState::Connecting => "conn:connecting",
            ConnState::Syncing { .. } => "conn:syncing",
            ConnState::Online { .. } => "conn:online",
            ConnState::LoggedOut { .. } => "conn:logged-out",
            _ => "conn:other",
        }
        .to_owned(),
        Act::Hold(None) => "hold:none".to_owned(),
        Act::Hold(Some(hold)) => format!("hold:{:?}", hold.reason),
        Act::Restart => "restart".to_owned(),
        Act::End(Exit::LoggedOut { .. }) => "end:logged-out".to_owned(),
        Act::End(Exit::Failed(_)) => "end:failed".to_owned(),
        Act::End(_) => "end:other".to_owned(),
        Act::Rows(_) => "rows".to_owned(),
        Act::Ephemeral(_) => "ephemeral".to_owned(),
        Act::Fetch { json, .. } => format!("fetch:{}", json["@type"].as_str().unwrap_or("?")),
        Act::Synced => "synced".to_owned(),
    }
}

/// The reason of the one `End(LoggedOut)` among `acts`.
fn logged_out_reason(acts: &[Act]) -> Option<String> {
    acts.iter().find_map(|a| match a {
        Act::End(Exit::LoggedOut { reason }) => Some(reason.clone()),
        _ => None,
    })
}

fn phone_session() -> Session {
    Session::login(Login::new(Flow::Phone, Rules::new(Vec::new(), None, None)))
}

/// A phone login up to the `getMe` in flight (TDLib says Ready before the number's `ok`, so the
/// `getMe` waits for that answer first).
fn at_get_me() -> Session {
    let mut s = phone_session();
    let acts = s.on_first(first(&[auth("authorizationStateWaitPhoneNumber")], 0), 0);
    assert_eq!(labels(&acts), ["step:phone"]);
    let txn = s.txn();
    let acts = s.on_input(txn, typed("phone", &format!("+{OWN_DIGITS}")), 0);
    assert_eq!(labels(&acts), ["send:setAuthenticationPhoneNumber"]);
    let acts = inbound(&mut s, &auth("authorizationStateReady"));
    assert_eq!(
        labels(&acts),
        ["step:linking"],
        "getMe waits for the number's answer"
    );
    let acts = s.on_answer(Asked::Login(ReqKind::SetPhone), Ok(ok()), 0);
    assert_eq!(labels(&acts), ["send:getMe"]);
    s
}

/// [`at_get_me`] up to the `AccountReady` handed out.
fn at_ready() -> Session {
    let mut s = at_get_me();
    let acts = s.on_answer(Asked::Login(ReqKind::GetMe), Ok(me()), 0);
    assert_eq!(labels(&acts), [format!("ready:{OWN_ID}")]);
    s
}

/// A resumed account: `Connecting` at once, nothing before TDLib's Ready, the hold stated once
/// before the first `Syncing`, then the table of §3.3 from live updates (M3: the start's frames are
/// replayed first).
#[test]
fn a_resume_shows_connecting_then_maps_connection_states_after_ready() {
    let mut s = Session::resume(Some(OWN_ID));
    let acts = s.on_first(
        first(
            &[
                my_id(OWN_ID),
                conn("connectionStateConnecting"),
                auth("authorizationStateReady"),
                conn("connectionStateUpdating"),
            ],
            0,
        ),
        0,
    );
    assert_eq!(
        labels(&acts),
        ["conn:connecting", "hold:none", "conn:syncing"]
    );
    assert!(s.authorized());
    // TG-3 C7b (L-6): Ready is Syncing until the start sync's rows are acked (Syncing shows
    // already); then Online.
    let acts = inbound(&mut s, &conn("connectionStateReady"));
    assert_eq!(
        labels(&acts),
        ["synced"],
        "the hold is stated once per connect"
    );
    assert_eq!(labels(&s.on_synced()), ["conn:online"]);
    let acts = inbound(&mut s, &conn("connectionStateReady"));
    assert!(acts.is_empty(), "a repeated state is not shown again");
    let acts = inbound(&mut s, &conn("connectionStateWaitingForNetwork"));
    assert_eq!(labels(&acts), ["conn:offline"]);
}

/// Before TDLib's Ready a connection state is remembered, not shown (§3.3: during a login or a
/// resume's start the auth state speaks).
#[test]
fn a_connection_state_before_ready_waits_for_ready() {
    let mut s = Session::resume(Some(OWN_ID));
    let acts = s.on_first(first(&[conn("connectionStateReady")], 0), 0);
    assert_eq!(labels(&acts), ["conn:connecting"]);
    assert_eq!(s.last_connection(), Some(ConnectionState::Ready));
    let acts = inbound(&mut s, &auth("authorizationStateReady"));
    assert_eq!(labels(&acts), ["hold:none", "conn:syncing", "synced"]);
    assert_eq!(labels(&s.on_synced()), ["conn:online"]);
}

/// M3 + M4: a stored freeze in the start's burst is stated before `Online` (no `None` then); its
/// lift lifts; after a restart the hold is stated again before the next `Online`.
#[test]
fn a_freeze_is_stated_before_online_at_every_connect_and_its_lift_lifts() {
    let mut s = Session::resume(Some(OWN_ID));
    let acts = s.on_first(
        first(
            &[
                freeze(true),
                auth("authorizationStateReady"),
                conn("connectionStateReady"),
            ],
            0,
        ),
        0,
    );
    assert_eq!(
        labels(&acts),
        ["conn:connecting", "hold:Frozen", "conn:syncing", "synced"]
    );
    assert_eq!(labels(&s.on_synced()), ["conn:online"]);
    s.on_restart();
    let acts = s.on_first(
        first(
            &[
                auth("authorizationStateReady"),
                conn("connectionStateReady"),
            ],
            0,
        ),
        0,
    );
    assert_eq!(
        labels(&acts),
        ["conn:connecting", "hold:Frozen", "conn:syncing", "synced"],
        "a freeze seen once is stated again at the next connect"
    );
    assert_eq!(
        labels(&s.on_synced()),
        ["conn:online"],
        "each helper run waits for its own start sync"
    );
    let acts = inbound(&mut s, &freeze(false));
    assert_eq!(labels(&acts), ["hold:none"]);
}

/// L-8 + M7: a resumed account whose TDLib sits in any login state is logged out (cause
/// Unknown) and ends the actor; no step, no `Logout` event; nothing comes after.
#[test]
fn a_resumed_account_tdlib_no_longer_knows_is_logged_out_without_a_logout_event() {
    for state in [
        auth("authorizationStateWaitPhoneNumber"),
        wait_code(),
        auth("authorizationStateWaitPassword"),
        auth("authorizationStateWaitRegistration"),
    ] {
        let mut s = Session::resume(Some(OWN_ID));
        let acts = s.on_first(first(&[state], 0), 0);
        assert_eq!(
            labels(&acts),
            ["conn:connecting", "conn:logged-out", "end:logged-out"]
        );
        assert_eq!(
            logged_out_reason(&acts).as_deref(),
            Some(states::SESSION_GONE)
        );
        assert!(s.is_quiet());
        assert!(inbound(&mut s, &conn("connectionStateReady")).is_empty());
    }
}

/// M7: TDLib's OWN LoggingOut after Ready is a logout; the one Kourier asked for (L5) is not, nor
/// its Closing and Closed.
#[test]
fn tdlibs_own_logout_after_ready_ends_the_session_but_an_asked_one_does_not() {
    let mut s = Session::resume(Some(OWN_ID));
    s.on_first(first(&[auth("authorizationStateReady")], 0), 0);
    let acts = inbound(&mut s, &auth("authorizationStateLoggingOut"));
    assert_eq!(labels(&acts), ["conn:logged-out", "end:logged-out"]);
    assert_eq!(
        logged_out_reason(&acts).as_deref(),
        Some(states::TELEGRAM_ENDED)
    );

    let mut s = Session::resume(Some(OWN_ID));
    s.on_first(first(&[auth("authorizationStateReady")], 0), 0);
    s.logout_asked();
    for name in [
        "authorizationStateLoggingOut",
        "authorizationStateClosing",
        "authorizationStateClosed",
    ] {
        assert!(inbound(&mut s, &auth(name)).is_empty(), "{name}");
    }
    assert!(!s.is_quiet());
}

/// A client that closes without a logout is the helper going away: a restart (TG-1b), no end.
#[test]
fn a_client_closed_without_a_logout_restarts() {
    let mut s = Session::resume(Some(OWN_ID));
    s.on_first(first(&[auth("authorizationStateReady")], 0), 0);
    let acts = inbound(&mut s, &auth("authorizationStateClosed"));
    assert_eq!(labels(&acts), ["restart"]);
    assert!(!s.is_quiet());
}

/// TDLib's `my_id` naming another user than the account's stored one fails the session; the same
/// id, and any id during a login, says nothing.
#[test]
fn another_users_my_id_fails_a_linked_session() {
    let mut s = Session::resume(Some(OWN_ID));
    let acts = s.on_first(first(&[my_id(OWN_ID)], 0), 0);
    assert_eq!(labels(&acts), ["conn:connecting"]);
    let acts = inbound(&mut s, &my_id(OWN_ID + 1));
    assert_eq!(labels(&acts), ["end:failed"]);
    // J3: UNSUPPORTED with a user message (the core halts the account at once, Signal's rule),
    // never `Internal` (the core restarts those until it gives up).
    match acts.as_slice() {
        [Act::End(Exit::Failed(e))] => {
            assert_eq!(e.kind, ErrorKind::Unsupported);
            assert_eq!(e.detail, OTHER_ACCOUNT);
            assert!(e.user_message.is_some(), "no words for the person");
        }
        _ => panic!("not one failed end"),
    }
    assert!(s.is_quiet());

    let mut s = phone_session();
    s.on_first(first(&[auth("authorizationStateWaitPhoneNumber")], 0), 0);
    assert!(inbound(&mut s, &my_id(OWN_ID + 1)).is_empty());
}

/// A phone login from the number to `done`: the requests one at a time (H1), WaitCode before the
/// number's `ok` (the rules file's order), connection states unseen during the login, the
/// `AccountReady` with `[tg, pn]`, `done` only after the core's ack, then the account is linked
/// and its connection shown with the connect's hold first (M4).
#[test]
fn a_phone_login_reaches_done_after_the_ack_and_then_shows_its_connection() {
    let mut s = phone_session();
    let acts = s.on_first(
        first(
            &[
                auth("authorizationStateWaitPhoneNumber"),
                conn("connectionStateConnectingToProxy"),
            ],
            0,
        ),
        0,
    );
    assert_eq!(labels(&acts), ["step:phone"], "no ConnState during a login");
    let txn = s.txn();
    let acts = s.on_input(txn, typed("phone", &format!("+{OWN_DIGITS}")), 0);
    assert_eq!(labels(&acts), ["send:setAuthenticationPhoneNumber"]);
    assert_eq!(s.in_flight(), Some(Asked::Login(ReqKind::SetPhone)));
    let acts = inbound(&mut s, &wait_code());
    assert_eq!(labels(&acts), ["step:code"]);
    let acts = s.on_answer(Asked::Login(ReqKind::SetPhone), Ok(ok()), 0);
    assert!(acts.is_empty());
    assert_eq!(s.in_flight(), None);
    let txn = s.txn();
    let acts = s.on_input(txn, typed("code", CODE), 0);
    assert_eq!(labels(&acts), ["send:checkAuthenticationCode"]);
    let acts = inbound(&mut s, &auth("authorizationStateReady"));
    assert_eq!(labels(&acts), ["step:linking"]);
    let acts = s.on_answer(Asked::Login(ReqKind::CheckCode), Ok(ok()), 0);
    assert_eq!(labels(&acts), ["send:getMe"]);
    let acts = s.on_answer(Asked::Login(ReqKind::GetMe), Ok(me()), 0);
    assert_eq!(labels(&acts), [format!("ready:{OWN_ID}")]);
    let Some(Act::AppendReady { event, .. }) = acts.first() else {
        panic!("no AppendReady");
    };
    let BackendEvent::AccountReady { own_ids, .. } = event.as_ref() else {
        panic!("no AccountReady");
    };
    assert_eq!(
        own_ids.iter().map(|u| u.scheme()).collect::<Vec<_>>(),
        ["tg", "pn"]
    );
    assert!(inbound(&mut s, &conn("connectionStateReady")).is_empty());
    assert!(!s.is_linked(), "not linked before the ack");
    let acts = s.on_acked();
    assert_eq!(
        labels(&acts),
        ["step:done", "hold:none", "conn:syncing", "synced"]
    );
    assert_eq!(labels(&s.on_synced()), ["conn:online"]);
    assert!(s.is_linked());
    assert_eq!(s.user_id(), Some(OWN_ID));
    assert!(s.on_acked().is_empty(), "done comes once");
}

/// A freeze seen during the login is no hold before `AccountReady`; it is stated at the link.
#[test]
fn a_freeze_seen_during_the_login_is_stated_at_the_link() {
    let mut s = at_ready();
    assert!(inbound(&mut s, &freeze(true)).is_empty());
    let acts = s.on_acked();
    assert_eq!(
        labels(&acts),
        ["step:done", "hold:Frozen", "conn:connecting"]
    );
}

/// Q-8: an `AccountReady` the core refused for good (or that could not be written) ends the
/// login `Failed` at `linking`, and the session goes quiet.
#[test]
fn a_refused_account_ready_ends_the_login_at_linking() {
    let mut s = at_ready();
    let acts = s.on_ready_failed();
    assert_eq!(labels(&acts), ["failed:linking"]);
    assert!(s.is_quiet());
    assert!(s.on_acked().is_empty());
}

/// B1: a terminal login outcome makes the session quiet; nothing comes after it.
#[test]
fn a_failed_login_makes_the_session_quiet() {
    let mut s = phone_session();
    let acts = s.on_first(first(&[auth("authorizationStateWaitRegistration")], 0), 0);
    assert_eq!(labels(&acts), ["failed:registration"]);
    assert!(s.is_quiet());
    assert!(inbound(&mut s, &auth("authorizationStateWaitPhoneNumber")).is_empty());
}

/// A helper restart while `getMe` waits: its answer never comes, so TDLib's Ready of the new
/// helper asks again (the linking step again, a new txn).
#[test]
fn a_restart_while_get_me_waits_asks_again() {
    let mut s = at_get_me();
    let txn = s.txn();
    s.on_restart();
    assert_eq!(s.in_flight(), None);
    assert!(!s.authorized());
    let acts = s.on_first(first(&[auth("authorizationStateReady")], 0), 0);
    assert_eq!(labels(&acts), ["step:linking", "send:getMe"]);
    assert!(s.txn() > txn);
}

/// J6: `authorized` is per helper (a restart resets it), `ever_authorized` is sticky: a Telegram
/// session exists once TDLib said Ready, whatever a restart did since, so a logout in the
/// back-off must still say so.
#[test]
fn ever_authorized_survives_a_restart() {
    let mut s = phone_session();
    assert!(!s.ever_authorized());
    s.on_first(first(&[auth("authorizationStateWaitPhoneNumber")], 0), 0);
    assert!(!s.ever_authorized(), "not before Ready");
    let txn = s.txn();
    s.on_input(txn, typed("phone", &format!("+{OWN_DIGITS}")), 0);
    inbound(&mut s, &auth("authorizationStateReady"));
    assert!(s.authorized() && s.ever_authorized());
    s.on_restart();
    assert!(!s.authorized(), "per helper");
    assert!(s.ever_authorized(), "a restart forgot the authorization");
}

/// Fix J5: a client that closes during a login (`Closing`, then `Closed`) is ONE restart, in the
/// start's replay as well as live: a second `Restart` would count a second failure of the back-off.
#[test]
fn a_close_without_a_logout_is_one_restart() {
    let mut s = phone_session();
    let acts = s.on_first(
        first(
            &[
                auth("authorizationStateWaitPhoneNumber"),
                auth("authorizationStateClosing"),
                auth("authorizationStateClosed"),
            ],
            0,
        ),
        0,
    );
    assert_eq!(labels(&acts), ["step:phone", "restart"]);
    assert!(!s.is_quiet(), "the login goes on after the restart");
    s.on_restart();
    let acts = s.on_first(first(&[auth("authorizationStateWaitPhoneNumber")], 0), 0);
    assert_eq!(labels(&acts), ["step:phone"]);
}

/// A helper restart after the `AccountReady` was handed out does not ask `getMe` again (the row is
/// in the outbox); the ack still ends the login `done`.
#[test]
fn a_restart_after_account_ready_does_not_ask_again() {
    let mut s = at_ready();
    s.on_restart();
    let acts = s.on_first(first(&[auth("authorizationStateReady")], 0), 0);
    assert!(acts.is_empty());
    let acts = s.on_acked();
    assert_eq!(labels(&acts), ["step:done", "conn:connecting"]);
}

/// M3: a start that dropped updates is not trusted for its states: the replay skips them and the
/// state is read again.
#[test]
fn a_start_that_dropped_updates_reads_the_state_again() {
    let mut s = phone_session();
    let acts = s.on_first(
        first(
            &[
                auth("authorizationStateWaitPhoneNumber"),
                conn("connectionStateConnecting"),
            ],
            1,
        ),
        0,
    );
    assert_eq!(labels(&acts), ["send:getAuthorizationState"]);
    let acts = s.on_answer(
        Asked::State,
        Ok(json!({"@type": "authorizationStateWaitPhoneNumber"})),
        0,
    );
    assert_eq!(labels(&acts), ["step:phone"]);
}

/// H1: a second request waits for the first one's answer (an error answer is an answer); a lost
/// helper answering the state read is a restart.
#[test]
fn a_second_request_waits_for_the_first_ones_answer() {
    let mut s = Session::resume(Some(OWN_ID));
    let acts = s.on_first(first(&[], 1), 0);
    assert_eq!(
        labels(&acts),
        ["conn:connecting", "send:getAuthorizationState"]
    );
    assert!(s.send(Asked::State, state_request()).is_none());
    let acts = s.on_answer(Asked::State, Err(LinkError::Td { code: 400 }), 0);
    assert_eq!(labels(&acts), ["send:getAuthorizationState"]);
    let acts = s.on_answer(Asked::State, Err(LinkError::Timeout), 0);
    assert_eq!(labels(&acts), ["restart"]);
}

/// Without a login an input is dropped unread, and wiped (J14).
#[test]
fn an_input_without_a_login_is_dropped_and_wiped() {
    let mut s = Session::resume(Some(OWN_ID));
    s.on_first(first(&[auth("authorizationStateReady")], 0), 0);
    let before = crate::login::wiped();
    assert!(s.on_input(1, typed("code", CODE), 0).is_empty());
    assert!(crate::login::wiped() > before, "the value was not wiped");
}

/// `Debug` of an act prints kinds: never a request's JSON (a typed password here).
#[test]
fn debug_of_an_act_prints_no_request_json() {
    let act = Act::Send {
        asked: Asked::Login(ReqKind::CheckPassword),
        json: Zeroizing::new(
            r#"{"@type":"checkAuthenticationPassword","password":"synthetic-password-3141"}"#
                .to_owned(),
        ),
    };
    let text = format!("{act:?}");
    assert!(!text.contains("synthetic-password"));
    assert!(text.contains("CheckPassword"));
}

// ---- TG-3 C7a: the receive, the sync lane and the read act ------------------------------------

const T0: u64 = 1_700_000_000_000;

/// A synthetic `updateNewMessage` of a private chat (the scrub's synthetic values).
fn new_message(chat_id: i64, id: i64) -> Value {
    json!({"@type": "updateNewMessage", "message": {"@type": "message", "id": id,
        "sender_id": {"@type": "messageSenderUser", "user_id": chat_id}, "chat_id": chat_id,
        "is_outgoing": false, "can_be_saved": true, "date": 1_700_000_100, "edit_date": 0,
        "media_album_id": "0", "content": {"@type": "messageText", "text":
        {"@type": "formattedText", "text": "synthetic text 951", "entities": []}}}})
}

/// A private chat as `getChat` answers it.
fn private_chat(id: i64) -> Value {
    json!({"@type": "chat", "id": id, "type": {"@type": "chatTypePrivate", "user_id": id},
        "title": "Bo Peer", "photo": null,
        "positions": [{"@type": "chatPosition", "list": {"@type": "chatListMain"},
            "order": "9000000000000000000", "is_pinned": false}],
        "has_protected_content": false, "is_marked_as_unread": false, "unread_count": 0,
        "last_read_inbox_message_id": 0, "last_read_outbox_message_id": 0,
        "unread_mention_count": 0, "notification_settings": {"@type": "chatNotificationSettings",
        "use_default_mute_for": true, "mute_for": 0}, "message_auto_delete_time": 0})
}

/// A user as `getUser` answers it.
fn peer_user(id: i64) -> Value {
    json!({"@type": "user", "id": id, "first_name": "Bo", "last_name": "Peer",
        "phone_number": "", "is_contact": true, "type": {"@type": "userTypeRegular"}})
}

/// A resumed session past Ready with this run's machines.
fn linked_run() -> Session {
    let mut s = Session::resume(Some(OWN_ID));
    // The link's first flush states the (empty) pinned list.
    s.begin_run(T0, false);
    s.on_first(first(&[auth("authorizationStateReady")], 0), T0);
    s
}

fn fetch_of(acts: &[Act]) -> (LaneAsk, Value) {
    acts.iter()
        .find_map(|a| match a {
            Act::Fetch { ask, json } => Some((ask.clone(), json.clone())),
            _ => None,
        })
        .expect("a sync request")
}

fn persisted_cmds(acts: &[Act]) -> Vec<CmdId> {
    acts.iter()
        .filter_map(|a| match a {
            Act::Rows(staged) => Some(staged.rows.iter().flat_map(|r| r.events.iter())),
            _ => None,
        })
        .flatten()
        .filter_map(|e| match e.event {
            BackendEvent::CommandPersisted { cmd } => Some(cmd),
            _ => None,
        })
        .collect()
}

/// The receive's requests go on the sync lane, never through the login's slot: with the slot
/// busy a `getChat` still goes, and the slot keeps its request (refresh C7a).
#[test]
fn a_sync_request_never_takes_the_login_slot() {
    let mut s = linked_run();
    assert!(
        s.send(Asked::State, state_request()).is_some(),
        "the slot is free"
    );
    let acts = s.on_inbound(&new_message(77, 1 << 20).to_string(), T0);
    assert_eq!(labels(&acts), ["fetch:getChat"]);
    let (ask, json) = fetch_of(&acts);
    assert_eq!(json["chat_id"], 77);
    assert_eq!(
        s.in_flight(),
        Some(Asked::State),
        "the login's slot kept its request"
    );
    assert_eq!(s.lane_in_flight(), Some(&ask));
    // The login's answer does not free the sync lane, nor the other way round.
    let acts = s.on_answer(Asked::State, Ok(auth("authorizationStateReady")), T0);
    assert!(!labels(&acts).iter().any(|l| l.starts_with("fetch:")));
    assert_eq!(s.lane_in_flight(), Some(&ask));
}

/// Q-B1: a private chat whose user TDLib told past the bound (here: never) is decided by ONE
/// `getUser`, on the sync lane after the `getChat` (not paced since TF3); then its messages are
/// rows (here: held for the chat's fresh-link batch, K5: the run has no cursor for it and its
/// start sync has not loaded the lists, so the row waits and a stop now is not clean).
#[test]
fn a_referenced_unknown_user_is_asked_with_get_user() {
    let mut s = linked_run();
    let acts = s.on_inbound(&new_message(78, 1 << 20).to_string(), T0);
    let (ask, _) = fetch_of(&acts);
    let acts = s.on_fetched(ask, Ok(private_chat(78)), T0 + 10);
    assert_eq!(
        labels(&acts),
        ["rows", "fetch:getUser"],
        "the chat's facts; getUser goes at once (TDLib answers it from memory: not paced, TF3)"
    );
    let (ask, json) = fetch_of(&acts);
    assert_eq!(json["user_id"], 78);
    let acts = s.on_fetched(ask, Ok(peer_user(78)), T0 + 15);
    assert_eq!(labels(&acts), ["rows"], "the chat decided");
    // Asked once per run: a second unknown reference asks nothing more.
    assert_eq!(s.lane_due_ms(), None);
    let acts = s.on_inbound(&new_message(78, 2 << 20).to_string(), T0);
    assert!(
        labels(&acts).is_empty(),
        "a chat with no cursor holds its live row until the lists' load: {:?}",
        labels(&acts)
    );
    assert!(s.receive_unsettled(), "a held row left a stop clean");
}

/// H3: a read here is `viewMessages(force_read: true)` of the delivered ids up to the marker on
/// the sync lane; its command is confirmed (`CommandPersisted`, durable) after the answer, and a
/// marker with nothing delivered is confirmed at once.
#[test]
fn a_read_here_is_view_messages_with_force_read() {
    let mut s = linked_run();
    let acts = s.read_marker(-100, incoming(&[2 << 20, 1 << 20]), CmdId(5), T0);
    assert_eq!(labels(&acts), ["fetch:viewMessages"]);
    assert!(
        persisted_cmds(&acts).is_empty(),
        "confirmed before it was sent"
    );
    let (ask, json) = fetch_of(&acts);
    assert_eq!(json["chat_id"], -100);
    assert_eq!(json["message_ids"], json!([1 << 20, 2 << 20]));
    assert_eq!(json["force_read"], true);
    let acts = s.on_fetched(ask, Ok(ok()), T0 + 5);
    assert_eq!(persisted_cmds(&acts), [CmdId(5)]);
    let acts = s.read_marker(-100, Vec::new(), CmdId(6), T0 + 6);
    assert_eq!(persisted_cmds(&acts), [CmdId(6)]);
    assert_eq!(s.lane_in_flight(), None);
}

/// TF11 (review pF2): a supergroup that became PUBLIC after rows were delivered is held now
/// (H5): a read marker of its chat sends no `viewMessages` (the stored ids are old) and is
/// confirmed at once. (Mutant: the hold check removed.)
#[test]
fn a_read_marker_of_a_chat_held_since_its_rows_sends_nothing() {
    let mut s = linked_run();
    let group = json!({"@type": "updateSupergroup", "supergroup": {"@type": "supergroup",
        "id": 1234, "usernames": {"@type": "usernames", "active_usernames": ["synthetic_9"]},
        "is_channel": false}});
    let chat = json!({"@type": "updateNewChat", "chat": {"@type": "chat",
        "id": -1_000_000_001_234_i64,
        "type": {"@type": "chatTypeSupergroup", "supergroup_id": 1234, "is_channel": false},
        "title": "Group 4", "positions": [{"@type": "chatPosition",
            "list": {"@type": "chatListMain"}, "order": "100", "is_pinned": false}]}});
    inbound(&mut s, &group);
    inbound(&mut s, &chat);
    let acts = s.read_marker(
        -1_000_000_001_234,
        incoming(&[1 << 20, 2 << 20]),
        CmdId(5),
        T0,
    );
    assert!(
        !labels(&acts).iter().any(|l| l.starts_with("fetch:")),
        "a request for a held chat"
    );
    assert_eq!(persisted_cmds(&acts), [CmdId(5)], "confirmed at once");
    assert_eq!(s.lane_in_flight(), None);
}

/// D3 of the re-check (TF7): a view page whose answer never came (the link closed, the request
/// timed out, a frame was not understood, the database was locked) is LOST: the read act is not
/// confirmed and the page goes again; only TDLib's own `ok` confirms. (Mutant: `Lost` mapped to
/// `Ok`: a read act confirmed for a page TDLib may never have seen.)
#[test]
fn a_view_page_without_an_answer_is_not_confirmed_and_goes_again() {
    for lost in [
        LinkError::Closed,
        LinkError::Timeout,
        LinkError::Frame,
        LinkError::DatabaseLocked,
    ] {
        let mut s = linked_run();
        let acts = s.read_marker(-100, incoming(&[1 << 20]), CmdId(5), T0);
        let (ask, _) = fetch_of(&acts);
        let acts = s.on_fetched(ask, Err(lost), T0 + 5);
        assert!(
            persisted_cmds(&acts).is_empty(),
            "{lost:?} confirmed the read act"
        );
        assert_eq!(
            labels(&acts),
            ["fetch:viewMessages"],
            "{lost:?}: the page goes again"
        );
        let (ask, _) = fetch_of(&acts);
        let acts = s.on_fetched(ask, Ok(ok()), T0 + 10);
        assert_eq!(persisted_cmds(&acts), [CmdId(5)], "{lost:?}: then ok");
    }
}

/// D2 of the re-check (TF1): `drop_receive` forgets the receive machines whose memory is ahead of
/// a file that refused a commit: they flush nothing more and no longer hold a stop unclean. The
/// control (the same state without the drop) flushes rows and is unsettled, so the test sees the
/// difference. (Mutant: `drop_receive` does nothing.)
#[test]
fn a_dropped_receive_flushes_nothing_and_holds_nothing() {
    let dirtied = || {
        let mut s = linked_run();
        inbound(
            &mut s,
            &json!({"@type": "updateUser", "user": peer_user(78)}),
        );
        inbound(
            &mut s,
            &json!({"@type": "updateNewChat", "chat": private_chat(78)}),
        );
        // A message of a chat TDLib never named: it waits for its `getChat`.
        inbound(&mut s, &new_message(88, 1 << 20));
        s
    };
    let mut control = dirtied();
    assert!(control.receive_unsettled(), "a message waits (control)");
    assert!(
        labels(&control.flush_receive()).contains(&"rows".to_owned()),
        "the machines hold patches (control)"
    );

    let mut s = dirtied();
    s.drop_receive();
    assert!(s.flush_receive().is_empty(), "a dropped machine flushed");
    assert!(!s.receive_unsettled(), "a dropped machine holds the stop");
}

// ---- TG-3 C7b: Online after the start sync -------------------------------------------------

/// L-6: TDLib's Ready is `Syncing` until the start sync's rows are acked (`on_synced`); then
/// `Online`, and at every later Ready of the same run at once (no new start sync). A restart's
/// run waits for its own.
#[test]
fn online_waits_for_the_start_sync() {
    let mut s = linked_run();
    let acts = inbound(&mut s, &conn("connectionStateReady"));
    assert_eq!(
        labels(&acts),
        ["hold:none", "conn:syncing"],
        "the load begins"
    );
    assert!(
        inbound(&mut s, &conn("connectionStateReady")).is_empty(),
        "a second Ready before the ack is still Syncing and begins no second start sync"
    );
    assert!(s.on_synced().is_empty(), "the load is not over");
    assert_eq!(labels(&load_lists(&mut s, T0)), ["synced"]);
    assert_eq!(labels(&s.on_synced()), ["conn:online"]);
    assert!(s.on_synced().is_empty(), "once per run");
    let acts = inbound(&mut s, &conn("connectionStateUpdating"));
    assert_eq!(labels(&acts), ["conn:syncing"]);
    let acts = inbound(&mut s, &conn("connectionStateReady"));
    assert_eq!(
        labels(&acts),
        ["conn:online"],
        "a later Ready is Online at once"
    );
    s.on_restart();
    s.begin_run(T0 + 1, false);
    let acts = s.on_first(
        first(
            &[
                auth("authorizationStateReady"),
                conn("connectionStateReady"),
            ],
            0,
        ),
        T0 + 1,
    );
    assert_eq!(
        labels(&acts),
        ["conn:connecting", "hold:none", "conn:syncing"]
    );
    assert_eq!(labels(&load_lists(&mut s, T0 + 10_000)), ["synced"]);
}

/// The acks of the start sync that come while TDLib is not Ready show nothing; the next Ready
/// is Online at once.
#[test]
fn the_start_sync_acked_while_connecting_shows_online_at_the_next_ready() {
    let mut s = linked_run();
    inbound(&mut s, &conn("connectionStateReady"));
    assert_eq!(labels(&load_lists(&mut s, T0)), ["synced"]);
    let acts = inbound(&mut s, &conn("connectionStateConnecting"));
    assert_eq!(labels(&acts), ["conn:connecting"]);
    assert!(s.on_synced().is_empty());
    let acts = inbound(&mut s, &conn("connectionStateReady"));
    assert_eq!(labels(&acts), ["conn:online"]);
}

/// The start sync's chat load with nothing in either list (both answer 404 at once), on the
/// lane's own clock from `t`: the acts of the last answer.
fn load_lists(s: &mut Session, t: u64) -> Vec<Act> {
    let mut last = Vec::new();
    for list in ["chatListMain", "chatListArchive"] {
        let due = s.lane_due_ms().expect("a list page waits");
        let act = s.pump_lane(due.max(t)).expect("loadChats");
        assert_eq!(label(&act), "fetch:loadChats");
        let (ask, json) = fetch_of(std::slice::from_ref(&act));
        assert_eq!(json["chat_list"]["@type"], list);
        assert_eq!(json["limit"], 100);
        last = s.on_fetched(ask, Err(LinkError::Td { code: 404 }), due.max(t) + 5);
    }
    last
}

// ---- TG-3 C8: the start sync's chat load -----------------------------------------------------

/// One `loadChats` page per second on the sync lane (brief §4.4, his "1 request per second"):
/// the next page waits for the pace on the lane's own clock (no wall time), a 404 moves to the
/// archive, the archive's 404 ends the start sync.
#[test]
fn paced_one_page_per_second() {
    let mut s = linked_run();
    inbound(&mut s, &conn("connectionStateReady"));
    assert_eq!(s.lane_due_ms(), Some(0), "the first page goes at once");
    let act = s.pump_lane(T0).expect("loadChats");
    let (ask, json) = fetch_of(std::slice::from_ref(&act));
    assert_eq!(json["chat_list"]["@type"], "chatListMain");
    let acts = s.on_fetched(ask, Ok(ok()), T0 + 5);
    assert!(acts.is_empty(), "the second page went inside the second");
    let due = s.lane_due_ms().expect("the next page waits");
    assert_eq!(due, T0 + crate::session::lane::FETCH_PACE_MS);
    assert!(s.pump_lane(due - 1).is_none(), "paced");
    let act = s.pump_lane(due).expect("the next page");
    let (ask, json) = fetch_of(std::slice::from_ref(&act));
    assert_eq!(json["chat_list"]["@type"], "chatListMain");
    let acts = s.on_fetched(ask, Err(LinkError::Td { code: 404 }), due + 5);
    assert!(acts.is_empty());
    let due2 = s.lane_due_ms().expect("the archive waits");
    assert_eq!(due2, due + crate::session::lane::FETCH_PACE_MS);
    let act = s.pump_lane(due2).expect("the archive");
    let (ask, json) = fetch_of(std::slice::from_ref(&act));
    assert_eq!(json["chat_list"]["@type"], "chatListArchive");
    let acts = s.on_fetched(ask, Err(LinkError::Td { code: 404 }), due2 + 5);
    assert_eq!(labels(&acts), ["synced"]);
}

// ---- TG-3 C9b: the in-memory decision after Updating -> Ready -------------------------------

/// The gap changes among `acts`.
fn gap_changes(acts: &[Act]) -> Vec<crate::store::GapChange> {
    acts.iter()
        .filter_map(|a| match a {
            Act::Rows(staged) => Some(staged.gaps.iter().copied()),
            _ => None,
        })
        .flatten()
        .collect()
}

/// G-8 (synthetic: no recording shows the transition with a moved `td_last`): TDLib's last
/// message of a chat moved past the cursor without a stream (5 delivered, TDLib says 9). A
/// Ready after `Connecting` decides nothing; `Updating -> Ready` takes the reconcile's decision
/// again in memory (no `loadChats`): the gap (5, 9) with cause `snapshot`, 9 as a catch-up row,
/// and the page from 9 on the paced lane.
#[test]
fn updating_to_ready_runs_the_in_memory_decision() {
    use crate::store::{GapCause, GapChange};
    let mut s = linked_run();
    inbound(&mut s, &conn("connectionStateReady"));
    assert_eq!(labels(&load_lists(&mut s, T0)), ["synced"]);
    assert_eq!(labels(&s.on_synced()), ["conn:online"]);
    let t = T0 + 10_000;
    for v in [
        json!({"@type": "updateUser", "user": peer_user(78)}),
        json!({"@type": "updateNewChat", "chat": private_chat(78)}),
        new_message(78, 5 << 20),
    ] {
        s.on_inbound(&v.to_string(), t);
    }
    let last = new_message(78, 9 << 20)["message"].clone();
    s.on_inbound(
        &json!({"@type": "updateChatLastMessage", "chat_id": 78, "last_message": last,
            "positions": []})
        .to_string(),
        t,
    );
    assert_eq!(s.lane_due_ms(), None, "asked before any transition");
    let acts = s.on_inbound(&conn("connectionStateConnecting").to_string(), t);
    assert!(gap_changes(&acts).is_empty());
    let acts = s.on_inbound(&conn("connectionStateReady").to_string(), t);
    assert!(
        gap_changes(&acts).is_empty(),
        "a Ready after Connecting decided"
    );
    assert_eq!(s.lane_due_ms(), None);

    s.on_inbound(&conn("connectionStateUpdating").to_string(), t);
    let acts = s.on_inbound(&conn("connectionStateReady").to_string(), t + 1);
    assert_eq!(
        gap_changes(&acts),
        [GapChange::Open {
            chat_id: 78,
            above: 5 << 20,
            below: 9 << 20,
            cause: GapCause::Snapshot,
        }]
    );
    let caught: Vec<bool> = acts
        .iter()
        .filter_map(|a| match a {
            Act::Rows(staged) => Some(staged.rows.iter().flat_map(|r| r.events.iter())),
            _ => None,
        })
        .flatten()
        .filter_map(|e| match &e.event {
            BackendEvent::Message(m) => Some(m.catch_up),
            _ => None,
        })
        .collect();
    assert_eq!(caught, [true], "td_last as one catch-up row");
    let (_, json) = fetch_of(&acts);
    assert_eq!(json["@type"], "getChatHistory");
    assert_eq!(json["chat_id"], 78);
    assert_eq!(json["from_message_id"], 9 << 20);
    assert!(
        !labels(&acts).contains(&"fetch:loadChats".to_owned()),
        "a chat load"
    );
}

/// TF16 (review pF7): the `Debug` of what a log line could print by accident (an `Act`, a staged
/// chat fact, a stored chat row, a file pointer, `Me`) names no TDLib remote id and no username: a
/// photo's unique id and a file's remote id are handles that fetch the file with this account.
/// (Mutants: the derived `Debug` back, one type at a time.)
#[test]
fn debug_of_staged_prints_no_remote_id() {
    use crate::content::FilePointer;
    use crate::markup::Me;
    use crate::store::{ChatFacts, ChatKind, ChatRow, Staged};

    const PHOTO: &str = "synthetic-unique-photo-1";
    const REMOTE: &str = "synthetic-remote-file-1";
    const USERNAME: &str = "synthetic_username_1";
    let facts = ChatFacts {
        chat_id: 77,
        kind: Some(ChatKind::Direct),
        photo_small: Some(Some(PHOTO.to_owned())),
        photo_big: Some(Some(format!("{PHOTO}-big"))),
        ..ChatFacts::default()
    };
    let staged = Staged {
        chats: vec![facts.clone()],
        ..Staged::default()
    };
    let row = ChatRow {
        chat_id: 77,
        kind: ChatKind::Direct,
        listed: true,
        held: None,
        delivered_max_id: 0,
        verified_max_id: 0,
        low_id: 0,
        read_inbox_max_id: 0,
        read_outbox_max_id: 0,
        photo_small: Some(PHOTO.to_owned()),
        photo_big: Some(format!("{PHOTO}-big")),
        moved_to: None,
        updated_ms: 0,
    };
    let pointer = FilePointer {
        id: REMOTE.to_owned(),
        unique_id: format!("{REMOTE}-unique"),
        size: 9,
    };
    let usernames = [USERNAME.to_owned()];
    let me = Me {
        id: OWN_ID,
        usernames: &usernames,
    };
    for (what, text) in [
        (
            "an Act",
            format!("{:?}", Act::Rows(Box::new(staged.clone()))),
        ),
        ("a ChatFacts", format!("{facts:?}")),
        ("a Staged", format!("{staged:?}")),
        ("a ChatRow", format!("{row:?}")),
        ("a FilePointer", format!("{pointer:?}")),
        ("a Me", format!("{me:?}")),
    ] {
        for secret in [PHOTO, REMOTE, USERNAME] {
            assert!(
                !text.contains(secret),
                "the Debug of {what} prints a remote id"
            );
        }
    }
}

// ---- TG-4 K4: the read act's confirm stores the position; the read re-assert --------------------

/// A linked run that knows the private chat 78 and its user (listed in the main list).
fn linked_run_with_chat_78() -> Session {
    let mut s = linked_run();
    inbound(
        &mut s,
        &json!({"@type": "updateUser", "user": peer_user(78)}),
    );
    inbound(
        &mut s,
        &json!({"@type": "updateNewChat", "chat": private_chat(78)}),
    );
    s
}

/// The read positions the acts' commits state, per chat, and the commands they confirm.
fn read_facts(acts: &[Act]) -> Vec<(i64, i64)> {
    acts.iter()
        .filter_map(|a| match a {
            Act::Rows(staged) => Some(staged.chats.iter()),
            _ => None,
        })
        .flatten()
        .filter_map(|f| f.read_inbox_max_id.map(|id| (f.chat_id, id)))
        .collect()
}

fn read_inbox_update(chat: i64, id: i64) -> Value {
    json!({"@type": "updateChatReadInbox", "chat_id": chat, "last_read_inbox_message_id": id,
        "unread_count": 0})
}

/// TG-4 K4, critic M3 (the POSTPONED path): TDLib holds its `updateChatReadInbox` while a
/// difference runs (`MessagesManager.cpp:27302`), so the `ok` of the read act can come FIRST. The
/// confirm then stages the act's highest incoming id as the chat's read position in the SAME
/// `Staged` as the command's `CommandPersisted`; the late echo adds no second fact. (Mutant: the
/// fact left out of the confirm -> no position in the commit, the echo stages it alone.)
#[test]
fn a_postponed_echo_leaves_the_confirm_to_state_the_position() {
    let mut s = linked_run_with_chat_78();
    let acts = s.read_marker(78, incoming(&[3 << 20, 2 << 20, 1 << 20]), CmdId(5), T0);
    let (ask, _) = fetch_of(&acts);
    let acts = s.on_fetched(ask, Ok(ok()), T0 + 5);
    assert_eq!(persisted_cmds(&acts), [CmdId(5)]);
    assert_eq!(
        read_facts(&acts),
        [(78, 3 << 20)],
        "the confirm's own commit states the read position"
    );
    let commits: Vec<&Act> = acts.iter().filter(|a| matches!(a, Act::Rows(_))).collect();
    assert_eq!(commits.len(), 1, "the command and its fact are ONE commit");
    // The echo that was postponed comes now: the position is stated already, no second fact.
    let acts = inbound(&mut s, &read_inbox_update(78, 3 << 20));
    assert!(
        read_facts(&acts).is_empty(),
        "the late echo staged the position again"
    );
}

/// TG-4 K4, critic M3 (TDLib's NORMAL order): the echo comes before the `ok`, the receive path
/// stages the position with it, and the confirm stages NO second fact (one fact, no double stage)
/// but still confirms the command. An act that holds only our own messages sends no request (U2)
/// and states no position at all. (Mutant: the confirm stages whatever `up_to` says.)
#[test]
fn the_echo_first_stages_the_position_once_and_an_own_message_stages_none() {
    let mut s = linked_run_with_chat_78();
    let acts = s.read_marker(78, incoming(&[3 << 20, 2 << 20]), CmdId(5), T0);
    let (ask, _) = fetch_of(&acts);
    let echo = inbound(&mut s, &read_inbox_update(78, 3 << 20));
    assert_eq!(read_facts(&echo), [(78, 3 << 20)], "the echo's fact");
    let acts = s.on_fetched(ask, Ok(ok()), T0 + 5);
    assert_eq!(persisted_cmds(&acts), [CmdId(5)]);
    assert!(
        read_facts(&acts).is_empty(),
        "the confirm staged the position a second time"
    );
    // The act holds only our own messages (U2: no incoming id, so no request): confirmed at once,
    // no position.
    let acts = s.read_marker(
        78,
        vec![(5 << 20, true), (4 << 20, true)],
        CmdId(6),
        T0 + 10,
    );
    assert_eq!(persisted_cmds(&acts), [CmdId(6)]);
    assert!(
        !labels(&acts).iter().any(|l| l.starts_with("fetch:")),
        "a request for an act of our own messages"
    );
    assert!(read_facts(&acts).is_empty(), "a position from our own id");
}

/// TG-4 K4 (D-2): the lists' load names the chat whose stored read position is above what TDLib
/// told in this run; the session puts ONE `viewMessages(chat, [stored id], force_read)` on the
/// lane (no command: its answer confirms nothing and a lost one is not retried). Pure over the
/// session; the machine's own cases are in `receive::tests::messages`.
#[test]
fn a_lost_read_goes_on_the_lane_as_one_view_and_confirms_nothing() {
    let mut s = linked_run();
    let mut out = receive::Out::default();
    out.reassert.push((78, 7 << 20));
    let acts = s.receive_outs(out, T0);
    let (ask, json) = fetch_of(&acts);
    assert_eq!(
        ask,
        LaneAsk::Reassert {
            chat_id: 78,
            id: 7 << 20
        }
    );
    assert_eq!(json["@type"], "viewMessages");
    assert_eq!(json["message_ids"], json!([7 << 20]));
    assert_eq!(json["force_read"], true);
    let acts = s.on_fetched(ask, Err(LinkError::Timeout), T0 + 5);
    assert!(persisted_cmds(&acts).is_empty());
    assert!(
        !labels(&acts).iter().any(|l| l.starts_with("fetch:")),
        "a lost re-assert went again"
    );
    assert_eq!(s.lane_in_flight(), None);
}

// ---- TG-4 fix round U1: a lost start-load answer is no answer -----------------------------------

/// A linked run with ONE fresh private chat 78 (3 unread, its last message 5) and a live message 6
/// of it held, both lists answered with 404: the chat's fresh page was asked at the lane clock
/// returned with it. (Synthetic: the scrub's values.)
fn fresh_page_open() -> (Session, LaneAsk, u64) {
    let mut s = linked_run();
    inbound(
        &mut s,
        &json!({"@type": "updateUser", "user": peer_user(78)}),
    );
    let mut chat = private_chat(78);
    chat["unread_count"] = json!(3);
    chat["last_message"] = new_message(78, 5 << 20)["message"].clone();
    inbound(&mut s, &json!({"@type": "updateNewChat", "chat": chat}));
    inbound(&mut s, &conn("connectionStateReady"));
    let acts = load_lists(&mut s, T0);
    assert!(
        !labels(&acts).contains(&"synced".to_owned()),
        "the start sync ended with a page open"
    );
    // A live message of the chat waits for its batch (H1).
    inbound(&mut s, &new_message(78, 6 << 20));
    assert!(s.receive_unsettled(), "the live row was not held");
    let at = s.lane_due_ms().expect("the page waits for its pace");
    let act = s.pump_lane(at).expect("the chat's page");
    let (ask, json) = fetch_of(std::slice::from_ref(&act));
    assert_eq!(json["@type"], "getChatHistory");
    assert_eq!(json["chat_id"], 78);
    (s, ask, at)
}

/// The delivered ids as the actor reads them for a read marker: all incoming.
fn incoming(ids: &[i64]) -> Vec<(i64, bool)> {
    ids.iter().map(|&id| (id, false)).collect()
}

/// The labels that say something was committed or the start sync ended.
fn committed_or_ended(acts: &[Act]) -> Vec<String> {
    labels(acts)
        .into_iter()
        .filter(|l| l == "rows" || l == "synced")
        .collect()
}

/// U1 (durability F1): the page of a fresh chat whose answer was LOST (a closed link racing the
/// restart, a timeout, a frame that is not understood) completes nothing: no row of the chat and
/// no last message reaches a commit, the held live row stays held (the stop stays unclean), the
/// start sync does not end, and the ask is back on the lane. After a closed link the restart then
/// drops it and the next run loads the chat from cursor 0. (Mutant: the old `Done::Fetch` for a
/// lost fetch: the plan completes with the last message alone and the cursor moves.)
#[test]
fn a_lost_fresh_page_answer_commits_nothing_of_the_chat() {
    let losts: [fn() -> LinkError; 3] = [
        || LinkError::Closed,
        || LinkError::Timeout,
        || LinkError::Frame,
    ];
    for lost in losts {
        let (mut s, ask, at) = fresh_page_open();
        let acts = s.on_fetched(ask, Err(lost()), at + 5);
        assert_eq!(
            committed_or_ended(&acts),
            Vec::<String>::new(),
            "a lost page answer completed the plan"
        );
        assert!(
            s.receive_unsettled(),
            "the held live row was released or the plan ended"
        );
        assert!(s.lane_due_ms().is_some(), "the page is not on the lane");
        // The restart that follows a closed link drops the ask and rebuilds the machines.
        s.on_restart();
        assert_eq!(s.lane_due_ms(), None, "the restart kept the lost page");
        assert!(!s.receive_unsettled());
    }
}

/// U1: a page that timed out is asked again in this run, the same request, paced like any fetch
/// (no cap: a network that is down stops the rest of the account too); its answer then completes
/// the plan in ONE commit that holds the last message and the live row that waited.
/// (Mutant: the old `Done::Fetch`: no second ask, the plan ended at the timeout.)
#[test]
fn a_timed_out_fresh_page_is_asked_again() {
    let (mut s, ask, at) = fresh_page_open();
    let acts = s.on_fetched(ask.clone(), Err(LinkError::Timeout), at + 5);
    assert!(committed_or_ended(&acts).is_empty());
    let due = s.lane_due_ms().expect("the same page waits");
    assert_eq!(due, at + crate::session::lane::FETCH_PACE_MS, "paced");
    assert!(s.pump_lane(due - 1).is_none());
    let act = s.pump_lane(due).expect("the page again");
    let (again, _) = fetch_of(std::slice::from_ref(&act));
    assert_eq!(again, ask, "another request");
    // A second timeout asks it a third time (no cap).
    let acts = s.on_fetched(again.clone(), Err(LinkError::Timeout), due + 5);
    assert!(committed_or_ended(&acts).is_empty());
    let due = s.lane_due_ms().expect("the page waits again");
    let act = s.pump_lane(due).expect("a third ask");
    let (third, _) = fetch_of(std::slice::from_ref(&act));
    assert_eq!(third, ask);
    // The answer (an empty page: TDLib holds nothing below the last message) completes the plan.
    let page = json!({"@type": "messages", "total_count": 0, "messages": []});
    let acts = s.on_fetched(third, Ok(page), due + 5);
    assert_eq!(
        committed_or_ended(&acts),
        ["rows", "synced"],
        "the plan did not complete in one commit"
    );
    assert!(!s.receive_unsettled(), "the held row was not released");
}

/// U1 (the `loadChats` twin): a chat-list answer that never came is no end of the list: the same
/// list page is asked again (paced), nothing is committed and the start sync does not end; only
/// TDLib's own 404 ends a list. (Mutant: the old `Done::Fetch`: the lost answer reads as a
/// refusal, the fresh-link load runs on the partial list and the sync ends.)
#[test]
fn a_lost_chat_list_answer_never_ends_the_list() {
    let mut s = linked_run();
    inbound(&mut s, &conn("connectionStateReady"));
    let act = s.pump_lane(T0).expect("the first list page");
    let (mut ask, json) = fetch_of(std::slice::from_ref(&act));
    assert_eq!(json["chat_list"]["@type"], "chatListMain");
    let mut t = T0;
    for lost in [LinkError::Timeout, LinkError::Closed] {
        let acts = s.on_fetched(ask.clone(), Err(lost), t + 5);
        assert!(
            committed_or_ended(&acts).is_empty(),
            "a lost list answer ended the list"
        );
        let due = s.lane_due_ms().expect("the same page waits");
        assert_eq!(due, t + crate::session::lane::FETCH_PACE_MS, "paced");
        t = due;
        let act = s.pump_lane(t).expect("the page again");
        let (again, json) = fetch_of(std::slice::from_ref(&act));
        assert_eq!(again, ask, "another request");
        assert_eq!(
            json["chat_list"]["@type"], "chatListMain",
            "the lost answer moved to the archive"
        );
        ask = again;
    }
    // TDLib's own 404s end the lists, and only they.
    let acts = s.on_fetched(ask, Err(LinkError::Td { code: 404 }), t + 5);
    assert!(committed_or_ended(&acts).is_empty());
    let due = s.lane_due_ms().expect("the archive waits");
    let act = s.pump_lane(due).expect("the archive");
    let (ask, json) = fetch_of(std::slice::from_ref(&act));
    assert_eq!(json["chat_list"]["@type"], "chatListArchive");
    let acts = s.on_fetched(ask, Err(LinkError::Td { code: 404 }), due + 5);
    assert_eq!(labels(&acts), ["synced"]);
}

// ---- TG-4 fix round U2: the read act tells TDLib only incoming ids ------------------------------

/// U2 (honesty HIGH-1), session level: the delivered ids of a marker come with their direction
/// and the request holds only the INCOMING ones (TDLib takes the maximum of the ids it is told
/// with no test of their direction, so an outgoing id would become its read position, its echo
/// and the server's `max_id`); the confirm stores the highest incoming id. An act whose ids are
/// all outgoing sends no request and confirms its command at once, with no position. (Mutant:
/// the outgoing ids back in the request.)
#[test]
fn a_read_act_tells_tdlib_only_the_incoming_ids() {
    let mut s = linked_run_with_chat_78();
    let rows = vec![
        (5 << 20, true),
        (4 << 20, false),
        (3 << 20, false),
        (2 << 20, true),
    ];
    let acts = s.read_marker(78, rows, CmdId(5), T0);
    let (ask, json) = fetch_of(&acts);
    assert_eq!(json["@type"], "viewMessages");
    assert_eq!(
        json["message_ids"],
        json!([3 << 20, 4 << 20]),
        "the request holds an outgoing id or an id above the highest incoming one"
    );
    let acts = s.on_fetched(ask, Ok(ok()), T0 + 5);
    assert_eq!(persisted_cmds(&acts), [CmdId(5)]);
    assert_eq!(read_facts(&acts), [(78, 4 << 20)]);
    // Only our own messages: no request at all, the command confirmed at once.
    let acts = s.read_marker(78, vec![(6 << 20, true)], CmdId(6), T0 + 10);
    assert!(!labels(&acts).iter().any(|l| l.starts_with("fetch:")));
    assert_eq!(persisted_cmds(&acts), [CmdId(6)]);
    assert!(read_facts(&acts).is_empty());
}
