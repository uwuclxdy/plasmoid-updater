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
/// The store files wallpaper images under the "KDE Wallpaper (other)" category
/// (299), but KNewStuff's `wallpaper.knsrc` installs from six sibling
/// categories, and an entry carries the category id it was filed under.
/// Measured 2026-11-03 against `GET /content/categories`, where all six have an
/// empty `parent_id`: "KDE Wallpaper 800x600" (360), "1024x768" (296),
/// "1280x1024" (297), "1440x900" (298), "1600x1200" (307), "(other)" (299).
const CATEGORY_WALLPAPER_SIZES: [u16; 6] = [360, 296, 297, 298, 307, 299];
/// The store files cursor themes here; KNewStuff's `xcursor.knsrc` uses the
/// same id. The live name is "X11 Mouse Theme", displayed as "Cursors"
/// (measured 2026-11-02 against `GET /content/categories`).
const CATEGORY_CURSOR_THEME: u16 = 107;
/// The live name is "Plasma Comic", displayed as "Plasma Comic Sources"
/// (measured 2026-11-02 against `GET /content/categories`).
const CATEGORY_COMIC_PLUGIN: u16 = 155;

/// Type of KDE Plasma component.
///
/// Maps to KDE Store category IDs and determines installation paths,
/// registry files, and update strategies.
///
/// The enum is `#[non_exhaustive]`: match with a wildcard arm, so a component
/// type added later is not a breaking change.
#[non_exhaustive]
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
    /// A cursor theme (`index.theme` and `cursors/`, installed under
    /// `~/.icons`).
    CursorTheme,
    /// A Plasma Comic plugin (`Plasma/Comic`).
    ComicPlugin,
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
            Self::CursorTheme => CATEGORY_CURSOR_THEME,
            Self::ComicPlugin => CATEGORY_COMIC_PLUGIN,
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
            Self::CursorTheme => &[CATEGORY_CURSOR_THEME],
            Self::ComicPlugin => &[CATEGORY_COMIC_PLUGIN],
        }
    }

    /// The store category ids whose entries belong to this component type: the
    /// type's catalog category plus every category its own `.knsrc` installs
    /// from.
    ///
    /// A widget is filed under its widget subcategory ("Plasma 6 Clocks"
    /// carries 708, not 705), so every live widget subcategory 706-713 matches.
    /// The rest of the 700-range is other component kinds (715 wallpaper
    /// plugins, 716 splash screens, 717 window decorations, 722 global themes,
    /// 724 sysmon applet designs; measured 2026-11-02 against
    /// `GET /content/categories`). The 723 category lists only typeid 724
    /// entries, so no entry carries 723. A wallpaper carries whichever of
    /// `wallpaper.knsrc`'s six categories it was filed under.
    pub(crate) fn accepted_type_ids(self) -> &'static [u16] {
        match self {
            Self::PlasmaWidget => &[
                CATEGORY_PLASMA_WIDGET,
                706,
                707,
                708,
                709,
                710,
                711,
                712,
                CATEGORY_PLASMA_WIDGET_IMPROVEMENTS,
            ],
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
            Self::Wallpaper => &CATEGORY_WALLPAPER_SIZES,
            Self::CursorTheme => &[CATEGORY_CURSOR_THEME],
            Self::ComicPlugin => &[CATEGORY_COMIC_PLUGIN],
        }
    }

    /// Returns true if the given store `type_id` belongs to this component type:
    /// membership in [`accepted_type_ids`](Self::accepted_type_ids).
    pub(crate) fn matches_type_id(self, type_id: u16) -> bool {
        self.accepted_type_ids().contains(&type_id)
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
            Self::ComicPlugin => Some("Plasma/Comic"),
            Self::AuroraeDecoration
            | Self::ColorScheme
            | Self::SddmTheme
            | Self::IconTheme
            | Self::Wallpaper
            | Self::CursorTheme => None,
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
        matches!(
            self,
            Self::IconTheme | Self::Wallpaper | Self::ColorScheme | Self::CursorTheme
        )
    }

    /// Returns the component types whose registry files decide the type of a
    /// component found by scanning this type's directory: itself, plus every
    /// other directory-scanned type that scans the same path.
    ///
    /// `GlobalTheme` and `SplashScreen` both use `plasma/look-and-feel`.
    /// Discovery reads each returned type's registry map and assigns an entry
    /// the type whose registry names it. A registry-only type is discovered
    /// from its own registry file, so its list is never consulted, even where
    /// its path coincides with another type's (`IconTheme` and `CursorTheme`
    /// both use `/usr/share/icons`).
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
            Self::CursorTheme => &[Self::CursorTheme],
            Self::ComicPlugin => &[Self::ComicPlugin],
        }
    }

    // -- Filesystem paths --

    /// Returns the user-local data directory suffix, or `None` for system-only
    /// types (e.g. SDDM) and for types whose user path is not under a data
    /// directory (cursor themes live in `~/.icons`).
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
            Self::CursorTheme => None,
            Self::ComicPlugin => Some("plasma/comics"),
        }
    }

    /// Returns the full user-local installation path for this component type.
    pub fn user_path(self) -> PathBuf {
        // KNewStuff's `xcursor.knsrc` installs with `InstallPath=.icons`
        // (measured 2026-11-02), so a cursor theme lives under the home
        // directory rather than under a data directory.
        if self == Self::CursorTheme {
            return crate::paths::user_home().join(".icons");
        }
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
            Self::IconTheme | Self::CursorTheme => "/usr/share/icons",
            Self::Wallpaper => "/usr/share/wallpapers",
            Self::ComicPlugin => "/usr/share/plasma/comics",
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
            Self::CursorTheme => "cursor-themes",
            Self::ComicPlugin => "comic-plugins",
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
            Self::CursorTheme => Some("xcursor.knsregistry"),
            Self::ComicPlugin => Some("comic.knsregistry"),
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
            Self::CursorTheme,
            Self::ComicPlugin,
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
            Self::CursorTheme,
            Self::ComicPlugin,
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
            Self::CursorTheme => write!(f, "Cursor Theme"),
            Self::ComicPlugin => write!(f, "Comic Plugin"),
        }
    }
}

