//! `mare sandbox setup | status` — the Windows enhanced sandbox tier.

use mare_binding::winsandbox_user::{self as wsu, Gate};

/// Resolve a deny-read entry against the selected workspace (`cwd`), expanding leading `~/`
/// and normalizing `.` / `..` / symlinks when ancestors exist.
fn resolve_deny_path(cwd: &std::path::Path, raw: &str) -> std::path::PathBuf {
    let expanded = mare_binding::ossandbox::expand(raw, cwd);
    mare_binding::ossandbox::resolve(&expanded).unwrap_or(expanded)
}

/// Deny-read list: built-in defaults (unless disabled) + `plugins.permissions.deny_read` +
/// `--deny-read`; also returns the user-configured part.
fn deny_list(
    cwd: &std::path::Path,
    cfg: &serde_json::Value,
    extra: &[std::path::PathBuf],
    no_defaults: bool,
) -> (Vec<std::path::PathBuf>, Vec<std::path::PathBuf>) {
    let mut more: Vec<std::path::PathBuf> = cfg
        .pointer("/plugins/permissions/deny_read")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|p| p.as_str()).map(|raw| resolve_deny_path(cwd, raw)).collect())
        .unwrap_or_default();
    more.extend(extra.iter().map(|p| resolve_deny_path(cwd, &p.to_string_lossy())));
    let user = wsu::deny_read_list(vec![], &more, false);
    (wsu::deny_read_list(wsu::env_default_deny_read(), &more, !no_defaults), user)
}

#[cfg(windows)]
const HANDOFF_PREFIX: &str = "mare-sandbox-handoff-";

#[cfg(windows)]
#[derive(serde::Serialize, serde::Deserialize)]
struct SetupHandoff {
    deny_read: Vec<std::path::PathBuf>,
    deny_read_user: Vec<std::path::PathBuf>,
    host_protected: Vec<std::path::PathBuf>,
}

#[cfg(windows)]
fn write_handoff(opts: &SetupHandoff) -> Result<std::path::PathBuf, String> {
    use std::io::Write;
    let dir = mare_shared::paths::state_dir();
    mare_shared::private::create_dir_all(&dir).map_err(|e| format!("sandbox handoff dir: {e}"))?;
    let path = dir.join(format!("{HANDOFF_PREFIX}{}-{}.json", std::process::id(), uuid::Uuid::new_v4()));
    let mut f = mare_shared::private::create_new(&path).map_err(|e| format!("sandbox handoff file: {e}"))?;
    let body = serde_json::to_vec(opts).map_err(|e| e.to_string())?;
    f.write_all(&body).map_err(|e| format!("writing sandbox handoff: {e}"))?;
    Ok(path)
}

#[cfg(windows)]
fn take_handoff(extra: &[std::path::PathBuf]) -> Result<Option<SetupHandoff>, String> {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    let [path] = extra else { return Ok(None) };
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else { return Ok(None) };
    if !(name.starts_with(HANDOFF_PREFIX) && name.ends_with(".json")) {
        return Ok(None);
    }
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("reading sandbox handoff: {e}"))?;
    if meta.file_type().is_symlink() || (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0) || !meta.is_file()
    {
        return Err("refusing non-regular sandbox handoff file".into());
    }
    let bytes = std::fs::read(path).map_err(|e| format!("reading sandbox handoff: {e}"))?;
    let _ = std::fs::remove_file(path);
    let parsed: SetupHandoff = serde_json::from_slice(&bytes).map_err(|e| format!("parsing sandbox handoff: {e}"))?;
    Ok(Some(parsed))
}

