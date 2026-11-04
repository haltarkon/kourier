//! The choice of which accounts' chats the list shows (blended chats): a Qt-free model every
//! front-end shares. The rules of the choice live here, not in the front-end: the click and the
//! toggle, "All Chats", the setting that turns blending off, the first complete answer of the
//! accounts as the baseline, an account that appears, the prune of accounts that are gone, the
//! carry of the choice over "Transfer History from Phone…", and the normalisation of a saved
//! choice. The front-end feeds it the accounts ([`ShownAccounts::observe`]), saves what the
//! outcome says and refreshes.
//!
//! The state is the set of HIDDEN accounts' uuids, not of the shown ones: a new account then
//! joins the list with no extra rule, and a stale uuid harms nothing. Every operation takes
//! the visible accounts' rows in panel order.

use std::collections::BTreeSet;

use kourier_model::{AccountId, AccountUuid};

use crate::conversations::AccountRow;

/// What an operation did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// The choice (or the blend mode) changed: the front-end refreshes the list, the title,
    /// the banner and the panel.
    pub changed: bool,
    /// The set of hidden accounts changed: the front-end saves it.
    pub save: bool,
    /// A toggle was refused because the account is the last one shown ("At least one account
    /// stays in the list"); nothing changed.
    pub last_stays: bool,
}

impl Outcome {
    fn saved() -> Self {
        Self {
            changed: true,
            save: true,
            last_stays: false,
        }
    }

    /// Adds what `other` did to this outcome.
    pub fn merge(&mut self, other: Self) {
        self.changed |= other.changed;
        self.save |= other.save;
        self.last_stays |= other.last_stays;
    }
}

/// A "Transfer History from Phone…" that is under way: the replaced account is removed and a
/// new one is linked (a NEW uuid). The choice of the old one passes to the id the replace's own
/// link gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Carry {
    /// The replaced account's uuid.
    old: AccountUuid,
    /// The replaced account's id: the source's news of the replace name it, and only the news
    /// of THIS replace end or answer it (an `Ended` of an earlier replace that is still queued
    /// must not end a later one).
    old_id: AccountId,
    /// The id the replace's login was given, once the core announced it.
    new_id: Option<AccountId>,
}

/// The choice of shown accounts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShownAccounts {
    /// Several accounts may be shown together (the setting). Off: one account, or all.
    blend: bool,
    /// The accounts whose chats the list leaves out.
    hidden: BTreeSet<AccountUuid>,
    /// The uuids of the last COMPLETE accounts answer; `None` before the first.
    baseline: Option<BTreeSet<AccountUuid>>,
    /// With ONE account, a click on it was the last choice (so it is tinted and titles the
    /// list as today, and "All Chats" is not): with one account the hidden set is empty
    /// either way.
    picked_one: bool,
    /// The replace under way.
    carry: Option<Carry>,
}

impl Default for ShownAccounts {
    fn default() -> Self {
        Self::load(true, [])
    }
}

impl ShownAccounts {
    /// The choice as the preferences saved it. The set is normalised at the first complete
    /// answer of the accounts ([`Self::observe`]): turning blending off with several
    /// shown means "All Chats" (his answer to Q1), the same at load.
    pub fn load(blend: bool, hidden: impl IntoIterator<Item = AccountUuid>) -> Self {
        Self {
            blend,
            hidden: hidden.into_iter().collect(),
            baseline: None,
            picked_one: false,
            carry: None,
        }
    }

    /// Several accounts may be shown together.
    pub fn blend(&self) -> bool {
        self.blend
    }

    /// The hidden accounts' uuids (what is saved).
    pub fn hidden(&self) -> &BTreeSet<AccountUuid> {
        &self.hidden
    }

    /// Whether the account is in the list.
    pub fn is_shown(&self, uuid: AccountUuid) -> bool {
        !self.hidden.contains(&uuid)
    }

    /// The replaced account's place counts as shown while its replace waits for the new
    /// account (1), else 0.
    fn waiting_slot(&self) -> usize {
        self.carry
            .is_some_and(|c| !self.hidden.contains(&c.old))
            .into()
    }

    /// A replace waits for the new account and the replaced one was shown.
    pub fn waiting(&self) -> bool {
        self.waiting_slot() == 1
    }

