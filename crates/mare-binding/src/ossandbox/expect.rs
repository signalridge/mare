//! Shared backend expectations for sandbox tests.

/// Backends a test can run on; an entry matches the selected mechanism when equal or a prefix ("landlock").
pub const ANY: &[&str] = &["bwrap", "landlock", "seatbelt", "windows-acl", "windows-user"];

/// Default sandbox backend expected on CI runners when `MARE_TEST_EXPECT_BACKEND` is unset.
/// Matches the actual backends active on GitHub Actions (`ubuntu-latest` -> `bwrap`,
/// `macos-latest` -> `seatbelt`, `windows-latest` default tier -> `windows-acl`;
/// `scripts/windows_sandbox_ci.ps1` handles the enhanced `windows-user` step explicitly)
/// and `scripts/accept.sh`.
#[cfg(any(debug_assertions, test))]
fn default_ci_backend() -> Option<&'static str> {
    #[cfg(target_os = "linux")]
    {
        Some("bwrap")
    }
    #[cfg(target_os = "macos")]
    {
        Some("seatbelt")
    }
    #[cfg(target_os = "windows")]
    {
        Some("windows-acl")
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

#[cfg(any(debug_assertions, test))]
fn env_truthy(value: Option<&str>) -> bool {
    value.is_some_and(|v| {
        let t = v.trim();
        !t.is_empty() && t != "0" && !t.eq_ignore_ascii_case("false")
    })
}

#[cfg(any(debug_assertions, test))]
fn resolve_expected(explicit: Option<&str>, ci: Option<&str>) -> Option<String> {
    if let Some(value) = explicit {
        return Some(value.to_string());
    }
    if env_truthy(ci) {
        return default_ci_backend().map(str::to_string);
    }
    None
}

#[cfg(any(debug_assertions, test))]
fn env_var(name: &str) -> Option<String> {
    std::env::var_os(name).map(|value| value.to_string_lossy().into_owned())
}

/// Return whether a test can run, failing when its expected backend is missing.
#[cfg(any(debug_assertions, test))]
pub fn require(test: &str, actual: &str, accepted: &[&str]) -> bool {
    let explicit = env_var("MARE_TEST_EXPECT_BACKEND");
    let ci = env_var("CI");
    let expected = resolve_expected(explicit.as_deref(), ci.as_deref());
    require_with_expected(test, actual, accepted, expected.as_deref())
}

/// Release builds never consult the test backend expectation seam.
#[cfg(not(any(debug_assertions, test)))]
pub fn require(_test: &str, actual: &str, accepted: &[&str]) -> bool {
    accepted.iter().any(|&entry| actual.starts_with(entry))
}

#[cfg(any(debug_assertions, test))]
fn require_with_expected(test: &str, actual: &str, accepted: &[&str], expected: Option<&str>) -> bool {
    let matches = |backend: &str| accepted.iter().any(|&entry| backend.starts_with(entry));
    if matches(actual) {
        return true;
    }
    let required = accepted.join("|");
    if let Some(expected) = expected.filter(|value| !value.is_empty()) {
        assert!(!matches(expected), "MARE_TEST_EXPECT_BACKEND={expected}, but this host selected {actual}: {test}");
        eprintln!("N/A[backend={required}]: {test} (expected backend here is {expected})");
    } else {
        eprintln!("SKIPPED[capability={required}]: {test} (backend is {actual})");
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{default_ci_backend, require_with_expected, resolve_expected};

    #[test]
    fn matching_actual_backend_runs_even_with_another_expectation() {
        assert!(require_with_expected("test", "landlock-partial", &["landlock"], Some("bwrap")));
    }

    #[test]
    #[should_panic(expected = "MARE_TEST_EXPECT_BACKEND=bwrap, but this host selected none: example")]
    fn missing_expected_backend_fails() {
        require_with_expected("example", "none", &["bwrap"], Some("bwrap"));
    }

    #[test]
    #[should_panic(expected = "MARE_TEST_EXPECT_BACKEND=landlock-partial, but this host selected none: example")]
    fn expected_backend_prefix_also_requires_the_backend() {
        require_with_expected("example", "none", &["landlock"], Some("landlock-partial"));
    }

    #[test]
    fn other_platform_is_not_applicable() {
        assert!(!require_with_expected("test", "bwrap", &["seatbelt"], Some("bwrap")));
    }

    #[test]
    fn absent_or_empty_expectation_skips() {
        assert!(!require_with_expected("test", "none", &["bwrap"], None));
        assert!(!require_with_expected("test", "none", &["bwrap"], Some("")));
    }

    #[test]
    fn ci_defaults_to_requiring_the_platform_backend_unless_explicitly_overridden() {
        assert_eq!(resolve_expected(None, Some("true")).as_deref(), default_ci_backend());
        assert_eq!(resolve_expected(None, Some("1")).as_deref(), default_ci_backend());
        assert_eq!(resolve_expected(None, Some("false")), None);
        assert_eq!(resolve_expected(None, Some("0")), None);
        assert_eq!(resolve_expected(None, None), None);
        assert_eq!(resolve_expected(Some(""), Some("true")).as_deref(), Some(""));
        assert_eq!(resolve_expected(Some("landlock"), Some("true")).as_deref(), Some("landlock"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[should_panic(expected = "MARE_TEST_EXPECT_BACKEND=bwrap, but this host selected landlock: bwrap_only")]
    fn linux_ci_default_rejects_silent_downgrade_to_landlock_on_bwrap_tests() {
        let expected = resolve_expected(None, Some("true"));
        assert_eq!(expected.as_deref(), Some("bwrap"));
        require_with_expected("bwrap_only", "landlock", &["bwrap"], expected.as_deref());
    }

    #[test]
    #[should_panic(expected = "but this host selected none: ci_example")]
    fn ci_default_fails_when_host_has_no_backend() {
        let expected = resolve_expected(None, Some("true"));
        let expected_ref = expected.as_deref().expect("CI target must have a default sandbox backend");
        require_with_expected("ci_example", "none", super::ANY, Some(expected_ref));
    }

    #[test]
    fn env_var_reads_present_and_absent_variables() {
        assert!(super::env_var("PATH").is_some());
        assert_eq!(super::env_var("MARE_DEFINITELY_UNSET_ENV_VAR_FOR_TEST"), None);
    }
}
