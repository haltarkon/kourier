//! Link flags of the release binary.
//!
//! RELR (packed relative relocations): the about 87,000 relative relocations of the PIE become a
//! bitmap, 2 MB less file and fewer relocations to apply at start (W5 chunk C0, measured in
//! `target/wa/briefs/w5/binary-room.md` B). The binary then needs glibc 2.36 or newer at run time;
//! `xtask/src/dist.rs` counts its `GLIBC_ABI_DT_RELR` version need as glibc 2.36 and `RELEASING.md`
//! says it. `-z pack-relative-relocs`, NOT `--pack-dyn-relocs=relr`: with the latter LLD 18 leaves
//! out the `GLIBC_ABI_DT_RELR` need and the loader refuses the binary. Release profile only: debug
//! and test builds link as before, so this file rebuilds nothing of theirs.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("PROFILE").as_deref() == Ok("release") {
        println!("cargo:rustc-link-arg-bins=-Wl,-z,pack-relative-relocs");
    }
}
