//! Black-box tests of the `plasmoid-updater` binary: exit codes, the
//! stdout/stderr split and logging, run in a sandboxed environment against
//! the local fake KDE Store the library tests use.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../libplasmoid-updater/tests/common/mod.rs"]
mod common;

use std::process::Stdio;

use common::{CatalogWidget, FakeStore, Listing, OLD_CONTENT, OLD_VERSION, SCHEME_NAME, Sandbox};

const API_URL_VAR: &str = "PLASMOID_UPDATER_API_URL";

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

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_plasmoid-updater")
}

/// Runs the binary in `sandbox` against `api_url` with stdin closed, so every
/// run is non-interactive.
fn run(sandbox: &Sandbox, api_url: &str, args: &[&str]) -> Run {
    let output = sandbox
        .command(bin())
        .env(API_URL_VAR, api_url)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    Run {
        code: output.status.code().expect("killed by a signal"),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn pending_update() -> (Sandbox, FakeStore) {
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    (sandbox, FakeStore::start(Listing::default()))
}

fn write_config(sandbox: &Sandbox, toml: &str) {
    std::fs::write(sandbox.config_dir().join("plasmoid-updater.toml"), toml).unwrap();
}

fn has_word(text: &str, word: &str) -> bool {
    text.split(|c: char| c.is_whitespace() || c == '\'' || c == '"' || c == '`')
        .any(|token| token == word)
}

#[test]
fn check_exits_0_when_everything_is_up_to_date() {
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing {
        version: OLD_VERSION,
        ..Listing::default()
    });
    let run = run(&sandbox, store.url(), &["--skip-plasma-detection", "check"]);
    assert_eq!(run.code, 0, "{run}");
}

#[test]
fn check_exits_100_when_an_update_is_available() {
    let (sandbox, store) = pending_update();
    let run = run(&sandbox, store.url(), &["--skip-plasma-detection", "check"]);
    assert_eq!(run.code, 100, "{run}");
    assert!(run.stdout.contains(SCHEME_NAME), "{run}");
}

#[test]
fn check_exits_1_and_names_the_component_when_its_lookup_fails() {
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing {
        in_catalog: false,
        per_id_fails: true,
        ..Listing::default()
    });
    let run = run(&sandbox, store.url(), &["--skip-plasma-detection", "check"]);
    assert_eq!(run.code, 1, "{run}");
    assert!(run.stderr.contains(SCHEME_NAME), "{run}");
}

#[test]
fn an_unreachable_store_exits_3() {
    let (sandbox, _store) = pending_update();
    let run = run(
        &sandbox,
        &common::dead_store_url(),
        &["--skip-plasma-detection", "check"],
    );
    assert_eq!(run.code, 3, "{run}");
}

#[test]
fn an_unknown_flag_exits_2() {
    let sandbox = Sandbox::new();
    let run = run(&sandbox, &common::dead_store_url(), &["--no-such-flag"]);
    assert_eq!(run.code, 2, "{run}");
}

#[test]
fn an_undetected_plasma_session_exits_3_and_names_the_skip_flag() {
    // Detection looks for the KNewStuff registry dir, which an empty sandbox
    // lacks.
    let sandbox = Sandbox::new();
    let run = run(&sandbox, &common::dead_store_url(), &["check"]);
    assert_eq!(run.code, 3, "{run}");
    assert!(run.stderr.contains("--skip-plasma-detection"), "{run}");
}

#[test]
fn a_closed_stdout_pipe_ends_the_run_without_a_panic() {
    let (sandbox, store) = pending_update();
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let output = sandbox
        .command(bin())
        .env(API_URL_VAR, store.url())
        .args(["--skip-plasma-detection", "list-installed"])
        .stdin(Stdio::null())
        .stdout(writer)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr:\n{stderr}");
    assert!(!has_word(&stderr, "panicked"), "stderr:\n{stderr}");
}

#[test]
fn a_failed_update_exits_1_with_the_failure_on_stderr_only() {
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing {
        md5_override: Some("00000000000000000000000000000000"),
        ..Listing::default()
    });
    let run = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "update", "-y"],
    );
    assert_eq!(run.code, 1, "{run}");
    let failure_lines = |text: &str| {
        text.lines()
            .filter(|line| line.to_ascii_lowercase().contains("checksum"))
            .count()
    };
    assert!(failure_lines(&run.stderr) > 0, "{run}");
    assert_eq!(failure_lines(&run.stdout), 0, "{run}");
    common::assert_content(&sandbox.scheme_path(), OLD_CONTENT);
}

