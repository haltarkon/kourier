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
}
