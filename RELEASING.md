# Releasing Kourier

The first release is a GitHub release with downloadable archives and nothing else (decision of
the maintainer, 2026-10-08: "At first we should publish only in archives on github"). Flatpak,
Flathub, distribution packages and the rest come after it.

**Nothing in this repository pushes, uploads or publishes by itself.** `cargo xtask dist` writes
five files on this machine's disk and stops. Every command below that leaves the machine
(`git push`, `gh release create`) is typed by the maintainer, in a session of his own, when he
decides to. A tool or an agent that is not told to do it does not run them.

**Nothing signs the archives.** None of the archives (binary, source, names) nor the two sums files carry a signature; the
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

**The binary carries no symbol table.** `cargo xtask dist` builds the release binary with its
function names (the profile keeps `strip = "debuginfo"`, so every other build keeps them), then
splits them off: `objcopy --only-keep-debug` writes the names file
`kourier-<build-id>.debug`, and `objcopy --strip-all --add-gnu-debuglink` writes the file that
is packed (about 51 MiB instead of 77). The names file goes into its own archive,
`kourier-<version>-linux-<arch>-names[-suffix].tar.gz`, with its own `SHA256SUMS.names`. The
build cannot be repeated (see above), so a names file that is lost after the binary was
published can never be made again: step 3 keeps a copy outside `target/`. A crash report that
shows absolute addresses, or a `RUST_BACKTRACE` stack, is read with the names file of that
release (the commands are in the archive's `README.txt`; a names file of another release gives
wrong names silently in a backtrace, which is why its name carries the build-id).
Packagers: a Flatpak built from source strips and writes its `.Debug` extension itself, from the
unstripped profile build (never set `strip = "symbols"` in the profile); an AppImage recipe uses
`names::split` (or `NO_STRIP=1` after the split) because linuxdeploy strips every ELF with a
`strip` older than RELR; distro packagers' debuginfo tools work on the unstripped build as usual.

## What you need

