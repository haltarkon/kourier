//! Rows of the accounts drawer and the conversation list, the chat header, sorting and
//! filtering (§8.4: sort pinned → last activity in Rust, search filters in Rust).

use std::cmp::Ordering;

use kourier_model::{
    Account, AccountId, AccountState, ConnState, ConvKind, Conversation, ConversationId, Message,
    NetworkKind, UserId,
};

use crate::i18n::{trc, trfc, trnc};
use crate::names::{Names, first_name, single_line};
use crate::preview::{self, Summary};
use crate::time::TimeContext;
use crate::timeline::Receipt;

/// User-facing name of a network.
pub fn network_name(kind: NetworkKind) -> &'static str {
    match kind {
        NetworkKind::Signal => "Signal",
        NetworkKind::WhatsApp => "WhatsApp",
        NetworkKind::Threema => "Threema",
        NetworkKind::Fake => "Test network",
    }
}

/// Key of the network badge icon (`qml/icons/<key>.svg`).
pub fn network_icon(kind: NetworkKind) -> &'static str {
    match kind {
        NetworkKind::Signal => "signal",
        NetworkKind::WhatsApp => "whatsapp",
        NetworkKind::Threema => "threema",
        NetworkKind::Fake => "fake",
    }
}

/// The state dot of an account.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AccountStatus {
    /// Connected and caught up.
    Online,
    /// Connecting or catching up.
    Connecting,
    /// Not connected; retrying.
    Offline,
    /// The user must act (relink, challenge) or the account is restricted.
    NeedsAction,
    /// Unlinked.
    LoggedOut,
}

impl AccountStatus {
    /// Derives the dot from the lifecycle and connection state.
    pub fn new(state: &AccountState, conn: &ConnState) -> Self {
        match state {
            AccountState::LoggedOut { .. } | AccountState::Removing => return Self::LoggedOut,
            AccountState::Linking => return Self::Connecting,
            // Still linked until the logout is through.
            AccountState::Active | AccountState::Unlinking => {}
        }
        match conn {
            ConnState::Online { .. } => Self::Online,
            ConnState::Connecting | ConnState::Syncing { .. } => Self::Connecting,
            ConnState::Offline { .. } => Self::Offline,
            ConnState::NeedsUserAction(_) | ConnState::Restricted { .. } => Self::NeedsAction,
            ConnState::LoggedOut { .. } => Self::LoggedOut,
        }
    }

    /// Stable name for QML (`online`, `connecting`, `offline`, `action`, `loggedOut`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Connecting => "connecting",
            Self::Offline => "offline",
            Self::NeedsAction => "action",
            Self::LoggedOut => "loggedOut",
        }
    }

    /// User-facing label.
    pub fn label(self) -> String {
        match self {
            Self::Online => trc("@info:status state of an account", "Online"),
            Self::Connecting => trc("@info:status state of an account", "Connecting…"),
            Self::Offline => trc("@info:status state of an account", "Offline"),
            Self::NeedsAction => trc("@info:status state of an account", "Needs attention"),
            Self::LoggedOut => trc("@info:status state of an account", "Logged out"),
        }
        .into_owned()
    }
}

/// One account in the drawer.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct AccountRow {
    /// Account id.
    pub id: AccountId,
    /// Profile name, or the network name.
    pub name: String,
    /// Network.
    pub network: NetworkKind,
    /// Network name ("Signal").
    pub network_name: String,
    /// Badge icon key.
    pub network_icon: String,
    /// State dot.
    pub status: AccountStatus,
    /// State label ("Online").
    pub status_label: String,
    /// Unread conversations' message count.
    pub unread: u32,
    /// `image://kourier/avatar/…` URL, empty without an avatar.
    pub avatar_url: String,
    /// The account's own number or address as the network shows it to people (Signal: the
    /// phone number); empty when the network reported none. Personal data.
    pub number: String,
    /// The account offers "Transfer History from Phone…": its network hands a newly linked
    /// device the history, and no transfer of it runs.
    pub offers_history: bool,
    /// The account offers "Unlink…": it is linked and its link is not still going on.
    pub offers_unlink: bool,
    /// Which device of its identity this account is ("device 3"), only when another account
    /// of the profile has the same name and number (a profile from before one identity was
    /// one account, link feedback D10); empty otherwise.
    pub device: String,
}

