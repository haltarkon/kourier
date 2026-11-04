//! The body of a mapped message (W3b, chunks C4a to C4c): the text through WhatsApp's formatting, the file of a
//! media message as a remote pointer, a sticker. PURE: no I/O, no clock, no logging.
//!
//! [`shown`] says what a (peeled) message of a class looks like in the timeline. `None` means this build has
//! no body for it (the caller makes an "unsupported" message that keeps the payload): a class the table
//! names as content whose field is empty or whose value the contract cannot hold.

use kourier_model::{Body, ContactCard, Identifier, IdentifierKind, Location, Poll};
use wacore::proto_helpers::MessageExt as _;
use waproto::buffa::MessageField;
use waproto::whatsapp as wa;

use super::class::{self, Class, Content};
use super::{Ctx, context};
use crate::media;

/// What a message shows, whether it mentions us, and what it keeps beside its body.
pub(super) struct Shown {
    pub(super) body: Body,
    pub(super) mentions_me: bool,
    /// The encoded `wa::Message` for the event's `raw` ([`kept_context`]), when there is something to keep.
    pub(super) keep: Option<Vec<u8>>,
}

/// The body of a (peeled) message of class `class`; `info` is the message's own context info.
pub(super) fn shown(
    outer: &wa::Message,
    base: &wa::Message,
    class: Class,
    info: Option<&wa::ContextInfo>,
    ctx: &Ctx,
) -> Option<Shown> {
    // A document the phone sent with a caption came inside its wrapper: the pointer remembers it (fix F11).
    let wrapped =
        class == Class::Content(Content::Document) && class::through_caption_wrapper(outer);
    let mut shown = shown_body(base, class, info, ctx, wrapped)?;
    shown.keep = kept_context(outer, base, class);
    Some(shown)
}

fn shown_body(
    base: &wa::Message,
    class: Class,
    info: Option<&wa::ContextInfo>,
    ctx: &Ctx,
    wrapped: bool,
) -> Option<Shown> {
    match class {
        Class::Text => {
            let text = base.text_content().filter(|t| !t.is_empty())?;
            let marked = context::marked(text, info, ctx);
            Some(Shown {
                body: Body::Text(marked.text),
                mentions_me: marked.mentions_me,
                keep: None,
            })
        }
        Class::Content(content) => content_body(base, content, info, ctx, wrapped),
        _ => None,
    }
}

fn content_body(
    base: &wa::Message,
    content: Content,
    info: Option<&wa::ContextInfo>,
    ctx: &Ctx,
    wrapped: bool,
) -> Option<Shown> {
    match content {
        Content::Image
        | Content::Video
        | Content::VideoNote
        | Content::Audio
        | Content::Document => {
            let file = media::file_in(base, content, wrapped)?;
            let caption = file.caption.map(|c| context::marked(&c, info, ctx));
            Some(Shown {
                mentions_me: caption.as_ref().is_some_and(|c| c.mentions_me),
                keep: None,
                body: Body::Media {
                    attachments: vec![file.attachment],
                    caption: caption.map(|c| c.text),
                },
            })
        }
        Content::Sticker => {
            let file = media::file_of(base, content)?;
            Some(Shown {
                body: Body::Sticker(Box::new(file.attachment)),
                mentions_me: false,
                keep: None,
            })
        }
        Content::Contact => contact(base).map(plain),
        Content::Location | Content::LiveLocation => location(base).map(plain),
        Content::Poll => poll(base).map(plain),
    }
}

/// A body that carries no text of ours: it mentions nobody.
fn plain(body: Body) -> Shown {
    Shown {
        body,
        mentions_me: false,
        keep: None,
    }
}

/// What a mapped message keeps in `raw` (the encoded `wa::Message` holding only a trimmed
/// `messageContextInfo` of the OUTER message): the album association of a child (its parent and place, for
/// the album grid of K-B3 C5) and the secret of a poll (the votes of K8a are encrypted with it). Everything
/// else the context info carries is dropped, and a message with neither keeps nothing.
fn kept_context(outer: &wa::Message, base: &wa::Message, class: Class) -> Option<Vec<u8>> {
    let context = outer
        .message_context_info
        .as_option()
        .or_else(|| base.message_context_info.as_option())?;
    let association = context.message_association.as_option();
    let secret = (class == Class::Content(Content::Poll))
        .then(|| context.message_secret.clone())
        .flatten()
        .filter(|secret| !secret.is_empty());
    if association.is_none() && secret.is_none() {
        return None;
    }
    let mut kept = wa::MessageContextInfo::default();
    if let Some(association) = association {
        kept.message_association = MessageField::some(association.clone());
    }
    kept.message_secret = secret;
    let mut message = wa::Message::default();
    message.message_context_info = MessageField::some(kept);
    Some(waproto::codec::message_to_vec(&message))
}

/// The longest place name, address, contact name, organisation or poll text kept.
const MAX_TEXT: usize = 1024;
/// The longest poll question kept.
const MAX_QUESTION: usize = 2048;
/// Most poll options kept (WhatsApp allows 12).
const MAX_OPTIONS: usize = 32;
/// Most phone numbers of one card kept.
const MAX_IDENTIFIERS: usize = 20;
/// The longest vCard read.
const MAX_VCARD: usize = 16 * 1024;

/// A string as a message states it: kept when it is not empty and not longer than `max` bytes.
fn kept(text: Option<&str>, max: usize) -> Option<String> {
    text.filter(|t| !t.trim().is_empty() && t.len() <= max)
        .map(str::to_owned)
}

