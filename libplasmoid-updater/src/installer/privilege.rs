// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    ffi::OsStr,
    os::unix::{fs::PermissionsExt as _, process::CommandExt as _},
    path::Path,
    process::Command,
};

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

/// A `sudo` that never prompts.
///
/// Each spawn runs in a process group of its own, out of the terminal's
/// foreground group, so a `sudo` that stopped to ask for a credential would
/// stop itself with SIGTTOU and never be reaped (measured under a pty
/// 2026-11-24). `-n` fails the call at once instead, with sudo's own words in
/// the failure. Every `sudo` in the crate is built here, so no spawn can miss
/// it.
fn sudo() -> Command {
    let mut command = Command::new("sudo");
    command.arg("-n");
    command
}

fn run_sudo(args: &[&str]) -> Result<(), ComponentFailure> {
    let os: Vec<&OsStr> = args.iter().copied().map(OsStr::new).collect();
    run_sudo_os(&os)
}

/// Runs `sudo` with every argument kept as an `OsStr`, so a non-UTF-8 path
/// crosses to the child unchanged.
fn run_sudo_os(args: &[&OsStr]) -> Result<(), ComponentFailure> {
    run_sudo_reading_os(args).map(|_| ())
}

/// Runs a command through sudo, reading its standard output, every argument
/// kept as an `OsStr` so a non-UTF-8 path survives the crossing.
///
/// Its output pipes are bounded by [`run_reading_bounded`]: a process sudo left
/// holding one cannot keep the call waiting.
pub(crate) fn run_sudo_reading_os(args: &[&OsStr]) -> Result<String, ComponentFailure> {
    let output = run_reading_bounded(sudo().args(args), "sudo")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ComponentFailure::new(
            FailureKind::Install,
            format!("sudo command failed: {}", stderr.trim()),
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Runs `command` to its own exit while draining its output pipes, and gives
/// a process it left holding one [`super::install::KILL_GRACE`] to close it.
///
/// [`Command::output`] waits for both pipes to reach EOF with no limit, so a
/// process the command started that holds one, without reading or closing it,
/// stalls the call for as long as it lives. The command's own run keeps no
/// deadline — a slow copy is the command's business — and only the pipes are
/// bounded. A pipe still held at the deadline ends the command's group through
/// [`super::install::end_leftover_group`], and the failure says the pipes
/// stayed open.
fn run_reading_bounded(
    command: &mut Command,
    tool: &str,
) -> Result<std::process::Output, ComponentFailure> {
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| {
            ComponentFailure::new(FailureKind::Install, format!("failed to run {tool}: {e}"))
        })?;
    let group = nix::unistd::Pid::from_raw(child.id().cast_signed());
    let mut readers = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        readers.push(super::install::spawn_reader(stdout));
    }
    if let Some(stderr) = child.stderr.take() {
        readers.push(super::install::spawn_reader(stderr));
    }
    let status = child.wait().map_err(|e| {
        ComponentFailure::new(FailureKind::Install, format!("failed to run {tool}: {e}"))
    })?;
    let (drained, readers_finished) = super::install::join_readers_bounded(
        readers,
        std::time::Instant::now() + super::install::KILL_GRACE,
    );
    if !readers_finished {
        let _ = super::install::end_leftover_group(&mut child, group);
        let mut failure = ComponentFailure::new(
            FailureKind::Install,
            format!("{tool} exited but kept its output pipes open"),
        );
        failure.process_may_still_run = true;
        return Err(failure);
    }
    let mut drained = drained.into_iter();
    Ok(std::process::Output {
        status,
        stdout: drained.next().unwrap_or_default(),
        stderr: drained.next().unwrap_or_default(),
    })
}

/// Creates a `Command` that is prefixed with sudo if not root.
pub(crate) fn sudo_command(program: &str) -> Command {
    if is_root() {
        Command::new(program)
    } else {
        let mut cmd = sudo();
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
    let output = run_reading_bounded(
        sudo().args([
            "env",
            "LC_ALL=C",
            "mv",
            "--exchange",
            "--no-target-directory",
            &a.to_string_lossy(),
            &b.to_string_lossy(),
        ]),
        "sudo mv",
    )?;

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

/// The failure of a write that cannot finish within
/// [`super::install::KILL_GRACE`].
///
/// The line keeps the installer's existing `{tool} timed out after {} s` form.
/// It counts as possibly running: the give-up has no positive check that sudo
/// and what it started are gone.
fn sudo_tee_timeout() -> ComponentFailure {
    let mut failure = ComponentFailure::new(
        FailureKind::Install,
        format!(
            "sudo tee timed out after {} s",
            super::install::KILL_GRACE.as_secs()
        ),
    );
    failure.process_may_still_run = true;
    failure
}

/// Ends sudo's group and gives up a write that cannot finish within
/// [`super::install::KILL_GRACE`].
///
/// The line names the bound alone, not sudo's partial words, and counts the
/// write's processes as possibly running. The abandoned pipe readers hold their
/// buffers until their pipes close.
fn give_up_sudo_tee(
    child: &mut std::process::Child,
    group: nix::unistd::Pid,
    stderr_reader: Option<std::thread::JoinHandle<Vec<u8>>>,
) -> ComponentFailure {
    let _ = super::install::end_leftover_group(child, group);
    drop(super::install::join_readers_bounded(
        stderr_reader.into_iter().collect(),
        std::time::Instant::now() + super::install::KILL_GRACE,
    ));
    sudo_tee_timeout()
}

/// Writes content through `sudo tee`, keeping it off the command line.
///
/// stderr is captured rather than inherited, so sudo's own words (a refusal, a
/// missing tty) land in the failure message instead of over the progress rows;
/// stdout is discarded because `tee` echoes the content back. The stderr pipe
/// is drained while the content is written: sudo or a child of it may fill the
/// pipe before it reads its stdin, and each side would then wait for a reader
/// that never comes. The child's exit and its stderr pipe are then waited on
/// with a bound of their own — the content goes in from a thread joined within
/// [`super::install::KILL_GRACE`], the child's exit gets the same grace, and
/// the stderr reader is joined within it — because a process sudo left running
/// can hold the pipe open, and a `tee` that took the content can still block
/// afterwards. A give-up on any of them ends sudo's group through
/// [`super::install::end_leftover_group`], which names the members it cannot
/// reach.
fn write_via_sudo_tee(path: &Path, content: &[u8]) -> Result<(), ComponentFailure> {
    use std::io::Write as _;
    let mut child = sudo()
        .arg("tee")
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| {
            ComponentFailure::new(FailureKind::Install, format!("failed to run sudo tee: {e}"))
        })?;
    let group = nix::unistd::Pid::from_raw(child.id().cast_signed());

    let stderr_reader = child.stderr.take().map(super::install::spawn_reader);

    // Taking the handle closes stdin when it drops, so `tee` sees EOF even when
    // the write fails. The thread answers the write's error, if any.
    #[expect(
        clippy::expect_used,
        reason = "stdin was spawned with Stdio::piped(), so the pipe handle always exists"
    )]
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let content = content.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&content).err());
    let (written, write_finished) = super::install::join_bounded(
        vec![writer],
        std::time::Instant::now() + super::install::KILL_GRACE,
    );

    let status = if write_finished {
        // The content is in, but the child may still block after taking it (a
        // stalled filesystem under the target): its exit gets the same grace
        // as a pipe wait. Reaping it even when the write failed keeps a
        // nonzero exit (e.g. a refused sudo) a better error than the broken
        // pipe the write sees.
        match super::install::reap_within(&mut child, super::install::KILL_GRACE) {
            Ok(true) => child
                .wait()
                .map_err(|e| ComponentFailure::io(FailureKind::Install, path, e))?,
            Err(e) => return Err(ComponentFailure::io(FailureKind::Install, path, e)),
            Ok(false) => return Err(give_up_sudo_tee(&mut child, group, stderr_reader)),
        }
    } else {
        match child.try_wait() {
            // The child ended on its own after the content stopped flowing:
            // its exit still says more than the bound does.
            Ok(Some(status)) => {
                let _ = super::install::end_leftover_group(&mut child, group);
                status
            }
            // Still running at the bound: it produced no exit of its own, so
            // the write is reported as the bound, counted as possibly running.
            _ => return Err(give_up_sudo_tee(&mut child, group, stderr_reader)),
        }
    };
    let (drained, stderr_finished) = super::install::join_readers_bounded(
        stderr_reader.into_iter().collect(),
        std::time::Instant::now() + super::install::KILL_GRACE,
    );
    if !stderr_finished {
        // The drain gave up with the holder still on the pipe, so what sudo
        // wrote is lost with it and the failure names the exit alone.
        let _ = super::install::end_leftover_group(&mut child, group);
    }
    let captured = drained.into_iter().next().unwrap_or_default();
    if !status.success() {
        let stderr = String::from_utf8_lossy(&captured);
        let detail = stderr.trim();
        let message = if detail.is_empty() {
            "sudo tee failed".to_string()
        } else {
            format!("sudo tee failed: {detail}")
        };
        return Err(ComponentFailure::new(FailureKind::Install, message));
    }
    if !write_finished {
        return Err(sudo_tee_timeout());
    }
    written
        .into_iter()
        .next()
        .flatten()
        .map_or(Ok(()), Err)
        .map_err(|e| ComponentFailure::io(FailureKind::Install, path, e))?;
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

