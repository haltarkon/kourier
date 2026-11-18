#include "kourier-qt/cpp/i18n.h"

#include <QtCore/QByteArray>

#include <KLocalizedString>

#include <algorithm>

namespace kourier::i18n {

namespace {

// KI18n takes NUL-terminated strings and copies them.
QByteArray bytes(rust::Str text) {
    return QByteArray(text.data(), qsizetype(text.size()));
}

// The highest of `%1`…`%9` in `text` (0: none), read as `kourier_viewmodel::i18n` reads
// them: a digit after `%` that no other digit follows.
int highestPlaceholder(const QByteArray& text) {
    const auto digit = [](char c) { return c >= '0' && c <= '9'; };
    int highest = 0;
    for (qsizetype i = 0; i + 1 < text.size(); ++i) {
        const char d = text.at(i + 1);
        if (text.at(i) == '%' && d >= '1' && d <= '9'
            && (i + 2 >= text.size() || !digit(text.at(i + 2)))) {
            highest = std::max(highest, d - '0');
        }
    }
    return highest;
}

// The placeholders `%first`…`%last` are given themselves as arguments, so they come back as
// they are. Leaving them without one is not enough: a KI18n built without NDEBUG (Arch and
// Manjaro build with CMake's build type "None") writes "(I18N_ARGUMENT_MISSING)" after every
// placeholder that has no argument, `relaxSubs` or not; neon's NDEBUG build does not.
KLocalizedString keepPlaceholders(KLocalizedString message, int first, int last) {
    for (int n = first; n <= last; ++n) {
        message = message.subs(QLatin1Char('%') + QString::number(n));
    }
    return message;
}

} // namespace

void setApplicationDomain(rust::Str domain) {
    KLocalizedString::setApplicationDomain(bytes(domain));
}

QString translate(rust::Str domain, rust::Str context, rust::Str msgid) {
    const QByteArray d = bytes(domain);
    const QByteArray c = bytes(context);
    const QByteArray text = bytes(msgid);
    const KLocalizedString message = c.isEmpty()
        ? ki18nd(d.constData(), text.constData())
        : ki18ndc(d.constData(), c.constData(), text.constData());
    // `relaxSubs`: a translation may leave out a placeholder of the English text.
    return keepPlaceholders(message.relaxSubs(), 1, highestPlaceholder(text)).toString();
}

QString translatePlural(rust::Str domain,
                        rust::Str context,
                        rust::Str singular,
                        rust::Str plural,
                        uint64_t count) {
    const QByteArray d = bytes(domain);
    const QByteArray c = bytes(context);
    const QByteArray one = bytes(singular);
    const QByteArray many = bytes(plural);
    const KLocalizedString message = c.isEmpty()
        ? ki18ndp(d.constData(), one.constData(), many.constData())
        : ki18ndcp(d.constData(), c.constData(), one.constData(), many.constData());
    const int highest = std::max(highestPlaceholder(one), highestPlaceholder(many));
    return keepPlaceholders(message.relaxSubs().subs(qulonglong(count)), 2, highest).toString();
}

QString formatNumber(uint64_t n) {
    return QStringLiteral("%L1").arg(qulonglong(n));
}

void addLocaleDir(rust::Str domain, const QString& dir) {
    KLocalizedString::addDomainLocaleDir(bytes(domain), dir);
}

void setLanguages(const QStringList& languages) {
    KLocalizedString::setLanguages(languages);
}

} // namespace kourier::i18n
