//! T5e: when a change of the root `Cargo.toml` and `Cargo.lock` is only a new workspace member
//! and that member's own dependency lines, the quick tier reads it as a change of that member,
//! not as "every build can change" (the full run).
//!
//! Why: a lane that adds a crate (the WhatsApp backend in the second worktree) edits the root
//! manifest (new `[workspace.dependencies]` keys) and the lock file (new packages), and until now
//! every quick run in it was the full run. The rule is strict on purpose, so that what it lets
//! through cannot change any other package's build:
//!
//! - the root manifest: every section and every line equal to the base's, except new KEYS of
//!   `[workspace.dependencies]` and new entries of an explicit `[workspace] members` list. A new
//!   section, a removed line, a changed value of an existing key (a shared dependency's version
//!   or features), `[patch]`, `[profile.*]`, `[workspace.package]`, `[workspace.lints]` and
//!   `default-members` all make it shared.
//! - the lock file: every package block of the base is still there, byte for byte, except that a
//!   workspace member's block (it has no `source`) may have grown new `dependencies` entries (its
//!   own manifest changed, so it is a changed package anyway). A changed version, source or
//!   checksum, a block of an external package that gained or lost an entry (a feature that turns
//!   an optional dependency on shows exactly so), a removed package, or a changed lock version
//!   make it shared. New blocks (the member itself and the packages only it pulls in) are fine.
//!
//! The features a new member turns on (T5 review F4). The lock file cannot show a `cfg` feature
//! of an existing crate, and feature unification makes it reach every `--workspace` build, so
//! [`shared_feature_use`] also reads the NEW members' own manifests and the new
//! `[workspace.dependencies]` keys: a dependency that carries `features`, `default-features` or
//! `package` (or a `[features]` line `dep/feature`) and whose target the base already builds (a
//! package of the base's lock file, or a crate directory with a `Cargo.toml` at the base) makes
//! the change shared. A dependency on a package that is new to the base stays additive.
//!
//! Known limit, written down: a plain `workspace = true` dependency on an existing package whose
//! workspace entry already carries features nobody else uses, and a dependency of a new member's
//! dependency, are not seen. The full run (`cargo xtask ci`) before the lane's landing sees them.
//!
//! Line based, no TOML dependency (a new dependency of xtask would change `Cargo.lock`).

use std::collections::{BTreeMap, BTreeSet};

/// What a change of the root `Cargo.toml` and `Cargo.lock` means for the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Root {
    /// Nothing but comments or white space differs.
    Same,
    /// Only a new member's lines: the new packages of the lock file, the new
    /// `[workspace.dependencies]` keys and `members` entries, and the members whose lock block
    /// gained dependency entries.
    Additive(Added),
    /// Something every package can feel; the reason is one sentence for the report.
    Shared(String),
}

/// What an additive change adds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Added {
    /// New keys of `[workspace.dependencies]`.
    pub dependencies: Vec<String>,
    /// New entries of an explicit `members` list.
    pub members: Vec<String>,
    /// Packages that are new in the lock file (name, sorted).
    pub packages: Vec<String>,
    /// Workspace members whose lock block gained dependency entries (name, sorted).
    pub grown: Vec<String>,
}

impl Added {
    /// One line for the report: what the change adds.
    pub fn describe(&self) -> String {
        let part = |what: &str, list: &[String]| {
            (!list.is_empty()).then(|| format!("{what}: {}", list.join(", ")))
        };
        [
            part("new workspace dependencies", &self.dependencies),
            part("new members entries", &self.members),
            part("new lock packages", &self.packages),
            part("members whose lock block grew", &self.grown),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("; ")
    }
}

/// The verdict for the pair of files; `old_*` are the base's text, `new_*` the working tree's.
pub fn classify(old_manifest: &str, new_manifest: &str, old_lock: &str, new_lock: &str) -> Root {
    let manifest = match compare_manifests(old_manifest, new_manifest) {
        Ok(added) => added,
        Err(reason) => return Root::Shared(format!("Cargo.toml: {reason}")),
    };
    let lock = match compare_locks(old_lock, new_lock) {
        Ok(added) => added,
        Err(reason) => return Root::Shared(format!("Cargo.lock: {reason}")),
    };
    let added = Added {
        dependencies: manifest.dependencies,
        members: manifest.members,
        packages: lock.packages,
        grown: lock.grown,
    };
    if added == Added::default() {
        Root::Same
    } else {
        Root::Additive(added)
    }
}

// ---------------------------------------------------------------------------------------------
// The root manifest.

/// One `key = value` line of a section (a value that spans lines is joined), values with their
/// white space collapsed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    key: String,
    value: String,
}

/// A manifest as its sections, in file order, each with its entries.
#[derive(Debug, Default)]
struct Doc {
    sections: Vec<(String, Vec<Entry>)>,
}

impl Doc {
    fn entries(&self, name: &str) -> Vec<&Entry> {
        self.sections
            .iter()
            .filter(|(n, _)| n == name)
            .flat_map(|(_, e)| e.iter())
            .collect()
    }

    fn names(&self) -> BTreeSet<&str> {
        self.sections.iter().map(|(n, _)| n.as_str()).collect()
    }
}

/// The line without its comment: a `#` outside a string starts one.
fn strip_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        match quote {
            Some(q) => {
                if escaped {
                    escaped = false;
                } else if c == '\\' && q == '"' {
                    escaped = true;
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                '#' => return &line[..i],
                _ => {}
            },
        }
    }
    line
}

