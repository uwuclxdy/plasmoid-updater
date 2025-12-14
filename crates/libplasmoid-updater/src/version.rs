// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Version comparison algorithm based on Apdatifier (https://github.com/exequtic/apdatifier) - MIT License

use std::cmp::Ordering;

/// normalizes a version string for comparison.
/// replaces non-digit/non-dot chars with dots, collapses consecutive dots, trims.
pub fn normalize(version: &str) -> String {
    let mut result = String::with_capacity(version.len());

    for c in version.chars() {
        if c.is_ascii_digit() || c == '.' {
            result.push(c);
        } else {
            result.push('.');
        }
    }

    // collapse consecutive dots
    let mut collapsed = String::with_capacity(result.len());
    let mut prev_dot = true; // start true to skip leading dots

    for c in result.chars() {
        if c == '.' {
            if !prev_dot {
                collapsed.push(c);
            }
            prev_dot = true;
        } else {
            collapsed.push(c);
            prev_dot = false;
        }
    }

    // trim trailing dots
    collapsed.trim_end_matches('.').to_string()
}

/// compares two version strings.
/// returns Ordering::Less if v1 < v2 (update available).
pub fn compare(v1: &str, v2: &str) -> Ordering {
    let n1 = normalize(v1);
    let n2 = normalize(v2);

    let parts1: Vec<u64> = n1.split('.').filter_map(|s| s.parse().ok()).collect();
    let parts2: Vec<u64> = n2.split('.').filter_map(|s| s.parse().ok()).collect();

    let max_len = parts1.len().max(parts2.len());

    for i in 0..max_len {
        let p1 = parts1.get(i).copied().unwrap_or(0);
        let p2 = parts2.get(i).copied().unwrap_or(0);

        match p1.cmp(&p2) {
            Ordering::Equal => continue,
            other => return other,
        }
    }

    Ordering::Equal
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
    let installed_normalized = normalize(installed_version);
    let available_normalized = normalize(available_version);

    // if both versions are non-empty, use version comparison only
    if !installed_normalized.is_empty() && !available_normalized.is_empty() {
        return compare(installed_version, available_version) == Ordering::Less;
    }

    // if available version is non-empty but installed is empty, it's an update
    if !available_normalized.is_empty() && installed_normalized.is_empty() {
        return true;
    }

    // both versions empty - use date comparison
    if !installed_date.is_empty() && !available_date.is_empty() {
        // extract just the date part (first 10 chars for YYYY-MM-DD)
        let local_date = &installed_date[..installed_date.len().min(10)];
        let store_date = &available_date[..available_date.len().min(10)];
        return store_date > local_date;
    }

    false
}
