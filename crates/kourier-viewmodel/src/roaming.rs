//! What a chat offers of the settings its network keeps for the account's other devices
//! ("roaming": archive, pin, mute, marked unread; architecture §8.3), the times of "Mute", and
//! the short notices these changes leave.
//!
//! The rules are the core's: a chat's [`Roaming`] state is its OWN account's statement and
//! whether the network lists the chat (`ConversationRow::roaming`), and an account's pin limit
//! is `AccountRow::pin_limit`. This module only words them and hands a front-end flags, so QML
//! shows what it is given and decides nothing. Times are computed here from the source's clock
//! and calendar ([`TimeContext`]), never in the front-end.

use kourier_core::facade::{ConversationRow, Roaming};
use kourier_model::ConversationId;

use crate::conversations::shown_archived;
use crate::i18n::{trc, trfc, trnc};
use crate::time::{Calendar, CivilDate, TimeContext, civil_from_days, days_from_civil};

const MS_PER_HOUR: i64 = 3_600_000;
const MS_PER_DAY: i64 = 86_400_000;

/// The end of a mute that reads as "always": Desktop's `MAX_SAFE_DATE`
/// (`ts/util/timestamp.std.ts`), the greatest instant a JavaScript `Date` holds. Signal Desktop
/// writes "always" as that value and other clients as `i64::MAX`; both are "always" here.
pub const ALWAYS_FROM_MS: i64 = 8_640_000_000_000_000;

/// The earliest "Until…" the dialog takes: a minute from now (his answer), so a time that has
/// passed by the time the click arrives is never sent as a mute that already ended.
pub const UNTIL_MARGIN_MS: i64 = 60_000;

/// The hour the "Until…" dialog starts at, tomorrow (his answer: "Tomorrow 8:00"; Desktop's own
/// default, `MuteUntilDialog.dom.tsx`).
pub const UNTIL_START_HOUR: u32 = 8;

/// The greyed line under the entries of a chat whose account's switch is off. His words, verbatim
/// (`A10-decisions.md`): it names the switch by its own label.
pub(crate) fn switch_off_text() -> String {
    trc(
        "@info:status under the greyed entries of a chat's menu, the account's switch is off",
        "To change this on your phone too, turn on “Sync chat changes with my phone” for this account in Settings.",
    )
    .into_owned()
}

/// Why the entries and the switch cannot be used now: the account is not linked (logged out, its
/// link still going on, or its network turned off). One message for the chat's line and the
/// switch row's note, so the two always say the same.
pub(crate) fn not_linked_text() -> String {
    trc(
        "@info:status why a chat's entries and the sync switch are greyed, the account is not linked",
        "Available while this account is linked.",
    )
    .into_owned()
}

/// "You can pin up to 4 chats.": the sentence of a refused pin and of the line under a greyed
/// Pin. `limit` is the account's (`AccountRow::pin_limit`), never a constant.
pub(crate) fn pin_limit_text(limit: u32) -> String {
    trnc(
        "@info:status",
        "You can pin up to %1 chat.",
        "You can pin up to %1 chats.",
        u64::from(limit),
    )
}

/// What the account of a chat says besides its [`Roaming`] state, for [`offer_in_account`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccountFacts {
    /// The account is linked and running (`CoreSource::offers_unlink`'s condition): the entries
    /// work only then.
    pub linked: bool,
    /// How many chats the account may pin (`AccountRow::pin_limit`); `None` = the network states
    /// no limit and Pin is never greyed for a count.
    pub pin_limit: Option<u32>,
    /// How many chats of THIS account are pinned, as the list holds them.
    pub pinned: u32,
}

impl AccountFacts {
    /// A linked account with no stated limit: what [`offer_of`] assumes.
    pub const OPEN: Self = Self {
        linked: true,
        pin_limit: None,
        pinned: 0,
    };
}

