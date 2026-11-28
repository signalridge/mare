//! Subcommand dispatch for `mare <subcommand>` invocations.

use std::path::Path;

use anyhow::Result;
use mare_config::Config;
use mare_host::{Host, HostOptions};
use mare_shared::ui::Mode;

use crate::cli::{Cli, Cmd, McpCmd, ModelCmd, PeerCmd, PluginCmd, RpcCmd, SandboxCmd, TokenCmd, WebCmd};

fn headless_no_session_opts(
    cli: &Cli,
    cwd: &Path,
    cli_flags: &serde_json::Map<String, serde_json::Value>,
) -> Result<HostOptions> {
    let mode = Mode::Print;
    let (cfg, trust, trusted, no_plugins) = crate::build_cli_config_and_trust(cli, cwd, mode)?;
    let mut opts =
        crate::build_host_options(cli, cwd, mode, &cfg, &trust, trusted, no_plugins, "", &[], cli_flags.clone());
    opts.args["no_session"] = serde_json::Value::Bool(true);
    Ok(opts)
}

/// Dispatch a CLI subcommand. Returns `Ok(Some(exit_code))` when the subcommand handled the
/// invocation, or `Ok(None)` when execution should continue into normal host startup (e.g. `acp`).
pub(crate) async fn dispatch(
    cmd: &Cmd,
    cli: &Cli,
    cwd: &Path,
    cli_flags: &serde_json::Map<String, serde_json::Value>,
) -> Result<Option<i32>> {
    match cmd {
        Cmd::Plugin { action: PluginCmd::Tree { json } } => {
            let opts = headless_no_session_opts(cli, cwd, cli_flags)?;
            Ok(Some(crate::plugins_cmd::run(opts, *json).await?))
        }
        Cmd::Plugin { action } => {
            plugin_cli(action, cli, cwd).await?;
            Ok(Some(0))
        }
        Cmd::Trust { dir, revoke } => {
            let target = match dir {
                Some(d) => {
                    let target = cwd.join(d);
                    if !target.is_dir() {
                        anyhow::bail!("not a directory: {}", target.display());
                    }
                    mare_config::trust::set_dir(&target, !revoke)?;
                    target
                }
                None => {
                    let rr = mare_shared::project::resource_root(cwd);
                    let subject = mare_shared::project::trust_subject(&rr).to_path_buf();
                    mare_config::trust::set(cwd, !revoke)?;
                    subject
                }
            };
            println!("{} {}", if *revoke { "untrusted" } else { "trusted" }, target.display());
            Ok(Some(0))
        }
        Cmd::Session { action } => Ok(Some(crate::session_cmd::run(action, cwd, cli.name.clone()).await?)),
        Cmd::Runtime => {
            println!("{}", mare_plugin::runtime::ensure()?.display());
            Ok(Some(0))
        }
        Cmd::Init => {
            for l in mare_plugin::runtime::init()? {
                println!("{l}");
            }
            Ok(Some(0))
        }
        Cmd::Doctor { fix, json } => {
            let trusted = !cli.no_trust && (cli.trust || mare_config::trust::decision(cwd).unwrap_or(false));
            if *fix {
                let mut cfg = mare_plugin::config_load::load(cwd, trusted).await?;
                apply_set_overrides(&mut cfg, &cli.set);
                mare_plugin_manager::set_project_scope(&mut cfg.value, cwd, trusted);
                let user_scope = serde_json::json!({});
                if [&cfg.value, &user_scope].iter().any(|scope| {
                    mare_plugin_manager::lock_path(scope).exists() && mare_plugin_manager::read_lock(scope).is_err()
                }) {
                    mare_plugin::pkg_binding::run_cli(move |_| mare_plugin_manager::repair_lock(&cfg.value)).await?;
                }
            }
            Ok(Some(crate::doctor::run(cwd, trusted, *fix, *json).await?))
        }
        Cmd::Login { provider, with_api_key, device_auth, oauth } => {
            let req = if provider.as_deref() == Some("status") {
                crate::login_cmd::Request::Status
            } else {
                crate::login_cmd::Request::Login {
                    provider: provider.clone(),
                    with_api_key: *with_api_key,
                    device_auth: *device_auth,
                    oauth: *oauth,
                }
            };
            let opts = headless_no_session_opts(cli, cwd, cli_flags)?;
            Ok(Some(crate::login_cmd::run(opts, req).await?))
        }
        Cmd::Logout { provider } => {
            let opts = headless_no_session_opts(cli, cwd, cli_flags)?;
            Ok(Some(crate::login_cmd::run(opts, crate::login_cmd::Request::Logout(provider.clone())).await?))
        }
        Cmd::Model { action: ModelCmd::Refresh } => {
            let opts = headless_no_session_opts(cli, cwd, cli_flags)?;
            Ok(Some(crate::login_cmd::run(opts, crate::login_cmd::Request::ModelsRefresh).await?))
        }
        Cmd::Completion { shell } => {
            use clap::CommandFactory;
            clap_complete::generate(*shell, &mut Cli::command(), "mare", &mut std::io::stdout());
            Ok(Some(0))
        }
        Cmd::Sandbox { action } => {
            let mut cfg = mare_plugin::config_load::load(cwd, false).await?;
            apply_set_overrides(&mut cfg, &cli.set);
            let code = match action {
                SandboxCmd::Setup { remove, deny_read, no_default_deny_read } => {
                    crate::sandbox_cmd::setup(cwd, &cfg.value, *remove, deny_read, *no_default_deny_read)
                }
                SandboxCmd::Status { require_full } => crate::sandbox_cmd::status(cwd, &cfg.value, *require_full),
            };
            Ok(Some(code))
        }
        Cmd::Test { paths, filter, update_snapshots } => {
            let no_plugins = cli.clean;
            let trusted = !cli.no_trust && (cli.trust || mare_config::trust::decision(cwd).unwrap_or(false));
            Ok(Some(crate::test_cmd::run(cwd, trusted, no_plugins, paths, filter.as_deref(), *update_snapshots).await?))
        }
        Cmd::Token { action: TokenCmd::Rotate { accept_partial } } => {
            let mut accepted_host_protect = false;
            for name in accept_partial {
                let caps = mare_binding::ossandbox::Cap::parse_accept(name).map_err(anyhow::Error::msg)?;
                if caps.contains(&mare_binding::ossandbox::Cap::FsHostProtect) {
                    accepted_host_protect = true;
                }
            }
            mare_binding::http_serve::rotate_host_token_with_partial(accepted_host_protect)
                .map_err(anyhow::Error::msg)?;
            println!("rotated host token");
            Ok(Some(0))
        }
        Cmd::Web { action: WebCmd::Url } => Ok(Some(crate::web::web_url_cli().await?)),
        Cmd::Mcp { action } => {
            let (name, server) = match action {
                McpCmd::List => ("list", None),
                McpCmd::ListTools { server } => ("list-tools", Some(server.clone())),
                McpCmd::Enable { server } => ("enable", Some(server.clone())),
                McpCmd::Disable { server } => ("disable", Some(server.clone())),
                McpCmd::Login { server } => ("login", Some(server.clone())),
            };
            let trusted = !cli.no_trust && (cli.trust || mare_config::trust::decision(cwd).unwrap_or(false));
            Ok(Some(mcp_cli(cwd, trusted, cli.approve_mcps, name, server).await?))
        }
        Cmd::Peer { action: PeerCmd::Send { to, text } } => Ok(Some(crate::peer::send_cli(to.as_deref(), text).await?)),
        Cmd::Rpc { action: RpcCmd::Schema } => {
            print!("{}", crate::rpc::schema_text()?);
            Ok(Some(0))
        }
        Cmd::Askpass { sock, prompt } => {
            #[cfg(unix)]
            return Ok(Some(crate::askpass::client(sock.as_deref(), prompt.clone())));
            #[cfg(not(unix))]
            {
                // no sudo on Windows: nothing to answer
                let _ = (sock, prompt);
                eprintln!("mare askpass: only available on Unix");
                Ok(Some(1))
            }
        }
        Cmd::Acp => Ok(None),
    }
}

