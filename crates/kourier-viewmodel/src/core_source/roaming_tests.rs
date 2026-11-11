//! A10d D1: the chat settings a network keeps for the account's other devices, in the viewmodel
//! (`crate::roaming`): what a chat offers, the commands, the notices that wait for the core's
//! row, the words of a refusal and of a switch that cannot be flipped, against the simulated core
//! of [`super::tests`].

use std::cell::Cell;
use std::rc::Rc;

use kourier_core::facade::{
    AccountRow as CoreAccount, ActionSubject, ConversationDetail, ConversationPage,
    ConversationRow as CoreConversation, CoreError, Query, QueryData, Refusal, Roaming, UiCommand,
    UiEvent,
};
use kourier_model::{
    AccountId, AccountState, BackendError, ConvKind, ConversationChange, ConversationId, ErrorKind,
    MessageId,
};

use super::tests::{
    NOW, Sent, Store, account, conv_row, message, result, take_queries, test_networks,
};
use super::*;
use crate::roaming::ChatTold;
use crate::time::FixedOffset;

const CONV: ConversationId = ConversationId(1);

/// A simulated core whose accounts and conversation rows a test sets, and whose timelines are
/// the plain store's.
struct World {
    accounts: Vec<CoreAccount>,
    rows: Vec<CoreConversation>,
    store: Store,
}

impl World {
    fn new(accounts: Vec<CoreAccount>, rows: Vec<CoreConversation>) -> Self {
        Self {
            accounts,
            rows,
            store: Store::new(&[(1, 5)]),
        }
    }

    fn row_mut(&mut self, id: i64) -> &mut CoreConversation {
        self.rows
            .iter_mut()
            .find(|r| r.id.0 == id)
            .expect("the row")
    }

    fn answer(&self, q: &Query) -> QueryData {
        match q {
            Query::Accounts => QueryData::Accounts(self.accounts.clone()),
            Query::Conversations(_) => QueryData::Conversations(ConversationPage {
                rows: self.rows.clone(),
                next: None,
            }),
            Query::Conversation(id) => {
                QueryData::Conversation(self.rows.iter().find(|r| r.id == *id).map(|row| {
                    Box::new(ConversationDetail {
                        row: row.clone(),
                        members: Vec::new(),
                        features: Default::default(),
                        timer: None,
                        draft: None,
                    })
                }))
            }
            other => self.store.answer(other),
        }
    }
}

/// An account of the test network, with the sync switch and the pin limit it states.
fn acct(id: i64, writes: Option<bool>, limit: Option<u32>) -> CoreAccount {
    CoreAccount {
        id: AccountId(id),
        roaming_writes: writes,
        pin_limit: limit,
        ..account()
    }
}

/// A 1:1 chat of `account` whose network lists it, answering for the account's switch as `On`.
fn chat(id: i64, account: i64) -> CoreConversation {
    CoreConversation {
        account: AccountId(account),
        kind: ConvKind::Direct,
        roaming: Roaming::On,
        ..conv_row(id, "Chat", 1000 - id)
    }
}

fn pinned(mut c: CoreConversation, place: u32) -> CoreConversation {
    c.pinned_order = Some(place);
    c
}

/// A source over `world` with a clock the test moves, everything read.
fn loaded(world: &World) -> (CoreSource, Sent, Rc<Cell<i64>>) {
    let clock = Rc::new(Cell::new(NOW));
    let sent: Sent = Rc::default();
    let (s2, c2) = (sent.clone(), clock.clone());
    let mut src = CoreSource::new(
        Box::new(move |c: UiCommand| {
            s2.borrow_mut().push(c);
            true
        }),
        CoreSourceConfig {
            now: Box::new(move || c2.get()),
            calendar: Box::new(FixedOffset(0)),
            networks: test_networks(),
            hostname: "workstation".into(),
            demo: None,
        },
    );
    src.reload();
    settle(&mut src, &sent, world);
    (src, sent, clock)
}

