// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// KDE Store category IDs for supported component types.
const CATEGORY_PLASMA_WIDGET: u16 = 705;
const CATEGORY_WALLPAPER_PLUGIN: u16 = 715;
const CATEGORY_KWIN_EFFECT: u16 = 719;
const CATEGORY_KWIN_SCRIPT: u16 = 720;
const CATEGORY_KWIN_SWITCHER: u16 = 721;
const CATEGORY_GLOBAL_THEME: u16 = 722;
const CATEGORY_PLASMA_STYLE: u16 = 709;
const CATEGORY_AURORAE_DECORATION: u16 = 114;
const CATEGORY_COLOR_SCHEME: u16 = 112;
const CATEGORY_SPLASH_SCREEN: u16 = 708;
const CATEGORY_SDDM_THEME: u16 = 101;
const CATEGORY_ICON_THEME: u16 = 132;
const CATEGORY_WALLPAPER: u16 = 299;

/// Type of KDE Plasma component.
///
/// Maps to KDE Store category IDs and determines installation paths,
/// registry files, and update strategies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentType {
    PlasmaWidget,
    WallpaperPlugin,
    KWinEffect,
    KWinScript,
    KWinSwitcher,
    GlobalTheme,
    PlasmaStyle,
    AuroraeDecoration,
    ColorScheme,
    SplashScreen,
    SddmTheme,
    IconTheme,
    Wallpaper,
}

impl ComponentType {
    pub const fn category_id(self) -> u16 {
        match self {
            Self::PlasmaWidget => CATEGORY_PLASMA_WIDGET,
            Self::WallpaperPlugin => CATEGORY_WALLPAPER_PLUGIN,
            Self::KWinEffect => CATEGORY_KWIN_EFFECT,
            Self::KWinScript => CATEGORY_KWIN_SCRIPT,
            Self::KWinSwitcher => CATEGORY_KWIN_SWITCHER,
            Self::GlobalTheme => CATEGORY_GLOBAL_THEME,
            Self::PlasmaStyle => CATEGORY_PLASMA_STYLE,
            Self::AuroraeDecoration => CATEGORY_AURORAE_DECORATION,
            Self::ColorScheme => CATEGORY_COLOR_SCHEME,
            Self::SplashScreen => CATEGORY_SPLASH_SCREEN,
            Self::SddmTheme => CATEGORY_SDDM_THEME,
            Self::IconTheme => CATEGORY_ICON_THEME,
            Self::Wallpaper => CATEGORY_WALLPAPER,
        }
    }

