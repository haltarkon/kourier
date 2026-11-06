//! The TEST phone's checks and acts (`tphone.rs`) against the scripted phone (`tphone_fake.rs`): for
//! each, a yes, a no and the fail-closed screens (unreadable, a missing node, taps that are ignored).
//! No device and no `adb` anywhere; the IDs are synthetic and built from parts.

use std::cell::RefCell;
use std::rc::Rc;

use super::{digest_shape, id_shape};
use crate::test_phone::{CutTag, FakePhone, FakeScreen, NodeSpec, Phone, RootScript, Table};
use crate::threema_run::prompt::digest_of_id;
use crate::threema_run::tphone::{self, ChatKind, Cut, Verdict, host_name};
use crate::threema_run::tphone_fake::{ThreemaFake, shell_handler};

const HOST: &str = "testhost";

fn fake() -> ThreemaFake {
    ThreemaFake {
        own_id: id_shape(),
        ..Default::default()
    }
}

fn kourier_row() -> (String, String) {
    (format!("Kourier ({HOST})"), "Linux".to_owned())
}

fn unlabelled_row() -> (String, String) {
    (
        "Device".to_owned(),
        "Information could not be obtained".to_owned(),
    )
}

// ---- the TEST ID ----------------------------------------------------------------------------------------------

#[test]
fn own_id_digest_is_the_allow_lists_digest_of_the_id_the_phone_shows() {
    let want = digest_of_id(id_shape().as_bytes()).unwrap();
    let mut p = fake().build();
    assert_eq!(tphone::own_id_digest(&mut p).unwrap(), want);
    // case and blanks are normalised like the typed ID
    let mut p = ThreemaFake {
        own_id: format!(" {} ", id_shape().to_lowercase()),
        ..fake()
    }
    .build();
    assert_eq!(tphone::own_id_digest(&mut p).unwrap(), want);
}

#[test]
fn own_id_digest_fails_closed_on_a_short_id_a_missing_node_and_unreadable_screens() {
    let mut p = ThreemaFake {
        own_id: "SHORT".into(),
        ..fake()
    }
    .build();
    assert!(tphone::own_id_digest(&mut p).is_err());
    // no profile tab at all
    let mut p = FakePhone::new("home")
        .screen(
            FakeScreen::new("home").node(NodeSpec::new().id("ch.threema.app:id/bottom_navigation")),
        )
        .on_start(tphone::PKG, "home");
    assert!(tphone::own_id_digest(&mut p).is_err());
    // the screen cannot be dumped
    let mut p = FakePhone::new("home")
        .screen(FakeScreen::unreadable("home"))
        .on_start(tphone::PKG, "home");
    assert!(tphone::own_id_digest(&mut p).is_err());
    // the tab is tapped but ignored every time
    let mut p = fake().build();
    p.ignore_taps("home", 99);
    assert!(tphone::own_id_digest(&mut p).is_err());
}

#[test]
fn an_error_of_a_check_carries_no_id_and_no_digest() {
    let mut p = ThreemaFake {
        own_id: id_shape() + "9",
        ..fake()
    }
    .build();
    let e = format!("{:#}", tphone::own_id_digest(&mut p).unwrap_err());
    assert!(
        !e.contains(&id_shape()) && !e.contains(&digest_shape()),
        "{e}"
    );
}

// ---- the linked devices list ----------------------------------------------------------------------------------

#[test]
fn the_list_shows_kourier_by_label_and_host_only() {
    let mut p = ThreemaFake {
        devices: vec![kourier_row()],
        ..fake()
    }
    .build();
    let (v, list) = tphone::list_shows_kourier(&mut p, HOST);
    assert_eq!(v, Verdict::Yes);
    assert_eq!((list.unwrap().mine, list.unwrap().unlabelled), (1, 0));
    // another host's Kourier row is not this PC's
    let mut p = ThreemaFake {
        devices: vec![("Kourier (elsewhere)".into(), "Linux".into())],
        ..fake()
    }
    .build();
    assert_eq!(tphone::list_shows_kourier(&mut p, HOST).0, Verdict::No);
    // an EMPTY list is never a checked answer (N1): a person is asked
    let mut p = fake().build();
    assert_eq!(tphone::list_shows_kourier(&mut p, HOST).0, Verdict::Ask);
}

#[test]
fn an_unlabelled_row_is_a_no_in_fixed_words() {
    let mut p = ThreemaFake {
        devices: vec![unlabelled_row()],
        ..fake()
    }
    .build();
    let (v, list) = tphone::list_shows_kourier(&mut p, HOST);
    assert_eq!(v, Verdict::No);
    let list = list.unwrap();
    assert_eq!((list.mine, list.unlabelled, list.rows), (0, 1, 1));
}

