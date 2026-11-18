//! Forward, the core's half so far (forward line, steps D1 and D3): the conversion of one source
//! message for one target ([`convert`], pure) and the plan a UI builds its dialog from
//! ([`plan`], [`crate::facade::Query::ForwardPlan`]).
//!
//! The yes/no of "this message may be forwarded" is written once, in
//! [`crate::actions::forward_verdict`]; the plan states it per message and the row's flag
//! ([`crate::facade::MessageActions::forward`]) is the same function. Nothing here logs a text, a
//! name or a number: the `Debug` of every type is free of them.

use std::collections::{HashMap, HashSet};

use kourier_db::rusqlite::{Connection, OptionalExtension};
use kourier_model::text::validate_range;
use kourier_model::{
    AccountId, AccountState, Attachment, AttachmentId, AttachmentKind, AttachmentState, Body,
    CannotSend, ContactCard, ConversationFeatures, ConversationId, FormatKind, ForwardRules,
    MessageId, NetworkKind, RangeKind, Text, TextRange, UserId,
};
use serde::{Deserialize, Serialize};

use crate::actions::{self, ForwardVerdict};
use crate::error::Result;
use crate::facade::{MessageRow, TimelinePage};
use crate::query;
use crate::sql;

/// How a converted forward looks (the dialog's Look choice). A native forward ignores it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ForwardLook {
    /// The content alone.
    Plain,
    /// A header line ("Forwarded from ...") above the first message of a run.
    Header,
    /// The header, and every line of the body quoted with `> `.
    Quoted,
    /// The selection as one picture (the front end's; the conversion treats it like `Plain`).
    Picture,
}

/// One kind of change the conversion makes for a target, with how many times (messages or
/// ranges) or which attachments it concerns. `Debug` carries no text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForwardChange {
    /// Formatting ranges the target cannot carry were dropped (the count of ranges).
    FormattingDropped(u32),
    /// Mentions became plain "@Name" (the count of mentions).
    MentionsAsText(u32),
    /// Links the target cannot carry became text with the URL added (the count of links).
    LinksAsText(u32),
    /// Files go as a generic file: the target takes their kind in no other way.
    FileAsGeneric(Vec<AttachmentId>),
    /// Files over the target's size limit for their kind were left out.
    FileTooLarge(Vec<AttachmentId>),
    /// Files the target takes in no way (not even as files) were left out.
    FileRefused(Vec<AttachmentId>),
    /// Stickers go as pictures (the count of messages).
    StickerAsPicture(u32),
    /// Contact cards go as text (the count of messages).
    ContactAsText(u32),
    /// Locations go as text with a map link (the count of messages).
    LocationAsText(u32),
    /// Polls go as text, without votes (the count of messages).
    PollAsText(u32),
    /// The quote of a reply is not carried (the count of messages).
    ReplyNotCarried(u32),
}

/// Adds `new` to `into`: one entry per kind of change, counts added, attachment ids united.
pub(crate) fn merge_changes(into: &mut Vec<ForwardChange>, new: Vec<ForwardChange>) {
    use ForwardChange as C;
    for n in new {
        let slot = into
            .iter_mut()
            .find(|c| std::mem::discriminant(*c) == std::mem::discriminant(&n));
        match (slot, n) {
            (Some(C::FormattingDropped(a)), C::FormattingDropped(b))
            | (Some(C::MentionsAsText(a)), C::MentionsAsText(b))
            | (Some(C::LinksAsText(a)), C::LinksAsText(b))
            | (Some(C::StickerAsPicture(a)), C::StickerAsPicture(b))
            | (Some(C::ContactAsText(a)), C::ContactAsText(b))
            | (Some(C::LocationAsText(a)), C::LocationAsText(b))
            | (Some(C::PollAsText(a)), C::PollAsText(b))
            | (Some(C::ReplyNotCarried(a)), C::ReplyNotCarried(b)) => *a += b,
            (Some(C::FileAsGeneric(a)), C::FileAsGeneric(b))
            | (Some(C::FileTooLarge(a)), C::FileTooLarge(b))
            | (Some(C::FileRefused(a)), C::FileRefused(b)) => {
                for id in b {
                    if !a.contains(&id) {
                        a.push(id);
                    }
                }
            }
            (None, n) => into.push(n),
            (Some(_), _) => {}
        }
    }
}

