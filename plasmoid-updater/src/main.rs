// SPDX-FileCopyrightText: 2025 uwuclxdy
// SPDX-License-Identifier: GPL-3.0-or-later

mod cli_config;
mod exit_code;
mod logger;

use std::io::{IsTerminal, Write as _};
use std::path::PathBuf;

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
#[command(after_help = concat!(
    "project: ",
    env!("CARGO_PKG_REPOSITORY"),
    "\nreport bugs: ",
    env!("CARGO_PKG_REPOSITORY"),
    "/issues",
))]
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
        #[arg(long, help = "print the result as JSON on stdout")]
        json: bool,
    },
    #[command(about = "list all installed components")]
    ListInstalled {
        #[arg(long, help = "print the result as JSON on stdout")]
        json: bool,
    },
    #[command(about = "update components")]
    Update {
        #[arg(help = "component name or directory to update")]
        component: Option<String>,
        #[arg(
            long,
            help = "automatically restart plasmashell",
            conflicts_with = "no_restart_plasma"
        )]
        restart_plasma: bool,
        #[arg(long, help = "do not restart plasmashell")]
        no_restart_plasma: bool,
        #[arg(
            short = 'y',
            long,
            help = "automatically confirm all updates (TOPGRADE_YES=1 does the same)"
        )]
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
        #[arg(long, help = "print the result as JSON on stdout")]
        json: bool,
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

/// What a run that may need a restart does about it.
#[derive(Debug, PartialEq, Eq)]
enum RestartPlan {
    /// Restart without asking.
    Restart,
    /// Ask first.
    Prompt,
    /// Leave plasmashell alone.
    Skip,
    /// Leave it alone, and say why on stderr.
    SkipWarn(&'static str),
}

const RESTART_PROMPT_SKIPPED: &str = "restart prompt skipped: stdin is not a terminal; \
     pass --restart-plasma to restart plasmashell anyway";

/// Whether and how this run restarts plasmashell.
fn restart_plan(decision: RestartDecision, needs_restart: bool, stdin_is_tty: bool) -> RestartPlan {
    if !needs_restart {
        return RestartPlan::Skip;
    }
    match decision {
        RestartDecision::Always => RestartPlan::Restart,
        RestartDecision::Never => RestartPlan::Skip,
        RestartDecision::Prompt if stdin_is_tty => RestartPlan::Prompt,
        RestartDecision::Prompt => RestartPlan::SkipWarn(RESTART_PROMPT_SKIPPED),
    }
}

#[derive(Default)]
struct UpdateArgs {
    component: Option<String>,
    restart_plasma: bool,
    no_restart_plasma: bool,
    yes: bool,
    dry_run: bool,
    ignore: Vec<String>,
    json: bool,
}

/// Why a run ended before its command finished.
enum Stop {
    /// stdout's reader went away, so the payload has nowhere to go.
    PipeClosed,
    /// The command line names something the run cannot act on.
    Usage(String),
    /// The run could not start or finish.
    Fatal(lib::Error),
    /// A required external tool is not installed or not on `PATH`; the bin's
    /// own spawn failure, reported rather than dressed as a library error.
    MissingTool(&'static str),
    /// A filesystem operation on the config file failed; the bin's own error,
    /// rendered the way the library's `Error::Io` reads.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Writing the payload to stdout failed; the bin's own stream, so the bin
    /// reports it rather than dressing it as a library error.
    Output(std::io::Error),
    /// Serializing a result for `--json` failed.
    Serialize(serde_json::Error),
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
        Err(Stop::Usage(message)) => {
            log::error!("{message}");
            ExitCode::Usage
        }
        Err(Stop::Fatal(error)) => {
            log::error!("{error}");
            ExitCode::Fatal
        }
        Err(Stop::MissingTool(tool)) => {
            log::error!("missing required tool: {tool}");
            ExitCode::Fatal
        }
        Err(Stop::Io { path, source }) => {
            log::error!("{}: {source}", path.display());
            ExitCode::Fatal
        }
        Err(Stop::Output(error)) => {
            log::error!("failed to write to stdout: {error}");
            ExitCode::Fatal
        }
        Err(Stop::Serialize(error)) => {
            log::error!("failed to serialize the result: {error}");
            ExitCode::Fatal
        }
    };