#[test]
fn the_list_check_is_unknown_when_the_path_cannot_be_walked() {
    // unreadable devices screen
    let mut p = fake().build();
    p = p.screen(FakeScreen::unreadable("devices"));
    assert!(matches!(
        tphone::list_shows_kourier(&mut p, HOST).0,
        Verdict::Unknown(_)
    ));
    // the overflow tap is swallowed
    let mut p = ThreemaFake {
        devices: vec![kourier_row()],
        ..fake()
    }
    .build();
    p.ignore_taps("home", 99);
    assert!(matches!(
        tphone::list_shows_kourier(&mut p, HOST).0,
        Verdict::Unknown(_)
    ));
    // the multi-device row is missing in Settings
    let mut p = ThreemaFake {
        devices: vec![kourier_row()],
        ..fake()
    }
    .build();
    p = p.screen(
        FakeScreen::new("settings").node(
            NodeSpec::new()
                .id("ch.threema.app:id/title_text_view")
                .text("Settings"),
        ),
    );
    assert!(matches!(
        tphone::list_shows_kourier(&mut p, HOST).0,
        Verdict::Unknown(_)
    ));
    // the path ends somewhere that is not the linked-devices screen
    let mut p = ThreemaFake {
        devices: vec![kourier_row()],
        ..fake()
    }
    .build();
    p = p.screen(
        FakeScreen::new("devices")
            .activity("ch.threema.app/.multidevice.wizard.LinkNewDeviceWizardActivity")
            .node(NodeSpec::new().id("ch.threema.app:id/link_device_button")),
    );
    assert!(matches!(
        tphone::list_shows_kourier(&mut p, HOST).0,
        Verdict::Unknown(_)
    ));
}

#[test]
fn the_path_never_taps_the_add_device_button() {
    let mut p = ThreemaFake {
        devices: vec![kourier_row()],
        ..fake()
    }
    .build();
    let _ = tphone::list_shows_kourier(&mut p, HOST);
    assert!(
        !p.acts().iter().any(
            |a| matches!(a, crate::test_phone::Act::Tap(l) if l.contains("link_device_button"))
        ),
        "{:?}",
        p.acts()
    );
}

#[test]
fn the_menu_path_is_yes_when_every_label_was_found_and_unknown_when_one_is_not() {
    let mut p = fake().build();
    assert_eq!(tphone::menu_path_as_runbook(&mut p), Verdict::Yes);
    let mut p = fake().build();
    p = p.screen(FakeScreen::new("overflow").node(NodeSpec::new().text("Help").clickable()));
    assert!(matches!(
        tphone::menu_path_as_runbook(&mut p),
        Verdict::Unknown(_)
    ));
}

#[test]
fn an_empty_list_is_never_a_checked_yes_n1_a_list_with_other_rows_is() {
    // N1: no rows (not loaded, a failed fetch, or truly empty) proves nothing: ASK-USER
    let mut p = fake().build();
    assert_eq!(tphone::list_is_clean(&mut p, HOST), Verdict::Ask);
    let mut p = ThreemaFake {
        list_unloaded: true,
        ..fake()
    }
    .build();
    assert_eq!(tphone::list_is_clean(&mut p, HOST), Verdict::Ask);
    let mut p = ThreemaFake {
        list_loading: true,
        ..fake()
    }
    .build();
    assert_eq!(tphone::list_is_clean(&mut p, HOST), Verdict::Ask);
    // rows, none of them Kourier or unlabelled: a checked yes
    let other = ("Threema Desktop".to_owned(), "Linux".to_owned());
    let mut p = ThreemaFake {
        devices: vec![other],
        ..fake()
    }
    .build();
    assert_eq!(tphone::list_is_clean(&mut p, HOST), Verdict::Yes);
    let mut p = ThreemaFake {
        devices: vec![kourier_row()],
        ..fake()
    }
    .build();
    assert_eq!(tphone::list_is_clean(&mut p, HOST), Verdict::No);
    let mut p = ThreemaFake {
        devices: vec![unlabelled_row()],
        ..fake()
    }
    .build();
    assert_eq!(tphone::list_is_clean(&mut p, HOST), Verdict::No);
    // unreadable: never "clean"
    let mut p = fake().build();
    p = p.screen(FakeScreen::unreadable("devices"));
    assert!(matches!(
        tphone::list_is_clean(&mut p, HOST),
        Verdict::Unknown(_)
    ));
}

#[test]
fn a_stale_kourier_row_next_to_an_unlabelled_current_one_is_no_yes() {
    let mut p = ThreemaFake {
        devices: vec![kourier_row(), unlabelled_row()],
        ..fake()
    }
    .build();
    assert_eq!(tphone::list_shows_kourier(&mut p, HOST).0, Verdict::No);
}

// ---- warnings, push --------------------------------------------------------------------------------------------

#[test]
fn a_warning_on_the_screen_or_in_the_shade_is_found_and_an_unreadable_shade_is_unknown() {
    let mut p = fake().build();
    assert_eq!(tphone::no_warning_on_test_phone(&mut p), Verdict::Yes);
    let mut p = ThreemaFake {
        warning_on_screen: true,
        ..fake()
    }
    .build();
    assert_eq!(tphone::no_warning_on_test_phone(&mut p), Verdict::No);
    let mut p = ThreemaFake {
        warning_in_shade: true,
        ..fake()
    }
    .build();
    assert_eq!(tphone::no_warning_on_test_phone(&mut p), Verdict::No);
    let mut p = ThreemaFake {
        shade_unreadable: true,
        ..fake()
    }
    .build();
    assert!(matches!(
        tphone::no_warning_on_test_phone(&mut p),
        Verdict::Unknown(_)
    ));
    // an unreadable main screen is no "no warning" either
    let mut p = FakePhone::new("home")
        .screen(FakeScreen::unreadable("home"))
        .on_start(tphone::PKG, "home");
    assert!(matches!(
        tphone::no_warning_on_test_phone(&mut p),
        Verdict::Unknown(_)
    ));
}

