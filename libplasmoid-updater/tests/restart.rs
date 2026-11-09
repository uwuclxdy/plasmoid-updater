//! `restart_plasmashell` restarts the systemd user unit that actually runs
//! plasmashell, the one whose main process owns `org.kde.plasmashell` on the
//! session bus, refuses any other unit, and reports success only once a new
//! plasmashell owns that name.
//!
//! The child-process pattern is the sandbox harness's: replacing `PATH` in
//! this process would need `unsafe`, so each test re-runs itself with a `PATH`
//! holding only the fake `busctl` and `systemctl` it wrote. The fakes use
//! shell builtins alone, since that `PATH` holds no `cat`.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use common::Sandbox;

const CHILD_VAR: &str = "PLASMOID_UPDATER_TEST_RESTART_CHILD";
const OUTCOME_VAR: &str = "PLASMOID_UPDATER_TEST_RESTART_OUTCOME";
const RESTARTS_VAR: &str = "PLASMOID_UPDATER_TEST_RESTART_COUNT";

const TRANSIENT_UNIT: &str = "app-org.kde.plasmashell@3f9c0a7d1e2b4c5d8e9f0a1b2c3d4e5f.service";
const VENDOR_UNIT: &str = "plasma-plasmashell.service";

/// What `org.kde.plasmashell` looks like once `systemctl restart` returns.
enum After {
    /// The old plasmashell still owns the name.
    Unchanged,
    /// Nothing owns the name.
    Gone,
    /// A new plasmashell with this pid owns the name.
    Owner(u32),
    /// `systemctl restart` exits 1 and the name stays as it was.
    Fail,
    /// `systemctl restart` prints this reason on stderr, exits 1, and the name
    /// stays as it was. The reason may span several lines.
    FailWithReason(&'static str),
    /// Nothing owns the name until this many `busctl` calls have gone by, then
    /// a new plasmashell with this pid does.
    OwnerAfterCalls { calls: u32, pid: u32 },
    /// A new plasmashell with this pid owns the name, but the first pid query
    /// fails as if the owner seen a moment before had just exited.
    OwnerVanishesOnce(u32),
}

/// The session as the fake `busctl` and `systemctl` present it.
struct Session {
    /// The pid owning `org.kde.plasmashell`, or `None` when no plasmashell runs.
    owner: Option<u32>,
    /// What systemd answers for the owner's pid: the unit id and that unit's
    /// main pid, or `None` when the pid belongs to no loaded user unit.
    unit: Option<(&'static str, u32)>,
    after: After,
}

/// The fake tools' working state, kept beside the sandbox that holds them.
struct Fakes {
    sandbox: Sandbox,
    state: PathBuf,
}

impl Fakes {
    fn new() -> Self {
        let sandbox = Sandbox::new();
        let state = sandbox.data_home().join("restart-state");
        std::fs::create_dir_all(&state).unwrap();
        Self { sandbox, state }
    }

    fn busctl(&self, session: &Session) -> &Self {
        if let Some(owner) = session.owner {
            std::fs::write(self.state.join("owner"), format!("{owner}\n")).unwrap();
        }
        let unit_by_pid = match session.unit {
            Some(_) => "echo 'o \"/org/freedesktop/systemd1/unit/fake\"'",
            None => {
                "echo \"Call failed: PID $owner does not belong to any loaded unit.\" >&2; exit 1"
            }
        };
        let (unit, main_pid) = session.unit.unwrap_or(("none.service", 0));
        // Only a service has a main process; systemd refuses the query on
        // anything else, a scope included (measured 2026-11-07 on systemd 262).
        let main_pid = if unit.ends_with(".service") {
            format!("echo 'u {main_pid}'")
        } else {
            "echo 'Failed to get property MainPID on interface org.freedesktop.systemd1.Service: Unknown interface org.freedesktop.systemd1.Service or property MainPID.' >&2; exit 1".to_owned()
        };
        // busctl prints a string C-escaped, so a unit name's backslash comes
        // out doubled (measured 2026-11-07 on systemd 262).
        let busctl_unit = unit.replace('\\', r"\\");
        self.sandbox.fake_tool(
            "busctl",
            &format!(
                r#"state='{state}'
printf '%s\n' "$*" >> "$state/busctl.log"
owner=''
[ -f "$state/owner" ] && read -r owner < "$state/owner"
calls=''
[ -f "$state/calls" ] && read -r calls < "$state/calls"
if [ -n "$calls" ]; then
  if [ "$calls" -eq 0 ]; then
    read -r owner < "$state/next-owner"
    echo "$owner" > "$state/owner"
    : > "$state/calls"
  else
    echo $((calls - 1)) > "$state/calls"
  fi
fi
manager=true
[ -f "$state/no-manager" ] && manager=false
if [ -f "$state/busctl-warning" ]; then while IFS= read -r line || [ -n "$line" ]; do printf '%s\n' "$line" >&2; done < "$state/busctl-warning"; fi
args=$(printf '%s|' "$@")
case "$args" in
  '--user|call|org.freedesktop.DBus|/org/freedesktop/DBus|org.freedesktop.DBus|NameHasOwner|s|org.kde.plasmashell|')
    if [ -n "$owner" ]; then echo 'b true'; else echo 'b false'; fi ;;
  '--user|call|org.freedesktop.DBus|/org/freedesktop/DBus|org.freedesktop.DBus|NameHasOwner|s|org.freedesktop.systemd1|')
    echo "b $manager" ;;
  '--user|call|org.freedesktop.DBus|/org/freedesktop/DBus|org.freedesktop.DBus|GetConnectionUnixProcessID|s|org.kde.plasmashell|')
    vanish=''
    [ -f "$state/vanish" ] && read -r vanish < "$state/vanish"
    if [ "$vanish" = once ]; then
      echo done > "$state/vanish"
      echo 'Call failed: The connection does not exist' >&2; exit 1
    fi
    if [ -n "$owner" ]; then echo "u $owner"; else echo 'Call failed: The connection does not exist' >&2; exit 1; fi ;;
  "--user|call|org.freedesktop.systemd1|/org/freedesktop/systemd1|org.freedesktop.systemd1.Manager|GetUnitByPID|u|$owner|")
    if [ "$manager" = false ]; then echo 'Call failed: The name is not activatable' >&2; exit 1; fi
    if [ -f "$state/GetUnitByPID-error" ]; then read -r line < "$state/GetUnitByPID-error"; printf '%s\n' "$line" >&2; exit 1; fi
    if [ -f "$state/GetUnitByPID-warning" ]; then while IFS= read -r line || [ -n "$line" ]; do printf '%s\n' "$line" >&2; done < "$state/GetUnitByPID-warning"; fi
    {unit_by_pid} ;;
  '--user|get-property|org.freedesktop.systemd1|/org/freedesktop/systemd1/unit/fake|org.freedesktop.systemd1.Service|MainPID|')
    if [ -f "$state/MainPID-error" ]; then read -r line < "$state/MainPID-error"; printf '%s\n' "$line" >&2; exit 1; fi
    {main_pid} ;;
  '--user|get-property|org.freedesktop.systemd1|/org/freedesktop/systemd1/unit/fake|org.freedesktop.systemd1.Unit|Id|')
    if [ -f "$state/Id-error" ]; then read -r line < "$state/Id-error"; printf '%s\n' "$line" >&2; exit 1; fi
    if [ -f "$state/unit-id" ]; then read -r line < "$state/unit-id"; printf '%s\n' "$line"
    else printf '%s\n' 's "{busctl_unit}"'; fi ;;
  *)
    echo "unexpected busctl call: $*" >&2; exit 99 ;;
