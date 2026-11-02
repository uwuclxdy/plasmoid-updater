// SPDX-FileCopyrightText: 2025 uwuclxdy
// SPDX-License-Identifier: GPL-3.0-or-later

mod cli_config;
mod exit_code;
mod logger;

use std::io::{IsTerminal, Write as _};

use clap::{CommandFactory as _, Parser, Subcommand};

use cli_config::CliConfig;
use exit_code::ExitCode;
use libplasmoid_updater as lib;

#[derive(Parser)]
#[command(name = "plasmoid-updater")]
#[command(about = "update kde plasma components from the kde store")]
#[command(version)]
#[command(long_version = concat!(
    env!("CARGO_PKG_VERSION"),
    "\nCopyright (C) 2025 uwuclxdy",
    "\nLicense GPLv3+: GNU GPL version 3 or later <https://gnu.org/licenses/gpl.html>",
    "\nThis is free software: you are free to change and redistribute it.",
    "\nThere is NO WARRANTY, to the extent permitted by law.",
))]
#[command(disable_help_subcommand = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    #[arg(
        long,
        global = true,
        help = "operate on system-wide components (needs sudo)"
    )]
    system: bool,

    #[arg(long, help = "open configuration file in editor")]
    edit_config: bool,

    #[arg(long, global = true, help = "skip KDE Plasma detection")]
    skip_plasma_detection: bool,

    #[arg(short, long, global = true, help = "errors only on stderr")]
    quiet: bool,

    #[arg(
        short,
        long,
        global = true,
        action = clap::ArgAction::Count,
        conflicts_with = "quiet",
        help = "more diagnostics on stderr (repeat for debug)"
    )]
    verbose: u8,
}

#[derive(Subcommand)]
enum Commands {
    #[command(about = "check for available updates")]
    Check {
        #[arg(
            long,
            value_delimiter = ',',
            value_name = "NAMES",
            help = "names to skip for this run (comma-separated or repeatable)"
        )]
        ignore: Vec<String>,
    },
    #[command(about = "list all installed components")]
    ListInstalled,
    #[command(about = "update components")]
    Update {
        #[arg(help = "component name or directory to update")]
        component: Option<String>,
        #[arg(long, help = "automatically restart plasmashell")]
        restart_plasma: bool,
        #[arg(long, help = "do not restart plasmashell")]
        no_restart_plasma: bool,
        #[arg(short = 'y', long, help = "automatically confirm all updates")]
        yes: bool,
        #[arg(long, help = "list what would be installed and change nothing")]
        dry_run: bool,
        #[arg(
            long,
            value_delimiter = ',',
            value_name = "NAMES",
            help = "names to skip for this run (comma-separated or repeatable)"
        )]
        ignore: Vec<String>,
    },
    #[command(about = "print a shell completion script")]
    Completions {
        #[arg(value_name = "SHELL", help = "shell to print completions for")]
        shell: clap_complete::Shell,
    },
}

#[derive(Clone, Copy)]
enum RestartDecision {
    Always,
    Never,
    Prompt,
}

#[derive(Default)]
struct UpdateArgs {
    component: Option<String>,
    restart_plasma: bool,
    no_restart_plasma: bool,
    yes: bool,
    dry_run: bool,
    ignore: Vec<String>,
}

/// Why a run ended before its command finished.
enum Stop {
    /// stdout's reader went away, so the payload has nowhere to go.
    PipeClosed,
    /// The run could not start or finish.
    Fatal(lib::Error),
    /// Writing the payload to stdout failed; the bin's own stream, so the bin
    /// reports it rather than dressing it as a library error.
    Output(std::io::Error),
}

impl From<lib::Error> for Stop {
    fn from(error: lib::Error) -> Self {
        Self::Fatal(error)
    }
}

