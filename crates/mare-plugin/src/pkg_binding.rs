//! `mare.plugins`: the package manager for the `/plugins` panel. The same operations as
//! `mare plugin …`; git and the network run on the blocking pool, and each returns its log lines.
//!
//! - `list_installed()` -> `{ {name, display, url, dir, local, installed, disabled, commit, locked,
//!   dirty, problem, constraint, branch} }` (`commit` is what's checked out, `locked` the lock's)
//! - `run(action, name?, opts?)` -> `{ok, log, error?}` — action = sync | install | update |
//!   uninstall | restore | clean | log | diff (log / diff need a name); `opts.on_line(line)` sees
//!   the log as it comes
//!   (live progress), `opts.cancel` (a `mare.task.token`) stops git. Dropping the call (the task
//!   is aborted: quit, plugin disposed) stops git too.
//! - `install` / `update` / `uninstall` accept catalog plugin names or declared source names.
//! - `catalog()` -> the catalog with `enabled` / `recommended` / `needed_by` / `managed`
//! - `set_enabled(name, on)` -> true | nil, err (conflicting or required plugins are refused)
//! - `fetch_catalog()` -> `{ok, log, error?}` (clone sources without enabling anything)
//! - `doctor()` -> `{ {name, ok, detail} }`: git and its version, the lock, plugin name clashes
//! - `watched()` -> paths whose change means "/reload to apply" (init.lua, plugins.json, the
//!   trusted project's `.mare/init.lua`, and the `lua/` modules the last config evaluation
//!   required)
//! - `dirs(kind)` -> `{ {name, dir} }`: the installed packages' `skills/`,
//!   `prompts/`, `themes/` or `types/` directories (or custom paths from `mare-plugin.toml`) in load
//!   order

use mlua::{Function, Lua, Table, Value};
use serde_json::{Value as J, json};
use tokio_util::sync::CancellationToken;

use mare_shared::lua::{config_to_json, json_to_lua, rt_err};

fn current_config(lua: &Lua) -> mlua::Result<J> {
    let mare: Table = lua.globals().get("mare")?;
    let mut config = config_to_json(lua, mare.get::<Value>("config")?)?;
    let cwd: String = mare.get("cwd")?;
    let trusted = mare.get::<Option<bool>>("trusted")?.unwrap_or(false);
    mare_plugin_manager::set_project_scope(&mut config, std::path::Path::new(&cwd), trusted);
    Ok(config)
}

fn outcome(lua: &Lua, r: anyhow::Result<()>, log: Vec<String>, cancelled: bool) -> mlua::Result<Value> {
    let log: Vec<String> = log.into_iter().map(|line| mare_shared::redact::redact_str(&line)).collect();
    let v = match r {
        Ok(()) if !cancelled => json!({ "ok": true, "log": log }),
        Ok(()) => json!({ "ok": false, "log": log, "error": "cancelled", "cancelled": true }),
        Err(e) if cancelled => json!({
            "ok": false,
            "log": log,
            "error": mare_shared::redact::redact_str(&format!("{e:#}")),
            "cancelled": true
        }),
        Err(e) => json!({ "ok": false, "log": log, "error": mare_shared::redact::redact_str(&format!("{e:#}")) }),
    };
    json_to_lua(lua, &v)
}

fn manifest_info(m: mare_plugin_manager::Manifest) -> J {
    let mut v = serde_json::to_value(m).unwrap_or(J::Null);
    if let Some(map) = v.as_object_mut() {
        map.remove("recommended");
    }
    v
}

/// A token for one blocking package operation: cancelled by the caller's `opts.cancel`, and when
/// the awaiting Lua call is dropped (its task aborted), so git never outlives the request.
fn op_token(opts: &Option<Table>) -> CancellationToken {
    match mare_binding::task::opt_token(opts) {
        Some(t) => t.child_token(),
        None => CancellationToken::new(),
    }
}

async fn run_blocking<T: Send + 'static>(lua: &Lua, work: impl FnOnce() -> T + Send + 'static) -> mlua::Result<T> {
    mare_binding::blocking::spawn(lua, work)?.await.map_err(rt_err)
}

