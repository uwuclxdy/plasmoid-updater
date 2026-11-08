//! Black-box tests of the binary's stream split, logging, exit codes and store
//! access, run in sandboxed environments against the shared fake KDE Store.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../libplasmoid-updater/tests/common/mod.rs"]
mod common;

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use common::{
    ExtraEntry, FakeStore, Listing, OLD_VERSION, SCHEME_FILE, SCHEME_ID, SCHEME_NAME, Sandbox,
};

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

fn run(sandbox: &Sandbox, api_url: &str, args: &[&str]) -> Run {
    run_in_config_dir(sandbox, api_url, &sandbox.config_dir(), args)
}

fn run_in_config_dir(
    sandbox: &Sandbox,
    api_url: &str,
    config_dir: &std::path::Path,
    args: &[&str],
) -> Run {
    let output = sandbox
        .command(bin())
        .env(API_URL_VAR, api_url)
        .env("PLASMOID_UPDATER_CONFIG_DIR", config_dir)
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

fn up_to_date() -> (Sandbox, FakeStore) {
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing {
        version: OLD_VERSION,
        ..Listing::default()
    });
    (sandbox, store)
}

fn write_config(sandbox: &Sandbox, toml: &str) {
    std::fs::write(sandbox.config_dir().join("plasmoid-updater.toml"), toml).unwrap();
}

/// The file of the scheme `install_unresolvable_scheme` registers.
const SECOND_FILE: &str = "Second.colors";

/// Registers a second color scheme, displayed as `name`, whose store entry no
/// listing carries, so a per-id lookup is its only route and the store answers
/// `404`.
fn install_unresolvable_scheme(sandbox: &Sandbox, name: &str) {
    let path = sandbox.data_home().join("color-schemes").join(SECOND_FILE);
    std::fs::write(&path, format!("[General]\nName={name}\n")).unwrap();
    let registry = sandbox.registry_path();
    let content = std::fs::read_to_string(&registry).unwrap();
    let entry = format!(
        r#"  <stuff category="112">
    <name>{name}</name>
    <providerid>api.kde-look.org</providerid>
    <version>1.0</version>
    <installedfile>{installed}</installedfile>
    <id>999002</id>
    <status>installed</status>
  </stuff>
</hotnewstuffregistry>"#,
        installed = path.display(),
    );
    std::fs::write(&registry, content.replace("</hotnewstuffregistry>", &entry)).unwrap();
}

#[test]
fn check_exits_1_when_an_update_and_a_failed_check_coexist() {
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    install_unresolvable_scheme(&sandbox, "Second Scheme");
    let store = FakeStore::start(Listing::default());
    let run = run(&sandbox, store.url(), &["--skip-plasma-detection", "check"]);
    assert_eq!(run.code, 1, "{run}");
    assert!(run.stdout.contains(SCHEME_NAME), "{run}");
    assert!(run.stderr.contains("Second Scheme"), "{run}");
}

#[test]
fn a_failed_stderr_write_is_swallowed_and_keeps_the_exit_code() {
    let (sandbox, store) = up_to_date();
    write_config(&sandbox, "excluded_patterns = [\"ab(c\"]\n");
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let output = sandbox
        .command(bin())
        .env(API_URL_VAR, store.url())
        .args(["--skip-plasma-detection", "check"])
        .stdin(Stdio::null())
        .stderr(writer)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("no updates available"));
}

#[test]
fn a_failed_stdout_write_is_fatal() {
    let (sandbox, store) = up_to_date();
    let full = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap();
    let output = sandbox
        .command(bin())
        .env(API_URL_VAR, store.url())
        .args(["--skip-plasma-detection", "check"])
        .stdin(Stdio::null())
        .stdout(full)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
}

