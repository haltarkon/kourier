// Translations through KI18n (architecture §9.5): the lookups behind the Rust `Translator` of
// this front-end (`src/l10n.rs`). A lookup returns the translated template; the arguments of a
// message are filled in by `kourier_viewmodel::i18n` afterwards, so they never pass through
// here. KLocalizedString is thread-safe: these are called from the runtime's threads too.
#pragma once

#include <QtCore/QString>
#include <QtCore/QStringList>

#include <cstdint>

#include "rust/cxx.h"

namespace kourier::i18n {

// Makes `domain` the application's gettext domain (`KLocalizedString::setApplicationDomain`).
void setApplicationDomain(rust::Str domain);

// The translation of `msgid` in `domain`, with the disambiguating `context` unless it is
// empty; `msgid` itself when no catalog has it. Placeholders come back as they are.
QString translate(rust::Str domain, rust::Str context, rust::Str msgid);

// The plural form of the translation for `count`. KI18n takes the count to choose the form
// and writes it into `%1` itself; every other placeholder comes back as it is.
QString translatePlural(rust::Str domain,
                        rust::Str context,
                        rust::Str singular,
                        rust::Str plural,
                        uint64_t count);

// Tests: catalogs of `domain` are also looked for under `dir`
// (`<dir>/<language>/LC_MESSAGES/<domain>.mo`), and the languages to translate into.
void addLocaleDir(rust::Str domain, const QString& dir);
void setLanguages(const QStringList& languages);

} // namespace kourier::i18n
