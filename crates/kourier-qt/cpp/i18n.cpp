#include "kourier-qt/cpp/i18n.h"

#include <QtCore/QByteArray>

#include <KLocalizedString>

namespace kourier::i18n {

namespace {

// KI18n takes NUL-terminated strings and copies them.
QByteArray bytes(rust::Str text) {
    return QByteArray(text.data(), qsizetype(text.size()));
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
    // No argument is given: `relaxSubs` leaves the placeholders in place without a complaint.
    return message.relaxSubs().toString();
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
    return message.relaxSubs().subs(qulonglong(count)).toString();
}

void addLocaleDir(rust::Str domain, const QString& dir) {
    KLocalizedString::addDomainLocaleDir(bytes(domain), dir);
}

void setLanguages(const QStringList& languages) {
    KLocalizedString::setLanguages(languages);
}

} // namespace kourier::i18n
