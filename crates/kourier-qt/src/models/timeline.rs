//! `TimelineModel`: the open conversation's header and timeline rows (row 0 = newest, for a
//! `ListView` with `verticalLayoutDirection: ListView.BottomToTop`).

use std::pin::Pin;

use cxx_qt::{CxxQtThread, CxxQtType};
use cxx_qt_lib::{
    QHash, QHashPair_i32_QByteArray, QList, QMap, QMapPair_QString_QVariant, QModelIndex, QString,
    QVariant,
};
use kourier_model::MessageId;
use kourier_viewmodel::diff::RoleSet;
use kourier_viewmodel::media_layout::AlbumRow;
use kourier_viewmodel::timeline::{Block, ImageBlock, ReactionChip, RowActions, UploadView};
use kourier_viewmodel::{ChatHeader, ChatOffer, MessageRow, Paging, RevisionList, RowKey};

use super::{Store, list_model_glue, role_hash, role_index, row_at};
use crate::bridge::qobject::{self, App};
use crate::shared::Shared;

/// Role names, in role-index order.
pub const ROLES: [&str; 24] = [
    "kind",
    "messageId",
    "own",
    "author",
    "avatarUrl",
    "showAuthor",
    "showAvatar",
    "groupStart",
    "groupEnd",
    "timeLabel",
    "blocks",
    "reactions",
    "edited",
    "deleted",
    "receipt",
    "text",
    "icon",
    "actions",
    "expiry",
    "tone",
    "wait",
    "waitTimer",
    // The author's user id (a group's sender; empty for a notice and where none is known): a
    // click on the name or the picture opens that person (P3).
    "authorId",
    // The pages the row's menu offers (the sender's, the mentioned people's): [{user, label}].
    "pages",
];

/// Rust side of `TimelineModel`.
#[derive(Default)]
pub struct TimelineRust {
    pub(crate) count: i32,
    pub(crate) conversation_id: i64,
    pub(crate) title: QString,
    pub(crate) subtitle: QString,
    pub(crate) typing: bool,
    pub(crate) is_group: bool,
    /// The header opens a details panel (a 1:1 chat, Note to Self, a group; not a channel).
    pub(crate) has_details: bool,
    pub(crate) is_self: bool,
    pub(crate) muted: bool,
    pub(crate) avatar_url: QString,
    /// The color of the chat's account (`#rrggbb`), for what is drawn behind the messages.
    pub(crate) tint: QString,
    pub(crate) cannot_send: QString,
    pub(crate) attach_kinds: QString,
    pub(crate) timer_label: QString,
    pub(crate) timer_secs: i32,
    pub(crate) timer_options: QVariant,
    /// The range of custom timers in seconds (`timer_custom_max` 0: none).
    pub(crate) timer_custom_min: i32,
    pub(crate) timer_custom_max: i32,
    pub(crate) timer_custom_step: i32,
    /// Deleting here is told to the account's other devices.
    pub(crate) deletes_sync: bool,
    /// Why the chat cannot be cleared or deleted now (its history still arrives); empty
    /// when it can.
    pub(crate) still_filling: QString,
    /// Why the timer cannot be changed now (the account is under a send hold); empty when it can.
    pub(crate) timer_held: QString,
    /// What the chat's menu offers of Archive, Pin, Mute and Mark as Unread
    /// ([`offer_variant`]); an empty map while no chat is open.
    pub(crate) chat_offer: QVariant,
    /// The offer behind `chat_offer` (to tell when it changes); `None` while no chat is open.
    offered: Option<ChatOffer>,
    /// The timers behind `timer_options` (to tell when they change).
    timer_offered: Vec<kourier_viewmodel::conversations::TimerOption>,
    /// Whether the header of `conversation_id` is loaded. While it is not, the properties
    /// above hold their defaults (`timer_secs` 0 reads as "off"), which say nothing about
    /// the conversation.
    header_loaded: bool,
    pub(crate) has_older: bool,
    pub(crate) has_newer: bool,
    pub(crate) loading: bool,
    pub(crate) store: Store<MessageRow, MessageQt>,
    /// Where "Read more" goes (set by `App`; `None` in a bare model).
    pub(crate) shared: Option<Shared>,
    pub(crate) app: Option<Box<CxxQtThread<App>>>,
}

/// One timeline row as Qt values.
pub struct MessageQt {
    kind: QString,
    message_id: i64,
    own: bool,
    author: QString,
    avatar_url: QString,
    show_author: bool,
    show_avatar: bool,
    group_start: bool,
    group_end: bool,
    time_label: QString,
    blocks: QVariant,
    reactions: QVariant,
    edited: bool,
    deleted: bool,
    receipt: QString,
    text: QString,
    icon: QString,
    actions: QVariant,
    expiry: QString,
    tone: QString,
    wait: QString,
    wait_timer: QString,
    author_id: QString,
    pages: QVariant,
}

fn s(v: &str) -> QVariant {
    QVariant::from(&QString::from(v))
}

fn map(entries: Vec<(&str, QVariant)>) -> QVariant {
    let mut m = QMap::<QMapPair_QString_QVariant>::default();
    for (k, v) in entries {
        m.insert(QString::from(k), v);
    }
    QVariant::from(&m)
}