/// One file of a converted message: a source attachment, sent as the kind it has or, with
/// `as_file`, as a generic file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardFile {
    /// The source attachment.
    pub attachment: AttachmentId,
    /// Sent as a generic file: the target refuses the attachment's own kind.
    pub as_file: bool,
}

/// One message to send to a target.
#[derive(Clone, Debug, PartialEq)]
pub enum ForwardPart {
    /// A text message.
    Text(Text),
    /// Files with an optional caption.
    Files {
        /// The files, in order.
        files: Vec<ForwardFile>,
        /// The caption (only where every file's kind takes one).
        caption: Option<Text>,
    },
    /// A contact card.
    Contact(ContactCard),
}

/// What the conversion made of one message for one target.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Converted {
    /// The messages to send, in order; empty when nothing is left.
    pub parts: Vec<ForwardPart>,
    /// What changed.
    pub changes: Vec<ForwardChange>,
}

/// What the target says it takes.
#[derive(Clone, Copy, Debug)]
pub struct TargetFacts<'a> {
    /// The target chat's features; `None` when its backend has stated none yet: no formatting
    /// is assumed, and every file goes as its own kind with no size limit (the backend decides,
    /// as for a send: `attachments::plan_files`).
    pub features: Option<&'a ConversationFeatures>,
    /// The target's network sends contact cards (`NetworkDescriptor::contact_cards`).
    pub contact_cards: bool,
}

/// How the conversion is asked to go.
#[derive(Clone, Copy)]
pub struct How<'a> {
    /// The target is on the source's account: a mention stays a mention where the target takes
    /// it; no header, no quoted look.
    pub native: bool,
    /// The look of a converted forward.
    pub look: ForwardLook,
    /// The UI's header line for the first message of a run; used only when not native and the
    /// look is `Header` or `Quoted`.
    pub header: Option<&'a Text>,
    /// The shown name of a mentioned person.
    pub names: &'a dyn Fn(&UserId) -> String,
}

fn supports(f: Option<&ConversationFeatures>, kind: FormatKind) -> bool {
    f.and_then(|f| f.formatting.get(&kind))
        .is_some_and(|s| s.is_usable())
}

struct Edit {
    start: usize,
    end: usize,
    with: String,
}

struct TextOut {
    text: Text,
    formatting: u32,
    mentions: u32,
    links: u32,
}

