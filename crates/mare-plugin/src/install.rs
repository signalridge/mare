//! Mare install-channel detection, suggested upgrade commands, and update-check policy resolution.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Default GitHub Releases API endpoint for checking the latest mare release.
pub const RELEASE_API_URL: &str = "https://api.github.com/repos/signalridge/mare/releases/latest";

/// Human-facing releases page used when no package-manager or self-update command applies.
pub const RELEASE_PAGE_URL: &str = "https://github.com/signalridge/mare/releases/latest";

/// Base URL for downloading release archives and checksums.
pub const RELEASE_DOWNLOAD_BASE_URL: &str = "https://github.com/signalridge/mare/releases/download";

/// Detected installation channel for the running `mare` executable.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallChannel {
    Brew,
    Scoop,
    Aur,
    Nix,
    Standalone,
    Other,
}

impl InstallChannel {
    /// Canonical lowercase identifier used in CLI `--method`, config, and Lua APIs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Brew => "brew",
            Self::Scoop => "scoop",
            Self::Aur => "aur",
            Self::Nix => "nix",
            Self::Standalone => "standalone",
            Self::Other => "other",
        }
    }

    /// Parse a user-supplied channel override (`--method`).
    pub fn parse_method(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "brew" | "homebrew" => Some(Self::Brew),
            "scoop" => Some(Self::Scoop),
            "aur" => Some(Self::Aur),
            "nix" => Some(Self::Nix),
            "standalone" | "install" | "script" => Some(Self::Standalone),
            "other" | "manual" => Some(Self::Other),
            _ => None,
        }
    }

    /// Whether an external package manager owns this binary (never self-overwrite).
    pub const fn is_managed(self) -> bool {
        matches!(self, Self::Brew | Self::Scoop | Self::Aur | Self::Nix)
    }
}

/// Update check policy configured via `mare.setup { update_check = ... }`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdatePolicy {
    Notify,
    Auto,
    Disabled,
}

impl UpdatePolicy {
    /// Canonical lowercase identifier exposed to Lua and diagnostics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Notify => "notify",
            Self::Auto => "auto",
            Self::Disabled => "disabled",
        }
    }

    /// Normalize JSON config value (`false` -> `Disabled`, `true` | `"notify"` -> `Notify`, `"auto"` -> `Auto`).
    pub fn from_config_value(v: Option<&serde_json::Value>) -> Self {
        match v {
            Some(serde_json::Value::Bool(false)) => Self::Disabled,
            Some(serde_json::Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
                "auto" => Self::Auto,
                "false" | "off" | "disabled" => Self::Disabled,
                _ => Self::Notify,
            },
            _ => Self::Notify,
        }
    }

    /// Resolve effective policy from top-level `update_check`, plugin `enabled`, and env opt-out.
    pub fn resolve(config_value: Option<&serde_json::Value>, plugin_enabled: Option<bool>, env_disabled: bool) -> Self {
        if plugin_enabled == Some(false) || env_disabled {
            return Self::Disabled;
        }
        Self::from_config_value(config_value)
    }
}

/// Resolved install metadata for the current process or a test layout.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InstallInfo {
    pub channel: InstallChannel,
    pub managed: bool,
    pub command: Option<String>,
    pub argv: Vec<String>,
    pub steps: Vec<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manual_hint: Option<String>,
    pub release_url: &'static str,
}

