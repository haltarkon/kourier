#include "kourier-qt/cpp/modeldata.h"

#include <QtCore/QCoreApplication>
#include <QtCore/QMetaObject>
#include <QtCore/QVariant>

#include <algorithm>
#include <vector>

namespace kourier {

namespace {
// The objects alive, in creation order (Qt thread only).
std::vector<ModelData*>& registry()
{
    static std::vector<ModelData*> r;
    return r;
}
} // namespace

std::unique_ptr<ModelBuffers> buildModelBuffers(
    rust::Slice<const float> positions, rust::Slice<const uint32_t> colors, uint32_t fallback,
    rust::Slice<const float> min, rust::Slice<const float> max)
{
    auto out = std::make_unique<ModelBuffers>();
    const qsizetype tris = qsizetype(positions.size() / 9);
    out->triangles = tris;
    out->positions.resize(tris * 3);
    out->normals.resize(tris * 3);
    out->uv1s.resize(tris * 3);
    const bool coloured = colors.size() == size_t(tris) && tris > 0;
    if (coloured) {
        out->colors.resize(tris * 3);
    }
    if (min.size() == 3 && max.size() == 3) {
        out->min = QVector3D(min[0], min[1], min[2]);
        out->max = QVector3D(max[0], max[1], max[2]);
    }
    const auto rgba = [](uint32_t c) {
        return QVector4D(float((c >> 24) & 0xff) / 255.f, float((c >> 16) & 0xff) / 255.f,
            float((c >> 8) & 0xff) / 255.f, 1.f);
    };
    const QVector4D neutral = rgba(fallback);
    const QVector2D bary[3] = { QVector2D(1, 0), QVector2D(0, 1), QVector2D(0, 0) };
    for (qsizetype t = 0; t < tris; ++t) {
        const float* v = positions.data() + 9 * t;
        const QVector3D a(v[0], v[1], v[2]), b(v[3], v[4], v[5]), c(v[6], v[7], v[8]);
        // A face normal; a degenerate triangle gets the up axis (it is not drawn anyway).
        QVector3D n = QVector3D::crossProduct(b - a, c - a);
        n = n.lengthSquared() > 0.f ? n.normalized() : QVector3D(0, 0, 1);
        const QVector3D corners[3] = { a, b, c };
        for (int k = 0; k < 3; ++k) {
            out->positions[3 * t + k] = corners[k];
            out->normals[3 * t + k] = n;
            out->uv1s[3 * t + k] = bary[k];
        }
        if (coloured) {
            const uint32_t own = colors[size_t(t)];
            const QVector4D col = own != 0 ? rgba(own) : neutral;
            for (int k = 0; k < 3; ++k) {
                out->colors[3 * t + k] = col;
            }
        }
    }
    return out;
}

void deliverModel(int64_t attachment, std::unique_ptr<ModelBuffers> buffers)
{
    // Any thread: the hand-over runs on the Qt thread, the lists never leave the shared pointer.
    QCoreApplication* app = QCoreApplication::instance();
    if (app == nullptr) {
        return;
    }
    const std::shared_ptr<ModelBuffers> shared(std::move(buffers));
    QMetaObject::invokeMethod(
        app,
        [attachment, shared]() {
            for (ModelData* d : registry()) {
                if (d->waiting() && d->attachment() == attachment) {
                    d->take(shared);
                    return;
                }
            }
        },
        Qt::QueuedConnection);
}

void failModel(int64_t attachment)
{
    for (ModelData* d : registry()) {
        if (d->waiting() && (attachment == 0 || d->attachment() == attachment)) {
            d->fail();
            return;
        }
    }
}

uint32_t waitingFallback(int64_t attachment)
{
    for (ModelData* d : registry()) {
        if (d->waiting() && d->attachment() == attachment) {
            return d->fallbackRgba();
        }
    }
    return 0x8fa3bfff;
}

int32_t modelDataAlive()
{
    return int32_t(registry().size());
}

} // namespace kourier

ModelData::ModelData(QObject* parent)
  : QObject(parent)
{
    kourier::registry().push_back(this);
}

ModelData::~ModelData()
{
    auto& r = kourier::registry();
    r.erase(std::remove(r.begin(), r.end(), this), r.end());
}

void ModelData::setAttachment(double a)
{
    if (qint64(a) != m_attachment) {
        m_attachment = qint64(a);
        emit attachmentChanged();
    }
}

void ModelData::setFallback(const QString& name)
{
    const QColor c(name);
    if (c.isValid() && c != m_fallback) {
        m_fallback = c;
        emit fallbackChanged();
    }
}

uint32_t ModelData::fallbackRgba() const
{
    return (uint32_t(m_fallback.red()) << 24) | (uint32_t(m_fallback.green()) << 16)
        | (uint32_t(m_fallback.blue()) << 8) | 0xff;
}

void ModelData::take(std::shared_ptr<kourier::ModelBuffers> b)
{
    m_b = std::move(b);
    m_ready = true;
    emit readyChanged();
}

void ModelData::fail()
{
    m_failed = true;
    emit failedChanged();
}

void ModelData::pushTo(QObject* mesh) const
{
    if (mesh == nullptr || !m_b) {
        return;
    }
    mesh->setProperty("positions", QVariant::fromValue(m_b->positions));
    mesh->setProperty("normals", QVariant::fromValue(m_b->normals));
    mesh->setProperty("uv1s", QVariant::fromValue(m_b->uv1s));
    if (!m_b->colors.isEmpty()) {
        mesh->setProperty("colors", QVariant::fromValue(m_b->colors));
    }
}

void ModelData::clear()
{
    const bool was = m_ready;
    m_b.reset();
    m_ready = false;
    m_failed = false;
    if (was) {
        emit readyChanged();
    }
}
