//! Local dates and the time labels of the conversation list and the timeline.
//!
//! The viewmodel never reads the system clock or time zone itself: callers pass a
//! [`TimeContext`] with "now" and a [`Calendar`] that knows the local UTC offset at any instant
//! (the Qt layer asks `QDateTime`, tests and the fixture use [`FixedOffset`]). Labels are
//! 24-hour and English for now; they go through [`crate::i18n`] once translations start (§9.5).

use std::fmt::Write as _;

use crate::i18n::{Msg, trc, trc_noop, trfc, trm};

const MS_PER_DAY: i64 = 86_400_000;

/// Local-time knowledge: the UTC offset in effect at an instant.
pub trait Calendar {
    /// Offset of local time from UTC in seconds (east positive) at `ts_ms` (ms since the
    /// Unix epoch).
    fn utc_offset_s(&self, ts_ms: i64) -> i32;
}

/// A calendar with one fixed UTC offset (tests, fixture, or UTC).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FixedOffset(pub i32);

impl Calendar for FixedOffset {
    fn utc_offset_s(&self, _ts_ms: i64) -> i32 {
        self.0
    }
}

/// A local calendar date.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CivilDate {
    /// Year (proleptic Gregorian).
    pub year: i64,
    /// Month, 1 … 12.
    pub month: u32,
    /// Day of the month, 1 … 31.
    pub day: u32,
}

/// Days since 1970-01-01 → civil date (Howard Hinnant's `civil_from_days`).
pub fn civil_from_days(days: i64) -> CivilDate {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = yoe + era * 400 + i64::from(month <= 2);
    CivilDate {
        year,
        // Both are in small ranges (see above), so the casts cannot truncate.
        month: month as u32,
        day: day as u32,
    }
}

/// Civil date → days since 1970-01-01 (Howard Hinnant's `days_from_civil`).
pub fn days_from_civil(date: CivilDate) -> i64 {
    let month = i64::from(date.month);
    let year = date.year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400); // [0, 399]
    let mp = (month + 9) % 12; // March = 0
    let doy = (153 * mp + 2) / 5 + i64::from(date.day) - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Day of the week, 0 = Monday … 6 = Sunday, of a day number.
pub fn weekday(days: i64) -> u32 {
    // 1970-01-01 was a Thursday (index 3).
    (days + 3).rem_euclid(7) as u32
}

const WEEKDAYS: [Msg; 7] = [
    trc_noop("@item:intext weekday, full name", "Monday"),
    trc_noop("@item:intext weekday, full name", "Tuesday"),
    trc_noop("@item:intext weekday, full name", "Wednesday"),
    trc_noop("@item:intext weekday, full name", "Thursday"),
    trc_noop("@item:intext weekday, full name", "Friday"),
    trc_noop("@item:intext weekday, full name", "Saturday"),
    trc_noop("@item:intext weekday, full name", "Sunday"),
];
const WEEKDAYS_SHORT: [Msg; 7] = [
    trc_noop("@item:intext weekday, abbreviated", "Mon"),
    trc_noop("@item:intext weekday, abbreviated", "Tue"),
    trc_noop("@item:intext weekday, abbreviated", "Wed"),
    trc_noop("@item:intext weekday, abbreviated", "Thu"),
    trc_noop("@item:intext weekday, abbreviated", "Fri"),
    trc_noop("@item:intext weekday, abbreviated", "Sat"),
    trc_noop("@item:intext weekday, abbreviated", "Sun"),
];
const MONTHS: [Msg; 12] = [
    trc_noop("@item:intext month name, full name", "January"),
    trc_noop("@item:intext month name, full name", "February"),
    trc_noop("@item:intext month name, full name", "March"),
    trc_noop("@item:intext month name, full name", "April"),
    trc_noop("@item:intext month name, full name", "May"),
    trc_noop("@item:intext month name, full name", "June"),
    trc_noop("@item:intext month name, full name", "July"),
    trc_noop("@item:intext month name, full name", "August"),
    trc_noop("@item:intext month name, full name", "September"),
    trc_noop("@item:intext month name, full name", "October"),
    trc_noop("@item:intext month name, full name", "November"),
    trc_noop("@item:intext month name, full name", "December"),
];
const MONTHS_SHORT: [Msg; 12] = [
    trc_noop("@item:intext month name, abbreviated", "Jan"),
    trc_noop("@item:intext month name, abbreviated", "Feb"),
    trc_noop("@item:intext month name, abbreviated", "Mar"),
    trc_noop("@item:intext month name, abbreviated", "Apr"),
    trc_noop("@item:intext month name, abbreviated", "May"),
    trc_noop("@item:intext month name, abbreviated", "Jun"),
    trc_noop("@item:intext month name, abbreviated", "Jul"),
    trc_noop("@item:intext month name, abbreviated", "Aug"),
    trc_noop("@item:intext month name, abbreviated", "Sep"),
    trc_noop("@item:intext month name, abbreviated", "Oct"),
    trc_noop("@item:intext month name, abbreviated", "Nov"),
    trc_noop("@item:intext month name, abbreviated", "Dec"),
];

