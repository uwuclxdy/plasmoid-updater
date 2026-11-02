// SPDX-License-Identifier: GPL-3.0-or-later

use std::{os::unix::fs::PermissionsExt as _, path::Path, process::Command};

use super::ComponentFailure;
use crate::FailureKind;

/// Checks if the current process is running as root (UID 0).
pub(crate) fn is_root() -> bool {
    nix::unistd::Uid::effective().is_root()
}

/// Checks if a path is in a system directory (requires root to modify).
pub(crate) fn is_system_path(path: &Path) -> bool {
    path.starts_with("/usr") || path.starts_with("/lib") || path.starts_with("/etc")
}

/// Returns true if writing to `path` requires privilege escalation.
pub(crate) fn needs_sudo(path: &Path) -> bool {
    is_system_path(path) && !is_root()
}

// --- Privileged Command Execution ---

fn run_sudo(args: &[&str]) -> Result<(), ComponentFailure> {
    let output = Command::new("sudo").args(args).output().map_err(|e| {
        ComponentFailure::new(FailureKind::Install, format!("failed to run sudo: {e}"))
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ComponentFailure::new(
            FailureKind::Install,
            format!("sudo command failed: {}", stderr.trim()),
        ));
    }

    Ok(())
}

/// Creates a `Command` that is prefixed with sudo if not root.
pub(crate) fn sudo_command(program: &str) -> Command {
    if is_root() {
        Command::new(program)
    } else {
        let mut cmd = Command::new("sudo");
        cmd.arg(program);
        cmd
    }
}

// --- Privileged File Operations ---

/// Copies a file, using sudo if the destination requires it.
pub(crate) fn copy_file(src: &Path, dest: &Path) -> Result<(), ComponentFailure> {
    if needs_sudo(dest) {
        run_sudo(&["cp", "-f", &src.to_string_lossy(), &dest.to_string_lossy()])
    } else {
        std::fs::copy(src, dest)
            .map_err(|e| ComponentFailure::io(FailureKind::Install, dest, e))?;
        Ok(())
    }
}

/// Recursively copies a directory's contents, using sudo if the destination requires it.
///
/// Uses `cp -rfT` for sudo to avoid creating a nested subdirectory when the
/// destination already exists.
pub(crate) fn copy_dir(src: &Path, dest: &Path) -> Result<(), ComponentFailure> {
    if needs_sudo(dest) {
        run_sudo(&[
            "cp",
            "-rfT",
            &src.to_string_lossy(),
            &dest.to_string_lossy(),
        ])
    } else {
        super::backup::copy_dir_recursive(src, dest)
    }
}

/// Creates directories recursively, using sudo if the path requires it.
pub(crate) fn create_dir_all(path: &Path) -> Result<(), ComponentFailure> {
    if needs_sudo(path) {
        run_sudo(&["mkdir", "-p", &path.to_string_lossy()])
    } else {
        std::fs::create_dir_all(path)
            .map_err(|e| ComponentFailure::io(FailureKind::Install, path, e))?;
        Ok(())
    }
}

/// Removes a file, using sudo if the path requires it.
pub(crate) fn remove_file(path: &Path) -> Result<(), ComponentFailure> {
    if needs_sudo(path) {
        run_sudo(&["rm", "-f", &path.to_string_lossy()])
    } else {
        std::fs::remove_file(path)
            .map_err(|e| ComponentFailure::io(FailureKind::Install, path, e))?;
        Ok(())
    }
}

/// Removes a directory recursively, using sudo if the path requires it.
pub(crate) fn remove_dir_all(path: &Path) -> Result<(), ComponentFailure> {
    if needs_sudo(path) {
        run_sudo(&["rm", "-rf", &path.to_string_lossy()])
    } else {
        std::fs::remove_dir_all(path)
            .map_err(|e| ComponentFailure::io(FailureKind::Install, path, e))?;
        Ok(())
    }
}

