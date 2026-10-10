import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami

// Older history is loading.
Item {
    width: ListView.view ? ListView.view.width : implicitWidth
    implicitHeight: busy.implicitHeight + Kirigami.Units.largeSpacing * 2

    QQC2.BusyIndicator {
        id: busy
        anchors.centerIn: parent
        running: true
    }
}
