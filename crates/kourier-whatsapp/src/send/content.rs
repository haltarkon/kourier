//! What a send says (W5a, chunks 4 and 5): an [`OutgoingRequest`] -> the `wa::Message` that goes into the ONE
//! `Client::send` call (decision D2). PURE: no I/O, no clock, no logging; the addressing inputs come from the
//! caller ([`Addressing`], a group's addressing mode).
//!
//! THE KINDS (brief §2.2, the probe's table in `target/wa/w5/scout.md`):
//! - a message: a text is `conversation`, or `extendedTextMessage` when it mentions someone or quotes a
//!   message; its formatting is WhatsApp's markup ([`render`], the inverse of `crate::markup`); the chat's
//!   timer is NOT written here: [`Built::expiration`] goes to `SendOptions::with_ephemeral_expiration`, and
//!   upstream writes `contextInfo.expiration` (promoting a bare `conversation`, WA Web's rule); the one exception is
//!   a document inside its caption wrapper, which upstream's call does not reach: its timer is written into the inner
//!   document here and [`Built::expiration`] stays `None` (fix F8). A body not sent
//!   (a sticker, a contact, a place, a poll: the composer makes none) is [`Unbuildable::NotYet`].
//! - a file (W5b, G10): ONE file per message (the core packs N files as N messages, the caption and the quote on
//!   the first); the message is [`media::file_message`] of the pointer the attempt hands in (the upload's, or the
//!   upload record's on a resend), its caption rendered as a text's, the quote and the mentions in the file's own
//!   context info (inside the caption wrapper of a document). More than one file is [`Unbuildable::ManyFiles`]; no
//!   pointer is [`Unbuildable::NoFile`].
//! - a quote: `contextInfo{stanzaId, participant, quotedMessage}`; the participant is the quoted author in
//!   the chat's namespace (ours from our own ids), the quoted message is rebuilt from the core's snapshot (its
//!   text, the kind, type and name of its file; no thumbnail).
//! - an edit: a top-level `protocolMessage{MESSAGE_EDIT, key = the ORIGINAL's key, editedMessage,
//!   timestampMs}` (K-B1 `EditTarget::Original`; upstream's own `build_edit_message` shape, which is
//!   crate-private at the pin); a caption edit writes the file again with [`media::file_message`], from the
//!   pointer the request's attachment carries (a file from the phone) or else the TARGET's upload record (a file
//!   Kourier sent: the core never stores an own pointer; W5b). Only our own messages are edited.
//! - delete for everyone: `protocolMessage{REVOKE, key}` with `fromMe = true` for our own message. A key with
//!   `fromMe = false` is an ADMIN's revoke (`edit="8"`: the probe): built for another member's message in a GROUP
//!   only, and only when the caller says we are an admin there (`admin`: the group's row, W6a2 B7b, his U4); the
//!   key names the author as the original message's key did when the backend still has that form (the inbox ledger,
//!   fix F3), else as the group writes him now (`participant`, [`KeyFor::Revoke`]). Anywhere else another's
//!   message is [`Unbuildable::NotOwn`]. A delete for me is not sent ([`Unbuildable::ForMe`]; the actor answers
//!   it, decision D9).
//! - a reaction: upstream's public `build_reaction_message(key, emoji, ms)`; taking it back is the empty
//!   text (the receive map's inverse, `map/modifier.rs`).
//!
//! Times in a built message are the request's `sent_at` (ms, allocated once with the id), so a resend under
//! the same id writes the same message.
//!
//! THE MARKUP ([`render`]). Each formatting range becomes a pair of markers WhatsApp's rule reads back (the
//! rule as `crate::markup` states it): a range is trimmed of white space at its ends and cut at line breaks;
//! a range whose start follows a letter or digit, or whose end is followed by one, cannot be written and is
//! left out; overlapping ranges are written nested (a range is closed and opened again where needed, mono
//! innermost: nothing is read inside mono; a pair never opens before or closes after white space, so the
//! white space at such a cut stays outside it). The result is read back with the receive map's own parser and
//! must give the same text and the same formatting; when it does not (a marker the person typed, an edge the
//! rule reads differently), the text is sent WITHOUT formatting rather than with formatting it did not have.
//! A mention is `@<user part>` plus the person's id in `mentionedJid` (one entry per mention, so the receive
//! rule, which takes the first free `@handle` per entry, finds each); a placeholder that names nobody (a
//! mention of everyone: WhatsApp states none, D9) is left out. Spoilers and links have no WhatsApp markup:
//! their text goes as it is.