    /// How many accounts are shown (the waiting slot of a replace included).
    pub fn shown_count(&self, rows: &[AccountRow]) -> usize {
        rows.iter().filter(|r| self.is_shown(r.uuid)).count() + self.waiting_slot()
    }

    /// The one account the list shows, for the front-end's `currentAccount`, the title and
    /// the banner: exactly one shown, and either there are several accounts or a click on
    /// the only one was the last choice (with one account the panel and the title stay as
    /// before blending: a click tints the account and names the list, "All Chats" tints
    /// "All Chats").
    pub fn current(&self, rows: &[AccountRow]) -> Option<AccountId> {
        if self.waiting_slot() == 1 {
            return None;
        }
        let mut shown = rows.iter().filter(|r| self.is_shown(r.uuid));
        let first = shown.next()?;
        if shown.next().is_some() {
            return None;
        }
        (rows.len() > 1 || self.picked_one).then_some(first.id)
    }

    /// "All Chats" is the tinted entry: no account is singled out and none is hidden. Before
    /// the first complete accounts answer a saved choice that hides accounts is not "All
    /// Chats" (the list is held empty meanwhile; the title waits too).
    pub fn all_shown(&self, rows: &[AccountRow]) -> bool {
        if self.baseline.is_none() && !self.hidden.is_empty() {
            return false;
        }
        self.current(rows).is_none() && rows.iter().all(|r| self.is_shown(r.uuid))
    }

    /// Whether `uuid` is the last account shown (its box is greyed out).
    pub fn is_last_shown(&self, rows: &[AccountRow], uuid: AccountUuid) -> bool {
        self.is_shown(uuid) && self.shown_count(rows) <= 1
    }

    /// Fills `shown` and `place` of the panel's rows (the one helper the front-end calls
    /// after `UiSource::accounts`): `place` is the account's place among the visible accounts
    /// of its network in panel order (1, 2, …), `0` when it is the only one of its network.
    pub fn fill_rows(&self, rows: &mut [AccountRow]) {
        let places = crate::conversations::network_places(rows.iter().map(|r| r.network));
        for (r, place) in rows.iter_mut().zip(places) {
            r.shown = self.is_shown(r.uuid);
            r.place = place;
        }
    }

    /// A click on an account: the list shows only it. Both modes.
    pub fn click(&mut self, rows: &[AccountRow], id: AccountId) -> Outcome {
        if !rows.iter().any(|r| r.id == id) {
            return Outcome::default();
        }
        let mut want: BTreeSet<AccountUuid> =
            rows.iter().filter(|r| r.id != id).map(|r| r.uuid).collect();
        // A replace under way keeps its place hidden: the new account takes it.
        if let Some(c) = self.carry {
            want.insert(c.old);
        }
        let picked = rows.len() == 1;
        let changed = want != self.hidden || picked != self.picked_one;
        let save = want != self.hidden;
        self.hidden = want;
        self.picked_one = picked;
        Outcome {
            changed,
            save,
            last_stays: false,
        }
    }

    /// Shift+click, Ctrl+click, Ctrl+Space, the box or the menu item: adds the account to the
    /// shown ones or removes it. With blending off, or with fewer than two accounts (the blend
    /// rules start at two: M12), it is a click. The last shown account stays (`last_stays`).
    pub fn toggle(&mut self, rows: &[AccountRow], id: AccountId) -> Outcome {
        if !self.blend || rows.len() < 2 {
            return self.click(rows, id);
        }
        let Some(me) = rows.iter().find(|r| r.id == id) else {
            return Outcome::default();
        };
        if self.hidden.contains(&me.uuid) {
            self.hidden.remove(&me.uuid);
        } else {
            if self.shown_count(rows) <= 1 {
                return Outcome {
                    last_stays: true,
                    ..Outcome::default()
                };
            }
            self.hidden.insert(me.uuid);
        }
        self.picked_one = false;
        Outcome::saved()
    }

    /// "All Chats": every account shown.
    pub fn show_all(&mut self) -> Outcome {
        let save = !self.hidden.is_empty();
        let changed = save || self.picked_one;
        self.hidden.clear();
        self.picked_one = false;
        Outcome {
            changed,
            save,
            last_stays: false,
        }
    }

