//! The records of `messages.jsonl`: one [`Item`] per exported message, written as JSON by
//! [`Item::json`] and as HTML by `html.rs`. Names live only in the people table: a record holds
//! person ids.

use std::collections::{BTreeSet, HashMap};

use kourier_core::facade::{ExportFileRefusal, ExportVerdict, PlannedMessage, QuoteState};
use kourier_model::text::validate_range;
use kourier_model::{
    AttachmentId, AttachmentKind, Body, CallDirection, CallMedia, CallState, Deletion, Placeholder,
    RangeKind, SystemEvent, Text, TextRange,
};
use serde_json::Value;

use super::entries::{Shown, shown_as};
use super::numbers::{FileClass, Ticks, class_of};
use super::people::{People, TableNames, shown_name};
use super::snapshot::is_own;
use crate::names::single_line;
use crate::preview::{
    attachment_label, body_summary, deleted_quote, deleted_text, disappearing_quote,
    left_out_quote, missing_quote, placeholder_summary, system_text, unsupported_label,
};
use crate::richtext::{allowed_href, find_urls, href_of_match};
use crate::save_name::clean_name;
use crate::time::{Calendar, civil_from_days};

/// The most characters of a quote's excerpt.
pub const EXCERPT_CHARS: usize = 200;

/// A small ordered JSON value: objects keep the order the keys are written in.
pub(crate) enum J {
    Raw(Value),
    Obj(Vec<(&'static str, J)>),
    Arr(Vec<J>),
}

impl J {
    pub(crate) fn s(x: &str) -> J {
        J::Raw(Value::String(x.to_owned()))
    }
    pub(crate) fn n(x: i64) -> J {
        J::Raw(Value::from(x))
    }
    pub(crate) fn b(x: bool) -> J {
        J::Raw(Value::Bool(x))
    }
    pub(crate) fn write(&self, out: &mut String) {
        match self {
            J::Raw(v) => {
                let text = serde_json::to_string(v).unwrap_or_else(|_| "null".into());
                push_line_safe(out, &text);
            }
            J::Obj(fields) => {
                out.push('{');
                for (i, (k, v)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push('"');
                    out.push_str(k);
                    out.push_str("\":");
                    v.write(out);
                }
                out.push('}');
            }
            J::Arr(items) => {
                out.push('[');
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write(out);
                }
                out.push(']');
            }
        }
    }
}

/// Appends JSON text with U+2028, U+2029 and U+0085 escaped: a reader that splits lines on them
/// (Python's `splitlines`, some JS readers) still sees one record per line. They occur only
/// inside JSON strings, where the escape is the same character.
fn push_line_safe(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            '\u{0085}' => out.push_str("\\u0085"),
            c => out.push(c),
        }
    }
}

/// A piece of a text with one set of attributes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Run {
    pub text: String,
    pub styles: Vec<&'static str>,
    /// The person id of a mention (`me` too), or `everyone`.
    pub mention: Option<String>,
    pub href: Option<String>,
}

/// A text as the export writes it.
#[derive(Clone, Debug, Default)]
pub(crate) struct TextOut {
    pub plain: String,
    pub runs: Vec<Run>,
    /// `runs` is written (the text has a style, a mention or a link).
    pub styled: bool,
}

impl TextOut {
    fn json(&self) -> J {
        let mut f = vec![("plain", J::s(&self.plain))];
        if self.styled {
            let runs = self
                .runs
                .iter()
                .map(|r| {
                    let mut o = vec![("t", J::s(&r.text))];
                    if !r.styles.is_empty() {
                        o.push(("s", J::Arr(r.styles.iter().map(|s| J::s(s)).collect())));
                    }
                    if let Some(m) = &r.mention {
                        o.push(("mention", J::s(m)));
                    }
                    if let Some(h) = &r.href {
                        o.push(("link", J::s(h)));
                    }
                    J::Obj(o)
                })
                .collect();
            f.push(("runs", J::Arr(runs)));
        }
        J::Obj(f)
    }
}

const STYLE_ORDER: [&str; 5] = ["bold", "italic", "strike", "mono", "spoiler"];

fn style_name(k: &RangeKind) -> Option<&'static str> {
    match k {
        RangeKind::Bold => Some("bold"),
        RangeKind::Italic => Some("italic"),
        RangeKind::Strike => Some("strike"),
        RangeKind::Mono => Some("mono"),
        RangeKind::Spoiler => Some("spoiler"),
        _ => None,
    }
}