// -- Internal types --

/// A KDE component installed on the local system.
///
/// The struct is `#[non_exhaustive]`: build it with
/// [`InstalledComponent::new`], never a struct literal from outside this crate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
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
    /// Builds a user component from what it is and where it lives.
    ///
    /// The version starts at `"0.0.0"` (unknown) and the release date empty;
    /// set them, and whether the component is system-wide, with
    /// [`with_version`](Self::with_version),
    /// [`with_release_date`](Self::with_release_date) and
    /// [`with_system`](Self::with_system).
    ///
    /// # Examples
    ///
    /// ```
    /// use libplasmoid_updater::{ComponentType, InstalledComponent};
    ///
    /// let component = InstalledComponent::new(
    ///     "Fake Scheme",
    ///     "FakeScheme.colors",
    ///     ComponentType::ColorScheme,
    ///     "/tmp/FakeScheme.colors",
    /// )
    /// .with_version("1.0")
    /// .with_system(true)
    /// .with_release_date("2026-01-01");
    /// assert_eq!(component.version, "1.0");
    /// assert!(component.is_system);
    /// assert_eq!(component.release_date, "2026-01-01");
    /// ```
    pub fn new(
        name: impl Into<String>,
        directory_name: impl Into<String>,
        component_type: ComponentType,
        path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            name: name.into(),
            directory_name: directory_name.into(),
            version: "0.0.0".to_string(),
            component_type,
            path: path.into(),
            is_system: false,
            release_date: String::new(),
        }
    }

    /// Sets the installed version recorded in the metadata or the KNewStuff
    /// registry.
    ///
    /// # Examples
    ///
    /// ```
    /// use libplasmoid_updater::{ComponentType, InstalledComponent};
    ///
    /// let component = InstalledComponent::new(
    ///     "Fake Scheme",
    ///     "FakeScheme.colors",
    ///     ComponentType::ColorScheme,
    ///     "/tmp/FakeScheme.colors",
    /// )
    /// .with_version("1.0");
    /// assert_eq!(component.version, "1.0");
    /// ```
    #[must_use]
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    /// Sets whether the component lives in a system-wide directory.
    ///
    /// # Examples
    ///
    /// ```
    /// use libplasmoid_updater::{ComponentType, InstalledComponent};
    ///
    /// let component = InstalledComponent::new(
    ///     "Fake Scheme",
    ///     "FakeScheme.colors",
    ///     ComponentType::ColorScheme,
    ///     "/usr/share/color-schemes/FakeScheme.colors",
    /// )
    /// .with_system(true);
    /// assert!(component.is_system);
    /// ```
    #[must_use]
    pub fn with_system(mut self, is_system: bool) -> Self {
        self.is_system = is_system;
        self
    }

    /// Sets the release date the store or the KNewStuff registry records
    /// (`YYYY-MM-DD` when known).
    ///
    /// # Examples
    ///
    /// ```
    /// use libplasmoid_updater::{ComponentType, InstalledComponent};
    ///
    /// let component = InstalledComponent::new(
    ///     "Fake Scheme",
    ///     "FakeScheme.colors",
    ///     ComponentType::ColorScheme,
    ///     "/tmp/FakeScheme.colors",
    /// )
    /// .with_release_date("2026-01-01");
    /// assert_eq!(component.release_date, "2026-01-01");
    /// ```
    #[must_use]
    pub fn with_release_date(mut self, release_date: impl Into<String>) -> Self {
        self.release_date = release_date.into();
        self
    }

    /// The installed path as a string: unique per component within a run,
    /// unlike the directory name, so progress events key their rows on it.
    pub(crate) fn event_id(&self) -> String {
        self.path.display().to_string()
    }
}

