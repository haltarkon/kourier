#include "kourier-qt/cpp/devtools.h"

#include <QtCore/QList>
#include <QtCore/QPair>
#include <QtGui/QCursor>
#include <QtGui/QImage>
#include <QtGui/QTextBlock>
#include <QtGui/QTextCursor>
#include <QtGui/QTextDocument>
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

int grabWindowToFile(QObject* window, const QString& path)
{
    auto* quickWindow = qobject_cast<QQuickWindow*>(window);
    if (quickWindow == nullptr) {
        return -1;
    }
    const QImage image = quickWindow->grabWindow();
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

void colorLinks(QObject* textDocument, const QColor& color)
{
    auto* quickDoc = qobject_cast<QQuickTextDocument*>(textDocument);
    QTextDocument* doc = quickDoc != nullptr ? quickDoc->textDocument() : nullptr;
    if (doc == nullptr) {
        return;
    }
    // The document is read-only: recoloring must not grow an undo stack.
    doc->setUndoRedoEnabled(false);
    // Collect the link ranges first: changing formats while iterating can merge or split
    // fragments under the iterator.
    QList<QPair<int, int>> ranges;
    for (QTextBlock block = doc->begin(); block.isValid(); block = block.next()) {
        for (auto it = block.begin(); !it.atEnd(); ++it) {
            const QTextFragment fragment = it.fragment();
            if (fragment.isValid() && fragment.charFormat().isAnchor()) {
                ranges.append({fragment.position(), fragment.length()});
            }
        }
    }
    QTextCharFormat format;
    format.setForeground(color);
    for (const auto& [position, length] : std::as_const(ranges)) {
        QTextCursor cursor(doc);
        cursor.setPosition(position);
        cursor.setPosition(position + length, QTextCursor::KeepAnchor);
        cursor.mergeCharFormat(format);
    }
}

} // namespace kourier