fn main() {
    let cli = Cli::parse();
    logger::init(cli.quiet, cli.verbose);

    let exit_code = match run(cli) {
        Ok(code) => code,
        Err(Stop::PipeClosed) => ExitCode::Success,
        Err(Stop::Fatal(error)) => {
            log::error!("{error}");
            ExitCode::Fatal
        }
        Err(Stop::Output(error)) => {
            log::error!("failed to write to stdout: {error}");
            ExitCode::Fatal
        }
    };

    std::process::exit(exit_code.into());
}

fn run(cli: Cli) -> Result<ExitCode, Stop> {
    if cli.edit_config {
        CliConfig::edit_config()?;
        return Ok(ExitCode::Success);
    }

    match &cli.command {
        Some(Commands::Completions { shell }) => completions(*shell),
        None => do_update(&configured(&cli)?, UpdateArgs::default(), cli.quiet),
        Some(Commands::Check { ignore }) => do_check(&configured(&cli)?, ignore, cli.quiet),
        Some(Commands::ListInstalled) => do_list_installed(&configured(&cli)?),
        Some(Commands::Update {
            component,
            restart_plasma,
            no_restart_plasma,
            yes,
            dry_run,
            ignore,
        }) => do_update(
            &configured(&cli)?,
            UpdateArgs {
                component: component.clone(),
                restart_plasma: *restart_plasma,
                no_restart_plasma: *no_restart_plasma,
                yes: *yes,
                dry_run: *dry_run,
                ignore: ignore.clone(),
            },
            cli.quiet,
        ),
    }
}

/// The config every command but `completions` runs with, after the
/// system-wide pre-flight that `completions` must not trigger.
fn configured(cli: &Cli) -> Result<CliConfig, Stop> {
    if cli.system && !is_root_user() {
        validate_sudo()?;
    }

    let mut config = CliConfig::load()?;
    config.inner.system = cli.system;
    config.inner.skip_plasma_detection = cli.skip_plasma_detection;
    Ok(config)
}

/// Writes one payload block to stdout, the only stream the binary's data takes.
///
/// A closed pipe ends the run with success; any other write failure is fatal.
fn payload(args: std::fmt::Arguments<'_>) -> Result<(), Stop> {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{args}")
        .and_then(|()| stdout.flush())
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::BrokenPipe => Stop::PipeClosed,
            _ => Stop::Output(error),
        })
}

fn completions(shell: clap_complete::Shell) -> Result<ExitCode, Stop> {
    let mut script = Vec::new();
    clap_complete::generate(shell, &mut Cli::command(), "plasmoid-updater", &mut script);
    // The generated script ends with its own newline, so the payload helper's
    // is the only one left.
    let script = String::from_utf8_lossy(&script);
    payload(format_args!("{}", script.trim_end_matches('\n')))?;
    Ok(ExitCode::Success)
}

/// The reporter for a run: `-q` leaves stderr to error lines alone, so the
/// progress reporter's draws are dropped with it.
fn reporter(quiet: bool) -> Box<dyn lib::Reporter> {
    if quiet {
        Box::new(())
    } else {
        Box::new(lib::cli::ProgressReporter::new())
    }
}

fn do_check(config: &CliConfig, ignore: &[String], quiet: bool) -> Result<ExitCode, Stop> {
    let mut check_config = config.inner.clone();
    check_config
        .excluded_packages
        .extend(ignore.iter().cloned());

    reporter(quiet).event(&lib::Event::Checking);

    let result = lib::check(&check_config)?;

    report_diagnostics(&result);

    if !result.available_updates.is_empty() {
        payload(format_args!(
            "{} update{} available.",
            result.available_updates.len(),
            if result.available_updates.len() == 1 {
                ""
            } else {
                "s"
            }
        ))?;
        payload(format_args!(
            "{}",
            lib::cli::updates_table(&result.available_updates)
        ))?;
    } else if result.diagnostics.is_empty() {
        payload(format_args!("no updates available"))?;
    }

    if !result.diagnostics.is_empty() {
        Ok(ExitCode::Failure)
    } else if result.available_updates.is_empty() {
        Ok(ExitCode::Success)
    } else {
        Ok(ExitCode::UpdatesAvailable)
    }
}