esac
"#,
                state = self.state.display(),
            ),
        );
        self
    }

    fn broken_busctl(&self, script: &str) -> &Self {
        self.sandbox.fake_tool("busctl", script);
        self
    }

    /// Makes `busctl` answer the `Unit.Id` query with `line`, verbatim.
    fn unit_id_line(&self, line: &str) -> &Self {
        std::fs::write(self.state.join("unit-id"), format!("{line}\n")).unwrap();
        self
    }

    /// Makes `busctl` fail `query` (`GetUnitByPID`, `Id` or `MainPID`) with
    /// `reason` on stderr.
    fn query_fails(&self, query: &str, reason: &str) -> &Self {
        std::fs::write(
            self.state.join(format!("{query}-error")),
            format!("{reason}\n"),
        )
        .unwrap();
        self
    }

    /// Makes `busctl` answer `GetUnitByPID` as usual and exit 0, after
    /// printing `warning` on stderr, every line of it verbatim.
    fn unit_lookup_warns(&self, warning: &str) -> &Self {
        std::fs::write(self.state.join("GetUnitByPID-warning"), warning).unwrap();
        self
    }

    /// Makes every `busctl` call print `warning` on stderr, every line of it
    /// verbatim, before it answers as usual.
    fn busctl_always_warns(&self, warning: &str) -> &Self {
        std::fs::write(self.state.join("busctl-warning"), warning).unwrap();
        self
    }

    /// Makes `systemctl restart` print `warning` on stderr, every line of it
    /// verbatim, before it acts as the session's [`After`] says. Replayed
    /// from a file, so the warning may hold any character a shell literal
    /// could not.
    fn systemctl_warns(&self, warning: &str) -> &Self {
        std::fs::write(self.state.join("restart-warning"), warning).unwrap();
        self
    }

    /// Makes the first pid query fail as if the owner had just exited.
    fn pid_query_fails_once(&self) -> &Self {
        std::fs::write(self.state.join("vanish"), "once\n").unwrap();
        self
    }

    /// A session bus with no systemd user manager on it.
    fn without_systemd_manager(&self) -> &Self {
        std::fs::write(self.state.join("no-manager"), b"").unwrap();
        self
    }

    fn systemctl(&self, session: &Session) -> &Self {
        let after = match session.after {
            After::Unchanged => ":".to_owned(),
            After::Gone => r#": > "$state/owner""#.to_owned(),
            After::Owner(pid) => format!(r#"echo {pid} > "$state/owner""#),
            After::Fail => "exit 1".to_owned(),
            After::FailWithReason(reason) => format!("printf '%s\\n' '{reason}' >&2; exit 1"),
            After::OwnerVanishesOnce(pid) => {
                format!(r#"echo {pid} > "$state/owner"; echo once > "$state/vanish""#)
            }
            After::OwnerAfterCalls { calls, pid } => format!(
                r#": > "$state/owner"; echo {pid} > "$state/next-owner"; echo {calls} > "$state/calls""#
            ),
        };
        self.sandbox.fake_tool(
            "systemctl",
            &format!(
                r#"state='{state}'
args=$(printf '%s|' "$@")
printf '%s\n' "$args" >> "$state/systemctl.log"
case "$args" in
  '--user|restart|'*)
    if [ -f "$state/restart-warning" ]; then while IFS= read -r line || [ -n "$line" ]; do printf '%s\n' "$line" >&2; done < "$state/restart-warning"; fi
    {after} ;;
  *)
    echo "unexpected systemctl call: $*" >&2; exit 99 ;;
esac
"#,
                state = self.state.display(),
            ),
        );
        self
    }

    /// Re-runs `test` as a child with only the fakes on `PATH`, and returns
    /// the result's `Debug` line and its `Display` line (empty for `Ok`).
    ///
    /// The library prints nothing, so every run fails when the child or a
    /// tool it ran wrote to stderr, or wrote to stdout beside libtest's own
    /// lines: a failed tool's reason travels inside the error instead. A run
    /// that logged a warning fails too; [`Fakes::run_logging`] is for a test
    /// that pins one.
    ///
    /// The child gets dead bus addresses, so a call that slips past the fakes
    /// fails instead of reaching the live user or system bus; as root a slip
    /// would reach root's managers, so the suite refuses to run there.
    fn run(&self, test: &str) -> (String, String) {
        let outcome = self.run_logging(test);
        assert_eq!(
            self.warnings(),
            Vec::<String>::new(),
            "the restart logged a warning"
        );
        outcome
    }

    /// [`Fakes::run`] for a test that pins what the restart logged: it leaves
    /// the warnings to [`Fakes::warnings`].
    fn run_logging(&self, test: &str) -> (String, String) {
        self.run_restarts(test, 1)
    }

    /// [`Fakes::run_logging`], with the child calling `restart_plasmashell`
    /// `restarts` times in one process. The outcome is the last call's; the
    /// warnings are every call's.
    fn run_restarts(&self, test: &str, restarts: u32) -> (String, String) {
        assert!(
            !nix::unistd::Uid::effective().is_root(),
            "the restart tests refuse to run as root"
        );
        let outcome = self.state.join("outcome");
        let dead_bus = format!("unix:path={}", self.state.join("no-bus").display());
        let output = self
            .sandbox
            .command(std::env::current_exe().unwrap())
            .args([test, "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD_VAR, "1")
            .env(OUTCOME_VAR, &outcome)
            .env(RESTARTS_VAR, restarts.to_string())
            .env("DBUS_SESSION_BUS_ADDRESS", &dead_bus)
            .env("DBUS_SYSTEM_BUS_ADDRESS", &dead_bus)
            .env("PATH", self.sandbox.bin_dir())
            .output()
            .unwrap();
        assert!(
            output.status.success() && outcome.exists(),
            "child run failed (body completed: {})\n--- stdout\n{}\n--- stderr\n{}",
            outcome.exists(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            output.stderr.is_empty(),
            "the restart wrote to stderr:\n{}",
            String::from_utf8_lossy(&output.stderr),
        );
        // `--nocapture` lets a stray write reach this stdout, where it lands
        // among (or inside) libtest's own lines for the one test.
        let stdout = String::from_utf8_lossy(&output.stdout);
        let test_line = format!("test {test} ... ok");
        assert!(
            stdout.lines().all(|line| {
                line.is_empty()
                    || line == "running 1 test"
                    || line == test_line
                    || line.starts_with("test result: ok. 1 passed;")
            }),
            "the restart wrote to stdout:\n{stdout}"
        );
        let text = std::fs::read_to_string(&outcome).unwrap();
        let (debug, display) = text.split_once('\n').unwrap();
        (debug.to_owned(), display.to_owned())
    }

    /// Every `systemctl` call that restarted a unit, as its argument line.
    fn restarts(&self) -> Vec<String> {
        read_log(&self.state.join("systemctl.log"))
            .into_iter()
            .filter(|line| line.contains("restart"))
            .collect()
    }

    /// Every record the library logged at warn level or above during the last
    /// [`Fakes::run`], one `LEVEL: text` line each, in order.
    fn warnings(&self) -> Vec<String> {
        let path = self.state.join("warnings");
        assert!(path.exists(), "the child wrote no warnings file");
        read_log(&path)
    }

    /// How many times `busctl` was called.
    fn busctl_calls(&self) -> usize {
        read_log(&self.state.join("busctl.log")).len()
    }
}

fn read_log(path: &Path) -> Vec<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => text.lines().map(str::to_owned).collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("{}: {error}", path.display()),
    }
}