/// A shared contact: the card's `FN`, `ORG` and `TEL` lines (the model's card has no more). The name is the
/// vCard's `FN`, else the `displayName` the sender's chat showed. A card with neither a name nor a number is
/// no card.
fn contact(base: &wa::Message) -> Option<Body> {
    let m = base.contact_message.as_option()?;
    let card = vcard(m.vcard.as_deref().unwrap_or_default());
    let name = card
        .name
        .or_else(|| kept(m.display_name.as_deref(), MAX_TEXT));
    if name.is_none() && card.identifiers.is_empty() {
        return None;
    }
    Some(Body::Contact(ContactCard {
        name,
        organization: card.organization,
        identifiers: card.identifiers,
        avatar: None,
    }))
}

#[derive(Default)]
struct Card {
    name: Option<String>,
    organization: Option<String>,
    identifiers: Vec<Identifier>,
}

/// vCard text unescaped (`\n`, `\,`, `\;`, `\\`).
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// A phone number as the card states it, in the form the model wants (E.164 with its `+`): the `waid`
/// parameter WhatsApp adds is the digits of the number, else the value's own `+` and digits. A value with
/// fewer than 3 digits is no number.
fn phone(params: &[&str], value: &str) -> Option<String> {
    let waid = params.iter().find_map(|p| {
        let (key, digits) = p.split_once('=')?;
        (key.eq_ignore_ascii_case("waid") && digits.bytes().all(|b| b.is_ascii_digit()))
            .then_some(digits)
    });
    let digits: String = match waid {
        Some(waid) if !waid.is_empty() => waid.to_owned(),
        _ => value.chars().filter(char::is_ascii_digit).collect(),
    };
    if digits.len() < 3 || digits.len() > 20 {
        return None;
    }
    let plus = waid.is_some_and(|w| !w.is_empty()) || value.trim_start().starts_with('+');
    Some(if plus { format!("+{digits}") } else { digits })
}

/// The lines of a vCard that matter. Never panics; reads at most [`MAX_VCARD`] bytes.
fn vcard(text: &str) -> Card {
    let mut card = Card::default();
    let end = (0..=text.len().min(MAX_VCARD))
        .rev()
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(0);
    for line in text[..end].lines() {
        let Some((head, value)) = line.split_once(':') else {
            continue;
        };
        let mut parts = head.split(';');
        // A group prefix (`item1.TEL`) is not part of the property's name.
        let name = parts.next().unwrap_or_default();
        let name = name.rsplit_once('.').map_or(name, |(_, n)| n);
        let params: Vec<&str> = parts.collect();
        if name.eq_ignore_ascii_case("FN") {
            card.name = card
                .name
                .take()
                .or_else(|| kept(Some(&unescape(value)), MAX_TEXT));
        } else if name.eq_ignore_ascii_case("ORG") {
            let first = value.split(';').next().unwrap_or_default();
            card.organization = card
                .organization
                .take()
                .or_else(|| kept(Some(&unescape(first)), MAX_TEXT));
        } else if name.eq_ignore_ascii_case("TEL")
            && card.identifiers.len() < MAX_IDENTIFIERS
            && let Some(number) = phone(&params, value)
        {
            let identifier = Identifier {
                kind: IdentifierKind::Phone,
                value: number,
            };
            if !card.identifiers.contains(&identifier) {
                card.identifiers.push(identifier);
            }
        }
    }
    card
}

/// Degrees as a model coordinate (degrees x 10^7), `None` for a value outside the globe or not a number.
fn degrees_e7(degrees: Option<f64>, limit: f64) -> Option<i32> {
    let degrees = degrees.filter(|d| d.is_finite() && d.abs() <= limit)?;
    // |degrees| <= 180 gives |e7| <= 1.8e9, inside `i32`.
    Some((degrees * 1e7).round() as i32)
}

/// A place: a location message, or the last position of a live location (no live updates; its caption is
/// the place's name). A position outside the globe is no location.
fn location(base: &wa::Message) -> Option<Body> {
    let (lat, lon, name, address) = if let Some(m) = base.location_message.as_option() {
        (
            m.degrees_latitude,
            m.degrees_longitude,
            kept(m.name.as_deref(), MAX_TEXT),
            kept(m.address.as_deref(), MAX_TEXT),
        )
    } else {
        let m = base.live_location_message.as_option()?;
        (
            m.degrees_latitude,
            m.degrees_longitude,
            kept(m.caption.as_deref(), MAX_TEXT),
            None,
        )
    };
    Some(Body::Location(Location {
        lat_e7: degrees_e7(lat, 90.0)?,
        lon_e7: degrees_e7(lon, 180.0)?,
        name,
        address,
    }))
}

/// A poll creation (versions 1 to 3, 5 and 6; the 4 wrapper is peeled): the question, the options, and whether
/// more than one may be chosen (`selectableOptionsCount` 1 is the single choice; 0 is "any number"). A poll
/// with no question or no option is no poll. The votes are another message kind (K8a).
fn poll(base: &wa::Message) -> Option<Body> {
    let m = [
        &base.poll_creation_message,
        &base.poll_creation_message_v2,
        &base.poll_creation_message_v3,
        &base.poll_creation_message_v5,
        &base.poll_creation_message_v6,
    ]
    .into_iter()
    .find_map(|field| field.as_option())?;
    let question = kept(m.name.as_deref(), MAX_QUESTION)?;
    let options: Vec<String> = m
        .options
        .iter()
        .filter_map(|o| kept(o.option_name.as_deref(), MAX_TEXT))
        .take(MAX_OPTIONS)
        .collect();
    if options.is_empty() {
        return None;
    }
    Some(Body::Poll(Poll {
        question,
        options,
        multiple: m.selectable_options_count != Some(1),
    }))
}
