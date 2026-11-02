//! Black-box tests of the binary's output formats: `--json` on every
//! subcommand that prints a result, `TOPGRADE_YES`, the help footer, and the
//! colour rules on every terminal surface.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../libplasmoid-updater/tests/common/mod.rs"]
mod common;

use std::io::{Read as _, Write as _};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nix::pty::openpty;
use serde_json::Value;

use common::{FakeStore, Listing, NEW_CONTENT, OLD_CONTENT, SCHEME_NAME, Sandbox};

const API_URL_VAR: &str = "PLASMOID_UPDATER_API_URL";
const REPO_URL: &str = "https://github.com/uwuclxdy/plasmoid-updater";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_plasmoid-updater")
}

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

fn output(mut command: Command) -> Run {
    let output = command.stdin(Stdio::null()).output().unwrap();
    Run {
        code: output.status.code().expect("killed by a signal"),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn command(sandbox: &Sandbox, store: &FakeStore, args: &[&str]) -> Command {
    let mut command = sandbox.command(bin());
    command
        .env(API_URL_VAR, store.url())
        .arg("--skip-plasma-detection")
        .args(args);
    command
}

fn run(sandbox: &Sandbox, store: &FakeStore, args: &[&str]) -> Run {
    output(command(sandbox, store, args))
}

fn pending_update() -> (Sandbox, FakeStore) {
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    (sandbox, FakeStore::start(Listing::default()))
}

/// The whole of stdout parsed as one JSON document: anything else on stdout
/// fails the parse.
fn json(run: &Run) -> Value {
    serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("stdout is not one JSON document ({e}):\n{run}"))
}

#[test]
fn check_json_prints_the_check_result_and_keeps_exit_100() {
    let (sandbox, store) = pending_update();
    let run = run(&sandbox, &store, &["check", "--json"]);
    assert_eq!(run.code, 100, "{run}");
    let value = json(&run);
    let updates = value["available_updates"]
        .as_array()
        .expect("available_updates");
    assert_eq!(updates.len(), 1, "{run}");
    assert_eq!(updates[0]["installed"]["name"], SCHEME_NAME, "{run}");
    assert_eq!(
        updates[0]["installed"]["component_type"], "color_scheme",
        "{run}"
    );
    assert!(value["skipped"].is_array(), "{run}");
    assert!(value["diagnostics"].is_array(), "{run}");
    assert!(value["unavailable"].is_array(), "{run}");
}

#[test]
fn list_installed_json_wraps_the_components_in_an_object() {
    let (sandbox, store) = pending_update();
    let run = run(&sandbox, &store, &["list-installed", "--json"]);
    assert_eq!(run.code, 0, "{run}");
    let value = json(&run);
    let components = value["components"].as_array().expect("components");
    assert_eq!(components.len(), 1, "{run}");
    assert_eq!(components[0]["name"], SCHEME_NAME, "{run}");
}

#[test]
fn update_json_prints_the_update_result() {
    let (sandbox, store) = pending_update();
    let run = run(
        &sandbox,
        &store,
        &["update", "-y", "--no-restart-plasma", "--json"],
    );
    assert_eq!(run.code, 0, "{run}");
    let value = json(&run);
    assert_eq!(
        value["succeeded"].as_array().map(Vec::len),
        Some(1),
        "{run}"
    );
    assert_eq!(value["succeeded"][0]["name"], SCHEME_NAME, "{run}");
    assert_eq!(
        value["succeeded"][0]["version"],
        common::NEW_VERSION,
        "a succeeded row carries the installed version: {run}"
    );
    assert_eq!(value["failed"].as_array().map(Vec::len), Some(0), "{run}");
    assert!(value["skipped"].is_array(), "{run}");
    common::assert_content(&sandbox.scheme_path(), NEW_CONTENT);
}

#[test]
fn update_dry_run_json_prints_the_check_shape_and_changes_nothing() {
    let (sandbox, store) = pending_update();
    let run = run(&sandbox, &store, &["update", "--dry-run", "--json"]);
    assert_eq!(run.code, 0, "{run}");
    let value = json(&run);
    let updates = value["available_updates"]
        .as_array()
        .expect("available_updates");
    assert_eq!(updates.len(), 1, "{run}");
    assert_eq!(updates[0]["installed"]["name"], SCHEME_NAME, "{run}");
    common::assert_content(&sandbox.scheme_path(), OLD_CONTENT);
}

#[test]
fn topgrade_yes_1_confirms_an_update_on_a_non_tty() {
    let (sandbox, store) = pending_update();
    let mut command = command(&sandbox, &store, &["update", "--no-restart-plasma"]);
    command.env("TOPGRADE_YES", "1");
    let run = output(command);
    assert_eq!(run.code, 0, "{run}");
    common::assert_content(&sandbox.scheme_path(), NEW_CONTENT);
}

#[test]
fn topgrade_yes_other_than_1_does_not_confirm() {
    let (sandbox, store) = pending_update();
    let mut command = command(&sandbox, &store, &["update", "--no-restart-plasma"]);
    command.env("TOPGRADE_YES", "0");
    let run = output(command);
    assert_eq!(run.code, 2, "{run}");
    assert!(run.stderr.contains("-y"), "{run}");
    common::assert_content(&sandbox.scheme_path(), OLD_CONTENT);
}

#[test]
fn topgrade_yes_counts_only_when_exactly_1() {
    for value in ["true", "yes", "01", "1 ", " 1", ""] {
        let (sandbox, store) = pending_update();
        let mut command = command(&sandbox, &store, &["update", "--no-restart-plasma"]);
        command.env("TOPGRADE_YES", value);
        let run = output(command);
        assert_eq!(run.code, 2, "TOPGRADE_YES={value:?}: {run}");
        common::assert_content(&sandbox.scheme_path(), OLD_CONTENT);
    }
}

#[test]
fn a_named_update_whose_check_failed_prints_its_result_as_json() {
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing {
        in_catalog: false,
        per_id_fails: true,
        ..Listing::default()
    });
    let run = run(
        &sandbox,
        &store,
        &["update", "-y", "--json", common::SCHEME_FILE],
    );
    assert_eq!(run.code, 1, "{run}");
    let value = json(&run);
    assert_eq!(
        value["succeeded"].as_array().map(Vec::len),
        Some(0),
        "{run}"
    );
    assert_eq!(value["failed"].as_array().map(Vec::len), Some(0), "{run}");
    assert_eq!(value["skipped"], serde_json::json!([SCHEME_NAME]), "{run}");
    assert!(run.stderr.contains(SCHEME_NAME), "{run}");
}

