//! W6a2 B1, THE GROUPS WIRE PROBE (verdict in `target/wa/w6/scout-groups.md`): what upstream gives Kourier at the pin
//! for the group half of W6a2, over the scripted server, BEFORE W6a2 maps any of it. A real client, built by Kourier's
//! own `build_bot` over the account's `KourierStore`, talks to the scripted Wire; the test is handed the client
//! (`wire::hand_over`) and calls upstream's group and blocklist reads itself; a raw handler of the TEST, registered
//! right after Kourier's (`wire::parts_observing`), records what the bus hands every raw handler for a `w:gp2`
//! notification (kinds, indices, flags, the synthetic times; every JID as a role word, never the JID).
//!
//! The paths, one connection, in order:
//! 0. the FIRST BURST: which IQs the client sends between `<success>` and `Online` + 2 s (the group ones are Kourier's fetch
//!    policy's start pair since B6, the blocklist ones are upstream's own and Kourier's read);
//! 1. `list_participating` (the overview projection);
//! 2. `fetch_metadata_batch` of six groups the server answers five ways (whole with every setting, whole with none,
//!    truncated, forbidden, not found; the sixth is not answered), then `resolve_participant_addresses`;
//! 3. a batch the server answers with an undocumented refusal code, a batch answered 503, an empty batch;
//! 4. `fetch_metadata` (the single query) of the truncated group, and of groups answered 403, 404 and 429;
//! 5. `get_blocklist` (a number, a LID, an entry with no time, an entry with no JID);
//! 6. a `w:gp2` notification of a LID group with fifteen actions (promote with and without `type`, demote, an older
//!    rename, a rename without a time, a description and its deletion, the four setting toggles, the timer and its
//!    end, the add mode, an unknown tag); when the client acks it;
//! 7. a `w:gp2` of a number-addressed group with an add and a remove;
//! 8. the server's `<groups_dirty>`.
//!
//! The facts are printed as `probe:` lines (kinds, counts, flags, the synthetic times; never a JID, a number or a
//! name), and each path's expectation is checked; the run ends with every mismatch listed. The body runs in ONE
//! offline child under `unshare -rn`; nothing here touches a network.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kourier_backend_api::{Command, Exit, StartMode};
use kourier_model::ConnState;
use kourier_whatsapp::StoreOwner;
use wacore::stanza::groups::{GroupNotificationAction as Action, GroupParticipantType};
use wacore::store::traits::{LidPnMappingEntry, ProtocolStore};
use wacore::types::events::{Event, EventHandler, GroupUpdate};
use wacore::types::message::AddressingMode;
use wacore_binary::{Jid, Node};
use whatsapp_rust::{
    GroupError, GroupHierarchy, GroupLookupResult, GroupMetadata, IqError, MemberAddMode,
    ParticipantType, SubgroupKind,
};

use crate::actor_real::{record, wait_for_states};
use crate::receive::{account_dir, count, raw};
use crate::support::capture::Capture;
use crate::support::wire::{self, Answer, BatchEntry, Gp2, GroupSpec, Member, Topic};
use crate::support::{self, db_path, key, peer};
use crate::wire::start_actor_observing;

// ---- the fixture's synthetic values (each a substring of no other) ----------------------------------

/// A LID group with every setting, a subgroup of [`G_PARENT`].
const G_FULL: &str = "120363000081726354";
/// A number-addressed community parent with no setting.
const G_PLAIN: &str = "120363000092837465";
/// A group the batch answers truncated (the single query answers it whole).
const G_TRUNC: &str = "120363000013579246";
/// A group the server refuses (403).
const G_FORBID: &str = "120363000024681357";
/// A group the server does not know (404).
const G_GONE: &str = "120363000035792468";
/// A group the batch asks and the answer leaves out (the single query: 429).
const G_OMIT: &str = "120363000046813579";
/// The community of [`G_FULL`].
const G_PARENT: &str = "120363000057924681";
/// A group the batch answers with an undocumented refusal code.
const G_BADCODE: &str = "120363000068135792";

const SUPER_LID: &str = "481516234200017";
const SUPER_PN: &str = "15559081726";
const ADMIN_LID: &str = "662607015000034";
const ADMIN_PN: &str = "15558123647";
/// A member the server lists by LID only; the store knows its number (a `usync` row put before the start).
const MEMBER_LID: &str = "299792458000051";
const MEMBER_PN: &str = "15557364918";
/// Added and removed by the notice of the number-addressed group.
const NEW_PN: &str = "15556284017";
/// The admin of the number-addressed group.
const PN_ADMIN: &str = "15555190372";
const BLOCKED_PN: &str = "15554729163";
const BLOCKED_LID: &str = "577215664901532";
const UNTIMED_PN: &str = "15553618294";

const SUBJECT: &str = "Quokka fixture subject";
const OLD_SUBJECT: &str = "Numbat fixture subject";
const TOPIC: &str = "Wombat fixture topic";
const NOTIFY: &str = "Bilby fixture notify";

/// Seconds, all distinct, so a field read from the wrong place shows.
const SUBJECT_T: u64 = 1_760_100_000;
const OLDER_SUBJECT_T: u64 = 1_760_000_000;
const CREATION_T: u64 = 1_759_000_000;
const TOPIC_T: u64 = 1_760_050_000;
const NOTICE_T: u64 = 1_760_200_000;
const BLOCK_T: u64 = 1_760_300_000;
const WEEK_S: u32 = 604_800;
const DAY_S: u32 = 86_400;

fn lid(user: &str) -> String {
    format!("{user}@lid")
}

fn pn(user: &str) -> String {
    format!("{user}@s.whatsapp.net")
}

fn group(user: &str) -> Jid {
    format!("{user}@g.us").parse().expect("a group JID")
}

/// Who a user part is, without keeping it.
fn who_user(user: &str) -> &'static str {
    match user {
        SUPER_LID => "super",
        SUPER_PN => "super-pn",
        ADMIN_LID => "admin",
        ADMIN_PN => "admin-pn",
        MEMBER_LID => "member",
        MEMBER_PN => "member-pn",
        NEW_PN => "new",
        PN_ADMIN => "pnadmin",
        BLOCKED_PN => "blocked-pn",
        BLOCKED_LID => "blocked-lid",
        UNTIMED_PN => "untimed-pn",
        G_FULL => "g-full",
        G_PLAIN => "g-plain",
        G_TRUNC => "g-trunc",
        G_FORBID => "g-forbid",
        G_GONE => "g-gone",
        G_OMIT => "g-omit",
        G_PARENT => "g-parent",
        G_BADCODE => "g-badcode",
        u if u == peer::ACCOUNT_LID => "us",
        u if u == peer::ACCOUNT_PN => "us-pn",
        _ => "other",
    }
}

