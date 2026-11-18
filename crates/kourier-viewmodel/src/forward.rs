//! Forward (forward line F5, design D5): the pure half of the viewmodel's forward flow. The
//! chats a forward may go to, the runs of messages that share a header line, the words of the
//! dialog (what is left out, what changes, why a chat is refused, the limits) and the updates
//! the front end takes. Qt-free and source-free; `core_source/forwarding.rs` keeps the state.
//!
//! Nothing here logs or `Debug`-prints a title, a name or a text.

use std::cmp::Ordering;
use std::collections::HashMap;

/// The look of a converted forward, for the front end's Settings combo and its card.
pub use kourier_core::facade::ForwardLook;
use kourier_core::facade::{ForwardChange, ForwardPlan, ForwardRefusal, ForwardVerdict};
use kourier_model::{AccountId, ConversationId, ForwardRules, MessageId, UserId};
use serde_json::json;

use crate::i18n::{trc, trfc, trnc};

/// Less than this between two neighbours keeps a run together (one hour).
pub const RUN_GAP_MS: i64 = 3_600_000;

// ---------------------------------------------------------------------------------------------
// Targets

/// One chat the forward dialog may list.
#[derive(Clone, PartialEq, Eq)]
pub struct ForwardTarget {
    /// The chat.
    pub id: ConversationId,
    /// Its account.
    pub account: AccountId,
    /// Its title, as the conversation list shows it.
    pub title: String,
    /// The picture URL, empty for initials.
    pub avatar_url: String,
    /// The chat with yourself.
    pub is_self: bool,
    /// The user can send here.
    pub can_send: bool,
    /// In the archive.
    pub archived: bool,
    /// Pinned position, `None` when not pinned.
    pub pinned_order: Option<u32>,
    /// Last activity (ms), the order after pinning.
    pub last_activity: i64,
    /// What the list row shows of its account (the tag or name); empty where only one account
    /// is shown ([`listed_forward_targets`] blanks it).
    pub account_tag: String,
    /// The account's color (`#rrggbb`).
    pub tint: String,
    /// The target is on the source chat's account: the network's own forward. Set by
    /// [`listed_forward_targets`].
    pub native: bool,
}

crate::safe_debug!(ForwardTarget {
    id,
    account,
    is_self,
    can_send,
    archived,
    pinned_order,
    last_activity,
    native
});

/// Role indices of the dialog's target list model.
pub mod target_role {
    /// The conversation id (the row's key).
    pub const CONVERSATION: u32 = 0;
    /// The title.
    pub const TITLE: u32 = 1;
    /// The picture URL.
    pub const AVATAR_URL: u32 = 2;
    /// The chat with yourself.
    pub const IS_SELF: u32 = 3;
    /// The account's tag text.
    pub const ACCOUNT_TAG: u32 = 4;
    /// The account's color.
    pub const TINT: u32 = 5;
    /// Same account as the source.
    pub const NATIVE: u32 = 6;
}

impl ForwardTarget {
    /// The row's identity in a list model: the chat.
    pub fn key(&self) -> ConversationId {
        self.id
    }

    /// Which roles differ between two versions of the same chat's row.
    pub fn changed_roles(a: &Self, b: &Self) -> crate::diff::RoleSet {
        use target_role::{ACCOUNT_TAG, AVATAR_URL, IS_SELF, NATIVE, TINT, TITLE};
        crate::diff::RoleSet::EMPTY
            .with_if(TITLE, a.title != b.title)
            .with_if(AVATAR_URL, a.avatar_url != b.avatar_url)
            .with_if(IS_SELF, a.is_self != b.is_self)
            .with_if(ACCOUNT_TAG, a.account_tag != b.account_tag)
            .with_if(TINT, a.tint != b.tint)
            .with_if(NATIVE, a.native != b.native)
    }
}

/// The dialog's rows: every chat of a SHOWN account where the user can send (Note to Self
/// included) whose title contains `query` (case-insensitive), in the list's order with the
/// archived ones after the inbox's. `native` is set against `source_account`; the account tag
/// stays only where two or more accounts are shown.
pub fn listed_forward_targets(
    all: &[ForwardTarget],
    shown: &[AccountId],
    source_account: AccountId,
    query: &str,
) -> Vec<ForwardTarget> {
    let query = query.trim().to_lowercase();
    let tags = shown.len() > 1;
    let mut out: Vec<ForwardTarget> = all
        .iter()
        .filter(|t| shown.contains(&t.account) && t.can_send)
        .filter(|t| query.is_empty() || t.title.to_lowercase().contains(&query))
        .cloned()
        .map(|mut t| {
            t.native = t.account == source_account;
            if !tags {
                t.account_tag.clear();
            }
            t
        })
        .collect();
    out.sort_by(|a, b| a.archived.cmp(&b.archived).then_with(|| list_order(a, b)));
    out
}

