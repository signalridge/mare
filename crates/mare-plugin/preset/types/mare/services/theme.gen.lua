---@meta _
-- GENERATED from types/mare/services/contracts/theme.lua (service `theme`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.ThemeService
local M = {}

--- End the current preview.
---@return mare.ThemeSnapshot
function M:cancel_preview() end

--- The active theme snapshot.
---@return mare.ThemeSnapshot
function M:get() end

--- Builtin, dynamic, inactive and override-only style tokens.
---@return {name: string, style: mare.Style}[]
function M:inspect() end

--- Installed and registered themes, sorted by name.
---@return {data: ({label: string, name: string, origin: string, variants: ("dark"|"light")[]})[]}
function M:list() end

--- Preview a theme. Affects rendering only, never the preference; ends with its caller.
---@param theme string
---@return mare.ThemeSnapshot
function M:preview(theme) end

--- Register a theme owned by the caller. Duplicate or invalid definitions fail without changing state; releasing the registration removes only it, and releasing the selected theme restores auto.
---@param definition {base?: "dark"|"light", icons?: table<string, string>, label?: string, name: string, palette?: table<string, string>, styles?: table<string, {bg?: string, bold?: boolean, dim?: boolean, fg?: string, italic?: boolean, reverse?: boolean, strike?: boolean, underline?: boolean}>, syntax?: string, variants?: {dark: {base?: "dark"|"light", palette?: table<string, string>, styles?: table<string, {bg?: string, bold?: boolean, dim?: boolean, fg?: string, italic?: boolean, reverse?: boolean, strike?: boolean, underline?: boolean}>, syntax?: string}, light: {base?: "dark"|"light", palette?: table<string, string>, styles?: table<string, {bg?: string, bold?: boolean, dim?: boolean, fg?: string, italic?: boolean, reverse?: boolean, strike?: boolean, underline?: boolean}>, syntax?: string}}}
---@return fun() dispose releases this registration
function M:register(definition) end

--- An owned style layer; later layers win, config.theme and explicit ui.style win over layers. Replacing a layer id keeps one layer; an older registration cannot remove its replacement.
---@param layer_id string
---@param styles table<string, {bg?: string, bold?: boolean, dim?: boolean, fg?: string, italic?: boolean, reverse?: boolean, strike?: boolean, underline?: boolean}>
---@return fun() dispose releases this registration
function M:register_override(layer_id, styles) end

--- Validate, optionally save atomically, then publish the selection. save defaults to false (process only); init.lua still wins on the next start.
---@param theme string
---@param opts? {save?: boolean}
---@return mare.ThemeSnapshot
function M:select(theme, opts) end
