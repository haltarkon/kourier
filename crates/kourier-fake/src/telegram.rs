//! The rehearsal network of Telegram (KT-1b, D-2 F5): the fake that PLAYS Telegram for the run
//! tool's rehearsal (`FakeNetwork::rehearsal_telegram()`), and the synthetic constants it speaks
//! with. It is a fake for a debug build with a throwaway profile and no network at all; nothing
//! here is a model of what Telegram really sends, and `kourier-fake` holds no Telegram code (no
//! type of `kourier-telegram` or `kourier-tdlib`): the shapes below are copied from what the real
//! backend states, and the backend's own tests compare the two.
//!
//! What it plays, in the order a real run meets it:
//!
//! - The descriptor is Telegram's WITHOUT the risk notice (the warning page is the UI's and has
//!   tests of its own; no dev command ticks it, and none should). It states no roaming setting
//!   (`roaming_writes: None`): Telegram writes none yet.
//! - The QR login shows a FIRST code for [`Timings::first_code`] that nobody scans (the run tool's
//!   first step looks at one code on screen and may stop there), then a second of
//!   [`Timings::code`] that the phone scans at once. The account the phone scans in is
//!   [`OWN_NUMBER`]'s and has two-step verification: [`PASSWORD`] is asked right after the scan
//!   (its hint is [`PASSWORD_HINT`]), and only then is the number checked against the tail the run
//!   tool expects ([`Script::honour_number_tail`]): a tail the number does not end in fails the
//!   login at the `qr` step with `NotTheExpectedAccount`, after the password, and no account is made
//!   (TDLib's order: the check runs once the login is Ready).
//! - The `phone` flow has ONE row, the own number: the typed number is checked against the expected
//!   tail BEFORE any code; the code ([`CODE`]) comes through the app and a new one by SMS after
//!   [`Timings::resend`]; then the password, a login e-mail ([`EMAIL_ADDRESS`], its code
//!   [`EMAIL_CODE`]) and the link. Any other number fails at `registration` (no account).
//! - A fresh link brings what Telegram's start sync brings: the account's history is served on
//!   demand, so a direct chat and a basic group each arrive with ONE `HistoryBatch` (the chat's
//!   last message and the unread ones, `has_more: false`), while the account says `Syncing`, then
//!   `Online`. A resumed account goes online and replays nothing: the script cursor is durable.
//! - [`FakeControl::arrive`](crate::FakeControl::arrive) puts one text from the other member into
//!   the first chat of a kind of the RUNNING account, on cue; its id has Telegram's shape.
//!
//! The ids have the shapes the real backend gives: users `tg:<digits>` (the account's own ids are
//! `[tg:<id>, pn:<12 digits>]`, `tg` first), a basic group `tg:-<digits>`, a message the server's
//! id shifted by 20 bits in decimal. Every value is synthetic. The two tails are 8 digits, the
//! right one is the end of [`OWN_NUMBER`], and neither is a substring of any other value here (a
//! test pins it, and that no constant is a substring of another), so a search of the files of a
//! rehearsal for a tail finds a leak and nothing else.
//!
//! Left to the run tool's later half (one small commit here then): more number rows (a flood
//! wait, a banned number, an e-mail setup), the frozen and logged-out cues, the supergroup, a
//! resume backlog, a longer sync window.

use std::time::Duration;

use kourier_backend_api::BackendEvent;
use kourier_model::{
    CodeDelivery, CodeResend, CodeVia, ContactPatch, ConvKey, ConversationFeatures, HistorySource,
    Identifier, IdentifierKind, MemberRole, NetworkKind, Patch, UserId, Window,
};

use crate::network::FakeNetwork;
use crate::script::{
    Delivery, LoginScript, PhoneAccount, PhoneLoginScript, Script, ScriptError, Step,
};
use crate::scripts::{T0, conversation, member, remote};