fn who(jid: &Jid) -> &'static str {
    who_user(jid.user.as_str())
}

fn who_opt(jid: Option<&Jid>) -> &'static str {
    jid.map_or("none", who)
}

// ---- the server's groups ----------------------------------------------------------------------------

fn full_group() -> GroupSpec {
    GroupSpec {
        user: G_FULL.into(),
        subject: Some(SUBJECT.into()),
        subject_time: Some(SUBJECT_T),
        subject_owner: Some(lid(SUPER_LID)),
        creation: Some(CREATION_T),
        creator: Some(lid(SUPER_LID)),
        addressing_mode: "lid",
        size: Some(4),
        participants: vec![
            Member {
                jid: lid(SUPER_LID),
                phone_number: Some(pn(SUPER_PN)),
                kind: Some("superadmin"),
            },
            Member {
                jid: lid(ADMIN_LID),
                phone_number: Some(pn(ADMIN_PN)),
                kind: Some("admin"),
            },
            Member {
                jid: lid(MEMBER_LID),
                phone_number: None,
                kind: None,
            },
            Member {
                jid: lid(peer::ACCOUNT_LID),
                phone_number: None,
                kind: None,
            },
        ],
        description: Some(Topic {
            id: "probe-topic-1".into(),
            body: TOPIC.into(),
            participant: Some(lid(ADMIN_LID)),
            t: TOPIC_T,
        }),
        locked: true,
        announcement: true,
        ephemeral: Some((WEEK_S, Some(1))),
        member_add_mode: Some("admin_add"),
        parent: false,
        linked_parent: Some(G_PARENT.into()),
        default_sub_group: false,
    }
}

fn plain_group() -> GroupSpec {
    GroupSpec {
        user: G_PLAIN.into(),
        subject: Some(OLD_SUBJECT.into()),
        creation: Some(CREATION_T),
        addressing_mode: "pn",
        participants: vec![
            Member {
                jid: pn(PN_ADMIN),
                phone_number: None,
                kind: Some("admin"),
            },
            Member {
                jid: pn(peer::ACCOUNT_PN),
                phone_number: None,
                kind: None,
            },
        ],
        member_add_mode: Some("all_member_add"),
        parent: true,
        ..GroupSpec::default()
    }
}

/// The truncated group as the single query answers it.
fn trunc_group() -> GroupSpec {
    GroupSpec {
        user: G_TRUNC.into(),
        subject: Some(SUBJECT.into()),
        addressing_mode: "pn",
        size: Some(1500),
        participants: vec![
            Member {
                jid: pn(PN_ADMIN),
                phone_number: None,
                kind: Some("superadmin"),
            },
            Member {
                jid: pn(NEW_PN),
                phone_number: None,
                kind: None,
            },
            Member {
                jid: pn(peer::ACCOUNT_PN),
                phone_number: None,
                kind: None,
            },
        ],
        ..GroupSpec::default()
    }
}

fn the_batch() -> Vec<BatchEntry> {
    vec![
        BatchEntry::Found(Box::new(full_group())),
        BatchEntry::Found(Box::new(plain_group())),
        BatchEntry::Truncated {
            user: G_TRUNC.into(),
            size: 1500,
        },
        BatchEntry::Refused {
            user: G_FORBID.into(),
            code: 403,
        },
        BatchEntry::Refused {
            user: G_GONE.into(),
            code: 404,
        },
    ]
}

/// What the batch rule answers now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Batch {
    /// [`the_batch`].
    Whole,
    /// A whole group and a refusal with code 500 in one answer.
    BadCode,
    /// `<iq type="error"><error code="503"/>`.
    Refused,
}

fn batch_rule(phase: Arc<Mutex<Batch>>) -> impl Fn(&Node) -> Option<Answer> + Send + Sync {
    let whole = wire::group_batch(the_batch());
    move |request| {
        if !wire::is_group_batch(request) {
            return None;
        }
        match *phase.lock().expect("lock") {
            Batch::Whole => whole(request),
            Batch::BadCode => Some(wire::batch_answer(&[
                BatchEntry::Found(Box::new(full_group())),
                BatchEntry::Refused {
                    user: G_BADCODE.into(),
                    code: 500,
                },
            ])),
            Batch::Refused => Some(Answer::Error(503)),
        }
    }
}

/// The single group query (`<query request="interactive"/>` to the group): the truncated group whole, refusals
/// for three others.
fn single_rule(request: &Node) -> Option<Answer> {
    request.get_optional_child("query")?;
    if wire::is_group_batch(request) {
        return None;
    }
    let to = attr(request, "to")?;
    let user = to.split('@').next().unwrap_or_default();
    match user {
        G_TRUNC => Some(Answer::Result(vec![trunc_group().node(true)])),
        G_FORBID => Some(Answer::Error(403)),
        G_GONE => Some(Answer::Error(404)),
        G_OMIT => Some(Answer::Error(429)),
        _ => None,
    }
}

fn blocked() -> Vec<(Option<String>, Option<u64>)> {
    vec![
        (Some(pn(BLOCKED_PN)), Some(BLOCK_T)),
        (Some(lid(BLOCKED_LID)), Some(BLOCK_T + 1)),
        (Some(pn(UNTIMED_PN)), None),
        (None, Some(BLOCK_T + 2)),
    ]
}

// ---- what the bus hands the raw handlers --------------------------------------------------------------

/// One `w:gp2` event as the probe keeps it: kinds, flags, synthetic times; no JID, no name.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    Update {
        notification: String,
        index: u32,
        kind: String,
        actor: &'static str,
        actor_pn: &'static str,
        notify: bool,
        offline: bool,
        lid: bool,
        incomplete: bool,
        at_ms: i64,
        detail: String,
        /// Whether the client had acked the notification when this event was handed.
        acked: bool,
    },
    Raw {
        notification: String,
        first_child: String,
        dirty: Vec<&'static str>,
        acked: bool,
    },
}