    std::process::exit(exit_code.into());
}

fn run(cli: Cli) -> Result<ExitCode, Stop> {
    if cli.edit_config {
        reject_unusable_config_dir()?;
        CliConfig::edit_config()?;
        return Ok(ExitCode::Success);
    }

    match &cli.command {
        Some(Commands::Completions { shell }) => completions(*shell),
        // A bare run is a request for the help, not for an update: it reads
        // no config and makes no store request.
        None => print_help(),
        Some(Commands::Check { ignore, json }) => {
            do_check(&configured(&cli)?, ignore, cli.quiet, *json)
        }
        Some(Commands::ListInstalled { json }) => do_list_installed(&configured(&cli)?, *json),
        Some(Commands::Update {
            component,
            restart_plasma,
            no_restart_plasma,
            yes,
            dry_run,
            ignore,
            json,
        }) => do_update(
            &configured(&cli)?,
            UpdateArgs {
                component: component.clone(),
                restart_plasma: *restart_plasma,
                no_restart_plasma: *no_restart_plasma,
                yes: *yes,
                dry_run: *dry_run,
                ignore: ignore.clone(),
                json: *json,
            },
            cli.quiet,
        ),
    }
}

fn print_help() -> Result<ExitCode, Stop> {
    let help = Cli::command().render_help();
    let text = if lib::cli::styled(lib::cli::Stream::Stdout) {
        help.ansi().to_string()
    } else {
        help.to_string()
    };
    payload(format_args!("{}", text.trim_end_matches('\n')))?;
    Ok(ExitCode::Success)
}

/// The config every command but `completions` runs with.
fn configured(cli: &Cli) -> Result<CliConfig, Stop> {
    reject_unusable_config_dir()?;

    let mut config = CliConfig::load()?;
    config.inner.system = cli.system;
    config.inner.skip_plasma_detection = cli.skip_plasma_detection;
    Ok(config)
}

/// A config directory that exists but is not a directory is the caller's usage
/// error: the run has nowhere to keep its settings and should say so plainly.
fn reject_unusable_config_dir() -> Result<(), Stop> {
    match cli_config::unusable_config_dir() {
        Some(dir) => Err(Stop::Usage(format!(
            "config directory {dir:?} is not a directory"
        ))),
        None => Ok(()),
    }
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

/// Writes one compact JSON document plus the payload helper's newline to
/// stdout.
fn payload_json<T: serde::Serialize>(value: &T) -> Result<(), Stop> {
    let json = serde_json::to_string(value).map_err(Stop::Serialize)?;
    payload(format_args!("{json}"))
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
struct Reporter(Option<lib::cli::ProgressReporter>);

impl Reporter {
    fn new(quiet: bool) -> Self {
        Self((!quiet).then(lib::cli::ProgressReporter::new))
    }

    fn as_dyn(&self) -> &dyn lib::Reporter {
        match &self.0 {
            Some(reporter) => reporter,
            None => &(),
        }
    }
}

/// `TOPGRADE_YES=1` confirms the way `-y` does, so topgrade can drive an
/// unattended update. Exactly `1` counts: any other value confirms nothing.
fn topgrade_yes() -> bool {
    std::env::var_os("TOPGRADE_YES").is_some_and(|value| value == std::ffi::OsStr::new("1"))
}

fn do_check(
    config: &CliConfig,
    ignore: &[String],
    quiet: bool,
    json: bool,
) -> Result<ExitCode, Stop> {
    let mut check_config = config.inner.clone();
    check_config
        .excluded_packages
        .extend(ignore.iter().cloned());

    let reporter = Reporter::new(quiet);
    let result = lib::check(&check_config, reporter.as_dyn())?;

    report_diagnostics(&result);

    if json {
        payload_json(&result)?;
    } else if !result.available_updates.is_empty() {
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
            lib::cli::updates_table(&result.available_updates, lib::cli::Stream::Stdout)
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
        report_diagnostic(&format!("{:?}", diagnostic.name), diagnostic);
    }
}

/// Reports every diagnostic but `except`: a component reported as a failure
/// row must not also print a diagnostic line.
fn report_diagnostics_except(result: &lib::CheckResult, except: &[&lib::Diagnostic]) {
    for diagnostic in &result.diagnostics {
        if except.contains(&diagnostic) {
            continue;
        }
        report_diagnostic(&format!("{:?}", diagnostic.name), diagnostic);
    }
}

/// Reports a diagnostic on stderr under the label the run names the component
/// by.
fn report_diagnostic(label: &str, diagnostic: &lib::Diagnostic) {
    log::error!("failed to check {label}: {}", diagnostic.reason);
}

/// The `--json` body of `list-installed`: an object, so a later field can join
/// the components without redefining the document's top level.
#[derive(serde::Serialize)]
struct InstalledComponents {
    components: Vec<lib::InstalledComponent>,
}

fn do_list_installed(config: &CliConfig, json: bool) -> Result<ExitCode, Stop> {
    let components = lib::get_installed(&config.inner)?;

    if json {
        payload_json(&InstalledComponents { components })?;
        return Ok(ExitCode::Success);
    }

    if components.is_empty() {
        payload(format_args!("no components installed"))?;
        return Ok(ExitCode::Success);
    }

    payload(format_args!(
        "{} installed component{}.",
        components.len(),
        if components.len() == 1 { "" } else { "s" }
    ))?;
    payload(format_args!(
        "{}",
        lib::cli::components_table(&components, lib::cli::Stream::Stdout)
    ))?;
    Ok(ExitCode::Success)
}

fn do_update(config: &CliConfig, args: UpdateArgs, quiet: bool) -> Result<ExitCode, Stop> {
    let mut update_config = config.inner.clone();
    update_config.excluded_packages.extend(args.ignore);

    if args.dry_run {
        return dry_run(&update_config, args.component.as_deref(), quiet, args.json);
    }

    // A named component is the caller's explicit choice, the way `-y` is for
    // the whole selection, so only an unnamed update needs the confirmation.
    let auto = args.yes || topgrade_yes() || config.assume_yes || config.update_all_by_default;
    if !auto && args.component.is_none() && !std::io::stdin().is_terminal() {
        return Err(Stop::Usage(
            "refusing to update on a non-interactive stdin; pass -y/--yes to confirm".into(),
        ));
    }

    let restart = if args.restart_plasma {
        RestartDecision::Always
    } else if args.no_restart_plasma {
        RestartDecision::Never
    } else if config.prompt_restart {
        RestartDecision::Prompt
    } else {
        RestartDecision::Never
    };

    // A killed run's leftovers are put back under the run lock before anything
    // reads the component directories: the lock-free check would list a
    // stranded tree as an interrupted install, and `install` would only repair
    // it after that check already reported the component. The named path needs
    // the repair first too, since it resolves its name against what is on disk.
    install_authenticated(&[], &update_config, &())?;

    if let Some(ref name) = args.component {
        return do_update_single(name, update_config, restart, quiet, args.json);
    }

    let reporter = Reporter::new(quiet);
    let check_result = lib::check(&update_config, reporter.as_dyn())?;
    let unavailable_diagnostics: Vec<&lib::Diagnostic> =
        check_result.unavailable_diagnostics().collect();
    report_diagnostics_except(&check_result, &unavailable_diagnostics);

    // The summary counts the same universe `check` reports: everything it set
    // aside (exclusions, components it could not settle) rides along as
    // skipped. A component with no matching download is a failure row instead,
    // so its diagnostic must not also count it skipped.
    let mut skipped = check_result.skipped.clone();
    let mut check_failed = false;
    for diagnostic in &check_result.diagnostics {
        if unavailable_diagnostics.contains(&diagnostic) {
            continue;
        }
        skipped.push(diagnostic.name.clone());
        check_failed = true;
    }

    let (installable, interactive_empty) = if auto || check_result.available_updates.is_empty() {
        (check_result.available_updates, false)
    } else {
        let selected = lib::cli::select(&check_result.available_updates)?;
        let empty = selected.is_empty();
        (selected, empty)
    };
    if interactive_empty && !args.json {
        payload(format_args!("nothing to update"))?;
    }

    // `install` runs even for an empty selection: it takes the lock and
    // repairs what a killed run left stranded, which is what a check's
    // "run update to restore" tells the user to do. Authenticating only now,
    // after the check's line and the selection prompt have settled, keeps the
    // password prompt off both, and off a run with nothing to install or
    // repair.
    let mut result = install_authenticated(&installable, &update_config, reporter.as_dyn())?;
    result.skipped.extend(skipped);
    result.failed.extend(check_result.unavailable);

    // The user declined every offered update and nothing else happened:
    // `nothing to update` already said so, and the summary's "no updates
    // available" would claim the store offered none.
    if interactive_empty
        && result.succeeded.is_empty()
        && result.failed.is_empty()
        && result.skipped.is_empty()
    {
        if args.json {
            payload_json(&result)?;
        }
        return Ok(ExitCode::Success);
    }

    finish_update(result, restart, check_failed, args.json)
}

/// The check shape a named dry run reports: only the outcomes of the named
/// components, so another component's failures do not leak into it.
fn named_check_result(
    available_updates: Vec<lib::AvailableUpdate>,
    skipped: Vec<String>,
    unmatched: Vec<lib::InstalledComponent>,
    diagnostics: Vec<lib::Diagnostic>,
    unavailable: Vec<lib::Failure>,
) -> lib::CheckResult {
    let mut result = lib::CheckResult::default();
    result.available_updates = available_updates;
    result.skipped = skipped;
    result.unmatched = unmatched;
    result.diagnostics = diagnostics;
    result.unavailable = unavailable;
    result
}

/// Lists what `update` would install, leaving the disk and the registry alone.
fn dry_run(
    config: &lib::Config,
    name: Option<&str>,
    quiet: bool,
    json: bool,
) -> Result<ExitCode, Stop> {
    let reporter = Reporter::new(quiet);
    let check_result = lib::check(config, reporter.as_dyn())?;

    match name {
        // A named run ends on the outcomes of the components the name covers;
        // another component's failed check is not this run's.
        Some(name) => named_dry_run(name, config, &check_result, json),
        None => {
            let unavailable_diagnostics: Vec<&lib::Diagnostic> =
                check_result.unavailable_diagnostics().collect();
            report_diagnostics_except(&check_result, &unavailable_diagnostics);
            report_failure_rows(check_result.unavailable.clone());

            // A failure row the check settled fails the run even though its
            // diagnostic line is not printed.
            let check_failed = !check_result.diagnostics.is_empty();
            if json {
                payload_json(&check_result)?;
            } else if check_result.available_updates.is_empty() {
                payload(format_args!("no updates available"))?;
            } else {
                payload(format_args!(
                    "{}",
                    lib::cli::updates_table(
                        &check_result.available_updates,
                        lib::cli::Stream::Stdout
                    )
                ))?;
            }

            if check_failed {
                Ok(ExitCode::Failure)
            } else {
                Ok(ExitCode::Success)
            }
        }
    }
}

/// The named dry run: every component the name covers answers by its own
/// outcome, and the run exits with the worst of them.
fn named_dry_run(
    name: &str,
    config: &lib::Config,
    check_result: &lib::CheckResult,
    json: bool,
) -> Result<ExitCode, Stop> {
    let resolved = resolve_named(name, config, check_result)?;
    if resolved.is_empty() {
        return Ok(unknown_component(name));
    }
    let multi = resolved.len() > 1;

    let mut updates = Vec::new();
    let mut skipped = Vec::new();
    let mut unmatched = Vec::new();
    let mut diagnostics = Vec::new();
    let mut unavailable = Vec::new();
    let mut failure = false;

    for named in &resolved {
        match &named.outcome {
            ComponentOutcome::Update(update) => updates.push(update.clone()),
            ComponentOutcome::Excluded => {
                skipped.push(named.component.name.clone());
                if !json {
                    payload_settled(Settled::Excluded, name, &named.component, multi)?;
                }
            }
            ComponentOutcome::Unmatched => {
                skipped.push(named.component.name.clone());
                unmatched.push(named.component.clone());
                if !json {
                    payload_settled(Settled::Unmatched, name, &named.component, multi)?;
                }
            }
            ComponentOutcome::UpToDate => {
                if !json {
                    payload_settled(Settled::UpToDate, name, &named.component, multi)?;
                }
            }
            ComponentOutcome::CheckFailed(diagnostic) => {
                diagnostics.push(diagnostic.clone());
                report_diagnostic(
                    &failed_check_label(name, &named.component, diagnostic, multi),
                    diagnostic,
                );
                failure = true;
            }
            ComponentOutcome::Unavailable(component_failure) => {
                // The failure and the diagnostic the check settled for it
                // travel together, the way `check` reports them.
                if let Some(diagnostic) =
                    check_result.unavailable_diagnostics().find(|diagnostic| {
                        diagnostic.path == component_failure.component.path
                            && diagnostic.name == component_failure.component.name
                    })
                {
                    diagnostics.push(diagnostic.clone());
                }
                unavailable.push(component_failure.clone());
                failure = true;
            }
        }
    }

    if !unavailable.is_empty() {
        report_failure_rows(unavailable.clone());
    }

    if json {
        payload_json(&named_check_result(
            updates,
            skipped,
            unmatched,
            diagnostics,
            unavailable,
        ))?;
    } else if !updates.is_empty() {
        payload(format_args!(
            "{}",
            lib::cli::updates_table(&updates, lib::cli::Stream::Stdout)
        ))?;
    }

    Ok(if failure {
        ExitCode::Failure
    } else {
        ExitCode::Success
    })
}

fn do_update_single(
    name: &str,
    config: lib::Config,
    restart: RestartDecision,
    quiet: bool,
    json: bool,
) -> Result<ExitCode, Stop> {
    let reporter = Reporter::new(quiet);
    let check_result = lib::check(&config, reporter.as_dyn())?;

    let resolved = resolve_named(name, &config, &check_result)?;
    if resolved.is_empty() {
        return Ok(unknown_component(name));
    }
    let multi = resolved.len() > 1;

    let mut updates = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = Vec::new();
    let mut check_failed = false;

    for named in &resolved {
        match &named.outcome {
            ComponentOutcome::Update(update) => updates.push(update.clone()),
            ComponentOutcome::Excluded => {
                skipped.push(named.component.name.clone());
                if !json {
                    payload_settled(Settled::Excluded, name, &named.component, multi)?;
                }
            }
            ComponentOutcome::Unmatched => {
                skipped.push(named.component.name.clone());
                if !json {
                    payload_settled(Settled::Unmatched, name, &named.component, multi)?;
                }
            }
            ComponentOutcome::UpToDate => {
                if !json {
                    payload_settled(Settled::UpToDate, name, &named.component, multi)?;
                }
            }
            ComponentOutcome::CheckFailed(diagnostic) => {
                skipped.push(diagnostic.name.clone());
                report_diagnostic(
                    &failed_check_label(name, &named.component, diagnostic, multi),
                    diagnostic,
                );
                check_failed = true;
            }
            ComponentOutcome::Unavailable(failure) => {
                failed.push(failure.clone());
                check_failed = true;
            }
        }
    }

    // Every component's update row installs together, the way an unnamed run
    // installs its selection.
    if !updates.is_empty() {
        let mut result = install_authenticated(&updates, &config, reporter.as_dyn())?;
        result.skipped.extend(skipped);
        result.failed.extend(failed);
        return finish_update(result, restart, check_failed, json);
    }

    // Nothing to install: every component the name covered settled or failed,
    // each reported the way the single-component path reports it.
    if json {
        let mut result = lib::UpdateResult::default();
        result.skipped = skipped;
        result.failed = failed;
        if result.failed.is_empty() {
            payload_json(&result)?;
        } else {
            return finish_update(result, restart, false, json);
        }
    } else if !failed.is_empty() {
        let mut result = lib::UpdateResult::default();
        result.skipped = skipped;
        result.failed = failed;
        return finish_update(result, restart, false, false);
    }
    Ok(if check_failed {
        ExitCode::Failure
    } else {
        ExitCode::Success
    })
}

/// One component a name covers, with the outcome its own identity decides.
#[derive(Debug)]
struct NamedComponent {
    /// The installed component.
    component: lib::InstalledComponent,
    /// Its own outcome.
    outcome: ComponentOutcome,
}

/// One covered component's own outcome on a named run.
#[derive(Debug)]
enum ComponentOutcome {
    /// The component has a pending update row.
    Update(lib::AvailableUpdate),
    /// An exclusion rule in the configuration matches the component.
    Excluded,
    /// No store entry of its type matches it, so a `--system` check lists it
    /// in `unmatched`.
    Unmatched,
    /// The component has no pending update.
    UpToDate,
    /// The component's store entry offers no download link for its version,
    /// so the run reports the failure row the check settled.
    Unavailable(lib::Failure),
    /// The check could not settle the component, with the diagnostic naming
    /// why.
    CheckFailed(lib::Diagnostic),
}

/// Resolves a display name or directory name to every installed component it
/// covers, each with the outcome its own identity decides. The empty list is
/// a name no installed component answers to.
fn resolve_named(
    name: &str,
    config: &lib::Config,
    check_result: &lib::CheckResult,
) -> Result<Vec<NamedComponent>, lib::Error> {
    let installed = lib::get_installed(config)?;
    Ok(resolve_components(name, &installed, config, check_result))
}

/// Decides each installed component the name covers by its own identity: the
/// update row, the no-download failure row and the classified outcome each
/// match by install path and type, never by name, so a same-named sibling's
/// row does not answer for a component. The list comes back in install-path
/// order, so the lines it drives print stably.
fn resolve_components(
    name: &str,
    installed: &[lib::InstalledComponent],
    config: &lib::Config,
    check_result: &lib::CheckResult,
) -> Vec<NamedComponent> {
    let mut resolved: Vec<NamedComponent> = installed
        .iter()
        .filter(|component| component.name == name || component.directory_name == name)
        .map(|component| {
            let outcome = check_result
                .available_updates
                .iter()
                .find(|update| own_row(component, &update.installed))
                .map(|update| ComponentOutcome::Update(update.clone()))
                .or_else(|| {
                    check_result
                        .unavailable
                        .iter()
                        .find(|failure| own_row(component, &failure.component))
                        .map(|failure| ComponentOutcome::Unavailable(failure.clone()))
                })
                .unwrap_or_else(|| classify_installed(component, config, check_result));
            NamedComponent {
                component: component.clone(),
                outcome,
            }
        })
        .collect();
    resolved.sort_by(|left, right| left.component.path.cmp(&right.component.path));
    resolved
}

/// Whether a check row is the component's own: the install path plus the
/// component type decide, never a name.
fn own_row(component: &lib::InstalledComponent, row: &lib::InstalledComponent) -> bool {
    row.path == component.path && row.component_type == component.component_type
}

/// Classifies an installed component the check left without an update row or
/// a no-download failure.
fn classify_installed(
    component: &lib::InstalledComponent,
    config: &lib::Config,
    check_result: &lib::CheckResult,
) -> ComponentOutcome {
    // A component the check could not settle has no update row to match, and
    // saying "no update available" would claim what the check did not learn.
    // The name joins the path: two component types can share one directory
    // under `--system` with different names.
    if let Some(diagnostic) = check_result
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.path == component.path && diagnostic.name == component.name)
    {
        return ComponentOutcome::CheckFailed(diagnostic.clone());
    }

    // Display names are shared between components, so identity is the
    // install path plus the type: an icon theme and a cursor theme can share
    // one directory under `--system`.
    if config.excludes(component) {
        ComponentOutcome::Excluded
    } else if check_result
        .unmatched
        .iter()
        .any(|unmatched| own_row(component, unmatched))
    {
        ComponentOutcome::Unmatched
    } else {
        ComponentOutcome::UpToDate
    }
}

/// An outcome the check settled without a failure.
#[derive(Clone, Copy)]
enum Settled {
    Excluded,
    Unmatched,
    UpToDate,
}

/// The name a named run's settled lines call a component by, and on a run
/// covering several components its failed-check lines too: the argument as
/// typed when it covered exactly one component, else the directory name and
/// the kind, since the shared name alone cannot tell them apart.
fn settled_label(name: &str, component: &lib::InstalledComponent, multi: bool) -> String {
    if multi {
        format!(
            "{:?} ({})",
            component.directory_name, component.component_type
        )
    } else {
        format!("{name:?}")
    }
}

/// The name a named run's failed-check line calls a component by. A failed
/// check is not a settled outcome, so one match keeps the display name the
/// diagnostic carries.
fn failed_check_label(
    name: &str,
    component: &lib::InstalledComponent,
    diagnostic: &lib::Diagnostic,
    multi: bool,
) -> String {
    if multi {
        settled_label(name, component, multi)
    } else {
        format!("{:?}", diagnostic.name)
    }
}

/// The stdout line a named run reports for a settled outcome: the dry run and
/// the update path share it, so the two cannot drift.
fn settled_outcome(settled: Settled, label: &str) -> String {
    match settled {
        Settled::Excluded => format!("{label} is excluded"),
        Settled::Unmatched => format!("no KDE Store entry matches {label}"),
        Settled::UpToDate => format!("no update available for {label}"),
    }
}

/// Prints the stdout line of one settled outcome, named by the shared label
/// rule.
fn payload_settled(
    settled: Settled,
    name: &str,
    component: &lib::InstalledComponent,
    multi: bool,
) -> Result<(), Stop> {
    let line = settled_outcome(settled, &settled_label(name, component, multi));
    payload(format_args!("{line}"))
}

/// Reports failure rows the check already settled, before any install runs.
fn report_failure_rows(failed: Vec<lib::Failure>) {
    if failed.is_empty() {
        return;
    }
    let mut result = lib::UpdateResult::default();
    result.failed = failed;
    lib::cli::print_failures(&result);
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
    json: bool,
) -> Result<ExitCode, Stop> {
    if result.succeeded.is_empty() && result.failed.is_empty() && result.skipped.is_empty() {
        if json {
            payload_json(&result)?;
        } else {
            payload(format_args!("no updates available"))?;
        }
        return Ok(ExitCode::Success);
    }

    // The result is the last payload, so a closed stdout costs that document,
    // never the run's own outcome.
    let written = if json {
        payload_json(&result)
    } else {
        payload(format_args!(
            "{}",
            lib::cli::update_summary(&result, lib::cli::Stream::Stdout)
        ))
    };
    match written {
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
    apply_restart(restart_plan(
        decision,
        needs,
        std::io::stdin().is_terminal(),
    ));
}

fn apply_restart(plan: RestartPlan) {
    let confirmed = match plan {
        RestartPlan::Restart => true,
        RestartPlan::Prompt => prompt_restart(),
        RestartPlan::Skip => false,
        RestartPlan::SkipWarn(message) => {
            log::warn!("{message}");
            false
        }
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

/// Authenticates a system-wide run with privileged work to do — `updates` to
/// install, or a killed run's leftovers for the repair to settle — then runs
/// the install: the library's own sudo calls find the cached credential
/// instead of stopping on a password prompt. `check`, `list-installed` and
/// `update --dry-run` never get here.
///
/// `needs_repair` probes the run lock, so a live run's swap does not count as
/// work, but another run can take or drop the lock between that probe and
/// `install`'s own: one that takes it makes `install` decline the work after
/// the `sudo -v` bought nothing, and one that exits after the probe leaves a
/// repair behind with no cached credential, so the library's own `sudo`
/// prompts on a TTY or fails.
fn install_authenticated(
    updates: &[lib::AvailableUpdate],
    config: &lib::Config,
    reporter: &dyn lib::Reporter,
) -> Result<lib::UpdateResult, Stop> {
    let needed = password_needed(config.system, is_root_user(), !updates.is_empty(), || {
        lib::needs_repair(config)
    })?;
    if needed {
        validate_sudo()?;
    }
    lib::install(updates, config, reporter).map_err(Stop::from)
}

/// Whether the `install` that follows needs a password: a system-wide run,
/// not root, with updates to install or a killed run's leftovers for the
/// repair to settle. The repair scan runs only when its answer can change the
/// outcome: a run with updates asks without it, and a user-directory run or
/// root never asks at all.
fn password_needed(
    system: bool,
    root: bool,
    has_updates: bool,
    repair_pending: impl FnOnce() -> Result<bool, lib::Error>,
) -> Result<bool, lib::Error> {
    if !system || root {
        return Ok(false);
    }
    if has_updates {
        return Ok(true);
    }
    repair_pending()
}

fn validate_sudo() -> Result<(), Stop> {
    let status = std::process::Command::new("sudo")
        .args(["-v"])
        .status()
        .map_err(|e| spawn_error("sudo", &e))?;

    if !status.success() {
        return Err(sudo_auth_failure(&status).into());
    }

    Ok(())
}

/// A failed `sudo -v` names the exit status the way every other command does.
fn sudo_auth_failure(status: &std::process::ExitStatus) -> lib::Error {
    lib::Error::Config(format!(
        "sudo authentication failed: {}",
        command_failure("sudo", status)
    ))
}

/// Renders a failed command's exit status the way an error line names it. The
/// bin's own copy of the library's `command_failure`, which is not public.
pub(crate) fn command_failure(command: &str, status: &std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt as _;

    match (status.code(), status.signal()) {
        (Some(code), _) => format!("{command} exited with status {code}"),
        (None, Some(signal)) => match nix::sys::signal::Signal::try_from(signal) {
            Ok(name) => format!("{command} was killed by signal {signal} ({name:?})"),
            Err(_) => format!("{command} was killed by signal {signal}"),
        },
        (None, None) => format!("{command} exited without a status code"),
    }
}

/// Maps a failed spawn of `command` to the error that names the real cause: a
/// missing binary is [`Stop::MissingTool`], anything else a
/// [`lib::Error::Config`] message naming the command. `Error::Io` carries a
/// touched path, which a command name is not.
fn spawn_error(command: &'static str, e: &std::io::Error) -> Stop {
    if e.kind() == std::io::ErrorKind::NotFound {
        Stop::MissingTool(command)
    } else {
        Stop::Fatal(lib::Error::Config(format!("failed to run {command}: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    /// Every input of the password decision, with the repair scan asked only
    /// when its answer can change the outcome.
    #[test]
    fn a_password_is_asked_only_for_privileged_work() {
        let never = || -> Result<bool, crate::lib::Error> {
            panic!("the repair scan ran although its answer cannot change the outcome")
        };
        // (system, root, has_updates, repair_pending, expected)
        let decided = [
            (false, false, true, "the run writes only user dirs"),
            (true, true, true, "root needs no password"),
        ];
        for (system, root, has_updates, case) in decided {
            assert!(
                !super::password_needed(system, root, has_updates, never).unwrap(),
                "{case}"
            );
        }
        assert!(
            super::password_needed(true, false, true, never).unwrap(),
            "a system update with something to install asks"
        );
        assert!(
            super::password_needed(true, false, false, || Ok(true)).unwrap(),
            "a system run with a killed run's leftovers to repair asks"
        );
        assert!(
            !super::password_needed(true, false, false, || Ok(false)).unwrap(),
            "a system run with nothing to install or repair never asks"
        );
        let failed = super::password_needed(true, false, false, || {
            Err(crate::lib::Error::Config("scan failed".into()))
        });
        assert!(
            matches!(failed, Err(crate::lib::Error::Config(ref m)) if m == "scan failed"),
            "a failed repair scan stops the run: {failed:?}"
        );
    }

    /// Library installs called in lines of code; a comment line is skipped.
    fn library_installs(source: &str) -> usize {
        source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .map(|line| line.matches(concat!("lib::", "install(")).count())
            .sum()
    }

    /// The password decision runs before every library install only when one
    /// function holds the bin's only call: a second call site could skip it,
    /// and the host-dependent `--system` tests that reach the call sites skip
    /// on a machine without system components.
    #[test]
    fn every_library_install_passes_the_password_decision() {
        assert_eq!(
            library_installs(concat!("let a = lib::", "install(&[]);")),
            1
        );
        assert_eq!(library_installs(concat!("// lib::", "install(&[])")), 0);
        assert_eq!(library_installs(include_str!("main.rs")), 1);
    }

    /// Whether the function holding the library install decides the password
    /// first, then authenticates whenever the decision says so, then installs.
    /// A narrowing folded into the `let needed = …` expression itself is not
    /// seen.
    fn authenticates_before_installing(source: &str) -> bool {
        let Some(start) = source.find(concat!("fn install_", "authenticated(")) else {
            return false;
        };
        let body = &source[start..];
        let body = &body[..body.find("\n}\n").unwrap_or(body.len())];
        let at = |needle: &str| body.find(needle);
        match (
            at(concat!("let needed = password_", "needed(")),
            at(concat!("if needed {\n        validate_", "sudo()?;\n    }")),
            at(concat!("lib::", "install(")),
        ) {
            (Some(decide), Some(authenticate), Some(install)) => {
                decide < authenticate && authenticate < install
            }
            _ => false,
        }
    }

    /// The host-dependent `--system` tests that run `sudo -v` skip on a machine
    /// without system components, so the order is pinned in the source too.
    #[test]
    fn the_library_install_follows_the_sudo_check() {
        let shaped = |inner: &str| format!("fn install_{}(\n{inner}\n}}\n", "authenticated");
        let decide = concat!("let needed = password_", "needed(");
        let authenticate = concat!("if needed {\n        validate_", "sudo()?;\n    }");
        let install = concat!("lib::", "install(u)");
        assert!(authenticates_before_installing(&shaped(&format!(
            "{decide}) {authenticate} {install}"
        ))));
        assert!(!authenticates_before_installing(&shaped(&format!(
            "{decide}) {install}"
        ))));
        assert!(!authenticates_before_installing(&shaped(&format!(
            "{decide}) {install} {authenticate}"
        ))));
        assert!(!authenticates_before_installing(&shaped(&format!(
            "{decide}) if needed && false {{\n        validate_{}()?;\n    }} {install}",
            "sudo"
        ))));
        assert!(authenticates_before_installing(include_str!("main.rs")));
    }

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

    /// `main` installs the stderr logger; this test binary installs its own so
    /// the level of each line is observable.
    fn install_capture_logger() {
        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| {
            log::set_logger(&CAPTURE).expect("no other logger in this test binary");
            log::set_max_level(log::LevelFilter::Debug);
        });
    }

    #[test]
    fn a_failed_restart_is_an_error_line() {
        install_capture_logger();

        super::log_restart_failure(&libplasmoid_updater::Error::Config(
            "could not reach plasmashell".into(),
        ));

        let logged = RESTART_LOG.lock().expect("the restart log mutex");
        // The log is process-wide, so another test's line may follow this one.
        assert!(
            logged.iter().any(|(level, message)| *level == log::Level::Error
                && message
                    == "failed to restart plasmashell: configuration error: could not reach plasmashell"),
            "{logged:?}"
        );
    }

    /// A failed `sudo -v` names the exit status, never the doubled
    /// `exit status: exit status: 1` shape an `ExitStatus` interpolates.
    #[test]
    fn a_failed_sudo_authentication_names_the_exit_status() {
        use std::os::unix::process::ExitStatusExt as _;

        let error = super::sudo_auth_failure(&std::process::ExitStatus::from_raw(1 << 8));

        assert_eq!(
            error.to_string(),
            "configuration error: sudo authentication failed: sudo exited with status 1"
        );
    }

    /// The bin's own formatter copy pins its signal branch, the same string the
    /// library's formatter renders for a signal death.
    #[test]
    fn the_bin_command_failure_names_a_signal_death() {
        use std::os::unix::process::ExitStatusExt as _;

        let message = super::command_failure("sudo", &std::process::ExitStatus::from_raw(9));

        assert_eq!(message, "sudo was killed by signal 9 (SIGKILL)");
    }

    /// The bin's own formatter copy pins its no-status branch, the same string
    /// the library's formatter renders when no code and no signal are known.
    #[test]
    fn the_bin_command_failure_names_a_missing_status() {
        use std::os::unix::process::ExitStatusExt as _;

        let message = super::command_failure("sudo", &std::process::ExitStatus::from_raw(0x7f));

        assert_eq!(message, "sudo exited without a status code");
    }

    #[test]
    fn a_restart_prompt_on_a_non_tty_warns_and_names_the_flag() {
        let plan = super::restart_plan(super::RestartDecision::Prompt, true, false);
        let super::RestartPlan::SkipWarn(message) = plan else {
            panic!("a non-interactive stdin must skip the restart prompt: {plan:?}");
        };
        assert!(message.contains("skipped"), "{message}");
        assert!(message.contains("--restart-plasma"), "{message}");
    }

    #[test]
    fn a_restart_prompt_on_a_tty_asks_and_a_skip_needs_no_warning() {
        use super::{RestartDecision, RestartPlan, restart_plan};
        assert_eq!(
            restart_plan(RestartDecision::Prompt, true, true),
            RestartPlan::Prompt
        );
        assert_eq!(
            restart_plan(RestartDecision::Prompt, false, false),
            RestartPlan::Skip
        );
        assert_eq!(
            restart_plan(RestartDecision::Always, true, false),
            RestartPlan::Restart
        );
        assert_eq!(
            restart_plan(RestartDecision::Never, true, true),
            RestartPlan::Skip
        );
    }

    #[test]
    fn a_skipped_restart_prompt_is_logged_as_a_warning() {
        install_capture_logger();

        super::apply_restart(super::RestartPlan::SkipWarn(super::RESTART_PROMPT_SKIPPED));

        let logged = RESTART_LOG.lock().expect("the restart log mutex");
        // Matched by its own message, not by position: the log is process-wide.
        assert!(
            logged
                .iter()
                .any(|(level, message)| *level == log::Level::Warn
                    && message == super::RESTART_PROMPT_SKIPPED),
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

    fn system_scheme() -> crate::lib::InstalledComponent {
        crate::lib::InstalledComponent::new(
            "Breeze Dark",
            "BreezeDark.colors",
            crate::lib::ComponentType::ColorScheme,
            "/usr/share/color-schemes/BreezeDark.colors",
        )
        .with_system(true)
    }

    fn skipping(name: &str) -> crate::lib::CheckResult {
        let mut result = crate::lib::CheckResult::default();
        result.skipped.push(name.to_string());
        result
    }

    /// The check shape of a `--system` run that skipped `component` for want
    /// of a store entry: its name in `skipped`, the component in `unmatched`.
    fn unmatching(component: &crate::lib::InstalledComponent) -> crate::lib::CheckResult {
        let mut result = skipping(&component.name);
        result.unmatched.push(component.clone());
        result
    }

    /// A diagnostic about `path`, built through the public serde shape: the
    /// type has no public constructor.
    fn diagnostic_for(name: &str, path: &str) -> crate::lib::Diagnostic {
        serde_json::from_value(serde_json::json!({
            "name": name,
            "path": path,
            "reason": "failed to fetch store entry",
        }))
        .unwrap()
    }

    /// A `--system` check skips a system component no store entry matches;
    /// no exclusion rule names it, so the named path must not call it excluded.
    #[test]
    fn a_skipped_component_no_rule_excludes_is_unmatched() {
        let component = system_scheme();
        let config = crate::lib::Config::new().with_system(true);
        let named = super::classify_installed(&component, &config, &unmatching(&component));
        assert!(
            matches!(&named, super::ComponentOutcome::Unmatched),
            "an unmatched skip read as another outcome"
        );
    }

    /// Two system schemes share the display name `Breeze Dark`; only the
    /// sibling is unmatched. The sibling's skip must not answer for the
    /// named component, and the sibling still reads unmatched.
    #[test]
    fn a_same_named_siblings_unmatched_skip_does_not_answer_for_a_component() {
        let component = system_scheme();
        let sibling = crate::lib::InstalledComponent::new(
            "Breeze Dark",
            "BreezeDark2.colors",
            crate::lib::ComponentType::ColorScheme,
            "/usr/share/color-schemes/BreezeDark2.colors",
        )
        .with_system(true);
        let config = crate::lib::Config::new().with_system(true);
        let check_result = unmatching(&sibling);

        assert!(
            matches!(
                super::classify_installed(&component, &config, &check_result),
                super::ComponentOutcome::UpToDate
            ),
            "a component read as unmatched through a same-named sibling's skip"
        );
        assert!(
            matches!(
                super::classify_installed(&sibling, &config, &check_result),
                super::ComponentOutcome::Unmatched
            ),
            "the unmatched sibling itself read as another outcome"
        );
    }

    /// An icon theme and a cursor theme can share one directory under
    /// `--system`: the cursor theme's unmatched skip is not the icon theme's.
    #[test]
    fn an_unmatched_component_of_another_type_at_the_same_path_does_not_answer() {
        let path = "/usr/share/icons/Breeze_Snow";
        let icons = crate::lib::InstalledComponent::new(
            "Breeze Snow",
            "Breeze_Snow",
            crate::lib::ComponentType::IconTheme,
            path,
        )
        .with_system(true);
        let cursors = crate::lib::InstalledComponent::new(
            "Breeze Snow",
            "Breeze_Snow",
            crate::lib::ComponentType::CursorTheme,
            path,
        )
        .with_system(true);
        let config = crate::lib::Config::new().with_system(true);

        assert!(
            matches!(
                super::classify_installed(&icons, &config, &unmatching(&cursors)),
                super::ComponentOutcome::UpToDate
            ),
            "an icon theme read as unmatched through a cursor theme at its path"
        );
    }

    /// A same-named sibling's failed check must not answer for a component
    /// the check settled, and still answers for the sibling itself.
    #[test]
    fn a_same_named_siblings_diagnostic_does_not_answer_for_a_component() {
        let component = crate::lib::InstalledComponent::new(
            "Fake Scheme",
            "FakeScheme.colors",
            crate::lib::ComponentType::ColorScheme,
            "/home/me/.local/share/color-schemes/FakeScheme.colors",
        );
        let sibling_path = "/home/me/.local/share/color-schemes/Second.colors";
        let sibling = crate::lib::InstalledComponent::new(
            "Fake Scheme",
            "Second.colors",
            crate::lib::ComponentType::ColorScheme,
            sibling_path,
        );
        let mut check_result = crate::lib::CheckResult::default();
        check_result
            .diagnostics
            .push(diagnostic_for("Fake Scheme", sibling_path));
        let config = crate::lib::Config::new();

        assert!(
            matches!(
                super::classify_installed(&component, &config, &check_result),
                super::ComponentOutcome::UpToDate
            ),
            "a component read as failed through a same-named sibling's diagnostic"
        );
        assert!(
            matches!(
                super::classify_installed(&sibling, &config, &check_result),
                super::ComponentOutcome::CheckFailed(diagnostic)
                    if diagnostic.path == std::path::Path::new(sibling_path)
            ),
            "the failed sibling itself read as another outcome"
        );
    }

    /// A diagnostic answers only for the component at its path AND name: two
    /// component types can share one directory under `--system` with
    /// different names, so the cursor theme's diagnostic is not the icon
    /// theme's.
    #[test]
    fn a_diagnostic_answers_only_for_the_component_of_its_path_and_name() {
        let path = "/usr/share/icons/Breeze_Snow";
        let icons = crate::lib::InstalledComponent::new(
            "Breeze Snow",
            "Breeze_Snow",
            crate::lib::ComponentType::IconTheme,
            path,
        )
        .with_system(true);
        let cursors = crate::lib::InstalledComponent::new(
            "Breeze Snow Cursors",
            "Breeze_Snow",
            crate::lib::ComponentType::CursorTheme,
            path,
        )
        .with_system(true);
        let mut check_result = crate::lib::CheckResult::default();
        check_result
            .diagnostics
            .push(diagnostic_for("Breeze Snow Cursors", path));
        let config = crate::lib::Config::new().with_system(true);

        assert!(
            matches!(
                super::classify_installed(&icons, &config, &check_result),
                super::ComponentOutcome::UpToDate
            ),
            "an icon theme read as failed through a cursor theme's diagnostic at its path"
        );
        assert!(
            matches!(
                super::classify_installed(&cursors, &config, &check_result),
                super::ComponentOutcome::CheckFailed(diagnostic)
                    if diagnostic.path == std::path::Path::new(path)
            ),
            "the cursor theme's own diagnostic read as another outcome"
        );
    }

    /// An exclusion still reads as one, by an exact rule or a pattern, whether
    /// or not the check listed the component as skipped.
    #[test]
    fn an_excluded_component_stays_excluded() {
        let component = system_scheme();
        let configs = [
            crate::lib::Config::new()
                .with_system(true)
                .with_excluded_packages(vec!["BreezeDark.colors".to_string()]),
            crate::lib::Config::new()
                .with_system(true)
                .with_excluded_patterns(vec!["^Breeze".to_string()]),
        ];
        for config in &configs {
            for check_result in [skipping("Breeze Dark"), crate::lib::CheckResult::default()] {
                let named = super::classify_installed(&component, config, &check_result);
                assert!(
                    matches!(&named, super::ComponentOutcome::Excluded),
                    "an excluded component read as another outcome"
                );
            }
        }
    }

    /// A component neither skipped nor excluded has no pending update.
    #[test]
    fn a_component_neither_skipped_nor_excluded_is_up_to_date() {
        let component = system_scheme();
        let config = crate::lib::Config::new().with_system(true);
        let named =
            super::classify_installed(&component, &config, &crate::lib::CheckResult::default());
        assert!(matches!(named, super::ComponentOutcome::UpToDate));
    }

    /// `skipped` lists display names, so on a user run a current component
    /// shares its name with an excluded sibling's skip; only a system
    /// component can be skipped for want of a store entry.
    #[test]
    fn a_user_component_sharing_a_skipped_name_is_up_to_date() {
        let current = crate::lib::InstalledComponent::new(
            "Weather Widget",
            "b.weather",
            crate::lib::ComponentType::PlasmaWidget,
            "/home/me/.local/share/plasma/plasmoids/b.weather",
        );
        let config =
            crate::lib::Config::new().with_excluded_packages(vec!["a.weather".to_string()]);
        let named = super::classify_installed(&current, &config, &skipping("Weather Widget"));
        assert!(
            matches!(named, super::ComponentOutcome::UpToDate),
            "a user component read as unmatched through a sibling's skip"
        );
    }

    /// The line both named paths print for a settled outcome, keyed by
    /// outcome rather than by print arm, so the two cannot drift.
    #[test]
    fn settled_outcomes_print_their_line() {
        let component = system_scheme();
        let label = super::settled_label("BreezeDark.colors", &component, false);
        let suffixed = super::settled_label("Breeze Dark", &component, true);

        assert_eq!(
            super::settled_outcome(super::Settled::Excluded, &label),
            r#""BreezeDark.colors" is excluded"#.to_string(),
            "an excluded outcome printed the wrong line"
        );
        assert_eq!(
            super::settled_outcome(super::Settled::Unmatched, &label),
            r#"no KDE Store entry matches "BreezeDark.colors""#.to_string(),
            "an unmatched outcome printed the wrong line"
        );
        assert_eq!(
            super::settled_outcome(super::Settled::UpToDate, &label),
            r#"no update available for "BreezeDark.colors""#.to_string(),
            "an up-to-date outcome printed the wrong line"
        );
        assert_eq!(
            super::settled_outcome(super::Settled::UpToDate, &suffixed),
            r#"no update available for "BreezeDark.colors" (Color Scheme)"#.to_string(),
            "the suffixed line lost the directory name or the kind"
        );
    }

    /// A name covering a pending update row and a same-named up-to-date
    /// sibling answers for each: the row stays the pending component's own,
    /// and the sibling's line carries the directory name and the kind, since
    /// the shared name alone cannot tell them apart.
    #[test]
    fn a_pending_row_and_an_up_to_date_sibling_answer_each_for_themselves() {
        let pending = crate::lib::InstalledComponent::new(
            "Fake Scheme",
            "First.colors",
            crate::lib::ComponentType::ColorScheme,
            "/home/me/.local/share/color-schemes/First.colors",
        );
        let current = crate::lib::InstalledComponent::new(
            "Fake Scheme",
            "Second.colors",
            crate::lib::ComponentType::ColorScheme,
            "/home/me/.local/share/color-schemes/Second.colors",
        );
        let row = crate::lib::AvailableUpdate::builder(pending.clone(), 42, "2.0").build();
        let mut check_result = crate::lib::CheckResult::default();
        check_result.available_updates.push(row.clone());
        let config = crate::lib::Config::new();

        let resolved = super::resolve_components(
            "Fake Scheme",
            &[pending.clone(), current.clone()],
            &config,
            &check_result,
        );

        assert_eq!(resolved.len(), 2, "{resolved:?}");
        let pending_resolved = resolved
            .iter()
            .find(|named| named.component.path == pending.path)
            .expect("the pending component is covered");
        assert!(
            matches!(
                &pending_resolved.outcome,
                super::ComponentOutcome::Update(update)
                    if update.installed.path == pending.path
            ),
            "the pending component's row read as another outcome"
        );
        let current_resolved = resolved
            .iter()
            .find(|named| named.component.path == current.path)
            .expect("the current sibling is covered");
        assert!(
            matches!(&current_resolved.outcome, super::ComponentOutcome::UpToDate),
            "the up-to-date sibling read as another outcome"
        );
        assert_eq!(
            super::settled_outcome(
                super::Settled::UpToDate,
                &super::settled_label("Fake Scheme", &current, true),
            ),
            r#"no update available for "Second.colors" (Color Scheme)"#.to_string(),
            "the suffixed up-to-date line named the wrong component"
        );
    }

    /// A same-named sibling's own row does not answer for a component: the
    /// name resolves to every covered component, and each answers by its own
    /// row's identity. A no-download failure row matches the same way, by
    /// install path and type.
    #[test]
    fn a_same_named_siblings_row_does_not_answer_for_a_component() {
        let component = crate::lib::InstalledComponent::new(
            "a.weather",
            "a.weather",
            crate::lib::ComponentType::PlasmaWidget,
            "/home/me/.local/share/plasma/plasmoids/a.weather",
        );
        let sibling = crate::lib::InstalledComponent::new(
            "a.weather",
            "b.weather",
            crate::lib::ComponentType::PlasmaWidget,
            "/home/me/.local/share/plasma/plasmoids/b.weather",
        );
        let same_path_other_type = crate::lib::InstalledComponent::new(
            "a.weather",
            "a.weather",
            crate::lib::ComponentType::CursorTheme,
            component.path.clone(),
        );
        let row = crate::lib::AvailableUpdate::builder(sibling.clone(), 42, "2.0").build();
        let mut check_result = crate::lib::CheckResult::default();
        check_result.available_updates.push(row.clone());
        let config = crate::lib::Config::new();

        let resolved = super::resolve_components(
            "a.weather",
            &[component.clone(), sibling.clone()],
            &config,
            &check_result,
        );

        let component_resolved = resolved
            .iter()
            .find(|named| named.component.path == component.path)
            .expect("the named component is covered");
        assert!(
            matches!(
                &component_resolved.outcome,
                super::ComponentOutcome::UpToDate
            ),
            "a sibling's own row answered for the component"
        );
        let sibling_resolved = resolved
            .iter()
            .find(|named| named.component.path == sibling.path)
            .expect("the sibling is covered");
        assert!(
            matches!(
                &sibling_resolved.outcome,
                super::ComponentOutcome::Update(update) if update.installed.path == sibling.path
            ),
            "the sibling's own row read as another outcome"
        );
        assert!(
            super::own_row(&component, &component)
                && !super::own_row(&component, &sibling)
                && !super::own_row(&component, &same_path_other_type),
            "the row identity predicate answered for the wrong component"
        );
    }
}
