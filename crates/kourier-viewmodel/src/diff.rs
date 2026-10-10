//! The list diff engine: turns an old and a new row list into the `ListOp`s a Qt list model
//! applies with `begin*/end*` pairs (§8.4).
//!
//! Ops are applied in order to the *current* list. After every op, positions below the one
//! being worked on are final: `Insert` and `Change` refer to rows of the new list at the same
//! index, so an adapter copies `new[at..at + n]` for an insert and `new[at]` for a change.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

/// A set of changed roles, one bit per role index (0 … 63) of a model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct RoleSet(pub u64);

impl RoleSet {
    /// No role.
    pub const EMPTY: RoleSet = RoleSet(0);
    /// Every role.
    pub const ALL: RoleSet = RoleSet(u64::MAX);

    /// Adds role `index` (ignored above 63).
    pub fn with(self, index: u32) -> Self {
        Self(self.0 | 1u64.checked_shl(index).unwrap_or(0))
    }

    /// Adds `index` when `changed`.
    pub fn with_if(self, index: u32, changed: bool) -> Self {
        if changed { self.with(index) } else { self }
    }

    /// Whether role `index` is in the set.
    pub fn contains(self, index: u32) -> bool {
        self.0 & 1u64.checked_shl(index).unwrap_or(0) != 0
    }

    /// Whether the set is empty.
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The role indices in the set.
    pub fn iter(self) -> impl Iterator<Item = u32> {
        (0..64).filter(move |i| self.contains(*i))
    }
}

/// One list mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ListOp {
    /// Replace everything with the new list.
    Reset,
    /// Insert `n` rows of the new list at `at`.
    Insert {
        /// Position (also in the new list).
        at: usize,
        /// Row count.
        n: usize,
    },
    /// Remove `n` rows starting at `at`.
    Remove {
        /// Position.
        at: usize,
        /// Row count.
        n: usize,
    },
    /// Move one row from `from` to `to` (`to` is its index after the move).
    Move {
        /// Current position.
        from: usize,
        /// Position after the move.
        to: usize,
    },
    /// Row `at` (= new row `at`) changed in `roles`.
    Change {
        /// Position.
        at: usize,
        /// Changed roles.
        roles: RoleSet,
    },
}

/// Computes the ops that turn `old` into `new`. Rows are matched by `key` (unique within each
/// list); `changed(old, new)` tells which roles of a kept row differ.
///
/// Removals come first (from the end), then a left-to-right pass that inserts new rows and
/// moves kept rows not in the longest increasing subsequence of their new positions, so a
/// single reordered row costs one or two moves.
pub fn diff<R, K, FK, FC>(old: &[R], new: &[R], key: FK, changed: FC) -> Vec<ListOp>
where
    K: Eq + Hash + Clone,
    FK: Fn(&R) -> K,
    FC: Fn(&R, &R) -> RoleSet,
{
    let new_pos: HashMap<K, usize> = new.iter().enumerate().map(|(i, r)| (key(r), i)).collect();
    let mut ops = Vec::new();

    // 1. Removals, from the end so indices stay valid; consecutive ones coalesce.
    let mut cur: Vec<(K, usize)> = Vec::with_capacity(old.len()); // (key, index into old)
    let mut removed: Vec<usize> = Vec::new();
    for (i, r) in old.iter().enumerate() {
        let k = key(r);
        if new_pos.contains_key(&k) {
            cur.push((k, i));
        } else {
            removed.push(i);
        }
    }
    let mut j = removed.len();
    while j > 0 {
        let end = removed[j - 1];
        let mut start = end;
        j -= 1;
        while j > 0 && removed[j - 1] + 1 == start {
            start = removed[j - 1];
            j -= 1;
        }
        ops.push(ListOp::Remove {
            at: start,
            n: end - start + 1,
        });
    }

    // 2. Rows that keep their relative order (LIS over new positions) never move.
    let positions: Vec<usize> = cur.iter().map(|(k, _)| new_pos[k]).collect();
    let stable: HashSet<K> = lis(&positions)
        .into_iter()
        .map(|i| cur[i].0.clone())
        .collect();

    // 3. Left to right: cur[..i] always equals new[..i].
    let mut i = 0;
    while i < new.len() {
        let k = key(&new[i]);
        if cur.get(i).is_some_and(|(ck, _)| *ck == k) {
            let roles = changed(&old[cur[i].1], &new[i]);
            if !roles.is_empty() {
                ops.push(ListOp::Change { at: i, roles });
            }
            i += 1;
            continue;
        }
        match cur
            .iter()
            .skip(i)
            .position(|(ck, _)| *ck == k)
            .map(|p| p + i)
        {
            Some(j) if stable.contains(&k) => {
                // A stable row is blocked by a moving row: send that one past it.
                let blocker_target = new_pos[&cur[i].0];
                let to = blocker_target.clamp(j, cur.len() - 1);
                let item = cur.remove(i);
                cur.insert(to, item);
                ops.push(ListOp::Move { from: i, to });
            }
            Some(j) => {
                let item = cur.remove(j);
                cur.insert(i, item);
                ops.push(ListOp::Move { from: j, to: i });
            }
            None => {
                // Coalesce a run of new rows.
                let mut n = 1;
                while i + n < new.len() && !cur_contains(&cur, &key(&new[i + n]), i) {
                    n += 1;
                }
                ops.push(ListOp::Insert { at: i, n });
                for off in 0..n {
                    cur.insert(i + off, (key(&new[i + off]), usize::MAX));
                }
                i += n;
            }
        }
    }
    // Inserted placeholders carry usize::MAX as old index; they never reach `changed`.
    ops
}

