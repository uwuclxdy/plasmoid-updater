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
    let inst_norm = normalize_version(installed_version);
    let avail_norm = normalize_version(available_version);

    // Fast path: identical non-empty version strings skip expensive parsing.
    // If strings are equal their Versioning representations are also equal,
    // so the only possible update signal is a newer release date.
    if !inst_norm.is_empty() && !avail_norm.is_empty() && inst_norm == avail_norm {
        return is_date_newer(installed_date, available_date);
    }

    // Try original strings first — normalization is only needed when raw parsing fails
    let installed_parsed = Versioning::new(installed_version)
        .or_else(|| Versioning::new(&inst_norm));
    let available_parsed = Versioning::new(available_version)
        .or_else(|| Versioning::new(&avail_norm));

    // both versions parseable: use semantic comparison, then date fallback
    if let (Some(inst), Some(avail)) = (&installed_parsed, &available_parsed) {
        if inst < avail {
            return true;
        }
        // same version — check if the release date is newer (content refresh)
        if inst == avail {
            return is_date_newer(installed_date, available_date);
        }
        return false;
    }

    // if available version is parseable but installed is not, it's an update
    if available_parsed.is_some() && installed_parsed.is_none() {
        return true;
    }

    // version strings differ as raw text (unparseable but not equal)
    if !inst_norm.is_empty() && !avail_norm.is_empty() && inst_norm != avail_norm {
        return true;
    }

    // fall back to date comparison
    is_date_newer(installed_date, available_date)
}

/// Returns true if `available_date` is strictly newer than `installed_date`.
fn is_date_newer(installed_date: &str, available_date: &str) -> bool {
    if installed_date.is_empty() || available_date.is_empty() {
        return false;
    }
    // extract just the date part (first 10 chars for YYYY-MM-DD)
    let local_date = &installed_date[..installed_date.len().min(10)];
    let store_date = &available_date[..available_date.len().min(10)];
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
    fn v_prefix_still_works_after_normalization_fallback() {
        // "v1.0.0" is parseable as-is by Versioning, so the raw-first path works
        assert!(is_update_available_with_date("v1.0.0", "v2.0.0", "", ""));
        assert!(!is_update_available_with_date("v2.0.0", "v1.0.0", "", ""));
    }
}