/// Run `f` on the blocking pool inside [`mare_plugin_manager::with_cancel`], streaming its log lines to
/// `on_line` while it runs; returns (result, all lines). Tracked by the host's blocking scope so
/// `Host::shutdown()` waits for cancellation cleanup (including git process-tree termination) to
/// complete before releasing host directories.
async fn run_logged<F>(
    lua: &Lua,
    token: CancellationToken,
    on_line: Option<Function>,
    f: F,
) -> mlua::Result<(anyhow::Result<()>, Vec<String>)>
where
    F: FnOnce(&mut dyn FnMut(String)) -> anyhow::Result<()> + Send + 'static,
{
    let guard = token.clone().drop_guard();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let mut job = mare_binding::blocking::spawn(lua, move || {
        mare_plugin_manager::with_cancel(&token, || {
            f(&mut |l| {
                let _ = tx.send(l);
            })
        })
    })?;
    let mut log = vec![];
    let r = loop {
        tokio::select! {
            Some(l) = rx.recv() => {
                let l = mare_shared::redact::redact_str(&l);
                if let Some(cb) = &on_line {
                    let _ = cb.call_async::<()>(l.clone()).await;
                }
                log.push(l);
            }
            r = &mut job => break r.map_err(rt_err)?,
        }
    };
    while let Ok(l) = rx.try_recv() {
        let l = mare_shared::redact::redact_str(&l);
        if let Some(cb) = &on_line {
            let _ = cb.call_async::<()>(l.clone()).await;
        }
        log.push(l);
    }
    guard.disarm();
    Ok((r, log))
}

/// Plugin names defined more than once (doctor): a later source overriding an earlier one from
/// a lower-precedence place is intended (listed); two packages (or two files in one place)
/// defining the same name is a clash.
fn plugin_name_check(cfg: &J, cwd: &std::path::Path, trusted: bool) -> (bool, String) {
    use std::collections::BTreeMap;
    let mut by: BTreeMap<String, Vec<mare_kernel::PluginSource>> = BTreeMap::new();
    for s in crate::loader::discover(cfg, cwd, trusted, false) {
        by.entry(s.name_hint.clone()).or_default().push(s);
    }
    let (mut clashes, mut overrides) = (vec![], vec![]);
    for (name, list) in by.iter().filter(|(_, l)| l.len() > 1) {
        let where_ = |s: &mare_kernel::PluginSource| {
            s.path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| s.origin.as_str().to_string())
        };
        let same_origin = list.windows(2).any(|w| w[0].origin == w[1].origin);
        let places: Vec<String> = list.iter().map(where_).collect();
        if same_origin {
            clashes.push(format!("{name}: {}", places.join(" and ")));
        } else {
            overrides.push(format!(
                "{name} ({} overrides {})",
                list.last().unwrap().origin.as_str(),
                list[0].origin.as_str()
            ));
        }
    }
    if !clashes.is_empty() {
        return (false, format!("defined twice in one place (the later one wins): {}", clashes.join("; ")));
    }
    if overrides.is_empty() {
        (true, "no duplicate names".into())
    } else {
        (true, format!("overrides: {}", overrides.join(", ")))
    }
}

/// `mare plugin …`: run a package operation on the blocking pool, printing its log; Ctrl-C cancels
/// it (git runs in its own process group, so the terminal's SIGINT reaches only mare — which
/// then kills git and its children instead of leaving them behind).
pub async fn run_cli<F>(f: F) -> anyhow::Result<()>
where
    F: FnOnce(&mut dyn FnMut(String)) -> anyhow::Result<()> + Send + 'static,
{
    let token = CancellationToken::new();
    let t2 = token.clone();
    let ctrl_c = tokio::task::spawn_local(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("mare: cancelling …");
            t2.cancel();
        }
    });
    let r = tokio::task::spawn_blocking(move || {
        mare_plugin_manager::with_cancel(&token, || f(&mut |l| println!("{}", mare_shared::redact::redact_str(&l))))
    })
    .await;
    ctrl_c.abort();
    r?.map_err(|e| anyhow::anyhow!("{}", mare_shared::redact::redact_str(&format!("{e:#}"))))
}

