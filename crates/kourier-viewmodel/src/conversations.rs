//! Rows of the accounts drawer and the conversation list, the chat header, sorting and
//! filtering (§8.4: sort pinned → last activity in Rust, search filters in Rust).

use std::cmp::Ordering;
use std::collections::BTreeSet;

use kourier_model::{
    Account, AccountHold, AccountId, AccountState, AccountUuid, BanReason, ConnState, ConvKind,
    Conversation, ConversationId, HoldReason, LogoutCause, Message, NetworkKind, NetworkNote,
    Restriction, UserId,
};

use crate::i18n::{trc, trfc, trnc};
use crate::login::NetworkMissing;
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
        NetworkKind::Telegram => "Telegram",
    }
}

/// Key of the network badge icon (`qml/icons/<key>.svg`).
pub fn network_icon(kind: NetworkKind) -> &'static str {
    match kind {
        NetworkKind::Signal => "signal",
        NetworkKind::WhatsApp => "whatsapp",
        NetworkKind::Threema => "threema",
        NetworkKind::Fake => "fake",
        NetworkKind::Telegram => "telegram",
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
    /// The account's stable uuid: what the saved choice of shown accounts names (Rust only,
    /// no role).
    pub uuid: AccountUuid,
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
    /// The account's chats are in the list (the choice of [`crate::shown_accounts`]; filled
    /// by [`ShownAccounts::fill_rows`](crate::shown_accounts::ShownAccounts::fill_rows), the
    /// builders leave it `true`).
    pub shown: bool,
    /// Its place among the visible accounts of its network in panel order (1, 2, …), `0`
    /// when it is the only one of its network (same filler).
    pub place: u32,
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
        uuid: account.uuid,
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
        shown: true,
        place: 0,
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
    /// In the archive, not in the inbox. A pinned chat is never archived (Signal's merge rule:
    /// a phone state with both shows the chat pinned in the inbox). Network-agnostic: whatever
    /// backend set the flag.
    pub archived: bool,
    /// Receipt of the previewed message when it is our own.
    pub receipt: Receipt,
    /// Account label next to the badge ("Work"); empty when only one account exists.
    pub account_badge: String,
    /// Badge icon key of the account's network; empty when only one account exists.
    pub badge_icon: String,
    /// The number on the badge: the account's place among the accounts of its network in the
    /// panel (1, 2, …), `0` (no number) when it is the only one of its network. Set by
    /// [`AccountTable::decorate`], with the account name and the icon.
    pub badge_number: u32,
    /// The account's place in the panel (0 = first): the key of the pinned block in
    /// [`compare_rows`]. No role; `u32::MAX` for an account the panel does not list.
    pub account_order: u32,
    /// The user protects the chat on another device: the preview is [`protected_preview`]
    /// (no author, icon, receipt or time) and a search finds the chat by its title only.
    pub protected: bool,
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
    archived,
    receipt,
    badge_number,
    account_order,
    protected,
});

impl ConversationRow {
    /// A protected chat shows none of its messages in the list (Threema Desktop's conversation
    /// list, `conversation-preview-list/helpers.ts` 46-56 and `ConversationPreview.svelte` 161:
    /// "Private" in place of the last message and of the draft, no time): the placeholder is
    /// the whole preview, with no author, icon, receipt or time label.
    pub(crate) fn hide_messages(&mut self) {
        self.preview_author = String::new();
        self.preview_source = PreviewSource::None;
        self.preview = protected_preview();
        self.preview_icon = String::new();
        self.receipt = Receipt::None;
        self.time_label = String::new();
    }

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
}

/// What the list shows in place of the last message of a protected chat ("Protected chat").
pub fn protected_preview() -> String {
    trc(
        "@item:intext in the chat list, in place of the last message of a protected chat",
        "Protected chat",
    )
    .into_owned()
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
    let mut row = ConversationRow {
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
        archived: shown_archived(c.archived, c.pinned_order),
        receipt,
        // (the account's chip, icon and number are the account table's:
        // [`AccountTable::decorate`], once per call)
        account_badge: String::new(),
        badge_icon: String::new(),
        badge_number: 0,
        account_order: 0,
        protected: c.protected,
    };
    if row.protected {
        row.hide_messages();
    }
    row
}

/// Whether a chat with these flags is in the archive: archived and not pinned (a pinned chat is
/// never archived; Signal's merge rule). The one rule of every row builder, of the unread
/// numbers that leave the archive out and of the archive's count.
pub fn shown_archived(archived: bool, pinned_order: Option<u32>) -> bool {
    archived && pinned_order.is_none()
}

/// The list order: pinned first, then newest activity, then id. The pinned block goes account
/// by account in the panel's order, each account's chats in its phone's pin order (with one
/// account, or the accounts' `account_order` all equal, that is the pin order alone).
pub fn compare_rows(a: &ConversationRow, b: &ConversationRow) -> Ordering {
    match (a.pinned_order, b.pinned_order) {
        (Some(x), Some(y)) => a
            .account_order
            .cmp(&b.account_order)
            .then(x.cmp(&y))
            .then(b.id.cmp(&a.id)),
        _ => compare_order(
            (a.pinned_order, a.last_activity, a.id),
            (b.pinned_order, b.last_activity, b.id),
        ),
    }
}

/// The list order of two chats given as `(pinned position, last activity, id)`: the one rule
/// of [`compare_rows`] and of the share picker ([`crate::share::listed_targets`]).
pub fn compare_order(
    a: (Option<u32>, i64, ConversationId),
    b: (Option<u32>, i64, ConversationId),
) -> Ordering {
    match (a.0, b.0) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => b.1.cmp(&a.1),
    }
    .then(b.2.cmp(&a.2))
}

