//! The CSP E2E message BODIES a reflection carries (TH5 A6): pure parsers over bytes, no I/O, no clock,
//! no logging. A reflected body is "the unpadded padded-data" (`md-d2d.proto`), so there is no crypto and
//! no padding to strip here. What is parsed in TH5a:
//!
//! - the two group containers (`csp.struct.yml` `group-member-container`: creator 8 bytes, group id
//!   `u64-le`, inner data; `group-creator-container`: group id, inner data);
//! - a TEXT: UTF-8 (a body that is not UTF-8 is refused, as Desktop's fatal decoder does), with the QUOTE
//!   prefix and the MENTIONS in the forms Desktop writes and reads (`task/common/quotes.ts`,
//!   `utils/mentions.ts`; the pinned spec describes neither, so both are `[I]`, [U: real phone] that
//!   Android and iOS write the same):
//!   - quote: `> quote #<16 lower-case hex>` + exactly two line breaks (`\n` or `\r\n` each) + the comment.
//!     The 16 digits are the quoted message id's eight wire bytes in order (`msg_id`'s text). A prefix that
//!     does not match, or that quotes the message itself, leaves the text WHOLE.
//!   - mention: `@[XXXXXXXX]` (an ID: `[A-Z0-9*][A-Z0-9]{7}`) becomes ONE [`MENTION_PLACEHOLDER`] with a
//!     [`RangeKind::Mention`] range; `@[@@@@@@@@]` becomes a [`RangeKind::MentionAll`]. At most
//!     [`MAX_MENTIONS`] per text (our number, `[I]`); the rest stay plain text.
//!
//! TH5b B1 adds (decisions R3, R4, R5):
//!
//! - MARKUP (`*bold*`, `_italic_`, `~strike~`; [`markup`]) on every text, caption and edit, after the quote and
//!   the mentions. Threema Desktop's own parser is not on disk, so the rule is ours (`[I]`, [U: real phone]):
//!   an OPENER is a marker at the start or after a character that is not alphanumeric and is followed by a
//!   character that is not white space; its CLOSER is the next same marker on the same line that follows a
//!   character that is not white space and is followed by the end or a character that is not alphanumeric;
//!   something must lie between. Spans of different markers nest (at most [`MAX_DEPTH`] levels); a marker with
//!   no partner stays text. The markers are removed, the inner text gets the range, the mention ranges move
//!   with the removed bytes.
//! - A LOCATION (`e2e.location`): `<lat>,<lon>[,<accuracy>]` then an address, or a name and an address
//!   ([`location`]); anything else is `None` and the map keeps the body raw as an Unsupported `location`.
//! - A FILE (`e2e.file`, a JSON object with the spec's one-letter keys; [`file`]): the rendering type, the
//!   media type and the metadata give the attachment's kind, size, name, dimensions and duration; the JSON
//!   itself is the remote pointer. `None` for JSON that does not parse or lacks `b`, `k`, `m` or `s`.
//!   FIX-P (P-L3, Desktop `task/common/file.ts`): the animated flag `x.a` is image metadata and makes a GIF
//!   of an `image/*` type only, and rendering type 2 is a sticker only for an `image/*` type; under any other
//!   type it is read as rendering 1 (a video, a sound) or is a file.
//!
//! TH5b B2 adds the three MODIFIERS (decisions R6, R7, R8; `csp-e2e.proto`): an `EditMessage` ([`edit`]: the
//! target and the new text, read like a text through the quote prefix, the mentions and the markup), a
//! `DeleteMessage` ([`delete`]) and a `Reaction` ([`reaction`]: apply or withdraw of one emoji of 1 to 64 bytes
//! of UTF-8). Each is a protobuf; one that does not decode is refused by its own rule.
//!
//! A refusal is a rule NAME (`&'static str`), never the input; the map drops the message by it.

use kourier_model::text::MENTION_PLACEHOLDER;
use kourier_model::{
    Attachment, AttachmentKind, AttachmentState, BackendBlob, Location, RangeKind, Text, TextRange,
    UserId,
};
use libthreema::common::MessageId;
use libthreema::protobuf::csp_e2e;
use prost::Message as _;
use serde_json::Value;

use crate::msg_id;

/// The most mentions turned into ranges in one text (`[I]`: our number; the spec has none).
pub(crate) const MAX_MENTIONS: usize = 64;

/// Refusal: a member container shorter than its 16 header bytes.
pub(crate) const RULE_CONTAINER_SHORT: &str = "container-short";
/// Refusal: a container whose creator is no Threema ID.
pub(crate) const RULE_CONTAINER_CREATOR: &str = "container-creator";
/// Refusal: a text that is not UTF-8.
pub(crate) const RULE_TEXT_UTF8: &str = "text-utf8";
/// Refusal: a text with nothing to show (empty, or a quote with an empty comment).
pub(crate) const RULE_TEXT_EMPTY: &str = "text-empty";

/// Refusal (R6): an `EditMessage` that does not decode.
pub(crate) const RULE_EDIT_INVALID: &str = "edit-invalid";
/// Refusal (R7): a `DeleteMessage` that does not decode.
pub(crate) const RULE_DELETE_INVALID: &str = "delete-invalid";
/// Refusal (R8): a `Reaction` that does not decode or sets no action.
pub(crate) const RULE_REACTION_INVALID: &str = "reaction-invalid";
/// Refusal (R8): a reaction emoji that is empty, over 64 bytes or not UTF-8 (the spec's receive steps).
pub(crate) const RULE_REACTION_BYTES: &str = "reaction-bytes";

/// The most bytes of a reaction's emoji (`csp-e2e.proto` Common Reaction Receive Steps 4.1 and 5.1).
pub(crate) const MAX_REACTION_BYTES: usize = 64;

/// Refusal (R9): a delivery receipt with no message id, or whose ids are not whole 8-byte ids.
pub(crate) const RULE_RECEIPT_SHORT: &str = "receipt-short";

/// The prefix of a quote (Desktop's `serializeQuoteText`: `> quote #<id>\n\n<comment>`).
const QUOTE_PREFIX: &str = "> quote #";
/// The digits of a quoted message id.
const ID_DIGITS: usize = 2 * MessageId::LENGTH;
/// What a mention opens and closes with.
const MENTION_OPEN: &str = "@[";
/// The identity text of `@All`.
const EVERYONE: &str = "@@@@@@@@";
/// A Threema ID has eight characters.
const IDENTITY_LEN: usize = 8;
/// The version of an attachment's `remote` blob (`BackendBlob::v`): the file message's JSON as received.
const POINTER_VERSION: u16 = 1;
/// The longest file name kept, in bytes (`csp.struct.yml` file: a sender never writes more than 256).
const MAX_NAME: usize = 256;
/// The deepest nesting of markup spans: bounds the recursion (WhatsApp's bound, decision R5).
const MAX_DEPTH: usize = 6;
/// The scheme of a Threema user.
const SCHEME: &str = "threema";

/// A group as a container header names it.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct GroupRef {
    /// The creator's Threema ID (personal data: no `Debug`).
    pub(crate) creator: String,
    /// The group id (the container's `u64-le`, the same value as a `fixed64`).
    pub(crate) group_id: u64,
}

impl std::fmt::Debug for GroupRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GroupRef { .. }")
    }
}

/// Whether `id` is the text of a Threema ID (libthreema's rule: eight characters, the first one of
/// `*`, `0-9`, `A-Z`, the others of `0-9`, `A-Z`).
pub(crate) fn is_identity(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == IDENTITY_LEN
        && bytes
            .first()
            .is_some_and(|b| *b == b'*' || b.is_ascii_digit() || b.is_ascii_uppercase())
        && bytes
            .iter()
            .skip(1)
            .all(|b| b.is_ascii_digit() || b.is_ascii_uppercase())
}

/// `group-member-container`: creator (8 bytes), group id (`u64-le`), the inner data. Refused:
/// [`RULE_CONTAINER_SHORT`] (fewer than 16 bytes), [`RULE_CONTAINER_CREATOR`] (not an ID).
pub(crate) fn member_container(body: &[u8]) -> Result<(GroupRef, &[u8]), &'static str> {
    let (creator, rest) = body
        .split_first_chunk::<IDENTITY_LEN>()
        .ok_or(RULE_CONTAINER_SHORT)?;
    let (group_id, inner) = rest.split_first_chunk::<8>().ok_or(RULE_CONTAINER_SHORT)?;
    let creator = std::str::from_utf8(creator).map_err(|_| RULE_CONTAINER_CREATOR)?;
    if !is_identity(creator) {
        return Err(RULE_CONTAINER_CREATOR);
    }
    Ok((
        GroupRef {
            creator: creator.to_owned(),
            group_id: u64::from_le_bytes(*group_id),
        },
        inner,
    ))
}

/// `group-creator-container`: group id (`u64-le`), the inner data. Refused: [`RULE_CONTAINER_SHORT`].
/// (TH5a drops every message that uses it; A7's group sync and TH6's leader receive read it.)
#[allow(
    dead_code,
    reason = "TH5a drops the group control messages that use it; tested, used from TH6"
)]
pub(crate) fn creator_container(body: &[u8]) -> Result<(u64, &[u8]), &'static str> {
    let (group_id, inner) = body.split_first_chunk::<8>().ok_or(RULE_CONTAINER_SHORT)?;
    Ok((u64::from_le_bytes(*group_id), inner))
}

/// A parsed text message.
#[derive(Clone, PartialEq)]
pub(crate) struct ParsedText {
    /// The text to show (quote prefix removed, mentions turned into placeholders and ranges).
    pub(crate) text: Text,
    /// The quoted message's id, when the text started with a valid quote of ANOTHER message.
    pub(crate) quoted: Option<MessageId>,
    /// The own ID is among the mentions, or `@All` was written in a group.
    pub(crate) mentions_me: bool,
}

impl std::fmt::Debug for ParsedText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParsedText")
            .field("text", &self.text)
            .field("quoted", &self.quoted.is_some())
            .field("mentions_me", &self.mentions_me)
            .finish()
    }
}

/// The text of a `text` body (module doc). `this` is the message's own id (a quote of itself is no
/// quote), `own` the own Threema ID, `in_group` whether `@All` counts as a mention of the user.
/// Refused: [`RULE_TEXT_UTF8`], [`RULE_TEXT_EMPTY`].
pub(crate) fn text(
    body: &[u8],
    this: MessageId,
    own: &str,
    in_group: bool,
) -> Result<ParsedText, &'static str> {
    let whole = std::str::from_utf8(body).map_err(|_| RULE_TEXT_UTF8)?;
    let (quoted, comment) = match split_quote(whole) {
        Some((id, comment)) if id != this => (Some(id), comment),
        _ => (None, whole),
    };
    if comment.is_empty() {
        return Err(RULE_TEXT_EMPTY);
    }
    let (text, mentions_me) = rich(comment, own, in_group);
    Ok(ParsedText {
        text,
        quoted,
        mentions_me,
    })
}

/// A piece of text with its mentions and its markup read (a text after the quote, a caption, an edit).
pub(crate) fn rich(comment: &str, own: &str, in_group: bool) -> (Text, bool) {
    let (text, mine) = mentions(comment, own, in_group);
    (markup(text), mine)
}

/// `> quote #<16 hex>` + two line breaks + the comment (`quotes.ts`'s regular expression), or `None`.
fn split_quote(text: &str) -> Option<(MessageId, &str)> {
    let rest = text.strip_prefix(QUOTE_PREFIX)?;
    let digits = rest.get(..ID_DIGITS)?;
    // `parse` takes only lower-case hex digits, exactly sixteen: one text per id.
    let id = msg_id::parse(digits)?;
    let mut rest = rest.get(ID_DIGITS..)?;
    for _ in 0..2 {
        rest = rest.strip_prefix('\r').unwrap_or(rest);
        rest = rest.strip_prefix('\n')?;
    }
    Some((id, rest))
}