#[test]
fn a_failed_stdout_write_reports_stdout_and_the_os_error() {
    let (sandbox, store) = up_to_date();
    let full = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap();
    let output = sandbox
        .command(bin())
        .env(API_URL_VAR, store.url())
        .args(["--skip-plasma-detection", "check"])
        .stdin(Stdio::null())
        .stdout(full)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let last = stderr.lines().last().unwrap_or_default();
    assert_eq!(
        last,
        "error: failed to write to stdout: No space left on device (os error 28)",
        "exit {:?}\n--- stderr\n{stderr}",
        output.status.code()
    );
}

#[test]
fn a_warning_line_is_the_lowercase_level_then_the_message() {
    let (sandbox, store) = up_to_date();
    write_config(&sandbox, "excluded_patterns = [\"ab(c\"]\n");
    let run = run(&sandbox, store.url(), &["--skip-plasma-detection", "check"]);
    assert_eq!(run.code, 0, "{run}");
    assert!(
        run.stderr
            .lines()
            .any(|line| line.starts_with("warn: ") && line.contains("ab(c")),
        "{run}"
    );
}

#[test]
fn update_of_an_installed_component_with_no_update_says_so_and_exits_0() {
    let (sandbox, store) = up_to_date();
    let run = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "update", "-y", SCHEME_FILE],
    );
    assert_eq!(run.code, 0, "{run}");
    assert!(
        run.stdout.contains(SCHEME_FILE) || run.stdout.contains(SCHEME_NAME),
        "{run}"
    );
}

#[test]
fn the_up_to_date_named_message_quotes_the_name() {
    let (sandbox, store) = up_to_date();
    let run = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "update", "-y", SCHEME_FILE],
    );
    assert_eq!(run.code, 0, "{run}");
    assert_eq!(
        run.stdout,
        format!("no update available for {SCHEME_FILE:?}\n"),
        "{run}"
    );
}

/// Two installed schemes share one display name: the named one is up to date,
/// its sibling's store entry is gone. The sibling's failed check must not
/// answer for the named component, on the dry run or the real update.
#[test]
fn a_same_named_siblings_failed_check_does_not_answer_for_a_named_component() {
    let (sandbox, store) = up_to_date();
    install_unresolvable_scheme(&sandbox, SCHEME_NAME);
    let expected = format!("no update available for {SCHEME_FILE:?}\n");

    for args in [
        [
            "--skip-plasma-detection",
            "update",
            "--dry-run",
            SCHEME_FILE,
        ],
        ["--skip-plasma-detection", "update", "-y", SCHEME_FILE],
    ] {
        let run = run(&sandbox, store.url(), &args);
        assert_eq!(run.code, 0, "{args:?}\n{run}");
        assert_eq!(run.stdout, expected, "{args:?}\n{run}");
    }

    // The sibling itself still reports its own failed check.
    let sibling = run(
        &sandbox,
        store.url(),
        &[
            "--skip-plasma-detection",
            "update",
            "--dry-run",
            SECOND_FILE,
        ],
    );
    assert_eq!(sibling.code, 1, "{sibling}");
    // One match keeps the display name in its failed-check line.
    assert!(
        sibling.stderr.lines().any(|line| line
            == format!("error: failed to check {SCHEME_NAME:?}: failed to fetch store entry")),
        "{sibling}"
    );
}

/// A display name two installed schemes share covers both: each answers for
/// itself, named by its directory and kind since the shared name alone cannot
/// tell them apart, and the run exits with the worst outcome.
#[test]
fn a_name_shared_by_two_components_answers_for_each() {
    let (sandbox, store) = up_to_date();
    install_unresolvable_scheme(&sandbox, SCHEME_NAME);
    let run = run(
        &sandbox,
        store.url(),
        &[
            "--skip-plasma-detection",
            "update",
            "--dry-run",
            SCHEME_NAME,
        ],
    );
    assert_eq!(run.code, 1, "{run}");
    assert_eq!(
        run.stdout,
        format!("no update available for {SCHEME_FILE:?} (Color Scheme)\n"),
        "{run}"
    );
    assert!(
        run.stderr.lines().any(|line| line
            == format!(
                "error: failed to check {SECOND_FILE:?} (Color Scheme): failed to fetch store entry"
            )),
        "{run}"
    );
}

