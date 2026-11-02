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
fn a_bare_run_prints_help_and_installs_nothing() {
    let (sandbox, store) = pending_update();
    let run = run(&sandbox, store.url(), &[]);
    assert_eq!(run.code, 0, "{run}");
    assert!(run.stdout.contains("Usage:"), "{run}");
    common::assert_content(&sandbox.scheme_path(), OLD_CONTENT);
}

#[test]
fn update_without_yes_on_a_non_tty_stdin_is_refused_naming_yes() {
    let (sandbox, store) = pending_update();
    let run = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "update"],
    );
    assert_eq!(run.code, 2, "{run}");
    assert!(run.stderr.contains("-y"), "{run}");
    common::assert_content(&sandbox.scheme_path(), OLD_CONTENT);
}

#[test]
fn update_of_a_named_component_on_a_non_tty_stdin_needs_no_yes() {
    let (sandbox, store) = pending_update();
    let run = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "update", common::SCHEME_FILE],
    );
    assert_eq!(run.code, 0, "{run}");
    common::assert_content(&sandbox.scheme_path(), common::NEW_CONTENT);
}

#[test]
fn assume_yes_in_the_config_still_installs_on_a_non_tty_stdin() {
    let (sandbox, store) = pending_update();
    write_config(&sandbox, "assume_yes = true\n");
    let run = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "update"],
    );
    assert_eq!(run.code, 0, "{run}");
    common::assert_content(&sandbox.scheme_path(), common::NEW_CONTENT);
}

#[test]
fn update_all_by_default_in_the_config_still_installs_on_a_non_tty_stdin() {
    let (sandbox, store) = pending_update();
    write_config(&sandbox, "update_all_by_default = true\n");
    let run = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "update"],
    );
    assert_eq!(run.code, 0, "{run}");
    common::assert_content(&sandbox.scheme_path(), common::NEW_CONTENT);
}

#[test]
fn restart_and_no_restart_flags_conflict() {
    let (sandbox, store) = pending_update();
    let run = run(
        &sandbox,
        store.url(),
        &[
            "--skip-plasma-detection",
            "update",
            "-y",
            "--restart-plasma",
            "--no-restart-plasma",
        ],
    );
    assert_eq!(run.code, 2, "{run}");
    common::assert_content(&sandbox.scheme_path(), OLD_CONTENT);
}