/// The one http/https predicate of the export's links: the href when it is an `http` or `https`
/// URL, else `None`. Both documents take their links through it (`allowed_href` also allows
/// `sgnl:` and other schemes, which may carry a phone number).
pub(crate) fn http_href(href: &str) -> Option<&str> {
    let h = allowed_href(href)?;
    (h.starts_with("http://") || h.starts_with("https://")).then_some(h)
}

/// A text cut into runs. Spoilers are written out; a mention is `@name` (the table's name).
///
/// One sweep over the cuts: the range starts and ends are sorted, the ranges that cover the window
/// are an active set (a counter per style, the links ordered by length), a mention is looked up
/// by its start and the URLs are walked with one moving index.
pub(crate) fn text_out(text: &Text, people: &People) -> TextOut {
    let body = text.body.as_str();
    let valid: Vec<&TextRange> = text
        .ranges
        .iter()
        .filter(|r| r.len > 0 && validate_range(body, r).is_ok())
        .collect();
    let mut mentions: HashMap<usize, &TextRange> = HashMap::new();
    for r in text.shown_mentions() {
        mentions.entry(r.start as usize).or_insert(r);
    }
    let mut urls = find_urls(body);
    urls.sort_unstable();
    let mut cuts: BTreeSet<usize> = BTreeSet::from([0, body.len()]);
    for r in &valid {
        cuts.insert(r.start as usize);
        cuts.insert(r.start as usize + r.len as usize);
    }
    for (s, e) in &urls {
        cuts.insert(*s);
        cuts.insert(*e);
    }
    let end_of = |i: usize| valid[i].start as usize + valid[i].len as usize;
    let mut by_start: Vec<usize> = (0..valid.len()).collect();
    by_start.sort_by_key(|&i| (valid[i].start, i));
    let mut by_end: Vec<usize> = (0..valid.len()).collect();
    by_end.sort_by_key(|&i| (end_of(i), i));
    let (mut next_start, mut next_end, mut next_url) = (0, 0, 0);
    let mut style_count = [0usize; STYLE_ORDER.len()];
    // The covering links, shortest first (the first of equal length wins).
    let mut links: BTreeSet<(usize, usize)> = BTreeSet::new();
    let link_of = |i: usize| match &valid[i].kind {
        RangeKind::Link(url) => http_href(url.trim()),
        _ => None,
    };
    let style_of = |i: usize| {
        style_name(&valid[i].kind).and_then(|n| STYLE_ORDER.iter().position(|x| *x == n))
    };

    let cuts: Vec<usize> = cuts.into_iter().collect();
    let mut runs: Vec<Run> = Vec::new();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        while next_end < by_end.len() && end_of(by_end[next_end]) <= a {
            let i = by_end[next_end];
            next_end += 1;
            if let Some(k) = style_of(i) {
                style_count[k] = style_count[k].saturating_sub(1);
            } else if link_of(i).is_some() {
                links.remove(&(valid[i].len as usize, i));
            }
        }
        while next_start < by_start.len() && (valid[by_start[next_start]].start as usize) <= a {
            let i = by_start[next_start];
            next_start += 1;
            if let Some(k) = style_of(i) {
                style_count[k] += 1;
            } else if link_of(i).is_some() {
                links.insert((valid[i].len as usize, i));
            }
        }
        while next_url < urls.len() && urls[next_url].1 <= a {
            next_url += 1;
        }
        let mention = mentions.get(&a);
        let (piece, mention_id) = match mention.map(|r| &r.kind) {
            Some(RangeKind::Mention(u)) => (
                format!("@{}", people.name_of(Some(u))),
                Some(people.lookup_id(Some(u))),
            ),
            Some(_) => ("@everyone".to_owned(), Some("everyone".to_owned())),
            None => (
                body[a..b].replace(kourier_model::text::MENTION_PLACEHOLDER, ""),
                None,
            ),
        };
        if piece.is_empty() {
            continue;
        }
        let styles: Vec<&'static str> = STYLE_ORDER
            .iter()
            .enumerate()
            .filter(|(k, _)| style_count[*k] > 0)
            .map(|(_, s)| *s)
            .collect();
        let mut href = links
            .iter()
            .next()
            .and_then(|(_, i)| link_of(*i))
            .map(str::to_owned);
        if href.is_none()
            && let Some((s, e)) = urls.get(next_url)
            && *s <= a
            && b <= *e
        {
            let h = href_of_match(&body[*s..*e]);
            href = http_href(&h).map(str::to_owned);
        }
        if mention_id.is_some() {
            href = None;
        }
        let run = Run {
            text: piece,
            styles,
            mention: mention_id,
            href,
        };
        match runs.last_mut() {
            Some(last)
                if last.mention.is_none()
                    && run.mention.is_none()
                    && last.styles == run.styles
                    && last.href == run.href =>
            {
                last.text.push_str(&run.text);
            }
            _ => runs.push(run),
        }
    }
    let plain: String = runs.iter().map(|r| r.text.as_str()).collect();
    let styled = runs
        .iter()
        .any(|r| !r.styles.is_empty() || r.mention.is_some() || r.href.is_some());
    TextOut {
        plain,
        runs,
        styled,
    }
}