use std::cell::Cell;
use std::collections::BTreeMap;

use kourier_backend_api::{OutgoingKind, OutgoingRequest};
use kourier_model::text::MENTION_PLACEHOLDER;
use kourier_model::{
    Attachment, AttachmentKind, Body, ConvKey, QuoteSnapshot, RangeKind, ReplyTo, TargetAuthor,
    Text, UserId,
};
use wacore_binary::Jid;
use waproto::buffa::MessageField;
use waproto::whatsapp as wa;

use super::address::{Addressing, KeyFor, Namespace, Unaddressable};
use crate::markup::{self, Mentioned};
use crate::media::{self, FileFacts, MediaPointer};

/// What goes into `Client::send`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Built {
    pub(crate) message: wa::Message,
    /// The chat's timer (seconds) for `SendOptions::with_ephemeral_expiration`; a message only. `None` for a
    /// document inside its caption wrapper: upstream's call does not reach it, so the timer is already in the inner
    /// document's context (fix F8).
    pub(crate) expiration: Option<u32>,
    /// Formatting the markup could not carry was left out (the text itself is whole). Log-safe count.
    pub(crate) plain: bool,
    /// A mention or a quote's participant was left out because it would have written a phone number into a group
    /// that hides numbers (fix F7). A flag only: nothing about the person.
    pub(crate) withheld: bool,
}

/// Why a request makes no message. Log-safe: a reason, never content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unbuildable {
    /// The chat, a person or a target has no address on the wire.
    Address(Unaddressable),
    /// A body not sent (stickers, contacts, places and polls are not offered).
    NotYet,
    /// A message with more than one file (G10: the core packs one file per message for WhatsApp).
    ManyFiles,
    /// Nothing to send: an empty text, a reaction without an emoji.
    Empty,
    /// An edit or a delete for everyone of a message that is not ours.
    NotOwn,
    /// A file message or a caption edit with no pointer to write the file with (or one that does not read).
    NoFile,
    /// A delete for me: WhatsApp's is an app-state change (W6), never a message.
    ForMe,
}

impl From<Unaddressable> for Unbuildable {
    fn from(reason: Unaddressable) -> Self {
        Self::Address(reason)
    }
}

