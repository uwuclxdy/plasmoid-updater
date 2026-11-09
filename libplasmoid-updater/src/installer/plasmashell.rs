// SPDX-License-Identifier: GPL-3.0-or-later

use std::process::{Command, Output, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::{Error, Result, types::ComponentType};

const PLASMASHELL_BUS_NAME: &str = "org.kde.plasmashell";
const VENDOR_UNIT: &str = "plasma-plasmashell.service";
const DBUS_DESTINATION: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const SYSTEMD_DESTINATION: &str = "org.freedesktop.systemd1";
const SYSTEMD_MANAGER_PATH: &str = "/org/freedesktop/systemd1";
const UNIT_NOT_LOADED: &str = "does not belong to any loaded unit";
const RESTART_WAIT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(250);

fn get_user_id() -> String {
    std::env::var("UID").unwrap_or_else(|_| nix::unistd::Uid::current().as_raw().to_string())
}

/// Restarts the systemd user unit whose main process owns
/// `org.kde.plasmashell` on the session bus, then waits for a new plasmashell
/// to take that name. With no plasmashell running it starts
/// `plasma-plasmashell.service`. The core never calls this; the caller decides
/// when (and whether) to.
///
/// # Errors
///
/// Returns [`Error::Restart`] when plasmashell runs in a unit that is not a
/// service whose main process it is, when it belongs to no systemd user unit
/// or no systemd user manager is on the session bus, when the old plasmashell
/// still owns the bus name after the restart, or when no new plasmashell owns
/// it within ten seconds of the restart. Returns [`Error::MissingTool`] when
/// `busctl` or `systemctl` is not on `PATH`, [`Error::Config`] naming the
/// command when it cannot be spawned, [`Error::Command`] naming the command,
/// its status and the reason it printed when it exits nonzero, and
/// [`Error::Parse`] when `busctl` prints output this library cannot parse.
pub fn restart_plasmashell() -> Result<()> {
    let (unit, old_pid) = target_unit()?;

    run("systemctl", &["--user", "restart", &unit])?;

    await_new_owner(&unit, old_pid)
}

/// The unit to restart and the pid of the plasmashell it runs. With no
/// plasmashell running the answer is the vendor unit and no pid, so the
/// restart starts plasmashell from its packaged unit.
fn target_unit() -> Result<(String, Option<u32>)> {
    if !bus_name_has_owner(PLASMASHELL_BUS_NAME)? {
        return Ok((VENDOR_UNIT.to_owned(), None));
    }

    let pid = bus_owner_pid()?;
    if !bus_name_has_owner(SYSTEMD_DESTINATION)? {
        return Err(no_unit_error(pid));
    }
    let Some(object_path) = unit_object_path_for_pid(pid)? else {
        return Err(no_unit_error(pid));
    };

    let unit = unit_id(&object_path)?;
    if !unit.ends_with(".service") || service_main_pid(&object_path)? != pid {
        return Err(Error::Restart(format!(
            "plasmashell (pid {pid}) runs in {unit}, not in a service plasmoid-updater could restart"
        )));
    }

    Ok((unit, Some(pid)))
}

/// Waits until a plasmashell other than `old_pid` owns the bus name. A pid
/// that still equals `old_pid` is the old process, so waiting cannot help.
fn await_new_owner(unit: &str, old_pid: Option<u32>) -> Result<()> {
    let deadline = Instant::now() + RESTART_WAIT;

    loop {
        match current_owner()? {
            Some(pid) if old_pid == Some(pid) => {
                return Err(Error::Restart(format!(
                    "{unit} restarted, but plasmashell (pid {pid}) still owns {PLASMASHELL_BUS_NAME}"
                )));
            }
            Some(_) => return Ok(()),
            None => {}
        }

        if Instant::now() >= deadline {
            return Err(Error::Restart(format!(
                "{unit} restarted, but no new plasmashell took {PLASMASHELL_BUS_NAME} within {} s",
                RESTART_WAIT.as_secs()
            )));
        }

        sleep(POLL_INTERVAL);
    }
}

/// The pid that owns `org.kde.plasmashell` now, or `None` while nothing does.
///
/// A failed `GetConnectionUnixProcessID` counts as `None`: the name was owned
/// a moment before, so the call fails when its owner exits in between, and
/// the next poll re-checks the name.
fn current_owner() -> Result<Option<u32>> {
    if !bus_name_has_owner(PLASMASHELL_BUS_NAME)? {
        return Ok(None);
    }

    match bus_owner_pid() {
        Ok(pid) => Ok(Some(pid)),
        Err(Error::Command { .. }) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Asks the session bus whether any process owns `name`.
fn bus_name_has_owner(name: &str) -> Result<bool> {
    let output = run(
        "busctl",
        &[
            "--user",
            "call",
            DBUS_DESTINATION,
            DBUS_PATH,
            DBUS_DESTINATION,
            "NameHasOwner",
            "s",
            name,
        ],
    )?;

    parse_bool("NameHasOwner", &output.stdout)
}

/// The error for a plasmashell whose pid no restartable systemd user unit
/// runs, whether because systemd is not on the session bus or because it does
/// not know the pid.
fn no_unit_error(pid: u32) -> Error {
    Error::Restart(format!(
        "plasmashell (pid {pid}) belongs to no systemd user unit plasmoid-updater could restart"
    ))
}

fn bus_owner_pid() -> Result<u32> {
    let output = run(
        "busctl",
        &[
            "--user",
            "call",
            DBUS_DESTINATION,
            DBUS_PATH,
            DBUS_DESTINATION,
            "GetConnectionUnixProcessID",
            "s",
            PLASMASHELL_BUS_NAME,
        ],
    )?;

    parse_u32("GetConnectionUnixProcessID", &output.stdout)
}

/// The systemd object path of the unit that owns `pid`, or `None` when
/// systemd answers that the pid belongs to no loaded unit.
fn unit_object_path_for_pid(pid: u32) -> Result<Option<String>> {
    let pid = pid.to_string();
    let output = command_output(
        "busctl",
        &[
            "--user",
            "call",
            SYSTEMD_DESTINATION,
            SYSTEMD_MANAGER_PATH,
            "org.freedesktop.systemd1.Manager",
            "GetUnitByPID",
            "u",
            &pid,
        ],
    )?;

    if !output.status.success() {
        if String::from_utf8_lossy(&output.stderr).contains(UNIT_NOT_LOADED) {
            return Ok(None);
        }
        return Err(command_error("busctl", &output));
    }

    parse_quoted("GetUnitByPID", "o", &output.stdout).map(Some)
}

fn unit_id(object_path: &str) -> Result<String> {
    let output = run(
        "busctl",
        &[
            "--user",
            "get-property",
            SYSTEMD_DESTINATION,
            object_path,
            "org.freedesktop.systemd1.Unit",
            "Id",
        ],
    )?;

    parse_quoted("Unit.Id", "s", &output.stdout)
}

fn service_main_pid(object_path: &str) -> Result<u32> {
    let output = run(
        "busctl",
        &[
            "--user",
            "get-property",
            SYSTEMD_DESTINATION,
            object_path,
            "org.freedesktop.systemd1.Service",
            "MainPID",
        ],
    )?;

    parse_u32("Service.MainPID", &output.stdout)
}

/// Builds a command that can reach the user's session bus even when the
/// caller neither inherited nor set the two environment variables for it.
fn user_command(tool: &'static str) -> Command {
    let mut cmd = Command::new(tool);

    let uid = get_user_id();

    if std::env::var("DBUS_SESSION_BUS_ADDRESS").is_err() {
        cmd.env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path=/run/user/{uid}/bus"),
        );
    }

    if std::env::var("XDG_RUNTIME_DIR").is_err() {
        cmd.env("XDG_RUNTIME_DIR", format!("/run/user/{uid}"));
    }

    cmd
}

/// Runs `tool` to completion and returns its output, leaving the exit status
/// to the caller. stderr is always captured, never inherited, so the library
/// prints nothing: a failed call's reason travels inside [`Error::Command`],
/// and a caller whose answer arrives as a failure message reads it off the
/// captured stderr.
fn command_output(tool: &'static str, args: &[&str]) -> Result<Output> {
    user_command(tool)
        .args(args)
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => Error::MissingTool { tool },
            _ => Error::Config(format!("failed to run {tool}: {error}")),
        })
}

