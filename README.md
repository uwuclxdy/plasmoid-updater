# plasmoid-updater

Updates KDE Plasma 6 components from KDE Store using a combination of Apdatifier's and KDE Discover's logic.

## Requirements

- bsdtar
- kpackagetool6

## Supported Components

| Component Type             | KDE Store Category |
|----------------------------|--------------------|
| Plasma Widgets (Plasmoids) | 705                |
| Wallpaper Plugins          | 715                |
| KWin Effects               | 719                |
| KWin Scripts               | 720                |
| Global Themes              | 722                |
| Plasma Styles              | 709                |
| Aurorae Window Decorations | 114                |
| Color Schemes              | 112                |
| Splash Screens             | 708                |
| SDDM Themes                | 101                |

## Installation

```bash
cargo install plasmoid-updater
```

Precompiled binaries available on [Releases](https://github.com/uwuclxdy/plasmoid-updater/releases/latest) page.

Build from source:

```bash
git clone https://github.com/uwuclxdy/plasmoid-updater
cd plasmoid-updater
cargo build --release
```

## Usage

```bash
# List installed components
plasmoid-updater list-installed

# Update all user components
plasmoid-updater update --all

# Update all system components
plasmoid-updater update --all --system

# Update a specific component
plasmoid-updater update <component-name>
```

## License

Licensed under MIT OR Apache-2.0.

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