/// The rehearsal account's own profile name.
pub const OWN_NAME: &str = "Rehearsal Tgowner";
/// The other person's name in the rehearsal (the run tool searches the files of a rehearsal for it).
pub const PEER_NAME: &str = "Rehearsal Tgpeer";
/// The rehearsal group's title.
pub const GROUP_NAME: &str = "Rehearsal Tggroup";
/// The rehearsal account's phone number (digits only): what the person types at `phone`.
pub const OWN_NUMBER: &str = "999000842917";
/// The other person's phone number (digits only): the number Telegram shows for them.
pub const PEER_NUMBER: &str = "999000263508";
/// The rehearsal account's Telegram user id (the part of its `tg:` id after the scheme).
pub const OWN_TG_ID: &str = "7041835296";
/// The other person's Telegram user id.
pub const PEER_TG_ID: &str = "6129470358";
/// The rehearsal group's chat id, signed as TDLib states a basic group's (the key is `tg:` and
/// this).
pub const GROUP_CHAT_ID: &str = "-4128790356";
/// The last digits of [`OWN_NUMBER`] a rehearsal types for the right phone.
pub const RIGHT_TAIL: &str = "00842917";
/// Last digits no number of the rehearsal ends in: the wrong phone.
pub const WRONG_TAIL: &str = "73610528";
/// The stem of the texts of the direct chat's history (`<stem> <n>`): the run tool searches the
/// files of a rehearsal for them, so a text that leaks shows.
pub const DIRECT_WORDS: &str = "direct talk";
/// The stem of the texts of the group's history (`<stem> <n>`).
pub const GROUP_WORDS: &str = "group talk";
/// The stem of the text of an arrival (`<stem> <n>`): the one the fake playing WhatsApp uses
/// (`rehearsal::ARRIVAL_WORDS`; a test pins that they are the same words).
pub const ARRIVAL_WORDS: &str = "Rehearsal arrival";
/// The login code the rehearsal's own number is sent (and the code of its new one).
pub const CODE: &str = "48215";
/// The two-step password of the rehearsal account.
pub const PASSWORD: &str = "kettle-rehearsal-pass";
/// The hint its owner gave: the password step's note.
pub const PASSWORD_HINT: &str = "a kitchen appliance";
/// The code the login e-mail's address is sent.
pub const EMAIL_CODE: &str = "730294";
/// The address the person types at the e-mail step.
pub const EMAIL_ADDRESS: &str = "rehearsal-mailbox@example.invalid";

/// The server ids (before the 20-bit shift) of the direct chat's history, the group's, and the
/// arrivals: disjoint, and every arrival is newer than any history message.
const DIRECT_ID_BASE: i64 = 100;
const GROUP_ID_BASE: i64 = 200;
const ARRIVAL_ID_BASE: i64 = 1000;
// The ranges do not meet: three direct messages, two group ones, any number of arrivals.
const _: () = assert!(DIRECT_ID_BASE + 3 <= GROUP_ID_BASE && GROUP_ID_BASE + 2 <= ARRIVAL_ID_BASE);

/// How long the rehearsal's walks take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timings {
    /// The first QR code is shown this long and nobody scans it.
    pub first_code: Duration,
    /// The second QR code is shown this long; the phone scans it at once.
    pub code: Duration,
    /// The wait before a new login code may be asked for at the `phone` flow's code step, from
    /// the first showing of the step.
    pub resend: Duration,
}

impl Timings {
    /// What a rehearsal run uses: a first code of fifteen seconds, a second of three, a new
    /// login code after thirty.
    pub const REAL: Self = Self {
        first_code: Duration::from_secs(15),
        code: Duration::from_secs(3),
        resend: Duration::from_secs(30),
    };
}

/// The features the Telegram backend states for a conversation in TG-3 (its `receive::chats::
/// features`, one source with the backend): edits are taken without a window, received deletes
/// have no window yet, read receipts and typing are always on, the history comes on demand. The
/// send side (formatting, files, the edit and delete windows) is not built yet.
/// `kourier-telegram`'s tests compare the two, so a change of either fails there. A direct chat and
/// a group state the same today; `_group` is the place where they will differ (mentions).
pub fn telegram_features(_group: bool) -> ConversationFeatures {
    ConversationFeatures {
        edit_receive: Some(Window {
            max_age_secs: None,
            max_count: None,
        }),
        delete_receive: None,
        read_receipts: true,
        typing: true,
        history: HistorySource::OnDemand,
        ..ConversationFeatures::default()
    }
}

/// The id of the `n`-th arrival (1, 2, ...) as TDLib shapes a message id: a server id shifted by
/// 20 bits, in decimal. Newer than every message of the rehearsal's history.
pub fn arrival_id(n: i64) -> String {
    message_id(ARRIVAL_ID_BASE + n)
}

/// A message id as TDLib states it (`server_id << 20`), in decimal.
fn message_id(server_id: i64) -> String {
    (server_id << 20).to_string()
}

