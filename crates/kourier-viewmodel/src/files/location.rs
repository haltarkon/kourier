//! The question before files leave with where they were taken (LOC; his answers 2026-10-26).
//!
//! When Send meets files whose verdict is true ([`StripFile::location`]) and the setting
//! "Files with location data" says "Ask each time", the composer asks once for all of them
//! ([`FileStrip::location_question`]) and sends nothing yet. The question is a SNAPSHOT
//! ([`LocationSnapshot`], critic F3): what it was asked about (the strip, the files that go
//! with their `as_file` and verdicts, the reply) and the text of the press. The answer
//! ([`FileStrip::answer_location`]) applies only to that: when any of it moved while the
//! dialog was open (a folder listing added a file, a verdict changed, the chat or the reply
//! changed), nothing goes and the question comes again ([`LocationOutcome::Stale`]). The
//! caption is not part of the comparison: the text that goes is the snapshot's own, taken
//! from the press (the lead's call BQ-R1), so a late re-tell of a long caption moves
//! nothing.
//!
//! [Send with Location] sends every file that goes, the asked-about ones with
//! `with_location`. [Don't Send] keeps the located files in the strip, held, and sends the
//! others with the text; when no file would go, NOTHING goes and the text stays (a caption
//! written for that photo must not leave alone; BQ2). The focus starts on [Don't Send] only
//! when that answer sends nothing; otherwise the body says what still goes (F2).
//!
//! Names and the text are personal data: `Debug` prints neither.

use std::path::PathBuf;

use kourier_model::AttachmentKind;

use super::{FileStrip, LocationPref, StripFile, shown_name};
use crate::i18n::{trc, trfc, trnc};

/// At most this many names are listed in the question; the rest is "and %1 more".
pub const SHOWN_NAMES: usize = 5;

/// What the dialog says: everything it shows comes from here (the dialog only shows it).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct LocationQuestion {
    /// "Send It Anyway?", or "Send Them Anyway?" for several files (QH1).
    pub title: String,
    /// His sentence by kind ("This video contains where it was taken."), or the count.
    pub text: String,
    /// The names of the files it is about, up to [`SHOWN_NAMES`] (outside text: plain text).
    pub names: Vec<String>,
    /// "and %1 more" when more files than names are asked about; else empty.
    pub more: String,
    /// What [Don't Send] still sends ("The other files and the text are sent without it."),
    /// empty when it sends nothing (F2).
    pub still_goes: String,
    /// The initial focus is on [Don't Send]: true only when that answer sends nothing; else
    /// it is on the check box, where Enter does nothing (F2).
    pub focus_dont_send: bool,
    /// What a screen reader is told with the dialog and its focused control: the body, the
    /// names of the files (and "and %1 more", U7) and the line of what still goes (F15).
    pub accessible: String,
    /// "Send with Location".
    pub send_label: String,
    /// "Don't Send".
    pub hold_label: String,
    /// "Remember my choice".
    pub remember_label: String,
    /// "You can change this in Settings."
    pub settings_hint: String,
    /// How many files it is about.
    pub count: usize,
}

crate::safe_debug!(LocationQuestion {
    title,
    focus_dont_send,
    count
});

/// One file the question is about, as it stood when it was asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AskedFile {
    /// The strip entry.
    pub id: u64,
    /// It goes as the file it is.
    pub as_file: bool,
    /// Its verdict: it would leave with its location.
    pub located: bool,
}

/// What a question was asked about (F3, BQ-R1): the strip, the conversation, every file that
/// goes with its `as_file` and verdict, the reply, and the text of the press.
#[derive(Clone, PartialEq, Eq)]
pub struct LocationSnapshot {
    strip: u64,
    conversation: i64,
    files: Vec<AskedFile>,
    reply: Option<i64>,
    text: String,
}

crate::safe_debug!(LocationSnapshot {
    strip,
    conversation,
    files,
    reply
});

impl LocationSnapshot {
    /// The conversation it was asked in.
    pub fn conversation(&self) -> i64 {
        self.conversation
    }

