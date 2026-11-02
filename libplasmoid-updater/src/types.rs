// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// KDE Store category IDs for supported component types.
const CATEGORY_PLASMA_WIDGET: u16 = 705;
/// The store's "Various Plasma 6 Improvements" widget subcategory. The 705
/// listing does not expand to it, so a catalog query has to name it.
const CATEGORY_PLASMA_WIDGET_IMPROVEMENTS: u16 = 713;
const CATEGORY_WALLPAPER_PLUGIN: u16 = 715;
const CATEGORY_KWIN_EFFECT: u16 = 719;
const CATEGORY_KWIN_SCRIPT: u16 = 720;
const CATEGORY_KWIN_SWITCHER: u16 = 721;
const CATEGORY_GLOBAL_THEME: u16 = 722;
const CATEGORY_PLASMA_STYLE: u16 = 104;
const CATEGORY_AURORAE_DECORATION: u16 = 114;
const CATEGORY_COLOR_SCHEME: u16 = 112;
const CATEGORY_SPLASH_SCREEN: u16 = 716;
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
    /// A Plasma applet (`metadata.json` `Plasma/Applet`).
    PlasmaWidget,
    /// A Plasma wallpaper plugin (`Plasma/Wallpaper`).
    WallpaperPlugin,
    /// A KWin effect (`KWin/Effect`).
    #[serde(rename = "kwin_effect")]
    KWinEffect,
    /// A KWin script (`KWin/Script`).
    #[serde(rename = "kwin_script")]
    KWinScript,
    /// A KWin window switcher (`KWin/WindowSwitcher`).
    #[serde(rename = "kwin_switcher")]
    KWinSwitcher,
    /// A global theme (`Plasma/LookAndFeel`).
    GlobalTheme,
    /// A Plasma desktop theme (`Plasma/Theme`).
    PlasmaStyle,
    /// An Aurorae window decoration theme.
    AuroraeDecoration,
    /// A colour scheme file (`.colors`).
    ColorScheme,
    /// A Plasma splash screen (`Plasma/LookAndFeel`).
    SplashScreen,
    /// An SDDM login theme (system-wide only).
    SddmTheme,
    /// An icon theme.
    IconTheme,
    /// A wallpaper package or image.
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

    /// The store categories a catalog query must name to list every entry of
    /// this type.
    ///
    /// Entries are filed under their subcategory id, and the store's 705
    /// listing expands to 706-712 only: entries filed under 713 need naming
    /// on their own (measured 2026-11-02 against `GET /content/data`).
    pub(crate) const fn catalog_ids(self) -> &'static [u16] {
        match self {
            Self::PlasmaWidget => &[CATEGORY_PLASMA_WIDGET, CATEGORY_PLASMA_WIDGET_IMPROVEMENTS],
            Self::WallpaperPlugin => &[CATEGORY_WALLPAPER_PLUGIN],
            Self::KWinEffect => &[CATEGORY_KWIN_EFFECT],
            Self::KWinScript => &[CATEGORY_KWIN_SCRIPT],
            Self::KWinSwitcher => &[CATEGORY_KWIN_SWITCHER],
            Self::GlobalTheme => &[CATEGORY_GLOBAL_THEME],
            Self::PlasmaStyle => &[CATEGORY_PLASMA_STYLE],
            Self::AuroraeDecoration => &[CATEGORY_AURORAE_DECORATION],
            Self::ColorScheme => &[CATEGORY_COLOR_SCHEME],
            Self::SplashScreen => &[CATEGORY_SPLASH_SCREEN],
            Self::SddmTheme => &[CATEGORY_SDDM_THEME],
            Self::IconTheme => &[CATEGORY_ICON_THEME],
            Self::Wallpaper => &[CATEGORY_WALLPAPER],
        }
    }

    /// Returns true if the given store `type_id` belongs to this component type.
    ///
    /// The OCS API returns the subcategory id in an entry's `typeid` field, so
    /// a widget filed under "Plasma 6 Clocks" carries 708, not 705. Only the
    /// live widget subcategories 706-713 match: the rest of the 700-range is
    /// other component kinds (715 wallpaper plugins, 716 splash screens, 717
    /// window decorations, 722 global themes, 724 sysmon applet designs;
    /// measured 2026-11-02 against `GET /content/categories`). The 723
    /// category lists only typeid 724 entries, so no entry carries 723.
    pub(crate) const fn matches_type_id(self, type_id: u16) -> bool {
        if self.category_id() == type_id {
            return true;
        }
        matches!((self, type_id), (Self::PlasmaWidget, 706..=713))
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

    /// Every supported component type, system- and user-local ones alike.
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

    /// The component types a user-local scan covers ([`all`](Self::all)
    /// without the system-only types).
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
    /// Display name from the component's metadata, or the directory name when
    /// the metadata has none.
    pub name: String,
    /// Directory (or, for single-file components, file) name on disk. This is
    /// what the widgets-id table keys on and what most store entries match.
    pub directory_name: String,
    /// Installed version as recorded in the metadata or the KNewStuff
    /// registry; `"0.0.0"` when unknown.
    pub version: String,
    /// Which kind of Plasma component this is.
    pub component_type: ComponentType,
    /// Root directory of the component, or the file itself for single-file
    /// components such as color schemes.
    #[serde(with = "pathbuf_serde")]
    pub path: PathBuf,
    /// `true` when the component lives in a system-wide directory.
    pub is_system: bool,
    /// Release date as recorded by the store or the KNewStuff registry: an
    /// ISO 8601 date (`YYYY-MM-DD`) when known, empty otherwise.
    pub release_date: String,
}

