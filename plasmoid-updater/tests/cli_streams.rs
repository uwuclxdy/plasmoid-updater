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

use common::{FakeStore, Listing, OLD_VERSION, SCHEME_FILE, SCHEME_ID, SCHEME_NAME, Sandbox};

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

/// Registers a second color scheme whose store entry no listing carries, so a
/// per-id lookup is its only route and the store answers `404`.
fn install_unresolvable_scheme(sandbox: &Sandbox) {
    let path = sandbox
        .data_home()
        .join("color-schemes")
        .join("Second.colors");
    std::fs::write(&path, "[General]\nName=Second Scheme\n").unwrap();
    let registry = sandbox.registry_path();
    let content = std::fs::read_to_string(&registry).unwrap();
    let entry = format!(
        r#"  <stuff category="112">
    <name>Second Scheme</name>
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
    install_unresolvable_scheme(&sandbox);
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
    install_unresolvable_scheme(&sandbox);
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
            .any(|line| line == "checking for updates..."),
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
            .any(|line| line == "checking for updates..."),
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
            .any(|line| line.starts_with(&format!("failed {SCHEME_NAME}: "))),
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

    let sandbox = Sandbox::new();
    let log = sandbox.config_dir().join("sudo.log");
    let sudo = logging_sudo(&sandbox, &log, 1);
    let run = run_with_sudo(
        &sandbox,
        &common::dead_store_url(),
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
    install_unresolvable_scheme(&sandbox);
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