    /// The text that goes with the answer: the one of the press, not the strip's caption.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The files that go, as asked about.
    pub fn files(&self) -> &[AskedFile] {
        &self.files
    }

    /// The entries the question was about (those with a true verdict), in order.
    pub fn located(&self) -> Vec<u64> {
        self.files
            .iter()
            .filter(|f| f.located)
            .map(|f| f.id)
            .collect()
    }
}

/// The user's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocationAnswer {
    /// [Send with Location].
    Send,
    /// [Don't Send]: the located files stay, held.
    Hold,
}

impl LocationAnswer {
    /// What "Remember my choice" makes of the setting with this answer.
    pub fn remembered(self) -> LocationPref {
        match self {
            Self::Send => LocationPref::Always,
            Self::Hold => LocationPref::Never,
        }
    }
}

/// What a send after the answer takes (the composer hands it to `UiSource::send_files`,
/// then calls [`FileStrip::take_sendable`]).
#[derive(Clone, PartialEq, Eq)]
pub struct LocationSend {
    /// The files, in order ([`FileStrip::sendable`] after the answer).
    pub paths: Vec<PathBuf>,
    /// For each: it goes as the file it is.
    pub as_file: Vec<bool>,
    /// For each: the user agreed that it leaves with its location.
    pub with_location: Vec<bool>,
    /// The text of the press (the caption).
    pub text: String,
}

crate::safe_debug!(LocationSend {
    as_file,
    with_location
});

/// What came of an answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocationOutcome {
    /// What the question was about moved while it was open: nothing goes, nothing is
    /// remembered, and Send asks again about what stands now.
    Stale,
    /// Send this.
    Send {
        /// What goes.
        send: LocationSend,
        /// The setting "Remember my choice" asks for; `None` when it was not ticked.
        remember: Option<LocationPref>,
    },
    /// No file would go: nothing goes and the text stays in the composer (BQ2).
    Nothing {
        /// The setting "Remember my choice" asks for; `None` when it was not ticked.
        remember: Option<LocationPref>,
    },
}

impl LocationOutcome {
    /// The setting to store (through Settings, the one owner of the pref, F1).
    pub fn remember(&self) -> Option<LocationPref> {
        match self {
            Self::Stale => None,
            Self::Send { remember, .. } | Self::Nothing { remember } => *remember,
        }
    }
}

impl FileStrip {
    /// What the question would be asked about now, from the press of Send with `text` in
    /// `conversation`, quoting `reply`.
    fn asked(&self, conversation: i64, text: &str, reply: Option<i64>) -> LocationSnapshot {
        LocationSnapshot {
            strip: self.base,
            conversation,
            files: self
                .files
                .iter()
                .filter(|f| f.goes())
                .map(|f| AskedFile {
                    id: f.id,
                    as_file: f.as_file,
                    located: f.location == Some(true),
                })
                .collect(),
            reply,
            text: text.to_owned(),
        }
    }

