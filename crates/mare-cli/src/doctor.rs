//! `mare doctor`: config, auth, connectivity, plugin problems, runtime dir. Plugins add their
//! own checks by listening to the `doctor` event and appending to the passed table:
//!
//! ```lua
//! ctx:on("kernel/doctor", function(checks) table.insert(checks, { name = "mcp", ok = true, detail = "2 servers" }) end)
//! ```

use std::path::Path;

use anyhow::Result;
use mlua::{MultiValue, Table, Value};

use mare_host::{Host, HostOptions};
use mare_shared::ui::Mode;

struct Report {
    failures: usize,
    /// ANSI colours: only on a terminal and without `NO_COLOR` (no-color.org)
    color: bool,
    /// `--json`: checks are collected and printed as one JSON document at the end
    json: Option<Vec<serde_json::Value>>,
}

impl Report {
    fn line(&mut self, ok: bool, name: &str, detail: &str) {
        self.check(ok, false, name, detail);
    }

    /// `warn`: shown with `!` (yellow) and never counted as a failure — something to know
    /// about that works
    fn check(&mut self, ok: bool, warn: bool, name: &str, detail: &str) {
        if !ok && !warn {
            self.failures += 1;
        }
        if let Some(j) = &mut self.json {
            let mut v = serde_json::json!({ "name": name.trim(), "ok": ok, "detail": detail });
            if warn {
                v["warn"] = serde_json::Value::Bool(true);
            }
            j.push(v);
            return;
        }
        let mark = if warn {
            "!"
        } else if ok {
            "✓"
        } else {
            "✗"
        };
        if self.color {
            let color = if warn {
                "33"
            } else if ok {
                "32"
            } else {
                "31"
            };
            println!("\x1b[{color}m{mark}\x1b[0m {name:<22} \x1b[2m{detail}\x1b[0m");
        } else {
            println!("{mark} {name:<22} {detail}");
        }
    }
}

/// Colour only when stdout is a terminal and `NO_COLOR` is unset or empty.
pub(crate) fn use_color() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
}

pub async fn run(cwd: &Path, trusted: bool, fix: bool, json: bool) -> Result<i32> {
    let mut r = Report { failures: 0, color: !json && use_color(), json: json.then(Vec::new) };
    if !json {
        println!("mare {} — doctor\n", env!("CARGO_PKG_VERSION"));
    }

    // directories and permissions: `--fix` only creates what's missing and tightens modes
    // (dirs 0700, credential / session files 0600) — nothing else is ever changed
    for (name, detail, ok) in fs_checks(fix) {
        r.line(ok, name, &detail);
    }

    check_terminal(&mut r);
    check_config(&mut r, cwd, trusted).await;
    check_auth_file(&mut r);

    // connectivity: only the configured providers, by the llm plugin (`doctor` event below)
    check_sandbox(&mut r, cwd);
    check_tmux(&mut r);
    // stdout/stderr capture: what mare can intercept is shown above the
    // live area; a program that opens the terminal device itself is outside that guarantee
    r.line(
        true,
        "external output",
        "output written to mare's stdout/stderr (plugins, libraries, child processes) is captured above the input box; programs that write to the terminal device directly can still disturb the screen",
    );
    check_sessions(&mut r);
    check_crash_guard(&mut r);

    // runtime dir
    match mare_plugin::runtime::ensure() {
        Ok(d) => r.line(true, "runtime", &d.display().to_string()),
        Err(e) => r.line(false, "runtime", &e.to_string()),
    }

    check_plugins(&mut r, cwd, trusted).await?;
    finish_report(r)
}