#[test]
fn the_warning_search_ignores_case_and_knows_only_texts_of_the_app() {
    let mut p = fake().build();
    p = p.with_shade(vec![
        NodeSpec::new().text("REMOVING LINKED DEVICE MAY HAVE FAILED"),
    ]);
    assert_eq!(tphone::no_warning_on_test_phone(&mut p), Verdict::No);
    // the two phrases the review found in no string of the app are not searched
    for w in tphone::WARNING_FRAGMENTS {
        assert_ne!(w, "unofficial");
        assert_ne!(w, "verify your account");
    }
    let mut p = fake().build();
    p = p.with_shade(vec![
        NodeSpec::new().text("an unofficial client; verify your account"),
    ]);
    assert_eq!(tphone::no_warning_on_test_phone(&mut p), Verdict::Yes);
}

#[test]
fn a_forward_security_line_is_no_warning() {
    let mut p = fake().build();
    p = p.with_shade(vec![NodeSpec::new().text(
        "A message without Perfect Forward Secrecy has been received. Verify that the sender has intentionally disabled Perfect Forward Secrecy.",
    )]);
    assert_eq!(tphone::no_warning_on_test_phone(&mut p), Verdict::Yes);
}

#[test]
fn the_push_setting_is_read_from_the_switch_and_a_missing_row_is_unknown() {
    let mut p = fake().build();
    assert_eq!(tphone::push_is_own(&mut p), Verdict::Yes);
    let mut p = ThreemaFake {
        push_on: false,
        ..fake()
    }
    .build();
    assert_eq!(tphone::push_is_own(&mut p), Verdict::No);
    let mut p = fake().build();
    p = p.screen(FakeScreen::new("notifs").node(NodeSpec::new().text("Something else")));
    assert!(matches!(tphone::push_is_own(&mut p), Verdict::Unknown(_)));
    let mut p = fake().build();
    p = p.screen(FakeScreen::unreadable("notifs"));
    assert!(matches!(tphone::push_is_own(&mut p), Verdict::Unknown(_)));
}

// ---- the screen: lock -------------------------------------------------------------------------------------------

fn power(word: &str, uid: u32) -> (FakePhone, Rc<RefCell<String>>) {
    let w = Rc::new(RefCell::new(word.to_owned()));
    let tables = Rc::new(RefCell::new((Vec::new(), Vec::new())));
    let p = fake()
        .build()
        .with_shell(shell_handler(Rc::clone(&w), uid, tables));
    (p, w)
}

#[test]
fn locking_sends_the_sleep_key_and_checks_the_screen_is_off() {
    let (mut p, _) = power("Asleep", 10194);
    assert_eq!(tphone::lock_test_phone(&mut p), Verdict::Yes);
    assert!(
        p.acts()
            .iter()
            .any(|a| matches!(a, crate::test_phone::Act::Key(k) if k == "KEYCODE_SLEEP"))
    );
    // the TEST phone dozes after the sleep key: locked (RT1)
    let (mut p, _) = power("Dozing", 10194);
    assert_eq!(tphone::lock_test_phone(&mut p), Verdict::Yes);
    // the key was sent but the screen is still on: no
    let (mut p, _) = power("Awake", 10194);
    assert_eq!(tphone::lock_test_phone(&mut p), Verdict::No);
    // a screen saver keeps the screen on: not locked (L6)
    let (mut p, _) = power("Dreaming", 10194);
    assert_eq!(tphone::lock_test_phone(&mut p), Verdict::No);
    // no shell, or a power dump without a wakefulness: unknown
    let mut p = fake().build();
    assert!(matches!(
        tphone::lock_test_phone(&mut p),
        Verdict::Unknown(_)
    ));
    let mut p = fake()
        .build()
        .with_shell(|_, _| Ok("nothing useful".into()));
    assert!(matches!(
        tphone::lock_test_phone(&mut p),
        Verdict::Unknown(_)
    ));
}

// ---- chats -------------------------------------------------------------------------------------------------------

const M: &str = "kourier-rt1-sleep-abcdef";
const OLD: &str = "kourier-rt1-sleep-zzzzzz";

#[test]
fn a_text_that_shows_exactly_once_is_a_yes_none_and_two_are_a_no() {
    let mut p = ThreemaFake {
        direct_texts: vec![M.to_owned()],
        ..fake()
    }
    .build();
    assert_eq!(
        tphone::chat_shows_once(&mut p, ChatKind::Direct, M),
        Verdict::Yes
    );
    let mut p = ThreemaFake {
        direct_texts: vec![M.to_owned(), M.to_owned()],
        ..fake()
    }
    .build();
    assert_eq!(
        tphone::chat_shows_once(&mut p, ChatKind::Direct, M),
        Verdict::No
    );
    let mut p = fake().build();
    assert_eq!(
        tphone::chat_shows_once(&mut p, ChatKind::Direct, M),
        Verdict::No
    );
    // the group chat is a chat of its own
    let mut p = ThreemaFake {
        group_texts: vec![M.to_owned()],
        ..fake()
    }
    .build();
    assert_eq!(
        tphone::chat_shows_once(&mut p, ChatKind::Group, M),
        Verdict::Yes
    );
    assert_eq!(
        tphone::chat_shows_once(&mut p, ChatKind::Direct, M),
        Verdict::No
    );
}