/// An available update for an installed component, with download metadata.
///
/// The struct is `#[non_exhaustive]`: build it with
/// [`AvailableUpdate::builder`], never a struct literal from outside this
/// crate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
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
///
/// The struct is `#[non_exhaustive]`: callers read its fields, and the
/// [`check`](crate::check) (in [`CheckResult::unavailable`](crate::CheckResult::unavailable)),
/// [`update`](crate::update) and [`install`](crate::install) runs produce the
/// rows. A caller never builds one.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
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
    /// The update carries no usable store metadata: the store entry had no
    /// download link matching its own version, or the selected update's
    /// download URL is empty.
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

/// The KDE Store page URL for a store content id.
pub(crate) fn store_page_url(content_id: u64) -> String {
    format!("https://store.kde.org/p/{content_id}")
}

/// Builds an [`AvailableUpdate`], the remaining store fields unset until their
/// setter fills them.
///
/// Start from [`AvailableUpdate::builder`], which takes the installed
/// component, the store content id and the version the store advertises.
///
/// # Examples
///
/// ```
/// use libplasmoid_updater::{AvailableUpdate, ComponentType, InstalledComponent};
///
/// let builder = AvailableUpdate::builder(
///     InstalledComponent::new(
///         "Fake Scheme",
///         "FakeScheme.colors",
///         ComponentType::ColorScheme,
///         "/tmp/FakeScheme.colors",
///     )
///     .with_version("1.0"),
///     42,
///     "2.0",
/// );
/// let copied = builder.clone();
/// assert!(format!("{copied:?}").contains("FakeScheme.colors"));
///
/// let update = builder
///     .download_url("https://example.org/FakeScheme.tar.gz")
///     .release_date("2026-01-01")
///     .download_size(Some(1024))
///     .build();
///
/// assert_eq!(update.latest_version, "2.0");
/// assert_eq!(update.store_url, "https://store.kde.org/p/42");
/// assert_eq!(update.download_size, Some(1024));
/// ```
#[derive(Debug, Clone)]
#[must_use]
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
    /// Sets the URL the archive or file downloads from, empty when unset.
    pub fn download_url(mut self, download_url: impl Into<String>) -> Self {
        self.download_url = download_url.into();
        self
    }

    /// Sets the store change date of the newer entry, empty when unset.
    pub fn release_date(mut self, release_date: impl Into<String>) -> Self {
        self.release_date = release_date.into();
        self
    }

    /// Sets the checksum the store advertises for this version.
    pub fn checksum(mut self, checksum: Option<String>) -> Self {
        self.checksum = checksum;
        self
    }

    /// Sets the advertised download size in bytes.
    pub fn download_size(mut self, size: Option<u64>) -> Self {
        self.download_size = size;
        self
    }

    /// Builds the update, filling in the store page URL from the content id.
    #[must_use]
    pub fn build(self) -> AvailableUpdate {
        AvailableUpdate {
            installed: self.installed,
            content_id: self.content_id,
            latest_version: self.latest_version,
            download_url: self.download_url,
            store_url: store_page_url(self.content_id),
            release_date: self.release_date,
            checksum: self.checksum,
            download_size: self.download_size,
        }
    }
}