/// The message of `req`. `group` is a group chat's addressing mode when known. `file` is the pointer of the file a
/// file message carries (the upload's, or its record's) or, for a caption edit, the TARGET's upload record's. `admin`
/// is true when the group's row says we are an admin of `req.conv` (a group we are still in): only then is another
/// member's message deleted for everyone (the admin revoke); it changes nothing else. `received_as` is the author of the
/// message an admin revoke deletes, in the form the message arrived under when the backend still has it (fix F3: the
/// revoke's key names him exactly as the original key did, [`Addressing::key_as_received`]); it is read for that revoke
/// only.
pub(crate) fn build(
    req: &OutgoingRequest,
    at: &Addressing<'_>,
    group: Option<Namespace>,
    file: Option<&MediaPointer>,
    admin: bool,
    received_as: Option<&UserId>,
) -> Result<Built, Unbuildable> {
    let conv = &req.conv;
    let names = Names::new(conv, at, group);
    let person = |user: &UserId| names.person(user);
    let plain_built = |message| Built {
        message,
        expiration: None,
        plain: false,
        withheld: false,
    };
    match &req.kind {
        OutgoingKind::Message {
            body,
            reply_to,
            expiry,
        } => {
            let quote = || names.quote(conv, reply_to.as_ref());
            let (mut message, plain) = match body {
                Body::Text(text) => {
                    let rendered = render(text, &person);
                    if rendered.text.trim().is_empty() {
                        return Err(Unbuildable::Empty);
                    }
                    let plain = rendered.plain;
                    (text_message(rendered, quote()?), plain)
                }
                Body::Media {
                    attachments,
                    caption,
                } => {
                    // The refusals the pointer does not decide are `file_side`'s: the attempt asks the same function
                    // before it touches the file ([`check_file`], fix F4).
                    let side = file_side(attachments, caption.as_ref(), quote, &person)?;
                    let pointer = file.ok_or(Unbuildable::NoFile)?;
                    let mut message =
                        media::file_message(pointer, &FileFacts::of(side.attachment), side.caption)
                            .ok_or(Unbuildable::NoFile)?;
                    set_context(&mut message, side.quote, side.mentioned);
                    (message, side.plain)
                }
                _ => return Err(Unbuildable::NotYet),
            };
            let expiration = expiry
                .map(|timer| u32::try_from(timer.as_secs()).unwrap_or(u32::MAX))
                .filter(|seconds| *seconds > 0);
            // A captioned document is a wrapper upstream's timer call does not reach (it would log a warning and send
            // the file without its timer, fix F8): the timer goes into the inner document, and upstream is not asked.
            let expiration = match expiration {
                Some(seconds) if media::put_timer_inside_wrapper(&mut message, seconds) => None,
                other => other,
            };
            Ok(Built {
                message,
                expiration,
                plain,
                withheld: names.withheld.get(),
            })
        }
        OutgoingKind::Edit { target, body } => {
            if !is_own(&target.author, at) {
                return Err(Unbuildable::NotOwn);
            }
            let key = at.key(conv, target, group, KeyFor::Edit)?;
            let (content, plain) = match body {
                Body::Text(text) => {
                    let rendered = render(text, &person);
                    if rendered.text.trim().is_empty() {
                        return Err(Unbuildable::Empty);
                    }
                    let plain = rendered.plain;
                    (text_message(rendered, None), plain)
                }
                Body::Media {
                    attachments,
                    caption,
                } => {
                    let attachment = attachments.first().ok_or(Unbuildable::NoFile)?;
                    // A file from the phone carries its pointer; a file Kourier sent has it in the upload record.
                    let carried = attachment.remote.as_ref().and_then(MediaPointer::from_blob);
                    let pointer = carried.as_ref().or(file).ok_or(Unbuildable::NoFile)?;
                    let (caption, mentioned, plain) = rendered_caption(caption.as_ref(), &person);
                    let mut content =
                        media::file_message(pointer, &FileFacts::of(attachment), caption)
                            .ok_or(Unbuildable::NoFile)?;
                    set_context(&mut content, None, mentioned);
                    (content, plain)
                }
                _ => return Err(Unbuildable::NotYet),
            };
            let mut p = wa::message::ProtocolMessage::default();
            p.key = MessageField::some(key);
            p.r#type = Some(wa::message::protocol_message::Type::MessageEdit);
            p.edited_message = MessageField::some(content);
            p.timestamp_ms = Some(req.sent_at);
            let mut message = wa::Message::default();
            message.protocol_message = MessageField::some(p);
            Ok(Built {
                message,
                expiration: None,
                plain,
                withheld: names.withheld.get(),
            })
        }
        OutgoingKind::Delete {
            target,
            for_everyone,
        } => {
            if !for_everyone {
                return Err(Unbuildable::ForMe);
            }
            // Another member's message: an admin's revoke, in a group where we are an admin, nowhere else (his U4). The
            // key then says `fromMe = false` with the author as `participant`, which upstream's send turns into the
            // wire attribute `edit="8"`.
            if !is_own(&target.author, at) && !(admin && matches!(conv, ConvKey::Group(_))) {
                return Err(Unbuildable::NotOwn);
            }
            let mut p = wa::message::ProtocolMessage::default();
            let received_as = received_as.filter(|_| admin);
            p.key = MessageField::some(at.key_as_received(
                conv,
                target,
                group,
                KeyFor::Revoke,
                received_as,
            )?);
            p.r#type = Some(wa::message::protocol_message::Type::Revoke);
            let mut message = wa::Message::default();
            message.protocol_message = MessageField::some(p);
            Ok(plain_built(message))
        }
        OutgoingKind::React {
            target,
            emoji,
            remove,
        } => {
            if !remove && emoji.is_empty() {
                return Err(Unbuildable::Empty);
            }
            let key = at.key(conv, target, group, KeyFor::Reaction)?;
            let text = if *remove { "" } else { emoji.as_str() };
            Ok(plain_built(wacore::proto_helpers::build_reaction_message(
                key,
                text,
                req.sent_at,
            )))
        }
    }
}

