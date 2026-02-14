# libplasmoid-updater

A Rust library for managing KDE Plasma 6 components from the KDE Store.

## Features

- **Component Discovery** – Automatically detect installed plasmoids, themes, effects, and more
- **Update Detection** – Check for available updates via KDE Store API
- **Safe Installation** – Install updates with automatic backup and rollback
- **Registry Integration** – Maintain KNewStuff registry compatibility
- **Parallel Processing** – Efficient concurrent update checking

## Supported Components

Plasma Widgets, Wallpaper Plugins, KWin Effects/Scripts, Global Themes, Plasma Styles, Color Schemes, Splash Screens, SDDM Themes, and Aurorae Decorations.

## Usage

```rust
use libplasmoid_updater::{ApiClient, Config, check_updates, update_components};

let config = Config::new();
let api_client = ApiClient::new();

// Check for updates
let result = check_updates(&config, false, &api_client)?;
println!("Found {} updates", result.updates.len());

// Install updates
let summary = update_components(
    &result.updates,
    &config.excluded_packages,
    api_client.http_client(),
    false,
);
println!("Updated: {}, Failed: {}", summary.succeeded.len(), summary.failed.len());
```

## Requirements

Runtime dependencies:
- `bsdtar` – Archive extraction
- `kpackagetool6` – KDE package installation

## CLI Tool

For a ready-to-use command-line interface, see the [`plasmoid-updater`](https://crates.io/crates/plasmoid-updater) binary crate.

## License

Licensed under MIT OR Apache-2.0.
