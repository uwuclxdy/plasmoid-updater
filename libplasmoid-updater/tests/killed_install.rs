//! A run killed while `kpackagetool6` writes a component: the next run puts
//! the component back at its pre-run version before it installs anything,
//! then updates it.

#![cfg(target_os = "linux")]
#![expect(clippy::unwrap_used, reason = "test helpers outside #[test] fns")]

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::{ExtraEntry, FakeStore, Listing, NEW_VERSION, OLD_VERSION, Sandbox};
use libplasmoid_updater::{Config, Reporter, update};

struct Quiet;

impl Reporter for Quiet {}

const COMIC_CATEGORY: u16 = 155;
const COMIC_ID: u64 = 999_061;
const COMIC_NAME: &str = "Killed Comic";
const PACKAGE_ID: &str = "org.example.killedcomic";

const ARCHIVE: &[(&str, &str)] = &[
    (
        "org.example.killedcomic/metadata.json",
        r#"{"KPackageStructure":"Plasma/Comic","KPlugin":{"Id":"org.example.killedcomic","Name":"Killed Comic","Version":"2.0"}}"#,
    ),
    ("org.example.killedcomic/contents/code/main.js", "// 2.0\n"),
];

/// A fake `kpackagetool6 -t Plasma/Comic -u <package>`. On each start it
/// appends the version the installed component's metadata holds, or
/// `missing`. While `kpt/block` exists it does what the real tool does first
/// (kpackagetool6 2.0 unlinks the old tree, then recreates the directory and
/// copies the package in), stops with only an empty directory written, records
/// its pid and waits to be killed; otherwise it installs the package.
const KPACKAGETOOL: &str = r#"dest="$XDG_DATA_HOME/plasma/comics/$(basename "$4")"
state="$XDG_DATA_HOME/kpt"
if [ -f "$dest/metadata.json" ]; then
  echo "$(sed -n 's/.*"Version":"\([^"]*\)".*/\1/p' "$dest/metadata.json")" >> "$state/version-at-start"
else
  echo missing >> "$state/version-at-start"
fi
if [ -e "$state/block" ]; then
  rm -f "$state/block"
  rm -rf "$dest"
  mkdir -p "$dest/contents"
  echo $$ > "$state/pid"
  exec sleep 600
fi
rm -rf "$dest"
mkdir -p "$dest"
cp -a "$4/." "$dest/"
exit 0
"#;

fn comic_dir(sandbox: &Sandbox) -> std::path::PathBuf {
    sandbox.data_home().join("plasma/comics").join(PACKAGE_ID)
}

fn install_old_comic(sandbox: &Sandbox) {
    let dir = comic_dir(sandbox);
    std::fs::create_dir_all(dir.join("contents/code")).unwrap();
    std::fs::write(
        dir.join("metadata.json"),
        format!(
            r#"{{"KPackageStructure":"Plasma/Comic","KPlugin":{{"Id":"{PACKAGE_ID}","Name":"{COMIC_NAME}","Version":"{OLD_VERSION}"}}}}"#
        ),
    )
    .unwrap();
    std::fs::write(dir.join("contents/code/main.js"), "// 1.0\n").unwrap();
    common::write_registry(
        sandbox,
        "comic.knsregistry",
        COMIC_CATEGORY,
        COMIC_NAME,
        OLD_VERSION,
        &format!("{}/metadata.json", dir.display()),
        Some(COMIC_ID),
    );
}

fn wait_for(path: &Path, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !path.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn kill(pid: i32) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    // The tool may already be gone with its run; only its absence is wanted.
    let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
}

#[test]
fn a_run_killed_while_kpackagetool_writes_is_repaired_before_the_next_install() {
    if let Some(child) = common::child() {
        let config = Config::new()
            .with_api_url(&child.store_url)
            .with_skip_plasma_detection(true)
            .with_inhibit_idle(false);
        let result = update(&config, &Quiet).unwrap();
        assert!(result.failed.is_empty(), "{result:?}");
        assert_eq!(result.succeeded.len(), 1, "{result:?}");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    install_old_comic(&sandbox);
    sandbox.fake_tool("kpackagetool6", KPACKAGETOOL);
    let state = sandbox.data_home().join("kpt");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("block"), b"").unwrap();
    let store = FakeStore::start(Listing {
        extras: vec![ExtraEntry {
            id: COMIC_ID,
            name: COMIC_NAME,
            type_id: COMIC_CATEGORY,
            version: NEW_VERSION,
            files: ARCHIVE,
        }],
        ..Listing::default()
    });
    let test = "a_run_killed_while_kpackagetool_writes_is_repaired_before_the_next_install";

    let mut killed_run = sandbox.spawn_child(test, store.url());
    wait_for(
        &state.join("pid"),
        "the first run's kpackagetool6 to start writing",
    );
    killed_run.kill().unwrap();
    killed_run.wait().unwrap();
    let tool_pid: i32 = std::fs::read_to_string(state.join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    kill(tool_pid);
    assert!(
        !comic_dir(&sandbox).join("metadata.json").exists(),
        "the killed run did not leave the component half-written"
    );

    sandbox.run_child(test, store.url());

    let starts = std::fs::read_to_string(state.join("version-at-start")).unwrap();
    assert_eq!(
        starts.lines().collect::<Vec<_>>(),
        [OLD_VERSION, OLD_VERSION],
        "the second run's install did not start from the component restored to its pre-run version"
    );
    common::assert_content(
        &comic_dir(&sandbox).join("contents/code/main.js"),
        "// 2.0\n",
    );
}
