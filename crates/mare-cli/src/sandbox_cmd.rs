//! `mare sandbox setup | status` — the Windows enhanced sandbox tier.

use mare_binding::winsandbox_user::{self as wsu, Gate};

/// Deny-read list: built-in defaults (unless disabled) + `sandbox.deny_read` + `--deny-read`;
/// also returns the user-configured part.
#[cfg_attr(not(windows), allow(dead_code))]
fn deny_list(
    cfg: &serde_json::Value,
    extra: &[std::path::PathBuf],
    no_defaults: bool,
) -> (Vec<std::path::PathBuf>, Vec<std::path::PathBuf>) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let resolve_abs = |p: std::path::PathBuf| if p.is_absolute() { p } else { cwd.join(p) };
    let sb = cfg.get("sandbox");
    let mut more: Vec<std::path::PathBuf> = sb
        .and_then(|s| s.get("deny_read"))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|p| p.as_str()).map(mare_shared::paths::expand).map(resolve_abs).collect())
        .unwrap_or_default();
    more.extend(extra.iter().map(|p| {
        let expanded = mare_shared::paths::expand(&p.to_string_lossy());
        resolve_abs(expanded)
    }));
    let use_defaults =
        !no_defaults && sb.and_then(|s| s.get("deny_read_defaults")).and_then(|v| v.as_bool()) != Some(false);
    let user = wsu::deny_read_list(vec![], &more, false);
    (wsu::deny_read_list(wsu::env_default_deny_read(), &more, use_defaults), user)
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

pub fn setup(cfg: &serde_json::Value, remove: bool, extra: &[std::path::PathBuf], no_defaults: bool) -> i32 {
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
            let (deny_read, deny_read_user) = deny_list(cfg, extra, no_defaults);
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
                let (deny_read, deny_read_user) = deny_list(cfg, extra, no_defaults);
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
        let _ = (cfg, remove, extra, no_defaults);
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

pub fn status(require_full: bool) -> i32 {
    let dir = wsu::sandbox_dir();
    match wsu::SetupState::load(&dir) {
        Ok(Some(s)) => {
            println!("setup: {} (version {})", wsu::setup_file(&dir).display(), s.version);
            println!("accounts: {} / {} (group {})", s.offline.name, s.online.name, wsu::GROUP);
            println!("helper: {}", s.helper.display());
            let user = s.user_deny_read(&wsu::env_default_deny_read()).len();
            println!(
                "deny-read: {} path(s), {user} configured by you (only those keep approved escalations sandboxed)",
                s.deny_read.len()
            );
            println!("WFP filters: {}; proxy ports {}-{}", s.wfp_filters, s.proxy_range.0, s.proxy_range.1);
        }
        Ok(None) => println!("setup: not installed (`mare sandbox setup`)"),
        Err(e) => println!("setup: unreadable: {e}"),
    }
    let (lines, code) = gate_status(wsu::gate(), require_full);
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
}