fn check_terminal(r: &mut Report) {
    // the terminal on every platform
    let var = |k: &str| std::env::var(k).ok();
    match mare_tui::termcaps::detect(&var) {
        Some(c) => {
            let yn = |b: bool| if b { "yes" } else { "no" };
            let mut d = format!(
                "{}: truecolor {}, wide emoji {}, kitty keyboard {}, bracketed paste {}",
                c.name,
                yn(c.truecolor),
                yn(c.emoji_wide),
                yn(c.kitty_keyboard),
                yn(c.bracketed_paste)
            );
            if let Some(q) = c.quirk {
                d.push_str(" — ");
                d.push_str(q);
            }
            r.line(c.quirk.is_none(), "terminal capabilities", &d);
        }
        None => r.line(
            true,
            "terminal capabilities",
            &format!(
                "unknown terminal (TERM={}): capabilities come from the startup probe only",
                var("TERM").unwrap_or_default()
            ),
        ),
    }

    // Windows: legacy conhost and Windows Terminal key forwarding
    if cfg!(windows) {
        let var = |k: &str| std::env::var(k).ok();
        for (ok, name, detail) in crate::terminal::check(&var) {
            r.line(ok, name, &detail);
        }
    }
}

async fn check_config(r: &mut Report, cwd: &Path, trusted: bool) {
    let init = mare_config::user_init();
    match mare_plugin::config_load::load(cwd, trusted).await {
        Ok(cfg) => {
            let detail =
                if init.exists() { init.display().to_string() } else { "no init.lua (mare init creates one)".into() };
            r.line(true, "config", &detail);
            let model = cfg.get_str("model").unwrap_or("(default)").to_string();
            r.line(true, "model", &model);
            // typos / type errors in mare.setup, mare.o and mare.plugin (did-you-mean warnings)
            r.line(
                cfg.warnings.is_empty(),
                "config options",
                &if cfg.warnings.is_empty() {
                    "no unknown names or type errors".into()
                } else {
                    cfg.warnings.join("; ")
                },
            );
        }
        Err(e) => r.line(false, "config", &format!("{e:#}")),
    }
    let rr = mare_shared::project::resource_root(cwd);
    let root_desc = match &rr.root {
        Some(root) => format!("{} (started in {})", root.display(), rr.start.display()),
        None => format!("none (started in {})", rr.start.display()),
    };
    r.line(true, "project resource root", &root_desc);
    if !rr.ignored.is_empty() {
        let list: Vec<_> = rr.ignored.iter().map(|p| p.display().to_string()).collect();
        r.check(false, true, "ignored project markers", &list.join(", "));
    }
    if !rr.rejected.is_empty() {
        let list: Vec<_> = rr.rejected.iter().map(|p| p.display().to_string()).collect();
        r.check(false, true, "ignored symlinks", &format!("target outside the root: {}", list.join(", ")));
    }
}

fn check_auth_file(r: &mut Report) {
    // auth.json: malformed or unsupported files are reported, never treated as "no credentials".
    let auth = mare_shared::paths::auth_file();
    let expected = mare_host::artifacts::get(mare_shared::artifact::ArtifactId::Auth)
        .and_then(|artifact| artifact.version)
        .map(u64::from);
    let problem = match expected {
        Some(version) => mare_config::versioned_json::read(&auth, version).err(),
        None => Some("auth.json format version is not registered".into()),
    };
    if let Some(problem) = problem {
        r.line(
            false,
            "auth.json",
            &format!(
                "{problem}: saved credentials can't be read or changed until it's fixed; fix the file or delete it and log in again"
            ),
        );
    }
    #[cfg(unix)]
    if auth.exists() {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&auth).map(|m| m.permissions().mode() & 0o777).unwrap_or(0);
        r.line(mode & 0o077 == 0, "auth.json mode", &format!("{mode:o}"));
    }
}

