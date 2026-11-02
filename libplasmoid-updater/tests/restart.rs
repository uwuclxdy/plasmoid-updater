//! `restart_plasmashell` reports a `systemctl` that cannot be spawned as a
//! missing tool, not as an I/O error about a path.
//!
//! The child-process pattern is the sandbox harness's: clearing `PATH` in this
//! process would need `unsafe`, so the test re-runs itself with a `PATH`
//! holding no `systemctl`.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::Sandbox;

const CHILD_VAR: &str = "PLASMOID_UPDATER_TEST_RESTART_CHILD";
const DONE_VAR: &str = "PLASMOID_UPDATER_TEST_RESTART_DONE";

#[test]
fn a_missing_systemctl_is_a_missing_tool() {
    if std::env::var_os(CHILD_VAR).is_some() {
        let err = libplasmoid_updater::restart_plasmashell().unwrap_err();
        let done = std::path::PathBuf::from(std::env::var_os(DONE_VAR).unwrap());
        std::fs::write(&done, b"ok").unwrap();
        match err {
            libplasmoid_updater::Error::MissingTool { tool } => assert_eq!(tool, "systemctl"),
            other => panic!("expected MissingTool for a spawn failure, got {other:?}"),
        }
        return;
    }

    let sandbox = Sandbox::new();
    let empty_path = sandbox.data_home().join("empty-path");
    std::fs::create_dir_all(&empty_path).unwrap();
    let done = sandbox.data_home().join("restart-done");
    let output = sandbox
        .command(std::env::current_exe().unwrap())
        .args([
            "a_missing_systemctl_is_a_missing_tool",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_VAR, "1")
        .env(DONE_VAR, &done)
        .env("PATH", &empty_path)
        .output()
        .unwrap();
    assert!(
        output.status.success() && done.exists(),
        "child run failed (body completed: {})\n--- stdout\n{}\n--- stderr\n{}",
        done.exists(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
