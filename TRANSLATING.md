# Translating Kourier

Kourier uses gettext, the system KDE applications use too. There is one catalog, the gettext
domain `kourier`, for everything you see: the interface, the notification texts, the name and
summary in the application menu and in software centres. The template is
[`po/kourier.pot`](po/kourier.pot); a translation is a file `po/<language>.po`.

> TODO(maintainer): the project is not hosted yet. When it is, a translation platform
> (Hosted Weblate is planned) is linked here and most translators will not need anything
> below except the sections on placeholders, plural forms and context. Until then, a
> translation is a pull request that adds one `.po` file.

## Add a language by hand

You need `gettext` (`msgmerge`, `msgfmt`) and a text editor made for PO files (Lokalize,
Poedit, Gtranslator) or any text editor.

1. Pick the language code the way glibc and KDE spell it: `de`, `uk`, `pt_BR`, `zh_CN`,
   `sr@latin`. Not `pt-BR`.
2. Copy the template and fill in the header. `msginit` does both:

   ```sh
   msginit --no-translator -i po/kourier.pot -l uk -o po/uk.po
   ```

   Check the `Plural-Forms` line in the header. `msginit` knows the rule of most languages;
   the full list is in the gettext manual ("Plural forms"). Do not copy the line of another
   program without checking: KDE's Ukrainian files, for example, use four forms.
3. List the language in `po/LINGUAS`, one code per line.
4. Translate. When the template changes, `cargo xtask i18n extract` (which developers run
   after changing a string) merges it into every language of `po/LINGUAS`: a new string
   arrives untranslated, a changed one is marked `fuzzy` with its old source beside it. The
   application shows English for both until you have looked. By hand it is:

   ```sh
   msgmerge --update --backup=none --previous --no-wrap --add-location=file po/uk.po po/kourier.pot
   ```
5. Check it:

   ```sh
   cargo xtask i18n check      # msgfmt -c on your file, and the placeholders of every string
   ```

## See your translation

```sh
cargo xtask i18n build       # po/*.po of LINGUAS -> target/locale/<lang>/LC_MESSAGES/kourier.mo
cargo build                  # a debug build looks for the catalogs by the variable below
LANGUAGE=uk LC_ALL=en_US.UTF-8 KOURIER_LOCALE_DIR=$PWD/target/locale \
    target/debug/kourier --fixture --page=chat
```

- `--fixture` runs on sample data and touches no account; `--page=<name>` opens one page
  (`chat`, `list`, `settings`, `files`, …).
- `LANGUAGE` is the language to show, and `LANGUAGE=uk:de` falls back to German for what is
  not translated. gettext translates nothing while the locale is `C`, `POSIX` or `C.UTF-8`:
  `LC_ALL` must name a UTF-8 locale that `locale -a` lists (`en_US.UTF-8` is only an example).
- `KOURIER_LOCALE_DIR` is read by debug builds only. An installed Kourier reads the system's
  `share/locale`; `cargo xtask install --prefix <dir>` puts the catalogs of `po/LINGUAS`
  there.

### The pseudo-language

`cargo xtask i18n build` also writes a language that needs no translator, `x-test`. Every
message is wrapped as `xx` + text + `xx`, placeholders intact, so a string without the wrapper
is not translatable at all (it was written into the code or QML without `i18n`), and a
string that is cut off or overflows shows up as soon as it is longer:

```sh
LANGUAGE=x-test LC_ALL=en_US.UTF-8 KOURIER_LOCALE_DIR=$PWD/target/locale \
    target/debug/kourier --fixture --page=settings
```

### Pictures of a language

```sh
cargo xtask shots --matrix --variant uk,x-test     # into target/shots-matrix/<variant>/
```

renders five pages (conversation list, chat, composer with files, settings, a long text),
wide and narrow, in Ukrainian and in the pseudo-language, without opening a window. Another
language needs its own entry in `VARIANTS` (`xtask/src/shots.rs`).

## What a string looks like

```po
#. TRANSLATORS: %1 is one name or several joined with commas, %2 the last name.
#: crates/kourier-viewmodel/src/preview.rs
#, kde-format
msgctxt "@info a list of names, %1 and %2 names"
msgid "%1 and %2"
msgstr ""
```

