//! A message's content -> Kourier's `Body`, and the message's own content facts (view once, the
//! timers, `meta`) (TG-3 C4a, brief §3.3; decisions H4, L-9, his "Generic line").
//!
//! PURE: no I/O, no logging, no panic on any input; the clock is an argument. The input is a
//! td_api `message` (or, for an edit, a `MessageContent` alone) and who we are; the output is a
//! [`Mapped`] the message machine (C6a) puts into an `IncomingMessage`.
//!
//! The rules:
//! - [`CONTENTS`] names EVERY `MessageContent` constructor of the pinned `td_api.tl` with its
//!   [`Family`] (`testdata/message-contents.txt` is the derived list a test compares it with). A
//!   `@type` outside it (none at the pin) is `Unsupported{kind_hint: "tg:unsupported"}`.
//! - `messageText` is `Body::Text` read by `markup::mark` (its link preview is not read).
//! - Media (photo, animation, audio, document, video, video note, voice note) is `Body::Media` with
//!   ONE attachment and the caption (`markup::mark`): kind, MIME, size, file name, dimensions,
//!   duration, and a POINTER only (L-9: nothing is downloaded until T6): `remote` = a `tg-file v1`
//!   blob (JSON `{id, unique_id, size}` of TDLib's `remoteFile`), state `Remote`. A file without remote
//!   ids has no pointer: state `Unavailable`. A photo takes its largest size. A sticker is
//!   `Body::Sticker` the same way.
//! - Location and venue are `Body::Location` (degrees x 10^7; a position outside the globe is no
//!   location); a live location is its current position, `meta` says `live`. A contact is
//!   `Body::Contact` (first and last name, the number as `+<digits>`; the vCard is not read). A poll
//!   is `Body::Poll` (question, options, multiple choice; votes are T8).
//! - A kind Kourier has words for takes Kourier's hint when it cannot be shown as itself: a sticker
//!   without a pointer is `sticker`, a contact with neither name nor number `contact`, a poll with no
//!   question or no option `poll` (its question as the summary when it has one), view-once media
//!   `view-once`, TDLib's `messageUnsupported` (content a newer TDLib knows) `newer-protocol`. EVERY
//!   other kind is a normal row `Unsupported{kind_hint: "tg:<type>", summary: None}` (his "Generic
//!   line": Kourier's generic text, no English of ours).
//! - Calls (C4b, decisions G-3: shown, never taken; patch 0002): `messageCall` (a private chat's
//!   ended 1:1 call) is a call LINE `Body::System(Call(..))`, its id the MESSAGE id (`unique_id` is 0
//!   for a call from another device), made at the message's date: INCOMING and not answered
//!   (`duration` 0, any reason but `Declined`) is `Missed`, said by the peer (`Origin::Remote`, the
//!   one state `may_come_from_peer` allows); incoming `Declined` and incoming answered (`duration` >
//!   0) are OUR act on another own device (`Declined` / `Answered`, [`Mapped::own`]); OUTGOING (from
//!   another own device) is `Answered` (`duration` > 0), `Declined` (the callee declined) or
//!   `Unanswered`. Media: `is_video`; `by`: the peer of an incoming call. `messageVideoChatStarted`
//!   is a group line `Started` (`by` the sender); every other group-call kind (`messageGroupCall`, a
//!   call not bound to a chat that can sit in a private chat, video chats scheduled, ended, invited)
//!   takes Kourier's `group-call` words: nothing is ever sent for a call.
//! - Service kinds (C4b, brief §3.3; `by` is the sender, `None` = us, the model's "You"): created
//!   (basic group, supergroup) `GroupCreated`; title `TitleChanged` (the title kept like a place
//!   name); photo set or deleted `AvatarChanged`; members added `MembersAdded` (our own join, the
//!   list being only us, `Joined`); a join by link or request `Joined` (us) or `MembersAdded{by: the
//!   member, users: [the member]}` (the model's "joined" wording); a member deleted `MemberLeft`
//!   when he deleted himself, else `MembersRemoved`; the auto-delete timer `TimerChanged` (0 = off).
//!   Kourier's words: a pin `pin`, an invoice, a payment or a refund `payment`. EVERY other service
//!   kind (the upgrade's `messageChatUpgradeTo`/`From` among them: the chat machine records the
//!   move, C5a) is the generic line. These need the message (its sender and direction): they are
//!   mapped by [`message`]; [`content`] alone (an edit's new content, which never is one of them)
//!   gives them the generic line.
//! - The self-destructed media (`messageExpired*`) is `Placeholder::Expired`.
//! - `self_destruct_type: Immediately` is view once (Signal's rule): `Unsupported{"view-once"}`, NO
//!   attachment and no pointer kept, `view_once: true`. `Timer{t}` is `Expiry{AfterRead, t}`, started
//!   once `self_destruct_in` runs; the chat's auto-delete (`auto_delete_in > 0`) is
//!   `Expiry{AfterSend}` from the message's date to `now + auto_delete_in`.
//! - `meta` is a `tg-msg v1` blob (JSON, only the facts that are set): `protected` (`can_be_saved`
//!   false: TG9's export never saves it), `restricted`, `sensitive`, `album` (`media_album_id`, one
//!   message per album item), `live`, `forward` (the origin's `@type` and date, no name). None when
//!   nothing is set.

