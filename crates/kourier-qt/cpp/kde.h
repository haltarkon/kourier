// The KDE Frameworks side of the Qt front-end (architecture §8.10): the defaults of a Kirigami
// application, the application's `KAboutData`, the KI18n context of the QML engine and KCrash.
// Everything of KDE that is linked is called from here and from `i18n.cpp`; what crosses into
// Rust is strings. No logic beyond KDE and Qt calls.
#pragma once

#include <QtCore/QString>
#include <QtCore/QStringList>
#include <QtQml/QQmlApplicationEngine>

#include "rust/cxx.h"

namespace kourier::kde {

// Makes `about` the application's data (`KAboutData::setApplicationData`): the About page, the
// crash reporter and Qt's application name, version, display name and desktop file name read
// it. `spdx` is the licence as an SPDX identifier; one KDE does not know is shown by that
// identifier. An empty `homepage` or `bugAddress` is not shown, and no organization domain is
// set (KDE's default would be kde.org). A non-empty `programLogo` (an icon name or a URL, kept
// as the string it is) is what the About page shows as the logo; without one it takes the
// window icon, then the icon theme's icon of the component name. `authors` holds name, task and
// e-mail address of each author in turn, `components` name, version and SPDX licence of each
// third-party component. Call it after the application object exists and before
// `applyDefaults`.
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
                  const QStringList& components);

// Whether an icon of this name is installed where every icon theme falls back to
// (`icons/hicolor/scalable/apps/<name>.svg` in one of the data directories): a few file
// lookups. Not `QIcon::hasThemeIcon`: for a name the theme lacks that walks every directory
// of the theme and of its parents, 10 ms and more at the start of a run that is not installed.
bool hasInstalledIcon(const QString& name);

// What KDE sets up for a Kirigami application (`KirigamiAppDefaults::apply`): the Qt Quick
// Controls style when neither the platform theme nor `QT_QUICK_CONTROLS_STYLE` chose one,
// Breeze as the fallback icon theme, and a colour scheme that follows the desktop also outside
// Plasma. Then the window icon, by its theme name `iconName` (empty: none; looking up a
// name the theme lacks is slow). Call it right after the application object was created.
void applyDefaults(const QString& iconName);

// The window icon from a file (a resource path): for a run that is not installed, whose icon
// theme has no icon of the application. Loading the file takes the SVG icon plugin, so this
// is called after the first frame, not at the start.
void setWindowIconFile(const QString& path);

// Whether the application has a window icon.
bool hasWindowIcon();

// Gives `engine` KI18n's context object, so QML's own `i18n*()` functions work in every file
// (Kirigami Add-ons call them), with `domain` as the application's catalog.
void setupLocalizedContext(QQmlApplicationEngine& engine, rust::Str domain);

// Starts KCrash: a crash from here on is handed to the desktop's crash reporter (DrKonqi).
void initializeCrashHandler();

// The line the crash reporter shows for a crash that follows (`KCrash::setErrorMessage`).
void setCrashMessage(rust::Str line);

} // namespace kourier::kde
