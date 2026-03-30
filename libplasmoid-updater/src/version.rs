// SPDX-License-Identifier: GPL-3.0-or-later

use versions::Versioning;

/// Normalizes a version string for more robust parsing.
///
/// Mirrors apdatifier's `clearVer()` approach:
/// - Strips leading non-numeric/non-dot prefix (e.g., "v", "Version ")
/// - Replaces non-alphanumeric separators with dots
/// - Collapses consecutive dots
/// - Strips leading/trailing dots
pub(crate) fn normalize_version(version: &str) -> String {
    if version.is_empty() {
        return String::new();
    }

    // Find where the version number actually starts
    let start = version
        .find(|c: char| c.is_ascii_digit())
        .unwrap_or(version.len());

    let trimmed = &version[start..];
    if trimmed.is_empty() {
        return String::new();
    }

    let mut result = String::with_capacity(trimmed.len());
    let mut last_was_dot = false;

    for c in trimmed.chars() {
        if c.is_ascii_alphanumeric() {
            last_was_dot = false;
            result.push(c);
        } else {
            // Replace any non-alphanumeric with dot, but collapse consecutive dots
            if !last_was_dot && !result.is_empty() {
                result.push('.');
                last_was_dot = true;
            }
        }
    }

    // Strip trailing dot
    if result.ends_with('.') {
        result.pop();
    }

    result
}

/// Returns true if there's an update based on version or date.
///
/// Mirrors KNewStuff's update detection: an update is available when the
/// version string differs OR the release date differs. When both versions
/// are parseable we use semantic comparison (newer = update); when they
/// are equal we fall back to date comparison to catch "refresh" uploads
/// where the author re-uploads the same version with a newer date.
pub(crate) fn is_update_available_with_date(
    installed_version: &str,
    available_version: &str,
    installed_date: &str,
    available_date: &str,
) -> bool {
    // Fast path: identical raw strings
    if !installed_version.is_empty()
        && !available_version.is_empty()
        && installed_version == available_version
    {
        return is_date_newer(installed_date, available_date);
    }

    // Try parsing originals first (preserves pre-release semantics)
    let inst_orig = Versioning::new(installed_version);
    let avail_orig = Versioning::new(available_version);

    if let (Some(inst), Some(avail)) = (&inst_orig, &avail_orig) {
        if inst < avail {
            return true;
        }
        if inst == avail {
            return is_date_newer(installed_date, available_date);
        }
        return false;
    }

    // Fall back to normalized comparison
    let inst_norm = normalize_version(installed_version);
    let avail_norm = normalize_version(available_version);

    if !inst_norm.is_empty() && !avail_norm.is_empty() && inst_norm == avail_norm {
        return is_date_newer(installed_date, available_date);
    }

    let inst_parsed = Versioning::new(&inst_norm);
    let avail_parsed = Versioning::new(&avail_norm);

    if let (Some(inst), Some(avail)) = (&inst_parsed, &avail_parsed) {
        if inst < avail {
            return true;
        }
        if inst == avail {
            return is_date_newer(installed_date, available_date);
        }
        return false;
    }

    if avail_parsed.is_some() && inst_parsed.is_none() {
        return true;
    }

    // Both unparseable and differ: we can't determine ordering,
    // so fall through to date comparison instead of assuming update.
    is_date_newer(installed_date, available_date)
}

/// Returns true if `available_date` is strictly newer than `installed_date`.
fn is_date_newer(installed_date: &str, available_date: &str) -> bool {
    if installed_date.is_empty() || available_date.is_empty() {
        return false;
    }
    // extract just the date part (first 10 chars for YYYY-MM-DD)
    let local_date = installed_date.get(..10).unwrap_or(installed_date);
    let store_date = available_date.get(..10).unwrap_or(available_date);
    store_date > local_date
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_v_prefix() {
        assert_eq!(normalize_version("v1.2.3"), "1.2.3");
    }

    #[test]
    fn normalize_strips_non_numeric_prefix() {
        assert_eq!(normalize_version("Version 2.0"), "2.0");
    }

    #[test]
    fn normalize_preserves_semver() {
        assert_eq!(normalize_version("1.2.3"), "1.2.3");
    }

    #[test]
    fn normalize_handles_date_format() {
        assert_eq!(normalize_version("2024.01.15"), "2024.01.15");
    }

    #[test]
    fn normalize_collapses_dots() {
        assert_eq!(normalize_version("1..2...3"), "1.2.3");
    }

    #[test]
    fn normalize_replaces_non_numeric_separators() {
        assert_eq!(normalize_version("1.2.3-beta1"), "1.2.3.beta1");
    }

    #[test]
    fn normalize_empty() {
        assert_eq!(normalize_version(""), "");
    }

    #[test]
    fn normalized_versions_detect_update() {
        assert!(is_update_available_with_date("v1.0", "v2.0", "", ""));
    }

    #[test]
    fn normalized_versions_no_update_when_equal() {
        assert!(!is_update_available_with_date("v1.0.0", "v1.0.0", "", ""));
    }

    #[test]
    fn date_comparison_does_not_panic_on_non_ascii() {
        // Multi-byte chars before byte 10 — must not panic.
        // "2024-é1-01" has é (2 bytes) making it 11 bytes; get(..10) truncates
        // to "2024-é1-0" which is still lexicographically < "2025-01-01"
        assert!(is_date_newer("2024-é1-01", "2025-01-01"));
        // "2025-ñ1-01" similarly truncates; year 2025 > 2024 so still newer
        assert!(is_date_newer("2024-01-01", "2025-ñ1-01"));
        // All multi-byte: ë (0xc3..) > '2' (0x32) so local_date > store_date
        assert!(!is_date_newer("ëëëëëëëëëë", "2025-01-01"));
    }

    #[test]
    fn unparseable_versions_fall_back_to_date_comparison() {
        // Use strings that Versioning truly cannot parse (contain spaces)
        // and normalize to empty (no digits), so both paths yield None.
        // Both unparseable, differ, installed date newer → no update
        assert!(!is_update_available_with_date(
            "!@#",
            "***",
            "2025-06-01",
            "2024-01-01"
        ));
        // Both unparseable, differ, available date newer → update
        assert!(is_update_available_with_date(
            "!@#",
            "***",
            "2024-01-01",
            "2025-06-01"
        ));
        // Both unparseable, differ, no dates → no update (conservative)
        assert!(!is_update_available_with_date("!@#", "***", "", ""));
    }

    #[test]
    fn prerelease_detected_as_update_to_release() {
        // 1.2.3-beta1 < 1.2.3 (pre-release is older than release)
        assert!(is_update_available_with_date(
            "1.2.3-beta1",
            "1.2.3",
            "",
            ""
        ));
    }

    #[test]
    fn release_not_downgraded_to_prerelease() {
        // 1.2.3 > 1.2.3-beta1 (release is newer than pre-release)
        assert!(!is_update_available_with_date(
            "1.2.3",
            "1.2.3-beta1",
            "",
            ""
        ));
    }

    #[test]
    fn v_prefix_still_works_after_normalization_fallback() {
        // v1.0 should still parse via normalization fallback
        assert!(is_update_available_with_date("v1.0", "v2.0", "", ""));
        assert!(!is_update_available_with_date("v2.0", "v1.0", "", ""));
    }
}