/// How many brackets (`[` and `{`) the text leaves open, outside strings.
fn open_depth(text: &str) -> i32 {
    let mut depth = 0;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in text.chars() {
        match quote {
            Some(q) => {
                if escaped {
                    escaped = false;
                } else if c == '\\' && q == '"' {
                    escaped = true;
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                '[' | '{' => depth += 1,
                ']' | '}' => depth -= 1,
                _ => {}
            },
        }
    }
    depth
}

/// White space outside strings collapsed to one space, the ends trimmed.
fn collapse(text: &str) -> String {
    let mut out = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut space = false;
    for c in text.chars() {
        if let Some(q) = quote {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' && q == '"' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        if c.is_whitespace() {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        if c == '"' || c == '\'' {
            quote = Some(c);
        }
        out.push(c);
    }
    out
}

/// The position of the first `=` outside strings.
fn split_key(line: &str) -> Option<(&str, &str)> {
    let mut quote: Option<char> = None;
    for (i, c) in line.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                '"' | '\'' => quote = Some(c),
                '=' => return Some((line[..i].trim(), line[i + 1..].trim())),
                _ => {}
            },
        }
    }
    None
}

fn parse_manifest(text: &str) -> Doc {
    let mut doc = Doc::default();
    doc.sections.push((String::new(), Vec::new()));
    // The entry whose value is still open (its key, the text so far).
    let mut pending: Option<(String, String)> = None;
    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some((key, mut value)) = pending.take() {
            value.push(' ');
            value.push_str(line);
            if open_depth(&value) > 0 {
                pending = Some((key, value));
            } else {
                push_entry(&mut doc, key, &value);
            }
            continue;
        }
        if line.starts_with('[') {
            let name = line.trim_matches(|c| c == '[' || c == ']').trim();
            // `[[bin]]` and `[bin]` are different things: keep the doubled brackets apart.
            let name = if line.starts_with("[[") {
                format!("[{name}]")
            } else {
                name.to_owned()
            };
            doc.sections.push((collapse_name(&name), Vec::new()));
            continue;
        }
        let Some((key, value)) = split_key(line) else {
            // A line that is neither: keep it as an entry without a key so a change shows.
            push_entry(&mut doc, String::new(), line);
            continue;
        };
        if open_depth(value) > 0 {
            pending = Some((key.to_owned(), value.to_owned()));
        } else {
            push_entry(&mut doc, key.to_owned(), value);
        }
    }
    if let Some((key, value)) = pending {
        push_entry(&mut doc, key, &value);
    }
    doc
}

/// `profile.dev.package."*"` keeps its dots and quotes; only the blanks around dots go.
fn collapse_name(name: &str) -> String {
    name.split('.').map(str::trim).collect::<Vec<_>>().join(".")
}

fn push_entry(doc: &mut Doc, key: String, value: &str) {
    let entry = Entry {
        key,
        value: collapse(value),
    };
    if let Some((_, entries)) = doc.sections.last_mut() {
        entries.push(entry);
    }
}

/// The strings of a one-line or joined array value: `["a", "b"]` -> a, b. `None` when the
/// value is not an array of plain strings.
fn string_array(value: &str) -> Option<Vec<String>> {
    let inner = value.trim().strip_prefix('[')?.strip_suffix(']')?;
    let mut out = Vec::new();
    for part in inner.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let s = part
            .strip_prefix('"')
            .and_then(|p| p.strip_suffix('"'))
            .or_else(|| part.strip_prefix('\'').and_then(|p| p.strip_suffix('\'')))?;
        out.push(s.to_owned());
    }
    Some(out)
}

#[derive(Debug, Default)]
struct ManifestAdded {
    dependencies: Vec<String>,
    members: Vec<String>,
}

fn compare_manifests(old: &str, new: &str) -> Result<ManifestAdded, String> {
    let (old, new) = (parse_manifest(old), parse_manifest(new));
    let mut added = ManifestAdded::default();
    let (old_names, new_names) = (old.names(), new.names());
    if let Some(name) = new_names.difference(&old_names).next() {
        return Err(format!("a new section [{name}] is shared"));
    }
    if let Some(name) = old_names.difference(&new_names).next() {
        return Err(format!("the section [{name}] is gone"));
    }
    for name in &old_names {
        let (before, after) = (old.entries(name), new.entries(name));
        match *name {
            "workspace.dependencies" => compare_dependencies(&before, &after, &mut added)?,
            "workspace" => compare_workspace(&before, &after, &mut added)?,
            _ => {
                if before != after {
                    return Err(format!("[{}] changed, and it is shared", display(name)));
                }
            }
        }
    }
    added.dependencies.sort();
    added.members.sort();
    Ok(added)
}

fn display(section: &str) -> &str {
    if section.is_empty() {
        "the top of the file"
    } else {
        section
    }
}

/// Existing keys keep their value (a shared dependency's version or features change every
/// package that uses it); new keys are new.
fn compare_dependencies(
    before: &[&Entry],
    after: &[&Entry],
    added: &mut ManifestAdded,
) -> Result<(), String> {
    let old: BTreeMap<&str, &str> = before
        .iter()
        .map(|e| (e.key.as_str(), e.value.as_str()))
        .collect();
    let new: BTreeMap<&str, &str> = after
        .iter()
        .map(|e| (e.key.as_str(), e.value.as_str()))
        .collect();
    for (key, value) in &old {
        match new.get(key) {
            None => return Err(format!("[workspace.dependencies] {key} was removed")),
            Some(now) if now != value => {
                return Err(format!(
                    "[workspace.dependencies] {key} changed (its version, features or source \
                     reach every package that uses it)"
                ));
            }
            Some(_) => {}
        }
    }
    if new.len() < after.len() || old.len() < before.len() {
        return Err("[workspace.dependencies] names a key twice".to_owned());
    }
    added.dependencies.extend(
        new.keys()
            .filter(|k| !old.contains_key(*k))
            .map(|k| (*k).to_owned()),
    );
    Ok(())
}