    /// The question Send asks under "Ask each time", with its snapshot; `None` when no file
    /// that goes would leave with its location, or while a verdict is pending (Send waits
    /// then). Held files are asked about again.
    pub fn location_question(
        &self,
        conversation: i64,
        text: &str,
        reply: Option<i64>,
    ) -> Option<(LocationQuestion, LocationSnapshot)> {
        if !self.can_send() {
            return None;
        }
        let located: Vec<&StripFile> = self.files.iter().filter(|f| f.located()).collect();
        let first = located.first()?;
        let n = located.len();
        let others = self
            .files
            .iter()
            .filter(|f| f.goes() && !f.located())
            .count();
        let title = if n == 1 {
            trc(
                "@title:dialog asking before one file that holds where it was taken is sent",
                "Send It Anyway?",
            )
        } else {
            trc(
                "@title:dialog asking before several files that hold where they were taken are sent",
                "Send Them Anyway?",
            )
        }
        .into_owned();
        // His answer for audio holds for several recordings too: "made", not "taken" (U8).
        let recordings = located
            .iter()
            .all(|f| matches!(f.kind, AttachmentKind::Audio | AttachmentKind::Voice));
        let text_line = if n == 1 {
            one_file_text(first)
        } else if recordings {
            trnc(
                "@info the question before several recordings that hold where they were made are sent, %1 how many",
                "%1 recording contains where it was made.",
                "%1 recordings contain where they were made.",
                n as u64,
            )
        } else {
            trnc(
                "@info the question before files that hold where they were taken are sent, %1 how many",
                "%1 file contains where it was taken.",
                "%1 files contain where they were taken.",
                n as u64,
            )
        };
        let names: Vec<String> = located
            .iter()
            .take(SHOWN_NAMES)
            .map(|f| shown_name(f))
            .collect();
        let more = if n > SHOWN_NAMES {
            trnc(
                "@info list of files, after the names shown, %1 how many more",
                "and %1 more",
                "and %1 more",
                (n - SHOWN_NAMES) as u64,
            )
        } else {
            String::new()
        };
        let still_goes = if others == 0 {
            String::new()
        } else {
            still_goes_line(n, !text.trim().is_empty())
        };
        // A screen reader hears WHICH files too (U7): the shown names, then "and %1 more". The
        // separator and the end of the list are translatable (UR6): a language sets its own
        // punctuation.
        let separator = trc(
            "@info separator between the file names a screen reader reads out",
            ", ",
        );
        let which = if more.is_empty() {
            names.join(&separator)
        } else {
            format!("{}{separator}{more}", names.join(&separator))
        };
        let which = trfc(
            "@info the end of the list of file names a screen reader reads out, %1 the list",
            "%1.",
            &[&which],
        );
        let accessible = [text_line.as_str(), &which, &still_goes]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let question = LocationQuestion {
            title,
            text: text_line,
            names,
            more,
            focus_dont_send: still_goes.is_empty(),
            still_goes,
            accessible,
            send_label: trc("@action:button", "Send with Location").into_owned(),
            hold_label: trc("@action:button", "Don't Send").into_owned(),
            remember_label: trc("@option:check", "Remember my choice").into_owned(),
            settings_hint: trc(
                "@info under the question before files with location data are sent",
                "You can change this in Settings.",
            )
            .into_owned(),
            count: n,
        };
        Some((question, self.asked(conversation, text, reply)))
    }

    /// Applies the answer to the question `asked` (from [`Self::location_question`]), with the
    /// chat now open (`conversation`) and the reply now quoted (`reply`); `remember` is the
    /// tick "Remember my choice". [Send with Location] lets the asked-about files go with
    /// their location; [Don't Send] holds them, and when no file would go then, nothing goes
    /// and the text stays. Anything the question was about that moved since makes the answer
    /// stale: nothing changes in the strip.
    pub fn answer_location(
        &mut self,
        asked: &LocationSnapshot,
        conversation: i64,
        reply: Option<i64>,
        answer: LocationAnswer,
        remember: bool,
    ) -> LocationOutcome {
        let now = self.asked(conversation, &asked.text, reply);
        if now != *asked || self.checking() {
            return LocationOutcome::Stale;
        }
        let remember = remember.then(|| answer.remembered());
        let located = asked.located();
        match answer {
            LocationAnswer::Send => {
                self.release(&located);
            }
            LocationAnswer::Hold => {
                self.hold(&located);
            }
        }
        let paths = self.sendable();
        if paths.is_empty() {
            return LocationOutcome::Nothing { remember };
        }
        let with_location = self
            .files
            .iter()
            .filter(|f| f.leaves())
            .map(|f| answer == LocationAnswer::Send && located.contains(&f.id))
            .collect();
        LocationOutcome::Send {
            send: LocationSend {
                paths,
                as_file: self.sendable_as_file(),
                with_location,
                text: asked.text.clone(),
            },
            remember,
        }
    }

