//! The gallery of a chat for the details panel (people pages P4; architecture §8.3): the rows of
//! "Media and Files". Qt-free: the core states the attachments (`Query::ChatMedia`), this module
//! makes of each one the row the front end draws (a square of "Media", a row of "Files") with the
//! same blocks the timeline builds ([`crate::timeline::attachment_block`]: the thumbnail's URL,
//! the blurhash, the download state, the size label, the file's icon), so a picture here and in
//! the chat are one picture.

use kourier_core::facade::{ChatMediaItem, MediaCursor, MediaKind};
use kourier_model::{AttachmentId, AttachmentKind, MessageId};

use crate::diff::RoleSet;
use crate::timeline::{Block, MediaState, attachment_block};

/// How many squares a page's section shows.
pub const SECTION_TILES: usize = 6;
/// What a section asks the core for: one more than it shows, so that one more says "more".
pub const SECTION_ASK: u32 = 7;
/// How many attachments a page of the "All Media and Files" level asks for (a page size, not a
/// limit: "more" asks the next page, to the end of the chat).
pub const PAGE: u32 = 48;
/// What the picture viewer's own query asks for: the attachments from its message on (the
/// message's own are what it keeps; an album has at most this many pictures).
pub const VIEWER_ASK: u32 = 32;

/// One of the five places the gallery is read at a time. The group page and the person page
/// each keep their section alive (hidden) while a page lies on top of them, and the level of
/// "All Media and Files" has a grid and a list of its own, so each is its own slot with its own
/// chat, rows and queries. The fifth is the picture viewer's: it reads the whole album of the
/// message the viewer shows, AT that message (a section holds six squares and a level only the
/// pages it loaded, neither is the message's album), and follows that message, not a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MediaSlot {
    /// The section of the group's page.
    GroupSection,
    /// The section of the person's page.
    PersonSection,
    /// The grid of "Media" on the level of all media.
    Media,
    /// The list of "Files" on the level of all media.
    Files,
    /// The picture viewer's own: the pictures of one message, read from that message
    /// ([`VIEWER_ASK`]; opened with `UiSource::open_media_viewer`, never by `open_chat_media`).
    Viewer,
}

impl MediaSlot {
    /// Every slot, in the order of [`Self::index`].
    pub const ALL: [Self; Self::COUNT] = [
        Self::GroupSection,
        Self::PersonSection,
        Self::Media,
        Self::Files,
        Self::Viewer,
    ];

    /// How many slots there are.
    pub const COUNT: usize = 5;

    /// The slot's place in a table of [`Self::COUNT`].
    pub const fn index(self) -> usize {
        match self {
            Self::GroupSection => 0,
            Self::PersonSection => 1,
            Self::Media => 2,
            Self::Files => 3,
            Self::Viewer => 4,
        }
    }

    /// The kind of attachment the slot lists.
    pub const fn kind(self) -> MediaKind {
        match self {
            Self::GroupSection | Self::PersonSection | Self::Media | Self::Viewer => {
                MediaKind::Visual
            }
            Self::Files => MediaKind::Files,
        }
    }

    /// The slots of the page's own places (the sections and the level): what the panel closes
    /// with itself. The viewer's slot lives as long as the viewer.
    pub const fn is_page_slot(self) -> bool {
        !matches!(self, Self::Viewer)
    }

    /// The level's grid and list go on with the next page ("more"); a section and the viewer's
    /// read never do.
    pub const fn goes_on(self) -> bool {
        matches!(self, Self::Media | Self::Files)
    }

    /// A section shows [`SECTION_TILES`] squares and never more.
    pub const fn is_section(self) -> bool {
        matches!(self, Self::GroupSection | Self::PersonSection)
    }

    /// How many attachments the slot asks for at its start.
    pub const fn first_page(self) -> u32 {
        match self {
            Self::GroupSection | Self::PersonSection => SECTION_ASK,
            Self::Media | Self::Files => PAGE,
            Self::Viewer => VIEWER_ASK,
        }
    }

    /// The name QML uses (`group`, `person`, `media`, `files`, `viewer`).
    pub const fn name(self) -> &'static str {
        match self {
            Self::GroupSection => "group",
            Self::PersonSection => "person",
            Self::Media => "media",
            Self::Files => "files",
            Self::Viewer => "viewer",
        }
    }

    /// The slot QML named.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }
}

