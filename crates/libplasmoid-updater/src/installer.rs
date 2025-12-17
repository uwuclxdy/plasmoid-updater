// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Installation logic based on Apdatifier (https://github.com/exequtic/apdatifier) - MIT License
// and KDE Discover (https://invent.kde.org/plasma/discover) - GPL-2.0+/LGPL-2.0+

use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use crate::{
    AvailableUpdate, ComponentType, Error, InstalledComponent, Result, UpdateSummary,
    backup::copy_dir_recursive, backup_component, registry, restore_from_backup,
};

const COLOR_SCHEME_EXTENSIONS: &[&str] = &[".colors", ".colorscheme"];
const DOWNLOAD_TIMEOUT_SECS: u64 = 120;

fn replace_destination<F>(dest: &Path, action: F) -> Result<()>
where
    F: FnOnce() -> Result<()>,
{
    if dest.exists() {
        if dest.is_dir() {
            fs::remove_dir_all(dest)?;
        } else {
            fs::remove_file(dest)?;
        }
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }

    action()
}

pub(crate) fn temp_dir() -> PathBuf {
    std::env::var("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
        .join("plasmoid-updater")
}

/// downloads a package with optional checksum verification.
/// the `show_progress` parameter is accepted for API compatibility but ignored
/// in the library - CLI consumers can implement their own progress reporting.
pub fn download_package(
    client: &reqwest::blocking::Client,
    url: &str,
    expected_checksum: Option<&str>,
    _show_progress: bool,
) -> Result<PathBuf> {
    let temp = temp_dir();
    fs::create_dir_all(&temp)?;

    let file_name = url
        .rsplit('/')
        .next()
        .unwrap_or("package.tar.gz")
        .to_string();

    let dest = temp.join(&file_name);

    let response = client
        .get(url)
        .timeout(Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
        .send()
        .map_err(|e| Error::download(format!("request failed: {e}")))?;

    if !response.status().is_success() {
        return Err(Error::download(format!(
            "http status {}",
            response.status()
        )));
    }

    let mut file = File::create(&dest)?;
    let mut hasher = md5::Context::new();

    let mut reader = response;
    let mut buffer = [0u8; 8192];

    loop {
        let bytes_read = reader
            .read(&mut buffer)
            .map_err(|e| Error::download(format!("read error: {e}")))?;

        if bytes_read == 0 {
            break;
        }

        let chunk = &buffer[..bytes_read];
        hasher.consume(chunk);
        file.write_all(chunk)?;
    }

    // verify checksum if provided
    if let Some(expected) = expected_checksum {
        let actual = format!("{:x}", hasher.finalize());
        if actual != expected.to_lowercase() {
            fs::remove_file(&dest).ok();
            return Err(Error::checksum(expected, actual));
        }
        log::debug!("**checksum:** verified md5 for {file_name}");
    }

    Ok(dest)
}

/// creates a shared HTTP client for downloads.
pub fn create_download_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
        .user_agent(concat!("plasmoid-updater/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("failed to create http client")
}

pub fn extract_archive(archive_path: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;

    let status = Command::new("bsdtar")
        .args([
            "-xf",
            &archive_path.to_string_lossy(),
            "-C",
            &dest.to_string_lossy(),
        ])
        .status()
        .map_err(|e| Error::extraction(format!("failed to run bsdtar: {e}")))?;

    if !status.success() {
        return Err(Error::extraction(format!(
            "bsdtar exited with status {}",
            status
        )));
    }

    Ok(())
}

pub fn find_metadata_json(dir: &Path) -> Option<PathBuf> {
    find_metadata_file_recursive(dir, "metadata.json")
}

fn find_metadata_file_recursive(dir: &Path, filename: &str) -> Option<PathBuf> {
    let direct = dir.join(filename);
    if direct.exists() {
        return Some(direct);
    }

    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir()
                && let Some(found) = find_metadata_file_recursive(&path, filename)
            {
                return Some(found);
            }
        }
    }

    None
}

fn find_package_dir(extract_dir: &Path) -> Option<PathBuf> {
    if let Some(metadata) = find_metadata_file_recursive(extract_dir, "metadata.json") {
        return metadata.parent().map(|p| p.to_path_buf());
    }

    if let Some(metadata) = find_metadata_file_recursive(extract_dir, "metadata.desktop") {
        return metadata.parent().map(|p| p.to_path_buf());
    }

    None
}

pub fn patch_metadata(
    metadata_path: &Path,
    component_type: ComponentType,
    new_version: &str,
) -> Result<()> {
    let content = fs::read_to_string(metadata_path)?;
    let mut json: serde_json::Value =
        serde_json::from_str(&content).map_err(Error::MetadataParse)?;

    if let Some(kpackage_type) = component_type.kpackage_type() {
        json["KPackageStructure"] = serde_json::Value::String(kpackage_type.to_string());
    }

    if let Some(kplugin) = json.get_mut("KPlugin") {
        kplugin["Version"] = serde_json::Value::String(new_version.to_string());
    }

    let patched = serde_json::to_string_pretty(&json)?;
    fs::write(metadata_path, patched)?;

    Ok(())
}

pub fn install_via_kpackagetool(package_dir: &Path, component_type: ComponentType) -> Result<()> {
    let kpackage_type = component_type
        .kpackage_type()
        .ok_or_else(|| Error::install(format!("{component_type} has no kpackage type")))?;

    let output = Command::new("kpackagetool6")
        .args(["-t", kpackage_type, "-u", &package_dir.to_string_lossy()])
        .output()
        .map_err(|e| Error::install(format!("failed to run kpackagetool6: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::install(format!(
            "kpackagetool6 failed: {}",
            stderr.trim()
        )));
    }

    Ok(())
}