/// Answers every query (and the follow-ups they cause) from `world`.
fn settle(src: &mut CoreSource, sent: &Sent, world: &World) -> Changes {
    let mut ch = Changes::default();
    for _ in 0..100 {
        let queries = take_queries(sent);
        if queries.is_empty() {
            return ch;
        }
        for (token, query) in queries {
            ch.merge(src.on_event(&result(token, world.answer(&query))));
        }
    }
    panic!("queries do not settle");
}

/// The core's list changed: tell the source and answer the read it causes.
fn relist(src: &mut CoreSource, sent: &Sent, world: &World) -> Changes {
    let mut ch = src.on_event(&UiEvent::ConversationsChanged { ids: Vec::new() });
    ch.merge(settle(src, sent, world));
    ch
}

/// The accounts changed: tell the source and answer the reads.
fn reaccount(src: &mut CoreSource, sent: &Sent, world: &World) -> Changes {
    let mut ch = src.on_event(&UiEvent::AccountsChanged);
    ch.merge(settle(src, sent, world));
    ch
}

/// Every command sent so far.
fn commands(sent: &Sent) -> Vec<UiCommand> {
    std::mem::take(&mut *sent.borrow_mut())
}

fn marks(cmds: &[UiCommand]) -> usize {
    cmds.iter()
        .filter(|c| matches!(c, UiCommand::MarkRead { .. }))
        .count()
}

fn refused(reason: Refusal) -> CoreError {
    CoreError::Refused {
        reason,
        detail: "the core's own words",
    }
}

fn backend(kind: ErrorKind) -> CoreError {
    CoreError::Backend(Box::new(BackendError::new(kind, "the network's detail")))
}

fn failed_change(src: &mut CoreSource, error: &CoreError) -> Changes {
    src.on_action_failed(
        "ChangeConversation",
        error,
        &ActionSubject::Conversation(CONV),
    )
}

/// A refused change is a toast (`ChatTold::Refused` for the chat), never a composer notice: the
/// words of the one told since the last call.
fn refusal_words(src: &mut CoreSource, error: &CoreError) -> String {
    let ch = failed_change(src, error);
    assert!(
        ch.told && !ch.notice,
        "{error:?}: a toast, no composer notice"
    );
    assert_eq!(src.notice(), "", "nothing in the composer");
    let told = src.take_told();
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0].kind(), "refused");
    assert_eq!(told[0].conv(), Some(CONV));
    told[0].words()
}

/// KI18n fills a plural's `%1` with the count before this crate fills the rest.
struct LikeKi18n;
impl crate::i18n::Translator for LikeKi18n {
    fn translate(&self, _context: &str, msgid: &str) -> String {
        msgid.to_owned()
    }
    fn translate_plural(&self, _c: &str, one: &str, many: &str, n: u64) -> String {
        let form = if n == 1 { one } else { many };
        form.replace("%1", &n.to_string())
    }
}

const LINKED_ONLY: &str = "Available while this account is linked.";

/// A chat of an account that is not linked cannot be changed whatever its switch says, and the
/// line says why (the greyed line would point at a switch that is disabled). Both places that
/// show the offer say the same: the list's `chat_offer` and the open chat's header.
#[test]
fn a_chat_of_an_unlinked_account_is_greyed() {
    let mut world = World::new(
        vec![acct(1, Some(true), None)],
        vec![chat(1, 1), chat(2, 1)],
    );
    let (mut src, sent, _) = loaded(&world);
    src.open_conversation(Some(CONV));
    settle(&mut src, &sent, &world);
    let linked = src.chat_offer(CONV);
    assert!(linked.shown && linked.on, "a linked account's chat works");
    assert!(linked.why.is_empty());
    assert_eq!(src.chat(CONV).expect("header").offer, linked);

    // Logged out: the account's row still states the flag.
    world.accounts[0].state = AccountState::LoggedOut {
        reason: String::new(),
    };
    reaccount(&mut src, &sent, &world);
    for offer in [
        src.chat_offer(CONV),
        src.chat_offer(ConversationId(2)),
        src.chat(CONV).expect("header").offer,
    ] {
        assert!(offer.shown, "the entries stay, greyed");
        assert!(!offer.on, "an unlinked account's chat is greyed");
        assert_eq!(offer.why, LINKED_ONLY);
    }

    // The switch off AND not linked: the reason is the account's, not a line that points at a
    // switch that cannot be used.
    world.rows[0].roaming = Roaming::Off;
    relist(&mut src, &sent, &world);
    let off = src.chat_offer(CONV);
    assert!(off.shown && !off.on);
    assert_eq!(off.why, LINKED_ONLY);

    // Linked again, the switch off: the line that says how to turn it on.
    world.accounts[0].state = AccountState::Active;
    reaccount(&mut src, &sent, &world);
    let off = src.chat_offer(CONV);
    assert!(off.shown && !off.on);
    assert!(off.why.starts_with("To change this on your phone too"));

    // A network that states nothing offers nothing, linked or not.
    world.rows[0].roaming = Roaming::Hidden;
    relist(&mut src, &sent, &world);
    let hidden = src.chat_offer(CONV);
    assert!(!hidden.shown && !hidden.on && hidden.why.is_empty());
}