/// The message that sets a 1:1 chat's disappearing timer (W6a2 B7b, D22): `protocolMessage{EPHEMERAL_SETTING,
/// ephemeralExpiration = the timer in seconds (0 = off), ephemeralSettingTimestamp = `at_s`, disappearingMode =
/// {initiator: CHANGED_IN_CHAT, trigger: UNKNOWN}}` and nothing else, exactly as the PINNED upstream writes it
/// (`Client::set_chat_disappearing_timer`'s private `build_ephemeral_setting_message`, `src/client/iq_ops.rs:226-249`,
/// which mirrors WA Web's `MsgChatActionUtils`; whatsmeow's older `SetDisappearingTimer` leaves the mode out, fix F1).
/// The timestamp is the VERSION of the setting: the receivers (and Kourier's own receive map,
/// `map::modifier::timer_setting`, which reads neither mode field) let a setting with an older one lose. The frame
/// carries no `contextInfo.expiration` of its own (the send's `expiration` stays `None`).
pub(crate) fn timer_setting(secs: u32, at_s: i64) -> wa::Message {
    let mut mode = wa::DisappearingMode::default();
    mode.initiator = Some(wa::disappearing_mode::Initiator::ChangedInChat);
    mode.trigger = Some(wa::disappearing_mode::Trigger::Unknown);
    let mut p = wa::message::ProtocolMessage::default();
    p.r#type = Some(wa::message::protocol_message::Type::EphemeralSetting);
    p.ephemeral_expiration = Some(secs);
    p.ephemeral_setting_timestamp = Some(at_s);
    p.disappearing_mode = MessageField::some(mode);
    let mut message = wa::Message::default();
    message.protocol_message = MessageField::some(p);
    message
}

/// The people of a request as the chat reads them: the namespace they are written in (a 1:1 chat's, the account's; a
/// group's mode) and whether a number was left out of a group that hides them (fix F7).
struct Names<'a> {
    at: &'a Addressing<'a>,
    ns: Option<Namespace>,
    in_group: bool,
    withheld: Cell<bool>,
}

impl<'a> Names<'a> {
    fn new(conv: &ConvKey, at: &'a Addressing<'a>, group: Option<Namespace>) -> Self {
        Self {
            at,
            ns: match conv {
                ConvKey::Direct(_) => Some(at.direct_namespace()),
                _ => group,
            },
            // A person as the members of a GROUP read them (fix F7): never a phone number where numbers are hidden.
            in_group: matches!(conv, ConvKey::Group(_)),
            withheld: Cell::new(false),
        }
    }

    fn named(&self, user: &UserId) -> Result<Option<Jid>, Unaddressable> {
        member(self.at, user, self.ns, self.in_group, &self.withheld)
    }

    fn person(&self, user: &UserId) -> Option<Jid> {
        self.named(user).ok().flatten()
    }

    /// The context info of the message a request quotes, if it quotes one.
    fn quote(
        &self,
        conv: &ConvKey,
        reply_to: Option<&ReplyTo>,
    ) -> Result<Option<wa::ContextInfo>, Unbuildable> {
        let named = |user: &UserId| self.named(user);
        let person = |user: &UserId| self.person(user);
        reply_to
            .map(|quote| quote_info(conv, quote, self.at, self.ns, &named, &person))
            .transpose()
    }
}

/// What a file message says beside its file, whatever the file's pointer is.
struct FileSide<'a> {
    attachment: &'a Attachment,
    /// The RENDERED caption (none when it renders empty).
    caption: Option<String>,
    mentioned: Vec<String>,
    plain: bool,
    quote: Option<wa::ContextInfo>,
}

