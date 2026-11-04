//! The td_api objects inside a `MessageContent` that the content map reads (TG-3 C4a; the pinned
//! 1.8.69 td_api; the `td_api.tl:303-712`, `:5454-6020` cites are the 1.8.68 file's, every object is unchanged). Each is read from the content's raw JSON with
//! `T::deserialize(&Value)` (no copy). As in `tdapi`: every field has `#[serde(default)]`, nothing
//! denies unknown fields, every free text is a [`Text`] (its `Debug` prints nothing), so the derived
//! `Debug`s print numbers and kinds only.

use serde::Deserialize;

use crate::tdapi::{File, FormattedText, Text};

/// `photoSize` (`td_api.tl:340`).
#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct PhotoSize {
    #[serde(default)]
    pub(super) photo: File,
    #[serde(default)]
    pub(super) width: i32,
    #[serde(default)]
    pub(super) height: i32,
}

/// `photo` (`td_api.tl:581`).
#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct Photo {
    #[serde(default)]
    pub(super) sizes: Vec<PhotoSize>,
}

/// `messagePhoto` (`td_api.tl:5487`); the live photo's `video` is not read.
#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct MessagePhoto {
    #[serde(default)]
    pub(super) photo: Photo,
    #[serde(default)]
    pub(super) caption: FormattedText,
}

/// `animation`, `audio`, `document`, `video`, `videoNote`, `voiceNote` (`td_api.tl:552-624`) read
/// as one shape: the fields each has, the rest stay 0 or empty. The file is under the kind's own key.
#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct Media {
    #[serde(default)]
    pub(super) duration: i32,
    #[serde(default)]
    pub(super) width: i32,
    #[serde(default)]
    pub(super) height: i32,
    /// A video note's side (it is square).
    #[serde(default)]
    pub(super) length: i32,
    #[serde(default)]
    pub(super) file_name: Text,
    #[serde(default)]
    pub(super) mime_type: Text,
    #[serde(default)]
    pub(super) animation: Option<File>,
    #[serde(default)]
    pub(super) audio: Option<File>,
    #[serde(default)]
    pub(super) document: Option<File>,
    #[serde(default)]
    pub(super) video: Option<File>,
    #[serde(default)]
    pub(super) voice: Option<File>,
}

impl Media {
    /// The file, whichever key the kind keeps it under.
    pub(super) fn file(&self) -> Option<&File> {
        self.animation
            .as_ref()
            .or(self.audio.as_ref())
            .or(self.document.as_ref())
            .or(self.video.as_ref())
            .or(self.voice.as_ref())
    }
}

/// `messageAnimation`, `messageAudio`, `messageDocument`, `messageVideo`, `messageVideoNote`,
/// `messageVoiceNote` (`td_api.tl:5465-5508`): the media under the kind's key, the caption.
#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct MessageMedia {
    #[serde(default)]
    pub(super) animation: Option<Media>,
    #[serde(default)]
    pub(super) audio: Option<Media>,
    #[serde(default)]
    pub(super) document: Option<Media>,
    #[serde(default)]
    pub(super) video: Option<Media>,
    #[serde(default)]
    pub(super) video_note: Option<Media>,
    #[serde(default)]
    pub(super) voice_note: Option<Media>,
    #[serde(default)]
    pub(super) caption: FormattedText,
}

/// `StickerFormat` (`td_api.tl:` `stickerFormatWebp|Tgs|Webm`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(tag = "@type")]
pub(super) enum StickerFormat {
    #[serde(rename = "stickerFormatWebp")]
    Webp,
    #[serde(rename = "stickerFormatTgs")]
    Tgs,
    #[serde(rename = "stickerFormatWebm")]
    Webm,
    #[default]
    #[serde(other)]
    Unknown,
}

/// `sticker` (`td_api.tl:593`).
#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct Sticker {
    #[serde(default)]
    pub(super) width: i32,
    #[serde(default)]
    pub(super) height: i32,
    #[serde(default)]
    pub(super) format: StickerFormat,
    #[serde(default)]
    pub(super) sticker: File,
}

/// `messageSticker` (`td_api.tl:5490`).
#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct MessageSticker {
    #[serde(default)]
    pub(super) sticker: Sticker,
}

/// `location` (`td_api.tl:647`). Its `Debug` prints nothing (a position is personal data).
#[derive(Clone, Copy, Default, Deserialize)]
pub(super) struct Position {
    #[serde(default)]
    pub(super) latitude: f64,
    #[serde(default)]
    pub(super) longitude: f64,
}

impl std::fmt::Debug for Position {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Position(..)")
    }
}

/// `messageLocation{location}` and `messageLiveLocation{location: liveLocation{location}}`
/// (`td_api.tl:5525-5528`, `:655`).
#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct MessageLocation {
    #[serde(default)]
    pub(super) location: Option<LocationOrLive>,
}