fn up_to_date() -> (Sandbox, FakeStore) {
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing {
        version: common::OLD_VERSION,
        ..Listing::default()
    });
    (sandbox, store)
}

#[test]
fn a_named_excluded_update_lists_the_display_name_in_skipped() {
    // Excluding by directory name too, so neither the matched rule nor the
    // argument can stand in for the display name.
    let cases = [
        (
            &["update", "-y", "--json", common::SCHEME_FILE][..],
            SCHEME_NAME,
        ),
        (
            &["update", "--dry-run", "--json", common::SCHEME_FILE][..],
            SCHEME_NAME,
        ),
        (
            &["update", "-y", "--json", SCHEME_NAME][..],
            common::SCHEME_FILE,
        ),
        (
            &["update", "--dry-run", "--json", SCHEME_NAME][..],
            common::SCHEME_FILE,
        ),
    ];
    for (args, rule) in cases {
        let (sandbox, store) = pending_update();
        std::fs::write(
            sandbox.config_dir().join("plasmoid-updater.toml"),
            format!("excluded_packages = [\"{rule}\"]\n"),
        )
        .unwrap();
        let run = run(&sandbox, &store, args);
        assert_eq!(run.code, 0, "{args:?}: {run}");
        assert_eq!(
            json(&run)["skipped"],
            serde_json::json!([SCHEME_NAME]),
            "{args:?}: {run}"
        );
        common::assert_content(&sandbox.scheme_path(), OLD_CONTENT);
    }
}

#[test]
fn a_named_update_prints_its_result_as_json() {
    let (sandbox, store) = pending_update();
    let run = run(
        &sandbox,
        &store,
        &[
            "update",
            "-y",
            "--no-restart-plasma",
            "--json",
            common::SCHEME_FILE,
        ],
    );
    assert_eq!(run.code, 0, "{run}");
    let value = json(&run);
    assert_eq!(value["succeeded"][0]["name"], SCHEME_NAME, "{run}");
    assert_eq!(
        value["succeeded"][0]["version"],
        common::NEW_VERSION,
        "{run}"
    );
    common::assert_content(&sandbox.scheme_path(), NEW_CONTENT);
}