fn list_order(a: &ForwardTarget, b: &ForwardTarget) -> Ordering {
    crate::conversations::compare_order(
        (a.pinned_order, a.last_activity, a.id),
        (b.pinned_order, b.last_activity, b.id),
    )
}

// ---------------------------------------------------------------------------------------------
// Runs and headers

/// Who wrote a going message, for the runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunSender {
    /// The account's own user (any of its devices).
    Own,
    /// One person.
    User(UserId),
    /// Nobody named.
    Unknown,
}

/// One going message as the runs see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunItem {
    /// The message.
    pub id: MessageId,
    /// Who wrote it.
    pub sender: RunSender,
    /// When it was sent (ms).
    pub at: i64,
}

impl RunItem {
    /// The item of a core row.
    pub fn of(row: &kourier_core::facade::MessageRow) -> Self {
        use kourier_core::facade::OriginKind;
        let sender = if row.origin != OriginKind::Remote {
            RunSender::Own
        } else {
            row.author
                .clone()
                .map_or(RunSender::Unknown, RunSender::User)
        };
        Self {
            id: row.id,
            sender,
            at: row.sent_at,
        }
    }
}

/// The first message of each run among `items` (the GOING messages in timeline order): a run is
/// consecutive messages of one sender with less than an hour between neighbours.
pub fn run_firsts(items: &[RunItem]) -> Vec<MessageId> {
    let mut out = Vec::new();
    let mut prev: Option<&RunItem> = None;
    for it in items {
        let same = prev.is_some_and(|p| {
            p.sender == it.sender && it.at.saturating_sub(p.at).abs() < RUN_GAP_MS
        });
        if !same {
            out.push(it.id);
        }
        prev = Some(it);
    }
    out
}

/// The header line of a run: "Forwarded from <name> · <date and time>".
pub fn header_line(name: &str, when: &str) -> String {
    // TRANSLATORS: %1 is the name of the person who wrote the forwarded message, %2 the date
    // and time it was sent.
    trfc(
        "@info forwarded message header",
        "Forwarded from %1 · %2",
        &[name, when],
    )
}

// ---------------------------------------------------------------------------------------------
// The words

/// Why a target is refused, in a sentence.
pub fn refusal_text(r: &ForwardRefusal) -> String {
    match r {
        ForwardRefusal::UnknownChat => trc(
            "@info:status why a forward target is refused",
            "This chat is no longer there.",
        )
        .into_owned(),
        ForwardRefusal::AccountNotLinked => trc(
            "@info:status why a forward target is refused",
            "The account of this chat is not linked.",
        )
        .into_owned(),
        ForwardRefusal::CannotSend(c) => crate::timeline::cannot_send_text(c),
        ForwardRefusal::NetworkOff => trc(
            "@info:status why a forward target is refused",
            "This network is turned off.",
        )
        .into_owned(),
        ForwardRefusal::TooManyChats => trc(
            "@info:status why a forward target is refused",
            "Too many chats for one forward.",
        )
        .into_owned(),
        ForwardRefusal::NothingToSend => trc(
            "@info:status why a forward target is refused",
            "None of the messages can be forwarded.",
        )
        .into_owned(),
        ForwardRefusal::SourceChanged => trc(
            "@info:status why a forward target is refused",
            "The messages changed since you chose them.",
        )
        .into_owned(),
        ForwardRefusal::FileNotHere => trc(
            "@info:status why a forward target is refused",
            "A file of the messages is not on this computer.",
        )
        .into_owned(),
        ForwardRefusal::Failed => trc(
            "@info:status why a forward target is refused",
            "Kourier could not prepare the forward.",
        )
        .into_owned(),
    }
}