/// The offer is read from the list, else from the open chat's detail (a chat the list does not
/// show), else nothing.
#[test]
fn the_offer_is_read_from_the_list_else_the_open_chat() {
    let mut world = World::new(vec![acct(1, Some(true), None)], vec![chat(1, 1)]);
    let (mut src, sent, _) = loaded(&world);
    assert!(src.chat_offer(CONV).on);
    assert_eq!(
        src.chat_offer(ConversationId(9)),
        crate::roaming::ChatOffer::default(),
        "a chat nobody knows offers nothing"
    );
    // A chat only the detail knows: the list was read without it and is not read again.
    let mut other = chat(5, 1);
    other.archived = true;
    world.rows.push(other);
    src.open_conversation(Some(ConversationId(5)));
    for (token, query) in take_queries(&sent) {
        if matches!(query, Query::Conversations(_)) {
            continue;
        }
        src.on_event(&result(token, world.answer(&query)));
    }
    let offer = src.chat_offer(ConversationId(5));
    assert!(offer.shown && offer.on && offer.archived, "from the detail");
    assert_eq!(src.chat(ConversationId(5)).expect("header").offer, offer);
}

/// Pin is greyed at a STATED limit of the chat's own account, counted over the pinned chats of
/// that account the list holds. The chat that is pinned keeps Unpin; another account's pins and a
/// network that states no limit never grey it.
#[test]
fn pin_at_the_stated_limit_is_greyed() {
    // Account 1 states 3 and pins 3; account 2 states 3 and pins 2; account 3 states no limit
    // and pins 4.
    let mut rows = vec![
        pinned(chat(1, 1), 0),
        pinned(chat(2, 1), 1),
        pinned(chat(3, 1), 2),
        chat(4, 1),
        pinned(chat(5, 2), 0),
        pinned(chat(6, 2), 1),
        chat(7, 2),
    ];
    for (i, id) in (10..14).enumerate() {
        rows.push(pinned(chat(id, 3), u32::try_from(i).expect("small")));
    }
    rows.push(chat(14, 3));
    let world = World::new(
        vec![
            acct(1, Some(true), Some(3)),
            acct(2, Some(true), Some(3)),
            acct(3, Some(true), None),
        ],
        rows,
    );
    let (src, _sent, _) = loaded(&world);
    crate::i18n::with_translator(LikeKi18n, || {
        let full = src.chat_offer(ConversationId(4));
        assert!(full.on, "everything else works");
        assert!(full.pin_at_limit, "3 of 3 are pinned");
        assert_eq!(full.pin_why, "You can pin up to 3 chats.");
        // The chat that is pinned keeps Unpin.
        let itself = src.chat_offer(CONV);
        assert!(itself.pinned && !itself.pin_at_limit && itself.pin_why.is_empty());
        // Account 2 pins 2 of 3: room for one more, though account 1 is full.
        let room = src.chat_offer(ConversationId(7));
        assert!(!room.pin_at_limit && room.pin_why.is_empty());
        // No stated limit: never greyed, however many are pinned.
        let free = src.chat_offer(ConversationId(14));
        assert!(!free.pin_at_limit && free.pin_why.is_empty());
    });
    // The limit of the account, not a constant: one account that states 4 and has 4 pinned.
    let mut rows: Vec<_> = (0..4)
        .map(|i| pinned(chat(20 + i64::from(i), 1), i))
        .collect();
    rows.push(chat(24, 1));
    let world = World::new(vec![acct(1, Some(true), Some(4))], rows);
    let (src, _sent, _) = loaded(&world);
    let full = src.chat_offer(ConversationId(24));
    assert!(full.pin_at_limit);
    assert_eq!(full.pin_why, "You can pin up to 4 chats.");
    // Three of four are pinned: room for one more.
    let world = World::new(
        vec![acct(1, Some(true), Some(4))],
        vec![
            pinned(chat(1, 1), 0),
            pinned(chat(2, 1), 1),
            pinned(chat(3, 1), 2),
            chat(4, 1),
        ],
    );
    let (src, _sent, _) = loaded(&world);
    assert!(!src.chat_offer(ConversationId(4)).pin_at_limit);
    // A switch that is off greys everything with its own line, not the pin's.
    let mut rows: Vec<_> = (0..4)
        .map(|i| pinned(chat(30 + i64::from(i), 1), i))
        .collect();
    let mut off = chat(34, 1);
    off.roaming = Roaming::Off;
    rows.push(off);
    let world = World::new(vec![acct(1, Some(false), Some(4))], rows);
    let (src, _sent, _) = loaded(&world);
    let off = src.chat_offer(ConversationId(34));
    assert!(off.shown && !off.on && !off.pin_at_limit && off.pin_why.is_empty());
    assert!(off.why.starts_with("To change this on your phone too"));
}