fn tier(kind: Option<GroupParticipantType>) -> &'static str {
    match kind {
        Some(GroupParticipantType::SuperAdmin) => "superadmin",
        Some(GroupParticipantType::Admin) => "admin",
        Some(GroupParticipantType::Participant) => "participant",
        None => "none",
    }
}

fn subject_word(subject: &str) -> &'static str {
    match subject {
        SUBJECT => "subject",
        OLD_SUBJECT => "old-subject",
        "" => "empty",
        _ => "other",
    }
}

fn people(participants: &[wacore::stanza::groups::GroupParticipantInfo]) -> String {
    participants
        .iter()
        .map(|p| {
            format!(
                "{}/{}/pn={}",
                who(&p.jid),
                tier(p.r#type),
                who_opt(p.phone_number.as_ref())
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The action's kind and what it carries, as synthetic words.
fn action_of(action: &Action) -> (&'static str, String) {
    match action {
        Action::Add { participants, .. } => ("add", people(participants)),
        Action::Remove { participants, .. } => ("remove", people(participants)),
        Action::Promote { participants } => ("promote", people(participants)),
        Action::Demote { participants } => ("demote", people(participants)),
        Action::Subject {
            subject,
            subject_owner,
            subject_time,
            ..
        } => (
            "subject",
            format!(
                "{} owner={} s_t={subject_time:?}",
                subject_word(subject),
                who_opt(subject_owner.as_ref())
            ),
        ),
        Action::Description { id, description } => (
            "description",
            format!(
                "id={id} body={}",
                match description.as_deref() {
                    Some(TOPIC) => "topic",
                    Some(_) => "other",
                    None => "deleted",
                }
            ),
        ),
        Action::Locked { threshold } => ("locked", format!("threshold={threshold:?}")),
        Action::Unlocked => ("unlocked", String::new()),
        Action::Announce => ("announce", String::new()),
        Action::NotAnnounce => ("not_announce", String::new()),
        Action::Ephemeral {
            expiration,
            trigger,
        } => (
            "ephemeral",
            format!("expiration={expiration} trigger={trigger:?}"),
        ),
        Action::MemberAddMode { mode } => ("member_add_mode", format!("mode={mode}")),
        Action::Unknown { tag } => ("unknown", format!("tag={tag}")),
        _ => ("other", String::new()),
    }
}

fn update_seen(u: &GroupUpdate, acked: bool) -> Seen {
    let (kind, detail) = action_of(&u.action);
    Seen::Update {
        notification: u.notification_id.clone().unwrap_or_default(),
        index: u.action_index,
        kind: kind.to_owned(),
        actor: who_opt(u.participant.as_ref()),
        actor_pn: who_opt(u.participant_pn.as_ref()),
        notify: u.notify.as_deref() == Some(NOTIFY),
        offline: u.offline.is_some(),
        lid: u.is_lid_addressing_mode,
        incomplete: u.has_incomplete_participant_information,
        at_ms: u.timestamp.timestamp_millis(),
        detail,
        acked,
    }
}

/// The test's raw handler: records the `w:gp2` events; at the stanza's raw `Notification` (the last event of a
/// stanza) it waits 150 ms, so an ack the client had sent before the dispatch would have reached the server.
struct Observer {
    seen: Mutex<Vec<Seen>>,
    server: wire::Wire,
}

impl Observer {
    fn acked(&self, id: &str) -> bool {
        self.server
            .frames()
            .iter()
            .any(|f| f.tag == "ack" && f.attr("id") == Some(id))
    }

    fn take(&self) -> Vec<Seen> {
        std::mem::take(&mut *self.seen.lock().expect("lock"))
    }

    /// Waits (at most 15 s) for a raw event of notification `id`, then 300 ms for stragglers; takes all.
    async fn stanza(&self, id: &str) -> Vec<Seen> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let done = self.seen.lock().expect("lock").iter().any(
                |s| matches!(s, Seen::Raw { notification, .. } if notification.as_str() == id),
            );
            if done || Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        self.take()
    }
}

impl EventHandler for Observer {
    fn handle_event(&self, event: Arc<Event>) {
        let seen = match &*event {
            Event::GroupUpdate(u) => {
                let id = u.notification_id.clone().unwrap_or_default();
                update_seen(u, self.acked(&id))
            }
            Event::Notification(node) => {
                let nr = node.get();
                let is_gp2 = nr
                    .attrs()
                    .optional_string("type")
                    .is_some_and(|t| t.as_ref() == "w:gp2");
                if !is_gp2 {
                    return;
                }
                let id = nr
                    .attrs()
                    .optional_string("id")
                    .map(|v| v.into_owned())
                    .unwrap_or_default();
                std::thread::sleep(Duration::from_millis(150));
                Seen::Raw {
                    first_child: nr
                        .children()
                        .and_then(|c| c.first())
                        .map(|f| f.tag.to_string())
                        .unwrap_or_default(),
                    dirty: wacore::stanza::groups::parse_groups_dirty(nr)
                        .unwrap_or_default()
                        .iter()
                        .map(who)
                        .collect(),
                    acked: self.acked(&id),
                    notification: id,
                }
            }
            _ => return,
        };
        self.seen.lock().expect("lock").push(seen);
    }
}

// ---- reading the results, with words ------------------------------------------------------------------

fn attr(node: &Node, key: &str) -> Option<String> {
    node.attrs.get(key).map(|v| v.as_str().into_owned())
}

/// A group error by its kind (L5: never formatted; a server error by its code).
fn group_error(e: &GroupError) -> String {
    match e {
        GroupError::Iq(IqError::ServerError { code, backoff, .. }) => {
            format!("server-error {code} backoff={backoff:?}")
        }
        GroupError::Iq(IqError::ParseError(_)) => "parse-error".into(),
        GroupError::Iq(IqError::Timeout) => "timeout".into(),
        GroupError::Iq(_) => "iq-other".into(),
        GroupError::InvalidRequest(_) => "invalid-request".into(),
        _ => "other".into(),
    }
}

fn hierarchy(h: &GroupHierarchy) -> String {
    match h {
        GroupHierarchy::Standalone => "standalone".into(),
        GroupHierarchy::Community => "community".into(),
        GroupHierarchy::Subgroup { parent, kind } => format!(
            "subgroup of {} ({})",
            who(parent),
            match kind {
                SubgroupKind::Regular => "regular",
                SubgroupKind::Announcement => "announcement",
                SubgroupKind::General => "general",
                _ => "other",
            }
        ),
        _ => "other".into(),
    }
}

fn role(t: ParticipantType) -> &'static str {
    match t {
        ParticipantType::Member => "member",
        ParticipantType::Admin => "admin",
        ParticipantType::SuperAdmin => "superadmin",
    }
}

/// A found group as words: the fields W6a2 maps (B4), never a JID or a name.
fn metadata(m: &GroupMetadata) -> Vec<String> {
    let members: Vec<String> = m
        .participants
        .iter()
        .map(|p| {
            format!(
                "{}/{}/pn={}/lid={}",
                who(&p.jid),
                role(p.participant_type),
                who_opt(p.phone_number.as_ref()),
                who_opt(p.lid.as_ref())
            )
        })
        .collect();
    vec![
        format!("id={}", who(&m.id)),
        format!(
            "subject={}",
            m.subject.as_deref().map_or("none", subject_word)
        ),
        format!("subject_time={:?}", m.subject_time),
        format!("subject_owner={}", who_opt(m.subject_owner.as_ref())),
        format!("creation={:?}", m.creation_time),
        format!("creator={}", who_opt(m.creator.as_ref())),
        format!(
            "addressing={}",
            match m.addressing_mode {
                AddressingMode::Lid => "lid",
                AddressingMode::Pn => "pn",
            }
        ),
        format!("members=[{}]", members.join(",")),
        format!("participant_count={:?}", m.participant_count),
        format!(
            "description={}",
            match m.description.as_deref() {
                Some(TOPIC) => "topic",
                Some(_) => "other",
                None => "none",
            }
        ),
        format!("description_id={:?}", m.description_id),
        format!(
            "description_owner={}",
            who_opt(m.description_owner.as_ref())
        ),
        format!("description_time={:?}", m.description_time),
        format!("locked={}", m.is_locked),
        format!("announcement={}", m.is_announcement),
        format!(
            "ephemeral={:?}",
            m.ephemeral.map(|e| (e.expiration, e.trigger))
        ),
        format!(
            "member_add_mode={}",
            match m.member_add_mode {
                Some(MemberAddMode::AdminAdd) => "admin_add",
                Some(MemberAddMode::AllMemberAdd) => "all_member_add",
                None => "none",
            }
        ),
        format!("hierarchy={}", hierarchy(&m.hierarchy())),
        format!("suspended={}", m.is_suspended),
        format!(
            "missing_identification={}",
            m.has_missing_participant_identification
        ),
    ]
}

fn result_words(r: &GroupLookupResult<Box<GroupMetadata>>) -> String {
    match r {
        GroupLookupResult::Found(m) => format!("found {}", who(&m.id)),
        GroupLookupResult::Truncated {
            id,
            participant_count,
        } => format!("truncated {} count={participant_count}", who(id)),
        GroupLookupResult::Forbidden(id) => format!("forbidden {}", who(id)),
        GroupLookupResult::NotFound(id) => format!("not-found {}", who(id)),
        _ => "other".into(),
    }
}

/// How many `get`/`set` IQs of namespace `xmlns` the client sent so far.
fn iqs(server: &wire::Wire, xmlns: &str) -> usize {
    server
        .frames()
        .iter()
        .filter(|f| {
            f.tag == "iq"
                && f.xmlns.as_deref() == Some(xmlns)
                && matches!(f.attr("type"), Some("get" | "set"))
        })
        .count()
}

/// Every IQ namespace the client asked so far, with its count.
fn iq_namespaces(server: &wire::Wire) -> BTreeMap<String, usize> {
    let mut all = BTreeMap::new();
    for f in server.frames() {
        if f.tag == "iq" && matches!(f.attr("type"), Some("get" | "set")) {
            *all.entry(f.xmlns.clone().unwrap_or_default()).or_insert(0) += 1;
        }
    }
    all
}

/// The IQ namespaces whose count grew since `last` (with the growth); `last` moves on.
fn new_iqs(server: &wire::Wire, last: &mut BTreeMap<String, usize>) -> BTreeMap<String, usize> {
    let now = iq_namespaces(server);
    let grown = now
        .iter()
        .filter_map(|(ns, n)| {
            let before = last.get(ns).copied().unwrap_or(0);
            (*n > before).then(|| (ns.clone(), n - before))
        })
        .collect();
    *last = now;
    grown
}

/// The last `w:g2` request the client sent, whole.
fn last_g2(server: &wire::Wire) -> Option<Node> {
    server
        .frame_nodes()
        .into_iter()
        .rev()
        .find(|n| n.tag == "iq" && attr(n, "xmlns").as_deref() == Some("w:g2"))
}

fn lid_rows(dir: &Path) -> i64 {
    count(&raw(dir), "SELECT count(*) FROM lid_pn_mapping")
}

/// The client's ack of notification `id`, as `class/type` (none yet: `None`).
fn ack_of(server: &wire::Wire, id: &str) -> Option<String> {
    server
        .frames()
        .iter()
        .find(|f| f.tag == "ack" && f.attr("id") == Some(id))
        .map(|f| {
            format!(
                "{}/{}",
                f.attr("class").unwrap_or("-"),
                f.attr("type").unwrap_or("-")
            )
        })
}

async fn wait_ack(server: &wire::Wire, id: &str) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(ack) = ack_of(server, id) {
            return Some(ack);
        }
        if Instant::now() > deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Gathers what did not come out as expected, so one run shows every path.
#[derive(Default)]
struct Expect(Vec<String>);

impl Expect {
    fn eq<T: PartialEq + std::fmt::Debug>(&mut self, what: &str, got: T, want: T) {
        if got != want {
            let line = format!("{what}: got {got:?}, expected {want:?}");
            eprintln!("probe: MISMATCH {line}");
            self.0.push(line);
        }
    }
}

fn print_seen(path: &str, seen: &[Seen]) {
    for s in seen {
        eprintln!("probe: {path}: {s:?}");
    }
}

// ---- the probe ---------------------------------------------------------------------------------------

/// The account: the paired device of the receive tests, plus the store's `usync` row for [`MEMBER_LID`].
async fn account() -> tempfile::TempDir {
    let (dir, _device) = account_dir().await;
    let owner = StoreOwner::open(&db_path(dir.path()), &key(), "wa-test").expect("open");
    owner
        .store()
        .put_lid_mapping(&LidPnMappingEntry {
            lid: MEMBER_LID.into(),
            phone_number: MEMBER_PN.into(),
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
            learning_source: "usync".into(),
        })
        .await
        .expect("the member's mapping");
    owner.close();
    dir
}

/// The fifteen actions of the LID group's notification, in order.
fn lid_notice() -> Gp2 {
    let el = |tag: &'static str| wacore_binary::builder::NodeBuilder::new(tag).build();
    Gp2 {
        group: G_FULL.into(),
        id: "probe-gp2-lid".into(),
        t: NOTICE_T,
        participant: Some(lid(ADMIN_LID)),
        participant_pn: Some(pn(ADMIN_PN)),
        notify: Some(NOTIFY.into()),
        lid: true,
        children: vec![
            wire::gp2_people("promote", &[(lid(MEMBER_LID), None, Some("admin"))]),
            wire::gp2_people("promote", &[(lid(peer::ACCOUNT_LID), None, None)]),
            wire::gp2_people("demote", &[(lid(SUPER_LID), Some(pn(SUPER_PN)), None)]),
            wire::gp2_subject(OLD_SUBJECT, Some(OLDER_SUBJECT_T)),
            wire::gp2_subject(SUBJECT, None),
            wire::gp2_description("probe-topic-2", Some(TOPIC)),
            wire::gp2_description("probe-topic-3", None),
            el("announcement"),
            el("not_announcement"),
            el("locked"),
            el("unlocked"),
            wacore_binary::builder::NodeBuilder::new("ephemeral")
                .attr("expiration", DAY_S.to_string())
                .attr("trigger", "1")
                .build(),
            el("not_ephemeral"),
            wacore_binary::builder::NodeBuilder::new("member_add_mode")
                .string_content("all_member_add")
                .build(),
            el("probe_unknown_action"),
        ],
    }
}

/// What the probe expects of the LID notice's fifteen events (`acked` false: each is handed before the ack).
fn lid_notice_expected() -> Vec<(u32, &'static str, String)> {
    vec![
        (0, "promote", "member/admin/pn=none".into()),
        (1, "promote", "us/participant/pn=none".into()),
        (2, "demote", "super/participant/pn=super-pn".into()),
        (
            3,
            "subject",
            format!("old-subject owner=none s_t=Some({OLDER_SUBJECT_T})"),
        ),
        (4, "subject", "subject owner=none s_t=None".into()),
        (5, "description", "id=probe-topic-2 body=topic".into()),
        (6, "description", "id=probe-topic-3 body=deleted".into()),
        (7, "announce", String::new()),
        (8, "not_announce", String::new()),
        (9, "locked", "threshold=None".into()),
        (10, "unlocked", String::new()),
        (
            11,
            "ephemeral",
            format!("expiration={DAY_S} trigger=Some(1)"),
        ),
        (12, "ephemeral", "expiration=0 trigger=None".into()),
        (13, "member_add_mode", "mode=all_member_add".into()),
        (14, "unknown", "tag=probe_unknown_action".into()),
    ]
}

async fn the_probe() {
    let dir = account().await;
    let server = wire::server();
    let phase = Arc::new(Mutex::new(Batch::Whole));
    server.on_iq(
        "w:g2",
        wire::participating(vec![
            full_group(),
            plain_group(),
            GroupSpec {
                user: G_TRUNC.into(),
                subject: Some(SUBJECT.into()),
                size: Some(1500),
                ..GroupSpec::default()
            },
        ]),
    );
    server.on_iq("w:g2", batch_rule(phase.clone()));
    server.on_iq("w:g2", single_rule);
    server.on_iq("blocklist", wire::blocklist(blocked()));
    let observer = Arc::new(Observer {
        seen: Mutex::new(Vec::new()),
        server: server.clone(),
    });
    let slot = wire::ClientSlot::default();
    let mut actor = start_actor_observing(
        dir.path(),
        StartMode::Resume,
        &server,
        &slot,
        observer.clone(),
    );
    let states = record(actor.conn());
    // The test plays the core: every envelope is taken and acknowledged.
    let (_, empty) = tokio::sync::mpsc::channel(1);
    let mut envelopes = std::mem::replace(&mut actor.ends.events, empty);
    let core_events = Arc::new(Mutex::new(0usize));
    let sink = core_events.clone();
    let drain = tokio::spawn(async move {
        while let Some(envelope) = envelopes.recv().await {
            *sink.lock().expect("lock") += envelope.events.len();
            if let Some(ack) = envelope.ack {
                ack.complete(Ok(()));
            }
        }
    });
    let mut expect = Expect::default();
    let rows_at_start = lid_rows(dir.path());
    let mut last_iqs = BTreeMap::new();

    // 0. The first burst: what the client asks by itself.
    actor.send(Command::Connect).await;
    assert!(
        server.wait_opened(1, Duration::from_secs(15)).await,
        "the handshake never completed"
    );
    server.send(&wire::success_as_the_server_writes_it(peer::ACCOUNT_LID));
    server.send(&wire::hand_over(&format!("{}:0@lid", peer::PEER_LID)));
    let client = slot
        .wait(Duration::from_secs(20))
        .await
        .expect("the client was never handed over");
    wait_for_states(&states, 20, &[ConnState::Online { detail: None }]).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    eprintln!(
        "probe: first burst: IQs by namespace {:?}",
        iq_namespaces(&server)
    );
    // Upstream asks no group by itself. Since B6 Kourier's fetch policy asks the start's TWO IQs when the connection is
    // `Online` (the overview, then ONE batch of the groups it lists: D9, M1); the probe's own calls come after.
    expect.eq(
        "first burst w:g2 IQs (Kourier's overview + batch)",
        iqs(&server, "w:g2"),
        2,
    );
    // Upstream's own read at every connection, and Kourier's one read per start after `Online` (B3, D22; Q1 of the scout:
    // the second identical IQ is accepted). The probe's own calls come after.
    expect.eq(
        "first burst blocklist IQs (upstream's own + Kourier's read)",
        iqs(&server, "blocklist"),
        2,
    );
    let groups = client.groups();

    eprintln!(
        "probe: IQs of the first burst: {:?}",
        new_iqs(&server, &mut last_iqs)
    );
    // 1. The overview list.
    let before = iqs(&server, "w:g2");
    let listed = groups.list_participating().await;
    let request = last_g2(&server);
    let shape = request.as_ref().and_then(|r| {
        r.get_optional_child("participating").map(|p| {
            (
                p.get_children_by_tag("participants").count(),
                p.get_children_by_tag("description").count(),
            )
        })
    });
    eprintln!(
        "probe: list_participating: {} IQ, to={:?}, asked (participants, description) children {shape:?}",
        iqs(&server, "w:g2") - before,
        request.as_ref().and_then(|r| attr(r, "to"))
    );
    match &listed {
        Ok(list) => {
            let words: Vec<String> = list
                .iter()
                .map(|o| {
                    format!(
                        "{} subject={} hierarchy={} count={:?}",
                        who(&o.id),
                        o.subject.as_deref().map_or("none", subject_word),
                        hierarchy(&o.hierarchy),
                        o.participant_count
                    )
                })
                .collect();
            for w in &words {
                eprintln!("probe: list_participating: {w}");
            }
            expect.eq(
                "list_participating",
                words,
                vec![
                    "g-full subject=subject hierarchy=subgroup of g-parent (regular) count=Some(4)"
                        .to_owned(),
                    "g-plain subject=old-subject hierarchy=community count=None".to_owned(),
                    "g-trunc subject=subject hierarchy=standalone count=Some(1500)".to_owned(),
                ],
            );
        }
        Err(e) => expect.eq("list_participating", group_error(e), "ok".into()),
    }
    expect.eq("list_participating IQs", iqs(&server, "w:g2") - before, 1);
    expect.eq("list_participating asks no members", shape, Some((0, 0)));

    eprintln!(
        "probe: IQs of the overview list: {:?}",
        new_iqs(&server, &mut last_iqs)
    );
    // 2. The batch: six groups asked, five answered five ways.
    let asked = [G_FULL, G_PLAIN, G_TRUNC, G_FORBID, G_GONE, G_OMIT].map(group);
    let before = iqs(&server, "w:g2");
    let rows_before = lid_rows(dir.path());
    let batch = groups.fetch_metadata_batch(&asked).await;
    let request = last_g2(&server);
    let asked_users: Vec<&'static str> = request
        .as_ref()
        .map(wire::batch_asked)
        .unwrap_or_default()
        .iter()
        .map(|u| who_user(u))
        .collect();
    eprintln!(
        "probe: batch: {} IQ, to={:?}, asked {asked_users:?}",
        iqs(&server, "w:g2") - before,
        request.as_ref().and_then(|r| attr(r, "to"))
    );
    expect.eq("batch IQs", iqs(&server, "w:g2") - before, 1);
    expect.eq(
        "batch asks each group once, in order",
        asked_users,
        vec![
            "g-full", "g-plain", "g-trunc", "g-forbid", "g-gone", "g-omit",
        ],
    );
    let results = match batch {
        Ok(results) => results,
        Err(e) => {
            expect.eq("batch", group_error(&e), "ok".into());
            Vec::new()
        }
    };
    let kinds: Vec<String> = results.iter().map(result_words).collect();
    eprintln!("probe: batch: results {kinds:?}");
    expect.eq(
        "batch results (the unanswered group has no entry)",
        kinds,
        vec![
            "found g-full".to_owned(),
            "found g-plain".to_owned(),
            "truncated g-trunc count=1500".to_owned(),
            "forbidden g-forbid".to_owned(),
            "not-found g-gone".to_owned(),
        ],
    );
    let mut found: Vec<GroupMetadata> = results
        .into_iter()
        .filter_map(|r| match r {
            GroupLookupResult::Found(m) => Some(*m),
            _ => None,
        })
        .collect();
    for m in &found {
        for w in metadata(m) {
            eprintln!("probe: batch: {}: {w}", who(&m.id));
        }
    }
    if let [full, plain] = found.as_slice() {
        expect.eq(
            "the whole group, as upstream reads it",
            metadata(full),
            vec![
                "id=g-full".into(),
                "subject=subject".into(),
                format!("subject_time=Some({SUBJECT_T})"),
                "subject_owner=super".into(),
                format!("creation=Some({CREATION_T})"),
                "creator=super".into(),
                "addressing=lid".into(),
                "members=[super/superadmin/pn=super-pn/lid=none,admin/admin/pn=admin-pn/lid=none,member/member/pn=none/lid=none,us/member/pn=none/lid=none]".into(),
                "participant_count=Some(4)".into(),
                "description=topic".into(),
                "description_id=Some(\"probe-topic-1\")".into(),
                "description_owner=admin".into(),
                format!("description_time=Some({TOPIC_T})"),
                "locked=true".into(),
                "announcement=true".into(),
                format!("ephemeral=Some((Some({WEEK_S}), Some(1)))"),
                "member_add_mode=admin_add".into(),
                "hierarchy=subgroup of g-parent (regular)".into(),
                "suspended=false".into(),
                "missing_identification=false".into(),
            ],
        );
        expect.eq(
            "the plain group, as upstream reads it",
            metadata(plain),
            vec![
                "id=g-plain".into(),
                "subject=old-subject".into(),
                "subject_time=None".into(),
                "subject_owner=none".into(),
                format!("creation=Some({CREATION_T})"),
                "creator=none".into(),
                "addressing=pn".into(),
                "members=[pnadmin/admin/pn=none/lid=none,us-pn/member/pn=none/lid=none]".into(),
                "participant_count=None".into(),
                "description=none".into(),
                "description_id=None".into(),
                "description_owner=none".into(),
                "description_time=None".into(),
                "locked=false".into(),
                "announcement=false".into(),
                "ephemeral=None".into(),
                "member_add_mode=all_member_add".into(),
                "hierarchy=community".into(),
                "suspended=false".into(),
                "missing_identification=false".into(),
            ],
        );
    } else {
        expect.eq("found groups", found.len(), 2);
    }
    expect.eq(
        "the batch learned no LID pair",
        lid_rows(dir.path()),
        rows_before,
    );
    // Upstream's opt-in enrichment from the store's pairs (no network).
    let before = iqs(&server, "w:g2");
    if let Some(full) = found.first_mut() {
        groups.resolve_participant_addresses(full).await;
        let pns: Vec<String> = full
            .participants
            .iter()
            .map(|p| format!("{}/pn={}", who(&p.jid), who_opt(p.phone_number.as_ref())))
            .collect();
        eprintln!("probe: resolve_participant_addresses: {pns:?}");
        expect.eq(
            "resolve fills the member's number from the store's usync row",
            pns,
            vec![
                "super/pn=super-pn".into(),
                "admin/pn=admin-pn".into(),
                "member/pn=member-pn".into(),
                "us/pn=none".into(),
            ],
        );
    }
    expect.eq("resolve asks nothing", iqs(&server, "w:g2") - before, 0);

    eprintln!(
        "probe: IQs of the batch: {:?}",
        new_iqs(&server, &mut last_iqs)
    );
    // 3. A batch with an undocumented refusal code, a refused batch, an empty batch: what fails, and whether upstream asks again.
    *phase.lock().expect("lock") = Batch::BadCode;
    let before = iqs(&server, "w:g2");
    let bad = groups
        .fetch_metadata_batch(&[group(G_FULL), group(G_BADCODE)])
        .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let bad = bad.map(|r| r.len()).map_err(|e| group_error(&e));
    eprintln!(
        "probe: batch with code 500: {bad:?}, {} IQ in 2 s",
        iqs(&server, "w:g2") - before
    );
    expect.eq(
        "a batch with an undocumented code fails whole",
        bad,
        Err("parse-error".into()),
    );
    expect.eq("no retry of that batch", iqs(&server, "w:g2") - before, 1);
    *phase.lock().expect("lock") = Batch::Refused;
    let before = iqs(&server, "w:g2");
    let refused = groups
        .fetch_metadata_batch(&[group(G_FULL)])
        .await
        .map(|r| r.len())
        .map_err(|e| group_error(&e));
    tokio::time::sleep(Duration::from_secs(2)).await;
    eprintln!(
        "probe: batch answered 503: {refused:?}, {} IQ in 2 s",
        iqs(&server, "w:g2") - before
    );
    expect.eq(
        "a refused batch",
        refused,
        Err("server-error 503 backoff=None".into()),
    );
    expect.eq(
        "no retry of a refused batch",
        iqs(&server, "w:g2") - before,
        1,
    );
    let before = iqs(&server, "w:g2");
    let empty = groups
        .fetch_metadata_batch(&[])
        .await
        .map(|r| r.len())
        .map_err(|e| group_error(&e));
    eprintln!(
        "probe: empty batch: {empty:?}, {} IQ",
        iqs(&server, "w:g2") - before
    );
    expect.eq("an empty batch", empty, Ok(0));
    expect.eq(
        "an empty batch asks nothing",
        iqs(&server, "w:g2") - before,
        0,
    );
    *phase.lock().expect("lock") = Batch::Whole;

    eprintln!(
        "probe: IQs of the failing batches: {:?}",
        new_iqs(&server, &mut last_iqs)
    );
    // 4. The single query: the truncated group whole; refusals 403, 404, 429.
    let before = iqs(&server, "w:g2");
    let mut single = Vec::new();
    for user in [G_TRUNC, G_FORBID, G_GONE, G_OMIT] {
        let r = groups.fetch_metadata(&group(user)).await;
        let words = match &r {
            Ok(m) => format!(
                "{}: members {} count={:?}",
                who_user(user),
                m.participants.len(),
                m.participant_count
            ),
            Err(e) => format!("{}: {}", who_user(user), group_error(e)),
        };
        eprintln!("probe: fetch_metadata: {words}");
        single.push(words);
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    eprintln!(
        "probe: fetch_metadata: {} IQs for 4 calls (2 s after)",
        iqs(&server, "w:g2") - before
    );
    expect.eq(
        "the single query",
        single,
        vec![
            "g-trunc: members 3 count=Some(1500)".to_owned(),
            "g-forbid: server-error 403 backoff=None".to_owned(),
            "g-gone: server-error 404 backoff=None".to_owned(),
            "g-omit: server-error 429 backoff=None".to_owned(),
        ],
    );
    expect.eq(
        "one IQ per single query, no retry",
        iqs(&server, "w:g2") - before,
        4,
    );

    eprintln!(
        "probe: IQs of the single queries: {:?}",
        new_iqs(&server, &mut last_iqs)
    );
    // 5. The blocklist.
    let before = iqs(&server, "blocklist");
    let rows_before = lid_rows(dir.path());
    let list = client.blocking().get_blocklist().await;
    let words = match &list {
        Ok(entries) => entries
            .iter()
            .map(|e| {
                format!(
                    "{} {} t={:?}",
                    who(&e.jid),
                    if e.jid.is_lid() { "lid" } else { "pn" },
                    e.timestamp
                )
            })
            .collect::<Vec<_>>(),
        Err(_) => vec!["error".to_owned()],
    };
    eprintln!(
        "probe: get_blocklist: {} IQ, entries {words:?}",
        iqs(&server, "blocklist") - before
    );
    expect.eq(
        "the blocklist (the entry with no JID is dropped)",
        words,
        vec![
            format!("blocked-pn pn t=Some({BLOCK_T})"),
            format!("blocked-lid lid t=Some({})", BLOCK_T + 1),
            "untimed-pn pn t=None".to_owned(),
        ],
    );
    expect.eq("one blocklist IQ", iqs(&server, "blocklist") - before, 1);
    expect.eq(
        "the blocklist learned no LID pair",
        lid_rows(dir.path()),
        rows_before,
    );

    eprintln!(
        "probe: IQs of the blocklist: {:?}",
        new_iqs(&server, &mut last_iqs)
    );
    // 6. A LID group's notification with fifteen actions.
    let before_g2 = iqs(&server, "w:g2");
    let before_core = *core_events.lock().expect("lock");
    server.send(&lid_notice().node());
    let seen = observer.stanza("probe-gp2-lid").await;
    print_seen("gp2 lid", &seen);
    let ack = wait_ack(&server, "probe-gp2-lid").await;
    eprintln!(
        "probe: gp2 lid: ack {ack:?}; Kourier told the core {} events; {} w:g2 IQs",
        *core_events.lock().expect("lock") - before_core,
        iqs(&server, "w:g2") - before_g2
    );
    let mut want: Vec<Seen> = lid_notice_expected()
        .into_iter()
        .map(|(index, kind, detail)| Seen::Update {
            notification: "probe-gp2-lid".into(),
            index,
            kind: kind.into(),
            actor: "admin",
            actor_pn: "admin-pn",
            notify: true,
            offline: false,
            lid: true,
            incomplete: false,
            at_ms: i64::try_from(NOTICE_T).expect("seconds") * 1000,
            detail,
            acked: false,
        })
        .collect();
    want.push(Seen::Raw {
        notification: "probe-gp2-lid".into(),
        first_child: "promote".into(),
        dirty: vec![],
        acked: false,
    });
    expect.eq("the LID notice's events", seen, want);
    expect.eq(
        "the LID notice is acked after its events",
        ack,
        Some("notification/w:gp2".into()),
    );
    expect.eq(
        "a notice makes the client ask nothing",
        iqs(&server, "w:g2") - before_g2,
        0,
    );

    eprintln!(
        "probe: IQs of the LID notice: {:?}",
        new_iqs(&server, &mut last_iqs)
    );
    // 7. A number-addressed group's notification: an add and a remove.
    let notice = Gp2 {
        group: G_PLAIN.into(),
        id: "probe-gp2-pn".into(),
        t: NOTICE_T + 1,
        participant: Some(pn(PN_ADMIN)),
        children: vec![
            wire::gp2_people("add", &[(pn(NEW_PN), None, None)]),
            wire::gp2_people("remove", &[(pn(NEW_PN), None, None)]),
        ],
        ..Gp2::default()
    };
    server.send(&notice.node());
    let seen = observer.stanza("probe-gp2-pn").await;
    print_seen("gp2 pn", &seen);
    let ack = wait_ack(&server, "probe-gp2-pn").await;
    eprintln!("probe: gp2 pn: ack {ack:?}");
    let at_ms = i64::try_from(NOTICE_T + 1).expect("seconds") * 1000;
    let update = |index: u32, kind: &str| Seen::Update {
        notification: "probe-gp2-pn".into(),
        index,
        kind: kind.into(),
        actor: "pnadmin",
        actor_pn: "none",
        notify: false,
        offline: false,
        lid: false,
        incomplete: false,
        at_ms,
        detail: "new/participant/pn=none".into(),
        acked: false,
    };
    expect.eq(
        "the number group's notice",
        seen,
        vec![
            update(0, "add"),
            update(1, "remove"),
            Seen::Raw {
                notification: "probe-gp2-pn".into(),
                first_child: "add".into(),
                dirty: vec![],
                acked: false,
            },
        ],
    );
    expect.eq(
        "the number group's notice is acked",
        ack,
        Some("notification/w:gp2".into()),
    );

    eprintln!(
        "probe: IQs of the number notice: {:?}",
        new_iqs(&server, &mut last_iqs)
    );
    // 8. The server's groups_dirty.
    let before_g2 = iqs(&server, "w:g2");
    server.send(&wire::groups_dirty(
        "probe-dirty",
        NOTICE_T + 2,
        &[G_FULL, G_PLAIN],
    ));
    let seen = observer.stanza("probe-dirty").await;
    print_seen("groups_dirty", &seen);
    let ack = wait_ack(&server, "probe-dirty").await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    eprintln!(
        "probe: groups_dirty: ack {ack:?}; {} w:g2 IQs in 2 s",
        iqs(&server, "w:g2") - before_g2
    );
    expect.eq(
        "groups_dirty reaches the handlers raw only",
        seen,
        vec![Seen::Raw {
            notification: "probe-dirty".into(),
            first_child: "groups_dirty".into(),
            dirty: vec!["g-full", "g-plain"],
            acked: false,
        }],
    );
    expect.eq("groups_dirty is acked", ack.is_some(), true);
    expect.eq(
        "groups_dirty makes the client ask nothing",
        iqs(&server, "w:g2") - before_g2,
        0,
    );

    eprintln!(
        "probe: IQs of groups_dirty: {:?}",
        new_iqs(&server, &mut last_iqs)
    );
    // The connection held; the store learned no pair from any of it.
    eprintln!(
        "probe: connections {} IQs by namespace {:?} LID rows {} -> {}",
        server.opened(),
        iq_namespaces(&server),
        rows_at_start,
        lid_rows(dir.path())
    );
    expect.eq("connections", server.opened(), 1);
    expect.eq(
        "LID rows over the whole run",
        lid_rows(dir.path()),
        rows_at_start,
    );
    drop(client);
    actor.send(Command::Shutdown).await;
    assert_eq!(actor.exit().await.0, Exit::Shutdown);
    drain.abort();
    assert_eq!(expect.0, Vec::<String>::new(), "the probe's expectations");
}

/// What the child's output and logs must never hold (D10): the fixture's numbers, LIDs, groups and names.
fn secrets() -> Vec<String> {
    [
        peer::ACCOUNT_PN,
        peer::ACCOUNT_LID,
        peer::PEER_LID,
        G_FULL,
        G_PLAIN,
        G_TRUNC,
        G_FORBID,
        G_GONE,
        G_OMIT,
        G_PARENT,
        G_BADCODE,
        SUPER_LID,
        SUPER_PN,
        ADMIN_LID,
        ADMIN_PN,
        MEMBER_LID,
        MEMBER_PN,
        NEW_PN,
        PN_ADMIN,
        BLOCKED_PN,
        BLOCKED_LID,
        UNTIMED_PN,
        SUBJECT,
        OLD_SUBJECT,
        TOPIC,
        NOTIFY,
        "Synthetic",
    ]
    .map(str::to_owned)
    .to_vec()
}

/// The parent: the probe runs in the offline child below, and nothing the child printed holds a secret of the
/// fixture. The probe's facts are printed again here (`--success-output immediate` shows them).
#[test]
fn probe_what_upstream_gives_for_groups_and_the_blocklist() {
    let output = support::offline::run_child_output(module_path!(), "groups_wire_child", &[]);
    for secret in secrets() {
        assert!(
            !output.contains(secret.as_str()),
            "the child printed one of the fixture's secrets"
        );
    }
    for line in output.lines().filter(|l| l.starts_with("probe:")) {
        println!("{line}");
    }
}

/// The child (a no-op as an ordinary test), with every log line recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn groups_wire_child() {
    if !support::offline::in_child() {
        return;
    }
    let log = Capture::default();
    log.install();
    the_probe().await;
    assert!(
        log.own_lines() >= 1,
        "the recorder saw no line of the crate"
    );
    assert_eq!(
        log.leaks(&secrets().iter().map(String::as_str).collect::<Vec<_>>()),
        Vec::<String>::new(),
        "a log line holds a secret of the fixture (the targets are named)"
    );
    assert_eq!(
        log.token_shaped(),
        Vec::<String>::new(),
        "a log line holds something shaped like a key or a token"
    );
}
