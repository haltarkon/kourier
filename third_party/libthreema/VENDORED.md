# libthreema, vendored

Threema's Rust protocol library, copied into Kourier so that the Threema backend
(`crates/kourier-threema`) builds against one pinned, reviewed copy. Kourier reads this tree; it
changes it only by the patches listed below, one commit each.

## Source

- Repository: `https://github.com/threema-ch/threema-ios`, directory `libthreema/`.
- Commit: `bf7c14ed50a7add80df2e02e90264e10af6a797d` (2026-10-20, "Version 7.5b75038").
- Threema's internal libthreema commit (the repository's `.libthreema-version`):
  `0be568014274debbbeb652d727151f9363863db8`.
- Protocol files: `threema-protocols/version.txt` = 23.
- Fallback pin, if this one ever has to be given up: `9a5e0f75c54909bd67c40ceddcaae4058fb2e0dd`.
- Licence: `AGPL-3.0-only` (the workspace manifest's `license`; no file carries an SPDX header),
  copyright Threema AG; `patches/blake2/` is RustCrypto's blake2 0.10.6 with Threema's change,
  `MIT OR Apache-2.0`. Kourier's `REUSE.toml` annotates both by path.

## How it was taken

```bash
git clone --filter=blob:none --sparse --depth 20 https://github.com/threema-ch/threema-ios repo
git -C repo sparse-checkout set libthreema
git -C repo checkout bf7c14ed50a7add80df2e02e90264e10af6a797d
cp -a repo/libthreema third_party/libthreema      # then the drops and the rename below
```

Nothing was fetched from a Threema server: GitHub only.

## Files dropped or renamed

| Path | Why |
|---|---|
| `lib/src/bindings/uniffi/`, `lib/src/bindings/wasm/` | the Swift/Kotlin and WebAssembly bindings; `lib/src/bindings/mod.rs` compiles them only with the `uniffi` / `wasm` features, which Kourier never turns on |
| `binding-tests/` | JavaScript tests of the wasm binding |
| `tools/build-uniffi.sh`, `tools/build-wasm.sh` | binding build scripts (`tools/uniffi-bindgen/` stays: it is a member of libthreema's own workspace) |
| `rustfmt.toml` (added, Kourier's) | `disable_all_formatting = true`: Kourier's `cargo fmt --all` (and ci's `--check`) formats the local path dependencies of its members too, and this tree is formatted with upstream's own style (no config in the copy); without it a Kourier fmt run would rewrite 144 upstream files |
| `rust-toolchain.toml` -> `rust-toolchain.toml.upstream` | upstream pins Rust 1.97; a file of that name would make rustup switch toolchains for every cargo run inside this directory. Kourier builds it with its own 1.99; the file is kept under the new name as the record of upstream's pin |

Everything else is byte for byte upstream's at the vendoring commit (A1); the patches below change the
files they name, and P3 also changes the vendored `Cargo.lock` (it is libthreema's own lock, used only
by runs of libthreema's own suite; Kourier's root `Cargo.lock` decides what Kourier builds). The comparison, from Kourier's root (`NEW` = the sparse checkout's `libthreema/`):

```bash
diff -r "$NEW" third_party/libthreema
# expected: "Only in" lines for the five dropped paths, the rename, VENDORED.md and rustfmt.toml; plus the
# files the patches below change
```

## Local patches

One Kourier commit each, inside this tree only; none is offered upstream unless the maintainer
says so (a pull request is publishing).

| # | Patch | Kourier commit | Upstream |
|---|---|---|---|
| P1 | `Reflected` header: skip `header_length - 1` bytes after the length byte (upstream skipped `header_length` and cut the envelope's first byte); tests in `lib/src/d2m/payload/post_handshake.rs` (16-byte header: the whole envelope; 20: 4 unknown bytes skipped; 15: refused) | `Threema: libthreema patch P1 Reflected header` | not offered |
| P2 | `version = "0.1.0"` on the internal path dependencies (`lib/Cargo.toml` libthreema-dependencies twice and libthreema-macros, `macros/Cargo.toml` libthreema-dependencies), so that cargo-deny's `wildcards = "deny"` accepts them and they need no `publish = false` (which would drop them from the notices); the suite still passes with upstream's lock and `--locked` | `Threema: libthreema patch P2 versions on the internal path dependencies` | not offered |
| P3 | blake2 as its own crate: `patches/blake2` is renamed to the package `libthreema-blake2` (its library keeps the name `blake2`) and `dependencies/Cargo.toml` takes it by `package =` with a path; the workspace's `[patch.crates-io] blake2` is removed and `patches/blake2` is `exclude`d from the workspace (a path dependency, not a member, so its dev-dependencies stay out of the lock), so Kourier's workspace needs no global patch and every other crate (argon2, libsignal) keeps crates.io's blake2. `Cargo.lock` changes with it: the path package is now `libthreema-blake2`, and crates.io's `blake2 0.10.6` is added for argon2. `derive_rph(stk)` is factored out of `lib/src/d2d_rendezvous/rxdtk.rs` and has a known-answer test against Python's `hashlib.blake2b` (the command is in the test's doc comment) | `Threema: libthreema patch P3 blake2 as its own crate + the path-hash vector` | not offered |
| P4 | `ClientInfo::ThirdParty { name, version, locale, os_name, os_architecture }` in `lib/src/common/mod.rs`: a client built on libthreema names itself instead of claiming a Threema app: user agent `<name>/<version> libthreema/0.1.0`, CSP client-info `<name>/<version>;L;<locale>;<os>;<arch>` (libthreema's own five-field `L` layout, the name inside the version field; `;` in a field becomes `_`), DeviceInfo platform `UNSPECIFIED`, details `<name> <version> (<os>)`; tests in `common/mod.rs` (`client_info_tests`) and a user-agent case in `https/mod.rs`. [U: which client-info layout the real chat server accepts: the protocol spec lists no `L` form at all, libthreema writes five fields; the first real login (RT1) shows it, watch the server's close code and log for a client-info complaint] | `Threema: libthreema patch P4 ClientInfo names a third-party client`, then `Threema: libthreema patch P4 keeps libthreema's client-info layout` | not offered (maybe later, asked first) |
| P5 | The crypto surface a client needs is `pub`, each function a thin wrapper over a line libthreema already runs (no new primitive, no new dependency, no feature): `crypto::{DecryptionFailed, EncryptionFailed}` (unit errors, nothing of the input); `DeviceGroupKey::open_reflected` (DGRK `reflect_key()` + the nonce-ahead decrypt: `(Nonce, envelope bytes as sent)`, the inverse of `ReflectPayload::encode_and_encrypt`; libthreema itself never opens a reflected envelope), `seal_reflected` (`reflect.rs`' two lines), `open_transaction_scope` / `seal_transaction_scope` (DGTSK, `begin.rs`' open, `manager.rs`' seal), `open_device_info` (DGDIK, the inverse of the `ClientHello` seal in `d2m/mod.rs`); `ClientKey::derive_csp_e2e_key` and `CspE2eKey` (field private) with `encrypt_message` / `decrypt_message` (MK) and `encrypt_metadata` / `decrypt_metadata` (MMK), `tag || ciphertext` with the message's nonce (the outgoing task's in-place seals, the incoming task's open); `Nonce::hash_with` (HMAC-SHA256 keyed by the identity: the nonce stores' form; `HashedNonce` already had public bytes). Files: `lib/src/common/{keys.rs,mod.rs}`, `lib/src/crypto/mod.rs`, and the cross-tests in `common::keys::tests` (`p5_*`: `seal_reflected` and the scope seal open with libthreema's own key lines, the device info sealed with the `ClientHello` line opens; every open refuses another group's key, another derived key, each flipped byte and 0/24/39 bytes), `common::nonce_hash_tests` (the HMAC vector, Python's `hmac`, the command in the doc), `csp_e2e::reflect::p5_tests` (libthreema's `encode_and_encrypt` -> `open_reflected`: same nonce, the decoded envelope equal; a wrong key and a flipped byte refused), `csp_e2e::transaction::manager::tests::p5_*` (the manager's sealed scope -> `open_transaction_scope`), `csp_e2e::message::task::incoming::tests::p5_*` (Threema's static message opens with the public boxes to the bytes of the incoming task's detached open; the public seals equal the outgoing task's byte for byte; a message sealed with them passes the incoming task's decrypt and decode; MK/MMK not interchangeable). Kourier's side: `crates/kourier-threema/tests/mock/crypto.rs` (the fake's own DGRK/DGTSK/DGDIK seals open with P5, P5's seals open with the fake, the `hash_with` vector from outside the crate) | `Threema: libthreema patch P5 the reflect, scope, device-info and E2E boxes and the nonce hash are public` | not offered |
| P6 | `crate-type = ["lib"]` in `lib/Cargo.toml` (upstream: `cdylib`, `lib`, `staticlib`, for its Swift, Kotlin and wasm bindings): as a dependency cargo built all three, a 19 MB `.so` and a 77 MB `.a` next to the rlib in every build; Kourier links only the rlib. Check: `ls target/debug/deps/liblibthreema*` shows no `.so` or `.a`; the suite still passes | `Threema: libthreema patch P6 the Rust library only` | not offered |
| P9 | `{deviceGroupIdPrefix8}`, `{blobIdPrefix}`, `{rendezvousPathPrefix8}` and `{backupIdPrefix8}` are the first byte as TWO lowercase hex digits (`{:02x}`; upstream wrote `{:x}`: ONE digit for a first byte below 0x10, so one device group in 16 got a mediator URL like `.../a/...`). The OPPF schema (`threema-protocols/src/oppf.schema.yml`: "the first two hex nibbles") and Threema Desktop (`apps/desktop/src/common/config.ts`, `slice(0, 2)`) agree; `{deviceGroupIdPrefix4}` was right already (one nibble). Six sites in `lib/src/common/config.rs`; tests in `common::config::prefix_tests` (6: the mediator URL for a first byte below 0x10 and from 0x10, the blob id, the blob mirror URL with both, the rendezvous path, the backup id). [U: real server] which form the real mediator routes: Desktop's, the one the official client sends, is assumed (RT1) | `Threema: libthreema patch P9 the mediator URL's prefix8 has two hex digits` | not offered |
| P10 | The servers' time is kept: `d2m::payload::ServerInfo` and `csp::payload::LoginAckData` gain `pub server_time_ms: u64`, the decoded `current_time` as the server sent it (ms since the Unix epoch), beside `clock_delta`, which stays. Upstream keeps only `clock_delta`, computed against the SYSTEM clock (`ClockDelta::calculate` calls `utc_now()`), so a client with an injectable clock could not apply the spec's 20-minute rule (`md-d2m.proto`, `ServerInfo.current_time`) to its own time, and no test could. Two files only (each struct is built in one place); tests `d2m::payload::handshake::tests::server_info_keeps_the_servers_time` and `csp::payload::handshake::tests::login_ack_data_keeps_the_servers_time` | `Threema: libthreema patch P10 ServerInfo and the login-ack keep the server's time` | not offered |
| P11 | `RendezvousServerBaseUrl::path` (`lib/src/common/config.rs`) is `pub` (upstream: `pub(crate)` with `#[expect(dead_code, reason = "Will use later")]`, and nothing in libthreema calls it: `TODO(LIB-11)` builds no `RendezvousInit`), with a doc comment, a `# Panics` section (an empty `rendezvous_path`) and `#[must_use]`, so libthreema's lints (`missing_docs`, `unreachable_pub`, pedantic) stay clean. A client that makes the join QR itself fills the rendezvous URL template through libthreema's own, P9-fixed prefixes (`{rendezvousPathPrefix4}` one nibble, `{rendezvousPathPrefix8}` two hex digits) instead of re-writing them. The body is upstream's, unchanged. Test: Kourier's `join::qr::tests` (`crates/kourier-threema`) calls it from outside the crate with a first byte below 0x10 and from 0x10; libthreema's own `config::prefix_tests` and `the_rendezvous_prefixes_are_one_and_two_digits` still cover the body | `Threema: libthreema patch P11 the rendezvous URL builder is public` | not offered |
| P12 | `DeviceGroupKey::path_public_key` (`lib/src/common/keys.rs`): the public half of the Device Group Path Key, a thin wrapper over two lines libthreema already runs. The blob mirror needs it as `deviceGroupId` and for the host prefixes (upstream keeps `path_key` `pub(crate)` and has no blob client). | this branch | not sent |

## Update recipe

1. Fetch the new threema-ios commit as above; read `.libthreema-version`.
2. `diff -r` the new `libthreema/` against this tree with the patches reverted; read every change
   in `lib/src/d2m/`, `lib/src/csp/`, `lib/src/d2d_rendezvous/`, `lib/src/common/` and the
   protocol files.
3. Replace the tree, apply the drops and the rename, re-apply the patches one commit each.
4. Run libthreema's own suite in this directory (`RUSTUP_TOOLCHAIN=1.99`, its own lock with
   `--locked`, then once with Kourier's lock copied in; test binaries in `unshare -rn`), and the
   path-hash vector of P3.
5. Update this file: both commits, the dropped files, the patch list.
6. The TLS pins are Kourier's, not libthreema's: compare Threema Desktop's SPKI pins of `*.threema.ch`
   (`apps/desktop/config/vite.config.ts`, `TLS_CERTIFICATE_PINS`) and the production config's hosts
   (`lib/src/common/config.rs`) with `crates/kourier-threema/src/dial/pins.rs`; a change is its own
   `Threema:` commit with the pins test updated (`dial::tests`, the production table).
7. The join of the device (`crates/kourier-threema`, TH4) is Kourier's too: `build.rs` compiles
   `threema-protocols/src/url-payloads.proto` (the one protocol file libthreema does not build) onto
   libthreema's generated `common` and `d2d_rendezvous` types, so a change of that file or of those
   messages must still build `src/proto.rs`; the emoji of the join are Threema Desktop's `emoji-list.ts`
   (`src/join/emoji.rs`, a digest test pins the list: compare it with Desktop's at each update).
