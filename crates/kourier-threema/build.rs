// SPDX-FileCopyrightText: 2026 The Kourier developers
// SPDX-License-Identifier: AGPL-3.0-only

//! Compiles the ONE protocol file libthreema leaves out of its own build: `url-payloads.proto`
//! (the join URL's payload, TH4). The messages it imports (`common`, `d2d_rendezvous`) are
//! libthreema's already, so they are mapped onto its generated types instead of being generated a
//! second time. Nothing here touches the network; the file is read from the vendored copy.

use std::{env, io, path::PathBuf};

fn main() -> io::Result<()> {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets the manifest dir"));
    let proto_dir = manifest_dir.join("../../third_party/libthreema/threema-protocols/src");
    let file = proto_dir.join("url-payloads.proto");

    println!("cargo:rerun-if-changed={}", file.display());

    prost_build::Config::new()
        .extern_path(".common", "::libthreema::protobuf::common")
        .extern_path(".d2d_rendezvous", "::libthreema::protobuf::d2d_rendezvous")
        .compile_protos(&[file], &[proto_dir])
}