/// The line for the messages left out for one verdict; `None` for `Goes`.
fn left_out_line(v: ForwardVerdict, n: u32) -> Option<String> {
    let n = u64::from(n);
    Some(match v {
        ForwardVerdict::Goes => return None,
        ForwardVerdict::ViewOnce => trnc(
            "@info:status, %1 how many messages",
            "%1 view-once message is not forwarded.",
            "%1 view-once messages are not forwarded.",
            n,
        ),
        ForwardVerdict::Notice => trnc(
            "@info:status, %1 how many lines",
            "%1 notice or call line is left out.",
            "%1 notices and call lines are left out.",
            n,
        ),
        ForwardVerdict::Deleted => trnc(
            "@info:status, %1 how many messages",
            "%1 deleted message is left out.",
            "%1 deleted messages are left out.",
            n,
        ),
        ForwardVerdict::Gone => trnc(
            "@info:status, %1 how many messages",
            "%1 message is no longer available.",
            "%1 messages are no longer available.",
            n,
        ),
        ForwardVerdict::Protected => trnc(
            "@info:status, %1 how many messages",
            "%1 message is in a chat that does not allow forwarding.",
            "%1 messages are in a chat that does not allow forwarding.",
            n,
        ),
        ForwardVerdict::Incomplete => trnc(
            "@info:status, %1 how many messages",
            "%1 long message is not fully loaded yet.",
            "%1 long messages are not fully loaded yet.",
            n,
        ),
    })
}

fn change_count(c: &ForwardChange) -> u32 {
    use ForwardChange as C;
    match c {
        C::FormattingDropped(n)
        | C::MentionsAsText(n)
        | C::LinksAsText(n)
        | C::StickerAsPicture(n)
        | C::ContactAsText(n)
        | C::LocationAsText(n)
        | C::PollAsText(n)
        | C::ReplyNotCarried(n) => *n,
        C::FileAsGeneric(v) | C::FileTooLarge(v) | C::FileRefused(v) => {
            u32::try_from(v.len()).unwrap_or(u32::MAX)
        }
    }
}

/// One change in a sentence.
pub fn change_text(c: &ForwardChange) -> String {
    use ForwardChange as C;
    let n = u64::from(change_count(c));
    match c {
        C::FormattingDropped(_) => trnc(
            "@info:status, %1 how many formatting marks",
            "%1 formatting mark the chat cannot show is dropped.",
            "%1 formatting marks the chat cannot show are dropped.",
            n,
        ),
        C::MentionsAsText(_) => trnc(
            "@info:status, %1 how many mentions",
            "%1 mention becomes plain text.",
            "%1 mentions become plain text.",
            n,
        ),
        C::LinksAsText(_) => trnc(
            "@info:status, %1 how many links",
            "%1 link becomes text with its address.",
            "%1 links become text with their addresses.",
            n,
        ),
        C::FileAsGeneric(_) => trnc(
            "@info:status, %1 how many files",
            "%1 file is sent as a plain file.",
            "%1 files are sent as plain files.",
            n,
        ),
        C::FileTooLarge(_) => trnc(
            "@info:status, %1 how many files",
            "%1 file is too large and is left out.",
            "%1 files are too large and are left out.",
            n,
        ),
        C::FileRefused(_) => trnc(
            "@info:status, %1 how many files",
            "%1 file is not accepted and is left out.",
            "%1 files are not accepted and are left out.",
            n,
        ),
        C::StickerAsPicture(_) => trnc(
            "@info:status, %1 how many stickers",
            "%1 sticker is sent as a picture.",
            "%1 stickers are sent as pictures.",
            n,
        ),
        C::ContactAsText(_) => trnc(
            "@info:status, %1 how many contact cards",
            "%1 contact card is sent as text.",
            "%1 contact cards are sent as text.",
            n,
        ),
        C::LocationAsText(_) => trnc(
            "@info:status, %1 how many locations",
            "%1 location is sent as text with a map link.",
            "%1 locations are sent as text with map links.",
            n,
        ),
        C::PollAsText(_) => trnc(
            "@info:status, %1 how many polls",
            "%1 poll is sent as text, without votes.",
            "%1 polls are sent as text, without votes.",
            n,
        ),
        C::ReplyNotCarried(_) => trnc(
            "@info:status, %1 how many replies",
            "%1 reply is sent without its quote.",
            "%1 replies are sent without their quotes.",
            n,
        ),
    }
}

