#include "kourier-qt/cpp/desktop.h"

#include <QtCore/QHash>
#include <QtCore/QStringList>
#include <QtGui/QColor>
#include <QtGui/QFont>
#include <QtGui/QGuiApplication>
#include <QtGui/QIcon>
#include <QtGui/QImage>
#include <QtGui/QPainter>
#include <QtGui/QPainterPath>
#include <QtGui/QPixmap>

#include <dlfcn.h>
#include <malloc.h>

namespace kourier {

namespace {

// Kirigami Add-ons' NameUtils (libKirigamiAddonsComponents, loaded with its QML plugin): the
// same colors and initials as the in-app Avatar. Looked up at run time; a missing library
// falls back to a local palette.
using ColorFn = QColor (*)(const QString&);
using InitialsFn = QString (*)(const QString&);
using UnsuitableFn = bool (*)(const QString&);

struct NameUtils {
    ColorFn color = nullptr;
    InitialsFn initials = nullptr;
    UnsuitableFn unsuitable = nullptr;
};

const NameUtils& nameUtils()
{
    static const NameUtils utils = [] {
        NameUtils u;
        const char* lib = "libKirigamiAddonsComponents.so.6";
        void* handle = dlopen(lib, RTLD_LAZY | RTLD_NOLOAD);
        if (handle == nullptr) {
            handle = dlopen(lib, RTLD_LAZY);
        }
        if (handle == nullptr) {
            return u;
        }
        u.color = reinterpret_cast<ColorFn>(dlsym(handle, "_ZN9NameUtils16colorsFromStringERK7QString"));
        u.initials = reinterpret_cast<InitialsFn>(dlsym(handle, "_ZN9NameUtils18initialsFromStringERK7QString"));
        u.unsuitable = reinterpret_cast<UnsuitableFn>(
            dlsym(handle, "_ZN9NameUtils29isStringUnsuitableForInitialsERK7QString"));
        return u;
    }();
    return utils;
}

QColor fallbackColor(const QString& key)
{
    static const QColor palette[] = {
        QColor(0xe93a9a), QColor(0xe93d58), QColor(0xe9643a), QColor(0xef973c),
        QColor(0xe8cb2d), QColor(0xb6e521), QColor(0x3dd425), QColor(0x00d485),
        QColor(0x00d3b8), QColor(0x3daee9), QColor(0xb875dc), QColor(0x926ee4),
    };
    return palette[qHash(key) % (sizeof(palette) / sizeof(palette[0]))];
}

QString fallbackInitials(const QString& name)
{
    const QStringList words = name.simplified().split(QLatin1Char(' '), Qt::SkipEmptyParts);
    QString out;
    for (const QString& w : words) {
        if (out.size() >= 2) {
            break;
        }
        if (w.at(0).isLetterOrNumber()) {
            out.append(w.at(0).toUpper());
        }
    }
    return out;
}

// The outline of an avatar of `size` pixels: a circle, or a square with corners of `corner`
// pixels when that is less than half the side.
QPainterPath avatarShape(int size, int corner)
{
    const qreal s = size;
    QPainterPath shape;
    if (corner <= 0) {
        shape.addRect(QRectF(0, 0, s, s));
    } else if (corner * 2 >= size) {
        shape.addEllipse(QRectF(0, 0, s, s));
    } else {
        shape.addRoundedRect(QRectF(0, 0, s, s), corner, corner);
    }
    return shape;
}

void drawPerson(QPainter& p, int size, const QPainterPath& shape)
{
    const qreal s = size;
    QPainterPath body;
    body.addEllipse(QPointF(s * 0.5, s * 0.40), s * 0.16, s * 0.16);
    QPainterPath shoulders;
    shoulders.addEllipse(QPointF(s * 0.5, s * 0.92), s * 0.30, s * 0.26);
    p.fillPath(body.united(shoulders).intersected(shape), Qt::white);
}

} // namespace

void setBadgeNumber(int64_t number)
{
    if (auto* app = qobject_cast<QGuiApplication*>(QCoreApplication::instance())) {
        app->setBadgeNumber(number);
    }
}

void setQuitOnLastWindowClosed(bool quit)
{
    QGuiApplication::setQuitOnLastWindowClosed(quit);
}

rust::Vec<uint8_t> renderIconArgb(const QString& path, int32_t size, bool dot)
{
    rust::Vec<uint8_t> out;
    if (size <= 0 || size > 512) {
        return out;
    }
    const QIcon icon(path);
    if (icon.isNull()) {
        return out;
    }
    QImage image(size, size, QImage::Format_ARGB32_Premultiplied);
    image.fill(Qt::transparent);
    {
        QPainter p(&image);
        p.setRenderHint(QPainter::Antialiasing);
        icon.paint(&p, QRect(0, 0, size, size));
        if (dot) {
            const qreal d = size * 0.42;
            const QRectF r(size - d, size - d, d, d);
            // Breeze's attention red: the palette highlight is blue like the icon itself and
            // would vanish at tray sizes.
            p.setPen(QPen(Qt::white, qMax(1.0, size / 16.0)));
            p.setBrush(QColor(0xda, 0x44, 0x53));
            p.drawEllipse(r.adjusted(0.5, 0.5, -0.5, -0.5));
        }
    }
    const QImage argb = image.convertToFormat(QImage::Format_ARGB32);
    out.reserve(static_cast<size_t>(size) * static_cast<size_t>(size) * 4);
    for (int y = 0; y < size; ++y) {
        const auto* line = reinterpret_cast<const QRgb*>(argb.constScanLine(y));
        for (int x = 0; x < size; ++x) {
            const QRgb px = line[x];
            out.push_back(static_cast<uint8_t>(qAlpha(px)));
            out.push_back(static_cast<uint8_t>(qRed(px)));
            out.push_back(static_cast<uint8_t>(qGreen(px)));
            out.push_back(static_cast<uint8_t>(qBlue(px)));
        }
    }
    return out;
}

rust::Vec<uint8_t> renderAvatarRgba(const QString& name, bool anonymous, int32_t size, int32_t corner)
{
    rust::Vec<uint8_t> out;
    if (size <= 0 || size > 512) {
        return out;
    }
    const NameUtils& utils = nameUtils();
    const QString key = anonymous ? name.right(4) : name;
    const QColor color = utils.color != nullptr ? utils.color(key) : fallbackColor(key);
    QString initials;
    const bool unsuitable = anonymous || name.trimmed().isEmpty()
        || (utils.unsuitable != nullptr && utils.unsuitable(name));
    if (!unsuitable) {
        initials = utils.initials != nullptr ? utils.initials(name) : fallbackInitials(name);
    }

    QImage image(size, size, QImage::Format_ARGB32_Premultiplied);
    image.fill(Qt::transparent);
    {
        QPainter p(&image);
        p.setRenderHint(QPainter::Antialiasing);
        p.setRenderHint(QPainter::TextAntialiasing);
        p.setPen(Qt::NoPen);
        p.setBrush(color);
        const QPainterPath shape = avatarShape(size, corner);
        p.drawPath(shape);
        if (initials.isEmpty()) {
            drawPerson(p, size, shape);
        } else {
            QFont font;
            font.setPixelSize(qMax(1, static_cast<int>(size * 0.40)));
            font.setWeight(QFont::DemiBold);
            p.setFont(font);
            p.setPen(Qt::white);
            p.drawText(QRect(0, 0, size, size), Qt::AlignCenter, initials);
        }
    }
    const QImage rgba = image.convertToFormat(QImage::Format_RGBA8888);
    const auto bytes = static_cast<size_t>(size) * static_cast<size_t>(size) * 4;
    out.reserve(bytes);
    for (int y = 0; y < size; ++y) {
        const uchar* line = rgba.constScanLine(y);
        for (int x = 0; x < size * 4; ++x) {
            out.push_back(line[x]);
        }
    }
    return out;
}

void trimMemory()
{
    malloc_trim(0);
}

} // namespace kourier