/// `[workspace]`: `members` may gain entries, nothing else may differ.
fn compare_workspace(
    before: &[&Entry],
    after: &[&Entry],
    added: &mut ManifestAdded,
) -> Result<(), String> {
    let keys = |entries: &[&Entry]| -> BTreeSet<String> {
        entries.iter().map(|e| e.key.clone()).collect()
    };
    if keys(before) != keys(after) {
        return Err("[workspace] gained or lost a key, and it is shared".to_owned());
    }
    for old in before {
        let Some(new) = after.iter().find(|e| e.key == old.key) else {
            continue;
        };
        if old.value == new.value {
            continue;
        }
        if old.key == "members"
            && let (Some(was), Some(now)) = (string_array(&old.value), string_array(&new.value))
        {
            if let Some(gone) = was.iter().find(|m| !now.contains(m)) {
                return Err(format!("[workspace] members lost {gone:?}"));
            }
            added
                .members
                .extend(now.into_iter().filter(|m| !was.contains(m)));
            continue;
        }
        return Err(format!("[workspace] {} changed, and it is shared", old.key));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The lock file.

/// One `[[package]]` block: its identity, its lines other than the dependency list, and the
/// dependency entries.
#[derive(Debug, PartialEq, Eq)]
struct Block {
    name: String,
    version: String,
    /// The `source = "…"` of an external package; a workspace member has none.
    source: Option<String>,
    /// Every line except the `dependencies` array, trimmed.
    lines: Vec<String>,
    dependencies: Vec<String>,
    /// The block's text as it stood, to compare an external one byte for byte.
    text: String,
}

#[derive(Debug, Default)]
struct Lock {
    /// What stands before the first block (`version = 4`).
    head: String,
    blocks: Vec<Block>,
}

fn quoted(line: &str, key: &str) -> Option<String> {
    let rest = line
        .strip_prefix(key)?
        .trim_start()
        .strip_prefix('=')?
        .trim();
    Some(rest.trim_matches('"').to_owned())
}

fn parse_lock(text: &str) -> Lock {
    let mut lock = Lock::default();
    let mut current: Option<Block> = None;
    let mut in_deps = false;
    for line in text.lines() {
        if line.trim() == "[[package]]" {
            if let Some(block) = current.take() {
                lock.blocks.push(block);
            }
            current = Some(Block {
                name: String::new(),
                version: String::new(),
                source: None,
                lines: Vec::new(),
                dependencies: Vec::new(),
                text: String::new(),
            });
            in_deps = false;
            continue;
        }
        let Some(block) = current.as_mut() else {
            lock.head.push_str(line.trim());
            lock.head.push('\n');
            continue;
        };
        let t = line.trim();
        // Blank lines separate blocks: a block's text has none, so the last block of a file
        // is the same text whether or not another block follows it.
        if !t.is_empty() {
            block.text.push_str(line);
            block.text.push('\n');
        }
        if in_deps {
            if t == "]" {
                in_deps = false;
            } else if !t.is_empty() {
                block
                    .dependencies
                    .push(t.trim_end_matches(',').trim_matches('"').to_owned());
            }
            continue;
        }
        if t.starts_with("dependencies") && t.ends_with('[') {
            in_deps = true;
            continue;
        }
        if t.starts_with("dependencies") && t.contains("[]") {
            // An empty list written on one line.
            continue;
        }
        if t.is_empty() {
            continue;
        }
        if let Some(name) = quoted(t, "name") {
            block.name = name;
        } else if let Some(version) = quoted(t, "version") {
            block.version = version;
        } else if let Some(source) = quoted(t, "source") {
            block.source = Some(source);
        }
        block.lines.push(t.to_owned());
    }
    if let Some(block) = current {
        lock.blocks.push(block);
    }
    lock
}

#[derive(Debug, Default)]
struct LockAdded {
    packages: Vec<String>,
    grown: Vec<String>,
}

fn compare_locks(old: &str, new: &str) -> Result<LockAdded, String> {
    if old == new {
        return Ok(LockAdded::default());
    }
    let (old, new) = (parse_lock(old), parse_lock(new));
    if old.head != new.head {
        return Err("the lock file's own header changed".to_owned());
    }
    let key = |b: &Block| -> (String, String, Option<String>) {
        (b.name.clone(), b.version.clone(), b.source.clone())
    };
    let mut now: BTreeMap<(String, String, Option<String>), &Block> = BTreeMap::new();
    for block in &new.blocks {
        if now.insert(key(block), block).is_some() {
            return Err(format!("{} {} stands twice", block.name, block.version));
        }
    }
    let mut added = LockAdded::default();
    let mut seen = BTreeSet::new();
    for block in &old.blocks {
        let k = key(block);
        seen.insert(k.clone());
        let Some(after) = now.get(&k) else {
            return Err(format!(
                "{} {} is gone or changed its version or source",
                block.name, block.version
            ));
        };
        if block.text == after.text {
            continue;
        }
        if block.source.is_some() {
            return Err(format!(
                "the package {} {} changed (an optional dependency turned on or off, or a \
                 checksum)",
                block.name, block.version
            ));
        }
        // A workspace member: its own lines stay, its dependencies may only grow.
        let gone = block
            .dependencies
            .iter()
            .any(|d| !after.dependencies.contains(d));
        if block.lines != after.lines || gone {
            return Err(format!(
                "the workspace member {} lost a dependency or changed its lines",
                block.name
            ));
        }
        added.grown.push(block.name.clone());
    }
    for (k, block) in &now {
        if !seen.contains(k) {
            added.packages.push(block.name.clone());
        }
    }
    added.packages.sort();
    added.packages.dedup();
    added.grown.sort();
    Ok(added)
}

// ---------------------------------------------------------------------------------------------
// Features turned on for crates the base already builds (T5 review F4).

/// The fields of a dependency entry that matter here.
#[derive(Debug, Default, Clone)]
struct Spec {
    /// `features = […]` is there.
    features: bool,
    /// `default-features = …` (or `default_features`) is there.
    default_features: bool,
    /// `package = "…"`: the real name of a renamed dependency.
    package: Option<String>,
    /// `path = "…"`.
    path: Option<String>,
    /// `workspace = true`.
    workspace: bool,
}

impl Spec {
    /// Whether the entry can change how a crate that exists is built.
    fn carries_features(&self) -> bool {
        self.features || self.default_features || self.package.is_some()
    }

    fn set(&mut self, key: &str, value: &str) {
        let text = value
            .trim()
            .trim_matches(|c| c == '"' || c == '\'')
            .to_owned();
        match key {
            "features" => self.features = true,
            "default-features" | "default_features" => self.default_features = true,
            "package" => self.package = Some(text),
            "path" => self.path = Some(text),
            "workspace" => self.workspace = value.trim() == "true",
            _ => {}
        }
    }
}

/// The `key = value` pairs of an inline table `{ a = 1, b = [..] }` (`None` when the value is not
/// one): commas inside strings and brackets do not split.
fn inline_fields(value: &str) -> Option<Vec<(String, String)>> {
    let inner = value.trim().strip_prefix('{')?.strip_suffix('}')?;
    let mut parts = Vec::new();
    let (mut depth, mut quote, mut start) = (0i32, None::<char>, 0usize);
    for (i, c) in inner.char_indices() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                '[' | '{' => depth += 1,
                ']' | '}' => depth -= 1,
                ',' if depth == 0 => {
                    parts.push(&inner[start..i]);
                    start = i + 1;
                }
                _ => {}
            },
        }
    }
    parts.push(&inner[start..]);
    Some(
        parts
            .into_iter()
            .filter_map(|p| split_key(p.trim()))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
    )
}