impl AvailableUpdate {
    /// Starts building an update for an installed component.
    ///
    /// The third argument is the version the store advertises, empty when the
    /// store entry names none. It is required here, not a setter, so a caller
    /// cannot leave it out by accident. The remaining store fields have named
    /// setters.
    ///
    /// # Examples
    ///
    /// ```
    /// use libplasmoid_updater::{AvailableUpdate, ComponentType, InstalledComponent};
    ///
    /// let update = AvailableUpdate::builder(
    ///     InstalledComponent::new(
    ///         "Fake Scheme",
    ///         "FakeScheme.colors",
    ///         ComponentType::ColorScheme,
    ///         "/tmp/FakeScheme.colors",
    ///     )
    ///     .with_version("1.0"),
    ///     42,
    ///     "2.0",
    /// )
    /// .download_url("https://example.org/FakeScheme.tar.gz")
    /// .build();
    /// assert_eq!(update.store_url, "https://store.kde.org/p/42");
    /// assert_eq!(update.latest_version, "2.0");
    /// ```
    ///
    /// Omitting the version does not compile:
    ///
    /// ```compile_fail
    /// use std::path::PathBuf;
    ///
    /// use libplasmoid_updater::{AvailableUpdate, ComponentType, InstalledComponent};
    ///
    /// let update = AvailableUpdate::builder(
    ///     InstalledComponent::new(
    ///         "Fake Scheme".to_string(),
    ///         "FakeScheme.colors".to_string(),
    ///         ComponentType::ColorScheme,
    ///         PathBuf::from("/tmp/FakeScheme.colors"),
    ///     ),
    ///     42,
    /// )
    /// .build();
    /// ```
    pub fn builder(
        installed: InstalledComponent,
        content_id: u64,
        latest_version: impl Into<String>,
    ) -> AvailableUpdateBuilder {
        AvailableUpdateBuilder {
            installed,
            content_id,
            latest_version: latest_version.into(),
            download_url: String::new(),
            release_date: String::new(),
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
///
/// The struct is `#[non_exhaustive]`: callers read its fields, and only
/// [`check`](crate::check) produces them. [`update`](crate::update) returns no
/// diagnostics: it folds every component it could not check into
/// [`UpdateResult::skipped`](crate::UpdateResult::skipped) names.
/// [`install`](crate::install) checks nothing, so its `skipped` is empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Diagnostic {
    /// Display name of the component. Same-named components exist (two
    /// installed color schemes can both be `Breeze`), and two component types
    /// can share one directory under `--system` with different names, so match
    /// a component by [`path`](Self::path) and this name.
    pub name: String,
    /// Install path of the component the diagnostic is about:
    /// [`InstalledComponent::path`], the same spelling as the reporter's
    /// event `id`. A component a killed run stranded is named by the path it
    /// is restored to.
    #[serde(with = "pathbuf_serde", default)]
    pub path: PathBuf,
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
    pub(crate) fn new(name: String, path: PathBuf, reason: String) -> Self {
        Self {
            name,
            path,
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
    /// Names of components the run does not attempt: excluded by the
    /// configuration's exclusion rules, and — in a system run — components
    /// with no matching store entry or a store entry of a different component
    /// type.
    pub skipped: Vec<String>,
    /// The components behind the store-entry skips in `skipped`.
    pub unmatched: Vec<InstalledComponent>,
}

impl UpdateCheckResult {
    /// Skips a component no store entry of its type matches: its name joins
    /// `skipped` and the component itself `unmatched`.
    pub fn skip_unmatched(&mut self, component: &InstalledComponent) {
        self.skipped.push(component.name.clone());
        self.unmatched.push(component.clone());
    }

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
    fn the_store_page_url_is_the_public_page_for_the_content_id() {
        assert_eq!(store_page_url(2380745), "https://store.kde.org/p/2380745");
    }

    #[test]
    fn a_wallpaper_matches_every_category_wallpaper_knsrc_installs_from() {
        assert_eq!(
            ComponentType::Wallpaper.accepted_type_ids(),
            &[360, 296, 297, 298, 307, 299][..]
        );
    }

    #[test]
    fn a_cursor_theme_and_an_icon_theme_do_not_match_each_other() {
        assert!(ComponentType::CursorTheme.matches_type_id(107));
        assert!(ComponentType::IconTheme.matches_type_id(132));
        assert!(!ComponentType::CursorTheme.matches_type_id(132));
        assert!(!ComponentType::IconTheme.matches_type_id(107));
        assert!(ComponentType::ComicPlugin.matches_type_id(155));
        assert!(!ComponentType::ComicPlugin.matches_type_id(107));
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
            diagnostic: Diagnostic::new(
                "Widget".to_string(),
                PathBuf::from("/plasmoids/org.example.widget"),
                NO_DOWNLOAD_URL_REASON.to_string(),
            )
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

    const CURSOR_PATH_CHILD: &str = "PLASMOID_UPDATER_CURSOR_PATH_TEST_CHILD";
    const CURSOR_PATH_MARKER: &str = "PLASMOID_UPDATER_CURSOR_PATH_TEST_MARKER";

    /// A cursor theme is installed by KNewStuff's `xcursor.knsrc`, whose
    /// `InstallPath=.icons` resolves against the home directory, not against an
    /// XDG data directory. The probe runs in a child process because setting
    /// `HOME` in-process needs `unsafe`.
    #[test]
    fn a_cursor_theme_installs_under_the_home_dot_icons_directory() {
        if std::env::var_os(CURSOR_PATH_CHILD).is_some() {
            let home = PathBuf::from(std::env::var("HOME").unwrap());
            let data = PathBuf::from(std::env::var("XDG_DATA_HOME").unwrap());
            assert_eq!(
                ComponentType::IconTheme.user_path(),
                data.join("icons"),
                "the control: an icon theme lives under XDG_DATA_HOME"
            );
            assert_eq!(
                ComponentType::CursorTheme.user_path(),
                home.join(".icons"),
                "a cursor theme must install under ~/.icons"
            );
            std::fs::write(std::env::var(CURSOR_PATH_MARKER).unwrap(), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let home = root.join("home");
        let data = root.join("data");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let marker = root.join("child-done");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "types::tests::a_cursor_theme_installs_under_the_home_dot_icons_directory",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &home)
            .env("XDG_DATA_HOME", &data)
            .env(CURSOR_PATH_CHILD, "1")
            .env(CURSOR_PATH_MARKER, &marker)
            .output()
            .unwrap();
        assert!(
            output.status.success() && marker.exists(),
            "child failed ({}; body completed: {})\n--- stdout\n{}\n--- stderr\n{}",
            output.status,
            marker.exists(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
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