/// Sorts rows into list order.
pub fn sort_rows(rows: &mut [ConversationRow]) {
    rows.sort_by(compare_rows);
}

/// Which conversations the list shows.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ConversationFilter {
    /// The accounts whose chats the list leaves out (the hidden ones; nothing hidden = all
    /// chats). Hidden, not shown: an account that joins is shown with no further rule. A
    /// source turns it into ids once per call ([`Self::resolve`]); a search ignores it.
    pub hidden: BTreeSet<AccountUuid>,
    /// Case-insensitive search in titles (and previews); empty = everything of the view. A
    /// search looks in BOTH views (Signal Desktop's: the inbox's matches first, then the
    /// archive's) and in EVERY account (the hidden ones' matches are mixed in by time).
    pub query: String,
    /// The archive view is shown: its chats instead of the inbox's (when no query is typed).
    pub archived: bool,
}

impl std::fmt::Debug for ConversationFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConversationFilter")
            .field("hidden", &self.hidden.len())
            .field("query_len", &self.query.len())
            .field("archived", &self.archived)
            .finish()
    }
}

/// The hidden accounts of one call, as the source's ids: [`ConversationFilter::resolve`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hidden {
    /// The accounts have not answered yet, so the uuids cannot be told from ids: nothing
    /// that depends on the hidden set is listed (no flash of chats that are hidden).
    Unknown,
    /// These accounts' chats are left out (empty = none).
    Ids(BTreeSet<AccountId>),
}

impl Default for Hidden {
    fn default() -> Self {
        Self::Ids(BTreeSet::new())
    }
}

impl ConversationFilter {
    /// Turns the hidden uuids into account ids with the source's `(id, uuid)` pairs. An empty
    /// set depends on nothing and resolves at once; a set before the accounts answered
    /// (`known` false) is [`Hidden::Unknown`]; a uuid no account has hides nothing.
    pub fn resolve(
        &self,
        accounts: impl IntoIterator<Item = (AccountId, AccountUuid)>,
        known: bool,
    ) -> Hidden {
        if self.hidden.is_empty() {
            return Hidden::default();
        }
        if !known {
            return Hidden::Unknown;
        }
        Hidden::Ids(
            accounts
                .into_iter()
                .filter(|(_, uuid)| self.hidden.contains(uuid))
                .map(|(id, _)| id)
                .collect(),
        )
    }

    /// Whether `row` passes. A search ignores `hidden` (every account is searched); without
    /// one, an account of `hidden` is left out, and so is every chat while `hidden` is
    /// [`Hidden::Unknown`].
    pub fn matches_resolved(&self, row: &ConversationRow, hidden: &Hidden) -> bool {
        let q = self.query.trim();
        if q.is_empty() {
            return match hidden {
                Hidden::Unknown => false,
                Hidden::Ids(ids) => !ids.contains(&row.account) && row.archived == self.archived,
            };
        }
        let q = q.to_lowercase();
        // A protected chat is found by its title only, never by what it holds (Threema
        // Desktop's message search leaves such chats out, `search/nav/store/helpers.ts` 164-167).
        row.title.to_lowercase().contains(&q)
            || (!row.protected && row.preview.to_lowercase().contains(&q))
    }

    /// Whether the archive view's count counts `account`'s archived chat (not by the query or
    /// the view): the hidden accounts' chats do not, and nothing does before they are known.
    pub fn counts_archived(&self, account: AccountId, hidden: &Hidden) -> bool {
        match hidden {
            Hidden::Unknown => false,
            Hidden::Ids(ids) => !ids.contains(&account),
        }
    }

    /// Filters and sorts `rows` into list order; the archive's matches of a search come after
    /// every inbox match, each group in list order.
    pub fn apply_resolved(
        &self,
        rows: impl IntoIterator<Item = ConversationRow>,
        hidden: &Hidden,
    ) -> Vec<ConversationRow> {
        let mut out: Vec<_> = rows
            .into_iter()
            .filter(|r| self.matches_resolved(r, hidden))
            .collect();
        sort_rows(&mut out);
        // (a stable sort: inside each group the list order stays)
        out.sort_by_key(|r| r.archived);
        out
    }
}

/// The unread messages of the hidden accounts: the sum of their rail pills (`unread`: chats
/// that are not muted and not archived; a chat only marked unread adds nothing, while the
/// taskbar's total counts it as one). The dot on the narrow window's menu button.
pub fn hidden_unread(accounts: &[AccountRow]) -> u32 {
    accounts
        .iter()
        .filter(|a| !a.shown)
        .map(|a| a.unread)
        .fold(0, u32::saturating_add)
}

/// Each account's place among the accounts of its network, in the order given: 1, 2, … where
/// the network has two or more accounts, `0` for the only one of its network. The one rule of
/// the number on the badge and of the panel's rows ([`AccountRow::place`]).
pub fn network_places(networks: impl IntoIterator<Item = NetworkKind>) -> Vec<u32> {
    let networks: Vec<NetworkKind> = networks.into_iter().collect();
    let mut seen: Vec<(NetworkKind, u32)> = Vec::new();
    let mut places: Vec<u32> = networks
        .iter()
        .map(|k| match seen.iter_mut().find(|(s, _)| s == k) {
            Some((_, n)) => {
                *n += 1;
                *n
            }
            None => {
                seen.push((*k, 1));
                1
            }
        })
        .collect();
    for (place, kind) in places.iter_mut().zip(&networks) {
        if seen.iter().any(|(k, n)| k == kind && *n == 1) {
            *place = 0;
        }
    }
    places
}

