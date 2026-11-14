//! The documents of an export (Export Selected): `messages.jsonl`, `index.html` and the head of
//! `manifest.json`, built from the core's [`ExportPlan`], the dialog's [`Ticks`] and the names
//! Kourier shows. Qt-free; [`build`] takes only owned, `Send` inputs so the caller may run it on
//! a worker thread. The format is described in `docs/export-format.md`.

mod entries;
pub mod flow;
mod html;
mod numbers;
mod people;
mod record;
mod snapshot;
#[cfg(test)]
mod tests;

use std::collections::HashMap;

use kourier_core::facade::{
    ExportDocument, ExportFile, ExportHead, ExportPlan, ExportStamp, ExportVerdict, LeftOut,
};
use kourier_model::{ConvKind, UserId};

use crate::i18n::trc;
use serde_json::{Map, Value, json};

pub use entries::{Entries, Shown, shown_as};
pub use numbers::{ClassCount, DialogNumbers, FileClass, ShownNumbers, Ticks, class_of, numbers};
pub use people::{
    PSEUDONYM_MEMBERS, People, Person, TableNames, hides_identity, looks_like_number, shown_name,
};
pub use snapshot::{NameSnapshot, OffsetTable};

use record::{file_goes, iso, offset_label};

/// Everything [`build`] needs, owned.
pub struct BuildInput {
    /// The plan the core answered.
    pub plan: ExportPlan,
    /// The dialog's ticks.
    pub ticks: Ticks,
    /// The names of the people the plan names ([`NameSnapshot::from_plan`]).
    pub names: NameSnapshot,
    /// What our own account is called in the people table.
    pub own_name: String,
    /// The chat's title as Kourier shows it.
    pub chat_title: String,
    /// The chat's kind.
    pub chat_kind: ConvKind,
    /// The other person of a `Direct` chat: the chat's title is their name from the people table
    /// (so the pseudonym rule applies). `None` for any other chat.
    pub peer: Option<UserId>,
    /// Our own account's ids: each is `me` in the people table, never a second person.
    pub own: Vec<UserId>,
    /// The network's display name ("Signal"): never a number or an account name.
    pub network: String,
    /// The UTC offsets at the plan's instants ([`OffsetTable::from_plan`]).
    pub offsets: OffsetTable,
    /// The time zone's name as the manifest says it (an IANA id, or an offset like `+03:00`).
    pub time_zone: String,
    /// `Kourier <version>`.
    pub exporter: String,
    /// A random key of this export, 32 bytes (the pseudonyms come from it).
    pub salt: [u8; 32],
    /// The page's language tag (`en`, `uk`).
    pub language: String,
}

/// What an export carries, for the end notice and the manifest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Messages in the documents.
    pub messages: u32,
    /// Files that go into the archive.
    pub files_included: u32,
    /// Files listed but not on this computer.
    pub files_not_on_this_computer: u32,
    /// View-once messages left out.
    pub left_out_view_once: u32,
    /// Disappearing messages left out (no tick).
    pub left_out_disappearing: u32,
    /// Messages gone for the user, left out.
    pub left_out_gone: u32,
    /// Files left out because their kind is not ticked.
    pub left_out_kinds: u32,
}

/// The result of [`build`], ready for `UiCommand::ExportArchive`.
pub struct Built {
    /// Exactly the messages the documents carry, each with the stamp the plan gave it (the core
    /// compares it at the command).
    pub messages: Vec<ExportStamp>,
    /// The files that go into the archive: accepted, of a listed message, kind ticked.
    pub files: Vec<ExportFile>,
    /// `index.html` and `messages.jsonl`, in the archive's order (never `manifest.json`: that is the core's).
    pub documents: Vec<ExportDocument>,
    /// The manifest's head (the core adds `entries` and `files_failed`).
    pub head: ExportHead,
    /// The numbers.
    pub counts: Counts,
}

fn kind_name(k: ConvKind) -> &'static str {
    match k {
        ConvKind::Direct => "direct",
        ConvKind::Group => "group",
        ConvKind::Broadcast => "broadcast",
        ConvKind::Channel => "channel",
    }
}

/// The title the three places (`<title>`, `<h1>`, `chat.title`) share.
fn chat_title_of(
    table: &mut People,
    kind: ConvKind,
    peer: Option<&UserId>,
    chat_title: &str,
) -> String {
    if kind == ConvKind::Direct
        && let Some(p) = peer
    {
        table.id_of(Some(p));
        return table.name_of(Some(p));
    }
    if let Some(t) = shown_name(chat_title) {
        return t;
    }
    let word = match kind {
        ConvKind::Group => trc("@title:window the title of an exported group chat", "Group"),
        ConvKind::Channel => trc("@title:window the title of an exported channel", "Channel"),
        ConvKind::Broadcast => trc(
            "@title:window the title of an exported broadcast list",
            "Broadcast list",
        ),
        ConvKind::Direct => trc("@title:window the title of an exported direct chat", "Chat"),
    };
    word.into_owned()
}

/// The title of an exported chat as the archive's NAME carries it: the documents' own title
/// ([`chat_title_of`]) with one difference. A direct chat whose peer's name hides an identity
/// has a pseudonym in the documents (a random one per export); a name must not carry it, nor
/// the number, so it says "Chat". Everything else is the very title of the documents.
pub fn archive_name_title(
    names: &NameSnapshot,
    own_name: &str,
    own: &[UserId],
    kind: ConvKind,
    members: u32,
    peer: Option<&UserId>,
    chat_title: &str,
) -> String {
    let mut table = People::new(names, own_name, own, kind, members, [0; 32]);
    let title = chat_title_of(&mut table, kind, peer, chat_title);
    let hidden = kind == ConvKind::Direct
        && peer.is_some()
        && table.list().last().is_some_and(|p| p.pseudonym);
    if hidden {
        return trc("@title:window the title of an exported direct chat", "Chat").into_owned();
    }
    title
}

