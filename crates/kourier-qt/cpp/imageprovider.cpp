#include "kourier-qt/cpp/imageprovider.h"

#include <QtCore/QBuffer>
#include <QtCore/QByteArray>
#include <QtCore/QMimeData>
#include <QtCore/QMutex>
#include <QtCore/QMutexLocker>
#include <QtCore/QRunnable>
#include <QtCore/QScopeGuard>
#include <QtCore/QSemaphore>
#include <QtCore/QThread>
#include <QtCore/QThreadPool>
#include <QtCore/QUrl>
#include <QtGui/QClipboard>
#include <QtGui/QFont>
#include <QtGui/QGuiApplication>
#include <QtGui/QImageReader>
#include <QtGui/QImageWriter>
#include <QtGui/QLinearGradient>
#include <QtGui/QPainter>
#include <QtGui/QPainterPath>
#include <QtQuick/QQuickImageProvider>
#include <QtQuick/QQuickTextureFactory>

#include <atomic>
#include <cmath>
#include <cstring>
#include <dlfcn.h>
#include <memory>
#include <vector>

namespace kourier {

namespace {

// Requests decoded at once; the rest queue (each worker may wait on the core's media read
// connection or a large decrypt).
constexpr int kWorkers = 3;

// Bytes before the file of an encoded payload: format, flags, codec, corner radius (u16).
constexpr size_t kEncodedHeader = 5;

// Requests that were asked for and whose worker has not finished (queued or running), also those
// whose view cancelled or went away meanwhile: a re-created delegate's request is still counted
// here while no Image says "loading" any more (`pendingImageRequests`).
std::atomic<int> g_pending{0};

// Test probe: the requests (`id WxH`, the size Qt asked for) while `logImageRequests(true)`.
std::atomic<bool> g_logRequests{false};
QMutex g_logMutex;
std::vector<QString> g_requestLog;

// Test probe (`raceImageFinishes`): 0 off, 1 a request returns only once its worker is done, 2 the
// same, and an emission of `finished` on a thread other than the response's is held inside Qt's
// emission until the response's thread is back in its event loop.
std::atomic<int> g_raceFinishes{0};

// Test probes of the thread that decodes (`decodeProbe`): how many pictures were scaled AFTER
// the reader had decoded them whole (the slow path), and how many pixels the last decode made.
thread_local int t_scaledAfterDecode = 0;
thread_local long long t_decodedPixels = 0;

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
    // Test probe (`raceImageFinishes`): released when the worker is done, and when it begins to
    // emit `finished` on its own thread.
    QSemaphore handedBack;
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

