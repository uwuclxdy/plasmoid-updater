# libplasmoid-updater

A Rust library for managing KDE Plasma 6 components from the KDE Store.

> [!IMPORTANT]
> The core logic (a combination of **Apdatifier** and **KDE Discover**) was ported to Rust with AI assistance (Claude Code and GH Copilot). The code was audited and tested by me. Feel free to contribute with an issue or PR.

## Requirements

Runtime dependencies:
- `bsdtar` – Archive extraction
- `kpackagetool6` – KDE package installation

## Features

- **Component Discovery** – Detects installed Plasmoids and other Plasma components
- **Multithreaded** update checking and installation via Rayon
- **Automatic Backups** with rollback on failure
- **Registry Integration** – Maintains KNewStuff registry compatibility
- **Sleep Inhibition** – Prevents system sleep during installs (logind DBus or systemd-inhibit)
- **Plasma 6 Support** – Compatible with Plasma 6.0 and later

## Supported Components

Plasma Widgets, Wallpaper Plugins, KWin Effects/Scripts/Switchers, Global Themes, Plasma Styles, Color Schemes, Splash Screens, SDDM Themes, Icon Themes, Wallpapers, and Aurorae Decorations.

## API

### Functions

| function | description |
| --- | --- |
| `check(&Config) -> Result<CheckResult>` | scan for available updates to installed KDE components |
| `update(&Config) -> Result<UpdateResult>` | apply available updates (interactive or automatic) |
| `show_installed(&Config) -> Result<()>` | print a table of all installed KDE components (`cli`) |
| `get_installed(&Config) -> Result<Vec<InstalledComponent>>` | return all installed KDE components; no network calls |
| `install_update(&AvailableUpdate, &Config) -> Result<()>` | install a single component update with backup/rollback |

### Config

Built with a builder pattern via `Config::new()`:

| method | description |
| --- | --- |
| `with_system(bool)` | scan system-wide components in `/usr/share` (requires root) |
| `with_excluded_packages(Vec<String>)` | skip these packages (by dir name or display name) |
| `with_widgets_id_table(HashMap<String, u64>)` | override fallback dir-to-content-ID mapping |
| `with_restart(RestartBehavior)` | plasmashell restart behavior after updates |
| `with_auto_confirm(bool)` | skip interactive prompts, apply all updates (`cli`) |
| `with_threads(usize)` | max parallel install threads (default: logical CPU count) |
| `with_skip_plasma_detection(bool)` | skip KDE environment check (for CI/testing) |
| `with_inhibit_idle(bool)` | inhibit system sleep during installs (default: `true`) |
| `Config::parse_widgets_id(&str)` | parse a `widgets-id` file into the fallback table format |

### Types

`RestartBehavior` : `Never` (default) | `Always` | `Prompt`

`CheckResult` returned by `check()`:
- `available_updates: Vec<AvailableUpdate>`
- `diagnostics: Vec<Diagnostic>`
- `has_updates() -> bool`, `update_count() -> usize`, `is_empty() -> bool`

`Diagnostic` : a component that could not be checked:
- fields: `name`, `reason`, `installed_version: Option<String>`, `available_version: Option<String>`, `content_id: Option<u64>`

`ComponentType` : enum of all supported KDE component kinds:
- variants: `PlasmaWidget`, `WallpaperPlugin`, `KWinEffect`, `KWinScript`, `KWinSwitcher`, `GlobalTheme`, `PlasmaStyle`, `AuroraeDecoration`, `ColorScheme`, `SplashScreen`, `SddmTheme`, `IconTheme`, `Wallpaper`
- `user_path() -> PathBuf`, `system_path() -> PathBuf`, `all() -> &[ComponentType]`, `all_user() -> &[ComponentType]`

`InstalledComponent` : a KDE component on the local system:
- fields: `name`, `directory_name`, `version`, `component_type: ComponentType`, `path: PathBuf`, `is_system: bool`, `release_date`

`AvailableUpdate` : an update with download metadata:
- fields: `installed: InstalledComponent`, `content_id: u64`, `latest_version`, `download_url`, `store_url`, `release_date`, `checksum: Option<String>`, `download_size: Option<u64>`

`FailedUpdate` : a component that failed to update:
- fields: `name`, `error`

`UnverifiedUpdate` : installed but post-install version could not be confirmed:
- fields: `name`, `expected_version`, `actual_version: Option<String>`

`UpdateResult` returned by `update()`:
- `succeeded: Vec<String>`, `failed: Vec<FailedUpdate>`, `skipped: Vec<String>`, `unverified: Vec<UnverifiedUpdate>`
- `has_failures() -> bool`, `is_empty() -> bool`, `success_count() -> usize`, `failure_count() -> usize`
- `print_summary()`, `print_error_table()` (requires `cli`)

`Error` : library error type with variants for OS, network, XML, IO, install, checksum, and more. `Result<T>` aliases `Result<T, Error>`.

## Cargo Features

Default features: `cli`, `inhibit`.

| feature | description |
| --- | --- |
| `cli` | Terminal output (spinner, tables, interactive selection), `show_installed()`, `UpdateResult::print_summary()`, and `UpdateResult::print_error_table()`. Pulls in indicatif, comfy-table, bytesize, inquire, is-terminal, terminal\_size. |
| `inhibit` | Inhibit system sleep/shutdown via logind DBus during installs. Pulls in zbus. Without this feature the library falls back to spawning `systemd-inhibit` as a subprocess. |
| `debug` | Print request count after `update()`. |

To use the library without terminal dependencies:

```toml
[dependencies]
libplasmoid-updater = { version = "0.1", default-features = false }
```

## CLI Tool

For a reference CLI implementation, see [`plasmoid-updater`](https://crates.io/crates/plasmoid-updater).

## License

Licensed under [GPL-3.0-or-later](https://www.gnu.org/licenses/gpl-3.0.html).
