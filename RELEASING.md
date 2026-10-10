# Releasing Kourier

The first release is a GitHub release with downloadable archives and nothing else (decision of
the maintainer, 2026-10-08: "At first we should publish only in archives on github"). Flatpak,
Flathub, distribution packages and the rest come after it.

**Nothing in this repository pushes, uploads or publishes by itself.** `cargo xtask dist` writes
three files on this machine's disk and stops. Every command below that leaves the machine
(`git push`, `gh release create`) is typed by the maintainer, in a session of his own, when he
decides to. A tool or an agent that is not told to do it does not run them.

**Nothing signs the archives.** Neither the two archives nor `SHA256SUMS` carry a signature; the
only signature in the process is the one on the git tag (step 4). `SHA256SUMS` tells a download
from the file that was uploaded, not who made it. A detached signature (`gpg --detach-sign`) is
the maintainer's decision; the tool makes none.

The archives are built on the maintainer's machine, so the binary needs libraries as new as this
machine's Qt 6 and KDE Frameworks 6. It uses Qt's private API, which ties it to one Qt minor
version (`README.txt` in the archive says which, and the KDE Frameworks version it was built with).
The build is **not** reproducible: two builds of one commit give two binaries. What is repeatable
is the packing: the same stage gives the same archive, byte for byte.

