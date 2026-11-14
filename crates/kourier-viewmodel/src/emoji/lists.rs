//! The three emoji lists: pins, favourites and recently used. One list for every account, local
//! only (nothing goes to a network; Signal's own preferred-reaction list is neither read nor
//! written). The rules are here; the app keeps the lists in `kourier.toml`.

use super::is_known;

/// Signal's six quick reactions, in Signal's order: what a fresh profile has pinned.
pub const DEFAULT_PINS: [&str; 6] = ["❤️", "👍", "👎", "😂", "😮", "😢"];
/// The reaction picker's quick row holds up to this many pins.
pub const PINS_MAX: usize = 6;
/// The first pins are one-click reactions in a message's hover bar.
pub const HOVER_PINS: usize = 3;
/// Recently used emoji kept, newest first.
pub const RECENT_MAX: usize = 24;
/// Favourites kept at most.
pub const FAVOURITES_MAX: usize = 500;

/// The default pins as owned strings.
pub fn default_pins() -> Vec<String> {
    DEFAULT_PINS.iter().map(|s| (*s).to_owned()).collect()
}

/// `list` cut to the table's emoji, each once, at most `max`.
pub fn clean(list: &[String], max: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for e in list {
        if out.len() == max {
            break;
        }
        if !out.contains(e) && is_known(e) {
            out.push(e.clone());
        }
    }
    out
}

/// The pins that are one-click reactions in the hover bar: the first [`HOVER_PINS`].
pub fn hover_pins(pins: &[String]) -> &[String] {
    &pins[..pins.len().min(HOVER_PINS)]
}

/// An emoji was used (picked, or sent as a reaction): it goes to the front of `recent`, once.
/// Returns whether the list changed. An emoji that is not in the table is ignored.
pub fn touch_recent(recent: &mut Vec<String>, emoji: &str) -> bool {
    if !is_known(emoji) || recent.first().is_some_and(|f| f == emoji) {
        return false;
    }
    recent.retain(|e| e != emoji);
    recent.insert(0, emoji.to_owned());
    recent.truncate(RECENT_MAX);
    true
}

/// Pins `emoji` at the end of `pins` or unpins it. A pin is refused (`false`, nothing changes)
/// when [`PINS_MAX`] are pinned already; unpinning the last pin is allowed.
pub fn toggle_pin(pins: &mut Vec<String>, emoji: &str) -> bool {
    if let Some(at) = pins.iter().position(|e| e == emoji) {
        pins.remove(at);
        return true;
    }
    if !is_known(emoji) || pins.len() >= PINS_MAX {
        return false;
    }
    pins.push(emoji.to_owned());
    true
}

/// Stars `emoji` (at the end of `favourites`) or takes the star away.
pub fn toggle_favourite(favourites: &mut Vec<String>, emoji: &str) -> bool {
    if let Some(at) = favourites.iter().position(|e| e == emoji) {
        favourites.remove(at);
        return true;
    }
    if !is_known(emoji) || favourites.len() >= FAVOURITES_MAX {
        return false;
    }
    favourites.push(emoji.to_owned());
    true
}
