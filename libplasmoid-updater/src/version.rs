// SPDX-License-Identifier: MIT OR Apache-2.0

use versions::Versioning;

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
    let installed_parsed = Versioning::new(installed_version);
    let available_parsed = Versioning::new(available_version);

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
    if !installed_version.is_empty()
        && !available_version.is_empty()
        && installed_version != available_version
    {
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