    // The test probe's view of the job (`raceImageFinishes`).
    Job& job() const { return *m_job; }

private:
    std::shared_ptr<Job> m_job;
};

// Tells Qt that the job of `response` is done; called on the worker with the job's mutex held.
// Never emitted here: Qt's pixmap reader connects to `finished` only after
// `requestImageResponse` returned, and Qt takes the receivers of an emission when it starts. A
// worker's emission that started before that connection reaches only the response's own slot,
// which sets a flag the reader checks right after it connected; when that slot runs after the
// check, the signal reached nobody, and the Image stays in Loading for ever with nothing pending
// here (a hidden blurhash `Image`, microseconds of work, 1 render in 277 of `cargo xtask shots`
// under load; `raceImageFinishes(2)` makes the order certain). Queued, `finished` is emitted on
// the response's thread (the reader's, which called `requestImageResponse`) once it is back in
// its event loop, connected. The mutex keeps the response alive while the call is posted (its
// destructor takes it first); Qt drops a queued call whose object was deleted.
void announce(QQuickImageResponse* response)
{
    QMetaObject::invokeMethod(response, [response] { Q_EMIT response->finished(); }, Qt::QueuedConnection);
}

// Test probe (`raceImageFinishes(2)`), called by Qt before every slot it calls directly while the
// probe is on: when the slot is the response's own `_q_finished` (connected first, in Qt's
// constructor; it sets the flag Qt's pixmap reader checks after it connected) and the emission
// runs on a thread other than the response's, the request may return now, and the emission waits
// until the response's thread is back in its event loop. Qt took the emission's receivers before
// this callback, and the reader connects and checks the flag before it goes back to its loop: the
// order in which a `finished` emitted by the worker reaches nobody.
void holdForeignFinish(QObject* receiver, int index, void**)
{
    static const int finishedSlot = QQuickImageResponse::staticMetaObject.indexOfSlot("_q_finished()");
    if (index != finishedSlot || receiver->thread() == QThread::currentThread()) {
        return;
    }
    auto* response = dynamic_cast<Response*>(receiver);
    if (response == nullptr) {
        return;
    }
    response->job().handedBack.release();
    auto back = std::make_shared<QSemaphore>();
    QMetaObject::invokeMethod(receiver, [back] { back->release(); }, Qt::QueuedConnection);
    back->tryAcquire(1, 5000);
}

// Qt's test hook for signal emissions (`QSignalSpyCallbackSet`, `qt_register_signal_spy_callbacks`
// in Qt's private `qobject_p.h`, the same since Qt 4). Looked up at run time, only when a test
// turns the probe on, so the app does not link one more private symbol of Qt.
struct SpyCallbacks {
    void (*signalBegin)(QObject*, int, void**);
    void (*slotBegin)(QObject*, int, void**);
    void (*signalEnd)(QObject*, int);
    void (*slotEnd)(QObject*, int);
};
SpyCallbacks g_spy{nullptr, &holdForeignFinish, nullptr, nullptr};

bool registerSpy(bool on)
{
    using Register = void (*)(SpyCallbacks*);
    static const auto reg = reinterpret_cast<Register>(
        dlsym(RTLD_DEFAULT, "_Z32qt_register_signal_spy_callbacksP21QSignalSpyCallbackSet"));
    if (reg == nullptr) {
        return false;
    }
    reg(on ? &g_spy : nullptr);
    return true;
}

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
        g_pending.fetch_add(1, std::memory_order_seq_cst);
        if (g_logRequests.load(std::memory_order_relaxed)) {
            QMutexLocker lock(&g_logMutex);
            g_requestLog.push_back(id + QLatin1Char(' ') + QString::number(requestedSize.width()) + QLatin1Char('x')
                + QString::number(requestedSize.height()));
        }
        m_pool.start([job, utf8, requestedSize, reader]() {
            // Counted down on every way out, after `finished` was announced.
            const auto done = qScopeGuard([&job] {
                g_pending.fetch_sub(1, std::memory_order_seq_cst);
                job->handedBack.release();
            });
            {
                QMutexLocker lock(&job->mutex);
                if (job->cancelled) {
                    job->error = QStringLiteral("cancelled");
                    if (job->response != nullptr) {
                        announce(job->response);
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
                announce(job->response);
            }
        });
        if (g_raceFinishes.load(std::memory_order_seq_cst) != 0) {
            // Test probe: the worker is done (or emitting) before Qt has the response.
            job->handedBack.tryAcquire(1, 5000);
        }
        return response;
    }

private:
    MediaReader m_reader;
    QThreadPool m_pool;
};

} // namespace

int pendingImageRequests()
{
    return g_pending.load(std::memory_order_seq_cst);
}

void logImageRequests(bool on)
{
    QMutexLocker lock(&g_logMutex);
    g_requestLog.clear();
    g_logRequests.store(on, std::memory_order_relaxed);
}

rust::Vec<rust::String> takeImageRequestLog()
{
    QMutexLocker lock(&g_logMutex);
    rust::Vec<rust::String> out;
    for (const QString& line : g_requestLog) {
        const QByteArray utf8 = line.toUtf8();
        out.push_back(rust::String(utf8.constData(), static_cast<size_t>(utf8.size())));
    }
    g_requestLog.clear();
    return out;
}