#[test]
fn a_named_dry_run_prints_the_check_shape() {
    let (sandbox, store) = pending_update();
    let run = run(
        &sandbox,
        &store,
        &["update", "--dry-run", "--json", common::SCHEME_FILE],
    );
    assert_eq!(run.code, 0, "{run}");
    let value = json(&run);
    assert_eq!(
        value["available_updates"][0]["installed"]["name"], SCHEME_NAME,
        "{run}"
    );
    common::assert_content(&sandbox.scheme_path(), OLD_CONTENT);
}

#[test]
fn nothing_to_do_still_prints_one_json_document() {
    let (sandbox, store) = up_to_date();
    let update = run(&sandbox, &store, &["update", "-y", "--json"]);
    assert_eq!(update.code, 0, "{update}");
    let value = json(&update);
    assert_eq!(value["succeeded"], serde_json::json!([]), "{update}");
    assert_eq!(value["failed"], serde_json::json!([]), "{update}");
    let check = run(&sandbox, &store, &["check", "--json"]);
    assert_eq!(check.code, 0, "{check}");
    assert_eq!(
        json(&check)["available_updates"],
        serde_json::json!([]),
        "{check}"
    );
}

/// Counts CSI sequences that move the cursor or erase (final byte `A`-`H`,
/// `J` or `K`), the redraws an animation writes.
fn cursor_moves(bytes: &[u8]) -> usize {
    let mut count = 0;
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b'[' {
            let mut end = i + 2;
            while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b';') {
                end += 1;
            }
            if end < bytes.len() && matches!(bytes[end], b'A'..=b'H' | b'J' | b'K') {
                count += 1;
            }
            i = end;
        } else {
            i += 1;
        }
    }
    count
}

#[test]
fn the_cursor_move_counter_counts_redraws_and_skips_styling() {
    assert_eq!(cursor_moves(b"\x1b[2A\x1b[J\r\x1b[K\x1b[1mA\x1b[0m"), 3);
}

#[test]
fn a_forced_colour_pipe_colours_the_progress_without_animating_it() {
    let (sandbox, store) = pending_update();
    let mut command = command(&sandbox, &store, UPDATE);
    command.env("CLICOLOR_FORCE", "1");
    let output = command.stdin(Stdio::null()).output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        styling_sequences(&output.stderr) > 0,
        "progress uncoloured: {stderr:?}"
    );
    assert_eq!(
        cursor_moves(&output.stderr),
        0,
        "progress animated into a pipe: {stderr:?}"
    );
}

/// A `check` whose only component the store cannot answer for, so stderr
/// carries the logger's error line and no progress colour.
fn failed_check_on_terminals(env: &[(&str, &str)]) -> StyledRun {
    let listing = Listing {
        in_catalog: false,
        per_id_fails: true,
        ..Listing::default()
    };
    on_terminals_against(listing, CHECK, env)
}

#[test]
fn the_log_lines_follow_the_styling_rule() {
    let styled = failed_check_on_terminals(&[("TERM", "xterm-256color")]);
    let env = [("TERM", "xterm-256color")];
    assert_eq!(styled.code, 1, "{}", describe(&env, &styled));
    assert!(
        String::from_utf8_lossy(&styled.stderr).contains(SCHEME_NAME),
        "no log line: {}",
        describe(&env, &styled)
    );
    assert!(
        styling_sequences(&styled.stderr) > 0,
        "log line unstyled: {}",
        describe(&env, &styled)
    );
    let env = [("TERM", "xterm-256color"), ("NO_COLOR", "1")];
    let plain = failed_check_on_terminals(&env);
    assert_eq!(plain.code, 1, "{}", describe(&env, &plain));
    assert!(
        String::from_utf8_lossy(&plain.stderr).contains(SCHEME_NAME),
        "no log line: {}",
        describe(&env, &plain)
    );
    assert_eq!(
        styling_sequences(&plain.stderr),
        0,
        "log line styled: {}",
        describe(&env, &plain)
    );
}