/// The refusals of a file message that the pointer does not decide (fix F4): not exactly one file, a quote naming another
/// chat, a quoted or mentioned author that does not parse; and the caption as it is written. The ONE place of these
/// rules: [`build`] calls it with the pointer in hand, [`check_file`] before the attempt touches the file.
fn file_side<'a>(
    attachments: &'a [Attachment],
    caption: Option<&Text>,
    quote: impl FnOnce() -> Result<Option<wa::ContextInfo>, Unbuildable>,
    person: &dyn Fn(&UserId) -> Option<Jid>,
) -> Result<FileSide<'a>, Unbuildable> {
    let attachment = match attachments {
        [] => return Err(Unbuildable::Empty),
        [one] => one,
        _ => return Err(Unbuildable::ManyFiles),
    };
    let (caption, mentioned, plain) = rendered_caption(caption, person);
    Ok(FileSide {
        attachment,
        caption,
        mentioned,
        plain,
        quote: quote()?,
    })
}

/// What the attempt learns of a file message before it reads, encrypts or uploads anything (fix F4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileCheck {
    /// The message carries a caption once rendered, which is what [`build`] writes: a document then goes inside its
    /// caption wrapper, and a file whose route would be audio (no caption there) goes as a document instead.
    pub(crate) captioned: bool,
}

/// The refusals of a file message that do not depend on its pointer, by the same function [`build`] uses (fix F4:
/// a refusal found after the upload would have cost a `w:m` IQ, a POST and an orphan on the CDN). `Ok` for any request
/// that is not a new message with a file (nothing to check before its file work).
pub(crate) fn check_file(
    req: &OutgoingRequest,
    at: &Addressing<'_>,
    group: Option<Namespace>,
) -> Result<FileCheck, Unbuildable> {
    let OutgoingKind::Message {
        body: Body::Media {
            attachments,
            caption,
        },
        reply_to,
        ..
    } = &req.kind
    else {
        return Ok(FileCheck { captioned: false });
    };
    let names = Names::new(&req.conv, at, group);
    let person = |user: &UserId| names.person(user);
    let side = file_side(
        attachments,
        caption.as_ref(),
        || names.quote(&req.conv, reply_to.as_ref()),
        &person,
    )?;
    Ok(FileCheck {
        captioned: side.caption.is_some(),
    })
}

fn is_own(author: &TargetAuthor, at: &Addressing<'_>) -> bool {
    match author {
        TargetAuthor::Own => true,
        TargetAuthor::User(user) => at.own.is_own(user),
        TargetAuthor::Unknown => false,
    }
}

/// A text as a message: `conversation` when nothing sits beside it, else `extendedTextMessage`.
fn text_message(rendered: Rendered, quote: Option<wa::ContextInfo>) -> wa::Message {
    let mut message = wa::Message::default();
    if quote.is_none() && rendered.mentioned.is_empty() {
        message.conversation = Some(rendered.text);
        return message;
    }
    let mut info = quote.unwrap_or_default();
    info.mentioned_jid = rendered.mentioned;
    let mut m = wa::message::ExtendedTextMessage::default();
    m.text = Some(rendered.text);
    m.context_info = MessageField::some(info);
    message.extended_text_message = MessageField::some(m);
    message
}

/// A caption as text: the rendered text (none when it is empty), the people it mentions, and whether formatting
/// was left out.
fn rendered_caption(
    caption: Option<&Text>,
    person: &dyn Fn(&UserId) -> Option<Jid>,
) -> (Option<String>, Vec<String>, bool) {
    match caption
        .map(|caption| render(caption, person))
        .filter(|r| !r.text.trim().is_empty())
    {
        Some(r) => (Some(r.text), r.mentioned, r.plain),
        None => (None, Vec::new(), false),
    }
}

/// The quote and the mentions of a caption, in the file's own context info.
fn set_context(message: &mut wa::Message, quote: Option<wa::ContextInfo>, mentioned: Vec<String>) {
    if quote.is_none() && mentioned.is_empty() {
        return;
    }
    let Some(field) = media::file_context(message) else {
        return;
    };
    let mut info = quote.unwrap_or_default();
    info.mentioned_jid = mentioned;
    *field = MessageField::some(info);
}

