//! No usage statistics, ever (his decision D4, W2 design 3.7): WhatsApp's own client sends `w:stats`
//! telemetry through a separate plugin, and Kourier never links it. Three layers: the lock file below,
//! the source check of `tests/builder.rs` (no file of `src/` names the plugin or its runtime, and
//! `w:stats` only a later decoder module), and, at run time, a tap that counts what leaves the client
//! (a later chunk).

use std::path::Path;

/// The packages of a `Cargo.lock` whose name contains `wam` (any case): the statistics plugin's crates
/// have it in their names, and no other package of the graph does.
fn wam_packages(lock: &str) -> Vec<String> {
    lock.lines()
        .filter_map(|line| {
            let name = line.strip_prefix("name = \"")?.strip_suffix('"')?;
            name.to_ascii_lowercase()
                .contains("wam")
                .then(|| name.to_owned())
        })
        .collect()
}

#[test]
fn the_lock_has_no_wam_package() {
    let lock = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    let text = std::fs::read_to_string(&lock).expect("the workspace's Cargo.lock");
    let packages = text.lines().filter(|l| *l == "[[package]]").count();
    assert!(
        packages > 300 && text.contains("name = \"kourier-whatsapp\""),
        "this does not look like the workspace lock ({packages} packages)"
    );
    assert_eq!(wam_packages(&text), Vec::<String>::new());
}

/// The check itself: a plugin crate in the lock is found, a look-alike is not.
#[test]
fn a_lock_with_a_wam_package_is_refused() {
    let lock = "[[package]]\nname = \"wasm-bindgen\"\nversion = \"0.2.0\"\n\n\
                [[package]]\nname = \"whatsapp-rust-plugin-wam\"\nversion = \"0.1.0\"\n\n\
                [[package]]\nname = \"Wacore-WAM-runtime\"\n\n\
                [[package]]\nname = \"kourier-whatsapp\"\n";
    assert_eq!(
        wam_packages(lock),
        ["whatsapp-rust-plugin-wam", "Wacore-WAM-runtime"]
    );
    assert!(wam_packages("name = \"swamp\"\n").len() == 1);
    assert!(wam_packages("[[package]]\nname = \"wasm-bindgen\"\n").is_empty());
}
