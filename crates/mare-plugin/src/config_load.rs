//! Run init.lua in a short-lived VM (for `mare plugin` / `mare doctor`) and install the config
//! functions plugins declare in `config_api`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context, Result};
use mare_config::Config;
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy)]
struct BuiltinConfigMeta {
    name_hint: &'static str,
    name: &'static str,
    overrides: bool,
    tags: &'static [&'static str],
    has_config_api: bool,
    has_config_schema: bool,
    has_env_bindings: bool,
}

const BUILTIN_CONFIG_META: &[BuiltinConfigMeta] = include!(concat!(env!("OUT_DIR"), "/builtin_config_meta.rs"));

fn lookup_builtin_meta(name_hint: &str) -> Option<&'static BuiltinConfigMeta> {
    BUILTIN_CONFIG_META.iter().find(|m| m.name_hint == name_hint)
}

#[derive(Clone)]
struct LazyBuiltinEntry {
    source: mare_kernel::PluginSource,
    has_schema: bool,
}

#[derive(Default)]
struct LazyRegistryInner {
    schemas_defaults: HashMap<String, LazyBuiltinEntry>,
    slots: HashMap<String, Vec<String>>,
}

#[derive(Clone)]
struct LazyRegistryHandle(Rc<RefCell<LazyRegistryInner>>);

impl mlua::UserData for LazyRegistryHandle {}

fn materialize_lazy_entry(
    lua: &mlua::Lua,
    schemas: &mlua::Table,
    defaults: &mlua::Table,
    config_view: Option<&mlua::Table>,
    name: &str,
    entry: &LazyBuiltinEntry,
) -> mlua::Result<()> {
    if !entry.has_schema {
        defaults.raw_set(name, lua.create_table()?)?;
        return Ok(());
    }
    let env = mare_shared::lua::isolated_env(lua)?;
    if let Some(view) = config_view {
        env.set("mare", view.clone())?;
    }
    let Ok(chunk) = entry.source.load_chunk(lua, env) else {
        defaults.raw_set(name, lua.create_table()?)?;
        return Ok(());
    };
    let Ok(def) = chunk.call::<mlua::Table>(()) else {
        defaults.raw_set(name, lua.create_table()?)?;
        return Ok(());
    };
    let schema = match def.get::<mlua::Value>("config_schema") {
        Ok(mlua::Value::Table(t)) => Some(t),
        _ => None,
    };
    let opts = match &schema {
        Some(s) => mare_shared::config_schema::defaults(lua, s)?,
        None => lua.create_table()?,
    };
    schemas.raw_set(name, schema)?;
    defaults.raw_set(name, opts)?;
    Ok(())
}

/// Read the Lua-side sibling tables (`schemas`, `defaults`, optional `view`) stored directly on a
/// lazy table's metatable so Rust closures never capture `mlua::Table` handles (avoiding
/// Rust↔Lua registry reference cycles across `/reload config`).
fn lazy_mt_tables(tbl: &mlua::Table) -> mlua::Result<(mlua::Table, mlua::Table, Option<mlua::Table>)> {
    let mt = tbl.metatable().ok_or_else(|| mlua::Error::runtime("lazy config table missing metatable"))?;
    let schemas: mlua::Table = mt.raw_get("_schemas")?;
    let defaults: mlua::Table = mt.raw_get("_defaults")?;
    let view: Option<mlua::Table> = mt.raw_get("_view")?;
    Ok((schemas, defaults, view))
}

