//! `mare update [--self|--plugins|--all] [-y] [--method <m>]`: unified update entry point.

use std::io::{IsTerminal, Write};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use mare_plugin::install::{InstallChannel, InstallInfo};
use mare_plugin::update::{check_latest_release, resolve_api_url, stage_release, staging_dir};
use mare_shared::env::is_offline_env;
use serde::Serialize;

use crate::cli::Cli;
use crate::commands::apply_set_overrides;

#[derive(Debug, Clone)]
pub(crate) struct UpdateArgs {
    pub self_update: bool,
    pub plugins: bool,
    pub all: bool,
    pub yes: bool,
    pub method: Option<String>,
    pub stash: bool,
    pub json: bool,
}

#[derive(Debug, Serialize)]
struct SelfOutcome {
    channel: String,
    status: &'static str,
    current_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    latest_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Debug, Serialize)]
struct PluginsOutcome {
    status: &'static str,
    reload_reminder: bool,
    log: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct UpdateReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    self_update: Option<SelfOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    plugins: Option<PluginsOutcome>,
}

fn confirm_prompt(question: &str, assume_yes: bool) -> Result<bool> {
    if assume_yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        bail!("confirmation required in non-interactive mode (pass -y / --yes to proceed)");
    }
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    let trimmed = answer.trim().to_ascii_lowercase();
    Ok(matches!(trimmed.as_str(), "y" | "yes"))
}

fn resolve_install_info_with_detected(method: Option<&str>, detected: InstallInfo) -> Result<InstallInfo> {
    let Some(m) = method else {
        return Ok(detected);
    };
    let ch = InstallChannel::parse_method(m)
        .with_context(|| format!("unknown --method `{m}` (expected: brew, scoop, aur, nix, standalone, other)"))?;
    if detected.managed && ch != detected.channel {
        let how = detected.command.as_deref().unwrap_or(detected.release_url);
        bail!(
            "refusing `--method {}`: running mare executable is owned by `{}` (update via: {how})",
            ch.as_str(),
            detected.channel.as_str(),
        );
    }
    if ch == InstallChannel::Standalone
        && detected.channel == InstallChannel::Other
        && let Some(hint) = &detected.manual_hint
    {
        bail!("refusing `--method standalone`: running mare executable is owned by a system package ({hint})");
    }
    if ch == detected.channel {
        return Ok(detected);
    }
    Ok(InstallInfo::for_channel(ch))
}

fn resolve_install_info(method: Option<&str>) -> Result<InstallInfo> {
    resolve_install_info_with_detected(method, InstallInfo::detect_current())
}

async fn update_self(info: &InstallInfo, api_url: &str, yes: bool, json: bool) -> Result<SelfOutcome> {
    let current = env!("CARGO_PKG_VERSION").to_string();
    let ch = info.channel;

    if info.managed {
        let cmd_str = info.command.clone().unwrap_or_default();
        if info.steps.is_empty() {
            bail!("managed channel `{}` has no configured update command", ch.as_str());
        }
        if !confirm_prompt(&format!("Run `{cmd_str}` to update mare?"), yes)? {
            if !json {
                println!("Skipped mare self-update.");
            }
            return Ok(SelfOutcome {
                channel: ch.as_str().to_string(),
                status: "skipped",
                current_version: current,
                latest_version: None,
                command: Some(cmd_str),
                message: Some("skipped by user".into()),
            });
        }
        if !json {
            println!("Running `{cmd_str}`...");
        }
        for step in &info.steps {
            let step_str = step.join(" ");
            let status = if json {
                let out = Command::new(&step[0])
                    .args(&step[1..])
                    .output()
                    .with_context(|| format!("running `{step_str}`"))?;
                out.status
            } else {
                Command::new(&step[0]).args(&step[1..]).status().with_context(|| format!("running `{step_str}`"))?
            };
            if !status.success() {
                bail!("`{step_str}` exited with {status}");
            }
        }
        return Ok(SelfOutcome {
            channel: ch.as_str().to_string(),
            status: "updated",
            current_version: current,
            latest_version: None,
            command: Some(cmd_str),
            message: Some(format!("ran `{}`", info.command.as_deref().unwrap_or_default())),
        });
    }

    if ch == InstallChannel::Other {
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "mare".into());
        let action = info
            .manual_hint
            .clone()
            .unwrap_or_else(|| format!("download the latest release manually from {}", info.release_url));
        let msg = format!(
            "mare at `{exe}` is not managed by install.sh/install.ps1 or a supported auto-update channel; {action}"
        );
        if !json {
            println!("{msg}");
        }
        return Ok(SelfOutcome {
            channel: ch.as_str().to_string(),
            status: "manual",
            current_version: current,
            latest_version: None,
            command: None,
            message: Some(msg),
        });
    }

    // Standalone channel: download release archive, verify checksum, smoke-check, stage for next start.
    let latest = check_latest_release(api_url, &current).await?;
    if !latest.newer {
        if !json {
            println!("mare {current} is already up to date.");
        }
        return Ok(SelfOutcome {
            channel: ch.as_str().to_string(),
            status: "up_to_date",
            current_version: current,
            latest_version: Some(latest.version),
            command: info.command.clone(),
            message: None,
        });
    }

    if !confirm_prompt(&format!("Download and stage mare {} (current: {current})?", latest.version), yes)? {
        if !json {
            println!("Skipped mare self-update.");
        }
        return Ok(SelfOutcome {
            channel: ch.as_str().to_string(),
            status: "skipped",
            current_version: current,
            latest_version: Some(latest.version),
            command: info.command.clone(),
            message: Some("skipped by user".into()),
        });
    }

    let exe = std::env::current_exe().context("resolving current_exe")?;
    let staged = stage_release(&latest, api_url, &staging_dir(), &exe, ch).await?;
    let msg = format!("Staged mare {} for next start (restart mare to activate).", staged.version);
    if !json {
        println!("{msg}");
    }
    Ok(SelfOutcome {
        channel: ch.as_str().to_string(),
        status: "staged",
        current_version: current,
        latest_version: Some(staged.version),
        command: info.command.clone(),
        message: Some(msg),
    })
}