/// A display name two schemes share: the named scheme's store entry offers no
/// download link for its version, the same-named sibling is current. Each
/// answers for itself on the dry run and the real update; the no-download row
/// never answers for the sibling.
#[test]
fn a_same_named_siblings_missing_download_does_not_answer_for_a_component() {
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    // Registered as store id 999002 at 1.0; the extra entry below serves that
    // id at 1.0, so this scheme is current rather than a 404.
    install_unresolvable_scheme(&sandbox, SCHEME_NAME);
    let store = FakeStore::start(Listing {
        link_version: Some("1.5"),
        extras: vec![ExtraEntry {
            id: 999_002,
            name: SCHEME_NAME,
            type_id: 112,
            version: OLD_VERSION,
            files: &[(SECOND_FILE, "[General]\nName=Fake Scheme\n")],
        }],
        ..Listing::default()
    });
    let skip = "--skip-plasma-detection";

    let dry = run(
        &sandbox,
        store.url(),
        &[skip, "update", "--dry-run", SCHEME_NAME],
    );
    assert_eq!(dry.code, 1, "{dry}");
    assert_eq!(
        dry.stdout,
        format!("no update available for {SECOND_FILE:?} (Color Scheme)\n"),
        "{dry}"
    );

    let current = run(
        &sandbox,
        store.url(),
        &[skip, "update", "--dry-run", SECOND_FILE],
    );
    assert_eq!(current.code, 0, "{current}");
    assert_eq!(
        current.stdout,
        format!("no update available for {SECOND_FILE:?}\n"),
        "{current}"
    );

    let real = run(
        &sandbox,
        store.url(),
        &[skip, "update", "-y", "--no-restart-plasma", SCHEME_NAME],
    );
    assert_eq!(real.code, 1, "{real}");
    assert_eq!(
        real.stdout,
        format!("no update available for {SECOND_FILE:?} (Color Scheme)\nfailed 1\n"),
        "{real}"
    );
}

/// `check --json` names each diagnostic's component by its install path, so
/// a caller can tell same-named components apart, and carries the `unmatched`
/// list, empty on a user run.
#[test]
fn check_json_diagnostics_carry_the_install_path() {
    let (sandbox, store) = up_to_date();
    install_unresolvable_scheme(&sandbox, SCHEME_NAME);
    let run = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "check", "--json"],
    );
    assert_eq!(run.code, 1, "{run}");
    let value: serde_json::Value = serde_json::from_str(&run.stdout).unwrap();

    assert_eq!(value["unmatched"], serde_json::json!([]), "{run}");
    let diagnostics = value["diagnostics"].as_array().unwrap();
    assert_eq!(diagnostics.len(), 1, "{run}");
    let path = std::path::Path::new(diagnostics[0]["path"].as_str().unwrap());
    let expected = sandbox.data_home().join("color-schemes").join(SECOND_FILE);
    assert_eq!(
        path.canonicalize().unwrap(),
        expected.canonicalize().unwrap(),
        "{run}"
    );
}

#[test]
fn the_excluded_named_message_quotes_the_name() {
    let (sandbox, store) = pending_update();
    write_config(
        &sandbox,
        &format!("excluded_packages = [\"{SCHEME_NAME}\"]\n"),
    );
    let run = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "update", "-y", SCHEME_FILE],
    );
    assert_eq!(run.code, 0, "{run}");
    assert_eq!(
        run.stdout,
        format!("{SCHEME_FILE:?} is excluded\n"),
        "{run}"
    );
}

#[test]
fn a_directory_name_rule_excludes_the_component_asked_for_by_display_name() {
    let (sandbox, store) = up_to_date();
    write_config(
        &sandbox,
        &format!("excluded_packages = [\"{SCHEME_FILE}\"]\n"),
    );
    let run = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "update", "-y", SCHEME_NAME],
    );
    assert_eq!(run.code, 0, "{run}");
    assert_eq!(
        run.stdout,
        format!("{SCHEME_NAME:?} is excluded\n"),
        "{run}"
    );
}

