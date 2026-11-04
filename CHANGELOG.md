# Changelog

All notable changes to this project are written here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

There is no release yet. Until the first one, the history is the git log; a draft of each release's
section is made with `git cliff --unreleased` (configuration in `cliff.toml`) and edited by hand
into notes a user can read.

## [Unreleased]

Pre-release work on version 0.1.0. See README.md for what works today.

- Chats of several accounts in one list: the side panel chooses which accounts' chats the list
  shows. A click on an account shows only it, Shift+click or Ctrl+click (also Ctrl+Space, a check box
  and the entry's menu) adds or removes one, and every account shown is mixed into one list, newest
  first, with pinned chats of each account on top. Hidden accounts are faded in the panel, still show
  their unread count, and a search looks in them too. A badge number on the avatar tells apart two
  accounts of the same network. In a narrow window the menu button counts the unread messages of
  hidden accounts. From the keyboard the side panel is one Tab stop (in a narrow window Shift+Tab from
  the search field reaches the menu button, and Space on it opens the panel): Up and Down move between
  its entries and Return or Space shows only the focused account. The choice
  is remembered; Settings > General > "Show chats of several accounts together" turns it off (a click
  then only switches the account).
- Archived chats can be opened: the chats you archived on your phone are listed under "Archived
  Chats (N)" at the end of the chat list, and a search finds them after the other matches. A new
  message brings a chat back to your chats (a muted one too, unless "Keep muted chats archived" is
  on in Signal's settings on your phone), as in Signal. Unread messages in archived chats count
  nowhere. Opening this version migrates the Signal data of each account (back up
  `~/.local/share/kourier` first).
- Safety number changes, refreshed sessions, changed phone numbers and accepted message requests
  no longer show as a chat's last message or move the chat up the list; they stay in the chat
  itself. Settings > General > "Show minor notices in the conversation list" brings them back.
  Opening this version migrates the chat database (back up `~/.local/share/kourier` first); a
  chat that ended in such a notice moves down once, and a chat that held nothing but such
  notices is hidden until a real message comes.
- A new link now offers the message history transfer from the phone, as Signal Desktop does (the
  phone asks the user, who can decline there); `KOURIER_HISTORY_TRANSFER=0` turns the offer off
  for one run.
- Pictures and videos in a chat are shown whole, not cropped: a single one fits a box 16 grid
  units wide and at most 1.5 times as tall (a phone screenshot is whole at about 200 px wide), a
  very thin one sits whole on the bubble's own colour, and an album shows up to five in rows of
  whole pictures, "+N" on the last. The small previews (a quoted message, the media page, the
  composer's list) stay squares.
- Pictures in a chat are drawn from the received file, not from the small 400 px preview, wherever
  the preview would be stretched: tall pictures and phone screenshots, and every picture on a
  display with a scale above 100 %. They were soft there.