fn list(items: impl IntoIterator<Item = QVariant>) -> QVariant {
    let mut l = QList::<QVariant>::default();
    for v in items {
        l.append(v);
    }
    QVariant::from(&l)
}

/// What a chat's menu offers (`ChatOffer`) as the map `TimelineModel.chatOffer` and
/// `App.chatOffer(id)` hold: `shown on archived pinned pinAtLimit pinWhy markedUnread hasUnread
/// muteOffered muted mutedLabel why`. QML shows the flags; it decides none of them. The texts are
/// the app's own words (no contact's text is in them).
pub fn offer_variant(o: &ChatOffer) -> QVariant {
    map(vec![
        ("shown", QVariant::from(&o.shown)),
        ("on", QVariant::from(&o.on)),
        ("archived", QVariant::from(&o.archived)),
        ("pinned", QVariant::from(&o.pinned)),
        ("pinAtLimit", QVariant::from(&o.pin_at_limit)),
        ("pinWhy", s(&o.pin_why)),
        ("markedUnread", QVariant::from(&o.marked_unread)),
        ("hasUnread", QVariant::from(&o.has_unread)),
        ("muteOffered", QVariant::from(&o.mute_offered)),
        ("muted", QVariant::from(&o.muted)),
        ("mutedLabel", s(&o.muted_label)),
        ("why", s(&o.why)),
    ])
}

/// The value of `TimelineModel.chatOffer` while no chat is open: an empty map (QML reads
/// `undefined` for every key, so nothing is shown), never an invalid variant.
pub fn no_offer() -> QVariant {
    map(Vec::new())
}

/// A picture or video as a `QVariantMap`: an image block, or one tile of an album.
fn image_variant(i: &ImageBlock) -> QVariant {
    map(vec![
        ("type", s("image")),
        (
            "attachment",
            QVariant::from(&i.attachment.map_or(0, |a| a.0)),
        ),
        ("kind", s(&format!("{:?}", i.kind).to_ascii_lowercase())),
        ("url", s(&i.url)),
        ("fullUrl", s(&i.full_url)),
        ("placeholderUrl", s(&i.placeholder_url)),
        ("animated", QVariant::from(&i.animated)),
        ("media", QVariant::from(&i.media)),
        ("width", QVariant::from(&i.width)),
        ("height", QVariant::from(&i.height)),
        ("blurhash", s(&i.blurhash)),
        ("state", s(i.state.as_str())),
        ("progress", QVariant::from(&f64::from(i.progress))),
        ("label", s(&i.label)),
        ("sticker", QVariant::from(&i.sticker)),
        ("ask", QVariant::from(&i.ask)),
        ("save", QVariant::from(&i.save)),
        ("uploading", QVariant::from(&i.upload.is_some())),
        ("uploadShare", QVariant::from(&upload_share(&i.upload))),
        ("uploadLabel", s(upload_label(&i.upload))),
        ("uploadCancel", QVariant::from(&upload_cancel(&i.upload))),
    ])
}

/// How much of the file is up (0 … 1) as the ring draws it; 0 without a ring.
fn upload_share(u: &Option<UploadView>) -> f64 {
    u.as_ref().map_or(0.0, |u| f64::from(u.share))
}

/// The percentage over the ring ("45%"); empty without a ring and for a file that waits.
fn upload_label(u: &Option<UploadView>) -> &str {
    u.as_ref().map_or("", |u| u.label.as_str())
}

/// Whether the X on the ring cancels the sending now.
fn upload_cancel(u: &Option<UploadView>) -> bool {
    u.as_ref().is_some_and(|u| u.cancel)
}

/// One row of an album as a `QVariantMap`: `count` tiles, `aspect` (the sum of the tiles'
/// aspects: the row is `width / aspect` tall) and `prefix`, the share of the row's width at
/// which each tile begins (`media_layout`; the pixel step is `AlbumBlock.qml`'s).
fn album_row_variant(row: &AlbumRow) -> QVariant {
    map(vec![
        (
            "count",
            QVariant::from(&i32::try_from(row.count).unwrap_or(i32::MAX)),
        ),
        ("aspect", QVariant::from(&row.aspect)),
        ("prefix", list(row.prefix.iter().map(QVariant::from))),
    ])
}

/// What the picture viewer steps through for one message: every picture and video of its
/// row, in order, each with `attachment`, `url` (full size; empty while it is not downloaded),
/// `animated`, `media`, `kind`, `state`, `progress` and `placeholder`. Empty for a row
/// without pictures.
pub fn viewer_items(row: &MessageRow) -> QVariant {
    list(row.blocks.iter().flat_map(Block::images).map(|i| {
        let local = i.state.as_str() == "local";
        map(vec![
            (
                "attachment",
                QVariant::from(&i.attachment.map_or(0, |a| a.0)),
            ),
            ("url", s(&i.full_url)),
            ("animated", QVariant::from(&(i.animated && local))),
            ("media", QVariant::from(&i.media)),
            ("kind", s(&format!("{:?}", i.kind).to_ascii_lowercase())),
            ("state", s(i.state.as_str())),
            ("progress", QVariant::from(&f64::from(i.progress))),
            ("placeholder", s(&i.placeholder_url)),
            ("ask", QVariant::from(&i.ask)),
            ("save", QVariant::from(&i.save)),
        ])
    }))
}