    /// The setting changed. Turning blending OFF with several accounts shown shows "All
    /// Chats" (his answer to Q1).
    pub fn set_blend(&mut self, rows: &[AccountRow], blend: bool) -> Outcome {
        if self.blend == blend {
            return Outcome::default();
        }
        self.blend = blend;
        let mut out = Outcome {
            changed: true,
            ..Outcome::default()
        };
        if !blend && self.shown_count(rows) > 1 {
            out.merge(self.show_all());
        }
        out
    }

    /// A "Transfer History from Phone…" of `old` was confirmed: the account goes and a new one
    /// is linked. Its choice passes to the new account ([`Self::replace_answered`] names it);
    /// meanwhile the replaced account's place counts as shown, so nothing turns into "All
    /// Chats" and the list may be empty.
    pub fn replace_begin(&mut self, rows: &[AccountRow], old: AccountId) -> Outcome {
        let mut out = Outcome::default();
        if let Some(c) = self.carry {
            out.merge(self.replace_ended(rows, c.old_id));
        }
        if let Some(r) = rows.iter().find(|r| r.id == old) {
            self.carry = Some(Carry {
                old: r.uuid,
                old_id: old,
                new_id: None,
            });
        }
        out
    }

    /// The core announced the id of the replace of `old`'s own login.
    pub fn replace_answered(&mut self, old: AccountId, new_id: AccountId) {
        if let Some(c) = self.carry.as_mut()
            && c.old_id == old
        {
            c.new_id.get_or_insert(new_id);
        }
    }

    /// The link of the replace of `old` was cancelled or failed (before or after the core
    /// announced its account): no new account comes. The replaced account's uuid is
    /// forgotten and the choice is pruned. The news of another replace changes nothing.
    pub fn replace_ended(&mut self, rows: &[AccountRow], old: AccountId) -> Outcome {
        if self.carry.is_none_or(|c| c.old_id != old) {
            return Outcome::default();
        }
        self.carry = None;
        if self.baseline.is_none() {
            return Outcome::default();
        }
        self.prune(rows)
    }

    /// The accounts the source lists now. `known`: the accounts have answered
    /// ([`crate::UiSource::accounts_known`]); before that nothing is done (the baseline, the
    /// prune and the appearance all wait for a complete answer).
    ///
    /// - The first complete answer is the BASELINE: no account "appears" then. A saved choice is
    ///   normalised with it (blending off and several shown: "All Chats").
    /// - Later, an account that was not in the last answer APPEARS: with blending on it joins
    ///   (nothing to do, it is not hidden); with blending off the list switches to it.
    /// - The new account of a replace takes the choice of the replaced one.
    /// - Accounts that are gone leave the set; when none of the accounts is shown any more, all
    ///   are (a waiting replace counts as shown).
    pub fn observe(&mut self, rows: &[AccountRow], known: bool) -> Outcome {
        if !known {
            return Outcome::default();
        }
        let mut out = Outcome::default();
        let present: BTreeSet<AccountUuid> = rows.iter().map(|r| r.uuid).collect();

        // The new account of a replace.
        if let Some(c) = self.carry
            && let Some(id) = c.new_id
            && let Some(new) = rows.iter().find(|r| r.id == id)
        {
            self.carry = None;
            if self.hidden.remove(&c.old) {
                self.hidden.insert(new.uuid);
                out.merge(Outcome::saved());
            }
            if let Some(base) = self.baseline.as_mut() {
                base.insert(new.uuid);
            }
            out.changed = true;
        }

        match self.baseline.take() {
            None => {
                out.merge(self.prune(rows));
                if !self.blend && self.shown_count(rows) > 1 {
                    out.merge(self.show_all());
                }
            }
            Some(base) => {
                let appeared = rows.iter().rfind(|r| !base.contains(&r.uuid));
                out.merge(self.prune(rows));
                if let Some(new) = appeared
                    && !self.blend
                {
                    out.merge(self.click(rows, new.id));
                }
            }
        }
        self.baseline = Some(present);
        out
    }

