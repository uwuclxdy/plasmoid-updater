// SPDX-License-Identifier: GPL-3.0-or-later

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
    pub(crate) const fn category_id(self) -> u16 {
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

    /// Returns true if the given store `type_id` belongs to this component type.
    ///
    /// The OCS API returns subcategory IDs in the `typeid` field. For example,
    /// PlasmaWidget queries with parent category 705 but store entries have
    /// specific subcategory IDs (706 "Applets", 708 "Clocks", 710 "Monitoring",
    /// etc.). This method accounts for those parent-child relationships.
    pub(crate) const fn matches_type_id(self, type_id: u16) -> bool {
        if self.category_id() == type_id {
            return true;
        }
        // PlasmaWidget (705) is the parent of all subcategories in the 700-range.
        // Using the full range is safe because other 700-range types (e.g. 708
        // SplashScreen, 709 PlasmaStyle) have their own `category_id()` which
        // fires first via the direct match above.
        matches!((self, type_id), (Self::PlasmaWidget, 700..=799))
    }

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

    /// Returns true if this type uses registry-based discovery only
    /// (no metadata files on disk).
    pub(crate) const fn registry_only(self) -> bool {
        matches!(self, Self::IconTheme | Self::Wallpaper | Self::ColorScheme)
    }

    /// Returns all component types that share the same filesystem path.
    ///
    /// `GlobalTheme` and `SplashScreen` both use `plasma/look-and-feel`.
    /// During discovery, components in shared directories need to be
    /// checked against all possible types to assign the correct `ComponentType`.
    pub(crate) fn shared_path_types(self) -> &'static [ComponentType] {
        match self {
            Self::GlobalTheme | Self::SplashScreen => &[Self::GlobalTheme, Self::SplashScreen],
            Self::PlasmaWidget => &[Self::PlasmaWidget],
            Self::WallpaperPlugin => &[Self::WallpaperPlugin],
            Self::KWinEffect => &[Self::KWinEffect],
            Self::KWinScript => &[Self::KWinScript],
            Self::KWinSwitcher => &[Self::KWinSwitcher],
            Self::PlasmaStyle => &[Self::PlasmaStyle],
            Self::AuroraeDecoration => &[Self::AuroraeDecoration],
            Self::ColorScheme => &[Self::ColorScheme],
            Self::SddmTheme => &[Self::SddmTheme],
            Self::IconTheme => &[Self::IconTheme],
            Self::Wallpaper => &[Self::Wallpaper],
        }
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

    /// Returns the system-wide installation path for this component type.
    pub fn system_path(self) -> PathBuf {
        PathBuf::from(match self {
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
        })
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

    pub(crate) const fn registry_file(self) -> Option<&'static str> {
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

// -- Internal types --

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
pub(crate) struct AvailableUpdateBuilder {
    installed: InstalledComponent,
    content_id: u64,
    latest_version: String,
    download_url: String,
    release_date: String,
    checksum: Option<String>,
    download_size: Option<u64>,
}

impl AvailableUpdateBuilder {
    pub(crate) fn checksum(mut self, checksum: Option<String>) -> Self {
        self.checksum = checksum;
        self
    }

    pub(crate) fn download_size(mut self, size: Option<u64>) -> Self {
        self.download_size = size;
        self
    }

    pub(crate) fn build(self) -> AvailableUpdate {
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
    pub(crate) fn builder(
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
pub(crate) struct StoreEntry {
    pub id: u64,
    pub name: String,
    pub version: String,
    pub type_id: u16,
    pub download_links: Vec<DownloadLink>,
    pub changed_date: String,
}

/// A download link for a store entry, with optional checksum and size.
#[derive(Debug, Clone)]
pub(crate) struct DownloadLink {
    pub url: String,
    pub version: String,
    pub checksum: Option<String>,
    pub size_kb: Option<u64>,
}

/// Metadata parsed from a component's `metadata.json` or `metadata.desktop` file.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct PackageMetadata {
    #[serde(rename = "KPlugin")]
    pub kplugin: Option<KPluginInfo>,
}

/// Plugin metadata from the `KPlugin` section of `metadata.json`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct KPluginInfo {
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
    pub(crate) fn name(&self) -> Option<&str> {
        self.kplugin.as_ref()?.name.as_deref()
    }

    pub(crate) fn version(&self) -> Option<&str> {
        self.kplugin.as_ref()?.version.as_deref()
    }
}

/// Diagnostic information about a component that could not be checked or updated.
///
/// Returned as part of [`CheckResult::diagnostics`](crate::CheckResult::diagnostics).
/// Contains the component name, the reason it was skipped, and optional version/ID metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub name: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_id: Option<u64>,
}

impl Diagnostic {
    pub(crate) fn new(name: String, reason: String) -> Self {
        Self {
            name,
            reason,
            installed_version: None,
            available_version: None,
            content_id: None,
        }
    }

    pub(crate) fn with_versions(
        mut self,
        installed: Option<String>,
        available: Option<String>,
    ) -> Self {
        self.installed_version = installed;
        self.available_version = available;
        self
    }

    pub(crate) fn with_content_id(mut self, id: u64) -> Self {
        self.content_id = Some(id);
        self
    }
}

/// Internal result of checking for available updates, including diagnostics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct UpdateCheckResult {
    pub updates: Vec<AvailableUpdate>,
    pub unresolved: Vec<Diagnostic>,
    pub check_failures: Vec<Diagnostic>,
}

impl UpdateCheckResult {
    pub fn add_update(&mut self, update: AvailableUpdate) {
        self.updates.push(update);
    }

    pub fn add_unresolved(&mut self, diagnostic: Diagnostic) {
        self.unresolved.push(diagnostic);
    }

    pub fn add_check_failure(&mut self, diagnostic: Diagnostic) {
        self.check_failures.push(diagnostic);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_path_types_returns_both_for_global_theme() {
        let types = ComponentType::GlobalTheme.shared_path_types();
        assert!(types.contains(&ComponentType::GlobalTheme));
        assert!(types.contains(&ComponentType::SplashScreen));
        assert_eq!(types.len(), 2);
    }

    #[test]
    fn shared_path_types_returns_both_for_splash_screen() {
        let types = ComponentType::SplashScreen.shared_path_types();
        assert!(types.contains(&ComponentType::GlobalTheme));
        assert!(types.contains(&ComponentType::SplashScreen));
        assert_eq!(types.len(), 2);
    }

    #[test]
    fn plasma_widget_matches_extended_subcategories() {
        assert!(ComponentType::PlasmaWidget.matches_type_id(705)); // parent
        assert!(ComponentType::PlasmaWidget.matches_type_id(706)); // existing
        assert!(ComponentType::PlasmaWidget.matches_type_id(714)); // previously missing
        assert!(ComponentType::PlasmaWidget.matches_type_id(718)); // previously missing
        assert!(ComponentType::PlasmaWidget.matches_type_id(723)); // existing
        assert!(!ComponentType::PlasmaWidget.matches_type_id(100)); // unrelated
        assert!(!ComponentType::PlasmaWidget.matches_type_id(800)); // out of range
    }

    #[test]
    fn shared_path_types_returns_single_for_unique_path() {
        assert_eq!(
            ComponentType::PlasmaWidget.shared_path_types(),
            &[ComponentType::PlasmaWidget]
        );
        assert_eq!(
            ComponentType::KWinEffect.shared_path_types(),
            &[ComponentType::KWinEffect]
        );
        assert_eq!(
            ComponentType::IconTheme.shared_path_types(),
            &[ComponentType::IconTheme]
        );
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
