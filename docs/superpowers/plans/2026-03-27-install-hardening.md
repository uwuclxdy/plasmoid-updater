# Install Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add upgrade-in-progress locking, post-install version verification, kpackage type expansion with fallback, and 3-tier idle inhibition to the library crate.

**Architecture:** Four independent features wired into the existing installer pipeline. Each feature is a self-contained module (`lock.rs`, `inhibit.rs`) or a surgical modification to existing modules (`mod.rs`, `install.rs`, `types.rs`). The flock guard and inhibit guard are RAII structs acquired at the top of the update flow and released on drop. Post-install verification is a new step after `post_install_tasks()`. kpackage expansion adds 3 type mappings with fallback to direct install.

**Tech Stack:** Rust (edition 2024), `nix` crate (flock), `zbus` (DBus inhibit, optional), existing `serde_json`/`freedesktop_entry_parser` for metadata reading.

**Spec:** `docs/superpowers/specs/2026-03-27-install-hardening-design.md`

---

## File Structure

| File | Responsibility |
|------|---------------|
| `libplasmoid-updater/Cargo.toml` | Add `zbus` optional dep, `inhibit` feature flag |
| `libplasmoid-updater/src/error.rs` | Add `AlreadyRunning` error variant |
| `libplasmoid-updater/src/config.rs` | Add `inhibit_idle: bool` field + builder method |
| `libplasmoid-updater/src/types.rs` | Expand `kpackage_type()`, add `has_direct_fallback()` |
| `libplasmoid-updater/src/installer/lock.rs` | New: `UpdateLock` flock guard |
| `libplasmoid-updater/src/installer/inhibit.rs` | New: `InhibitGuard` with 3-tier fallback |
| `libplasmoid-updater/src/installer/mod.rs` | Add `InstallOutcome`, `verify_installed_version()`, kpackage fallback, re-export new modules |
| `libplasmoid-updater/src/installer/install.rs` | Add `has_direct_fallback()` branch in `install_from_archive` |
| `libplasmoid-updater/src/lib.rs` | Acquire `UpdateLock` in `update()`/`install_update()`, add `unverified` to `UpdateResult` |
| `libplasmoid-updater/src/utils.rs` | Acquire `InhibitGuard`, collect `InstallOutcome` into `UpdateResult` |
| `libplasmoid-updater/src/cli/output.rs` | Show `unverified` count in summary |
| `libplasmoid-updater/src/paths.rs` | Add `runtime_dir()` helper for lock file path |

---

### Task 1: Add `AlreadyRunning` error variant

**Files:**
- Modify: `libplasmoid-updater/src/error.rs:5-68` (Error enum)
- Modify: `libplasmoid-updater/src/error.rs:70-84` (is_skippable)

- [ ] **Step 1: Add the error variant**

In `libplasmoid-updater/src/error.rs`, add to the `Error` enum after the `NoUpdatesAvailable` variant:

```rust
    #[error("another plasmoid-updater instance is already running")]
    AlreadyRunning,
```

And update `is_skippable()` to include it:

```rust
    pub fn is_skippable(&self) -> bool {
        matches!(self, Self::NoUpdatesAvailable | Self::ComponentNotFound(_) | Self::AlreadyRunning)
    }
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p libplasmoid-updater --no-default-features`
Expected: compiles with no errors.

- [ ] **Step 3: Commit**

```bash
git add libplasmoid-updater/src/error.rs
git commit -m "feat: add AlreadyRunning error variant"
```

---

### Task 2: Add `runtime_dir()` helper to paths module

**Files:**
- Modify: `libplasmoid-updater/src/paths.rs`

- [ ] **Step 1: Add runtime_dir function**

Add after the existing `cache_home()` function in `libplasmoid-updater/src/paths.rs`:

```rust
/// Returns the XDG runtime directory, or a UID-namespaced /tmp fallback.
pub(crate) fn runtime_dir() -> PathBuf {
    std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(format!("/tmp/plasmoid-updater-{}", nix::unistd::Uid::effective()))
        })
}
```