**The source archive is the repository only.** The binary links libsignal and hundreds of other
crates; their sources are fetched by cargo and are in neither archive. The AGPL-3.0 asks for the
source of what is passed on. The maintainer's decision (2026-10-08) is not to attach an archive
of the dependencies' sources but to ship the way to get them: the source archive holds
`scripts/fetch-dependency-sources.sh`, which runs `cargo vendor --locked` and so fetches exactly
the versions and git revisions `Cargo.lock` names; the README.txt of the binary archive says so.
`CLAUDE.md` (working notes that name the maintainer's machine) is left out of the source archive
by `.gitattributes` (`export-ignore`).
The notices of those libraries, and of the C libraries and the Rust standard library inside the
binary, are in `THIRD-PARTY.html` in the binary archive.

## What you need

`cargo`, `git`, `tar`, `gzip`, `sha256sum`, `objdump` (binutils), `msgfmt` (gettext),
`cargo-about` (`cargo install cargo-about --locked`), `desktop-file-validate`
(`desktop-file-utils`), `appstreamcli` (`appstream`), `reuse`, `pkg-config` (the KDE Frameworks
version) and `zstd` (the search of the compressed Qt resources; without it the tool says they
were not searched), the toolchain's documentation component (the Rust standard library's
licence files), and for the last two steps `gh` (GitHub's command line tool) and a signing key
for git tags.

## Before the first release (once)

`cargo xtask dist` without `--release` prints what still stands in the way and writes it to
`target/dist/NOT-READY.txt`; with `--release` each item is an error. Today (the list changes, run
the command):

- no repository address: the About data (`crates/kourier-app/src/about.rs`, `homepage`) name none,
  and a binary archive that is passed on alone must say in its `README.txt` where its source is;

- the blanks the maintainer fills: `grep -rn 'TODO(maint' .` (name, contact addresses, the
  project's home);
- the application id `app.kourier.Kourier` is a placeholder and changes with the project's final
  name (CLAUDE.md, Rules);
- tracked files that name the maintainer's directories (CLAUDE.md and the docs do): they are in
  the source archive;
- not checked by the tool, decide before the first push: the git history still carries an old
  name in earlier commits and every commit carries the author's name and address. Rewriting
  history is a decision of the maintainer.

## The steps of a release

All commands run from the repository root, on the commit that is to be released, with a clean
tree.

1. **Version and notes.** Set `version` under `[workspace.package]` in `Cargo.toml`, draft the
   section with `git cliff --unreleased`, edit it into `CHANGELOG.md`, commit.

2. **Checks.**

   ```bash
   cargo xtask ci                      # everything: format, clippy, deps-check, tests, qmllint, screenshots
   cargo xtask perf                    # the full run, 13-15 min; exit 0 = every budget met
   reuse lint
   cargo deny check && cargo audit
   ```

   The real-server smoke run contacts Signal's servers with the dedicated test account and
   needs the maintainer's go in that session. It is a runbook of steps, `docs/smoke-run.md`;
   its start command is

   ```bash
   cargo xtask smoke start --env production --confirmed-by-user --expect-tail <last digits of the test number>
   ```

   followed by the `step …` commands the runbook lists, `stop` and `report`. `ci` and `perf`
   never touch that profile, and `dist` does not run any of this.

3. **The archives.**

   ```bash
   cargo xtask dist --release
   ```

   Its first act is to remove what an earlier run left in `target/dist/` (every
   `kourier-*.tar.gz`, `SHA256SUMS`, `NOT-READY.txt` and the stage directory, nothing else),
   so an old archive never stands beside a refusal. Then it refuses a tree with changes,
   writes `NOT-READY.txt` and stops while a finding is left, and builds the release binary
   into `target/dist-build` with every absolute path of this machine mapped away and with
   `--no-default-features --features signal` (the Signal backend, no test network: the binary
   has no `--fixture`). When the build is done it reads the commit and `git status` again and
   fails if either moved, and writes a stamp beside the binary (commit, dirty flag, features,
   Qt, KDE Frameworks and rustc versions, checksum of the binary). It searches the binary,
   and the decompressed zstd frames of its Qt resources, for the home directory, the user
   name, `/media/`, the host name, git's `user.name` and `user.email` and the project's former
   name, stages the installed tree (the desktop file claims no `sgnl://` or
   `signalcaptcha://` link), packs, and then unpacks what it packed and checks it. The
   result, in `target/dist/`:

   ```
   kourier-<version>-linux-<arch>.tar.gz    the installed tree, README.txt, licences
   kourier-<version>-source.tar.gz          git archive of the commit
   SHA256SUMS                               over both
   ```

   Only a `--release` run that found nothing writes these bare names. Every other run
   writes `…-unreleased.tar.gz` (a tree with changes: `…-dirty.tar.gz`), and the top
   directory inside each archive ends the same way.

   The command prints the requirements it read from the binary (shared libraries, glibc,
   libstdc++, the Qt minor version and why, QML modules, what is needed at run time) and the
   sizes. Read them once: they go into `README.txt` in the archive. Look at the tree it
   unpacked as a user would:

   ```bash
   mkdir -p /tmp/kourier-try && tar -xzf target/dist/kourier-*-linux-*.tar.gz -C /tmp/kourier-try
   /tmp/kourier-try/kourier-*/bin/kourier --version
   (cd target/dist && sha256sum -c SHA256SUMS)
   ```

   Put the repository address into the About data first (`homepage` in
   `crates/kourier-app/src/about.rs`) or the run stops at the finding about it.

4. **A signed tag on the commit the archives were made from.**

   ```bash
   git tag -s v0.1.0 -m "Kourier 0.1.0"
   git tag -v v0.1.0
   test "$(git rev-parse 'v0.1.0^{commit}')" = "$(git rev-parse HEAD)" && echo same commit
   ```

5. **Only when the maintainer says so: publish.** The first push sends the whole history to
   GitHub; read "Before the first release" first.

   ```bash
   git remote add origin <the repository's URL>      # first time only
   git push origin main v0.1.0
   gh release create v0.1.0 --draft --verify-tag --title "Kourier 0.1.0" \
       --notes-file release-notes.md \
       target/dist/kourier-0.1.0-linux-x86_64.tar.gz \
       target/dist/kourier-0.1.0-source.tar.gz \
       target/dist/SHA256SUMS
   ```

   `--draft` keeps the release invisible. Open it on GitHub, read the notes and the file list,
   and press "Publish release" by hand. GitHub adds its own "Source code" archives to every
   release; the one attached here is `git archive` of the tagged commit with a fixed set of
   modes and a top directory named for the version, which is why it is attached as well. It
   is not signed (see the top of this file): the tag is.

6. **After publishing**, from a clean directory: download the three files, run
   `sha256sum -c SHA256SUMS`, unpack the archive and run `bin/kourier --version`.

## What `cargo xtask dist` checks, and what it does not

**`--release` guarantees**, when it ends without an error:

- the names are the bare ones, and they are the commit's: the tree had no changes, the commit
  and `git status` were the same at the end of the build as at its start, the binary's stamp
  names that commit, a clean tree, the feature set `--no-default-features --features signal`
  and the checksum of the binary that was packed, and the source archive is
  `git archive <that commit hash>` (not `HEAD`) with modes 0644 and 0755 (it carries the commit
  in its header, which the tool compares);
- no finding of `NOT-READY.txt` is left: no maintainer blank, no placeholder application id, no
  tracked file that names a private path, a repository address in the About data;
- the binary is no measuring build (never `perf` or `dumpable`) and has no test network:
  `bin/kourier --version` prints `Kourier <version>`, `--fixture` is refused, and no code of
  the test network crate is in it;
- the home directory, the user name, a rooted `/media/`, the host name, git's `user.name` and
  `user.email` and the project's former name are in neither the binary (also inside its zstd
  frames, when `zstd` is installed) nor any packed file;
- the archive holds exactly the expected paths, modes 0755/0644, owner 0, the commit's time as
  time stamp; `desktop-file-validate` and `appstreamcli validate` pass; the desktop file claims
  no `x-scheme-handler`; `THIRD-PARTY.html` has every crate and a section "Bundled libraries"
  (SQLCipher by Zetetic, SQLite, BoringSSL, the Rust standard library: the licence files of
  this machine's checkout and toolchain), and the check fails without "Zetetic" and
  "BoringSSL" in it;
- `README.txt` names the Qt it was built with and why only that minor version runs it, the KDE
  Frameworks version, the glibc and libstdc++ versions the binary needs (a weak symbol sets
  none), what is needed at run time that the binary cannot show, and the repository address;
- the checksums match.

`--no-build` and `--allow-dirty` also check the stamp (a binary built from another commit or
tree state, with other features, or replaced since, is refused); they never write the bare
names. With a tree that has changes the stamp cannot tell which changes it was built from.

**It does not check:** that the application starts and works (that is `ci`, `perf` and the
smoke run), that the build is reproducible, that other distributions can run the binary, that
the licence texts are complete beyond generating them with `cargo about` and copying the files
named above, that the dependencies' sources are attached (they are not), or that anything is
signed (nothing is).

A development build for looking at: `cargo xtask dist --allow-dirty` (names end in `-dirty`,
the source archive is still the last commit). `--allow-paths` lets a binary with private names
through for a build that stays on this machine; `--release` refuses it, and refuses
`--allow-dirty` and `--no-build`.