async fn plugin_cli(action: &PluginCmd, cli: &Cli, cwd: &Path) -> Result<()> {
    let trusted = !cli.no_trust && (cli.trust || mare_config::trust::decision(cwd).unwrap_or(false));
    let mut cfg = mare_plugin::config_load::load(cwd, trusted).await?;
    apply_set_overrides(&mut cfg, &cli.set);
    mare_plugin_manager::set_project_scope(&mut cfg.value, cwd, trusted);
    if let PluginCmd::List { json: true } = action {
        let rows = serde_json::to_value(mare_plugin_manager::status(&cfg.value)?)?;
        println!("{}", serde_json::to_string_pretty(&mare_shared::redact::redact_json(&rows))?);
        return Ok(());
    }
    let cfg = cfg.value;
    let action = action.clone();
    mare_plugin::pkg_binding::run_cli(move |log| {
        let install = matches!(&action, PluginCmd::Install { .. });
        match action {
            PluginCmd::Sync => mare_plugin_manager::sync(&cfg, log),
            PluginCmd::Update { name, stash } => {
                let source = match name {
                    Some(name) => Some(mare_plugin_manager::plugin_source(&cfg, &name)?.unwrap_or(name)),
                    None => None,
                };
                mare_plugin_manager::update(&cfg, source.as_deref(), stash, log)
            }
            PluginCmd::Check { names, json } => {
                let mut resolved = Vec::new();
                for name in names {
                    resolved.push(mare_plugin_manager::plugin_source(&cfg, &name)?.unwrap_or(name));
                }
                let rows = mare_plugin_manager::check(&cfg, &resolved)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&rows)?);
                } else {
                    for row in rows {
                        log(format!("{}: {} ({} behind)", row.name, row.status, row.behind_count));
                        if let Some(err) = row.error {
                            log(format!("  {err}"));
                        }
                    }
                }
                Ok(())
            }
            PluginCmd::Restore { name, repair } => {
                if repair {
                    mare_plugin_manager::repair_lock(&cfg)?;
                }
                let source = match name {
                    Some(name) => Some(mare_plugin_manager::plugin_source(&cfg, &name)?.unwrap_or(name)),
                    None => None,
                };
                mare_plugin_manager::restore_only(&cfg, source.as_deref(), log)
            }
            PluginCmd::List { .. } => mare_plugin_manager::list(&cfg, log),
            PluginCmd::Clean { force } => mare_plugin_manager::remove_unused(&cfg, force, log),
            PluginCmd::Install { names } | PluginCmd::Uninstall { names } => {
                if install {
                    mare_plugin_manager::install_plugins(&cfg, &names, log)
                } else {
                    mare_plugin_manager::uninstall_plugins(&cfg, &names, log)
                }
            }
            PluginCmd::Tree { .. } => unreachable!("tree is handled by dispatch"),
        }
    })
    .await
}

