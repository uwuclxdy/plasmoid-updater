//! The install record a run keeps beside a component while it installs it:
//! where it may sit, when a later run repairs from it, keeps it, warns about it
//! or removes it, each run in a sandboxed child process (see `common`).

#![cfg(target_os = "linux")]
#![expect(clippy::unwrap_used, reason = "test helpers outside #[test] fns")]

mod common;

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use common::{
    ExtraEntry, FakeStore, Listing, NEW_CONTENT, OLD_CONTENT, OLD_VERSION, SCHEME_FILE, SCHEME_ID,
    SCHEME_NAME, Sandbox, assert_content,
};
use libplasmoid_updater::{
    AvailableUpdate, CheckResult, ComponentType, Config, FailureKind, InstalledComponent, Reporter,
    UpdateResult, check, install, update,
};
use log::{Level, LevelFilter, Log, Metadata, Record};

struct Quiet;

impl Reporter for Quiet {}

const RECORD_SUFFIX: &str = ".plasmoid-updater-backup";
/// The backup directory name an earlier run's backups carry.
const TS: &str = "2020-01-01T00-00-00";
const MODE: &str = "PU_RECORD_TEST_MODE";
const PENDING_REASON: &str = "interrupted install, run update to restore";

// --- log capture in the child ---

static LINES: Mutex<Vec<(Level, String)>> = Mutex::new(Vec::new());
/// The record path the logger snapshots when the installer logs that a
/// component's files landed, and what it found there.
static WATCH: Mutex<Option<PathBuf>> = Mutex::new(None);
static SNAPSHOT: Mutex<Seen> = Mutex::new(Seen::NotLogged);

/// What the watched record held when the installer logged the landed files.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Seen {
    /// No such log line came.
    NotLogged,
    /// No record was there.
    Absent,
    Held(Vec<u8>),
}

struct Capture;

impl Log for Capture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &Record<'_>) {
        let message = record.args().to_string();
        // The installer logs this inside the install slot, once the files are
        // in place and before the run settles the component's record.
        if message.starts_with("copied ")
            && let Some(watched) = WATCH.lock().unwrap().clone()
        {
            *SNAPSHOT.lock().unwrap() = std::fs::read(watched).map_or(Seen::Absent, Seen::Held);
        }
        LINES.lock().unwrap().push((record.level(), message));
    }

    fn flush(&self) {}
}

static CAPTURE: Capture = Capture;

fn capture_logs() {
    log::set_logger(&CAPTURE).unwrap();
    log::set_max_level(LevelFilter::Trace);
}

/// Every warning logged so far in this child, oldest first, but the one the
/// fake store's extra entries draw by design: they advertise no MD5.
fn warnings() -> Vec<String> {
    LINES
        .lock()
        .unwrap()
        .iter()
        .filter(|(level, message)| {
            *level == Level::Warn
                && !message.ends_with(
                    ": the store advertises no checksum, so the download is unverified and capped at 536870912 bytes",
                )
        })
        .map(|(_, message)| message.clone())
        .collect()
}

fn watch(record: PathBuf) {
    *WATCH.lock().unwrap() = Some(record);
}

fn snapshot() -> Seen {
    SNAPSHOT.lock().unwrap().clone()
}

// --- child runs ---

fn config(child: &common::Child) -> Config {
    Config::new()
        .with_api_url(&child.store_url)
        .with_skip_plasma_detection(true)
        .with_inhibit_idle(false)
}

fn mode() -> String {
    std::env::var(MODE).unwrap()
}

/// Runs `test`'s child body in `mode`, with `env` on top of the sandbox's
/// environment, and fails unless it ran to the end.
fn run(sandbox: &Sandbox, test: &str, store: &str, mode: &str, env: &[(&str, &OsStr)]) {
    run_in(sandbox, test, store, mode, env, None);
}

fn run_in(
    sandbox: &Sandbox,
    test: &str,
    store: &str,
    mode: &str,
    env: &[(&str, &OsStr)],
    cwd: Option<&Path>,
) {
    run_checking(sandbox, test, store, mode, env, cwd, || {});
}