/// The core's `TooMany` words the account's limit as KI18n fills it (the count is `%1`), with the
/// limit the account states, whatever it is.
#[test]
fn a_refused_pin_names_the_accounts_limit() {
    let world = World::new(vec![acct(1, Some(true), Some(3))], vec![chat(1, 1)]);
    let (mut src, _sent, _) = loaded(&world);
    crate::i18n::with_translator(LikeKi18n, || {
        assert_eq!(
            refusal_words(&mut src, &refused(Refusal::TooMany)),
            "You can pin up to 3 chats."
        );
    });
    // One chat is the singular.
    let world = World::new(vec![acct(1, Some(true), Some(1))], vec![chat(1, 1)]);
    let (mut one, _sent, _) = loaded(&world);
    crate::i18n::with_translator(LikeKi18n, || {
        assert_eq!(
            refusal_words(&mut one, &refused(Refusal::TooMany)),
            "You can pin up to 1 chat."
        );
    });
    // An account that states no limit (a refusal it cannot word): the generic line.
    let world = World::new(vec![acct(1, Some(true), None)], vec![chat(1, 1)]);
    let (mut none, _sent, _) = loaded(&world);
    assert_eq!(
        refusal_words(&mut none, &refused(Refusal::TooMany)),
        "The change could not be made."
    );
}

/// Any other refusal or failure of a change says so.
#[test]
fn a_refused_change_says_so() {
    let world = World::new(vec![acct(1, Some(true), Some(4))], vec![chat(1, 1)]);
    let (mut src, _sent, _) = loaded(&world);
    for error in [
        refused(Refusal::NotAllowedHere),
        refused(Refusal::Gone),
        CoreError::Stopped,
        backend(ErrorKind::Transient { retry_after: None }),
    ] {
        assert_eq!(
            refusal_words(&mut src, &error),
            "The change could not be made.",
            "{error:?}"
        );
    }
}

/// The network's own count of the pins (it lists chats this device does not) refuses with
/// `TooLarge`; that, and only that backend kind, is the pin limit's sentence.
#[test]
fn a_pin_the_backend_refuses_names_the_limit() {
    let world = World::new(vec![acct(1, Some(true), Some(4))], vec![chat(1, 1)]);
    let (mut src, _sent, _) = loaded(&world);
    assert_eq!(
        refusal_words(&mut src, &backend(ErrorKind::TooLarge)),
        "You can pin up to 4 chats."
    );
    // Another kind of failure is not a count.
    assert_eq!(
        refusal_words(&mut src, &backend(ErrorKind::InvalidInput)),
        "The change could not be made."
    );
    assert_eq!(
        refusal_words(&mut src, &backend(ErrorKind::Unauthorized)),
        "The change could not be made."
    );
}

