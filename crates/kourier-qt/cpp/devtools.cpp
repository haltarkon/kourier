#include "kourier-qt/cpp/devtools.h"

#include <QtCore/QList>
#include <QtCore/QStringList>
#include <QtCore/QPair>
#include <QtGui/QCursor>
#include <QtGui/QFont>
#include <QtGui/QImage>
#include <QtGui/QTextBlock>
#include <QtGui/QTextCursor>
#include <QtGui/QTextDocument>
#include <QtQuick/QQuickItem>
#include <QtQuick/QQuickTextDocument>
#include <QtQuick/QQuickWindow>

#include <QtCore/QByteArray>
#include <QtCore/QtGlobal>

#include <optional>
#include <utility>

namespace kourier {

namespace {
std::optional<rust::Fn<void(int32_t, rust::Str, rust::Str)>> g_sink;

void routeMessage(QtMsgType type, const QMessageLogContext& context, const QString& message)
{
    if (!g_sink) {
        return;
    }
    const QByteArray text = message.toUtf8();
    const char* category = context.category != nullptr ? context.category : "default";
    (*g_sink)(static_cast<int32_t>(type), rust::Str(category),
        rust::Str(text.constData(), static_cast<size_t>(text.size())));
}
} // namespace

void installMessageHandler(rust::Fn<void(int32_t, rust::Str, rust::Str)> sink)
{
    g_sink = sink;
    qInstallMessageHandler(routeMessage);
}

namespace {
// The previous grab of grabIfStable.
QImage g_previousGrab;

// Saves `image` as a PNG at `path` and returns its mean luminance, or -1.
int saveWithLuma(const QImage& image, const QString& path)
{
    if (image.isNull() || !image.save(path, "PNG")) {
        return -1;
    }
    // Mean Rec. 601 luma over a sparse grid: enough to tell a light theme from a dark one.
    const QImage rgb = image.convertToFormat(QImage::Format_RGB32);
    qint64 sum = 0;
    qint64 count = 0;
    for (int y = 0; y < rgb.height(); y += 4) {
        const auto* line = reinterpret_cast<const QRgb*>(rgb.constScanLine(y));
        for (int x = 0; x < rgb.width(); x += 4) {
            const QRgb p = line[x];
            sum += (299 * qRed(p) + 587 * qGreen(p) + 114 * qBlue(p)) / 1000;
            ++count;
        }
    }
    return count > 0 ? static_cast<int>(sum / count) : -1;
}

int loadingBelow(const QQuickItem* item)
{
    int loading = 0;
    // QQuickImageBase::Status: Null 0, Ready 1, Loading 2, Error 3.
    if (item->inherits("QQuickImageBase") && item->property("status").toInt() == 2) {
        ++loading;
    }
    for (const QQuickItem* child : item->childItems()) {
        loading += loadingBelow(child);
    }
    return loading;
}

// What `loadingBelow` counted, one entry per image: its name (the item's objectName, else its
// class), its parent's, whether it is visible, its size, its `source` and its `sourceSize`. The
// line `shot: unsettled images N [...]` of a render that never settled names the image that
// stalls (a hidden stand-in whose source changes looks the same as a picture that never came).
void describeLoading(const QQuickItem* item, QStringList& out)
{
    if (item->inherits("QQuickImageBase") && item->property("status").toInt() == 2) {
        const auto name = [](const QObject* o) {
            return o->objectName().isEmpty() ? QString::fromLatin1(o->metaObject()->className()) : o->objectName();
        };
        const QSize sourceSize = item->property("sourceSize").toSize();
        QString source = item->property("source").toUrl().toString();
        if (source.size() > 100) {
            source = source.left(100) + QStringLiteral("...");
        }
        const QQuickItem* parent = item->parentItem();
        out << QStringLiteral("%1 in %2, %3, %4x%5, source %6, sourceSize %7x%8")
                   .arg(name(item), parent != nullptr ? name(parent) : QStringLiteral("-"),
                       item->isVisible() ? QStringLiteral("visible") : QStringLiteral("hidden"))
                   .arg(qRound(item->width()))
                   .arg(qRound(item->height()))
                   .arg(source)
                   .arg(sourceSize.width())
                   .arg(sourceSize.height());
    }
    for (const QQuickItem* child : item->childItems()) {
        describeLoading(child, out);
    }
}

// The visible images below `item` that draw an attachment (`image://kourier/thumb/…` or
// `…/att/…`): the pictures whose URL changes when the core derives a thumbnail.
int attachmentPicturesBelow(const QQuickItem* item)
{
    int pictures = 0;
    if (item->isVisible() && item->inherits("QQuickImageBase")) {
        const QString source = item->property("source").toUrl().toString();
        if (source.startsWith(QLatin1String("image://kourier/thumb/"))
            || source.startsWith(QLatin1String("image://kourier/att/"))) {
            ++pictures;
        }
    }
    for (const QQuickItem* child : item->childItems()) {
        pictures += attachmentPicturesBelow(child);
    }
    return pictures;
}
} // namespace

int grabWindowToFile(QObject* window, const QString& path)
{
    auto* quickWindow = qobject_cast<QQuickWindow*>(window);
    if (quickWindow == nullptr) {
        return -1;
    }
    return saveWithLuma(quickWindow->grabWindow(), path);
}

int imagesLoading(QObject* window)
{
    auto* quickWindow = qobject_cast<QQuickWindow*>(window);
    if (quickWindow == nullptr) {
        return -1;
    }
    return loadingBelow(quickWindow->contentItem());
}

QString loadingImages(QObject* window)
{
    auto* quickWindow = qobject_cast<QQuickWindow*>(window);
    if (quickWindow == nullptr) {
        return QString();
    }
    QStringList out;
    describeLoading(quickWindow->contentItem(), out);
    return out.join(QStringLiteral("; "));
}

int attachmentPictures(QObject* window)
{
    auto* quickWindow = qobject_cast<QQuickWindow*>(window);
    if (quickWindow == nullptr) {
        return -1;
    }
    return attachmentPicturesBelow(quickWindow->contentItem());
}

int grabIfStable(QObject* window, const QString& path)
{
    auto* quickWindow = qobject_cast<QQuickWindow*>(window);
    if (quickWindow == nullptr) {
        return -1;
    }
    const QImage image = quickWindow->grabWindow();
    if (image.isNull()) {
        return -1;
    }
    if (g_previousGrab.isNull() || g_previousGrab != image) {
        g_previousGrab = image;
        return -2;
    }
    return saveWithLuma(image, path);
}

QString richTextPlain(const QString& html)
{
    QTextDocument doc;
    doc.setHtml(html);
    QString text = doc.toRawText();
    text.replace(QChar::LineSeparator, QLatin1Char('\n'));
    text.replace(QChar::ParagraphSeparator, QLatin1Char('\n'));
    text.replace(QChar::Nbsp, QLatin1Char(' '));
    return text;
}

void parkCursor()
{
    QCursor::setPos(QPoint(-100000, -100000));
}

namespace {
// The link a mention becomes (viewmodel `richtext::MENTION_HREF`).
bool isMention(const QTextCharFormat& format)
{
    return format.anchorHref().startsWith(QLatin1String("kourier:mention/"));
}

void recolorLinks(QTextDocument* doc, const QColor& color, const QColor& ink)
{
    // The document is read-only: recoloring must not grow an undo stack.
    doc->setUndoRedoEnabled(false);
    // Collect the link ranges first: changing formats while iterating can merge or split
    // fragments under the iterator.
    struct Span {
        int position;
        int length;
        bool mention;
    };
    QList<Span> ranges;
    for (QTextBlock block = doc->begin(); block.isValid(); block = block.next()) {
        for (auto it = block.begin(); !it.atEnd(); ++it) {
            const QTextFragment fragment = it.fragment();
            if (fragment.isValid() && fragment.charFormat().isAnchor()) {
                ranges.append({fragment.position(), fragment.length(), isMention(fragment.charFormat())});
            }
        }
    }
    QTextCharFormat linkFormat;
    linkFormat.setForeground(color);
    // A mention looks as it did before it was a link: the text's ink, no underline.
    QTextCharFormat mentionFormat;
    mentionFormat.setForeground(ink);
    mentionFormat.setFontUnderline(false);
    mentionFormat.setUnderlineStyle(QTextCharFormat::NoUnderline);
    for (const auto& span : std::as_const(ranges)) {
        QTextCursor cursor(doc);
        cursor.setPosition(span.position);
        cursor.setPosition(span.position + span.length, QTextCursor::KeepAnchor);
        cursor.mergeCharFormat(span.mention ? mentionFormat : linkFormat);
    }
}
} // namespace

void colorLinks(QObject* textDocument, const QColor& color, const QColor& ink)
{
    auto* quickDoc = qobject_cast<QQuickTextDocument*>(textDocument);
    QTextDocument* doc = quickDoc != nullptr ? quickDoc->textDocument() : nullptr;
    if (doc == nullptr) {
        return;
    }
    recolorLinks(doc, color, ink);
}

QString richTextLinkLooks(const QString& html, const QColor& color, const QColor& ink)
{
    QTextDocument doc;
    doc.setHtml(html);
    recolorLinks(&doc, color, ink);
    QStringList lines;
    for (QTextBlock block = doc.begin(); block.isValid(); block = block.next()) {
        for (auto it = block.begin(); !it.atEnd(); ++it) {
            const QTextCharFormat format = it.fragment().charFormat();
            if (!format.isAnchor()) {
                continue;
            }
            lines.append(QStringLiteral("%1 %2 %3 %4")
                             .arg(format.anchorHref(), format.foreground().color().name(),
                                 format.fontUnderline() ? QStringLiteral("1") : QStringLiteral("0"),
                                 format.fontWeight() >= QFont::Bold ? QStringLiteral("1") : QStringLiteral("0")));
        }
    }
    return lines.join(QLatin1Char('\n'));
}

} // namespace kourier
