//! `EmojiModel` and `EmojiLists` (architecture §8.3): what the emoji pickers show.
//!
//! `EmojiModel` lists the emoji of one CLDR group or the hits of a search as positions in the
//! static table of `kourier_viewmodel::emoji`; its two roles are computed from the table when QML
//! asks. A picker that is closed sets the group to -1 and the query to "" and the rows are gone.
//!
//! `EmojiLists` holds the one local list of pins, favourites and recently used emoji (every
//! account, never sent to a network) and is the ONE writer of `emoji-pins`, `emoji-favourites` and
//! `emoji-recent` (through `DesktopHandle::update_prefs`; the Settings page lists none of them).

use std::pin::Pin;
use std::time::Duration;

use cxx_qt::{CxxQtType, Threading};
use cxx_qt_lib::{QHash, QHashPair_i32_QByteArray, QModelIndex, QString, QStringList, QVariant};
use kourier_app::desktop::DesktopHandle;
use kourier_app::prefs::Prefs;
use kourier_viewmodel::emoji::{self, Lang, lists};

use super::{role_hash, role_index};
use crate::bridge::qobject;

const ROLES: [&str; 2] = ["glyph", "name"];

/// How long after the last change the lists are written.
const WRITE_AFTER: Duration = Duration::from_millis(1500);

/// Rust side of `EmojiModel`.
pub struct EmojiModelRust {
    pub(crate) count: i32,
    pub(crate) query: QString,
    pub(crate) group: i32,
    /// Positions in the table, in the order shown.
    pub(crate) rows: Vec<u16>,
    pub(crate) lang: Lang,
}

impl Default for EmojiModelRust {
    fn default() -> Self {
        Self {
            count: 0,
            query: QString::default(),
            group: -1,
            rows: Vec::new(),
            lang: Lang::En,
        }
    }
}

impl qobject::EmojiModel {
    /// `QAbstractItemModel::data`.
    pub fn data(&self, index: &QModelIndex, role: i32) -> QVariant {
        let (Ok(row), Some(role)) = (usize::try_from(index.row()), role_index(role, ROLES.len()))
        else {
            return QVariant::default();
        };
        let Some(&at) = self.rows.get(row) else {
            return QVariant::default();
        };
        let at = usize::from(at);
        match role {
            0 => QVariant::from(&QString::from(emoji::glyph(at))),
            _ => QVariant::from(&QString::from(emoji::name(at, self.lang))),
        }
    }

    /// `QAbstractItemModel::rowCount`.
    pub fn row_count(&self, parent: &QModelIndex) -> i32 {
        if parent.is_valid() {
            0
        } else {
            i32::try_from(self.rows.len()).unwrap_or(i32::MAX)
        }
    }

    /// `QAbstractItemModel::roleNames`.
    pub fn role_names(&self) -> QHash<QHashPair_i32_QByteArray> {
        role_hash(&ROLES)
    }

    /// The search text changed.
    pub fn set_query_text(mut self: Pin<&mut Self>, text: &QString) {
        if self.query == *text {
            return;
        }
        self.as_mut().rust_mut().query = text.clone();
        self.as_mut().query_changed();
        self.rebuild();
    }

    /// The shown group changed.
    pub fn set_group_index(mut self: Pin<&mut Self>, group: i32) {
        if self.group == group {
            return;
        }
        self.as_mut().rust_mut().group = group;
        self.as_mut().group_changed();
        self.rebuild();
    }

    /// The rows again from the query and the group: a search when text stands, else the group.
    fn rebuild(mut self: Pin<&mut Self>) {
        let lang = Lang::ui();
        let query = self.query.to_string();
        let rows: Vec<u16> = if !query.trim().is_empty() {
            emoji::search(&query, lang)
        } else if let Ok(g) = usize::try_from(self.group) {
            emoji::group_range(g)
                .filter_map(|i| u16::try_from(i).ok())
                .collect()
        } else {
            Vec::new()
        };
        // SAFETY: a begin is paired with its end below, on this thread.
        unsafe { self.as_mut().begin_reset_model() };
        {
            let mut rust = self.as_mut().rust_mut();
            rust.lang = lang;
            rust.rows = rows;
            // Nothing is kept for a closed picker.
            if rust.rows.is_empty() {
                rust.rows.shrink_to_fit();
            }
        }
        // SAFETY: pairs the begin above.
        unsafe { self.as_mut().end_reset_model() };
        let n = i32::try_from(self.rows.len()).unwrap_or(i32::MAX);
        self.set_count(n);
    }

    /// The glyph of row `row`.
    pub fn glyph_at(&self, row: i32) -> QString {
        usize::try_from(row)
            .ok()
            .and_then(|r| self.rows.get(r))
            .map(|&at| QString::from(emoji::glyph(usize::from(at))))
            .unwrap_or_default()
    }

    /// The interface-language name of `glyph`.
    pub fn name_of(&self, glyph: &QString) -> QString {
        let glyph = glyph.to_string();
        emoji::index_of(&glyph)
            .map(|i| QString::from(emoji::name(i, Lang::ui())))
            .unwrap_or_default()
    }

    /// How many groups the table has.
    pub fn group_count(&self) -> i32 {
        i32::try_from(emoji::group_count()).unwrap_or(0)
    }

    /// The Unicode name of group `group`.
    pub fn group_key(&self, group: i32) -> QString {
        usize::try_from(group)
            .map(|g| QString::from(emoji::group_key(g)))
            .unwrap_or_default()
    }

    /// The first emoji of group `group`: its tab's icon.
    pub fn group_icon(&self, group: i32) -> QString {
        usize::try_from(group)
            .map(|g| QString::from(emoji::glyph(emoji::group_range(g).start)))
            .unwrap_or_default()
    }