#[test]
fn a_marker_counts_case_insensitively_with_whitespace_trimmed_rt1() {
    // Android capitalises the first letter of what he types; the marker passed in may carry spaces
    let typed = format!("K{}", &M[1..]);
    let mut p = ThreemaFake {
        direct_texts: vec![typed],
        ..fake()
    }
    .build();
    assert_eq!(
        tphone::chat_shows_once(&mut p, ChatKind::Direct, &format!("  {M}\n")),
        Verdict::Yes
    );
    // two of them (one capitalised, one not) are two
    let mut p = ThreemaFake {
        direct_texts: vec![format!("K{}", &M[1..]), M.to_owned()],
        ..fake()
    }
    .build();
    assert_eq!(
        tphone::chat_shows_once(&mut p, ChatKind::Direct, M),
        Verdict::No
    );
}

#[test]
fn another_steps_marker_shown_extra_is_a_mixup_not_a_lost_text_rt1() {
    let other = |seen| {
        vec![tphone::OtherMarker {
            step: "mirror".into(),
            marker: OLD.into(),
            seen,
        }]
    };
    let mut p = ThreemaFake {
        direct_texts: vec![OLD.to_owned()],
        ..fake()
    }
    .build();
    // the own marker is missing, the other one shows once and was recorded 0 times: a mix-up
    let got = tphone::chat_shows_once_among(&mut p, ChatKind::Direct, M, &other(0));
    assert_eq!(got.verdict, Verdict::Unknown(tphone::MIXUP));
    assert_eq!(got.mixup, Some((0, 1)));
    // it was recorded once: nothing extra, a lost text as before
    let got = tphone::chat_shows_once_among(&mut p, ChatKind::Direct, M, &other(1));
    assert_eq!((got.verdict, got.mixup), (Verdict::No, None));
    // the own marker is there: the answer is its own, the extra copy of another is no business of it
    let mut p = ThreemaFake {
        direct_texts: vec![OLD.to_owned(), M.to_owned()],
        ..fake()
    }
    .build();
    let got = tphone::chat_shows_once_among(&mut p, ChatKind::Direct, M, &other(0));
    assert_eq!((got.verdict, got.mixup), (Verdict::Yes, None));
}

#[test]
fn a_copy_of_an_older_attempt_never_counts_b2() {
    // the new text is LOST and the old attempt's copy stands in the chat: no
    let mut p = ThreemaFake {
        direct_texts: vec![OLD.to_owned()],
        ..fake()
    }
    .build();
    assert_eq!(
        tphone::chat_shows_once(&mut p, ChatKind::Direct, M),
        Verdict::No
    );
    // the new one arrived next to the old: exactly one of the new word
    let mut p = ThreemaFake {
        direct_texts: vec![OLD.to_owned(), M.to_owned()],
        ..fake()
    }
    .build();
    assert_eq!(
        tphone::chat_shows_once(&mut p, ChatKind::Direct, M),
        Verdict::Yes
    );
}

#[test]
fn the_markers_are_unique_per_attempt_and_made_of_letters() {
    let a = tphone::random_letters();
    let b = tphone::random_letters();
    assert_eq!(a.len(), 6);
    assert!(a.bytes().all(|c| c.is_ascii_lowercase()));
    assert_ne!(a, b, "two attempts, two words");
    assert_eq!(
        tphone::marker_for("sleep", "abcdef"),
        "kourier-rt1-sleep-abcdef"
    );
}

#[test]
fn a_chat_that_cannot_be_opened_is_unknown_never_a_count() {
    let mut p = ThreemaFake {
        chats_unreadable: true,
        ..fake()
    }
    .build();
    assert!(matches!(
        tphone::chat_shows_once(&mut p, ChatKind::Direct, M),
        Verdict::Unknown(_)
    ));
    // two rows that look like the 1:1 chat: the row is not guessed
    let mut p = ThreemaFake {
        two_direct_rows: true,
        ..fake()
    }
    .build();
    assert!(matches!(
        tphone::chat_shows_once(&mut p, ChatKind::Direct, M),
        Verdict::Unknown(_)
    ));
    // the row tap is ignored
    let mut p = fake().build();
    p.ignore_taps("chats", 99);
    assert!(matches!(
        tphone::chat_shows_once(&mut p, ChatKind::Direct, M),
        Verdict::Unknown(_)
    ));
}

fn row_spec(name: &str, group: bool) -> NodeSpec {
    let mut r = NodeSpec::new()
        .clickable()
        .child(
            NodeSpec::new()
                .class("android.widget.ImageView")
                .clickable()
                .desc("avatar"),
        )
        .child(NodeSpec::new().class("android.widget.TextView").text(name));
    if group {
        r = r.child(
            NodeSpec::new()
                .class("android.widget.TextView")
                .desc("Group Chat")
                .text("g"),
        );
    }
    r
}

fn home_to_chats(p: FakePhone, chats: FakeScreen) -> FakePhone {
    p.screen(chats)
}

#[test]
fn the_whole_chat_list_is_scanned_m4_a_second_1to1_row_below_the_fold_fails_closed() {
    // the first screen shows the group and ONE 1:1 row; scrolled down there is a second 1:1 row
    let base = fake().build();
    let first = FakeScreen::new("chats")
        .node(NodeSpec::new().id("ch.threema.app:id/bottom_navigation"))
        .node(row_spec("Alpha", false))
        .node(row_spec("Group", true))
        .on_swipe_up("chats2");
    let second = FakeScreen::new("chats2")
        .node(row_spec("Group", true))
        .node(row_spec("Beta", false));
    let mut p = home_to_chats(base, first).screen(second);
    assert!(tphone::open_chat(&mut p, ChatKind::Direct).is_err());
    // a distribution list or any extra row is never taken for the recipient: nothing was typed
    assert!(p.typed().is_empty());
}