/// In the child: runs the restart and writes its outcome for the parent.
/// Returns `true` when this process is the child.
fn child_restart() -> bool {
    if std::env::var_os(CHILD_VAR).is_none() {
        return false;
    }
    let outcome = PathBuf::from(std::env::var_os(OUTCOME_VAR).unwrap());
    log::set_logger(&WarningLog).unwrap();
    log::set_max_level(log::LevelFilter::Warn);
    let restarts: u32 = std::env::var(RESTARTS_VAR).unwrap().parse().unwrap();
    let mut result = libplasmoid_updater::restart_plasmashell();
    for _ in 1..restarts {
        result = libplasmoid_updater::restart_plasmashell();
    }
    let display = match &result {
        Ok(()) => String::new(),
        Err(error) => error.to_string(),
    };
    let warnings = WARNINGS.lock().unwrap().join("\n");
    std::fs::write(outcome.with_file_name("warnings"), warnings).unwrap();
    std::fs::write(outcome, format!("{result:?}\n{display}")).unwrap();
    true
}

/// Every warning or error the library logged in the child, as `LEVEL: text`.
static WARNINGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The child's logger: keeps each record at warn level or above in
/// [`WARNINGS`] and prints nothing, so the parent can pin the record exactly.
struct WarningLog;

impl log::Log for WarningLog {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            WARNINGS
                .lock()
                .unwrap()
                .push(format!("{}: {}", record.level(), record.args()));
        }
    }

    fn flush(&self) {}
}

