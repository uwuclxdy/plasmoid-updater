// SPDX-License-Identifier: MIT OR Apache-2.0

mod cli_config;
mod exit_code;

use clap::{Parser, Subcommand};

use cli_config::CliConfig;
use exit_code::ExitCode;
use libplasmoid_updater::{UpdateError, check, show_installed, update};

#[derive(Parser)]
#[command(name = "plasmoid-updater")]
#[command(about = "update kde plasma components from the kde store")]
#[command(version)]
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
}

#[derive(Subcommand)]
enum Commands {
    #[command(about = "check for available updates")]
    Check,
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
    },
}

#[derive(Default)]
struct UpdateArgs {
    component: Option<String>,
    restart_plasma: bool,
    no_restart_plasma: bool,
    yes: bool,
}

fn main() {
    let cli = Cli::parse();

    let exit_code = run(cli).unwrap_or_else(|e| {
        eprintln!("error: {e}");
        ExitCode::FatalError
    });

    std::process::exit(exit_code.into());
}

fn run(cli: Cli) -> Result<ExitCode, libplasmoid_updater::Error> {
    if cli.edit_config {
        CliConfig::edit_config()?;
        return Ok(ExitCode::Success);
    }

    let mut config = CliConfig::load()?;
    config.inner.system = cli.system;

    execute_command(&cli, &config)
}

fn execute_command(cli: &Cli, config: &CliConfig) -> Result<ExitCode, libplasmoid_updater::Error> {
    if cli.system && !is_root_user() {
        validate_sudo()?;
    }

    match &cli.command {
        None => do_update(config, UpdateArgs::default()),
        Some(Commands::Check) => do_check(config),
        Some(Commands::ListInstalled) => do_list_installed(config),
        Some(Commands::Update {
            component,
            restart_plasma,
            no_restart_plasma,
            yes,
        }) => do_update(
            config,
            UpdateArgs {
                component: component.clone(),
                restart_plasma: *restart_plasma,
                no_restart_plasma: *no_restart_plasma,
                yes: *yes,
            },
        ),
    }
}

fn do_check(config: &CliConfig) -> Result<ExitCode, libplasmoid_updater::Error> {
    check(&config.inner).map_err(|e| libplasmoid_updater::Error::other(e.to_string()))?;
    Ok(ExitCode::Success)
}

fn do_list_installed(config: &CliConfig) -> Result<ExitCode, libplasmoid_updater::Error> {
    show_installed(&config.inner)?;
    Ok(ExitCode::Success)
}

fn do_update(config: &CliConfig, args: UpdateArgs) -> Result<ExitCode, libplasmoid_updater::Error> {
    let mut update_config = config.inner.clone();

    if args.yes || config.assume_yes || config.update_all_by_default {
        update_config.yes = true;
    }

    if args.restart_plasma {
        update_config.restart = libplasmoid_updater::RestartBehavior::Always;
    } else if args.no_restart_plasma {
        update_config.restart = libplasmoid_updater::RestartBehavior::Never;
    }

    if let Some(ref name) = args.component {
        return do_update_single(name, update_config);
    }

    do_full_update(update_config)
}

fn do_update_single(
    name: &str,
    mut config: libplasmoid_updater::Config,
) -> Result<ExitCode, libplasmoid_updater::Error> {
    let check_result =
        check(&config).map_err(|e| libplasmoid_updater::Error::other(e.to_string()))?;

    let matched = check_result
        .available_updates
        .iter()
        .any(|u| u.name == name || u.directory_name == name);

    if !matched {
        println!("no update available for '{name}'");
        return Ok(ExitCode::Success);
    }

    let excluded: Vec<String> = check_result
        .available_updates
        .iter()
        .filter(|u| u.name != name && u.directory_name != name)
        .map(|u| u.directory_name.clone())
        .collect();

    config.excluded_packages.extend(excluded);
    config.yes = true;

    do_full_update(config)
}

fn do_full_update(
    config: libplasmoid_updater::Config,
) -> Result<ExitCode, libplasmoid_updater::Error> {
    match update(&config) {
        Ok(result) => {
            result.print_summary();
            if result.has_failures() {
                result.print_error_table();
                Ok(ExitCode::PartialFailure)
            } else {
                Ok(ExitCode::Success)
            }
        }
        Err(UpdateError::Check(e)) => Err(libplasmoid_updater::Error::other(e.to_string())),
        Err(UpdateError::Other(e)) => Err(libplasmoid_updater::Error::other(e.to_string())),
    }
}

fn is_root_user() -> bool {
    nix::unistd::Uid::effective().is_root()
}

fn validate_sudo() -> Result<(), libplasmoid_updater::Error> {
    let status = std::process::Command::new("sudo")
        .args(["-v"])
        .status()
        .map_err(|e| libplasmoid_updater::Error::other(format!("failed to run sudo: {e}")))?;

    if !status.success() {
        return Err(libplasmoid_updater::Error::other(
            "sudo authentication failed",
        ));
    }

    Ok(())
}
