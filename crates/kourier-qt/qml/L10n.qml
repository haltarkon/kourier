pragma Singleton

import org.kde.ki18n

// Translations for QML strings: `L10n.i18nc("@context", "Text")` on the `kourier` gettext
// domain (§9.5). One shared context for the whole UI.
KI18nContext {
    translationDomain: "kourier"
}
