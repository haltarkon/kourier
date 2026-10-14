import QtQuick
import org.kde.kirigami as Kirigami
import app.kourier

// Older history is loading.
Item {
    width: ListView.view ? ListView.view.width : implicitWidth
    implicitHeight: busy.implicitHeight + Kirigami.Units.largeSpacing * 2

    Spinner {
        id: busy
        anchors.centerIn: parent
    }
}
