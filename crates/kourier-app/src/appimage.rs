//! Running from an AppImage (architecture §14.1): the one file `cargo xtask dist` also makes,
//! which holds the program with the Qt, KDE Frameworks and other libraries it was built with. Its
//! runtime mounts the file (or, with `APPIMAGE_EXTRACT_AND_RUN=1` or `--appimage-extract-and-run`,
//! unpacks it into a temporary folder) and starts the program inside (`AppRun` is a link to
//! `usr/bin/kourier`), telling it in [`RUNTIME_ENV`] where it runs.
//!
//! What changes in such a run: Qt's platform theme is the portal's unless the person chose one
//! (`kourier_qt::prepare_environment`: the AppImage holds no KDE platform theme, and a theme of the
//! system's would need the system's Qt), the translations come from the AppImage
//! (`kourier_qt::l10n`), the autostart entry starts the AppImage's file and not the program in a
//! mount that is gone after the run (`desktop::autostart_target`), and a program Kourier starts for
//! the person gets none of [`RUNTIME_ENV`] (`process::child_env_drop`). Restart Now keeps them: the
//! new process is the same program in the same mount.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// What the AppImage runtime sets for the program it starts: the AppImage's file, its mount point
/// (or the folder it was unpacked into), the working folder the AppImage was started from and the
/// name it was started by.
pub const RUNTIME_ENV: [&str; 4] = ["APPIMAGE", "APPDIR", "OWD", "ARGV0"];

/// The AppImage this process runs from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppImage {
    /// The AppImage's file (`APPIMAGE`).
    pub file: PathBuf,
    /// Where its runtime mounted or unpacked it (`APPDIR`).
    pub dir: PathBuf,
}

/// The AppImage `exe` (the path of this program) runs from, as `env` says: `APPIMAGE` and `APPDIR`
/// are absolute paths and `exe` lies inside `APPDIR`. The variables alone are not enough: a plain
/// Kourier started by an AppImage (a launcher packed as one) inherits that AppImage's.
pub fn of(exe: &Path, env: impl Fn(&str) -> Option<OsString>) -> Option<AppImage> {
    let file = PathBuf::from(env("APPIMAGE")?);
    let dir = PathBuf::from(env("APPDIR")?);
    let inside = dir.parent().is_some() && exe.starts_with(&dir) && exe != dir;
    (file.is_absolute() && dir.is_absolute() && inside).then_some(AppImage { file, dir })
}

/// [`of`] this process (`/proc/self/exe` and its environment).
pub fn current() -> Option<AppImage> {
    let exe = std::env::current_exe().ok()?;
    of(&exe, |name| std::env::var_os(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| OsString::from(v))
        }
    }

    /// The mounted AppImage and the unpacked one (`--appimage-extract-and-run`) are both found.
    #[test]
    fn a_program_inside_appdir_runs_from_the_appimage() {
        let mounted = [
            ("APPIMAGE", "/home/p/Apps/Kourier-x86_64.AppImage"),
            ("APPDIR", "/tmp/.mount_KourieAbC123"),
        ];
        assert_eq!(
            of(
                Path::new("/tmp/.mount_KourieAbC123/usr/bin/kourier"),
                env(&mounted)
            ),
            Some(AppImage {
                file: PathBuf::from("/home/p/Apps/Kourier-x86_64.AppImage"),
                dir: PathBuf::from("/tmp/.mount_KourieAbC123"),
            })
        );
        let unpacked = [
            ("APPIMAGE", "/home/p/Kourier.AppImage"),
            ("APPDIR", "/tmp/appimage_extracted_0123abcd"),
        ];
        assert!(
            of(
                Path::new("/tmp/appimage_extracted_0123abcd/usr/bin/kourier"),
                env(&unpacked)
            )
            .is_some()
        );
    }

    /// A plain Kourier with an AppImage's variables (inherited from a launcher packed as one),
    /// a mount point that only shares a prefix of its name, half the variables, relative paths
    /// or the root as the mount point: no AppImage.
    #[test]
    fn variables_that_do_not_hold_this_program_are_not_an_appimage() {
        let other = [
            ("APPIMAGE", "/home/p/Launcher.AppImage"),
            ("APPDIR", "/tmp/.mount_LauncXyz"),
        ];
        assert_eq!(of(Path::new("/usr/bin/kourier"), env(&other)), None);
        let prefix = [
            ("APPIMAGE", "/home/p/Kourier.AppImage"),
            ("APPDIR", "/tmp/.mount_Kour"),
        ];
        assert_eq!(
            of(
                Path::new("/tmp/.mount_Kourie1/usr/bin/kourier"),
                env(&prefix)
            ),
            None
        );
        let exe = Path::new("/tmp/.mount_K/usr/bin/kourier");
        assert_eq!(of(exe, env(&[("APPDIR", "/tmp/.mount_K")])), None);
        assert_eq!(of(exe, env(&[("APPIMAGE", "/home/p/K.AppImage")])), None);
        assert_eq!(
            of(
                exe,
                env(&[("APPIMAGE", "K.AppImage"), ("APPDIR", "/tmp/.mount_K")])
            ),
            None
        );
        assert_eq!(
            of(
                Path::new("tmp/.mount_K/usr/bin/kourier"),
                env(&[
                    ("APPIMAGE", "/home/p/K.AppImage"),
                    ("APPDIR", "tmp/.mount_K")
                ])
            ),
            None
        );
        assert_eq!(
            of(
                exe,
                env(&[("APPIMAGE", "/home/p/K.AppImage"), ("APPDIR", "/")])
            ),
            None
        );
    }
}
