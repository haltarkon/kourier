//! Telegram's text entities -> Kourier's `Text` (TG-3 C3, brief §3.4; architecture §3.6).
//!
//! PURE: no I/O, no clock, no logging, no panic on any input. The input is a `formattedText` (a
//! message text or a media caption) and who we are; the output is a [`Marked`]: a Kourier
//! [`Text`](kourier_model::Text) whose ranges are UTF-8 byte offsets, whether the text mentions us,
//! and how many entities were left out.
//!
//! The rules:
//! - TDLib's offsets and lengths are UTF-16 code units (`td_api.tl:110`). They are converted once
//!   with the text's [`Utf16Map`]; an entity that starts or ends inside a surrogate pair, past the end,
//!   at a negative offset or with no length is DROPPED (the text stays as it is).
//! - Bold, Italic, Strikethrough and Spoiler keep their kind; Code, Pre and PreCode are `Mono` (a
//!   language is dropped).
//! - `textEntityTypeTextUrl` (a hidden link) is `Link(url)` for an `http`/`https` url, else plain.
//!   Decision L-10 (G-4) made it plain UNLESS the UI shows the target before opening: it does
//!   (`kourier-qt/qml/timeline/blocks/TextBlock.qml`: a link's click opens the "Open Link?" dialog
//!   whose body is the url as plain text, and only that dialog's Open opens it; the viewmodel
//!   renders an href only for `http`, `https`, `sgnl`, `kourier-viewmodel/src/richtext.rs`).
//! - `textEntityTypeUrl` is `Link(<the span's text>)` when the span is an `http://` or `https://` url;
//!   a bare domain stays plain.
//! - `textEntityTypeMentionName(user_id)` REPLACES its span by ONE [`MENTION_PLACEHOLDER`] covered by
//!   `Mention(tg:<user_id>)` (the WhatsApp rule: the UI shows Kourier's name of that person). Of two
//!   overlapping mentions the first in the text wins; the other stays text. Another entity that ends
//!   or starts INSIDE a replaced span is dropped; one that covers a whole span covers its placeholder.
//! - `textEntityTypeMention` (`@username`) stays plain text (it names no user id); it mentions us when
//!   its username is one of ours (case-insensitive).
//! - Underline, the quotes, CustomEmoji (its fallback emoji is the span's text), Hashtag, Cashtag,
//!   BotCommand, EmailAddress, PhoneNumber, BankCardNumber, TonAddress, MediaTimestamp, DateTime and
//!   any type this build does not know: plain (shared C10 / T8 add kinds).
//! - Nested and overlapping entities are kept: Kourier's ranges may overlap. The result passes
//!   [`Text::sanitize`](kourier_model::Text::sanitize) unchanged (it is run as the last step anyway).

use kourier_model::text::{MENTION_PLACEHOLDER, Utf16Map};
use kourier_model::{RangeKind, TextRange, UserId};

use crate::tdapi::{FormattedText, TextEntity, TextEntityType};

/// Who we are, for `mentions_me`. `Debug` prints the id and how many usernames, never a username
/// (TF16).
#[derive(Clone, Copy)]
pub(crate) struct Me<'a> {
    /// Our Telegram user id (`my_id`).
    pub(crate) id: i64,
    /// Our active usernames (without `@`).
    pub(crate) usernames: &'a [String],
}

impl std::fmt::Debug for Me<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Me")
            .field("id", &self.id)
            .field("usernames", &self.usernames.len())
            .finish()
    }
}

/// A text with its entities read. `Debug` prints no text (the model's `Text` hides its body and
/// links).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct Marked {
    pub(crate) text: kourier_model::Text,
    /// A mention of our user id, or an `@` of one of our usernames.
    pub(crate) mentions_me: bool,
    /// Entities left out (outside the text, inside a surrogate pair or a mention's span, a mention of
    /// no valid user id).
    pub(crate) dropped: usize,
}

/// One mention that replaces its span: UTF-8 bytes of the INPUT text.
struct Replaced {
    start: usize,
    end: usize,
    user: UserId,
}

