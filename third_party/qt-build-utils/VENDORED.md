# qt-build-utils, vendored

KDAB's build-script helper of cxx-qt (`cxx-qt-build` runs moc, rcc, qmltyperegistrar and
qmlcachegen through it), copied into Kourier so that one patch can change how qmlcachegen is run.
The root `Cargo.toml` puts it in place of the crates.io package with `[patch.crates-io]`; it is
no workspace member (the `exclude` list), so its own tests and lints stay upstream's.

## Source

- crates.io package `qt-build-utils` 0.10.0 (the copy cargo unpacked into
  `~/.cargo/registry/src/index.crates.io-*/qt-build-utils-0.10.0`), which is
  `https://github.com/KDAB/cxx-qt` commit `61562797ba25558a7c7dba8017deca038f3ee9e4` (tag
  `v0.10.0`), directory `crates/qt-build-utils` (its `.cargo_vcs_info.json`). `src/` is byte for
  byte the tag's, compared with the reference clone `~/projects/messengers/refs/cxx-qt` at that
  tag.
- Licence: `MIT OR Apache-2.0` (both texts in `LICENSES/`), copyright KDAB (every `.rs` file's
  SPDX header). `REUSE.toml` annotates the manifest, which has no header.

## How it was taken

```bash
cp -a ~/.cargo/registry/src/index.crates.io-*/qt-build-utils-0.10.0/{src,Cargo.toml} third_party/qt-build-utils/
```

`Cargo.toml` is the normalised manifest of the package (the original inherits from upstream's
workspace). `Cargo.toml.orig`, `Cargo.lock` and `.cargo_vcs_info.json` of the package are left
out. Added by Kourier: this file and `rustfmt.toml` (`disable_all_formatting = true`, so that
Kourier's `cargo fmt --all` does not restyle upstream's files).

## Patches

| # | File | What and why |
|---|---|---|
| Q1 | `src/tool/qmlcachegen.rs` | `--only-bytecode` on every per-file qmlcachegen run: the QML files are compiled to bytecode and no ahead-of-time C++ is generated. The AOT C++ calls three private symbols of libQt6Qml (`QQmlPrivate::AOTCompiledContext::mark` twice, `QQmlPrivate::compositeMetaType`), which Debian and KDE neon version `Qt_6_PRIVATE_API` and Fedora `Qt_6.<minor>_PRIVATE_API`: a binary that needs them loads only on the Qt build it was linked against (and only for one Qt minor). Without them the binary needs only public `Qt_6` symbols. The bytecode is checked against the running Qt at load; when it does not match, Qt compiles the QML from the source files in the same resources, so a newer Qt is slower to start but still works. |

A change here reaches the tests and pictures of `kourier-qt` and `kourier-coexist` (the two
crates that build Qt code through cxx-qt-build) and their dependents: `xtask/src/quick.rs`
`READERS` and `can_change_a_picture`, and the screenshot digest `xtask/src/shots.rs`
`DIGEST_ROOTS`.

There is no switch for this in cxx-qt-build 0.10 or in upstream's `main` (checked 2026-11-11).
When cxx-qt gains one (or `QmlModule` options for qmlcachegen), drop this copy for it.