/// What the facts of the dialog need from the source: names of chats and of networks.
pub struct ViewFacts<'a> {
    /// A chat's title as the list shows it.
    pub title: &'a dyn Fn(ConversationId) -> String,
    /// The name of an account's network ("WhatsApp").
    pub network: &'a dyn Fn(AccountId) -> String,
}

/// The dialog's view of a [`ForwardPlan`] (its targets are the ticked chats).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardDialogView {
    /// "Forward Message" / "Forward %1 Messages".
    pub title: String,
    /// How many messages go.
    pub count: u32,
    /// One line per kind of left-out message.
    pub left_out: Vec<String>,
    /// Per ticked target: the chat and its refusal words (empty: none).
    pub targets: Vec<(ConversationId, String)>,
    /// What changes, naming the chats a line concerns when it is not all of them.
    pub changes: Vec<String>,
    /// The limit lines.
    pub limits: Vec<String>,
    /// Any ticked target is converted (the Look choice matters).
    pub look_shown: bool,
    /// At least one ticked target is not refused and no limit is passed.
    pub can_forward: bool,
    /// Files to download first.
    pub downloads: u32,
    /// Files the network no longer has.
    pub lost: u32,
}

impl ForwardDialogView {
    /// The view of `plan`.
    pub fn new(plan: &ForwardPlan, facts: &ViewFacts<'_>) -> Self {
        let count = u32::try_from(
            plan.messages
                .iter()
                .filter(|m| m.verdict == ForwardVerdict::Goes)
                .count(),
        )
        .unwrap_or(u32::MAX);
        let title = trnc(
            "@title:window",
            "Forward Message",
            "Forward %1 Messages",
            u64::from(count),
        );
        let mut left_out = Vec::new();
        for v in [
            ForwardVerdict::ViewOnce,
            ForwardVerdict::Notice,
            ForwardVerdict::Deleted,
            ForwardVerdict::Gone,
            ForwardVerdict::Protected,
            ForwardVerdict::Incomplete,
        ] {
            let n = plan.messages.iter().filter(|m| m.verdict == v).count();
            if n > 0
                && let Some(line) = left_out_line(v, u32::try_from(n).unwrap_or(u32::MAX))
            {
                left_out.push(line);
            }
        }
        let targets: Vec<(ConversationId, String)> = plan
            .targets
            .iter()
            .map(|t| {
                (
                    t.conv,
                    t.refused.as_ref().map(refusal_text).unwrap_or_default(),
                )
            })
            .collect();

        // The changes: one line per kind, over the targets that are not refused.
        let open: Vec<&kourier_core::facade::PlannedTarget> = plan
            .targets
            .iter()
            .filter(|t| t.refused.is_none())
            .collect();
        let mut groups: Vec<(ForwardChange, Vec<ConversationId>)> = Vec::new();
        for t in &open {
            for c in &t.changes {
                match groups
                    .iter_mut()
                    .find(|(g, _)| std::mem::discriminant(g) == std::mem::discriminant(c))
                {
                    Some((g, convs)) => {
                        if change_count(c) > change_count(g) {
                            *g = c.clone();
                        }
                        convs.push(t.conv);
                    }
                    None => groups.push((c.clone(), vec![t.conv])),
                }
            }
        }
        let changes = groups
            .iter()
            .map(|(c, convs)| {
                let line = change_text(c);
                if convs.len() == open.len() {
                    line
                } else {
                    let names = convs
                        .iter()
                        .map(|c| (facts.title)(*c))
                        .collect::<Vec<_>>()
                        .join(", ");
                    trfc(
                        "@info:status, %1 a line of changes, %2 the chats it applies to",
                        "%1 in %2.",
                        &[line.trim_end_matches('.'), &names],
                    )
                }
            })
            .collect();

        let limits = limit_lines(plan, facts);
        let limit_passed = plan
            .targets
            .iter()
            .any(|t| t.refused == Some(ForwardRefusal::TooManyChats));
        Self {
            title,
            count,
            left_out,
            targets,
            changes,
            limits,
            look_shown: open.iter().any(|t| !t.native),
            can_forward: count > 0 && !open.is_empty() && !limit_passed,
            downloads: u32::try_from(plan.to_download.len()).unwrap_or(u32::MAX),
            lost: u32::try_from(plan.lost.len()).unwrap_or(u32::MAX),
        }
    }