/// What the viewer steps through when there is nothing: an empty list (never an invalid value).
pub fn no_viewer_items() -> QVariant {
    list(std::iter::empty())
}

/// What the picture viewer steps through for one message of a GALLERY slot: every picture of the
/// message the slot holds, in the slot's order, each with `attachment`, `url` (full size; empty
/// while it is not downloaded), `animated`, `media`, `kind`, `state`, `progress`, `placeholder`
/// and `ask` (the same shape as [`viewer_items`], so the viewer does not know the difference).
/// Empty when the slot holds none of them.
pub fn gallery_viewer_items(
    rows: &[kourier_viewmodel::chat_media::MediaRow],
    message: kourier_model::MessageId,
) -> QVariant {
    list(rows.iter().filter(|r| r.message == message).map(|r| {
        let local = r.state.as_str() == "local";
        map(vec![
            ("attachment", QVariant::from(&r.attachment.0)),
            ("url", s(&r.full_url)),
            ("animated", QVariant::from(&(r.animated && local))),
            ("media", QVariant::from(&r.media)),
            ("kind", s(kourier_viewmodel::media::kind_name(r.kind))),
            ("state", s(r.state.as_str())),
            ("progress", QVariant::from(&f64::from(r.progress))),
            ("placeholder", s(&r.placeholder_url)),
            ("ask", QVariant::from(&false)),
            ("save", QVariant::from(&r.save)),
        ])
    }))
}

/// The versions of a message as the history dialog lists them (newest first): `when`,
/// `original`, `current`, and the text as a text block (`html`, `plain`, `spoilers`); `empty`
/// for a version without a caption.
pub fn revisions_variant(revisions: &RevisionList) -> QVariant {
    list(revisions.rows.iter().map(|r| {
        let text = r.text.clone().unwrap_or_default();
        map(vec![
            ("when", s(&r.when)),
            ("original", QVariant::from(&r.original)),
            ("current", QVariant::from(&r.current)),
            ("empty", QVariant::from(&r.text.is_none())),
            ("text", block_variant(&Block::Text(text))),
        ])
    }))
}

/// Whether `new` shows ANOTHER number than `held`, the last view of the level that HAD a number
/// (the caller holds it through answers without one, V3): the same person, both keys known, and
/// different (U2: their key changed under the open level). The first answer of a level (nothing
/// held), a person without a number and a change of state alone are none.
pub fn safety_swapped(
    held: Option<&kourier_viewmodel::safety::SafetyView>,
    new: &kourier_viewmodel::safety::SafetyView,
) -> bool {
    held.is_some_and(|h| {
        h.account == new.account
            && h.user == new.user
            && matches!((&h.key, &new.key), (Some(before), Some(now)) if before != now)
    })
}

/// The Qt side's memory of the number the level showed (V3): the last view that HAD a number,
/// kept through answers without one (the account stopped, a query failed) and forgotten only with
/// the level. [`SafetySeen::observe`] says whether an answer is another number than that one.
#[derive(Default)]
pub struct SafetySeen(Option<kourier_viewmodel::safety::SafetyView>);

impl SafetySeen {
    /// Whether `new` is ANOTHER number than the last one seen ([`safety_swapped`]); a view with a
    /// number becomes the one seen.
    pub fn observe(&mut self, new: &kourier_viewmodel::safety::SafetyView) -> bool {
        let swapped = safety_swapped(self.0.as_ref(), new);
        if new.key.is_some() {
            self.0 = Some(new.clone());
        }
        swapped
    }

    /// The level is gone: the next number is a first one.
    pub fn forget(&mut self) {
        self.0 = None;
    }
}

/// What the Qt side holds of the safety-number level (P5a U11, V3, Y4): whether a level is open (asked for and not
/// closed), the view on screen (a button acts on ITS person and key, never on a newer answer QML has not shown yet)
/// and the number the level last showed. An answer that comes while no level is open (the source's pending view
/// taken in the same drain that closed the level) is dropped, so "no level on screen, no held view" holds always.
#[derive(Default)]
pub struct SafetyLevel {
    open: bool,
    shown: Option<kourier_viewmodel::safety::SafetyView>,
    seen: SafetySeen,
}

impl SafetyLevel {
    /// A level is asked for: nothing of an earlier one is held, and answers are taken from now on.
    pub fn begin(&mut self) {
        self.end();
        self.open = true;
    }

    /// The level is gone (closed, or about to be asked for again): no view is held, answers are dropped.
    pub fn end(&mut self) {
        self.open = false;
        self.shown = None;
        self.seen.forget();
    }

    /// An answer of the source: the map to show and the view now held, or `None` when no level is open (the answer
    /// is dropped and nothing is held).
    pub fn receive(&mut self, view: kourier_viewmodel::safety::SafetyView) -> Option<QVariant> {
        if !self.open {
            return None;
        }
        let swapped = self.seen.observe(&view);
        let map = safety_variant(&view, swapped);
        self.shown = Some(view);
        Some(map)
    }