/// Creates directories recursively as root with `mode`, whatever the path
/// looks like. Every level this call creates gets `mode` set on it: `mkdir -p
/// -m` gives the mode only to the directory it names last, and a level left to
/// the umask (027, 077) is unsearchable for the invoking user, whose trust
/// check and restore read through it. A directory that already exists keeps
/// its own mode.
pub(crate) fn create_dir_all_as_root(path: &Path, mode: u32) -> Result<(), ComponentFailure> {
    let levels: Vec<&Path> = path
        .ancestors()
        .filter(|level| level.parent().is_some())
        .collect();
    if is_root() {
        for level in levels.into_iter().rev() {
            match std::fs::create_dir(level) {
                Ok(()) => std::fs::set_permissions(level, std::fs::Permissions::from_mode(mode))
                    .map_err(|e| ComponentFailure::io(FailureKind::Install, level, e))?,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(ComponentFailure::io(FailureKind::Install, level, e)),
            }
        }
        Ok(())
    } else {
        let mode = format!("{mode:o}");
        let mut args: Vec<&OsStr> = vec![
            OsStr::new("mkdir"),
            OsStr::new("-p"),
            OsStr::new("-m"),
            OsStr::new(&mode),
        ];
        for level in levels.into_iter().rev() {
            args.push(level.as_os_str());
        }
        run_sudo_os(&args)
    }
}

/// Copies a file as root, whatever the path looks like.
pub(crate) fn copy_file_as_root(src: &Path, dest: &Path) -> Result<(), ComponentFailure> {
    if is_root() {
        std::fs::copy(src, dest)
            .map_err(|e| ComponentFailure::io(FailureKind::Install, dest, e))?;
        Ok(())
    } else {
        run_sudo_os(&[
            OsStr::new("cp"),
            OsStr::new("-f"),
            src.as_os_str(),
            dest.as_os_str(),
        ])
    }
}

/// Recursively copies a directory's contents as root, whatever the paths look
/// like, into an existing destination without nesting it. `-H` dereferences
/// only the command-line source, the way the user-mode copy reads a component's
/// own path through a link while a link inside the package stays a link.
pub(crate) fn copy_dir_as_root(src: &Path, dest: &Path) -> Result<(), ComponentFailure> {
    if is_root() {
        super::backup::copy_dir_recursive(src, dest)
    } else {
        run_sudo_os(&[
            OsStr::new("cp"),
            OsStr::new("-rfT"),
            OsStr::new("-H"),
            src.as_os_str(),
            dest.as_os_str(),
        ])
    }
}

/// Removes a file as root, whatever the path looks like; a file already gone
/// is gone, as the non-root arm's `rm -f` has it.
pub(crate) fn remove_file_as_root(path: &Path) -> Result<(), ComponentFailure> {
    if is_root() {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(ComponentFailure::io(FailureKind::Install, path, e)),
        }
    } else {
        run_sudo_os(&[OsStr::new("rm"), OsStr::new("-f"), path.as_os_str()])
    }
}

/// Removes a directory tree as root, whatever the path looks like.
pub(crate) fn remove_dir_all_as_root(path: &Path) -> Result<(), ComponentFailure> {
    if is_root() {
        std::fs::remove_dir_all(path)
            .map_err(|e| ComponentFailure::io(FailureKind::Install, path, e))?;
        Ok(())
    } else {
        run_sudo_os(&[OsStr::new("rm"), OsStr::new("-rf"), path.as_os_str()])
    }
}