/// What one chat's menu shows of Archive, Pin, Mute and Mark as Unread: flags for a front-end
/// to bind to. `Mark as Read` is not here (it is local and stays as it is for every network).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct ChatOffer {
    /// The four entries are shown ([`Roaming::Off`] or [`Roaming::On`]); `false`: not at all
    /// (a network that writes no such settings, a chat the network does not list yet).
    pub shown: bool,
    /// They work. `false` with `shown`: they are greyed and [`Self::why`] says why.
    pub on: bool,
    /// The chat is in the archive (the one rule, [`shown_archived`]: a pinned chat is not).
    pub archived: bool,
    /// The chat is pinned.
    pub pinned: bool,
    /// The user marked the chat unread.
    pub marked_unread: bool,
    /// The chat has unread messages or the mark: Mark as Unread is offered only when it has
    /// neither (Desktop's `!isUnread && !markedUnread`).
    pub has_unread: bool,
    /// A mute can be set: not for Note to Self (the Signal backend refuses it).
    pub mute_offered: bool,
    /// The chat is muted now (what Unmute clears).
    pub muted: bool,
    /// "Muted until tomorrow at 09:00" or "Muted always" while [`Self::muted`]; empty otherwise.
    pub muted_label: String,
    /// The line under the greyed entries; empty when they work or are not shown.
    pub why: String,
    /// Pin is greyed because the account has pinned as many chats as it may (the chat itself is
    /// not pinned); [`Self::pin_why`] says so.
    pub pin_at_limit: bool,
    /// "You can pin up to 4 chats." while [`Self::pin_at_limit`]; empty otherwise.
    pub pin_why: String,
}

/// The offer of a chat of a linked account that states no pin limit.
pub fn offer_of(c: &ConversationRow, time: &TimeContext<'_>) -> ChatOffer {
    offer_in_account(c, time, AccountFacts::OPEN)
}

/// The offer of a chat, given what its account says: whether it is linked now and how its pins
/// stand against its limit.
pub fn offer_in_account(
    c: &ConversationRow,
    time: &TimeContext<'_>,
    account: AccountFacts,
) -> ChatOffer {
    let shown = matches!(c.roaming, Roaming::Off | Roaming::On);
    // An account that is not linked cannot be asked, and a greyed line that points at its switch
    // would point at a switch that is disabled: the reason is the account's, whatever the
    // switch says.
    let on = c.roaming == Roaming::On && account.linked;
    let why = if !shown {
        String::new()
    } else if !account.linked {
        not_linked_text()
    } else if c.roaming == Roaming::Off {
        switch_off_text()
    } else {
        String::new()
    };
    let pinned = c.pinned_order.is_some();
    let pin_at_limit = on
        && !pinned
        && account
            .pin_limit
            .is_some_and(|limit| account.pinned >= limit);
    let pin_why = match account.pin_limit {
        Some(limit) if pin_at_limit => pin_limit_text(limit),
        _ => String::new(),
    };
    // (`muted_until` is what Unmute clears; `local_muted_until` has no writer today.)
    let muted_end = c.muted_until.filter(|end| *end > time.now_ms);
    ChatOffer {
        shown,
        on,
        archived: shown_archived(c.archived, c.pinned_order),
        pinned,
        marked_unread: c.marked_unread,
        has_unread: c.unread > 0 || c.marked_unread,
        mute_offered: !c.is_self,
        muted: muted_end.is_some(),
        muted_label: muted_end.map_or_else(String::new, |end| muted_label(end, time)),
        why,
        pin_at_limit,
        pin_why,
    }
}

/// "Muted until tomorrow at 09:00", or "Muted always" for a mute that has no end. The bound
/// is tested BEFORE the time is written: `until` of `i64::MAX` is not a date anybody means.
pub fn muted_label(end_ms: i64, time: &TimeContext<'_>) -> String {
    if end_ms >= ALWAYS_FROM_MS {
        return trc("@info:status a chat is muted with no end", "Muted always").into_owned();
    }
    trfc(
        "@info:status a chat is muted until a time, %1 the time (tomorrow at 09:00)",
        "Muted until %1",
        &[&time.until(end_ms)],
    )
}

/// The ready-made mutes of the menu, in Signal Desktop's order (`getMuteOptions.std.ts`:
/// 1 hour, 8 hours, 1 day, 1 week, then "Until…" (a dialog, [`until_start`]), then always).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MuteChoice {
    /// One hour from now.
    Hour,
    /// Eight hours from now.
    EightHours,
    /// One day from now.
    Day,
    /// One week from now.
    Week,
    /// For ever.
    Always,
}

