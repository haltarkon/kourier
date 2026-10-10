#include "kourier-qt/cpp/imageprovider.h"

#include <QtCore/QBuffer>
#include <QtCore/QByteArray>
#include <QtCore/QMimeData>
#include <QtCore/QMutex>
#include <QtCore/QMutexLocker>
#include <QtCore/QRunnable>
#include <QtCore/QThreadPool>
#include <QtCore/QUrl>
#include <QtGui/QClipboard>
#include <QtGui/QGuiApplication>
#include <QtGui/QImageReader>
#include <QtGui/QLinearGradient>
#include <QtGui/QPainter>
#include <QtGui/QPainterPath>
#include <QtQuick/QQuickImageProvider>
#include <QtQuick/QQuickTextureFactory>

#include <cstring>
#include <memory>

namespace kourier {

namespace {

// Requests decoded at once; the rest queue (each worker may wait on the core's media read
// connection or a large decrypt).
constexpr int kWorkers = 3;

// Bytes before the file of an encoded payload: format, flags, codec, corner radius (u16).
constexpr size_t kEncodedHeader = 5;

uint32_t readU32(const rust::Vec<uint8_t>& v, size_t at)
{
    return static_cast<uint32_t>(v[at]) | (static_cast<uint32_t>(v[at + 1]) << 8)
        | (static_cast<uint32_t>(v[at + 2]) << 16) | (static_cast<uint32_t>(v[at + 3]) << 24);
}

// Qt's format name for a payload's codec byte (`Codec` in src/media.rs), or null. These are the
// only decoders the provider uses: the bytes come from other people, and a reader left to
// detect the format picks any installed plugin by content (SVG, PDF, PostScript through
// Ghostscript, camera raw, PSD, …).
const char* codecFormat(uint8_t codec)
{
    switch (codec) {
    case 1:
        return "jpeg";
    case 2:
        return "png";
    case 3:
        return "gif";
    case 4:
        return "webp";
    case 5:
        return "bmp";
    case 6:
        return "heif";
    case 7:
        return "avif";
    case 8:
        return "jxl";
    default:
        return nullptr;
    }
}

// Scale that makes `natural` cover `requested` (either side may be unset), never above 1.
QSize coverSize(const QSize& natural, const QSize& requested)
{
    if (!natural.isValid() || natural.isEmpty()) {
        return natural;
    }
    const bool w = requested.width() > 0;
    const bool h = requested.height() > 0;
    if (!w && !h) {
        return natural;
    }
    double scale = 0;
    if (w) {
        scale = static_cast<double>(requested.width()) / natural.width();
    }
    if (h) {
        scale = qMax(scale, static_cast<double>(requested.height()) / natural.height());
    }
    if (scale >= 1.0) {
        return natural;
    }
    return QSize(qMax(1, qRound(natural.width() * scale)), qMax(1, qRound(natural.height() * scale)));
}

// The centered square of `image`, `side` pixels wide, with corners of `radius` pixels and
// transparent outside them. A radius of half the side or more is a circle; 0 keeps the
// square as it is.
QImage cornerCrop(const QImage& image, int side, qreal radius)
{
    const int s = qMin(image.width(), image.height());
    if (s <= 0 || side <= 0) {
        return {};
    }
    const QImage square = image.copy((image.width() - s) / 2, (image.height() - s) / 2, s, s)
                              .scaled(side, side, Qt::IgnoreAspectRatio, Qt::SmoothTransformation);
    if (radius <= 0) {
        return square;
    }
    QImage out(side, side, QImage::Format_ARGB32_Premultiplied);
    out.fill(Qt::transparent);
    QPainter p(&out);
    p.setRenderHint(QPainter::Antialiasing);
    p.setRenderHint(QPainter::SmoothPixmapTransform);
    QPainterPath shape;
    if (radius * 2 >= side) {
        shape.addEllipse(QRectF(0, 0, side, side));
    } else {
        shape.addRoundedRect(QRectF(0, 0, side, side), radius, radius);
    }
    p.setClipPath(shape);
    p.drawImage(0, 0, square);
    return out;
}

// State shared by a response and its runnable: the response may go away first (the engine
// deletes it only after `finished`, but the runnable never relies on that).
struct Job {
    QMutex mutex;
    QQuickImageResponse* response = nullptr;
    QImage image;
    QString error;
    bool cancelled = false;
};

class Response : public QQuickImageResponse {
public:
    explicit Response(std::shared_ptr<Job> job)
        : m_job(std::move(job))
    {
        m_job->response = this;
    }