/// Writes every component the check could not settle to stderr, named with
/// the component it is about.
fn report_diagnostics(result: &lib::CheckResult) {
    for diagnostic in &result.diagnostics {
        report_diagnostic(diagnostic);
    }
}

fn report_diagnostic(diagnostic: &lib::Diagnostic) {
    log::error!(
        "failed to check {:?}: {}",
        diagnostic.name,
        diagnostic.reason
    );
}

fn do_list_installed(config: &CliConfig) -> Result<ExitCode, Stop> {
    let components = lib::get_installed(&config.inner)?;

    if components.is_empty() {
        payload(format_args!("no components installed"))?;
        return Ok(ExitCode::Success);
    }

    payload(format_args!(
        "{} installed component{}.",
        components.len(),
        if components.len() == 1 { "" } else { "s" }
    ))?;
    payload(format_args!("{}", lib::cli::components_table(&components)))?;
    Ok(ExitCode::Success)
}

fn do_update(config: &CliConfig, args: UpdateArgs, quiet: bool) -> Result<ExitCode, Stop> {
    let mut update_config = config.inner.clone();
    update_config.excluded_packages.extend(args.ignore);

    if args.dry_run {
        return dry_run(&update_config, args.component.as_deref(), quiet);
    }

    let auto = args.yes || config.assume_yes || config.update_all_by_default;

    let restart = if args.restart_plasma {
        RestartDecision::Always
    } else if args.no_restart_plasma {
        RestartDecision::Never
    } else if config.prompt_restart {
        RestartDecision::Prompt
    } else {
        RestartDecision::Never
    };

    if let Some(ref name) = args.component {
        return do_update_single(name, update_config, restart, quiet);
    }

    let reporter = reporter(quiet);
    reporter.event(&lib::Event::Checking);

    let check_result = lib::check(&update_config)?;
    report_diagnostics(&check_result);

    // The summary counts the same universe `check` reports: everything it set
    // aside (exclusions, components it could not settle) rides along as
    // skipped, including on the branch that installs nothing.
    let mut skipped = check_result.skipped.clone();
    skipped.extend(check_result.diagnostics.iter().map(|d| d.name.clone()));
    let check_failed = !check_result.diagnostics.is_empty();

    let installable = if auto || !std::io::stdin().is_terminal() {
        check_result.available_updates
    } else if check_result.available_updates.is_empty() {
        Vec::new()
    } else {
        let selected = lib::cli::select(&check_result.available_updates)?;
        if selected.is_empty() {
            payload(format_args!("nothing to update"))?;
            return Ok(if check_failed {
                ExitCode::Failure
            } else {
                ExitCode::Success
            });
        }
        selected
    };

    let mut result = if installable.is_empty() {
        lib::UpdateResult::default()
    } else {
        lib::install(&installable, &update_config, reporter.as_ref())?
    };
    result.skipped.extend(skipped);

    finish_update(result, restart, check_failed)
}

/// Lists what `update` would install, leaving the disk and the registry alone.
fn dry_run(config: &lib::Config, name: Option<&str>, quiet: bool) -> Result<ExitCode, Stop> {
    reporter(quiet).event(&lib::Event::Checking);

    let check_result = lib::check(config)?;

    let (updates, check_failed) = match name {
        // A named run ends on the named component's own outcome; another
        // component's failed check is not this run's.
        Some(name) => match resolve_named(name, config, &check_result)? {
            Named::Updates(updates) => (updates, false),
            Named::Excluded => {
                payload(format_args!("{name:?} is excluded"))?;
                return Ok(ExitCode::Success);
            }
            Named::UpToDate => {
                payload(format_args!("no update available for {name:?}"))?;
                return Ok(ExitCode::Success);
            }
            Named::CheckFailed(diagnostic) => {
                report_diagnostic(&diagnostic);
                return Ok(ExitCode::Failure);
            }
            Named::Unknown => return Ok(unknown_component(name)),
        },
        None => {
            report_diagnostics(&check_result);
            (
                check_result.available_updates.clone(),
                !check_result.diagnostics.is_empty(),
            )
        }
    };

    if updates.is_empty() {
        payload(format_args!("no updates available"))?;
    } else {
        payload(format_args!("{}", lib::cli::updates_table(&updates)))?;
    }

    if check_failed {
        Ok(ExitCode::Failure)
    } else {
        Ok(ExitCode::Success)
    }
}