/// One account as the account table reads it, in panel order.
pub struct TableAccount {
    /// Account id.
    pub id: AccountId,
    /// Its network.
    pub network: NetworkKind,
    /// The name its chats carry (the chip): the profile name, else the network's name; empty
    /// for none (the pure fixture's accounts that carry no label).
    pub name: String,
}

struct TableEntry {
    id: AccountId,
    /// Place in the panel.
    order: u32,
    /// Place among its network's accounts, `0` for the only one ([`network_places`]).
    place: u32,
    shown: bool,
    /// The chip text, the number added where another account of the network has the name.
    chip: String,
}

/// What the rows of one list call need to know about the accounts: built ONCE per call from the
/// visible accounts in panel order and the call's hidden set, then [`Self::decorate`] gives every
/// row its network icon, account chip, badge number and place in the panel.
///
/// The rules (his answers of 2026-10-13/14):
/// - the network icon on the avatar: two or more accounts are visible (hidden ones count);
/// - the number on it: the account's place among its network's accounts whenever the network has
///   two or more, also when one account is shown ("Stays");
/// - the account's name next to the title: two or more accounts are shown; in a search also on
///   every row of a hidden account (its matches are found and marked with their account);
/// - the name is the account's own, else its network's (the caller's `name`); where another
///   visible account of the SAME network has the same name it adds its place ("Andrii · 2").
///   Two accounts of different networks keep plain names: each is the only one of its network,
///   so its icon already tells them apart.
pub struct AccountTable {
    entries: Vec<TableEntry>,
    badge: bool,
    shown_count: usize,
    searching: bool,
}

impl AccountTable {
    /// The table of the visible `accounts` (panel order) for a call whose hidden set is
    /// `hidden`; `searching`: the call has a query. While the hidden set is
    /// [`Hidden::Unknown`] every account counts as shown.
    pub fn new(
        accounts: impl IntoIterator<Item = TableAccount>,
        hidden: &Hidden,
        searching: bool,
    ) -> Self {
        let accounts: Vec<TableAccount> = accounts.into_iter().collect();
        let places = network_places(accounts.iter().map(|a| a.network));
        let entries: Vec<TableEntry> = accounts
            .iter()
            .zip(&places)
            .enumerate()
            .map(|(i, (a, &place))| {
                let name = single_line(&a.name);
                let twin = accounts.iter().any(|b| {
                    b.id != a.id && b.network == a.network && single_line(&b.name) == name
                });
                let chip = if twin && !name.is_empty() {
                    trfc(
                        "@item:intext account name on a chat row, %1 the name, %2 the account's number among the accounts of its network",
                        "%1 · %2",
                        &[&name, &place.to_string()],
                    )
                } else {
                    name
                };
                TableEntry {
                    id: a.id,
                    order: u32::try_from(i).unwrap_or(u32::MAX),
                    place,
                    shown: match hidden {
                        Hidden::Unknown => true,
                        Hidden::Ids(ids) => !ids.contains(&a.id),
                    },
                    chip,
                }
            })
            .collect();
        Self {
            badge: entries.len() > 1,
            shown_count: entries.iter().filter(|e| e.shown).count(),
            entries,
            searching,
        }
    }

    /// Sets the icon, chip, number and panel place of `row` (its `account` names the entry).
    pub fn decorate(&self, row: &mut ConversationRow) {
        row.badge_icon = if self.badge {
            network_icon(row.network).to_owned()
        } else {
            String::new()
        };
        let Some(e) = self.entries.iter().find(|e| e.id == row.account) else {
            // (an account the panel does not list: a link under way or a removal)
            row.account_badge = String::new();
            row.badge_number = 0;
            row.account_order = u32::MAX;
            return;
        };
        row.account_order = e.order;
        row.badge_number = e.place;
        row.account_badge = if (e.shown && self.shown_count > 1) || (!e.shown && self.searching) {
            e.chip.clone()
        } else {
            String::new()
        };
    }

