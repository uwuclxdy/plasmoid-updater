// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashSet;
use std::fmt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Returns the user's data directory, respecting XDG_DATA_HOME.
pub(crate) fn data_home() -> PathBuf {
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| user_home().join(".local/share"))
}

/// Returns the user's cache directory, respecting XDG_CACHE_HOME.
pub(crate) fn cache_home() -> PathBuf {
    std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| user_home().join(".cache"))
}

/// Returns the user's configuration directory, respecting XDG_CONFIG_HOME.
pub(crate) fn config_home() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| user_home().join(".config"))
}

/// Returns the directories a user-local component may live in.
///
/// `user_home` is included because legacy icon themes install under `~/.icons`,
/// which the XDG data directories do not cover.
pub(crate) fn user_data_roots() -> Vec<PathBuf> {
    vec![data_home(), config_home(), user_home().to_path_buf()]
}

/// The system data roots `XDG_DATA_DIRS` names, and those of them the trust
/// check passed when the list was first asked for.
struct SystemDataDirs {
    candidates: Vec<PathBuf>,
    trusted: Vec<PathBuf>,
}

static SYSTEM_DATA_DIRS: OnceLock<SystemDataDirs> = OnceLock::new();

/// Returns the system-wide data directories a system component may come from,
/// respecting XDG_DATA_DIRS.
///
/// The XDG base directory spec's default applies when the variable is unset or
/// empty. Only directories the user cannot write are kept: `XDG_DATA_DIRS` is
/// the user's own environment, and a per-user tree on it (a package manager's
/// export directory) holds registry files anything running as the user can
/// rewrite.
///
/// A user run (`system == false`) gets an empty list without reading the
/// environment or the filesystem: user components never come from these
/// directories. A `--system` run resolves the list once per process; every
/// consumer still judges each path under it before use, since a directory can
/// turn untrusted later in the run.
pub(crate) fn system_data_dirs(system: bool) -> &'static [PathBuf] {
    if !system {
        return &[];
    }
    &system_dirs().trusted
}

/// Returns every system data root `XDG_DATA_DIRS` names, judged by nothing:
/// a caller that must see a root's trust change within the process asks the
/// trust check about each one itself.
pub(crate) fn system_data_candidates(system: bool) -> &'static [PathBuf] {
    if !system {
        return &[];
    }
    &system_dirs().candidates
}

fn system_dirs() -> &'static SystemDataDirs {
    SYSTEM_DATA_DIRS.get_or_init(|| {
        let raw = std::env::var("XDG_DATA_DIRS")
            .ok()
            .filter(|dirs| !dirs.is_empty())
            .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
        system_dirs_from(&raw, system_path_trust)
    })
}

/// Splits an XDG_DATA_DIRS value into its directories and keeps apart the
/// ones that may hold system data; a refused directory is left out of those,
/// with a warning.
fn system_dirs_from(raw: &str, trust: fn(&Path) -> Trust) -> SystemDataDirs {
    let candidates: Vec<PathBuf> = raw
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .collect();
    let trusted = candidates
        .iter()
        .filter(|dir| matches!(trust(dir), Trust::Trusted))
        .cloned()
        .collect();
    SystemDataDirs {
        candidates,
        trusted,
    }
}

/// What a failed stat or open proves about the path it named.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Absent {
    /// Nothing is there: the path does not exist, or one of its components
    /// is not a directory.
    Gone,
    /// The path is too long for a path-based call to reach, so nothing any
    /// reader or writer of this run looks for by it is there for them.
    Unreachable,
}

/// The one rule for what a failed stat or open of a path proves: `NotFound`
/// and `ENOTDIR` that nothing is there, `ENAMETOOLONG` that nothing there can
/// be reached by that path. Any other failure proves nothing (`None`): the
/// path may hold what was looked for.
pub(crate) fn absence(error: &std::io::Error) -> Option<Absent> {
    if matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    ) {
        Some(Absent::Gone)
    } else if error.raw_os_error() == Some(nix::libc::ENAMETOOLONG) {
        Some(Absent::Unreachable)
    } else {
        None
    }
}

