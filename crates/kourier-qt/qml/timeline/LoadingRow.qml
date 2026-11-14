import QtQuick
import org.kde.kirigami as Kirigami
import app.kourier

// Older history is loading.
Item {
    // The row's number in the model: the timeline finds the row it holds on to by it.
    required property int index

    width: ListView.view ? ListView.view.width : implicitWidth
    implicitHeight: busy.implicitHeight + Kirigami.Units.largeSpacing * 2

    Spinner {
        id: busy
        anchors.centerIn: parent
    }
}
