// A Quick item that does nothing but report the end of a polish (`.claude/rules/timeline.md`,
// "The polish hook"). `request()` asks the scene for a polish of this item; `QQuickWindowPrivate::
// polishItems` runs it right after the item that is being polished when the request came from
// inside that item's own `updatePolish` or layout, and before the scene graph is synced, so what
// `due()` does lands in the same frame. The timeline uses it to move its `contentY` and lay the
// list out again OUTSIDE the list's own layout loop: a write of `contentY` inside the loop made
// the ListView refill and release the rows the loop was still walking (a use-after-free in
// `QQuickListViewPrivate::layoutVisibleItems`). No content, no size, nothing drawn.
#pragma once

#include <QtQml/qqmlregistration.h>
#include <QtQuick/QQuickItem>

class PolishHook : public QQuickItem
{
    Q_OBJECT
    QML_ELEMENT

public:
    explicit PolishHook(QQuickItem* parent = nullptr)
      : QQuickItem(parent)
    {}

    /// Asks for `due()` at the end of the current (or the next) polish pass.
    Q_INVOKABLE void request() { polish(); }

signals:
    void due();

protected:
    void updatePolish() override { emit due(); }
};
