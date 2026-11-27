//! The environment every child process starts from (MIT): the parent's environment minus credential-shaped names
//! (`/KEY|PASSWORD|SECRET|TOKEN/i`) and minus mare's own `MARE_*` names, so harness identity and
//! secrets never leak implicitly. Variables a caller passes explicitly merge *after* this scrub,
//! so a deliberately forwarded credential still works. One function for every spawner (proc, job,
//! pty, and later MCP servers and sub-agents) keeps the rule in one place.

use std::ffi::OsString;

/// Credential-shaped variable name (case-insensitive substring).
pub fn is_sensitive(name: &str) -> bool {
    let up = name.to_ascii_uppercase();
    ["KEY", "PASSWORD", "SECRET", "TOKEN"].iter().any(|p| up.contains(p))
}

/// Adjustments from configuration (`[plugins.tools] env = { include, exclude }`): `include`
/// keeps a name the scrub would drop, `exclude` drops one it would keep. Names compare
/// case-insensitively (Windows environment names are).
#[derive(Debug, Default, Clone)]
pub struct EnvPolicy {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

impl EnvPolicy {
    fn has(list: &[String], name: &str) -> bool {
        list.iter().any(|n| n.eq_ignore_ascii_case(name))
    }

    /// Whether the parent's variable `name` reaches a child.
    pub fn keeps(&self, name: &str) -> bool {
        if Self::has(&self.exclude, name) {
            return false;
        }
        if Self::has(&self.include, name) {
            return true;
        }
        !(is_sensitive(name) || name.to_ascii_uppercase().starts_with("MARE_"))
    }
}

/// The scrubbed parent environment.
pub fn scrubbed_parent_env(policy: &EnvPolicy) -> Vec<(OsString, OsString)> {
    let mut out: Vec<(OsString, OsString)> =
        std::env::vars_os().filter(|(k, _)| policy.keeps(&k.to_string_lossy())).collect();
    // with a proxy configured, children also get the loopback NO_PROXY defaults; explicit
    // variables merge after this
    let lookup =
        |k: &str| out.iter().find(|(n, _)| n.to_string_lossy() == k).map(|(_, v)| v.to_string_lossy().into_owned());
    let overlay = super::proxyenv::child_overlay(&lookup);
    out.extend(overlay.into_iter().map(|(k, v)| (OsString::from(k), OsString::from(v))));
    out
}

/// Remove TUI askpass helpers (`SUDO_ASKPASS` and its `shim_dir/bin` `sudo -A` wrapper on `PATH`)
/// from a confined child's environment so sandboxed commands neither see the askpass shim nor get
/// forced through `sudo -A` when setuid privilege elevation is blocked by the sandbox.
pub fn strip_askpass_env(env: &mut Vec<(OsString, OsString)>) {
    let askpass_bin_dir = env
        .iter()
        .find(|(k, _)| k.to_string_lossy().eq_ignore_ascii_case("SUDO_ASKPASS"))
        .map(|(_, v)| std::path::PathBuf::from(v))
        .or_else(|| std::env::var_os("SUDO_ASKPASS").map(std::path::PathBuf::from))
        .and_then(|p| p.parent().map(|dir| dir.join("bin")));
    env.retain(|(k, _)| !k.to_string_lossy().eq_ignore_ascii_case("SUDO_ASKPASS"));
    if let Some(bin_dir) = askpass_bin_dir {
        for (k, v) in env.iter_mut() {
            if k.to_string_lossy().eq_ignore_ascii_case("PATH") {
                let kept: Vec<std::path::PathBuf> = std::env::split_paths(v).filter(|p| p != &bin_dir).collect();
                if let Ok(joined) = std::env::join_paths(kept) {
                    *v = joined;
                }
            }
        }
    }
}

/// Read `env_include` / `env_exclude` from spawn options.
pub fn policy_from(t: &mlua::Table) -> mlua::Result<EnvPolicy> {
    Ok(EnvPolicy {
        include: t.get::<Option<Vec<String>>>("env_include")?.unwrap_or_default(),
        exclude: t.get::<Option<Vec<String>>>("env_exclude")?.unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubbed_mare_child_preserves_private_home_fallbacks() {
        const CHILD: &str = "MARE_TEST_SUBENV_ISOLATED_CHILD";
        if std::env::var_os(CHILD).is_some() {
            assert!(std::env::var_os("MARE_HOME").is_none(), "production scrub still removes MARE_HOME");
            let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("isolated HOME"));
            let root = home.parent().unwrap();
            crate::test_home::assert_xdg(root);
            for path in [
                mare_shared::paths::state_file(),
                mare_shared::paths::auth_file(),
                mare_shared::paths::host_token_file(),
            ] {
                assert!(path.starts_with(root), "{} escaped the private root", path.display());
            }
            return;
        }
        assert_eq!(std::env::var("MARE_NEXTEST_HOME_DEPTH").as_deref(), Ok("1"));
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "subenv::tests::scrubbed_mare_child_preserves_private_home_fallbacks", "--nocapture"])
            .env_clear()
            .envs(scrubbed_parent_env(&EnvPolicy::default()))
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "child failed: {}", String::from_utf8_lossy(&output.stderr));
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"), "child test did not run");
    }

    #[cfg(unix)]
    #[test]
    fn cleared_mare_and_xdg_still_use_the_private_unix_home() {
        const CHILD: &str = "MARE_TEST_CLEARED_HOME_CHILD";
        if let Some(expected) = std::env::var_os(CHILD) {
            let root = std::path::PathBuf::from(expected);
            for path in [
                mare_shared::paths::state_file(),
                mare_shared::paths::auth_file(),
                mare_shared::paths::host_token_file(),
            ] {
                assert!(path.starts_with(&root), "{} escaped the private root", path.display());
            }
            return;
        }
        assert_eq!(std::env::var("MARE_NEXTEST_HOME_DEPTH").as_deref(), Ok("1"));
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "subenv::tests::cleared_mare_and_xdg_still_use_the_private_unix_home", "--nocapture"])
            .env(CHILD, home.parent().unwrap());
        for name in ["MARE_HOME", "XDG_STATE_HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME"] {
            command.env_remove(name);
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "child failed: {}", String::from_utf8_lossy(&output.stderr));
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"), "child test did not run");
    }

    #[test]
    fn strip_askpass_env_removes_sudo_askpass_and_shim_bin_from_path() {
        let shim_dir = std::path::PathBuf::from("/tmp/mare-askpass-bin-123-abcd");
        let helper = shim_dir.join("askpass");
        let shim_bin = shim_dir.join("bin");
        let path_val = std::env::join_paths([shim_bin.as_path(), std::path::Path::new("/usr/bin")]).unwrap();
        let mut env = vec![
            (OsString::from("SUDO_ASKPASS"), helper.into_os_string()),
            (OsString::from("PATH"), path_val),
            (OsString::from("KEEP"), OsString::from("1")),
        ];
        strip_askpass_env(&mut env);
        assert!(env.iter().all(|(k, _)| k != "SUDO_ASKPASS"));
        let path_after = env.iter().find(|(k, _)| k == "PATH").unwrap().1.clone();
        let dirs: Vec<std::path::PathBuf> = std::env::split_paths(&path_after).collect();
        assert!(!dirs.contains(&shim_bin), "shim bin must be removed from confined PATH: {dirs:?}");
        assert!(dirs.contains(&std::path::PathBuf::from("/usr/bin")));
    }

    #[test]
    fn credential_shaped_and_mare_names_are_dropped_unless_included() {
        let p = EnvPolicy::default();
        for n in ["OPENAI_API_KEY", "github_token", "DB_PASSWORD", "AWS_SECRET_ACCESS_KEY", "MARE_HOME"] {
            assert!(!p.keeps(n), "{n} should be scrubbed");
        }
        for n in ["PATH", "HOME", "LANG", "HTTPS_PROXY", "SSH_AUTH_SOCK"] {
            assert!(p.keeps(n), "{n} should be kept");
        }
        let p = EnvPolicy { include: vec!["GH_TOKEN".into()], exclude: vec!["http_proxy".into()] };
        assert!(p.keeps("GH_TOKEN"));
        assert!(!p.keeps("HTTP_PROXY"));
    }
}