    /// Whether `glyph` is an emoji of the table.
    pub fn is_known(&self, glyph: &QString) -> bool {
        emoji::is_known(&glyph.to_string())
    }
}

/// Rust side of `EmojiLists`.
pub struct EmojiListsRust {
    pub(crate) pins: QStringList,
    /// The first pins: the one-click reactions of a message's hover bar.
    pub(crate) hover_pins: QStringList,
    pub(crate) favourites: QStringList,
    pub(crate) recent: QStringList,
    pins_v: Vec<String>,
    favourites_v: Vec<String>,
    recent_v: Vec<String>,
    desktop: Option<DesktopHandle>,
    /// The app's runtime: it waits out the quiet time before a write.
    runtime: Option<tokio::runtime::Handle>,
    /// Counts the changes; a write waits for the last one's quiet time.
    generation: u64,
    /// Changes not yet written.
    dirty: bool,
}

impl Default for EmojiListsRust {
    fn default() -> Self {
        Self::from_prefs(&Prefs::default(), None)
    }
}

fn list(v: &[String]) -> QStringList {
    let mut out = QStringList::default();
    for e in v {
        out.append(QString::from(e.as_str()));
    }
    out
}

impl EmojiListsRust {
    /// The lists of `prefs`, cut to the table's emoji (the pins are Signal's six when none are stored).
    pub(crate) fn from_prefs(prefs: &Prefs, desktop: Option<DesktopHandle>) -> Self {
        let pins_v = match &prefs.emoji_pins {
            Some(p) => lists::clean(p, lists::PINS_MAX),
            None => lists::default_pins(),
        };
        let favourites_v = lists::clean(&prefs.emoji_favourites, lists::FAVOURITES_MAX);
        let recent_v = lists::clean(&prefs.emoji_recent, lists::RECENT_MAX);
        Self {
            pins: list(&pins_v),
            hover_pins: list(lists::hover_pins(&pins_v)),
            favourites: list(&favourites_v),
            recent: list(&recent_v),
            pins_v,
            favourites_v,
            recent_v,
            desktop,
            runtime: None,
            generation: 0,
            dirty: false,
        }
    }
}

impl qobject::EmojiLists {
    /// The lists changed in memory: tell QML and write after the quiet time.
    fn touched(mut self: Pin<&mut Self>) {
        {
            let mut rust = self.as_mut().rust_mut();
            rust.pins = list(&rust.pins_v);
            rust.hover_pins = list(lists::hover_pins(&rust.pins_v));
            rust.favourites = list(&rust.favourites_v);
            rust.recent = list(&rust.recent_v);
            rust.dirty = true;
            rust.generation += 1;
        }
        self.as_mut().changed();
        let Some(runtime) = self.runtime.clone() else {
            return;
        };
        let generation = self.generation;
        let thread = self.qt_thread();
        // On the app's runtime, never a thread of its own: each new thread can cost glibc a
        // malloc arena that is kept for the life of the process (RAM2: 61 arenas, 534 MiB, on
        // his laptop after a day).
        runtime.spawn(async move {
            tokio::time::sleep(WRITE_AFTER).await;
            let _ = thread.queue(move |mut lists| {
                if lists.generation == generation {
                    lists.as_mut().flush();
                }
            });
        });
    }

    /// An emoji was picked or sent as a reaction.
    pub fn used(mut self: Pin<&mut Self>, emoji: &QString) {
        let emoji = emoji.to_string();
        let changed = lists::touch_recent(&mut self.as_mut().rust_mut().recent_v, &emoji);
        if changed {
            self.touched();
        }
    }

    /// Pins or unpins `emoji`.
    pub fn toggle_pin(mut self: Pin<&mut Self>, emoji: &QString) -> bool {
        let emoji = emoji.to_string();
        let changed = lists::toggle_pin(&mut self.as_mut().rust_mut().pins_v, &emoji);
        if changed {
            self.as_mut().touched();
            // A pin is a choice, not a use: it is written at once.
            self.flush();
        }
        changed
    }

    /// Stars or unstars `emoji`.
    pub fn toggle_favourite(mut self: Pin<&mut Self>, emoji: &QString) -> bool {
        let emoji = emoji.to_string();
        let changed = lists::toggle_favourite(&mut self.as_mut().rust_mut().favourites_v, &emoji);
        if changed {
            self.as_mut().touched();
            self.flush();
        }
        changed
    }

    /// Writes the lists to the preferences: the pins are stored only when they differ from Signal's six.
    pub fn flush(mut self: Pin<&mut Self>) {
        if !self.dirty {
            return;
        }
        self.as_mut().rust_mut().dirty = false;
        let Some(desktop) = self.desktop.clone() else {
            return;
        };
        let pins = self.pins_v.clone();
        let favourites = self.favourites_v.clone();
        let recent = self.recent_v.clone();
        desktop.update_prefs(move |p| {
            p.emoji_pins = (pins != lists::default_pins()).then_some(pins);
            p.emoji_favourites = favourites;
            p.emoji_recent = recent;
        });
    }
}

impl qobject::EmojiLists {
    /// Gives the lists their preferences and the desktop handle that saves them (App's start).
    pub(crate) fn attach(
        mut self: Pin<&mut Self>,
        prefs: &Prefs,
        desktop: DesktopHandle,
        runtime: tokio::runtime::Handle,
    ) {
        let mut rust = EmojiListsRust::from_prefs(prefs, Some(desktop));
        rust.runtime = Some(runtime);
        *self.as_mut().rust_mut() = rust;
    }
}