- **`#.` lines** are notes from the developers. Read them.
- **`msgctxt`** says what the string is and where it is shown. It starts with a KDE marker:

  | Marker | The text is |
  |---|---|
  | `@title`, `@title:window`, `@title:tab` | a heading, a page or window title, a tab |
  | `@action:button`, `@action:inmenu` | a button, a menu entry |
  | `@info`, `@info:status`, `@info:tooltip` | a message, a line of status, a tooltip |
  | `@info:preview` | the one-line preview of a message in the list |
  | `@info:placeholder` | the grey hint in an empty field |
  | `@label` | the label of a field or a setting |
  | `@item:inlistbox`, `@item:intext` | one entry of a list, a word used inside a sentence |

  The words after the marker explain the string ("`%1` how many files"). The same English
  word in two contexts is two strings and can be two words in your language.
- **Capital letters:** buttons, menu entries and titles are in Title Case in English
  ("Add Account"); labels, messages and tooltips in sentence case. Follow your language's own
  convention for the same kinds of text; in most languages that is sentence case everywhere.
- **`kde-format`** is a flag: `msgfmt -c` checks that you keep the placeholders.

## Placeholders: do not translate them

`%1`, `%2` … `%9` are replaced by a name, a number, a size or another translated text when
the string is shown.

- Keep every placeholder of the English text, spelled the same way, and add none. You may
  move them: `"%1 added %2"` can become `"%2 was added by %1"`.
- A placeholder is the percent sign and a digit. `%1%` is the value `%1` and a percent sign.
- Do not put spaces or punctuation inside or around them that the English text lacks, unless
  your language needs them.
- Names (`Signal`, `Kourier`) are not translated. The name of the application in the
  `Name=` line of the menu entry is kept as it is.
- Text that was glued from pieces ("`%1 file was added from`" + "“Photos”.") carries a
  `TRANSLATORS` note that says how the pieces are joined; translate each piece so that they
  read right in that order.

## Plural forms

A message with a number comes as `msgid` / `msgid_plural` with one `msgstr[n]` per form of
your language:

```po
msgctxt "@info a length of time"
msgid "%1 hour"
msgid_plural "%1 hours"
msgstr[0] ""
msgstr[1] ""
```

- `%1` in a plural message is the number. How many forms there are, and which number uses
  which, is the `Plural-Forms` line of your file's header. Translate every form.
- A form that is used for exactly one number may say it in words ("one hour") and leave out
  `%1`; `cargo xtask i18n check` allows that for `%1` only. Look at your `Plural-Forms`
  first: in the Slavic languages the form for 1 is also the form for 21, 31, 101, so it
  must keep `%1`.
- When the English singular has no number ("Send a Folder" / "Send %1 Folders"), a form
  that is used for 1 only is translated like it; a form that also serves 21 says the number.
- A text that differs for one and for several without showing the number is two messages
  in Kourier, not a plural (their contexts say "the one file" and "two or more").

## The menu entry and the software-centre text

The `Name`, `GenericName`, `Comment` and `Keywords` of the desktop file and the summary and
description of the AppStream metainfo are in the same catalog (their `#:` lines name
`packaging/`). Keep the `;` that ends every keyword in the `Keywords` string
("chat;messenger;signal;"). These strings have no context, as gettext's tools for those files
expect.

## For developers

- A string in QML is `L10n.i18nc("@context", "Text %1", arg)` (`i18ncp` for a plural); in Rust
  `trc`, `trnc` or `trfc` of `kourier_viewmodel::i18n` (see its module documentation).
  There is no call without a context, no `qsTr`, no KUIT tags (`xi18n*`, `<emphasis>`), no
  `{0}` or `{n}`.
- `cargo xtask i18n extract` rewrites `po/kourier.pot`; commit it with the change. `cargo xtask
  i18n check` runs in `cargo xtask ci` (both tiers) and fails when the template is behind,
  when a string has no context, uses KUIT tags or `{…}`, or when a translation of
  `po/LINGUAS` does not compile or changes the placeholders.
- A `// TRANSLATORS: …` comment above a call (in Rust or in QML) goes into the catalog for
  the translators. Put one on every string that is glued from parts or
  whose placeholders are not obvious.
- Rust texts in a `const` table are marked with `trc_noop` where the table is written and
  translated with `trm` where it is used.

## Licence

A translation is part of Kourier and is distributed under the AGPL-3.0-only, like the rest.
By sending it you agree to that (the sign-off of [CONTRIBUTING.md](CONTRIBUTING.md) covers it).