/// [`command_output`], with a nonzero exit status turned into
/// [`Error::Command`].
fn run(tool: &'static str, args: &[&str]) -> Result<Output> {
    let output = command_output(tool, args)?;

    if output.status.success() {
        Ok(output)
    } else {
        Err(command_error(tool, &output))
    }
}

fn command_error(tool: &'static str, output: &Output) -> Error {
    Error::Command {
        command: tool,
        status: output.status,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn output_line<'a>(call: &str, stdout: &'a [u8]) -> Result<&'a str> {
    let text = std::str::from_utf8(stdout)
        .map_err(|error| parse_error(call, &format!("output is not UTF-8: {error}")))?;

    text.lines()
        .next()
        .ok_or_else(|| parse_error(call, "no output"))
}

fn parse_error(call: &str, detail: &str) -> Error {
    Error::Parse {
        what: "busctl output",
        detail: format!("{call}: {detail}"),
    }
}

fn unexpected(call: &str, line: &str) -> Error {
    parse_error(call, &format!("unexpected output {line:?}"))
}

fn parse_bool(call: &str, stdout: &[u8]) -> Result<bool> {
    let line = output_line(call, stdout)?;

    match line {
        "b true" => Ok(true),
        "b false" => Ok(false),
        _ => Err(unexpected(call, line)),
    }
}