/// The text for the target: ranges where the target takes their kind, mentions as "@Name"
/// (all of them unless native), links as text with the URL.
fn convert_text(src: &Text, target: &TargetFacts<'_>, how: &How<'_>) -> TextOut {
    let f = target.features;
    let body = &src.body;
    let mut out = TextOut {
        text: Text::default(),
        formatting: 0,
        mentions: 0,
        links: 0,
    };
    let mut kept: Vec<&TextRange> = Vec::new();
    let mut edits: Vec<Edit> = Vec::new();
    // One mention per placeholder: the one the text shows.
    let shown = src.shown_mentions();
    for r in &shown {
        if how.native && supports(f, r.kind.format_kind()) {
            kept.push(r);
        } else {
            out.mentions += 1;
            let name = match &r.kind {
                RangeKind::Mention(u) => (how.names)(u),
                _ => "everyone".to_owned(),
            };
            let name: String = name
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect();
            edits.push(Edit {
                start: r.start as usize,
                end: r.start as usize + '\u{FFFC}'.len_utf8(),
                with: format!("@{name}"),
            });
        }
    }
    for r in src
        .ranges
        .iter()
        .filter(|r| validate_range(body, r).is_ok())
    {
        match &r.kind {
            RangeKind::Mention(_) | RangeKind::MentionAll => {}
            RangeKind::Link(url) => {
                if supports(f, FormatKind::Link) {
                    kept.push(r);
                } else {
                    out.links += 1;
                    let (s, e) = (r.start as usize, r.start as usize + r.len as usize);
                    if &body[s..e] != url {
                        edits.push(Edit {
                            start: e,
                            end: e,
                            with: format!(" ({url})"),
                        });
                    }
                }
            }
            other => {
                if supports(f, other.format_kind()) {
                    kept.push(r);
                } else {
                    out.formatting += 1;
                }
            }
        }
    }
    edits.sort_by_key(|e| (e.start, e.end));
    let mut text = String::with_capacity(body.len());
    let mut at = 0;
    for e in &edits {
        if e.start < at {
            continue;
        }
        text.push_str(&body[at..e.start]);
        text.push_str(&e.with);
        at = e.end;
    }
    text.push_str(&body[at..]);
    let shift = |p: usize| -> usize {
        let mut p2 = p as i64;
        for e in edits.iter().filter(|e| e.end <= p) {
            p2 += e.with.len() as i64 - (e.end - e.start) as i64;
        }
        p2 as usize
    };
    let ranges = kept
        .iter()
        .filter_map(|r| {
            let s = shift(r.start as usize);
            let e = shift(r.start as usize + r.len as usize);
            Some(TextRange {
                start: u32::try_from(s).ok()?,
                len: u32::try_from(e.checked_sub(s)?).ok()?,
                kind: r.kind.clone(),
            })
        })
        .collect();
    out.text = Text { body: text, ranges };
    let dropped = out.text.sanitize();
    debug_assert_eq!(dropped, 0, "the conversion made a range invalid");
    out
}

/// Every line of the text prefixed with "> " (ranges shifted).
fn quote(t: &mut Text) {
    if t.body.is_empty() {
        return;
    }
    let len = t.body.len();
    let mut starts = vec![0usize];
    starts.extend(
        t.body
            .match_indices('\n')
            .map(|(i, _)| i + 1)
            .filter(|p| *p < len),
    );
    let mut body = String::with_capacity(len + 2 * starts.len());
    let mut at = 0;
    for s in &starts {
        body.push_str(&t.body[at..*s]);
        body.push_str("> ");
        at = *s;
    }
    body.push_str(&t.body[at..]);
    for r in &mut t.ranges {
        let (s, e) = (r.start as usize, r.start as usize + r.len as usize);
        let s2 = s + 2 * starts.iter().filter(|p| **p <= s).count();
        let e2 = e + 2 * starts.iter().filter(|p| **p < e).count();
        r.start = s2 as u32;
        r.len = (e2 - s2) as u32;
    }
    t.body = body;
}

/// `header` as the first line, then `body` (ranges shifted).
fn with_header(header: &Text, body: Text) -> Text {
    if body.is_empty() {
        return header.clone();
    }
    let off = header.body.len() + 1;
    let mut ranges = header.ranges.clone();
    ranges.extend(body.ranges.into_iter().map(|r| TextRange {
        start: r.start + off as u32,
        len: r.len,
        kind: r.kind,
    }));
    Text {
        body: format!("{}\n{}", header.body, body.body),
        ranges,
    }
}

struct Conv<'a, 'b> {
    target: &'a TargetFacts<'a>,
    how: &'a How<'b>,
    header: Option<Text>,
    quoted: bool,
    changes: Vec<ForwardChange>,
}

impl Conv<'_, '_> {
    fn count(&mut self, c: ForwardChange) {
        merge_changes(&mut self.changes, vec![c]);
    }

    /// A source text converted for the target, with its changes noted (no header, no quote yet).
    fn text(&mut self, src: &Text) -> Text {
        let o = convert_text(src, self.target, self.how);
        for (n, c) in [
            (
                o.formatting,
                ForwardChange::FormattingDropped as fn(u32) -> _,
            ),
            (o.mentions, ForwardChange::MentionsAsText),
            (o.links, ForwardChange::LinksAsText),
        ] {
            if n > 0 {
                self.count(c(n));
            }
        }
        o.text
    }