pub fn setup(
    cwd: &std::path::Path,
    cfg: &serde_json::Value,
    remove: bool,
    extra: &[std::path::PathBuf],
    no_defaults: bool,
) -> i32 {
    #[cfg(windows)]
    {
        use mare_binding::winsandbox_user::win;
        if !win::is_elevated() {
            // Resolve invoking-user deny_read and host-protected paths BEFORE UAC elevation so an
            // elevated Admin profile does not overwrite the invoking user's paths.
            if remove {
                let args = vec!["sandbox".into(), "setup".into(), "--remove".into()];
                eprintln!("mare: elevation required — approve the UAC prompt");
                return match win::relaunch_elevated(&args) {
                    Ok(code) => code,
                    Err(e) => {
                        eprintln!("mare sandbox: {e}");
                        1
                    }
                };
            }
            let (deny_read, deny_read_user) = deny_list(cwd, cfg, extra, no_defaults);
            let host_protected: Vec<std::path::PathBuf> = mare_binding::ossandbox::protected::host_protected()
                .into_iter()
                .filter(|e| e.shape == mare_binding::ossandbox::protected::HostShape::Dir)
                .map(|e| e.path)
                .collect();
            let handoff_path = match write_handoff(&SetupHandoff { deny_read, deny_read_user, host_protected }) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("mare sandbox: {e}");
                    return 1;
                }
            };
            let args = vec![
                "sandbox".into(),
                "setup".into(),
                "--deny-read".into(),
                handoff_path.to_string_lossy().into_owned(),
            ];
            eprintln!("mare: elevation required — approve the UAC prompt");
            let rc = win::relaunch_elevated(&args);
            let _ = std::fs::remove_file(&handoff_path);
            return match rc {
                Ok(code) => code,
                Err(e) => {
                    eprintln!("mare sandbox: {e}");
                    1
                }
            };
        }
        let mut log = |m: &str| println!("  {m}");
        if remove {
            return match win::remove(&mut log) {
                Ok(()) => {
                    println!("enhanced sandbox removed");
                    0
                }
                Err(e) => {
                    eprintln!("mare sandbox: {e}");
                    1
                }
            };
        }
        let opts = match take_handoff(extra) {
            Ok(Some(h)) => win::SetupOpts {
                deny_read: h.deny_read,
                deny_read_user: h.deny_read_user,
                host_protected: h.host_protected,
            },
            Ok(None) => {
                let (deny_read, deny_read_user) = deny_list(cwd, cfg, extra, no_defaults);
                let host_protected = mare_binding::ossandbox::protected::host_protected()
                    .into_iter()
                    .filter(|e| e.shape == mare_binding::ossandbox::protected::HostShape::Dir)
                    .map(|e| e.path)
                    .collect();
                win::SetupOpts { deny_read, deny_read_user, host_protected }
            }
            Err(e) => {
                eprintln!("mare sandbox setup: {e}");
                return 1;
            }
        };
        match win::setup(&opts, &mut log) {
            Ok(s) => {
                println!("enhanced sandbox installed in {}", wsu::sandbox_dir().display());
                println!("run `mare sandbox status` to self-test it (sandboxed commands also self-test on first use)");
                if s.wfp_filters == 0 {
                    println!("warning: no WFP filters — the network stays uncontrolled and the tier won't report full");
                }
                0
            }
            Err(e) => {
                eprintln!("mare sandbox setup: {e}");
                1
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (cwd, cfg, remove, extra, no_defaults);
        eprintln!("mare sandbox setup: the enhanced tier is Windows-only (this platform uses its native sandbox)");
        2
    }
}

fn gate_status(gate: &Gate, require_full: bool) -> (Vec<String>, i32) {
    match gate {
        Gate::Full(_) => (
            vec![
                "self-test: passed — enhanced tier active (enforcement: partial)".into(),
                "network: per-call isolation unavailable: proxy ports are shared across sandbox calls".into(),
            ],
            // A successful account self-test does not establish per-call network isolation.
            // Keep the complete-enforcement contract separate from enhanced availability.
            i32::from(require_full),
        ),
        Gate::NotConfigured => {
            (vec!["self-test: skipped — default tier (enforcement: partial)".into()], i32::from(require_full))
        }
        Gate::Fallback(why) => {
            (vec![format!("self-test: {why}\nusing the default tier (enforcement: partial)")], i32::from(require_full))
        }
    }
}

fn deny_read_mismatch(configured_user: &[std::path::PathBuf], installed_user: &[std::path::PathBuf]) -> Option<String> {
    use std::collections::HashSet;
    let installed_keys: HashSet<String> = installed_user.iter().map(|p| wsu::path_key(p)).collect();
    let configured_keys: HashSet<String> = configured_user.iter().map(|p| wsu::path_key(p)).collect();
    let missing: Vec<String> = configured_user
        .iter()
        .filter(|p| !installed_keys.contains(&wsu::path_key(p)))
        .map(|p| p.display().to_string())
        .collect();
    let extra: Vec<String> = installed_user
        .iter()
        .filter(|p| !configured_keys.contains(&wsu::path_key(p)))
        .map(|p| p.display().to_string())
        .collect();
    if missing.is_empty() && extra.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    if !missing.is_empty() {
        parts.push(format!("missing from ACLs: {}", missing.join(", ")));
    }
    if !extra.is_empty() {
        parts.push(format!("extra in ACLs: {}", extra.join(", ")));
    }
    Some(format!(
        "deny-read mismatch: plugins.permissions.deny_read differs from installed ACLs ({}) — run `mare sandbox setup` to sync",
        parts.join("; ")
    ))
}