/// Makes the documents. A plan whose chat may not be exported gives an empty [`Built`] (the
/// core refuses the command with `ChatRefused` anyway).
pub fn build(input: BuildInput) -> Built {
    let BuildInput {
        plan,
        ticks,
        names,
        own_name,
        chat_title,
        chat_kind,
        peer,
        own,
        network,
        offsets,
        time_zone,
        exporter,
        salt,
        language,
    } = input;
    let mut counts = Counts::default();
    let mut carried = Vec::new();
    if plan.chat.is_ok() {
        for pm in &plan.messages {
            match pm.verdict {
                ExportVerdict::Included => carried.push(pm),
                ExportVerdict::NeedsTick if ticks.disappearing => carried.push(pm),
                ExportVerdict::NeedsTick => counts.left_out_disappearing += 1,
                ExportVerdict::LeftOut(LeftOut::ViewOnce) => counts.left_out_view_once += 1,
                ExportVerdict::LeftOut(LeftOut::Gone) => counts.left_out_gone += 1,
            }
        }
    }

    // The people, in order of first appearance.
    let mut table = People::new(&names, &own_name, &own, chat_kind, plan.members, salt);
    for pm in &carried {
        for u in snapshot::users_of(pm) {
            table.id_of(Some(&u));
        }
    }

    // The chat's title: a direct chat is its peer's name from the table (a pseudonym when the
    // name hides an identity); any other kind is `chat_title` unless the name rule hides it.
    let title = chat_title_of(&mut table, chat_kind, peer.as_ref(), &chat_title);

    // The file entries.
    let mut set = Entries::new();
    let mut by_attachment: HashMap<kourier_model::AttachmentId, String> = HashMap::new();
    let mut files = Vec::new();
    for pm in &carried {
        for f in &pm.files {
            if file_goes(pm, f, &ticks) {
                let entry = set.next(f.file_name.as_deref(), &f.mime, f.sniffed.as_deref());
                by_attachment.insert(f.attachment, entry.clone());
                files.push(ExportFile {
                    attachment: f.attachment,
                    entry,
                });
            }
        }
    }

    let items: Vec<record::Item> = carried
        .iter()
        .map(|pm| record::item(pm, &table, &offsets, &by_attachment, &ticks))
        .collect();
    counts.messages = u32::try_from(items.len()).unwrap_or(u32::MAX);
    counts.files_included = u32::try_from(files.len()).unwrap_or(u32::MAX);
    for it in &items {
        for f in &it.files {
            if f.state == "not_on_this_computer" {
                counts.files_not_on_this_computer += 1;
            }
            if f.kind_left_out {
                counts.left_out_kinds += 1;
            }
        }
    }

    let mut jsonl = String::new();
    for it in &items {
        jsonl.push_str(&it.json());
        jsonl.push('\n');
    }
    let safe_lang = if !language.is_empty()
        && language
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        language.as_str()
    } else {
        "en"
    };
    let page = html::Page {
        title: &title,
        lang: safe_lang,
        now: plan.now,
        calendar: &offsets,
    };
    let index = html::index(&items, &page);

    let people: Vec<Value> = table
        .list()
        .iter()
        .map(|p| {
            let mut m = Map::new();
            m.insert("id".into(), p.id.clone().into());
            m.insert("name".into(), p.name.clone().into());
            if p.own {
                m.insert("self".into(), true.into());
            }
            if p.pseudonym {
                m.insert("pseudonym".into(), true.into());
            }
            Value::Object(m)
        })
        .collect();
    let mut head = Map::new();
    head.insert("format".into(), "kourier-chat-export".into());
    head.insert("version".into(), 1.into());
    head.insert("exported_at".into(), iso(plan.now, &offsets).into());
    head.insert("time_zone".into(), time_zone.into());
    head.insert("exporter".into(), exporter.into());
    head.insert(
        "chat".into(),
        json!({"title": title, "kind": kind_name(chat_kind), "network": network}),
    );
    head.insert("selection".into(), true.into());
    head.insert("people".into(), Value::Array(people));
    head.insert(
        "options".into(),
        json!({
            "pictures": ticks.pictures,
            "videos": ticks.videos,
            "other_files": ticks.other_files,
            "disappearing": ticks.disappearing,
            "people": if table.all_pseudonyms() { "pseudonyms" } else { "names" },
        }),
    );
    head.insert(
        "counts".into(),
        json!({
            "messages": counts.messages,
            "files_included": counts.files_included,
            "files_not_on_this_computer": counts.files_not_on_this_computer,
            "left_out": {
                "view_once": counts.left_out_view_once,
                "disappearing": counts.left_out_disappearing,
                "gone": counts.left_out_gone,
                "kinds": counts.left_out_kinds,
            },
        }),
    );
    let _ = offset_label;

    Built {
        messages: carried.iter().map(|pm| pm.stamp).collect(),
        files,
        documents: vec![
            ExportDocument {
                entry: "index.html".to_owned(),
                bytes: index.into_bytes(),
            },
            ExportDocument {
                entry: "messages.jsonl".to_owned(),
                bytes: jsonl.into_bytes(),
            },
        ],
        head: ExportHead(head),
        counts,
    }
}
