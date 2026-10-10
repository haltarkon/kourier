# Accessibility

This is a short, modest statement of what is in the code today. Kourier has not been tested with a
screen reader by a person yet, so treat everything below as "built to", not as "verified".

## What is built

Kourier's interface is Qt Quick with Kirigami. Their controls take part in the platform's
accessibility interface (AT-SPI on Linux), and Kourier adds names, roles and descriptions to its
own items. In `crates/kourier-qt/qml/` this includes:

- the message list as a list with the name "Messages"; every message as a list item whose name
  holds the author and the time and whose description holds the state (edited, failed, delivered
  and so on); a deleted message says so; system notes ("safety number changed") are list items too;
- conversations in the chat list, named by their title;
- buttons with names for reactions (with their count and who reacted), for "Read more", for the
  edit history, and for a quoted message (which also acts as a button);
- pictures and albums named as such ("Album of 4 pictures");
- the account banner (offline, logged out, a challenge) as a status bar;
- names for the fields of the custom timer dialog, the file list in the composer, the list of
  attachments in the delete question, the emoji categories and the folder question.

Keyboard use, without a mouse:

- Tab moves the focus between the main areas of the window.
- In the message list: Up and Down move the current message; Enter or Space shows more of a long
  text, or opens the row's picture, file or voice note, or else opens the row's menu; Ctrl+Enter
  goes to the message a reply quotes; the Menu key or Shift+F10 opens the message menu; Escape
  returns to the composer.
- In the composer: Enter sends, Shift+Enter starts a new line, Up in an empty composer edits your
  last message, Escape cancels an edit or a reply, Ctrl+V pastes files or a picture. In the strip
  of waiting files, Delete removes the file.
- In the picture viewer: Left and Right step through the pictures. In the emoji grid: Enter or
  Space picks.

The interface follows the system's color scheme and font through Qt and Kirigami, and has a narrow
layout for small windows.

## Known gaps

- **No manual screen-reader pass.** Nobody has used Kourier with Orca yet, so names, reading order
  and the amount of speech are unchecked.
- **No announcement of new messages.** A message that arrives is not announced to a screen reader.
- **Focus order and focus visibility have not been audited** across all dialogs.
- **No shortcuts yet** for switching chats, searching, or marking a chat read, and they cannot be
  configured.
- **Contrast.** A review of the theme on 2026-10-06 found text that is below the WCAG AA ratio of
  4.5:1 in a few places (for example the accent color behind your own messages and some secondary
  text). Fixing this is planned before a first release; until then, assume it is not met everywhere.
- **High-contrast schemes, large fonts and right-to-left languages** have not been checked.
- **Pictures** are named only as "Picture", "Video" or "Album of N pictures"; Kourier has no
  description for what they show.
- **No automated accessibility tests.** The interface tests send keys and pointer events, but no
  test reads the accessibility tree.

## Reporting a problem

Please report anything that is hard to use with a keyboard, a screen reader, a large font or a
high-contrast scheme. Say which assistive technology and which desktop you use.
TODO(maintainer): name the issue label or place for accessibility reports once the project is
hosted.