fn store_that_cannot_check_the_scheme() -> FakeStore {
    FakeStore::start(Listing {
        in_catalog: false,
        per_id_fails: true,
        ..Listing::default()
    })
}

#[test]
fn a_named_update_whose_check_failed_is_a_failure() {
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = store_that_cannot_check_the_scheme();
    let run = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "update", "-y", SCHEME_FILE],
    );
    assert_eq!(run.code, 1, "{run}");
    assert!(run.stderr.contains(SCHEME_NAME), "{run}");
    assert!(!run.stdout.contains("no update available"), "{run}");
}

#[test]
fn a_named_dry_run_whose_check_failed_is_a_failure() {
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = store_that_cannot_check_the_scheme();
    let run = run(
        &sandbox,
        store.url(),
        &[
            "--skip-plasma-detection",
            "update",
            "--dry-run",
            SCHEME_FILE,
        ],
    );
    assert_eq!(run.code, 1, "{run}");
    assert!(!run.stdout.contains("no update available"), "{run}");
}

#[test]
fn a_named_dry_run_predicts_the_named_run_when_another_check_failed() {
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    install_unresolvable_scheme(&sandbox, "Second Scheme");
    let store = FakeStore::start(Listing::default());
    let dry = run(
        &sandbox,
        store.url(),
        &[
            "--skip-plasma-detection",
            "update",
            "--dry-run",
            SCHEME_FILE,
        ],
    );
    let real = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "update", "-y", SCHEME_FILE],
    );
    assert_eq!(real.code, 0, "{real}");
    assert_eq!(
        dry.code, real.code,
        "the named dry run does not predict the named run:\n{dry}\n{real}"
    );
}

#[test]
fn a_dry_run_draws_the_checking_progress_line() {
    let (sandbox, store) = pending_update();
    let check = run(&sandbox, store.url(), &["--skip-plasma-detection", "check"]);
    assert!(
        check
            .stderr
            .lines()
            .any(|line| line == "checking 1 component"),
        "check does not draw the line this test pins: {check}"
    );

    let dry = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "update", "--dry-run"],
    );
    assert_eq!(dry.code, 0, "{dry}");
    assert!(
        dry.stderr
            .lines()
            .any(|line| line == "checking 1 component"),
        "{dry}"
    );

    let quiet = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "-q", "update", "--dry-run"],
    );
    assert_eq!(quiet.stderr, "", "{quiet}");
}

/// A store advertising a checksum no download can match, so the install fails
/// at the checksum stage.
fn store_with_bad_checksum() -> FakeStore {
    FakeStore::start(Listing {
        md5_override: Some(UNMATCHABLE_MD5),
        ..Listing::default()
    })
}

const UNMATCHABLE_MD5: &str = "00000000000000000000000000000000";

#[test]
fn a_failed_install_is_reported_by_the_reporter_and_the_table_only() {
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = store_with_bad_checksum();
    let run = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "update", "-y"],
    );
    assert_eq!(run.code, 1, "{run}");
    let error_text = format!("checksum mismatch: expected {UNMATCHABLE_MD5}");
    assert_eq!(
        run.stderr.matches(&error_text).count(),
        2,
        "the failure is not reported once by the reporter and once by the table:\n{run}"
    );
    assert!(
        run.stderr
            .lines()
            .any(|line| line.starts_with(&format!("\u{2717} {SCHEME_NAME} [ failed ] "))),
        "the reporter did not report the failure:\n{run}"
    );
}

#[test]
fn a_failed_install_reports_the_table_with_stdout_closed() {
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = store_with_bad_checksum();
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let output = sandbox
        .command(bin())
        .env(API_URL_VAR, store.url())
        .args(["--skip-plasma-detection", "update", "-y"])
        .stdin(Stdio::null())
        .stdout(writer)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "stderr:\n{stderr}");
    assert!(
        stderr.contains("ERROR"),
        "the failure table is missing:\n{stderr}"
    );
    assert!(stderr.contains("checksum mismatch"), "stderr:\n{stderr}");
}