    /// Quote and header on a finished text; `None` when nothing is left of it.
    fn decorate(&self, mut t: Text) -> Option<Text> {
        if self.quoted {
            quote(&mut t);
        }
        if let Some(h) = &self.header {
            t = with_header(h, t);
        }
        (!t.body.trim().is_empty()).then_some(t)
    }

    /// The header line alone, for a part that takes no caption.
    fn header_part(&self) -> Option<ForwardPart> {
        self.header.clone().map(ForwardPart::Text)
    }

    /// A text part (plain text content of a location, poll or contact).
    fn text_part(&mut self, plain: String) -> Vec<ForwardPart> {
        match self.decorate(Text::plain(plain)) {
            Some(t) => vec![ForwardPart::Text(t)],
            None => Vec::new(),
        }
    }
}

fn attachment_files(
    c: &mut Conv<'_, '_>,
    list: &[&Attachment],
    sticker: bool,
) -> (Vec<ForwardFile>, bool) {
    let f = c.target.features;
    let (mut generic, mut large, mut refused) = (Vec::new(), Vec::new(), Vec::new());
    let mut files = Vec::new();
    let mut all_caption = true;
    for a in list {
        let Some(id) = a.id else { continue };
        let animated = sticker
            && (a.kind == AttachmentKind::Gif
                || a.mime == "image/gif"
                || (a.mime == "image/webp" && a.duration_ms.is_some()));
        let Some(f) = f else {
            // No features stated: the file goes as it is and the backend decides.
            files.push(ForwardFile {
                attachment: id,
                as_file: false,
            });
            continue;
        };
        let own = f
            .files
            .get(&a.kind)
            .filter(|_| !(animated && !f.files.contains_key(&AttachmentKind::Gif)));
        let (limits, as_file) = match own {
            Some(l) => (*l, false),
            None => match f.files.get(&AttachmentKind::File) {
                Some(l) => (*l, true),
                None => {
                    refused.push(id);
                    continue;
                }
            },
        };
        if limits.max_size > 0 && a.size > limits.max_size {
            large.push(id);
            continue;
        }
        if as_file {
            generic.push(id);
        }
        all_caption &= limits.caption;
        files.push(ForwardFile {
            attachment: id,
            as_file,
        });
    }
    for (ids, make) in [
        (
            generic,
            ForwardChange::FileAsGeneric as fn(Vec<AttachmentId>) -> _,
        ),
        (large, ForwardChange::FileTooLarge),
        (refused, ForwardChange::FileRefused),
    ] {
        if !ids.is_empty() {
            c.count(make(ids));
        }
    }
    (files, all_caption)
}

fn media_parts(
    c: &mut Conv<'_, '_>,
    list: &[&Attachment],
    caption: Option<&Text>,
    sticker: bool,
) -> Vec<ForwardPart> {
    let (files, all_caption) = attachment_files(c, list, sticker);
    let caption = caption
        .filter(|t| !t.body.trim().is_empty())
        .map(|t| c.text(t));
    let mut parts = Vec::new();
    if files.is_empty() {
        if let Some(t) = caption.and_then(|t| c.decorate(t)) {
            parts.push(ForwardPart::Text(t));
        }
        return parts;
    }
    let decorated = match caption {
        Some(t) => c.decorate(t),
        None => c.header.clone(),
    };
    if all_caption {
        parts.push(ForwardPart::Files {
            files,
            caption: decorated,
        });
    } else {
        // No caption on these kinds: the text (and the header) goes first, as a text of its own.
        parts.extend(decorated.map(ForwardPart::Text));
        parts.push(ForwardPart::Files {
            files,
            caption: None,
        });
    }
    parts
}

