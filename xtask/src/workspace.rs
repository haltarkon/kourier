//! The workspace's packages and their dependency graph, read from `cargo metadata`: what the
//! front-end seam rules ([`crate::seam`]) and the quick CI tier ([`crate::quick`]) work on.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// The kind of a dependency edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Normal,
    Dev,
    Build,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Normal => "normal",
            Kind::Dev => "dev",
            Kind::Build => "build",
        }
    }
}

/// A workspace member.
#[derive(Debug, Clone)]
pub struct Package {
    pub name: String,
    /// Its directory relative to the workspace root, with `/` separators.
    pub dir: String,
    /// The names of its integration test targets (`tests/*.rs` and `[[test]]`).
    pub tests: Vec<String>,
}

/// The members and every resolved dependency edge between packages by name, members and
/// external crates alike. Edges of all kinds and of every enabled feature are in it; two
/// versions of one crate share a node.
#[derive(Debug, Default)]
pub struct Workspace {
    pub packages: Vec<Package>,
    pub edges: BTreeMap<String, Vec<(String, Kind)>>,
}

impl Workspace {
    /// `cargo metadata` of the workspace at `root` with all features on, so a dependency behind
    /// a feature is seen too.
    pub fn load(root: &Path) -> Result<Workspace> {
        let out = crate::cargo()
            .args(["metadata", "--format-version", "1", "--all-features"])
            .current_dir(root)
            .output()
            .context("cargo metadata")?;
        if !out.status.success() {
            bail!(
                "cargo metadata failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let meta: Value = serde_json::from_slice(&out.stdout).context("cargo metadata output")?;
        Workspace::from_metadata(&meta)
    }

    pub fn from_metadata(meta: &Value) -> Result<Workspace> {
        let root = Path::new(meta["workspace_root"].as_str().context("workspace_root")?);
        let members: Vec<&str> = meta["workspace_members"]
            .as_array()
            .context("workspace_members")?
            .iter()
            .filter_map(Value::as_str)
            .collect();
        let mut names: BTreeMap<&str, &str> = BTreeMap::new();
        let mut packages = Vec::new();
        for p in meta["packages"].as_array().context("packages")? {
            let (Some(id), Some(name)) = (p["id"].as_str(), p["name"].as_str()) else {
                continue;
            };
            names.insert(id, name);
            if !members.contains(&id) {
                continue;
            }
            let manifest = Path::new(p["manifest_path"].as_str().context("manifest_path")?);
            let dir = manifest
                .parent()
                .and_then(|d| d.strip_prefix(root).ok())
                .with_context(|| format!("{name}: its manifest is not below the workspace root"))?;
            let mut tests: Vec<String> = p["targets"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|t| {
                    t["kind"]
                        .as_array()
                        .is_some_and(|k| k.iter().any(|k| k == "test"))
                })
                .filter_map(|t| t["name"].as_str().map(str::to_owned))
                .collect();
            tests.sort();
            packages.push(Package {
                name: name.to_owned(),
                dir: dir.to_string_lossy().replace('\\', "/"),
                tests,
            });
        }
        packages.sort_by(|a, b| a.name.cmp(&b.name));
        let mut edges: BTreeMap<String, Vec<(String, Kind)>> = BTreeMap::new();
        for node in meta["resolve"]["nodes"].as_array().into_iter().flatten() {
            let Some(from) = node["id"].as_str().and_then(|id| names.get(id)) else {
                continue;
            };
            let list = edges.entry((*from).to_owned()).or_default();
            for dep in node["deps"].as_array().into_iter().flatten() {
                let Some(to) = dep["pkg"].as_str().and_then(|id| names.get(id)) else {
                    continue;
                };
                let kinds = dep["dep_kinds"].as_array().filter(|k| !k.is_empty());
                let kinds: Vec<Kind> = match kinds {
                    Some(kinds) => kinds
                        .iter()
                        .map(|k| match k["kind"].as_str() {
                            Some("dev") => Kind::Dev,
                            Some("build") => Kind::Build,
                            _ => Kind::Normal,
                        })
                        .collect(),
                    None => vec![Kind::Normal],
                };
                for kind in kinds {
                    let edge = ((*to).to_owned(), kind);
                    if !list.contains(&edge) {
                        list.push(edge);
                    }
                }
            }
        }
        for list in edges.values_mut() {
            list.sort();
        }
        Ok(Workspace { packages, edges })
    }

    pub fn package(&self, name: &str) -> Option<&Package> {
        self.packages.iter().find(|p| p.name == name)
    }

    pub fn is_member(&self, name: &str) -> bool {
        self.package(name).is_some()
    }

    /// The edges out of `name` (none for a crate the graph has no node for).
    pub fn deps_of(&self, name: &str) -> &[(String, Kind)] {
        self.edges.get(name).map_or(&[], Vec::as_slice)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use serde_json::json;

    /// A made-up workspace: `members` as (name, dir, integration tests), `edges` as (from, to,
    /// kind); any name in an edge that is not a member is an external crate.
    pub fn made_up(members: &[(&str, &str, &[&str])], edges: &[(&str, &str, Kind)]) -> Workspace {
        let mut ws = Workspace::default();
        for (name, dir, tests) in members {
            ws.packages.push(Package {
                name: (*name).to_owned(),
                dir: (*dir).to_owned(),
                tests: tests.iter().map(|t| (*t).to_owned()).collect(),
            });
        }
        for (from, to, kind) in edges {
            ws.edges
                .entry((*from).to_owned())
                .or_default()
                .push(((*to).to_owned(), *kind));
        }
        ws
    }

    #[test]
    fn metadata_is_read_into_members_tests_and_edges() {
        let meta = json!({
            "workspace_root": "/w",
            "workspace_members": ["path+file:///w/crates/a#0.1.0", "path+file:///w/xtask#0.1.0"],
            "packages": [
                {"id": "path+file:///w/crates/a#0.1.0", "name": "a",
                 "manifest_path": "/w/crates/a/Cargo.toml",
                 "targets": [
                    {"kind": ["lib"], "name": "a"},
                    {"kind": ["test"], "name": "two"},
                    {"kind": ["test"], "name": "one"}]},
                {"id": "path+file:///w/xtask#0.1.0", "name": "xtask",
                 "manifest_path": "/w/xtask/Cargo.toml",
                 "targets": [{"kind": ["bin"], "name": "xtask"}]},
                {"id": "registry+x#serde@1.0.0", "name": "serde",
                 "manifest_path": "/home/.cargo/registry/serde/Cargo.toml", "targets": []}
            ],
            "resolve": {"nodes": [
                {"id": "path+file:///w/crates/a#0.1.0", "deps": [
                    {"name": "serde", "pkg": "registry+x#serde@1.0.0",
                     "dep_kinds": [{"kind": null, "target": null}, {"kind": "dev", "target": null}]}]},
                {"id": "path+file:///w/xtask#0.1.0", "deps": []},
                {"id": "registry+x#serde@1.0.0", "deps": []}
            ]}
        });
        let ws = Workspace::from_metadata(&meta).unwrap();
        let names: Vec<&str> = ws.packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["a", "xtask"]);
        assert_eq!(ws.package("a").unwrap().dir, "crates/a");
        assert_eq!(ws.package("a").unwrap().tests, ["one", "two"]);
        assert_eq!(ws.package("xtask").unwrap().dir, "xtask");
        assert!(!ws.is_member("serde"));
        assert_eq!(
            ws.deps_of("a"),
            [
                ("serde".to_owned(), Kind::Normal),
                ("serde".to_owned(), Kind::Dev)
            ]
        );
        assert!(ws.deps_of("serde").is_empty());
    }

    /// The workspace crates that need an `opt-level` of their own in dev and test builds
    /// (T2, 2026-10-14): `[profile.dev.package."*"]` covers only the packages that are not
    /// workspace members, and these two instantiate the hot GENERIC code of dependencies (the
    /// JPEG encoder, the fixture's pictures and the blurhash input in `kourier-media`; the
    /// AES-GCM stream of every attachment file in `kourier-blob`), which a generic function
    /// compiles at the opt-level of the crate that names the concrete types.
    const DEV_OPTIMISED: [&str; 2] = ["kourier-media", "kourier-blob"];

    /// The `opt-level` a manifest gives `[profile.dev.package.<name>]` (`name` as written in the
    /// header: `"*"` keeps its quotes). Read line by line: `toml` is not an xtask dependency and
    /// adding one changes `Cargo.lock`, which makes the next `ci` render every picture again.
    fn dev_package_opt_level(manifest: &str, name: &str) -> Option<u32> {
        let header = format!("[profile.dev.package.{name}]");
        let mut inside = false;
        for line in manifest.lines().map(str::trim) {
            if line.starts_with('[') {
                inside = line == header;
            } else if inside && let Some(rest) = line.strip_prefix("opt-level") {
                let value = rest.trim_start().strip_prefix('=')?;
                return value.split('#').next()?.trim().parse().ok();
            }
        }
        None
    }

    #[test]
    fn the_opt_level_of_one_dev_package_is_read_from_its_own_table() {
        let manifest = r#"
[profile.dev.package."*"]
# a comment
opt-level = 2
debug = "line-tables-only"
[profile.dev.package.sha2]
opt-level = 3   # why
[profile.dev.package.kourier-media]
debug = true
[profile.release]
opt-level = 1
"#;
        assert_eq!(dev_package_opt_level(manifest, "\"*\""), Some(2));
        assert_eq!(dev_package_opt_level(manifest, "sha2"), Some(3));
        // A table without the key, a table that is not there and a key of another table.
        assert_eq!(dev_package_opt_level(manifest, "kourier-media"), None);
        assert_eq!(dev_package_opt_level(manifest, "kourier-blob"), None);
        assert_eq!(
            dev_package_opt_level("[profile.dev]\nopt-level = 1\n", "\"*\""),
            None
        );
    }

    /// Without this the dev and test builds run libsignal, SQLCipher, BoringSSL and the image
    /// codecs unoptimised again: a chat screenshot cost 9.5 CPU-s instead of ~3 (T2 measured
    /// the effect, `target/next/t2-build.md`). A test cannot assert a CPU time (load decides),
    /// so it asserts the setting.
    #[test]
    fn dependencies_and_the_hot_workspace_crates_are_optimised_in_dev_builds() {
        let manifest = std::fs::read_to_string(crate::root().join("Cargo.toml")).unwrap();
        let level = |name: &str| dev_package_opt_level(&manifest, name);
        assert!(
            level("\"*\"") >= Some(1),
            "[profile.dev.package.\"*\"] opt-level must be 1 or more: {:?}",
            level("\"*\"")
        );
        for krate in DEV_OPTIMISED {
            assert!(
                crate::root()
                    .join("crates")
                    .join(krate)
                    .join("Cargo.toml")
                    .is_file(),
                "{krate} is not a workspace crate any more: rename it in DEV_OPTIMISED and Cargo.toml"
            );
            assert!(
                level(krate) >= Some(1),
                "[profile.dev.package.{krate}] opt-level must be 1 or more: {:?}",
                level(krate)
            );
        }
        // The two that were there before T2 stay (a named package wins over "*").
        assert_eq!(level("sqlcipher-crypto-provider"), Some(3));
        assert_eq!(level("sha2"), Some(3));
    }
}
