//! SemVer 2.0.0 release version comparison shared by update notices and self-update logic.

use std::cmp::Ordering;

use semver::Version;

/// Trim whitespace and strip a single optional leading `v` or `V` tag prefix.
pub fn normalize_tag(raw: &str) -> &str {
    let trimmed = raw.trim();
    trimmed.strip_prefix('v').or_else(|| trimmed.strip_prefix('V')).unwrap_or(trimmed)
}

/// Parse a release version string (accepting an optional leading `v`).
pub fn parse_version(raw: &str) -> Option<Version> {
    Version::parse(normalize_tag(raw)).ok()
}

/// Pure SemVer 2.0.0 precedence comparison (build metadata ignored; leading `v`/`V` accepted).
/// Returns `None` if either string is not a valid SemVer 2.0.0 version.
pub fn compare(a: &str, b: &str) -> Option<Ordering> {
    let (va, vb) = (parse_version(a)?, parse_version(b)?);
    Some(va.cmp_precedence(&vb))
}

/// Whether `candidate` has strictly greater SemVer 2.0.0 precedence than `current` (pure SemVer
/// ordering, including `1.0.1-rc.1 > 1.0.0`). Returns `false` if either input is malformed.
pub fn is_newer_semver(candidate: &str, current: &str) -> bool {
    compare(candidate, current) == Some(Ordering::Greater)
}

/// Release-channel update policy: whether `candidate` should be offered as an update over `current`:
/// - Uses SemVer 2.0.0 precedence (prerelease → stable notifies; numeric prerelease parts order
///   numerically, e.g. `1.0.0-rc.10 > 1.0.0-rc.2`; build metadata has no precedence).
/// - A stable release never upgrades to a prerelease (`1.0.0` does not upgrade to `1.0.0-rc.1` or
///   `1.0.1-rc.1`).
/// - Malformed version strings return `false`.
pub fn is_newer_release(candidate: &str, current: &str) -> bool {
    let (Some(cand), Some(curr)) = (parse_version(candidate), parse_version(current)) else {
        return false;
    };
    if curr.pre.is_empty() && !cand.pre.is_empty() {
        return false;
    }
    cand.cmp_precedence(&curr) == Ordering::Greater
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_semver_precedence_orders_prereleases_and_ignores_build_metadata() {
        assert_eq!(compare("v1.0.1-rc.1", "1.0.0"), Some(Ordering::Greater));
        assert!(is_newer_semver("v1.0.1-rc.1", "1.0.0"));
        assert_eq!(compare("v1.0.0", "1.0.0-rc.1"), Some(Ordering::Greater));
        assert_eq!(compare("v1.0.0-rc.10", "1.0.0-rc.2"), Some(Ordering::Greater));
        assert_eq!(compare("1.0.0+build.2", "1.0.0+build.1"), Some(Ordering::Equal));
        assert!(!is_newer_semver("1.0.0+build.2", "1.0.0+build.1"));
        assert_eq!(compare("not-a-version", "1.0.0"), None);
        assert!(!is_newer_semver("not-a-version", "1.0.0"));
    }

    #[test]
    fn prerelease_upgrades_to_stable() {
        assert!(is_newer_release("v1.0.0", "1.0.0-rc.1"));
        assert!(is_newer_release("1.0.0", "1.0.0-alpha.1"));
        assert!(is_newer_release("v1.0.0", "1.0.0-beta.2+build.9"));
    }

    #[test]
    fn prerelease_identifiers_order_per_semver() {
        assert!(is_newer_release("v1.0.0-rc.2", "1.0.0-rc.1"));
        assert!(is_newer_release("v1.0.0-rc.10", "1.0.0-rc.2"));
        assert!(is_newer_release("v1.0.0-rc.1", "1.0.0-beta.9"));
        assert!(!is_newer_release("v1.0.0-rc.2", "1.0.0-rc.10"));
        assert!(!is_newer_release("v1.0.0-rc.1", "1.0.0-rc.1"));
        assert!(!is_newer_release("v1.0.0-rc.1+b2", "1.0.0-rc.1+b1"));
    }

    #[test]
    fn stable_never_upgrades_to_prerelease() {
        assert!(!is_newer_release("v1.0.0-rc.1", "1.0.0"));
        assert!(!is_newer_release("v1.0.1-rc.1", "1.0.0"));
        assert!(!is_newer_release("v2.0.0-alpha.1", "1.0.0"));
    }

    #[test]
    fn stable_upgrades_and_rejects_malformed() {
        assert!(is_newer_release("v1.0.1", "1.0.0"));
        assert!(is_newer_release("v1.2.10", "1.2.9"));
        assert!(!is_newer_release("v1.0.0", "1.0.0"));
        assert!(!is_newer_release("v0.9.9", "1.0.0"));
        assert!(!is_newer_release("not-a-version", "1.0.0"));
        assert!(!is_newer_release("v1.0.1", "not-a-version"));
    }
}