#[test]
fn a_dry_run_exits_1_when_a_component_could_not_be_checked() {
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = store_that_cannot_check_the_scheme();
    let run = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "update", "--dry-run"],
    );
    assert_eq!(run.code, 1, "{run}");
    assert!(run.stderr.contains(SCHEME_NAME), "{run}");
}

#[test]
fn a_check_diagnostic_reprs_the_component_name() {
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = store_that_cannot_check_the_scheme();
    let run = run(&sandbox, store.url(), &["--skip-plasma-detection", "check"]);
    assert_eq!(run.code, 1, "{run}");
    assert!(
        run.stderr.contains("failed to check \"Fake Scheme\":"),
        "{run}"
    );
}

#[test]
fn quiet_and_verbose_are_global_and_mutually_exclusive() {
    let (sandbox, store) = up_to_date();
    for args in [
        vec!["--skip-plasma-detection", "-q", "check"],
        vec!["--skip-plasma-detection", "check", "-q"],
        vec!["--skip-plasma-detection", "-v", "check"],
        vec!["--skip-plasma-detection", "-vv", "check"],
    ] {
        let run = run(&sandbox, store.url(), &args);
        assert_eq!(run.code, 0, "{args:?}: {run}");
    }
    let conflict = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "-q", "-v", "check"],
    );
    assert_eq!(conflict.code, 2, "{conflict}");
}

#[test]
fn quiet_leaves_stderr_empty_on_a_successful_check() {
    let (sandbox, store) = pending_update();
    let run = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "-q", "check"],
    );
    assert_eq!(run.code, 100, "{run}");
    assert_eq!(run.stderr, "", "{run}");
}

#[test]
fn quiet_leaves_stderr_empty_on_a_successful_update() {
    let (sandbox, store) = pending_update();
    let run = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "-q", "update", "-y"],
    );
    assert_eq!(run.code, 0, "{run}");
    assert_eq!(run.stderr, "", "{run}");
}

#[test]
fn completions_cover_every_supported_shell() {
    let sandbox = Sandbox::new();
    for shell in ["bash", "zsh", "fish", "elvish", "powershell"] {
        let run = run(&sandbox, &common::dead_store_url(), &["completions", shell]);
        assert_eq!(run.code, 0, "{shell}: {run}");
        assert!(!run.stdout.trim().is_empty(), "{shell}: {run}");
    }
}

#[test]
fn a_completions_script_ends_with_one_newline() {
    let sandbox = Sandbox::new();
    for shell in ["bash", "zsh", "fish", "elvish", "powershell"] {
        let run = run(&sandbox, &common::dead_store_url(), &["completions", shell]);
        assert_eq!(run.code, 0, "{shell}: {run}");
        assert!(
            run.stdout.ends_with('\n') && !run.stdout.ends_with("\n\n"),
            "{shell}: the script does not end with exactly one newline",
        );
    }
}

#[test]
fn completions_ignore_a_malformed_config() {
    let sandbox = Sandbox::new();
    write_config(&sandbox, "excluded_packages = [[[\n");
    let run = run(
        &sandbox,
        &common::dead_store_url(),
        &["completions", "bash"],
    );
    assert_eq!(run.code, 0, "{run}");
    assert!(run.stdout.contains("complete -F"), "{run}");
}

#[test]
fn completions_ignore_a_config_with_an_unknown_key() {
    let sandbox = Sandbox::new();
    write_config(&sandbox, "asume_yes = true\n");
    let run = run(
        &sandbox,
        &common::dead_store_url(),
        &["completions", "bash"],
    );
    assert_eq!(run.code, 0, "{run}");
    assert!(run.stdout.contains("complete -F"), "{run}");
}

