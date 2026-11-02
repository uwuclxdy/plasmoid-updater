# plasmoid-updater

Updates KDE Plasma 6 components from KDE Store using a combination of Apdatifier's and KDE Discover's logic.

> [!IMPORTANT]
> The core logic (a combination of **Apdatifier** and **KDE Discover**) was ported to Rust with AI assistance (Claude Code and GH Copilot). The code was audited and tested by me. Feel free to contribute with an issue or PR.

## Requirements

- bsdtar, kpackagetool6 (`systemctl` too, for the optional plasmashell restart)
- [Rust](https://rust-lang.org/tools/install/)

## Installation

**Using Cargo (Recommended)**
```
cargo install plasmoid-updater
```

**From source using Cargo:**

```
git clone https://github.com/uwuclxdy/plasmoid-updater
cd plasmoid-updater
cargo install --path plasmoid-updater/
```

**Binary release**

Precompiled binary available on [GitHub Releases](https://github.com/uwuclxdy/plasmoid-updater/releases/latest).

## Usage

```
$ plasmoid-updater --help
update kde plasma components from the kde store

Usage: plasmoid-updater [OPTIONS] [COMMAND]

Commands:
  check           check for available updates
  list-installed  list all installed components
  update          update components
  completions     print a shell completion script

Options:
      --system                 operate on system-wide components (needs sudo)
      --edit-config            open configuration file in editor
      --skip-plasma-detection  skip KDE Plasma detection
  -q, --quiet                  errors only on stderr
  -v, --verbose...             more diagnostics on stderr (repeat for debug)
  -h, --help                   Print help
  -V, --version                Print version
```

```
$ plasmoid-updater update --help
update components

Usage: plasmoid-updater update [OPTIONS] [COMPONENT]

Arguments:
  [COMPONENT]  component name or directory to update

Options:
      --restart-plasma         automatically restart plasmashell
      --no-restart-plasma      do not restart plasmashell
  -y, --yes                    automatically confirm all updates
      --dry-run                list what would be installed and change nothing
      --ignore <NAMES>         names to skip for this run (comma-separated or repeatable)
      --system                 operate on system-wide components (needs sudo)
      --skip-plasma-detection  skip KDE Plasma detection
  -q, --quiet                  errors only on stderr
  -v, --verbose...             more diagnostics on stderr (repeat for debug)
  -h, --help                   Print help

```

`update --dry-run` prints the same table `check` prints — name, current version, available version, store ID, size and type — and touches neither the installed files nor the KDE Store registry. It matches `--ignore` and the config exclusions exactly as a real `update` does. Like `check`, it exits 1 when a component could not be checked; a named dry run answers only for the component it names, exactly as a named `update` does.

`--ignore` works on both `check` and `update` and skips the named components for that run only, on top of the config exclusions. Example: `plasmoid-updater update --ignore weather,clock`.

Running `plasmoid-updater` without a subcommand is the same as `update`. On a terminal, `update` opens an interactive selection menu unless `-y`, `assume_yes`, `update_all_by_default` or a COMPONENT is given; without a TTY it applies all non-excluded updates. `update [COMPONENT]` matches a component by its display name or directory name — a store ID matches nothing.

By default a run restarts nothing. After updates to widgets, plasma styles, global themes, splash screens or KWin switchers, `--restart-plasma` restarts plasmashell, `prompt_restart = true` in the config asks first (on a terminal only), and `--no-restart-plasma` skips it (when both flags are given, `--restart-plasma` wins).

Components that fail to update don't stop the run: failures are summarized at the end, and the run exits 1 when any component failed or could not be checked. An install whose post-install version cannot be verified counts as a failure too, so it exits 1 as well.

## Exit codes

| Code | Meaning |
| ---- | ------- |
| 0 | success |
| 1 | at least one component failed, or an install's version could not be verified, or `check`, `update` or `update --dry-run` could not settle a component |
| 2 | usage error: an unknown option or subcommand, or `update <name>` naming no installed component |
| 3 | the run could not start or finish |
| 100 | `check` found updates to install |

`check` exits 0 when everything is up to date, 100 when updates are available, and 1 when at least one component could not be checked (the reason goes to stderr, naming the component). A failed check outranks an available update: `check` exits 1 rather than 100 so no failure is masked.

## Logging

Only the payload — the tables, the summary, the completion script — goes to stdout. Diagnostics, progress and log lines go to stderr.

| Flag | stderr |
| ---- | ------ |
| `-q` | errors only; no progress |
| (none) | warnings, plus progress |
| `-v` | info |
| `-vv` | debug |

`-q` and `-v` are global flags (`plasmoid-updater -q check` and `plasmoid-updater check -q` both work) and cannot be combined. `-v` and `-vv` raise the log level and leave progress on. Log lines read `<level>: <message>`, with the level bold on a colour-capable terminal, and a multi-line message keeps its lines indented under the one entry. `RUST_LOG` is not read.

## Shell completions

`plasmoid-updater completions <SHELL>` prints a completion script on stdout for `bash`, `zsh`, `fish`, `elvish` or `powershell`:

```
plasmoid-updater completions bash > ~/.local/share/bash-completion/completions/plasmoid-updater
```

## Configuration

Settings live in a TOML file at `$XDG_CONFIG_HOME/plasmoid-updater.toml` (usually
`~/.config/plasmoid-updater.toml`). Open it with `plasmoid-updater --edit-config`. Set
`PLASMOID_UPDATER_CONFIG_DIR` to point at a different directory (tests, sandboxes, multiple
profiles).

```toml
# exact name or directory match
excluded_packages = ["widget-name", "another.widget"]
# regex, matched against both the name and directory of each component
excluded_patterns = ["^org\\.kde\\.", "weather"]
update_all_by_default = false
assume_yes = false        # confirm all updates without prompting
prompt_restart = true     # ask before restarting plasmashell
```

`--edit-config` creates the file with commented examples if it does not exist and opens it in `$EDITOR` (`nano` when unset). Excluded components are skipped during updates and hidden from `check`. Invalid regex patterns are dropped instead of aborting the run.

## Topgrade integration (preview)

[Topgrade](https://github.com/topgrade-rs/topgrade/) is a CLI tool that updates everything with a single command.

As this is meant to be a part of Topgrade at some point, I thought it's worth showing how to add it as a custom step until it's not officially supported.

Run `topgrade --edit-config` and add the following under `[commands]`:

```toml
[commands]
"Plasmoids" = "plasmoid-updater update -y"
"Plasmoids System" = "plasmoid-updater update -y --system"
```

![](images/topgrade_custom_step.png)

## Supported (Updatable) Components

| Component Type             | KDE Store Category |
| -------------------------- | ------------------ |
| Plasma Widgets (Plasmoids) | 705                |
| Wallpaper Plugins          | 715                |
| KWin Effects               | 719                |
| KWin Scripts               | 720                |
| KWin Switchers             | 721                |
| Global Themes              | 722                |
| Plasma Styles              | 709                |
| Aurorae Window Decorations | 114                |
| Color Schemes              | 112                |
| Splash Screens             | 708                |
| SDDM Themes                | 101 (system only)  |
| Icon Themes                | 132                |
| Wallpapers                 | 299                |

SDDM themes live under `/usr/share/sddm/themes` and are only handled with `--system` (needs root). Installs and scans respect `$XDG_DATA_HOME` (default `~/.local/share`).

---

## License

Licensed under [GPL-3.0-or-later](https://www.gnu.org/licenses/gpl-3.0.html).

## Acknowledgments & Credits

This project was made possible by studying and learning from:

### [Apdatifier](https://github.com/exequtic/apdatifier)

**Author:** [exequtic](https://github.com/exequtic)
**License:** [MIT License](https://github.com/exequtic/apdatifier/blob/main/LICENSE.md)

Apdatifier is a KDE Plasma widget that monitors for updates to Arch Linux packages, Flatpak applications, and Plasma widgets. The widget update logic, version comparison algorithms, ID resolution fallback chain, and the `widgets-id` mapping table in this project are derived from Apdatifier's implementation.

Concepts taken from Apdatifier:
- 3-tier ID resolution strategy (name matching → KNewStuff registry → static lookup table)
- Version normalization and comparison algorithm
- Download link selection logic for multi-file packages
- Metadata patching approach for installation

### [KDE Discover](https://invent.kde.org/plasma/discover)

**License:** GPL-2.0-only, GPL-3.0-only, LicenseRef-KDE-Accepted-GPL

KDE Discover is the official software center for KDE Plasma. The understanding of KNewStuff registry file formats, kpackagetool6 usage patterns, and OCS API interaction in this project is based on studying Discover's source code.

Concepts taken from KDE Discover:
- KNewStuff registry structure (`.knsregistry` files)
- Installation flow via kpackagetool6
- OCS API v1.6 response format and status codes

### Open Collaboration Services (OCS) API

The [OCS specification](https://www.freedesktop.org/wiki/Specifications/open-collaboration-services/) defines the REST API used by the KDE Store (store.kde.org / api.kde-look.org).

---

*This project is not affiliated with or endorsed by KDE e.V.*
