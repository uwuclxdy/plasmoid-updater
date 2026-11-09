# libplasmoid-updater

A Rust library for updating KDE Plasma 6 components from the KDE Store.

> [!IMPORTANT]
> The core logic (a combination of **Apdatifier** and **KDE Discover**) was ported to Rust with AI assistance (Claude Code and GH Copilot). The code was audited and tested by me. Feel free to contribute with an issue or PR.

## Requirements

Runtime dependencies:
- `bsdtar` – Archive extraction, for `update` and `install` only (`check` needs no extractor)
- `kpackagetool6` – KDE package installation
- `systemctl` and `busctl` – plasmashell restart, when a restart is requested
- `sudo` – only for `--system` operations
- `sh` and `stty` – for `cli::select` (the `cli` feature), while its prompt is open

## Features

- **Component Discovery** – Detects installed Plasmoids and other Plasma components
- **Multithreaded** update checking and installation via Rayon
- **Automatic Backups** with rollback on failure, kept under `$XDG_CACHE_HOME/plasmoid-updater/backups/` for user components and `/var/lib/plasmoid-updater/backups/` (root-owned) for `--system` ones (the 5 most recent per component type are kept)
- **Registry Integration** – Maintains KNewStuff registry compatibility
- **Sleep Inhibition** – Prevents system sleep during installs (logind DBus or systemd-inhibit)
- **Plasma 6 Support** – Linux and KDE Plasma 6 only

## Supported Components

Plasma Widgets, Wallpaper Plugins, KWin Effects/Scripts/Switchers, Global Themes, Plasma Styles, Color Schemes, Splash Screens, SDDM Themes, Icon Themes, Cursor Themes, Wallpapers, Plasma Comic Plugins, and Aurorae Decorations.

## API

### Functions

| function | description |
| --- | --- |
| `check(&Config, &dyn Reporter) -> Result<CheckResult>` | scan for available updates to installed KDE components |
| `update(&Config, &dyn Reporter) -> Result<UpdateResult>` | install every non-excluded available update |
| `install(&[AvailableUpdate], &Config, &dyn Reporter) -> Result<UpdateResult>` | install a caller-chosen selection exactly as given (exclusions apply in `check` and `update`; an empty selection takes the run lock and repairs what a killed run left stranded, or returns an empty result when another run already holds the lock) |
| `needs_repair(&Config) -> Result<bool>` | whether a killed run left a stranded component or a dead `-new`/`-old` copy for `install` to put back or clean up; reads the disk only, so a caller elevating a `--system` run asks it before an empty `install`; while another run holds the update lock it answers `false`, since an empty `install` then returns without repairing |
| `restart_plasmashell() -> Result<()>` | restart the systemd user service that runs plasmashell (with none running, start `plasma-plasmashell.service`), then wait for a new plasmashell; fails when that service cannot be identified or no new plasmashell appears; the caller decides when; a `busctl` or `systemctl` call that succeeds but prints non-blank stderr has it logged through the `log` facade as one warning, `<tool>: <stderr as one line>`, and an identical warning logs once per `restart_plasmashell` call, so a host sees it only through an installed `log` logger |
| `get_installed(&Config) -> Result<Vec<InstalledComponent>>` | return all installed KDE components; no network calls |

### Reporter

Progress leaves the core only through a `Reporter` hook that does nothing by default (plain `&()` works; a named type is an empty `impl Reporter for T {}`). `Event::Checking { components }` is emitted once by `check()` and by `update()`, after the scan has counted the installed components the run will check, and `Event::Checked` closes that part on every path, a store error or a panic included (`install()` emits neither; its caller drives the check itself and passes the reporter to `check()`). `Event::Downloading { name, id, bytes, total }`, `Event::Installed { name, id, version }` and `Event::Failed { name, id, message }` are emitted during the install; `version` is the version the install verified, the same one `UpdateResult::succeeded` records. `Event` is `#[non_exhaustive]`, and so are its field-carrying variants: match with a wildcard arm and `..`, so a later stage or field stays a minor change. Each per-component event carries `name`, the display name to render, and `id`, the component's installed path — unique within a run except where two component types share one directory under `--system`; reporters key progress rows on it.

### Errors

Two levels: `Error` means the run could not start or finish (unsupported OS, no Plasma, already running, missing tool, an external command that exited nonzero, network, rate limit, API status, parse, io with path, config, a restart that cannot be done safely or does not bring up a new plasmashell); `Error` and its struct-like variants are `#[non_exhaustive]`, so match with a wildcard arm and `..`. A per-component problem is a `Failure` row (fields `component`, `kind`, `message`) inside a successful `UpdateResult`, with `FailureKind` one of `NoMatchingDownload`, `Download`, `Checksum`, `Extract`, `Install`, `RestoreFailed`, `Unverified`, `Registry`. `UpdateResult` holds `succeeded` (verified installs), `failed` and `skipped` (excluded and unresolved names); `needs_restart()` tells whether a plasmashell restart is warranted.

### Config

Built with a builder table via `Config::new()`:

| method | description |
| --- | --- |
| `with_system(bool)` | scan system-wide components in `/usr/share` (requires root) |
| `with_excluded_packages(Vec<String>)` | skip these packages (by dir name or display name) |
| `with_excluded_patterns(Vec<String>)` | skip packages whose name or dir matches these regexes (invalid ones are dropped with a warning) |
| `with_widgets_id_table(HashMap<String, u64>)` | override fallback dir-to-content-ID mapping |
| `with_api_url(impl Into<String>)` | OCS base URL every store request targets (default `https://api.kde-look.org/ocs/v1`) |
| `with_threads(usize)` | max parallel install threads (default: logical CPU count) |
| `with_skip_plasma_detection(bool)` | skip KDE environment check (for CI/testing) |
| `with_inhibit_idle(bool)` | inhibit system sleep during installs (default: `true`) |
| `excludes(&InstalledComponent) -> bool` | whether the exclusion rules above exclude a component, by the same rules `check()` and `update()` apply; an invalid pattern matches nothing and logs no warning |
| `Config::parse_widgets_id(&str)` | parse text in the `widgets-id` format into a fallback table for `with_widgets_id_table` |

### Types

`CheckResult`, `UpdateResult`, `InstalledComponent`, `AvailableUpdate`, `Failure`, `Diagnostic`, `Config` and `ComponentType` are `#[non_exhaustive]`: read their fields and match `ComponentType` with a wildcard arm, and never build one by struct literal from outside the crate. `CheckResult` and `UpdateResult` start from `Default::default()` and are filled by field assignment (`result.skipped.push(..)`); `InstalledComponent` starts from `InstalledComponent::new(name, directory_name, component_type, path)` and takes `with_version`, `with_system` and `with_release_date`; `AvailableUpdate` starts from `AvailableUpdate::builder(installed, content_id, latest_version)` — the version is required there — and takes the named setters `download_url`, `release_date`, `checksum` and `download_size`; `Config` starts from `Config::new()` (or `Default`, which holds the same values) and takes its `with_*` methods, each consuming `self`. `Failure` rows are produced by `check()` (in `CheckResult::unavailable`), `update()` and `install()`; `Diagnostic` values only by `check()`. `update()` returns no diagnostics and names every component it could not check in `skipped`; `install()` checks nothing, so its `skipped` is empty. Read these fields, never build them.

`CheckResult` returned by `check()`:
- `available_updates: Vec<AvailableUpdate>` — empty when the store has nothing newer (not an error); `diagnostics` may still list components that could not be checked, so `has_updates()` answers "anything to install" and `is_empty()` answers "nothing found at all" — no updates, no diagnostics and nothing skipped
- `skipped: Vec<String>` — names of components the run did not attempt: excluded by the config's exclusion rules, and in a `--system` run components with no matching store entry or a store entry of a different component type; an excluded component never appears in `available_updates` (exclusions apply in `check()` and `update()`; `install()` installs its selection exactly as given); display names are not unique, so a name here does not say which component was skipped
- `unmatched: Vec<InstalledComponent>` — the components behind the store-entry skips in `skipped` (a `--system` run only): no store entry matches, or the matching entry is another component type; an excluded component is never listed here. Identify one by `path` and `component_type`: an icon theme and a cursor theme can share one directory
- `diagnostics: Vec<Diagnostic>` — each with the component's display `name`, its install `path` (the same value as `InstalledComponent::path` and the reporter's event `id`; match a component by it and by `name`, never by name alone; a component a killed run stranded is named by the path it is restored to), the `reason`, and the installed version, store version and content id when known
- `unavailable: Vec<Failure>` — components whose store entry offers no download link for its version, each a `NoMatchingDownload` failure row to report the way `update()` does; the same components are also listed in `diagnostics`
- `has_updates() -> bool`, `update_count() -> usize`, `is_empty() -> bool`
- `unavailable_diagnostics() -> impl Iterator<Item = &Diagnostic>` — the `diagnostics` entries carrying the no-download reason, one per `unavailable` row, so a caller reporting those as failures keeps them out of `skipped` and out of the plain diagnostic output

`Diagnostic` : a component that could not be checked:
- fields: `name`, `reason`, `installed_version: Option<String>`, `available_version: Option<String>`, `content_id: Option<u64>`

`ComponentType` : enum of all supported KDE component kinds:
- variants: `PlasmaWidget`, `WallpaperPlugin`, `KWinEffect`, `KWinScript`, `KWinSwitcher`, `GlobalTheme`, `PlasmaStyle`, `AuroraeDecoration`, `ColorScheme`, `SplashScreen`, `SddmTheme`, `IconTheme`, `Wallpaper`, `CursorTheme`, `ComicPlugin`
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
| `cli` | Terminal kit: `cli::styled(Stream)` (whether a stream takes styling, with the rule clap's `--help` follows), `cli::ProgressReporter` (stderr progress; colour follows `cli::styled`, animation additionally needs a terminal stderr; one reporter draws at a time, a settled one starts a fresh block below later output, and on a terminal each redrawn line is clipped to the terminal width so a redraw never miscounts its rows), `cli::print_above` (writes a log line above the live progress block and redraws its rows intact, or plainly on stderr when none is live; safe from any thread), `cli::select` (multi-select prompt, styled by the same predicate; an empty list returns empty, a non-empty one needs a TTY stdin, and Ctrl-C raises SIGINT in the calling process so a caller that handles it gets an empty selection back while a killed run's terminal is restored by a watchdog process; needs `sh` and `stty` while the prompt is open), `cli::print_failures` (failure table on stderr), `cli::update_summary` (the `updated 1 · failed 1 · skipped 2` line, hiding zero counts), and the check/installed tables as strings the caller prints (`updates_table`, `components_table`; the caller names the stream it prints on). Every colour comes from one palette: 24-bit RGB on a `COLORTERM=truecolor` terminal, the xterm-256 index otherwise, and plain text when `cli::styled` is false. Pulls in comfy-table, bytesize, inquire, anstream. |
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
