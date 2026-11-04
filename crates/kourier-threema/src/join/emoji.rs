// SPDX-FileCopyrightText: Threema AG
// SPDX-FileCopyrightText: 2026 The Kourier developers
// SPDX-License-Identifier: AGPL-3.0-only

//! Ported from Threema Desktop `apps/desktop/src/app/ui/linking/emoji-list.ts` (threema-desktop `93a6615c`,
//! 2026-09-14): the 128 emoji the linking wizard shows to compare the rendezvous path hash.
//!
//! After the phone nominates a path, both sides hold the same 32 byte rendezvous path hash (RPH). The new
//! device shows three emoji made from its first three bytes and the user checks that the phone shows the same
//! three (`steps/ConfirmEmoji.svelte`: `rph[0]`, `rph[1]`, `rph[2]`; `LinkingEmojiLoader.svelte`:
//! `EMOJI_LIST[byte % EMOJI_LIST.length]`). The list has 128 entries, a factor of 256, so the bytes spread over
//! it without bias. The phone must use the same list for the comparison to mean anything [U: the Android app's
//! list equals Desktop's].
//!
//! Desktop's entries are Unicode code-point sequences (`['1F408', '200D', '2B1B']`); here each is written with the
//! same code points as a string, the `FE0F` variation selectors and the zero-width-joiner sequence kept, and the
//! font draws it (Desktop ships an SVG per emoji, Kourier does not). The glyph is in the comment of each row.
//! The RPH proves that both ends hold the rendezvous path, so it is never logged; nothing here logs.