use std::time::Duration;

use kourier_model::{
    Attachment, AttachmentKind, AttachmentState, BackendBlob, Body, CallDirection, CallLine,
    CallMedia, CallState, ContactCard, DisappearKind, Expiry, Identifier, IdentifierKind, Location,
    Origin, Placeholder, Poll, SystemEvent, UserId,
};
use serde::Deserialize;
use serde_json::Value;

use crate::markup::{self, Me};
use crate::tdapi::{File, FormattedText, Message, MessageSender, Raw, SelfDestructType};

mod td;

/// The version of an attachment's `remote` blob (`tg-file v1`: JSON `{id, unique_id, size}`).
pub(crate) const FILE_BLOB_VERSION: u16 = 1;
/// The version of a message's `meta` blob (`tg-msg v1`).
pub(crate) const MSG_META_VERSION: u16 = 1;

/// What a content is, for the map. [`Family::Call`], [`Family::GroupCall`] and
/// [`Family::Service`] need the whole message ([`message`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Family {
    Text,
    Photo,
    /// Animation, audio, document, video, video note, voice note: one attachment and a caption.
    Media(AttachmentKind),
    Sticker,
    /// A self-destructed photo, video, video note or voice note.
    Expired,
    Location,
    LiveLocation,
    Venue,
    Contact,
    Poll,
    /// The emoji is the text.
    AnimatedEmoji,
    /// TDLib's own "this TDLib cannot show it".
    NewerProtocol,
    /// A 1:1 call's line.
    Call,
    /// A group call or video chat.
    GroupCall,
    /// A service message (system events, Kourier's hints, or the generic line).
    Service,
    /// Content Kourier has no words for: the generic line.
    Generic,
}