#[test]
fn an_unset_term_or_clicolor_0_turns_the_log_lines_off() {
    for env in [
        &[][..],
        &[("TERM", "xterm-256color"), ("CLICOLOR", "0")][..],
    ] {
        let plain = failed_check_on_terminals(env);
        assert_eq!(plain.code, 1, "{}", describe(env, &plain));
        assert!(
            String::from_utf8_lossy(&plain.stderr).contains(SCHEME_NAME),
            "no log line: {}",
            describe(env, &plain)
        );
        assert_eq!(
            styling_sequences(&plain.stderr),
            0,
            "log line styled: {}",
            describe(env, &plain)
        );
    }
}

#[test]
fn a_succeeded_row_carries_the_store_release_date_of_the_installed_version() {
    // The fake store's `<changed>` for the new version is
    // 2026-10-01T00:00:00+00:00.
    let (sandbox, store) = pending_update();
    let run = run(
        &sandbox,
        &store,
        &["update", "-y", "--no-restart-plasma", "--json"],
    );
    assert_eq!(run.code, 0, "{run}");
    assert_eq!(
        json(&run)["succeeded"][0]["release_date"],
        "2026-10-01",
        "{run}"
    );
}

#[test]
fn help_ends_with_the_project_and_bug_tracker_urls() {
    let mut command = Sandbox::new().command(bin());
    command.arg("--help");
    let run = output(command);
    assert_eq!(run.code, 0, "{run}");
    let expected = format!("project: {REPO_URL}\nreport bugs: {REPO_URL}/issues");
    assert!(
        run.stdout.trim_end().ends_with(&expected),
        "help does not end with {expected:?}:\n{run}"
    );
}

/// Counts SGR sequences (`ESC [ <params> m`) that set an attribute: a bare
/// reset (`ESC [ m`, `ESC [ 0 m`) styles nothing and is not counted.
fn styling_sequences(bytes: &[u8]) -> usize {
    let mut count = 0;
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b'[' {
            let start = i + 2;
            let mut end = start;
            while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b';') {
                end += 1;
            }
            if end < bytes.len() && bytes[end] == b'm' {
                let params = &bytes[start..end];
                if params.iter().any(|b| !matches!(b, b'0' | b';')) {
                    count += 1;
                }
            }
            i = end;
        } else {
            i += 1;
        }
    }
    count
}

#[test]
fn the_styling_counter_counts_attributes_and_skips_resets() {
    assert_eq!(
        styling_sequences(b"\x1b[1mA\x1b[0m\x1b[m\x1b[38;5;2mB\x1b[2K"),
        2
    );
}

/// A pty whose output a background thread collects until every slave fd is
/// closed.
struct Capture {
    slave: std::os::fd::OwnedFd,
    bytes: Arc<Mutex<Vec<u8>>>,
    reader: JoinHandle<()>,
}

impl Capture {
    fn new() -> Self {
        let pty = openpty(None, None).unwrap();
        let mut master = std::fs::File::from(pty.master);
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&bytes);
        let reader = std::thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            while let Ok(n) = master.read(&mut buffer) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buffer[..n]);
            }
        });
        Self {
            slave: pty.slave,
            bytes,
            reader,
        }
    }

    fn stdio(&self) -> Stdio {
        Stdio::from(self.slave.try_clone().unwrap())
    }

    /// Closes this side's slave fd and waits for the reader to drain the pty.
    fn finish(self) -> Vec<u8> {
        drop(self.slave);
        self.reader.join().unwrap();
        Arc::try_unwrap(self.bytes).unwrap().into_inner().unwrap()
    }
}

