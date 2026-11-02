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
        // `--no-target-directory` refuses to nest `src` inside an existing
        // directory, the way `fs::rename` does.
        run_sudo(&[
            "mv",
            "--no-target-directory",
            &src.to_string_lossy(),
            &dest.to_string_lossy(),
        ])
    } else {
        std::fs::rename(src, dest)
            .map_err(|e| ComponentFailure::io(FailureKind::Install, dest, e))?;
        Ok(())
    }
}

/// Exchanges two paths in one atomic step (`renameat2` with `RENAME_EXCHANGE`).
///
/// Returns `Ok(false)` where the exchange is unavailable — a filesystem that
/// refuses it, a kernel without the call, or a non-glibc target — so the
/// caller keeps its two-rename fallback.
pub(crate) fn exchange(a: &Path, b: &Path) -> Result<bool, ComponentFailure> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        use nix::errno::Errno;
        use nix::fcntl::{AT_FDCWD, RenameFlags, renameat2};

        match renameat2(AT_FDCWD, a, AT_FDCWD, b, RenameFlags::RENAME_EXCHANGE) {
            Ok(()) => Ok(true),
            Err(Errno::EINVAL | Errno::ENOSYS | Errno::EOPNOTSUPP) => Ok(false),
            Err(e) => Err(ComponentFailure::io(
                FailureKind::Install,
                b,
                std::io::Error::from_raw_os_error(e as i32),
            )),
        }
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        let _ = (a, b);
        Ok(false)
    }
}

/// Whether the `mv` this process runs lists `--exchange` (GNU coreutils 9.5+).
///
/// Probed at most once per process: the answer cannot change mid-run, and a
/// per-component probe would spawn `mv` once per component.
pub(crate) fn mv_supports_exchange() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        Command::new("mv")
            .arg("--help")
            .output()
            .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains("--exchange"))
    })
}