/// A quote.
#[derive(Clone, Debug)]
pub(crate) struct ReplyOut {
    pub id: Option<String>,
    pub state: &'static str,
    pub author: Option<String>,
    pub excerpt: String,
}

/// One file of a message.
#[derive(Clone, Debug)]
pub(crate) struct FileOut {
    pub name: Option<String>,
    pub path: Option<String>,
    pub kind: &'static str,
    pub mime: String,
    pub size: u64,
    pub dims: Option<(u32, u32)>,
    pub duration_ms: Option<u32>,
    pub caption: Option<String>,
    pub state: &'static str,
    /// How the page shows the entry ([`shown_as`]): a picture, a link or text only.
    pub shown: Shown,
    /// Counted in `left_out.kinds` (a kind the ticks left out).
    pub kind_left_out: bool,
}

/// One exported message.
pub(crate) struct Item {
    pub id: String,
    pub ts: i64,
    pub time: String,
    pub own: bool,
    pub author: Option<String>,
    pub author_name: Option<String>,
    pub kind: &'static str,
    pub text: Option<TextOut>,
    pub reply: Option<ReplyOut>,
    pub reply_author_name: Option<String>,
    pub edited_at: Option<String>,
    pub edits: Vec<(String, TextOut)>,
    pub reactions: Vec<(String, Vec<String>)>,
    pub files: Vec<FileOut>,
    pub special: Option<(&'static str, J)>,
    /// The timeline's words for a notice, call, deleted line, stand-in or card.
    pub words: String,
}

impl Item {
    pub(crate) fn json(&self) -> String {
        let mut f: Vec<(&'static str, J)> = vec![
            ("id", J::s(&self.id)),
            ("time", J::s(&self.time)),
            ("ts", J::n(self.ts)),
        ];
        if let Some(a) = &self.author {
            f.push(("author", J::s(a)));
        }
        f.push(("kind", J::s(self.kind)));
        if let Some(t) = &self.text {
            f.push(("text", t.json()));
        }
        if let Some(r) = &self.reply {
            let mut o = Vec::new();
            if let Some(id) = &r.id {
                o.push(("id", J::s(id)));
            }
            o.push(("state", J::s(r.state)));
            if let Some(a) = &r.author {
                o.push(("author", J::s(a)));
            }
            o.push(("excerpt", J::s(&r.excerpt)));
            f.push(("reply_to", J::Obj(o)));
        }
        if let Some(e) = &self.edited_at {
            f.push(("edited_at", J::s(e)));
        }
        if !self.edits.is_empty() {
            let edits = self
                .edits
                .iter()
                .map(|(at, t)| J::Obj(vec![("at", J::s(at)), ("plain", J::s(&t.plain))]))
                .collect();
            f.push(("edits", J::Arr(edits)));
        }
        if !self.reactions.is_empty() {
            let r = self
                .reactions
                .iter()
                .map(|(e, by)| {
                    J::Obj(vec![
                        ("emoji", J::s(e)),
                        ("by", J::Arr(by.iter().map(|b| J::s(b)).collect())),
                    ])
                })
                .collect();
            f.push(("reactions", J::Arr(r)));
        }
        if !self.files.is_empty() {
            let files = self.files.iter().map(FileOut::json).collect();
            f.push(("files", J::Arr(files)));
        }
        if let Some((k, v)) = &self.special {
            f.push((
                k,
                J::Obj(match v {
                    J::Obj(o) => o.iter().map(|(k, v)| (*k, v.clone_j())).collect(),
                    _ => Vec::new(),
                }),
            ));
        }
        let mut out = String::new();
        J::Obj(f).write(&mut out);
        out
    }
}

impl J {
    fn clone_j(&self) -> J {
        match self {
            J::Raw(v) => J::Raw(v.clone()),
            J::Obj(o) => J::Obj(o.iter().map(|(k, v)| (*k, v.clone_j())).collect()),
            J::Arr(a) => J::Arr(a.iter().map(J::clone_j).collect()),
        }
    }
}

impl FileOut {
    fn json(&self) -> J {
        let opt = |s: &Option<String>| s.as_deref().map_or(J::Raw(Value::Null), J::s);
        let mut o = vec![
            ("name", opt(&self.name)),
            ("path", opt(&self.path)),
            ("kind", J::s(self.kind)),
            ("mime", J::s(&self.mime)),
            ("size", J::Raw(Value::from(self.size))),
        ];
        if let Some((w, h)) = self.dims {
            o.push(("dims", J::Arr(vec![J::n(i64::from(w)), J::n(i64::from(h))])));
        }
        if let Some(d) = self.duration_ms {
            o.push(("duration_ms", J::n(i64::from(d))));
        }
        if let Some(c) = &self.caption {
            o.push(("caption", J::s(c)));
        }
        o.push(("state", J::s(self.state)));
        J::Obj(o)
    }
}

/// `ts` as ISO 8601 with the local offset at that instant.
pub(crate) fn iso(ts: i64, cal: &dyn Calendar) -> String {
    let off = cal.utc_offset_s(ts);
    let local = ts.saturating_add(i64::from(off) * 1000);
    let days = local.div_euclid(86_400_000);
    let sec = local.rem_euclid(86_400_000) / 1000;
    let d = civil_from_days(days);
    let sign = if off < 0 { '-' } else { '+' };
    let a = off.unsigned_abs();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{sign}{:02}:{:02}",
        d.year,
        d.month,
        d.day,
        sec / 3600,
        sec % 3600 / 60,
        sec % 60,
        a / 3600,
        a % 3600 / 60
    )
}