impl InstallInfo {
    /// Detect the active install channel using the current process environment.
    pub fn detect_current() -> Self {
        let exe = std::env::current_exe().ok();
        let canonical = exe.as_deref().and_then(|p| dunce::canonicalize(p).ok());
        let install_dir = std::env::var_os("MARE_INSTALL_DIR").filter(|s| !s.is_empty()).map(PathBuf::from);
        let homebrew_prefix = std::env::var_os("HOMEBREW_PREFIX").filter(|s| !s.is_empty()).map(PathBuf::from);
        let scoop = std::env::var_os("SCOOP").filter(|s| !s.is_empty()).map(PathBuf::from);
        let scoop_global = std::env::var_os("SCOOP_GLOBAL").filter(|s| !s.is_empty()).map(PathBuf::from);
        let local_app_data =
            std::env::var_os("LOCALAPPDATA").filter(|s| !s.is_empty()).map(PathBuf::from).or_else(dirs::data_local_dir);
        let pacman_local = PathBuf::from("/var/lib/pacman/local");
        let dpkg_info = PathBuf::from("/var/lib/dpkg/info");
        let home = dirs::home_dir();

        let ctx = DetectContext {
            exe_path: exe.as_deref(),
            canonical_exe: canonical.as_deref(),
            home: home.as_deref(),
            local_app_data: local_app_data.as_deref(),
            install_dir_env: install_dir.as_deref(),
            homebrew_prefix_env: homebrew_prefix.as_deref(),
            scoop_env: scoop.as_deref(),
            scoop_global_env: scoop_global.as_deref(),
            pacman_local_dir: cfg!(target_os = "linux").then_some(pacman_local.as_path()),
            dpkg_info_dir: cfg!(target_os = "linux").then_some(dpkg_info.as_path()),
            rpm_owned: None,
            path_env: std::env::var_os("PATH"),
            is_macos: cfg!(target_os = "macos"),
            is_windows: cfg!(windows),
            is_linux: cfg!(target_os = "linux"),
        };
        detect_install_info(&ctx)
    }