    /// For each file of [`Self::sendable`]: it may leave with its location without a
    /// question. Under "Always send" every file is flagged, whatever its verdict (a verdict
    /// that turns true between the plan and the import must not bounce a file the user said
    /// always to send; F16); otherwise none is (under "Ask each time" the answer flags them,
    /// under "Never send" they are held).
    pub fn sendable_with_location(&self, pref: LocationPref) -> Vec<bool> {
        let always = pref == LocationPref::Always;
        self.files
            .iter()
            .filter(|f| f.leaves())
            .map(|_| always)
            .collect()
    }
}

/// His sentence for one file, by what it is (QH1, the audio answer, BQ4).
fn one_file_text(file: &StripFile) -> String {
    if file.archive {
        return trc(
            "@info the question before a .zip of a folder is sent",
            "This archive holds files that contain where they were taken.",
        )
        .into_owned();
    }
    match file.kind {
        AttachmentKind::Video => trc(
            "@info the question before a video that holds where it was taken is sent",
            "This video contains where it was taken.",
        ),
        AttachmentKind::Image => trc(
            "@info the question before a photo that holds where it was taken is sent",
            "This photo contains where it was taken.",
        ),
        AttachmentKind::Audio | AttachmentKind::Voice => trc(
            "@info the question before a recording that holds where it was made is sent",
            "This recording contains where it was made.",
        ),
        AttachmentKind::Gif | AttachmentKind::File => trc(
            "@info the question before a file that holds where it was taken is sent",
            "This file contains where it was taken.",
        ),
    }
    .into_owned()
}

/// What [Don't Send] still sends (F2): the other files, and the text when there is one; `n`
/// is how many files the question is about ("without it" / "without them").
fn still_goes_line(n: usize, text: bool) -> String {
    match (n == 1, text) {
        (true, true) => trc(
            "@info what Don't Send still sends: the other files and the text, without the one file asked about",
            "The other files and the text are sent without it.",
        ),
        (false, true) => trc(
            "@info what Don't Send still sends: the other files and the text, without the files asked about",
            "The other files and the text are sent without them.",
        ),
        (true, false) => trc(
            "@info what Don't Send still sends: the other files, without the one file asked about",
            "The other files are sent without it.",
        ),
        (false, false) => trc(
            "@info what Don't Send still sends: the other files, without the files asked about",
            "The other files are sent without them.",
        ),
    }
    .into_owned()
}

#[cfg(test)]
mod tests {
    use super::super::tests::{is_folder, listed, paths, plan_located};
    use super::*;
    use crate::files::{FolderCount, FolderStage, HandedFile};
    use AttachmentKind::{Audio, File, Image, Video, Voice};

    const CONV: i64 = 7;

    fn ask(strip: &FileStrip, text: &str) -> (LocationQuestion, LocationSnapshot) {
        strip
            .location_question(CONV, text, None)
            .expect("a question")
    }

    #[test]
    fn the_answer_applies_only_to_what_was_asked_about() {
        // A located video and a folder whose walk ends while the dialog is open.
        let mut strip = FileStrip::starting_at(1 << 32);
        strip.add(paths(&["trip.mp4", "Raw footage"]), false);
        let folder = is_folder(&mut strip, 1);
        plan_located(&mut strip, &[Video], &[true]);
        let (question, asked) = ask(&strip, "the beach");
        assert_eq!(question.count, 1);
        assert_eq!(asked.located(), [strip.files()[0].id]);
        // The folder becomes its files: a located photo among them (F3).
        assert!(
            strip.folder_counted(
                folder,
                FolderCount {
                    files: 2,
                    bytes: 2_000,
                    ..FolderCount::default()
                }
            ) != crate::files::Counted::Gone
        );
        assert_eq!(
            strip.files()[1].folder.as_ref().map(|f| f.stage),
            Some(FolderStage::Asking)
        );
        strip.answer_add();
        assert_eq!(strip.folder_listed(folder, listed(&["a.jpg", "b.pdf"])), 2);
        plan_located(&mut strip, &[Video, Image, File], &[true, true, false]);
        // [Send with Location] to the old question: nothing goes, the new photo is not flagged,
        // nothing is remembered.
        assert_eq!(
            strip.answer_location(&asked, CONV, None, LocationAnswer::Send, true),
            LocationOutcome::Stale
        );
        assert!(strip.files().iter().all(|f| !f.held));
        // The question comes back about both located files.
        let (again, asked) = ask(&strip, "the beach");
        assert_eq!(again.count, 2);
        assert_eq!(again.title, "Send Them Anyway?");
        let LocationOutcome::Send { send, remember } =
            strip.answer_location(&asked, CONV, None, LocationAnswer::Send, false)
        else {
            panic!("sent");
        };
        assert_eq!(remember, None);
        assert_eq!(send.paths.len(), 3);
        assert_eq!(send.with_location, [true, true, false]);
        assert_eq!(send.text, "the beach");
    }