/// A fake `sudo` that records each call in `sudo-calls` beside it and fails,
/// so a read-only command that authenticates is caught either way.
fn fake_sudo_dir(sandbox: &Sandbox) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = sandbox.tmp_dir().join("fake-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let sudo = bin.join("sudo");
    std::fs::write(
        &sudo,
        format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\nexit 1\n",
            bin.join("sudo-calls").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn run_with_fake_sudo(sandbox: &Sandbox, api_url: &str, args: &[&str]) -> (Run, Vec<String>) {
    let bin = fake_sudo_dir(sandbox);
    let mut path = std::ffi::OsString::from(&bin);
    path.push(":");
    path.push(std::env::var_os("PATH").unwrap_or_default());
    let output = sandbox
        .command(self::bin())
        .env("PATH", path)
        .env(API_URL_VAR, api_url)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let run = Run {
        code: output.status.code().expect("killed by a signal"),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    };
    let calls = std::fs::read_to_string(bin.join("sudo-calls"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    (run, calls)
}

#[test]
fn system_check_does_not_authenticate() {
    let (sandbox, store) = pending_update();
    let (run, calls) = run_with_fake_sudo(
        &sandbox,
        store.url(),
        &["--system", "--skip-plasma-detection", "check"],
    );
    assert!(calls.is_empty(), "check ran sudo {calls:?}: {run}");
    assert!(!run.stderr.contains("sudo authentication failed"), "{run}");
}

#[test]
fn system_update_dry_run_does_not_authenticate() {
    let (sandbox, store) = pending_update();
    let (run, calls) = run_with_fake_sudo(
        &sandbox,
        store.url(),
        &[
            "--system",
            "--skip-plasma-detection",
            "update",
            "-y",
            "--dry-run",
        ],
    );
    assert!(
        calls.is_empty(),
        "update --dry-run ran sudo {calls:?}: {run}"
    );
    common::assert_content(&sandbox.scheme_path(), OLD_CONTENT);
}

#[test]
fn system_list_installed_does_not_authenticate() {
    let (sandbox, store) = pending_update();
    let (run, calls) = run_with_fake_sudo(
        &sandbox,
        store.url(),
        &["--system", "--skip-plasma-detection", "list-installed"],
    );
    assert!(calls.is_empty(), "list-installed ran sudo {calls:?}: {run}");
    assert_eq!(run.code, 0, "{run}");
}

#[test]
fn system_update_of_a_named_component_authenticates() {
    let (sandbox, store) = pending_update();
    let (run, calls) = run_with_fake_sudo(
        &sandbox,
        store.url(),
        &[
            "--system",
            "--skip-plasma-detection",
            "update",
            common::SCHEME_FILE,
        ],
    );
    assert_eq!(
        calls,
        ["-v"],
        "a named system update did not authenticate: {run}"
    );
    assert_ne!(run.code, 0, "{run}");
    common::assert_content(&sandbox.scheme_path(), OLD_CONTENT);
}

/// Runs `check` with the config directory pointed at `config_dir`.
fn check_with_config_dir(sandbox: &Sandbox, api_url: &str, config_dir: &std::path::Path) -> Run {
    let output = sandbox
        .command(bin())
        .env(API_URL_VAR, api_url)
        .env("PLASMOID_UPDATER_CONFIG_DIR", config_dir)
        .args(["--skip-plasma-detection", "check"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    Run {
        code: output.status.code().expect("killed by a signal"),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

#[test]
fn a_config_dir_below_a_regular_file_is_a_usage_error_naming_it() {
    let (sandbox, store) = pending_update();
    let file = sandbox.tmp_dir().join("not-a-dir");
    std::fs::write(&file, "").unwrap();
    let config_dir = file.join("sub");
    let run = check_with_config_dir(&sandbox, store.url(), &config_dir);
    assert_eq!(run.code, 2, "{run}");
    assert!(
        run.stderr.contains(&config_dir.display().to_string()),
        "{run}"
    );
}

#[test]
fn a_dangling_config_dir_symlink_is_a_usage_error_naming_it() {
    let (sandbox, store) = pending_update();
    let link = sandbox.tmp_dir().join("dangling-config");
    std::os::unix::fs::symlink(sandbox.tmp_dir().join("missing"), &link).unwrap();
    let run = check_with_config_dir(&sandbox, store.url(), &link);
    assert_eq!(run.code, 2, "{run}");
    assert!(run.stderr.contains(&link.display().to_string()), "{run}");
}

#[test]
fn the_config_dir_usage_error_quotes_a_path_that_needs_escaping() {
    let (sandbox, store) = pending_update();
    let not_a_dir = sandbox.tmp_dir().join("not-a-dir\nname");
    std::fs::write(&not_a_dir, "").unwrap();
    let run = check_with_config_dir(&sandbox, store.url(), &not_a_dir);
    assert_eq!(run.code, 2, "{run}");
    assert!(
        run.stderr.contains(r"not-a-dir\nname"),
        "the path is not quoted in stderr: {run}"
    );
    assert!(
        !run.stderr.contains("not-a-dir\nname"),
        "the raw newline reached stderr: {run}"
    );
}

#[test]
fn a_config_dir_that_is_a_regular_file_is_a_usage_error_naming_it() {
    let (sandbox, store) = pending_update();
    let not_a_dir = sandbox.tmp_dir().join("not-a-dir");
    std::fs::write(&not_a_dir, "").unwrap();
    let output = sandbox
        .command(bin())
        .env(API_URL_VAR, store.url())
        .env("PLASMOID_UPDATER_CONFIG_DIR", &not_a_dir)
        .args(["--skip-plasma-detection", "check"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "stderr:\n{stderr}");
    assert!(
        stderr.contains(&not_a_dir.display().to_string()),
        "stderr:\n{stderr}"
    );
}

/// Runs `--edit-config` with `EDITOR` set to `editor`.
fn edit_config(sandbox: &Sandbox, editor: &str) -> Run {
    let output = sandbox
        .command(bin())
        .env("EDITOR", editor)
        .arg("--edit-config")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    Run {
        code: output.status.code().expect("killed by a signal"),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

#[test]
fn edit_config_fails_when_the_editor_fails() {
    let sandbox = Sandbox::new();
    let run = edit_config(&sandbox, "false");
    assert_ne!(run.code, 0, "{run}");
    assert!(
        has_word(&run.stderr, "false") || run.stderr.contains("\"false\""),
        "{run}"
    );
}

#[test]
fn edit_config_of_a_missing_editor_says_it_was_not_found() {
    let sandbox = Sandbox::new();
    let run = edit_config(&sandbox, "definitely-not-a-real-editor-xyz");
    assert_eq!(run.code, 3, "{run}");
    assert!(
        run.stderr
            .contains("editor 'definitely-not-a-real-editor-xyz' not found on PATH"),
        "{run}"
    );
}

#[test]
fn edit_config_error_names_the_editor_without_repeating_status() {
    let sandbox = Sandbox::new();
    let run = edit_config(&sandbox, "false");
    assert_eq!(run.code, 3, "{run}");
    assert!(
        run.stderr
            .contains("editor 'false' exited with exit status: 1"),
        "{run}"
    );
}

#[test]
fn a_missing_editor_given_by_path_says_it_could_not_be_run() {
    let sandbox = Sandbox::new();
    let run = edit_config(&sandbox, "/opt/plasmoid-updater-no-such-editor");
    assert_eq!(run.code, 3, "{run}");
    assert!(
        run.stderr.contains(
            "editor '/opt/plasmoid-updater-no-such-editor' could not be run \
             (exit 127: not found or not executable)"
        ),
        "{run}"
    );
    assert!(!run.stderr.contains("on PATH"), "{run}");
}

/// `EDITOR` set to an empty string counts as unset, so the default editor runs
/// rather than the shell trying to execute the config file itself.
#[test]
fn a_blank_editor_falls_back_to_the_default_editor() {
    use std::os::unix::fs::PermissionsExt;
    let sandbox = Sandbox::new();
    let bin_dir = sandbox.tmp_dir().join("default-editor-bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let args_file = sandbox.tmp_dir().join("default-editor-args");
    let nano = bin_dir.join("nano");
    std::fs::write(
        &nano,
        format!("#!/bin/sh\necho \"$@\" > '{}'\n", args_file.display()),
    )
    .unwrap();
    std::fs::set_permissions(&nano, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut path = std::ffi::OsString::from(&bin_dir);
    path.push(":");
    path.push(std::env::var_os("PATH").unwrap_or_default());

    let output = sandbox
        .command(bin())
        .env("PATH", path)
        .env("EDITOR", "")
        .arg("--edit-config")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr:\n{stderr}");
    let config = sandbox.config_dir().join("plasmoid-updater.toml");
    let args = std::fs::read_to_string(&args_file).unwrap_or_default();
    assert_eq!(
        args.trim_end(),
        config.display().to_string(),
        "the default editor did not get the config path; stderr:\n{stderr}"
    );
}

#[test]
fn edit_config_splits_an_editor_command_with_arguments() {
    use std::os::unix::fs::PermissionsExt;
    let sandbox = Sandbox::new();
    let editor = sandbox.tmp_dir().join("fake-editor");
    let args_file = sandbox.tmp_dir().join("editor-args");
    std::fs::write(
        &editor,
        format!("#!/bin/sh\necho \"$@\" > '{}'\n", args_file.display()),
    )
    .unwrap();
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap();

    let run = edit_config(&sandbox, &format!("{} --wait", editor.display()));

    assert_eq!(run.code, 0, "{run}");
    let args = std::fs::read_to_string(&args_file).unwrap_or_default();
    let config = sandbox.config_dir().join("plasmoid-updater.toml");
    assert_eq!(
        args.trim_end(),
        format!("--wait {}", config.display()),
        "{run}"
    );
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