/// "Chat archived" is told once the core's row says the chat is archived, never at the click: a
/// refusal tells nothing and a narrow window may show nothing of the change. The same for the
/// other two notices.
#[test]
fn the_archive_notice_waits_for_the_row() {
    let mut world = World::new(
        vec![acct(1, Some(true), Some(4))],
        vec![pinned(chat(1, 1), 0), chat(2, 1), chat(3, 1)],
    );
    let (mut src, sent, _) = loaded(&world);
    // Archive the pinned chat: nothing is told at the click.
    assert!(src.set_archived(CONV, true));
    assert!(src.take_told().is_empty(), "not at the click");
    let cmds = commands(&sent);
    assert!(matches!(
        cmds.as_slice(),
        [UiCommand::ChangeConversation {
            conv,
            change: ConversationChange::SetArchived { archived: true }
        }] if *conv == CONV
    ));
    // The list is read again before the core applied it: still nothing.
    let ch = relist(&mut src, &sent, &world);
    assert!(!ch.told);
    assert!(src.take_told().is_empty());
    // The core applied it (archiving a pinned chat unpins it).
    let row = world.row_mut(1);
    row.archived = true;
    row.pinned_order = None;
    let ch = relist(&mut src, &sent, &world);
    assert!(ch.told, "the row says archived now");
    assert!(ch.any());
    let told = src.take_told();
    assert_eq!(
        told,
        [ChatTold::Archived {
            conv: CONV,
            was_pinned: true
        }]
    );
    assert_eq!(told[0].words(), "Chat archived");
    // Told once.
    relist(&mut src, &sent, &world);
    assert!(src.take_told().is_empty());

    // Unarchive: "Chat returned to inbox" once the row is back in the inbox.
    src.set_archived(CONV, false);
    relist(&mut src, &sent, &world);
    assert!(src.take_told().is_empty());
    world.row_mut(1).archived = false;
    assert!(relist(&mut src, &sent, &world).told);
    assert_eq!(src.take_told(), [ChatTold::Unarchived { conv: CONV }]);

    // Mark as Unread: told once the row carries the mark; removing the mark tells nothing.
    src.set_marked_unread(ConversationId(2), true);
    relist(&mut src, &sent, &world);
    assert!(src.take_told().is_empty());
    world.row_mut(2).marked_unread = true;
    assert!(relist(&mut src, &sent, &world).told);
    assert_eq!(
        src.take_told(),
        [ChatTold::MarkedUnread {
            conv: ConversationId(2)
        }]
    );
    src.set_marked_unread(ConversationId(2), false);
    world.row_mut(2).marked_unread = false;
    assert!(!relist(&mut src, &sent, &world).told);
    assert!(src.take_told().is_empty());
}

/// An entry that the row never confirms is forgotten after 30 s by the source's clock: a change
/// made by someone else minutes later is not this click's.
#[test]
fn a_click_the_row_never_confirms_is_forgotten_after_thirty_seconds() {
    let mut world = World::new(
        vec![acct(1, Some(true), None)],
        vec![chat(1, 1), chat(2, 1)],
    );
    let (mut src, sent, clock) = loaded(&world);
    src.set_archived(CONV, true);
    src.set_archived(ConversationId(2), true);
    clock.set(NOW + 29_000);
    world.row_mut(1).archived = true;
    assert!(
        relist(&mut src, &sent, &world).told,
        "29 s: still this click's"
    );
    assert_eq!(src.take_told().len(), 1);
    clock.set(NOW + 31_000);
    world.row_mut(2).archived = true;
    assert!(!relist(&mut src, &sent, &world).told, "31 s: forgotten");
    assert!(src.take_told().is_empty());
}