async fn update_plugins(cfg_val: serde_json::Value, stash: bool, json: bool) -> Result<PluginsOutcome> {
    let result = if json {
        tokio::task::spawn_blocking(move || {
            let mut log = Vec::new();
            let res = mare_plugin_manager::update(&cfg_val, None, stash, &mut |line| log.push(line));
            (res, log)
        })
        .await?
    } else {
        let mut captured = Vec::new();
        let res = tokio::task::spawn_blocking(move || {
            let mut lines = Vec::new();
            let res = mare_plugin_manager::update(&cfg_val, None, stash, &mut |line| {
                println!("{line}");
                lines.push(line);
            });
            (res, lines)
        })
        .await?;
        captured.extend(res.1);
        (res.0, captured)
    };

    match result.0 {
        Ok(()) => {
            if !json {
                println!("Run /reload in active mare sessions to apply updated plugins.");
            }
            Ok(PluginsOutcome { status: "updated", reload_reminder: true, log: result.1, error: None })
        }
        Err(e) => {
            let err_msg = format!("{e:#}");
            Ok(PluginsOutcome {
                status: "failed",
                reload_reminder: false,
                log: result.1,
                error: Some(mare_shared::redact::redact_str(&err_msg)),
            })
        }
    }
}

pub(crate) async fn run(args: UpdateArgs, cli: &Cli, cwd: &Path) -> Result<i32> {
    if is_offline_env() {
        bail!("offline mode is active (MARE_OFFLINE); cannot run `mare update`");
    }

    let do_self = args.self_update || args.all || (!args.self_update && !args.plugins);
    let do_plugins = args.plugins || args.all || (!args.self_update && !args.plugins);
    let info = resolve_install_info(args.method.as_deref())?;

    let trusted = !cli.no_trust && (cli.trust || mare_config::trust::decision(cwd).unwrap_or(false));
    let mut cfg = mare_plugin::config_load::load(cwd, trusted).await?;
    apply_set_overrides(&mut cfg, &cli.set);
    mare_plugin_manager::set_project_scope(&mut cfg.value, cwd, trusted);

    let cfg_url = cfg
        .value
        .get("plugins")
        .and_then(|p| p.get("update-check"))
        .and_then(|u| u.get("url"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let api_url = resolve_api_url(cfg_url.as_deref());

    let mut report = UpdateReport { self_update: None, plugins: None };
    let mut exit_code = 0;

    if do_self {
        report.self_update = Some(update_self(&info, &api_url, args.yes, args.json).await?);
    }

    if do_plugins {
        let p_out = update_plugins(cfg.value, args.stash, args.json).await?;
        if let Some(err) = &p_out.error {
            if !args.json {
                eprintln!("plugin update failed: {err}");
            }
            exit_code = 1;
        }
        report.plugins = Some(p_out);
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    }

    Ok(exit_code)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use mare_plugin::install::{DetectContext, InstallChannel, InstallInfo, detect_install_info};

    use super::resolve_install_info_with_detected;

    #[test]
    fn method_override_cannot_bypass_managed_or_system_package_ownership() {
        for managed_ch in [InstallChannel::Brew, InstallChannel::Scoop, InstallChannel::Aur, InstallChannel::Nix] {
            let detected = InstallInfo::for_channel(managed_ch);
            let err = resolve_install_info_with_detected(Some("standalone"), detected.clone()).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains(managed_ch.as_str()), "error must name owning channel `{managed_ch:?}`: {msg}");
            assert!(msg.contains("--method standalone"), "error must mention rejected override: {msg}");

            // Same-channel override is allowed and preserves detected info.
            let same = resolve_install_info_with_detected(Some(managed_ch.as_str()), detected.clone()).unwrap();
            assert_eq!(same, detected);
        }

        // System `.deb` package (detected as `Other` with manual_hint) also refuses `--method standalone`.
        let tmp = tempfile::tempdir().unwrap();
        let dpkg = tmp.path().join("dpkg");
        std::fs::create_dir_all(&dpkg).unwrap();
        std::fs::write(dpkg.join("mare.list"), b"/usr/bin/mare\n").unwrap();
        let deb_detected = detect_install_info(&DetectContext {
            exe_path: Some(Path::new("/usr/bin/mare")),
            dpkg_info_dir: Some(&dpkg),
            rpm_owned: Some(false),
            is_linux: true,
            ..Default::default()
        });
        assert!(resolve_install_info_with_detected(Some("standalone"), deb_detected).is_err());

        // Unowned binary (`Other` with no system-package hint) permits explicit overrides.
        let unowned = InstallInfo::for_channel(InstallChannel::Other);
        assert_eq!(
            resolve_install_info_with_detected(Some("standalone"), unowned.clone()).unwrap().channel,
            InstallChannel::Standalone
        );
        assert_eq!(resolve_install_info_with_detected(Some("brew"), unowned).unwrap().channel, InstallChannel::Brew);
    }
}