crate::safe_debug!(AccountRow {
    id,
    network,
    status,
    unread
});

/// Builds the drawer row of an account.
pub fn account_row(account: &Account, conn: &ConnState, unread: u32) -> AccountRow {
    let status = AccountStatus::new(&account.state, conn);
    let network_name = network_name(account.network).to_owned();
    let name = account
        .display_name
        .as_deref()
        .map(single_line)
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| network_name.clone());
    AccountRow {
        id: account.id,
        name,
        network: account.network,
        network_icon: network_icon(account.network).to_owned(),
        network_name,
        status,
        status_label: status.label(),
        unread,
        avatar_url: String::new(),
        number: String::new(),
        offers_history: false,
        offers_unlink: false,
        device: String::new(),
    }
}

/// Whose text the conversation-list preview shows; QML styles the author label and shows the
/// receipt by it (never by comparing translated labels).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PreviewSource {
    /// No author label: nothing yet, a system event, a 1:1 peer, note to self.
    #[default]
    None,
    /// Our own message ("You"): the receipt is shown.
    Own,
    /// Another member of a group ("Olena").
    Peer,
    /// The unsent draft ("Draft").
    Draft,
}

impl PreviewSource {
    /// Stable name for QML (`""`, `own`, `peer`, `draft`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Own => "own",
            Self::Peer => "peer",
            Self::Draft => "draft",
        }
    }
}

/// One row of the conversation list.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ConversationRow {
    /// Conversation id.
    pub id: ConversationId,
    /// Its account.
    pub account: AccountId,
    /// Its network.
    pub network: NetworkKind,
    /// 1:1, group, …
    pub kind: ConvKind,
    /// Note to self.
    pub is_self: bool,
    /// Title.
    pub title: String,
    /// Avatar URL, empty for initials.
    pub avatar_url: String,
    /// Who wrote the previewed message ("You", "Olena", "Draft"); empty in 1:1 chats.
    pub preview_author: String,
    /// What [`Self::preview_author`] stands for.
    pub preview_source: PreviewSource,
    /// One-line preview.
    pub preview: String,
    /// Icon of the preview (media kinds), empty for text.
    pub preview_icon: String,
    /// "typing…" label while someone types; replaces the preview.
    pub typing: String,
    /// Time label of the last activity.
    pub time_label: String,
    /// Last activity (ms), the sort key after pinning.
    pub last_activity: i64,
    /// Unread messages.
    pub unread: u32,
    /// Unread mentions of us.
    pub mentions: u32,
    /// Marked unread by the user.
    pub marked_unread: bool,
    /// Muted (synced or local), now.
    pub muted: bool,
    /// Pinned position, `None` when not pinned.
    pub pinned_order: Option<u32>,
    /// Receipt of the previewed message when it is our own.
    pub receipt: Receipt,
    /// Account label next to the badge ("Work"); empty when only one account exists.
    pub account_badge: String,
    /// Badge icon key of the account's network; empty when only one account exists.
    pub badge_icon: String,
}

crate::safe_debug!(ConversationRow {
    id,
    account,
    network,
    kind,
    is_self,
    preview_source,
    last_activity,
    unread,
    mentions,
    marked_unread,
    muted,
    pinned_order,
    receipt,
});

impl ConversationRow {
    /// Pinned.
    pub fn pinned(&self) -> bool {
        self.pinned_order.is_some()
    }
}

/// What a conversation row needs besides the conversation.
pub struct ConversationInput<'a> {
    /// The conversation.
    pub conversation: &'a Conversation,
    /// Its newest message, if any.
    pub last: Option<&'a Message>,
    /// Users typing now.
    pub typing: &'a [UserId],
    /// The network of the conversation's account.
    pub network: NetworkKind,
    /// Several accounts exist: rows carry a network badge.
    pub multi_account: bool,
    /// Short account label shown next to the badge ("Work"); empty for none.
    pub account_label: &'a str,
}

/// Whether a conversation is muted at `now_ms` (synced or local mute).
pub fn is_muted(c: &Conversation, now_ms: i64) -> bool {
    [c.muted_until, c.local_muted_until]
        .into_iter()
        .flatten()
        .any(|until| until > now_ms)
}

