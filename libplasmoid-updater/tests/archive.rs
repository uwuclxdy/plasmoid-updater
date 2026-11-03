//! What the installer does with what the store serves: which download it
//! picks, how it bounds it, and how it drives `kpackagetool6`, each test in
//! its own sandboxed child process (see `common`).

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{
    CatalogWidget, FakeStore, Listing, NEW_CONTENT, NEW_VERSION, OLD_CONTENT, SCHEME_NAME, Sandbox,
    assert_content,
};
use libplasmoid_updater::{Config, FailureKind, Reporter, check, install, update};

struct Quiet;

impl Reporter for Quiet {}

const WIDGET_DIR: &str = "org.fake.widget";
const WIDGET_STORE_ID: u64 = 424_242;

fn config(api_url: &str) -> Config {
    Config::new()
        .with_api_url(api_url)
        .with_skip_plasma_detection(true)
        .with_inhibit_idle(false)
}

fn write_widget(dir: &std::path::Path, id: &str, version: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("metadata.json"),
        format!(
            r#"{{"KPlugin": {{"Name": "Fake Widget", "Id": "{id}", "Version": "{version}"}}}}"#
        ),
    )
    .unwrap();
}

fn widget_metadata(sandbox: &Sandbox) -> String {
    std::fs::read_to_string(
        sandbox
            .data_home()
            .join("plasma/plasmoids")
            .join(WIDGET_DIR)
            .join("metadata.json"),
    )
    .unwrap()
}

fn store_widget() -> CatalogWidget {
    CatalogWidget {
        id: WIDGET_STORE_ID,
        name: "Fake Widget",
        type_id: 713,
        version: "2.0",
    }
}

/// A fake `kpackagetool6` that appends its argv to `kpackagetool.log` in the
/// sandbox's bin dir, then runs `body`.
fn fake_kpackagetool(sandbox: &Sandbox, body: &str) -> std::path::PathBuf {
    let log = sandbox.bin_dir().join("kpackagetool.log");
    sandbox.fake_tool(
        "kpackagetool6",
        &format!("echo \"$*\" >> '{}'\n{body}", log.display()),
    );
    log
}

