//! What the application says about itself (architecture §8.10, rule R6): name, version,
//! summary, licence, copyright, links, credits. One plain value that every front-end reads for
//! its About page and its toolkit's application data (KDE: `KAboutData`; GNOME:
//! `adw::AboutDialog`), so the text is typed once and no front-end takes the version from its
//! own `env!`.
//!
//! The project has no site yet, so no link is set and no author is named; a front-end shows
//! only what is `Some` or listed. [`AboutInfo::components`] is for the third-party notices
//! (name, version, licence of each library): it stays empty until they are generated from
//! `about.toml` and compiled in.

/// A person or a group credited on the About page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Credit {
    pub name: &'static str,
    /// What they did ("Maintainer", "Translation: German"), if it is stated.
    pub task: Option<&'static str>,
    pub email: Option<&'static str>,
}

/// A third-party component and the licence it is used under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Component {
    pub name: &'static str,
    pub version: &'static str,
    /// An SPDX licence expression.
    pub license: &'static str,
}

/// The application's own data. A front-end copies what it needs; nothing here names a toolkit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AboutInfo {
    /// The name people read ("Kourier"; the working name).
    pub display_name: &'static str,
    /// The name for machines: file names, `KAboutData`'s component name. It is also the gettext
    /// domain, `kourier_viewmodel::i18n::DOMAIN` (this crate does not depend on the viewmodel).
    pub component_name: &'static str,
    /// The version of the workspace (`Cargo.toml` `[workspace.package]`).
    pub version: &'static str,
    /// One line that says what the application is.
    pub summary: &'static str,
    /// The licence as an SPDX identifier.
    pub license: &'static str,
    pub copyright: &'static str,
    pub homepage: Option<&'static str>,
    pub bug_report: Option<&'static str>,
    pub translate: Option<&'static str>,
    pub authors: &'static [Credit],
    pub components: &'static [Component],
}

/// The application's About data.
pub const fn info() -> AboutInfo {
    AboutInfo {
        display_name: "Kourier",
        component_name: "kourier",
        // Every crate takes its version from the workspace, so this is the workspace's.
        version: env!("CARGO_PKG_VERSION"),
        summary: "Unofficial messenger compatible with Signal",
        license: "AGPL-3.0-only",
        copyright: "© 2026 The Kourier developers",
        homepage: None,
        bug_report: None,
        translate: None,
        authors: &[],
        components: &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The workspace's own `version`, read from the manifest at the root.
    fn workspace_version() -> String {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
        let text = std::fs::read_to_string(manifest).unwrap();
        let table: toml::Table = toml::from_str(&text).unwrap();
        table["workspace"]["package"]["version"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn the_version_is_the_workspaces() {
        assert_eq!(info().version, workspace_version());
        assert_eq!(info().version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn nothing_is_claimed_that_does_not_exist_yet() {
        let about = info();
        assert_eq!(about.license, "AGPL-3.0-only");
        assert_eq!(about.display_name, "Kourier");
        assert_eq!(about.summary, "Unofficial messenger compatible with Signal");
        assert!(about.copyright.contains("The Kourier developers"));
        // The project has no site, so no link; no author is named; no notices are compiled in.
        assert_eq!(about.homepage, None);
        assert_eq!(about.bug_report, None);
        assert_eq!(about.translate, None);
        assert!(about.authors.is_empty());
        assert!(about.components.is_empty());
    }
}