/// Returns whether ownership `uid` and permission bits `mode` make a path
/// trustworthy as system data: root owns it and group or others cannot write.
pub(crate) fn trusted_owner(uid: u32, mode: u32) -> bool {
    uid == 0 && mode & 0o022 == 0
}

/// What a trust check says about a path.
pub(crate) enum Trust {
    /// Every component is a real, root-owned file or directory that group or
    /// others cannot write.
    Trusted,
    /// The check decided against the path: it is absent, a symlink, not
    /// root's, or writable by group or others.
    Refused,
    /// A component could not be looked at, so nothing is known about the path:
    /// what sits under it, a registry file included, cannot be ruled out.
    Unreadable {
        /// The component the stat failed on.
        component: PathBuf,
        /// That failure.
        error: std::io::Error,
    },
}

/// Why a trust check could not decide a path: the component it could not look
/// at and that failure, as the trust warning names them.
#[expect(
    clippy::unnecessary_debug_formatting,
    reason = "the failing component is quoted as data, as the trust warning prints it"
)]
pub(crate) fn cannot_check(component: &Path, error: &std::io::Error) -> String {
    format!("cannot check {component:?} ({error})")
}

/// Returns what the trust check says about `path`: an absolute path whose
/// every component is a real, root-owned file or directory that group or
/// others cannot write, or why it is not.
///
/// Every component is judged, not only the last: a user-owned symlink partway
/// down would otherwise land the lookup wherever the user points it, and the
/// real directory it resolves to would lend its trust to that path.
///
/// A component that exists and fails the check is named in a warning, once per
/// process for each distinct refusal; a path [`absence`] proves absent is
/// refused without a warning, since a directory missing from `XDG_DATA_DIRS`
/// is a normal configuration. A refusal in one ancestor decides the whole
/// path, even when a deeper component cannot be looked at: nothing under a
/// refused ancestor is trusted. Only a path whose every ancestor is trusted
/// and that holds a component the check could not look at is
/// [`Trust::Unreadable`], and it is warned about: the path is not refused, it
/// is unknown.
#[expect(
    clippy::unnecessary_debug_formatting,
    reason = "the failing component is quoted as data, as the trust warning prints it"
)]
pub(crate) fn system_path_trust(path: &Path) -> Trust {
    if !path.is_absolute() {
        warn_untrusted_once(path, format_args!("not an absolute path"));
        return Trust::Refused;
    }

    let mut unseen = None;
    for component in path.ancestors() {
        match std::fs::symlink_metadata(component) {
            Ok(metadata) => {
                let trusted = !metadata.file_type().is_symlink()
                    && trusted_owner(metadata.uid(), metadata.mode());
                if !trusted {
                    warn_untrusted_once(
                        path,
                        format_args!(
                            "{component:?} is a symlink, not owned by root, or writable by group or others"
                        ),
                    );
                    return Trust::Refused;
                }
            }
            Err(e) if absence(&e).is_some() => return Trust::Refused,
            Err(e) => {
                if unseen.is_none() {
                    unseen = Some((component.to_path_buf(), e));
                }
            }
        }
    }
    if let Some((component, error)) = unseen {
        let why = cannot_check(&component, &error);
        warn_untrusted_once(path, format_args!("{why}"));
        return Trust::Unreadable { component, error };
    }
    Trust::Trusted
}

/// Returns whether [`system_path_trust`] trusts `path`.
pub(crate) fn is_trusted_system_path(path: &Path) -> bool {
    matches!(system_path_trust(path), Trust::Trusted)
}

static WARNED_REFUSALS: OnceLock<Mutex<HashSet<(PathBuf, String)>>> = OnceLock::new();