/// A `location`, or a `liveLocation` holding one: both shapes in one (the live one's position is
/// its `location`).
#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct LocationOrLive {
    #[serde(default)]
    pub(super) latitude: f64,
    #[serde(default)]
    pub(super) longitude: f64,
    #[serde(default)]
    pub(super) location: Option<Position>,
}

impl LocationOrLive {
    pub(super) fn position(&self) -> Position {
        self.location.unwrap_or(Position {
            latitude: self.latitude,
            longitude: self.longitude,
        })
    }
}

/// `venue` (`td_api.tl:664`); the provider's ids are not read.
#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct Venue {
    #[serde(default)]
    pub(super) location: Position,
    #[serde(default)]
    pub(super) title: Text,
    #[serde(default)]
    pub(super) address: Text,
}

/// `messageVenue` (`td_api.tl:5531`).
#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct MessageVenue {
    #[serde(default)]
    pub(super) venue: Venue,
}

/// `contact` (`td_api.tl:641`); the vCard is not read (the model's card has a name and numbers).
#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct Contact {
    #[serde(default)]
    pub(super) phone_number: Text,
    #[serde(default)]
    pub(super) first_name: Text,
    #[serde(default)]
    pub(super) last_name: Text,
}

/// `messageContact` (`td_api.tl:5534`).
#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct MessageContact {
    #[serde(default)]
    pub(super) contact: Contact,
}

/// `pollOption` (`td_api.tl:456`): its text only (votes are T8).
#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct PollOption {
    #[serde(default)]
    pub(super) text: FormattedText,
}

/// `poll` (`td_api.tl:712`): the question, the options, single or multiple choice.
#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct Poll {
    #[serde(default)]
    pub(super) question: FormattedText,
    #[serde(default)]
    pub(super) options: Vec<PollOption>,
    #[serde(default)]
    pub(super) allows_multiple_answers: bool,
}

/// `messagePoll` (`td_api.tl:5555`).
#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct MessagePoll {
    #[serde(default)]
    pub(super) poll: Poll,
}

/// `messageText` (`td_api.tl:5454`); the link preview is not read.
#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct MessageText {
    #[serde(default)]
    pub(super) text: FormattedText,
}

/// `messageAnimatedEmoji` (`td_api.tl:5537`): the emoji is its text.
#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct MessageAnimatedEmoji {
    #[serde(default)]
    pub(super) emoji: Text,
}

/// `messageForwardInfo` (`td_api.tl:3264`): the origin's kind and the original date (no names).
#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct ForwardInfo {
    #[serde(default)]
    pub(super) origin: Option<ForwardOrigin>,
    #[serde(default)]
    pub(super) date: i32,
}

/// `MessageOrigin`'s `@type` only.
#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct ForwardOrigin {
    #[serde(default, rename = "@type")]
    pub(super) r#type: String,
}

/// `CallDiscardReason` (`td_api.tl:7316-7334`); the upgrade's invite link is not read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(tag = "@type")]
pub(super) enum DiscardReason {
    #[serde(rename = "callDiscardReasonMissed")]
    Missed,
    #[serde(rename = "callDiscardReasonDeclined")]
    Declined,
    #[serde(rename = "callDiscardReasonDisconnected")]
    Disconnected,
    #[serde(rename = "callDiscardReasonHungUp")]
    HungUp,
    #[serde(rename = "callDiscardReasonUpgradeToGroupCall")]
    UpgradeToGroupCall,
    /// `callDiscardReasonEmpty`, and a reason this pin does not know.
    #[default]
    #[serde(other)]
    Empty,
}

/// `messageCall` (`td_api.tl:5591`); `unique_id` is not read (0 for a call from another device:
/// the call's id is the message's).
#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct MessageCall {
    #[serde(default)]
    pub(super) is_video: bool,
    #[serde(default)]
    pub(super) discard_reason: DiscardReason,
    #[serde(default)]
    pub(super) duration: i32,
}

/// `messageBasicGroupChatCreate`, `messageSupergroupChatCreate`, `messageChatChangeTitle`
/// (`td_api.tl:5629-5635`) read as one shape: the title (the member ids of a create are not read).
#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct MessageTitle {
    #[serde(default)]
    pub(super) title: Text,
}

/// `messageChatAddMembers` (`td_api.tl:5659`).
#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct MessageAddMembers {
    #[serde(default)]
    pub(super) member_user_ids: Vec<i64>,
}

/// `messageChatDeleteMember` (`td_api.tl:5671`).
#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct MessageDeleteMember {
    #[serde(default)]
    pub(super) user_id: i64,
}

/// `messageChatSetMessageAutoDeleteTime` (`td_api.tl:5701`); `from_user_id` is not read.
#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct MessageAutoDeleteTime {
    #[serde(default)]
    pub(super) message_auto_delete_time: i32,
}