pub(crate) fn apply_set_overrides(cfg: &mut Config, overrides: &[(String, serde_json::Value)]) {
    for (k, v) in overrides {
        cfg.set_dotted(k, v.clone());
    }
}

/// `mare mcp …`: a headless host whose `mcp` plugin answers the `mcp/cli` event.
async fn mcp_cli(cwd: &Path, trusted: bool, approve: bool, action: &str, server: Option<String>) -> Result<i32> {
    use std::io::{IsTerminal, Write};

    let opts = HostOptions {
        cwd: cwd.to_path_buf(),
        mode: Mode::Print,
        config: serde_json::json!({}),
        load_init: true,
        project_trusted: trusted,
        args: serde_json::json!({ "no_session": true, "approve_mcps": approve }),
        no_user_plugins: false,
    };
    let host = Host::new(opts).await?;
    let lua = host.lua();
    let req = lua.create_table()?;
    let tty = std::io::stdin().is_terminal();
    req.set("action", action)?;
    req.set("server", server)?;
    req.set("tty", tty)?;
    req.set(
        "ask",
        lua.create_async_function(|_, (title, _opts): (String, Option<mlua::Table>)| async move {
            if !std::io::stdin().is_terminal() {
                std::future::pending::<()>().await;
            }
            eprint!("{title}: ");
            let _ = std::io::stderr().flush();
            let (tx, rx) = tokio::sync::oneshot::channel();
            std::thread::spawn(move || {
                let mut s = String::new();
                let line = std::io::stdin().read_line(&mut s).ok().filter(|n| *n > 0).map(|_| s.trim().to_string());
                let _ = tx.send(line);
            });
            Ok(rx.await.ok().flatten())
        })?,
    )?;
    req.set("lines", lua.create_table()?)?;
    req.set("code", 0)?;
    host.kernel.emit("mcp/cli", mlua::MultiValue::from_vec(vec![mlua::Value::Table(req.clone())])).await;
    let lines: mlua::Table = req.get("lines")?;
    if lines.raw_len() == 0 {
        println!("the mcp plugin is disabled");
    }
    for l in lines.sequence_values::<String>().flatten() {
        println!("{l}");
    }
    let code: i32 = req.get("code")?;
    host.shutdown().await;
    Ok(code)
}