fn do_update_single(
    name: &str,
    config: lib::Config,
    restart: RestartDecision,
    quiet: bool,
) -> Result<ExitCode, Stop> {
    let reporter = reporter(quiet);
    reporter.event(&lib::Event::Checking);

    let check_result = lib::check(&config)?;

    match resolve_named(name, &config, &check_result)? {
        Named::Updates(updates) => {
            let result = lib::install(&updates, &config, reporter.as_ref())?;
            finish_update(result, restart, false)
        }
        Named::Excluded => {
            payload(format_args!("{name:?} is excluded"))?;
            Ok(ExitCode::Success)
        }
        Named::UpToDate => {
            payload(format_args!("no update available for {name:?}"))?;
            Ok(ExitCode::Success)
        }
        Named::CheckFailed(diagnostic) => {
            report_diagnostic(&diagnostic);
            Ok(ExitCode::Failure)
        }
        Named::Unknown => Ok(unknown_component(name)),
    }
}

/// What a component name on the command line resolves to.
enum Named {
    /// Pending updates for the installed component the name matched.
    Updates(Vec<lib::AvailableUpdate>),
    /// The installed component is excluded by the configuration.
    Excluded,
    /// The installed component has no pending update.
    UpToDate,
    /// The check could not settle the installed component, with the diagnostic
    /// naming why.
    CheckFailed(lib::Diagnostic),
    /// No installed component answers to the name.
    Unknown,
}

/// Resolves a display name or directory name to its pending updates, falling
/// back to the installed set for the components the check left out.
fn resolve_named(
    name: &str,
    config: &lib::Config,
    check_result: &lib::CheckResult,
) -> Result<Named, lib::Error> {
    let matched: Vec<lib::AvailableUpdate> = check_result
        .available_updates
        .iter()
        .filter(|u| u.installed.name == name || u.installed.directory_name == name)
        .cloned()
        .collect();
    if !matched.is_empty() {
        return Ok(Named::Updates(matched));
    }

    let installed = lib::get_installed(config)?;
    let Some(component) = installed
        .iter()
        .find(|c| c.name == name || c.directory_name == name)
    else {
        return Ok(Named::Unknown);
    };

    // A component the check could not settle has no update row to match, and
    // saying "no update available" would claim what the check did not learn.
    if let Some(diagnostic) = check_result
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.name == component.name)
    {
        return Ok(Named::CheckFailed(diagnostic.clone()));
    }

    // The check's skipped list holds an excluded component's display name,
    // whichever rule — exact or pattern — matched it.
    let excluded = check_result.skipped.contains(&component.name)
        || config.excluded_packages.iter().any(|rule| {
            *rule == name || *rule == component.name || *rule == component.directory_name
        });
    if excluded {
        Ok(Named::Excluded)
    } else {
        Ok(Named::UpToDate)
    }
}

/// A name no installed component answers to is the caller's usage error.
fn unknown_component(name: &str) -> ExitCode {
    log::error!("no installed component named {name:?}");
    ExitCode::Usage
}

