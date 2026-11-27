//! Plugin implementation modules for `require`: one open loader with owner-partitioned caches,
//! cross-plugin root ambiguity detection, cycle detection, and dependent-reload tracking.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use mlua::{Function, MultiValue, Table, Value};

use crate::bytecode::load_cached_chunk;
use crate::{Kernel, Origin, PluginDef};

/// The owner name for core modules (`cordis*`, `mare_kernel*`, `mare_lib*`, `mare.test`).
pub(crate) const CORE_OWNER: &str = "core";

/// Module roots reserved for core; a plugin `lua/` defining one records a doctor problem.
const CORE_ROOTS: &[&str] = &["cordis", "mare_kernel", "mare_lib", "mare"];

#[derive(Default)]
pub(crate) struct Modules {
    /// plugin -> (`lua/` source, plugin environment RegistryKey, env table pointer)
    dirs: HashMap<String, (ModuleDir, mlua::RegistryKey, usize)>,
    /// module root name -> sorted unique list of providing plugin owners
    roots: HashMap<String, Vec<String>>,
    /// plugin owner -> RegistryKey(Table) holding that owner's loaded modules (`core` uses `package.loaded`)
    loaded: HashMap<String, mlua::RegistryKey>,
    /// caller owner -> set of provider owners it required during its current apply/run
    deps: HashMap<String, BTreeSet<String>>,
    /// in-flight `(provider, module_name)` stack for cycle detection
    loading: Vec<(String, String)>,
    /// shared isolated environment for `core`-owned modules (`mare_lib*`, `cordis*`, `mare_kernel*`)
    core_env: Option<mlua::RegistryKey>,
    /// compiled Lua wrapper factory (`@core/require`) reused across all `make_require` calls
    require_wrapper: Option<mlua::RegistryKey>,
    /// last seen hash of disk-backed `core` modules (`mare_lib*`) for dev-mode reload unloading
    core_modules_hash: Option<String>,
    /// provider owner -> modules hash snapshot taken when its loaded partition was first populated
    loaded_hashes: HashMap<String, String>,
    /// plugins that completed `apply` and have `ROW_CACHE[name].dep_prints` initialized
    active: HashSet<String>,
    /// already-reported `(plugin, root)` reserved-root doctor problems
    reserved_root_problems: HashSet<(String, String)>,
}

/// Where a plugin's implementation modules come from.
#[derive(Clone, PartialEq, Eq)]
enum ModuleDir {
    /// a directory plugin's own `lua/`: any module name
    Own(PathBuf),
    /// a built-in plugin (`preset/plugins/<name>.lua`): the shared `preset/plugins/lua/`, modules
    /// whose root is `<name>`
    Shared(PathBuf),
    /// a built-in plugin loaded from embedded bytecode: [`builtin_modules`], same naming rule
    Embedded,
}

/// Built-in implementation modules embedded in the binary: (module name, precompiled bytecode),
/// e.g. ("mcp.panel", …) for `preset/plugins/lua/mcp/panel.lua`. Set once by mare-plugin.
type BuiltinModuleEntry = (&'static str, &'static [u8]);
type BuiltinModulesByOwner = HashMap<&'static str, Vec<BuiltinModuleEntry>>;

static BUILTIN_MODULES: std::sync::OnceLock<&'static [BuiltinModuleEntry]> = std::sync::OnceLock::new();
static BUILTIN_MODULES_BY_OWNER: std::sync::OnceLock<BuiltinModulesByOwner> = std::sync::OnceLock::new();

pub fn set_builtin_modules(m: &'static [(&'static str, &'static [u8])]) {
    debug_assert!(
        m.windows(2).all(|w| w[0].0 < w[1].0),
        "set_builtin_modules requires strictly sorted unique module names"
    );
    let _ = BUILTIN_MODULES.set(m);
}

fn builtin_modules() -> &'static [(&'static str, &'static [u8])] {
    BUILTIN_MODULES.get().copied().unwrap_or(&[])
}

fn index_builtin_modules(mods: &'static [BuiltinModuleEntry]) -> BuiltinModulesByOwner {
    let mut map: BuiltinModulesByOwner = HashMap::new();
    for &(name, code) in mods {
        if let Some(root) = builtin_root_owner(name) {
            map.entry(root).or_default().push((name, code));
        }
    }
    map
}

fn builtin_modules_for(owner: &str) -> &'static [BuiltinModuleEntry] {
    let Some(mods) = BUILTIN_MODULES.get().copied() else {
        return &[];
    };
    let index = BUILTIN_MODULES_BY_OWNER.get_or_init(|| index_builtin_modules(mods));
    index.get(owner).map_or(&[], Vec::as_slice)
}