/// `verbosity` was a config key the 0.1.x releases documented. A config
/// written then must still run after the upgrade: the key is reported as
/// unused, not fatal, in exactly one warning whose path is quoted (this one
/// holds a newline, so quoting is what keeps it a single line).
#[test]
fn a_retired_config_key_is_ignored_with_a_warning() {
    let (sandbox, store) = up_to_date();
    let config_dir = sandbox.tmp_dir().join("quoting\ndir");
    std::fs::create_dir_all(&config_dir).unwrap();
    let config_path = config_dir.join("plasmoid-updater.toml");
    std::fs::write(&config_path, "verbosity = \"verbose\"\n").unwrap();
    let run = run_in_config_dir(
        &sandbox,
        store.url(),
        &config_dir,
        &["--skip-plasma-detection", "check"],
    );
    assert_eq!(run.code, 0, "{run}");
    assert!(run.stderr.contains("warn:"), "{run}");
    assert!(run.stderr.contains("verbosity"), "{run}");
    assert!(run.stderr.contains("no longer used"), "{run}");
    let expected = format!(
        "warn: config key `verbosity` in {config_path:?} is no longer used; \
         remove it from the config (-q and -v set the log level)"
    );
    assert_eq!(
        run.stderr
            .lines()
            .filter(|line| line.starts_with("warn:"))
            .collect::<Vec<_>>(),
        vec![expected.as_str()],
        "{run}"
    );
}

/// A typo'd config key must not be silently ignored: the run fails on the
/// config the way a malformed file does, naming the key and the file.
#[test]
fn an_unknown_config_key_fails_the_run_naming_the_key_and_the_file() {
    let (sandbox, store) = pending_update();
    write_config(&sandbox, "asume_yes = true\n");
    let run = run(&sandbox, store.url(), &["--skip-plasma-detection", "check"]);
    assert_eq!(run.code, 3, "{run}");
    assert!(run.stderr.contains("configuration error:"), "{run}");
    assert!(run.stderr.contains("asume_yes"), "{run}");
    assert!(run.stderr.contains("unknown field `asume_yes`"), "{run}");
    assert!(
        run.stderr.contains(
            &sandbox
                .config_dir()
                .join("plasmoid-updater.toml")
                .display()
                .to_string()
        ),
        "{run}"
    );
    let quoted = format!("{:?}", sandbox.config_dir().join("plasmoid-updater.toml"));
    assert!(run.stderr.contains(&quoted), "{run}");
}

/// The keys the config accepts, as the unknown-key error lists them: the
/// retired `verbosity` key is read and reported, never offered as valid.
const UNKNOWN_KEY_ERROR: &str = "unknown field `asume_yes`, expected one of \
    `excluded_packages`, `excluded_patterns`, `update_all_by_default`, \
    `assume_yes`, `prompt_restart`";

/// Runs `check` against a config dir named `quoting\ndir` holding `toml`, so a
/// path interpolated raw would split its message onto a second line.
fn check_with_config_in_a_quoting_dir(toml: &str) -> (Run, std::path::PathBuf) {
    let (sandbox, store) = up_to_date();
    let config_dir = sandbox.tmp_dir().join("quoting\ndir");
    std::fs::create_dir_all(&config_dir).unwrap();
    let config_path = config_dir.join("plasmoid-updater.toml");
    std::fs::write(&config_path, toml).unwrap();
    let run = run_in_config_dir(
        &sandbox,
        store.url(),
        &config_dir,
        &["--skip-plasma-detection", "check"],
    );
    (run, config_path)
}

/// A file without the retired key keeps toml's own error: the first line
/// quotes the path and gives the position, then the source snippet, then the
/// expected keys.
#[test]
fn an_unknown_key_error_keeps_the_snippet_and_lists_only_live_keys() {
    let (run, config_path) = check_with_config_in_a_quoting_dir("asume_yes = true\n");
    assert_eq!(run.code, 3, "{run}");
    let lines: Vec<&str> = run.stderr.lines().map(str::trim).collect();
    let expected_first = format!(
        "error: configuration error: failed to parse config file {config_path:?}: \
         TOML parse error at line 1, column 1"
    );
    assert_eq!(
        lines.first().copied(),
        Some(expected_first.as_str()),
        "{run}"
    );
    assert!(lines.contains(&"1 | asume_yes = true"), "{run}");
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.starts_with("unknown field"))
            .collect::<Vec<_>>(),
        vec![&UNKNOWN_KEY_ERROR],
        "{run}"
    );
}