/// `https://www.openstreetmap.org/...` for a point in 1e-7 degrees, in exact integer arithmetic.
fn map_link(lat_e7: i32, lon_e7: i32) -> String {
    fn deg(v: i32) -> String {
        let v = i64::from(v);
        let sign = if v < 0 { "-" } else { "" };
        let a = v.abs();
        format!("{sign}{}.{:07}", a / 10_000_000, a % 10_000_000)
    }
    let (lat, lon) = (deg(lat_e7), deg(lon_e7));
    format!("https://www.openstreetmap.org/?mlat={lat}&mlon={lon}#map=16/{lat}/{lon}")
}

/// One source message converted for one target (D3 of the forward line). Pure.
pub fn convert(body: &Body, target: &TargetFacts<'_>, how: How<'_>) -> Converted {
    let converted = !how.native;
    let header = (converted && matches!(how.look, ForwardLook::Header | ForwardLook::Quoted))
        .then_some(how.header)
        .flatten()
        .map(|h| {
            let mut t = convert_text(
                h,
                target,
                &How {
                    native: false,
                    ..how
                },
            )
            .text;
            if supports(target.features, FormatKind::Italic) && !t.body.is_empty() {
                t.ranges.push(TextRange {
                    start: 0,
                    len: t.body.len() as u32,
                    kind: RangeKind::Italic,
                });
            }
            t
        });
    let mut c = Conv {
        target,
        how: &how,
        header,
        quoted: converted && how.look == ForwardLook::Quoted,
        changes: Vec::new(),
    };
    let parts = match body {
        Body::Text(t) => {
            let t = c.text(t);
            c.decorate(t).map(ForwardPart::Text).into_iter().collect()
        }
        Body::Media {
            attachments,
            caption,
        } => {
            let list: Vec<&Attachment> = attachments.iter().filter(|a| !a.long_text).collect();
            media_parts(&mut c, &list, caption.as_ref(), false)
        }
        Body::Sticker(a) => {
            c.count(ForwardChange::StickerAsPicture(1));
            media_parts(&mut c, &[a.as_ref()], a.caption.as_ref(), true)
        }
        Body::Contact(card) => {
            if target.contact_cards {
                let mut p: Vec<ForwardPart> = c.header_part().into_iter().collect();
                p.push(ForwardPart::Contact(card.clone()));
                p
            } else {
                c.count(ForwardChange::ContactAsText(1));
                let mut bits: Vec<String> = Vec::new();
                bits.extend(card.name.clone().filter(|s| !s.is_empty()));
                bits.extend(card.organization.clone().filter(|s| !s.is_empty()));
                bits.extend(card.identifiers.iter().map(|i| i.value.clone()));
                c.text_part(bits.join(", "))
            }
        }
        Body::Location(l) => {
            c.count(ForwardChange::LocationAsText(1));
            let mut lines: Vec<String> = Vec::new();
            lines.extend(l.name.clone().filter(|s| !s.is_empty()));
            lines.extend(l.address.clone().filter(|s| !s.is_empty()));
            lines.push(map_link(l.lat_e7, l.lon_e7));
            c.text_part(lines.join("\n"))
        }
        Body::Poll(p) => {
            c.count(ForwardChange::PollAsText(1));
            let mut lines = vec![p.question.clone()];
            lines.extend(p.options.iter().map(|o| format!("\u{2022} {o}")));
            c.text_part(lines.join("\n"))
        }
        Body::System(_) | Body::Placeholder(_) | Body::Unsupported { .. } => Vec::new(),
    };
    Converted {
        parts,
        changes: c.changes,
    }
}

// ---- the plan ----

/// Why a target is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForwardRefusal {
    /// The chat is unknown or removed.
    UnknownChat,
    /// The chat's account is not linked.
    AccountNotLinked,
    /// The chat's features (or a block) say we cannot send in it.
    CannotSend(CannotSend),
    /// The chat's network is not registered in this run.
    NetworkOff,
    /// More chats of one account than the network allows in one forward (`ForwardRules`).
    TooManyChats,
    /// No message goes.
    NothingToSend,
    /// A message the UI asked to forward no longer goes (deleted, hidden, viewed once ...)
    /// since the UI read its plan.
    SourceChanged,
    /// A file of a going message is not here or lost, and the UI did not agree to leave it out.
    FileNotHere,
    /// The core failed to prepare or store this target's forward (nothing of it stays).
    Failed,
}

