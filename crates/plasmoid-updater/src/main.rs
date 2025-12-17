// SPDX-License-Identifier: MIT OR Apache-2.0

mod config;

use std::io::{self, Write};

use clap::{Parser, Subcommand};
use libplasmoid_updater::{
    AvailableUpdate, CheckResult, InstalledComponent, JsonOutput, UpdateSummary, Verbosity,
    check_updates_system_with_details, check_updates_with_details, create_download_client,
    list_installed, restart_plasmashell, update_component_with_client,
};

use crate::config::CliConfig;

mod exit_codes {
    pub const SUCCESS: i32 = 0;
    pub const PARTIAL_FAILURE: i32 = 1;
    pub const FATAL_ERROR: i32 = 2;
}

mod ansi {
    pub const BOLD: &str = "\x1b[1m";
    pub const RESET: &str = "\x1b[0m";

    #[inline]
    pub fn bold(s: &str) -> String {
        format!("{BOLD}{s}{RESET}")
    }
}

fn is_excluded(update: &AvailableUpdate, excluded: &[String]) -> bool {
    excluded
        .iter()
        .any(|e| e == &update.installed.directory_name || e == &update.installed.name)
}

fn is_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

fn escalate_with_sudo() -> Result<i32, libplasmoid_updater::Error> {
    let args: Vec<String> = std::env::args().collect();

    let status = std::process::Command::new("sudo")
        .args(&args)
        .status()
        .map_err(|e| libplasmoid_updater::Error::other(format!("failed to run sudo: {e}")))?;

    Ok(status.code().unwrap_or(exit_codes::FATAL_ERROR))
}

#[derive(Parser)]
#[command(name = "plasmoid-updater")]
#[command(about = "update kde plasma components from the kde store")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// operate on system-wide components (requires sudo)
    #[arg(long, global = true)]
    system: bool,

    /// automatically restart plasmashell after updates
    #[arg(long, global = true)]
    restart_plasma: bool,

    /// do not restart plasmashell after updates
    #[arg(long, global = true)]
    no_restart_plasma: bool,

    /// enable verbose output
    #[arg(short, long, global = true)]
    verbose: bool,

    /// output results as json
    #[arg(long, global = true)]
    json: bool,

    /// open configuration file in editor
    #[arg(long)]
    edit_config: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// check for available updates
    Check,
    /// list all installed components
    ListInstalled,
    /// update components
    Update {
        /// component name or directory to update
        component: Option<String>,

        /// update all components with available updates
        #[arg(long)]
        all: bool,
    },
}

fn main() {
    let cli = Cli::parse();

    if is_root() && !cli.system {
        output_error(&cli, "running as root requires --system flag");
        std::process::exit(exit_codes::FATAL_ERROR);
    }

    if cli.edit_config {
        if let Err(e) = CliConfig::edit_config() {
            output_error(&cli, &e.to_string());
            std::process::exit(exit_codes::FATAL_ERROR);
        }
        return;
    }

    let config = match CliConfig::load() {
        Ok(c) => c,
        Err(e) => {
            output_error(&cli, &format!("failed to load config: {e}"));
            std::process::exit(exit_codes::FATAL_ERROR);
        }
    };

    let verbosity = if cli.verbose {
        Verbosity::Verbose
    } else {
        config.verbosity
    };

    let result = match cli.command.as_ref().unwrap_or(&Commands::Check) {
        Commands::Check => cmd_check(&cli, &config, verbosity),
        Commands::ListInstalled => cmd_list_installed(&cli, verbosity),
        Commands::Update { component, all } => {
            if cli.system && !is_root() {
                match escalate_with_sudo() {
                    Ok(code) => std::process::exit(code),
                    Err(e) => {
                        output_error(&cli, &e.to_string());
                        std::process::exit(exit_codes::FATAL_ERROR);
                    }
                }
            }
            cmd_update(&cli, &config, component.as_deref(), *all, verbosity)
        }
    };

    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            output_error(&cli, &e.to_string());
            std::process::exit(exit_codes::FATAL_ERROR);
        }
    }
}

fn output_error(cli: &Cli, msg: &str) {
    if cli.json {
        let output: JsonOutput<()> = JsonOutput::err(msg);
        println!("{}", serde_json::to_string(&output).unwrap());
    } else {
        eprintln!("{} {msg}", ansi::bold("error:"));
    }
}