#[test]
fn the_single_1to1_row_below_the_fold_is_found_by_scrolling() {
    let base = fake().build();
    let first = FakeScreen::new("chats")
        .node(row_spec("Group", true))
        .on_swipe_up("chats2");
    let second = FakeScreen::new("chats2")
        .node(row_spec("Group", true))
        .node(row_spec("Alpha", false))
        .on_tap_text("Alpha", "chat-direct");
    let mut p = home_to_chats(base, first).screen(second);
    // the tab tap leads to "chats" in the fake; the list is then scrolled
    assert!(tphone::open_chat(&mut p, ChatKind::Direct).is_ok());
    assert_eq!(p.current(), "chat-direct");
}

#[test]
fn the_mirror_text_is_typed_once_sent_by_one_tap_and_shown_once() {
    let text = "kourier-rt1-mirror-abcdef";
    let mut p = ThreemaFake {
        sent_text: text.into(),
        ..fake()
    }
    .build();
    let send = tphone::mirror_prepare(&mut p, text).unwrap();
    assert_eq!(p.typed(), [text.to_owned()]);
    let taps = p.taps();
    tphone::mirror_tap(&mut p, &send).unwrap();
    assert_eq!(p.taps(), taps + 1, "one tap");
    assert_eq!(
        tphone::chat_shows_once(&mut p, ChatKind::Direct, text),
        Verdict::Yes
    );
}

#[test]
fn the_mirror_prepare_fails_closed_and_types_nothing_unless_all_is_as_expected() {
    let text = "kourier-rt1-mirror-abcdef";
    // the text already stands in the chat
    let mut p = ThreemaFake {
        direct_texts: vec![text.into()],
        ..fake()
    }
    .build();
    assert!(tphone::mirror_prepare(&mut p, text).is_err());
    assert!(p.typed().is_empty());
    // an ambiguous chat list
    let mut p = ThreemaFake {
        two_direct_rows: true,
        ..fake()
    }
    .build();
    assert!(tphone::mirror_prepare(&mut p, text).is_err());
    assert!(p.typed().is_empty());
    // N4: a draft that cannot be wiped: the field is not exactly the text afterwards, so nothing is
    // sent (no tap on the send control)
    let mut p = ThreemaFake {
        draft: true,
        sent_text: text.into(),
        ..fake()
    }
    .build();
    assert!(tphone::mirror_prepare(&mut p, text).is_err());
    assert!(
        !p.acts()
            .iter()
            .any(|a| matches!(a, crate::test_phone::Act::Tap(l) if l.contains("send_button")))
    );
    // a draft the delete key wipes (his own or an earlier attempt's) is gone before the text is typed
    let mut p = ThreemaFake {
        draft_wipable: true,
        sent_text: text.into(),
        ..fake()
    }
    .build();
    assert!(tphone::mirror_prepare(&mut p, text).is_ok());
}

// ---- the firewall cut ------------------------------------------------------------------------------------------

type Tables = Rc<RefCell<(Vec<String>, Vec<String>)>>;

fn firewall_phone(uid: u32) -> (FakePhone, Tables) {
    let tables: Tables = Rc::new(RefCell::new((Vec::new(), Vec::new())));
    let w = Rc::new(RefCell::new("Awake".to_owned()));
    let p = fake()
        .build()
        .with_shell(shell_handler(w, uid, Rc::clone(&tables)));
    (p, tables)
}

fn lens(t: &Tables) -> (usize, usize) {
    (t.borrow().0.len(), t.borrow().1.len())
}

#[test]
fn the_root_scripts_are_the_closed_fixed_lines_h2_with_a_tag_and_the_wait_flag() {
    let tag = CutTag::new("abcdef").unwrap();
    let cut = RootScript::CutOff {
        uid: 10194,
        tag: tag.clone(),
        seconds: 1800,
    }
    .line();
    assert_eq!(
        cut,
        "iptables -w -I OUTPUT -m owner --uid-owner 10194 -m comment --comment kourier-cut-abcdef -j DROP; ip6tables -w -I OUTPUT -m owner --uid-owner 10194 -m comment --comment kourier-cut-abcdef -j DROP; nohup sh -c \"sleep 1800; iptables -w -D OUTPUT -m owner --uid-owner 10194 -m comment --comment kourier-cut-abcdef -j DROP; ip6tables -w -D OUTPUT -m owner --uid-owner 10194 -m comment --comment kourier-cut-abcdef -j DROP\" >/dev/null 2>&1 &"
    );
    assert!(
        cut.chars()
            .all(|c| c.is_ascii_alphanumeric() || " _./-:=;&>\"".contains(c))
    );
    assert_eq!(
        RootScript::ListOutput(Table::V6).line(),
        "ip6tables -w -S OUTPUT"
    );
    assert_eq!(
        RootScript::DeleteDrop {
            table: Table::V4,
            uid: 7,
            tag
        }
        .line(),
        "iptables -w -D OUTPUT -m owner --uid-owner 7 -m comment --comment kourier-cut-abcdef -j DROP"
    );
}

