//! Runs the coexistence binary (architecture §5.3, S1 "Done when") and checks its dynamic symbol
//! table: BoringSSL's OpenSSL-named symbols must not be exported, or Qt's libssl.so.3 could bind
//! to them.

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_kourier-coexist");

#[test]
fn boringssl_sqlcipher_and_qt_tls_in_one_process() {
    let out = Command::new(BIN)
        .env("QT_QPA_PLATFORM", "offscreen")
        .env("QT_FORCE_STDERR_LOGGING", "1")
        .output()
        .expect("run kourier-coexist");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    println!("{stdout}");
    eprintln!("{stderr}");
    assert!(
        out.status.success(),
        "kourier-coexist failed: {}",
        out.status
    );
    for line in [
        "libsignal-net TLS (BoringSSL): handshake + GET ok",
        "SQLCipher: query ok",
        "QtNetwork: supportsSsl = true",
        "libssl loaded by Qt: /",
        "libsignal-net TLS and SQLCipher still ok with libssl loaded",
        "PSS: ",
        "coexist: OK",
    ] {
        assert!(stdout.contains(line), "missing {line:?}");
    }
}

#[test]
fn dynamic_symbol_table_has_no_openssl_names() {
    let out = Command::new("nm")
        .args(["-D", "--defined-only", BIN])
        .output()
        .expect("run nm");
    assert!(out.status.success(), "nm failed");
    let listing = String::from_utf8_lossy(&out.stdout);
    let bad: Vec<&str> = listing
        .lines()
        .filter_map(|l| l.split_whitespace().last())
        .filter(|s| ["SSL_", "EVP_", "CRYPTO_"].iter().any(|p| s.starts_with(p)))
        .collect();
    assert!(bad.is_empty(), "exported OpenSSL-named symbols: {bad:?}");
    // The binary really links BoringSSL (statically): its symbols are there, just not exported.
    let all = Command::new("nm")
        .args(["--defined-only", BIN])
        .output()
        .expect("run nm");
    let all = String::from_utf8_lossy(&all.stdout);
    assert!(
        all.lines().any(|l| l.ends_with(" SSL_CTX_new")),
        "BoringSSL is not linked; the check would prove nothing"
    );
}
