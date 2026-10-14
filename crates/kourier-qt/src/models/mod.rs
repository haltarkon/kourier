//! Rust list models (§8.1, §8.4): rows of prebuilt Qt values, updated with `ListOp`s from the
//! viewmodel's diff engine through one safe [`apply`] that pairs every `begin*` with its `end*`.

pub mod accounts;
pub mod attachments;
pub mod conversations;
pub mod timeline;

use std::pin::Pin;

use cxx_qt_lib::{QByteArray, QHash, QHashPair_i32_QByteArray, QModelIndex, QVector};
use kourier_viewmodel::diff::{self, ListOp, RoleSet};

/// First custom role id (`Qt::UserRole + 1`).
pub const FIRST_ROLE: i32 = 257;

/// Rows of one model: viewmodel rows (diff input) and their prebuilt Qt form.
pub struct Store<V, Q> {
    /// Viewmodel rows, in model order.
    pub view: Vec<V>,
    /// Qt rows, parallel to `view`.
    pub qt: Vec<Q>,
    /// Generation of the applied rows; older results are dropped (§8.5).
    pub generation: u64,
    /// How often [`set_rows`] had to fall back to a reset because the ops did not reproduce
    /// the rows (a diff or adapter bug; tests assert it stays 0).
    pub resyncs: u64,
}

impl<V, Q> Default for Store<V, Q> {
    fn default() -> Self {
        Self {
            view: Vec::new(),
            qt: Vec::new(),
            generation: 0,
            resyncs: 0,
        }
    }
}

/// Role names of a model as `roleNames()` returns them.
pub fn role_hash(names: &[&str]) -> QHash<QHashPair_i32_QByteArray> {
    let mut roles = QHash::<QHashPair_i32_QByteArray>::default();
    for (i, name) in names.iter().enumerate() {
        roles.insert(role_id(i), QByteArray::from(*name));
    }
    roles
}

/// Role id of role index `i`.
pub fn role_id(i: usize) -> i32 {
    FIRST_ROLE + i32::try_from(i).unwrap_or(i32::MAX - FIRST_ROLE)
}

/// Role index of a role id, if it is one of ours.
pub fn role_index(role: i32, count: usize) -> Option<usize> {
    let i = usize::try_from(role.checked_sub(FIRST_ROLE)?).ok()?;
    (i < count).then_some(i)
}