    /// The listed rows of a search (after [`Self::decorate`] and the filter): when they hold
    /// chats of two or more accounts, EVERY row names its account, the shown account's too (his
    /// Q2: hidden matches mixed in by time, "each with its account's name on the row"). Without
    /// a search nothing changes.
    pub fn finish(&self, rows: &mut [ConversationRow]) {
        if !self.searching {
            return;
        }
        let Some(first) = rows.first().map(|r| r.account) else {
            return;
        };
        if rows.iter().all(|r| r.account == first) {
            return;
        }
        for r in rows.iter_mut().filter(|r| r.account_badge.is_empty()) {
            if let Some(e) = self.entries.iter().find(|e| e.id == r.account) {
                r.account_badge.clone_from(&e.chip);
            }
        }
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
    /// The other person of a 1:1 chat (the user's own id in Note to Self): whose page the
    /// header opens. `None` for a group.
    pub peer: Option<UserId>,
    /// The header opens a details panel: the person's page (a 1:1 chat, Note to Self) or the
    /// group's. A channel or a distribution list has no page yet, so its header is a plain title.
    pub has_details: bool,
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
    /// Why the disappearing-message timer cannot be changed now (the account's network holds
    /// back what is sent, [`AccountHold`]); empty when it can.
    pub timer_held: String,
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
    has_details,
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
    /// Part of the history the network pushed after the link never arrived (WhatsApp: the
    /// phone sent a chunk once and this computer did not get it, `BackendNotice::HistoryLost`).
    /// A quiet line that STAYS for the session (the backend tells it once per start, not
    /// again) and the lowest of all kinds: every state, a running transfer and the quiet
    /// "getting your history" line stand above it, so it shows only where the account shows
    /// nothing else.
    HistoryLost,
    /// Connecting to the network.
    Connecting,
    /// Connected, catching up on what arrived meanwhile.
    Syncing,
    /// The network sends the account's message history to this computer in pieces (WhatsApp,
    /// after the link): chats work, older messages are still coming. A quiet line that
    /// passes by itself; it says more than "getting new messages" and less than the profiles
    /// a new link still fetches.
    History,
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
    /// The account's network is turned off in this Kourier (an experimental network the person
    /// has not switched on at this start): the account is paused. Its chats stay readable,
    /// nothing is sent or received, and "Turn On" switches the network on again. It stops
    /// messages now, so it goes before `TransferFailed` and every passing state, and behind
    /// the states of an account that is logged out or gave up (those the person must fix
    /// whatever the network switch says).
    NetworkOff,
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
    /// Stable name for QML (`historyLost`, `connecting`, `syncing`, `offline`, `error`, `restricted`,
    /// `clock`, `challenge`, `loggedOut`, `networkOff`). `history`, `updating`, `transfer` and `transferFailed` never
    /// reach it:
    /// a history transfer is not the state banner's kind (`UiSource::transfer_banner`), the
    /// two only say where a transfer stands in the order of ONE account's banners.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HistoryLost => "historyLost",
            Self::Connecting => "connecting",
            Self::Syncing => "syncing",
            Self::History => "history",
            Self::Updating => "updating",
            Self::Transfer => "transfer",
            Self::Offline => "offline",
            Self::TransferFailed => "transferFailed",
            Self::NetworkOff => "networkOff",
            Self::Error => "error",
            Self::Restricted => "restricted",
            Self::Clock => "clock",
            Self::Challenge => "challenge",
            Self::LoggedOut => "loggedOut",
        }
    }

    /// Whether the state passes by itself (a quiet indicator, not a call to act). The lost
    /// history is quiet and does not pass (it stays for the session).
    pub fn passing(self) -> bool {
        matches!(
            self,
            Self::Connecting | Self::Syncing | Self::History | Self::Offline
        )
    }
}

/// The banners above the list for one choice of shown accounts
/// ([`crate::source::UiSource::banners`]).
#[derive(Clone, PartialEq, Default)]
pub struct Banners {
    /// The most pressing state: of the shown accounts, and of a hidden one that needs the user
    /// (`Error`, `Restricted`, `Clock`, `Challenge`, `LoggedOut`).
    pub state: Option<Banner>,
    /// The most pressing history transfer of any account, a hidden one's too (it shows
    /// whatever that account's state is).
    pub transfer: Option<Banner>,
    /// The most pressing history transfer of the SHOWN accounts alone: what the empty-list
    /// placeholder may tell about (a hidden account's transfer is not coming into this list).
    pub shown_transfer: Option<Banner>,
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
    /// What the network itself said about a ban or a logout, shown under `text` as PLAIN
    /// text (one line, cut); empty when it said nothing. Never logged.
    pub explanation: String,
    /// The page the network offers about it, an `https://` address or empty. Opened only by
    /// a click on the button labelled [`Banner::appeal_label`] through `App.openExternalUrl`.
    pub appeal_url: String,
    /// The label of that button ("Open WhatsApp's Page"); empty with no address.
    pub appeal_label: String,
}

crate::safe_debug!(Banner { account, kind });

/// What a banner needs besides the account's state.
#[derive(Clone, Copy)]
pub struct BannerContext<'a> {
    /// The account's network, by name ("WhatsApp").
    pub network: &'a str,
    /// Now and the calendar: the labels of "until" and whether a ban is over.
    pub time: &'a TimeContext<'a>,
}