impl InstalledComponent {
    /// The installed path as a string: unique per component within a run,
    /// unlike the directory name, so progress events key their rows on it.
    pub(crate) fn event_id(&self) -> String {
        self.path.display().to_string()
    }
}

/// An available update for an installed component, with download metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailableUpdate {
    /// The component this update would replace.
    pub installed: InstalledComponent,
    /// KDE Store content id of the newer entry.
    pub content_id: u64,
    /// Version the store advertises.
    pub latest_version: String,
    /// URL the archive or file downloads from.
    pub download_url: String,
    /// Human-facing store page (`https://store.kde.org/p/<id>`).
    pub store_url: String,
    /// Store change date of the newer entry (ISO 8601 when known).
    pub release_date: String,
    /// MD5 checksum the store advertises, when it advertises one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    /// Advertised download size in bytes, when advertised.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_size: Option<u64>,
}

/// One component that failed inside an otherwise successful run.
#[derive(Debug, Clone, Serialize)]
pub struct Failure {
    /// The component the failure is about.
    pub component: InstalledComponent,
    /// Which stage failed.
    pub kind: FailureKind,
    /// Human-readable description of what went wrong.
    pub message: String,
}

/// Which stage of an install failed.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// The store entry had no download link matching its own version.
    NoMatchingDownload,
    /// Downloading the archive or file failed.
    Download,
    /// The downloaded archive's checksum did not match the advertised one.
    Checksum,
    /// Extracting the downloaded archive failed.
    Extract,
    /// Installing the extracted files (kpackagetool6, direct copy) failed.
    Install,
    /// The install failed and restoring the backup failed too, so the
    /// component may be left in a partial state.
    RestoreFailed,
    /// The files were installed, but the post-install version could not be
    /// verified to match the expected version. The component still counts
    /// toward [`UpdateResult::needs_restart`](crate::UpdateResult::needs_restart).
    Unverified,
    /// The files were installed, but the KNewStuff registry could not be
    /// rewritten, so Discover still records the previous version. The
    /// component still counts toward
    /// [`UpdateResult::needs_restart`](crate::UpdateResult::needs_restart).
    Registry,
}

impl std::fmt::Display for FailureKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            Self::NoMatchingDownload => "no matching download",
            Self::Download => "download",
            Self::Checksum => "checksum",
            Self::Extract => "extract",
            Self::Install => "install",
            Self::RestoreFailed => "restore",
            Self::Unverified => "unverified",
            Self::Registry => "registry",
        };
        f.write_str(label)
    }
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
    /// `<personid>` in the OCS payload: the uploader's store username.
    pub author: String,
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
    #[serde(rename = "Authors", default)]
    pub authors: Vec<KPluginAuthor>,
}

/// One `KPlugin.Authors` entry of a component's `metadata.json`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct KPluginAuthor {
    #[serde(rename = "Name")]
    pub name: Option<String>,
}

impl PackageMetadata {
    pub(crate) fn name(&self) -> Option<&str> {
        self.kplugin.as_ref()?.name.as_deref()
    }

    pub(crate) fn version(&self) -> Option<&str> {
        self.kplugin.as_ref()?.version.as_deref()
    }

    /// The author names the metadata records, trimmed and without empties.
    pub(crate) fn author_names(&self) -> Vec<String> {
        let Some(kplugin) = &self.kplugin else {
            return Vec::new();
        };
        kplugin
            .authors
            .iter()
            .filter_map(|author| author.name.as_deref())
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect()
    }
}

