//! What discovery writes to the log (see `common` for the sandboxed
//! child-process harness).

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Mutex;

use common::Sandbox;
use libplasmoid_updater::{Config, get_installed};
use log::{Level, LevelFilter, Log, Metadata, Record};

#[derive(Debug)]
struct Line {
    level: Level,
    message: String,
}

static LINES: Mutex<Vec<Line>> = Mutex::new(Vec::new());

struct Capture;

impl Log for Capture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &Record<'_>) {
        if let Ok(mut lines) = LINES.lock() {
            lines.push(Line {
                level: record.level(),
                message: record.args().to_string(),
            });
        }
    }

    fn flush(&self) {}
}

static CAPTURE: Capture = Capture;

/// Runs discovery in the child and returns everything it logged.
fn discovery_log() -> Vec<Line> {
    let _ = log::set_logger(&CAPTURE);
    log::set_max_level(LevelFilter::Trace);
    let config = Config::new()
        .with_skip_plasma_detection(true)
        .with_inhibit_idle(false);
    get_installed(&config).unwrap();
    std::mem::take(&mut *LINES.lock().unwrap())
}

fn theme(sandbox: &Sandbox, dir: &str, name: &str) {
    let package = sandbox.data_home().join("plasma/look-and-feel").join(dir);
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("metadata.json"),
        format!(r#"{{"KPlugin":{{"Name":"{name}","Version":"1.0"}}}}"#),
    )
    .unwrap();
}

/// Writes `colorschemes.knsregistry` holding one entry named `Evil` whose
/// `installedfile` is `xml_path`.
fn scheme_registry(sandbox: &Sandbox, xml_path: &str) {
    let dir = sandbox.data_home().join("knewstuff3");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("colorschemes.knsregistry"),
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<hotnewstuffregistry>
  <stuff category="112">
    <name>Evil</name>
    <version>9.9</version>
    <installedfile>{xml_path}</installedfile>
    <id>1</id>
  </stuff>
</hotnewstuffregistry>
"#
        ),
    )
    .unwrap();
}

fn truncated_registry(sandbox: &Sandbox, file: &str) {
    let dir = sandbox.data_home().join("knewstuff3");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(file),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<hotnewstuffregistry>
  <stuff category="112">
    <name>Broken"
"#,
    )
    .unwrap();
}

#[test]
fn a_registry_entry_path_reaches_the_log_in_its_escaped_repr() {
    if let Some(child) = common::child() {
        let lines = discovery_log();
        let warnings: Vec<&Line> = lines
            .iter()
            .filter(|line| line.message.contains("outside the user data directories"))
            .collect();
        assert_eq!(warnings.len(), 1, "{lines:?}");
        assert_eq!(
            warnings[0].message,
            "ignoring Color Scheme entry \"Evil\": \
             \"/usr/share/color-schemes/evil\\nscheme.colors\" is outside the user data directories"
        );
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    scheme_registry(&sandbox, "/usr/share/color-schemes/evil\nscheme.colors");
    sandbox.run_child(
        "a_registry_entry_path_reaches_the_log_in_its_escaped_repr",
        &common::dead_store_url(),
    );
}

#[test]
fn a_broken_shared_path_registry_is_logged_once() {
    if let Some(child) = common::child() {
        let lines = discovery_log();
        let errors: Vec<&Line> = lines
            .iter()
            .filter(|line| {
                line.level == Level::Error && line.message.contains("lookandfeel.knsregistry")
            })
            .collect();
        assert_eq!(errors.len(), 1, "{lines:?}");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    theme(&sandbox, "surviving-theme", "Surviving Theme");
    truncated_registry(&sandbox, "lookandfeel.knsregistry");
    sandbox.run_child(
        "a_broken_shared_path_registry_is_logged_once",
        &common::dead_store_url(),
    );
}