impl MuteChoice {
    /// The choices in menu order.
    pub const ALL: [Self; 5] = [
        Self::Hour,
        Self::EightHours,
        Self::Day,
        Self::Week,
        Self::Always,
    ];

    /// The key a front-end names the choice by.
    pub fn key(self) -> &'static str {
        match self {
            Self::Hour => "hour",
            Self::EightHours => "eight",
            Self::Day => "day",
            Self::Week => "week",
            Self::Always => "always",
        }
    }

    /// The choice named by `key`.
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.key() == key)
    }
}

/// The end of a mute chosen from the menu, ms since the epoch: `now_ms` plus the duration, or
/// `i64::MAX` for always (what the Signal backend maps back to "for ever").
pub fn mute_end(choice: MuteChoice, now_ms: i64) -> i64 {
    let span = match choice {
        MuteChoice::Hour => MS_PER_HOUR,
        MuteChoice::EightHours => 8 * MS_PER_HOUR,
        MuteChoice::Day => MS_PER_DAY,
        MuteChoice::Week => 7 * MS_PER_DAY,
        MuteChoice::Always => return i64::MAX,
    };
    now_ms.saturating_add(span)
}

/// A date and a time on the user's own clock, as the "Until…" dialog shows them. A field beyond
/// its range rolls over (day 31 of a 30-day month is the 1st of the next, hour 24 is the next
/// day); the dialog's pickers give valid ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalPick {
    /// Year.
    pub year: i64,
    /// Month, 1 to 12.
    pub month: u32,
    /// Day of the month.
    pub day: u32,
    /// Hour, 0 to 23.
    pub hour: u32,
    /// Minute, 0 to 59.
    pub minute: u32,
}

/// Where the "Until…" dialog starts: tomorrow at 8:00 on the local clock (his answer).
pub fn until_start(time: &TimeContext<'_>) -> LocalPick {
    let tomorrow = civil_from_days(time.day_number(time.now_ms) + 1);
    LocalPick {
        year: tomorrow.year,
        month: tomorrow.month,
        day: tomorrow.day,
        hour: UNTIL_START_HOUR,
        minute: 0,
    }
}

/// The instant (ms since the epoch) a local date and time is, under `calendar`. The offset that
/// counts is the one in force AT the picked time, not the one in force now: tomorrow 8:00 is a
/// different instant on either side of a clock change. A time the clock skips (03:30 on the day
/// it goes forward) reads with the offset from BEFORE the change, so it lands where the clock
/// shows 04:30; a time it repeats (03:30 on the day it goes back) is the FIRST of the two.
pub fn local_to_ms(pick: LocalPick, calendar: &dyn Calendar) -> i64 {
    let day = days_from_civil(CivilDate {
        year: pick.year,
        month: pick.month,
        day: pick.day,
    });
    let local = day * MS_PER_DAY + (i64::from(pick.hour) * 60 + i64::from(pick.minute)) * 60_000;
    let offset_ms = |at: i64| i64::from(calendar.utc_offset_s(at)) * 1000;
    // The offsets a day either side of the pick: the one before a change and the one after it.
    let before = offset_ms(local - MS_PER_DAY);
    let after = offset_ms(local + MS_PER_DAY);
    let first = local - before;
    if offset_ms(first) == before {
        return first;
    }
    let second = local - after;
    if offset_ms(second) == after {
        return second;
    }
    // The clock never showed this time: it was skipped.
    first
}

/// Whether a mute that ends at `end_ms` is allowed: at least a minute from now (his answer).
pub fn until_allowed(end_ms: i64, now_ms: i64) -> bool {
    end_ms >= now_ms.saturating_add(UNTIL_MARGIN_MS)
}

