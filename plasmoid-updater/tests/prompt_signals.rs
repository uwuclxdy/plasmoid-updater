//! The selection prompt on a real pseudo-terminal: whatever ends the prompt,
//! the terminal it put in raw mode is handed back as it was found.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../libplasmoid-updater/tests/common/mod.rs"]
mod common;

use std::io::{Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::process::{Child, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::pty::openpty;
use nix::sys::signal::{Signal, kill, killpg};
use nix::sys::termios::{LocalFlags, tcgetattr};
use nix::unistd::Pid;

use common::{FakeStore, Listing, OLD_CONTENT, Sandbox};

const API_URL_VAR: &str = "PLASMOID_UPDATER_API_URL";
const PROMPT_TEXT: &str = "select to apply";
const SHOW_CURSOR: &str = "\x1b[?25h";
const BRACKETED_PASTE_OFF: &str = "\x1b[?2004l";

/// The line-discipline flags a raw-mode prompt turns off.
fn cooked_flags(slave: &OwnedFd) -> LocalFlags {
    tcgetattr(slave).unwrap().local_flags
        & (LocalFlags::ECHO | LocalFlags::ICANON | LocalFlags::ISIG)
}

struct Session {
    child: Child,
    master: std::fs::File,
    slave: OwnedFd,
    screen: Arc<Mutex<Vec<u8>>>,
    _sandbox: Sandbox,
    _store: FakeStore,
}

impl Session {
    /// Starts an interactive `update` on a fresh pty, against a store holding
    /// one pending update, and waits until the selection prompt is drawn.
    fn at_the_prompt() -> Self {
        Self::at_the_prompt_of(Listing::default())
    }

    fn at_the_prompt_of(listing: Listing) -> Self {
        let sandbox = Sandbox::new();
        sandbox.install_old_scheme();
        let store = FakeStore::start(listing);
        let pty = openpty(None, None).unwrap();
        let child = sandbox
            .command(env!("CARGO_BIN_EXE_plasmoid-updater"))
            .env(API_URL_VAR, store.url())
            .env("TERM", "xterm-256color")
            .args(["--skip-plasma-detection", "update"])
            .stdin(Stdio::from(pty.slave.try_clone().unwrap()))
            .stdout(Stdio::from(pty.slave.try_clone().unwrap()))
            .stderr(Stdio::from(pty.slave.try_clone().unwrap()))
            // Its own process group, so a test can signal the whole group the
            // way a terminal hangup or `kill -- -<pgid>` does.
            .process_group(0)
            .spawn()
            .unwrap();
        let master = std::fs::File::from(pty.master);
        let screen = Arc::new(Mutex::new(Vec::new()));
        let mut reader = master.try_clone().unwrap();
        let sink = Arc::clone(&screen);
        std::thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buffer) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buffer[..n]);
            }
        });
        let session = Self {
            child,
            master,
            slave: pty.slave,
            screen,
            _sandbox: sandbox,
            _store: store,
        };
        session.wait_for_screen(PROMPT_TEXT);
        session
    }

    fn screen(&self) -> String {
        String::from_utf8_lossy(&self.screen.lock().unwrap()).into_owned()
    }

    /// How many bytes the pty has shown so far.
    fn screen_len(&self) -> usize {
        self.screen.lock().unwrap().len()
    }

    /// What the pty showed after the first `since` bytes.
    fn screen_since(&self, since: usize) -> String {
        String::from_utf8_lossy(&self.screen.lock().unwrap()[since..]).into_owned()
    }

    fn wait_for_screen(&self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !self.screen().contains(text) {
            assert!(
                Instant::now() < deadline,
                "{text:?} never appeared; screen:\n{}",
                self.screen()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                panic!("the run did not end; screen:\n{}", self.screen());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Asserts the pty is back in the mode a fresh one starts in and that,
    /// after the first `since` bytes (the screen at the moment the prompt was
    /// ended), the cursor was shown and bracketed paste turned off: an idle
    /// prompt's own redraws show the cursor too, so only output written after
    /// the end proves the restore.
    ///
    /// A run killed at the prompt cannot restore the terminal itself; the
    /// process that does sees the death after the parent can, so the check
    /// waits up to two seconds for the restore rather than reading the mode
    /// at the instant the exit is reaped.
    fn assert_terminal_restored(&self, baseline: LocalFlags, since: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while cooked_flags(&self.slave) != baseline && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            cooked_flags(&self.slave),
            baseline,
            "the terminal was left in raw mode"
        );
        // The escape sequences follow the mode restore at once; the pause lets
        // the reader thread drain them from the pty.
        std::thread::sleep(Duration::from_millis(100));
        let after = self.screen_since(since);
        assert!(
            after.contains(SHOW_CURSOR),
            "the cursor was not shown after the prompt ended: {after:?}"
        );
        assert!(
            after.contains(BRACKETED_PASTE_OFF),
            "bracketed paste was not turned off after the prompt ended: {after:?}"
        );
    }
}

fn fresh_pty_flags() -> LocalFlags {
    let pty = openpty(None, None).unwrap();
    cooked_flags(&pty.slave)
}

fn signal_at_the_prompt(signal: Signal) {
    let baseline = fresh_pty_flags();
    let mut session = Session::at_the_prompt();
    let since = session.screen_len();
    kill(
        Pid::from_raw(i32::try_from(session.child.id()).unwrap()),
        signal,
    )
    .unwrap();

    let status = session.wait();

    session.assert_terminal_restored(baseline, since);
    assert_eq!(
        status.signal(),
        Some(signal as i32),
        "the run did not end by {signal}: {status:?}"
    );
    common::assert_content(&session._sandbox.scheme_path(), OLD_CONTENT);
}

#[test]
fn sigterm_at_the_prompt_restores_the_terminal() {
    signal_at_the_prompt(Signal::SIGTERM);
}

#[test]
fn sighup_at_the_prompt_restores_the_terminal() {
    signal_at_the_prompt(Signal::SIGHUP);
}

#[test]
fn sigquit_at_the_prompt_restores_the_terminal() {
    signal_at_the_prompt(Signal::SIGQUIT);
}

#[test]
fn sigint_at_the_prompt_restores_the_terminal() {
    signal_at_the_prompt(Signal::SIGINT);
}

/// A signal to the whole process group, as a terminal hangup or
/// `kill -- -<pgid>` sends, reaches everything the run started.
#[test]
fn sigterm_to_the_process_group_at_the_prompt_restores_the_terminal() {
    let baseline = fresh_pty_flags();
    let mut session = Session::at_the_prompt();
    let since = session.screen_len();
    killpg(
        Pid::from_raw(i32::try_from(session.child.id()).unwrap()),
        Signal::SIGTERM,
    )
    .unwrap();

    let status = session.wait();

    session.assert_terminal_restored(baseline, since);
    assert_eq!(
        status.signal(),
        Some(Signal::SIGTERM as i32),
        "the run did not end by SIGTERM: {status:?}"
    );
    common::assert_content(&session._sandbox.scheme_path(), OLD_CONTENT);
}

#[test]
fn ctrl_c_at_the_prompt_ends_the_run_as_interrupted() {
    let baseline = fresh_pty_flags();
    let mut session = Session::at_the_prompt();
    let since = session.screen_len();
    session.master.write_all(b"\x03").unwrap();

    let status = session.wait();

    session.assert_terminal_restored(baseline, since);
    assert_eq!(
        status.signal(),
        Some(Signal::SIGINT as i32),
        "Ctrl-C did not end the run as interrupted: {status:?}; screen:\n{}",
        session.screen()
    );
    common::assert_content(&session._sandbox.scheme_path(), OLD_CONTENT);
}

/// Once the prompt has closed, a signal must end the run the way it would
/// have without the prompt: nothing the prompt set up may outlive it.
fn signal_after_the_prompt_closed(signal: Signal) {
    let mut session = Session::at_the_prompt_of(Listing {
        download_stalls: true,
        ..Listing::default()
    });
    // Enter accepts the preselected update, so the run moves on to a download
    // the store holds open.
    session.master.write_all(b"\r").unwrap();
    session.wait_for_screen("Fake Scheme");
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        session.child.try_wait().unwrap().is_none(),
        "the run ended before the signal; screen:\n{}",
        session.screen()
    );
    kill(
        Pid::from_raw(i32::try_from(session.child.id()).unwrap()),
        signal,
    )
    .unwrap();

    let status = session.wait();

    assert_eq!(
        status.signal(),
        Some(signal as i32),
        "the run did not end by {signal} after the prompt: {status:?}"
    );
}

#[test]
fn sigterm_after_the_prompt_closed_still_ends_the_run() {
    signal_after_the_prompt_closed(Signal::SIGTERM);
}

#[test]
fn sigint_after_the_prompt_closed_still_ends_the_run() {
    signal_after_the_prompt_closed(Signal::SIGINT);
}

#[test]
fn escape_at_the_prompt_selects_nothing_and_exits_0() {
    let baseline = fresh_pty_flags();
    let mut session = Session::at_the_prompt();
    let since = session.screen_len();
    session.master.write_all(b"\x1b").unwrap();

    let status = session.wait();

    session.assert_terminal_restored(baseline, since);
    assert_eq!(status.code(), Some(0), "screen:\n{}", session.screen());
    common::assert_content(&session._sandbox.scheme_path(), OLD_CONTENT);
}