/// "Now" plus the local calendar: everything a time label depends on.
#[derive(Clone, Copy)]
pub struct TimeContext<'a> {
    /// The current instant, ms since the Unix epoch.
    pub now_ms: i64,
    /// Local offsets.
    pub calendar: &'a dyn Calendar,
}

impl<'a> TimeContext<'a> {
    /// A context for `now_ms` in `calendar`.
    pub fn new(now_ms: i64, calendar: &'a dyn Calendar) -> Self {
        Self { now_ms, calendar }
    }

    fn local_ms(&self, ts_ms: i64) -> i64 {
        ts_ms.saturating_add(i64::from(self.calendar.utc_offset_s(ts_ms)) * 1000)
    }

    /// Local day number (days since 1970-01-01 in local time) of an instant.
    pub fn day_number(&self, ts_ms: i64) -> i64 {
        self.local_ms(ts_ms).div_euclid(MS_PER_DAY)
    }

    /// Local date of an instant.
    pub fn date(&self, ts_ms: i64) -> CivilDate {
        civil_from_days(self.day_number(ts_ms))
    }

    /// Local wall-clock time, `HH:MM` (24-hour).
    pub fn clock(&self, ts_ms: i64) -> String {
        let ms_of_day = self.local_ms(ts_ms).rem_euclid(MS_PER_DAY);
        let minutes = ms_of_day / 60_000;
        format!("{:02}:{:02}", minutes / 60, minutes % 60)
    }

    /// Label of the conversation list: `14:05` today, `Yesterday`, a short weekday within the
    /// last week, `12 Sep` this year, `12 Sep 2025` before. Instants in the future (clock skew)
    /// count as today.
    pub fn list_label(&self, ts_ms: i64) -> String {
        let today = self.day_number(self.now_ms);
        let day = self.day_number(ts_ms).min(today);
        let age = today - day;
        if age == 0 {
            return self.clock(ts_ms);
        }
        if age == 1 {
            return trc(
                "@item:intext in the chat list, the day before today",
                "Yesterday",
            )
            .into_owned();
        }
        if age < 7 {
            return trm(WEEKDAYS_SHORT[weekday(day) as usize]).into_owned();
        }
        let d = civil_from_days(day);
        let month = trm(MONTHS_SHORT[(d.month - 1) as usize]);
        if d.year == civil_from_days(today).year {
            format!("{} {month}", d.day)
        } else {
            format!("{} {month} {}", d.day, d.year)
        }
    }

    /// A coming instant in words, for "until": `today at 14:00`, `tomorrow at 09:00`,
    /// `18 Oct at 14:00` (the year too when it is not this one). An instant that has passed
    /// still reads as the day it falls on (`today at 14:00` stays true all day); callers show
    /// another sentence once the instant is past. Each shape is its own message, because a
    /// translated date or day word may decline.
    pub fn until(&self, ts_ms: i64) -> String {
        let today = self.day_number(self.now_ms);
        let day = self.day_number(ts_ms);
        let clock = self.clock(ts_ms);
        if day == today {
            return trfc(
                "@info:status a time later today, %1 the time",
                "today at %1",
                &[&clock],
            );
        }
        if day == today + 1 {
            return trfc(
                "@info:status a time tomorrow, %1 the time",
                "tomorrow at %1",
                &[&clock],
            );
        }
        let d = civil_from_days(day);
        let month = trm(MONTHS_SHORT[(d.month - 1) as usize]);
        let date = if d.year == civil_from_days(today).year {
            format!("{} {month}", d.day)
        } else {
            format!("{} {month} {}", d.day, d.year)
        };
        trfc(
            "@info:status a day and a time, %1 the day (18 Oct), %2 the time",
            "%1 at %2",
            &[&date, &clock],
        )
    }