/// The spec of one `key = value` dependency entry (a bare version string has no field).
fn spec_of_value(value: &str) -> Spec {
    let mut spec = Spec::default();
    for (key, value) in inline_fields(value).unwrap_or_default() {
        spec.set(&key, &value);
    }
    spec
}

/// The dependencies a manifest names, with their spec: the entries of `[dependencies]`,
/// `[dev-dependencies]`, `[build-dependencies]` (also under `[target.<cfg>.…]`) and of the
/// dotted tables `[dependencies.name]`. With `workspace` the root's `[workspace.dependencies]`
/// are read in the same way.
fn dependency_specs(doc: &Doc, workspace: bool) -> Vec<(String, Spec)> {
    let mut out = Vec::new();
    for (name, entries) in &doc.sections {
        let kinds: &[&str] = if workspace {
            &["workspace.dependencies"]
        } else {
            &["dependencies", "dev-dependencies", "build-dependencies"]
        };
        for kind in kinds {
            let flat = name == kind
                || (name.starts_with("target.") && name.ends_with(&format!(".{kind}")));
            if flat {
                // `name = {…}`, and the dotted form `name.features = […]` (merged into `name`).
                let mut merged: Vec<(String, Spec)> = Vec::new();
                for e in entries {
                    let (name, field) = match e.key.split_once('.') {
                        Some((name, field)) => (name.trim_matches('"'), Some(field)),
                        None => (e.key.as_str(), None),
                    };
                    let at = match merged.iter().position(|(k, _)| k == name) {
                        Some(at) => at,
                        None => {
                            merged.push((name.to_owned(), Spec::default()));
                            merged.len() - 1
                        }
                    };
                    match field {
                        Some(field) => merged[at].1.set(field, &e.value),
                        None => {
                            let spec = spec_of_value(&e.value);
                            let have = &mut merged[at].1;
                            have.features |= spec.features;
                            have.default_features |= spec.default_features;
                            have.workspace |= spec.workspace;
                            have.package = have.package.take().or(spec.package);
                            have.path = have.path.take().or(spec.path);
                        }
                    }
                }
                out.extend(merged);
                continue;
            }
            // `[dependencies.name]`, `[target.cfg.dependencies.name]`.
            let marker = format!("{kind}.");
            let dotted = if let Some(rest) = name.strip_prefix(&marker) {
                Some(rest)
            } else {
                name.find(&format!(".{marker}"))
                    .filter(|_| name.starts_with("target."))
                    .map(|i| &name[i + 1 + marker.len()..])
            };
            if let Some(key) = dotted {
                let mut spec = Spec::default();
                for e in entries {
                    spec.set(&e.key, &e.value);
                }
                out.push((key.trim_matches('"').to_owned(), spec));
            }
        }
    }
    out
}

/// What a dependency names: a package, or a crate directory.
enum Target {
    Package(String),
    Dir(String),
}

/// What the dependency `key` with `spec` points at, a `workspace = true` one through the root's
/// own entry.
fn target_of(key: &str, spec: &Spec, root: &BTreeMap<String, Spec>) -> Target {
    if let Some(package) = &spec.package {
        return Target::Package(package.clone());
    }
    let mut spec = spec.clone();
    if spec.workspace
        && let Some(inherited) = root.get(key)
    {
        spec = inherited.clone();
    }
    if let Some(package) = spec.package {
        return Target::Package(package);
    }
    match spec.path {
        Some(path) => Target::Dir(path.trim_matches('/').to_owned()),
        None => Target::Package(key.to_owned()),
    }
}

/// Whether the base builds the target: a package of its lock file or a directory with a
/// `Cargo.toml` at the base.
fn base_builds(
    target: &Target,
    packages: &BTreeSet<String>,
    has_file: &dyn Fn(&str) -> bool,
) -> Option<String> {
    match target {
        Target::Package(name) => packages.contains(name).then(|| name.clone()),
        Target::Dir(dir) => has_file(&format!("{dir}/Cargo.toml")).then(|| dir.clone()),
    }
}