/// Turns the mentions of `comment` into placeholders and ranges (module doc); whether the user is named.
fn mentions(comment: &str, own: &str, in_group: bool) -> (Text, bool) {
    let mut body = String::with_capacity(comment.len());
    let mut ranges = Vec::new();
    let mut mine = false;
    let mut rest = comment;
    while let Some(at) = rest.find(MENTION_OPEN) {
        let (before, from) = rest.split_at(at);
        let identity = from
            .get(MENTION_OPEN.len()..MENTION_OPEN.len() + IDENTITY_LEN)
            .filter(|_| {
                from.get(MENTION_OPEN.len() + IDENTITY_LEN..)
                    .is_some_and(|after| after.starts_with(']'))
            });
        let kind = identity
            .filter(|_| ranges.len() < MAX_MENTIONS)
            .and_then(|id| {
                if id == EVERYONE {
                    mine |= in_group;
                    Some(RangeKind::MentionAll)
                } else if is_identity(id) {
                    mine |= id == own;
                    UserId::from_parts(SCHEME, id).ok().map(RangeKind::Mention)
                } else {
                    None
                }
            });
        body.push_str(before);
        match kind {
            Some(kind) => {
                let start = u32::try_from(body.len()).unwrap_or(u32::MAX);
                body.push(MENTION_PLACEHOLDER);
                ranges.push(TextRange {
                    start,
                    len: u32::try_from(MENTION_PLACEHOLDER.len_utf8()).unwrap_or(3),
                    kind,
                });
                rest = &from[MENTION_OPEN.len() + IDENTITY_LEN + 1..];
            }
            None => {
                // Not a mention (or one too many): the two characters stay and the scan goes on after them.
                body.push_str(MENTION_OPEN);
                rest = &from[MENTION_OPEN.len()..];
            }
        }
    }
    body.push_str(rest);
    (Text { body, ranges }, mine)
}

// ---------------------------------------------------------------------------------------------------------
// MARKUP (decision R5)
// ---------------------------------------------------------------------------------------------------------

/// One position of a text before its markup is read: a character, or the placeholder of a mention (its
/// range is carried over).
#[derive(Clone, Copy)]
enum Atom {
    Ch(char),
    /// The placeholder of the mention range with this index.
    Mention(usize),
}

impl Atom {
    fn is_space(self) -> bool {
        matches!(self, Self::Ch(c) if c.is_whitespace())
    }

    /// A letter or a digit. A mention's placeholder is none (its text ends in `]`, as Desktop writes it).
    fn is_alphanumeric(self) -> bool {
        matches!(self, Self::Ch(c) if c.is_alphanumeric())
    }

    fn is_line_break(self) -> bool {
        matches!(self, Self::Ch('\n' | '\r'))
    }
}

/// The markers: bold, italic, strike.
const MARKERS: [char; 3] = ['*', '_', '~'];

fn marker_of(atom: Atom) -> Option<usize> {
    match atom {
        Atom::Ch(c) => MARKERS.iter().position(|m| *m == c),
        Atom::Mention(_) => None,
    }
}

fn kind_of(marker: usize) -> RangeKind {
    match marker {
        0 => RangeKind::Bold,
        1 => RangeKind::Italic,
        _ => RangeKind::Strike,
    }
}

/// Reads the markup of `text` (module doc). `text` carries MENTION ranges only (what [`mentions`] makes):
/// the result has the markers removed, the format ranges added and the mention ranges moved to where their
/// placeholders now are, all sorted by start (longer first). A text with no marker character is returned as
/// it is.
pub(crate) fn markup(text: Text) -> Text {
    markup_from(text, 0)
}

/// [`markup`] with the nesting already `depth` levels deep (the test of the bound: three markers cannot
/// nest deeper than three levels by the closer rule, so the bound is exercised from a start depth).
fn markup_from(text: Text, depth: usize) -> Text {
    if !text.body.contains(MARKERS) {
        return text;
    }
    let Text { body, ranges } = text;
    let placeholders: std::collections::HashMap<u32, usize> = ranges
        .iter()
        .enumerate()
        .map(|(index, range)| (range.start, index))
        .collect();
    let atoms: Vec<Atom> = body
        .char_indices()
        .map(|(at, c)| {
            match u32::try_from(at)
                .ok()
                .and_then(|at| placeholders.get(&at).copied())
            {
                Some(index) if c == MENTION_PLACEHOLDER => Atom::Mention(index),
                _ => Atom::Ch(c),
            }
        })
        .collect();
    let mut reader = Reader::new(&atoms, &ranges);
    reader.parse(0, atoms.len(), depth);
    let Reader { body, out, .. } = reader;
    // By start, the longer first; on a tie the format range before the mention it wraps, and of two format
    // ranges the OUTER one (it was emitted last) first.
    let mut ordered: Vec<(usize, TextRange)> = out.into_iter().enumerate().collect();
    ordered.sort_by_key(|(emitted, r)| {
        (
            r.start,
            std::cmp::Reverse(r.len),
            matches!(r.kind, RangeKind::Mention(_) | RangeKind::MentionAll),
            std::cmp::Reverse(*emitted),
        )
    });
    Text {
        body,
        ranges: ordered.into_iter().map(|(_, r)| r).collect(),
    }
}

struct Reader<'a> {
    atoms: &'a [Atom],
    /// The mention ranges of the input (an atom names its range by index).
    mentions: &'a [TextRange],
    /// For each index `i` (and `n`): the index of the first line break at or after `i`, `n` if none.
    next_break: Vec<usize>,
    /// Per marker: the places where a pair may CLOSE (the marker follows a character that is not white space
    /// and is followed by the end or a character that is not alphanumeric), sorted.
    closers: [Vec<usize>; 3],
    body: String,
    out: Vec<TextRange>,
}

impl<'a> Reader<'a> {
    fn new(atoms: &'a [Atom], mentions: &'a [TextRange]) -> Self {
        let n = atoms.len();
        let mut next_break = vec![n; n + 1];
        for i in (0..n).rev() {
            next_break[i] = if atoms[i].is_line_break() {
                i
            } else {
                next_break[i + 1]
            };
        }
        let mut closers: [Vec<usize>; 3] = Default::default();
        for (j, atom) in atoms.iter().enumerate() {
            let Some(marker) = marker_of(*atom) else {
                continue;
            };
            let after_ink = j > 0 && !atoms[j - 1].is_space();
            let before_boundary = atoms.get(j + 1).is_none_or(|next| !next.is_alphanumeric());
            if after_ink && before_boundary {
                closers[marker].push(j);
            }
        }
        Self {
            atoms,
            mentions,
            next_break,
            closers,
            body: String::with_capacity(n),
            out: Vec::new(),
        }
    }

    /// Emits `atoms[from..to]` with its pairs read, `depth` spans deep.
    fn parse(&mut self, from: usize, to: usize, depth: usize) {
        let mut i = from;
        while i < to {
            if let Some((marker, close)) = self.pair_at(i, to, depth) {
                self.span(i + 1, close, kind_of(marker), depth + 1);
                i = close + 1;
            } else {
                self.emit(self.atoms[i]);
                i += 1;
            }
        }
    }

    /// The marker and the close of the pair that opens at `i`, when one does (and ends before `to`).
    fn pair_at(&self, i: usize, to: usize, depth: usize) -> Option<(usize, usize)> {
        if depth >= MAX_DEPTH {
            return None;
        }
        let marker = marker_of(self.atoms[i])?;
        let after_word = i > 0 && self.atoms[i - 1].is_alphanumeric();
        let opens = !after_word && i + 1 < to && !self.atoms[i + 1].is_space();
        if !opens {
            return None;
        }
        // The first valid closer with one character of content between, on the same line.
        let closers = &self.closers[marker];
        let at = closers.partition_point(|j| *j < i + 2);
        let close = *closers.get(at)?;
        (close < to && self.next_break[i + 1] >= close).then_some((marker, close))
    }

    /// Emits the content `from..to` as one range of `kind`.
    fn span(&mut self, from: usize, to: usize, kind: RangeKind, depth: usize) {
        let start = self.body.len();
        self.parse(from, to, depth);
        let len = self.body.len() - start;
        if let (Ok(start), Ok(len)) = (u32::try_from(start), u32::try_from(len))
            && len > 0
        {
            self.out.push(TextRange { start, len, kind });
        }
    }