/// Title of a conversation: its title, the peer's name, or "Note to Self".
pub fn conversation_title(c: &Conversation, names: &dyn Names) -> String {
    if c.is_self {
        return trc(
            "@item:intext title of the chat with yourself",
            "Note to Self",
        )
        .into_owned();
    }
    if let Some(t) = c
        .title
        .as_deref()
        .map(single_line)
        .filter(|t| !t.is_empty())
    {
        return t;
    }
    match c.key.direct_peer() {
        Some(peer) => names.display_name(peer),
        None => trc(
            "@item:intext title of a group without a name",
            "Unnamed group",
        )
        .into_owned(),
    }
}

/// "Olena is typing…" (or "typing…" in 1:1 chats).
pub fn typing_label(typing: &[UserId], is_group: bool, names: &dyn Names) -> String {
    if typing.is_empty() {
        return String::new();
    }
    if !is_group {
        return trc("@info:status in a one-to-one chat", "typing…").into_owned();
    }
    let first = |u: &UserId| first_name(&names.display_name(u)).to_owned();
    match typing {
        [a] => trfc(
            "@info:status in a group chat, %1 a first name",
            "%1 is typing…",
            &[&first(a)],
        ),
        [a, b] => trfc(
            "@info:status in a group chat, %1 and %2 first names",
            "%1 and %2 are typing…",
            &[&first(a), &first(b)],
        ),
        more => trnc(
            "@info:status in a group chat, %1 how many people (at least 3)",
            "%1 people are typing…",
            "%1 people are typing…",
            more.len() as u64,
        ),
    }
}

/// Builds a conversation-list row.
pub fn conversation_row(
    input: &ConversationInput<'_>,
    names: &dyn Names,
    time: &TimeContext<'_>,
) -> ConversationRow {
    let c = input.conversation;
    let is_group = c.kind != ConvKind::Direct;
    let (mut preview_author, mut preview_source, mut summary, mut receipt) = match input.last {
        Some(m) => {
            let own = m.origin.is_own();
            let system = matches!(m.body, kourier_model::Body::System(_));
            let (author, source) = if system || c.is_self {
                (String::new(), PreviewSource::None)
            } else if own {
                (
                    trc(
                        "@item:intext author of the last message in the chat list, yourself",
                        "You",
                    )
                    .into_owned(),
                    PreviewSource::Own,
                )
            } else if is_group {
                let name = m
                    .origin
                    .author()
                    .map(|u| first_name(&names.display_name(u)).to_owned())
                    .unwrap_or_default();
                let source = if name.is_empty() {
                    PreviewSource::None
                } else {
                    PreviewSource::Peer
                };
                (name, source)
            } else {
                (String::new(), PreviewSource::None)
            };
            let receipt = if source == PreviewSource::Own && m.deleted.is_none() {
                Receipt::from_state(m.out_state.as_ref())
            } else {
                Receipt::None
            };
            (author, source, preview::message_summary(m, names), receipt)
        }
        None => (
            String::new(),
            PreviewSource::None,
            Summary::default(),
            Receipt::None,
        ),
    };
    if let Some(draft) = c.draft.as_ref().filter(|d| !d.body.trim().is_empty()) {
        preview_author = trc(
            "@item:intext in the chat list, the text of an unsent message follows",
            "Draft",
        )
        .into_owned();
        preview_source = PreviewSource::Draft;
        receipt = Receipt::None;
        summary = Summary {
            text: preview::text_line(draft, names),
            icon: String::new(),
        };
    }
    let last_activity = input
        .last
        .map_or(c.last_activity, |m| m.sent_at.max(c.last_activity));
    let (account_badge, badge_icon) = if input.multi_account {
        (
            single_line(input.account_label),
            network_icon(input.network).to_owned(),
        )
    } else {
        (String::new(), String::new())
    };
    ConversationRow {
        id: c.id,
        account: c.account,
        network: input.network,
        kind: c.kind,
        is_self: c.is_self,
        title: conversation_title(c, names),
        avatar_url: String::new(),
        preview_author,
        preview_source,
        preview: summary.text,
        preview_icon: summary.icon,
        typing: typing_label(input.typing, is_group, names),
        time_label: if last_activity > 0 {
            time.list_label(last_activity)
        } else {
            String::new()
        },
        last_activity,
        unread: c.unread,
        mentions: c.mentions,
        marked_unread: c.marked_unread,
        muted: is_muted(c, time.now_ms),
        pinned_order: c.pinned_order,
        receipt,
        account_badge,
        badge_icon,
    }
}