/// Every `MessageContent` constructor of the pinned `td_api.tl` (1.8.69, 108; 107 at 1.8.68), in the file's order.
pub(crate) const CONTENTS: &[(&str, Family)] = &[
    ("messageText", Family::Text),
    ("messageRichMessage", Family::Generic),
    ("messageAnimation", Family::Media(AttachmentKind::Gif)),
    ("messageAudio", Family::Media(AttachmentKind::Audio)),
    ("messageDocument", Family::Media(AttachmentKind::File)),
    ("messagePaidMedia", Family::Generic),
    ("messagePhoto", Family::Photo),
    ("messageSticker", Family::Sticker),
    ("messageVideo", Family::Media(AttachmentKind::Video)),
    ("messageVideoNote", Family::Media(AttachmentKind::Video)),
    ("messageVoiceNote", Family::Media(AttachmentKind::Voice)),
    ("messageExpiredPhoto", Family::Expired),
    ("messageExpiredVideo", Family::Expired),
    ("messageExpiredVideoNote", Family::Expired),
    ("messageExpiredVoiceNote", Family::Expired),
    ("messageLiveLocation", Family::LiveLocation),
    ("messageLocation", Family::Location),
    ("messageVenue", Family::Venue),
    ("messageContact", Family::Contact),
    ("messageAnimatedEmoji", Family::AnimatedEmoji),
    ("messageDice", Family::Generic),
    ("messageGame", Family::Generic),
    ("messagePoll", Family::Poll),
    ("messageStakeDice", Family::Generic),
    ("messageStory", Family::Generic),
    ("messageChecklist", Family::Generic),
    ("messageInvoice", Family::Service),
    ("messageCall", Family::Call),
    ("messageGroupCall", Family::GroupCall),
    ("messageVideoChatScheduled", Family::GroupCall),
    ("messageVideoChatStarted", Family::GroupCall),
    ("messageVideoChatEnded", Family::GroupCall),
    ("messageInviteVideoChatParticipants", Family::GroupCall),
    ("messagePollOptionAdded", Family::Service),
    ("messagePollOptionDeleted", Family::Service),
    ("messageBasicGroupChatCreate", Family::Service),
    ("messageSupergroupChatCreate", Family::Service),
    ("messageChatChangeTitle", Family::Service),
    ("messageChatChangePhoto", Family::Service),
    ("messageChatDeletePhoto", Family::Service),
    ("messageChatOwnerLeft", Family::Service),
    ("messageChatOwnerChanged", Family::Service),
    ("messageChatHasProtectedContentToggled", Family::Service),
    (
        "messageChatHasProtectedContentDisableRequested",
        Family::Service,
    ),
    ("messageChatAddMembers", Family::Service),
    ("messageChatJoinByLink", Family::Service),
    ("messageChatJoinByRequest", Family::Service),
    ("messageChatJoinFromCommunity", Family::Service),
    ("messageChatDeleteMember", Family::Service),
    ("messageChatAddedToCommunity", Family::Service),
    ("messageChatRemovedFromCommunity", Family::Service),
    ("messageChatUpgradeTo", Family::Service),
    ("messageChatUpgradeFrom", Family::Service),
    ("messagePinMessage", Family::Service),
    ("messageScreenshotTaken", Family::Service),
    ("messageChatSetBackground", Family::Service),
    ("messageChatSetTheme", Family::Service),
    ("messageChatSetMessageAutoDeleteTime", Family::Service),
    ("messageChatBoost", Family::Service),
    ("messageForumTopicCreated", Family::Service),
    ("messageForumTopicEdited", Family::Service),
    ("messageForumTopicIsClosedToggled", Family::Service),
    ("messageForumTopicIsHiddenToggled", Family::Service),
    ("messageSuggestProfilePhoto", Family::Service),
    ("messageSuggestBirthdate", Family::Service),
    ("messageCustomServiceAction", Family::Service),
    ("messageGameScore", Family::Service),
    ("messageManagedBotCreated", Family::Service),
    ("messagePaymentSuccessful", Family::Service),
    ("messagePaymentSuccessfulBot", Family::Service),
    ("messagePaymentRefunded", Family::Service),
    ("messageGiftedPremium", Family::Service),
    ("messagePremiumGiftCode", Family::Service),
    ("messageGiveawayCreated", Family::Service),
    ("messageGiveaway", Family::Service),
    ("messageGiveawayCompleted", Family::Service),
    ("messageGiveawayWinners", Family::Service),
    ("messageGiftedStars", Family::Service),
    ("messageGiftedGrams", Family::Service),
    ("messageGiveawayPrizeStars", Family::Service),
    ("messageTonWalletTransfer", Family::Service),
    ("messageTonConnectRequest", Family::Service),
    ("messageGift", Family::Service),
    ("messageUpgradedGift", Family::Service),
    ("messageRefundedUpgradedGift", Family::Service),
    ("messageUpgradedGiftPurchaseOffer", Family::Service),
    ("messageUpgradedGiftPurchaseOfferRejected", Family::Service),
    ("messagePaidMessagesRefunded", Family::Service),
    ("messagePaidMessagePriceChanged", Family::Service),
    ("messageDirectMessagePriceChanged", Family::Service),
    // LC-20: new at 1.8.69 ("the sender used a link of the other user, which allows unpaid messages"): a system
    // event about paid messages like its neighbour, so the generic line `tg:messageUnpaidMessagesAllowedByLink`.
    ("messageUnpaidMessagesAllowedByLink", Family::Service),
    ("messageChecklistTasksDone", Family::Service),
    ("messageChecklistTasksAdded", Family::Service),
    ("messageSuggestedPostApprovalFailed", Family::Service),
    ("messageSuggestedPostApproved", Family::Service),
    ("messageSuggestedPostDeclined", Family::Service),
    ("messageSuggestedPostPaid", Family::Service),
    ("messageSuggestedPostRefunded", Family::Service),
    ("messageContactRegistered", Family::Service),
    ("messageUsersShared", Family::Service),
    ("messageChatShared", Family::Service),
    ("messageBotWriteAccessAllowed", Family::Service),
    ("messageWebAppDataSent", Family::Service),
    ("messageWebAppDataReceived", Family::Service),
    ("messagePassportDataSent", Family::Service),
    ("messagePassportDataReceived", Family::Service),
    ("messageProximityAlertTriggered", Family::Service),
    ("messageUnsupported", Family::NewerProtocol),
];

/// The kind hint of a `@type` outside [`CONTENTS`] (and of a content with no `@type`).
pub(crate) const UNKNOWN_HINT: &str = "tg:unsupported";