fn status_lines(
    dir: &std::path::Path,
    cwd: &std::path::Path,
    cfg: &serde_json::Value,
    gate: &Gate,
    require_full: bool,
    is_windows: bool,
) -> (Vec<String>, i32) {
    let mut lines = Vec::new();
    let configured_user = if is_windows { deny_list(cwd, cfg, &[], false).1 } else { Vec::new() };
    match wsu::SetupState::load(dir) {
        Ok(Some(s)) => {
            lines.push(format!("setup: {} (version {})", wsu::setup_file(dir).display(), s.version));
            lines.push(format!("accounts: {} / {} (group {})", s.offline.name, s.online.name, wsu::GROUP));
            lines.push(format!("helper: {}", s.helper.display()));
            let installed_user = s.user_deny_read(&wsu::env_default_deny_read());
            lines.push(format!(
                "deny-read: {} path(s), {} configured by you (only those keep approved escalations sandboxed)",
                s.deny_read.len(),
                installed_user.len()
            ));
            if is_windows && let Some(mismatch) = deny_read_mismatch(&configured_user, &installed_user) {
                lines.push(mismatch);
            }
            lines.push(format!("WFP filters: {}; proxy ports {}-{}", s.wfp_filters, s.proxy_range.0, s.proxy_range.1));
        }
        Ok(None) => {
            lines.push("setup: not installed (`mare sandbox setup`)".into());
            if is_windows && !configured_user.is_empty() {
                lines.push(format!(
                    "deny-read mismatch: {} path(s) configured in plugins.permissions.deny_read are not installed yet — run `mare sandbox setup`",
                    configured_user.len()
                ));
            }
        }
        Err(e) => lines.push(format!("setup: unreadable: {e}")),
    }
    let (gate_lines, code) = gate_status(gate, require_full);
    lines.extend(gate_lines);
    (lines, code)
}