bool raceImageFinishes(int32_t how)
{
    const bool hooked = registerSpy(how == 2);
    if (how == 2 && !hooked) {
        g_raceFinishes.store(0, std::memory_order_seq_cst);
        return false;
    }
    g_raceFinishes.store(how, std::memory_order_seq_cst);
    return true;
}

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
        // The reader scales in the file's own orientation and turns the result afterwards: for
        // a picture stored sideways (EXIF 90/270) the size to ask it for is the transposed one.
        const bool rotated = reader.transformation().testFlag(QImageIOHandler::TransformationRotate90);
        QSize natural = reader.size();
        if (natural.isValid() && rotated) {
            natural.transpose();
        }
        // An avatar is cut to a square afterwards: its shorter side has to cover the side asked
        // for (decoding it whole costs twice as much for a 2 MP photo of a contact).
        const int asked = qMax(requested.width(), requested.height());
        const QSize cover = cornered ? QSize(asked, asked) : requested;
        QSize target = coverSize(natural, cover);
        // A timeline picture is drawn whole (`PreserveAspectFit`) into a rectangle of its own
        // shape, rounded to a pixel, and Qt asks for exactly that rectangle. `coverSize` rounds
        // the other side by itself and can come out a pixel off (1080x2340 asked 398x864 gives
        // 399x864): drawn into 398 the picture would be resampled by 0.9975 and lose its fine
        // detail. A cover within a pixel of what was asked on both sides is the request itself
        // (at most one pixel of distortion); a square, a quote's thumbnail or a picture of
        // another shape is not.
        if (!cornered && requested.width() > 0 && requested.height() > 0 && target.isValid()
            && qAbs(target.width() - requested.width()) <= 1 && qAbs(target.height() - requested.height()) <= 1) {
            target = requested;
        }
        if (target.isValid() && target != natural && reader.supportsOption(QImageIOHandler::ScaledSize)) {
            reader.setScaledSize(rotated ? target.transposed() : target);
        }
        image = reader.read();
        t_decodedPixels = static_cast<long long>(image.width()) * image.height();
        // Formats that do not report their size before decoding, decoders that cannot scale
        // and rotated pictures are scaled afterwards. `wanted` has the picture's proportions
        // already; scaling to exactly it (not "keeping" the ratio, which truncates one side
        // and can come out a pixel short of what was asked for) gives QML the size it laid
        // the picture out for.
        const QSize wanted = target.isValid() ? target : coverSize(image.size(), cover);
        if (!image.isNull() && wanted.isValid() && image.size() != wanted) {
            ++t_scaledAfterDecode;
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

namespace {

class ArtProvider : public QQuickImageProvider {
public:
    ArtProvider()
        : QQuickImageProvider(QQuickImageProvider::Image)
    {
    }

    QImage requestImage(const QString& id, QSize* size, const QSize& requested) override
    {
        const QStringList parts = id.split(QLatin1Char('/'));
        const QColor color = parts.size() == 2 ? QColor(QLatin1Char('#') + parts[1]) : QColor();
        if (!color.isValid()) {
            return {};
        }
        QImage image;
        if (parts[0] == QLatin1String("doodle")) {
            QImageReader reader(QStringLiteral(":/qt/qml/app/kourier/icons/doodle.svg"), "svg");
            reader.setScaledSize(requested.isValid() ? requested : QSize(480, 480));
            image = reader.read().convertToFormat(QImage::Format_ARGB32_Premultiplied);
            if (!image.isNull()) {
                QPainter painter(&image);
                painter.setCompositionMode(QPainter::CompositionMode_SourceIn);
                painter.fillRect(image.rect(), color);
            }
        } else if (parts[0] == QLatin1String("swatch")) {
            image = QImage(requested.isValid() ? requested : QSize(16, 16),
                QImage::Format_ARGB32_Premultiplied);
            image.fill(Qt::transparent);
            QPainter painter(&image);
            painter.setRenderHint(QPainter::Antialiasing);
            painter.setPen(Qt::NoPen);
            painter.setBrush(color);
            painter.drawEllipse(image.rect().adjusted(1, 1, -1, -1));
        }
        if (size != nullptr) {
            *size = image.size();
        }
        return image;
    }
};

} // namespace

void installArtProvider(QQmlApplicationEngine& engine)
{
    engine.addImageProvider(QStringLiteral("kourierart"), new ArtProvider);
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

rust::Vec<uint8_t> detailImage(int32_t width, int32_t height, rust::Str format)
{
    rust::Vec<uint8_t> out;
    if (width <= 0 || height <= 0 || width > 8192 || height > 8192) {
        return out;
    }
    const QByteArray name(format.data(), static_cast<qsizetype>(format.size()));
    QImage image(width, height, QImage::Format_RGB32);
    image.fill(Qt::white);
    {
        QPainter p(&image);
        p.setRenderHint(QPainter::Antialiasing, false);
        p.setRenderHint(QPainter::TextAntialiasing, true);
        p.setPen(Qt::NoPen);
        p.setBrush(Qt::black);
        const int band = qMax(1, height / 8);
        // Two bands of vertical stripes, 8 px black and 8 px white: still 4 px a period when
        // the picture is drawn at 27 % of its size (a phone screenshot in the timeline), so
        // no scaler turns them into moire. (Stripes of 3 px were tried: they measure the
        // scaler, not the picture.)
        for (int x = 0; x < width; x += 16) {
            p.drawRect(x, 0, 8, 2 * band);
        }
        // One band of horizontal stripes, 6 px and 6 px.
        for (int y = 2 * band; y < 3 * band; y += 12) {
            p.drawRect(0, y, width, 6);
        }
        // A big line of text and a small one, black on white.
        p.setPen(Qt::black);
        QFont big;
        big.setPixelSize(qMax(12, width / 8));
        big.setBold(true);
        p.setFont(big);
        p.drawText(QRect(0, 3 * band, width, 2 * band), Qt::AlignCenter, QStringLiteral("KOURIER"));
        QFont small;
        small.setPixelSize(qMax(8, width / 50));
        p.setFont(small);
        p.drawText(QRect(0, 5 * band, width, band), Qt::AlignCenter, QStringLiteral("The quick brown fox 0123456789"));
        // Thin diagonals in the rest.
        p.setPen(QPen(Qt::black, 3));
        for (int d = -height; d < width; d += 16) {
            p.drawLine(d, 6 * band, d + 2 * band, height);
        }
    }
    QByteArray encoded;
    QBuffer buffer(&encoded);
    buffer.open(QIODevice::WriteOnly);
    const int quality = name == "jpeg" || name == "jpg" ? 92 : -1;
    if (!image.save(&buffer, name.constData(), quality)) {
        return out;
    }
    out.reserve(static_cast<size_t>(encoded.size()));
    for (const char c : encoded) {
        out.push_back(static_cast<uint8_t>(c));
    }
    return out;
}

namespace {

// Rec. 601 luma of every pixel of an RGB32 image, row by row.
std::vector<double> lumaOf(const QImage& rgb)
{
    std::vector<double> luma;
    luma.reserve(static_cast<size_t>(rgb.width()) * static_cast<size_t>(rgb.height()));
    for (int y = 0; y < rgb.height(); ++y) {
        const auto* line = reinterpret_cast<const QRgb*>(rgb.constScanLine(y));
        for (int x = 0; x < rgb.width(); ++x) {
            luma.push_back(0.299 * qRed(line[x]) + 0.587 * qGreen(line[x]) + 0.114 * qBlue(line[x]));
        }
    }
    return luma;
}

// Variance of the 4-neighbour Laplacian over the interior of a `w` x `h` luma plane: the usual
// measure of how much fine detail a picture holds (a blur lowers it).
double laplacianVariance(const std::vector<double>& luma, int w, int h)
{
    if (w < 3 || h < 3) {
        return 0;
    }
    double sum = 0;
    double sumSquares = 0;
    double count = 0;
    for (int y = 1; y < h - 1; ++y) {
        for (int x = 1; x < w - 1; ++x) {
            const size_t i = static_cast<size_t>(y) * static_cast<size_t>(w) + static_cast<size_t>(x);
            const double l = luma[i - 1] + luma[i + 1] + luma[i - static_cast<size_t>(w)] + luma[i + static_cast<size_t>(w)] - 4 * luma[i];
            sum += l;
            sumSquares += l * l;
            count += 1;
        }
    }
    const double mean = sum / count;
    return sumSquares / count - mean * mean;
}

// PSNR (dB, 99 for identical) of two luma planes of `w` x `h` after each is averaged in blocks
// of 8 x 8 pixels: it tells the right picture, at the right place, from a wrong one, and
// does not care for the fine stripes, which move by a fraction of a pixel with the rounding
// of a crop and are what `laplacianVariance` is for.
double coarsePsnr(const std::vector<double>& a, const std::vector<double>& b, int w, int h)
{
    constexpr int block = 8;
    double squares = 0;
    double count = 0;
    for (int by = 0; by + block <= h; by += block) {
        for (int bx = 0; bx + block <= w; bx += block) {
            double sa = 0;
            double sb = 0;
            for (int y = by; y < by + block; ++y) {
                for (int x = bx; x < bx + block; ++x) {
                    const size_t i = static_cast<size_t>(y) * static_cast<size_t>(w) + static_cast<size_t>(x);
                    sa += a[i];
                    sb += b[i];
                }
            }
            const double d = (sa - sb) / (block * block);
            squares += d * d;
            count += 1;
        }
    }
    if (count == 0) {
        return 0;
    }
    const double mse = squares / count;
    return mse <= 0 ? 99.0 : qMin(99.0, 10.0 * std::log10(255.0 * 255.0 / mse));
}

} // namespace

rust::Vec<double> cornerProbe(rust::Vec<uint8_t> grab)
{
    rust::Vec<double> out;
    QImage got;
    if (!got.loadFromData(grab.data(), static_cast<qsizetype>(grab.size()), "PNG") || got.isNull()) {
        return out;
    }
    got = got.convertToFormat(QImage::Format_ARGB32);
    double area = 0;
    for (int y = 0; y < got.height() / 2; ++y) {
        for (int x = 0; x < got.width() / 2; ++x) {
            area += 1.0 - qAlpha(got.pixel(x, y)) / 255.0;
        }
    }
    out.push_back(got.width());
    out.push_back(got.height());
    out.push_back(area);
    return out;
}

rust::Vec<double> pictureProbe(rust::Vec<uint8_t> file, uint8_t codec, rust::Vec<uint8_t> grab)
{
    rust::Vec<double> out;
    const char* format = codecFormat(codec);
    if (format == nullptr) {
        return out;
    }
    QImage got;
    if (!got.loadFromData(grab.data(), static_cast<qsizetype>(grab.size()), "PNG") || got.isNull()) {
        return out;
    }
    got = got.convertToFormat(QImage::Format_RGB32);
    // Both references are cut in the middle to the grab's size, as PreserveAspectCrop does.
    const auto cut = [&got](const QImage& image) {
        return image.copy((image.width() - got.width()) / 2, (image.height() - got.height()) / 2, got.width(), got.height())
            .convertToFormat(QImage::Format_RGB32);
    };
    // The file decoded whole and scaled by Qt's smooth scaling (nothing of the provider in it).
    QByteArray fileBytes(reinterpret_cast<const char*>(file.data()), static_cast<qsizetype>(file.size()));
    QBuffer fileBuffer(&fileBytes);
    fileBuffer.open(QIODevice::ReadOnly);
    QImageReader reader(&fileBuffer, format);
    reader.setAutoDetectImageFormat(false);
    reader.setDecideFormatFromContent(false);
    reader.setAutoTransform(true);
    const QImage whole = reader.read();
    if (whole.isNull()) {
        return out;
    }
    const QImage smooth = cut(whole.scaled(got.size(), Qt::KeepAspectRatioByExpanding, Qt::SmoothTransformation));
    // The file as the provider decodes it for a request of the grab's size (what the item
    // draws when the file is served, one to one).
    rust::Vec<uint8_t> payload;
    payload.reserve(file.size() + kEncodedHeader);
    for (const uint8_t head : {uint8_t(1), uint8_t(0), codec, uint8_t(0), uint8_t(0)}) {
        payload.push_back(head);
    }
    for (const uint8_t b : file) {
        payload.push_back(b);
    }
    const QImage decoded = decodeMediaPayload(payload, got.size());
    if (decoded.isNull()) {
        return out;
    }
    const QImage provided = cut(decoded);
    const std::vector<double> a = lumaOf(got);
    const std::vector<double> b = lumaOf(provided);
    const std::vector<double> c = lumaOf(smooth);
    const auto psnr = [&a, &got](const std::vector<double>& other) {
        return coarsePsnr(a, other, got.width(), got.height());
    };
    out.push_back(psnr(b));
    out.push_back(laplacianVariance(a, got.width(), got.height()));
    out.push_back(laplacianVariance(c, got.width(), got.height()));
    out.push_back(got.width());
    out.push_back(got.height());
    out.push_back(psnr(c));
    out.push_back(laplacianVariance(b, got.width(), got.height()));
    return out;
}


rust::Vec<uint8_t> orientedImage(int32_t width, int32_t height, int32_t orientation)
{
    rust::Vec<uint8_t> out;
    if (width <= 0 || height <= 0 || width > 8192 || height > 8192 || orientation < 1 || orientation > 8) {
        return out;
    }
    // Stored left half red, right half blue, with a few thin lines so a decoder has work.
    QImage image(width, height, QImage::Format_RGB32);
    image.fill(QColor(220, 30, 30));
    {
        QPainter p(&image);
        p.fillRect(QRect(width / 2, 0, width - width / 2, height), QColor(30, 30, 220));
        p.setPen(QPen(Qt::white, 2));
        for (int x = 0; x < width; x += 24) {
            p.drawLine(x, 0, x, height / 6);
        }
    }
    QByteArray encoded;
    QBuffer buffer(&encoded);
    buffer.open(QIODevice::WriteOnly);
    QImageWriter writer(&buffer, "jpeg");
    writer.setQuality(95);
    if (!writer.write(image) || encoded.size() < 2) {
        return out;
    }
    // Qt's writer would turn the pixels itself for a transformation; the orientation has to be
    // a tag, as a phone writes it: an Exif APP1 segment (big-endian TIFF, one entry, tag 0x0112)
    // right after the SOI marker.
    const unsigned char exif[] = {
        0xff, 0xe1, 0x00, 0x22, 'E', 'x', 'i', 'f', 0x00, 0x00, // marker, length 34, "Exif\0\0"
        'M', 'M', 0x00, 0x2a, 0x00, 0x00, 0x00, 0x08, // TIFF header, IFD0 at 8
        0x00, 0x01, // one entry
        0x01, 0x12, 0x00, 0x03, 0x00, 0x00, 0x00, 0x01, 0x00, static_cast<unsigned char>(orientation), 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, // no next IFD
    };
    out.reserve(static_cast<size_t>(encoded.size()) + sizeof(exif));
    out.push_back(static_cast<uint8_t>(encoded[0]));
    out.push_back(static_cast<uint8_t>(encoded[1]));
    for (const unsigned char c : exif) {
        out.push_back(c);
    }
    for (qsizetype i = 2; i < encoded.size(); ++i) {
        out.push_back(static_cast<uint8_t>(encoded[i]));
    }
    return out;
}

rust::Vec<int32_t> decodeProbe(rust::Vec<uint8_t> file, uint8_t codec, int32_t width, int32_t height, bool cornered)
{
    rust::Vec<int32_t> out;
    rust::Vec<uint8_t> payload;
    payload.reserve(file.size() + kEncodedHeader);
    for (const uint8_t head : {uint8_t(1), uint8_t(cornered ? 1 : 0), codec, uint8_t(0), uint8_t(0)}) {
        payload.push_back(head);
    }
    for (const uint8_t b : file) {
        payload.push_back(b);
    }
    t_scaledAfterDecode = 0;
    t_decodedPixels = 0;
    const QImage image = decodeMediaPayload(payload, QSize(width, height));
    if (image.isNull()) {
        return out;
    }
    out.push_back(image.width());
    out.push_back(image.height());
    out.push_back(t_scaledAfterDecode);
    // The middle of the top tenth and of the bottom tenth, as 0xRRGGBB.
    const auto rgb = [&image](int y) { return static_cast<int32_t>(image.pixel(image.width() / 2, y) & 0xffffff); };
    out.push_back(rgb(image.height() / 10));
    out.push_back(rgb(image.height() - 1 - image.height() / 10));
    out.push_back(static_cast<int32_t>(qMin<long long>(t_decodedPixels, INT32_MAX)));
    return out;
}

} // namespace kourier