/// The longest place name, address, contact name, poll question or option kept (bytes); a longer one
/// is cut at a character boundary.
const MAX_TEXT: usize = 1024;
/// Most poll options kept (Telegram allows 12).
const MAX_OPTIONS: usize = 32;
/// Most members one notice names (Telegram adds at most 200 at once); the rest are left out.
const MAX_MEMBERS: usize = 256;

/// The family and the table's name (`'static`, so a kind hint never holds input) of a `@type`.
pub(crate) fn family(type_name: &str) -> Option<(&'static str, Family)> {
    CONTENTS
        .iter()
        .find(|(name, _)| *name == type_name)
        .copied()
}

/// A content read: its body, whether its text mentions us, the entities left out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Content {
    pub(crate) body: Body,
    pub(crate) mentions_me: bool,
    pub(crate) dropped: usize,
    /// A live location (for `meta`).
    pub(crate) live: bool,
}

impl Content {
    fn of(body: Body) -> Self {
        Self {
            body,
            mentions_me: false,
            dropped: 0,
            live: false,
        }
    }
}

/// A message's content and its content facts. `Debug` prints kinds and counts (the model's types
/// hide texts, names, positions and pointers).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Mapped {
    pub(crate) body: Body,
    pub(crate) mentions_me: bool,
    pub(crate) dropped: usize,
    pub(crate) view_once: bool,
    /// TDLib marks the message restricted (`restriction_reason`, TF13).
    pub(crate) restricted: bool,
    pub(crate) expiry: Option<Expiry>,
    pub(crate) meta: Option<BackendBlob>,
    /// The row is said by one of our own devices whatever its sender: a call line the peer may not
    /// say (an incoming call answered or declined elsewhere). See [`origin`].
    pub(crate) own: bool,
}

/// The `Unsupported` body with a hint and no summary.
fn hint(kind_hint: &str) -> Body {
    Body::Unsupported {
        kind_hint: kind_hint.to_owned(),
        summary: None,
    }
}

/// The generic line of a content of the table: `tg:<type>`.
fn generic(name: &'static str) -> Body {
    hint(&format!("tg:{name}"))
}

/// Reads one `MessageContent` (a message's, or an edit's new content).
pub(crate) fn content(raw: &Raw, me: Me<'_>) -> Content {
    let value = raw.value();
    let Some((name, family)) = family(raw.type_name()) else {
        return Content::of(hint(UNKNOWN_HINT));
    };
    match family {
        Family::Text => {
            let text = read::<td::MessageText>(value).text;
            let marked = markup::mark(&text, me);
            Content {
                body: Body::Text(marked.text),
                mentions_me: marked.mentions_me,
                dropped: marked.dropped,
                live: false,
            }
        }
        Family::Photo => {
            let m = read::<td::MessagePhoto>(value);
            let largest = m
                .photo
                .sizes
                .iter()
                .max_by_key(|s| i64::from(s.width.max(0)) * i64::from(s.height.max(0)));
            match largest {
                Some(size) => {
                    let mut a = attachment(AttachmentKind::Image, &size.photo);
                    a.mime = "image/jpeg".to_owned();
                    a.dims = dims(size.width, size.height);
                    media(a, &m.caption, me)
                }
                None => Content::of(generic(name)),
            }
        }
        Family::Media(kind) => {
            let m = read::<td::MessageMedia>(value);
            let inner = [
                &m.animation,
                &m.audio,
                &m.document,
                &m.video,
                &m.video_note,
                &m.voice_note,
            ]
            .into_iter()
            .find_map(Option::as_ref);
            match inner.and_then(|i| i.file().map(|f| (i, f))) {
                Some((inner, file)) => {
                    let mut a = attachment(kind, file);
                    a.mime = inner.mime_type.as_str().to_owned();
                    if a.mime.is_empty() {
                        a.mime = default_mime(name).to_owned();
                    }
                    a.file_name = kept(inner.file_name.as_str());
                    a.dims = if inner.length > 0 {
                        dims(inner.length, inner.length)
                    } else {
                        dims(inner.width, inner.height)
                    };
                    a.duration_ms = (inner.duration > 0)
                        .then(|| u32::try_from(inner.duration).ok()?.checked_mul(1000))
                        .flatten();
                    media(a, &m.caption, me)
                }
                None => Content::of(generic(name)),
            }
        }
        Family::Sticker => {
            let s = read::<td::MessageSticker>(value).sticker;
            let mut a = attachment(AttachmentKind::Image, &s.sticker);
            if a.remote.is_none() {
                return Content::of(hint("sticker"));
            }
            a.mime = match s.format {
                td::StickerFormat::Webp => "image/webp",
                td::StickerFormat::Tgs => "application/x-tgsticker",
                td::StickerFormat::Webm => "video/webm",
                td::StickerFormat::Unknown => "application/octet-stream",
            }
            .to_owned();
            a.dims = dims(s.width, s.height);
            Content::of(Body::Sticker(Box::new(a)))
        }
        Family::Expired => Content::of(Body::Placeholder(Placeholder::Expired)),
        Family::Location | Family::LiveLocation => {
            let position = read::<td::MessageLocation>(value)
                .location
                .unwrap_or_default()
                .position();
            match location(position, None, None) {
                Some(body) => Content {
                    live: family == Family::LiveLocation,
                    ..Content::of(body)
                },
                None => Content::of(generic(name)),
            }
        }
        Family::Venue => {
            let v = read::<td::MessageVenue>(value).venue;
            match location(v.location, kept(v.title.as_str()), kept(v.address.as_str())) {
                Some(body) => Content::of(body),
                None => Content::of(generic(name)),
            }
        }
        Family::Contact => Content::of(contact(&read::<td::MessageContact>(value).contact)),
        Family::Poll => Content::of(poll(&read::<td::MessagePoll>(value).poll)),
        Family::AnimatedEmoji => {
            let emoji = read::<td::MessageAnimatedEmoji>(value).emoji;
            if emoji.is_empty() {
                Content::of(generic(name))
            } else {
                Content::of(Body::text(emoji.as_str()))
            }
        }
        Family::NewerProtocol => Content::of(hint("newer-protocol")),
        Family::Call | Family::GroupCall | Family::Service | Family::Generic => {
            Content::of(generic(name))
        }
    }
}

