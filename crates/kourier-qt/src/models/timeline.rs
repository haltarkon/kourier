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
use kourier_viewmodel::timeline::{Block, ImageBlock, ReactionChip, RowActions};
use kourier_viewmodel::{ChatHeader, MessageRow, Paging, RevisionList, RowKey};

use super::{Store, list_model_glue, role_hash, role_index, row_at};
use crate::bridge::qobject::{self, App};
use crate::shared::Shared;

/// Role names, in role-index order.
pub const ROLES: [&str; 22] = [
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
    pub(crate) is_self: bool,
    pub(crate) muted: bool,
    pub(crate) avatar_url: QString,
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
        Block::Album(items) => map(vec![
            ty,
            ("items", list(items.iter().map(image_variant))),
            (
                "count",
                QVariant::from(&i32::try_from(items.len()).unwrap_or(i32::MAX)),
            ),
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
        ("retry", QVariant::from(&a.retry)),
        ("askAttachments", QVariant::from(&a.ask_attachments)),
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
            _ => QVariant::from(&r.wait_timer),
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
        let secs_of = |v: u32| i32::try_from(v).unwrap_or(i32::MAX);
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