/// A file holding the retired key is checked without it: the error still
/// names the typo and lists only live keys, and the retired key still earns
/// its one warning.
#[test]
fn an_unknown_key_beside_the_retired_one_lists_only_live_keys() {
    let (run, config_path) =
        check_with_config_in_a_quoting_dir("verbosity = \"verbose\"\nasume_yes = true\n");
    assert_eq!(run.code, 3, "{run}");
    let expected_error = format!(
        "error: configuration error: failed to parse config file {config_path:?}: \
         {UNKNOWN_KEY_ERROR}"
    );
    assert_eq!(
        run.stderr
            .lines()
            .filter(|line| line.starts_with("error:"))
            .collect::<Vec<_>>(),
        vec![expected_error.as_str()],
        "{run}"
    );
    assert_eq!(
        run.stderr
            .lines()
            .filter(|line| line.starts_with("warn:"))
            .count(),
        1,
        "{run}"
    );
}

/// The retired key is dropped before the strict pass, so a value of the wrong
/// type is reported and ignored like any other.
#[test]
fn a_retired_config_key_of_the_wrong_type_is_ignored_with_a_warning() {
    let (run, config_path) = check_with_config_in_a_quoting_dir("verbosity = 1\n");
    assert_eq!(run.code, 0, "{run}");
    let expected = format!(
        "warn: config key `verbosity` in {config_path:?} is no longer used; \
         remove it from the config (-q and -v set the log level)"
    );
    assert_eq!(
        run.stderr
            .lines()
            .filter(|line| line.starts_with("warn:"))
            .collect::<Vec<_>>(),
        vec![expected.as_str()],
        "{run}"
    );
}

/// A file that is not TOML at all keeps toml's position and snippet, with or
/// without the retired key.
#[test]
fn a_config_syntax_error_keeps_the_snippet() {
    for (toml, position) in [
        ("assume_yes = \n", "line 1, column 14"),
        (
            "verbosity = \"verbose\"\nassume_yes = \n",
            "line 2, column 14",
        ),
    ] {
        let (run, config_path) = check_with_config_in_a_quoting_dir(toml);
        assert_eq!(run.code, 3, "{run}");
        let expected = format!(
            "error: configuration error: failed to parse config file {config_path:?}: \
             TOML parse error at {position}"
        );
        assert_eq!(run.stderr.lines().next(), Some(expected.as_str()), "{run}");
    }
}

/// Writes an executable `sudo` that appends its arguments to `log`, and
/// returns the directory holding it.
fn logging_sudo(sandbox: &Sandbox, log: &std::path::Path, status: i32) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = sandbox.config_dir().join("fakebin");
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("sudo");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit {status}\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