/// Renames (moves) a file or directory atomically, using sudo mv if the destination requires it.
///
/// Both `src` and `dest` must be on the same filesystem for the rename to be atomic.
/// For user-local paths this is guaranteed when `src` is a sibling of `dest`.
pub(crate) fn rename(src: &Path, dest: &Path) -> Result<(), ComponentFailure> {
    if needs_sudo(dest) {
        run_sudo(&["mv", &src.to_string_lossy(), &dest.to_string_lossy()])
    } else {
        std::fs::rename(src, dest)
            .map_err(|e| ComponentFailure::io(FailureKind::Install, dest, e))?;
        Ok(())
    }
}

/// Writes content to a file, using sudo tee if the path requires it.
pub(crate) fn write_file(path: &Path, content: &[u8]) -> Result<(), ComponentFailure> {
    if needs_sudo(path) {
        write_via_sudo_tee(path, content)
    } else {
        std::fs::write(path, content)
            .map_err(|e| ComponentFailure::io(FailureKind::Install, path, e))?;
        Ok(())
    }
}

/// Writes content to a file as root, whatever the path looks like.
pub(crate) fn write_file_as_root(path: &Path, content: &[u8]) -> Result<(), ComponentFailure> {
    if is_root() {
        std::fs::write(path, content)
            .map_err(|e| ComponentFailure::io(FailureKind::Install, path, e))?;
        Ok(())
    } else {
        write_via_sudo_tee(path, content)
    }
}

/// Writes content through `sudo tee`, keeping it off the command line.
fn write_via_sudo_tee(path: &Path, content: &[u8]) -> Result<(), ComponentFailure> {
    use std::io::Write;
    let mut child = Command::new("sudo")
        .args(["tee", &path.to_string_lossy()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .map_err(|e| {
            ComponentFailure::new(FailureKind::Install, format!("failed to run sudo tee: {e}"))
        })?;

    #[expect(
        clippy::expect_used,
        reason = "stdin was spawned with Stdio::piped(), so the pipe handle always exists"
    )]
    let write_result = child
        .stdin
        .as_mut()
        .expect("stdin was piped")
        .write_all(content);

    // Reap the child even when the write failed: a nonzero exit (e.g. a refused
    // sudo) is a better error than the broken pipe the write sees.
    let status = child
        .wait()
        .map_err(|e| ComponentFailure::io(FailureKind::Install, path, e))?;
    if !status.success() {
        return Err(ComponentFailure::new(
            FailureKind::Install,
            "sudo tee failed",
        ));
    }
    write_result.map_err(|e| ComponentFailure::io(FailureKind::Install, path, e))?;
    Ok(())
}

/// Sets a file's permission bits as root, whatever the path looks like.
pub(crate) fn set_mode_as_root(path: &Path, mode: u32) -> Result<(), ComponentFailure> {
    if is_root() {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| ComponentFailure::io(FailureKind::Install, path, e))?;
        Ok(())
    } else {
        run_sudo(&["chmod", &format!("{mode:o}"), &path.to_string_lossy()])
    }
}