/// Removes an empty directory as root, whatever the path looks like.
pub(crate) fn remove_dir_as_root(path: &Path) -> Result<(), ComponentFailure> {
    if is_root() {
        std::fs::remove_dir(path)
            .map_err(|e| ComponentFailure::io(FailureKind::Install, path, e))?;
        Ok(())
    } else {
        run_sudo_os(&[OsStr::new("rmdir"), path.as_os_str()])
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::io::Read as _;
    use std::os::fd::AsFd as _;
    use std::path::PathBuf;

    use super::*;

    /// A root run removes a file already gone without failing: the non-root
    /// arm's `rm -f` has the same answer.
    #[test]
    fn removing_a_missing_file_as_root_succeeds() {
        if !is_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("gone");
        let removed = remove_file_as_root(&missing);
        assert!(removed.is_ok(), "{removed:?}");
    }

    /// A refused `sudo tee` neither prints over the terminal its child would
    /// inherit nor loses sudo's own words: the child's stderr is captured into
    /// the failure message. A pty stands in for the user's terminal, so an
    /// inherited fd 2 is what would put sudo's text on it.
    #[test]
    fn a_refused_sudo_tee_keeps_sudo_text_off_the_terminal() {
        if is_root() {
            return; // root writes with fs::write and never runs sudo
        }

        if std::env::var_os("PU_TEST_TEE_CHILD").is_some() {
            let target = PathBuf::from(std::env::var("PU_TEST_TEE_TARGET").unwrap());
            let failed = write_file_as_root(&target, b"payload")
                .expect_err("the fake sudo refuses the write");
            assert!(
                failed.message.contains("sudo tee failed"),
                "the failure lost its own words: {}",
                failed.message
            );
            assert!(
                failed.message.contains("sudo: a password is required"),
                "the failure lost sudo's words: {}",
                failed.message
            );
            std::fs::write(
                std::env::var_os("PU_TEST_TEE_RESULT").unwrap(),
                failed.message,
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
            "#!/bin/sh\nprintf 'sudo: a password is required\\n' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake_sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
        let target = dir.path().join("tee-target");
        let result = dir.path().join("failure");
        let path = OsString::from(format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ));

        let pty = nix::pty::openpty(None, None).unwrap();
        // glibc's `openpty` hands back descriptors without close-on-exec
        // (measured 2026-11-24 on glibc 2.44: both fds come back with
        // `FD_CLOEXEC` clear): a sibling test's child would inherit the slave,
        // keep the master's reader from seeing EOF, and stall this test until
        // that child exits.
        for fd in [pty.master.as_fd(), pty.slave.as_fd()] {
            nix::fcntl::fcntl(
                fd,
                nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
            )
            .unwrap();
        }
        let mut master = std::fs::File::from(pty.master);
        let terminal = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut buffer = [0u8; 4096];
            while let Ok(n) = master.read(&mut buffer) {
                if n == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..n]);
            }
            bytes
        });

        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "installer::privilege::tests::a_refused_sudo_tee_keeps_sudo_text_off_the_terminal",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", path)
            .env("PU_TEST_TEE_CHILD", "1")
            .env("PU_TEST_TEE_TARGET", &target)
            .env("PU_TEST_TEE_RESULT", &result)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::from(pty.slave));
        let mut child = command.spawn().unwrap();
        // `Command` keeps its own copy of the slave fd; it must close before
        // the reader can see the end of the pty.
        drop(command);
        let status = child.wait().unwrap();
        let terminal = terminal.join().unwrap();

        assert!(
            status.success(),
            "child run failed ({}); terminal: {}",
            status,
            String::from_utf8_lossy(&terminal)
        );
        assert_eq!(
            String::from_utf8_lossy(&terminal),
            "",
            "sudo's text reached the terminal"
        );
        assert_eq!(
            std::fs::read_to_string(&result).unwrap(),
            "sudo tee failed: sudo: a password is required"
        );
    }

    /// A fake `sudo` that floods stderr before reading its stdin neither
    /// deadlocks the write nor loses the flood: stderr is drained while the
    /// content is written. Without the drain each side fills its pipe and
    /// waits for a reader that never comes.
    #[test]
    fn a_sudo_tee_stderr_flood_does_not_block_the_write() {
        if is_root() {
            return; // root writes with fs::write and never runs sudo
        }

        if std::env::var_os("PU_TEST_TEE_FLOOD_CHILD").is_some() {
            let target = PathBuf::from(std::env::var("PU_TEST_TEE_FLOOD_TARGET").unwrap());
            let content = vec![b'x'; 300_000];
            let failed =
                write_file_as_root(&target, &content).expect_err("the fake sudo refuses the write");
            std::fs::write(
                std::env::var_os("PU_TEST_TEE_FLOOD_RESULT").unwrap(),
                failed.message,
            )
            .unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake_sudo = bin.join("sudo");
        // More than a pipe buffer to stderr before it reads the content.
        std::fs::write(
            &fake_sudo,
            "#!/bin/sh\nhead -c 200000 /dev/zero | tr '\\0' e >&2\ncat >/dev/null\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake_sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
        let target = dir.path().join("tee-target");
        let result = dir.path().join("failure");
        let path = OsString::from(format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ));

        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "installer::privilege::tests::a_sudo_tee_stderr_flood_does_not_block_the_write",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", path)
            .env("PU_TEST_TEE_FLOOD_CHILD", "1")
            .env("PU_TEST_TEE_FLOOD_TARGET", &target)
            .env("PU_TEST_TEE_FLOOD_RESULT", &result)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .process_group(0);
        let mut child = command.spawn().unwrap();
        let bound = std::time::Duration::from_secs(15);
        let deadline = std::time::Instant::now() + bound;
        let finished = loop {
            if child.try_wait().unwrap().is_some() {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        };
        if !finished {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(child.id().cast_signed()),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let status = child.wait().unwrap();
        let mut child_stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            drop(pipe.read_to_string(&mut child_stderr));
        }
        assert!(
            finished,
            "the write did not finish within {bound:?}: a full stderr pipe and the content awaited each other\n--- child stderr\n{child_stderr}"
        );
        assert!(
            status.success(),
            "child run failed ({status})\n--- child stderr\n{child_stderr}"
        );
        let message = std::fs::read_to_string(&result).unwrap();
        let prefix = "sudo tee failed: ";
        assert!(
            message.starts_with(prefix)
                && message.len() == prefix.len() + 200_000
                && message[prefix.len()..].bytes().all(|byte| byte == b'e'),
            "the failure did not carry sudo's whole flood: {} bytes, head {:?}",
            message.len(),
            String::from_utf8_lossy(message.as_bytes().get(..40).unwrap_or_default())
        );
    }

    /// Whether `pid` is gone within `bound`, asked with signal 0 until it is.
    fn gone_within(pid: i32, bound: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + bound;
        loop {
            if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None)
                == Err(nix::errno::Errno::ESRCH)
            {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    /// The `-n` lives in one constructor: [`sudo`]. A `sudo` spawn built
    /// anywhere else in the crate would slip past the argv test, so the crate's
    /// source holds exactly one literal sudo construction, in this module's
    /// helper. A spawn spelled from a variable or `format!` is not seen.
    #[test]
    fn sudo_is_constructed_in_one_place() {
        fn source_files(dir: &Path, files: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    source_files(&path, files);
                } else if path
                    .extension()
                    .is_some_and(|extension| extension == OsStr::new("rs"))
                {
                    files.push(path);
                }
            }
        }
        let mut files = Vec::new();
        source_files(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        let needle = "Command::new(\"sudo\")";
        let sites: Vec<(String, usize)> = files
            .into_iter()
            .map(|path| {
                let text = std::fs::read_to_string(&path).unwrap();
                (path.display().to_string(), text.matches(needle).count())
            })
            .filter(|(_, count)| *count > 0)
            .collect();
        assert_eq!(
            sites,
            vec![(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("src/installer/privilege.rs")
                    .display()
                    .to_string(),
                1
            )],
            "a sudo spawn outside `sudo()` escapes the -n rule"
        );
    }

    /// The pid `pid_file` records, when the fixture wrote one.
    fn recorded_pid(pid_file: &Path) -> Option<i32> {
        std::fs::read_to_string(pid_file).ok()?.trim().parse().ok()
    }

    /// Kills the process the pid file records, when it still runs.
    ///
    /// A fake records the pid of a background holder, whose process group is
    /// the fake's, so cleanup reaches it by pid and never by `killpg`.
    fn kill_recorded_pid(pid_file: &Path) {
        let Some(pid) = recorded_pid(pid_file) else {
            return;
        };
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }

    /// Every `sudo` the library spawns passes `-n` first, so a missing or
    /// expired credential fails the component at once instead of leaving a
    /// `sudo` stopped in a process group no terminal can answer.
    #[test]
    fn every_sudo_spawn_passes_non_interactive() {
        if is_root() {
            return; // root runs the command directly and never runs sudo
        }

        if std::env::var_os("PU_TEST_SUDO_ARGV_CHILD").is_some() {
            let a = std::env::var("PU_TEST_SUDO_ARGV_A").unwrap();
            let b = std::env::var("PU_TEST_SUDO_ARGV_B").unwrap();
            let target = std::env::var("PU_TEST_SUDO_ARGV_TARGET").unwrap();
            // The fake refuses each call; the argv it recorded is the point.
            run_sudo_reading_os(&[OsStr::new("true")]).expect_err("the fake sudo refuses the read");
            sudo_exchange(Path::new(&a), Path::new(&b))
                .expect_err("the fake sudo refuses the exchange");
            write_file_as_root(Path::new(&target), b"payload")
                .expect_err("the fake sudo refuses the write");
            std::fs::write(std::env::var_os("PU_TEST_SUDO_ARGV_DONE").unwrap(), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake_sudo = bin.join("sudo");
        // One block per invocation: its whole argv, one argument per line.
        std::fs::write(
            &fake_sudo,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$PU_TEST_SUDO_ARGV_LOG\"\n\
             printf '\\n' >> \"$PU_TEST_SUDO_ARGV_LOG\"\n\
             printf 'sudo: a password is required\\n' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake_sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
        let log = dir.path().join("sudo-argv.log");
        let done = dir.path().join("child-done");
        let a = dir.path().join("new tree");
        let b = "/usr/share/plasma/plasmoids/tree";
        let target = dir.path().join("tee-target");
        let path = OsString::from(format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ));

        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "installer::privilege::tests::every_sudo_spawn_passes_non_interactive",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", path)
            .env("PU_TEST_SUDO_ARGV_CHILD", "1")
            .env("PU_TEST_SUDO_ARGV_LOG", &log)
            .env("PU_TEST_SUDO_ARGV_DONE", &done)
            .env("PU_TEST_SUDO_ARGV_A", &a)
            .env("PU_TEST_SUDO_ARGV_B", b)
            .env("PU_TEST_SUDO_ARGV_TARGET", &target)
            .output()
            .unwrap();
        assert!(
            output.status.success() && done.exists(),
            "child run failed: {}\n--- stderr\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        );

        let log = std::fs::read_to_string(&log).unwrap();
        let blocks: Vec<Vec<String>> = log
            .split("\n\n")
            .filter(|block| !block.trim().is_empty())
            .map(|block| block.lines().map(str::to_string).collect())
            .collect();
        assert_eq!(
            blocks,
            vec![
                argv(&["-n", "true"]),
                argv(&[
                    "-n",
                    "env",
                    "LC_ALL=C",
                    "mv",
                    "--exchange",
                    "--no-target-directory",
                    &a.to_string_lossy(),
                    b,
                ]),
                argv(&["-n", "tee", &target.to_string_lossy()]),
            ],
            "a sudo spawn can prompt"
        );

        let kpackagetool = sudo_command("kpackagetool6");
        assert_eq!(
            kpackagetool
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<String>>(),
            argv(&["-n", "kpackagetool6"]),
            "the elevated kpackagetool6 route can prompt"
        );
    }

    /// A `sudo` that stops itself the way sudo does when a prompt is due in a
    /// background process group — its `tgetpass.c` sends itself SIGTTOU after
    /// the terminal refuses the write (measured under a pty 2026-11-24) — must
    /// not hold the write: `-n` refuses at once and the call returns.
    #[test]
    fn a_sudo_that_would_stop_itself_fails_the_write_at_once() {
        if is_root() {
            return; // root writes with fs::write and never runs sudo
        }

        if std::env::var_os("PU_TEST_SUDO_STOP_CHILD").is_some() {
            let target = PathBuf::from(std::env::var("PU_TEST_SUDO_STOP_TARGET").unwrap());
            let started = std::time::Instant::now();
            let failed = write_file_as_root(&target, b"payload")
                .expect_err("the fake sudo refuses the write");
            let elapsed = started.elapsed();
            std::fs::write(
                std::env::var_os("PU_TEST_SUDO_STOP_ELAPSED").unwrap(),
                elapsed.as_secs_f64().to_string(),
            )
            .unwrap();
            std::fs::write(
                std::env::var_os("PU_TEST_SUDO_STOP_RESULT").unwrap(),
                format!("{}|{}", failed.message, failed.process_may_still_run),
            )
            .unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake_sudo = bin.join("sudo");
        // Without `-n` the fake stops itself and never exits, as sudo does
        // instead of prompting where it cannot reach a terminal; with `-n` it
        // refuses at once.
        std::fs::write(
            &fake_sudo,
            "#!/bin/sh\necho $$ > \"$PU_TEST_SUDO_STOP_PID\"\n\
             if [ \"$1\" = -n ]; then printf 'sudo: a password is required\\n' >&2; exit 1; fi\n\
             kill -TTOU $$\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake_sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
        let target = dir.path().join("tee-target");
        let pid_path = dir.path().join("sudo.pid");
        let elapsed_path = dir.path().join("elapsed");
        let result = dir.path().join("result");
        let path = OsString::from(format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ));

        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "installer::privilege::tests::a_sudo_that_would_stop_itself_fails_the_write_at_once",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", path)
            .env("PU_TEST_SUDO_STOP_CHILD", "1")
            .env("PU_TEST_SUDO_STOP_TARGET", &target)
            .env("PU_TEST_SUDO_STOP_PID", &pid_path)
            .env("PU_TEST_SUDO_STOP_ELAPSED", &elapsed_path)
            .env("PU_TEST_SUDO_STOP_RESULT", &result)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .process_group(0);
        let mut child = command.spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let finished = loop {
            if child.try_wait().unwrap().is_some() {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        };
        let stopped = recorded_pid(&pid_path);
        let stopped_gone =
            stopped.is_none_or(|pid| gone_within(pid, std::time::Duration::from_secs(3)));
        if !finished {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(child.id().cast_signed()),
                nix::sys::signal::Signal::SIGKILL,
            );
            kill_recorded_pid(&pid_path);
        }
        let status = child.wait().unwrap();
        let mut child_stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            drop(pipe.read_to_string(&mut child_stderr));
        }
        // Clean up whatever the stopped sudo left, so a red assertion below
        // leaves no process behind.
        kill_recorded_pid(&pid_path);
        assert!(
            finished,
            "the write waited for a sudo that stopped itself\n--- child stderr\n{child_stderr}"
        );
        assert!(
            status.success(),
            "child run failed ({status})\n--- child stderr\n{child_stderr}"
        );
        let elapsed: f64 = std::fs::read_to_string(&elapsed_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let bound = crate::installer::install::KILL_GRACE + std::time::Duration::from_secs(2);
        assert!(
            elapsed < bound.as_secs_f64(),
            "the write waited for a sudo that would prompt: {elapsed:.3} s"
        );
        assert_eq!(
            std::fs::read_to_string(&result).unwrap(),
            "sudo tee failed: sudo: a password is required|false",
            "the refusal did not fail the write with sudo's own words"
        );
        assert!(
            stopped_gone,
            "the fake sudo is still running after the write returned"
        );
    }

    /// A `sudo tee` still running at the write's bound is reported as the
    /// bound, with the write counted as possibly running, not as a failed exit
    /// the child never produced; the give-up ends its group.
    #[test]
    fn a_sudo_tee_still_running_at_the_bound_reports_the_timeout() {
        if is_root() {
            return; // root writes with fs::write and never runs sudo
        }

        if std::env::var_os("PU_TEST_TEE_RUNNING_CHILD").is_some() {
            let target = PathBuf::from(std::env::var("PU_TEST_TEE_RUNNING_TARGET").unwrap());
            let content = vec![b'x'; 300_000];
            let started = std::time::Instant::now();
            let failed =
                write_file_as_root(&target, &content).expect_err("the fake sudo never reads stdin");
            let elapsed = started.elapsed();
            std::fs::write(
                std::env::var_os("PU_TEST_TEE_RUNNING_ELAPSED").unwrap(),
                elapsed.as_secs_f64().to_string(),
            )
            .unwrap();
            std::fs::write(
                std::env::var_os("PU_TEST_TEE_RUNNING_RESULT").unwrap(),
                format!("{}|{}", failed.message, failed.process_may_still_run),
            )
            .unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake_sudo = bin.join("sudo");
        // Runs past the bound without reading the content, so the write cannot
        // finish while the child lives.
        std::fs::write(
            &fake_sudo,
            "#!/bin/sh\necho $$ > \"$PU_TEST_TEE_RUNNING_PID\"\nexec sleep 30\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake_sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
        let target = dir.path().join("tee-target");
        let pid_path = dir.path().join("sudo.pid");
        let elapsed_path = dir.path().join("elapsed");
        let result = dir.path().join("result");
        let path = OsString::from(format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ));

        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "installer::privilege::tests::a_sudo_tee_still_running_at_the_bound_reports_the_timeout",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", path)
            .env("PU_TEST_TEE_RUNNING_CHILD", "1")
            .env("PU_TEST_TEE_RUNNING_TARGET", &target)
            .env("PU_TEST_TEE_RUNNING_PID", &pid_path)
            .env("PU_TEST_TEE_RUNNING_ELAPSED", &elapsed_path)
            .env("PU_TEST_TEE_RUNNING_RESULT", &result)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .process_group(0);
        let mut child = command.spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let finished = loop {
            if child.try_wait().unwrap().is_some() {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        };
        let recorded = recorded_pid(&pid_path);
        let recorded_gone =
            recorded.is_none_or(|pid| gone_within(pid, std::time::Duration::from_secs(3)));
        if !finished {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(child.id().cast_signed()),
                nix::sys::signal::Signal::SIGKILL,
            );
            kill_recorded_pid(&pid_path);
        }
        let status = child.wait().unwrap();
        let mut child_stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            drop(pipe.read_to_string(&mut child_stderr));
        }
        // Clean up whatever the give-up left, so a red assertion below leaves
        // no process behind.
        kill_recorded_pid(&pid_path);
        assert!(
            finished,
            "the write did not finish within 15 s\n--- child stderr\n{child_stderr}"
        );
        assert!(
            status.success(),
            "child run failed ({status})\n--- child stderr\n{child_stderr}"
        );
        // The holder leg runs before the elapsed and line legs so it binds the
        // give-up's group kill on its own.
        assert!(
            recorded_gone,
            "the fake sudo {recorded:?} is still running after the write returned"
        );
        let elapsed: f64 = std::fs::read_to_string(&elapsed_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let bound = crate::installer::install::KILL_GRACE + std::time::Duration::from_secs(2);
        assert!(
            elapsed < bound.as_secs_f64(),
            "the write waited for a sudo that never read it: {elapsed:.3} s"
        );
        assert_eq!(
            std::fs::read_to_string(&result).unwrap(),
            "sudo tee timed out after 5 s|true",
            "the give-up did not report the bound and the possibly-running write"
        );
    }

    /// A `sudo tee` that takes the whole content and then blocks — a stalled
    /// filesystem under the target — is bounded too: the child's exit gets the
    /// same grace as the write, and the give-up reports the bound.
    #[test]
    fn a_sudo_tee_that_blocks_after_the_content_times_out() {
        if is_root() {
            return; // root writes with fs::write and never runs sudo
        }

        if std::env::var_os("PU_TEST_TEE_SLOW_CHILD").is_some() {
            let target = PathBuf::from(std::env::var("PU_TEST_TEE_SLOW_TARGET").unwrap());
            let started = std::time::Instant::now();
            let failed = write_file_as_root(&target, b"payload")
                .expect_err("the fake sudo blocks after taking the content");
            let elapsed = started.elapsed();
            std::fs::write(
                std::env::var_os("PU_TEST_TEE_SLOW_ELAPSED").unwrap(),
                elapsed.as_secs_f64().to_string(),
            )
            .unwrap();
            std::fs::write(
                std::env::var_os("PU_TEST_TEE_SLOW_RESULT").unwrap(),
                format!("{}|{}", failed.message, failed.process_may_still_run),
            )
            .unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake_sudo = bin.join("sudo");
        // Takes the content, then blocks without exiting.
        std::fs::write(
            &fake_sudo,
            "#!/bin/sh\nif [ \"$1\" = -n ]; then shift; fi\n\
             cat > \"$PU_TEST_TEE_SLOW_WRITTEN\"\n\
             echo $$ > \"$PU_TEST_TEE_SLOW_PID\"\nexec sleep 30\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake_sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
        let target = dir.path().join("tee-target");
        let written = dir.path().join("written");
        let pid_path = dir.path().join("sudo.pid");
        let elapsed_path = dir.path().join("elapsed");
        let result = dir.path().join("result");
        let path = OsString::from(format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ));

        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "installer::privilege::tests::a_sudo_tee_that_blocks_after_the_content_times_out",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", path)
            .env("PU_TEST_TEE_SLOW_CHILD", "1")
            .env("PU_TEST_TEE_SLOW_TARGET", &target)
            .env("PU_TEST_TEE_SLOW_WRITTEN", &written)
            .env("PU_TEST_TEE_SLOW_PID", &pid_path)
            .env("PU_TEST_TEE_SLOW_ELAPSED", &elapsed_path)
            .env("PU_TEST_TEE_SLOW_RESULT", &result)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .process_group(0);
        let mut child = command.spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let finished = loop {
            if child.try_wait().unwrap().is_some() {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        };
        let recorded = recorded_pid(&pid_path);
        let recorded_gone =
            recorded.is_none_or(|pid| gone_within(pid, std::time::Duration::from_secs(3)));
        if !finished {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(child.id().cast_signed()),
                nix::sys::signal::Signal::SIGKILL,
            );
            kill_recorded_pid(&pid_path);
        }
        let status = child.wait().unwrap();
        let mut child_stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            drop(pipe.read_to_string(&mut child_stderr));
        }
        // Clean up whatever the give-up left, so a red assertion below leaves
        // no process behind.
        kill_recorded_pid(&pid_path);
        assert!(
            finished,
            "the write did not finish within 15 s\n--- child stderr\n{child_stderr}"
        );
        assert!(
            status.success(),
            "child run failed ({status})\n--- child stderr\n{child_stderr}"
        );
        assert_eq!(
            std::fs::read_to_string(&written).unwrap(),
            "payload",
            "the content never reached the fake tee"
        );
        assert!(
            recorded_gone,
            "the fake sudo {recorded:?} is still running after the write returned"
        );
        let elapsed: f64 = std::fs::read_to_string(&elapsed_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let bound = crate::installer::install::KILL_GRACE + std::time::Duration::from_secs(2);
        assert!(
            elapsed < bound.as_secs_f64(),
            "the write waited for a tee that blocked after taking the content: {elapsed:.3} s"
        );
        assert_eq!(
            std::fs::read_to_string(&result).unwrap(),
            "sudo tee timed out after 5 s|true",
            "the give-up did not report the bound and the possibly-running write"
        );
    }

    /// Runs [`write_file_as_root`] against a fake `sudo` that execs the real
    /// `tee`, so the content crosses on the pipe and the file holds it.
    #[test]
    fn a_sudo_tee_write_lands_the_content() {
        if is_root() {
            return; // root writes with fs::write and never runs sudo
        }

        if std::env::var_os("PU_TEST_TEE_OK_CHILD").is_some() {
            let target = PathBuf::from(std::env::var("PU_TEST_TEE_OK_TARGET").unwrap());
            write_file_as_root(&target, b"payload").expect("the fake sudo runs the real tee");
            std::fs::write(std::env::var_os("PU_TEST_TEE_OK_DONE").unwrap(), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake_sudo = bin.join("sudo");
        std::fs::write(
            &fake_sudo,
            "#!/bin/sh\nif [ \"$1\" = -n ]; then shift; fi\nexec \"$@\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake_sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
        let target = dir.path().join("tee-target");
        let done = dir.path().join("child-done");
        let path = OsString::from(format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ));

        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "installer::privilege::tests::a_sudo_tee_write_lands_the_content",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", path)
            .env("PU_TEST_TEE_OK_CHILD", "1")
            .env("PU_TEST_TEE_OK_TARGET", &target)
            .env("PU_TEST_TEE_OK_DONE", &done)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()
            .unwrap();
        assert!(
            output.status.success() && done.exists(),
            "child run failed ({})\n--- child stderr\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "payload",
            "the content did not land through tee's stdin"
        );
    }

    /// A fake `sudo` that leaves a background process holding its stdin,
    /// without reading it, does not keep a write larger than the pipe buffer
    /// waiting: the content goes in from a thread joined within the bound, and
    /// the give-up ends sudo's group, so the holder goes with it.
    #[test]
    fn a_sudo_tee_stdin_holder_does_not_hold_the_write() {
        if is_root() {
            return; // root writes with fs::write and never runs sudo
        }

        if std::env::var_os("PU_TEST_TEE_STDIN_CHILD").is_some() {
            let target = PathBuf::from(std::env::var("PU_TEST_TEE_STDIN_TARGET").unwrap());
            let content = vec![b'x'; 300_000];
            let started = std::time::Instant::now();
            let failed =
                write_file_as_root(&target, &content).expect_err("the fake sudo refuses the write");
            let elapsed = started.elapsed();
            std::fs::write(
                std::env::var_os("PU_TEST_TEE_STDIN_ELAPSED").unwrap(),
                elapsed.as_secs_f64().to_string(),
            )
            .unwrap();
            std::fs::write(
                std::env::var_os("PU_TEST_TEE_STDIN_RESULT").unwrap(),
                failed.message,
            )
            .unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake_sudo = bin.join("sudo");
        // The background process holds the pipe and never reads it, so a write
        // past the pipe buffer blocks until the bound ends it. Its stdin is a
        // duplicate: a shell hands a background job /dev/null for stdin. Its
        // stderr is dropped, so the pipe it holds is the write's alone.
        let holder_secs = crate::installer::install::KILL_GRACE.as_secs() + 4;
        std::fs::write(
            &fake_sudo,
            format!(
                "#!/bin/sh\nexec 3<&0\nsleep {holder_secs} 2>/dev/null &\necho $! > \"$PU_TEST_TEE_STDIN_HOLDER\"\nexit 1\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake_sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
        let target = dir.path().join("tee-target");
        let elapsed_path = dir.path().join("elapsed");
        let result = dir.path().join("failure");
        let holder_path = dir.path().join("holder.pid");
        let path = OsString::from(format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ));

        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "installer::privilege::tests::a_sudo_tee_stdin_holder_does_not_hold_the_write",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", path)
            .env("PU_TEST_TEE_STDIN_CHILD", "1")
            .env("PU_TEST_TEE_STDIN_TARGET", &target)
            .env("PU_TEST_TEE_STDIN_ELAPSED", &elapsed_path)
            .env("PU_TEST_TEE_STDIN_RESULT", &result)
            .env("PU_TEST_TEE_STDIN_HOLDER", &holder_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .process_group(0);
        let mut child = command.spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let finished = loop {
            if child.try_wait().unwrap().is_some() {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        };
        if !finished {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(child.id().cast_signed()),
                nix::sys::signal::Signal::SIGKILL,
            );
            kill_recorded_pid(&holder_path);
        }
        let status = child.wait().unwrap();
        let mut child_stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            drop(pipe.read_to_string(&mut child_stderr));
        }
        let holder = recorded_pid(&holder_path);
        let holder_gone =
            holder.is_none_or(|pid| gone_within(pid, std::time::Duration::from_secs(3)));
        // Clean up whatever the give-up left, so a red assertion below leaves
        // no holder behind.
        kill_recorded_pid(&holder_path);
        assert!(
            finished,
            "the child run did not finish within 15 s\n--- child stderr\n{child_stderr}"
        );
        assert!(
            status.success(),
            "child run failed ({status})\n--- child stderr\n{child_stderr}"
        );
        let elapsed: f64 = std::fs::read_to_string(&elapsed_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let bound = crate::installer::install::KILL_GRACE + std::time::Duration::from_secs(2);
        assert!(
            elapsed < bound.as_secs_f64(),
            "the write waited for a process holding sudo's stdin: {elapsed:.3} s"
        );
        assert_eq!(
            std::fs::read_to_string(&result).unwrap(),
            "sudo tee failed",
            "the failure did not name sudo's failed exit"
        );
        assert!(
            holder_gone,
            "the fake's stdin holder is still running after the write returned"
        );
    }

    /// A `sudo tee` that exits 0 while its content never got in cannot be
    /// called a success: the give-up reports the bound, and counts the write's
    /// processes as possibly running.
    #[test]
    fn a_sudo_tee_give_up_with_no_failed_exit_reports_the_timeout() {
        if is_root() {
            return; // root writes with fs::write and never runs sudo
        }

        if std::env::var_os("PU_TEST_TEE_TIMEOUT_CHILD").is_some() {
            let target = PathBuf::from(std::env::var("PU_TEST_TEE_TIMEOUT_TARGET").unwrap());
            let content = vec![b'x'; 300_000];
            let started = std::time::Instant::now();
            let failed = write_file_as_root(&target, &content)
                .expect_err("the fake sudo exits without taking the content");
            let elapsed = started.elapsed();
            std::fs::write(
                std::env::var_os("PU_TEST_TEE_TIMEOUT_ELAPSED").unwrap(),
                elapsed.as_secs_f64().to_string(),
            )
            .unwrap();
            std::fs::write(
                std::env::var_os("PU_TEST_TEE_TIMEOUT_RESULT").unwrap(),
                format!("{}|{}", failed.message, failed.process_may_still_run),
            )
            .unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake_sudo = bin.join("sudo");
        // The fake succeeds at once while a background process holds the pipe
        // unread, so only the write's bound can end the call.
        let holder_secs = crate::installer::install::KILL_GRACE.as_secs() + 4;
        std::fs::write(
            &fake_sudo,
            format!(
                "#!/bin/sh\nexec 3<&0\nsleep {holder_secs} 2>/dev/null &\necho $! > \"$PU_TEST_TEE_TIMEOUT_HOLDER\"\nexit 0\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake_sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
        let target = dir.path().join("tee-target");
        let elapsed_path = dir.path().join("elapsed");
        let result = dir.path().join("result");
        let holder_path = dir.path().join("holder.pid");
        let path = OsString::from(format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ));

        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "installer::privilege::tests::a_sudo_tee_give_up_with_no_failed_exit_reports_the_timeout",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", path)
            .env("PU_TEST_TEE_TIMEOUT_CHILD", "1")
            .env("PU_TEST_TEE_TIMEOUT_TARGET", &target)
            .env("PU_TEST_TEE_TIMEOUT_ELAPSED", &elapsed_path)
            .env("PU_TEST_TEE_TIMEOUT_RESULT", &result)
            .env("PU_TEST_TEE_TIMEOUT_HOLDER", &holder_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .process_group(0);
        let mut child = command.spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let finished = loop {
            if child.try_wait().unwrap().is_some() {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        };
        if !finished {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(child.id().cast_signed()),
                nix::sys::signal::Signal::SIGKILL,
            );
            kill_recorded_pid(&holder_path);
        }
        let status = child.wait().unwrap();
        let mut child_stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            drop(pipe.read_to_string(&mut child_stderr));
        }
        let holder = recorded_pid(&holder_path);
        let holder_gone =
            holder.is_none_or(|pid| gone_within(pid, std::time::Duration::from_secs(3)));
        // Clean up whatever the give-up left, so a red assertion below leaves
        // no holder behind.
        kill_recorded_pid(&holder_path);
        assert!(
            finished,
            "the child run did not finish within 15 s\n--- child stderr\n{child_stderr}"
        );
        assert!(
            status.success(),
            "child run failed ({status})\n--- child stderr\n{child_stderr}"
        );
        let elapsed: f64 = std::fs::read_to_string(&elapsed_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let bound = crate::installer::install::KILL_GRACE + std::time::Duration::from_secs(2);
        assert!(
            elapsed < bound.as_secs_f64(),
            "the write waited for a process holding sudo's stdin: {elapsed:.3} s"
        );
        assert_eq!(
            std::fs::read_to_string(&result).unwrap(),
            "sudo tee timed out after 5 s|true",
            "the give-up did not report the bound and the possibly-running write"
        );
        assert!(
            holder_gone,
            "the fake's stdin holder is still running after the write returned"
        );
    }

    /// A fake `sudo` that leaves a background process holding its stdout does
    /// not keep an elevated read waiting: the output pipes are joined within
    /// the bound, the failure names the pipes, and the give-up ends sudo's
    /// group so the holder goes with it.
    #[test]
    fn a_sudo_read_does_not_wait_for_a_stdout_holder() {
        if std::env::var_os("PU_TEST_SUDO_READ_CHILD").is_some() {
            let started = std::time::Instant::now();
            let outcome = match run_sudo_reading_os(&[OsStr::new("true")]) {
                Ok(text) => format!("ok: {text}"),
                Err(e) => format!("error: {}", e.message),
            };
            let elapsed = started.elapsed();
            std::fs::write(
                std::env::var_os("PU_TEST_SUDO_READ_ELAPSED").unwrap(),
                elapsed.as_secs_f64().to_string(),
            )
            .unwrap();
            std::fs::write(
                std::env::var_os("PU_TEST_SUDO_READ_RESULT").unwrap(),
                outcome,
            )
            .unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake_sudo = bin.join("sudo");
        // The background process inherits stdout and holds it open past the
        // fake's exit, so a plain `.output()` waits for it to die.
        let holder_secs = crate::installer::install::KILL_GRACE.as_secs() + 4;
        std::fs::write(
            &fake_sudo,
            format!(
                "#!/bin/sh\nsleep {holder_secs} &\necho $! > \"$PU_TEST_SUDO_READ_HOLDER\"\nprintf 'answer\\n'\nexit 0\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake_sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
        let elapsed_path = dir.path().join("elapsed");
        let result = dir.path().join("result");
        let holder_path = dir.path().join("holder.pid");
        let path = OsString::from(format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ));

        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "installer::privilege::tests::a_sudo_read_does_not_wait_for_a_stdout_holder",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", path)
            .env("PU_TEST_SUDO_READ_CHILD", "1")
            .env("PU_TEST_SUDO_READ_ELAPSED", &elapsed_path)
            .env("PU_TEST_SUDO_READ_RESULT", &result)
            .env("PU_TEST_SUDO_READ_HOLDER", &holder_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .process_group(0);
        let mut child = command.spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let finished = loop {
            if child.try_wait().unwrap().is_some() {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        };
        if !finished {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(child.id().cast_signed()),
                nix::sys::signal::Signal::SIGKILL,
            );
            kill_recorded_pid(&holder_path);
        }
        let status = child.wait().unwrap();
        let mut child_stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            drop(pipe.read_to_string(&mut child_stderr));
        }
        let holder = recorded_pid(&holder_path);
        let holder_gone =
            holder.is_none_or(|pid| gone_within(pid, std::time::Duration::from_secs(3)));
        // Clean up whatever the give-up left, so a red assertion below leaves
        // no holder behind.
        kill_recorded_pid(&holder_path);
        assert!(
            finished,
            "the child run did not finish within 15 s\n--- child stderr\n{child_stderr}"
        );
        assert!(
            status.success(),
            "child run failed ({status})\n--- child stderr\n{child_stderr}"
        );
        let elapsed: f64 = std::fs::read_to_string(&elapsed_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let bound = crate::installer::install::KILL_GRACE + std::time::Duration::from_secs(2);
        assert!(
            elapsed < bound.as_secs_f64(),
            "the read waited for a process holding sudo's stdout: {elapsed:.3} s"
        );
        assert_eq!(
            std::fs::read_to_string(&result).unwrap(),
            "error: sudo exited but kept its output pipes open",
            "the read did not name the pipes a process kept open"
        );
        assert!(
            holder_gone,
            "the fake's stdout holder is still running after the read returned"
        );
    }

    /// A fake `sudo` that leaves a background process holding its stderr does
    /// not keep [`sudo_exchange`] waiting: the output pipes are joined within
    /// the bound, the failure names them, and the give-up ends sudo's group so
    /// the holder goes with it.
    #[test]
    fn a_sudo_exchange_does_not_wait_for_a_pipe_holder() {
        if std::env::var_os("PU_TEST_EXCHANGE_HOLDER_CHILD").is_some() {
            let a = std::env::var("PU_TEST_EXCHANGE_A").unwrap();
            let b = std::env::var("PU_TEST_EXCHANGE_B").unwrap();
            let started = std::time::Instant::now();
            let outcome = match sudo_exchange(Path::new(&a), Path::new(&b)) {
                Ok(true) => "exchanged".to_string(),
                Ok(false) => "fallback".to_string(),
                Err(e) => format!("error: {}", e.message),
            };
            let elapsed = started.elapsed();
            std::fs::write(
                std::env::var_os("PU_TEST_EXCHANGE_HOLDER_ELAPSED").unwrap(),
                elapsed.as_secs_f64().to_string(),
            )
            .unwrap();
            std::fs::write(
                std::env::var_os("PU_TEST_EXCHANGE_HOLDER_RESULT").unwrap(),
                outcome,
            )
            .unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake_sudo = bin.join("sudo");
        // The background process inherits stderr and holds it open past the
        // fake's exit, so a plain `.output()` waits for it to die.
        let holder_secs = crate::installer::install::KILL_GRACE.as_secs() + 4;
        std::fs::write(
            &fake_sudo,
            format!(
                "#!/bin/sh\nsleep {holder_secs} &\necho $! > \"$PU_TEST_EXCHANGE_HOLDER\"\nexit 0\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake_sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
        let elapsed_path = dir.path().join("elapsed");
        let result = dir.path().join("result");
        let holder_path = dir.path().join("holder.pid");
        let a = dir.path().join("new tree");
        // Only ever passed to the fake sudo, which runs no command.
        let b = Path::new("/usr/share/plasma/plasmoids/tree");
        let path = OsString::from(format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ));

        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "installer::privilege::tests::a_sudo_exchange_does_not_wait_for_a_pipe_holder",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", path)
            .env("PU_TEST_EXCHANGE_HOLDER_CHILD", "1")
            .env("PU_TEST_EXCHANGE_HOLDER_ELAPSED", &elapsed_path)
            .env("PU_TEST_EXCHANGE_HOLDER_RESULT", &result)
            .env("PU_TEST_EXCHANGE_HOLDER", &holder_path)
            .env("PU_TEST_EXCHANGE_A", &a)
            .env("PU_TEST_EXCHANGE_B", b)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .process_group(0);
        let mut child = command.spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let finished = loop {
            if child.try_wait().unwrap().is_some() {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        };
        if !finished {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(child.id().cast_signed()),
                nix::sys::signal::Signal::SIGKILL,
            );
            kill_recorded_pid(&holder_path);
        }
        let status = child.wait().unwrap();
        let mut child_stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            drop(pipe.read_to_string(&mut child_stderr));
        }
        let holder = recorded_pid(&holder_path);
        let holder_gone =
            holder.is_none_or(|pid| gone_within(pid, std::time::Duration::from_secs(3)));
        // Clean up whatever the give-up left, so a red assertion below leaves
        // no holder behind.
        kill_recorded_pid(&holder_path);
        assert!(
            finished,
            "the child run did not finish within 15 s\n--- child stderr\n{child_stderr}"
        );
        assert!(
            status.success(),
            "child run failed ({status})\n--- child stderr\n{child_stderr}"
        );
        let elapsed: f64 = std::fs::read_to_string(&elapsed_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let bound = crate::installer::install::KILL_GRACE + std::time::Duration::from_secs(2);
        assert!(
            elapsed < bound.as_secs_f64(),
            "the exchange waited for a process holding sudo's stderr: {elapsed:.3} s"
        );
        assert_eq!(
            std::fs::read_to_string(&result).unwrap(),
            "error: sudo mv exited but kept its output pipes open",
            "the exchange did not name the pipes a process kept open"
        );
        assert!(
            holder_gone,
            "the fake's pipe holder is still running after the exchange returned"
        );
    }

    /// A fake `sudo` that leaves a background process holding its stderr does
    /// not keep the write waiting: the reader is joined with the same bound as
    /// every other reader in the installer, and the give-up ends sudo's group
    /// so no process the write started outlives it.
    #[test]
    fn a_sudo_tee_stderr_holder_does_not_hold_the_write() {
        if is_root() {
            return; // root writes with fs::write and never runs sudo
        }

        if std::env::var_os("PU_TEST_TEE_HOLDER_CHILD").is_some() {
            let target = PathBuf::from(std::env::var("PU_TEST_TEE_HOLDER_TARGET").unwrap());
            let started = std::time::Instant::now();
            let failed = write_file_as_root(&target, b"payload")
                .expect_err("the fake sudo refuses the write");
            let elapsed = started.elapsed();
            std::fs::write(
                std::env::var_os("PU_TEST_TEE_HOLDER_ELAPSED").unwrap(),
                elapsed.as_secs_f64().to_string(),
            )
            .unwrap();
            std::fs::write(
                std::env::var_os("PU_TEST_TEE_HOLDER_RESULT").unwrap(),
                failed.message,
            )
            .unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake_sudo = bin.join("sudo");
        // The background process inherits the stderr pipe and outlives sudo,
        // by more than the bound below, so a plain join cannot pass. It records
        // its pid, so the test can insist the write left no process behind.
        let holder_secs = crate::installer::install::KILL_GRACE.as_secs() + 4;
        std::fs::write(
            &fake_sudo,
            format!(
                "#!/bin/sh\nsleep {holder_secs} &\necho $! > \"$PU_TEST_TEE_HOLDER_PID\"\ncat >/dev/null\nexit 1\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake_sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
        let target = dir.path().join("tee-target");
        let elapsed_path = dir.path().join("elapsed");
        let result = dir.path().join("failure");
        let holder_path = dir.path().join("holder.pid");
        let path = OsString::from(format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ));

        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "installer::privilege::tests::a_sudo_tee_stderr_holder_does_not_hold_the_write",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", path)
            .env("PU_TEST_TEE_HOLDER_CHILD", "1")
            .env("PU_TEST_TEE_HOLDER_TARGET", &target)
            .env("PU_TEST_TEE_HOLDER_ELAPSED", &elapsed_path)
            .env("PU_TEST_TEE_HOLDER_RESULT", &result)
            .env("PU_TEST_TEE_HOLDER_PID", &holder_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .process_group(0);
        let mut child = command.spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let finished = loop {
            if child.try_wait().unwrap().is_some() {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        };
        if !finished {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(child.id().cast_signed()),
                nix::sys::signal::Signal::SIGKILL,
            );
            kill_recorded_pid(&holder_path);
        }
        let status = child.wait().unwrap();
        let mut child_stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            drop(pipe.read_to_string(&mut child_stderr));
        }
        let holder = recorded_pid(&holder_path);
        let holder_gone =
            holder.is_none_or(|pid| gone_within(pid, std::time::Duration::from_secs(3)));
        // Clean up whatever the give-up left, so a red assertion below leaves
        // no holder behind.
        kill_recorded_pid(&holder_path);
        assert!(
            finished,
            "the child run did not finish within 15 s\n--- child stderr\n{child_stderr}"
        );
        assert!(
            status.success(),
            "child run failed ({status})\n--- child stderr\n{child_stderr}"
        );
        let elapsed: f64 = std::fs::read_to_string(&elapsed_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let bound = crate::installer::install::KILL_GRACE + std::time::Duration::from_secs(2);
        assert!(
            elapsed < bound.as_secs_f64(),
            "the write waited for a process holding sudo's stderr: {elapsed:.3} s"
        );
        assert_eq!(
            std::fs::read_to_string(&result).unwrap(),
            "sudo tee failed",
            "the failure did not name sudo's failed exit"
        );
        assert!(
            holder_gone,
            "the fake's stderr holder is still running after the write returned"
        );
    }

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
            "#!/bin/sh\nif [ \"$1\" = -n ]; then shift; fi\n\
             printf '%s\\n' \"$@\" > \"$PU_TEST_SUDO_LOG\"\nexit 1\n",
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
            "#!/bin/sh\nif [ \"$1\" = -n ]; then shift; fi\n\
             printf '%s\\n' \"$@\" > \"$PU_TEST_EXCHANGE_LOG\"\n\
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

    /// A `--system` directory copy dereferences only the top-level link, the
    /// way the user-mode copy does: a component's own path may be a link,
    /// while a link inside the package must stay a link.
    #[test]
    fn copy_dir_as_root_follows_only_the_top_level_link() {
        use std::os::unix::fs::PermissionsExt;
        use std::path::PathBuf;

        if is_root() {
            return; // root takes the direct `copy_dir_recursive` branch
        }

        if std::env::var_os("PU_TEST_COPY_DIR_CHILD").is_some() {
            let root = PathBuf::from(std::env::var("PU_TEST_COPY_DIR_ROOT").unwrap());
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

            let dest = root.join("backup");
            copy_dir_as_root(&root.join("linked"), &dest).unwrap();

            assert!(
                !std::fs::symlink_metadata(&dest)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "the top-level link was recreated as a link"
            );
            assert_eq!(
                std::fs::read_to_string(dest.join("file.txt")).unwrap(),
                "kept"
            );
            assert!(
                std::fs::symlink_metadata(dest.join("inner"))
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "a link inside the package was followed"
            );
            std::fs::write(root.join("done"), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake_sudo = bin.join("sudo");
        std::fs::write(
            &fake_sudo,
            "#!/bin/sh\nif [ \"$1\" = -n ]; then shift; fi\n\
             printf '%s\\n' \"$*\" >> \"$PU_TEST_SUDO_LOG\"\nexec \"$@\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake_sudo, std::fs::Permissions::from_mode(0o755)).unwrap();

        let real = root.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("file.txt"), b"kept").unwrap();
        let outside = root.join("outside.txt");
        std::fs::write(&outside, b"outside").unwrap();
        std::os::unix::fs::symlink(&outside, real.join("inner")).unwrap();
        std::os::unix::fs::symlink(&real, root.join("linked")).unwrap();

        let sudo_log = root.join("sudo.log");
        let path = OsString::from(format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ));
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "installer::privilege::tests::copy_dir_as_root_follows_only_the_top_level_link",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", path)
            .env("PU_TEST_SUDO_LOG", &sudo_log)
            .env("PU_TEST_COPY_DIR_CHILD", "1")
            .env("PU_TEST_COPY_DIR_ROOT", &root)
            .env("PU_TEST_FAKE_SUDO", &fake_sudo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child run failed: {}\n--- stdout\n{}\n--- stderr\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );

        let log = std::fs::read_to_string(&sudo_log).unwrap();
        assert!(
            log.lines().any(|line| line.starts_with("cp -rfT")),
            "the copy did not run through sudo: {log}"
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
            "#!/bin/sh\nif [ \"$1\" = -n ]; then shift; fi\n\
             printf '%s\\n' \"$@\" > \"$PU_TEST_SUDO_LOG\"\nexec \"$@\"\n",
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
    fn system_paths_need_sudo_when_not_root() {
        if is_root() {
            return;
        }
        assert!(needs_sudo(Path::new("/usr/share/plasma/test")));
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

    /// The `--system` base and every level below it get the mode on the levels
    /// the run creates, whatever the umask: `mkdir -p -m` alone gives the mode
    /// only to the directory it names last, and a 027/077 umask leaves the
    /// intermediate levels unsearchable for the invoking user.
    #[test]
    fn create_dir_all_as_root_sets_every_created_levels_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::path::PathBuf;

        if is_root() {
            return; // root takes the direct `create_dir` branch
        }

        if std::env::var_os("PU_TEST_MKDIR_CHILD").is_some() {
            let root = PathBuf::from(std::env::var("PU_TEST_MKDIR_ROOT").unwrap());
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

            let base = root.join("var/lib/plasmoid-updater/backups");
            create_dir_all_as_root(&base, 0o755).unwrap();

            for level in [
                root.join("var"),
                root.join("var/lib"),
                root.join("var/lib/plasmoid-updater"),
                root.join("var/lib/plasmoid-updater/backups"),
            ] {
                let mode = std::fs::metadata(&level).unwrap().permissions().mode() & 0o7777;
                assert_eq!(mode, 0o755, "{} took the umask", level.display());
            }
            std::fs::write(root.join("done"), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake_sudo = bin.join("sudo");
        std::fs::write(
            &fake_sudo,
            "#!/bin/sh\nif [ \"$1\" = -n ]; then shift; fi\numask 077\n\
             printf '%s\\n' \"$*\" >> \"$PU_TEST_SUDO_LOG\"\nexec \"$@\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake_sudo, std::fs::Permissions::from_mode(0o755)).unwrap();

        let log = root.join("sudo.log");
        let path = OsString::from(format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ));
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "installer::privilege::tests::create_dir_all_as_root_sets_every_created_levels_mode",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", path)
            .env("PU_TEST_SUDO_LOG", &log)
            .env("PU_TEST_MKDIR_CHILD", "1")
            .env("PU_TEST_MKDIR_ROOT", &root)
            .env("PU_TEST_FAKE_SUDO", &fake_sudo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child failed: {}\n--- stdout\n{}\n--- stderr\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    /// A non-UTF-8 path survives the sudo crossing: `to_string_lossy` would
    /// turn the name into U+FFFD, so the copy would read or write a different
    /// name than the `PathBuf` the caller handed in.
    #[test]
    fn a_non_utf8_path_survives_the_sudo_crossing() {
        use std::os::unix::ffi::OsStringExt as _;
        use std::os::unix::fs::PermissionsExt as _;
        use std::path::PathBuf;

        if is_root() {
            return; // root takes the direct `fs::copy` branch
        }

        if std::env::var_os("PU_TEST_NONUTF8_CHILD").is_some() {
            let root = PathBuf::from(std::env::var("PU_TEST_NONUTF8_ROOT").unwrap());
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

            let mut src_name = b"bad-".to_vec();
            src_name.push(0xFF);
            src_name.extend_from_slice(b"-name");
            let mut dest_name = b"dest-".to_vec();
            dest_name.push(0xFE);
            let src = root.join(std::ffi::OsString::from_vec(src_name.clone()));
            let dest = root.join(std::ffi::OsString::from_vec(dest_name.clone()));
            std::fs::write(&src, b"kept").unwrap();

            copy_file_as_root(&src, &dest).unwrap();

            assert_eq!(
                std::fs::read(&dest).unwrap(),
                b"kept",
                "the copy did not land under the non-UTF-8 destination name"
            );
            let log = std::fs::read(root.join("sudo.log")).unwrap();
            assert!(
                log.contains(&0xFF),
                "the sudo argv lost the non-UTF-8 byte: {:?}",
                String::from_utf8_lossy(&log)
            );
            std::fs::write(root.join("done"), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake_sudo = bin.join("sudo");
        std::fs::write(
            &fake_sudo,
            "#!/bin/sh\nif [ \"$1\" = -n ]; then shift; fi\n\
             printf '%s\\n' \"$*\" >> \"$PU_TEST_SUDO_LOG\"\nexec \"$@\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake_sudo, std::fs::Permissions::from_mode(0o755)).unwrap();

        let log = root.join("sudo.log");
        let path = OsString::from(format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ));
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "installer::privilege::tests::a_non_utf8_path_survives_the_sudo_crossing",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", path)
            .env("PU_TEST_SUDO_LOG", &log)
            .env("PU_TEST_NONUTF8_CHILD", "1")
            .env("PU_TEST_NONUTF8_ROOT", &root)
            .env("PU_TEST_FAKE_SUDO", &fake_sudo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child failed: {}\n--- stdout\n{}\n--- stderr\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