/// Reads `formatted`'s entities.
pub(crate) fn mark(formatted: &FormattedText, me: Me<'_>) -> Marked {
    let input = formatted.text.as_str();
    let map = Utf16Map::new(input);
    let mut dropped = 0;
    let mut mentions_me = false;

    // Every entity with its span in UTF-8 bytes of the input; a span that does not lie on character
    // boundaries is dropped here.
    let mut spans: Vec<(usize, usize, &TextEntity)> = Vec::new();
    for entity in &formatted.entities {
        match utf8_span(&map, entity) {
            Some((s, e)) => spans.push((s, e, entity)),
            None => dropped += 1,
        }
    }

    // The mentions that replace their span, first in the text first, none overlapping.
    let mut candidates: Vec<(usize, usize, i64)> = spans
        .iter()
        .filter_map(|(s, e, entity)| match entity.r#type {
            TextEntityType::MentionName { user_id } => Some((*s, *e, user_id)),
            _ => None,
        })
        .collect();
    candidates.sort_by_key(|(s, e, _)| (*s, *e));
    let mut replaced: Vec<Replaced> = Vec::new();
    for (start, end, user_id) in candidates {
        if user_id == me.id {
            mentions_me = true;
        }
        let user = (user_id > 0)
            .then(|| UserId::from_parts("tg", &user_id.to_string()).ok())
            .flatten();
        match user {
            Some(user) if replaced.last().is_none_or(|r| r.end <= start) => {
                replaced.push(Replaced { start, end, user });
            }
            // Overlaps an earlier mention, or names no user: its span stays text.
            _ => dropped += 1,
        }
    }

    // The output body: the input with each replaced span as one placeholder.
    let mut body = String::with_capacity(input.len());
    let mut at = 0;
    for r in &replaced {
        body.push_str(&input[at..r.start]);
        body.push(MENTION_PLACEHOLDER);
        at = r.end;
    }
    body.push_str(&input[at..]);

    let mut ranges = Vec::new();
    for (s, e, entity) in &spans {
        if matches!(entity.r#type, TextEntityType::MentionName { .. }) {
            continue;
        }
        if let TextEntityType::Mention = entity.r#type {
            let name = input[*s..*e].trim_start_matches('@');
            if !name.is_empty() && me.usernames.iter().any(|u| u.eq_ignore_ascii_case(name)) {
                mentions_me = true;
            }
        }
        let Some(kind) = kind_of(&entity.r#type, &input[*s..*e]) else {
            continue;
        };
        match (moved(&replaced, *s), moved(&replaced, *e)) {
            (Some(start), Some(end)) if start < end => ranges.push(range(start, end, kind)),
            _ => dropped += 1,
        }
    }
    let mut out = 0usize;
    let mut at = 0usize;
    for r in &replaced {
        out += r.start - at;
        let len = MENTION_PLACEHOLDER.len_utf8();
        ranges.push(range(out, out + len, RangeKind::Mention(r.user.clone())));
        out += len;
        at = r.end;
    }

    let mut text = kourier_model::Text { body, ranges };
    dropped += text.sanitize();
    Marked {
        text,
        mentions_me,
        dropped,
    }
}

/// An entity's span in UTF-8 bytes of the text, or `None` when it is empty, negative, past the end
/// or starts or ends inside a surrogate pair.
fn utf8_span(map: &Utf16Map, entity: &TextEntity) -> Option<(usize, usize)> {
    let offset = u32::try_from(entity.offset).ok()?;
    let length = u32::try_from(entity.length).ok().filter(|l| *l > 0)?;
    let (start, len) = map.range_to_utf8(offset, length)?;
    Some((start as usize, (start + len) as usize))
}

/// Where a byte offset of the input lies in the output body: `None` strictly inside a replaced span.
/// Each span before it is one placeholder in the output (longer or shorter than the span).
fn moved(replaced: &[Replaced], at: usize) -> Option<usize> {
    let mut removed = 0usize;
    let mut placeholders = 0usize;
    for r in replaced {
        if at >= r.end {
            removed += r.end - r.start;
            placeholders += MENTION_PLACEHOLDER.len_utf8();
        } else if at > r.start {
            return None;
        } else {
            break;
        }
    }
    // `removed` <= `at`: every span counted ends at or before `at`.
    Some(at - removed + placeholders)
}

/// The Kourier kind of an entity (`span` = its text), `None` for one that stays plain.
fn kind_of(ty: &TextEntityType, span: &str) -> Option<RangeKind> {
    Some(match ty {
        TextEntityType::Bold => RangeKind::Bold,
        TextEntityType::Italic => RangeKind::Italic,
        TextEntityType::Strikethrough => RangeKind::Strike,
        TextEntityType::Spoiler => RangeKind::Spoiler,
        TextEntityType::Code | TextEntityType::Pre | TextEntityType::PreCode { .. } => {
            RangeKind::Mono
        }
        TextEntityType::TextUrl { url } => RangeKind::Link(web_url(url.as_str())?.to_owned()),
        TextEntityType::Url => RangeKind::Link(web_url(span)?.to_owned()),
        _ => return None,
    })
}

/// `url` when it is an `http://` or `https://` url with something after the scheme and no white
/// space or control character.
fn web_url(url: &str) -> Option<&str> {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    let rest = if lower.starts_with("https://") {
        &url[8..]
    } else if lower.starts_with("http://") {
        &url[7..]
    } else {
        return None;
    };
    let clean = !rest.is_empty() && !url.chars().any(|c| c.is_whitespace() || c.is_control());
    clean.then_some(url)
}

fn range(start: usize, end: usize, kind: RangeKind) -> TextRange {
    TextRange {
        start: u32::try_from(start).unwrap_or(u32::MAX),
        len: u32::try_from(end - start).unwrap_or(0),
        kind,
    }
}

#[cfg(test)]
mod tests;