/// The offset as `+03:00`.
pub(crate) fn offset_label(off: i32) -> String {
    let sign = if off < 0 { '-' } else { '+' };
    let a = off.unsigned_abs();
    format!("{sign}{:02}:{:02}", a / 3600, a % 3600 / 60)
}

fn cut_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", s[..i].trim_end()),
        None => s.to_owned(),
    }
}

/// Whether the file of a planned message goes into the archive with these ticks. A deleted row
/// carries no file, whatever the plan lists (the one place of that rule).
pub(crate) fn file_goes(
    pm: &PlannedMessage,
    f: &kourier_core::facade::PlannedFile,
    t: &Ticks,
) -> bool {
    pm.row.deleted.is_none() && f.accepted.is_ok() && t.has(class_of(&pm.row.body, f.kind))
}

fn kind_name(body: &Body, k: AttachmentKind) -> &'static str {
    if matches!(body, Body::Sticker(_)) {
        return "sticker";
    }
    match k {
        AttachmentKind::Image => "image",
        AttachmentKind::Video => "video",
        AttachmentKind::Audio => "audio",
        AttachmentKind::Voice => "voice",
        AttachmentKind::Gif => "gif",
        AttachmentKind::File => "file",
    }
}

fn call_json(line: &kourier_model::CallLine, people: &People) -> J {
    let by = match (&line.by, line.direction) {
        (Some(u), _) => Some(people.lookup_id(Some(u))),
        (None, CallDirection::Outgoing) => Some("me".to_owned()),
        (None, CallDirection::Incoming) => None,
    };
    let mut o = vec![(
        "direction",
        J::s(match line.direction {
            CallDirection::Incoming => "incoming",
            CallDirection::Outgoing => "outgoing",
        }),
    )];
    if let Some(m) = line.media {
        o.push((
            "media",
            J::s(match m {
                CallMedia::Voice => "voice",
                CallMedia::Video => "video",
            }),
        ));
    }
    o.push((
        "state",
        J::s(match line.state {
            CallState::Started => "started",
            CallState::Ringing { .. } => "ringing",
            CallState::Missed => "missed",
            CallState::Unanswered => "unanswered",
            CallState::Declined => "declined",
            CallState::Answered => "answered",
        }),
    ));
    o.push(("group", J::b(line.group)));
    if let Some(b) = by {
        o.push(("by", J::s(&b)));
    }
    J::Obj(o)
}

