// Host setup installs process-wide network approval and environment state.
#[cfg(test)]
pub(crate) static HOST_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

use std::io::IsTerminal;
use std::path::Path;

use anyhow::Result;

mod acp;
#[cfg(not(feature = "bench-system-allocator"))]
mod alloc;
mod app;
/// In-process TUI driver for the e2e tests (not a public API).
#[doc(hidden)]
pub use app::testkit;
#[cfg(unix)]
mod askpass;
mod cli;
mod commands;
mod diagnostics;
mod doctor;
mod editor_cmd;
mod hardening;
mod headless;
mod headless_output;
mod login_cmd;
mod not_sent;
mod peer;
mod plain;
mod plugins_cmd;
#[doc(hidden)]
pub mod rpc;
mod sandbox_cmd;
mod session_addr;
mod session_cmd;
mod signals;
mod startup_error;
mod terminal;
mod test_cmd;
mod update_cmd;
mod web;

use mare_config::Config;
use mare_host::{Host, HostOptions};
use mare_shared::ui::Mode;

use cli::{Cli, Cmd, parse_cli_with_discovery};
use diagnostics::{CrashGuard, init_logging, install_crash_hook};
pub(crate) use diagnostics::{TUI_ACTIVE, write_crash_report};

static PROTOCOL_STDOUT: std::sync::OnceLock<std::fs::File> = std::sync::OnceLock::new();

/// Redirect OS fd 1 onto stderr for protocol-framed stdout modes (`--json`, `--rpc`, `acp`)
/// while retaining a cloned handle to the real protocol stdout for the dedicated writer.
pub(crate) fn detach_protocol_stdout() {
    let _ = PROTOCOL_STDOUT.get_or_init(|| {
        #[cfg(unix)]
        {
            use std::os::fd::AsFd;
            let saved = std::io::stdout().as_fd().try_clone_to_owned().expect("clone stdout fd");
            // SAFETY: STDERR_FILENO (2) and STDOUT_FILENO (1) are valid standard descriptors.
            unsafe {
                libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO);
            }
            std::fs::File::from(saved)
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsHandle;
            let saved = std::io::stdout().as_handle().try_clone_to_owned().expect("clone stdout handle");
            std::fs::File::from(saved)
        }
    });
}

/// Clone the dedicated protocol stdout handle (or process stdout if not detached).
pub(crate) fn clone_protocol_stdout() -> std::io::Result<std::fs::File> {
    if let Some(file) = PROTOCOL_STDOUT.get() {
        return file.try_clone();
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        Ok(std::fs::File::from(std::io::stdout().as_fd().try_clone_to_owned()?))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsHandle;
        Ok(std::fs::File::from(std::io::stdout().as_handle().try_clone_to_owned()?))
    }
}

/// Write one newline-delimited protocol record to the dedicated protocol stdout.
pub(crate) fn write_protocol_stdout_line(line: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(file) = PROTOCOL_STDOUT.get() {
        let mut cloned = file.try_clone()?;
        writeln!(cloned, "{line}")?;
        return cloned.flush();
    }
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}")?;
    out.flush()
}

