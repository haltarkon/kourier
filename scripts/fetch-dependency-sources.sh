#!/bin/sh
# Fetches the source of every dependency this version of Kourier is built from, at exactly
# the versions and git revisions that Cargo.lock names, into ./vendor (several hundred MB).
#
# Kourier is AGPL-3.0-only and links other free software statically (among it Signal's
# libsignal, also AGPL-3.0-only). The source archive of a release holds Kourier's own source;
# this script completes it with the corresponding source of the dependencies, from the
# places Cargo.lock records (crates.io and the named git repositories).
#
# Run it in the unpacked source archive or in a checkout:
#   sh scripts/fetch-dependency-sources.sh
# Then build without the network:
#   cargo build --release --locked --offline --config vendor-config.toml \
#       -p kourier --no-default-features --features signal
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$here"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cargo is not installed (https://rustup.rs); the toolchain is named in rust-toolchain.toml" >&2
    exit 1
fi
if [ ! -f Cargo.lock ]; then
    echo "Cargo.lock is not here: run this in Kourier's source tree" >&2
    exit 1
fi

# --locked: nothing but what Cargo.lock names; --versioned-dirs: a directory name says the
# version it holds.
cargo vendor --locked --versioned-dirs vendor > vendor-config.toml

echo
echo "The dependencies' sources are in $here/vendor"
echo "($(find vendor -mindepth 1 -maxdepth 1 -type d | wc -l) packages; their licences are in each package's directory)."
echo "vendor-config.toml tells cargo to build from them: see the head of this script."