    pub const fn kpackage_type(self) -> Option<&'static str> {
        match self {
            Self::PlasmaWidget => Some("Plasma/Applet"),
            Self::WallpaperPlugin => Some("Plasma/Wallpaper"),
            Self::KWinEffect => Some("KWin/Effect"),
            Self::KWinScript => Some("KWin/Script"),
            Self::KWinSwitcher => Some("KWin/WindowSwitcher"),
            _ => None,
        }
    }

    /// Returns true if this type uses registry-based discovery only
    /// (no metadata files on disk).
    pub const fn registry_only(self) -> bool {
        matches!(self, Self::IconTheme | Self::Wallpaper | Self::ColorScheme)
    }

    // -- Filesystem paths --

    /// Returns the user-local data directory suffix, or `None` for system-only types (e.g., SDDM).
    pub(crate) const fn user_suffix(self) -> Option<&'static str> {
        match self {
            Self::PlasmaWidget => Some("plasma/plasmoids"),
            Self::WallpaperPlugin => Some("plasma/wallpapers"),
            Self::KWinEffect => Some("kwin/effects"),
            Self::KWinScript => Some("kwin/scripts"),
            Self::KWinSwitcher => Some("kwin/tabbox"),
            Self::GlobalTheme | Self::SplashScreen => Some("plasma/look-and-feel"),
            Self::PlasmaStyle => Some("plasma/desktoptheme"),
            Self::AuroraeDecoration => Some("aurorae/themes"),
            Self::ColorScheme => Some("color-schemes"),
            Self::SddmTheme => None,
            Self::IconTheme => Some("icons"),
            Self::Wallpaper => Some("wallpapers"),
        }
    }

    /// Returns the full user-local installation path for this component type.
    pub fn user_path(self) -> PathBuf {
        match self.user_suffix() {
            Some(suffix) => crate::paths::data_home().join(suffix),
            None => PathBuf::new(),
        }
    }

    /// Returns the system-wide installation path string for this component type.
    pub const fn system_path_str(self) -> &'static str {
        match self {
            Self::PlasmaWidget => "/usr/share/plasma/plasmoids",
            Self::WallpaperPlugin => "/usr/share/plasma/wallpapers",
            Self::KWinEffect => "/usr/share/kwin/effects",
            Self::KWinScript => "/usr/share/kwin/scripts",
            Self::KWinSwitcher => "/usr/share/kwin/tabbox",
            Self::GlobalTheme | Self::SplashScreen => "/usr/share/plasma/look-and-feel",
            Self::PlasmaStyle => "/usr/share/plasma/desktoptheme",
            Self::AuroraeDecoration => "/usr/share/aurorae/themes",
            Self::ColorScheme => "/usr/share/color-schemes",
            Self::SddmTheme => "/usr/share/sddm/themes",
            Self::IconTheme => "/usr/share/icons",
            Self::Wallpaper => "/usr/share/wallpapers",
        }
    }

    /// Returns the system-wide installation path for this component type.
    pub fn system_path(self) -> PathBuf {
        PathBuf::from(self.system_path_str())
    }

    /// Returns the backup subdirectory name for this component type.
    pub(crate) const fn backup_subdir(self) -> &'static str {
        match self {
            Self::PlasmaWidget => "plasma-plasmoids",
            Self::WallpaperPlugin => "plasma-wallpapers",
            Self::KWinEffect => "kwin-effects",
            Self::KWinScript => "kwin-scripts",
            Self::KWinSwitcher => "kwin-tabbox",
            Self::GlobalTheme => "plasma-look-and-feel",
            Self::PlasmaStyle => "plasma-desktoptheme",
            Self::AuroraeDecoration => "aurorae-themes",
            Self::ColorScheme => "color-schemes",
            Self::SplashScreen => "plasma-splash",
            Self::SddmTheme => "sddm-themes",
            Self::IconTheme => "icons",
            Self::Wallpaper => "wallpapers",
        }
    }

    // -- Registry --

    pub const fn registry_file(self) -> Option<&'static str> {
        match self {
            Self::PlasmaWidget => Some("plasmoids.knsregistry"),
            Self::KWinEffect => Some("kwineffect.knsregistry"),
            Self::KWinScript => Some("kwinscripts.knsregistry"),
            Self::KWinSwitcher => Some("kwinswitcher.knsregistry"),
            Self::WallpaperPlugin => Some("wallpaperplugin.knsregistry"),
            Self::GlobalTheme => Some("lookandfeel.knsregistry"),
            Self::PlasmaStyle => Some("plasma-themes.knsregistry"),
            Self::AuroraeDecoration => Some("aurorae.knsregistry"),
            Self::ColorScheme => Some("colorschemes.knsregistry"),
            Self::SplashScreen => Some("ksplash.knsregistry"),
            Self::SddmTheme => Some("sddmtheme.knsregistry"),
            Self::IconTheme => Some("icons.knsregistry"),
            Self::Wallpaper => Some("wallpaper.knsregistry"),
        }
    }

    // -- Enumeration --

    pub const fn all() -> &'static [ComponentType] {
        &[
            Self::PlasmaWidget,
            Self::WallpaperPlugin,
            Self::KWinEffect,
            Self::KWinScript,
            Self::KWinSwitcher,
            Self::GlobalTheme,
            Self::PlasmaStyle,
            Self::AuroraeDecoration,
            Self::ColorScheme,
            Self::SplashScreen,
            Self::SddmTheme,
            Self::IconTheme,
            Self::Wallpaper,
        ]
    }

    pub const fn all_user() -> &'static [ComponentType] {
        &[
            Self::PlasmaWidget,
            Self::WallpaperPlugin,
            Self::KWinEffect,
            Self::KWinScript,
            Self::KWinSwitcher,
            Self::GlobalTheme,
            Self::PlasmaStyle,
            Self::AuroraeDecoration,
            Self::ColorScheme,
            Self::SplashScreen,
            Self::IconTheme,
            Self::Wallpaper,
        ]
    }
}

impl std::fmt::Display for ComponentType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PlasmaWidget => write!(f, "Plasma Widget"),
            Self::WallpaperPlugin => write!(f, "Wallpaper Plugin"),
            Self::KWinEffect => write!(f, "KWin Effect"),
            Self::KWinScript => write!(f, "KWin Script"),
            Self::KWinSwitcher => write!(f, "KWin Switcher"),
            Self::GlobalTheme => write!(f, "Global Theme"),
            Self::PlasmaStyle => write!(f, "Plasma Style"),
            Self::AuroraeDecoration => write!(f, "Aurorae Decoration"),
            Self::ColorScheme => write!(f, "Color Scheme"),
            Self::SplashScreen => write!(f, "Splash Screen"),
            Self::SddmTheme => write!(f, "SDDM Theme"),
            Self::IconTheme => write!(f, "Icon Theme"),
            Self::Wallpaper => write!(f, "Wallpaper"),
        }
    }
}

/// A KDE component installed on the local system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledComponent {
    pub name: String,
    pub directory_name: String,
    pub version: String,
    pub component_type: ComponentType,
    #[serde(with = "pathbuf_serde")]
    pub path: PathBuf,
    pub is_system: bool,
    pub release_date: String,
}

/// An available update for an installed component, with download metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailableUpdate {
    pub installed: InstalledComponent,
    pub content_id: u64,
    pub latest_version: String,
    pub download_url: String,
    pub store_url: String,
    pub release_date: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_size: Option<u64>,
}

/// Builder for constructing [`AvailableUpdate`] instances with optional fields.
pub struct AvailableUpdateBuilder {
    installed: InstalledComponent,
    content_id: u64,
    latest_version: String,
    download_url: String,
    release_date: String,
    checksum: Option<String>,
    download_size: Option<u64>,
}