/// The list order: pinned first (by pin position), then newest activity, then id.
pub fn compare_rows(a: &ConversationRow, b: &ConversationRow) -> Ordering {
    match (a.pinned_order, b.pinned_order) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => b.last_activity.cmp(&a.last_activity),
    }
    .then(b.id.cmp(&a.id))
}

/// Sorts rows into list order.
pub fn sort_rows(rows: &mut [ConversationRow]) {
    rows.sort_by(compare_rows);
}

/// Which conversations the list shows.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ConversationFilter {
    /// Only this account ("All chats" = `None`).
    pub account: Option<AccountId>,
    /// Case-insensitive search in titles (and previews); empty = everything.
    pub query: String,
}

impl std::fmt::Debug for ConversationFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConversationFilter")
            .field("account", &self.account)
            .field("query_len", &self.query.len())
            .finish()
    }
}

impl ConversationFilter {
    /// Whether `row` passes.
    pub fn matches(&self, row: &ConversationRow) -> bool {
        if self.account.is_some_and(|a| a != row.account) {
            return false;
        }
        let q = self.query.trim();
        if q.is_empty() {
            return true;
        }
        let q = q.to_lowercase();
        row.title.to_lowercase().contains(&q) || row.preview.to_lowercase().contains(&q)
    }

    /// Filters and sorts `rows` into list order.
    pub fn apply(&self, rows: impl IntoIterator<Item = ConversationRow>) -> Vec<ConversationRow> {
        let mut out: Vec<_> = rows.into_iter().filter(|r| self.matches(r)).collect();
        sort_rows(&mut out);
        out
    }
}

/// The chat page header.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ChatHeader {
    /// Conversation id.
    pub id: ConversationId,
    /// Its account.
    pub account: AccountId,
    /// Title.
    pub title: String,
    /// Typing label, else members count, else the account's network.
    pub subtitle: String,
    /// Someone is typing (the subtitle is the typing label).
    pub typing: bool,
    /// Avatar URL, empty for initials.
    pub avatar_url: String,
    /// Group-like conversation.
    pub is_group: bool,
    /// Note to self.
    pub is_self: bool,
    /// Muted now.
    pub muted: bool,
    /// Why sending is impossible; empty when it is possible.
    pub cannot_send: String,
    /// What the composer may attach (`ConversationFeatures.files`).
    pub attach: crate::media::AttachRules,
    /// The disappearing-message timer in seconds (0 = off).
    pub timer_secs: u32,
    /// The timer as the header shows it ("1 week"); empty when off.
    pub timer: String,
    /// The timers the conversation offers, "Off" first; empty when the timer cannot be
    /// changed here (the network has none, or we cannot send). A timer that is set and is
    /// none of the offered values (a custom time, or one set on another device) is listed
    /// too, so the menu always marks what holds.
    pub timers: Vec<TimerOption>,
    /// The range of timers the conversation takes besides the offered ones ("Custom Time…");
    /// `None`: only the offered values, or the timer cannot be changed here.
    pub timer_range: Option<TimerSpan>,
    /// Deleting here (the chat's history, the chat, an attachment) is also told to the
    /// account's other devices; `false`: it stays on this device (the network has no such
    /// message, or the account is logged out).
    pub deletes_sync: bool,
    /// Why "Clear Chat History" and "Delete Chat" are not offered now (its message history
    /// is still arriving); empty when they are.
    pub still_filling: String,
}

/// The custom timers a conversation takes: every `min + k × step` seconds up to `max`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TimerSpan {
    /// The shortest timer (seconds, at least 1).
    pub min: u32,
    /// The longest timer (seconds).
    pub max: u32,
    /// The distance between two values (seconds, at least 1).
    pub step: u32,
}

impl TimerSpan {
    /// Whether `secs` is one of its values. (A step of 0 counts as 1.)
    pub fn contains(&self, secs: u32) -> bool {
        secs >= self.min && secs <= self.max && (secs - self.min).is_multiple_of(self.step.max(1))
    }