/// Writes the summary and, on stderr, the failure table; applies the restart
/// decision and maps the outcome to the exit code. A failed check is a failure
/// of the run even when every component it could settle was up to date.
fn finish_update(
    result: lib::UpdateResult,
    restart: RestartDecision,
    check_failed: bool,
) -> Result<ExitCode, Stop> {
    if result.succeeded.is_empty() && result.failed.is_empty() && result.skipped.is_empty() {
        payload(format_args!("no updates available"))?;
        return Ok(ExitCode::Success);
    }

    // The summary is the last payload, so a closed stdout costs that line,
    // never the run's own outcome.
    match payload(format_args!(
        "Update Summary: {} succeeded, {} failed, {} skipped ({} total)",
        result.succeeded.len(),
        result.failed.len(),
        result.skipped.len(),
        result.succeeded.len() + result.failed.len() + result.skipped.len(),
    )) {
        Err(Stop::PipeClosed) => {}
        other => other?,
    }

    if !result.failed.is_empty() {
        lib::cli::print_failures(&result);
    }

    maybe_restart(result.needs_restart(), restart);

    if result.failed.is_empty() && !check_failed {
        Ok(ExitCode::Success)
    } else {
        Ok(ExitCode::Failure)
    }
}

fn maybe_restart(needs: bool, decision: RestartDecision) {
    if !needs {
        return;
    }
    let confirmed = match decision {
        RestartDecision::Always => true,
        RestartDecision::Prompt => std::io::stdin().is_terminal() && prompt_restart(),
        RestartDecision::Never => false,
    };
    if confirmed && let Err(error) = lib::restart_plasmashell() {
        log_restart_failure(&error);
    }
}

/// A restart the caller asked for is part of the command, not a hint: its
/// failure has to survive `-q`, which keeps only error lines.
fn log_restart_failure(error: &lib::Error) {
    log::error!("failed to restart plasmashell: {error}");
}