/// One asked message of a [`ForwardPlan`].
#[derive(Clone, Debug, PartialEq)]
pub struct PlannedForward {
    /// The asked id.
    pub id: MessageId,
    /// The row as the timeline gives it; `None` only for [`ForwardVerdict::Gone`].
    pub row: Option<MessageRow>,
    /// What forwarding does with it.
    pub verdict: ForwardVerdict,
}

/// One target of a [`ForwardPlan`].
#[derive(Clone, Debug, PartialEq)]
pub struct PlannedTarget {
    /// The chat.
    pub conv: ConversationId,
    /// Its account; `None` for an unknown chat.
    pub account: Option<AccountId>,
    /// Same account as the source chat: the network's own forward.
    pub native: bool,
    /// Why nothing goes to it.
    pub refused: Option<ForwardRefusal>,
    /// What the conversion changes (look `Plain`; the viewmodel adds the header and picture
    /// lines). Empty for a refused target.
    pub changes: Vec<ForwardChange>,
}

/// The answer to [`crate::facade::Query::ForwardPlan`].
#[derive(Clone, Debug, PartialEq)]
pub struct ForwardPlan {
    /// The source chat.
    pub source: ConversationId,
    /// The core clock (ms) when the plan was read.
    pub now: i64,
    /// The asked messages: the visible ones in timeline order, then the `Gone` ones.
    pub messages: Vec<PlannedForward>,
    /// The targets, in the order asked.
    pub targets: Vec<PlannedTarget>,
    /// Files of going messages that are not here and can be fetched.
    pub to_download: Vec<AttachmentId>,
    /// Files of going messages the network no longer has.
    pub lost: Vec<AttachmentId>,
    /// The forward rules of each target account's network (where it has any).
    pub limits: Vec<(AccountId, ForwardRules)>,
    /// A going message was forwarded many times under the source network's rules.
    pub many: bool,
}

/// What the plan needs of a registered network.
#[derive(Clone, Copy, Debug)]
pub(crate) struct NetFacts {
    pub contact_cards: bool,
    pub forwards: Option<ForwardRules>,
}

struct TargetRow {
    account: AccountId,
    blocked: bool,
    features: Option<ConversationFeatures>,
    active: bool,
    kind: NetworkKind,
}

/// The columns of [`target_row`] as stored: account, blocked, features, state, its reason,
/// network.
type TargetRaw = (i64, bool, Option<String>, String, Option<String>, String);

fn target_row(conn: &Connection, conv: ConversationId) -> Result<Option<TargetRow>> {
    let raw: Option<TargetRaw> = conn
        .prepare_cached(
            "SELECT c.account_id, c.blocked, c.features, a.state, a.state_reason, a.network
             FROM conversations c JOIN accounts a ON a.id = c.account_id WHERE c.id = ?1",
        )?
        .query_row([conv.0], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })
        .optional()?;
    let Some((account, blocked, features, state, reason, network)) = raw else {
        return Ok(None);
    };
    Ok(Some(TargetRow {
        account: AccountId(account),
        blocked,
        features: sql::from_json_opt(features)?,
        active: sql::account_state(&state, reason)? == AccountState::Active,
        kind: sql::network(&network)?,
    }))
}

/// What a forward job needs of a target chat.
pub(crate) struct TargetInfo {
    pub account: AccountId,
    pub kind: NetworkKind,
    pub features: Option<ConversationFeatures>,
}

/// The target chat's account, network and stored features; `None` for an unknown chat.
pub(crate) fn target_facts(conn: &Connection, conv: ConversationId) -> Result<Option<TargetInfo>> {
    Ok(target_row(conn, conv)?.map(|t| TargetInfo {
        account: t.account,
        kind: t.kind,
        features: t.features,
    }))
}

