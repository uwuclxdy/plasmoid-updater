# libplasmoid-updater

A Rust library for managing KDE Plasma 6 components from the KDE Store.

> [!IMPORTANT]
> The core logic (combination of **Apdatifier** and **KDE Discover** + custom fixes) was ported to Rust in majority with
> AI assistance. I audited the generated code, refactored, fixed obvious bugs, and tested it myself. Open to contributions on GitHub.

## Requirements

Runtime dependencies:
- `bsdtar` – Archive extraction
- `kpackagetool6` – KDE package installation

## Features

- **Component Discovery** – Detects installed Plasmoids and other Plasma components
- **Update Check** – Via KDE Store API (OCS)
- **Safe Installation** – Installs updates with automatic backup and rollback on failure
- **Registry Integration** – Maintains KNewStuff registry compatibility
- **Plasma 6 Support** – Compatible with Plasma 6.0 and later

## Supported Components

Plasma Widgets, Wallpaper Plugins, KWin Effects/Scripts, Global Themes, Plasma Styles, Color Schemes, Splash Screens, SDDM Themes, and Aurorae Decorations.

## API

### Functions

| function                                                    | description                                             |
| ----------------------------------------------------------- | ------------------------------------------------------- |
| `check(&Config) -> Result<CheckResult, CheckError>`         | scan for available updates to installed KDE components  |
| `update(&Config) -> Result<UpdateResult, UpdateError>`      | apply available updates (interactive or automatic)      |
| `show_installed(&Config) -> Result<()>`                     | print a table of all installed KDE components (`cli`)   |
| `get_installed(&Config) -> Result<Vec<InstalledComponent>>` | returns all installed KDE components; no network calls  |
| `install_update(&AvailableUpdate) -> Result<()>`            | installs a single component update with backup/rollback |

### Config

Built with a builder pattern via `Config::new()`:

| method                                        | description                                                 |
| --------------------------------------------- | ----------------------------------------------------------- |
| `with_system(bool)`                           | scan system-wide components in `/usr/share` (requires root) |
| `with_excluded_packages(Vec<String>)`         | skip these packages (by dir name or display name)           |
| `with_widgets_id_table(HashMap<String, u64>)` | override fallback dir→content-ID mapping                    |
| `with_restart(RestartBehavior)`               | plasmashell restart behavior after updates                  |
| `with_yes(bool)`                              | auto-confirm without interactive prompts                    |
| `Config::parse_widgets_id(&str)`              | parse a `widgets-id` file into the fallback table format    |

### Types

**`RestartBehavior`** — `Never` (default) | `Always` | `Prompt`

**`CheckResult`** — returned by `check()`:
- `available_updates: Vec<AvailableUpdateInfo>` — list of updates found
- `diagnostics: Vec<(String, String)>` — components that could not be checked (name, reason)
- `has_updates() -> bool`, `update_count() -> usize`

**`AvailableUpdateInfo`** — fields: `name`, `directory_name`, `current_version`, `available_version`, `component_type`, `content_id`, `download_size: Option<u64>`

**`ComponentType`** — enum of all supported KDE component kinds:
- variants: `PlasmaWidget`, `WallpaperPlugin`, `KWinEffect`, `KWinScript`, `KWinSwitcher`, `GlobalTheme`, `PlasmaStyle`, `AuroraeDecoration`, `ColorScheme`, `SplashScreen`, `SddmTheme`, `IconTheme`, `Wallpaper`
- `user_path() -> PathBuf`, `system_path() -> PathBuf`, `all() -> &[ComponentType]`, `all_user() -> &[ComponentType]`

**`InstalledComponent`** — a KDE component installed on the local system:
- fields: `name`, `directory_name`, `version`, `component_type: ComponentType`, `path: PathBuf`, `is_system: bool`, `release_date`

**`AvailableUpdate`** — an available update with download metadata:
- fields: `installed: InstalledComponent`, `content_id: u64`, `latest_version`, `download_url`, `store_url`, `release_date`, `checksum: Option<String>`, `download_size: Option<u64>`

**`UpdateResult`** — returned by `update()`:
- `succeeded: Vec<String>`, `failed: Vec<(String, String)>`, `skipped: Vec<String>`
- `has_failures() -> bool`, `is_empty() -> bool`
- `print_summary()`, `print_error_table()` — requires `cli` feature

**`CheckError`** — `UnsupportedOS(String)` | `NotKDE` | `Other(Box<dyn Error>)`

**`UpdateError`** — `Check(CheckError)` | `Other(Box<dyn Error>)`

**`Error`** — base library error type; `Result<T>` aliases `Result<T, Error>`

## Cargo Features

This library provides optional features to reduce compile times and dependencies for different use cases:

- **`cli`** (optional) – Enables CLI-specific functionality including terminal output formatting, progress indicators, and interactive prompts. Required for `show_installed()`, `UpdateResult::print_summary()`, and `UpdateResult::print_error_table()`. Also enables terminal output (spinner, tables, interactive selection) in `check()` and `update()`.

By default, no features are enabled. The core library functionality (update checking and installation) is always available.

### Using the CLI feature

```toml
[dependencies]
libplasmoid-updater = { version = "0.1", features = ["cli"] }
```

## CLI Tool

As a reference CLI-tool implementation, see [`plasmoid-updater`](https://crates.io/crates/plasmoid-updater) binary crate.

## License

Licensed under MIT OR Apache-2.0.