/// The emoji of the linking wizard, in Desktop's order (`emoji-list.ts:3-130`, one entry per line).
pub(crate) const EMOJI: [&str; 128] = [
    "\u{1F435}",                 // 🐵 (emoji-list.ts:3)
    "\u{1F436}",                 // 🐶 (emoji-list.ts:4)
    "\u{1F429}",                 // 🐩 (emoji-list.ts:5)
    "\u{1F98A}",                 // 🦊 (emoji-list.ts:6)
    "\u{1F408}\u{200D}\u{2B1B}", // 🐈‍⬛ (emoji-list.ts:7)
    "\u{1F434}",                 // 🐴 (emoji-list.ts:8)
    "\u{1F9AC}",                 // 🦬 (emoji-list.ts:9)
    "\u{1F42E}",                 // 🐮 (emoji-list.ts:10)
    "\u{1F437}",                 // 🐷 (emoji-list.ts:11)
    "\u{1F42A}",                 // 🐪 (emoji-list.ts:12)
    "\u{1F992}",                 // 🦒 (emoji-list.ts:13)
    "\u{1F418}",                 // 🐘 (emoji-list.ts:14)
    "\u{1F42D}",                 // 🐭 (emoji-list.ts:15)
    "\u{1F430}",                 // 🐰 (emoji-list.ts:16)
    "\u{1F43F}\u{FE0F}",         // 🐿️ (emoji-list.ts:17)
    "\u{1F43C}",                 // 🐼 (emoji-list.ts:18)
    "\u{1F998}",                 // 🦘 (emoji-list.ts:19)
    "\u{1F414}",                 // 🐔 (emoji-list.ts:20)
    "\u{1F54A}\u{FE0F}",         // 🕊️ (emoji-list.ts:21)
    "\u{1F986}",                 // 🦆 (emoji-list.ts:22)
    "\u{1F9A2}",                 // 🦢 (emoji-list.ts:23)
    "\u{1F989}",                 // 🦉 (emoji-list.ts:24)
    "\u{1F9A4}",                 // 🦤 (emoji-list.ts:25)
    "\u{1FAB6}",                 // 🪶 (emoji-list.ts:26)
    "\u{1F9A9}",                 // 🦩 (emoji-list.ts:27)
    "\u{1F438}",                 // 🐸 (emoji-list.ts:28)
    "\u{1F40B}",                 // 🐋 (emoji-list.ts:29)
    "\u{1F42C}",                 // 🐬 (emoji-list.ts:30)
    "\u{1F9AD}",                 // 🦭 (emoji-list.ts:31)
    "\u{1F41F}",                 // 🐟 (emoji-list.ts:32)
    "\u{1F41A}",                 // 🐚 (emoji-list.ts:33)
    "\u{1FAB2}",                 // 🪲 (emoji-list.ts:34)
    "\u{1F578}\u{FE0F}",         // 🕸️ (emoji-list.ts:35)
    "\u{1F3F5}\u{FE0F}",         // 🏵️ (emoji-list.ts:36)
    "\u{1F33B}",                 // 🌻 (emoji-list.ts:37)
    "\u{1F332}",                 // 🌲 (emoji-list.ts:38)
    "\u{1F33F}",                 // 🌿 (emoji-list.ts:39)
    "\u{2618}\u{FE0F}",          // ☘️ (emoji-list.ts:40)
    "\u{1F349}",                 // 🍉 (emoji-list.ts:41)
    "\u{1F34B}",                 // 🍋 (emoji-list.ts:42)
    "\u{1F34E}",                 // 🍎 (emoji-list.ts:43)
    "\u{1F352}",                 // 🍒 (emoji-list.ts:44)
    "\u{1F353}",                 // 🍓 (emoji-list.ts:45)
    "\u{1FAD0}",                 // 🫐 (emoji-list.ts:46)
    "\u{1F345}",                 // 🍅 (emoji-list.ts:47)
    "\u{1F951}",                 // 🥑 (emoji-list.ts:48)
    "\u{1F955}",                 // 🥕 (emoji-list.ts:49)
    "\u{1F966}",                 // 🥦 (emoji-list.ts:50)
    "\u{1F344}",                 // 🍄 (emoji-list.ts:51)
    "\u{1FAD8}",                 // 🫘 (emoji-list.ts:52)
    "\u{1F9C2}",                 // 🧂 (emoji-list.ts:53)
    "\u{1F36A}",                 // 🍪 (emoji-list.ts:54)
    "\u{1F36B}",                 // 🍫 (emoji-list.ts:55)
    "\u{2615}",                  // ☕ (emoji-list.ts:56)
    "\u{1F9CA}",                 // 🧊 (emoji-list.ts:57)
    "\u{1F962}",                 // 🥢 (emoji-list.ts:58)
    "\u{1F5FA}\u{FE0F}",         // 🗺️ (emoji-list.ts:59)
    "\u{1F30B}",                 // 🌋 (emoji-list.ts:60)
    "\u{1F3D5}\u{FE0F}",         // 🏕️ (emoji-list.ts:61)
    "\u{1F3DD}\u{FE0F}",         // 🏝️ (emoji-list.ts:62)
    "\u{1F3DB}\u{FE0F}",         // 🏛️ (emoji-list.ts:63)
    "\u{1F682}",                 // 🚂 (emoji-list.ts:64)
    "\u{1F69A}",                 // 🚚 (emoji-list.ts:65)
    "\u{1F69C}",                 // 🚜 (emoji-list.ts:66)
    "\u{1F6E4}\u{FE0F}",         // 🛤️ (emoji-list.ts:67)
    "\u{2693}",                  // ⚓ (emoji-list.ts:68)
    "\u{1F6F0}\u{FE0F}",         // 🛰️ (emoji-list.ts:69)
    "\u{1F680}",                 // 🚀 (emoji-list.ts:70)
    "\u{1F319}",                 // 🌙 (emoji-list.ts:71)
    "\u{2600}\u{FE0F}",          // ☀️ (emoji-list.ts:72)
    "\u{2B50}",                  // ⭐ (emoji-list.ts:73)
    "\u{1F308}",                 // 🌈 (emoji-list.ts:74)
    "\u{2602}\u{FE0F}",          // ☂️ (emoji-list.ts:75)
    "\u{2744}\u{FE0F}",          // ❄️ (emoji-list.ts:76)
    "\u{2603}\u{FE0F}",          // ☃️ (emoji-list.ts:77)
    "\u{1F525}",                 // 🔥 (emoji-list.ts:78)
    "\u{1F4A7}",                 // 💧 (emoji-list.ts:79)
    "\u{2728}",                  // ✨ (emoji-list.ts:80)
    "\u{1F388}",                 // 🎈 (emoji-list.ts:81)
    "\u{1F380}",                 // 🎀 (emoji-list.ts:82)
    "\u{1F947}",                 // 🥇 (emoji-list.ts:83)
    "\u{1F3C0}",                 // 🏀 (emoji-list.ts:84)
    "\u{1F3D0}",                 // 🏐 (emoji-list.ts:85)
    "\u{1F3B3}",                 // 🎳 (emoji-list.ts:86)
    "\u{1F3D3}",                 // 🏓 (emoji-list.ts:87)
    "\u{26F3}",                  // ⛳ (emoji-list.ts:88)
    "\u{1F3AF}",                 // 🎯 (emoji-list.ts:89)
    "\u{1F579}\u{FE0F}",         // 🕹️ (emoji-list.ts:90)
    "\u{1F9E9}",                 // 🧩 (emoji-list.ts:91)
    "\u{1F9F8}",                 // 🧸 (emoji-list.ts:92)
    "\u{2660}\u{FE0F}",          // ♠️ (emoji-list.ts:93)
    "\u{2665}\u{FE0F}",          // ♥️ (emoji-list.ts:94)
    "\u{1F457}",                 // 👗 (emoji-list.ts:95)
    "\u{1F451}",                 // 👑 (emoji-list.ts:96)
    "\u{1F514}",                 // 🔔 (emoji-list.ts:97)
    "\u{1F3B7}",                 // 🎷 (emoji-list.ts:98)
    "\u{1F3B8}",                 // 🎸 (emoji-list.ts:99)
    "\u{1F5A8}\u{FE0F}",         // 🖨️ (emoji-list.ts:100)
    "\u{1F4F8}",                 // 📸 (emoji-list.ts:101)
    "\u{1F56F}\u{FE0F}",         // 🕯️ (emoji-list.ts:102)
    "\u{1F4D6}",                 // 📖 (emoji-list.ts:103)
    "\u{1F4E6}",                 // 📦 (emoji-list.ts:104)
    "\u{1F4EE}",                 // 📮 (emoji-list.ts:105)
    "\u{1F4DD}",                 // 📝 (emoji-list.ts:106)
    "\u{1F4BC}",                 // 💼 (emoji-list.ts:107)
    "\u{1F4CB}",                 // 📋 (emoji-list.ts:108)
    "\u{1F512}",                 // 🔒 (emoji-list.ts:109)
    "\u{1F511}",                 // 🔑 (emoji-list.ts:110)
    "\u{2692}\u{FE0F}",          // ⚒️ (emoji-list.ts:111)
    "\u{1FA83}",                 // 🪃 (emoji-list.ts:112)
    "\u{2696}\u{FE0F}",          // ⚖️ (emoji-list.ts:113)
    "\u{1F517}",                 // 🔗 (emoji-list.ts:114)
    "\u{1FA9D}",                 // 🪝 (emoji-list.ts:115)
    "\u{1F52C}",                 // 🔬 (emoji-list.ts:116)
    "\u{1FA91}",                 // 🪑 (emoji-list.ts:117)
    "\u{1F6BD}",                 // 🚽 (emoji-list.ts:118)
    "\u{1F9F9}",                 // 🧹 (emoji-list.ts:119)
    "\u{1FAA3}",                 // 🪣 (emoji-list.ts:120)
    "\u{1FAE7}",                 // 🫧 (emoji-list.ts:121)
    "\u{26AB}",                  // ⚫ (emoji-list.ts:122)
    "\u{1F7E8}",                 // 🟨 (emoji-list.ts:123)
    "\u{25B6}\u{FE0F}",          // ▶️ (emoji-list.ts:124)
    "\u{1F4F6}",                 // 📶 (emoji-list.ts:125)
    "\u{1F4A5}",                 // 💥 (emoji-list.ts:126)
    "\u{1F4AC}",                 // 💬 (emoji-list.ts:127)
    "\u{1F4AB}",                 // 💫 (emoji-list.ts:128)
    "\u{1F440}",                 // 👀 (emoji-list.ts:129)
    "\u{1F463}",                 // 👣 (emoji-list.ts:130)
];