    fn emit(&mut self, atom: Atom) {
        match atom {
            Atom::Ch(c) => self.body.push(c),
            Atom::Mention(index) => {
                let start = self.body.len();
                self.body.push(MENTION_PLACEHOLDER);
                if let (Ok(start), Some(range)) = (u32::try_from(start), self.mentions.get(index)) {
                    self.out.push(TextRange {
                        start,
                        len: range.len,
                        kind: range.kind.clone(),
                    });
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// LOCATION (decision R4)
// ---------------------------------------------------------------------------------------------------------

/// A location message's body (`e2e.location`, module doc), or `None` when it is not one: not UTF-8, a first
/// line that is not `<lat>,<lon>[,<accuracy>]` of finite decimals, a latitude outside -90..=90 or a longitude
/// outside -180..=180. The accuracy is read (it must be a number) and not kept: the contract has no field.
/// Then no line (no place), one line (the address), two (the name and the address) or more (the name is the
/// second line, the address the rest joined by line breaks); a literal backslash and `n` in the address is a
/// line break (the spec escapes them).
pub(crate) fn location(inner: &[u8]) -> Option<Location> {
    let text = std::str::from_utf8(inner).ok()?;
    let (first, rest) = match text.split_once('\n') {
        Some((first, rest)) => (first, Some(rest)),
        None => (text, None),
    };
    let mut numbers = first.split(',');
    let lat = decimal(numbers.next()?)?;
    let lon = decimal(numbers.next()?)?;
    if let Some(accuracy) = numbers.next() {
        decimal(accuracy)?;
    }
    if numbers.next().is_some() {
        return None;
    }
    let lines: Vec<&str> = rest
        .map(|rest| rest.split('\n').collect())
        .unwrap_or_default();
    let (name, address) = match lines.as_slice() {
        [] => (None, None),
        [address] => (None, Some((*address).to_owned())),
        [name, address] => (Some((*name).to_owned()), Some((*address).to_owned())),
        [name, address @ ..] => (Some((*name).to_owned()), Some(address.join("\n"))),
    };
    Some(Location {
        lat_e7: degrees_e7(lat, 90.0)?,
        lon_e7: degrees_e7(lon, 180.0)?,
        name: name.filter(|name| !name.is_empty()),
        address: address
            .map(|address| address.replace("\\n", "\n"))
            .filter(|address| !address.is_empty()),
    })
}

/// A finite decimal number as the spec writes coordinates (an optional sign, digits, an optional point): no
/// exponent, no `inf`, no `NaN`.
fn decimal(token: &str) -> Option<f64> {
    let token = token.trim();
    let digits = token.strip_prefix(['-', '+']).unwrap_or(token);
    let plain = digits.bytes().all(|b| b.is_ascii_digit() || b == b'.');
    if !plain || !digits.bytes().any(|b| b.is_ascii_digit()) {
        return None;
    }
    token.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// Degrees as the model's coordinate (degrees x 10^7, rounded), `None` outside `-limit..=limit`.
fn degrees_e7(degrees: f64, limit: f64) -> Option<i32> {
    // |degrees| <= 180 gives |e7| <= 1.8e9, inside `i32`.
    (degrees.abs() <= limit).then(|| (degrees * 1e7).round() as i32)
}

// ---------------------------------------------------------------------------------------------------------
// FILE (decision R3)
// ---------------------------------------------------------------------------------------------------------

/// A parsed file message.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParsedFile {
    /// Rendering type 2 of an `image/*` type: the attachment is a sticker (an image).
    pub(crate) sticker: bool,
    /// The one attachment: kind, type, size, name, dimensions, duration and the pointer; state `Remote`.
    pub(crate) attachment: Attachment,
    /// The message's caption (`d`): mentions and markup read, no quote form; `None` when empty or absent.
    pub(crate) caption: Option<Text>,
    /// The caption names the own ID, or `@All` was written in a group.
    pub(crate) mentions_me: bool,
}

/// A file message's body (module doc), or `None`: JSON that does not parse, is no object, or lacks `b` (the
/// file's blob id, 32 hex digits), `k` (the key, 64 hex digits), `m` (the media type) or `s` (the size).
/// `own` and `in_group` are for the caption's mentions.
pub(crate) fn file(inner: &[u8], own: &str, in_group: bool) -> Option<ParsedFile> {
    let json: Value = serde_json::from_slice(inner).ok()?;
    let object = json.as_object()?;
    let text = |key: &str| object.get(key).and_then(Value::as_str);
    if !text("b").is_some_and(|b| is_hex(b, 32)) || !text("k").is_some_and(|k| is_hex(k, 64)) {
        return None;
    }
    let mime = bare_mime(text("m")?)?;
    let size = object.get("s")?.as_u64()?;
    let meta = object.get("x").and_then(Value::as_object);
    let in_meta = |key: &str| meta.and_then(|m| m.get(key));
    // The rendering type: `j`; not set -> the deprecated `i` (1 is media); unassigned or unknown -> a file.
    let rendering = match object.get("j") {
        // Set: 0, 1 or 2 as assigned, anything else (a number nobody assigned, a text, a float) is a file.
        Some(j) if !j.is_null() => j.as_u64().filter(|r| *r <= 2).unwrap_or_default(),
        _ => u64::from(object.get("i").and_then(Value::as_u64) == Some(1)),
    };
    // P-L3 (Desktop `task/common/file.ts`, the spec's `a`: "For images"): the animated flag is image metadata, so
    // it makes a GIF of an `image/*` type only; rendering 2 is a sticker only for an `image/*` type, any other
    // type under it is read as rendering 1 (a video, a sound) or is a file.
    let is_image = mime.starts_with("image/");
    let animated = is_image && in_meta("a").and_then(Value::as_bool) == Some(true);
    let sticker = rendering == 2 && is_image;
    let kind = match rendering {
        0 => AttachmentKind::File,
        _ if sticker => AttachmentKind::Image,
        _ if mime == "image/gif" || animated => AttachmentKind::Gif,
        _ if is_image => AttachmentKind::Image,
        _ if mime.starts_with("video/") => AttachmentKind::Video,
        _ if mime.starts_with("audio/") => AttachmentKind::Voice,
        _ => AttachmentKind::File,
    };
    let dims = matches!(
        kind,
        AttachmentKind::Image | AttachmentKind::Video | AttachmentKind::Gif
    )
    .then(|| {
        let side = |key: &str| {
            in_meta(key)
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .filter(|v| *v > 0)
        };
        side("w").zip(side("h"))
    })
    .flatten();
    let duration_ms = matches!(
        kind,
        AttachmentKind::Video | AttachmentKind::Voice | AttachmentKind::Audio
    )
    .then(|| in_meta("d").and_then(Value::as_f64).and_then(millis))
    .flatten();
    let file_name = text("n")
        .map(|name| {
            name.get(..name.floor_char_boundary(MAX_NAME))
                .unwrap_or(name)
        })
        .filter(|name| !name.is_empty())
        .map(str::to_owned);
    let (caption, mentions_me) = match text("d").filter(|d| !d.is_empty()) {
        Some(d) => {
            let (caption, mine) = rich(d, own, in_group);
            (Some(caption), mine)
        }
        None => (None, false),
    };
    Some(ParsedFile {
        sticker,
        attachment: Attachment {
            kind,
            mime,
            size,
            file_name,
            dims,
            duration_ms,
            state: AttachmentState::Remote,
            remote: Some(BackendBlob::new(POINTER_VERSION, inner.to_vec())),
            ..Attachment::default()
        },
        caption,
        mentions_me,
    })
}

/// Whether `text` is `digits` hexadecimal digits.
fn is_hex(text: &str, digits: usize) -> bool {
    text.len() == digits && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A media type without parameters, trimmed and lower case; `None` when nothing is left.
fn bare_mime(mime: &str) -> Option<String> {
    let bare = mime.split(';').next().unwrap_or_default().trim();
    (!bare.is_empty()).then(|| bare.to_ascii_lowercase())
}

/// Seconds (a float) as milliseconds, rounded and at most `u32::MAX`; `None` for a negative or non-finite value.
fn millis(seconds: f64) -> Option<u32> {
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    let ms = (seconds * 1000.0).round();
    Some(if ms >= f64::from(u32::MAX) {
        u32::MAX
    } else {
        ms as u32
    })
}

// ---------------------------------------------------------------------------------------------------------
// EDIT, DELETE, REACTION (decisions R6, R7, R8)
// ---------------------------------------------------------------------------------------------------------

/// A parsed `EditMessage`.
#[derive(Clone, PartialEq)]
pub(crate) struct ParsedEdit {
    /// The edited message's id (the sender's own id for it).
    pub(crate) target: MessageId,
    /// The new text: a valid quote prefix at its start removed, mentions and markup read. May be empty (the
    /// core drops it for a text message and removes the caption of a file).
    pub(crate) text: Text,
}

impl std::fmt::Debug for ParsedEdit {
    /// Counts only: the text is a body.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParsedEdit")
            .field("text_bytes", &self.text.body.len())
            .field("ranges", &self.text.ranges.len())
            .finish()
    }
}

/// The `EditMessage` of an inner body (the data inside the group container for a group edit). Refused:
/// [`RULE_EDIT_INVALID`] (the protobuf does not decode, a text that is not UTF-8 included). A quote prefix
/// the phone may write at the start of an edited text is removed whatever message it names (`[U: real phone]`
/// whether one writes it; the message keeps the quote it had, the map says `keeps_quote: None`); an empty text
/// is passed on.
pub(crate) fn edit(inner: &[u8], own: &str, in_group: bool) -> Result<ParsedEdit, &'static str> {
    let edit = csp_e2e::EditMessage::decode(inner).map_err(|_| RULE_EDIT_INVALID)?;
    let comment = split_quote(&edit.text).map_or(edit.text.as_str(), |(_, rest)| rest);
    let (text, _mentions_me) = rich(comment, own, in_group);
    Ok(ParsedEdit {
        target: MessageId(edit.message_id),
        text,
    })
}

/// The message a `DeleteMessage` removes. Refused: [`RULE_DELETE_INVALID`] (the protobuf does not decode).
pub(crate) fn delete(inner: &[u8]) -> Result<MessageId, &'static str> {
    csp_e2e::DeleteMessage::decode(inner)
        .map(|delete| MessageId(delete.message_id))
        .map_err(|_| RULE_DELETE_INVALID)
}

/// A parsed `Reaction`.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ParsedReaction {
    /// The reacted-to message's id.
    pub(crate) target: MessageId,
    /// The emoji, 1 to [`MAX_REACTION_BYTES`] bytes of UTF-8.
    pub(crate) emoji: String,
    /// `withdraw` instead of `apply`.
    pub(crate) remove: bool,
}

impl std::fmt::Debug for ParsedReaction {
    /// The direction only.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParsedReaction")
            .field("remove", &self.remove)
            .finish_non_exhaustive()
    }
}

/// The `Reaction` of an inner body. Refused: [`RULE_REACTION_INVALID`] (the protobuf does not decode, or it
/// sets neither `apply` nor `withdraw`), [`RULE_REACTION_BYTES`] (the emoji is empty, over 64 bytes or not
/// UTF-8: the spec decodes it as UTF-8 and discards an empty result; a byte sequence that is no UTF-8 is
/// refused here too, R8).
pub(crate) fn reaction(inner: &[u8]) -> Result<ParsedReaction, &'static str> {
    use csp_e2e::reaction::Action;
    let reaction = csp_e2e::Reaction::decode(inner).map_err(|_| RULE_REACTION_INVALID)?;
    let (bytes, remove) = match reaction.action {
        Some(Action::Apply(bytes)) => (bytes, false),
        Some(Action::Withdraw(bytes)) => (bytes, true),
        None => return Err(RULE_REACTION_INVALID),
    };
    if bytes.is_empty() || bytes.len() > MAX_REACTION_BYTES {
        return Err(RULE_REACTION_BYTES);
    }
    let emoji = String::from_utf8(bytes).map_err(|_| RULE_REACTION_BYTES)?;
    Ok(ParsedReaction {
        target: MessageId(reaction.message_id),
        emoji,
        remove,
    })
}

// ---------------------------------------------------------------------------------------------------------
// DELIVERY RECEIPT, TYPING (decisions R9, R12)
// ---------------------------------------------------------------------------------------------------------

/// A parsed `delivery-receipt` (`csp.struct.yml`): the status byte as sent (the map decides what it means:
/// 1 received, 2 read, 3 and 4 the deprecated reactions) and the message ids, in order.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ParsedReceipt {
    pub(crate) status: u8,
    pub(crate) ids: Vec<MessageId>,
}

impl std::fmt::Debug for ParsedReceipt {
    /// The status and the count only: an id is a peer's message.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParsedReceipt")
            .field("status", &self.status)
            .field("ids", &self.ids.len())
            .finish()
    }
}

/// The `delivery-receipt` of an inner body (inside the group container for a group receipt): a status byte
/// and ONE OR MORE 8-byte message ids (`message-ids: u64-le[]`; the wire bytes in order are the id, as
/// everywhere in this crate). Refused: [`RULE_RECEIPT_SHORT`] (no id, or a length that is not 1 + 8n).
pub(crate) fn receipt(inner: &[u8]) -> Result<ParsedReceipt, &'static str> {
    let (status, ids) = inner.split_first().ok_or(RULE_RECEIPT_SHORT)?;
    let (whole, rest) = ids.as_chunks::<{ MessageId::LENGTH }>();
    if whole.is_empty() || !rest.is_empty() {
        return Err(RULE_RECEIPT_SHORT);
    }
    let ids = whole
        .iter()
        .map(|bytes| MessageId(u64::from_le_bytes(*bytes)))
        .collect();
    Ok(ParsedReceipt {
        status: *status,
        ids,
    })
}

/// The `typing-indicator` of a body: ONE byte, 1 typing, 0 stopped (`csp.struct.yml`: "Other values are
/// invalid"); anything else is `None`.
pub(crate) fn typing(body: &[u8]) -> Option<bool> {
    match body {
        [0] => Some(false),
        [1] => Some(true),
        _ => None,
    }
}

/// The three modifiers' protobufs written BY HAND as the wire carries them (field 1 `fixed64` = the eight
/// bytes of the message id, then the one string or bytes field), for the unit tests of this directory: a test
/// that built its input with prost's own encoder would only show that the decoder reads what the encoder
/// writes.
#[cfg(test)]
pub(crate) mod test_wire {
    /// Field 1 (`message_id`, `fixed64`): tag 0x09 and the eight bytes.
    fn id(id: [u8; 8]) -> Vec<u8> {
        let mut out = vec![0x09];
        out.extend_from_slice(&id);
        out
    }

    /// A length-delimited field `number` holding `bytes` (the lengths used here are under 128).
    fn field(number: u8, bytes: &[u8]) -> Vec<u8> {
        let mut out = vec![
            (number << 3) | 2,
            u8::try_from(bytes.len()).expect("under 128"),
        ];
        out.extend_from_slice(bytes);
        out
    }

    /// `EditMessage`: the id and field 2 (`text`), also when the text is empty.
    pub(crate) fn edit(target: [u8; 8], text: &str) -> Vec<u8> {
        [id(target), field(2, text.as_bytes())].concat()
    }

    /// `DeleteMessage`: the id.
    pub(crate) fn delete(target: [u8; 8]) -> Vec<u8> {
        id(target)
    }

    /// `Reaction` with `apply` (field 2, bytes).
    pub(crate) fn apply(target: [u8; 8], emoji: &[u8]) -> Vec<u8> {
        [id(target), field(2, emoji)].concat()
    }