#[test]
fn update_of_an_unknown_component_is_a_usage_error() {
    let (sandbox, store) = pending_update();
    let run = run(
        &sandbox,
        store.url(),
        &[
            "--skip-plasma-detection",
            "update",
            "-y",
            "no.such.component",
        ],
    );
    assert_eq!(run.code, 2, "{run}");
    assert!(run.stderr.contains("no.such.component"), "{run}");
    assert!(run.stdout.is_empty(), "{run}");
}

#[test]
fn update_of_a_component_excluded_by_name_says_so_when_asked_for_by_directory() {
    let (sandbox, store) = pending_update();
    write_config(
        &sandbox,
        &format!("excluded_packages = [\"{SCHEME_NAME}\"]\n"),
    );
    let run = run(
        &sandbox,
        store.url(),
        &[
            "--skip-plasma-detection",
            "update",
            "-y",
            common::SCHEME_FILE,
        ],
    );
    assert!(
        has_word(&run.stdout, "excluded") || has_word(&run.stderr, "excluded"),
        "{run}"
    );
    common::assert_content(&sandbox.scheme_path(), OLD_CONTENT);
}

#[test]
fn update_dry_run_lists_the_update_and_changes_nothing() {
    let (sandbox, store) = pending_update();
    let run = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "update", "-y", "--dry-run"],
    );
    assert_eq!(run.code, 0, "{run}");
    assert!(run.stdout.contains(SCHEME_NAME), "{run}");
    assert!(has_word(&run.stdout, common::NEW_VERSION), "{run}");
    common::assert_content(&sandbox.scheme_path(), OLD_CONTENT);
    assert!(
        sandbox
            .registry_content()
            .contains(&format!("<version>{OLD_VERSION}</version>")),
        "the registry moved on a dry run"
    );
}

#[test]
fn a_component_with_no_matching_download_is_a_failure_not_a_skip() {
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing {
        link_version: Some("1.5"),
        ..Listing::default()
    });
    let run = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "update", "-y"],
    );
    assert_eq!(run.code, 1, "{run}");
    assert!(run.stdout.contains("1 failed"), "{run}");
    assert!(run.stdout.contains("0 skipped"), "{run}");
    common::assert_content(&sandbox.scheme_path(), OLD_CONTENT);
}

#[test]
fn a_named_component_with_no_matching_download_fails_the_update() {
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing {
        link_version: Some("1.5"),
        ..Listing::default()
    });
    let run = run(
        &sandbox,
        store.url(),
        &[
            "--skip-plasma-detection",
            "update",
            "-y",
            common::SCHEME_FILE,
        ],
    );
    assert_eq!(run.code, 1, "{run}");
    assert!(run.stdout.contains("1 failed"), "{run}");
    assert!(run.stderr.contains(SCHEME_NAME), "{run}");
}

#[test]
fn a_named_component_with_no_matching_download_fails_the_dry_run() {
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing {
        link_version: Some("1.5"),
        ..Listing::default()
    });
    let run = run(
        &sandbox,
        store.url(),
        &[
            "--skip-plasma-detection",
            "update",
            "-y",
            "--dry-run",
            common::SCHEME_FILE,
        ],
    );
    assert_eq!(run.code, 1, "{run}");
    assert!(run.stderr.contains("no matching download"), "{run}");
    assert!(run.stderr.contains(SCHEME_NAME), "{run}");
}

#[test]
fn update_repairs_a_component_a_killed_run_stranded() {
    let sandbox = Sandbox::new();
    let plasmoids = sandbox.data_home().join("plasma/plasmoids");
    let stranded = plasmoids.join(".org.fake.widget.plasmoid-updater-old");
    std::fs::create_dir_all(&stranded).unwrap();
    std::fs::write(
        stranded.join("metadata.json"),
        r#"{"KPlugin": {"Name": "Fake Widget", "Id": "org.fake.widget", "Version": "1.0"}}"#,
    )
    .unwrap();
    let store = FakeStore::start(Listing::default());
    let run = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "update", "-y"],
    );
    assert!(
        plasmoids.join("org.fake.widget/metadata.json").is_file(),
        "the stranded widget was not restored: {run}"
    );
}

