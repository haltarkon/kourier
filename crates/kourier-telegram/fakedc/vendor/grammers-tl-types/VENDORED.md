# grammers-tl-types, vendored for the fake DC (TEST ONLY)

This directory is `grammers-tl-types` 0.11.0 with ONE file replaced: `tl/api.tl` is grammers' layer-232 schema. The
fake Telegram data centre (`crates/kourier-telegram/fakedc/`, a workspace of its own) uses it to write the TL objects
TDLib 1.8.69 (MTProto layer 232) parses. It is never part of Kourier's workspace, never in the main `Cargo.lock` and
never in a shipped binary (`cargo xtask tdlib fakedc build` builds it for the test-only fake DC and for nothing else).

## Why

crates.io's newest `grammers-tl-types` is 0.11.0 (2026-10-15), layer 230. Layer 232 renamed nine constructor ids,
among them `channel` (`d49f34c6` -> `7ad15240`) and `channelFull` (`a04e8d3a` -> `c4c4dd40`), which the fake DC's
supergroups are made of: TDLib 1.8.69 cannot parse an answer that holds a 230 `channel`. grammers' master has the
layer-232 file; its other changes after 0.11.0 do not touch this crate. A git dependency on grammers would need a
new source host in `deny.toml` (LC-18 keeps it as it is), so the crate is vendored. The next crates.io release at
layer 232 or above replaces this directory with a plain `grammers-tl-types = "=<version>"` again.

## What it is

- Package: `grammers-tl-types` 0.11.0 from crates.io (checksum
  `069e64c424bde5764391bc72f07085a12177d7053c8a0da355425527fd7c221f`, the one in `fakedc/Cargo.lock` before the
  vendoring), taken unpacked from the local registry copy and checked against the `.crate` file of the registry
  cache: `diff -r` of the unpacked package against this directory shows, besides this note and `rustfmt.toml` (Kourier's
  two files), ONE difference, `tl/api.tl` (the package has no `.cargo-ok`, cargo's own marker of an unpacked registry copy).
  `Cargo.toml`, `Cargo.toml.orig`, `Cargo.lock` (cargo ignores a member's own lock), `build.rs`, `src/`,
  `tl/mtproto.tl`, `README.md` and `.cargo_vcs_info.json` are byte for byte the package's.
- `tl/api.tl`: `grammers-tl-types/tl/api.tl` of codeberg.org/Lonami/grammers at commit
  `dfffb7144f8a57aeda4dcd041ebb5a71a0ed63ef` ("Update to layer 232", 2026-10-21; its parent is
  `15b7afa46a9c6853aa9d8efc9d70a0f46b1781c3`, the commit 0.11.0 was packaged from, and that commit's `api.tl` is byte
  for byte the package's), 294,194 bytes, sha256
  `1a4d6691018348d8735ccadc25c16ba9fb5ac7eae4bb5d4d422d0909edee2332`, first line `// LAYER 232`. Fetched twice
  (the LC-20 planner, then the build chunk C3 from
  `https://codeberg.org/Lonami/grammers/raw/commit/dfffb7144f8a57aeda4dcd041ebb5a71a0ed63ef/grammers-tl-types/tl/api.tl`):
  the two copies are equal. Where grammers took the file from: it names none (not verified).
- Cross-check: every constructor id of this `api.tl` equals the id TDLib's own `telegram_api.tl` at the pinned commit
  gives the same name (2,578 names in both, 0 differ; `scripts/tlids.py` of the LC-20 build).
  `xtask` refuses a build, an `env` and a `record` when this file's `// LAYER` line differs from the layer of
  `third_party/tdlib/tdlib.pin`.
- Alternative not taken: building the TL types from TDLib's own `telegram_api.tl` (the pin's file, Boost licence)
  would tie the schema to the pin by construction, but that file has a preamble, lower-case `vector<...>` and
  `invokeWith*Prefix = Error` lines grammers' parser may refuse (not tried).

## What cargo makes of it

The fake DC's `Cargo.toml` names it as a path dependency (`version = "=0.11.0"` stays as a guard); a path dependency
inside the workspace directory is a member of the fake DC's workspace. Three consequences, none of them a
change to a file here:

- `fakedc/Cargo.lock` lists the package without a source and locks the packages of its optional and dev dependencies
  as well (`serde`, `serde_bytes`, `serde_derive`, `toml` and `toml`'s tree: `equivalent`, `hashbrown`, `indexmap`,
  `serde_spanned`, `toml_datetime`, `toml_parser`, `toml_writer`, `winnow`, all MIT or Apache-2.0). Nothing of it is built:
  the fake DC's build and tests compile only the root package and what it needs, and the feature `impl-serde` is off. `cargo
  deny check licenses bans sources` sees them, and allows them.
- cargo prints `warning: resolver for the non root package will be ignored` for this manifest (its `resolver = "2"`);
  it is a cargo notice, not a rustc warning, and the manifest stays byte for byte the package's.
- `cargo fmt --check --all` of the fake DC's workspace reaches this directory; `rustfmt.toml` here switches it off.

## Licence

grammers is dual licensed `MIT OR Apache-2.0` (`license` in `Cargo.toml`); the crates.io package carries no licence
file, the texts are in `LICENSES/` of this repository (`MIT.txt`, `Apache-2.0.txt`). `REUSE.toml` gives every file here,
but this note and `rustfmt.toml`, to "developers of the grammers project" (override rows, last in the file); this note
and `rustfmt.toml` are Kourier's own (AGPL-3.0-only).

## Formatting

`rustfmt.toml` switches formatting off: `cargo fmt --check --all` of the fake DC's workspace would otherwise check
upstream's sources against Kourier's style. Do not edit a file here by hand; to change it, replace the directory.

## To update

1. Take the new package (or the new `api.tl` of grammers) and replace the files, keeping `rustfmt.toml` and this note.
2. Write the new checksum, commit and sha256 above; check `diff -r` against the package.
3. Regenerate `fakedc/Cargo.lock` (`cargo update --manifest-path crates/kourier-telegram/fakedc/Cargo.toml -p
   grammers-tl-types`, through `srv`, with `-f` for the lock) and run `cargo xtask tdlib fakedc build`.