/// Logs each distinct refusal of `path` once per process: callers judge the
/// same path on every use, and a repeat line would bury the first, while a new
/// reason for the same path is still news.
fn warn_untrusted_once(path: &Path, detail: fmt::Arguments<'_>) {
    let detail = detail.to_string();
    let warned = WARNED_REFUSALS.get_or_init(|| Mutex::new(HashSet::new()));
    let first = warned
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert((path.to_path_buf(), detail.clone()));
    if first {
        log::warn!(target: "paths", "ignoring {path:?}: {detail}");
    }
}

/// Returns the KNewStuff3 registry directory.
pub(crate) fn knewstuff_dir() -> PathBuf {
    data_home().join("knewstuff3")
}

/// Returns true if KDE Plasma is detected on this system.
///
/// Checks for the presence of the KNewStuff3 registry directory, which is
/// created when KDE Plasma runs for the first time. This works over SSH and
/// in headless environments where session environment variables are not set.
pub(crate) fn is_kde() -> bool {
    knewstuff_dir().exists()
}

static USER_HOME: OnceLock<PathBuf> = OnceLock::new();

/// Gets the user's home directory, even when running with sudo.
/// Cached after first call via `OnceLock`.
pub(crate) fn user_home() -> &'static Path {
    USER_HOME.get_or_init(resolve_user_home)
}

fn resolve_user_home() -> PathBuf {
    if let Ok(sudo_home) = std::env::var("SUDO_USER_HOME") {
        return PathBuf::from(sudo_home);
    }

    if let Ok(sudo_user) = std::env::var("SUDO_USER") {
        if let Ok(output) = std::process::Command::new("getent")
            .args(["passwd", &sudo_user])
            .output()
            && let Ok(line) = String::from_utf8(output.stdout)
            && let Some(home) = line.split(':').nth(5)
        {
            return PathBuf::from(home);
        }
        return PathBuf::from(format!("/home/{}", sudo_user));
    }

    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir().unwrap_or_else(|| {
                log::warn!(
                    target: "paths",
                    "could not determine home directory; \
                     defaulting to {}",
                    fallback_home().display()
                );
                fallback_home()
            })
        })
}

