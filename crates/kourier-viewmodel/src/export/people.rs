//! The people table of an export: `me`, then `p1`, `p2` ... in order of first appearance.
//!
//! Names are the ones Kourier shows. A group of [`PSEUDONYM_MEMBERS`] or more, a person without
//! a profile name (the shown name is the fallback made of the id) and a shown name that hides an
//! identity ([`hides_identity`]) all become a pseudonym `p_` + 6 hex digits, made from a random
//! salt of this export and never written. A number known as an identifier, a network id or an
//! avatar is never written.

use std::collections::{HashMap, HashSet};

use kourier_model::{ConvKind, UserId};

use crate::i18n::trc;
use crate::names::{Names, fallback_name, single_line};
use crate::richtext::is_invisible_or_bidi;

/// Members from which a group's people are pseudonyms.
pub const PSEUDONYM_MEMBERS: u32 = 100;

/// One person of the table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Person {
    /// `me`, `p1` ...
    pub id: String,
    /// The shown name, or the pseudonym.
    pub name: String,
    /// The name is a pseudonym.
    pub pseudonym: bool,
    /// Our own account.
    pub own: bool,
}

/// The table, filled as the documents are made.
pub struct People<'a> {
    names: &'a dyn Names,
    salt: [u8; 32],
    all_pseudonyms: bool,
    /// Our own account's ids: each is `me`, never a second person.
    own_ids: HashSet<UserId>,
    list: Vec<Person>,
    index: HashMap<UserId, usize>,
    taken: HashSet<String>,
    /// The counter of the pseudonyms past the bounded probe.
    overflow: u64,
}

/// Whether a shown name is (or holds) a phone number: seven digits or more (any Unicode digit),
/// or four digits or more when everything else is white space or one of `+-().`.
pub fn looks_like_number(name: &str) -> bool {
    let digits = name.chars().filter(|c| c.is_numeric()).count();
    let only = name
        .chars()
        .all(|c| c.is_numeric() || c.is_whitespace() || "+-().".contains(c));
    digits >= 7 || (digits >= 4 && only)
}

/// Whether a shown name is e-mail-like: an `@`, then a `.` after it.
fn looks_like_email(name: &str) -> bool {
    name.find('@').is_some_and(|i| name[i..].contains('.'))
}

/// THE name rule: whether a shown name hides an identity, so it may not be written. It does when
/// it is empty, equals its own fallback (`own_fallback`, when there is a user), holds the `⋯` of
/// any fallback, is e-mail-like, or is number-like ([`looks_like_number`]). Used for every person,
/// our own name, a contact card's name and organization and the chat title.
pub fn hides_identity(name: &str, own_fallback: Option<&str>) -> bool {
    name.is_empty()
        || own_fallback.is_some_and(|f| name == f)
        || name.contains('\u{22ef}')
        || looks_like_email(name)
        || looks_like_number(name)
}

/// `name` as the export writes it (cleaned), or `None` when it hides an identity.
pub fn shown_name(name: &str) -> Option<String> {
    let c = clean(name);
    (!hides_identity(&c, None)).then_some(c)
}

/// The pseudonym's number of `user` in one export: the first 24 bits of
/// SHA-256(salt, scheme, ":", value). A KEYED hash: whoever knows some members' ids and their
/// pseudonyms cannot solve for the salt and name the others (an unkeyed mix could be inverted).
fn mix(salt: &[u8; 32], user: &UserId) -> u32 {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(salt);
    h.update(user.scheme().as_bytes());
    h.update(b":");
    h.update(user.value().as_bytes());
    let d = h.finalize();
    u32::from_be_bytes([0, d[0], d[1], d[2]])
}

/// How many neighbouring values a pseudonym tries before the counter suffix.
const PROBE: u32 = 64;