Add `use nix` if not already imported (paths.rs currently only uses `std::path::PathBuf`).

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p libplasmoid-updater --no-default-features`
Expected: compiles with no errors.

- [ ] **Step 3: Commit**

```bash
git add libplasmoid-updater/src/paths.rs
git commit -m "feat: add runtime_dir() helper for XDG_RUNTIME_DIR"
```

---

### Task 3: Implement `UpdateLock` guard

**Files:**
- Create: `libplasmoid-updater/src/installer/lock.rs`
- Modify: `libplasmoid-updater/src/installer/mod.rs:7-11` (add `mod lock;`)

- [ ] **Step 1: Write the lock module**

Create `libplasmoid-updater/src/installer/lock.rs`:

```rust
// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs::{File, OpenOptions};

use nix::fcntl::{Flock, FlockArg};

use crate::{Error, Result, paths};

/// RAII guard that holds an exclusive flock on the update lock file.
///
/// While this guard is alive, no other `plasmoid-updater` instance can
/// acquire the lock. The lock is released automatically when dropped
/// (or if the process crashes, the kernel releases it).
pub(crate) struct UpdateLock {
    _flock: Flock<File>,
}

impl UpdateLock {
    /// Attempts to acquire an exclusive, non-blocking lock.
    ///
    /// Returns `Err(Error::AlreadyRunning)` if another instance holds the lock.
    pub(crate) fn acquire() -> Result<Self> {
        let lock_path = paths::runtime_dir().join("plasmoid-updater.lock");

        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                Error::other(format!("failed to create lock directory: {e}"))
            })?;
        }

        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|e| Error::other(format!("failed to open lock file: {e}")))?;

        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(flock) => {
                log::debug!(target: "lock", "acquired update lock at {}", lock_path.display());
                Ok(Self { _flock: flock })
            }
            Err((_, errno)) => {
                log::debug!(target: "lock", "lock acquisition failed: {errno}");
                Err(Error::AlreadyRunning)
            }
        }
    }
}
```

- [ ] **Step 2: Register the module**

In `libplasmoid-updater/src/installer/mod.rs`, add after line 9 (`mod install;`):

```rust
mod lock;
```

And add the re-export after line 26:

```rust
pub(crate) use lock::UpdateLock;
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check -p libplasmoid-updater --no-default-features`
Expected: compiles with no errors (UpdateLock is defined but not yet used — that's fine).

- [ ] **Step 4: Commit**

```bash
git add libplasmoid-updater/src/installer/lock.rs libplasmoid-updater/src/installer/mod.rs
git commit -m "feat: add UpdateLock flock guard for concurrent instance detection"
```

---

### Task 4: Wire `UpdateLock` into public API

**Files:**
- Modify: `libplasmoid-updater/src/lib.rs:115-149` (`update()` function)
- Modify: `libplasmoid-updater/src/lib.rs:228-232` (`install_update()` function)

- [ ] **Step 1: Add lock acquisition to `update()`**

In `libplasmoid-updater/src/lib.rs`, modify the `update()` function to acquire the lock at the top, before environment validation:

```rust
pub fn update(config: &Config) -> Result<UpdateResult> {
    let _lock = installer::UpdateLock::acquire()?;

    crate::utils::validate_environment(config.skip_plasma_detection)?;

    // ... rest of function unchanged
```

- [ ] **Step 2: Add lock acquisition to `install_update()`**

```rust
pub fn install_update(update: &AvailableUpdate, _config: &Config) -> Result<()> {
    let _lock = installer::UpdateLock::acquire()?;

    let api_client = ApiClient::new();
    let counter = api_client.request_counter();
    installer::update_component(update, api_client.http_client(), |_| {}, &counter)
}
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check -p libplasmoid-updater --no-default-features`
Expected: compiles with no errors.

- [ ] **Step 4: Commit**

```bash
git add libplasmoid-updater/src/lib.rs
git commit -m "feat: acquire UpdateLock at start of update() and install_update()"
```

---

### Task 5: Expand `kpackage_type()` and add `has_direct_fallback()`

**Files:**
- Modify: `libplasmoid-updater/src/types.rs:77-86` (`kpackage_type()`)

- [ ] **Step 1: Expand `kpackage_type()` with new mappings**

In `libplasmoid-updater/src/types.rs`, replace the `kpackage_type()` method:

```rust
    pub(crate) const fn kpackage_type(self) -> Option<&'static str> {
        match self {
            Self::PlasmaWidget => Some("Plasma/Applet"),
            Self::WallpaperPlugin => Some("Plasma/Wallpaper"),
            Self::KWinEffect => Some("KWin/Effect"),
            Self::KWinScript => Some("KWin/Script"),
            Self::KWinSwitcher => Some("KWin/WindowSwitcher"),
            Self::GlobalTheme => Some("Plasma/LookAndFeel"),
            Self::PlasmaStyle => Some("Plasma/Theme"),
            Self::SplashScreen => Some("Plasma/LookAndFeel"),
            _ => None,
        }
    }
