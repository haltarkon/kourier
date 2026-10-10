//! Compiles the Signal protobufs copied from Signal Desktop (`protos/`, header records the
//! commit), `protos/backup.proto` (libsignal v0.104.0 `rust/message-backup/src/proto/`,
//! copied unchanged, package `signal.backup`; the history-transfer archive, `src/archive.rs`)
//! and `protos/RingRTC.proto` (the mock server's trimmed copy of RingRTC's signaling proto,
//! package `signaling`; the ring of a group call, `src/receive/calls.rs`) with prost-build. The generated code is included by the private `proto` module and
//! never appears in a public item (docs/architecture.md §2.3).

use std::io;

/// The proto files we need: all in package `signalservice` except `backup.proto` (package
/// `signal.backup`) and `RingRTC.proto` (package `signaling`), each generated into its own
/// file.
const PROTOS: &[&str] = &[
    "protos/SignalService.proto",
    "protos/SignalStorage.proto",
    "protos/Groups.proto",
    "protos/DeviceMessages.proto",
    "protos/DeviceName.proto",
    "protos/backup.proto",
    "protos/RingRTC.proto",
];

fn main() -> io::Result<()> {
    println!("cargo:rerun-if-changed=protos");
    let mut config = prost_build::Config::new();
    // Groups.proto and SignalStorage.proto use proto3 `optional`; protoc 3.21 needs the flag
    // (libsignal passes it for the same reason).
    config.protoc_arg("--experimental_allow_proto3_optional");
    config.compile_protos(PROTOS, &["protos"])
}
