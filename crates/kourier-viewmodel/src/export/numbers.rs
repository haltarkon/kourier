//! The dialog's choices ([`Ticks`]) and the numbers it shows ([`numbers`]).

use kourier_core::facade::{ExportFileRefusal, ExportPlan, ExportVerdict, LeftOut, PlannedFile};
use kourier_model::{AttachmentKind, Body};

/// The dialog's ticks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ticks {
    /// Pictures (images, stickers, animated pictures) go into the archive.
    pub pictures: bool,
    /// Videos go into the archive.
    pub videos: bool,
    /// Every other file goes into the archive.
    pub other_files: bool,
    /// Disappearing messages are exported too (the copy does not disappear).
    pub disappearing: bool,
}

/// The three kinds of file the dialog offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileClass {
    /// Images, stickers and animated pictures.
    Pictures,
    /// Videos.
    Videos,
    /// Everything else.
    Other,
}

impl Ticks {
    /// Whether files of `class` are ticked.
    pub fn has(&self, class: FileClass) -> bool {
        match class {
            FileClass::Pictures => self.pictures,
            FileClass::Videos => self.videos,
            FileClass::Other => self.other_files,
        }
    }
}

/// The class of a file of a message with `body`.
pub fn class_of(body: &Body, kind: AttachmentKind) -> FileClass {
    if matches!(body, Body::Sticker(_)) {
        return FileClass::Pictures;
    }
    match kind {
        AttachmentKind::Image | AttachmentKind::Gif => FileClass::Pictures,
        AttachmentKind::Video => FileClass::Videos,
        AttachmentKind::Audio | AttachmentKind::Voice | AttachmentKind::File => FileClass::Other,
    }
}

/// Count and total size of one class of file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClassCount {
    /// How many files.
    pub count: u32,
    /// Their sizes together, in bytes.
    pub bytes: u64,
}

impl ClassCount {
    fn add(&mut self, f: &PlannedFile) {
        self.count += 1;
        self.bytes = self.bytes.saturating_add(f.size);
    }
}

/// What the dialog shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DialogNumbers {
    /// Messages that go in whatever the ticks are.
    pub included: u32,
    /// Disappearing messages: in only with the tick.
    pub disappearing: u32,
    /// Left out: view-once messages (never saved).
    pub left_out_view_once: u32,
    /// Left out: messages gone for the user.
    pub left_out_gone: u32,
    /// Files that can go in, per class, of the messages that go in whatever the ticks are.
    pub pictures: ClassCount,
    /// See [`Self::pictures`].
    pub videos: ClassCount,
    /// See [`Self::pictures`].
    pub other_files: ClassCount,
    /// Files of the disappearing messages, per class (they go in with the disappearing tick AND
    /// the class's own tick).
    pub disappearing_pictures: ClassCount,
    /// See [`Self::disappearing_pictures`].
    pub disappearing_videos: ClassCount,
    /// See [`Self::disappearing_pictures`].
    pub disappearing_other: ClassCount,
    /// Files that are not on this computer (listed, never fetched), of the messages that go in
    /// whatever the ticks are.
    pub not_here: u32,
    /// Files not on this computer of the disappearing messages: the dialog adds them to
    /// `not_here` only with the tick.
    pub not_here_disappearing: u32,
}

impl DialogNumbers {
    /// Whether the selection holds a disappearing message.
    pub fn has_disappearing(&self) -> bool {
        self.disappearing > 0
    }

    /// How many files the disappearing messages hold in all (every class).
    pub fn disappearing_files(&self) -> u32 {
        self.disappearing_pictures.count
            + self.disappearing_videos.count
            + self.disappearing_other.count
    }

