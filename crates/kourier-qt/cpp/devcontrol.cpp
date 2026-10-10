#include "kourier-qt/cpp/devcontrol.h"

#include <QtCore/QByteArray>
#include <QtCore/QCoreApplication>
#include <QtCore/QMetaObject>
#include <QtCore/QVariant>
#include <QtGui/QGuiApplication>
#include <QtGui/QMouseEvent>
#include <QtGui/QWindow>
#include <QtQuick/QQuickWindow>

namespace kourier::dev {

QObject* mainWindow()
{
    // Popups shown as their own windows (menus, tooltips) have a transient parent and can come
    // first in the list: prefer the window without one.
    const auto windows = QGuiApplication::topLevelWindows();
    QQuickWindow* fallback = nullptr;
    for (QWindow* window : windows) {
        if (auto* quick = qobject_cast<QQuickWindow*>(window)) {
            if (quick->transientParent() == nullptr) {
                return quick;
            }
            if (fallback == nullptr) {
                fallback = quick;
            }
        }
    }
    return fallback;
}

bool invokeQml(QObject* object, const QString& method)
{
    if (object == nullptr) {
        return false;
    }
    const QByteArray name = method.toUtf8();
    return QMetaObject::invokeMethod(object, name.constData(), Qt::DirectConnection);
}

bool invokeQmlString(QObject* object, const QString& method, const QString& arg)
{
    if (object == nullptr) {
        return false;
    }
    const QByteArray name = method.toUtf8();
    return QMetaObject::invokeMethod(object, name.constData(), Qt::DirectConnection, Q_ARG(QString, arg));
}

bool invokeQmlNumber(QObject* object, const QString& method, double arg)
{
    if (object == nullptr) {
        return false;
    }
    const QByteArray name = method.toUtf8();
    return QMetaObject::invokeMethod(object, name.constData(), Qt::DirectConnection, Q_ARG(double, arg));
}

QObject* objectProperty(QObject* object, const QString& name)
{
    if (object == nullptr) {
        return nullptr;
    }
    const QByteArray key = name.toUtf8();
    return object->property(key.constData()).value<QObject*>();
}

bool setBoolProperty(QObject* object, const QString& name, bool value)
{
    if (object == nullptr) {
        return false;
    }
    const QByteArray key = name.toUtf8();
    if (object->metaObject()->indexOfProperty(key.constData()) < 0) {
        return false;
    }
    return object->setProperty(key.constData(), value);
}

bool resizeWindow(QObject* window, int width, int height)
{
    auto* w = qobject_cast<QWindow*>(window);
    if (w == nullptr) {
        return false;
    }
    w->resize(width, height);
    return true;
}

int64_t windowSize(QObject* window)
{
    auto* w = qobject_cast<QWindow*>(window);
    if (w == nullptr) {
        return -1;
    }
    return static_cast<int64_t>(w->width()) * 65536 + w->height();
}

bool dragMouse(QObject* window, double x1, double y1, double x2, double y2, int steps)
{
    auto* w = qobject_cast<QWindow*>(window);
    if (w == nullptr || steps < 1) {
        return false;
    }
    const auto send = [w](QEvent::Type type, QPointF pos, Qt::MouseButton button, Qt::MouseButtons buttons) {
        QMouseEvent event(type, pos, w->mapToGlobal(pos), button, buttons, Qt::NoModifier);
        QCoreApplication::sendEvent(w, &event);
        QCoreApplication::processEvents();
    };
    const QPointF from(x1, y1);
    const QPointF to(x2, y2);
    send(QEvent::MouseMove, from, Qt::NoButton, Qt::NoButton);
    send(QEvent::MouseButtonPress, from, Qt::LeftButton, Qt::LeftButton);
    for (int i = 1; i <= steps; ++i) {
        const QPointF p = from + (to - from) * (static_cast<double>(i) / steps);
        send(QEvent::MouseMove, p, Qt::NoButton, Qt::LeftButton);
    }
    send(QEvent::MouseButtonRelease, to, Qt::LeftButton, Qt::NoButton);
    return true;
}

bool windowVisible(QObject* window)
{
    auto* w = qobject_cast<QWindow*>(window);
    return w != nullptr && w->isVisible();
}

void closeWindow(QObject* window)
{
    if (auto* w = qobject_cast<QWindow*>(window)) {
        w->close();
    }
}

void quitApp(int code)
{
    QCoreApplication::exit(code);
}

} // namespace kourier::dev