/// The reason a new member or a new workspace dependency makes the change shared because it
/// turns features on (or renames) something the base already builds; `None` when it does not.
/// `new_members` are the new members' names with their `Cargo.toml` text, `base_has_file`
/// answers whether the base has a file (a crate directory's manifest).
pub fn shared_feature_use(
    old_manifest: &str,
    new_manifest: &str,
    old_lock: &str,
    new_members: &[(String, String)],
    base_has_file: &dyn Fn(&str) -> bool,
) -> Option<String> {
    let (old, new) = (parse_manifest(old_manifest), parse_manifest(new_manifest));
    let packages: BTreeSet<String> = parse_lock(old_lock)
        .blocks
        .into_iter()
        .map(|b| b.name)
        .collect();
    let old_keys: BTreeSet<String> = dependency_specs(&old, true)
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    let root: BTreeMap<String, Spec> = dependency_specs(&new, true).into_iter().collect();
    for (key, spec) in &root {
        if old_keys.contains(key) || !spec.carries_features() {
            continue;
        }
        if let Some(what) = base_builds(&target_of(key, spec, &root), &packages, base_has_file) {
            return Some(format!(
                "the new [workspace.dependencies] key {key} sets features or a package name for {what}, \
                 which the base already builds (feature unification reaches every build)"
            ));
        }
    }
    for (member, text) in new_members {
        let doc = parse_manifest(text);
        let deps = dependency_specs(&doc, false);
        for (key, spec) in &deps {
            if !spec.carries_features() {
                continue;
            }
            if let Some(what) = base_builds(&target_of(key, spec, &root), &packages, base_has_file)
            {
                return Some(format!(
                    "the new member {member} sets features or a package name for {what} \
                     (dependency {key}), which the base already builds"
                ));
            }
        }
        // `[features] x = ["dep/feature", "dep?/feature"]` turns a feature on in a dependency.
        for entry in doc.entries("features") {
            for item in string_array(&entry.value).unwrap_or_default() {
                let Some((dep, _)) = item.split_once('/') else {
                    continue;
                };
                let dep = dep.trim_start_matches("dep:").trim_end_matches('?');
                let Some((key, spec)) = deps.iter().find(|(k, _)| k == dep) else {
                    continue;
                };
                if let Some(what) =
                    base_builds(&target_of(key, spec, &root), &packages, base_has_file)
                {
                    return Some(format!(
                        "the new member {member} turns a feature of {what} on through its \
                         feature {} ({item}), and the base already builds {what}",
                        entry.key
                    ));
                }
            }
        }
    }
    None
}