    /// What a press of the button `action` acts on: the person and the KEY of the number on screen, only when that
    /// number has a key and its buttons offer `action` (U11).
    pub fn target(
        &self,
        action: kourier_viewmodel::safety::SafetyAction,
    ) -> Option<(
        kourier_model::AccountId,
        kourier_model::UserId,
        kourier_model::IdentityFingerprint,
    )> {
        let view = self.shown.as_ref()?;
        let key = view.key.clone()?;
        view.buttons
            .iter()
            .any(|b| b.action == action)
            .then(|| (view.account, view.user.clone(), key))
    }
}

/// The safety-number level (P5a) as the map `App.safety` holds. The scannable bytes go over as
/// a `QByteArray` (the QR code's content), never as a string (critic M6). The key is NOT in the
/// map: Rust holds it (`App::safety_action`). `swapped`: this answer is another number than the
/// one on screen ([`safety_swapped`]); the level then does not move the keyboard onto a button.
pub fn safety_variant(v: &kourier_viewmodel::safety::SafetyView, swapped: bool) -> QVariant {
    map(vec![
        ("account", QVariant::from(&v.account.0)),
        ("user", s(v.user.as_str())),
        ("title", s(&v.title)),
        ("accessibleTitle", s(&v.accessible_title)),
        ("groups", list(v.groups.iter().map(|g| s(g)))),
        ("accessibleDigits", s(&v.accessible_digits)),
        (
            "scannable",
            QVariant::from(&cxx_qt_lib::QByteArray::from(v.scannable.as_slice())),
        ),
        ("hasNumber", QVariant::from(&v.key.is_some())),
        ("hint", s(&v.hint)),
        ("stateLine", s(&v.state_line)),
        (
            "buttons",
            list(v.buttons.iter().map(|b| {
                map(vec![
                    ("action", s(b.action.name())),
                    ("label", s(&b.label)),
                    ("primary", QVariant::from(&b.primary)),
                ])
            })),
        ),
        ("reason", s(&v.reason)),
        ("notice", s(&v.notice)),
        ("swapped", QVariant::from(&swapped)),
    ])
}

/// The person's page for the details panel (`PersonView`) as a `QVariantMap`. Texts are plain
/// text: the panel sets `textFormat: Text.PlainText` on every label that shows one; the one
/// exception is the about text's HTML, which `richtext` made (escaped, whitelisted).
pub fn person_variant(v: &kourier_viewmodel::person::PersonView) -> QVariant {
    map(vec![
        ("account", QVariant::from(&v.account.0)),
        ("user", s(v.user.as_str())),
        ("isSelf", QVariant::from(&v.is_self)),
        ("name", s(&v.name)),
        ("profileName", s(&v.profile_name)),
        ("avatarUrl", s(&v.avatar_url)),
        ("about", s(&v.about)),
        ("aboutEmoji", s(&v.about_emoji)),
        // What the page shows of it, made by `richtext` (escaped HTML; a link around each address
        // only in an accepted chat), and the addresses of those links for the keyboard's menu.
        ("aboutHtml", s(&v.about_html)),
        ("aboutLinks", QVariant::from(&v.about_links)),
        ("aboutHrefs", list(v.about_hrefs.iter().map(|h| s(h)))),
        ("number", s(&v.number)),
        ("notes", list(v.notes.iter().map(|n| s(n)))),
        ("chat", QVariant::from(&v.chat.map_or(0, |c| c.0))),
        (
            "timerSecs",
            QVariant::from(&i32::try_from(v.timer_secs).unwrap_or(i32::MAX)),
        ),
        ("timer", s(&v.timer)),
        (
            "timers",
            list(v.timers.iter().map(|t| {
                map(vec![
                    (
                        "secs",
                        QVariant::from(&i32::try_from(t.secs).unwrap_or(i32::MAX)),
                    ),
                    ("label", s(&t.label)),
                ])
            })),
        ),
        ("timerCustom", QVariant::from(&v.timer_range.is_some())),
        // The range of "Custom Time…" of her chat (the page names its own chat's range, never
        // the open chat's).
        (
            "timerMin",
            QVariant::from(&secs_of(v.timer_range.map_or(0, |r| r.min))),
        ),
        (
            "timerMax",
            QVariant::from(&secs_of(v.timer_range.map_or(0, |r| r.max))),
        ),
        (
            "timerStep",
            QVariant::from(&secs_of(v.timer_range.map_or(1, |r| r.step))),
        ),
        // Why the last change of her chat's timer failed (F4): a line under the selector.
        ("timerFailed", s(&v.timer_failed)),
        (
            "groups",
            list(v.groups.iter().map(|g| {
                map(vec![
                    ("id", QVariant::from(&g.id.0)),
                    ("title", s(&g.title)),
                    ("avatarUrl", s(&g.avatar_url)),
                ])
            })),
        ),
        (
            "groupsPreview",
            QVariant::from(&i32::try_from(v.groups_preview).unwrap_or(i32::MAX)),
        ),
        ("groupsMore", s(&v.groups_more)),
        ("safetyShown", QVariant::from(&v.safety_shown)),
        // P5b: whether "Copy Link" shows and why it is disabled; the link itself stays in Rust.
        ("linkShown", QVariant::from(&v.link_shown)),
        ("linkNote", s(&v.link_note)),
        // P5c: whether "Share Contact…" shows and why it is disabled; the card is the core's.
        ("shareShown", QVariant::from(&v.share_shown)),
        ("shareNote", s(&v.share_note)),
        // P5c, SD9: the picker's line about what the card holds (the core's card, worded by the viewmodel).
        ("shareCard", s(&v.share_card)),
        ("safetyState", s(&v.safety_state)),
    ])
}