async fn run_pkg_action(lua: &Lua, action: String, name: Option<String>, opts: Option<Table>) -> mlua::Result<Value> {
    let cfg = current_config(lua)?;
    let token = op_token(&opts);
    let check = token.clone();
    let on_line = opts.as_ref().and_then(|o| o.get::<Option<Function>>("on_line").ok().flatten());
    let stash = opts.as_ref().and_then(|o| o.get::<Option<bool>>("stash").ok().flatten()).unwrap_or(false);
    let force = opts.as_ref().and_then(|o| o.get::<Option<bool>>("force").ok().flatten()).unwrap_or(false);
    let dry_run = opts.as_ref().and_then(|o| o.get::<Option<bool>>("dry_run").ok().flatten()).unwrap_or(false);
    let (r, log) = run_logged(lua, token, on_line, move |sink| {
        let need = |n: &Option<String>| n.clone().ok_or_else(|| anyhow::anyhow!("{action} needs a package name"));
        match action.as_str() {
            "sync" => mare_plugin_manager::sync_only(&cfg, name.as_deref(), sink),
            "install" | "uninstall" => {
                let mut source = match &name {
                    Some(name) => mare_plugin_manager::plugin_source(&cfg, name)?,
                    None => None,
                };
                if action == "install"
                    && source.is_none()
                    && name.as_ref().is_some_and(|name| !name.contains('/'))
                    && !mare_plugin_manager::specs(&cfg).iter().any(|spec| Some(&spec.name) == name.as_ref())
                    && mare_plugin_manager::catalog_for(&cfg)?.is_empty()
                {
                    mare_plugin_manager::fetch_catalog_for(&cfg, sink)?;
                    source = mare_plugin_manager::plugin_source(&cfg, name.as_deref().unwrap_or_default())?;
                }
                match (action.as_str(), name.as_ref(), source) {
                    ("install", Some(name), Some(_)) => {
                        mare_plugin_manager::install_plugins(&cfg, std::slice::from_ref(name), sink)
                    }
                    ("uninstall", Some(name), Some(_)) => {
                        mare_plugin_manager::uninstall_plugins(&cfg, std::slice::from_ref(name), sink)
                    }
                    ("install", _, _) => mare_plugin_manager::sync_only(&cfg, name.as_deref(), sink),
                    _ => mare_plugin_manager::clean_only(&cfg, name.as_deref(), force, sink),
                }
            }
            "update" => {
                let resolved = match &name {
                    Some(name) => Some(mare_plugin_manager::plugin_source(&cfg, name)?.unwrap_or_else(|| name.clone())),
                    None => None,
                };
                mare_plugin_manager::update(&cfg, resolved.as_deref(), stash, sink)
            }
            "restore" => mare_plugin_manager::restore_only(&cfg, name.as_deref(), sink),
            "clean" if dry_run => {
                mare_plugin_manager::PkgOptions::from_config(&cfg).preview_clean(&cfg, name.as_deref(), force, sink)
            }
            "clean" => mare_plugin_manager::clean_only(&cfg, name.as_deref(), force, sink),
            "log" => mare_plugin_manager::log_of(&cfg, &need(&name)?, sink),
            "diff" => mare_plugin_manager::diff_of(&cfg, &need(&name)?, sink),
            other => Err(anyhow::anyhow!(
                "unknown action {other} (sync | install | update | uninstall | restore | clean | log | diff)"
            )),
        }
    })
    .await?;
    outcome(lua, r, log, check.is_cancelled())
}