/// Diagnostic information about a component that could not be checked or updated.
///
/// Returned as part of [`CheckResult::diagnostics`](crate::CheckResult::diagnostics).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Display name of the component.
    pub name: String,
    /// Why the component could not be checked.
    pub reason: String,
    /// Installed version when one could be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    /// Version the store advertises, when the entry was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_version: Option<String>,
    /// Store content id, when one was resolved.
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

/// The diagnostic reason a component whose store entry offers no download link
/// for its version carries. [`crate::CheckResult::unavailable_diagnostics`]
/// keys on it.
pub(crate) const NO_DOWNLOAD_URL_REASON: &str = "no download url available";

/// A component whose store entry offered no download link for its version.
///
/// It is neither unresolved nor skipped: `check` reports the diagnostic and
/// `update` reports a failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct NoMatchingDownload {
    pub component: InstalledComponent,
    pub diagnostic: Diagnostic,
}

impl NoMatchingDownload {
    /// The failure row a caller reports for this component, naming the version
    /// the store entry carried.
    pub(crate) fn into_failure(self) -> Failure {
        let version = self
            .diagnostic
            .available_version
            .as_deref()
            .unwrap_or("(unknown)");
        Failure {
            component: self.component,
            kind: FailureKind::NoMatchingDownload,
            message: format!("no download link matches the store entry's version {version:?}"),
        }
    }
}

/// Internal result of checking for available updates, including diagnostics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct UpdateCheckResult {
    pub updates: Vec<AvailableUpdate>,
    pub unresolved: Vec<Diagnostic>,
    pub check_failures: Vec<Diagnostic>,
    /// Components whose store entry offers no download link for its version, so
    /// `update` reports a failure rather than skipping them.
    pub no_matching_download: Vec<NoMatchingDownload>,
    /// Names of components dropped by the configuration's exclusion rules.
    pub skipped: Vec<String>,
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

    pub fn add_no_matching_download(
        &mut self,
        component: InstalledComponent,
        diagnostic: Diagnostic,
    ) {
        self.no_matching_download.push(NoMatchingDownload {
            component,
            diagnostic,
        });
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
    fn plasma_widget_matches_its_live_subcategories() {
        for id in [705, 706, 707, 708, 709, 710, 711, 712, 713] {
            assert!(
                ComponentType::PlasmaWidget.matches_type_id(id),
                "widget type id {id}"
            );
        }
        assert!(!ComponentType::PlasmaWidget.matches_type_id(100));
        assert!(!ComponentType::PlasmaWidget.matches_type_id(800));
    }

    #[test]
    fn plasma_widget_does_not_match_other_component_types() {
        // Other kinds sharing the 700-range (measured 2026-11-02 against
        // GET /content/categories; 714 is not listed there).
        for id in [714, 715, 716, 722, 723, 724] {
            assert!(
                !ComponentType::PlasmaWidget.matches_type_id(id),
                "widget type id {id}"
            );
        }
        assert!(!ComponentType::PlasmaWidget.matches_type_id(717)); // window decorations
        assert!(!ComponentType::PlasmaWidget.matches_type_id(719)); // kwin effects
    }

    #[test]
    fn category_ids_match_the_live_store() {
        assert_eq!(ComponentType::PlasmaWidget.category_id(), 705);
        assert_eq!(ComponentType::PlasmaStyle.category_id(), 104);
        assert_eq!(ComponentType::SplashScreen.category_id(), 716);
        assert_eq!(ComponentType::GlobalTheme.category_id(), 722);
    }

    #[test]
    fn the_no_match_failure_reprs_the_store_version() {
        let entry = NoMatchingDownload {
            component: InstalledComponent {
                name: "Widget".to_string(),
                directory_name: "org.example.widget".to_string(),
                version: "1.0".to_string(),
                component_type: ComponentType::PlasmaWidget,
                path: PathBuf::from("/plasmoids/org.example.widget"),
                is_system: false,
                release_date: String::new(),
            },
            diagnostic: Diagnostic::new("Widget".to_string(), NO_DOWNLOAD_URL_REASON.to_string())
                .with_versions(Some("1.0".to_string()), Some("2.0".to_string())),
        };

        let failure = entry.into_failure();

        assert_eq!(failure.kind, FailureKind::NoMatchingDownload);
        assert_eq!(
            failure.message,
            "no download link matches the store entry's version \"2.0\""
        );
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