/// [`classify`], and a change that is only a new member's lines is shared all the same when the
/// member (or a new workspace key) turns features on for something the base builds
/// ([`shared_feature_use`]).
pub fn classify_with_members(
    old_manifest: &str,
    new_manifest: &str,
    old_lock: &str,
    new_lock: &str,
    new_members: &[(String, String)],
    base_has_file: &dyn Fn(&str) -> bool,
) -> Root {
    let verdict = classify(old_manifest, new_manifest, old_lock, new_lock);
    if let Root::Additive(_) = &verdict
        && let Some(reason) = shared_feature_use(
            old_manifest,
            new_manifest,
            old_lock,
            new_members,
            base_has_file,
        )
    {
        return Root::Shared(format!("Cargo.toml: {reason}"));
    }
    verdict
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of the real file, shortened.
    const BASE: &str = r#"
[workspace]
resolver = "3"
members = ["crates/*", "xtask"]
default-members = ["crates/*"]

[workspace.package]
version = "0.1.0"
edition = "2024"

[workspace.dependencies]
# Internal crates
kourier-model = { path = "crates/kourier-model" }
anyhow = "1"
tokio = { version = "1.52", features = ["rt-multi-thread", "macros", "time"] }
rustls = { version = "0.23", default-features = false, features = [
    "ring",
    "std",   # a comment inside the array
] }
url-dep = { git = "https://example.org/x?a=1#frag", rev = "abc" } # trailing comment

[patch.crates-io]
boring = { git = "https://github.com/signalapp/boring", tag = "signal-v5.2.0" }

[profile.dev.package."*"]
opt-level = 2
debug = "line-tables-only"

[profile.release]
lto = "thin"
"#;

    fn with(base: &str, from: &str, to: &str) -> String {
        assert!(base.contains(from), "{from:?} not in the base");
        base.replacen(from, to, 1)
    }

    fn manifest(new: &str) -> Result<ManifestAdded, String> {
        compare_manifests(BASE, new)
    }

    fn reason(new: &str) -> String {
        manifest(new).expect_err("shared")
    }

    #[test]
    fn comments_and_white_space_are_no_change() {
        let spaced = BASE
            .replace("anyhow = \"1\"", "anyhow   =   \"1\"   # for errors")
            .replace("\n\n", "\n\n\n# a new comment\n");
        let added = manifest(&spaced).expect("same");
        assert!(added.dependencies.is_empty() && added.members.is_empty());
        // A `#` inside a string is not a comment: the URL fragment is part of the value.
        let doc = parse_manifest(BASE);
        let url = doc
            .entries("workspace.dependencies")
            .into_iter()
            .find(|e| e.key == "url-dep")
            .expect("url-dep");
        assert!(url.value.contains("#frag"), "{}", url.value);
        assert!(!url.value.contains("trailing"), "{}", url.value);
    }

    #[test]
    fn a_multi_line_value_is_one_entry_without_its_comments() {
        let doc = parse_manifest(BASE);
        let rustls = doc
            .entries("workspace.dependencies")
            .into_iter()
            .find(|e| e.key == "rustls")
            .expect("rustls");
        assert_eq!(
            rustls.value,
            r#"{ version = "0.23", default-features = false, features = [ "ring", "std", ] }"#
        );
    }

    #[test]
    fn a_new_workspace_dependency_key_is_additive() {
        let new = with(
            BASE,
            "anyhow = \"1\"\n",
            "anyhow = \"1\"\nnew-thing = { version = \"2\", default-features = false }\nnewer = \"3\"\n",
        );
        let added = manifest(&new).expect("additive");
        assert_eq!(added.dependencies, ["new-thing", "newer"]);
        assert!(added.members.is_empty());
    }

    #[test]
    fn a_changed_value_of_an_existing_dependency_is_shared() {
        // A version, a feature added to a shared crate (W1's real shape: nine shared crates
        // gained features), a source or a removal: each reaches every user of the crate.
        for (from, to) in [
            ("anyhow = \"1\"", "anyhow = \"1.0.5\""),
            (
                "features = [\"rt-multi-thread\", \"macros\", \"time\"]",
                "features = [\"rt-multi-thread\", \"macros\", \"time\", \"fs\"]",
            ),
            ("rev = \"abc\"", "rev = \"abd\""),
            ("kourier-model = { path = \"crates/kourier-model\" }\n", ""),
        ] {
            let r = reason(&with(BASE, from, to));
            assert!(r.contains("[workspace.dependencies]"), "{from}: {r}");
        }
        let r = reason(&with(BASE, "\"ring\",", "\"ring\", \"tls12\","));
        assert!(r.contains("rustls"), "{r}");
    }

    #[test]
    fn a_new_section_is_shared() {
        for section in [
            "\n[profile.release.package.whatsapp-rust]\nopt-level = \"s\"\n",
            "\n[workspace.lints.rust]\nunsafe_code = \"warn\"\n",
            "\n[workspace.metadata.x]\na = 1\n",
        ] {
            let r = reason(&format!("{BASE}{section}"));
            assert!(r.contains("new section"), "{r}");
        }
    }

    #[test]
    fn a_changed_patch_profile_or_package_table_is_shared() {
        for (from, to, name) in [
            (
                "tag = \"signal-v5.2.0\"",
                "tag = \"signal-v5.3.0\"",
                "patch.crates-io",
            ),
            (
                "opt-level = 2\n",
                "opt-level = 3\n",
                "profile.dev.package.\"*\"",
            ),
            ("lto = \"thin\"", "lto = \"fat\"", "profile.release"),
            (
                "edition = \"2024\"",
                "edition = \"2021\"",
                "workspace.package",
            ),
            (
                "default-members = [\"crates/*\"]",
                "default-members = [\"crates/*\", \"xtask\"]",
                "workspace",
            ),
        ] {
            let r = reason(&with(BASE, from, to));
            assert!(r.contains(name), "{from}: {r}");
        }
    }

    #[test]
    fn an_explicit_members_list_may_gain_entries_but_not_lose_them() {
        let new = with(
            BASE,
            "members = [\"crates/*\", \"xtask\"]",
            "members = [\"crates/*\", \"xtask\", \"crates/new-one\"]",
        );
        assert_eq!(
            manifest(&new).expect("additive").members,
            ["crates/new-one"]
        );
        let r = reason(&with(
            BASE,
            "members = [\"crates/*\", \"xtask\"]",
            "members = [\"crates/*\"]",
        ));
        assert!(r.contains("members lost"), "{r}");
    }

    #[test]
    fn a_removed_section_is_shared() {
        let r = reason(&with(BASE, "[profile.release]\nlto = \"thin\"\n", ""));
        assert!(r.contains("is gone"), "{r}");
    }

    #[test]
    fn the_real_manifest_parses_into_its_sections() {
        let text = include_str!("../../Cargo.toml");
        let doc = parse_manifest(text);
        let names = doc.names();
        for want in [
            "workspace",
            "workspace.package",
            "workspace.dependencies",
            "patch.crates-io",
            "profile.dev.package.\"*\"",
            "profile.release",
        ] {
            assert!(names.contains(want), "{want} missing from {names:?}");
        }
        // One entry per `key = value` line of the dependency table, none lost to a comment.
        let table = doc.entries("workspace.dependencies");
        assert!(table.len() > 50, "{} entries", table.len());
        assert!(
            table
                .iter()
                .all(|e| !e.key.is_empty() && !e.value.is_empty())
        );
        // Compared with itself the real file is no change at all.
        assert_eq!(
            compare_manifests(text, text)
                .expect("same")
                .dependencies
                .len(),
            0
        );
    }

    const LOCK: &str = r#"# This file is automatically @generated by Cargo.
# It is not intended for manual editing.
version = 4

[[package]]
name = "anyhow"
version = "1.0.100"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "aaa"

[[package]]
name = "kourier-app"
version = "0.1.0"
dependencies = [
 "anyhow",
 "kourier-model",
]

[[package]]
name = "kourier-model"
version = "0.1.0"

[[package]]
name = "tokio"
version = "1.52.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "bbb"
dependencies = [
 "anyhow",
 "pin-project-lite",
]

[[package]]
name = "pin-project-lite"
version = "0.2.16"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "ccc"
"#;

    fn lock(new: &str) -> Result<LockAdded, String> {
        compare_locks(LOCK, new)
    }

    #[test]
    fn new_packages_in_the_lock_are_additive() {
        let new = format!(
            "{LOCK}\n[[package]]\nname = \"kourier-whatsapp\"\nversion = \"0.1.0\"\n\
             dependencies = [\n \"anyhow\",\n \"wacore\",\n]\n\n[[package]]\nname = \"wacore\"\n\
             version = \"0.2.0\"\nsource = \"git+https://example.org/wa?rev=1#1\"\n"
        );
        let added = lock(&new).expect("additive");
        assert_eq!(added.packages, ["kourier-whatsapp", "wacore"]);
        assert!(added.grown.is_empty());
    }

    #[test]
    fn a_second_version_of_an_existing_package_is_a_new_block() {
        let new = format!(
            "{LOCK}\n[[package]]\nname = \"anyhow\"\nversion = \"1.0.101\"\nsource = \
             \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"ddd\"\n"
        );
        let added = lock(&new).expect("additive");
        assert_eq!(added.packages, ["anyhow"]);
    }

    #[test]
    fn a_workspace_member_may_gain_dependency_entries() {
        let new = with(
            LOCK,
            " \"anyhow\",\n \"kourier-model\",\n]",
            " \"anyhow\",\n \"kourier-model\",\n \"kourier-whatsapp\",\n]",
        );
        let new = format!("{new}\n[[package]]\nname = \"kourier-whatsapp\"\nversion = \"0.1.0\"\n");
        let added = lock(&new).expect("additive");
        assert_eq!(added.grown, ["kourier-app"]);
        assert_eq!(added.packages, ["kourier-whatsapp"]);
    }

    #[test]
    fn a_workspace_member_that_lost_a_dependency_is_shared() {
        let r = lock(&with(
            LOCK,
            " \"anyhow\",\n \"kourier-model\",\n]",
            " \"anyhow\",\n]",
        ))
        .expect_err("shared");
        assert!(r.contains("kourier-app"), "{r}");
    }

    #[test]
    fn a_changed_external_package_is_shared() {
        // A new version, a changed checksum, and the case that matters most: a feature of a
        // shared crate that turns an optional dependency on shows as a new entry of ITS block.
        for (from, to) in [
            ("version = \"1.0.100\"", "version = \"1.0.101\""),
            ("checksum = \"aaa\"", "checksum = \"aab\""),
            (
                " \"anyhow\",\n \"pin-project-lite\",\n]",
                " \"anyhow\",\n \"pin-project-lite\",\n \"libc\",\n]",
            ),
            (" \"anyhow\",\n \"pin-project-lite\",\n]", " \"anyhow\",\n]"),
        ] {
            let r = lock(&with(LOCK, from, to)).expect_err("shared");
            assert!(!r.is_empty(), "{from}");
        }
    }

    #[test]
    fn a_removed_package_or_a_new_lock_version_is_shared() {
        let r = lock(&with(
            LOCK,
            "[[package]]\nname = \"kourier-model\"\nversion = \"0.1.0\"\n",
            "",
        ))
        .expect_err("shared");
        assert!(r.contains("kourier-model"), "{r}");
        let r = lock(&with(LOCK, "version = 4", "version = 5")).expect_err("shared");
        assert!(r.contains("header"), "{r}");
    }

    #[test]
    fn the_real_lock_file_parses_block_for_block() {
        let text = include_str!("../../Cargo.lock");
        let parsed = parse_lock(text);
        assert_eq!(parsed.blocks.len(), text.matches("[[package]]").count());
        assert!(
            parsed
                .blocks
                .iter()
                .all(|b| !b.name.is_empty() && !b.version.is_empty())
        );
        assert!(
            parsed
                .blocks
                .iter()
                .any(|b| b.name == "kourier-core" && b.source.is_none()),
            "a workspace member has no source"
        );
        let same = compare_locks(text, text).expect("same");
        assert!(same.packages.is_empty() && same.grown.is_empty());
    }

    #[test]
    fn the_report_names_what_the_change_adds() {
        let added = Added {
            dependencies: vec!["new-thing".to_owned()],
            members: Vec::new(),
            packages: vec!["kourier-new".to_owned(), "new-thing".to_owned()],
            grown: vec!["kourier-app".to_owned()],
        };
        assert_eq!(
            added.describe(),
            "new workspace dependencies: new-thing; new lock packages: kourier-new, new-thing; \
             members whose lock block grew: kourier-app"
        );
        assert_eq!(Added::default().describe(), "");
    }

    // --- T5 review F4: the features a new member turns on --------------------------------------

    /// A new member's manifest with these dependency lines.
    fn new_member(deps: &str) -> Vec<(String, String)> {
        vec![(
            "kourier-newlane".to_owned(),
            format!("[package]\nname = \"kourier-newlane\"\n\n{deps}\n"),
        )]
    }

    /// What the base has: the crate directories of two members.
    fn base_has(file: &str) -> bool {
        file == "crates/kourier-model/Cargo.toml" || file == "crates/kourier-app/Cargo.toml"
    }

    /// The verdict for BASE + `key` (a new `[workspace.dependencies]` line) + a lock file that
    /// has the new member's block, and the new member's manifest `deps`.
    fn verdict(key: &str, deps: &str) -> Root {
        let manifest = with(
            BASE,
            "anyhow = \"1\"\n",
            &format!("anyhow = \"1\"\n{key}\n"),
        );
        let lock = format!(
            "{LOCK}\n[[package]]\nname = \"kourier-newlane\"\nversion = \"0.1.0\"\n\
             dependencies = [\n \"kourier-model\",\n]\n"
        );
        classify_with_members(BASE, &manifest, LOCK, &lock, &new_member(deps), &base_has)
    }

    const NEWLANE: &str = "kourier-newlane = { path = \"crates/kourier-newlane\" }";

    fn shared_because(verdict: Root) -> String {
        match verdict {
            Root::Shared(reason) => reason,
            other => panic!("shared expected, got {other:?}"),
        }
    }

    #[test]
    fn a_new_member_that_turns_features_on_for_a_crate_of_the_base_is_shared() {
        // This repo's real case: `kourier-desktop`'s `testing` feature, turned on by a new member
        // that depends on it, adds no lock edge and rebuilds everything above that crate.
        let reason = shared_because(verdict(
            NEWLANE,
            "[dependencies]\nkourier-model = { workspace = true, features = [\"testing\"] }",
        ));
        assert!(
            reason.starts_with("Cargo.toml: the new member kourier-newlane sets features"),
            "{reason}"
        );
        assert!(reason.contains("crates/kourier-model"), "{reason}");
        // The same in the other spellings of a dependency.
        for deps in [
            "[dependencies.kourier-model]\nworkspace = true\nfeatures = [\"testing\"]",
            "[dev-dependencies]\nkourier-model = { workspace = true, features = [\"testing\"] }",
            "[target.'cfg(unix)'.dependencies]\nkourier-model = { workspace = true, features = [\"x\"] }",
            "[dependencies]\nkourier-model.workspace = true\nkourier-model.features = [\"testing\"]",
            "[dependencies]\ntokio = { workspace = true, default-features = false }",
            "[dependencies]\nkourier-model = { workspace = true }\n\n[features]\nwith-testing = [\"kourier-model/testing\"]",
            "[dependencies]\nkourier-model = { workspace = true, optional = true }\n\n[features]\nwith-testing = [\"dep:kourier-model\", \"kourier-model?/testing\"]",
        ] {
            let reason = shared_because(verdict(NEWLANE, deps));
            assert!(
                reason.starts_with("Cargo.toml: the new member kourier-newlane"),
                "{deps}: {reason}"
            );
        }
    }

    #[test]
    fn a_new_member_with_plain_dependencies_stays_additive() {
        // Plain `workspace = true`, a version, a path, a feature of its own, a package the base
        // does not build: nothing the base builds changes.
        for deps in [
            "[dependencies]\nkourier-model = { workspace = true }\nanyhow.workspace = true",
            "[dependencies]\nbrand-new = { version = \"1\", features = [\"x\"], default-features = false }",
            "[dependencies]\nrenamed = { package = \"not-in-the-lock\", version = \"1\" }",
            "[dependencies]\nkourier-model = { workspace = true }\n\n[features]\nown = []\nbigger = [\"own\"]",
        ] {
            match verdict(NEWLANE, deps) {
                Root::Additive(added) => {
                    assert_eq!(added.dependencies, ["kourier-newlane"], "{deps}");
                }
                other => panic!("{deps}: {other:?}"),
            }
        }
    }

    #[test]
    fn a_new_workspace_key_that_sets_features_for_a_crate_of_the_base_is_shared() {
        // By path: the base has `crates/kourier-model`.
        let reason = shared_because(verdict(
            "kourier-model-testing = { path = \"crates/kourier-model\", features = [\"testing\"] }",
            "[dependencies]",
        ));
        assert!(
            reason.contains("[workspace.dependencies] key kourier-model-testing"),
            "{reason}"
        );
        // By an alias of a package of the lock file, with a feature that is only a `cfg`.
        let reason = shared_because(verdict(
            "tokio-proc = { package = \"tokio\", version = \"1.52\", features = [\"process\"] }",
            "[dependencies]",
        ));
        assert!(
            reason.contains("tokio-proc") && reason.contains("tokio"),
            "{reason}"
        );
        // By its own name: the key IS a package of the lock file.
        let reason = shared_because(verdict(
            "pin-project-lite = { version = \"0.2\", default-features = false }",
            "[dependencies]",
        ));
        assert!(reason.contains("pin-project-lite"), "{reason}");
        // New to the base: features on a crate nobody else builds are its own.
        assert!(matches!(
            verdict(
                "brand-new = { version = \"1\", features = [\"x\"], default-features = false }",
                "[dependencies]\nbrand-new = { workspace = true }"
            ),
            Root::Additive(_)
        ));
        assert!(matches!(
            verdict(
                "elsewhere = { package = \"not-in-the-lock\", version = \"1\" }",
                "[dependencies]"
            ),
            Root::Additive(_)
        ));
    }

    #[test]
    fn the_feature_check_leaves_a_shared_verdict_as_it_is() {
        // A changed value is shared by `classify`, with its own reason.
        let manifest = with(BASE, "anyhow = \"1\"", "anyhow = \"1.0.5\"");
        let verdict = classify_with_members(BASE, &manifest, LOCK, LOCK, &[], &base_has);
        assert!(
            matches!(&verdict, Root::Shared(r) if r.contains("anyhow")),
            "{verdict:?}"
        );
        assert_eq!(
            classify_with_members(BASE, BASE, LOCK, LOCK, &[], &base_has),
            Root::Same
        );
    }

    #[test]
    fn the_verdict_is_shared_when_either_file_is() {
        let grown = with(
            BASE,
            "anyhow = \"1\"\n",
            "anyhow = \"1\"\nnew-thing = \"2\"\n",
        );
        let lock_plus = format!("{LOCK}\n[[package]]\nname = \"new-thing\"\nversion = \"2.0.0\"\n");
        match classify(BASE, &grown, LOCK, &lock_plus) {
            Root::Additive(added) => {
                assert_eq!(added.dependencies, ["new-thing"]);
                assert_eq!(added.packages, ["new-thing"]);
            }
            other => panic!("{other:?}"),
        }
        // The manifest is shared (a feature of a shared crate), the lock only grows.
        let features = with(&grown, "\"time\"]", "\"time\", \"fs\"]");
        let verdict = classify(BASE, &features, LOCK, &lock_plus);
        assert!(
            matches!(&verdict, Root::Shared(r) if r.starts_with("Cargo.toml:")),
            "{verdict:?}"
        );
        // The manifest only grows, the lock changes a version of an existing package.
        let bumped = with(&lock_plus, "version = \"1.0.100\"", "version = \"1.0.101\"");
        let verdict = classify(BASE, &grown, LOCK, &bumped);
        assert!(
            matches!(&verdict, Root::Shared(r) if r.starts_with("Cargo.lock:")),
            "{verdict:?}"
        );
        // Nothing but white space: the same.
        assert_eq!(
            classify(BASE, &format!("{BASE}\n\n# note\n"), LOCK, LOCK),
            Root::Same
        );
    }
}