/// A person as the people of the chat read him (fix F7). In a GROUP never a phone number where numbers are hidden:
/// `Ok(None)` and `withheld` set, the caller leaves the mention or the participant out; in a 1:1 chat the plain rule.
fn member(
    at: &Addressing<'_>,
    user: &UserId,
    ns: Option<Namespace>,
    in_group: bool,
    withheld: &Cell<bool>,
) -> Result<Option<Jid>, Unaddressable> {
    let found = if in_group {
        at.public_person(user, ns)
    } else {
        at.person_in(user, ns)
    };
    match found {
        Ok(jid) => Ok(Some(jid)),
        Err(Unaddressable::NumberInLidGroup) => {
            withheld.set(true);
            Ok(None)
        }
        Err(other) => Err(other),
    }
}

/// The context info of a quote.
fn quote_info(
    conv: &ConvKey,
    quote: &ReplyTo,
    at: &Addressing<'_>,
    ns: Option<Namespace>,
    member: &dyn Fn(&UserId) -> Result<Option<Jid>, Unaddressable>,
    person: &dyn Fn(&UserId) -> Option<Jid>,
) -> Result<wa::ContextInfo, Unbuildable> {
    let target = &quote.target;
    if target.conv.as_ref().is_some_and(|stated| stated != conv) {
        return Err(Unaddressable::OtherConversation.into());
    }
    let mut info = wa::ContextInfo::default();
    info.stanza_id = Some(target.local.to_string());
    let author = match &target.author {
        TargetAuthor::Own => Some(at.own_in(ns.unwrap_or(Namespace::Lid))?),
        // A participant that would be a phone number in a group that hides numbers is left out (fix F7): the receiver
        // finds the message by its id.
        TargetAuthor::User(user) => member(user)?,
        // The quote states its author, or none is known: the receiver finds the message by its id.
        TargetAuthor::Unknown => quote.author.as_ref().map(member).transpose()?.flatten(),
    };
    info.participant = author.map(|jid| jid.to_string());
    if let Some(message) = quote.snapshot.as_ref().and_then(|s| quoted(s, person)) {
        info.quoted_message = MessageField::some(message);
    }
    Ok(info)
}

/// The quoted message as the snapshot shows it: its text (or the caption of its file), and the kind, type
/// and name of the file. `None` when the snapshot shows nothing.
fn quoted(
    snapshot: &QuoteSnapshot,
    person: &dyn Fn(&UserId) -> Option<Jid>,
) -> Option<wa::Message> {
    let text = snapshot
        .text
        .as_ref()
        .map(|text| render(text, person).text)
        .filter(|text| !text.is_empty());
    let mime = snapshot.mime.clone();
    let mut message = wa::Message::default();
    match snapshot.attachment {
        None => message.conversation = Some(text?),
        Some(AttachmentKind::Image) => {
            let mut m = wa::message::ImageMessage::default();
            m.mimetype = mime;
            m.caption = text;
            message.image_message = MessageField::some(m);
        }
        Some(kind @ (AttachmentKind::Video | AttachmentKind::Gif)) => {
            let mut m = wa::message::VideoMessage::default();
            m.mimetype = mime;
            m.caption = text;
            m.gif_playback = (kind == AttachmentKind::Gif).then_some(true);
            message.video_message = MessageField::some(m);
        }
        Some(kind @ (AttachmentKind::Audio | AttachmentKind::Voice)) => {
            let mut m = wa::message::AudioMessage::default();
            m.mimetype = mime;
            m.ptt = (kind == AttachmentKind::Voice).then_some(true);
            message.audio_message = MessageField::some(m);
        }
        Some(AttachmentKind::File) => {
            let mut m = wa::message::DocumentMessage::default();
            m.mimetype = mime;
            m.caption = text;
            m.file_name.clone_from(&snapshot.file_name);
            message.document_message = MessageField::some(m);
        }
    }
    Some(message)
}

