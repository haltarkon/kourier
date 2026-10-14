import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

// "Busy": a QQC2.BusyIndicator, which in a picture taken for comparison (`--screenshot`) stands
// still as an icon instead: a turning spinner is never the same twice, and the picture is only
// taken when nothing moves (architecture §13, "Stable screenshots"). The desktop style hides a
// stopped BusyIndicator, which is why the still one is an icon. Use it for every spinner;
// `visible` and the size work as they do for the indicator (the size defaults to the indicator's).
Item {
    id: root

    // Whether it is busy (a spinner that is not running draws nothing).
    property bool running: true
    // The picture for comparison: the spinner stands still.
    readonly property bool still: App.screenshot !== ""

    implicitWidth: busy.implicitWidth
    implicitHeight: busy.implicitHeight

    QQC2.BusyIndicator {
        id: busy
        anchors.fill: parent
        visible: !root.still
        running: root.running && root.visible && !root.still
    }
    Kirigami.Icon {
        anchors.fill: parent
        visible: root.still && root.running
        source: "process-working-symbolic"
    }
}