fn parse_u32(call: &str, stdout: &[u8]) -> Result<u32> {
    let line = output_line(call, stdout)?;

    line.strip_prefix("u ")
        .ok_or_else(|| unexpected(call, line))?
        .parse()
        .map_err(|_| unexpected(call, line))
}

/// Reads a `busctl` string or object path answer, which busctl prints
/// C-escaped (measured 2026-11-07 on systemd 262): a unit name's `\` comes out
/// as `\\`. A unit name holds only ASCII letters, digits and `:-_.\@`, and an
/// object path only ASCII letters, digits, `_` and `/`, so a bare `"` or any
/// escape other than `\\` and `\"` means the answer is not one this module
/// asked for.
fn parse_quoted(call: &str, sigil: &str, stdout: &[u8]) -> Result<String> {
    let line = output_line(call, stdout)?;
    let quoted = line
        .strip_prefix(sigil)
        .and_then(|rest| rest.strip_prefix(" \""))
        .and_then(|rest| rest.strip_suffix('"'))
        .ok_or_else(|| unexpected(call, line))?;

    let mut value = String::with_capacity(quoted.len());
    let mut chars = quoted.chars();
    while let Some(ch) = chars.next() {
        let ch = match ch {
            '\\' => match chars.next() {
                Some(escaped @ ('\\' | '"')) => escaped,
                _ => return Err(unexpected(call, line)),
            },
            '"' => return Err(unexpected(call, line)),
            ch => ch,
        };
        value.push(ch);
    }

    Ok(value)
}

/// Returns `true` if this component type needs a plasmashell restart after
/// an update (widgets, plasma styles, global themes, splash screens and
/// KWin switchers).
pub(crate) fn requires_restart(component_type: ComponentType) -> bool {
    matches!(
        component_type,
        ComponentType::PlasmaWidget
            | ComponentType::PlasmaStyle
            | ComponentType::GlobalTheme
            | ComponentType::SplashScreen
            | ComponentType::KWinSwitcher
    )
}
