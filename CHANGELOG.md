# Changelog

All notable changes to this project are written here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

There is no release yet. Until the first one, the history is the git log; a draft of each release's
section is made with `git cliff --unreleased` (configuration in `cliff.toml`) and edited by hand
into notes a user can read.

## [Unreleased]

Pre-release work on version 0.1.0. See README.md for what works today.

- A new link now offers the message history transfer from the phone, as Signal Desktop does (the
  phone asks the user, who can decline there); `KOURIER_HISTORY_TRANSFER=0` turns the offer off
  for one run.
- Pictures in a chat are drawn from the received file, not from the small 400 px preview, wherever
  the preview would be stretched: tall pictures and phone screenshots, and every picture on a
  display with a scale above 100 %. They were soft there.