    /// Forgets the uuids of accounts that are gone (a waiting replace keeps its account's), and
    /// shows all when no account is shown.
    fn prune(&mut self, rows: &[AccountRow]) -> Outcome {
        let mut out = Outcome::default();
        let before = self.hidden.len();
        let carried = self.carry.map(|c| c.old);
        self.hidden
            .retain(|u| Some(*u) == carried || rows.iter().any(|r| r.uuid == *u));
        if self.hidden.len() != before {
            out.merge(Outcome::saved());
        }
        if !rows.is_empty() && self.shown_count(rows) == 0 {
            out.merge(self.show_all());
            out.changed = true;
            out.save = true;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use kourier_model::NetworkKind;

    use super::*;
    use crate::conversations::AccountStatus;

    fn uuid(n: u128) -> AccountUuid {
        AccountUuid(uuid::Uuid::from_u128(n))
    }

    fn row(id: i64, network: NetworkKind) -> AccountRow {
        AccountRow {
            id: AccountId(id),
            uuid: uuid(id as u128),
            name: format!("Account {id}"),
            network,
            network_name: String::new(),
            network_icon: String::new(),
            status: AccountStatus::Online,
            status_label: String::new(),
            unread: 0,
            avatar_url: String::new(),
            number: String::new(),
            offers_history: false,
            offers_unlink: false,
            device: String::new(),
            shown: true,
            place: 0,
        }
    }

    /// Signal 1, Signal 2, Threema 3.
    fn three() -> Vec<AccountRow> {
        vec![
            row(1, NetworkKind::Signal),
            row(2, NetworkKind::Signal),
            row(3, NetworkKind::Threema),
        ]
    }

    fn known(rows: &[AccountRow]) -> ShownAccounts {
        let mut s = ShownAccounts::default();
        s.observe(rows, true);
        s
    }

    fn shown_ids(s: &ShownAccounts, rows: &[AccountRow]) -> Vec<i64> {
        rows.iter()
            .filter(|r| s.is_shown(r.uuid))
            .map(|r| r.id.0)
            .collect()
    }

    #[test]
    fn a_click_shows_only_that_account() {
        let rows = three();
        let mut s = known(&rows);
        let out = s.click(&rows, AccountId(2));
        assert!(out.changed && out.save);
        assert_eq!(shown_ids(&s, &rows), [2]);
        assert_eq!(s.current(&rows), Some(AccountId(2)));
        assert!(!s.all_shown(&rows));
        // The same click again changes nothing (nothing to save).
        assert_eq!(s.click(&rows, AccountId(2)), Outcome::default());
        // A click on an account that is not there changes nothing.
        assert_eq!(s.click(&rows, AccountId(9)), Outcome::default());
        assert_eq!(shown_ids(&s, &rows), [2]);
    }

    #[test]
    fn all_chats_shows_every_account() {
        let rows = three();
        let mut s = known(&rows);
        s.click(&rows, AccountId(1));
        let out = s.show_all();
        assert!(out.changed && out.save);
        assert_eq!(shown_ids(&s, &rows), [1, 2, 3]);
        assert_eq!(s.current(&rows), None);
        assert!(s.all_shown(&rows));
        assert_eq!(s.show_all(), Outcome::default());
    }

    #[test]
    fn a_shift_click_adds_and_removes_an_account() {
        let rows = three();
        let mut s = known(&rows);
        s.click(&rows, AccountId(1));
        s.toggle(&rows, AccountId(3));
        assert_eq!(shown_ids(&s, &rows), [1, 3]);
        assert_eq!(s.current(&rows), None, "two shown single out none");
        assert!(!s.all_shown(&rows), "and 'All Chats' is not tinted");
        let out = s.toggle(&rows, AccountId(1));
        assert!(out.changed && out.save);
        assert_eq!(shown_ids(&s, &rows), [3]);
        assert_eq!(s.current(&rows), Some(AccountId(3)));
    }

    #[test]
    fn the_last_shown_account_stays() {
        let rows = three();
        let mut s = known(&rows);
        s.click(&rows, AccountId(2));
        let out = s.toggle(&rows, AccountId(2));
        assert!(out.last_stays && !out.changed && !out.save);
        assert_eq!(shown_ids(&s, &rows), [2]);
        assert!(s.is_last_shown(&rows, uuid(2)));
        assert!(
            !s.is_last_shown(&rows, uuid(1)),
            "a hidden one is not shown"
        );
    }

    #[test]
    fn with_blending_off_a_toggle_is_a_click() {
        let rows = three();
        let mut s = ShownAccounts::load(false, []);
        s.observe(&rows, true);
        s.click(&rows, AccountId(1));
        s.toggle(&rows, AccountId(3));
        assert_eq!(shown_ids(&s, &rows), [3], "the toggle only switched");
    }

    #[test]
    fn turning_blending_off_with_several_shown_shows_all_chats() {
        let rows = three();
        let mut s = known(&rows);
        s.click(&rows, AccountId(1));
        s.toggle(&rows, AccountId(2));
        let out = s.set_blend(&rows, false);
        assert!(out.changed && out.save);
        assert_eq!(
            shown_ids(&s, &rows),
            [1, 2, 3],
            "'All Chats', not the first"
        );
        assert!(!s.blend());
        // With ONE shown account the choice stays.
        let mut s = known(&rows);
        s.click(&rows, AccountId(2));
        let out = s.set_blend(&rows, false);
        assert!(out.changed && !out.save);
        assert_eq!(shown_ids(&s, &rows), [2]);
        // Turning it on again changes the mode only.
        let out = s.set_blend(&rows, true);
        assert!(out.changed && !out.save && s.blend());
        assert_eq!(s.set_blend(&rows, true), Outcome::default());
    }

    #[test]
    fn a_saved_choice_is_normalised_with_the_same_rule_at_load() {
        let rows = three();
        // Blending off but two shown (a hand edit, or a crash between the flip and the save).
        let mut s = ShownAccounts::load(false, [uuid(3)]);
        let out = s.observe(&rows, true);
        assert!(out.changed && out.save);
        assert!(s.hidden().is_empty(), "'All Chats'");
        // One shown stays as saved.
        let mut s = ShownAccounts::load(false, [uuid(1), uuid(3)]);
        let out = s.observe(&rows, true);
        assert!(!out.save);
        assert_eq!(shown_ids(&s, &rows), [2]);
        // With blending on, a saved choice of several is kept.
        let mut s = ShownAccounts::load(true, [uuid(3)]);
        s.observe(&rows, true);
        assert_eq!(shown_ids(&s, &rows), [1, 2]);
    }

    #[test]
    fn a_removed_account_leaves_the_choice_and_an_empty_choice_shows_all() {
        let mut rows = three();
        let mut s = ShownAccounts::load(true, [uuid(1), uuid(3)]);
        s.observe(&rows, true);
        assert_eq!(shown_ids(&s, &rows), [2]);
        // A hidden account goes: it leaves the saved set.
        rows.remove(0);
        let out = s.observe(&rows, true);
        assert!(out.save);
        assert_eq!(s.hidden().iter().copied().collect::<Vec<_>>(), [uuid(3)]);
        // The only shown account goes: all chats.
        rows.remove(0);
        let out = s.observe(&rows, true);
        assert!(out.changed && out.save);
        assert!(s.hidden().is_empty());
        assert_eq!(shown_ids(&s, &rows), [3]);
    }

    #[test]
    fn an_unlinked_account_keeps_its_choice() {
        // An unlinked (logged out) account stays in the rows, so nothing prunes it.
        let mut rows = three();
        let mut s = ShownAccounts::load(true, [uuid(2)]);
        s.observe(&rows, true);
        rows[1].status = AccountStatus::LoggedOut;
        let out = s.observe(&rows, true);
        assert!(!out.changed && !out.save);
        assert_eq!(s.hidden().iter().copied().collect::<Vec<_>>(), [uuid(2)]);
    }

    #[test]
    fn a_prune_before_the_first_accounts_answer_changes_nothing() {
        // The accounts did not answer: the rows are empty and the saved choice stays whole.
        let mut s = ShownAccounts::load(true, [uuid(1), uuid(2)]);
        let out = s.observe(&[], false);
        assert_eq!(out, Outcome::default());
        assert_eq!(s.hidden().len(), 2);
        // Even with rows in hand, an unknown answer prunes nothing and sets no baseline.
        let rows = vec![row(7, NetworkKind::Signal)];
        assert_eq!(s.observe(&rows, false), Outcome::default());
        assert_eq!(s.hidden().len(), 2);
        // The first complete answer then prunes and is the baseline (no switch to account 7).
        let mut s = ShownAccounts::load(false, [uuid(1)]);
        s.observe(&[], false);
        let out = s.observe(&rows, true);
        assert!(out.save);
        assert!(s.hidden().is_empty());
    }

    #[test]
    fn a_new_account_joins_with_blending_and_takes_over_without() {
        let rows = three();
        // Blending on: the new account is shown with the others.
        let mut s = known(&rows);
        s.click(&rows, AccountId(1));
        let mut more = rows.clone();
        more.push(row(4, NetworkKind::Fake));
        let out = s.observe(&more, true);
        assert!(!out.save, "nothing to save: it simply is not hidden");
        assert_eq!(shown_ids(&s, &more), [1, 4]);
        // Blending off: the list switches to it.
        let mut s = ShownAccounts::load(false, []);
        s.observe(&rows, true);
        let out = s.observe(&more, true);
        assert!(out.changed && out.save);
        assert_eq!(shown_ids(&s, &more), [4]);
        // The first answer of a start is the baseline: nothing switches then.
        let mut s = ShownAccounts::load(false, []);
        let out = s.observe(&more, true);
        assert!(!out.changed);
        assert_eq!(shown_ids(&s, &more), [1, 2, 3, 4]);
    }

    #[test]
    fn a_replaced_account_keeps_its_choice() {
        let rows = three();
        // The replaced account is hidden: the new one is hidden too.
        let mut s = known(&rows);
        s.click(&rows, AccountId(3));
        assert_eq!(shown_ids(&s, &rows), [3]);
        s.replace_begin(&rows, AccountId(1));
        // RemoveAccount went: the old account is gone from the rows; its uuid is kept.
        let mut after = vec![rows[1].clone(), rows[2].clone()];
        s.observe(&after, true);
        assert!(
            s.hidden().contains(&uuid(1)),
            "kept while the replace waits"
        );
        s.replace_answered(AccountId(1), AccountId(9));
        after.push(row(9, NetworkKind::Signal));
        let out = s.observe(&after, true);
        assert!(out.save);
        assert!(!s.hidden().contains(&uuid(1)));
        assert!(s.hidden().contains(&uuid(9)), "the new account is hidden");
        assert_eq!(shown_ids(&s, &after), [3]);
    }

    #[test]
    fn a_replace_is_keyed_by_its_own_login() {
        let rows = three();
        let mut s = known(&rows);
        s.click(&rows, AccountId(3));
        s.replace_begin(&rows, AccountId(1));
        let mut after = vec![rows[1].clone(), rows[2].clone()];
        // The link was cancelled: the carry is dropped, the old uuid is forgotten.
        let out = s.replace_ended(&after, AccountId(1));
        assert!(out.save);
        assert!(!s.hidden().contains(&uuid(1)));
        // A later link of the same network is an ordinary new account: it is shown.
        after.push(row(9, NetworkKind::Signal));
        s.observe(&after, true);
        assert_eq!(shown_ids(&s, &after), [3, 9]);
        // An id nobody asked for does not take a carry.
        let mut s = known(&rows);
        s.click(&rows, AccountId(3));
        s.replace_begin(&rows, AccountId(1));
        s.replace_answered(AccountId(1), AccountId(8));
        let mut after = vec![rows[1].clone(), rows[2].clone()];
        after.push(row(9, NetworkKind::Signal));
        s.observe(&after, true);
        assert!(s.hidden().contains(&uuid(1)), "still waiting for 8");
        assert!(s.is_shown(uuid(9)), "9 is not the replace's login");
    }

    #[test]
    fn while_a_replace_waits_its_slot_counts_as_shown() {
        // The replaced account is the only one shown (blending off): no prune to "All Chats".
        let rows = three();
        let mut s = ShownAccounts::load(false, [uuid(2), uuid(3)]);
        s.observe(&rows, true);
        assert_eq!(shown_ids(&s, &rows), [1]);
        s.replace_begin(&rows, AccountId(1));
        let after = vec![rows[1].clone(), rows[2].clone()];
        let out = s.observe(&after, true);
        assert!(!out.save);
        assert_eq!(s.hidden().len(), 2, "both visible accounts stay hidden");
        assert!(s.waiting());
        assert_eq!(s.shown_count(&after), 1);
        assert!(!s.is_last_shown(&after, uuid(2)));
        // The link ends without an account: now the prune turns it into "All Chats".
        let out = s.replace_ended(&after, AccountId(1));
        assert!(out.save);
        assert!(s.hidden().is_empty());
        // The new account then takes the slot: only it is shown.
        let mut s = ShownAccounts::load(false, [uuid(2), uuid(3)]);
        s.observe(&rows, true);
        s.replace_begin(&rows, AccountId(1));
        s.replace_answered(AccountId(1), AccountId(9));
        let mut after = vec![rows[1].clone(), rows[2].clone()];
        s.observe(&after, true);
        after.push(row(9, NetworkKind::Signal));
        s.observe(&after, true);
        assert_eq!(shown_ids(&s, &after), [9]);
    }

    #[test]
    fn a_click_during_a_replace_hides_the_waiting_slot() {
        let rows = three();
        let mut s = known(&rows);
        s.click(&rows, AccountId(1));
        s.replace_begin(&rows, AccountId(1));
        let after = vec![rows[1].clone(), rows[2].clone()];
        s.click(&after, AccountId(2));
        assert!(
            !s.waiting(),
            "the replaced account is not 'only it' any more"
        );
        s.replace_answered(AccountId(1), AccountId(9));
        let mut after = after;
        after.push(row(9, NetworkKind::Signal));
        s.observe(&after, true);
        assert_eq!(shown_ids(&s, &after), [2], "the new account is hidden");
    }

    #[test]
    fn with_one_account_the_panel_and_title_stay_as_today() {
        let rows = vec![row(1, NetworkKind::Signal)];
        let mut s = known(&rows);
        // "All Chats" is tinted, no account singled out.
        assert!(s.all_shown(&rows));
        assert_eq!(s.current(&rows), None);
        // A click tints the account and titles the list with its name.
        let out = s.click(&rows, AccountId(1));
        assert!(out.changed && !out.save, "the stored choice is the same");
        assert_eq!(s.current(&rows), Some(AccountId(1)));
        assert!(!s.all_shown(&rows));
        // "All Chats" tints "All Chats" again.
        assert!(s.show_all().changed);
        assert!(s.all_shown(&rows));
        // With one account a toggle (Shift/Ctrl+click, Ctrl+Space) is the click: it tints the
        // account, never "At least one account stays in the list" (the blend rules start at
        // two: M12).
        let out = s.toggle(&rows, AccountId(1));
        assert!(!out.last_stays, "one account: a toggle is a click");
        assert!(out.changed && !out.save);
        assert_eq!(s.current(&rows), Some(AccountId(1)));
        assert!(s.is_shown(uuid(1)));
        // With two accounts the blend rules apply: one shown is singled out.
        let two = vec![row(1, NetworkKind::Signal), row(2, NetworkKind::Signal)];
        let mut s = known(&two);
        s.click(&two, AccountId(1));
        assert_eq!(s.current(&two), Some(AccountId(1)));
        // The pick of the only account ends with a second account.
        let mut s = known(&rows);
        s.click(&rows, AccountId(1));
        assert_eq!(s.current(&two), None, "shown: 1 and 2");
    }

    #[test]
    fn with_one_account_a_toggle_is_a_click_in_both_modes() {
        let rows = vec![row(1, NetworkKind::Signal)];
        for blend in [true, false] {
            let mut s = ShownAccounts::load(blend, []);
            s.observe(&rows, true);
            let out = s.toggle(&rows, AccountId(1));
            assert!(
                !out.last_stays,
                "blend {blend}: no refusal with one account"
            );
            assert_eq!(s.current(&rows), Some(AccountId(1)), "blend {blend}");
            assert!(!s.all_shown(&rows));
            // A second toggle keeps it picked (a click on the picked account).
            let out = s.toggle(&rows, AccountId(1));
            assert!(!out.last_stays && !out.changed);
            assert_eq!(s.current(&rows), Some(AccountId(1)));
        }
        // With two, the last shown one stays.
        let two = vec![row(1, NetworkKind::Signal), row(2, NetworkKind::Signal)];
        let mut s = known(&two);
        s.click(&two, AccountId(1));
        assert!(s.toggle(&two, AccountId(1)).last_stays);
    }

    #[test]
    fn a_replace_that_ends_after_its_announcement_frees_its_slot() {
        // A shown, B hidden. "Transfer History from Phone..." on A; the core announced the new
        // account 9 (still linking, so not in the rows), then the link was cancelled or failed.
        let rows = vec![row(1, NetworkKind::Signal), row(2, NetworkKind::Signal)];
        let mut s = known(&rows);
        s.click(&rows, AccountId(1));
        s.replace_begin(&rows, AccountId(1));
        s.replace_answered(AccountId(1), AccountId(9));
        let after = vec![rows[1].clone()];
        s.observe(&after, true);
        assert!(s.waiting());
        assert_eq!(s.shown_count(&after), 1, "the slot counts while it waits");
        // The news of ANOTHER replace (a stale one) changes nothing.
        assert_eq!(s.replace_ended(&after, AccountId(5)), Outcome::default());
        assert!(s.waiting());
        // The end of this one: the slot goes, nothing is shown, so all are.
        let out = s.replace_ended(&after, AccountId(1));
        assert!(out.changed && out.save);
        assert!(!s.waiting());
        assert!(s.hidden().is_empty(), "the old uuid is forgotten");
        assert_eq!(s.shown_count(&after), 1, "the real count: B");
        assert!(s.all_shown(&after));
        // The last-shown guard holds again with two accounts (B and a later one).
        let two = vec![rows[1].clone(), row(3, NetworkKind::Signal)];
        s.observe(&two, true);
        s.click(&two, AccountId(2));
        assert!(s.toggle(&two, AccountId(2)).last_stays);
        assert_eq!(s.shown_count(&two), 1);
        // The account 9 that never came takes no choice when an account of that id shows.
        let late = vec![
            rows[1].clone(),
            row(3, NetworkKind::Signal),
            row(9, NetworkKind::Signal),
        ];
        s.observe(&late, true);
        assert!(s.is_shown(uuid(9)), "an ordinary new account joins");
    }

    #[test]
    fn a_saved_choice_is_not_all_chats_before_the_accounts_answer() {
        let rows = three();
        let s = ShownAccounts::load(true, [uuid(2)]);
        assert!(!s.all_shown(&[]), "the saved choice hides an account");
        assert!(
            !s.all_shown(&rows),
            "not before the first complete answer either"
        );
        let mut s = s;
        s.observe(&rows, true);
        assert!(!s.all_shown(&rows));
        assert_eq!(shown_ids(&s, &rows), [1, 3]);
        // Nothing saved: "All Chats" from the start.
        assert!(ShownAccounts::load(true, []).all_shown(&[]));
        // A saved uuid of an account that is gone: pruned at the first answer, "All Chats".
        let mut s = ShownAccounts::load(true, [uuid(7)]);
        assert!(!s.all_shown(&[]));
        s.observe(&rows, true);
        assert!(s.all_shown(&rows));
    }

    #[test]
    fn fill_rows_marks_shown_and_numbers_the_places() {
        let mut rows = three();
        let mut s = known(&rows);
        s.click(&rows, AccountId(2));
        s.fill_rows(&mut rows);
        assert_eq!(
            rows.iter().map(|r| r.shown).collect::<Vec<_>>(),
            [false, true, false]
        );
        // Signal 1, Signal 2, Threema alone: 1, 2, 0 (hiding does not renumber).
        assert_eq!(rows.iter().map(|r| r.place).collect::<Vec<_>>(), [1, 2, 0]);
        // An account that goes closes the numbers up.
        let mut rest = vec![rows[1].clone(), rows[2].clone()];
        s.fill_rows(&mut rest);
        assert_eq!(rest.iter().map(|r| r.place).collect::<Vec<_>>(), [0, 0]);
    }
}
