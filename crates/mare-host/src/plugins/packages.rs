//! Packages before plugins load and their `init` / `config` hooks.

use mare_kernel::Kernel;

/// Packages before plugins load: plugins.json renames, missing packages
/// installed (their progress lines go to `progress`, for the activity area; finished installs
/// only to the log), the packages' `types/` mirrored for LuaLS; return problems to the caller.
/// git runs on the blocking pool and stops when `cancel` is cancelled (shutdown) or this future
/// is dropped.
pub(crate) async fn install_packages(
    lua: &mlua::Lua,
    config: &serde_json::Value,
    cancel: &tokio_util::sync::CancellationToken,
    progress: Option<tokio::sync::mpsc::UnboundedSender<String>>,
) -> Vec<String> {
    let c2 = config.clone();
    let token = cancel.child_token();
    let _guard = token.clone().drop_guard();
    let job = match mare_binding::blocking::spawn(lua, move || {
        mare_plugin_manager::with_cancel(&token, || {
            let mut n = mare_plugin_manager::migrate_extras_for(&c2);
            let pending = mare_plugin_manager::apply_pending_updates_with_progress(&c2, &mut |line| {
                tracing::info!("pending plugin update: {line}");
            });
            n.extend(pending);
            let install = mare_plugin_manager::install_missing_with_progress(
                &c2,
                &mut |name| tracing::info!("installed package {name}"),
                &mut |line| {
                    if let Some(p) = &progress {
                        let _ = p.send(line);
                    }
                },
            );
            drop(progress);
            if let Err(e) = mare_plugin_manager::refresh_types(&c2) {
                tracing::warn!("package types not mirrored: {e:#}");
            }
            let problems = mare_plugin_manager::problems(&c2);
            n.extend(install);
            n.extend(problems);
            n
        })
    }) {
        Ok(job) => job,
        Err(error) => return vec![format!("package installation task failed: {error}")],
    };
    job.await.unwrap_or_else(|error| vec![format!("package installation task failed: {error}")])
}

/// Run packages' `init(pkg)` (before their plugins load) or `config(pkg, opts)` (after) —
/// lazy.nvim `init` / `config` — for packages that are installed and loadable only (one that
/// failed to install or is blocked by its manifest runs neither). `config = true` means "the
/// default": the package's opts already reach its plugins. Returns the failures.
pub(crate) async fn package_hooks(kernel: &Kernel, which: &str, config: &serde_json::Value) -> Vec<String> {
    let lua = kernel.lua();
    let has_hook: bool = lua
        .load(
            r#"
local which = ...
for _, p in ipairs((mare.config and mare.config.packages) or {}) do
  if p.enabled ~= false and type(p[which]) == "function" then
    return true
  end
end
return false
"#,
        )
        .set_name("@core/package-hooks-check")
        .call(which)
        .unwrap_or(true);
    if !has_hook {
        return vec![];
    }
    let loaded: Vec<(String, String)> = {
        let c = config.clone();
        match mare_binding::blocking::spawn(lua, move || {
            mare_plugin_manager::installed(&c)
                .into_iter()
                .map(|p| (p.name, p.dir.to_string_lossy().into_owned()))
                .collect()
        }) {
            Ok(job) => job.await.unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    };
    if loaded.is_empty() {
        return vec![];
    }
    let run = || -> mlua::Result<mlua::Function> {
        lua.load(
            r#"
local which, loaded = ...
local problems = {}
for _, p in ipairs((mare.config and mare.config.packages) or {}) do
  local f = p[which]
  local dir = loaded[p.name]
  if type(f) == "function" and p.enabled ~= false and dir then
    local info = { name = p.name, display = p.display, dir = dir, subdir = p.subdir }
    local ok, err
    if which == "init" then ok, err = pcall(f, info) else ok, err = pcall(f, info, p.opts or {}) end
    if not ok then
      problems[#problems + 1] = ("package %s: %s failed: %s"):format(tostring(p.name), which, tostring(err))
    end
  end
end
return problems
"#,
        )
        .set_name("@core/package-hooks")
        .into_function()
    };
    let Ok(f) = run() else { return vec![] };
    let Ok(map) = lua.create_table() else { return vec![] };
    for (n, d) in loaded {
        let _ = map.set(n, d);
    }
    f.call_async::<Vec<String>>((which.to_string(), map))
        .await
        .unwrap_or_else(|e| vec![format!("package {which}: {e}")])
}