    /// The value of the span nearest to `secs` (what a "custom time" input settles on). A
    /// span whose end lies before its start has one value, its start; a step of 0 counts
    /// as 1.
    pub fn nearest(&self, secs: u64) -> u32 {
        let min = u64::from(self.min);
        let max = u64::from(self.max).max(min);
        let step = u64::from(self.step.max(1));
        let clamped = secs.clamp(min, max);
        let steps = (clamped - min + step / 2) / step;
        let value = (min + steps * step).min(max - (max - min) % step);
        u32::try_from(value).unwrap_or(self.min)
    }
}

/// The range of custom timers a conversation takes (`ConversationFeatures.disappearing_custom`):
/// the widest stated range (a network states one per kind of timer; the menu sets the kind the
/// network uses). `None` when it states none.
pub fn timer_span(features: &kourier_model::ConversationFeatures) -> Option<TimerSpan> {
    features
        .disappearing_custom
        .values()
        .filter(|r| r.max_secs > 0 && r.max_secs >= r.min_secs)
        .max_by_key(|r| r.max_secs)
        .map(|r| TimerSpan {
            min: r.min_secs.max(1),
            max: r.max_secs,
            step: r.step_secs.max(1),
        })
}

/// The entries of the timer menu: the offered values ([`timer_options`]), plus the timer that
/// is set when it is none of them, in its place by length. With only a custom range and no
/// offered values the menu is "Off" and what is set.
pub fn timer_menu(
    features: &kourier_model::ConversationFeatures,
    current: u32,
) -> Vec<TimerOption> {
    let mut options = timer_options(features);
    if options.is_empty() {
        if timer_span(features).is_none() {
            return options;
        }
        options.push(TimerOption {
            secs: 0,
            label: trc("@item:inlistbox disappearing messages timer", "Off").into_owned(),
        });
    }
    if current > 0 && !options.iter().any(|o| o.secs == current) {
        let at = options
            .iter()
            .position(|o| o.secs > current)
            .unwrap_or(options.len());
        options.insert(
            at,
            TimerOption {
                secs: current,
                label: preview::timer_label(std::time::Duration::from_secs(u64::from(current))),
            },
        );
    }
    options
}

crate::safe_debug!(ChatHeader {
    id,
    account,
    typing,
    is_group,
    is_self,
    muted,
    timer_secs
});

/// One entry of the disappearing-message menu.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TimerOption {
    /// Seconds; 0 turns the timer off.
    pub secs: u32,
    /// "Off", "30 seconds", "1 week".
    pub label: String,
}

/// The timer entries a conversation offers (`ConversationFeatures.disappearing`): "Off", then
/// every listed value in ascending order. Empty when the network offers none.
pub fn timer_options(features: &kourier_model::ConversationFeatures) -> Vec<TimerOption> {
    let mut secs: Vec<u32> = features
        .disappearing
        .values()
        .flatten()
        .copied()
        .filter(|s| *s > 0)
        .collect();
    secs.sort_unstable();
    secs.dedup();
    if secs.is_empty() {
        return Vec::new();
    }
    std::iter::once(TimerOption {
        secs: 0,
        label: trc("@item:inlistbox disappearing messages timer", "Off").into_owned(),
    })
    .chain(secs.into_iter().map(|s| TimerOption {
        secs: s,
        label: preview::timer_label(std::time::Duration::from_secs(u64::from(s))),
    }))
    .collect()
}

/// What an account needs the user to know or do, shown above the conversation list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BannerKind {
    /// Connecting to the network.
    Connecting,
    /// Connected, catching up on what arrived meanwhile.
    Syncing,
    /// A freshly linked account still fetches the profiles of its contacts and the groups
    /// the server has not given yet (after its history transfer, or without one): chats
    /// work, some names and groups are still coming. It says more than "connecting" or
    /// "getting new messages", and less than a running transfer.
    Updating,
    /// The history transfer of a new link runs (waiting for the phone, downloading,
    /// reading): nothing new arrives until it ends. It says more than "connecting" or
    /// "getting new messages", and less than any state that cuts the account off.
    Transfer,
    /// Not connected; it reconnects by itself.
    Offline,
    /// The history transfer of a new link failed: the banner stays until the user dismisses
    /// it or tries again. It waits for the user, so it goes before a state that passes by
    /// itself, and behind every state that stops messages now.
    TransferFailed,
    /// The supervisor gave up on the account after repeated errors: "Restart".
    Error,
    /// The network is temporarily restricted for this account.
    Restricted,
    /// The system clock is off: nothing can be sent or received until it is set.
    Clock,
    /// The network wants a challenge solved in the browser before it takes more messages.
    Challenge,
    /// This device was unlinked: "Link Again".
    LoggedOut,
}

