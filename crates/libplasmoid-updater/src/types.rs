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

    /// returns true if this type uses registry-based discovery only.
    pub const fn registry_only(self) -> bool {
        matches!(self, Self::IconTheme | Self::Wallpaper | Self::ColorScheme)
    }

    pub fn user_path(self) -> PathBuf {
        let data_home = std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".local/share"));

        match self {
            Self::PlasmaWidget => data_home.join("plasma/plasmoids"),
            Self::WallpaperPlugin => data_home.join("plasma/wallpapers"),
            Self::KWinEffect => data_home.join("kwin/effects"),
            Self::KWinScript => data_home.join("kwin/scripts"),
            Self::KWinSwitcher => data_home.join("kwin/tabbox"),
            Self::GlobalTheme => data_home.join("plasma/look-and-feel"),
            Self::PlasmaStyle => data_home.join("plasma/desktoptheme"),
            Self::AuroraeDecoration => data_home.join("aurorae/themes"),
            Self::ColorScheme => data_home.join("color-schemes"),
            Self::SplashScreen => data_home.join("plasma/look-and-feel"),
            Self::SddmTheme => PathBuf::new(),
            Self::IconTheme => data_home.join("icons"),
            Self::Wallpaper => data_home.join("wallpapers"),
        }
    }

    pub fn system_path(self) -> PathBuf {
        match self {
            Self::PlasmaWidget => PathBuf::from("/usr/share/plasma/plasmoids"),
            Self::WallpaperPlugin => PathBuf::from("/usr/share/plasma/wallpapers"),
            Self::KWinEffect => PathBuf::from("/usr/share/kwin/effects"),
            Self::KWinScript => PathBuf::from("/usr/share/kwin/scripts"),
            Self::KWinSwitcher => PathBuf::from("/usr/share/kwin/tabbox"),
            Self::GlobalTheme => PathBuf::from("/usr/share/plasma/look-and-feel"),
            Self::PlasmaStyle => PathBuf::from("/usr/share/plasma/desktoptheme"),
            Self::AuroraeDecoration => PathBuf::from("/usr/share/aurorae/themes"),
            Self::ColorScheme => PathBuf::from("/usr/share/color-schemes"),
            Self::SplashScreen => PathBuf::from("/usr/share/plasma/look-and-feel"),
            Self::SddmTheme => PathBuf::from("/usr/share/sddm/themes"),
            Self::IconTheme => PathBuf::from("/usr/share/icons"),
            Self::Wallpaper => PathBuf::from("/usr/share/wallpapers"),
        }
    }

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

impl AvailableUpdate {
    pub fn new(
        installed: InstalledComponent,
        content_id: u64,
        latest_version: String,
        download_url: String,
        release_date: String,
    ) -> Self {
        let store_url = format!("https://store.kde.org/p/{content_id}");
        Self {
            installed,
            content_id,
            latest_version,
            download_url,
            store_url,
            release_date,
            checksum: None,
            download_size: None,
        }
    }

    pub fn with_checksum(mut self, checksum: Option<String>) -> Self {
        self.checksum = checksum;
        self
    }

    pub fn with_download_size(mut self, size: Option<u64>) -> Self {
        self.download_size = size;
        self
    }
}

#[derive(Debug, Clone)]
pub struct StoreEntry {
    pub id: u64,
    pub name: String,
    pub version: String,
    pub type_id: u16,
    pub download_links: Vec<DownloadLink>,
    pub changed_date: String,
}

#[derive(Debug, Clone)]
pub struct DownloadLink {
    pub url: String,
    pub version: String,
    pub checksum: Option<String>,
    pub size_kb: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PackageMetadata {
    #[serde(rename = "KPlugin")]
    pub kplugin: Option<KPluginInfo>,
    #[serde(rename = "KPackageStructure")]
    pub kpackage_structure: Option<String>,
}

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

    pub fn total_processed(&self) -> usize {
        self.succeeded.len() + self.failed.len() + self.skipped.len()
    }

    pub fn exit_code(&self) -> i32 {
        if self.has_failures() { 1 } else { 0 }
    }

    pub fn merge(&mut self, other: UpdateSummary) {
        self.succeeded.extend(other.succeeded);
        self.failed.extend(other.failed);
        self.skipped.extend(other.skipped);
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CheckResult {
    pub updates: Vec<AvailableUpdate>,
    pub unresolved: Vec<(String, String)>,
    pub check_failures: Vec<(String, String)>,
}

impl CheckResult {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_update(&mut self, update: AvailableUpdate) {
        self.updates.push(update);
    }

    pub fn add_unresolved(&mut self, name: String, reason: String) {
        self.unresolved.push((name, reason));
    }

    pub fn add_check_failure(&mut self, name: String, reason: String) {
        self.check_failures.push((name, reason));
    }

    pub fn has_issues(&self) -> bool {
        !self.unresolved.is_empty() || !self.check_failures.is_empty()
    }

    pub fn merge(&mut self, other: CheckResult) {
        self.updates.extend(other.updates);
        self.unresolved.extend(other.unresolved);
        self.check_failures.extend(other.check_failures);
    }
}

/// JSON output structures for CLI
#[derive(Debug, Serialize)]
pub struct JsonOutput<T> {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
}

impl<T> JsonOutput<T> {
    pub fn ok(data: T) -> Self {
        Self {
            success: true,
            error: None,
            data: Some(data),
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            success: false,
            error: Some(msg.into()),
            data: None,
        }
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