fn notice_json(e: &SystemEvent, people: &People) -> J {
    let id = |u: &kourier_model::UserId| J::s(&people.lookup_id(Some(u)));
    let ids = |us: &[kourier_model::UserId]| J::Arr(us.iter().map(id).collect());
    #[allow(clippy::type_complexity)]
    let (event, by, users, title, timer, verified): (
        &str,
        Option<&kourier_model::UserId>,
        Vec<kourier_model::UserId>,
        Option<&String>,
        Option<u64>,
        Option<bool>,
    ) = match e {
        SystemEvent::MembersAdded { by, users } => (
            "members_added",
            by.as_ref(),
            users.clone(),
            None,
            None,
            None,
        ),
        SystemEvent::MembersRemoved { by, users } => (
            "members_removed",
            by.as_ref(),
            users.clone(),
            None,
            None,
            None,
        ),
        SystemEvent::MemberLeft { user } => {
            ("member_left", None, vec![user.clone()], None, None, None)
        }
        SystemEvent::TitleChanged { by, title } => (
            "title_changed",
            by.as_ref(),
            vec![],
            Some(title),
            None,
            None,
        ),
        SystemEvent::AvatarChanged { by } => {
            ("avatar_changed", by.as_ref(), vec![], None, None, None)
        }
        SystemEvent::DescriptionChanged { by } => {
            ("description_changed", by.as_ref(), vec![], None, None, None)
        }
        SystemEvent::TimerChanged { by, timer } => (
            "timer_changed",
            by.as_ref(),
            vec![],
            None,
            Some(timer.map_or(0, |t| t.as_secs())),
            None,
        ),
        SystemEvent::IdentityKeyChanged { user } => (
            "identity_key_changed",
            None,
            vec![user.clone()],
            None,
            None,
            None,
        ),
        SystemEvent::SessionRefreshed { user } => (
            "session_refreshed",
            None,
            vec![user.clone()],
            None,
            None,
            None,
        ),
        SystemEvent::GroupCreated { by } => {
            ("group_created", by.as_ref(), vec![], None, None, None)
        }
        SystemEvent::MessageRequestAccepted => {
            ("message_request_accepted", None, vec![], None, None, None)
        }
        SystemEvent::Joined => ("joined", None, vec![], None, None, None),
        SystemEvent::NumberChanged { user } => {
            ("number_changed", None, vec![user.clone()], None, None, None)
        }
        SystemEvent::VerifiedChanged { user, verified, .. } => (
            "verified_changed",
            None,
            vec![user.clone()],
            None,
            None,
            Some(*verified),
        ),
        SystemEvent::Call(_) => ("call", None, vec![], None, None, None),
    };
    // `by: None` of these events means us.
    let by_is_us = matches!(
        e,
        SystemEvent::MembersAdded { by: None, .. }
            | SystemEvent::MembersRemoved { by: None, .. }
            | SystemEvent::TitleChanged { by: None, .. }
            | SystemEvent::AvatarChanged { by: None }
            | SystemEvent::DescriptionChanged { by: None }
            | SystemEvent::TimerChanged { by: None, .. }
            | SystemEvent::GroupCreated { by: None }
    );
    let mut o = vec![("event", J::s(event))];
    if let Some(b) = by {
        o.push(("by", id(b)));
    } else if by_is_us {
        o.push(("by", J::s("me")));
    }
    if !users.is_empty() {
        o.push(("users", ids(&users)));
    }
    if let Some(t) = title {
        o.push(("title", J::s(&single_line(t))));
    }
    if let Some(t) = timer {
        o.push(("timer", J::Raw(Value::from(t))));
    }
    if let Some(v) = verified {
        o.push(("verified", J::b(v)));
    }
    J::Obj(o)
}

