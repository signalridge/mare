//! Process-wide public Lua module registry (generated from `preset/types/**/*.lua` `---@meta`
//! declarations by `mare-plugin/build.rs`) and debug built-in `lua/` directory registration.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use crate::Kernel;

static PUBLIC_MODULES: OnceLock<&'static [&'static str]> = OnceLock::new();
static BUILTIN_LUA_DIR: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

/// Register the sorted, unique list of public Lua modules generated from `preset/types/`.
pub fn set_public_modules(modules: &'static [&'static str]) {
    debug_assert!(
        modules.windows(2).all(|w| w[0] < w[1]),
        "set_public_modules requires strictly sorted unique module names"
    );
    let _ = PUBLIC_MODULES.set(modules);
}

/// Register the live built-in `preset/plugins/lua` directory in debug / `MARE_DEV_PLUGINS` runs
/// so core-owned `mare_lib*` modules remain reachable regardless of which built-ins are enabled.
pub fn set_builtin_lua_dir(dir: PathBuf) {
    let slot = BUILTIN_LUA_DIR.get_or_init(|| Mutex::new(None));
    *slot.lock().unwrap() = Some(dir);
}

pub(crate) fn builtin_lua_dir() -> Option<PathBuf> {
    BUILTIN_LUA_DIR.get().and_then(|slot| slot.lock().unwrap().clone())
}

impl Kernel {
    /// Sorted list of public Lua modules declared with `---@meta <name>` under `preset/types/`.
    pub fn public_modules() -> &'static [&'static str] {
        PUBLIC_MODULES.get().copied().unwrap_or(&[])
    }
}