    ~Response() override
    {
        QMutexLocker lock(&m_job->mutex);
        m_job->response = nullptr;
    }

    QQuickTextureFactory* textureFactory() const override
    {
        QMutexLocker lock(&m_job->mutex);
        return QQuickTextureFactory::textureFactoryForImage(m_job->image);
    }

    QString errorString() const override
    {
        QMutexLocker lock(&m_job->mutex);
        return m_job->error;
    }

    void cancel() override
    {
        QMutexLocker lock(&m_job->mutex);
        m_job->cancelled = true;
    }

private:
    std::shared_ptr<Job> m_job;
};

class Provider : public QQuickAsyncImageProvider {
public:
    explicit Provider(MediaReader reader)
        : m_reader(reader)
    {
        m_pool.setMaxThreadCount(kWorkers);
        m_pool.setObjectName(QStringLiteral("kourier-media"));
    }

    ~Provider() override
    {
        m_pool.clear();
        m_pool.waitForDone();
    }

    QQuickImageResponse* requestImageResponse(const QString& id, const QSize& requestedSize) override
    {
        auto job = std::make_shared<Job>();
        auto* response = new Response(job);
        const QByteArray utf8 = id.toUtf8();
        const MediaReader reader = m_reader;
        m_pool.start([job, utf8, requestedSize, reader]() {
            {
                QMutexLocker lock(&job->mutex);
                if (job->cancelled) {
                    job->error = QStringLiteral("cancelled");
                    if (job->response != nullptr) {
                        Q_EMIT job->response->finished();
                    }
                    return;
                }
            }
            rust::Vec<uint8_t> payload = reader(
                rust::Str(utf8.constData(), static_cast<size_t>(utf8.size())),
                requestedSize.width(),
                requestedSize.height());
            const QImage image = decodeMediaPayload(payload, requestedSize);
            QMutexLocker lock(&job->mutex);
            if (image.isNull()) {
                job->error = QStringLiteral("unavailable");
            } else {
                job->image = image;
            }
            if (job->response != nullptr) {
                Q_EMIT job->response->finished();
            }
        });
        return response;
    }

private:
    MediaReader m_reader;
    QThreadPool m_pool;
};

} // namespace

QImage decodeMediaPayload(rust::Vec<uint8_t>& payload, const QSize& requested)
{
    if (payload.size() < 2) {
        return {};
    }
    const uint8_t format = payload[0];
    const bool cornered = (payload[1] & 1) != 0;
    // Encoded images: the corner radius (pixels of the requested size) is in bytes 3 and 4.
    const int radius = format == 1 && payload.size() >= kEncodedHeader
        ? payload[3] | (payload[4] << 8)
        : 0;
    QImage image;
    const char* codec = format == 1 && payload.size() > kEncodedHeader ? codecFormat(payload[2]) : nullptr;
    if (codec != nullptr) {
        const QByteArray bytes = QByteArray::fromRawData(
            reinterpret_cast<const char*>(payload.data() + kEncodedHeader),
            static_cast<qsizetype>(payload.size() - kEncodedHeader));
        QBuffer buffer;
        buffer.setData(bytes);
        buffer.open(QIODevice::ReadOnly);
        // Exactly the decoder Rust named, and no detection by content.
        QImageReader reader(&buffer, codec);
        reader.setAutoDetectImageFormat(false);
        reader.setDecideFormatFromContent(false);
        reader.setAutoTransform(true);
        QSize natural = reader.size();
        if (natural.isValid() && reader.transformation().testFlag(QImageIOHandler::TransformationRotate90)) {
            natural.transpose();
        }
        const QSize target = cornered ? QSize() : coverSize(natural, requested);
        if (target.isValid() && target != natural && reader.supportsOption(QImageIOHandler::ScaledSize)
            && !reader.transformation().testFlag(QImageIOHandler::TransformationRotate90)) {
            reader.setScaledSize(target);
        }
        image = reader.read();
        // Formats that do not report their size before decoding, decoders that cannot scale
        // and rotated pictures are scaled afterwards. `wanted` has the picture's proportions
        // already; scaling to exactly it (not "keeping" the ratio, which truncates one side
        // and can come out a pixel short of what was asked for) gives QML the size it laid
        // the picture out for.
        const QSize wanted = target.isValid() || cornered ? target : coverSize(image.size(), requested);
        if (!image.isNull() && wanted.isValid() && image.size() != wanted) {
            image = image.scaled(wanted, Qt::IgnoreAspectRatio, Qt::SmoothTransformation);
        }
    } else if (format == 2 && payload.size() >= 10) {
        const auto w = static_cast<int>(readU32(payload, 2));
        const auto h = static_cast<int>(readU32(payload, 6));
        const size_t need = static_cast<size_t>(w) * static_cast<size_t>(h) * 4;
        if (w > 0 && h > 0 && payload.size() - 10 == need) {
            image = QImage(payload.data() + 10, w, h, w * 4, QImage::Format_RGBA8888).copy();
        }
    }
    // Plaintext bytes do not linger in freed memory.
    std::memset(payload.data(), 0, payload.size());
    if (image.isNull()) {
        return {};
    }
    if (cornered) {
        int side = qMin(image.width(), image.height());
        const int asked = qMax(requested.width(), requested.height());
        if (asked > 0) {
            side = qMin(side, asked);
        }
        // The radius is given for the requested side; a smaller picture keeps the proportion.
        const qreal scale = asked > 0 ? static_cast<qreal>(side) / asked : 1.0;
        image = cornerCrop(image, side, radius * scale);
    }
    return image;
}

void installImageProvider(QQmlApplicationEngine& engine, MediaReader reader)
{
    engine.addImageProvider(QStringLiteral("kourier"), new Provider(reader));
}

bool pasteClipboardImage(rust::Fn<void(rust::Vec<uint8_t>)> done)
{
    QClipboard* clipboard = QGuiApplication::clipboard();
    if (clipboard == nullptr) {
        return false;
    }
    const QImage image = clipboard->image();
    if (image.isNull()) {
        return false;
    }
    QThreadPool::globalInstance()->start([image, done]() {
        QByteArray png;
        QBuffer buffer(&png);
        buffer.open(QIODevice::WriteOnly);
        rust::Vec<uint8_t> out;
        if (image.save(&buffer, "PNG")) {
            out.reserve(static_cast<size_t>(png.size()));
            for (const char c : png) {
                out.push_back(static_cast<uint8_t>(c));
            }
        }
        done(std::move(out));
    });
    return true;
}

bool setClipboardImagePng(rust::Vec<uint8_t> png)
{
    QImage image;
    if (!image.loadFromData(png.data(), static_cast<int>(png.size()), "PNG") || QGuiApplication::clipboard() == nullptr) {
        return false;
    }
    QGuiApplication::clipboard()->setImage(image);
    return true;
}

QStringList clipboardFiles()
{
    QStringList out;
    const QClipboard* clipboard = QGuiApplication::clipboard();
    const QMimeData* data = clipboard != nullptr ? clipboard->mimeData() : nullptr;
    if (data == nullptr || !data->hasUrls()) {
        return out;
    }
    const QList<QUrl> urls = data->urls();
    for (const QUrl& url : urls) {
        if (url.isLocalFile()) {
            out.append(url.toLocalFile());
        }
    }
    return out;
}

void setClipboardFiles(const QStringList& paths)
{
    QClipboard* clipboard = QGuiApplication::clipboard();
    if (clipboard == nullptr) {
        return;
    }
    QList<QUrl> urls;
    for (const QString& path : paths) {
        urls.append(QUrl::fromLocalFile(path));
    }
    auto* data = new QMimeData;
    data->setUrls(urls);
    clipboard->setMimeData(data);
}

void setClipboardText(const QString& text)
{
    if (QClipboard* clipboard = QGuiApplication::clipboard()) {
        clipboard->setText(text);
    }
}

QString clipboardText()
{
    QClipboard* clipboard = QGuiApplication::clipboard();
    return clipboard != nullptr ? clipboard->text() : QString();
}

namespace {

// `bytes` as an avatar payload with the corner radius `radius`, decoded for a `side` request;
// null when nothing was decoded.
QImage probeAvatar(rust::Vec<uint8_t> bytes, uint8_t codec, int32_t side, uint16_t radius)
{
    rust::Vec<uint8_t> payload;
    payload.reserve(bytes.size() + kEncodedHeader);
    payload.push_back(1);
    payload.push_back(1);
    payload.push_back(codec);
    payload.push_back(static_cast<uint8_t>(radius & 0xff));
    payload.push_back(static_cast<uint8_t>(radius >> 8));
    for (const uint8_t b : bytes) {
        payload.push_back(b);
    }
    const QImage image = decodeMediaPayload(payload, QSize(side, side));
    if (image.isNull() || image.width() != side || image.height() != side) {
        return {};
    }
    return image;
}

} // namespace

int32_t roundAvatarProbe(rust::Vec<uint8_t> bytes, uint8_t codec, int32_t side)
{
    const QImage image = probeAvatar(std::move(bytes), codec, side, 0xffff);
    if (image.isNull()) {
        return -1;
    }
    return qAlpha(image.pixel(0, 0)) * 1000 + qAlpha(image.pixel(side / 2, side / 2));
}

int32_t avatarCornerProbe(rust::Vec<uint8_t> bytes, uint8_t codec, int32_t side, int32_t radius)
{
    const QImage image = probeAvatar(std::move(bytes), codec, side, static_cast<uint16_t>(qBound(0, radius, 0xffff)));
    if (image.isNull()) {
        return -1;
    }
    const int inset = side / 10;
    return (qAlpha(image.pixel(0, 0)) << 16) | (qAlpha(image.pixel(inset, inset)) << 8)
        | qAlpha(image.pixel(side / 2, side / 2));
}

rust::Vec<uint8_t> samplePng(int32_t width, int32_t height, int32_t hue)
{
    return sampleImage(width, height, hue, rust::Str("png"));
}

rust::Vec<uint8_t> sampleImage(int32_t width, int32_t height, int32_t hue, rust::Str format)
{
    rust::Vec<uint8_t> out;
    if (width <= 0 || height <= 0 || width > 4096 || height > 4096) {
        return out;
    }
    const QByteArray name(format.data(), static_cast<qsizetype>(format.size()));
    QImage image(width, height, QImage::Format_RGB32);
    {
        QPainter p(&image);
        QLinearGradient g(0, 0, width, height);
        g.setColorAt(0, QColor::fromHsl(hue % 360, 150, 170));
        g.setColorAt(1, QColor::fromHsl((hue + 50) % 360, 120, 90));
        p.fillRect(image.rect(), g);
        p.setPen(Qt::NoPen);
        p.setBrush(QColor(255, 255, 255, 70));
        p.drawEllipse(QPointF(width * 0.7, height * 0.35), width * 0.18, width * 0.18);
    }
    QByteArray encoded;
    QBuffer buffer(&encoded);
    buffer.open(QIODevice::WriteOnly);
    if (!image.save(&buffer, name.constData())) {
        return out;
    }
    out.reserve(static_cast<size_t>(encoded.size()));
    for (const char c : encoded) {
        out.push_back(static_cast<uint8_t>(c));
    }
    return out;
}

} // namespace kourier