    /// The JSON text the dialog reads (numbers, ids and the dialog's own sentences).
    pub fn to_json(&self) -> String {
        json!({
            "title": self.title,
            "count": self.count,
            "leftOut": self.left_out,
            "targets": self.targets.iter()
                .map(|(c, r)| json!({"id": c.0, "refusal": r}))
                .collect::<Vec<_>>(),
            "changes": self.changes,
            "limits": self.limits,
            "lookShown": self.look_shown,
            "canForward": self.can_forward,
            "downloads": self.downloads,
            "lost": self.lost,
        })
        .to_string()
    }
}

fn rules_of(plan: &ForwardPlan, account: AccountId) -> Option<ForwardRules> {
    plan.limits
        .iter()
        .find(|(a, _)| *a == account)
        .map(|(_, r)| *r)
}

/// The sentence of a passed limit: which network's, and how many chats at once.
fn limit_sentence(network: &str, max: u16) -> String {
    if max == 1 {
        trfc(
            "@info:status, %1 the network's name",
            "%1 forwards to one chat at a time.",
            &[network],
        )
    } else {
        trfc(
            "@info:status, %1 the network's name, %2 how many chats",
            "%1 forwards to at most %2 chats at once.",
            &[network, &max.to_string()],
        )
    }
}

/// The "many" rule's sentence: a message forwarded many times.
fn many_sentence(network: &str, max: u16) -> String {
    if max == 1 {
        trfc(
            "@info:status, %1 the network's name",
            "%1 sends a message forwarded many times to one chat at a time.",
            &[network],
        )
    } else {
        trfc(
            "@info:status, %1 the network's name, %2 how many chats",
            "%1 sends a message forwarded many times to at most %2 chats at once.",
            &[network, &max.to_string()],
        )
    }
}

/// The limit lines: one per account with a target refused for `TooManyChats`.
fn limit_lines(plan: &ForwardPlan, facts: &ViewFacts<'_>) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut seen: Vec<(AccountId, bool)> = Vec::new();
    for t in plan
        .targets
        .iter()
        .filter(|t| t.refused == Some(ForwardRefusal::TooManyChats))
    {
        let Some(account) = t.account else { continue };
        let Some(rules) = rules_of(plan, account) else {
            continue;
        };
        // Targets of the account not refused for another reason: more than `max_chats` of
        // them is the plain limit, else the "many" rule held the native ones back.
        let candidates = plan
            .targets
            .iter()
            .filter(|o| o.account == Some(account))
            .filter(|o| matches!(o.refused, None | Some(ForwardRefusal::TooManyChats)))
            .count();
        let plain = rules.max_chats.is_some_and(|m| candidates > usize::from(m));
        let many = !plain && t.native && plan.many;
        if seen.contains(&(account, many)) {
            continue;
        }
        seen.push((account, many));
        let network = (facts.network)(account);
        if many {
            lines.push(many_sentence(&network, rules.many_max_chats));
        } else if let Some(m) = rules.max_chats {
            lines.push(limit_sentence(&network, m));
        }
    }
    lines
}

// ---------------------------------------------------------------------------------------------
// The notice at the end

/// The notice of a finished forward: where it went and which chats refused and why.
pub fn ended_notice(
    sent: &[ConversationId],
    refused: &[(ConversationId, ForwardRefusal)],
    title: &dyn Fn(ConversationId) -> String,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    match sent {
        [] => {}
        [one] => lines.push(trfc(
            "@info:status, %1 the title of the chat",
            "Forwarded to %1",
            &[&title(*one)],
        )),
        many => lines.push(trnc(
            "@info:status, %1 how many chats",
            "Forwarded to %1 chat",
            "Forwarded to %1 chats",
            many.len() as u64,
        )),
    }
    for (conv, why) in refused {
        lines.push(trfc(
            "@info:status, %1 the title of the chat, %2 why",
            "Not forwarded to %1: %2",
            &[&title(*conv), &refusal_text(why)],
        ));
    }
    lines.join("\n")
}

/// The notice of a job that ends before the command: nothing was sent.
pub fn nothing_sent_notice(plan: &ForwardPlan, title: &dyn Fn(ConversationId) -> String) -> String {
    let refused: Vec<(ConversationId, ForwardRefusal)> = plan
        .targets
        .iter()
        .filter_map(|t| t.refused.clone().map(|r| (t.conv, r)))
        .collect();
    ended_notice(&[], &refused, title)
}

// ---------------------------------------------------------------------------------------------
// What the front end takes