/// Reads a whole message's content and its content facts; `now_ms` is when it was received (the
/// timers left are counted from it).
pub(crate) fn message(msg: &Message, me: Me<'_>, now_ms: i64) -> Mapped {
    let mut own = false;
    let (mut read, whole) = match family(msg.content.type_name()) {
        Some((_, Family::Call)) => {
            let (body, by_us) = call(msg);
            own = by_us;
            (Content::of(body), true)
        }
        Some((name, Family::GroupCall)) => (Content::of(group_call(msg, name)), true),
        Some((name, Family::Service)) => (Content::of(service(msg, name, me)), true),
        _ => (content(&msg.content, me), false),
    };
    // A call or a service message is never view once (only media self-destructs).
    let view_once = !whole && matches!(msg.self_destruct_type, Some(SelfDestructType::Immediately));
    if view_once {
        // Never a pointer: the file would outlive its one view (Signal's rule).
        read.body = hint("view-once");
        read.mentions_me = false;
    }
    // A message TDLib marks restricted (`restriction_reason`, the plan's TSC M7, TF13): access to
    // its content "must be restricted" (td_api.tl), so Kourier shows its generic line: no text
    // and no pointer. `has_sensitive_content` stays a fact (`meta`); T6 must blur such media.
    let restricted = msg
        .restriction_info
        .as_ref()
        .is_some_and(|r| !r.restriction_reason.is_empty());
    if restricted {
        read.body = hint("tg:restricted");
        read.mentions_me = false;
    }
    let date_ms = i64::from(msg.date).saturating_mul(1000);
    let expiry = match msg.self_destruct_type {
        Some(SelfDestructType::Timer { self_destruct_time }) if self_destruct_time > 0 => {
            let timer_ms = i64::from(self_destruct_time) * 1000;
            Some(Expiry {
                kind: DisappearKind::AfterRead,
                timer: Duration::from_secs(u64::from(self_destruct_time.unsigned_abs())),
                started_at: (msg.self_destruct_in > 0).then(|| {
                    now_ms
                        .saturating_add(msg.self_destruct_in.saturating_mul(1000))
                        .saturating_sub(timer_ms)
                }),
            })
        }
        _ if msg.auto_delete_in > 0 => {
            let ends = now_ms.saturating_add(msg.auto_delete_in.saturating_mul(1000));
            let timer_ms = u64::try_from(ends.saturating_sub(date_ms)).unwrap_or(0);
            Some(Expiry {
                kind: DisappearKind::AfterSend,
                timer: Duration::from_millis(timer_ms),
                started_at: Some(date_ms),
            })
        }
        _ => None,
    };
    Mapped {
        meta: meta(msg, read.live),
        body: read.body,
        mentions_me: read.mentions_me,
        dropped: read.dropped,
        view_once,
        restricted,
        expiry,
        own,
    }
}

