// The 3D viewer's mesh as Quick 3D wants it (`.claude/rules`: the viewer is created on the first
// open; this class imports no Quick 3D, it only holds `QList`s). The core's mesh arrives as plain
// float buffers behind an `Arc` on the Rust side; a worker thread builds the lists
// (`buildModelBuffers`, no Qt object touched), the Qt thread hands them to the `ModelData` that
// waits for the attachment (`deliverModel`). The viewer pushes them into the `ProceduralMesh` with
// `pushTo` (implicit sharing: no copy) and calls `clear()` when it closes.
// (An include guard, not `#pragma once`: the header is reached by two paths, `cpp/` for moc and the
// copy cxx-qt makes for the bridge, which are two files to the compiler.)
#ifndef KOURIER_MODELDATA_H
#define KOURIER_MODELDATA_H

#include <QtCore/QList>
#include <QtCore/QObject>
#include <QtGui/QColor>
#include <QtGui/QVector2D>
#include <QtGui/QVector3D>
#include <QtGui/QVector4D>
#include <QtQml/qqmlregistration.h>

#include <memory>

#include "rust/cxx.h"

namespace kourier {

// What the worker made: plain lists and the box, no QObject.
struct ModelBuffers {
    QList<QVector3D> positions;
    QList<QVector3D> normals;
    QList<QVector2D> uv1s;
    QList<QVector4D> colors;
    QVector3D min;
    QVector3D max;
    qint64 triangles = 0;
};

// Worker thread: `positions` are nine floats per triangle (mm), `colors` one `0xRRGGBBAA` per
// triangle (0 = none) or empty for a model without colours; a triangle without a colour gets
// `fallback` (`0xRRGGBBAA`).
std::unique_ptr<ModelBuffers> buildModelBuffers(
    rust::Slice<const float> positions, rust::Slice<const uint32_t> colors, uint32_t fallback,
    rust::Slice<const float> min, rust::Slice<const float> max);

// Any thread: gives `buffers` to the `ModelData` waiting for `attachment` on the Qt thread's next
// turn (dropped when none waits any more).
void deliverModel(int64_t attachment, std::unique_ptr<ModelBuffers> buffers);

// Qt thread: the open of `attachment` failed (`attachment` 0: the oldest waiting `ModelData`).
void failModel(int64_t attachment);

// Qt thread: the fallback colour (`0xRRGGBBAA`) of the `ModelData` waiting for `attachment`; the
// neutral blue when none waits.
uint32_t waitingFallback(int64_t attachment);

// Number of `ModelData` objects alive (tests: the lists are dropped with the viewer).
int32_t modelDataAlive();

} // namespace kourier

class ModelData : public QObject
{
    Q_OBJECT
    QML_ELEMENT
    // The attachment this object waits for; set before `App.openModel`.
    Q_PROPERTY(double attachment READ attachment WRITE setAttachment NOTIFY attachmentChanged FINAL)
    // The colour of the triangles a coloured model gives none (the model colour setting).
    Q_PROPERTY(QString fallback READ fallback WRITE setFallback NOTIFY fallbackChanged FINAL)
    Q_PROPERTY(bool ready READ ready NOTIFY readyChanged FINAL)
    Q_PROPERTY(bool failed READ failed NOTIFY failedChanged FINAL)
    Q_PROPERTY(bool coloured READ coloured NOTIFY readyChanged FINAL)
    Q_PROPERTY(int triangles READ triangles NOTIFY readyChanged FINAL)
    // Size of the box in millimetres, and its centre (plain numbers: the module's type table
    // names no QVector3D for the linter).
    Q_PROPERTY(double boxX READ boxX NOTIFY readyChanged FINAL)
    Q_PROPERTY(double boxY READ boxY NOTIFY readyChanged FINAL)
    Q_PROPERTY(double boxZ READ boxZ NOTIFY readyChanged FINAL)
    Q_PROPERTY(double centerX READ centerX NOTIFY readyChanged FINAL)
    Q_PROPERTY(double centerY READ centerY NOTIFY readyChanged FINAL)
    Q_PROPERTY(double centerZ READ centerZ NOTIFY readyChanged FINAL)
    Q_PROPERTY(QList<QVector3D> positions READ positions NOTIFY readyChanged FINAL)
    Q_PROPERTY(QList<QVector3D> normals READ normals NOTIFY readyChanged FINAL)
    Q_PROPERTY(QList<QVector2D> uv1s READ uv1s NOTIFY readyChanged FINAL)
    Q_PROPERTY(QList<QVector4D> colors READ colors NOTIFY readyChanged FINAL)

public:
    explicit ModelData(QObject* parent = nullptr);
    ~ModelData() override;

    double attachment() const { return double(m_attachment); }
    void setAttachment(double a);
    QString fallback() const { return m_fallback.name(); }
    void setFallback(const QString& c);
    bool ready() const { return m_ready; }
    bool failed() const { return m_failed; }
    bool coloured() const { return !m_b || !m_b->colors.isEmpty(); }
    int triangles() const { return m_b ? int(m_b->triangles) : 0; }
    QVector3D boxMm() const { return m_b ? m_b->max - m_b->min : QVector3D(); }
    QVector3D centerMm() const { return m_b ? (m_b->max + m_b->min) / 2 : QVector3D(); }
    double boxX() const { return boxMm().x(); }
    double boxY() const { return boxMm().y(); }
    double boxZ() const { return boxMm().z(); }
    double centerX() const { return centerMm().x(); }
    double centerY() const { return centerMm().y(); }
    double centerZ() const { return centerMm().z(); }
    QList<QVector3D> positions() const { return m_b ? m_b->positions : QList<QVector3D>(); }
    QList<QVector3D> normals() const { return m_b ? m_b->normals : QList<QVector3D>(); }
    QList<QVector2D> uv1s() const { return m_b ? m_b->uv1s : QList<QVector2D>(); }
    QList<QVector4D> colors() const { return m_b ? m_b->colors : QList<QVector4D>(); }

    // Sets the lists on a `ProceduralMesh` from C++ (no JS array in between).
    Q_INVOKABLE void pushTo(QObject* mesh) const;
    // Drops the lists (the viewer closes); the object waits for nothing afterwards.
    Q_INVOKABLE void clear();

    // The colour as the worker takes it (`0xRRGGBBAA`).
    uint32_t fallbackRgba() const;
    // Called by `deliverModel` / `failModel` on the Qt thread.
    void take(std::shared_ptr<kourier::ModelBuffers> b);
    void fail();
    bool waiting() const { return m_attachment > 0 && !m_ready && !m_failed; }

signals:
    void attachmentChanged();
    void fallbackChanged();
    void readyChanged();
    void failedChanged();

private:
    qint64 m_attachment = 0;
    QColor m_fallback = QColor(0x8f, 0xa3, 0xbf);
    bool m_ready = false;
    bool m_failed = false;
    std::shared_ptr<kourier::ModelBuffers> m_b;
};

#endif // KOURIER_MODELDATA_H