fn id(scheme: &str, value: &str) -> Result<UserId, ScriptError> {
    Ok(UserId::from_parts(scheme, value)?)
}

/// The rehearsal account's script (see the module docs). The same script plays a fresh start and
/// a resumed one.
///
/// # Errors
/// Never in practice: the ids are constants of a valid shape.
pub fn script(timings: Timings) -> Result<Script, ScriptError> {
    let own_tg = id("tg", OWN_TG_ID)?;
    let own_pn = id("pn", OWN_NUMBER)?;
    let peer = id("tg", PEER_TG_ID)?;
    let direct = ConvKey::Direct(peer.clone());
    let group = ConvKey::Group(format!("tg:{GROUP_CHAT_ID}").into());

    // History rows are old and silent (the start sync is not news); the first `read` of them are
    // read, the rest are the unread ones the sync brings after the chat's last message.
    let history = |conv: &ConvKey, base: i64, n: i64, read: i64, text: &str| {
        (0..n)
            .map(|i| {
                let mut m = remote(
                    conv,
                    &peer,
                    &message_id(base + i),
                    T0 + (base + i) * 60_000,
                    &format!("{text} {i}"),
                );
                m.silent = true;
                m.read = i < read;
                m
            })
            .collect::<Vec<_>>()
    };

    // What Telegram shows of the other person: a name and the number they share.
    let mut contact = ContactPatch::new(peer.clone());
    contact.profile_name = Patch::Set(PEER_NAME.into());
    contact.identifiers = Some(vec![Identifier {
        kind: IdentifierKind::Phone,
        value: format!("+{PEER_NUMBER}"),
    }]);
    contact.number_shown = Some(true);
    let contact = BackendEvent::ContactUpsert(contact);

    // One commit per chat (the fresh-link load): the author's contact, the chat, ONE batch.
    let direct_chat = Step::Deliver(Delivery::new(vec![
        contact.clone(),
        BackendEvent::ConversationUpsert(conversation(
            &direct,
            None,
            None,
            &telegram_features(false),
        )),
        BackendEvent::HistoryBatch {
            conv: direct.clone(),
            messages: history(&direct, DIRECT_ID_BASE, 3, 1, DIRECT_WORDS),
            has_more: false,
            progress: None,
        },
    ]));
    let group_chat = Step::Deliver(Delivery::new(vec![
        contact,
        BackendEvent::ConversationUpsert(conversation(
            &group,
            Some(GROUP_NAME),
            Some(vec![
                member(&peer, MemberRole::Admin),
                member(&own_tg, MemberRole::Member),
            ]),
            &telegram_features(true),
        )),
        BackendEvent::HistoryBatch {
            conv: group.clone(),
            messages: history(&group, GROUP_ID_BASE, 2, 0, GROUP_WORDS),
            has_more: false,
            progress: None,
        },
    ]));

    // The account the phone scans in is the one the `phone` flow's row names; it has the
    // two-step password and, in the phone flow, a login e-mail.
    let delivery = CodeDelivery {
        via: CodeVia::App,
        resend: Some(CodeResend {
            after: timings.resend,
            via: CodeVia::Sms,
        }),
    };
    let own_row = PhoneAccount::new(OWN_NUMBER, CODE, delivery)
        .with_password(PASSWORD, Some(PASSWORD_HINT))
        .with_email(EMAIL_CODE);

    let mut s = Script::new("rehearsal-telegram", vec![own_tg, own_pn])
        .with_number(OWN_NUMBER)
        .with_phone_login(PhoneLoginScript::new(vec![own_row]))
        .with_first_qr_for(timings.first_code);
    // It plays Telegram as the real run sees it: no roaming setting is written yet.
    s.roaming_writes = None;
    s.display_name = Some(OWN_NAME.to_owned());
    s.login = LoginScript::Qr {
        ask_device_name: false,
        codes: 2,
        valid_for: timings.code,
        scanned_at: Some(1),
        linked_as: None,
    };
    s.honour_number_tail = true;
    s.steps = vec![
        // Ready shows Syncing until the start sync is over (the real backend's Online follows the
        // acks of the last chat).
        Step::Conn(kourier_model::ConnState::Syncing { fraction: None }),
        direct_chat,
        group_chat,
        Step::CatchUpDone,
    ];
    Ok(s)
}