/// Exchanges two paths in one step through `sudo mv --exchange`, so a system
/// path has no moment where either is absent.
///
/// Returns `Ok(false)` where `mv` itself refused the exchange — a filesystem
/// without `RENAME_EXCHANGE`, a destination on another filesystem, or an `mv`
/// from `sudo`'s `secure_path` that does not know the flag — so the caller
/// keeps its two-rename fallback. Any other failure is sudo's own, such as an
/// authentication it did not get, and stays an error: falling back would only
/// prompt again for a privilege this run does not have.
pub(crate) fn sudo_exchange(a: &Path, b: &Path) -> Result<bool, ComponentFailure> {
    // The refusals below are matched by their English phrases, and `mv` prints
    // them in the locale it inherits. `LC_ALL` goes as an argument to `env`
    // rather than to sudo, whose environment policy would strip it.
    let output = Command::new("sudo")
        .args([
            "env",
            "LC_ALL=C",
            "mv",
            "--exchange",
            "--no-target-directory",
            &a.to_string_lossy(),
            &b.to_string_lossy(),
        ])
        .output()
        .map_err(|e| {
            ComponentFailure::new(FailureKind::Install, format!("failed to run sudo mv: {e}"))
        })?;

    if output.status.success() {
        return Ok(true);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    if refused_exchange(detail) {
        log::debug!(
            target: "install",
            "sudo mv --exchange failed for {} ({}): {}",
            b.display(),
            output.status,
            detail,
        );
        return Ok(false);
    }
    Err(ComponentFailure::new(
        FailureKind::Install,
        if detail.is_empty() {
            format!(
                "sudo mv --exchange failed for {} ({})",
                b.display(),
                output.status
            )
        } else {
            format!(
                "sudo mv --exchange failed for {} ({}): {detail}",
                b.display(),
                output.status
            )
        },
    ))
}

/// The phrases a failing `mv --exchange` prints when the exchange itself is
/// what it cannot do, as opposed to sudo refusing to run it at all.
///
/// Measured 2026-11-02 with coreutils 9.12, by forcing `renameat2` to fail:
/// EINVAL, EOPNOTSUPP and ENOSYS all print `Operation not supported` (the
/// wrapper reports each of the three as ENOTSUP), a real cross-device pair
/// prints `Invalid cross-device link`, and EPERM, EACCES and ENOENT keep text
/// that matches none of these. `Invalid argument` and `Function not
/// implemented` are the raw texts of EINVAL and ENOSYS, for an `mv` that
/// reports them as they are. The option words are the `mv` from before
/// coreutils 9.5, which has no exchange and rejects it the way `mv --bogus`
/// rejects an unknown option.
const EXCHANGE_REFUSALS: &[&str] = &[
    "Operation not supported",
    "Invalid cross-device link",
    "Invalid argument",
    "Function not implemented",
    "--exchange",
    "unrecognized option",
    "invalid option",
];

/// Whether the failing `mv`'s own output says it does not accept the exchange,
/// as opposed to sudo refusing to run it at all.
fn refused_exchange(stderr: &str) -> bool {
    EXCHANGE_REFUSALS
        .iter()
        .any(|phrase| stderr.contains(phrase))
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
    use std::ffi::OsString;

    use super::*;

    /// Builds a fake `sudo` that records its arguments and refuses, then runs
    /// this test again with that `sudo` first on `PATH`.
    #[test]
    fn sudo_mv_refuses_to_target_an_existing_directory() {
        if is_root() {
            return; // root takes the plain `fs::rename` branch
        }

        if std::env::var_os("PU_TEST_SUDO_LOG").is_some() {
            let src = std::env::var("PU_TEST_SUDO_SRC").unwrap();
            let dest = std::env::var("PU_TEST_SUDO_DEST").unwrap();
            let fake = std::env::var("PU_TEST_FAKE_SUDO").unwrap();
            let resolved = Command::new("sh")
                .args(["-c", "command -v sudo"])
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&resolved.stdout).trim(),
                fake,
                "a real sudo would run against a system path"
            );

            let _ = rename(Path::new(&src), Path::new(&dest));
            std::fs::write(std::env::var_os("PU_TEST_SUDO_DONE").unwrap(), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake_sudo = bin.join("sudo");
        std::fs::write(
            &fake_sudo,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$PU_TEST_SUDO_LOG\"\nexit 1\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let log = dir.path().join("sudo-args.log");
        let done = dir.path().join("child-done");
        let src = dir.path().join("tree");
        // Only ever passed to the fake sudo, which exits 1.
        let dest = Path::new("/usr/share/plasma/plasmoids/tree");
        let path = OsString::from(format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ));

        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "installer::privilege::tests::sudo_mv_refuses_to_target_an_existing_directory",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", path)
            .env("PU_TEST_SUDO_LOG", &log)
            .env("PU_TEST_SUDO_DONE", &done)
            .env("PU_TEST_SUDO_SRC", &src)
            .env("PU_TEST_SUDO_DEST", dest)
            .env("PU_TEST_FAKE_SUDO", &fake_sudo)
            .output()
            .unwrap();
        assert!(
            output.status.success() && done.exists(),
            "child run failed: {}\n--- stderr\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        );

        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            format!(
                "mv\n--no-target-directory\n{}\n{}\n",
                src.display(),
                dest.display()
            )
        );
    }

    /// Runs [`sudo_exchange`] against a recording fake `sudo` first on `PATH`,
    /// in a child process because the environment change needs a spawn. The
    /// fake refuses the way a filesystem without the exchange does.
    #[test]
    fn sudo_mv_exchange_is_one_step() {
        if std::env::var_os("PU_TEST_EXCHANGE_LOG").is_some() {
            let a = std::env::var("PU_TEST_EXCHANGE_A").unwrap();
            let b = std::env::var("PU_TEST_EXCHANGE_B").unwrap();
            let fake = std::env::var("PU_TEST_FAKE_SUDO").unwrap();
            let resolved = Command::new("sh")
                .args(["-c", "command -v sudo"])
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&resolved.stdout).trim(),
                fake,
                "a real sudo would run against a system path"
            );

            let refused = sudo_exchange(Path::new(&a), Path::new(&b));
            assert!(
                matches!(refused, Ok(false)),
                "a refused mv --exchange must not read as an exchange: {refused:?}"
            );
            std::fs::write(std::env::var_os("PU_TEST_EXCHANGE_DONE").unwrap(), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake_sudo = bin.join("sudo");
        std::fs::write(
            &fake_sudo,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$PU_TEST_EXCHANGE_LOG\"\n\
             printf \"mv: cannot exchange '.p-new' and 'dest-dir': Operation not supported\\n\" >&2\nexit 1\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let log = dir.path().join("sudo-args.log");
        let done = dir.path().join("child-done");
        let a = dir.path().join("new tree");
        // Only ever passed to the fake sudo, which exits 1.
        let b = Path::new("/usr/share/plasma/plasmoids/tree");
        let path = OsString::from(format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ));

        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "installer::privilege::tests::sudo_mv_exchange_is_one_step",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", path)
            .env("PU_TEST_EXCHANGE_LOG", &log)
            .env("PU_TEST_EXCHANGE_DONE", &done)
            .env("PU_TEST_EXCHANGE_A", &a)
            .env("PU_TEST_EXCHANGE_B", b)
            .env("PU_TEST_FAKE_SUDO", &fake_sudo)
            .output()
            .unwrap();
        assert!(
            output.status.success() && done.exists(),
            "child run failed: {}\n--- stderr\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        );

        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            format!(
                "env\nLC_ALL=C\nmv\n--exchange\n--no-target-directory\n{}\n{}\n",
                a.display(),
                b.display()
            )
        );
    }

    /// The stderr lines `mv --exchange` printed when `renameat2` was forced to
    /// fail, one per errno, beside the lines that are not `mv` refusing the
    /// exchange at all.
    #[test]
    fn only_mv_exchange_refusals_read_as_a_refusal() {
        for stderr in [
            // coreutils 9.12, EINVAL, EOPNOTSUPP and ENOSYS alike.
            "mv: cannot exchange '.p-new' and 'dest-dir': Operation not supported",
            // coreutils 9.12, EXDEV across a real device boundary.
            "mv: cannot exchange '.plasmoid-new' and '/usr/share/plasma': Invalid cross-device link",
            // The raw strerror texts for EINVAL and ENOSYS.
            "mv: cannot exchange '.p-new' and 'dest-dir': Invalid argument",
            "mv: cannot exchange '.p-new' and 'dest-dir': Function not implemented",
            // An `mv` with no `--exchange`, the way `mv --bogus` is rejected.
            "mv: unrecognized option '--exchange'",
        ] {
            assert!(refused_exchange(stderr), "{stderr}");
        }

        for stderr in [
            "sudo: a password is required",
            "mv: cannot exchange '.p-new' and 'dest-dir': Operation not permitted",
            "mv: cannot exchange '.p-new' and 'dest-dir': Permission denied",
            "mv: cannot exchange '.p-new' and 'dest-dir': No such file or directory",
            "mv: cannot stat 'nope': No such file or directory",
            "",
        ] {
            assert!(!refused_exchange(stderr), "{stderr}");
        }
    }

    const SUDO_REFUSAL_CHILD: &str =
        "installer::privilege::tests::a_sudo_refusal_is_an_error_not_an_exchange_refusal";

    /// Runs [`sudo_exchange`] against a fake `sudo` that refuses to run
    /// anything, the way an unanswered authentication prompt does.
    #[test]
    fn a_sudo_refusal_is_an_error_not_an_exchange_refusal() {
        if std::env::var_os("PU_TEST_REFUSAL_RESULT").is_some() {
            let a = std::env::var("PU_TEST_EXCHANGE_A").unwrap();
            let b = std::env::var("PU_TEST_EXCHANGE_B").unwrap();
            let resolved = Command::new("sh")
                .args(["-c", "command -v sudo"])
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&resolved.stdout).trim(),
                std::env::var("PU_TEST_FAKE_SUDO").unwrap(),
                "a real sudo would run against a system path"
            );
            let outcome = match sudo_exchange(Path::new(&a), Path::new(&b)) {
                Ok(true) => "exchanged".to_string(),
                Ok(false) => "fallback".to_string(),
                Err(e) => format!("error: {}", e.message),
            };
            std::fs::write(std::env::var_os("PU_TEST_REFUSAL_RESULT").unwrap(), outcome).unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake_sudo = bin.join("sudo");
        std::fs::write(
            &fake_sudo,
            "#!/bin/sh\nprintf 'sudo: a password is required\\n' >&2\nexit 1\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let result = dir.path().join("result");
        let a = dir.path().join("new tree");
        // Only ever passed to the fake sudo, which runs no command.
        let b = Path::new("/usr/share/plasma/plasmoids/tree");
        let path = OsString::from(format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ));

        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                SUDO_REFUSAL_CHILD,
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", path)
            .env("PU_TEST_REFUSAL_RESULT", &result)
            .env("PU_TEST_EXCHANGE_A", &a)
            .env("PU_TEST_EXCHANGE_B", b)
            .env("PU_TEST_FAKE_SUDO", &fake_sudo)
            .output()
            .unwrap();
        assert!(
            output.status.success() && result.exists(),
            "child run failed: {}\n--- stderr\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        );

        let outcome = std::fs::read_to_string(&result).unwrap();
        assert!(
            outcome.starts_with("error: "),
            "a sudo refusal read as a filesystem refusal: {outcome}"
        );
        assert!(outcome.contains("a password is required"), "{outcome}");
    }

    const LOCALE_CHILD: &str = "installer::privilege::tests::a_sudo_mv_is_run_in_the_c_locale";

    /// The refusal phrases are English, so the `mv` sudo runs must not inherit
    /// the desktop's locale. The fake `sudo` runs the command it was given, and
    /// the fake `mv` records the locale it ended up with.
    #[test]
    fn a_sudo_mv_is_run_in_the_c_locale() {
        if std::env::var_os("PU_TEST_LOCALE_RESULT").is_some() {
            let a = std::env::var("PU_TEST_EXCHANGE_A").unwrap();
            let b = std::env::var("PU_TEST_EXCHANGE_B").unwrap();
            let resolved = Command::new("sh")
                .args(["-c", "command -v sudo"])
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&resolved.stdout).trim(),
                std::env::var("PU_TEST_FAKE_SUDO").unwrap(),
                "a real sudo would run against a system path"
            );
            let exchanged = matches!(sudo_exchange(Path::new(&a), Path::new(&b)), Ok(true));
            std::fs::write(
                std::env::var_os("PU_TEST_LOCALE_RESULT").unwrap(),
                if exchanged { "exchanged" } else { "refused" },
            )
            .unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake_sudo = bin.join("sudo");
        std::fs::write(
            &fake_sudo,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$PU_TEST_SUDO_LOG\"\nexec \"$@\"\n",
        )
        .unwrap();
        let fake_mv = bin.join("mv");
        std::fs::write(
            &fake_mv,
            "#!/bin/sh\nprintf 'LC_ALL=%s\\nLANG=%s\\n' \"$LC_ALL\" \"$LANG\" > \"$PU_TEST_MV_ENV\"\nexit 0\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            for script in [&fake_sudo, &fake_mv] {
                std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }

        let log = dir.path().join("sudo-args.log");
        let mv_env = dir.path().join("mv-env.log");
        let result = dir.path().join("result");
        let a = dir.path().join("new tree");
        // Only ever passed to the fake sudo, which runs no real `mv`.
        let b = Path::new("/usr/share/plasma/plasmoids/tree");
        let path = OsString::from(format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ));

        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", LOCALE_CHILD, "--nocapture", "--test-threads=1"])
            .env_clear()
            .env("PATH", path)
            .env("LC_ALL", "de_DE.UTF-8")
            .env("LANG", "de_DE.UTF-8")
            .env("PU_TEST_SUDO_LOG", &log)
            .env("PU_TEST_MV_ENV", &mv_env)
            .env("PU_TEST_LOCALE_RESULT", &result)
            .env("PU_TEST_EXCHANGE_A", &a)
            .env("PU_TEST_EXCHANGE_B", b)
            .env("PU_TEST_FAKE_SUDO", &fake_sudo)
            .output()
            .unwrap();
        assert!(
            output.status.success() && result.exists(),
            "child run failed: {}\n--- stderr\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        );

        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            format!(
                "env\nLC_ALL=C\nmv\n--exchange\n--no-target-directory\n{}\n{}\n",
                a.display(),
                b.display()
            )
        );
        assert_eq!(
            std::fs::read_to_string(&mv_env).unwrap(),
            "LC_ALL=C\nLANG=de_DE.UTF-8\n",
            "the mv under sudo inherited the desktop's locale"
        );
        assert_eq!(std::fs::read_to_string(&result).unwrap(), "exchanged");
    }

    const MV_HELP_CHILD: &str =
        "installer::privilege::tests::the_exchange_probe_reads_the_mv_on_path";

    /// The probe answers from the `mv` its `PATH` resolves, not from whatever
    /// `mv` this box ships. Each shape runs in its own child: the answer is
    /// cached for the process, and a real `mv` on `PATH` would answer for the
    /// fake one.
    #[test]
    fn the_exchange_probe_reads_the_mv_on_path() {
        if let Ok(expected) = std::env::var("PU_TEST_MV_HELPS_WITH_EXCHANGE") {
            assert_eq!(mv_supports_exchange(), expected == "yes");
            return;
        }

        for (helps, help_text) in [
            ("yes", "  --exchange  exchange source and destination\\n"),
            ("no", "Usage: mv [OPTION]... SOURCE... DIRECTORY\\n"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let bin = dir.path().join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let fake_mv = bin.join("mv");
            std::fs::write(
                &fake_mv,
                format!("#!/bin/sh\nprintf '%s' '{help_text}'\nexit 0\n"),
            )
            .unwrap();
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&fake_mv, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let path = OsString::from(format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ));
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", MV_HELP_CHILD, "--nocapture", "--test-threads=1"])
                .env_clear()
                .env("PATH", path)
                .env("PU_TEST_MV_HELPS_WITH_EXCHANGE", helps)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "the probe disagreed with a fake mv that helps: {helps}\n--- stderr\n{}",
                String::from_utf8_lossy(&output.stderr),
            );
        }
    }

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