impl Mapped {
    /// The message must never get a file pointer from a later edit (TF14): a view-once or
    /// self-destructing message (the file would outlive its view) or a restricted one. An edit of
    /// such a message is dropped, the hint stays.
    pub(crate) fn sealed(&self) -> bool {
        self.view_once
            || self.restricted
            || self
                .expiry
                .as_ref()
                .is_some_and(|e| e.kind == DisappearKind::AfterRead)
    }
}

/// Who said a mapped message: one of our own devices for an outgoing message (a message THIS
/// Kourier sent is matched by the send path first) and for a row [`Mapped::own`] says is ours,
/// else its sender (a chat sender is `tg:<its negative id>`, L-8). `None`: a sender of no kind.
pub(crate) fn origin(msg: &Message, mapped: &Mapped) -> Option<Origin> {
    if msg.is_outgoing || mapped.own {
        return Some(Origin::OwnOtherDevice { device: None });
    }
    sender(msg).map(|author| Origin::Remote {
        author,
        device: None,
    })
}

/// The `tg:` id of a user; `None` for an id that is no user's.
fn user(id: i64) -> Option<UserId> {
    (id > 0)
        .then(|| UserId::from_parts("tg", &id.to_string()).ok())
        .flatten()
}

/// The sender's `tg:` id (a chat's is its negative id).
fn sender(msg: &Message) -> Option<UserId> {
    match msg.sender_id {
        MessageSender::User { user_id } => user(user_id),
        MessageSender::Chat { chat_id } if chat_id != 0 => {
            UserId::from_parts("tg", &chat_id.to_string()).ok()
        }
        MessageSender::Chat { .. } | MessageSender::Unknown => None,
    }
}

/// The sender's user id, when the sender is a user.
fn sender_user(msg: &Message) -> Option<i64> {
    match msg.sender_id {
        MessageSender::User { user_id } => Some(user_id),
        MessageSender::Chat { .. } | MessageSender::Unknown => None,
    }
}

/// The message is ours: outgoing, or sent by our user id.
fn ours(msg: &Message, me: Me<'_>) -> bool {
    msg.is_outgoing || sender_user(msg) == Some(me.id)
}

/// A `messageCall`'s line and whether one of our own devices said it (module docs).
fn call(msg: &Message) -> (Body, bool) {
    let c = read::<td::MessageCall>(msg.content.value());
    let at = i64::from(msg.date).saturating_mul(1000);
    let answered = c.duration > 0;
    let declined = c.discard_reason == td::DiscardReason::Declined;
    let (direction, state, own) = if msg.is_outgoing {
        let state = if answered {
            CallState::Answered
        } else if declined {
            CallState::Declined
        } else {
            CallState::Unanswered
        };
        (CallDirection::Outgoing, state, true)
    } else if answered {
        (CallDirection::Incoming, CallState::Answered, true)
    } else if declined {
        (CallDirection::Incoming, CallState::Declined, true)
    } else {
        (CallDirection::Incoming, CallState::Missed, false)
    };
    let media = if c.is_video {
        CallMedia::Video
    } else {
        CallMedia::Voice
    };
    let mut line = CallLine::signal(direction, false, state, at).with_media(media, at);
    if direction == CallDirection::Incoming
        && let Some(peer) = sender(msg)
    {
        line = line.with_by(peer, at);
    }
    (Body::System(SystemEvent::Call(line)), own)
}

/// A group call's or video chat's body (module docs).
fn group_call(msg: &Message, name: &'static str) -> Body {
    if name != "messageVideoChatStarted" {
        return hint("group-call");
    }
    let at = i64::from(msg.date).saturating_mul(1000);
    let direction = if msg.is_outgoing {
        CallDirection::Outgoing
    } else {
        CallDirection::Incoming
    };
    let mut line = CallLine::signal(direction, true, CallState::Started, at);
    if direction == CallDirection::Incoming
        && let Some(by) = sender(msg)
    {
        line = line.with_by(by, at);
    }
    Body::System(SystemEvent::Call(line))
}