/// Runs the binary with `sudo_dir` first on `PATH`.
fn run_with_sudo(
    sandbox: &Sandbox,
    api_url: &str,
    sudo_dir: &std::path::Path,
    args: &[&str],
) -> Run {
    let path = std::env::var("PATH").unwrap_or_default();
    let output = sandbox
        .command(bin())
        .env(API_URL_VAR, api_url)
        .env("PATH", format!("{}:{path}", sudo_dir.display()))
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

#[test]
fn system_completions_do_not_ask_for_sudo() {
    let sandbox = Sandbox::new();
    let log = sandbox.config_dir().join("sudo.log");
    let sudo = logging_sudo(&sandbox, &log, 0);
    let run = run_with_sudo(
        &sandbox,
        &common::dead_store_url(),
        &sudo,
        &["--system", "completions", "bash"],
    );
    assert_eq!(run.code, 0, "{run}");
    assert!(run.stdout.contains("complete -F"), "{run}");
    assert!(
        !log.exists(),
        "the completions command ran the sudo pre-flight: {}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
}

#[test]
fn the_system_pre_flight_runs_sudo_from_path() {
    // An effective root user skips the pre-flight by design, so it cannot be
    // observed from there.
    let root = std::process::Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "0");
    if root {
        return;
    }

    // Only a run with something to install authenticates.
    let sandbox = Sandbox::new();
    let Some((store, _)) = common::store_updating_a_host_system_component(&sandbox, bin()) else {
        return;
    };
    let log = sandbox.config_dir().join("sudo.log");
    let sudo = logging_sudo(&sandbox, &log, 1);
    let run = run_with_sudo(
        &sandbox,
        store.url(),
        &sudo,
        &["--skip-plasma-detection", "--system", "update", "-y"],
    );
    assert_eq!(run.code, 3, "{run}");
    assert!(run.stderr.contains("sudo authentication failed"), "{run}");
    assert!(
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("-v"),
        "the fake sudo on PATH never ran"
    );
}

#[test]
fn an_unnamed_update_reports_a_component_whose_check_failed() {
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = store_that_cannot_check_the_scheme();
    let run = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "update", "-y"],
    );
    assert_eq!(run.code, 1, "{run}");
    assert!(
        run.stderr.contains("failed to check \"Fake Scheme\""),
        "{run}"
    );
}

#[test]
fn a_named_update_does_not_report_another_components_failed_check() {
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    install_unresolvable_scheme(&sandbox, "Second Scheme");
    let store = FakeStore::start(Listing {
        version: OLD_VERSION,
        ..Listing::default()
    });
    let run = run(
        &sandbox,
        store.url(),
        &["--skip-plasma-detection", "update", "-y", SCHEME_FILE],
    );
    assert_eq!(run.code, 0, "{run}");
    assert!(!run.stderr.contains("Second Scheme"), "{run}");
}

#[test]
fn the_api_url_environment_variable_is_not_documented_in_help() {
    let sandbox = Sandbox::new();
    let run = run(&sandbox, &common::dead_store_url(), &["--help"]);
    assert_eq!(run.code, 0, "{run}");
    assert!(!run.stdout.contains(API_URL_VAR), "{run}");
}

#[test]
fn update_of_a_named_component_looks_the_store_up_once() {
    let (sandbox, store) = pending_update();
    let (proxy_url, seen) = counting_proxy(store.url());
    let run = run(
        &sandbox,
        &proxy_url,
        &["--skip-plasma-detection", "update", "-y", SCHEME_FILE],
    );
    assert_eq!(run.code, 0, "{run}");
    let seen = seen.lock().unwrap().clone();
    let detail = format!("/ocs/v1/content/data/{SCHEME_ID}");
    let lookups = seen.iter().filter(|target| **target == detail).count();
    assert_eq!(lookups, 1, "store requests: {seen:?}");
    // The archive comes from the fake store's own address, not the proxy, so
    // the run's single catalog-side request is the check's lookup.
    assert_eq!(seen.len(), 1, "store requests: {seen:?}");
}

/// Forwards every request to `upstream` and records its target path.
fn counting_proxy(upstream: &str) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream = upstream.to_string();
    let recorded = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let upstream = upstream.clone();
            let recorded = Arc::clone(&recorded);
            std::thread::spawn(move || forward(stream, &upstream, recorded));
        }
    });
    (format!("http://127.0.0.1:{port}/ocs/v1"), seen)
}

fn forward(mut client: TcpStream, upstream: &str, seen: Arc<Mutex<Vec<String>>>) {
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let mut head = String::new();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => head.push_str(&line),
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
    }
    let target = head.split_whitespace().nth(1).unwrap_or("").to_string();
    seen.lock().unwrap().push(target);

    let address = upstream
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or_default();
    let Ok(mut server) = TcpStream::connect(address) else {
        return;
    };
    if server.write_all(head.as_bytes()).is_err() {
        return;
    }
    let mut response = Vec::new();
    let _ = server.read_to_end(&mut response);
    let _ = client.write_all(&response);
}
