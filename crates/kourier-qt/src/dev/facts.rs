//! `chat_facts`: what the core's OWN row of one chat says, as marks (T2, T-1, decision M12).
//!
//! The shown list is the inbox, so an archived chat is not in `chats` at all (and a pinned chat
//! is never shown archived): the run tool's `settings` step cannot read the archive from the
//! view. This is the debug-only query of the core's rows it reads instead. It runs OFF the Qt
//! thread (the async side of the channel, like `wait`): the facade query is awaited in
//! [`super::run`], and this module only turns the answer into plain values, so the Qt thread
//! never blocks and never touches a database. Ids, booleans, counts, times and state names only:
//! no title, no member, no text.

use kourier_app::facade::{ConversationDetail, MessageRow};
use kourier_model::{CallDirection, CallState, ConvKind, MemberRole};
use serde_json::{Value, json};

/// How many of the newest timeline rows the call lines are read from. A test chat holds a few
/// calls; a page this size cannot miss one the step has just made.
pub(crate) const CALL_ROWS: u32 = 200;

/// The name of a call line's state in the dev channel.
pub(crate) fn call_state_name(state: CallState) -> &'static str {
    match state {
        CallState::Started => "started",
        CallState::Ringing { .. } => "ringing",
        CallState::Missed => "missed",
        CallState::Unanswered => "unanswered",
        CallState::Declined => "declined",
        CallState::Answered => "answered",
    }
}

/// The name of a call line's direction in the dev channel.
pub(crate) fn call_direction_name(direction: CallDirection) -> &'static str {
    match direction {
        CallDirection::Incoming => "incoming",
        CallDirection::Outgoing => "outgoing",
    }
}

/// Whether a chat with these two mute ends is muted at `now_ms`: either end in the future. The
/// phone's "for ever" is `i64::MAX` in the core. The very rule of the shown list's `muted`
/// (`kourier_viewmodel::conversations::is_muted`), restated for the core's raw row.
pub(crate) fn muted_at(
    muted_until: Option<i64>,
    local_muted_until: Option<i64>,
    now_ms: i64,
) -> bool {
    [muted_until, local_muted_until]
        .into_iter()
        .flatten()
        .any(|until| until > now_ms)
}

/// The call lines of `rows` (a timeline page, oldest first as the core gives it), newest
/// first: `{state, direction, group}` each. Rows that are no call line are skipped.
pub(crate) fn call_lines(rows: &[MessageRow]) -> Vec<Value> {
    rows.iter()
        .rev()
        .filter_map(|row| row.body.call_line())
        .map(|line| {
            json!({
                "state": call_state_name(line.state),
                "direction": call_direction_name(line.direction),
                "group": line.group,
            })
        })
        .collect()
}

/// The answer of `chat_facts` for one chat: the core's flags, `last_activity` (ms: when the newest
/// thing, a message or a call line, happened; how a driver sees that something arrived in a chat
/// the shown list hides), the call lines of its newest page and, for a group, its member counts
/// (`members` = joined, `admins` of them, `pending` = invited or asking).
pub(crate) fn chat_facts(detail: &ConversationDetail, rows: &[MessageRow], now_ms: i64) -> Value {
    let row = &detail.row;
    let kind = match row.kind {
        ConvKind::Direct => "direct",
        ConvKind::Group => "group",
        ConvKind::Broadcast | ConvKind::Channel => "other",
    };
    let mut facts = json!({
        "conversation": row.id.0,
        "kind": kind,
        "archived": row.archived,
        "pinned": row.pinned_order.is_some(),
        "muted": muted_at(row.muted_until, row.local_muted_until, now_ms),
        "marked_unread": row.marked_unread,
        "last_activity": row.last_activity,
        "calls": call_lines(rows),
    });
    if row.kind == ConvKind::Group {
        let count = |wanted: fn(MemberRole) -> bool| {
            detail.members.iter().filter(|m| wanted(m.role)).count()
        };
        facts["members"] = json!(count(|r| matches!(
            r,
            MemberRole::Member | MemberRole::Admin
        )));
        facts["admins"] = json!(count(|r| r == MemberRole::Admin));
        facts["pending"] = json!(count(|r| matches!(
            r,
            MemberRole::Pending | MemberRole::Requesting
        )));
    }
    facts
}

#[cfg(test)]
mod tests {
    use super::*;
    use kourier_app::facade::{ConversationRow, OriginKind, Roaming};
    use kourier_model::{
        AccountId, Body, CallLine, ConversationFeatures, ConversationId, Member, MessageId,
        SystemEvent, UserId,
    };

    fn user(n: u8) -> UserId {
        UserId::from_parts("pn", &format!("99900048{n:04}")).expect("id")
    }

    fn row(kind: ConvKind) -> ConversationRow {
        ConversationRow {
            id: ConversationId(7),
            account: AccountId(1),
            kind,
            is_self: false,
            title: None,
            peer: None,
            avatar: None,
            preview: None,
            last_activity: 0,
            unread: 0,
            mentions: 0,
            marked_unread: false,
            muted_until: None,
            local_muted_until: None,
            archived: false,
            pinned_order: None,
            blocked: false,
            request_pending: false,
            features_hash: 0,
            has_draft: false,
            hidden: false,
            can_send: true,
            roaming: Roaming::Hidden,
            protected: false,
        }
    }