fn ensure_lazy_registry(
    lua: &mlua::Lua,
    schemas: &mlua::Table,
    defaults: &mlua::Table,
    slots: &mlua::Table,
    config_view: Option<&mlua::Table>,
) -> mlua::Result<Rc<RefCell<LazyRegistryInner>>> {
    let st = mare_shared::lua::core::config::state(lua)?;
    if let Ok(mlua::Value::UserData(ud)) = st.raw_get::<mlua::Value>("lazy_builtins")
        && let Ok(handle) = ud.borrow::<LazyRegistryHandle>()
    {
        if let Some(mt) = schemas.metatable() {
            mt.raw_set("_view", config_view.cloned())?;
        }
        if let Some(mt) = defaults.metatable() {
            mt.raw_set("_view", config_view.cloned())?;
        }
        return Ok(handle.0.clone());
    }
    let reg = Rc::new(RefCell::new(LazyRegistryInner::default()));
    st.raw_set("lazy_builtins", lua.create_userdata(LazyRegistryHandle(reg.clone()))?)?;

    // Shared `__index` / `__pairs` metamethod factories for `plugin_schemas` and `plugin_defaults`.
    // Closures capture only pure-Rust `reg`; Lua table references live solely on the metatable so
    // Lua's GC collects everything when `core.config.state` is replaced on `/reload config`.
    for target in [schemas, defaults] {
        let reg_idx = reg.clone();
        let reg_pairs = reg.clone();
        let mt = lua.create_table()?;
        mt.raw_set("_schemas", schemas.clone())?;
        mt.raw_set("_defaults", defaults.clone())?;
        mt.raw_set("_view", config_view.cloned())?;
        mt.raw_set(
            "__index",
            lua.create_function(move |lua, (tbl, key): (mlua::Table, mlua::Value)| {
                let mlua::Value::String(k) = key else { return Ok(mlua::Value::Nil) };
                let Ok(name) = k.to_str() else { return Ok(mlua::Value::Nil) };
                let entry = reg_idx.borrow_mut().schemas_defaults.remove(name.as_ref());
                let Some(entry) = entry else { return Ok(mlua::Value::Nil) };
                let (schemas_tbl, defaults_tbl, view) = lazy_mt_tables(&tbl)?;
                materialize_lazy_entry(lua, &schemas_tbl, &defaults_tbl, view.as_ref(), name.as_ref(), &entry)?;
                tbl.raw_get(name.as_ref())
            })?,
        )?;
        mt.raw_set(
            "__pairs",
            lua.create_function(move |lua, tbl: mlua::Table| {
                let pending: Vec<(String, LazyBuiltinEntry)> =
                    reg_pairs.borrow_mut().schemas_defaults.drain().collect();
                if !pending.is_empty() {
                    let (schemas_tbl, defaults_tbl, view) = lazy_mt_tables(&tbl)?;
                    for (name, entry) in pending {
                        materialize_lazy_entry(lua, &schemas_tbl, &defaults_tbl, view.as_ref(), &name, &entry)?;
                    }
                }
                let next_fn: mlua::Function = lua.globals().get("next")?;
                Ok((next_fn, tbl, mlua::Value::Nil))
            })?,
        )?;
        target.set_metatable(Some(mt))?;
    }

    // Metatable for `plugin_slots` (pure-Rust `reg` only).
    {
        let reg_idx = reg.clone();
        let reg_pairs = reg.clone();
        let mt = lua.create_table()?;
        mt.raw_set(
            "__index",
            lua.create_function(move |lua, (tbl, key): (mlua::Table, mlua::Value)| {
                let mlua::Value::String(k) = key else { return Ok(mlua::Value::Nil) };
                let Ok(name) = k.to_str() else { return Ok(mlua::Value::Nil) };
                let tags = reg_idx.borrow_mut().slots.remove(name.as_ref());
                let Some(tags) = tags else { return Ok(mlua::Value::Nil) };
                let entry = slot_entry(lua, "builtin", &tags)?;
                tbl.raw_set(name.as_ref(), entry.clone())?;
                Ok(mlua::Value::Table(entry))
            })?,
        )?;
        mt.raw_set(
            "__pairs",
            lua.create_function(move |lua, tbl: mlua::Table| {
                let pending: Vec<(String, Vec<String>)> = reg_pairs.borrow_mut().slots.drain().collect();
                for (name, tags) in pending {
                    tbl.raw_set(name, slot_entry(lua, "builtin", &tags)?)?;
                }
                let next_fn: mlua::Function = lua.globals().get("next")?;
                Ok((next_fn, tbl, mlua::Value::Nil))
            })?,
        )?;
        slots.set_metatable(Some(mt))?;
    }

    Ok(reg)
}

/// Evaluate plugin definitions (builtin, user, trusted project) and install the config functions
/// they declare in `config_api`, so init.lua can call e.g. `mare.tool.command{...}`.
pub fn install_plugin_config_apis(
    lua: &mlua::Lua,
    mare: &mlua::Table,
    c: &mlua::Table,
    cwd: &Path,
    project_trusted: bool,
    config_view: Option<&mlua::Table>,
) -> Result<()> {
    let no_user = mare.get::<bool>("no_user_plugins").unwrap_or(false);
    let t_discover = std::time::Instant::now();
    let mut scope_cfg = Value::Object(Map::new());
    mare_plugin_manager::set_project_scope(&mut scope_cfg, cwd, project_trusted);
    let sources = crate::loader::discover(&scope_cfg, cwd, project_trusted, no_user);
    mare_shared::profile::record("config: discover plugin APIs", t_discover.elapsed());
    let t_definitions = std::time::Instant::now();
    let apis = declare_plugins(lua, sources, config_view)?;
    mare_shared::profile::record("config: evaluate plugin APIs", t_definitions.elapsed());
    crate::engines::config::load_plugin_config_api(lua, mare, c, apis)?;
    Ok(())
}

fn slot_entry(lua: &mlua::Lua, origin: &str, tags: &[String]) -> mlua::Result<mlua::Table> {
    let entry = lua.create_table()?;
    entry.set("builtin", origin == "builtin")?;
    entry.set("origin", origin)?;
    let list = lua.create_table()?;
    for t in tags {
        list.push(t.as_str())?;
    }
    entry.set("tags", list)?;
    Ok(entry)
}

/// Build/refresh plugin slot metadata from effective slot definitions.
/// Every discovered slot gets `{ builtin, origin, tags }` so introspection registers
/// `plugins.<name>.enabled` and `plugins.preset = "minimal"` expansion.
pub fn sync_plugin_slots(lua: &mlua::Lua, declared: &[mare_kernel::Declared]) -> mlua::Result<()> {
    let slots = lua.create_table()?;
    for d in declared.iter().filter(|d| d.effective) {
        slots.set(d.name.as_str(), slot_entry(lua, &d.origin, &d.tags)?)?;
    }
    mare_shared::lua::core::config::set_plugin_slots(lua, mlua::Value::Table(slots))
}

