# Privacy

This describes what Kourier does with your data, as of this version of the code. The sources are
`docs/architecture.md` sections 5 (storage) and 9 (logging, security), and the code itself. If the
code and this page disagree, that is a bug in one of them: please report it.

Kourier is an unofficial client for Signal. Signal's own privacy policy
(<https://signal.org/legal/>) governs what Signal's servers do with your account and traffic.

## No telemetry

Kourier has no telemetry, no analytics, no usage statistics, no advertising, no accounts of its
own, no update check and no server of its own. It sends nothing to the developers.

If it crashes, it writes a report (version, thread, backtrace, the last 200 redacted log lines) to
a file on your computer. The next start offers to show it; nothing is uploaded, and you decide
whether to copy it into a bug report.

## Where it connects

Only to Signal's service: the chat servers, the Storage Service and Signal's file hosts (CDNs),
checked against Signal's own certificate authorities and trust roots. The connection comes from
your computer's address, as with any Signal client.

- To find those servers, Kourier uses libsignal's name lookup: your system's resolver first, then
  DNS-over-HTTPS to Cloudflare (1.1.1.1), then addresses built into the library. If your resolver
  fails, Cloudflare sees that your computer looked up Signal's host names.
- Signal's servers see Kourier's name and version in the user agent (`Kourier/<version>
  libsignal/<version>`) and the linked device's name, which you choose when you link. The device
  shows in the list of linked devices in the Signal app on your phone, where you can remove it at
  any time.
- A link in a message is never fetched for a preview, and a link you drop on the composer is not
  fetched. A link opens in your browser only after you click it, and only when it is an `https`
  address.

## What is stored, and where

Directories follow the XDG layout; `--profile <name>` and `KOURIER_HOME` change them. Directories
are mode 0700, files 0600.

| Where | What | Protection |
|---|---|---|
| `~/.local/share/kourier/core.db` | conversations, messages, contacts, settings, attachment keys | SQLCipher 4 database, key derived from the master key |
| `~/.local/share/kourier/accounts/<id>/backend.db` | Signal identity and session keys, group secrets, profile keys | SQLCipher 4 database, its own random key per account |
| `~/.local/share/kourier/accounts/<id>/key.wrap` | that account's database key | encrypted (AES-256-GCM) under a key made from the master key and a per-account secret in the Secret Service |
| `~/.local/share/kourier/accounts/<id>/blobs/` | received and sent files, thumbnails, avatars | encrypted file by file (AES-256-GCM, a random key per file, kept in `core.db`) |
| the Secret Service (KWallet or GNOME Keyring) | the master key and one secret per account | the keyring's own protection (your login password) |
| `~/.config/kourier/kourier.toml` | preferences (tray, autostart, notification content, compact layout) | none; no secrets in it |
| `~/.local/state/kourier/logs/` | rotating logs (7 files of 10 MiB) and crash reports | redacted, see below |
| `~/.cache/kourier/` | Qt's QML cache; folders packed as `.zip` for sending (`outgoing/`) | the `.zip` is plaintext (mode 0600) and is removed after use, at exit and at the next start |
| `$XDG_RUNTIME_DIR/kourier/` | the single-instance lock; plaintext copies of media while they play and pictures pasted into the composer | memory-backed (tmpfs); deleted when the view closes, at exit and at start |

When no Secret Service is available (or you decline to unlock it), Kourier asks for a passphrase at
start and derives the master key from it with Argon2id. There is no plaintext fallback.

Still pictures and avatars are decrypted in memory only. A file you save or open leaves Kourier's
protection: it is a normal file in the place you chose.

**Logs.** Kourier never writes message texts, captions, file names, contact or account names, phone
numbers, keys or tokens to logs. Service ids appear shortened to their last four digits and phone
numbers as `+[number]`. A test checks the logs of the end-to-end suite for the names, numbers and
texts of its fixture contacts. Redaction is a safeguard, not a promise about every possible line:
read a log before you share it.

**Notifications.** A notification goes to your desktop's notification service. By default it shows
the sender and the message text, which can appear on a lock screen or on a screen you share. The
setting "notification content" can reduce it to the sender, or to "New message".

**Disappearing messages.** A message whose timer ran out is erased from the database and its files
are removed. When the clock has not yet been confirmed against a server, a setting decides whether
Kourier hides what is due, keeps it, or erases it by your computer's clock.

## Deleting your data

1. Remove the linked device: in the Signal app on your phone, Settings, Linked devices, remove
   Kourier. Signal then refuses the device's credentials, and Kourier, at its next connection,
   treats the account as unlinked and deletes that account's data and its secret in the Secret
   Service, which makes the account's database unreadable even where old copies of the file remain
   on a disk. (The core has a command to remove an account; the interface has no button for it
   yet.)
2. Delete `~/.local/share/kourier/`, `~/.config/kourier/`, `~/.local/state/kourier/` and
   `~/.cache/kourier/`.
3. Delete Kourier's items from the Secret Service (look for the application id in
   `packaging/*.desktop`; it is `app.kourier.Kourier` for now and changes with the project's name).

Limits you should know: deleted messages leave the live database and its logs, but blocks the file
system wrote earlier on an SSD can stay readable to someone who also has the master key. Old
backups of the keyring file are protected only by your login password. Kourier cannot protect
messages from malware running as your user or from someone using your unlocked session.

## Questions and corrections

TODO(maintainer): a contact address or the place for privacy questions.
