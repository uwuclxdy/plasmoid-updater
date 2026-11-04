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

static SYSTEM_DATA_DIRS: OnceLock<Vec<PathBuf>> = OnceLock::new();

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
/// directories. A `--system` run resolves the candidate list once per process;
/// every consumer still judges each path under it before use, since a directory
/// can turn untrusted later in the run.
pub(crate) fn system_data_dirs(system: bool) -> &'static [PathBuf] {
    if !system {
        return &[];
    }
    SYSTEM_DATA_DIRS.get_or_init(|| {
        let raw = std::env::var("XDG_DATA_DIRS")
            .ok()
            .filter(|dirs| !dirs.is_empty())
            .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
        trusted_system_dirs(&raw)
    })
}

/// Splits an XDG_DATA_DIRS value into the directories that may hold system
/// data: the user-owned ones are dropped, each with a warning.
fn trusted_system_dirs(raw: &str) -> Vec<PathBuf> {
    raw.split(':')
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .filter(|dir| is_trusted_system_path(dir))
        .collect()
}

/// Returns whether ownership `uid` and permission bits `mode` make a path
/// trustworthy as system data: root owns it and group or others cannot write.
pub(crate) fn trusted_owner(uid: u32, mode: u32) -> bool {
    uid == 0 && mode & 0o022 == 0
}

/// Returns whether `path` is an absolute path whose every component is a real,
/// root-owned file or directory that group or others cannot write.
///
/// Every component is judged, not only the last: a user-owned symlink partway
/// down would otherwise land the lookup wherever the user points it, and the
/// real directory it resolves to would lend its trust to that path.
///
/// A component that exists and fails the check is named in a warning, once per
/// process for each distinct refusal; a path that does not exist is not trusted
/// and not warned about, since a directory missing from `XDG_DATA_DIRS` is a
/// normal configuration.
pub(crate) fn is_trusted_system_path(path: &Path) -> bool {
    if !path.is_absolute() {
        warn_untrusted_once(path, format_args!("not an absolute path"));
        return false;
    }

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
                    return false;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
            Err(e) => {
                warn_untrusted_once(path, format_args!("{component:?}: {e}"));
                return false;
            }
        }
    }
    true
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
        assert_eq!(warnings(&unreadable, "Permission denied"), 1, "{lines:?}");
        assert_eq!(warnings(&relative, "not an absolute path"), 1, "{lines:?}");
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

        let roots = trusted_system_dirs(&format!(
            "{}:{}",
            user_owned.display(),
            root_owned.display()
        ));

        assert_eq!(roots, vec![root_owned]);
    }
}
