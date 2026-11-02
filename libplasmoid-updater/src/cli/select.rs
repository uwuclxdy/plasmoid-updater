// SPDX-License-Identifier: GPL-3.0-or-later

use std::io::{IsTerminal, Write as _};
use std::os::fd::AsFd;
use std::process::{Child, ChildStdin, Command, Stdio};

use inquire::InquireError;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use super::CLEAR_LINE_SEQUENCE;
use crate::{Error, Result, types::AvailableUpdate};

const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
const DISABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004l";

/// The line that tells the watchdog the prompt ended normally.
const DONE_LINE: &[u8] = b"done\n";

/// Runs as its own process beside the prompt: its stderr is the terminal and
/// `$1` the mode that terminal had before the prompt entered raw mode.
///
/// The ready line tells the prompt it may take the terminal over, so the mode
/// can never go raw with nothing left to put it back. The done line means the
/// prompt is over and the process still tends to its own terminal. Anything
/// else, end of input included, means the process died while the prompt held
/// the terminal, so this process puts the terminal back the way it found it.
///
/// A signal that goes to the whole process group reaches this process too, so
/// it ignores the terminating ones: the ignored disposition carries over to the
/// `stty` it runs, and the restore cannot be cut short by the same signal.
/// SIGTTOU stays at its default, so `stty` fails once a shell has taken the
/// terminal back; the escape sequences are written only when it succeeded,
/// which keeps a late restore from writing onto that shell's prompt.
const WATCHDOG_SCRIPT: &str = r#"trap '' HUP INT QUIT TERM
printf 'ready\n'
IFS= read -r line || true
if [ "$line" = done ]; then
    exit 0
fi
if stty "$1" <&2 2>/dev/null; then
    printf '\033[?25h\033[?2004l' >&2
fi
"#;

const READY_LINE: &str = "ready\n";

/// The multi-select prompt over available updates.
///
/// Every offered update is pre-checked; the caller filters the list it passes
/// if it wants fewer options offered. Escape returns an empty selection, which
/// the caller can treat as "change nothing".
///
/// Ctrl-C puts the terminal back as the prompt found it and raises `SIGINT` in
/// this process. The process ends by that signal unless the caller handles it,
/// in which case an empty selection is returned. A signal that kills the
/// process while the prompt is open ends it by that signal's own status, and a
/// watchdog process restores the terminal the prompt had taken over, even when
/// the signal went to the whole process group. The watchdog exits untouched
/// once the prompt returns.
///
/// The prompt needs `sh` and `stty` on `PATH` while it is open.
///
/// # Errors
///
/// Returns [`Error::Config`] when `updates` is non-empty and stdin is not a
/// terminal (the prompt would otherwise hang a non-interactive caller), when
/// the terminal watchdog cannot start (the prompt is not opened without it),
/// and when the prompt itself fails. An empty `updates` returns an empty
/// selection without touching the terminal.
pub fn select(updates: &[AvailableUpdate]) -> Result<Vec<AvailableUpdate>> {
    if updates.is_empty() {
        return Ok(Vec::new());
    }

    if !std::io::stdin().is_terminal() {
        return Err(Error::Config(
            "interactive selection requires a terminal on stdin; \
             pass a preselected list to `install` instead"
                .into(),
        ));
    }

    let mut watchdog = Watchdog::start().map_err(|e| {
        Error::Config(format!(
            "failed to start the prompt's terminal watchdog: {e}"
        ))
    })?;

    let options = format_menu_options(updates);
    let prompt = format!(
        "{} update{} available, select to apply:",
        updates.len(),
        if updates.len() == 1 { "" } else { "s" }
    );

    match inquire::MultiSelect::new(&prompt, options)
        .with_default(&(0..updates.len()).collect::<Vec<usize>>())
        .with_page_size(15)
        .raw_prompt()
    {
        Ok(selected) => {
            let mut stderr = std::io::stderr();
            if stderr.is_terminal() {
                let _ = write!(stderr, "{CLEAR_LINE_SEQUENCE}");
                let _ = stderr.flush();
            }
            Ok(selected
                .into_iter()
                .map(|opt| updates[opt.index].clone())
                .collect())
        }
        // Escape means "change nothing".
        Err(InquireError::OperationCanceled) => Ok(Vec::new()),
        // The prompt has already left raw mode by the time it reports Ctrl-C,
        // so handing back the terminal and the watchdog is all that is left
        // before the run ends the way a real SIGINT would.
        Err(InquireError::OperationInterrupted) => {
            watchdog.finish();
            // Process-directed, so a host that blocks SIGINT and consumes it
            // on another thread still sees the interrupt; a thread-directed
            // raise would stay pending on this thread and be lost to it.
            if let Err(e) = kill(Pid::this(), Signal::SIGINT) {
                log::error!("could not raise SIGINT: {e}");
            }
            Ok(Vec::new())
        }
        Err(e) => Err(Error::Config(format!("selection prompt failed: {e}"))),
    }
}

/// A process that owns the terminal's pre-prompt mode and the only write end of
/// a pipe, and restores the terminal when this process dies holding it.
///
/// Ending normally means telling it the prompt is over. Dropping it does that
/// on every path out of the prompt, a panic included, and shows the cursor
/// again, which inquire does not.
struct Watchdog {
    child: Child,
    pipe: Option<ChildStdin>,
}