impl BannerKind {
    /// Stable name for QML (`connecting`, `syncing`, `offline`, `error`, `restricted`,
    /// `clock`, `challenge`, `loggedOut`). `updating`, `transfer` and `transferFailed` never
    /// reach it:
    /// a history transfer is not the state banner's kind (`UiSource::transfer_banner`), the
    /// two only say where a transfer stands in the order of ONE account's banners.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Syncing => "syncing",
            Self::Updating => "updating",
            Self::Transfer => "transfer",
            Self::Offline => "offline",
            Self::TransferFailed => "transferFailed",
            Self::Error => "error",
            Self::Restricted => "restricted",
            Self::Clock => "clock",
            Self::Challenge => "challenge",
            Self::LoggedOut => "loggedOut",
        }
    }

    /// Whether the state passes by itself (a quiet indicator, not a call to act).
    pub fn passing(self) -> bool {
        matches!(self, Self::Connecting | Self::Syncing | Self::Offline)
    }
}

/// A banner above the conversation list: the most pressing state among the accounts shown,
/// or (chosen apart, `transfer` set) the history transfer of one of them.
#[derive(Clone, PartialEq)]
pub struct Banner {
    /// The account it is about.
    pub account: AccountId,
    /// What it is.
    pub kind: BannerKind,
    /// The text.
    pub text: String,
    /// Label of its action button ("Link Again", "Verify…", "Restart"); empty for none.
    pub action: String,
    /// Where a challenge is solved (handed to the browser); empty otherwise. Never logged.
    pub url: String,
    /// The history transfer the banner shows ([`BannerKind::Transfer`],
    /// [`BannerKind::TransferFailed`]): the same view the link page shows large.
    pub transfer: Option<crate::transfer::TransferView>,
}

crate::safe_debug!(Banner { account, kind });

/// The banner an account's state calls for; `None` when it is online. `stopped`: the
/// supervisor gave up on it (`UiEvent::AccountError`).
pub fn account_banner(
    account: AccountId,
    state: &AccountState,
    conn: &ConnState,
    stopped: bool,
) -> Option<Banner> {
    use kourier_model::UserAction;
    // `action` is the label of the banner's button, already translated; empty for none.
    let banner = |kind, text: String, action: String, url: &str| {
        Some(Banner {
            account,
            kind,
            text,
            action,
            url: url.to_owned(),
            transfer: None,
        })
    };
    let no_action = String::new;
    let logged_out = || {
        banner(
            BannerKind::LoggedOut,
            trc(
                "@info:status banner of an account",
                "This device is no longer linked. Link it again to send and receive messages.",
            )
            .into_owned(),
            trc("@action:button in the banner of an account", "Link Again").into_owned(),
            "",
        )
    };
    match state {
        AccountState::LoggedOut { .. } => return logged_out(),
        AccountState::Linking | AccountState::Removing => return None,
        AccountState::Active | AccountState::Unlinking => {}
    }
    match conn {
        ConnState::LoggedOut { .. } | ConnState::NeedsUserAction(UserAction::Relink) => {
            logged_out()
        }
        ConnState::NeedsUserAction(UserAction::SolveChallenge { url }) => banner(
            BannerKind::Challenge,
            trc(
                "@info:status banner of an account",
                "Messages are waiting: the network asks for a quick check that a person is sending them.",
            )
            .into_owned(),
            trc("@action:button in the banner of an account", "Verify…").into_owned(),
            browser_url(url),
        ),
        ConnState::NeedsUserAction(UserAction::FixClock { skew_ms }) => {
            banner(BannerKind::Clock, clock_text(*skew_ms), no_action(), "")
        }
        ConnState::NeedsUserAction(UserAction::UpdateApp) => banner(
            BannerKind::Restricted,
            trc(
                "@info:status banner of an account",
                "This version of Kourier is too old for the network. Update it to send and receive messages.",
            )
            .into_owned(),
            no_action(),
            "",
        ),
        ConnState::NeedsUserAction(UserAction::Reconnect) => banner(
            BannerKind::Error,
            trc(
                "@info:status banner of an account",
                "Another session took over the connection.",
            )
            .into_owned(),
            trc("@action:button in the banner of an account", "Reconnect").into_owned(),
            "",
        ),
        ConnState::Restricted { reason, .. } => {
            let reason = single_line(reason);
            banner(
                BannerKind::Restricted,
                if reason.is_empty() {
                    trc(
                        "@info:status banner of an account",
                        "The network restricted this account for now.",
                    )
                    .into_owned()
                } else {
                    reason
                },
                no_action(),
                "",
            )
        }
        _ if stopped => banner(
            BannerKind::Error,
            trc(
                "@info:status banner of an account",
                "This account stopped after repeated errors.",
            )
            .into_owned(),
            trc("@action:button in the banner of an account", "Restart").into_owned(),
            "",
        ),
        ConnState::Offline { .. } => banner(
            BannerKind::Offline,
            trc(
                "@info:status banner of an account",
                "Offline. Messages are sent once the connection is back.",
            )
            .into_owned(),
            no_action(),
            "",
        ),
        ConnState::Connecting => banner(
            BannerKind::Connecting,
            trc("@info:status banner of an account", "Connecting…").into_owned(),
            no_action(),
            "",
        ),
        ConnState::Syncing { .. } => banner(
            BannerKind::Syncing,
            trc(
                "@info:status banner of an account",
                "Getting new messages…",
            )
            .into_owned(),
            no_action(),
            "",
        ),
        ConnState::Online { .. } => None,
    }
}