fn cmd_check(
    cli: &Cli,
    config: &CliConfig,
    verbosity: Verbosity,
) -> Result<i32, libplasmoid_updater::Error> {
    let result = if cli.system {
        check_updates_system_with_details(&config.inner)?
    } else {
        check_updates_with_details(&config.inner)?
    };

    if cli.json {
        let output = JsonOutput::ok(&result);
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(exit_codes::SUCCESS);
    }

    if verbosity == Verbosity::Verbose && result.has_issues() {
        print_check_issues(&result);
    }

    if result.updates.is_empty() {
        if verbosity != Verbosity::Quiet {
            println!("{} no updates available", ansi::bold("info:"));
        }
        return Ok(exit_codes::SUCCESS);
    }

    if verbosity == Verbosity::Quiet {
        println!("{}", result.updates.len());
    } else {
        println!(
            "{} {} update(s) available:",
            ansi::bold("info:"),
            result.updates.len()
        );
        println!();
        print_updates_table(&result.updates, verbosity);
    }

    Ok(exit_codes::SUCCESS)
}

fn cmd_list_installed(cli: &Cli, verbosity: Verbosity) -> Result<i32, libplasmoid_updater::Error> {
    let components = list_installed(cli.system)?;

    if cli.json {
        let output = JsonOutput::ok(&components);
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(exit_codes::SUCCESS);
    }

    if components.is_empty() {
        if verbosity != Verbosity::Quiet {
            println!("{} no components installed", ansi::bold("info:"));
        }
        return Ok(exit_codes::SUCCESS);
    }

    if verbosity == Verbosity::Quiet {
        println!("{}", components.len());
    } else {
        println!(
            "{} {} installed component(s):",
            ansi::bold("info:"),
            components.len()
        );
        println!();
        print_components_table(&components, verbosity);
    }

    Ok(exit_codes::SUCCESS)
}