#[test]
fn the_cut_stands_tagged_in_both_tables_and_the_removal_checks_them_gone() {
    let (mut p, tables) = firewall_phone(10194);
    let cut = tphone::cut_threema(&mut p, 600).unwrap();
    assert_eq!(cut.uid, 10194);
    assert_eq!(lens(&tables), (1, 1));
    assert!(tables.borrow().0[0].contains(&format!("kourier-cut-{}", cut.nonce)));
    assert!(tphone::cut_stands(&mut p, &cut).unwrap());
    assert!(tphone::uncut_threema(&mut p, &cut).unwrap());
    assert_eq!(lens(&tables), (0, 0));
    // the cut was gone already (the phone's timer): told, not an error here
    assert!(!tphone::uncut_threema(&mut p, &cut).unwrap());
    assert!(!tphone::cut_stands(&mut p, &cut).unwrap());
}

#[test]
fn a_new_cut_removes_the_rules_of_an_older_one_of_this_tool() {
    let (mut p, tables) = firewall_phone(10194);
    let first = tphone::cut_threema(&mut p, 600).unwrap();
    let second = tphone::cut_threema(&mut p, 600).unwrap();
    assert_ne!(first.nonce, second.nonce);
    assert_eq!(lens(&tables), (1, 1), "the older pair went out first");
    assert!(!tphone::cut_stands(&mut p, &first).unwrap());
    assert!(tphone::cut_stands(&mut p, &second).unwrap());
}

#[test]
fn a_timer_deletes_only_its_own_cut_h2_a_rearmed_cut_survives_the_old_timer() {
    let (mut p, tables) = firewall_phone(10194);
    let old = tphone::cut_threema(&mut p, 600).unwrap();
    let new = tphone::rearm_cut(&mut p, &old, 3600).unwrap();
    assert_ne!(old.nonce, new.nonce);
    assert_eq!(
        lens(&tables),
        (1, 1),
        "the old pair is out, the new pair stands"
    );
    // the OLD timer fires: it deletes its own tag, which is gone; the new cut stands
    for table in [Table::V4, Table::V6] {
        let _ = p.root(&RootScript::DeleteDrop {
            table,
            uid: 10194,
            tag: CutTag::new(&old.nonce).unwrap(),
        });
    }
    assert!(tphone::cut_stands(&mut p, &new).unwrap());
    // a cut the timer ended is seen as ended
    for table in [Table::V4, Table::V6] {
        p.root(&RootScript::DeleteDrop {
            table,
            uid: 10194,
            tag: CutTag::new(&new.nonce).unwrap(),
        })
        .unwrap();
    }
    assert!(!tphone::cut_stands(&mut p, &new).unwrap());
}

#[test]
fn the_cut_and_its_removal_fail_closed() {
    // no uid for Threema
    let mut p = fake()
        .build()
        .with_shell(|_, _| Ok("Package [x]\n userId=3\n".into()));
    assert!(tphone::cut_threema(&mut p, 600).is_err());
    let mut p = fake().build().with_shell(|_, _| Ok(String::new()));
    assert!(tphone::cut_threema(&mut p, 600).is_err());
    // root refuses: no rule is trusted
    let mut p = fake().build().with_shell(|root, l| {
        if root {
            anyhow::bail!("su: permission denied")
        }
        assert!(l.starts_with("dumpsys package"));
        Ok("userId=10194".into())
    });
    assert!(tphone::cut_threema(&mut p, 600).is_err());
    // only the IPv4 table took the rule: not a cut, and what went in is taken back
    let tables: Tables = Rc::new(RefCell::new((Vec::new(), Vec::new())));
    let mut inner = shell_handler(
        Rc::new(RefCell::new("Awake".into())),
        10194,
        Rc::clone(&tables),
    );
    let t2 = Rc::clone(&tables);
    let mut p = fake().build().with_shell(move |root, l| {
        if root && l.contains("nohup") {
            let out = inner(root, l)?;
            t2.borrow_mut().1.clear();
            return Ok(out);
        }
        inner(root, l)
    });
    assert!(tphone::cut_threema(&mut p, 600).is_err());
    assert_eq!(lens(&tables), (0, 0));
    // a rule that stays after its removal is an error
    let cut = Cut {
        uid: 10194,
        nonce: "abcdef".into(),
    };
    let mut p = fake().build().with_shell(|root, l| {
        if root && l.contains("-S") {
            return Ok("-A OUTPUT -m owner --uid-owner 10194 -m comment --comment kourier-cut-abcdef -j DROP".into());
        }
        Ok(String::new())
    });
    assert!(tphone::uncut_threema(&mut p, &cut).is_err());
}

#[test]
fn only_rules_of_threemas_uid_with_drop_and_this_tag_count() {
    let cut = Cut {
        uid: 10194,
        nonce: "abcdef".into(),
    };
    let mut p = fake().build().with_shell(|_, _| {
        Ok("-P OUTPUT ACCEPT\n\
-A OUTPUT -m owner --uid-owner 10195 -m comment --comment kourier-cut-abcdef -j DROP\n\
-A OUTPUT -m owner --uid-owner 101940 -m comment --comment kourier-cut-abcdef -j DROP\n\
-A OUTPUT -m owner --uid-owner 10194 -m comment --comment kourier-cut-abcdef -j ACCEPT\n\
-A OUTPUT -m owner --uid-owner 10194 -m comment --comment kourier-cut-zzzzzz -j DROP\n\
-A OUTPUT -m owner --uid-owner 10194 -j DROP\n"
            .into())
    });
    assert_eq!(
        tphone::cut_rules(&mut p, 10194, &cut.nonce).unwrap(),
        (0, 0)
    );
    // a quoted comment (as iptables-save may print it) is read
    let mut p = fake().build().with_shell(|_, _| {
        Ok("-A OUTPUT -m owner --uid-owner 10194 -m comment --comment \"kourier-cut-abcdef\" -j DROP".into())
    });
    assert_eq!(
        tphone::cut_rules(&mut p, 10194, &cut.nonce).unwrap(),
        (1, 1)
    );
}