/// A refused click tells nothing: the entry goes with the refusal, so the phone archiving the
/// chat a minute later does not read as this click's notice.
#[test]
fn a_refused_archive_tells_nothing() {
    let mut world = World::new(vec![acct(1, Some(true), None)], vec![chat(1, 1)]);
    let (mut src, sent, _) = loaded(&world);
    src.set_archived(CONV, true);
    // The refusal is told (a toast of its own), the archive's notice never comes.
    assert_eq!(
        refusal_words(&mut src, &refused(Refusal::NotAllowedHere)),
        "The change could not be made."
    );
    // The chat is archived some other way.
    world.row_mut(1).archived = true;
    assert!(!relist(&mut src, &sent, &world).told);
    assert!(src.take_told().is_empty(), "no notice for a refused click");
}

/// Undo takes the archive back and pins the chat again when the archive took the pin; it leaves
/// no notice of its own (Desktop's Undo shows no second toast).
#[test]
fn undo_brings_back_the_pin_without_a_notice() {
    let mut world = World::new(vec![acct(1, Some(true), Some(4))], vec![chat(1, 1)]);
    world.row_mut(1).archived = true;
    let (mut src, sent, _) = loaded(&world);
    commands(&sent);
    assert!(src.undo_archive(CONV, true));
    let cmds = commands(&sent);
    assert!(
        matches!(
            cmds.as_slice(),
            [
                UiCommand::ChangeConversation {
                    conv: a,
                    change: ConversationChange::SetArchived { archived: false }
                },
                UiCommand::ChangeConversation {
                    conv: b,
                    change: ConversationChange::SetPinned { pinned: true }
                }
            ] if *a == CONV && *b == CONV
        ),
        "{cmds:?}"
    );
    // A chat that was not pinned is only unarchived.
    assert!(src.undo_archive(CONV, false));
    let cmds = commands(&sent);
    assert!(
        matches!(
            cmds.as_slice(),
            [UiCommand::ChangeConversation {
                change: ConversationChange::SetArchived { archived: false },
                ..
            }]
        ),
        "{cmds:?}"
    );
    // The row comes back to the inbox: still no notice.
    let row = world.row_mut(1);
    row.archived = false;
    row.pinned_order = Some(0);
    assert!(!relist(&mut src, &sent, &world).told);
    assert!(src.take_told().is_empty());
}

/// The sync switch refused: a notice of its own (a toast), and NO composer notice, which does not
/// show over Settings.
#[test]
fn the_switch_refused_is_told() {
    let world = World::new(vec![acct(1, Some(false), None)], vec![chat(1, 1)]);
    let (mut src, _sent, _) = loaded(&world);
    let ch = src.on_action_failed(
        "SetRoamingWrites",
        &CoreError::Stopped,
        &ActionSubject::None,
    );
    assert!(ch.told, "the toast is on its way");
    assert!(!ch.notice, "nothing in the composer");
    assert_eq!(src.notice(), "");
    let told = src.take_told();
    assert_eq!(told, [ChatTold::SwitchRefused]);
    assert_eq!(told[0].words(), "The setting could not be saved.");
    assert!(src.take_told().is_empty());
}

/// Mark as Unread on the OPEN chat stays: the click's own echo must not run the reading that
/// clears it. A newer incoming message (a real reading) still clears it.
#[test]
fn marking_the_open_chat_unread_stays() {
    let mut world = World::new(vec![acct(1, Some(true), None)], vec![chat(1, 1)]);
    let (mut src, sent, _) = loaded(&world);
    src.open_conversation(Some(CONV));
    settle(&mut src, &sent, &world);
    src.set_presence(true, true);
    assert_eq!(
        marks(&commands(&sent)),
        0,
        "nothing unread: nothing to read"
    );
    // The click, and the core's echo.
    assert!(src.set_marked_unread(CONV, true));
    let cmds = commands(&sent);
    assert!(matches!(
        cmds.as_slice(),
        [UiCommand::ChangeConversation {
            change: ConversationChange::SetMarkedUnread {
                marked_unread: true
            },
            ..
        }]
    ));
    world.row_mut(1).marked_unread = true;
    relist(&mut src, &sent, &world);
    src.on_event(&UiEvent::ConversationsChanged { ids: vec![CONV] });
    settle(&mut src, &sent, &world);
    assert_eq!(
        marks(&commands(&sent)),
        0,
        "the mark is not read away by its own echo"
    );
    assert!(src.chat_offer(CONV).marked_unread);
    // A new incoming message is a real reading: the mark goes.
    world
        .store
        .convs
        .get_mut(&1)
        .expect("conv")
        .push(message(1, 7, true));
    src.on_event(&UiEvent::MessagesChanged {
        conv: CONV,
        ids: vec![MessageId(100_007)],
    });
    settle(&mut src, &sent, &world);
    assert_eq!(marks(&commands(&sent)), 1, "a newer message reads the chat");
}