/// A challenge address as it may be handed to the browser: an `https://` web address, else
/// nothing (the dialog then only takes the answer pasted by hand). The address comes from the
/// backend, which may have it from the network: anything that is not a web page (a `file:`
/// path, another application's scheme) must never reach `Qt.openUrlExternally` (§9.4).
fn browser_url(url: &str) -> &str {
    let web = url
        .get(..8)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
        && url.len() > 8
        && !url.chars().any(|c| c.is_control() || c.is_whitespace());
    if web { url } else { "" }
}

/// "The clock of this computer is about 2 hours ahead. …"
fn clock_text(skew_ms: i64) -> String {
    let minutes = skew_ms.unsigned_abs() / 60_000;
    let amount = if minutes >= 120 {
        trnc(
            "@info a length of time",
            "%1 hour",
            "%1 hours",
            (minutes + 30) / 60,
        )
    } else {
        trnc(
            "@info a length of time",
            "%1 minute",
            "%1 minutes",
            minutes.max(1),
        )
    };
    if skew_ms >= 0 {
        trfc(
            "@info:status banner of an account, %1 a length of time",
            "The clock of this computer is about %1 ahead. Set the date and time to send and receive messages.",
            &[&amount],
        )
    } else {
        trfc(
            "@info:status banner of an account, %1 a length of time",
            "The clock of this computer is about %1 behind. Set the date and time to send and receive messages.",
            &[&amount],
        )
    }
}

/// A timer in whole seconds for the header (0 = off).
pub fn timer_secs(timer: Option<std::time::Duration>) -> u32 {
    timer.map_or(0, |t| u32::try_from(t.as_secs()).unwrap_or(u32::MAX))
}