    /// `Reaction` with `withdraw` (field 3, bytes).
    pub(crate) fn withdraw(target: [u8; 8], emoji: &[u8]) -> Vec<u8> {
        [id(target), field(3, emoji)].concat()
    }

    /// `Reaction` with neither action: the id alone.
    pub(crate) fn no_action(target: [u8; 8]) -> Vec<u8> {
        id(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic IDs, each a substring of no other test value.
    const OWN: &str = "A6OWN001";
    const OTHER: &str = "A6OTH002";
    const CREATOR: &str = "A6CRT003";

    fn id(bytes: [u8; 8]) -> MessageId {
        MessageId::from(bytes)
    }

    fn parse(text: &str) -> Result<ParsedText, &'static str> {
        super::text(text.as_bytes(), id([9; 8]), OWN, false)
    }

    #[test]
    fn a_plain_text_is_the_whole_body() {
        let parsed = parse("hello there").expect("a text");
        assert_eq!(parsed.text.body, "hello there");
        assert!(parsed.text.ranges.is_empty());
        assert_eq!(parsed.quoted, None);
        assert!(!parsed.mentions_me);
    }

    #[test]
    fn a_quote_prefix_is_removed_and_the_id_is_the_wire_bytes_in_order() {
        let parsed = parse("> quote #0102030405060708\n\nthe answer").expect("a quote");
        assert_eq!(
            parsed.text.body, "the answer",
            "the prefix stayed in the text"
        );
        assert_eq!(
            parsed.quoted.map(MessageId::to_bytes),
            Some([1, 2, 3, 4, 5, 6, 7, 8]),
            "the quoted id is not the digits' bytes in order"
        );
        // The same digits with CRLF line breaks (each break may be \r\n) and a comment that goes on.
        let crlf = parse("> quote #ff00ab100000009c\r\n\r\nsecond\n\nline").expect("crlf");
        assert_eq!(crlf.text.body, "second\n\nline");
        assert_eq!(
            crlf.quoted.map(MessageId::to_bytes),
            Some([0xff, 0, 0xab, 0x10, 0, 0, 0, 0x9c])
        );
        // Exactly two breaks are the separator; a third belongs to the comment.
        let three = parse("> quote #0102030405060708\n\n\nc").expect("three");
        assert_eq!(three.text.body, "\nc");
    }

    #[test]
    fn a_text_that_only_looks_like_a_quote_stays_whole() {
        for bad in [
            // A bad id: upper case, 15 digits, a non-hex digit, 17 digits.
            "> quote #0102030405060708A\n\nx",
            "> quote #010203040506070\n\nx",
            "> quote #01020304050607ZZ\n\nx",
            "> quote #01020304050607AB\n\nx",
            // One break, a space, no break; a different prefix.
            "> quote #0102030405060708\nx",
            "> quote #0102030405060708 \n\nx",
            "> quote #0102030405060708x",
            ">quote #0102030405060708\n\nx",
        ] {
            let parsed = parse(bad).expect("a text");
            assert_eq!(parsed.quoted, None, "a malformed quote was taken");
            assert_eq!(parsed.text.body, bad, "a malformed quote lost its prefix");
        }
        // A quote of the message itself is no quote (Desktop's "quoted itself").
        let own = super::text(b"> quote #0909090909090909\n\nme", id([9; 8]), OWN, false)
            .expect("a text");
        assert_eq!(own.quoted, None);
        assert_eq!(own.text.body, "> quote #0909090909090909\n\nme");
    }

    #[test]
    fn a_mention_is_one_placeholder_with_its_user() {
        let text = format!("hi @[{OTHER}] and @[{OWN}]!");
        let parsed = parse(&text).expect("a text");
        let marks = MENTION_PLACEHOLDER.len_utf8();
        assert_eq!(parsed.text.body, "hi \u{fffc} and \u{fffc}!");
        assert_eq!(parsed.text.ranges.len(), 2);
        assert_eq!(parsed.text.ranges[0].start, 3);
        assert_eq!(parsed.text.ranges[0].len as usize, marks);
        assert_eq!(
            parsed.text.ranges[1].start as usize,
            3 + marks + " and ".len()
        );
        for (range, who) in parsed.text.ranges.iter().zip([OTHER, OWN]) {
            assert_eq!(
                range.kind,
                RangeKind::Mention(UserId::from_parts("threema", who).expect("a user")),
                "a mention names another user"
            );
        }
        // Every range passes the core's own check, and the user is named.
        assert!(parsed.text.shown_mentions().len() == 2);
        assert!(
            parsed.mentions_me,
            "the own ID was named and mentions_me is off"
        );
        let other = parse(&format!("@[{OTHER}]")).expect("a text");
        assert!(!other.mentions_me, "another user's mention is not ours");
    }

    #[test]
    fn at_all_is_a_range_and_a_mention_of_the_user_only_in_a_group() {
        let text = "@[@@@@@@@@] meeting";
        let solo = super::text(text.as_bytes(), id([9; 8]), OWN, false).expect("1:1");
        assert_eq!(solo.text.ranges[0].kind, RangeKind::MentionAll);
        assert!(!solo.mentions_me, "@All mentioned the user in a 1:1 chat");
        let group = super::text(text.as_bytes(), id([9; 8]), OWN, true).expect("group");
        assert_eq!(group.text.body, "\u{fffc} meeting");
        assert!(
            group.mentions_me,
            "@All did not mention the user in a group"
        );
    }

    #[test]
    fn the_sixty_fifth_mention_stays_plain_text() {
        let one = format!("@[{OTHER}]");
        let text = vec![one.as_str(); MAX_MENTIONS + 1].join(" ");
        let parsed = parse(&text).expect("a text");
        assert_eq!(parsed.text.ranges.len(), MAX_MENTIONS);
        assert!(
            parsed.text.body.ends_with(&one),
            "the mention over the bound was not left as written"
        );
        assert_eq!(
            parsed.text.body.matches(MENTION_PLACEHOLDER).count(),
            MAX_MENTIONS
        );
    }

    #[test]
    fn what_is_not_a_mention_stays_as_typed() {
        for plain in [
            "@[short]",
            "@[lowercase]",
            "@[A6OWN00]",
            "@[A6OWN0012]",
            "@[A6OWN001",
            "@[@@@@@@@]",
            "@ [A6OWN001]",
            "[A6OWN001]",
            "@[",
            "@[@[",
        ] {
            let parsed = parse(plain).expect("a text");
            assert_eq!(parsed.text.body, plain, "a non-mention was changed");
            assert!(parsed.text.ranges.is_empty());
        }
        // A broken opener before a real mention does not hide it.
        let mixed = parse(&format!("@[@[{OTHER}]")).expect("a text");
        assert_eq!(mixed.text.body, "@[\u{fffc}");
        assert_eq!(mixed.text.ranges[0].start, 2);
        // A Gateway ID ('*' first) is a valid mention.
        let gateway = parse("@[*3MAPUSH]").expect("a text");
        assert_eq!(gateway.text.ranges.len(), 1);
    }

    #[test]
    fn a_quote_and_a_mention_work_together_and_the_ranges_follow_the_comment() {
        let text = format!("> quote #0a0b0c0d0e0f1011\n\nthanks @[{OTHER}]");
        let parsed = parse(&text).expect("a text");
        assert_eq!(parsed.text.body, "thanks \u{fffc}");
        assert_eq!(
            parsed.text.ranges[0].start, 7,
            "the range is in the comment"
        );
        assert_eq!(
            parsed.quoted.map(MessageId::to_bytes).map(|b| b[0]),
            Some(0x0a)
        );
        // Multi-byte text before a mention: byte offsets, not characters.
        let wide = parse(&format!("héllo @[{OTHER}]")).expect("a text");
        assert_eq!(wide.text.ranges[0].start, 7);
        assert!(wide.text.shown_mentions().len() == 1);
    }

    #[test]
    fn a_malformed_text_is_refused_by_its_rule() {
        assert_eq!(
            super::text(&[0xff, 0xfe, b'a'], id([9; 8]), OWN, false).unwrap_err(),
            RULE_TEXT_UTF8
        );
        assert_eq!(parse("").unwrap_err(), RULE_TEXT_EMPTY);
        // A quote with nothing after it has no comment to show.
        assert_eq!(
            parse("> quote #0102030405060708\n\n").unwrap_err(),
            RULE_TEXT_EMPTY
        );
    }

    #[test]
    fn the_member_container_is_creator_group_id_then_the_inner_data() {
        let mut body = Vec::new();
        body.extend_from_slice(CREATOR.as_bytes());
        body.extend_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
        body.extend_from_slice(b"inner");
        let (group, inner) = member_container(&body).expect("a container");
        assert_eq!(group.creator, CREATOR);
        assert_eq!(group.group_id, 0x0102_0304_0506_0708);
        assert_eq!(inner, b"inner");
        // The group id is little-endian: the first byte after the creator is the lowest.
        assert_eq!(body[8], 0x08);
        // An empty inner part is a container too.
        assert_eq!(member_container(&body[..16]).expect("empty inner").1, b"");
    }

    #[test]
    fn a_malformed_member_container_is_refused_by_its_rule() {
        let mut body = Vec::new();
        body.extend_from_slice(CREATOR.as_bytes());
        body.extend_from_slice(&1u64.to_le_bytes());
        assert_eq!(
            member_container(&body[..15]).unwrap_err(),
            RULE_CONTAINER_SHORT
        );
        assert_eq!(member_container(&[]).unwrap_err(), RULE_CONTAINER_SHORT);
        for bad in [&b"a6crt003"[..], b"A6CRT0 3", b"\xff6CRT003"] {
            let mut body = bad.to_vec();
            body.extend_from_slice(&1u64.to_le_bytes());
            assert_eq!(
                member_container(&body).unwrap_err(),
                RULE_CONTAINER_CREATOR,
                "a creator that is no Threema ID was taken"
            );
        }
    }

    #[test]
    fn the_creator_container_is_the_group_id_then_the_inner_data() {
        let mut body = 7u64.to_le_bytes().to_vec();
        body.extend_from_slice(b"x");
        let (group_id, inner) = creator_container(&body).expect("a container");
        assert_eq!((group_id, inner), (7, &b"x"[..]));
        assert_eq!(
            creator_container(&body[..7]).unwrap_err(),
            RULE_CONTAINER_SHORT
        );
    }

    #[test]
    fn the_identity_rule_is_libthreemas() {
        for good in ["ABCDEFGH", "01234567", "*3MAPUSH", "A6OWN001"] {
            assert!(is_identity(good), "a valid ID was refused");
            assert!(
                libthreema::common::ThreemaId::try_from(good).is_ok(),
                "libthreema refuses what the rule takes"
            );
        }
        for bad in [
            "",
            "abcdefgh",
            "ABCDEFG",
            "ABCDEFGHI",
            "A*CDEFGH",
            "ABCD EFG",
        ] {
            assert!(!is_identity(bad), "an invalid ID was taken");
            assert!(
                libthreema::common::ThreemaId::try_from(bad).is_err(),
                "libthreema takes what the rule refuses"
            );
        }
    }

    // ---------------------------------------------------------------------------------------------------
    // TH5b B1: markup (R5)
    // ---------------------------------------------------------------------------------------------------

    /// The text and its ranges read as (kind, covered text) pairs, in the order the output keeps (by start,
    /// the longer first): a literal view of a range, no offsets.
    fn marked(input: &str) -> (String, Vec<(String, String)>) {
        let (text, _) = rich(input, OWN, false);
        let shown = text
            .ranges
            .iter()
            .map(|r| {
                let covered = &text.body[r.start as usize..(r.start + r.len) as usize];
                let kind = match &r.kind {
                    RangeKind::Bold => "bold",
                    RangeKind::Italic => "italic",
                    RangeKind::Strike => "strike",
                    RangeKind::Mention(_) => "mention",
                    RangeKind::MentionAll => "all",
                    _ => "other",
                };
                (kind.to_owned(), covered.to_owned())
            })
            .collect();
        (text.body, shown)
    }

    fn check(input: &str, body: &str, ranges: &[(&str, &str)]) {
        let (got_body, got_ranges) = marked(input);
        let wanted: Vec<(String, String)> = ranges
            .iter()
            .map(|(k, c)| ((*k).to_owned(), (*c).to_owned()))
            .collect();
        assert_eq!(got_body, body, "the body of {input:?}");
        assert_eq!(got_ranges, wanted, "the ranges of {input:?}");
    }

    #[test]
    fn markup_pairs_lose_their_markers_and_range_the_inner_text() {
        check("*bold*", "bold", &[("bold", "bold")]);
        check("_italic_", "italic", &[("italic", "italic")]);
        check("~strike~", "strike", &[("strike", "strike")]);
        check(
            "say *hello* to _all_ ~of~ you.",
            "say hello to all of you.",
            &[("bold", "hello"), ("italic", "all"), ("strike", "of")],
        );
        // Punctuation around a pair is fine, before the opener and after the closer.
        check("(*bold*)", "(bold)", &[("bold", "bold")]);
        check(
            "*bold*, then *more*!",
            "bold, then more!",
            &[("bold", "bold"), ("bold", "more")],
        );
        // The markers of a pair that spans several words and punctuation inside.
        check(
            "*one, two. three*",
            "one, two. three",
            &[("bold", "one, two. three")],
        );
        // Offsets are bytes: multi-byte text and an astral character.
        check("é *ü* ö", "é ü ö", &[("bold", "ü")]);
        check("*😀*", "😀", &[("bold", "😀")]);
    }

    #[test]
    fn markup_ranges_are_byte_offsets_of_the_new_body() {
        let (text, _) = rich("é *ü* ö", OWN, false);
        // "é " is 3 bytes; the covered "ü" is 2 bytes.
        assert_eq!((text.ranges[0].start, text.ranges[0].len), (3, 2));
        let (text, _) = rich("*a* *b*", OWN, false);
        let at: Vec<(u32, u32)> = text.ranges.iter().map(|r| (r.start, r.len)).collect();
        assert_eq!(at, [(0, 1), (2, 1)]);
    }

    #[test]
    fn a_marker_in_the_middle_of_a_word_stays_text() {
        // Both the opener and the closer are inside a word.
        check("a*b*c", "a*b*c", &[]);
        // Only the OPENER is in a word: the closer alone would be valid (it ends at a space).
        check("a*b* c", "a*b* c", &[]);
        check("x_y_ z", "x_y_ z", &[]);
        check("1~2~ 3", "1~2~ 3", &[]);
        check("snake_case_name", "snake_case_name", &[]);
        check("5*3*2", "5*3*2", &[]);
        // Only the CLOSER is in a word: the opener alone is valid.
        check("*a*b c", "*a*b c", &[]);
        // The first closer in a word does not end the span; the next valid one does.
        check("_snake_case_", "snake_case", &[("italic", "snake_case")]);
    }

    #[test]
    fn a_pair_never_spans_a_line_break_and_an_unpaired_marker_stays() {
        check("*two\nlines*", "*two\nlines*", &[]);
        check("*two\r\nlines*", "*two\r\nlines*", &[]);
        check(
            "*one*\n*two*",
            "one\ntwo",
            &[("bold", "one"), ("bold", "two")],
        );
        check("*unpaired", "*unpaired", &[]);
        check("unpaired*", "unpaired*", &[]);
        check("5 * 3 * 2", "5 * 3 * 2", &[]);
        check("* not bold *", "* not bold *", &[]);
        // Nothing between the markers: no pair.
        check("**", "**", &[]);
        check("a ** b", "a ** b", &[]);
        // A marker after white space whose partner follows white space.
        check("*a *", "*a *", &[]);
    }

    #[test]
    fn spans_of_different_markers_nest() {
        check("*_both_*", "both", &[("bold", "both"), ("italic", "both")]);
        check(
            "*bold _and italic_ text*",
            "bold and italic text",
            &[("bold", "bold and italic text"), ("italic", "and italic")],
        );
        check(
            "~*_all three_*~",
            "all three",
            &[
                ("strike", "all three"),
                ("bold", "all three"),
                ("italic", "all three"),
            ],
        );
        // Two spans side by side.
        check("*a* _b_", "a b", &[("bold", "a"), ("italic", "b")]);
        // A marker of the span's own kind inside it is no new span.
        check("*a *b* c*", "a *b c*", &[("bold", "a *b")]);
    }

    #[test]
    fn the_seventh_level_of_nesting_stays_text() {
        // The closer rule (the NEXT valid closer of the same marker) cannot nest a marker in its own
        // kind, so three markers reach three levels from text; the bound is tested from a start depth.
        let nested = |depth: usize| {
            let (text, _) = mentions("*_~x~_*", OWN, false);
            let text = markup_from(text, depth);
            let covered: Vec<String> = text
                .ranges
                .iter()
                .map(|r| text.body[r.start as usize..(r.start + r.len) as usize].to_owned())
                .collect();
            (text.body, covered)
        };
        assert_eq!(nested(0), ("x".to_owned(), vec!["x".to_owned(); 3]));
        // Starting at level 5 (depth 4): the bold and the italic span are the 5th and 6th levels, the
        // strike span would be the 7th and stays as typed.
        assert_eq!(
            nested(4),
            ("~x~".to_owned(), vec!["~x~".to_owned(); 2]),
            "the 7th level was read"
        );
        // Starting at the 6th level (depth 5): one span only.
        assert_eq!(nested(5), ("_~x~_".to_owned(), vec!["_~x~_".to_owned()]));
        // At the bound itself nothing is read.
        assert_eq!(nested(MAX_DEPTH), ("*_~x~_*".to_owned(), Vec::new()));
    }

    #[test]
    fn a_marker_beside_a_mention_reads_and_the_mention_ranges_move() {
        let mention = format!("@[{OTHER}]");
        let user = UserId::from_parts("threema", OTHER).expect("a user");
        let wide = u32::try_from(MENTION_PLACEHOLDER.len_utf8()).expect("3");
        // A mention after a span: the removed markers move its range left by two bytes.
        let (text, _) = rich(&format!("*hi* {mention}!"), OWN, false);
        assert_eq!(text.body, "hi \u{fffc}!");
        assert_eq!(
            text.ranges,
            vec![
                TextRange {
                    start: 0,
                    len: 2,
                    kind: RangeKind::Bold
                },
                TextRange {
                    start: 3,
                    len: wide,
                    kind: RangeKind::Mention(user.clone())
                },
            ]
        );
        // A mention INSIDE a span: the span covers the placeholder, the mention range keeps its place in it.
        let (text, _) = rich(&format!("*{mention}*"), OWN, false);
        assert_eq!(text.body, "\u{fffc}");
        assert_eq!(
            text.ranges,
            vec![
                TextRange {
                    start: 0,
                    len: wide,
                    kind: RangeKind::Bold
                },
                TextRange {
                    start: 0,
                    len: wide,
                    kind: RangeKind::Mention(user.clone())
                },
            ]
        );
        // A mention BEFORE a marker opens it (the placeholder is no letter): it keeps its start.
        let (text, _) = rich(&format!("{mention}*bold*"), OWN, false);
        assert_eq!(text.body, "\u{fffc}bold");
        assert_eq!(
            text.ranges,
            vec![
                TextRange {
                    start: 0,
                    len: wide,
                    kind: RangeKind::Mention(user.clone())
                },
                TextRange {
                    start: wide,
                    len: 4,
                    kind: RangeKind::Bold
                },
            ]
        );
        // A mention AFTER a closer does not stop it.
        let (text, _) = rich(&format!("*bold*{mention}"), OWN, false);
        assert_eq!(text.body, "bold\u{fffc}");
        assert_eq!(text.ranges.len(), 2);
        assert_eq!((text.ranges[1].start, text.ranges[1].len), (4, wide));
        // `@All` is carried over like a person, and mentions_me survives the markup.
        let (text, mine) = rich("_@[@@@@@@@@] meeting_", OWN, true);
        assert_eq!(text.body, "\u{fffc} meeting");
        assert!(mine);
        assert!(
            text.ranges
                .iter()
                .any(|r| r.kind == RangeKind::MentionAll && r.start == 0)
        );
        assert!(
            text.ranges
                .iter()
                .any(|r| r.kind == RangeKind::Italic && r.start == 0)
        );
        // The core's own check of ranges passes, and the one mention still counts.
        let mut checked = text.clone();
        assert_eq!(
            checked.sanitize(),
            0,
            "a range does not pass the core's check"
        );
        assert_eq!(text.shown_mentions().len(), 1);
    }

    #[test]
    fn a_text_with_a_quote_reads_its_markup_after_the_quote_and_a_caption_too() {
        let parsed = parse("> quote #0102030405060708\n\nthanks *very* much").expect("a quote");
        assert_eq!(parsed.text.body, "thanks very much");
        assert_eq!(parsed.text.ranges.len(), 1);
        assert_eq!(
            (parsed.text.ranges[0].start, parsed.text.ranges[0].len),
            (7, 4)
        );
        assert!(parsed.quoted.is_some());
        // A text that is only markup characters is still a text (nothing is left to refuse).
        assert_eq!(parse("**").expect("a text").text.body, "**");
    }

    /// A deterministic generator of marker-heavy text (no dependency): characters from an alphabet of the
    /// markers, white space, line breaks, letters, digits, a mention, an astral emoji, multi-byte letters.
    fn generated(count: usize) -> impl Iterator<Item = String> {
        let alphabet: Vec<String> = [
            "*", "_", "~", " ", "\n", "\r\n", "a", "b", "1", ".", "(", ")", "é", "😀", "**", "*_",
            "_~", "~~",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .chain([format!("@[{OTHER}]"), "@[@@@@@@@@]".to_owned()])
        .collect();
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        (0..count).map(move |_| {
            let mut next = || {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                usize::try_from(state >> 33).unwrap_or(0)
            };
            let len = next() % 33;
            (0..len)
                .map(|_| alphabet[next() % alphabet.len()].as_str())
                .collect::<String>()
        })
    }

    #[test]
    fn markup_never_panics_every_range_is_valid_and_no_other_character_is_lost() {
        let strip = |s: &str| -> String { s.chars().filter(|c| !MARKERS.contains(c)).collect() };
        for input in generated(20_000) {
            let (before, _) = mentions(&input, OWN, true);
            let marked = markup(before.clone());
            // No character that is not a marker is lost or made up (a marker may stay as text).
            assert_eq!(
                strip(&marked.body),
                strip(&before.body),
                "markup lost or added a character of {input:?}"
            );
            // The body only ever loses markers; the mentions all stay, once each.
            assert!(marked.body.len() <= before.body.len());
            let count = |t: &Text| {
                t.ranges
                    .iter()
                    .filter(|r| matches!(r.kind, RangeKind::Mention(_) | RangeKind::MentionAll))
                    .count()
            };
            assert_eq!(
                count(&marked),
                count(&before),
                "a mention was lost in {input:?}"
            );
            let mut checked = marked.clone();
            assert_eq!(checked.sanitize(), 0, "an invalid range for {input:?}");
            // Sorted by start.
            assert!(marked.ranges.windows(2).all(|w| w[0].start <= w[1].start));
        }
    }

    // ---------------------------------------------------------------------------------------------------
    // TH5b B1: location (R4)
    // ---------------------------------------------------------------------------------------------------

    fn place(body: &str) -> Option<Location> {
        location(body.as_bytes())
    }

    fn at(lat_e7: i32, lon_e7: i32, name: Option<&str>, address: Option<&str>) -> Option<Location> {
        Some(Location {
            lat_e7,
            lon_e7,
            name: name.map(str::to_owned),
            address: address.map(str::to_owned),
        })
    }

    #[test]
    fn a_location_is_degrees_times_ten_million_in_its_three_forms() {
        // Coordinates only; with an accuracy (read, not kept).
        assert_eq!(
            place("47.3769,8.5417"),
            at(473_769_000, 85_417_000, None, None)
        );
        assert_eq!(
            place("47.3769,8.5417,12.5"),
            at(473_769_000, 85_417_000, None, None)
        );
        // An address on the second line.
        assert_eq!(
            place("-33.8688,151.2093\nMain Street 1, Town"),
            at(
                -338_688_000,
                1_512_093_000,
                None,
                Some("Main Street 1, Town")
            )
        );
        // A name on the second line and the address on the third (with an accuracy).
        assert_eq!(
            place("48.8584,-122.4194,5\nHarbour\n1 Quay Street"),
            at(
                488_584_000,
                -1_224_194_000,
                Some("Harbour"),
                Some("1 Quay Street")
            )
        );
        // The edges of the globe fit; zero is a place.
        assert_eq!(place("90,180"), at(900_000_000, 1_800_000_000, None, None));
        assert_eq!(
            place("-90.0,-180.000"),
            at(-900_000_000, -1_800_000_000, None, None)
        );
        assert_eq!(place("0,0"), at(0, 0, None, None));
        // A sign and a bare point are decimals too; spaces around a number are ignored.
        assert_eq!(place("+1.5, -.5"), at(15_000_000, -5_000_000, None, None));
    }

    #[test]
    fn a_location_rounds_to_the_seventh_decimal() {
        assert_eq!(
            place("1.00000004,2"),
            at(10_000_000, 20_000_000, None, None)
        );
        assert_eq!(
            place("1.00000006,2"),
            at(10_000_001, 20_000_000, None, None)
        );
        assert_eq!(
            place("-1.00000006,2"),
            at(-10_000_001, 20_000_000, None, None)
        );
    }

    #[test]
    fn a_location_address_keeps_its_escaped_line_breaks_and_extra_lines() {
        // A literal backslash and n in the address is a line break.
        assert_eq!(
            place("1,2\nMain 1\\nSecond floor"),
            at(10_000_000, 20_000_000, None, Some("Main 1\nSecond floor"))
        );
        // More than two lines: the name is the second line, the address the rest joined by line breaks.
        assert_eq!(
            place("1,2\nName\nStreet 1\nCity\nCountry"),
            at(
                10_000_000,
                20_000_000,
                Some("Name"),
                Some("Street 1\nCity\nCountry")
            )
        );
        // An empty line is no value; a trailing line feed is no place.
        assert_eq!(place("1,2\n"), at(10_000_000, 20_000_000, None, None));
        assert_eq!(
            place("1,2\n\nStreet"),
            at(10_000_000, 20_000_000, None, Some("Street"))
        );
        // Multi-byte text is kept as it is.
        assert_eq!(
            place("1,2\nCafé 😀\nRue É 3"),
            at(10_000_000, 20_000_000, Some("Café 😀"), Some("Rue É 3"))
        );
    }

    #[test]
    fn a_body_that_is_no_location_is_none() {
        for bad in [
            "",
            "1",
            "1,",
            ",1",
            ",",
            "1,2,",
            "1,2,x",
            "1,2,3,4",
            "abc,1",
            "1,abc",
            "1;2",
            "1 2",
            "NaN,1",
            "1,inf",
            "inf,inf",
            "1e1,2",
            "0x10,2",
            "1.2.3,4",
            "--1,2",
            "-,2",
            ".,2",
            // Out of range: latitude over 90, longitude over 180, a hair over.
            "91,0",
            "-91,0",
            "0,181",
            "0,-181",
            "90.0000001,0",
            "0,180.0000001",
            "1000000,2",
            // The first line is not on the first line.
            "\n1,2",
            "Name\n1,2",
        ] {
            assert_eq!(place(bad), None, "{bad:?} was taken");
        }
        // Not UTF-8.
        assert_eq!(location(&[b'1', b',', b'2', b'\n', 0xff, 0xfe]), None);
    }

    // ---------------------------------------------------------------------------------------------------
    // TH5b B1: file (R3)
    // ---------------------------------------------------------------------------------------------------

    const BLOB: &str = "0123456789abcdef0123456789abcdef";
    const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    /// The minimal valid JSON of a file message: `b`, `k`, `m` (a JPEG) and `s`.
    fn base() -> serde_json::Value {
        serde_json::json!({ "b": BLOB, "k": KEY, "m": "image/jpeg", "s": 1234 })
    }

    fn with(fields: serde_json::Value) -> serde_json::Value {
        let mut v = base();
        for (key, value) in fields.as_object().expect("an object") {
            if value.is_null() {
                v.as_object_mut().expect("an object").remove(key);
            } else {
                v[key] = value.clone();
            }
        }
        v
    }

    fn bytes(v: &serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(v).expect("json")
    }

    fn read(v: &serde_json::Value) -> Option<ParsedFile> {
        file(&bytes(v), OWN, false)
    }

    fn kind_of_file(v: &serde_json::Value) -> AttachmentKind {
        read(v).expect("a file").attachment.kind
    }

    #[test]
    fn a_picture_is_an_image_with_every_field_of_its_json() {
        let json = serde_json::json!({
            "j": 1, "i": 1, "k": KEY, "b": BLOB, "t": "ffffffffffffffffffffffffffffffff",
            "p": "image/jpeg", "m": "image/jpeg", "n": "synthetic.jpg", "s": 123_456,
            "d": "a *synthetic* caption", "c": "0123456789abcdef0123456789abcdef",
            "x": { "w": 640, "h": 480, "a": false }
        });
        let parsed = read(&json).expect("a picture");
        assert!(!parsed.sticker);
        assert_eq!(
            parsed.attachment,
            Attachment {
                kind: AttachmentKind::Image,
                mime: "image/jpeg".to_owned(),
                size: 123_456,
                file_name: Some("synthetic.jpg".to_owned()),
                dims: Some((640, 480)),
                duration_ms: None,
                state: AttachmentState::Remote,
                remote: Some(BackendBlob::new(1, bytes(&json))),
                ..Attachment::default()
            }
        );
        // The caption is read as a text: markup removed, one range.
        let caption = parsed.caption.expect("a caption");
        assert_eq!(caption.body, "a synthetic caption");
        assert_eq!(caption.ranges.len(), 1);
        assert_eq!((caption.ranges[0].start, caption.ranges[0].len), (2, 9));
        assert_eq!(caption.ranges[0].kind, RangeKind::Bold);
        assert!(!parsed.mentions_me);
        // The pointer is the JSON exactly as received, byte for byte (keys, blob ids and all).
        let raw = br#"{"b":"0123456789abcdef0123456789abcdef","k":"00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff","m":"image/png","s":9, "j":1}"#;
        assert_eq!(
            file(raw, OWN, false).expect("a file").attachment.remote,
            Some(BackendBlob::new(1, raw.to_vec()))
        );
    }

    #[test]
    fn the_kind_follows_the_rendering_type_the_media_type_and_the_animated_flag() {
        // Rendering 1 (media): by media type.
        for (mime, kind) in [
            ("image/jpeg", AttachmentKind::Image),
            ("image/png", AttachmentKind::Image),
            ("image/webp", AttachmentKind::Image),
            ("image/gif", AttachmentKind::Gif),
            ("video/mp4", AttachmentKind::Video),
            ("video/quicktime", AttachmentKind::Video),
            ("audio/aac", AttachmentKind::Voice),
            ("audio/mp4", AttachmentKind::Voice),
            ("application/pdf", AttachmentKind::File),
            ("text/plain", AttachmentKind::File),
            ("application/octet-stream", AttachmentKind::File),
        ] {
            assert_eq!(
                kind_of_file(&with(serde_json::json!({ "j": 1, "m": mime }))),
                kind,
                "{mime} as media"
            );
        }
        // The animated flag makes a GIF of an image that is no GIF.
        assert_eq!(
            kind_of_file(&with(
                serde_json::json!({ "j": 1, "m": "image/webp", "x": { "a": true } })
            )),
            AttachmentKind::Gif
        );
        assert_eq!(
            kind_of_file(&with(
                serde_json::json!({ "j": 1, "m": "image/webp", "x": { "a": false } })
            )),
            AttachmentKind::Image
        );
        // P-L3: `a` is image metadata ("For images", Desktop reads it in its image arm only): on any other
        // type it changes nothing, so a video, a sound, a PDF or a text with `a: true` keeps its own kind.
        for (mime, kind) in [
            ("video/mp4", AttachmentKind::Video),
            ("video/quicktime", AttachmentKind::Video),
            ("audio/aac", AttachmentKind::Voice),
            ("application/pdf", AttachmentKind::File),
            ("text/plain", AttachmentKind::File),
        ] {
            assert_eq!(
                kind_of_file(&with(
                    serde_json::json!({ "j": 1, "m": mime, "x": { "a": true } })
                )),
                kind,
                "{mime} with the animated flag"
            );
        }
        // Rendering 0: a file whatever its type, also a picture, a video, a sound or an animation.
        for mime in [
            "image/jpeg",
            "video/mp4",
            "audio/aac",
            "image/gif",
            "application/pdf",
        ] {
            assert_eq!(
                kind_of_file(&with(serde_json::json!({
                    "j": 0, "m": mime, "x": { "a": true, "w": 10, "h": 10, "d": 3.0 }
                }))),
                AttachmentKind::File,
                "{mime} as a file"
            );
        }
        // Rendering 2: a sticker, an image.
        let sticker =
            read(&with(serde_json::json!({ "j": 2, "m": "image/webp" }))).expect("a sticker");
        assert!(sticker.sticker);
        assert_eq!(sticker.attachment.kind, AttachmentKind::Image);
        assert_eq!(sticker.attachment.mime, "image/webp");
        // P-L3: rendering 2 is a sticker for an `image/*` type ONLY (also an animated one, and a GIF); under
        // any other type it follows the rendering-1 rules, as Desktop's `file.ts` reads a sticker of a
        // video or a sound as a video or a sound and of anything else as a file.
        for mime in ["image/png", "image/gif", "image/jpeg"] {
            let one = read(&with(
                serde_json::json!({ "j": 2, "m": mime, "x": { "a": true } }),
            ))
            .expect("a sticker");
            assert!(one.sticker, "{mime} is not a sticker");
            assert_eq!(one.attachment.kind, AttachmentKind::Image, "{mime}");
        }
        for (mime, kind) in [
            ("video/mp4", AttachmentKind::Video),
            ("audio/aac", AttachmentKind::Voice),
            ("application/pdf", AttachmentKind::File),
            ("text/plain", AttachmentKind::File),
            ("application/octet-stream", AttachmentKind::File),
        ] {
            let one = read(&with(
                serde_json::json!({ "j": 2, "m": mime, "x": { "a": true, "w": 5, "h": 6, "d": 2.0 } }),
            ))
            .expect("a file");
            assert!(!one.sticker, "{mime} is a sticker");
            assert_eq!(one.attachment.kind, kind, "{mime} under rendering 2");
            // The kind decides the rest: a video has its sides and length, a sound its length, a file none.
            assert_eq!(
                (one.attachment.dims, one.attachment.duration_ms),
                match kind {
                    AttachmentKind::Video => (Some((5, 6)), Some(2000)),
                    AttachmentKind::Voice => (None, Some(2000)),
                    _ => (None, None),
                },
                "{mime} under rendering 2"
            );
        }
        // The other kinds are not stickers.
        assert!(
            !read(&with(serde_json::json!({ "j": 1 })))
                .expect("media")
                .sticker
        );
        assert!(
            !read(&with(serde_json::json!({ "j": 0 })))
                .expect("a file")
                .sticker
        );
    }

    #[test]
    fn without_j_the_deprecated_i_decides_and_an_unassigned_j_is_a_file() {
        // `j` absent: `i` 1 is media, anything else a file.
        assert_eq!(
            kind_of_file(&with(serde_json::json!({ "i": 1 }))),
            AttachmentKind::Image
        );
        assert_eq!(
            kind_of_file(&with(serde_json::json!({ "i": 0 }))),
            AttachmentKind::File
        );
        assert_eq!(kind_of_file(&base()), AttachmentKind::File, "no j and no i");
        assert_eq!(
            kind_of_file(&with(serde_json::json!({ "i": 2 }))),
            AttachmentKind::File
        );
        assert_eq!(
            kind_of_file(&with(serde_json::json!({ "i": "1" }))),
            AttachmentKind::File
        );
        // `j` null is not set.
        let mut by_hand = base();
        by_hand["j"] = serde_json::Value::Null;
        by_hand["i"] = serde_json::json!(1);
        assert_eq!(kind_of_file(&by_hand), AttachmentKind::Image, "j null, i 1");
        // `j` set to a value nobody assigned: a file, `i` or not.
        for j in [
            serde_json::json!(3),
            serde_json::json!(255),
            serde_json::json!(-1),
            serde_json::json!("1"),
            serde_json::json!(1.5),
            serde_json::json!(true),
        ] {
            let mut v = base();
            v["j"] = j.clone();
            v["i"] = serde_json::json!(1);
            assert_eq!(kind_of_file(&v), AttachmentKind::File, "j = {j}");
        }
    }

    #[test]
    fn the_media_type_is_lower_case_without_parameters() {
        let got = |m: &str| {
            read(&with(serde_json::json!({ "j": 1, "m": m })))
                .map(|f| (f.attachment.mime, f.attachment.kind))
        };
        assert_eq!(
            got("Image/JPEG; charset=binary"),
            Some(("image/jpeg".to_owned(), AttachmentKind::Image))
        );
        assert_eq!(
            got("  video/MP4  "),
            Some(("video/mp4".to_owned(), AttachmentKind::Video))
        );
        assert_eq!(got("; charset=x"), None, "no type before the parameters");
        assert_eq!(got(""), None);
        assert_eq!(got("   "), None);
    }

    #[test]
    fn dimensions_come_from_x_for_pictures_and_videos_only_and_need_both_sides() {
        let dims = |fields: serde_json::Value| read(&with(fields)).expect("a file").attachment.dims;
        let media =
            |mime: &str, x: serde_json::Value| serde_json::json!({ "j": 1, "m": mime, "x": x });
        assert_eq!(
            dims(media(
                "image/jpeg",
                serde_json::json!({ "w": 640, "h": 480 })
            )),
            Some((640, 480))
        );
        assert_eq!(
            dims(media(
                "video/mp4",
                serde_json::json!({ "w": 1920, "h": 1080, "d": 5 })
            )),
            Some((1920, 1080))
        );
        assert_eq!(
            dims(media("image/gif", serde_json::json!({ "w": 3, "h": 4 }))),
            Some((3, 4))
        );
        // The sticker's image has them too.
        assert_eq!(
            dims(serde_json::json!({ "j": 2, "m": "image/webp", "x": { "w": 512, "h": 512 } })),
            Some((512, 512))
        );
        // Audio and a file have none.
        assert_eq!(
            dims(media("audio/aac", serde_json::json!({ "w": 3, "h": 4 }))),
            None
        );
        assert_eq!(
            dims(serde_json::json!({ "j": 0, "x": { "w": 3, "h": 4 } })),
            None
        );
        // Zero, one side, a negative, a float, a text, or more than a `u32`: none.
        for x in [
            serde_json::json!({ "w": 0, "h": 480 }),
            serde_json::json!({ "w": 640, "h": 0 }),
            serde_json::json!({ "w": 640 }),
            serde_json::json!({ "h": 480 }),
            serde_json::json!({ "w": -640, "h": 480 }),
            serde_json::json!({ "w": 640.5, "h": 480 }),
            serde_json::json!({ "w": "640", "h": "480" }),
            serde_json::json!({ "w": 4_294_967_296_u64, "h": 480 }),
        ] {
            assert_eq!(dims(media("image/jpeg", x.clone())), None, "{x}");
        }
        // `x` that is no object is ignored.
        assert_eq!(
            dims(media("image/jpeg", serde_json::json!([640, 480]))),
            None
        );
    }

    #[test]
    fn the_duration_is_seconds_rounded_to_milliseconds_for_sound_and_video() {
        let ms = |mime: &str, d: serde_json::Value| {
            read(&with(
                serde_json::json!({ "j": 1, "m": mime, "x": { "d": d } }),
            ))
            .expect("a file")
            .attachment
            .duration_ms
        };
        assert_eq!(ms("audio/aac", serde_json::json!(12.5)), Some(12_500));
        assert_eq!(ms("video/mp4", serde_json::json!(3)), Some(3_000));
        assert_eq!(ms("audio/aac", serde_json::json!(0.0004)), Some(0));
        assert_eq!(ms("audio/aac", serde_json::json!(0.0006)), Some(1));
        assert_eq!(ms("audio/aac", serde_json::json!(1.2346)), Some(1_235));
        assert_eq!(ms("audio/aac", serde_json::json!(1.2344)), Some(1_234));
        // The largest `u32` of milliseconds, and the cut above it.
        assert_eq!(
            ms("video/mp4", serde_json::json!(4_294_967.295)),
            Some(u32::MAX)
        );
        assert_eq!(
            ms("video/mp4", serde_json::json!(4_294_967.294)),
            Some(4_294_967_294)
        );
        assert_eq!(ms("video/mp4", serde_json::json!(1e12)), Some(u32::MAX));
        // Nothing for a negative, a text, or an image.
        assert_eq!(ms("audio/aac", serde_json::json!(-1.0)), None);
        assert_eq!(ms("audio/aac", serde_json::json!("12")), None);
        assert_eq!(ms("image/jpeg", serde_json::json!(12.5)), None);
        // And none without `d`.
        assert_eq!(
            read(&with(serde_json::json!({ "j": 1, "m": "audio/aac" })))
                .expect("a file")
                .attachment
                .duration_ms,
            None
        );
        // The check on the function itself: the edge of the float.
        assert_eq!(millis(f64::NAN), None);
        assert_eq!(millis(f64::INFINITY), None);
        assert_eq!(millis(0.0), Some(0));
    }

    #[test]
    fn the_name_is_cut_at_a_character_boundary_within_256_bytes() {
        let name = |n: &str| {
            read(&with(serde_json::json!({ "n": n })))
                .expect("a file")
                .attachment
                .file_name
        };
        assert_eq!(name("report.pdf"), Some("report.pdf".to_owned()));
        // Exactly 256 bytes stays whole; 257 is cut.
        assert_eq!(name(&"a".repeat(256)), Some("a".repeat(256)));
        assert_eq!(name(&"a".repeat(257)), Some("a".repeat(256)));
        assert_eq!(name(&"a".repeat(1000)), Some("a".repeat(256)));
        // Three-byte characters: 85 of them are 255 bytes, the 86th would cross 256.
        assert_eq!(name(&"€".repeat(100)), Some("€".repeat(85)));
        // Two-byte characters after one byte: the cut falls inside the 128th, so 127 fit.
        let mixed = format!("a{}", "é".repeat(200));
        assert_eq!(name(&mixed), Some(format!("a{}", "é".repeat(127))));
        // No name, an empty name and a name that is no string are none.
        assert_eq!(name(""), None);
        assert_eq!(read(&base()).expect("a file").attachment.file_name, None);
        let mut v = base();
        v["n"] = serde_json::json!(5);
        assert_eq!(read(&v).expect("a file").attachment.file_name, None);
    }

    #[test]
    fn the_caption_is_a_text_with_mentions_and_markup_or_none() {
        let caption = |d: serde_json::Value, group: bool| {
            let mut v = base();
            v["d"] = d;
            let parsed = file(&bytes(&v), OWN, group).expect("a file");
            (parsed.caption, parsed.mentions_me)
        };
        // None for an empty caption and one that is no string.
        assert_eq!(caption(serde_json::json!(""), false), (None, false));
        assert_eq!(caption(serde_json::json!(5), false), (None, false));
        assert_eq!(read(&base()).expect("a file").caption, None);
        // A plain caption is the whole text, no quote form: a quote prefix stays in it.
        let quote = "> quote #0102030405060708\n\nthe caption";
        let (plain, _) = caption(serde_json::json!(quote), false);
        assert_eq!(plain, Some(Text::plain(quote)));
        // A mention of us and a markup span, in one caption.
        let (text, mine) = caption(serde_json::json!(format!("hi @[{OWN}], ~bye~")), false);
        let text = text.expect("a caption");
        assert!(mine, "the own ID was named in the caption");
        assert_eq!(text.body, "hi \u{fffc}, bye");
        assert_eq!(text.ranges.len(), 2);
        assert_eq!(
            text.ranges[0].kind,
            RangeKind::Mention(UserId::from_parts("threema", OWN).expect("a user"))
        );
        // "hi " 3 bytes, the placeholder 3, ", " 2: "bye" starts at byte 8.
        assert_eq!(
            (
                text.ranges[1].start,
                text.ranges[1].len,
                text.ranges[1].kind.clone()
            ),
            (8, 3, RangeKind::Strike)
        );
        // `@All` mentions us in a group only.
        let all = serde_json::json!("@[@@@@@@@@] look");
        assert!(!caption(all.clone(), false).1);
        assert!(caption(all, true).1);
        // Whitespace alone is a caption (it is text).
        assert_eq!(
            caption(serde_json::json!(" "), false).0,
            Some(Text::plain(" "))
        );
    }

    #[test]
    fn json_that_is_no_file_is_none() {
        let none = |v: &serde_json::Value| assert!(read(v).is_none(), "{v} was taken");
        // The valid base passes, so each of these fails for the one thing it changes.
        assert!(read(&base()).is_some());
        // Missing: b, k, m, s.
        for key in ["b", "k", "m", "s"] {
            none(&with(serde_json::json!({ key: serde_json::Value::Null })));
        }
        // The blob id: 31, 33 and 32 characters with a non-hex digit; a number.
        none(&with(serde_json::json!({ "b": &BLOB[1..] })));
        none(&with(serde_json::json!({ "b": format!("{BLOB}0") })));
        none(&with(
            serde_json::json!({ "b": format!("{}g", &BLOB[1..]) }),
        ));
        none(&with(serde_json::json!({ "b": 5 })));
        // The key: 63 and 65 characters, a non-hex digit; the blob id's length is not the key's.
        none(&with(serde_json::json!({ "k": &KEY[1..] })));
        none(&with(serde_json::json!({ "k": format!("{KEY}0") })));
        none(&with(serde_json::json!({ "k": format!("{}z", &KEY[1..]) })));
        none(&with(serde_json::json!({ "k": BLOB })));
        // The type: not a string, empty.
        none(&with(serde_json::json!({ "m": 5 })));
        none(&with(serde_json::json!({ "m": "" })));
        // The size: a text, negative, fractional, null.
        none(&with(serde_json::json!({ "s": "1234" })));
        none(&with(serde_json::json!({ "s": -1 })));
        none(&with(serde_json::json!({ "s": 12.5 })));
        let mut null_size = base();
        null_size["s"] = serde_json::Value::Null;
        none(&null_size);
        // Not an object, not JSON, empty, not UTF-8.
        assert!(file(b"[]", OWN, false).is_none());
        assert!(file(b"null", OWN, false).is_none());
        assert!(file(b"\"text\"", OWN, false).is_none());
        assert!(file(b"{", OWN, false).is_none());
        assert!(file(b"", OWN, false).is_none());
        assert!(file(b"{\"b\":", OWN, false).is_none());
        assert!(file(&[0xff, 0xfe, 0xfd], OWN, false).is_none());
        // Trailing garbage after the object.
        let mut trailing = bytes(&base());
        trailing.extend_from_slice(b" x");
        assert!(file(&trailing, OWN, false).is_none());
        // Upper-case hex digits and a size of zero are accepted.
        let upper =
            with(serde_json::json!({ "b": BLOB.to_uppercase(), "k": KEY.to_uppercase(), "s": 0 }));
        assert_eq!(read(&upper).expect("a file").attachment.size, 0);
    }

    #[test]
    fn a_deeply_nested_json_is_none_and_never_overflows_the_stack() {
        let mut deep = String::new();
        for _ in 0..100_000 {
            deep.push('[');
        }
        assert!(file(deep.as_bytes(), OWN, false).is_none());
        let wrapped = format!("{{\"x\":{deep}}}");
        assert!(file(wrapped.as_bytes(), OWN, false).is_none());
    }

    // ---- TH5b B2: the edit, the delete and the reaction (R6, R7, R8) ----------------------------------

    use super::test_wire as wire;

    const TARGET: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

    #[test]
    fn an_edit_is_the_target_and_the_text_read_like_a_text() {
        let parsed = edit(&wire::edit(TARGET, "new *bold* text"), OWN, false).expect("an edit");
        assert_eq!(
            parsed.target.to_bytes(),
            TARGET,
            "the target is not the wire bytes in order"
        );
        assert_eq!(parsed.text.body, "new bold text");
        assert_eq!(
            parsed.text.ranges,
            vec![TextRange {
                start: 4,
                len: 4,
                kind: RangeKind::Bold
            }]
        );
        // A mention is a placeholder with its range; `@All` counts only in a group (no flag here, but the range kind).
        let mention = format!("@[{OTHER}] and @[@@@@@@@@]");
        let parsed = edit(&wire::edit(TARGET, &mention), OWN, true).expect("an edit");
        assert_eq!(parsed.text.body, "\u{fffc} and \u{fffc}");
        assert_eq!(
            parsed.text.ranges[0].kind,
            RangeKind::Mention(UserId::from_parts("threema", OTHER).expect("a user"))
        );
        assert_eq!(parsed.text.ranges[1].kind, RangeKind::MentionAll);
    }

    #[test]
    fn an_edit_loses_a_valid_quote_prefix_and_an_empty_text_is_passed_on() {
        // Any message's quote is removed, the target's own too; a comment that goes on stays whole.
        for quoted in ["0a0a0a0a0a0a0a0a", "0102030405060708"] {
            let text = format!("> quote #{quoted}\n\nsecond\n\nline");
            let parsed = edit(&wire::edit(TARGET, &text), OWN, false).expect("an edit");
            assert_eq!(
                parsed.text.body, "second\n\nline",
                "the quote prefix stayed"
            );
        }
        // A prefix with no comment leaves an empty text; so does an empty edit; both are passed on.
        for text in ["> quote #0a0a0a0a0a0a0a0a\r\n\r\n", ""] {
            let parsed = edit(&wire::edit(TARGET, text), OWN, false).expect("an empty edit");
            assert!(parsed.text.body.is_empty() && parsed.text.ranges.is_empty());
        }
        // An edit with no text field at all (proto3 leaves an empty string out) is an empty text too.
        let bare = edit(&wire::delete(TARGET), OWN, false).expect("an edit without a text");
        assert!(bare.text.body.is_empty());
        // A text that only looks like a quote stays whole (a bad id).
        let looks = "> quote #0A0A0A0A0A0A0A0A\n\nx";
        assert_eq!(
            edit(&wire::edit(TARGET, looks), OWN, false)
                .expect("an edit")
                .text
                .body,
            looks
        );
    }

    #[test]
    fn an_edit_that_does_not_decode_is_edit_invalid() {
        for (what, bytes) in [
            ("a cut id", vec![0x09, 1, 2, 3]),
            ("a text that is no UTF-8", vec![0x12, 2, 0xff, 0xfe]),
            ("a text cut short", vec![0x12, 9, b'a']),
            ("the id with another wire type", vec![0x08, 1]),
        ] {
            assert_eq!(edit(&bytes, OWN, false), Err(RULE_EDIT_INVALID), "{what}");
        }
    }

    #[test]
    fn a_delete_is_the_target_id_and_one_that_does_not_decode_is_delete_invalid() {
        assert_eq!(
            delete(&wire::delete(TARGET)).map(MessageId::to_bytes),
            Ok(TARGET)
        );
        // No id at all: id 0, as proto3 reads an absent field.
        assert_eq!(delete(&[]).map(MessageId::to_bytes), Ok([0; 8]));
        assert_eq!(
            delete(&[0x09, 1, 2]).map(MessageId::to_bytes),
            Err(RULE_DELETE_INVALID)
        );
        assert_eq!(
            delete(&[0x08, 1]).map(MessageId::to_bytes),
            Err(RULE_DELETE_INVALID)
        );
    }

    #[test]
    fn a_reaction_applies_or_withdraws_one_emoji_of_one_to_64_bytes() {
        let thumbs = "\u{1f44d}".as_bytes();
        let applied = reaction(&wire::apply(TARGET, thumbs)).expect("apply");
        assert_eq!(
            (
                applied.target.to_bytes(),
                applied.emoji.as_str(),
                applied.remove
            ),
            (TARGET, "\u{1f44d}", false)
        );
        let withdrawn = reaction(&wire::withdraw(TARGET, thumbs)).expect("withdraw");
        assert_eq!(
            (
                withdrawn.target.to_bytes(),
                withdrawn.emoji.as_str(),
                withdrawn.remove
            ),
            (TARGET, "\u{1f44d}", true)
        );
        // 64 bytes pass (sixteen 4-byte emoji), 65 do not, in both directions; 1 byte passes.
        let sixteen = "\u{1f44d}".repeat(16);
        assert_eq!(sixteen.len(), 64);
        for build in [wire::apply, wire::withdraw] {
            assert_eq!(
                reaction(&build(TARGET, sixteen.as_bytes()))
                    .expect("64 bytes")
                    .emoji,
                sixteen
            );
            assert_eq!(
                reaction(&build(TARGET, &[b'x'; 65])),
                Err(RULE_REACTION_BYTES)
            );
            assert_eq!(reaction(&build(TARGET, b"")), Err(RULE_REACTION_BYTES));
            assert_eq!(reaction(&build(TARGET, b"+")).expect("1 byte").emoji, "+");
            // Bytes that are no UTF-8 (a lone continuation byte; a cut sequence).
            assert_eq!(reaction(&build(TARGET, &[0x80])), Err(RULE_REACTION_BYTES));
            assert_eq!(
                reaction(&build(TARGET, &[0xf0, 0x9f, 0x91])),
                Err(RULE_REACTION_BYTES)
            );
        }
    }

    #[test]
    fn a_reaction_with_no_action_or_that_does_not_decode_is_reaction_invalid() {
        assert_eq!(
            reaction(&wire::no_action(TARGET)),
            Err(RULE_REACTION_INVALID)
        );
        assert_eq!(reaction(&[]), Err(RULE_REACTION_INVALID));
        assert_eq!(reaction(&[0x09, 1, 2, 3]), Err(RULE_REACTION_INVALID));
        assert_eq!(reaction(&[0x12, 9, b'a']), Err(RULE_REACTION_INVALID));
    }

    #[test]
    fn the_modifiers_debug_prints_no_text_and_no_emoji() {
        let edit = edit(&wire::edit(TARGET, "synthetic secret text"), OWN, false).expect("an edit");
        let reaction = reaction(&wire::apply(TARGET, "\u{1f680}".as_bytes())).expect("a reaction");
        let shown = format!("{edit:?} {reaction:?}");
        assert!(
            !shown.contains("secret") && !shown.contains('\u{1f680}'),
            "a Debug output shows a body"
        );
    }

    // ---- TH5b B3: the delivery receipt and the typing indicator (R9, R12) -------------------------------

    /// R9: the status byte, then the ids, each its eight wire bytes in order (the id's text is those bytes in
    /// hex); a 9-byte body is one id.
    #[test]
    fn a_receipt_is_its_status_and_its_ids_in_order() {
        let first = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        let second = [0xf0, 0xe0, 0xd0, 0xc0, 0xb0, 0xa0, 0x90, 0x80];
        let body = [vec![2], first.to_vec(), second.to_vec()].concat();
        let parsed = receipt(&body).expect("a receipt");
        assert_eq!(parsed.status, 2);
        let texts: Vec<String> = parsed.ids.iter().map(|id| msg_id::format(*id)).collect();
        assert_eq!(texts, ["0102030405060708", "f0e0d0c0b0a09080"]);
        let one = receipt(&[[9].as_slice(), &first].concat()).expect("one id");
        assert_eq!((one.status, one.ids), (9, vec![id(first)]));
    }

    /// R9: no id (an empty or a 1-byte body) or a part of an id (10, 16 bytes) is `receipt-short`; the status
    /// is not judged here (the map does).
    #[test]
    fn a_receipt_without_whole_ids_is_receipt_short() {
        for length in [0usize, 1, 2, 8, 10, 16, 18] {
            assert_eq!(
                receipt(&vec![1; length]),
                Err(RULE_RECEIPT_SHORT),
                "{length} bytes"
            );
        }
        assert!(receipt(&[1; 17]).is_ok(), "17 bytes are two ids");
    }

    #[test]
    fn the_receipts_debug_prints_no_id() {
        let parsed = receipt(&[[1].as_slice(), &[0xab; 8]].concat()).expect("a receipt");
        let shown = format!("{parsed:?}");
        assert!(
            !shown.contains("ab") && !shown.contains("171"),
            "the Debug output shows an id"
        );
    }

    /// R12: one byte, 1 typing and 0 stopped; any other byte, no byte or more than one is `None`.
    #[test]
    fn typing_is_one_byte_zero_or_one() {
        assert_eq!(typing(&[1]), Some(true));
        assert_eq!(typing(&[0]), Some(false));
        for body in [&[2u8][..], &[0xff], &[], &[1, 0], &[0, 0]] {
            assert_eq!(typing(body), None, "{body:?}");
        }
    }
    /// TH6a S2 (the hand-over of TH5b): what the send core WRITES, the follower's parsers READ BACK. A text body is
    /// read as the same text (a multi-byte one, a line break, a quote prefix and a mention written the way the
    /// parser documents them), and a group member container yields its creator, its group id and its inner bytes.
    /// The writers are `send::body`'s; the other direction (the fake's builders against the writers, byte for byte)
    /// is the connection tests'.
    #[test]
    fn what_the_send_core_writes_is_read_back() {
        use libthreema::common::ThreemaId;

        use crate::send::body as written;

        for text in ["hello there", "é\n😀 ok", "line\n\nbreak"] {
            let parsed = super::text(&written::text(text), id([9; 8]), OWN, false)
                .expect("the writer's text parses");
            assert_eq!(parsed.text.body, text);
            assert_eq!(parsed.quoted, None);
        }
        // A quote and a mention are the text's own content; the writer does not touch them.
        let quoted = written::text(&format!("> quote #0102030405060708\n\nsee @[{OTHER}]"));
        let parsed = super::text(&quoted, id([9; 8]), OWN, false).expect("a quote");
        assert_eq!(
            parsed.quoted.map(MessageId::to_bytes),
            Some([1, 2, 3, 4, 5, 6, 7, 8])
        );
        assert_eq!(parsed.text.ranges.len(), 1);

        // The member container: creator, group id, inner bytes.
        let creator = ThreemaId::try_from(CREATOR).expect("an ID");
        let container = written::group_member_container(creator, 0x1122_3344_5566_7788, b"inner");
        let (group, inner) = member_container(&container).expect("the container parses");
        assert_eq!(group.creator, CREATOR);
        assert_eq!(group.group_id, 0x1122_3344_5566_7788);
        assert_eq!(inner, b"inner");
        // The text of a group message is the container around the text's bytes, and both parse.
        let group_text = written::group_member_container(creator, 7, &written::text("hi group"));
        let (_, inner) = member_container(&group_text).expect("a container");
        let parsed = super::text(inner, id([9; 8]), OWN, true).expect("the inner text");
        assert_eq!(parsed.text.body, "hi group");
        // A Gateway creator keeps its `*` through both.
        let gateway = ThreemaId::try_from("*A6GATE3").expect("a Gateway ID");
        let (group, _) = member_container(&written::group_member_container(gateway, 1, &[]))
            .expect("a Gateway creator");
        assert_eq!(group.creator, "*A6GATE3");
    }
}