/// One attachment of a chat's gallery as the front end draws it. Flat: a square of "Media" uses
/// the picture's fields, a row of "Files" the file's. `Debug` shows ids, the kind and the state
/// only (never a name).
#[derive(Clone, PartialEq)]
pub struct MediaRow {
    /// The attachment (the row's identity in a list model).
    pub attachment: AttachmentId,
    /// Its message (what the viewer follows, the cursor).
    pub message: MessageId,
    /// The message's sort key (the cursor; not drawn).
    pub sort_key: i64,
    /// Its place in the message (the cursor; not drawn).
    pub idx: u32,
    /// What it is (a "GIF" that is a video clip is a video, as in the timeline).
    pub kind: AttachmentKind,
    /// The thumbnail's URL (`image://kourier/thumb/…`); empty until downloaded and for a clip.
    pub url: String,
    /// The viewer's still (`image://kourier/att/…`); empty until downloaded, for a clip and for
    /// a GIF that plays from its copy.
    pub full_url: String,
    /// The blurhash placeholder's URL; empty without one.
    pub placeholder_url: String,
    /// Plays from a copy (a GIF).
    pub animated: bool,
    /// A well-known picture, sound or video type: opened without a question.
    pub media: bool,
    /// Pixel width (0 = unknown).
    pub width: u32,
    /// Pixel height (0 = unknown).
    pub height: u32,
    /// Download state.
    pub state: MediaState,
    /// Download progress, 0 … 1.
    pub progress: f32,
    /// A picture's size or a clip's duration ("1.2 MB", "0:42").
    pub label: String,
    /// A file's name.
    pub name: String,
    /// A file's "2.4 MB · PDF".
    pub detail: String,
    /// A file's Breeze icon.
    pub icon: String,
    /// "Save As…" accepts it (the core's answer, `ChatMediaItem::save`).
    pub save: bool,
}

crate::safe_debug!(MediaRow {
    attachment,
    message,
    kind,
    state
});

/// The role indices of a media row in a list model (`attachment` is role 0, the key).
pub mod media_role {
    /// The attachment's id (the row's key).
    pub const ATTACHMENT: u32 = 0;
    /// The message's id.
    pub const MESSAGE: u32 = 1;
    /// The kind's name.
    pub const KIND: u32 = 2;
    /// The thumbnail URL.
    pub const URL: u32 = 3;
    /// The viewer's still.
    pub const FULL_URL: u32 = 4;
    /// The blurhash placeholder.
    pub const PLACEHOLDER_URL: u32 = 5;
    /// Plays from a copy.
    pub const ANIMATED: u32 = 6;
    /// A well-known type.
    pub const MEDIA: u32 = 7;
    /// Pixel width.
    pub const WIDTH: u32 = 8;
    /// Pixel height.
    pub const HEIGHT: u32 = 9;
    /// The download state's name.
    pub const STATE: u32 = 10;
    /// Download progress.
    pub const PROGRESS: u32 = 11;
    /// Size or duration.
    pub const LABEL: u32 = 12;
    /// A file's name.
    pub const NAME: u32 = 13;
    /// A file's detail line.
    pub const DETAIL: u32 = 14;
    /// A file's icon.
    pub const ICON: u32 = 15;
    /// "Save As…" accepts it.
    pub const SAVE: u32 = 16;
}

impl MediaRow {
    /// The row of one attachment of the gallery; `None` for one the core listed without an id or
    /// that is not what the slot's kind draws (never: the statement filters by kind).
    pub fn new(item: &ChatMediaItem, kind: MediaKind) -> Option<Self> {
        let id = item.attachment.id?;
        let mut row = Self {
            attachment: id,
            message: item.message,
            sort_key: item.sort_key,
            idx: item.idx,
            kind: item.attachment.kind,
            url: String::new(),
            full_url: String::new(),
            placeholder_url: String::new(),
            animated: false,
            media: false,
            width: 0,
            height: 0,
            state: MediaState::Remote,
            progress: 0.0,
            label: String::new(),
            name: String::new(),
            detail: String::new(),
            icon: String::new(),
            save: item.save,
        };
        match (kind, attachment_block(&item.attachment, false)) {
            (MediaKind::Visual, Block::Image(i)) => {
                row.kind = i.kind;
                row.url = i.url;
                row.full_url = i.full_url;
                row.placeholder_url = i.placeholder_url;
                row.animated = i.animated;
                row.media = i.media;
                row.width = i.width;
                row.height = i.height;
                row.state = i.state;
                row.progress = i.progress;
                row.label = i.label;
            }
            (MediaKind::Files, Block::File(f)) => {
                row.media = f.media;
                row.state = f.state;
                row.progress = f.progress;
                row.name = f.name;
                row.detail = f.detail;
                row.icon = f.icon;
            }
            _ => return None,
        }
        Some(row)
    }