fn as_i32(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// What [`apply`] needs from a list model. Implemented by `list_model_glue!` on top of the
/// inherited `QAbstractItemModel` methods.
pub trait ListModel {
    /// Viewmodel row type.
    type View: Clone + PartialEq;
    /// Prebuilt Qt row type.
    type Qt;
    /// Diff identity of a row (unique within the model).
    type Key: Eq + std::hash::Hash + Clone;

    /// The row store.
    fn store(&self) -> &Store<Self::View, Self::Qt>;
    /// The row store, mutably.
    fn store_mut(self: Pin<&mut Self>) -> &mut Store<Self::View, Self::Qt>;
    /// Builds the Qt form of a row.
    fn build(view: &Self::View) -> Self::Qt;
    /// Which roles differ between two versions of a row.
    fn changed_roles(old: &Self::View, new: &Self::View) -> RoleSet;
    /// Diff identity of a row.
    fn key(view: &Self::View) -> Self::Key;
    /// Updates the `count` property.
    fn update_count(self: Pin<&mut Self>, count: i32);

    /// `beginInsertRows`.
    ///
    /// # Safety
    ///
    /// Must be paired with [`Self::end_insert`].
    unsafe fn begin_insert(self: Pin<&mut Self>, first: i32, last: i32);
    /// `endInsertRows`.
    ///
    /// # Safety
    ///
    /// Must follow [`Self::begin_insert`].
    unsafe fn end_insert(self: Pin<&mut Self>);
    /// `beginRemoveRows`.
    ///
    /// # Safety
    ///
    /// Must be paired with [`Self::end_remove`].
    unsafe fn begin_remove(self: Pin<&mut Self>, first: i32, last: i32);
    /// `endRemoveRows`.
    ///
    /// # Safety
    ///
    /// Must follow [`Self::begin_remove`].
    unsafe fn end_remove(self: Pin<&mut Self>);
    /// `beginMoveRows` of one row; `dest` uses Qt's "before the move" convention.
    ///
    /// # Safety
    ///
    /// When it returns `true`, must be paired with [`Self::end_move`].
    unsafe fn begin_move(self: Pin<&mut Self>, from: i32, dest: i32) -> bool;
    /// `endMoveRows`.
    ///
    /// # Safety
    ///
    /// Must follow a successful [`Self::begin_move`].
    unsafe fn end_move(self: Pin<&mut Self>);
    /// `beginResetModel`.
    ///
    /// # Safety
    ///
    /// Must be paired with [`Self::end_reset`].
    unsafe fn begin_reset(self: Pin<&mut Self>);
    /// `endResetModel`.
    ///
    /// # Safety
    ///
    /// Must follow [`Self::begin_reset`].
    unsafe fn end_reset(self: Pin<&mut Self>);
    /// Emits `dataChanged` for one row.
    fn emit_changed(self: Pin<&mut Self>, row: i32, roles: &QVector<i32>);
}

/// Replaces the model's rows with `new` (generation `generation`): diffs against the current
/// rows and applies the ops with paired `begin*/end*` calls. Returns `false` (and changes
/// nothing) when `generation` is older than the rows already shown.
pub fn set_rows<M: ListModel>(mut model: Pin<&mut M>, generation: u64, new: Vec<M::View>) -> bool {
    if generation < model.store().generation {
        return false;
    }
    let ops = diff::diff_or_reset(&model.store().view, &new, M::key, M::changed_roles);
    apply(model.as_mut(), &ops, &new);
    let same_keys = {
        let view = &model.store().view;
        view.len() == new.len() && view.iter().zip(&new).all(|(a, b)| M::key(a) == M::key(b))
    };
    if same_keys {
        // Rows whose differences touch no role (a sort key, a pin position, an author id)
        // get no `Change` op: take them over silently, nothing visible changed.
        let store = model.as_mut().store_mut();
        for (have, want) in store.view.iter_mut().zip(&new) {
            if have != want {
                *have = want.clone();
            }
        }
    } else {
        // The ops must reproduce the row order; if Qt refused one (a bug), resynchronise with
        // a reset rather than show rows that differ from the data.
        eprintln!("kourier: list diff did not reproduce the rows; resetting the model");
        apply(model.as_mut(), &[ListOp::Reset], &new);
        model.as_mut().store_mut().resyncs += 1;
    }
    let store = model.as_mut().store_mut();
    store.generation = generation;
    let count = as_i32(store.qt.len());
    model.update_count(count);
    true
}

/// Replaces row `at` with `new` where the model's roles say it differs (the row's diff
/// identity must be the same): the Qt row is built again and only the roles that changed are
/// announced. For the one-row updates the per-second tick makes; `false` when the row is not
/// there, is another one, or did not change.
pub fn patch_row<M: ListModel>(mut model: Pin<&mut M>, at: usize, new: M::View) -> bool {
    let store = model.store();
    let Some(old) = store.view.get(at) else {
        return false;
    };
    if M::key(old) != M::key(&new) || *old == new {
        return false;
    }
    let roles = M::changed_roles(old, &new);
    let store = model.as_mut().store_mut();
    store.qt[at] = M::build(&new);
    store.view[at] = new;
    let mut ids = QVector::<i32>::default();
    for i in roles.iter() {
        ids.append(role_id(i as usize));
    }
    model.emit_changed(as_i32(at), &ids);
    true
}

/// Applies `ops` (from [`diff::diff`] against the current rows) taking rows from `new`.
pub fn apply<M: ListModel>(mut model: Pin<&mut M>, ops: &[ListOp], new: &[M::View]) {
    for op in ops {
        match *op {
            ListOp::Reset => {
                // SAFETY: begin/end are paired around the storage swap.
                unsafe { model.as_mut().begin_reset() };
                let store = model.as_mut().store_mut();
                store.view = new.to_vec();
                store.qt = new.iter().map(M::build).collect();
                unsafe { model.as_mut().end_reset() };
            }
            ListOp::Insert { at, n } => {
                if n == 0 || at + n > new.len() || at > model.store().qt.len() {
                    continue;
                }
                // SAFETY: paired with end_insert below.
                unsafe { model.as_mut().begin_insert(as_i32(at), as_i32(at + n - 1)) };
                let store = model.as_mut().store_mut();
                for (off, v) in new[at..at + n].iter().enumerate() {
                    store.view.insert(at + off, v.clone());
                    store.qt.insert(at + off, M::build(v));
                }
                unsafe { model.as_mut().end_insert() };
            }
            ListOp::Remove { at, n } => {
                if n == 0 || at + n > model.store().qt.len() {
                    continue;
                }
                // SAFETY: paired with end_remove below.
                unsafe { model.as_mut().begin_remove(as_i32(at), as_i32(at + n - 1)) };
                let store = model.as_mut().store_mut();
                store.view.drain(at..at + n);
                store.qt.drain(at..at + n);
                unsafe { model.as_mut().end_remove() };
            }
            ListOp::Move { from, to } => {
                let len = model.store().qt.len();
                if from == to || from >= len || to >= len {
                    continue;
                }
                // Qt wants the destination index before the row is taken out.
                let dest = if to > from { to + 1 } else { to };
                // SAFETY: end_move runs only when begin_move accepted the move.
                if unsafe { model.as_mut().begin_move(as_i32(from), as_i32(dest)) } {
                    let store = model.as_mut().store_mut();
                    let v = store.view.remove(from);
                    store.view.insert(to, v);
                    let q = store.qt.remove(from);
                    store.qt.insert(to, q);
                    unsafe { model.as_mut().end_move() };
                }
            }
            ListOp::Change { at, roles } => {
                if at >= model.store().qt.len() || at >= new.len() {
                    continue;
                }
                let store = model.as_mut().store_mut();
                store.view[at] = new[at].clone();
                store.qt[at] = M::build(&new[at]);
                let mut ids = QVector::<i32>::default();
                for i in roles.iter() {
                    ids.append(role_id(i as usize));
                }
                model.as_mut().emit_changed(as_i32(at), &ids);
            }
        }
    }
}

/// Implements [`ListModel`] for a bridge type whose Rust struct has a `store` field.
macro_rules! list_model_glue {
    ($qobject:ty, $view:ty, $qt:ty, $key:ty) => {
        impl $crate::models::ListModel for $qobject {
            type View = $view;
            type Qt = $qt;
            type Key = $key;

            fn store(&self) -> &$crate::models::Store<$view, $qt> {
                &self.store
            }

            fn store_mut(
                self: ::std::pin::Pin<&mut Self>,
            ) -> &mut $crate::models::Store<$view, $qt> {
                &mut ::cxx_qt::CxxQtType::rust_mut(self).get_mut().store
            }

            fn build(view: &$view) -> $qt {
                <$qt>::from_view(view)
            }

            fn changed_roles(old: &$view, new: &$view) -> kourier_viewmodel::diff::RoleSet {
                <$qt>::changed_roles(old, new)
            }

            fn key(view: &$view) -> $key {
                <$qt>::key(view)
            }

            fn update_count(self: ::std::pin::Pin<&mut Self>, count: i32) {
                if *self.count() != count {
                    <$qobject>::set_count(self, count);
                }
            }

            unsafe fn begin_insert(self: ::std::pin::Pin<&mut Self>, first: i32, last: i32) {
                unsafe {
                    self.begin_insert_rows(&::cxx_qt_lib::QModelIndex::default(), first, last)
                }
            }

            unsafe fn end_insert(self: ::std::pin::Pin<&mut Self>) {
                unsafe { self.end_insert_rows() }
            }

            unsafe fn begin_remove(self: ::std::pin::Pin<&mut Self>, first: i32, last: i32) {
                unsafe {
                    self.begin_remove_rows(&::cxx_qt_lib::QModelIndex::default(), first, last)
                }
            }

            unsafe fn end_remove(self: ::std::pin::Pin<&mut Self>) {
                unsafe { self.end_remove_rows() }
            }

            unsafe fn begin_move(self: ::std::pin::Pin<&mut Self>, from: i32, dest: i32) -> bool {
                let root = ::cxx_qt_lib::QModelIndex::default();
                unsafe { self.begin_move_rows(&root, from, from, &root, dest) }
            }

            unsafe fn end_move(self: ::std::pin::Pin<&mut Self>) {
                unsafe { self.end_move_rows() }
            }

            unsafe fn begin_reset(self: ::std::pin::Pin<&mut Self>) {
                unsafe { self.begin_reset_model() }
            }

            unsafe fn end_reset(self: ::std::pin::Pin<&mut Self>) {
                unsafe { self.end_reset_model() }
            }

            fn emit_changed(
                self: ::std::pin::Pin<&mut Self>,
                row: i32,
                roles: &::cxx_qt_lib::QVector<i32>,
            ) {
                let index = self.index(row, 0, &::cxx_qt_lib::QModelIndex::default());
                self.data_changed(&index, &index, roles);
            }
        }
    };
}
pub(crate) use list_model_glue;

/// Row access shared by the `data()` overrides.
pub fn row_at<'a, V, Q>(store: &'a Store<V, Q>, index: &QModelIndex) -> Option<&'a Q> {
    usize::try_from(index.row())
        .ok()
        .and_then(|r| store.qt.get(r))
}