fn cur_contains<K: Eq>(cur: &[(K, usize)], k: &K, from: usize) -> bool {
    cur.iter().skip(from).any(|(ck, _)| ck == k)
}

/// Indices of one longest strictly increasing subsequence of `xs` (patience sorting).
fn lis(xs: &[usize]) -> Vec<usize> {
    let mut tails: Vec<usize> = Vec::new(); // indices into xs
    let mut prev: Vec<Option<usize>> = vec![None; xs.len()];
    for (i, &x) in xs.iter().enumerate() {
        let pos = tails.partition_point(|&t| xs[t] < x);
        if pos > 0 {
            prev[i] = Some(tails[pos - 1]);
        }
        if pos == tails.len() {
            tails.push(i);
        } else {
            tails[pos] = i;
        }
    }
    let mut out = Vec::with_capacity(tails.len());
    let mut at = tails.last().copied();
    while let Some(i) = at {
        out.push(i);
        at = prev[i];
    }
    out.reverse();
    out
}

/// Like [`diff`], but returns `[Reset]` when the ops would touch most of the list (a
/// conversation switch, a big reload): one reset is cheaper for the view than many ops.
pub fn diff_or_reset<R, K, FK, FC>(old: &[R], new: &[R], key: FK, changed: FC) -> Vec<ListOp>
where
    K: Eq + Hash + Clone,
    FK: Fn(&R) -> K,
    FC: Fn(&R, &R) -> RoleSet,
{
    if old.is_empty() || new.is_empty() {
        return if old.is_empty() && new.is_empty() {
            Vec::new()
        } else {
            vec![ListOp::Reset]
        };
    }
    let ops = diff(old, new, key, changed);
    let structural: usize = ops
        .iter()
        .map(|op| match op {
            ListOp::Insert { n, .. } | ListOp::Remove { n, .. } => *n,
            ListOp::Move { .. } | ListOp::Reset => 1,
            ListOp::Change { .. } => 0,
        })
        .sum();
    if structural > 16 && structural * 2 > new.len().max(old.len()) {
        vec![ListOp::Reset]
    } else {
        ops
    }
}

