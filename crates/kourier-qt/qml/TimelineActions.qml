pragma Singleton

import QtQuick

// What a message row asks the timeline to show: its context menu, the reaction picker, the
// list of who reacted, the delete question. One set of popups serves every row (delegates stay
// light), and rows know which of them the popups are about.
//
// `row` describes the message: `messageId`, `own`, `author`, `actions` (the model's flags),
// `copyText`, `reactions` (the model's chips), `failed` (its send failed), `edited` (it has
// earlier versions), `system` (it is a notice, not a message), `mediaAction` (what activating
// its picture, file or voice note does now: "view", "open", "play", "pause", "download" or
// ""), `mediaLabel` (that action in words), `quoted` (it quotes a message that can be jumped
// to) and `attachments` (its pictures and files, each with `attachment`, `name`, `icon` and
// `thumb`).
QtObject {
    id: actions

    // The message whose menu, picker or reaction list is open (its toolbar stays visible while
    // the pointer is over the popup); 0 for none.
    property real activeMessage: 0
    // The message a quote just jumped to: its row flashes once. 0 for none.
    property real flashMessage: 0
    // Counts jumps, so a second jump to the same message flashes again.
    property int flashSerial: 0

    // Open the context menu of `row` at (`x`, `y`) of `anchor` (a pointer position); with a
    // negative `x`, beside `anchor` instead, so the message stays in view (the Menu key).
    signal menuRequested(Item anchor, var row, real x, real y)
    // Open the reaction picker of `row` at `anchor`.
    signal pickerRequested(Item anchor, var row)
    // Show who reacted to `row`, at `anchor`.
    signal reactionsRequested(Item anchor, var row)
    // Ask how to delete `row` (for everyone or on this account).
    signal deleteRequested(var row)
    // Show the earlier versions of `row` (an edited message).
    signal historyRequested(var row)
    // Do what the message's picture, file or voice note does when it is activated (view it,
    // open it, play it, download it).
    signal activateRequested(real message)
    // Go to the message that `message` quotes.
    signal quoteRequested(real message)
    // Ask which attachment of `row` to delete for this account, and whether to.
    signal attachmentDeleteRequested(var row)
    // Show one step more of the long text of `message` ("Read more").
    signal readMoreRequested(real message)

    function menu(anchor: Item, row: var, x: real, y: real): void {
        actions.menuRequested(anchor, row, x, y);
    }

    function picker(anchor: Item, row: var): void {
        actions.pickerRequested(anchor, row);
    }

    function reactions(anchor: Item, row: var): void {
        actions.reactionsRequested(anchor, row);
    }

    function remove(row: var): void {
        actions.deleteRequested(row);
    }

    function history(row: var): void {
        actions.historyRequested(row);
    }

    function activate(message: real): void {
        actions.activateRequested(message);
    }

    function quote(message: real): void {
        actions.quoteRequested(message);
    }

    function removeAttachment(row: var): void {
        actions.attachmentDeleteRequested(row);
    }

    function readMore(message: real): void {
        actions.readMoreRequested(message);
    }

    function flash(message: real): void {
        actions.flashMessage = message;
        actions.flashSerial += 1;
    }
}