fn lookup_builtin_module<'a>(mods: &'a [(&str, &'a [u8])], name: &str) -> Option<&'a [u8]> {
    mods.binary_search_by_key(&name, |(n, _)| *n).ok().map(|i| mods[i].1)
}

/// First dot-separated segment of a module name.
fn module_root(name: &str) -> &str {
    name.split('.').next().unwrap_or(name)
}

/// Whether module `name` belongs to root `owner` (`owner` or `owner.…`).
fn owned_by(name: &str, owner: &str) -> bool {
    name == owner || name.strip_prefix(owner).is_some_and(|r| r.starts_with('.'))
}

/// Packaging ownership of a module inside the shared built-in `preset/plugins/lua/` (or embedded
/// `BUILTIN_MODULES`): `mare_lib*` belongs to `core`, everything else belongs to the built-in
/// named by its root segment.
pub(crate) fn builtin_root_owner(name: &str) -> Option<&str> {
    if name.is_empty() {
        return None;
    }
    if owned_by(name, "mare_lib") { Some(CORE_OWNER) } else { Some(module_root(name)) }
}

/// Whether `name` belongs to the `core` owner (`cordis*`, `mare_kernel*`, `mare_lib*`, `mare.test`).
fn is_core_module(name: &str) -> bool {
    owned_by(name, "cordis") || owned_by(name, "mare_kernel") || owned_by(name, "mare_lib") || name == "mare.test"
}

/// `(module name, bytecode)` of every embedded kernel library module (`cordis*`, `mare_kernel*`),
/// compiled by build.rs under `@kernel/lua/<path>` with debug info kept.
static KERNEL_LUA: &[(&str, &[u8])] = include!(concat!(env!("OUT_DIR"), "/kernel_lua.rs"));

fn valid_module_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('\0')
        && name.split('.').all(|seg| {
            !seg.is_empty()
                && seg != "."
                && seg != ".."
                && !seg.contains(['/', '\\'])
                && seg.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-')
        })
}

enum Resolution {
    CoreKernel {
        code: &'static [u8],
        chunk_name: String,
    },
    CoreDisk {
        path: PathBuf,
    },
    CoreEmbedded {
        code: &'static [u8],
        chunk_name: String,
    },
    /// Built-in plugin module loaded from disk in debug/dev mode (never uses disk luac cache).
    BuiltinDisk {
        provider: String,
        path: PathBuf,
    },
    /// Third-party / user / project plugin module (`ModuleDir::Own`), cached in `$XDG_CACHE_HOME/mare/luac`
    /// keyed by `(owner_dir, path, mtime, size)`.
    PluginDisk {
        provider: String,
        owner_dir: PathBuf,
        path: PathBuf,
    },
    PluginEmbedded {
        provider: String,
        code: &'static [u8],
        chunk_name: String,
    },
}

impl Resolution {
    fn provider(&self) -> &str {
        match self {
            Self::CoreKernel { .. } | Self::CoreDisk { .. } | Self::CoreEmbedded { .. } => CORE_OWNER,
            Self::BuiltinDisk { provider, .. }
            | Self::PluginDisk { provider, .. }
            | Self::PluginEmbedded { provider, .. } => provider,
        }
    }
}

/// Check `<dir>/<rel>.lua` and `<dir>/<rel>/init.lua` with process-path policy and symlink guards.
fn find_safe_file(lua: &mlua::Lua, dir: &Path, rel: &str, tried: &mut String) -> mlua::Result<Option<PathBuf>> {
    let canon_dir = dunce::canonicalize(dir).ok();
    for cand in [dir.join(format!("{rel}.lua")), dir.join(rel).join("init.lua")] {
        // Candidate metadata and the source/bytecode cache both derive from this path.
        // A tool must not use the module loader to read around its policy.
        mare_shared::exec_ctx::check_process_path(lua, "read", &cand.to_string_lossy())?;
        let is_safe = canon_dir.as_ref().is_some_and(|root| {
            std::fs::symlink_metadata(&cand).is_ok_and(|m| {
                !m.file_type().is_symlink()
                    && m.file_type().is_file()
                    && dunce::canonicalize(&cand).is_ok_and(|c| c.starts_with(root))
            })
        });
        if is_safe {
            return Ok(Some(cand));
        }
        let _ = std::fmt::Write::write_fmt(tried, format_args!("\n\tno file '{}'", cand.display()));
    }
    Ok(None)
}

/// Scope guard popping `(provider, name)` from `Modules::loading` even if Lua raises.
struct LoadingGuard {
    kernel: Kernel,
    provider: String,
    name: String,
}

impl Drop for LoadingGuard {
    fn drop(&mut self) {
        if let Ok(mut m) = self.kernel.0.modules.lock()
            && let Some(pos) = m.loading.iter().rposition(|(p, n)| p == &self.provider && n == &self.name)
        {
            m.loading.remove(pos);
        }
    }
}

impl Kernel {
    /// Test hosts only (`mare test`, the kernel tests): `require("mare_kernel.test").boot()`
    /// hands a test the booted host. A production host never registers it.
    pub fn expose_test_boot(&self) {
        let w = self.weak();
        let lua = self.lua();
        let result: mlua::Result<()> = (|| {
            let boot = lua.create_function(move |_, ()| match Self::from_weak(&w) {
                Some(k) => k.cordis_boot(),
                None => Err(mlua::Error::runtime("kernel gone")),
            })?;
            let module = lua.create_table()?;
            module.set("boot", boot)?;
            let loader = lua.create_function(move |_, ()| Ok(module.clone()))?;
            let package: Table = lua.globals().get("package")?;
            let preload: Table = package.get("preload")?;
            preload.set("mare_kernel.test", loader)
        })();
        if let Err(e) = result {
            tracing::warn!("mare_kernel.test unavailable: {e}");
        }
    }

    fn core_env(&self) -> mlua::Result<Table> {
        let lua = self.lua();
        {
            let m = self.0.modules.lock().unwrap();
            if let Some(key) = &m.core_env
                && let Ok(t) = lua.registry_value::<Table>(key)
            {
                return Ok(t);
            }
        }
        let env = mare_shared::lua::isolated_env(lua)?;
        env.raw_set("require", self.make_require(CORE_OWNER)?)?;
        let key = lua.create_registry_value(env.clone())?;
        self.0.modules.lock().unwrap().core_env = Some(key);
        Ok(env)
    }

    fn core_shared_lua_dir(&self) -> Option<PathBuf> {
        if let Some(dir) = crate::lua_env::public_modules::builtin_lua_dir()
            && dir.is_dir()
        {
            return Some(dir);
        }
        let m = self.0.modules.lock().unwrap();
        m.dirs.values().find_map(|(d, _, _)| match d {
            ModuleDir::Shared(p) => Some(p.clone()),
            _ => None,
        })
    }

    /// Read an already-loaded module from `provider`'s in-memory partition without allocating a
    /// table or touching the filesystem/policy.
    fn loaded_in_partition(&self, provider: &str, name: &str) -> mlua::Result<Option<Value>> {
        let lua = self.lua();
        let tbl = {
            let m = self.0.modules.lock().unwrap();
            m.loaded.get(provider).and_then(|key| lua.registry_value::<Table>(key).ok())
        };
        let Some(tbl) = tbl else { return Ok(None) };
        let val: Value = tbl.raw_get(name)?;
        if val != Value::Nil && val != Value::Boolean(false) { Ok(Some(val)) } else { Ok(None) }
    }

    /// Compiled once per VM: wraps a Rust `require` closure so failures raise native Lua strings.
    fn require_wrapper(&self) -> mlua::Result<Function> {
        let lua = self.lua();
        {
            let m = self.0.modules.lock().unwrap();
            if let Some(key) = &m.require_wrapper
                && let Ok(f) = lua.registry_value::<Function>(key)
            {
                return Ok(f);
            }
        }
        let factory = lua
            .load(
                r#"local inner = ...
return function(name)
    local ok, a, b = inner(name)
    if not ok then
        error(a, 2)
    end
    return a, b
end"#,
            )
            .set_name("@core/require")
            .into_function()?;
        let key = lua.create_registry_value(factory.clone())?;
        self.0.modules.lock().unwrap().require_wrapper = Some(key);
        Ok(factory)
    }

    fn owner_partition(&self, provider: &str) -> mlua::Result<Table> {
        let lua = self.lua();
        if provider == CORE_OWNER {
            let package: Table = lua.globals().get("package")?;
            return package.get("loaded");
        }
        {
            let m = self.0.modules.lock().unwrap();
            if let Some(key) = m.loaded.get(provider)
                && let Ok(tbl) = lua.registry_value::<Table>(key)
            {
                return Ok(tbl);
            }
        }
        let tbl = lua.create_table()?;
        let key = lua.create_registry_value(tbl.clone())?;
        self.0.modules.lock().unwrap().loaded.insert(provider.to_string(), key);
        Ok(tbl)
    }

    fn provider_env(&self, provider: &str) -> mlua::Result<Table> {
        if provider == CORE_OWNER {
            return self.core_env();
        }
        let lua = self.lua();
        let key_env = {
            let m = self.0.modules.lock().unwrap();
            m.dirs.get(provider).and_then(|(_, key, _)| lua.registry_value::<Table>(key).ok())
        };
        match key_env {
            Some(env) => Ok(env),
            None => self.core_env(),
        }
    }

    /// Resolve `(caller, name)` according to the open owner-partitioned model:
    /// Step 3: caller's own `lua/` (unless name is a reserved core module)
    /// Step 4: core modules (`cordis*`, `mare_kernel*`, `mare_lib*`, `mare.test`)
    /// Step 5: unique plugin providing `module_root(name)` (error naming all if >= 2).
    fn resolve(&self, caller: &str, name: &str) -> mlua::Result<Result<Resolution, String>> {
        let lua = self.lua();
        let rel = name.replace('.', "/");
        let mut tried = String::new();

        // Step 3: caller's own `lua/` (takes priority over cross-plugin lookup, never hijacks core).
        if caller != CORE_OWNER && !is_core_module(name) {
            let caller_dir = self.0.modules.lock().unwrap().dirs.get(caller).map(|(d, _, _)| d.clone());
            if let Some(dir) = caller_dir {
                match dir {
                    ModuleDir::Own(d) => {
                        if let Some(path) = find_safe_file(lua, &d, &rel, &mut tried)? {
                            return Ok(Ok(Resolution::PluginDisk { provider: caller.to_string(), owner_dir: d, path }));
                        }
                    }
                    ModuleDir::Shared(d) if builtin_root_owner(name) == Some(caller) => {
                        if let Some(path) = find_safe_file(lua, &d, &rel, &mut tried)? {
                            return Ok(Ok(Resolution::BuiltinDisk { provider: caller.to_string(), path }));
                        }
                    }
                    ModuleDir::Embedded if builtin_root_owner(name) == Some(caller) => {
                        if let Some(code) = lookup_builtin_module(builtin_modules(), name) {
                            return Ok(Ok(Resolution::PluginEmbedded {
                                provider: caller.to_string(),
                                code,
                                chunk_name: format!("@builtin/lua/{rel}.lua"),
                            }));
                        }
                    }
                    _ => {}
                }
            }
        }

        // Step 4: core modules (`cordis*`, `mare_kernel*`, `mare_lib*`, `mare.test`).
        if is_core_module(name) {
            if owned_by(name, "cordis") || owned_by(name, "mare_kernel") {
                if let Some((_, code)) = KERNEL_LUA.iter().find(|(n, _)| *n == name) {
                    return Ok(Ok(Resolution::CoreKernel { code, chunk_name: format!("@kernel/lua/{rel}.lua") }));
                }
                return Ok(Err(format!("\n\tno kernel library module '{name}'")));
            }
            if owned_by(name, "mare_lib") {
                if let Some(shared_dir) = self.core_shared_lua_dir()
                    && let Some(path) = find_safe_file(lua, &shared_dir, &rel, &mut tried)?
                {
                    return Ok(Ok(Resolution::CoreDisk { path }));
                }
                if let Some(code) = lookup_builtin_module(builtin_modules(), name) {
                    return Ok(Ok(Resolution::CoreEmbedded { code, chunk_name: format!("@builtin/lua/{rel}.lua") }));
                }
                return Ok(Err(if tried.is_empty() { format!("\n\tno core module '{name}'") } else { tried }));
            }
            return Ok(Err(format!("\n\tno core module '{name}'")));
        }

        // Step 5: cross-plugin lookup by module root segment.
        let root = module_root(name);
        let providers: Vec<String> = {
            let m = self.0.modules.lock().unwrap();
            m.roots.get(root).cloned().unwrap_or_default()
        };
        if providers.len() >= 2 {
            return Ok(Err(format!(
                "module '{name}' is ambiguous: root '{root}' is provided by multiple plugins ({})",
                providers.join(", ")
            )));
        }
        if let Some(provider) = providers.into_iter().next() {
            let prov_dir = self.0.modules.lock().unwrap().dirs.get(&provider).map(|(d, _, _)| d.clone());
            if let Some(dir) = prov_dir {
                match dir {
                    ModuleDir::Own(d) => {
                        if let Some(path) = find_safe_file(lua, &d, &rel, &mut tried)? {
                            return Ok(Ok(Resolution::PluginDisk { provider, owner_dir: d, path }));
                        }
                    }
                    ModuleDir::Shared(d) if builtin_root_owner(name) == Some(provider.as_str()) => {
                        if let Some(path) = find_safe_file(lua, &d, &rel, &mut tried)? {
                            return Ok(Ok(Resolution::BuiltinDisk { provider, path }));
                        }
                    }
                    ModuleDir::Embedded if builtin_root_owner(name) == Some(provider.as_str()) => {
                        if let Some(code) = lookup_builtin_module(builtin_modules(), name) {
                            return Ok(Ok(Resolution::PluginEmbedded {
                                provider,
                                code,
                                chunk_name: format!("@builtin/lua/{rel}.lua"),
                            }));
                        }
                    }
                    _ => {}
                }
            }
        }

        if tried.is_empty() {
            tried = format!("\n\tno field package.preload['{name}']\n\tno plugin provides module '{name}'");
        }
        Ok(Err(tried))
    }

    fn load_resolution(&self, res: &Resolution, name: &str) -> mlua::Result<(Function, String)> {
        let lua = self.lua();
        let env = self.provider_env(res.provider())?;
        match res {
            Resolution::CoreKernel { code, chunk_name }
            | Resolution::CoreEmbedded { code, chunk_name }
            | Resolution::PluginEmbedded { code, chunk_name, .. } => {
                let f = lua
                    .load(*code)
                    .set_mode(mlua::chunk::ChunkMode::Binary)
                    .set_name(chunk_name.clone())
                    .set_environment(env)
                    .into_function()?;
                Ok((f, chunk_name[1..].to_string()))
            }
            Resolution::CoreDisk { path } | Resolution::BuiltinDisk { path, .. } => {
                mare_shared::exec_ctx::check_process_path(lua, "read", &path.to_string_lossy())
                    .map_err(|e| mlua::Error::runtime(format!("loading module {name}: {e}")))?;
                let src =
                    std::fs::read(path).map_err(|e| mlua::Error::runtime(format!("loading module {name}: {e}")))?;
                let chunk_name = format!("@{}", path.display());
                let f = lua
                    .load(&src[..])
                    .set_mode(mlua::chunk::ChunkMode::Text)
                    .set_name(chunk_name)
                    .set_environment(env)
                    .into_function()?;
                Ok((f, path.to_string_lossy().into_owned()))
            }
            Resolution::PluginDisk { owner_dir, path, .. } => {
                let chunk_name = format!("@{}", path.display());
                let f = load_cached_chunk(lua, Some(owner_dir), path, &chunk_name, Some(env))
                    .map_err(|e| mlua::Error::runtime(format!("loading module {name}: {e}")))?;
                Ok((f, path.to_string_lossy().into_owned()))
            }
        }
    }

    /// Build a `require(name)` function bound to `owner` (plugin name or `core`).
    pub(crate) fn make_require(&self, owner: &str) -> mlua::Result<Function> {
        let lua = self.lua();
        let w = self.weak();
        let owner = owner.to_string();
        let inner = lua.create_function(move |lua, name: String| {
            let err_ret = |msg: String| -> mlua::Result<MultiValue> {
                Ok(MultiValue::from_vec(vec![Value::Boolean(false), Value::String(lua.create_string(&msg)?)]))
            };
            let Some(k) = Self::from_weak(&w) else {
                return err_ret("kernel gone".to_string());
            };
            if !valid_module_name(&name) {
                return err_ret(format!("module '{name}' not found:\n\tinvalid module name '{name}'"));
            }

            let package: Table = lua.globals().get("package")?;
            let pkg_loaded: Table = package.get("loaded")?;

            // Step 1: global overrides and core modules in `package.loaded`.
            let existing: Value = pkg_loaded.raw_get(name.as_str())?;
            if existing != Value::Nil && existing != Value::Boolean(false) {
                if is_core_module(&name) {
                    k.record_module_dep(&owner, CORE_OWNER);
                }
                return Ok(MultiValue::from_vec(vec![Value::Boolean(true), existing]));
            }

            // Step 2: `package.preload[name]` takes highest precedence.
            let preload: Table = package.get("preload")?;
            if let Value::Function(loader) = preload.raw_get::<Value>(name.as_str())? {
                let loader_data = Value::String(lua.create_string(":preload:")?);
                let res: Value = loader.call((name.as_str(), loader_data.clone()))?;
                let stored = if res == Value::Nil {
                    let cur: Value = pkg_loaded.raw_get(name.as_str())?;
                    if cur == Value::Nil {
                        pkg_loaded.raw_set(name.as_str(), true)?;
                        Value::Boolean(true)
                    } else {
                        cur
                    }
                } else {
                    pkg_loaded.raw_set(name.as_str(), res.clone())?;
                    res
                };
                return Ok(MultiValue::from_vec(vec![Value::Boolean(true), stored, loader_data]));
            }

            // Fast path (memory-only, zero disk I/O or FS policy checks):
            // 3a. Caller's own loaded partition.
            if owner != CORE_OWNER
                && !is_core_module(&name)
                && let Some(cached) = k.loaded_in_partition(&owner, &name)?
            {
                return Ok(MultiValue::from_vec(vec![Value::Boolean(true), cached]));
            }
            // 3b. Unique cross-plugin provider from the in-memory root index.
            if !is_core_module(&name) {
                let unique_other = {
                    let m = k.0.modules.lock().unwrap();
                    m.roots
                        .get(module_root(&name))
                        .and_then(|provs| (provs.len() == 1 && provs[0] != owner).then(|| provs[0].clone()))
                };
                if let Some(provider) = unique_other
                    && let Some(cached) = k.loaded_in_partition(&provider, &name)?
                {
                    k.record_module_dep(&owner, &provider);
                    return Ok(MultiValue::from_vec(vec![Value::Boolean(true), cached]));
                }
            }

            // Slow path (cache miss): resolve on disk / embedded bytecode with FS policy checks.
            let res = match k.resolve(&owner, &name)? {
                Ok(r) => r,
                Err(msg) => {
                    if msg.starts_with("module '") {
                        return err_ret(msg);
                    }
                    return err_ret(format!("module '{name}' not found:{msg}"));
                }
            };
            let provider = res.provider().to_string();

            // Check provider's loaded partition (covers ambiguous-root own-file or embedded hit).
            let partition = k.owner_partition(&provider)?;
            let cached: Value = partition.raw_get(name.as_str())?;
            if cached != Value::Nil && cached != Value::Boolean(false) {
                k.record_module_dep(&owner, &provider);
                return Ok(MultiValue::from_vec(vec![Value::Boolean(true), cached]));
            }

            if matches!(res, Resolution::CoreDisk { .. }) {
                let need_init = k.0.modules.lock().unwrap().core_modules_hash.is_none();
                if need_init {
                    let h = k.core_modules_hash_string();
                    k.0.modules.lock().unwrap().core_modules_hash = Some(h);
                }
            } else if provider != CORE_OWNER {
                k.ensure_provider_loaded_hash(&provider);
            }

            // Track cross-owner dependency edge `owner -> provider` (only for plugin callers;
            // `core` is the global require used by eval/init.lua and is never reloaded as a plugin).
            k.record_module_dep(&owner, &provider);

            // Cycle detection on `(provider, name)`.
            {
                let mut m = k.0.modules.lock().unwrap();
                if let Some(pos) = m.loading.iter().position(|(p, n)| p == &provider && n == &name) {
                    let mut chain: Vec<&str> = m.loading[pos..].iter().map(|(_, n)| n.as_str()).collect();
                    chain.push(name.as_str());
                    return err_ret(format!("require cycle: {}", chain.join(" -> ")));
                }
                m.loading.push((provider.clone(), name.clone()));
            }
            let _guard = LoadingGuard { kernel: k.clone(), provider: provider.clone(), name: name.clone() };

            let (chunk_fn, loader_data_str) = k.load_resolution(&res, &name)?;
            let loader_data = Value::String(lua.create_string(&loader_data_str)?);
            let result: Value = chunk_fn.call((name.as_str(), loader_data.clone()))?;
            drop(_guard);

            let stored = if result == Value::Nil {
                let cur: Value = partition.raw_get(name.as_str())?;
                if cur == Value::Nil {
                    partition.raw_set(name.as_str(), true)?;
                    Value::Boolean(true)
                } else {
                    cur
                }
            } else {
                partition.raw_set(name.as_str(), result.clone())?;
                result
            };
            Ok(MultiValue::from_vec(vec![Value::Boolean(true), stored, loader_data]))
        })?;
        self.require_wrapper()?.call::<Function>(inner)
    }

    /// Install the global `require` (bound to `core`), clear `package.path`/`package.cpath` and
    /// Lua's default file/C searchers so only `package.preload` (`package.searchers[1]`) remains.
    pub(crate) fn install_searcher(&self) -> mlua::Result<()> {
        let lua = self.lua();
        let package: Table = lua.globals().get("package")?;
        let searchers: Table = package.get("searchers")?;
        lua.load(
            r#"local s, package = ...
               for i = #s, 2, -1 do s[i] = nil end
               package.path, package.cpath = "", """#,
        )
        .set_name("@core/searchers")
        .call::<()>((searchers, package))?;
        lua.globals().raw_set("require", self.make_require(CORE_OWNER)?)?;
        Ok(())
    }

    /// Register a plugin's implementation modules for `require` and refresh the root index.
    pub(crate) fn add_module_dir(&self, d: &PluginDef) {
        let Some(dir) = module_dir_of(d) else { return };
        let env_ptr = d.env.to_pointer() as usize;
        {
            let m = self.0.modules.lock().unwrap();
            if m.dirs.get(&d.name).is_some_and(|(prev_dir, _, prev_ptr)| prev_dir == &dir && *prev_ptr == env_ptr) {
                return;
            }
        }
        let Ok(env) = self.lua().create_registry_value(d.env.clone()) else { return };
        let provided_roots = roots_of(&d.name, &dir);
        let mut reserved_problems = Vec::new();
        {
            let mut m = self.0.modules.lock().unwrap();
            remove_from_roots(&mut m.roots, &d.name);
            for root in provided_roots {
                if CORE_ROOTS.contains(&root.as_str()) {
                    if m.reserved_root_problems.insert((d.name.clone(), root.clone())) {
                        reserved_problems.push(format!(
                            "plugin `{}` defines module root `{}` which is reserved by core",
                            d.name, root
                        ));
                    }
                    continue;
                }
                let entry = m.roots.entry(root).or_default();
                if let Err(pos) = entry.binary_search(&d.name) {
                    entry.insert(pos, d.name.clone());
                }
            }
            m.dirs.insert(d.name.clone(), (dir, env, env_ptr));
        }
        for msg in reserved_problems {
            tracing::warn!("{msg}");
            self.add_problem(msg);
        }
    }

    /// Feed `h` with what plugin `d` provides from its own module dir.
    pub(crate) fn hash_modules(d: &PluginDef, h: &mut impl std::hash::Hasher) {
        match module_dir_of(d) {
            Some(ModuleDir::Own(root)) => hash_root_dir(&root, &|_| true, h),
            Some(ModuleDir::Shared(root)) => {
                hash_root_dir(&root, &|n| builtin_root_owner(n) == Some(d.name.as_str()), h)
            }
            Some(ModuleDir::Embedded) => {
                use std::hash::Hash;
                for (n, code) in builtin_modules_for(&d.name) {
                    n.hash(h);
                    (code.as_ptr() as usize, code.len()).hash(h);
                }
            }
            None => {}
        }
    }

    /// Hash disk-backed `core` modules (`mare_lib*` in dev mode) so edits trigger dependent reload.
    pub(crate) fn hash_core_modules(&self, h: &mut impl std::hash::Hasher) {
        if let Some(dir) = self.core_shared_lua_dir() {
            hash_root_dir(&dir, &|n| builtin_root_owner(n) == Some(CORE_OWNER), h);
        }
    }

    fn core_modules_hash_string(&self) -> String {
        use std::hash::Hasher;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.hash_core_modules(&mut h);
        format!("{:016x}", h.finish())
    }

    /// During reload (after old rows leave and before `apply_rows`), drop disk-backed `mare_lib*`
    /// modules from `package.loaded` if dev sources changed since they were last loaded.
    pub(crate) fn sync_core_modules(&self) {
        let prev = self.0.modules.lock().unwrap().core_modules_hash.clone();
        let Some(prev) = prev else { return };
        let cur = self.core_modules_hash_string();
        if cur != prev {
            self.unload_core_mare_lib();
            self.0.modules.lock().unwrap().core_modules_hash = Some(cur);
        }
    }

    /// Drop disk-backed `mare_lib*` modules from `package.loaded` when dev sources changed.
    fn unload_core_mare_lib(&self) {
        let Ok(package) = self.lua().globals().get::<Table>("package") else { return };
        let Ok(loaded) = package.get::<Table>("loaded") else { return };
        let mut keys = Vec::new();
        for pair in loaded.clone().pairs::<Value, Value>() {
            if let Ok((Value::String(k), _)) = pair
                && let Ok(s) = k.to_str()
                && owned_by(&s, "mare_lib")
            {
                keys.push(s.to_string());
            }
        }
        for k in keys {
            let _ = loaded.raw_set(k, Value::Nil);
        }
    }

    /// Snapshot `provider`'s modules hash when its loaded partition is first populated so later
    /// cross-owner cache hits record the version that was actually loaded into memory.
    fn ensure_provider_loaded_hash(&self, provider: &str) {
        if self.0.modules.lock().unwrap().loaded_hashes.contains_key(provider) {
            return;
        }
        let h = self.provider_modules_hash(provider);
        self.0.modules.lock().unwrap().loaded_hashes.entry(provider.to_string()).or_insert(h);
    }

    /// Cached modules hash for `provider`'s currently loaded partition, if populated.
    pub(crate) fn cached_provider_modules_hash(&self, provider: &str) -> Option<String> {
        let m = self.0.modules.lock().unwrap();
        if provider == CORE_OWNER { m.core_modules_hash.clone() } else { m.loaded_hashes.get(provider).cloned() }
    }

    /// Mark `plugin` active for incremental post-`apply` dependency fingerprint recording.
    pub(crate) fn mark_module_active(&self, plugin: &str) {
        self.0.modules.lock().unwrap().active.insert(plugin.to_string());
    }

    /// Clear `plugin`'s active flag when its fiber begins unloading.
    pub(crate) fn mark_module_inactive(&self, plugin: &str) {
        self.0.modules.lock().unwrap().active.remove(plugin);
    }

    /// Record a cross-owner `require` edge `caller -> provider`. When `caller` is already active
    /// (lazy `require` after `apply()`), incrementally record `provider`'s loaded modules hash in
    /// `ROW_CACHE[caller].dep_prints`.
    fn record_module_dep(&self, caller: &str, provider: &str) {
        if caller == CORE_OWNER || caller == provider {
            return;
        }
        let record_row = {
            let mut m = self.0.modules.lock().unwrap();
            let inserted = m.deps.entry(caller.to_string()).or_default().insert(provider.to_string());
            inserted && m.active.contains(caller)
        };
        if record_row {
            self.record_row_dep_print(caller, provider);
        }
    }

    /// Provider owners that `plugin` required during its current apply/run.
    pub(crate) fn module_deps(&self, plugin: &str) -> Vec<String> {
        self.0.modules.lock().unwrap().deps.get(plugin).map(|s| s.iter().cloned().collect()).unwrap_or_default()
    }

    /// Transitive closure of plugins that required modules from `provider`, in BFS dependency order
    /// (direct dependents before transitive dependents).
    #[doc(hidden)]
    pub fn module_dependents(&self, provider: &str) -> Vec<String> {
        let m = self.0.modules.lock().unwrap();
        let mut out = Vec::new();
        let mut seen = HashSet::from([provider.to_string()]);
        let mut queue = VecDeque::from([provider.to_string()]);
        while let Some(cur) = queue.pop_front() {
            let mut next: Vec<String> = m
                .deps
                .iter()
                .filter(|(caller, providers)| {
                    caller.as_str() != CORE_OWNER && providers.contains(&cur) && !seen.contains(caller.as_str())
                })
                .map(|(caller, _)| caller.clone())
                .collect();
            next.sort();
            for dep in next {
                seen.insert(dep.clone());
                out.push(dep.clone());
                queue.push_back(dep);
            }
        }
        out
    }

    /// Whether plugin `name`'s `lua/` dir is registered with the module loader (tests).
    #[doc(hidden)]
    pub fn module_dir_registered(&self, name: &str) -> bool {
        self.0.modules.lock().unwrap().dirs.contains_key(name)
    }

    /// Whether `module` is currently loaded in `owner`'s module partition (`"core"` = `package.loaded`).
    #[doc(hidden)]
    pub fn is_module_loaded(&self, owner: &str, module: &str) -> bool {
        let lua = self.lua();
        if owner == CORE_OWNER {
            return lua
                .globals()
                .get::<Table>("package")
                .and_then(|p| p.get::<Table>("loaded"))
                .and_then(|l| l.raw_get::<Value>(module))
                .is_ok_and(|v| v != Value::Nil && v != Value::Boolean(false));
        }
        let m = self.0.modules.lock().unwrap();
        m.loaded
            .get(owner)
            .and_then(|key| lua.registry_value::<Table>(key).ok())
            .and_then(|tbl| tbl.raw_get::<Value>(module).ok())
            .is_some_and(|v| v != Value::Nil && v != Value::Boolean(false))
    }

    /// O(1) unload of a plugin's module partition, dependency edges, and root index entries.
    pub(crate) fn unload_modules(&self, plugin: &str) {
        let mut m = self.0.modules.lock().unwrap();
        m.loaded.remove(plugin);
        m.deps.remove(plugin);
        m.dirs.remove(plugin);
        m.loaded_hashes.remove(plugin);
        m.active.remove(plugin);
        remove_from_roots(&mut m.roots, plugin);
    }

    /// Total number of loaded Lua modules across `package.loaded` (`core`) and every plugin's
    /// owner partition (`modules.loaded`).
    pub(crate) fn loaded_module_count(&self) -> u32 {
        let lua = self.lua();
        let mut total = lua
            .globals()
            .get::<Table>("package")
            .and_then(|pkg| pkg.get::<Table>("loaded"))
            .map_or(0, |loaded| loaded.pairs::<Value, Value>().count() as u32);
        let tables: Vec<Table> = {
            let m = self.0.modules.lock().unwrap();
            m.loaded.values().filter_map(|key| lua.registry_value::<Table>(key).ok()).collect()
        };
        for tbl in tables {
            total = total.saturating_add(tbl.pairs::<Value, Value>().count() as u32);
        }
        total
    }

    /// Compile-check every cold Lua module in active third-party (non-builtin) directory plugins'
    /// `lua/` trees without executing module bodies, so syntax errors in modules not required
    /// during `apply` are caught by `mare doctor` with zero side effects.
    /// Returns `(checked_count, Vec<(plugin, module, error)>)`.
    pub fn precheck_third_party_modules(&self) -> (usize, Vec<(String, String, String)>) {
        let targets: Vec<(String, PathBuf)> = {
            let active_non_builtins: HashSet<String> =
                self.st().defs.iter().filter(|d| d.source.origin != Origin::Builtin).map(|d| d.name.clone()).collect();
            let m = self.0.modules.lock().unwrap();
            let mut list: Vec<(String, PathBuf)> = m
                .dirs
                .iter()
                .filter(|(name, _)| active_non_builtins.contains(*name))
                .filter_map(|(name, (dir, _, _))| match dir {
                    ModuleDir::Own(path) => Some((name.clone(), path.clone())),
                    _ => None,
                })
                .collect();
            list.sort_by(|a, b| a.0.cmp(&b.0));
            list
        };

        let lua = self.lua();
        let mut checked = 0usize;
        let mut failures = Vec::new();
        for (plugin, lua_dir) in targets {
            let modules = listed_dir(&lua_dir);
            for (path, module_name) in modules.iter() {
                if CORE_ROOTS.contains(&module_root(module_name)) {
                    continue;
                }
                checked += 1;
                if self.is_module_loaded(&plugin, module_name) {
                    continue;
                }
                let chunk_name = format!("@{}", path.display());
                if let Err(err) = load_cached_chunk(lua, Some(&lua_dir), path, &chunk_name, None) {
                    failures.push((plugin.clone(), module_name.clone(), err.to_string()));
                }
            }
        }
        (checked, failures)
    }
}

fn remove_from_roots(roots: &mut HashMap<String, Vec<String>>, plugin: &str) {
    roots.retain(|_, providers| {
        providers.retain(|p| p != plugin);
        !providers.is_empty()
    });
}

fn roots_of(plugin: &str, dir: &ModuleDir) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    match dir {
        ModuleDir::Own(root) => {
            for (_, name) in listed_dir(root).iter() {
                out.insert(module_root(name).to_string());
            }
        }
        ModuleDir::Shared(root) => {
            if listed_dir(root).iter().any(|(_, n)| builtin_root_owner(n) == Some(plugin)) {
                out.insert(plugin.to_string());
            }
        }
        ModuleDir::Embedded => {
            if !builtin_modules_for(plugin).is_empty() {
                out.insert(plugin.to_string());
            }
        }
    }
    out
}

fn walk_lua_dir(
    dir: &Path,
    root: &Path,
    canon_root: &Path,
    visited: &mut HashSet<PathBuf>,
    depth: usize,
    out: &mut Vec<(PathBuf, String)>,
) {
    if depth >= 32 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.filter_map(Result::ok).map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        let Ok(meta) = std::fs::symlink_metadata(&p) else { continue };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            let Ok(canon) = dunce::canonicalize(&p) else { continue };
            if !canon.starts_with(canon_root) || !visited.insert(canon) {
                continue;
            }
            walk_lua_dir(&p, root, canon_root, visited, depth + 1, out);
        } else if meta.file_type().is_file() && p.extension().is_some_and(|e| e == "lua") {
            let Ok(canon) = dunce::canonicalize(&p) else { continue };
            if !canon.starts_with(canon_root) {
                continue;
            }
            let rel = p.strip_prefix(root).unwrap_or(&p).with_extension("");
            let mut name = rel.to_string_lossy().replace(['/', '\\'], ".");
            if let Some(s) = name.strip_suffix(".init") {
                name = s.to_string();
            }
            out.push((p, name));
        }
    }
}

fn listed_dir(root: &Path) -> std::rc::Rc<Vec<(PathBuf, String)>> {
    if let Some(found) = MODULE_SCAN.with(|c| c.borrow().as_ref().and_then(|c| c.dirs.get(root).cloned())) {
        return found;
    }
    let mut out = vec![];
    if let Ok(canon_root) = dunce::canonicalize(root) {
        let mut visited = HashSet::from([canon_root.clone()]);
        walk_lua_dir(root, root, &canon_root, &mut visited, 0, &mut out);
    }
    let out = std::rc::Rc::new(out);
    MODULE_SCAN.with(|c| {
        if let Some(c) = c.borrow_mut().as_mut() {
            c.dirs.insert(root.to_path_buf(), out.clone());
        }
    });
    out
}

fn digest_file(p: &Path) -> u64 {
    use std::hash::Hash;
    if let Some(found) = MODULE_SCAN.with(|c| c.borrow().as_ref().and_then(|c| c.files.get(p).copied())) {
        return found;
    }
    let mut f = std::collections::hash_map::DefaultHasher::new();
    std::fs::read(p).unwrap_or_default().hash(&mut f);
    let found = std::hash::Hasher::finish(&f);
    MODULE_SCAN.with(|c| {
        if let Some(c) = c.borrow_mut().as_mut() {
            c.files.insert(p.to_path_buf(), found);
        }
    });
    found
}

fn hash_root_dir(root: &Path, keep: &dyn Fn(&str) -> bool, h: &mut impl std::hash::Hasher) {
    use std::hash::Hash;
    for (p, name) in listed_dir(root).iter().filter(|(_, n)| keep(n)) {
        name.hash(h);
        digest_file(p).hash(h);
    }
}

/// The module files seen within one rows computation ([`ModuleScan::scope`]): each root's listing
/// and each file's content digest, so a reload walks and reads them once.
#[derive(Default)]
pub(crate) struct ModuleScan {
    dirs: HashMap<PathBuf, std::rc::Rc<Vec<(PathBuf, String)>>>,
    files: HashMap<PathBuf, u64>,
}

thread_local! {
    static MODULE_SCAN: std::cell::RefCell<Option<ModuleScan>> = const { std::cell::RefCell::new(None) };
}

impl ModuleScan {
    pub(crate) fn scope<T>(f: impl FnOnce() -> T) -> T {
        struct Reset(bool);
        impl Drop for Reset {
            fn drop(&mut self) {
                if self.0 {
                    MODULE_SCAN.with(|c| *c.borrow_mut() = None);
                }
            }
        }
        let outer = MODULE_SCAN.with(|c| {
            let mut c = c.borrow_mut();
            let fresh = c.is_none();
            if fresh {
                *c = Some(Self::default());
            }
            fresh
        });
        let _reset = Reset(outer);
        f()
    }
}

/// Where `d`'s implementation modules come from, if it has any.
fn module_dir_of(d: &PluginDef) -> Option<ModuleDir> {
    match &d.source.path {
        Some(p) if p.is_dir() => p.join("lua").is_dir().then(|| ModuleDir::Own(p.join("lua"))),
        Some(p) if d.source.origin == Origin::Builtin => {
            p.parent().map(|dir| dir.join("lua")).filter(|l| l.is_dir()).map(ModuleDir::Shared)
        }
        None if d.source.origin == Origin::Builtin && d.source.code.is_some() => {
            (!builtin_modules_for(&d.name).is_empty()).then_some(ModuleDir::Embedded)
        }
        _ => None,
    }
}

#[cfg(test)]
mod kernel_bytecode_tests {
    use super::KERNEL_LUA;

    #[test]
    fn embedded_bytecode_is_the_source_compiled_with_the_same_chunk_name() {
        let lua = mlua::Lua::new();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lua");
        assert!(KERNEL_LUA.len() > 20, "the kernel libraries are embedded");
        for (module, code) in KERNEL_LUA {
            let rel = module.replace('.', "/");
            let file = [root.join(format!("{rel}.lua")), root.join(rel.clone()).join("init.lua")]
                .into_iter()
                .find(|p| p.is_file())
                .unwrap_or_else(|| panic!("no source for {module}"));
            let chunk = format!("@kernel/lua/{rel}.lua");
            let src = std::fs::read_to_string(&file).unwrap();
            let compiled = lua.load(&src).set_name(chunk.clone()).into_function().unwrap();
            assert!(compiled.dump(false) == *code, "{module}: the embedded bytecode is not its source compiled");
            let loaded = lua.load(*code).set_mode(mlua::chunk::ChunkMode::Binary).into_function().unwrap();
            assert_eq!(loaded.info().source.as_deref(), Some(chunk.as_str()), "{module}: chunk name");
        }
    }

    #[test]
    fn an_error_in_kernel_bytecode_reports_file_and_line() {
        let lua = mlua::Lua::new();
        let (_, code) = KERNEL_LUA.iter().find(|(n, _)| *n == "cordis.fiber").expect("cordis.fiber");
        let loaded = lua.load(*code).set_mode(mlua::chunk::ChunkMode::Binary).into_function().unwrap();
        let err = loaded.call::<mlua::Value>(()).unwrap_err().to_string();
        assert!(err.contains("kernel/lua/cordis/fiber.lua:"), "{err}");
    }

    #[test]
    fn valid_module_name_rejects_traversal_and_separators() {
        assert!(super::valid_module_name("cordis.fiber"));
        assert!(super::valid_module_name("tools.pipeline_guard"));
        assert!(!super::valid_module_name(""));
        assert!(!super::valid_module_name(".."));
        assert!(!super::valid_module_name("foo..bar"));
        assert!(!super::valid_module_name("../secret"));
        assert!(!super::valid_module_name("a/b"));
        assert!(!super::valid_module_name("a\\b"));
        assert!(!super::valid_module_name("a\0b"));
    }

    #[test]
    fn lookup_builtin_module_binary_searches_sorted_table() {
        let table: &[(&str, &[u8])] = &[
            ("mare_lib", b"lib"),
            ("plugins.news", b"news"),
            ("plugins.watch", b"watch"),
            ("review.runner", b"runner"),
        ];
        assert_eq!(super::lookup_builtin_module(table, "mare_lib"), Some(b"lib".as_slice()));
        assert_eq!(super::lookup_builtin_module(table, "plugins.watch"), Some(b"watch".as_slice()));
        assert_eq!(super::lookup_builtin_module(table, "review.runner"), Some(b"runner".as_slice()));
        assert_eq!(super::lookup_builtin_module(table, "plugins.missing"), None);
        assert_eq!(super::lookup_builtin_module(table, "aaa"), None);
        assert_eq!(super::lookup_builtin_module(table, "zzz"), None);
    }

    #[tokio::test]
    async fn single_plugin_reload_syncs_changed_core_mare_lib() {
        struct CleanDir(std::path::PathBuf);
        impl Drop for CleanDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        tokio::task::LocalSet::new()
            .run_until(async {
                let nanos =
                    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
                let root = std::env::temp_dir().join(format!("mare-req-sync-{}-{nanos}", std::process::id()));
                let _clean = CleanDir(root.clone());
                let lua_dir = root.join("lua/mare_lib");
                std::fs::create_dir_all(&lua_dir).unwrap();
                let probe = lua_dir.join("probe.lua");
                std::fs::write(&probe, "return { ver = 1 }").unwrap();
                let plugin_file = root.join("foo.lua");
                std::fs::write(
                    &plugin_file,
                    r#"return {
                        name = "foo",
                        apply = function()
                            local p = require("mare_lib.probe")
                            mare._probe_ver = p.ver
                        end,
                    }"#,
                )
                .unwrap();

                let k = crate::Kernel::new(mlua::Lua::new(), serde_json::json!({})).unwrap();
                let mare = k.lua().create_table().unwrap();
                let task = k.lua().create_table().unwrap();
                task.set(
                    "token",
                    k.lua()
                        .create_function(|_, ()| {
                            Ok(mare_shared::lua::Token(tokio_util::sync::CancellationToken::new()))
                        })
                        .unwrap(),
                )
                .unwrap();
                task.set("clock", k.lua().create_function(|_, ()| Ok(0.0_f64)).unwrap()).unwrap();
                mare.set("task", task).unwrap();
                mare.set("log", k.lua().create_function(|_, _: mlua::MultiValue| Ok(())).unwrap()).unwrap();
                k.lua().globals().set("mare", mare).unwrap();

                let src = crate::PluginSource {
                    origin: crate::Origin::Builtin,
                    path: Some(plugin_file),
                    code: None,
                    name_hint: "foo".to_string(),
                };
                let def = k.load_def(&src).unwrap();
                k.st().defs.push(def);
                k.start().await.unwrap();

                let v1: i64 = k.lua().load("return mare._probe_ver").eval().unwrap();
                assert_eq!(v1, 1);

                std::fs::write(&probe, "return { ver = 2 }").unwrap();
                let future = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
                std::fs::File::options().write(true).open(&probe).unwrap().set_modified(future).unwrap();

                k.reload(Some("foo")).await.unwrap();
                let v2: i64 = k.lua().load("return mare._probe_ver").eval().unwrap();
                assert_eq!(v2, 2, "single-plugin /reload must sync changed dev-mode mare_lib modules");
            })
            .await;
    }

    #[test]
    fn builtin_module_owner_index_matches_linear_scan() {
        static SAMPLE: &[(&str, &[u8])] = &[
            ("mare_lib", b"ml0"),
            ("mare_lib.util", b"ml1"),
            ("tools", b"t0"),
            ("tools-apply-patch.exec", b"tap0"),
            ("tools.alpha", b"t1"),
            ("tools.beta", b"t2"),
        ];
        let indexed = super::index_builtin_modules(SAMPLE);
        for owner in ["core", "tools", "tools-apply-patch", "missing"] {
            let expected: Vec<(&str, &[u8])> =
                SAMPLE.iter().copied().filter(|(n, _)| super::builtin_root_owner(n) == Some(owner)).collect();
            let got = indexed.get(owner).map_or(&[][..], Vec::as_slice);
            assert_eq!(got, expected.as_slice(), "owner={owner}");
        }
    }
}