impl AvailableUpdateBuilder {
    pub fn checksum(mut self, checksum: Option<String>) -> Self {
        self.checksum = checksum;
        self
    }

    pub fn download_size(mut self, size: Option<u64>) -> Self {
        self.download_size = size;
        self
    }

    pub fn build(self) -> AvailableUpdate {
        let store_url = format!("https://store.kde.org/p/{}", self.content_id);
        AvailableUpdate {
            installed: self.installed,
            content_id: self.content_id,
            latest_version: self.latest_version,
            download_url: self.download_url,
            store_url,
            release_date: self.release_date,
            checksum: self.checksum,
            download_size: self.download_size,
        }
    }
}

impl AvailableUpdate {
    pub fn builder(
        installed: InstalledComponent,
        content_id: u64,
        latest_version: String,
        download_url: String,
        release_date: String,
    ) -> AvailableUpdateBuilder {
        AvailableUpdateBuilder {
            installed,
            content_id,
            latest_version,
            download_url,
            release_date,
            checksum: None,
            download_size: None,
        }
    }
}

/// An entry from the KDE Store API representing a published component.
#[derive(Debug, Clone)]
pub struct StoreEntry {
    pub id: u64,
    pub name: String,
    pub version: String,
    pub type_id: u16,
    pub download_links: Vec<DownloadLink>,
    pub changed_date: String,
}

/// A download link for a store entry, with optional checksum and size.
#[derive(Debug, Clone)]
pub struct DownloadLink {
    pub url: String,
    pub version: String,
    pub checksum: Option<String>,
    pub size_kb: Option<u64>,
}

/// Metadata parsed from a component's `metadata.json` or `metadata.desktop` file.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PackageMetadata {
    #[serde(rename = "KPlugin")]
    pub kplugin: Option<KPluginInfo>,
    #[serde(rename = "KPackageStructure")]
    pub kpackage_structure: Option<String>,
}

/// Plugin metadata from the `KPlugin` section of `metadata.json`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct KPluginInfo {
    #[serde(rename = "Name")]
    pub name: Option<String>,
    #[serde(rename = "Version")]
    pub version: Option<String>,
    #[serde(rename = "Description")]
    pub description: Option<String>,
    #[serde(rename = "Icon")]
    pub icon: Option<String>,
}

impl PackageMetadata {
    pub fn name(&self) -> Option<&str> {
        self.kplugin.as_ref()?.name.as_deref()
    }

    pub fn version(&self) -> Option<&str> {
        self.kplugin.as_ref()?.version.as_deref()
    }
}

/// Summary of a batch update operation, tracking successes, failures, and skips.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateSummary {
    pub succeeded: Vec<String>,
    pub failed: Vec<(String, String)>,
    pub skipped: Vec<String>,
}

impl UpdateSummary {
    pub fn add_success(&mut self, name: String) {
        self.succeeded.push(name);
    }

    pub fn add_failure(&mut self, name: String, reason: String) {
        self.failed.push((name, reason));
    }

    pub fn add_skipped(&mut self, name: String) {
        self.skipped.push(name);
    }

    pub fn has_failures(&self) -> bool {
        !self.failed.is_empty()
    }

    pub fn exit_code(&self) -> i32 {
        if self.has_failures() { 1 } else { 0 }
    }
}

/// Detailed diagnostic information about component check status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentDiagnostic {
    pub name: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_id: Option<u64>,
}

impl ComponentDiagnostic {
    pub fn new(name: String, reason: String) -> Self {
        Self {
            name,
            reason,
            installed_version: None,
            available_version: None,
            content_id: None,
        }
    }

    pub fn with_versions(mut self, installed: Option<String>, available: Option<String>) -> Self {
        self.installed_version = installed;
        self.available_version = available;
        self
    }

    pub fn with_content_id(mut self, id: u64) -> Self {
        self.content_id = Some(id);
        self
    }
}

/// Result of checking for available updates, including diagnostics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateCheckResult {
    pub updates: Vec<AvailableUpdate>,
    /// Components that couldn't be matched to KDE Store entries.
    pub unresolved: Vec<ComponentDiagnostic>,
    /// Components that were matched but failed during update check.
    pub check_failures: Vec<ComponentDiagnostic>,
}

impl UpdateCheckResult {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_update(&mut self, update: AvailableUpdate) {
        self.updates.push(update);
    }

    pub fn add_unresolved(&mut self, diagnostic: ComponentDiagnostic) {
        self.unresolved.push(diagnostic);
    }

    pub fn add_check_failure(&mut self, diagnostic: ComponentDiagnostic) {
        self.check_failures.push(diagnostic);
    }

    pub fn has_issues(&self) -> bool {
        !self.unresolved.is_empty() || !self.check_failures.is_empty()
    }
}

mod pathbuf_serde {
    use std::path::{Path, PathBuf};

    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(path: &Path, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&path.to_string_lossy())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<PathBuf, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(PathBuf::from(s))
    }
}