fn restart_line(unit: &str) -> String {
    format!("--user|restart|--|{unit}|")
}

#[test]
fn restarts_the_transient_unit_that_runs_plasmashell() {
    if child_restart() {
        return;
    }
    let session = Session {
        owner: Some(4242),
        unit: Some((TRANSIENT_UNIT, 4242)),
        after: After::Owner(4343),
    };
    let fakes = Fakes::new();
    fakes.busctl(&session).systemctl(&session);

    let (debug, _) = fakes.run("restarts_the_transient_unit_that_runs_plasmashell");

    assert_eq!(debug, "Ok(())");
    assert_eq!(fakes.restarts(), [restart_line(TRANSIENT_UNIT)]);
}

#[test]
fn waits_for_a_transient_unit_whose_plasmashell_takes_the_name_late() {
    if child_restart() {
        return;
    }
    let session = Session {
        owner: Some(4242),
        unit: Some((TRANSIENT_UNIT, 4242)),
        after: After::OwnerAfterCalls {
            calls: 3,
            pid: 4343,
        },
    };
    let fakes = Fakes::new();
    fakes.busctl(&session).systemctl(&session);

    let (debug, _) = fakes.run("waits_for_a_transient_unit_whose_plasmashell_takes_the_name_late");

    assert_eq!(debug, "Ok(())");
    assert_eq!(fakes.restarts(), [restart_line(TRANSIENT_UNIT)]);
}

#[test]
fn keeps_waiting_when_the_owner_vanishes_between_two_queries() {
    if child_restart() {
        return;
    }
    let session = Session {
        owner: Some(4242),
        unit: Some((TRANSIENT_UNIT, 4242)),
        after: After::OwnerVanishesOnce(4343),
    };
    let fakes = Fakes::new();
    fakes.busctl(&session).systemctl(&session);

    let (debug, _) = fakes.run("keeps_waiting_when_the_owner_vanishes_between_two_queries");

    assert_eq!(debug, "Ok(())");
    assert_eq!(fakes.restarts(), [restart_line(TRANSIENT_UNIT)]);
}

#[test]
fn restarts_the_vendor_unit_when_it_runs_plasmashell() {
    if child_restart() {
        return;
    }
    let session = Session {
        owner: Some(4242),
        unit: Some((VENDOR_UNIT, 4242)),
        after: After::Owner(4343),
    };
    let fakes = Fakes::new();
    fakes.busctl(&session).systemctl(&session);

    let (debug, _) = fakes.run("restarts_the_vendor_unit_when_it_runs_plasmashell");

    assert_eq!(debug, "Ok(())");
    assert_eq!(fakes.restarts(), [restart_line(VENDOR_UNIT)]);
}

#[test]
fn restarts_a_service_whose_name_holds_an_escape_by_its_real_name() {
    if child_restart() {
        return;
    }
    // A desktop file id's `-` is escaped in the transient unit's name.
    let unit = r"app-my\x2dplasmashell@3f9c0a7d1e2b4c5d8e9f0a1b2c3d4e5f.service";
    let session = Session {
        owner: Some(4242),
        unit: Some((unit, 4242)),
        after: After::Owner(4343),
    };
    let fakes = Fakes::new();
    fakes.busctl(&session).systemctl(&session);

    let (debug, _) = fakes.run("restarts_a_service_whose_name_holds_an_escape_by_its_real_name");

    assert_eq!(debug, "Ok(())");
    assert_eq!(fakes.restarts(), [restart_line(unit)]);
}

