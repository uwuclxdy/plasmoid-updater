// SPDX-FileCopyrightText: 2025 uwuclxdy
// SPDX-License-Identifier: GPL-3.0-or-later

mod cli_config;
mod exit_code;

use std::io::{IsTerminal, Write as _};

use clap::{Parser, Subcommand};

use cli_config::CliConfig;
use exit_code::ExitCode;
use libplasmoid_updater as lib;
use libplasmoid_updater::Reporter as _;

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
        #[arg(
            long,
            value_delimiter = ',',
            value_name = "NAMES",
            help = "names to skip for this run (comma-separated or repeatable)"
        )]
        ignore: Vec<String>,
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
    ignore: Vec<String>,
}

fn main() {
    let cli = Cli::parse();

    let exit_code = run(cli).unwrap_or_else(|e| {
        eprintln!("error: {e}");
        ExitCode::FatalError
    });

    std::process::exit(exit_code.into());
}

fn run(cli: Cli) -> Result<ExitCode, lib::Error> {
    if cli.edit_config {
        CliConfig::edit_config()?;
        return Ok(ExitCode::Success);
    }

    let mut config = CliConfig::load()?;
    config.inner.system = cli.system;
    config.inner.skip_plasma_detection = cli.skip_plasma_detection;

    execute_command(&cli, &config)
}

fn execute_command(cli: &Cli, config: &CliConfig) -> Result<ExitCode, lib::Error> {
    if cli.system && !is_root_user() {
        validate_sudo()?;
    }

    match &cli.command {
        None => do_update(config, UpdateArgs::default()),
        Some(Commands::Check { ignore }) => do_check(config, ignore),
        Some(Commands::ListInstalled) => do_list_installed(config),
        Some(Commands::Update {
            component,
            restart_plasma,
            no_restart_plasma,
            yes,
            ignore,
        }) => do_update(
            config,
            UpdateArgs {
                component: component.clone(),
                restart_plasma: *restart_plasma,
                no_restart_plasma: *no_restart_plasma,
                yes: *yes,
                ignore: ignore.clone(),
            },
        ),
    }
}

fn do_check(config: &CliConfig, ignore: &[String]) -> Result<ExitCode, lib::Error> {
    let mut check_config = config.inner.clone();
    check_config
        .excluded_packages
        .extend(ignore.iter().cloned());

    let reporter = lib::cli::ProgressReporter::new();
    reporter.event(&lib::Event::Checking);

    let result = lib::check(&check_config)?;

    if result.available_updates.is_empty() {
        println!("no updates available");
        return Ok(ExitCode::Success);
    }

    println!(
        "{} update{} available.",
        result.available_updates.len(),
        if result.available_updates.len() == 1 {
            ""
        } else {
            "s"
        }
    );
    lib::cli::print_updates_table(&result.available_updates);
    Ok(ExitCode::Success)
}

fn do_list_installed(config: &CliConfig) -> Result<ExitCode, lib::Error> {
    let components = lib::get_installed(&config.inner)?;

    if components.is_empty() {
        println!("no components installed");
        return Ok(ExitCode::Success);
    }

    println!(
        "{} installed component{}.",
        components.len(),
        if components.len() == 1 { "" } else { "s" }
    );
    lib::cli::print_components_table(&components);
    Ok(ExitCode::Success)
}

fn do_update(config: &CliConfig, args: UpdateArgs) -> Result<ExitCode, lib::Error> {
    let mut update_config = config.inner.clone();
    update_config.excluded_packages.extend(args.ignore);

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
        return do_update_single(name, update_config, restart);
    }

    let reporter = lib::cli::ProgressReporter::new();

    let result = if auto || !std::io::stdin().is_terminal() {
        lib::update(&update_config, &reporter)?
    } else {
        reporter.event(&lib::Event::Checking);
        let check_result = lib::check(&update_config)?;
        // The summary must count the same universe the update() path counts:
        // everything check() set aside (exclusions, unresolved components)
        // rides along as skipped, including on the branch that installs nothing.
        let mut skipped = check_result.skipped;
        skipped.extend(check_result.diagnostics.iter().map(|d| d.name.clone()));
        if check_result.available_updates.is_empty() {
            return Ok(finish_update(
                lib::UpdateResult {
                    skipped,
                    ..lib::UpdateResult::default()
                },
                restart,
            ));
        }
        let selected = lib::cli::select(&check_result.available_updates)?;
        if selected.is_empty() {
            println!("nothing to update");
            return Ok(ExitCode::Success);
        }
        let mut result = lib::install(&selected, &update_config, &reporter)?;
        result.skipped.extend(skipped);
        result
    };

    Ok(finish_update(result, restart))
}

fn do_update_single(
    name: &str,
    config: lib::Config,
    restart: RestartDecision,
) -> Result<ExitCode, lib::Error> {
    let reporter = lib::cli::ProgressReporter::new();
    reporter.event(&lib::Event::Checking);

    let check_result = lib::check(&config)?;

    let excluded = excluded_by_configuration(name, &config, &check_result.skipped);

    let matched: Vec<lib::AvailableUpdate> = check_result
        .available_updates
        .into_iter()
        .filter(|u| u.installed.name == name || u.installed.directory_name == name)
        .collect();

    if matched.is_empty() {
        if excluded {
            println!("'{name}' is excluded by configuration");
        } else {
            println!("no update available for '{name}'");
        }
        return Ok(ExitCode::Success);
    }

    let result = lib::install(&matched, &config, &reporter)?;
    Ok(finish_update(result, restart))
}

/// Prints the summary and failure table, applies the restart decision and
/// maps the outcome to today's exit codes.
fn finish_update(result: lib::UpdateResult, restart: RestartDecision) -> ExitCode {
    if result.succeeded.is_empty() && result.failed.is_empty() && result.skipped.is_empty() {
        println!("no updates available");
        return ExitCode::Success;
    }

    println!(
        "Update Summary: {} succeeded, {} failed, {} skipped ({} total)",
        result.succeeded.len(),
        result.failed.len(),
        result.skipped.len(),
        result.succeeded.len() + result.failed.len() + result.skipped.len(),
    );

    if !result.failed.is_empty() {
        lib::cli::print_failures(&result);
    }

    maybe_restart(result.needs_restart(), restart);

    if result.failed.is_empty() {
        ExitCode::Success
    } else {
        ExitCode::PartialFailure
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
    if confirmed && let Err(e) = lib::restart_plasmashell() {
        eprintln!("warning: failed to restart plasmashell: {e}");
    }
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

/// True when `name` matches a configured exclusion, so an empty `update
/// <name>` match is the exclusion rather than a component that does not exist.
///
/// The check's `skipped` list holds the excluded components' display names;
/// `excluded_packages` holds the exact names and directories configured,
/// `--ignore` included. A name matched only by an `excluded_patterns` regex is
/// not detected here — the bin holds no regex engine.
fn excluded_by_configuration(name: &str, config: &lib::Config, skipped: &[String]) -> bool {
    skipped.iter().any(|excluded| excluded == name)
        || config
            .excluded_packages
            .iter()
            .any(|excluded| excluded == name)
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