/// A short notice a change of a chat or of the sync switch leaves, told once by
/// [`crate::UiSource::take_told`]. The front-end shows `words()` as a passive notification (with
/// "Undo" for an archive).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatTold {
    /// The chat is archived now (and was pinned before: Undo pins it again).
    Archived {
        /// The chat.
        conv: ConversationId,
        /// It was pinned when the click came (archiving unpins it).
        was_pinned: bool,
    },
    /// The chat is back in the inbox.
    Unarchived {
        /// The chat.
        conv: ConversationId,
    },
    /// The chat is marked unread.
    MarkedUnread {
        /// The chat.
        conv: ConversationId,
    },
    /// The sync switch could not be changed.
    SwitchRefused,
    /// A change of a chat (archive, pin, mute, mark as unread) was refused: the words say why
    /// (the pin limit, or that it was not made). Told as a toast, because the click came from
    /// the list as often as from the open chat.
    Refused {
        /// The chat.
        conv: ConversationId,
        /// The sentence.
        words: String,
    },
}

impl ChatTold {
    /// The words of the notice.
    pub fn words(&self) -> String {
        match self {
            Self::Archived { .. } => trc("@info:status", "Chat archived"),
            Self::Unarchived { .. } => trc("@info:status", "Chat returned to inbox"),
            Self::MarkedUnread { .. } => trc("@info:status", "Chat marked unread"),
            Self::SwitchRefused => trc("@info:status", "The setting could not be saved."),
            Self::Refused { words, .. } => return words.clone(),
        }
        .into_owned()
    }