#[test]
fn a_unit_name_with_an_unknown_escape_is_a_parse_error() {
    if child_restart() {
        return;
    }
    let session = Session {
        owner: Some(4242),
        unit: Some((TRANSIENT_UNIT, 4242)),
        after: After::Owner(4343),
    };
    let fakes = Fakes::new();
    fakes
        .unit_id_line(r#"s "app-my\x2dplasmashell@3f9c0a7d1e2b4c5d8e9f0a1b2c3d4e5f.service""#)
        .busctl(&session)
        .systemctl(&session);

    let (debug, _) = fakes.run("a_unit_name_with_an_unknown_escape_is_a_parse_error");

    assert!(debug.starts_with("Err(Parse {"), "{debug}");
    assert_eq!(fakes.restarts(), Vec::<String>::new());
}

#[test]
fn starts_the_vendor_unit_when_no_plasmashell_runs() {
    if child_restart() {
        return;
    }
    let session = Session {
        owner: None,
        unit: None,
        after: After::Owner(4343),
    };
    let fakes = Fakes::new();
    fakes.busctl(&session).systemctl(&session);

    let (debug, _) = fakes.run("starts_the_vendor_unit_when_no_plasmashell_runs");

    assert_eq!(debug, "Ok(())");
    assert_eq!(fakes.restarts(), [restart_line(VENDOR_UNIT)]);
}

#[test]
fn a_plasmashell_inside_a_terminal_scope_is_left_alone() {
    if child_restart() {
        return;
    }
    let session = Session {
        owner: Some(4242),
        unit: Some(("app-org.kde.konsole-4242.scope", 0)),
        after: After::Owner(4343),
    };
    let fakes = Fakes::new();
    fakes.busctl(&session).systemctl(&session);

    let (debug, display) = fakes.run("a_plasmashell_inside_a_terminal_scope_is_left_alone");

    assert_eq!(
        debug,
        r#"Err(Restart("plasmashell (pid 4242) runs in app-org.kde.konsole-4242.scope, not in a service plasmoid-updater could restart"))"#
    );
    assert_eq!(
        display,
        "plasmashell (pid 4242) runs in app-org.kde.konsole-4242.scope, not in a service plasmoid-updater could restart"
    );
    assert_eq!(fakes.restarts(), Vec::<String>::new());
}

#[test]
fn a_service_whose_main_process_is_not_plasmashell_is_left_alone() {
    if child_restart() {
        return;
    }
    let unit = "app-org.kde.konsole@0d1e2f3a4b5c6d7e8f9a0b1c2d3e4f5a.service";
    let session = Session {
        owner: Some(4242),
        unit: Some((unit, 999)),
        after: After::Owner(4343),
    };
    let fakes = Fakes::new();
    fakes.busctl(&session).systemctl(&session);

    let (debug, _) = fakes.run("a_service_whose_main_process_is_not_plasmashell_is_left_alone");

    assert_eq!(
        debug,
        format!(
            r#"Err(Restart("plasmashell (pid 4242) runs in {unit}, not in a service plasmoid-updater could restart"))"#
        )
    );
    assert_eq!(fakes.restarts(), Vec::<String>::new());
}

#[test]
fn a_plasmashell_in_no_user_unit_is_left_alone() {
    if child_restart() {
        return;
    }
    let session = Session {
        owner: Some(4242),
        unit: None,
        after: After::Owner(4343),
    };
    let fakes = Fakes::new();
    fakes.busctl(&session).systemctl(&session);

    let (debug, _) = fakes.run("a_plasmashell_in_no_user_unit_is_left_alone");

    assert_eq!(
        debug,
        r#"Err(Restart("plasmashell (pid 4242) belongs to no systemd user unit plasmoid-updater could restart"))"#
    );
    assert_eq!(fakes.restarts(), Vec::<String>::new());
}

#[test]
fn a_plasmashell_with_no_systemd_manager_is_left_alone() {
    if child_restart() {
        return;
    }
    let session = Session {
        owner: Some(4242),
        unit: None,
        after: After::Owner(4343),
    };
    let fakes = Fakes::new();
    fakes
        .without_systemd_manager()
        .busctl(&session)
        .systemctl(&session);

    let (debug, _) = fakes.run("a_plasmashell_with_no_systemd_manager_is_left_alone");

    assert_eq!(
        debug,
        r#"Err(Restart("plasmashell (pid 4242) belongs to no systemd user unit plasmoid-updater could restart"))"#
    );
    assert_eq!(fakes.restarts(), Vec::<String>::new());
}

#[test]
fn a_plasmashell_still_running_after_the_restart_is_an_error() {
    if child_restart() {
        return;
    }
    let session = Session {
        owner: Some(4242),
        unit: Some((VENDOR_UNIT, 4242)),
        after: After::Unchanged,
    };
    let fakes = Fakes::new();
    fakes.busctl(&session).systemctl(&session);

    let (debug, _) = fakes.run("a_plasmashell_still_running_after_the_restart_is_an_error");

    assert_eq!(
        debug,
        r#"Err(Restart("plasma-plasmashell.service restarted, but plasmashell (pid 4242) still owns org.kde.plasmashell"))"#
    );
    assert_eq!(fakes.restarts(), [restart_line(VENDOR_UNIT)]);
}

#[test]
fn a_plasmashell_that_never_comes_back_is_an_error() {
    if child_restart() {
        return;
    }
    let session = Session {
        owner: Some(4242),
        unit: Some((TRANSIENT_UNIT, 4242)),
        after: After::Gone,
    };
    let fakes = Fakes::new();
    fakes.busctl(&session).systemctl(&session);

    let started = Instant::now();
    let (debug, _) = fakes.run("a_plasmashell_that_never_comes_back_is_an_error");
    let elapsed = started.elapsed();

    assert_eq!(
        debug,
        format!(
            r#"Err(Restart("{TRANSIENT_UNIT} restarted, but no new plasmashell took org.kde.plasmashell within 10 s"))"#
        )
    );
    assert_eq!(fakes.restarts(), [restart_line(TRANSIENT_UNIT)]);
    // The wait is bounded work: it lasts the advertised 10 s, stops soon
    // after, and polls at an interval: neither a hot loop nor one late check.
    assert!(
        elapsed >= Duration::from_secs(10) && elapsed < Duration::from_secs(20),
        "{elapsed:?}"
    );
    let calls = fakes.busctl_calls();
    assert!((16..=200).contains(&calls), "{calls} busctl calls");
}

#[test]
fn a_missing_busctl_is_a_missing_tool() {
    if child_restart() {
        return;
    }
    let session = Session {
        owner: Some(4242),
        unit: Some((VENDOR_UNIT, 4242)),
        after: After::Owner(4343),
    };
    let fakes = Fakes::new();
    fakes.systemctl(&session);

    let (debug, _) = fakes.run("a_missing_busctl_is_a_missing_tool");

    assert_eq!(debug, r#"Err(MissingTool { tool: "busctl" })"#);
    assert_eq!(fakes.restarts(), Vec::<String>::new());
}

#[test]
fn a_failing_busctl_is_a_command_error_naming_it() {
    if child_restart() {
        return;
    }
    let session = Session {
        owner: Some(4242),
        unit: Some((VENDOR_UNIT, 4242)),
        after: After::Owner(4343),
    };
    let fakes = Fakes::new();
    fakes.broken_busctl("exit 1\n").systemctl(&session);

    let (debug, display) = fakes.run("a_failing_busctl_is_a_command_error_naming_it");

    assert!(
        debug.starts_with(r#"Err(Command { command: "busctl""#),
        "{debug}"
    );
    // A tool that gave no reason leaves the status line bare.
    assert_eq!(display, "busctl exited with status 1");
    assert_eq!(fakes.restarts(), Vec::<String>::new());
}

#[test]
fn a_failing_busctl_names_its_reason_in_the_error() {
    if child_restart() {
        return;
    }
    let session = Session {
        owner: Some(4242),
        unit: Some((VENDOR_UNIT, 4242)),
        after: After::Owner(4343),
    };
    let fakes = Fakes::new();
    fakes
        .broken_busctl("echo 'Failed to connect to bus: No such file or directory' >&2\nexit 1\n")
        .systemctl(&session);

    let (debug, display) = fakes.run("a_failing_busctl_names_its_reason_in_the_error");

    assert!(
        debug.starts_with(r#"Err(Command { command: "busctl""#),
        "{debug}"
    );
    assert_eq!(
        display,
        "busctl exited with status 1: Failed to connect to bus: No such file or directory"
    );
    assert_eq!(fakes.restarts(), Vec::<String>::new());
}

#[test]
fn a_failed_unit_lookup_names_its_reason_in_the_error() {
    if child_restart() {
        return;
    }
    let session = Session {
        owner: Some(4242),
        unit: Some((TRANSIENT_UNIT, 4242)),
        after: After::Owner(4343),
    };
    let fakes = Fakes::new();
    fakes
        .query_fails("GetUnitByPID", "Call failed: Access denied")
        .busctl(&session)
        .systemctl(&session);

    let (debug, display) = fakes.run("a_failed_unit_lookup_names_its_reason_in_the_error");

    assert!(
        debug.starts_with(r#"Err(Command { command: "busctl""#),
        "{debug}"
    );
    assert_eq!(
        display,
        "busctl exited with status 1: Call failed: Access denied"
    );
    assert_eq!(fakes.restarts(), Vec::<String>::new());
}

#[test]
fn a_failed_unit_id_query_names_its_reason_in_the_error() {
    if child_restart() {
        return;
    }
    let session = Session {
        owner: Some(4242),
        unit: Some((TRANSIENT_UNIT, 4242)),
        after: After::Owner(4343),
    };
    let fakes = Fakes::new();
    fakes
        .query_fails(
            "Id",
            "Failed to get property Id on interface org.freedesktop.systemd1.Unit: Access denied",
        )
        .busctl(&session)
        .systemctl(&session);

    let (debug, display) = fakes.run("a_failed_unit_id_query_names_its_reason_in_the_error");

    assert!(
        debug.starts_with(r#"Err(Command { command: "busctl""#),
        "{debug}"
    );
    assert_eq!(
        display,
        "busctl exited with status 1: Failed to get property Id on interface org.freedesktop.systemd1.Unit: Access denied"
    );
    assert_eq!(fakes.restarts(), Vec::<String>::new());
}

#[test]
fn a_failed_main_pid_query_names_its_reason_in_the_error() {
    if child_restart() {
        return;
    }
    let session = Session {
        owner: Some(4242),
        unit: Some((TRANSIENT_UNIT, 4242)),
        after: After::Owner(4343),
    };
    let fakes = Fakes::new();
    fakes
        .query_fails(
            "MainPID",
            "Failed to get property MainPID on interface org.freedesktop.systemd1.Service: Access denied",
        )
        .busctl(&session)
        .systemctl(&session);

    let (debug, display) = fakes.run("a_failed_main_pid_query_names_its_reason_in_the_error");

    assert!(
        debug.starts_with(r#"Err(Command { command: "busctl""#),
        "{debug}"
    );
    assert_eq!(
        display,
        "busctl exited with status 1: Failed to get property MainPID on interface org.freedesktop.systemd1.Service: Access denied"
    );
    assert_eq!(fakes.restarts(), Vec::<String>::new());
}

#[test]
fn a_failed_pid_query_before_the_restart_names_its_reason_in_the_error() {
    if child_restart() {
        return;
    }
    let session = Session {
        owner: Some(4242),
        unit: Some((TRANSIENT_UNIT, 4242)),
        after: After::Owner(4343),
    };
    let fakes = Fakes::new();
    fakes
        .pid_query_fails_once()
        .busctl(&session)
        .systemctl(&session);

    let (debug, display) =
        fakes.run("a_failed_pid_query_before_the_restart_names_its_reason_in_the_error");

    assert!(
        debug.starts_with(r#"Err(Command { command: "busctl""#),
        "{debug}"
    );
    assert_eq!(
        display,
        "busctl exited with status 1: Call failed: The connection does not exist"
    );
    assert_eq!(fakes.restarts(), Vec::<String>::new());
}

#[test]
fn unparseable_busctl_output_is_a_parse_error() {
    if child_restart() {
        return;
    }
    let session = Session {
        owner: Some(4242),
        unit: Some((VENDOR_UNIT, 4242)),
        after: After::Owner(4343),
    };
    let fakes = Fakes::new();
    fakes
        .broken_busctl("echo 'something busctl never prints'\n")
        .systemctl(&session);

    let (debug, _) = fakes.run("unparseable_busctl_output_is_a_parse_error");

    assert!(debug.starts_with("Err(Parse {"), "{debug}");
    assert_eq!(fakes.restarts(), Vec::<String>::new());
}

#[test]
fn a_missing_systemctl_is_a_missing_tool() {
    if child_restart() {
        return;
    }
    let session = Session {
        owner: Some(4242),
        unit: Some((VENDOR_UNIT, 4242)),
        after: After::Owner(4343),
    };
    let fakes = Fakes::new();
    fakes.busctl(&session);

    let (debug, _) = fakes.run("a_missing_systemctl_is_a_missing_tool");

    assert_eq!(debug, r#"Err(MissingTool { tool: "systemctl" })"#);
}

#[test]
fn a_failing_systemctl_is_a_command_error_naming_it() {
    if child_restart() {
        return;
    }
    let session = Session {
        owner: Some(4242),
        unit: Some((VENDOR_UNIT, 4242)),
        after: After::Fail,
    };
    let fakes = Fakes::new();
    fakes.busctl(&session).systemctl(&session);

    let (debug, display) = fakes.run("a_failing_systemctl_is_a_command_error_naming_it");

    assert!(
        debug.starts_with(r#"Err(Command { command: "systemctl""#),
        "{debug}"
    );
    assert_eq!(display, "systemctl exited with status 1");
    assert_eq!(fakes.restarts(), [restart_line(VENDOR_UNIT)]);
}

#[test]
fn a_failing_systemctl_names_its_reason_in_the_error() {
    if child_restart() {
        return;
    }
    let session = Session {
        owner: Some(4242),
        unit: Some((VENDOR_UNIT, 4242)),
        after: After::FailWithReason(
            "Failed to restart plasma-plasmashell.service: Unit plasma-plasmashell.service not loaded.",
        ),
    };
    let fakes = Fakes::new();
    fakes.busctl(&session).systemctl(&session);

    let (debug, display) = fakes.run("a_failing_systemctl_names_its_reason_in_the_error");

    assert!(
        debug.starts_with(r#"Err(Command { command: "systemctl""#),
        "{debug}"
    );
    assert_eq!(
        display,
        "systemctl exited with status 1: Failed to restart plasma-plasmashell.service: Unit plasma-plasmashell.service not loaded."
    );
    assert_eq!(fakes.restarts(), [restart_line(VENDOR_UNIT)]);
}

#[test]
fn a_reason_over_several_lines_renders_as_one_line() {
    if child_restart() {
        return;
    }
    let session = Session {
        owner: Some(4242),
        unit: Some((VENDOR_UNIT, 4242)),
        after: After::FailWithReason(
            "  Job for plasma-plasmashell.service failed.  \n\n\tSee the journal for details.\n",
        ),
    };
    let fakes = Fakes::new();
    fakes.busctl(&session).systemctl(&session);

    let (_, display) = fakes.run("a_reason_over_several_lines_renders_as_one_line");

    // Each line trimmed, blank lines dropped, the rest joined by one space.
    assert_eq!(
        display,
        "systemctl exited with status 1: Job for plasma-plasmashell.service failed. See the journal for details."
    );
}

#[test]
fn a_restart_that_succeeds_with_a_warning_logs_it_as_one_line() {
    if child_restart() {
        return;
    }
    // systemctl's own text (measured 2026-11-09 in systemd 262's systemctl
    // binary), printed on a restart that still succeeds when the unit file
    // changed on disk since the last daemon-reload.
    let session = Session {
        owner: Some(4242),
        unit: Some((VENDOR_UNIT, 4242)),
        after: After::Owner(4343),
    };
    let fakes = Fakes::new();
    fakes
        .busctl(&session)
        .systemctl(&session)
        .systemctl_warns("Warning: The unit file, source configuration file or drop-ins of plasma-plasmashell.service changed on disk. Run 'systemctl --user daemon-reload' to reload units.\n");

    let (debug, _) =
        fakes.run_logging("a_restart_that_succeeds_with_a_warning_logs_it_as_one_line");

    assert_eq!(debug, "Ok(())");
    assert_eq!(fakes.restarts(), [restart_line(VENDOR_UNIT)]);
    assert_eq!(
        fakes.warnings(),
        [
            "WARN: systemctl: Warning: The unit file, source configuration file or drop-ins of plasma-plasmashell.service changed on disk. Run 'systemctl --user daemon-reload' to reload units."
        ]
    );
}

#[test]
fn a_unit_lookup_that_succeeds_with_a_warning_logs_it_as_one_line() {
    if child_restart() {
        return;
    }
    let session = Session {
        owner: Some(4242),
        unit: Some((TRANSIENT_UNIT, 4242)),
        after: After::Owner(4343),
    };
    let fakes = Fakes::new();
    fakes
        .busctl(&session)
        // No newline after the last line: a tool's stderr need not end in one.
        .unit_lookup_warns("  Some notice from busctl.\n\n\tA second line of it.  ")
        .systemctl(&session);

    let (debug, _) =
        fakes.run_logging("a_unit_lookup_that_succeeds_with_a_warning_logs_it_as_one_line");

    assert_eq!(debug, "Ok(())");
    assert_eq!(fakes.restarts(), [restart_line(TRANSIENT_UNIT)]);
    // Joined like a failed call's reason: each line trimmed, blank lines
    // dropped, the rest joined by one space.
    assert_eq!(
        fakes.warnings(),
        ["WARN: busctl: Some notice from busctl. A second line of it."]
    );
}

#[test]
fn a_unit_lookup_that_succeeds_with_blank_stderr_logs_nothing() {
    if child_restart() {
        return;
    }
    let session = Session {
        owner: Some(4242),
        unit: Some((TRANSIENT_UNIT, 4242)),
        after: After::Owner(4343),
    };
    let fakes = Fakes::new();
    fakes
        .busctl(&session)
        .unit_lookup_warns(" \n\t\n")
        .systemctl(&session);

    // `Fakes::run` fails on any logged warning.
    let (debug, _) = fakes.run("a_unit_lookup_that_succeeds_with_blank_stderr_logs_nothing");

    assert_eq!(debug, "Ok(())");
    assert_eq!(fakes.restarts(), [restart_line(TRANSIENT_UNIT)]);
}

#[test]
fn a_warning_repeated_within_one_restart_logs_once() {
    if child_restart() {
        return;
    }
    // Nothing owns the name for a few polls after the restart, so the same
    // busctl notice arrives on many calls.
    let session = Session {
        owner: Some(4242),
        unit: Some((TRANSIENT_UNIT, 4242)),
        after: After::OwnerAfterCalls {
            calls: 3,
            pid: 4343,
        },
    };
    let fakes = Fakes::new();
    fakes
        .busctl(&session)
        .busctl_always_warns("Notice: every call.\n")
        .unit_lookup_warns("Lookup notice.\n")
        .systemctl(&session)
        .systemctl_warns("Notice: every call.\n");

    let (debug, _) = fakes.run_logging("a_warning_repeated_within_one_restart_logs_once");

    assert_eq!(debug, "Ok(())");
    // 6 calls find and check the unit and the poll that finds the new owner
    // makes 2, so a 9th call is a poll that found nothing: the notice
    // repeated inside the wait too.
    assert!(
        fakes.busctl_calls() >= 9,
        "{} busctl calls",
        fakes.busctl_calls()
    );
    // One record per distinct line: the unit lookup's stderr renders as a
    // different line, and the same text from systemctl is another tool's.
    assert_eq!(
        fakes.warnings(),
        [
            "WARN: busctl: Notice: every call.",
            "WARN: busctl: Notice: every call. Lookup notice.",
            "WARN: systemctl: Notice: every call.",
        ]
    );
}

#[test]
fn each_restart_logs_its_own_warnings() {
    if child_restart() {
        return;
    }
    let session = Session {
        owner: Some(4242),
        unit: Some((TRANSIENT_UNIT, 4242)),
        after: After::Owner(4343),
    };
    let fakes = Fakes::new();
    fakes
        .busctl(&session)
        .busctl_always_warns("Notice: every call.\n")
        .systemctl(&session);

    let (debug, _) = fakes.run_restarts("each_restart_logs_its_own_warnings", 2);

    // The second call finds the new plasmashell outside the unit's main
    // process and refuses; it still logs the notice its own calls printed.
    assert_eq!(
        debug,
        format!(
            r#"Err(Restart("plasmashell (pid 4343) runs in {TRANSIENT_UNIT}, not in a service plasmoid-updater could restart"))"#
        )
    );
    assert_eq!(fakes.restarts(), [restart_line(TRANSIENT_UNIT)]);
    assert_eq!(
        fakes.warnings(),
        [
            "WARN: busctl: Notice: every call.",
            "WARN: busctl: Notice: every call.",
        ]
    );
}

#[test]
fn a_restart_after_a_refused_one_logs_its_own_warnings() {
    if child_restart() {
        return;
    }
    // Both calls refuse a plasmashell inside a terminal's scope, so neither
    // reaches the restart: a fresh start must not depend on the earlier
    // call's outcome.
    let session = Session {
        owner: Some(4242),
        unit: Some(("app-org.kde.konsole-4242.scope", 0)),
        after: After::Owner(4343),
    };
    let fakes = Fakes::new();
    fakes
        .busctl(&session)
        .busctl_always_warns("Notice: every call.\n")
        .systemctl(&session);

    let (debug, _) = fakes.run_restarts("a_restart_after_a_refused_one_logs_its_own_warnings", 2);

    assert_eq!(
        debug,
        r#"Err(Restart("plasmashell (pid 4242) runs in app-org.kde.konsole-4242.scope, not in a service plasmoid-updater could restart"))"#
    );
    assert_eq!(fakes.restarts(), Vec::<String>::new());
    assert_eq!(
        fakes.warnings(),
        [
            "WARN: busctl: Notice: every call.",
            "WARN: busctl: Notice: every call.",
        ]
    );
}