fn join_prompt_args(args: &[String], cwd: &std::path::Path) -> String {
    args.iter()
        .map(|a| {
            if let Some(p) = a.strip_prefix('@')
                && p.chars().any(char::is_whitespace)
                && !p.starts_with('"')
                && !p.starts_with('\'')
                && std::fs::symlink_metadata(cwd.join(p)).is_ok()
            {
                format!("@\"{p}\"")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The `mare` binary's entry point (`src/bin/mare.rs` calls this).
pub fn main() -> Result<()> {
    let first_arg = std::env::args_os().nth(1);
    let first_arg = first_arg.as_deref();
    if first_arg == Some(std::ffi::OsStr::new(mare_binding::sandbox::WORKER_ARG)) {
        hardening::apply();
        return mare_binding::sandbox::worker_main();
    }
    // `mare __sandbox-exec …`: the Windows sandbox runner (spawned in place of a confined
    // command, like sandbox-exec / bwrap) — no runtime, no config, just start and mirror the child
    if first_arg == Some(std::ffi::OsStr::new(mare_binding::winsandbox::RUNNER_ARG)) {
        let args: Vec<String> = std::env::args_os().skip(2).map(|a| a.to_string_lossy().into_owned()).collect();
        mare_binding::platform::exit(mare_binding::winsandbox::runner_main(&args));
    }
    // `mare __netns-proxy …`: inside bubblewrap's network namespace, the loopback bridge to the
    // sandbox proxy, then the confined command (Linux `net_proxy`)
    #[cfg(target_os = "linux")]
    if first_arg == Some(std::ffi::OsStr::new(mare_binding::ossandbox::NETNS_ARG)) {
        let args: Vec<String> = std::env::args_os().skip(2).map(|a| a.to_string_lossy().into_owned()).collect();
        mare_binding::platform::exit(mare_binding::ossandbox::netns_main(&args));
    }
    // `mare __sandbox-selftest …`: the enhanced tier's self-test, run inside the sandbox
    if first_arg == Some(std::ffi::OsStr::new(mare_binding::winsandbox_user::SELFTEST_ARG)) {
        let args: Vec<String> = std::env::args_os().skip(2).map(|a| a.to_string_lossy().into_owned()).collect();
        mare_binding::platform::exit(mare_binding::winsandbox_user::selftest_main(&args));
    }
    // before any thread exists: no core dumps / debugger attach / LD_* or DYLD_* injection
    hardening::apply();
    if !matches!(first_arg, Some(a) if a == "askpass") {
        maybe_relaunch_with_staged_update();
    }
    mare_shared::ui::install_hooks(mare_tui::hooks());
    mare_shared::profile::start();
    // Actor model: the main Lua VM is owned by this single-threaded runtime; everything that
    // touches Lua runs on its LocalSet. Blocking work goes to the blocking pool.
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let local = tokio::task::LocalSet::new();
    let parsed = parse_cli_with_discovery(|pre_cwd, no_plugins, cli_trust| {
        local.block_on(&rt, mare_host::discover_cli_flags(pre_cwd, no_plugins, cli_trust))
    });
    mare_shared::profile::mark("cli parsed");
    if parsed.cli.startuptime.is_some() || parsed.cli.profile_startup {
        mare_shared::profile::enable_startup_profile();
        let every = std::env::var("MARE_PROFILE_LUA_EVERY")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(1_000_000);
        mare_shared::profile::enable_lua_sampling(every);
    }
    init_logging();
    mare_shared::profile::mark("logging");
    install_crash_hook();
    let _crash = CrashGuard;
    let code = local.block_on(&rt, run(parsed.cli, parsed.flags))?;
    rt.shutdown_timeout(std::time::Duration::from_millis(200));
    signals::finish_process(code);
}

async fn run(cli: Cli, cli_flags: serde_json::Map<String, serde_json::Value>) -> Result<i32> {
    signals::install_forceable_shutdown()?;
    let pre_json = cli.command.is_none() && (cli.json || cli.mode.as_deref() == Some("json"));
    let cwd = match &cli.cwd {
        Some(c) => match dunce::canonicalize(c) {
            Ok(p) => p,
            Err(e) => {
                let msg = format!("cannot access directory {}: {e}", c.display());
                if pre_json {
                    return Ok(crate::headless::usage_error(true, &msg));
                }
                return Err(anyhow::anyhow!(msg));
            }
        },
        None => match std::env::current_dir() {
            Ok(p) => p,
            Err(e) => {
                let msg = format!("cannot determine current working directory: {e} (use --cwd to specify one)");
                if pre_json {
                    return Ok(crate::headless::usage_error(true, &msg));
                }
                return Err(anyhow::anyhow!(msg));
            }
        },
    };
    if let Some(cmd) = &cli.command
        && let Some(code) = commands::dispatch(cmd, &cli, &cwd, &cli_flags).await?
    {
        return Ok(code);
    }

    let mode = resolve_mode(&cli);
    if cli.print && cli.mode.as_deref() == Some("rpc") {
        eprintln!("mare: --print cannot be used with --mode rpc");
        return Ok(2);
    }
    if (cli.web || cli.web_listen.is_some()) && mode != Mode::Rpc {
        return Ok(crate::headless::usage_error(
            mode == Mode::Json,
            "--web / --web-listen cannot be used with --mode text or --mode json",
        ));
    }
    let add_dirs = if cli.add_dir.is_empty() {
        Vec::new()
    } else if mode == Mode::Acp {
        eprintln!("mare: --add-dir does not apply to `mare acp` (use ACP `additionalDirectories` instead)");
        return Ok(2);
    } else {
        let report_usage = |err: &str| {
            if matches!(mode, Mode::Print | Mode::Json) {
                return crate::headless::usage_error(mode == Mode::Json, err);
            }
            eprintln!("mare: {err}");
            2
        };
        let dirs = match mare_binding::workspace::validate_additional_dirs(&cwd, &cli.add_dir) {
            Ok(dirs) => dirs,
            Err(err) => return Ok(report_usage(&err)),
        };
        let mut utf8_dirs = Vec::with_capacity(dirs.len());
        for dir in dirs {
            let Some(s) = dir.to_str() else {
                return Ok(report_usage(&format!("--add-dir path is not valid UTF-8: {}", dir.display())));
            };
            utf8_dirs.push(s.to_owned());
        }
        utf8_dirs
    };
    let (cfg, trust, trusted, no_plugins) = match build_cli_config_and_trust(&cli, &cwd, mode) {
        Ok(v) => v,
        Err(e) => {
            if mode == Mode::Json {
                return Ok(crate::headless::usage_error(true, &format!("{e:#}")));
            }
            return Err(e);
        }
    };
    // refresh the read-only runtime copy when the version changed — off the startup path
    // (blocking pool), since nothing in the first frame or first request needs it; in TUI mode
    // defer spawning until after the first frame so ~200 file writes never contend with startup
    // (especially under Windows Defender real-time scanning).
    if mode != Mode::Tui {
        tokio::task::spawn_blocking(|| {
            if let Err(e) = mare_plugin::runtime::ensure() {
                tracing::warn!("runtime dir: {e}");
            }
        });
    }

    let output_schema = match load_output_schema(&cli, &cwd, mode) {
        Ok(s) => s,
        Err(code) => return Ok(code),
    };
    let deadline =
        matches!(mode, Mode::Print | Mode::Json).then(|| cli.timeout.map(|d| std::time::Instant::now() + d)).flatten();
    let prompt = match resolve_prompt(&cli, &cwd, mode, deadline).await {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let remaining_timeout = deadline.map(|dl| dl.saturating_duration_since(std::time::Instant::now()));
    let opts = build_host_options(&cli, &cwd, mode, &cfg, &trust, trusted, no_plugins, &prompt, &add_dirs, cli_flags);
    run_mode(mode, &cli, opts, prompt, output_schema, remaining_timeout).await
}

fn resolve_mode(cli: &Cli) -> Mode {
    if matches!(cli.command, Some(Cmd::Acp)) {
        return Mode::Acp;
    }
    match (cli.mode.as_deref(), cli.json, cli.print) {
        (Some("rpc"), _, _) => Mode::Rpc,
        _ if cli.rpc => Mode::Rpc,
        (Some("json"), _, _) | (_, true, _) => Mode::Json,
        (Some("text"), _, _) | (_, _, true) => Mode::Print,
        _ if cli.web || cli.web_listen.is_some() => Mode::Rpc,
        _ if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() => Mode::Print,
        _ => Mode::Tui,
    }
}

pub(crate) fn build_cli_config_and_trust(
    cli: &Cli,
    cwd: &Path,
    mode: Mode,
) -> Result<(Config, mare_config::trust::Trust, bool, bool)> {
    // Project trust (asks once, interactive modes only), then init.lua (+ trusted .mare/init.lua).
    let no_plugins = cli.clean;
    let cli_trust = if cli.trust {
        Some(true)
    } else if cli.no_trust {
        Some(false)
    } else {
        None
    };
    // CLI overrides; the host runs init.lua itself and merges these on top
    let mut cfg = Config::default();
    commands::apply_set_overrides(&mut cfg, &cli.set);
    mare_shared::profile::mark("before trust");
    // the global default as known before init.lua (--set, MARE_TRUST_DEFAULT, state.json); the
    // plugin layer and an init.lua default settle the rest once the host runs (settle_trust)
    let trust_default = mare_config::trust::DefaultTrust::before_config(&cfg.value).map_err(anyhow::Error::msg)?;
    let trust = mare_config::trust::resolve_with_default(cwd, mode == Mode::Tui, cli_trust, trust_default);
    mare_shared::profile::mark("trust resolved");
    let trusted = !no_plugins && trust.trusted;
    if let Some(m) = &cli.model {
        cfg.value["model"] = serde_json::Value::String(m.clone());
    }
    if let Some(e) = &cli.effort {
        cfg.value["effort"] = serde_json::Value::String(e.clone());
    }
    if let Some(t) = &cli.tools {
        let enabled: Vec<String> = t.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        cfg.value["tools_enabled"] = serde_json::json!(enabled);
    }
    if !cli.exclude_tools.is_empty() {
        let excluded: Vec<String> =
            cli.exclude_tools.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        if !excluded.is_empty() {
            cfg.value["tools_excluded"] = serde_json::json!(excluded);
        }
    }
    if let Some(n) = cli.max_turns {
        cfg.set_dotted("plugins.agent.max_turns", serde_json::json!(n));
    }
    if let Some(a) = &cli.append_system_prompt {
        cfg.set_dotted("plugins.context.append", serde_json::Value::String(a.clone()));
    }
    if cli.yolo {
        cfg.set_dotted("plugins.permissions.preset", serde_json::Value::String("full-access".into()));
    } else if let Some(p) = &cli.permission {
        cfg.set_dotted("plugins.permissions.preset", serde_json::Value::String(p.clone()));
    }
    if let Some(ap) = &cli.approval_policy {
        cfg.set_dotted("plugins.approvals.policy", serde_json::Value::String(ap.clone()));
    }
    Ok((cfg, trust, trusted, no_plugins))
}

fn load_output_schema(cli: &Cli, cwd: &Path, mode: Mode) -> std::result::Result<Option<serde_json::Value>, i32> {
    let headless_json = mode == Mode::Json;
    // --timeout and --output-schema bound headless runs only (clap already refuses them with --rpc)
    if cli.timeout.is_some() && !matches!(mode, Mode::Print | Mode::Json) {
        eprintln!("mare: --timeout only applies to headless runs (-p / --json)");
        return Err(2);
    }
    if cli.output_schema.is_some() && !matches!(mode, Mode::Print | Mode::Json) {
        eprintln!("mare: --output-schema only applies to headless runs (-p / --json)");
        return Err(2);
    }
    let Some(path) = &cli.output_schema else {
        return Ok(None);
    };
    let resolved = cwd.join(path);
    let raw = std::fs::read_to_string(&resolved).map_err(|e| {
        crate::headless::usage_error(headless_json, &format!("reading --output-schema {}: {e}", path.display()))
    })?;
    let parsed: serde_json::Value = serde_json::from_str(mare_shared::strip_utf8_bom(&raw)).map_err(|e| {
        crate::headless::usage_error(headless_json, &format!("invalid JSON in --output-schema {}: {e}", path.display()))
    })?;
    mare_shared::schema::OutputSchema::compile(parsed.clone()).map_err(|e| {
        crate::headless::usage_error(
            headless_json,
            &format!("invalid schema in --output-schema {}: {e}", path.display()),
        )
    })?;
    Ok(Some(parsed))
}

async fn resolve_prompt(
    cli: &Cli,
    cwd: &Path,
    mode: Mode,
    deadline: Option<std::time::Instant>,
) -> std::result::Result<String, i32> {
    if !matches!(mode, Mode::Print | Mode::Json) {
        return Ok(join_prompt_args(&cli.prompt, cwd));
    }
    let headless_json = mode == Mode::Json;
    let report_err = |err: StdinError| match err {
        StdinError::Usage(msg) => crate::headless::usage_error(headless_json, &msg),
        StdinError::Timeout => crate::headless::timeout_error(headless_json, "--timeout reached while reading stdin"),
    };
    // `-` (or no prompt with piped stdin) reads the prompt from stdin; a prompt argument
    // gets piped stdin appended as <stdin>…</stdin>
    let dash = cli.prompt.len() == 1 && cli.prompt[0] == "-";
    let args: &[String] = if dash { &[] } else { &cli.prompt };
    let mut prompt = join_prompt_args(args, cwd);
    let piped = !std::io::stdin().is_terminal();
    if dash || (prompt.is_empty() && piped && !cli.continue_) {
        match read_stdin_prompt(deadline).await {
            Ok(s) if !s.trim().is_empty() => prompt = s,
            Ok(_) => return Err(crate::headless::usage_error(headless_json, "no prompt provided via stdin")),
            Err(e) => return Err(report_err(e)),
        }
    } else if piped && (!prompt.is_empty() || cli.continue_) {
        match read_stdin_prompt(deadline).await {
            Ok(s) if !s.trim().is_empty() => {
                prompt = if prompt.is_empty() { s } else { prompt_with_stdin(&prompt, &s) };
            }
            Ok(_) => {}
            Err(e) => return Err(report_err(e)),
        }
    }
    Ok(prompt)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_host_options(
    cli: &Cli,
    cwd: &Path,
    mode: Mode,
    cfg: &Config,
    trust: &mare_config::trust::Trust,
    trusted: bool,
    no_plugins: bool,
    prompt: &str,
    add_dirs: &[String],
    cli_flags: serde_json::Map<String, serde_json::Value>,
) -> HostOptions {
    let mut args = serde_json::json!({
        "continue": cli.continue_,
        "resume": cli.resume,
        "all": cli.all,
        "fork_session": cli.fork_session,
        "session_id": cli.session_id,
        "agent": cli.agent,
        "model": cli.model,
        "effort": cli.effort,
        "name": cli.name,
        "no_session": cli.no_session,
        "prompt": prompt,
        // "trusted" | "untrusted" | "undecided": context labels AGENTS.md of untrusted projects
        "trust": trust.state(),
        "approve_mcps": cli.approve_mcps,
        // the TUI asks with an overlay after its first frame (then loads them, if trusted)
        "trust_pending": !no_plugins && trust.source == mare_config::trust::Source::Pending,
        // what decided (Host::settle_trust: plugins and the init.lua default may still decide)
        "trust_source": trust_source(trust.source),
        "flags": cli_flags,
    });
    if !add_dirs.is_empty() {
        args["add_dirs"] = serde_json::json!(add_dirs);
    }
    let mut cli_flag_paths: Vec<&'static str> = Vec::new();
    if cli.yolo || cli.permission.is_some() {
        cli_flag_paths.push("plugins.permissions.preset");
    }
    if cli.approval_policy.is_some() {
        cli_flag_paths.push("plugins.approvals.policy");
    }
    if !cli_flag_paths.is_empty() {
        args["cli_flag_paths"] = serde_json::json!(cli_flag_paths);
    }
    HostOptions {
        cwd: cwd.to_path_buf(),
        mode,
        config: cfg.value.clone(),
        load_init: !no_plugins,
        project_trusted: trusted,
        args,
        no_user_plugins: no_plugins,
    }
}

async fn run_mode(
    mode: Mode,
    cli: &Cli,
    opts: HostOptions,
    prompt: String,
    output_schema: Option<serde_json::Value>,
    timeout: Option<std::time::Duration>,
) -> Result<i32> {
    match mode {
        Mode::Tui if cli.profile_startup => {
            let file = std::env::temp_dir().join(format!("mare-profile-startup-{}.txt", std::process::id()));
            // SAFETY: single-threaded here, before the TUI starts
            unsafe { std::env::set_var("MARE_PROFILE_STARTUP_EXIT", "1") };
            let code = crate::app::run(opts, None, Some(file.clone()), cli.strict_config).await?;
            if code == 0 {
                match std::fs::read_to_string(&file) {
                    Ok(t) => print!("{t}"),
                    Err(e) => eprintln!("mare: no startup profile written: {e}"),
                }
            }
            let _ = std::fs::remove_file(&file);
            Ok(code)
        }
        // TERM=dumb: no cursor movement at all — plain line-by-line text
        Mode::Tui if crate::plain::is_dumb(&|k| std::env::var(k).ok()) => {
            crate::plain::run(opts, if prompt.is_empty() { None } else { Some(prompt) }, cli.strict_config).await
        }
        Mode::Tui => {
            crate::app::run(
                opts,
                if prompt.is_empty() { None } else { Some(prompt) },
                cli.startuptime.clone(),
                cli.strict_config,
            )
            .await
        }
        Mode::Print | Mode::Json => {
            let json = mode == Mode::Json;
            if json {
                detach_protocol_stdout();
            }
            if prompt.trim().is_empty() && !cli.continue_ {
                return Ok(crate::headless::usage_error(
                    json,
                    "no prompt given (with -c, an empty prompt continues an interrupted turn)",
                ));
            }
            // a host that can't start (init.lua, configuration) is a usage error: exit code 2
            let host = match Host::new(opts).await {
                Ok(h) => h,
                Err(e) => return Ok(crate::headless::usage_error(json, &format!("{e:#}"))),
            };
            if let Err(code) = prepare_non_tui_host(&host, cli.strict_config, cli.peer, Some(json)).await {
                return Ok(code);
            }
            crate::headless::run(host, prompt, json, cli.stream_deltas, timeout, output_schema).await
        }
        Mode::Rpc => {
            let run_stdio = cli.rpc || cli.mode.as_deref() == Some("rpc");
            if run_stdio {
                detach_protocol_stdout();
            }
            let manager = mare_host::HostManager::new();
            let rt = manager.create_session(opts.clone()).await.map_err(|e| anyhow::anyhow!("{e}"))?;
            if let Err(code) = prepare_non_tui_host(&rt.host, cli.strict_config, false, None).await {
                let _ = manager.shutdown_all().await;
                return Ok(code);
            }
            let rpc = crate::rpc::RpcRuntime::new(manager, opts, rt.host.clone(), Some(rt.session_id.clone()), true);
            if cli.peer {
                start_peer_manager(&rpc, &rt.host);
            }
            let cfg = rt.host.config();
            let mut web_server = if crate::web::should_start_webserver(&cfg, cli.web, cli.web_listen.as_deref()) {
                match crate::web::start_host_webserver(&rpc, &cfg, cli.web_listen.as_deref()).await {
                    Ok(srv) => {
                        tracing::info!(addr = %srv.addr, tls = srv.tls, "host webserver started");
                        Some(srv)
                    }
                    Err(msg) => {
                        eprintln!("mare: {msg}");
                        let _ = rpc.manager.shutdown_all().await;
                        return Ok(2);
                    }
                }
            } else {
                None
            };
            let code = if run_stdio {
                crate::rpc::run_manager(rpc.clone()).await?
            } else {
                tokio::select! {
                    _ = crate::signals::terminate() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
                let _ = rpc.manager.shutdown_all_report().await;
                0
            };
            if let Some(srv) = web_server.as_mut() {
                srv.shutdown().await;
            }
            Ok(code)
        }
        Mode::Acp => {
            detach_protocol_stdout();
            crate::acp::run(opts, cli.strict_config).await
        }
        Mode::Child => unreachable!(),
    }
}

async fn prepare_non_tui_host(
    host: &std::sync::Arc<Host>,
    strict_config: bool,
    peer: bool,
    headless_json: Option<bool>,
) -> std::result::Result<(), i32> {
    let fail = |msg: String| match headless_json {
        Some(json) => crate::headless::usage_error(json, &msg),
        None => {
            eprintln!("mare: {msg}");
            2
        }
    };
    if let Err(msg) = report_config_warnings(host, strict_config) {
        host.shutdown().await;
        return Err(fail(msg));
    }
    // a broken init.lua: no UI to fix it from, so report it and stop
    if let Some(msg) = host.config_error() {
        host.shutdown().await;
        return Err(fail(msg));
    }
    report_plugin_warnings(host);
    if let Err(e) = settle_headless_trust(host).await {
        host.shutdown().await;
        return Err(fail(format!("{e:#}")));
    }
    if peer {
        start_peer(host);
    }
    Ok(())
}

/// `mare.args.trust_source`: what decided project trust before the host started.
fn trust_source(s: mare_config::trust::Source) -> &'static str {
    use mare_config::trust::Source;
    match s {
        Source::Cli => "cli",
        Source::Saved | Source::Asked => "saved",
        Source::Default => "default",
        Source::Pending => "pending",
        Source::Undecided => "undecided",
        Source::NotNeeded => "none",
    }
}

/// Headless and RPC never ask: `trust/resolve` plugins and the configured
/// default may still decide; when nothing does, the project resources stay unloaded and stderr
/// says why.
pub(crate) async fn settle_headless_trust(host: &std::sync::Arc<Host>) -> anyhow::Result<()> {
    if host.settle_trust().await? {
        let rr = mare_shared::project::resource_root(&host.cwd);
        let at = rr
            .root
            .as_deref()
            .filter(|r| *r != rr.start.as_path())
            .map(|r| format!(" at {}", r.display()))
            .unwrap_or_default();
        eprintln!(
            "mare: this folder has project resources (.mare/…, .agents/skills){at} that were not loaded: \
             it isn't trusted yet — pass --trust, or open mare here once and answer the trust question"
        );
    }
    Ok(())
}

/// Largest stdin prompt (larger input is a usage error, not unbounded memory).
const MAX_STDIN_PROMPT: usize = 64 * 1024 * 1024;

enum StdinError {
    Usage(String),
    Timeout,
}

fn read_stdin_bytes() -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .take((MAX_STDIN_PROMPT as u64) + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("failed to read the prompt from stdin: {e}"))?;
    Ok(bytes)
}

fn decode_stdin_bytes(bytes: Vec<u8>) -> Result<String, String> {
    if bytes.len() > MAX_STDIN_PROMPT {
        return Err(format!("stdin is larger than {} MiB", MAX_STDIN_PROMPT / (1024 * 1024)));
    }
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    String::from_utf8(bytes.to_vec()).map_err(|e| {
        format!(
            "stdin is not valid UTF-8 (invalid byte at offset {}): pass text, or convert it first",
            e.utf8_error().valid_up_to()
        )
    })
}

/// Read the prompt from stdin to EOF (UTF-8, at most [`MAX_STDIN_PROMPT`]), bounded by `deadline`
/// when `--timeout` is set.
/// Reference: Codex `codex-rs/exec/src/lib.rs:2643-2674` (`read_to_end` without a first-byte cutoff).
async fn read_stdin_prompt(deadline: Option<std::time::Instant>) -> Result<String, StdinError> {
    let bytes = match deadline {
        None => read_stdin_bytes().map_err(StdinError::Usage)?,
        Some(dl) => {
            let remaining = dl.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(StdinError::Timeout);
            }
            let (tx, rx) = tokio::sync::oneshot::channel();
            std::thread::spawn(move || {
                let _ = tx.send(read_stdin_bytes());
            });
            match tokio::time::timeout(remaining, rx).await {
                Ok(Ok(Ok(bytes))) => bytes,
                Ok(Ok(Err(e))) => return Err(StdinError::Usage(e)),
                Ok(Err(_)) => return Err(StdinError::Usage("failed to read the prompt from stdin".into())),
                Err(_) => return Err(StdinError::Timeout),
            }
        }
    };
    decode_stdin_bytes(bytes).map_err(StdinError::Usage)
}

/// A prompt argument plus piped stdin.
fn prompt_with_stdin(prompt: &str, stdin: &str) -> String {
    let mut out = format!("{prompt}\n\n<stdin>\n{stdin}");
    if !stdin.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("</stdin>");
    out
}

/// Configuration warnings (unknown `mare.o.*` names, type errors, bad `MARE_*` values) go to
/// stderr in non-TUI modes; `--strict-config` turns them into a usage error (exit code 2) in all
/// modes including TUI.
pub(crate) fn report_config_warnings(host: &Host, strict: bool) -> std::result::Result<(), String> {
    let mut warnings = mare_config::env_warnings();
    warnings.extend(mare_shared::lua::core::config::warnings_from_lua(host.lua()));
    if warnings.is_empty() {
        return Ok(());
    }
    if strict {
        return Err(format!("configuration warnings (--strict-config): {}", warnings.join("; ")));
    }
    for w in &warnings {
        eprintln!("mare: config warning: {w}");
    }
    Ok(())
}

/// Headless / RPC: plugins that failed at startup without blocking requests are
/// reported once on stderr; the run goes on without them.
pub(crate) fn report_plugin_warnings(host: &Host) {
    for w in host.plugin_warnings() {
        eprintln!("mare: warning: {w}");
    }
}

/// Serve the peer socket and register; a failure doesn't stop the session, but is
/// never silent — other sessions can't reach this one (on Windows, e.g., when another local user
/// holds its pipe's name): a notice in the TUI, stderr otherwise (stdout carries the protocol).
pub(crate) fn start_peer(host: &std::sync::Arc<Host>) {
    report_peer_start(host, crate::peer::start(host));
}

pub(crate) fn start_peer_manager(rpc: &std::rc::Rc<crate::rpc::RpcRuntime>, host: &std::sync::Arc<Host>) {
    report_peer_start(host, crate::peer::start_manager(rpc.clone(), host));
}

fn report_peer_start(host: &std::sync::Arc<Host>, result: Result<bool>) {
    let Err(e) = result else { return };
    let msg = format!("peer socket unavailable, other sessions can't reach this one: {e:#}");
    tracing::warn!("{msg}");
    let notify = || -> mlua::Result<()> {
        let ui = host.lua().globals().get::<mlua::Table>("mare")?.get::<mlua::Table>("ui")?;
        ui.get::<mlua::Function>("notify")?.call::<()>((msg.as_str(), "warning"))
    };
    if host.mode != Mode::Tui {
        eprintln!("mare: warning: {msg}");
    } else if let Err(e) = notify() {
        tracing::warn!("peer socket notice: {e}");
    }
}

/// Apply any staged standalone update at startup and re-exec the newly swapped binary.
fn maybe_relaunch_with_staged_update() {
    let applied = match mare_plugin::update::apply_staged_update() {
        Ok(Some(applied)) => applied,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!("staged update swap skipped: {e:#}");
            return;
        }
    };
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err =
            std::process::Command::new(&applied.exe_path).args(&args).env(mare_plugin::update::REEXEC_ENV, "1").exec();
        tracing::warn!("re-exec after staged update failed: {err}");
    }
    #[cfg(not(unix))]
    {
        match std::process::Command::new(&applied.exe_path)
            .args(&args)
            .env(mare_plugin::update::REEXEC_ENV, "1")
            .status()
        {
            Ok(status) => std::process::exit(status.code().unwrap_or(1)),
            Err(err) => tracing::warn!("re-exec after staged update failed: {err}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::join_prompt_args;

    #[test]
    fn unit_process_uses_nextest_private_home() {
        let home = std::path::PathBuf::from(std::env::var_os("MARE_HOME").expect("private MARE_HOME"));
        assert_eq!(std::env::var("MARE_NEXTEST_HOME_DEPTH").as_deref(), Ok("1"));
        assert!(mare_shared::paths::host_token_file().starts_with(home));
    }

    #[test]
    fn join_prompt_args_preserves_inline_mentions_and_quotes_real_space_paths() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(join_prompt_args(&["@a.txt summarize it".into()], dir.path()), "@a.txt summarize it");
        let spaced = dir.path().join("my file.txt");
        std::fs::write(&spaced, "ok").unwrap();
        let abs_arg = format!("@{}", spaced.display());
        assert_eq!(join_prompt_args(&[abs_arg], dir.path()), format!("@\"{}\"", spaced.display()));
        assert_eq!(join_prompt_args(&["@my file.txt".into()], dir.path()), "@\"my file.txt\"");
    }
}