// ---- the markup ---------------------------------------------------------------------------------------------

/// WhatsApp's markers, in the order pairs are opened (mono innermost: nothing is read inside it).
const MARKS: [char; 4] = ['*', '_', '~', '`'];
const MONO: usize = 3;

fn mark_of(kind: &RangeKind) -> Option<usize> {
    match kind {
        RangeKind::Bold => Some(0),
        RangeKind::Italic => Some(1),
        RangeKind::Strike => Some(2),
        RangeKind::Mono => Some(MONO),
        RangeKind::Spoiler | RangeKind::Mention(_) | RangeKind::MentionAll | RangeKind::Link(_) => {
            None
        }
    }
}

/// A text written for the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rendered {
    /// The text with WhatsApp's markers and `@<user>` for each mention.
    pub(crate) text: String,
    /// The mentioned people's ids, one per mention in the order of the text.
    pub(crate) mentioned: Vec<String>,
    /// Some formatting could not be written and was left out.
    pub(crate) plain: bool,
}

/// Intervals of one kind, merged where they touch.
fn merged(mut spans: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    spans.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
    for (start, end) in spans {
        match out.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => out.push((start, end)),
        }
    }
    out
}

/// `text` as WhatsApp's markup. `person` writes a mentioned person's id (`None`: the mention is left out).
pub(crate) fn render(text: &Text, person: &dyn Fn(&UserId) -> Option<Jid>) -> Rendered {
    let mut text = text.clone();
    text.sanitize();
    let body = text.body.as_str();
    let mentions: BTreeMap<usize, UserId> = text
        .shown_mentions()
        .into_iter()
        .filter_map(|range| match &range.kind {
            RangeKind::Mention(user) => Some((range.start as usize, user.clone())),
            _ => None,
        })
        .collect();

    // The text without markers as it goes on the wire, and as the receiver's parser gives it back.
    let mut wire = String::with_capacity(body.len());
    let mut expect = String::with_capacity(body.len());
    let mut to_wire = vec![0; body.len() + 1];
    let mut to_expect = vec![0; body.len() + 1];
    let mut mentioned = Vec::new();
    let mut handles: Vec<Mentioned> = Vec::new();
    let mut mention_starts = Vec::new();
    let mut expect_mentions = Vec::new();
    for (i, c) in body.char_indices() {
        to_wire[i] = wire.len();
        to_expect[i] = expect.len();
        if c != MENTION_PLACEHOLDER {
            wire.push(c);
            expect.push(c);
            continue;
        }
        let Some((user, jid)) = mentions
            .get(&i)
            .and_then(|user| Some((user, person(user)?)))
        else {
            continue;
        };
        mention_starts.push(wire.len());
        expect_mentions.push((expect.len(), user.clone()));
        wire.push('@');
        wire.push_str(&jid.user);
        expect.push(MENTION_PLACEHOLDER);
        mentioned.push(jid.to_string());
        handles.push(Mentioned {
            handle: jid.user.to_string(),
            user: user.clone(),
            me: false,
        });
    }
    to_wire[body.len()] = wire.len();
    to_expect[body.len()] = expect.len();

    // The formatting, as wire and expected intervals per marker.
    let mut spans: [Vec<(usize, usize)>; 4] = Default::default();
    let mut expected: [Vec<(usize, usize)>; 4] = Default::default();
    let mut left_out = false;
    for range in &text.ranges {
        let (Some(mark), Some(end)) = (mark_of(&range.kind), range.end()) else {
            continue;
        };
        let (start, end) = (range.start as usize, end as usize);
        let mut line_start = start;
        for (offset, c) in body[start..end]
            .char_indices()
            .chain(std::iter::once((end - start, '\n')))
        {
            if c != '\n' && c != '\r' {
                continue;
            }
            let line = &body[line_start..start + offset];
            let lead = line.len() - line.trim_start().len();
            let (a, b) = (line_start + lead, line_start + line.trim_end().len());
            line_start = start + offset + c.len_utf8();
            if a >= b {
                continue;
            }
            let (from, to) = (to_wire[a], to_wire[b]);
            let word_before = wire[..from]
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric);
            let word_after = wire[to..].chars().next().is_some_and(char::is_alphanumeric)
                || mention_starts.contains(&to);
            if from >= to || word_before || word_after {
                left_out = true;
                continue;
            }
            spans[mark].push((from, to));
            expected[mark].push((to_expect[a], to_expect[b]));
        }
    }
    let spans = spans.map(merged);
    if spans.iter().all(Vec::is_empty) {
        return Rendered {
            text: wire,
            mentioned,
            plain: left_out,
        };
    }

    // The markers: at every boundary close what ends (and what must close for it), open what starts.
    let mut cuts: Vec<usize> = spans.iter().flatten().flat_map(|(a, b)| [*a, *b]).collect();
    cuts.push(wire.len());
    cuts.sort_unstable();
    cuts.dedup();
    let active = |at: usize, mark: usize| spans[mark].iter().any(|(a, b)| *a <= at && at < *b);
    let mut out = String::with_capacity(wire.len() + 8 * cuts.len());
    let mut stack: Vec<usize> = Vec::new();
    let mut from = 0;
    for (index, &cut) in cuts.iter().enumerate() {
        out.push_str(&wire[from..cut]);
        from = cut;
        let want: Vec<usize> = (0..MARKS.len())
            .filter(|mark| cut < wire.len() && active(cut, *mark))
            .collect();
        let opens_outer = want
            .iter()
            .any(|mark| *mark != MONO && !stack.contains(mark));
        let keep = stack
            .iter()
            .take_while(|mark| want.contains(mark) && !(**mark == MONO && opens_outer))
            .count();
        if stack.len() > keep {
            // A pair cannot close after white space: the closers go before it.
            let tail = out.len() - out.trim_end().len();
            let space = out.split_off(out.len() - tail);
            while stack.len() > keep {
                if let Some(mark) = stack.pop() {
                    out.push(MARKS[mark]);
                }
            }
            out.push_str(&space);
        }
        if want.iter().any(|mark| !stack.contains(mark)) {
            // Nor open before white space: the openers go after it, and not at all in a stretch of it.
            let next = cuts.get(index + 1).copied().unwrap_or(wire.len());
            let stretch = &wire[cut..next];
            let lead = stretch.len() - stretch.trim_start().len();
            if lead == stretch.len() {
                continue;
            }
            out.push_str(&stretch[..lead]);
            from = cut + lead;
            for mark in want {
                if !stack.contains(&mark) {
                    out.push(MARKS[mark]);
                    stack.push(mark);
                }
            }
        }
    }

    // Read back with the receive map's own rule: the same text, the same formatting, the same mentions.
    let read = markup::mark_up(&out, &handles);
    let mut read_spans: [Vec<(usize, usize)>; 4] = Default::default();
    let mut read_mentions = Vec::new();
    for range in &read.text.ranges {
        let start = range.start as usize;
        match &range.kind {
            RangeKind::Mention(user) => read_mentions.push((start, user.clone())),
            kind => {
                if let Some(mark) = mark_of(kind) {
                    read_spans[mark].push((start, start + range.len as usize));
                }
            }
        }
    }
    read_mentions.sort();
    // Formatting on white space is not compared: a cut leaves the white space at it outside the pair.
    let ink = |spans: &Vec<(usize, usize)>| -> Vec<usize> {
        let mut at: Vec<usize> = spans
            .iter()
            .flat_map(|(a, b)| {
                expect
                    .get(*a..*b)
                    .unwrap_or_default()
                    .char_indices()
                    .filter(|(_, c)| !c.is_whitespace())
                    .map(move |(i, _)| a + i)
            })
            .collect();
        at.sort_unstable();
        at.dedup();
        at
    };
    let same = read.text.body == expect
        && read_spans.iter().map(ink).eq(expected.iter().map(ink))
        && read_mentions == expect_mentions;
    if same {
        Rendered {
            text: out,
            mentioned,
            plain: left_out,
        }
    } else {
        Rendered {
            text: wire,
            mentioned,
            plain: true,
        }
    }
}

#[cfg(test)]
mod tests;