struct StyledRun {
    code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Runs `args` against one pending update with stdout and stderr each on its
/// own pty and `env` on top of the sandbox's (which sets no `TERM`).
fn on_terminals(args: &[&str], env: &[(&str, &str)]) -> StyledRun {
    on_terminals_against(Listing::default(), args, env)
}

/// [`on_terminals`] against a store serving `listing`.
fn on_terminals_against(listing: Listing, args: &[&str], env: &[(&str, &str)]) -> StyledRun {
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(listing);
    let out = Capture::new();
    let err = Capture::new();
    let mut command = command(&sandbox, &store, args);
    command
        .envs(env.iter().copied())
        .stdout(out.stdio())
        .stderr(err.stdio());
    let status = command.stdin(Stdio::null()).status().unwrap();
    // `Command` keeps its own copies of the slave fds; they must close before
    // the readers can see the end of the pty.
    drop(command);
    StyledRun {
        code: status.code().expect("killed by a signal"),
        stdout: out.finish(),
        stderr: err.finish(),
    }
}

fn describe(env: &[(&str, &str)], run: &StyledRun) -> String {
    format!(
        "env {env:?}: exit {}\n--- stdout\n{:?}\n--- stderr\n{:?}",
        run.code,
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    )
}

const CHECK: &[&str] = &["check"];
const UPDATE: &[&str] = &["update", "-y", "--no-restart-plasma"];

/// `check`'s stdout carries the styled table; `update`'s stderr carries the
/// styled progress rows.
fn assert_styled(env: &[(&str, &str)]) {
    let check = on_terminals(CHECK, env);
    assert_eq!(check.code, 100, "{}", describe(env, &check));
    assert!(
        styling_sequences(&check.stdout) > 0,
        "table unstyled: {}",
        describe(env, &check)
    );
    let update = on_terminals(UPDATE, env);
    assert_eq!(update.code, 0, "{}", describe(env, &update));
    assert!(
        styling_sequences(&update.stderr) > 0,
        "progress unstyled: {}",
        describe(env, &update)
    );
}

fn assert_unstyled(env: &[(&str, &str)]) {
    for (args, code) in [(CHECK, 100), (UPDATE, 0)] {
        let run = on_terminals(args, env);
        assert_eq!(run.code, code, "{}", describe(env, &run));
        assert_eq!(
            styling_sequences(&run.stdout),
            0,
            "stdout styled: {}",
            describe(env, &run)
        );
        assert_eq!(
            styling_sequences(&run.stderr),
            0,
            "stderr styled: {}",
            describe(env, &run)
        );
        let shown = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(
            shown.contains(SCHEME_NAME),
            "nothing was shown: {}",
            describe(env, &run)
        );
    }
}

const LIST: &[&str] = &["list-installed"];
/// A bare run prints the help.
const BARE: &[&str] = &[];
const BOLD: &[u8] = b"\x1b[1m";

/// An update whose download fails its checksum, so stderr carries the
/// failure table (bold headers; the progress rows use colour, never bold).
fn failing_update(env: &[(&str, &str)]) -> StyledRun {
    let listing = Listing {
        md5_override: Some("00000000000000000000000000000000"),
        ..Listing::default()
    };
    on_terminals_against(listing, UPDATE, env)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[test]
fn a_colour_terminal_styles_the_table_and_the_progress() {
    assert_styled(&[("TERM", "xterm-256color")]);
}

#[test]
fn a_colour_terminal_styles_the_installed_table_the_failure_table_and_the_help() {
    let env = [("TERM", "xterm-256color")];
    let list = on_terminals(LIST, &env);
    assert_eq!(list.code, 0, "{}", describe(&env, &list));
    assert!(
        styling_sequences(&list.stdout) > 0,
        "installed table unstyled: {}",
        describe(&env, &list)
    );
    let failing = failing_update(&env);
    assert_eq!(failing.code, 1, "{}", describe(&env, &failing));
    assert!(
        contains(&failing.stderr, BOLD),
        "failure table unstyled: {}",
        describe(&env, &failing)
    );
    let bare = on_terminals(BARE, &env);
    assert_eq!(bare.code, 0, "{}", describe(&env, &bare));
    assert!(
        styling_sequences(&bare.stdout) > 0,
        "bare-run help unstyled: {}",
        describe(&env, &bare)
    );
}

#[test]
fn no_color_turns_off_the_installed_table_the_failure_table_and_the_help() {
    let env = [("TERM", "xterm-256color"), ("NO_COLOR", "1")];
    for run in [
        on_terminals(LIST, &env),
        failing_update(&env),
        on_terminals(BARE, &env),
    ] {
        assert_eq!(
            styling_sequences(&run.stdout),
            0,
            "stdout styled: {}",
            describe(&env, &run)
        );
        assert_eq!(
            styling_sequences(&run.stderr),
            0,
            "stderr styled: {}",
            describe(&env, &run)
        );
        let shown = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(
            shown.contains(SCHEME_NAME) || shown.contains("Usage:"),
            "nothing was shown: {}",
            describe(&env, &run)
        );
    }
}

#[test]
fn a_plain_pipe_is_unstyled_on_both_streams() {
    let env = [("TERM", "xterm-256color")];
    for args in [CHECK, UPDATE] {
        let (sandbox, store) = pending_update();
        let mut command = command(&sandbox, &store, args);
        command.envs(env);
        let output = command.stdin(Stdio::null()).output().unwrap();
        assert_eq!(
            styling_sequences(&output.stdout),
            0,
            "{args:?} piped stdout styled: {:?}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert_eq!(
            styling_sequences(&output.stderr),
            0,
            "{args:?} piped stderr styled: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn an_empty_no_color_does_not_turn_styling_off() {
    assert_styled(&[("TERM", "xterm-256color"), ("NO_COLOR", "")]);
}

#[test]
fn no_color_turns_styling_off_on_both_streams() {
    assert_unstyled(&[("TERM", "xterm-256color"), ("NO_COLOR", "1")]);
}

#[test]
fn term_dumb_turns_styling_off_on_both_streams() {
    assert_unstyled(&[("TERM", "dumb")]);
}

#[test]
fn an_unset_term_turns_styling_off_on_both_streams() {
    assert_unstyled(&[]);
}

#[test]
fn clicolor_0_turns_styling_off_on_both_streams() {
    assert_unstyled(&[("TERM", "xterm-256color"), ("CLICOLOR", "0")]);
}

#[test]
fn clicolor_force_styles_a_piped_table() {
    let (sandbox, store) = pending_update();
    let mut command = command(&sandbox, &store, &["check"]);
    command.env("CLICOLOR_FORCE", "1");
    let output = command.stdin(Stdio::null()).output().unwrap();
    assert_eq!(output.status.code(), Some(100));
    assert!(
        styling_sequences(&output.stdout) > 0,
        "piped table unstyled: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// Opens the selection prompt on one pty with `env`, presses Escape, and
/// returns everything the pty showed.
fn prompt_screen(env: &[(&str, &str)]) -> Vec<u8> {
    let (sandbox, store) = pending_update();
    let pty = openpty(None, None).unwrap();
    let mut command = command(&sandbox, &store, &["update", "--no-restart-plasma"]);
    command
        .envs(env.iter().copied())
        .stdin(Stdio::from(pty.slave.try_clone().unwrap()))
        .stdout(Stdio::from(pty.slave.try_clone().unwrap()))
        .stderr(Stdio::from(pty.slave.try_clone().unwrap()));
    let mut child = command.spawn().unwrap();
    drop(command);
    drop(pty.slave);
    let mut master = std::fs::File::from(pty.master);
    let screen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&screen);
    let mut reader = master.try_clone().unwrap();
    let drain = std::thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buffer) {
            if n == 0 {
                break;
            }
            sink.lock().unwrap().extend_from_slice(&buffer[..n]);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while !String::from_utf8_lossy(&screen.lock().unwrap()).contains("select to apply") {
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!(
                "the prompt never appeared: {:?}",
                String::from_utf8_lossy(&screen.lock().unwrap())
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // Let the first full draw land before ending the prompt.
    std::thread::sleep(Duration::from_millis(200));
    master.write_all(b"\x1b").unwrap();
    let status = child.wait().unwrap();
    drain.join().unwrap();
    let bytes = Arc::try_unwrap(screen).unwrap().into_inner().unwrap();
    assert_eq!(
        status.code(),
        Some(0),
        "screen: {:?}",
        String::from_utf8_lossy(&bytes)
    );
    bytes
}

#[test]
fn the_prompt_is_styled_on_a_colour_terminal() {
    let screen = prompt_screen(&[("TERM", "xterm-256color")]);
    assert!(
        styling_sequences(&screen) > 0,
        "prompt unstyled: {:?}",
        String::from_utf8_lossy(&screen)
    );
}

#[test]
fn clicolor_0_turns_the_prompt_styling_off() {
    let screen = prompt_screen(&[("TERM", "xterm-256color"), ("CLICOLOR", "0")]);
    assert_eq!(
        styling_sequences(&screen),
        0,
        "prompt styled: {:?}",
        String::from_utf8_lossy(&screen)
    );
}

#[test]
fn term_dumb_turns_the_prompt_styling_off() {
    let screen = prompt_screen(&[("TERM", "dumb")]);
    assert_eq!(
        styling_sequences(&screen),
        0,
        "prompt styled: {:?}",
        String::from_utf8_lossy(&screen)
    );
}
