# libplasmoid-updater

A Rust library for updating KDE Plasma 6 components from the KDE Store.

> [!IMPORTANT]
> The core logic (a combination of **Apdatifier** and **KDE Discover**) was ported to Rust with AI assistance (Claude Code and GH Copilot). The code was audited and tested by me. Feel free to contribute with an issue or PR.

## Requirements

Runtime dependencies:
- `bsdtar` – Archive extraction
- `kpackagetool6` – KDE package installation
- `systemctl` – plasmashell restart, when a restart is requested
- `sudo` – only for `--system` operations

## Features

- **Component Discovery** – Detects installed Plasmoids and other Plasma components
- **Multithreaded** update checking and installation via Rayon
- **Automatic Backups** with rollback on failure, kept under `$XDG_CACHE_HOME/plasmoid-updater/backups/` (the 5 most recent per component type are kept)
- **Registry Integration** – Maintains KNewStuff registry compatibility
- **Sleep Inhibition** – Prevents system sleep during installs (logind DBus or systemd-inhibit)
- **Plasma 6 Support** – Linux and KDE Plasma 6 only

## Supported Components

Plasma Widgets, Wallpaper Plugins, KWin Effects/Scripts/Switchers, Global Themes, Plasma Styles, Color Schemes, Splash Screens, SDDM Themes, Icon Themes, Wallpapers, and Aurorae Decorations.

## API

### Functions

| function | description |
| --- | --- |
| `check(&Config) -> Result<CheckResult>` | scan for available updates to installed KDE components |
| `update(&Config, &dyn Reporter) -> Result<UpdateResult>` | install every non-excluded available update |
| `install(&[AvailableUpdate], &Config, &dyn Reporter) -> Result<UpdateResult>` | install a caller-chosen selection exactly as given (exclusions apply once, in `check`; an empty selection changes nothing) |
| `restart_plasmashell() -> Result<()>` | restart plasmashell via systemd; the caller decides when |
| `get_installed(&Config) -> Result<Vec<InstalledComponent>>` | return all installed KDE components; no network calls |

### Reporter

Progress leaves the core only through a `Reporter` hook that does nothing by default (plain `&()` works; a named type is an empty `impl Reporter for T {}`). `Event::Checking` is emitted by `update()` (not `install()`, whose caller drives the check itself); `Event::Downloading { name, id, bytes, total }`, `Event::Installed { name, id }` and `Event::Failed { name, id, message }` are emitted by both during the install. `Event` is `#[non_exhaustive]`. Each per-component event carries `name`, the display name to render, and `id`, the component's installed path — unique within a run, so reporters can key progress rows on it.

### Errors

Two levels: `Error` means the run could not start or finish (unsupported OS, no Plasma, already running, missing tool, network, rate limit, API status, parse, io with path, config); a per-component problem is a `Failure { component, kind, message }` row inside a successful `UpdateResult`, with `FailureKind` one of `NoMatchingDownload`, `Download`, `Checksum`, `Extract`, `Install`, `RestoreFailed`, `Unverified`. `UpdateResult` holds `succeeded` (verified installs), `failed` and `skipped` (excluded and unresolved names); `needs_restart()` tells whether a plasmashell restart is warranted.

### Config

Built with a builder table via `Config::new()`:

| method | description |
| --- | --- |
| `with_system(bool)` | scan system-wide components in `/usr/share` (requires root) |
| `with_excluded_packages(Vec<String>)` | skip these packages (by dir name or display name) |
| `with_excluded_patterns(Vec<String>)` | skip packages whose name or dir matches these regexes (invalid ones are dropped with a warning) |
| `with_widgets_id_table(HashMap<String, u64>)` | override fallback dir-to-content-ID mapping |
| `with_store_url(impl Into<String>)` | OCS base URL every store request targets (default `https://api.kde-look.org/ocs/v1`) |
| `with_threads(usize)` | max parallel install threads (default: logical CPU count) |
| `with_skip_plasma_detection(bool)` | skip KDE environment check (for CI/testing) |
| `with_inhibit_idle(bool)` | inhibit system sleep during installs (default: `true`) |
| `Config::parse_widgets_id(&str)` | parse text in the `widgets-id` format into a fallback table for `with_widgets_id_table` |

### Types

`CheckResult` returned by `check()`:
- `available_updates: Vec<AvailableUpdate>` — empty when the store has nothing newer (not an error); `diagnostics` may still list components that could not be checked, so `has_updates()` answers "anything to install" and `is_empty()` answers "nothing found at all" — no updates, no diagnostics and nothing skipped
- `skipped: Vec<String>` — names of components excluded by the config's exclusion rules; exclusions apply once, in `check()`, so excluded components never appear in `available_updates` (and `install()` installs its selection exactly as given)
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

### Failure rows

A component that failed inside an otherwise successful run is a `Failure`:
- fields: `component: InstalledComponent`, `kind: FailureKind`, `message: String`
- an install whose post-install version cannot be verified is a `Failure` of kind `Unverified` and still counts toward `needs_restart()`

## Cargo Features

Default features: `cli`, `inhibit`.

| feature | description |
| --- | --- |
| `cli` | Terminal kit: `cli::ProgressReporter` (stderr progress; uncoloured and non-animated under `NO_COLOR`, `TERM=dumb` or a non-TTY stderr), `cli::select` (multi-select prompt; an empty list returns empty, a non-empty one needs a TTY stdin), `cli::print_failures` (failure table on stderr), and the check/installed tables as strings the caller prints (`updates_table`, `components_table`). Pulls in comfy-table, bytesize, inquire. |
| `inhibit` | Inhibit system sleep/shutdown via logind DBus during installs. Pulls in zbus. Without this feature the library falls back to spawning `systemd-inhibit` as a subprocess. |

To use the library without terminal dependencies:

```toml
[dependencies]
libplasmoid-updater = { version = "0.2.1", default-features = false }
```

## CLI Tool

For a reference CLI implementation, see [`plasmoid-updater`](https://crates.io/crates/plasmoid-updater).

## License

Licensed under [GPL-3.0-or-later](https://www.gnu.org/licenses/gpl-3.0.html).
