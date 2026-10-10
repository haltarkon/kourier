import QtQuick

// The roles of one row of the composer's file list (`AttachmentListModel`), so the list can
// ask its current row what it is.
Item {
    required property int index
    required property string name
    required property string kind
    required property string icon
    required property string preview
    required property string size
    required property string reason
    // The number of the message it travels in (0: not known, or it stays behind).
    required property int part
    required property bool partFirst
    required property bool partLast
    // The line of a message passes this row, which is not part of it.
    required property bool partThrough
    // A picture or a video: it can be sent as the file it is.
    required property bool canOriginal
    // It is sent as the file it is.
    required property bool original
    // Something to know about it that holds nothing back (empty for nothing); for a folder,
    // what is happening to it.
    required property string note
    // A folder that is not files or an archive yet: "counting", "asking", "listing" or
    // "packing"; empty for a file.
    required property string stage
    // How far a folder is packed, 0 to 1.
    required property real progress
}