impl<'a> People<'a> {
    /// A table whose person `me` is called `own_name` (the translated "You" when that name hides
    /// an identity). `own_ids` are our account's ids. A chat that is not direct and whose
    /// roster is unknown (`members` 0) gets pseudonyms for everyone, like a big one.
    pub fn new(
        names: &'a dyn Names,
        own_name: &str,
        own_ids: &[UserId],
        kind: ConvKind,
        members: u32,
        salt: [u8; 32],
    ) -> Self {
        let own = Person {
            id: "me".to_owned(),
            name: shown_name(own_name).unwrap_or_else(|| {
                trc(
                    "@item:intext the account owner's name in an exported chat",
                    "You",
                )
                .into_owned()
            }),
            pseudonym: false,
            own: true,
        };
        Self {
            names,
            salt,
            all_pseudonyms: members >= PSEUDONYM_MEMBERS
                || (kind != ConvKind::Direct && members == 0),
            own_ids: own_ids.iter().cloned().collect(),
            list: vec![own],
            index: HashMap::new(),
            taken: HashSet::new(),
            overflow: 0,
        }
    }

    /// The people so far, `me` first.
    pub fn list(&self) -> &[Person] {
        &self.list
    }

    /// Whether names are pseudonyms for everybody (a big group).
    pub fn all_pseudonyms(&self) -> bool {
        self.all_pseudonyms
    }

    /// The id of `user` (`None` = us), adding the person on first sight.
    pub fn id_of(&mut self, user: Option<&UserId>) -> String {
        let Some(user) = user else {
            return "me".to_owned();
        };
        if self.own_ids.contains(user) {
            return "me".to_owned();
        }
        if let Some(i) = self.index.get(user) {
            return self.list[*i].id.clone();
        }
        let shown = clean(&self.names.display_name(user));
        let pseudonym =
            self.all_pseudonyms || hides_identity(&shown, Some(&clean(&fallback_name(user))));
        let name = if pseudonym {
            self.pseudonym(user)
        } else {
            shown
        };
        let id = format!("p{}", self.list.len());
        self.index.insert(user.clone(), self.list.len());
        self.list.push(Person {
            id: id.clone(),
            name,
            pseudonym,
            own: false,
        });
        id
    }

    /// A pseudonym no one else has: `p_` + 6 hex digits, a bounded probe of neighbours, then a
    /// counter suffix (so the loop always ends).
    fn pseudonym(&mut self, user: &UserId) -> String {
        let n = mix(&self.salt, user);
        for i in 0..PROBE {
            let name = format!("p_{:06x}", n.wrapping_add(i) & 0x00ff_ffff);
            if self.taken.insert(name.clone()) {
                return name;
            }
        }
        loop {
            self.overflow += 1;
            let name = format!("p_{n:06x}_{}", self.overflow);
            if self.taken.insert(name.clone()) {
                return name;
            }
        }
    }

    /// The id of a person already in the table (`me` for us); `p?` for one never added.
    pub fn lookup_id(&self, user: Option<&UserId>) -> String {
        match user {
            None => "me".to_owned(),
            Some(u) if self.own_ids.contains(u) => "me".to_owned(),
            Some(u) => self
                .index
                .get(u)
                .map_or_else(|| "p?".to_owned(), |i| self.list[*i].id.clone()),
        }
    }

    /// The name of a person already in the table (a fixed neutral word for one that is not).
    pub fn name_of(&self, user: Option<&UserId>) -> String {
        match user {
            None => self.list[0].name.clone(),
            Some(u) if self.own_ids.contains(u) => self.list[0].name.clone(),
            Some(u) => self.index.get(u).map_or_else(
                || {
                    trc(
                        "@item:intext a person the export has no name for",
                        "Someone",
                    )
                    .into_owned()
                },
                |i| self.list[*i].name.clone(),
            ),
        }
    }
}

/// A name as the export writes it: one line, no bidi or invisible control.
fn clean(name: &str) -> String {
    single_line(
        &name
            .chars()
            .filter(|c| !is_invisible_or_bidi(*c))
            .collect::<String>(),
    )
}

/// The names of a finished table, as [`Names`] for the notice and call texts.
pub struct TableNames<'a, 'b>(pub &'b People<'a>);

impl Names for TableNames<'_, '_> {
    fn display_name(&self, user: &UserId) -> String {
        self.0.name_of(Some(user))
    }
}