/// Makes the item of one message. `entries` maps the attachments that go into the archive to
/// their entry names.
pub(crate) fn item(
    pm: &PlannedMessage,
    people: &People,
    cal: &dyn Calendar,
    entries: &HashMap<AttachmentId, String>,
    ticks: &Ticks,
) -> Item {
    let row = &pm.row;
    let table = TableNames(people);
    let own = is_own(pm);
    let author = match (&row.body, &row.author) {
        (Body::System(_), _) => None,
        (_, Some(u)) => Some(people.lookup_id(Some(u))),
        (_, None) if own => Some("me".to_owned()),
        _ => None,
    };
    let author_name = match (&row.body, &row.author) {
        (Body::System(_), _) => None,
        (_, Some(u)) => Some(people.name_of(Some(u))),
        (_, None) if own => Some(people.name_of(None)),
        _ => None,
    };
    let mut it = Item {
        id: format!("m{}", row.id.0),
        ts: row.sent_at,
        time: iso(row.sent_at, cal),
        own,
        author,
        author_name,
        kind: "text",
        text: None,
        reply: None,
        reply_author_name: None,
        edited_at: None,
        edits: Vec::new(),
        reactions: Vec::new(),
        files: Vec::new(),
        special: None,
        words: String::new(),
    };

    // The quote.
    if let Some(r) = &row.reply {
        let author = if r.own {
            Some("me".to_owned())
        } else {
            r.author.as_ref().map(|u| people.lookup_id(Some(u)))
        };
        it.reply_author_name = if r.own {
            Some(people.name_of(None))
        } else {
            r.author.as_ref().map(|u| people.name_of(Some(u)))
        };
        // The quoted original's own verdict (the plan strips a left-out quote; a disappearing
        // one shows only with the tick).
        let excerpt = match &r.text {
            _ if matches!(pm.quote, Some(ExportVerdict::LeftOut(_))) => left_out_quote().text,
            _ if pm.quote == Some(ExportVerdict::NeedsTick) && !ticks.disappearing => {
                disappearing_quote().text
            }
            Some(t) if !t.body.trim().is_empty() => text_out(t, people).plain,
            _ => match (r.attachment, &r.file_name) {
                (Some(AttachmentKind::File), Some(n)) => n.expose().clone(),
                (Some(k), _) => attachment_label(k, 1).text,
                (None, _) if r.state == QuoteState::Deleted => deleted_quote().text,
                (None, _) => missing_quote().text,
            },
        };
        it.reply = Some(ReplyOut {
            id: r.target.map(|t| format!("m{}", t.0)),
            state: match r.state {
                QuoteState::Known => "known",
                QuoteState::Missing => "missing",
                QuoteState::Deleted => "deleted",
            },
            author,
            excerpt: cut_chars(&single_line(&excerpt), EXCERPT_CHARS),
        });
    }

    // Edits: the earlier versions, oldest first (the current one is the text).
    it.edited_at = row.edited_at.map(|t| iso(t, cal));
    if pm.revisions.len() > 1 {
        for rev in &pm.revisions[..pm.revisions.len() - 1] {
            let t = rev
                .text
                .as_ref()
                .map_or_else(TextOut::default, |t| text_out(t, people));
            it.edits.push((iso(rev.at, cal), t));
        }
    }
    for g in &row.reactions {
        let by = g
            .users
            .iter()
            .map(|u| people.lookup_id(u.as_ref()))
            .collect();
        it.reactions.push((g.emoji.clone(), by));
    }

    let attachments: Vec<&kourier_model::Attachment> = match &row.body {
        Body::Media { attachments, .. } => attachments.iter().collect(),
        Body::Sticker(a) => vec![&**a],
        _ => Vec::new(),
    };

    if let Some(d) = &row.deleted {
        it.kind = "deleted";
        let (who, by) = match d {
            Deletion::ForEveryone { by } => {
                ("everyone", by.as_ref().map(|u| people.lookup_id(Some(u))))
            }
            Deletion::Unattributed => ("everyone", None),
            Deletion::ForMe => ("me", None),
        };
        let mut o = vec![("for", J::s(who))];
        if let Some(b) = by {
            o.push(("by", J::s(&b)));
        }
        it.special = Some(("deleted", J::Obj(o)));
        it.words = deleted_text(d, own, row.author.as_ref());
        it.edits.clear();
        it.edited_at = None;
        it.reactions.clear();
        it.reply = None;
        return it;
    }

    match &row.body {
        Body::Text(t) => it.text = Some(text_out(t, people)),
        Body::Media { caption, .. } => {
            it.kind = "media";
            it.text = caption
                .as_ref()
                .filter(|c| !c.body.trim().is_empty())
                .map(|c| text_out(c, people));
        }
        Body::Sticker(_) => it.kind = "sticker",
        Body::Contact(c) => {
            it.kind = "contact";
            let mut card = c.clone();
            card.name = card.name.as_deref().and_then(shown_name);
            card.organization = card.organization.as_deref().and_then(shown_name);
            let mut o = Vec::new();
            if let Some(n) = &card.name {
                o.push(("name", J::s(n)));
            }
            if let Some(n) = &card.organization {
                o.push(("organization", J::s(n)));
            }
            it.special = Some(("contact", J::Obj(o)));
            it.words = body_summary(&Body::Contact(card), &table).text;
        }
        Body::Location(l) => {
            it.kind = "location";
            let mut o = Vec::new();
            if let Some(n) = &l.name {
                o.push(("name", J::s(&single_line(n))));
            }
            if let Some(n) = &l.address {
                o.push(("address", J::s(&single_line(n))));
            }
            o.push(("lat", J::Raw(Value::from(f64::from(l.lat_e7) / 1e7))));
            o.push(("lon", J::Raw(Value::from(f64::from(l.lon_e7) / 1e7))));
            it.special = Some(("location", J::Obj(o)));
            it.words = body_summary(&row.body, &table).text;
        }
        Body::Poll(p) => {
            it.kind = "poll";
            it.special = Some((
                "poll",
                J::Obj(vec![
                    ("question", J::s(&p.question)),
                    (
                        "options",
                        J::Arr(p.options.iter().map(|o| J::s(o)).collect()),
                    ),
                    ("multiple", J::b(p.multiple)),
                ]),
            ));
            it.words = p.question.clone();
        }
        Body::System(SystemEvent::Call(line)) => {
            it.kind = "call";
            it.special = Some(("call", call_json(line, people)));
            it.words = system_text(&SystemEvent::Call(line.clone()), &table);
        }
        Body::System(e) => {
            it.kind = "notice";
            it.special = Some(("notice", notice_json(e, people)));
            it.words = system_text(e, &table);
        }
        Body::Placeholder(p) => {
            it.kind = "placeholder";
            let s = match p {
                Placeholder::Undecryptable { .. } => "undecryptable",
                Placeholder::ViewOnceOpened => "view_once_opened",
                Placeholder::Expired => "expired",
            };
            it.special = Some(("placeholder", J::Obj(vec![("state", J::s(s))])));
            it.words = placeholder_summary(*p).text;
        }
        Body::Unsupported { kind_hint, .. } => {
            it.kind = "unsupported";
            it.special = Some(("unsupported", J::Obj(vec![("kind", J::s(kind_hint))])));
            it.words = unsupported_label(kind_hint).text;
        }
    }

    for pf in &pm.files {
        if pf.accepted == Err(ExportFileRefusal::LongText) {
            continue;
        }
        let att = attachments.iter().find(|a| a.id == Some(pf.attachment));
        let goes = file_goes(pm, pf, ticks);
        let path = if goes {
            entries.get(&pf.attachment).cloned()
        } else {
            None
        };
        let (state, kind_left_out) = match (pf.accepted, &path) {
            (Err(ExportFileRefusal::NotHere), _) => ("not_on_this_computer", false),
            (Ok(()), Some(_)) => ("included", false),
            (Ok(()), None) => ("left_out", !ticks.has(class_of(&row.body, pf.kind))),
            (Err(_), _) => ("left_out", false),
        };
        let name = pf
            .file_name
            .as_deref()
            .map(clean_name)
            .filter(|n| !n.is_empty());
        it.files.push(FileOut {
            name,
            shown: path
                .as_deref()
                .map_or(Shown::Text, |p| shown_as(p, pf.sniffed.as_deref())),
            path,
            kind: kind_name(&row.body, pf.kind),
            mime: pf.mime.clone(),
            size: pf.size,
            dims: att.and_then(|a| a.dims),
            duration_ms: att.and_then(|a| a.duration_ms),
            caption: att
                .and_then(|a| a.caption.as_ref())
                .filter(|c| !c.body.trim().is_empty())
                .map(|c| text_out(c, people).plain),
            state,
            kind_left_out,
        });
    }
    let _ = FileClass::Other;
    it
}
