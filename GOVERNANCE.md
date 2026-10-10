# Governance

Kourier is a small project with one maintainer. This page says how decisions are made today, so that
contributors know what to expect, and how that can change.

## Roles

- **Maintainer.** Decides what goes into the project, reviews and merges changes, makes releases,
  and enforces the [Code of Conduct](CODE_OF_CONDUCT.md). Today: TODO(maintainer): your name, and
  how people reach you (see also [SECURITY.md](SECURITY.md) for private reports).
- **Contributors.** Anyone who sends a change, a report, a review or a translation. They sign off
  their commits under the [Developer Certificate of Origin](CONTRIBUTING.md), keep their own
  copyright and give no copyright assignment.

## How decisions are made

- The maintainer decides, after hearing the arguments in the open issue or pull request. A
  disagreement is settled by the maintainer; a contributor who disagrees can fork, which the
  license permits.
- Decisions that shape the project are written down in `docs/architecture.md`, in the tables of
  decisions with their reasons, and the document is changed in the same change as the code. That
  record, not the discussion, is what holds.
- Some things are fixed unless there is a very good reason: the license (AGPL-3.0-only, because
  libsignal is), the DCO instead of a CLA, never using "Signal" in the project's name or icon, no
  telemetry, and the rule that tests never touch a real Signal account.
- Changes to this page, to the license or to the Code of Conduct are made in the open, with time
  for comments before they are merged.

## Becoming a maintainer

Maintainers are added by the current maintainer. The usual path is a record of good changes and
careful reviews over some months, an understanding of the design and its rules, and agreement with
the Code of Conduct. A maintainer who has been inactive for six months may be moved to emeritus
status; they can return on request.

TODO(maintainer): confirm or change this path and the inactivity period; they are a proposal.

## Continuity

Everything depends on one person today. The goal is at least two people with administrative and
release rights on the repository, the release signing keys and any accounts the project holds
before a 1.0 release. TODO(maintainer): name the person who can take over if you cannot, and say
where the project's domain, accounts and signing keys are kept once they exist (today there are
none).

## Name and trademarks

"Kourier" is a working name, and the project will be renamed before its first public release.
TODO(maintainer): replace this with the final name. The project is not affiliated with Signal
Messenger, LLC, and does not use "Signal" in its name, domain, handles or icon.
