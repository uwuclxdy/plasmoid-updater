// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{path::Path, process::Command};

use crate::{Error, Result};

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

fn run_sudo(args: &[&str]) -> Result<()> {
    let output = Command::new("sudo")
        .args(args)
        .output()
        .map_err(|e| Error::install(format!("failed to run sudo: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::install(format!(
            "sudo command failed: {}",
            stderr.trim()
        )));
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
pub(crate) fn copy_file(src: &Path, dest: &Path) -> Result<()> {
    if needs_sudo(dest) {
        run_sudo(&["cp", "-f", &src.to_string_lossy(), &dest.to_string_lossy()])
    } else {
        std::fs::copy(src, dest)?;
        Ok(())
    }
}

/// Recursively copies a directory, using sudo if the destination requires it.
pub(crate) fn copy_dir(src: &Path, dest: &Path) -> Result<()> {
    if needs_sudo(dest) {
        run_sudo(&["cp", "-rf", &src.to_string_lossy(), &dest.to_string_lossy()])
    } else {
        super::backup::copy_dir_recursive(src, dest)
    }
}

/// Creates directories recursively, using sudo if the path requires it.
pub(crate) fn create_dir_all(path: &Path) -> Result<()> {
    if needs_sudo(path) {
        run_sudo(&["mkdir", "-p", &path.to_string_lossy()])
    } else {
        std::fs::create_dir_all(path)?;
        Ok(())
    }
}

/// Removes a file, using sudo if the path requires it.
pub(crate) fn remove_file(path: &Path) -> Result<()> {
    if needs_sudo(path) {
        run_sudo(&["rm", "-f", &path.to_string_lossy()])
    } else {
        std::fs::remove_file(path)?;
        Ok(())
    }
}

/// Removes a directory recursively, using sudo if the path requires it.
pub(crate) fn remove_dir_all(path: &Path) -> Result<()> {
    if needs_sudo(path) {
        run_sudo(&["rm", "-rf", &path.to_string_lossy()])
    } else {
        std::fs::remove_dir_all(path)?;
        Ok(())
    }
}

/// Writes content to a file, using sudo tee if the path requires it.
pub(crate) fn write_file(path: &Path, content: &[u8]) -> Result<()> {
    if needs_sudo(path) {
        use std::io::Write;
        let mut child = Command::new("sudo")
            .args(["tee", &path.to_string_lossy()])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .map_err(|e| Error::install(format!("failed to run sudo tee: {e}")))?;

        child
            .stdin
            .as_mut()
            .expect("stdin was piped")
            .write_all(content)?;

        let status = child.wait()?;
        if !status.success() {
            return Err(Error::install("sudo tee failed"));
        }
        Ok(())
    } else {
        std::fs::write(path, content)?;
        Ok(())
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