/// Evaluate plugin definitions and record what config_api.lua checks a plugin's values against:
/// their declared defaults and `config_schema`, plus slot metadata per plugin.
/// Returns the `config_api` tables they declare.
fn declare_plugins(
    lua: &mlua::Lua,
    sources: Vec<mare_kernel::PluginSource>,
    config_view: Option<&mlua::Table>,
) -> Result<mlua::Table> {
    let apis = lua.create_table()?;
    // plugin default opts, for typo / type warnings in mare.plugin(name, opts)
    let defaults = mare_shared::lua::core::config::ensure_plugin_defaults(lua)?;
    // declarations (`config_schema`): what config_api.lua checks a plugin's values against
    let schemas = mare_shared::lua::core::config::ensure_plugin_schemas(lua)?;
    // slot metadata (`{ builtin, origin, tags }`) for `plugins.<name>.enabled` & `plugins.preset`
    let slots = mare_shared::lua::core::config::ensure_plugin_slots(lua)?;

    enum ParsedPayload {
        Eager { schema: Option<mlua::Table>, opts: mlua::Table, api: Option<mlua::Table> },
        Lazy { source: mare_kernel::PluginSource, has_schema: bool },
    }

    struct Parsed {
        origin: mare_kernel::Origin,
        name: String,
        overrides: bool,
        tags: Vec<String>,
        payload: ParsedPayload,
    }

    // Both debug and release builds use the exact same `BUILTIN_CONFIG_META` lazy-loading path
    // whenever built-ins are not overridden via `MARE_DEV_PLUGINS` (the only difference between
    // debug and release is whether `src` loads source text from `preset/plugins/` or embedded
    // bytecode). Built-ins declaring `config_api` or `env` bindings stay eager so
    // `plugin_env_bindings` keeps a single source of truth (`plugin_schemas`).
    let dev_override = std::env::var_os("MARE_DEV_PLUGINS").is_some();
    let mut parsed = Vec::new();
    for src in sources {
        if src.origin == mare_kernel::Origin::Builtin
            && !dev_override
            && let Some(meta) = lookup_builtin_meta(&src.name_hint)
            && !meta.has_config_api
            && !meta.has_env_bindings
        {
            if meta.name == "preset" {
                continue;
            }
            parsed.push(Parsed {
                origin: src.origin,
                name: meta.name.to_string(),
                overrides: meta.overrides,
                tags: meta.tags.iter().map(|s| (*s).to_string()).collect(),
                payload: ParsedPayload::Lazy { source: src, has_schema: meta.has_config_schema },
            });
            continue;
        }
        let env = mare_shared::lua::isolated_env(lua)?;
        if let Some(view) = config_view {
            env.set("mare", view.clone())?;
        }
        let Ok(chunk) = src.load_chunk(lua, env) else { continue };
        let Ok(def) = chunk.call::<mlua::Table>(()) else { continue };
        let name: String = def.get::<Option<String>>("name").ok().flatten().unwrap_or_else(|| src.name_hint.clone());
        // `preset` is a reserved field of the `plugins` table, never a plugin slot
        if name == "preset" {
            continue;
        }
        let overrides = def.get::<Option<bool>>("override").ok().flatten().unwrap_or(false);
        let tags: Vec<String> = match def.get::<mlua::Value>("tags") {
            Ok(mlua::Value::Table(t)) => t.sequence_values::<String>().filter_map(|v| v.ok()).collect(),
            _ => vec![],
        };
        let schema = match def.get::<mlua::Value>("config_schema") {
            Ok(mlua::Value::Table(t)) => Some(t),
            _ => None,
        };
        let opts = match &schema {
            Some(schema) => mare_shared::config_schema::defaults(lua, schema)?,
            None => lua.create_table()?,
        };
        let api = match def.get::<mlua::Value>("config_api") {
            Ok(mlua::Value::Table(api)) => Some(api),
            _ => None,
        };
        parsed.push(Parsed {
            origin: src.origin,
            name,
            overrides,
            tags,
            payload: ParsedPayload::Eager { schema, opts, api },
        });
    }

    // Same-name definitions from one source are twins and neither participates.
    let mut counts: HashMap<(&'static str, String), usize> = HashMap::new();
    for p in &parsed {
        *counts.entry((p.origin.as_str(), p.name.clone())).or_insert(0) += 1;
    }
    let mut picks: Vec<Parsed> = Vec::new();
    for p in parsed {
        if counts.get(&(p.origin.as_str(), p.name.clone())).copied().unwrap_or(0) > 1 {
            continue;
        }
        match picks.iter_mut().find(|cur| cur.name == p.name) {
            Some(cur) if p.origin > cur.origin && p.overrides => *cur = p,
            Some(_) => {}
            None => picks.push(p),
        }
    }

    let has_any_lazy = picks.iter().any(|p| matches!(p.payload, ParsedPayload::Lazy { .. }));
    let existing_lazy = mare_shared::lua::core::config::state(lua)?
        .raw_get::<mlua::Value>("lazy_builtins")
        .ok()
        .and_then(|v| match v {
            mlua::Value::UserData(ud) => ud.borrow::<LazyRegistryHandle>().ok().map(|h| h.0.clone()),
            _ => None,
        });
    let check_existing_slots = existing_lazy.is_some() || slots.pairs::<mlua::Value, mlua::Value>().next().is_some();
    let lazy_reg = if has_any_lazy {
        Some(ensure_lazy_registry(lua, &schemas, &defaults, &slots, config_view)?)
    } else {
        existing_lazy
    };

    for p in picks {
        // When `merge_package_opts` passes package sources after initial discovery, do not let
        // a lower-origin or non-overriding source replace an existing effective slot.
        if check_existing_slots && let Ok(mlua::Value::Table(existing)) = slots.get::<mlua::Value>(p.name.as_str()) {
            let cur_origin: String = existing.get("origin").unwrap_or_default();
            if let Some(cur) = mare_kernel::Origin::parse(&cur_origin)
                && !(p.origin > cur && p.overrides)
            {
                continue;
            }
        }
        match p.payload {
            ParsedPayload::Eager { schema, opts, api } => {
                if let Some(ref reg) = lazy_reg {
                    let mut guard = reg.borrow_mut();
                    guard.schemas_defaults.remove(&p.name);
                    guard.slots.remove(&p.name);
                }
                schemas.raw_set(p.name.clone(), schema)?;
                defaults.raw_set(p.name.clone(), opts)?;
                slots.raw_set(p.name, slot_entry(lua, p.origin.as_str(), &p.tags)?)?;
                if let Some(api) = api {
                    apis.push(api)?;
                }
            }
            ParsedPayload::Lazy { source, has_schema } => {
                if let Some(ref reg) = lazy_reg {
                    let mut guard = reg.borrow_mut();
                    guard.schemas_defaults.insert(p.name.clone(), LazyBuiltinEntry { source, has_schema });
                    guard.slots.insert(p.name.clone(), p.tags);
                }
            }
        }
    }

    // Ensure `next(state.plugin_defaults) ~= nil` holds even when every discovered plugin was lazy
    // (config_api_validate.lua relies on `next(state.plugin_defaults) ~= nil` to distinguish
    // "plugins have been declared" from an uninitialized config VM).
    if let Some(ref reg) = lazy_reg
        && defaults.pairs::<mlua::Value, mlua::Value>().next().is_none()
    {
        let first = {
            let mut guard = reg.borrow_mut();
            guard
                .schemas_defaults
                .keys()
                .next()
                .cloned()
                .and_then(|k| guard.schemas_defaults.remove(&k).map(|v| (k, v)))
        };
        if let Some((name, entry)) = first {
            materialize_lazy_entry(lua, &schemas, &defaults, config_view, &name, &entry)?;
        }
    }
    Ok(apis)
}

/// Merge the package declarations the config files made into
/// `config.packages`: once per evaluation, after every config file ran. Their `opts` follow through
/// [`merge_package_opts`] once the whole configuration (state, environment, command line) is known.
pub fn finalize_packages(lua: &mlua::Lua) -> Result<()> {
    if let Some(f) = mare_shared::lua::core::config::opt_finalize_packages_fn(lua) {
        f.call::<()>(())?;
    }
    Ok(())
}

/// Put the enabled packages' `opts` under `plugins` in `config`, the final configuration (config
/// files, state, environment and command line merged), as its lowest layer: a value any of those
/// set wins. The packages are located with this same configuration (`plugins.url_format`, …), as the
/// runtime locates them, so the plugins they ship declare their defaults and `config_schema` first
/// and each package's opts are checked against them like every other entry point.
pub fn merge_package_opts(lua: &mlua::Lua, config: &mut Value, config_view: Option<&mlua::Table>) -> Result<()> {
    let sources = crate::loader::package_sources(config);
    if !sources.is_empty() {
        declare_plugins(lua, sources, config_view)?;
    }
    if let Some(check) = mare_shared::lua::core::config::opt_check_config_fn(lua) {
        let arg = match config.get("plugins").filter(|v| v.is_object()) {
            Some(v) => mare_shared::lua::config_to_lua(lua, v)?,
            None => mlua::Value::Nil,
        };
        if let mlua::Value::Table(checked) = check.call::<mlua::Value>((arg, mlua::Value::Nil, true))?
            && let Some(root) = config.as_object_mut()
        {
            let json = mare_shared::lua::config_to_json(lua, mlua::Value::Table(checked))?;
            root.insert("plugins".to_string(), json);
        }
    }
    let Some(merge) = mare_shared::lua::core::config::opt_merge_package_opts_fn(lua) else { return Ok(()) };
    let layer = match merge.call::<mlua::Value>(())? {
        mlua::Value::Table(t) => mare_shared::lua::config_to_json(lua, mlua::Value::Table(t))?,
        _ => return Ok(()),
    };
    let Value::Object(layer) = layer else { return Ok(()) };
    let Some(root) = config.as_object_mut() else { return Ok(()) };
    let plugins = root.entry("plugins").or_insert_with(|| Value::Object(Map::new()));
    let Some(plugins) = plugins.as_object_mut() else { return Ok(()) };
    for (name, mut base) in layer {
        if let Some(own) = plugins.remove(&name) {
            mare_shared::lua::core::config::merge_plugin_opts_respecting_schema(lua, &name, &mut base, own);
        }
        plugins.insert(name, base);
    }
    Ok(())
}

/// The config modules (`<config>/lua/…`, `.mare/lua/…`) the last config evaluation in this VM
/// actually `require`d — together with the init files, the set whose change means "/reload to
/// apply" (`mare.plugins.watched()`); other files under `lua/` are not part of the config.
type Used = std::rc::Rc<std::cell::RefCell<Vec<PathBuf>>>;

#[derive(Default, Clone)]
pub struct ConfigModules(pub Vec<PathBuf>);

/// Execute config layers with private module caches. Config modules do not enter the shared
/// `package.loaded`, whose entries belong to plugins and the Lua runtime. The module files that
/// were loaded are recorded as [`ConfigModules`] in the VM's app data (even when a layer fails:
/// fixing that module is what the user does next).
///
/// `errors`: collect each file's failure (Neovim's "Error detected while processing <file>")
/// and go on with the next file, keeping what the failed one set before its error —
/// startup then runs on the built-in defaults plus that. Without it the first
/// failure is returned (a reload keeps its last good configuration).
pub async fn run_init_files(
    lua: &mlua::Lua,
    cwd: &Path,
    files: &[(PathBuf, &str)],
    config_view: Option<&mlua::Table>,
    errors: Option<&mut Vec<(PathBuf, String)>>,
) -> Result<()> {
    let used = Used::default();
    let r = run_init_layers(lua, cwd, files, config_view, &used, errors).await;
    let mut modules = used.take();
    modules.sort();
    modules.dedup();
    lua.set_app_data(ConfigModules(modules));
    r
}

async fn run_init_layers(
    lua: &mlua::Lua,
    cwd: &Path,
    files: &[(PathBuf, &str)],
    config_view: Option<&mlua::Table>,
    used: &Used,
    mut errors: Option<&mut Vec<(PathBuf, String)>>,
) -> Result<()> {
    fn find_config_module(root: &Path, name: &str) -> mlua::Result<Option<PathBuf>> {
        let mut relative = PathBuf::new();
        for segment in name.split('.') {
            if segment.is_empty() || !segment.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
                return Ok(None);
            }
            relative.push(segment);
        }
        let candidates = [root.join(&relative).with_extension("lua"), root.join(&relative).join("init.lua")];
        for path in candidates {
            match std::fs::metadata(&path) {
                Ok(metadata) if metadata.is_file() => return Ok(Some(path)),
                Ok(_) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(mlua::Error::external(format!("{}: {error}", path.display()))),
            }
        }
        Ok(None)
    }
    fn resolver(lua: &mlua::Lua, root: Option<PathBuf>) -> mlua::Result<mlua::Function> {
        lua.create_function(move |_, name: String| {
            let Some(ref root) = root else {
                return Ok(None);
            };
            Ok(find_config_module(root, &name)?.map(|p| p.to_string_lossy().into_owned()))
        })
    }
    fn loader_fn(lua: &mlua::Lua, root: Option<PathBuf>, used: Used) -> mlua::Result<mlua::Function> {
        lua.create_function(move |lua, (name, env): (String, mlua::Table)| {
            let Some(ref root) = root else {
                return Ok((None::<mlua::Function>, Some(format!("config module not found: {name}"))));
            };
            let Some(path) = find_config_module(root, &name)? else {
                return Ok((None::<mlua::Function>, Some(format!("config module not found: {name}"))));
            };
            used.borrow_mut().push(path.clone());
            let chunk_name = format!("@{}", path.display());
            match mare_kernel::load_cached_chunk(lua, Some(root), &path, &chunk_name, Some(env)) {
                Ok(chunk) => Ok((Some(chunk), None)),
                Err(err) => Ok((None, Some(err.to_string()))),
            }
        })
    }
    let user_root = mare_config::user_init().parent().unwrap_or_else(|| Path::new(".")).join("lua");
    let project_root = mare_shared::project::resource_root(cwd).mare_dir().map(|m| m.join("lua"));
    let user_modules = user_root.is_dir();
    let project_modules = project_root.as_ref().is_some_and(|r| r.is_dir());
    let (user_require, project_require): (mlua::Function, mlua::Function) = lua
        .load(
            r#"
        local user_resolve, user_load, project_resolve, project_load, config_mare = ...
        local default_require = require
        local function loader(resolve, load_mod, fallback)
          local loaded, loading = {}, {}
          local require_config
          require_config = function(name)
            local cached = loaded[name]
            if cached ~= nil and cached ~= false then return cached end
            local path = resolve(name)
            if path == nil then return fallback(name) end
            if loading[name] then error('config module import cycle: ' .. name, 2) end
            local env = setmetatable({}, { __index = _G })
            env._G, env.require = env, require_config
            if config_mare then env.mare = config_mare end
            local chunk, err = load_mod(name, env)
            if not chunk then error(err, 0) end
            loading[name] = true
            local ok, value = pcall(chunk, name, path)
            loading[name] = nil
            if not ok then error(value, 0) end
            if value == nil then value = true end
            loaded[name] = value
            return value
          end
          return require_config
        end
        local user = loader(user_resolve, user_load, default_require)
        return user, loader(project_resolve, project_load, user)
    "#,
        )
        .set_name("@core/config_require")
        .call((
            resolver(lua, Some(user_root.clone()))?,
            loader_fn(lua, Some(user_root), used.clone())?,
            resolver(lua, project_root.clone())?,
            loader_fn(lua, project_root, used.clone())?,
            config_view.cloned(),
        ))?;
    for (f, layer) in files.iter().filter(|(f, _)| f.exists()) {
        mare_shared::lua::core::config::set_layer(lua, layer)?;
        let run = async {
            let env = mare_shared::lua::isolated_env(lua)?;
            if let Some(view) = config_view {
                env.set("mare", view.clone())?;
            }
            if *layer == "project" && project_modules {
                env.set("require", project_require.clone())?;
            } else if user_modules {
                env.set("require", user_require.clone())?;
            }
            let owner_root = f.parent().unwrap_or_else(|| Path::new("."));
            let chunk =
                mare_kernel::load_cached_chunk(lua, Some(owner_root), f, &format!("@{}", f.display()), Some(env))
                    .with_context(|| format!("loading {}", f.display()))?;
            let thread = lua.create_thread(chunk)?;
            let _origin = crate::config_origin::Scope::enter(lua, &thread, layer, f)?;
            thread.into_async::<()>(())?.await?;
            anyhow::Ok(())
        };
        if let Err(e) = run.await {
            match errors.as_deref_mut() {
                Some(errors) => errors.push((f.clone(), format!("{e:#}"))),
                None => anyhow::bail!("{}: {e:#}", f.display()),
            }
        }
    }
    Ok(())
}