```

- [ ] **Step 2: Add `has_direct_fallback()` method**

Add after `kpackage_type()`:

```rust
    /// Returns `true` if this type can fall back to direct file installation
    /// when `kpackagetool6` fails.
    ///
    /// Only the newly-added kpackage types (GlobalTheme, PlasmaStyle, SplashScreen)
    /// have this fallback, since they previously worked with direct install.
    /// The original 5 types (PlasmaWidget, WallpaperPlugin, KWinEffect, KWinScript,
    /// KWinSwitcher) have always required kpackagetool6 and have no fallback.
    pub(crate) const fn has_direct_fallback(self) -> bool {
        matches!(
            self,
            Self::GlobalTheme | Self::PlasmaStyle | Self::SplashScreen
        )
    }
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check -p libplasmoid-updater --no-default-features`
Expected: compiles with no errors.

- [ ] **Step 4: Commit**

```bash
git add libplasmoid-updater/src/types.rs
git commit -m "feat: expand kpackage_type() for GlobalTheme/PlasmaStyle/SplashScreen"
```

---

### Task 6: Add kpackagetool6 fallback in `install_from_archive`

**Files:**
- Modify: `libplasmoid-updater/src/installer/mod.rs:122-151` (`install_from_archive`)

- [ ] **Step 1: Replace the kpackage/direct branch with fallback logic**

In `libplasmoid-updater/src/installer/mod.rs`, replace lines 143-147 inside `install_from_archive()`:

```rust
    // old:
    let result = if component.component_type.kpackage_type().is_some() {
        install::install_via_kpackage(&extract_dir, component, new_version)
    } else {
        install::install_direct(&extract_dir, component)
    };
```

with:

```rust
    let result = if component.component_type.kpackage_type().is_some() {
        match install::install_via_kpackage(&extract_dir, component, new_version) {
            Ok(()) => Ok(()),
            Err(e) if component.component_type.has_direct_fallback() => {
                log::warn!(
                    target: "install",
                    "kpackagetool6 failed for {}, falling back to direct install: {e}",
                    component.name,
                );
                install::install_direct(&extract_dir, component)
            }
            Err(e) => Err(e),
        }
    } else {
        install::install_direct(&extract_dir, component)
    };
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p libplasmoid-updater --no-default-features`
Expected: compiles with no errors.

- [ ] **Step 3: Commit**

```bash
git add libplasmoid-updater/src/installer/mod.rs
git commit -m "feat: fall back to direct install when kpackagetool6 fails for new types"
```

---

### Task 7: Add `InstallOutcome` and post-install verification

**Files:**
- Modify: `libplasmoid-updater/src/installer/mod.rs`

- [ ] **Step 1: Add `InstallOutcome` struct and `verify_installed_version` function**

In `libplasmoid-updater/src/installer/mod.rs`, add after the existing imports (after line 24):

```rust
use crate::version::normalize_version;

/// Outcome of a single component update, including post-install verification.
pub(crate) struct InstallOutcome {
    /// `true` if the post-install version matches the expected version.
    pub verified: bool,
    /// The version we expected to install.
    pub expected_version: String,
    /// The version actually found on disk after install, if readable.
    pub actual_version: Option<String>,
}
```

Add the verification function before `handle_installation_failure`:

```rust
fn verify_installed_version(update: &AvailableUpdate) -> InstallOutcome {
    let component = &update.installed;
    let expected = &update.latest_version;

    let actual = read_installed_version(component);

    let verified = match &actual {
        Some(v) => normalize_version(v) == normalize_version(expected),
        None => false,
    };

    if verified {
        log::debug!(
            target: "verify",
            "{}: version {} confirmed",
            component.name, expected,
        );
    } else {
        log::warn!(
            target: "verify",
            "{}: expected version {}, found {}",
            component.name,
            expected,
            actual.as_deref().unwrap_or("(unreadable)"),
        );
    }

    InstallOutcome {
        verified,
        expected_version: expected.clone(),
        actual_version: actual,
    }
}