/// A chat left and opened again is read again, and that clears the mark the user put on it: the
/// chat in between had nothing unread, so nothing else would have reset `last_marked`.
#[test]
fn a_chat_opened_again_reads_the_mark_away() {
    let mut world = World::new(
        vec![acct(1, Some(true), None)],
        vec![chat(1, 1), chat(2, 1)],
    );
    world.store = Store::new(&[(1, 5), (2, 5)]);
    let (mut src, sent, _) = loaded(&world);
    src.open_conversation(Some(CONV));
    settle(&mut src, &sent, &world);
    src.set_presence(true, true);
    src.set_marked_unread(CONV, true);
    world.row_mut(1).marked_unread = true;
    relist(&mut src, &sent, &world);
    assert_eq!(
        marks(&commands(&sent)),
        0,
        "it stays while the chat is open"
    );
    // Leave it for a chat with nothing unread, and come back.
    src.open_conversation(Some(ConversationId(2)));
    settle(&mut src, &sent, &world);
    assert_eq!(
        marks(&commands(&sent)),
        0,
        "the other chat has nothing unread"
    );
    src.open_conversation(Some(CONV));
    settle(&mut src, &sent, &world);
    let cmds = commands(&sent);
    assert_eq!(marks(&cmds), 1, "opening it again reads it: {cmds:?}");
    assert!(cmds.iter().any(|c| matches!(
        c,
        UiCommand::MarkRead { conv } if *conv == CONV
    )));
}

/// Each command sends the chat's own change, to the chat that survived a merge.
#[test]
fn the_commands_send_the_chats_own_change() {
    let world = World::new(vec![acct(1, Some(true), Some(4))], vec![chat(1, 1)]);
    let (mut src, sent, _) = loaded(&world);
    commands(&sent);
    let one = |sent: &Sent| {
        let mut cmds = commands(sent);
        assert_eq!(cmds.len(), 1, "{cmds:?}");
        cmds.remove(0)
    };
    for archived in [true, false] {
        assert!(src.set_archived(CONV, archived));
        assert!(matches!(
            one(&sent),
            UiCommand::ChangeConversation {
                conv,
                change: ConversationChange::SetArchived { archived: a }
            } if conv == CONV && a == archived
        ));
    }
    for pinned in [true, false] {
        assert!(src.set_pinned(CONV, pinned));
        assert!(matches!(
            one(&sent),
            UiCommand::ChangeConversation {
                conv,
                change: ConversationChange::SetPinned { pinned: p }
            } if conv == CONV && p == pinned
        ));
    }
    for until in [Some(NOW + 3_600_000), Some(i64::MAX), None] {
        assert!(src.set_muted(CONV, until));
        assert!(matches!(
            one(&sent),
            UiCommand::ChangeConversation {
                conv,
                change: ConversationChange::SetMuted { until: u }
            } if conv == CONV && u == until
        ));
    }
    for marked in [true, false] {
        assert!(src.set_marked_unread(CONV, marked));
        assert!(matches!(
            one(&sent),
            UiCommand::ChangeConversation {
                conv,
                change: ConversationChange::SetMarkedUnread { marked_unread: m }
            } if conv == CONV && m == marked
        ));
    }
    for on in [true, false] {
        assert!(src.set_roaming_writes(AccountId(1), on));
        assert!(matches!(
            one(&sent),
            UiCommand::SetRoamingWrites { account, on: o }
                if account == AccountId(1) && o == on
        ));
    }
    // A chat merged away: the command goes to the one that is left.
    src.on_event(&UiEvent::ConversationMerged {
        from: ConversationId(8),
        into: CONV,
    });
    // (The merge asks for the list again: not a command of the click.)
    take_queries(&sent);
    assert!(src.set_pinned(ConversationId(8), true));
    assert!(matches!(
        one(&sent),
        UiCommand::ChangeConversation { conv, .. } if conv == CONV
    ));
    // Each click clears the composer's old notice.
    src.on_action_failed("SendContact", &CoreError::Stopped, &ActionSubject::None);
    assert!(!src.notice().is_empty());
    src.set_muted(CONV, None);
    assert_eq!(src.notice(), "");
}