    /// Build `InstallInfo` for an explicit `--method` override.
    pub fn for_channel(channel: InstallChannel) -> Self {
        build_install_info(channel, BrewKind::Formula, "yay", "mare-bin", None)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BrewKind {
    Formula,
    Cask,
}

/// Pure inputs for [`detect_install_info`].
#[derive(Clone, Debug, Default)]
pub struct DetectContext<'a> {
    pub exe_path: Option<&'a Path>,
    pub canonical_exe: Option<&'a Path>,
    pub home: Option<&'a Path>,
    pub local_app_data: Option<&'a Path>,
    pub install_dir_env: Option<&'a Path>,
    pub homebrew_prefix_env: Option<&'a Path>,
    pub scoop_env: Option<&'a Path>,
    pub scoop_global_env: Option<&'a Path>,
    pub pacman_local_dir: Option<&'a Path>,
    pub dpkg_info_dir: Option<&'a Path>,
    pub rpm_owned: Option<bool>,
    pub path_env: Option<std::ffi::OsString>,
    pub is_macos: bool,
    pub is_windows: bool,
    pub is_linux: bool,
}

fn normalize_slash_lower(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").to_ascii_lowercase()
}

fn is_nix_path(path: &Path) -> bool {
    normalize_slash_lower(path).starts_with("/nix/store/")
}

fn is_scoop_path(path: &Path, ctx: &DetectContext<'_>) -> bool {
    let norm = normalize_slash_lower(path);
    if norm.contains("/scoop/apps/mare/") || norm.contains("/scoop/shims/") {
        return true;
    }
    for base in [ctx.scoop_env, ctx.scoop_global_env].into_iter().flatten() {
        let base_norm = normalize_slash_lower(base);
        let base_trimmed = base_norm.trim_end_matches('/');
        if !base_trimmed.is_empty() && norm.starts_with(&format!("{base_trimmed}/apps/mare/")) {
            return true;
        }
    }
    false
}

/// Match only binaries whose actual path lives in Homebrew's `Cellar/mare/` or `Caskroom/mare/`.
/// Never match an arbitrary binary placed inside `<prefix>/bin` (e.g. `MARE_INSTALL_DIR=/opt/homebrew/bin`).
fn detect_brew_kind(path: &Path, ctx: &DetectContext<'_>) -> Option<BrewKind> {
    let norm = normalize_slash_lower(path);
    if norm.contains("/cellar/mare/") {
        return Some(BrewKind::Formula);
    }
    if norm.contains("/caskroom/mare/") {
        return Some(BrewKind::Cask);
    }
    if let Some(prefix) = ctx.homebrew_prefix_env {
        let pnorm = normalize_slash_lower(prefix);
        let p = pnorm.trim_end_matches('/');
        if !p.is_empty() {
            if norm.starts_with(&format!("{p}/cellar/mare/")) {
                return Some(BrewKind::Formula);
            }
            if norm.starts_with(&format!("{p}/caskroom/mare/")) {
                return Some(BrewKind::Cask);
            }
        }
    }
    None
}

fn find_in_path(path_env: Option<&OsStr>, names: &[&str], is_windows: bool) -> Option<String> {
    let path_env = path_env?;
    for dir in std::env::split_paths(path_env) {
        for &name in names {
            let candidate = if is_windows { dir.join(format!("{name}.exe")) } else { dir.join(name) };
            if candidate.is_file() {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn detect_aur_package(pacman_local_dir: &Path) -> Option<&'static str> {
    let entries = std::fs::read_dir(pacman_local_dir).ok()?;
    let mut found_mare = false;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("mare-bin-") {
            return Some("mare-bin");
        }
        if name.starts_with("mare-") && name[5..].chars().next().is_some_and(|c| c.is_ascii_digit()) {
            found_mare = true;
        }
    }
    found_mare.then_some("mare")
}

fn detect_dpkg_owned(dpkg_info_dir: &Path) -> bool {
    if dpkg_info_dir.join("mare.list").is_file() {
        return true;
    }
    let Ok(entries) = std::fs::read_dir(dpkg_info_dir) else {
        return false;
    };
    entries.flatten().any(|e| {
        e.file_name().to_string_lossy().starts_with("mare:") && e.file_name().to_string_lossy().ends_with(".list")
    })
}

fn detect_rpm_owned_live(exe: &Path) -> bool {
    std::process::Command::new("rpm")
        .args(["-qf", "--quiet"])
        .arg(exe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn path_eq_normalized(a: &Path, b: &Path, case_insensitive: bool) -> bool {
    let na = normalize_slash_lower(a);
    let nb = normalize_slash_lower(b);
    if case_insensitive {
        na == nb
    } else {
        a.to_string_lossy().replace('\\', "/") == b.to_string_lossy().replace('\\', "/")
    }
}

fn is_standalone_path(path: &Path, ctx: &DetectContext<'_>) -> bool {
    let exe_name = if ctx.is_windows { "mare.exe" } else { "mare" };
    if let Some(dir) = ctx.install_dir_env {
        let expected = dir.join(exe_name);
        if path_eq_normalized(path, &expected, ctx.is_windows) {
            return true;
        }
        if let Ok(canon_dir) = dunce::canonicalize(dir)
            && path_eq_normalized(path, &canon_dir.join(exe_name), ctx.is_windows)
        {
            return true;
        }
    }
    if ctx.is_windows {
        if let Some(local) = ctx.local_app_data
            && path_eq_normalized(path, &local.join("mare").join("bin").join("mare.exe"), true)
        {
            return true;
        }
        if let Some(home) = ctx.home
            && path_eq_normalized(
                path,
                &home.join("AppData").join("Local").join("mare").join("bin").join("mare.exe"),
                true,
            )
        {
            return true;
        }
    } else if let Some(home) = ctx.home {
        let default_bin = home.join(".local").join("bin").join("mare");
        if path_eq_normalized(path, &default_bin, false) {
            return true;
        }
        if let Ok(canon_bin) = dunce::canonicalize(home.join(".local").join("bin"))
            && path_eq_normalized(path, &canon_bin.join("mare"), false)
        {
            return true;
        }
    }
    false
}

/// Classify an installation layout from explicit context (unit-testable without global env).
pub fn detect_install_info(ctx: &DetectContext<'_>) -> InstallInfo {
    let candidates: Vec<&Path> = [ctx.canonical_exe, ctx.exe_path].into_iter().flatten().collect();

    for &p in &candidates {
        if is_nix_path(p) {
            return build_install_info(InstallChannel::Nix, BrewKind::Formula, "yay", "mare-bin", None);
        }
    }
    for &p in &candidates {
        if is_scoop_path(p, ctx) {
            return build_install_info(InstallChannel::Scoop, BrewKind::Formula, "yay", "mare-bin", None);
        }
    }
    for &p in &candidates {
        if let Some(kind) = detect_brew_kind(p, ctx) {
            return build_install_info(InstallChannel::Brew, kind, "yay", "mare-bin", None);
        }
    }
    if ctx.is_linux && candidates.iter().any(|p| normalize_slash_lower(p) == "/usr/bin/mare") {
        if let Some(pkg) = ctx.pacman_local_dir.and_then(detect_aur_package) {
            let helper = find_in_path(ctx.path_env.as_deref(), &["yay", "paru"], ctx.is_windows)
                .unwrap_or_else(|| "yay".to_string());
            return build_install_info(InstallChannel::Aur, BrewKind::Formula, &helper, pkg, None);
        }
        if ctx.dpkg_info_dir.is_some_and(detect_dpkg_owned) {
            return build_install_info(
                InstallChannel::Other,
                BrewKind::Formula,
                "yay",
                "mare-bin",
                Some(format!("download the newer .deb package from {RELEASE_PAGE_URL}")),
            );
        }
        let is_rpm = match ctx.rpm_owned {
            Some(v) => v,
            None => candidates.first().is_some_and(|p| detect_rpm_owned_live(p)),
        };
        if is_rpm {
            return build_install_info(
                InstallChannel::Other,
                BrewKind::Formula,
                "yay",
                "mare-bin",
                Some(format!("download the newer .rpm package from {RELEASE_PAGE_URL}")),
            );
        }
    }
    for &p in &candidates {
        if is_standalone_path(p, ctx) {
            return build_install_info(InstallChannel::Standalone, BrewKind::Formula, "yay", "mare-bin", None);
        }
    }
    build_install_info(InstallChannel::Other, BrewKind::Formula, "yay", "mare-bin", None)
}

fn build_install_info(
    channel: InstallChannel,
    brew_kind: BrewKind,
    aur_helper: &str,
    aur_pkg: &str,
    manual_hint: Option<String>,
) -> InstallInfo {
    let steps: Vec<Vec<String>> = match channel {
        InstallChannel::Brew => match brew_kind {
            BrewKind::Formula => vec![vec!["brew".into(), "upgrade".into(), "mare".into()]],
            BrewKind::Cask => vec![
                vec!["brew".into(), "uninstall".into(), "--cask".into(), "mare".into()],
                vec!["brew".into(), "install".into(), "signalridge/tap/mare".into()],
            ],
        },
        InstallChannel::Scoop => vec![vec!["scoop".into(), "update".into(), "mare".into()]],
        InstallChannel::Aur => vec![vec![aur_helper.into(), "-S".into(), aur_pkg.into()]],
        InstallChannel::Nix => vec![vec!["nix".into(), "profile".into(), "upgrade".into(), "mare".into()]],
        InstallChannel::Standalone => vec![vec!["mare".into(), "update".into(), "--self".into()]],
        InstallChannel::Other => Vec::new(),
    };
    let argv = steps.first().cloned().unwrap_or_default();
    let command =
        if steps.is_empty() { None } else { Some(steps.iter().map(|s| s.join(" ")).collect::<Vec<_>>().join(" && ")) };
    InstallInfo {
        channel,
        managed: channel.is_managed(),
        command,
        argv,
        steps,
        manual_hint,
        release_url: RELEASE_PAGE_URL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_brew_formula_and_cask_by_owner_not_prefix() {
        for path in [
            "/opt/homebrew/Cellar/mare/1.0.1/bin/mare",
            "/usr/local/Cellar/mare/1.0.1/bin/mare",
            "/home/linuxbrew/.linuxbrew/Cellar/mare/1.0.1/bin/mare",
        ] {
            let info = detect_install_info(&DetectContext {
                canonical_exe: Some(Path::new(path)),
                is_macos: !path.contains("linuxbrew"),
                is_linux: path.contains("linuxbrew"),
                ..Default::default()
            });
            assert_eq!(info.channel, InstallChannel::Brew, "path: {path}");
            assert!(info.managed);
            assert_eq!(info.command.as_deref(), Some("brew upgrade mare"));
        }

        // Legacy Homebrew cask emits the release migration command.
        let cask = detect_install_info(&DetectContext {
            canonical_exe: Some(Path::new("/opt/homebrew/Caskroom/mare/1.0.0/mare")),
            is_macos: true,
            ..Default::default()
        });
        assert_eq!(cask.channel, InstallChannel::Brew);
        assert!(cask.managed);
        assert_eq!(cask.command.as_deref(), Some("brew uninstall --cask mare && brew install signalridge/tap/mare"));
        assert_eq!(cask.steps.len(), 2);

        // `MARE_INSTALL_DIR=/opt/homebrew/bin` with a non-Cellar binary must be Standalone, NOT Brew.
        let standalone_in_brew_bin = detect_install_info(&DetectContext {
            exe_path: Some(Path::new("/opt/homebrew/bin/mare")),
            canonical_exe: Some(Path::new("/opt/homebrew/bin/mare")),
            install_dir_env: Some(Path::new("/opt/homebrew/bin")),
            homebrew_prefix_env: Some(Path::new("/opt/homebrew")),
            is_macos: true,
            ..Default::default()
        });
        assert_eq!(standalone_in_brew_bin.channel, InstallChannel::Standalone);
        assert!(!standalone_in_brew_bin.managed);

        // Unowned `/opt/homebrew/bin/mare` without standalone env stays `other`.
        let manual = detect_install_info(&DetectContext {
            canonical_exe: Some(Path::new("/opt/homebrew/bin/mare")),
            is_macos: true,
            ..Default::default()
        });
        assert_eq!(manual.channel, InstallChannel::Other);
        assert_eq!(manual.command, None);
    }

    #[test]
    fn detects_scoop_layout_and_command() {
        let info = detect_install_info(&DetectContext {
            canonical_exe: Some(Path::new(r"C:\Users\alice\scoop\apps\mare\1.0.1\mare.exe")),
            is_windows: true,
            ..Default::default()
        });
        assert_eq!(info.channel, InstallChannel::Scoop);
        assert!(info.managed);
        assert_eq!(info.command.as_deref(), Some("scoop update mare"));
    }

    #[test]
    fn detects_nix_store_prefix_including_wrapper_and_rejects_nested_segment() {
        for nix_path in [
            "/nix/store/0123456789abcdef-mare-1.0.1/bin/mare",
            "/nix/store/0123456789abcdef-mare-1.0.1/bin/.mare-wrapped",
        ] {
            let info = detect_install_info(&DetectContext {
                canonical_exe: Some(Path::new(nix_path)),
                is_linux: true,
                ..Default::default()
            });
            assert_eq!(info.channel, InstallChannel::Nix, "path: {nix_path}");
            assert!(info.managed);
            assert_eq!(info.command.as_deref(), Some("nix profile upgrade mare"));
        }

        // Arbitrary user path merely containing `/nix/store/` as a substring is NOT Nix.
        let nested = detect_install_info(&DetectContext {
            canonical_exe: Some(Path::new("/home/me/build/nix/store/demo/bin/mare")),
            is_linux: true,
            ..Default::default()
        });
        assert_eq!(nested.channel, InstallChannel::Other);
        assert!(!nested.managed);
    }

    #[test]
    fn detects_aur_without_system_upgrade_and_deb_rpm_manual_hints() {
        let tmp = tempfile::tempdir().unwrap();
        let pacman_local = tmp.path().join("pacman-local");
        let bin_dir = tmp.path().join("bin");
        std::fs::create_dir_all(pacman_local.join("mare-bin-1.0.1-1")).unwrap();
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(bin_dir.join("paru"), b"#!/bin/sh\n").unwrap();

        let info = detect_install_info(&DetectContext {
            exe_path: Some(Path::new("/usr/bin/mare")),
            pacman_local_dir: Some(&pacman_local),
            path_env: Some(bin_dir.as_os_str().to_os_string()),
            is_linux: true,
            ..Default::default()
        });
        assert_eq!(info.channel, InstallChannel::Aur);
        assert!(info.managed);
        assert_eq!(info.command.as_deref(), Some("paru -S mare-bin"));

        let dpkg_dir = tmp.path().join("dpkg-info");
        std::fs::create_dir_all(&dpkg_dir).unwrap();
        std::fs::write(dpkg_dir.join("mare.list"), b"/usr/bin/mare\n").unwrap();
        let deb = detect_install_info(&DetectContext {
            exe_path: Some(Path::new("/usr/bin/mare")),
            dpkg_info_dir: Some(&dpkg_dir),
            rpm_owned: Some(false),
            is_linux: true,
            ..Default::default()
        });
        assert_eq!(deb.channel, InstallChannel::Other);
        assert!(deb.manual_hint.as_deref().is_some_and(|h| h.contains(".deb")));

        let rpm = detect_install_info(&DetectContext {
            exe_path: Some(Path::new("/usr/bin/mare")),
            rpm_owned: Some(true),
            is_linux: true,
            ..Default::default()
        });
        assert_eq!(rpm.channel, InstallChannel::Other);
        assert!(rpm.manual_hint.as_deref().is_some_and(|h| h.contains(".rpm")));
    }

    #[test]
    fn detects_standalone_unix_windows_and_override_dir() {
        let unix = detect_install_info(&DetectContext {
            exe_path: Some(Path::new("/home/alice/.local/bin/mare")),
            home: Some(Path::new("/home/alice")),
            is_linux: true,
            ..Default::default()
        });
        assert_eq!(unix.channel, InstallChannel::Standalone);
        assert!(!unix.managed);
        assert_eq!(unix.command.as_deref(), Some("mare update --self"));

        let custom_dir = detect_install_info(&DetectContext {
            exe_path: Some(Path::new("/opt/mare/bin/mare")),
            install_dir_env: Some(Path::new("/opt/mare/bin")),
            is_linux: true,
            ..Default::default()
        });
        assert_eq!(custom_dir.channel, InstallChannel::Standalone);

        let win = detect_install_info(&DetectContext {
            exe_path: Some(Path::new(r"C:\Users\Alice\AppData\Local\mare\bin\mare.exe")),
            local_app_data: Some(Path::new(r"C:\Users\Alice\AppData\Local")),
            is_windows: true,
            ..Default::default()
        });
        assert_eq!(win.channel, InstallChannel::Standalone);
        assert_eq!(win.command.as_deref(), Some("mare update --self"));
    }

    #[test]
    fn treats_cargo_or_arbitrary_binary_as_other() {
        let info = detect_install_info(&DetectContext {
            exe_path: Some(Path::new("/home/alice/.cargo/bin/mare")),
            home: Some(Path::new("/home/alice")),
            is_linux: true,
            ..Default::default()
        });
        assert_eq!(info.channel, InstallChannel::Other);
        assert!(!info.managed);
        assert_eq!(info.command, None);
    }

    #[test]
    fn update_policy_resolution_is_single_source_of_truth() {
        assert_eq!(UpdatePolicy::from_config_value(Some(&serde_json::json!(false))), UpdatePolicy::Disabled);
        assert_eq!(UpdatePolicy::from_config_value(Some(&serde_json::json!(true))), UpdatePolicy::Notify);
        assert_eq!(UpdatePolicy::from_config_value(Some(&serde_json::json!("auto"))), UpdatePolicy::Auto);
        assert_eq!(UpdatePolicy::from_config_value(Some(&serde_json::json!("off"))), UpdatePolicy::Disabled);
        assert_eq!(UpdatePolicy::from_config_value(None), UpdatePolicy::Notify);
        assert_eq!(UpdatePolicy::resolve(Some(&serde_json::json!("auto")), Some(false), false), UpdatePolicy::Disabled);
        assert_eq!(UpdatePolicy::resolve(Some(&serde_json::json!("auto")), Some(true), true), UpdatePolicy::Disabled);
        assert_eq!(UpdatePolicy::resolve(Some(&serde_json::json!("auto")), Some(true), false), UpdatePolicy::Auto);
        assert_eq!(UpdatePolicy::resolve(None, None, false).as_str(), "notify");
    }
}