pub fn status(cwd: &std::path::Path, cfg: &serde_json::Value, require_full: bool) -> i32 {
    let dir = wsu::sandbox_dir();
    let (lines, code) = status_lines(&dir, cwd, cfg, wsu::gate(), require_full, cfg!(windows));
    for line in lines {
        println!("{line}");
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample_setup_state() -> wsu::SetupState {
        let acct = |name: &str, sid: &str| wsu::Account {
            name: name.into(),
            sid: sid.into(),
            password: "dpapi-placeholder".into(),
        };
        wsu::SetupState {
            version: 1,
            offline: acct("mare-sbx-off", "S-1-5-21-1"),
            online: acct("mare-sbx-on", "S-1-5-21-2"),
            group_sid: "S-1-5-21-3".into(),
            helper: PathBuf::from("mare.exe"),
            helper_sha256: "0".repeat(64),
            deny_read: vec![],
            deny_read_user: Some(vec![]),
            canary: PathBuf::from("canary"),
            proxy_range: (40000, 40010),
            wfp_filters: 4,
        }
    }

    #[test]
    fn gate_status_full_reports_partial_network_and_fails_only_when_full_required() {
        let gate = Gate::Full(Box::new(sample_setup_state()));

        let (lines, code) = gate_status(&gate, false);
        assert_eq!(code, 0);
        assert_eq!(
            lines,
            vec![
                "self-test: passed — enhanced tier active (enforcement: partial)",
                "network: per-call isolation unavailable: proxy ports are shared across sandbox calls",
            ]
        );

        let (lines_req, code_req) = gate_status(&gate, true);
        assert_eq!(code_req, 1);
        assert_eq!(lines_req, lines);
    }

    #[test]
    fn gate_status_not_configured_and_fallback_fail_only_when_full_required() {
        let (lines, code) = gate_status(&Gate::NotConfigured, false);
        assert_eq!(code, 0);
        assert_eq!(lines, vec!["self-test: skipped — default tier (enforcement: partial)"]);
        assert_eq!(gate_status(&Gate::NotConfigured, true).1, 1);

        let fallback = Gate::Fallback("missing WinSta0".into());
        let (fb_lines, fb_code) = gate_status(&fallback, false);
        assert_eq!(fb_code, 0);
        assert_eq!(fb_lines, vec!["self-test: missing WinSta0\nusing the default tier (enforcement: partial)"]);
        assert_eq!(gate_status(&fallback, true).1, 1);
    }

    #[test]
    fn deny_list_merges_set_overrides_and_resolves_relative_paths_against_workspace_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_b = dunce::canonicalize(tmp.path()).unwrap().join("workspace-b");
        std::fs::create_dir_all(&workspace_b).unwrap();
        std::fs::write(workspace_b.join("cfg-secret.txt"), "cfg").unwrap();
        std::fs::write(workspace_b.join("cli-secret.txt"), "cli").unwrap();

        let mut cfg = mare_config::Config::default();
        crate::commands::apply_set_overrides(
            &mut cfg,
            &[("plugins.permissions.deny_read".into(), serde_json::json!(["cfg-secret.txt"]))],
        );

        let (all, user) = deny_list(&workspace_b, &cfg.value, &[PathBuf::from("cli-secret.txt")], false);
        let expected_cfg = workspace_b.join("cfg-secret.txt");
        let expected_cli = workspace_b.join("cli-secret.txt");
        assert_eq!(user, vec![expected_cfg.clone(), expected_cli.clone()]);
        assert!(all.contains(&expected_cfg) && all.contains(&expected_cli));
        assert_ne!(workspace_b, std::env::current_dir().unwrap());
    }

    #[test]
    fn status_lines_reports_mismatch_between_config_and_installed_acls() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sandbox-dir");
        let ws = dunce::canonicalize(tmp.path()).unwrap().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let secret_a = ws.join("a.txt");
        let secret_b = ws.join("b.txt");
        std::fs::write(&secret_a, "a").unwrap();
        std::fs::write(&secret_b, "b").unwrap();

        let cfg_a = serde_json::json!({ "plugins": { "permissions": { "deny_read": ["a.txt"] } } });
        let (non_win_lines, _) = status_lines(&dir, &ws, &cfg_a, &Gate::NotConfigured, false, false);
        assert!(
            !non_win_lines.iter().any(|l| l.contains("deny-read mismatch")),
            "non-Windows status must not emit Windows setup mismatch warnings: {non_win_lines:?}"
        );

        let (uninstalled_lines, _) = status_lines(&dir, &ws, &cfg_a, &Gate::NotConfigured, false, true);
        assert!(
            uninstalled_lines.iter().any(|l| l.contains("deny-read mismatch")),
            "uninstalled setup with configured deny_read must note mismatch: {uninstalled_lines:?}"
        );

        let mut state = sample_setup_state();
        state.deny_read = vec![secret_a.clone()];
        state.deny_read_user = Some(vec![secret_a.clone()]);
        state.save(&dir).unwrap();

        let (matching_lines, _) = status_lines(&dir, &ws, &cfg_a, &Gate::NotConfigured, false, true);
        assert!(
            !matching_lines.iter().any(|l| l.contains("deny-read mismatch")),
            "matching config and ACLs must not report mismatch: {matching_lines:?}"
        );

        let cfg_b = serde_json::json!({ "plugins": { "permissions": { "deny_read": ["b.txt"] } } });
        let (non_win_state_lines, _) = status_lines(&dir, &ws, &cfg_b, &Gate::NotConfigured, false, false);
        assert!(
            !non_win_state_lines.iter().any(|l| l.contains("deny-read mismatch")),
            "non-Windows status must not report ACL mismatch: {non_win_state_lines:?}"
        );

        let (mismatch_lines, _) = status_lines(&dir, &ws, &cfg_b, &Gate::NotConfigured, false, true);
        let line =
            mismatch_lines.iter().find(|l| l.contains("deny-read mismatch")).expect("mismatch line must be present");
        assert!(line.contains("missing from ACLs") && line.contains("b.txt"), "{line}");
        assert!(line.contains("extra in ACLs") && line.contains("a.txt"), "{line}");
    }
}