pub fn install(lua: &Lua, mare: &Table) -> mlua::Result<()> {
    let t = match mare.get::<Option<Table>>("plugins")? {
        Some(plugins) => plugins,
        None => lua.create_table()?,
    };
    t.set(
        "diagnostics",
        lua.create_async_function(|lua, ()| async move {
            let cfg = current_config(&lua)?;
            let diagnostics = run_blocking(&lua, move || mare_plugin_manager::diagnostics(&cfg)).await?;
            json_to_lua(&lua, &serde_json::to_value(diagnostics).map_err(rt_err)?)
        })?,
    )?;
    t.set(
        "check",
        lua.create_async_function(|lua, (names, opts): (Option<Vec<String>>, Option<Table>)| async move {
            let cfg = current_config(&lua)?;
            let token = op_token(&opts);
            let _guard = token.clone().drop_guard();
            let rows = run_blocking(&lua, move || {
                mare_plugin_manager::with_cancel(&token, || {
                    let mut resolved = Vec::new();
                    for name in names.unwrap_or_default() {
                        resolved.push(mare_plugin_manager::plugin_source(&cfg, &name)?.unwrap_or(name));
                    }
                    mare_plugin_manager::check(&cfg, &resolved)
                })
            })
            .await?
            .map_err(rt_err)?;
            json_to_lua(&lua, &serde_json::to_value(rows).map_err(rt_err)?)
        })?,
    )?;
    t.set(
        "list_installed",
        lua.create_async_function(|lua, opts: Option<Table>| async move {
            let cfg = current_config(&lua)?;
            let with_manifest =
                opts.as_ref().and_then(|o| o.get::<Option<bool>>("manifest").ok().flatten()).unwrap_or(false);
            let token = op_token(&opts);
            let guard = token.clone().drop_guard();
            let rows = run_blocking(&lua, move || -> anyhow::Result<Vec<J>> {
                let list = mare_plugin_manager::with_cancel(&token, || mare_plugin_manager::status(&cfg))?;
                Ok(list
                    .into_iter()
                    .map(|mut s| {
                        let manifest = if with_manifest && s.installed && !s.disabled {
                            mare_plugin_manager::manifest(std::path::Path::new(&s.dir))
                                .ok()
                                .flatten()
                                .map(manifest_info)
                        } else {
                            None
                        };
                        s.url = mare_shared::redact::redact_str(&s.url);
                        s.problem = s.problem.map(|p| mare_shared::redact::redact_str(&p));
                        let mut row = serde_json::to_value(&s).unwrap_or(J::Null);
                        if let (Some(map), Some(m)) = (row.as_object_mut(), manifest) {
                            map.insert("manifest".into(), m);
                        }
                        row
                    })
                    .collect())
            })
            .await?;
            guard.disarm();
            match rows {
                Ok(r) => json_to_lua(&lua, &J::Array(r)),
                Err(e) => Err(mlua::Error::runtime(format!("{e:#}"))),
            }
        })?,
    )?;
    t.set(
        "run",
        lua.create_async_function(|lua, (action, name, opts): (String, Option<String>, Option<Table>)| async move {
            run_pkg_action(&lua, action, name, opts).await
        })?,
    )?;
    for (method, action) in [("install", "install"), ("update", "update"), ("uninstall", "uninstall")] {
        t.set(
            method,
            lua.create_async_function(move |lua, (name, opts): (Option<String>, Option<Table>)| async move {
                run_pkg_action(&lua, action.to_string(), name, opts).await
            })?,
        )?;
    }
    t.set(
        "manifest",
        lua.create_async_function(|lua, name: String| async move {
            let cfg = current_config(&lua)?;
            let m = run_blocking(&lua, move || -> anyhow::Result<Option<mare_plugin_manager::Manifest>> {
                let Some(row) = mare_plugin_manager::status(&cfg)?.into_iter().find(|s| s.name == name) else {
                    return Ok(None);
                };
                if !row.installed || row.disabled {
                    return Ok(None);
                }
                mare_plugin_manager::manifest(std::path::Path::new(&row.dir))
            })
            .await?
            .map_err(rt_err)?;
            match m {
                Some(manifest) => json_to_lua(&lua, &serde_json::to_value(manifest).map_err(rt_err)?),
                None => Ok(Value::Nil),
            }
        })?,
    )?;
    t.set(
        "catalog",
        lua.create_async_function(|lua, ()| async move {
            let cwd: String = lua.globals().get::<Table>("mare")?.get("cwd")?;
            let cfg = current_config(&lua)?;
            let r =
                run_blocking(&lua, move || mare_plugin_manager::extras_for(std::path::Path::new(&cwd), &cfg)).await?;
            match r {
                Ok(list) => json_to_lua(&lua, &serde_json::to_value(list).map_err(rt_err)?),
                Err(e) => Err(mlua::Error::runtime(format!("{e:#}"))),
            }
        })?,
    )?;
    t.set(
        "set_enabled",
        lua.create_async_function(|lua, (name, on, source): (String, bool, Option<String>)| async move {
            let cfg = current_config(&lua)?;
            let r =
                run_blocking(&lua, move || mare_plugin_manager::set_extra(&name, on, source.as_deref(), &cfg)).await?;
            let v = match r {
                Ok(()) => vec![Value::Boolean(true)],
                Err(e) => vec![Value::Nil, Value::String(lua.create_string(format!("{e:#}"))?)],
            };
            Ok(mlua::MultiValue::from_vec(v))
        })?,
    )?;
    t.set(
        "fetch_catalog",
        lua.create_async_function(|lua, opts: Option<Table>| async move {
            let cfg = current_config(&lua)?;
            let token = op_token(&opts);
            let check = token.clone();
            let on_line = opts.as_ref().and_then(|o| o.get::<Option<Function>>("on_line").ok().flatten());
            let (r, log) =
                run_logged(&lua, token, on_line, move |sink| mare_plugin_manager::fetch_catalog_for(&cfg, sink))
                    .await?;
            outcome(&lua, r, log, check.is_cancelled())
        })?,
    )?;
    t.set(
        "doctor",
        lua.create_async_function(|lua, ()| async move {
            let mare: Table = lua.globals().get("mare")?;
            let cwd: String = mare.get("cwd")?;
            let trusted: bool = mare.get::<Option<bool>>("trusted")?.unwrap_or(false);
            let cfg = current_config(&lua)?;
            let checks = run_blocking(&lua, move || {
                let mut v = mare_plugin_manager::doctor_for(&cfg);
                let (ok, detail) = plugin_name_check(&cfg, std::path::Path::new(&cwd), trusted);
                v.push(("plugin names".into(), ok, detail));
                v
            })
            .await?;
            let out = lua.create_table()?;
            for (name, ok, detail) in checks {
                let c = lua.create_table()?;
                c.set("name", name)?;
                c.set("ok", ok)?;
                c.set("detail", detail)?;
                out.push(c)?;
            }
            Ok(out)
        })?,
    )?;
    t.set(
        "watched",
        lua.create_function(|lua, ()| {
            let mut v = vec![
                mare_config::user_init().to_string_lossy().into_owned(),
                mare_plugin_manager::extras_path(&serde_json::json!({})).to_string_lossy().into_owned(),
            ];
            let mare: Table = lua.globals().get("mare")?;
            let scoped = mare_plugin_manager::extras_path(&current_config(lua)?).to_string_lossy().into_owned();
            if !v.contains(&scoped) {
                v.push(scoped);
            }
            if mare.get::<Option<bool>>("trusted")?.unwrap_or(false) {
                let cwd: String = mare.get("cwd")?;
                if let Some(mare_dir) = mare_shared::project::resource_root(std::path::Path::new(&cwd)).mare_dir() {
                    v.push(mare_dir.join("init.lua").to_string_lossy().into_owned());
                }
            }
            // only the `lua/` modules the config actually required, never the whole directory
            if let Some(m) = lua.app_data_ref::<crate::config_load::ConfigModules>() {
                v.extend(m.0.iter().map(|p| p.to_string_lossy().into_owned()));
            }
            Ok(v)
        })?,
    )?;
    t.set(
        "dirs",
        lua.create_function(|lua, kind: Option<String>| {
            let out = lua.create_table()?;
            // --no-user-plugins loads no package at all: none of their content either
            let mare: Table = lua.globals().get("mare")?;
            if mare.get::<Option<bool>>("no_user_plugins")?.unwrap_or(false) {
                if let Some(k) = kind.as_deref()
                    && !mare_plugin_manager::CONTENT_KINDS.contains(&k)
                {
                    return Err(mlua::Error::runtime(format!(
                        "mare.plugins.dirs: unknown kind {k} ({})",
                        mare_plugin_manager::CONTENT_KINDS.join(" | ")
                    )));
                }
                return Ok(out);
            }
            let cfg = current_config(lua)?;
            let dirs = mare_plugin_manager::resource_dirs(&cfg, kind.as_deref())
                .map_err(|e| mlua::Error::runtime(format!("mare.plugins.dirs: {e}")))?;
            for (name, dir) in dirs {
                let row = lua.create_table()?;
                row.set("name", name)?;
                row.set("dir", dir.to_string_lossy().replace('\\', "/"))?;
                out.push(row)?;
            }
            Ok(out)
        })?,
    )?;
    t.set("default_source", mare_plugin_manager::DEFAULT_SOURCE)?;
    // mare's own NEWS.md (the copy in the runtime directory is the same text)
    t.set("mare_news", lua.create_function(|_, ()| Ok(crate::news_text().into_owned()))?)?;
    // Generic read-only resource reader for installed packages (`<pkg>/resources/<path>`) and
    // built-in preset resources (`preset/resources/<path>`), transparently gunzipping `.gz` files.
    t.set(
        "read_resource",
        lua.create_function(|lua, (path, opts): (String, Option<Table>)| {
            let norm = crate::validate_resource_path(&path)
                .map_err(|e| mlua::Error::runtime(format!("mare.plugins.read_resource: {e}")))?;
            let target_pkg: Option<String> = match opts {
                Some(ref o) => o.get("package")?,
                None => None,
            };
            let mare: Table = lua.globals().get("mare")?;
            let no_user_plugins = mare.get::<Option<bool>>("no_user_plugins")?.unwrap_or(false);
            if !no_user_plugins {
                let cfg = current_config(lua)?;
                let dirs = mare_plugin_manager::resource_dirs(&cfg, None)
                    .map_err(|e| mlua::Error::runtime(format!("mare.plugins.read_resource: {e}")))?;
                for (name, dir) in dirs {
                    if let Some(ref want) = target_pkg
                        && &name != want
                    {
                        continue;
                    }
                    let res_root = dir.join("resources");
                    let candidate = res_root.join(&norm);
                    if candidate.is_file() {
                        let canon_root = mare_shared::paths::resolve_path(&res_root)
                            .map_err(|e| mlua::Error::runtime(format!("mare.plugins.read_resource: {e}")))?
                            .physical;
                        let canon_file = mare_shared::paths::resolve_path(&candidate)
                            .map_err(|e| mlua::Error::runtime(format!("mare.plugins.read_resource: {e}")))?
                            .physical;
                        if !canon_file.starts_with(&canon_root) {
                            return Err(mlua::Error::runtime(format!(
                                "mare.plugins.read_resource: path escapes package resources: {path:?}"
                            )));
                        }
                        let bytes = std::fs::read(&canon_file)
                            .map_err(|e| mlua::Error::runtime(format!("mare.plugins.read_resource: {e}")))?;
                        let text = crate::decode_resource_bytes(&norm, &bytes)
                            .map_err(|e| mlua::Error::runtime(format!("mare.plugins.read_resource: {e}")))?;
                        return Ok(Some(text));
                    }
                }
            }
            if target_pkg.is_some() {
                return Ok(None);
            }
            crate::read_builtin_resource(&norm)
                .map_err(|e| mlua::Error::runtime(format!("mare.plugins.read_resource: {e}")))
        })?,
    )?;
    t.set("news_version", crate::NEWS_VERSION)?;
    mare.set("plugins", t)
}

/// Raw-read the `mare.plugins` table without invoking metamethods (used by config rollback).
pub fn plugins_table(mare: &Table) -> mlua::Result<Option<Table>> {
    mare.raw_get("plugins")
}