fn prompt_restart() -> bool {
    let mut err = std::io::stderr();
    let _ = write!(err, "Restart plasmashell now? [y/N] ");
    let _ = err.flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn is_root_user() -> bool {
    nix::unistd::Uid::effective().is_root()
}

fn validate_sudo() -> Result<(), lib::Error> {
    let status = std::process::Command::new("sudo")
        .args(["-v"])
        .status()
        .map_err(|e| spawn_error("sudo", &e))?;

    if !status.success() {
        return Err(lib::Error::Config(format!(
            "sudo authentication failed: {status}"
        )));
    }

    Ok(())
}

/// Maps a failed spawn of `command` to the error that names the real cause: a
/// missing binary is [`lib::Error::MissingTool`], anything else a
/// [`lib::Error::Config`] message naming the command. `Error::Io` carries a
/// touched path, which a command name is not.
fn spawn_error(command: &'static str, e: &std::io::Error) -> lib::Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        lib::Error::MissingTool { tool: command }
    } else {
        lib::Error::Config(format!("failed to run {command}: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    // Assembled from parts so the scan does not match its own source.
    const STDIO_MACROS: [&str; 4] = [
        concat!("print", "ln!"),
        concat!("print", "!"),
        concat!("eprint", "ln!"),
        concat!("eprint", "!"),
    ];
    const WRITE_MACROS: [&str; 2] = [concat!("write", "!"), concat!("write", "ln!")];
    /// A fresh handle taken from the stdout stream.
    const STDOUT_OPEN: &str = concat!("std", "out(");
    const STDOUT_CALL: &str = concat!("std", "out()");

    fn bare_stdio_macro(source: &str) -> Option<&'static str> {
        STDIO_MACROS
            .into_iter()
            .find(|needle| source.contains(*needle))
    }

    /// The write macro whose target expression takes a fresh stdout handle.
    fn stdout_write_macro(source: &str) -> Option<&'static str> {
        WRITE_MACROS.into_iter().find(|macro_name| {
            source.match_indices(macro_name).any(|(at, _)| {
                let args = &source[at + macro_name.len()..];
                let receiver = &args[..args.find([',', ')']).unwrap_or(args.len())];
                receiver.contains(STDOUT_OPEN)
            })
        })
    }

    /// Counts the fresh stdout handles taken in lines of code. A comment line
    /// is skipped, so prose that quotes a `stdout()` call counts as none.
    fn stdout_handles(source: &str) -> usize {
        source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .map(|line| line.matches(STDOUT_CALL).count())
            .sum()
    }

    fn source_files(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                found.extend(source_files(&path));
            } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
                found.push(path);
            }
        }
        found
    }

    static RESTART_LOG: std::sync::Mutex<Vec<(log::Level, String)>> =
        std::sync::Mutex::new(Vec::new());

    struct CaptureLogger;

    impl log::Log for CaptureLogger {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }

        fn log(&self, record: &log::Record<'_>) {
            RESTART_LOG
                .lock()
                .expect("the restart log mutex")
                .push((record.level(), record.args().to_string()));
        }

        fn flush(&self) {}
    }

    static CAPTURE: CaptureLogger = CaptureLogger;

    #[test]
    fn a_failed_restart_is_an_error_line() {
        // `main` installs the stderr logger; this test binary installs its own
        // so the level of the line is observable.
        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| {
            log::set_logger(&CAPTURE).expect("no other logger in this test binary");
            log::set_max_level(log::LevelFilter::Debug);
        });

        super::log_restart_failure(&libplasmoid_updater::Error::Config(
            "systemctl exited with status 1".to_string(),
        ));

        let logged = RESTART_LOG.lock().expect("the restart log mutex");
        assert_eq!(
            logged.last(),
            Some(&(
                log::Level::Error,
                "failed to restart plasmashell: configuration error: systemctl exited with status 1"
                    .to_string()
            )),
            "{logged:?}"
        );
    }

    #[test]
    fn a_comment_naming_a_stdout_handle_is_not_counted() {
        let commented = concat!(
            "/// The payload helper takes a ",
            "std",
            "out()",
            " of its own.\nfn f() {}\n",
        );
        assert_eq!(stdout_handles(commented), 0);

        let code = concat!("fn payload() { let mut ", "std", "out()", "; }\n");
        assert_eq!(stdout_handles(code), 1);
    }

    #[test]
    fn the_scan_flags_a_bare_stdio_macro() {
        assert_eq!(
            bare_stdio_macro(concat!("let x = 1;\n", "print", "ln!(\"hi\");\n")),
            Some(concat!("print", "ln!")),
        );
    }

    #[test]
    fn the_scan_flags_a_write_to_the_stdout_stream() {
        let payload_bypass = concat!(
            "fn f() {\n    let _ = write",
            "ln!(",
            "std",
            "::io::",
            "stdout",
            "(), \"hi\");\n}\n",
        );
        assert_eq!(
            stdout_write_macro(payload_bypass),
            Some(concat!("write", "ln!"))
        );
        assert_eq!(stdout_handles(payload_bypass), 1);

        // The negative control: a write aimed at stderr is not a payload bypass.
        let diagnostics = concat!("fn f() { let _ = write", "ln!(err, \"hi\"); }\n");
        assert_eq!(stdout_write_macro(diagnostics), None);
        assert_eq!(stdout_handles(diagnostics), 0);
    }

    /// Pins that every payload write goes through the `payload` helper.
    ///
    /// Residual it cannot see: an aliased import (`use std::io::stdout as out;`
    /// then `out()`) or a spaced `stdout ()` spelling takes a handle this count
    /// never matches.
    #[test]
    fn payload_only_leaves_through_the_stdout_helper() {
        let files = source_files(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"));
        assert!(
            files.iter().any(|file| file.ends_with("logger.rs")),
            "the scan skipped a source file: {files:?}"
        );
        let mut handles = 0;
        for file in &files {
            let source = std::fs::read_to_string(file).unwrap();
            assert_eq!(
                bare_stdio_macro(&source),
                None,
                "{}: a bare stdio macro bypasses the payload and log helpers",
                file.display()
            );
            assert_eq!(
                stdout_write_macro(&source),
                None,
                "{}: a write macro targets stdout directly",
                file.display()
            );
            handles += stdout_handles(&source);
        }
        assert_eq!(
            handles, 1,
            "only the payload helper may take a stdout handle"
        );
    }
}