/// The three emoji for a rendezvous path hash: `EMOJI[byte % 128]` of its first three bytes (Desktop:
/// `LinkingEmojiLoader.svelte` with `rph[0]`, `rph[1]` and `rph[2]`).
pub(crate) fn emoji_of(rph: &[u8; 32]) -> [&'static str; 3] {
    std::array::from_fn(|i| EMOJI[usize::from(rph[i]) % 128])
}

#[cfg(test)]
mod tests {
    use sha2::{Digest as _, Sha256};

    use super::{EMOJI, emoji_of};

    /// The count is 128: a factor of 256, as Desktop's comment demands, and `% 128` is `& 0x7f`.
    #[test]
    fn there_are_128_emoji_and_no_duplicate() {
        assert_eq!(EMOJI.len(), 128);
        let mut seen = std::collections::HashSet::new();
        for glyph in EMOJI {
            assert!(!glyph.is_empty(), "an entry is empty");
            assert!(seen.insert(glyph), "an entry appears twice");
        }
    }

    /// The first and the last five entries against literals copied from Desktop's `emoji-list.ts`: lines 3-7
    /// (`1F435`, `1F436`, `1F429`, `1F98A`, `1F408 200D 2B1B`, the black cat is a zero-width-joiner sequence)
    /// and lines 126-130 (`1F4A5`, `1F4AC`, `1F4AB`, `1F440`, `1F463`).
    #[test]
    fn the_first_and_last_five_are_desktops() {
        let first = [
            "\u{1F435}",
            "\u{1F436}",
            "\u{1F429}",
            "\u{1F98A}",
            "\u{1F408}\u{200D}\u{2B1B}",
        ];
        let last = [
            "\u{1F4A5}",
            "\u{1F4AC}",
            "\u{1F4AB}",
            "\u{1F440}",
            "\u{1F463}",
        ];
        assert_eq!(
            EMOJI[..5],
            first,
            "the first five differ from emoji-list.ts:3-7"
        );
        assert_eq!(
            EMOJI[123..],
            last,
            "the last five differ from emoji-list.ts:126-130"
        );
    }