/// Run `init.lua` (and the trusted project's `.mare/init.lua`) and collect the settings.
pub async fn load(cwd: &Path, project_trusted: bool) -> Result<Config> {
    let mut files = vec![mare_config::user_init()];
    if project_trusted && let Some(mare_dir) = mare_shared::project::resource_root(cwd).mare_dir() {
        files.push(mare_dir.join("init.lua"));
    }
    load_files(cwd, &files).await
}

pub async fn load_files(cwd: &Path, files: &[PathBuf]) -> Result<Config> {
    let lua = crate::new_lua()?;
    let env = crate::Env {
        cwd: cwd.to_path_buf(),
        mode: crate::Mode::Child,
        ui: mare_shared::ui::UiShared::new(),
        config: Value::Object(Map::new()),
        args: Value::Null,
    };
    let mare = crate::install(&lua, &env)?;
    let trusted = files.len() > 1;
    install_plugin_config_apis(&lua, &mare, &mare.get::<mlua::Table>("config")?, cwd, trusted, None)?;
    let layers: Vec<_> =
        files.iter().enumerate().map(|(i, f)| (f.clone(), if i == 0 { "user" } else { "project" })).collect();
    mare_shared::lua::core::config::set_in_config(&lua, true)?;
    let run_res = run_init_files(&lua, cwd, &layers, None, None).await;
    mare_shared::lua::core::config::set_in_config(&lua, false)?;
    run_res?;
    finalize_packages(&lua)?;
    let c: mlua::Value = mare.get("config")?;
    let value = mare_shared::lua::lua_to_json(&lua, c)?;
    let mut cfg = Config { value: if value.is_object() { value } else { Value::Object(Map::new()) }, warnings: vec![] };
    let check = mare_shared::lua::core::config::check_config_fn(&lua)?;
    if let Some(state) = mare_config::read_state().map_err(anyhow::Error::msg)? {
        let checked: mlua::Value = check.call((mare_shared::lua::config_to_lua(&lua, &state)?, "state"))?;
        let checked = match checked {
            mlua::Value::Nil => state,
            other => mare_shared::lua::lua_to_json(&lua, other)?,
        };
        cfg.apply_state_defaults_from(checked);
    }
    for (k, v) in mare_config::env_overrides() {
        let values = serde_json::json!({ k.clone(): v.clone() });
        let checked: mlua::Value =
            check.call((mare_shared::lua::config_to_lua(&lua, &values)?, format!("env MARE_{}", k.to_uppercase())))?;
        let checked = match checked {
            mlua::Value::Nil => values,
            other => mare_shared::lua::lua_to_json(&lua, other)?,
        };
        mare_config::merge_json(&mut cfg.value, checked);
    }
    for binding in mare_shared::lua::core::config::plugin_env_bindings(&lua)? {
        let Ok(raw) = std::env::var(&binding.env_var) else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        let parsed = match mare_config::parse_value(&raw) {
            Ok(v) => v,
            Err(msg) => {
                mare_shared::lua::core::config::record_plugin_env_parse_error(&lua, &binding, &msg)?;
                continue;
            }
        };
        let mut layer_cfg = Config { value: Value::Object(Map::new()), warnings: vec![] };
        layer_cfg.set_dotted(&binding.path, parsed);
        let checked: mlua::Value = check
            .call((mare_shared::lua::config_to_lua(&lua, &layer_cfg.value)?, format!("env {}", binding.env_var)))?;
        let checked = match checked {
            mlua::Value::Nil => layer_cfg.value,
            other => mare_shared::lua::lua_to_json(&lua, other)?,
        };
        let mut cur = Some(&checked);
        for seg in binding.path.split('.') {
            cur = cur.and_then(|v| v.get(seg));
        }
        if let Some(val) = cur {
            cfg.set_dotted(&binding.path, val.clone());
        }
    }
    merge_package_opts(&lua, &mut cfg.value, None)?;
    let mut warnings = mare_config::env_warnings();
    warnings.extend(mare_shared::lua::core::config::warnings(&lua)?);
    cfg.warnings = warnings;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    #[test]
    fn preset_choices_match_loader_presets() {
        let lua = crate::new_lua().unwrap();
        let env = crate::Env {
            cwd: std::env::temp_dir(),
            mode: crate::Mode::Child,
            ui: mare_shared::ui::UiShared::new(),
            config: serde_json::json!({}),
            args: serde_json::Value::Null,
        };
        crate::install(&lua, &env).unwrap();
        let entry: mlua::Table = mare_shared::lua::core::config::plugin_preset_entry(&lua).unwrap().unwrap();
        let choices: Vec<String> = entry.get("choices").unwrap();
        let expected: Vec<&str> = crate::loader::PLUGIN_PRESETS.to_vec();
        assert_eq!(choices.iter().map(String::as_str).collect::<Vec<_>>(), expected);
    }

    #[test]
    fn fresh_install_config_identity_reused_once_and_cleared_on_reeval() {
        let lua = crate::new_lua().unwrap();
        let env = crate::Env {
            cwd: std::env::temp_dir(),
            mode: crate::Mode::Child,
            ui: mare_shared::ui::UiShared::new(),
            config: serde_json::json!({}),
            args: serde_json::Value::Null,
        };
        let mare = crate::install(&lua, &env).unwrap();
        let installed_cfg: mlua::Table = mare.get("config").unwrap();
        let taken = mare_shared::lua::core::config::take_fresh_install_config(&lua).unwrap().unwrap();
        assert_eq!(taken.to_pointer(), installed_cfg.to_pointer());
        assert!(mare_shared::lua::core::config::take_fresh_install_config(&lua).unwrap().is_none());

        // On an untouched fresh VM, `begin_reeval_state` must clear `fresh_install_config` on the
        // new candidate state without needing `take_fresh_install_config` first.
        let lua2 = crate::new_lua().unwrap();
        let env2 = crate::Env {
            cwd: std::env::temp_dir(),
            mode: crate::Mode::Child,
            ui: mare_shared::ui::UiShared::new(),
            config: serde_json::json!({}),
            args: serde_json::Value::Null,
        };
        crate::install(&lua2, &env2).unwrap();
        let undo = lua2.create_table().unwrap();
        let prev = mare_shared::lua::core::config::begin_reeval_state(&lua2, undo).unwrap();
        assert!(mare_shared::lua::core::config::take_fresh_install_config(&lua2).unwrap().is_none());
        // Restoring `prev` restores the untouched fresh-install table.
        mare_shared::lua::core::config::set_state(&lua2, prev).unwrap();
        assert!(mare_shared::lua::core::config::take_fresh_install_config(&lua2).unwrap().is_some());
    }

    #[test]
    fn config_engine_caches_loaded_chunks_on_core_config() {
        let lua = crate::new_lua().unwrap();
        let env = crate::Env {
            cwd: std::env::temp_dir(),
            mode: crate::Mode::Child,
            ui: mare_shared::ui::UiShared::new(),
            config: serde_json::json!({}),
            args: serde_json::Value::Null,
        };
        let mare = crate::install(&lua, &env).unwrap();
        let cfg = mare_shared::lua::core::config::handle(&lua).unwrap();
        let api_ptr_1 = cfg.raw_get::<mlua::Function>("config_api_chunk").unwrap().to_pointer();
        let pkg_ptr_1 = cfg.raw_get::<mlua::Function>("config_packages_chunk").unwrap().to_pointer();

        let c2 = lua.create_table().unwrap();
        crate::engines::config::load_config_api(&lua, &mare, &c2).unwrap();
        crate::engines::config::load_config_packages(&lua, &mare, &c2).unwrap();
        crate::engines::config::load_config_txn(&lua, &mare).unwrap();

        let api_ptr_2 = cfg.raw_get::<mlua::Function>("config_api_chunk").unwrap().to_pointer();
        let pkg_ptr_2 = cfg.raw_get::<mlua::Function>("config_packages_chunk").unwrap().to_pointer();
        let txn_ptr_1 = cfg.raw_get::<mlua::Function>("config_txn_chunk").unwrap().to_pointer();
        crate::engines::config::load_config_txn(&lua, &mare).unwrap();
        let txn_ptr_2 = cfg.raw_get::<mlua::Function>("config_txn_chunk").unwrap().to_pointer();

        assert_eq!(api_ptr_1, api_ptr_2);
        assert_eq!(pkg_ptr_1, pkg_ptr_2);
        assert_eq!(txn_ptr_1, txn_ptr_2);
    }

    #[test]
    fn builtin_lazy_loading_covers_env_override_typo_introspect_and_rollback() {
        let lua = crate::new_lua().unwrap();
        let env = crate::Env {
            cwd: std::env::temp_dir(),
            mode: crate::Mode::Child,
            ui: mare_shared::ui::UiShared::new(),
            config: serde_json::json!({}),
            args: serde_json::Value::Null,
        };
        let mare = crate::install(&lua, &env).unwrap();
        let cfg_tbl: mlua::Table = mare.get("config").unwrap();
        super::install_plugin_config_apis(&lua, &mare, &cfg_tbl, &std::env::temp_dir(), false, None).unwrap();

        let schemas = mare_shared::lua::core::config::ensure_plugin_schemas(&lua).unwrap();
        let defaults = mare_shared::lua::core::config::ensure_plugin_defaults(&lua).unwrap();
        let slots = mare_shared::lua::core::config::ensure_plugin_slots(&lua).unwrap();

        // Eager built-ins (`config_api` + `env` bindings): materialized immediately in debug & release.
        for eager in ["llm", "status", "tools", "user-config", "approvals"] {
            assert!(!defaults.raw_get::<mlua::Value>(eager).unwrap().is_nil(), "{eager} should be eager");
        }
        // Non-`config_api`, non-`env` built-ins stay lazy until touched (works in debug & release!).
        assert!(schemas.raw_get::<mlua::Value>("statusline").unwrap().is_nil());
        assert!(defaults.raw_get::<mlua::Value>("statusline").unwrap().is_nil());
        assert!(schemas.raw_get::<mlua::Value>("compact").unwrap().is_nil());

        // Single-source `plugin_env_bindings` discovers `MARE_APPROVAL_POLICY` without loading lazy built-ins.
        let bindings = mare_shared::lua::core::config::plugin_env_bindings(&lua).unwrap();
        assert!(bindings.iter().any(|b| b.env_var == "MARE_APPROVAL_POLICY"), "{bindings:?}");
        assert!(schemas.raw_get::<mlua::Value>("statusline").unwrap().is_nil());

        // Single-key `__index`: configuring `statusline` materializes only `statusline`, leaving `compact` lazy.
        lua.load(r#"mare.plugin("statusline", { bogus_key = true })"#).exec().unwrap();
        assert!(!schemas.raw_get::<mlua::Value>("statusline").unwrap().is_nil());
        assert!(schemas.raw_get::<mlua::Value>("compact").unwrap().is_nil());
        let warnings = mare_shared::lua::core::config::warnings(&lua).unwrap();
        assert!(warnings.iter().any(|w| w.contains("statusline") && w.contains("bogus_key")), "{warnings:?}");

        // Typo suggestion (`did you mean`) triggers `__pairs` and suggests the lazy built-in `compact`.
        lua.load(r#"mare.plugin("compct", {})"#).exec().unwrap();
        let warnings = mare_shared::lua::core::config::warnings(&lua).unwrap();
        assert!(warnings.iter().any(|w| w.contains("compct") && w.contains("compact")), "{warnings:?}");

        // `/config` introspection (`config_declared` + `config_describe`) sees lazy built-ins and their schemas.
        let declared: Vec<String> = lua.load("return mare.config_declared()").eval().unwrap();
        assert!(declared.contains(&"plugins.compact.enabled".to_string()));
        assert!(declared.contains(&"plugins.statusline.enabled".to_string()));
        let described: mlua::Table = lua
            .load(r#"return mare.config_describe({ "plugins.compact.enabled", "plugins.approvals.policy" })"#)
            .eval()
            .unwrap();
        assert!(described.get::<mlua::Table>("plugins.compact.enabled").is_ok());
        assert!(described.get::<mlua::Table>("plugins.approvals.policy").is_ok());

        // Staged `/reload config` + `override = true` + rollback restores prior state cleanly.
        let undo = lua.create_table().unwrap();
        let prev = mare_shared::lua::core::config::begin_reeval_state(&lua, undo).unwrap();
        let staged_cfg = lua.create_table().unwrap();
        let staged_view = lua.create_table().unwrap();
        staged_view.raw_set("config", staged_cfg.clone()).unwrap();
        super::install_plugin_config_apis(&lua, &mare, &staged_cfg, &std::env::temp_dir(), false, Some(&staged_view))
            .unwrap();
        let override_src = mare_kernel::PluginSource {
            name_hint: "compact".into(),
            origin: mare_kernel::Origin::Package,
            path: None,
            code: Some(
                br#"return {
                    name = "compact",
                    override = true,
                    config_schema = { custom_flag = { type = "boolean", default = true } },
                    apply = function() end,
                }"#,
            ),
        };
        super::declare_plugins(&lua, vec![override_src], Some(&staged_view)).unwrap();
        let staged_slots = mare_shared::lua::core::config::ensure_plugin_slots(&lua).unwrap();
        let staged_defaults = mare_shared::lua::core::config::ensure_plugin_defaults(&lua).unwrap();
        assert_eq!(staged_slots.get::<mlua::Table>("compact").unwrap().get::<String>("origin").unwrap(), "package");
        assert!(staged_defaults.raw_get::<mlua::Table>("compact").unwrap().get::<bool>("custom_flag").unwrap());

        // Rollback swaps back to `prev`: `compact` slot is builtin again and custom_flag is gone.
        mare_shared::lua::core::config::set_state(&lua, prev).unwrap();
        assert_eq!(slots.get::<mlua::Table>("compact").unwrap().get::<String>("origin").unwrap(), "builtin");
        assert!(defaults.get::<mlua::Table>("compact").unwrap().get::<mlua::Value>("custom_flag").unwrap().is_nil());
    }

    #[test]
    fn reload_config_lazy_registry_does_not_leak_tables_or_views() {
        let lua = crate::new_lua().unwrap();
        let env = crate::Env {
            cwd: std::env::temp_dir(),
            mode: crate::Mode::Child,
            ui: mare_shared::ui::UiShared::new(),
            config: serde_json::json!({}),
            args: serde_json::Value::Null,
        };
        let mare = crate::install(&lua, &env).unwrap();

        // Track every reload cycle's `plugin_schemas`, `plugin_defaults`, `plugin_slots`,
        // `lazy_builtins` userdata, and staged `config_view` in a weak-valued Lua table.
        let weak: mlua::Table = lua.load("return setmetatable({}, { __mode = 'v' })").eval().unwrap();
        let mut mem_after_warmup = 0.0f64;

        for iter in 0..20 {
            let undo = lua.create_table().unwrap();
            let _prev = mare_shared::lua::core::config::begin_reeval_state(&lua, undo).unwrap();
            let staged_cfg = lua.create_table().unwrap();
            let staged_view = lua.create_table().unwrap();
            staged_view.raw_set("config", staged_cfg.clone()).unwrap();
            super::install_plugin_config_apis(
                &lua,
                &mare,
                &staged_cfg,
                &std::env::temp_dir(),
                false,
                Some(&staged_view),
            )
            .unwrap();

            // Touch one lazy built-in in half the reloads to exercise `__index` materialization too.
            if iter % 2 == 0 {
                let s = mare_shared::lua::core::config::ensure_plugin_schemas(&lua).unwrap();
                let _ = s.get::<mlua::Value>("statusline").unwrap();
            }

            // Commit the reload (matches `reevaluate_config`, releasing the previous cycle's undo list).
            mare_shared::lua::core::config::set_api_undo(&lua, mlua::Value::Nil).unwrap();

            // Record the previous cycle's replaced state objects (everything except the final active state).
            if iter < 19 {
                let st = mare_shared::lua::core::config::state(&lua).unwrap();
                weak.raw_set(format!("schemas_{iter}"), st.raw_get::<mlua::Value>("plugin_schemas").unwrap()).unwrap();
                weak.raw_set(format!("defaults_{iter}"), st.raw_get::<mlua::Value>("plugin_defaults").unwrap())
                    .unwrap();
                weak.raw_set(format!("slots_{iter}"), st.raw_get::<mlua::Value>("plugin_slots").unwrap()).unwrap();
                weak.raw_set(format!("lazy_{iter}"), st.raw_get::<mlua::Value>("lazy_builtins").unwrap()).unwrap();
                weak.raw_set(format!("view_{iter}"), staged_view).unwrap();
            }
            if iter == 2 {
                lua.gc_collect().unwrap();
                lua.gc_collect().unwrap();
                mem_after_warmup = lua.load("return collectgarbage('count')").eval::<f64>().unwrap();
            }
        }

        // Two full GC cycles must collect every superseded reload's tables, userdata, and staged views.
        lua.gc_collect().unwrap();
        lua.gc_collect().unwrap();
        let remaining: Vec<String> =
            weak.pairs::<String, mlua::Value>().filter_map(|kv| kv.ok().map(|(k, _)| k)).collect();
        assert!(remaining.is_empty(), "leaked reload objects remained alive after GC: {remaining:?}");

        let mem_after_20: f64 = lua.load("return collectgarbage('count')").eval().unwrap();
        assert!(
            mem_after_20 < mem_after_warmup + 256.0,
            "Lua heap grew across 20 reloads: warmup={mem_after_warmup:.1}KB, after_20={mem_after_20:.1}KB"
        );
    }
}