impl FakeNetwork {
    /// The fake that rehearses a real Telegram run (KT-1b, [`crate::telegram`]): Telegram as the
    /// UI sees it but WITHOUT the risk notice, a QR login whose first code nobody scans and whose
    /// second links, the account's password after the scan and the number check after the
    /// password, the `phone` flow of the own number, a direct chat and a basic group served on
    /// link, and [`FakeControl::arrive`](crate::FakeControl::arrive) for one message on cue.
    /// Every id and name is synthetic.
    ///
    /// # Panics
    /// Never: the script's ids are constants of a valid shape.
    pub fn rehearsal_telegram() -> Self {
        Self::rehearsal_telegram_with(Timings::REAL)
    }

    /// [`rehearsal_telegram`](Self::rehearsal_telegram) with other waits (tests use milliseconds).
    ///
    /// # Panics
    /// Never: the script's ids are constants of a valid shape.
    pub fn rehearsal_telegram_with(timings: Timings) -> Self {
        let script = script(timings).expect("the rehearsal's ids are valid");
        Self::playing(NetworkKind::Telegram, script).without_risk_notice()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `&str` constant of the module (the order is the file's).
    const CONSTANTS: [(&str, &str); 18] = [
        ("OWN_NAME", OWN_NAME),
        ("PEER_NAME", PEER_NAME),
        ("GROUP_NAME", GROUP_NAME),
        ("OWN_NUMBER", OWN_NUMBER),
        ("PEER_NUMBER", PEER_NUMBER),
        ("OWN_TG_ID", OWN_TG_ID),
        ("PEER_TG_ID", PEER_TG_ID),
        ("GROUP_CHAT_ID", GROUP_CHAT_ID),
        ("RIGHT_TAIL", RIGHT_TAIL),
        ("WRONG_TAIL", WRONG_TAIL),
        ("DIRECT_WORDS", DIRECT_WORDS),
        ("GROUP_WORDS", GROUP_WORDS),
        ("ARRIVAL_WORDS", ARRIVAL_WORDS),
        ("CODE", CODE),
        ("PASSWORD", PASSWORD),
        ("PASSWORD_HINT", PASSWORD_HINT),
        ("EMAIL_CODE", EMAIL_CODE),
        ("EMAIL_ADDRESS", EMAIL_ADDRESS),
    ];

    const SENTINEL: &str = "000000000000000";

    #[test]
    fn no_constant_is_a_substring_of_another_and_the_ids_have_the_real_shapes() {
        // The one containment there is on purpose: the right tail ends the own number.
        assert!(OWN_NUMBER.ends_with(RIGHT_TAIL));
        for (i, (name, value)) in CONSTANTS.iter().enumerate() {
            assert!(!value.is_empty(), "{name}");
            assert!(!value.contains(SENTINEL), "{name} holds the sentinel");
            for (j, (other_name, other)) in CONSTANTS.iter().enumerate() {
                if i == j || (*name, *other_name) == ("RIGHT_TAIL", "OWN_NUMBER") {
                    continue;
                }
                assert!(!other.contains(value), "{name} is inside {other_name}");
            }
        }
        // Distinct values (a substring test passes two equal short ones only when one is empty).
        let mut values: Vec<&str> = CONSTANTS.iter().map(|(_, v)| *v).collect();
        values.sort_unstable();
        values.dedup();
        assert_eq!(values.len(), CONSTANTS.len());

        // The shapes: numbers of 12 digits (the display digits of a number, no `+`), user ids of at
        // most 10 digits, a group's signed chat id, tails of 8 digits.
        for number in [OWN_NUMBER, PEER_NUMBER] {
            assert_eq!(number.len(), 12, "{number}");
            assert!(number.bytes().all(|b| b.is_ascii_digit()), "{number}");
        }
        for tg in [OWN_TG_ID, PEER_TG_ID] {
            assert!((6..=10).contains(&tg.len()), "{tg}");
            assert!(tg.bytes().all(|b| b.is_ascii_digit()), "{tg}");
            let user = id("tg", tg).expect("id");
            assert_eq!((user.scheme(), user.value()), ("tg", tg));
        }
        assert!(GROUP_CHAT_ID.starts_with('-') && GROUP_CHAT_ID.parse::<i64>().is_ok());
        let key = ConvKey::Group(format!("tg:{GROUP_CHAT_ID}").into());
        assert_eq!(
            key.validate(),
            Ok(()),
            "the leading `-` passes the scoped-id rule"
        );
        assert_eq!(ConvKey::parse(&key.encode()).expect("a key"), key);
        let own_pn = id("pn", OWN_NUMBER).expect("id");
        assert_eq!(own_pn.value(), OWN_NUMBER);
        for tail in [RIGHT_TAIL, WRONG_TAIL] {
            assert!(tail.len() >= 8, "{tail}");
            assert!(tail.bytes().all(|b| b.is_ascii_digit()), "{tail}");
        }
        assert!(!OWN_NUMBER.ends_with(WRONG_TAIL));
        assert!(!PEER_NUMBER.ends_with(RIGHT_TAIL) && !PEER_NUMBER.ends_with(WRONG_TAIL));
        assert!(EMAIL_ADDRESS.ends_with("@example.invalid"));
        assert_eq!(
            ARRIVAL_WORDS,
            crate::rehearsal::ARRIVAL_WORDS,
            "the arrival's text comes from the one place every fake reads"
        );
    }

    /// A constant added to the module but not to [`CONSTANTS`] would escape the substring test.
    #[test]
    fn every_string_constant_of_the_module_is_in_the_substring_test() {
        let source = include_str!("telegram.rs");
        let named: Vec<&str> = source
            .lines()
            .filter_map(|l| l.strip_prefix("pub const "))
            .filter(|l| l.contains(": &str ="))
            .filter_map(|l| l.split(':').next())
            .collect();
        let listed: Vec<&str> = CONSTANTS.iter().map(|(n, _)| *n).collect();
        assert_eq!(named, listed, "the file's order");
    }

    #[test]
    fn a_message_id_is_a_server_id_shifted_by_twenty_bits_and_an_arrival_is_the_newest() {
        assert_eq!(message_id(1), "1048576");
        assert_eq!(arrival_id(1), ((ARRIVAL_ID_BASE + 1) << 20).to_string());
        assert!(
            arrival_id(1)
                .parse::<i64>()
                .is_ok_and(|n| n % (1 << 20) == 0)
        );
        // Every arrival is newer than any history message and no two ids are the same.
        let number = |id: String| id.parse::<i64>().expect("decimal");
        let newest_history = number(message_id(GROUP_ID_BASE + 1));
        assert!(number(arrival_id(1)) > newest_history);
        assert_ne!(arrival_id(1), arrival_id(2));
    }

    #[test]
    fn the_real_timings_are_fifteen_three_and_thirty_seconds() {
        assert_eq!(Timings::REAL.first_code, Duration::from_secs(15));
        assert_eq!(Timings::REAL.code, Duration::from_secs(3));
        assert_eq!(Timings::REAL.resend, Duration::from_secs(30));
        let s = script(Timings::REAL).expect("script");
        assert_eq!(s.first_qr_for, Some(Duration::from_secs(15)));
        match s.login {
            LoginScript::Qr {
                codes,
                valid_for,
                scanned_at,
                ..
            } => assert_eq!(
                (codes, valid_for, scanned_at),
                (2, Duration::from_secs(3), Some(1)),
                "two codes, the second scanned"
            ),
            other => panic!("{other:?}"),
        }
        assert!(s.honour_number_tail);
        assert_eq!(s.roaming_writes, None);
        assert_eq!(s.number.as_deref(), Some(OWN_NUMBER));
    }

    #[test]
    fn the_own_row_has_everything_so_one_rehearsal_walks_every_step() {
        let s = script(Timings::REAL).expect("script");
        let rows = s.phone_login.expect("a phone flow").numbers;
        assert_eq!(rows.len(), 1, "the own number is the one row");
        let row = &rows[0];
        assert_eq!(row.digits, OWN_NUMBER);
        assert_eq!(row.code, CODE);
        assert_eq!(row.refusal, None);
        let password = row.password.as_ref().expect("a password");
        assert_eq!(password.password, PASSWORD);
        assert_eq!(password.hint.as_deref(), Some(PASSWORD_HINT));
        assert_eq!(row.email_code.as_deref(), Some(EMAIL_CODE));
        assert_eq!(row.delivery.via, CodeVia::App);
        assert_eq!(
            row.delivery.resend,
            Some(CodeResend {
                after: Timings::REAL.resend,
                via: CodeVia::Sms
            })
        );
    }
}