    /// The whole table: the entries' code points written as Desktop writes them (upper-case hex, at least four
    /// digits, joined by `-`, one entry per line) hash to the digest of `emoji-list.ts:3-130` in the same
    /// notation. The digest comes from (run in the Desktop checkout):
    ///
    /// ```text
    /// python3 -I -c 'import re,hashlib; rows=[re.findall(r"\x27([0-9A-F]+)\x27",l) for l in open(
    /// "apps/desktop/src/app/ui/linking/emoji-list.ts") if l.strip().startswith("[")]; print(hashlib.sha256(
    /// "".join("-".join(r)+"\n" for r in rows if r).encode()).hexdigest())'
    /// ```
    #[test]
    fn the_whole_table_is_desktops() {
        let mut text = String::new();
        for glyph in EMOJI {
            let points: Vec<String> = glyph
                .chars()
                .map(|c| format!("{:04X}", u32::from(c)))
                .collect();
            text.push_str(&points.join("-"));
            text.push('\n');
        }
        let digest = Sha256::digest(text.as_bytes());
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex, "87fe3a45a69448bd7b5c453bba7ee1bdd99f40eac208172b94a5f0d779ff38da",
            "the table differs from emoji-list.ts"
        );
    }

    /// The byte maps by `% 128`: 0 and 128 are the first entry, 0x7f the last.
    #[test]
    fn a_byte_maps_modulo_128() {
        let mut rph = [0_u8; 32];
        rph[1] = 0x7f;
        rph[2] = 0x80;
        assert_eq!(emoji_of(&rph), [EMOJI[0], EMOJI[127], EMOJI[0]]);
        rph[0] = 0xff;
        rph[1] = 0x81;
        rph[2] = 0xfe;
        assert_eq!(emoji_of(&rph), [EMOJI[127], EMOJI[1], EMOJI[126]]);
    }

    /// One fixed path hash: the one of libthreema's P3 known-answer test (`d2d_rendezvous/rxdtk.rs`, the hash of
    /// the STK `00 01 02 .. 1f`), `4e84f0da 1921ba57 ..`. Only its first three bytes count, worked out by hand:
    /// `0x4e` = 78 -> entry 78 = `emoji-list.ts:81` (`1F388`, balloon); `0x84` = 132 = 128 + 4 -> entry 4 =
    /// `emoji-list.ts:7` (`1F408 200D 2B1B`, black cat); `0xf0` = 240 = 128 + 112 -> entry 112 = `emoji-list.ts:115`
    /// (`1FA9D`, hook).
    #[test]
    fn a_fixed_path_hash_gives_three_known_emoji() {
        let rph: [u8; 32] = [
            0x4e, 0x84, 0xf0, 0xda, 0x19, 0x21, 0xba, 0x57, 0x58, 0xfb, 0x6f, 0xf9, 0xee, 0x6d,
            0x84, 0x50, 0x3e, 0x01, 0xc4, 0xd2, 0x28, 0x28, 0xe4, 0x88, 0x60, 0x0c, 0x4e, 0x21,
            0x48, 0x47, 0x46, 0xcc,
        ];
        assert_eq!(
            emoji_of(&rph),
            ["\u{1F388}", "\u{1F408}\u{200D}\u{2B1B}", "\u{1FA9D}"]
        );
    }

    /// Bytes after the third do not count.
    #[test]
    fn only_the_first_three_bytes_count() {
        let mut rph = [7_u8; 32];
        let before = emoji_of(&rph);
        for byte in &mut rph[3..] {
            *byte = 0xa5;
        }
        assert_eq!(emoji_of(&rph), before);
    }
}
