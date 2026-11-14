//! The names of the files inside the archive: `files/<name>`.
//!
//! Each name is the sender's file name cleaned by [`clean_name`] (or the generic word plus the
//! extension of the content), numbered across the whole set with [`with_number`] so that no two
//! are equal even when compared case-insensitively (the writer's own rule is case-sensitive).
//! Every result is one path component the writer accepts ([`check_entry_name`]).

use std::collections::{HashMap, HashSet};

use kourier_core::facade::{check_entry_name, extension_for_mime};

use crate::save_name::{clean_name, generic_stem, with_number};

/// The extensions a browser opens as a passive file (media, a document, text, an archive). An
/// ALLOW-LIST, never a deny-list: a browser sniffs a file whose extension it does not know and
/// may open it as an HTML page (script, no Content-Security-Policy, from `file://`), so only what
/// is known to be passive is linked from `index.html`.
const PASSIVE: [&str; 18] = [
    "mp4", "m4v", "mov", "webm", "mkv", "mp3", "m4a", "aac", "ogg", "oga", "opus", "flac", "wav",
    "pdf", "txt", "csv", "json", "zip",
];

/// The extensions of the pictures a browser shows, by their sniffed type.
fn picture_extensions(sniffed: &str) -> Option<&'static [&'static str]> {
    Some(match sniffed {
        "image/jpeg" => &["jpg", "jpeg"],
        "image/png" => &["png"],
        "image/gif" => &["gif"],
        "image/webp" => &["webp"],
        "image/avif" => &["avif"],
        _ => return None,
    })
}

/// The final extension of an entry name, lowercased.
fn final_extension(entry: &str) -> Option<String> {
    let name = entry.rsplit('/').next().unwrap_or(entry);
    name.rsplit_once('.').map(|(_, e)| e.to_lowercase())
}

/// How `index.html` shows an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shown {
    /// An `<img>` and a link: sniffed as a picture, and the entry ends in its type's extension.
    Picture,
    /// A link: the entry's final extension is on the passive allow-list.
    Link,
    /// Text only (its name, size and entry path), no `href`: the file is opened from the folder.
    Text,
}

/// THE rule of what the page may link, used by [`Entries`] (to name a picture) and by the page.
/// `sniffed` is what the content proved.
pub fn shown_as(entry: &str, sniffed: Option<&str>) -> Shown {
    let ext = final_extension(entry);
    if let (Some(exts), Some(ext)) = (sniffed.and_then(picture_extensions), ext.as_deref())
        && exts.contains(&ext)
    {
        return Shown::Picture;
    }
    match ext {
        Some(e)
            if PASSIVE.contains(&e.as_str()) && sniffed.and_then(picture_extensions).is_none() =>
        {
            Shown::Link
        }
        _ => Shown::Text,
    }
}

/// Hands out unique entry names.
#[derive(Default)]
pub struct Entries {
    seen: HashSet<String>,
    /// The next number to try for a case-folded base name, so a run of equal names (a phone
    /// calls every picture alike) costs one step each, not one per earlier file.
    next_number: HashMap<String, u32>,
    /// The counter of the fallback names past [`MAX_TRIES`].
    fallback: u64,
}

impl Entries {
    /// A new, empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// The entry name of a file: `file_name` as the sender gave it (may be missing), the MIME
    /// type it states and what its content proved.
    pub fn next(&mut self, file_name: Option<&str>, mime: &str, sniffed: Option<&str>) -> String {
        let ext = extension_for_mime(sniffed.unwrap_or(mime));
        let mut base = match file_name {
            Some(n) => clean_name(n),
            None => generic_stem(),
        };
        if file_name.is_none_or(|n| !n.contains('.'))
            && let Some(ext) = ext
        {
            base = clean_name(&format!("{base}.{ext}"));
        }
        // A picture's entry ends in its sniffed type's extension (`photo.html` becomes
        // `photo.html.jpg`): the link and the `<img>` open an image, never the sender's markup.
        if let Some(sniffed) = sniffed
            && picture_extensions(sniffed).is_some()
            && shown_as(&format!("files/{base}"), Some(sniffed)) != Shown::Picture
            && let Some(ext) = extension_for_mime(sniffed)
        {
            base = clean_name(&format!("{base}.{ext}"));
        }
        // A name the writer refuses by itself (a reserved device name, nothing but refused
        // characters) falls back to the generic word and the content's extension.
        if check_entry_name(&format!("files/{base}")).is_err() {
            base = match ext {
                Some(ext) => format!("{}.{ext}", generic_stem()),
                None => generic_stem(),
            };
            if check_entry_name(&format!("files/{base}")).is_err() {
                base = "file".to_owned();
            }
        }
        let base_key = base.to_lowercase();
        let mut n = self.next_number.get(&base_key).copied().unwrap_or(0);
        while n <= MAX_TRIES {
            let name = if n == 0 {
                base.clone()
            } else {
                with_number(&base, n)
            };
            let key = name.to_lowercase();
            let entry = format!("files/{name}");
            if !self.seen.contains(&key) && check_entry_name(&entry).is_ok() {
                self.seen.insert(key);
                self.next_number.insert(base_key, n + 1);
                return entry;
            }
            n += 1;
        }
        // Past the cap: the generic word with a counter of its own, unique by construction
        // (the counter only grows, and a name made of letters and digits passes the rule).
        loop {
            self.fallback += 1;
            let name = format!("file-{}", self.fallback);
            if self.seen.insert(name.clone()) {
                return format!("files/{name}");
            }
        }
    }
}

/// How many numbers one base name tries before the fallback.
const MAX_TRIES: u32 = 100_000;