    /// The kind a front-end names the notice by.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Archived { .. } => "archived",
            Self::Unarchived { .. } => "unarchived",
            Self::MarkedUnread { .. } => "marked-unread",
            Self::SwitchRefused => "switch-refused",
            Self::Refused { .. } => "refused",
        }
    }

    /// The chat the notice is about; `None` for the switch's.
    pub fn conv(&self) -> Option<ConversationId> {
        match self {
            Self::Archived { conv, .. }
            | Self::Unarchived { conv }
            | Self::MarkedUnread { conv }
            | Self::Refused { conv, .. } => Some(*conv),
            Self::SwitchRefused => None,
        }
    }

    /// An archive that took a pin with it (Undo pins the chat again).
    pub fn was_pinned(&self) -> bool {
        matches!(
            self,
            Self::Archived {
                was_pinned: true,
                ..
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::{FixedOffset, weekday};
    use kourier_core::facade::ConversationRow;
    use kourier_model::{AccountId, ConvKind};

    const NOW: i64 = 1_760_000_000_000;

    /// A chat row with the fields these tests care about.
    fn row(roaming: Roaming) -> ConversationRow {
        ConversationRow {
            id: ConversationId(1),
            account: AccountId(1),
            kind: ConvKind::Direct,
            is_self: false,
            title: None,
            peer: None,
            avatar: None,
            preview: None,
            last_activity: 0,
            unread: 0,
            mentions: 0,
            marked_unread: false,
            muted_until: None,
            local_muted_until: None,
            archived: false,
            pinned_order: None,
            blocked: false,
            request_pending: false,
            features_hash: 0,
            has_draft: false,
            hidden: false,
            can_send: true,
            roaming,
            protected: false,
        }
    }

    fn offer(c: &ConversationRow) -> ChatOffer {
        offer_of(c, &TimeContext::new(NOW, &FixedOffset(0)))
    }

    /// Kyiv's rule: UTC+2, and UTC+3 from the last Sunday of March 01:00 UTC to the last Sunday
    /// of October 01:00 UTC. Only the two years these tests need are checked by name.
    struct Kyiv;

    fn last_sunday(year: i64, month: u32) -> i64 {
        // March and October both have 31 days; 0 = Monday ... 6 = Sunday.
        let last = days_from_civil(CivilDate {
            year,
            month,
            day: 31,
        });
        last - (i64::from(weekday(last)) + 1) % 7
    }

    impl Calendar for Kyiv {
        fn utc_offset_s(&self, ts_ms: i64) -> i32 {
            let year = civil_from_days(ts_ms.div_euclid(MS_PER_DAY)).year;
            let start = last_sunday(year, 3) * MS_PER_DAY + MS_PER_HOUR;
            let end = last_sunday(year, 10) * MS_PER_DAY + MS_PER_HOUR;
            if ts_ms >= start && ts_ms < end {
                3 * 3600
            } else {
                2 * 3600
            }
        }
    }

    /// An instant from a date and a time in UTC.
    fn utc(year: i64, month: u32, day: u32, hour: i64, minute: i64) -> i64 {
        days_from_civil(CivilDate { year, month, day }) * MS_PER_DAY + (hour * 60 + minute) * 60_000
    }

    fn pick(year: i64, month: u32, day: u32, hour: u32, minute: u32) -> LocalPick {
        LocalPick {
            year,
            month,
            day,
            hour,
            minute,
        }
    }

    #[test]
    fn the_calendar_of_these_tests_changes_on_the_days_the_rule_says() {
        // The clock goes forward on 2027-03-28 and back on 2026-10-25.
        assert_eq!(Kyiv.utc_offset_s(utc(2027, 3, 28, 0, 59)), 2 * 3600);
        assert_eq!(Kyiv.utc_offset_s(utc(2027, 3, 28, 1, 0)), 3 * 3600);
        assert_eq!(Kyiv.utc_offset_s(utc(2026, 10, 25, 0, 59)), 3 * 3600);
        assert_eq!(Kyiv.utc_offset_s(utc(2026, 10, 25, 1, 0)), 2 * 3600);
    }

    /// Signal Desktop's list (`getMuteOptions.std.ts`): 1 hour, 8 hours, 1 day, 1 week, always.
    /// "Always" is a mute with an end, never none (0 would be an unmute).
    #[test]
    fn the_mute_list_is_signal_desktops() {
        let ends: Vec<i64> = MuteChoice::ALL.iter().map(|c| mute_end(*c, NOW)).collect();
        assert_eq!(
            ends,
            [
                NOW + 3_600_000,
                NOW + 8 * 3_600_000,
                NOW + 86_400_000,
                NOW + 7 * 86_400_000,
                i64::MAX
            ]
        );
        assert!(
            ends.iter().all(|end| *end > NOW),
            "every choice ends after now"
        );
        // The keys a front-end names them by round-trip.
        for c in MuteChoice::ALL {
            assert_eq!(MuteChoice::from_key(c.key()), Some(c));
        }
        assert_eq!(MuteChoice::from_key("fortnight"), None);
        // A clock at the end of time does not wrap.
        assert_eq!(mute_end(MuteChoice::Week, i64::MAX - 5), i64::MAX);
    }

    /// His answer: "Tomorrow 8:00". Tomorrow's 8:00 is a different instant on either side of a
    /// clock change, because the offset that counts is the one at the picked time.
    #[test]
    fn until_starts_tomorrow_at_eight_on_both_sides_of_a_clock_change() {
        let check = |now: i64, day: (i64, u32, u32)| {
            let time = TimeContext::new(now, &Kyiv);
            let start = until_start(&time);
            assert_eq!(start, pick(day.0, day.1, day.2, 8, 0), "tomorrow, 8:00");
            let ms = local_to_ms(start, &Kyiv);
            let there = TimeContext::new(ms, &Kyiv);
            assert_eq!(there.clock(ms), "08:00", "the clock shows 8:00 then");
            assert_eq!(
                there.date(ms),
                CivilDate {
                    year: day.0,
                    month: day.1,
                    day: day.2
                }
            );
            ms
        };
        // Spring: the clock goes forward during the night of 27 to 28 March 2027. Tomorrow 8:00
        // is 05:00 UTC (UTC+3), not 06:00 (the +2 of now).
        let spring = check(utc(2027, 3, 27, 18, 0), (2027, 3, 28));
        assert_eq!(spring, utc(2027, 3, 28, 5, 0));
        // Autumn: it goes back during the night of 24 to 25 October 2026. Tomorrow 8:00 is
        // 06:00 UTC (UTC+2), not 05:00 (the +3 of now).
        let autumn = check(utc(2026, 10, 24, 17, 0), (2026, 10, 25));
        assert_eq!(autumn, utc(2026, 10, 25, 6, 0));
        // An ordinary day, and a late evening that is already "tomorrow" by the UTC date.
        let plain = check(utc(2026, 11, 10, 9, 0), (2026, 11, 11));
        assert_eq!(plain, utc(2026, 11, 11, 6, 0));
        let late = check(utc(2026, 11, 10, 23, 30), (2026, 11, 12));
        assert_eq!(late, utc(2026, 11, 12, 6, 0));
    }

    /// L8: a time the clock skips reads with the offset from before the change, a time it
    /// repeats is the first one.
    #[test]
    fn a_time_the_clock_skips_and_one_it_repeats() {
        // 2027-03-28 03:30 does not exist (the clock goes 02:59 -> 04:00). It reads with the +2
        // from before the change: 01:30 UTC, which the clock shows as 04:30.
        let skipped = local_to_ms(pick(2027, 3, 28, 3, 30), &Kyiv);
        assert_eq!(skipped, utc(2027, 3, 28, 1, 30));
        let clock = TimeContext::new(skipped, &Kyiv);
        assert_eq!(clock.clock(skipped), "04:30");
        // 2026-10-25 03:30 happens twice (03:30 at +3, then again at +2): the first, 00:30 UTC.
        let repeated = local_to_ms(pick(2026, 10, 25, 3, 30), &Kyiv);
        assert_eq!(repeated, utc(2026, 10, 25, 0, 30));
        // ... and the second one is later by the hour the clock went back.
        assert_eq!(
            Kyiv.utc_offset_s(repeated),
            3 * 3600,
            "the first 03:30 is still summer time"
        );
        // The minutes around them are plain.
        assert_eq!(
            local_to_ms(pick(2027, 3, 28, 2, 59), &Kyiv),
            utc(2027, 3, 28, 0, 59)
        );
        assert_eq!(
            local_to_ms(pick(2027, 3, 28, 4, 0), &Kyiv),
            utc(2027, 3, 28, 1, 0)
        );
        assert_eq!(
            local_to_ms(pick(2026, 10, 25, 4, 0), &Kyiv),
            utc(2026, 10, 25, 2, 0)
        );
        // One fixed offset: the offset subtracted.
        assert_eq!(
            local_to_ms(pick(2026, 11, 11, 8, 0), &FixedOffset(-5 * 3600)),
            utc(2026, 11, 11, 13, 0)
        );
    }

    /// His answer: the earliest is a minute from now.
    #[test]
    fn until_is_refused_inside_the_next_minute() {
        assert!(until_allowed(NOW + 60_000, NOW), "exactly a minute is fine");
        assert!(!until_allowed(NOW + 59_999, NOW));
        assert!(!until_allowed(NOW, NOW));
        assert!(!until_allowed(NOW - 1, NOW), "the past");
        assert!(until_allowed(NOW + 86_400_000, NOW));
        assert!(
            until_allowed(i64::MAX, NOW),
            "no overflow at the end of time"
        );
    }

    /// Each state of the core's row shows what the brief's table says: nothing for a network
    /// that writes no such settings or a chat it does not list, greyed with the line when the
    /// account's switch is off, working when it is on.
    #[test]
    fn the_offer_says_what_each_state_shows() {
        let hidden = offer(&row(Roaming::Hidden));
        assert!(!hidden.shown && !hidden.on && hidden.why.is_empty());
        let not_listed = offer(&row(Roaming::NotListed));
        assert!(
            !not_listed.shown && !not_listed.on && not_listed.why.is_empty(),
            "a request or a number-only person offers none of the four"
        );
        let off = offer(&row(Roaming::Off));
        assert!(off.shown && !off.on, "greyed");
        assert_eq!(
            off.why,
            "To change this on your phone too, turn on “Sync chat changes with my phone” for this account in Settings."
        );
        let on = offer(&row(Roaming::On));
        assert!(on.shown && on.on);
        assert!(on.why.is_empty(), "nothing to explain when it works");
        // The state of the chat itself, whatever the roaming state.
        let mut c = row(Roaming::On);
        c.archived = true;
        c.marked_unread = true;
        c.unread = 0;
        c.pinned_order = None;
        let o = offer(&c);
        assert!(o.archived && o.marked_unread && o.has_unread && !o.pinned);
        c.marked_unread = false;
        c.unread = 3;
        assert!(offer(&c).has_unread, "unread messages count");
        c.unread = 0;
        assert!(!offer(&c).has_unread, "Mark as Unread is offered");
    }

    /// M9: the archive state is the one rule (`shown_archived`): a pinned chat is not archived,
    /// whatever its raw flag says (the phone can state both).
    #[test]
    fn archived_is_the_one_rule() {
        let mut c = row(Roaming::On);
        c.archived = true;
        c.pinned_order = None;
        assert!(offer(&c).archived);
        c.pinned_order = Some(0);
        let o = offer(&c);
        assert!(!o.archived, "pinned and flagged: in the inbox");
        assert!(o.pinned);
        c.archived = false;
        assert!(!offer(&c).archived);
    }

    #[test]
    fn note_to_self_offers_no_mute() {
        let mut c = row(Roaming::On);
        assert!(offer(&c).mute_offered);
        c.is_self = true;
        let o = offer(&c);
        assert!(!o.mute_offered, "Note to Self has no mute");
        assert!(o.shown && o.on, "the other three are there");
    }

    /// The label under Unmute names the end; a mute that has ended is no mute; Unmute clears
    /// `muted_until` (`local_muted_until` has no writer, so it does not count here).
    #[test]
    fn muted_label_names_the_end() {
        let mut c = row(Roaming::On);
        assert!(!offer(&c).muted && offer(&c).muted_label.is_empty());
        // In a day: "tomorrow at 14:20" (UTC offset 0 in this context).
        c.muted_until = Some(NOW + 86_400_000);
        let o = offer(&c);
        assert!(o.muted);
        let tomorrow = TimeContext::new(NOW, &FixedOffset(0)).until(NOW + 86_400_000);
        assert_eq!(o.muted_label, format!("Muted until {tomorrow}"));
        assert!(o.muted_label.starts_with("Muted until tomorrow at "));
        // Later today.
        c.muted_until = Some(NOW + 3_600_000);
        assert!(offer(&c).muted_label.starts_with("Muted until today at "));
        // Always, however the network writes it.
        c.muted_until = Some(i64::MAX);
        let o = offer(&c);
        assert!(o.muted);
        assert_eq!(o.muted_label, "Muted always");
        // Ended (and exactly now): not muted.
        c.muted_until = Some(NOW);
        assert!(!offer(&c).muted && offer(&c).muted_label.is_empty());
        c.muted_until = Some(NOW - 1);
        assert!(!offer(&c).muted);
        // The local mute is not what the menu clears.
        c.muted_until = None;
        c.local_muted_until = Some(NOW + 3_600_000);
        assert!(!offer(&c).muted);
    }

    /// L7: Desktop's bound, `MAX_SAFE_DATE`, is "always" too, and is tested before the time is
    /// written (`until` of such an instant is not a date).
    #[test]
    fn always_from_desktops_bound() {
        let time = TimeContext::new(NOW, &FixedOffset(0));
        assert_eq!(ALWAYS_FROM_MS, 8_640_000_000_000_000);
        assert_eq!(muted_label(8_640_000_000_000_000, &time), "Muted always");
        assert_eq!(muted_label(8_640_000_000_000_001, &time), "Muted always");
        assert_eq!(muted_label(i64::MAX, &time), "Muted always");
        let just_below = muted_label(8_639_999_999_999_999, &time);
        assert!(
            just_below.starts_with("Muted until "),
            "one millisecond below the bound is still a time: {just_below}"
        );
    }

    #[test]
    fn the_words_of_the_notices_and_their_kinds() {
        let conv = ConversationId(7);
        let archived = ChatTold::Archived {
            conv,
            was_pinned: true,
        };
        assert_eq!(archived.words(), "Chat archived");
        assert_eq!(
            ChatTold::Unarchived { conv }.words(),
            "Chat returned to inbox"
        );
        assert_eq!(
            ChatTold::MarkedUnread { conv }.words(),
            "Chat marked unread"
        );
        assert_eq!(
            ChatTold::SwitchRefused.words(),
            "The setting could not be saved."
        );
        assert_eq!(archived.kind(), "archived");
        assert_eq!(archived.conv(), Some(conv));
        assert!(archived.was_pinned());
        assert!(
            !ChatTold::Archived {
                conv,
                was_pinned: false
            }
            .was_pinned()
        );
        assert_eq!(ChatTold::SwitchRefused.conv(), None);
        assert!(!ChatTold::SwitchRefused.was_pinned());
    }
}
