#include "kourier-qt/cpp/kde.h"

#include <QtCore/QStandardPaths>
#include <QtGui/QGuiApplication>
#include <QtGui/QIcon>

#include <KAboutData>
#include <KCrash>
#include <KLocalizedQmlContext>
#include <KLocalizedString>
#include <KirigamiAppDefaults>

namespace kourier::kde {

void setAboutData(const QString& componentName,
                  const QString& displayName,
                  const QString& version,
                  const QString& summary,
                  const QString& spdx,
                  const QString& copyright,
                  const QString& homepage,
                  const QString& bugAddress,
                  const QString& desktopFileName,
                  const QString& programLogo,
                  const QStringList& authors,
                  const QStringList& components) {
    // (The long constructor: its default bug address is KDE's bug tracker.)
    KAboutData about(componentName, displayName, version, summary, KAboutLicense::Unknown,
                     copyright, QString(), homepage, bugAddress);
    about.setOrganizationDomain(QByteArray());
    about.setDesktopFileName(desktopFileName);
    if (!programLogo.isEmpty()) {
        // A string, not a QIcon: the About page's icon takes it as a name or a URL, and
        // nothing is read from the file until the page is opened.
        about.setProgramLogo(programLogo);
    }
    const KAboutLicense license = KAboutLicense::byKeyword(spdx);
    if (license.key() == KAboutLicense::Custom) {
        // REUSE-IgnoreStart (the text shown on the About page, not a licence tag of this file)
        about.setLicenseText(QStringLiteral("SPDX-License-Identifier: %1").arg(spdx));
        // REUSE-IgnoreEnd
    } else {
        about.setLicense(license.key());
    }
    for (qsizetype i = 0; i + 2 < authors.size(); i += 3) {
        about.addAuthor(authors[i], authors[i + 1], authors[i + 2]);
    }
    for (qsizetype i = 0; i + 2 < components.size(); i += 3) {
        const KAboutLicense used = KAboutLicense::byKeyword(components[i + 2]);
        if (used.key() == KAboutLicense::Custom) {
            about.addComponent(components[i], components[i + 2], components[i + 1]);
        } else {
            about.addComponent(components[i], QString(), components[i + 1], QString(),
                               used.key());
        }
    }
    KAboutData::setApplicationData(about);
}

bool hasInstalledIcon(const QString& name) {
    const QString file = QStringLiteral("icons/hicolor/scalable/apps/") + name + QStringLiteral(".svg");
    return !QStandardPaths::locate(QStandardPaths::GenericDataLocation, file).isEmpty();
}

void applyDefaults(const QString& iconName) {
    KirigamiAppDefaults::apply(qGuiApp);
    if (!iconName.isEmpty()) {
        QGuiApplication::setWindowIcon(QIcon::fromTheme(iconName));
    }
}

void setWindowIconFile(const QString& path) {
    QGuiApplication::setWindowIcon(QIcon(path));
}

bool hasWindowIcon() {
    return !QGuiApplication::windowIcon().isNull();
}

void setupLocalizedContext(QQmlApplicationEngine& engine, rust::Str domain) {
    KLocalizedQmlContext* context = KLocalization::setupLocalizedContext(&engine);
    context->setTranslationDomain(QString::fromUtf8(domain.data(), qsizetype(domain.size())));
}

void initializeCrashHandler() {
    KCrash::initialize();
}

void setCrashMessage(rust::Str line) {
    KCrash::setErrorMessage(QString::fromUtf8(line.data(), qsizetype(line.size())));
}

} // namespace kourier::kde