    fn detail(row: ConversationRow, members: Vec<Member>) -> ConversationDetail {
        ConversationDetail {
            row,
            members,
            features: ConversationFeatures::default(),
            timer: None,
            draft: None,
        }
    }

    fn member(n: u8, role: MemberRole) -> Member {
        Member {
            user: user(n),
            role,
        }
    }

    fn call(state: CallState, direction: CallDirection, group: bool) -> MessageRow {
        let line = CallLine {
            direction,
            group,
            state,
            media: None,
            media_at: 0,
            by: None,
            by_at: 0,
        };
        let mut message = text_row();
        message.body = Body::System(SystemEvent::Call(line));
        message
    }

    fn text_row() -> MessageRow {
        MessageRow {
            id: MessageId(1),
            sort_key: 0,
            author: Some(user(9)),
            author_name: None,
            author_avatar: None,
            origin: OriginKind::Remote,
            device: None,
            body: Body::text("x"),
            reply: None,
            sent_at: 0,
            server_at: None,
            received_at: 0,
            revision: 0,
            edited_at: None,
            deleted: None,
            out_state: None,
            read_state: None,
            reactions: Vec::new(),
            expiry: None,
            view_once: false,
            history: false,
            mentions_me: false,
            outbox: None,
            actions: Default::default(),
        }
    }

    /// The flags come from the CORE's row, each from its own field: an archived chat says so
    /// although the shown list holds none, a pinned chat is pinned whatever its archive flag
    /// says, and the mute is read from either end (the phone's or this computer's) against
    /// `now`, "for ever" included.
    #[test]
    fn the_flags_are_the_cores_rows_own() {
        let mut r = row(ConvKind::Direct);
        r.archived = true;
        r.marked_unread = true;
        r.last_activity = 1_761_900_000_123;
        let facts = chat_facts(&detail(r, Vec::new()), &[], 1_000);
        assert_eq!(facts["conversation"], json!(7));
        assert_eq!(facts["last_activity"], json!(1_761_900_000_123_i64));
        assert_eq!(facts["kind"], json!("direct"));
        assert_eq!(facts["archived"], json!(true));
        assert_eq!(facts["marked_unread"], json!(true));
        assert_eq!(facts["pinned"], json!(false));
        assert_eq!(facts["muted"], json!(false));
        assert_eq!(facts["calls"], json!([]));
        assert!(facts.get("members").is_none(), "{facts}");

        let mut r = row(ConvKind::Direct);
        r.pinned_order = Some(0);
        r.archived = true;
        let facts = chat_facts(&detail(r, Vec::new()), &[], 1_000);
        assert_eq!(facts["pinned"], json!(true));
        assert_eq!(
            facts["archived"],
            json!(true),
            "the core's flag, not the shown rule"
        );

        assert!(!muted_at(None, None, 1_000));
        assert!(!muted_at(Some(0), None, 1_000));
        assert!(!muted_at(Some(1_000), Some(999), 1_000), "ends at `now`");
        assert!(muted_at(Some(1_001), None, 1_000));
        assert!(muted_at(None, Some(1_001), 1_000), "this computer's mute");
        assert!(muted_at(Some(i64::MAX), None, 1_000), "for ever");
    }

    /// A group states its member counts: joined members, the admins among them, and those
    /// only invited or asking, apart; a direct chat states none.
    #[test]
    fn a_group_counts_its_members_and_admins() {
        let members = vec![
            member(1, MemberRole::Admin),
            member(2, MemberRole::Admin),
            member(3, MemberRole::Member),
            member(4, MemberRole::Pending),
            member(5, MemberRole::Requesting),
        ];
        let facts = chat_facts(&detail(row(ConvKind::Group), members), &[], 0);
        assert_eq!(facts["kind"], json!("group"));
        assert_eq!(facts["members"], json!(3));
        assert_eq!(facts["admins"], json!(2));
        assert_eq!(facts["pending"], json!(2));
    }

    /// The call lines come newest first (the page is oldest first), with their state and
    /// direction; a row that is no call line is skipped.
    #[test]
    fn the_call_lines_come_newest_first_with_their_state_and_direction() {
        let rows = vec![
            call(CallState::Missed, CallDirection::Incoming, false),
            text_row(),
            call(CallState::Answered, CallDirection::Outgoing, false),
            call(
                CallState::Ringing { deadline: 5 },
                CallDirection::Incoming,
                true,
            ),
        ];
        let lines = call_lines(&rows);
        assert_eq!(
            lines,
            vec![
                json!({"state": "ringing", "direction": "incoming", "group": true}),
                json!({"state": "answered", "direction": "outgoing", "group": false}),
                json!({"state": "missed", "direction": "incoming", "group": false}),
            ]
        );
        let facts = chat_facts(&detail(row(ConvKind::Direct), Vec::new()), &rows, 0);
        assert_eq!(facts["calls"], json!(lines));
    }

    /// Every state and both directions have their own name.
    #[test]
    fn every_call_state_has_its_own_name() {
        let names = [
            call_state_name(CallState::Started),
            call_state_name(CallState::Ringing { deadline: 1 }),
            call_state_name(CallState::Missed),
            call_state_name(CallState::Unanswered),
            call_state_name(CallState::Declined),
            call_state_name(CallState::Answered),
        ];
        let mut sorted = names.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "{names:?}");
        assert_ne!(
            call_direction_name(CallDirection::Incoming),
            call_direction_name(CallDirection::Outgoing)
        );
    }
}