    #[test]
    fn a_moved_verdict_or_another_chat_or_reply_asks_again_but_a_caption_does_not() {
        let mut strip = FileStrip::new();
        strip.add(paths(&["trip.mp4", "notes.pdf"]), false);
        plan_located(&mut strip, &[Video, File], &[true, false]);
        // The press carried a long caption; the strip is told of it 300 ms later (BQ-R1):
        // the serial moves, the verdicts stay, and the answer still holds with the press's text.
        let long = "x".repeat(600);
        let (_, asked) = ask(&strip, &long);
        assert!(strip.set_caption(true, Some(long.clone()), false));
        let LocationOutcome::Send { send, .. } =
            strip.answer_location(&asked, CONV, None, LocationAnswer::Send, false)
        else {
            panic!("a late caption is no change of what was asked");
        };
        assert_eq!(send.text, long);
        assert_eq!(send.with_location, [true, false]);
        // Another chat, another reply: stale.
        assert_eq!(
            strip.answer_location(&asked, CONV + 1, None, LocationAnswer::Send, false),
            LocationOutcome::Stale
        );
        assert_eq!(
            strip.answer_location(&asked, CONV, Some(3), LocationAnswer::Send, false),
            LocationOutcome::Stale
        );
        // A verdict that changed: stale.
        plan_located(&mut strip, &[Video, File], &[true, true]);
        assert_eq!(
            strip.answer_location(&asked, CONV, None, LocationAnswer::Hold, false),
            LocationOutcome::Stale
        );
        // `as_file` toggled (a new verdict pending): no question, the answer is stale.
        let (_, asked) = ask(&strip, "");
        assert!(strip.set_as_file(0, true));
        assert!(strip.location_question(CONV, "", None).is_none());
        assert_eq!(
            strip.answer_location(&asked, CONV, None, LocationAnswer::Send, false),
            LocationOutcome::Stale
        );
    }

    #[test]
    fn dont_send_with_a_lone_located_file_sends_nothing_and_keeps_the_text() {
        let mut strip = FileStrip::new();
        strip.add(paths(&["beach.jpg"]), false);
        plan_located(&mut strip, &[Image], &[true]);
        let (question, asked) = ask(&strip, "for you");
        // [Don't Send] sends nothing: it has the focus, no line of what still goes (F2).
        assert!(question.focus_dont_send);
        assert!(question.still_goes.is_empty());
        assert_eq!(
            strip.answer_location(&asked, CONV, None, LocationAnswer::Hold, true),
            LocationOutcome::Nothing {
                remember: Some(LocationPref::Never)
            }
        );
        // The file stays, held; the next Send asks again.
        assert!(strip.files()[0].held);
        assert!(strip.sendable().is_empty());
        assert!(strip.location_question(CONV, "for you", None).is_some());
    }

