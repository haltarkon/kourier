//! `world.json` (TD-4a C7's scrub reads it): every ORIGINAL the world served, so the scrub can
//! replace each one and count what survived. Written by hand (the workspace has no JSON crate):
//! `{"own": {...}, "users": [{"id", "first", "phone"}], "chats": [{"id", "kind", "title",
//! "username"}], "texts": [...]}`. Ids are the chat's peer form (`u201`, `g301`, `c401`).

use super::{ChatId, Kind, OWN_USER_ID, World};
use std::fmt::Write as _;

/// A JSON string literal of `s`.
pub fn quote(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", u32::from(c));
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

impl ChatId {
    /// The control socket's and `world.json`'s name of the chat (`u201`, `g301`, `c401`).
    pub fn name(self) -> String {
        match self {
            ChatId::User(u) => format!("u{u}"),
            ChatId::Basic(c) => format!("g{c}"),
            ChatId::Channel(c) => format!("c{c}"),
        }
    }

    /// The chat a control command names.
    pub fn parse(s: &str) -> Option<ChatId> {
        let (k, n) = s.split_at_checked(1)?;
        let n: i64 = n.parse().ok().filter(|n| *n > 0)?;
        match k {
            "u" => Some(ChatId::User(n)),
            "g" => Some(ChatId::Basic(n)),
            "c" => Some(ChatId::Channel(n)),
            _ => None,
        }
    }
}

impl World {
    /// The world's originals as `world.json`.
    pub fn to_json(&self) -> String {
        let mut o = String::from("{");
        let own = self.users.get(&OWN_USER_ID);
        let _ = write!(
            o,
            "\"own\":{{\"id\":{OWN_USER_ID},\"first\":{},\"phone\":{}}}",
            quote(own.map_or("", |p| p.first.as_str())),
            quote(own.map_or("", |p| p.phone.as_str()))
        );
        o.push_str(",\"users\":[");
        let users: Vec<String> = self
            .users
            .iter()
            .filter(|(id, _)| **id != OWN_USER_ID)
            .map(|(id, p)| {
                format!(
                    "{{\"id\":{id},\"first\":{},\"phone\":{}}}",
                    quote(&p.first),
                    quote(&p.phone)
                )
            })
            .collect();
        o.push_str(&users.join(","));
        o.push_str("],\"chats\":[");
        let chats: Vec<String> = self
            .chats
            .iter()
            .map(|(id, c)| {
                let (kind, title, username) = match &c.kind {
                    Kind::Private => ("private", None, None),
                    Kind::Basic { title } => ("basic", Some(title), None),
                    Kind::Super { title, username } => ("super", Some(title), username.as_ref()),
                    Kind::Broadcast { title, username } => {
                        ("broadcast", Some(title), username.as_ref())
                    }
                };
                format!(
                    "{{\"id\":{},\"kind\":\"{kind}\",\"title\":{},\"username\":{}}}",
                    quote(&id.name()),
                    title.map_or("null".into(), |t| quote(t)),
                    username.map_or("null".into(), |u| quote(u))
                )
            })
            .collect();
        o.push_str(&chats.join(","));
        o.push_str("],\"texts\":[");
        let texts: Vec<String> = self.served_texts.iter().map(|t| quote(t)).collect();
        o.push_str(&texts.join(","));
        o.push_str("]}\n");
        o
    }
}