fn read_installed_version(component: &InstalledComponent) -> Option<String> {
    // For registry-only types, read from the KNewStuff registry
    if component.component_type.registry_only() {
        return read_version_from_registry(component);
    }

    // Try metadata.json first
    let json_path = component.path.join("metadata.json");
    if json_path.exists() {
        if let Ok(content) = fs::read_to_string(&json_path) {
            if let Ok(meta) = serde_json::from_str::<crate::types::PackageMetadata>(&content) {
                if let Some(v) = meta.version() {
                    return Some(v.to_string());
                }
            }
        }
    }

    // Fall back to metadata.desktop
    let desktop_path = component.path.join("metadata.desktop");
    if desktop_path.exists() {
        if let Ok(content) = fs::read_to_string(&desktop_path) {
            for line in content.lines() {
                if let Some(version) = line.strip_prefix("X-KDE-PluginInfo-Version=") {
                    return Some(version.to_string());
                }
            }
        }
    }

    None
}

fn read_version_from_registry(component: &InstalledComponent) -> Option<String> {
    use crate::registry::RegistryManager;

    let manager = RegistryManager::for_component_type(component.component_type)?;
    let entries = manager.read_entries().ok()?;
    entries
        .iter()
        .find(|e| e.name == component.name || e.installed_path == component.path)
        .map(|e| e.version.clone())
}
```

- [ ] **Step 2: Change `update_component` to return `InstallOutcome`**

Replace the `update_component` function signature and body:

```rust
pub(crate) fn update_component(
    update: &AvailableUpdate,
    client: &reqwest::blocking::Client,
    reporter: impl Fn(u8),
    counter: &AtomicUsize,
) -> Result<InstallOutcome> {
    let component = &update.installed;

    let backup_path = create_backup(component)?;
    reporter(1);

    match perform_installation(update, client, &reporter, counter) {
        Ok(()) => {
            post_install_tasks(update)?;
            let outcome = verify_installed_version(update);
            log::info!(target: "update", "updated {}", component.name);
            Ok(outcome)
        }
        Err(e) => {
            log::error!(target: "install", "failed for {}: {e}", component.name);
            handle_installation_failure(&backup_path, &component.path)?;
            Err(e)
        }
    }
}
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check -p libplasmoid-updater --no-default-features`
Expected: Compilation errors in `utils.rs` and `lib.rs` because they expect `Result<()>` from `update_component`. This is expected and will be fixed in the next task.

- [ ] **Step 4: Commit**

```bash
git add libplasmoid-updater/src/installer/mod.rs
git commit -m "feat: add InstallOutcome and post-install version verification"
```

---

### Task 8: Add `unverified` field to `UpdateResult` and wire up outcomes

**Files:**
- Modify: `libplasmoid-updater/src/lib.rs:164-169` (`UpdateResult` struct)
- Modify: `libplasmoid-updater/src/lib.rs:171-190` (`UpdateResult` impl)
- Modify: `libplasmoid-updater/src/lib.rs:228-232` (`install_update()`)
- Modify: `libplasmoid-updater/src/utils.rs:145-199` (`install_selected_updates`)

- [ ] **Step 1: Add `unverified` to `UpdateResult`**

In `libplasmoid-updater/src/lib.rs`, modify the `UpdateResult` struct:

```rust
#[derive(Debug, Clone, Default, Serialize)]
pub struct UpdateResult {
    pub succeeded: Vec<String>,
    pub failed: Vec<FailedUpdate>,
    pub skipped: Vec<String>,
    /// Components that installed successfully but whose post-install version
    /// could not be verified to match the expected version.
    pub unverified: Vec<String>,
}
```

Update `is_empty()`:

```rust
    pub fn is_empty(&self) -> bool {
        self.succeeded.is_empty()
            && self.failed.is_empty()
            && self.skipped.is_empty()
            && self.unverified.is_empty()
    }
```

- [ ] **Step 2: Update `install_selected_updates` to collect outcomes**

In `libplasmoid-updater/src/utils.rs`, replace the match block inside the `par_iter` closure (lines 177-189):

```rust
            match installer::update_component(update, api_client.http_client(), reporter, &counter)
            {
                Ok(outcome) => {
                    #[cfg(feature = "cli")]
                    ui.complete_task(index, true);
                    let mut r = result.lock();
                    r.succeeded.push(name.clone());
                    if !outcome.verified {
                        r.unverified.push(name);
                    }
                }
                Err(e) => {
                    #[cfg(feature = "cli")]
                    ui.complete_task(index, false);
                    result.lock().failed.push(FailedUpdate { name, error: e.to_string() });
                }
            }
