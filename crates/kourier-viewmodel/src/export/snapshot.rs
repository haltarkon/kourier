//! Owned, `Send` copies of what the documents need from the Qt thread: the display names of the
//! people a plan names, and the UTC offsets at the instants it uses.

use std::collections::HashMap;

use kourier_core::facade::{ExportPlan, OriginKind, PlannedMessage};
use kourier_model::{Body, Deletion, RangeKind, SystemEvent, Text, UserId};

use crate::names::{Names, fallback_name};
use crate::time::Calendar;

/// The display names of the users of a plan, copied from a [`Names`] on the caller's thread.
#[derive(Clone, Default)]
pub struct NameSnapshot {
    names: HashMap<UserId, String>,
}

impl std::fmt::Debug for NameSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NameSnapshot")
            .field("len", &self.names.len())
            .finish()
    }
}

impl NameSnapshot {
    /// An empty snapshot (every user gets the fallback name).
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the name of `user`.
    pub fn insert(&mut self, user: UserId, name: impl Into<String>) {
        self.names.insert(user, name.into());
    }

    /// Adds the name `names` shows for `user` (a person the plan does not name, like the other
    /// side of a direct chat), unless one is there.
    pub fn add(&mut self, user: &UserId, names: &dyn Names) {
        self.names
            .entry(user.clone())
            .or_insert_with(|| names.display_name(user));
    }

    /// Every user the plan names (authors, reactors, quoted authors, mentions, notice and call
    /// actors), with the name `names` shows for each.
    pub fn from_plan(plan: &ExportPlan, names: &dyn Names) -> Self {
        let mut out = Self::new();
        for pm in &plan.messages {
            for user in users_of(pm) {
                out.names
                    .entry(user)
                    .or_insert_with_key(|u| names.display_name(u));
            }
        }
        out
    }
}

impl Names for NameSnapshot {
    fn display_name(&self, user: &UserId) -> String {
        self.names
            .get(user)
            .cloned()
            .unwrap_or_else(|| fallback_name(user))
    }
}

/// UTC offsets at the instants a plan uses, copied from a [`Calendar`] on the caller's thread
/// (a Qt time zone is not `Send`). An instant that was not copied gets `default_s`.
#[derive(Clone, Debug, Default)]
pub struct OffsetTable {
    offsets: HashMap<i64, i32>,
    default_s: i32,
}

impl OffsetTable {
    /// A table that answers `offset_s` everywhere (tests, UTC).
    pub fn fixed(offset_s: i32) -> Self {
        Self {
            offsets: HashMap::new(),
            default_s: offset_s,
        }
    }

    /// The offsets at every instant of the plan: when each message was sent, edited, and the
    /// plan's own clock.
    pub fn from_plan(plan: &ExportPlan, calendar: &dyn Calendar) -> Self {
        let mut offsets = HashMap::new();
        let mut put = |ts: i64| {
            offsets
                .entry(ts)
                .or_insert_with(|| calendar.utc_offset_s(ts));
        };
        put(plan.now);
        for pm in &plan.messages {
            put(pm.row.sent_at);
            if let Some(e) = pm.row.edited_at {
                put(e);
            }
            for r in &pm.revisions {
                put(r.at);
            }
        }
        Self {
            offsets,
            default_s: calendar.utc_offset_s(plan.now),
        }
    }
}

impl Calendar for OffsetTable {
    fn utc_offset_s(&self, ts_ms: i64) -> i32 {
        self.offsets.get(&ts_ms).copied().unwrap_or(self.default_s)
    }
}

fn text_users(text: &Text, out: &mut Vec<UserId>) {
    for r in text.shown_mentions() {
        if let RangeKind::Mention(u) = &r.kind {
            out.push(u.clone());
        }
    }
}

/// The people of one message other than us, in the order of first appearance: the author, the
/// quoted author, the mentioned, the reactors, the actors of a notice or call, the one who
/// deleted it.
pub(crate) fn users_of(pm: &PlannedMessage) -> Vec<UserId> {
    let row = &pm.row;
    let mut out = Vec::new();
    let notice = matches!(row.body, Body::System(_));
    if !notice && let Some(a) = &row.author {
        out.push(a.clone());
    }
    if let Some(r) = &row.reply {
        if !r.own
            && let Some(a) = &r.author
        {
            out.push(a.clone());
        }
        if let Some(t) = &r.text {
            text_users(t, &mut out);
        }
    }
    match &row.body {
        Body::Text(t) => text_users(t, &mut out),
        Body::Media {
            attachments,
            caption,
        } => {
            if let Some(c) = caption {
                text_users(c, &mut out);
            }
            for a in attachments {
                if let Some(c) = &a.caption {
                    text_users(c, &mut out);
                }
            }
        }
        _ => {}
    }
    for rev in &pm.revisions {
        if let Some(t) = &rev.text {
            text_users(t, &mut out);
        }
    }
    for g in &row.reactions {
        out.extend(g.users.iter().flatten().cloned());
    }
    if let Body::System(e) = &row.body {
        out.extend(event_users(e));
    }
    if let Some(Deletion::ForEveryone { by: Some(u) }) = &row.deleted {
        out.push(u.clone());
    }
    out
}

/// Whether the row is our own message (`author: None` of a message that is not a notice).
pub(crate) fn is_own(pm: &PlannedMessage) -> bool {
    !matches!(pm.row.body, Body::System(_))
        && pm.row.author.is_none()
        && pm.row.origin != OriginKind::Remote
}

/// The people a notice or a call line names.
pub(crate) fn event_users(e: &SystemEvent) -> Vec<UserId> {
    match e {
        SystemEvent::MembersAdded { by, users } | SystemEvent::MembersRemoved { by, users } => {
            by.iter().chain(users.iter()).cloned().collect()
        }
        SystemEvent::MemberLeft { user }
        | SystemEvent::IdentityKeyChanged { user }
        | SystemEvent::SessionRefreshed { user }
        | SystemEvent::NumberChanged { user }
        | SystemEvent::VerifiedChanged { user, .. } => vec![user.clone()],
        SystemEvent::TitleChanged { by, .. }
        | SystemEvent::AvatarChanged { by }
        | SystemEvent::DescriptionChanged { by }
        | SystemEvent::TimerChanged { by, .. }
        | SystemEvent::GroupCreated { by } => by.iter().cloned().collect(),
        SystemEvent::Call(line) => line.by.iter().cloned().collect(),
        SystemEvent::MessageRequestAccepted | SystemEvent::Joined => Vec::new(),
    }
}
