// SPDX-License-Identifier: MIT OR Apache-2.0

use std::cmp::Ordering;

use versions::Versioning;

/// compares two version strings.
/// returns Ordering::Less if v1 < v2 (update available).
pub fn compare(v1: &str, v2: &str) -> Ordering {
    match (Versioning::new(v1), Versioning::new(v2)) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => Ordering::Equal,
    }
}

/// returns true if `available` is newer than `installed`.
pub fn is_update_available(installed: &str, available: &str) -> bool {
    compare(installed, available) == Ordering::Less
}

/// returns true if there's an update based on version or date.
/// date comparison is used when both versions are empty.
pub fn is_update_available_with_date(
    installed_version: &str,
    available_version: &str,
    installed_date: &str,
    available_date: &str,
) -> bool {
    let installed_parsed = Versioning::new(installed_version);
    let available_parsed = Versioning::new(available_version);

    // if both versions are parseable, use version comparison only
    if let (Some(inst), Some(avail)) = (&installed_parsed, &available_parsed) {
        return inst < avail;
    }

    // if available version is parseable but installed is not, it's an update
    if available_parsed.is_some() && installed_parsed.is_none() {
        return true;
    }

    // both versions unparseable - use date comparison
    if !installed_date.is_empty() && !available_date.is_empty() {
        // extract just the date part (first 10 chars for YYYY-MM-DD)
        let local_date = &installed_date[..installed_date.len().min(10)];
        let store_date = &available_date[..available_date.len().min(10)];
        return store_date > local_date;
    }

    false
}