    /// The rows of a page's items, in the page's order.
    pub fn rows(items: &[ChatMediaItem], kind: MediaKind) -> Vec<Self> {
        items.iter().filter_map(|i| Self::new(i, kind)).collect()
    }

    /// The row's identity in a list model: the attachment.
    pub fn key(&self) -> AttachmentId {
        self.attachment
    }

    /// Where a page that goes on after this row starts.
    pub fn cursor(&self) -> MediaCursor {
        MediaCursor {
            sort_key: self.sort_key,
            message: self.message,
            idx: Some(self.idx),
        }
    }

    /// Which roles differ between two versions of the same attachment's row.
    pub fn changed_roles(a: &Self, b: &Self) -> RoleSet {
        use media_role::*;
        RoleSet::EMPTY
            .with_if(MESSAGE, a.message != b.message)
            .with_if(KIND, a.kind != b.kind)
            .with_if(URL, a.url != b.url)
            .with_if(FULL_URL, a.full_url != b.full_url)
            .with_if(PLACEHOLDER_URL, a.placeholder_url != b.placeholder_url)
            .with_if(ANIMATED, a.animated != b.animated)
            .with_if(MEDIA, a.media != b.media)
            .with_if(WIDTH, a.width != b.width)
            .with_if(HEIGHT, a.height != b.height)
            .with_if(STATE, a.state != b.state)
            .with_if(PROGRESS, a.progress != b.progress)
            .with_if(LABEL, a.label != b.label)
            .with_if(NAME, a.name != b.name)
            .with_if(DETAIL, a.detail != b.detail)
            .with_if(ICON, a.icon != b.icon)
            .with_if(SAVE, a.save != b.save)
    }
}

/// What a slot tells the front end ([`crate::source::UiSource::take_chat_media`]): the rows it
/// lists now, and the two facts a page decides by.
#[derive(Clone, Debug, PartialEq)]
pub struct MediaUpdate {
    /// Which slot.
    pub slot: MediaSlot,
    /// The rows: a section's first [`SECTION_TILES`], else every row loaded so far. Empty when
    /// the chat has none of the kind (or the slot was closed or its chat is gone).
    pub rows: Vec<MediaRow>,
    /// More of the kind follow the last row: a section's "Show All", a level's next page.
    pub more: bool,
    /// The other kind has something (a chat with files and no pictures still has a section).
    pub other: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_slots_are_five_and_named_for_qml() {
        assert_eq!(MediaSlot::ALL.len(), 5);
        assert_eq!(MediaSlot::ALL.len(), MediaSlot::COUNT);
        for (i, s) in MediaSlot::ALL.into_iter().enumerate() {
            assert_eq!(s.index(), i);
            assert_eq!(MediaSlot::from_name(s.name()), Some(s));
        }
        assert_eq!(MediaSlot::from_name("all"), None);
        assert!(MediaSlot::GroupSection.is_section() && MediaSlot::PersonSection.is_section());
        assert!(!MediaSlot::Media.is_section() && !MediaSlot::Files.is_section());
        assert_eq!(MediaSlot::Files.kind(), MediaKind::Files);
        assert_eq!(MediaSlot::Media.kind(), MediaKind::Visual);
        assert_eq!(MediaSlot::PersonSection.first_page(), SECTION_ASK);
        assert_eq!(MediaSlot::Files.first_page(), PAGE);
        assert_eq!(MediaSlot::Viewer.first_page(), VIEWER_ASK);
        assert_eq!(MediaSlot::Viewer.kind(), MediaKind::Visual);
        // The panel closes the pages' places with itself, not the viewer's.
        assert!(
            MediaSlot::ALL
                .iter()
                .filter(|s| !s.is_page_slot())
                .eq([&MediaSlot::Viewer])
        );
        // Only the level's grid and list go on with a next page.
        assert!(
            MediaSlot::ALL
                .iter()
                .filter(|s| s.goes_on())
                .eq([&MediaSlot::Media, &MediaSlot::Files])
        );
        assert_eq!(SECTION_ASK as usize, SECTION_TILES + 1);
    }
}