/// A component a killed run stranded is repaired by the run that reports it:
/// the check must not still count it skipped or make the run fail.
#[test]
fn an_update_that_repairs_a_stranded_component_does_not_fail_the_run() {
    let sandbox = Sandbox::new();
    let plasmoids = sandbox.data_home().join("plasma/plasmoids");
    let stranded = plasmoids.join(".org.fake.widget.plasmoid-updater-old");
    std::fs::create_dir_all(&stranded).unwrap();
    std::fs::write(
        stranded.join("metadata.json"),
        r#"{"KPlugin": {"Name": "Fake Widget", "Id": "org.fake.widget", "Version": "1.0"}}"#,
    )
    .unwrap();
    let store = FakeStore::start(Listing {
        widget: Some(CatalogWidget {
            id: 424_242,
            name: "Fake Widget",
            type_id: 713,
            version: OLD_VERSION,
        }),
        ..Listing::default()
    });

    let run = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "update", "-y"],
    );

    assert_eq!(run.code, 0, "{run}");
    assert!(!run.stderr.contains("interrupted install"), "{run}");
    assert!(
        plasmoids.join("org.fake.widget/metadata.json").is_file(),
        "{run}"
    );
}

#[test]
fn a_named_update_restores_a_stranded_component() {
    let sandbox = Sandbox::new();
    let plasmoids = sandbox.data_home().join("plasma/plasmoids");
    let stranded = plasmoids.join(".org.fake.widget.plasmoid-updater-old");
    std::fs::create_dir_all(&stranded).unwrap();
    std::fs::write(
        stranded.join("metadata.json"),
        r#"{"KPlugin": {"Name": "Fake Widget", "Id": "org.fake.widget", "Version": "1.0"}}"#,
    )
    .unwrap();
    let store = FakeStore::start(Listing {
        widget: Some(CatalogWidget {
            id: 424_242,
            name: "Fake Widget",
            type_id: 713,
            version: OLD_VERSION,
        }),
        ..Listing::default()
    });

    let run = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "update", "-y", "Fake Widget"],
    );

    assert_eq!(run.code, 0, "{run}");
    assert!(
        plasmoids.join("org.fake.widget/metadata.json").is_file(),
        "the named update did not restore the stranded component: {run}"
    );
}

/// A component whose store entry offers no download link is a failure row; it
/// must not also print a "failed to check" diagnostic line, on the update or
/// on a dry run.
#[test]
fn a_no_matching_download_prints_one_failure_row_not_a_diagnostic() {
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing {
        link_version: Some("1.5"),
        ..Listing::default()
    });

    let update_run = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "update", "-y"],
    );
    assert_eq!(update_run.code, 1, "{update_run}");
    assert!(update_run.stdout.contains("1 failed"), "{update_run}");
    assert!(
        !update_run.stderr.contains("failed to check"),
        "{update_run}"
    );

    let dry_run = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "update", "-y", "--dry-run"],
    );
    assert_eq!(dry_run.code, 1, "{dry_run}");
    assert!(!dry_run.stderr.contains("failed to check"), "{dry_run}");
}

#[test]
fn a_non_interactive_update_reports_each_installed_component_on_stderr() {
    let (sandbox, store) = pending_update();
    let run = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "update", "-y"],
    );
    assert_eq!(run.code, 0, "{run}");
    assert!(run.stderr.contains(SCHEME_NAME), "{run}");
}

#[test]
fn an_invalid_exclude_pattern_warns_on_stderr() {
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing {
        version: OLD_VERSION,
        ..Listing::default()
    });
    write_config(&sandbox, "excluded_patterns = [\"ab(c\"]\n");
    let run = run(&sandbox, store.url(), &["--skip-plasma-detection", "check"]);
    assert_eq!(run.code, 0, "{run}");
    assert!(run.stderr.contains("ab(c"), "{run}");
}

#[test]
fn quiet_drops_warnings_from_stderr() {
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing {
        version: OLD_VERSION,
        ..Listing::default()
    });
    write_config(&sandbox, "excluded_patterns = [\"ab(c\"]\n");
    let run = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "-q", "check"],
    );
    assert_eq!(run.code, 0, "{run}");
    assert!(!run.stderr.contains("ab(c"), "{run}");
}

#[test]
fn verbose_and_debug_flags_are_accepted() {
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing {
        version: OLD_VERSION,
        ..Listing::default()
    });
    for flag in ["-v", "-vv"] {
        let run = run(
            &sandbox,
            store.url(),
            &["--skip-plasma-detection", flag, "check"],
        );
        assert_eq!(run.code, 0, "{flag}: {run}");
    }
}

#[test]
fn completions_print_a_shell_script_on_stdout() {
    let sandbox = Sandbox::new();
    let run = run(
        &sandbox,
        &common::dead_store_url(),
        &["completions", "bash"],
    );
    assert_eq!(run.code, 0, "{run}");
    assert!(run.stdout.contains("complete -F"), "{run}");
    assert!(run.stdout.contains("plasmoid-updater"), "{run}");
}