fn check_sandbox(r: &mut Report, cwd: &Path) {
    // sandbox: the backend and the capability dimensions probed on this host
    let (ok, detail) = mare_binding::ossandbox::capability_summary();
    r.line(ok, "sandbox", &detail);
    // an approved escalation under restricted reads: kept sandboxed, or refused —
    // a warning only, it matters once a session restricts reads
    let b = mare_binding::ossandbox::backend();
    let (ok, detail) = mare_binding::deny_read::capability_line(b, &b.caps());
    r.check(ok, !ok, "sandbox deny-read", &detail);
    let (ok, detail) = mare_binding::deny_read::explicit_capability_line(b, &b.caps());
    r.check(ok, !ok, "sandbox explicit deny-read", &detail);
    // per-domain network rules: forced through the filtering proxy, or refused
    if let Some((ok, detail)) = mare_binding::ossandbox::net_proxy_line() {
        r.check(ok, !ok, "sandbox net_proxy", &detail);
    }
    // Windows: Git Bash in the sandbox — the enhanced tier runs it with its directory
    // locked against the sandbox accounts; the default tier can't, so commands default to PowerShell
    if let Some((ok, warn, detail)) = mare_binding::ossandbox::msys_report() {
        r.check(ok, warn, "sandbox git bash", &detail);
    }
    // Windows: a workspace containing %TEMP% (home folder, `C:\`) shares its write access with
    // the private temp directory — allowed, reported
    match mare_binding::ossandbox::resolve(cwd) {
        Ok(root) => {
            if let Some(note) = mare_binding::ossandbox::temp_isolation_note(std::slice::from_ref(&root)) {
                r.check(false, true, "sandbox temp", note);
            }
        }
        Err(err) => r.check(false, true, "sandbox temp", &format!("cannot resolve workspace: {err}")),
    }
    r.line(true, "network rules", mare_binding::netauth::GUARANTEE);
}

fn check_tmux_values(r: &mut Report, clipboard: Option<&str>, extended_keys: Option<&str>) {
    match clipboard {
        Some("on") | Some("external") => r.line(true, "tmux clipboard", "set-clipboard is on (OSC 52 copy works)"),
        Some(other) => r.line(
            false,
            "tmux clipboard",
            &format!("set-clipboard is {other:?}: copying from the live area won't reach the system clipboard — add `set -g set-clipboard on` to tmux.conf"),
        ),
        None => r.line(false, "tmux clipboard", "couldn't run `tmux show -gv set-clipboard`"),
    }
    // Shift+Enter inside tmux arrives as plain Enter unless tmux forwards extended keys
    match extended_keys {
        Some("on") | Some("always") => r.line(true, "tmux keys", "extended-keys is on"),
        _ => r.line(
            false,
            "tmux keys",
            "extended-keys is off: Shift+Enter arrives as Enter — use `\\` + Enter or Alt+Enter for a newline, or add `set -g extended-keys on` to tmux.conf",
        ),
    }
}