fn find_color_scheme_file(dir: &Path) -> Option<PathBuf> {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    for ext in COLOR_SCHEME_EXTENSIONS {
                        if name.ends_with(ext) {
                            return Some(path);
                        }
                    }
                }
            } else if path.is_dir()
                && let Some(found) = find_color_scheme_file(&path)
            {
                return Some(found);
            }
        }
    }
    None
}

fn find_installable_dir(extract_dir: &Path, component_type: ComponentType) -> Option<PathBuf> {
    if has_component_structure(extract_dir, component_type) {
        return Some(extract_dir.to_path_buf());
    }

    if let Ok(entries) = fs::read_dir(extract_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && has_component_structure(&path, component_type) {
                return Some(path);
            }
        }
    }

    None
}

fn has_component_structure(dir: &Path, component_type: ComponentType) -> bool {
    match component_type {
        ComponentType::AuroraeDecoration => {
            dir.join("decoration.svg").exists() || dir.join("aurorae").exists()
        }
        ComponentType::GlobalTheme | ComponentType::SplashScreen => {
            dir.join("metadata.json").exists() || dir.join("metadata.desktop").exists()
        }
        ComponentType::PlasmaStyle => {
            dir.join("colors").exists()
                || dir.join("widgets").exists()
                || dir.join("metadata.desktop").exists()
        }
        ComponentType::SddmTheme => {
            dir.join("theme.conf").exists() || dir.join("Main.qml").exists()
        }
        ComponentType::KWinSwitcher => {
            dir.join("metadata.json").exists() || dir.join("contents").exists()
        }
        _ => false,
    }
}

pub fn install_direct(extract_dir: &Path, component: &InstalledComponent) -> Result<()> {
    let dest_dir = &component.path;

    match component.component_type {
        ComponentType::ColorScheme => install_color_scheme(extract_dir, dest_dir),
        ComponentType::IconTheme => install_icon_theme(extract_dir, dest_dir),
        ComponentType::Wallpaper => install_wallpaper(extract_dir, component),
        ComponentType::AuroraeDecoration => {
            install_theme_dir(extract_dir, dest_dir, component.component_type)
        }
        ComponentType::GlobalTheme
        | ComponentType::PlasmaStyle
        | ComponentType::SplashScreen
        | ComponentType::SddmTheme => {
            install_theme_dir(extract_dir, dest_dir, component.component_type)
        }
        _ => Err(Error::install(format!(
            "{} should use kpackagetool",
            component.component_type
        ))),
    }
}

fn install_color_scheme(extract_dir: &Path, dest_path: &Path) -> Result<()> {
    let color_file = find_color_scheme_file(extract_dir)
        .ok_or_else(|| Error::install("no color scheme file found in archive"))?;

    replace_destination(dest_path, || {
        fs::copy(&color_file, dest_path)?;
        log::debug!(
            "**install:** copied color scheme to {}",
            dest_path.display()
        );
        Ok(())
    })
}

fn find_icon_theme_dir(extract_dir: &Path) -> Option<PathBuf> {
    if extract_dir.join("index.theme").exists() {
        return Some(extract_dir.to_path_buf());
    }

    if let Ok(entries) = fs::read_dir(extract_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path.join("index.theme").exists() {
                return Some(path);
            }
        }
    }
    None
}

