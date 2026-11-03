//! Ctrl-C at the prompt as a host sees it: the prompt raises SIGINT in the
//! calling process, so a host that has the signal blocked receives it through
//! `sigwait` and gets an empty selection back instead of dying.
//!
//! The child that calls the prompt is this same test binary, re-run for one
//! test. The parent blocks SIGINT before spawning it, so the child starts with
//! every thread it later creates blocking the signal too.

#![cfg(all(target_os = "linux", feature = "cli"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read as _, Write as _};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use nix::pty::openpty;
use nix::sys::signal::{SigSet, Signal};

use libplasmoid_updater::{AvailableUpdate, ComponentType, InstalledComponent};

const CHILD_VAR: &str = "PLASMOID_UPDATER_SELECT_HOST_CHILD";
const PROMPT_TEXT: &str = "select to apply";
const SELECTION_MARKER: &str = "selection=0";
const SIGNAL_MARKER: &str = "sigint=SIGINT";

fn pending_update() -> AvailableUpdate {
    AvailableUpdate::builder(
        InstalledComponent::new(
            "Fake Scheme".to_string(),
            "FakeScheme.colors".to_string(),
            ComponentType::ColorScheme,
            std::path::PathBuf::from("/tmp/test"),
        )
        .with_version("1.0"),
        1,
        "2.0",
    )
    .build()
}

/// The blocking set every thread of this process and its children carry here.
fn sigint_set() -> SigSet {
    let mut set = SigSet::empty();
    set.add(Signal::SIGINT);
    set
}

#[test]
fn ctrl_c_reaches_a_host_that_blocks_sigint() {
    // The child inherits this thread's mask, which is the state a host that
    // consumes SIGINT with `sigwait` runs in.
    sigint_set().thread_block().unwrap();

    let pty = openpty(None, None).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "blocked_sigint_host", "--nocapture"])
        .env(CHILD_VAR, "1")
        .env("TERM", "xterm-256color")
        .stdin(Stdio::from(pty.slave.try_clone().unwrap()))
        .stdout(Stdio::from(pty.slave.try_clone().unwrap()))
        .stderr(Stdio::from(pty.slave.try_clone().unwrap()))
        .spawn()
        .unwrap();

    let mut master = std::fs::File::from(pty.master);
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

    let screen_text = || String::from_utf8_lossy(&screen.lock().unwrap()).into_owned();
    wait_for(&screen_text, PROMPT_TEXT);
    master.write_all(b"\x03").unwrap();

    wait_for_exit(&mut child, &screen_text);
    let output = screen_text();
    assert!(
        output.contains(SELECTION_MARKER),
        "the host did not get an empty selection back: {output:?}"
    );
    assert!(
        output.contains(SIGNAL_MARKER),
        "the SIGINT was not delivered to the host: {output:?}"
    );
}

fn wait_for(screen: &dyn Fn() -> String, text: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !screen().contains(text) {
        assert!(
            Instant::now() < deadline,
            "{text:?} never appeared; screen:\n{}",
            screen()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_exit(child: &mut Child, screen: &dyn Fn() -> String) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "the host failed: {status:?}; screen:\n{}",
                screen()
            );
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("the host did not end; screen:\n{}", screen());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Runs only as the child of the test above, on a pty, with SIGINT blocked.
#[test]
fn blocked_sigint_host() {
    if std::env::var_os(CHILD_VAR).is_none() {
        return;
    }
    // The mask is inherited from the spawning test; blocking here keeps the
    // child correct when this test is run on its own.
    sigint_set().thread_block().unwrap();

    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(sigint_set().wait());
    });

    let selection = libplasmoid_updater::cli::select(&[pending_update()]);
    let delivered = receiver.recv_timeout(Duration::from_secs(5)).ok();

    println!(
        "selection={}",
        selection
            .map(|selected| selected.len())
            .unwrap_or(usize::MAX)
    );
    println!(
        "sigint={}",
        delivered
            .map(|waited| waited
                .map_or_else(|_| "error".to_string(), |signal| format!("{signal:?}")))
            .unwrap_or_else(|| "none".to_string())
    );
}