fn cmd_update(
    cli: &Cli,
    config: &CliConfig,
    component: Option<&str>,
    all: bool,
    verbosity: Verbosity,
) -> Result<i32, libplasmoid_updater::Error> {
    let result = if cli.system {
        check_updates_system_with_details(&config.inner)?
    } else {
        check_updates_with_details(&config.inner)?
    };

    if verbosity == Verbosity::Verbose && result.has_issues() && !cli.json {
        print_check_issues(&result);
    }

    let updates = result.updates;

    if updates.is_empty() {
        if cli.json {
            let output = JsonOutput::ok(UpdateSummary::default());
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else if verbosity != Verbosity::Quiet {
            println!("{} no updates available", ansi::bold("info:"));
        }
        return Ok(exit_codes::SUCCESS);
    }

    let to_update: Vec<&AvailableUpdate> = if let Some(name) = component {
        updates
            .iter()
            .filter(|u| {
                u.installed.name.eq_ignore_ascii_case(name)
                    || u.installed.directory_name == name
                    || u.content_id.to_string() == name
            })
            .collect()
    } else if all || config.update_all_by_default {
        updates
            .iter()
            .filter(|u| !is_excluded(u, &config.excluded_packages))
            .collect()
    } else if cli.json {
        updates
            .iter()
            .filter(|u| !is_excluded(u, &config.excluded_packages))
            .collect()
    } else {
        select_updates_interactive(&updates, config, verbosity)?
    };

    if to_update.is_empty() {
        if component.is_some() {
            if cli.json {
                let output: JsonOutput<()> =
                    JsonOutput::err("component not found or no update available");
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                eprintln!(
                    "{} component not found or no update available",
                    ansi::bold("error:")
                );
            }
            return Ok(exit_codes::PARTIAL_FAILURE);
        }
        if cli.json {
            let output = JsonOutput::ok(UpdateSummary::default());
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else if verbosity != Verbosity::Quiet {
            println!("{} no updates to apply (all excluded)", ansi::bold("info:"));
        }
        return Ok(exit_codes::SUCCESS);
    }

    let needs_restart = to_update
        .iter()
        .any(|u| libplasmoid_updater::installer::requires_plasmashell_restart(&u.installed));

    let mut summary = UpdateSummary::default();
    let client = create_download_client();

    for update in &to_update {
        let name = update.installed.name.clone();
        if !cli.json && verbosity != Verbosity::Quiet {
            println!(
                "{} {} {} -> {}",
                ansi::bold("update:"),
                name,
                update.installed.version,
                update.latest_version
            );
        }

        match update_component_with_client(update, &client, false) {
            Ok(()) => {
                if !cli.json && verbosity != Verbosity::Quiet {
                    println!("{} {name}", ansi::bold("success:"));
                }
                summary.add_success(name);
            }
            Err(e) => {
                if !cli.json {
                    eprintln!("{} {name}: {e}", ansi::bold("failed:"));
                }
                summary.add_failure(name, e.to_string());
            }
        }
    }

    if cli.json {
        let output = JsonOutput::ok(&summary);
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!();
        print_summary(&summary, verbosity);
    }

    if needs_restart && !summary.succeeded.is_empty() && !cli.json {
        handle_restart(cli, config)?;
    }

    Ok(summary.exit_code())
}

fn print_updates_table(updates: &[AvailableUpdate], verbosity: Verbosity) {
    let name_width = name_width(updates, |u| &u.installed.name);

    if verbosity == Verbosity::Verbose {
        println!(
            "{:<name_width$}  {:>10}  {:>10}  {:>10}  {:>8}  TYPE",
            "NAME", "CURRENT", "AVAILABLE", "ID", "SIZE"
        );
        println!("{}", "-".repeat(name_width + 60));

        for u in updates {
            let size = u
                .download_size
                .map(format_size)
                .unwrap_or_else(|| "-".to_string());
            println!(
                "{:<name_width$}  {:>10}  {:>10}  {:>10}  {:>8}  {}",
                u.installed.name,
                u.installed.version,
                u.latest_version,
                u.content_id,
                size,
                u.installed.component_type
            );
        }
    } else {
        println!(
            "{:<name_width$}  {:>10}  {:>10}",
            "NAME", "CURRENT", "AVAILABLE"
        );
        println!("{}", "-".repeat(name_width + 24));

        for u in updates {
            println!(
                "{:<name_width$}  {:>10}  {:>10}",
                u.installed.name, u.installed.version, u.latest_version
            );
        }
    }
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;

    if bytes >= MB {
        format!("{:.1}M", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0}K", bytes as f64 / KB as f64)
    } else {
        format!("{}B", bytes)
    }
}

fn name_width<T>(items: &[T], name: impl Fn(&T) -> &str) -> usize {
    items
        .iter()
        .map(|i| name(i).len())
        .max()
        .unwrap_or(10)
        .max(10)
}

fn print_components_table(components: &[InstalledComponent], verbosity: Verbosity) {
    let name_width = name_width(components, |c| &c.name);

    if verbosity == Verbosity::Verbose {
        println!("{:<name_width$}  {:>10}  TYPE", "NAME", "VERSION");
        println!("{}", "-".repeat(name_width + 30));

        for c in components {
            println!(
                "{:<name_width$}  {:>10}  {}",
                c.name, c.version, c.component_type
            );
        }
    } else {
        println!("{:<name_width$}  {:>10}", "NAME", "VERSION");
        println!("{}", "-".repeat(name_width + 12));

        for c in components {
            println!("{:<name_width$}  {:>10}", c.name, c.version);
        }
    }
}

fn print_summary(summary: &UpdateSummary, verbosity: Verbosity) {
    if verbosity == Verbosity::Quiet {
        return;
    }

    if !summary.succeeded.is_empty() || !summary.failed.is_empty() {
        println!(
            "{} {} succeeded, {} failed, {} skipped",
            ansi::bold("summary:"),
            summary.succeeded.len(),
            summary.failed.len(),
            summary.skipped.len()
        );
    }

    if !summary.failed.is_empty() && verbosity == Verbosity::Verbose {
        println!();
        println!("{}", ansi::bold("failures:"));
        for (name, reason) in &summary.failed {
            println!("  - {name}: {reason}");
        }
    }
}

fn print_check_issues(result: &CheckResult) {
    if !result.unresolved.is_empty() {
        println!(
            "{} {} component(s) could not be matched to kde store:",
            ansi::bold("info:"),
            result.unresolved.len()
        );
        for (name, reason) in &result.unresolved {
            println!("  - {name}: {reason}");
        }
        println!();
    }

    if !result.check_failures.is_empty() {
        println!(
            "{} {} component(s) failed during check:",
            ansi::bold("warn:"),
            result.check_failures.len()
        );
        for (name, reason) in &result.check_failures {
            println!("  - {name}: {reason}");
        }
        println!();
    }
}

fn select_updates_interactive<'a>(
    updates: &'a [AvailableUpdate],
    config: &CliConfig,
    verbosity: Verbosity,
) -> Result<Vec<&'a AvailableUpdate>, libplasmoid_updater::Error> {
    if !atty::is(atty::Stream::Stdin) {
        if verbosity != Verbosity::Quiet {
            println!(
                "{} {} update(s) available:",
                ansi::bold("info:"),
                updates.len()
            );
            println!();
            print_updates_table(updates, verbosity);
            println!();
            println!("run with --all to update all, or specify a component name");
        }
        return Ok(vec![]);
    }

    let available: Vec<_> = updates
        .iter()
        .filter(|u| !is_excluded(u, &config.excluded_packages))
        .collect();

    if available.is_empty() {
        if verbosity != Verbosity::Quiet {
            println!("{} no updates to apply (all excluded)", ansi::bold("info:"));
        }
        return Ok(vec![]);
    }

    println!(
        "{} {} update(s) available:",
        ansi::bold("info:"),
        available.len()
    );
    println!();

    let name_width = available
        .iter()
        .map(|u| u.installed.name.len())
        .max()
        .unwrap_or(10)
        .max(10);

    println!(
        "  #  {:<name_width$}  {:>10}  {:>10}",
        "NAME", "CURRENT", "AVAILABLE"
    );
    println!("     {}", "-".repeat(name_width + 24));

    for (i, u) in available.iter().enumerate() {
        println!(
            "{:>3}  {:<name_width$}  {:>10}  {:>10}",
            i + 1,
            u.installed.name,
            u.installed.version,
            u.latest_version
        );
    }

    println!();
    print!(
        "{} enter numbers to update (e.g. 1,3,5 or 1-3 or 'all'), or 'q' to quit: ",
        ansi::bold("select:")
    );
    io::stdout().flush().ok();

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return Ok(vec![]);
    }

    let input = input.trim().to_lowercase();
    if input.is_empty() || input == "q" || input == "quit" {
        return Ok(vec![]);
    }

    if input == "all" || input == "a" {
        return Ok(available);
    }

    let mut selected_indices = std::collections::HashSet::new();

    for part in input.split(',') {
        let part = part.trim();
        if part.contains('-') {
            let bounds: Vec<&str> = part.split('-').collect();
            if bounds.len() == 2
                && let (Ok(start), Ok(end)) =
                    (bounds[0].parse::<usize>(), bounds[1].parse::<usize>())
            {
                for i in start..=end {
                    if i >= 1 && i <= available.len() {
                        selected_indices.insert(i - 1);
                    }
                }
            }
        } else if let Ok(n) = part.parse::<usize>()
            && n >= 1
            && n <= available.len()
        {
            selected_indices.insert(n - 1);
        }
    }

    let mut indices: Vec<_> = selected_indices.into_iter().collect();
    indices.sort_unstable();

    Ok(indices.into_iter().map(|i| available[i]).collect())
}

fn handle_restart(cli: &Cli, config: &CliConfig) -> Result<(), libplasmoid_updater::Error> {
    if cli.no_restart_plasma {
        return Ok(());
    }

    if cli.restart_plasma {
        println!("{} restarting plasmashell...", ansi::bold("info:"));
        restart_plasmashell()?;
        println!("{} plasmashell restarted", ansi::bold("info:"));
        return Ok(());
    }

    if config.prompt_restart && atty::is(atty::Stream::Stdin) {
        print!("{} restart plasmashell now? [y/N] ", ansi::bold("prompt:"));
        io::stdout().flush().ok();

        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_ok() {
            let answer = input.trim().to_lowercase();
            if answer == "y" || answer == "yes" {
                println!("{} restarting plasmashell...", ansi::bold("info:"));
                restart_plasmashell()?;
                println!("{} plasmashell restarted", ansi::bold("info:"));
            }
        }
    }

    Ok(())
}
