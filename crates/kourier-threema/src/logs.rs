//! What libthreema's log lines may say (TH4 chunk L, decision C11; architecture §9.1). Pure: nothing here logs.
//!
//! libthreema logs through `tracing`. Its `#[tracing::instrument(fields(?self, ..))]` spans sit at INFO and
//! print the protocol object they belong to with every event inside them, its `trace!(?result, ..)` events
//! print whole results, and the data of its 101 `warn!` and `error!` call sites is in FIELDS, never in the
//! message. The app's default filter is `info`, so without a rule those spans and events reach the log. The
//! rule is the one of WhatsApp's upstream crates, minus the redaction of a message:
//!
//! - [`is_upstream`] says whether a record comes from libthreema, a crate of its family or a transport crate
//!   of `src/dial/`, by its MODULE PATH (the target is a free string and the formatted line does not carry the
//!   module).
//! - [`keeps`] is the level rule: nothing of those crates is written below WARN, whatever `KOURIER_LOG` says,
//!   so the user's filter can narrow what they say but never widen it. Spans are judged the same way by the
//!   callsite's module path, so libthreema's INFO spans are off.
//!
//! The TRANSPORT crates ([`TRANSPORT_MODULES`], the list in `src/dial/modules.rs`: the dial directory is the
//! one place the source check lets name those crates, so the list lives there and this file writes no name)
//! are upstream too, so the rule does not depend on another network being built in: WhatsApp's list names
//! only some of them (the TLS ones), and only in a build with `whatsapp`. The crates of the dial print what
//! their requests and sockets do (the HTTP client the host it connects to at DEBUG and the bytes it reads
//! and writes at TRACE, the WebSocket client the handshake's request at TRACE, the HTTP and HTTP/2
//! libraries their frames): nothing the log may hold, and below WARN nothing the user needs. Signal's
//! `libsignal-net` links the same crates, so a build with Signal loses their INFO and DEBUG lines as well
//! (WhatsApp's arm already does that for the TLS library).
//!
//! `kourier-app` (`logging/upstream.rs`) puts both into the two `fmt` layers: a record of those crates that
//! passes is written as time, level, target and its MESSAGE alone (no field, no span field) and does not go
//! through WhatsApp's `redact_upstream` (unless the WhatsApp arm owns the module as well: the TLS library in
//! a default build); the writer's own `redact` still runs on the finished line.

use tracing::Level;

/// The crates whose records are libthreema's, by the first segment of the module path (a `-` in a crate
/// name is `_` there): the library itself. The crates of its family are [`UPSTREAM_FAMILY`], not names here.
/// The vendored workspace's other crates are checked against both by a test, so a crate added by an update of
/// the vendored copy cannot slip by. The transport crates are [`TRANSPORT_MODULES`].
pub const UPSTREAM_MODULES: &[&str] = &["libthreema"];

/// The transport crates of `src/dial/` (TLS, WebSocket, HTTP/2 and HTTPS: see `src/dial/modules.rs` for the
/// list, why each is in it and which are left out), by the first segment of the module path. The list is the
/// dial directory's because the source check lets only that directory name the crates.
pub const TRANSPORT_MODULES: &[&str] = crate::dial::TRANSPORT_MODULES;

/// The crates of the vendored workspace that are named `libthreema_<what they are>` (the macros, the
/// dependencies' re-export): `libthreema-macros` is `libthreema_macros` in a module path.
pub const UPSTREAM_FAMILY: &str = "libthreema_";

/// Whether a record whose module path is `module_path` comes from libthreema or from a transport crate of the
/// dial ([`TRANSPORT_MODULES`]). Only the first segment counts, and it must be a whole crate name
/// (`libthreemax::y` and a transport crate's name with a letter added are not). A record without a module path is not decided here:
/// the caller's rule for those is the safe side.
pub fn is_upstream(module_path: &str) -> bool {
    let root = module_path.split("::").next().unwrap_or("");
    UPSTREAM_MODULES.contains(&root)
        || TRANSPORT_MODULES.contains(&root)
        || root.starts_with(UPSTREAM_FAMILY)
}

