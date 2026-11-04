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
        report_diagnostic(diagnostic);
    }
}

/// Reports every diagnostic but `except`: a component reported as a failure
/// row must not also print a diagnostic line.
fn report_diagnostics_except(result: &lib::CheckResult, except: &[&lib::Diagnostic]) {
    for diagnostic in &result.diagnostics {
        if except.contains(&diagnostic) {
            continue;
        }
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

    authenticate_system(config.inner.system)?;

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
    lib::install(&[], &update_config, &())?;

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
    // "run update to restore" tells the user to do.
    let mut result = lib::install(&installable, &update_config, reporter.as_dyn())?;
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

/// The check shape a named dry run reports: only the named component's own
/// outcome, so another component's failures do not leak into it.
fn named_check_result(
    available_updates: Vec<lib::AvailableUpdate>,
    skipped: Vec<String>,
    diagnostics: Vec<lib::Diagnostic>,
    unavailable: Vec<lib::Failure>,
) -> lib::CheckResult {
    let mut result = lib::CheckResult::default();
    result.available_updates = available_updates;
    result.skipped = skipped;
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
        // A named run ends on the named component's own outcome; another
        // component's failed check is not this run's.
        Some(name) => match resolve_named(name, config, &check_result)? {
            Named::Updates(updates) => {
                if json {
                    payload_json(&named_check_result(
                        updates,
                        Vec::new(),
                        Vec::new(),
                        Vec::new(),
                    ))?;
                } else {
                    payload(format_args!(
                        "{}",
                        lib::cli::updates_table(&updates, lib::cli::Stream::Stdout)
                    ))?;
                }
                Ok(ExitCode::Success)
            }
            Named::Excluded(display_name) => {
                settled_dry_run(Settled::Excluded, vec![display_name], name, json)
            }
            Named::Unmatched(display_name) => {
                settled_dry_run(Settled::Unmatched, vec![display_name], name, json)
            }
            Named::UpToDate => settled_dry_run(Settled::UpToDate, Vec::new(), name, json),
            Named::Unavailable(failure) => {
                report_failure_rows(vec![failure.clone()]);
                if json {
                    // The failure and the diagnostic the check settled for it
                    // travel together, the way `check` reports them.
                    let diagnostic = check_result
                        .diagnostics
                        .iter()
                        .find(|diagnostic| diagnostic.name == failure.component.name)
                        .cloned();
                    payload_json(&named_check_result(
                        Vec::new(),
                        Vec::new(),
                        diagnostic.into_iter().collect(),
                        vec![failure],
                    ))?;
                }
                Ok(ExitCode::Failure)
            }
            Named::CheckFailed(diagnostic) => {
                report_diagnostic(&diagnostic);
                if json {
                    payload_json(&named_check_result(
                        Vec::new(),
                        Vec::new(),
                        vec![diagnostic],
                        Vec::new(),
                    ))?;
                }
                Ok(ExitCode::Failure)
            }
            Named::Unknown => Ok(unknown_component(name)),
        },
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

fn do_update_single(
    name: &str,
    config: lib::Config,
    restart: RestartDecision,
    quiet: bool,
    json: bool,
) -> Result<ExitCode, Stop> {
    let reporter = Reporter::new(quiet);
    let check_result = lib::check(&config, reporter.as_dyn())?;

    match resolve_named(name, &config, &check_result)? {
        Named::Updates(updates) => {
            let result = lib::install(&updates, &config, reporter.as_dyn())?;
            finish_update(result, restart, false, json)
        }
        Named::Excluded(display_name) => {
            settled_update(Settled::Excluded, vec![display_name], name, json)
        }
        Named::Unmatched(display_name) => {
            settled_update(Settled::Unmatched, vec![display_name], name, json)
        }
        Named::UpToDate => settled_update(Settled::UpToDate, Vec::new(), name, json),
        Named::Unavailable(failure) => {
            let mut result = lib::UpdateResult::default();
            result.failed.push(failure);
            finish_update(result, restart, false, json)
        }
        Named::CheckFailed(diagnostic) => {
            report_diagnostic(&diagnostic);
            if json {
                let mut result = lib::UpdateResult::default();
                result.skipped.push(diagnostic.name);
                payload_json(&result)?;
            }
            Ok(ExitCode::Failure)
        }
        Named::Unknown => Ok(unknown_component(name)),
    }
}

/// What a component name on the command line resolves to.
enum Named {
    /// Pending updates for the installed component the name matched.
    Updates(Vec<lib::AvailableUpdate>),
    /// An exclusion rule in the configuration matches the installed component,
    /// with its display name, the name `skipped` lists on every path.
    Excluded(String),
    /// No exclusion rule matches the installed component and the check's
    /// `skipped` lists its display name: under `--system`, no store entry of
    /// its type matches it, or a same-named system sibling's does not, since
    /// `skipped` carries display names only.
    Unmatched(String),
    /// The installed component has no pending update.
    UpToDate,
    /// The installed component's store entry offers no download link for its
    /// version, so the run reports the failure row the check settled.
    Unavailable(lib::Failure),
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

    if let Some(failure) = check_result
        .unavailable
        .iter()
        .find(|failure| failure.component.name == name || failure.component.directory_name == name)
    {
        return Ok(Named::Unavailable(failure.clone()));
    }

    let installed = lib::get_installed(config)?;
    let Some(component) = installed
        .iter()
        .find(|c| c.name == name || c.directory_name == name)
    else {
        return Ok(Named::Unknown);
    };

    Ok(classify_installed(component, config, check_result))
}

/// Classifies an installed component the check left without an update row.
fn classify_installed(
    component: &lib::InstalledComponent,
    config: &lib::Config,
    check_result: &lib::CheckResult,
) -> Named {
    // A component the check could not settle has no update row to match, and
    // saying "no update available" would claim what the check did not learn.
    if let Some(diagnostic) = check_result
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.name == component.name)
    {
        return Named::CheckFailed(diagnostic.clone());
    }

    // `skipped` holds excluded components and, under `--system`, those no
    // store entry matches, so only the rules themselves can tell them apart.
    // It lists display names, which siblings can share: a user component is
    // never skipped for want of a store entry, and a system one can read
    // unmatched through a same-named sibling's skip.
    if config.excludes(component) {
        Named::Excluded(component.name.clone())
    } else if component.is_system && check_result.skipped.contains(&component.name) {
        Named::Unmatched(component.name.clone())
    } else {
        Named::UpToDate
    }
}

/// The line a named run prints for a component no store entry matches.
fn unmatched_line(name: &str) -> String {
    format!("no KDE Store entry matches {name:?}")
}

/// An outcome the check settled without a failure.
#[derive(Clone, Copy)]
enum Settled {
    Excluded,
    Unmatched,
    UpToDate,
}

/// The stdout line and exit code a named run reports for a settled outcome:
/// the dry run and the update path share it, so the two cannot drift.
fn settled_outcome(settled: Settled, name: &str) -> (String, ExitCode) {
    match settled {
        Settled::Excluded => (format!("{name:?} is excluded"), ExitCode::Success),
        Settled::Unmatched => (unmatched_line(name), ExitCode::Success),
        Settled::UpToDate => (
            format!("no update available for {name:?}"),
            ExitCode::Success,
        ),
    }
}

/// Reports a settled outcome on a named dry run: the check shape with the
/// display names `skipped` lists under `--json`, else its line.
fn settled_dry_run(
    settled: Settled,
    skipped: Vec<String>,
    name: &str,
    json: bool,
) -> Result<ExitCode, Stop> {
    let (line, code) = settled_outcome(settled, name);
    if json {
        payload_json(&named_check_result(
            Vec::new(),
            skipped,
            Vec::new(),
            Vec::new(),
        ))?;
    } else {
        payload(format_args!("{line}"))?;
    }
    Ok(code)
}

/// Reports a settled outcome on a named update: an update result with the
/// display names `skipped` lists under `--json`, else its line.
fn settled_update(
    settled: Settled,
    skipped: Vec<String>,
    name: &str,
    json: bool,
) -> Result<ExitCode, Stop> {
    let (line, code) = settled_outcome(settled, name);
    if json {
        let mut result = lib::UpdateResult::default();
        result.skipped = skipped;
        payload_json(&result)?;
    } else {
        payload(format_args!("{line}"))?;
    }
    Ok(code)
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

/// Authenticates a system-wide run up front, so the installs do not stop on a
/// password prompt. Only a run that writes under `--system` gets here; `check`,
/// `list-installed` and `update --dry-run` read and never authenticate.
fn authenticate_system(system_wide: bool) -> Result<(), Stop> {
    if system_wide && !is_root_user() {
        validate_sudo()
    } else {
        Ok(())
    }
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

    /// A `--system` check skips a system component no store entry matches;
    /// no exclusion rule names it, so the named path must not call it excluded.
    #[test]
    fn a_skipped_component_no_rule_excludes_is_unmatched() {
        let component = system_scheme();
        let config = crate::lib::Config::new().with_system(true);
        let named = super::classify_installed(&component, &config, &skipping("Breeze Dark"));
        assert!(
            matches!(&named, super::Named::Unmatched(name) if name == "Breeze Dark"),
            "an unmatched skip read as another outcome"
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
                    matches!(&named, super::Named::Excluded(name) if name == "Breeze Dark"),
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
        assert!(matches!(named, super::Named::UpToDate));
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
            matches!(named, super::Named::UpToDate),
            "a user component read as unmatched through a sibling's skip"
        );
    }

    #[test]
    fn the_unmatched_line_names_the_missing_store_entry() {
        assert_eq!(
            super::unmatched_line("Breeze Dark"),
            r#"no KDE Store entry matches "Breeze Dark""#
        );
    }

    /// The line and exit code both named paths print for a settled outcome,
    /// keyed by outcome rather than by print arm, so the two cannot drift.
    #[test]
    fn settled_outcomes_print_their_line_and_succeed() {
        let name = "BreezeDark.colors";

        assert_eq!(
            super::settled_outcome(super::Settled::Excluded, name),
            (
                r#""BreezeDark.colors" is excluded"#.to_string(),
                super::ExitCode::Success
            ),
            "an excluded outcome printed the wrong line or exit code"
        );
        assert_eq!(
            super::settled_outcome(super::Settled::Unmatched, name),
            (
                r#"no KDE Store entry matches "BreezeDark.colors""#.to_string(),
                super::ExitCode::Success
            ),
            "an unmatched outcome printed the wrong line or exit code"
        );
        assert_eq!(
            super::settled_outcome(super::Settled::UpToDate, name),
            (
                r#"no update available for "BreezeDark.colors""#.to_string(),
                super::ExitCode::Success
            ),
            "an up-to-date outcome printed the wrong line or exit code"
        );
    }
}
