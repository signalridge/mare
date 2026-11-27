---@meta _
-- GENERATED from types/mare/services/contracts/plugins.lua (service `plugins`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.PluginsService
local M = {}

--- Fetch and compare plugin revisions without moving checkouts or modifying locks.
---@param names? string[]
---@return {data: ({behind_count: integer, breaking: boolean, commits: string[], error?: string, name: string, status: "up_to_date"|"outdated"|"pinned"|"local"|"unreachable"})[]}
function M:check(names) end

--- Offer the optional plugins, install selected packages, and return the outcome; nil when the offer is not due.
---@return ({enabled: string[], failed: string[], refused: {name: string, reason: string}[], result: "chosen"|"skipped"|"unavailable", synced: boolean})|nil
function M:choose_plugins() end

--- Install a catalog plugin by short name or sync a declared source; raises `plugins.failed` on error.
---@param name? string
---@param opts? {reload?: boolean}
---@return {log: string[], reload?: {added: string[], notices: string[], reloaded: string[], removed: string[], replaced: string[]}}
function M:install(name, opts) end

--- Declared/installed packages, parsed `mare-plugin.toml` manifests, and resource directories.
---@param kind? "skills"|"prompts"|"themes"|"types"
---@return {data: ({branch?: string, commit?: string, constraint?: string, dir: string, dirty: boolean, disabled: boolean, display: string, installed: boolean, local: boolean, locked?: string, manifest?: {category?: string, conflicts: string[], dependencies: string[], deps: {optional: string[], required: string[]}, description?: string, homepage?: string, keywords: string[], license?: string, name?: string, news: string, platforms: ("linux"|"macos"|"windows")[], plugins: string[], prompts: string[], provides?: {commands: string[], flags: string[], hooks: string[], mcp: string[], prompts: string[], services: string[], skills: string[], themes: string[], tools: string[]}, requires?: string, skills: string[], themes: string[], version?: string, web?: {dir: string, fallback?: string, index?: string, prefix?: string}}, name: string, problem?: string, scope: "user"|"project"|"init.lua", url: string})[], diagnostics: {constraint?: string, message: string, path: string, plugin: string}[], dirs: {dir: string, name: string}[]}
function M:list(kind) end

--- Show NEWS.md sections for mare / enabled plugins whose version changed since last seen.
---@return string[]
function M:news() end

--- Uninstall a catalog plugin by short name or clean an unused source; raises `plugins.failed` on error.
---@param name? string
---@param opts? {reload?: boolean}
---@return {log: string[], reload?: {added: string[], notices: string[], reloaded: string[], removed: string[], replaced: string[]}}
function M:uninstall(name, opts) end

--- Update declared git packages to their newest allowed revision (`update`); raises `plugins.failed` on error.
---@param name? string
---@param opts? {reload?: boolean, stash?: boolean}
---@return {log: string[], reload?: {added: string[], notices: string[], reloaded: string[], removed: string[], replaced: string[]}}
function M:update(name, opts) end