/// A service message's body (module docs).
fn service(msg: &Message, name: &'static str, me: Me<'_>) -> Body {
    let value = msg.content.value();
    let mine = ours(msg, me);
    let by = if mine { None } else { sender(msg) };
    let event = match name {
        "messageBasicGroupChatCreate" | "messageSupergroupChatCreate" => {
            SystemEvent::GroupCreated { by }
        }
        "messageChatChangeTitle" => SystemEvent::TitleChanged {
            by,
            title: kept(read::<td::MessageTitle>(value).title.as_str()).unwrap_or_default(),
        },
        "messageChatChangePhoto" | "messageChatDeletePhoto" => SystemEvent::AvatarChanged { by },
        "messageChatAddMembers" => {
            let ids = read::<td::MessageAddMembers>(value).member_user_ids;
            if mine && ids == [me.id] {
                SystemEvent::Joined
            } else {
                let users: Vec<UserId> =
                    ids.into_iter().filter_map(user).take(MAX_MEMBERS).collect();
                if users.is_empty() {
                    return generic(name);
                }
                SystemEvent::MembersAdded { by, users }
            }
        }
        "messageChatJoinByLink" | "messageChatJoinByRequest" => {
            if mine {
                SystemEvent::Joined
            } else {
                match sender_user(msg).and_then(user) {
                    Some(member) => SystemEvent::MembersAdded {
                        by: Some(member.clone()),
                        users: vec![member],
                    },
                    None => return generic(name),
                }
            }
        }
        "messageChatDeleteMember" => {
            let id = read::<td::MessageDeleteMember>(value).user_id;
            let Some(gone) = user(id) else {
                return generic(name);
            };
            if sender_user(msg) == Some(id) {
                SystemEvent::MemberLeft { user: gone }
            } else {
                SystemEvent::MembersRemoved {
                    by,
                    users: vec![gone],
                }
            }
        }
        "messageChatSetMessageAutoDeleteTime" => {
            let secs = read::<td::MessageAutoDeleteTime>(value).message_auto_delete_time;
            SystemEvent::TimerChanged {
                by,
                timer: u64::try_from(secs)
                    .ok()
                    .filter(|s| *s > 0)
                    .map(Duration::from_secs),
            }
        }
        "messagePinMessage" => return hint("pin"),
        "messageInvoice"
        | "messagePaymentSuccessful"
        | "messagePaymentSuccessfulBot"
        | "messagePaymentRefunded" => return hint("payment"),
        _ => return generic(name),
    };
    Body::System(event)
}

/// A td_api object read from the content's JSON; a shape that does not fit reads as its default
/// (every field is optional), so the map still says what kind it was.
fn read<T: for<'de> Deserialize<'de> + Default>(value: &Value) -> T {
    T::deserialize(value).unwrap_or_default()
}

/// `tg-file v1`: TDLib's remote ids and the size (what T6 downloads by). `Debug` prints the size
/// only: the remote id is a handle that downloads the file with this account (TF16).
#[derive(serde::Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct FilePointer {
    pub(crate) id: String,
    pub(crate) unique_id: String,
    pub(crate) size: u64,
}

impl std::fmt::Debug for FilePointer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilePointer")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

/// An attachment of a kind with the file's pointer, or no pointer (`Unavailable`) when the file has
/// no remote ids.
fn attachment(kind: AttachmentKind, file: &File) -> Attachment {
    let size = u64::try_from(if file.size > 0 {
        file.size
    } else {
        file.expected_size
    })
    .unwrap_or(0);
    let pointer =
        (!file.remote.id.is_empty() || !file.remote.unique_id.is_empty()).then(|| FilePointer {
            id: file.remote.id.as_str().to_owned(),
            unique_id: file.remote.unique_id.as_str().to_owned(),
            size,
        });
    let remote = pointer.and_then(|p| {
        serde_json::to_vec(&p)
            .ok()
            .map(|bytes| BackendBlob::new(FILE_BLOB_VERSION, bytes))
    });
    Attachment {
        kind,
        size,
        state: if remote.is_some() {
            AttachmentState::Remote
        } else {
            AttachmentState::Unavailable
        },
        remote,
        ..Attachment::default()
    }
}

/// A media body of one attachment and its caption.
fn media(attachment: Attachment, caption: &FormattedText, me: Me<'_>) -> Content {
    let marked = (!caption.text.is_empty()).then(|| markup::mark(caption, me));
    Content {
        mentions_me: marked.as_ref().is_some_and(|m| m.mentions_me),
        dropped: marked.as_ref().map_or(0, |m| m.dropped),
        body: Body::Media {
            attachments: vec![attachment],
            caption: marked.map(|m| m.text),
        },
        live: false,
    }
}

