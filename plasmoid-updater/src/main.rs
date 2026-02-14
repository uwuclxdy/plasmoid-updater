// SPDX-License-Identifier: MIT OR Apache-2.0

mod cli_config;
mod commands;
mod exit_code;
mod output;
mod progress;
mod system;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use cli_config::CliConfig;
use exit_code::ExitCode;
use output::{Verbosity, output_error, print_fatal_error};
use system::{escalate_with_sudo, is_root};

#[derive(Parser)]
#[command(name = "plasmoid-updater")]
#[command(about = "update kde plasma components from the kde store")]
#[command(version)]
#[command(disable_help_subcommand = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    #[arg(long, global = true)]
    system: bool,

    #[arg(short, long, global = true)]
    verbose: bool,

    #[arg(long, global = true)]
    json: bool,

    #[arg(long)]
    edit_config: bool,
}

#[derive(Subcommand)]
enum Commands {
    Check,
    ListInstalled,
    Update {
        component: Option<String>,
        #[arg(long)]
        restart_plasma: bool,
        #[arg(long)]
        no_restart_plasma: bool,
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

fn main() {
    let cli = Cli::parse();

    let exit_code = run(cli).unwrap_or_else(|e| {
        print_fatal_error(&e.to_string());
        ExitCode::FatalError
    });

    std::process::exit(exit_code.as_i32());
}

fn run(cli: Cli) -> Result<ExitCode, libplasmoid_updater::Error> {
    validate_root_usage(&cli)?;

    if cli.edit_config {
        return handle_edit_config(cli.json);
    }

    let config = load_config(cli.json)?;
    let verbosity = resolve_verbosity(&cli, &config);

    execute_command(&cli, &config, verbosity)
}

fn validate_root_usage(cli: &Cli) -> Result<(), libplasmoid_updater::Error> {
    if is_root() && !cli.system {
        output_error(cli.json, "running as root requires --system flag");
        return Err(libplasmoid_updater::Error::other("invalid root usage"));
    }
    Ok(())
}

fn handle_edit_config(json: bool) -> Result<ExitCode, libplasmoid_updater::Error> {
    CliConfig::edit_config().inspect_err(|e| {
        output_error(json, &e.to_string());
    })?;
    Ok(ExitCode::Success)
}

fn load_config(json: bool) -> Result<CliConfig, libplasmoid_updater::Error> {
    let widgets_id_path = widgets_id_path();
    let path = if widgets_id_path.exists() {
        Some(widgets_id_path.as_path())
    } else {
        None
    };

    CliConfig::load_with_widgets_id(path).map_err(|e| {
        output_error(json, &format!("failed to load config: {e}"));
        e
    })
}

fn widgets_id_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|root| root.join("../../widgets-id"))
        .unwrap_or_else(|| PathBuf::from("../../widgets-id"))
}

fn resolve_verbosity(cli: &Cli, config: &CliConfig) -> Verbosity {
    if cli.verbose {
        Verbosity::Verbose
    } else {
        config.verbosity
    }
}

fn execute_command(
    cli: &Cli,
    config: &CliConfig,
    verbosity: Verbosity,
) -> Result<ExitCode, libplasmoid_updater::Error> {
    let api_client = libplasmoid_updater::ApiClient::new();

    match cli.command.as_ref().unwrap_or(&Commands::Check) {
        Commands::Check => {
            commands::check::execute(cli.system, cli.json, config, verbosity, &api_client)
        }
        Commands::ListInstalled => {
            commands::list_installed::execute(cli.system, cli.json, verbosity)
        }
        Commands::Update {
            component,
            restart_plasma,
            no_restart_plasma,
            yes,
        } => {
            handle_sudo_escalation(cli)?;
            commands::update::execute(
                cli.system,
                cli.json,
                config,
                commands::update::Options {
                    component: component.as_deref(),
                    restart_plasma: *restart_plasma,
                    no_restart_plasma: *no_restart_plasma,
                    yes: *yes,
                    verbosity,
                },
                &api_client,
            )
        }
    }
}

fn handle_sudo_escalation(cli: &Cli) -> Result<(), libplasmoid_updater::Error> {
    if cli.system && !is_root() {
        let code = escalate_with_sudo()?;
        std::process::exit(code.as_i32());
    }
    Ok(())
}