/// Applies `ops` to `list` the way a model adapter does, taking inserted and changed rows from
/// `new`. Used by tests and by adapters that keep plain row vectors.
pub fn apply<R: Clone>(list: &mut Vec<R>, ops: &[ListOp], new: &[R]) {
    for op in ops {
        match *op {
            ListOp::Reset => {
                list.clear();
                list.extend_from_slice(new);
            }
            ListOp::Insert { at, n } => {
                for (off, row) in new[at..at + n].iter().enumerate() {
                    list.insert(at + off, row.clone());
                }
            }
            ListOp::Remove { at, n } => {
                list.drain(at..at + n);
            }
            ListOp::Move { from, to } => {
                let row = list.remove(from);
                list.insert(to, row);
            }
            ListOp::Change { at, .. } => list[at] = new[at].clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    type Row = (u32, u32); // (key, payload)

    fn d(old: &[Row], new: &[Row]) -> Vec<ListOp> {
        diff(
            old,
            new,
            |r| r.0,
            |a, b| RoleSet::EMPTY.with_if(0, a.1 != b.1),
        )
    }

    fn check(old: &[Row], new: &[Row]) -> Vec<ListOp> {
        let ops = d(old, new);
        let mut list = old.to_vec();
        apply(&mut list, &ops, new);
        assert_eq!(list, new, "ops {ops:?}");
        ops
    }

    fn rows(keys: &[u32]) -> Vec<Row> {
        keys.iter().map(|k| (*k, 0)).collect()
    }

    #[test]
    fn simple_cases() {
        assert_eq!(check(&rows(&[1, 2, 3]), &rows(&[1, 2, 3])), []);
        assert_eq!(
            check(&rows(&[1, 2, 3]), &rows(&[0, 1, 2, 3])),
            [ListOp::Insert { at: 0, n: 1 }]
        );
        assert_eq!(
            check(&rows(&[1, 2, 3]), &rows(&[1, 9, 8, 2, 3])),
            [ListOp::Insert { at: 1, n: 2 }]
        );
        assert_eq!(
            check(&rows(&[1, 2, 3, 4, 5]), &rows(&[1, 5])),
            [ListOp::Remove { at: 1, n: 3 }]
        );
        assert_eq!(
            check(&rows(&[1, 2, 3, 4]), &rows(&[2, 4])),
            [
                ListOp::Remove { at: 2, n: 1 },
                ListOp::Remove { at: 0, n: 1 }
            ]
        );
        assert_eq!(
            check(&[(1, 0), (2, 0)], &[(1, 1), (2, 0)]),
            [ListOp::Change {
                at: 0,
                roles: RoleSet(1)
            }]
        );
    }

    #[test]
    fn one_reordered_row_is_one_move() {
        // A conversation bumped to the top.
        assert_eq!(
            check(&rows(&[1, 2, 3, 4]), &rows(&[4, 1, 2, 3])),
            [ListOp::Move { from: 3, to: 0 }]
        );
        // A row sinking to the bottom.
        assert_eq!(
            check(&rows(&[1, 2, 3, 4]), &rows(&[2, 3, 4, 1])),
            [ListOp::Move { from: 0, to: 3 }]
        );
        // A row moving into the middle.
        assert_eq!(
            check(&rows(&[1, 2, 3, 4, 5]), &rows(&[2, 3, 1, 4, 5])),
            [ListOp::Move { from: 0, to: 2 }]
        );
    }

    #[test]
    fn moved_and_changed() {
        let ops = check(&[(1, 0), (2, 0), (3, 0)], &[(3, 7), (1, 0), (2, 0)]);
        assert_eq!(
            ops,
            [
                ListOp::Move { from: 2, to: 0 },
                ListOp::Change {
                    at: 0,
                    roles: RoleSet(1)
                }
            ]
        );
    }

    #[test]
    fn reset_for_big_changes() {
        let old = rows(&(0..100).collect::<Vec<_>>());
        let new = rows(&(100..200).collect::<Vec<_>>());
        assert_eq!(
            diff_or_reset(&old, &new, |r| r.0, |_, _| RoleSet::EMPTY),
            [ListOp::Reset]
        );
        let mut grown = old.clone();
        grown.insert(0, (500, 0));
        assert_eq!(
            diff_or_reset(&old, &grown, |r| r.0, |_, _| RoleSet::EMPTY),
            [ListOp::Insert { at: 0, n: 1 }]
        );
        assert_eq!(
            diff_or_reset(&[], &old, |r| r.0, |_, _| RoleSet::EMPTY),
            [ListOp::Reset]
        );
    }

    #[test]
    fn role_sets() {
        let s = RoleSet::EMPTY.with(0).with(5).with(63).with(64);
        assert!(s.contains(5) && s.contains(63) && !s.contains(1));
        assert_eq!(s.iter().collect::<Vec<_>>(), [0, 5, 63]);
    }

    fn unique_rows() -> impl Strategy<Value = Vec<Row>> {
        proptest::collection::vec((0u32..40, 0u32..3), 0..30).prop_map(|v| {
            let mut seen = HashSet::new();
            v.into_iter().filter(|(k, _)| seen.insert(*k)).collect()
        })
    }

    proptest! {
        /// Any old/new pair: applying the ops yields the new list, moves stay bounded.
        #[test]
        fn ops_reproduce_new(old in unique_rows(), new in unique_rows()) {
            let ops = check(&old, &new);
            let moves = ops.iter().filter(|o| matches!(o, ListOp::Move { .. })).count();
            prop_assert!(moves <= 2 * old.len().min(new.len()));
            for op in &ops {
                if let ListOp::Change { at, .. } = op {
                    prop_assert!(*at < new.len());
                }
            }
        }

        /// Shuffles of one list only move rows.
        #[test]
        fn permutations_only_move(keys in Just((0u32..20).collect::<Vec<_>>()).prop_shuffle()) {
            let old = rows(&(0..20).collect::<Vec<_>>());
            let new = rows(&keys);
            let ops = check(&old, &new);
            let only_moves = ops.iter().all(|o| matches!(o, ListOp::Move { .. }));
            prop_assert!(only_moves, "ops {:?}", ops);
        }
    }
}