/// The account's row says whether it has the switch, its state, and why the switch cannot be
/// flipped now: only an account that is not linked cannot (the zone is local, so an account that
/// is merely offline can).
#[test]
fn the_switch_row_says_why_not() {
    let mut logged_out = acct(2, Some(false), None);
    logged_out.state = AccountState::LoggedOut {
        reason: String::new(),
    };
    let mut no_switch = acct(3, None, None);
    no_switch.state = AccountState::LoggedOut {
        reason: String::new(),
    };
    let mut offline = acct(4, Some(true), Some(4));
    offline.conn = kourier_model::ConnState::Offline { retry_at_ms: None };
    let world = World::new(
        vec![acct(1, Some(false), None), logged_out, no_switch, offline],
        vec![chat(1, 1)],
    );
    let (src, _sent, _) = loaded(&world);
    let rows = src.accounts();
    let by = |id: i64| rows.iter().find(|r| r.id == AccountId(id)).expect("row");
    assert_eq!(by(1).roaming_writes, Some(false));
    assert_eq!(by(1).roaming_note, "", "a linked account can flip it");
    assert_eq!(by(2).roaming_writes, Some(false));
    assert_eq!(by(2).roaming_note, LINKED_ONLY);
    assert_eq!(by(3).roaming_writes, None);
    assert_eq!(by(3).roaming_note, "", "no switch, nothing to explain");
    assert_eq!(by(4).roaming_writes, Some(true));
    assert_eq!(by(4).roaming_note, "", "an offline account can flip it");
}

/// The open chat's menu words its mute against now and offers Mute once the mute ends; the core
/// sends no event for either, so the tick raises the header when the end passes between two
/// ticks, and when the local date changes under a muted chat (M2). The timer runs only while
/// there is a bounded end ahead; "always" needs none.
#[test]
fn a_mute_end_and_a_midnight_refresh_the_open_chats_offer() {
    let end = NOW + 60 * 60_000;
    let mut world = World::new(vec![acct(1, Some(true), None)], vec![chat(1, 1)]);
    world.row_mut(1).muted_until = Some(end);
    let (mut src, sent, clock) = loaded(&world);
    src.open_conversation(Some(CONV));
    settle(&mut src, &sent, &world);
    assert!(src.has_timers(), "a mute that ends wakes the tick");
    assert!(!src.tick().header, "the first tick only notes the time");
    clock.set(NOW + 30 * 60_000);
    assert!(!src.tick().header, "the mute stands");
    clock.set(end + 1_000);
    assert!(src.tick().header, "the end passed between two ticks");
    assert!(!src.tick().header, "told once");
    // A mute that lasts: the date changing under it changes "until today at ..." to "until
    // tomorrow at ...".
    world.row_mut(1).muted_until = Some(NOW + 5 * 24 * 3_600_000);
    relist(&mut src, &sent, &world);
    settle(&mut src, &sent, &world);
    // (NOW is 08:53 UTC on the fixture's calendar: +16 h is 00:53 of the next local day.)
    let hour = 3_600_000;
    clock.set(NOW + 5 * hour);
    src.tick();
    clock.set(NOW + 5 * hour + 60_000);
    assert!(!src.tick().header, "the same date");
    clock.set(NOW + 16 * hour);
    assert!(src.tick().header, "a new local day");
    // "Always" never ends: no timer for it.
    world.row_mut(1).muted_until = Some(i64::MAX);
    relist(&mut src, &sent, &world);
    settle(&mut src, &sent, &world);
    assert!(!src.has_timers());
}
