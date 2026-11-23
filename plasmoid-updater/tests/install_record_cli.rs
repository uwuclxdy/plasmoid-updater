//! How the binary reports a component an interrupted install's record holds,
//! run in a sandboxed environment against the local fake KDE Store.

#![cfg(target_os = "linux")]
#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns"
)]

#[path = "../../libplasmoid-updater/tests/common/mod.rs"]
mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;

use common::{FakeStore, Listing, OLD_CONTENT, SCHEME_FILE, Sandbox, assert_content};

const API_URL_VAR: &str = "PLASMOID_UPDATER_API_URL";
const KEPT_LINE: &str =
    "error: failed to check \"Fake Scheme\": interrupted install, its install record ";

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl std::fmt::Display for Run {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "exit {}\n--- stdout\n{}\n--- stderr\n{}",
            self.code, self.stdout, self.stderr
        )
    }
}

fn run(sandbox: &Sandbox, api_url: &str, data_home: Option<&Path>, args: &[&str]) -> Run {
    let mut command = sandbox.command(env!("CARGO_BIN_EXE_plasmoid-updater"));
    command
        .env(API_URL_VAR, api_url)
        .arg("--skip-plasma-detection")
        .args(args)
        .stdin(Stdio::null());
    if let Some(data_home) = data_home {
        command.env("XDG_DATA_HOME", data_home);
    }
    let output = command.output().unwrap();
    Run {
        code: output.status.code().expect("killed by a signal"),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// The fake scheme with a record beside it that names a backup under another
/// cache home's backups, which every run keeps; the store offers an update.
fn kept_record() -> (Sandbox, FakeStore, PathBuf) {
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let other = sandbox
        .home_dir()
        .parent()
        .unwrap()
        .join("other-cache/plasmoid-updater/backups/2020-01-01T00-00-00/color-schemes");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join(SCHEME_FILE), "other\n").unwrap();
    let record = sandbox
        .scheme_path()
        .with_file_name(format!(".{SCHEME_FILE}.plasmoid-updater-backup"));
    std::fs::write(&record, format!("{}\n", other.join(SCHEME_FILE).display())).unwrap();
    (sandbox, FakeStore::start(Listing::default()), record)
}

fn kept_line(run: &Run) -> bool {
    run.stderr.lines().any(|line| {
        line.starts_with(KEPT_LINE)
            && line.contains(".FakeScheme.colors.plasmoid-updater-backup is kept because ")
            && line.ends_with("; check the component, then delete that record to update it")
    })
}

#[test]
fn update_counts_a_component_its_kept_record_holds_once() {
    let (sandbox, store, record) = kept_record();

    let run = run(&sandbox, store.url(), None, &["update", "-y"]);

    assert_eq!(run.code, 1, "{run}");
    assert_eq!(run.stdout.trim_end(), "skipped 1", "{run}");
    assert!(kept_line(&run), "{run}");
    assert!(record.exists());
    assert_content(&sandbox.scheme_path(), OLD_CONTENT);
}

/// The bin repairs before its check and its install repairs again; a record
/// it keeps still draws its warning once in one run.
#[test]
fn update_warns_once_about_the_kept_record_it_holds() {
    let (sandbox, store, record) = kept_record();

    let run = run(&sandbox, store.url(), None, &["update", "-y"]);

    assert_eq!(run.code, 1, "{run}");
    let warned = run
        .stderr
        .lines()
        .filter(|line| line.contains("keeping the install record"))
        .count();
    assert_eq!(warned, 1, "{run}");
    assert!(record.exists());
}

#[test]
fn a_named_update_over_a_kept_record_fails_with_its_line_under_a_dotdot_data_home() {
    let (sandbox, store, record) = kept_record();
    let data = sandbox.data_home().join("../data");

    for args in [
        ["update", "Fake Scheme", "-y"],
        ["update", "Fake Scheme", "--dry-run"],
    ] {
        let run = run(&sandbox, store.url(), Some(&data), &args);
        assert_eq!(run.code, 1, "{args:?}: {run}");
        assert!(kept_line(&run), "{args:?}: {run}");
    }
    assert!(record.exists());
    assert_content(&sandbox.scheme_path(), OLD_CONTENT);
}

#[test]
fn check_json_keeps_its_top_level_keys_beside_a_record_row() {
    let (sandbox, store, _record) = kept_record();

    let run = run(&sandbox, store.url(), None, &["check", "--json"]);

    let document: serde_json::Value = serde_json::from_str(&run.stdout).unwrap();
    let mut keys: Vec<&str> = document
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "available_updates",
            "diagnostics",
            "skipped",
            "unavailable",
            "unmatched"
        ],
        "{run}"
    );
    let reason = document["diagnostics"][0]["reason"].as_str().unwrap_or("");
    assert!(
        reason.starts_with("interrupted install, its install record "),
        "{run}"
    );
}