`cargo`, `git`, `tar`, `gzip`, `sha256sum`, `objdump` (binutils), `msgfmt` (gettext),
`objcopy`, `readelf` and `nm` (binutils, the names split and its checks), `cargo-about`
(`cargo install cargo-about --locked`), `desktop-file-validate` (`desktop-file-utils`), `appstreamcli` (`appstream`), `reuse`, `pkg-config` (the KDE Frameworks
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
   `kourier-*.tar.gz`, `SHA256SUMS`, `SHA256SUMS.names`, `NOT-READY.txt` and the stage directory, nothing else),
   so an old archive never stands beside a refusal. Then it refuses a tree with changes,
   writes `NOT-READY.txt` and stops while a finding is left, and builds the release binary
   into `target/dist-build` with every absolute path of this machine mapped away and with
   `--no-default-features --features signal,whatsapp` (the Signal backend and the WhatsApp
   backend, which a release build compiles in and does not register; no test network: the binary
   has no `--fixture`). When the build is done it reads the commit and `git status` again and
   fails if either moved, and writes a stamp beside the binary (commit, dirty flag, features,
   Qt, KDE Frameworks and rustc versions, checksum of the binary). It prints `objcopy
   --version`, splits the binary (a build with debug information is refused: a policy of the
   release profile, not a size need), and from here on every check reads the shipped file, the
   split's output, not the build output. It searches the binary,
   and the decompressed zstd frames of its Qt resources, for the home directory, the user
   name, `/media/`, the host name, git's `user.name` and `user.email` and the project's former
   name, stages the installed tree (the desktop file claims no `sgnl://` or
   `signalcaptcha://` link), packs, and then unpacks what it packed and checks it. The
   result, in `target/dist/`:

   ```
   kourier-<version>-linux-<arch>.tar.gz        the installed tree (binary without its symbol
                                                table), README.txt, licences
   kourier-<version>-linux-<arch>-names.tar.gz  bin/.debug/kourier-<build-id>.debug
   kourier-<version>-source.tar.gz              git archive of the commit
   SHA256SUMS                                   over the binary and the source archive
   SHA256SUMS.names                             over the names archive
   ```

   Only a `--release` run that found nothing writes these bare names. Every other run
   writes `…-unreleased.tar.gz` (a tree with changes: `…-dirty.tar.gz`), and the top
   directory inside each archive ends the same way; the grade goes last in the names
   archive's name (`…-names-unreleased.tar.gz`).

   The command prints the requirements it read from the binary (shared libraries, glibc,
   libstdc++, the Qt minor version and why, QML modules, what is needed at run time) and the
   sizes. Read them once: they go into `README.txt` in the archive. Look at the tree it
   unpacked as a user would:

   ```bash
   mkdir -p /tmp/kourier-try && tar -xzf target/dist/kourier-*-linux-x86_64.tar.gz -C /tmp/kourier-try
   /tmp/kourier-try/kourier-*/bin/kourier --version
   (cd target/dist && sha256sum -c SHA256SUMS && sha256sum -c SHA256SUMS.names)
   ```

   The pattern `kourier-*-linux-x86_64.tar.gz` names the binary archive exactly (a looser
   `kourier-*-linux-*.tar.gz` also matches the names archive, and tar then takes it for a
   member name and exits 2); for an unreleased run use `…-linux-x86_64-unreleased.tar.gz`.

   Put the repository address into the About data first (`homepage` in
   `crates/kourier-app/src/about.rs`) or the run stops at the finding about it.

   **Keep the names file.** `target/dist/` is emptied by the next `dist` run (an agent's check
   is enough) and `target/` can be lost, and the build cannot be repeated, so copy the names
   archive and `SHA256SUMS.names` to a backed-up directory outside `target/` and outside every
   repository, one directory per version, whatever is published. When the build ran on another
   machine, copy `target/dist` back to this one first.

   ```bash
   mkdir -p <releases>/0.1.0
   cp target/dist/kourier-0.1.0-linux-x86_64-names.tar.gz target/dist/SHA256SUMS.names \
      <releases>/0.1.0/
   (cd <releases>/0.1.0 && sha256sum -c SHA256SUMS.names)
   ```

   Whether the names archive is also published beside the binary (like Flatpak's `.Debug`) or
   kept only there is the maintainer's call at the first release (step 5).

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
   # only if the names archive is published too (the maintainer's call, see below):
   #   add target/dist/kourier-0.1.0-linux-x86_64-names.tar.gz target/dist/SHA256SUMS.names
   ```

   Publishing the names archive or not changes nothing in the archives: the README inside the
   binary archive says the names file "is attached to the release when the maintainer publishes
   it; otherwise ask for it" and holds for both answers. Keep the file in the directory of step 3
   either way.

   `--draft` keeps the release invisible. Open it on GitHub, read the notes and the file list,
   and press "Publish release" by hand. GitHub adds its own "Source code" archives to every
   release; the one attached here is `git archive` of the tagged commit with a fixed set of
   modes and a top directory named for the version, which is why it is attached as well. It
   is not signed (see the top of this file): the tag is.

6. **After publishing**, from a clean directory: download the files that were
   attached (three, or five with the names archive and `SHA256SUMS.names`), run
   `sha256sum -c SHA256SUMS` (and `sha256sum -c SHA256SUMS.names` beside the names archive), unpack the archive and run `bin/kourier --version`.

## What `cargo xtask dist` checks, and what it does not

**`--release` guarantees**, when it ends without an error:

- the names are the bare ones, and they are the commit's: the tree had no changes, the commit
  and `git status` were the same at the end of the build as at its start, the binary's stamp
  names that commit, a clean tree, the feature set `--no-default-features --features signal,whatsapp`
  and the checksum of the build output (the packed binary is that output without its symbol
  table: the same build-id, and the CRC of the names file in its debuglink), and the source archive is
  `git archive <that commit hash>` (not `HEAD`) with modes 0644 and 0755 (it carries the commit
  in its header, which the tool compares);
- no finding of `NOT-READY.txt` is left: no maintainer blank, no placeholder application id, no
  tracked file that names a private path, a repository address in the About data;
- the binary is no measuring build (never `perf` or `dumpable`) and has no test network:
  `bin/kourier --version` prints `Kourier <version>`, `--fixture` is refused, and no code of
  the test network crate is in it;
- the binary does not offer WhatsApp or Threema: `bin/kourier --print-networks`, run on a temporary
  profile whose `kourier.toml` has `networks-on = ["whatsapp", "threema"]`, names neither of them among
  the networks it would register nor among those it would list as off, and names both in `on=`
  (a release build offers them in neither list until the user lifts that rule; the only proof of the
  release arm on the packed binary, since the tests are debug builds; Threema is not in the archive's
  features at all, so it is safe by construction and the check guards the day it joins);
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
  none; the release binary's packed relative relocations, `crates/kourier/build.rs`, need
  glibc 2.36 or newer, and its `GLIBC_ABI_DT_RELR` version need counts as 2.36), what is needed at run time that the binary cannot show, and the repository address;
- the names file is the one that belongs to the packed binary: the build output, the unpacked
  `bin/kourier` and the unpacked names file carry the same GNU build-id, the debuglink stores
  `kourier-<build-id>.debug` and the names file's CRC, the packed binary has no `.symtab`, and
  its dynamic symbols are the build output's; the names archive holds exactly that one file and
  is searched for private names like every packed file;
- the checksums match, in `SHA256SUMS` and in `SHA256SUMS.names`.

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