fn check_tmux(r: &mut Report) {
    if std::env::var_os("TMUX").is_none() {
        return;
    }
    let v = std::process::Command::new("tmux")
        .args(["show", "-gv", "set-clipboard"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let ek = std::process::Command::new("tmux")
        .args(["show", "-gv", "extended-keys"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    check_tmux_values(r, v.as_deref(), ek.as_deref());
}

fn check_sessions(r: &mut Report) {
    // session files: resuming trusts the checkpoint (length + mtime) and
    // parses only what follows it, so a damaged older prefix shows up here — the one place that
    // strictly parses every session in full
    let root = mare_shared::paths::sessions_dir();
    let (mut checked, mut bad, mut torn) = (0usize, vec![], vec![]);
    for path in mare_store::index::session_files(&root) {
        match mare_store::check_file(&path) {
            Ok(n) => {
                checked += 1;
                // a half-written last entry (crash mid-write): normal, cut before the next
                // append — listed so a crash leaves a trace
                if n > 0 {
                    torn.push(format!("{} ({n} bytes)", path.display()));
                }
            }
            Err(e) => bad.push(format!("{}: {e}", path.display())),
        }
    }
    if bad.is_empty() {
        r.line(true, "session files", &format!("{checked} parsed"));
    } else {
        r.line(false, "session files", &format!("damaged (view read-only; back up or repair): {}", bad.join("; ")));
    }
    if !torn.is_empty() {
        r.check(
            false,
            true,
            "session torn writes",
            &format!("half-written last entry, ignored and cut at the next resume: {}", torn.join("; ")),
        );
    }
    session_index(r, &root);
}

fn check_crash_guard(r: &mut Report) {
    check_crash_guard_in(r, &mare_shared::paths::state_dir());
}

fn check_crash_guard_in(r: &mut Report, state_dir: &Path) {
    let backups = mare_binding::crashguard::quarantined_backups(state_dir);
    if !backups.is_empty() {
        let names: Vec<_> =
            backups.iter().map(|p| p.file_name().unwrap_or_default().to_string_lossy().into_owned()).collect();
        r.check(
            false,
            true,
            "crash guard",
            &format!("corrupt crash-guard.json was quarantined and crash counts reset: {}", names.join(", ")),
        );
    }
}

async fn check_plugins(r: &mut Report, cwd: &Path, trusted: bool) -> Result<()> {
    // plugins (+ checks contributed by plugins via the `doctor` event)
    let opts = HostOptions {
        cwd: cwd.to_path_buf(),
        mode: Mode::Print,
        config: serde_json::json!({}),
        load_init: true,
        project_trusted: trusted,
        args: serde_json::json!({ "no_session": true }),
        no_user_plugins: false,
    };
    let host = match Host::new(opts).await {
        Ok(h) => h,
        Err(e) => {
            r.line(false, "plugins", &format!("{e:#}"));
            return Ok(());
        }
    };
    let problems = host.kernel.problems();
    let names = host.kernel.plugin_status();
    let active = names.iter().filter(|p| p.2).count();
    r.line(problems.is_empty(), "plugins", &format!("{active}/{} active", names.len()));
    for p in &problems {
        r.line(false, "  plugin", p);
    }
    let (checked_modules, module_errors) = host.kernel.precheck_third_party_modules();
    if module_errors.is_empty() {
        r.line(true, "plugin modules", &format!("{checked_modules} checked"));
    } else {
        r.line(false, "plugin modules", &format!("{} of {checked_modules} failed to compile", module_errors.len()));
        for (plugin, module, err) in &module_errors {
            r.line(false, "  plugin module", &format!("{plugin} ({module}): {err}"));
        }
    }
    for (plugin, reporter, key, _kind, message) in host.kernel.declaration_problems() {
        r.check(false, true, "  declaration", &format!("[{reporter}] {plugin}: {message} ({key})"));
    }
    // `override = true` replaced a same-name definition
    for (name, was) in host.kernel.plugin_overrides() {
        let what = if was == mare_kernel::Origin::Builtin {
            "built-in".to_string()
        } else {
            format!("{} plugin", was.as_str())
        };
        r.line(true, "  override", &format!("{name} overrides {what} {name}"));
    }
    check_permission_policy(r, &host);
    check_key_conflicts(r, &host);
    check_missing_essential_keys(r, &host);
    check_unbound_keymap_actions(r, &host);
    emit_plugin_doctor_checks(r, &host).await?;
    host.shutdown().await;
    Ok(())
}

fn check_permission_policy(r: &mut Report, host: &Host) {
    let policy = mare_binding::ossandbox::current(host.lua());
    let sandbox = policy.as_ref().map_or("none", |p| p.level.as_str());
    if host.service_table("permissions").is_some() {
        let provider = host
            .kernel
            .service_provider("permissions")
            .map(|(name, origin)| format!("{name} ({})", origin.as_str()))
            .unwrap_or_else(|| "unknown provider".into());
        let detail = format!(
            "permissions service: {provider}; session sandbox: {sandbox}; provider behavior is not verified; policy hooks may add restrictions"
        );
        r.check(policy.is_some(), policy.is_none(), "permission policy", &detail);
    } else {
        r.check(
            false,
            true,
            "permission policy",
            &format!("no permissions service; session sandbox: {sandbox}; built-in approvals unavailable; other policy hooks may apply"),
        );
    }
}

fn check_key_conflicts(r: &mut Report, host: &Host) {
    // key bindings a terminal / multiplexer is known to take first
    let binds: Vec<(String, String)> = host
        .ui
        .keymap
        .read()
        .map(|m| m.bindable_contexts().into_iter().flat_map(|c| m.bindings(c)).collect())
        .unwrap_or_default();
    let mut table = mare_tui::keyconflict::builtin();
    if let Some(list) = host.kernel.config().get("key_conflicts").and_then(|v| v.as_array().cloned()) {
        for e in list {
            let s = |k: &str| e.get(k).and_then(|v| v.as_str()).map(str::to_string);
            if let Some(key) = s("key") {
                table.push(mare_tui::keyconflict::Entry {
                    key,
                    terminal: s("terminal").unwrap_or_else(|| "any".into()),
                    by: s("by").unwrap_or_else(|| "the terminal or OS".into()),
                    fix: s("fix").unwrap_or_else(|| "rebind it in mare".into()),
                });
            }
        }
    }
    let terms = mare_tui::keyconflict::terminal_id(&|k| std::env::var(k).ok());
    let found = mare_tui::keyconflict::find(&binds, &table, &terms);
    let detail = if found.is_empty() {
        "none for this terminal".to_string()
    } else {
        found.iter().map(|c| format!("{} ({}) — {}; {}", c.key, c.action, c.by, c.fix)).collect::<Vec<_>>().join("; ")
    };
    r.line(found.is_empty(), "key conflicts", &detail);
}

fn check_missing_essential_keys(r: &mut Report, host: &Host) {
    let missing = host.ui.keymap.read().map(|m| m.missing_essential()).unwrap_or_default();
    for (action, context) in missing {
        r.check(false, true, "key", &format!("{action} has no key in \"{context}\""));
    }
}

fn check_unbound_keymap_actions(r: &mut Report, host: &Host) {
    let unbound = host.ui.keymap.read().map(|m| m.unbound_essential_actions()).unwrap_or_default();
    let detail = if unbound.is_empty() {
        "none".to_string()
    } else {
        let list = unbound.iter().map(|(ctx, action)| format!("{action} ({ctx})")).collect::<Vec<_>>().join(", ");
        format!("no key bound for {list} ({} still cancels modals)", mare_shared::bindings::key_label("ctrl+c"))
    };
    r.check(unbound.is_empty(), !unbound.is_empty(), "unbound keymap actions", &detail);
}

async fn emit_plugin_doctor_checks(r: &mut Report, host: &Host) -> Result<()> {
    let checks: Table = host.lua().create_table()?;
    host.kernel.emit("kernel/doctor", MultiValue::from_vec(vec![Value::Table(checks.clone())])).await;
    for c in checks.sequence_values::<Table>().flatten() {
        let name: String = c.get::<Option<String>>("name").ok().flatten().unwrap_or_else(|| "plugin check".into());
        let ok = c.get::<Option<bool>>("ok").ok().flatten().unwrap_or(false);
        let warn = c.get::<Option<bool>>("warn").ok().flatten().unwrap_or(false);
        let detail = c.get::<Option<String>>("detail").ok().flatten().unwrap_or_default();
        r.check(ok, warn, &name, &detail);
    }
    Ok(())
}

fn finish_report(mut r: Report) -> Result<i32> {
    if let Some(checks) = r.json.take() {
        let doc = serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "ok": r.failures == 0,
            "failures": r.failures,
            "checks": checks,
        });
        println!("{}", serde_json::to_string_pretty(&doc)?);
        return Ok(if r.failures == 0 { 0 } else { 1 });
    }
    println!();
    if r.failures == 0 {
        println!("all checks passed");
        Ok(0)
    } else {
        println!("{} check(s) failed", r.failures);
        Ok(1)
    }
}

/// The session index: ok / created / rebuilt (an
/// unusable file set aside) / degraded (can't be opened: listings scan the session files), and
/// its rows against the session files after a reconcile.
fn session_index(r: &mut Report, root: &Path) {
    let files = mare_store::index::session_files(root).len();
    let s = match mare_store::index::ready(root) {
        Ok(s) => s,
        Err(e) => {
            r.line(false, "session index", &format!("degraded, /resume scans the session files: {e:#}"));
            return;
        }
    };
    let rec = mare_store::index::reconcile(&s);
    let ix = s.lock().unwrap_or_else(|e| e.into_inner());
    let rows = ix.count().unwrap_or(-1);
    let (state, warn) = match ix.status() {
        mare_store::index::Status::Ok => ("ok".to_string(), false),
        mare_store::index::Status::Created => ("created".to_string(), false),
        mare_store::index::Status::Rebuilt(why) => (format!("rebuilt ({why})"), true),
    };
    let note = match rec {
        Ok(rec) if rec.failed > 0 => format!("; {} files without a session header aren't indexed", rec.failed),
        Ok(_) => String::new(),
        Err(e) => format!("; reconcile failed: {e:#}"),
    };
    let ok = !warn && note.is_empty() && rows == files as i64;
    let detail = format!("{state}: {rows} rows, {files} session files ({} journal){note}", ix.journal_mode());
    r.check(ok, true, "session index", &detail);
}

/// Directory and permission checks; with `fix`, create missing directories and set 0700 / 0600.
fn fs_checks(fix: bool) -> Vec<(&'static str, String, bool)> {
    use mare_shared::paths;
    let dirs = [paths::config_dir(), paths::state_dir(), paths::data_dir(), paths::sessions_dir(), paths::log_dir()];
    let mut priv_paths = Vec::new();
    priv_paths.push((paths::auth_file(), 0o600));
    if let Some(host_dir) = paths::host_token_file().parent() {
        priv_paths.push((host_dir.to_path_buf(), 0o700));
    }
    priv_paths.push((paths::host_token_file(), 0o600));
    priv_paths.push((paths::sessions_dir(), 0o700));
    priv_paths.push((paths::state_dir(), 0o700));
    visit_session_jsonl(&paths::sessions_dir(), &mut |p| priv_paths.push((p, 0o600)));
    fs_checks_with(fix, &dirs, &priv_paths, &mut set_mode)
}

fn fs_checks_with(
    fix: bool,
    dirs: &[std::path::PathBuf],
    priv_paths: &[(std::path::PathBuf, u32)],
    apply_mode: &mut impl FnMut(&std::path::Path, u32) -> std::io::Result<()>,
) -> Vec<(&'static str, String, bool)> {
    let mut out = vec![];
    let mut missing = vec![];
    let mut loose = vec![];
    for d in dirs {
        if !d.exists() {
            if fix && std::fs::create_dir_all(d).is_ok() {
                if let Err(e) = apply_mode(d, 0o700)
                    && !priv_paths.iter().any(|(pp, _)| pp == d)
                {
                    loose.push(format!("{} ({e})", d.display()));
                }
            } else {
                missing.push(d.display().to_string());
            }
        }
    }
    out.push((
        "directories",
        if missing.is_empty() {
            "present".into()
        } else {
            format!("missing: {} (mare doctor --fix creates them)", missing.join(", "))
        },
        missing.is_empty(),
    ));
    {
        let mut check = |p: &std::path::Path, want: u32| {
            let Ok(meta) = std::fs::metadata(p) else { return };
            if !is_open(p, &meta) {
                return;
            }
            if fix {
                if let Err(e) = apply_mode(p, want) {
                    loose.push(format!("{} ({e})", p.display()));
                    return;
                }
                if let Ok(after) = std::fs::metadata(p)
                    && is_open(p, &after)
                {
                    loose.push(p.display().to_string());
                }
            } else {
                loose.push(p.display().to_string());
            }
        };
        for (p, want) in priv_paths {
            check(p, *want);
        }
        out.push((
            "permissions",
            if loose.is_empty() {
                "credentials and sessions are private".into()
            } else if cfg!(windows) {
                format!("readable by other users: {} (mare doctor --fix restricts them to you)", loose.join(", "))
            } else {
                format!("readable by group/others: {} (mare doctor --fix sets 0700 / 0600)", loose.join(", "))
            },
            loose.is_empty(),
        ));
    }
    out
}

/// Unix: any group/other mode bits; Windows: a DACL that admits more than this user, SYSTEM, and
/// Administrators (`mare_shared::private`).
fn is_open(p: &std::path::Path, meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = p;
        meta.permissions().mode() & 0o077 != 0
    }
    #[cfg(windows)]
    {
        let _ = meta;
        mare_shared::private::check(p).is_err()
    }
}

/// Unix: chmod `mode`; Windows (no mode bits): the private DACL of `mare_shared::private`.
fn set_mode(p: &std::path::Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(windows)]
    {
        let _ = mode;
        mare_shared::private::restrict(p)
    }
}

