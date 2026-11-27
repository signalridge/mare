//! Proxy settings for `mare.http` and for child processes.
//!
//! - Supported: HTTP(S) proxies from `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` (either casing)
//!   with `NO_PROXY`, read by reqwest itself. SOCKS proxies and PAC
//!   files are not supported: that is a clear error, never a silent direct connection.
//! - Child processes inherit the proxy variables exactly as the user wrote them, in the user's
//!   casing; when a proxy is set but no
//!   `NO_PROXY` is, the loopback defaults are added.

pub const PROXY_VARS: &[&str] = &["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"];
pub const DEFAULT_NO_PROXY: &str = "localhost,127.0.0.1,::1";

/// Err with a readable message when a proxy variable names something mare.http can't use.
pub fn check_supported(env: &dyn Fn(&str) -> Option<String>) -> Result<(), String> {
    for k in PROXY_VARS {
        let Some(v) = env(k) else { continue };
        let lower = v.trim().to_ascii_lowercase();
        if lower.starts_with("socks") {
            return Err(format!(
                "{k}={v}: SOCKS proxies are not supported — set an HTTP or HTTPS proxy (e.g. http://host:port)"
            ));
        }
        if lower.ends_with(".pac") || lower.starts_with("pac+") || lower.contains(".pac?") {
            return Err(format!(
                "{k}={v}: PAC files are not supported — set the proxy URL itself (e.g. http://host:port)"
            ));
        }
    }
    Ok(())
}

/// Variables to add to a child process's environment: nothing when no proxy is configured;
/// otherwise the loopback `NO_PROXY` defaults if neither casing of `NO_PROXY` is set. The proxy
/// variables themselves are inherited unchanged (callers keep the parent's values).
pub fn child_overlay(env: &dyn Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    let has_proxy = PROXY_VARS.iter().any(|k| env(k).is_some());
    if !has_proxy || env("NO_PROXY").is_some() || env("no_proxy").is_some() {
        return vec![];
    }
    vec![("NO_PROXY".into(), DEFAULT_NO_PROXY.into()), ("no_proxy".into(), DEFAULT_NO_PROXY.into())]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let m: Vec<(String, String)> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k: &str| m.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    }

    #[test]
    fn socks_and_pac_are_refused_with_a_clear_message() {
        assert!(check_supported(&env(&[("HTTPS_PROXY", "http://proxy:3128")])).is_ok());
        let e = check_supported(&env(&[("ALL_PROXY", "socks5://127.0.0.1:1080")])).unwrap_err();
        assert!(e.contains("SOCKS proxies are not supported"), "{e}");
        let e = check_supported(&env(&[("https_proxy", "http://wpad/proxy.pac")])).unwrap_err();
        assert!(e.contains("PAC files are not supported"), "{e}");
    }

    #[test]
    fn children_get_loopback_no_proxy_only_when_a_proxy_is_set_without_one() {
        assert!(child_overlay(&env(&[])).is_empty());
        assert!(child_overlay(&env(&[("HTTPS_PROXY", "http://p:1"), ("no_proxy", "corp")])).is_empty());
        let o = child_overlay(&env(&[("https_proxy", "http://p:1")]));
        assert!(o.contains(&("NO_PROXY".into(), DEFAULT_NO_PROXY.into())), "{o:?}");
    }
}