/// The home directory used when neither `HOME` nor a passwd entry resolves.
///
/// The name deliberately does not start with `plasmoid-updater-`: the download
/// sweep removes every directory of that shape under the temp dir, and a home
/// directory is the one thing it must never take for a dead download.
pub(crate) fn fallback_home() -> PathBuf {
    PathBuf::from("/tmp/.plasmoid-updater-fallback-home")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::logs::captured_logs_while;

    #[test]
    fn user_home_returns_consistent_path() {
        let first = user_home();
        let second = user_home();
        assert_eq!(first, second);
        assert!(
            std::ptr::eq(first, second),
            "should return same &'static ref"
        );
    }

    #[test]
    fn data_home_is_under_user_home_or_xdg() {
        let dh = data_home();
        if std::env::var("XDG_DATA_HOME").is_ok() {
            // XDG override — just check it's a PathBuf
            assert!(!dh.as_os_str().is_empty());
        } else {
            assert!(dh.starts_with(user_home()));
        }
    }

    #[test]
    fn cache_home_is_under_user_home_or_xdg() {
        let ch = cache_home();
        if std::env::var("XDG_CACHE_HOME").is_ok() {
            assert!(!ch.as_os_str().is_empty());
        } else {
            assert!(ch.starts_with(user_home()));
        }
    }

    #[test]
    fn a_user_run_gets_no_system_data_dirs() {
        assert!(system_data_dirs(false).is_empty());
    }

    #[test]
    fn trusted_owner_accepts_only_root_owned_unwritable_paths() {
        assert!(trusted_owner(0, 0o755));
        assert!(trusted_owner(0, 0o644));
        assert!(!trusted_owner(0, 0o775), "group-writable");
        assert!(!trusted_owner(0, 0o757), "other-writable");
        assert!(!trusted_owner(0, 0o777), "world-writable");
        assert!(!trusted_owner(1000, 0o755), "the user's own directory");
    }

    #[test]
    fn a_user_owned_path_is_not_a_trusted_system_path() {
        if nix::unistd::Uid::effective().is_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();

        assert!(!is_trusted_system_path(dir.path()));

        assert!(!is_trusted_system_path(&dir.path().join("missing")));
    }

    #[test]
    fn an_untrusted_path_is_warned_about_once_per_process() {
        if nix::unistd::Uid::effective().is_root() {
            return;
        }
        let repeated = tempfile::tempdir().unwrap();
        let distinct = tempfile::tempdir().unwrap();

        let lines = captured_logs_while(|| {
            assert!(!is_trusted_system_path(repeated.path()));
            assert!(!is_trusted_system_path(repeated.path()));
            assert!(!is_trusted_system_path(distinct.path()));
        });

        let warnings = |path: &Path| {
            lines
                .iter()
                .filter(|line| {
                    line.starts_with("paths: ignoring ") && line.contains(&format!("{path:?}"))
                })
                .count()
        };
        assert_eq!(warnings(repeated.path()), 1, "{lines:?}");
        assert_eq!(warnings(distinct.path()), 1, "{lines:?}");
    }

    #[test]
    fn an_unreadable_or_relative_path_is_warned_about_once_per_process() {
        use std::os::unix::fs::PermissionsExt;

        if nix::unistd::Uid::effective().is_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let unreadable = locked.join("knewstuff3");
        let relative = PathBuf::from(dir.path().file_name().unwrap());

        let lines = captured_logs_while(|| {
            for _ in 0..2 {
                assert!(!is_trusted_system_path(&unreadable));
                assert!(!is_trusted_system_path(&relative));
            }
        });
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();

        let warnings = |path: &Path, reason: &str| {
            lines
                .iter()
                .filter(|line| {
                    line.starts_with(&format!("paths: ignoring {path:?}: "))
                        && line.contains(reason)
                })
                .count()
        };
        assert_eq!(
            warnings(
                &unreadable,
                "is a symlink, not owned by root, or writable by group or others"
            ),
            1,
            "{lines:?}"
        );
        assert_eq!(warnings(&relative, "not an absolute path"), 1, "{lines:?}");
    }

    /// A refusal in an ancestor decides the whole path even where a deeper
    /// component cannot be looked at: nothing under a refused ancestor is
    /// trusted, and the refusal is what the warning names.
    #[test]
    fn a_refused_ancestor_decides_even_where_a_component_is_unseen() {
        use std::os::unix::fs::PermissionsExt;

        if nix::unistd::Uid::effective().is_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let unseen = locked.join("knewstuff3");

        let verdict = system_path_trust(&unseen);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(matches!(verdict, Trust::Refused));
        assert!(matches!(
            system_path_trust(&dir.path().join("missing")),
            Trust::Refused
        ));
    }

    /// A path whose every ancestor is trusted and whose own stat fails is
    /// warned about with what could not be checked, not with the path a second
    /// time.
    #[test]
    fn an_undecidable_path_names_what_could_not_be_checked() {
        // A trusted ancestor is root's alone, so a non-root run's only
        // undecidable path lies below one: the kernel keeps process 1's fd
        // directory for root.
        if nix::unistd::Uid::effective().is_root() {
            return;
        }
        // The state exists only where every ancestor is trusted: a user
        // namespace shows the host's root as an unmapped uid, and the check
        // then refuses the ancestor instead.
        if !is_trusted_system_path(Path::new("/proc/1/fd")) {
            eprintln!("skipped: /proc/1/fd is not a trusted ancestor on this host");
            return;
        }
        let undecidable = Path::new("/proc/1/fd/0");
        let Err(unseen) = std::fs::symlink_metadata(undecidable) else {
            eprintln!(
                "skipped: this host lets the run stat {}",
                undecidable.display()
            );
            return;
        };
        if unseen.kind() != std::io::ErrorKind::PermissionDenied {
            eprintln!(
                "skipped: {} fails with {unseen}, not a permission denial",
                undecidable.display()
            );
            return;
        }

        let lines = captured_logs_while(|| {
            assert!(matches!(
                system_path_trust(undecidable),
                Trust::Unreadable { .. }
            ));
        });
        let expected = format!(
            "paths: ignoring {undecidable:?}: cannot check {undecidable:?} (Permission denied (os error 13))"
        );
        assert!(lines.iter().any(|line| line == &expected), "{lines:?}");
    }

    /// The trust warning's detail names what could not be checked, in
    /// parentheses after it.
    #[test]
    fn the_unreadable_detail_names_the_component_and_error() {
        let error = std::io::Error::from_raw_os_error(nix::libc::EACCES);
        assert_eq!(
            cannot_check(Path::new("/r/secret/share"), &error),
            "cannot check \"/r/secret/share\" (Permission denied (os error 13))"
        );
    }

    /// A path through a regular file, or one holding a component too long to
    /// name, cannot exist: the check refuses it as it refuses a missing path,
    /// at once and with no warning, whatever its ancestors are.
    #[test]
    fn a_path_that_cannot_exist_is_refused_without_a_warning() {
        if nix::unistd::Uid::effective().is_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, b"").unwrap();
        let through_file = file.join("knewstuff3");
        let too_long = dir.path().join("n".repeat(300)).join("knewstuff3");

        let lines = captured_logs_while(|| {
            assert!(matches!(system_path_trust(&through_file), Trust::Refused));
            assert!(matches!(system_path_trust(&too_long), Trust::Refused));
        });

        let named: Vec<&String> = lines
            .iter()
            .filter(|line| {
                line.contains(&format!("{through_file:?}"))
                    || line.contains(&format!("{too_long:?}"))
            })
            .collect();
        assert_eq!(named, Vec::<&String>::new());
    }

    /// The absence rule every stat and open of the record scan, the registry
    /// read and the trust check shares.
    #[test]
    fn only_a_missing_path_a_file_in_the_way_or_a_too_long_name_proves_absence() {
        let error = std::io::Error::from_raw_os_error;
        assert_eq!(absence(&error(nix::libc::ENOENT)), Some(Absent::Gone));
        assert_eq!(absence(&error(nix::libc::ENOTDIR)), Some(Absent::Gone));
        assert_eq!(
            absence(&error(nix::libc::ENAMETOOLONG)),
            Some(Absent::Unreachable)
        );
        for unknown in [
            nix::libc::EACCES,
            nix::libc::EIO,
            nix::libc::ELOOP,
            nix::libc::EISDIR,
        ] {
            assert_eq!(absence(&error(unknown)), None, "errno {unknown}");
        }
    }

    #[test]
    fn a_user_owned_symlink_to_a_root_owned_directory_is_not_trusted() {
        if nix::unistd::Uid::effective().is_root() {
            return;
        }
        let target = PathBuf::from("/usr");
        let through_target = target.join("share");
        if !is_trusted_system_path(&target) || !through_target.is_dir() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("usr-link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(
            !is_trusted_system_path(&link),
            "a user-owned symlink to {:?} was trusted",
            target
        );
        // The ancestor case proves the symlink rule only where the real
        // directory behind the link is itself trusted.
        if !is_trusted_system_path(&through_target) {
            return;
        }
        assert!(
            !is_trusted_system_path(&link.join("share")),
            "a path whose ancestor is a user-owned symlink was trusted"
        );
    }

    #[test]
    fn a_user_owned_data_directory_is_dropped_and_a_root_owned_one_kept() {
        if nix::unistd::Uid::effective().is_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let user_owned = std::fs::canonicalize(dir.path()).unwrap();
        let root_owned = PathBuf::from("/usr/share");
        if !is_trusted_system_path(&root_owned) {
            return;
        }

        let roots = system_dirs_from(
            &format!("{}:{}", user_owned.display(), root_owned.display()),
            system_path_trust,
        )
        .trusted;

        assert_eq!(roots, vec![root_owned]);
    }
}