```

- [ ] **Step 3: Update `install_update` to discard outcome**

In `libplasmoid-updater/src/lib.rs`, update `install_update()`:

```rust
pub fn install_update(update: &AvailableUpdate, _config: &Config) -> Result<()> {
    let _lock = installer::UpdateLock::acquire()?;

    let api_client = ApiClient::new();
    let counter = api_client.request_counter();
    installer::update_component(update, api_client.http_client(), |_| {}, &counter)?;
    Ok(())
}
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo check -p libplasmoid-updater --no-default-features`
Expected: compiles with no errors.

- [ ] **Step 5: Commit**

```bash
git add libplasmoid-updater/src/lib.rs libplasmoid-updater/src/utils.rs
git commit -m "feat: collect InstallOutcome into UpdateResult.unverified"
```

---

### Task 9: Update CLI summary to show unverified count

**Files:**
- Modify: `libplasmoid-updater/src/cli/output.rs:93-103` (`print_summary`)

- [ ] **Step 1: Update print_summary**

Replace the `print_summary` function:

```rust
pub fn print_summary(update_result: &UpdateResult) {
    let total = update_result.succeeded.len()
        + update_result.failed.len()
        + update_result.skipped.len();

    if update_result.unverified.is_empty() {
        println!(
            "Update Summary: {} succeeded, {} failed, {} skipped ({} total)",
            update_result.succeeded.len(),
            update_result.failed.len(),
            update_result.skipped.len(),
            total,
        );
    } else {
        println!(
            "Update Summary: {} succeeded ({} unverified), {} failed, {} skipped ({} total)",
            update_result.succeeded.len(),
            update_result.unverified.len(),
            update_result.failed.len(),
            update_result.skipped.len(),
            total,
        );
    }
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p libplasmoid-updater`
Expected: compiles with no errors.

- [ ] **Step 3: Commit**

```bash
git add libplasmoid-updater/src/cli/output.rs
git commit -m "feat: show unverified count in CLI update summary"
```

---

### Task 10: Add `inhibit_idle` config field

**Files:**
- Modify: `libplasmoid-updater/src/config.rs:55-102` (Config struct)
- Modify: `libplasmoid-updater/src/config.rs:104-269` (Config impl)

- [ ] **Step 1: Add the field to Config**

In `libplasmoid-updater/src/config.rs`, add after the `skip_plasma_detection` field (line 101):

```rust
    /// When `true` (default), inhibit system idle/sleep/shutdown during installs.
    ///
    /// Uses a 3-tier fallback: logind DBus → `systemd-inhibit` subprocess → no-op.
    /// Set to `false` if the caller handles its own power management inhibition.
    pub inhibit_idle: bool,
```

- [ ] **Step 2: Update `Config::new()` to set the default**

In the `Config::new()` method, change the return to explicitly set `inhibit_idle`:

```rust
    pub fn new() -> Self {
        Self {
            widgets_id_table: Self::parse_widgets_id(DEFAULT_WIDGETS_ID),
            inhibit_idle: true,
            ..Default::default()
        }
    }
```

- [ ] **Step 3: Add builder method**

Add after `with_skip_plasma_detection`:

```rust
    /// Sets whether to inhibit system idle/sleep/shutdown during installs.
    ///
    /// Defaults to `true`. Set to `false` if the calling application handles
    /// its own power management inhibition (e.g., a GUI app using DBus directly).
    pub fn with_inhibit_idle(mut self, inhibit: bool) -> Self {
        self.inhibit_idle = inhibit;
        self
    }
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo check -p libplasmoid-updater --no-default-features`
Expected: compiles with no errors.

- [ ] **Step 5: Commit**

```bash
git add libplasmoid-updater/src/config.rs
git commit -m "feat: add inhibit_idle config field (default: true)"
```

---

### Task 11: Add `zbus` dependency and `inhibit` feature flag

**Files:**
- Modify: `libplasmoid-updater/Cargo.toml`

- [ ] **Step 1: Add the feature flag and dependency**

In `libplasmoid-updater/Cargo.toml`, update the `[features]` section:

```toml
[features]
default = ["cli", "inhibit"]
cli = ["indicatif", "comfy-table", "bytesize", "inquire", "is-terminal", "terminal_size"]
inhibit = ["zbus"]
debug = []
```

Add `zbus` to the dependencies section (after `nix`):

```toml
zbus = { version = "5.14.0", default-features = false, optional = true }
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p libplasmoid-updater`
Expected: compiles with no errors. zbus is now available behind `cfg(feature = "inhibit")`.

- [ ] **Step 3: Verify it compiles without inhibit**

Run: `cargo check -p libplasmoid-updater --no-default-features`
Expected: compiles with no errors.

- [ ] **Step 4: Commit**

```bash
git add libplasmoid-updater/Cargo.toml
git commit -m "feat: add zbus dependency behind optional 'inhibit' feature flag"
```

---

### Task 12: Implement `InhibitGuard` with 3-tier fallback

**Files:**
- Create: `libplasmoid-updater/src/installer/inhibit.rs`
- Modify: `libplasmoid-updater/src/installer/mod.rs` (add `mod inhibit;`)

- [ ] **Step 1: Write the inhibit module**

Create `libplasmoid-updater/src/installer/inhibit.rs`:

```rust
// SPDX-License-Identifier: GPL-3.0-or-later

use std::process::{Child, Command, Stdio};

/// RAII guard that inhibits system idle/sleep/shutdown while held.
///
/// Uses a 3-tier fallback:
/// 1. DBus logind `Inhibit()` call (requires `inhibit` feature / `zbus`)
/// 2. `systemd-inhibit` subprocess holding a `cat` process
/// 3. No-op with a warning (non-systemd systems or missing tools)
pub(crate) enum InhibitGuard {
    #[cfg(feature = "inhibit")]
    Dbus(std::os::unix::io::OwnedFd),
    Subprocess(Child),
    None,
}

impl InhibitGuard {
    /// Acquires an inhibit lock. Never fails — falls through to `None` on error.
    pub(crate) fn acquire() -> Self {
        #[cfg(feature = "inhibit")]
        if let Some(guard) = try_dbus_inhibit() {
            return guard;
        }

        if let Some(guard) = try_subprocess_inhibit() {
            return guard;
        }

        log::warn!(
            target: "inhibit",
            "could not inhibit system sleep; neither logind DBus nor systemd-inhibit available"
        );
        Self::None
    }
}

impl Drop for InhibitGuard {
    fn drop(&mut self) {
        match self {
            #[cfg(feature = "inhibit")]
            Self::Dbus(_) => {
                log::debug!(target: "inhibit", "releasing DBus inhibit lock");
                // OwnedFd closes automatically
            }
            Self::Subprocess(child) => {
                log::debug!(target: "inhibit", "killing systemd-inhibit subprocess");
                let _ = child.kill();
                let _ = child.wait();
            }
            Self::None => {}
        }
    }
}

#[cfg(feature = "inhibit")]
fn try_dbus_inhibit() -> Option<InhibitGuard> {
    use std::os::unix::io::OwnedFd;
    use zbus::blocking::Connection;

    let conn = match Connection::system() {
        Ok(c) => c,
        Err(e) => {
            log::debug!(target: "inhibit", "DBus connection failed: {e}");
            return None;
        }
    };

    let reply = match conn.call_method(
        Some("org.freedesktop.login1"),
        "/org/freedesktop/login1",
        Some("org.freedesktop.login1.Manager"),
        "Inhibit",
        &("idle:sleep:shutdown", "plasmoid-updater", "Installing updates", "block"),
    ) {
        Ok(r) => r,
        Err(e) => {
            log::debug!(target: "inhibit", "logind Inhibit() call failed: {e}");
            return None;
        }
    };

    match reply.body().deserialize::<OwnedFd>() {
        Ok(fd) => {
            log::debug!(target: "inhibit", "acquired logind DBus inhibit lock");
            Some(InhibitGuard::Dbus(fd))
        }
        Err(e) => {
            log::debug!(target: "inhibit", "failed to deserialize inhibit fd: {e}");
            None
        }
    }
}

fn try_subprocess_inhibit() -> Option<InhibitGuard> {
    let child = Command::new("systemd-inhibit")
        .args([
            "--what=idle:sleep:shutdown",
            "--who=plasmoid-updater",
            "--why=Installing updates",
            "cat",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();

    match child {
        Ok(child) => {
            log::debug!(target: "inhibit", "acquired systemd-inhibit subprocess lock");
            Some(InhibitGuard::Subprocess(child))
        }
        Err(e) => {
            log::debug!(target: "inhibit", "systemd-inhibit spawn failed: {e}");
            None
        }
    }
}
```

- [ ] **Step 2: Register the module and re-export**

In `libplasmoid-updater/src/installer/mod.rs`, add after `mod lock;`:

```rust
mod inhibit;
```

And add the re-export next to the `UpdateLock` re-export:

```rust
pub(crate) use inhibit::InhibitGuard;
```

- [ ] **Step 3: Verify it compiles with inhibit feature**

Run: `cargo check -p libplasmoid-updater`
Expected: compiles with no errors.

- [ ] **Step 4: Verify it compiles without inhibit feature**

Run: `cargo check -p libplasmoid-updater --no-default-features`
Expected: compiles with no errors (DBus tier is `cfg`'d out).

- [ ] **Step 5: Commit**

```bash
git add libplasmoid-updater/src/installer/inhibit.rs libplasmoid-updater/src/installer/mod.rs
git commit -m "feat: add InhibitGuard with 3-tier fallback (DBus/subprocess/no-op)"
```

---

### Task 13: Wire `InhibitGuard` into the install pipeline

**Files:**
- Modify: `libplasmoid-updater/src/utils.rs:145-199` (`install_selected_updates`)
- Modify: `libplasmoid-updater/src/lib.rs:228-232` (`install_update()`)

- [ ] **Step 1: Acquire inhibit guard in `install_selected_updates`**

In `libplasmoid-updater/src/utils.rs`, add at the top of `install_selected_updates`, after the `result` mutex is created and before the `#[cfg(feature = "cli")]` line:

```rust
    let _inhibit = if config.inhibit_idle {
        installer::InhibitGuard::acquire()
    } else {
        installer::InhibitGuard::None
    };
```

This requires adding `installer::InhibitGuard` to the imports. Update the existing `use crate::` block to include `installer`.

- [ ] **Step 2: Acquire inhibit guard in `install_update`**

In `libplasmoid-updater/src/lib.rs`, update `install_update()` to acquire inhibit before the component update. The function should now read:

```rust
pub fn install_update(update: &AvailableUpdate, config: &Config) -> Result<()> {
    let _lock = installer::UpdateLock::acquire()?;
    let _inhibit = if config.inhibit_idle {
        installer::InhibitGuard::acquire()
    } else {
        installer::InhibitGuard::None
    };

    let api_client = ApiClient::new();
    let counter = api_client.request_counter();
    installer::update_component(update, api_client.http_client(), |_| {}, &counter)?;
    Ok(())
}
```

Note: `_config` parameter becomes `config` (remove the underscore prefix since it's now used).

- [ ] **Step 3: Verify it compiles**

Run: `cargo check -p libplasmoid-updater`
Expected: compiles with no errors.

- [ ] **Step 4: Verify without inhibit feature**

Run: `cargo check -p libplasmoid-updater --no-default-features`
Expected: compiles with no errors. `InhibitGuard::None` is the only variant used.

- [ ] **Step 5: Commit**

```bash
git add libplasmoid-updater/src/utils.rs libplasmoid-updater/src/lib.rs
git commit -m "feat: acquire InhibitGuard during install pipeline"
```

---

### Task 14: Final integration check

**Files:** None (verification only)

- [ ] **Step 1: Full build with all features**

Run: `cargo build -p libplasmoid-updater`
Expected: builds successfully.

- [ ] **Step 2: Full build without optional features**

Run: `cargo build -p libplasmoid-updater --no-default-features`
Expected: builds successfully.

- [ ] **Step 3: Build the binary crate**

Run: `cargo build -p plasmoid-updater`
Expected: builds successfully. The binary crate depends on the lib with default features.

- [ ] **Step 4: Run existing tests**

Run: `cargo test -p libplasmoid-updater`
Expected: all existing tests pass. No behavioral regressions.

- [ ] **Step 5: Run clippy**

Run: `cargo clippy -p libplasmoid-updater -- -D warnings`
Expected: no warnings.

- [ ] **Step 6: Commit any clippy fixes if needed, then tag**

```bash
git add -A
git commit -m "chore: fix any clippy warnings from install hardening"
```

(Skip commit if no changes needed.)