fn text_of(body: &Body) -> Option<&Text> {
    match body {
        Body::Text(t) => Some(t),
        Body::Media { caption, .. } => caption.as_ref(),
        Body::Sticker(a) => a.caption.as_ref(),
        _ => None,
    }
}

pub(crate) fn files_of(body: &Body) -> Vec<&Attachment> {
    match body {
        Body::Media { attachments, .. } => attachments.iter().filter(|a| !a.long_text).collect(),
        Body::Sticker(a) => vec![a.as_ref()],
        _ => Vec::new(),
    }
}

/// The shown name of each person mentioned in the texts of `rows` (one read per distinct
/// person; "someone" where there is none), read in the source `account`'s name space.
pub(crate) fn mention_names(
    conn: &Connection,
    account: Option<AccountId>,
    rows: &[&MessageRow],
) -> Result<HashMap<UserId, String>> {
    let mut names: HashMap<UserId, String> = HashMap::new();
    let Some(account) = account else {
        return Ok(names);
    };
    let own: Option<(String, Option<String>)> = conn
        .prepare_cached("SELECT own_ids, display_name FROM accounts WHERE id = ?1")?
        .query_row([account.0], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let (own_ids, own_name): (Vec<UserId>, Option<String>) = match own {
        Some((j, n)) => (sql::from_json(&j)?, n),
        None => (Vec::new(), None),
    };
    for row in rows {
        let Some(t) = text_of(&row.body) else {
            continue;
        };
        for r in t.shown_mentions() {
            let RangeKind::Mention(u) = &r.kind else {
                continue;
            };
            if names.contains_key(u) {
                continue;
            }
            let name = if own_ids.contains(u) {
                own_name.clone()
            } else {
                query::display_name(conn, account.0, u.as_str())?
            };
            names.insert(
                u.clone(),
                name.filter(|n| !n.trim().is_empty())
                    .unwrap_or_else(|| "someone".to_owned()),
            );
        }
    }
    Ok(names)
}

/// [`crate::facade::Query::ForwardPlan`]. The rows come THROUGH [`query::messages_by_ids`]; the
/// page is returned beside the plan so the runtime registers its media handles.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan(
    conn: &Connection,
    source: ConversationId,
    ids: &[MessageId],
    targets: &[ConversationId],
    now: i64,
    uploading: &HashSet<MessageId>,
    paused: Option<UserId>,
    registry: &dyn Fn(NetworkKind) -> Option<NetFacts>,
) -> Result<(ForwardPlan, TimelinePage)> {
    let src_net = crate::outbox::network_of(conn, source)?;
    let src_facts = src_net.and_then(|(_, k)| registry(k));
    let registered = src_net.is_none() || src_facts.is_some();
    let src_rules = src_facts.and_then(|f| f.forwards);
    let page = query::messages_by_ids(
        conn, source, ids, now, registered, uploading, paused, src_rules,
    )?;
    let src_features: Option<ConversationFeatures> = {
        let raw: Option<Option<String>> = conn
            .prepare_cached("SELECT features FROM conversations WHERE id = ?1")?
            .query_row([source.0], |r| r.get(0))
            .optional()?;
        sql::from_json_opt(raw.flatten())?
    };
    let mut messages: Vec<PlannedForward> = Vec::new();
    let mut seen: HashSet<i64> = HashSet::new();
    for row in &page.rows {
        seen.insert(row.id.0);
        let verdict = actions::forward_verdict(
            &row.body,
            row.deleted.as_ref().map(|_| "Deleted"),
            row.view_once,
            false,
            src_features.as_ref(),
        );
        messages.push(PlannedForward {
            id: row.id,
            row: Some(row.clone()),
            verdict,
        });
    }
    for id in ids {
        if seen.insert(id.0) {
            messages.push(PlannedForward {
                id: *id,
                row: None,
                verdict: ForwardVerdict::Gone,
            });
        }
    }
    let going: Vec<&MessageRow> = messages
        .iter()
        .filter(|m| m.verdict == ForwardVerdict::Goes)
        .filter_map(|m| m.row.as_ref())
        .collect();

    let names = mention_names(conn, src_net.map(|(a, _)| a), &going)?;
    let name_of = |u: &UserId| {
        names
            .get(u)
            .cloned()
            .unwrap_or_else(|| "someone".to_owned())
    };

    let mut to_download: Vec<AttachmentId> = Vec::new();
    let mut lost: Vec<AttachmentId> = Vec::new();
    for row in &going {
        for a in files_of(&row.body) {
            let Some(id) = a.id else { continue };
            let list = match a.state {
                AttachmentState::Local => continue,
                AttachmentState::Remote
                | AttachmentState::Downloading { .. }
                | AttachmentState::Asking
                | AttachmentState::Failed { retryable: true } => &mut to_download,
                _ => &mut lost,
            };
            if !list.contains(&id) {
                list.push(id);
            }
        }
    }
    let many = going.iter().any(|r| r.forwarded.is_some_and(|m| m.many));

    let any_goes = !going.is_empty();
    let mut out_targets: Vec<PlannedTarget> = Vec::new();
    let mut limits: Vec<(AccountId, ForwardRules)> = Vec::new();
    let mut used: HashMap<i64, usize> = HashMap::new();
    let mut native_used = 0usize;
    let mut done: HashSet<i64> = HashSet::new();
    for conv in targets {
        if !done.insert(conv.0) {
            continue;
        }
        let Some(t) = target_row(conn, *conv)? else {
            out_targets.push(PlannedTarget {
                conv: *conv,
                account: None,
                native: false,
                refused: Some(ForwardRefusal::UnknownChat),
                changes: Vec::new(),
            });
            continue;
        };
        let facts = registry(t.kind);
        let rules = facts.and_then(|f| f.forwards);
        if let Some(r) = rules
            && !limits.iter().any(|(a, _)| *a == t.account)
        {
            limits.push((t.account, r));
        }
        let native = src_net.is_some_and(|(a, _)| a == t.account);
        let cannot = t
            .features
            .as_ref()
            .and_then(|f| f.cannot_send.clone())
            .or_else(|| t.blocked.then_some(CannotSend::Blocked));
        let mut refused = if !t.active {
            Some(ForwardRefusal::AccountNotLinked)
        } else if let Some(c) = cannot {
            Some(ForwardRefusal::CannotSend(c))
        } else if facts.is_none() {
            Some(ForwardRefusal::NetworkOff)
        } else {
            None
        };
        if refused.is_none() {
            let over = rules
                .and_then(|r| r.max_chats)
                .is_some_and(|m| used.get(&t.account.0).copied().unwrap_or(0) >= usize::from(m));
            let over_many = native
                && many
                && src_rules.is_some_and(|r| native_used >= usize::from(r.many_max_chats));
            if over || over_many {
                refused = Some(ForwardRefusal::TooManyChats);
            } else if !any_goes {
                refused = Some(ForwardRefusal::NothingToSend);
            } else {
                *used.entry(t.account.0).or_default() += 1;
                if native {
                    native_used += 1;
                }
            }
        }
        let mut changes = Vec::new();
        if refused.is_none() {
            let target = TargetFacts {
                features: t.features.as_ref(),
                contact_cards: facts.is_some_and(|f| f.contact_cards),
            };
            for row in &going {
                let conv = convert(
                    &row.body,
                    &target,
                    How {
                        native,
                        look: ForwardLook::Plain,
                        header: None,
                        names: &name_of,
                    },
                );
                merge_changes(&mut changes, conv.changes);
                if row.reply.is_some() {
                    merge_changes(&mut changes, vec![ForwardChange::ReplyNotCarried(1)]);
                }
            }
        }
        out_targets.push(PlannedTarget {
            conv: *conv,
            account: Some(t.account),
            native,
            refused,
            changes,
        });
    }
    Ok((
        ForwardPlan {
            source,
            now,
            messages,
            targets: out_targets,
            to_download,
            lost,
            limits,
            many,
        },
        page,
    ))
}

#[cfg(test)]
mod tests;
