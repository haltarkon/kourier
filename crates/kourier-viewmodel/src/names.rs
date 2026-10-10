//! Display names of users, as rows and texts need them.

use std::collections::HashMap;

use kourier_model::UserId;

use crate::i18n::{trc, trfc};

/// Resolves a user to the name the UI shows (contact name, profile name or a fallback).
pub trait Names {
    /// The display name of `user`; never empty.
    fn display_name(&self, user: &UserId) -> String;
}

/// A name table with a fallback for unknown users.
#[derive(Clone, Default)]
pub struct NameMap {
    names: HashMap<UserId, String>,
}

impl std::fmt::Debug for NameMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NameMap")
            .field("len", &self.names.len())
            .finish()
    }
}

impl NameMap {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the name of `user`.
    pub fn insert(&mut self, user: UserId, name: impl Into<String>) {
        self.names.insert(user, name.into());
    }

    /// The stored name, if any.
    pub fn get(&self, user: &UserId) -> Option<&str> {
        self.names.get(user).map(String::as_str)
    }
}

impl Names for NameMap {
    fn display_name(&self, user: &UserId) -> String {
        match self.names.get(user) {
            Some(n) if !n.trim().is_empty() => single_line(n),
            _ => fallback_name(user),
        }
    }
}

/// Id schemes whose value is a phone number (the same list as the model's log redaction).
const PHONE_SCHEMES: [&str; 4] = ["e164", "pn", "phone", "tel"];

/// The label of a user without any name (profiles, contact sync and nicknames arrive later):
/// the phone number when the id is one (`e164:+4915…`, WhatsApp's `pn:4915…`), else the
/// network's "user" label with the last four characters of the id ("Signal user ⋯1f2e").
/// Never the raw id.
pub fn fallback_name(user: &UserId) -> String {
    let value = user.value();
    if PHONE_SCHEMES.contains(&user.scheme()) || value.starts_with('+') {
        let number = single_line(value);
        // `pn:` values carry no `+`; a bare digit string is shown as an international number.
        if !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()) {
            return format!("+{number}");
        }
        if !number.is_empty() {
            return number;
        }
    }
    let tail: Vec<char> = user
        .value()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    let last4: String = tail[tail.len().saturating_sub(4)..].iter().collect();
    if last4.is_empty() {
        return trc("@item:intext a person without a name", "Unknown").into_owned();
    }
    // TRANSLATORS: the name of a person with no name, made from the end of their id: %1 is
    // four characters.
    match user.scheme() {
        "aci" | "pni" => trfc(
            "@item:intext a person without a name, %1 the end of their id",
            "Signal user ⋯%1",
            &[&last4],
        ),
        "threema" => trfc(
            "@item:intext a person without a name, %1 the end of their id",
            "Threema user ⋯%1",
            &[&last4],
        ),
        "lid" => trfc(
            "@item:intext a person without a name, %1 the end of their id",
            "WhatsApp user ⋯%1",
            &[&last4],
        ),
        _ => trfc(
            "@item:intext a person without a name, %1 the end of their id",
            "User ⋯%1",
            &[&last4],
        ),
    }
}

/// `s` on one line: newlines and other control characters become spaces, runs of whitespace
/// collapse, and the ends are trimmed.
pub fn single_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut ws = false;
    for c in s.chars() {
        if c.is_whitespace() || c.is_control() {
            ws = !out.is_empty();
        } else {
            if ws {
                out.push(' ');
                ws = false;
            }
            out.push(c);
        }
    }
    out
}

/// The first word of a name ("Olena Kovalenko" → "Olena"), for compact labels.
pub fn first_name(name: &str) -> &str {
    name.split_whitespace().next().unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_fallbacks() {
        let mut m = NameMap::new();
        let a = UserId::parse("aci:a").expect("id");
        let b = UserId::parse("aci:b").expect("id");
        m.insert(a.clone(), " Olena \n Kovalenko ");
        m.insert(b.clone(), " \t");
        assert_eq!(m.display_name(&a), "Olena Kovalenko");
        assert_eq!(m.display_name(&b), "Signal user ⋯b");
        assert_eq!(
            m.display_name(&UserId::parse("aci:c").expect("id")),
            "Signal user ⋯c"
        );
        assert_eq!(first_name("Olena Kovalenko"), "Olena");
        assert_eq!(single_line("a\u{7}b\r\nc"), "a b c");
    }

    #[test]
    fn fallback_names_never_show_raw_ids() {
        let id = |s: &str| UserId::parse(s).expect("id");
        assert_eq!(
            fallback_name(&id("aci:5f3c2a10-1111-4222-8333-0123456a1f2e")),
            "Signal user ⋯1f2e"
        );
        assert_eq!(
            fallback_name(&id("pni:9d0e8f00-1111-4222-8333-0123456789ab")),
            "Signal user ⋯89ab"
        );
        assert_eq!(fallback_name(&id("e164:+4915112345678")), "+4915112345678");
        assert_eq!(fallback_name(&id("pn:4915112345678")), "+4915112345678");
        assert_eq!(
            fallback_name(&id("lid:123456789012345")),
            "WhatsApp user ⋯2345"
        );
        assert_eq!(fallback_name(&id("threema:ECHOECHO")), "Threema user ⋯ECHO");
        assert_eq!(fallback_name(&id("x:--")), "Unknown");
        for u in [
            "aci:5f3c2a10-1111-4222-8333-0123456a1f2e",
            "threema:ECHOECHO",
        ] {
            let shown = fallback_name(&id(u));
            assert!(!shown.contains(':'), "{shown}");
            assert!(!shown.contains(id(u).value()), "{shown}");
        }
    }
}