fn install_icon_theme(extract_dir: &Path, dest_dir: &Path) -> Result<()> {
    let source_dir = find_icon_theme_dir(extract_dir)
        .ok_or_else(|| Error::install("no icon theme (index.theme) found in archive"))?;

    replace_destination(dest_dir, || {
        fs::create_dir_all(dest_dir)?;
        copy_dir_recursive(&source_dir, dest_dir)?;
        log::debug!("**install:** copied icon theme to {}", dest_dir.display());
        Ok(())
    })
}

fn find_wallpaper_source(extract_dir: &Path) -> Option<PathBuf> {
    if extract_dir.join("contents").exists() || extract_dir.join("metadata.json").exists() {
        return Some(extract_dir.to_path_buf());
    }

    if let Ok(entries) = fs::read_dir(extract_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir()
                && (path.join("contents").exists() || path.join("metadata.json").exists())
            {
                return Some(path);
            }
        }
    }

    const IMAGE_EXTENSIONS: &[&str] = &[".jpg", ".jpeg", ".png", ".webp", ".svg"];
    if let Ok(entries) = fs::read_dir(extract_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file()
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
            {
                let lower = name.to_lowercase();
                if IMAGE_EXTENSIONS.iter().any(|ext| lower.ends_with(ext)) {
                    return Some(path);
                }
            }
        }
    }

    None
}

fn install_wallpaper(extract_dir: &Path, component: &InstalledComponent) -> Result<()> {
    let source = find_wallpaper_source(extract_dir)
        .ok_or_else(|| Error::install("no wallpaper found in archive"))?;

    let dest = &component.path;

    if source.is_file() {
        replace_destination(dest, || {
            fs::copy(&source, dest)?;
            log::debug!("**install:** copied wallpaper to {}", dest.display());
            Ok(())
        })
    } else {
        replace_destination(dest, || {
            fs::create_dir_all(dest)?;
            copy_dir_recursive(&source, dest)?;
            log::debug!("**install:** copied wallpaper dir to {}", dest.display());
            Ok(())
        })
    }
}

fn install_theme_dir(
    extract_dir: &Path,
    dest_dir: &Path,
    component_type: ComponentType,
) -> Result<()> {
    let source_dir = find_installable_dir(extract_dir, component_type).ok_or_else(|| {
        Error::install(format!(
            "no valid {component_type} structure found in archive"
        ))
    })?;

    replace_destination(dest_dir, || {
        fs::create_dir_all(dest_dir)?;
        copy_dir_recursive(&source_dir, dest_dir)?;
        log::debug!(
            "**install:** copied {} to {}",
            component_type,
            dest_dir.display()
        );
        Ok(())
    })
}

fn is_raw_installable_file(path: &Path, component_type: ComponentType) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let lower = name.to_lowercase();

    match component_type {
        ComponentType::ColorScheme => COLOR_SCHEME_EXTENSIONS
            .iter()
            .any(|ext| lower.ends_with(ext)),
        ComponentType::Wallpaper => {
            const IMAGE_EXTENSIONS: &[&str] = &[".jpg", ".jpeg", ".png", ".webp", ".svg"];
            IMAGE_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
        }
        _ => false,
    }
}

fn install_raw_file(downloaded: &Path, component: &InstalledComponent) -> Result<()> {
    let dest = &component.path;

    replace_destination(dest, || {
        fs::copy(downloaded, dest)?;
        log::debug!("**install:** copied raw file to {}", dest.display());
        Ok(())
    })
}

