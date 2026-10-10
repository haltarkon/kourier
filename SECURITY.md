# Security policy

Kourier handles private messages, so a security report is taken seriously and handled privately.

## How to report a vulnerability

Do not open a public issue, a public pull request or a public discussion for a vulnerability.
Send the report privately:

- TODO(maintainer): the private channel. When the project is hosted, turn on the host's private
  vulnerability reporting and name it here; also give an e-mail address for reports.
- TODO(maintainer): an OpenPGP key (fingerprint and where to fetch it) for encrypted reports, or
  state that none is offered.

Please include the version (or commit), how you installed it, what you did, what you expected and
what happened. A proof of concept helps; it must not use anyone else's account or data.

## What to expect

Kourier has one maintainer working in spare time, so these are targets, not guarantees:

- an acknowledgment within 7 days;
- an assessment and a plan within 14 days;
- a fix and a coordinated public disclosure within 90 days of the report, sooner when the problem is
  being exploited, and later only by agreement with you;
- credit in the release notes and the advisory, if you want it.

TODO(maintainer): confirm or change these numbers before publishing.

There is no bug bounty.

## Supported versions

There is no release yet. Until there is, only the current state of the main branch is supported.
After the first release, only the latest release is.

## What is in scope

Kourier's own code and the files in this repository:

- reading and storing data: the encrypted databases, the key handling and the Secret Service use,
  the encrypted attachment files, the places plaintext can land (`docs/architecture.md` sections 5
  and 9);
- parsing of data that comes from the network or from other programs: Signal envelopes, protobuf
  messages, attachments, pictures and avatars, rich text, links, the `sgnl://` and
  `signalcaptcha://` handlers and the D-Bus interfaces;
- a leak of message content, names, numbers, keys or tokens into logs, crash reports, file names,
  notifications or error messages;
- the interface showing something other than what was sent (spoofed names, markup injection,
  links that open when they should not);
- the files in `packaging/` and the build and release scripts.

## What is out of scope

- Vulnerabilities in libsignal, in the Signal protocol or in Signal's servers. Report those to
  Signal at <security@signal.org> (Signal's published address for its application and libraries).
  Tell us as well if Kourier is affected in a way Signal's own clients are not.
- Vulnerabilities in Qt, KDE Frameworks, SQLCipher, the Secret Service or other libraries Kourier
  uses, unless Kourier uses them in a way that makes the problem worse. Report those upstream.
- What the threat model does not claim to cover: malware running as your user, access to an
  unlocked session or an unlocked keyring, a compromised phone that holds the primary device, and
  the metadata Signal's servers see.
- Builds made with the `dumpable` or `perf` cargo features (measuring builds, which
  `cargo xtask install` refuses), and the debug-only development hooks (`--dev-control`,
  `KOURIER_DEV_*`), which release builds do not contain.
- Findings that need you to be on someone else's account or network to try them, social
  engineering, and reports from automated scanners with no demonstrated impact.

## Research rules

Test on your own data and your own test account. Do not send traffic to Signal's servers that could
affect other users or the service. If you think you have exposed another person's data, stop,
delete it and tell us.
