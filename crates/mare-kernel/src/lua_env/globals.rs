//! `mare.g` (state that survives `/reload`) and `mare.loader`.

use mlua::{Table, Value};

use crate::Kernel;
use crate::bytecode::{loader_prune, loader_reset};

impl Kernel {
    /// `mare.g` (hot reload item 6; WezTerm `wezterm.GLOBAL`, Neovim
    /// `vim.g`): one table owned by the kernel for state that must survive `/reload` (plugins are
    /// re-applied in the same Lua state, so the table lives on) and is gone when mare exits. Only
    /// strings, numbers, booleans and tables (WezTerm's restriction); values are copied in and
    /// reads return copies, so a plugin can't keep a live reference into another's state. No
    /// read-modify-write lock: the last writer wins — key by plugin name.
    pub(crate) fn install_globals(&self) -> mlua::Result<()> {
        let lua = self.lua();
        let Ok(mare) = lua.globals().get::<Table>("mare") else { return Ok(()) };
        if mare.raw_get::<Value>("g")? != Value::Nil {
            return Ok(());
        }
        let g: Table = lua
            .load(
                r#"
local store = {}
local function copy(v, seen, depth)
  local t = type(v)
  if t == "string" or t == "number" or t == "boolean" or t == "nil" then return v end
  if t ~= "table" then error("mare.g only stores strings, numbers, booleans and tables (got " .. t .. ")", 3) end
  if depth > 64 then error("mare.g: table nested too deeply", 3) end
  if seen[v] then error("mare.g: tables with cycles can't be stored", 3) end
  seen[v] = true
  local out = {}
  for k, x in pairs(v) do
    local kt = type(k)
    if kt ~= "string" and kt ~= "number" then error("mare.g: table keys must be strings or numbers", 3) end
    out[k] = copy(x, seen, depth + 1)
  end
  seen[v] = nil
  return out
end
return setmetatable({}, {
  __index = function(_, k) return copy(store[k], {}, 0) end,
  __newindex = function(_, k, v) store[k] = copy(v, {}, 0) end,
  __pairs = function()
    local snap = copy(store, {}, 0)
    return next, snap, nil
  end,
  __metatable = false,
})
"#,
            )
            .set_name("@kernel/mare.g")
            .eval()?;
        mare.raw_set("g", g)?;
        // mare.loader.reset([path]) / mare.loader.prune()
        let loader = lua.create_table()?;
        loader.set(
            "reset",
            lua.create_function(|lua, path: Option<String>| {
                mare_shared::exec_ctx::refuse_deny_read(lua, "loader.reset")?;
                Ok(loader_reset(path.as_deref().map(std::path::Path::new)))
            })?,
        )?;
        loader.set(
            "prune",
            lua.create_function(|lua, ()| {
                mare_shared::exec_ctx::refuse_deny_read(lua, "loader.prune")?;
                Ok(loader_prune())
            })?,
        )?;
        mare.raw_set("loader", loader)?;
        self.install_log_and_profile(&mare)
    }

    /// `mare.log(level, msg)` and `mare.profile.*`. A log
    /// line is attributed to the owner the running work entered (`prims.owner.enter`).
    fn install_log_and_profile(&self, mare: &Table) -> mlua::Result<()> {
        let lua = self.lua();
        let w = self.weak();
        mare.raw_set(
            "log",
            lua.create_function(move |_, (level, msg): (String, String)| {
                let plugin = crate::plugins::prims::current_owner()
                    .and_then(|id| Self::from_weak(&w).and_then(|k| k.ctx_plugin(id)))
                    .unwrap_or_default();
                match level.as_str() {
                    "error" => tracing::error!(target: "plugin", "[{plugin}] {msg}"),
                    "warn" => tracing::warn!(target: "plugin", "[{plugin}] {msg}"),
                    "debug" => tracing::debug!(target: "plugin", "[{plugin}] {msg}"),
                    _ => tracing::info!(target: "plugin", "[{plugin}] {msg}"),
                }
                Ok(())
            })?,
        )?;
        let profile = lua.create_table()?;
        profile.set(
            "mark",
            lua.create_function(|_, label: String| {
                mare_shared::profile::mark(&label);
                Ok(())
            })?,
        )?;
        // report() -> { {at = ms, took = ms, label}, ... }: startup timeline and plugin load times
        profile.set(
            "report",
            lua.create_function(|lua, ()| {
                let t = lua.create_table()?;
                for (i, e) in mare_shared::profile::entries().into_iter().enumerate() {
                    let r = lua.create_table()?;
                    r.set("at", e.at.as_secs_f64() * 1000.0)?;
                    r.set("took", e.took.as_secs_f64() * 1000.0)?;
                    r.set("label", e.label)?;
                    t.set(i + 1, r)?;
                }
                Ok(t)
            })?,
        )?;
        profile.set(
            "slow_callbacks",
            lua.create_function(|lua, ()| {
                let t = lua.create_table()?;
                for (i, cb) in mare_shared::profile::slow_callbacks().into_iter().enumerate() {
                    let r = lua.create_table()?;
                    r.set("at_ms", cb.at_ms)?;
                    r.set("ms", cb.ms)?;
                    r.set("plugin", cb.plugin)?;
                    r.set("event", cb.event)?;
                    t.set(i + 1, r)?;
                }
                Ok(t)
            })?,
        )?;
        let w = self.weak();
        profile.set(
            "callback_timings",
            lua.create_function(move |lua, limit: Option<usize>| {
                let limit =
                    limit.unwrap_or_else(|| Self::from_weak(&w).map(|k| k.slow_callback_table_rows()).unwrap_or(100));
                let t = lua.create_table()?;
                for (i, row) in mare_shared::profile::callback_timings(limit).into_iter().enumerate() {
                    let r = lua.create_table()?;
                    r.set("plugin", row.plugin)?;
                    r.set("event", row.event)?;
                    r.set("count", row.count)?;
                    r.set("total_ms", row.total_ms)?;
                    r.set("max_ms", row.max_ms)?;
                    t.set(i + 1, r)?;
                }
                Ok(t)
            })?,
        )?;
        mare.raw_set("profile", profile)
    }
}