    #[test]
    fn dont_send_with_other_files_sends_them_and_the_text_and_holds_the_located() {
        let mut strip = FileStrip::new();
        strip.add(paths(&["trip.mp4", "notes.pdf", "voice.m4a"]), false);
        plan_located(&mut strip, &[Video, File, Audio], &[true, false, true]);
        let (question, asked) = ask(&strip, "see");
        assert!(!question.focus_dont_send, "Enter must not send (F2)");
        assert_eq!(
            question.still_goes,
            "The other files and the text are sent without them."
        );
        assert_eq!(
            question.accessible,
            "2 files contain where they were taken. trip.mp4, voice.m4a. The other files and the text are sent without them."
        );
        let LocationOutcome::Send { send, remember } =
            strip.answer_location(&asked, CONV, None, LocationAnswer::Hold, false)
        else {
            panic!("the others go");
        };
        assert_eq!(remember, None);
        assert_eq!(send.paths, paths(&["notes.pdf"]));
        assert_eq!(send.with_location, [false]);
        assert_eq!(send.text, "see");
        assert_eq!(strip.take_sendable().len(), 1);
        assert_eq!(strip.len(), 2);
        assert!(strip.files().iter().all(|f| f.held));
    }

    #[test]
    fn remember_gives_the_setting_of_the_answer() {
        let mut strip = FileStrip::new();
        strip.add(paths(&["trip.mp4"]), false);
        plan_located(&mut strip, &[Video], &[true]);
        let (_, asked) = ask(&strip, "");
        let outcome = strip.answer_location(&asked, CONV, None, LocationAnswer::Send, true);
        assert_eq!(outcome.remember(), Some(LocationPref::Always));
        let LocationOutcome::Send { send, .. } = outcome else {
            panic!("sent");
        };
        assert_eq!(send.with_location, [true]);
        assert_eq!(LocationAnswer::Hold.remembered(), LocationPref::Never);
    }