/// The MIME type of a kind whose file says none.
fn default_mime(name: &str) -> &'static str {
    match name {
        "messageAnimation" | "messageVideo" | "messageVideoNote" => "video/mp4",
        "messageVoiceNote" => "audio/ogg",
        "messageAudio" => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

/// Width and height when both are positive.
fn dims(width: i32, height: i32) -> Option<(u32, u32)> {
    Some((
        u32::try_from(width).ok().filter(|w| *w > 0)?,
        u32::try_from(height).ok().filter(|h| *h > 0)?,
    ))
}

/// A string kept when it holds more than white space, cut to [`MAX_TEXT`] bytes at a character
/// boundary.
fn kept(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let end = (0..=text.len().min(MAX_TEXT))
        .rev()
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(0);
    Some(text[..end].to_owned())
}

/// Degrees as a model coordinate (degrees x 10^7), `None` outside the globe or not a number.
fn degrees_e7(degrees: f64, limit: f64) -> Option<i32> {
    // |degrees| <= 180 gives |e7| <= 1.8e9, inside `i32`.
    (degrees.is_finite() && degrees.abs() <= limit).then(|| (degrees * 1e7).round() as i32)
}

/// A place; `None` when the position is outside the globe.
fn location(p: td::Position, name: Option<String>, address: Option<String>) -> Option<Body> {
    Some(Body::Location(Location {
        lat_e7: degrees_e7(p.latitude, 90.0)?,
        lon_e7: degrees_e7(p.longitude, 180.0)?,
        name,
        address,
    }))
}

/// A shared contact: the name and the number (`+<digits>`, 3..=20 digits); neither is `contact`.
fn contact(c: &td::Contact) -> Body {
    let name = kept(&format!(
        "{} {}",
        c.first_name.as_str().trim(),
        c.last_name.as_str().trim()
    ));
    let digits: String = c
        .phone_number
        .as_str()
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    let identifiers: Vec<Identifier> = (3..=20)
        .contains(&digits.len())
        .then(|| Identifier {
            kind: IdentifierKind::Phone,
            value: format!("+{digits}"),
        })
        .into_iter()
        .collect();
    if name.is_none() && identifiers.is_empty() {
        return hint("contact");
    }
    Body::Contact(ContactCard {
        name,
        organization: None,
        identifiers,
        avatar: None,
    })
}

/// A poll: question, options, multiple choice; without a question or an option it is `poll` with
/// the question (if any) as the summary.
fn poll(p: &td::Poll) -> Body {
    let question = kept(p.question.text.as_str());
    let options: Vec<String> = p
        .options
        .iter()
        .filter_map(|o| kept(o.text.text.as_str()))
        .take(MAX_OPTIONS)
        .collect();
    match question {
        Some(question) if !options.is_empty() => Body::Poll(Poll {
            question,
            options,
            multiple: p.allows_multiple_answers,
        }),
        question => Body::Unsupported {
            kind_hint: "poll".to_owned(),
            summary: question,
        },
    }
}

/// `tg-msg v1`: what the export (TG9) and later steps read; only the facts that are set.
#[derive(serde::Serialize, Deserialize, Default, PartialEq, Eq, Debug)]
#[serde(default)]
pub(crate) struct MsgMeta {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) protected: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) restricted: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) sensitive: bool,
    /// The album's id in decimal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) album: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) live: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) forward: Option<Forward>,
}

/// A forward's origin kind (`messageOriginUser`, ...) and its original date (s).
#[derive(serde::Serialize, Deserialize, Default, PartialEq, Eq, Debug)]
pub(crate) struct Forward {
    pub(crate) origin: String,
    pub(crate) date: i32,
}

/// The message's `meta` blob, or None when no fact is set.
fn meta(msg: &Message, live: bool) -> Option<BackendBlob> {
    let restriction = msg.restriction_info.as_ref();
    let forward = msg.forward_info.as_ref().map(|raw| {
        let f = read::<td::ForwardInfo>(raw.value());
        Forward {
            // A `@type` of td_api, never free text: kept only when it is one.
            origin: f
                .origin
                .map(|o| o.r#type)
                .filter(|t| {
                    t.starts_with("messageOrigin") && t.bytes().all(|b| b.is_ascii_alphabetic())
                })
                .unwrap_or_default(),
            date: f.date,
        }
    });
    let meta = MsgMeta {
        protected: !msg.can_be_saved,
        restricted: restriction.is_some_and(|r| !r.restriction_reason.is_empty()),
        sensitive: restriction.is_some_and(|r| r.has_sensitive_content),
        album: (msg.media_album_id != 0).then(|| msg.media_album_id.to_string()),
        live,
        forward,
    };
    if meta == MsgMeta::default() {
        return None;
    }
    serde_json::to_vec(&meta)
        .ok()
        .map(|bytes| BackendBlob::new(MSG_META_VERSION, bytes))
}

#[cfg(test)]
mod tests;