/// Recursively visit `.jsonl` session files under `root` without following symlinks.
fn visit_session_jsonl(root: &Path, visit: &mut impl FnMut(std::path::PathBuf)) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() && path.extension().is_some_and(|x| x == "jsonl") {
                visit(path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_crash_guard_reports_quarantined_backups() {
        let dir = tempfile::tempdir().unwrap();
        let mut clean = Report { failures: 0, color: false, json: Some(Vec::new()) };
        check_crash_guard_in(&mut clean, dir.path());
        assert!(clean.json.as_ref().unwrap().is_empty());

        std::fs::write(dir.path().join("crash-guard.json.bad-0000000000001"), b"{bad").unwrap();
        let mut warned = Report { failures: 0, color: false, json: Some(Vec::new()) };
        check_crash_guard_in(&mut warned, dir.path());
        let rows = warned.json.as_ref().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["name"], "crash guard");
        assert_eq!(rows[0]["warn"], true);
        assert!(rows[0]["detail"].as_str().unwrap().contains("crash-guard.json.bad-0000000000001"));
    }

    #[test]
    fn check_tmux_values_reports_extended_keys_and_clipboard_states() {
        let mut ok_report = Report { failures: 0, color: false, json: Some(Vec::new()) };
        check_tmux_values(&mut ok_report, Some("on"), Some("always"));
        assert_eq!(ok_report.failures, 0);
        let ok_rows = ok_report.json.as_ref().unwrap();
        assert_eq!(ok_rows.len(), 2);
        assert_eq!(ok_rows[0]["name"], "tmux clipboard");
        assert_eq!(ok_rows[0]["ok"], true);
        assert_eq!(ok_rows[1]["name"], "tmux keys");
        assert_eq!(ok_rows[1]["ok"], true);

        let mut bad_report = Report { failures: 0, color: false, json: Some(Vec::new()) };
        check_tmux_values(&mut bad_report, Some("off"), Some("off"));
        assert_eq!(bad_report.failures, 2);
        let bad_rows = bad_report.json.as_ref().unwrap();
        assert_eq!(bad_rows[0]["ok"], false);
        assert!(bad_rows[0]["detail"].as_str().unwrap().contains("set-clipboard"));
        assert_eq!(bad_rows[1]["ok"], false);
        let keys_detail = bad_rows[1]["detail"].as_str().unwrap();
        assert!(keys_detail.contains("extended-keys is off") && keys_detail.contains("Shift+Enter"), "{keys_detail}");
    }

    #[test]
    fn fs_checks_fix_reports_error_and_unmodified_permissions_as_failures() {
        let dir = tempfile::tempdir().unwrap();
        let sess = dir.path().join("s1.jsonl");
        std::fs::write(&sess, "{}\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&sess, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        #[cfg(windows)]
        mare_shared::private::testing::open_to_everyone(&sess);

        // 1) OS refuses permission tightening -> permissions check fails, names file, exit code 1.
        let checks = fs_checks_with(true, &[], &[(sess.clone(), 0o600)], &mut |_, _| {
            Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "read-only fs"))
        });
        let perm = checks.iter().find(|c| c.0 == "permissions").unwrap();
        assert!(!perm.2, "{perm:?}");
        assert!(perm.1.contains("s1.jsonl") && perm.1.contains("read-only fs"), "{perm:?}");
        let mut r = Report { failures: 0, color: false, json: Some(Vec::new()) };
        for (name, detail, ok) in checks {
            r.line(ok, name, &detail);
        }
        assert_eq!(finish_report(r).unwrap(), 1);

        // 2) set_mode returns Ok without tightening permissions -> post-check still fails.
        let checks_noop = fs_checks_with(true, &[], &[(sess.clone(), 0o600)], &mut |_, _| Ok(()));
        let perm_noop = checks_noop.iter().find(|c| c.0 == "permissions").unwrap();
        assert!(!perm_noop.2, "{perm_noop:?}");
        assert!(perm_noop.1.contains("s1.jsonl"), "{perm_noop:?}");

        // 3) Real set_mode succeeds -> permissions check passes and file mode/DACL is private.
        let checks_ok = fs_checks_with(true, &[], &[(sess.clone(), 0o600)], &mut set_mode);
        let perm_ok = checks_ok.iter().find(|c| c.0 == "permissions").unwrap();
        assert!(perm_ok.2, "{perm_ok:?}");
        assert_eq!(perm_ok.1, "credentials and sessions are private");
        assert!(!is_open(&sess, &std::fs::metadata(&sess).unwrap()));
    }
}