#[test]
fn a_rehearsal_phone_is_always_the_scripted_one() {
    // the fake's own ID is read, nothing else
    let mut p = tphone::rehearsal_phone();
    assert!(tphone::own_id_digest(&mut p).is_ok());
    let (v, list) = tphone::list_shows_kourier(&mut p, &host_name().unwrap());
    assert_eq!(v, Verdict::Yes);
    assert_eq!(list.unwrap().mine, 1);
    assert!(p.connect().is_ok());
}

#[test]
fn serials_are_checked_once_and_a_phone_is_never_opened_by_a_test_through_adb() {
    assert!(tphone::check_serial("192.0.2.19:5555").is_ok());
    assert!(tphone::check_serial("emulator-5554").is_ok());
    for bad in ["", "-s", "a b", "a;b", "a'b"] {
        assert!(tphone::check_serial(bad).is_err(), "{bad:?}");
    }
}

// ---- `id --from-test-phone` and the relayed answer -------------------------------------------------------------

mod ops {
    use serde_json::{Value, json};

    use super::super::supervise::{Rig, quick};
    use super::super::{digest_shape, id_shape};
    use crate::threema_run::launch::{QUESTION_KIND, open_question};
    use crate::threema_run::profile::State;
    use crate::threema_run::prompt::{
        digest_of_id, id_from_test_phone, list_text, question_kind, relayed_answer,
    };

    fn with_phone(rig: &Rig) {
        let mut state = State::load(&rig.dir).expect("readable").expect("a state");
        state.set("test_phone", json!("fake:1"));
        state.save(&rig.dir).expect("saved");
    }

    #[test]
    fn id_from_the_test_phone_is_read_by_the_supervisor_and_only_its_digest_stands_b1() {
        let rig = Rig::new("tp-id");
        with_phone(&rig);
        let mut run = rig.run("silent", true, quick());
        run.wait_for("the wait for the ID", |i| i["id"] == json!("waiting"));
        // N3: the steps take the serial from the supervisor's own copy
        assert_eq!(
            run.info().expect("info")["test_phone_serial"],
            json!("fake:1")
        );
        let out = id_from_test_phone(&rig.dir, &rig.socket()).expect("taken");
        let digest = digest_of_id(id_shape().as_bytes()).expect("hashed");
        assert!(out.contains("read from the TEST phone"), "{out}");
        assert!(
            !out.contains(&id_shape()) && !out.contains(&digest),
            "{out}"
        );
        assert_eq!(
            std::fs::read_to_string(rig.dir.allow_file()).expect("the list"),
            list_text(&digest)
        );
        assert_eq!(run.info().expect("info")["id"], json!("taken"));
        // `hand`: the supervisor reads the phone again and compares in its own process.
        let got = run.call(&json!({"op": "hand"})).expect("hand");
        assert_eq!(
            (got["same"].clone(), got["known"].clone()),
            (json!(true), json!(true))
        );
        rig.phone.lock().unwrap().own_id = "SYNTHOTH".into();
        let got = run.call(&json!({"op": "hand"})).expect("hand");
        assert_eq!(got["same"], json!(false));
        assert!(!rig.written_text().contains(&digest));
        run.stop();
        run.join().expect("ended");
    }

    #[test]
    fn allow_phone_takes_no_digest_from_any_client_b1() {
        let rig = Rig::new("tp-id-client");
        with_phone(&rig);
        let mut run = rig.run("silent", true, quick());
        run.wait_for("the wait for the ID", |i| i["id"] == json!("waiting"));
        // a client that chooses the digest (here: another ID's) is refused, and no list is written
        let chosen = digest_of_id(b"SYNTHOTH").expect("hashed");
        let e = run
            .call(&json!({"op": "allow_phone", "digest": chosen}))
            .expect_err("refused");
        assert!(format!("{e:#}").contains("takes no digest"), "{e:#}");
        assert!(!rig.dir.allow_file().exists());
        assert_eq!(run.info().expect("info")["id"], json!("waiting"));
        // the ordinary `allow` stays behind the terminal rule
        let e = run
            .call(&json!({"op": "allow", "digest": digest_shape()}))
            .expect_err("no terminal");
        assert!(format!("{e:#}").contains("terminal"), "{e:#}");
        run.stop();
        run.join().expect("ended");
    }