/// [`run_in`], with `on_disk` judging what the run left on disk before the
/// child's own outcome is required, so a run whose child fails still reports
/// what it did to the disk first.
fn run_checking(
    sandbox: &Sandbox,
    test: &str,
    store: &str,
    mode: &str,
    env: &[(&str, &OsStr)],
    cwd: Option<&Path>,
    on_disk: impl FnOnce(),
) {
    let marker = sandbox.child_marker();
    if let Err(e) = std::fs::remove_file(&marker) {
        assert_eq!(e.kind(), std::io::ErrorKind::NotFound, "{e}");
    }
    let mut command = sandbox.child_command(test, store);
    command.env(MODE, mode).envs(env.iter().copied());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command.output().unwrap();
    on_disk();
    assert!(
        output.status.success() && marker.exists(),
        "child run {mode} of {test} failed ({}; body completed: {})\n--- stdout\n{}\n--- stderr\n{}",
        output.status,
        marker.exists(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// The sandbox root, which holds `home`, `data`, `cache` and the rest.
fn root(sandbox: &Sandbox) -> PathBuf {
    sandbox.home_dir().parent().unwrap().to_path_buf()
}

/// `XDG_DATA_HOME` spelled through a `..` step: the same directory as the
/// sandbox's data home.
fn dotdot_data_home(sandbox: &Sandbox) -> PathBuf {
    sandbox.data_home().join("../data")
}

// --- fixtures ---

fn backup_root(sandbox: &Sandbox) -> PathBuf {
    sandbox.cache_dir().join("plasmoid-updater/backups")
}

/// The record path beside `component`.
fn record_of(component: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".");
    name.push(component.file_name().unwrap());
    name.push(RECORD_SUFFIX);
    component.with_file_name(name)
}

/// Writes a record beside `component` naming `backup`.
fn plant_record(component: &Path, backup: &Path) -> PathBuf {
    let record = record_of(component);
    std::fs::create_dir_all(record.parent().unwrap()).unwrap();
    let mut bytes = backup.as_os_str().as_encoded_bytes().to_vec();
    bytes.push(b'\n');
    std::fs::write(&record, bytes).unwrap();
    record
}

/// A file backup under `base` an earlier run took, holding `content`.
fn plant_file_backup(
    base: &Path,
    timestamp: &str,
    subdir: &str,
    name: &str,
    content: &str,
) -> PathBuf {
    let backup = base.join(timestamp).join(subdir).join(name);
    std::fs::create_dir_all(backup.parent().unwrap()).unwrap();
    std::fs::write(&backup, content).unwrap();
    backup
}

/// The backup path an earlier run recorded and that is gone now.
fn lost_backup(sandbox: &Sandbox, subdir: &str, name: &str) -> PathBuf {
    backup_root(sandbox).join(TS).join(subdir).join(name)
}

/// The fake color scheme at `path`, registered as the store's entry.
fn install_scheme_at(sandbox: &Sandbox, path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
    register_scheme_at(sandbox, path);
}

fn register_scheme_at(sandbox: &Sandbox, path: &Path) {
    common::write_registry(
        sandbox,
        "colorschemes.knsregistry",
        112,
        SCHEME_NAME,
        OLD_VERSION,
        &path.display().to_string(),
        Some(SCHEME_ID),
    );
}

/// The backups of `subdir`'s `name` this run's own backups hold, newest last:
/// every timestamp directory that is not one of `planted`.
fn own_backups(sandbox: &Sandbox, subdir: &str, name: &str, planted: &[&str]) -> Vec<PathBuf> {
    let mut own: Vec<PathBuf> = std::fs::read_dir(backup_root(sandbox))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| !planted.iter().any(|ts| path.ends_with(ts)))
        .map(|path| path.join(subdir).join(name))
        .filter(|path| path.exists())
        .collect();
    own.sort();
    own
}

const COMIC_ID: u64 = 999_071;
const COMIC_NAME: &str = "Record Comic";
const COMIC_DIR: &str = "org.example.recordcomic";
const COMIC_ARCHIVE: &[(&str, &str)] = &[
    (
        "org.example.recordcomic/metadata.json",
        r#"{"KPackageStructure":"Plasma/Comic","KPlugin":{"Id":"org.example.recordcomic","Name":"Record Comic","Version":"2.0"}}"#,
    ),
    ("org.example.recordcomic/contents/code/main.js", "// 2.0\n"),
];

/// A fake `kpackagetool6 -t Plasma/Comic -u <package>`. On each start it
/// appends the installed version (or `missing`) to `kpt/version-at-start` and
/// what the component's install record holds (or `none`) to
/// `kpt/record-at-start`. With `kpt/block` it unlinks the old tree, leaves an
/// empty directory, records its pid and waits to be killed; with `kpt/fail`
/// it fails the way the real tool does; otherwise it installs the package.
const KPACKAGETOOL: &str = r#"dest="$XDG_DATA_HOME/plasma/comics/$(basename "$4")"
record="$XDG_DATA_HOME/plasma/comics/.$(basename "$4").plasmoid-updater-backup"
state="$HOME/kpt"
if [ -e "$state/unlock-site" ]; then
  chmod 755 "$XDG_DATA_HOME/color-schemes"
fi
if [ -f "$dest/metadata.json" ]; then
  echo "$(sed -n 's/.*"Version":"\([^"]*\)".*/\1/p' "$dest/metadata.json")" >> "$state/version-at-start"
else
  echo missing >> "$state/version-at-start"
fi
if [ -e "$record" ]; then
  cat "$record" >> "$state/record-at-start"
else
  echo none >> "$state/record-at-start"
fi
if [ -e "$state/block" ]; then
  rm -f "$state/block"
  rm -rf "$dest"
  mkdir -p "$dest/contents"
  echo $$ > "$state/pid"
  exec sleep 600
fi
if [ -e "$state/fail" ]; then
  echo "Error: refused" >&2
  exit 7
fi
rm -rf "$dest"
mkdir -p "$dest"
cp -a "$4/." "$dest/"
if [ -e "$state/lock-type-dir" ]; then
  chmod 500 "$(dirname "$dest")"
fi
exit 0
"#;

fn comic_dir(sandbox: &Sandbox) -> PathBuf {
    sandbox.data_home().join("plasma/comics").join(COMIC_DIR)
}

fn kpt(sandbox: &Sandbox) -> PathBuf {
    sandbox.home_dir().join("kpt")
}

fn install_old_comic(sandbox: &Sandbox) {
    let dir = comic_dir(sandbox);
    std::fs::create_dir_all(dir.join("contents/code")).unwrap();
    std::fs::write(
        dir.join("metadata.json"),
        format!(
            r#"{{"KPackageStructure":"Plasma/Comic","KPlugin":{{"Id":"{COMIC_DIR}","Name":"{COMIC_NAME}","Version":"{OLD_VERSION}"}}}}"#
        ),
    )
    .unwrap();
    std::fs::write(dir.join("contents/code/main.js"), "// 1.0\n").unwrap();
    common::write_registry(
        sandbox,
        "comic.knsregistry",
        155,
        COMIC_NAME,
        OLD_VERSION,
        &format!("{}/metadata.json", dir.display()),
        Some(COMIC_ID),
    );
    sandbox.fake_tool("kpackagetool6", KPACKAGETOOL);
    std::fs::create_dir_all(kpt(sandbox)).unwrap();
}

fn comic_store() -> FakeStore {
    FakeStore::start(Listing {
        extras: vec![ExtraEntry {
            id: COMIC_ID,
            name: COMIC_NAME,
            type_id: 155,
            version: "2.0",
            files: COMIC_ARCHIVE,
        }],
        ..Listing::default()
    })
}

fn kpt_lines(sandbox: &Sandbox, file: &str) -> Vec<String> {
    std::fs::read_to_string(kpt(sandbox).join(file))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

fn wait_for(path: &Path, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !path.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Starts `mode` of `test` in the sandbox, waits until the fake tool blocks
/// mid-install, then kills the run and the tool.
fn kill_mid_install(
    sandbox: &Sandbox,
    test: &str,
    store: &str,
    env: &[(&str, &OsStr)],
    cwd: Option<&Path>,
) {
    std::fs::write(kpt(sandbox).join("block"), b"").unwrap();
    let mut command = sandbox.child_command(test, store);
    command
        .env(MODE, "update")
        .envs(env.iter().copied())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let mut killed = command.spawn().unwrap();
    let pid = kpt(sandbox).join("pid");
    wait_for(&pid, "the fake kpackagetool6 to start writing");
    killed.kill().unwrap();
    killed.wait().unwrap();
    let tool: i32 = std::fs::read_to_string(&pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // The tool may already be gone with its run; only its absence is wanted.
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(tool),
        nix::sys::signal::Signal::SIGKILL,
    );
    std::fs::remove_file(pid).unwrap();
    assert!(
        !comic_dir(sandbox).join("metadata.json").exists(),
        "the killed run did not leave the component half-written"
    );
}

/// The child body every test shares: `mode` names the library call.
fn child_call(child: &common::Child) -> Outcome {
    capture_logs();
    let config = config(child);
    match mode().as_str() {
        "update" => Outcome::Update(update(&config, &Quiet).unwrap()),
        "install" => Outcome::Update(install(&[], &config, &Quiet).unwrap()),
        "check" => Outcome::Check(check(&config, &Quiet).unwrap()),
        other => panic!("unknown mode {other}"),
    }
}

#[derive(Debug)]
enum Outcome {
    Update(UpdateResult),
    Check(CheckResult),
}

impl Outcome {
    fn update(self) -> UpdateResult {
        match self {
            Self::Update(result) => result,
            Self::Check(result) => panic!("not an update: {result:?}"),
        }
    }

    fn check(self) -> CheckResult {
        match self {
            Self::Check(result) => result,
            Self::Update(result) => panic!("not a check: {result:?}"),
        }
    }
}

// --- expected lines ---

fn row1_parts(record: &Path) -> (String, &'static str) {
    (
        format!(
            "removing the unusable install record {}: ",
            record.display()
        ),
        "; nothing is restored from it",
    )
}

fn row5_parts(dest: &Path, backup: &Path) -> (String, &'static str) {
    (
        format!(
            "failed to restore {} from its backup {} (",
            dest.display(),
            backup.display()
        ),
        "); its install record stays, so the next run tries again",
    )
}

fn row6_parts(record: &Path) -> (String, &'static str) {
    (
        format!(
            "keeping the install record {} and restoring nothing: ",
            record.display()
        ),
        "; check the component, then delete that record to update it",
    )
}

fn row7(dest: &Path, backup: &Path) -> String {
    format!(
        "{}: the backup {} an interrupted install recorded is gone; `update` reinstalls it",
        dest.display(),
        backup.display()
    )
}

fn row8(dest: &Path, backup: &Path, record: &Path) -> String {
    format!(
        "{}: the backup {} an interrupted install recorded is gone, and this run offers no update for it; reinstall it, then delete {}",
        dest.display(),
        backup.display(),
        record.display()
    )
}

fn row9_parts(record: &Path) -> (String, &'static str) {
    (
        format!(
            "interrupted install, its install record {} is kept because ",
            record.display()
        ),
        "; check the component, then delete that record to update it",
    )
}

fn row2_parts(name: &str) -> (String, &'static str) {
    (
        format!("{name:?}: installing it unprotected: no install record could be written ("),
        "), so if this run is killed mid-install, no later run can restore it",
    )
}

fn has_parts(line: &str, (head, tail): &(String, &str)) -> bool {
    line.starts_with(head.as_str()) && line.ends_with(tail) && line.len() > head.len() + tail.len()
}

fn assert_one_line(lines: &[String], parts: &(String, &str)) {
    assert!(
        lines.len() == 1 && has_parts(&lines[0], parts),
        "expected exactly one line `{}...{}`, got {lines:?}",
        parts.0,
        parts.1
    );
}

// --- the record's lifecycle ---

/// A landed update holds its record while the tool writes, names an existing
/// backup in it, and leaves none behind, so the next run's repair puts nothing
/// back over the update.
#[test]
fn a_landed_update_leaves_no_record_after_holding_one_during_its_install() {
    const TEST: &str = "a_landed_update_leaves_no_record_after_holding_one_during_its_install";
    if let Some(child) = common::child() {
        let outcome = child_call(&child);
        if mode() == "update" {
            let result = outcome.update();
            assert_eq!(result.succeeded.len(), 1, "{result:?}");
        }
        assert_eq!(warnings(), Vec::<String>::new());
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    install_old_comic(&sandbox);
    let store = comic_store();

    run(&sandbox, TEST, store.url(), "update", &[]);

    let held = kpt_lines(&sandbox, "record-at-start");
    assert_eq!(held.len(), 1, "{held:?}");
    let backup = PathBuf::from(&held[0]);
    assert!(
        backup.starts_with(backup_root(&sandbox)) && backup.join("metadata.json").is_file(),
        "the record held during the install names no backup of the component: {held:?}"
    );
    assert!(
        !record_of(&comic_dir(&sandbox)).exists(),
        "the landed update left its record"
    );

    run(&sandbox, TEST, store.url(), "install", &[]);
    assert_content(
        &comic_dir(&sandbox).join("contents/code/main.js"),
        "// 2.0\n",
    );
}

/// An install that fails and restores the component leaves no record either.
#[test]
fn a_failed_install_that_restores_leaves_no_record() {
    const TEST: &str = "a_failed_install_that_restores_leaves_no_record";
    if let Some(child) = common::child() {
        let outcome = child_call(&child);
        if mode() == "update" {
            let result = outcome.update();
            assert_eq!(result.failed.len(), 1, "{result:?}");
            assert_eq!(result.failed[0].kind, FailureKind::Install, "{result:?}");
        }
        assert_eq!(warnings(), Vec::<String>::new());
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    install_old_comic(&sandbox);
    std::fs::write(kpt(&sandbox).join("fail"), b"").unwrap();
    let store = comic_store();

    run(&sandbox, TEST, store.url(), "update", &[]);

    let held = kpt_lines(&sandbox, "record-at-start");
    assert!(
        held.len() == 1 && held[0] != "none",
        "the failed install held no record: {held:?}"
    );
    assert!(
        !record_of(&comic_dir(&sandbox)).exists(),
        "the restored install left its record"
    );
    run(&sandbox, TEST, store.url(), "install", &[]);
    assert_content(
        &comic_dir(&sandbox).join("contents/code/main.js"),
        "// 1.0\n",
    );
}

/// The frozen killed-run shape under other spellings of the data and cache
/// homes: the record the killed run wrote is found and repaired from.
#[test]
fn a_kill_under_another_spelling_of_the_data_or_cache_home_is_repaired() {
    const TEST: &str = "a_kill_under_another_spelling_of_the_data_or_cache_home_is_repaired";
    if let Some(child) = common::child() {
        let outcome = child_call(&child);
        if mode() == "update" {
            let result = outcome.update();
            assert!(result.failed.is_empty(), "{result:?}");
            assert_eq!(result.succeeded.len(), 1, "{result:?}");
        }
        child.done();
        return;
    }
    // A `..` step in the cache home, the next run's repair via `install(&[])`.
    let sandbox = Sandbox::new();
    install_old_comic(&sandbox);
    let store = comic_store();
    let cache = sandbox.home_dir().join("../cache");
    let env = [("XDG_CACHE_HOME", cache.as_os_str())];
    kill_mid_install(&sandbox, TEST, store.url(), &env, None);
    run(&sandbox, TEST, store.url(), "install", &env);
    assert_content(
        &comic_dir(&sandbox).join("contents/code/main.js"),
        "// 1.0\n",
    );
    assert!(!record_of(&comic_dir(&sandbox)).exists());

    // A relative cache home, both runs from the same directory, the next run
    // an `update` that installs from the restored tree.
    let sandbox = Sandbox::new();
    install_old_comic(&sandbox);
    let cwd = root(&sandbox);
    let env = [("XDG_CACHE_HOME", OsStr::new("cache"))];
    kill_mid_install(&sandbox, TEST, store.url(), &env, Some(&cwd));
    run_in(&sandbox, TEST, store.url(), "update", &env, Some(&cwd));
    assert_eq!(kpt_lines(&sandbox, "version-at-start"), ["1.0", "1.0"]);
    assert_content(
        &comic_dir(&sandbox).join("contents/code/main.js"),
        "// 2.0\n",
    );

    // A `..` step in the data home.
    let sandbox = Sandbox::new();
    install_old_comic(&sandbox);
    let data = dotdot_data_home(&sandbox);
    let env = [("XDG_DATA_HOME", data.as_os_str())];
    kill_mid_install(&sandbox, TEST, store.url(), &env, None);
    run(&sandbox, TEST, store.url(), "update", &env);
    assert_eq!(kpt_lines(&sandbox, "version-at-start"), ["1.0", "1.0"]);
}

/// A restore the repair cannot finish keeps the record and the component out
/// of the offer; once the restore can land, the next run restores before it
/// installs anything.
#[test]
fn a_failed_repair_keeps_its_record_and_the_next_run_restores_before_installing() {
    const TEST: &str =
        "a_failed_repair_keeps_its_record_and_the_next_run_restores_before_installing";
    if let Some(child) = common::child() {
        let root = PathBuf::from(std::env::var("PU_RECORD_ROOT").unwrap());
        let scheme = root.join("data/color-schemes").join(SCHEME_FILE);
        let backup = root
            .join("cache/plasmoid-updater/backups")
            .join(TS)
            .join("color-schemes")
            .join(SCHEME_FILE);
        match std::env::var("PU_RECORD_STEP").unwrap().as_str() {
            "blocked" => {
                let result = child_call(&child).update();
                assert_one_line(&warnings(), &row5_parts(&scheme, &backup));
                assert!(
                    result.succeeded.is_empty() && result.failed.is_empty(),
                    "{result:?}"
                );
                assert_eq!(result.skipped, [SCHEME_NAME], "{result:?}");
            }
            "selected" => {
                capture_logs();
                let component = InstalledComponent::new(
                    SCHEME_NAME,
                    SCHEME_FILE,
                    ComponentType::ColorScheme,
                    &scheme,
                )
                .with_version(OLD_VERSION);
                let host = child.store_url.trim_end_matches("/ocs/v1");
                let selected = AvailableUpdate::builder(component, SCHEME_ID, "2.0")
                    .download_url(format!("{host}/dl/fake-scheme.tar.gz"))
                    .build();
                let result = install(&[selected], &config(&child), &Quiet).unwrap();
                assert_eq!(result.failed.len(), 1, "{result:?}");
                assert_eq!(result.failed[0].kind, FailureKind::RestoreFailed);
                let record = record_of(&scheme);
                let parts = (
                    format!(
                        "an interrupted install may have left it half-written, and restoring its backup {} failed (",
                        backup.display()
                    ),
                    "",
                );
                assert!(
                    has_parts(&result.failed[0].message, &parts)
                        && result.failed[0].message.ends_with(&format!(
                            "); its install record {} stays so the next run tries again",
                            record.display()
                        )),
                    "{}",
                    result.failed[0].message
                );
            }
            _ => {
                let result = child_call(&child).update();
                assert_eq!(result.succeeded.len(), 1, "{result:?}");
                assert_eq!(warnings(), Vec::<String>::new());
            }
        }
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let scheme = sandbox.scheme_path();
    register_scheme_at(&sandbox, &scheme);
    // A non-empty directory where the scheme belongs takes no file's rename.
    std::fs::create_dir_all(&scheme).unwrap();
    std::fs::write(scheme.join("occupant"), b"x").unwrap();
    let backup = plant_file_backup(
        &backup_root(&sandbox),
        TS,
        "color-schemes",
        SCHEME_FILE,
        OLD_CONTENT,
    );
    let record = plant_record(&scheme, &backup);
    let written = std::fs::read(&record).unwrap();
    let store = FakeStore::start(Listing::default());
    let root = root(&sandbox);
    let base = [("PU_RECORD_ROOT", root.as_os_str())];

    for step in ["blocked", "selected"] {
        let env = [base[0], ("PU_RECORD_STEP", OsStr::new(step))];
        run(&sandbox, TEST, store.url(), "update", &env);
        assert_eq!(
            std::fs::read(&record).unwrap(),
            written,
            "the record changed after the failed restore ({step})"
        );
    }

    std::fs::remove_dir_all(&scheme).unwrap();
    let env = [base[0], ("PU_RECORD_STEP", OsStr::new("free"))];
    run(&sandbox, TEST, store.url(), "update", &env);
    assert_content(&scheme, NEW_CONTENT);
    assert!(!record.exists(), "the record survived the landed repair");
    let own = own_backups(&sandbox, "color-schemes", SCHEME_FILE, &[TS]);
    assert_eq!(own.len(), 1, "{own:?}");
    assert_content(&own[0], OLD_CONTENT);
}

// --- lost backups ---

/// A record whose backup is gone draws one warning from each check while an
/// update is offered, survives an install that fails and restores, and goes
/// with the update that lands.
#[test]
fn a_lost_record_warns_each_check_and_only_a_landed_update_removes_it() {
    const TEST: &str = "a_lost_record_warns_each_check_and_only_a_landed_update_removes_it";
    if let Some(child) = common::child() {
        let root = PathBuf::from(std::env::var("PU_RECORD_ROOT").unwrap());
        let dest = root.join("data/plasma/comics").join(COMIC_DIR);
        let backup = root
            .join("cache/plasmoid-updater/backups")
            .join(TS)
            .join("comic-plugins")
            .join(COMIC_DIR);
        drop(child_call(&child));
        assert_eq!(warnings(), [row7(&dest, &backup)]);
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    install_old_comic(&sandbox);
    let record = plant_record(
        &comic_dir(&sandbox),
        &lost_backup(&sandbox, "comic-plugins", COMIC_DIR),
    );
    let written = std::fs::read(&record).unwrap();
    let store = comic_store();
    let root = root(&sandbox);
    let env = [("PU_RECORD_ROOT", root.as_os_str())];

    run(&sandbox, TEST, store.url(), "check", &env);
    std::fs::write(kpt(&sandbox).join("fail"), b"").unwrap();
    run(&sandbox, TEST, store.url(), "update", &env);
    assert_eq!(std::fs::read(&record).unwrap(), written);
    assert_eq!(
        kpt_lines(&sandbox, "record-at-start"),
        [String::from_utf8(written).unwrap().trim_end()],
        "the install over the lost record replaced it"
    );
    std::fs::remove_file(kpt(&sandbox).join("fail")).unwrap();
    run(&sandbox, TEST, store.url(), "update", &env);
    assert!(!record.exists(), "the landed update kept the lost record");
    assert_content(
        &comic_dir(&sandbox).join("contents/code/main.js"),
        "// 2.0\n",
    );
}

/// A kill during an install that runs over a lost record leaves the lost
/// record as it was, and the next check still warns about it.
#[test]
fn a_kill_during_an_install_over_a_lost_record_keeps_the_lost_record() {
    const TEST: &str = "a_kill_during_an_install_over_a_lost_record_keeps_the_lost_record";
    if let Some(child) = common::child() {
        let root = PathBuf::from(std::env::var("PU_RECORD_ROOT").unwrap());
        let dest = root.join("data/plasma/comics").join(COMIC_DIR);
        let backup = root
            .join("cache/plasmoid-updater/backups")
            .join(TS)
            .join("comic-plugins")
            .join(COMIC_DIR);
        drop(child_call(&child));
        if mode() == "check" {
            assert_eq!(warnings(), [row8(&dest, &backup, &record_of(&dest))]);
        }
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    install_old_comic(&sandbox);
    let record = plant_record(
        &comic_dir(&sandbox),
        &lost_backup(&sandbox, "comic-plugins", COMIC_DIR),
    );
    let written = std::fs::read(&record).unwrap();
    let store = comic_store();
    let root = root(&sandbox);
    let env = [("PU_RECORD_ROOT", root.as_os_str())];

    kill_mid_install(&sandbox, TEST, store.url(), &env, None);
    assert_eq!(
        std::fs::read(&record).unwrap(),
        written,
        "the killed install replaced the lost record"
    );
    // The half-written tree has no metadata, so nothing offers its update.
    run(&sandbox, TEST, store.url(), "check", &env);
}

/// A lost record beside a component at the store's latest goes with the next
/// real `update`, which logs nothing about it; `check` leaves it and stays
/// silent. The data home is spelled through a `..` step, while the registry
/// names the component in its resolved spelling.
#[test]
fn a_lost_record_beside_a_current_component_is_removed_silently_by_update_only() {
    const TEST: &str =
        "a_lost_record_beside_a_current_component_is_removed_silently_by_update_only";
    if let Some(child) = common::child() {
        let outcome = child_call(&child);
        if mode() == "check" {
            let result = outcome.check();
            assert!(result.diagnostics.is_empty(), "{result:?}");
            assert!(result.available_updates.is_empty(), "{result:?}");
        }
        assert_eq!(warnings(), Vec::<String>::new());
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let record = plant_record(
        &sandbox.scheme_path(),
        &lost_backup(&sandbox, "color-schemes", SCHEME_FILE),
    );
    let store = FakeStore::start(Listing {
        version: OLD_VERSION,
        ..Listing::default()
    });
    let data = dotdot_data_home(&sandbox);
    let env = [("XDG_DATA_HOME", data.as_os_str())];

    run(&sandbox, TEST, store.url(), "check", &env);
    assert!(record.exists(), "check removed the lost record");
    run(&sandbox, TEST, store.url(), "update", &env);
    assert!(
        !record.exists(),
        "update kept the current component's record"
    );
    assert_content(&sandbox.scheme_path(), OLD_CONTENT);
}

/// A lost record beside a component the check offers an update for draws the
/// row that says `update` reinstalls it, with the data home spelled through a
/// `..` step.
#[test]
fn an_offered_components_lost_record_says_update_reinstalls_it() {
    const TEST: &str = "an_offered_components_lost_record_says_update_reinstalls_it";
    if let Some(child) = common::child() {
        let root = PathBuf::from(std::env::var("PU_RECORD_ROOT").unwrap());
        let dest = root.join("data/color-schemes").join(SCHEME_FILE);
        let backup = root
            .join("cache/plasmoid-updater/backups")
            .join(TS)
            .join("color-schemes")
            .join(SCHEME_FILE);
        let result = child_call(&child).check();
        assert_eq!(result.available_updates.len(), 1, "{result:?}");
        assert_eq!(warnings(), [row7(&dest, &backup)]);
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let record = plant_record(
        &sandbox.scheme_path(),
        &lost_backup(&sandbox, "color-schemes", SCHEME_FILE),
    );
    let store = FakeStore::start(Listing::default());
    let data = dotdot_data_home(&sandbox);
    let root = root(&sandbox);
    let env = [
        ("XDG_DATA_HOME", data.as_os_str()),
        ("PU_RECORD_ROOT", root.as_os_str()),
    ];
    run(&sandbox, TEST, store.url(), "check", &env);
    assert!(record.exists());
}

/// A lost record no component of this run answers for draws the row that
/// says no update is offered, and it stays.
#[test]
fn a_lost_record_no_update_is_offered_for_says_to_reinstall_and_delete_it() {
    const TEST: &str = "a_lost_record_no_update_is_offered_for_says_to_reinstall_and_delete_it";
    if let Some(child) = common::child() {
        let root = PathBuf::from(std::env::var("PU_RECORD_ROOT").unwrap());
        let dest = root.join("data/color-schemes/Gone.colors");
        let backup = root
            .join("cache/plasmoid-updater/backups")
            .join(TS)
            .join("color-schemes/Gone.colors");
        drop(child_call(&child));
        assert_eq!(warnings(), [row8(&dest, &backup, &record_of(&dest))]);
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let gone = sandbox.data_home().join("color-schemes/Gone.colors");
    let record = plant_record(
        &gone,
        &lost_backup(&sandbox, "color-schemes", "Gone.colors"),
    );
    let store = FakeStore::start(Listing::default());
    let root = root(&sandbox);
    let env = [("PU_RECORD_ROOT", root.as_os_str())];
    run(&sandbox, TEST, store.url(), "check", &env);
    run(&sandbox, TEST, store.url(), "update", &env);
    assert!(record.exists(), "a lost record nothing settles was removed");
}

// --- records nobody can use ---

/// A torn record, one past the size cap, a symlink in the record's place (to a
/// whole valid record) and one naming another component's backup: each is
/// named in one warning and removed, nothing is restored from any, and the
/// link's target stays.
#[test]
fn a_corrupt_record_is_removed_and_restores_nothing() {
    const TEST: &str = "a_corrupt_record_is_removed_and_restores_nothing";
    if let Some(child) = common::child() {
        let root = PathBuf::from(std::env::var("PU_RECORD_ROOT").unwrap());
        drop(child_call(&child));
        let mut lines = warnings();
        lines.sort();
        let mut records: Vec<PathBuf> = ["A", "B", "C", "D"]
            .iter()
            .map(|name| record_of(&root.join(format!("data/color-schemes/{name}.colors"))))
            .collect();
        records.sort();
        assert_eq!(lines.len(), 4, "{lines:?}");
        for (line, record) in lines.iter().zip(&records) {
            assert!(has_parts(line, &row1_parts(record)), "{line}");
        }
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let schemes = sandbox.data_home().join("color-schemes");
    let base = backup_root(&sandbox);
    let mut records = Vec::new();
    for name in ["A", "B", "C", "D"] {
        let file = format!("{name}.colors");
        let component = schemes.join(&file);
        std::fs::create_dir_all(&schemes).unwrap();
        std::fs::write(&component, "mine\n").unwrap();
        plant_file_backup(&base, TS, "color-schemes", &file, "backup\n");
        records.push(record_of(&component));
    }
    let backup_of = |name: &str| base.join(TS).join("color-schemes").join(name);
    std::fs::write(
        &records[0],
        backup_of("A.colors").as_os_str().as_encoded_bytes(),
    )
    .unwrap();
    std::fs::write(&records[1], format!("{}\n", "/".repeat(4096))).unwrap();
    let elsewhere = root(&sandbox).join("valid-record");
    std::fs::write(&elsewhere, format!("{}\n", backup_of("C.colors").display())).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &records[2]).unwrap();
    std::fs::write(
        &records[3],
        format!("{}\n", backup_of("A.colors").display()),
    )
    .unwrap();
    let target = std::fs::read(&elsewhere).unwrap();
    let store = FakeStore::start(Listing::default());
    let root = root(&sandbox);
    let env = [("PU_RECORD_ROOT", root.as_os_str())];

    run(&sandbox, TEST, store.url(), "install", &env);

    for record in &records {
        assert!(
            record.symlink_metadata().is_err(),
            "{} survived",
            record.display()
        );
    }
    for name in ["A", "B", "C", "D"] {
        assert_content(&schemes.join(format!("{name}.colors")), "mine\n");
    }
    assert_eq!(std::fs::read(&elsewhere).unwrap(), target);
}

/// A record naming a backup outside this run's backup base (another cache
/// home) is kept and warned about, even under another type's subdirectory;
/// the check reports the component through it alone, and a selection that
/// names the component anyway is refused.
#[test]
fn a_record_naming_a_backup_outside_this_base_is_kept() {
    const TEST: &str = "a_record_naming_a_backup_outside_this_base_is_kept";
    if let Some(child) = common::child() {
        let root = PathBuf::from(std::env::var("PU_RECORD_ROOT").unwrap());
        let scheme = root.join("data/color-schemes").join(SCHEME_FILE);
        let record = record_of(&scheme);
        match mode().as_str() {
            "install" => {
                drop(child_call(&child));
                assert_one_line(&warnings(), &row6_parts(&record));
            }
            "check" => {
                let result = child_call(&child).check();
                assert!(result.available_updates.is_empty(), "{result:?}");
                assert_eq!(result.diagnostics.len(), 1, "{result:?}");
                let row = &result.diagnostics[0];
                assert_eq!(
                    (row.name.as_str(), row.path.as_path()),
                    (SCHEME_NAME, scheme.as_path())
                );
                assert!(
                    has_parts(&row.reason, &row9_parts(&record)),
                    "{}",
                    row.reason
                );
                assert_eq!(warnings(), Vec::<String>::new());
            }
            _ => {
                capture_logs();
                let component = InstalledComponent::new(
                    SCHEME_NAME,
                    SCHEME_FILE,
                    ComponentType::ColorScheme,
                    &scheme,
                )
                .with_version(OLD_VERSION);
                let host = child.store_url.trim_end_matches("/ocs/v1");
                let selected = AvailableUpdate::builder(component, SCHEME_ID, "2.0")
                    .download_url(format!("{host}/dl/fake-scheme.tar.gz"))
                    .build();
                let result = install(&[selected], &config(&child), &Quiet).unwrap();
                assert_eq!(result.failed.len(), 1, "{result:?}");
                assert_eq!(result.failed[0].kind, FailureKind::Install);
                let parts = (
                    format!(
                        "not updating it: the install record {} an interrupted install left is kept because ",
                        record.display()
                    ),
                    "; check the component, then delete that record to update it",
                );
                assert!(
                    has_parts(&result.failed[0].message, &parts),
                    "{}",
                    result.failed[0].message
                );
            }
        }
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let other = root(&sandbox).join("other-cache/plasmoid-updater/backups");
    let backup = plant_file_backup(&other, TS, "icons", SCHEME_FILE, "other\n");
    let record = plant_record(&sandbox.scheme_path(), &backup);
    let written = std::fs::read(&record).unwrap();
    let store = FakeStore::start(Listing::default());
    let root = root(&sandbox);
    let env = [("PU_RECORD_ROOT", root.as_os_str())];

    for mode in ["install", "check", "selected"] {
        run(&sandbox, TEST, store.url(), mode, &env);
        assert_eq!(std::fs::read(&record).unwrap(), written, "{mode}");
        assert_content(&sandbox.scheme_path(), OLD_CONTENT);
    }
    assert_content(&backup, "other\n");
}

/// An unreadable record is kept, never read as corrupt: its site lists it, so
/// it is there. The repair keeps it, the check reports its component through
/// it, and a selection that names the component anyway is refused.
#[test]
fn an_unreadable_record_is_kept() {
    const TEST: &str = "an_unreadable_record_is_kept";
    if nix::unistd::Uid::effective().is_root() {
        eprintln!("skipped: root reads a mode-000 file");
        return;
    }
    if let Some(child) = common::child() {
        let root = PathBuf::from(std::env::var("PU_RECORD_ROOT").unwrap());
        let scheme = root.join("data/color-schemes").join(SCHEME_FILE);
        let record = record_of(&scheme);
        match mode().as_str() {
            "install" => {
                drop(child_call(&child));
                assert_one_line(&warnings(), &row6_parts(&record));
            }
            "selected" => {
                capture_logs();
                let component = InstalledComponent::new(
                    SCHEME_NAME,
                    SCHEME_FILE,
                    ComponentType::ColorScheme,
                    &scheme,
                )
                .with_version(OLD_VERSION);
                let host = child.store_url.trim_end_matches("/ocs/v1");
                let selected = AvailableUpdate::builder(component, SCHEME_ID, "2.0")
                    .download_url(format!("{host}/dl/fake-scheme.tar.gz"))
                    .build();
                let result = install(&[selected], &config(&child), &Quiet).unwrap();
                assert_eq!(result.failed.len(), 1, "{result:?}");
                assert_eq!(result.failed[0].kind, FailureKind::Install);
                assert_eq!(
                    result.failed[0].message,
                    format!(
                        "not updating it: the install record {} an interrupted install left is kept because it cannot be read (Permission denied (os error 13)); check the component, then delete that record to update it",
                        record.display()
                    )
                );
            }
            _ => {
                let result = child_call(&child).check();
                assert!(result.available_updates.is_empty(), "{result:?}");
                assert_eq!(result.diagnostics.len(), 1, "{result:?}");
                assert!(
                    has_parts(&result.diagnostics[0].reason, &row9_parts(&record)),
                    "{result:?}"
                );
            }
        }
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let backup = plant_file_backup(
        &backup_root(&sandbox),
        TS,
        "color-schemes",
        SCHEME_FILE,
        OLD_CONTENT,
    );
    let record = plant_record(&sandbox.scheme_path(), &backup);
    std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o000)).unwrap();
    let store = FakeStore::start(Listing::default());
    let root = root(&sandbox);
    let env = [("PU_RECORD_ROOT", root.as_os_str())];
    for mode in ["install", "check", "selected"] {
        run(&sandbox, TEST, store.url(), mode, &env);
        assert!(record.exists(), "{mode} removed the unreadable record");
    }
}

/// A recorded backup whose lookup fails for another reason than its absence
/// is not read as gone: the record is kept, not warned about as lost.
#[test]
fn a_backup_that_cannot_be_looked_up_is_not_read_as_gone() {
    const TEST: &str = "a_backup_that_cannot_be_looked_up_is_not_read_as_gone";
    if nix::unistd::Uid::effective().is_root() {
        eprintln!("skipped: root searches a mode-000 directory");
        return;
    }
    if let Some(child) = common::child() {
        let root = PathBuf::from(std::env::var("PU_RECORD_ROOT").unwrap());
        let record = record_of(&root.join("data/color-schemes").join(SCHEME_FILE));
        let result = child_call(&child).check();
        assert!(result.available_updates.is_empty(), "{result:?}");
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert!(
            has_parts(&result.diagnostics[0].reason, &row9_parts(&record)),
            "{result:?}"
        );
        assert_eq!(warnings(), Vec::<String>::new());
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let backup = plant_file_backup(
        &backup_root(&sandbox),
        TS,
        "color-schemes",
        SCHEME_FILE,
        OLD_CONTENT,
    );
    let record = plant_record(&sandbox.scheme_path(), &backup);
    let closed = backup_root(&sandbox).join(TS);
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000)).unwrap();
    let store = FakeStore::start(Listing::default());
    let root = root(&sandbox);
    let env = [("PU_RECORD_ROOT", root.as_os_str())];
    run(&sandbox, TEST, store.url(), "check", &env);
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(record.exists());
}

/// A type directory the run can list but not search: no record can be opened
/// there, yet its listing shows none, so the install reads no record beside
/// its component, as the walk does, and never refuses it over a record that
/// is not there. It goes on to the backup, which fails on the directory's own
/// mode, since nothing in it can be read.
#[test]
fn a_site_listed_without_the_record_holds_none_though_nothing_there_opens() {
    const TEST: &str = "a_site_listed_without_the_record_holds_none_though_nothing_there_opens";
    if nix::unistd::Uid::effective().is_root() {
        eprintln!("skipped: root searches a mode-0644 directory");
        return;
    }
    if let Some(child) = common::child() {
        let root = PathBuf::from(std::env::var("PU_RECORD_ROOT").unwrap());
        let scheme = root.join("data/color-schemes").join(SCHEME_FILE);
        let record = record_of(&scheme);
        let result = child_call(&child).update();
        let row4 = format!(
            "not updating it: the install record {} an interrupted install left is kept because ",
            record.display()
        );
        assert!(
            !result
                .failed
                .iter()
                .any(|failure| failure.message.starts_with(&row4)),
            "the install was refused over a record that is not there: {result:?}"
        );
        assert_eq!(result.failed.len(), 1, "{result:?}");
        assert_eq!(result.failed[0].kind, FailureKind::Install);
        assert_eq!(
            result.failed[0].message,
            format!("{}: Permission denied (os error 13)", scheme.display()),
            "the install did not go on to back up the component"
        );
        assert_eq!(warnings(), Vec::<String>::new());
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let site = sandbox.data_home().join("color-schemes");
    let record = record_of(&sandbox.scheme_path());
    let store = FakeStore::start(Listing::default());
    let root = root(&sandbox);
    let env = [("PU_RECORD_ROOT", root.as_os_str())];
    std::fs::set_permissions(&site, std::fs::Permissions::from_mode(0o644)).unwrap();
    run_checking(&sandbox, TEST, store.url(), "update", &env, None, || {
        std::fs::set_permissions(&site, std::fs::Permissions::from_mode(0o755)).unwrap();
    });
    assert!(!record.exists(), "a record appeared beside the component");
    assert_content(&sandbox.scheme_path(), OLD_CONTENT);
}

/// A type directory the run can search but not list, holding no record: the
/// open's NotFound proves nothing sits there even though the listing cannot
/// show it, so the update lands and leaves no record behind.
#[test]
fn a_record_missing_from_a_site_that_cannot_be_listed_holds_none() {
    const TEST: &str = "a_record_missing_from_a_site_that_cannot_be_listed_holds_none";
    if nix::unistd::Uid::effective().is_root() {
        eprintln!("skipped: root lists a mode-0311 directory");
        return;
    }
    if let Some(child) = common::child() {
        let root = PathBuf::from(std::env::var("PU_RECORD_ROOT").unwrap());
        let site = root.join("data/color-schemes");
        let record = record_of(&site.join(SCHEME_FILE));
        let result = child_call(&child).update();
        assert!(result.failed.is_empty(), "{result:?}");
        assert_eq!(result.succeeded.len(), 1, "{result:?}");
        let error = std::fs::read_dir(&site).expect_err("the site stayed listable in the child");
        assert_eq!(
            warnings(),
            [format!(
                "cannot read the install records in {} ({}), so this run deletes no old backup",
                site.display(),
                error
            )]
        );
        assert!(!record.exists(), "the update left its record behind");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let site = sandbox.data_home().join("color-schemes");
    let record = record_of(&sandbox.scheme_path());
    let store = FakeStore::start(Listing::default());
    let root = root(&sandbox);
    let env = [("PU_RECORD_ROOT", root.as_os_str())];
    std::fs::set_permissions(&site, std::fs::Permissions::from_mode(0o311)).unwrap();
    run_checking(&sandbox, TEST, store.url(), "update", &env, None, || {
        std::fs::set_permissions(&site, std::fs::Permissions::from_mode(0o755)).unwrap();
    });
    assert!(!record.exists(), "the update left its record behind");
    assert_content(&sandbox.scheme_path(), NEW_CONTENT);
}

/// A record that cannot be opened in a site the run cannot list whole: the
/// settle cannot prove whether a record sits there, so it refuses the install
/// naming the site's listing error, never claiming a record it did not read.
#[test]
fn an_unopenable_record_in_an_unlistable_site_names_the_listing_error() {
    const TEST: &str = "an_unopenable_record_in_an_unlistable_site_names_the_listing_error";
    if nix::unistd::Uid::effective().is_root() {
        eprintln!("skipped: root lists a mode-0311 directory");
        return;
    }
    if let Some(child) = common::child() {
        let root = PathBuf::from(std::env::var("PU_RECORD_ROOT").unwrap());
        let site = root.join("data/color-schemes");
        let record = record_of(&site.join(SCHEME_FILE));
        let result = child_call(&child).update();
        assert!(result.succeeded.is_empty(), "{result:?}");
        assert_eq!(result.failed.len(), 1, "{result:?}");
        assert_eq!(result.failed[0].kind, FailureKind::Install, "{result:?}");
        let error = std::fs::read_dir(&site).expect_err("the site stayed listable in the child");
        assert_eq!(
            result.failed[0].message,
            format!(
                "not updating it: an interrupted install may have left the install record {}, but its directory cannot be read ({error}); make that directory readable, then update again",
                record.display()
            )
        );
        assert!(record.exists(), "the refused update removed the record");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let site = sandbox.data_home().join("color-schemes");
    let backup = plant_file_backup(
        &backup_root(&sandbox),
        TS,
        "color-schemes",
        SCHEME_FILE,
        OLD_CONTENT,
    );
    let record = plant_record(&sandbox.scheme_path(), &backup);
    std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o000)).unwrap();
    let store = FakeStore::start(Listing::default());
    let root = root(&sandbox);
    let env = [("PU_RECORD_ROOT", root.as_os_str())];
    std::fs::set_permissions(&site, std::fs::Permissions::from_mode(0o311)).unwrap();
    run_checking(&sandbox, TEST, store.url(), "update", &env, None, || {
        std::fs::set_permissions(&site, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o644)).unwrap();
    });
    assert!(record.exists(), "the refused update removed the record");
    assert_content(&sandbox.scheme_path(), OLD_CONTENT);
}

// --- where records sit ---

/// A component name that leaves no room for its record's name installs
/// unprotected with one warning, as it did before records; one byte shorter,
/// the install holds its record.
#[test]
fn a_name_too_long_for_its_record_installs_unprotected_and_one_byte_less_gets_one() {
    const TEST: &str =
        "a_name_too_long_for_its_record_installs_unprotected_and_one_byte_less_gets_one";
    if let Some(child) = common::child() {
        let scheme = PathBuf::from(std::env::var("PU_RECORD_SCHEME").unwrap());
        watch(record_of(&scheme));
        let result = child_call(&child).update();
        assert_eq!(result.succeeded.len(), 1, "{result:?}");
        let seen = snapshot();
        if scheme.file_name().unwrap().len() == 231 {
            assert_one_line(&warnings(), &row2_parts(SCHEME_NAME));
            assert_eq!(seen, Seen::Absent);
        } else {
            assert_eq!(warnings(), Vec::<String>::new());
            let Seen::Held(held) = seen else {
                panic!("the install held no record: {seen:?}");
            };
            let backup = PathBuf::from(String::from_utf8(held).unwrap().trim_end());
            assert_content(&backup, OLD_CONTENT);
        }
        child.done();
        return;
    }
    for length in [231, 230] {
        let sandbox = Sandbox::new();
        let file = format!("{}.colors", "s".repeat(length - ".colors".len()));
        let scheme = sandbox.data_home().join("color-schemes").join(&file);
        install_scheme_at(&sandbox, &scheme, OLD_CONTENT);
        let store = FakeStore::start(Listing::default());
        let env = [("PU_RECORD_SCHEME", scheme.as_os_str())];
        run(&sandbox, TEST, store.url(), "update", &env);
        assert_content(&scheme, NEW_CONTENT);
    }
}

/// Under a data home spelled through a `..` step, a registry component's
/// update holds its record, with no warning at all.
#[test]
fn a_registry_components_update_holds_its_record_under_a_dotdot_data_home() {
    const TEST: &str = "a_registry_components_update_holds_its_record_under_a_dotdot_data_home";
    if let Some(child) = common::child() {
        let root = PathBuf::from(std::env::var("PU_RECORD_ROOT").unwrap());
        let scheme = root.join("data/color-schemes").join(SCHEME_FILE);
        watch(record_of(&scheme));
        let result = child_call(&child).update();
        assert_eq!(result.succeeded.len(), 1, "{result:?}");
        assert_eq!(warnings(), Vec::<String>::new());
        let seen = snapshot();
        let Seen::Held(held) = seen else {
            panic!("the install held no record: {seen:?}");
        };
        let backup = PathBuf::from(String::from_utf8(held).unwrap().trim_end());
        assert_content(&backup, OLD_CONTENT);
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing::default());
    let data = dotdot_data_home(&sandbox);
    let root = root(&sandbox);
    let env = [
        ("XDG_DATA_HOME", data.as_os_str()),
        ("PU_RECORD_ROOT", root.as_os_str()),
    ];
    run(&sandbox, TEST, store.url(), "update", &env);
    assert_content(&sandbox.scheme_path(), NEW_CONTENT);
    assert!(!record_of(&sandbox.scheme_path()).exists());
}

/// A record beside a component the KNewStuff registry names outside this data
/// home's type directories: `check` reports the component through it, and the
/// next `update` restores the component before it installs anything.
#[test]
fn a_record_beside_a_registry_component_outside_the_data_home_is_found() {
    const TEST: &str = "a_record_beside_a_registry_component_outside_the_data_home_is_found";
    if let Some(child) = common::child() {
        let elsewhere = PathBuf::from(std::env::var("PU_RECORD_SCHEME").unwrap());
        let outcome = child_call(&child);
        if mode() == "check" {
            let result = outcome.check();
            assert!(result.available_updates.is_empty(), "{result:?}");
            assert_eq!(result.diagnostics.len(), 1, "{result:?}");
            let row = &result.diagnostics[0];
            assert_eq!(
                (row.name.as_str(), row.path.as_path(), row.reason.as_str()),
                (SCHEME_NAME, elsewhere.as_path(), PENDING_REASON)
            );
        } else {
            let result = outcome.update();
            assert_eq!(result.succeeded.len(), 1, "{result:?}");
        }
        assert_eq!(warnings(), Vec::<String>::new());
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let elsewhere = sandbox
        .home_dir()
        .join("dataA/color-schemes")
        .join(SCHEME_FILE);
    install_scheme_at(&sandbox, &elsewhere, "half\n");
    let backup = plant_file_backup(
        &backup_root(&sandbox),
        TS,
        "color-schemes",
        SCHEME_FILE,
        OLD_CONTENT,
    );
    let record = plant_record(&elsewhere, &backup);
    let store = FakeStore::start(Listing::default());
    let env = [("PU_RECORD_SCHEME", elsewhere.as_os_str())];

    run(&sandbox, TEST, store.url(), "check", &env);
    assert_content(&elsewhere, "half\n");
    run(&sandbox, TEST, store.url(), "update", &env);
    assert_content(&elsewhere, NEW_CONTENT);
    assert!(!record.exists());
    let own = own_backups(&sandbox, "color-schemes", SCHEME_FILE, &[TS]);
    assert_eq!(own.len(), 1, "{own:?}");
    assert_content(&own[0], OLD_CONTENT);
}

/// A lost record beside a current registry component outside this data home's
/// type directories goes with the next `update`, which logs nothing.
#[test]
fn a_lost_record_beside_a_registry_component_outside_the_data_home_is_settled() {
    const TEST: &str = "a_lost_record_beside_a_registry_component_outside_the_data_home_is_settled";
    if let Some(child) = common::child() {
        drop(child_call(&child).update());
        assert_eq!(warnings(), Vec::<String>::new());
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let elsewhere = sandbox
        .home_dir()
        .join("dataA/color-schemes")
        .join(SCHEME_FILE);
    install_scheme_at(&sandbox, &elsewhere, OLD_CONTENT);
    let record = plant_record(
        &elsewhere,
        &lost_backup(&sandbox, "color-schemes", SCHEME_FILE),
    );
    let store = FakeStore::start(Listing {
        version: OLD_VERSION,
        ..Listing::default()
    });
    run(&sandbox, TEST, store.url(), "update", &[]);
    assert!(
        !record.exists(),
        "the lost record outside the data home stayed"
    );
}

/// Two color schemes in one registry: the store's fake scheme at `first`, and
/// an extra scheme `Other.colors` in the data home that the store updates.
fn register_two_schemes(sandbox: &Sandbox, first: &Path) {
    let other = sandbox.data_home().join("color-schemes/Other.colors");
    std::fs::create_dir_all(other.parent().unwrap()).unwrap();
    std::fs::write(&other, "[General]\nName=Other\n# old\n").unwrap();
    let registry = sandbox.registry_path();
    std::fs::create_dir_all(registry.parent().unwrap()).unwrap();
    let entry = |name: &str, path: &Path, id: u64| {
        format!(
            "  <stuff category=\"112\">\n    <name>{name}</name>\n    <providerid>api.kde-look.org</providerid>\n    <version>1.0</version>\n    <installedfile>{}</installedfile>\n    <id>{id}</id>\n    <status>installed</status>\n  </stuff>\n",
            path.display()
        )
    };
    std::fs::write(
        &registry,
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE khotnewstuff3>\n<hotnewstuffregistry>\n{}{}</hotnewstuffregistry>\n",
            entry(SCHEME_NAME, first, SCHEME_ID),
            entry("Other", &other, OTHER_ID),
        ),
    )
    .unwrap();
}

const OTHER_ID: u64 = 999_072;
const OTHER_ARCHIVE: &[(&str, &str)] = &[("Other.colors", "[General]\nName=Other\n# new\n")];

fn other_scheme_store() -> FakeStore {
    FakeStore::start(Listing {
        version: OLD_VERSION,
        extras: vec![ExtraEntry {
            id: OTHER_ID,
            name: "Other",
            type_id: 112,
            version: "2.0",
            files: OTHER_ARCHIVE,
        }],
        ..Listing::default()
    })
}

const PLANTED: [&str; 6] = [
    "2020-01-01T00-00-00",
    "2020-01-02T00-00-00",
    "2020-01-03T00-00-00",
    "2020-01-04T00-00-00",
    "2020-01-05T00-00-00",
    "2020-01-06T00-00-00",
];

/// Retention keeps the backup an open record names, beside a registry
/// component outside this data home whose restore cannot land, while another
/// component's update prunes its type past the keep count.
#[test]
fn retention_keeps_the_backup_an_open_record_names() {
    const TEST: &str = "retention_keeps_the_backup_an_open_record_names";
    if let Some(child) = common::child() {
        let result = child_call(&child).update();
        assert_eq!(result.succeeded.len(), 1, "{result:?}");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let elsewhere = sandbox
        .home_dir()
        .join("dataA/color-schemes")
        .join(SCHEME_FILE);
    register_two_schemes(&sandbox, &elsewhere);
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("occupant"), b"x").unwrap();
    let base = backup_root(&sandbox);
    for ts in PLANTED {
        plant_file_backup(&base, ts, "color-schemes", "Other.colors", "planted\n");
    }
    let named = plant_file_backup(&base, PLANTED[0], "color-schemes", SCHEME_FILE, OLD_CONTENT);
    plant_record(&elsewhere, &named);
    let store = other_scheme_store();

    run(&sandbox, TEST, store.url(), "update", &[]);

    assert_content(&named, OLD_CONTENT);
    let pruned: Vec<&str> = PLANTED[1..]
        .iter()
        .copied()
        .filter(|ts| !base.join(ts).exists())
        .collect();
    assert_eq!(pruned.len(), 2, "retention pruned {pruned:?}");
}

/// A site directory the run can only search, not list, hides whatever records
/// it holds from every reader that enumerates the sites. The run says so once,
/// and retention keeps every backup while such a site exists, since one of
/// them may be the backup a record there names.
#[test]
fn an_unlistable_site_dir_is_reported_and_retention_spares_every_backup() {
    const TEST: &str = "an_unlistable_site_dir_is_reported_and_retention_spares_every_backup";
    if nix::unistd::Uid::effective().is_root() {
        eprintln!("skipped: root lists a mode-0311 directory");
        return;
    }
    if let Some(child) = common::child() {
        let root = PathBuf::from(std::env::var("PU_RECORD_ROOT").unwrap());
        let site = root.join("home/dataA/color-schemes");
        let result = child_call(&child).update();
        assert_eq!(result.succeeded.len(), 1, "{result:?}");
        // The error the site's listing fails with, measured here so the
        // assertion does not carry a locale's translation of it.
        let error = std::fs::read_dir(&site).expect_err("the site stayed listable in the child");
        assert_eq!(
            warnings(),
            [format!(
                "cannot read the install records in {} ({}), so this run deletes no old backup",
                site.display(),
                error
            )]
        );
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let elsewhere = sandbox
        .home_dir()
        .join("dataA/color-schemes")
        .join(SCHEME_FILE);
    register_two_schemes(&sandbox, &elsewhere);
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("occupant"), b"x").unwrap();
    let base = backup_root(&sandbox);
    for ts in PLANTED {
        plant_file_backup(&base, ts, "color-schemes", "Other.colors", "planted\n");
    }
    let named = plant_file_backup(&base, PLANTED[0], "color-schemes", SCHEME_FILE, OLD_CONTENT);
    plant_record(&elsewhere, &named);
    let site = elsewhere.parent().unwrap().to_path_buf();
    let store = other_scheme_store();
    let root = root(&sandbox);
    let env = [("PU_RECORD_ROOT", root.as_os_str())];
    std::fs::set_permissions(&site, std::fs::Permissions::from_mode(0o311)).unwrap();

    run(&sandbox, TEST, store.url(), "update", &env);

    std::fs::set_permissions(&site, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_content(&named, OLD_CONTENT);
    for ts in PLANTED {
        assert!(
            base.join(ts).exists(),
            "retention pruned {ts} while a site could not be read"
        );
    }
}

/// A site the run can search, not list, when its walk warns, then made
/// listable by the fake `kpackagetool6` while the run installs: the warning's
/// promise holds to the run's own retention, which prunes nothing.
#[test]
fn a_site_readable_again_before_retention_still_spares_every_backup() {
    const TEST: &str = "a_site_readable_again_before_retention_still_spares_every_backup";
    if nix::unistd::Uid::effective().is_root() {
        eprintln!("skipped: root lists a mode-0311 directory");
        return;
    }
    if let Some(child) = common::child() {
        let data = PathBuf::from(std::env::var("XDG_DATA_HOME").unwrap());
        let site = data.join("color-schemes");
        let result = child_call(&child).update();
        assert_eq!(result.succeeded.len(), 2, "{result:?}");
        let warning: Vec<String> = warnings()
            .into_iter()
            .filter(|line| line.starts_with("cannot read the install records in "))
            .collect();
        let head = format!("cannot read the install records in {} (", site.display());
        assert!(
            warning.len() == 1
                && has_parts(&warning[0], &(head, "), so this run deletes no old backup")),
            "{warning:?}"
        );
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    install_old_comic(&sandbox);
    let elsewhere = sandbox
        .home_dir()
        .join("dataA/color-schemes")
        .join(SCHEME_FILE);
    install_scheme_at(&sandbox, &elsewhere, OLD_CONTENT);
    let base = backup_root(&sandbox);
    for ts in PLANTED {
        plant_file_backup(&base, ts, "color-schemes", "Other.colors", "planted\n");
    }
    let site = sandbox.data_home().join("color-schemes");
    std::fs::create_dir_all(&site).unwrap();
    std::fs::write(kpt(&sandbox).join("unlock-site"), b"").unwrap();
    std::fs::set_permissions(&site, std::fs::Permissions::from_mode(0o311)).unwrap();
    let store = FakeStore::start(Listing {
        extras: vec![ExtraEntry {
            id: COMIC_ID,
            name: COMIC_NAME,
            type_id: 155,
            version: "2.0",
            files: COMIC_ARCHIVE,
        }],
        ..Listing::default()
    });

    run(&sandbox, TEST, store.url(), "update", &[]);

    std::fs::set_permissions(&site, std::fs::Permissions::from_mode(0o755)).unwrap();
    for ts in PLANTED {
        assert!(
            base.join(ts).exists(),
            "retention pruned {ts} though the walk that warned could not read {site:?}"
        );
    }
}

/// A `check` that could not read a site, then an `install` in the same
/// process once the site reads whole: the warning's promise reaches the
/// install's retention, which prunes nothing.
#[test]
fn a_check_that_could_not_read_a_site_spares_a_later_installs_retention() {
    const TEST: &str = "a_check_that_could_not_read_a_site_spares_a_later_installs_retention";
    if nix::unistd::Uid::effective().is_root() {
        eprintln!("skipped: root lists a mode-0311 directory");
        return;
    }
    if let Some(child) = common::child() {
        capture_logs();
        let data = PathBuf::from(std::env::var("XDG_DATA_HOME").unwrap());
        let site = data.join("color-schemes");
        let error = std::fs::read_dir(&site).expect_err("the site was listable before the check");
        let config = config(&child);
        let report = check(&config, &Quiet).unwrap();
        assert_eq!(report.available_updates.len(), 1, "{report:?}");
        std::fs::set_permissions(&site, std::fs::Permissions::from_mode(0o755)).unwrap();
        let result = install(&report.available_updates, &config, &Quiet).unwrap();
        assert_eq!(result.succeeded.len(), 1, "{result:?}");
        let warning: Vec<String> = warnings()
            .into_iter()
            .filter(|line| line.starts_with("cannot read the install records in "))
            .collect();
        assert_eq!(
            warning,
            [format!(
                "cannot read the install records in {} ({}), so this run deletes no old backup",
                site.display(),
                error
            )]
        );
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let elsewhere = sandbox
        .home_dir()
        .join("dataA/color-schemes")
        .join(SCHEME_FILE);
    install_scheme_at(&sandbox, &elsewhere, OLD_CONTENT);
    let base = backup_root(&sandbox);
    for ts in PLANTED {
        plant_file_backup(&base, ts, "color-schemes", "Other.colors", "planted\n");
    }
    let site = sandbox.data_home().join("color-schemes");
    std::fs::create_dir_all(&site).unwrap();
    std::fs::set_permissions(&site, std::fs::Permissions::from_mode(0o311)).unwrap();
    let store = FakeStore::start(Listing::default());

    sandbox.run_child(TEST, store.url());

    std::fs::set_permissions(&site, std::fs::Permissions::from_mode(0o755)).unwrap();
    for ts in PLANTED {
        assert!(
            base.join(ts).exists(),
            "retention pruned {ts} though an earlier check could not read {site:?}"
        );
    }
}

/// The line a run logs for a KNewStuff registry it cannot read while it finds
/// where records may sit, `reason` being the error it names the file in.
fn unread_registry_line(reason: &str) -> String {
    format!(
        "cannot read a KNewStuff registry ({reason}); install records beside the components it lists may be skipped, so this run deletes no old backup"
    )
}

/// A `check` that could not read a KNewStuff registry, then an `install` in the
/// same process once the registry reads: the registry warning's promise reaches
/// the install's retention, which prunes nothing.
#[test]
fn a_check_that_could_not_read_a_registry_spares_a_later_installs_retention() {
    const TEST: &str = "a_check_that_could_not_read_a_registry_spares_a_later_installs_retention";
    if nix::unistd::Uid::effective().is_root() {
        eprintln!("skipped: root reads a mode-000 file");
        return;
    }
    if let Some(child) = common::child() {
        capture_logs();
        let registry = PathBuf::from(std::env::var("PU_RECORD_REGISTRY").unwrap());
        // The error the registry read fails with is measured here, so the
        // assertion does not carry a locale's translation of it.
        let error =
            std::fs::read(&registry).expect_err("the registry was readable before the check");
        let config = config(&child);
        let report = check(&config, &Quiet).unwrap();
        assert_eq!(report.available_updates.len(), 1, "{report:?}");
        std::fs::set_permissions(&registry, std::fs::Permissions::from_mode(0o644)).unwrap();
        let result = install(&report.available_updates, &config, &Quiet).unwrap();
        assert_eq!(result.succeeded.len(), 1, "{result:?}");
        let unread: Vec<String> = warnings()
            .into_iter()
            .filter(|line| line.starts_with("cannot read a KNewStuff registry ("))
            .collect();
        assert_eq!(
            unread,
            [unread_registry_line(&format!(
                "{}: {error}",
                registry.display()
            ))]
        );
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let elsewhere = sandbox
        .home_dir()
        .join("dataA/color-schemes")
        .join(SCHEME_FILE);
    install_scheme_at(&sandbox, &elsewhere, OLD_CONTENT);
    let base = backup_root(&sandbox);
    for ts in PLANTED {
        plant_file_backup(&base, ts, "color-schemes", "Other.colors", "planted\n");
    }
    let registry = sandbox.data_home().join("knewstuff3/comic.knsregistry");
    std::fs::write(
        &registry,
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE khotnewstuff3>\n<hotnewstuffregistry>\n</hotnewstuffregistry>\n",
    )
    .unwrap();
    std::fs::set_permissions(&registry, std::fs::Permissions::from_mode(0o000)).unwrap();
    let store = FakeStore::start(Listing::default());
    let env = [("PU_RECORD_REGISTRY", registry.as_os_str())];

    run(&sandbox, TEST, store.url(), "check", &env);

    for ts in PLANTED {
        assert!(
            base.join(ts).exists(),
            "retention pruned {ts} though an earlier check could not read {registry:?}"
        );
    }
}

const OTHER_COMIC_DIR: &str = "org.example.othercomic";

/// A second comic, outside this data home's type directories, that only the
/// comic registry places: a non-empty directory where a killed run's record
/// names a file backup, so the repair cannot put it back and the record stays
/// open. Returns the comic's path.
fn comic_only_the_registry_places(sandbox: &Sandbox, backup: &Path) -> PathBuf {
    let other = sandbox
        .home_dir()
        .join("dataA/plasma/comics")
        .join(OTHER_COMIC_DIR);
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("occupant"), b"x").unwrap();
    plant_record(&other, backup);
    other
}

/// The comic registry naming the data home's comic and `other`.
fn two_comics_registry(sandbox: &Sandbox, other: &Path) -> String {
    let entry = |name: &str, dir: &Path, id: u64| {
        format!(
            "  <stuff category=\"155\">\n    <name>{name}</name>\n    <providerid>api.kde-look.org</providerid>\n    <version>{OLD_VERSION}</version>\n    <installedfile>{}/metadata.json</installedfile>\n    <id>{id}</id>\n    <status>installed</status>\n  </stuff>\n",
            dir.display()
        )
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE khotnewstuff3>\n<hotnewstuffregistry>\n{}{}</hotnewstuffregistry>\n",
        entry(COMIC_NAME, &comic_dir(sandbox), COMIC_ID),
        entry("Other Comic", other, 999_075),
    )
}

/// A comic registry that names a component outside this data home's type
/// directories, beside which a killed run left its record, cannot be parsed:
/// the record's site is unknown, so the run says it could not read that
/// registry and retention keeps the backup the record names, while another
/// update of that type passes the keep count. With the same registry intact,
/// the record is found and its backup kept as well.
#[test]
fn an_unparseable_registry_keeps_the_backup_a_record_beside_its_component_names() {
    const TEST: &str =
        "an_unparseable_registry_keeps_the_backup_a_record_beside_its_component_names";
    if let Some(child) = common::child() {
        let corrupt = std::env::var("PU_RECORD_REGISTRY").is_ok();
        let result = child_call(&child).update();
        if corrupt {
            let registry = PathBuf::from(std::env::var("PU_RECORD_REGISTRY").unwrap());
            // The parser's own words for what it refused sit between the two
            // parts.
            let line = unread_registry_line("\0");
            let (opening, closing) = line.split_once('\0').unwrap();
            let unread: Vec<String> = warnings()
                .into_iter()
                .filter(|line| line.starts_with(opening))
                .collect();
            let head = format!(
                "{opening}failed to parse knewstuff registry: {}: ",
                registry.display()
            );
            assert_one_line(&unread, &(head, closing));
        } else {
            assert_eq!(result.succeeded.len(), 1, "{result:?}");
        }
        child.done();
        return;
    }
    for corrupt in [false, true] {
        let sandbox = Sandbox::new();
        install_old_comic(&sandbox);
        let base = backup_root(&sandbox);
        for ts in PLANTED {
            plant_file_backup(&base, ts, "comic-plugins", COMIC_DIR, "planted\n");
        }
        let named = plant_file_backup(
            &base,
            PLANTED[0],
            "comic-plugins",
            OTHER_COMIC_DIR,
            OLD_CONTENT,
        );
        let other = comic_only_the_registry_places(&sandbox, &named);
        let registry = sandbox.data_home().join("knewstuff3/comic.knsregistry");
        let text = two_comics_registry(&sandbox, &other);
        let text = if corrupt {
            text.strip_suffix("</hotnewstuffregistry>\n")
                .unwrap()
                .to_string()
        } else {
            text
        };
        std::fs::write(&registry, text).unwrap();
        let store = comic_store();
        let env: Vec<(&str, &OsStr)> = if corrupt {
            vec![("PU_RECORD_REGISTRY", registry.as_os_str())]
        } else {
            Vec::new()
        };

        run_checking(&sandbox, TEST, store.url(), "update", &env, None, || {
            assert!(
                named.exists(),
                "retention pruned the backup an open record names (registry corrupt: {corrupt})"
            );
            assert!(record_of(&other).exists(), "the record went");
            let pruned = PLANTED[1..]
                .iter()
                .filter(|ts| !base.join(ts).exists())
                .count();
            assert_eq!(pruned, if corrupt { 0 } else { 2 }, "corrupt: {corrupt}");
        });
    }
}

/// A KNewStuff registry that exists but cannot be read hides the components it
/// names as surely as one that does not parse: the run says so, naming the
/// file, and retention keeps every backup, the one a record beside such a
/// component names included.
#[test]
fn an_unreadable_registry_is_reported_and_retention_spares_every_backup() {
    const TEST: &str = "an_unreadable_registry_is_reported_and_retention_spares_every_backup";
    if nix::unistd::Uid::effective().is_root() {
        eprintln!("skipped: root reads a mode-000 file");
        return;
    }
    if let Some(child) = common::child() {
        let registry = PathBuf::from(std::env::var("PU_RECORD_REGISTRY").unwrap());
        let result = child_call(&child).update();
        assert_eq!(result.succeeded.len(), 1, "{result:?}");
        // The error reading the registry fails with, measured here so the
        // assertion does not carry a locale's translation of it.
        let error = std::fs::read(&registry).expect_err("the registry stayed readable");
        let named = format!("{}: {error}", registry.display());
        // The record scan's line comes from the repair, before the check's
        // store-id lookup names the same file on its own.
        assert_eq!(warnings(), [unread_registry_line(&named), named]);
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    register_two_schemes(&sandbox, &sandbox.scheme_path());
    std::fs::write(sandbox.scheme_path(), OLD_CONTENT).unwrap();
    let base = backup_root(&sandbox);
    for ts in PLANTED {
        plant_file_backup(&base, ts, "color-schemes", "Other.colors", "planted\n");
    }
    let named = plant_file_backup(
        &base,
        PLANTED[0],
        "color-schemes",
        OTHER_COMIC_DIR,
        OLD_CONTENT,
    );
    let other = comic_only_the_registry_places(&sandbox, &named);
    let registry = sandbox.data_home().join("knewstuff3/comic.knsregistry");
    std::fs::write(&registry, two_comics_registry(&sandbox, &other)).unwrap();
    std::fs::set_permissions(&registry, std::fs::Permissions::from_mode(0o000)).unwrap();
    let store = other_scheme_store();
    let env = [("PU_RECORD_REGISTRY", registry.as_os_str())];

    run_checking(&sandbox, TEST, store.url(), "update", &env, None, || {
        assert!(
            named.exists(),
            "retention pruned the backup an open record names"
        );
        for ts in PLANTED {
            assert!(
                base.join(ts).exists(),
                "retention pruned {ts} while a registry could not be read"
            );
        }
    });
}

/// A site whose path runs through a regular file cannot hold a record, so it
/// is no error: the run says nothing about it, and retention prunes past the
/// keep count as usual.
#[test]
fn a_site_under_a_regular_file_is_no_error() {
    const TEST: &str = "a_site_under_a_regular_file_is_no_error";
    if let Some(child) = common::child() {
        let result = child_call(&child).update();
        assert_eq!(result.succeeded.len(), 1, "{result:?}");
        assert_eq!(warnings(), Vec::<String>::new());
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let blocker = sandbox.home_dir().join("blocker");
    std::fs::write(&blocker, b"a file, not a directory").unwrap();
    register_two_schemes(&sandbox, &blocker.join("color-schemes").join(SCHEME_FILE));
    let base = backup_root(&sandbox);
    for ts in PLANTED {
        plant_file_backup(&base, ts, "color-schemes", "Other.colors", "planted\n");
    }
    let store = other_scheme_store();

    run_checking(&sandbox, TEST, store.url(), "update", &[], None, || {
        let pruned = PLANTED.iter().filter(|ts| !base.join(ts).exists()).count();
        assert_eq!(pruned, 2, "retention kept what it should prune");
    });
}

/// A site whose path holds a component too long to name cannot hold a record
/// any reader or the writer reaches, so it is no error either: the run says
/// nothing about it, and retention prunes past the keep count as usual.
#[test]
fn a_site_too_long_to_name_is_no_error() {
    const TEST: &str = "a_site_too_long_to_name_is_no_error";
    if let Some(child) = common::child() {
        let result = child_call(&child).update();
        assert_eq!(result.succeeded.len(), 1, "{result:?}");
        assert_eq!(warnings(), Vec::<String>::new());
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let too_long = sandbox.home_dir().join("n".repeat(300));
    register_two_schemes(&sandbox, &too_long.join("color-schemes").join(SCHEME_FILE));
    let base = backup_root(&sandbox);
    for ts in PLANTED {
        plant_file_backup(&base, ts, "color-schemes", "Other.colors", "planted\n");
    }
    let store = other_scheme_store();

    run_checking(&sandbox, TEST, store.url(), "update", &[], None, || {
        let pruned = PLANTED.iter().filter(|ts| !base.join(ts).exists()).count();
        assert_eq!(pruned, 2, "retention kept what it should prune");
    });
}

/// Retention keeps every backup of a component whose record cannot be read,
/// since any of them may be the one the record names.
#[test]
fn retention_keeps_every_backup_an_unreadable_record_may_name() {
    const TEST: &str = "retention_keeps_every_backup_an_unreadable_record_may_name";
    if nix::unistd::Uid::effective().is_root() {
        eprintln!("skipped: root reads a mode-000 file");
        return;
    }
    if let Some(child) = common::child() {
        let result = child_call(&child).update();
        assert_eq!(result.succeeded.len(), 1, "{result:?}");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    register_two_schemes(&sandbox, &sandbox.scheme_path());
    std::fs::write(sandbox.scheme_path(), OLD_CONTENT).unwrap();
    let base = backup_root(&sandbox);
    for ts in PLANTED {
        plant_file_backup(&base, ts, "color-schemes", SCHEME_FILE, OLD_CONTENT);
    }
    let record = plant_record(&sandbox.scheme_path(), &base.join(PLANTED[0]));
    std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o000)).unwrap();
    let store = other_scheme_store();

    run(&sandbox, TEST, store.url(), "update", &[]);

    for ts in PLANTED {
        assert!(
            base.join(ts)
                .join("color-schemes")
                .join(SCHEME_FILE)
                .exists(),
            "{ts} was pruned"
        );
    }
}

/// No reader looks beside a component outside the type directories that no
/// registry names, and no writer writes there: a selection naming such a
/// component installs unprotected with one warning.
#[test]
fn no_record_sits_outside_the_type_directories_and_the_registered_components() {
    const TEST: &str = "no_record_sits_outside_the_type_directories_and_the_registered_components";
    if let Some(child) = common::child() {
        let root = PathBuf::from(std::env::var("PU_RECORD_ROOT").unwrap());
        let target = root.join("home/elsewhere/color-schemes").join(SCHEME_FILE);
        match mode().as_str() {
            "selected" => {
                capture_logs();
                watch(record_of(&target));
                let component = InstalledComponent::new(
                    SCHEME_NAME,
                    SCHEME_FILE,
                    ComponentType::ColorScheme,
                    &target,
                )
                .with_version(OLD_VERSION);
                let host = child.store_url.trim_end_matches("/ocs/v1");
                let selected = AvailableUpdate::builder(component, SCHEME_ID, "2.0")
                    .download_url(format!("{host}/dl/fake-scheme.tar.gz"))
                    .build();
                let result = install(&[selected], &config(&child), &Quiet).unwrap();
                assert_eq!(result.succeeded.len(), 1, "{result:?}");
                let lines = warnings();
                assert_one_line(&lines, &row2_parts(SCHEME_NAME));
                assert!(lines[0].contains(" cannot be recorded), so"), "{lines:?}");
                let planted = root
                    .join("cache/plasmoid-updater/backups")
                    .join(TS)
                    .join("color-schemes")
                    .join(SCHEME_FILE);
                let mut planted = planted.into_os_string().into_encoded_bytes();
                planted.push(b'\n');
                assert_eq!(
                    snapshot(),
                    Seen::Held(planted),
                    "the install wrote a record where no reader looks"
                );
            }
            "check" => {
                let result = child_call(&child).check();
                assert!(result.diagnostics.is_empty(), "{result:?}");
                assert_eq!(warnings(), Vec::<String>::new());
            }
            _ => {
                drop(child_call(&child));
                assert_eq!(warnings(), Vec::<String>::new());
            }
        }
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let target = sandbox
        .home_dir()
        .join("elsewhere/color-schemes")
        .join(SCHEME_FILE);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, OLD_CONTENT).unwrap();
    let unlisted = sandbox.home_dir().join("elsewhere/color-schemes/X.colors");
    std::fs::write(&unlisted, "half\n").unwrap();
    let backup = plant_file_backup(
        &backup_root(&sandbox),
        TS,
        "color-schemes",
        "X.colors",
        OLD_CONTENT,
    );
    let record = plant_record(&unlisted, &backup);
    let written = std::fs::read(&record).unwrap();
    // The install of `target` reads no record beside it either.
    let planted = plant_file_backup(
        &backup_root(&sandbox),
        TS,
        "color-schemes",
        SCHEME_FILE,
        "planted\n",
    );
    let beside_target = plant_record(&target, &planted);
    let target_record = std::fs::read(&beside_target).unwrap();
    let store = FakeStore::start(Listing::default());
    let root = root(&sandbox);
    let env = [("PU_RECORD_ROOT", root.as_os_str())];

    for mode in ["check", "install", "selected"] {
        run(&sandbox, TEST, store.url(), mode, &env);
        assert_eq!(std::fs::read(&record).unwrap(), written, "{mode}");
        assert_eq!(
            std::fs::read(&beside_target).unwrap(),
            target_record,
            "{mode}"
        );
        assert_content(&unlisted, "half\n");
    }
    assert_content(&target, NEW_CONTENT);
}

/// A component a killed run left both in a hidden `-old` sibling and with a
/// pending record is reported once, through its record.
#[test]
fn a_stranded_tree_with_a_record_is_one_row() {
    const TEST: &str = "a_stranded_tree_with_a_record_is_one_row";
    if let Some(child) = common::child() {
        let root = PathBuf::from(std::env::var("PU_RECORD_ROOT").unwrap());
        let dest = root.join("data/plasma/comics").join(COMIC_DIR);
        let result = child_call(&child).check();
        let rows: Vec<(&str, &Path, &str)> = result
            .diagnostics
            .iter()
            .map(|row| (row.name.as_str(), row.path.as_path(), row.reason.as_str()))
            .collect();
        assert_eq!(rows, [(COMIC_DIR, dest.as_path(), PENDING_REASON)]);
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    install_old_comic(&sandbox);
    let dest = comic_dir(&sandbox);
    let stranded = dest.with_file_name(format!(".{COMIC_DIR}.plasmoid-updater-old"));
    std::fs::rename(&dest, &stranded).unwrap();
    let backup = backup_root(&sandbox)
        .join(TS)
        .join("comic-plugins")
        .join(COMIC_DIR);
    std::fs::create_dir_all(&backup).unwrap();
    std::fs::write(backup.join("metadata.json"), "{}").unwrap();
    plant_record(&dest, &backup);
    let store = comic_store();
    let root = root(&sandbox);
    let env = [("PU_RECORD_ROOT", root.as_os_str())];
    run(&sandbox, TEST, store.url(), "check", &env);
}

/// While another run holds the update lock, its records belong to installs
/// still running, so the check reports none and warns about none.
#[test]
fn a_check_under_another_runs_lock_reads_no_record() {
    const TEST: &str = "a_check_under_another_runs_lock_reads_no_record";
    if let Some(child) = common::child() {
        let result = child_call(&child).check();
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(result.available_updates.len(), 1, "{result:?}");
        assert_eq!(warnings(), Vec::<String>::new());
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let other = root(&sandbox).join("other-cache/plasmoid-updater/backups");
    let held = plant_file_backup(&other, TS, "color-schemes", SCHEME_FILE, "other\n");
    plant_record(&sandbox.scheme_path(), &held);
    let gone = sandbox.data_home().join("color-schemes/Gone.colors");
    plant_record(
        &gone,
        &lost_backup(&sandbox, "color-schemes", "Gone.colors"),
    );
    std::fs::create_dir_all(sandbox.cache_dir()).unwrap();
    let lock = std::fs::File::create(sandbox.cache_dir().join("plasmoid-updater.lock")).unwrap();
    lock.lock().unwrap();
    let store = FakeStore::start(Listing::default());
    run(&sandbox, TEST, store.url(), "check", &[]);
    drop(lock);
}

/// A valid record whose backup sits under another type's subdirectory of the
/// backup base is restored from before the update, never removed as corrupt.
#[test]
fn a_record_naming_another_types_backup_is_restored_from() {
    const TEST: &str = "a_record_naming_another_types_backup_is_restored_from";
    if let Some(child) = common::child() {
        let result = child_call(&child).update();
        assert_eq!(result.succeeded.len(), 1, "{result:?}");
        assert_eq!(warnings(), Vec::<String>::new());
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    std::fs::write(sandbox.scheme_path(), "half\n").unwrap();
    let backup = plant_file_backup(
        &backup_root(&sandbox),
        TS,
        "icons",
        SCHEME_FILE,
        OLD_CONTENT,
    );
    let record = plant_record(&sandbox.scheme_path(), &backup);
    let store = FakeStore::start(Listing::default());
    run(&sandbox, TEST, store.url(), "update", &[]);
    assert_content(&sandbox.scheme_path(), NEW_CONTENT);
    assert!(!record.exists());
    let own = own_backups(&sandbox, "color-schemes", SCHEME_FILE, &[TS]);
    assert_eq!(own.len(), 1, "{own:?}");
    assert_content(&own[0], OLD_CONTENT);
}

/// Under a `..`-spelled data home, a pending record beside a component the
/// registry names and whose file is missing is reported once, through the
/// record, and not offered as an update.
#[test]
fn a_pending_record_beside_a_missing_component_is_one_row_and_no_update() {
    const TEST: &str = "a_pending_record_beside_a_missing_component_is_one_row_and_no_update";
    if let Some(child) = common::child() {
        let root = PathBuf::from(std::env::var("PU_RECORD_ROOT").unwrap());
        let scheme = root.join("data/color-schemes").join(SCHEME_FILE);
        let result = child_call(&child).check();
        assert!(result.available_updates.is_empty(), "{result:?}");
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        let row = &result.diagnostics[0];
        assert_eq!(
            (row.name.as_str(), row.path.as_path(), row.reason.as_str()),
            (SCHEME_NAME, scheme.as_path(), PENDING_REASON)
        );
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    register_scheme_at(&sandbox, &sandbox.scheme_path());
    let backup = plant_file_backup(
        &backup_root(&sandbox),
        TS,
        "color-schemes",
        SCHEME_FILE,
        OLD_CONTENT,
    );
    plant_record(&sandbox.scheme_path(), &backup);
    let store = FakeStore::start(Listing::default());
    let data = dotdot_data_home(&sandbox);
    let root = root(&sandbox);
    let env = [
        ("XDG_DATA_HOME", data.as_os_str()),
        ("PU_RECORD_ROOT", root.as_os_str()),
    ];
    run(&sandbox, TEST, store.url(), "check", &env);
}

const ICONS_ID: u64 = 999_073;
const CURSORS_ID: u64 = 999_074;
const THEME_ARCHIVE: &[(&str, &str)] = &[("Breeze/index.theme", "[Icon Theme]\nName=Breeze\n")];

/// One directory two components share (an icon theme and a cursor theme the
/// registry both names at `~/.icons/Breeze`) gets one record row per
/// component, each named and placed as that component, and neither is
/// offered; a record no component holds gets one row named by its file name.
#[test]
fn a_record_row_answers_for_each_component_at_its_path_and_for_none() {
    const TEST: &str = "a_record_row_answers_for_each_component_at_its_path_and_for_none";
    if let Some(child) = common::child() {
        let root = PathBuf::from(std::env::var("PU_RECORD_ROOT").unwrap());
        let theme = root.join("home/.icons/Breeze");
        let gone = root.join("data/color-schemes/Gone.colors");
        let result = child_call(&child).check();
        assert!(result.available_updates.is_empty(), "{result:?}");
        let mut rows: Vec<(&str, &Path)> = result
            .diagnostics
            .iter()
            .map(|row| (row.name.as_str(), row.path.as_path()))
            .collect();
        rows.sort();
        assert_eq!(
            rows,
            [
                ("Breeze Cursors", theme.as_path()),
                ("Breeze Icons", theme.as_path()),
                ("Gone.colors", gone.as_path()),
            ],
            "{result:?}"
        );
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let theme = sandbox.home_dir().join(".icons/Breeze");
    std::fs::create_dir_all(theme.join("cursors")).unwrap();
    std::fs::write(theme.join("index.theme"), "[Icon Theme]\nName=Breeze\n").unwrap();
    common::write_registry(
        &sandbox,
        "icons.knsregistry",
        132,
        "Breeze Icons",
        OLD_VERSION,
        &theme.display().to_string(),
        Some(ICONS_ID),
    );
    common::write_registry(
        &sandbox,
        "xcursor.knsregistry",
        107,
        "Breeze Cursors",
        OLD_VERSION,
        &format!("{}/*", theme.display()),
        Some(CURSORS_ID),
    );
    let other = root(&sandbox).join("other-cache/plasmoid-updater/backups");
    let held = plant_file_backup(&other, TS, "icons", "Breeze", "other\n");
    plant_record(&theme, &held);
    let gone = sandbox.data_home().join("color-schemes/Gone.colors");
    let gone_backup = plant_file_backup(&other, TS, "color-schemes", "Gone.colors", "other\n");
    plant_record(&gone, &gone_backup);
    let store = FakeStore::start(Listing {
        extras: vec![
            ExtraEntry {
                id: ICONS_ID,
                name: "Breeze Icons",
                type_id: 132,
                version: "2.0",
                files: THEME_ARCHIVE,
            },
            ExtraEntry {
                id: CURSORS_ID,
                name: "Breeze Cursors",
                type_id: 107,
                version: "2.0",
                files: THEME_ARCHIVE,
            },
        ],
        ..Listing::default()
    });
    let root = root(&sandbox);
    let env = [("PU_RECORD_ROOT", root.as_os_str())];
    run(&sandbox, TEST, store.url(), "check", &env);
}

/// A record the install cannot remove once its files are in place is not a
/// landed update: the next run's repair would restore the backup it names over
/// the update, so the run refuses the component and keeps the record.
#[test]
fn a_record_that_cannot_be_removed_after_the_install_refuses_it() {
    const TEST: &str = "a_record_that_cannot_be_removed_after_the_install_refuses_it";
    if let Some(child) = common::child() {
        let data = PathBuf::from(std::env::var("XDG_DATA_HOME").unwrap());
        let record = record_of(&data.join("plasma/comics").join(COMIC_DIR));
        let result = child_call(&child).update();
        assert!(result.succeeded.is_empty(), "{result:?}");
        assert_eq!(result.failed.len(), 1, "{result:?}");
        assert_eq!(result.failed[0].kind, FailureKind::Registry, "{result:?}");
        let error = std::fs::remove_file(&record).expect_err("the record was removable");
        assert_eq!(
            result.failed[0].message,
            format!(
                "updated, but its install record {} could not be removed ({error}), so the KNewStuff registry was not updated; delete that record by hand, or the next update that can read and write its directory first puts the old version back from its backup",
                record.display()
            )
        );
        assert!(record.exists(), "the refused component lost its record");
        assert_eq!(warnings().len(), 1, "{:?}", warnings());
        assert!(
            warnings()[0].starts_with(&format!("failed to remove {}: ", record.display())),
            "{:?}",
            warnings()
        );
        child.done();
        return;
    }
    if nix::unistd::Uid::effective().is_root() {
        return;
    }
    let sandbox = Sandbox::new();
    install_old_comic(&sandbox);
    std::fs::write(kpt(&sandbox).join("lock-type-dir"), b"").unwrap();
    let store = comic_store();

    run(&sandbox, TEST, store.url(), "update", &[]);

    let comics = sandbox.data_home().join("plasma/comics");
    std::fs::set_permissions(&comics, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        record_of(&comic_dir(&sandbox)).exists(),
        "the refused update removed its record"
    );
    assert_content(
        &comic_dir(&sandbox).join("contents/code/main.js"),
        "// 2.0\n",
    );
}

/// A registry the run cannot read may name the component's directory, so the
/// install reads the component's own record before it runs, though the walk
/// could not reach its site: a record left unrestored would be put back over
/// the landed update by the next run.
#[test]
fn a_record_the_walk_cannot_see_is_still_settled_before_the_install() {
    const TEST: &str = "a_record_the_walk_cannot_see_is_still_settled_before_the_install";
    if let Some(child) = common::child() {
        let elsewhere = PathBuf::from(std::env::var("PU_RECORD_SCHEME").unwrap());
        let record = record_of(&elsewhere);
        let component = InstalledComponent::new(
            SCHEME_NAME,
            SCHEME_FILE,
            ComponentType::ColorScheme,
            &elsewhere,
        )
        .with_version(OLD_VERSION);
        let host = child.store_url.trim_end_matches("/ocs/v1");
        let selected = AvailableUpdate::builder(component, SCHEME_ID, "2.0")
            .download_url(format!("{host}/dl/fake-scheme.tar.gz"))
            .build();
        let result = install(&[selected], &config(&child), &Quiet).unwrap();
        assert_eq!(result.failed.len(), 1, "{result:?}");
        assert_eq!(result.failed[0].kind, FailureKind::Registry, "{result:?}");
        assert!(
            !record.exists(),
            "the install went on over the record the walk could not see"
        );
        assert_content(&elsewhere, NEW_CONTENT);
        child.done();
        return;
    }
    if nix::unistd::Uid::effective().is_root() {
        return;
    }
    let sandbox = Sandbox::new();
    let elsewhere = sandbox
        .home_dir()
        .join("dataA/color-schemes")
        .join(SCHEME_FILE);
    install_scheme_at(&sandbox, &elsewhere, "half\n");
    let backup = plant_file_backup(
        &backup_root(&sandbox),
        TS,
        "color-schemes",
        SCHEME_FILE,
        OLD_CONTENT,
    );
    let record = plant_record(&elsewhere, &backup);
    let registry = sandbox
        .data_home()
        .join("knewstuff3/colorschemes.knsregistry");
    std::fs::set_permissions(&registry, std::fs::Permissions::from_mode(0o000)).unwrap();
    let store = FakeStore::start(Listing::default());
    let env = [("PU_RECORD_SCHEME", elsewhere.as_os_str())];

    run(&sandbox, TEST, store.url(), "update", &env);

    assert!(!record.exists(), "the record the walk could not see stayed");
    assert_content(&elsewhere, NEW_CONTENT);
}