    #[test]
    fn the_question_names_the_kind_and_counts_the_rest() {
        let one = |name: &str, kind: AttachmentKind| {
            let mut strip = FileStrip::new();
            strip.add(paths(&[name]), false);
            plan_located(&mut strip, &[kind], &[true]);
            ask(&strip, "").0
        };
        let video = one("trip.mp4", Video);
        assert_eq!(video.title, "Send It Anyway?");
        assert_eq!(video.text, "This video contains where it was taken.");
        assert_eq!(video.names, ["trip.mp4"]);
        assert_eq!(video.more, "");
        assert_eq!(video.send_label, "Send with Location");
        assert_eq!(video.hold_label, "Don't Send");
        assert_eq!(video.remember_label, "Remember my choice");
        assert_eq!(video.settings_hint, "You can change this in Settings.");
        // (U7: the names are in what a screen reader hears.)
        assert_eq!(
            video.accessible,
            "This video contains where it was taken. trip.mp4."
        );
        assert_eq!(
            one("a.jpg", Image).text,
            "This photo contains where it was taken."
        );
        assert_eq!(
            one("a.m4a", Audio).text,
            "This recording contains where it was made."
        );
        assert_eq!(
            one("a.dng", File).text,
            "This file contains where it was taken."
        );
        // Seven located files: five names, "and 2 more".
        let mut strip = FileStrip::new();
        let names: Vec<String> = (0..7).map(|i| format!("p{i}.jpg")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        strip.add(paths(&names), false);
        plan_located(&mut strip, &[Image; 7], &[true; 7]);
        let many = ask(&strip, "").0;
        assert_eq!(many.text, "7 files contain where they were taken.");
        assert_eq!(many.names.len(), SHOWN_NAMES);
        assert_eq!(many.more, "and 2 more");
        assert_eq!(
            many.accessible,
            "7 files contain where they were taken. p0.jpg, p1.jpg, p2.jpg, p3.jpg, p4.jpg, and 2 more."
        );
        // Several recordings are "made", not "taken" (U8); a recording among other kinds keeps
        // the generic line.
        let kinds = |kinds: &[AttachmentKind]| {
            let mut strip = FileStrip::new();
            let names: Vec<String> = (0..kinds.len()).map(|i| format!("r{i}.m4a")).collect();
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            strip.add(paths(&names), false);
            plan_located(&mut strip, kinds, &[true; 4][..kinds.len()]);
            ask(&strip, "").0.text
        };
        assert_eq!(
            kinds(&[Audio, Audio]),
            "2 recordings contain where they were made."
        );
        assert_eq!(
            kinds(&[Audio, Voice, Audio]),
            "3 recordings contain where they were made."
        );
        assert_eq!(
            kinds(&[Audio, Video]),
            "2 files contain where they were taken."
        );
        assert_eq!(
            kinds(&[Audio, File]),
            "2 files contain where they were taken."
        );
        // One file and others, no text: the line without the text.
        let mut strip = FileStrip::new();
        strip.add(paths(&["trip.mp4", "notes.pdf"]), false);
        plan_located(&mut strip, &[Video, File], &[true, false]);
        assert_eq!(
            ask(&strip, " ").0.still_goes,
            "The other files are sent without it."
        );
        // Nothing located, or a verdict pending: no question.
        let mut strip = FileStrip::new();
        strip.add(paths(&["notes.pdf"]), false);
        assert!(strip.location_question(CONV, "", None).is_none());
        plan_located(&mut strip, &[File], &[false]);
        assert!(strip.location_question(CONV, "", None).is_none());
    }

    /// UR6: a language sets the punctuation of the list a screen reader hears.
    #[test]
    fn the_names_for_a_screen_reader_take_their_separator_and_end_from_the_catalog() {
        struct Punctuation;
        impl crate::i18n::Translator for Punctuation {
            fn translate(&self, _context: &str, msgid: &str) -> String {
                match msgid {
                    ", " => " \u{b7} ".to_owned(),
                    "%1." => "%1;".to_owned(),
                    other => other.to_owned(),
                }
            }
            fn translate_plural(&self, _c: &str, s: &str, p: &str, n: u64) -> String {
                (if n == 1 { s } else { p }).to_owned()
            }
        }
        let mut strip = FileStrip::new();
        strip.add(
            paths(&["a.mp4", "b.mp4", "c.mp4", "d.mp4", "e.mp4", "f.mp4"]),
            false,
        );
        plan_located(&mut strip, &[Video; 6], &[true; 6]);
        let accessible = crate::i18n::with_translator(Punctuation, || ask(&strip, "").0.accessible);
        assert_eq!(
            accessible,
            "6 files contain where they were taken. \
             a.mp4 \u{b7} b.mp4 \u{b7} c.mp4 \u{b7} d.mp4 \u{b7} e.mp4 \u{b7} and 1 more;"
        );
        // Without a catalog it is the English list, as before.
        assert_eq!(
            ask(&strip, "").0.accessible,
            "6 files contain where they were taken. a.mp4, b.mp4, c.mp4, d.mp4, e.mp4, and 1 more."
        );
    }

    #[test]
    fn an_archive_of_a_folder_is_asked_about_as_one() {
        let mut strip = FileStrip::new();
        strip.add(paths(&["Raw footage"]), false);
        let folder = is_folder(&mut strip, 0);
        strip.folder_counted(
            folder,
            FolderCount {
                files: 2,
                bytes: 2_000,
                ..FolderCount::default()
            },
        );
        strip.answer_zip();
        // The packer found a located file inside (BQ4); the core cannot look into the archive
        // and says false: the archive's verdict is true all the same, after every plan.
        assert!(strip.folder_packed(folder, PathBuf::from("/t/Raw footage.zip"), 2_000, true));
        assert_eq!(strip.files()[0].location, None, "Send waits for the plan");
        plan_located(&mut strip, &[File], &[false]);
        assert_eq!(strip.files()[0].location, Some(true));
        assert_eq!(strip.located(), [strip.files()[0].id]);
        let question = ask(&strip, "").0;
        assert_eq!(
            question.text,
            "This archive holds files that contain where they were taken."
        );
        assert_eq!(question.names, ["Raw footage.zip"]);
    }

    /// A folder's archive with a located file inside, sent and handed back (P1).
    fn archive_sent_once() -> StripFile {
        let mut strip = FileStrip::new();
        strip.add(paths(&["Raw footage"]), false);
        let folder = is_folder(&mut strip, 0);
        strip.folder_counted(
            folder,
            FolderCount {
                files: 2,
                bytes: 2_000,
                ..FolderCount::default()
            },
        );
        strip.answer_zip();
        assert!(strip.folder_packed(folder, PathBuf::from("/t/Raw footage.zip"), 2_000, true));
        plan_located(&mut strip, &[File], &[false]);
        let mut sent = strip.take_sendable();
        assert_eq!(sent.len(), 1);
        sent.remove(0)
    }

    #[test]
    fn a_returned_archive_still_says_what_is_inside_it() {
        // The send was refused as a whole (the quoted message is gone) and the archive
        // comes back as a plain path: the next plan's verdict is true, Send asks, and
        // "Never send" holds it (P1; the core cannot look inside a zip).
        let zip = archive_sent_once();
        assert_eq!(zip.archive_made(), Some(true));
        let handed = || HandedFile {
            path: zip.path.clone(),
            pasted: zip.pasted,
            as_file: zip.as_file,
            archive: zip.archive_made(),
        };
        for how in ["add_handed", "restore_refused"] {
            let mut back = FileStrip::new();
            if how == "add_handed" {
                assert_eq!(back.add_handed([handed()]), 1);
            } else {
                // (Marked for another reason than its location: the mark goes with the plan.)
                back.restore_refused(vec![(
                    handed(),
                    kourier_core::facade::FileRefusal::Unreadable,
                )]);
            }
            plan_located(&mut back, &[File], &[false]);
            assert_eq!(back.files()[0].location, Some(true), "{how}");
            assert!(back.files()[0].archive, "{how}");
            let (question, _) = ask(&back, "");
            assert_eq!(
                question.text, "This archive holds files that contain where they were taken.",
                "{how}"
            );
            // Under Never it is held, not sent.
            assert_eq!(back.hold(&back.located()), 1, "{how}");
            assert!(back.sendable().is_empty(), "{how}");
        }
        // An archive without a located file inside, and a plain file, return as they were.
        let mut plain = FileStrip::new();
        plain.add_handed([HandedFile {
            path: PathBuf::from("/t/Other.zip"),
            pasted: true,
            as_file: false,
            archive: Some(false),
        }]);
        plan_located(&mut plain, &[File], &[false]);
        assert_eq!(plain.files()[0].location, Some(false));
        assert!(plain.files()[0].archive);
        assert!(plain.location_question(CONV, "", None).is_none());
    }

    #[test]
    fn always_flags_every_file_that_goes() {
        let mut strip = FileStrip::new();
        strip.add(paths(&["trip.mp4", "notes.pdf", "gone.bin"]), false);
        plan_located(&mut strip, &[Video, File, File], &[true, false, false]);
        // A refused file is not sent and not flagged.
        strip.files[2].refusal = Some(kourier_core::facade::FileRefusal::Unreadable);
        assert_eq!(
            strip.sendable_with_location(LocationPref::Always),
            [true, true]
        );
        assert_eq!(
            strip.sendable_with_location(LocationPref::Ask),
            [false, false]
        );
        strip.hold(&strip.located());
        assert_eq!(strip.sendable_with_location(LocationPref::Never), [false]);
        assert_eq!(strip.sendable_with_location(LocationPref::Always), [true]);
    }

    #[test]
    fn debug_shows_no_names_paths_or_text() {
        let mut strip = FileStrip::new();
        strip.add(paths(&["secretname.mp4"]), false);
        plan_located(&mut strip, &[Video], &[true]);
        let (question, asked) = ask(&strip, "secrettext");
        let LocationOutcome::Send { send, .. } =
            strip.answer_location(&asked, CONV, None, LocationAnswer::Send, false)
        else {
            panic!("sent");
        };
        let all = format!("{question:?} {asked:?} {send:?}");
        assert!(!all.contains("secret"), "{all}");
    }
}