/// updates a single component using provided HTTP client.
pub fn update_component_with_client(
    update: &AvailableUpdate,
    client: &reqwest::blocking::Client,
    show_progress: bool,
) -> Result<()> {
    let component = &update.installed;

    log::info!(
        "**update:** {} {} -> {}",
        component.name,
        component.version,
        update.latest_version
    );

    let backup_path = backup_component(component)?;
    log::debug!("**backup:** created at {}", backup_path.display());

    let downloaded_path = match download_package(
        client,
        &update.download_url,
        update.checksum.as_deref(),
        show_progress,
    ) {
        Ok(p) => p,
        Err(e) => {
            log::error!("**download:** failed for {}: {e}", component.name);
            return Err(e);
        }
    };

    let install_result = if is_raw_installable_file(&downloaded_path, component.component_type) {
        let result = install_raw_file(&downloaded_path, component);
        let _ = fs::remove_file(&downloaded_path);
        result
    } else {
        let extract_dir = temp_dir().join(format!("extract-{}", component.directory_name));
        if extract_dir.exists() {
            fs::remove_dir_all(&extract_dir)?;
        }

        if let Err(e) = extract_archive(&downloaded_path, &extract_dir) {
            log::error!("**extract:** failed for {}: {e}", component.name);
            let _ = fs::remove_file(&downloaded_path);
            return Err(e);
        }

        let _ = fs::remove_file(&downloaded_path);

        let result = if component.component_type.kpackage_type().is_some() {
            install_via_kpackage(&extract_dir, component, &update.latest_version)
        } else {
            install_direct(&extract_dir, component)
        };

        let _ = fs::remove_dir_all(&extract_dir);
        result
    };

    if let Err(e) = install_result {
        log::error!("**install:** failed for {}: {e}", component.name);

        if let Err(restore_err) = restore_from_backup(&backup_path, &component.path) {
            log::error!("**restore:** failed: {restore_err}");
        } else {
            log::info!("**restore:** reverted to backup");
        }

        return Err(e);
    }

    let installed_metadata = component.path.join("metadata.json");
    if installed_metadata.exists()
        && let Err(e) = patch_metadata(
            &installed_metadata,
            component.component_type,
            &update.latest_version,
        )
    {
        log::warn!("**patch:** failed to update installed metadata: {e}");
    }

    if let Err(e) = registry::update_registry_after_install(update) {
        log::warn!("**registry:** failed to update: {e}");
    }

    log::info!("**success:** updated {}", component.name);
    Ok(())
}

/// updates a single component (creates its own HTTP client).
pub fn update_component(update: &AvailableUpdate) -> Result<()> {
    let client = create_download_client();
    update_component_with_client(update, &client, false)
}

fn install_via_kpackage(
    extract_dir: &Path,
    component: &InstalledComponent,
    new_version: &str,
) -> Result<()> {
    let package_dir = find_package_dir(extract_dir).ok_or(Error::MetadataNotFound)?;

    let metadata_json = package_dir.join("metadata.json");
    if metadata_json.exists()
        && let Err(e) = patch_metadata(&metadata_json, component.component_type, new_version)
    {
        log::warn!("**patch:** failed for {}: {e}", component.name);
    }

    install_via_kpackagetool(&package_dir, component.component_type)
}

/// updates multiple components sequentially with shared client.
pub fn update_components(updates: &[AvailableUpdate], excluded: &[String]) -> UpdateSummary {
    let client = create_download_client();
    update_components_with_client(updates, excluded, &client, false)
}

/// updates multiple components with provided client.
pub fn update_components_with_client(
    updates: &[AvailableUpdate],
    excluded: &[String],
    client: &reqwest::blocking::Client,
    show_progress: bool,
) -> UpdateSummary {
    let mut summary = UpdateSummary::default();

    for update in updates {
        let name = update.installed.name.clone();
        let dir_name = &update.installed.directory_name;

        if excluded.iter().any(|e| e == dir_name || e == &name) {
            log::debug!("**skip:** {} (excluded)", name);
            summary.add_skipped(name);
            continue;
        }

        match update_component_with_client(update, client, show_progress) {
            Ok(()) => summary.add_success(name),
            Err(e) => summary.add_failure(name, e.to_string()),
        }
    }

    summary
}

fn get_user_id() -> Option<String> {
    std::env::var("UID").ok().or_else(|| {
        Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
    })
}

pub fn restart_plasmashell() -> Result<()> {
    let mut cmd = Command::new("systemctl");
    cmd.args(["--user", "restart", "plasma-plasmashell.service"]);

    if std::env::var("DBUS_SESSION_BUS_ADDRESS").is_err()
        && let Some(uid) = get_user_id()
    {
        cmd.env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path=/run/user/{uid}/bus"),
        );
    }

    if std::env::var("XDG_RUNTIME_DIR").is_err()
        && let Some(uid) = get_user_id()
    {
        cmd.env("XDG_RUNTIME_DIR", format!("/run/user/{uid}"));
    }

    let status = cmd
        .status()
        .map_err(|e| Error::other(format!("failed to restart plasmashell: {e}")))?;

    if !status.success() {
        return Err(Error::other(format!(
            "systemctl restart failed with status {}",
            status
        )));
    }

    Ok(())
}

pub fn requires_plasmashell_restart(component: &InstalledComponent) -> bool {
    matches!(
        component.component_type,
        ComponentType::PlasmaWidget
            | ComponentType::PlasmaStyle
            | ComponentType::GlobalTheme
            | ComponentType::SplashScreen
            | ComponentType::KWinSwitcher
    )
}

pub fn any_requires_restart(updates: &[AvailableUpdate]) -> bool {
    updates
        .iter()
        .any(|u| requires_plasmashell_restart(&u.installed))
}
