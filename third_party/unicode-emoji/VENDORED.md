# Unicode emoji data, vendored

The inputs of `cargo xtask emoji`, which writes `crates/kourier-viewmodel/src/emoji/table.rs` (the
emoji the pickers offer, with their names and search words in English and Ukrainian). Public data;
nothing of the user's is sent anywhere to fetch it. Licence: Unicode License v3 (`Unicode-3.0`,
`LICENSE` here, `LICENSES/Unicode-3.0.txt`), copyright Unicode, Inc.

## Files

| File | Source | Version |
|---|---|---|
| `emoji-test.txt` | `https://unicode.org/Public/emoji/latest/emoji-test.txt` (fetched 2026-11-12) | Emoji 18.0, file date 2026-04-30 |
| `annotations-en.xml`, `annotations-uk.xml` | `https://raw.githubusercontent.com/unicode-org/cldr/release-49/common/annotations/{en,uk}.xml` | CLDR 49 (tag `release-49`) |
| `annotationsDerived-en.xml`, `annotationsDerived-uk.xml` | the same tag, `common/annotationsDerived/{en,uk}.xml` | CLDR 49 |
| `LICENSE` | the tag's `LICENSE` (the Unicode License v3 text follows the UTS #35 notice) | CLDR 49 |

sha256 (`sha256sum`):

```
8f3735cda1f92a779d78af67cf86066bb1f07143dc22f2ac29394d9bc57ab21a  emoji-test.txt
14d0e9bb5003c085250df4ac7bd7ba1e26d92e74174d7a3a2f42dc16e07fb155  annotations-en.xml
1247fc11f058a65a7c63c1766d611b58e5f22d3ec6e47d8656e195b1c1ac70cc  annotations-uk.xml
a4a6c1a4c341d94179cb116623c5f44c47400a5bc55da483423ef7430ecf622e  annotationsDerived-en.xml
387eb4b9d70cb7e049856b3cba2e504610080c5d9889351f7a07d22826c46d1f  annotationsDerived-uk.xml
65eb33419e22a297550a811b5b535f5e532f9a7e0114b516c0ea18afd53c3804  LICENSE
```

## What the generator keeps

- Fully-qualified sequences only; the `Component` group, every skin-tone sequence (U+1F3FB..1F3FF) and
  every hair-component sequence (U+1F9B0..1F9B3) are left out.
- **Emoji version cap: 16.0** (`EMOJI_VERSION_CAP` in the table). The system font, Noto Color Emoji
  2.047 (fonts-noto-color-emoji 2.047-0ubuntu0.24.04.1, the same on the PC and the builder), draws
  all 1,906 tone-free sequences up to Emoji 16.0 (checked 2026-11-12 on its cmap and GSUB ligatures,
  FE0F ignored as the font's own `ccmp` does) and none of the new single emoji of 17.0 and 18.0
  (U+1FAEA, 1FAEF, 1FAC8, 1FACD, 1FA8A ...). To raise the cap: install a font that draws the newer
  version, re-run the check, change `EMOJI_VERSION_CAP` in `xtask/src/emoji.rs`, run
  `cargo xtask emoji`.
- Names and search words: `annotations` first, `annotationsDerived` for what it lacks (U+FE0F is
  removed from every `cp` there). An emoji CLDR has no Ukrainian name for shows its English name
  and is found by its English words only.

## Updating

Fetch the new files, update the table above and `cargo xtask emoji`; the unit test
`emoji::tests::the_checked_in_table_is_what_the_generator_writes` fails until the table is regenerated.