/// What the forward flow tells the window ([`crate::source::UiSource::take_forward`]).
#[derive(Clone, PartialEq, Eq)]
pub enum ForwardUpdate {
    /// The dialog's view ([`ForwardDialogView::to_json`]): opens it or replaces its lines.
    Dialog(String),
    /// Files downloaded of those needed, for the job `request`.
    Progress {
        /// The job.
        request: u64,
        /// Files here.
        done: u32,
        /// Files needed.
        total: u32,
    },
    /// Files of the job are no longer on the network: ask "Send Without Them / Cancel"
    /// ([`crate::source::UiSource::forward_without`]).
    AskWithout {
        /// The job.
        request: u64,
        /// How many files.
        count: u32,
    },
    /// Render the messages as pictures and answer with
    /// [`crate::source::UiSource::forward_pictures`] (F7).
    RenderPictures {
        /// The job.
        request: u64,
        /// The chat the messages are in.
        source: ConversationId,
        /// The going messages, in timeline order.
        messages: Vec<MessageId>,
    },
    /// The job is over, with its notice (empty: say nothing).
    Ended {
        /// The job.
        request: u64,
        /// The words.
        notice: String,
    },
    /// The chat's staged forward changed (staged, replaced, dropped, waiting, sent).
    StagedChanged(ConversationId),
    /// A forward asked for was refused: the notice only.
    Refused(String),
}

/// Prints lengths and ids only.
impl std::fmt::Debug for ForwardUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dialog(json) => f.debug_tuple("Dialog").field(&json.len()).finish(),
            Self::Progress {
                request,
                done,
                total,
            } => f
                .debug_struct("Progress")
                .field("request", request)
                .field("done", done)
                .field("total", total)
                .finish(),
            Self::AskWithout { request, count } => f
                .debug_struct("AskWithout")
                .field("request", request)
                .field("count", count)
                .finish(),
            Self::RenderPictures {
                request,
                source,
                messages,
            } => f
                .debug_struct("RenderPictures")
                .field("request", request)
                .field("source", source)
                .field("messages", &messages.len())
                .finish(),
            Self::Ended { request, notice } => f
                .debug_struct("Ended")
                .field("request", request)
                .field("notice", &notice.len())
                .finish(),
            Self::StagedChanged(c) => f.debug_tuple("StagedChanged").field(c).finish(),
            Self::Refused(t) => f.debug_tuple("Refused").field(&t.len()).finish(),
        }
    }
}

/// A forward staged into a chat's composer ([`crate::source::UiSource::forward_of`]).
#[derive(Clone, PartialEq, Eq)]
pub struct StagedForward {
    /// How many messages.
    pub count: usize,
    /// The source chat's title as the list shows it.
    pub from: String,
    /// The look.
    pub look: ForwardLook,
    /// The target is on the source's account.
    pub native: bool,
    /// Its job runs.
    pub waiting: bool,
}

crate::safe_debug!(StagedForward {
    count,
    look,
    native,
    waiting
});

/// The ids of the going messages of a plan, in timeline order.
pub fn going_ids(plan: &ForwardPlan) -> Vec<MessageId> {
    plan.messages
        .iter()
        .filter(|m| m.verdict == ForwardVerdict::Goes)
        .map(|m| m.id)
        .collect()
}

/// The command's headers for `plan`: the first going message of each run with its line.
/// `name_of` words the sender, `when` the time.
pub fn headers_for(
    plan: &ForwardPlan,
    name_of: &dyn Fn(&kourier_core::facade::MessageRow) -> String,
    when: &dyn Fn(i64) -> String,
) -> Vec<(MessageId, kourier_model::Text)> {
    let rows: Vec<&kourier_core::facade::MessageRow> = plan
        .messages
        .iter()
        .filter(|m| m.verdict == ForwardVerdict::Goes)
        .filter_map(|m| m.row.as_ref())
        .collect();
    let items: Vec<RunItem> = rows.iter().map(|r| RunItem::of(r)).collect();
    let by_id: HashMap<MessageId, &&kourier_core::facade::MessageRow> =
        rows.iter().map(|r| (r.id, r)).collect();
    run_firsts(&items)
        .into_iter()
        .filter_map(|id| {
            let row = by_id.get(&id)?;
            Some((
                id,
                kourier_model::Text::plain(header_line(&name_of(row), &when(row.sent_at))),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests;