    /// A local date in short form, for a date that is not "recent" (when a link was made):
    /// `12 Sep` this year, `12 Sep 2025` before. Unlike [`Self::list_label`] it never says a
    /// time, `Yesterday` or a weekday.
    pub fn short_date(&self, ts_ms: i64) -> String {
        let day = self.day_number(ts_ms);
        let d = civil_from_days(day);
        let month = trm(MONTHS_SHORT[(d.month - 1) as usize]);
        if d.year == civil_from_days(self.day_number(self.now_ms)).year {
            format!("{} {month}", d.day)
        } else {
            format!("{} {month} {}", d.day, d.year)
        }
    }

    /// Label of a timeline date header for a local day number: `Today`, `Yesterday`, the
    /// weekday within the last week, `Saturday, 12 September` this year, `12 September 2025`
    /// before.
    pub fn date_header(&self, day: i64) -> String {
        let today = self.day_number(self.now_ms);
        let age = today - day;
        if age <= 0 {
            return trc("@title date header of the timeline, today", "Today").into_owned();
        }
        if age == 1 {
            return trc(
                "@title date header of the timeline, the day before today",
                "Yesterday",
            )
            .into_owned();
        }
        let wd = trm(WEEKDAYS[weekday(day) as usize]);
        if age < 7 {
            return wd.into_owned();
        }
        let d = civil_from_days(day);
        let month = trm(MONTHS[(d.month - 1) as usize]);
        let mut out = String::new();
        if d.year == civil_from_days(today).year {
            let _ = write!(out, "{wd}, {} {month}", d.day);
        } else {
            let _ = write!(out, "{} {month} {}", d.day, d.year);
        }
        out
    }

    /// The next instant at which [`Self::list_label`] or [`Self::date_header`] of any past
    /// instant can change: the next local midnight. A model schedules one refresh there.
    pub fn next_label_change(&self) -> i64 {
        let offset_ms = i64::from(self.calendar.utc_offset_s(self.now_ms)) * 1000;
        let next_day = self.day_number(self.now_ms) + 1;
        next_day * MS_PER_DAY - offset_ms
    }
}

/// Instant (ms since the epoch) of a local wall-clock time in a fixed offset; for fixtures and
/// tests.
pub fn local_instant(date: CivilDate, hour: u32, minute: u32, offset: FixedOffset) -> i64 {
    let days = days_from_civil(date);
    let local = days * MS_PER_DAY + i64::from(hour) * 3_600_000 + i64::from(minute) * 60_000;
    local - i64::from(offset.0) * 1000
}

/// Human file size: `512 B`, `1.2 KB`, `3.4 MB`, `1.0 GB` (decimal units, like Signal).
pub fn file_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    while value >= 999.95 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// The two sizes of the line under an upload ("Uploading 45% · 12 of 32 MB"), both in the unit
/// of the total, which is the unit and the figure of [`file_size`]'s label for the same file
/// (the detail line beside it): from 10 units up his whole-number form, below 10 units one
/// decimal for both ("1.2 of 4.2 MB"). The whole total is that label's one-decimal figure
/// rounded half up (31.46 MB is labelled "31.5 MB" and read "of 32 MB", never "of 31"); the
/// first number is the bytes done without a unit, rounded down in the same form (never above
/// the total, and the total itself once all is up). Below 1 KB the unit is bytes. Decimal
/// units, like [`file_size`].
pub fn file_size_pair(done: u64, total: u64) -> (String, String) {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if total < 1000 {
        return (done.min(total).to_string(), format!("{total} B"));
    }
    // The label's own unit and one-decimal figure, in tenths: read back from its text, so the
    // line cannot disagree with the label by a rounding.
    let label = file_size(total);
    let (figure, name) = label.split_once(' ').unwrap_or((&label, "KB"));
    let mut unit = UNITS.iter().position(|u| *u == name).unwrap_or(0);
    let mut tenths: u128 = figure
        .replace('.', "")
        .parse()
        .unwrap_or_else(|_| u128::from(total) / 100);
    // His whole-number form from 10 units up; a total that rounds to 1000 of its unit is 1.0
    // of the next one, in the decimal form.
    let mut whole = tenths >= 100;
    if whole {
        tenths = (tenths + 5) / 10 * 10;
        if tenths >= 10_000 && unit < UNITS.len() - 1 {
            unit += 1;
            tenths = 10;
            whole = false;
        }
    }
    let divisor = 1000_u128.pow(unit as u32 + 1);
    // The bytes done in tenths of the unit, rounded down, never above the total.
    let done_tenths = if done >= total {
        tenths
    } else {
        (u128::from(done) * 10 / divisor).min(tenths)
    };
    let show = |t: u128| {
        if whole {
            (t / 10).to_string()
        } else {
            format!("{}.{}", t / 10, t % 10)
        }
    };
    (
        show(done_tenths),
        format!("{} {}", show(tenths), UNITS[unit]),
    )
}