impl Watchdog {
    fn start() -> std::io::Result<Self> {
        let terminal = std::io::stdin().as_fd().try_clone_to_owned()?;
        let mode = saved_mode(&terminal)?;
        let child = Command::new("sh")
            .arg("-c")
            .arg(WATCHDOG_SCRIPT)
            .arg("plasmoid-updater-watchdog")
            .arg(&mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(terminal))
            .spawn()?;
        // `Stdio::piped` keeps both pipe ends close-on-exec, so no other child
        // of this process can hold them open.
        Self::from_child(child)
    }

    fn from_child(mut child: Child) -> std::io::Result<Self> {
        match ready_pipe(&mut child) {
            Ok(pipe) => Ok(Self {
                child,
                pipe: Some(pipe),
            }),
            // Nothing may outlive a watchdog that never announced itself.
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(error)
            }
        }
    }

    /// Shows the cursor, tells the watchdog the prompt ended normally, and
    /// reaps it. Safe to call more than once.
    fn finish(&mut self) {
        let mut stderr = std::io::stderr();
        let _ = stderr.write_all(SHOW_CURSOR);
        let _ = stderr.write_all(DISABLE_BRACKETED_PASTE);
        let _ = stderr.flush();
        if let Some(mut pipe) = self.pipe.take() {
            let _ = pipe.write_all(DONE_LINE);
            let _ = pipe.flush();
        }
        let _ = self.child.wait();
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Waits for the watchdog's ready line and returns the pipe that later tells it
/// the prompt is over.
fn ready_pipe(child: &mut Child) -> std::io::Result<ChildStdin> {
    let pipe = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("the terminal watchdog has no input pipe"))?;
    let ready = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("the terminal watchdog has no ready pipe"))?;
    let mut ready = std::io::BufReader::new(ready);
    let mut line = String::new();
    std::io::BufRead::read_line(&mut ready, &mut line)?;
    if line != READY_LINE {
        return Err(std::io::Error::other(
            "the terminal watchdog did not report that it is ready",
        ));
    }
    Ok(pipe)
}

/// The terminal's mode, in the form `stty` accepts back, captured from an
/// inherited descriptor before the prompt changes it: the device can be one the
/// process may use but cannot reopen by path, such as a terminal it was handed
/// that another user owns.
fn saved_mode(terminal: &impl AsFd) -> std::io::Result<String> {
    let output = Command::new("stty")
        .arg("--save")
        .stdin(Stdio::from(terminal.as_fd().try_clone_to_owned()?))
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "stty could not read the terminal mode: {} ({})",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let mode = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if mode.is_empty() {
        return Err(std::io::Error::other("stty reported no terminal mode"));
    }
    Ok(mode)
}

fn format_menu_options(updates: &[AvailableUpdate]) -> Vec<String> {
    let name_width = updates
        .iter()
        .map(|u| u.installed.name.len())
        .max()
        .unwrap_or(10)
        .max(10);

    updates
        .iter()
        .map(|u| {
            format!(
                "{:<name_width$} {} \u{2192} {}",
                u.installed.name,
                super::output::format_version(&u.installed.version),
                super::output::format_version(&u.latest_version)
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;
    use crate::types::{ComponentType, InstalledComponent};

    fn update(name: &str, from: &str, to: &str) -> AvailableUpdate {
        AvailableUpdate {
            installed: InstalledComponent {
                name: name.to_string(),
                directory_name: name.to_string(),
                version: from.to_string(),
                component_type: ComponentType::PlasmaWidget,
                path: std::path::PathBuf::from("/tmp/test"),
                is_system: false,
                release_date: String::new(),
            },
            content_id: 1,
            latest_version: to.to_string(),
            download_url: String::new(),
            store_url: String::new(),
            release_date: String::new(),
            checksum: None,
            download_size: None,
        }
    }

    #[test]
    fn menu_options_align_names_and_show_both_versions() {
        let options =
            format_menu_options(&[update("B", "1.0", "2.0"), update("Aaa", "3.0", "4.0")]);
        assert_eq!(options.len(), 2);
        // names pad to the 10-char minimum width
        assert!(
            options[0].starts_with("B          1.0 \u{2192} 2.0"),
            "{}",
            options[0]
        );
        assert!(
            options[1].starts_with("Aaa        3.0 \u{2192} 4.0"),
            "{}",
            options[1]
        );
    }

    /// The mode is read from the descriptor the prompt will put in raw mode,
    /// not by reopening the device: a terminal handed to this process can be
    /// one whose path it may not open.
    #[test]
    fn the_mode_is_read_through_the_inherited_descriptor() {
        let pty = nix::pty::openpty(None, None).expect("a pty");
        let device = std::fs::read_link(format!("/proc/self/fd/{}", pty.slave.as_raw_fd()))
            .expect("the pty's device path");
        std::fs::set_permissions(&device, std::fs::Permissions::from_mode(0o200))
            .expect("this process owns the pty");
        assert!(
            std::fs::File::open(&device).is_err(),
            "the device is still openable by path: {}",
            device.display()
        );

        let mode = saved_mode(&pty.slave).expect("the mode of the inherited terminal");
        assert!(!mode.is_empty());
    }

    /// A watchdog that never reports ready leaves no process behind, neither
    /// running nor as a zombie.
    #[test]
    fn a_watchdog_that_never_reports_ready_is_killed_and_reaped() {
        let child = Command::new("sh")
            .args(["-c", "printf 'nope\\n'; exec sleep 30"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("a shell to stand in for the watchdog");
        let pid = child.id();

        assert!(Watchdog::from_child(child).is_err());

        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "the watchdog was left behind (pid {pid})"
        );
    }
}