    /// What goes into the archive with these ticks: the one place the dialog's numbers per tick
    /// come from (the dialog only words them). Equals what [`super::build`] carries for the
    /// same ticks.
    pub fn shown(&self, ticks: Ticks) -> ShownNumbers {
        let with = ticks.disappearing;
        // What a class holds with the disappearing tick (its own tick aside): the kind's line.
        let offer = |base: ClassCount, timed: ClassCount| {
            if with {
                ClassCount {
                    count: base.count + timed.count,
                    bytes: base.bytes.saturating_add(timed.bytes),
                }
            } else {
                base
            }
        };
        let offered_pictures = offer(self.pictures, self.disappearing_pictures);
        let offered_videos = offer(self.videos, self.disappearing_videos);
        let offered_other = offer(self.other_files, self.disappearing_other);
        let goes = |class: FileClass, c: ClassCount| {
            if ticks.has(class) {
                c
            } else {
                ClassCount::default()
            }
        };
        let pictures = goes(FileClass::Pictures, offered_pictures);
        let videos = goes(FileClass::Videos, offered_videos);
        let other_files = goes(FileClass::Other, offered_other);
        ShownNumbers {
            messages: self.included + if with { self.disappearing } else { 0 },
            pictures,
            videos,
            other_files,
            offered_pictures,
            offered_videos,
            offered_other,
            total_bytes: pictures
                .bytes
                .saturating_add(videos.bytes)
                .saturating_add(other_files.bytes),
            not_here: self.not_here + if with { self.not_here_disappearing } else { 0 },
        }
    }

    /// Messages left out, all reasons.
    pub fn left_out(&self) -> u32 {
        self.left_out_view_once + self.left_out_gone
    }
}

/// What the dialog shows for one set of ticks ([`DialogNumbers::shown`]): `pictures`, `videos`,
/// `other_files` and `total_bytes` are what goes into the archive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShownNumbers {
    /// Messages that go into the archive.
    pub messages: u32,
    /// Pictures that go in.
    pub pictures: ClassCount,
    /// Videos that go in.
    pub videos: ClassCount,
    /// Other files that go in.
    pub other_files: ClassCount,
    /// What each class holds with the disappearing tick, whatever its own tick says (the kind's
    /// line in the dialog: unticking Pictures does not zero its own label).
    pub offered_pictures: ClassCount,
    /// See [`Self::offered_pictures`].
    pub offered_videos: ClassCount,
    /// See [`Self::offered_pictures`].
    pub offered_other: ClassCount,
    /// The size of every file that goes in, in bytes.
    pub total_bytes: u64,
    /// Files listed without their file (not on this computer), of the messages that go in.
    pub not_here: u32,
}

/// The dialog's numbers for a plan. Cheap: one pass over the plan.
pub fn numbers(plan: &ExportPlan) -> DialogNumbers {
    let mut n = DialogNumbers::default();
    if plan.chat.is_err() {
        return n;
    }
    for pm in &plan.messages {
        let carried = match pm.verdict {
            ExportVerdict::Included => {
                n.included += 1;
                true
            }
            ExportVerdict::NeedsTick => {
                n.disappearing += 1;
                false
            }
            ExportVerdict::LeftOut(LeftOut::ViewOnce) => {
                n.left_out_view_once += 1;
                continue;
            }
            ExportVerdict::LeftOut(LeftOut::Gone) => {
                n.left_out_gone += 1;
                continue;
            }
        };
        // A deleted row carries no file (the one rule: `record::file_goes`).
        if pm.row.deleted.is_some() {
            continue;
        }
        for f in &pm.files {
            match f.accepted {
                Err(ExportFileRefusal::NotHere) if carried => n.not_here += 1,
                Err(ExportFileRefusal::NotHere) => n.not_here_disappearing += 1,
                Err(_) => {}
                Ok(()) => {
                    let (base, timed) = match class_of(&pm.row.body, f.kind) {
                        FileClass::Pictures => (&mut n.pictures, &mut n.disappearing_pictures),
                        FileClass::Videos => (&mut n.videos, &mut n.disappearing_videos),
                        FileClass::Other => (&mut n.other_files, &mut n.disappearing_other),
                    };
                    if carried { base.add(f) } else { timed.add(f) }
                }
            }
        }
    }
    n
}