/// Duration as `m:ss` (or `h:mm:ss`), for voice notes and videos.
pub fn duration(ms: u32) -> String {
    let total = ms / 1000;
    let (h, m, s) = (total / 3600, (total / 60) % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KYIV: FixedOffset = FixedOffset(3 * 3600);

    /// The sizes under an upload ("Uploading 45% · 12 of 30 MB"): from 10 units up his whole
    /// numbers, below 10 units one decimal for both, always in the unit of the total and the
    /// figure of the size label; the done part rounded down and equal to the total once all is
    /// up, bytes below 1 KB, and a total that rounds to 1000 moves up a unit.
    #[test]
    fn the_upload_sizes_are_whole_numbers_from_ten_units_and_one_decimal_below() {
        let pair = |done, total| {
            let (up, all) = file_size_pair(done, total);
            format!("{up} of {all}")
        };
        assert_eq!(pair(0, 0), "0 of 0 B");
        assert_eq!(pair(500, 999), "500 of 999 B");
        assert_eq!(pair(5_000, 999), "999 of 999 B", "never above the total");
        assert_eq!(pair(999, 1_000), "0.9 of 1.0 KB", "done is rounded down");
        assert_eq!(pair(100_000, 400_000), "100 of 400 KB");
        assert_eq!(pair(399_999, 400_000), "399 of 400 KB");
        assert_eq!(pair(12_900_000, 30_000_000), "12 of 30 MB");
        assert_eq!(pair(29_999_000, 30_000_000), "29 of 30 MB");
        assert_eq!(pair(30_000_000, 30_000_000), "30 of 30 MB");
        // Below 10 units: one decimal for both, the total the text of the size label.
        assert_eq!(pair(1_000_000, 1_600_000), "1.0 of 1.6 MB");
        assert_eq!(pair(1_599_999, 1_600_000), "1.5 of 1.6 MB");
        assert_eq!(pair(1_600_000, 1_600_000), "1.6 of 1.6 MB", "all is up");
        assert_eq!(pair(1_200_000, 4_200_000), "1.2 of 4.2 MB");
        assert_eq!(pair(2_500_000_000, 3_400_000_000), "2.5 of 3.4 GB");
        // The total is the label's one-decimal figure rounded half up: 31.46 MB is labelled
        // "31.5 MB", so the line says "of 32 MB", never "of 31".
        assert_eq!(file_size(31_457_280), "31.5 MB");
        assert_eq!(pair(12_000_000, 31_457_280), "12 of 32 MB");
        assert_eq!(pair(31_457_280, 31_457_280), "32 of 32 MB");
        assert_eq!(file_size(31_440_000), "31.4 MB");
        assert_eq!(pair(1_000_000, 31_440_000), "1 of 31 MB");
        // 9.96 MB is labelled "10.0 MB": 10 units, so the whole form.
        assert_eq!(file_size(9_960_000), "10.0 MB");
        assert_eq!(pair(5_000_000, 9_960_000), "5 of 10 MB");
        assert_eq!(file_size(9_940_000), "9.9 MB");
        assert_eq!(pair(5_000_000, 9_940_000), "5.0 of 9.9 MB");
        // A total that rounds to 1000 KB is 1.0 of the next unit.
        assert_eq!(pair(999_000, 999_600), "0.9 of 1.0 MB");
        assert_eq!(pair(999_600, 999_600), "1.0 of 1.0 MB");
    }

    fn date(year: i64, month: u32, day: u32) -> CivilDate {
        CivilDate { year, month, day }
    }

    #[test]
    fn civil_round_trip() {
        assert_eq!(civil_from_days(0), date(1970, 1, 1));
        assert_eq!(days_from_civil(date(2000, 3, 1)), 11_017);
        assert_eq!(civil_from_days(-1), date(1969, 12, 31));
        for days in (-800_000..800_000).step_by(997) {
            assert_eq!(days_from_civil(civil_from_days(days)), days);
        }
        assert_eq!(
            civil_from_days(days_from_civil(date(2024, 2, 29))),
            date(2024, 2, 29)
        );
        // 2026-10-06 is a Tuesday.
        assert_eq!(weekday(days_from_civil(date(2026, 10, 6))), 1);
    }

    #[test]
    fn list_labels() {
        let now = local_instant(date(2026, 10, 6), 14, 32, KYIV);
        let t = TimeContext::new(now, &KYIV);
        let at = |d: CivilDate, h, m| local_instant(d, h, m, KYIV);
        assert_eq!(t.list_label(at(date(2026, 10, 6), 9, 5)), "09:05");
        assert_eq!(t.list_label(at(date(2026, 10, 6), 0, 0)), "00:00");
        assert_eq!(t.list_label(at(date(2026, 10, 5), 23, 59)), "Yesterday");
        assert_eq!(t.list_label(at(date(2026, 10, 3), 12, 0)), "Sat");
        assert_eq!(t.list_label(at(date(2026, 9, 30), 12, 0)), "Wed");
        assert_eq!(t.list_label(at(date(2026, 9, 29), 12, 0)), "29 Sep");
        assert_eq!(t.list_label(at(date(2025, 12, 31), 12, 0)), "31 Dec 2025");
        // A future instant (clock skew) shows its time.
        assert_eq!(t.list_label(at(date(2026, 10, 7), 1, 0)), "01:00");
    }

    #[test]
    fn until_says_today_tomorrow_or_the_date() {
        let now = local_instant(date(2026, 10, 6), 14, 32, KYIV);
        let t = TimeContext::new(now, &KYIV);
        let at = |d: CivilDate, h, m| local_instant(d, h, m, KYIV);
        assert_eq!(t.until(at(date(2026, 10, 6), 18, 5)), "today at 18:05");
        // The local day decides, not 24 hours: 00:10 is tomorrow although it is 9 hours away.
        assert_eq!(t.until(at(date(2026, 10, 7), 0, 10)), "tomorrow at 00:10");
        assert_eq!(t.until(at(date(2026, 10, 18), 14, 0)), "18 Oct at 14:00");
        assert_eq!(t.until(at(date(2027, 1, 2), 9, 0)), "2 Jan 2027 at 09:00");
        // The same instant is another day in another offset.
        let utc = TimeContext::new(now, &FixedOffset(0));
        assert_eq!(utc.until(at(date(2026, 10, 7), 0, 10)), "today at 21:10");
    }

    #[test]
    fn local_midnight_follows_the_offset() {
        // 22:30 UTC on Oct 5 is already Oct 6 in Kyiv.
        let utc = local_instant(date(2026, 10, 5), 22, 30, FixedOffset(0));
        let kyiv = TimeContext::new(utc, &KYIV);
        let utc_ctx = TimeContext::new(utc, &FixedOffset(0));
        assert_eq!(kyiv.date(utc), date(2026, 10, 6));
        assert_eq!(kyiv.clock(utc), "01:30");
        assert_eq!(utc_ctx.date(utc), date(2026, 10, 5));
        let next = kyiv.next_label_change();
        assert_eq!(next, local_instant(date(2026, 10, 7), 0, 0, KYIV));
    }

    #[test]
    fn date_headers() {
        let now = local_instant(date(2026, 10, 6), 14, 32, KYIV);
        let t = TimeContext::new(now, &KYIV);
        let day = |d| days_from_civil(d);
        assert_eq!(t.date_header(day(date(2026, 10, 6))), "Today");
        assert_eq!(t.date_header(day(date(2026, 10, 5))), "Yesterday");
        assert_eq!(t.date_header(day(date(2026, 10, 2))), "Friday");
        assert_eq!(
            t.date_header(day(date(2026, 9, 12))),
            "Saturday, 12 September"
        );
        assert_eq!(t.date_header(day(date(2024, 1, 2))), "2 January 2024");
    }

    #[test]
    fn sizes_and_durations() {
        assert_eq!(file_size(0), "0 B");
        assert_eq!(file_size(999), "999 B");
        assert_eq!(file_size(1000), "1.0 KB");
        assert_eq!(file_size(1_234_567), "1.2 MB");
        assert_eq!(file_size(999_960), "1.0 MB");
        assert_eq!(file_size(3_400_000_000), "3.4 GB");
        assert_eq!(duration(0), "0:00");
        assert_eq!(duration(65_400), "1:05");
        assert_eq!(duration(3_725_000), "1:02:05");
    }
}