/// Whether a record may be written at all: upstream's (libthreema's and the dial's transport crates') only from
/// WARN up (tracing orders levels by verbosity, so ERROR and WARN are the two that compare at most WARN),
/// everyone else's as the filters of the app say.
pub fn keeps(module_path: &str, level: Level) -> bool {
    level <= Level::WARN || !is_upstream(module_path)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// The crate of libthreema and its family are upstream's, whole crate names only.
    #[test]
    fn libthreema_and_its_family_are_upstream_by_whole_crate_name() {
        for module in [
            "libthreema",
            "libthreema::d2m",
            "libthreema::d2d_rendezvous",
            "libthreema::d2d_rendezvous::tests::x",
            "libthreema_macros",
            "libthreema_macros::y",
            "libthreema_dependencies::z",
            "libthreema_blake2::w",
        ] {
            assert!(is_upstream(module), "{module}");
        }
        for module in [
            "libthreemax::y",
            "libthreemax",
            "libthreem::a",
            "libthreema2::a",
            "Libthreema::a",
            "kourier_threema::conn",
            "kourier_threema::libthreema",
            "libsignal_net::chat",
            "whatsapp_rust::client",
            "blake2::Blake2b",
            "",
            "::libthreema",
        ] {
            assert!(!is_upstream(module), "{module}");
        }
    }

    /// Below WARN nothing of libthreema is kept, at any level of any of its crates; everything else is kept
    /// at every level (the filters of the app decide there), and WARN and ERROR pass for everyone.
    #[test]
    fn libthreema_is_kept_from_warn_up_and_everyone_else_at_every_level() {
        for level in [Level::TRACE, Level::DEBUG, Level::INFO] {
            for module in [
                "libthreema",
                "libthreema::d2m",
                "libthreema::d2d_rendezvous",
                "libthreema_macros::y",
            ] {
                assert!(!keeps(module, level), "{module} at {level}");
            }
            for module in ["libthreemax::y", "kourier_threema::conn", "libsignal_net"] {
                assert!(keeps(module, level), "{module} at {level}");
            }
            // The transport crates of the dial: cut like libthreema, a longer crate name is not theirs.
            for name in TRANSPORT_MODULES {
                assert!(!keeps(name, level), "{name} at {level}");
                assert!(
                    !keeps(&format!("{name}::a::b"), level),
                    "{name}::a::b at {level}"
                );
                assert!(keeps(&format!("{name}x::a"), level), "{name}x at {level}");
            }
        }
        for level in [Level::WARN, Level::ERROR] {
            for module in ["libthreema::d2m", "libthreema_macros::y", "libthreemax::y"] {
                assert!(keeps(module, level), "{module} at {level}");
            }
            for name in TRANSPORT_MODULES {
                assert!(keeps(name, level), "{name} at {level}");
                assert!(keeps(&format!("{name}::a::b"), level), "{name}::a::b");
            }
        }
    }

    /// The transport crates of `src/dial/` are upstream by whole crate name, in a build without WhatsApp too:
    /// the rule is Threema's own (WhatsApp's list names only some of them, and only when that backend is
    /// built). The source check lets only the dial directory write the crates' names, so the rows are made
    /// from the list: every name bare and with a path, and never with a letter, a digit or a `_` more.
    #[test]
    fn the_transport_crates_of_the_dial_are_upstream_by_whole_crate_name() {
        assert!(
            TRANSPORT_MODULES.len() >= 10,
            "{} names",
            TRANSPORT_MODULES.len()
        );
        for name in TRANSPORT_MODULES {
            assert!(is_upstream(name), "{name}");
            assert!(is_upstream(&format!("{name}::a")), "{name}::a");
            assert!(is_upstream(&format!("{name}::a::b::c")), "{name}::a::b::c");
            for longer in [
                format!("{name}x::a"),
                format!("{name}2"),
                format!("{name}_x::a"),
                format!("x{name}::a"),
                format!("{}::a", name.to_uppercase()),
                format!("kourier_threema::{name}"),
                format!("kourier_threema::x::{name}"),
                format!("::{name}"),
            ] {
                assert!(!is_upstream(&longer), "{longer}");
            }
        }
        // Distinct names, and none of them is libthreema's own entry or shadows a Kourier crate.
        let mut sorted = TRANSPORT_MODULES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), TRANSPORT_MODULES.len(), "a name twice");
        for name in TRANSPORT_MODULES {
            assert!(!name.starts_with("kourier"), "{name}");
            assert!(!UPSTREAM_MODULES.contains(name), "{name}");
        }
    }

    /// The package name of a manifest and the name its library has in a module path (`[lib] name`, else the
    /// package name with `_` for `-`).
    fn crate_name(manifest: &Path) -> (String, String) {
        let text = std::fs::read_to_string(manifest).expect("a manifest of the vendored copy");
        let table: toml::Table = text.parse().expect("a manifest is TOML");
        let package = table
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(toml::Value::as_str)
            .expect("a package name")
            .to_owned();
        let library = table
            .get("lib")
            .and_then(|l| l.get("name"))
            .and_then(toml::Value::as_str)
            .map_or_else(|| package.replace('-', "_"), str::to_owned);
        (package, library)
    }

    /// Every crate of the vendored workspace (its members and the excluded path dependency) is either named
    /// by the rule or listed here with the reason it is not: a crate that an update of the vendored copy adds
    /// stops this test until someone decides, because a record of a crate the rule does not name is written
    /// as the filters say (INFO by default).
    #[test]
    fn every_vendored_crate_is_named_by_the_rule_or_listed_with_its_reason() {
        /// The library names the rule leaves out, and why.
        const NOT_UPSTREAM: &[(&str, &str)] = &[
            (
                "blake2",
                "patch P3's `libthreema-blake2`: its library keeps the registry crate's name `blake2`, which \
                 argon2 uses too, and it has no log call at all",
            ),
            (
                "uniffi_bindgen",
                "a build tool of the dropped bindings: never linked into the app",
            ),
        ];

        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../third_party/libthreema");
        let workspace: toml::Table = std::fs::read_to_string(root.join("Cargo.toml"))
            .expect("the vendored workspace manifest")
            .parse()
            .expect("TOML");
        let listed = |key: &str| -> Vec<String> {
            workspace
                .get("workspace")
                .and_then(|w| w.get(key))
                .and_then(toml::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|dir| dir.as_str().map(str::to_owned))
                .collect()
        };
        let dirs: Vec<String> = [listed("members"), listed("exclude")].concat();
        assert!(
            dirs.len() >= 4,
            "the vendored workspace names its crates: {dirs:?}"
        );
        let mut named = 0;
        for dir in &dirs {
            let (package, library) = crate_name(&root.join(dir).join("Cargo.toml"));
            if library.starts_with("libthreema") {
                assert!(
                    is_upstream(&library),
                    "{package} (library `{library}`) is libthreema's family and the rule does not name it"
                );
                named += 1;
            } else {
                assert!(
                    NOT_UPSTREAM.iter().any(|(name, _)| *name == library),
                    "{package} (library `{library}`) is a new crate of the vendored copy: name it in \
                     UPSTREAM_MODULES or list it in NOT_UPSTREAM with the reason it logs nothing"
                );
            }
        }
        // Not vacuous: the library, the macros and the dependencies' crate are there. The renamed blake2
        // (package `libthreema-blake2`) is told apart by its LIBRARY name, which is what a module path has.
        assert!(named >= 3, "only {named} crates of the family were found");
        for (name, why) in NOT_UPSTREAM {
            assert!(!is_upstream(name), "{name} ({why}) is named by the rule");
        }
    }
}