/// The banner an account's state calls for; `None` when it is online. `stopped`: the
/// supervisor gave up on it (`UiEvent::AccountError`).
pub fn account_banner(
    account: AccountId,
    state: &AccountState,
    conn: &ConnState,
    stopped: bool,
    ctx: &BannerContext<'_>,
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
            explanation: String::new(),
            appeal_url: String::new(),
            appeal_label: String::new(),
        })
    };
    let no_action = String::new;
    // The connection state may say why the device was logged out (a stored row does not).
    let logged_out = || {
        let (cause, note) = match conn {
            ConnState::LoggedOut { cause, note, .. } => (*cause, note.clone()),
            _ => (LogoutCause::default(), NetworkNote::default()),
        };
        let mut b = banner(
            BannerKind::LoggedOut,
            logged_out_text(cause, ctx.network),
            // A network that closed the account or refused the app has nothing to link again.
            if matches!(cause, LogoutCause::AccountGone | LogoutCause::Unofficial) {
                no_action()
            } else {
                trc("@action:button in the banner of an account", "Link Again").into_owned()
            },
            "",
        );
        if let Some(b) = b.as_mut() {
            add_note(b, &note, ctx.network);
        }
        b
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
        ConnState::Restricted {
            until_ms,
            kind,
            note,
            ..
        } => {
            // The backend's `reason` is for the log; the words are ours (R4).
            let ended = until_ms.is_some_and(|t| i64::try_from(t).unwrap_or(i64::MAX) <= ctx.time.now_ms);
            let until = until_ms.map(|t| ctx.time.until(i64::try_from(t).unwrap_or(i64::MAX)));
            let mut b = if ended {
                // The ban is over but the connection does not come back by itself: the
                // person says when ("Reconnect" is the restart of the account).
                banner(
                    BannerKind::Error,
                    trfc(
                        "@info:status banner of an account, %1 the name of its network",
                        "The %1 ban on this account has ended. Reconnect to send and receive again.",
                        &[ctx.network],
                    ),
                    trc("@action:button in the banner of an account", "Reconnect").into_owned(),
                    "",
                )
            } else {
                banner(
                    BannerKind::Restricted,
                    restricted_text(*kind, ctx.network, until.as_deref()),
                    no_action(),
                    "",
                )
            };
            if let Some(b) = b.as_mut() {
                add_note(b, note, ctx.network);
            }
            b
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

/// The sentence for a connection the network refuses for now. `until`: the label of the end
/// ("today at 14:00"), absent when the network gave none. A reason is never a piece put into
/// a sentence: every reason has its own message, with and without a date.
fn restricted_text(kind: Restriction, net: &str, until: Option<&str>) -> String {
    let Restriction::Banned(reason) = kind else {
        return match until {
            Some(u) => trfc(
                "@info:status banner of an account, %1 the name of the network, %2 the end",
                "%1 restricted this account until %2.",
                &[net, u],
            ),
            None => trfc(
                "@info:status banner of an account, %1 the name of the network",
                "%1 restricted this account for now.",
                &[net],
            ),
        };
    };
    match (reason, until) {
        (BanReason::TooManyStrangers, Some(u)) => trfc(
            "@info:status banner of an account, %1 the name of the network, %2 the end",
            "%1 banned this account until %2: you sent too many messages to people who have not saved your number.",
            &[net, u],
        ),
        (BanReason::TooManyStrangers, None) => trfc(
            "@info:status banner of an account, %1 the name of the network",
            "%1 banned this account for now: you sent too many messages to people who have not saved your number.",
            &[net],
        ),
        (BanReason::ManyBlocked, Some(u)) => trfc(
            "@info:status banner of an account, %1 the name of the network, %2 the end",
            "%1 banned this account until %2: too many people blocked you.",
            &[net, u],
        ),
        (BanReason::ManyBlocked, None) => trfc(
            "@info:status banner of an account, %1 the name of the network",
            "%1 banned this account for now: too many people blocked you.",
            &[net],
        ),
        (BanReason::TooManyGroups, Some(u)) => trfc(
            "@info:status banner of an account, %1 the name of the network, %2 the end",
            "%1 banned this account until %2: you created too many groups with people who have not saved your number.",
            &[net, u],
        ),
        (BanReason::TooManyGroups, None) => trfc(
            "@info:status banner of an account, %1 the name of the network",
            "%1 banned this account for now: you created too many groups with people who have not saved your number.",
            &[net],
        ),
        (BanReason::SameMessageToMany, Some(u)) => trfc(
            "@info:status banner of an account, %1 the name of the network, %2 the end",
            "%1 banned this account until %2: you sent the same message to too many people.",
            &[net, u],
        ),
        (BanReason::SameMessageToMany, None) => trfc(
            "@info:status banner of an account, %1 the name of the network",
            "%1 banned this account for now: you sent the same message to too many people.",
            &[net],
        ),
        (BanReason::BroadcastList, Some(u)) => trfc(
            "@info:status banner of an account, %1 the name of the network, %2 the end",
            "%1 banned this account until %2: you sent too many messages to a broadcast list.",
            &[net, u],
        ),
        (BanReason::BroadcastList, None) => trfc(
            "@info:status banner of an account, %1 the name of the network",
            "%1 banned this account for now: you sent too many messages to a broadcast list.",
            &[net],
        ),
        (BanReason::Other(_), Some(u)) => trfc(
            "@info:status banner of an account, %1 the name of the network, %2 the end",
            "%1 banned this account until %2.",
            &[net, u],
        ),
        (BanReason::Other(_), None) => trfc(
            "@info:status banner of an account, %1 the name of the network",
            "%1 banned this account for now.",
            &[net],
        ),
    }
}

/// The banner of an account whose network holds back what the user sends while the connection
/// stays up ([`AccountHold`]): the same kind as a restriction, so it stands in the order where
/// a restriction does. `None` once its end has passed: the core lifts a hold by itself at its
/// end, and until it says so the banner must not claim a date that is gone.
pub fn hold_banner(
    account: AccountId,
    hold: &AccountHold,
    ctx: &BannerContext<'_>,
) -> Option<Banner> {
    let end = hold.until_ms.map(|t| i64::try_from(t).unwrap_or(i64::MAX));
    if end.is_some_and(|t| t <= ctx.time.now_ms) {
        return None;
    }
    let until = end.map(|t| ctx.time.until(t));
    Some(Banner {
        account,
        kind: BannerKind::Restricted,
        text: hold_text(hold.reason, ctx.network, until.as_deref()),
        action: String::new(),
        url: String::new(),
        transfer: None,
        explanation: String::new(),
        appeal_url: String::new(),
        appeal_label: String::new(),
    })
}

/// The sentence of a send hold. Each reason has its own message, with and without a date:
/// what is held and for how long is the one thing the person needs from it.
fn hold_text(reason: HoldReason, net: &str, until: Option<&str>) -> String {
    match (reason, until) {
        (HoldReason::ReachOutLocked, Some(u)) => trfc(
            "@info:status banner of an account, %1 the name of the network, %2 the end",
            "%1 restricts this account until %2: messages to people who have not written to you may be refused. Kourier holds your messages until then.",
            &[net, u],
        ),
        (HoldReason::ReachOutLocked, None) => trfc(
            "@info:status banner of an account, %1 the name of the network",
            "%1 restricts this account for now: messages to people who have not written to you may be refused. Kourier holds your messages until it ends.",
            &[net],
        ),
        // A frozen account (Telegram): read-only on the network's side until it lifts the freeze,
        // which no date announces (`until_ms` is always None for it), so one sentence says it,
        // with the way out: the appeal is made in the network's own app.
        (HoldReason::Frozen, _) => trfc(
            "@info:status banner of an account, %1 the name of the network",
            "%1 has frozen this account. You can read your chats; Kourier holds what you send until %1 lifts it. To appeal, open the %1 app.",
            &[net],
        ),
        (HoldReason::Unknown, Some(u)) => trfc(
            "@info:status banner of an account, %1 the name of the network, %2 the end",
            "%1 is not taking the messages you send until %2. Kourier holds them until then.",
            &[net, u],
        ),
        (HoldReason::Unknown, None) => trfc(
            "@info:status banner of an account, %1 the name of the network",
            "%1 is not taking the messages you send for now. Kourier holds them until it ends.",
            &[net],
        ),
    }
}

/// The sentence of an account that was logged out, by the cause the network gave.
fn logged_out_text(cause: LogoutCause, net: &str) -> String {
    match cause {
        LogoutCause::Unknown => trc(
            "@info:status banner of an account",
            "This device is no longer linked. Link it again to send and receive messages.",
        )
        .into_owned(),
        LogoutCause::RemovedOnPhone => trc(
            "@info:status banner of an account",
            "This computer was removed from the linked devices on your phone. Link it again to send and receive messages.",
        )
        .into_owned(),
        LogoutCause::PhoneInactive => trfc(
            "@info:status banner of an account, %1 the name of the network",
            "%1 unlinked this computer because your phone was not used for a long time. Link it again to send and receive messages.",
            &[net],
        ),
        LogoutCause::AccountGone => trfc(
            "@info:status banner of an account, %1 the name of the network",
            "%1 closed or locked this account.",
            &[net],
        ),
        LogoutCause::Unofficial => trfc(
            "@info:status banner of an account, %1 the name of the network",
            "%1 logged this account out because Kourier is not an official app.",
            &[net],
        ),
    }
}

/// How long the network's own explanation may be on a banner (characters).
const EXPLANATION_CHARS: usize = 400;

/// What a network said itself (a banner's explanation, a login field's note) as ONE cut line of
/// plain text: newlines and control characters become spaces, the ends are trimmed, and a text
/// longer than [`EXPLANATION_CHARS`] ends in "…". Empty when it said nothing.
pub(crate) fn note_line(text: &str) -> String {
    let line = single_line(text);
    if line.chars().count() > EXPLANATION_CHARS {
        let cut: String = line.chars().take(EXPLANATION_CHARS).collect();
        format!("{}…", cut.trim_end())
    } else {
        line
    }
}

/// Puts what the network itself said under the banner's sentence: its text as one cut line
/// of plain text, its page only when it is an `https://` address.
fn add_note(banner: &mut Banner, note: &NetworkNote, net: &str) {
    if let Some(text) = note.explanation.as_deref() {
        banner.explanation = note_line(text);
    }
    if let Some(url) = note.appeal_url.as_deref() {
        let web = browser_url(url);
        if !web.is_empty() {
            banner.appeal_url = web.to_owned();
            banner.appeal_label = trfc(
                "@action:button in the banner of an account, %1 the name of the network",
                "Open %1's Page",
                &[net],
            );
        }
    }
}

/// The quiet line of an account whose network pushed its history and lost part of it
/// ([`BannerKind::HistoryLost`]). The count of lost chunks is not shown: a chunk is the
/// network's unit, not something a person counts, and the sentence is the same for one and
/// for many. No button: nothing here can bring the history back (Kourier cannot ask the phone again).
pub fn history_lost_banner(account: AccountId) -> Banner {
    Banner {
        account,
        kind: BannerKind::HistoryLost,
        text: trc(
            "@info:status banner of an account whose network lost part of its message history",
            "Part of your message history did not arrive from your phone.",
        )
        .into_owned(),
        action: String::new(),
        url: String::new(),
        transfer: None,
        explanation: String::new(),
        appeal_url: String::new(),
        appeal_label: String::new(),
    }
}

/// The banner of an account whose network is turned off in this Kourier: `name` is the
/// network's name ("WhatsApp"). The button is "Turn On and Restart" (it does restart: the
/// setting needs it; K-A fix U6); what it does is the front-end's, not the banner's.
pub fn network_off_banner(account: AccountId, name: &str) -> Banner {
    Banner {
        account,
        kind: BannerKind::NetworkOff,
        text: trfc(
            "@info:status banner of an account, %1 the name of its network",
            "%1 is turned off. Turn it on to send and receive.",
            &[name],
        ),
        action: trc(
            "@action:button in the banner of an account, turns a network on and restarts Kourier",
            "Turn On and Restart",
        )
        .into_owned(),
        url: String::new(),
        transfer: None,
        explanation: String::new(),
        appeal_url: String::new(),
        appeal_label: String::new(),
    }
}

/// The banner of an account whose network this build cannot log in to (Telegram without its API
/// key or library): `name` is the network's name. The account is paused and its chats stay
/// readable, like one of a network that is turned off, but turning the network on would not
/// help, so there is no button: nothing the person can press fixes it (KT-1b, his Q4 and U-1).
/// It stands in the order where [`network_off_banner`] does.
pub fn network_missing_banner(account: AccountId, name: &str, missing: NetworkMissing) -> Banner {
    Banner {
        account,
        kind: BannerKind::NetworkOff,
        text: match missing {
            NetworkMissing::ApiKey => trfc(
                "@info:status banner of an account, %1 the name of its network",
                "This build of Kourier has no %1 API key, so it cannot connect to %1. The chats stay readable.",
                &[name],
            ),
            NetworkMissing::Library => trfc(
                "@info:status banner of an account, %1 the name of its network",
                "The %1 part of Kourier is missing from this installation. The chats stay readable.",
                &[name],
            ),
        },
        action: String::new(),
        url: String::new(),
        transfer: None,
        explanation: String::new(),
        appeal_url: String::new(),
        appeal_label: String::new(),
    }
}

/// A challenge address as it may be handed to the browser: an `https://` web address, else
/// nothing (the dialog then only takes the answer pasted by hand). The address comes from the
/// backend, which may have it from the network: anything that is not a web page (a `file:`
/// path, another application's scheme) must never reach `App.openExternalUrl` (§9.4).
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

/// How many of a group's member rows are people IN the group: members and admins. A person
/// who is only invited, or who asked to join, is no member yet. The header's "6 members" and the
/// group page's count both count with this, so they cannot disagree.
pub fn full_member_count(roles: impl IntoIterator<Item = kourier_model::MemberRole>) -> usize {
    use kourier_model::MemberRole;
    roles
        .into_iter()
        .filter(|r| matches!(r, MemberRole::Member | MemberRole::Admin))
        .count()
}

/// "6 members": the group's size as the header and the group page say it.
pub fn members_line(count: u64) -> String {
    trnc(
        "@info:status subtitle of a group chat, %1 how many members",
        "%1 member",
        "%1 members",
        count,
    )
}

/// Whether the header of a conversation of this kind opens a details panel (`ChatHeader::has_details`).
pub fn kind_has_details(kind: ConvKind) -> bool {
    matches!(kind, ConvKind::Direct | ConvKind::Group)
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
        members_line(full_member_count(c.members.iter().map(|m| m.role)) as u64)
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
        peer: c.key.direct_peer().cloned(),
        has_details: kind_has_details(c.kind),
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
        timer_held: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;

    #[test]
    fn every_network_has_a_name_and_a_badge_file() {
        let icons = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../kourier-qt/icons");
        for kind in NetworkKind::ALL {
            assert!(!network_name(kind).is_empty(), "{kind} has no name");
            let file = icons.join(format!("{}.svg", network_icon(kind)));
            assert!(
                file.is_file(),
                "{kind}: the badge {} is missing",
                file.display()
            );
        }
        assert_eq!(network_name(NetworkKind::Telegram), "Telegram");
        assert_eq!(network_icon(NetworkKind::Telegram), "telegram");
    }
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
    fn the_pinned_block_goes_account_by_account_and_the_share_order_stays() {
        let mut rows = vec![row(1, Some(0), 1), row(2, Some(5), 1), row(3, Some(1), 1)];
        // ids 1 and 3 belong to the second account of the panel, 2 to the first.
        rows[0].account_order = 1;
        rows[1].account_order = 0;
        rows[2].account_order = 1;
        sort_rows(&mut rows);
        assert_eq!(rows.iter().map(|r| r.id.0).collect::<Vec<_>>(), [2, 1, 3]);
        // The share picker's order knows no account: the pin position alone.
        assert_eq!(
            compare_order(
                (Some(5), 1, ConversationId(2)),
                (Some(0), 1, ConversationId(1))
            ),
            Ordering::Greater
        );
    }

    #[test]
    fn places_count_per_network() {
        use NetworkKind::{Signal, Threema};
        assert_eq!(
            network_places([Signal, Threema, Signal, Signal]),
            [1, 0, 2, 3]
        );
        assert_eq!(network_places([Signal]), [0]);
    }

    #[test]
    fn while_the_hidden_set_is_unknown_every_account_counts_as_shown() {
        let accounts = || {
            (1..=2).map(|n| TableAccount {
                id: AccountId(n),
                network: NetworkKind::Signal,
                name: format!("A{n}"),
            })
        };
        let table = AccountTable::new(accounts(), &Hidden::Unknown, false);
        let mut r = row(1, None, 1);
        r.account = AccountId(1);
        table.decorate(&mut r);
        assert_eq!(r.account_badge, "A1");
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
    fn filter_by_hidden_account_and_query() {
        let mut a = row(1, None, 1);
        a.title = "Olena Kovalenko".into();
        a.account = AccountId(1);
        let mut b = row(2, None, 2);
        b.title = "FPV Squad".into();
        b.preview = "props for olena".into();
        b.account = AccountId(2);
        let u1 = AccountUuid(uuid::Uuid::from_u128(1));
        let accounts = [
            (AccountId(1), u1),
            (AccountId(2), AccountUuid(uuid::Uuid::from_u128(2))),
        ];
        let f = ConversationFilter {
            query: "OLENA".into(),
            ..ConversationFilter::default()
        };
        let none = f.resolve(accounts, true);
        assert_eq!(f.apply_resolved([a.clone(), b.clone()], &none).len(), 2);
        // Without a query the hidden account's chats are left out ...
        let f = ConversationFilter {
            hidden: BTreeSet::from([u1]),
            ..ConversationFilter::default()
        };
        let hidden = f.resolve(accounts, true);
        assert_eq!(hidden, Hidden::Ids(BTreeSet::from([AccountId(1)])));
        let out = f.apply_resolved([a.clone(), b.clone()], &hidden);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, ConversationId(2));
        // ... and a search finds them all the same.
        let f = ConversationFilter {
            hidden: BTreeSet::from([u1]),
            query: "OLENA".into(),
            ..ConversationFilter::default()
        };
        let hidden = f.resolve(accounts, true);
        assert_eq!(f.apply_resolved([a, b], &hidden).len(), 2);
    }

    /// S-P: a protected chat is found by its title only, never by the text of its preview (nor by
    /// the placeholder that stands there).
    #[test]
    fn a_search_finds_a_protected_chat_by_its_title_only() {
        let mut a = row(1, None, 1);
        a.title = "Olena Kovalenko".into();
        let mut b = row(2, None, 2);
        b.title = "FPV Squad".into();
        b.preview = "props for olena".into();
        b.protected = true;
        let search = |q: &str, rows: [&ConversationRow; 2]| {
            ConversationFilter {
                query: q.into(),
                ..ConversationFilter::default()
            }
            .apply_resolved(rows.map(Clone::clone), &Hidden::default())
            .iter()
            .map(|r| r.id.0)
            .collect::<Vec<_>>()
        };
        assert_eq!(
            search("olena", [&a, &b]),
            [1],
            "not by what the protected chat holds"
        );
        assert_eq!(search("squad", [&a, &b]), [2], "by its title");
        b.protected = false;
        assert_eq!(
            search("olena", [&a, &b]).len(),
            2,
            "the same chat unprotected: by its text too"
        );
    }

    /// S-P: the row of a protected chat (the fixture's builder) shows the placeholder in place
    /// of its last message and of its draft, with no author, icon, receipt or time.
    #[test]
    fn a_protected_chat_s_row_shows_the_placeholder() {
        let fx = fixture::Fixture::build().expect("fixture");
        let names = NameMap::new();
        let time = TimeContext::new(fx.now_ms, &FixedOffset(0));
        let (c, last) = fx
            .conversations
            .iter()
            .find_map(|c| {
                fx.messages
                    .get(&c.id)
                    .and_then(|v| v.last())
                    .map(|m| (c, m))
            })
            .expect("a chat with a message");
        let mut c = c.clone();
        c.draft = Some(kourier_model::Text::plain("my unsent secret"));
        let build = |c: &Conversation| {
            conversation_row(
                &ConversationInput {
                    conversation: c,
                    last: Some(last),
                    typing: &[],
                    network: NetworkKind::Signal,
                },
                &names,
                &time,
            )
        };
        let open = build(&c);
        assert_eq!(open.preview_source, PreviewSource::Draft);
        assert!(!open.time_label.is_empty());
        c.protected = true;
        let r = build(&c);
        assert!(r.protected);
        assert_eq!(r.preview, protected_preview());
        assert_eq!(r.preview, "Protected chat");
        assert_eq!(
            (
                r.preview_author.as_str(),
                r.preview_source,
                r.preview_icon.as_str(),
                r.time_label.as_str(),
                r.receipt
            ),
            ("", PreviewSource::None, "", "", Receipt::None)
        );
    }

    /// S-P review L8: with NO draft, the last message of a protected chat is the user's own
    /// picture, which the unprotected row shows with its media icon, "You" and the receipt: the
    /// protected row shows none of them (the fixture builder, and anything else that hands the
    /// builder a message, does not depend on the core having dropped the preview).
    #[test]
    fn a_protected_chat_s_row_hides_an_own_picture_without_a_draft() {
        let fx = fixture::Fixture::build().expect("fixture");
        let names = NameMap::new();
        let time = TimeContext::new(fx.now_ms, &FixedOffset(0));
        let mut c = fx
            .conversations
            .iter()
            .find(|c| !c.is_self && c.draft.is_none())
            .expect("a chat without a draft")
            .clone();
        let picture = fixture::message(
            kourier_model::MessageId(9_000),
            c.id,
            kourier_model::Origin::OwnThisDevice,
            fx.now_ms - 60_000,
            kourier_model::Body::Media {
                attachments: vec![kourier_model::Attachment {
                    kind: kourier_model::AttachmentKind::Image,
                    mime: "image/jpeg".into(),
                    ..kourier_model::Attachment::default()
                }],
                caption: None,
            },
        );
        let build = |c: &Conversation| {
            conversation_row(
                &ConversationInput {
                    conversation: c,
                    last: Some(&picture),
                    typing: &[],
                    network: NetworkKind::Signal,
                },
                &names,
                &time,
            )
        };
        let open = build(&c);
        assert_eq!(open.preview_source, PreviewSource::Own);
        assert!(!open.preview_icon.is_empty(), "the media icon");
        assert_ne!(open.receipt, Receipt::None, "the receipt of an own message");
        assert!(!open.time_label.is_empty());
        c.protected = true;
        let r = build(&c);
        assert_eq!(r.preview, "Protected chat");
        assert_eq!(
            (
                r.preview_author.as_str(),
                r.preview_source,
                r.preview_icon.as_str(),
                r.time_label.as_str(),
                r.receipt
            ),
            ("", PreviewSource::None, "", "", Receipt::None)
        );
    }

    #[test]
    fn a_filter_debug_prints_counts_only() {
        let f = ConversationFilter {
            hidden: BTreeSet::from([AccountUuid(uuid::Uuid::from_u128(0xabcd_1234))]),
            query: "secret".into(),
            archived: false,
        };
        let d = format!("{f:?}");
        assert!(d.contains("hidden: 1"), "{d}");
        assert!(!d.contains("abcd") && !d.contains("secret"), "{d}");
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