/// Builds the chat header of a conversation.
pub fn chat_header(
    c: &Conversation,
    typing: &[UserId],
    network: NetworkKind,
    names: &dyn Names,
    time: &TimeContext<'_>,
) -> ChatHeader {
    let is_group = c.kind != ConvKind::Direct;
    let typing_text = typing_label(typing, is_group, names);
    let subtitle = if !typing_text.is_empty() {
        typing_text.clone()
    } else if c.is_self {
        trc(
            "@info:status subtitle of the chat with yourself",
            "Messages to yourself",
        )
        .into_owned()
    } else if is_group {
        trnc(
            "@info:status subtitle of a group chat, %1 how many members",
            "%1 member",
            "%1 members",
            c.members.len() as u64,
        )
    } else {
        network_name(network).to_owned()
    };
    let cannot_send = match &c.features.cannot_send {
        None => String::new(),
        Some(reason) => crate::timeline::cannot_send_text(reason),
    };
    ChatHeader {
        id: c.id,
        account: c.account,
        title: conversation_title(c, names),
        subtitle,
        typing: !typing_text.is_empty(),
        avatar_url: String::new(),
        is_group,
        is_self: c.is_self,
        muted: is_muted(c, time.now_ms),
        cannot_send,
        attach: crate::media::AttachRules::new(&c.features.files),
        timer_secs: timer_secs(c.timer),
        timer: c.timer.map(preview::timer_label).unwrap_or_default(),
        timers: if c.features.cannot_send.is_some() {
            Vec::new()
        } else {
            timer_menu(&c.features, timer_secs(c.timer))
        },
        timer_range: if c.features.cannot_send.is_some() {
            None
        } else {
            timer_span(&c.features)
        },
        deletes_sync: c.features.delete_for_me,
        still_filling: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::names::NameMap;
    use crate::time::FixedOffset;

    fn row(id: i64, pinned: Option<u32>, last: i64) -> ConversationRow {
        let fx = fixture::Fixture::build().expect("fixture");
        let c = &fx.conversations[0];
        let names = NameMap::new();
        let time = TimeContext::new(fx.now_ms, &FixedOffset(0));
        let mut r = conversation_row(
            &ConversationInput {
                conversation: c,
                last: None,
                typing: &[],
                network: NetworkKind::Signal,
                multi_account: false,
                account_label: "",
            },
            &names,
            &time,
        );
        r.id = ConversationId(id);
        r.pinned_order = pinned;
        r.last_activity = last;
        r
    }

    #[test]
    fn sort_pinned_then_activity() {
        let mut rows = vec![
            row(1, None, 10),
            row(2, Some(1), 1),
            row(3, None, 30),
            row(4, Some(0), 2),
            row(5, None, 30),
        ];
        sort_rows(&mut rows);
        let ids: Vec<i64> = rows.iter().map(|r| r.id.0).collect();
        assert_eq!(ids, [4, 2, 5, 3, 1]);
    }

    #[test]
    fn filter_by_account_and_query() {
        let mut a = row(1, None, 1);
        a.title = "Olena Kovalenko".into();
        a.account = AccountId(1);
        let mut b = row(2, None, 2);
        b.title = "FPV Squad".into();
        b.preview = "props for olena".into();
        b.account = AccountId(2);
        let f = ConversationFilter {
            account: None,
            query: "OLENA".into(),
        };
        assert_eq!(f.apply([a.clone(), b.clone()]).len(), 2);
        let f = ConversationFilter {
            account: Some(AccountId(1)),
            query: " ".into(),
        };
        let out = f.apply([a, b]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, ConversationId(1));
    }

    #[test]
    fn typing_labels() {
        let mut n = NameMap::new();
        let a = UserId::parse("aci:a").expect("id");
        let b = UserId::parse("aci:b").expect("id");
        let c = UserId::parse("aci:c").expect("id");
        n.insert(a.clone(), "Olena Kovalenko");
        n.insert(b.clone(), "Taras");
        assert_eq!(typing_label(&[], true, &n), "");
        assert_eq!(typing_label(std::slice::from_ref(&a), false, &n), "typing…");
        assert_eq!(
            typing_label(std::slice::from_ref(&a), true, &n),
            "Olena is typing…"
        );
        assert_eq!(
            typing_label(&[a.clone(), b.clone()], true, &n),
            "Olena and Taras are typing…"
        );
        assert_eq!(typing_label(&[a, b, c], true, &n), "3 people are typing…");
    }

    #[test]
    fn account_status_dots() {
        let active = AccountState::Active;
        assert_eq!(
            AccountStatus::new(&active, &ConnState::Online { detail: None }),
            AccountStatus::Online
        );
        assert_eq!(
            AccountStatus::new(&active, &ConnState::Syncing { fraction: None }),
            AccountStatus::Connecting
        );
        assert_eq!(
            AccountStatus::new(
                &AccountState::LoggedOut {
                    reason: String::new()
                },
                &ConnState::Online { detail: None }
            ),
            AccountStatus::LoggedOut
        );
    }
}