/// Seconds as the `int` QML numbers use.
fn secs_of(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

/// The group's page for the details panel (`GroupView`, without its member rows: its `member_count`
/// says how many there are) as a `QVariantMap`. Texts are plain
/// text: the panel sets `textFormat: Text.PlainText` on every label that shows one; the one
/// exception is the description's HTML, which `richtext` made (escaped, whitelisted).
pub fn group_variant(v: &kourier_viewmodel::group::GroupView) -> QVariant {
    map(vec![
        ("account", QVariant::from(&v.account.0)),
        ("id", QVariant::from(&v.id.0)),
        ("name", s(&v.name)),
        ("avatarUrl", s(&v.avatar_url)),
        ("description", s(&v.description)),
        // The description as `richtext` made it (escaped HTML; a link around each address only in
        // an accepted chat), and the addresses of those links for the keyboard's menu.
        ("descriptionHtml", s(&v.description_html)),
        ("descriptionLinks", QVariant::from(&v.description_links)),
        (
            "descriptionHrefs",
            list(v.description_hrefs.iter().map(|h| s(h))),
        ),
        ("notes", list(v.notes.iter().map(|n| s(n)))),
        ("timerSecs", QVariant::from(&secs_of(v.timer_secs))),
        ("timer", s(&v.timer_label)),
        (
            "timers",
            list(v.timers.iter().map(|t| {
                map(vec![
                    ("secs", QVariant::from(&secs_of(t.secs))),
                    ("label", s(&t.label)),
                ])
            })),
        ),
        ("timerCustom", QVariant::from(&v.timer_range.is_some())),
        (
            "timerMin",
            QVariant::from(&secs_of(v.timer_range.map_or(0, |r| r.min))),
        ),
        (
            "timerMax",
            QVariant::from(&secs_of(v.timer_range.map_or(0, |r| r.max))),
        ),
        (
            "timerStep",
            QVariant::from(&secs_of(v.timer_range.map_or(1, |r| r.step))),
        ),
        ("timerNote", s(&v.timer_note)),
        // Why the last change of the group's timer failed (F4): a line under the selector.
        ("timerFailed", s(&v.timer_failed)),
        ("membersTitle", s(&v.members_title)),
        ("invited", s(&v.invited)),
        ("requests", s(&v.requests)),
        // What the page may do with the invite link; the link itself stays in Rust.
        ("invites", s(&v.invites)),
        ("inviteOn", QVariant::from(&v.invite_on)),
        ("adminLabel", s(&v.admin_label)),
        // The rows themselves are the two member models' (`App.groupMembers`, `App.groupPreview`).
        (
            "memberCount",
            QVariant::from(&i32::try_from(v.member_count).unwrap_or(i32::MAX)),
        ),
        (
            "membersPreview",
            QVariant::from(&i32::try_from(v.members_preview).unwrap_or(i32::MAX)),
        ),
        ("membersMore", s(&v.members_more)),
        ("rules", list(v.rules.iter().map(|r| s(r)))),
    ])
}

/// One content block as a `QVariantMap` with a `type` key (see `timeline/blocks/*.qml`).
pub fn block_variant(b: &Block) -> QVariant {
    let ty = ("type", s(b.type_name()));
    match b {
        // `html` and `spoilers` are the part that is shown; `plain` is always the whole text
        // (the menu's Copy, the row's accessible name).
        Block::Text(t) => map(vec![
            ty,
            ("html", s(&t.html)),
            ("plain", s(t.full_plain())),
            ("links", list(t.links.iter().map(|l| s(l)))),
            ("jumbo", QVariant::from(&t.jumbo)),
            ("more", QVariant::from(&t.more)),
            (
                "moreWords",
                QVariant::from(&i32::try_from(t.more_words).unwrap_or(i32::MAX)),
            ),
            (
                "spoilers",
                list(t.spoilers.iter().map(|sp| {
                    map(vec![
                        ("start", QVariant::from(&sp.start)),
                        ("len", QVariant::from(&sp.len)),
                    ])
                })),
            ),
            // The people a `kourier:mention/<i>` link of `html` opens (user ids, no names).
            (
                "mentions",
                list(
                    t.mentions
                        .iter()
                        .zip(&t.mention_spoilers)
                        .map(|(u, hidden)| {
                            map(vec![
                                ("user", s(u.as_str())),
                                ("spoiler", QVariant::from(hidden)),
                            ])
                        }),
                ),
            ),
        ]),
        Block::Quote(q) => map(vec![
            ty,
            ("author", s(&q.author)),
            ("text", s(&q.text)),
            ("icon", s(&q.icon)),
            ("own", QVariant::from(&q.own)),
            ("target", QVariant::from(&q.target.map_or(-1, |m| m.0))),
            ("state", s(q.state.as_str())),
            ("thumbUrl", s(&q.thumb_url)),
            ("standIn", QVariant::from(&q.stand_in)),
        ]),
        Block::Image(i) => image_variant(i),
        Block::Album(album) => map(vec![
            ty,
            ("items", list(album.items.iter().map(image_variant))),
            (
                "count",
                QVariant::from(&i32::try_from(album.items.len()).unwrap_or(i32::MAX)),
            ),
            ("rows", list(album.rows.iter().map(album_row_variant))),
            // The ring of the "+N" tile: the pictures behind it together.
            ("moreUploading", QVariant::from(&album.more.is_some())),
            ("moreShare", QVariant::from(&upload_share(&album.more))),
            ("moreLabel", s(upload_label(&album.more))),
            ("moreCancel", QVariant::from(&upload_cancel(&album.more))),
        ]),
        Block::File(f) => map(vec![
            ty,
            (
                "attachment",
                QVariant::from(&f.attachment.map_or(0, |a| a.0)),
            ),
            ("media", QVariant::from(&f.media)),
            ("name", s(&f.name)),
            ("detail", s(&f.detail)),
            ("icon", s(&f.icon)),
            ("state", s(f.state.as_str())),
            ("progress", QVariant::from(&f64::from(f.progress))),
            ("voice", QVariant::from(&f.voice)),
            ("ask", QVariant::from(&f.ask)),
            ("save", QVariant::from(&f.save)),
            ("uploading", QVariant::from(&f.upload.is_some())),
            ("uploadShare", QVariant::from(&upload_share(&f.upload))),
            ("uploadLabel", s(upload_label(&f.upload))),
            ("uploadCancel", QVariant::from(&upload_cancel(&f.upload))),
        ]),
        Block::Placeholder(p) => map(vec![ty, ("text", s(&p.text)), ("icon", s(&p.icon))]),
    }
}

fn reaction_variant(r: &ReactionChip) -> QVariant {
    map(vec![
        ("emoji", s(&r.emoji)),
        ("count", QVariant::from(&r.count)),
        ("mine", QVariant::from(&r.mine)),
        ("names", list(r.names.iter().map(|n| s(n)))),
        (
            "ownAt",
            QVariant::from(&r.own_at.and_then(|i| i32::try_from(i).ok()).unwrap_or(-1)),
        ),
    ])
}

/// What the row offers, as the context menu and the hover toolbar read it.
fn actions_variant(a: &RowActions) -> QVariant {
    map(vec![
        ("reply", QVariant::from(&a.reply)),
        ("react", QVariant::from(&a.react)),
        ("edit", QVariant::from(&a.edit)),
        ("copy", QVariant::from(&a.copy)),
        ("deleteForMe", QVariant::from(&a.delete_for_me)),
        ("deleteForEveryone", QVariant::from(&a.delete_for_everyone)),
        ("deleteAttachment", QVariant::from(&a.delete_attachment)),
        ("takeBack", QVariant::from(&a.take_back)),
        ("cancelSending", QVariant::from(&a.cancel_sending)),
        ("retry", QVariant::from(&a.retry)),
        ("askAttachments", QVariant::from(&a.ask_attachments)),
        ("saveAttachments", QVariant::from(&a.save_attachments)),
        ("showSender", QVariant::from(&a.show_sender)),
        ("showPeople", QVariant::from(&a.show_people)),
        ("any", QVariant::from(&a.any())),
    ])
}

impl MessageQt {
    fn from_view(v: &MessageRow) -> Self {
        Self {
            kind: QString::from(v.kind.as_str()),
            message_id: v.message_id().map_or(-1, |m| m.0),
            own: v.own,
            author: QString::from(v.author.as_str()),
            avatar_url: QString::from(v.avatar_url.as_str()),
            show_author: v.show_author,
            show_avatar: v.show_avatar,
            group_start: v.group_start,
            group_end: v.group_end,
            time_label: QString::from(v.time_label.as_str()),
            blocks: list(v.blocks.iter().map(block_variant)),
            reactions: list(v.reactions.iter().map(reaction_variant)),
            edited: v.edited,
            deleted: v.deleted,
            receipt: QString::from(v.receipt.as_str()),
            text: QString::from(v.text.as_str()),
            icon: QString::from(v.icon.as_str()),
            actions: actions_variant(&v.actions),
            expiry: QString::from(v.expiry.as_str()),
            tone: QString::from(v.tone.as_str()),
            wait: QString::from(v.wait.as_str()),
            wait_timer: QString::from(v.wait_timer.as_str()),
            author_id: QString::from(v.author_id.as_ref().map_or("", |u| u.as_str())),
            pages: list(v.pages.iter().map(|p| {
                map(vec![
                    ("user", s(p.user.as_str())),
                    ("label", s(&p.label)),
                    ("safety", QVariant::from(&p.safety)),
                ])
            })),
        }
    }

    fn changed_roles(a: &MessageRow, b: &MessageRow) -> RoleSet {
        RoleSet::EMPTY
            .with_if(0, a.kind != b.kind)
            .with_if(2, a.own != b.own)
            .with_if(3, a.author != b.author)
            .with_if(4, a.avatar_url != b.avatar_url)
            .with_if(5, a.show_author != b.show_author)
            .with_if(6, a.show_avatar != b.show_avatar)
            .with_if(7, a.group_start != b.group_start)
            .with_if(8, a.group_end != b.group_end)
            .with_if(9, a.time_label != b.time_label)
            .with_if(10, a.blocks != b.blocks)
            .with_if(11, a.reactions != b.reactions)
            .with_if(12, a.edited != b.edited)
            .with_if(13, a.deleted != b.deleted)
            .with_if(14, a.receipt != b.receipt)
            .with_if(15, a.text != b.text)
            .with_if(16, a.icon != b.icon)
            .with_if(17, a.actions != b.actions)
            .with_if(18, a.expiry != b.expiry)
            .with_if(19, a.tone != b.tone)
            .with_if(20, a.wait != b.wait)
            .with_if(21, a.wait_timer != b.wait_timer)
            .with_if(22, a.author_id != b.author_id)
            .with_if(23, a.pages != b.pages)
    }

    fn key(v: &MessageRow) -> RowKey {
        v.key
    }
}

impl qobject::TimelineModel {
    /// `QAbstractItemModel::data`.
    pub fn data(&self, index: &QModelIndex, role: i32) -> QVariant {
        let (Some(r), Some(role)) = (row_at(&self.store, index), role_index(role, ROLES.len()))
        else {
            return QVariant::default();
        };
        match role {
            0 => QVariant::from(&r.kind),
            1 => QVariant::from(&r.message_id),
            2 => QVariant::from(&r.own),
            3 => QVariant::from(&r.author),
            4 => QVariant::from(&r.avatar_url),
            5 => QVariant::from(&r.show_author),
            6 => QVariant::from(&r.show_avatar),
            7 => QVariant::from(&r.group_start),
            8 => QVariant::from(&r.group_end),
            9 => QVariant::from(&r.time_label),
            10 => r.blocks.clone(),
            11 => r.reactions.clone(),
            12 => QVariant::from(&r.edited),
            13 => QVariant::from(&r.deleted),
            14 => QVariant::from(&r.receipt),
            15 => QVariant::from(&r.text),
            16 => QVariant::from(&r.icon),
            17 => r.actions.clone(),
            18 => QVariant::from(&r.expiry),
            19 => QVariant::from(&r.tone),
            20 => QVariant::from(&r.wait),
            21 => QVariant::from(&r.wait_timer),
            22 => QVariant::from(&r.author_id),
            _ => r.pages.clone(),
        }
    }

    /// `QAbstractItemModel::rowCount`.
    pub fn row_count(&self, parent: &QModelIndex) -> i32 {
        if parent.is_valid() {
            0
        } else {
            i32::try_from(self.store.qt.len()).unwrap_or(i32::MAX)
        }
    }

    /// `QAbstractItemModel::roleNames`.
    pub fn role_names(&self) -> QHash<QHashPair_i32_QByteArray> {
        role_hash(&ROLES)
    }

    /// The viewmodel rows currently shown (tests).
    pub fn rows(&self) -> &[MessageRow] {
        &self.store.view
    }

    /// How many pictures, videos and files of the rows shown are downloading.
    pub fn downloading_attachments(&self) -> i32 {
        i32::try_from(kourier_viewmodel::timeline::downloading(&self.store.view))
            .unwrap_or(i32::MAX)
    }

    /// How many upload rings of the rows shown have a figure (a `--screenshot` of the upload
    /// pages waits for one).
    pub fn uploads_reported(&self) -> i32 {
        i32::try_from(kourier_viewmodel::timeline::uploads_reported(
            &self.store.view,
        ))
        .unwrap_or(i32::MAX)
    }

    /// Whether the newest own message of the rows shown has left the outbox (the `media` page's
    /// picture waits for it).
    pub fn newest_own_left_the_outbox(&self) -> bool {
        kourier_viewmodel::timeline::newest_own_left_the_outbox(&self.store.view)
    }

    /// "Read more" on a long text: the message shows one step more of it. Its row is updated in
    /// place (the source says what changed; `App` applies it after this call returned).
    pub fn read_more(self: Pin<&mut Self>, message: i64) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        if message <= 0 {
            return;
        }
        let changes = shared.borrow_mut().source.read_more(MessageId(message));
        crate::controllers::defer(&shared, self.app.as_deref(), changes);
    }

    /// The row index of a message, when it is among the rows.
    pub fn row_of(&self, message: kourier_model::MessageId) -> Option<i32> {
        self.store
            .view
            .iter()
            .position(|r| r.key == RowKey::Message(message))
            .and_then(|i| i32::try_from(i).ok())
    }

    /// Whether the open conversation's header is loaded: only then do `timer_secs` and the
    /// other header properties describe the conversation and not their defaults.
    pub fn header_loaded(&self) -> bool {
        self.header_loaded
    }

    /// Shows conversation `id` with `header` (`None` while it loads, or with `id` 0 to clear
    /// the page). Properties change only when they differ, so bindings do not re-evaluate
    /// needlessly.
    pub fn set_header(mut self: Pin<&mut Self>, id: i64, header: Option<&ChatHeader>) {
        let header = header.filter(|h| h.id.0 == id);
        if self.header_loaded != header.is_some() {
            self.as_mut().rust_mut().header_loaded = header.is_some();
        }
        let text = |f: fn(&ChatHeader) -> &str| QString::from(header.map_or("", f));
        if *self.conversation_id() != id {
            self.as_mut().set_conversation_id(id);
        }
        let title = text(|h| &h.title);
        if *self.title() != title {
            self.as_mut().set_title(title);
        }
        let subtitle = text(|h| &h.subtitle);
        if *self.subtitle() != subtitle {
            self.as_mut().set_subtitle(subtitle);
        }
        let avatar = text(|h| &h.avatar_url);
        if *self.avatar_url() != avatar {
            self.as_mut().set_avatar_url(avatar);
        }
        let filling = text(|h| &h.still_filling);
        if *self.still_filling() != filling {
            self.as_mut().set_still_filling(filling);
        }
        let timer_held = text(|h| &h.timer_held);
        if *self.timer_held() != timer_held {
            self.as_mut().set_timer_held(timer_held);
        }
        let tint = text(|h| &h.tint);
        if *self.tint() != tint {
            self.as_mut().set_tint(tint);
        }
        // The offer is compared as the Rust value: the map is built (and QML's bindings
        // re-evaluated) only when it changed, also the first time (an unset property is an
        // invalid variant, which QML must never read).
        let offer = header.map(|h| h.offer.clone());
        if self.offered != offer || !self.chat_offer().is_valid() {
            let shown = offer.as_ref().map_or_else(no_offer, offer_variant);
            self.as_mut().rust_mut().offered = offer;
            self.as_mut().set_chat_offer(shown);
        }
        let cannot = text(|h| &h.cannot_send);
        if *self.cannot_send() != cannot {
            self.as_mut().set_cannot_send(cannot);
        }
        let kinds = QString::from(
            header
                .map(|h| h.attach.names())
                .unwrap_or_default()
                .as_str(),
        );
        if *self.attach_kinds() != kinds {
            self.as_mut().set_attach_kinds(kinds);
        }
        let timer = text(|h| &h.timer);
        if *self.timer_label() != timer {
            self.as_mut().set_timer_label(timer);
        }
        let secs = header.map_or(0, |h| i32::try_from(h.timer_secs).unwrap_or(i32::MAX));
        // The menu marks the current timer, so its entries follow the timer too.
        let options_changed =
            self.timer_offered != header.map(|h| h.timers.clone()).unwrap_or_default();
        if *self.timer_secs() != secs {
            self.as_mut().set_timer_secs(secs);
        }
        // Also the first time: QML reads a list, never an invalid variant.
        if options_changed || !self.timer_options().is_valid() {
            let timers = header.map(|h| h.timers.clone()).unwrap_or_default();
            let options = list(timers.iter().map(|t| {
                map(vec![
                    (
                        "secs",
                        QVariant::from(&i32::try_from(t.secs).unwrap_or(i32::MAX)),
                    ),
                    ("label", s(&t.label)),
                ])
            }));
            self.as_mut().rust_mut().timer_offered = timers;
            self.as_mut().set_timer_options(options);
        }
        // The custom range ("Custom Time…"): 0 as its end means there is none.
        let span = header.and_then(|h| h.timer_range);
        let (min, max, step) = span.map_or((0, 0, 1), |s| {
            (secs_of(s.min), secs_of(s.max), secs_of(s.step))
        });
        if *self.timer_custom_min() != min {
            self.as_mut().set_timer_custom_min(min);
        }
        if *self.timer_custom_step() != step {
            self.as_mut().set_timer_custom_step(step);
        }
        if *self.timer_custom_max() != max {
            self.as_mut().set_timer_custom_max(max);
        }
        let flag = |f: fn(&ChatHeader) -> bool| header.is_some_and(f);
        if *self.deletes_sync() != flag(|h| h.deletes_sync) {
            self.as_mut().set_deletes_sync(flag(|h| h.deletes_sync));
        }
        if *self.typing() != flag(|h| h.typing) {
            self.as_mut().set_typing(flag(|h| h.typing));
        }
        if *self.is_group() != flag(|h| h.is_group) {
            self.as_mut().set_is_group(flag(|h| h.is_group));
        }
        if *self.has_details() != flag(|h| h.has_details) {
            self.as_mut().set_has_details(flag(|h| h.has_details));
        }
        if *self.is_self() != flag(|h| h.is_self) {
            self.as_mut().set_is_self(flag(|h| h.is_self));
        }
        if *self.muted() != flag(|h| h.muted) {
            self.as_mut().set_muted(flag(|h| h.muted));
        }
    }

    /// Shows the paging state of the window.
    pub fn set_paging(mut self: Pin<&mut Self>, paging: Paging) {
        if *self.has_older() != paging.has_older {
            self.as_mut().set_has_older(paging.has_older);
        }
        if *self.has_newer() != paging.has_newer {
            self.as_mut().set_has_newer(paging.has_newer);
        }
        if *self.loading() != paging.loading {
            self.as_mut().set_loading(paging.loading);
        }
    }
}

list_model_glue!(qobject::TimelineModel, MessageRow, MessageQt, RowKey);
