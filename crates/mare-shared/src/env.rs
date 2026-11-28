//! Generic environment switch helpers shared across mare crates.

/// Whether an env var string represents a truthy opt-out/switch (`1`, `true`, `yes`, `on`).
pub fn parse_env_truthy(val: &str) -> bool {
    matches!(val.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

/// True when `MARE_OFFLINE` is set to a truthy value.
pub fn is_offline_env() -> bool {
    std::env::var("MARE_OFFLINE").is_ok_and(|v| parse_env_truthy(&v))
}

/// True when background/CLI update checks are disabled via `MARE_NO_UPDATE_CHECK` or `MARE_OFFLINE`.
pub fn is_update_check_disabled_env() -> bool {
    is_offline_env() || std::env::var("MARE_NO_UPDATE_CHECK").is_ok_and(|v| parse_env_truthy(&v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truthy_env_parsing() {
        for v in ["1", "true", "TRUE", "yes", "Yes", "on", " on "] {
            assert!(parse_env_truthy(v), "expected truthy: {v:?}");
        }
        for v in ["", "0", "false", "no", "off", "random"] {
            assert!(!parse_env_truthy(v), "expected falsy: {v:?}");
        }
    }
}