    #[test]
    fn id_from_the_test_phone_is_refused_in_a_run_without_one_and_reads_nothing() {
        let rig = Rig::new("tp-id-no");
        let mut run = rig.run("silent", true, quick());
        run.wait_for("the wait for the ID", |i| i["id"] == json!("waiting"));
        let e = format!(
            "{:#}",
            id_from_test_phone(&rig.dir, &rig.socket()).expect_err("refused")
        );
        assert!(e.contains("no test phone"), "{e}");
        let e = run
            .call(&json!({"op": "allow_phone"}))
            .expect_err("refused");
        assert!(format!("{e:#}").contains("no test phone"), "{e:#}");
        let e = run.call(&json!({"op": "hand"})).expect_err("refused");
        assert!(format!("{e:#}").contains("no test phone"), "{e:#}");
        assert!(!rig.dir.allow_file().exists());
        assert_eq!(run.info().expect("info")["id"], json!("waiting"));
        run.stop();
        run.join().expect("ended");
    }

    #[test]
    fn id_from_the_test_phone_fails_closed_on_an_unreadable_or_short_id() {
        for bad in ["short", "off"] {
            let rig = Rig::new(&format!("tp-id-{bad}"));
            with_phone(&rig);
            if bad == "short" {
                rig.phone.lock().unwrap().own_id = "ABC".into();
            } else {
                rig.phone.lock().unwrap().chats_unreadable = false;
                rig.phone.lock().unwrap().own_id = String::new();
            }
            let mut run = rig.run("silent", true, quick());
            run.wait_for("the wait for the ID", |i| i["id"] == json!("waiting"));
            let e = format!(
                "{:#}",
                id_from_test_phone(&rig.dir, &rig.socket()).expect_err("refused")
            );
            assert!(e.contains("nothing was taken"), "{e}");
            assert!(!e.contains(&id_shape()));
            assert!(!rig.dir.allow_file().exists(), "{bad}");
            assert_eq!(run.info().expect("info")["id"], json!("waiting"));
            run.stop();
            run.join().expect("ended");
        }
    }

    #[test]
    fn a_relayed_answer_closes_the_failed_links_question_alone_only_with_a_test_phone() {
        // no test phone: refused, the question stays
        let rig = Rig::new("tp-rel-no");
        open_question(&rig.dir, 7).expect("a question");
        let e = format!(
            "{:#}",
            relayed_answer(&rig.dir, None, true, QUESTION_KIND).expect_err("refused")
        );
        assert!(e.contains("no test phone"), "{e}");
        assert!(question_kind(&rig.dir).is_some());
        // with one: closed, the answer recorded
        let rig = Rig::new("tp-rel-yes");
        with_phone(&rig);
        let mut state = State::load(&rig.dir).expect("readable").expect("a state");
        state.set("link_at", json!(5));
        state.save(&rig.dir).expect("saved");
        open_question(&rig.dir, 7).expect("a question");
        // a relay that names another question is refused and the question stays open
        let e = format!(
            "{:#}",
            relayed_answer(&rig.dir, None, false, "pp-delivered").expect_err("refused")
        );
        assert!(e.contains("not the one named"), "{e}");
        assert!(question_kind(&rig.dir).is_some());
        let out = relayed_answer(&rig.dir, None, false, QUESTION_KIND).expect("taken");
        assert!(out.contains("relayed by the lead"), "{out}");
        assert!(question_kind(&rig.dir).is_none());
        let state = rig.state();
        assert_eq!(state["link_at"], Value::Null);
        assert_eq!(state["last_link_question"]["answer"], json!("no"));
    }

    #[test]
    fn a_relayed_answer_through_a_live_supervisor_names_its_origin_and_never_the_emoji() {
        let rig = Rig::new("tp-rel-live");
        with_phone(&rig);
        let mut run = rig.run("normal", false, quick());
        run.ready();
        run.call(&json!({"op": "ask", "name": "pp-read-it"}))
            .expect("ask");
        // RT1: a relay with the WRONG name is refused with the fixed sentence, nothing is taken and
        // the question stays open; with the right name it is taken
        let e = format!(
            "{:#}",
            relayed_answer(&rig.dir, Some(&rig.socket()), true, "pp-delivered")
                .expect_err("refused")
        );
        assert!(e.contains("not the one named"), "{e}");
        assert!(!e.contains("pp-delivered"), "the name is not echoed: {e}");
        assert_eq!(run.info().expect("info")["question"], json!("pp-read-it"));
        let got = run
            .call(&json!({"op": "answer_of", "name": "pp-read-it"}))
            .expect("answer_of");
        assert_eq!(got["answer"], Value::Null, "nothing was taken: {got}");
        let out = relayed_answer(&rig.dir, Some(&rig.socket()), true, "pp-read-it").expect("taken");
        assert!(out.contains("relayed by the lead"), "{out}");
        let got = run
            .call(&json!({"op": "answer_of", "name": "pp-read-it"}))
            .expect("answer_of");
        assert_eq!(
            (got["answer"].clone(), got["origin"].clone()),
            (json!(true), json!("relayed"))
        );
        // nothing asked: refused
        let e = format!(
            "{:#}",
            relayed_answer(&rig.dir, Some(&rig.socket()), true, "pp-read-it").expect_err("refused")
        );
        assert!(e.contains("no question is open"), "{e}");
        // H5/M7: the emoji comparison is typed in his terminal, never relayed
        run.call(&json!({"op": "ask", "name": "emoji-equal"}))
            .expect("ask");
        let e = run
            .call(&json!({"op": "relay_answer", "name": "emoji-equal", "yes": true}))
            .expect_err("refused");
        assert!(format!("{e:#}").contains("never relayed"), "{e:#}");
        assert_eq!(run.info().expect("info")["question"], json!("emoji-equal"));
        run.stop();
        run.join().expect("ended");
    }
}