/// Replaces `dest` with `src` through a rename as root, whatever the path looks
/// like.
///
/// `--no-target-directory` makes `mv` replace the file at `dest` instead of
/// moving `src` inside a directory there, which is what a rename must do.
pub(crate) fn replace_file_as_root(src: &Path, dest: &Path) -> Result<(), ComponentFailure> {
    if is_root() {
        std::fs::rename(src, dest)
            .map_err(|e| ComponentFailure::io(FailureKind::Install, dest, e))?;
        Ok(())
    } else {
        run_sudo(&[
            "mv",
            "--no-target-directory",
            "--",
            &src.to_string_lossy(),
            &dest.to_string_lossy(),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_system_path() {
        assert!(is_system_path(Path::new(
            "/usr/share/plasma/plasmoids/test"
        )));
        assert!(is_system_path(Path::new("/usr/local/share/kwin/effects")));
        assert!(is_system_path(Path::new("/lib/kde/plasma")));
        assert!(is_system_path(Path::new("/etc/xdg/plasma")));
        assert!(!is_system_path(Path::new("/home/user/.local/share/plasma")));
        assert!(!is_system_path(Path::new("/tmp/test")));
    }

    #[test]
    fn test_needs_sudo() {
        if !is_root() {
            assert!(needs_sudo(Path::new("/usr/share/plasma/test")));
            assert!(!needs_sudo(Path::new("/home/user/.local/share/plasma")));
            assert!(!needs_sudo(Path::new("/tmp/test")));
        }
    }

    #[test]
    fn test_sudo_command() {
        let cmd = sudo_command("kpackagetool6");
        let program = cmd.get_program().to_string_lossy().to_string();
        if is_root() {
            assert_eq!(program, "kpackagetool6");
        } else {
            assert_eq!(program, "sudo");
        }
    }
    #[test]
    fn needs_sudo_for_system_paths_when_not_root() {
        if is_root() {
            return;
        }
        assert!(needs_sudo(Path::new("/usr/share/plasma/plasmoids/test")));
        assert!(needs_sudo(Path::new("/usr/local/share/kwin/effects")));
        assert!(needs_sudo(Path::new("/lib/kde/plasma")));
        assert!(needs_sudo(Path::new("/etc/xdg/something")));
    }

    #[test]
    fn no_sudo_for_user_paths() {
        assert!(!needs_sudo(Path::new("/home/user/.local/share/plasma")));
        assert!(!needs_sudo(Path::new("/tmp/test")));
    }

    #[test]
    fn sudo_command_wraps_when_not_root() {
        let cmd = sudo_command("kpackagetool6");
        let program = cmd.get_program().to_string_lossy().to_string();
        if is_root() {
            assert_eq!(program, "kpackagetool6");
        } else {
            assert_eq!(program, "sudo");
        }
    }

    #[test]
    fn copy_file_to_non_system_path() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dest = dir.path().join("dest.txt");

        std::fs::write(&src, b"test content").unwrap();

        copy_file(&src, &dest).unwrap();

        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "test content");
    }

    #[test]
    fn create_dir_all_non_system_path() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b/c");

        create_dir_all(&nested).unwrap();

        assert!(nested.is_dir());
    }

    #[test]
    fn remove_file_non_system_path() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("to_remove.txt");
        std::fs::write(&f, b"data").unwrap();

        remove_file(&f).unwrap();

        assert!(!f.exists());
    }

    #[test]
    fn remove_dir_all_non_system_path() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("file.txt"), b"data").unwrap();

        remove_dir_all(&dir.path().join("a")).unwrap();

        assert!(!dir.path().join("a").exists());
    }

    #[test]
    fn write_file_non_system_path() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("written.txt");

        write_file(&f, b"hello world").unwrap();

        assert_eq!(std::fs::read_to_string(&f).unwrap(), "hello world");
    }

    #[test]
    fn rename_non_system_path() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dest = dir.path().join("dest.txt");
        std::fs::write(&src, b"hello").unwrap();

        rename(&src, &dest).unwrap();

        assert!(!src.exists());
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "hello");
    }

    #[test]
    fn rename_replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("new.txt");
        let dest = dir.path().join("old.txt");
        std::fs::write(&src, b"new").unwrap();
        std::fs::write(&dest, b"old").unwrap();

        rename(&src, &dest).unwrap();

        assert!(!src.exists());
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new");
    }

    #[test]
    fn copy_dir_non_system_path() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src_dir");
        let dest = dir.path().join("dest_dir");

        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("file.txt"), b"content").unwrap();
        std::fs::create_dir_all(&dest).unwrap();

        copy_dir(&src, &dest).unwrap();

        assert_eq!(
            std::fs::read_to_string(dest.join("file.txt")).unwrap(),
            "content"
        );
    }
}