/// Each logged call's operation flag, the third argument after `-t <type>`.
fn calls(log: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(|line| {
            line.split_whitespace()
                .nth(2)
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

#[test]
fn a_store_entry_whose_only_link_is_older_fails_as_no_matching_download() {
    if let Some(child) = common::child() {
        let result = update(&config(&child.store_url), &Quiet).unwrap();
        assert!(result.succeeded.is_empty(), "{result:?}");
        assert_eq!(result.failed.len(), 1, "{result:?}");
        assert_eq!(result.failed[0].kind, FailureKind::NoMatchingDownload);
        assert!(result.skipped.is_empty(), "{result:?}");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing {
        link_version: Some("1.5"),
        ..Listing::default()
    });
    sandbox.run_child(
        "a_store_entry_whose_only_link_is_older_fails_as_no_matching_download",
        store.url(),
    );
    assert_content(&sandbox.scheme_path(), OLD_CONTENT);
}

#[test]
fn a_store_entry_whose_only_link_carries_no_version_installs() {
    if let Some(child) = common::child() {
        let result = update(&config(&child.store_url), &Quiet).unwrap();
        assert_eq!(result.succeeded.len(), 1, "{result:?}");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing {
        link_version: Some(""),
        ..Listing::default()
    });
    sandbox.run_child(
        "a_store_entry_whose_only_link_carries_no_version_installs",
        store.url(),
    );
    assert_content(&sandbox.scheme_path(), NEW_CONTENT);
}

#[test]
fn a_download_with_no_md5_is_capped_at_its_advertised_size() {
    if let Some(child) = common::child() {
        let result = update(&config(&child.store_url), &Quiet).unwrap();
        assert_eq!(result.failed.len(), 1, "{result:?}");
        assert_eq!(result.failed[0].kind, FailureKind::Download);
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing {
        no_md5: true,
        size_kb_override: Some(1),
        download_padding: 2 * 1024 * 1024,
        ..Listing::default()
    });
    sandbox.run_child(
        "a_download_with_no_md5_is_capped_at_its_advertised_size",
        store.url(),
    );
    assert_content(&sandbox.scheme_path(), OLD_CONTENT);
}

#[test]
fn an_empty_install_restores_a_component_stranded_by_a_killed_run() {
    if let Some(child) = common::child() {
        let result = install(&[], &config(&child.store_url), &Quiet).unwrap();
        assert!(result.failed.is_empty(), "{result:?}");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let plasmoids = sandbox.data_home().join("plasma/plasmoids");
    write_widget(
        &plasmoids.join(format!(".{WIDGET_DIR}.plasmoid-updater-old")),
        WIDGET_DIR,
        "1.0",
    );
    sandbox.run_child(
        "an_empty_install_restores_a_component_stranded_by_a_killed_run",
        &common::dead_store_url(),
    );
    assert!(
        plasmoids.join(WIDGET_DIR).join("metadata.json").is_file(),
        "the stranded widget was not put back"
    );
}

#[test]
fn check_carries_a_no_matching_download_in_unavailable() {
    if let Some(child) = common::child() {
        let result = check(&config(&child.store_url), &()).unwrap();
        assert!(result.available_updates.is_empty(), "{result:?}");
        assert_eq!(result.unavailable.len(), 1, "{result:?}");
        assert_eq!(result.unavailable[0].kind, FailureKind::NoMatchingDownload);
        assert!(
            result.unavailable[0].message.contains(NEW_VERSION),
            "the failure does not name the store version: {}",
            result.unavailable[0].message
        );
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.name == SCHEME_NAME),
            "the check stopped reporting the diagnosis: {result:?}"
        );
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing {
        link_version: Some("1.5"),
        ..Listing::default()
    });
    sandbox.run_child(
        "check_carries_a_no_matching_download_in_unavailable",
        store.url(),
    );
}

#[test]
fn check_reports_a_component_stranded_by_a_killed_run() {
    if let Some(child) = common::child() {
        let result = check(&config(&child.store_url), &()).unwrap();
        let stranded = result
            .diagnostics
            .iter()
            .find(|d| d.name == "Fake Widget" || d.name == WIDGET_DIR)
            .unwrap_or_else(|| panic!("no diagnostic names the stranded widget: {result:?}"));
        assert_eq!(
            stranded.reason,
            "interrupted install, run update to restore"
        );
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let plasmoids = sandbox.data_home().join("plasma/plasmoids");
    let stranded = plasmoids.join(format!(".{WIDGET_DIR}.plasmoid-updater-old"));
    write_widget(&stranded, WIDGET_DIR, "1.0");
    let store = FakeStore::start(Listing::default());
    sandbox.run_child(
        "check_reports_a_component_stranded_by_a_killed_run",
        store.url(),
    );
    assert!(
        stranded.join("metadata.json").is_file() && !plasmoids.join(WIDGET_DIR).exists(),
        "check changed the stranded tree"
    );
}

/// `kpackagetool6`'s refusal is matched by its English text, so every call has
/// to run in the C locale whatever locale the run inherits.
#[test]
fn every_kpackagetool_call_runs_in_the_c_locale() {
    if let Some(child) = common::child() {
        let _ = update(&config(&child.store_url), &Quiet).unwrap();
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let plasmoids = sandbox.data_home().join("plasma/plasmoids");
    write_widget(&plasmoids.join(WIDGET_DIR), WIDGET_DIR, "1.0");
    let log = sandbox.bin_dir().join("locale.log");
    sandbox.fake_tool(
        "kpackagetool6",
        &format!(
            "printf '%s|%s\\n' \"$LC_ALL\" \"$LANGUAGE\" >> '{}'\nexit 0\n",
            log.display()
        ),
    );
    let store = FakeStore::start(Listing {
        widget: Some(store_widget()),
        widget_package_id: Some(WIDGET_DIR),
        ..Listing::default()
    });
    sandbox.run_child("every_kpackagetool_call_runs_in_the_c_locale", store.url());
    let seen = std::fs::read_to_string(&log).unwrap_or_default();
    let calls: Vec<&str> = seen.lines().collect();
    assert!(!calls.is_empty(), "kpackagetool6 never ran");
    assert!(
        calls.iter().all(|line| *line == "C|C"),
        "a kpackagetool6 call ran outside the C locale: {calls:?}"
    );
}

#[test]
fn a_kpackagetool_update_failure_is_not_retried_as_remove_and_install() {
    if let Some(child) = common::child() {
        let result = update(&config(&child.store_url), &Quiet).unwrap();
        assert_eq!(result.failed.len(), 1, "{result:?}");
        assert_eq!(result.failed[0].kind, FailureKind::Install);
        assert!(
            result.failed[0]
                .message
                .contains("Could not delete package from: x"),
            "the failure lost kpackagetool's own message: {result:?}"
        );
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let plasmoids = sandbox.data_home().join("plasma/plasmoids");
    write_widget(&plasmoids.join(WIDGET_DIR), WIDGET_DIR, "1.0");
    let log = fake_kpackagetool(
        &sandbox,
        r#"case "$3" in
  -u) echo "Error: Uninstallation of package failed: Could not delete package from: x" >&2; exit 7 ;;
esac
exit 0
"#,
    );
    let store = FakeStore::start(Listing {
        widget: Some(store_widget()),
        widget_package_id: Some(WIDGET_DIR),
        ..Listing::default()
    });
    sandbox.run_child(
        "a_kpackagetool_update_failure_is_not_retried_as_remove_and_install",
        store.url(),
    );
    let calls = calls(&log);
    assert!(
        calls.iter().all(|op| op == "-u"),
        "a non-stale -u failure ran -r or -i: {calls:?}"
    );
    assert!(widget_metadata(&sandbox).contains(r#""Version": "1.0""#));
}

/// The not-installed text alone, under another exit status, is some other
/// failure: only the measured pair (exit 2 and the text) means "not installed".
#[test]
fn a_not_installed_text_under_another_exit_status_is_a_plain_failure() {
    if let Some(child) = common::child() {
        let result = update(&config(&child.store_url), &Quiet).unwrap();
        assert_eq!(result.failed.len(), 1, "{result:?}");
        assert_eq!(result.failed[0].kind, FailureKind::Install);
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let plasmoids = sandbox.data_home().join("plasma/plasmoids");
    write_widget(&plasmoids.join(WIDGET_DIR), WIDGET_DIR, "1.0");
    let log = fake_kpackagetool(
        &sandbox,
        r#"case "$3" in
  -u) echo "Error: Plugin org.fake.widget is not installed."; exit 7 ;;
esac
exit 0
"#,
    );
    let store = FakeStore::start(Listing {
        widget: Some(store_widget()),
        widget_package_id: Some(WIDGET_DIR),
        ..Listing::default()
    });
    sandbox.run_child(
        "a_not_installed_text_under_another_exit_status_is_a_plain_failure",
        store.url(),
    );
    let calls = calls(&log);
    assert!(
        calls.iter().all(|op| op == "-u"),
        "exit 7 was read as not installed: {calls:?}"
    );
}

#[test]
fn a_kpackagetool_not_installed_answer_installs_without_removing() {
    if let Some(child) = common::child() {
        let _ = update(&config(&child.store_url), &Quiet).unwrap();
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let plasmoids = sandbox.data_home().join("plasma/plasmoids");
    write_widget(&plasmoids.join(WIDGET_DIR), WIDGET_DIR, "1.0");
    let log = fake_kpackagetool(
        &sandbox,
        r#"case "$3" in
  -u) echo "Error: Plugin org.fake.widget is not installed."; exit 2 ;;
esac
exit 0
"#,
    );
    let store = FakeStore::start(Listing {
        widget: Some(store_widget()),
        widget_package_id: Some(WIDGET_DIR),
        ..Listing::default()
    });
    sandbox.run_child(
        "a_kpackagetool_not_installed_answer_installs_without_removing",
        store.url(),
    );
    let calls = calls(&log);
    assert!(
        calls.iter().all(|op| op != "-r"),
        "the not-installed answer ran -r: {calls:?}"
    );
    // The store version is written into the installed metadata after any
    // install step that returns Ok, so only a file the package alone carries
    // proves the package landed.
    assert!(
        plasmoids
            .join(WIDGET_DIR)
            .join("contents/ui/main.qml")
            .is_file(),
        "the not-installed answer installed nothing"
    );
}

/// The not-installed answer is not the only thing kpackagetool prints on
/// stdout: a failure it reports there must reach the failure row, not an empty
/// `kpackagetool6 failed: `.
#[test]
fn a_kpackagetool_failure_reported_on_stdout_is_shown() {
    if let Some(child) = common::child() {
        let result = update(&config(&child.store_url), &Quiet).unwrap();
        assert_eq!(result.failed.len(), 1, "{result:?}");
        assert_eq!(result.failed[0].kind, FailureKind::Install);
        assert!(
            result.failed[0].message.contains("Could not read package"),
            "the failure lost kpackagetool's own message: {result:?}"
        );
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let plasmoids = sandbox.data_home().join("plasma/plasmoids");
    write_widget(&plasmoids.join(WIDGET_DIR), WIDGET_DIR, "1.0");
    fake_kpackagetool(
        &sandbox,
        r#"case "$3" in
  -u) echo "Error: Could not read package"; exit 4 ;;
esac
exit 0
"#,
    );
    let store = FakeStore::start(Listing {
        widget: Some(store_widget()),
        widget_package_id: Some(WIDGET_DIR),
        ..Listing::default()
    });
    sandbox.run_child(
        "a_kpackagetool_failure_reported_on_stdout_is_shown",
        store.url(),
    );
}

/// kpackagetool addresses a package by its plugin id, so a component whose
/// directory is named differently is installed at its own path: going through
/// the tool would add a second tree under the id and leave this one stale.
#[test]
fn a_component_whose_directory_name_differs_from_its_plugin_id_is_installed_directly() {
    const PACKAGE_ID: &str = "io.github.example.claudeusage";

    if let Some(child) = common::child() {
        let result = update(&config(&child.store_url), &Quiet).unwrap();
        assert_eq!(result.succeeded.len(), 1, "{result:?}");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let plasmoids = sandbox.data_home().join("plasma/plasmoids");
    let installed = plasmoids.join("claudeusage");
    write_widget(&installed, PACKAGE_ID, "1.0");
    std::fs::create_dir_all(installed.join("contents/ui")).unwrap();
    std::fs::write(installed.join("contents/ui/main.qml"), "old").unwrap();
    let log = fake_kpackagetool(&sandbox, "exit 0\n");
    let store = FakeStore::start(Listing {
        widget: Some(store_widget()),
        widget_package_id: Some(PACKAGE_ID),
        ..Listing::default()
    });
    sandbox.run_child(
        "a_component_whose_directory_name_differs_from_its_plugin_id_is_installed_directly",
        store.url(),
    );
    let calls = calls(&log);
    assert!(
        calls.is_empty(),
        "kpackagetool6 addressed a component by the wrong name: {calls:?}"
    );
    assert!(
        !plasmoids.join(PACKAGE_ID).exists(),
        "a second tree was installed under the plugin id"
    );
    assert_eq!(
        std::fs::read_to_string(installed.join("contents/ui/main.qml")).unwrap(),
        "import QtQuick\nItem {}\n",
        "the component's own directory did not get the new tree"
    );
}

/// The package root is found beside the link, so only the metadata guard can
/// keep a symlinked `metadata.json` away from kpackagetool.
#[test]
fn a_symlinked_package_metadata_is_refused_through_update() {
    if let Some(child) = common::child() {
        let result = update(&config(&child.store_url), &Quiet).unwrap();
        assert_eq!(result.failed.len(), 1, "{result:?}");
        assert_eq!(result.failed[0].kind, FailureKind::Install);
        assert!(
            result.failed[0].message.contains("not a regular file"),
            "{result:?}"
        );
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let plasmoids = sandbox.data_home().join("plasma/plasmoids");
    write_widget(&plasmoids.join(WIDGET_DIR), WIDGET_DIR, "1.0");
    let log = fake_kpackagetool(&sandbox, "exit 0\n");
    let store = FakeStore::start(Listing {
        widget: Some(store_widget()),
        widget_package_id: Some(WIDGET_DIR),
        widget_metadata_symlink: true,
        ..Listing::default()
    });
    sandbox.run_child(
        "a_symlinked_package_metadata_is_refused_through_update",
        store.url(),
    );
    let calls = calls(&log);
    assert!(
        calls.is_empty(),
        "kpackagetool6 read a symlinked package metadata: {calls:?}"
    );
    assert!(widget_metadata(&sandbox).contains(r#""Version": "1.0""#));
}

/// A package whose links stay inside the root on paper but resolve outside it
/// must be refused before kpackagetool6, which follows them, ever runs.
#[test]
fn a_package_link_leaving_the_extract_dir_is_refused_through_update() {
    if let Some(child) = common::child() {
        let result = update(&config(&child.store_url), &Quiet).unwrap();
        assert_eq!(result.failed.len(), 1, "{result:?}");
        assert_eq!(result.failed[0].kind, FailureKind::Install);
        assert!(
            result.failed[0]
                .message
                .contains("outside the package root"),
            "{result:?}"
        );
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let plasmoids = sandbox.data_home().join("plasma/plasmoids");
    write_widget(&plasmoids.join(WIDGET_DIR), WIDGET_DIR, "1.0");
    let log = fake_kpackagetool(&sandbox, "exit 0\n");
    let store = FakeStore::start(Listing {
        widget: Some(store_widget()),
        widget_package_id: Some(WIDGET_DIR),
        widget_package_link: true,
        ..Listing::default()
    });
    sandbox.run_child(
        "a_package_link_leaving_the_extract_dir_is_refused_through_update",
        store.url(),
    );
    let calls = calls(&log);
    assert!(
        calls.is_empty(),
        "kpackagetool6 ran on a package with a link out of it: {calls:?}"
    );
    assert!(widget_metadata(&sandbox).contains(r#""Version": "1.0""#));
}

/// The not-installed answer on a component whose own directory carries the
/// plugin id means kpackagetool does not recognise the installed tree: `-i`
/// would answer "already exists", so the package is copied into that tree.
#[test]
fn a_not_installed_answer_copies_over_the_installed_directory() {
    if let Some(child) = common::child() {
        let result = update(&config(&child.store_url), &Quiet).unwrap();
        assert_eq!(result.succeeded.len(), 1, "{result:?}");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let plasmoids = sandbox.data_home().join("plasma/plasmoids");
    write_widget(&plasmoids.join(WIDGET_DIR), WIDGET_DIR, "1.0");
    let log = fake_kpackagetool(
        &sandbox,
        r#"case "$3" in
  -u) echo "Error: Plugin org.fake.widget is not installed."; exit 2 ;;
  -i) echo "Error: Plugin org.fake.widget already exists."; exit 4 ;;
esac
exit 0
"#,
    );
    let store = FakeStore::start(Listing {
        widget: Some(store_widget()),
        widget_package_id: Some(WIDGET_DIR),
        ..Listing::default()
    });
    sandbox.run_child(
        "a_not_installed_answer_copies_over_the_installed_directory",
        store.url(),
    );
    let calls = calls(&log);
    assert!(
        calls.iter().all(|op| op != "-i"),
        "the not-installed answer ran -i over an existing directory: {calls:?}"
    );
    assert!(widget_metadata(&sandbox).contains(r#""Version": "2.0""#));
    assert_eq!(
        std::fs::read_to_string(plasmoids.join(WIDGET_DIR).join("contents/ui/main.qml")).unwrap(),
        "import QtQuick\nItem {}\n",
        "the installed directory did not get the package tree"
    );
}

#[test]
fn a_store_package_with_another_plugin_id_is_refused() {
    if let Some(child) = common::child() {
        let result = update(&config(&child.store_url), &Quiet).unwrap();
        assert_eq!(result.failed.len(), 1, "{result:?}");
        assert!(
            result.failed[0]
                .message
                .contains("store entry is a different widget"),
            "{result:?}"
        );
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let plasmoids = sandbox.data_home().join("plasma/plasmoids");
    write_widget(&plasmoids.join(WIDGET_DIR), WIDGET_DIR, "1.0");
    let log = fake_kpackagetool(&sandbox, "exit 0\n");
    let store = FakeStore::start(Listing {
        widget: Some(store_widget()),
        widget_package_id: Some("org.someone.else"),
        ..Listing::default()
    });
    sandbox.run_child(
        "a_store_package_with_another_plugin_id_is_refused",
        store.url(),
    );
    let calls = calls(&log);
    assert!(calls.is_empty(), "kpackagetool6 ran: {calls:?}");
    assert!(widget_metadata(&sandbox).contains(r#""Version": "1.0""#));
}
